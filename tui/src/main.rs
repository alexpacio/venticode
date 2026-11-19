mod client;
use anyhow::{Context, Result, anyhow, bail, ensure};
use client::{Remote, origin};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, Paragraph, Wrap},
};
use reqwest::Method;
use serde_json::{Value, json};
use std::{io, process::Command};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

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
    en: bool,
    projects: Vec<Value>,
    models: Vec<Value>,
    harnesses: Vec<Value>,
    chats: Vec<Value>,
    project: Value,
    chat: Value,
    model: String,
    lines: Vec<String>,
    last_text: String,
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
impl App {
    fn msg<'a>(&self, it: &'a str, en: &'a str) -> &'a str {
        if self.en { en } else { it }
    }
    fn path(&self) -> Result<String> {
        Ok(format!(
            "/conversations/{}",
            self.chat["id"]
                .as_str()
                .context("Apri una conversazione / Open a conversation")?
        ))
    }
    fn note(&mut self, s: impl AsRef<str>) {
        self.last_text.clear();
        self.lines.push(clean(s.as_ref()));
        self.scroll = 0;
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
            .filter(|c| c["project_id"] == self.project["id"])
            .cloned()
            .collect()
    }
    fn close(&mut self) {
        if let Some(f) = self.feed.take() {
            f.abort()
        }
        self.chat = Value::Null;
        self.lines.clear();
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
        let id = id.to_owned();
        self.feed = Some(tokio::spawn(async move { remote.follow(id, tx).await }));
        Ok(())
    }
    fn event(&mut self, e: Value) {
        let val = |k| string(&e, k);
        match e["type"].as_str() {
            Some("text") => {
                let id = val("message_id");
                if self.last_text != id || self.lines.is_empty() {
                    self.lines.push(String::new());
                    self.last_text = id
                }
                self.lines
                    .last_mut()
                    .unwrap()
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
                self.note(format!("› {}", val("content")));
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
            Some("tool_call") => self.note(format!("→ {} {}", val("operation"), val("target"))),
            Some("tool_output") => self.note(val("data")),
            Some("tool_result") => {
                self.note(format!("{} {}", val("status"), val("summary")));
                if let Some(patch) = e["diff"]["patch"].as_str().filter(|s| !s.is_empty()) {
                    self.note(format!("{}\n{}", string(&e["diff"], "path"), patch));
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
            Some("error") => self.note(val("message")),
            _ => {}
        }
    }
    async fn send(&mut self, text: &str, intent: &str, reference: Option<&str>) -> Result<()> {
        ensure!(!text.trim().is_empty(), "Messaggio vuoto / Empty message");
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
            "/quit" => return Ok(true),
            "/help" => self.note(self.msg(
                "/projects · /use N · /harnesses · /folder N radice percorso · /chats · /open N · /models · /model N · /new titolo\n/plan richiesta · /build · /goal obiettivo · /cancel-goal · /answer testo · /allow · /deny · /stop · /quit",
                "/projects · /use N · /harnesses · /folder N root path · /chats · /open N · /models · /model N · /new title\n/plan request · /build · /goal objective · /cancel-goal · /answer text · /allow · /deny · /stop · /quit",
            )),
            "/projects" | "/harnesses" | "/models" | "/chats" => {
                self.refresh().await?;
                let listing = match command {
                    "/projects" => entries(&self.projects),
                    "/models" => entries(&self.models),
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
            "/open" => {
                let id = string(choice(&self.project_chats(), argument)?, "id");
                self.open(&id).await?;
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
            "/new" => {
                ensure!(!self.project.is_null(), "/projects → /use N");
                let mut payload = json!({"project_id":self.project["id"],"model":self.model});
                if !argument.is_empty() {
                    payload["title"] = json!(argument);
                }
                let chat = self.remote.api(Method::POST, "/conversations", Some(payload)).await?;
                self.open(&string(&chat, "id")).await?;
                self.refresh().await?;
            }
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
    if let Some(id) = start {
        app.open(&id).await?
    } else {
        app.note(app.msg(
            "Scegli un progetto con /use N. Poi /chats o /new titolo.",
            "Select a project with /use N. Then /chats or /new title.",
        ));
        app.note(entries(&app.projects));
    }
    let mut terminal = ratatui::try_init()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ratatui::restore();
        }
    }
    let _restore = Restore;
    let mut keys = EventStream::new();
    let mut draft_history: Vec<String> = Vec::new();
    let mut hist_index = 0;
    let mut caret = 0;
    loop {
        terminal.draw(|f| {
            let [head, body, foot, input] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(2),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .areas(f.area());
            let title = clean(&format!(
                " Venticode  {} / {}  ·  {}  ·  {}  {}",
                string(&app.project, "name"),
                string(&app.chat, "title"),
                app.model,
                string(&app.chat, "permission_mode"),
                string(&app.chat, "state")
            ));
            f.render_widget(
                Paragraph::new(title).style(Style::new().fg(Color::Cyan)),
                head,
            );
            let text = app.lines.join("\n");
            let width = usize::from(body.width.max(1));
            let height = text
                .split('\n')
                .map(|s| UnicodeWidthStr::width(s).max(1).div_ceil(width))
                .sum::<usize>();
            let offset = height
                .saturating_sub(body.height as usize)
                .min(u16::MAX as usize) as u16;
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((offset.saturating_sub(app.scroll), 0)),
                body,
            );
            let status = if app.status.is_empty() {
                app.msg(
                    "Invio invia · Esc interrompe · PgSu/PgGiù scorri · /help",
                    "Enter send · Esc interrupt · PgUp/PgDn scroll · /help",
                )
            } else {
                &app.status
            };
            f.render_widget(
                Paragraph::new(clean(status)).style(Style::new().fg(Color::Yellow)),
                foot,
            );
            let title = if app.question.is_empty() {
                app.msg("Messaggio", "Message")
            } else {
                app.msg("Risposta", "Answer")
            };
            let pos = UnicodeWidthStr::width(&app.input[..caret]);
            let avail = usize::from(input.width.saturating_sub(3));
            let left = pos.saturating_sub(avail).min(u16::MAX as usize) as u16;
            f.render_widget(
                Paragraph::new(app.input.as_str())
                    .scroll((0, left))
                    .block(Block::bordered().title(title)),
                input,
            );
            f.set_cursor_position((
                input.x.saturating_add(1 + pos.min(avail) as u16),
                input.y + 1,
            ));
        })?;
        tokio::select! {
            Some((id,v))=rx.recv()=>{if app.chat["id"]==id{match v{Ok(e)=>app.event(e),Err(e)=>app.status=e.to_string()}}},
            item=keys.next()=>{let Some(ev)=item else{break};if let Event::Key(k)=ev?{
                if k.kind==KeyEventKind::Release{continue}
                match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL)=>break,
                    KeyCode::Esc=>{if let Err(e)=app.command("/stop").await{app.status=e.to_string()}},
                    KeyCode::PageUp=>app.scroll=app.scroll.saturating_add(12),KeyCode::PageDown=>app.scroll=app.scroll.saturating_sub(12),
                    KeyCode::Up=>{if hist_index>0{hist_index-=1;app.input=draft_history[hist_index].clone();caret=app.input.len();}},
                    KeyCode::Down=>{hist_index=(hist_index+1).min(draft_history.len());app.input=draft_history.get(hist_index).cloned().unwrap_or_default();caret=app.input.len();},
                    KeyCode::Left=>caret=app.input[..caret].char_indices().next_back().map_or(0,|(pos,_)|pos),
                    KeyCode::Right=>caret+=app.input[caret..].chars().next().map_or(0,char::len_utf8),
                    KeyCode::Home=>caret=0,KeyCode::End=>caret=app.input.len(),
                    KeyCode::Delete if caret<app.input.len()=>{app.input.remove(caret);},
                    KeyCode::Backspace if caret>0=>{let prev=app.input[..caret].char_indices().next_back().unwrap().0;app.input.drain(prev..caret);caret=prev;},
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL)=>{app.input.insert(caret,c);caret+=c.len_utf8();},
                    KeyCode::Enter=>{let line=app.input.trim().to_owned();if line.is_empty(){continue}app.status.clear();
                        match app.command(&line).await{Ok(quit)=>{if quit{break}draft_history.push(line);hist_index=draft_history.len();app.input.clear();caret=0},Err(e)=>app.status=e.to_string()}
                    },_=>{}
                }
            }}
        }
    }
    app.close();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut portal = None;
    let mut start = None;
    let mut stdin = false;
    let mut browser = true;
    let mut en = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--portal" => portal = Some(args.next().context("--portal URL")?),
            "--conversation" => start = Some(args.next().context("--conversation UUID")?),
            "--ticket-stdin" => stdin = true,
            "--no-browser" => browser = false,
            "--en" => en = true,
            "--help" | "-h" => {
                println!(
                    "venticode-tui --portal https://portal.example.org [--conversation UUID] [--en] [--no-browser] [--ticket-stdin]"
                );
                return Ok(());
            }
            _ => return Err(anyhow!("Unknown option: {arg}")),
        }
    }
    let portal = match portal {
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
    let url = format!("{portal}/app/venticode?terminal=1");
    let code = if stdin {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        line
    } else {
        eprintln!(
            "{} {url}",
            if en {
                "Sign in using your browser:"
            } else {
                "Accedi con il browser:"
            }
        );
        if browser {
            #[cfg(windows)]
            {
                let _ = Command::new("rundll32.exe")
                    .args(["url.dll,FileProtocolHandler", &url])
                    .spawn();
            }
            #[cfg(not(windows))]
            {
                let _ = Command::new("xdg-open").arg(&url).spawn();
            }
        }
        rpassword::prompt_password(if en {
            "One-time code: "
        } else {
            "Codice monouso: "
        })?
    };
    let remote = Remote::connect(&portal, code.trim()).await?;
    drop(code);
    let (tx, rx) = mpsc::channel(256);
    run_ui(
        App {
            remote,
            en,
            projects: vec![],
            models: vec![],
            harnesses: vec![],
            chats: vec![],
            project: Value::Null,
            chat: Value::Null,
            model: String::new(),
            lines: vec![],
            last_text: String::new(),
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
