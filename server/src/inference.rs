use super::*;
use futures_util::{Stream, StreamExt};
use std::{collections::HashMap, pin::Pin};

pub(super) struct Provider {
    protocol: Protocol,
    endpoint: reqwest::Url,
    key: Option<String>,
    model: String,
    max_tokens: u32,
    tools: bool,
    vision: bool,
}

#[derive(Clone, Copy)]
enum Protocol {
    OpenAi,
    Anthropic,
}

impl Provider {
    pub fn from_env() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let protocol = match env::var("VENTICODE_LLM_PROVIDER")?.as_str() {
            "openai" => Protocol::OpenAi,
            "anthropic" => Protocol::Anthropic,
            _ => return Err("Set VENTICODE_LLM_PROVIDER to openai or anthropic".into()),
        };
        let (default_base, suffix) = match protocol {
            Protocol::OpenAi => ("https://api.openai.com/v1", "chat/completions"),
            Protocol::Anthropic => ("https://api.anthropic.com/v1", "messages"),
        };
        let base = env::var("VENTICODE_LLM_BASE_URL").unwrap_or_else(|_| default_base.into());
        let endpoint = reqwest::Url::parse(&format!("{}/{suffix}", base.trim_end_matches('/')))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !endpoint.path().ends_with(suffix)
        {
            return Err("Invalid VENTICODE_LLM_BASE_URL".into());
        }
        let key = env::var("VENTICODE_LLM_API_KEY")
            .ok()
            .filter(|v| !v.is_empty());
        if (base == default_base || matches!(protocol, Protocol::Anthropic)) && key.is_none() {
            return Err("Set VENTICODE_LLM_API_KEY".into());
        }
        let model = env::var("VENTICODE_MODEL")?;
        let max_tokens =
            env::var("VENTICODE_LLM_MAX_TOKENS").map_or(Ok(4096), |v| v.parse::<u32>())?;
        let tools = env::var("VENTICODE_LLM_TOOLS").map_or(Ok(true), |v| v.parse::<bool>())?;
        let vision =
            env::var("VENTICODE_LLM_VISION").map_or(Ok(false), |value| value.parse::<bool>())?;
        if model.trim().is_empty() || max_tokens == 0 {
            return Err("Configure a model and a positive VENTICODE_LLM_MAX_TOKENS".into());
        }
        Ok(Self {
            protocol,
            endpoint,
            key,
            model,
            max_tokens,
            tools,
            vision,
        })
    }

    pub fn models(&self) -> Value {
        json!({"items":[{"id":self.model,"name":self.model,"default":true,"tools":self.tools,"vision":self.vision}]})
    }
}

type Events = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

pub(super) async fn infer(app: &App, h: &HeaderMap, request: &Value) -> Result<Events> {
    let (res, protocol) = if let Some(local) = &app.standalone {
        let p = &local.provider;
        let mut builder = app.http.post(p.endpoint.clone());
        let body = match p.protocol {
            Protocol::OpenAi => {
                if let Some(key) = &p.key {
                    builder = builder.bearer_auth(key);
                }
                let mut body = request.clone();
                if p.endpoint.host_str() != Some("api.openai.com") {
                    body.as_object_mut()
                        .ok_or_else(bad)?
                        .remove("parallel_tool_calls");
                    body.as_object_mut()
                        .ok_or_else(bad)?
                        .remove("stream_options");
                }
                body
            }
            Protocol::Anthropic => {
                builder = builder
                    .header("x-api-key", p.key.as_deref().ok_or_else(unavailable)?)
                    .header("anthropic-version", "2023-06-01");
                anthropic_request(request, p.max_tokens)?
            }
        };
        (
            builder
                .json(&body)
                .send()
                .await
                .map_err(|_| unavailable())?,
            p.protocol,
        )
    } else {
        (
            app.control_request(h, Method::POST, "infer")?
                .json(request)
                .send()
                .await
                .map_err(|_| unavailable())?,
            Protocol::OpenAi,
        )
    };
    if !res.status().is_success() {
        let status = res.status();
        let message = if app.standalone.is_some() {
            "Provider LLM non disponibile / LLM provider unavailable".to_owned()
        } else {
            let body: Value = res.json().await.unwrap_or_default();
            body["error"]["message"]
                .as_str()
                .unwrap_or("Gateway unavailable")
                .to_owned()
        };
        return Err(Error(status, message));
    }
    let stream = async_stream::try_stream! {
        let mut upstream = sse_stream::SseByteStream::new(res.bytes_stream());
        let mut translation = AnthropicStream::default();
        while let Some(frame) = upstream.next().await {
            let Some(data) = frame.map_err(|_| unavailable())?.data else { continue };
            match protocol {
                Protocol::OpenAi => yield data,
                Protocol::Anthropic => {
                    let value: Value = serde_json::from_str(&data).map_err(|_| unavailable())?;
                    if let Some(frame) = translation.translate(&value)? { yield frame; }
                }
            }
        }
    };
    Ok(Box::pin(stream))
}

fn anthropic_request(request: &Value, max_tokens: u32) -> Result<Value> {
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for msg in request["messages"].as_array().ok_or_else(bad)? {
        let role = msg["role"].as_str().ok_or_else(bad)?;
        let text = msg["content"].as_str().unwrap_or_default();
        if role == "system" {
            system.push(text);
            continue;
        }
        let mut blocks = Vec::new();
        if role == "tool" {
            blocks.push(
                json!({"type":"tool_result","tool_use_id":msg["tool_call_id"],"content":text}),
            );
        } else if let Some(parts) = msg["content"].as_array() {
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => blocks
                        .push(json!({"type":"text","text":part["text"].as_str().ok_or_else(bad)?})),
                    Some("image_url") => {
                        let data = part["image_url"]["url"]
                            .as_str()
                            .and_then(|url| url.strip_prefix("data:image/jpeg;base64,"))
                            .ok_or_else(bad)?;
                        blocks.push(json!({"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":data}}));
                    }
                    _ => return Err(bad()),
                }
            }
        } else if !text.is_empty() {
            blocks.push(json!({"type":"text","text":text}));
        }
        if let Some(calls) = msg["tool_calls"].as_array() {
            for call in calls {
                let input: Value =
                    serde_json::from_str(call["function"]["arguments"].as_str().ok_or_else(bad)?)
                        .map_err(|_| bad())?;
                blocks.push(json!({"type":"tool_use","id":call["id"],"name":call["function"]["name"],"input":input}));
            }
        }
        if blocks.is_empty() {
            continue;
        }
        let role = if role == "tool" { "user" } else { role };
        if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
            last["content"]
                .as_array_mut()
                .ok_or_else(bad)?
                .extend(blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    let max_tokens = request["max_tokens"]
        .as_u64()
        .map_or(u64::from(max_tokens), |limit| {
            limit.min(u64::from(max_tokens))
        });
    let mut body = json!({"model":request["model"],"max_tokens":max_tokens,"stream":true,"system":system.join("\n\n"),"messages":messages});
    if let Some(tools) = request["tools"]
        .as_array()
        .filter(|tools| !tools.is_empty())
    {
        body["tools"] = json!(tools.iter().map(|tool| {
            let function = &tool["function"];
            json!({"name":function["name"],"description":function["description"],"input_schema":function["parameters"]})
        }).collect::<Vec<_>>());
        body["tool_choice"] = json!({"type":"auto","disable_parallel_tool_use":true});
    }
    Ok(body)
}

#[derive(Default)]
struct AnthropicStream {
    input_tokens: Option<u64>,
    next_tool: usize,
    open_tools: HashMap<u64, (usize, bool, Value)>,
    reason_seen: bool,
}

impl AnthropicStream {
    fn translate(&mut self, event: &Value) -> Result<Option<String>> {
        let index = || event["index"].as_u64().ok_or_else(unavailable);
        let frame = match event["type"].as_str() {
            Some("message_start") => {
                self.input_tokens = event["message"]["usage"]["input_tokens"].as_u64();
                return Ok(None);
            }
            Some("content_block_start") if event["content_block"]["type"] == "tool_use" => {
                let tool = &event["content_block"];
                let number = self.next_tool;
                self.next_tool += 1;
                if number >= 16
                    || self
                        .open_tools
                        .insert(index()?, (number, false, tool["input"].clone()))
                        .is_some()
                {
                    return Err(bad());
                }
                json!({"choices":[{"delta":{"tool_calls":[{"index":number,"id":tool["id"],"function":{"name":tool["name"]}}]}}]})
            }
            Some("content_block_start") if event["content_block"]["type"] == "text" => {
                let text = &event["content_block"]["text"];
                if text.as_str().is_none_or(str::is_empty) {
                    return Ok(None);
                }
                json!({"choices":[{"delta":{"content":text}}]})
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => json!({"choices":[{"delta":{"content":delta["text"]}}]}),
                    Some("input_json_delta") => {
                        let (number, fragments, _) =
                            self.open_tools.get_mut(&index()?).ok_or_else(unavailable)?;
                        *fragments = true;
                        json!({"choices":[{"delta":{"tool_calls":[{"index":number,"function":{"arguments":delta["partial_json"]}}]}}]})
                    }
                    _ => return Ok(None),
                }
            }
            Some("content_block_stop") => {
                let Some((number, false, input)) = self.open_tools.remove(&index()?) else {
                    return Ok(None);
                };
                json!({"choices":[{"delta":{"tool_calls":[{"index":number,"function":{"arguments":input.to_string()}}]}}]})
            }
            Some("message_delta") => {
                let choices = match event["delta"]["stop_reason"].as_str() {
                    Some("max_tokens" | "model_context_window_exceeded") => {
                        json!([{"finish_reason":"length"}])
                    }
                    Some("tool_use") => json!([{"finish_reason":"tool_calls"}]),
                    Some("end_turn" | "stop_sequence" | "refusal") => {
                        json!([{"finish_reason":"stop"}])
                    }
                    None => json!([]),
                    _ => return Err(unavailable()),
                };
                self.reason_seen |= event["delta"]["stop_reason"].is_string();
                self.input_tokens = event["usage"]["input_tokens"]
                    .as_u64()
                    .or(self.input_tokens);
                json!({"choices":choices,"usage":{
                    "prompt_tokens":self.input_tokens,
                    "completion_tokens":event["usage"]["output_tokens"]
                }})
            }
            Some("message_stop") if self.reason_seen && self.open_tools.is_empty() => {
                return Ok(Some("[DONE]".into()));
            }
            Some("message_stop" | "error") => return Err(unavailable()),
            _ => return Ok(None),
        };
        Ok(Some(frame.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_requests_group_tool_results_and_preserve_their_ids() {
        let request = json!({"model":"fixture","messages":[
            {"role":"system","content":"System instructions"},
            {"role":"user","content":"Read the file"},
            {"role":"assistant","content":"Reading","tool_calls":[
                {"id":"tool_1","function":{"name":"read","arguments":"{\"path\":\"README\"}"}},
                {"id":"tool_2","function":{"name":"ls","arguments":"{}"}}
            ]},
            {"role":"tool","tool_call_id":"tool_1","content":"File"},
            {"role":"tool","tool_call_id":"tool_2","content":"Files"},
            {"role":"user","content":"Summarize"}
        ],"tools":[{"function":{"name":"read","description":"Read","parameters":{"type":"object"}}}]});
        let translated = anthropic_request(&request, 512).unwrap();
        assert_eq!(translated["system"], "System instructions");
        assert_eq!(translated["max_tokens"], 512);
        assert_eq!(
            translated["tools"][0]["input_schema"],
            json!({"type":"object"})
        );
        assert_eq!(
            translated["messages"][1]["content"][1]["input"]["path"],
            "README"
        );
        assert_eq!(
            translated["messages"][2]["content"][0]["tool_use_id"],
            "tool_1"
        );
        assert_eq!(
            translated["messages"][2]["content"][1]["tool_use_id"],
            "tool_2"
        );
        assert_eq!(translated["messages"][2]["content"][2]["text"], "Summarize");
    }

    #[test]
    fn anthropic_stream_uses_native_tool_blocks_and_requires_a_closed_turn() {
        let mut stream = AnthropicStream::default();
        assert!(stream.translate(&json!({"type":"message_stop"})).is_err());
        let frames = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":12}}}),
            json!({"type":"content_block_start","index":22,"content_block":{"type":"tool_use","id":"call","name":"ls","input":{}}}),
            json!({"type":"content_block_delta","index":22,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":22,"delta":{"type":"input_json_delta","partial_json":"\".\"}"}}),
            json!({"type":"content_block_stop","index":22}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}),
        ];
        let chunks: Vec<Value> = frames
            .iter()
            .filter_map(|frame| stream.translate(frame).unwrap())
            .map(|frame| serde_json::from_str(&frame).unwrap())
            .collect();
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            0
        );
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call"
        );
        let input: String = chunks[1..3]
            .iter()
            .map(|chunk| {
                chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&input).unwrap(),
            json!({"path":"."})
        );
        assert_eq!(
            chunks[3]["usage"],
            json!({"prompt_tokens":12,"completion_tokens":8})
        );
        assert_eq!(
            stream
                .translate(&json!({"type":"message_stop"}))
                .unwrap()
                .as_deref(),
            Some("[DONE]")
        );
    }
}
