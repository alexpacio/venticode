use eframe::egui::{self, Event, Key, MouseWheelUnit};
use js_sys::Function;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

#[derive(Default)]
struct ScreenState {
    context: Option<egui::Context>,
    pending: Option<egui::ColorImage>,
    size: egui::Vec2,
    visible: bool,
    human: bool,
    desktop: bool,
}

#[wasm_bindgen]
pub struct RemoteScreen {
    runner: eframe::WebRunner,
    state: Rc<RefCell<ScreenState>>,
}

#[wasm_bindgen]
impl RemoteScreen {
    #[wasm_bindgen(constructor)]
    #[expect(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            runner: eframe::WebRunner::new(),
            state: Rc::default(),
        }
    }

    pub async fn start(&self, canvas: HtmlCanvasElement, send: Function) -> Result<(), JsValue> {
        let state = self.state.clone();
        self.runner
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(move |creation| {
                    state.borrow_mut().context = Some(creation.egui_ctx.clone());
                    Ok(Box::new(Screen {
                        state,
                        texture: None,
                        send,
                    }))
                }),
            )
            .await
    }

    pub fn set_frame(&self, pixels: &[u8], width: u32, height: u32, remote_w: f32, remote_h: f32) {
        if width == 0
            || width > 4096
            || height == 0
            || height > 4096
            || pixels.len() != width as usize * height as usize * 4
            || !(1.0..=4096.0).contains(&remote_w)
            || !(1.0..=4096.0).contains(&remote_h)
        {
            return;
        }
        let mut state = self.state.borrow_mut();
        state.pending = Some(egui::ColorImage::from_rgba_unmultiplied(
            [width as usize, height as usize],
            pixels,
        ));
        state.size = egui::vec2(remote_w, remote_h);
        state.visible = true;
        if let Some(context) = &state.context {
            context.request_repaint();
        }
    }

    pub fn clear(&self) {
        let mut state = self.state.borrow_mut();
        state.pending = None;
        state.visible = false;
        if let Some(context) = &state.context {
            context.request_repaint();
        }
    }

    pub fn set_control(&self, human: bool) {
        let mut state = self.state.borrow_mut();
        state.human = human;
        if let Some(context) = &state.context {
            context.request_repaint();
        }
    }

    pub fn set_desktop(&self, desktop: bool) {
        self.state.borrow_mut().desktop = desktop;
    }

    pub fn destroy(&self) {
        self.runner.destroy();
    }
}

struct Screen {
    state: Rc<RefCell<ScreenState>>,
    texture: Option<egui::TextureHandle>,
    send: Function,
}

impl Screen {
    fn input(&self, input: Value) {
        let _ = self
            .send
            .call1(&JsValue::NULL, &JsValue::from_str(&input.to_string()));
    }
}

impl eframe::App for Screen {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let bounds = ui.available_rect_before_wrap();
        ui.painter()
            .rect_filled(bounds, 0.0, egui::Color32::from_rgb(15, 20, 29));
        let (visible, human, desktop, size, pending) = {
            let mut state = self.state.borrow_mut();
            (state.visible, state.human, state.desktop, state.size, state.pending.take())
        };
        if !visible {
            self.texture = None;
            return;
        }
        if let Some(image) = pending {
            match &mut self.texture {
                Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ui.ctx().load_texture(
                        "browser",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }
        let Some(texture) = &self.texture else { return };
        let scale = (bounds.width() / size.x).min(bounds.height() / size.y);
        if scale <= 0.0 {
            return;
        }
        let rect = egui::Rect::from_center_size(bounds.center(), size * scale);
        ui.painter().image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        if !human {
            return;
        }
        let point = |position: egui::Pos2| (position - rect.min) / scale;
        let (focused, events, click, hover) = ui.input(|input| {
            let click = input
                .pointer
                .primary_clicked()
                .then(|| input.pointer.interact_pos())
                .flatten();
            (
                input.focused,
                input.events.clone(),
                click,
                input.pointer.hover_pos(),
            )
        });
        if !desktop && let Some(position) = click.filter(|position| rect.contains(*position)) {
            let position = point(position);
            self.input(json!({"kind":"click", "x":position.x, "y":position.y}));
        }
        for event in events {
            match event {
                Event::PointerMoved(position) if desktop && rect.contains(position) => {
                    let position = point(position);
                    self.input(json!({"kind":"move","x":position.x,"y":position.y}));
                }
                Event::PointerButton {pos,button,pressed,..} if desktop && (rect.contains(pos) || !pressed) => {
                    let position = point(rect.clamp(pos));
                    let button = match button {
                        egui::PointerButton::Primary => 1,
                        egui::PointerButton::Middle => 2,
                        egui::PointerButton::Secondary => 3,
                        _ => continue,
                    };
                    self.input(json!({"kind":"pointer","x":position.x.min(size.x-1.0),"y":position.y.min(size.y-1.0),"button":button,"down":pressed}));
                }
                Event::MouseWheel { unit, delta, .. } if delta.y != 0.0 => {
                    if let Some(position) = hover.filter(|position| rect.contains(*position)) {
                        let position = point(position);
                        let amount = match unit {
                            MouseWheelUnit::Point => 1.0 / scale,
                            MouseWheelUnit::Line => 32.0,
                            MouseWheelUnit::Page => size.y,
                        };
                        self.input(json!({"kind":"scroll","x":position.x,"y":position.y,
                            "delta":(-delta.y * amount).clamp(-1500.0, 1500.0)}));
                    }
                }
                Event::Text(text) | Event::Paste(text) if focused && !text.is_empty() => {
                    let mut rest = &text[..text.floor_char_boundary(4096)];
                    while !rest.is_empty() {
                        let (chunk, tail) = rest.split_at(rest.floor_char_boundary(1024));
                        self.input(json!({"kind":"text","text":chunk}));
                        rest = tail;
                    }
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if desktop && focused => {
                    if !modifiers.ctrl && !modifiers.command && !modifiers.alt && key.name().len() == 1 {
                        continue;
                    }
                    let mut shortcut = String::new();
                    if modifiers.ctrl || modifiers.command { shortcut.push_str("ctrl+"); }
                    if modifiers.alt { shortcut.push_str("alt+"); }
                    if modifiers.shift { shortcut.push_str("shift+"); }
                    shortcut.push_str(match key {
                        Key::ArrowLeft => "ArrowLeft", Key::ArrowRight => "ArrowRight",
                        Key::ArrowUp => "ArrowUp", Key::ArrowDown => "ArrowDown",
                        _ => key.name(),
                    });
                    self.input(json!({"kind":"key","key":shortcut}));
                }
                Event::Key { key, pressed: true, modifiers, .. }
                    if focused && !modifiers.command && !modifiers.alt => {
                    let name = match key {
                        Key::Enter => "Enter",
                        Key::Backspace => "Backspace",
                        Key::Tab => "Tab",
                        Key::Escape => "Escape",
                        Key::ArrowLeft => "ArrowLeft",
                        Key::ArrowRight => "ArrowRight",
                        Key::ArrowUp => "ArrowUp",
                        Key::ArrowDown => "ArrowDown",
                        Key::Delete => "Delete",
                        Key::Home => "Home",
                        Key::End => "End",
                        Key::PageUp => "PageUp",
                        Key::PageDown => "PageDown",
                        _ => continue,
                    };
                    self.input(json!({"kind":"key","key":name}));
                }
                _ => {}
            }
        }
    }
}
