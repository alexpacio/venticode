use anyhow::{Context, Result, ensure};
use asupersync::Cx;
use asupersync::bytes::Buf;
use asupersync::channel::mpsc::Sender;
use asupersync::http::body::{Body, Frame};
use asupersync::http::h1::{HttpClient, Method};
use asupersync::time::{sleep, timeout, wall_now};
use asupersync::tls::Certificate;
use serde_json::Value;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;
use url::Url;

pub enum StreamUpdate {
    Event(Value),
    Status(String),
    AuthExpired(String),
    Fatal(anyhow::Error),
}

#[derive(Clone)]
pub struct Remote {
    client: HttpClient,
    pub workspace: String,
    cookie: String,
}

pub fn origin(input: &str) -> Result<String> {
    validate_origin(input, false)
}

pub fn standalone_origin(input: &str) -> Result<String> {
    validate_origin(input, true)
}

fn validate_origin(input: &str, allow_loopback: bool) -> Result<String> {
    let url = Url::parse(input)?;
    let local = allow_loopback
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    ensure!(
        (url.scheme() == "https" || local)
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "HTTPS origin required"
    );
    Ok(url.origin().ascii_serialization())
}

fn parse_status_error(status: u16, body: &[u8]) -> anyhow::Error {
    if let Ok(value) = serde_json::from_slice::<Value>(body)
        && let Some(msg) = value["error"]["message"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
    {
        return anyhow::anyhow!("{status}: {msg}");
    }
    let snippet: String = String::from_utf8_lossy(body)
        .trim()
        .chars()
        .take(160)
        .collect();
    if snippet.is_empty() {
        anyhow::anyhow!("HTTP {status}")
    } else {
        anyhow::anyhow!("HTTP {status}: {snippet}")
    }
}

fn form_encode(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish().into_bytes()
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
}

fn jittered_backoff(base_secs: u64) -> Duration {
    let base_ms = base_secs.saturating_mul(1000);
    let span_ms = (base_ms / 2).max(1);
    let rand =
        std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), base_ms);
    let min_ms = base_ms - base_ms / 4;
    Duration::from_millis((min_ms + rand % span_ms).clamp(500, 30_000))
}

impl Remote {
    fn http(ca: Option<&std::path::Path>, workspace: &str) -> Result<HttpClient> {
        let mut builder = HttpClient::builder()
            .no_redirects()
            .no_cookie_store()
            .request_timeout(Duration::from_secs(35));
        let url = Url::parse(workspace)?;
        if url.host_str().is_some_and(is_loopback_host) {
            builder = builder.no_proxy();
        }
        if let Some(path) = ca {
            for cert in Certificate::from_pem_file(path)
                .map_err(|e| anyhow::anyhow!("Invalid CA PEM {}: {e}", path.display()))?
            {
                builder = builder.add_root_certificate(cert);
            }
        }
        Ok(builder.build())
    }

    pub fn saved(&self) -> Result<String> {
        Ok(serde_json::json!({"url": self.workspace, "cookie": self.cookie}).to_string())
    }

    pub fn restore(
        ca: Option<&std::path::Path>,
        saved: &str,
        allow_loopback: bool,
    ) -> Result<Self> {
        let value: Value = serde_json::from_str(saved)?;
        let workspace = validate_origin(
            value["url"].as_str().context("Missing workspace")?,
            allow_loopback,
        )?;
        let client = Self::http(ca, &workspace)?;
        let cookie = value["cookie"]
            .as_str()
            .context("Missing cookie")?
            .to_owned();
        ensure!(
            !cookie.is_empty() && !cookie.contains(['\r', '\n']),
            "Invalid cookie"
        );
        Ok(Self {
            client,
            workspace,
            cookie,
        })
    }

    fn auth_headers(&self) -> Vec<(String, String)> {
        vec![
            ("Origin".into(), self.workspace.clone()),
            ("Cookie".into(), self.cookie.clone()),
        ]
    }

    pub async fn valid(&self, cx: &Cx) -> Result<bool> {
        let res = self
            .client
            .request_with_timeout(
                cx,
                Method::Get,
                &format!("{}/api/v1/venticode/session", self.workspace),
                self.auth_headers(),
                Vec::new(),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Connection failed: {e}"))?;
        if res.status == 401 {
            return Ok(false);
        }
        if !(200..300).contains(&res.status) {
            return Err(parse_status_error(res.status, &res.body));
        }
        Ok(true)
    }

    pub async fn connect_standalone(
        cx: &Cx,
        ca: Option<&std::path::Path>,
        address: &str,
        token: &str,
    ) -> Result<Self> {
        let workspace = standalone_origin(address)?;
        let client = Self::http(ca, &workspace)?;
        let headers = vec![
            ("Origin".into(), workspace.clone()),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ];
        let res = client
            .request_with_timeout(
                cx,
                Method::Post,
                &format!("{workspace}/venticode/login"),
                headers,
                form_encode(&[("token", token)]),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Sign-in request failed: {e}"))?;
        Self::authenticated(client, workspace, res.status, &res.headers)
    }

    pub async fn connect(
        cx: &Cx,
        ca: Option<&std::path::Path>,
        portal: &str,
        code: &str,
    ) -> Result<Self> {
        let link: Value = serde_json::from_str(code).context("Invalid one-time code")?;
        let workspace = origin(link["url"].as_str().context("Missing workspace")?)?;
        let client = Self::http(ca, &workspace)?;
        let ticket = link["ticket"].as_str().context("Missing ticket")?;
        let headers = vec![
            ("Origin".into(), origin(portal)?),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ];
        let res = client
            .request_with_timeout(
                cx,
                Method::Post,
                &format!("{workspace}/venticode/bootstrap"),
                headers,
                form_encode(&[("ticket", ticket)]),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Bootstrap request failed: {e}"))?;
        Self::authenticated(client, workspace, res.status, &res.headers)
    }

    fn authenticated(
        client: HttpClient,
        workspace: String,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<Self> {
        ensure!(status == 303, "Sign-in failed ({status})");
        let raw = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .filter_map(|(_, v)| v.split(';').next())
            .find(|s| s.starts_with("venticode_session="))
            .context("Missing workspace session")?
            .to_owned();
        Ok(Self {
            client,
            workspace,
            cookie: raw,
        })
    }

    pub async fn api(
        &self,
        cx: &Cx,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut headers = self.auth_headers();
        let bytes = if let Some(data) = body {
            headers.push(("Content-Type".into(), "application/json".into()));
            serde_json::to_vec(&data)?
        } else {
            Vec::new()
        };
        let res = self
            .client
            .request_with_timeout(
                cx,
                method,
                &format!("{}/api/v1/venticode{path}", self.workspace),
                headers,
                bytes,
                Duration::from_secs(35),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Request failed: {e}"))?;
        if res.status == 204 {
            return Ok(Value::Null);
        }
        if !(200..300).contains(&res.status) {
            return Err(parse_status_error(res.status, &res.body));
        }
        serde_json::from_slice(&res.body).context("Invalid JSON response from Venticode")
    }

    pub async fn list(&self, cx: &Cx, path: &str) -> Result<Vec<Value>> {
        self.api(cx, Method::Get, path, None).await?["items"]
            .as_array()
            .cloned()
            .context("Expected list")
    }

    pub async fn follow(
        &self,
        cx: Cx,
        id: String,
        start_cursor: i64,
        en: bool,
        tx: Sender<(String, StreamUpdate)>,
    ) {
        let mut cursor: i64 = start_cursor;
        let mut backoff_secs: u64 = 1;
        loop {
            if cx.is_cancel_requested() {
                break;
            }
            match self
                .follow_once(&cx, &id, &mut cursor, en, &tx, &mut backoff_secs)
                .await
            {
                FollowOutcome::Cancelled => break,
                FollowOutcome::AuthExpired(msg) => {
                    let _ = tx
                        .send(&cx, (id.clone(), StreamUpdate::AuthExpired(msg)))
                        .await;
                    break;
                }
                FollowOutcome::Fatal(error) => {
                    let _ = tx.send(&cx, (id.clone(), StreamUpdate::Fatal(error))).await;
                    break;
                }
                FollowOutcome::Reconnect(reason) => {
                    if cx.is_cancel_requested() {
                        break;
                    }
                    let delay = jittered_backoff(backoff_secs);
                    let secs = delay.as_millis().div_ceil(1000).max(1);
                    let msg = if en {
                        format!("Stream interrupted ({reason}); reconnecting in {secs}s…")
                    } else {
                        format!("Stream interrotto ({reason}); riconnessione tra {secs}s…")
                    };
                    let _ = tx.send(&cx, (id.clone(), StreamUpdate::Status(msg))).await;
                    sleep(wall_now(), delay).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                }
            }
        }
    }

    async fn follow_once(
        &self,
        cx: &Cx,
        id: &str,
        cursor: &mut i64,
        en: bool,
        tx: &Sender<(String, StreamUpdate)>,
        backoff_secs: &mut u64,
    ) -> FollowOutcome {
        let mut headers = self.auth_headers();
        headers.push(("Accept".into(), "text/event-stream".into()));
        if *cursor > 0 {
            headers.push(("Last-Event-ID".into(), cursor.to_string()));
        }
        let mut stream = match self
            .client
            .request_streaming(
                cx,
                Method::Get,
                &format!(
                    "{}/api/v1/venticode/conversations/{id}/stream?after={cursor}",
                    self.workspace
                ),
                headers,
                Vec::new(),
            )
            .await
        {
            Ok(stream) => stream,
            Err(err) => return FollowOutcome::Reconnect(err.to_string()),
        };
        let status = stream.head.status;
        if status == 401 {
            return FollowOutcome::AuthExpired(if en {
                "Session expired (401)".into()
            } else {
                "Sessione scaduta (401)".into()
            });
        }
        if matches!(status, 403 | 404) {
            let msg = if en {
                format!("Session unavailable ({status})")
            } else {
                format!("Sessione non disponibile ({status})")
            };
            return FollowOutcome::Fatal(anyhow::anyhow!(msg));
        }
        if !(200..300).contains(&status) {
            return FollowOutcome::Reconnect(format!("HTTP {status}"));
        }
        *backoff_secs = 1;
        let _ = tx
            .send(cx, (id.to_owned(), StreamUpdate::Status(String::new())))
            .await;

        let mut parser = SseParser::default();
        loop {
            if cx.is_cancel_requested() {
                return FollowOutcome::Cancelled;
            }
            let next = timeout(
                wall_now(),
                Duration::from_secs(45),
                next_body_chunk(&mut stream.body),
            )
            .await;
            let chunk = match next {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(None) => {
                    return FollowOutcome::Reconnect(if en {
                        "stream closed".into()
                    } else {
                        "stream chiuso".into()
                    });
                }
                Ok(Some(Err(err))) => return FollowOutcome::Reconnect(err.to_string()),
                Err(_) => {
                    return FollowOutcome::Reconnect(if en {
                        "stream timeout".into()
                    } else {
                        "timeout dello stream".into()
                    });
                }
            };
            let parsed = parser.push(&chunk);
            if parsed.overflowed {
                let msg = if en {
                    "Skipped oversized stream frame".to_owned()
                } else {
                    "Frame stream troppo grande ignorato".to_owned()
                };
                let _ = tx
                    .send(cx, (id.to_owned(), StreamUpdate::Status(msg)))
                    .await;
            }
            for ev in parsed.events {
                if ev.event == "authorization_expired" {
                    return FollowOutcome::AuthExpired(if en {
                        "Session expired".into()
                    } else {
                        "Sessione scaduta".into()
                    });
                }
                if ev.event != "transcript" {
                    continue;
                }
                let v: Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(err) => {
                        let msg = if en {
                            format!("Skipped malformed stream event ({err})")
                        } else {
                            format!("Evento stream non valido ignorato ({err})")
                        };
                        let _ = tx
                            .send(cx, (id.to_owned(), StreamUpdate::Status(msg)))
                            .await;
                        continue;
                    }
                };
                let Some(seq) = v["sequence"].as_i64() else {
                    let msg = if en {
                        "Skipped stream event without sequence".to_owned()
                    } else {
                        "Evento stream senza sequenza ignorato".to_owned()
                    };
                    let _ = tx
                        .send(cx, (id.to_owned(), StreamUpdate::Status(msg)))
                        .await;
                    continue;
                };
                if seq <= *cursor {
                    continue;
                }
                *cursor = seq;
                if tx
                    .send(cx, (id.to_owned(), StreamUpdate::Event(v)))
                    .await
                    .is_err()
                {
                    return FollowOutcome::Cancelled;
                }
            }
        }
    }
}

enum FollowOutcome {
    Reconnect(String),
    AuthExpired(String),
    Fatal(anyhow::Error),
    Cancelled,
}

async fn next_body_chunk<B>(body: &mut B) -> Option<Result<Vec<u8>, B::Error>>
where
    B: Body + Unpin,
{
    std::future::poll_fn(|task_cx| match Pin::new(&mut *body).poll_frame(task_cx) {
        Poll::Ready(Some(Ok(Frame::Data(mut data)))) => {
            let mut out = Vec::with_capacity(data.remaining());
            while data.has_remaining() {
                let slice = data.chunk();
                out.extend_from_slice(slice);
                let len = slice.len();
                data.advance(len);
            }
            Poll::Ready(Some(Ok(out)))
        }
        Poll::Ready(Some(Ok(Frame::Trailers(_)))) => Poll::Ready(Some(Ok(Vec::new()))),
        Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
        Poll::Ready(None) => Poll::Ready(None),
        Poll::Pending => Poll::Pending,
    })
    .await
}

const MAX_SSE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const MAX_SSE_DATA_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct SseEvent {
    event: String,
    data: String,
    id: Option<String>,
}

struct SsePushResult {
    events: Vec<SseEvent>,
    overflowed: bool,
}

struct SseEventState {
    event: String,
    data: Vec<String>,
    data_bytes: usize,
    last_event_id: Option<String>,
    discarding_event: bool,
    max_data: usize,
}

impl SseEventState {
    fn new(max_data: usize) -> Self {
        Self {
            event: String::new(),
            data: Vec::new(),
            data_bytes: 0,
            last_event_id: None,
            discarding_event: false,
            max_data,
        }
    }

    fn abort_event(&mut self) {
        self.event.clear();
        self.data.clear();
        self.data_bytes = 0;
        self.discarding_event = true;
    }

    fn process_line(&mut self, raw: &[u8], out: &mut Vec<SseEvent>, overflowed: &mut bool) {
        let text = String::from_utf8_lossy(raw);
        if text.is_empty() {
            if self.discarding_event {
                self.discarding_event = false;
                self.event.clear();
                self.data.clear();
                self.data_bytes = 0;
            } else if !self.data.is_empty() {
                let event = if self.event.is_empty() {
                    "message".to_owned()
                } else {
                    std::mem::take(&mut self.event)
                };
                let data = std::mem::take(&mut self.data).join("\n");
                self.data_bytes = 0;
                out.push(SseEvent {
                    event,
                    data,
                    id: self.last_event_id.clone(),
                });
            } else {
                self.event.clear();
            }
            return;
        }
        if self.discarding_event {
            return;
        }
        if let Some(rest) = text.strip_prefix("event:") {
            self.event = rest.strip_prefix(' ').unwrap_or(rest).to_owned();
        } else if let Some(rest) = text.strip_prefix("data:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            let added = val.len() + usize::from(!self.data.is_empty());
            if self.data_bytes.saturating_add(added) > self.max_data {
                self.abort_event();
                *overflowed = true;
            } else {
                self.data.push(val.to_owned());
                self.data_bytes += added;
            }
        } else if let Some(rest) = text.strip_prefix("id:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            if !val.contains('\0') {
                self.last_event_id = Some(val.to_owned());
            }
        }
    }
}

struct SseParser {
    buf: Vec<u8>,
    state: SseEventState,
    skip_leading_lf: bool,
    discarding_line: bool,
    max_buf: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_limits(MAX_SSE_BUFFER_BYTES, MAX_SSE_DATA_BYTES)
    }
}

impl SseParser {
    fn with_limits(max_buf: usize, max_data: usize) -> Self {
        Self {
            buf: Vec::new(),
            state: SseEventState::new(max_data),
            skip_leading_lf: false,
            discarding_line: false,
            max_buf: max_buf.max(1),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> SsePushResult {
        let mut out = Vec::new();
        let mut overflowed = false;
        let mut remaining = bytes;

        while !remaining.is_empty() {
            if self.skip_leading_lf {
                self.skip_leading_lf = false;
                if remaining[0] == b'\n' {
                    remaining = &remaining[1..];
                    continue;
                }
            }
            if self.discarding_line {
                if let Some(pos) = remaining.iter().position(|&b| b == b'\n' || b == b'\r') {
                    let sep = remaining[pos];
                    remaining = &remaining[pos + 1..];
                    if sep == b'\r' {
                        if remaining.first() == Some(&b'\n') {
                            remaining = &remaining[1..];
                        } else if remaining.is_empty() {
                            self.skip_leading_lf = true;
                        }
                    }
                    self.discarding_line = false;
                    continue;
                }
                break;
            }

            let space = self.max_buf.saturating_sub(self.buf.len());
            let take = remaining.len().min(space);
            self.buf.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];

            let mut cursor = 0usize;
            while cursor < self.buf.len() {
                if self.skip_leading_lf {
                    self.skip_leading_lf = false;
                    if self.buf[cursor] == b'\n' {
                        cursor += 1;
                        continue;
                    }
                }
                let Some(rel) = self.buf[cursor..]
                    .iter()
                    .position(|&b| b == b'\n' || b == b'\r')
                else {
                    break;
                };
                let line_end = cursor + rel;
                let sep = self.buf[line_end];
                self.state
                    .process_line(&self.buf[cursor..line_end], &mut out, &mut overflowed);
                cursor = line_end + 1;
                if sep == b'\r' {
                    if cursor < self.buf.len() {
                        if self.buf[cursor] == b'\n' {
                            cursor += 1;
                        }
                    } else {
                        self.skip_leading_lf = true;
                    }
                }
            }
            if cursor > 0 {
                self.buf.drain(..cursor);
            }

            if self.buf.len() >= self.max_buf && !remaining.is_empty() {
                if matches!(remaining[0], b'\n' | b'\r') {
                    let sep = remaining[0];
                    remaining = &remaining[1..];
                    if sep == b'\r' {
                        if remaining.first() == Some(&b'\n') {
                            remaining = &remaining[1..];
                        } else if remaining.is_empty() {
                            self.skip_leading_lf = true;
                        }
                    }
                    self.state
                        .process_line(&self.buf, &mut out, &mut overflowed);
                    self.buf.clear();
                } else {
                    self.buf.clear();
                    self.state.abort_event();
                    self.discarding_line = true;
                    overflowed = true;
                }
            }
        }

        SsePushResult {
            events: out,
            overflowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Remote, SseEvent, SseParser, is_loopback_host, jittered_backoff, origin};
    use std::time::Duration;

    #[test]
    fn bootstrap_uses_an_https_origin() {
        assert_eq!(
            origin("https://portal.example:8443/").unwrap(),
            "https://portal.example:8443"
        );
        for input in [
            "http://portal.example",
            "https://user@portal.example",
            "https://portal.example/app",
            "https://portal.example/?ticket=1",
            "https://portal.example/#fragment",
        ] {
            assert!(origin(input).is_err(), "{input}");
        }
    }

    #[test]
    fn loopback_hosts_bypass_proxy() {
        for host in ["localhost", "app.localhost", "127.0.0.1", "[::1]", "::1"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["example.com", "127.0.0.2", "notlocalhost"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn restore_enforces_https_for_portal_sessions() {
        let saved_http =
            serde_json::json!({"url": "http://127.0.0.1:8080", "cookie": "venticode_session=abc"})
                .to_string();
        assert!(Remote::restore(None, &saved_http, false).is_err());
        assert!(Remote::restore(None, &saved_http, true).is_ok());

        let saved_https = serde_json::json!({
            "url": "https://workspace.example",
            "cookie": "venticode_session=abc"
        })
        .to_string();
        assert!(Remote::restore(None, &saved_https, false).is_ok());
    }

    #[test]
    fn jittered_backoff_stays_within_bounds() {
        for base in [1, 2, 4, 8, 16, 30] {
            let d = jittered_backoff(base);
            assert!(d >= Duration::from_millis(500));
            assert!(d <= Duration::from_secs(30));
        }
    }

    #[test]
    fn sse_parser_handles_split_chunks_and_events() {
        let mut p = SseParser::default();
        let r1 = p.push(b"id: 7\r\nevent: transcript\r\ndata: {\"a\":");
        assert!(r1.events.is_empty());
        assert!(!r1.overflowed);
        let r2 = p.push(b"1}\r\n\r\nevent: heartbeat\ndata: {}\n\n");
        assert!(!r2.overflowed);
        assert_eq!(
            r2.events,
            vec![
                SseEvent {
                    event: "transcript".into(),
                    data: "{\"a\":1}".into(),
                    id: Some("7".into()),
                },
                SseEvent {
                    event: "heartbeat".into(),
                    data: "{}".into(),
                    id: Some("7".into()),
                },
            ]
        );
    }

    #[test]
    fn sse_parser_handles_bare_cr_and_split_crlf() {
        let mut p = SseParser::default();
        let r1 = p.push(b"id: 1\rdata: hello\r\r");
        assert_eq!(
            r1.events,
            vec![SseEvent {
                event: "message".into(),
                data: "hello".into(),
                id: Some("1".into()),
            }]
        );
        // Split CRLF across chunk boundary must not synthesize an extra blank line.
        let r2 = p.push(b"data: part1\r");
        assert!(r2.events.is_empty());
        let r3 = p.push(b"\ndata: part2\r\n\r\n");
        assert_eq!(
            r3.events,
            vec![SseEvent {
                event: "message".into(),
                data: "part1\npart2".into(),
                id: Some("1".into()),
            }]
        );
    }

    #[test]
    fn sse_parser_compacts_cursor_and_enforces_oversized_line_and_data_caps() {
        let mut p = SseParser::with_limits(32, 32);
        // Many events in one chunk compact buffer down to trailing incomplete fragment.
        let mut chunk = Vec::new();
        for i in 0..50 {
            chunk.extend_from_slice(format!("data: {i}\n\n").as_bytes());
        }
        chunk.extend_from_slice(b"data: tail");
        let r = p.push(&chunk);
        assert!(!r.overflowed);
        assert_eq!(r.events.len(), 50);
        assert_eq!(p.buf, b"data: tail");
        let r_tail = p.push(b"\n\n");
        assert_eq!(r_tail.events.len(), 1);
        assert_eq!(r_tail.events[0].data, "tail");
        assert!(p.buf.is_empty());

        // Oversized single line (> 32 bytes) drops that event and recovers on next valid event.
        let mut long_chunk = Vec::new();
        long_chunk.extend_from_slice(b"event: transcript\ndata: ");
        long_chunk.extend_from_slice(&[b'x'; 80]);
        long_chunk.extend_from_slice(b"\n\nevent: transcript\ndata: ok\n\n");
        let r_over = p.push(&long_chunk);
        assert!(r_over.overflowed);
        assert!(p.buf.len() <= 32);
        assert_eq!(
            r_over.events,
            vec![SseEvent {
                event: "transcript".into(),
                data: "ok".into(),
                id: None,
            }]
        );

        // Oversized accumulated multi-line data (> 32 bytes) drops that event and recovers.
        let r_data = p.push(b"data: 0123456789\ndata: 0123456789\ndata: 0123456789\ndata: 0123456789\n\ndata: after\n\n");
        assert!(r_data.overflowed);
        assert_eq!(
            r_data.events,
            vec![SseEvent {
                event: "message".into(),
                data: "after".into(),
                id: None,
            }]
        );
    }
}
