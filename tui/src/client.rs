use anyhow::{Context, Result, bail, ensure};
use asupersync::Cx;
use asupersync::bytes::Buf;
use asupersync::channel::mpsc::Sender;
use asupersync::http::body::{Body, Frame};
use asupersync::http::h1::{HttpClient, Method};
use asupersync::time::{sleep, timeout, wall_now};
use asupersync::tls::Certificate;
use serde_json::Value;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;
use url::Url;

pub enum StreamUpdate {
    Event(Value),
    Status(String),
    Fatal(anyhow::Error),
}

#[derive(Clone)]
pub struct Remote {
    client: HttpClient,
    pub workspace: String,
    cookie: String,
}

pub fn origin(input: &str) -> Result<String> {
    validate_origin(input, false)
}

pub fn standalone_origin(input: &str) -> Result<String> {
    validate_origin(input, true)
}

fn validate_origin(input: &str, allow_loopback: bool) -> Result<String> {
    let url = Url::parse(input)?;
    let local = allow_loopback
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    ensure!(
        (url.scheme() == "https" || local)
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

fn parse_status_error(status: u16, body: &[u8]) -> anyhow::Error {
    if let Ok(value) = serde_json::from_slice::<Value>(body)
        && let Some(msg) = value["error"]["message"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
    {
        return anyhow::anyhow!("{status}: {msg}");
    }
    let snippet: String = String::from_utf8_lossy(body)
        .trim()
        .chars()
        .take(160)
        .collect();
    if snippet.is_empty() {
        anyhow::anyhow!("HTTP {status}")
    } else {
        anyhow::anyhow!("HTTP {status}: {snippet}")
    }
}

fn form_encode(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish().into_bytes()
}

impl Remote {
    fn http(ca: Option<&std::path::Path>, workspace: &str) -> Result<HttpClient> {
        let mut builder = HttpClient::builder()
            .no_redirects()
            .no_cookie_store()
            .request_timeout(Duration::from_secs(35));
        let url = Url::parse(workspace)?;
        if let Some(host) = url
            .host_str()
            .filter(|host| *host == "localhost" || host.ends_with(".localhost"))
        {
            let _ = host;
            builder = builder.no_proxy();
        }
        if let Some(path) = ca {
            for cert in Certificate::from_pem_file(path)
                .map_err(|e| anyhow::anyhow!("Invalid CA PEM {}: {e}", path.display()))?
            {
                builder = builder.add_root_certificate(cert);
            }
        }
        Ok(builder.build())
    }

    pub fn saved(&self) -> Result<String> {
        Ok(serde_json::json!({"url": self.workspace, "cookie": self.cookie}).to_string())
    }

    pub fn restore(ca: Option<&std::path::Path>, saved: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(saved)?;
        let workspace = standalone_origin(value["url"].as_str().context("Missing workspace")?)?;
        let client = Self::http(ca, &workspace)?;
        let cookie = value["cookie"]
            .as_str()
            .context("Missing cookie")?
            .to_owned();
        ensure!(
            !cookie.is_empty() && !cookie.contains(['\r', '\n']),
            "Invalid cookie"
        );
        Ok(Self {
            client,
            workspace,
            cookie,
        })
    }

    fn auth_headers(&self) -> Vec<(String, String)> {
        vec![
            ("Origin".into(), self.workspace.clone()),
            ("Cookie".into(), self.cookie.clone()),
        ]
    }

    pub async fn valid(&self, cx: &Cx) -> Result<bool> {
        let res = self
            .client
            .request_with_timeout(
                cx,
                Method::Get,
                &format!("{}/api/v1/venticode/session", self.workspace),
                self.auth_headers(),
                Vec::new(),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Connection failed: {e}"))?;
        if res.status == 401 {
            return Ok(false);
        }
        if !(200..300).contains(&res.status) {
            return Err(parse_status_error(res.status, &res.body));
        }
        Ok(true)
    }

    pub async fn connect_standalone(
        cx: &Cx,
        ca: Option<&std::path::Path>,
        address: &str,
        token: &str,
    ) -> Result<Self> {
        let workspace = standalone_origin(address)?;
        let client = Self::http(ca, &workspace)?;
        let headers = vec![
            ("Origin".into(), workspace.clone()),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ];
        let res = client
            .request_with_timeout(
                cx,
                Method::Post,
                &format!("{workspace}/venticode/login"),
                headers,
                form_encode(&[("token", token)]),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Sign-in request failed: {e}"))?;
        Self::authenticated(client, workspace, res.status, &res.headers)
    }

    pub async fn connect(
        cx: &Cx,
        ca: Option<&std::path::Path>,
        portal: &str,
        code: &str,
    ) -> Result<Self> {
        let link: Value = serde_json::from_str(code).context("Invalid one-time code")?;
        let workspace = origin(link["url"].as_str().context("Missing workspace")?)?;
        let client = Self::http(ca, &workspace)?;
        let ticket = link["ticket"].as_str().context("Missing ticket")?;
        let headers = vec![
            ("Origin".into(), origin(portal)?),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ];
        let res = client
            .request_with_timeout(
                cx,
                Method::Post,
                &format!("{workspace}/venticode/bootstrap"),
                headers,
                form_encode(&[("ticket", ticket)]),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Bootstrap request failed: {e}"))?;
        Self::authenticated(client, workspace, res.status, &res.headers)
    }

    fn authenticated(
        client: HttpClient,
        workspace: String,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<Self> {
        ensure!(status == 303, "Sign-in failed ({status})");
        let raw = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .filter_map(|(_, v)| v.split(';').next())
            .find(|s| s.starts_with("venticode_session="))
            .context("Missing workspace session")?
            .to_owned();
        Ok(Self {
            client,
            workspace,
            cookie: raw,
        })
    }

    pub async fn api(
        &self,
        cx: &Cx,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut headers = self.auth_headers();
        let bytes = if let Some(data) = body {
            headers.push(("Content-Type".into(), "application/json".into()));
            serde_json::to_vec(&data)?
        } else {
            Vec::new()
        };
        let res = self
            .client
            .request_with_timeout(
                cx,
                method,
                &format!("{}/api/v1/venticode{path}", self.workspace),
                headers,
                bytes,
                Duration::from_secs(35),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Request failed: {e}"))?;
        if res.status == 204 {
            return Ok(Value::Null);
        }
        if !(200..300).contains(&res.status) {
            return Err(parse_status_error(res.status, &res.body));
        }
        serde_json::from_slice(&res.body).context("Invalid JSON response from Venticode")
    }

    pub async fn list(&self, cx: &Cx, path: &str) -> Result<Vec<Value>> {
        self.api(cx, Method::Get, path, None).await?["items"]
            .as_array()
            .cloned()
            .context("Expected list")
    }

    pub async fn follow(&self, cx: Cx, id: String, tx: Sender<(String, StreamUpdate)>) {
        let mut cursor: i64 = 0;
        let mut backoff_secs: u64 = 1;
        loop {
            if cx.is_cancel_requested() {
                break;
            }
            let result = self
                .follow_once(&cx, &id, &mut cursor, &tx, &mut backoff_secs)
                .await;
            match result {
                Ok(()) => {}
                Err(error) => {
                    let _ = tx.send(&cx, (id.clone(), StreamUpdate::Fatal(error))).await;
                    break;
                }
            }
            if cx.is_cancel_requested() {
                break;
            }
            let _ = tx
                .send(
                    &cx,
                    (
                        id.clone(),
                        StreamUpdate::Status(format!("Reconnecting in {backoff_secs}s…")),
                    ),
                )
                .await;
            sleep(wall_now(), Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(15);
        }
    }

    async fn follow_once(
        &self,
        cx: &Cx,
        id: &str,
        cursor: &mut i64,
        tx: &Sender<(String, StreamUpdate)>,
        backoff_secs: &mut u64,
    ) -> Result<()> {
        let mut headers = self.auth_headers();
        headers.push(("Accept".into(), "text/event-stream".into()));
        if *cursor > 0 {
            headers.push(("Last-Event-ID".into(), cursor.to_string()));
        }
        let Ok(mut stream) = self
            .client
            .request_streaming(
                cx,
                Method::Get,
                &format!(
                    "{}/api/v1/venticode/conversations/{id}/stream?after={cursor}",
                    self.workspace
                ),
                headers,
                Vec::new(),
            )
            .await
        else {
            return Ok(());
        };
        let status = stream.head.status;
        if (400..500).contains(&status) && status != 429 {
            bail!("Session unavailable ({status})");
        }
        if !(200..300).contains(&status) {
            return Ok(());
        }
        *backoff_secs = 1;
        let _ = tx
            .send(cx, (id.to_owned(), StreamUpdate::Status(String::new())))
            .await;

        let mut parser = SseParser::default();
        loop {
            if cx.is_cancel_requested() {
                return Ok(());
            }
            let next = timeout(
                wall_now(),
                Duration::from_secs(45),
                next_body_chunk(&mut stream.body),
            )
            .await;
            let chunk = match next {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(None) | Ok(Some(Err(_))) | Err(_) => return Ok(()),
            };
            for (event_name, data) in parser.push(&chunk) {
                if event_name == "authorization_expired" {
                    bail!("Session expired");
                }
                if event_name != "transcript" {
                    continue;
                }
                let v: Value = serde_json::from_str(&data)?;
                let seq = v["sequence"].as_i64().context("Missing event sequence")?;
                if seq <= *cursor {
                    continue;
                }
                *cursor = seq;
                tx.send(cx, (id.to_owned(), StreamUpdate::Event(v)))
                    .await
                    .map_err(|_| anyhow::anyhow!("Terminal closed"))?;
            }
        }
    }
}

async fn next_body_chunk<B>(body: &mut B) -> Option<Result<Vec<u8>, B::Error>>
where
    B: Body + Unpin,
{
    std::future::poll_fn(|task_cx| match Pin::new(&mut *body).poll_frame(task_cx) {
        Poll::Ready(Some(Ok(Frame::Data(mut data)))) => {
            let mut out = Vec::with_capacity(data.remaining());
            while data.has_remaining() {
                let slice = data.chunk();
                out.extend_from_slice(slice);
                let len = slice.len();
                data.advance(len);
            }
            Poll::Ready(Some(Ok(out)))
        }
        Poll::Ready(Some(Ok(Frame::Trailers(_)))) => Poll::Ready(Some(Ok(Vec::new()))),
        Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
        Poll::Ready(None) => Poll::Ready(None),
        Poll::Pending => Poll::Pending,
    })
    .await
}

#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line = self.buf.drain(..=pos).collect::<Vec<u8>>();
            if line.ends_with(b"\n") {
                line.pop();
            }
            if line.ends_with(b"\r") {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line);
            if text.is_empty() {
                if !self.data.is_empty() {
                    let ev = if self.event.is_empty() {
                        "message".to_owned()
                    } else {
                        std::mem::take(&mut self.event)
                    };
                    let payload = std::mem::take(&mut self.data).join("\n");
                    out.push((ev, payload));
                } else {
                    self.event.clear();
                }
            } else if let Some(rest) = text.strip_prefix("event:") {
                self.event = rest.trim_start().to_owned();
            } else if let Some(rest) = text.strip_prefix("data:") {
                let val = rest.strip_prefix(' ').unwrap_or(rest);
                self.data.push(val.to_owned());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{SseParser, origin};

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

    #[test]
    fn sse_parser_handles_split_chunks_and_events() {
        let mut p = SseParser::default();
        assert!(p.push(b"event: transcript\r\ndata: {\"a\":").is_empty());
        let events = p.push(b"1}\r\n\r\nevent: heartbeat\ndata: {}\n\n");
        assert_eq!(
            events,
            vec![
                ("transcript".into(), "{\"a\":1}".into()),
                ("heartbeat".into(), "{}".into()),
            ]
        );
    }
}
