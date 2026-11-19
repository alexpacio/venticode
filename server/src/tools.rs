use super::*;
use crate::history::{append_output, tool_content};
use crate::runs::{append, record, stage};
use std::time::Duration;

const READ: &[&str] = &["list_files", "find_files", "search_files", "read_file"];
const EDIT: &[&str] = &["write_file", "edit_file", "mkdir", "rename", "remove"];
const ACTION: &[&str] = &[
    "execute",
    "terminal_start",
    "agent_start",
    "mcp_call_tool",
    "preview",
];
const COMPUTER_OPERATIONS: &[&str] = &[
    "ask_model",
    "desktop_act",
    "desktop_screenshot",
    "project_service_start",
    "browser_open_preview",
];
const LOCAL_MCP: &[&str] = &["mcp_list_servers", "mcp_list_tools"];

pub(super) fn is_project_operation(op: &str) -> bool {
    READ.contains(&op) || EDIT.contains(&op) || ACTION.contains(&op) || LOCAL_MCP.contains(&op)
}

pub(super) fn is_computer_operation(op: &str) -> bool {
    COMPUTER_OPERATIONS.contains(&op)
}

pub(super) fn definitions(mode: &str, bot: Option<&Value>, standalone: bool) -> Value {
    let runtime = bot.and_then(|b| b["runtime"].as_str()).unwrap_or("project");
    let operations: Vec<_> = READ
        .iter()
        .chain(EDIT)
        .chain(ACTION)
        .chain(LOCAL_MCP.iter().filter(|_| standalone))
        .copied()
        .filter(|op| {
            runtime != "kitesurf"
                && policy(mode, false, op, bot).is_ok_and(|decision| decision != "deny")
        })
        .collect();
    let mut functions = Vec::new();
    if !operations.is_empty() {
        let mut guidance = String::from(
            "Run one listed operation in the selected project. File paths are relative to the project folder.",
        );
        if operations.contains(&"list_files") {
            guidance.push_str(" To inspect the top level, use list_files with empty arguments. search_files takes query and optional path. read_file takes path and optional start_line/end_line.");
        }
        if operations.contains(&"edit_file") {
            guidance.push_str(" write_file takes path and content. To change existing text use edit_file with path and an edits array of {target, replacement} pairs.");
        }
        if operations.contains(&"execute") {
            guidance.push_str(
                " execute takes command as a string and an optional working-directory path.",
            );
        }
        if runtime == "computer" && operations.contains(&"execute") {
            guidance.push_str(" Set arguments.model_access to true when an approved command needs to invoke venticode-cli ask. Use project_service_start for a service that must remain running through this run.");
        }
        if operations.contains(&"mcp_list_servers") {
            guidance.push_str(" mcp_list_servers returns server ids and revisions. mcp_list_tools uses server_id and revision. mcp_call_tool also takes tool and parameters.");
        }
        functions.push(json!({"type":"function","function":{"name":"project_tool",
            "description":guidance,
            "parameters":{"type":"object","additionalProperties":false,"required":["operation","arguments"],"properties":{
                "operation":{"type":"string","enum":operations},"arguments":{"type":"object"}}}}}));
    }
    functions.push(json!({"type":"function","function":{"name":"update_tasks","description":"Replace the task list displayed in this conversation.","parameters":{"type":"object","additionalProperties":false,"required":["todos"],"properties":{"todos":{"type":"array","maxItems":64,"items":{"type":"object","additionalProperties":false,"required":["content","status"],"properties":{"content":{"type":"string","minLength":1,"maxLength":512},"status":{"type":"string","enum":["pending","in_progress","completed"]}}}}}}}}));
    Value::Array(functions)
}
pub(super) fn runtime_definitions(
    output: &mut Value,
    mode: &str,
    bot: Option<&Value>,
    vision: bool,
    model_access: bool,
) {
    let runtime = bot.and_then(|b| b["runtime"].as_str()).unwrap_or("project");
    let tools = output.as_array_mut().expect("tool definitions");
    if matches!(runtime, "kitesurf" | "computer") {
        tools.extend(browser::definitions().filter(|d| {
            policy(
                mode,
                false,
                d["function"]["name"].as_str().unwrap_or_default(),
                bot,
            )
            .is_ok_and(|p| p != "deny")
        }));
    }
    if runtime != "computer" {
        return;
    }
    for (name, description, properties, required, available) in [
        (
            "ask_model",
            "Ask the configured model a separate text question via venticode-cli.",
            json!({"prompt":{"type":"string","maxLength":32768}}),
            vec!["prompt"],
            model_access,
        ),
        (
            "project_service_start",
            "Start a project service for this run.",
            json!({"command":{"type":"string","maxLength":32768}}),
            vec!["command"],
            true,
        ),
        (
            "browser_open_preview",
            "Open a service on this computer's loopback interface.",
            json!({"port":{"type":"integer","minimum":1024,"maximum":65535},"path":{"type":"string"}}),
            vec!["port"],
            true,
        ),
        (
            "desktop_screenshot",
            "Observe the current Xfce desktop.",
            json!({}),
            vec![],
            vision,
        ),
        (
            "desktop_act",
            "Send up to twelve desktop input events. Kinds are click, move, pointer, scroll, text and key.",
            json!({"actions":{"type":"array","minItems":1,"maxItems":12,"items":{"type":"object","required":["kind"],"properties":{
            "kind":{"enum":["click","move","pointer","scroll","text","key"]},"x":{"type":"number"},"y":{"type":"number"},"button":{"type":"integer"},"down":{"type":"boolean"},
            "delta":{"type":"number"},"text":{"type":"string"},"key":{"type":"string"}},"additionalProperties":false}}}),
            vec!["actions"],
            vision,
        ),
    ] {
        if available && policy(mode, false, name, bot).is_ok_and(|decision| decision != "deny") {
            tools.push(json!({"type":"function","function":{"name":name,"description":description,"parameters":{
                "type":"object","properties":properties,"required":required,"additionalProperties":false
            }}}));
        }
    }
}

// Flat schemas let the model generate a path or edit without a second, untyped arguments object.
const FILE_FUNCTIONS: &[(&str, &str, &str, &[&str])] = &[
    (
        "read",
        "read_file",
        "Read a UTF-8 project file before editing it.",
        &["path"],
    ),
    (
        "write",
        "write_file",
        "Create a project file with the supplied contents.",
        &["path", "content"],
    ),
    (
        "edit",
        "edit_file",
        "Replace one unique exact piece of text. Read the original file first.",
        &["path", "oldText", "newText"],
    ),
    (
        "ls",
        "list_files",
        "List entries under a project-relative directory. Omit path for the project root.",
        &[],
    ),
    (
        "grep",
        "search_files",
        "Find occurrences of query in project files.",
        &["query"],
    ),
    (
        "bash",
        "execute",
        "Execute a command with the project host's native shell and return its actual output and exit status.",
        &["command"],
    ),
];

pub(super) fn file_functions(defs: &mut Value, mode: &str) {
    for &(name, op, description, required) in FILE_FUNCTIONS {
        if policy(mode, false, op, None).is_ok_and(|p| p != "deny") {
            let mut properties = json!({});
            for key in required {
                properties[*key] = json!({"type":"string"});
            }
            if matches!(name, "ls" | "grep" | "bash") {
                properties["path"] = json!({"type":"string"});
            }
            defs.as_array_mut().unwrap().push(json!({"type":"function","function":{"name":name,"description":description,
                "parameters":{"type":"object","additionalProperties":false,"properties":properties,"required":required}}}));
        }
    }
}
fn project_arguments(name: &str, body: Value) -> Result<Value> {
    if name == "project_tool" {
        return Ok(body);
    }
    let (_, op, _, required) = FILE_FUNCTIONS
        .iter()
        .find(|f| f.0 == name)
        .ok_or_else(bad)?;
    if !body.is_object() || required.iter().any(|k| !body[*k].is_string()) {
        return Err(bad());
    }
    let args = if name == "edit" {
        json!({"path":body["path"],"edits":[{"target":body["oldText"],"replacement":body["newText"]}]})
    } else {
        body
    };
    Ok(json!({"operation":op,"arguments":args}))
}
fn policy(mode: &str, yolo: bool, op: &str, bot: Option<&Value>) -> Result<&'static str> {
    let category = if READ.contains(&op)
        || matches!(op, "mcp_list_servers" | "desktop_screenshot")
        || browser::is_browser_tool(op)
    {
        "read"
    } else if EDIT.contains(&op) {
        "write"
    } else if matches!(op, "mcp_call_tool" | "mcp_list_tools") {
        "mcp"
    } else if ACTION.contains(&op) || COMPUTER_OPERATIONS.contains(&op) {
        "execute"
    } else {
        return Err(bad());
    };
    if mode == "plan" && category != "read" {
        return Ok("deny");
    }
    if let Some(cfg) = bot {
        return Ok(match (category, cfg["tools"][category].as_str()) {
            _ if matches!(op, "terminal_start" | "agent_start") => "deny",
            (_, Some("off") | None) => "deny",
            ("read", Some("auto")) => "allow",
            _ => "ask",
        });
    }
    Ok(
        if category == "read" || yolo || mode == "auto_edit" && category == "write" {
            "allow"
        } else {
            "ask"
        },
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Decision {
    decision: String,
}
pub(super) async fn decide(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<Decision>,
) -> Result<Json<Value>> {
    let member = Member::load(&app, &h, true).await?;
    let state = match body.decision.as_str() {
        "allow_once" | "allow_project" => "allowed",
        "deny" => "denied",
        _ => return Err(bad()),
    };
    let r=sqlx::query("SELECT a.conversation_id,a.run_id,a.operation,a.fingerprint,c.project_id FROM approvals a JOIN conversation_json c ON c.id=a.conversation_id WHERE a.id=$1 AND workspace=$2 AND owner_id=$3")
        .bind(id).bind(member.workspace).bind(member.user).fetch_one(&app.db).await?;
    let conversation = r.get("conversation_id");
    let run: Uuid = r.get("run_id");
    projects::owned(&app, &member, conversation).await?;
    let persist = body.decision == "allow_project"
        && crate::mentions::bot_context(&app, conversation)
            .await?
            .is_none();
    let event=stage(&app,conversation,json!({"type":"approval","approval_id":id,"call_id":id,"operation":r.get::<String,_>("operation"),"approval_state":state})).await?;
    let mut tx = app.db.begin().await?;
    let c=sqlx::query("UPDATE conversations SET state='running' WHERE id=$1 AND active_run=$2 AND state='waiting'")
        .bind(conversation).bind(run).execute(&mut *tx).await?;
    if c.rows_affected() != 1 {
        return Err(conflict());
    }
    let n = sqlx::query("UPDATE approvals SET state=$2 WHERE id=$1 AND state='pending'")
        .bind(id)
        .bind(state)
        .execute(&mut *tx)
        .await?;
    if n.rows_affected() != 1 {
        return Err(conflict());
    }
    if persist {
        sqlx::query("INSERT INTO project_grants VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(r.get::<Uuid, _>("project_id"))
            .bind(r.get::<String, _>("operation"))
            .bind(r.get::<String, _>("fingerprint"))
            .execute(&mut *tx)
            .await?;
    }
    append(&mut tx, conversation, event).await?;
    tx.commit().await?;
    let _ = app.events.send(conversation);
    Ok(Json(json!({"state":state})))
}
async fn relay(app: &App, h: &HeaderMap, mut body: Value) -> Result<Value> {
    if let Some(computers) = &app.computers {
        let key = match body["action"].as_str() {
            Some("execute") => body["harness_id"]
                .as_str()
                .and_then(|v| v.parse().ok())
                .and_then(|id| computers.runner(id)),
            _ => body[if body["action"] == "input" {
                "session_id"
            } else {
                "request_id"
            }]
            .as_str()
            .and_then(|v| v.parse().ok())
            .and_then(|id| computers.request_runner(id)),
        };
        if let Some(runner) = key {
            let cookie = h
                .get(header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(forbidden)?;
            body["managed"] = json!(true);
            return runner.action(app.storage.fingerprint(cookie), &body);
        }
    }
    if let Some(local) = &app.standalone {
        let session = if body["action"] == "cancel" {
            local.session_hash(h)?
        } else {
            local.authorize(app, h).await?
        };
        return local.runner.action(session, &body);
    }
    let r = app
        .control_request(h, Method::POST, "tool")?
        .json(&body)
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = r.status();
    let v: Value = r.json().await.map_err(|_| unavailable())?;
    if status.is_success() {
        Ok(v)
    } else {
        Err(Error(
            status,
            v["error"]["message"]
                .as_str()
                .unwrap_or("Harness unavailable")
                .into(),
        ))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionInput {
    id: Uuid,
    action: String,
    text: Option<String>,
}
pub(super) async fn input(
    State(app): State<App>,
    h: HeaderMap,
    Path((conversation, call)): Path<(Uuid, Uuid)>,
    Json(b): Json<SessionInput>,
) -> Result<impl IntoResponse> {
    let member = Member::load(&app, &h, true).await?;
    let owned = projects::owned(&app, &member, conversation).await?;
    let active = owned
        .get::<Option<Uuid>, _>("active_run")
        .ok_or_else(conflict)?;
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT c.id FROM conversations c JOIN interactive_calls i ON i.conversation_id=c.id AND i.run_id=c.active_run WHERE c.id=$1 AND i.id=$2 AND c.active_run=$3 FOR SHARE OF c")
        .bind(conversation).bind(call).bind(active).fetch_one(&mut *tx).await?;
    let result=relay(&app,&h,json!({"action":"input","input_action":b.action,"request_id":b.id,"session_id":call,"project_id":owned.get::<Uuid,_>("project_id"),"text":b.text})).await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(result)))
}
// Interrupting a run drops its pending tool future. Revoke its exact consent immediately.
struct CancelOnDrop {
    request: Option<(App, HeaderMap, Uuid)>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some((app, headers, id)) = self.request.take() {
            tokio::spawn(async move {
                let _ = relay(&app, &headers, json!({"action":"cancel","request_id":id})).await;
            });
        }
    }
}
pub(super) async fn call(
    app: &App,
    h: &HeaderMap,
    id: Uuid,
    run: Uuid,
    name: &str,
    body: Value,
    kitesurf: &mut Option<browser::Browser>,
) -> Result<String> {
    if name == "update_tasks" {
        let Some(todos) = body["todos"].as_array().filter(|v| v.len() <= 64) else {
            return Ok(
                "Invalid update_tasks arguments: expected a todos array (max 64 items).".into(),
            );
        };
        if body.as_object().is_none_or(|b| b.len() != 1) {
            return Ok("Invalid update_tasks arguments: only the 'todos' field is allowed.".into());
        }
        for todo in todos {
            if !todo.as_object().is_some_and(|t| t.len() == 2)
                || !todo["content"]
                    .as_str()
                    .is_some_and(|c| !c.trim().is_empty() && c.chars().count() <= 512)
                || !matches!(
                    todo["status"].as_str(),
                    Some("pending" | "in_progress" | "completed")
                )
            {
                return Ok("Invalid update_tasks item: each todo must have non-empty 'content' (<=512 chars) and 'status' in pending|in_progress|completed.".into());
            }
        }
        let member = Member::load(app, h, true).await?;
        projects::owned(app, &member, id).await?;
        record(app, id, run, json!({"type":"todo","todos":todos})).await?;
        return Ok("Task list updated".into());
    }
    let body = if browser::is_browser_tool(name) || COMPUTER_OPERATIONS.contains(&name) {
        json!({"operation":name,"arguments":body})
    } else {
        match project_arguments(name, body) {
            Ok(body) => body,
            Err(_) => {
                return Ok(format!(
                    "Invalid arguments for tool '{name}'. Check required fields and types."
                ));
            }
        }
    };
    let Some(op) = body["operation"].as_str() else {
        return Ok("Invalid tool call: missing 'operation' string.".into());
    };
    if LOCAL_MCP.contains(&op) && app.standalone.is_none() {
        return Err(bad());
    }
    let args = &body["arguments"];
    if !args.is_object() {
        return Ok(format!(
            "Invalid arguments for operation '{op}': expected a JSON object."
        ));
    }
    let member = Member::load(app, h, true).await?;
    let c = projects::owned(app, &member, id).await?;
    let runtime = c.get::<&str, _>("runtime");
    let bot = crate::mentions::bot_context(app, id).await?;
    let browser_tool = browser::is_browser_tool(op);
    let harness = if let Some(harness_id) = c.get::<Option<Uuid>, _>("harness_id") {
        member.harness(harness_id, c.get("root"))?.clone()
    } else {
        Value::Null
    };
    if (runtime == "kitesurf" && !browser_tool)
        || (browser_tool && !matches!(runtime, "kitesurf" | "computer"))
        || (COMPUTER_OPERATIONS.contains(&op) && runtime != "computer")
        || (harness["managed"] == true && matches!(op, "terminal_start" | "agent_start"))
        || (args["model_access"] == true && (runtime != "computer" || op != "execute"))
    {
        return Err(forbidden());
    }
    if browser_tool && browser::validate(op, args).is_err() {
        return Ok(format!(
            "Invalid arguments for browser tool '{op}'. Check required string fields."
        ));
    }
    let project = c.get::<Option<Uuid>, _>("project_id");
    let decision = policy(
        c.get("permission_mode"),
        harness["owned"] == true && harness["mode"] == "yolo",
        op,
        bot.as_ref(),
    )?;
    let call = Uuid::new_v4();
    let target = args.to_string();
    record(
        app,
        id,
        run,
        json!({"type":"tool_call","call_id":call,"operation":op,"arguments":args,"target":target}),
    )
    .await?;
    let allowed = match decision {
        "allow" => true,
        "ask" => {
            let fingerprint = app.storage.fingerprint(&target);
            let granted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM project_grants WHERE project_id=$1 AND operation=$2 AND fingerprint=$3)")
                .bind(project).bind(op).bind(&fingerprint).fetch_one(&app.db).await?;
            if granted && bot.is_none() {
                true
            } else {
                let event=stage(app,id,json!({"type":"approval","call_id":call,"approval_id":call,"operation":op,"approval_state":"pending","summary":target})).await?;
                let waiting = stage(
                    app,
                    id,
                    json!({"type":"run","run_id":run,"state":"waiting"}),
                )
                .await?;
                let mut tx = app.db.begin().await?;
                let n = sqlx::query(
                    "UPDATE conversations SET state='waiting' WHERE id=$1 AND active_run=$2",
                )
                .bind(id)
                .bind(run)
                .execute(&mut *tx)
                .await?;
                if n.rows_affected() != 1 {
                    return Err(conflict());
                }
                sqlx::query("INSERT INTO approvals(id,conversation_id,run_id,operation,fingerprint) VALUES($1,$2,$3,$4,$5)")
                    .bind(call).bind(id).bind(run).bind(op).bind(fingerprint).execute(&mut *tx).await?;
                append(&mut tx, id, event).await?;
                append(&mut tx, id, waiting).await?;
                tx.commit().await?;
                let _ = app.events.send(id);
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let s: String = sqlx::query_scalar("SELECT state FROM approvals WHERE id=$1")
                        .bind(call)
                        .fetch_one(&app.db)
                        .await?;
                    if s == "expired" {
                        return Err(conflict());
                    }
                    if s != "pending" {
                        break s == "allowed";
                    }
                }
            }
        }
        _ => false,
    };
    if !allowed {
        record(app,id,run,json!({"type":"tool_result","call_id":call,"status":"denied","summary":"Operazione negata / Operation denied"})).await?;
        return Ok("Operation denied".into());
    }
    if runtime == "kitesurf" {
        if kitesurf.is_none() {
            *kitesurf = Some(browser::Browser::start(app).await?);
        }
        return surface_result(
            app,
            id,
            run,
            call,
            op,
            kitesurf.as_mut().unwrap().call(op, args).await,
        )
        .await;
    }
    let managed = if harness["managed"] == true {
        Some(
            app.computers
                .as_ref()
                .ok_or_else(unavailable)?
                .ready(app, member.workspace, member.user)
                .await?,
        )
    } else {
        None
    };
    if let Some(runner) = &managed
        && (browser_tool
            || matches!(
                op,
                "desktop_screenshot" | "desktop_act" | "browser_open_preview"
            ))
    {
        let computers = app.computers.as_ref().ok_or_else(unavailable)?;
        let display = computers.display(runner.id());
        let _control = display.read().await;
        sqlx::query("SELECT id FROM conversations WHERE id=$1 AND active_run=$2")
            .bind(id)
            .bind(run)
            .fetch_one(&app.db)
            .await?;
        let (operation, arguments) = match op {
            "desktop_act" => (
                "desktop_input",
                json!({"kind":"batch","actions":args["actions"]}),
            ),
            "desktop_screenshot" => ("desktop_frame", args.clone()),
            _ => (op, args.clone()),
        };
        return surface_result(
            app,
            id,
            run,
            call,
            op,
            computer::rpc(runner, operation, arguments).await,
        )
        .await;
    }
    let mut args = args.clone();
    if op == "project_service_start" {
        args["run_id"] = json!(run);
    }
    // Fence the external dispatch against interruption. The row lock spans the acknowledgement.
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE id=$1 AND active_run=$2 FOR UPDATE")
        .bind(id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
    let response=relay(app,h,json!({"action":"execute","request_id":call,"project_id":project,"harness_id":c.get::<Uuid,_>("harness_id"),
        "root":c.get::<String,_>("root"),"project_path":c.get::<String,_>("path"),"operation":op,"arguments":args,"include_diff":bot.is_none() && harness["owned"] == true})).await;
    tx.commit().await?;
    response?;
    let mut cancel = CancelOnDrop {
        request: Some((app.clone(), h.clone(), call)),
    };
    let mut cursor = 0;
    let mut result = String::new();
    let mut failed = false;
    let mut requires_upgrade = false;
    let mut diff = Value::Null;
    let mut interactive_registered = false;
    let mut pending_stdout = Vec::<u8>::new();
    let mut pending_stderr = Vec::<u8>::new();
    let local_runner = managed
        .as_ref()
        .map(Arc::as_ref)
        .or_else(|| app.standalone.as_ref().map(|local| &local.runner));
    loop {
        let frame = relay(
            app,
            h,
            json!({"action":"poll","request_id":call,"after":cursor}),
        )
        .await?;
        let entries = frame["frames"].as_array().ok_or_else(unavailable)?;
        for e in entries {
            if e["type"] == "complete" {
                failed |= e["success"] != true;
                continue;
            }
            let mut payload = e["result"].clone();
            if payload["type"] == "model_request" {
                if let Some(runner) = &managed
                    && (op == "ask_model" || op == "execute" && args["model_access"] == true)
                {
                    computer::complete_model_request(app, h, id, run, call, runner, &payload)
                        .await?;
                }
                continue;
            }
            requires_upgrade |=
                payload["type"] == "error" && payload["error"] == "Unsupported harness operation";
            failed |= payload["type"] == "error"
                || (payload["type"] == "exit" && payload["code"].as_i64() != Some(0));
            if matches!(op, "edit_file" | "write_file")
                && let Some(patch) = payload.as_object_mut().and_then(|p| p.remove("diff"))
                && patch["path"] == args["path"]
                && patch["patch"].is_string()
                && patch["additions"].is_u64()
                && patch["deletions"].is_u64()
            {
                diff = patch;
            }
            if payload["type"] == "session" && bot.is_none() {
                let interactive = match op {
                    "terminal_start" => "terminal",
                    "agent_start" => "agent",
                    _ => return Err(bad()),
                };
                sqlx::query(
                    "INSERT INTO interactive_calls VALUES($1,$2,$3) ON CONFLICT DO NOTHING",
                )
                .bind(call)
                .bind(id)
                .bind(run)
                .execute(&app.db)
                .await?;
                interactive_registered = true;
                record(app,id,run,json!({"type":"tool_output","call_id":call,"interactive":interactive,"stream":"stdout","data":""})).await?;
                continue;
            }
            let (stream, data) = if payload["type"] == "output" {
                use base64::{Engine, engine::general_purpose::STANDARD};
                let bytes = STANDARD
                    .decode(payload["data"].as_str().ok_or_else(bad)?)
                    .map_err(|_| bad())?;
                let stream = if payload["stream"] == "stderr" {
                    "stderr"
                } else {
                    "stdout"
                };
                let buf = if stream == "stderr" {
                    &mut pending_stderr
                } else {
                    &mut pending_stdout
                };
                buf.extend_from_slice(&bytes);
                let decoded = take_utf8_prefix(buf, false);
                if decoded.is_empty() {
                    continue;
                }
                (stream, decoded)
            } else {
                (
                    "stdout",
                    payload
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| payload.to_string()),
                )
            };
            record(
                app,
                id,
                run,
                json!({"type":"tool_output","call_id":call,"stream":stream,"data":data}),
            )
            .await?;
            append_output(&mut result, &data);
        }
        cursor = frame["after"].as_i64().ok_or_else(unavailable)?;
        if !matches!(frame["state"].as_str(), Some("pending" | "sent")) && entries.len() < 32 {
            for (stream, buf) in [
                ("stdout", &mut pending_stdout),
                ("stderr", &mut pending_stderr),
            ] {
                let tail = take_utf8_prefix(buf, true);
                if !tail.is_empty() {
                    record(
                        app,
                        id,
                        run,
                        json!({"type":"tool_output","call_id":call,"stream":stream,"data":tail}),
                    )
                    .await?;
                    append_output(&mut result, &tail);
                }
            }
            if interactive_registered {
                let _ = sqlx::query("DELETE FROM interactive_calls WHERE id=$1")
                    .bind(call)
                    .execute(&app.db)
                    .await;
            }
            if let Some(runner) = local_runner {
                runner.forget(call);
            }
            let done = frame["state"] == "done";
            let (status, summary) = if requires_upgrade {
                (
                    "error",
                    "Aggiorna e riavvia venticode-harness per usare gli strumenti di progetto / Update and restart venticode-harness to use project tools",
                )
            } else if !done {
                (
                    "error",
                    "Esito non confermato. Verifica il progetto prima di riprovare / Outcome unconfirmed. Verify the project before retrying",
                )
            } else if failed {
                (
                    "error",
                    "Operazione non riuscita. Verifica il progetto prima di riprovare / Operation failed. Verify the project before retrying",
                )
            } else {
                ("ok", "Operazione completata / Operation completed")
            };
            let mut event =
                json!({"type":"tool_result","call_id":call,"status":status,"summary":summary});
            if status == "ok" && !diff.is_null() {
                event["diff"] = diff;
            }
            record(app, id, run, event).await?;
            cancel.request = None;
            if requires_upgrade {
                return Err(Error(StatusCode::CONFLICT, summary.into()));
            }
            if !done {
                return Err(Error(StatusCode::SERVICE_UNAVAILABLE, summary.into()));
            }
            return Ok(tool_content(status, summary, &result));
        }
        if entries.len() < 32 {
            if let Some(runner) = local_runner {
                runner.wait(Duration::from_millis(200)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

fn take_utf8_prefix(buf: &mut Vec<u8>, flush: bool) -> String {
    if buf.is_empty() {
        return String::new();
    }
    if flush {
        let out = String::from_utf8_lossy(buf).into_owned();
        buf.clear();
        return out;
    }
    let mut out = String::new();
    loop {
        match std::str::from_utf8(buf) {
            Ok(valid) => {
                out.push_str(valid);
                buf.clear();
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if valid_len > 0 {
                    out.push_str(std::str::from_utf8(&buf[..valid_len]).unwrap());
                    buf.drain(..valid_len);
                }
                match err.error_len() {
                    Some(invalid_len) => {
                        out.push('\u{FFFD}');
                        buf.drain(..invalid_len);
                    }
                    None => break,
                }
            }
        }
    }
    out
}

async fn surface_result(
    app: &App,
    id: Uuid,
    run: Uuid,
    call: Uuid,
    op: &str,
    value: Result<Value>,
) -> Result<String> {
    match value {
        Ok(value) if op == "desktop_screenshot" => {
            record(app,id,run,json!({"type":"tool_result","call_id":call,"status":"ok","summary":"Desktop screenshot"})).await?;
            Ok(value.to_string())
        }
        Ok(value) => {
            let text = value.to_string();
            record(
                app,
                id,
                run,
                json!({"type":"tool_output","call_id":call,"stream":"stdout","data":text}),
            )
            .await?;
            record(app,id,run,json!({"type":"tool_result","call_id":call,"status":"ok","summary":"Operation completed"})).await?;
            Ok(text)
        }
        Err(error) => {
            record(
                app,
                id,
                run,
                json!({"type":"tool_result","call_id":call,"status":"error","summary":error.1}),
            )
            .await?;
            Ok(error.1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schemas_only_offer_available_operations() {
        let plan = definitions("plan", None, false);
        let operations = |schema: &Value| {
            schema[0]["function"]["parameters"]["properties"]["operation"]["enum"].clone()
        };
        assert_eq!(operations(&plan), json!(READ));
        let bot = json!({"tools":{"write":"review","execute":"review"}});
        let available = operations(&definitions("ask", Some(&bot), false));
        let available = available.as_array().unwrap();
        assert!(available.contains(&json!("write_file")));
        for excluded in [
            "read_file",
            "terminal_start",
            "agent_start",
            "mcp_call_tool",
        ] {
            assert!(!available.contains(&json!(excluded)));
        }
        let tasks_only = definitions("plan", Some(&bot), false);
        assert_eq!(tasks_only.as_array().unwrap().len(), 1);
        assert_eq!(tasks_only[0]["function"]["name"], "update_tasks");
    }
    #[test]
    fn plan_is_read_only_even_on_owned_yolo() {
        assert_eq!(policy("plan", true, "execute", None).ok(), Some("deny"));
        assert_eq!(policy("plan", false, "read_file", None).ok(), Some("allow"));
        assert_eq!(
            policy("auto_edit", false, "execute", None).ok(),
            Some("ask")
        );
        assert_eq!(policy("ask", false, "write_file", None).ok(), Some("ask"));
        assert!(policy("ask", true, "unknown", None).is_err());
    }

    #[test]
    fn browser_tools_are_shared_and_machine_tools_require_their_capabilities() {
        let bot = |runtime| json!({"runtime":runtime,"tools":{"read":"auto","execute":"review"}});
        let runtime_names = |runtime, vision, model| {
            let bot = bot(runtime);
            let mut tools = definitions("ask", Some(&bot), false);
            runtime_definitions(&mut tools, "ask", Some(&bot), vision, model);
            tools
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let kitesurf = runtime_names("kitesurf", true, true);
        let computer = runtime_names("computer", false, false);
        assert!(
            !kitesurf
                .iter()
                .any(|name| name == "project_tool" || name == "ask_model")
        );
        assert!(
            kitesurf
                .iter()
                .filter(|name| browser::is_browser_tool(name))
                .all(|name| computer.contains(name))
        );
        assert!(
            !computer
                .iter()
                .any(|name| name.starts_with("desktop_") || name == "ask_model")
        );
        let computer = runtime_names("computer", true, true);
        assert!(computer.iter().any(|name| name == "desktop_screenshot"));
        assert!(computer.iter().any(|name| name == "ask_model"));
    }
}
