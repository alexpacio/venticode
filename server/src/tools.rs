use super::*;
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
pub(super) fn definitions() -> Value {
    let names: Vec<_> = READ.iter().chain(EDIT).chain(ACTION).collect();
    json!([{"type":"function","function":{"name":"project_tool","description":"Work on the selected project. Paths are relative to the project. For execute supply command as a string. For a file read or write supply path, plus content for writes. Operations and their exact arguments are reviewed according to project policy.",
        "parameters":{"type":"object","additionalProperties":false,"required":["operation","arguments"],"properties":{
        "operation":{"type":"string","enum":names},"arguments":{"type":"object"}}}}},
      {"type":"function","function":{"name":"update_tasks","description":"Replace the task list displayed in this conversation.","parameters":{"type":"object","additionalProperties":false,"required":["todos"],"properties":{"todos":{"type":"array","maxItems":64,"items":{"type":"object","additionalProperties":false,"required":["content","status"],"properties":{"content":{"type":"string","minLength":1,"maxLength":512},"status":{"type":"string","enum":["pending","in_progress","completed"]}}}}}}}}])
}
fn policy(mode: &str, yolo: bool, op: &str) -> Result<&'static str> {
    if !READ.contains(&op) && !EDIT.contains(&op) && !ACTION.contains(&op) {
        return Err(bad());
    }
    Ok(if READ.contains(&op) {
        "allow"
    } else if mode == "plan" {
        "deny"
    } else if yolo || mode == "auto_edit" && EDIT.contains(&op) {
        "allow"
    } else {
        "ask"
    })
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
    Ok(Json(json!({"state":state})))
}
async fn relay(app: &App, h: &HeaderMap, body: Value) -> Result<Value> {
    let r = app
        .control_request(h, Method::POST, "tool")
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
) -> Result<String> {
    if name == "update_tasks" {
        let todos = body["todos"]
            .as_array()
            .filter(|v| v.len() <= 64)
            .ok_or_else(bad)?;
        if body.as_object().is_none_or(|b| b.len() != 1) {
            return Err(bad());
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
                return Err(bad());
            }
        }
        let member = Member::load(app, h, true).await?;
        projects::owned(app, &member, id).await?;
        record(app, id, run, json!({"type":"todo","todos":todos})).await?;
        return Ok("Task list updated".into());
    }
    if name != "project_tool" {
        return Err(bad());
    }
    let op = body["operation"].as_str().ok_or_else(bad)?;
    let args = &body["arguments"];
    if !args.is_object() {
        return Err(bad());
    }
    let member = Member::load(app, h, true).await?;
    let c = projects::owned(app, &member, id).await?;
    let harness = member.harness(c.get("harness_id"), c.get("root"))?;
    let project: Uuid = c.get("project_id");
    let mut decision = policy(
        c.get("permission_mode"),
        harness["owned"] == true && harness["mode"] == "yolo",
        op,
    )?;
    let bot = crate::mentions::bot_context(app, id).await?;
    if let Some(cfg) = &bot
        && decision != "deny"
    {
        let category = if READ.contains(&op) {
            "read"
        } else if EDIT.contains(&op) {
            "write"
        } else if op == "mcp_call_tool" {
            "mcp"
        } else {
            "execute"
        };
        decision = match (category, cfg["tools"][category].as_str()) {
            _ if matches!(op, "terminal_start" | "agent_start") => "deny",
            (_, Some("off") | None) => "deny",
            ("read", Some("auto")) => "allow",
            _ => "ask",
        };
    }
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
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let s: String = sqlx::query_scalar("SELECT state FROM approvals WHERE id=$1")
                        .bind(call)
                        .fetch_one(&app.db)
                        .await?;
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
    let mut diff = Value::Null;
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
                (stream, String::from_utf8_lossy(&bytes).into_owned())
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
            if result.len() < 65536 {
                result.extend(data.chars().take(65536 - result.len()));
            }
        }
        cursor = frame["after"].as_i64().ok_or_else(unavailable)?;
        if !matches!(frame["state"].as_str(), Some("pending" | "sent")) && entries.len() < 32 {
            let status = if !failed && frame["state"] == "done" {
                "ok"
            } else {
                "error"
            };
            let mut event = json!({"type":"tool_result","call_id":call,"status":status,"summary":frame["state"]});
            if status == "ok" && !diff.is_null() {
                event["diff"] = diff;
            }
            record(app, id, run, event).await?;
            cancel.request = None;
            if status != "ok" {
                return Err(unavailable());
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_is_read_only_even_on_owned_yolo() {
        assert_eq!(policy("plan", true, "execute").ok(), Some("deny"));
        assert_eq!(policy("plan", false, "read_file").ok(), Some("allow"));
        assert_eq!(policy("auto_edit", false, "execute").ok(), Some("ask"));
        assert_eq!(policy("ask", false, "write_file").ok(), Some("ask"));
        assert!(policy("ask", true, "unknown").is_err());
    }
}
