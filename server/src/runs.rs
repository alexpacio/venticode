use super::*;
use axum::response::sse::{Event, Sse};
use futures_util::{StreamExt, TryStreamExt};
use std::{convert::Infallible, time::Duration};

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/conversations/{id}/messages", post(send))
        .route(
            "/conversations/{id}/goal/{goal}",
            delete(crate::agent::cancel_goal),
        )
        .route("/conversations/{id}/interrupt", post(interrupt))
        .route("/conversations/{id}/events", get(events))
        .route("/conversations/{id}/stream", get(stream))
}
type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Message {
    pub id: Uuid,
    pub content: String,
    #[serde(default)]
    pub intent: Intent,
    pub reference_id: Option<Uuid>,
}
#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Intent {
    #[default]
    Message,
    Goal,
    Plan,
    Answer,
    ImplementPlan,
}
#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: i64,
}

pub(super) async fn stage(app: &App, id: Uuid, mut event: Value) -> Result<Uuid> {
    let object = Uuid::new_v4();
    event["created_at"] = json!(chrono::Utc::now().to_rfc3339());
    let mut tx = app.db.begin().await?;
    let n=sqlx::query("INSERT INTO stored_objects SELECT $2,workspace,owner_id FROM conversation_json WHERE id=$1")
        .bind(id).bind(object).execute(&mut *tx).await?;
    if n.rows_affected() != 1 {
        return Err(missing());
    }
    sqlx::query("INSERT INTO object_cleanup VALUES($1,now()+interval '1 hour')")
        .bind(object)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.storage.put(object, &event).await?;
    Ok(object)
}
// Caller holds the conversation row lock. Commit the event reference with its state transition.
pub(super) async fn append(tx: &mut Tx<'_>, id: Uuid, object: Uuid) -> Result<()> {
    let n = sqlx::query("DELETE FROM object_cleanup WHERE object_id=$1")
        .bind(object)
        .execute(&mut **tx)
        .await?;
    if n.rows_affected() != 1 {
        return Err(conflict());
    }
    sqlx::query("INSERT INTO events SELECT $1,COALESCE(MAX(sequence),0)+1,$2 FROM events WHERE conversation_id=$1")
        .bind(id).bind(object).execute(&mut **tx).await?;
    Ok(())
}
pub(super) async fn record(app: &App, id: Uuid, run: Uuid, event: Value) -> Result<()> {
    let object = stage(app, id, event).await?;
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE id=$1 AND active_run=$2 FOR UPDATE")
        .bind(id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
    append(&mut tx, id, object).await?;
    tx.commit().await?;
    Ok(())
}
pub(super) async fn read_events(app: &App, id: Uuid, after: i64, limit: i64) -> Result<Vec<Value>> {
    let rows=sqlx::query("SELECT sequence,object_id FROM events WHERE conversation_id=$1 AND sequence>$2 ORDER BY sequence LIMIT $3")
        .bind(id).bind(after).bind(limit).fetch_all(&app.db).await?;
    futures_util::stream::iter(rows)
        .map(|row| async move {
            let mut event = app.storage.get(row.get("object_id")).await?;
            // Exports read the complete encrypted object. Bound patches sent during replay.
            if let Some(patch) = event.pointer_mut("/diff/patch")
                && let Some(full) = patch.as_str()
                && full.len() > 65536
            {
                *patch = json!(&full[..full.floor_char_boundary(65536)]);
            }
            event["sequence"] = json!(row.get::<i64, _>("sequence"));
            Ok(event)
        })
        .buffered(8)
        .try_collect()
        .await
}
async fn events(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<Cursor>,
) -> Result<Json<Value>> {
    if q.after < 0 {
        return Err(bad());
    }
    let m = Member::load(&app, &h, false).await?;
    let row = projects::owned(&app, &m, id).await?;
    let items = read_events(&app, id, q.after, 1000).await?;
    Ok(Json(
        json!({"cursor":items.last().and_then(|v|v["sequence"].as_i64()).unwrap_or(q.after),"items":items,"state":row.get::<String,_>("state")}),
    ))
}
async fn stream(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<Cursor>,
) -> Result<Response> {
    let last = h
        .get("last-event-id")
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|n| *n >= 0)
                .ok_or_else(bad)
        })
        .transpose()?
        .unwrap_or(0);
    if q.after < 0 {
        return Err(bad());
    }
    let mut cursor = q.after.max(last);
    let m = Member::load(&app, &h, false).await?;
    projects::owned(&app, &m, id).await?;
    let feed = async_stream::stream! {
        let mut heartbeat=tokio::time::Instant::now();
        loop {
            match Member::load(&app,&h,false).await {
                Ok(m) if projects::owned(&app,&m,id).await.is_ok()=>{},
                _=>{yield Ok::<_,Infallible>(Event::default().event("authorization_expired").data("{}"));break;}
            }
            let Ok(items)=read_events(&app,id,cursor,64).await else{break;};
            let full=items.len()==64;
            for event in items {
                cursor=event["sequence"].as_i64().expect("stored sequence");
                yield Ok(Event::default().event("transcript").id(cursor.to_string()).data(event.to_string()));
            }
            if heartbeat.elapsed()>=Duration::from_secs(15){yield Ok(Event::default().event("heartbeat").data("{}"));heartbeat=tokio::time::Instant::now();}
            if !full {tokio::time::sleep(Duration::from_millis(750)).await;}
        }
    };
    Ok(([("x-accel-buffering", "no")], Sse::new(feed)).into_response())
}

pub(super) async fn send(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<Message>,
) -> Result<(StatusCode, Json<Value>)> {
    let m = Member::load(&app, &h, true).await?;
    let row = projects::owned(&app, &m, id).await?;
    if body.content.trim().is_empty() || body.content.chars().count() > 100000 {
        return Err(bad());
    }
    if matches!(body.intent, Intent::Answer | Intent::ImplementPlan) != body.reference_id.is_some()
    {
        return Err(bad());
    }
    let fingerprint = if body.intent == Intent::Message {
        app.storage.fingerprint(&body.content)
    } else {
        format!(
            "agent-{}",
            app.storage
                .fingerprint(&json!([body.intent, body.reference_id, body.content]).to_string())
        )
    };
    let run = Uuid::new_v4();
    let existing = |row: sqlx::postgres::PgRow| {
        if row.get::<Uuid, _>("conversation_id") != id
            || row.get::<String, _>("fingerprint") != fingerprint
        {
            Err(conflict())
        } else {
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({"run_id":row.get::<Uuid,_>("run_id"),"message_id":body.id})),
            ))
        }
    };
    const REPLAY: &str = "SELECT run_id,conversation_id,fingerprint FROM messages WHERE workspace=$1 AND owner_id=$2 AND id=$3";
    if let Some(old) = sqlx::query(REPLAY)
        .bind(m.workspace)
        .bind(m.user)
        .bind(body.id)
        .fetch_optional(&app.db)
        .await?
    {
        return existing(old);
    }
    // Reject stale controls before staging encrypted content. The UPDATE repeats the
    // checks under the row lock for concurrent senders.
    let state = row.get::<Value, _>("document");
    let reference = json!(body.reference_id);
    if (state["question_id"].is_string() && body.intent != Intent::Answer)
        || (body.intent == Intent::Answer && state["question_id"] != reference)
        || (body.intent == Intent::ImplementPlan
            && (state["plan_id"] != reference || state["permission_mode"] != "plan"))
        || (body.intent == Intent::Goal && state["goal_id"].is_string())
    {
        return Err(conflict());
    }
    if body.intent == Intent::Goal {
        let model = row.get::<&str, _>("model");
        let catalog = app.control_json(&h, "models").await?;
        if !catalog["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|v| v["id"] == model && v["tools"] == true))
        {
            return Err(Error(StatusCode::UNPROCESSABLE_ENTITY,"Scegli un modello con strumenti per avviare un obiettivo / Choose a tool-capable model to start a goal".into()));
        }
    }
    let harness = m.harness(row.get("harness_id"), row.get("root"))?;
    if harness["online"] != true {
        return Err(Error(StatusCode::LOCKED, "Harness offline".into()));
    }
    let mut message = json!({"type":"user_message","message_id":body.id,"content":body.content,"intent":body.intent});
    if let Some(reference) = body.reference_id {
        message["reference_id"] = json!(reference);
    }
    let text = stage(&app, id, message).await?;
    let started = stage(
        &app,
        id,
        json!({"type":"run","run_id":run,"state":"running"}),
    )
    .await?;
    let mut tx = app.db.begin().await?;
    let inserted=sqlx::query("INSERT INTO messages(workspace,owner_id,id,conversation_id,fingerprint,run_id) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
        .bind(m.workspace).bind(m.user).bind(body.id).bind(id).bind(&fingerprint).bind(run).execute(&mut *tx).await?;
    if inserted.rows_affected() == 0 {
        return existing(
            sqlx::query(REPLAY)
                .bind(m.workspace)
                .bind(m.user)
                .bind(body.id)
                .fetch_one(&mut *tx)
                .await?,
        );
    }
    let updated=sqlx::query("UPDATE conversations SET state='running',active_run=$2,lease_until=now()+interval '90 seconds',updated_at=now(),
        permission_mode=CASE $3 WHEN 'plan' THEN 'plan' WHEN 'implement_plan' THEN 'ask' ELSE permission_mode END,
        question_id=NULL, plan_id=NULL, goal_id=CASE WHEN $3='goal' THEN $5 ELSE goal_id END,revision=revision+1
        WHERE id=$1 AND active_run IS NULL AND (question_id IS NULL OR $3='answer')
          AND ($3!='answer' OR question_id=$4) AND ($3!='implement_plan' OR (permission_mode='plan' AND plan_id=$4))
          AND ($3!='goal' OR goal_id IS NULL) RETURNING model")
        .bind(id).bind(run).bind(serde_json::to_value(body.intent).map_err(|_|bad())?.as_str()).bind(body.reference_id).bind(body.id)
        .fetch_optional(&mut *tx).await?.ok_or_else(conflict)?;
    append(&mut tx, id, text).await?;
    append(&mut tx, id, started).await?;
    tx.commit().await?;
    tokio::spawn(work(app, h, id, run, updated.get("model")));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"run_id":run,"message_id":body.id})),
    ))
}
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Completion {
    Answered,
    Interrupted,
    Failed,
    Expired,
}
pub(super) async fn finish(app: &App, id: Uuid, run: Uuid, reason: Completion) -> Result<()> {
    let (state, outcome) = match reason {
        Completion::Answered => ("idle", "completed"),
        Completion::Interrupted => ("idle", "interrupted"),
        Completion::Failed | Completion::Expired => ("failed", "failed"),
    };
    let object = stage(app, id, json!({"type":"run","run_id":run,"state":state})).await?;
    let mut tx = app.db.begin().await?;
    let updated=sqlx::query("UPDATE conversations SET state=$3,active_run=NULL,lease_until=NULL,updated_at=now() WHERE id=$1 AND active_run=$2 AND (NOT $4 OR lease_until<now())")
        .bind(id).bind(run).bind(state).bind(reason == Completion::Expired).execute(&mut *tx).await?;
    if updated.rows_affected() == 0 {
        return Err(conflict());
    }
    sqlx::query("DELETE FROM interactive_calls WHERE run_id=$1")
        .bind(run)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE messages SET outcome=$2 WHERE run_id=$1")
        .bind(run)
        .bind(outcome)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE approvals SET state='expired' WHERE run_id=$1 AND state='pending'")
        .bind(run)
        .execute(&mut *tx)
        .await?;
    append(&mut tx, id, object).await?;
    tx.commit().await?;
    Ok(())
}
async fn interrupt(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, true).await?;
    let row = projects::owned(&app, &m, id).await?;
    if let Some(run) = row.get::<Option<Uuid>, _>("active_run") {
        finish(&app, id, run, Completion::Interrupted).await?;
    }
    Ok(Json(json!({"state":"idle"})))
}

async fn work(app: App, h: HeaderMap, id: Uuid, run: Uuid, model: String) {
    // Keep polling inference even when lease renewal waits for a row held by its recorder.
    let keep_alive = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let member = Member::load(&app, &h, false).await?;
            projects::owned(&app, &member, id).await?;
            crate::mentions::bot_context(&app, id).await?;
            let renewed=sqlx::query("UPDATE conversations SET lease_until=now()+interval '90 seconds' WHERE id=$1 AND active_run=$2 AND lease_until>now()")
                .bind(id).bind(run).execute(&app.db).await?;
            if renewed.rows_affected() != 1 {
                return Err(conflict());
            }
        }
    };
    let outcome = tokio::select! {
        result=generate(&app,&h,id,run,model)=>result,
        lost=keep_alive=>lost,
    };
    let reason = if outcome.is_ok() {
        Completion::Answered
    } else {
        Completion::Failed
    };
    if let Err(e) = outcome {
        let _ = record(&app, id, run, json!({"type":"error","message":e.1})).await;
    }
    let _ = finish(&app, id, run, reason).await;
}

fn system_message(mode: &str, use_tools: bool, bot: Option<&Value>) -> Value {
    let mut prompt = String::from(include_str!("conversation-prompt.txt").trim_end());
    let guidance = match (use_tools, mode) {
        (false, _) => {
            "This model has no tools. Work from the conversation. If you need project contents or test output, ask the user for the specific missing input."
        }
        (true, "plan") => {
            "Plan mode is active. Use the available evidence to propose concrete steps and verification. Leave project changes for an implementation turn."
        }
        (true, _) => {
            "Use the declared tools when the request needs project work. Inspect the relevant files first, preserve existing changes, and verify the requested behavior with a focused check when available."
        }
    };
    prompt.push_str("\n\n");
    prompt.push_str(guidance);
    if let Some(extra) = bot
        .and_then(|b| b["instructions"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        prompt.push_str("\n\nInstructions configured for this bot connection:\n");
        prompt.push_str(extra);
    }
    json!({"role":"system","content":prompt})
}

async fn generate(app: &App, h: &HeaderMap, id: Uuid, run: Uuid, model: String) -> Result<()> {
    let member = Member::load(app, h, false).await?;
    let conversation = projects::owned(app, &member, id).await?;
    let mode = conversation.get::<&str, _>("permission_mode");
    let catalog = app.control_json(h, "models").await?;
    let selected = catalog["items"]
        .as_array()
        .and_then(|items| items.iter().find(|v| v["id"] == model))
        .ok_or_else(forbidden)?;
    let use_tools = selected["tools"] == true;
    let mut history = crate::history::History::default();
    let mut cursor = 0;
    loop {
        let batch = read_events(app, id, cursor, 128).await?;
        if batch.is_empty() {
            break;
        }
        for e in batch {
            cursor = e["sequence"].as_i64().ok_or_else(unavailable)?;
            history.push(e, use_tools);
        }
    }
    let goal = conversation.get::<Value, _>("document")["goal_id"].is_string();
    let mut messages = history.into_messages();
    let mut repairs = 0;
    // Reserve the first message for the current connection instructions on each model call.
    messages.insert(0, Value::Null);
    for _ in 0..24 {
        let mut calls = std::collections::BTreeMap::<u64, (String, String, String)>::new();
        let mut answer = String::new();
        let bot = crate::mentions::bot_context(app, id).await?;
        messages[0] = system_message(mode, use_tools, bot.as_ref());
        if goal && bot.is_none() && use_tools && mode != "plan" {
            let instruction = messages[0]["content"].as_str().unwrap_or_default();
            messages[0]["content"] = json!(format!(
                "{instruction}\nAn explicit user goal is active. Continue until you verify its acceptance criteria, then call complete_goal. If blocked by a missing decision, call ask_question."
            ));
        }
        let mut request = json!({"model":model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
        if use_tools {
            request["tools"] = tools::definitions(mode, bot.as_ref(), app.standalone.is_some());
            if bot.is_none() {
                tools::file_functions(&mut request["tools"], mode);
                crate::agent::actions(&mut request["tools"], mode, goal);
            }
            request["parallel_tool_calls"] = json!(false);
        }
        let mut stream = inference::infer(app, h, &request).await?;
        let msg = Uuid::new_v4();
        let mut done = false;
        let mut truncated = false;
        while let Some(event) = stream.next().await {
            let data = event?;
            if data == "[DONE]" {
                done = true;
                break;
            }
            let v: Value = serde_json::from_str(&data).map_err(|_| unavailable())?;
            if v.get("error").is_some() {
                return Err(unavailable());
            }
            truncated |= v["choices"][0]["finish_reason"] == "length";
            if let (Some(input), Some(output)) = (
                v["usage"]["prompt_tokens"].as_u64(),
                v["usage"]["completion_tokens"].as_u64(),
            ) {
                record(app,id,run,json!({"type":"run","run_id":run,"state":"running","usage":{"input_tokens":input,"output_tokens":output}})).await?;
            }
            if let Some(parts) = v["choices"][0]["delta"]["tool_calls"].as_array() {
                for part in parts {
                    let index = part["index"].as_u64().filter(|n| *n < 16).ok_or_else(bad)?;
                    let call = calls.entry(index).or_default();
                    for (field, target) in [
                        (&part["id"], &mut call.0),
                        (&part["function"]["name"], &mut call.1),
                        (&part["function"]["arguments"], &mut call.2),
                    ] {
                        if let Some(piece) = field.as_str() {
                            if target.len() + piece.len() > 128 * 1024 {
                                return Err(bad());
                            }
                            target.push_str(piece);
                        }
                    }
                }
            }
            if let Some(delta) = v["choices"][0]["delta"]["content"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                answer.push_str(delta);
                record(
                    app,
                    id,
                    run,
                    json!({"type":"text","run_id":run,"message_id":msg,"delta":delta}),
                )
                .await?;
            }
        }
        if !done {
            return Err(unavailable());
        }
        if calls.is_empty() && !truncated {
            if bot.is_none() && mode == "plan" && !answer.trim().is_empty() {
                return crate::agent::checkpoint(app, id, run, "plan", &json!({"content":answer}))
                    .await;
            }
            if goal && use_tools && bot.is_none() {
                messages.push(json!({"role":"assistant","content":answer}));
                messages.push(json!({"role":"user","content":"Continue the active goal. Verify its result and call complete_goal, or use ask_question if user input is needed."}));
                continue;
            }
            return Ok(());
        }
        if truncated
            || calls.values().any(|(key, name, args)| {
                key.is_empty()
                    || name.is_empty()
                    || serde_json::from_str::<Value>(args).map_or(true, |v| !v.is_object())
            })
        {
            if !calls.is_empty() && repairs < 2 {
                repairs += 1;
                messages.push(json!({"role":"user","content":"The previous function request was incomplete or invalid. No operation in that batch was executed. Send one smaller call with complete JSON arguments."}));
                continue;
            }
            return Err(Error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "La risposta del modello era incompleta. Riprova con una richiesta più piccola / The model response was incomplete. Try a smaller request".into(),
            ));
        }
        if !use_tools {
            return Err(forbidden());
        }
        let checkpoint = calls.values().find_map(|(_, name, args)| {
            let kind = match name.as_str() {
                "ask_question" => "question",
                "propose_plan" => "plan",
                "complete_goal" => "goal_completed",
                _ => return None,
            };
            Some((kind, args))
        });
        if let Some((kind, args)) = checkpoint {
            if bot.is_some() || calls.len() != 1 {
                return Err(bad());
            }
            return crate::agent::checkpoint(
                app,
                id,
                run,
                kind,
                &serde_json::from_str(args).map_err(|_| bad())?,
            )
            .await;
        }
        let requested:Vec<_>=calls.values().map(|(key,name,args)|json!({"id":key,"type":"function","function":{"name":name,"arguments":args}})).collect();
        messages.push(json!({"role":"assistant","content":answer,"tool_calls":requested}));
        for (_, (key, name, args)) in calls {
            let result = tools::call(
                app,
                h,
                id,
                run,
                &name,
                serde_json::from_str(&args).map_err(|_| bad())?,
            )
            .await?;
            messages.push(json!({"role":"tool","tool_call_id":key,"content":result}));
        }
    }
    Err(Error(
        StatusCode::CONFLICT,
        "Limite strumenti raggiunto / Tool round limit reached".into(),
    ))
}
