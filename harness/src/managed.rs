use crate::{protocol::Mode, runtime};
use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::HashMap, env, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::mpsc,
    task::JoinSet,
};
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};
use uuid::Uuid;

const DESKTOP: &str = "/run/venticode-desktop/bridge.sock";
const IDENTITY: &str = "/var/run/secrets/venticode-computer/token";
const LIMIT: usize = 8 * 1024 * 1024;

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
    loop {
        let started = tokio::time::Instant::now();
        let attempt_fut = async {
            let mut request = origin.as_str().into_client_request()?;
            let token = tokio::fs::read_to_string(IDENTITY).await?;
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {}", token.trim()).parse()?);
            connect(request).await
        };
        tokio::select! {
            _ = term.recv() => {
                tracing::info!("managed harness received SIGTERM, exiting");
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
            res = attempt_fut => {
                if started.elapsed() >= Duration::from_secs(30) {
                    attempt = 0;
                }
                if let Err(error) = res {
                    attempt = attempt.saturating_add(1);
                    let delay = Duration::from_secs((2u64.saturating_pow(attempt.min(4))).min(15));
                    tracing::warn!(error = %error, retry_secs = delay.as_secs(), "managed harness relay reconnecting");
                    tokio::select! {
                        _ = term.recv() => return Ok(()),
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            }
        }
        tokio::select! {
            _ = term.recv() => return Ok(()),
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

async fn connect(request: tokio_tungstenite::tungstenite::http::Request<()>) -> Result<()> {
    ensure!(Path::new(DESKTOP).exists(), "Desktop is still starting");
    let config = WebSocketConfig::default().max_message_size(Some(LIMIT));
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(8),
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await??;
    socket.send(Message::Text(json!({"type":"inventory","roots":["work"],
        "host_info":{"os":"linux","arch":env::consts::ARCH,"version":env!("CARGO_PKG_VERSION"),
            "model_access":Path::new("/usr/local/bin/venticode-cli").is_file()}
    }).to_string().into())).await?;
    let (reply, mut frames) = mpsc::channel(16);
    let mut jobs = JoinSet::new();
    let mut active: HashMap<Uuid, (tokio::task::AbortHandle, mpsc::Sender<Value>)> = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                ensure!(heard.elapsed() < Duration::from_secs(20), "Relay heartbeat timed out");
                socket.send(Message::Ping(Default::default())).await?;
            }
            Some(frame) = frames.recv() => socket.send(frame).await?,
            Some(job) = jobs.join_next(), if !jobs.is_empty() => {
                if let Ok(id) = job { active.remove(&id); }
            }
            frame = socket.next() => {
                heard = tokio::time::Instant::now();
                let Some(frame) = frame else { break; };
                let text = match frame? {
                    Message::Text(text) => text,
                    Message::Ping(_) => { socket.flush().await?; continue; }
                    Message::Close(_) => break,
                    _ => continue,
                };
                let mut frame: Value = serde_json::from_str(&text)?;
                let id: Uuid = serde_json::from_value(frame["id"].clone())?;
                match frame["type"].as_str() {
                    Some("execute") if active.len() < 32 && !active.contains_key(&id) => {
                        let payload = frame["payload"].take();
                        let out = reply.clone();
                        let (responses, inbox) = mpsc::channel(1);
                        let handle = jobs.spawn(async move {
                            let result = execute(id, payload, &out, inbox).await;
                            if let Err(error) = &result {
                                let _ = runtime::output(&out,id,json!({"type":"error","error":error.to_string()})).await;
                            }
                            let _ = out.send(Message::Text(json!({"id":id,"type":"complete",
                                "success":result.is_ok()}).to_string().into())).await;
                            id
                        });
                        active.insert(id, (handle, responses));
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
        "--property=TimeoutStopSec=3",
        "--setenv=HOME=/data/home",
    ]);
    command
}

struct StopUnit(String);
impl Drop for StopUnit {
    fn drop(&mut self) {
        let unit = self.0.clone();
        tokio::spawn(async move {
            let _ = Command::new("/usr/local/bin/systemctl")
                .args(["stop", &unit])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        });
    }
}

async fn execute(
    id: Uuid,
    payload: Value,
    tx: &mpsc::Sender<Message>,
    inbox: mpsc::Receiver<Value>,
) -> Result<()> {
    ensure!(
        payload["root"] == "work" && payload["operation"] == "project_tool",
        "Invalid managed request"
    );
    let request = &payload["arguments"];
    let op = request["operation"].as_str().context("Missing operation")?;
    if op.starts_with("browser_") || op.starts_with("desktop_") {
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
        let reply = line(&mut BufReader::new(stream), LIMIT).await?;
        ensure!(
            reply["error"].is_null(),
            "{}",
            reply["error"].as_str().unwrap_or("Desktop service error")
        );
        return runtime::output(tx, id, json!({"type":"result","value":reply["value"]})).await;
    }
    if op == "project_service_start" {
        let run: Uuid = serde_json::from_value(request["arguments"]["run_id"].clone())?;
        let command = request["arguments"]["command"]
            .as_str()
            .context("Missing command")?;
        let base = ventiharness_terminal::files::Files::open(Path::new("/data/work"))?;
        let path = base.directory(
            request["project_path"]
                .as_str()
                .context("Missing project path")?,
        )?;
        let name = format!("venticode-preview-{}-{}", run.simple(), id.simple());
        let status = unit_command(&name)
            .arg("--working-directory")
            .arg(path)
            .args(["--", "/bin/sh", "-c", command])
            .status()
            .await?;
        ensure!(status.success(), "Could not start project service");
        return runtime::output(tx, id, json!({"unit":name})).await;
    }
    if op == "project_service_stop" {
        let run: Uuid = serde_json::from_value(request["arguments"]["run_id"].clone())?;
        let _ = Command::new("/usr/local/bin/systemctl")
            .args([
                "stop",
                &format!("venticode-preview-{}-*.service", run.simple()),
            ])
            .status()
            .await?;
        return runtime::output(tx, id, json!({"type":"result","value":true})).await;
    }
    ensure!(
        !matches!(op, "terminal_start" | "agent_start"),
        "Interactive process is unavailable in the managed harness"
    );
    let unit = format!("venticode-task-{}.service", id.simple());
    let _cleanup = StopUnit(unit.clone());
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
    let mut stderr = child.stderr.take();
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
        if !status.success() {
            let mut err_buf = Vec::new();
            if let Some(se) = stderr.take() {
                let _ = se.take(4096).read_to_end(&mut err_buf).await;
            }
            let detail = String::from_utf8_lossy(&err_buf).trim().to_owned();
            if !detail.is_empty() {
                tracing::warn!(job_id = %id, stderr = %detail, "managed worker failed");
            }
            if saw_error {
                anyhow::bail!("Managed worker reported an error");
            }
            if detail.is_empty() {
                anyhow::bail!("Managed command failed");
            }
            anyhow::bail!("Managed command failed: {detail}");
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

async fn broker(
    listener: UnixListener,
    unit: &str,
    id: Uuid,
    tx: &mpsc::Sender<Message>,
    mut inbox: mpsc::Receiver<Value>,
) -> Result<()> {
    loop {
        let (peer, _) = listener.accept().await?;
        let creds = peer.peer_cred()?;
        let Some(pid) = creds.pid() else { continue };
        let cgroup = tokio::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .await
            .unwrap_or_default();
        if creds.uid() != 10001
            || !cgroup
                .lines()
                .any(|entry| entry.rsplit('/').next() == Some(unit))
        {
            continue;
        }
        let mut peer = BufReader::new(peer);
        let Ok(Ok(request)) =
            tokio::time::timeout(Duration::from_secs(2), line(&mut peer, 48 * 1024)).await
        else {
            continue;
        };
        let Some(prompt) = request["prompt"]
            .as_str()
            .filter(|p| !p.is_empty() && p.len() <= 32768)
        else {
            continue;
        };
        let query = Uuid::new_v4();
        let query_str = query.to_string();
        while inbox.try_recv().is_ok() {}
        runtime::output(
            tx,
            id,
            json!({"type":"model_request","query":query,"prompt":prompt}),
        )
        .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(50);
        let response = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break json!({"error":"Model request timed out"});
            }
            match tokio::time::timeout(remaining, inbox.recv()).await {
                Ok(Some(frame)) if frame["query"] == query_str => break frame,
                Ok(Some(_)) => continue,
                _ => break json!({"error":"Model request timed out"}),
            }
        };
        let _ = peer
            .get_mut()
            .write_all(format!("{}\n", response).as_bytes())
            .await;
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
