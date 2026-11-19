use anyhow::Result;
use asupersync::Cx;
use asupersync::channel::oneshot;
use asupersync::http::h1::{
    Http1Config, Http1Listener, Http1ListenerConfig, Method, Request, Response,
};
use asupersync::net::TcpListener;
use asupersync::time::{timeout, wall_now};
use rich_rust::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;
use uuid::Uuid;

// The ticket travels in the fragment, so it never appears in a request URL.
// Replace the fragment before posting it back to this random, loopback-only path.
const RETURN_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Venticode</title>
<p id="status">Collegamento… / Connecting…</p><script>
const code = location.hash.slice(1);
history.replaceState(null, '', location.pathname);
fetch(location.pathname, {method:'POST',body:decodeURIComponent(code)}).then(r=>{
document.getElementById('status').textContent = r.ok ? 'Torna al terminale / Return to the terminal' : 'Collegamento fallito / Connection failed';
}).catch(()=>{
document.getElementById('status').textContent = 'Collegamento fallito / Connection failed';
});</script>"#;

fn sign_in_url(portal: &str, port: &str, state: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("{portal}/app/venticode"))?;
    url.query_pairs_mut()
        .append_pair("terminal", "1")
        .append_pair("port", port)
        .append_pair("state", state);
    Ok(url)
}

pub async fn browser(cx: &Cx, portal: &str, en: bool) -> Result<String> {
    let tcp = TcpListener::bind("127.0.0.1:0").await?;
    let local = tcp.local_addr()?;
    let host = local.to_string();
    let state = Uuid::new_v4().to_string();
    let port = local.port().to_string();
    let expected_path = format!("/{state}");
    let expected_origin = format!("http://{host}");
    let (sender, mut receiver) = oneshot::channel::<String>();
    let sender = Arc::new(Mutex::new(Some(sender)));

    let config = Http1ListenerConfig::default()
        .http_config(
            Http1Config::default()
                .max_body_size(8192)
                .allowed_hosts(Some(vec![host.clone()])),
        )
        .drain_timeout(Duration::from_secs(2));

    let path_for_handler = expected_path.clone();
    let host_for_handler = host.clone();
    let origin_for_handler = expected_origin.clone();
    let sender_for_handler = sender.clone();

    let listener = Http1Listener::from_listener(
        tcp,
        move |req: Request| {
            let expected_path = path_for_handler.clone();
            let expected_host = host_for_handler.clone();
            let expected_origin = origin_for_handler.clone();
            let sender = sender_for_handler.clone();
            async move {
                if req.uri != expected_path {
                    return Response::new(404, "Not Found", Vec::new());
                }
                match req.method {
                    Method::Get => Response::new(200, "OK", RETURN_PAGE.as_bytes().to_vec())
                        .with_header("Content-Type", "text/html; charset=utf-8"),
                    Method::Post => {
                        let host_ok = req
                            .header_value("host")
                            .is_some_and(|v| v.eq_ignore_ascii_case(&expected_host));
                        let origin_ok = req
                            .header_value("origin")
                            .is_some_and(|v| v.eq_ignore_ascii_case(&expected_origin));
                        if !host_ok || !origin_ok {
                            return Response::new(403, "Forbidden", Vec::new());
                        }
                        let Ok(body) = String::from_utf8(req.body) else {
                            return Response::new(400, "Bad Request", Vec::new());
                        };
                        if body.is_empty() {
                            return Response::new(400, "Bad Request", Vec::new());
                        }
                        let taken = sender.lock().unwrap_or_else(|e| e.into_inner()).take();
                        if let Some(tx) = taken {
                            if let Some(req_cx) = Cx::current() {
                                let _ = tx.send(&req_cx, body);
                            }
                            Response::new(200, "OK", Vec::new())
                        } else {
                            Response::new(410, "Gone", Vec::new())
                        }
                    }
                    _ => Response::new(405, "Method Not Allowed", Vec::new()),
                }
            }
        },
        config,
    );

    let url = sign_in_url(portal, &port, &state)?;
    let console = Console::new();
    let opened = open::that_detached(url.as_str()).is_ok();
    let body = if en {
        format!(
            "Sign in in your browser:\n{url}\n{}",
            if opened {
                "Waiting for callback… (Older portal: --manual)"
            } else {
                "Could not open a browser automatically. Open the URL above or use --manual."
            }
        )
    } else {
        format!(
            "Accedi nel browser:\n{url}\n{}",
            if opened {
                "In attesa del collegamento… (Portale precedente: --manual)"
            } else {
                "Impossibile aprire il browser automaticamente. Apri l'URL sopra o usa --manual."
            }
        )
    };
    let panel = Panel::from_text(&body)
        .title("Venticode")
        .border_style(Style::parse("cyan").unwrap_or_default());
    console.print_renderable(&panel);

    let shutdown = listener.shutdown_signal();
    let mut server_task = cx
        .spawn(move |child_cx| async move {
            let _ = listener.run_owned_in(&child_cx).await;
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn login callback server: {e}"))?;

    let code = timeout(wall_now(), Duration::from_secs(600), receiver.recv(cx)).await;
    let _ = shutdown.begin_drain(Duration::from_secs(2));
    let _ = server_task.join(cx).await;
    let code = code
        .map_err(|_| anyhow::anyhow!("Login timed out"))?
        .map_err(|_| anyhow::anyhow!("Login callback closed"))?;
    Ok(code)
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
