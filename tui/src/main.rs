mod client;
mod editor;
mod login;
mod view;
use anyhow::{Context, Result, anyhow, bail, ensure};
use client::{Remote, origin};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::{Value, json};
use std::{io, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

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
struct App {
    remote: Remote,
    credential: keyring::Entry,
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
    feed: Option<JoinHandle<()>>,
    tx: mpsc::Sender<(String, Result<Value>)>,
}
#[derive(Clone, Copy)]
enum Picker {
    Models,
    Projects,
    Chats,
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
    async fn choose(&mut self, picker: Picker, index: usize) -> Result<()> {
        match picker {
            Picker::Models => {
                self.command(&format!("/model {}", index + 1)).await?;
            }
            Picker::Projects => {
                let project = if self.projects.is_empty() {
                    let (h, root) = self.folder_options()[index];
                    let body = json!({"harness_id":h["id"],"root":root,"path":"."});
                    let p = self
                        .remote
                        .api(Method::POST, "/projects", Some(body))
                        .await?;
                    self.projects.push(p.clone());
                    p
                } else {
                    self.projects[index].clone()
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
                let id = string(&self.project_chats()[index - 1], "id");
                self.open(&id).await?;
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
    fn record(&mut self, role: view::Role, s: impl AsRef<str>) {
        self.last_text.clear();
        self.lines.push(view::Message {
            role,
            text: clean(s.as_ref()),
        });
    }
    async fn refresh(&mut self) -> Result<()> {
        (self.projects, self.models, self.harnesses) = tokio::try_join!(
            self.remote.list("/projects"),
            self.remote.list("/models"),
            self.remote.list("/harnesses")
        )?;
        self.chats = self.remote.list("/conversations").await?;
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
            f.abort()
        }
        self.chat = Value::Null;
        self.lines.clear();
        self.patches.clear();
        self.question.clear();
        self.choices.clear();
        self.retry = None;
        self.plan.clear();
        self.goal.clear();
        self.approval.clear();
        self.last_text.clear();
        self.scroll = 0;
    }
    async fn open(&mut self, id: &str) -> Result<()> {
        let _: Uuid = id.parse()?;
        let c = self
            .remote
            .api(Method::GET, &format!("/conversations/{id}"), None)
            .await?;
        self.activate(c)
    }
    fn activate(&mut self, c: Value) -> Result<()> {
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
            .context("Project unavailable")?;
        self.chat = c;
        self.model = string(&self.chat, "model");
        self.question = string(&self.chat, "question_id");
        self.plan = string(&self.chat, "plan_id");
        self.goal = string(&self.chat, "goal_id");
        let remote = self.remote.clone();
        let tx = self.tx.clone();
        self.feed = Some(tokio::spawn(async move { remote.follow(id, tx).await }));
        Ok(())
    }
    fn event(&mut self, e: Value) {
        let val = |k| string(&e, k);
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
                    self.last_text = id
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
                    self.goal = val("message_id")
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
            }
            Some("plan") => {
                self.plan = val("reference_id");
                self.note(format!("{}\n/build", val("content")));
            }
            Some("goal_completed" | "goal_cancelled") => {
                self.goal.clear();
                if e["type"] == "goal_cancelled" {
                    self.question.clear();
                    self.plan.clear();
                    self.choices.clear();
                }
                self.note(format!("✓ {}", val("content")));
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
                    self.patches.push(clean(&diff));
                    self.record(view::Role::Output, diff);
                }
            }
            Some("approval") => {
                if e["approval_state"] == "pending" {
                    self.approval = val("approval_id");
                    self.note(format!("{}\n/allow  /deny", val("summary")));
                } else {
                    self.approval.clear()
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
                self.chat["state"] = e["state"].clone();
                if e["state"] == "idle" || e["state"] == "failed" {
                    self.approval.clear()
                }
            }
            Some("error") => self.record(view::Role::Error, val("message")),
            _ => {}
        }
    }
    async fn new_chat(&mut self, title: &str) -> Result<()> {
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
            .api(Method::POST, "/conversations", Some(body))
            .await?;
        self.activate(chat.clone())?;
        self.chats.insert(0, chat);
        Ok(())
    }
    fn welcome(&mut self) {
        self.note(self.msg("Scrivi il primo messaggio e premi Invio per iniziare. Premi F2 per riprendere una conversazione.", "Write your first message and press Enter to start. Press F2 to resume a conversation."));
    }
    async fn send(&mut self, text: &str, intent: &str, reference: Option<&str>) -> Result<()> {
        ensure!(!text.trim().is_empty(), "Messaggio vuoto / Empty message");
        if self.chat.is_null() && matches!(intent, "message" | "plan" | "goal") {
            self.new_chat("").await?;
        }
        let mut req = json!({"content":text});
        if intent != "message" {
            req["intent"] = json!(intent)
        }
        if let Some(id) = reference {
            req["reference_id"] = json!(id)
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
        self.remote.api(Method::POST, &path, Some(req)).await?;
        self.retry = None;
        Ok(())
    }
    async fn command(&mut self, line: &str) -> Result<bool> {
        let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        match command {
            "/logout" => {
                self.remote.api(Method::DELETE, "/session", None).await?;
                // The server has already revoked this session, including any cached copy.
                let _ = self.credential.delete_credential();
                return Ok(true);
            }
            "/diff" => {
                if self.patches.is_empty() { self.note(self.msg("Nessuna modifica nella conversazione", "No changes in this conversation")); }
                else { self.note(self.patches.join("\n")); }
            }
            "/rename" => {
                ensure!(!argument.is_empty(), "/rename <title>");
                let body = json!({"revision":self.chat["revision"], "title":argument});
                self.chat = self.remote.api(Method::PATCH, &self.path()?, Some(body)).await?;
            }
            "/status" => self.note(format!("{} · {} · {} · {}", self.remote.workspace, self.model, string(&self.chat,"permission_mode"), string(&self.chat,"state"))),
            "/quit" | "/exit" => return Ok(true),
            "/browser" => {
                ensure!(argument.is_empty(), "/browser");
                let url = format!("{}/browser", self.remote.workspace);
                let _ = open::that_detached(&url);
                self.note(format!("{} · {url}", self.msg("Browser remoto", "Remote browser")));
            }
            "/help" => self.note(self.msg(
                "/projects · /use N · /harnesses · /folder N radice percorso · /chats · /open N · /models · /model N · /new titolo\n/plan richiesta · /build · /goal obiettivo · /cancel-goal · /answer testo · /allow · /deny · /stop · /resume [N] · /status · /permissions · /diff · /rename · /browser · /logout · /exit\nAlt+Invio a capo · Ctrl+L modello · Ctrl+O strumenti · PgSu/PgGiù scorri · Fine segue · Esc interrompe · Ctrl+C esci",
                "/projects · /use N · /harnesses · /folder N root path · /chats · /open N · /models · /model N · /new title\n/plan request · /build · /goal objective · /cancel-goal · /answer text · /allow · /deny · /stop · /resume [N] · /status · /permissions · /diff · /rename · /browser · /logout · /exit\nAlt+Enter newline · Ctrl+L model · Ctrl+O tools · PgUp/PgDn scroll · End follow · Esc interrupt · Ctrl+C quit",
            )),
            "/projects" | "/harnesses" | "/models" | "/chats" | "/resume" | "/model" if argument.is_empty() => {
                self.refresh().await?;
                let listing = match command {
                    "/projects" => entries(&self.projects),
                    "/models" | "/model" => entries(&self.models),
                    "/harnesses" => self.harnesses.iter().enumerate()
                        .map(|(index, harness)| format!("{}  {}  {}", index + 1, string(harness, "name"), harness["roots"]))
                        .collect::<Vec<_>>().join("\n"),
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
            "/folder" => {
                let mut parts = argument.splitn(3, ' ');
                let harness = choice(&self.harnesses, parts.next().unwrap_or(""))?;
                let root = parts.next().context("/folder N root path")?;
                let folder = parts.next().unwrap_or(".");
                let payload = json!({"harness_id":harness["id"],"root":root,"path":folder});
                let project = self.remote.api(Method::POST, "/projects", Some(payload)).await?;
                self.close();
                self.project = project;
                self.refresh().await?;
            }
            "/open" | "/resume" => {
                let id = string(choice(&self.project_chats(), argument)?, "id");
                self.open(&id).await?;
            }
            "/permissions" => {
                if argument.is_empty() { self.note("ask · auto_edit · plan"); }
                else {
                    ensure!(["ask", "auto_edit", "plan"].contains(&argument), "/permissions ask|auto_edit|plan");
                    let path = self.path()?;
                    let payload = json!({"revision":self.chat["revision"],"permission_mode":argument});
                    self.chat = self.remote.api(Method::PATCH,&path,Some(payload)).await?;
                }
            }
            "/model" => {
                let model = string(choice(&self.models, argument)?, "id");
                if !self.chat.is_null() {
                    let path = self.path()?;
                    let chat = self.remote.api(Method::GET, &path, None).await?;
                    let update = json!({"revision":chat["revision"],"model":model});
                    self.chat = self.remote.api(Method::PATCH, &path, Some(update)).await?;
                }
                self.model = model;
            }
            "/new" => self.new_chat(argument).await?,
            "/stop" => {
                let path = format!("{}/interrupt", self.path()?);
                self.remote.api(Method::POST, &path, None).await?;
            }
            "/allow" | "/deny" => {
                ensure!(!self.approval.is_empty(), "Nessuna approvazione / No pending approval");
                let decision = if command == "/allow" { "allow_once" } else { "deny" };
                let path = format!("/approvals/{}", self.approval);
                self.remote.api(Method::POST, &path, Some(json!({"decision":decision}))).await?;
            }
            "/goal" | "/plan" => self.send(argument, &command[1..], None).await?,
            "/build" => {
                let id = self.plan.clone();
                ensure!(!id.is_empty(), "Nessun piano / No pending plan");
                let text = self.msg("Implementa il piano approvato", "Implement the approved plan");
                self.send(text, "implement_plan", Some(&id)).await?;
            }
            "/cancel-goal" => {
                ensure!(!self.goal.is_empty(), "Nessun obiettivo / No active goal");
                let path = format!("{}/goal/{}", self.path()?, self.goal);
                self.remote.api(Method::DELETE, &path, None).await?;
            }
            _ if command.starts_with('/') && command != "/answer" => {
                bail!("Comando sconosciuto / Unknown command: {command}")
            }
            _ if !self.question.is_empty() => {
                let question = self.question.clone();
                let answer = if command == "/answer" { argument } else { line };
                let choice = answer.parse::<usize>().ok().and_then(|n| n.checked_sub(1))
                    .and_then(|i| self.choices.get(i)).map(String::as_str).unwrap_or(answer).to_owned();
                self.send(&choice, "answer", Some(&question)).await?;
            }
            _ => {
                ensure!(command != "/answer", "Nessuna domanda / No pending question");
                self.send(line, "message", None).await?;
            }
        }
        Ok(false)
    }
}
async fn run_ui(
    mut app: App,
    mut rx: mpsc::Receiver<(String, Result<Value>)>,
    start: Option<String>,
) -> Result<()> {
    app.refresh().await?;
    app.model = app
        .models
        .iter()
        .find(|m| m["default"] == true)
        .or(app.models.first())
        .map(|v| string(v, "id"))
        .unwrap_or_default();
    let mut picker = None;
    if let Some(id) = start {
        app.open(&id).await?;
    } else if let [project] = app.projects.as_slice() {
        app.project = project.clone();
        app.welcome();
    } else {
        picker = Some(Picker::Projects);
    }
    crossterm::terminal::enable_raw_mode()?;
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
            if self.0 {
                let _ = crossterm::execute!(
                    io::stdout(),
                    crossterm::event::PopKeyboardEnhancementFlags
                );
            }
            let _ = crossterm::execute!(
                io::stdout(),
                crossterm::style::ResetColor,
                crossterm::cursor::Show,
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let mut _restore = Restore(false);
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    let mut terminal = ftui::TerminalWriter::new(
        io::stdout(),
        ftui::ScreenMode::AltScreen,
        ftui::UiAnchor::Bottom,
        ftui::TerminalCapabilities::detect(),
    );
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        crossterm::execute!(
            io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }
    _restore.0 = enhanced;
    let mut keys = EventStream::new();
    let mut draft_history: Vec<String> = Vec::new();
    let mut hist_index = 0;
    let mut caret = 0;
    let mut parked_draft = String::new();
    let mut hide_menu = false;
    use ftui::widgets::spinner::DOTS;
    let mut tick = tokio::time::interval(Duration::from_millis(150));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame = 0;
    let mut selection = 0usize;
    loop {
        let options = match picker {
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
            None => Vec::new(),
        };
        let suggestions: Vec<_> = if picker.is_some() {
            options.iter().map(String::as_str).collect()
        } else {
            COMMANDS
                .iter()
                .copied()
                .filter(|cmd| {
                    !hide_menu && app.input.starts_with('/') && cmd.starts_with(&app.input)
                })
                .collect()
        };
        selection = selection.min(suggestions.len().saturating_sub(1));
        let working = app.chat["state"] == "running" && app.feed.is_some();
        let size = crossterm::terminal::size()?;
        if (terminal.width(), terminal.height()) != size {
            terminal.set_size(size.0, size.1);
        }
        let width = usize::from(size.0.saturating_sub(2)).max(1);
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
        let ev = tokio::select! {
            _ = tick.tick(), if working => { frame = (frame + 1) % DOTS.len(); continue; }
            Some((id,event)) = rx.recv() => {
                if app.chat["id"] == id {
                    match event {
                        Ok(event) => app.event(event),
                        Err(error) => { app.feed.take(); app.status = error.to_string(); }
                    }
                }
                continue;
            }
            item = keys.next() => { let Some(event) = item else {break}; event? }
        };
        if let Event::Paste(paste) = ev {
            if picker.is_some() {
                continue;
            }
            let text =
                clean(&paste.replace("\r\n", "\n").replace('\r', "\n")).replace('\t', "    ");
            app.input.insert_str(caret, &text);
            caret += text.len();
            hide_menu = false;
        } else if let Event::Key(k) = ev {
            if k.kind == KeyEventKind::Release {
                continue;
            }
            match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::F(2) | KeyCode::F(3) => match app.refresh().await {
                    Ok(()) => {
                        picker = Some(if k.code == KeyCode::F(3) || app.project.is_null() {
                            Picker::Projects
                        } else {
                            Picker::Chats
                        });
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
                KeyCode::Esc if picker.is_some() => {
                    picker = None;
                    selection = 0;
                }
                KeyCode::Enter if let Some(menu) = picker => {
                    if options.is_empty() {
                        continue;
                    }
                    match app.choose(menu, selection).await {
                        Ok(()) => {
                            picker = None;
                            selection = 0;
                            app.status.clear();
                        }
                        Err(error) => app.status = error.to_string(),
                    }
                }
                _ if picker.is_some() && !matches!(k.code, KeyCode::Up | KeyCode::Down) => {}
                KeyCode::Char('o') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.compact_tools = !app.compact_tools
                }
                KeyCode::Esc if !suggestions.is_empty() => hide_menu = true,
                KeyCode::Esc if !app.chat.is_null() => {
                    if let Err(e) = app.command("/stop").await {
                        app.status = e.to_string()
                    }
                }
                KeyCode::Up if !suggestions.is_empty() => selection = selection.saturating_sub(1),
                KeyCode::Down if !suggestions.is_empty() => {
                    selection = (selection + 1).min(suggestions.len() - 1)
                }
                KeyCode::Tab if !suggestions.is_empty() => {
                    app.input = format!("{} ", suggestions[selection]);
                    caret = app.input.len();
                    selection = 0;
                }
                KeyCode::PageUp => app.scroll = app.scroll.saturating_add(12),
                KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(12),
                KeyCode::Up | KeyCode::Down
                    if let Some(next) =
                        editor::vertical(&app.input, caret, width, k.code == KeyCode::Down) =>
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
                        .map_or(0, |(pos, _)| pos)
                }
                KeyCode::Right => {
                    caret += app.input[caret..]
                        .graphemes(true)
                        .next()
                        .map_or(0, str::len)
                }
                KeyCode::Home => caret = app.input[..caret].rfind('\n').map_or(0, |n| n + 1),
                KeyCode::End if app.scroll > 0 => app.scroll = 0,
                KeyCode::End => {
                    caret += app.input[caret..]
                        .find('\n')
                        .unwrap_or(app.input.len() - caret)
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
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
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
                    let line = suggestions
                        .get(selection)
                        .copied()
                        .unwrap_or(app.input.trim())
                        .to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    app.status.clear();
                    match app.command(&line).await {
                        Ok(quit) => {
                            if quit {
                                break;
                            }
                            draft_history.push(line);
                            hist_index = draft_history.len();
                            app.input.clear();
                            parked_draft.clear();
                            app.scroll = 0;
                            hide_menu = false;
                            caret = 0
                        }
                        Err(e) => app.status = e.to_string(),
                    }
                }
                _ => {}
            }
        }
    }
    app.close();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
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
        match arg.as_str() {
            "--portal" => portal = Some(args.next().context("--portal URL")?),
            "--standalone" => {
                standalone = Some(client::standalone_origin(
                    &args.next().context("--standalone URL")?,
                )?)
            }
            "--conversation" => start = Some(args.next().context("--conversation UUID")?),
            "--ticket-stdin" => stdin = true,
            "--no-browser" => {
                browser = false;
                manual = true;
            }
            "--manual" => manual = true,
            "--ca" => ca = Some(std::path::PathBuf::from(args.next().context("--ca PEM")?)),
            "--en" => en = true,
            "--help" | "-h" => {
                println!(
                    "venticode-tui --portal https://portal.example.org [--conversation UUID] [--en] [--manual] [--ca PEM] [--no-browser] [--ticket-stdin]"
                );
                println!(
                    "venticode-tui --standalone https://code.example.org [--conversation UUID] [--en] [--ca PEM]"
                );
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
    let portal = match portal.or_else(|| standalone.clone()) {
        Some(value) if standalone.is_some() => value,
        Some(value) => origin(&value)?,
        None => {
            eprintln!(
                "{}",
                if en {
                    "Ventic portal HTTPS URL"
                } else {
                    "URL HTTPS del portale Ventic"
                }
            );
            let mut value = String::new();
            io::stdin().read_line(&mut value)?;
            origin(value.trim())?
        }
    };
    let credential = keyring::Entry::new(
        if standalone.is_some() {
            "venticode-tui-standalone"
        } else {
            "venticode-tui"
        },
        &portal,
    )?;
    let mut session = None;
    if !stdin {
        match credential.get_password() {
            Ok(saved) => {
                let remote = Remote::restore(ca.as_deref(), &saved)?;
                if remote.valid().await? {
                    session = Some(remote);
                } else {
                    credential.delete_credential()?;
                }
            }
            Err(keyring::Error::NoEntry) => (),
            Err(e) => eprintln!("Portachiavi non disponibile / Keyring unavailable: {e}"),
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
            eprintln!("Accedi nel browser / Sign in in your browser: {url}");
            if browser {
                let _ = open::that_detached(&url);
            }
            rpassword::prompt_password(if en {
                "One-time code: "
            } else {
                "Codice monouso: "
            })?
        } else {
            login::browser(&portal).await?
        };
        let remote = if standalone.is_some() {
            Remote::connect_standalone(ca.as_deref(), &portal, code.trim()).await?
        } else {
            Remote::connect(ca.as_deref(), &portal, code.trim()).await?
        };
        if let Err(e) = credential.set_password(&remote.saved()?) {
            eprintln!("Sessione solo in memoria / Session available for this launch only: {e}");
        }
        remote
    };
    let (tx, rx) = mpsc::channel(256);
    run_ui(
        App {
            remote,
            credential,
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
            feed: None,
            tx,
        },
        rx,
        start,
    )
    .await
}
