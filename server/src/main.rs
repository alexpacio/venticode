mod agent;
mod connections;
mod history;
mod mentions;
mod privacy;
mod projects;
mod runs;
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
    control: String,
    storage: Arc<storage::Storage>,
    service_token: String,
    control_hash: Vec<u8>,
}
type Result<T> = std::result::Result<T, Error>;
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
    ) -> reqwest::RequestBuilder {
        let mut req = self.http.request(
            method,
            format!("{}/internal/venticode/{path}", self.control),
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
        req.header("x-service-token", &self.service_token)
    }
    async fn control_json(&self, h: &HeaderMap, path: &str) -> Result<Value> {
        let res = self
            .control_request(h, Method::GET, path)
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
        if write {
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
    let upstream = app
        .control_request(&h, method, path)
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
    let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
    sqlx::migrate!("./server/migrations").run(&db).await?;
    let app = App {
        db,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        control: env::var("CONTROL_URL")?,
        service_token: env::var("VENTICODE_SERVICE_TOKEN")?,
        control_hash: hex::decode(env::var("CONTROL_SERVICE_TOKEN_HASH")?)?,
        storage: Arc::new(storage::Storage::new()?),
    };
    let api = Router::new()
        .route("/session", get(proxy).delete(proxy))
        .route("/models", get(proxy))
        .route("/harnesses", get(proxy))
        .merge(projects::routes())
        .merge(connections::routes())
        .merge(mentions::routes())
        .route("/approvals/{id}", post(tools::decide))
        .route(
            "/conversations/{id}/sessions/{call}/input",
            post(tools::input),
        )
        .merge(runs::routes())
        .fallback(|| async { missing() });
    let assets = env::var("WEB_ROOT").unwrap_or_else(|_| "web/dist".into());
    let router = Router::new()
        .route("/internal/privacy/{action}", post(privacy::handle))
        .route("/hooks/{kind}/{id}", post(mentions::hook))
        .route("/oauth/slack", get(connections::callback))
        .route("/healthz", get(|| async { "ok" }))
        .route("/venticode/bootstrap", post(proxy))
        .nest("/api/v1/venticode", api)
        .fallback_service(
            ServeDir::new(&assets).fallback(ServeFile::new(format!("{assets}/index.html"))),
        )
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
