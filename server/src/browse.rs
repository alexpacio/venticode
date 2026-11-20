use super::*;
use std::time::Duration;

/// Bounded window for a read-only directory listing round-trip to the harness.
const BROWSE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Deserialize)]
pub(super) struct BrowseQuery {
    root: String,
    path: Option<String>,
}

/// `GET /harnesses/{id}/ls` — list a directory inside a granted harness root so
/// the TUI (and web) can browse folders before creating a project. Read-only:
/// it only ever dispatches the `list_files` project operation, never a write,
/// edit or execute, and the relative path is validated against traversal.
pub(super) async fn list(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Query(q): Query<BrowseQuery>,
    h: HeaderMap,
) -> Result<Json<Value>> {
    let member = Member::load(&app, &h, false).await?;
    // Authorize the harness + granted root exactly like project creation does.
    member.harness(id, &q.root)?;
    let path = q.path.as_deref().unwrap_or(".");
    if !crate::projects::relative(path) {
        return Err(bad());
    }
    let request_id = Uuid::new_v4();
    let execute = json!({
        "action": "execute",
        "request_id": request_id,
        "project_id": request_id,
        "harness_id": id,
        "root": q.root,
        "project_path": path,
        "operation": "list_files",
        "arguments": {"path": "."},
    });
    let value = browse(&app, &h, &execute, request_id).await?;
    let items = value["items"].as_array().cloned().unwrap_or_default();
    Ok(Json(json!({"items": items, "path": path})))
}

/// Drive an `execute` + `poll` round-trip through `relay` (which routes to the
/// computer, standalone or control-plane runner) and return the `list_files`
/// result value. Mirrors `computer::rpc` but keeps the caller's headers so the
/// control-plane path can authorize the request. Cancels the request on failure.
pub(super) async fn browse(
    app: &App,
    h: &HeaderMap,
    execute: &Value,
    request_id: Uuid,
) -> Result<Value> {
    browse_within(app, h, execute, request_id, BROWSE_TIMEOUT).await
}

/// [`browse`] with a caller-chosen deadline (repository downloads take longer).
pub(super) async fn browse_within(
    app: &App,
    h: &HeaderMap,
    execute: &Value,
    request_id: Uuid,
    deadline: Duration,
) -> Result<Value> {
    crate::tools::relay(app, h, execute.clone()).await?;
    let outcome = tokio::time::timeout(deadline, async {
        let mut after = 0i64;
        let mut result = None;
        loop {
            let poll = crate::tools::relay(
                app,
                h,
                json!({"action": "poll", "request_id": request_id, "after": after}),
            )
            .await?;
            if let Some(frames) = poll["frames"].as_array() {
                for frame in frames {
                    if frame["result"]["type"] == "error" {
                        return Err(Error(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            frame["result"]["error"]
                                .as_str()
                                .unwrap_or("Harness request failed")
                                .to_owned(),
                        ));
                    }
                    if frame["result"]["type"] == "result" {
                        result = Some(frame["result"]["value"].clone());
                    }
                }
            }
            after = poll["after"].as_i64().unwrap_or(after);
            match poll["state"].as_str() {
                Some("done") => return result.ok_or_else(unavailable),
                Some("failed" | "cancelled") => return Err(unavailable()),
                _ => {}
            }
            // Yield between polls; the harness replies promptly for a local list.
            tokio::task::yield_now().await;
        }
    })
    .await;
    if outcome.is_err() || outcome.as_ref().is_ok_and(|v| v.is_err()) {
        let _ = crate::tools::relay(
            app,
            h,
            json!({"action": "cancel", "request_id": request_id}),
        )
        .await;
    }
    outcome.map_err(|_| unavailable())?
}

#[cfg(test)]
mod tests {

    #[test]
    fn browse_rejects_traversal_and_absolute_paths() {
        // The query path must pass the same gate project creation enforces.
        for ok in [".", "src", "src/app"] {
            assert!(crate::projects::relative(ok), "{ok} should be allowed");
        }
        for bad in ["", "..", "/etc", "src/../secrets", "a\\b", "./src"] {
            assert!(!crate::projects::relative(bad), "{bad} should be rejected");
        }
    }
}
