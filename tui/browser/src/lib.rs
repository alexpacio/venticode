use ftui::{Frame, GraphemePool};
use serde::Deserialize;
use serde_json::Value;
use wasm_bindgen::prelude::*;

#[path = "../../src/editor.rs"]
mod editor;
#[path = "../../src/view.rs"]
mod view;

#[derive(Default, Deserialize)]
#[serde(default)]
struct App {
    en: bool,
    project: Value,
    chat: Value,
    model: String,
    lines: Vec<view::Message>,
    compact_tools: bool,
    input: String,
    status: String,
    approval: String,
    question: String,
    plan: String,
    scroll: u16,
    // Mouse-selection fields (unused in the browser build; required by view::draw).
    body_top: u16,
    body_height: u16,
    body_texts: Vec<String>,
    sel: Option<(u16, u16)>,
}
impl App {
    fn msg<'a>(&self, it: &'a str, en: &'a str) -> &'a str {
        if self.en { en } else { it }
    }
}
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}
fn string(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_owned()
}

fn css_color(color: ftui::PackedRgba) -> String {
    if color.a() == 0 {
        String::new()
    } else {
        format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
    }
}

#[wasm_bindgen]
pub fn render(state: &str, width: u16, height: u16) -> Result<String, JsValue> {
    let mut app: App =
        serde_json::from_str(state).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width.clamp(1, 240), height.clamp(1, 100), &mut pool);
    let caret = app.input.len();
    view::draw(&mut frame, &mut app, caret, &[], 0, None, None);
    let buffer = frame.buffer;
    let mut rows = Vec::new();
    for y in 0..buffer.height() {
        let mut spans: Vec<(String, String, String)> = Vec::new();
        for x in 0..buffer.width() {
            let cell = buffer.get(x, y).unwrap();
            if cell.content.is_continuation() {
                continue;
            }
            let text = if let Some(id) = cell.content.grapheme_id() {
                pool.get(id).unwrap_or(" ").to_owned()
            } else {
                cell.content.as_char().unwrap_or(' ').to_string()
            };
            let color = css_color(cell.fg);
            let background = css_color(cell.bg);
            if let Some((prev, bg, value)) = spans.last_mut()
                && prev == &color
                && bg == &background
            {
                value.push_str(&text);
            } else {
                spans.push((color, background, text));
            }
        }
        rows.push(spans);
    }
    serde_json::to_string(&serde_json::json!({"rows":rows,"scroll":app.scroll}))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}
