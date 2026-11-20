use crate::protocol::{Agent, Mode, Request};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use ventiharness_terminal::{
    files::{Edit, Files},
    process,
    pty::Terminal,
};

type Reply = mpsc::Sender<Message>;
type Sessions = Arc<Mutex<HashMap<Uuid, (Uuid, String, mpsc::Sender<Value>)>>>;

const AGENT_STDIN_FRAME_LIMIT: usize = 131072;
const AGENT_STDOUT_FRAME_LIMIT: usize = 2 * 1024 * 1024;
const AGENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Runtime {
    clients: Arc<crate::mcp::Clients>,
    sessions: Sessions,
    preview_http: reqwest_mcp::Client,
}

struct Session {
    id: Uuid,
    sessions: Sessions,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.sessions.lock().unwrap().remove(&self.id);
    }
}

struct BlockingDropTerminal(Arc<Mutex<Option<Terminal>>>);
impl Drop for BlockingDropTerminal {
    fn drop(&mut self) {
        if let Some(terminal) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tokio::task::spawn_blocking(move || drop(terminal));
        }
    }
}

pub async fn output(tx: &Reply, id: Uuid, result: Value) -> Result<()> {
    let frame = json!({"type":"output","id":id,"result":result}).to_string();
    ensure!(
        frame.len() <= 8 * 1024 * 1024,
        "Harness frame exceeds 8 MiB"
    );
    tx.send(Message::Text(frame.into())).await?;
    Ok(())
}

fn text<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .with_context(|| format!("Missing {name}"))
}

// A missing file is an empty preimage. Failed or partial reads cannot describe a diff.
fn project_text(files: &Files, path: &str) -> Option<String> {
    match files.read(path) {
        Ok(mut value) if value["encoding"] == "utf8" && value["truncated"] == false => {
            serde_json::from_value(value["content"].take()).ok()
        }
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Some(String::new())
        }
        _ => None,
    }
}

fn project_diff(path: &str, before: &str, after: &str) -> Value {
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(200))
        .diff_lines(before, after);
    let (mut additions, mut deletions) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => additions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    json!({
        "path": path,
        "additions": additions,
        "deletions": deletions,
        "patch": diff
            .unified_diff()
            .header(&format!("a/{path}"), &format!("b/{path}"))
            .to_string()
    })
}

async fn write_agent_frame(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= AGENT_STDIN_FRAME_LIMIT,
        "Agent input too large"
    );
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    Ok(())
}

impl Runtime {
    pub fn new() -> Result<Self> {
        let preview_http = reqwest_mcp::Client::builder()
            .no_proxy()
            .redirect(reqwest_mcp::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            sessions: Arc::default(),
            clients: Arc::default(),
            preview_http,
        })
    }

    pub fn release(&self, id: Uuid) {
        self.clients.release(id);
    }

    pub fn evict_idle(&self, max_idle: Duration) {
        self.clients.evict_idle(max_idle);
    }

    #[cfg(feature = "ventic-relay")]
    pub fn retain(&self, consents: &[Value]) {
        self.clients.retain(consents);
    }

    fn session(&self, id: Uuid, request: &Request) -> (Session, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(8);
        self.sessions
            .lock()
            .unwrap()
            .insert(id, (request.consent_id, request.root.clone(), tx));
        (
            Session {
                id,
                sessions: self.sessions.clone(),
            },
            rx,
        )
    }

    async fn send_input(&self, request: &Request) -> Result<()> {
        let id: Uuid = text(&request.arguments, "session_id")?.parse()?;
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            let (consent, root, tx) = sessions.get(&id).context("Session unavailable")?;
            ensure!(
                *consent == request.consent_id && *root == request.root,
                "Session not granted"
            );
            tx.clone()
        };
        tx.send(json!({"operation":request.operation,"arguments":request.arguments}))
            .await?;
        Ok(())
    }

    pub async fn execute(
        &self,
        id: Uuid,
        payload: Value,
        roots: &HashMap<String, PathBuf>,
        daemon_mode: Mode,
        default_cwd: &Path,
        tx: &Reply,
    ) -> Result<()> {
        let mut request: Request = serde_json::from_value(payload)?;
        let yolo = daemon_mode == Mode::Yolo;

        // Old harnesses reject this operation. Never run a subfolder request relative to the entire grant.
        let include_diff =
            request.operation == "project_tool" && request.arguments["include_diff"] == true;
        let is_project_tool = request.operation == "project_tool";
        let project_path = if is_project_tool {
            let p = text(&request.arguments, "project_path")?.to_owned();
            request.operation = text(&request.arguments, "operation")?.to_owned();
            request.arguments = request.arguments["arguments"].take();
            Some(p)
        } else {
            None
        };

        let root_entry = roots.get(&request.root).cloned();
        let default_cwd_owned = default_cwd.to_path_buf();
        let files: Arc<Files> = tokio::task::spawn_blocking(move || -> Result<Arc<Files>> {
            let files = if is_project_tool {
                let root = root_entry.context("Missing project root")?;
                let scoped = Files::open(&root)?;
                let directory = scoped.directory(project_path.as_deref().unwrap_or("."))?;
                Files::open(&directory)?
            } else if yolo {
                let base = root_entry.unwrap_or(default_cwd_owned);
                Files::open_yolo(&base)?
            } else {
                let root = root_entry.context("Root not granted locally")?;
                ensure!(
                    root.canonicalize()? == root,
                    "Granted root changed; pair again"
                );
                Files::open(&root)?
            };
            Ok(Arc::new(files))
        })
        .await??;

        let args = &request.arguments;
        let path = args["path"].as_str().unwrap_or(".").to_owned();
        let result = match request.operation.as_str() {
            "list_files" => {
                let files = files.clone();
                tokio::task::spawn_blocking(move || files.list(&path)).await??
            }
            "find_files" => {
                let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;
                let pattern = args["pattern"].as_str().map(str::to_owned);
                let files = files.clone();
                tokio::task::spawn_blocking(move || {
                    files.find_files(&path, pattern.as_deref(), max_results)
                })
                .await??
            }
            "search_files" => {
                let query = text(args, "query")?.to_owned();
                let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(false);
                let use_regex = args["regex"].as_bool().unwrap_or(false);
                let max_results = args["max_results"].as_u64().unwrap_or(100) as usize;
                let pattern = args["pattern"].as_str().map(str::to_owned);
                let files = files.clone();
                tokio::task::spawn_blocking(move || {
                    files.search_files(
                        &path,
                        &query,
                        pattern.as_deref(),
                        case_sensitive,
                        use_regex,
                        max_results,
                    )
                })
                .await??
            }
            "read_file" => {
                let start_line = args["start_line"].as_u64().map(|v| v as usize);
                let end_line = args["end_line"].as_u64().map(|v| v as usize);
                let files = files.clone();
                tokio::task::spawn_blocking(move || files.read_range(&path, start_line, end_line))
                    .await??
            }
            "write_file" => {
                let encoding = args["encoding"].as_str().unwrap_or("utf8").to_owned();
                let target_path = text(args, "path")?.to_owned();
                let content_str = text(args, "content")?.to_owned();
                let files = files.clone();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let before = include_diff
                        .then(|| project_text(&files, &target_path))
                        .flatten();
                    let written_bytes = match encoding.as_str() {
                        "base64" => {
                            let raw = STANDARD
                                .decode(content_str.trim())
                                .context("Invalid base64 content for write_file")?;
                            files.write(&target_path, &raw)?;
                            raw.len()
                        }
                        "utf8" => {
                            let raw = content_str.as_bytes();
                            files.write(&target_path, raw)?;
                            raw.len()
                        }
                        _ => anyhow::bail!("Unsupported write_file encoding '{encoding}'"),
                    };
                    let mut out = json!({"written": true, "bytes": written_bytes});
                    if let Some(before) = before
                        && let Some(after) = project_text(&files, &target_path)
                    {
                        out["diff"] = project_diff(&target_path, &before, &after);
                    }
                    Ok(out)
                })
                .await??
            }
            "edit_file" => {
                let edits: Vec<Edit> = serde_json::from_value(
                    args.get("edits").cloned().context("Missing edits array")?,
                )?;
                let target_path = text(args, "path")?.to_owned();
                let expected_sha256 = args["expected_sha256"].as_str().map(str::to_owned);
                let files = files.clone();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let before = include_diff
                        .then(|| project_text(&files, &target_path))
                        .flatten();
                    let mut out = files.edit(&target_path, &edits, expected_sha256.as_deref())?;
                    if let Some(before) = before
                        && let Some(after) = project_text(&files, &target_path)
                    {
                        out["diff"] = project_diff(&target_path, &before, &after);
                    }
                    Ok(out)
                })
                .await??
            }
            "mkdir" => {
                let target_path = text(args, "path")?.to_owned();
                let files = files.clone();
                tokio::task::spawn_blocking(move || files.mkdir(&target_path)).await??;
                json!({"created":true})
            }
            "rename" => {
                let target_path = text(args, "path")?.to_owned();
                let destination = text(args, "destination")?.to_owned();
                let files = files.clone();
                tokio::task::spawn_blocking(move || files.rename(&target_path, &destination))
                    .await??;
                json!({"renamed":true})
            }
            "remove" => {
                let target_path = text(args, "path")?.to_owned();
                let files = files.clone();
                tokio::task::spawn_blocking(move || files.remove(&target_path)).await??;
                json!({"removed":true})
            }
            "execute" => {
                let files_dir = files.clone();
                let cwd = tokio::task::spawn_blocking(move || files_dir.directory(&path)).await??;
                let command = text(args, "command")?;
                let timeout = args["timeout_secs"]
                    .as_u64()
                    .map(|secs| Duration::from_secs(secs.clamp(1, 600)))
                    .unwrap_or(process::DEFAULT_TIMEOUT);
                let (events, mut rx) = mpsc::channel(8);
                let task = process::execute_with_timeout(&cwd, command, events, timeout);
                tokio::pin!(task);
                loop {
                    tokio::select! {
                        result = &mut task => {
                            result?;
                            while let Some(event) = rx.recv().await {
                                output(tx, id, serde_json::to_value(event)?).await?;
                            }
                            break;
                        }
                        Some(event) = rx.recv() => {
                            output(tx, id, serde_json::to_value(event)?).await?;
                        }
                    }
                }
                return Ok(());
            }
            "terminal_start" => {
                let files_dir = files.clone();
                let cwd = tokio::task::spawn_blocking(move || files_dir.directory(&path)).await??;
                return self.terminal(id, &request, cwd, tx).await;
            }
            "foreign_sessions" | "foreign_session_import" => {
                let files = files.clone();
                let id = args["id"].as_str().unwrap_or_default().to_owned();
                let list = request.operation == "foreign_sessions";
                tokio::task::spawn_blocking(move || {
                    let cwd = files.directory(".")?;
                    if list {
                        Ok(crate::foreign::list(&cwd))
                    } else {
                        crate::foreign::import(&cwd, &id)
                    }
                })
                .await??
            }
            "agent_list" => {
                tokio::task::spawn_blocking(|| {
                    json!({
                        "agents": Agent::ALL
                            .into_iter()
                            .map(|agent| json!({
                                "id": agent.name(),
                                "protocol": agent.protocol(),
                                "available": which::which(agent.binary()).is_ok()
                            }))
                            .collect::<Vec<_>>()
                    })
                })
                .await?
            }
            "agent_start" => {
                let files_dir = files.clone();
                let cwd = tokio::task::spawn_blocking(move || files_dir.directory(&path)).await??;
                return self.agent(id, &request, cwd, tx).await;
            }
            "terminal_input" | "terminal_resize" | "terminal_stop" | "agent_input"
            | "agent_prompt" | "agent_stop" => {
                self.send_input(&request).await?;
                json!({"accepted":true})
            }
            "mcp_list_servers" => {
                let base = files.base_path().to_path_buf();
                tokio::task::spawn_blocking(move || crate::mcp::list(&base)).await??
            }
            "mcp_list_tools" | "mcp_call_tool" => {
                if !yolo {
                    let server = text(args, "server_id")?;
                    let revision = text(args, "revision")?;
                    ensure!(
                        request
                            .mcp_grants
                            .get(server)
                            .is_some_and(|v| v == revision),
                        "MCP server configuration not approved for this conversation"
                    );
                }
                self.clients
                    .call(
                        request.consent_id,
                        files.base_path(),
                        &request.operation,
                        args,
                    )
                    .await?
            }
            "catalog_list" => {
                crate::catalog::list(text(args, "repo")?, args["ref"].as_str().unwrap_or("main"))
                    .await?
            }
            "catalog_install" => crate::catalog::install(files.clone(), args).await?,
            "catalog_remove" => crate::catalog::remove(files.clone(), args).await?,
            "catalog_installed" => {
                let files = files.clone();
                tokio::task::spawn_blocking(move || crate::catalog::installed(&files)).await?
            }
            "skill_list" => {
                let base = files.base_path().to_path_buf();
                tokio::task::spawn_blocking(move || crate::skills::list(&base)).await?
            }
            "skill_read" => {
                let base = files.base_path().to_path_buf();
                let name = text(args, "name")?.to_owned();
                tokio::task::spawn_blocking(move || crate::skills::read(&base, &name)).await??
            }
            "fetch_url" => crate::url_read::fetch(text(args, "url")?, args["raw"] == true).await?,
            "extension_list_tools" => crate::extensions::list(files.clone()).await?,
            "extension_call_tool" => {
                let tool = text(args, "tool")?.to_owned();
                let parameters = args.get("parameters").cloned().unwrap_or_else(|| json!({}));
                ensure!(
                    parameters.is_object(),
                    "Extension tool parameters must be an object"
                );
                crate::extensions::call(files.clone(), tool, parameters).await?
            }
            "preview" => {
                let port = args["port"]
                    .as_u64()
                    .filter(|p| (1024..=65535).contains(p))
                    .context("Invalid preview port (must be 1024..=65535)")?;
                let preview_path = text(args, "path")?;
                ensure!(
                    preview_path.starts_with('/')
                        && !preview_path.starts_with("//")
                        && !preview_path.contains(['\r', '\n', '\\']),
                    "Invalid preview path"
                );
                let url = format!("http://127.0.0.1:{port}{preview_path}");
                let mut response = self.preview_http.get(url).send().await?;
                let status = response.status().as_u16();
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await? {
                    ensure!(
                        bytes.len() + chunk.len() <= 512 * 1024,
                        "Preview response too large"
                    );
                    bytes.extend(chunk);
                }
                json!({"status":status,"content_type":content_type,"body":STANDARD.encode(bytes)})
            }
            _ => anyhow::bail!("Unsupported harness operation"),
        };
        output(tx, id, result).await
    }

    async fn terminal(&self, id: Uuid, request: &Request, cwd: PathBuf, tx: &Reply) -> Result<()> {
        let rows: u16 = request.arguments["rows"]
            .as_u64()
            .unwrap_or(24)
            .try_into()?;
        let cols: u16 = request.arguments["cols"]
            .as_u64()
            .unwrap_or(80)
            .try_into()?;
        let (terminal, mut reader) =
            tokio::task::spawn_blocking(move || Terminal::start(&cwd, rows, cols)).await??;
        let term = Arc::new(Mutex::new(Some(terminal)));
        let _drop_guard = BlockingDropTerminal(term.clone());
        let (_guard, mut input) = self.session(id, request);
        let (writer, mut bytes) = mpsc::channel(8);
        tokio::task::spawn_blocking(move || {
            let mut buffer = [0; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.blocking_send(STANDARD.encode(&buffer[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        output(
            tx,
            id,
            json!({"type":"session","session_id":id,"kind":"terminal"}),
        )
        .await?;
        loop {
            tokio::select! {
                bytes = bytes.recv() => match bytes {
                    Some(data) => output(tx, id, json!({"type":"output","stream":"pty","data":data})).await?,
                    None => break,
                },
                message = input.recv() => {
                    let message = message.context("Session closed")?;
                    match message["operation"].as_str() {
                        Some("terminal_input") => {
                            let decoded = STANDARD.decode(text(&message["arguments"], "data")?)?;
                            let term_arc = term.clone();
                            tokio::task::spawn_blocking(move || {
                                let mut guard = term_arc.lock().unwrap_or_else(|e| e.into_inner());
                                guard.as_mut().context("Terminal closed")?.input(&decoded)
                            })
                            .await??;
                        }
                        Some("terminal_resize") => {
                            let rows: u16 = message["arguments"]["rows"]
                                .as_u64()
                                .context("Rows required")?
                                .try_into()?;
                            let cols: u16 = message["arguments"]["cols"]
                                .as_u64()
                                .context("Columns required")?
                                .try_into()?;
                            let term_arc = term.clone();
                            tokio::task::spawn_blocking(move || {
                                let guard = term_arc.lock().unwrap_or_else(|e| e.into_inner());
                                guard.as_ref().context("Terminal closed")?.resize(rows, cols)
                            })
                            .await??;
                        }
                        Some("terminal_stop") => break,
                        _ => anyhow::bail!("Invalid terminal input"),
                    }
                }
            }
        }
        let exit_code = {
            let term_arc = term.clone();
            tokio::task::spawn_blocking(move || -> Result<Option<u32>> {
                let mut terminal = term_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                let code = match terminal.as_mut() {
                    Some(t) => t.status()?,
                    None => None,
                };
                drop(terminal);
                Ok(code)
            })
            .await??
        };
        output(tx, id, json!({"type":"exit","code":exit_code})).await
    }

    async fn agent(&self, id: Uuid, request: &Request, cwd: PathBuf, tx: &Reply) -> Result<()> {
        let agent: Agent = serde_json::from_value(request.arguments["agent"].clone())?;
        let protocol = Arc::new(Mutex::new(crate::agents::Session::new(
            agent,
            &cwd,
            request.arguments["prompt"].as_str(),
        )?));
        let binary_name = agent.binary();
        let binary = tokio::task::spawn_blocking(move || which::which(binary_name))
            .await?
            .context("Install and sign in to the agent locally first")?;
        let args = agent
            .arguments()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut cmd = process::command(
            binary.to_str().context("Invalid executable path")?,
            &args,
            &cwd,
        );
        cmd.env_remove("CLAUDECODE");
        let mut child = process::spawn(cmd)?;
        let mut stdin = child.stdin().take().context("Missing agent stdin")?;
        let stdout = child.stdout().take().context("Missing agent stdout")?;
        let stderr = child.stderr().take().context("Missing agent stderr")?;
        let (_guard, mut input) = self.session(id, request);
        output(
            tx,
            id,
            json!({"type":"session","session_id":id,"agent":agent.name(),"protocol":agent.protocol()}),
        )
        .await?;
        let (initial, initial_ready) = {
            let mut guard = protocol.lock().unwrap();
            let msgs = guard.start();
            (msgs, guard.is_ready())
        };
        for message in initial {
            write_agent_frame(&mut stdin, &message).await?;
        }
        stdin.flush().await?;

        let handshake_ready = Arc::new(AtomicBool::new(initial_ready));
        let (proto_tx, mut proto_rx) = mpsc::unbounded_channel::<Value>();

        let proto_read = protocol.clone();
        let ready_read = handshake_ready.clone();
        let read = async {
            let mut stdout = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                let size = (&mut stdout)
                    .take((AGENT_STDOUT_FRAME_LIMIT + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .await?;
                if size == 0 {
                    return Ok::<_, anyhow::Error>(());
                }
                ensure!(
                    size <= AGENT_STDOUT_FRAME_LIMIT && line.ends_with(b"\n"),
                    "Agent frame too large or incomplete"
                );
                let event: Value = match serde_json::from_slice(&line) {
                    Ok(event) => event,
                    Err(error) => {
                        let preview = String::from_utf8_lossy(&line);
                        let trimmed = preview.trim();
                        if !trimmed.is_empty() {
                            tracing::warn!(
                                job_id = %id,
                                agent = agent.name(),
                                error = %error,
                                line = %trimmed.chars().take(200).collect::<String>(),
                                "skipping non-JSON stdout line from agent"
                            );
                        }
                        continue;
                    }
                };
                let (outgoing, is_ready) = {
                    let mut guard = proto_read.lock().unwrap();
                    let out = guard.receive(&event)?;
                    (out, guard.is_ready())
                };
                if is_ready {
                    ready_read.store(true, Ordering::Release);
                }
                for message in outgoing {
                    let _ = proto_tx.send(message);
                }
                output(tx, id, json!({"type":"agent_event","event":event})).await?;
            }
        };

        let proto_write = protocol.clone();
        let write = async {
            loop {
                tokio::select! {
                    biased;
                    Some(proto_msg) = proto_rx.recv() => {
                        write_agent_frame(&mut stdin, &proto_msg).await?;
                        while let Ok(next_msg) = proto_rx.try_recv() {
                            write_agent_frame(&mut stdin, &next_msg).await?;
                        }
                        stdin.flush().await?;
                    }
                    message = input.recv() => {
                        let Some(message) = message else {
                            return Ok::<_, anyhow::Error>(());
                        };
                        if message["operation"] == "agent_stop" {
                            return Ok::<_, anyhow::Error>(());
                        }
                        let payloads: Vec<Value> = if message["operation"] == "agent_prompt"
                            || (message["operation"] == "agent_input"
                                && message["arguments"]["prompt"].is_string()
                                && message["arguments"].get("message").is_none())
                        {
                            let prompt_text = text(&message["arguments"], "prompt")?;
                            proto_write.lock().unwrap().prompt(prompt_text)?
                        } else {
                            ensure!(message["operation"] == "agent_input", "Invalid agent input");
                            let value = &message["arguments"]["message"];
                            ensure!(value.is_object(), "Agent message must be an object");
                            vec![value.clone()]
                        };
                        for value in payloads {
                            write_agent_frame(&mut stdin, &value).await?;
                        }
                        stdin.flush().await?;
                    }
                }
            }
        };

        let drain = async {
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            let mut captured_lines = 0usize;
            let mut captured_bytes = 0usize;
            loop {
                line.clear();
                let size = (&mut reader)
                    .take(4096)
                    .read_until(b'\n', &mut line)
                    .await?;
                if size == 0 {
                    return Ok::<_, anyhow::Error>(());
                }
                if captured_lines < 64 && captured_bytes < 16 * 1024 {
                    let text_line = String::from_utf8_lossy(&line).trim_end().to_owned();
                    if !text_line.is_empty() {
                        captured_lines += 1;
                        captured_bytes += text_line.len();
                        let _ =
                            output(tx, id, json!({"type":"agent_stderr","text":text_line})).await;
                    }
                } else {
                    let mut rest = reader.into_inner();
                    tokio::io::copy(&mut rest, &mut tokio::io::sink()).await?;
                    return Ok(());
                }
            }
        };

        let handshake_deadline = tokio::time::sleep(AGENT_HANDSHAKE_TIMEOUT);
        tokio::pin!(read, write, drain, handshake_deadline);
        let mut stderr_closed = false;
        let mut handshake_checked = initial_ready;
        loop {
            tokio::select! {
                result = &mut read => {
                    result?;
                    break;
                }
                result = &mut write => {
                    result?;
                    break;
                }
                result = &mut drain, if !stderr_closed => {
                    result?;
                    stderr_closed = true;
                }
                _ = &mut handshake_deadline, if !handshake_checked => {
                    handshake_checked = true;
                    if !handshake_ready.load(Ordering::Acquire) {
                        let _ = child.start_kill();
                        anyhow::bail!("Agent handshake timed out after 30s");
                    }
                }
            }
        }
        if !stderr_closed {
            let _ = tokio::time::timeout(Duration::from_millis(250), &mut drain).await;
        }
        let _ = child.start_kill();
        let status = child.wait().await?;
        output(tx, id, json!({"type":"exit","code":status.code()})).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn project_tool_reads_only_the_selected_folder_in_both_modes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/README"), "selected folder").unwrap();
        std::fs::write(root.join("README"), "outer folder").unwrap();
        std::fs::write(root.join("outer-only.txt"), "outside project").unwrap();
        let roots = HashMap::from([("work".to_owned(), root.clone())]);
        let runtime = Runtime::new().unwrap();
        let (reply, mut frames) = mpsc::channel(2);
        for mode in [Mode::Sandboxed, Mode::Yolo] {
            let mut request = json!({"consent_id":Uuid::new_v4(),"root":"work","mode":"sandboxed","mcp_grants":{},
                "operation":"project_tool","arguments":{"project_path":"src","operation":"list_files","arguments":{}}});
            runtime
                .execute(Uuid::new_v4(), request.clone(), &roots, mode, &root, &reply)
                .await
                .unwrap();
            let frame = frames.recv().await.unwrap().into_text().unwrap();
            assert!(frame.contains("README"));
            assert!(!frame.contains("outer-only.txt"));

            request["arguments"]["operation"] = json!("read_file");
            request["arguments"]["arguments"]["path"] = json!("README");
            runtime
                .execute(Uuid::new_v4(), request.clone(), &roots, mode, &root, &reply)
                .await
                .unwrap();
            let frame = frames.recv().await.unwrap().into_text().unwrap();
            assert!(frame.contains("selected folder"));
            request["arguments"]["arguments"]["path"] = json!("../README");
            assert!(
                runtime
                    .execute(Uuid::new_v4(), request, &roots, mode, &root, &reply)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn project_diff_tracks_real_writes_when_authorized() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let roots = HashMap::from([("work".to_owned(), root.clone())]);
        let runtime = Runtime::new().unwrap();
        let (reply, mut frames) = mpsc::channel(2);
        for (content, include, added, removed) in [
            ("one\n", true, 1, 0),
            ("two\n", true, 1, 1),
            ("private\n", false, 0, 0),
        ] {
            let request = json!({"consent_id":Uuid::new_v4(),"root":"work","mode":"sandboxed","operation":"project_tool","mcp_grants":{},
              "arguments":{"project_path":".","include_diff":include,"operation":"write_file","arguments":{"path":"README","content":content}}});
            runtime
                .execute(
                    Uuid::new_v4(),
                    request,
                    &roots,
                    Mode::Sandboxed,
                    &root,
                    &reply,
                )
                .await
                .unwrap();
            let output: Value =
                serde_json::from_str(&frames.recv().await.unwrap().into_text().unwrap()).unwrap();
            let diff = &output["result"]["diff"];
            if include {
                assert_eq!(diff["additions"], added);
                assert_eq!(diff["deletions"], removed);
                assert!(
                    diff["patch"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("+{content}"))
                );
            } else {
                assert!(diff.is_null());
            }
        }
    }

    #[tokio::test]
    async fn preview_rejects_privileged_ports() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let roots = HashMap::from([("work".to_owned(), root.clone())]);
        let runtime = Runtime::new().unwrap();
        let (reply, _frames) = mpsc::channel(2);
        for port in [0u64, 22, 80, 443, 1023, 65536] {
            let request = json!({
                "consent_id": Uuid::new_v4(),
                "root": "work",
                "mode": "sandboxed",
                "operation": "preview",
                "arguments": {"port": port, "path": "/"},
                "mcp_grants": {}
            });
            assert!(
                runtime
                    .execute(
                        Uuid::new_v4(),
                        request,
                        &roots,
                        Mode::Sandboxed,
                        &root,
                        &reply
                    )
                    .await
                    .is_err(),
                "expected port {port} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn session_input_requires_same_consent_and_root() {
        let runtime = Runtime::new().unwrap();
        let id = Uuid::new_v4();
        let mut request = Request {
            consent_id: Uuid::new_v4(),
            root: "project".into(),
            mode: Mode::Sandboxed,
            operation: "agent_input".into(),
            arguments: json!({"session_id":id,"message":{"id":1}}),
            mcp_grants: Default::default(),
        };
        let (_guard, mut input) = runtime.session(id, &request);
        runtime.send_input(&request).await.unwrap();
        assert_eq!(input.recv().await.unwrap()["operation"], "agent_input");
        request.root = "other".into();
        assert!(runtime.send_input(&request).await.is_err());
        request.root = "project".into();
        request.consent_id = Uuid::new_v4();
        assert!(runtime.send_input(&request).await.is_err());
    }
}
