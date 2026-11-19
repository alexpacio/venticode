mod agent;
mod browser;
mod computer;
mod connections;
mod history;
mod inference;
mod log;
mod mentions;
mod privacy;
mod projects;
mod runner;
mod runs;
mod secrets;
mod standalone;
mod storage;
mod tools;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Executor, PgPool, Row, postgres::PgPoolOptions};
use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub(crate) enum HubEvent {
    Stored,
    Approval(Uuid),
}

/// Per-conversation event fan-out hub with automatic pruning when no subscriber remains.
#[derive(Default)]
pub(crate) struct EventHub {
    channels: Mutex<HashMap<Uuid, broadcast::Sender<HubEvent>>>,
}

impl EventHub {
    pub fn subscribe(&self, conversation_id: Uuid) -> broadcast::Receiver<HubEvent> {
        let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = map.get(&conversation_id) {
            return tx.subscribe();
        }
        let (tx, rx) = broadcast::channel(64);
        map.insert(conversation_id, tx);
        rx
    }

    pub fn notify_stored(&self, conversation_id: Uuid) {
        self.send(conversation_id, HubEvent::Stored);
    }

    pub fn notify_approval(&self, conversation_id: Uuid, approval_id: Uuid) {
        self.send(conversation_id, HubEvent::Approval(approval_id));
    }

    fn send(&self, conversation_id: Uuid, event: HubEvent) {
        let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = map.get(&conversation_id)
            && tx.send(event).is_err()
            && tx.receiver_count() == 0
        {
            map.remove(&conversation_id);
        }
    }

    pub fn prune_if_empty(&self, conversation_id: Uuid) {
        let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if map
            .get(&conversation_id)
            .is_some_and(|tx| tx.receiver_count() == 0)
        {
            map.remove(&conversation_id);
        }
    }
}

/// Registry of active in-memory run cancellation tokens for immediate `interrupt()` and shutdown.
#[derive(Default)]
pub(crate) struct RunRegistry {
    tokens: Mutex<HashMap<Uuid, CancellationToken>>,
}

impl RunRegistry {
    pub fn register(&self, run_id: Uuid) -> CancellationToken {
        let token = CancellationToken::new();
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id, token.clone());
        token
    }

    pub fn cancel(&self, run_id: Uuid) {
        if let Some(token) = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&run_id)
            .cloned()
        {
            token.cancel();
        }
    }

    pub fn cancel_all(&self) {
        let tokens: Vec<_> = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for token in tokens {
            token.cancel();
        }
    }

    pub fn remove(&self, run_id: Uuid) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id);
    }
}

#[derive(Clone)]
struct App {
    db: PgPool,
    /// Streaming-capable HTTP client (no overall body timeout; callers apply TTFB/idle timeouts).
    http: reqwest::Client,
    /// Bounded HTTP client (20s total timeout) for control-plane & metadata API calls.
    http_api: reqwest::Client,
    storage: Arc<storage::Storage>,
    ventic: Option<Arc<Ventic>>,
    standalone: Option<Arc<standalone::Standalone>>,
    computers: Option<Arc<computer::Computers>>,
    events: Arc<EventHub>,
    runs: Arc<RunRegistry>,
    tasks: TaskTracker,
    shutdown: watch::Receiver<bool>,
}

struct Ventic {
    control: String,
    service_token: String,
    control_hash: Vec<u8>,
}
type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
struct Error(StatusCode, String);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":{"message":self.1}}))).into_response()
    }
}
fn bad() -> Error {
    Error(
        StatusCode::BAD_REQUEST,
        "Richiesta non valida / Invalid request".into(),
    )
}
fn missing() -> Error {
    Error(
        StatusCode::NOT_FOUND,
        "Risorsa non trovata / Resource not found".into(),
    )
}
fn conflict() -> Error {
    Error(StatusCode::CONFLICT, "Conflitto / Conflict".into())
}
fn forbidden() -> Error {
    Error(
        StatusCode::FORBIDDEN,
        "Accesso negato / Access denied".into(),
    )
}
fn unavailable() -> Error {
    Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Servizio non disponibile / Service unavailable".into(),
    )
}
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => missing(),
            sqlx::Error::Database(ref db) if db.is_unique_violation() => conflict(),
            sqlx::Error::Database(ref db) if db.message().contains("Subject erased") => forbidden(),
            other => {
                tracing::error!(error = %other, "database query failed");
                unavailable()
            }
        }
    }
}

fn require_env(name: &str) -> std::result::Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("Missing required environment variable: {name}").into())
}

/// Join all `Cookie` headers (handling HTTP/2 multi-header cookie splitting).
pub(crate) fn merged_cookies(h: &HeaderMap) -> Option<String> {
    let mut out = String::new();
    for v in h.get_all(header::COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(s);
    }
    (!out.is_empty()).then_some(out)
}

impl App {
    fn control_request_with(
        &self,
        client: &reqwest::Client,
        h: &HeaderMap,
        method: Method,
        path: &str,
        forward_content_type: bool,
    ) -> Result<reqwest::RequestBuilder> {
        let ventic = self.ventic.as_ref().ok_or_else(missing)?;
        let mut req = client.request(
            method,
            format!("{}/internal/venticode/{path}", ventic.control),
        );
        for name in [header::HOST, header::ORIGIN] {
            if let Some(v) = h.get(&name) {
                req = req.header(name, v);
            }
        }
        if let Some(cookies) = merged_cookies(h) {
            req = req.header(header::COOKIE, cookies);
        }
        if forward_content_type && let Some(ct) = h.get(header::CONTENT_TYPE) {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        Ok(req.header("x-service-token", &ventic.service_token))
    }

    fn control_request(
        &self,
        h: &HeaderMap,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        self.control_request_with(&self.http, h, method, path, false)
    }

    fn control_api_request(
        &self,
        h: &HeaderMap,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        self.control_request_with(&self.http_api, h, method, path, false)
    }

    async fn control_json(&self, h: &HeaderMap, path: &str) -> Result<Value> {
        if let Some(local) = &self.standalone {
            return local.json(self, h, path).await;
        }
        let res = self
            .control_api_request(h, Method::GET, path)?
            .send()
            .await
            .map_err(|_| unavailable())?;
        let status = res.status();
        let value: Value = res.json().await.map_err(|_| unavailable())?;
        if !status.is_success() {
            return Err(Error(
                status,
                value["error"]["message"]
                    .as_str()
                    .unwrap_or("Access denied")
                    .into(),
            ));
        }
        Ok(value)
    }
}
#[derive(Clone)]
struct Member {
    workspace: Uuid,
    user: Uuid,
    harnesses: Vec<Value>,
}
impl Member {
    async fn load(app: &App, h: &HeaderMap, write: bool) -> Result<Self> {
        if let Some(local) = &app.standalone {
            local.validate_host(h, write)?;
        } else if write {
            let host = h
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(forbidden)?;
            if !h
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|o| o.eq_ignore_ascii_case(&format!("https://{host}")))
            {
                return Err(forbidden());
            }
        }
        let (session, inventory) = tokio::try_join!(
            app.control_json(h, "session"),
            app.control_json(h, "harnesses")
        )?;
        let mut member = Self {
            workspace: serde_json::from_value(session["workspace"]["id"].clone())
                .map_err(|_| unavailable())?,
            user: serde_json::from_value(session["user"]["id"].clone())
                .map_err(|_| unavailable())?,
            harnesses: inventory["items"]
                .as_array()
                .cloned()
                .ok_or_else(unavailable)?,
        };
        if let Some(computers) = &app.computers
            && let Some(item) = computers
                .inventory(app, member.workspace, member.user)
                .await?
        {
            member.harnesses.push(item);
        }
        Ok(member)
    }
    fn harness(&self, id: Uuid, root: &str) -> Result<&Value> {
        self.harnesses
            .iter()
            .find(|v| {
                v["id"] == id.to_string()
                    && v["roots"]
                        .as_array()
                        .is_some_and(|roots| roots.iter().any(|r| r == root))
                    && (v["owned"] == true || v["mode"] == "sandboxed")
            })
            .ok_or_else(missing)
    }
}
async fn proxy(
    State(app): State<App>,
    method: Method,
    uri: axum::http::Uri,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let path = uri.path().rsplit('/').next().ok_or_else(missing)?;
    if method == Method::GET
        && let Some(computers) = &app.computers
        && matches!(path, "session" | "harnesses")
    {
        let value = if path == "session" {
            let mut session = app.control_json(&h, path).await?;
            session["bot_runtimes"] = json!(computers.runtimes());
            session
        } else {
            json!({"items":Member::load(&app, &h, false).await?.harnesses})
        };
        return Ok(Json(value).into_response());
    }
    if let Some(local) = &app.standalone {
        return match (method, path) {
            (Method::DELETE, "session") => local.sign_out(&app, &h).await,
            (Method::GET, "session" | "models" | "harnesses") => {
                Ok(Json(local.json(&app, &h, path).await?).into_response())
            }
            _ => Err(missing()),
        };
    }
    let upstream = app
        .control_request_with(&app.http_api, &h, method, path, true)?
        .body(body)
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut result = (status, Body::from_stream(upstream.bytes_stream())).into_response();
    for name in [header::CONTENT_TYPE, header::SET_COOKIE, header::LOCATION] {
        for v in headers.get_all(&name) {
            result.headers_mut().append(&name, v.clone());
        }
    }
    Ok(result)
}

async fn readyz(State(app): State<App>) -> impl IntoResponse {
    if *app.shutdown.borrow() {
        return (StatusCode::SERVICE_UNAVAILABLE, "draining");
    }
    match sqlx::query("SELECT 1").execute(&app.db).await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(error) => {
            tracing::warn!(error = %error, "readiness database check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
    }
}

async fn security_headers(
    req: axum::http::Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    let immutable_asset = req.uri().path().starts_with("/assets/");
    let mut res = next.run(req).await;
    let cache = if immutable_asset && res.status().is_success() {
        "public, max-age=31536000, immutable"
    } else {
        "private, no-store"
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, cache.parse().unwrap());
    res.headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    // `same-origin` avoids leaking referrers cross-origin while allowing browsers
    // to send the real `Origin` header on same-origin POST forms such as `/venticode/login`.
    res.headers_mut()
        .insert("referrer-policy", "same-origin".parse().unwrap());
    res
}

async fn shutdown_signal(shutdown_tx: watch::Sender<bool>, runs: Arc<RunRegistry>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, draining"),
        _ = terminate => tracing::info!("received SIGTERM, draining"),
    }
    let _ = shutdown_tx.send(true);
    runs.cancel_all();
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    log::init();
    let mode = env::var("VENTICODE_MODE").unwrap_or_else(|_| "ventic".into());
    let (ventic, standalone) = match mode.as_str() {
        "standalone" => (None, Some(Arc::new(standalone::Standalone::from_env()?))),
        "ventic" => {
            let control = require_env("CONTROL_URL")?.trim_end_matches('/').to_owned();
            let parsed = reqwest::Url::parse(&control)
                .map_err(|_| "CONTROL_URL must be a valid HTTP or HTTPS URL")?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
                return Err("CONTROL_URL must be an HTTP or HTTPS URL".into());
            }
            let service_token = require_env("VENTICODE_SERVICE_TOKEN")?;
            let control_hash = hex::decode(require_env("CONTROL_SERVICE_TOKEN_HASH")?.trim())
                .map_err(|_| "CONTROL_SERVICE_TOKEN_HASH must be valid hex")?;
            if control_hash.len() != 32 {
                return Err("CONTROL_SERVICE_TOKEN_HASH must decode to 32 bytes (SHA-256)".into());
            }
            (
                Some(Arc::new(Ventic {
                    control,
                    service_token,
                    control_hash,
                })),
                None,
            )
        }
        _ => return Err("Set VENTICODE_MODE to standalone or ventic".into()),
    };
    let database_url = require_env("DATABASE_URL")?;
    let max_connections = env::var("DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(20, |n| n.clamp(1, 200));
    let db = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET statement_timeout = '30s'").await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .map_err(|e| format!("Failed to connect to DATABASE_URL: {e}"))?;
    sqlx::migrate!("./server/migrations").run(&db).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(30))
        .build()?;
    let http_api = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(30))
        .build()?;
    let runs = Arc::new(RunRegistry::default());
    let app = App {
        db,
        http,
        http_api,
        ventic,
        standalone,
        computers: computer::Computers::from_env()?.map(Arc::new),
        storage: Arc::new(storage::Storage::new()?),
        events: Arc::new(EventHub::default()),
        runs: runs.clone(),
        tasks: TaskTracker::new(),
        shutdown: shutdown_rx,
    };
    if let Some(local) = &app.standalone {
        local
            .runner
            .load_inventory(&app.db, local.workspace)
            .await?;
    }
    let api = Router::new()
        .route("/session", get(proxy).delete(proxy))
        .route("/models", get(proxy))
        .route("/harnesses", get(proxy))
        .merge(projects::routes())
        .route("/approvals/{id}", post(tools::decide))
        .route(
            "/conversations/{id}/sessions/{call}/input",
            post(tools::input),
        )
        .merge(runs::routes())
        .merge(connections::routes())
        .merge(mentions::routes())
        .merge(computer::routes())
        .fallback(|| async { missing() });
    let assets = env::var("WEB_ROOT").unwrap_or_else(|_| "web/dist".into());
    if !std::path::Path::new(&assets).exists() {
        tracing::warn!(web_root = %assets, "WEB_ROOT directory does not exist");
    }
    let frame_ancestor = app
        .ventic
        .as_ref()
        .and_then(|_| env::var("PORTAL_ORIGIN").ok())
        .and_then(|origin| reqwest::Url::parse(&origin).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "'none'".into());
    let csp = header::HeaderValue::from_str(&format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self' ws: wss:; object-src 'none'; base-uri 'self'; frame-ancestors {frame_ancestor}"
    ))?;
    let router = if app.ventic.is_some() {
        Router::new()
            .route("/internal/privacy/{action}", post(privacy::handle))
            .route("/venticode/bootstrap", post(proxy))
    } else {
        Router::new().route("/internal/harness/ws", get(runner::upgrade))
    };
    let router = router
        .route("/internal/computer/{id}/ws", get(computer::enroll))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/hooks/{kind}/{id}", post(mentions::hook))
        .route("/oauth/slack", get(connections::callback))
        .route(
            "/venticode/login",
            get(standalone::form).post(standalone::login),
        )
        .nest("/api/v1/venticode", api)
        .fallback_service(
            ServeDir::new(&assets).fallback(ServeFile::new(format!("{assets}/index.html"))),
        )
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(app.clone());
    let mentions_task = tokio::spawn(mentions::worker(app.clone()));
    let computer_task = app
        .computers
        .as_ref()
        .map(|_| tokio::spawn(computer::maintain(app.clone())));
    let storage_task = tokio::spawn(storage::maintain(app.clone()));
    let listen = env::var("LISTEN").unwrap_or_else(|_| "127.0.0.1:4189".into());
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(
        mode = %mode,
        listen = %listen,
        web_root = %assets,
        max_db_connections = max_connections,
        computers = app.computers.is_some(),
        "venticode server started"
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal(shutdown_tx, runs))
        .await?;
    app.tasks.close();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        app.tasks.wait().await;
        let _ = mentions_task.await;
        if let Some(task) = computer_task {
            let _ = task.await;
        }
        let _ = storage_task.await;
    })
    .await;
    tracing::info!("venticode server stopped");
    Ok(())
}
