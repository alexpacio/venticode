use anyhow::{Context, Result};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::get,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};
use uuid::Uuid;

// The ticket travels in the fragment, so it never appears in a request URL.
// Replace the fragment before posting it back to this random, loopback-only path.
const RETURN_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Venticode</title>
<p id="status">Collegamento… / Connecting…</p><script>
const code = location.hash.slice(1);
history.replaceState(null, '', location.pathname);
fetch(location.pathname, {method:'POST',body:decodeURIComponent(code)}).then(r=>{
document.getElementById('status').textContent = r.ok ? 'Torna al terminale / Return to the terminal' : 'Collegamento fallito / Connection failed';
});</script>"#;

fn sign_in_url(portal: &str, port: &str, state: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{portal}/app/venticode"))?;
    url.query_pairs_mut()
        .append_pair("terminal", "1")
        .append_pair("port", port)
        .append_pair("state", state);
    Ok(url)
}

pub async fn browser(portal: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let host = listener.local_addr()?.to_string();
    let state = Uuid::new_v4().to_string();
    let port = listener.local_addr()?.port().to_string();
    let path = format!("/{state}");
    let (sender, receiver) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(sender)));
    let route = get(|| async { Html(RETURN_PAGE) }).post(
        move |headers: HeaderMap, body: String| async move {
            if headers.get("host").and_then(|h| h.to_str().ok()) != Some(&host)
                || headers.get("origin").and_then(|h| h.to_str().ok())
                    != Some(&format!("http://{host}"))
            {
                return StatusCode::FORBIDDEN;
            }
            match sender.lock().unwrap().take() {
                Some(tx) if !body.is_empty() => {
                    let _ = tx.send(body);
                    StatusCode::OK
                }
                _ => StatusCode::GONE,
            }
        },
    );
    let app = Router::new()
        .route(&path, route)
        .layer(DefaultBodyLimit::max(8192));
    let url = sign_in_url(portal, &port, &state)?;
    eprintln!(
        "Accedi nel browser / Sign in in your browser:\n{url}\nPortale precedente / Older portal: --manual"
    );
    let _ = open::that_detached(url.as_str());
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let code = tokio::time::timeout(Duration::from_secs(600), receiver).await;
    let _ = stop.send(());
    task.await??;
    Ok(code.context("Login timed out")??)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_encodes_only_loopback_port_and_nonce() {
        let state = Uuid::new_v4().to_string();
        let url = sign_in_url("https://portal.example", "49152", &state).unwrap();
        assert_eq!(url.path(), "/app/venticode");
        assert_eq!(
            url.query().unwrap(),
            format!("terminal=1&port=49152&state={state}")
        );
    }
}
