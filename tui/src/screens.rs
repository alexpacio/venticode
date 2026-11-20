//! Center-pane screens besides the conversation: bot settings and mentions,
//! bot connections, the remote browser and desktop, sign-in, and the modal
//! forms they open.

use crate::app::{
    Act, App, Field, FieldKind, Focus, Form, Pick, Screen, ScreenFrame, clean, string,
};
use crate::platform::{self, Cx, Method, SocketEvent};
use crate::view::{BLUE, DIM, GOLD, GRAY, GREEN, ORANGE, RED, WHITE, bordered, fg, paint};
use anyhow::{Result, bail, ensure};
use ftui::Frame;
use ftui::core::event::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use ftui::core::geometry::Rect;
use ftui::style::{Color, Style};
use ftui::text::{Line, Span, WrapMode, display_width};
use serde_json::{Value, json};
use std::sync::Arc;

const SELECTED_BG: u8 = 237;

pub fn runtime_label(app: &App, runtime: &str) -> String {
    match runtime {
        "computer" => app.msg("Computer Ubuntu", "Ubuntu computer").into(),
        "kitesurf" => "Kitesurf".into(),
        _ => app.msg("Progetto", "Project").into(),
    }
}

fn provider(kind: &str) -> &str {
    match kind {
        "slack" => "Slack",
        "telegram" => "Telegram",
        "gitlab" => "GitLab",
        other => other,
    }
}

fn policy_label(app: &App, value: &str) -> String {
    match value {
        "auto" => app.msg("Automatico", "Automatic"),
        "review" => app.msg("Con revisione", "With review"),
        "off" => app.msg("Disattivato", "Off"),
        other => other,
    }
    .into()
}

pub fn mention_glyph(state: &str) -> (&'static str, u8) {
    match state {
        "review" => ("◆", GOLD),
        "replied" => ("✓", GREEN),
        "answering" | "sending" => ("✻", ORANGE),
        "failed" | "uncertain" => ("✗", RED),
        _ => ("·", DIM),
    }
}

/// A row of a screen; rows with an empty key are headings.
struct Item {
    key: String,
    line: Line<'static>,
}

impl Item {
    fn head(line: Line<'static>) -> Self {
        Item {
            key: String::new(),
            line,
        }
    }
    fn row(key: impl Into<String>, spans: Vec<Span<'static>>) -> Self {
        Item {
            key: key.into(),
            line: Line::from_spans(spans),
        }
    }
}

fn label_value(label: &str, value: String, color: u8) -> Vec<Span<'static>> {
    let pad = 22usize.saturating_sub(display_width(label));
    vec![
        Span::styled(format!("  {label}{}", " ".repeat(pad)), fg(GRAY)),
        Span::styled(value, fg(color)),
    ]
}

fn check(on: bool) -> &'static str {
    if on { "[✓] " } else { "[ ] " }
}

// ----- Bot screen ----------------------------------------------------------

fn bot<'a>(app: &'a App, id: &str) -> Option<&'a Value> {
    app.bots.iter().find(|b| b["id"] == id)
}

fn bot_items(app: &App, id: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let Some(b) = bot(app, id) else {
        out.push(Item::head(Line::styled(
            app.msg("Bot non trovato", "Bot not found"),
            fg(RED),
        )));
        return out;
    };
    let enabled = b["enabled"] == true;
    out.push(Item::head(Line::from_spans(vec![
        Span::styled("● ", fg(if enabled { GREEN } else { DIM })),
        Span::styled(clean(&string(b, "handle")), fg(WHITE).bold()),
        Span::styled(format!("  {}", clean(&string(b, "name"))), fg(GRAY)),
        Span::styled(
            format!(
                "  {}",
                if enabled {
                    app.msg("Attivo", "Active")
                } else {
                    app.msg("In pausa", "Paused")
                }
            ),
            fg(if enabled { GREEN } else { GOLD }),
        ),
    ])));
    let runtime = string(b, "runtime");
    let place = match runtime.as_str() {
        "computer" | "kitesurf" => runtime_label(app, &runtime),
        _ => {
            let harness = app.harness_of(&b["harness_id"]).map(|h| string(h, "name"));
            match (harness, b["root"].as_str()) {
                (Some(h), Some(root)) => {
                    format!("{} {h} ({root})", app.msg("strumenti su", "tools on"))
                }
                _ => String::new(),
            }
        }
    };
    if !place.is_empty() {
        out.push(Item::head(Line::styled(
            format!("  {}", clean(&place)),
            fg(DIM),
        )));
    }
    out.push(Item::head(Line::default()));
    if app.admin() {
        out.push(Item::row(
            "enabled",
            label_value(
                app.msg("Stato", "State"),
                if enabled {
                    app.msg("● Attivo", "● Active")
                } else {
                    app.msg("○ In pausa", "○ Paused")
                }
                .into(),
                if enabled { GREEN } else { GOLD },
            ),
        ));
        if app.runtimes().len() > 1 {
            out.push(Item::row(
                "runtime",
                label_value(
                    app.msg("Ambiente", "Environment"),
                    format!("‹ {} ›", runtime_label(app, &runtime)),
                    WHITE,
                ),
            ));
        }
        let triggers = &b["triggers"];
        out.push(Item::row(
            "mentions",
            label_value(
                app.msg("Attivazione", "Triggers"),
                format!(
                    "{}{} {}",
                    check(triggers["mentions"] == true),
                    app.msg("Menzioni di", "Mentions of"),
                    string(b, "handle")
                ),
                WHITE,
            ),
        ));
        out.push(Item::row(
            "dm",
            label_value(
                "",
                format!(
                    "{}{}",
                    check(triggers["direct_messages"] == true),
                    app.msg("Messaggi diretti", "Direct messages")
                ),
                WHITE,
            ),
        ));
        for (key, it, en) in [
            ("read", "Lettura file", "File reads"),
            ("write", "Scrittura file", "File writes"),
            ("execute", "Comandi", "Commands"),
            ("mcp", "Strumenti MCP", "MCP tools"),
        ] {
            let value = string(&b["tools"], key);
            out.push(Item::row(
                format!("tool:{key}"),
                label_value(
                    if app.en { en } else { it },
                    format!("‹ {} ›", policy_label(app, &value)),
                    match value.as_str() {
                        "auto" => GREEN,
                        "review" => GOLD,
                        _ => DIM,
                    },
                ),
            ));
        }
        let instructions = string(b, "instructions");
        let first = instructions.lines().next().unwrap_or("").to_owned();
        out.push(Item::row(
            "instructions",
            label_value(
                app.msg("Istruzioni", "Instructions"),
                if first.is_empty() {
                    app.msg("(nessuna) · Invio per scrivere", "(none) · Enter to write")
                        .into()
                } else {
                    clean(&first)
                },
                WHITE,
            ),
        ));
        out.push(Item::row(
            "rate",
            label_value(
                app.msg("Risposte massime per ora", "Max replies per hour"),
                b["rate_limit_per_hour"].to_string(),
                WHITE,
            ),
        ));
    }
    let channels: Vec<String> = b["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| format!("{} {}", provider(&string(c, "kind")), string(c, "name")))
        .collect();
    out.push(Item::row(
        "channels",
        label_value(
            app.msg("Canali", "Channels"),
            if channels.is_empty() {
                app.msg(
                    "nessuno · Invio per le connessioni",
                    "none · Enter for connections",
                )
                .into()
            } else {
                clean(&channels.join(", "))
            },
            BLUE,
        ),
    ));
    out.push(Item::head(Line::default()));
    out.push(Item::head(Line::from_spans(vec![
        Span::styled(app.msg("MENZIONI", "MENTIONS"), fg(GRAY).bold()),
        Span::styled(
            format!(
                "  {}",
                app.msg(
                    "Approva la bozza per pubblicarla nel canale.",
                    "Approve a draft to post it to the channel."
                )
            ),
            fg(DIM),
        ),
    ])));
    match app.nav.mentions(id) {
        None => out.push(Item::head(Line::styled(
            format!("  {}", app.msg("Caricamento…", "Loading…")),
            fg(DIM),
        ))),
        Some(Err(e)) => out.push(Item::head(Line::styled(format!("  {}", clean(e)), fg(RED)))),
        Some(Ok(list)) if list.is_empty() => out.push(Item::head(Line::styled(
            format!(
                "  {} {}",
                app.msg("Nessuna menzione di", "No mentions of"),
                string(b, "handle")
            ),
            fg(DIM),
        ))),
        Some(Ok(list)) => {
            for m in list {
                let state = string(m, "state");
                let (glyph, color) = mention_glyph(&state);
                let key = format!("m:{}", string(m, "id"));
                out.push(Item::row(
                    key.clone(),
                    vec![
                        Span::styled(format!("  {glyph} "), fg(color)),
                        Span::styled(clean(&string(m, "author")), fg(WHITE).bold()),
                        Span::styled(
                            format!(
                                " · {} {} · {}",
                                provider(&string(&m["channel"], "kind")),
                                clean(&string(&m["channel"], "name")),
                                state_label(app, &state)
                            ),
                            fg(DIM),
                        ),
                    ],
                ));
                out.push(Item::head(Line::styled(
                    format!("    {}", clean(&string(m, "text")).replace('\n', " ")),
                    fg(WHITE),
                )));
                let reply = string(m, "reply");
                if !reply.is_empty() {
                    let label = if state == "review" {
                        app.msg("Bozza: ", "Draft: ")
                    } else {
                        "⎿ "
                    };
                    out.push(Item::head(Line::from_spans(vec![
                        Span::styled(format!("    {label}"), fg(DIM)),
                        Span::styled(
                            clean(&reply).replace('\n', " "),
                            fg(if state == "review" { GOLD } else { GRAY }),
                        ),
                    ])));
                }
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    out.push(Item::head(Line::styled(
                        format!(
                            "    ⎿ {} · {}",
                            crate::app::tool_head(
                                &string(call, "operation"),
                                &string(call, "target")
                            ),
                            string(call, "status")
                        ),
                        fg(DIM),
                    )));
                }
            }
        }
    }
    out
}

fn state_label(app: &App, state: &str) -> String {
    match state {
        "review" => app.msg("da rivedere", "needs review"),
        "replied" => app.msg("risposto", "replied"),
        "answering" => app.msg("sta rispondendo", "answering"),
        "sending" => app.msg("invio", "sending"),
        "failed" => app.msg("non riuscito", "failed"),
        "uncertain" => app.msg("incerto", "uncertain"),
        "discarded" => app.msg("scartato", "discarded"),
        other => other,
    }
    .into()
}

pub async fn patch_bot(app: &mut App, cx: &Cx, id: &str, mut change: Value) -> Result<()> {
    let current = bot(app, id).cloned().unwrap_or_default();
    change["revision"] = current["revision"].clone();
    let updated = app
        .remote
        .api(cx, Method::Patch, &format!("/bots/{id}"), Some(change))
        .await?;
    if let Some(slot) = app.bots.iter_mut().find(|b| b["id"] == updated["id"]) {
        *slot = updated;
    }
    app.status = app.msg("Salvato", "Saved").into();
    Ok(())
}

pub async fn load_mentions(app: &mut App, cx: &Cx, id: &str) {
    let result = app
        .remote
        .list(cx, &format!("/bots/{id}/mentions"))
        .await
        .map_err(|e| e.to_string());
    app.nav.set_mentions(id, result);
}

pub async fn open_bot(app: &mut App, cx: &Cx, id: &str) {
    app.show(Screen::Bot(id.to_owned()));
    load_mentions(app, cx, id).await;
}

async fn review(app: &mut App, cx: &Cx, bot: &str, mention: &str, decision: &str) -> Result<()> {
    let state = app
        .nav
        .mentions(bot)
        .and_then(|r| r.as_ref().ok())
        .and_then(|l| l.iter().find(|m| m["id"] == mention))
        .map(|m| string(m, "state"))
        .unwrap_or_default();
    ensure!(
        state == "review",
        app.msg("Nessuna risposta da rivedere", "No reply held for review")
    );
    let path = format!("/bots/{bot}/mentions/{mention}/review");
    app.remote
        .api(cx, Method::Post, &path, Some(json!({"decision": decision})))
        .await?;
    load_mentions(app, cx, bot).await;
    Ok(())
}

async fn bot_key(app: &mut App, cx: &Cx, id: &str, key: &str, k: KeyEvent) -> Result<()> {
    let Some(b) = bot(app, id).cloned() else {
        return Ok(());
    };
    let activate = matches!(
        k.code,
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
    );
    if let Some(mention) = key.strip_prefix("m:") {
        match k.code {
            KeyCode::Char('a') => return review(app, cx, id, mention, "approve").await,
            KeyCode::Char('x') => return review(app, cx, id, mention, "discard").await,
            KeyCode::Enter => {
                let conversation = app
                    .nav
                    .mentions(id)
                    .and_then(|r| r.as_ref().ok())
                    .and_then(|l| l.iter().find(|m| m["id"] == mention))
                    .map(|m| string(m, "conversation_id"))
                    .unwrap_or_default();
                ensure!(
                    !conversation.is_empty(),
                    app.msg(
                        "Questa menzione non ha una conversazione",
                        "This mention has no conversation"
                    )
                );
                return app.open(cx, &conversation).await;
            }
            _ => return Ok(()),
        }
    }
    if !activate && !matches!(k.code, KeyCode::Char('e')) {
        return Ok(());
    }
    match key {
        "enabled" => patch_bot(app, cx, id, json!({"enabled": b["enabled"] != true})).await,
        _ if k.code == KeyCode::Char('e') => {
            patch_bot(app, cx, id, json!({"enabled": b["enabled"] != true})).await
        }
        "runtime" => app.open_picker(Pick::Runtime(id.to_owned())),
        "mentions" | "dm" => {
            let mut triggers = b["triggers"].clone();
            let field = if key == "dm" {
                "direct_messages"
            } else {
                "mentions"
            };
            triggers[field] = json!(triggers[field] != true);
            patch_bot(app, cx, id, json!({"triggers": triggers})).await
        }
        tool if tool.starts_with("tool:") => {
            let name = match &tool[5..] {
                "read" => "read",
                "write" => "write",
                "execute" => "execute",
                _ => "mcp",
            };
            if matches!(k.code, KeyCode::Left | KeyCode::Right) {
                let order = ["auto", "review", "off"];
                let current = string(&b["tools"], name);
                let i = order.iter().position(|v| *v == current).unwrap_or(0);
                let next = if k.code == KeyCode::Right {
                    (i + 1) % 3
                } else {
                    (i + 2) % 3
                };
                let mut tools = b["tools"].clone();
                tools[name] = json!(order[next]);
                patch_bot(app, cx, id, json!({"tools": tools})).await
            } else {
                app.open_picker(Pick::Policy(id.to_owned(), name))
            }
        }
        "instructions" => {
            app.form = Some(Form::new(
                app.msg("Istruzioni del bot", "Bot instructions").into(),
                vec![Field::new(
                    "instructions",
                    app.msg(
                        "Istruzioni (Ctrl+J a capo)",
                        "Instructions (Ctrl+J newline)",
                    ),
                    &string(&b, "instructions"),
                    FieldKind::Multiline,
                )],
                Act::Instructions(id.to_owned()),
            ));
            Ok(())
        }
        "rate" => {
            app.form = Some(Form::new(
                app.msg("Risposte massime per ora", "Max replies per hour")
                    .into(),
                vec![Field::new(
                    "rate",
                    "1–1000",
                    &b["rate_limit_per_hour"].to_string(),
                    FieldKind::Text,
                )],
                Act::RateLimit(id.to_owned()),
            ));
            Ok(())
        }
        "channels" => {
            app.show(Screen::Connections);
            Ok(())
        }
        _ => Ok(()),
    }
}

// ----- Connections screen --------------------------------------------------

fn connection_items(app: &App) -> Vec<Item> {
    let mut out = vec![Item::head(Line::styled(
        app.msg("Connessioni bot", "Bot connections"),
        fg(WHITE).bold(),
    ))];
    out.push(Item::head(Line::default()));
    if app.connections.is_empty() {
        out.push(Item::head(Line::styled(
            app.msg(
                "Nessuna connessione configurata.",
                "No connections configured.",
            ),
            fg(DIM),
        )));
    }
    let admin = app.admin();
    for c in &app.connections {
        let id = string(c, "id");
        let status = string(c, "status");
        let (text, color) = match status.as_str() {
            "connected" => (app.msg("● Connesso", "● Connected"), GREEN),
            "pending" => (
                app.msg("◆ In attesa di autorizzazione", "◆ Awaiting authorization"),
                GOLD,
            ),
            _ => (app.msg("○ Disconnesso", "○ Disconnected"), DIM),
        };
        out.push(Item::head(Line::from_spans(vec![
            Span::styled(
                format!(
                    "{} · {}  ",
                    provider(&string(c, "kind")),
                    clean(&string(c, "destination"))
                ),
                fg(WHITE).bold(),
            ),
            Span::styled(text, fg(color)),
        ])));
        if admin && c["kind"] == "slack" {
            out.push(Item::row(
                format!("webhook:{id}"),
                label_value(
                    app.msg("URL eventi Slack", "Slack events URL"),
                    format!(
                        "{} · c {}",
                        clean(&string(c, "webhook_url")),
                        app.msg("copia", "copy")
                    ),
                    BLUE,
                ),
            ));
        }
        if status == "connected" {
            out.push(Item::row(
                format!("open:{id}"),
                vec![Span::styled(
                    format!("  ↗ {}", app.msg("Apri menzioni", "Open mentions")),
                    fg(WHITE),
                )],
            ));
            let linked = string(c, "linked_user_id");
            if linked.is_empty() {
                out.push(Item::row(
                    format!("link:{id}"),
                    vec![Span::styled(
                        format!(
                            "  + {}",
                            app.msg("Collega il mio account", "Link my account")
                        ),
                        fg(WHITE),
                    )],
                ));
            } else {
                out.push(Item::row(
                    format!("unlink:{id}"),
                    vec![
                        Span::styled(
                            format!(
                                "  {} {} · ",
                                app.msg("Account collegato:", "Linked account:"),
                                clean(&linked)
                            ),
                            fg(GRAY),
                        ),
                        Span::styled(app.msg("Scollega", "Unlink"), fg(RED)),
                    ],
                ));
            }
        }
        if admin && status != "disconnected" {
            if c["project_id"].is_string()
                || matches!(c["runtime"].as_str(), Some("kitesurf" | "computer"))
            {
                out.push(Item::row(
                    format!("replace:{id}"),
                    vec![Span::styled(
                        format!(
                            "  ✎ {}",
                            app.msg("Sostituisci credenziali", "Replace credentials")
                        ),
                        fg(WHITE),
                    )],
                ));
            }
            out.push(Item::row(
                format!("disconnect:{id}"),
                vec![Span::styled(
                    format!("  ✗ {}", app.msg("Disconnetti", "Disconnect")),
                    fg(RED),
                )],
            ));
        }
        out.push(Item::head(Line::default()));
    }
    if !app.link_command.is_empty() {
        out.push(Item::row(
            "command",
            vec![
                Span::styled(
                    app.msg(
                        "Invia questo comando dal tuo account: ",
                        "Send this command from your account: ",
                    ),
                    fg(GOLD),
                ),
                Span::styled(clean(&app.link_command), fg(WHITE).bold()),
                Span::styled(format!(" · c {}", app.msg("copia", "copy")), fg(DIM)),
            ],
        ));
    }
    if admin {
        out.push(Item::row(
            "add",
            vec![Span::styled(
                format!("+ {}", app.msg("Aggiungi connessione", "Add connection")),
                fg(ORANGE),
            )],
        ));
    }
    out
}

fn connection_form(app: &App, existing: Option<Value>) -> Form {
    let mut fields = Vec::new();
    let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
        items
            .iter()
            .map(|(l, v)| (l.to_string(), v.to_string()))
            .collect()
    };
    let kind = existing
        .as_ref()
        .map_or("slack".to_owned(), |c| string(c, "kind"));
    if existing.is_none() {
        fields.push(Field::new(
            "kind",
            app.msg("Servizio", "Service"),
            &kind,
            FieldKind::Choice(pairs(&[
                ("Slack", "slack"),
                ("Telegram", "telegram"),
                ("GitLab", "gitlab"),
            ])),
        ));
        fields.push(Field::new(
            "destination",
            app.msg(
                "ID del canale, chat o progetto",
                "Channel, chat or project ID",
            ),
            "",
            FieldKind::Text,
        ));
        let runtimes = app.runtimes();
        if runtimes.len() > 1 {
            fields.push(Field::new(
                "runtime",
                app.msg("Ambiente del bot", "Bot environment"),
                &runtimes[0],
                FieldKind::Choice(
                    runtimes
                        .iter()
                        .map(|r| (runtime_label(app, r), r.clone()))
                        .collect(),
                ),
            ));
        }
        if !app.projects.is_empty() {
            fields.push(Field::new(
                "project",
                app.msg(
                    "Progetto (ambiente Progetto)",
                    "Project (Project environment)",
                ),
                &string(&app.projects[0], "id"),
                FieldKind::Choice(
                    app.projects
                        .iter()
                        .map(|p| {
                            (
                                format!("{} · {}", string(p, "name"), App::project_label(p)),
                                string(p, "id"),
                            )
                        })
                        .collect(),
                ),
            ));
        }
    }
    credential_fields(app, &kind, &mut fields);
    let mut form = Form::new(
        if existing.is_some() {
            app.msg("Sostituisci credenziali", "Replace credentials")
        } else {
            app.msg("Aggiungi connessione", "Add connection")
        }
        .into(),
        fields,
        Act::Connection(existing),
    );
    if kind == "slack" {
        form.body = format!(
            "{} {}/oauth/slack",
            app.msg("URL di reindirizzamento OAuth:", "OAuth redirect URL:"),
            app.remote.workspace
        );
    }
    form
}

fn credential_fields(app: &App, kind: &str, fields: &mut Vec<Field>) {
    fields.retain(|f| {
        !matches!(
            f.key,
            "client_id" | "client_secret" | "signing_secret" | "token"
        )
    });
    if kind == "slack" {
        fields.push(Field::new(
            "client_id",
            app.msg("ID client", "Client ID"),
            "",
            FieldKind::Text,
        ));
        fields.push(Field::new(
            "client_secret",
            app.msg("Segreto client", "Client secret"),
            "",
            FieldKind::Secret,
        ));
        fields.push(Field::new(
            "signing_secret",
            app.msg("Segreto di firma", "Signing secret"),
            "",
            FieldKind::Secret,
        ));
    } else {
        fields.push(Field::new(
            "token",
            app.msg("Token del bot", "Bot token"),
            "",
            FieldKind::Secret,
        ));
    }
}

async fn connections_key(app: &mut App, cx: &Cx, key: &str, k: KeyEvent) -> Result<()> {
    let (action, id) = key.split_once(':').unwrap_or((key, ""));
    let c = app.connections.iter().find(|c| c["id"] == id).cloned();
    if k.code == KeyCode::Char('c') {
        let text = match action {
            "webhook" => c
                .as_ref()
                .map(|c| string(c, "webhook_url"))
                .unwrap_or_default(),
            "command" => app.link_command.clone(),
            _ => String::new(),
        };
        if !text.is_empty() {
            platform::copy(&text);
            app.status = app.msg("Copiato", "Copied").into();
        }
        return Ok(());
    }
    if k.code == KeyCode::Char('r') {
        app.refresh(cx).await?;
        return Ok(());
    }
    if k.code != KeyCode::Enter {
        return Ok(());
    }
    match action {
        "open" => open_bot(app, cx, id).await,
        "link" => {
            let label = format!(
                "{} {}",
                app.msg("Il tuo ID utente su", "Your user ID on"),
                provider(&c.map(|c| string(&c, "kind")).unwrap_or_default())
            );
            app.form = Some(Form::new(
                app.msg("Collega il mio account", "Link my account").into(),
                vec![Field::new("identity", &label, "", FieldKind::Text)],
                Act::Link(id.to_owned()),
            ));
        }
        "unlink" => {
            app.form = Some(Form::confirm(
                app.msg("Scollega account", "Unlink account").into(),
                app.msg(
                    "Scollegare il tuo account da questo bot?",
                    "Unlink your account from this bot?",
                )
                .into(),
                Act::Unlink(id.to_owned()),
            ));
        }
        "replace" => app.form = Some(connection_form(app, c)),
        "add" => app.form = Some(connection_form(app, None)),
        "disconnect" => {
            app.form = Some(Form::confirm(
                app.msg("Disconnetti", "Disconnect").into(),
                app.msg(
                    "Disconnettere il bot e revocare i collegamenti degli utenti?",
                    "Disconnect the bot and revoke user links?",
                )
                .into(),
                Act::Disconnect(id.to_owned()),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Run the screen-specific form actions.
pub async fn act(app: &mut App, cx: &Cx, form: &Form) -> Result<()> {
    match &form.action {
        Act::Instructions(id) => {
            patch_bot(
                app,
                cx,
                id,
                json!({"instructions": form.get("instructions")}),
            )
            .await?;
        }
        Act::RateLimit(id) => {
            let rate: i64 = form.get("rate").trim().parse().unwrap_or(0);
            ensure!((1..=1000).contains(&rate), "1–1000");
            patch_bot(app, cx, id, json!({"rate_limit_per_hour": rate})).await?;
        }
        Act::Link(id) => {
            let identity = form.get("identity").trim();
            ensure!(
                !identity.is_empty(),
                app.msg("ID obbligatorio", "ID required")
            );
            let proof = app
                .remote
                .api(
                    cx,
                    Method::Post,
                    &format!("/connections/{id}/link"),
                    Some(json!({"external_user_id": identity})),
                )
                .await?;
            let minutes = (proof["expires_in"].as_f64().unwrap_or(300.0) / 60.0)
                .round()
                .max(1.0);
            app.link_command = format!(
                "{} ({} {minutes} min)",
                string(&proof, "command"),
                app.msg("entro", "within")
            );
            app.body_key = "command".into();
        }
        Act::Unlink(id) => {
            app.remote
                .api(cx, Method::Delete, &format!("/connections/{id}/link"), None)
                .await?;
            app.refresh(cx).await?;
        }
        Act::Disconnect(id) => {
            app.remote
                .api(cx, Method::Delete, &format!("/connections/{id}"), None)
                .await?;
            app.refresh(cx).await?;
        }
        Act::Connection(existing) => {
            let kind = existing
                .as_ref()
                .map_or_else(|| form.get("kind").to_owned(), |c| string(c, "kind"));
            let runtime = existing.as_ref().map_or_else(
                || {
                    let r = form.get("runtime");
                    if r.is_empty() {
                        app.runtimes().first().cloned().unwrap_or("project".into())
                    } else {
                        r.to_owned()
                    }
                },
                |c| string(c, "runtime"),
            );
            let keys: &[&str] = if kind == "slack" {
                &["client_id", "client_secret", "signing_secret"]
            } else {
                &["token"]
            };
            let mut credentials = json!({});
            for key in keys {
                ensure!(
                    !form.get(key).trim().is_empty(),
                    "{} {key}",
                    app.msg("Campo obbligatorio:", "Required field:")
                );
                credentials[*key] = json!(form.get(key).trim());
            }
            let destination = existing.as_ref().map_or_else(
                || form.get("destination").trim().to_owned(),
                |c| string(c, "destination"),
            );
            ensure!(
                !destination.is_empty(),
                app.msg("Destinazione obbligatoria", "Destination required")
            );
            let project = if let Some(c) = existing {
                c["project_id"].clone()
            } else if runtime == "project" {
                ensure!(
                    !form.get("project").is_empty(),
                    app.msg(
                        "Aggiungi prima un progetto da un harness.",
                        "Add a project from a harness first."
                    )
                );
                json!(form.get("project"))
            } else {
                Value::Null
            };
            let body = json!({"kind": kind, "destination": destination, "runtime": runtime, "project_id": project, "credentials": credentials});
            let (method, path) = match existing {
                Some(c) => (Method::Put, format!("/connections/{}", string(c, "id"))),
                None => (Method::Post, "/connections".to_owned()),
            };
            let result = app.remote.api(cx, method, &path, Some(body)).await?;
            if let Some(url) = result["authorize_url"].as_str() {
                ensure!(
                    url.starts_with("https://slack.com/oauth/v2/authorize"),
                    app.msg(
                        "URL di autorizzazione non valido.",
                        "Invalid authorization URL."
                    )
                );
                platform::navigate(url);
            }
            app.refresh(cx).await?;
        }
        _ => {}
    }
    Ok(())
}

// ----- Remote browser and desktop ------------------------------------------

fn desktop(app: &App) -> bool {
    app.screen == Screen::Desktop
}

pub fn live_start(app: &mut App, cx: &Cx) -> Result<()> {
    if app.live.socket.is_some() {
        return Ok(());
    }
    let path = if desktop(app) {
        "/computer/screen".to_owned()
    } else {
        let thread = if app.live.thread.is_empty() {
            platform::new_id()
        } else {
            app.live.thread.clone()
        };
        app.live.thread = thread.clone();
        format!("/browser-agent/{thread}")
    };
    let id = app.next_socket_id();
    app.live.socket_id = id;
    app.live.error.clear();
    app.live.history.clear();
    app.live.state = "opening";
    app.live.last_activity = platform::now_ms();
    match platform::socket(cx, &app.remote, &path, id, app.tx.clone()) {
        Ok(socket) => app.live.socket = Some(socket),
        Err(e) => {
            app.live.state = "closed";
            bail!(e);
        }
    }
    Ok(())
}

fn send(app: &mut App, packet: Value) -> bool {
    app.live
        .socket
        .as_ref()
        .is_some_and(|s| s.send(packet.to_string()))
}

pub fn live_input(app: &mut App, input: Value) {
    if !app.live.human || app.live.state != "ready" {
        return;
    }
    let tab = if desktop(app) {
        "desktop".to_owned()
    } else {
        app.live.active.clone()
    };
    send(app, json!({"type": "input", "tab": tab, "input": input}));
}

pub fn live_ask(app: &mut App, prompt: &str) -> Result<()> {
    ensure!(
        app.live.state == "ready" && !app.live.human && !app.live.pending,
        app.msg(
            "Avvia Kitesurf e attendi che l'assistente sia libero.",
            "Start Kitesurf and wait until the assistant is idle."
        )
    );
    if send(app, json!({"type": "ask", "prompt": prompt})) {
        app.live.pending = true;
        app.live
            .history
            .push(json!({"role": "user", "content": prompt}));
    }
    Ok(())
}

pub fn live_open(app: &mut App, raw: &str) -> Result<()> {
    let raw = raw.trim();
    ensure!(!raw.is_empty(), "URL");
    let has_scheme = raw.split_once(':').is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    });
    let url = if has_scheme {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    send(app, json!({"type": "open", "url": url}));
    Ok(())
}

pub fn live_closed(app: &mut App, failed: bool) {
    if let Some(socket) = app.live.socket.take() {
        socket.close();
    }
    let was_ready = app.live.state == "ready";
    app.live.state = "closed";
    app.live.tabs.clear();
    app.live.active.clear();
    app.live.frame = None;
    app.live.human = false;
    app.live.pending = false;
    app.live.changing = false;
    if failed && app.live.error.is_empty() {
        app.live.error = if was_ready {
            app.msg("Connessione interrotta.", "Connection lost.")
        } else {
            app.msg(
                "Impossibile avviare la sessione remota.",
                "Could not start the remote session.",
            )
        }
        .into();
    }
    if app.focus == Focus::Body && app.screen == Screen::Chat {
        app.focus = Focus::Editor;
    }
    app.dirty = true;
}

pub fn live_tick(app: &mut App, now: f64) {
    let idle = now - app.live.last_activity;
    let limit = match app.live.state {
        "ready" => 40_000.0,
        "opening" => 90_000.0,
        "closing" => 3_000.0,
        _ => return,
    };
    if idle > limit {
        let failed = app.live.state != "closing";
        live_closed(app, failed);
    }
}

fn decode_frame(bytes: &[u8]) -> Option<(Value, &[u8])> {
    let end = bytes.iter().take(1024).position(|b| *b == b'\n')?;
    let header: Value = serde_json::from_slice(&bytes[..end]).ok()?;
    let (w, h) = (header["width"].as_f64()?, header["height"].as_f64()?);
    (header["type"] == "frame"
        && header["tab"].is_string()
        && (1.0..=4096.0).contains(&w)
        && (1.0..=4096.0).contains(&h)
        && end + 1 < bytes.len())
    .then(|| (header, &bytes[end + 1..]))
}

pub fn socket_event(app: &mut App, event: SocketEvent) {
    app.live.last_activity = platform::now_ms();
    match event {
        SocketEvent::Binary(bytes) => {
            let Some((header, jpeg)) = decode_frame(&bytes) else {
                return;
            };
            let wanted = if desktop(app) {
                "desktop".to_owned()
            } else {
                app.live.active.clone()
            };
            if header["tab"] == wanted.as_str() {
                if desktop(app) && app.live.state == "opening" {
                    app.live.state = "ready";
                }
                let serial = app.next_frame_serial();
                app.live.frame = Some(ScreenFrame {
                    width: header["width"].as_f64().unwrap_or(1.0),
                    height: header["height"].as_f64().unwrap_or(1.0),
                    jpeg: Arc::new(jpeg.to_vec()),
                    serial,
                });
            }
        }
        SocketEvent::Text(text) => {
            let Ok(packet) = serde_json::from_str::<Value>(&text) else {
                return;
            };
            match packet["type"].as_str() {
                Some("ready") => app.live.state = "ready",
                Some("tabs") => {
                    let active = string(&packet, "active");
                    if active != app.live.active {
                        app.live.frame = None;
                    }
                    app.live.active = active;
                    app.live.tabs = packet["tabs"].as_array().cloned().unwrap_or_default();
                }
                Some("history") => {
                    app.live.history = packet["messages"].as_array().cloned().unwrap_or_default();
                }
                Some("control") => {
                    app.live.human = packet["human"] == true;
                    app.live.changing = false;
                }
                Some("working") => app.live.pending = true,
                Some("idle") => app.live.pending = false,
                Some("error") => {
                    app.live.changing = false;
                    app.live.pending = false;
                    app.live.error = if packet["status"] == 403 {
                        app.msg(
                            "Azione o sito non consentito.",
                            "Action or site not allowed.",
                        )
                    } else {
                        app.msg(
                            "Impossibile completare l'azione.",
                            "Could not complete the action.",
                        )
                    }
                    .into();
                }
                Some("ended") => {
                    let reason = string(&packet, "reason");
                    if reason == "busy" {
                        app.live.error = app
                            .msg(
                                "Questa sessione è già aperta.",
                                "This session is already open.",
                            )
                            .into();
                    }
                    live_closed(app, reason != "closed");
                }
                _ => {}
            }
        }
        SocketEvent::Closed => {
            if app.live.state == "closed" {
                return;
            }
            let closing = app.live.state == "closing";
            live_closed(app, !closing);
        }
    }
}

/// Keys while the remote screen has control: everything goes to the remote,
/// except Ctrl+] which hands control back.
pub async fn live_key(app: &mut App, _cx: &Cx, k: KeyEvent) -> Result<()> {
    let ctrl = k.modifiers.contains(Modifiers::CTRL);
    // Terminals report Ctrl+] as `]` or `5` with Control.
    if (ctrl && matches!(k.code, KeyCode::Char(']' | '5'))) || k.code == KeyCode::F(12) {
        if send(app, json!({"type": "release"})) {
            app.live.changing = true;
        }
        return Ok(());
    }
    let named = match k.code {
        KeyCode::Enter => "Enter",
        KeyCode::Backspace => "Backspace",
        KeyCode::Tab => "Tab",
        KeyCode::Escape => "Escape",
        KeyCode::Left => "ArrowLeft",
        KeyCode::Right => "ArrowRight",
        KeyCode::Up => "ArrowUp",
        KeyCode::Down => "ArrowDown",
        KeyCode::Delete => "Delete",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::Char(c) if !ctrl && !k.modifiers.contains(Modifiers::ALT) => {
            live_input(app, json!({"kind": "text", "text": c.to_string()}));
            return Ok(());
        }
        KeyCode::Char(_) => "",
        _ => return Ok(()),
    };
    let mut key = String::new();
    if ctrl {
        key.push_str("ctrl+");
    }
    if k.modifiers.contains(Modifiers::ALT) {
        key.push_str("alt+");
    }
    if k.modifiers.contains(Modifiers::SHIFT) && !named.is_empty() {
        key.push_str("shift+");
    }
    match k.code {
        KeyCode::Char(c) => key.push(c.to_ascii_lowercase()),
        _ => key.push_str(named),
    }
    live_input(app, json!({"kind": "key", "key": key}));
    Ok(())
}

/// Native mouse on the half-block image: clicks, pointer and wheel.
pub fn live_mouse(app: &mut App, m: MouseEvent) -> bool {
    let r = app.live.image;
    let inside =
        r.width > 0 && m.x >= r.x && m.x < r.x + r.width && m.y >= r.y && m.y < r.y + r.height;
    if !inside || !matches!(app.screen, Screen::Browser | Screen::Desktop) {
        return false;
    }
    let Some(frame) = app.live.frame.as_ref() else {
        return false;
    };
    if !app.live.human {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            app.focus = Focus::Body;
        }
        return matches!(m.kind, MouseEventKind::Down(_));
    }
    let x = (f64::from(m.x - r.x) + 0.5) / f64::from(r.width) * frame.width;
    let y = (f64::from(m.y - r.y) + 0.5) / f64::from(r.height) * frame.height;
    let desktop = desktop(app);
    let button = |b: MouseButton| match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    };
    let input = match m.kind {
        MouseEventKind::Down(b) if desktop => {
            json!({"kind":"pointer","x":x,"y":y,"button":button(b),"down":true})
        }
        MouseEventKind::Up(b) if desktop => {
            json!({"kind":"pointer","x":x,"y":y,"button":button(b),"down":false})
        }
        MouseEventKind::Down(MouseButton::Left) => json!({"kind":"click","x":x,"y":y}),
        MouseEventKind::Moved | MouseEventKind::Drag(_) if desktop => {
            json!({"kind":"move","x":x,"y":y})
        }
        MouseEventKind::ScrollUp => json!({"kind":"scroll","x":x,"y":y,"delta":-120}),
        MouseEventKind::ScrollDown => json!({"kind":"scroll","x":x,"y":y,"delta":120}),
        _ => return true,
    };
    app.focus = Focus::Body;
    live_input(app, input);
    true
}

async fn live_body_key(app: &mut App, cx: &Cx, k: KeyEvent) -> Result<()> {
    let ready = app.live.state == "ready";
    match k.code {
        KeyCode::Enter | KeyCode::Char('s') if app.live.state == "closed" => live_start(app, cx)?,
        KeyCode::Left | KeyCode::Right if app.live.state == "closed" && !desktop(app) => {
            let mut ids: Vec<String> = vec![String::new()];
            ids.extend(app.live.threads.iter().map(|t| string(t, "id")));
            let i = ids.iter().position(|t| *t == app.live.thread).unwrap_or(0);
            let n = ids.len();
            let next = if k.code == KeyCode::Right {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            };
            app.live.thread = ids[next].clone();
        }
        KeyCode::Char('c') if ready && !app.live.changing => {
            let packet = if app.live.human { "release" } else { "take" };
            if send(app, json!({"type": packet})) {
                app.live.changing = true;
                app.live.error.clear();
            }
        }
        KeyCode::Char('o') if ready && !desktop(app) => {
            ensure!(
                app.live.human,
                app.msg(
                    "Prendi il controllo (c) per navigare.",
                    "Take control (c) to browse."
                )
            );
            let url = app
                .live
                .tabs
                .iter()
                .find(|t| t["targetId"] == app.live.active.as_str())
                .map(|t| string(t, "url"))
                .filter(|u| u != "about:blank")
                .unwrap_or_default();
            app.form = Some(Form::new(
                app.msg("Apri indirizzo", "Open address").into(),
                vec![Field::new("url", "URL", &url, FieldKind::Text)],
                Act::OpenUrl,
            ));
        }
        KeyCode::Char('n') if ready && !desktop(app) && app.live.human => {
            send(app, json!({"type": "new_tab"}));
        }
        KeyCode::Char(c @ '1'..='9') if ready && !desktop(app) && app.live.human => {
            if let Some(tab) = app.live.tabs.get(c as usize - '1' as usize) {
                let id = string(tab, "targetId");
                send(app, json!({"type": "tab", "id": id}));
            }
        }
        KeyCode::Char('x') if app.live.state != "closed" => {
            if app.live.state == "opening" || !send(app, json!({"type": "end"})) {
                live_closed(app, false);
            } else {
                app.live.state = "closing";
                app.live.last_activity = platform::now_ms();
            }
        }
        KeyCode::Escape => app.focus = Focus::Editor,
        _ => {}
    }
    Ok(())
}

// ----- Dispatch ------------------------------------------------------------

fn items(app: &App) -> Vec<Item> {
    match &app.screen {
        Screen::Bot(id) => bot_items(app, id),
        Screen::Connections => connection_items(app),
        _ => Vec::new(),
    }
}

pub async fn body_key(app: &mut App, cx: &Cx, k: KeyEvent) -> Result<()> {
    // `/` starts a command from any screen, as in Claude Code.
    if k.code == KeyCode::Char('/') && app.input.is_empty() {
        app.focus = Focus::Editor;
        app.input.push('/');
        app.caret = 1;
        return Ok(());
    }
    if matches!(app.screen, Screen::Browser | Screen::Desktop) {
        return live_body_key(app, cx, k).await;
    }
    let list = items(app);
    let keys: Vec<&str> = list
        .iter()
        .map(|i| i.key.as_str())
        .filter(|k| !k.is_empty())
        .collect();
    let sel = keys.iter().position(|k| *k == app.body_key).unwrap_or(0);
    match k.code {
        KeyCode::Up => {
            if let Some(key) = keys.get(sel.saturating_sub(1)) {
                app.body_key = (*key).to_owned();
            }
            return Ok(());
        }
        KeyCode::Down => {
            if let Some(key) = keys.get((sel + 1).min(keys.len().saturating_sub(1))) {
                app.body_key = (*key).to_owned();
            }
            return Ok(());
        }
        KeyCode::Escape => {
            app.focus = Focus::Editor;
            return Ok(());
        }
        _ => {}
    }
    let Some(key) = keys.get(sel).map(|k| (*k).to_owned()) else {
        return Ok(());
    };
    app.body_key = key.clone();
    match app.screen.clone() {
        Screen::Bot(id) => {
            if k.code == KeyCode::Char('r') {
                app.refresh(cx).await?;
                load_mentions(app, cx, &id).await;
                return Ok(());
            }
            bot_key(app, cx, &id, &key, k).await
        }
        Screen::Connections => connections_key(app, cx, &key, k).await,
        _ => Ok(()),
    }
}

pub async fn body_click(app: &mut App, cx: &Cx, x: u16, y: u16) -> Result<bool> {
    let _ = cx;
    if !matches!(app.screen, Screen::Bot(_) | Screen::Connections)
        || y < app.body_top
        || x < app.body_left
    {
        return Ok(false);
    }
    let row = usize::from(y - app.body_top);
    let list = items(app);
    if row >= usize::from(app.body_height) {
        return Ok(false);
    }
    if let Some(item) = list.get(row + app.scroll_body()) {
        if !item.key.is_empty() {
            app.body_key = item.key.clone();
        }
        app.focus = Focus::Body;
        return Ok(true);
    }
    Ok(false)
}

impl App {
    fn scroll_body(&self) -> usize {
        usize::from(self.scroll)
    }
}

// ----- Forms ---------------------------------------------------------------

pub async fn form_key(app: &mut App, cx: &Cx, k: KeyEvent) -> Result<()> {
    let Some(form) = app.form.as_mut() else {
        return Ok(());
    };
    let ctrl = k.modifiers.contains(Modifiers::CTRL);
    let n = form.fields.len();
    let kind = form.fields.get(form.focus).map(|f| f.kind.clone());
    let mut rebuild_kind = None;
    match k.code {
        KeyCode::Escape => {
            app.form = None;
            return Ok(());
        }
        KeyCode::Tab | KeyCode::Down if n > 1 => {
            form.focus = (form.focus + 1) % n;
            form.caret = form.fields[form.focus].value.len();
        }
        KeyCode::BackTab | KeyCode::Up if n > 1 => {
            form.focus = (form.focus + n - 1) % n;
            form.caret = form.fields[form.focus].value.len();
        }
        KeyCode::Left | KeyCode::Right if matches!(kind, Some(FieldKind::Choice(_))) => {
            let field = &mut form.fields[form.focus];
            if let FieldKind::Choice(options) = &field.kind {
                let i = options.iter().position(|o| o.1 == field.value).unwrap_or(0);
                let len = options.len().max(1);
                let next = if k.code == KeyCode::Right {
                    (i + 1) % len
                } else {
                    (i + len - 1) % len
                };
                field.value = options.get(next).map(|o| o.1.clone()).unwrap_or_default();
                if field.key == "kind" {
                    rebuild_kind = Some(field.value.clone());
                }
            }
        }
        KeyCode::Left => {
            let field = &form.fields[form.focus];
            form.caret = field.value[..form.caret]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
        }
        KeyCode::Right => {
            let field = &form.fields[form.focus];
            form.caret += field.value[form.caret..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        }
        KeyCode::Home => form.caret = 0,
        KeyCode::End => form.caret = form.fields[form.focus].value.len(),
        KeyCode::Backspace
            if n > 0 && form.caret > 0 && !matches!(kind, Some(FieldKind::Choice(_))) =>
        {
            let field = &mut form.fields[form.focus];
            let prev = field.value[..form.caret]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
            field.value.drain(prev..form.caret);
            form.caret = prev;
        }
        KeyCode::Char('j') if ctrl && kind == Some(FieldKind::Multiline) => {
            form.fields[form.focus].value.insert(form.caret, '\n');
            form.caret += 1;
        }
        KeyCode::Enter
            if k.modifiers.intersects(Modifiers::SHIFT | Modifiers::ALT)
                && kind == Some(FieldKind::Multiline) =>
        {
            form.fields[form.focus].value.insert(form.caret, '\n');
            form.caret += 1;
        }
        KeyCode::Enter if form.focus + 1 < n => {
            form.focus += 1;
            form.caret = form.fields[form.focus].value.len();
        }
        KeyCode::Enter => {
            let form = app.form.take().expect("form");
            if let Err(e) = app.act(cx, &form).await {
                let mut form = form;
                form.error = e.to_string();
                app.form = Some(form);
            }
            return Ok(());
        }
        KeyCode::Char(c) if !ctrl && n > 0 && !matches!(kind, Some(FieldKind::Choice(_))) => {
            form.fields[form.focus].value.insert(form.caret, c);
            form.caret += c.len_utf8();
        }
        _ => {}
    }
    if let Some(kind) = rebuild_kind {
        let mut fields = std::mem::take(&mut app.form.as_mut().expect("form").fields);
        credential_fields(app, &kind, &mut fields);
        let body = if kind == "slack" {
            format!(
                "{} {}/oauth/slack",
                app.msg("URL di reindirizzamento OAuth:", "OAuth redirect URL:"),
                app.remote.workspace
            )
        } else {
            String::new()
        };
        let form = app.form.as_mut().expect("form");
        form.fields = fields;
        form.body = body;
    }
    Ok(())
}

pub fn draw_form(f: &mut Frame, app: &App) {
    let Some(form) = app.form.as_ref() else {
        return;
    };
    let area = f.area();
    let width = area.width.saturating_sub(4).min(72);
    let inner = usize::from(width.saturating_sub(4)).max(1);
    let mut lines: Vec<Line> = Vec::new();
    if !form.body.is_empty() {
        for l in Line::raw(form.body.clone()).wrap(inner, WrapMode::WordChar) {
            lines.push(Line::styled(l.to_plain_text(), fg(WHITE)));
        }
        lines.push(Line::default());
    }
    let mut cursor = None;
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        lines.push(Line::styled(
            clean(&field.label),
            if focused { fg(ORANGE).bold() } else { fg(GRAY) },
        ));
        let marker = if focused { "❯ " } else { "  " };
        let text = match &field.kind {
            FieldKind::Secret => "•".repeat(field.value.chars().count()),
            FieldKind::Choice(options) => format!(
                "‹ {} ›",
                options
                    .iter()
                    .find(|o| o.1 == field.value)
                    .map_or(field.value.as_str(), |o| o.0.as_str())
            ),
            _ => clean(&field.value),
        };
        let rows: Vec<&str> = if field.kind == FieldKind::Multiline {
            text.split('\n').collect()
        } else {
            vec![text.as_str()]
        };
        let visible = if field.kind == FieldKind::Multiline {
            6
        } else {
            1
        };
        let (caret_row, caret_col) = {
            let before = &field.value[..form.caret.min(field.value.len())];
            let row = before.matches('\n').count();
            let col = display_width(before.rsplit('\n').next().unwrap_or(""));
            let col = if field.kind == FieldKind::Secret {
                before.chars().count()
            } else {
                col
            };
            (row, col)
        };
        let first = (caret_row + 1).saturating_sub(visible);
        for (r, row) in rows.iter().enumerate().skip(first).take(visible) {
            let start = if focused && r == caret_row {
                caret_col.saturating_sub(inner.saturating_sub(3))
            } else {
                0
            };
            let clipped: String = row
                .chars()
                .skip(start)
                .take(inner.saturating_sub(2))
                .collect();
            if focused && r == caret_row && !matches!(field.kind, FieldKind::Choice(_)) {
                cursor = Some((lines.len(), 2 + caret_col - start));
            }
            lines.push(Line::from_spans(vec![
                Span::styled(if r == first { marker } else { "  " }, fg(ORANGE)),
                Span::styled(clipped, fg(WHITE)),
            ]));
        }
        if rows.is_empty() || (field.kind == FieldKind::Multiline && rows.len() <= first) {
            lines.push(Line::styled(marker, fg(ORANGE)));
        }
    }
    if !form.error.is_empty() {
        lines.push(Line::default());
        for l in Line::raw(form.error.clone()).wrap(inner, WrapMode::WordChar) {
            lines.push(Line::styled(l.to_plain_text(), fg(RED)));
        }
    }
    lines.push(Line::default());
    let hint = if form.fields.is_empty() {
        app.msg("Invio conferma · Esc annulla", "Enter confirm · Esc cancel")
    } else {
        app.msg(
            "Invio avanti/conferma · Tab campo · ←→ scelta · Esc annulla",
            "Enter next/confirm · Tab field · ←→ choice · Esc cancel",
        )
    };
    lines.push(Line::styled(hint, fg(DIM)));
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let body = bordered(f, rect, &form.title, ORANGE);
    paint(f, body, lines);
    if let Some((row, col)) = cursor {
        let y = body.y + row as u16;
        if y < body.y + body.height {
            f.set_cursor(Some((
                body.x + (col as u16).min(body.width.saturating_sub(1)),
                y,
            )));
        }
    }
}

// ----- Drawing -------------------------------------------------------------

/// Draw the center body of a non-conversation screen.
pub fn draw_body(f: &mut Frame, area: Rect, app: &mut App) {
    app.body_top = area.y;
    app.body_left = area.x;
    app.body_height = area.height;
    match app.screen.clone() {
        Screen::Browser | Screen::Desktop => draw_live(f, area, app),
        _ => {
            let list = items(app);
            let focused = app.focus == Focus::Body;
            if app.body_key.is_empty()
                && let Some(first) = list.iter().find(|i| !i.key.is_empty())
            {
                app.body_key = first.key.clone();
            }
            let sel = list
                .iter()
                .position(|i| i.key == app.body_key && !i.key.is_empty());
            // Keep the selected row visible.
            let height = usize::from(area.height);
            let mut offset = usize::from(app.scroll).min(list.len().saturating_sub(height));
            if let Some(s) = sel {
                if s < offset {
                    offset = s;
                } else if height > 0 && s >= offset + height {
                    offset = s + 1 - height;
                }
            }
            app.scroll = offset as u16;
            for (i, item) in list.iter().enumerate().skip(offset).take(height) {
                let y = area.y + (i - offset) as u16;
                let row = Rect::new(area.x, y, area.width, 1);
                let mut spans = vec![Span::styled(
                    if Some(i) == sel && focused {
                        "❯"
                    } else {
                        " "
                    },
                    fg(ORANGE),
                )];
                spans.extend(item.line.spans().iter().cloned());
                let mut p =
                    ftui::widgets::paragraph::Paragraph::new(ftui::text::Text::from_lines([
                        Line::from_spans(spans),
                    ]));
                if Some(i) == sel && focused {
                    p = p.style(Style::new().bg(Color::Ansi256(SELECTED_BG)));
                }
                ftui::Widget::render(&p, row, f);
            }
        }
    }
}

fn draw_live(f: &mut Frame, area: Rect, app: &mut App) {
    let desktop = desktop(app);
    let live = &app.live;
    let (state_text, color) = match live.state {
        "ready" if live.human => (app.msg("● hai il controllo", "● you have control"), GOLD),
        "ready" if live.pending => (
            app.msg(
                "✻ l'assistente sta navigando…",
                "✻ the assistant is browsing…",
            ),
            ORANGE,
        ),
        "ready" => (app.msg("● pronto", "● ready"), GREEN),
        "opening" => (app.msg("✻ avvio…", "✻ starting…"), ORANGE),
        "closing" => (app.msg("✻ chiusura…", "✻ closing…"), DIM),
        _ => (app.msg("○ fermo", "○ stopped"), DIM),
    };
    let title = if desktop {
        app.msg("Desktop remoto", "Remote desktop")
    } else {
        app.msg("Browser remoto", "Remote browser")
    };
    let mut header = vec![
        Span::styled(format!("{title}  "), fg(WHITE).bold()),
        Span::styled(state_text, fg(color)),
    ];
    if desktop && let Some(state) = live.status["state"].as_str() {
        header.push(Span::styled(format!("  · computer {state}"), fg(DIM)));
    }
    let mut lines = vec![Line::from_spans(header)];
    let focused = app.focus == Focus::Body;
    let hint = match live.state {
        "closed" if desktop => app.msg("Invio avvia il desktop", "Enter start the desktop").to_owned(),
        "closed" => {
            let thread = if live.thread.is_empty() {
                app.msg("Nuova conversazione", "New conversation").to_owned()
            } else {
                let when = live
                    .threads
                    .iter()
                    .find(|t| t["id"] == live.thread.as_str())
                    .map(|t| string(t, "updated_at").replace('T', " ").chars().take(16).collect::<String>())
                    .unwrap_or_default();
                format!(
                    "{} {} {when}",
                    app.msg("Conversazione", "Conversation"),
                    live.thread.chars().take(8).collect::<String>()
                )
            };
            format!(
                "‹ {thread} › ←→ {} · {}",
                app.msg("conversazione", "conversation"),
                app.msg("Invio avvia Kitesurf", "Enter starts Kitesurf")
            )
        }
        _ if live.human => app
            .msg(
                "Tasti e clic vanno allo schermo remoto · Ctrl+] restituisce il controllo",
                "Keys and clicks go to the remote screen · Ctrl+] hands back control",
            )
            .to_owned(),
        _ if desktop => app.msg("c prendi il controllo · x chiudi", "c take control · x close").to_owned(),
        _ => app
            .msg(
                "c prendi il controllo · o indirizzo · n scheda · 1-9 schede · x termina · Esc prompt",
                "c take control · o address · n tab · 1-9 tabs · x end · Esc prompt",
            )
            .to_owned(),
    };
    let hint = if focused || live.state == "closed" {
        hint
    } else {
        app.msg(
            "Tab per i comandi dello schermo · scrivi sotto per l'assistente",
            "Tab for screen controls · type below to ask the assistant",
        )
        .to_owned()
    };
    lines.push(Line::styled(hint, fg(if focused { GRAY } else { DIM })));
    if !desktop && !live.tabs.is_empty() {
        let mut spans = Vec::new();
        for (i, tab) in live.tabs.iter().enumerate() {
            let active = tab["targetId"] == live.active.as_str();
            let mut title: String = clean(&string(tab, "title")).chars().take(24).collect();
            if title.is_empty() {
                title = app.msg("Nuova scheda", "New tab").into();
            }
            spans.push(Span::styled(
                format!(" {} {title} ", i + 1),
                if active {
                    Style::new()
                        .fg(Color::Ansi256(WHITE))
                        .bg(Color::Ansi256(SELECTED_BG))
                        .bold()
                } else {
                    fg(GRAY)
                },
            ));
        }
        lines.push(Line::from_spans(spans));
        if let Some(tab) = live
            .tabs
            .iter()
            .find(|t| t["targetId"] == live.active.as_str())
        {
            lines.push(Line::styled(
                format!("↗ {}", clean(&string(tab, "url"))),
                fg(BLUE),
            ));
        }
    }
    if !live.error.is_empty() {
        lines.push(Line::styled(clean(&live.error), fg(RED)));
    }
    let header_height = (lines.len() as u16).min(area.height);
    paint(
        f,
        Rect::new(area.x, area.y, area.width, header_height),
        lines,
    );
    // Without the inspector pane, the assistant history sits under the screen.
    let history_rows: u16 = if !desktop && !app.nav.inspector_visible() && !live.history.is_empty()
    {
        5
    } else {
        0
    };
    let image = Rect::new(
        area.x,
        area.y + header_height + 1,
        area.width,
        area.height.saturating_sub(header_height + 1 + history_rows),
    );
    app.live.image = Rect::default();
    if let Some(frame) = app.live.frame.as_ref() {
        // Half-block cells are square pixels: fit the frame keeping its aspect.
        let max_w = f64::from(image.width);
        let max_h = f64::from(image.height) * 2.0;
        let scale = (max_w / frame.width).min(max_h / frame.height);
        let w = ((frame.width * scale) as u16).clamp(1, image.width.max(1));
        let h = (((frame.height * scale) / 2.0).ceil() as u16).clamp(1, image.height.max(1));
        let rect = Rect::new(image.x + (image.width - w) / 2, image.y, w, h);
        platform::draw_image(f, rect, frame);
        app.live.image = rect;
    } else if image.height > 0 {
        let text = match app.live.state {
            "closed" if desktop => app.msg(
                "Avvia il desktop per vederlo qui.",
                "Start the desktop to see it here.",
            ),
            "closed" => app.msg(
                "Avvia Kitesurf per vedere il browser qui.",
                "Start Kitesurf to see the browser here.",
            ),
            _ => app.msg(
                "In attesa dello schermo remoto…",
                "Waiting for the remote screen…",
            ),
        };
        let y = image.y + image.height / 2;
        let x = image.x + image.width.saturating_sub(display_width(text) as u16) / 2;
        paint(
            f,
            Rect::new(x, y, image.width.saturating_sub(x - image.x), 1),
            [Line::styled(text, fg(DIM))],
        );
    }
    if history_rows > 0 {
        let rect = Rect::new(
            area.x,
            area.y + area.height - history_rows,
            area.width,
            history_rows,
        );
        paint(
            f,
            rect,
            history_lines(app, usize::from(area.width))
                .into_iter()
                .rev()
                .take(history_rows as usize)
                .rev(),
        );
    }
}

/// The browser assistant conversation, Claude Code style.
pub fn history_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for message in &app.live.history {
        let user = message["role"] == "user";
        let text = clean(&string(message, "content"));
        for (i, l) in Line::raw(text)
            .wrap(width.saturating_sub(2).max(1), WrapMode::WordChar)
            .into_iter()
            .enumerate()
        {
            let marker = match (i, user) {
                (0, true) => "> ",
                (0, false) => "● ",
                _ => "  ",
            };
            out.push(Line::from_spans(vec![
                Span::styled(marker, fg(if user { GRAY } else { WHITE })),
                Span::styled(l.to_plain_text(), fg(if user { GRAY } else { WHITE })),
            ]));
        }
    }
    if app.live.pending && !app.live.human {
        out.push(Line::styled(
            app.msg(
                "✻ L'assistente sta navigando…",
                "✻ The assistant is browsing…",
            ),
            fg(ORANGE),
        ));
    }
    out
}

/// Interactive tool sessions read like a terminal in the transcript.
pub fn interactive_hint(app: &App) -> Option<String> {
    let (_, kind) = app.interactive()?;
    Some(if kind == "terminal" {
        app.msg(
            "Terminale interattivo · le righe che scrivi vanno al terminale · /end-session chiude",
            "Interactive terminal · lines you type go to the terminal · /end-session closes it",
        )
        .into()
    } else {
        app.msg(
            "Sotto-agente interattivo · le righe che scrivi vanno all'agente · /end-session chiude",
            "Interactive sub-agent · lines you type go to the agent · /end-session closes it",
        )
        .into()
    })
}
