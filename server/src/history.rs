use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;

const TOOL_OUTPUT_LIMIT: usize = 65_536;
const DEFAULT_CONTEXT_TOKENS: usize = 128_000;
const SHAKE_TOOL_KEEP_RECENT: usize = 6;
const SHAKE_TOOL_THRESHOLD_BYTES: usize = 1_500;

pub(super) fn append_output(output: &mut String, chunk: &str) {
    let remaining = TOOL_OUTPUT_LIMIT.saturating_sub(output.len());
    output.push_str(&chunk[..chunk.floor_char_boundary(remaining)]);
}

pub(super) fn tool_content(status: &str, summary: &str, output: &str) -> String {
    match (status, output.is_empty()) {
        ("ok", false) => output.to_owned(),
        ("ok", true) => "Tool completed without output".into(),
        ("denied", _) => "Operation denied".into(),
        (_, true) => format!("Tool failed: {summary}"),
        (_, false) => format!("Tool failed: {summary}\n{output}"),
    }
}

struct Tool {
    id: String,
    name: String,
    arguments: Value,
    output: String,
    finished: Option<(String, String)>,
}

#[derive(Default)]
pub(super) struct History {
    messages: Vec<Value>,
    pending: Vec<Tool>,
}

impl History {
    pub(super) fn push(&mut self, event: Value, use_tools: bool) {
        match event["type"].as_str() {
            Some("user_message") => {
                self.flush_pending(
                    "error",
                    "Outcome unknown. Verify the project before retrying.",
                );
                let text = event["content"].as_str().unwrap_or_default();
                let content = match event["intent"].as_str() {
                    Some("goal") => format!("Goal: {text}"),
                    Some("implement_plan") => {
                        format!("The user approved the proposed plan. {text}")
                    }
                    _ => text.to_owned(),
                };
                self.messages.push(json!({"role":"user","content":content}));
            }
            Some("plan" | "question" | "goal_completed") => {
                self.flush_pending("error", "Outcome unknown");
                let mut content = event["content"].as_str().unwrap_or_default().to_owned();
                if let Some(opts) = event["options"].as_array() {
                    for opt in opts.iter().filter_map(Value::as_str) {
                        content.push_str("\n- ");
                        content.push_str(opt);
                    }
                }
                if self
                    .messages
                    .last()
                    .is_none_or(|m| m["role"] != "assistant" || m["content"] != content)
                {
                    self.messages
                        .push(json!({"role":"assistant","content":content}));
                }
            }
            Some("goal_cancelled") => {
                self.flush_pending(
                    "error",
                    "Outcome unknown. Verify the project before retrying.",
                );
                self.messages
                    .push(json!({"role":"user","content":"The previous goal was cancelled."}));
            }
            Some("text") => {
                self.flush_pending(
                    "error",
                    "Outcome unknown. Verify the project before retrying.",
                );
                let delta = event["delta"].as_str().unwrap_or("");
                if let Some(Value::String(text)) = self
                    .messages
                    .last_mut()
                    .filter(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
                    .and_then(|m| m.get_mut("content"))
                {
                    text.push_str(delta);
                } else {
                    self.messages
                        .push(json!({"role":"assistant","content":delta}));
                }
            }
            Some("tool_call") if use_tools => {
                // If all previously queued tools already finished, flush the prior turn batch first.
                if !self.pending.is_empty() && self.pending.iter().all(|t| t.finished.is_some()) {
                    self.flush_pending("error", "Outcome unknown");
                }
                if let (Some(id), Some(operation), Some(arguments)) = (
                    event["call_id"].as_str(),
                    event["operation"].as_str(),
                    event["arguments"].as_object(),
                ) {
                    let is_direct = crate::browser::is_browser_tool(operation)
                        || crate::tools::is_computer_operation(operation);
                    let (name, args) = if is_direct {
                        (operation.to_owned(), Value::Object(arguments.clone()))
                    } else {
                        (
                            "project_tool".to_owned(),
                            json!({"operation":operation,"arguments":arguments}),
                        )
                    };
                    self.pending.push(Tool {
                        id: id.to_owned(),
                        name,
                        arguments: args,
                        output: String::new(),
                        finished: None,
                    });
                }
            }
            Some("todo") if use_tools => {
                if let (Some(sequence), Some(todos)) =
                    (event["sequence"].as_i64(), event["todos"].as_array())
                {
                    self.flush_pending(
                        "error",
                        "Outcome unknown. Verify the project before retrying.",
                    );
                    self.pending.push(Tool {
                        id: format!("todo_{sequence}"),
                        name: "update_tasks".to_owned(),
                        arguments: json!({"todos":todos}),
                        output: "Task list updated".into(),
                        finished: Some(("ok".into(), String::new())),
                    });
                    self.flush_pending("ok", "");
                }
            }
            Some("tool_output") if use_tools => {
                if let Some(call_id) = event["call_id"].as_str()
                    && let Some(tool) = self.pending.iter_mut().find(|t| t.id == call_id)
                    && let Some(data) = event["data"].as_str()
                {
                    append_output(&mut tool.output, data);
                }
            }
            Some("tool_result") if use_tools => {
                if let Some(call_id) = event["call_id"].as_str()
                    && let Some(tool) = self.pending.iter_mut().find(|t| t.id == call_id)
                {
                    tool.finished = Some((
                        event["status"].as_str().unwrap_or("error").to_owned(),
                        event["summary"]
                            .as_str()
                            .unwrap_or("Outcome unknown")
                            .to_owned(),
                    ));
                    if self.pending.iter().all(|t| t.finished.is_some()) {
                        self.flush_pending("error", "Outcome unknown");
                    }
                }
            }
            _ => {}
        }
    }

    fn flush_pending(&mut self, default_status: &str, default_summary: &str) {
        if self.pending.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending);
        let mut calls = Vec::with_capacity(batch.len());
        let mut results = Vec::with_capacity(batch.len());
        for tool in batch {
            let id = format!("call_{}", tool.id);
            let arguments = tool.arguments.to_string();
            calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": tool.name, "arguments": arguments}
            }));
            let (status, summary) = tool
                .finished
                .as_ref()
                .map(|(s, m)| (s.as_str(), m.as_str()))
                .unwrap_or((default_status, default_summary));
            let output = tool_content(status, summary, &tool.output);
            results.push(json!({"role":"tool","tool_call_id":id,"content":output}));
        }
        if let Some(last) = self
            .messages
            .last_mut()
            .filter(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
        {
            last["tool_calls"] = Value::Array(calls);
        } else {
            self.messages
                .push(json!({"role":"assistant","content":"","tool_calls":calls}));
        }
        self.messages.extend(results);
    }

    pub(super) fn into_messages(mut self) -> Vec<Value> {
        self.flush_pending(
            "error",
            "Outcome unknown. Verify the project before retrying.",
        );
        self.messages
    }
}

/// Zero-allocation byte counter for JSON values (inspired by pi_agent_rust/src/compaction.rs).
struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn estimate_tokens(messages: &[Value]) -> usize {
    let mut counter = ByteCounter(0);
    for msg in messages {
        let _ = serde_json::to_writer(&mut counter, msg);
    }
    // Conservative ~3.5 bytes per token estimate + per-message framing overhead.
    (counter.0 / 3) + messages.len() * 8
}

pub(super) fn context_budget_tokens() -> usize {
    let limit = std::env::var("VENTICODE_CONTEXT_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 8_000)
        .unwrap_or(DEFAULT_CONTEXT_TOKENS);
    // Keep 20% headroom for tool schemas and model output.
    limit.saturating_mul(4) / 5
}

/// Rescale the byte-estimate budget by the provider's reported prompt tokens for the
/// messages last sent (pi sizes context from real usage, not only estimates).
pub(super) fn calibrated_budget(
    budget: usize,
    messages: &[Value],
    last: Option<(usize, u64)>,
) -> usize {
    match last {
        Some((len, tokens)) if tokens > 0 && len <= messages.len() => {
            let ratio = (estimate_tokens(&messages[..len]) as f64 / tokens as f64).clamp(0.25, 4.0);
            (budget as f64 * ratio) as usize
        }
        _ => budget,
    }
}

pub(super) const COMPACTION_PREFIX: &str = "[Context compacted:";

/// Two-tier context compaction (inspired by pi_agent_rust's `ShakeFirst` + sliding window):
/// 1. Stub out bulky old `role: "tool"` outputs (keeping the most recent messages verbatim).
/// 2. If still over budget, drop oldest complete turns without splitting `tool_calls` from their
///    matching `tool` results, inserting a compaction notice after the system message.
///
/// Returns `None` when nothing changed, otherwise the dropped messages (empty after a shake
/// only). The notice lists the files the dropped turns read and modified; callers may replace
/// it with [`summarize`]'s model-written summary.
pub(super) fn compact_messages(
    messages: &mut Vec<Value>,
    budget_tokens: usize,
) -> Option<Vec<Value>> {
    if estimate_tokens(messages) <= budget_tokens {
        return None;
    }
    let mut compacted = false;
    let keep_from = messages.len().saturating_sub(SHAKE_TOOL_KEEP_RECENT);
    for (idx, msg) in messages.iter_mut().enumerate() {
        if idx == 0 || idx >= keep_from {
            continue;
        }
        if msg["role"] == "tool"
            && let Some(content) = msg["content"].as_str()
            && content.len() > SHAKE_TOOL_THRESHOLD_BYTES
        {
            let head_end = content.floor_char_boundary(256);
            let tail_start = content.ceil_char_boundary(content.len().saturating_sub(192));
            let stub = format!(
                "{}\n... [compacted {} bytes of earlier tool output] ...\n{}",
                &content[..head_end],
                content.len(),
                &content[tail_start..]
            );
            msg["content"] = Value::String(stub);
            compacted = true;
        }
    }
    if estimate_tokens(messages) <= budget_tokens {
        return compacted.then(Vec::new);
    }
    // Tier 2: drop oldest complete turns after index 0 (system) while keeping at least the last turn.
    let start_idx = if messages.first().is_some_and(|m| m["role"] == "system") {
        1
    } else {
        0
    };
    let mut dropped = Vec::new();
    while estimate_tokens(messages) > budget_tokens && messages.len() > start_idx + 4 {
        // Identify the end of the oldest atomic turn starting at `start_idx`:
        // an assistant message with `tool_calls` must stay grouped with its immediately following `tool` messages.
        let mut drop_end = start_idx + 1;
        if messages[start_idx]["role"] == "assistant"
            && messages[start_idx].get("tool_calls").is_some()
        {
            while drop_end < messages.len() && messages[drop_end]["role"] == "tool" {
                drop_end += 1;
            }
        }
        // Never drop everything; keep at least the final 2 messages.
        if drop_end >= messages.len().saturating_sub(2) {
            break;
        }
        dropped.extend(messages.drain(start_idx..drop_end));
    }
    if !dropped.is_empty() {
        messages.insert(
            start_idx,
            json!({"role":"user","content":compaction_notice(&dropped, None)}),
        );
    }
    (compacted || !dropped.is_empty()).then_some(dropped)
}

/// Notice for dropped messages: the optional summary plus pi's `<read-files>` /
/// `<modified-files>` lists, carried forward from an earlier notice that was dropped too.
pub(super) fn compaction_notice(dropped: &[Value], summary: Option<&str>) -> String {
    let (read, modified) = file_operations(dropped);
    let mut out = format!(
        "{COMPACTION_PREFIX} {} earlier messages omitted to stay within the model context window.]",
        dropped.len()
    );
    if let Some(summary) = summary {
        out.push_str("\n\n");
        out.push_str(summary.trim());
    }
    for (tag, files) in [("read-files", read), ("modified-files", modified)] {
        if !files.is_empty() {
            out.push_str(&format!(
                "\n\n<{tag}>\n{}\n</{tag}>",
                files.into_iter().collect::<Vec<_>>().join("\n")
            ));
        }
    }
    out
}

/// Files read and modified by the dropped tool calls (pi's `FileOperations`).
fn file_operations(dropped: &[Value]) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut read, mut modified) = (BTreeSet::new(), BTreeSet::new());
    for msg in dropped {
        if let Some(previous) = msg["content"]
            .as_str()
            .filter(|c| c.starts_with(COMPACTION_PREFIX))
        {
            for (tag, set) in [("read-files", &mut read), ("modified-files", &mut modified)] {
                if let Some((_, rest)) = previous.split_once(&format!("<{tag}>\n"))
                    && let Some((list, _)) = rest.split_once(&format!("\n</{tag}>"))
                {
                    set.extend(list.lines().map(str::to_owned));
                }
            }
        }
        for call in msg["tool_calls"].as_array().into_iter().flatten() {
            let Ok(args) = serde_json::from_str::<Value>(
                call["function"]["arguments"].as_str().unwrap_or("{}"),
            ) else {
                continue;
            };
            let (op, args) = match call["function"]["name"].as_str().unwrap_or_default() {
                "project_tool" => (
                    args["operation"].as_str().unwrap_or_default().to_owned(),
                    args["arguments"].clone(),
                ),
                "read" => ("read_file".into(), args),
                "edit" => ("edit_file".into(), args),
                "write" => ("write_file".into(), args),
                _ => continue,
            };
            let set = match op.as_str() {
                "read_file" => &mut read,
                "edit_file" | "write_file" | "mkdir" | "remove" | "rename" => &mut modified,
                _ => continue,
            };
            for key in ["path", "destination"] {
                if let Some(path) = args[key].as_str().filter(|p| !p.is_empty()) {
                    set.insert(path.to_owned());
                }
            }
        }
    }
    // A modified file was necessarily read in its latest form; list it once.
    read.retain(|p| !modified.contains(p));
    (read, modified)
}

const SUMMARY_SYSTEM: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";
const SUMMARY_FORMAT: &str = "Use this EXACT format:\n\n## Goal\n[What is the user trying to accomplish?]\n\n## Constraints & Preferences\n- [Constraints, preferences, or requirements the user mentioned, or \"(none)\"]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Data, examples, or references needed to continue, or \"(none)\"]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";
const SUMMARY_TRANSCRIPT_BYTES: usize = 200 * 1024;

/// Ask the model for pi's structured checkpoint summary of the dropped messages,
/// updating the previous summary when an earlier notice is among them. `None` on any failure.
pub(super) async fn summarize(
    app: &crate::App,
    h: &axum::http::HeaderMap,
    model: &str,
    dropped: &[Value],
    vault: &mut crate::secrets::Vault,
) -> Option<String> {
    use futures_util::StreamExt;
    let mut previous = None;
    let mut transcript = String::new();
    let clip = |s: &str, n: usize| s[..s.floor_char_boundary(n)].to_owned();
    for msg in dropped {
        let content = msg["content"].as_str().unwrap_or_default();
        match msg["role"].as_str() {
            Some("user") if content.starts_with(COMPACTION_PREFIX) => previous = Some(content),
            Some("user") => transcript.push_str(&format!("[User]: {content}\n\n")),
            Some("assistant") => {
                if !content.is_empty() {
                    transcript.push_str(&format!("[Assistant]: {content}\n\n"));
                }
                for call in msg["tool_calls"].as_array().into_iter().flatten() {
                    let args = call["function"]["arguments"].as_str().unwrap_or_default();
                    transcript.push_str(&format!(
                        "[Assistant tool call]: {}({})\n\n",
                        call["function"]["name"].as_str().unwrap_or_default(),
                        clip(args, 1024)
                    ));
                }
            }
            Some("tool") => {
                transcript.push_str(&format!("[Tool result]: {}\n\n", clip(content, 2048)))
            }
            _ => {}
        }
    }
    if transcript.len() > SUMMARY_TRANSCRIPT_BYTES {
        let start = transcript.ceil_char_boundary(transcript.len() - SUMMARY_TRANSCRIPT_BYTES);
        transcript.drain(..start);
    }
    let instruction = match previous {
        Some(previous) => format!(
            "<previous-summary>\n{previous}\n</previous-summary>\n\nThe messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags. Update it: preserve existing information, add new progress and decisions, move completed items to Done, and update Next Steps.\n\n{SUMMARY_FORMAT}"
        ),
        None => format!(
            "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\n{SUMMARY_FORMAT}"
        ),
    };
    let mut request = json!({"model":model,"stream":true,"max_tokens":4096,"messages":[
        {"role":"system","content":SUMMARY_SYSTEM},
        {"role":"user","content":format!("<conversation>\n{transcript}</conversation>\n\n{instruction}")}
    ]});
    vault.obfuscate(app, &mut request["messages"]);
    let collect = async {
        let mut stream = crate::inference::infer(app, h, &request).await.ok()?;
        let mut summary = String::new();
        while let Some(data) = stream.next().await {
            let data = data.ok()?;
            if data == "[DONE]" {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&data)
                && let Some(delta) = v["choices"][0]["delta"]["content"].as_str()
            {
                summary.push_str(delta);
            }
        }
        Some(summary)
    };
    let summary = tokio::time::timeout(std::time::Duration::from_secs(120), collect)
        .await
        .ok()
        .flatten()?;
    (!summary.trim().is_empty()).then_some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_completed_failed_and_interrupted_tools_in_order() {
        let mut history = History::default();
        for event in [
            json!({"type":"user_message","content":"Inspect the project"}),
            json!({"type":"text","delta":"I will inspect it. "}),
            json!({"type":"text","delta":"First the README."}),
            json!({"type":"tool_call","call_id":"1","operation":"read_file","arguments":{"path":"README"}}),
            json!({"type":"tool_output","call_id":"1","data":"Retained README"}),
            json!({"type":"tool_result","call_id":"1","status":"ok"}),
            json!({"type":"tool_call","call_id":"2","operation":"write_file","arguments":{"path":"README"}}),
            json!({"type":"tool_result","call_id":"2","status":"denied"}),
            json!({"type":"tool_call","call_id":"3","operation":"execute","arguments":{}}),
            json!({"type":"tool_output","call_id":"3","data":"Partial output"}),
            json!({"type":"user_message","content":"Continue from there"}),
        ] {
            history.push(event, true);
        }
        let messages = history.into_messages();
        assert_eq!(
            messages[1]["content"],
            "I will inspect it. First the README."
        );
        for (assistant, tool) in [(1, 2), (3, 4), (5, 6)] {
            assert_eq!(
                messages[assistant]["tool_calls"][0]["id"],
                messages[tool]["tool_call_id"]
            );
        }
        assert_eq!(messages[2]["content"], "Retained README");
        assert_eq!(messages[4]["content"], "Operation denied");
        assert!(
            messages[6]["content"]
                .as_str()
                .unwrap()
                .contains("Verify the project before retrying")
        );
        assert_eq!(messages[7]["content"], "Continue from there");
    }

    #[test]
    fn replays_parallel_tool_batches_together() {
        let mut history = History::default();
        for event in [
            json!({"type":"user_message","content":"Read two files"}),
            json!({"type":"tool_call","call_id":"a","operation":"read_file","arguments":{"path":"A"}}),
            json!({"type":"tool_call","call_id":"b","operation":"read_file","arguments":{"path":"B"}}),
            json!({"type":"tool_output","call_id":"b","data":"Body B"}),
            json!({"type":"tool_output","call_id":"a","data":"Body A"}),
            json!({"type":"tool_result","call_id":"b","status":"ok"}),
            json!({"type":"tool_result","call_id":"a","status":"ok"}),
        ] {
            history.push(event, true);
        }
        let messages = history.into_messages();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_a");
        assert_eq!(messages[1]["tool_calls"][1]["id"], "call_b");
        assert_eq!(messages[2]["tool_call_id"], "call_a");
        assert_eq!(messages[2]["content"], "Body A");
        assert_eq!(messages[3]["tool_call_id"], "call_b");
        assert_eq!(messages[3]["content"], "Body B");
    }

    #[test]
    fn compact_messages_shakes_old_tool_outputs_before_dropping_turns() {
        let big = "x".repeat(20_000);
        let mut messages = vec![
            json!({"role":"system","content":"sys"}),
            json!({"role":"user","content":"first"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":big}),
            json!({"role":"assistant","content":"done 1"}),
            json!({"role":"user","content":"second"}),
            json!({"role":"assistant","content":"done 2"}),
            json!({"role":"user","content":"third"}),
            json!({"role":"assistant","content":"done 3"}),
            json!({"role":"user","content":"fourth"}),
        ];
        let changed = compact_messages(&mut messages, 2_000);
        assert!(changed.is_some());
        assert!(
            messages[3]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted 20000 bytes")
        );
    }

    #[test]
    fn dropped_turns_list_file_operations_and_carry_them_forward() {
        let call = |name: &str, args: Value| json!({"role":"assistant","content":"","tool_calls":[{"id":"c","type":"function","function":{"name":name,"arguments":args.to_string()}}]});
        let mut messages = vec![json!({"role":"system","content":"sys"})];
        messages.push(json!({"role":"user","content":"x".repeat(9_000)}));
        messages.push(call("read", json!({"path":"src/a.rs"})));
        messages.push(json!({"role":"tool","tool_call_id":"c","content":"ok"}));
        messages.push(call(
            "project_tool",
            json!({"operation":"rename","arguments":{"path":"old.rs","destination":"new.rs"}}),
        ));
        messages.push(json!({"role":"tool","tool_call_id":"c","content":"ok"}));
        messages.push(call(
            "edit",
            json!({"path":"src/a.rs","oldText":"a","newText":"b"}),
        ));
        messages.push(json!({"role":"tool","tool_call_id":"c","content":"ok"}));
        for text in ["recent 1", "recent 2", "recent 3", "recent 4"] {
            messages.push(json!({"role":"user","content":text}));
        }
        let dropped = compact_messages(&mut messages, 50).unwrap();
        assert!(!dropped.is_empty());
        let notice = messages[1]["content"].as_str().unwrap();
        assert!(notice.starts_with(COMPACTION_PREFIX));
        assert!(notice.contains("<modified-files>\nnew.rs\nold.rs\nsrc/a.rs\n</modified-files>"));
        assert!(!notice.contains("<read-files>"));
        // A later compaction that drops this notice keeps its lists.
        let again = compaction_notice(
            &[messages[1].clone(), call("read", json!({"path":"b.md"}))],
            Some("## Goal\nx"),
        );
        assert!(again.contains("## Goal\nx"));
        assert!(again.contains("<read-files>\nb.md\n</read-files>"));
        assert!(again.contains("src/a.rs"));
        assert_eq!(messages.last().unwrap()["content"], "recent 4");
    }

    #[test]
    fn budget_follows_provider_token_counts() {
        let messages = vec![json!({"role":"user","content":"x".repeat(3_000)})];
        let estimated = estimate_tokens(&messages) as u64;
        assert_eq!(calibrated_budget(1000, &messages, None), 1000);
        // The provider counted twice the estimate: the budget in estimate units halves.
        assert_eq!(
            calibrated_budget(1000, &messages, Some((1, estimated * 2))),
            500
        );
        assert_eq!(
            calibrated_budget(1000, &messages, Some((2, estimated))),
            1000
        );
    }

    #[test]
    fn output_is_bounded_in_utf8_bytes() {
        let mut output = String::new();
        append_output(&mut output, &"é".repeat(TOOL_OUTPUT_LIMIT));
        append_output(&mut output, "ignored");
        assert_eq!(output.len(), TOOL_OUTPUT_LIMIT);
        assert!(output.ends_with('é'));
    }

    #[test]
    fn direct_browser_and_computer_tools_keep_their_declared_names() {
        let mut history = History::default();
        for event in [
            json!({"type":"user_message","content":"Open the page"}),
            json!({"type":"tool_call","call_id":"b1","operation":"browser_open","arguments":{"url":"https://example.test"}}),
            json!({"type":"tool_output","call_id":"b1","data":"{\"navigation\":\"started\"}"}),
            json!({"type":"tool_result","call_id":"b1","status":"ok"}),
            json!({"type":"tool_call","call_id":"d1","operation":"desktop_screenshot","arguments":{}}),
            json!({"type":"goal_cancelled"}),
        ] {
            history.push(event, true);
        }
        let messages = history.into_messages();
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "browser_open"
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                messages[1]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            json!({"url":"https://example.test"})
        );
        assert_eq!(
            messages[3]["tool_calls"][0]["function"]["name"],
            "desktop_screenshot"
        );
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[5]["role"], "user");
    }
}
