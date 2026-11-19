use crate::{protocol, runtime};
use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::HashMap, env, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};
use uuid::Uuid;

pub async fn run() -> Result<()> {
    let origin = env::var("VENTICODE_SERVER_URL").context("Set VENTICODE_SERVER_URL")?;
    let mut url = url::Url::parse(&origin)?;
    ensure!(
        url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "VENTICODE_SERVER_URL must be an origin"
    );
    let scheme = match (url.scheme(), url.host_str()) {
        ("https", _) => "wss",
        ("http", Some("localhost" | "127.0.0.1" | "[::1]")) => "ws",
        _ => anyhow::bail!("Use HTTPS for a non-loopback server"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("Invalid server URL"))?;
    url.set_path("/internal/harness/ws");
    let token = env::var("VENTICODE_HARNESS_TOKEN").context("Set VENTICODE_HARNESS_TOKEN")?;
    ensure!(
        token.len() >= 32,
        "Use a random harness token of at least 32 characters"
    );
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let roots = Arc::new(roots()?);
    let grants: std::collections::BTreeMap<String, String> = serde_json::from_str(
        &env::var("VENTICODE_HARNESS_MCP_GRANTS").unwrap_or_else(|_| "{}".into()),
    )?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            result = connect(request.clone(), roots.clone(), &grants) => {
                if let Err(error) = result {
                    eprintln!("Venticode harness will reconnect: {error}");
                }
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
    }
}

async fn connect(
    request: tokio_tungstenite::tungstenite::http::Request<()>,
    roots: Arc<HashMap<String, PathBuf>>,
    grants: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let config = WebSocketConfig::default().max_message_size(Some(8 * 1024 * 1024));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await??;
    let (mut sink, mut source) = socket.split();
    let mut labels = roots.keys().collect::<Vec<_>>();
    labels.sort_unstable();
    sink.send(Message::Text(json!({"type":"inventory", "roots":labels,
        "host_info":{"os":env::consts::OS,"arch":env::consts::ARCH,"version":env!("CARGO_PKG_VERSION")}
    }).to_string().into())).await?;
    let runtime = runtime::Runtime::new()?;
    let (reply, mut frames) = mpsc::channel(16);
    let mut jobs = JoinSet::new();
    let mut active = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                ensure!(heard.elapsed() < Duration::from_secs(20), "Server heartbeat timed out");
                sink.send(Message::Ping(Default::default())).await?;
            }
            Some(frame) = frames.recv() => sink.send(frame).await?,
            Some(job) = jobs.join_next(), if !jobs.is_empty() => {
                if let Ok(id) = job {
                    active.remove(&id);
                    runtime.release(id);
                }
            }
            frame = source.next() => {
                let Some(frame) = frame else { break };
                heard = tokio::time::Instant::now();
                let text = match frame? {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    Message::Ping(_) => { sink.flush().await?; continue; }
                    _ => continue,
                };
                let mut frame: Value = serde_json::from_str(&text)?;
                let id: Uuid = serde_json::from_value(frame["id"].clone())?;
                match frame["type"].as_str() {
                    Some("execute") if active.len() < 32 && !active.contains_key(&id) => {
                        let mut payload = frame["payload"].take();
                        payload["mcp_grants"] = json!(grants);
                        let runtime = runtime.clone();
                        let reply = reply.clone();
                        let roots = roots.clone();
                        let handle = jobs.spawn(async move {
                            let result = runtime.execute(id, payload, &roots, protocol::Mode::Sandboxed, std::path::Path::new("."), &reply).await;
                            if let Err(error) = &result {
                                let _ = runtime::output(&reply, id, json!({"type":"error","error":error.to_string()})).await;
                            }
                            let _ = reply.send(Message::Text(json!({"type":"complete","id":id,"success":result.is_ok()}).to_string().into())).await;
                            id
                        });
                        active.insert(id, handle);
                    }
                    Some("input") if jobs.len() < 64 => {
                        let runtime = runtime.clone();
                        let reply = reply.clone();
                        let roots = roots.clone();
                        let payload = frame["payload"].take();
                        jobs.spawn(async move {
                            let _ = runtime.execute(id, payload, &roots, protocol::Mode::Sandboxed, std::path::Path::new("."), &reply).await;
                            id
                        });
                    }
                    Some("cancel") => {
                        if let Some(handle) = active.remove(&id) { handle.abort(); }
                        runtime.release(id);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn roots() -> Result<HashMap<String, PathBuf>> {
    let configured: HashMap<String, PathBuf> = serde_json::from_str(
        &env::var("VENTICODE_HARNESS_ROOTS").context("Set VENTICODE_HARNESS_ROOTS")?,
    )?;
    ensure!(
        !configured.is_empty() && configured.len() <= 32,
        "Configure 1–32 roots"
    );
    configured
        .into_iter()
        .map(|(label, path)| {
            ensure!(
                !label.is_empty()
                    && label.len() <= 80
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                "Invalid root label"
            );
            ensure!(path.is_absolute(), "Use an absolute path for every root");
            let path = path
                .canonicalize()
                .with_context(|| format!("Root {label} unavailable"))?;
            ensure!(
                path.is_dir() && path.parent().is_some(),
                "Invalid project root"
            );
            Ok((label, path))
        })
        .collect()
}
