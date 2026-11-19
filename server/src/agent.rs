use super::*;
use crate::runs::{append, stage};

pub(super) fn actions(tools: &mut Value, mode: &str, goal: bool) {
    let tools = tools.as_array_mut().expect("function declarations");
    let mut action = |name: &str, description: &str, options: bool| {
        let mut properties = json!({"content":{"type":"string","minLength":1,"maxLength":8000}});
        if options {
            properties["options"] =
                json!({"type":"array","maxItems":6,"items":{"type":"string","maxLength":160}});
        }
        tools.push(json!({"type":"function","function":{"name":name,"description":description,
            "parameters":{"type":"object","additionalProperties":false,"required":["content"],"properties":properties}}}));
    };
    action(
        "ask_question",
        "Pause for a user decision that affects the implementation. Ask one specific question, optionally offering choices. The user can also answer freely.",
        true,
    );
    if mode == "plan" {
        action(
            "propose_plan",
            "Present the plan for user approval. Include concrete implementation steps and checks. This ends the turn.",
            false,
        );
    } else if goal {
        action(
            "complete_goal",
            "Finish the active goal only after verifying its acceptance criteria. Summarize actual evidence and checks.",
            false,
        );
    }
}

pub(super) async fn checkpoint(
    app: &App,
    id: Uuid,
    run: Uuid,
    kind: &str,
    args: &Value,
) -> Result<()> {
    let content = args["content"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 8000)
        .ok_or_else(bad)?;
    if args.as_object().is_none_or(|a| {
        a.keys()
            .any(|k| k != "content" && !(k == "options" && kind == "question"))
    }) {
        return Err(bad());
    }
    let reference = if kind == "goal_completed" {
        sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT goal_id FROM conversations WHERE id=$1 AND active_run=$2",
        )
        .bind(id)
        .bind(run)
        .fetch_one(&app.db)
        .await?
        .ok_or_else(conflict)?
    } else {
        Uuid::new_v4()
    };
    let mut event = json!({"type":kind,"run_id":run,"reference_id":reference,"content":content});
    if let Some(v) = args.get("options") {
        let options = v.as_array().ok_or_else(bad)?;
        if options.len() > 6
            || options.iter().any(|item| {
                item.as_str()
                    .is_none_or(|s| s.trim().is_empty() || s.chars().count() > 160)
            })
        {
            return Err(bad());
        }
        event["options"] = v.clone();
    }
    let object = stage(app, id, event).await?;
    let mut tx = app.db.begin().await?;
    let changed=sqlx::query("UPDATE conversations SET question_id=CASE WHEN $3='question' THEN $4 ELSE question_id END,
        plan_id=CASE WHEN $3='plan' THEN $4 ELSE plan_id END,goal_id=CASE WHEN $3='goal_completed' THEN NULL ELSE goal_id END,revision=revision+1
        WHERE id=$1 AND active_run=$2 AND NOT from_bot AND ($3!='plan' OR permission_mode='plan') AND ($3!='goal_completed' OR (goal_id IS NOT NULL AND permission_mode!='plan'))")
        .bind(id).bind(run).bind(kind).bind(reference).execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        return Err(conflict());
    }
    append(&mut tx, id, object).await?;
    tx.commit().await?;
    let _ = app.events.send(id);
    Ok(())
}

pub(super) async fn cancel_goal(
    State(app): State<App>,
    h: HeaderMap,
    Path((id, goal)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let member = Member::load(&app, &h, true).await?;
    let conversation = projects::owned(&app, &member, id).await?;
    if conversation.get::<Value, _>("document")["goal_id"] != goal.to_string()
        || conversation.get::<Option<Uuid>, _>("active_run").is_some()
    {
        return Err(conflict());
    }
    let object = stage(
        &app,
        id,
        json!({"type":"goal_cancelled","reference_id":goal}),
    )
    .await?;
    let mut tx = app.db.begin().await?;
    let row=sqlx::query("UPDATE conversations SET goal_id=NULL,question_id=NULL,plan_id=NULL,revision=revision+1 WHERE id=$1 AND goal_id=$2 AND active_run IS NULL")
        .bind(id).bind(goal).execute(&mut *tx).await?;
    if row.rows_affected() != 1 {
        return Err(conflict());
    }
    append(&mut tx, id, object).await?;
    tx.commit().await?;
    let _ = app.events.send(id);
    Ok(StatusCode::NO_CONTENT)
}
