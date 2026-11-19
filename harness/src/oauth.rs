use crate::mcp::Server;
use anyhow::{Context, Result, ensure};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialRefreshGuard, CredentialStore,
    OAuthState, StoredCredentials,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Store {
    id: String,
    url: String,
}
fn store_error(_: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError("Local MCP credential store unavailable".into())
}
impl Store {
    fn entry(&self) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(crate::config::CREDENTIAL_SERVICE, &self.id)
    }
}
#[async_trait::async_trait]
impl CredentialStore for Store {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match self.entry().map_err(store_error)?.get_password() {
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
        let raw =
            zeroize::Zeroizing::new(json!({"url":self.url,"credentials":credentials}).to_string());
        self.entry()
            .map_err(store_error)?
            .set_password(&raw)
            .map_err(store_error)
    }
    async fn clear(&self) -> Result<(), AuthError> {
        match self.entry().map_err(store_error)?.delete_credential() {
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
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(CredentialRefreshGuard::new(file))),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await
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
pub async fn login(server: &Server) -> Result<()> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
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
    tokio::time::timeout(std::time::Duration::from_secs(300),async {
        loop {
            let (stream,peer)=listener.accept().await?;
            ensure!(peer.ip().is_loopback(),"Nonlocal OAuth callback");
            let mut stream=BufReader::new(stream);
            let mut line=String::new();
            (&mut stream).take(8192).read_line(&mut line).await?;
            let target=line.split_whitespace().nth(1).context("Invalid callback")?;
            if !line.starts_with("GET /callback?") { stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;continue; }
            let callback=format!("http://127.0.0.1:{}{target}",listener.local_addr()?.port());
            state.handle_callback_url(&callback).await?;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 23\r\nConnection: close\r\n\r\nSigned in. Close window.").await?;
            return Ok::<_,anyhow::Error>(());
        }
    }).await??;
    eprintln!("Accesso MCP completato / MCP signed in");
    Ok(())
}
