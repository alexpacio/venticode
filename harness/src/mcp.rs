use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams},
    transport::{StreamableHttpClientTransport, TokioChildProcess},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

const BLOCKED_ENV_VARS: &[&str] = &[
    "VENTICODE_HARNESS_TOKEN",
    "VENTICLI_TOKEN",
    "VENTIHARNESS_TOKEN",
    "OPEN_TERMINAL_API_KEY",
];

fn is_blocked_env(name: &str) -> bool {
    BLOCKED_ENV_VARS
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(name))
}

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

enum ConfigLocation {
    Host(PathBuf),
    Project(PathBuf),
}

fn registry() -> Result<PathBuf> {
    Ok(crate::config::directory()?.join("mcp.json"))
}

fn config_locations(root: &Path) -> Vec<(String, PathBuf, ConfigLocation)> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let mut locations = Vec::new();
    if let Ok(reg) = registry() {
        locations.push((
            "ventiharness".into(),
            reg.clone(),
            ConfigLocation::Host(reg),
        ));
    }
    if let Some(home_dir) = &home {
        let p = home_dir.join(".claude.json");
        locations.push(("claude-code".into(), p.clone(), ConfigLocation::Host(p)));
        let p = home_dir.join(".cursor/mcp.json");
        locations.push(("cursor".into(), p.clone(), ConfigLocation::Host(p)));
    }
    if let Some(codex_home) = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".codex")))
    {
        let p = codex_home.join("config.toml");
        locations.push(("codex".into(), p.clone(), ConfigLocation::Host(p)));
    }
    for (source, rel) in [
        ("project", ".mcp.json"),
        ("cursor-project", ".cursor/mcp.json"),
        ("vscode-project", ".vscode/mcp.json"),
        ("codex-project", ".codex/config.toml"),
    ] {
        let rel_path = PathBuf::from(rel);
        locations.push((
            source.into(),
            root.join(&rel_path),
            ConfigLocation::Project(rel_path),
        ));
    }

    #[cfg(target_os = "macos")]
    let config_dir = home.as_ref().map(|h| h.join("Library/Application Support"));
    #[cfg(windows)]
    let config_dir = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    #[cfg(all(unix, not(target_os = "macos")))]
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));

    if let Some(config) = config_dir {
        let p = config.join("Claude/claude_desktop_config.json");
        locations.push(("claude-desktop".into(), p.clone(), ConfigLocation::Host(p)));
        let p = config.join("Code/User/mcp.json");
        locations.push(("vscode".into(), p.clone(), ConfigLocation::Host(p)));
    }
    locations
}

fn decode_bytes(path: &Path, raw_bytes: Vec<u8>) -> Result<Value> {
    ensure!(raw_bytes.len() <= MAX_CONFIG_BYTES, "MCP config too large");
    let raw = String::from_utf8(raw_bytes).context("MCP config must be valid UTF-8")?;
    if path.extension().is_some_and(|v| v == "toml") {
        Ok(serde_json::to_value(toml::from_str::<toml::Value>(&raw)?)?)
    } else {
        Ok(json5::from_str(&raw)?)
    }
}

fn decode(path: &Path) -> Result<Value> {
    let bytes = ventiharness_terminal::files::read_host_bounded_file(path, MAX_CONFIG_BYTES)?
        .context("MCP config not found")?;
    decode_bytes(path, bytes)
}

pub fn discover(root: &Path) -> Result<(Vec<Server>, Vec<Value>)> {
    let mut servers = Vec::new();
    let mut errors = Vec::new();
    for (source, display_path, location) in config_locations(root) {
        let raw_opt = match &location {
            ConfigLocation::Project(rel) => {
                ventiharness_terminal::files::read_sandboxed_nofollow(root, rel, MAX_CONFIG_BYTES)
            }
            ConfigLocation::Host(path) => {
                ventiharness_terminal::files::read_host_bounded_file(path, MAX_CONFIG_BYTES)
            }
        };
        let raw_bytes = match raw_opt {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(_) => {
                errors.push(json!({"source": source, "error": "Invalid MCP configuration"}));
                continue;
            }
        };
        let value = match decode_bytes(&display_path, raw_bytes) {
            Ok(value) => value,
            Err(_) => {
                errors.push(json!({"source": source, "error": "Invalid MCP configuration"}));
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
                let id = hex::encode(Sha256::digest(format!(
                    "{}\0{name}",
                    display_path.display()
                )));
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
    let root_str = root.to_str().context("Invalid project path")?;
    let mut out = String::with_capacity(value.len());
    let mut cursor = 0usize;
    while cursor < value.len() {
        let rest = &value[cursor..];
        let Some(rel_start) = rest.find("${") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..rel_start]);
        let after_open = cursor + rel_start + 2;
        let rel_end = value[after_open..]
            .find('}')
            .context("Invalid environment reference")?;
        let end = after_open + rel_end;
        let token = &value[after_open..end];
        if token == "workspaceFolder" {
            out.push_str(root_str);
        } else if let Some(var_name) = token.strip_prefix("env:") {
            ensure!(!var_name.is_empty(), "Invalid environment reference");
            ensure!(
                !is_blocked_env(var_name),
                "Access to harness secret environment variable '{var_name}' is blocked"
            );
            let env_val = std::env::var(var_name).context("MCP environment variable missing")?;
            out.push_str(&env_val);
        } else {
            bail!("MCP config requires local input setup");
        }
        cursor = end + 1;
    }
    Ok(out)
}

async fn list_all_tools(service: &Service) -> Result<Vec<rmcp::model::Tool>> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let page = tokio::time::timeout(
                Duration::from_secs(30),
                service.list_tools(
                    cursor
                        .clone()
                        .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor))),
                ),
            )
            .await
            .context("MCP list_tools timed out")??;
            tools.extend(page.tools);
            ensure!(tools.len() <= 4096, "Too many MCP tools");
            cursor = page.next_cursor;
            if let Some(cursor) = &cursor {
                ensure!(seen.insert(cursor.clone()), "MCP pagination cycle");
            } else {
                break;
            }
        }
        Ok(tools)
    })
    .await
    .context("MCP list_tools overall timeout")?
}

enum InvokeError {
    Validation(anyhow::Error),
    Transport(anyhow::Error),
}

async fn invoke(
    cached: &mut CachedService,
    operation: &str,
    args: &Value,
) -> std::result::Result<Value, InvokeError> {
    if operation == "mcp_list_tools" || cached.tools.is_none() {
        let tools = list_all_tools(&cached.service)
            .await
            .map_err(InvokeError::Transport)?;
        cached.tools = Some(tools);
        cached.validators.clear();
    }
    if operation == "mcp_list_tools" {
        return Ok(json!({"tools": cached.tools.as_ref().unwrap()}));
    }
    let name = args["tool"]
        .as_str()
        .context("Tool required")
        .map_err(InvokeError::Validation)?;
    let tool = cached
        .tools
        .as_ref()
        .unwrap()
        .iter()
        .find(|t| t.name == name)
        .context("Unknown MCP tool")
        .map_err(InvokeError::Validation)?;
    let parameters = args["parameters"]
        .as_object()
        .context("Tool parameters must be an object")
        .map_err(InvokeError::Validation)?;

    if !cached.validators.contains_key(name) {
        let schema = serde_json::to_value(&tool.input_schema)
            .map_err(|e| InvokeError::Validation(e.into()))?;
        let validator = jsonschema::validator_for(&schema).map_err(|e| {
            InvokeError::Validation(anyhow::anyhow!("Invalid MCP tool schema: {e}"))
        })?;
        cached.validators.insert(name.to_owned(), validator);
    }
    let validator = cached.validators.get(name).unwrap();
    let instance = Value::Object(parameters.clone());
    if let Err(err) = validator.validate(&instance) {
        return Err(InvokeError::Validation(anyhow::anyhow!(
            "Invalid MCP tool arguments: {err}"
        )));
    }
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        cached.service.call_tool(
            CallToolRequestParams::new(name.to_owned()).with_arguments(parameters.clone()),
        ),
    )
    .await
    .context("MCP call_tool timed out")
    .map_err(InvokeError::Transport)?
    .map_err(|e| InvokeError::Transport(e.into()))?;
    serde_json::to_value(result).map_err(|e| InvokeError::Transport(e.into()))
}

type Service = rmcp::service::RunningService<rmcp::RoleClient, ()>;

struct CachedService {
    service: Service,
    tools: Option<Vec<rmcp::model::Tool>>,
    validators: HashMap<String, jsonschema::Validator>,
    last_used: Instant,
}

type Slot = std::sync::Arc<tokio::sync::Mutex<Option<CachedService>>>;

#[derive(Default)]
pub struct Clients(std::sync::Mutex<HashMap<(uuid::Uuid, PathBuf, String), Slot>>);

impl Clients {
    pub fn release(&self, request: uuid::Uuid) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _, _), _| *id != request);
    }

    pub fn evict_idle(&self, max_idle: Duration) {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, slot| {
            let Ok(mut guard) = slot.try_lock() else {
                return true;
            };
            let Some(cached) = guard.as_ref() else {
                return false;
            };
            if cached.service.is_closed() || now.duration_since(cached.last_used) >= max_idle {
                if let Some(evicted) = guard.take() {
                    tokio::spawn(async move {
                        let _ =
                            tokio::time::timeout(Duration::from_secs(2), evicted.service.cancel())
                                .await;
                    });
                }
                return false;
            }
            true
        });
    }

    #[cfg(feature = "ventic-relay")]
    pub fn retain(&self, consents: &[Value]) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _, _), _| {
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
            false,
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
        let slot = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if slot.as_ref().is_none_or(|c| c.service.is_closed()) {
            let service = tokio::time::timeout(Duration::from_secs(30), connect(root, &server))
                .await
                .context("MCP server connection timed out")??;
            *slot = Some(CachedService {
                service,
                tools: None,
                validators: HashMap::new(),
                last_used: Instant::now(),
            });
        }
        match invoke(slot.as_mut().unwrap(), operation, args).await {
            Ok(val) => {
                if let Some(cached) = slot.as_mut() {
                    cached.last_used = Instant::now();
                }
                Ok(val)
            }
            Err(InvokeError::Validation(err)) => {
                if let Some(cached) = slot.as_mut() {
                    cached.last_used = Instant::now();
                }
                Err(err)
            }
            Err(InvokeError::Transport(err)) => {
                if let Some(evicted) = slot.take() {
                    let _ = tokio::time::timeout(Duration::from_secs(2), evicted.service.cancel())
                        .await;
                }
                Err(err)
            }
        }
    }
}

async fn connect(root: &Path, server: &Server) -> Result<Service> {
    if let Some(url) = server.config["url"].as_str() {
        let url = url::Url::parse(url)?;
        ensure!(
            url.scheme() == "https"
                || url.scheme() == "http"
                    && matches!(
                        url.host_str(),
                        Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
                    ),
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
            ensure!(
                !is_blocked_env(name),
                "Access to harness secret environment variable '{name}' is blocked"
            );
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
                ensure!(
                    !is_blocked_env(key),
                    "Access to harness secret environment variable '{key}' is blocked"
                );
                cmd.env(key, std::env::var(key)?);
            }
        }
        for name in BLOCKED_ENV_VARS {
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
                tokio::time::sleep(Duration::from_millis(50)).await;
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

fn managed_server_dir(entry: &Value, servers_root: &Path) -> Option<PathBuf> {
    let command = entry["command"].as_str()?;
    let rel = Path::new(command).strip_prefix(servers_root).ok()?;
    match rel.components().next()? {
        Component::Normal(dir_name) => Some(servers_root.join(dir_name)),
        _ => None,
    }
}

fn save(name: &str, value: Option<Value>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 80, "Invalid server name");
    let config_dir = crate::config::directory()?;
    let servers_root = config_dir.join("servers");
    let path = config_dir.join("mcp.json");
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
    let new_dir = value
        .as_ref()
        .and_then(|v| managed_server_dir(v, &servers_root));
    let previous = if let Some(value) = value {
        entries.insert(name.into(), value)
    } else {
        entries.remove(name)
    };
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temporary.write_all(&serde_json::to_vec_pretty(&config)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    if let Some(old_dir) = previous
        .as_ref()
        .and_then(|entry| managed_server_dir(entry, &servers_root))
        && new_dir.as_ref() != Some(&old_dir)
    {
        let _ = std::fs::remove_dir_all(old_dir);
    }
    Ok(())
}

fn valid_pkg_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg.as_bytes()[0].is_ascii_alphanumeric()
        && seg
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._".contains(&c))
}

fn pinned(package: &str, npm: bool) -> bool {
    if npm {
        let Some((raw_name, version)) = package.rsplit_once('@') else {
            return false;
        };
        let valid_name = if let Some(scoped) = raw_name.strip_prefix('@') {
            scoped.split_once('/').is_some_and(|(scope, pkg)| {
                !pkg.contains('/') && valid_pkg_segment(scope) && valid_pkg_segment(pkg)
            })
        } else {
            !raw_name.contains('/') && valid_pkg_segment(raw_name)
        };
        valid_name
            && !version.is_empty()
            && version.as_bytes()[0].is_ascii_digit()
            && version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
            && !version.contains('*')
            && semver::Version::parse(version).is_ok()
    } else {
        let Some((name, version)) = package.split_once("==") else {
            return false;
        };
        !name.contains('/')
            && valid_pkg_segment(name)
            && !version.is_empty()
            && version.as_bytes()[0].is_ascii_digit()
            && version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
            && !version.contains('*')
    }
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
            let id = resolved.id.clone();
            let res = tokio::task::spawn_blocking(move || {
                keyring::Entry::new(crate::config::CREDENTIAL_SERVICE, &id)?.delete_credential()
            })
            .await?;
            match res {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Mcp::Test { server, project } => {
            let root = project.canonicalize()?;
            let resolved = selected(&root, server, true)?;
            let started = Instant::now();
            let tools_val = tokio::time::timeout(Duration::from_secs(30), async {
                let service = connect(&root, &resolved).await?;
                let tools_result = list_all_tools(&service).await;
                let _ = tokio::time::timeout(Duration::from_secs(2), service.cancel()).await;
                Ok::<_, anyhow::Error>(json!({"tools": tools_result?}))
            })
            .await
            .context("MCP test timed out after 30s")??;
            let elapsed_ms = started.elapsed().as_millis();
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
        let mut service = CachedService {
            service: connect(root.path(), &server).await.unwrap(),
            tools: None,
            validators: HashMap::new(),
            last_used: Instant::now(),
        };
        let invalid = invoke(
            &mut service,
            "mcp_call_tool",
            &json!({"tool":"counter","parameters":{"unexpected":true}}),
        )
        .await;
        match invalid {
            Err(InvokeError::Validation(err)) => {
                assert!(err.to_string().contains("Invalid MCP tool arguments:"));
            }
            _ => panic!("expected validation error"),
        }
        assert!(service.validators.contains_key("counter"));
        for expected in ["1", "2"] {
            let result = invoke(
                &mut service,
                "mcp_call_tool",
                &json!({"tool":"counter","parameters":{}}),
            )
            .await
            .ok()
            .unwrap();
            assert_eq!(result["content"][0]["text"], expected);
        }
        service.service.cancel().await.unwrap();
    }

    #[test]
    fn installs_require_exact_package_versions() {
        for value in [
            "server@latest",
            "server@1.x",
            "server@^1.0.0",
            "--script=bad@1.0.0",
            "https://host/package@1.0.0",
            "unscoped/slash@1.0.0",
            "@scope/sub/pkg@1.0.0",
            "@scope/@1.0.0",
        ] {
            assert!(!pinned(value, true), "expected npm reject for {value}");
        }
        assert!(pinned("@scope/server@1.2.3", true));
        assert!(pinned("server@1.2.3", true));
        assert!(pinned("server==1.2.3", false));
        for value in [
            "server>=1",
            "server==1.*",
            "server==1;extra",
            "-r==1",
            "org/pkg==1.2.3",
            "@scope/pkg==1.2.3",
        ] {
            assert!(!pinned(value, false), "expected python reject for {value}");
        }
    }

    #[test]
    fn expanded_blocks_harness_secrets_and_is_single_pass() {
        let dir = tempfile::tempdir().unwrap();
        for secret in BLOCKED_ENV_VARS {
            let expr = format!("Bearer ${{env:{secret}}}");
            let err = expanded(&expr, dir.path()).unwrap_err();
            assert!(
                err.to_string().contains("blocked"),
                "expected blocked error for {secret}, got: {err}"
            );
        }
        // Single-pass check: an env value containing ${env:...} must NOT re-expand or infinite-loop.
        let key = "VENTICODE_TEST_RECURSIVE_EXPAND";
        unsafe {
            std::env::set_var(key, "${env:VENTICODE_TEST_RECURSIVE_EXPAND}");
        }
        let result = expanded(&format!("prefix-${{env:{key}}}-suffix"), dir.path()).unwrap();
        unsafe {
            std::env::remove_var(key);
        }
        assert_eq!(
            result,
            "prefix-${env:VENTICODE_TEST_RECURSIVE_EXPAND}-suffix"
        );

        // Workspace folder expansion works and unknown placeholders fail.
        let ws = expanded("${workspaceFolder}/sub", dir.path()).unwrap();
        assert_eq!(ws, format!("{}/sub", dir.path().display()));
        assert!(expanded("${input:secret}", dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discover_rejects_symlinked_project_mcp_config() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("evil.json"),
            r#"{"mcpServers":{"evil":{"command":"echo"}}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("evil.json"),
            root.path().join(".mcp.json"),
        )
        .unwrap();
        let (servers, errors) = discover(root.path()).unwrap();
        assert!(!servers.iter().any(|s| s.name == "evil"));
        assert!(errors.iter().any(|e| e["source"] == "project"));
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
