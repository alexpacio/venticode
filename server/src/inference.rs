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
        let protocol = match require_env("VENTICODE_LLM_PROVIDER")?.as_str() {
            "openai" => Protocol::OpenAi,
            "anthropic" => Protocol::Anthropic,
            _ => return Err("Set VENTICODE_LLM_PROVIDER to openai or anthropic".into()),
        };
        let (default_base, suffix) = match protocol {
            Protocol::OpenAi => ("https://api.openai.com/v1", "chat/completions"),
            Protocol::Anthropic => ("https://api.anthropic.com/v1", "messages"),
        };
        let base = env::var("VENTICODE_LLM_BASE_URL").unwrap_or_else(|_| default_base.into());
        let trimmed_base = base.trim().trim_end_matches('/');
        if trimmed_base.ends_with(&format!("/{suffix}")) {
            return Err(format!(
                "VENTICODE_LLM_BASE_URL should be the API base (e.g. ending in /v1), without '/{suffix}'"
            )
            .into());
        }
        let endpoint = reqwest::Url::parse(&format!("{trimmed_base}/{suffix}"))
            .map_err(|e| format!("Invalid VENTICODE_LLM_BASE_URL: {e}"))?;
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
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        if (trimmed_base == default_base || matches!(protocol, Protocol::Anthropic))
            && key.is_none()
        {
            return Err("Set VENTICODE_LLM_API_KEY".into());
        }
        let model = require_env("VENTICODE_MODEL")?;
        let max_tokens = env::var("VENTICODE_LLM_MAX_TOKENS")
            .map_or(Ok(8192), |v| v.trim().parse::<u32>())
            .map_err(|_| "VENTICODE_LLM_MAX_TOKENS must be a positive integer")?;
        let tools = env::var("VENTICODE_LLM_TOOLS")
            .map_or(Ok(true), |v| v.trim().parse::<bool>())
            .map_err(|_| "VENTICODE_LLM_TOOLS must be true or false")?;
        let vision = env::var("VENTICODE_LLM_VISION")
            .map_or(Ok(false), |value| value.trim().parse::<bool>())
            .map_err(|_| "VENTICODE_LLM_VISION must be true or false")?;
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

const MAX_RETRIES: u32 = 3;
const TTFB_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

fn retry_delay(attempt: u32, headers: Option<&HeaderMap>) -> Duration {
    if let Some(raw) = headers
        .and_then(|h| h.get(header::RETRY_AFTER))
        .and_then(|v| v.to_str().ok())
    {
        let trimmed = raw.trim();
        if let Ok(secs) = trimmed.parse::<u64>() {
            return Duration::from_secs(secs.clamp(1, 15));
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
            let delta = (dt.timestamp() - chrono::Utc::now().timestamp()).clamp(1, 15) as u64;
            return Duration::from_secs(delta);
        }
    }
    let base_ms = 500u64.saturating_mul(1u64 << attempt.min(4));
    // Deterministic-free lightweight jitter in [0, 300ms) derived from subsec nanos.
    let jitter_ms = u64::from(chrono::Utc::now().timestamp_subsec_nanos() % 300);
    Duration::from_millis((base_ms + jitter_ms).min(10_000))
}

/// Conservative retry classifier (inspired by pi_agent_rust/src/failover.rs):
/// strictly retries transient capacity/network failures; never retries 400/401/403/404/422.
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

async fn read_capped_text(mut res: reqwest::Response, limit: usize) -> String {
    let mut buf = Vec::new();
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout(Duration::from_secs(5), res.chunk()).await
    {
        let remaining = limit.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let take = chunk.len().min(remaining);
        buf.extend_from_slice(&chunk[..take]);
        if buf.len() >= limit {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn classify_standalone_error(status: StatusCode, body: &str) -> Error {
    let lower = body.to_ascii_lowercase();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Error(
            status,
            "Autenticazione provider LLM non valida / Invalid LLM provider credentials".into(),
        );
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Error(
            status,
            "Limite di richieste LLM raggiunto. Riprova tra poco / LLM rate limit reached. Try again shortly".into(),
        );
    }
    if status == StatusCode::BAD_REQUEST
        && (lower.contains("context_length")
            || lower.contains("maximum context")
            || lower.contains("prompt is too long")
            || lower.contains("too many tokens"))
    {
        return Error(
            StatusCode::BAD_REQUEST,
            "Contesto troppo lungo per il modello / Context length exceeded for this model".into(),
        );
    }
    Error(
        status,
        "Provider LLM non disponibile / LLM provider unavailable".into(),
    )
}

pub(super) async fn infer(app: &App, h: &HeaderMap, request: &Value) -> Result<Events> {
    let mut attempt = 0u32;
    let mut strip_openai_extras = env::var("VENTICODE_LLM_LEGACY_COMPAT")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"));
    let (res, protocol) = loop {
        let send_fut = async {
            if let Some(local) = &app.standalone {
                let p = &local.provider;
                let mut builder = app.http.post(p.endpoint.clone());
                let body = match p.protocol {
                    Protocol::OpenAi => {
                        if let Some(key) = &p.key {
                            builder = builder.bearer_auth(key);
                        }
                        let mut body = request.clone();
                        if strip_openai_extras && let Some(obj) = body.as_object_mut() {
                            obj.remove("parallel_tool_calls");
                            obj.remove("stream_options");
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
                Ok::<_, Error>((builder.json(&body).send().await, p.protocol))
            } else {
                Ok((
                    app.control_request(h, Method::POST, "infer")?
                        .json(request)
                        .send()
                        .await,
                    Protocol::OpenAi,
                ))
            }
        };
        let timed = tokio::time::timeout(TTFB_TIMEOUT, send_fut).await;
        match timed {
            Ok(Ok((Ok(res), protocol)))
                if res.status() == StatusCode::BAD_REQUEST
                    && matches!(protocol, Protocol::OpenAi)
                    && app.standalone.is_some()
                    && !strip_openai_extras =>
            {
                let body = read_capped_text(res, MAX_ERROR_BODY_BYTES).await;
                let lower = body.to_ascii_lowercase();
                if lower.contains("stream_options") || lower.contains("parallel_tool_calls") {
                    tracing::info!(
                        "OpenAI-compatible endpoint rejected stream_options/parallel_tool_calls; retrying without them"
                    );
                    strip_openai_extras = true;
                    continue;
                }
                let preview: String = body.chars().take(400).collect();
                tracing::error!(status = 400, body = %preview, "standalone LLM provider returned 400");
                return Err(classify_standalone_error(StatusCode::BAD_REQUEST, &body));
            }
            Ok(Ok((Ok(res), _protocol)))
                if is_retryable_status(res.status()) && attempt < MAX_RETRIES =>
            {
                let status = res.status();
                let wait = retry_delay(attempt, Some(res.headers()));
                tracing::warn!(
                    status = status.as_u16(),
                    attempt = attempt + 1,
                    wait_ms = wait.as_millis() as u64,
                    "retrying transient LLM response"
                );
                attempt += 1;
                tokio::time::sleep(wait).await;
            }
            Ok(Ok((Ok(res), protocol))) => break (res, protocol),
            Ok(Ok((Err(err), _))) if attempt < MAX_RETRIES => {
                let wait = retry_delay(attempt, None);
                tracing::warn!(error = %err, attempt = attempt + 1, wait_ms = wait.as_millis() as u64, "retrying transient LLM transport error");
                attempt += 1;
                tokio::time::sleep(wait).await;
            }
            Ok(Ok((Err(err), _))) => {
                tracing::error!(error = %err, "LLM request failed");
                return Err(unavailable());
            }
            Ok(Err(err)) => return Err(err),
            Err(_) if attempt < MAX_RETRIES => {
                let wait = retry_delay(attempt, None);
                tracing::warn!(
                    attempt = attempt + 1,
                    wait_ms = wait.as_millis() as u64,
                    "LLM request timed out waiting for response headers, retrying"
                );
                attempt += 1;
                tokio::time::sleep(wait).await;
            }
            Err(_) => {
                tracing::error!("LLM request timed out waiting for response headers");
                return Err(unavailable());
            }
        }
    };
    if !res.status().is_success() {
        let status = res.status();
        let raw = read_capped_text(res, MAX_ERROR_BODY_BYTES).await;
        if app.standalone.is_some() {
            let preview: String = raw.chars().take(400).collect();
            tracing::error!(status = status.as_u16(), body = %preview, "standalone LLM provider returned error");
            return Err(classify_standalone_error(status, &raw));
        } else {
            let body: Value = serde_json::from_str(&raw).unwrap_or_default();
            tracing::warn!(status = status.as_u16(), "control LLM inference error");
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("Gateway unavailable")
                .to_owned();
            return Err(Error(status, message));
        }
    }
    let stream = async_stream::try_stream! {
        let mut upstream = sse_stream::SseByteStream::new(res.bytes_stream());
        let mut translation = AnthropicStream::default();
        while let Some(frame) = upstream.next().await {
            let Some(data) = frame.map_err(|_| unavailable())?.data else { continue };
            if data.trim().is_empty() {
                continue;
            }
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

/// Build an Anthropic Messages API request with prompt-caching breakpoints on the system prompt,
/// final tool definition, and final user/tool-result content block (inspired by pi_agent_rust).
fn anthropic_request(request: &Value, max_tokens: u32) -> Result<Value> {
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for msg in request["messages"].as_array().ok_or_else(bad)? {
        let role = msg["role"].as_str().ok_or_else(bad)?;
        let text = msg["content"].as_str().unwrap_or_default();
        if role == "system" {
            if !text.trim().is_empty() {
                system.push(text);
            }
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
                let raw_args = call["function"]["arguments"].as_str().ok_or_else(bad)?;
                let input: Value = if raw_args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(raw_args).map_err(|_| bad())?
                };
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
    // Add an ephemeral prompt-cache breakpoint to the final content block of the last user message.
    if let Some(last_user) = messages.iter_mut().rev().find(|m| m["role"] == "user")
        && let Some(last_block) = last_user["content"]
            .as_array_mut()
            .and_then(|a| a.last_mut())
        && let Some(obj) = last_block.as_object_mut()
    {
        obj.insert("cache_control".into(), json!({"type":"ephemeral"}));
    }
    let max_tokens = request["max_tokens"]
        .as_u64()
        .map_or(u64::from(max_tokens), |limit| {
            limit.min(u64::from(max_tokens))
        });
    let mut body =
        json!({"model":request["model"],"max_tokens":max_tokens,"stream":true,"messages":messages});
    if !system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": system.join("\n\n"),
            "cache_control": {"type": "ephemeral"}
        }]);
    }
    if let Some(tools) = request["tools"]
        .as_array()
        .filter(|tools| !tools.is_empty())
    {
        let mut translated: Vec<Value> = tools
            .iter()
            .map(|tool| {
                let function = &tool["function"];
                json!({"name":function["name"],"description":function["description"],"input_schema":function["parameters"]})
            })
            .collect();
        if let Some(last_tool) = translated.last_mut().and_then(Value::as_object_mut) {
            last_tool.insert("cache_control".into(), json!({"type":"ephemeral"}));
        }
        body["tools"] = Value::Array(translated);
        let disable_parallel = request["parallel_tool_calls"] == json!(false);
        body["tool_choice"] = if disable_parallel {
            json!({"type":"auto","disable_parallel_tool_use":true})
        } else {
            json!({"type":"auto"})
        };
    }
    Ok(body)
}

#[derive(Default)]
struct AnthropicStream {
    input_tokens: Option<u64>,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    next_tool: usize,
    open_tools: HashMap<u64, (usize, bool, Value)>,
    reason_seen: bool,
}

impl AnthropicStream {
    fn translate(&mut self, event: &Value) -> Result<Option<String>> {
        let index = || event["index"].as_u64().ok_or_else(unavailable);
        let frame = match event["type"].as_str() {
            Some("message_start") => {
                let usage = &event["message"]["usage"];
                self.cache_read_tokens = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                self.cache_creation_tokens =
                    usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.input_tokens = usage["input_tokens"].as_u64().map(|base| {
                    base.saturating_add(self.cache_read_tokens)
                        .saturating_add(self.cache_creation_tokens)
                });
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
                        let partial = delta["partial_json"].as_str().unwrap_or("");
                        if partial.is_empty() {
                            return Ok(None);
                        }
                        *fragments = true;
                        json!({"choices":[{"delta":{"tool_calls":[{"index":number,"function":{"arguments":partial}}]}}]})
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
                    Some(other) => {
                        tracing::warn!(stop_reason = %other, "unrecognized Anthropic stop_reason, treating as stop");
                        json!([{"finish_reason":"stop"}])
                    }
                };
                self.reason_seen |= event["delta"]["stop_reason"].is_string();
                if let Some(base) = event["usage"]["input_tokens"].as_u64() {
                    self.input_tokens = Some(
                        base.saturating_add(self.cache_read_tokens)
                            .saturating_add(self.cache_creation_tokens),
                    );
                }
                let mut usage = json!({
                    "prompt_tokens": self.input_tokens,
                    "completion_tokens": event["usage"]["output_tokens"],
                });
                if self.cache_read_tokens > 0 {
                    usage["cache_read_input_tokens"] = json!(self.cache_read_tokens);
                }
                if self.cache_creation_tokens > 0 {
                    usage["cache_creation_input_tokens"] = json!(self.cache_creation_tokens);
                }
                json!({"choices":choices,"usage":usage})
            }
            Some("message_stop") if self.reason_seen && self.open_tools.is_empty() => {
                return Ok(Some("[DONE]".into()));
            }
            Some("error") => {
                let err_type = event["error"]["type"].as_str().unwrap_or("unknown");
                let err_msg = event["error"]["message"].as_str().unwrap_or("");
                tracing::warn!(error_type = %err_type, error = %err_msg, "Anthropic stream emitted error event");
                return Err(unavailable());
            }
            Some("message_stop") => return Err(unavailable()),
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
        assert_eq!(translated["system"][0]["text"], "System instructions");
        assert_eq!(
            translated["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(translated["max_tokens"], 512);
        assert_eq!(
            translated["tools"][0]["input_schema"],
            json!({"type":"object"})
        );
        assert_eq!(
            translated["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
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
        assert_eq!(
            translated["messages"][2]["content"][2]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn anthropic_request_omits_empty_system_and_respects_parallel_tool_calls() {
        let req = json!({
            "model": "fixture",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"function": {"name": "read", "description": "Read", "parameters": {"type": "object"}}}],
            "parallel_tool_calls": false
        });
        let translated = anthropic_request(&req, 512).unwrap();
        assert!(translated.get("system").is_none());
        assert_eq!(
            translated["tool_choice"],
            json!({"type":"auto","disable_parallel_tool_use":true})
        );
    }

    #[test]
    fn anthropic_stream_uses_native_tool_blocks_and_requires_a_closed_turn() {
        let mut stream = AnthropicStream::default();
        assert!(stream.translate(&json!({"type":"message_stop"})).is_err());
        let frames = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":100}}}),
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
            json!({"prompt_tokens":112,"completion_tokens":8,"cache_read_input_tokens":100})
        );
        assert_eq!(
            stream
                .translate(&json!({"type":"message_stop"}))
                .unwrap()
                .as_deref(),
            Some("[DONE]")
        );
    }

    #[test]
    fn anthropic_zero_arg_tool_emits_empty_object_even_with_empty_delta() {
        let mut stream = AnthropicStream::default();
        let frames = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_0","name":"ls","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
        ];
        let chunks: Vec<Value> = frames
            .iter()
            .filter_map(|frame| stream.translate(frame).unwrap())
            .map(|frame| serde_json::from_str(&frame).unwrap())
            .collect();
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }

    #[test]
    fn retry_classifier_never_retries_auth_or_validation_errors() {
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
    }
}
