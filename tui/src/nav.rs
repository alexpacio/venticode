//! Lateral panes: the navigator tree on the left (workspace links, projects
//! and conversations, harness folders, bots and mentions) and the inspector
//! on the right, which details the highlighted node or the open session.

use crate::app::{Act, App, Field, FieldKind, Focus, Form, Screen, clean, string};
use crate::platform::Cx;
use crate::screens::{self, mention_glyph};
use crate::view::{BLUE, DIM, GOLD, GRAY, GREEN, ORANGE, RED, WHITE, fg, paint};
use anyhow::Result;
use ftui::core::event::{KeyCode, KeyEvent, Modifiers};
use ftui::{
    Frame, Widget,
    core::geometry::Rect,
    style::{Color, Style},
    text::{Line, Span, Text, WrapMode},
    widgets::paragraph::Paragraph,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// Entries shown per folder; huge directories stay navigable.
const MAX_DIR_ENTRIES: usize = 300;
const SELECTED_BG: u8 = 237;

#[derive(Default)]
pub struct Nav {
    /// Ctrl+B or the `‹` on its divider collapses the tree (it still opens
    /// while it has focus).
    pub hidden: bool,
    /// Ctrl+Alt+B or the `›` on its divider collapses the inspector.
    pub inspector_hidden: bool,
    /// Key of the highlighted row; keys survive reloads, indexes do not.
    pub key: String,
    /// `/` search over conversation titles; Some while active.
    pub filter: Option<String>,
    typing: bool,
    /// Keys whose expansion differs from the default (sections open, items closed).
    toggled: HashSet<String>,
    dirs: HashMap<String, Result<Vec<Value>, String>>,
    mentions: HashMap<String, Result<Vec<Value>, String>>,
    /// Layout of the last frame, for mouse hit-testing.
    rect: Rect,
    /// Clickable dividers of the tree and the inspector.
    dividers: [Rect; 2],
    offset: usize,
    inspector: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Link,
    Section,
    Project,
    Chat,
    Harness,
    Dir,
    File,
    Bot,
    Mention,
    Info,
}

pub struct Row {
    pub key: String,
    pub depth: usize,
    pub kind: Kind,
    /// The project, conversation, harness, directory entry, bot or mention.
    pub item: Value,
    /// Harness of a folder or file, bot of a mention.
    pub owner: Value,
    pub root: String,
    pub path: String,
}

impl Row {
    fn new(key: String, depth: usize, kind: Kind, item: &Value) -> Self {
        Row {
            key,
            depth,
            kind,
            item: item.clone(),
            owner: Value::Null,
            root: String::new(),
            path: String::new(),
        }
    }
    fn info(key: String, depth: usize, text: &str) -> Self {
        Row::new(key, depth, Kind::Info, &json!(text))
    }
    fn expandable(&self) -> bool {
        matches!(
            self.kind,
            Kind::Section | Kind::Project | Kind::Harness | Kind::Dir | Kind::Bot
        )
    }
}

impl Nav {
    pub fn is_open(&self, key: &str) -> bool {
        self.toggled.contains(key) ^ key.starts_with("s:")
    }
    fn set_open(&mut self, key: &str, open: bool) {
        if self.is_open(key) != open && !self.toggled.remove(key) {
            self.toggled.insert(key.to_owned());
        }
    }
    /// Highlight `key` and make sure the panes are visible.
    pub fn reveal(&mut self, key: String) {
        self.hidden = false;
        self.key = key;
    }
    pub fn contains(&self, x: u16, y: u16) -> bool {
        let r = self.rect;
        r.width > 0 && x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
    }
    pub fn scroll_by(&mut self, delta: isize) {
        self.offset = self.offset.saturating_add_signed(delta);
    }
    pub fn mentions(&self, bot: &str) -> Option<&Result<Vec<Value>, String>> {
        self.mentions.get(bot)
    }
    pub fn set_mentions(&mut self, bot: &str, list: Result<Vec<Value>, String>) {
        self.mentions.insert(bot.to_owned(), list);
    }
    pub fn inspector_visible(&self) -> bool {
        self.inspector
    }
    /// Highlight an opened conversation and expand its project.
    pub fn track(&mut self, chat: &Value) {
        if let Some(project) = chat["project_id"].as_str() {
            self.set_open(&format!("p:{project}"), true);
        }
        self.key = format!("c:{}", string(chat, "id"));
    }
}

pub fn project_key(app: &App) -> String {
    app.project["id"]
        .as_str()
        .map_or_else(|| "s:projects".to_owned(), |id| format!("p:{id}"))
}

fn dir_key(harness: &str, root: &str, path: &str) -> String {
    format!("d:{harness}\0{root}\0{path}")
}

fn join(path: &str, name: &str) -> String {
    if path == "." || path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}/{name}")
    }
}

fn matches(app: &App, chat: &Value) -> bool {
    app.nav.filter.as_ref().is_none_or(|q| {
        string(chat, "title")
            .to_lowercase()
            .contains(&q.to_lowercase())
    })
}

fn dir_rows(out: &mut Vec<Row>, app: &App, harness: &Value, root: &str, path: &str, depth: usize) {
    let hid = string(harness, "id");
    let key = dir_key(&hid, root, path);
    let mut row = Row::new(key.clone(), depth, Kind::Dir, &Value::Null);
    row.owner = harness.clone();
    row.root = root.to_owned();
    row.path = path.to_owned();
    out.push(row);
    if !app.nav.is_open(&key) {
        return;
    }
    let info = |text: &str| Row::info(format!("i:{key}"), depth + 1, text);
    match app.nav.dirs.get(&key) {
        None => out.push(info(app.msg("Caricamento…", "Loading…"))),
        Some(Err(error)) => out.push(info(error)),
        Some(Ok(entries)) if entries.is_empty() => {
            out.push(info(app.msg("Cartella vuota", "Empty folder")))
        }
        Some(Ok(entries)) => {
            let (dirs, files): (Vec<&Value>, Vec<&Value>) =
                entries.iter().partition(|e| e["directory"] == true);
            for entry in dirs.iter().chain(&files).take(MAX_DIR_ENTRIES) {
                let child = join(path, &string(entry, "name"));
                if entry["directory"] == true {
                    dir_rows(out, app, harness, root, &child, depth + 1);
                } else {
                    let mut row = Row::new(
                        format!("f:{hid}\0{root}\0{child}"),
                        depth + 1,
                        Kind::File,
                        entry,
                    );
                    row.owner = harness.clone();
                    row.root = root.to_owned();
                    row.path = child;
                    out.push(row);
                }
            }
            if entries.len() > MAX_DIR_ENTRIES {
                out.push(info(&format!("… +{}", entries.len() - MAX_DIR_ENTRIES)));
            }
        }
    }
}

/// Flatten the visible tree. Row keys are stable across reloads.
pub fn rows(app: &App) -> Vec<Row> {
    let nav = &app.nav;
    let filtering = nav.filter.as_ref().is_some_and(|q| !q.is_empty());
    let mut out = Vec::new();

    if app.live.available {
        out.push(Row::new("l:browser".into(), 0, Kind::Link, &Value::Null));
    }
    if app.desktop {
        out.push(Row::new("l:desktop".into(), 0, Kind::Link, &Value::Null));
    }
    if app.admin() || !app.connections.is_empty() {
        out.push(Row::new(
            "l:connections".into(),
            0,
            Kind::Link,
            &Value::Null,
        ));
    }

    out.push(Row::new(
        "s:projects".into(),
        0,
        Kind::Section,
        &Value::Null,
    ));
    if nav.is_open("s:projects") {
        if app.projects.is_empty() {
            out.push(Row::info(
                "i:projects".into(),
                1,
                app.msg(
                    "Nessun progetto · apri un harness e premi n su una cartella",
                    "No projects · open a harness and press n on a folder",
                ),
            ));
        }
        for project in &app.projects {
            let id = string(project, "id");
            let key = format!("p:{id}");
            let chats: Vec<&Value> = app
                .chats
                .iter()
                .filter(|c| c["project_id"] == project["id"] && matches(app, c))
                .collect();
            if filtering && chats.is_empty() {
                continue;
            }
            out.push(Row::new(key.clone(), 1, Kind::Project, project));
            if nav.is_open(&key) || filtering {
                if chats.is_empty() {
                    out.push(Row::info(
                        format!("i:{key}"),
                        2,
                        app.msg(
                            "Nessuna conversazione · n per iniziare",
                            "No conversations · n to start",
                        ),
                    ));
                }
                for chat in chats {
                    out.push(Row::new(
                        format!("c:{}", string(chat, "id")),
                        2,
                        Kind::Chat,
                        chat,
                    ));
                }
            }
        }
    }

    let loose: Vec<&Value> = app
        .chats
        .iter()
        .filter(|c| !c["project_id"].is_string() && matches(app, c))
        .collect();
    if !loose.is_empty() {
        out.push(Row::new("s:chats".into(), 0, Kind::Section, &Value::Null));
        if nav.is_open("s:chats") || filtering {
            for chat in loose {
                out.push(Row::new(
                    format!("c:{}", string(chat, "id")),
                    1,
                    Kind::Chat,
                    chat,
                ));
            }
        }
    }

    out.push(Row::new(
        "s:harnesses".into(),
        0,
        Kind::Section,
        &Value::Null,
    ));
    if nav.is_open("s:harnesses") {
        if app.harnesses.is_empty() {
            out.push(Row::info(
                "i:harnesses".into(),
                1,
                app.msg(
                    "Nessun harness collegato · o apre il portale",
                    "No harness connected · o opens the portal",
                ),
            ));
        }
        for harness in &app.harnesses {
            let key = format!("h:{}", string(harness, "id"));
            out.push(Row::new(key.clone(), 1, Kind::Harness, harness));
            if nav.is_open(&key) {
                for root in harness["roots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    dir_rows(&mut out, app, harness, root, ".", 2);
                }
            }
        }
    }

    out.push(Row::new("s:bots".into(), 0, Kind::Section, &Value::Null));
    if nav.is_open("s:bots") {
        if app.bots.is_empty() {
            out.push(Row::info(
                "i:bots".into(),
                1,
                app.msg("Nessun bot disponibile", "No bots available"),
            ));
        }
        for bot in &app.bots {
            let id = string(bot, "id");
            let key = format!("b:{id}");
            out.push(Row::new(key.clone(), 1, Kind::Bot, bot));
            if !nav.is_open(&key) {
                continue;
            }
            let info = |text: &str| Row::info(format!("i:{key}"), 2, text);
            match nav.mentions.get(&id) {
                None => out.push(info(app.msg("Caricamento…", "Loading…"))),
                Some(Err(error)) => out.push(info(error)),
                Some(Ok(list)) if list.is_empty() => {
                    out.push(info(app.msg("Nessuna menzione", "No mentions")))
                }
                Some(Ok(list)) => {
                    for mention in list {
                        let mut row = Row::new(
                            format!("m:{}", string(mention, "id")),
                            2,
                            Kind::Mention,
                            mention,
                        );
                        row.owner = bot.clone();
                        out.push(row);
                    }
                }
            }
        }
    }
    out
}

fn selected(rows: &[Row], key: &str) -> usize {
    rows.iter().position(|r| r.key == key).unwrap_or(0)
}

fn label(app: &App, row: &Row) -> Vec<Span<'static>> {
    let open = app.nav.is_open(&row.key);
    let arrow = if !row.expandable() {
        "  "
    } else if open {
        "▾ "
    } else {
        "▸ "
    };
    let mut spans = vec![
        Span::raw("  ".repeat(row.depth)),
        Span::styled(arrow, fg(DIM)),
    ];
    let item = &row.item;
    let current = |on: bool| if on { fg(ORANGE).bold() } else { fg(WHITE) };
    match row.kind {
        Kind::Link => {
            let (glyph, text, screen) = match row.key.as_str() {
                "l:browser" => (
                    "↗ ",
                    app.msg("Browser remoto", "Remote browser"),
                    Screen::Browser,
                ),
                "l:desktop" => (
                    "▣ ",
                    app.msg("Desktop remoto", "Remote desktop"),
                    Screen::Desktop,
                ),
                _ => (
                    "↔ ",
                    app.msg("Connessioni bot", "Bot connections"),
                    Screen::Connections,
                ),
            };
            spans.pop();
            spans.push(Span::styled(glyph, fg(BLUE)));
            spans.push(Span::styled(text, current(app.screen == screen)));
        }
        Kind::Section => {
            let (name, count) = match row.key.as_str() {
                "s:projects" => (app.msg("PROGETTI", "PROJECTS"), app.projects.len()),
                "s:chats" => (
                    app.msg("CONVERSAZIONI", "CONVERSATIONS"),
                    app.chats
                        .iter()
                        .filter(|c| !c["project_id"].is_string())
                        .count(),
                ),
                "s:harnesses" => {
                    let online = app.harnesses.iter().filter(|h| h["online"] == true).count();
                    spans.push(Span::styled("HARNESS", fg(GRAY).bold()));
                    spans.push(Span::styled(
                        format!(" {online}/{} online", app.harnesses.len()),
                        fg(DIM),
                    ));
                    return spans;
                }
                _ => ("BOT", app.bots.len()),
            };
            spans.push(Span::styled(name, fg(GRAY).bold()));
            spans.push(Span::styled(format!(" {count}"), fg(DIM)));
        }
        Kind::Project => {
            spans.push(Span::styled("◆ ", fg(ORANGE)));
            spans.push(Span::styled(
                clean(&string(item, "name")),
                current(item["id"] == app.project["id"] && app.screen == Screen::Chat),
            ));
        }
        Kind::Chat => {
            let (glyph, color) = match item["state"].as_str() {
                Some("running") => ("✻ ", GOLD),
                Some("waiting") => ("◆ ", GOLD),
                Some("failed") => ("✗ ", RED),
                _ => ("› ", DIM),
            };
            spans.push(Span::styled(glyph, fg(color)));
            let title = string(item, "title");
            spans.push(Span::styled(
                if title.is_empty() {
                    app.msg("Senza titolo", "Untitled").to_owned()
                } else {
                    clean(&title)
                },
                current(item["id"] == app.chat["id"] && app.screen == Screen::Chat),
            ));
        }
        Kind::Harness => {
            let online = item["online"] == true;
            spans.push(Span::styled(
                if online { "● " } else { "○ " },
                fg(if online { GREEN } else { DIM }),
            ));
            spans.push(Span::styled(
                clean(&string(item, "name")),
                fg(if online { WHITE } else { GRAY }),
            ));
            if item["mode"] == "yolo" {
                spans.push(Span::styled(" yolo", fg(RED)));
            }
            if item["owned"] == false {
                spans.push(Span::styled(app.msg(" condiviso", " shared"), fg(DIM)));
            }
        }
        Kind::Dir if row.path == "." => {
            spans.push(Span::styled("⌂ ", fg(BLUE)));
            spans.push(Span::styled(clean(&row.root), fg(BLUE).bold()));
        }
        Kind::Dir => {
            let name = row.path.rsplit('/').next().unwrap_or(&row.path);
            spans.push(Span::styled(format!("{}/", clean(name)), fg(BLUE)));
        }
        Kind::File => spans.push(Span::styled(clean(&string(item, "name")), fg(GRAY))),
        Kind::Bot => {
            let enabled = item["enabled"] == true;
            spans.push(Span::styled("● ", fg(if enabled { GREEN } else { DIM })));
            spans.push(Span::styled(
                clean(&string(item, "handle")),
                current(app.screen == Screen::Bot(string(item, "id"))),
            ));
            spans.push(Span::styled(
                format!(" {}", clean(&string(item, "name"))),
                fg(DIM),
            ));
        }
        Kind::Mention => {
            let (glyph, color) = mention_glyph(item["state"].as_str().unwrap_or(""));
            spans.push(Span::styled(format!("{glyph} "), fg(color)));
            spans.push(Span::styled(
                format!("{}: ", clean(&string(item, "author"))),
                fg(GRAY),
            ));
            spans.push(Span::styled(
                clean(&string(item, "text")).replace('\n', " "),
                fg(WHITE),
            ));
        }
        Kind::Info => spans.push(Span::styled(
            clean(item.as_str().unwrap_or("")),
            fg(DIM).italic(),
        )),
    }
    spans
}

/// Key hints for the highlighted row, shown at the foot of the tree.
fn hints(app: &App, row: Option<&Row>) -> String {
    if app.nav.typing {
        return app
            .msg(
                "Scrivi per cercare · Invio conferma · Esc annulla",
                "Type to search · Enter keep · Esc clear",
            )
            .into();
    }
    let action = match row.map(|r| r.kind) {
        Some(Kind::Link) => app.msg("Invio apri", "Enter open"),
        Some(Kind::Project) => app.msg(
            "Invio apri · n nuova chat · d elimina",
            "Enter open · n new chat · d delete",
        ),
        Some(Kind::Chat) => app.msg("Invio riprendi · d elimina", "Enter resume · d delete"),
        Some(Kind::Harness) => app.msg(
            "Invio espandi · a aggiungi progetto · o portale",
            "Enter expand · a add project · o portal",
        ),
        Some(Kind::Dir) => app.msg(
            "Invio espandi · n crea progetto",
            "Enter expand · n new project",
        ),
        Some(Kind::Bot) => app.msg(
            "Invio impostazioni · → menzioni · e attiva",
            "Enter settings · → mentions · e enable",
        ),
        Some(Kind::Mention) if row.is_some_and(|r| r.item["state"] == "review") => app.msg(
            "a approva · x scarta · Invio chat",
            "a approve · x discard · Enter chat",
        ),
        Some(Kind::Mention) => app.msg("Invio apri chat", "Enter open chat"),
        _ => app.msg("Invio espandi", "Enter expand"),
    };
    format!(
        "{action} · / {} · r {} · Tab",
        app.msg("cerca", "search"),
        app.msg("aggiorna", "refresh")
    )
}

/// A pane divider with its collapse/expand icon on top; the whole column is
/// the click target.
fn divider(f: &mut Frame, x: u16, top: u16, height: u16, icon: &'static str) -> Rect {
    paint(
        f,
        Rect::new(x, top, 1, height),
        (0..height).map(|i| {
            if i == 0 {
                Line::styled(icon, fg(GRAY))
            } else {
                Line::styled("│", fg(DIM))
            }
        }),
    );
    Rect::new(x, top, 1, height)
}

/// Collapse or expand the tree (`inspector == false`) or the inspector.
pub fn toggle(app: &mut App, inspector: bool) {
    if inspector {
        app.nav.inspector_hidden = !app.nav.inspector_hidden;
        return;
    }
    let open = !app.nav.hidden || app.focus == Focus::Nav;
    app.nav.hidden = open;
    if open && app.focus == Focus::Nav {
        app.focus = Focus::Editor;
    }
}

/// Split the screen into [tree | center | inspector] and draw both panes.
/// A collapsed pane leaves its divider as a one-column rail to reopen it.
/// Returns the center area for the active screen.
pub fn draw(f: &mut Frame, app: &mut App) -> Rect {
    let area = f.area();
    let focused = app.focus == Focus::Nav;
    app.nav.inspector = false;
    app.nav.rect = Rect::default();
    app.nav.dividers = [Rect::default(); 2];
    let narrow = area.width < 90;
    if area.width < 40 || app.screen == Screen::SignedOut || (narrow && !focused) {
        return area;
    }
    // On narrow terminals the focused tree takes the whole screen.
    if narrow {
        draw_tree(f, area, app);
        return Rect::default();
    }
    let tree_open = !app.nav.hidden || focused;
    let tree_width = if tree_open {
        (area.width / 4).clamp(28, 40)
    } else {
        0
    };
    if tree_open {
        draw_tree(f, Rect::new(area.x, area.y, tree_width, area.height), app);
    }
    let left = area.x + tree_width;
    let icon = if tree_open { "‹" } else { "›" };
    app.nav.dividers[0] = divider(f, left, area.y, area.height, icon);
    // Column of the right divider, or the screen end without an inspector.
    let mut right = area.x + area.width;
    if area.width >= 130 {
        let open = !app.nav.inspector_hidden;
        let inspector = if open {
            (area.width / 4).clamp(30, 46)
        } else {
            0
        };
        right -= inspector + 1;
        let icon = if open { "›" } else { "‹" };
        app.nav.dividers[1] = divider(f, right, area.y, area.height, icon);
        if open {
            app.nav.inspector = true;
            draw_inspector(
                f,
                Rect::new(right + 2, area.y, inspector - 2, area.height),
                app,
            );
        }
    }
    Rect::new(
        left + 2,
        area.y,
        (right - left - 1).saturating_sub(2),
        area.height,
    )
}

fn draw_tree(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = rows(app);
    let sel = selected(&rows, &app.nav.key);
    if let Some(row) = rows.get(sel) {
        app.nav.key = row.key.clone();
    }
    let focused = app.focus == Focus::Nav;
    let workspace = clean(&string(&app.session["workspace"], "name"));
    paint(
        f,
        Rect::new(area.x, area.y, area.width, 1),
        [Line::from_spans(vec![
            Span::styled("✻ ", fg(ORANGE)),
            Span::styled("Venticode", fg(if focused { ORANGE } else { WHITE }).bold()),
            Span::styled(format!("  {workspace}"), fg(DIM)),
        ])],
    );
    let search = app.nav.filter.as_ref().map(|q| {
        Line::from_spans(vec![
            Span::styled("⌕ ", fg(BLUE)),
            Span::styled(clean(q), fg(WHITE)),
            Span::styled(if app.nav.typing { "▏" } else { "" }, fg(ORANGE)),
        ])
    });
    paint(f, Rect::new(area.x, area.y + 1, area.width, 1), search);
    let footer = if focused { 2 } else { 1 };
    let height = usize::from(area.height.saturating_sub(2 + footer));
    let body_top = area.y + 2;
    let offset = &mut app.nav.offset;
    if sel < *offset {
        *offset = sel;
    } else if height > 0 && sel >= *offset + height {
        *offset = sel + 1 - height;
    }
    *offset = (*offset).min(rows.len().saturating_sub(height));
    let offset = *offset;
    app.nav.rect = Rect::new(area.x, body_top, area.width, height as u16);
    for (i, row) in rows.iter().enumerate().skip(offset).take(height) {
        let y = body_top + (i - offset) as u16;
        let mut p = Paragraph::new(Text::from_lines([Line::from_spans(label(app, row))]));
        if i == sel {
            p = p.style(Style::new().bg(Color::Ansi256(if focused { SELECTED_BG } else { 235 })));
        }
        p.render(Rect::new(area.x, y, area.width, 1), f);
    }
    let y = area.y + area.height.saturating_sub(footer);
    if focused {
        let hint = hints(app, rows.get(sel));
        paint(
            f,
            Rect::new(area.x, y, area.width, 2),
            Line::raw(hint)
                .wrap(usize::from(area.width).max(1), WrapMode::WordChar)
                .into_iter()
                .map(|l| Line::styled(l.to_plain_text(), fg(DIM))),
        );
    } else {
        let user = clean(&string(&app.session["user"], "name"));
        paint(
            f,
            Rect::new(area.x, y, area.width, 1),
            [Line::styled(format!("◯ {user}"), fg(DIM))],
        );
    }
}

fn field(lines: &mut Vec<Line<'static>>, name: &str, value: impl AsRef<str>, color: u8) {
    let value = clean(value.as_ref());
    if value.is_empty() {
        return;
    }
    lines.push(Line::from_spans(vec![
        Span::styled(format!("{name} "), fg(DIM)),
        Span::styled(value, fg(color)),
    ]));
}

fn heading(lines: &mut Vec<Line<'static>>, text: &str) {
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::styled(clean(text), fg(WHITE).bold()));
}

fn on_off(app: &App, value: &Value) -> &'static str {
    if value == true {
        app.msg("sì", "on")
    } else {
        app.msg("no", "off")
    }
}

fn harness_name(app: &App, id: &Value) -> String {
    app.harness_of(id)
        .map(|h| string(h, "name"))
        .unwrap_or_default()
}

/// Details of the highlighted node, or of the open session.
fn draw_inspector(f: &mut Frame, area: Rect, app: &App) {
    let rows = rows(app);
    let row = (app.focus == Focus::Nav)
        .then(|| rows.get(selected(&rows, &app.nav.key)))
        .flatten();
    let mut lines = Vec::new();
    match row.map(|r| (r.kind, &r.item, r)) {
        Some((Kind::Project, p, _)) => {
            heading(&mut lines, &string(p, "name"));
            field(
                &mut lines,
                "harness",
                harness_name(app, &p["harness_id"]),
                WHITE,
            );
            field(
                &mut lines,
                app.msg("cartella", "folder"),
                App::project_label(p),
                BLUE,
            );
            let count = app
                .chats
                .iter()
                .filter(|c| c["project_id"] == p["id"])
                .count();
            field(
                &mut lines,
                app.msg("chat", "chats"),
                count.to_string(),
                WHITE,
            );
        }
        Some((Kind::Chat, c, _)) => {
            heading(&mut lines, &string(c, "title"));
            field(
                &mut lines,
                app.msg("stato", "state"),
                string(c, "state"),
                GOLD,
            );
            field(
                &mut lines,
                app.msg("modello", "model"),
                string(c, "model"),
                WHITE,
            );
            field(&mut lines, "effort", string(c, "effort"), WHITE);
            field(
                &mut lines,
                app.msg("permessi", "mode"),
                string(c, "permission_mode"),
                WHITE,
            );
            field(
                &mut lines,
                app.msg("aggiornata", "updated"),
                string(c, "updated_at"),
                GRAY,
            );
        }
        Some((Kind::Harness, h, _)) => harness_details(app, h, &mut lines),
        Some((Kind::Dir | Kind::File, entry, r)) => {
            heading(&mut lines, &format!("{}/{}", r.root, r.path));
            field(&mut lines, "harness", string(&r.owner, "name"), WHITE);
            if r.kind == Kind::File {
                field(
                    &mut lines,
                    app.msg("dimensione", "size"),
                    format!("{} B", entry["bytes"].as_u64().unwrap_or(0)),
                    WHITE,
                );
            } else if let Some(Ok(list)) = app.nav.dirs.get(&r.key) {
                field(
                    &mut lines,
                    app.msg("elementi", "entries"),
                    list.len().to_string(),
                    WHITE,
                );
                let existing = app.projects.iter().find(|p| {
                    p["harness_id"] == r.owner["id"]
                        && string(p, "root") == r.root
                        && string(p, "path") == r.path
                });
                match existing {
                    Some(p) => field(
                        &mut lines,
                        app.msg("progetto", "project"),
                        string(p, "name"),
                        ORANGE,
                    ),
                    None => field(
                        &mut lines,
                        "n",
                        app.msg("crea un progetto qui", "create a project here"),
                        GRAY,
                    ),
                }
            }
        }
        Some((Kind::Bot, b, _)) => {
            heading(
                &mut lines,
                &format!("{} {}", string(b, "handle"), string(b, "name")),
            );
            let enabled = b["enabled"] == true;
            field(
                &mut lines,
                app.msg("attivo", "enabled"),
                on_off(app, &b["enabled"]),
                if enabled { GREEN } else { RED },
            );
            field(
                &mut lines,
                "runtime",
                screens::runtime_label(app, &string(b, "runtime")),
                WHITE,
            );
            field(
                &mut lines,
                "harness",
                harness_name(app, &b["harness_id"]),
                WHITE,
            );
            let channels: Vec<String> = b["channels"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| format!("{}:{}", string(c, "kind"), string(c, "name")))
                .collect();
            field(
                &mut lines,
                app.msg("canali", "channels"),
                channels.join(", "),
                WHITE,
            );
        }
        Some((Kind::Mention, m, _)) => {
            let state = string(m, "state");
            let (glyph, color) = mention_glyph(&state);
            heading(&mut lines, &format!("{glyph} {}", string(m, "author")));
            field(
                &mut lines,
                app.msg("canale", "channel"),
                format!(
                    "{}:{}",
                    string(&m["channel"], "kind"),
                    string(&m["channel"], "name")
                ),
                WHITE,
            );
            field(&mut lines, app.msg("stato", "state"), &state, color);
            field(
                &mut lines,
                app.msg("ricevuta", "received"),
                string(m, "created_at"),
                GRAY,
            );
            heading(&mut lines, app.msg("Messaggio", "Message"));
            lines.push(Line::styled(clean(&string(m, "text")), fg(WHITE)));
            let reply = string(m, "reply");
            if !reply.is_empty() {
                heading(
                    &mut lines,
                    if state == "review" {
                        app.msg("Bozza di risposta", "Draft reply")
                    } else {
                        app.msg("Risposta", "Reply")
                    },
                );
                lines.push(Line::styled(
                    clean(&reply),
                    fg(if state == "review" { GOLD } else { GRAY }),
                ));
            }
        }
        _ if app.screen == Screen::Browser => {
            heading(
                &mut lines,
                app.msg("Assistente browser", "Browser assistant"),
            );
            lines.extend(screens::history_lines(app, usize::from(area.width)));
            if app.live.history.is_empty() {
                lines.push(Line::styled(
                    app.msg(
                        "Scrivi una richiesta nel prompt in basso.",
                        "Type a request in the prompt below.",
                    ),
                    fg(DIM),
                ));
            }
        }
        _ => session(app, &mut lines),
    }
    paint(
        f,
        area,
        lines
            .iter()
            .flat_map(|l| l.wrap(usize::from(area.width).max(1), WrapMode::WordChar)),
    );
}

fn harness_details(app: &App, h: &Value, lines: &mut Vec<Line<'static>>) {
    heading(lines, &string(h, "name"));
    let online = h["online"] == true;
    field(
        lines,
        app.msg("stato", "state"),
        if online { "online" } else { "offline" },
        if online { GREEN } else { RED },
    );
    field(
        lines,
        app.msg("modalità", "mode"),
        if h["mode"] == "yolo" {
            app.msg("YOLO · nessuna richiesta", "YOLO · no prompts")
        } else {
            app.msg("Consenso · chiede per strumento", "Consent · asks per tool")
        },
        if h["mode"] == "yolo" { RED } else { WHITE },
    );
    let host = &h["host_info"];
    if host.is_object() {
        field(
            lines,
            "host",
            format!(
                "{} {} · venticode-harness {}",
                string(host, "os"),
                string(host, "arch"),
                string(host, "version")
            ),
            WHITE,
        );
    }
    field(
        lines,
        app.msg("proprio", "owned"),
        on_off(app, &h["owned"]),
        WHITE,
    );
    let roots: Vec<&str> = h["roots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    field(lines, app.msg("radici", "roots"), roots.join(", "), BLUE);
    if let Some(servers) = h["mcp_servers"].as_array().filter(|s| !s.is_empty()) {
        heading(lines, app.msg("Server MCP", "MCP servers"));
        for s in servers {
            let (dot, color, text) = match s["status"].as_str() {
                Some("running") => (
                    "● ",
                    GREEN,
                    format!("{} {}", s["tools"], app.msg("strumenti", "tools")),
                ),
                Some("auth_required") => (
                    "● ",
                    RED,
                    app.msg("accesso richiesto", "sign-in required").into(),
                ),
                _ => ("○ ", DIM, app.msg("fermo", "stopped").into()),
            };
            lines.push(Line::from_spans(vec![
                Span::styled(dot, fg(color)),
                Span::styled(clean(&string(s, "name")), fg(WHITE)),
                Span::styled(format!(" {} · {text}", string(s, "transport")), fg(DIM)),
            ]));
        }
    }
}

fn session(app: &App, lines: &mut Vec<Line<'static>>) {
    heading(lines, app.msg("Sessione", "Session"));
    field(
        lines,
        "workspace",
        string(&app.session["workspace"], "name"),
        GRAY,
    );
    field(
        lines,
        app.msg("progetto", "project"),
        string(&app.project, "name"),
        ORANGE,
    );
    if !app.project.is_null() {
        field(
            lines,
            app.msg("cartella", "folder"),
            App::project_label(&app.project),
            BLUE,
        );
    }
    field(lines, "chat", string(&app.chat, "title"), WHITE);
    field(lines, app.msg("modello", "model"), &app.model, WHITE);
    field(lines, "effort", &app.effort, WHITE);
    field(
        lines,
        app.msg("permessi", "mode"),
        app.current_mode(),
        WHITE,
    );
    let state = string(&app.chat, "state");
    field(
        lines,
        app.msg("stato", "state"),
        &state,
        if state == "idle" { GREEN } else { GOLD },
    );
    if let Some(ratio) = app.usage["context_ratio"].as_f64() {
        let pct = (ratio * 100.0).clamp(0.0, 100.0).round() as usize;
        let filled = pct / 5;
        let color = if pct >= 90 {
            RED
        } else if pct >= 75 {
            GOLD
        } else {
            GREEN
        };
        lines.push(Line::from_spans(vec![
            Span::styled(format!("{} ", app.msg("contesto", "context")), fg(DIM)),
            Span::styled("█".repeat(filled), fg(color)),
            Span::styled("░".repeat(20 - filled), fg(DIM)),
            Span::styled(format!(" {pct}%"), fg(color)),
        ]));
    }
    if !app.todos.is_empty() {
        heading(lines, app.msg("Attività", "Todos"));
        for todo in &app.todos {
            let (mark, color) = match todo["status"].as_str() {
                Some("completed") => ("✓ ", GREEN),
                Some("in_progress") => ("▸ ", GOLD),
                _ => ("○ ", GRAY),
            };
            lines.push(Line::from_spans(vec![
                Span::styled(mark, fg(color)),
                Span::styled(clean(&string(todo, "content")), fg(WHITE)),
            ]));
        }
    }
    if !app.changes.is_empty() {
        heading(lines, app.msg("Modifiche", "Changes"));
        for (path, add, del) in &app.changes {
            lines.push(Line::from_spans(vec![
                Span::styled(format!("+{add} "), fg(GREEN)),
                Span::styled(format!("-{del} "), fg(RED)),
                Span::styled(clean(path), fg(WHITE)),
            ]));
        }
    }
    if let Some(h) = app.current_harness() {
        lines.push(Line::default());
        harness_details(app, h, lines);
    }
}

// ----- Actions -------------------------------------------------------------

async fn load_dir(app: &mut App, cx: &Cx, row: &Row) {
    let result = app
        .remote
        .list_dir(cx, &string(&row.owner, "id"), &row.root, &row.path)
        .await
        .map_err(|e| e.to_string());
    app.nav.dirs.insert(row.key.clone(), result);
}

/// Expand or collapse a row, fetching folder listings and mentions lazily.
async fn expand(app: &mut App, cx: &Cx, row: &Row, open: bool) {
    if !row.expandable() {
        return;
    }
    app.nav.set_open(&row.key, open);
    if !open {
        return;
    }
    match row.kind {
        Kind::Dir if !app.nav.dirs.contains_key(&row.key) => load_dir(app, cx, row).await,
        Kind::Bot => {
            let id = string(&row.item, "id");
            if !app.nav.mentions.contains_key(&id) {
                screens::load_mentions(app, cx, &id).await;
            }
        }
        _ => {}
    }
}

async fn activate(app: &mut App, cx: &Cx, row: &Row) -> Result<()> {
    match row.kind {
        Kind::Link => {
            app.show(match row.key.as_str() {
                "l:browser" => Screen::Browser,
                "l:desktop" => Screen::Desktop,
                _ => Screen::Connections,
            });
        }
        Kind::Project => {
            app.select_project(&row.item);
            expand(app, cx, row, true).await;
        }
        Kind::Chat => app.open(cx, &string(&row.item, "id")).await?,
        Kind::Bot => screens::open_bot(app, cx, &string(&row.item, "id")).await,
        Kind::Mention => {
            let id = string(&row.item, "conversation_id");
            anyhow::ensure!(
                !id.is_empty(),
                app.msg(
                    "Questa menzione non ha una conversazione",
                    "This mention has no conversation"
                )
            );
            app.open(cx, &id).await?;
        }
        _ => {
            let open = !app.nav.is_open(&row.key);
            expand(app, cx, row, open).await;
        }
    }
    Ok(())
}

/// `n`: a new conversation in a project, or a new project on a folder.
async fn create(app: &mut App, cx: &Cx, row: &Row) -> Result<()> {
    let project = match row.kind {
        Kind::Project => row.item.clone(),
        Kind::Dir => {
            let existing = app.projects.iter().find(|p| {
                p["harness_id"] == row.owner["id"]
                    && string(p, "root") == row.root
                    && string(p, "path") == row.path
            });
            match existing {
                Some(p) => p.clone(),
                None => {
                    let body =
                        json!({"harness_id": row.owner["id"], "root": row.root, "path": row.path});
                    let p = app
                        .remote
                        .api(cx, crate::platform::Method::Post, "/projects", Some(body))
                        .await?;
                    app.projects.push(p.clone());
                    p
                }
            }
        }
        _ => return Ok(()),
    };
    app.close();
    app.project = project.clone();
    app.select_project(&project);
    Ok(())
}

async fn review(app: &mut App, cx: &Cx, row: &Row, decision: &str) -> Result<()> {
    anyhow::ensure!(
        row.item["state"] == "review",
        app.msg("Nessuna risposta da rivedere", "No reply held for review")
    );
    let bot = string(&row.owner, "id");
    let path = format!("/bots/{bot}/mentions/{}/review", string(&row.item, "id"));
    app.remote
        .api(
            cx,
            crate::platform::Method::Post,
            &path,
            Some(json!({"decision": decision})),
        )
        .await?;
    screens::load_mentions(app, cx, &bot).await;
    Ok(())
}

/// Reload inventories and every expanded listing.
async fn reload(app: &mut App, cx: &Cx) -> Result<()> {
    app.refresh(cx).await?;
    app.nav.dirs.clear();
    app.nav.mentions.clear();
    for row in rows(app) {
        if app.nav.is_open(&row.key) && matches!(row.kind, Kind::Dir | Kind::Bot) {
            expand(app, cx, &row, true).await;
        }
    }
    if let Screen::Bot(id) = app.screen.clone() {
        screens::load_mentions(app, cx, &id).await;
    }
    Ok(())
}

/// Handle a key while the tree has focus.
pub async fn key(app: &mut App, cx: &Cx, k: KeyEvent) -> Result<()> {
    if app.nav.typing {
        let filter = app.nav.filter.get_or_insert_with(String::new);
        match k.code {
            KeyCode::Escape => {
                app.nav.filter = None;
                app.nav.typing = false;
            }
            KeyCode::Enter => app.nav.typing = false,
            KeyCode::Backspace => {
                filter.pop();
            }
            KeyCode::Char(c) if !k.modifiers.contains(Modifiers::CTRL) => filter.push(c),
            _ => {}
        }
        return Ok(());
    }
    if k.code == KeyCode::Escape && app.nav.filter.is_some() {
        app.nav.filter = None;
        return Ok(());
    }
    let rows = rows(app);
    let sel = selected(&rows, &app.nav.key);
    let Some(row) = rows.get(sel) else {
        return Ok(());
    };
    let goto = |i: usize, nav: &mut Nav| {
        if let Some(r) = rows.get(i) {
            nav.key = r.key.clone();
        }
    };
    let page = usize::from(app.nav.rect.height.max(1));
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => goto(sel.saturating_sub(1), &mut app.nav),
        KeyCode::Down | KeyCode::Char('j') => goto(sel + 1, &mut app.nav),
        KeyCode::PageUp => goto(sel.saturating_sub(page), &mut app.nav),
        KeyCode::PageDown => goto((sel + page).min(rows.len() - 1), &mut app.nav),
        KeyCode::Home => goto(0, &mut app.nav),
        KeyCode::End => goto(rows.len() - 1, &mut app.nav),
        KeyCode::Right | KeyCode::Char('l') if row.expandable() => {
            if app.nav.is_open(&row.key) {
                goto(sel + 1, &mut app.nav);
            } else {
                expand(app, cx, row, true).await;
            }
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if row.expandable() && app.nav.is_open(&row.key) {
                expand(app, cx, row, false).await;
            } else if let Some(up) = rows[..sel].iter().rposition(|r| r.depth < row.depth) {
                goto(up, &mut app.nav);
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') => activate(app, cx, row).await?,
        KeyCode::Char('/') => {
            app.nav.filter = Some(String::new());
            app.nav.typing = true;
        }
        KeyCode::Char('n') => create(app, cx, row).await?,
        KeyCode::Char('r') => reload(app, cx).await?,
        KeyCode::Char('d') if row.kind == Kind::Project => {
            app.confirm_delete_project(&string(&row.item, "id"))
        }
        KeyCode::Char('d') if row.kind == Kind::Chat => {
            app.confirm_delete_chat(&string(&row.item, "id"))
        }
        KeyCode::Char('o') if row.kind == Kind::Harness || row.key == "s:harnesses" => {
            let portal = string(&app.session, "portal_url");
            if !portal.is_empty() && app.session["standalone"] != true {
                crate::platform::open_url(&format!(
                    "{}/app/harnesses",
                    portal.trim_end_matches('/')
                ));
            }
        }
        KeyCode::Char('a') if row.kind == Kind::Harness => {
            let roots: Vec<(String, String)> = row.item["roots"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|r| (r.to_owned(), r.to_owned()))
                .collect();
            anyhow::ensure!(
                !roots.is_empty(),
                app.msg("Nessuna cartella autorizzata", "No granted folder")
            );
            let first = roots[0].1.clone();
            app.form = Some(Form::new(
                format!(
                    "{} {}",
                    app.msg("Nuovo progetto su", "New project on"),
                    string(&row.item, "name")
                ),
                vec![
                    Field::new(
                        "root",
                        app.msg("Cartella autorizzata", "Granted folder"),
                        &first,
                        FieldKind::Choice(roots),
                    ),
                    Field::new(
                        "path",
                        app.msg(
                            "Sottocartella (relativa, . per la radice)",
                            "Subfolder (relative, . for the root)",
                        ),
                        ".",
                        FieldKind::Text,
                    ),
                    Field::new(
                        "name",
                        app.msg("Nome (facoltativo)", "Name (optional)"),
                        "",
                        FieldKind::Text,
                    ),
                ],
                Act::AddProject(row.item.clone()),
            ));
        }
        KeyCode::Char('e') if row.kind == Kind::Bot => {
            let enabled = row.item["enabled"] != true;
            screens::patch_bot(
                app,
                cx,
                &string(&row.item, "id"),
                json!({"enabled": enabled}),
            )
            .await?;
        }
        KeyCode::Char('a') if row.kind == Kind::Mention => review(app, cx, row, "approve").await?,
        KeyCode::Char('x') if row.kind == Kind::Mention => review(app, cx, row, "discard").await?,
        _ => {}
    }
    Ok(())
}

/// Mouse click inside the tree: highlight the row, activate on a second click.
pub async fn click(app: &mut App, cx: &Cx, x: u16, y: u16) -> Result<bool> {
    let hit = |r: &Rect| r.width > 0 && x == r.x && y >= r.y && y < r.y + r.height;
    if let Some(i) = app.nav.dividers.iter().position(hit) {
        toggle(app, i == 1);
        return Ok(true);
    }
    if !app.nav.contains(x, y) {
        return Ok(false);
    }
    let rows = rows(app);
    let index = app.nav.offset + usize::from(y - app.nav.rect.y);
    let Some(target) = rows.get(index) else {
        return Ok(true);
    };
    let again = app.focus == Focus::Nav && app.nav.key == target.key;
    app.focus = Focus::Nav;
    app.nav.key = target.key.clone();
    if again {
        activate(app, cx, target).await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_collapse_independently() {
        let mut app = crate::app::tests::test_app();
        toggle(&mut app, true);
        assert!(app.nav.inspector_hidden && !app.nav.hidden);
        // A tree shown only because it has focus collapses on the first toggle.
        app.nav.hidden = true;
        app.focus = Focus::Nav;
        toggle(&mut app, false);
        assert!(app.nav.hidden && app.focus == Focus::Editor);
        toggle(&mut app, false);
        assert!(!app.nav.hidden);
    }

    #[test]
    fn expansion_defaults() {
        let mut nav = Nav::default();
        assert!(nav.is_open("s:projects"));
        assert!(!nav.is_open("p:1"));
        nav.set_open("s:projects", false);
        nav.set_open("p:1", true);
        nav.set_open("p:1", true);
        assert!(!nav.is_open("s:projects"));
        assert!(nav.is_open("p:1"));
        nav.set_open("p:1", false);
        assert_eq!(nav.toggled.len(), 1);
    }

    #[test]
    fn tree_lists_projects_harness_folders_and_bot_mentions() {
        let mut app = crate::app::tests::test_app();
        app.projects =
            vec![json!({"id":"p1","name":"api","harness_id":"h1","root":"code","path":"."})];
        app.chats = vec![
            json!({"id":"c1","project_id":"p1","title":"Fix","state":"idle"}),
            json!({"id":"c2","project_id":null,"title":"Bot chat","state":"idle"}),
        ];
        app.harnesses = vec![json!({"id":"h1","name":"laptop","online":true,"roots":["code"]})];
        app.bots = vec![json!({"id":"b1","name":"Bot","handle":"@ventic","enabled":true})];
        app.nav.set_open("p:p1", true);
        app.nav.set_open("h:h1", true);
        let root = dir_key("h1", "code", ".");
        app.nav.set_open(&root, true);
        app.nav.dirs.insert(
            root,
            Ok(vec![
                json!({"name":"README.md","directory":false,"bytes":3}),
                json!({"name":"src","directory":true,"bytes":0}),
            ]),
        );
        app.nav.set_open("b:b1", true);
        app.nav.set_mentions(
            "b1",
            Ok(vec![
                json!({"id":"m1","author":"ann","text":"hi","state":"review"}),
            ]),
        );
        let tree = rows(&app);
        let kinds: Vec<Kind> = tree.iter().map(|r| r.kind).collect();
        use Kind::*;
        assert_eq!(
            kinds,
            [
                Section, Project, Chat, Section, Chat, Section, Harness, Dir, Dir, File, Section,
                Bot, Mention
            ]
        );
        assert_eq!(tree[8].path, "src");
        assert_eq!(tree[9].path, "README.md");
        assert_eq!(tree[12].owner["id"], "b1");
        // Searching keeps only matching conversations and their projects.
        app.nav.filter = Some("bot".into());
        let kinds: Vec<Kind> = rows(&app).iter().map(|r| r.kind).collect();
        assert_eq!(kinds[..3], [Section, Section, Chat]);
    }
}
