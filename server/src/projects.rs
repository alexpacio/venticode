use super::*;

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/projects", get(list).post(create))
        .route("/projects/{id}", delete(remove))
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
    let rows=sqlx::query("SELECT document,harness_id,root FROM project_json WHERE workspace=$1 AND owner_id=$2 AND ($3::uuid IS NULL OR harness_id=$3) ORDER BY created_at,id")
        .bind(m.workspace).bind(m.user).bind(q.harness_id).fetch_all(&app.db).await?;
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
    title: Option<String>,
    permission_mode: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    revision: i64,
    model: Option<String>,
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
fn validate(title: Option<&str>, mode: Option<&str>) -> Result<()> {
    if title.is_some_and(|s| s.trim().is_empty() || s.chars().count() > 200)
        || mode.is_some_and(|s| !matches!(s, "ask" | "auto_edit" | "plan"))
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
    validate(body.title.as_deref(), body.permission_mode.as_deref())?;
    let model = model(&app, &h, body.model.as_deref()).await?;
    let mut tx = app.db.begin().await?;
    let row=sqlx::query("SELECT harness_id,root FROM projects WHERE workspace=$1 AND owner_id=$2 AND id=$3 FOR SHARE")
        .bind(m.workspace).bind(m.user).bind(body.project_id).fetch_one(&mut *tx).await?;
    if !visible(&m, &row) {
        return Err(missing());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO conversations(id,project_id,title,model,permission_mode) VALUES($1,$2,$3,$4,$5)")
        .bind(id).bind(body.project_id).bind(body.title.as_deref().unwrap_or("Nuova conversazione").trim())
        .bind(model).bind(body.permission_mode.as_deref().unwrap_or("ask")).execute(&mut *tx).await?;
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
    let rows=sqlx::query("SELECT document,harness_id,root,runtime FROM conversation_json WHERE workspace=$1 AND owner_id=$2 AND ($3::uuid IS NULL OR project_id=$3) ORDER BY updated_at DESC,id")
        .bind(m.workspace).bind(m.user).bind(q.project_id).fetch_all(&app.db).await?;
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
    if body.title.is_none() && body.permission_mode.is_none() && body.model.is_none() {
        return Err(bad());
    }
    validate(body.title.as_deref(), body.permission_mode.as_deref())?;
    if let Some(name) = &body.model {
        model(&app, &h, Some(name)).await?;
    }
    let mut tx = app.db.begin().await?;
    let result=sqlx::query("UPDATE conversations SET title=COALESCE($3,title),model=COALESCE($4,model),plan_id=CASE WHEN $5 IS NOT NULL AND $5!=permission_mode THEN NULL ELSE plan_id END,permission_mode=COALESCE($5,permission_mode),revision=revision+1,updated_at=now() WHERE id=$1 AND revision=$2 AND active_run IS NULL")
        .bind(id).bind(body.revision).bind(body.title.as_deref().map(str::trim)).bind(body.model).bind(body.permission_mode).execute(&mut *tx).await?;
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
