use serde_json::{Value, json};

const TOOL_OUTPUT_LIMIT: usize = 65_536;

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
}

#[derive(Default)]
pub(super) struct History {
    messages: Vec<Value>,
    pending: Option<Tool>,
}

impl History {
    pub(super) fn push(&mut self, event: Value, use_tools: bool) {
        match event["type"].as_str() {
            Some("user_message") => {
                self.finish_pending(
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
                self.finish_pending("error", "Outcome unknown");
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
                self.finish_pending(
                    "error",
                    "Outcome unknown. Verify the project before retrying.",
                );
                self.messages
                    .push(json!({"role":"user","content":"The previous goal was cancelled."}));
            }
            Some("text") => {
                if let Some(Value::String(text)) = self
                    .messages
                    .last_mut()
                    .filter(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
                    .and_then(|m| m.get_mut("content"))
                {
                    text.push_str(event["delta"].as_str().unwrap_or(""));
                } else {
                    self.messages
                        .push(json!({"role":"assistant","content":event["delta"]}));
                }
            }
            Some("tool_call") if use_tools => {
                self.finish_pending(
                    "error",
                    "Outcome unknown. Verify the project before retrying.",
                );
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
                    self.pending = Some(Tool {
                        id: id.to_owned(),
                        name,
                        arguments: args,
                        output: String::new(),
                    });
                }
            }
            Some("todo") if use_tools => {
                if let (Some(sequence), Some(todos)) =
                    (event["sequence"].as_i64(), event["todos"].as_array())
                {
                    self.finish_pending(
                        "error",
                        "Outcome unknown. Verify the project before retrying.",
                    );
                    self.pending = Some(Tool {
                        id: format!("todo_{sequence}"),
                        name: "update_tasks".to_owned(),
                        arguments: json!({"todos":todos}),
                        output: "Task list updated".into(),
                    });
                    self.finish_pending("ok", "");
                }
            }
            Some("tool_output") if use_tools => {
                if let Some(tool) = self
                    .pending
                    .as_mut()
                    .filter(|tool| event["call_id"] == tool.id)
                    && let Some(data) = event["data"].as_str()
                {
                    append_output(&mut tool.output, data);
                }
            }
            Some("tool_result")
                if use_tools
                    && self
                        .pending
                        .as_ref()
                        .is_some_and(|tool| event["call_id"] == tool.id) =>
            {
                self.finish_pending(
                    event["status"].as_str().unwrap_or("error"),
                    event["summary"].as_str().unwrap_or("Outcome unknown"),
                );
            }
            _ => {}
        }
    }

    fn finish_pending(&mut self, status: &str, summary: &str) {
        if let Some(tool) = self.pending.take() {
            let id = format!("call_{}", tool.id);
            let arguments = tool.arguments.to_string();
            let call = json!({"id":id,"type":"function","function":{"name":tool.name,"arguments":arguments}});
            if let Some(last) = self
                .messages
                .last_mut()
                .filter(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
            {
                last["tool_calls"] = json!([call]);
            } else {
                self.messages
                    .push(json!({"role":"assistant","content":"","tool_calls":[call]}));
            }
            let output = tool_content(status, summary, &tool.output);
            self.messages
                .push(json!({"role":"tool","tool_call_id":id,"content":output}));
        }
    }

    pub(super) fn into_messages(mut self) -> Vec<Value> {
        self.finish_pending(
            "error",
            "Outcome unknown. Verify the project before retrying.",
        );
        self.messages
    }
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
