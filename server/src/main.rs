mod agent;
mod connections;
mod history;
mod inference;
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
use sqlx::{PgPool, Row};
use std::{env, sync::Arc};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

#[derive(Clone)]
struct App {
    db: PgPool,
    http: reqwest::Client,
    storage: Arc<storage::Storage>,
    ventic: Option<Arc<Ventic>>,
    standalone: Option<Arc<standalone::Standalone>>,
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
            sqlx::Error::Database(ref e) if e.is_unique_violation() => conflict(),
            _ => unavailable(),
        }
    }
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
            if h.get(header::ORIGIN).and_then(|v| v.to_str().ok())
                != Some(&format!("https://{host}"))
            {
                return Err(forbidden());
            }
        }
        let (session, inventory) = tokio::try_join!(
            app.control_json(h, "session"),
            app.control_json(h, "harnesses")
        )?;
        Ok(Self {
            workspace: serde_json::from_value(session["workspace"]["id"].clone())
                .map_err(|_| unavailable())?,
            user: serde_json::from_value(session["user"]["id"].clone())
                .map_err(|_| unavailable())?,
            harnesses: inventory["items"]
                .as_array()
                .cloned()
                .ok_or_else(unavailable)?,
        })
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
        if let Some(v) = headers.get(&name) {
            result.headers_mut().insert(name, v.clone());
        }
    }
    Ok(result)
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (ventic, standalone) = match env::var("VENTICODE_MODE").as_deref() {
        Ok("standalone") => (None, Some(Arc::new(standalone::Standalone::from_env()?))),
        Ok("ventic") | Err(env::VarError::NotPresent) => (
            Some(Arc::new(Ventic {
                control: env::var("CONTROL_URL")?,
                service_token: env::var("VENTICODE_SERVICE_TOKEN")?,
                control_hash: hex::decode(env::var("CONTROL_SERVICE_TOKEN_HASH")?)?,
            })),
            None,
        ),
        _ => return Err("Set VENTICODE_MODE to standalone or ventic".into()),
    };
    let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
    sqlx::migrate!("./server/migrations").run(&db).await?;
    let app = App {
        db,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        ventic,
        standalone,
        storage: Arc::new(storage::Storage::new()?),
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
        .fallback(|| async { missing() });
    let assets = env::var("WEB_ROOT").unwrap_or_else(|_| "web/dist".into());
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
        .route("/healthz", get(|| async { "ok" }))
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
        .layer(axum::middleware::map_response(
            |mut res: Response| async move {
                res.headers_mut()
                    .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
                res.headers_mut()
                    .insert("x-content-type-options", "nosniff".parse().unwrap());
                res
            },
        ))
        .with_state(app.clone());
    tokio::spawn(mentions::worker(app.clone()));
    tokio::spawn(storage::maintain(app));
    let listener = tokio::net::TcpListener::bind(
        env::var("LISTEN").unwrap_or_else(|_| "127.0.0.1:4189".into()),
    )
    .await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
