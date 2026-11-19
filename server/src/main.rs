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
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{env, sync::Arc, time::Duration};
use tokio::sync::{broadcast, watch};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

#[derive(Clone)]
struct App {
    db: PgPool,
    http: reqwest::Client,
    storage: Arc<storage::Storage>,
    ventic: Option<Arc<Ventic>>,
    standalone: Option<Arc<standalone::Standalone>>,
    computers: Option<Arc<computer::Computers>>,
    events: broadcast::Sender<Uuid>,
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

impl App {
    fn control_request(
        &self,
        h: &HeaderMap,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let ventic = self.ventic.as_ref().ok_or_else(missing)?;
        let mut req = self.http.request(
            method,
            format!("{}/internal/venticode/{path}", ventic.control),
        );
        for name in [
            header::HOST,
            header::ORIGIN,
            header::COOKIE,
            header::CONTENT_TYPE,
        ] {
            if let Some(v) = h.get(&name) {
                req = req.header(name, v);
            }
        }
        Ok(req.header("x-service-token", &ventic.service_token))
    }
    async fn control_json(&self, h: &HeaderMap, path: &str) -> Result<Value> {
        if let Some(local) = &self.standalone {
            return local.json(self, h, path).await;
        }
        let res = self
            .control_request(h, Method::GET, path)?
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
        .control_request(&h, method, path)?
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
    res.headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    res
}

async fn shutdown_signal(shutdown_tx: watch::Sender<bool>) {
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
        .connect(&database_url)
        .await
        .map_err(|e| format!("Failed to connect to DATABASE_URL: {e}"))?;
    sqlx::migrate!("./server/migrations").run(&db).await?;
    let (events, _) = broadcast::channel(512);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app = App {
        db,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()?,
        ventic,
        standalone,
        computers: computer::Computers::from_env()?.map(Arc::new),
        storage: Arc::new(storage::Storage::new()?),
        events,
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
    let frame_policy = header::HeaderValue::from_str(&format!("frame-ancestors {frame_ancestor}"))?;
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
            frame_policy,
        ))
        .layer(axum::middleware::from_fn(security_headers))
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
        .with_graceful_shutdown(shutdown_signal(shutdown_tx))
        .await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
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
