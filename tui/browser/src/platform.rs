//! Web platform for the shared TUI core: fetch, EventSource and WebSocket
//! from the browser, plus hooks into the page for clipboard, navigation and
//! the remote-screen image overlay.

use crate::app::ScreenFrame;
use anyhow::{Context, Result, anyhow};
use ftui::Frame;
use ftui::core::geometry::Rect;
use ftui::text::Line;
use js_sys::{Function, Reflect, Uint8Array};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    BinaryType, EventSource, MessageEvent, Request, RequestCredentials, RequestInit,
    RequestRedirect, Response, WebSocket,
};

pub const CAN_QUIT: bool = false;

/// (cells, serial, jpeg, remote width, remote height)
pub type Overlay = (Rect, u64, std::sync::Arc<Vec<u8>>, f64, f64);
const API: &str = "/api/v1/venticode";

thread_local! {
    static HOOKS: RefCell<JsValue> = const { RefCell::new(JsValue::UNDEFINED) };
    static DEV: Cell<bool> = const { Cell::new(false) };
    /// Remote-screen image drawn in the last frame: (area, serial, frame).
    pub static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

pub fn install(hooks: JsValue, dev: bool) {
    HOOKS.with(|h| *h.borrow_mut() = hooks);
    DEV.with(|d| d.set(dev));
}

/// Call `hooks[name](...args)` if the page provided it.
pub fn hook(name: &str, args: &[JsValue]) {
    HOOKS.with(|hooks| {
        let hooks = hooks.borrow();
        if let Ok(f) = Reflect::get(&hooks, &JsValue::from_str(name))
            && let Ok(f) = f.dyn_into::<Function>()
        {
            let array = js_sys::Array::new();
            for arg in args {
                array.push(arg);
            }
            let _ = f.apply(&JsValue::NULL, &array);
        }
    });
}

pub fn dev() -> bool {
    DEV.with(Cell::get)
}

#[derive(Clone)]
pub struct Cx;

#[derive(Clone, Copy)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

pub enum StreamUpdate {
    Event(Value),
    Status(String),
    AuthExpired(String),
    // Matched by the shared core; EventSource retries instead of failing.
    #[allow(dead_code)]
    Fatal(String),
}

pub enum SocketEvent {
    Text(String),
    Binary(Vec<u8>),
    Closed,
}

pub enum Update {
    Stream(String, StreamUpdate),
    Socket(u64, SocketEvent),
}

pub type Tx = Rc<RefCell<VecDeque<Update>>>;

pub fn channel() -> (Tx, Tx) {
    let queue = Tx::default();
    (queue.clone(), queue)
}

fn js_error(e: JsValue) -> anyhow::Error {
    anyhow!(
        "{}",
        e.as_string()
            .or_else(|| Reflect::get(&e, &"message".into())
                .ok()
                .and_then(|m| m.as_string()))
            .unwrap_or_else(|| "network error".into())
    )
}

fn urlencode(value: &str) -> String {
    js_sys::encode_uri_component(value).into()
}

#[derive(Clone)]
pub struct Remote {
    pub workspace: String,
}

impl Remote {
    pub async fn api(
        &self,
        _cx: &Cx,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let init = RequestInit::new();
        init.set_method(method.name());
        init.set_credentials(RequestCredentials::SameOrigin);
        let request = match body {
            Some(body) => {
                init.set_body(&JsValue::from_str(&body.to_string()));
                let request = Request::new_with_str_and_init(&format!("{API}{path}"), &init)
                    .map_err(js_error)?;
                request
                    .headers()
                    .set("Content-Type", "application/json")
                    .map_err(js_error)?;
                request
            }
            None => {
                Request::new_with_str_and_init(&format!("{API}{path}"), &init).map_err(js_error)?
            }
        };
        let window = web_sys::window().context("window")?;
        let response: Response = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| anyhow!("Request failed: {}", js_error(e)))?
            .dyn_into()
            .map_err(js_error)?;
        let status = response.status();
        if status == 204 {
            return Ok(Value::Null);
        }
        let text = JsFuture::from(response.text().map_err(js_error)?)
            .await
            .map_err(js_error)?
            .as_string()
            .unwrap_or_default();
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| text.chars().take(160).collect());
            return Err(anyhow!("{status}: {message}"));
        }
        serde_json::from_str(&text).context("Invalid JSON response from Venticode")
    }

    pub async fn list(&self, cx: &Cx, path: &str) -> Result<Vec<Value>> {
        self.api(cx, Method::Get, path, None).await?["items"]
            .as_array()
            .cloned()
            .context("Expected list")
    }

    pub async fn list_dir(
        &self,
        cx: &Cx,
        harness: &str,
        root: &str,
        path: &str,
    ) -> Result<Vec<Value>> {
        let mut url = format!("/harnesses/{harness}/ls?root={}", urlencode(root));
        if path != "." {
            url.push_str(&format!("&path={}", urlencode(path)));
        }
        self.list(cx, &url).await
    }
}

/// A conversation stream; the browser's EventSource reconnects by itself.
pub struct Feed {
    source: EventSource,
    _handlers: Vec<Closure<dyn FnMut(JsValue)>>,
}

impl Feed {
    pub fn stop(self) {
        self.source.close();
    }
}

pub fn follow(
    _cx: &Cx,
    _remote: &Remote,
    id: String,
    cursor: i64,
    en: bool,
    tx: Tx,
) -> Option<Feed> {
    let source =
        EventSource::new(&format!("{API}/conversations/{id}/stream?after={cursor}")).ok()?;
    let mut handlers = Vec::new();
    let push = |tx: &Tx, id: &str, update: StreamUpdate| {
        tx.borrow_mut()
            .push_back(Update::Stream(id.to_owned(), update))
    };
    {
        let (tx, id) = (tx.clone(), id.clone());
        let f = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            let Some(data) = event
                .dyn_ref::<MessageEvent>()
                .and_then(|e| e.data().as_string())
            else {
                return;
            };
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                push(&tx, &id, StreamUpdate::Event(value));
            }
        });
        let _ = source.add_event_listener_with_callback("transcript", f.as_ref().unchecked_ref());
        handlers.push(f);
    }
    {
        let (tx, id) = (tx.clone(), id.clone());
        let f = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            let msg = if en {
                "Session expired"
            } else {
                "Sessione scaduta"
            };
            push(&tx, &id, StreamUpdate::AuthExpired(msg.into()));
        });
        let _ = source
            .add_event_listener_with_callback("authorization_expired", f.as_ref().unchecked_ref());
        handlers.push(f);
    }
    {
        let (tx, id) = (tx.clone(), id.clone());
        let f = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            let msg = if en {
                "Connection lost, reconnecting…"
            } else {
                "Connessione persa, riconnessione in corso…"
            };
            push(&tx, &id, StreamUpdate::Status(msg.into()));
        });
        source.set_onerror(Some(f.as_ref().unchecked_ref()));
        handlers.push(f);
    }
    {
        let f = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            push(&tx, &id, StreamUpdate::Status(String::new()))
        });
        source.set_onopen(Some(f.as_ref().unchecked_ref()));
        handlers.push(f);
    }
    Some(Feed {
        source,
        _handlers: handlers,
    })
}

pub struct Socket {
    ws: WebSocket,
    _handlers: Vec<Closure<dyn FnMut(JsValue)>>,
}

impl Socket {
    pub fn send(&self, text: String) -> bool {
        self.ws.ready_state() == WebSocket::OPEN
            && self.ws.buffered_amount() < 64 * 1024
            && self.ws.send_with_str(&text).is_ok()
    }
    pub fn close(self) {
        let _ = self.ws.close();
    }
}

pub fn socket(_cx: &Cx, _remote: &Remote, path: &str, id: u64, tx: Tx) -> Result<Socket> {
    let location = web_sys::window().context("window")?.location();
    let scheme = if location.protocol().map_err(js_error)? == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = location.host().map_err(js_error)?;
    let ws = WebSocket::new(&format!("{scheme}://{host}{API}{path}")).map_err(js_error)?;
    ws.set_binary_type(BinaryType::Arraybuffer);
    let mut handlers = Vec::new();
    {
        let tx = tx.clone();
        let f = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            let Some(event) = event.dyn_ref::<MessageEvent>() else {
                return;
            };
            let data = event.data();
            let update = if let Some(text) = data.as_string() {
                SocketEvent::Text(text)
            } else if data.is_instance_of::<js_sys::ArrayBuffer>() {
                SocketEvent::Binary(Uint8Array::new(&data).to_vec())
            } else {
                return;
            };
            tx.borrow_mut().push_back(Update::Socket(id, update));
        });
        ws.set_onmessage(Some(f.as_ref().unchecked_ref()));
        handlers.push(f);
    }
    {
        let f = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            tx.borrow_mut()
                .push_back(Update::Socket(id, SocketEvent::Closed));
        });
        ws.set_onclose(Some(f.as_ref().unchecked_ref()));
        handlers.push(f);
    }
    Ok(Socket {
        ws,
        _handlers: handlers,
    })
}

pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

pub fn bell() {}

pub fn read_clipboard() -> Option<String> {
    None
}

pub fn copy(text: &str) {
    hook("copy", &[text.into()]);
}

pub fn new_id() -> String {
    web_sys::window()
        .and_then(|w| w.crypto().ok())
        .map(|c| c.random_uuid())
        .unwrap_or_default()
}

pub fn set_location(path: &str) {
    hook("location", &[path.into()]);
}

pub fn save_language(en: bool) {
    hook("language", &[en.into()]);
}

pub fn open_url(url: &str) {
    hook("open", &[url.into()]);
}

pub fn navigate(url: &str) {
    hook("navigate", &[url.into()]);
}

pub fn signed_out(redirect: &str, portal: &str) {
    hook("signedOut", &[redirect.into(), portal.into()]);
}

/// A standalone instance serves its token form at `/venticode/login`; a
/// Ventic workspace redirects there to the portal instead.
pub async fn token_login(_cx: &Cx) -> bool {
    let init = RequestInit::new();
    init.set_credentials(RequestCredentials::SameOrigin);
    init.set_redirect(RequestRedirect::Manual);
    let Ok(request) = Request::new_with_str_and_init("/venticode/login", &init) else {
        return false;
    };
    let Some(window) = web_sys::window() else {
        return false;
    };
    match JsFuture::from(window.fetch_with_request(&request)).await {
        Ok(response) => response
            .dyn_into::<Response>()
            .is_ok_and(|r| r.status() == 200),
        Err(_) => false,
    }
}

/// Exchange the access token for a session cookie. Ok(false): wrong token.
pub async fn login(_cx: &Cx, token: &str) -> Result<bool> {
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_credentials(RequestCredentials::SameOrigin);
    init.set_body(&JsValue::from_str(&format!("token={}", urlencode(token))));
    let request = Request::new_with_str_and_init("/venticode/login", &init).map_err(js_error)?;
    request
        .headers()
        .set("Content-Type", "application/x-www-form-urlencoded")
        .map_err(js_error)?;
    let window = web_sys::window().context("window")?;
    let response: Response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| anyhow!("Request failed: {}", js_error(e)))?
        .dyn_into()
        .map_err(js_error)?;
    match response.status() {
        // The service answers 303 to `/`; fetch follows it.
        200..300 if response.redirected() => Ok(true),
        401 => Ok(false),
        status => {
            let text = JsFuture::from(response.text().map_err(js_error)?)
                .await
                .ok()
                .and_then(|t| t.as_string())
                .unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("HTTP {status}"));
            Err(anyhow!(message))
        }
    }
}

pub fn sign_in(dev: bool) {
    hook("signIn", &[dev.into()]);
}

/// The page draws the real JPEG over these cells; the grid stays blank.
pub fn draw_image(f: &mut Frame, area: Rect, frame: &ScreenFrame) {
    crate::view::paint(
        f,
        area,
        (0..area.height).map(|_| Line::raw(" ".repeat(usize::from(area.width)))),
    );
    OVERLAY.with(|o| {
        *o.borrow_mut() = Some((
            area,
            frame.serial,
            frame.jpeg.clone(),
            frame.width,
            frame.height,
        ));
    });
}
