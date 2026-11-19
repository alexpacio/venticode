use super::*;
use futures_util::{SinkExt, StreamExt};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const PORT: u16 = 8787;
const TOOLS: &[(&str, &str, &[&str])] = &[
    ("browser_tabs", "List browser tabs.", &[]),
    ("browser_new_tab", "Open and select a blank tab.", &[]),
    (
        "browser_select_tab",
        "Select a tab returned by browser_tabs.",
        &["id"],
    ),
    (
        "browser_open",
        "Open a URL on an approved origin.",
        &["url"],
    ),
    (
        "browser_snapshot",
        "Read the active page, its text, and links.",
        &[],
    ),
    (
        "browser_follow_link",
        "Follow a link by exact URL and label from the active page.",
        &["url", "label"],
    ),
];

pub(super) fn is_browser_tool(op: &str) -> bool {
    TOOLS.iter().any(|(name, _, _)| *name == op)
}

pub(super) fn definitions() -> impl Iterator<Item = Value> {
    TOOLS.iter().map(|(name, description, required)| {
        let properties: serde_json::Map<String, Value> = required
            .iter()
            .map(|key| ((*key).into(), json!({"type":"string","maxLength":4096})))
            .collect();
        json!({"type":"function","function":{"name":name,"description":description,"parameters":{
            "type":"object","properties":properties,"required":required,"additionalProperties":false
        }}})
    })
}

pub(super) fn validate(op: &str, args: &Value) -> Result<()> {
    let (_, _, required) = TOOLS
        .iter()
        .find(|(name, _, _)| *name == op)
        .ok_or_else(bad)?;
    if args.as_object().is_none_or(|obj| {
        obj.len() != required.len()
            || required.iter().any(|key| {
                !obj.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.len() <= 4096)
            })
    }) {
        return Err(bad());
    }
    Ok(())
}

struct Pod {
    computers: Arc<computer::Computers>,
    name: String,
    uid: Option<String>,
}

impl Drop for Pod {
    fn drop(&mut self) {
        let computers = self.computers.clone();
        let (name, uid) = (self.name.clone(), self.uid.clone());
        tokio::spawn(async move {
            let _ = computers
                .delete(&format!("pods/{name}"), uid.as_deref())
                .await;
        });
    }
}

pub(super) struct Browser {
    _pod: Option<Pod>,
    origins: Vec<String>,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    sequence: u64,
    tab: Option<(String, String)>,
}

impl Browser {
    pub async fn start(app: &App) -> Result<Self> {
        let computers = app.computers.clone().ok_or_else(unavailable)?;
        let image = computers.kitesurf.as_ref().ok_or_else(unavailable)?;
        let name = format!("kitesurf-{}", Uuid::new_v4().simple());
        // Construct cleanup guard before creating the pod so cancellation mid-start cannot leak it.
        let mut pod = Pod {
            computers: computers.clone(),
            name: name.clone(),
            uid: None,
        };
        let created = computers.create("pods", json!({
            "apiVersion":"v1","kind":"Pod","metadata":{"name":name,"labels":{"app.kubernetes.io/name":"venticode-kitesurf"}},
            "spec":{"runtimeClassName":"gvisor","serviceAccountName":"computer","automountServiceAccountToken":false,
                "enableServiceLinks":false,"restartPolicy":"Never","activeDeadlineSeconds":900,"terminationGracePeriodSeconds":5,
                "securityContext":{"runAsNonRoot":true,"runAsUser":10001,"seccompProfile":{"type":"RuntimeDefault"}},
                "containers":[{"name":"kitesurf","image":image,
                    "securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true},
                    "ports":[{"containerPort":PORT}],"readinessProbe":{"tcpSocket":{"port":PORT},"periodSeconds":2},
                    "resources":{"requests":{"cpu":"250m","memory":"256Mi"},"limits":{"cpu":"2","memory":"2Gi","ephemeral-storage":"256Mi"}},
                    "volumeMounts":[{"name":"tmp","mountPath":"/tmp"}]}],
                "volumes":[{"name":"tmp","emptyDir":{"sizeLimit":"64Mi"}}]}
        })).await?;
        let mut observed = match created {
            Some(pod) => pod,
            None => computers.pod(&name).await?.ok_or_else(unavailable)?,
        };
        let uid = observed["metadata"]["uid"]
            .as_str()
            .ok_or_else(unavailable)?
            .to_owned();
        pod.uid = Some(uid.clone());
        for _ in 0..60 {
            if observed["metadata"]["uid"] != uid
                || matches!(
                    observed["status"]["phase"].as_str(),
                    Some("Failed" | "Succeeded")
                )
            {
                break;
            }
            if observed["status"]["conditions"]
                .as_array()
                .is_some_and(|conditions| {
                    conditions
                        .iter()
                        .any(|c| c["type"] == "Ready" && c["status"] == "True")
                })
                && let Some(ip) = observed["status"]["podIP"]
                    .as_str()
                    .and_then(|s| s.parse::<IpAddr>().ok())
            {
                let url = format!("ws://{}/devtools/browser", SocketAddr::new(ip, PORT));
                let origins = pod.computers.origins.clone();
                let mut browser = Self::connect(&url, origins, Some(pod)).await?;
                browser.new_tab().await?;
                return Ok(browser);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            observed = pod
                .computers
                .pod(&pod.name)
                .await?
                .ok_or_else(unavailable)?;
        }
        Err(unavailable())
    }

    async fn connect(url: &str, origins: Vec<String>, pod: Option<Pod>) -> Result<Self> {
        let config = WebSocketConfig::default().max_message_size(Some(4 * 1024 * 1024));
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async_with_config(url, Some(config), false),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        Ok(Self {
            _pod: pod,
            origins,
            socket,
            sequence: 0,
            tab: None,
        })
    }

    async fn cdp(&mut self, method: &str, params: Value, page: bool) -> Result<Value> {
        self.sequence += 1;
        let mut request = json!({"id":self.sequence,"method":method,"params":params});
        if page {
            request["sessionId"] = json!(self.tab.as_ref().ok_or_else(missing)?.1);
        }
        tokio::time::timeout(Duration::from_secs(6), async {
            self.socket
                .send(Message::Text(request.to_string().into()))
                .await
                .map_err(|_| unavailable())?;
            while let Some(packet) = self.socket.next().await {
                match packet.map_err(|_| unavailable())? {
                    Message::Text(text) => {
                        let reply: Value =
                            serde_json::from_str(&text).map_err(|_| unavailable())?;
                        if reply["id"] == self.sequence {
                            return if reply["error"].is_null() {
                                Ok(reply["result"].clone())
                            } else {
                                Err(unavailable())
                            };
                        }
                    }
                    Message::Ping(_) => self.socket.flush().await.map_err(|_| unavailable())?,
                    _ => {}
                }
            }
            Err(unavailable())
        })
        .await
        .map_err(|_| unavailable())?
    }

    async fn tabs(&mut self) -> Result<Value> {
        let reply = self.cdp("Target.getTargets", json!({}), false).await?;
        let tabs: Vec<Value> = reply["targetInfos"]
            .as_array()
            .ok_or_else(unavailable)?
            .iter()
            .filter(|t| t["type"] == "page")
            .map(|t| json!({"targetId":t["targetId"],"title":t["title"],"url":t["url"]}))
            .collect();
        Ok(json!({"type":"tabs","tabs":tabs,"active":self.tab.as_ref().map(|(t,_)|t)}))
    }

    async fn select(&mut self, id: &str) -> Result<Value> {
        let mut tabs = self.tabs().await?;
        if !tabs["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["targetId"] == id)
        {
            return Err(missing());
        }
        if self.tab.as_ref().is_none_or(|(target, _)| target != id) {
            let reply = self
                .cdp(
                    "Target.attachToTarget",
                    json!({"targetId":id,"flatten":true}),
                    false,
                )
                .await?;
            self.tab = Some((
                id.into(),
                reply["sessionId"].as_str().ok_or_else(unavailable)?.into(),
            ));
        }
        tabs["active"] = json!(id);
        Ok(tabs)
    }

    async fn new_tab(&mut self) -> Result<Value> {
        if self.tabs().await?["tabs"]
            .as_array()
            .is_some_and(|tabs| tabs.len() >= 6)
        {
            return Err(conflict());
        }
        let reply = self
            .cdp("Target.createTarget", json!({"url":"about:blank"}), false)
            .await?;
        self.select(reply["targetId"].as_str().ok_or_else(unavailable)?)
            .await
    }

    async fn snapshot(&mut self) -> Result<Value> {
        let script = r#"JSON.stringify({url:location.href,title:document.title,
            text:(document.body?.innerText??'').slice(0,10000),links:Array.from(document.querySelectorAll('a[href]')).slice(0,50)
            .map(a=>({label:a.innerText.slice(0,140),url:a.href}))})"#;
        let reply = self
            .cdp(
                "Runtime.evaluate",
                json!({"expression":script,"returnByValue":true}),
                true,
            )
            .await?;
        let view: Value =
            serde_json::from_str(reply["result"]["value"].as_str().ok_or_else(unavailable)?)
                .map_err(|_| unavailable())?;
        if view["url"] != "about:blank" && !self.approved(view["url"].as_str().unwrap_or_default())
        {
            return Err(forbidden());
        }
        Ok(view)
    }

    fn approved(&self, url: &str) -> bool {
        reqwest::Url::parse(url).is_ok_and(|url| {
            matches!(url.scheme(), "https" | "http")
                && url.username().is_empty()
                && url.password().is_none()
                && self.origins.contains(&url.origin().ascii_serialization())
        })
    }

    async fn open(&mut self, url: &str) -> Result<Value> {
        if !self.approved(url) {
            return Err(forbidden());
        }
        let reply = self.cdp("Page.navigate", json!({"url":url}), true).await?;
        if reply["errorText"].is_string() {
            return Err(unavailable());
        }
        Ok(json!({"navigation":"started"}))
    }

    pub async fn call(&mut self, op: &str, args: &Value) -> Result<Value> {
        validate(op, args)?;
        let url = args["url"].as_str().unwrap_or_default();
        match op {
            "browser_tabs" => self.tabs().await,
            "browser_new_tab" => self.new_tab().await,
            "browser_select_tab" => self.select(args["id"].as_str().ok_or_else(bad)?).await,
            "browser_open" => self.open(url).await,
            "browser_snapshot" => self.snapshot().await,
            "browser_follow_link" => {
                let view = self.snapshot().await?;
                if !view["links"]
                    .as_array()
                    .ok_or_else(unavailable)?
                    .iter()
                    .any(|link| link["url"] == url && link["label"] == args["label"])
                {
                    return Err(conflict());
                }
                self.open(url).await
            }
            _ => Err(bad()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a disposable Kitesurf worker and an HTTP fixture"]
    async fn kitesurf_browser_tool_contract() {
        let relay = env::var("VENTICODE_TEST_KITESURF_WS").unwrap();
        let origin = env::var("VENTICODE_TEST_KITESURF_ORIGIN").unwrap();
        let mut browser = Browser::connect(&relay, vec![origin.clone()], None)
            .await
            .unwrap();
        let tabs = browser.call("browser_new_tab", &json!({})).await.unwrap();
        let id = tabs["active"].as_str().unwrap();
        assert_eq!(
            browser
                .call("browser_select_tab", &json!({"id":id}))
                .await
                .unwrap()["active"],
            id
        );
        assert_eq!(
            browser.call("browser_tabs", &json!({})).await.unwrap()["active"],
            id
        );
        browser
            .call("browser_open", &json!({"url":origin}))
            .await
            .unwrap();
        wait_for_title(&mut browser, "Fixture").await;
        browser
            .call(
                "browser_follow_link",
                &json!({"url":format!("{origin}/details.html"),"label":"Details"}),
            )
            .await
            .unwrap();
        wait_for_title(&mut browser, "Details").await;
        assert!(
            browser
                .call("browser_open", &json!({"url":"https://outside.invalid"}))
                .await
                .is_err()
        );
    }

    async fn wait_for_title(browser: &mut Browser, title: &str) {
        for _ in 0..40 {
            if browser
                .call("browser_snapshot", &json!({}))
                .await
                .is_ok_and(|page| page["title"] == title)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Kitesurf did not load {title}");
    }
}
