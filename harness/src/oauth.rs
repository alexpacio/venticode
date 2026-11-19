use crate::mcp::Server;
use anyhow::{Context, Result};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialRefreshGuard, CredentialStore,
    OAuthState, StoredCredentials,
};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Store {
    id: String,
    url: String,
}
fn store_error(error: impl std::fmt::Display) -> AuthError {
    tracing::warn!(error = %error, "local MCP credential store error");
    AuthError::CredentialStoreError("Local MCP credential store unavailable".into())
}
#[async_trait::async_trait]
impl CredentialStore for Store {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let id = self.id.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(crate::config::CREDENTIAL_SERVICE, &id)?;
            entry.get_password()
        })
        .await
        .map_err(store_error)?;
        match fetched {
            Ok(raw) => {
                let raw = zeroize::Zeroizing::new(raw);
                let value: serde_json::Value = serde_json::from_str(&raw).map_err(store_error)?;
                if value["url"] != self.url {
                    return Ok(None);
                }
                Ok(Some(
                    serde_json::from_value(value["credentials"].clone()).map_err(store_error)?,
                ))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }
    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let id = self.id.clone();
        let raw =
            zeroize::Zeroizing::new(json!({"url":self.url,"credentials":credentials}).to_string());
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(crate::config::CREDENTIAL_SERVICE, &id)?;
            entry.set_password(&raw)
        })
        .await
        .map_err(store_error)?
        .map_err(store_error)
    }
    async fn clear(&self) -> Result<(), AuthError> {
        let id = self.id.clone();
        let deleted = tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(crate::config::CREDENTIAL_SERVICE, &id)?;
            entry.delete_credential()
        })
        .await
        .map_err(store_error)?;
        match deleted {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(store_error(error)),
        }
    }
    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let path = crate::config::directory()
            .map_err(store_error)?
            .join(format!("oauth-{}.lock", self.id));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(store_error)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(CredentialRefreshGuard::new(file))),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(AuthError::InternalError(
                            "Timed out waiting for OAuth credential refresh lock".into(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(error) => return Err(store_error(error)),
            }
        }
    }
    async fn on_refresh_token_rejected(
        &self,
        _credentials: &StoredCredentials,
    ) -> Result<(), AuthError> {
        self.clear().await
    }
}
pub async fn manager(server: &Server) -> Result<AuthorizationManager> {
    let url = server.config["url"]
        .as_str()
        .context("HTTP MCP server required")?;
    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_credential_store(Store {
        id: server.id.clone(),
        url: url.into(),
    });
    manager.initialize_from_store().await?;
    Ok(manager)
}

async fn drain_http_headers(stream: &mut BufReader<tokio::net::TcpStream>) {
    let mut header = String::new();
    for _ in 0..64 {
        header.clear();
        match (&mut *stream).take(4096).read_line(&mut header).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if header == "\r\n" || header == "\n" {
                    break;
                }
            }
        }
    }
}

async fn write_http_response(
    stream: &mut BufReader<tokio::net::TcpStream>,
    response: &[u8],
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        stream.get_mut().write_all(response).await?;
        stream.get_mut().flush().await?;
        let _ = stream.get_mut().shutdown().await;
        Ok::<(), std::io::Error>(())
    })
    .await
    .context("Timed out writing OAuth callback response")??;
    Ok(())
}

pub async fn login(server: &Server) -> Result<()> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let mut state = OAuthState::Unauthorized(manager(server).await?);
    let mut request = AuthorizationRequest::new(&redirect)
        .with_client_name("ventiharness")
        .with_initial_scope_selection();
    if let Some(client) = server.config["oauth"]["client_id"].as_str() {
        request = request.with_preregistered_client(client);
    }
    state.start_authorization(request).await?;
    let url = state.get_authorization_url().await?;
    eprintln!("Aprire / Open: {url}");
    let _ = open::that_detached(&url);
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(error) => {
                    tracing::warn!(error = %error, "OAuth callback accept error");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(5), async {
                let n = (&mut stream).take(8192).read_line(&mut line).await?;
                if n > 0 {
                    drain_http_headers(&mut stream).await;
                }
                Ok::<usize, std::io::Error>(n)
            })
            .await;
            let Ok(Ok(n)) = read else {
                continue;
            };
            if n == 0 {
                continue;
            }
            let Some(target) = line.split_whitespace().nth(1) else {
                continue;
            };
            if !line.starts_with("GET /callback?") {
                let _ = write_http_response(
                    &mut stream,
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
                continue;
            }
            let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
            let has_oauth_param = query
                .split('&')
                .any(|pair| pair.starts_with("code=") || pair.starts_with("error="));
            if !has_oauth_param {
                let _ = write_http_response(
                    &mut stream,
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
                continue;
            }
            let callback = format!("http://127.0.0.1:{port}{target}");
            if let Err(err) = state.handle_callback_url(&callback).await {
                let _ = write_http_response(
                    &mut stream,
                    b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nSign-in failed.",
                )
                .await;
                return Err(err.into());
            }
            let _ = write_http_response(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 24\r\nConnection: close\r\n\r\nSigned in. Close window.",
            )
            .await;
            return Ok::<_, anyhow::Error>(());
        }
    })
    .await??;
    eprintln!("Accesso MCP completato / MCP signed in");
    Ok(())
}
