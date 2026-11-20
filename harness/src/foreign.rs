//! Claude Code and Codex session import, ported from pi_agent_rust's
//! `session_import`. Only sessions recorded for the project folder are listed,
//! and an import is addressed by the listed id, never by a caller-supplied path.
//! Completed tool exchanges stay tool calls; calls without a recorded result
//! are closed as failed history so they are never treated as pending work.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 64 * 1024;
const MAX_LISTED: usize = 50;
const MAX_CODEX_FILES: usize = 5_000;
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;
// The import result travels in one relay frame (8 MiB); keep headroom for framing.
const MAX_EVENT_BYTES: usize = 6 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Claude,
    Codex,
}

struct Found {
    id: String,
    source: Source,
    path: PathBuf,
    modified_ms: u64,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn claude_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".claude")))
        .map(|d| d.join("projects"))
}

fn codex_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
        .map(|d| d.join("sessions"))
}

/// Claude Code names a project's log folder after its cwd with every
/// non-alphanumeric character replaced by `-`.
fn claude_folder(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn first_line(path: &Path) -> Option<Value> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(file)
        .take(MAX_HEADER_BYTES)
        .read_until(b'\n', &mut line)
        .ok()?;
    serde_json::from_slice(&line).ok()
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_CODEX_FILES {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && depth > 0 {
            jsonl_files(&path, out, depth - 1);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

fn discover(cwd: &Path) -> Vec<Found> {
    let mut found = Vec::new();
    if let Some(dir) = claude_dir() {
        let mut files = Vec::new();
        jsonl_files(&dir.join(claude_folder(cwd)), &mut files, 0);
        for path in files {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            found.push(Found {
                id: format!("claude:{stem}"),
                source: Source::Claude,
                modified_ms: modified_ms(&path),
                path,
            });
        }
    }
    if let Some(dir) = codex_dir() {
        // ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl, opened by a session_meta record.
        let mut files = Vec::new();
        jsonl_files(&dir, &mut files, 3);
        for path in files {
            let meta = first_line(&path);
            let recorded = meta
                .as_ref()
                .and_then(|m| m.pointer("/payload/cwd"))
                .and_then(Value::as_str);
            if recorded.is_some_and(|c| Path::new(c) == cwd) {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                found.push(Found {
                    id: format!("codex:{stem}"),
                    source: Source::Codex,
                    modified_ms: modified_ms(&path),
                    path,
                });
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.modified_ms));
    found
}

fn read_source(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).context("Cannot open the session log")?;
    ensure!(
        file.metadata()?.is_file(),
        "Session log must be a regular file"
    );
    let mut raw = Vec::new();
    file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut raw)?;
    ensure!(
        raw.len() as u64 <= MAX_SOURCE_BYTES,
        "Session log exceeds the 128 MiB import limit"
    );
    Ok(raw)
}

fn records(raw: &[u8]) -> Vec<Value> {
    raw.split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(limit);
    format!("{}\n… [truncated {} bytes]", &text[..end], text.len() - end)
}

/// Text of a string or an array of text-like content blocks.
fn content_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item["type"].as_str() {
                Some("image" | "input_image" | "image_url") => Some("[image]"),
                _ => item
                    .get("text")
                    .or_else(|| item.get("input_text"))
                    .or_else(|| item.get("output_text"))
                    .and_then(Value::as_str),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn target(arguments: &Value) -> String {
    [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "input",
    ]
    .iter()
    .find_map(|k| arguments[*k].as_str())
    .map(|s| s.chars().take(120).collect())
    .unwrap_or_default()
}

/// Builds venticode transcript events from foreign records.
#[derive(Default)]
struct Events {
    out: Vec<Value>,
    bytes: usize,
    truncated: bool,
    message: usize,
    in_assistant: bool,
    open: Vec<String>,
    known: HashSet<String>,
    title: String,
}

impl Events {
    fn push(&mut self, event: Value) {
        if self.truncated {
            return;
        }
        let size = event.to_string().len();
        if self.bytes + size > MAX_EVENT_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += size;
        self.out.push(event);
    }

    fn close_open(&mut self) {
        for id in std::mem::take(&mut self.open) {
            self.push(json!({"type":"tool_result","call_id":id,"status":"error",
                "summary":"No result was recorded in the imported session"}));
        }
    }

    fn user(&mut self, text: &str) {
        // Harness-injected turns (slash-command echoes, Codex context) are not the user's prompts.
        const INJECTED: &[&str] = &[
            "<command-",
            "<local-command-",
            "<environment_context>",
            "<user_instructions>",
        ];
        let trimmed = text.trim_start();
        if trimmed.is_empty() || INJECTED.iter().any(|p| trimmed.starts_with(p)) {
            return;
        }
        self.close_open();
        self.in_assistant = false;
        self.message += 1;
        if self.title.is_empty() {
            self.title = text.lines().next().unwrap_or("").chars().take(80).collect();
        }
        let id = format!("import-{}", self.message);
        self.push(
            json!({"type":"user_message","message_id":id,"content":clip(text, MAX_TEXT_BYTES)}),
        );
    }

    fn assistant_id(&mut self) -> String {
        if !self.in_assistant {
            self.in_assistant = true;
            self.message += 1;
        }
        format!("import-{}", self.message)
    }

    fn text(&mut self, kind: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let id = self.assistant_id();
        self.push(json!({"type":kind,"message_id":id,"delta":clip(text, MAX_TEXT_BYTES)}));
    }

    fn call(&mut self, id: &str, name: &str, arguments: Value) {
        self.assistant_id();
        if !self.known.insert(id.to_owned()) {
            return;
        }
        let target = target(&arguments);
        self.open.push(id.to_owned());
        self.push(json!({"type":"tool_call","call_id":id,"operation":name,"arguments":arguments,"target":target}));
    }

    /// Orphan results (no recorded call) are dropped: they cannot be paired.
    fn result(&mut self, id: &str, output: &str, failed: bool) {
        let Some(i) = self.open.iter().position(|o| o == id) else {
            return;
        };
        self.open.remove(i);
        if !output.is_empty() {
            self.push(json!({"type":"tool_output","call_id":id,"data":clip(output, MAX_TOOL_OUTPUT_BYTES)}));
        }
        let (status, summary) = if failed {
            ("error", "Failed in the imported session")
        } else {
            ("ok", "")
        };
        self.push(json!({"type":"tool_result","call_id":id,"status":status,"summary":summary}));
    }
}

/// Keep the latest main-thread branch of a parent-linked Claude log, like pi:
/// the last non-sidechain message is the leaf and only its ancestors remain.
fn claude_branch(records: Vec<Value>) -> Vec<Value> {
    let is_message = |r: &Value| matches!(r["type"].as_str(), Some("user" | "assistant"));
    if !records
        .iter()
        .any(|r| is_message(r) && r.get("parentUuid").is_some())
    {
        return records;
    }
    let parents: HashMap<&str, Option<&str>> = records
        .iter()
        .filter_map(|r| Some((r["uuid"].as_str()?, r["parentUuid"].as_str())))
        .collect();
    let Some(leaf) = records
        .iter()
        .rev()
        .find(|r| is_message(r) && r["isSidechain"] != true)
        .and_then(|r| r["uuid"].as_str())
    else {
        return Vec::new();
    };
    let mut keep = HashSet::new();
    let mut cursor = Some(leaf);
    while let Some(id) = cursor {
        if !keep.insert(id.to_owned()) {
            break; // cycle guard
        }
        cursor = parents.get(id).copied().flatten();
    }
    records
        .into_iter()
        .filter(|r| r["uuid"].as_str().is_some_and(|id| keep.contains(id)))
        .collect()
}

fn convert_claude(records: Vec<Value>, events: &mut Events) {
    for record in claude_branch(records) {
        let role = record["type"].as_str().unwrap_or("");
        if !matches!(role, "user" | "assistant") || record["isMeta"] == true {
            continue;
        }
        let content = &record["message"]["content"];
        let Some(blocks) = content.as_array() else {
            if role == "user" {
                events.user(&content_text(content));
            } else {
                events.text("text", &content_text(content));
            }
            continue;
        };
        let mut user_text = Vec::new();
        for block in blocks {
            match (role, block["type"].as_str()) {
                ("user", Some("tool_result")) => {
                    if let Some(id) = block["tool_use_id"].as_str() {
                        events.result(
                            id,
                            &content_text(&block["content"]),
                            block["is_error"] == true,
                        );
                    }
                }
                ("user", _) => user_text.push(content_text(&json!([block]))),
                ("assistant", Some("tool_use")) => {
                    if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str())
                        && block["input"].is_object()
                    {
                        events.call(id, name, block["input"].clone());
                    }
                }
                ("assistant", Some("thinking")) => {
                    events.text("reasoning", block["thinking"].as_str().unwrap_or(""));
                }
                ("assistant", Some("text")) => {
                    events.text("text", block["text"].as_str().unwrap_or(""));
                }
                _ => {}
            }
        }
        events.user(&user_text.join("\n"));
    }
}

fn convert_codex(records: Vec<Value>, events: &mut Events) {
    for record in records {
        if record["type"] != "response_item" {
            continue;
        }
        let p = &record["payload"];
        match p["type"].as_str() {
            Some("message") => {
                let text = content_text(&p["content"]);
                match p["role"].as_str() {
                    Some("user") => events.user(&text),
                    Some("assistant") => events.text("text", &text),
                    _ => {} // system/developer instructions belong to Codex, not this conversation
                }
            }
            Some("reasoning") => {
                let text = content_text(&p["summary"]);
                events.text("reasoning", &text);
            }
            Some(kind @ ("function_call" | "custom_tool_call")) => {
                let (Some(id), Some(name)) = (p["call_id"].as_str(), p["name"].as_str()) else {
                    continue;
                };
                let arguments = if kind == "custom_tool_call" {
                    json!({"input": p["input"].as_str().unwrap_or("")})
                } else {
                    match &p["arguments"] {
                        Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
                        other => other.clone(),
                    }
                };
                if arguments.is_object() {
                    events.call(id, name, arguments);
                }
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                if let Some(id) = p["call_id"].as_str() {
                    let output = &p["output"];
                    // function_call_output.output is often {"output":..,"metadata":{"exit_code":..}} as text.
                    let parsed = output
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .filter(Value::is_object);
                    let (text, failed) = match &parsed {
                        Some(o) => (
                            content_text(&o["output"]),
                            o["metadata"]["exit_code"].as_i64().is_some_and(|c| c != 0),
                        ),
                        None => (content_text(output), p["success"] == false),
                    };
                    events.result(id, &text, failed);
                }
            }
            _ => {}
        }
    }
}

fn convert(source: Source, raw: &[u8]) -> Events {
    let mut events = Events::default();
    match source {
        Source::Claude => convert_claude(records(raw), &mut events),
        Source::Codex => convert_codex(records(raw), &mut events),
    }
    events.close_open();
    events
}

fn label(source: Source) -> &'static str {
    match source {
        Source::Claude => "Claude Code",
        Source::Codex => "Codex",
    }
}

/// `foreign_sessions`: sessions recorded for this project folder, newest first.
pub fn list(cwd: &Path) -> Value {
    let items: Vec<Value> = discover(cwd)
        .into_iter()
        .take(MAX_LISTED)
        .map(|f| {
            // Titles need the first prompt; a capped read keeps the listing fast.
            let mut head = Vec::new();
            if let Ok(file) = std::fs::File::open(&f.path) {
                let _ = file.take(256 * 1024).read_to_end(&mut head);
            }
            let cut = head.iter().rposition(|b| *b == b'\n').unwrap_or(0);
            let title = convert(f.source, &head[..cut]).title;
            json!({"id":f.id,"source":label(f.source),"title":title,"modified_ms":f.modified_ms})
        })
        .collect();
    json!({ "items": items })
}

/// `foreign_session_import`: convert the listed session `id` into transcript events.
pub fn import(cwd: &Path, id: &str) -> Result<Value> {
    let found = discover(cwd)
        .into_iter()
        .find(|f| f.id == id)
        .context("Session not found for this project")?;
    let events = convert(found.source, &read_source(&found.path)?);
    ensure!(
        !events.out.is_empty(),
        "The session has no messages to import"
    );
    Ok(json!({
        "source": label(found.source),
        "title": events.title,
        "events": events.out,
        "truncated": events.truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(records: &[Value]) -> Vec<u8> {
        records
            .iter()
            .map(|r| format!("{r}\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn claude_keeps_the_latest_branch_and_pairs_tool_results() {
        let raw = lines(&[
            json!({"type":"summary","summary":"x"}),
            json!({"type":"user","uuid":"u0","parentUuid":null,"message":{"content":"<command-name>/model</command-name>"}}),
            json!({"type":"user","uuid":"u1","parentUuid":"u0","message":{"content":"Fix the build"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[
                {"type":"thinking","thinking":"look first"},
                {"type":"text","text":"Checking."},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make"}},
                {"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"x"}}]}}),
            json!({"type":"user","uuid":"r1","parentUuid":"a1","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"error: x","is_error":true},
                {"type":"tool_result","tool_use_id":"orphan","content":"?"}]}}),
            json!({"type":"user","uuid":"old","parentUuid":"u1","message":{"content":"abandoned branch"}}),
            json!({"type":"assistant","uuid":"side","parentUuid":"r1","isSidechain":true,"message":{"content":"sub"}}),
            json!({"type":"assistant","uuid":"a2","parentUuid":"r1","message":{"content":[{"type":"text","text":"Done."}]}}),
        ]);
        let e = convert(Source::Claude, &raw);
        let kinds: Vec<_> = e.out.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "user_message",
                "reasoning",
                "text",
                "tool_call",
                "tool_call",
                "tool_output",
                "tool_result",
                "text",
                "tool_result"
            ]
        );
        assert_eq!(e.title, "Fix the build");
        assert_eq!(e.out[3]["target"], "make");
        assert_eq!(e.out[6]["status"], "error");
        // t2 never got a result: closed as failed history, never left pending.
        assert_eq!(e.out[8]["call_id"], "t2");
        assert!(
            !e.out
                .iter()
                .any(|v| v["content"] == "abandoned branch" || v["delta"] == "sub")
        );
        assert_eq!(e.out[1]["message_id"], e.out[7]["message_id"]);
    }

    #[test]
    fn codex_imports_messages_calls_and_outputs() {
        let item = |p: Value| json!({"type":"response_item","payload":p});
        let raw = lines(&[
            json!({"type":"session_meta","payload":{"cwd":"/p"}}),
            item(
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}),
            ),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x"}]}),
            ),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"run tests"}]}),
            ),
            item(
                json!({"type":"function_call","name":"shell","call_id":"c1","arguments":"{\"command\":[\"cargo\",\"test\"]}"}),
            ),
            item(
                json!({"type":"function_call_output","call_id":"c1","output":"{\"output\":\"1 failed\",\"metadata\":{\"exit_code\":101}}"}),
            ),
            item(
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"One test fails."}]}),
            ),
        ]);
        let e = convert(Source::Codex, &raw);
        let kinds: Vec<_> = e.out.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "user_message",
                "tool_call",
                "tool_output",
                "tool_result",
                "text"
            ]
        );
        assert_eq!(e.out[0]["content"], "run tests");
        assert_eq!(e.out[2]["data"], "1 failed");
        assert_eq!(e.out[3]["status"], "error");
    }

    #[test]
    fn claude_folder_matches_claude_code_naming() {
        assert_eq!(
            claude_folder(Path::new("/home/a/my.proj")),
            "-home-a-my-proj"
        );
    }
}
