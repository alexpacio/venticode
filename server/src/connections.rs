use super::*;
use axum::response::Redirect;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/connections", get(list).post(create))
        .route("/connections/{id}", delete(disconnect).put(rotate))
        .route("/connections/{id}/link", post(link).delete(unlink))
        .route("/bots", get(bots))
        .route("/bots/{id}", get(bot).patch(edit_bot))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    kind: String,
    destination: String,
    project_id: Option<Uuid>,
    runtime: Option<String>,
    credentials: Value,
}
pub(super) fn digest(v: &str) -> String {
    hex::encode(Sha256::digest(v.as_bytes()))
}
fn random() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
pub(super) fn host(h: &HeaderMap) -> Result<&str> {
    h.get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(forbidden)
}
pub(super) fn origin(app: &App, h: &HeaderMap) -> Result<String> {
    if let Some(local) = &app.standalone {
        local.validate_host(h, false)?;
        Ok(local.origin.clone())
    } else {
        Ok(format!("https://{}", host(h)?))
    }
}
pub(super) async fn member(
    app: &App,
    h: &HeaderMap,
    write: bool,
    admin: bool,
) -> Result<(Uuid, Uuid)> {
    if write && h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(&origin(app, h)?) {
        return Err(forbidden());
    }
    let v = app.control_json(h, "session").await?;
    if admin && !matches!(v["role"].as_str(), Some("tenant_admin" | "reseller_admin")) {
        return Err(forbidden());
    }
    Ok((
        serde_json::from_value(v["workspace"]["id"].clone()).map_err(|_| unavailable())?,
        serde_json::from_value(v["user"]["id"].clone()).map_err(|_| unavailable())?,
    ))
}
pub(super) async fn private(app: &App, h: &HeaderMap, b: Value) -> Result<Value> {
    if let Some(local) = &app.standalone {
        return local.bot_identity(app, h, b).await;
    }
    let res = app
        .control_request(h, Method::POST, "connection")?
        .json(&b)
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = res.status();
    let v: Value = res.json().await.map_err(|_| unavailable())?;
    if !status.is_success() {
        return Err(Error(
            status,
            v["error"]["message"]
                .as_str()
                .unwrap_or("Connection unavailable")
                .into(),
        ));
    }
    Ok(v)
}
pub(super) async fn provider(app: &App, b: Value) -> Result<Value> {
    let url = env::var("CONNECTOR_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:4190/exchange".into());
    let res = app
        .http
        .post(&url)
        .json(&b)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "connector request failed");
            unavailable()
        })?;
    if !res.status().is_success() {
        tracing::warn!(
            status = res.status().as_u16(),
            "connector returned error status"
        );
        return Err(unavailable());
    }
    res.json().await.map_err(|_| unavailable())
}
pub(super) async fn creds(app: &App, h: &HeaderMap, id: Uuid) -> Result<Value> {
    if let Some(local) = &app.standalone {
        local.validate_host(h, false)?;
        let sealed: Vec<u8> = sqlx::query_scalar("SELECT standalone_credentials FROM connections WHERE id=$1 AND workspace=$2 AND standalone_credentials IS NOT NULL")
            .bind(id).bind(local.workspace).fetch_one(&app.db).await?;
        return app
            .storage
            .open(&credential_binding(local.workspace, id), &sealed);
    }
    private(app, h, json!({"action":"read","connection_id":id})).await
}

fn credential_binding(workspace: Uuid, connection: Uuid) -> Vec<u8> {
    [
        b"venticode-bot-credentials-v1".as_slice(),
        workspace.as_bytes(),
        connection.as_bytes(),
    ]
    .concat()
}

async fn store_creds(
    app: &App,
    h: &HeaderMap,
    tx: &mut sqlx::PgTransaction<'_>,
    id: Uuid,
    credentials: &Value,
) -> Result<()> {
    if let Some(local) = &app.standalone {
        let sealed = app
            .storage
            .seal(&credential_binding(local.workspace, id), credentials)?;
        sqlx::query(
            "UPDATE connections SET standalone_credentials=$3 WHERE id=$1 AND workspace=$2",
        )
        .bind(id)
        .bind(local.workspace)
        .bind(sealed)
        .execute(&mut **tx)
        .await?;
    } else {
        private(
            app,
            h,
            json!({"action":"store","connection_id":id,"credentials":credentials}),
        )
        .await?;
    }
    Ok(())
}

async fn remove_grant(tx: &mut sqlx::PgTransaction<'_>, id: Uuid, owner: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM standalone_bot_grants WHERE connection_id=$1 AND owner_id=$2")
        .bind(id)
        .bind(owner)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn summary(r: &PgRow, origin: &str) -> Value {
    let id: Uuid = r.get("id");
    let kind: &str = r.get("kind");
    json!({"id":id,"kind":kind,"destination":r.get::<String,_>("destination"),"identity":r.get::<String,_>("identity"),"status":r.get::<String,_>("status"),
        "linked_user_id":r.try_get::<Option<String>,_>("linked_user_id").unwrap_or(None),"project_id":r.get::<Option<Uuid>,_>("project_id"),"runtime":r.get::<String,_>("runtime"),"webhook_url":format!("{origin}/hooks/{kind}/{id}")})
}
async fn list(State(app): State<App>, h: HeaderMap) -> Result<Json<Value>> {
    let (w, u) = member(&app, &h, false, false).await?;
    let origin = origin(&app, &h)?;
    let rows = sqlx::query("SELECT c.*,l.external_id AS linked_user_id FROM connections c LEFT JOIN external_links l ON l.connection_id=c.id AND l.user_id=$2 WHERE c.workspace=$1 ORDER BY c.id")
        .bind(w).bind(u)
        .fetch_all(&app.db)
        .await?;
    Ok(Json(
        json!({"items":rows.iter().map(|r|summary(r,&origin)).collect::<Vec<_>>()}),
    ))
}
async fn create(
    State(app): State<App>,
    h: HeaderMap,
    Json(b): Json<Setup>,
) -> Result<impl IntoResponse> {
    Ok((StatusCode::CREATED, Json(setup(&app, &h, None, b).await?)))
}
async fn rotate(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(b): Json<Setup>,
) -> Result<Json<Value>> {
    Ok(Json(setup(&app, &h, Some(id), b).await?))
}
async fn setup(app: &App, h: &HeaderMap, existing: Option<Uuid>, mut b: Setup) -> Result<Value> {
    let (w, u) = member(app, h, true, true).await?;
    if !matches!(b.kind.as_str(), "slack" | "telegram" | "gitlab")
        || b.destination.is_empty()
        || b.destination.len() > 64
        || !b.credentials.is_object()
    {
        return Err(bad());
    }
    let keys = if b.kind == "slack" {
        &["client_id", "client_secret", "signing_secret"][..]
    } else {
        &["token"][..]
    };
    let supplied = b.credentials.as_object().ok_or_else(bad)?;
    if supplied.len() != keys.len()
        || keys.iter().any(|key| {
            !supplied
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty() && v.len() <= 512)
        })
    {
        return Err(bad());
    }
    let valid = match b.kind.as_str() {
        "slack" => b.destination.bytes().all(|v| v.is_ascii_alphanumeric()),
        "telegram" => b.destination.parse::<i64>().is_ok(),
        _ => b.destination.parse::<u64>().is_ok(),
    };
    if !valid {
        return Err(bad());
    }
    let m = Member::load(app, h, true).await?;
    let id = existing.unwrap_or_else(Uuid::new_v4);
    let mut tx = app.db.begin().await?;
    if existing.is_some() {
        let row=sqlx::query("SELECT kind,destination,project_id,runtime FROM connections WHERE id=$1 AND workspace=$2 FOR UPDATE")
            .bind(id).bind(w).fetch_one(&mut *tx).await?;
        if row.get::<String, _>("kind") != b.kind
            || row.get::<String, _>("destination") != b.destination
            || row.get::<Option<Uuid>, _>("project_id") != b.project_id
            || b.runtime
                .as_deref()
                .is_some_and(|v| v != row.get::<&str, _>("runtime"))
        {
            return Err(conflict());
        }
    } else {
        let runtime = b.runtime.as_deref().unwrap_or("project");
        validate_runtime(app, runtime, b.project_id)?;
        if let Some(project) = b.project_id {
            let project = sqlx::query(
                "SELECT harness_id,root FROM projects WHERE workspace=$1 AND owner_id=$2 AND id=$3",
            )
            .bind(w)
            .bind(u)
            .bind(project)
            .fetch_one(&mut *tx)
            .await?;
            m.harness(project.get("harness_id"), project.get("root"))?;
        }
        sqlx::query("INSERT INTO connections(id,workspace,created_by,kind,destination,project_id,runtime) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(id).bind(w).bind(u).bind(&b.kind).bind(&b.destination).bind(b.project_id).bind(runtime).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT id FROM connections WHERE id=$1 AND workspace=$2 FOR UPDATE")
        .bind(id)
        .bind(w)
        .fetch_one(&mut *tx)
        .await?;
    b.credentials["webhook_secret"] = json!(random());
    let public_origin = origin(app, h)?;
    let webhook = format!("{public_origin}/hooks/{}/{id}", b.kind);
    if b.kind == "slack" {
        let state = random();
        let redirect = format!("{public_origin}/oauth/slack");
        let mut url =
            reqwest::Url::parse("https://slack.com/oauth/v2/authorize").expect("static URL");
        url.query_pairs_mut()
            .append_pair(
                "client_id",
                b.credentials["client_id"].as_str().ok_or_else(bad)?,
            )
            .append_pair(
                "scope",
                "app_mentions:read,im:history,chat:write,channels:read,groups:read",
            )
            .append_pair("redirect_uri", &redirect)
            .append_pair("state", &state);
        sqlx::query("UPDATE connections SET status='pending',enabled=false,state_hash=$2,state_until=now()+interval '10 minutes',oauth_user=$3 WHERE id=$1")
            .bind(id).bind(digest(&state)).bind(u).execute(&mut *tx).await?;
        store_creds(app, h, &mut tx, id, &b.credentials).await?;
        tx.commit().await?;
        return Ok(json!({"id":id,"authorize_url":url.as_str(),"webhook_url":webhook}));
    }
    let identity=provider(app,json!({"kind":b.kind,"operation":"identify","credentials":b.credentials,"destination":b.destination})).await?;
    let info = if b.kind == "telegram" {
        &identity["result"]
    } else {
        &identity
    };
    let external = info["id"].as_i64().ok_or_else(bad)?.to_string();
    let username = info["username"].as_str().ok_or_else(bad)?;
    provider(app,json!({"kind":b.kind,"operation":"inspect","credentials":b.credentials,"destination":b.destination})).await?;
    // Serialize ownership before changing the provider's webhook. Telegram allows only one per bot.
    let binding = if b.kind == "telegram" {
        format!("telegram:{external}")
    } else {
        format!("gitlab:{}", b.destination)
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&binding)
        .execute(&mut *tx)
        .await?;
    let occupied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM connections WHERE kind=$1 AND id<>$2 AND status<>'disconnected' AND external_id IS NOT NULL AND (CASE WHEN kind='telegram' THEN external_id=$3 ELSE destination=$4 END))")
        .bind(&b.kind).bind(id).bind(&external).bind(&b.destination).fetch_one(&mut *tx).await?;
    if occupied {
        return Err(conflict());
    }
    let old: Option<String> = sqlx::query_scalar("SELECT external_id FROM connections WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if old.is_some_and(|v| v != external) {
        return Err(conflict());
    }
    let mut hook_id = None;
    if b.kind == "gitlab" {
        for page in 1..=100 {
            let result=provider(app,json!({"kind":"gitlab","operation":"hooks","page":page.to_string(),"credentials":b.credentials,"destination":b.destination})).await?;
            let hooks = result.as_array().ok_or_else(unavailable)?;
            if let Some(hook) = hooks.iter().find(|v| v["url"] == webhook) {
                hook_id = Some(hook["id"].as_i64().ok_or_else(unavailable)?.to_string());
                break;
            }
            if hooks.len() < 100 {
                break;
            }
            if page == 100 {
                return Err(unavailable());
            }
        }
    }
    // Keep the binding locked while installing and recording the external hook.
    sqlx::query("UPDATE connections SET status='pending',enabled=false,identity=$2,external_id=$3 WHERE id=$1")
        .bind(id).bind(username).bind(&external).execute(&mut *tx).await?;
    store_creds(app, h, &mut tx, id, &b.credentials).await?;
    let revision: i64 = sqlx::query_scalar(
        "UPDATE connections SET revision=revision+1 WHERE id=$1 RETURNING revision",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    // A retry can find the reserved binding and sealed credentials even after an ambiguous remote install.
    let mut tx = app.db.begin().await?;
    sqlx::query(
        "SELECT id FROM connections WHERE id=$1 AND revision=$2 AND status='pending' FOR UPDATE",
    )
    .bind(id)
    .bind(revision)
    .fetch_one(&mut *tx)
    .await?;
    let installed=provider(app,json!({"kind":b.kind,"operation":"install","credentials":b.credentials,"destination":b.destination,"webhook_url":webhook,"hook_id":hook_id})).await?;
    let row=sqlx::query("UPDATE connections SET status='connected',identity=$2,external_id=$3,hook_id=$4,revision=revision+1 WHERE id=$1 RETURNING *")
        .bind(id).bind(username).bind(&external).bind(installed["id"].as_i64().map(|n|n.to_string())).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(summary(&row, &public_origin))
}
#[derive(Deserialize)]
pub(super) struct OAuth {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}
pub(super) async fn callback(
    State(app): State<App>,
    h: HeaderMap,
    Query(q): Query<OAuth>,
) -> Result<Redirect> {
    if let Some(error) = q.error.as_deref() {
        tracing::info!(oauth_error = %error, "Slack OAuth cancelled or denied");
        return Ok(Redirect::to("/connections?error=oauth_cancelled"));
    }
    let state = q.state.as_deref().ok_or_else(bad)?;
    let code = q.code.as_deref().ok_or_else(bad)?;
    let (w, u) = member(&app, &h, false, true).await?;
    let mut tx = app.db.begin().await?;
    let r=sqlx::query("SELECT id,destination,external_id,identity FROM connections WHERE kind='slack' AND workspace=$1 AND oauth_user=$2 AND state_hash=$3 AND state_until>now() FOR UPDATE")
        .bind(w).bind(u).bind(digest(state)).fetch_one(&mut *tx).await?;
    let id: Uuid = r.get("id");
    let mut c = creds(&app, &h, id).await?;
    let public_origin = origin(&app, &h)?;
    let result=provider(&app,json!({"kind":"slack","operation":"exchange","code":code,"credentials":c,"redirect_uri":format!("{public_origin}/oauth/slack")})).await?;
    c["token"] = json!(result["access_token"].as_str().ok_or_else(forbidden)?);
    let team = result["team"]["id"].as_str().ok_or_else(forbidden)?;
    let bot = result["bot_user_id"].as_str().ok_or_else(forbidden)?;
    if r.get::<Option<String>, _>("external_id")
        .is_some_and(|old| old != team || r.get::<String, _>("identity") != bot)
    {
        return Err(conflict());
    }
    let occupied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM connections WHERE id<>$1 AND kind='slack' AND external_id=$2 AND destination=$3 AND status<>'disconnected')")
        .bind(id).bind(team).bind(r.get::<String,_>("destination")).fetch_one(&mut *tx).await?;
    if occupied {
        return Err(conflict());
    }
    provider(&app,json!({"kind":"slack","operation":"inspect","credentials":c,"destination":r.get::<String,_>("destination")})).await?;
    // OAuth state and user binding replace the cross-site browser callback's Origin.
    let mut trusted = h.clone();
    trusted.insert(header::ORIGIN, public_origin.parse().map_err(|_| bad())?);
    store_creds(&app, &trusted, &mut tx, id, &c).await?;
    sqlx::query("UPDATE connections SET state_hash=NULL,state_until=NULL,status='connected',identity=$2,external_id=$3,revision=revision+1 WHERE id=$1")
        .bind(id).bind(bot).bind(team).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Redirect::to("/connections"))
}
async fn disconnect(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let (w, owner) = member(&app, &h, true, true).await?;
    let mut tx = app.db.begin().await?;
    let r=sqlx::query("UPDATE connections SET status='disconnected',enabled=false,state_hash=NULL,revision=revision+1 WHERE id=$1 AND workspace=$2 RETURNING *")
        .bind(id).bind(w).fetch_one(&mut *tx).await?;
    let kind: &str = r.get("kind");
    if let Some(external) = r.get::<Option<String>, _>("external_id") {
        let destination: &str = r.get("destination");
        let binding = if kind == "telegram" {
            format!("telegram:{external}")
        } else {
            format!("{kind}:{destination}")
        };
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(binding)
            .execute(&mut *tx)
            .await?;
        match creds(&app, &h, id).await {
            Ok(c) => {
                if let Err(err) = provider(
                    &app,
                    json!({"kind":kind,"operation":"remove","credentials":c,"destination":destination,"hook_id":r.get::<Option<String>,_>("hook_id")}),
                )
                .await
                {
                    tracing::warn!(connection_id = %id, kind = %kind, status = err.0.as_u16(), "remote webhook removal failed during disconnect; proceeding with local cleanup");
                }
            }
            Err(Error(StatusCode::NOT_FOUND, _)) => {}
            Err(err) => {
                tracing::warn!(connection_id = %id, status = err.0.as_u16(), "could not read credentials during disconnect; proceeding with local cleanup");
            }
        }
    }
    if app.standalone.is_some() {
        sqlx::query("UPDATE connections SET standalone_credentials=NULL WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        remove_grant(&mut tx, id, owner).await?;
    } else {
        private(&app, &h, json!({"action":"remove","connection_id":id})).await?;
    }
    sqlx::query("DELETE FROM external_links WHERE connection_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM link_challenges WHERE connection_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    external_user_id: String,
}
async fn link(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(b): Json<Link>,
) -> Result<Json<Value>> {
    let (w, u) = member(&app, &h, true, false).await?;
    if b.external_user_id.is_empty()
        || b.external_user_id.len() > 64
        || !b
            .external_user_id
            .bytes()
            .all(|v| v.is_ascii_alphanumeric())
    {
        return Err(bad());
    }
    sqlx::query("SELECT id FROM connections WHERE id=$1 AND workspace=$2 AND status='connected'")
        .bind(id)
        .bind(w)
        .fetch_one(&app.db)
        .await?;
    let grant = private(&app, &h, json!({"action":"link","connection_id":id})).await?;
    let code = random();
    sqlx::query("INSERT INTO link_challenges VALUES($1,$2,$3,$4,$5,now()+interval '5 minutes')")
        .bind(digest(&code))
        .bind(id)
        .bind(b.external_user_id)
        .bind(u)
        .bind(serde_json::from_value::<Uuid>(grant["grant"].clone()).map_err(|_| unavailable())?)
        .execute(&app.db)
        .await?;
    Ok(Json(
        json!({"command":format!("@ventic link {code}"),"expires_in":300}),
    ))
}
async fn unlink(State(app): State<App>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let (workspace, user) = member(&app, &h, true, false).await?;
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT id FROM connections WHERE workspace=$1 AND id=$2 FOR UPDATE")
        .bind(workspace)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    for query in [
        "DELETE FROM link_challenges WHERE connection_id=$1 AND user_id=$2",
        "DELETE FROM external_links WHERE connection_id=$1 AND user_id=$2",
    ] {
        sqlx::query(query)
            .bind(id)
            .bind(user)
            .execute(&mut *tx)
            .await?;
    }
    if app.standalone.is_some() {
        remove_grant(&mut tx, id, user).await?;
    } else {
        private(&app, &h, json!({"action":"unlink","connection_id":id})).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}

fn validate_runtime(app: &App, runtime: &str, project: Option<Uuid>) -> Result<()> {
    let valid = match runtime {
        "project" => project.is_some(),
        "computer" | "kitesurf" => app
            .computers
            .as_ref()
            .is_some_and(|c| c.runtimes().contains(&runtime)),
        _ => false,
    };
    if valid { Ok(()) } else { Err(bad()) }
}

const BOT: &str = "SELECT c.*,p.harness_id,p.root FROM connections c LEFT JOIN projects p ON p.id=c.project_id WHERE c.workspace=$1";
fn bot_document(r: &PgRow) -> Value {
    json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("identity"),"handle":"@ventic","harness_id":r.get::<Option<Uuid>,_>("harness_id"),"root":r.get::<Option<String>,_>("root"),"runtime":r.get::<String,_>("runtime"),
        "enabled":r.get::<bool,_>("enabled"),"revision":r.get::<i64,_>("revision"),"channels":[{"kind":r.get::<String,_>("kind"),"name":r.get::<String,_>("destination")}],
        "triggers":r.get::<Value,_>("triggers"),"tools":r.get::<Value,_>("tools"),"instructions":r.get::<String,_>("instructions"),"rate_limit_per_hour":r.get::<i32,_>("rate_limit_per_hour")})
}
async fn bots(State(app): State<App>, h: HeaderMap) -> Result<Json<Value>> {
    let (w, _) = member(&app, &h, false, false).await?;
    let rows = sqlx::query(&format!("{BOT} AND c.status='connected' ORDER BY c.id"))
        .bind(w)
        .fetch_all(&app.db)
        .await?;
    Ok(Json(
        json!({"items":rows.iter().map(bot_document).collect::<Vec<_>>()}),
    ))
}
async fn bot(State(app): State<App>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let (w, _) = member(&app, &h, false, false).await?;
    Ok(Json(bot_document(
        &sqlx::query(&format!("{BOT} AND c.id=$2 AND c.status='connected'"))
            .bind(w)
            .bind(id)
            .fetch_one(&app.db)
            .await?,
    )))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    revision: i64,
    runtime: Option<String>,
    enabled: Option<bool>,
    instructions: Option<String>,
    rate_limit_per_hour: Option<i32>,
    triggers: Option<Value>,
    tools: Option<Value>,
}
async fn edit_bot(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(b): Json<Edit>,
) -> Result<Json<Value>> {
    let (w, _) = member(&app, &h, true, true).await?;
    if b.instructions
        .as_ref()
        .is_some_and(|v| v.chars().count() > 8000)
        || b.rate_limit_per_hour
            .is_some_and(|n| !(1..=1000).contains(&n))
        || b.triggers.as_ref().is_some_and(|v| {
            v.as_object().is_none_or(|o| o.len() != 2)
                || !v["mentions"].is_boolean()
                || !v["direct_messages"].is_boolean()
        })
        || b.tools.as_ref().is_some_and(|v| {
            v.as_object().is_none_or(|o| o.len() != 4)
                || ["read", "write", "execute", "mcp"]
                    .iter()
                    .any(|k| !matches!(v[*k].as_str(), Some("auto" | "review" | "off")))
        })
    {
        return Err(bad());
    }
    if let Some(runtime) = b.runtime.as_deref() {
        let project = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT project_id FROM connections WHERE workspace=$1 AND id=$2",
        )
        .bind(w)
        .bind(id)
        .fetch_one(&app.db)
        .await?;
        validate_runtime(&app, runtime, project)?;
    }
    let n=sqlx::query("UPDATE connections SET enabled=COALESCE($4,enabled),instructions=COALESCE($5,instructions),rate_limit_per_hour=COALESCE($6,rate_limit_per_hour),triggers=COALESCE($7,triggers),tools=COALESCE($8,tools),runtime=COALESCE($9,runtime),revision=revision+1 WHERE id=$1 AND workspace=$2 AND revision=$3 AND status='connected'")
        .bind(id).bind(w).bind(b.revision).bind(b.enabled).bind(b.instructions).bind(b.rate_limit_per_hour).bind(b.triggers).bind(b.tools).bind(b.runtime).execute(&app.db).await?;
    if n.rows_affected() != 1 {
        return Err(conflict());
    }
    bot(State(app), h, Path(id)).await
}
