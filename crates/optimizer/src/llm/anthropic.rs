//! Anthropic provider — implements `LlmClient` for Claude models.
//! Uses the Anthropic Messages API with tool use.
//! Auth: `x-api-key` header + `anthropic-version: 2023-06-01`.
//!
//! Key differences from OpenAI/Gemini:
//! - System prompt is a top-level `system` field, not a message
//! - Tool results use `tool_result` content blocks with `tool_use_id`
//! - `max_tokens` is mandatory
//! - Response content is an array of typed blocks (text, tool_use)

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::body::{body_cap_exceeded, body_capped, hit_body_cap, json_capped, read_body_capped};
use super::cancel::{sleep_observing, CANCELLED};
use super::openai_compat::{
    as_transport_error, doubled_backoff, http_client, is_retryable_status, retry_after_delay,
    RateReserve, CHAT_REQUEST_TIMEOUT, METADATA_TIMEOUT,
};
use super::rate::{persist_usage, PersistedUsage, RateTracker};
use super::sse::{slot_index_rejected, MAX_TOOL_CALL_INDEX};
use super::{http_error, KeyValidationResult, LlmClient, LlmError, ToolDefinition};

/// Anthropic's conservative default requests-per-minute ceiling.
const RPM_LIMIT: u32 = 50;

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Completion ceiling for both the plain and the tool-loop path.
///
/// Deliberately not the openai-compat family's 16_384: Anthropic enforces a
/// per-model `max_tokens` ceiling and 400s a request above it, and the older
/// Claude 3 models top out at 8192/4096. What is fixed here is the
/// undocumented *halving* — the tool loop used to send 4096 while plain
/// generation sent 8192, so tool answers got a quarter of the budget every
/// other provider gives them (GLM F24).
const ANTHROPIC_MAX_TOKENS: u32 = 8192;

pub struct AnthropicClient {
    api_key: String,
    model: String,
    http: reqwest::blocking::Client,
    cache: crate::llm::response_cache::ResponseCache,
    rate: Mutex<RateTracker>,
    usage_path: Option<PathBuf>,
}

// Anthropic API Types

#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    /// Always streamed: reasoning models can think for minutes, and a
    /// buffered response holds an idle connection the whole time.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

/// Anthropic content can be a simple string or an array of content blocks.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Serialize, Debug, Clone)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Deserialize, Debug)]
struct MessagesResponse {
    content: Option<Vec<ContentBlock>>,
    #[allow(dead_code)]
    stop_reason: Option<String>,
}

// SSE Streaming

/// One Anthropic SSE `data:` payload. The `type` field selects the event;
/// only the fields relevant per event are deserialized.
#[derive(Deserialize)]
struct StreamEvent {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<ContentBlock>,
    /// Mid-stream failure: `{"type":"error","error":{"type":…,"message":…}}`.
    #[serde(default)]
    error: Option<Value>,
    /// `message_start` carries `message.usage` (input tokens).
    #[serde(default)]
    message: Option<StartMessage>,
    /// `message_delta` carries top-level `usage` (cumulative output tokens).
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: Option<WireUsage>,
}

/// Anthropic's `usage` object. Cache writes and reads are billed input,
/// at their own rates, so they count toward the prompt total here.
#[derive(Deserialize, Default, Clone, Copy)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    text: Option<String>,
    /// `thinking_delta` payload, when extended thinking is on.
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// A content block under assembly from ordered deltas.
enum StreamBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

/// Reads an Anthropic Messages SSE body into a `MessagesResponse`.
///
/// Event sequence (Anthropic Messages streaming): `message_start`,
/// `content_block_start`, `content_block_delta` (`text_delta` /
/// `input_json_delta`), `content_block_stop`, `message_delta` (carries
/// `stop_reason`), `message_stop`. An `error` event is the mid-stream
/// failure channel — the HTTP status is already 200 by then.
fn read_anthropic_stream<R: std::io::Read>(
    reader: R,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<MessagesResponse, LlmError> {
    use std::io::BufRead;

    let mut blocks: Vec<Option<StreamBlock>> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut input: Option<WireUsage> = None;
    let mut output_tokens: Option<u64> = None;

    let mut capped = body_capped(reader);
    for line in std::io::BufReader::new(&mut capped).lines() {
        if is_cancelled() {
            return Err(LlmError::Unavailable(CANCELLED.to_string()));
        }
        let line = line.map_err(http_error)?;
        let line = line.trim();
        // `event:` name lines, comments and blanks carry no payload of their
        // own. Anthropic always frames its JSON with `data:`, unlike
        // OpenRouter (see `sse::stream_payload`).
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            continue;
        };
        let event: StreamEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        match event.r#type.as_str() {
            "message_start" => {
                input = event.message.and_then(|m| m.usage);
                output_tokens = input.and_then(|u| u.output_tokens);
            }
            "content_block_start" => {
                // Bound the wire-supplied index before it can size the Vec.
                if event.index > MAX_TOOL_CALL_INDEX {
                    return Err(slot_index_rejected(event.index));
                }
                // At most MAX_TOOL_CALL_INDEX + 1 slots, guaranteed above.
                blocks.resize_with(blocks.len().max(event.index + 1), || None);
                blocks[event.index] = match event.content_block {
                    Some(ContentBlock::Text { .. }) => Some(StreamBlock::Text {
                        text: String::new(),
                    }),
                    Some(ContentBlock::ToolUse { id, name, .. }) => Some(StreamBlock::ToolUse {
                        id,
                        name,
                        input_json: String::new(),
                    }),
                    _ => None,
                };
            }
            "content_block_delta" => {
                let Some(Some(block)) = blocks.get_mut(event.index) else {
                    continue;
                };
                if let Some(delta) = event.delta {
                    if delta.r#type == "thinking_delta" {
                        super::live::reasoning(delta.thinking.as_deref().unwrap_or_default());
                    }
                    match (delta.r#type.as_str(), block) {
                        ("text_delta", StreamBlock::Text { text }) => {
                            let piece = delta.text.as_deref().unwrap_or_default();
                            super::live::content(piece);
                            text.push_str(piece);
                        }
                        ("input_json_delta", StreamBlock::ToolUse { input_json, .. }) => {
                            input_json.push_str(delta.partial_json.as_deref().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(n) = event.usage.and_then(|u| u.output_tokens) {
                    output_tokens = Some(n);
                }
                if let Some(delta) = event.delta {
                    if delta.stop_reason.is_some() {
                        stop_reason = delta.stop_reason;
                    }
                }
            }
            "error" => {
                let err = event.error.unwrap_or(Value::Null);
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error")
                    .to_string();
                let overloaded = err
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.contains("overloaded"));
                return Err(LlmError::Api {
                    status: if overloaded { 529 } else { 502 },
                    message,
                });
            }
            _ => {}
        }
    }

    if hit_body_cap(&capped) {
        return Err(body_cap_exceeded("Anthropic message"));
    }
    // Billed whether or not the body held anything usable.
    super::usage::record((input.is_some() || output_tokens.is_some()).then(|| {
        let u = input.unwrap_or_default();
        let prompt = u.input_tokens.unwrap_or(0)
            + u.cache_creation_input_tokens.unwrap_or(0)
            + u.cache_read_input_tokens.unwrap_or(0);
        super::usage::ResponseUsage {
            prompt: Some(prompt),
            completion: output_tokens,
            total: None,
            cost_usd: None,
        }
    }));

    let content = blocks
        .into_iter()
        .flatten()
        .map(|block| match block {
            StreamBlock::Text { text } => ContentBlock::Text { text },
            StreamBlock::ToolUse {
                id,
                name,
                input_json,
            } => ContentBlock::ToolUse {
                id,
                name,
                input: match super::parse_tool_arguments(&input_json) {
                    Ok(v) => v,
                    Err(err) => err,
                },
            },
        })
        .collect::<Vec<_>>();
    let content = (!content.is_empty()).then_some(content);

    Ok(MessagesResponse {
        content,
        stop_reason,
    })
}

impl AnthropicClient {
    pub fn new(api_key: &str, model: &str) -> Result<Self, LlmError> {
        let http = http_client()?;
        Ok(Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            http,
            cache: crate::llm::response_cache::ResponseCache::new(1800, 64),
            rate: Mutex::new(RateTracker::new(RPM_LIMIT)),
            usage_path: None,
        })
    }

    /// `GET /v1/models` — shared by key validation and the model catalog.
    /// Does not spend a Messages token.
    fn models_request(&self) -> reqwest::blocking::RequestBuilder {
        self.http
            .get(format!("{}/models", ANTHROPIC_API_BASE))
            .timeout(METADATA_TIMEOUT)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
    }

    pub fn with_persistence(
        api_key: &str,
        model: &str,
        usage_path: PathBuf,
    ) -> Result<Self, LlmError> {
        let http = http_client()?;

        let rate = if usage_path.exists() {
            match std::fs::read_to_string(&usage_path)
                .ok()
                .and_then(|s| serde_json::from_str::<PersistedUsage>(&s).ok())
            {
                Some(persisted) => RateTracker::from_persisted(persisted, RPM_LIMIT),
                None => RateTracker::new(RPM_LIMIT),
            }
        } else {
            RateTracker::new(RPM_LIMIT)
        };

        Ok(Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            http,
            cache: crate::llm::response_cache::ResponseCache::new(1800, 64),
            rate: Mutex::new(rate),
            usage_path: Some(usage_path),
        })
    }

    fn send_messages(
        &self,
        messages: &[AnthropicMessage],
        system: Option<&str>,
        tools: Option<&[ToolDefinition]>,
        max_tokens: u32,
    ) -> Result<MessagesResponse, LlmError> {
        const MAX_RETRIES: u32 = 3;
        // Retries, 529 backoff and the stream: all of it is the run waiting on the model.
        let _wait = super::usage::WaitTimer::start();

        let is_cancelled = super::cancel::is_cancelled;

        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check_and_reserve()?;
        // Released on drop unless `keep()` runs: the hand-written undo on
        // each early return missed the mid-stream failure path (GLM F16).
        let mut reserve = RateReserve::held(&self.rate);

        let anthropic_tools = tools.map(|defs| {
            defs.iter()
                .map(|td| AnthropicTool {
                    name: td.name.clone(),
                    description: td.description.clone(),
                    input_schema: td.parameters.clone(),
                })
                .collect::<Vec<_>>()
        });

        let request = MessagesRequest {
            model: self.model.clone(),
            max_tokens,
            messages: messages.to_vec(),
            system: system.map(|s| s.to_string()),
            tools: anthropic_tools,
            stream: Some(true),
        };

        let url = format!("{}/messages", ANTHROPIC_API_BASE);
        let mut last_error: Option<LlmError> = None;
        let mut next_delay = std::time::Duration::from_secs(5);

        for attempt in 0..MAX_RETRIES {
            if is_cancelled() {
                return Err(LlmError::Unavailable(CANCELLED.to_string()));
            }
            if attempt > 0
                && !{
                    let _waiting =
                        super::usage::Waiting::new(next_delay, super::usage::WaitReason::Retry);
                    sleep_observing(next_delay, &is_cancelled)
                }
            {
                return Err(LlmError::Unavailable(CANCELLED.to_string()));
            }
            if attempt > 0 {
                next_delay = doubled_backoff(next_delay);
            }

            let resp = match self
                .http
                .post(&url)
                .timeout(CHAT_REQUEST_TIMEOUT)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
            {
                Ok(r) => r,
                Err(e) => {
                    if attempt == MAX_RETRIES - 1 {
                        return Err(http_error(e));
                    }
                    last_error = Some(http_error(e));
                    continue;
                }
            };

            let status = resp.status().as_u16();
            match status {
                200 => match read_anthropic_stream(resp, &is_cancelled) {
                    Ok(body) => {
                        reserve.keep();
                        let rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
                        self.persist_usage(&rate);
                        return Ok(body);
                    }
                    // Anthropic ships `overloaded_error` in-band on a 200,
                    // the same class of trap as OpenRouter's in-band 429.
                    Err(LlmError::Api { status, message })
                        if is_retryable_status(status) && attempt + 1 < MAX_RETRIES =>
                    {
                        last_error = Some(as_transport_error(status, message));
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                401 => return Err(LlmError::InvalidKey),
                status if is_retryable_status(status) => {
                    if let Some(delay) = retry_after_delay(resp.headers()) {
                        next_delay = delay;
                    }
                    last_error = Some(as_transport_error(status, read_body_capped(resp)));
                    continue;
                }
                _ => {
                    return Err(LlmError::Api {
                        status,
                        message: read_body_capped(resp),
                    })
                }
            }
        }

        Err(last_error.unwrap_or_else(|| LlmError::Api {
            status: 500,
            message: "Anthropic server error after retries".into(),
        }))
    }

    fn persist_usage(&self, rate: &RateTracker) {
        persist_usage(self.usage_path.as_deref(), rate);
    }
}

/// Extract text from Anthropic content blocks.
fn extract_text(blocks: &[ContentBlock]) -> Option<String> {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join(""))
    }
}

/// Extract tool use blocks from content.
fn extract_tool_uses(blocks: &[ContentBlock]) -> Vec<(String, String, Value)> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Drop oldest tool-call turn(s) when the conversation exceeds the token
/// budget. An Anthropic turn is one assistant message (with tool_use blocks)
/// followed by one user message (with tool_result blocks); pairs must be
/// dropped atomically. The initial user prompt and the most recent turn are
/// always preserved.
fn trim_messages(messages: &mut Vec<AnthropicMessage>, budget_tokens: usize) {
    use super::trim::estimate_tokens;

    fn message_tokens(m: &AnthropicMessage) -> usize {
        match &m.content {
            AnthropicContent::Text(s) => estimate_tokens(s),
            AnthropicContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => estimate_tokens(text),
                    ContentBlock::ToolUse { input, .. } => estimate_tokens(&input.to_string()),
                    ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
                })
                .sum(),
        }
    }

    let mut total: usize = messages.iter().map(message_tokens).sum();
    if total <= budget_tokens {
        return;
    }

    loop {
        let turn_starts: Vec<usize> = messages
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, m)| m.role == "assistant")
            .map(|(i, _)| i)
            .collect();

        if turn_starts.len() < 2 {
            return;
        }

        messages.drain(turn_starts[0]..turn_starts[1]);
        total = messages.iter().map(message_tokens).sum();
        if total <= budget_tokens {
            return;
        }
    }
}

impl super::tool_loop::TurnDriver for AnthropicClient {
    type Conv = Vec<AnthropicMessage>;
    type Err = LlmError;

    fn open(&self, prompt: &str) -> Vec<AnthropicMessage> {
        vec![AnthropicMessage {
            role: "user".to_string(),
            content: AnthropicContent::Text(prompt.to_string()),
        }]
    }
    fn trim(&self, conv: &mut Vec<AnthropicMessage>) {
        trim_messages(conv, super::trim::SAFE_PROMPT_BUDGET_TOKENS);
    }
    fn turn(
        &self,
        conv: &mut Vec<AnthropicMessage>,
        tools: Option<&[ToolDefinition]>,
        _mode: super::tool_loop::TurnMode,
    ) -> Result<super::tool_loop::Turn, LlmError> {
        let response = self.send_messages(conv, None, tools, ANTHROPIC_MAX_TOKENS)?;
        let blocks = response.content.unwrap_or_default();
        let calls = extract_tool_uses(&blocks)
            .into_iter()
            .map(|(id, name, input)| super::tool_loop::ToolCall {
                id,
                name,
                args: input,
            })
            .collect();
        let text = extract_text(&blocks);
        // Every block goes back as it came: the assistant turn is the
        // provider's, not a flattening of it.
        conv.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: AnthropicContent::Blocks(blocks),
        });
        Ok(super::tool_loop::Turn { text, calls })
    }
    fn push_tool_results(
        &self,
        conv: &mut Vec<AnthropicMessage>,
        results: &[(super::tool_loop::ToolCall, Value)],
    ) {
        let blocks = results
            .iter()
            .map(|(call, value)| ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: serde_json::to_string(value).unwrap_or_default(),
            })
            .collect();
        conv.push(AnthropicMessage {
            role: "user".to_string(),
            content: AnthropicContent::Blocks(blocks),
        });
    }
    fn push_user(&self, conv: &mut Vec<AnthropicMessage>, text: &str) {
        conv.push(AnthropicMessage {
            role: "user".to_string(),
            content: AnthropicContent::Text(text.to_string()),
        });
    }
    fn cancelled(&self) -> LlmError {
        LlmError::Unavailable(CANCELLED.to_string())
    }
    fn no_answer(&self, detail: String) -> LlmError {
        LlmError::Parse(format!("Anthropic: {detail}"))
    }
    fn is_deadline(&self, err: &LlmError) -> bool {
        super::openai_compat::is_deadline(err)
    }
    fn is_function_call_failure(&self, err: &LlmError) -> bool {
        super::openai_compat::is_function_call_failure(err)
    }
}

impl LlmClient for AnthropicClient {
    fn provider_name(&self) -> &str {
        "Anthropic"
    }

    /// Validate the API key via `GET /v1/models`.
    ///
    /// Same path as [`Self::list_models`]; does not spend a Messages token.
    fn validate_key(&self) -> Result<(), LlmError> {
        let resp = self.models_request().send().map_err(http_error)?;

        match resp.status().as_u16() {
            200 => Ok(()),
            401 => Err(LlmError::InvalidKey),
            // 400 with "credit balance" means key is valid but account has no credits.
            // 403 means key is valid but account is disabled/restricted.
            // Accept these as valid keys — billing is a separate concern.
            400 | 403 => {
                let body = read_body_capped(resp);
                if body.contains("credit balance")
                    || body.contains("billing")
                    || body.contains("disabled")
                    || body.contains("permission")
                {
                    Ok(())
                } else {
                    Err(LlmError::Api {
                        status: 400,
                        message: body,
                    })
                }
            }
            429 => Ok(()), // Rate limited means key is valid
            status => {
                let body = read_body_capped(resp);
                Err(LlmError::Api {
                    status,
                    message: body,
                })
            }
        }
    }

    /// Validate the API key via `GET /v1/models` (no Messages token spent).
    fn validate_key_detailed(&self) -> KeyValidationResult {
        let resp = match self.models_request().send() {
            Ok(r) => r,
            Err(e) => {
                return KeyValidationResult {
                    valid: false,
                    message: "Cannot connect to Anthropic API. Check your internet connection."
                        .into(),
                    warning: Some(e.to_string()),
                };
            }
        };

        let status = resp.status().as_u16();
        let body = read_body_capped(resp);

        match status {
            200 => KeyValidationResult {
                valid: true,
                message: "Anthropic key validated successfully!".into(),
                warning: None,
            },
            401 => KeyValidationResult {
                valid: false,
                message: "Invalid Anthropic API key. Check that you copied the full key from console.anthropic.com/settings/keys.".into(),
                warning: None,
            },
            400 if super::has_billing_keyword(&body) => KeyValidationResult {
                valid: true,
                message: "Anthropic key is valid!".into(),
                warning: Some("Your account has no credits. Add credits at console.anthropic.com to use this provider.".into()),
            },
            403 => KeyValidationResult {
                valid: true,
                message: "Anthropic key is valid!".into(),
                warning: Some("Your account may be restricted. Check console.anthropic.com/settings for details.".into()),
            },
            429 => KeyValidationResult {
                valid: true,
                message: "Anthropic key is valid!".into(),
                warning: Some("Currently rate-limited. Try again shortly.".into()),
            },
            _ => KeyValidationResult {
                valid: false,
                message: format!("Anthropic API error (HTTP {}).", status),
                warning: if body.is_empty() { None } else { Some(body) },
            },
        }
    }

    fn generate(&self, prompt: &str) -> Result<String, LlmError> {
        let messages = vec![AnthropicMessage {
            role: "user".to_string(),
            content: AnthropicContent::Text(prompt.to_string()),
        }];

        let response = self.send_messages(&messages, None, None, ANTHROPIC_MAX_TOKENS)?;
        let blocks = response
            .content
            .ok_or_else(|| LlmError::Parse("No content from Anthropic".into()))?;
        extract_text(&blocks).ok_or_else(|| LlmError::Parse("No text in Anthropic response".into()))
    }

    fn generate_cached(&self, prompt: &str) -> Result<String, LlmError> {
        if let Some(text) = self.cache.get(prompt) {
            return Ok(text);
        }

        let text = self.generate(prompt)?;
        self.cache.insert(prompt, text.clone());

        Ok(text)
    }

    fn generate_with_tools_progress(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        execute_tool: &mut dyn FnMut(&str, &Value) -> Value,
        max_turns: usize,
        on_progress: &mut dyn FnMut(usize, usize, &[String]),
    ) -> Result<String, LlmError> {
        super::tool_loop::run(self, prompt, tools, execute_tool, max_turns, on_progress)
    }

    fn list_models(&self) -> Result<Vec<super::ModelInfo>, LlmError> {
        let resp = self.models_request().send().map_err(http_error)?;

        match resp.status().as_u16() {
            200 => {}
            401 => return Err(LlmError::InvalidKey),
            429 => return Err(LlmError::RateLimited(read_body_capped(resp))),
            status => {
                let body = read_body_capped(resp);
                return Err(LlmError::Api {
                    status,
                    message: body,
                });
            }
        }

        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Option<Vec<ModelEntry>>,
        }
        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
            display_name: Option<String>,
        }

        let body: ModelsResponse = json_capped(resp)?;

        let entries = body.data.unwrap_or_default();

        let mut models: Vec<super::ModelInfo> = entries
            .into_iter()
            .map(|m| super::ModelInfo {
                // Anthropic publishes no free tier and no per-model price in
                // its catalog: every model here bills. Nor does it publish
                // capabilities, so the rest stays at its default of "not
                // stated" rather than being invented here.
                free: false,
                display_name: m.display_name.unwrap_or_else(|| m.id.clone()),
                id: m.id,
                ..Default::default()
            })
            .collect();

        // Sort by ID (alphabetical puts claude-haiku before claude-sonnet, etc.)
        models.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(models)
    }

    fn remaining_quota(&self) -> u32 {
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remaining_today()
    }

    fn clear_cache(&self) {
        self.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A14-3 — key check must not spend a Messages token. Pin both
    /// validate_key entry points to GET /v1/models.
    #[test]
    fn validate_key_does_not_post_messages() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/llm/anthropic.rs"));
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");

        let validate_key = production
            .split("fn validate_key(")
            .nth(1)
            .and_then(|s| s.split("fn validate_key_detailed").next())
            .expect("validate_key body");
        assert!(
            !validate_key.contains("/messages"),
            "validate_key must not POST /v1/messages"
        );
        assert!(
            validate_key.contains("models_request"),
            "validate_key must use GET /v1/models"
        );
        assert!(
            !validate_key.contains(".post("),
            "validate_key must not POST"
        );

        let detailed = production
            .split("fn validate_key_detailed")
            .nth(1)
            .and_then(|s| s.split("fn generate(").next())
            .expect("validate_key_detailed body");
        assert!(
            !detailed.contains("/messages"),
            "validate_key_detailed must not POST /v1/messages"
        );
        assert!(
            detailed.contains("models_request"),
            "validate_key_detailed must use GET /v1/models"
        );
        assert!(
            !detailed.contains(".post("),
            "validate_key_detailed must not POST"
        );
    }

    #[test]
    fn test_provider_name() {
        let client = AnthropicClient::new("fake-key", "claude-sonnet-4-6").unwrap();
        assert_eq!(client.provider_name(), "Anthropic");
    }

    #[test]
    fn test_remaining_quota_default() {
        let client = AnthropicClient::new("fake-key", "claude-sonnet-4-6").unwrap();
        assert_eq!(client.remaining_quota(), 10000);
    }

    #[test]
    fn test_tool_definition_to_anthropic_format() {
        let defs = [ToolDefinition {
            name: "get_profession_info".into(),
            description: "Get profession details".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "profession": { "type": "string" }
                },
                "required": ["profession"]
            }),
        }];

        let anthropic_tools: Vec<AnthropicTool> = defs
            .iter()
            .map(|td| AnthropicTool {
                name: td.name.clone(),
                description: td.description.clone(),
                input_schema: td.parameters.clone(),
            })
            .collect();

        assert_eq!(anthropic_tools.len(), 1);
        assert_eq!(anthropic_tools[0].name, "get_profession_info");

        // Verify serialization format
        let json = serde_json::to_value(&anthropic_tools[0]).unwrap();
        assert_eq!(json["name"], "get_profession_info");
        assert_eq!(json["input_schema"]["type"], "object");
    }

    #[test]
    fn test_content_block_serialization() {
        let text_block = ContentBlock::Text {
            text: "Hello".into(),
        };
        let json = serde_json::to_value(&text_block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello");

        let tool_use = ContentBlock::ToolUse {
            id: "toolu_123".into(),
            name: "get_profession_info".into(),
            input: serde_json::json!({"profession": "Warrior"}),
        };
        let json = serde_json::to_value(&tool_use).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "toolu_123");
        assert_eq!(json["name"], "get_profession_info");

        let tool_result = ContentBlock::ToolResult {
            tool_use_id: "toolu_123".into(),
            content: r#"{"name":"Warrior"}"#.into(),
        };
        let json = serde_json::to_value(&tool_result).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "toolu_123");
    }

    #[test]
    fn test_extract_text_from_blocks() {
        let blocks = vec![
            ContentBlock::Text {
                text: "Hello ".into(),
            },
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "test".into(),
                input: Value::Null,
            },
            ContentBlock::Text {
                text: "world".into(),
            },
        ];
        assert_eq!(extract_text(&blocks), Some("Hello world".into()));
    }

    #[test]
    fn test_extract_text_empty_blocks() {
        let blocks = vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "test".into(),
            input: Value::Null,
        }];
        assert_eq!(extract_text(&blocks), None);
    }

    #[test]
    fn test_extract_tool_uses() {
        let blocks = vec![
            ContentBlock::Text {
                text: "Let me check".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "get_profession_info".into(),
                input: serde_json::json!({"profession": "Warrior"}),
            },
            ContentBlock::ToolUse {
                id: "toolu_2".into(),
                name: "get_spec_traits".into(),
                input: serde_json::json!({"spec_name": "Berserker"}),
            },
        ];

        let uses = extract_tool_uses(&blocks);
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].0, "toolu_1");
        assert_eq!(uses[0].1, "get_profession_info");
        assert_eq!(uses[1].0, "toolu_2");
        assert_eq!(uses[1].1, "get_spec_traits");
    }

    #[test]
    fn test_trim_messages_drops_oldest_turn() {
        fn user_text(s: &str) -> AnthropicMessage {
            AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Text(s.into()),
            }
        }
        fn assistant_turn(id: &str, name: &str, payload: &str) -> AnthropicMessage {
            AnthropicMessage {
                role: "assistant".into(),
                content: AnthropicContent::Blocks(vec![ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input: serde_json::json!({ "q": payload }),
                }]),
            }
        }
        fn user_tool_result(id: &str, payload: &str) -> AnthropicMessage {
            AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: payload.into(),
                }]),
            }
        }

        let filler = "x".repeat(400);
        let mut messages = vec![
            user_text("initial prompt"),
            assistant_turn("tu_1", "get_trait_details", &filler),
            user_tool_result("tu_1", &filler),
            assistant_turn("tu_2", "get_trait_details", &filler),
            user_tool_result("tu_2", &filler),
            assistant_turn("tu_3", "get_trait_details", &filler),
            user_tool_result("tu_3", &filler),
        ];
        let original_len = messages.len();

        trim_messages(&mut messages, 200);

        assert!(
            messages.len() < original_len,
            "expected trimming, got {}",
            messages.len()
        );
        // Initial prompt preserved.
        assert_eq!(messages[0].role, "user");
        match &messages[0].content {
            AnthropicContent::Text(s) => assert_eq!(s, "initial prompt"),
            _ => panic!("first message lost text content"),
        }
        // Last turn's tool_use_id still matches its tool_result.
        let last_assistant_idx = messages
            .iter()
            .rposition(|m| m.role == "assistant")
            .expect("must retain at least one assistant");
        let last_use_id = match &messages[last_assistant_idx].content {
            AnthropicContent::Blocks(blocks) => blocks
                .iter()
                .find_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .expect("last assistant has a tool_use block"),
            _ => panic!("last assistant not block-shaped"),
        };
        assert_eq!(last_use_id, "tu_3");
        // Every tool_result still has a matching tool_use earlier.
        for (i, m) in messages.iter().enumerate() {
            if let AnthropicContent::Blocks(blocks) = &m.content {
                for b in blocks {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                        let paired = messages[..i].iter().any(|prev| {
                                if let AnthropicContent::Blocks(pbs) = &prev.content {
                                    pbs.iter().any(|pb| {
                                        matches!(pb, ContentBlock::ToolUse { id, .. } if id == tool_use_id)
                                    })
                                } else {
                                    false
                                }
                            });
                        assert!(paired, "orphaned tool_use_id {}", tool_use_id);
                    }
                }
            }
        }
    }

    #[test]
    fn test_trim_messages_noop_under_budget() {
        let mut messages = vec![AnthropicMessage {
            role: "user".into(),
            content: AnthropicContent::Text("short".into()),
        }];
        trim_messages(&mut messages, 10_000);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_anthropic_content_text_serialization() {
        let msg = AnthropicMessage {
            role: "user".to_string(),
            content: AnthropicContent::Text("Hello".to_string()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Hello");
    }

    #[test]
    fn test_anthropic_content_blocks_serialization() {
        let msg = AnthropicMessage {
            role: "user".to_string(),
            content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_123".into(),
                content: r#"{"ok": true}"#.into(),
            }]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert!(json["content"].is_array());
        assert_eq!(json["content"][0]["type"], "tool_result");
    }

    #[test]
    fn test_tool_use_id_round_trip() {
        // Simulate Anthropic's response: an assistant message with one tool_use
        // block. Parse like `send_messages` does, extract the id via the same
        // `extract_tool_uses` helper `generate_with_tools_progress` uses, then
        // build the follow-up tool_result block and assert the id survives.
        let server_response_json = r#"{
            "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "toolu_01A8bL9Qrx", "name": "square_number", "input": {"number": 7}}
            ]
        }"#;

        let body: MessagesResponse =
            serde_json::from_str(server_response_json).expect("parse MessagesResponse");
        let blocks = body.content.expect("content blocks");
        let uses = extract_tool_uses(&blocks);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "toolu_01A8bL9Qrx");
        assert_eq!(uses[0].1, "square_number");

        let result_block = ContentBlock::ToolResult {
            tool_use_id: uses[0].0.clone(),
            content: r#"{"result":49}"#.into(),
        };
        let wire = serde_json::to_value(&result_block).unwrap();
        assert_eq!(wire["type"], "tool_result");
        assert_eq!(wire["tool_use_id"], "toolu_01A8bL9Qrx");
    }

    /// GLM F3 / Claude F18 — the Anthropic half of the index cap.
    ///
    /// `StreamEvent.index` is a `usize` taken verbatim from the wire and used
    /// as a `Vec` position. The old `while blocks.len() <= event.index {
    /// blocks.push(None) }` grew the vector to an attacker-chosen length one
    /// element at a time: at `usize::MAX` that is an OOM abort, which does not
    /// unwind, so the `catch_unwind` around the optimize worker cannot save
    /// the game process. The guard has to reject the index *before* any
    /// allocation, which is why this asserts on elapsed time as well as on the
    /// error — a cap that allocated first would not return instantly.
    #[test]
    fn anthropic_content_block_index_cap() {
        fn block_start(index: &str, kind: &str) -> String {
            match kind {
                "tool_use" => format!(
                    "data: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"get_trait_details\",\"input\":{{}}}}}}\n"
                ),
                _ => format!(
                    "data: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n"
                ),
            }
        }

        // One past the cap: rejected, with the status and wording the SSE half
        // uses so the two paths report identically.
        let over = block_start(&(MAX_TOOL_CALL_INDEX + 1).to_string(), "text");
        match read_anthropic_stream(over.as_bytes(), &|| false)
            .expect_err("out-of-range index must fail")
        {
            LlmError::Api { status, message } => {
                assert_eq!(status, 502);
                assert!(message.contains("exceeds"), "got: {message}");
            }
            other => panic!("expected Api error, got: {other}"),
        }

        // The shape that aborts the process, on both block kinds. Timed: the
        // guard must fire before the allocation, not after it.
        for kind in ["text", "tool_use"] {
            let hostile = block_start(&usize::MAX.to_string(), kind);
            let started = std::time::Instant::now();
            assert!(
                read_anthropic_stream(hostile.as_bytes(), &|| false).is_err(),
                "usize::MAX index must be rejected ({kind})"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1),
                "the cap must reject before allocating, not after ({kind})"
            );
        }

        // The last legal slot still assembles, so the cap is not off by one.
        let last = MAX_TOOL_CALL_INDEX.to_string();
        let ok = format!(
            "{}data: {{\"type\":\"content_block_delta\",\"index\":{last},\"delta\":{{\"type\":\"text_delta\",\"text\":\"hi\"}}}}\ndata: {{\"type\":\"message_stop\"}}\n",
            block_start(&last, "text")
        );
        let body =
            read_anthropic_stream(ok.as_bytes(), &|| false).expect("in-range index must parse");
        let blocks = body.content.expect("content");
        assert_eq!(blocks.len(), 1, "sparse slots must not become blocks");
        assert_eq!(extract_text(&blocks).as_deref(), Some("hi"));
    }

    #[test]
    fn test_read_anthropic_stream_assembles_text_blocks() {
        let sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1"}}
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}
event: content_block_stop
data: {"type":"content_block_stop","index":0}
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}
event: message_stop
data: {"type":"message_stop"}
"#;
        let body = read_anthropic_stream(sse.as_bytes(), &|| false).expect("stream parses");
        match body.content.as_deref() {
            Some([ContentBlock::Text { text }]) => assert_eq!(text, "Hello"),
            other => panic!("expected one text block, got {other:?}"),
        }
        assert_eq!(body.stop_reason.as_deref(), Some("end_turn"));
    }

    /// Input tokens arrive on `message_start`, cumulative output tokens on
    /// `message_delta` (Anthropic streaming docs).
    #[test]
    fn read_anthropic_stream_records_start_and_delta_usage() {
        let sse = r#"data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":1200,"cache_read_input_tokens":300,"output_tokens":1}}}
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}
"#;
        let scope = crate::llm::usage::UsageScope::new();
        read_anthropic_stream(sse.as_bytes(), &|| false).expect("stream parses");
        let run = scope.total();
        assert_eq!(run.tokens.prompt, 1500);
        assert_eq!(run.tokens.completion, 42);
        assert_eq!(run.tokens.total, 1542);
        assert_eq!(run.tokens.requests, 1);
    }

    #[test]
    fn test_read_anthropic_stream_assembles_tool_use_json() {
        let sse = r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"pick","input":{}}}
event: content_block_delta
        data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"slot\":"}}
        data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"heal\"}"}}
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}
"#;
        let body = read_anthropic_stream(sse.as_bytes(), &|| false).expect("stream parses");
        match body.content.as_deref() {
            Some([ContentBlock::ToolUse { id, name, input }]) => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "pick");
                assert_eq!(input["slot"], "heal");
            }
            other => panic!("expected one tool_use block, got {other:?}"),
        }
        assert_eq!(body.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn test_read_anthropic_stream_error_event_maps_to_api_error() {
        let sse = r#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}
"#;
        match read_anthropic_stream(sse.as_bytes(), &|| false) {
            Err(LlmError::Api { status, message }) => {
                assert_eq!(status, 529);
                assert!(message.contains("Overloaded"));
            }
            _other => panic!("expected Api error"),
        }
    }
}
