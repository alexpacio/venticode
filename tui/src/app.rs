//! Application core shared by the console client and the web build: state,
//! the transcript, slash commands, keyboard and mouse handling, and every API
//! action. The platform module supplies HTTP, streams, sockets and clipboard.

use crate::platform::{self, Cx, Method, Remote, StreamUpdate, Tx, Update};
use crate::{editor, nav, screens, view};
use anyhow::{Context, Result, bail, ensure};
use ftui::Frame;
use ftui::core::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

const MAX_DRAFT_HISTORY: usize = 200;
const MAX_PATCHES: usize = 200;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const REPLAY_BELL_GRACE_MS: f64 = 500.0;
const REFRESH_EVERY_MS: f64 = 30_000.0;
/// Pasted payloads longer than this collapse to a `[Pasted N chars #k]`
/// placeholder; the full text is still sent with the message.
const PASTE_COLLAPSE_THRESHOLD: usize = 2048;
/// Claude Code's thinking spinner cycles through star glyphs.
pub const STARS: [&str; 6] = ["✻", "✽", "✢", "✳", "✶", "·"];

/// Slash commands with their Italian and English descriptions.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("/help", "Comandi e scorciatoie", "Commands and shortcuts"),
    (
        "/new",
        "Nuova conversazione [titolo]",
        "New conversation [title]",
    ),
    (
        "/resume",
        "Riprendi una conversazione [N]",
        "Resume a conversation [N]",
    ),
    (
        "/skills",
        "Skill e plugin installati · /skill:nome",
        "Installed skills and plugins · /skill:name",
    ),
    (
        "/showcase",
        "Sfoglia e installa skill e plugin [owner/repo]",
        "Browse and install skills and plugins [owner/repo]",
    ),
    (
        "/uninstall",
        "Rimuovi: skill|plugin nome [user]",
        "Remove: skill|plugin name [user]",
    ),
    (
        "/import",
        "Importa una sessione Claude Code o Codex",
        "Import a Claude Code or Codex session",
    ),
    ("/model", "Scegli il modello", "Choose the model"),
    ("/effort", "Sforzo di ragionamento", "Reasoning effort"),
    ("/permissions", "Modalità dei permessi", "Permission mode"),
    (
        "/plan",
        "Chiedi un piano prima di modificare",
        "Ask for a plan first",
    ),
    (
        "/build",
        "Esegui il piano approvato",
        "Run the approved plan",
    ),
    ("/goal", "Lavora fino a un obiettivo", "Work towards a goal"),
    (
        "/cancel-goal",
        "Annulla l'obiettivo attivo",
        "Cancel the active goal",
    ),
    ("/answer", "Rispondi alla domanda", "Answer the question"),
    (
        "/allow",
        "Consenti lo strumento in attesa",
        "Allow the pending tool",
    ),
    (
        "/deny",
        "Nega lo strumento in attesa",
        "Deny the pending tool",
    ),
    ("/stop", "Interrompi l'esecuzione", "Interrupt the run"),
    (
        "/input",
        "Invia una riga alla sessione interattiva",
        "Send a line to the interactive session",
    ),
    (
        "/end-session",
        "Chiudi la sessione interattiva",
        "Close the interactive session",
    ),
    ("/status", "Stato della sessione", "Session status"),
    (
        "/rename",
        "Rinomina la conversazione",
        "Rename the conversation",
    ),
    (
        "/delete",
        "Elimina la conversazione",
        "Delete the conversation",
    ),
    (
        "/delete-project",
        "Elimina il progetto",
        "Delete the project",
    ),
    (
        "/diff",
        "Modifiche della conversazione",
        "Changes in this conversation",
    ),
    ("/copy", "Copia l'ultima risposta", "Copy the last reply"),
    (
        "/paste",
        "Incolla dagli appunti",
        "Paste from the clipboard",
    ),
    ("/projects", "Elenca i progetti", "List projects"),
    ("/use", "Usa il progetto N", "Use project N"),
    ("/harnesses", "Elenca gli harness", "List harnesses"),
    (
        "/folder",
        "Sfoglia o crea un progetto",
        "Browse or create a project",
    ),
    ("/chats", "Elenca le conversazioni", "List conversations"),
    ("/open", "Apri la conversazione N", "Open conversation N"),
    ("/models", "Elenca i modelli", "List models"),
    ("/bots", "Bot e menzioni", "Bots and mentions"),
    ("/connections", "Connessioni dei bot", "Bot connections"),
    ("/browser", "Browser remoto", "Remote browser"),
    ("/desktop", "Desktop remoto", "Remote desktop"),
    ("/language", "Cambia lingua", "Switch language"),
    ("/reconnect", "Riconnetti lo stream", "Reconnect the stream"),
    ("/logout", "Esci dal workspace", "Sign out"),
    ("/exit", "Chiudi", "Quit"),
];

pub fn string(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_owned()
}

pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect()
}

pub fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
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
            let name = v["name"]
                .as_str()
                .or(v["title"].as_str())
                .or(v["id"].as_str())
                .unwrap_or("?");
            format!("{}  {name}", n + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn paste_placeholder(index: usize, payload: &str) -> String {
    format!("[Pasted {} chars #{}]", payload.chars().count(), index + 1)
}

/// Insert pasted text at the caret. Long pastes collapse to a numbered
/// placeholder; the full text is kept in `payloads` and restored on send.
pub fn insert_paste(input: &mut String, caret: &mut usize, raw: &str, payloads: &mut Vec<String>) {
    let text = clean(&raw.replace("\r\n", "\n").replace('\r', "\n")).replace('\t', "    ");
    let text = if text.chars().count() > PASTE_COLLAPSE_THRESHOLD {
        payloads.push(text);
        paste_placeholder(payloads.len() - 1, &payloads[payloads.len() - 1])
    } else {
        text
    };
    input.insert_str(*caret, &text);
    *caret += text.len();
}

/// Swap each placeholder still present for its payload. Matching by text,
/// not by offset, keeps edits made around a placeholder from corrupting it.
pub fn expand_pasted(input: &str, payloads: &[String]) -> String {
    payloads
        .iter()
        .enumerate()
        .fold(input.to_owned(), |text, (i, payload)| {
            text.replacen(&paste_placeholder(i, payload), payload, 1)
        })
}

pub fn delete_prev_word(input: &mut String, caret: &mut usize) {
    let trimmed = input[..*caret].trim_end_matches(char::is_whitespace);
    let start = trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    input.drain(start..*caret);
    *caret = start;
}

pub fn append_capped_output(dest: &mut String, chunk: &str, max_bytes: usize) {
    const TRUNCATED: &str = "\n… [output truncated]";
    if dest.len() >= max_bytes {
        return;
    }
    let remaining = max_bytes - dest.len();
    if chunk.len() <= remaining {
        dest.push_str(chunk);
    } else {
        let mut cut = remaining;
        while cut > 0 && !chunk.is_char_boundary(cut) {
            cut -= 1;
        }
        dest.push_str(&chunk[..cut]);
        dest.push_str(TRUNCATED);
    }
}

/// Rows a picker shows at once; longer lists scroll with the selection.
pub const PICKER_ROWS: usize = 12;

/// Percent-encode a query value (the browser build has no `url` crate).
fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `skill · name · description`, with plugin compatibility hints.
fn showcase_label(item: &Value, en: bool) -> String {
    let mut label = format!("{} · {}", string(item, "kind"), string(item, "name"));
    if item["kind"] == "plugin" {
        label.push_str(match (item["tools"] == true, en) {
            (true, true) => " [tools]",
            (true, false) => " [strumenti]",
            (false, true) => " [no tools: commands/UI only]",
            (false, false) => " [senza strumenti: solo comandi/UI]",
        });
    }
    let description: String = string(item, "description").chars().take(90).collect();
    if !description.is_empty() {
        label.push_str(" · ");
        label.push_str(&description);
    }
    clean(&label)
}

/// Slash menu entries `(command, italiano, english)`: the built-in commands,
/// then `/skill:name` for each listed pi skill (its description is not translated).
pub fn slash_suggestions(
    input: &str,
    hide: bool,
    skills: &[Value],
) -> Vec<(String, String, String)> {
    if hide || !input.starts_with('/') || input.contains(' ') {
        return Vec::new();
    }
    let skills = skills.iter().map(|s| {
        let description = string(s, "description");
        (
            format!("/skill:{}", string(s, "name")),
            description.clone(),
            description,
        )
    });
    COMMANDS
        .iter()
        .map(|c| (c.0.to_owned(), c.1.to_owned(), c.2.to_owned()))
        .chain(skills)
        .filter(|c| c.0.starts_with(input))
        .collect()
}

fn needs_tab_completion(line: &str, suggestions: usize, skills: &[Value]) -> bool {
    line.starts_with('/')
        && !line.starts_with("//")
        && !line.contains(' ')
        && !slash_suggestions(line, false, skills)
            .iter()
            .any(|c| c.0 == line)
        && suggestions > 0
}

/// Claude Code style tool title: `Read(path)`, `Bash(cargo test)`.
pub fn tool_head(operation: &str, target: &str) -> String {
    let name = match operation {
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Update",
        "execute" => "Bash",
        "list_files" => "List",
        "find_files" => "Find",
        "search_files" => "Search",
        "mkdir" => "Mkdir",
        "rename" => "Rename",
        "remove" => "Remove",
        "terminal_start" => "Terminal",
        "agent_start" => "Agent",
        "ask_model" => "Ask",
        "preview" => "Preview",
        "project_service_start" => "Service",
        "fetch_url" => "Fetch",
        other if other.starts_with("extension_") => "Extension",
        other if other.starts_with("mcp_") => "MCP",
        other if other.starts_with("browser_") => "Browser",
        other if other.starts_with("desktop_") => "Desktop",
        other => other,
    };
    if target.is_empty() {
        name.to_owned()
    } else {
        format!("{name}({})", target.replace('\n', " "))
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Role {
    User,
    Assistant,
    Thinking,
    Tool,
    #[default]
    Notice,
    Error,
    Todo,
}

/// One transcript entry. Tools keep their title, status, result summary,
/// output and diff together so they render as one Claude Code style block.
#[derive(Clone, Default, Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub head: String,
    pub status: String,
    pub note: String,
    pub diff: String,
    pub interactive: String,
}

impl Message {
    pub fn new(role: Role, text: impl Into<String>) -> Self {
        Message {
            role,
            text: text.into(),
            ..Default::default()
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Screen {
    Chat,
    Bot(String),
    Connections,
    Browser,
    Desktop,
    SignedOut,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Focus {
    Editor,
    Nav,
    Body,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Pick {
    Model,
    Effort,
    Mode,
    Chat,
    /// Importable Claude Code/Codex sessions as (label, id).
    Import(Vec<(String, String)>),
    /// Showcase sources as (label, source id).
    Showcase(Vec<(String, String)>),
    /// Items of a showcase source: (source, items as (label, "kind\tpath")).
    ShowcaseItems(String, Vec<(String, String)>),
    /// Where to install a skill: (source, repository path).
    ShowcaseScope(String, String),
    Runtime(String),
    Policy(String, &'static str),
}

/// A Claude Code style list prompt (`/model`, `/effort`, ...).
pub struct Picker {
    pub title: String,
    /// (label, value)
    pub items: Vec<(String, String)>,
    pub current: String,
    pub sel: usize,
    pub action: Pick,
}

#[derive(Clone, PartialEq, Debug)]
pub enum FieldKind {
    Text,
    Secret,
    Multiline,
    /// (label, value); `Field::value` holds the selected value.
    Choice(Vec<(String, String)>),
}

pub struct Field {
    /// Stable id used when a form is rebuilt or submitted.
    pub key: &'static str,
    pub label: String,
    pub value: String,
    pub kind: FieldKind,
}

#[derive(Clone, Debug)]
pub enum Act {
    Rename,
    DeleteChat(String),
    DeleteProject(String),
    AddProject(Value),
    Instructions(String),
    RateLimit(String),
    Link(String),
    Unlink(String),
    Disconnect(String),
    Connection(Option<Value>),
    OpenUrl,
}

/// A modal form or confirmation drawn as a bordered dialog.
pub struct Form {
    pub title: String,
    pub body: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub caret: usize,
    pub action: Act,
    pub error: String,
}

impl Field {
    pub fn new(key: &'static str, label: &str, value: &str, kind: FieldKind) -> Self {
        Field {
            key,
            label: label.to_owned(),
            value: value.to_owned(),
            kind,
        }
    }
}

impl Form {
    pub fn confirm(title: String, body: String, action: Act) -> Self {
        Form {
            title,
            body,
            fields: vec![],
            focus: 0,
            caret: 0,
            action,
            error: String::new(),
        }
    }
    pub fn new(title: String, fields: Vec<Field>, action: Act) -> Self {
        let caret = fields.first().map_or(0, |f| f.value.len());
        Form {
            title,
            body: String::new(),
            fields,
            focus: 0,
            caret,
            action,
            error: String::new(),
        }
    }
    pub fn value(&self, i: usize) -> &str {
        self.fields.get(i).map_or("", |f| f.value.as_str())
    }
    pub fn get(&self, key: &str) -> &str {
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map_or("", |f| f.value.as_str())
    }
}

/// A frame of a remote browser tab or desktop.
pub struct ScreenFrame {
    pub width: f64,
    pub height: f64,
    pub jpeg: Arc<Vec<u8>>,
    pub serial: u64,
}

/// A live remote browser or desktop session over a WebSocket.
#[derive(Default)]
pub struct Live {
    pub socket: Option<platform::Socket>,
    pub socket_id: u64,
    /// closed | opening | ready | closing
    pub state: &'static str,
    pub tabs: Vec<Value>,
    pub active: String,
    pub frame: Option<ScreenFrame>,
    pub human: bool,
    pub pending: bool,
    pub changing: bool,
    pub history: Vec<Value>,
    pub threads: Vec<Value>,
    pub thread: String,
    pub error: String,
    pub available: bool,
    pub status: Value,
    /// Screen cells of the last drawn image, for mouse mapping.
    pub image: ftui::core::geometry::Rect,
    pub last_activity: f64,
}

/// A pending tool approval, shown as a numbered permission prompt.
#[derive(Clone, Default)]
pub struct Approval {
    pub id: String,
    pub head: String,
    pub detail: String,
    pub sel: usize,
}

pub struct App {
    pub remote: Remote,
    pub tx: Tx,
    pub en: bool,
    pub session: Value,
    pub projects: Vec<Value>,
    pub models: Vec<Value>,
    pub harnesses: Vec<Value>,
    pub chats: Vec<Value>,
    pub bots: Vec<Value>,
    pub connections: Vec<Value>,
    pub project: Value,
    /// pi skills of a project as `(project id, items)`, loaded by `/skills`.
    pub skills: (Value, Vec<Value>),
    pub chat: Value,
    pub model: String,
    pub effort: String,
    /// Permission mode for the next new conversation.
    pub mode: String,
    pub lines: Vec<Message>,
    tools: HashMap<String, usize>,
    pub compact_tools: bool,
    last_text: String,
    pub patches: Vec<String>,
    pub todos: Vec<Value>,
    /// (path, additions, deletions)
    pub changes: Vec<(String, i64, i64)>,
    pub usage: Value,
    pub status: String,
    pub question: String,
    pub choices: Vec<String>,
    pub question_sel: usize,
    retry: Option<(String, Value, String)>,
    pub plan: String,
    pub plan_sel: usize,
    pub goal: String,
    pub approval: Option<Approval>,
    pub scroll: u16,
    pub last_sequence: i64,
    replaying: bool,
    opened_at: f64,
    pub run_started: f64,
    feed: Option<platform::Feed>,
    pub screen: Screen,
    pub focus: Focus,
    pub nav: nav::Nav,
    pub picker: Option<Picker>,
    pub form: Option<Form>,
    pub live: Live,
    pub desktop: bool,
    /// Selected row key on the bot and connection screens.
    pub body_key: String,
    pub link_command: String,
    // Editor
    pub input: String,
    pub caret: usize,
    history: Vec<String>,
    hist_index: usize,
    parked: String,
    pub hide_menu: bool,
    pub selection: usize,
    pasted: Vec<String>,
    pub editor_width: usize,
    // Layout of the last frame, for the mouse.
    pub body_top: u16,
    pub body_left: u16,
    pub body_height: u16,
    pub body_texts: Vec<String>,
    pub sel: Option<(u16, u16)>,
    pub spinner: usize,
    last_spin: f64,
    last_refresh: f64,
    frame_serial: u64,
    socket_serial: u64,
    pub quit: bool,
    pub dirty: bool,
    /// Signed out of a standalone instance: the prompt takes the access token.
    pub token_login: bool,
    start_path: String,
}

impl App {
    pub fn new(remote: Remote, en: bool, tx: Tx) -> Self {
        App {
            remote,
            tx,
            en,
            session: Value::Null,
            projects: vec![],
            models: vec![],
            harnesses: vec![],
            chats: vec![],
            bots: vec![],
            connections: vec![],
            project: Value::Null,
            skills: (Value::Null, vec![]),
            chat: Value::Null,
            model: String::new(),
            effort: "medium".into(),
            mode: "ask".into(),
            lines: vec![],
            tools: HashMap::new(),
            compact_tools: true,
            last_text: String::new(),
            patches: vec![],
            todos: vec![],
            changes: vec![],
            usage: Value::Null,
            status: String::new(),
            question: String::new(),
            choices: vec![],
            question_sel: 0,
            retry: None,
            plan: String::new(),
            plan_sel: 0,
            goal: String::new(),
            approval: None,
            scroll: 0,
            last_sequence: 0,
            replaying: false,
            opened_at: 0.0,
            run_started: 0.0,
            feed: None,
            screen: Screen::Chat,
            focus: Focus::Editor,
            nav: nav::Nav::default(),
            picker: None,
            form: None,
            live: Live {
                state: "closed",
                ..Default::default()
            },
            desktop: false,
            body_key: String::new(),
            link_command: String::new(),
            input: String::new(),
            caret: 0,
            history: vec![],
            hist_index: 0,
            parked: String::new(),
            hide_menu: false,
            selection: 0,
            pasted: vec![],
            editor_width: 80,
            body_top: 0,
            body_left: 0,
            body_height: 0,
            body_texts: vec![],
            sel: None,
            spinner: 0,
            last_spin: 0.0,
            last_refresh: 0.0,
            frame_serial: 0,
            socket_serial: 0,
            quit: false,
            dirty: true,
            token_login: false,
            start_path: String::new(),
        }
    }

    pub fn msg<'a>(&self, it: &'a str, en: &'a str) -> &'a str {
        if self.en { en } else { it }
    }

    pub fn admin(&self) -> bool {
        matches!(
            self.session["role"].as_str(),
            Some("tenant_admin" | "reseller_admin")
        )
    }

    pub fn runtimes(&self) -> Vec<String> {
        self.session["bot_runtimes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }

    /// Skills loaded for the current project, for `/skill:name` completion.
    pub fn skill_list(&self) -> &[Value] {
        if self.skills.0 == self.project["id"] {
            &self.skills.1
        } else {
            &[]
        }
    }

    pub fn note(&mut self, s: impl AsRef<str>) {
        self.record(Role::Notice, s);
    }

    fn record(&mut self, role: Role, s: impl AsRef<str>) {
        self.last_text.clear();
        self.lines.push(Message::new(role, clean(s.as_ref())));
    }

    pub fn working(&self) -> bool {
        matches!(self.chat["state"].as_str(), Some("running" | "waiting")) && self.feed.is_some()
    }

    pub fn harness_of(&self, id: &Value) -> Option<&Value> {
        self.harnesses.iter().find(|h| &h["id"] == id)
    }

    /// Harness serving the open conversation or the selected project.
    pub fn current_harness(&self) -> Option<&Value> {
        let id = if self.chat["harness_id"].is_string() {
            &self.chat["harness_id"]
        } else {
            &self.project["harness_id"]
        };
        self.harness_of(id)
    }

    pub fn project_label(project: &Value) -> String {
        let (root, path) = (string(project, "root"), string(project, "path"));
        if path == "." || path.is_empty() {
            root
        } else {
            format!("{root}/{path}")
        }
    }

    fn notify_bell(&mut self) {
        if self.replaying {
            if platform::now_ms() - self.opened_at < REPLAY_BELL_GRACE_MS {
                return;
            }
            self.replaying = false;
        }
        platform::bell();
    }

    // ----- Inventory -------------------------------------------------------

    /// Load the session; a 401 shows the sign-in screen.
    pub async fn start(&mut self, cx: &Cx, path: &str) -> Result<()> {
        self.start_path = path.to_owned();
        match self.remote.api(cx, Method::Get, "/session", None).await {
            Ok(session) => self.session = session,
            Err(e) if e.to_string().starts_with("401") || e.to_string().starts_with("HTTP 401") => {
                self.token_login = platform::token_login(cx).await;
                self.screen = Screen::SignedOut;
                self.focus = Focus::Editor;
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        self.refresh(cx).await?;
        self.model = self
            .models
            .iter()
            .find(|m| m["default"] == true)
            .or(self.models.first())
            .map(|v| string(v, "id"))
            .unwrap_or_default();
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let mut parts = route.trim_start_matches('/').splitn(2, '/');
        let (head, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        match head {
            "c" if is_uuid(id) => self.open(cx, id).await?,
            "p" => {
                if let Some(p) = self.projects.iter().find(|p| p["id"] == id).cloned() {
                    self.select_project(&p);
                }
            }
            "bots" if is_uuid(id) => self.show(Screen::Bot(id.to_owned())),
            "connections" => {
                self.show(Screen::Connections);
                if query.contains("error=oauth_cancelled") {
                    self.status = self
                        .msg(
                            "Autorizzazione Slack annullata.",
                            "Slack authorization cancelled.",
                        )
                        .into();
                }
            }
            "browser" => self.show(Screen::Browser),
            "computer" => self.show(Screen::Desktop),
            _ => {
                if let [project] = self.projects.as_slice() {
                    let project = project.clone();
                    self.select_project(&project);
                } else if self.projects.is_empty() {
                    let key = self
                        .harnesses
                        .first()
                        .map_or("s:harnesses".to_owned(), |h| {
                            format!("h:{}", string(h, "id"))
                        });
                    self.nav.reveal(key);
                    self.focus = Focus::Nav;
                } else {
                    self.nav.reveal("s:projects".into());
                    self.focus = Focus::Nav;
                }
            }
        }
        Ok(())
    }

    pub async fn refresh(&mut self, cx: &Cx) -> Result<()> {
        self.chats = self.remote.list(cx, "/conversations").await?;
        self.projects = self.remote.list(cx, "/projects").await?;
        self.models = self.remote.list(cx, "/models").await?;
        self.harnesses = self.remote.list(cx, "/harnesses").await?;
        // Optional inventories: members without access see empty sections.
        self.bots = self.remote.list(cx, "/bots").await.unwrap_or_default();
        self.connections = self
            .remote
            .list(cx, "/connections")
            .await
            .unwrap_or_default();
        match self
            .remote
            .api(cx, Method::Get, "/browser-agent", None)
            .await
        {
            Ok(v) => {
                self.live.available = true;
                self.live.threads = v["threads"].as_array().cloned().unwrap_or_default();
            }
            Err(_) => self.live.available = false,
        }
        self.desktop = self.runtimes().iter().any(|r| r == "computer");
        if self.desktop {
            self.live.status = self
                .remote
                .api(cx, Method::Get, "/computer", None)
                .await
                .unwrap_or(Value::Null);
        }
        if let Some(fresh) = self.chats.iter().find(|c| c["id"] == self.chat["id"]) {
            for key in ["title", "permission_mode", "model", "effort", "revision"] {
                self.chat[key] = fresh[key].clone();
            }
        }
        self.last_refresh = platform::now_ms();
        Ok(())
    }

    pub fn project_chats(&self) -> Vec<Value> {
        self.chats
            .iter()
            .filter(|c| self.project.is_null() || c["project_id"] == self.project["id"])
            .cloned()
            .collect()
    }

    /// Switch the center pane and keep the URL in step.
    pub fn show(&mut self, screen: Screen) {
        let path = match &screen {
            Screen::Bot(id) => format!("/bots/{id}"),
            Screen::Connections => "/connections".into(),
            Screen::Browser => "/browser".into(),
            Screen::Desktop => "/computer".into(),
            _ => self.chat_path(),
        };
        platform::set_location(&path);
        // The browser keeps the prompt for the assistant; Tab reaches the screen.
        self.focus = if matches!(screen, Screen::Chat | Screen::SignedOut | Screen::Browser) {
            Focus::Editor
        } else {
            Focus::Body
        };
        self.body_key.clear();
        self.link_command.clear();
        self.screen = screen;
        self.dirty = true;
    }

    fn chat_path(&self) -> String {
        if let Some(id) = self.chat["id"].as_str() {
            format!("/c/{id}")
        } else if let Some(id) = self.project["id"].as_str() {
            format!("/p/{id}")
        } else {
            "/".into()
        }
    }

    // ----- Conversation lifecycle ------------------------------------------

    pub fn close(&mut self) {
        if let Some(f) = self.feed.take() {
            f.stop();
        }
        self.chat = Value::Null;
        self.lines.clear();
        self.tools.clear();
        view::clear_markdown_cache();
        self.patches.clear();
        self.question.clear();
        self.choices.clear();
        self.retry = None;
        self.plan.clear();
        self.goal.clear();
        self.approval = None;
        self.todos.clear();
        self.changes.clear();
        self.usage = Value::Null;
        self.last_text.clear();
        self.scroll = 0;
        self.last_sequence = 0;
        self.replaying = false;
    }

    pub fn select_project(&mut self, project: &Value) {
        if self.project["id"] != project["id"] || !self.chat.is_null() {
            self.close();
            self.project = project.clone();
        }
        self.show(Screen::Chat);
    }

    pub async fn open(&mut self, cx: &Cx, id: &str) -> Result<()> {
        ensure!(is_uuid(id), "ID non valido / Invalid ID");
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
        self.model = string(&c, "model");
        self.effort = string(&c, "effort");
        self.question = string(&c, "question_id");
        self.plan = string(&c, "plan_id");
        self.goal = string(&c, "goal_id");
        self.nav.track(&c);
        self.chat = c;
        self.replaying = true;
        self.opened_at = platform::now_ms();
        self.feed = platform::follow(cx, &self.remote, id, 0, self.en, self.tx.clone());
        self.show(Screen::Chat);
        Ok(())
    }

    async fn new_chat(&mut self, cx: &Cx, title: &str, mode: &str) -> Result<()> {
        ensure!(
            !self.project.is_null(),
            "F2 · Scegli un progetto / Choose a project"
        );
        let mut body = json!({"project_id": self.project["id"], "permission_mode": mode});
        if !self.model.is_empty() {
            body["model"] = json!(self.model);
        }
        // Without this the effort picked before the first message was dropped.
        if !self.effort.is_empty() {
            body["effort"] = json!(self.effort);
        }
        if !title.is_empty() {
            body["title"] = json!(title);
        }
        let chat = self
            .remote
            .api(cx, Method::Post, "/conversations", Some(body))
            .await?;
        self.chats.insert(0, chat.clone());
        self.activate(cx, chat)
    }

    fn path(&self) -> Result<String> {
        Ok(format!(
            "/conversations/{}",
            self.chat["id"].as_str().context(self.msg(
                "Nessuna conversazione: scrivi il primo messaggio.",
                "No conversation: write your first message."
            ))?
        ))
    }

    pub async fn send(
        &mut self,
        cx: &Cx,
        text: &str,
        intent: &str,
        reference: Option<&str>,
    ) -> Result<()> {
        ensure!(!text.trim().is_empty(), "Messaggio vuoto / Empty message");
        if self.chat.is_null() && matches!(intent, "message" | "plan" | "goal") {
            let mode = if intent == "plan" {
                "plan".to_owned()
            } else {
                self.mode.clone()
            };
            self.new_chat(cx, "", &mode).await?;
        }
        if let Some(h) = self.current_harness() {
            ensure!(
                h["online"] == true || h["managed"] == true,
                "{} {}",
                string(h, "name"),
                self.msg(
                    "è offline: avvia venticode-harness per continuare.",
                    "is offline: start venticode-harness to continue."
                )
            );
        }
        let mut req = json!({"content": text});
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
            .map(|t| t.2.clone())
            .unwrap_or_else(platform::new_id);
        self.retry = Some((path.clone(), req.clone(), id.clone()));
        req["id"] = json!(id);
        self.replaying = false;
        self.remote.api(cx, Method::Post, &path, Some(req)).await?;
        self.retry = None;
        Ok(())
    }

    async fn patch_chat(&mut self, cx: &Cx, mut patch: Value) -> Result<()> {
        let path = self.path()?;
        // Turns advance `revision` server-side without an SSE event.
        let latest = self.remote.api(cx, Method::Get, &path, None).await?;
        patch["revision"] = latest["revision"].clone();
        let updated = self
            .remote
            .api(cx, Method::Patch, &path, Some(patch))
            .await?;
        for key in [
            "title",
            "permission_mode",
            "model",
            "effort",
            "revision",
            "plan_id",
        ] {
            self.chat[key] = updated[key].clone();
        }
        if let Some(c) = self.chats.iter_mut().find(|c| c["id"] == updated["id"]) {
            *c = updated;
        }
        Ok(())
    }

    pub async fn set_mode(&mut self, cx: &Cx, mode: &str) -> Result<()> {
        if self.chat.is_null() {
            self.mode = mode.to_owned();
        } else {
            self.patch_chat(cx, json!({"permission_mode": mode}))
                .await?;
        }
        Ok(())
    }

    pub fn current_mode(&self) -> String {
        if self.chat.is_null() {
            self.mode.clone()
        } else {
            string(&self.chat, "permission_mode")
        }
    }

    // ----- Transcript ------------------------------------------------------

    fn tool(&mut self, call_id: &str) -> Option<&mut Message> {
        let index = *self.tools.get(call_id)?;
        self.lines.get_mut(index)
    }

    pub fn event(&mut self, e: Value) {
        if let Some(seq) = e["sequence"].as_i64() {
            if seq <= self.last_sequence {
                return;
            }
            self.last_sequence = seq;
        }
        let val = |k| string(&e, k);
        let rows_before: usize = self
            .lines
            .last()
            .map_or(0, |m| m.text.matches('\n').count());
        let count_before = self.lines.len();
        let last_assistant = |lines: &[Message], text: &str| {
            lines
                .last()
                .is_some_and(|l| l.role == Role::Assistant && l.text.trim() == text.trim())
        };
        match e["type"].as_str() {
            Some("text") => {
                let id = val("message_id");
                if self.last_text != id
                    || !self.lines.last().is_some_and(|l| l.role == Role::Assistant)
                {
                    self.lines.push(Message::new(Role::Assistant, ""));
                    self.last_text = id;
                }
                if let Some(last) = self.lines.last_mut() {
                    last.text.push_str(&clean(&val("delta")));
                }
            }
            Some("reasoning") => {
                if !self.lines.last().is_some_and(|l| l.role == Role::Thinking) {
                    self.lines.push(Message::new(Role::Thinking, ""));
                }
                if let Some(last) = self.lines.last_mut() {
                    last.text.push_str(&clean(&val("delta")));
                }
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
                self.record(Role::User, val("content"));
            }
            Some("question") => {
                self.question = val("reference_id");
                self.question_sel = 0;
                self.choices = e["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let content = val("content");
                if !last_assistant(&self.lines, &content) {
                    self.record(Role::Assistant, content);
                }
                self.notify_bell();
            }
            Some("plan") => {
                self.plan = val("reference_id");
                self.plan_sel = 0;
                let content = val("content");
                if !last_assistant(&self.lines, &content) {
                    self.record(Role::Assistant, content);
                }
                self.notify_bell();
            }
            Some(kind @ ("goal_completed" | "goal_cancelled")) => {
                self.goal.clear();
                if kind == "goal_cancelled" {
                    self.question.clear();
                    self.plan.clear();
                    self.choices.clear();
                }
                let content = val("content");
                self.note(format!(
                    "✓ {}",
                    if content.is_empty() {
                        self.msg("Obiettivo annullato", "Goal cancelled")
                    } else {
                        &content
                    }
                ));
                self.notify_bell();
            }
            Some("tool_call") => {
                let mut m = Message::new(Role::Tool, "");
                m.head = clean(&tool_head(&val("operation"), &val("target")));
                m.status = "running".into();
                let call = val("call_id");
                if !call.is_empty() {
                    self.tools.insert(call, self.lines.len());
                }
                self.last_text.clear();
                self.lines.push(m);
            }
            Some("tool_output") => {
                let data = clean(&val("data"));
                let interactive = val("interactive");
                if let Some(tool) = self.tool(&val("call_id")) {
                    if !interactive.is_empty() {
                        tool.interactive = interactive;
                    }
                    append_capped_output(&mut tool.text, &data, MAX_OUTPUT_BYTES);
                }
            }
            Some("tool_result") => {
                let status = e["status"].as_str().unwrap_or("ok").to_owned();
                let summary = clean(&val("summary"));
                let patch = e["diff"]["patch"].as_str().unwrap_or("").to_owned();
                let path = string(&e["diff"], "path");
                if let Some(tool) = self.tool(&val("call_id")) {
                    tool.status = status.clone();
                    tool.note = summary;
                    tool.diff = clean(&patch);
                }
                if !patch.is_empty() {
                    if self.patches.len() >= MAX_PATCHES {
                        self.patches.remove(0);
                    }
                    self.patches.push(clean(&format!("{path}\n{patch}")));
                }
                if status == "ok" && !path.is_empty() {
                    let (add, del) = (
                        e["diff"]["additions"].as_i64().unwrap_or(0),
                        e["diff"]["deletions"].as_i64().unwrap_or(0),
                    );
                    match self.changes.iter_mut().find(|c| c.0 == path) {
                        Some(entry) => {
                            entry.1 += add;
                            entry.2 += del;
                        }
                        None => self.changes.push((path, add, del)),
                    }
                }
            }
            Some("approval") => {
                let call = val("call_id");
                if e["approval_state"] == "pending" {
                    let head = self.tool(&call).map(|t| t.head.clone()).unwrap_or_default();
                    if let Some(tool) = self.tool(&call) {
                        tool.status = "waiting".into();
                    }
                    self.approval = Some(Approval {
                        id: val("approval_id"),
                        head,
                        detail: clean(&val("summary")),
                        sel: 0,
                    });
                    self.notify_bell();
                } else {
                    let state = val("approval_state");
                    if let Some(tool) = self.tool(&call)
                        && tool.status == "waiting"
                    {
                        tool.status = match state.as_str() {
                            "denied" => "denied",
                            "expired" => "cancelled",
                            _ => "running",
                        }
                        .into();
                    }
                    if self
                        .approval
                        .as_ref()
                        .is_some_and(|a| a.id == val("approval_id"))
                    {
                        self.approval = None;
                    }
                }
            }
            Some("todo") => {
                if let Some(v) = e["todos"].as_array() {
                    self.todos = v.clone();
                    let text = v
                        .iter()
                        .map(|x| format!("{}\t{}", string(x, "status"), string(x, "content")))
                        .collect::<Vec<_>>()
                        .join("\n");
                    match self.lines.last_mut() {
                        Some(last) if last.role == Role::Todo => last.text = clean(&text),
                        _ => self.record(Role::Todo, text),
                    }
                }
            }
            Some("run") => {
                let was_running =
                    matches!(self.chat["state"].as_str(), Some("running" | "waiting"));
                self.chat["state"] = e["state"].clone();
                if !e["usage"].is_null() {
                    self.usage = e["usage"].clone();
                }
                let running = matches!(e["state"].as_str(), Some("running" | "waiting"));
                if running && !was_running {
                    self.run_started = platform::now_ms();
                }
                if !running {
                    self.approval = None;
                    for line in &mut self.lines {
                        if line.role == Role::Tool
                            && matches!(line.status.as_str(), "running" | "waiting")
                        {
                            line.status = "cancelled".into();
                        }
                    }
                    if was_running {
                        self.notify_bell();
                    }
                }
                if let Some(c) = self.chats.iter_mut().find(|c| c["id"] == self.chat["id"]) {
                    c["state"] = e["state"].clone();
                }
            }
            Some("error") => self.record(Role::Error, val("message")),
            _ => {}
        }
        // Keep the viewport anchored while the user reads older output.
        if self.scroll > 0 {
            let added = if self.lines.len() > count_before {
                (self.lines.len() - count_before) * 2
            } else {
                self.lines
                    .last()
                    .map_or(0, |m| m.text.matches('\n').count())
                    .saturating_sub(rows_before)
            };
            self.scroll = self.scroll.saturating_add(added as u16);
        }
        self.dirty = true;
    }

    /// The running interactive tool session, if any: (call id, kind).
    pub fn interactive(&self) -> Option<(String, String)> {
        self.tools.iter().find_map(|(call, &i)| {
            let m = self.lines.get(i)?;
            (m.status == "running" && !m.interactive.is_empty())
                .then(|| (call.clone(), m.interactive.clone()))
        })
    }

    // ----- Platform updates ------------------------------------------------

    pub fn update(&mut self, update: Update) {
        self.dirty = true;
        match update {
            Update::Stream(id, u) if self.chat["id"] == id => match u {
                StreamUpdate::Event(event) => self.event(event),
                StreamUpdate::Status(msg) => self.status = msg,
                StreamUpdate::AuthExpired(msg) => {
                    self.feed = None;
                    self.status = msg;
                    self.screen = Screen::SignedOut;
                }
                StreamUpdate::Fatal(error) => {
                    self.feed = None;
                    self.status = error;
                }
            },
            Update::Socket(id, ev) if self.live.socket_id == id => screens::socket_event(self, ev),
            _ => {}
        }
    }

    /// Animations, the replay grace period and a periodic inventory refresh.
    pub async fn tick(&mut self, cx: &Cx) {
        let now = platform::now_ms();
        if self.working() && now - self.last_spin >= 150.0 {
            self.spinner = (self.spinner + 1) % STARS.len();
            self.last_spin = now;
            self.dirty = true;
        }
        if self.replaying && now - self.opened_at >= REPLAY_BELL_GRACE_MS {
            self.replaying = false;
        }
        if self.screen != Screen::SignedOut
            && self.form.is_none()
            && now - self.last_refresh >= REFRESH_EVERY_MS
        {
            self.last_refresh = now;
            if self.refresh(cx).await.is_ok() {
                self.dirty = true;
            }
        }
        screens::live_tick(self, now);
    }

    // ----- Input -----------------------------------------------------------

    pub async fn handle(&mut self, cx: &Cx, event: Event) {
        self.dirty = true;
        let result = match event {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.key(cx, k).await,
            Event::Paste(p) => {
                self.paste(&p.text);
                Ok(())
            }
            Event::Mouse(m) => self.mouse(cx, m).await,
            _ => Ok(()),
        };
        if let Err(e) = result {
            match self.form.as_mut() {
                Some(form) => form.error = e.to_string(),
                None => self.status = e.to_string(),
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.live.human && self.focus == Focus::Body {
            screens::live_input(self, json!({"kind": "text", "text": text}));
            return;
        }
        if let Some(form) = self.form.as_mut() {
            if let Some(field) = form.fields.get_mut(form.focus)
                && !matches!(field.kind, FieldKind::Choice(_))
            {
                let text = clean(text);
                let text = if field.kind == FieldKind::Multiline {
                    text
                } else {
                    text.replace('\n', " ")
                };
                field.value.insert_str(form.caret, &text);
                form.caret += text.len();
            }
            return;
        }
        if self.picker.is_some() || self.focus != Focus::Editor {
            return;
        }
        insert_paste(&mut self.input, &mut self.caret, text, &mut self.pasted);
        self.hide_menu = false;
    }

    async fn key(&mut self, cx: &Cx, k: KeyEvent) -> Result<()> {
        let ctrl = k.modifiers.contains(Modifiers::CTRL);
        if ctrl && k.code == KeyCode::Char('c') && !(self.live.human && self.focus == Focus::Body) {
            // Claude Code: Ctrl+C clears the prompt, a second press quits.
            if !self.input.is_empty() {
                self.input.clear();
                self.caret = 0;
                self.pasted.clear();
            } else if self.form.take().is_none() && self.picker.take().is_none() {
                self.quit = platform::CAN_QUIT;
            }
            return Ok(());
        }
        if self.screen == Screen::SignedOut {
            return self.signed_out_key(cx, k).await;
        }
        if self.live.human && self.focus == Focus::Body {
            return screens::live_key(self, cx, k).await;
        }
        if self.form.is_some() {
            return screens::form_key(self, cx, k).await;
        }
        if self.picker.is_some() {
            return self.picker_key(cx, k).await;
        }
        match k.code {
            KeyCode::F(2) => {
                self.refresh(cx).await?;
                self.nav.reveal(nav::project_key(self));
                self.focus = Focus::Nav;
                return Ok(());
            }
            KeyCode::F(3) => {
                self.refresh(cx).await?;
                self.nav.reveal("s:harnesses".into());
                self.focus = Focus::Nav;
                return Ok(());
            }
            KeyCode::F(4) => {
                nav::toggle(self, false);
                self.nav.inspector_hidden = self.nav.hidden;
                return Ok(());
            }
            // Ctrl+B / Ctrl+Alt+B toggle the side panes, as in VS Code.
            KeyCode::Char('b') if ctrl => {
                nav::toggle(self, k.modifiers.contains(Modifiers::ALT));
                return Ok(());
            }
            KeyCode::Char('l') if ctrl => return self.open_picker(Pick::Model),
            KeyCode::Char('o') if ctrl => {
                self.compact_tools = !self.compact_tools;
                return Ok(());
            }
            KeyCode::BackTab => {
                // Shift+Tab cycles the permission mode, as in Claude Code.
                let next = match self.current_mode().as_str() {
                    "ask" => "auto_edit",
                    "auto_edit" => "plan",
                    _ => "ask",
                };
                return self.set_mode(cx, next).await;
            }
            _ => {}
        }
        let suggestions = slash_suggestions(&self.input, self.hide_menu, self.skill_list()).len();
        if k.code == KeyCode::Tab && !(self.focus == Focus::Editor && suggestions > 0) {
            self.cycle_focus();
            return Ok(());
        }
        match self.focus {
            Focus::Nav => return self.nav_key(cx, k).await,
            Focus::Body => return screens::body_key(self, cx, k).await,
            Focus::Editor => {}
        }
        if self.approval.is_some() && self.input.is_empty() {
            return self.approval_key(cx, k).await;
        }
        if self.input.is_empty() && self.prompt_key(cx, k).await? {
            return Ok(());
        }
        self.editor_key(cx, k).await
    }

    /// Sign-in screen: type or paste the access token (standalone), or press
    /// Enter to sign in through the portal.
    async fn signed_out_key(&mut self, cx: &Cx, k: KeyEvent) -> Result<()> {
        let ctrl = k.modifiers.contains(Modifiers::CTRL);
        if !self.token_login {
            match k.code {
                KeyCode::Enter if platform::CAN_QUIT => self.quit = true,
                KeyCode::Enter => platform::sign_in(false),
                KeyCode::Char('d') if platform::dev() => platform::sign_in(true),
                _ => {}
            }
            return Ok(());
        }
        match k.code {
            KeyCode::Enter => {
                let token = self.input.trim().to_owned();
                ensure!(
                    !token.is_empty(),
                    self.msg("Incolla il token di accesso", "Paste the access token")
                );
                if platform::login(cx, &token).await? {
                    self.input.clear();
                    self.caret = 0;
                    self.status.clear();
                    self.token_login = false;
                    self.screen = Screen::Chat;
                    let path = self.start_path.clone();
                    self.start(cx, &path).await?;
                } else {
                    self.status = self
                        .msg("Token di accesso non valido", "Invalid access token")
                        .into();
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(Modifiers::ALT) => {
                self.input.push(c)
            }
            _ => {}
        }
        self.caret = self.input.len();
        Ok(())
    }

    fn cycle_focus(&mut self) {
        let body = !matches!(self.screen, Screen::Chat | Screen::SignedOut);
        self.focus = match self.focus {
            Focus::Editor => {
                self.nav.hidden = false;
                Focus::Nav
            }
            Focus::Nav if body => Focus::Body,
            _ => Focus::Editor,
        };
    }

    /// Numbered choices for a pending question or a finished plan.
    async fn prompt_key(&mut self, cx: &Cx, k: KeyEvent) -> Result<bool> {
        if !self.question.is_empty() && !self.choices.is_empty() {
            let n = self.choices.len();
            match k.code {
                KeyCode::Up => self.question_sel = self.question_sel.saturating_sub(1),
                KeyCode::Down => self.question_sel = (self.question_sel + 1).min(n - 1),
                KeyCode::Enter => {
                    let answer = self.choices[self.question_sel].clone();
                    let question = self.question.clone();
                    self.send(cx, &answer, "answer", Some(&question)).await?;
                }
                KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                    let answer = self.choices[c as usize - '1' as usize].clone();
                    let question = self.question.clone();
                    self.send(cx, &answer, "answer", Some(&question)).await?;
                }
                _ => return Ok(false),
            }
            return Ok(true);
        }
        if !self.plan.is_empty() && !self.working() {
            match k.code {
                KeyCode::Up | KeyCode::Down => self.plan_sel ^= 1,
                KeyCode::Enter | KeyCode::Char('1')
                    if k.code != KeyCode::Enter || self.plan_sel == 0 =>
                {
                    self.command(cx, "/build").await?;
                }
                KeyCode::Enter | KeyCode::Char('2') => {
                    // Keep planning: the next message refines the plan.
                    self.plan.clear();
                }
                _ => return Ok(false),
            }
            return Ok(true);
        }
        Ok(false)
    }

    async fn approval_key(&mut self, cx: &Cx, k: KeyEvent) -> Result<()> {
        let Some(approval) = self.approval.as_mut() else {
            return Ok(());
        };
        let decision = match k.code {
            KeyCode::Up => {
                approval.sel = approval.sel.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                approval.sel = (approval.sel + 1).min(2);
                return Ok(());
            }
            KeyCode::Char('1') => 0,
            KeyCode::Char('2') => 1,
            KeyCode::Char('3') | KeyCode::Escape => 2,
            KeyCode::Enter => approval.sel,
            // Typing starts a message instead, as in Claude Code.
            _ => return self.editor_key(cx, k).await,
        };
        let id = approval.id.clone();
        let decision = ["allow_once", "allow_project", "deny"][decision];
        self.remote
            .api(
                cx,
                Method::Post,
                &format!("/approvals/{id}"),
                Some(json!({"decision": decision})),
            )
            .await?;
        self.approval = None;
        Ok(())
    }

    async fn editor_key(&mut self, cx: &Cx, k: KeyEvent) -> Result<()> {
        let m = k.modifiers;
        let ctrl = m.contains(Modifiers::CTRL);
        let alt = m.contains(Modifiers::ALT);
        let suggestions = slash_suggestions(&self.input, self.hide_menu, self.skill_list());
        let count = suggestions.len();
        self.selection = self.selection.min(count.saturating_sub(1));
        let width = self.editor_width.max(1);
        match k.code {
            KeyCode::Char('a') if ctrl => {
                self.caret = self.input[..self.caret].rfind('\n').map_or(0, |n| n + 1);
            }
            KeyCode::Char('e') if ctrl => {
                self.caret += self.input[self.caret..]
                    .find('\n')
                    .unwrap_or(self.input.len() - self.caret);
            }
            KeyCode::Char('u') if ctrl => {
                let start = self.input[..self.caret].rfind('\n').map_or(0, |n| n + 1);
                self.input.drain(start..self.caret);
                self.caret = start;
            }
            KeyCode::Char('k') if ctrl => {
                let end = self.caret
                    + self.input[self.caret..]
                        .find('\n')
                        .unwrap_or(self.input.len() - self.caret);
                self.input.drain(self.caret..end);
            }
            KeyCode::Char('w') if ctrl => delete_prev_word(&mut self.input, &mut self.caret),
            KeyCode::Char('j') if ctrl => {
                self.input.insert(self.caret, '\n');
                self.caret += 1;
            }
            KeyCode::Char('v') if ctrl => {
                if let Some(text) = platform::read_clipboard() {
                    insert_paste(&mut self.input, &mut self.caret, &text, &mut self.pasted);
                }
            }
            KeyCode::Escape if count > 0 => self.hide_menu = true,
            KeyCode::Escape if self.working() => self.command(cx, "/stop").await.map(|_| ())?,
            KeyCode::Escape if !self.input.is_empty() => {
                // Esc clears the prompt (Claude Code: double Esc).
                self.input.clear();
                self.caret = 0;
                self.pasted.clear();
            }
            KeyCode::Up if alt => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down if alt => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up if count > 0 => self.selection = self.selection.saturating_sub(1),
            KeyCode::Down if count > 0 => self.selection = (self.selection + 1).min(count - 1),
            KeyCode::Tab if count > 0 => {
                self.input = format!("{} ", suggestions[self.selection].0);
                self.caret = self.input.len();
                self.selection = 0;
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(12),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(12),
            KeyCode::Up | KeyCode::Down
                if let Some(next) =
                    editor::vertical(&self.input, self.caret, width, k.code == KeyCode::Down) =>
            {
                self.caret = next;
            }
            KeyCode::Up => {
                if self.hist_index > 0 {
                    if self.hist_index == self.history.len() {
                        self.parked = self.input.clone();
                    }
                    self.hist_index -= 1;
                    self.input = self.history[self.hist_index].clone();
                    self.caret = self.input.len();
                }
            }
            KeyCode::Down if self.hist_index < self.history.len() => {
                self.hist_index += 1;
                self.input = self
                    .history
                    .get(self.hist_index)
                    .cloned()
                    .unwrap_or_else(|| self.parked.clone());
                self.caret = self.input.len();
            }
            KeyCode::Left => {
                self.caret = self.input[..self.caret]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(pos, _)| pos);
            }
            KeyCode::Right => {
                self.caret += self.input[self.caret..]
                    .graphemes(true)
                    .next()
                    .map_or(0, str::len);
            }
            KeyCode::Home => {
                self.caret = self.input[..self.caret].rfind('\n').map_or(0, |n| n + 1);
            }
            KeyCode::End if self.scroll > 0 && self.caret == self.input.len() => self.scroll = 0,
            KeyCode::End => {
                self.caret += self.input[self.caret..]
                    .find('\n')
                    .unwrap_or(self.input.len() - self.caret);
            }
            KeyCode::Delete if self.caret < self.input.len() => {
                let n = self.input[self.caret..]
                    .graphemes(true)
                    .next()
                    .map_or(0, str::len);
                self.input.drain(self.caret..self.caret + n);
            }
            KeyCode::Backspace if self.caret > 0 => {
                let prev = self.input[..self.caret]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(i, _)| i);
                self.input.drain(prev..self.caret);
                self.caret = prev;
            }
            KeyCode::Enter if m.intersects(Modifiers::SHIFT | Modifiers::ALT) => {
                self.input.insert(self.caret, '\n');
                self.caret += 1;
            }
            KeyCode::Enter
                if self.input.trim().is_empty()
                    && matches!(self.screen, Screen::Browser | Screen::Desktop)
                    && self.live.state == "closed" =>
            {
                screens::live_start(self, cx)?;
            }
            KeyCode::Enter => return self.submit(cx, count).await,
            KeyCode::Char('?') if self.input.is_empty() => {
                return self.command(cx, "/help").await.map(|_| ());
            }
            KeyCode::Char(c)
                if !m.intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER) =>
            {
                self.selection = 0;
                self.input.insert(self.caret, c);
                self.caret += c.len_utf8();
            }
            _ => return Ok(()),
        }
        self.hide_menu = false;
        Ok(())
    }

    async fn submit(&mut self, cx: &Cx, suggestions: usize) -> Result<()> {
        let line = self.input.trim().to_owned();
        if line.is_empty() {
            return Ok(());
        }
        if needs_tab_completion(&line, suggestions, self.skill_list()) {
            self.input = format!(
                "{} ",
                slash_suggestions(&line, false, self.skill_list())
                    [self.selection.min(suggestions - 1)]
                .0
            );
            self.caret = self.input.len();
            return Ok(());
        }
        if line == "/paste" {
            self.input.clear();
            self.caret = 0;
            self.pasted.clear();
            match platform::read_clipboard() {
                Some(text) => {
                    insert_paste(&mut self.input, &mut self.caret, &text, &mut self.pasted)
                }
                None => bail!(self.msg(
                    "Impossibile leggere gli appunti",
                    "Could not read clipboard"
                )),
            }
            return Ok(());
        }
        self.status.clear();
        // Placeholders expand for commands too (/plan, /goal, /answer).
        let line = expand_pasted(&line, &self.pasted);
        if self.screen == Screen::Browser && !line.starts_with('/') {
            screens::live_ask(self, &line)?;
        } else {
            self.command(cx, &line).await?;
        }
        self.history.push(line);
        if self.history.len() > MAX_DRAFT_HISTORY {
            self.history.remove(0);
        }
        self.hist_index = self.history.len();
        self.input.clear();
        self.parked.clear();
        self.scroll = 0;
        self.caret = 0;
        self.pasted.clear();
        Ok(())
    }

    // ----- Pickers ---------------------------------------------------------

    pub fn open_picker(&mut self, action: Pick) -> Result<()> {
        let it = !self.en;
        let pair = |a: &str, b: &str, v: &str| ((if it { a } else { b }).to_owned(), v.to_owned());
        let (title, items, current) = match &action {
            Pick::Model => (
                self.msg("Scegli il modello", "Select model"),
                self.models
                    .iter()
                    .map(|m| (clean(m["name"].as_str().unwrap_or("?")), string(m, "id")))
                    .collect::<Vec<_>>(),
                self.model.clone(),
            ),
            Pick::Effort => (
                self.msg("Sforzo di ragionamento", "Reasoning effort"),
                vec![
                    pair("Nessuno", "None", "none"),
                    pair("Basso", "Low", "low"),
                    pair("Medio", "Medium", "medium"),
                    pair("Alto", "High", "high"),
                ],
                self.effort.clone(),
            ),
            Pick::Mode => (
                self.msg("Modalità dei permessi", "Permission mode"),
                vec![
                    pair("Chiedi prima di modificare", "Ask before edits", "ask"),
                    pair("Accetta modifiche", "Accept edits", "auto_edit"),
                    pair("Solo piano", "Plan only", "plan"),
                ],
                self.current_mode(),
            ),
            Pick::Chat => (
                self.msg("Riprendi una conversazione", "Resume a conversation"),
                self.project_chats()
                    .iter()
                    .map(|c| (clean(&string(c, "title")), string(c, "id")))
                    .collect(),
                string(&self.chat, "id"),
            ),
            Pick::Import(items) => (
                self.msg("Importa una sessione", "Import a session"),
                items.clone(),
                String::new(),
            ),
            Pick::Showcase(items) => (
                self.msg(
                    "Showcase · scegli una sorgente (/showcase owner/repo per altre)",
                    "Showcase · choose a source (/showcase owner/repo for others)",
                ),
                items.clone(),
                String::new(),
            ),
            Pick::ShowcaseItems(_, items) => (
                self.msg("Installa · ↑/↓ PgUp/PgDn", "Install · ↑/↓ PgUp/PgDn"),
                items.clone(),
                String::new(),
            ),
            Pick::ShowcaseScope(..) => (
                self.msg("Dove installare la skill?", "Where should the skill go?"),
                vec![
                    pair(
                        "Questo progetto (.pi/skills)",
                        "This project (.pi/skills)",
                        "project",
                    ),
                    pair(
                        "Tutti i progetti di questo harness (~/.pi/agent/skills)",
                        "Every project on this harness (~/.pi/agent/skills)",
                        "user",
                    ),
                ],
                String::new(),
            ),
            Pick::Runtime(bot) => {
                let bot = self
                    .bots
                    .iter()
                    .find(|b| &b["id"] == bot)
                    .cloned()
                    .unwrap_or_default();
                (
                    self.msg("Ambiente del bot", "Bot environment"),
                    self.runtimes()
                        .into_iter()
                        .filter(|r| r != "project" || bot["harness_id"].is_string())
                        .map(|r| (screens::runtime_label(self, &r), r))
                        .collect(),
                    string(&bot, "runtime"),
                )
            }
            Pick::Policy(bot, key) => {
                let bot = self
                    .bots
                    .iter()
                    .find(|b| &b["id"] == bot)
                    .cloned()
                    .unwrap_or_default();
                (
                    self.msg("Criterio per lo strumento", "Tool policy"),
                    vec![
                        pair("Automatico", "Automatic", "auto"),
                        pair("Con revisione", "With review", "review"),
                        pair("Disattivato", "Off", "off"),
                    ],
                    string(&bot["tools"], key),
                )
            }
        };
        ensure!(
            !items.is_empty(),
            self.msg("Niente da scegliere", "Nothing to choose")
        );
        let sel = items.iter().position(|i| i.1 == current).unwrap_or(0);
        self.picker = Some(Picker {
            title: title.to_owned(),
            items,
            current,
            sel,
            action,
        });
        Ok(())
    }

    async fn picker_key(&mut self, cx: &Cx, k: KeyEvent) -> Result<()> {
        let Some(p) = self.picker.as_mut() else {
            return Ok(());
        };
        let n = p.items.len();
        let chosen = match k.code {
            KeyCode::Escape => {
                self.picker = None;
                return Ok(());
            }
            KeyCode::Up => {
                p.sel = p.sel.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                p.sel = (p.sel + 1).min(n.saturating_sub(1));
                return Ok(());
            }
            KeyCode::PageUp => {
                p.sel = p.sel.saturating_sub(PICKER_ROWS);
                return Ok(());
            }
            KeyCode::PageDown => {
                p.sel = (p.sel + PICKER_ROWS).min(n.saturating_sub(1));
                return Ok(());
            }
            KeyCode::Enter => p.sel,
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                c as usize - '1' as usize
            }
            _ => return Ok(()),
        };
        let p = self.picker.take().expect("picker");
        let value = p.items[chosen].1.clone();
        match p.action {
            Pick::Model => {
                if !self.chat.is_null() {
                    self.patch_chat(cx, json!({"model": value})).await?;
                }
                self.model = value;
            }
            Pick::Effort => {
                if !self.chat.is_null() {
                    self.patch_chat(cx, json!({"effort": value})).await?;
                }
                self.effort = value;
            }
            Pick::Mode => self.set_mode(cx, &value).await?,
            Pick::Chat => self.open(cx, &value).await?,
            Pick::Import(_) => {
                let path = format!("/projects/{}/imports", string(&self.project, "id"));
                let chat = self
                    .remote
                    .api(cx, Method::Post, &path, Some(json!({"id": value})))
                    .await?;
                self.chats.insert(0, chat.clone());
                self.activate(cx, chat)?;
            }
            Pick::Showcase(_) => self.showcase_items(cx, &value).await?,
            Pick::ShowcaseItems(source, _) => {
                let (kind, path) = value.split_once('\t').unwrap_or(("", ""));
                if kind == "skill" {
                    self.open_picker(Pick::ShowcaseScope(source, path.to_owned()))?;
                } else {
                    self.showcase_install(cx, &source, kind, path, "project")
                        .await?;
                }
            }
            Pick::ShowcaseScope(source, path) => {
                self.showcase_install(cx, &source, "skill", &path, &value)
                    .await?
            }
            Pick::Runtime(bot) => {
                screens::patch_bot(self, cx, &bot, json!({"runtime": value})).await?
            }
            Pick::Policy(bot, key) => {
                let mut tools = self
                    .bots
                    .iter()
                    .find(|b| b["id"] == bot)
                    .map(|b| b["tools"].clone())
                    .unwrap_or_default();
                tools[key] = json!(value);
                screens::patch_bot(self, cx, &bot, json!({"tools": tools})).await?;
            }
        }
        Ok(())
    }

    // ----- Mouse -----------------------------------------------------------

    async fn mouse(&mut self, cx: &Cx, m: MouseEvent) -> Result<()> {
        if screens::live_mouse(self, m) {
            return Ok(());
        }
        let over_nav = self.nav.contains(m.x, m.y);
        match m.kind {
            MouseEventKind::ScrollUp if over_nav => self.nav.scroll_by(-3),
            MouseEventKind::ScrollDown if over_nav => self.nav.scroll_by(3),
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_add(3);
                self.sel = None;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(3);
                self.sel = None;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.nav_click(cx, m.x, m.y).await? {
                    self.sel = None;
                    return Ok(());
                }
                if screens::body_click(self, cx, m.x, m.y).await? {
                    return Ok(());
                }
                self.focus = Focus::Editor;
                let row = m.y.saturating_sub(self.body_top);
                self.sel = (self.screen == Screen::Chat
                    && m.y >= self.body_top
                    && row < self.body_height
                    && !self.body_texts.is_empty())
                .then_some((row, row));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((start, _)) = self.sel {
                    let row =
                        m.y.saturating_sub(self.body_top)
                            .min(self.body_height.saturating_sub(1));
                    self.sel = Some((start, row));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((a, b)) = self.sel.take() {
                    let (lo, hi) = (a.min(b) as usize, a.max(b) as usize);
                    let text = self
                        .body_texts
                        .get(lo..=hi)
                        .map(|rows| rows.join("\n"))
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        platform::copy(&text);
                        self.status = self
                            .msg("Testo copiato negli appunti", "Copied to clipboard")
                            .into();
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ----- Rendering -------------------------------------------------------

    pub fn draw(&mut self, f: &mut Frame) {
        let center = nav::draw(f, self);
        self.editor_width = usize::from(center.width.saturating_sub(4)).max(1);
        if center.width > 0 {
            view::draw(f, center, self);
        }
        if self.form.is_some() {
            screens::draw_form(f, self);
        }
        if self.focus != Focus::Editor
            || self.form.is_some()
            || self.approval.is_some() && self.input.is_empty()
        {
            f.set_cursor(None);
        }
        self.dirty = false;
    }

    pub fn next_frame_serial(&mut self) -> u64 {
        self.frame_serial += 1;
        self.frame_serial
    }

    pub fn next_socket_id(&mut self) -> u64 {
        self.socket_serial += 1;
        self.socket_serial
    }

    // ----- Commands --------------------------------------------------------

    pub async fn command(&mut self, cx: &Cx, line: &str) -> Result<()> {
        if let Some(escaped) = line.strip_prefix("//") {
            let literal = format!("/{escaped}");
            return self.send(cx, &literal, "message", None).await;
        }
        let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        match command {
            "/logout" => {
                let result = self.remote.api(cx, Method::Delete, "/session", None).await;
                let redirect = result
                    .ok()
                    .and_then(|v| v["redirect_url"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                platform::signed_out(&redirect, &string(&self.session, "portal_url"));
                self.quit = true;
            }
            "/quit" | "/exit" => self.quit = platform::CAN_QUIT,
            "/help" => self.note(self.help()),
            "/diff" => {
                if self.patches.is_empty() {
                    self.note(self.msg(
                        "Nessuna modifica nella conversazione",
                        "No changes in this conversation",
                    ));
                } else {
                    let diff = self.patches.join("\n");
                    self.lines.push(Message {
                        role: Role::Tool,
                        head: "Diff".into(),
                        status: "ok".into(),
                        diff,
                        ..Default::default()
                    });
                }
            }
            "/copy" => {
                let last = self
                    .lines
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Assistant && !m.text.trim().is_empty())
                    .map(|m| m.text.clone());
                match last {
                    Some(text) => {
                        platform::copy(&text);
                        self.status = self
                            .msg(
                                "Risposta copiata negli appunti",
                                "Copied last reply to clipboard",
                            )
                            .into();
                    }
                    None => {
                        self.status = self
                            .msg("Nessuna risposta da copiare", "No reply to copy")
                            .into()
                    }
                }
            }
            "/reconnect" => {
                let id = string(&self.chat, "id");
                ensure!(
                    !id.is_empty(),
                    self.msg("Nessuna conversazione attiva", "No active conversation")
                );
                if let Some(f) = self.feed.take() {
                    f.stop();
                }
                self.feed = platform::follow(
                    cx,
                    &self.remote,
                    id,
                    self.last_sequence,
                    self.en,
                    self.tx.clone(),
                );
                self.status.clear();
            }
            "/rename" if argument.is_empty() => {
                self.path()?;
                self.form = Some(Form::new(
                    self.msg("Rinomina conversazione", "Rename conversation")
                        .into(),
                    vec![Field::new(
                        "title",
                        self.msg("Titolo", "Title"),
                        &string(&self.chat, "title"),
                        FieldKind::Text,
                    )],
                    Act::Rename,
                ));
            }
            "/rename" => self.patch_chat(cx, json!({"title": argument})).await?,
            "/delete" => {
                let id = string(&self.chat, "id");
                ensure!(
                    !id.is_empty(),
                    self.msg("Nessuna conversazione attiva", "No active conversation")
                );
                self.confirm_delete_chat(&id);
            }
            "/delete-project" => {
                let id = string(&self.project, "id");
                ensure!(
                    !id.is_empty(),
                    self.msg("Nessun progetto selezionato", "No project selected")
                );
                self.confirm_delete_project(&id);
            }
            "/status" => {
                let parts: Vec<String> = [
                    string(&self.session["workspace"], "name"),
                    string(&self.session["user"], "name"),
                    string(&self.project, "name"),
                    self.model.clone(),
                    self.effort.clone(),
                    self.current_mode(),
                    string(&self.chat, "state"),
                ]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect();
                self.note(parts.join(" · "));
            }
            "/language" => {
                self.en = match argument {
                    "it" => false,
                    "en" => true,
                    _ => !self.en,
                };
                platform::save_language(self.en);
            }
            "/browser" => self.show(Screen::Browser),
            "/desktop" | "/computer" => {
                ensure!(
                    self.desktop,
                    self.msg(
                        "Desktop remoto non disponibile",
                        "Remote desktop unavailable"
                    )
                );
                self.show(Screen::Desktop);
            }
            "/connections" => self.show(Screen::Connections),
            "/bots" => {
                self.nav.reveal("s:bots".into());
                self.focus = Focus::Nav;
            }
            "/projects" | "/harnesses" | "/models" | "/chats" if argument.is_empty() => {
                self.refresh(cx).await?;
                let listing = match command {
                    "/projects" => entries(&self.projects),
                    "/models" => entries(&self.models),
                    "/harnesses" => self
                        .harnesses
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let roots: Vec<&str> = h["roots"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .collect();
                            format!("{}  {}  {}", i + 1, string(h, "name"), roots.join(", "))
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => entries(&self.project_chats()),
                };
                self.note(listing);
            }
            "/model" if argument.is_empty() => self.open_picker(Pick::Model)?,
            "/effort" if argument.is_empty() => self.open_picker(Pick::Effort)?,
            "/permissions" if argument.is_empty() => self.open_picker(Pick::Mode)?,
            "/resume" | "/open" if argument.is_empty() => self.open_picker(Pick::Chat)?,
            "/skills" => {
                let id = string(&self.project, "id");
                ensure!(
                    !id.is_empty(),
                    self.msg(
                        "F2 · scegli prima un progetto",
                        "F2 · choose a project first"
                    )
                );
                let listing = self
                    .remote
                    .api(cx, Method::Get, &format!("/projects/{id}/skills"), None)
                    .await?;
                let items = listing["items"].as_array().cloned().unwrap_or_default();
                let lines: Vec<String> = items
                    .iter()
                    .map(|s| {
                        clean(&format!(
                            "/skill:{:<24} {}",
                            string(s, "name"),
                            string(s, "description")
                        ))
                    })
                    .collect();
                self.skills = (self.project["id"].clone(), items);
                let plugins = self
                    .remote
                    .api(cx, Method::Get, &format!("/projects/{id}/plugins"), None)
                    .await?;
                let plugins: Vec<String> = plugins["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        clean(&format!(
                            "plugin {:<24} {}",
                            string(p, "name"),
                            string(p, "path")
                        ))
                    })
                    .collect();
                let mut out = Vec::new();
                out.push(if lines.is_empty() {
                    self.msg(
                        "Nessuna skill in .pi/skills, .agents/skills, ~/.pi/agent/skills o ~/.agents/skills",
                        "No skills in .pi/skills, .agents/skills, ~/.pi/agent/skills or ~/.agents/skills",
                    )
                    .to_owned()
                } else {
                    lines.join("\n")
                });
                if !plugins.is_empty() {
                    out.push(plugins.join("\n"));
                }
                out.push(
                    self.msg(
                        "/showcase per installarne altri",
                        "/showcase to install more",
                    )
                    .to_owned(),
                );
                self.note(out.join("\n\n"));
            }
            "/showcase" => {
                let id = string(&self.project, "id");
                ensure!(
                    !id.is_empty(),
                    self.msg(
                        "F2 · scegli prima un progetto",
                        "F2 · choose a project first"
                    )
                );
                if argument.is_empty() {
                    let listing = self.remote.api(cx, Method::Get, "/showcase", None).await?;
                    let lang = if self.en { "en" } else { "it" };
                    let items = listing["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|s| {
                            let label = format!(
                                "{} · {}",
                                string(s, "title"),
                                string(&s["description"], lang)
                            );
                            (clean(&label), string(s, "id"))
                        })
                        .collect();
                    self.open_picker(Pick::Showcase(items))?;
                } else {
                    self.showcase_items(cx, argument).await?;
                }
            }
            "/uninstall" => {
                let id = string(&self.project, "id");
                let mut parts = argument.split_whitespace();
                let (kind, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let scope = parts.next().unwrap_or("project");
                ensure!(
                    !id.is_empty() && matches!(kind, "skill" | "plugin") && !name.is_empty(),
                    "/uninstall skill|plugin nome/name [user]"
                );
                let body = json!({"kind": kind, "name": name, "scope": scope});
                let out = self
                    .remote
                    .api(
                        cx,
                        Method::Post,
                        &format!("/projects/{id}/showcase/remove"),
                        Some(body),
                    )
                    .await?;
                self.skills.0 = Value::Null;
                self.note(if out["removed"] == true {
                    self.msg("Rimosso", "Removed")
                } else {
                    self.msg("Non installato", "Not installed")
                });
            }
            "/import" => {
                let id = string(&self.project, "id");
                ensure!(
                    !id.is_empty(),
                    self.msg(
                        "F2 · scegli prima un progetto",
                        "F2 · choose a project first"
                    )
                );
                let listing = self
                    .remote
                    .api(cx, Method::Get, &format!("/projects/{id}/imports"), None)
                    .await?;
                let items = listing["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        let label = format!("{} · {}", string(s, "source"), string(s, "title"));
                        (clean(&label), string(s, "id"))
                    })
                    .collect();
                self.open_picker(Pick::Import(items))?;
            }
            "/use" => {
                let project = choice(&self.projects, argument)?.clone();
                self.select_project(&project);
            }
            "/folder" if argument.is_empty() => {
                self.refresh(cx).await?;
                self.nav.reveal("s:harnesses".into());
                self.focus = Focus::Nav;
            }
            "/folder" => {
                let mut parts = argument.splitn(3, ' ');
                let harness = choice(&self.harnesses, parts.next().unwrap_or(""))?;
                let root = parts.next().context("/folder N root path")?;
                let folder = parts.next().unwrap_or(".");
                let payload = json!({"harness_id": harness["id"], "root": root, "path": folder});
                let project = self
                    .remote
                    .api(cx, Method::Post, "/projects", Some(payload))
                    .await?;
                self.projects.push(project.clone());
                self.select_project(&project);
            }
            "/open" | "/resume" => {
                let id = string(choice(&self.project_chats(), argument)?, "id");
                self.open(cx, &id).await?;
            }
            "/permissions" => {
                ensure!(
                    ["ask", "auto_edit", "plan"].contains(&argument),
                    "/permissions ask|auto_edit|plan"
                );
                self.set_mode(cx, argument).await?;
            }
            "/model" => {
                let model = string(choice(&self.models, argument)?, "id");
                if !self.chat.is_null() {
                    self.patch_chat(cx, json!({"model": model})).await?;
                }
                self.model = model;
            }
            "/effort" => {
                ensure!(
                    ["none", "low", "medium", "high"].contains(&argument),
                    "/effort none|low|medium|high"
                );
                if !self.chat.is_null() {
                    self.patch_chat(cx, json!({"effort": argument})).await?;
                }
                self.effort = argument.to_owned();
            }
            "/new" => {
                if self.project.is_null() {
                    bail!(self.msg(
                        "F2 · scegli prima un progetto",
                        "F2 · choose a project first"
                    ));
                }
                if argument.is_empty() {
                    self.close();
                    self.show(Screen::Chat);
                } else {
                    let mode = self.mode.clone();
                    self.new_chat(cx, argument, &mode).await?;
                }
            }
            "/stop" => {
                let path = format!("{}/interrupt", self.path()?);
                self.remote.api(cx, Method::Post, &path, None).await?;
            }
            "/allow" | "/deny" => {
                let approval = self
                    .approval
                    .clone()
                    .context("Nessuna approvazione / No pending approval")?;
                let decision = if command == "/allow" {
                    "allow_once"
                } else {
                    "deny"
                };
                self.remote
                    .api(
                        cx,
                        Method::Post,
                        &format!("/approvals/{}", approval.id),
                        Some(json!({"decision": decision})),
                    )
                    .await?;
                self.approval = None;
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
            "/input" | "/end-session" => {
                let (call, _) = self.interactive().context(self.msg(
                    "Nessuna sessione interattiva in corso",
                    "No interactive session running",
                ))?;
                let path = format!("{}/sessions/{call}/input", self.path()?);
                let body = if command == "/input" {
                    json!({"id": platform::new_id(), "action": "send", "text": argument})
                } else {
                    json!({"id": platform::new_id(), "action": "stop"})
                };
                self.remote.api(cx, Method::Post, &path, Some(body)).await?;
            }
            _ if command.starts_with('/') && command != "/answer" => {
                bail!(
                    "Comando sconosciuto / Unknown command: {command} (// per iniziare con / · // to start with /)"
                )
            }
            _ if !self.question.is_empty() => {
                let question = self.question.clone();
                let answer = if command == "/answer" { argument } else { line };
                let answer = answer
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| self.choices.get(i))
                    .map(String::as_str)
                    .unwrap_or(answer)
                    .to_owned();
                self.send(cx, &answer, "answer", Some(&question)).await?;
            }
            _ => {
                ensure!(
                    command != "/answer",
                    "Nessuna domanda / No pending question"
                );
                if let Some((call, _)) = self.interactive() {
                    // A running interactive session takes plain lines, as a terminal would.
                    let path = format!("{}/sessions/{call}/input", self.path()?);
                    let body = json!({"id": platform::new_id(), "action": "send", "text": line});
                    self.remote.api(cx, Method::Post, &path, Some(body)).await?;
                } else {
                    self.send(cx, line, "message", None).await?;
                }
            }
        }
        Ok(())
    }

    /// Open the item picker for a showcase source id or `owner/repo`.
    async fn showcase_items(&mut self, cx: &Cx, source: &str) -> Result<()> {
        self.note(self.msg(
            "Scarico il repository dall'harness…",
            "Downloading the repository on the harness…",
        ));
        let path = format!(
            "/projects/{}/showcase?source={}",
            string(&self.project, "id"),
            query_escape(source)
        );
        let listing = self.remote.api(cx, Method::Get, &path, None).await?;
        let items: Vec<(String, String)> = listing["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|i| {
                (
                    showcase_label(i, self.en),
                    format!("{}\t{}", string(i, "kind"), string(i, "path")),
                )
            })
            .collect();
        ensure!(
            !items.is_empty(),
            self.msg(
                "Nessuna skill o plugin pi in questo repository",
                "No pi skills or plugins in this repository"
            )
        );
        self.open_picker(Pick::ShowcaseItems(source.to_owned(), items))
    }

    async fn showcase_install(
        &mut self,
        cx: &Cx,
        source: &str,
        kind: &str,
        path: &str,
        scope: &str,
    ) -> Result<()> {
        let url = format!("/projects/{}/showcase/install", string(&self.project, "id"));
        let body = json!({"source": source, "kind": kind, "path": path, "scope": scope});
        let out = self.remote.api(cx, Method::Post, &url, Some(body)).await?;
        // Completion for /skill: reloads on the next /skills.
        self.skills.0 = Value::Null;
        let name = string(&out, "name");
        let place = string(&out, "installed");
        let files = out["files"].as_u64().unwrap_or(0);
        let note = if kind == "skill" {
            format!(
                "{} {name} → {place} ({files} file) · /skills, /skill:{name}",
                self.msg("Skill installata", "Skill installed")
            )
        } else {
            format!(
                "{} {name} → {place} ({files} file) · {}",
                self.msg("Plugin installato", "Plugin installed"),
                self.msg(
                    "l'agente lo trova con extension_list_tools",
                    "the agent finds it with extension_list_tools"
                )
            )
        };
        self.note(clean(&note));
        Ok(())
    }

    fn help(&self) -> String {
        let mut out: Vec<String> = COMMANDS
            .iter()
            .map(|(c, it, en)| format!("{c:<16} {}", if self.en { en } else { it }))
            .collect();
        out.push(String::new());
        out.push(self.msg(
            "Tab pannelli · F2 progetti · F3 harness · F4 mostra/nasconde i pannelli · Ctrl+B albero · Ctrl+Alt+B dettagli · Shift+Tab modalità · Ctrl+L modello · Ctrl+O espande gli strumenti · Esc interrompe · Ctrl+C cancella/esce · Shift+Invio a capo · ? aiuto",
            "Tab panes · F2 projects · F3 harnesses · F4 show/hide panes · Ctrl+B tree · Ctrl+Alt+B inspector · Shift+Tab mode · Ctrl+L model · Ctrl+O expand tools · Esc interrupt · Ctrl+C clear/quit · Shift+Enter newline · ? help",
        ).into());
        out.join("\n")
    }

    pub fn confirm_delete_chat(&mut self, id: &str) {
        let title = self
            .chats
            .iter()
            .find(|c| c["id"] == id)
            .map(|c| string(c, "title"))
            .unwrap_or_default();
        self.form = Some(Form::confirm(
            self.msg("Elimina conversazione", "Delete conversation")
                .into(),
            format!(
                "{} “{title}”? {}",
                self.msg("Eliminare", "Delete"),
                self.msg(
                    "Trascrizione e output verranno cancellati.",
                    "Transcript and output will be erased."
                )
            ),
            Act::DeleteChat(id.to_owned()),
        ));
    }

    pub fn confirm_delete_project(&mut self, id: &str) {
        let name = self
            .projects
            .iter()
            .find(|p| p["id"] == id)
            .map(|p| string(p, "name"))
            .unwrap_or_default();
        self.form = Some(Form::confirm(
            self.msg("Elimina progetto", "Delete project").into(),
            format!(
                "{} “{name}” {}",
                self.msg("Eliminare il progetto", "Delete project"),
                self.msg(
                    "e tutte le sue conversazioni? I file sull'harness non vengono toccati.",
                    "and all its conversations? Files on the harness are not touched."
                )
            ),
            Act::DeleteProject(id.to_owned()),
        ));
    }

    /// Run a submitted form or confirmation.
    pub async fn act(&mut self, cx: &Cx, form: &Form) -> Result<()> {
        match &form.action {
            Act::Rename => {
                let title = form.value(0).trim();
                ensure!(!title.is_empty(), self.msg("Titolo vuoto", "Empty title"));
                self.patch_chat(cx, json!({"title": title})).await?;
            }
            Act::DeleteChat(id) => {
                self.remote
                    .api(cx, Method::Delete, &format!("/conversations/{id}"), None)
                    .await?;
                self.chats.retain(|c| c["id"] != id.as_str());
                if self.chat["id"] == id.as_str() {
                    self.close();
                    self.show(Screen::Chat);
                }
            }
            Act::DeleteProject(id) => {
                self.remote
                    .api(cx, Method::Delete, &format!("/projects/{id}"), None)
                    .await?;
                self.projects.retain(|p| p["id"] != id.as_str());
                self.chats.retain(|c| c["project_id"] != id.as_str());
                if self.project["id"] == id.as_str() {
                    self.close();
                    self.project = Value::Null;
                    self.show(Screen::Chat);
                }
            }
            Act::AddProject(harness) => {
                let path = form.get("path").trim();
                let mut body = json!({
                    "harness_id": harness["id"],
                    "root": form.get("root"),
                    "path": if path.is_empty() { "." } else { path },
                });
                if !form.get("name").trim().is_empty() {
                    body["name"] = json!(form.get("name").trim());
                }
                let project = self
                    .remote
                    .api(cx, Method::Post, "/projects", Some(body))
                    .await?;
                self.projects.push(project.clone());
                self.select_project(&project);
            }
            Act::OpenUrl => screens::live_open(self, form.value(0))?,
            _ => screens::act(self, cx, form).await?,
        }
        Ok(())
    }

    // ----- Navigator -------------------------------------------------------

    async fn nav_key(&mut self, cx: &Cx, k: KeyEvent) -> Result<()> {
        if k.code == KeyCode::Escape && self.nav.filter.is_none() {
            self.focus = Focus::Editor;
            return Ok(());
        }
        nav::key(self, cx, k).await
    }

    async fn nav_click(&mut self, cx: &Cx, x: u16, y: u16) -> Result<bool> {
        nav::click(self, cx, x, y).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_app() -> App {
        let (tx, _rx) = platform::channel();
        App::new(platform::test_remote(), true, tx)
    }

    #[test]
    fn ctrl_w_handles_multibyte_unicode_whitespace_without_panicking() {
        let mut input = "hello\u{00A0}world".to_owned();
        let mut caret = input.len();
        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "hello\u{00A0}");
        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "");
        let mut input = "foo\u{3000}bar\u{3000}".to_owned();
        let mut caret = input.len();
        delete_prev_word(&mut input, &mut caret);
        assert_eq!(input, "foo\u{3000}");
    }

    #[test]
    fn transcript_dedupes_sequences_and_folds_tools() {
        let mut app = test_app();
        app.event(json!({"sequence":1,"type":"user_message","content":"hello"}));
        app.event(json!({"sequence":2,"type":"text","message_id":"m1","delta":"world"}));
        app.event(json!({"sequence":2,"type":"text","message_id":"m1","delta":"world"}));
        assert_eq!(app.lines.len(), 2);
        assert_eq!(app.lines[1].text, "world");
        app.event(json!({"sequence":3,"type":"tool_call","call_id":"t1","operation":"execute","target":"ls"}));
        app.event(json!({"sequence":4,"type":"tool_output","call_id":"t1","data":"a\nb"}));
        app.event(json!({"sequence":5,"type":"approval","call_id":"t1","approval_id":"a1","approval_state":"pending","summary":"ls"}));
        assert_eq!(app.lines[2].status, "waiting");
        assert_eq!(app.approval.as_ref().unwrap().head, "Bash(ls)");
        app.event(json!({"sequence":6,"type":"approval","call_id":"t1","approval_id":"a1","approval_state":"approved"}));
        assert!(app.approval.is_none());
        app.event(json!({"sequence":7,"type":"tool_result","call_id":"t1","status":"ok","summary":"exit 0",
            "diff":{"path":"f.rs","patch":"+x","additions":1,"deletions":0}}));
        let tool = &app.lines[2];
        assert_eq!(
            (tool.status.as_str(), tool.text.as_str(), tool.note.as_str()),
            ("ok", "a\nb", "exit 0")
        );
        assert_eq!(app.changes, vec![("f.rs".to_owned(), 1, 0)]);
        app.event(json!({"sequence":8,"type":"todo","todos":[{"status":"pending","content":"x"}]}));
        app.event(
            json!({"sequence":9,"type":"todo","todos":[{"status":"completed","content":"x"}]}),
        );
        assert_eq!(app.lines.len(), 4);
        assert_eq!(app.lines[3].text, "completed\tx");
        app.close();
        assert_eq!(app.last_sequence, 0);
    }

    #[test]
    fn slash_command_enter_requires_exact_match_or_arguments() {
        let partial = slash_suggestions("/q", false, &[]).len();
        assert!(!needs_tab_completion(
            "/exit",
            slash_suggestions("/exit", false, &[]).len(),
            &[]
        ));
        assert_eq!(partial, 0);
        let de = slash_suggestions("/de", false, &[]);
        assert!(de.iter().any(|c| c.0 == "/delete"));
        assert!(needs_tab_completion("/de", de.len(), &[]));
        assert!(!needs_tab_completion("//q", 0, &[]));
        assert!(slash_suggestions("/model 1", false, &[]).is_empty());
        assert_eq!(query_escape("acme/tools@v1 x"), "acme/tools%40v1%20x");
        let plugin = json!({"kind":"plugin","name":"todo","tools":false,"description":"Todos"});
        assert_eq!(
            showcase_label(&plugin, true),
            "plugin · todo [no tools: commands/UI only] · Todos"
        );
        // pi skills complete as /skill:name and run as typed.
        let skills = [json!({"name":"sql-expert","description":"SQL help"})];
        let found = slash_suggestions("/skill:s", false, &skills);
        assert_eq!(
            found,
            [(
                "/skill:sql-expert".to_owned(),
                "SQL help".to_owned(),
                "SQL help".to_owned()
            )]
        );
        assert!(needs_tab_completion("/skill:s", found.len(), &skills));
        assert!(!needs_tab_completion("/skill:sql-expert", 1, &skills));
    }

    #[test]
    fn output_cap_preserves_utf8_boundaries() {
        let mut buf = String::new();
        append_capped_output(&mut buf, "éééé", 5);
        assert!(buf.starts_with("éé"));
        assert!(buf.contains("output truncated"));
        let len = buf.len();
        append_capped_output(&mut buf, "more", 5);
        assert_eq!(buf.len(), len);
    }

    #[test]
    fn long_pastes_collapse_and_expand_after_edits_around_them() {
        let mut payloads = Vec::new();
        let mut input = String::new();
        let mut caret = 0;
        let long = "x".repeat(PASTE_COLLAPSE_THRESHOLD + 1);
        insert_paste(&mut input, &mut caret, &long, &mut payloads);
        insert_paste(&mut input, &mut caret, " and ", &mut payloads);
        insert_paste(&mut input, &mut caret, &format!("{long}y"), &mut payloads);
        assert_eq!(payloads.len(), 2);
        input.insert_str(0, "see: ");
        assert_eq!(
            expand_pasted(&input, &payloads),
            format!("see: {long} and {long}y")
        );
        let partial = input.replace(&paste_placeholder(0, &payloads[0]), "");
        assert_eq!(
            expand_pasted(&partial, &payloads),
            format!("see:  and {long}y")
        );
    }

    #[test]
    fn uuid_check_and_tool_heads() {
        assert!(is_uuid("c0ffee00-1234-4abc-8def-0123456789ab"));
        assert!(!is_uuid("../etc"));
        assert_eq!(tool_head("read_file", "a.rs"), "Read(a.rs)");
        assert_eq!(tool_head("mcp_call_tool", ""), "MCP");
    }
}
