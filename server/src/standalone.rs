use super::*;
use axum::{
    extract::{Form, Query},
    response::{Html, Redirect},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::Instant,
};
use subtle::ConstantTimeEq;

pub(super) struct Standalone {
    pub provider: inference::Provider,
    pub runner: runner::Runner,
    pub origin: String,
    pub host: String,
    secure: bool,
    pub workspace: Uuid,
    pub user: Uuid,
    login_hash: [u8; 32],
    failed_logins: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Standalone {
    pub fn from_env() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let origin_raw = require_env("VENTICODE_ORIGIN")?;
        let url = reqwest::Url::parse(origin_raw.trim())
            .map_err(|e| format!("Invalid VENTICODE_ORIGIN: {e}"))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        let secure = url.scheme() == "https";
        if !(secure || url.scheme() == "http" && loopback)
            || url.path() != "/"
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "VENTICODE_ORIGIN must be an HTTPS origin or a loopback HTTP origin".into(),
            );
        }
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap()),
            None => url.host_str().unwrap().to_owned(),
        };
        let workspace = require_env("VENTICODE_WORKSPACE_ID")?
            .trim()
            .parse()
            .map_err(|_| "VENTICODE_WORKSPACE_ID must be a UUID")?;
        let user = require_env("VENTICODE_USER_ID")?
            .trim()
            .parse()
            .map_err(|_| "VENTICODE_USER_ID must be a UUID")?;
        let harness_id = env::var("VENTICODE_HARNESS_ID")
            .map_or(Ok(workspace), |id| id.trim().parse())
            .map_err(|_| "VENTICODE_HARNESS_ID must be a UUID")?;
        let harness_secret = require_env("VENTICODE_HARNESS_TOKEN")?;
        let login_secret = require_env("VENTICODE_LOGIN_TOKEN")?;
        let valid_token = |token: &str| {
            (32..=1024).contains(&token.len()) && token.bytes().all(|v| v.is_ascii_graphic())
        };
        if !valid_token(&login_secret)
            || !valid_token(&harness_secret)
            || login_secret == harness_secret
        {
            return Err(
                "Configure separate random login and harness tokens of at least 32 characters"
                    .into(),
            );
        }
        Ok(Self {
            provider: inference::Provider::from_env()?,
            runner: runner::Runner::new(harness_id, &harness_secret),
            origin: url.origin().ascii_serialization(),
            host,
            secure,
            workspace,
            user,
            login_hash: Sha256::digest(login_secret.as_bytes()).into(),
            failed_logins: Mutex::default(),
        })
    }

    pub fn validate_host(&self, h: &HeaderMap, write: bool) -> Result<()> {
        let host_ok = h
            .get(header::HOST)
            .and_then(|s| s.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case(&self.host));
        let origin_ok = !write
            || h.get(header::ORIGIN)
                .and_then(|s| s.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case(&self.origin));
        if !host_ok || !origin_ok {
            tracing::warn!(expected_host = %self.host, write, "rejected request due to host/origin mismatch");
            return Err(forbidden());
        }
        Ok(())
    }

    fn client_bucket(h: &HeaderMap) -> String {
        if let Some(xff) = h.get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(first) = xff
                .split(',')
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return first.chars().take(64).collect();
        }
        if let Some(ip) = h
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return ip.chars().take(64).collect();
        }
        "default".into()
    }

    fn record_failed_login(&self, h: &HeaderMap) -> bool {
        let key = Self::client_bucket(h);
        let mut map = self.failed_logins.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Instant::now() - Duration::from_secs(60);
        if map.len() >= 1024 {
            map.retain(|_, q| {
                while q.front().is_some_and(|t| *t < cutoff) {
                    q.pop_front();
                }
                !q.is_empty()
            });
            if map.len() >= 1024 {
                map.clear();
            }
        }
        let attempts = map.entry(key).or_default();
        while attempts.front().is_some_and(|t| *t < cutoff) {
            attempts.pop_front();
        }
        if attempts.len() >= 10 {
            return true;
        }
        attempts.push_back(Instant::now());
        false
    }

    fn login_rate_limited(&self, h: &HeaderMap) -> bool {
        let key = Self::client_bucket(h);
        let mut map = self.failed_logins.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Instant::now() - Duration::from_secs(60);
        let Some(attempts) = map.get_mut(&key) else {
            return false;
        };
        while attempts.front().is_some_and(|t| *t < cutoff) {
            attempts.pop_front();
        }
        attempts.len() >= 10
    }

    pub fn session_hash(&self, h: &HeaderMap) -> Result<String> {
        self.validate_host(h, false)?;
        let mut cookies = h
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|v| v.trim().strip_prefix("venticode_session="));
        let session = cookies
            .next()
            .filter(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(unauthorized)?;
        if cookies.next().is_some() {
            return Err(unauthorized());
        }
        Ok(connections::digest(session))
    }

    pub async fn authorize(&self, app: &App, h: &HeaderMap) -> Result<String> {
        self.active_session(app, h).await.map(|(hash, _)| hash)
    }

    async fn active_session(&self, app: &App, h: &HeaderMap) -> Result<(String, bool)> {
        let hash = self.session_hash(h)?;
        let (delegated, needs_refresh): (bool, bool) = sqlx::query_as(
            "SELECT bot_grant IS NOT NULL, bot_grant IS NULL AND expires_at < now()+interval '6 hours' FROM standalone_sessions WHERE token_hash=$1 AND workspace=$2 AND owner_id=$3 AND expires_at>now() AND absolute_expires_at>now()",
        )
        .bind(&hash)
        .bind(self.workspace)
        .bind(self.user)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(unauthorized)?;
        if needs_refresh {
            let _ = sqlx::query(
                "UPDATE standalone_sessions SET expires_at=LEAST(now()+interval '12 hours', absolute_expires_at) WHERE token_hash=$1 AND bot_grant IS NULL",
            )
            .bind(&hash)
            .execute(&app.db)
            .await;
        }
        Ok((hash, delegated))
    }

    pub async fn json(&self, app: &App, h: &HeaderMap, path: &str) -> Result<Value> {
        let (_, delegated) = self.active_session(app, h).await?;
        match path {
            "session" => Ok(
                json!({"user":{"id":self.user,"name":"Venticode"},"workspace":{"id":self.workspace,"name":"Venticode"},"role":if delegated { "member" } else { "tenant_admin" },"portal_url":self.origin,"standalone":true}),
            ),
            "models" => Ok(self.provider.models()),
            "harnesses" => Ok(self.runner.inventory()),
            _ => Err(missing()),
        }
    }

    fn cookie(&self, value: &str, seconds: u32) -> String {
        format!(
            "venticode_session={value}; Path=/; Max-Age={seconds}; HttpOnly; SameSite=Lax{}",
            if self.secure { "; Secure" } else { "" }
        )
    }

    pub async fn sign_out(&self, app: &App, h: &HeaderMap) -> Result<Response> {
        self.validate_host(h, true)?;
        let hash = self.authorize(app, h).await?;
        sqlx::query("DELETE FROM standalone_sessions WHERE token_hash=$1")
            .bind(hash)
            .execute(&app.db)
            .await?;
        Ok(([(header::SET_COOKIE, self.cookie("", 0))], Json(json!({"signed_out":true,"redirect_url":format!("{}/venticode/login", self.origin)}))).into_response())
    }

    pub async fn bot_identity(&self, app: &App, headers: &HeaderMap, body: Value) -> Result<Value> {
        self.validate_host(headers, true)?;
        let connection =
            serde_json::from_value::<Uuid>(body["connection_id"].clone()).map_err(|_| bad())?;
        match body["action"].as_str() {
            Some("link") => {
                if self.active_session(app, headers).await?.1 {
                    return Err(forbidden());
                }
                let grant: Uuid = sqlx::query_scalar("INSERT INTO standalone_bot_grants(id,connection_id,owner_id)
                    SELECT $1,id,$3 FROM connections WHERE id=$2 AND workspace=$4 AND status='connected'
                    ON CONFLICT (connection_id,owner_id) DO UPDATE SET id=standalone_bot_grants.id RETURNING id")
                    .bind(Uuid::new_v4()).bind(connection).bind(self.user).bind(self.workspace).fetch_one(&app.db).await?;
                Ok(json!({"grant":grant}))
            }
            Some("delegate") => {
                let grant =
                    serde_json::from_value::<Uuid>(body["grant"].clone()).map_err(|_| bad())?;
                let token = session_token();
                let result = sqlx::query("INSERT INTO standalone_sessions(token_hash,workspace,owner_id,expires_at,bot_grant)
                    SELECT $1,c.workspace,g.owner_id,now()+interval '30 minutes',g.id
                    FROM standalone_bot_grants g JOIN connections c ON c.id=g.connection_id
                    WHERE g.id=$2 AND c.id=$3 AND g.owner_id=$4 AND c.workspace=$5 AND c.status='connected' AND c.enabled
                      AND EXISTS(SELECT FROM external_links l WHERE l.connection_id=c.id AND l.user_id=g.owner_id AND l.control_grant=g.id)")
                    .bind(connections::digest(&token)).bind(grant).bind(connection).bind(self.user).bind(self.workspace).execute(&app.db).await?;
                if result.rows_affected() != 1 {
                    return Err(forbidden());
                }
                Ok(json!({"token":token,"user_id":self.user}))
            }
            _ => Err(bad()),
        }
    }
}

fn session_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn unauthorized() -> Error {
    Error(
        StatusCode::UNAUTHORIZED,
        "Accedi a Venticode / Sign in to Venticode".into(),
    )
}

#[derive(Deserialize)]
pub(super) struct SignIn {
    browser: Option<String>,
}

pub(super) async fn form(
    State(app): State<App>,
    h: HeaderMap,
    Query(request): Query<SignIn>,
) -> Result<Response> {
    let Some(local) = &app.standalone else {
        let portal = env::var("PORTAL_ORIGIN")
            .ok()
            .and_then(|origin| reqwest::Url::parse(origin.trim()).ok())
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(|url| url.origin().ascii_serialization())
            .ok_or_else(missing)?;
        if request.browser.as_deref() == Some("1")
            && let Some(workspace) = h
                .get(header::HOST)
                .and_then(|host| host.to_str().ok())
                .and_then(|host| host.split_once('.'))
                .and_then(|(id, _)| id.parse::<Uuid>().ok())
        {
            return Ok(Redirect::to(&format!(
                "{portal}/app/venticode?standalone=1&browser=1&workspace={workspace}",
            ))
            .into_response());
        }
        return Ok(Redirect::to(&portal).into_response());
    };
    local.validate_host(&h, false)?;
    if local.authorize(&app, &h).await.is_ok() {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(login_page(&h, false).into_response())
}

fn login_page(h: &HeaderMap, failed: bool) -> Html<String> {
    let english = h
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("en"));
    let (lang, title, token, button, error) = if english {
        (
            "en",
            "Sign in to Venticode",
            "Access token",
            "Sign in",
            "Invalid access token",
        )
    } else {
        (
            "it",
            "Accedi a Venticode",
            "Token di accesso",
            "Accedi",
            "Token di accesso non valido",
        )
    };
    let notice = if failed {
        format!(r#"<p class="error" role="alert">{error}</p>"#)
    } else {
        String::new()
    };
    Html(format!(
        r#"<!doctype html><html lang="{lang}"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>:root{{color-scheme:light;--bg:#f5f4f0;--surface:#ffffff;--ink:#111318;--muted:#5d6066;--line:#d5d6d3;--accent:#f5a83f;--accent-ink:#16181b;--bad:#ac3039;--bad-soft:#fcebed}}@media(prefers-color-scheme:dark){{:root{{color-scheme:dark;--bg:#0e0f11;--surface:#16181b;--ink:#f2f0eb;--muted:#b7b5ae;--line:#32373d;--bad:#ff8c95;--bad-soft:#321f23}}}}*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;background:var(--bg);color:var(--ink);font:14px/1.5 Inter,"Segoe UI",system-ui,sans-serif}}main{{width:min(24rem,100%);background:var(--surface);border:1px solid var(--line);padding:24px;display:flex;flex-direction:column;gap:16px}}.mark{{width:32px;height:32px;background:var(--accent);color:var(--accent-ink);display:grid;place-items:center;font:600 13px ui-monospace,Consolas,monospace}}h1{{margin:0;font-size:20px;font-weight:600;letter-spacing:-.01em}}form{{display:grid;gap:12px}}label{{font-size:13px;color:var(--muted)}}input{{font:inherit;padding:10px 12px;background:var(--bg);color:var(--ink);border:1px solid var(--line);border-radius:0;width:100%}}input:focus-visible,button:focus-visible{{outline:2px solid var(--accent);outline-offset:2px}}button{{font:600 14px/1 inherit;min-height:42px;padding:0 16px;background:var(--accent);color:var(--accent-ink);border:0;border-radius:0;cursor:pointer}}button:hover{{filter:brightness(1.06)}}.error{{margin:0;padding:10px 12px;font-size:13px;color:var(--bad);background:var(--bad-soft);border:1px solid var(--bad)}}</style><main><div class="mark" aria-hidden="true">&gt;_</div><h1>{title}</h1>{notice}<form method="post" action="/venticode/login"><label for="token">{token}</label><input id="token" name="token" type="password" autocomplete="current-password" maxlength="1024" required autofocus><button type="submit">{button}</button></form></main></html>"#
    ))
}

#[derive(Deserialize)]
pub(super) struct Credentials {
    token: String,
}

pub(super) async fn login(
    State(app): State<App>,
    h: HeaderMap,
    Form(body): Form<Credentials>,
) -> Result<Response> {
    let local = app.standalone.as_ref().ok_or_else(missing)?;
    local.validate_host(&h, true)?;
    if local.login_rate_limited(&h) {
        tracing::warn!("standalone login rate limit reached");
        return Err(Error(
            StatusCode::TOO_MANY_REQUESTS,
            "Troppi tentativi di accesso. Riprova tra un minuto / Too many sign-in attempts. Try again in a minute".into(),
        ));
    }
    if body.token.len() > 1024
        || !bool::from(
            local
                .login_hash
                .ct_eq(Sha256::digest(body.token.as_bytes()).as_slice()),
        )
    {
        local.record_failed_login(&h);
        tracing::warn!("standalone login rejected: invalid access token");
        return Ok((StatusCode::UNAUTHORIZED, login_page(&h, true)).into_response());
    }
    let token = session_token();
    let mut tx = app.db.begin().await?;
    if let Ok(old) = local.session_hash(&h) {
        sqlx::query(
            "DELETE FROM standalone_sessions WHERE token_hash=$1 AND workspace=$2 AND owner_id=$3",
        )
        .bind(old)
        .bind(local.workspace)
        .bind(local.user)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("INSERT INTO standalone_sessions(token_hash,workspace,owner_id,expires_at) VALUES($1,$2,$3,now()+interval '12 hours')")
        .bind(connections::digest(&token)).bind(local.workspace).bind(local.user).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((
        [(header::SET_COOKIE, local.cookie(&token, 43_200))],
        Redirect::to("/"),
    )
        .into_response())
}
