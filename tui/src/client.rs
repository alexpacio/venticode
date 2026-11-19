use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use reqwest::{Client, Method, Url, header};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
pub struct Remote {
    client: Client,
    pub workspace: String,
    cookie: header::HeaderValue,
}

pub fn origin(input: &str) -> Result<String> {
    let url = Url::parse(input)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "HTTPS origin required"
    );
    Ok(url.origin().ascii_serialization())
}

impl Remote {
    pub fn http(ca: Option<&std::path::Path>) -> Result<Client> {
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10));
        if let Some(path) = ca {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
        }
        Ok(builder.build()?)
    }
    pub fn saved(&self) -> Result<String> {
        Ok(serde_json::json!({"url": self.workspace, "cookie":self.cookie.to_str()?}).to_string())
    }
    pub fn restore(client: Client, saved: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(saved)?;
        let workspace = origin(value["url"].as_str().context("Missing workspace")?)?;
        let mut cookie =
            header::HeaderValue::from_str(value["cookie"].as_str().context("Missing cookie")?)?;
        cookie.set_sensitive(true);
        Ok(Self {
            client,
            workspace,
            cookie,
        })
    }
    pub async fn valid(&self) -> Result<bool> {
        let res = self
            .request(Method::GET, "/session")
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if res.status() == 401 {
            return Ok(false);
        }
        res.error_for_status()?;
        Ok(true)
    }
    pub async fn connect(client: Client, portal: &str, code: &str) -> Result<Self> {
        let link: Value = serde_json::from_str(code).context("Invalid one-time code")?;
        let workspace = origin(link["url"].as_str().context("Missing workspace")?)?;
        let ticket = link["ticket"].as_str().context("Missing ticket")?;
        let result = client
            .post(format!("{workspace}/venticode/bootstrap"))
            .timeout(Duration::from_secs(30))
            .header(header::ORIGIN, origin(portal)?)
            .form(&[("ticket", ticket)])
            .send()
            .await?;
        ensure!(
            result.status() == 303,
            "Sign-in failed ({})",
            result.status()
        );
        let raw = result
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .find(|s| s.starts_with("venticode_session="))
            .context("Missing workspace session")?;
        let mut cookie = header::HeaderValue::from_str(raw)?;
        cookie.set_sensitive(true);
        Ok(Self {
            client,
            workspace,
            cookie,
        })
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}/api/v1/venticode{path}", self.workspace))
            .header(header::ORIGIN, &self.workspace)
            .header(header::COOKIE, self.cookie.clone())
    }
    pub async fn api(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.request(method, path).timeout(Duration::from_secs(35));
        if let Some(data) = body {
            req = req.json(&data);
        }
        let res = req.send().await?;
        let status = res.status();
        if status.as_u16() == 204 {
            return Ok(Value::Null);
        }
        let v: Value = res
            .json()
            .await
            .context("Invalid response from Venticode")?;
        ensure!(
            status.is_success(),
            "{status}: {}",
            v["error"]["message"].as_str().unwrap_or("Request failed")
        );
        Ok(v)
    }
    pub async fn list(&self, path: &str) -> Result<Vec<Value>> {
        self.api(Method::GET, path, None).await?["items"]
            .as_array()
            .cloned()
            .context("Expected list")
    }
    pub async fn follow(&self, id: String, tx: Sender<(String, Result<Value>)>) {
        let mut cursor = 0;
        loop {
            let result = async {
                let Ok(res) = self
                    .request(
                        Method::GET,
                        &format!("/conversations/{id}/stream?after={cursor}"),
                    )
                    .send()
                    .await
                else {
                    return Ok(());
                };
                if res.status().is_client_error() && res.status().as_u16() != 429 {
                    bail!("Session unavailable ({})", res.status())
                }
                if !res.status().is_success() {
                    return Ok(());
                }
                let mut stream = sse_stream::SseByteStream::new(res.bytes_stream());
                while let Some(Ok(event)) = stream.next().await {
                    if event.event.as_deref() == Some("authorization_expired") {
                        bail!("Session expired")
                    }
                    if event.event.as_deref() != Some("transcript") {
                        continue;
                    }
                    let v: Value = serde_json::from_str(event.data.as_deref().unwrap_or(""))?;
                    let seq = v["sequence"].as_i64().context("Missing event sequence")?;
                    if seq <= cursor {
                        continue;
                    }
                    if seq != cursor + 1 {
                        return Ok(());
                    }
                    tx.send((id.clone(), Ok(v)))
                        .await
                        .context("Terminal closed")?;
                    cursor = seq;
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                let _ = tx.send((id.clone(), Err(e))).await;
                break;
            }
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::origin;

    #[test]
    fn bootstrap_uses_an_https_origin() {
        assert_eq!(
            origin("https://portal.example:8443/").unwrap(),
            "https://portal.example:8443"
        );
        for input in [
            "http://portal.example",
            "https://user@portal.example",
            "https://portal.example/app",
            "https://portal.example/?ticket=1",
            "https://portal.example/#fragment",
        ] {
            assert!(origin(input).is_err(), "{input}");
        }
    }
}
