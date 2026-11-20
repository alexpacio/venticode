use super::*;

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/projects", get(list).post(create))
        .route("/projects/{id}", delete(remove))
        .route("/projects/{id}/imports", get(foreign_sessions).post(import))
        .route("/conversations", get(conversations).post(start))
        .route(
            "/conversations/{id}",
            get(conversation).patch(update).delete(remove_conversation),
        )
}
#[derive(Deserialize)]
struct Filter {
    harness_id: Option<Uuid>,
    project_id: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProject {
    harness_id: Uuid,
    root: String,
    path: Option<String>,
    name: Option<String>,
}
pub(super) fn relative(path: &str) -> bool {
    path == "."
        || (path.len() <= 1024
            && !path.contains(['\\', ':', '\0'])
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}
fn visible(member: &Member, row: &sqlx::postgres::PgRow) -> bool {
    if let Some(id) = row.get::<Option<Uuid>, _>("harness_id") {
        member.harness(id, row.get("root")).is_ok()
    } else {
        row.try_get::<&str, _>("runtime")
            .is_ok_and(|runtime| runtime == "kitesurf")
    }
}
async fn create(
    State(app): State<App>,
    h: HeaderMap,
    Json(body): Json<NewProject>,
) -> Result<impl IntoResponse> {
    let m = Member::load(&app, &h, true).await?;
    m.harness(body.harness_id, &body.root)?;
    let path = body.path.as_deref().unwrap_or(".");
    let name = body
        .name
        .as_deref()
        .unwrap_or_else(|| {
            if path == "." {
                &body.root
            } else {
                path.rsplit('/').next().unwrap_or(path)
            }
        })
        .trim();
    if !relative(path) || name.is_empty() || name.chars().count() > 80 {
        return Err(bad());
    }
    let item: Value = sqlx::query_scalar(
        "INSERT INTO projects AS p(id,workspace,owner_id,harness_id,root,path,name)
        VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING to_jsonb(p)-'workspace'-'owner_id'",
    )
    .bind(Uuid::new_v4())
    .bind(m.workspace)
    .bind(m.user)
    .bind(body.harness_id)
    .bind(&body.root)
    .bind(path)
    .bind(name)
    .fetch_one(&app.db)
    .await?;
    Ok((StatusCode::CREATED, Json(item)))
}
async fn list(
    State(app): State<App>,
    h: HeaderMap,
    Query(q): Query<Filter>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, false).await?;
    let rows = sqlx::query(
        "SELECT document,harness_id,root FROM project_json WHERE workspace=$1 AND owner_id=$2 AND ($3::uuid IS NULL OR harness_id=$3) ORDER BY created_at,id LIMIT 500",
    )
    .bind(m.workspace)
    .bind(m.user)
    .bind(q.harness_id)
    .fetch_all(&app.db)
    .await?;
    Ok(Json(
        json!({"items":rows.iter().filter(|r|visible(&m,r)).map(|r|r.get::<Value,_>("document")).collect::<Vec<_>>()}),
    ))
}

async fn remove(State(app): State<App>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, true).await?;
    let mut tx = app.db.begin().await?;
    let row=sqlx::query("SELECT harness_id,root FROM projects WHERE workspace=$1 AND owner_id=$2 AND id=$3 FOR UPDATE")
        .bind(m.workspace).bind(m.user).bind(id).fetch_one(&mut *tx).await?;
    if !visible(&m, &row) {
        return Err(missing());
    }
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM conversations WHERE project_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    if states
        .iter()
        .any(|v| matches!(v.as_str(), "running" | "waiting"))
    {
        return Err(conflict());
    }
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT FROM connections WHERE project_id=$1 AND status<>'disconnected')",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "Disconnetti prima il bot / Disconnect the bot first".into(),
        ));
    }
    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewConversation {
    project_id: Uuid,
    model: Option<String>,
    effort: Option<String>,
    title: Option<String>,
    permission_mode: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    revision: i64,
    model: Option<String>,
    effort: Option<String>,
    title: Option<String>,
    permission_mode: Option<String>,
}
async fn model(app: &App, h: &HeaderMap, requested: Option<&str>) -> Result<String> {
    let models = app.control_json(h, "models").await?;
    let options = models["items"].as_array().ok_or_else(unavailable)?;
    let selected = if let Some(name) = requested {
        options.iter().find(|m| m["id"] == name)
    } else {
        options
            .iter()
            .find(|m| m["default"] == true)
            .or(options.first())
    };
    selected
        .and_then(|m| m["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(bad)
}
fn validate(title: Option<&str>, mode: Option<&str>, effort: Option<&str>) -> Result<()> {
    if title.is_some_and(|s| s.trim().is_empty() || s.chars().count() > 200)
        || mode.is_some_and(|s| !matches!(s, "ask" | "auto_edit" | "plan"))
        || effort.is_some_and(|s| !matches!(s, "none" | "low" | "medium" | "high"))
    {
        return Err(bad());
    }
    Ok(())
}
async fn start(
    State(app): State<App>,
    h: HeaderMap,
    Json(body): Json<NewConversation>,
) -> Result<impl IntoResponse> {
    let m = Member::load(&app, &h, true).await?;
    validate(
        body.title.as_deref(),
        body.permission_mode.as_deref(),
        body.effort.as_deref(),
    )?;
    let model = model(&app, &h, body.model.as_deref()).await?;
    let mut tx = app.db.begin().await?;
    let row=sqlx::query("SELECT harness_id,root FROM projects WHERE workspace=$1 AND owner_id=$2 AND id=$3 FOR SHARE")
        .bind(m.workspace).bind(m.user).bind(body.project_id).fetch_one(&mut *tx).await?;
    if !visible(&m, &row) {
        return Err(missing());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO conversations(id,project_id,title,model,effort,permission_mode) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(body.project_id).bind(body.title.as_deref().unwrap_or("Nuova conversazione").trim())
        .bind(model).bind(body.effort.as_deref().unwrap_or("medium"))
        .bind(body.permission_mode.as_deref().unwrap_or("ask")).execute(&mut *tx).await?;
    let item: Value = sqlx::query_scalar("SELECT document FROM conversation_json WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(item)))
}
async fn conversations(
    State(app): State<App>,
    h: HeaderMap,
    Query(q): Query<Filter>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, false).await?;
    let rows = sqlx::query(
        "SELECT document,harness_id,root,runtime FROM conversation_json WHERE workspace=$1 AND owner_id=$2 AND ($3::uuid IS NULL OR project_id=$3) ORDER BY updated_at DESC,id LIMIT 500",
    )
    .bind(m.workspace)
    .bind(m.user)
    .bind(q.project_id)
    .fetch_all(&app.db)
    .await?;
    Ok(Json(
        json!({"items":rows.iter().filter(|r|visible(&m,r)).take(200).map(|r|r.get::<Value,_>("document")).collect::<Vec<_>>()}),
    ))
}
pub(super) async fn owned(app: &App, m: &Member, id: Uuid) -> Result<sqlx::postgres::PgRow> {
    let row =
        sqlx::query("SELECT * FROM conversation_json WHERE workspace=$1 AND owner_id=$2 AND id=$3")
            .bind(m.workspace)
            .bind(m.user)
            .bind(id)
            .fetch_one(&app.db)
            .await?;
    if !visible(m, &row) {
        return Err(missing());
    }
    Ok(row)
}
async fn conversation(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, false).await?;
    Ok(Json(owned(&app, &m, id).await?.get("document")))
}
async fn update(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<Edit>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, true).await?;
    owned(&app, &m, id).await?;
    if body.title.is_none()
        && body.permission_mode.is_none()
        && body.model.is_none()
        && body.effort.is_none()
    {
        return Err(bad());
    }
    validate(
        body.title.as_deref(),
        body.permission_mode.as_deref(),
        body.effort.as_deref(),
    )?;
    if let Some(name) = &body.model {
        model(&app, &h, Some(name)).await?;
    }
    let mut tx = app.db.begin().await?;
    let result=sqlx::query("UPDATE conversations SET title=COALESCE($3,title),model=COALESCE($4,model),effort=COALESCE($5,effort),plan_id=CASE WHEN $6 IS NOT NULL AND $6!=permission_mode THEN NULL ELSE plan_id END,permission_mode=COALESCE($6,permission_mode),revision=revision+1,updated_at=now() WHERE id=$1 AND revision=$2 AND active_run IS NULL")
        .bind(id).bind(body.revision).bind(body.title.as_deref().map(str::trim)).bind(body.model).bind(body.effort.as_deref()).bind(body.permission_mode).execute(&mut *tx).await?;
    if result.rows_affected() != 1 {
        return Err(conflict());
    }
    let item = sqlx::query_scalar("SELECT document FROM conversation_json WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(item))
}
async fn remove_conversation(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, true).await?;
    owned(&app, &m, id).await?;
    if sqlx::query("DELETE FROM conversations WHERE id=$1 AND active_run IS NULL")
        .bind(id)
        .execute(&app.db)
        .await?
        .rows_affected()
        != 1
    {
        return Err(conflict());
    }
    Ok(Json(json!({"deleted":true})))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn granted_root_relative_paths() {
        for p in [".", "src", "foo/bar"] {
            assert!(relative(p));
        }
        for p in [
            "",
            "..",
            "/src",
            "src/../other",
            "C:/win",
            "src\\other",
            "./src",
            "src//x",
        ] {
            assert!(!relative(p));
        }
    }
}

const MAX_IMPORTED_EVENTS: usize = 20_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    id: String,
}

/// Run an import operation on the project's harness, scoped to the project folder.
async fn foreign(
    app: &App,
    h: &HeaderMap,
    m: &Member,
    project: Uuid,
    operation: &str,
    arguments: Value,
) -> Result<Value> {
    let row = sqlx::query(
        "SELECT harness_id,root,path FROM projects WHERE workspace=$1 AND owner_id=$2 AND id=$3",
    )
    .bind(m.workspace)
    .bind(m.user)
    .bind(project)
    .fetch_optional(&app.db)
    .await?
    .ok_or_else(missing)?;
    let harness: Uuid = row
        .get::<Option<Uuid>, _>("harness_id")
        .ok_or_else(missing)?;
    let root: String = row.get("root");
    m.harness(harness, &root)?;
    let request_id = Uuid::new_v4();
    let execute = json!({
        "action": "execute", "request_id": request_id, "project_id": project,
        "harness_id": harness, "root": root, "project_path": row.get::<String, _>("path"),
        "operation": operation, "arguments": arguments,
    });
    crate::browse::browse(app, h, &execute, request_id).await
}

/// `GET /projects/{id}/imports` — Claude Code and Codex sessions recorded for this project.
async fn foreign_sessions(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, false).await?;
    let value = foreign(&app, &h, &m, id, "foreign_sessions", json!({})).await?;
    let items: Vec<Value> = value["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            Some(json!({
                "id": i["id"].as_str().filter(|s| s.len() <= 512)?,
                "source": i["source"].as_str()?,
                "title": i["title"].as_str().unwrap_or_default().chars().take(200).collect::<String>(),
                "modified_ms": i["modified_ms"].as_u64().unwrap_or(0),
            }))
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// The harness is outside the trust boundary: keep only known event shapes and fields.
fn imported_event(e: &Value) -> Option<Value> {
    let s = |k: &str| e[k].as_str();
    let kind = s("type")?;
    Some(match kind {
        "user_message" => {
            json!({"type":kind,"message_id":s("message_id")?,"content":s("content")?})
        }
        "text" | "reasoning" => {
            json!({"type":kind,"message_id":s("message_id")?,"delta":s("delta")?})
        }
        "tool_call" => json!({"type":kind,"call_id":s("call_id")?,"operation":s("operation")?,
            "arguments":Value::Object(e["arguments"].as_object()?.clone()),"target":s("target").unwrap_or_default()}),
        "tool_output" => json!({"type":kind,"call_id":s("call_id")?,"data":s("data")?}),
        "tool_result" => json!({"type":kind,"call_id":s("call_id")?,
            "status":s("status").filter(|v| matches!(*v, "ok" | "error"))?,"summary":s("summary").unwrap_or_default()}),
        _ => return None,
    })
}

/// `POST /projects/{id}/imports` — import one listed session as a new conversation.
async fn import(
    State(app): State<App>,
    h: HeaderMap,
    Path(project): Path<Uuid>,
    Json(body): Json<Import>,
) -> Result<impl IntoResponse> {
    let m = Member::load(&app, &h, true).await?;
    if body.id.is_empty() || body.id.len() > 512 {
        return Err(bad());
    }
    let value = foreign(
        &app,
        &h,
        &m,
        project,
        "foreign_session_import",
        json!({"id": body.id}),
    )
    .await?;
    let events: Vec<Value> = value["events"]
        .as_array()
        .ok_or_else(unavailable)?
        .iter()
        .take(MAX_IMPORTED_EVENTS)
        .filter_map(imported_event)
        .collect();
    if events.is_empty() {
        return Err(bad());
    }
    let source = value["source"].as_str().unwrap_or("Import");
    let first = value["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let title: String = format!(
        "{source} · {}",
        first.unwrap_or("sessione importata / imported session")
    )
    .chars()
    .take(200)
    .collect();
    let model = model(&app, &h, None).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO conversations(id,project_id,title,model,effort,permission_mode) VALUES($1,$2,$3,$4,'medium','ask')")
        .bind(id).bind(project).bind(&title).bind(model).execute(&app.db).await?;
    let mut objects = Vec::with_capacity(events.len() + 1);
    use futures_util::StreamExt;
    let mut staged =
        futures_util::stream::iter(events.into_iter().map(|e| crate::runs::stage(&app, id, e)))
            .buffered(16);
    while let Some(object) = staged.next().await {
        objects.push(object?);
    }
    drop(staged);
    if value["truncated"] == true {
        objects.push(crate::runs::stage(&app, id, json!({"type":"text","message_id":"import-truncated",
            "delta":"[Importazione troncata: la sessione è troppo grande / Import truncated: the session is too large]"})).await?);
    }
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    for object in objects {
        crate::runs::append(&mut tx, id, object).await?;
    }
    let item: Value = sqlx::query_scalar("SELECT document FROM conversation_json WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(item)))
}
