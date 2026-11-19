use serde_json::{Value, json};
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

/// Two-tier context compaction (inspired by pi_agent_rust's `ShakeFirst` + sliding window):
/// 1. Stub out bulky old `role: "tool"` outputs (keeping the most recent messages verbatim).
/// 2. If still over budget, drop oldest complete turns without splitting `tool_calls` from their
///    matching `tool` results, inserting a clean compaction notice after the system message.
pub(super) fn compact_messages(messages: &mut Vec<Value>, budget_tokens: usize) -> bool {
    if estimate_tokens(messages) <= budget_tokens {
        return false;
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
        return compacted;
    }
    // Tier 2: drop oldest complete turns after index 0 (system) while keeping at least the last turn.
    let start_idx = if messages.first().is_some_and(|m| m["role"] == "system") {
        1
    } else {
        0
    };
    let mut dropped_messages = 0usize;
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
        dropped_messages += drop_end - start_idx;
        messages.drain(start_idx..drop_end);
        compacted = true;
    }
    if dropped_messages > 0 {
        messages.insert(
            start_idx,
            json!({
                "role": "user",
                "content": format!("[Context compacted: {dropped_messages} earlier messages omitted to stay within the model context window.]")
            }),
        );
    }
    compacted
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
        assert!(changed);
        assert!(
            messages[3]["content"]
                .as_str()
                .unwrap()
                .contains("[compacted 20000 bytes")
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
