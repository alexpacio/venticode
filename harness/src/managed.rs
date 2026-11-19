use crate::{protocol::Mode, runtime};
use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};
use uuid::Uuid;

const DESKTOP: &str = "/run/venticode-desktop/bridge.sock";
const IDENTITY: &str = "/var/run/secrets/venticode-computer/token";
const LIMIT: usize = 8 * 1024 * 1024;
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const WS_PING_TIMEOUT: Duration = Duration::from_secs(5);
const DESKTOP_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_STDERR_CAP: usize = 64 * 1024;
const MAX_PREVIEW_SERVICES: usize = 4;
const MAX_MODEL_CALLS: usize = 32;
const MAX_CONCURRENT_MODEL_CALLS: usize = 4;
const BROKER_REQUEST_LIMIT: usize = 196 * 1024;
const FALLBACK_VENTICODE_UID: u32 = 10001;

type ActiveUnits = Arc<Mutex<HashSet<String>>>;

fn parse_passwd_uid(passwd: &str, user: &str) -> Option<u32> {
    for line in passwd.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(user) {
            let _pw = fields.next()?;
            return fields.next()?.parse::<u32>().ok();
        }
    }
    None
}

fn venticode_uid() -> u32 {
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
        return FALLBACK_VENTICODE_UID;
    };
    parse_passwd_uid(&passwd, "venticode").unwrap_or(FALLBACK_VENTICODE_UID)
}

async fn stop_active_units(units: &ActiveUnits) {
    let to_stop: Vec<String> = {
        let mut guard = units.lock().unwrap_or_else(|e| e.into_inner());
        guard.drain().collect()
    };
    if to_stop.is_empty() {
        return;
    }
    tracing::info!(count = to_stop.len(), "stopping active transient units");
    let _ = tokio::time::timeout(
        SHUTDOWN_STOP_TIMEOUT,
        Command::new("/usr/local/bin/systemctl")
            .arg("stop")
            .args(&to_stop)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
}

pub async fn run() -> Result<()> {
    let id: Uuid = env::var("VENTICODE_MANAGED_ID")?.parse()?;
    let mut origin = url::Url::parse(&env::var("VENTICODE_MANAGED_ORIGIN")?)?;
    let scheme = match origin.scheme() {
        "https" => "wss",
        "http" if origin.host_str().is_some_and(|host| host.ends_with(".svc")) => "ws",
        _ => anyhow::bail!("Invalid managed relay origin"),
    };
    origin
        .set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("Invalid relay scheme"))?;
    origin.set_path(&format!("/internal/computer/{id}/ws"));
    let mut attempt = 0u32;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let active_units: ActiveUnits = Arc::default();
    loop {
        let started = tokio::time::Instant::now();
        let units_ref = active_units.clone();
        let origin_ref = origin.clone();
        let attempt_fut = async move {
            let mut request = origin_ref.as_str().into_client_request()?;
            let token = tokio::fs::read_to_string(IDENTITY).await?;
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {}", token.trim()).parse()?);
            connect(request, units_ref).await
        };
        tokio::select! {
            _ = term.recv() => {
                tracing::info!("managed harness received SIGTERM, stopping active units");
                stop_active_units(&active_units).await;
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("managed harness received SIGINT, stopping active units");
                stop_active_units(&active_units).await;
                return Ok(());
            }
            res = attempt_fut => {
                stop_active_units(&active_units).await;
                if started.elapsed() >= Duration::from_secs(30) {
                    attempt = 0;
                }
                if let Err(error) = res {
                    attempt = attempt.saturating_add(1);
                    let delay = Duration::from_secs((2u64.saturating_pow(attempt.min(4))).min(15));
                    tracing::warn!(error = %error, retry_secs = delay.as_secs(), "managed harness relay reconnecting");
                    tokio::select! {
                        _ = term.recv() => {
                            stop_active_units(&active_units).await;
                            return Ok(());
                        }
                        _ = tokio::signal::ctrl_c() => {
                            stop_active_units(&active_units).await;
                            return Ok(());
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            }
        }
        tokio::select! {
            _ = term.recv() => {
                stop_active_units(&active_units).await;
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                stop_active_units(&active_units).await;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

async fn connect(
    request: tokio_tungstenite::tungstenite::http::Request<()>,
    active_units: ActiveUnits,
) -> Result<()> {
    ensure!(Path::new(DESKTOP).exists(), "Desktop is still starting");
    let config = WebSocketConfig::default().max_message_size(Some(LIMIT));
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(8),
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await??;
    tokio::time::timeout(
        WS_SEND_TIMEOUT,
        socket.send(Message::Text(
            json!({"type":"inventory","roots":["work"],
                "host_info":{"os":"linux","arch":env::consts::ARCH,"version":env!("CARGO_PKG_VERSION"),
                    "model_access":Path::new("/usr/local/bin/venticode-cli").is_file()}
            })
            .to_string()
            .into(),
        )),
    )
    .await
    .context("Inventory send timed out")??;
    let (reply, mut frames) = mpsc::channel(16);
    let mut jobs = JoinSet::new();
    let mut active: HashMap<Uuid, (tokio::task::AbortHandle, mpsc::Sender<Value>)> = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                ensure!(heard.elapsed() < Duration::from_secs(20), "Relay heartbeat timed out");
                tokio::time::timeout(
                    WS_PING_TIMEOUT,
                    socket.send(Message::Ping(Default::default())),
                )
                .await
                .context("Relay ping send timed out")??;
            }
            Some(frame) = frames.recv() => {
                tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(frame))
                    .await
                    .context("Relay frame send timed out")??;
            }
            Some(job) = jobs.join_next(), if !jobs.is_empty() => {
                if let Ok(id) = job { active.remove(&id); }
            }
            frame = socket.next() => {
                heard = tokio::time::Instant::now();
                let Some(frame) = frame else { break; };
                let text = match frame? {
                    Message::Text(text) => text,
                    Message::Ping(_) => {
                        tokio::time::timeout(WS_PING_TIMEOUT, socket.flush())
                            .await
                            .context("Relay pong flush timed out")??;
                        continue;
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(mut frame) = serde_json::from_str::<Value>(&text) else {
                    tracing::warn!("ignoring malformed managed WebSocket frame");
                    continue;
                };
                let Ok(id) = serde_json::from_value::<Uuid>(frame["id"].clone()) else {
                    tracing::warn!("ignoring managed frame without valid UUID id");
                    continue;
                };
                match frame["type"].as_str() {
                    Some("execute") if active.len() < 32 && !active.contains_key(&id) => {
                        let payload = frame["payload"].take();
                        let out = reply.clone();
                        let units = active_units.clone();
                        let (responses, inbox) = mpsc::channel(8);
                        let handle = jobs.spawn(async move {
                            let result = execute(id, payload, &out, inbox, units).await;
                            if let Err(error) = &result {
                                let _ = runtime::output(&out, id, json!({"type":"error","error":error.to_string()})).await;
                            }
                            let _ = out.send(Message::Text(json!({"id":id,"type":"complete",
                                "success":result.is_ok()}).to_string().into())).await;
                            id
                        });
                        active.insert(id, (handle, responses));
                    }
                    Some("execute") if !active.contains_key(&id) => {
                        let err_frame = json!({
                            "type": "output",
                            "id": id,
                            "result": {"type": "error", "error": "Harness concurrency limit reached"}
                        });
                        tokio::time::timeout(
                            WS_SEND_TIMEOUT,
                            socket.send(Message::Text(err_frame.to_string().into())),
                        )
                        .await
                        .context("Rejection error send timed out")??;
                        let complete_frame = json!({"id": id, "type": "complete", "success": false});
                        tokio::time::timeout(
                            WS_SEND_TIMEOUT,
                            socket.send(Message::Text(complete_frame.to_string().into())),
                        )
                        .await
                        .context("Rejection complete send timed out")??;
                    }
                    Some("model_result") => {
                        if let Some((_, replies)) = active.get(&id) { let _ = replies.try_send(frame); }
                    }
                    Some("cancel") => {
                        if let Some((handle, _)) = active.remove(&id) { handle.abort(); }
                    }
                    _ => {}
                }
            }
        }
    }
    jobs.abort_all();
    stop_active_units(&active_units).await;
    Ok(())
}

fn unit_command(unit: &str) -> Command {
    let mut command = Command::new("/usr/local/bin/systemd-run");
    command.arg(format!("--unit={unit}")).args([
        "--quiet",
        "--collect",
        "--no-ask-password",
        "--expand-environment=no",
        "--uid=venticode",
        "--gid=venticode",
        "--service-type=exec",
        "--property=NoNewPrivileges=yes",
        "--property=UMask=0002",
        "--property=RuntimeMaxSec=10min",
        "--property=KillMode=control-group",
        "--property=TasksMax=128",
        "--property=MemoryMax=2G",
        "--property=CPUQuota=200%",
        "--property=TimeoutStopSec=3",
        "--setenv=HOME=/data/home",
    ]);
    command
}

struct StopUnit {
    unit: String,
    active_units: ActiveUnits,
}

impl StopUnit {
    fn new(unit: String, active_units: ActiveUnits) -> Self {
        active_units
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(unit.clone());
        Self { unit, active_units }
    }
}

impl Drop for StopUnit {
    fn drop(&mut self) {
        let should_stop = self
            .active_units
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.unit);
        if !should_stop {
            return;
        }
        let unit = self.unit.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                SHUTDOWN_STOP_TIMEOUT,
                Command::new("/usr/local/bin/systemctl")
                    .args(["stop", &unit])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            )
            .await;
        });
    }
}

async fn active_preview_count() -> Result<usize> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("/usr/local/bin/systemctl")
            .args([
                "list-units",
                "--type=service",
                "--state=active,activating",
                "--no-legend",
                "--plain",
                "venticode-preview-*.service",
            ])
            .output(),
    )
    .await
    .context("Timed out checking active preview services")??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}

async fn execute(
    id: Uuid,
    payload: Value,
    tx: &mpsc::Sender<Message>,
    inbox: mpsc::Receiver<Value>,
    active_units: ActiveUnits,
) -> Result<()> {
    ensure!(
        payload["root"] == "work" && payload["operation"] == "project_tool",
        "Invalid managed request"
    );
    let request = &payload["arguments"];
    let op = request["operation"].as_str().context("Missing operation")?;
    if op.starts_with("browser_") || op.starts_with("desktop_") {
        let reply = tokio::time::timeout(DESKTOP_RPC_TIMEOUT, async {
            let mut stream = UnixStream::connect(DESKTOP).await?;
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"operation":op,"arguments":request["arguments"]})
                    )
                    .as_bytes(),
                )
                .await?;
            line(&mut BufReader::new(stream), LIMIT).await
        })
        .await
        .context("Desktop service RPC timed out")??;
        ensure!(
            reply["error"].is_null(),
            "{}",
            reply["error"].as_str().unwrap_or("Desktop service error")
        );
        return runtime::output(tx, id, json!({"type":"result","value":reply["value"]})).await;
    }
    if op == "project_service_start" {
        ensure!(
            active_preview_count().await? < MAX_PREVIEW_SERVICES,
            "Maximum of {MAX_PREVIEW_SERVICES} concurrent preview services reached"
        );
        let run: Uuid = serde_json::from_value(request["arguments"]["run_id"].clone())?;
        let command = request["arguments"]["command"]
            .as_str()
            .context("Missing command")?;
        let project_path = request["project_path"]
            .as_str()
            .context("Missing project path")?
            .to_owned();
        let path = tokio::task::spawn_blocking(move || -> Result<std::path::PathBuf> {
            let base = ventiharness_terminal::files::Files::open(Path::new("/data/work"))?;
            base.directory(&project_path)
        })
        .await??;
        let name = format!("venticode-preview-{}-{}", run.simple(), id.simple());
        let status = unit_command(&name)
            .arg("--working-directory")
            .arg(path)
            .args(["--", "/bin/sh", "-c", command])
            .status()
            .await?;
        ensure!(status.success(), "Could not start project service");
        active_units
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(format!("{name}.service"));
        return runtime::output(tx, id, json!({"unit":name})).await;
    }
    if op == "project_service_stop" {
        let run: Uuid = serde_json::from_value(request["arguments"]["run_id"].clone())?;
        let prefix = format!("venticode-preview-{}-", run.simple());
        let _ = tokio::time::timeout(
            SHUTDOWN_STOP_TIMEOUT,
            Command::new("/usr/local/bin/systemctl")
                .args(["stop", &format!("{prefix}*.service")])
                .status(),
        )
        .await;
        active_units
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|u| !u.starts_with(&prefix));
        return runtime::output(tx, id, json!({"type":"result","value":true})).await;
    }
    ensure!(
        !matches!(op, "terminal_start" | "agent_start"),
        "Interactive process is unavailable in the managed harness"
    );
    let unit = format!("venticode-task-{}.service", id.simple());
    let _cleanup = StopUnit::new(unit.clone(), active_units);
    let mut command = unit_command(&unit);
    command.args(["--wait", "--pipe"]);
    let model =
        op == "ask_model" || request["arguments"]["model_access"] == true && op == "execute";
    let socket = format!("/run/venticode-harness/{id}.sock");
    let _socket = model.then(|| RemoveSocket(socket.clone()));
    let listener = if model {
        use std::os::unix::fs::PermissionsExt;
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
        command.arg(format!("--setenv=VENTICODE_MODEL_SOCKET={socket}"));
        Some(listener)
    } else {
        None
    };
    let mut child = command
        .args(["--", "/usr/local/bin/venticode-harness", "worker"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("Worker stdin unavailable")?;
    stdin
        .write_all(format!("{}\n", json!({"id":id,"payload":payload})).as_bytes())
        .await?;
    drop(stdin);
    let mut stdout = BufReader::new(child.stdout.take().context("Worker stdout unavailable")?);
    let stderr_task = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let remaining = WORKER_STDERR_CAP.saturating_sub(buf.len());
                        if remaining > 0 {
                            buf.extend_from_slice(&chunk[..n.min(remaining)]);
                        }
                    }
                }
            }
            buf
        })
    });
    let work = async {
        let mut saw_error = false;
        loop {
            let mut bytes = Vec::new();
            if (&mut stdout)
                .take(LIMIT as u64)
                .read_until(b'\n', &mut bytes)
                .await?
                == 0
            {
                break;
            }
            ensure!(bytes.ends_with(b"\n"), "Oversized worker frame");
            let data: Value = serde_json::from_slice(&bytes)?;
            if data["type"] == "error" {
                saw_error = true;
            }
            runtime::output(tx, id, data).await?;
        }
        let status = child.wait().await?;
        let err_buf = if let Some(mut task) = stderr_task {
            match tokio::time::timeout(Duration::from_secs(1), &mut task).await {
                Ok(Ok(buf)) => buf,
                _ => {
                    task.abort();
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if !status.success() {
            let detail_full = String::from_utf8_lossy(&err_buf);
            let detail = detail_full.trim();
            let detail_excerpt: String = detail.chars().take(4096).collect();
            if !detail_excerpt.is_empty() {
                tracing::warn!(job_id = %id, stderr = %detail_excerpt, "managed worker failed");
            }
            if saw_error {
                anyhow::bail!("Managed worker reported an error");
            }
            if detail_excerpt.is_empty() {
                anyhow::bail!("Managed command failed");
            }
            anyhow::bail!("Managed command failed: {detail_excerpt}");
        }
        ensure!(!saw_error, "Managed worker reported an error");
        Ok(())
    };
    if let Some(listener) = listener {
        tokio::select! {
            result = work => result,
            _ = broker(listener, &unit, id, tx, inbox) => {
                anyhow::bail!("Model broker stopped unexpectedly")
            }
        }
    } else {
        work.await
    }
}

struct RemoveSocket(String);
impl Drop for RemoveSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn line(stream: &mut (impl tokio::io::AsyncBufRead + Unpin), max: usize) -> Result<Value> {
    let mut data = Vec::new();
    stream.take(max as u64).read_until(b'\n', &mut data).await?;
    ensure!(
        data.ends_with(b"\n"),
        "Incomplete or oversized JSON request"
    );
    Ok(serde_json::from_slice(&data)?)
}

async fn write_peer_json(stream: &mut UnixStream, value: &Value) {
    let payload = format!("{value}\n");
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = stream.write_all(payload.as_bytes()).await;
        let _ = stream.flush().await;
    })
    .await;
}

async fn broker(
    listener: UnixListener,
    unit: &str,
    id: Uuid,
    tx: &mpsc::Sender<Message>,
    mut inbox: mpsc::Receiver<Value>,
) -> Result<()> {
    let expected_uid = venticode_uid();
    let unit_owned = unit.to_owned();
    let inflight: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> = Arc::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let concurrency = Arc::new(Semaphore::new(MAX_CONCURRENT_MODEL_CALLS));
    let mut peer_tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            maybe_frame = inbox.recv() => {
                let Some(frame) = maybe_frame else {
                    return Ok(());
                };
                if let Some(query) = frame["query"].as_str() {
                    let waiter = inflight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(query);
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(frame);
                    }
                }
            }
            Some(_) = peer_tasks.join_next(), if !peer_tasks.is_empty() => {}
            accept_res = listener.accept() => {
                let (mut peer, _) = match accept_res {
                    Ok(conn) => conn,
                    Err(error) => {
                        tracing::warn!(job_id = %id, error = %error, "model broker accept error");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(creds) = peer.peer_cred() else {
                    continue;
                };
                let Some(pid) = creds.pid() else {
                    continue;
                };
                let cgroup = tokio::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                    .await
                    .unwrap_or_default();
                if creds.uid() != expected_uid
                    || !cgroup
                        .lines()
                        .any(|entry| entry.rsplit('/').next() == Some(unit_owned.as_str()))
                {
                    continue;
                }
                if calls.load(Ordering::Acquire) >= MAX_MODEL_CALLS {
                    peer_tasks.spawn(async move {
                        write_peer_json(
                            &mut peer,
                            &json!({"error": "Per-execution model call quota exceeded"}),
                        )
                        .await;
                    });
                    continue;
                }
                let Ok(permit) = concurrency.clone().try_acquire_owned() else {
                    peer_tasks.spawn(async move {
                        write_peer_json(
                            &mut peer,
                            &json!({"error": "Model broker busy; too many concurrent requests"}),
                        )
                        .await;
                    });
                    continue;
                };
                let inflight_ref = inflight.clone();
                let calls_ref = calls.clone();
                let tx_ref = tx.clone();
                peer_tasks.spawn(async move {
                    let _permit = permit;
                    let mut peer = BufReader::new(peer);
                    let Ok(Ok(request)) = tokio::time::timeout(
                        Duration::from_secs(2),
                        line(&mut peer, BROKER_REQUEST_LIMIT),
                    )
                    .await
                    else {
                        write_peer_json(
                            peer.get_mut(),
                            &json!({"error": "Invalid or oversized model request"}),
                        )
                        .await;
                        return;
                    };
                    let Some(prompt) = request["prompt"]
                        .as_str()
                        .filter(|p| !p.is_empty() && p.len() <= 32768)
                    else {
                        write_peer_json(
                            peer.get_mut(),
                            &json!({"error": "Prompt must be 1..=32768 bytes"}),
                        )
                        .await;
                        return;
                    };
                    let reserved = calls_ref.try_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                        (c < MAX_MODEL_CALLS).then_some(c + 1)
                    });
                    if reserved.is_err() {
                        write_peer_json(
                            peer.get_mut(),
                            &json!({"error": "Per-execution model call quota exceeded"}),
                        )
                        .await;
                        return;
                    }
                    let query = Uuid::new_v4();
                    let query_str = query.to_string();
                    let (waiter_tx, waiter_rx) = oneshot::channel();
                    inflight_ref
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(query_str.clone(), waiter_tx);
                    if runtime::output(
                        &tx_ref,
                        id,
                        json!({"type":"model_request","query":query,"prompt":prompt}),
                    )
                    .await
                    .is_err()
                    {
                        inflight_ref
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&query_str);
                        write_peer_json(
                            peer.get_mut(),
                            &json!({"error": "Relay channel closed"}),
                        )
                        .await;
                        return;
                    }
                    let response = match tokio::time::timeout(Duration::from_secs(50), waiter_rx).await {
                        Ok(Ok(frame)) => frame,
                        _ => {
                            inflight_ref
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&query_str);
                            json!({"error": "Model request timed out"})
                        }
                    };
                    write_peer_json(peer.get_mut(), &response).await;
                });
            }
        }
    }
}

pub async fn worker() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut request = line(&mut input, LIMIT).await?;
    let id: Uuid = serde_json::from_value(request["id"].clone())?;
    let payload = request["payload"].take();
    let (tx, mut rx) = mpsc::channel(8);
    let roots = HashMap::from([("work".into(), "/data/work".into())]);
    let mut stdout = tokio::io::stdout();
    let work = async {
        if payload["arguments"]["operation"] == "ask_model" {
            let prompt = payload["arguments"]["arguments"]["prompt"]
                .as_str()
                .context("Missing prompt")?;
            let mut cli = Command::new("/usr/local/bin/venticode-cli")
                .args(["ask", "--json"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            cli.stdin
                .take()
                .context("CLI stdin unavailable")?
                .write_all(prompt.as_bytes())
                .await?;
            let reply = cli.wait_with_output().await?;
            if !reply.status.success() {
                let err = String::from_utf8_lossy(&reply.stderr).trim().to_owned();
                if err.is_empty() {
                    anyhow::bail!("Model request failed");
                }
                anyhow::bail!("Model request failed: {err}");
            }
            return runtime::output(&tx, id, serde_json::from_slice(&reply.stdout)?).await;
        }
        runtime::Runtime::new()?
            .execute(
                id,
                payload,
                &roots,
                Mode::Sandboxed,
                Path::new("/data/work"),
                &tx,
            )
            .await
    };
    let mut work = std::pin::pin!(work);
    let forward = |frame: Message| -> Result<Vec<u8>> {
        let frame: Value = serde_json::from_str(frame.to_text()?)?;
        Ok(format!("{}\n", frame["result"]).into_bytes())
    };
    loop {
        tokio::select! {
            result = &mut work => {
                while let Ok(frame) = rx.try_recv() { stdout.write_all(&forward(frame)?).await?; }
                if let Err(error) = result {
                    stdout.write_all(format!("{}\n",json!({"type":"error","error":error.to_string()})).as_bytes()).await?;
                    stdout.flush().await?;
                    std::process::exit(1);
                }
                stdout.flush().await?;
                return Ok(());
            }
            Some(frame) = rx.recv() => {
                stdout.write_all(&forward(frame)?).await?;
                stdout.flush().await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_passwd_uid_finds_user_and_falls_back() {
        let passwd =
            "root:x:0:0:root:/root:/bin/bash\nventicode:x:10042:10042::/data/home:/bin/sh\n";
        assert_eq!(parse_passwd_uid(passwd, "venticode"), Some(10042));
        assert_eq!(parse_passwd_uid(passwd, "missing"), None);
    }

    #[test]
    fn unit_command_sets_memory_and_cpu_limits() {
        let cmd = unit_command("venticode-task-test.service");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a.starts_with("--property=MemoryMax=")));
        assert!(args.iter().any(|a| a.starts_with("--property=CPUQuota=")));
    }
}
