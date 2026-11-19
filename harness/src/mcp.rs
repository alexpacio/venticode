use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams},
    transport::{StreamableHttpClientTransport, TokioChildProcess},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Mcp {
    List {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    Add {
        name: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Remove {
        name: String,
    },
    Install {
        name: String,
        #[arg(long, conflicts_with = "python", required_unless_present = "python")]
        npm: Option<String>,
        #[arg(long)]
        python: Option<String>,
        #[arg(long)]
        executable: String,
    },
    Login {
        server: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    Logout {
        server: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// Locally connect to an MCP server, verify handshake, and list its tools.
    Test {
        server: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub source: String,
    pub revision: String,
    pub config: Value,
}
fn registry() -> Result<PathBuf> {
    Ok(crate::config::directory()?.join("mcp.json"))
}
fn paths(root: &Path) -> Vec<(String, PathBuf)> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut paths = vec![
        ("ventiharness".into(), registry().unwrap_or_default()),
        ("claude-code".into(), home.join(".claude.json")),
        ("cursor".into(), home.join(".cursor/mcp.json")),
        (
            "codex".into(),
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"))
                .join("config.toml"),
        ),
        ("project".into(), root.join(".mcp.json")),
        ("cursor-project".into(), root.join(".cursor/mcp.json")),
        ("vscode-project".into(), root.join(".vscode/mcp.json")),
        ("codex-project".into(), root.join(".codex/config.toml")),
    ];
    #[cfg(target_os = "macos")]
    let config = home.join("Library/Application Support");
    #[cfg(windows)]
    let config = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default();
    #[cfg(all(unix, not(target_os = "macos")))]
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    paths.push((
        "claude-desktop".into(),
        config.join("Claude/claude_desktop_config.json"),
    ));
    paths.push(("vscode".into(), config.join("Code/User/mcp.json")));
    paths
}
fn decode(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)?;
    ensure!(raw.len() <= 1024 * 1024, "MCP config too large");
    if path.extension().is_some_and(|v| v == "toml") {
        Ok(serde_json::to_value(toml::from_str::<toml::Value>(&raw)?)?)
    } else {
        Ok(json5::from_str(&raw)?)
    }
}
pub fn discover(root: &Path) -> Result<(Vec<Server>, Vec<Value>)> {
    let mut servers = Vec::new();
    let mut errors = Vec::new();
    for (source, path) in paths(root) {
        if !path.is_file() {
            continue;
        }
        let value = match decode(&path) {
            Ok(value) => value,
            Err(_) => {
                errors.push(json!({"source":source,"error":"Invalid MCP configuration"}));
                continue;
            }
        };
        let mut tables = vec![
            value
                .get("mcpServers")
                .or_else(|| value.get("mcp_servers"))
                .or_else(|| value.get("servers")),
        ];
        if source == "claude-code" {
            tables.push(
                value
                    .get("projects")
                    .and_then(|v| v.get(root.to_string_lossy().as_ref()))
                    .and_then(|v| v.get("mcpServers")),
            );
        }
        for table in tables.into_iter().flatten().filter_map(Value::as_object) {
            for (name, config) in table {
                let id = hex::encode(Sha256::digest(format!("{}\0{name}", path.display())));
                let revision = hex::encode(Sha256::digest(serde_json::to_vec(config)?));
                servers.push(Server {
                    id,
                    name: name.clone(),
                    source: source.clone(),
                    revision,
                    config: config.clone(),
                });
            }
        }
    }
    servers.sort_by(|a, b| a.id.cmp(&b.id));
    servers.dedup_by(|a, b| a.id == b.id);
    Ok((servers, errors))
}
fn source_disabled(config: &Value) -> bool {
    config["disabled"] == true || config["enabled"] == false
}
pub fn list(root: &Path) -> Result<Value> {
    let (servers, errors) = discover(root)?;
    Ok(
        json!({"servers":servers.into_iter().map(|s|json!({"id":s.id,"name":s.name,"source":s.source,"revision":s.revision,"transport":if s.config["url"].is_string(){"http"}else{"stdio"},"source_disabled":source_disabled(&s.config)})).collect::<Vec<_>>(),"errors":errors}),
    )
}
fn resolve_from(servers: Vec<Server>, id: &str, allow_disabled: bool) -> Result<Server> {
    let server = if let Some(exact) = servers.iter().find(|s| s.id == id).cloned() {
        exact
    } else {
        let mut by_name = servers.into_iter().filter(|s| s.name == id);
        let first = by_name.next().context("MCP server not found")?;
        ensure!(
            by_name.next().is_none(),
            "Multiple MCP servers match name '{id}'; specify the server id"
        );
        first
    };
    ensure!(
        allow_disabled || !source_disabled(&server.config),
        "MCP server is disabled in local configuration"
    );
    Ok(server)
}
fn selected(root: &Path, id: &str, allow_disabled: bool) -> Result<Server> {
    resolve_from(discover(root)?.0, id, allow_disabled)
}
fn expanded(value: &str, root: &Path) -> Result<String> {
    let mut value = value.replace(
        "${workspaceFolder}",
        root.to_str().context("Invalid project path")?,
    );
    while let Some(start) = value.find("${env:") {
        let end = value[start..]
            .find('}')
            .context("Invalid environment reference")?
            + start;
        let env =
            std::env::var(&value[start + 6..end]).context("MCP environment variable missing")?;
        value.replace_range(start..=end, &env);
    }
    ensure!(
        !value.contains("${"),
        "MCP config requires local input setup"
    );
    Ok(value)
}
async fn invoke(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    operation: &str,
    args: &Value,
) -> Result<Value> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = service
            .list_tools(
                cursor
                    .clone()
                    .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor))),
            )
            .await?;
        tools.extend(page.tools);
        ensure!(tools.len() <= 4096, "Too many MCP tools");
        cursor = page.next_cursor;
        if let Some(cursor) = &cursor {
            ensure!(seen.insert(cursor.clone()), "MCP pagination cycle");
        } else {
            break;
        }
    }
    if operation == "mcp_list_tools" {
        return Ok(json!({"tools":tools}));
    }
    let name = args["tool"].as_str().context("Tool required")?;
    let tool = tools
        .into_iter()
        .find(|t| t.name == name)
        .context("Unknown MCP tool")?;
    let parameters = args["parameters"]
        .as_object()
        .context("Tool parameters must be an object")?;
    let schema = serde_json::to_value(tool.input_schema)?;
    ensure!(
        jsonschema::is_valid(&schema, &Value::Object(parameters.clone())),
        "Invalid MCP tool arguments"
    );
    let result = service
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(parameters.clone()))
        .await?;
    Ok(serde_json::to_value(result)?)
}
type Service = rmcp::service::RunningService<rmcp::RoleClient, ()>;
type Slot = std::sync::Arc<tokio::sync::Mutex<Option<Service>>>;
#[derive(Default)]
pub struct Clients(
    std::sync::Mutex<std::collections::HashMap<(uuid::Uuid, PathBuf, String), Slot>>,
);
impl Clients {
    pub fn release(&self, request: uuid::Uuid) {
        self.0
            .lock()
            .unwrap()
            .retain(|(id, _, _), _| *id != request);
    }
    #[cfg(feature = "ventic-relay")]
    pub fn retain(&self, consents: &[Value]) {
        self.0.lock().unwrap().retain(|(id, _, _), _| {
            consents
                .iter()
                .filter_map(Value::as_str)
                .any(|value| uuid::Uuid::parse_str(value).ok().as_ref() == Some(id))
        });
    }
    pub async fn call(
        &self,
        consent: uuid::Uuid,
        root: &Path,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        let server = selected(
            root,
            args["server_id"].as_str().context("Server required")?,
            true,
        )?;
        ensure!(
            args["revision"] == server.revision,
            "MCP configuration changed; approve again"
        );

        let key = (
            consent,
            root.to_owned(),
            format!("{}:{}", server.id, server.revision),
        );
        let slot = self.0.lock().unwrap().entry(key).or_default().clone();
        let mut slot = slot.lock().await;
        if slot.as_ref().is_none_or(|service| service.is_closed()) {
            *slot = Some(connect(root, &server).await?);
        }
        invoke(slot.as_ref().unwrap(), operation, args).await
    }
}
async fn connect(root: &Path, server: &Server) -> Result<Service> {
    if let Some(url) = server.config["url"].as_str() {
        let url = url::Url::parse(url)?;
        ensure!(
            url.scheme() == "https"
                || url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")),
            "HTTPS required for remote MCP"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "Credentials must use local headers"
        );
        let mut headers = reqwest_mcp::header::HeaderMap::new();
        if let Some(values) = server.config["headers"]
            .as_object()
            .or_else(|| server.config["http_headers"].as_object())
        {
            for (name, value) in values {
                headers.insert(
                    name.parse::<reqwest_mcp::header::HeaderName>()?,
                    expanded(value.as_str().context("Invalid header")?, root)?.parse()?,
                );
            }
        }
        if let Some(name) = server.config["bearer_token_env_var"].as_str() {
            headers.insert(
                reqwest_mcp::header::AUTHORIZATION,
                format!("Bearer {}", std::env::var(name)?).parse()?,
            );
        }
        let client = reqwest_mcp::Client::builder()
            .default_headers(headers)
            .redirect(reqwest_mcp::redirect::Policy::none())
            .build()?;
        let manager = crate::oauth::manager(server).await?;
        let client = rmcp::transport::auth::AuthClient::new(client, manager);
        let transport = StreamableHttpClientTransport::with_client(
            client,
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                url.to_string(),
            ),
        );
        Ok(().serve(transport).await?)
    } else {
        let name = server.config["command"]
            .as_str()
            .context("MCP command required")?;
        let mut cmd = crate::config::command(name)?;
        cmd.current_dir(root).kill_on_drop(true);
        if let Some(args) = server.config["args"].as_array() {
            for arg in args {
                cmd.arg(expanded(
                    arg.as_str().context("Invalid MCP argument")?,
                    root,
                )?);
            }
        }
        if let Some(env) = server.config["env"].as_object() {
            for (key, value) in env {
                cmd.env(
                    key,
                    expanded(value.as_str().context("Invalid MCP environment")?, root)?,
                );
            }
        }
        if let Some(env) = server.config["env_vars"].as_array() {
            for name in env {
                let key = name.as_str().context("Invalid environment name")?;
                cmd.env(key, std::env::var(key)?);
            }
        }
        for name in [
            "VENTICLI_TOKEN",
            "VENTIHARNESS_TOKEN",
            "VENTICODE_HARNESS_TOKEN",
        ] {
            cmd.env_remove(name);
        }
        let mut cmd = process_wrap::tokio::CommandWrap::from(cmd);
        cmd.wrap(process_wrap::tokio::KillOnDrop);
        #[cfg(unix)]
        cmd.wrap(process_wrap::tokio::ProcessSession);
        #[cfg(windows)]
        cmd.wrap(process_wrap::tokio::JobObject);
        let spawned = TokioChildProcess::builder(cmd)
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let transport = spawned.0;
        let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(stderr) = spawned.1 {
            let capture = stderr_buf.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
                let mut reader = BufReader::new(stderr);
                let mut line = Vec::new();
                let mut lines = 0usize;
                loop {
                    line.clear();
                    let Ok(size) = (&mut reader).take(2048).read_until(b'\n', &mut line).await
                    else {
                        break;
                    };
                    if size == 0 {
                        break;
                    }
                    if lines < 32 {
                        let text = String::from_utf8_lossy(&line);
                        let mut guard = capture.lock().unwrap();
                        if guard.len() + text.len() <= 8192 {
                            guard.push_str(&text);
                            lines += 1;
                            continue;
                        }
                    }
                    let mut rest = reader.into_inner();
                    let _ = tokio::io::copy(&mut rest, &mut tokio::io::sink()).await;
                    break;
                }
            });
        }
        match ().serve(transport).await {
            Ok(service) => Ok(service),
            Err(err) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let stderr_text = stderr_buf.lock().unwrap().trim().to_owned();
                if stderr_text.is_empty() {
                    Err(err.into())
                } else {
                    Err(anyhow::anyhow!(
                        "MCP handshake failed ({err}); stderr:\n{stderr_text}"
                    ))
                }
            }
        }
    }
}
fn save(name: &str, value: Option<Value>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 80, "Invalid server name");
    let path = registry()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut config = if path.exists() {
        decode(&path)?
    } else {
        json!({"mcpServers":{}})
    };
    let entries = config["mcpServers"]
        .as_object_mut()
        .context("Invalid MCP registry")?;
    if let Some(value) = value {
        entries.insert(name.into(), value);
    } else {
        entries.remove(name);
    }
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temporary.write_all(&serde_json::to_vec_pretty(&config)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}
fn pinned(package: &str, npm: bool) -> bool {
    let pair = if npm {
        package.rsplit_once('@')
    } else {
        package.split_once("==")
    };
    pair.is_some_and(|(name, version)| {
        let name = if npm {
            name.strip_prefix('@').unwrap_or(name)
        } else {
            name
        };
        !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._/".contains(&c))
            && !version.is_empty()
            && version.as_bytes()[0].is_ascii_digit()
            && version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
            && !version.contains('*')
            && (!npm || semver::Version::parse(version).is_ok())
    })
}
pub async fn manage(command: &Mcp) -> Result<()> {
    match command {
        Mcp::List { project } => println!(
            "{}",
            serde_json::to_string_pretty(&list(&project.canonicalize()?)?)?
        ),
        Mcp::Add { name, url, command } => {
            ensure!(
                url.is_some() != !command.is_empty(),
                "Provide either --url or a command"
            );
            save(
                name,
                Some(if let Some(url) = url {
                    json!({"url":url})
                } else {
                    json!({"command":command[0],"args":command[1..]})
                }),
            )?;
        }
        Mcp::Remove { name } => save(name, None)?,
        Mcp::Install {
            name,
            npm,
            python,
            executable,
        } => {
            ensure!(
                !executable.is_empty() && !executable.contains(['/', '\\']) && executable != "..",
                "Invalid executable"
            );
            let parent = crate::config::directory()?.join("servers");
            std::fs::create_dir_all(&parent)?;
            let staging = tempfile::tempdir_in(parent)?;
            let path = staging.path().to_owned();
            let program = if npm.is_some() { "npm" } else { "uv" };
            let mut cmd = crate::config::command(program)?;
            let binary = if let Some(package) = npm {
                ensure!(
                    pinned(package, true),
                    "Pin an npm package with an exact version"
                );
                cmd.args(["install", "--prefix"])
                    .arg(&path)
                    .arg("--save-exact")
                    .arg(package);
                path.join("node_modules/.bin").join(if cfg!(windows) {
                    format!("{executable}.cmd")
                } else {
                    executable.clone()
                })
            } else {
                let package = python.as_deref().context("Python package required")?;
                ensure!(
                    pinned(package, false),
                    "Pin a Python package with an exact version"
                );
                let status = crate::config::command("uv")?
                    .arg("venv")
                    .arg(&path)
                    .status()
                    .await?;
                ensure!(status.success(), "Python environment creation failed");
                let bin = path.join(if cfg!(windows) { "Scripts" } else { "bin" });
                cmd.args(["pip", "install", "--python"])
                    .arg(bin.join(if cfg!(windows) {
                        "python.exe"
                    } else {
                        "python"
                    }))
                    .arg(package);
                bin.join(if cfg!(windows) {
                    format!("{executable}.exe")
                } else {
                    executable.clone()
                })
            };
            ensure!(cmd.status().await?.success(), "MCP installation failed");
            ensure!(binary.is_file(), "Requested MCP executable not installed");
            save(name, Some(json!({"command":binary,"args":[]})))?;
            // Keep the venv at its original path: Python launchers embed it.
            let _ = staging.keep();
        }
        Mcp::Login { server, project } => {
            crate::mcp::oauth_login(&selected(&project.canonicalize()?, server, true)?).await?
        }
        Mcp::Logout { server, project } => {
            let resolved = selected(&project.canonicalize()?, server, true)?;
            keyring::Entry::new(crate::config::CREDENTIAL_SERVICE, &resolved.id)?
                .delete_credential()?;
        }
        Mcp::Test { server, project } => {
            let root = project.canonicalize()?;
            let resolved = selected(&root, server, true)?;
            let started = std::time::Instant::now();
            let service = connect(&root, &resolved).await?;
            let tools_val = invoke(&service, "mcp_list_tools", &json!({})).await?;
            let elapsed_ms = started.elapsed().as_millis();
            let _ = service.cancel().await;
            let tools = tools_val["tools"].as_array().cloned().unwrap_or_default();
            let tool_names: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t["name"],
                        "description": t["description"]
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "id": resolved.id,
                    "name": resolved.name,
                    "source": resolved.source,
                    "revision": resolved.revision,
                    "latency_ms": elapsed_ms,
                    "tool_count": tool_names.len(),
                    "tools": tool_names
                }))?
            );
        }
    }
    Ok(())
}
async fn oauth_login(server: &Server) -> Result<()> {
    crate::oauth::login(server).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_stdio_mcp_initializes_validates_and_keeps_session_state() {
        if which::which("node").is_err() {
            eprintln!("MCP runner test requires node on PATH");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let script = r#"const rl=require('node:readline').createInterface({input:process.stdin}); let count=0;
rl.on('line',line=>{const m=JSON.parse(line);if(m.id===undefined)return;let result;
if(m.method==='initialize')result={protocolVersion:m.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'synthetic',version:'1'}};
else if(m.method==='tools/list')result={tools:[{name:'counter',description:'Synthetic counter',inputSchema:{type:'object',additionalProperties:false}}]};
else if(m.method==='tools/call')result={content:[{type:'text',text:String(++count)}]};
else result={};process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:m.id,result})+'\n');});"#;
        let server = Server {
            id: "synthetic".into(),
            name: "synthetic".into(),
            source: "test".into(),
            revision: "test".into(),
            config: json!({"command":"node","args":["-e",script]}),
        };
        let service = connect(root.path(), &server).await.unwrap();
        assert!(
            invoke(
                &service,
                "mcp_call_tool",
                &json!({"tool":"counter","parameters":{"unexpected":true}})
            )
            .await
            .is_err()
        );
        for expected in ["1", "2"] {
            let result = invoke(
                &service,
                "mcp_call_tool",
                &json!({"tool":"counter","parameters":{}}),
            )
            .await
            .unwrap();
            assert_eq!(result["content"][0]["text"], expected);
        }
        service.cancel().await.unwrap();
    }
    #[test]
    fn installs_require_exact_package_versions() {
        for value in [
            "server@latest",
            "server@1.x",
            "server@^1.0.0",
            "--script=bad@1.0.0",
            "https://host/package@1.0.0",
        ] {
            assert!(!pinned(value, true));
        }
        assert!(pinned("@scope/server@1.2.3", true));
        assert!(pinned("server==1.2.3", false));
        for value in ["server>=1", "server==1.*", "server==1;extra", "-r==1"] {
            assert!(!pinned(value, false));
        }
    }
    #[test]
    fn parse_jsonc_and_codex_toml_without_starting_processes() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("mcp.json");
        std::fs::write(
            &json,
            "{ // comment\n mcpServers: {test: {command: 'never-run',},},}",
        )
        .unwrap();
        assert_eq!(
            decode(&json).unwrap()["mcpServers"]["test"]["command"],
            "never-run"
        );
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "[mcp_servers.test]\ncommand = 'never-run'\n").unwrap();
        assert_eq!(
            decode(&toml).unwrap()["mcp_servers"]["test"]["command"],
            "never-run"
        );
    }
    #[test]
    fn resolve_rejects_disabled_servers_and_supports_unambiguous_names() {
        let servers = vec![
            Server {
                id: "id-enabled".into(),
                name: "alpha".into(),
                source: "project".into(),
                revision: "r1".into(),
                config: json!({"command":"echo"}),
            },
            Server {
                id: "id-disabled".into(),
                name: "beta".into(),
                source: "project".into(),
                revision: "r2".into(),
                config: json!({"command":"echo","disabled":true}),
            },
            Server {
                id: "id-dup-1".into(),
                name: "dup".into(),
                source: "project".into(),
                revision: "r3".into(),
                config: json!({"command":"echo"}),
            },
            Server {
                id: "id-dup-2".into(),
                name: "dup".into(),
                source: "cursor".into(),
                revision: "r4".into(),
                config: json!({"command":"echo"}),
            },
        ];
        assert_eq!(
            resolve_from(servers.clone(), "alpha", false).unwrap().id,
            "id-enabled"
        );
        assert!(resolve_from(servers.clone(), "id-disabled", false).is_err());
        assert!(resolve_from(servers.clone(), "beta", false).is_err());
        assert_eq!(
            resolve_from(servers.clone(), "beta", true).unwrap().id,
            "id-disabled"
        );
        assert!(resolve_from(servers.clone(), "dup", false).is_err());
        assert_eq!(
            resolve_from(servers, "id-dup-2", false).unwrap().source,
            "cursor"
        );
    }
}
