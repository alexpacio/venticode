mod client;
mod editor;
mod login;
mod store;
mod view;

use anyhow::{Context, Result, anyhow, bail, ensure};
use asupersync::Cx;
use asupersync::channel::mpsc;
use asupersync::http::h1::Method;
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{sleep, wall_now};
use client::{Remote, StreamUpdate, origin};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use rich_rust::prelude::*;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

const MAX_DRAFT_HISTORY: usize = 200;
const MAX_PATCHES: usize = 200;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_EVENTS_PER_FRAME: usize = 32;
const REPLAY_BELL_GRACE: Duration = Duration::from_millis(500);

const COMMANDS: &[&str] = &[
    "/help",
    "/browser",
    "/new",
    "/resume",
    "/model",
    "/permissions",
    "/status",
    "/rename",
    "/diff",
    "/copy",
    "/reconnect",
    "/plan",
    "/goal",
    "/build",
    "/answer",
    "/allow",
    "/deny",
    "/stop",
    "/projects",
    "/use",
    "/harnesses",
    "/folder",
    "/chats",
    "/open",
    "/models",
    "/cancel-goal",
    "/logout",
    "/exit",
    "/quit",
];

fn string(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_owned()
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect()
}

fn choice<'a>(items: &'a [Value], value: &str) -> Result<&'a Value> {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| items.get(n))
        .or_else(|| items.iter().find(|v| v["id"] == value))
        .context("Indice o ID non valido / Invalid index or ID")
}

fn entries(items: &[Value]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(n, v)| {
            format!(
                "{}  {}",
                n + 1,
                v["name"]
                    .as_str()
                    .or(v["title"].as_str())
                    .or(v["id"].as_str())
                    .unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_to_clipboard(text: &str) -> bool {
    if let Ok(mut clip) = arboard::Clipboard::new()
        && clip.set_text(text.to_owned()).is_ok()
    {
        return true;
    }
    // OSC 52 fallback for remote/SSH terminals.
    let mut b64 = String::with_capacity(text.len().div_ceil(3) * 4);
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        b64.push(TABLE[((n >> 18) & 63) as usize] as char);
        b64.push(TABLE[((n >> 12) & 63) as usize] as char);
        b64.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        b64.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    let _ = write!(io::stdout(), "\x1b]52;c;{b64}\x07");
    let _ = io::stdout().flush();
    true
}

fn ring_bell() {
    let _ = write!(io::stdout(), "\x07");
    let _ = io::stdout().flush();
}

fn delete_prev_word(input: &mut String, caret: &mut usize) {
    let trimmed = input[..*caret].trim_end_matches(char::is_whitespace);
    let start = trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    input.drain(start..*caret);
    *caret = start;
}

fn append_capped_output(dest: &mut String, chunk: &str, max_bytes: usize) {
    const TRUNCATED: &str = "\n… [output truncated]";
    if dest.len() >= max_bytes {
        return;
    }
    let remaining = max_bytes - dest.len();
    if chunk.len() <= remaining {
        dest.push_str(chunk);
    } else {
        let mut cut = remaining.min(chunk.len());
        while cut > 0 && !chunk.is_char_boundary(cut) {
            cut -= 1;
        }
        dest.push_str(&chunk[..cut]);
        dest.push_str(TRUNCATED);
    }
}

fn slash_suggestions(input: &str, hide_menu: bool) -> Vec<&'static str> {
    if hide_menu || !input.starts_with('/') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .copied()
        .filter(|cmd| cmd.starts_with(input))
        .collect()
}

fn needs_tab_completion(line: &str, suggestions: &[&str]) -> bool {
    line.starts_with('/')
        && !line.starts_with("//")
        && !line.contains(' ')
        && !COMMANDS.contains(&line)
        && !suggestions.is_empty()
}

fn picker_options(app: &App, picker: Option<Picker>) -> Vec<String> {
    match picker {
        Some(Picker::Models) => app
            .models
            .iter()
            .map(|m| clean(m["name"].as_str().or(m["id"].as_str()).unwrap_or("?")))
            .collect(),
        Some(Picker::Projects) if app.projects.is_empty() => app
            .folder_options()
            .iter()
            .map(|(h, root)| clean(&format!("{} · {}", string(h, "name"), root)))
            .collect(),
        Some(Picker::Projects) => app
            .projects
            .iter()
            .map(|p| {
                clean(&format!(
                    "{} · {}/{}",
                    string(p, "name"),
                    string(p, "root"),
                    string(p, "path")
                ))
            })
            .collect(),
        Some(Picker::Chats) => std::iter::once(
            app.msg("Nuova conversazione", "New conversation")
                .to_owned(),
        )
        .chain(
            app.project_chats()
                .iter()
                .map(|c| clean(&string(c, "title"))),
        )
        .collect(),
        Some(Picker::Browse) => browse_rows(app),
        None => Vec::new(),
    }
}

/// Row labels for the active browse: [create-here] [parent?] [dirs...] [files...].
/// `choose_browse` relies on this exact ordering.
fn browse_rows(app: &App) -> Vec<String> {
    let Some(b) = app.browse.as_ref() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let here = if b.at_root() {
        app.msg("Crea progetto qui", "Create project here")
    } else {
        app.msg(
            "Crea progetto in questa cartella",
            "Create project in this folder",
        )
    };
    rows.push(clean(&format!("● {here}")));
    if !b.at_root() {
        rows.push(clean("↑ .."));
    }
    for entry in &b.entries {
        let name = string(entry, "name");
        if entry["directory"] == true {
            rows.push(clean(&format!("📁 {name}/")));
        }
    }
    for entry in &b.entries {
        if entry["directory"] == true {
            continue;
        }
        let name = string(entry, "name");
        let bytes = entry["bytes"].as_u64().unwrap_or(0);
        rows.push(clean(&format!("   {name}  ({bytes} B)")));
    }
    if rows.len() == 1 && !b.error.is_empty() {
        rows.push(clean(&format!("⚠ {}", b.error)));
    } else if b.loading {
        rows.push(clean(app.msg("Caricamento…", "Loading…")));
    }
    rows
}

struct FeedHandle {
    task: asupersync::runtime::TaskHandle<()>,
}

impl FeedHandle {
    fn stop(self) {
        self.task.abort();
    }
}

struct App {
    remote: Remote,
    store: store::SessionStore,
    en: bool,
    projects: Vec<Value>,
    models: Vec<Value>,
    harnesses: Vec<Value>,
    chats: Vec<Value>,
    project: Value,
    chat: Value,
    model: String,
    lines: Vec<view::Message>,
    compact_tools: bool,
    last_text: String,
    patches: Vec<String>,
    input: String,
    status: String,
    question: String,
    choices: Vec<String>,
    retry: Option<(String, Value, Uuid)>,
    plan: String,
    goal: String,
    approval: String,
    scroll: u16,
    last_sequence: i64,
    replaying: bool,
    opened_at: Instant,
    feed: Option<FeedHandle>,
    tx: mpsc::Sender<(String, StreamUpdate)>,
    browse: Option<Browse>,
    /// One-shot: run_ui opens the browse picker after a /folder command.
    open_browse: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Picker {
    Models,
    Projects,
    Chats,
    Browse,
}

/// State for the interactive folder browser (F3 / /folder drill-down).
struct Browse {
    harness: Value,
    root: String,
    path: String,
    entries: Vec<Value>,
    loading: bool,
    error: String,
}

impl Browse {
    /// True when `path` is the granted root itself (so there's no parent row).
    fn at_root(&self) -> bool {
        self.path == "." || self.path.is_empty()
    }
}

/// `..` parent of a relative path, clamped at the root (".").
fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => ".".to_owned(),
        Some((parent, _)) => parent.to_owned(),
    }
}

/// Join a directory name onto a relative path, keeping "." at the root.
fn join(path: &str, name: &str) -> String {
    if path == "." || path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}/{name}")
    }
}

impl Picker {
    fn label(self, app: &App) -> &'static str {
        match self {
            Self::Models => app.msg("Scegli modello", "Select model"),
            Self::Projects if app.projects.is_empty() && app.folder_options().is_empty() => app
                .msg(
                    "Collega un harness con una cartella nel portale, poi premi F3.",
                    "Connect a harness with a folder in the portal, then press F3.",
                ),
            Self::Projects if app.projects.is_empty() => app.msg(
                "Crea il primo progetto: scegli una cartella",
                "Create your first project: choose a folder",
            ),
            Self::Projects => app.msg("Scegli il progetto in cui lavorare", "Choose a project"),
            Self::Chats => app.msg("Scegli una conversazione", "Choose a conversation"),
            Self::Browse => app.msg(
                "Sfoglia le cartelle · Invio per entrare · . per creare qui",
                "Browse folders · Enter to open · . to create here",
            ),
        }
    }
}

impl App {
    fn msg<'a>(&self, it: &'a str, en: &'a str) -> &'a str {
        if self.en { en } else { it }
    }

    fn folder_options(&self) -> Vec<(&Value, &str)> {
        self.harnesses
            .iter()
            .flat_map(|h| {
                h["roots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(move |root| (h, root))
            })
            .collect()
    }

    /// Enter the folder browser at the first available harness root.
    async fn start_browse(&mut self, cx: &Cx) -> Result<()> {
        let folders = self.folder_options();
        let Some(&(h, root)) = folders.first() else {
            self.note(self.msg(
                "Nessun harness con cartelle disponibili.",
                "No harness with folders available.",
            ));
            return Ok(());
        };
        self.browse = Some(Browse {
            harness: h.clone(),
            root: root.to_owned(),
            path: ".".to_owned(),
            entries: Vec::new(),
            loading: true,
            error: String::new(),
        });
        self.reload_browse(cx).await
    }

    /// Fetch the current browse directory's entries.
    async fn reload_browse(&mut self, cx: &Cx) -> Result<()> {
        let (harness_id, root, path) = {
            let b = self.browse.as_ref().context("No active browse")?;
            (string(&b.harness, "id"), b.root.clone(), b.path.clone())
        };
        let result = self.remote.list_dir(cx, &harness_id, &root, &path).await;
        if let Some(b) = self.browse.as_mut() {
            b.loading = false;
            match result {
                Ok(entries) => {
                    b.entries = entries;
                    b.error.clear();
                }
                Err(err) => {
                    b.entries.clear();
                    b.error = err.to_string();
                }
            }
        }
        Ok(())
    }

    /// Create a project at the current browse path and open it.
    async fn create_browse_project(&mut self, cx: &Cx) -> Result<()> {
        let (harness_id, root, path) = {
            let b = self.browse.as_ref().context("No active browse")?;
            (string(&b.harness, "id"), b.root.clone(), b.path.clone())
        };
        let body = json!({"harness_id": harness_id, "root": root, "path": path});
        let p = self
            .remote
            .api(cx, Method::Post, "/projects", Some(body))
            .await?;
        self.browse = None;
        self.projects.push(p.clone());
        self.close();
        self.project = p;
        self.welcome();
        Ok(())
    }

    /// Handle Enter on a highlighted browse row: create-here, parent, dir, or file.
    async fn choose_browse(&mut self, cx: &Cx, index: usize) -> Result<()> {
        // Row layout produced by browse_rows(): [create-here] [parent?] [dirs...] [files...]
        let b = self.browse.as_mut().context("No active browse")?;
        if b.loading {
            return Ok(());
        }
        if index == 0 {
            return self.create_browse_project(cx).await;
        }
        let mut cursor = 1;
        if !b.at_root() {
            if index == cursor {
                let path = parent(&b.path);
                b.path = path;
                b.loading = true;
                return self.reload_browse(cx).await;
            }
            cursor += 1;
        }
        let entry = b.entries.get(index - cursor).cloned();
        match entry {
            Some(e) if e["directory"] == true => {
                let name = string(&e, "name");
                let path = join(&b.path, &name);
                b.path = path;
                b.loading = true;
                self.reload_browse(cx).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn choose(&mut self, cx: &Cx, picker: Picker, index: usize) -> Result<()> {
        match picker {
            Picker::Models => {
                self.command(cx, &format!("/model {}", index + 1)).await?;
            }
            Picker::Browse => {
                self.choose_browse(cx, index).await?;
            }
            Picker::Projects => {
                let project = if self.projects.is_empty() {
                    let folders = self.folder_options();
                    let &(h, root) = folders
                        .get(index)
                        .context("Cartella non valida / Invalid folder selection")?;
                    let body = json!({"harness_id":h["id"],"root":root,"path":"."});
                    let p = self
                        .remote
                        .api(cx, Method::Post, "/projects", Some(body))
                        .await?;
                    self.projects.push(p.clone());
                    p
                } else {
                    self.projects
                        .get(index)
                        .cloned()
                        .context("Progetto non valido / Invalid project selection")?
                };
                self.close();
                self.project = project;
                self.welcome();
            }
            Picker::Chats if index == 0 => {
                self.close();
                self.welcome();
            }
            Picker::Chats => {
                let chats = self.project_chats();
                let target = chats
                    .get(index.saturating_sub(1))
                    .context("Conversazione non valida / Invalid conversation selection")?;
                let id = string(target, "id");
                self.open(cx, &id).await?;
            }
        }
        Ok(())
    }

    fn path(&self) -> Result<String> {
        Ok(format!(
            "/conversations/{}",
            self.chat["id"].as_str().context(self.msg(
                "Premi F2 per scegliere una conversazione, oppure scrivi il primo messaggio.",
                "Press F2 to choose a conversation, or write your first message."
            ))?
        ))
    }

    fn note(&mut self, s: impl AsRef<str>) {
        self.record(view::Role::Notice, s);
    }

    fn working(&self) -> bool {
        self.chat["state"] == "running" && self.feed.is_some()
    }

    fn notify_bell(&mut self) {
        if self.replaying {
            if self.opened_at.elapsed() < REPLAY_BELL_GRACE {
                return;
            }
            self.replaying = false;
        }
        ring_bell();
    }

    fn record(&mut self, role: view::Role, s: impl AsRef<str>) {
        self.last_text.clear();
        let text = clean(s.as_ref());
        if matches!(role, view::Role::Output) {
            if let Some(last) = self.lines.last_mut()
                && matches!(last.role, view::Role::Output)
            {
                if !text.is_empty() {
                    append_capped_output(&mut last.text, &text, MAX_OUTPUT_BYTES);
                }
                return;
            }
            let mut capped = String::new();
            append_capped_output(&mut capped, &text, MAX_OUTPUT_BYTES);
            self.lines.push(view::Message { role, text: capped });
            return;
        }
        self.lines.push(view::Message { role, text });
    }

    async fn refresh(&mut self, cx: &Cx) -> Result<()> {
        let r_projects = self.remote.clone();
        let r_models = self.remote.clone();
        let r_harnesses = self.remote.clone();
        let mut t_projects = cx
            .spawn(move |child_cx| async move { r_projects.list(&child_cx, "/projects").await })
            .map_err(|e| anyhow!("Failed to fetch projects: {e}"))?;
        let mut t_models = cx
            .spawn(move |child_cx| async move { r_models.list(&child_cx, "/models").await })
            .map_err(|e| anyhow!("Failed to fetch models: {e}"))?;
        let mut t_harnesses = cx
            .spawn(move |child_cx| async move { r_harnesses.list(&child_cx, "/harnesses").await })
            .map_err(|e| anyhow!("Failed to fetch harnesses: {e}"))?;
        let chats = match self.remote.list(cx, "/conversations").await {
            Ok(v) => v,
            Err(err) => {
                t_projects.abort();
                t_models.abort();
                t_harnesses.abort();
                return Err(err);
            }
        };
        let projects = match t_projects.join(cx).await {
            Ok(Ok(v)) => v,
            Ok(Err(err)) => {
                t_models.abort();
                t_harnesses.abort();
                return Err(err);
            }
            Err(err) => {
                t_models.abort();
                t_harnesses.abort();
                return Err(err.into());
            }
        };
        let models = match t_models.join(cx).await {
            Ok(Ok(v)) => v,
            Ok(Err(err)) => {
                t_harnesses.abort();
                return Err(err);
            }
            Err(err) => {
                t_harnesses.abort();
                return Err(err.into());
            }
        };
        let harnesses = t_harnesses.join(cx).await??;
        self.projects = projects;
        self.models = models;
        self.harnesses = harnesses;
        self.chats = chats;
        Ok(())
    }

    fn project_chats(&self) -> Vec<Value> {
        self.chats
            .iter()
            .filter(|c| self.project.is_null() || c["project_id"] == self.project["id"])
            .cloned()
            .collect()
    }

    fn close(&mut self) {
        if let Some(f) = self.feed.take() {
            f.stop();
        }
        self.chat = Value::Null;
        self.lines.clear();
        view::clear_markdown_cache();
        self.patches.clear();
        self.question.clear();
        self.choices.clear();
        self.retry = None;
        self.plan.clear();
        self.goal.clear();
        self.approval.clear();
        self.last_text.clear();
        self.scroll = 0;
        self.last_sequence = 0;
        self.replaying = false;
    }

    async fn open(&mut self, cx: &Cx, id: &str) -> Result<()> {
        let _: Uuid = id.parse()?;
        let c = self
            .remote
            .api(cx, Method::Get, &format!("/conversations/{id}"), None)
            .await?;
        self.activate(cx, c)
    }

    fn activate(&mut self, cx: &Cx, c: Value) -> Result<()> {
        let id = c["id"]
            .as_str()
            .context("Missing conversation ID")?
            .to_owned();
        self.close();
        self.project = self
            .projects
            .iter()
            .find(|p| p["id"] == c["project_id"])
            .cloned()
            .unwrap_or(Value::Null);
        self.chat = c;
        self.model = string(&self.chat, "model");
        self.question = string(&self.chat, "question_id");
        self.plan = string(&self.chat, "plan_id");
        self.goal = string(&self.chat, "goal_id");
        self.replaying = true;
        self.opened_at = Instant::now();
        self.start_feed(cx, id);
        Ok(())
    }

    fn start_feed(&mut self, cx: &Cx, id: String) {
        if let Some(f) = self.feed.take() {
            f.stop();
        }
        let remote = self.remote.clone();
        let tx = self.tx.clone();
        let start_cursor = self.last_sequence;
        let en = self.en;
        if let Ok(task) = cx.spawn(move |child_cx| async move {
            remote.follow(child_cx, id, start_cursor, en, tx).await;
        }) {
            self.feed = Some(FeedHandle { task });
        }
    }

    fn event(&mut self, e: Value) {
        if let Some(seq) = e["sequence"].as_i64() {
            if seq <= self.last_sequence {
                return;
            }
            self.last_sequence = seq;
        }
        let val = |k| string(&e, k);
        let prev_newlines: usize = self
            .lines
            .last()
            .map_or(0, |m| m.text.matches('\n').count());
        let prev_len = self.lines.len();
        match e["type"].as_str() {
            Some("text") => {
                let id = val("message_id");
                if self.last_text != id
                    || !self
                        .lines
                        .last()
                        .is_some_and(|line| matches!(line.role, view::Role::Assistant))
                {
                    self.lines.push(view::Message {
                        role: view::Role::Assistant,
                        text: String::new(),
                    });
                    self.last_text = id;
                }
                self.lines
                    .last_mut()
                    .unwrap()
                    .text
                    .push_str(&clean(&val("delta")));
            }
            Some("user_message") => {
                self.question.clear();
                self.plan.clear();
                if e["intent"] == "plan" {
                    self.chat["permission_mode"] = json!("plan");
                } else if e["intent"] == "implement_plan" {
                    self.chat["permission_mode"] = json!("ask");
                }
                if e["intent"] == "goal" {
                    self.goal = val("message_id");
                }
                self.record(view::Role::User, val("content"));
            }
            Some("question") => {
                self.question = val("reference_id");
                self.choices = e["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let choices = self
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(i, x)| format!("{}  {x}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.note(format!("? {}\n{}", val("content"), choices));
                self.notify_bell();
            }
            Some("plan") => {
                self.plan = val("reference_id");
                self.note(format!("{}\n/build", val("content")));
                self.notify_bell();
            }
            Some("goal_completed" | "goal_cancelled") => {
                self.goal.clear();
                if e["type"] == "goal_cancelled" {
                    self.question.clear();
                    self.plan.clear();
                    self.choices.clear();
                }
                self.note(format!("✓ {}", val("content")));
                self.notify_bell();
            }
            Some("tool_call") => self.record(
                view::Role::Tool,
                format!("{} {}", val("operation"), val("target")),
            ),
            Some("tool_output") => self.record(view::Role::Output, val("data")),
            Some("tool_result") => {
                self.note(format!("{} {}", val("status"), val("summary")));
                if let Some(patch) = e["diff"]["patch"].as_str().filter(|s| !s.is_empty()) {
                    let diff = format!("{}\n{}", string(&e["diff"], "path"), patch);
                    if self.patches.len() >= MAX_PATCHES {
                        let excess = self.patches.len() + 1 - MAX_PATCHES;
                        self.patches.drain(..excess);
                    }
                    self.patches.push(clean(&diff));
                    self.record(view::Role::Output, diff);
                }
            }
            Some("approval") => {
                if e["approval_state"] == "pending" {
                    self.approval = val("approval_id");
                    self.note(format!("{}\n/allow  /deny", val("summary")));
                    self.notify_bell();
                } else {
                    self.approval.clear();
                }
            }
            Some("todo") => {
                if let Some(v) = e["todos"].as_array() {
                    self.note(
                        v.iter()
                            .map(|x| format!("{}  {}", string(x, "status"), string(x, "content")))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
            Some("run") => {
                let was_running = self.chat["state"] == "running";
                self.chat["state"] = e["state"].clone();
                if e["state"] == "idle" || e["state"] == "failed" {
                    self.approval.clear();
                    if was_running {
                        self.notify_bell();
                    }
                }
            }
            Some("error") => self.record(view::Role::Error, val("message")),
            _ => {}
        }
        // Keep the viewport anchored if the user has scrolled up while streaming.
        if self.scroll > 0 {
            let added_msgs = self.lines.len().saturating_sub(prev_len);
            let added_newlines = if added_msgs == 0 {
                self.lines
                    .last()
                    .map_or(0, |m| m.text.matches('\n').count())
                    .saturating_sub(prev_newlines)
            } else {
                added_msgs * 2
            };
            self.scroll = self.scroll.saturating_add(added_newlines as u16);
        }
    }

    async fn new_chat(&mut self, cx: &Cx, title: &str) -> Result<()> {
        ensure!(
            !self.project.is_null(),
            "F3 · Scegli un progetto / Choose a project"
        );
        let mut body = json!({"project_id":self.project["id"]});
        if !self.model.is_empty() {
            body["model"] = json!(self.model);
        }
        if !title.is_empty() {
            body["title"] = json!(title);
        }
        let chat = self
            .remote
            .api(cx, Method::Post, "/conversations", Some(body))
            .await?;
        self.activate(cx, chat.clone())?;
        self.chats.insert(0, chat);
        Ok(())
    }

    fn welcome(&mut self) {
        self.note(self.msg(
            "Scrivi il primo messaggio e premi Invio per iniziare. Premi F2 per riprendere una conversazione.",
            "Write your first message and press Enter to start. Press F2 to resume a conversation.",
        ));
    }

    async fn send(
        &mut self,
        cx: &Cx,
        text: &str,
        intent: &str,
        reference: Option<&str>,
    ) -> Result<()> {
        ensure!(!text.trim().is_empty(), "Messaggio vuoto / Empty message");
        if self.chat.is_null() && matches!(intent, "message" | "plan" | "goal") {
            self.new_chat(cx, "").await?;
        }
        let mut req = json!({"content":text});
        if intent != "message" {
            req["intent"] = json!(intent);
        }
        if let Some(id) = reference {
            req["reference_id"] = json!(id);
        }
        let path = format!("{}/messages", self.path()?);
        let id = self
            .retry
            .as_ref()
            .filter(|(p, v, _)| p == &path && v == &req)
            .map(|t| t.2)
            .unwrap_or_else(Uuid::new_v4);
        self.retry = Some((path.clone(), req.clone(), id));
        req["id"] = json!(id.to_string());
        self.replaying = false;
        self.remote.api(cx, Method::Post, &path, Some(req)).await?;
        self.retry = None;
        Ok(())
    }

    async fn patch_chat(&mut self, cx: &Cx, mut patch: Value) -> Result<()> {
        let path = self.path()?;
        // Always fetch the latest revision before PATCH because turns and agent
        // transitions advance `revision` server-side without pushing it over SSE.
        let latest = self.remote.api(cx, Method::Get, &path, None).await?;
        patch["revision"] = latest["revision"].clone();
        self.chat = self
            .remote
            .api(cx, Method::Patch, &path, Some(patch))
            .await?;
        Ok(())
    }

    async fn command(&mut self, cx: &Cx, line: &str) -> Result<bool> {
        // Allow sending literal slash-prefixed text (like file paths) with `//...`.
        if let Some(escaped) = line.strip_prefix("//") {
            let literal = format!("/{escaped}");
            self.send(cx, &literal, "message", None).await?;
            return Ok(false);
        }
        let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        match command {
            "/logout" => {
                let _ = self.remote.api(cx, Method::Delete, "/session", None).await;
                self.store.clear();
                return Ok(true);
            }
            "/diff" => {
                if self.patches.is_empty() {
                    self.note(self.msg(
                        "Nessuna modifica nella conversazione",
                        "No changes in this conversation",
                    ));
                } else {
                    self.note(self.patches.join("\n"));
                }
            }
            "/copy" => {
                let last_reply = self
                    .lines
                    .iter()
                    .rev()
                    .find(|m| matches!(m.role, view::Role::Assistant) && !m.text.trim().is_empty())
                    .map(|m| m.text.clone());
                if let Some(text) = last_reply {
                    copy_to_clipboard(&text);
                    self.note(self.msg("Risposta copiata negli appunti", "Copied last reply to clipboard"));
                } else {
                    self.note(self.msg("Nessuna risposta da copiare", "No reply to copy"));
                }
            }
            "/reconnect" => {
                let id = self
                    .chat["id"]
                    .as_str()
                    .context(self.msg("Nessuna conversazione attiva", "No active conversation"))?
                    .to_owned();
                self.start_feed(cx, id);
                self.status.clear();
                self.note(self.msg("Riconnessione allo stream…", "Reconnecting stream…"));
            }
            "/rename" => {
                ensure!(!argument.is_empty(), "/rename <title>");
                self.patch_chat(cx, json!({"title": argument})).await?;
            }
            "/status" => {
                let parts: Vec<String> = [
                    self.remote.workspace.clone(),
                    string(&self.project, "name"),
                    self.model.clone(),
                    string(&self.chat, "permission_mode"),
                    string(&self.chat, "state"),
                    if self.chat.is_null() {
                        String::new()
                    } else if self.feed.is_some() {
                        self.msg("stream attivo", "stream connected").to_owned()
                    } else {
                        self.msg("stream disconnesso", "stream disconnected")
                            .to_owned()
                    },
                ]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect();
                self.note(parts.join(" · "));
            }
            "/quit" | "/exit" => return Ok(true),
            "/browser" => {
                ensure!(argument.is_empty(), "/browser");
                let url = format!("{}/browser", self.remote.workspace);
                let _ = open::that_detached(&url);
                self.note(format!(
                    "{} · {url}",
                    self.msg("Browser remoto", "Remote browser")
                ));
            }
            "/help" => self.note(self.msg(
                "/projects · /use N · /harnesses · /folder (sfoglia) · /folder N radice percorso · /chats · /open N · /models · /model N · /new titolo\n/plan richiesta · /build · /goal obiettivo · /cancel-goal · /answer testo · /allow · /deny · /stop · /resume [N] · /status · /permissions · /diff · /copy · /reconnect · /rename · /browser · /logout · /exit\nF2 conversazioni · F3 progetti/sfoglia · Tab completa · Shift/Alt+Invio o Ctrl+J a capo · Ctrl+A/E/U/K/W modifica · Ctrl+L modello · Ctrl+O strumenti · Rotella/PgSu/PgGiù/Alt+↑↓ scorri · Fine segue · // testo con / · Esc interrompe · Ctrl+C esci",
                "/projects · /use N · /harnesses · /folder (browse) · /folder N root path · /chats · /open N · /models · /model N · /new title\n/plan request · /build · /goal objective · /cancel-goal · /answer text · /allow · /deny · /stop · /resume [N] · /status · /permissions · /diff · /copy · /reconnect · /rename · /browser · /logout · /exit\nF2 chats · F3 projects/browse · Tab complete · Shift/Alt+Enter or Ctrl+J newline · Ctrl+A/E/U/K/W edit · Ctrl+L model · Ctrl+O tools · Wheel/PgUp/PgDn/Alt+↑↓ scroll · End follow · // literal slash · Esc interrupt · Ctrl+C quit",
            )),
            "/projects" | "/harnesses" | "/models" | "/chats" | "/resume" | "/model"
                if argument.is_empty() =>
            {
                self.refresh(cx).await?;
                let listing = match command {
                    "/projects" => entries(&self.projects),
                    "/models" | "/model" => entries(&self.models),
                    "/harnesses" => self
                        .harnesses
                        .iter()
                        .enumerate()
                        .map(|(index, harness)| {
                            format!(
                                "{}  {}  {}",
                                index + 1,
                                string(harness, "name"),
                                harness["roots"]
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => entries(&self.project_chats()),
                };
                self.note(listing);
            }
            "/use" => {
                let project = choice(&self.projects, argument)?.clone();
                self.close();
                self.project = project;
                self.note(entries(&self.project_chats()));
            }
            "/folder" if argument.is_empty() => {
                self.refresh(cx).await?;
                self.start_browse(cx).await?;
                self.open_browse = self.browse.is_some();
            }
            "/folder" => {
                let mut parts = argument.splitn(3, ' ');
                let harness = choice(&self.harnesses, parts.next().unwrap_or(""))?;
                let root = parts.next().context("/folder N root path")?;
                let folder = parts.next().unwrap_or(".");
                let payload = json!({"harness_id":harness["id"],"root":root,"path":folder});
                let project = self
                    .remote
                    .api(cx, Method::Post, "/projects", Some(payload))
                    .await?;
                self.close();
                self.project = project;
                self.refresh(cx).await?;
            }
            "/open" | "/resume" => {
                let id = string(choice(&self.project_chats(), argument)?, "id");
                self.open(cx, &id).await?;
            }
            "/permissions" => {
                if argument.is_empty() {
                    self.note("ask · auto_edit · plan");
                } else {
                    ensure!(
                        ["ask", "auto_edit", "plan"].contains(&argument),
                        "/permissions ask|auto_edit|plan"
                    );
                    self.patch_chat(cx, json!({"permission_mode": argument}))
                        .await?;
                }
            }
            "/model" => {
                let model = string(choice(&self.models, argument)?, "id");
                if !self.chat.is_null() {
                    self.patch_chat(cx, json!({"model": model})).await?;
                }
                self.model = model;
            }
            "/new" => self.new_chat(cx, argument).await?,
            "/stop" => {
                let path = format!("{}/interrupt", self.path()?);
                self.remote.api(cx, Method::Post, &path, None).await?;
            }
            "/allow" | "/deny" => {
                ensure!(
                    !self.approval.is_empty(),
                    "Nessuna approvazione / No pending approval"
                );
                let decision = if command == "/allow" {
                    "allow_once"
                } else {
                    "deny"
                };
                let path = format!("/approvals/{}", self.approval);
                self.remote
                    .api(cx, Method::Post, &path, Some(json!({"decision":decision})))
                    .await?;
            }
            "/goal" | "/plan" => self.send(cx, argument, &command[1..], None).await?,
            "/build" => {
                let id = self.plan.clone();
                ensure!(!id.is_empty(), "Nessun piano / No pending plan");
                let text = self.msg(
                    "Implementa il piano approvato",
                    "Implement the approved plan",
                );
                self.send(cx, text, "implement_plan", Some(&id)).await?;
            }
            "/cancel-goal" => {
                ensure!(!self.goal.is_empty(), "Nessun obiettivo / No active goal");
                let path = format!("{}/goal/{}", self.path()?, self.goal);
                self.remote.api(cx, Method::Delete, &path, None).await?;
            }
            _ if command.starts_with('/') && command != "/answer" => {
                bail!("Comando sconosciuto / Unknown command: {command} (usa // per iniziare con /)")
            }
            _ if !self.question.is_empty() => {
                let question = self.question.clone();
                let answer = if command == "/answer" {
                    argument
                } else {
                    line
                };
                let choice = answer
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| self.choices.get(i))
                    .map(String::as_str)
                    .unwrap_or(answer)
                    .to_owned();
                self.send(cx, &choice, "answer", Some(&question)).await?;
            }
            _ => {
                ensure!(
                    command != "/answer",
                    "Nessuna domanda / No pending question"
                );
                self.send(cx, line, "message", None).await?;
            }
        }
        Ok(false)
    }
}

fn restore_terminal(enhanced: bool) {
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    if enhanced {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::style::ResetColor,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

async fn run_ui(
    cx: &Cx,
    mut app: App,
    mut rx: mpsc::Receiver<(String, StreamUpdate)>,
    start: Option<String>,
) -> Result<()> {
    app.refresh(cx).await?;
    app.model = app
        .models
        .iter()
        .find(|m| m["default"] == true)
        .or(app.models.first())
        .map(|v| string(v, "id"))
        .unwrap_or_default();
    let mut picker = None;
    if let Some(id) = start {
        app.open(cx, &id).await?;
    } else if let [project] = app.projects.as_slice() {
        app.project = project.clone();
        app.welcome();
    } else if app.projects.is_empty() && !app.folder_options().is_empty() {
        app.start_browse(cx).await?;
        picker = Some(Picker::Browse);
    } else {
        picker = Some(Picker::Projects);
    }

    crossterm::terminal::enable_raw_mode()?;
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            restore_terminal(self.0);
        }
    }
    let mut _restore = Restore(false);
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }
    _restore.0 = enhanced;

    // Ensure panics restore the terminal cleanly so the panic trace stays visible.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(enhanced);
        prev_hook(info);
    }));

    let terminated = Arc::new(AtomicBool::new(false));
    let term_flag = terminated.clone();
    let _ = ctrlc::set_handler(move || {
        term_flag.store(true, Ordering::SeqCst);
    });

    // Dedicated terminal input thread feeding events without blocking the async runtime.
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader_flag = stop_reader.clone();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<io::Result<Event>>();
    let reader_thread = std::thread::spawn(move || {
        while !reader_flag.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(40)) {
                Ok(true) => {
                    let ev = crossterm::event::read();
                    if ev_tx.send(ev).is_err() {
                        break;
                    }
                }
                Ok(false) => {}
                Err(err) => {
                    let _ = ev_tx.send(Err(err));
                    break;
                }
            }
        }
    });

    let mut terminal = ftui::TerminalWriter::new(
        io::stdout(),
        ftui::ScreenMode::AltScreen,
        ftui::UiAnchor::Bottom,
        ftui::TerminalCapabilities::detect(),
    );
    let mut draft_history: Vec<String> = Vec::new();
    let mut hist_index = 0;
    let mut caret = 0;
    let mut parked_draft = String::new();
    let mut hide_menu = false;
    use ftui::widgets::spinner::DOTS;
    let mut last_tick = Instant::now();
    let mut frame = 0;
    let mut selection = 0usize;
    let mut dirty = true;
    let mut last_size = crossterm::terminal::size()?;

    'ui: loop {
        if terminated.load(Ordering::Relaxed) {
            break;
        }
        // A /folder command requested the interactive browser: open it now.
        if app.open_browse {
            app.open_browse = false;
            picker = Some(Picker::Browse);
            selection = 0;
            dirty = true;
        }
        // Drain all available SSE stream updates before rendering.
        let mut got_sse = false;
        while let Ok((id, update)) = rx.try_recv() {
            if app.chat["id"] == id {
                got_sse = true;
                dirty = true;
                match update {
                    StreamUpdate::Event(event) => app.event(event),
                    StreamUpdate::Status(msg) => app.status = msg,
                    StreamUpdate::AuthExpired(msg) => {
                        app.feed.take();
                        app.store.clear();
                        app.status = msg;
                    }
                    StreamUpdate::Fatal(error) => {
                        app.feed.take();
                        app.status = error.to_string();
                    }
                }
            }
        }
        if !got_sse && app.replaying && app.opened_at.elapsed() >= REPLAY_BELL_GRACE {
            app.replaying = false;
        }

        // Drain queued terminal input events (up to MAX_EVENTS_PER_FRAME) before rendering.
        let mut processed = 0usize;
        while processed < MAX_EVENTS_PER_FRAME {
            let ev = match ev_rx.try_recv() {
                Ok(item) => item?,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'ui,
            };
            processed += 1;
            match ev {
                Event::Resize(w, h) => {
                    last_size = (w, h);
                    dirty = true;
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll = app.scroll.saturating_add(3);
                        dirty = true;
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll = app.scroll.saturating_sub(3);
                        dirty = true;
                    }
                    _ => {}
                },
                Event::Paste(paste) => {
                    if picker.is_some() {
                        continue;
                    }
                    let text = clean(&paste.replace("\r\n", "\n").replace('\r', "\n"))
                        .replace('\t', "    ");
                    app.input.insert_str(caret, &text);
                    caret += text.len();
                    hide_menu = false;
                    dirty = true;
                }
                Event::Key(k) => {
                    if k.kind == KeyEventKind::Release {
                        continue;
                    }
                    dirty = true;
                    let width = usize::from(last_size.0.saturating_sub(2)).max(1);
                    let options = picker_options(&app, picker);
                    let suggestions: Vec<&str> = if picker.is_some() {
                        options.iter().map(String::as_str).collect()
                    } else {
                        slash_suggestions(&app.input, hide_menu)
                    };
                    selection = selection.min(suggestions.len().saturating_sub(1));

                    match k.code {
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            break 'ui;
                        }
                        KeyCode::F(2) | KeyCode::F(3) => match app.refresh(cx).await {
                            Ok(()) => {
                                if k.code == KeyCode::F(2) {
                                    picker = Some(if app.project.is_null() {
                                        Picker::Projects
                                    } else {
                                        Picker::Chats
                                    });
                                } else if app.projects.is_empty()
                                    && !app.folder_options().is_empty()
                                {
                                    app.start_browse(cx).await?;
                                    picker = Some(Picker::Browse);
                                } else {
                                    picker = Some(Picker::Projects);
                                }
                                selection = 0;
                                app.status.clear();
                            }
                            Err(error) => app.status = error.to_string(),
                        },
                        KeyCode::Char('l') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.models.is_empty() {
                                picker = Some(Picker::Models);
                                selection = app
                                    .models
                                    .iter()
                                    .position(|m| m["id"] == app.model)
                                    .unwrap_or(0);
                            }
                        }
                        KeyCode::Esc if picker == Some(Picker::Browse) => {
                            let at_root = app.browse.as_ref().is_some_and(Browse::at_root);
                            if at_root {
                                picker = None;
                                app.browse = None;
                                selection = 0;
                            } else {
                                if let Some(b) = app.browse.as_mut() {
                                    b.path = parent(&b.path);
                                    b.loading = true;
                                    b.entries.clear();
                                }
                                if let Err(error) = app.reload_browse(cx).await {
                                    app.status = error.to_string();
                                }
                                selection = 0;
                            }
                        }
                        KeyCode::Esc if picker.is_some() => {
                            picker = None;
                            selection = 0;
                        }
                        KeyCode::Enter if let Some(menu) = picker => {
                            if options.is_empty() {
                                continue;
                            }
                            match app.choose(cx, menu, selection).await {
                                Ok(()) => {
                                    // Browse manages its own lifecycle: drill in/out
                                    // keeps the picker open; creating a project clears
                                    // `browse` and closes the picker.
                                    if menu != Picker::Browse || app.browse.is_none() {
                                        picker = None;
                                    }
                                    selection = 0;
                                    app.status.clear();
                                }
                                Err(error) => app.status = error.to_string(),
                            }
                        }
                        _ if picker.is_some() && !matches!(k.code, KeyCode::Up | KeyCode::Down) => {
                        }
                        KeyCode::Char('o') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.compact_tools = !app.compact_tools;
                        }
                        KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            caret = app.input[..caret].rfind('\n').map_or(0, |n| n + 1);
                        }
                        KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            caret += app.input[caret..]
                                .find('\n')
                                .unwrap_or(app.input.len() - caret);
                        }
                        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            let start = app.input[..caret].rfind('\n').map_or(0, |n| n + 1);
                            app.input.drain(start..caret);
                            caret = start;
                            hide_menu = false;
                        }
                        KeyCode::Char('k') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            let end = caret
                                + app.input[caret..]
                                    .find('\n')
                                    .unwrap_or(app.input.len() - caret);
                            app.input.drain(caret..end);
                            hide_menu = false;
                        }
                        KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            delete_prev_word(&mut app.input, &mut caret);
                            hide_menu = false;
                        }
                        KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input.insert(caret, '\n');
                            caret += 1;
                        }
                        KeyCode::Esc if !suggestions.is_empty() => hide_menu = true,
                        KeyCode::Esc if !app.chat.is_null() => {
                            if let Err(e) = app.command(cx, "/stop").await {
                                app.status = e.to_string();
                            }
                        }
                        KeyCode::Up if k.modifiers.contains(KeyModifiers::ALT) => {
                            app.scroll = app.scroll.saturating_add(1);
                        }
                        KeyCode::Down if k.modifiers.contains(KeyModifiers::ALT) => {
                            app.scroll = app.scroll.saturating_sub(1);
                        }
                        KeyCode::Up if !suggestions.is_empty() => {
                            selection = selection.saturating_sub(1);
                        }
                        KeyCode::Down if !suggestions.is_empty() => {
                            selection = (selection + 1).min(suggestions.len() - 1);
                        }
                        KeyCode::Tab if !suggestions.is_empty() => {
                            app.input = format!("{} ", suggestions[selection]);
                            caret = app.input.len();
                            selection = 0;
                        }
                        KeyCode::PageUp => app.scroll = app.scroll.saturating_add(12),
                        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(12),
                        KeyCode::Up | KeyCode::Down
                            if let Some(next) = editor::vertical(
                                &app.input,
                                caret,
                                width,
                                k.code == KeyCode::Down,
                            ) =>
                        {
                            caret = next;
                        }
                        KeyCode::Up => {
                            if hist_index > 0 {
                                if hist_index == draft_history.len() {
                                    parked_draft = app.input.clone();
                                }
                                hist_index -= 1;
                                app.input = draft_history[hist_index].clone();
                                caret = app.input.len();
                            }
                        }
                        KeyCode::Down if hist_index < draft_history.len() => {
                            hist_index = (hist_index + 1).min(draft_history.len());
                            app.input = draft_history
                                .get(hist_index)
                                .cloned()
                                .unwrap_or_else(|| parked_draft.clone());
                            caret = app.input.len();
                        }
                        KeyCode::Left => {
                            caret = app.input[..caret]
                                .grapheme_indices(true)
                                .next_back()
                                .map_or(0, |(pos, _)| pos);
                        }
                        KeyCode::Right => {
                            caret += app.input[caret..]
                                .graphemes(true)
                                .next()
                                .map_or(0, str::len);
                        }
                        KeyCode::Home => {
                            caret = app.input[..caret].rfind('\n').map_or(0, |n| n + 1);
                        }
                        KeyCode::End if app.scroll > 0 && app.input.is_empty() => app.scroll = 0,
                        KeyCode::End => {
                            if caret == app.input.len() && app.scroll > 0 {
                                app.scroll = 0;
                            } else {
                                caret += app.input[caret..]
                                    .find('\n')
                                    .unwrap_or(app.input.len() - caret);
                            }
                        }
                        KeyCode::Delete if caret < app.input.len() => {
                            let n = app.input[caret..].graphemes(true).next().unwrap().len();
                            app.input.drain(caret..caret + n);
                            hide_menu = false;
                        }
                        KeyCode::Backspace if caret > 0 => {
                            let prev = app.input[..caret]
                                .grapheme_indices(true)
                                .next_back()
                                .unwrap()
                                .0;
                            app.input.drain(prev..caret);
                            caret = prev;
                            hide_menu = false;
                        }
                        KeyCode::Char(c)
                            if !k.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) =>
                        {
                            hide_menu = false;
                            selection = 0;
                            app.input.insert(caret, c);
                            caret += c.len_utf8();
                        }
                        KeyCode::Enter
                            if k.modifiers
                                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                        {
                            app.input.insert(caret, '\n');
                            caret += 1;
                        }
                        KeyCode::Enter => {
                            let line = app.input.trim().to_owned();
                            if line.is_empty() {
                                continue;
                            }
                            if needs_tab_completion(&line, &suggestions) {
                                app.status = app
                                    .msg(
                                        "Premi Tab per completare il comando",
                                        "Press Tab to complete the command",
                                    )
                                    .to_owned();
                                continue;
                            }
                            app.status.clear();
                            match app.command(cx, &line).await {
                                Ok(quit) => {
                                    if quit {
                                        break 'ui;
                                    }
                                    draft_history.push(line);
                                    if draft_history.len() > MAX_DRAFT_HISTORY {
                                        let excess = draft_history.len() - MAX_DRAFT_HISTORY;
                                        draft_history.drain(..excess);
                                    }
                                    hist_index = draft_history.len();
                                    app.input.clear();
                                    parked_draft.clear();
                                    app.scroll = 0;
                                    hide_menu = false;
                                    caret = 0;
                                }
                                Err(e) => app.status = e.to_string(),
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let size = crossterm::terminal::size()?;
        if size != last_size {
            last_size = size;
            dirty = true;
        }
        let working = app.working();
        if working && last_tick.elapsed() >= Duration::from_millis(150) {
            frame = (frame + 1) % DOTS.len();
            last_tick = Instant::now();
            dirty = true;
        }

        if dirty {
            let options = picker_options(&app, picker);
            let suggestions: Vec<&str> = if picker.is_some() {
                options.iter().map(String::as_str).collect()
            } else {
                slash_suggestions(&app.input, hide_menu)
            };
            selection = selection.min(suggestions.len().saturating_sub(1));
            if (terminal.width(), terminal.height()) != size {
                terminal.set_size(size.0, size.1);
            }
            let buffer = terminal.take_render_buffer(size.0, size.1);
            let mut screen = ftui::Frame::from_buffer(buffer, terminal.pool_mut());
            let picker_label = picker.map(|p| p.label(&app));
            view::draw(
                &mut screen,
                &mut app,
                caret,
                &suggestions,
                selection,
                working.then_some(DOTS[frame]),
                picker_label,
            );
            let (buffer, cursor) = (screen.buffer, screen.cursor_position);
            terminal.present_ui_owned(buffer, cursor, cursor.is_some())?;
            if terminal.pool().len() > usize::from(size.0) * usize::from(size.1) {
                terminal.gc(None);
            }
            dirty = false;
        }

        if processed < MAX_EVENTS_PER_FRAME && !got_sse {
            sleep(wall_now(), Duration::from_millis(25)).await;
        }
    }
    stop_reader.store(true, Ordering::Relaxed);
    app.close();
    let _ = reader_thread.join();
    Ok(())
}

fn print_help() {
    let console = Console::new();
    let mut table = Table::new()
        .title(format!("venticode-tui v{}", env!("CARGO_PKG_VERSION")))
        .with_column(Column::new("Option").style(Style::parse("bold cyan").unwrap_or_default()))
        .with_column(Column::new("Description"));
    for (flag, desc) in [
        ("--portal <URL>", "Ventic portal HTTPS origin"),
        (
            "--standalone <URL>",
            "Standalone Venticode origin (uses access token)",
        ),
        (
            "--conversation <UUID>",
            "Open a specific conversation at startup",
        ),
        ("--en", "Use English UI copy"),
        ("--manual", "Paste the one-time sign-in code manually"),
        (
            "--no-browser",
            "Do not launch a system browser (implies --manual)",
        ),
        ("--ticket-stdin", "Read the one-time code from stdin"),
        ("--ca <PEM>", "Additional PEM root CA certificate"),
        ("--version, -V", "Print version information"),
        ("--help, -h", "Show this help"),
    ] {
        table.add_row_cells([flag, desc]);
    }
    console.print_renderable(&table);
}

fn next_flag_value(
    flag: &str,
    inline: Option<&str>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String> {
    if let Some(val) = inline {
        Ok(val.to_owned())
    } else {
        args.next()
            .with_context(|| format!("{flag} requires a value"))
    }
}

async fn run_cli(cx: Cx) -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut portal = None;
    let mut standalone = None;
    let mut start = None;
    let mut stdin = false;
    let mut browser = true;
    let mut manual = false;
    let mut ca = None;
    let mut en = false;
    while let Some(arg) = args.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
        match flag {
            "--portal" => portal = Some(next_flag_value("--portal", inline, &mut args)?),
            "--standalone" => {
                let raw = next_flag_value("--standalone", inline, &mut args)?;
                standalone = Some(client::standalone_origin(&raw)?);
            }
            "--conversation" => {
                start = Some(next_flag_value("--conversation", inline, &mut args)?);
            }
            "--ticket-stdin" => stdin = true,
            "--no-browser" => {
                browser = false;
                manual = true;
            }
            "--manual" => manual = true,
            "--ca" => {
                ca = Some(std::path::PathBuf::from(next_flag_value(
                    "--ca", inline, &mut args,
                )?));
            }
            "--en" => en = true,
            "--version" | "-V" => {
                println!("venticode-tui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => return Err(anyhow!("Unknown option: {arg}")),
        }
    }
    if standalone.is_some() && (portal.is_some() || stdin || manual) {
        return Err(anyhow!(
            "--standalone uses an access token and cannot be combined with the portal sign-in options"
        ));
    }
    let console = Console::new();
    let portal = match portal.or_else(|| standalone.clone()) {
        Some(value) if standalone.is_some() => value,
        Some(value) => origin(&value)?,
        None => {
            console.print(if en {
                "[bold cyan]Ventic portal HTTPS URL:[/]"
            } else {
                "[bold cyan]URL HTTPS del portale Ventic:[/]"
            });
            let mut value = String::new();
            io::stdin().read_line(&mut value)?;
            origin(value.trim())?
        }
    };
    let store = store::SessionStore::open(
        if standalone.is_some() {
            "venticode-tui-standalone"
        } else {
            "venticode-tui"
        },
        &portal,
    );
    let mut session = None;
    if !stdin && let Some(saved) = store.load() {
        match Remote::restore(ca.as_deref(), &saved, standalone.is_some()) {
            Ok(remote) => {
                match remote.valid(&cx).await {
                    Ok(true) => session = Some(remote),
                    Ok(false) => {
                        store.clear();
                    }
                    Err(err) => {
                        console.print(&format!(
                            "[yellow]{}[/]",
                            if en {
                                format!("Could not validate saved session ({err}); signing in again.")
                            } else {
                                format!("Impossibile verificare la sessione salvata ({err}); nuovo accesso.")
                            }
                        ));
                    }
                }
            }
            Err(err) => {
                console.print(&format!(
                    "[yellow]{}[/]",
                    if en {
                        format!("Ignoring corrupt saved session ({err}); signing in again.")
                    } else {
                        format!("Sessione salvata non valida ({err}); nuovo accesso.")
                    }
                ));
                store.clear();
            }
        }
    }
    let remote = if let Some(remote) = session {
        remote
    } else {
        let code = if standalone.is_some() {
            rpassword::prompt_password(if en {
                "Venticode access token: "
            } else {
                "Token di accesso Venticode: "
            })?
        } else if stdin {
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            line
        } else if manual {
            let url = format!("{portal}/app/venticode?terminal=1");
            let msg = if en {
                format!("Sign in in your browser:\n{url}")
            } else {
                format!("Accedi nel browser:\n{url}")
            };
            let panel = Panel::from_text(&msg)
                .title("Venticode")
                .border_style(Style::parse("cyan").unwrap_or_default());
            console.print_renderable(&panel);
            if browser {
                let _ = open::that_detached(&url);
            }
            rpassword::prompt_password(if en {
                "One-time code: "
            } else {
                "Codice monouso: "
            })?
        } else {
            login::browser(&cx, &portal, en).await?
        };
        let remote = if standalone.is_some() {
            Remote::connect_standalone(&cx, ca.as_deref(), &portal, code.trim()).await?
        } else {
            Remote::connect(&cx, ca.as_deref(), &portal, code.trim()).await?
        };
        if let Err(e) = store.save(&remote.saved()?) {
            console.print(&format!(
                "[dim]{}[/]",
                if en {
                    format!("Session available for this launch only: {e}")
                } else {
                    format!("Sessione solo in memoria: {e}")
                }
            ));
        } else if store.is_file_backend() {
            let where_ = store
                .file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            console.print(&format!(
                "[dim]{}[/]",
                if en {
                    format!("Session saved unencrypted in {where_}")
                } else {
                    format!("Sessione salvata senza cifratura in {where_}")
                }
            ));
        }
        remote
    };
    let (tx, rx) = mpsc::channel(256);
    run_ui(
        &cx,
        App {
            remote,
            store,
            en,
            projects: vec![],
            models: vec![],
            harnesses: vec![],
            chats: vec![],
            project: Value::Null,
            chat: Value::Null,
            model: String::new(),
            lines: vec![],
            compact_tools: true,
            last_text: String::new(),
            patches: Vec::new(),
            input: String::new(),
            status: String::new(),
            question: String::new(),
            choices: vec![],
            retry: None,
            plan: String::new(),
            goal: String::new(),
            approval: String::new(),
            scroll: 0,
            last_sequence: 0,
            replaying: false,
            opened_at: Instant::now(),
            feed: None,
            tx,
            browse: None,
            open_browse: false,
        },
        rx,
        start,
    )
    .await
}

fn main() {
    let runtime = match RuntimeBuilder::new().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Failed to start async runtime: {err}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async {
        let cx = Cx::current().expect("asupersync runtime context");
        run_cli(cx).await
    });
    if let Err(err) = result {
        let console = Console::new();
        let text = format!("{err:#}");
        let panel = Panel::from_text(&text)
            .title("venticode-tui")
            .border_style(Style::parse("bold red").unwrap_or_default());
        console.print_renderable(&panel);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let saved = serde_json::json!({
            "url": "https://workspace.example",
            "cookie": "venticode_session=test"
        })
        .to_string();
        let remote = Remote::restore(None, &saved, false).unwrap();
        let store =
            crate::store::SessionStore::open("venticode-tui-test", "https://workspace.example");
        let (tx, _rx) = mpsc::channel(16);
        App {
            remote,
            store,
            en: true,
            projects: vec![],
            models: vec![],
            harnesses: vec![],
            chats: vec![],
            project: Value::Null,
            chat: Value::Null,
            model: String::new(),
            lines: vec![],
            compact_tools: true,
            last_text: String::new(),
            patches: Vec::new(),
            input: String::new(),
            status: String::new(),
            question: String::new(),
            choices: vec![],
            retry: None,
            plan: String::new(),
            goal: String::new(),
            approval: String::new(),
            scroll: 0,
            last_sequence: 0,
            replaying: false,
            opened_at: Instant::now(),
            feed: None,
            tx,
            browse: None,
            open_browse: false,
        }
    }

    #[test]
    fn ctrl_w_handles_multibyte_unicode_whitespace_without_panicking() {
        let mut input = "hello\u{00A0}world".to_owned();
        let mut caret = input.len();
        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "hello\u{00A0}");
        assert_eq!(caret, input.len());

        // Second Ctrl+W trims the trailing U+00A0 and deletes "hello".
        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "");
        assert_eq!(caret, 0);

        // Ideographic space U+3000 (3 bytes in UTF-8).
        let mut input = "foo\u{3000}bar\u{3000}".to_owned();
        let mut caret = input.len();
        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "foo\u{3000}");
        assert_eq!(caret, input.len());

        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "");
        assert_eq!(caret, 0);
    }

    #[test]
    fn sequence_deduplication_ignores_replayed_events_on_reconnect() {
        let mut app = test_app();
        app.event(json!({
            "sequence": 1,
            "type": "user_message",
            "content": "hello"
        }));
        app.event(json!({
            "sequence": 2,
            "type": "text",
            "message_id": "m1",
            "delta": "world"
        }));
        assert_eq!(app.last_sequence, 2);
        assert_eq!(app.lines.len(), 2);

        // Reconnect replays seq 1 and 2 — must be ignored (no duplicates).
        app.event(json!({
            "sequence": 1,
            "type": "user_message",
            "content": "hello"
        }));
        app.event(json!({
            "sequence": 2,
            "type": "text",
            "message_id": "m1",
            "delta": "world"
        }));
        assert_eq!(app.last_sequence, 2);
        assert_eq!(app.lines.len(), 2);
        assert_eq!(app.lines[1].text, "world");

        // Newer event seq 3 is accepted.
        app.event(json!({
            "sequence": 3,
            "type": "text",
            "message_id": "m1",
            "delta": "!"
        }));
        assert_eq!(app.last_sequence, 3);
        assert_eq!(app.lines[1].text, "world!");

        // Closing conversation resets last_sequence to 0.
        app.close();
        assert_eq!(app.last_sequence, 0);
    }

    #[test]
    fn slash_command_enter_requires_exact_match_or_arguments() {
        let partial = slash_suggestions("/q", false);
        assert_eq!(partial, vec!["/quit"]);
        assert!(needs_tab_completion("/q", &partial));

        let exact = slash_suggestions("/quit", false);
        assert!(!needs_tab_completion("/quit", &exact));

        let with_args = slash_suggestions("/model 1", false);
        assert!(!needs_tab_completion("/model 1", &with_args));

        let escaped = slash_suggestions("//q", false);
        assert!(!needs_tab_completion("//q", &escaped));
    }

    #[test]
    fn output_cap_preserves_utf8_boundaries() {
        let mut buf = String::new();
        // 'é' is 2 bytes; capping at 5 bytes must cut at 4 bytes without panicking.
        append_capped_output(&mut buf, "éééé", 5);
        assert!(buf.starts_with("éé"));
        assert!(buf.contains("output truncated"));
        let len_after = buf.len();
        append_capped_output(&mut buf, "more", 5);
        assert_eq!(buf.len(), len_after);
    }
}
