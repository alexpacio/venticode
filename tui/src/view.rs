//! Conversation view in the look of Claude Code: welcome box, `●` blocks with
//! `⎿` results, a rounded prompt box, numbered permission and choice prompts,
//! the spinner line and the mode footer.

use crate::app::{App, Focus, Message, Role, STARS, Screen, clean, slash_suggestions, string};
use crate::{editor, platform, screens};
use ftui::{
    Frame, Widget,
    core::geometry::Rect,
    style::{Color, Style},
    text::{Line, Span, Text, WrapMode, display_width},
    widgets::paragraph::Paragraph,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const MAX_MARKDOWN_CACHE_ENTRIES: usize = 256;

// Claude Code terminal palette (256-colour indexes): warm brand orange,
// stacked grays, white body text and a light-blue accent.
pub const ORANGE: u8 = 174; // brand accent: logo, prompt, selection
pub const WHITE: u8 = 231; // primary body text
pub const GRAY: u8 = 246; // secondary text
pub const DIM: u8 = 244; // rules, placeholders, footer
pub const BLUE: u8 = 153; // inline code, paths, plan mode
pub const GOLD: u8 = 220; // working / attention
pub const GREEN: u8 = 114; // success, additions
pub const RED: u8 = 203; // errors, deletions
pub const PURPLE: u8 = 183; // accept-edits mode

pub fn fg(n: u8) -> Style {
    Style::new().fg(Color::Ansi256(n))
}

pub fn paint<'a>(frame: &mut Frame, area: Rect, lines: impl IntoIterator<Item = Line<'a>>) {
    Paragraph::new(Text::from_lines(lines)).render(area, frame);
}

/// Draw a rounded box with an optional title and return its inner area.
pub fn bordered(f: &mut Frame, area: Rect, title: &str, color: u8) -> Rect {
    if area.width < 4 || area.height < 2 {
        return Rect::default();
    }
    let inner = usize::from(area.width - 2);
    let title = clean(title);
    let top = if title.is_empty() {
        format!("╭{}╮", "─".repeat(inner))
    } else {
        let label: String = format!(" {title} ")
            .chars()
            .take(inner.saturating_sub(2))
            .collect();
        let rest = inner.saturating_sub(1 + display_width(&label));
        format!("╭─{label}{}╮", "─".repeat(rest))
    };
    let mut lines = vec![Line::styled(top, fg(color))];
    for _ in 0..area.height.saturating_sub(2) {
        lines.push(Line::from_spans(vec![
            Span::styled("│", fg(color)),
            Span::raw(" ".repeat(inner)),
            Span::styled("│", fg(color)),
        ]));
    }
    lines.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), fg(color)));
    paint(f, area, lines);
    Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height - 2,
    )
}

struct CachedMarkdown {
    width: usize,
    hash: u64,
    lines: Vec<Line<'static>>,
}

thread_local! {
    static MARKDOWN_CACHE: RefCell<HashMap<usize, CachedMarkdown>> = RefCell::new(HashMap::new());
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn cached_markdown(idx: usize, text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let hash = hash_str(text);
    MARKDOWN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(entry) = cache.get(&idx)
            && entry.width == width
            && entry.hash == hash
        {
            return entry.lines.clone();
        }
        let lines: Vec<Line<'static>> = ftui_extras::markdown::render_markdown(text)
            .lines()
            .iter()
            .flat_map(|line| line.wrap(width, WrapMode::WordChar))
            .collect();
        if cache.len() >= MAX_MARKDOWN_CACHE_ENTRIES && !cache.contains_key(&idx) {
            cache.clear();
        }
        cache.insert(
            idx,
            CachedMarkdown {
                width,
                hash,
                lines: lines.clone(),
            },
        );
        lines
    })
}

pub fn clear_markdown_cache() {
    MARKDOWN_CACHE.with(|cache| cache.borrow_mut().clear());
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    Line::raw(text)
        .wrap(width.max(1), WrapMode::WordChar)
        .into_iter()
        .map(|l| l.to_plain_text())
        .collect()
}

/// Lines with a first-line marker and a hanging indent.
fn hanging(text: &str, width: usize, marker: (&str, Style), style: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for content in text.split('\n') {
        for row in wrap(content, width.saturating_sub(2)) {
            let prefix = if out.is_empty() {
                marker.0.to_owned()
            } else {
                "  ".to_owned()
            };
            out.push(Line::from_spans(vec![
                Span::styled(prefix, marker.1),
                Span::styled(row, style),
            ]));
        }
    }
    out
}

fn expand_hint(app: &App, hidden: usize) -> Line<'static> {
    Line::styled(
        format!(
            "     … +{hidden} {}",
            app.msg("righe (ctrl+o per espandere)", "lines (ctrl+o to expand)")
        ),
        fg(DIM),
    )
}

fn tool_lines(app: &App, msg: &Message, width: usize) -> Vec<Line<'static>> {
    let (bullet, color) = match msg.status.as_str() {
        "ok" => ("● ", GREEN),
        "waiting" => ("● ", GOLD),
        "error" | "denied" | "cancelled" => ("● ", RED),
        _ => (
            "● ",
            if app.spinner.is_multiple_of(2) {
                GRAY
            } else {
                DIM
            },
        ),
    };
    let (name, args) = msg
        .head
        .split_once('(')
        .map_or((msg.head.as_str(), ""), |(n, a)| (n, a));
    let mut head = vec![
        Span::styled(bullet, fg(color)),
        Span::styled(name.to_owned(), fg(WHITE).bold()),
    ];
    if !args.is_empty() {
        let args: String = format!("({args}")
            .chars()
            .take(width.saturating_sub(display_width(name) + 2))
            .collect();
        head.push(Span::styled(args, fg(GRAY)));
    }
    let mut out = vec![Line::from_spans(head)];
    let body = width.saturating_sub(5).max(1);
    let mut first = true;
    let mut push = |out: &mut Vec<Line<'static>>, text: String, style: Style| {
        let marker = if first { "  ⎿  " } else { "     " };
        first = false;
        out.push(Line::from_spans(vec![
            Span::styled(marker, fg(DIM)),
            Span::styled(text, style),
        ]));
    };
    let status_note = match msg.status.as_str() {
        "waiting" => Some((
            app.msg("In attesa di approvazione…", "Waiting for approval…"),
            GOLD,
        )),
        "denied" => Some((app.msg("Negato", "Denied"), RED)),
        "cancelled" => Some((app.msg("Interrotto", "Interrupted"), RED)),
        _ => None,
    };
    if !msg.note.is_empty() {
        let color = if msg.status == "error" { RED } else { GRAY };
        for row in wrap(&msg.note, body) {
            push(&mut out, row, fg(color));
        }
    } else if let Some((text, color)) = status_note {
        push(&mut out, text.into(), fg(color));
    }
    let output: Vec<String> = msg
        .text
        .trim_end_matches('\n')
        .split('\n')
        .filter(|_| !msg.text.is_empty())
        .flat_map(|l| {
            editor::rows(l, body)
                .into_iter()
                .map(|r| r.text.to_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    let limit = if app.compact_tools { 3 } else { usize::MAX };
    for row in output.iter().take(limit) {
        push(&mut out, row.clone(), fg(GRAY));
    }
    if output.len() > limit {
        out.push(expand_hint(app, output.len() - limit));
    }
    if !msg.diff.is_empty() {
        let rows: Vec<&str> = msg.diff.split('\n').collect();
        let limit = if app.compact_tools { 16 } else { usize::MAX };
        for row in rows.iter().take(limit) {
            let style = if row.starts_with("@@") {
                fg(BLUE)
            } else if row.starts_with('+') {
                Style::new()
                    .fg(Color::Ansi256(WHITE))
                    .bg(Color::Rgb(ftui::style::Rgb::new(26, 64, 34)))
            } else if row.starts_with('-') {
                Style::new()
                    .fg(Color::Ansi256(WHITE))
                    .bg(Color::Rgb(ftui::style::Rgb::new(84, 32, 36)))
            } else {
                fg(GRAY)
            };
            let clipped: String = row.chars().take(body).collect();
            push(&mut out, clipped, style);
        }
        if rows.len() > limit {
            out.push(expand_hint(app, rows.len() - limit));
        }
    }
    out
}

fn transcript(app: &App, idx: usize, msg: &Message, width: usize) -> Vec<Line<'static>> {
    match msg.role {
        Role::Assistant => cached_markdown(idx, &msg.text, width.saturating_sub(2))
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let mut spans = vec![if i == 0 {
                    Span::styled("● ", fg(WHITE))
                } else {
                    Span::raw("  ")
                }];
                spans.extend(line.spans().iter().cloned());
                Line::from_spans(spans)
            })
            .collect(),
        Role::User => hanging(&msg.text, width, ("> ", fg(GRAY)), fg(GRAY)),
        Role::Thinking => {
            let mut out = vec![Line::styled(
                format!("✻ {}", app.msg("Ragionamento…", "Thinking…")),
                fg(GRAY).italic(),
            )];
            let rows: Vec<String> = msg
                .text
                .split('\n')
                .flat_map(|l| wrap(l, width.saturating_sub(2)))
                .collect();
            let limit = if app.compact_tools { 2 } else { usize::MAX };
            for row in rows.iter().take(limit) {
                out.push(Line::styled(format!("  {row}"), fg(DIM).italic()));
            }
            if rows.len() > limit {
                out.push(expand_hint(app, rows.len() - limit));
            }
            out
        }
        Role::Tool => tool_lines(app, msg, width),
        Role::Error => hanging(&msg.text, width, ("● ", fg(RED)), fg(RED)),
        Role::Notice => hanging(&msg.text, width, ("  ", fg(DIM)), fg(GRAY)),
        Role::Todo => {
            let mut out = vec![Line::from_spans(vec![
                Span::styled("● ", fg(GREEN)),
                Span::styled(
                    app.msg("Aggiorna attività", "Update Todos"),
                    fg(WHITE).bold(),
                ),
            ])];
            for (i, row) in msg.text.split('\n').enumerate() {
                let (status, content) = row.split_once('\t').unwrap_or(("pending", row));
                let (mark, style) = match status {
                    "completed" => ("☒ ", fg(DIM).strikethrough()),
                    "in_progress" => ("☐ ", fg(WHITE).bold()),
                    _ => ("☐ ", fg(GRAY)),
                };
                out.push(Line::from_spans(vec![
                    Span::styled(if i == 0 { "  ⎿  " } else { "     " }, fg(DIM)),
                    Span::styled(mark, style),
                    Span::styled(content.to_owned(), style),
                ]));
            }
            out
        }
    }
}

/// An orange rounded box, Claude Code's welcome-box style.
fn boxed(width: usize, rows: Vec<Vec<Span<'static>>>) -> Vec<Line<'static>> {
    let width = width.clamp(20, 76);
    let inner = width - 4;
    let mut lines = vec![Line::styled(
        format!("╭{}╮", "─".repeat(width - 2)),
        fg(ORANGE),
    )];
    for spans in rows {
        let mut used = 0;
        let mut all = vec![Span::styled("│ ", fg(ORANGE))];
        for span in spans {
            let text: String = span.as_str().chars().take(inner - used).collect();
            used += display_width(&text);
            all.push(Span::styled(text, span.style.unwrap_or_default()));
        }
        all.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        all.push(Span::styled(" │", fg(ORANGE)));
        lines.push(Line::from_spans(all));
    }
    lines.push(Line::styled(
        format!("╰{}╯", "─".repeat(width - 2)),
        fg(ORANGE),
    ));
    lines
}

fn welcome_title(app: &App) -> Vec<Span<'static>> {
    vec![
        Span::styled("✻ ", fg(ORANGE)),
        Span::styled(
            app.msg("Benvenuto in Venticode!", "Welcome to Venticode!"),
            fg(WHITE).bold(),
        ),
    ]
}

/// The sign-in screen: the welcome box with what to type in the prompt.
fn signed_out(app: &App, width: usize) -> Vec<Line<'static>> {
    let hint = if app.token_login {
        app.msg(
            "  Accedi con il token di accesso di questa istanza.",
            "  Sign in with this instance's access token.",
        )
    } else if platform::CAN_QUIT {
        app.msg(
            "  Sessione scaduta: riavvia venticode-tui per accedere.",
            "  Session expired: start venticode-tui again to sign in.",
        )
    } else {
        app.msg("  Premi Invio per accedere.", "  Press Enter to sign in.")
    };
    let mut rows = vec![
        welcome_title(app),
        vec![],
        vec![Span::styled(hint, fg(GRAY))],
    ];
    if app.token_login {
        rows.push(vec![Span::styled(
            app.msg(
                "  È il valore di VENTICODE_LOGIN_TOKEN del servizio.",
                "  It is the service's VENTICODE_LOGIN_TOKEN value.",
            ),
            fg(DIM).italic(),
        )]);
    }
    if platform::dev() && !app.token_login {
        rows.push(vec![Span::styled(
            app.msg(
                "  d · accesso di sviluppo (dati sintetici)",
                "  d · development sign-in (synthetic data)",
            ),
            fg(DIM).italic(),
        )]);
    }
    boxed(width, rows)
}

/// Claude Code's welcome box at the top of every conversation.
fn welcome(app: &App, width: usize) -> Vec<Line<'static>> {
    let place = match (app.current_harness(), app.project.is_null()) {
        (_, true) => app
            .msg(
                "nessun progetto · F2 progetti · F3 harness",
                "no project · F2 projects · F3 harnesses",
            )
            .to_owned(),
        (Some(h), false) => format!(
            "{} · {}",
            string(h, "name"),
            App::project_label(&app.project)
        ),
        (None, false) => App::project_label(&app.project),
    };
    let mut lines = boxed(
        width,
        vec![
            welcome_title(app),
            vec![],
            vec![Span::styled(
                format!(
                    "  {}",
                    app.msg(
                        "/help per i comandi · Tab per i pannelli · ? scorciatoie",
                        "/help for commands · Tab for panes · ? shortcuts"
                    )
                ),
                fg(GRAY).italic(),
            )],
            vec![],
            vec![Span::styled(format!("  cwd: {}", clean(&place)), fg(GRAY))],
        ],
    );
    if app.lines.is_empty() {
        lines.push(Line::default());
        if app.project.is_null() {
            lines.push(Line::styled(
                app.msg(
                    " Scegli un progetto nel pannello a sinistra per iniziare.",
                    " Choose a project in the left pane to start.",
                ),
                fg(GRAY),
            ));
        } else {
            lines.push(Line::styled(
                app.msg(" Suggerimenti per iniziare:", " Tips for getting started:"),
                fg(GRAY),
            ));
            for tip in [
                app.msg(
                    " 1. Chiedi di spiegare, modificare o testare il codice del progetto",
                    " 1. Ask to explain, change or test the project's code",
                ),
                app.msg(
                    " 2. /plan per un piano prima delle modifiche · /goal per un obiettivo",
                    " 2. /plan for a plan before changes · /goal for an objective",
                ),
                app.msg(
                    " 3. Shift+Tab cambia la modalità dei permessi",
                    " 3. Shift+Tab switches the permission mode",
                ),
            ] {
                lines.push(Line::styled(tip, fg(DIM)));
            }
        }
    }
    lines
}

fn draw_transcript(f: &mut Frame, body: Rect, app: &mut App) {
    let width = usize::from(body.width);
    let needed = usize::from(app.scroll) + usize::from(body.height);
    // Traverse from the tail so the redraw cost follows the visible history.
    let mut history: Vec<Line<'static>> = Vec::new();
    let mut complete = true;
    for (idx, msg) in app.lines.iter().enumerate().rev() {
        history.push(Line::default());
        history.extend(transcript(app, idx, msg, width).into_iter().rev());
        if history.len() >= needed && idx > 0 {
            complete = false;
            break;
        }
    }
    if complete {
        history.extend(welcome(app, width).into_iter().rev());
    }
    app.scroll = (usize::from(app.scroll))
        .min(history.len().saturating_sub(usize::from(body.height))) as u16;
    let skip = usize::from(app.scroll);
    history.truncate(skip + usize::from(body.height));
    let visible: Vec<Line<'static>> = history.drain(skip.min(history.len())..).rev().collect();
    // Pin short transcripts to the top, like a terminal scrollback.
    app.body_top = body.y;
    app.body_left = body.x;
    app.body_height = body.height;
    app.body_texts = visible.iter().map(Line::to_plain_text).collect();
    paint(f, body, visible);
    if let Some((a, b)) = app.sel {
        for row in a.min(b)..=a.max(b) {
            if let Some(text) = app.body_texts.get(usize::from(row)) {
                paint(
                    f,
                    Rect::new(body.x, body.y + row, body.width, 1),
                    [Line::styled(
                        text.clone(),
                        Style::new().bg(Color::Ansi256(239)),
                    )],
                );
            }
        }
    }
}

/// Numbered choice list in the Claude Code permission-prompt style.
fn choices(items: &[String], sel: usize, current: Option<usize>) -> Vec<Line<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let on = i == sel;
            let mut spans = vec![
                Span::styled(if on { "❯ " } else { "  " }, fg(ORANGE)),
                Span::styled(
                    format!("{}. {}", i + 1, clean(label)),
                    if on { fg(ORANGE).bold() } else { fg(WHITE) },
                ),
            ];
            if current == Some(i) {
                spans.push(Span::styled(" ✓", fg(GREEN)));
            }
            Line::from_spans(spans)
        })
        .collect()
}

/// The prompt panel above the input: picker, approval, question, plan,
/// slash menu or the interactive-session hint.
fn panel(app: &App, width: usize) -> Option<(String, u8, Vec<Line<'static>>)> {
    let inner = width.saturating_sub(4).max(1);
    if let Some(p) = &app.picker {
        let labels: Vec<String> = p.items.iter().map(|i| i.0.clone()).collect();
        let current = p.items.iter().position(|i| i.1 == p.current);
        let mut lines = choices(&labels, p.sel, current);
        lines.push(Line::default());
        lines.push(Line::styled(
            app.msg("Invio conferma · Esc annulla", "Enter confirm · Esc cancel"),
            fg(DIM),
        ));
        return Some((p.title.clone(), BLUE, lines));
    }
    if let Some(a) = &app.approval {
        let mut lines = vec![Line::styled(clean(&a.head), fg(WHITE).bold())];
        if !a.detail.is_empty() && !a.head.contains(a.detail.as_str()) {
            for row in wrap(&a.detail, inner.saturating_sub(2)).into_iter().take(6) {
                lines.push(Line::styled(format!("  {row}"), fg(GRAY)));
            }
        }
        lines.push(Line::default());
        lines.push(Line::styled(
            app.msg("Vuoi procedere?", "Do you want to proceed?"),
            fg(WHITE),
        ));
        let options = [
            app.msg("Sì", "Yes").to_owned(),
            app.msg(
                "Sì, e non chiedere più in questo progetto",
                "Yes, and don't ask again in this project",
            )
            .to_owned(),
            app.msg(
                "No, e dimmi cosa fare diversamente (esc)",
                "No, and tell me what to do differently (esc)",
            )
            .to_owned(),
        ];
        lines.extend(choices(&options, a.sel, None));
        let harness = app
            .current_harness()
            .map(|h| string(h, "name"))
            .unwrap_or_default();
        return Some((
            format!(
                "{} · {harness}",
                app.msg("Permesso richiesto", "Permission required")
            ),
            GOLD,
            lines,
        ));
    }
    if !app.question.is_empty() && !app.choices.is_empty() && app.input.is_empty() {
        let mut lines = choices(&app.choices, app.question_sel, None);
        lines.push(Line::default());
        lines.push(Line::styled(
            app.msg(
                "Invio sceglie · oppure scrivi la tua risposta",
                "Enter picks · or type your own answer",
            ),
            fg(DIM),
        ));
        return Some((app.msg("Domanda", "Question").into(), BLUE, lines));
    }
    if !app.plan.is_empty() && !app.working() && app.input.is_empty() {
        let options = [
            app.msg("Sì, esegui il piano", "Yes, run the plan")
                .to_owned(),
            app.msg("No, continua a pianificare", "No, keep planning")
                .to_owned(),
        ];
        let mut lines = vec![Line::styled(
            app.msg("Vuoi procedere con il piano?", "Would you like to proceed?"),
            fg(WHITE),
        )];
        lines.extend(choices(&options, app.plan_sel, None));
        return Some((
            app.msg("Piano pronto", "Ready to code?").into(),
            BLUE,
            lines,
        ));
    }
    None
}

fn activity(app: &App) -> Option<Line<'static>> {
    let state = string(&app.chat, "state");
    if app.working() {
        if state == "waiting" {
            return Some(Line::styled(
                format!("◆ {}", app.msg("In attesa di te", "Waiting for you")),
                fg(GOLD),
            ));
        }
        let secs = ((platform::now_ms() - app.run_started) / 1000.0).max(0.0) as u64;
        let elapsed = if app.run_started > 0.0 {
            format!("{secs}s · ")
        } else {
            String::new()
        };
        return Some(Line::from_spans(vec![
            Span::styled(format!("{} ", STARS[app.spinner]), fg(ORANGE)),
            Span::styled(app.msg("Al lavoro… ", "Working… "), fg(ORANGE)),
            Span::styled(
                format!(
                    "({elapsed}{})",
                    app.msg("esc per interrompere", "esc to interrupt")
                ),
                fg(DIM),
            ),
        ]));
    }
    screens::interactive_hint(app).map(|hint| Line::styled(format!("⎿ {hint}"), fg(BLUE)))
}

fn footer(app: &App, width: usize) -> Line<'static> {
    let left: (String, u8) = if app.scroll > 0 {
        (
            format!(
                "↑ {} · {}",
                app.scroll,
                app.msg("Fine per seguire", "End to follow")
            ),
            DIM,
        )
    } else if app.screen == Screen::SignedOut {
        (app.msg("Invio accedi", "Enter signs in").into(), DIM)
    } else if app.screen != Screen::Chat {
        (
            app.msg(
                "Tab cambia pannello · Esc torna al prompt · /help",
                "Tab switches pane · Esc back to prompt · /help",
            )
            .into(),
            DIM,
        )
    } else {
        match app.current_mode().as_str() {
            "auto_edit" => (
                app.msg(
                    "▸▸ accetta modifiche (shift+tab per cambiare)",
                    "▸▸ accept edits on (shift+tab to cycle)",
                )
                .into(),
                PURPLE,
            ),
            "plan" => (
                app.msg(
                    "‖ modalità piano (shift+tab per cambiare)",
                    "‖ plan mode on (shift+tab to cycle)",
                )
                .into(),
                BLUE,
            ),
            _ => (
                app.msg("? per le scorciatoie", "? for shortcuts").into(),
                DIM,
            ),
        }
    };
    let mut right = Vec::new();
    if !app.model.is_empty() {
        right.push(clean(&app.model));
    }
    if !app.effort.is_empty() && app.effort != "none" {
        right.push(clean(&app.effort));
    }
    if let Some(ratio) = app.usage["context_ratio"].as_f64() {
        right.push(format!("{}% ctx", (ratio * 100.0).round()));
    }
    // Signed out there is no model or session to describe yet.
    let right = if app.screen == Screen::SignedOut {
        String::new()
    } else {
        right.join(" · ")
    };
    let pad = width.saturating_sub(display_width(&left.0) + display_width(&right) + 2);
    Line::from_spans(vec![
        Span::styled(format!("  {}", left.0), fg(left.1)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, fg(DIM)),
    ])
}

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let width = usize::from(area.width);
    let inner = width.saturating_sub(4).max(1);
    let rows = editor::rows(&app.input, inner);
    let hide_input = app.approval.is_some() && app.input.is_empty() && app.focus == Focus::Editor;
    let editor_height = if hide_input {
        0
    } else {
        rows.len().clamp(1, 6) as u16 + 2
    };
    let banner = u16::from(!app.status.is_empty());
    let suggestions = slash_suggestions(&app.input, app.hide_menu);
    let panel = panel(app, width);
    let panel_height = match &panel {
        Some((_, _, lines)) => lines.len() as u16 + 2,
        None => suggestions.len().min(8) as u16,
    };
    let fixed = banner + 1 + editor_height + 1;
    let panel_height = panel_height.min(area.height.saturating_sub(fixed + 3));
    let body_height = area.height.saturating_sub(fixed + panel_height);
    let mut y = area.y;
    let mut next = |h: u16| {
        let r = Rect::new(area.x, y, area.width, h);
        y += h;
        r
    };
    let body = next(body_height);
    let banner_rect = next(banner);
    let activity_rect = next(1);
    let panel_rect = next(panel_height);
    let input = next(editor_height);
    let footer_rect = next(1);

    if app.screen == Screen::SignedOut {
        paint(f, body, signed_out(app, usize::from(body.width)));
    } else if app.screen == Screen::Chat {
        draw_transcript(f, body, app);
    } else {
        screens::draw_body(f, body, app);
    }
    if banner > 0 {
        paint(
            f,
            banner_rect,
            [Line::styled(format!("  {}", clean(&app.status)), fg(RED))],
        );
    }
    if let Some(line) = activity(app) {
        paint(f, activity_rect, [line]);
    }
    match panel {
        Some((title, color, lines)) => {
            let inner = bordered(f, panel_rect, &title, color);
            paint(f, inner, lines);
        }
        None => {
            let sel = app.selection.min(suggestions.len().saturating_sub(1));
            let height = usize::from(panel_rect.height);
            let start = (sel + 1).saturating_sub(height);
            for (i, cmd) in suggestions.iter().enumerate().skip(start).take(height) {
                let on = i == sel;
                let desc = if app.en { cmd.2 } else { cmd.1 };
                let line = Line::from_spans(vec![
                    Span::styled(if on { "  ❯ " } else { "    " }, fg(ORANGE)),
                    Span::styled(
                        format!("{:<18}", cmd.0),
                        if on { fg(BLUE).bold() } else { fg(GRAY) },
                    ),
                    Span::styled(desc, if on { fg(WHITE) } else { fg(DIM) }),
                ]);
                paint(
                    f,
                    Rect::new(
                        panel_rect.x,
                        panel_rect.y + (i - start) as u16,
                        panel_rect.width,
                        1,
                    ),
                    [line],
                );
            }
        }
    }
    if editor_height > 0 {
        let focused = app.focus == Focus::Editor && app.form.is_none() && app.picker.is_none();
        let box_inner = bordered(f, input, "", if focused { GRAY } else { DIM });
        let text_width = usize::from(box_inner.width.saturating_sub(2)).max(1);
        let rows = editor::rows(&app.input, text_width);
        let (cursor_row, col) = editor::position(&rows, app.caret);
        let first = (cursor_row + 1).saturating_sub(usize::from(box_inner.height));
        let placeholder = match app.screen {
            Screen::SignedOut if app.token_login => app.msg(
                "Incolla il token di accesso e premi Invio",
                "Paste the access token and press Enter",
            ),
            Screen::SignedOut => app.msg("Premi Invio per accedere", "Press Enter to sign in"),
            Screen::Browser => app.msg(
                "Chiedi all'assistente browser…",
                "Ask the browser assistant…",
            ),
            Screen::Chat if !app.question.is_empty() => {
                app.msg("Rispondi alla domanda…", "Answer the question…")
            }
            Screen::Chat if app.interactive().is_some() => app.msg(
                "Riga per la sessione interattiva…",
                "Line for the interactive session…",
            ),
            Screen::Chat => app.msg(
                "Scrivi un messaggio, /plan o /goal…",
                "Type a message, /plan or /goal…",
            ),
            _ => app.msg("Scrivi un comando /…", "Type a /command…"),
        };
        let visible = rows
            .iter()
            .enumerate()
            .skip(first)
            .take(usize::from(box_inner.height))
            .map(|(i, row)| {
                let prompt = if i == 0 { "> " } else { "  " };
                let value = if app.input.is_empty() {
                    Span::styled(placeholder, fg(DIM))
                } else if app.screen == Screen::SignedOut {
                    // The access token never shows on screen.
                    Span::styled("•".repeat(row.text.chars().count()), fg(WHITE))
                } else {
                    Span::styled(row.text.to_owned(), fg(WHITE))
                };
                Line::from_spans(vec![
                    Span::styled(prompt, fg(if focused { WHITE } else { DIM })),
                    value,
                ])
            })
            .collect::<Vec<_>>();
        paint(f, box_inner, visible);
        if focused && box_inner.width > 2 {
            f.set_cursor(Some((
                box_inner.x + (2 + col).min(usize::from(box_inner.width) - 1) as u16,
                box_inner.y + (cursor_row - first) as u16,
            )));
        }
    }
    paint(f, footer_rect, [footer(app, width)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_app;

    #[test]
    fn assistant_markdown_preserves_visible_text_and_emphasis() {
        clear_markdown_cache();
        let app = test_app();
        let message = Message::new(Role::Assistant, "Use **Venticode** locally.");
        let lines = transcript(&app, 0, &message, 80);
        let line = lines
            .iter()
            .find(|l| l.to_plain_text().contains("Venticode"))
            .unwrap();
        assert_eq!(line.to_plain_text(), "● Use Venticode locally.");
        assert!(
            line.spans()
                .iter()
                .any(|s| s.as_str() == "Venticode" && s.style.is_some())
        );
        let updated = Message::new(Role::Assistant, "Different **text**.");
        assert!(
            transcript(&app, 0, &updated, 80)
                .iter()
                .any(|l| l.to_plain_text().contains("Different text."))
        );
    }

    #[test]
    fn tools_render_as_claude_code_blocks_and_collapse_output() {
        let mut app = test_app();
        let mut tool = Message::new(Role::Tool, "a\nb\nc\nd\ne");
        tool.head = "Bash(cargo test)".into();
        tool.status = "ok".into();
        tool.note = "exit 0".into();
        let render = |app: &App| {
            transcript(app, 0, &tool, 40)
                .iter()
                .map(Line::to_plain_text)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            render(&app),
            [
                "● Bash(cargo test)",
                "  ⎿  exit 0",
                "     a",
                "     b",
                "     c",
                "     … +2 lines (ctrl+o to expand)"
            ]
        );
        app.compact_tools = false;
        assert_eq!(render(&app).len(), 7);
    }
}
