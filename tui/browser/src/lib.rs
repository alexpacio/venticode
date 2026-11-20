//! Web build of venticode-tui: the shared core runs in WebAssembly, the page
//! forwards keyboard, mouse and paste events and paints the returned cells.

#![recursion_limit = "512"]

#[path = "../../src/app.rs"]
mod app;
#[path = "../../src/editor.rs"]
mod editor;
#[path = "../../src/nav.rs"]
mod nav;
mod platform;
#[path = "../../src/screens.rs"]
mod screens;
#[path = "../../src/view.rs"]
mod view;

use ftui::core::event::{
    Event, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind, PasteEvent,
};
use ftui::{Frame, GraphemePool, render::cell::StyleFlags};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

enum Input {
    Event(Event),
    /// Pointer input already mapped to remote-screen pixels by the page.
    Remote(Value),
}

#[wasm_bindgen]
pub struct Terminal {
    inputs: Rc<RefCell<VecDeque<Input>>>,
    size: Rc<Cell<(u16, u16)>>,
    en: bool,
}

fn css_color(color: ftui::PackedRgba) -> String {
    if color.a() == 0 {
        String::new()
    } else {
        format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
    }
}

/// Style bits handed to the DOM renderer: 1 bold, 2 italic, 4 strikethrough.
fn style_mask(cell: &ftui::Cell) -> u8 {
    let flags = cell.attrs.flags();
    u8::from(flags.contains(StyleFlags::BOLD))
        | (u8::from(flags.contains(StyleFlags::ITALIC)) << 1)
        | (u8::from(flags.contains(StyleFlags::STRIKETHROUGH)) << 2)
}

fn key_code(key: &str, shift: bool) -> Option<KeyCode> {
    Some(match key {
        "Enter" => KeyCode::Enter,
        "Escape" => KeyCode::Escape,
        "Backspace" => KeyCode::Backspace,
        "Tab" if shift => KeyCode::BackTab,
        "Tab" => KeyCode::Tab,
        "Delete" => KeyCode::Delete,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        f if f.len() > 1 && f.starts_with('F') => KeyCode::F(f[1..].parse().ok()?),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next()?),
        _ => return None,
    })
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Serialize the frame as rows of [fg, bg, text, style] spans.
fn rows(buffer: &ftui::Buffer, pool: &GraphemePool) -> Vec<Vec<(String, String, String, u8)>> {
    let mut rows = Vec::new();
    for y in 0..buffer.height() {
        let mut spans: Vec<(String, String, String, u8)> = Vec::new();
        for x in 0..buffer.width() {
            let Some(cell) = buffer.get(x, y) else {
                continue;
            };
            if cell.content.is_continuation() {
                continue;
            }
            let text = match cell.content.grapheme_id() {
                Some(id) => pool.get(id).unwrap_or(" ").to_owned(),
                None => cell.content.as_char().unwrap_or(' ').to_string(),
            };
            let (fg, bg, flags) = (css_color(cell.fg), css_color(cell.bg), style_mask(cell));
            match spans.last_mut() {
                Some((f, b, value, m)) if *f == fg && *b == bg && *m == flags => {
                    value.push_str(&text)
                }
                _ => spans.push((fg, bg, text, flags)),
            }
        }
        rows.push(spans);
    }
    rows
}

#[wasm_bindgen]
impl Terminal {
    /// `hooks` is an object of page callbacks: draw, image, copy, open,
    /// navigate, location, language, signedOut, signIn.
    #[wasm_bindgen(constructor)]
    pub fn new(hooks: JsValue, en: bool, dev: bool, width: u16, height: u16) -> Terminal {
        platform::install(hooks, dev);
        Terminal {
            inputs: Rc::default(),
            size: Rc::new(Cell::new((width.max(1), height.max(1)))),
            en,
        }
    }

    /// Load the session for `path` (`/c/<id>`, `/bots/<id>`, ...) and run.
    pub fn start(&self, path: String) {
        let inputs = self.inputs.clone();
        let size = self.size.clone();
        let en = self.en;
        wasm_bindgen_futures::spawn_local(async move {
            let cx = platform::Cx;
            let (tx, rx) = platform::channel();
            let origin = web_sys::window()
                .and_then(|w| w.location().origin().ok())
                .unwrap_or_default();
            let mut app = app::App::new(platform::Remote { workspace: origin }, en, tx);
            if let Err(e) = app.start(&cx, &path).await {
                app.status = e.to_string();
            }
            let mut pool = GraphemePool::new();
            let mut last = (0, 0);
            let mut serial = 0u64;
            loop {
                let pending: Vec<Input> = inputs.borrow_mut().drain(..).collect();
                for input in pending {
                    match input {
                        Input::Event(event) => app.handle(&cx, event).await,
                        Input::Remote(value) => {
                            screens::live_input(&mut app, value);
                            app.dirty = true;
                        }
                    }
                }
                let updates: Vec<platform::Update> = rx.borrow_mut().drain(..).collect();
                for update in updates {
                    app.update(update);
                }
                app.tick(&cx).await;
                let (w, h) = size.get();
                if app.dirty || (w, h) != last {
                    last = (w, h);
                    platform::OVERLAY.with(|o| *o.borrow_mut() = None);
                    let mut frame = Frame::new(w, h, &mut pool);
                    app.draw(&mut frame);
                    let cursor = frame.cursor_position.map(|(x, y)| json!([x, y]));
                    let buffer = frame.buffer;
                    let rows = rows(&buffer, &pool);
                    let overlay = platform::OVERLAY.with(|o| o.borrow_mut().take());
                    let image = overlay.as_ref().map(|(r, s, _, fw, fh)| {
                        json!({"x": r.x, "y": r.y, "w": r.width, "h": r.height, "serial": s, "width": fw, "height": fh, "human": app.live.human, "desktop": app.screen == app::Screen::Desktop})
                    });
                    if let Some((_, s, jpeg, _, _)) = &overlay
                        && *s != serial
                    {
                        serial = *s;
                        platform::hook(
                            "image",
                            &[
                                js_sys::Uint8Array::from(jpeg.as_slice()).into(),
                                JsValue::from(*s as f64),
                            ],
                        );
                    }
                    let payload = json!({"rows": rows, "cursor": cursor, "image": image});
                    platform::hook("draw", &[JsValue::from_str(&payload.to_string())]);
                }
                sleep(16).await;
            }
        });
    }

    pub fn key(&self, key: String, ctrl: bool, alt: bool, shift: bool, meta: bool) -> bool {
        let Some(code) = key_code(&key, shift) else {
            return false;
        };
        let mut modifiers = Modifiers::NONE;
        for (on, m) in [
            (ctrl || meta, Modifiers::CTRL),
            (alt, Modifiers::ALT),
            (shift, Modifiers::SHIFT),
        ] {
            if on {
                modifiers |= m;
            }
        }
        let code = match code {
            KeyCode::Char(c) if ctrl => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        self.inputs.borrow_mut().push_back(Input::Event(Event::Key(
            KeyEvent::new(code).with_modifiers(modifiers),
        )));
        true
    }

    pub fn paste(&self, text: String) {
        self.inputs
            .borrow_mut()
            .push_back(Input::Event(Event::Paste(PasteEvent::bracketed(text))));
    }

    /// kind: down | up | drag | move | wheelup | wheeldown; button 0 left, 1 middle, 2 right.
    pub fn mouse(&self, kind: &str, button: u8, x: u16, y: u16) {
        let button = match button {
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::Left,
        };
        let kind = match kind {
            "down" => MouseEventKind::Down(button),
            "up" => MouseEventKind::Up(button),
            "drag" => MouseEventKind::Drag(button),
            "move" => MouseEventKind::Moved,
            "wheelup" => MouseEventKind::ScrollUp,
            "wheeldown" => MouseEventKind::ScrollDown,
            _ => return,
        };
        self.inputs
            .borrow_mut()
            .push_back(Input::Event(Event::Mouse(MouseEvent::new(kind, x, y))));
    }

    /// Input for the remote screen, in remote pixels (see `screens::live_input`).
    pub fn remote(&self, json: &str) {
        if let Ok(value) = serde_json::from_str(json) {
            self.inputs.borrow_mut().push_back(Input::Remote(value));
        }
    }

    pub fn resize(&self, width: u16, height: u16) {
        self.size.set((width.clamp(1, 400), height.clamp(1, 200)));
    }
}
