use super::*;
use crate::connections::{creds, digest, host, member, origin, private, provider};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::postgres::PgRow;
use std::time::Duration;
use subtle::ConstantTimeEq;

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/bots/{id}/mentions", get(list))
        .route("/bots/{id}/mentions/{mention}/review", post(review))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().ok_or_else(bad)
}
fn workspace_headers(app: &App, workspace: Uuid) -> Result<HeaderMap> {
    if let Some(local) = &app.standalone {
        if workspace != local.workspace {
            return Err(forbidden());
        }
        let mut h = HeaderMap::new();
        h.insert(header::HOST, local.host.parse().map_err(|_| bad())?);
        h.insert(header::ORIGIN, local.origin.parse().map_err(|_| bad())?);
        return Ok(h);
    }
    let base = env::var("VENTICODE_HOST").map_err(|_| unavailable())?;
    let portal = reqwest::Url::parse(&env::var("PORTAL_ORIGIN").map_err(|_| unavailable())?)
        .map_err(|_| unavailable())?;
    let host = format!(
        "{workspace}.{base}{}",
        portal.port().map(|n| format!(":{n}")).unwrap_or_default()
    );
    let mut h = HeaderMap::new();
    h.insert(header::HOST, host.parse().map_err(|_| bad())?);
    h.insert(
        header::ORIGIN,
        format!("https://{host}").parse().map_err(|_| bad())?,
    );
    Ok(h)
}
fn verify(kind: &str, h: &HeaderMap, raw: &[u8], c: &Value) -> Result<()> {
    let get = |name: &str| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(forbidden)
    };
    if kind == "slack" {
        let ts = get("x-slack-request-timestamp")?;
        let seconds = ts.parse::<i64>().map_err(|_| forbidden())?;
        if chrono::Utc::now().timestamp().abs_diff(seconds) > 300 {
            return Err(forbidden());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(field(c, "signing_secret")?.as_bytes())
            .map_err(|_| forbidden())?;
        mac.update(b"v0:");
        mac.update(ts.as_bytes());
        mac.update(b":");
        mac.update(raw);
        let tag = get("x-slack-signature")?
            .strip_prefix("v0=")
            .ok_or_else(forbidden)?;
        mac.verify_slice(&hex::decode(tag).map_err(|_| forbidden())?)
            .map_err(|_| forbidden())
    } else {
        let name = if kind == "telegram" {
            "x-telegram-bot-api-secret-token"
        } else {
            "x-gitlab-token"
        };
        if bool::from(
            get(name)?
                .as_bytes()
                .ct_eq(field(c, "webhook_secret")?.as_bytes()),
        ) {
            Ok(())
        } else {
            Err(forbidden())
        }
    }
}
struct Inbound {
    delivery: String,
    sender: String,
    text: String,
    address: Value,
    mentioned: bool,
    dm: bool,
}
fn inbound(kind: &str, v: &Value, r: &PgRow) -> Result<Option<Inbound>> {
    let destination: &str = r.get("destination");
    let identity: &str = r.get("identity");
    let external: &str = r.get("external_id");
    Ok(match kind {
        "slack" => {
            let e = &v["event"];
            if v["team_id"] != external
                || e["bot_id"].is_string()
                || e["user"] == identity
                || !e["subtype"].is_null()
            {
                return Ok(None);
            }
            let dm = e["channel_type"] == "im";
            if !(e["type"] == "app_mention" || dm && e["type"] == "message")
                || (!dm && e["channel"] != destination)
            {
                return Ok(None);
            }
            Some(Inbound {
                delivery: field(v, "event_id")?.into(),
                sender: field(e, "user")?.into(),
                text: field(e, "text")?.into(),
                address: json!({"destination":e["channel"],"thread":e["thread_ts"].as_str().unwrap_or(field(e,"ts")?)}),
                mentioned: e["type"] == "app_mention",
                dm,
            })
        }
        "telegram" => {
            let m = &v["message"];
            if m["from"]["is_bot"] != false || !m["text"].is_string() {
                return Ok(None);
            }
            let dm = m["chat"]["type"] == "private";
            let dest = m["chat"]["id"].as_i64().ok_or_else(bad)?.to_string();
            if !dm && dest != destination {
                return Ok(None);
            }
            let text = field(m, "text")?.to_owned();
            let mentioned = text.split_whitespace().any(|v| {
                v.eq_ignore_ascii_case(&format!("@{identity}"))
                    || v.eq_ignore_ascii_case("@ventic")
                    || v.eq_ignore_ascii_case(&format!("/ventic@{identity}"))
            });
            Some(Inbound {
                delivery: v["update_id"].as_i64().ok_or_else(bad)?.to_string(),
                sender: m["from"]["id"].as_i64().ok_or_else(bad)?.to_string(),
                text,
                address: json!({"destination":dest,"thread":m["message_id"].as_i64().ok_or_else(bad)?.to_string()}),
                mentioned,
                dm,
            })
        }
        "gitlab" => {
            let a = &v["object_attributes"];
            if v["object_kind"] != "note"
                || a["system"] == true
                || a["action"] == "update"
                || v["project"]["id"].as_i64() != destination.parse::<i64>().ok()
                || v["user"]["id"].as_i64() == external.parse::<i64>().ok()
            {
                return Ok(None);
            }
            let (kind, iid) = match a["noteable_type"].as_str() {
                Some("Issue") => ("issues", &v["issue"]["iid"]),
                Some("MergeRequest") => ("merge_requests", &v["merge_request"]["iid"]),
                _ => return Ok(None),
            };
            let text = field(a, "note")?.to_owned();
            let mentioned = text
                .split_whitespace()
                .any(|s| s.trim_end_matches([',', ':']) == "@ventic");
            Some(Inbound {
                delivery: a["id"].as_i64().ok_or_else(bad)?.to_string(),
                sender: v["user"]["id"].as_i64().ok_or_else(bad)?.to_string(),
                text,
                address: json!({"destination":destination,"object_type":kind,"iid":iid.as_i64().ok_or_else(bad)?.to_string(),"thread":a["discussion_id"].as_str().unwrap_or("root")}),
                mentioned,
                dm: false,
            })
        }
        _ => return Err(missing()),
    })
}
async fn reply(app: &App, r: &PgRow, c: &Value, address: &Value, text: &str) -> Result<Value> {
    let mut req = address.clone();
    req["kind"] = json!(r.get::<String, _>("kind"));
    req["operation"] = json!("reply");
    req["credentials"] = c.clone();
    req["text"] = json!(text);
    provider(app, req).await
}
pub(super) async fn hook(
    State(app): State<App>,
    Path((kind, id)): Path<(String, Uuid)>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let r = sqlx::query(
        "SELECT * FROM connections WHERE id=$1 AND kind=$2 AND status IN ('connected','pending')",
    )
    .bind(id)
    .bind(&kind)
    .fetch_one(&app.db)
    .await?;
    let w: Uuid = r.get("workspace");
    let trusted = workspace_headers(&app, w)?;
    if host(&h)? != host(&trusted)? {
        return Err(forbidden());
    }
    let c = creds(&app, &trusted, id).await?;
    verify(&kind, &h, &body, &c)?;
    let v: Value = serde_json::from_slice(&body).map_err(|_| bad())?;
    if kind == "slack" && v["type"] == "url_verification" {
        return Ok(Json(json!({"challenge":field(&v,"challenge")?})));
    }
    if r.get::<String, _>("status") != "connected" {
        return Ok(Json(json!({"ok":true})));
    }
    let Some(ev) = inbound(&kind, &v, &r)? else {
        return Ok(Json(json!({"ok":true})));
    };
    let words: Vec<_> = ev.text.split_whitespace().collect();
    if let [.., "link", code] = words.as_slice()
        && code.len() == 64
    {
        let mut tx = app.db.begin().await?;
        let row=sqlx::query("DELETE FROM link_challenges WHERE code_hash=$1 AND connection_id=$2 AND external_id=$3 AND expires_at>now() RETURNING user_id,control_grant")
            .bind(digest(code)).bind(id).bind(&ev.sender).fetch_optional(&mut *tx).await?;
        if let Some(row) = row {
            let user: Uuid = row.get("user_id");
            sqlx::query("INSERT INTO external_links VALUES($1,$2,$3,$4) ON CONFLICT(connection_id,external_id) DO UPDATE SET control_grant=excluded.control_grant WHERE external_links.user_id=excluded.user_id")
                .bind(id).bind(&ev.sender).bind(user).bind(row.get::<Uuid,_>("control_grant")).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        return Ok(Json(json!({"ok":true})));
    }
    let triggers: Value = r.get("triggers");
    if !r.get::<bool, _>("enabled")
        || !(ev.mentioned && triggers["mentions"] == true
            || ev.dm && triggers["direct_messages"] == true)
    {
        return Ok(Json(json!({"ok":true})));
    }
    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM external_links WHERE connection_id=$1 AND external_id=$2",
    )
    .bind(id)
    .bind(&ev.sender)
    .fetch_optional(&app.db)
    .await?;
    if owner.is_none() {
        let claim=sqlx::query("INSERT INTO link_notices(connection_id,delivery_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(id).bind(&ev.delivery).execute(&app.db).await?;
        if claim.rows_affected() == 1 {
            reply(
                &app,
                &r,
                &c,
                &ev.address,
                &format!(
                    "Collega il tuo account / Link your account: {}/connections",
                    origin(&app, &trusted)?
                ),
            )
            .await?;
        }
        return Ok(Json(json!({"ok":true})));
    }
    let owner = owner.ok_or_else(forbidden)?;
    if ev.text.chars().count() > 100000 {
        return Err(bad());
    }
    let eligible: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT FROM mentions WHERE connection_id=$1 AND delivery_id=$2)
        AND (SELECT count(*) FROM mentions WHERE connection_id=$1 AND created_at>now()-interval '1 hour')<$3")
        .bind(id).bind(&ev.delivery).bind(i64::from(r.get::<i32,_>("rate_limit_per_hour"))).fetch_one(&app.db).await?;
    if !eligible {
        return Ok(Json(json!({"ok":true})));
    }
    let obj =
        standalone_object(&app, w, owner, &json!({"text":ev.text,"author":ev.sender})).await?;
    let mut tx = app.db.begin().await?;
    sqlx::query(
        "SELECT id FROM connections WHERE id=$1 AND enabled AND status='connected' FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM mentions WHERE connection_id=$1 AND created_at>now()-interval '1 hour'").bind(id).fetch_one(&mut *tx).await?;
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM mentions WHERE connection_id=$1 AND delivery_id=$2)",
    )
    .bind(id)
    .bind(&ev.delivery)
    .fetch_one(&mut *tx)
    .await?;
    if !existing && count < i64::from(r.get::<i32, _>("rate_limit_per_hour")) {
        sqlx::query("DELETE FROM object_cleanup WHERE object_id=$1")
            .bind(obj)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mentions(id,connection_id,delivery_id,owner_id,address,body_object,state) VALUES($1,$2,$3,$4,$5,$6,'pending')")
            .bind(Uuid::new_v4()).bind(id).bind(ev.delivery).bind(owner).bind(ev.address).bind(obj).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}
async fn standalone_object(app: &App, w: Uuid, u: Uuid, data: &Value) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let mut tx = app.db.begin().await?;
    sqlx::query("INSERT INTO stored_objects VALUES($1,$2,$3)")
        .bind(id)
        .bind(w)
        .bind(u)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO object_cleanup VALUES($1,now()+interval '1 hour')")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.storage.put(id, data).await?;
    Ok(id)
}
pub(super) async fn bot_context(app: &App, id: Uuid) -> Result<Option<Value>> {
    let r=sqlx::query("SELECT x.from_bot,x.runtime,c.enabled,c.status,c.tools,c.instructions FROM conversations x LEFT JOIN connections c ON c.id=x.bot_connection WHERE x.id=$1")
        .bind(id).fetch_one(&app.db).await?;
    if !r.get::<bool, _>("from_bot") {
        return Ok(None);
    }
    if r.get::<Option<bool>, _>("enabled") != Some(true)
        || r.get::<Option<String>, _>("status").as_deref() != Some("connected")
    {
        return Err(forbidden());
    }
    Ok(Some(
        json!({"tools":r.get::<Value,_>("tools"),"instructions":r.get::<String,_>("instructions"),"runtime":r.get::<String,_>("runtime")}),
    ))
}
const SELECT: &str = "SELECT m.*,c.kind,c.workspace,c.destination,c.status,c.enabled,c.project_id,c.runtime,c.rate_limit_per_hour FROM mentions m JOIN connections c ON c.id=m.connection_id";
async fn document(app: &App, r: &PgRow) -> Result<Value> {
    let content = app.storage.get(r.get("body_object")).await?;
    let reply = if let Some(obj) = r.get::<Option<Uuid>, _>("reply_object") {
        app.storage.get(obj).await?
    } else {
        Value::Null
    };
    let state = match r.get::<&str, _>("state") {
        "pending" | "starting" => "answering",
        v => v,
    };
    Ok(
        json!({"id":r.get::<Uuid,_>("id"),"channel":{"kind":r.get::<String,_>("kind"),"name":r.get::<Value,_>("address")["destination"]},
        "author":content["author"],"text":content["text"],"state":state,"reply":reply.as_str(),"created_at":r.get::<String,_>("created"),"tool_calls":[],
        "conversation_id":r.get::<Option<Uuid>,_>("conversation_id")}),
    )
}
async fn list(State(app): State<App>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let (w, u) = member(&app, &h, false, false).await?;
    let rows=sqlx::query(&format!("SELECT v.*,to_jsonb(v.created_at)#>>'{{}}' AS created FROM ({SELECT}) v WHERE workspace=$1 AND owner_id=$2 AND connection_id=$3 ORDER BY created_at DESC LIMIT 100"))
        .bind(w).bind(u).bind(id).fetch_all(&app.db).await?;
    let mut items = Vec::new();
    for r in rows {
        items.push(document(&app, &r).await?)
    }
    Ok(Json(json!({"items":items})))
}
#[derive(Deserialize)]
struct Review {
    decision: String,
}
async fn review(
    State(app): State<App>,
    h: HeaderMap,
    Path((id, mention)): Path<(Uuid, Uuid)>,
    Json(b): Json<Review>,
) -> Result<Json<Value>> {
    let actor = Member::load(&app, &h, true).await?;
    let (w, u) = (actor.workspace, actor.user);
    let state = match b.decision.as_str() {
        "approve" => "sending",
        "discard" => "discarded",
        _ => return Err(bad()),
    };
    let mut tx = app.db.begin().await?;
    let r=sqlx::query(&format!("{SELECT} WHERE m.id=$1 AND connection_id=$2 AND c.workspace=$3 AND owner_id=$4 FOR UPDATE OF m,c"))
        .bind(mention).bind(id).bind(w).bind(u).fetch_one(&mut *tx).await?;
    if r.get::<String, _>("state") != "review"
        || !r.get::<bool, _>("enabled")
        || r.get::<String, _>("status") != "connected"
    {
        return Err(conflict());
    }
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM external_links WHERE connection_id=$1 AND user_id=$2)",
    )
    .bind(id)
    .bind(u)
    .fetch_one(&mut *tx)
    .await?;
    if !owned {
        return Err(forbidden());
    }
    if let Some(conversation) = r.get::<Option<Uuid>, _>("conversation_id") {
        projects::owned(&app, &actor, conversation).await?;
    }
    let c = creds(&app, &h, id).await?;
    let body = app.storage.get(r.get("reply_object")).await?;
    sqlx::query("UPDATE mentions SET state=$2,lease_until=now()+interval '2 minutes' WHERE id=$1")
        .bind(mention)
        .bind(state)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if state == "sending" {
        let result = reply(
            &app,
            &r,
            &c,
            &r.get("address"),
            body.as_str().ok_or_else(bad)?,
        )
        .await;
        let (next, remote) = match &result {
            Ok(v) => (
                "replied",
                v["ts"].as_str().map(str::to_owned).unwrap_or_else(|| {
                    v["id"]
                        .as_i64()
                        .or(v["result"]["message_id"].as_i64())
                        .unwrap_or_default()
                        .to_string()
                }),
            ),
            Err(_) => ("uncertain", String::new()),
        };
        sqlx::query("UPDATE mentions SET state=$2,remote_id=$3,replied_at=CASE WHEN $2='replied' THEN now() END WHERE id=$1 AND state='sending'").bind(mention).bind(next).bind(remote).execute(&app.db).await?;
        result?;
    }
    let row = sqlx::query(&format!(
        "SELECT v.*,to_jsonb(v.created_at)#>>'{{}}' AS created FROM ({SELECT}) v WHERE id=$1"
    ))
    .bind(mention)
    .fetch_one(&app.db)
    .await?;
    Ok(Json(document(&app, &row).await?))
}

pub(super) async fn worker(app: App) {
    loop {
        let lease = Uuid::new_v4();
        let claimed=sqlx::query_scalar::<_,Uuid>("UPDATE mentions SET lease_id=$1, lease_until=now()+interval '2 minutes' WHERE id=(
            SELECT id FROM mentions WHERE state IN ('pending','answering') AND (lease_until IS NULL OR lease_until<now()) ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id").bind(lease).fetch_optional(&app.db).await;
        if let Ok(Some(id)) = claimed {
            if process(&app, id, lease).await.is_err() {
                let _=sqlx::query("UPDATE mentions SET state='failed',lease_until=NULL WHERE id=$1 AND lease_id=$2 AND state IN ('pending','answering')").bind(id).bind(lease).execute(&app.db).await;
            }
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await
        }
    }
}
async fn process(app: &App, id: Uuid, lease: Uuid) -> Result<()> {
    let r = sqlx::query(&format!(
        "{SELECT} WHERE m.id=$1 AND m.lease_id=$2 AND c.status='connected' AND c.enabled"
    ))
    .bind(id)
    .bind(lease)
    .fetch_one(&app.db)
    .await?;
    let w: Uuid = r.get("workspace");
    let u: Uuid = r.get("owner_id");
    let conn: Uuid = r.get("connection_id");
    if let Some(run) = r.get::<Option<Uuid>, _>("run_id") {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM conversations WHERE id=$1 AND active_run=$2)",
        )
        .bind(r.get::<Option<Uuid>, _>("conversation_id"))
        .bind(run)
        .fetch_one(&app.db)
        .await?;
        if live {
            sqlx::query("UPDATE mentions SET lease_until=now()+interval '5 seconds' WHERE id=$1 AND lease_id=$2")
                .bind(id).bind(lease)
                .execute(&app.db)
                .await?;
            return Ok(());
        }
    }
    let mut h = workspace_headers(app, w)?;
    let content = app.storage.get(r.get("body_object")).await?;
    let grant:Uuid=sqlx::query_scalar("SELECT control_grant FROM external_links WHERE connection_id=$1 AND external_id=$2 AND user_id=$3")
        .bind(conn).bind(field(&content,"author")?).bind(u).fetch_one(&app.db).await?;
    let v = private(
        app,
        &h,
        json!({"action":"delegate","grant":grant,"connection_id":conn}),
    )
    .await?;
    if v["user_id"] != u.to_string() {
        return Err(forbidden());
    }
    h.insert(
        header::COOKIE,
        format!("venticode_session={}", field(&v, "token")?)
            .parse()
            .map_err(|_| bad())?,
    );
    let member = Member::load(app, &h, true).await?;
    let mut tx = app.db.begin().await?;
    let mapped: Option<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM mentions WHERE id=$1 AND lease_id=$2 FOR UPDATE",
    )
    .bind(id)
    .bind(lease)
    .fetch_one(&mut *tx)
    .await?;
    let conversation = if let Some(x) = mapped {
        x
    } else {
        let runtime: String =
            sqlx::query_scalar("SELECT runtime FROM connections WHERE id=$1 FOR SHARE")
                .bind(conn)
                .fetch_one(&mut *tx)
                .await?;
        let models = app.control_json(&h, "models").await?;
        let model = models["items"]
            .as_array()
            .and_then(|m| m.iter().find(|v| v["default"] == true).or(m.first()))
            .and_then(|m| m["id"].as_str())
            .ok_or_else(bad)?;
        let personal = match runtime.as_str() {
            "kitesurf" => {
                if app.computers.as_ref().is_none_or(|c| c.kitesurf.is_none()) {
                    return Err(unavailable());
                }
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                    .bind(format!("{w}:{u}"))
                    .execute(&mut *tx)
                    .await?;
                if sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT FROM erased_subjects WHERE workspace=$1 AND owner_id=$2)",
                )
                .bind(w)
                .bind(u)
                .fetch_one(&mut *tx)
                .await?
                {
                    return Err(forbidden());
                }
                None
            }
            "project" | "computer" => {
                let (harness, root, path, name) = if runtime == "computer" {
                    let computers = app.computers.as_ref().ok_or_else(unavailable)?;
                    let harness: Uuid = sqlx::query_scalar("INSERT INTO managed_computers(workspace,owner_id,id) VALUES($1,$2,$3)
                        ON CONFLICT(workspace,owner_id) DO UPDATE SET touched_at=now() RETURNING id")
                        .bind(w).bind(u).bind(Uuid::new_v4()).fetch_one(&mut *tx).await?;
                    if !computers.runtimes().contains(&"computer") {
                        return Err(unavailable());
                    }
                    (
                        harness,
                        "work".to_owned(),
                        ".".to_owned(),
                        "Ubuntu".to_owned(),
                    )
                } else {
                    let project =
                        sqlx::query("SELECT * FROM projects WHERE id=$1 AND workspace=$2")
                            .bind(r.get::<Option<Uuid>, _>("project_id"))
                            .bind(w)
                            .fetch_one(&mut *tx)
                            .await?;
                    member.harness(project.get("harness_id"), project.get("root"))?;
                    (
                        project.get("harness_id"),
                        project.get("root"),
                        project.get("path"),
                        project.get("name"),
                    )
                };
                Some(sqlx::query_scalar::<_,Uuid>("INSERT INTO projects(id,workspace,owner_id,harness_id,root,path,name) VALUES($1,$2,$3,$4,$5,$6,$7)
                    ON CONFLICT(workspace,owner_id,harness_id,root,path) DO UPDATE SET id=projects.id RETURNING id")
                    .bind(Uuid::new_v4()).bind(w).bind(u).bind(harness).bind(root).bind(path).bind(name).fetch_one(&mut *tx).await?)
            }
            _ => return Err(bad()),
        };
        let x = Uuid::new_v4();
        sqlx::query("INSERT INTO conversations(id,project_id,title,model,permission_mode,from_bot,bot_connection,runtime,browser_workspace,browser_owner)
            VALUES($1,$2,'@ventic',$3,'ask',true,$4,$5,$6,$7)")
            .bind(x).bind(personal).bind(model).bind(conn).bind(runtime)
            .bind(personal.is_none().then_some(w)).bind(personal.is_none().then_some(u)).execute(&mut *tx).await?;
        sqlx::query("UPDATE mentions SET conversation_id=$2 WHERE id=$1")
            .bind(id)
            .bind(x)
            .execute(&mut *tx)
            .await?;
        x
    };
    tx.commit().await?;
    if r.get::<Option<Uuid>, _>("run_id").is_none() {
        let (_, Json(started)) = runs::send(
            State(app.clone()),
            h,
            Path(conversation),
            Json(runs::Message {
                id,
                intent: Default::default(),
                reference_id: None,
                content: field(&content, "text")?.to_owned(),
            }),
        )
        .await?;
        sqlx::query("UPDATE mentions SET state='answering',run_id=$2,lease_until=now()+interval '2 seconds' WHERE id=$1 AND lease_id=$3")
            .bind(id).bind(serde_json::from_value::<Uuid>(started["run_id"].clone()).map_err(|_|bad())?).bind(lease).execute(&app.db).await?;
        return Ok(());
    }
    projects::owned(app, &member, conversation).await?;
    let run: Uuid = r.get("run_id");
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM messages WHERE run_id=$1 AND conversation_id=$2")
            .bind(run)
            .bind(conversation)
            .fetch_one(&app.db)
            .await?;
    if outcome != "completed" {
        return Err(unavailable());
    }
    let mut after = 0;
    let mut text = String::new();
    loop {
        let batch = runs::read_events(app, conversation, after, 128).await?;
        if batch.is_empty() {
            break;
        }
        for event in batch {
            after = event["sequence"].as_i64().ok_or_else(bad)?;
            if event["type"] == "text" && event["run_id"] == run.to_string() {
                text.push_str(event["delta"].as_str().unwrap_or(""))
            }
        }
    }
    if text.trim().is_empty() {
        return Err(bad());
    }
    let obj = standalone_object(app, w, u, &json!(text)).await?;
    let mut tx = app.db.begin().await?;
    let n=sqlx::query("UPDATE mentions SET state='review',reply_object=$2,lease_until=NULL WHERE id=$1 AND state='answering' AND lease_id=$3").bind(id).bind(obj).bind(lease).execute(&mut *tx).await?;
    if n.rows_affected() == 1 {
        sqlx::query("DELETE FROM object_cleanup WHERE object_id=$1")
            .bind(obj)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
