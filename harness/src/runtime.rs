use crate::protocol::{Agent, Mode, Request};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
#[derive(Clone)]
pub struct Runtime {
    clients: Arc<crate::mcp::Clients>,
    sessions: Sessions,
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
        .timeout(std::time::Duration::from_millis(200))
        .diff_lines(before, after);
    let (mut additions, mut deletions) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => additions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    json!({"path":path,"additions":additions,"deletions":deletions,"patch":diff.unified_diff().header(&format!("a/{path}"),&format!("b/{path}")).to_string()})
}
impl Runtime {
    pub fn new() -> Result<Self> {
        Ok(Self {
            sessions: Arc::default(),
            clients: Arc::default(),
        })
    }
    pub fn release(&self, id: Uuid) {
        self.clients.release(id);
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
        let files = if yolo {
            let base = roots
                .get(&request.root)
                .cloned()
                .unwrap_or_else(|| default_cwd.to_path_buf());
            Files::open_yolo(&base)?
        } else {
            let root = roots
                .get(&request.root)
                .context("Root not granted locally")?;
            ensure!(
                root.canonicalize()? == *root,
                "Granted root changed; pair again"
            );
            Files::open(root)?
        };
        // Old harnesses reject this operation. Never run a subfolder request relative to the entire grant.
        let include_diff =
            request.operation == "project_tool" && request.arguments["include_diff"] == true;
        let files = if request.operation == "project_tool" {
            let scoped = Files::open(roots.get(&request.root).context("Missing project root")?)?;
            let directory = scoped.directory(text(&request.arguments, "project_path")?)?;
            request.operation = text(&request.arguments, "operation")?.to_owned();
            request.arguments = request.arguments["arguments"].take();
            Files::open(&directory)?
        } else {
            files
        };
        let args = &request.arguments;
        let path = args["path"].as_str().unwrap_or(".");
        let before = (include_diff
            && matches!(request.operation.as_str(), "write_file" | "edit_file"))
        .then(|| project_text(&files, path))
        .flatten();
        let mut result = match request.operation.as_str() {
            "list_files" => files.list(path)?,
            "find_files" => {
                let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;
                files.find_files(path, args["pattern"].as_str(), max_results)?
            }
            "search_files" => {
                let query = text(args, "query")?;
                let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(false);
                let max_results = args["max_results"].as_u64().unwrap_or(100) as usize;
                files.search_files(
                    path,
                    query,
                    args["pattern"].as_str(),
                    case_sensitive,
                    max_results,
                )?
            }
            "read_file" => {
                let start_line = args["start_line"].as_u64().map(|v| v as usize);
                let end_line = args["end_line"].as_u64().map(|v| v as usize);
                files.read_range(path, start_line, end_line)?
            }
            "write_file" => {
                let encoding = args["encoding"].as_str().unwrap_or("utf8");
                let written_bytes = match encoding {
                    "base64" => {
                        let raw = STANDARD
                            .decode(text(args, "content")?.trim())
                            .context("Invalid base64 content for write_file")?;
                        files.write(text(args, "path")?, &raw)?;
                        raw.len()
                    }
                    "utf8" => {
                        let raw = text(args, "content")?.as_bytes();
                        files.write(text(args, "path")?, raw)?;
                        raw.len()
                    }
                    _ => anyhow::bail!("Unsupported write_file encoding '{encoding}'"),
                };
                json!({"written": true, "bytes": written_bytes})
            }
            "edit_file" => {
                let edits: Vec<Edit> = serde_json::from_value(
                    args.get("edits").cloned().context("Missing edits array")?,
                )?;
                files.edit(
                    text(args, "path")?,
                    &edits,
                    args["expected_sha256"].as_str(),
                )?
            }
            "mkdir" => {
                files.mkdir(text(args, "path")?)?;
                json!({"created":true})
            }
            "rename" => {
                files.rename(text(args, "path")?, text(args, "destination")?)?;
                json!({"renamed":true})
            }
            "remove" => {
                files.remove(text(args, "path")?)?;
                json!({"removed":true})
            }
            "execute" => {
                let cwd = files.directory(path)?;
                let command = text(args, "command")?;
                let (events, mut rx) = mpsc::channel(8);
                let task = process::execute(&cwd, command, events);
                tokio::pin!(task);
                loop {
                    tokio::select! {
                        result = &mut task => { result?; while let Some(event)=rx.recv().await { output(tx,id,serde_json::to_value(event)?).await?; } break; },
                        Some(event) = rx.recv() => output(tx,id,serde_json::to_value(event)?).await?,
                    }
                }
                return Ok(());
            }
            "terminal_start" => {
                return self
                    .terminal(id, &request, files.directory(path)?, tx)
                    .await;
            }
            "agent_list" => {
                json!({"agents":Agent::ALL.into_iter().map(|agent| json!({"id":agent.name(),"protocol":agent.protocol(),"available":which::which(agent.binary()).is_ok()})).collect::<Vec<_>>()})
            }
            "agent_start" => return self.agent(id, &request, files.directory(path)?, tx).await,
            "terminal_input" | "terminal_resize" | "terminal_stop" | "agent_input"
            | "agent_prompt" | "agent_stop" => {
                self.send_input(&request).await?;
                json!({"accepted":true})
            }
            "mcp_list_servers" => crate::mcp::list(files.base_path())?,
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
            "preview" => {
                let port = args["port"]
                    .as_u64()
                    .filter(|p| *p > 0 && *p <= 65535)
                    .context("Invalid preview port")?;
                let path = text(args, "path")?;
                ensure!(
                    path.starts_with('/')
                        && !path.starts_with("//")
                        && !path.contains(['\r', '\n', '\\']),
                    "Invalid preview path"
                );
                let url = format!("http://127.0.0.1:{port}{path}");
                let http = reqwest_mcp::Client::builder()
                    .no_proxy()
                    .redirect(reqwest_mcp::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(20))
                    .build()?;
                let mut response = http.get(url).send().await?;
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
        if let Some(before) = before
            && let Some(after) = project_text(&files, path)
        {
            result["diff"] = project_diff(path, &before, &after);
        }
        output(tx, id, result).await
    }
    async fn terminal(&self, id: Uuid, request: &Request, cwd: PathBuf, tx: &Reply) -> Result<()> {
        let rows = request.arguments["rows"]
            .as_u64()
            .unwrap_or(24)
            .try_into()?;
        let cols = request.arguments["cols"]
            .as_u64()
            .unwrap_or(80)
            .try_into()?;
        let (mut terminal, mut reader) = Terminal::start(&cwd, rows, cols)?;
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
                bytes = bytes.recv() => match bytes { Some(data)=>output(tx,id,json!({"type":"output","stream":"pty","data":data})).await?,None=>break },
                message = input.recv() => {
                    let message=message.context("Session closed")?;
                    match message["operation"].as_str() {
                        Some("terminal_input") => terminal.input(&STANDARD.decode(text(&message["arguments"],"data")?)?)?,
                        Some("terminal_resize") => terminal.resize(message["arguments"]["rows"].as_u64().context("Rows required")?.try_into()?,message["arguments"]["cols"].as_u64().context("Columns required")?.try_into()?)?,
                        Some("terminal_stop") => break,
                        _=>anyhow::bail!("Invalid terminal input"),
                    }
                }
            }
        }
        output(tx, id, json!({"type":"exit","code":terminal.status()?})).await
    }
    async fn agent(&self, id: Uuid, request: &Request, cwd: PathBuf, tx: &Reply) -> Result<()> {
        let agent: Agent = serde_json::from_value(request.arguments["agent"].clone())?;
        let protocol = Arc::new(Mutex::new(crate::agents::Session::new(
            agent,
            &cwd,
            request.arguments["prompt"].as_str(),
        )?));
        let binary = which::which(agent.binary())
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
        output(tx,id,json!({"type":"session","session_id":id,"agent":agent.name(),"protocol":agent.protocol()})).await?;
        let initial = protocol.lock().unwrap().start();
        for message in initial {
            stdin.write_all(&serde_json::to_vec(&message)?).await?;
            stdin.write_all(b"\n").await?;
        }
        stdin.flush().await?;
        let proto_read = protocol.clone();
        let read = async {
            let mut stdout = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                let size = (&mut stdout)
                    .take(512 * 1024 + 1)
                    .read_until(b'\n', &mut line)
                    .await?;
                if size == 0 {
                    return Ok::<_, anyhow::Error>(());
                }
                ensure!(
                    size <= 512 * 1024 && line.ends_with(b"\n"),
                    "Agent frame too large or incomplete"
                );
                let event: Value = serde_json::from_slice(&line)?;
                let outgoing = proto_read.lock().unwrap().receive(&event)?;
                for message in outgoing {
                    let mut input = request.clone();
                    input.operation = "agent_input".into();
                    input.arguments = json!({"session_id":id,"message":message});
                    self.send_input(&input).await?;
                }
                output(tx, id, json!({"type":"agent_event","event":event})).await?;
            }
        };
        let proto_write = protocol.clone();
        let write = async {
            while let Some(message) = input.recv().await {
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
                    let mut bytes = serde_json::to_vec(&value)?;
                    ensure!(bytes.len() <= 32768, "Agent input too large");
                    bytes.push(b'\n');
                    stdin.write_all(&bytes).await?;
                }
                stdin.flush().await?;
            }
            Ok(())
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
        tokio::pin!(read, write, drain);
        let mut stderr_closed = false;
        loop {
            tokio::select! {
                result = &mut read => { result?; break; },
                result = &mut write => { result?; break; },
                result = &mut drain, if !stderr_closed => { result?; stderr_closed = true; },
            }
        }
        child.start_kill()?;
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
