use super::*;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
#[derive(Deserialize)]
pub(super) struct Subject {
    workspace: Uuid,
    user: Uuid,
}
pub(super) async fn handle(
    State(app): State<App>,
    Path(action): Path<String>,
    h: HeaderMap,
    Json(s): Json<Subject>,
) -> Result<Json<Value>> {
    let token = h
        .get("x-service-token")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() >= 32)
        .ok_or_else(forbidden)?;
    if !bool::from(
        app.ventic
            .as_ref()
            .ok_or_else(forbidden)?
            .control_hash
            .as_slice()
            .ct_eq(&Sha256::digest(token.as_bytes())),
    ) {
        return Err(forbidden());
    }
    let mut tx = app.db.begin().await?;
    match action.as_str() {
        "export" => {
            use futures_util::{StreamExt, TryStreamExt};
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
            let projects: Vec<Value> = sqlx::query_scalar(
                "SELECT document FROM project_json WHERE workspace=$1 AND owner_id=$2 ORDER BY id",
            )
            .bind(s.workspace)
            .bind(s.user)
            .fetch_all(&mut *tx)
            .await?;
            let conversations:Vec<Value>=sqlx::query_scalar("SELECT document FROM conversation_json WHERE workspace=$1 AND owner_id=$2 ORDER BY id")
                .bind(s.workspace).bind(s.user).fetch_all(&mut *tx).await?;
            let event_rows: Vec<(Uuid, i64, Uuid)> = sqlx::query(
                "SELECT e.conversation_id,e.sequence,e.object_id FROM events e JOIN conversation_json c ON c.id=e.conversation_id WHERE workspace=$1 AND owner_id=$2 ORDER BY c.id,sequence",
            )
            .bind(s.workspace)
            .bind(s.user)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|r| (r.get("conversation_id"), r.get("sequence"), r.get("object_id")))
            .collect();
            let links:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('connection_id',c.id,'kind',c.kind,'external_user_id',l.external_id) FROM external_links l JOIN connections c ON c.id=l.connection_id WHERE c.workspace=$1 AND l.user_id=$2")
                .bind(s.workspace).bind(s.user).fetch_all(&mut *tx).await?;
            let connections:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'kind',kind,'destination',destination,'project_id',project_id,'status',status,'instructions',instructions,'triggers',triggers,'tools',tools) FROM connections WHERE workspace=$1 AND created_by=$2")
                .bind(s.workspace).bind(s.user).fetch_all(&mut *tx).await?;
            let mention_rows: Vec<(Uuid, Uuid, String, Uuid, Option<Uuid>, Value)> = sqlx::query(
                "SELECT m.id,m.connection_id,m.state,m.body_object,m.reply_object,m.address FROM mentions m JOIN connections c ON c.id=m.connection_id WHERE c.workspace=$1 AND m.owner_id=$2 ORDER BY m.created_at,m.id",
            )
            .bind(s.workspace)
            .bind(s.user)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.get("id"),
                    r.get("connection_id"),
                    r.get("state"),
                    r.get("body_object"),
                    r.get("reply_object"),
                    r.get("address"),
                )
            })
            .collect();
            // Release the REPEATABLE READ DB snapshot before fetching objects from S3.
            tx.commit().await?;
            let events: Vec<Value> = futures_util::stream::iter(event_rows)
                .map(|(conversation_id, sequence, object_id)| {
                    let storage = app.storage.clone();
                    async move {
                        let mut event = storage.get(object_id).await?;
                        event["conversation_id"] = json!(conversation_id);
                        event["sequence"] = json!(sequence);
                        Ok::<_, Error>(event)
                    }
                })
                .buffered(8)
                .try_collect()
                .await?;
            let mentions: Vec<Value> = futures_util::stream::iter(mention_rows)
                .map(
                    |(id, connection_id, state, body_object, reply_object, address)| {
                        let storage = app.storage.clone();
                        async move {
                            let content = storage.get(body_object).await?;
                            let reply = match reply_object {
                                Some(reply_id) => storage.get(reply_id).await?,
                                None => Value::Null,
                            };
                            Ok::<_, Error>(json!({
                                "id": id,
                                "connection_id": connection_id,
                                "state": state,
                                "content": content,
                                "reply": reply,
                                "address": address
                            }))
                        }
                    },
                )
                .buffered(8)
                .try_collect()
                .await?;
            Ok(Json(
                json!({"projects":projects,"conversations":conversations,"events":events,"external_links":links,"connections":connections,"mentions":mentions}),
            ))
        }
        "erase" => {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                .bind(format!("{}:{}", s.workspace, s.user))
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO erased_subjects VALUES($1,$2) ON CONFLICT DO NOTHING")
                .bind(s.workspace)
                .bind(s.user)
                .execute(&mut *tx)
                .await?;
            for statement in [
                "DELETE FROM mentions m USING connections c WHERE m.connection_id=c.id AND c.workspace=$1 AND m.owner_id=$2",
                "DELETE FROM external_links l USING connections c WHERE l.connection_id=c.id AND c.workspace=$1 AND (l.user_id=$2 OR c.created_by=$2)",
                "DELETE FROM link_challenges l USING connections c WHERE l.connection_id=c.id AND c.workspace=$1 AND (l.user_id=$2 OR c.created_by=$2)",
                "UPDATE connections SET status='disconnected',enabled=false,created_by=NULL,state_hash=NULL,oauth_user=NULL,instructions='',revision=revision+1 WHERE workspace=$1 AND created_by=$2",
            ] {
                sqlx::query(statement)
                    .bind(s.workspace)
                    .bind(s.user)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("DELETE FROM projects WHERE workspace=$1 AND owner_id=$2")
                .bind(s.workspace)
                .bind(s.user)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM conversations WHERE browser_workspace=$1 AND browser_owner=$2",
            )
            .bind(s.workspace)
            .bind(s.user)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE object_cleanup SET due_at=now() WHERE object_id IN (SELECT object_id FROM stored_objects WHERE workspace=$1 AND owner_id=$2)",
            )
            .bind(s.workspace)
            .bind(s.user)
            .execute(&mut *tx)
            .await?;
            let pending: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT FROM stored_objects WHERE workspace=$1 AND owner_id=$2)",
            )
            .bind(s.workspace)
            .bind(s.user)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            let computer_deleted = if let Some(computers) = &app.computers {
                computers.erase(&app, s.workspace, s.user).await?
            } else {
                !sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT FROM managed_computers WHERE workspace=$1 AND owner_id=$2)")
                    .bind(s.workspace).bind(s.user).fetch_one(&app.db).await?
            };
            Ok(Json(json!({"deleted":!pending && computer_deleted})))
        }
        _ => Err(missing()),
    }
}
