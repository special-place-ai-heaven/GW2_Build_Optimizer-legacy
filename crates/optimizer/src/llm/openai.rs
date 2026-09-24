//! OpenAI provider — implements `LlmClient` for GPT-4o and compatible models.
//! Uses the OpenAI Chat Completions API with function calling.
//! API key is sent via `Authorization: Bearer <key>` header.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value;

use super::body::{json_capped, read_body_capped};
use super::openai_compat::{
    http_client, send_chat, Message, ProviderCore, CHAT_REQUEST_TIMEOUT, MAX_COMPLETION_TOKENS,
    METADATA_TIMEOUT,
};
use super::rate::{persist_usage, PersistedUsage, RateTracker};
use super::trim::trim_openai_messages;
use super::{KeyValidationResult, LlmClient, LlmError, ToolDefinition};

/// OpenAI's conservative default requests-per-minute ceiling.
const RPM_LIMIT: u32 = 60;

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";

pub struct OpenAiClient {
    api_key: String,
    model: String,
    http: reqwest::blocking::Client,
    cache: crate::llm::response_cache::ResponseCache,
    rate: Mutex<RateTracker>,
    usage_path: Option<PathBuf>,
}

impl OpenAiClient {
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

    fn send_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message, LlmError> {
        self.send_chat_capped(
            messages,
            tools,
            MAX_COMPLETION_TOKENS,
            CHAT_REQUEST_TIMEOUT,
            None,
        )
    }

    /// The request that writes the plate: small cap, its own deadline. See
    /// [`super::openai_compat::CLOSING_MAX_TOKENS`].
    fn send_closing(&self, messages: &[Message]) -> Result<Message, LlmError> {
        self.send_chat_capped(
            messages,
            None,
            super::openai_compat::CLOSING_MAX_TOKENS,
            super::openai_compat::CLOSING_REQUEST_TIMEOUT,
            None,
        )
    }

    /// `send_chat` with an explicit completion budget (see `generate_brief`).
    fn send_chat_capped(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        max_tokens: u32,
        request_timeout: std::time::Duration,
        tool_choice: Option<&'static str>,
    ) -> Result<Message, LlmError> {
        let extra_headers: [(&str, String); 0] = [];
        let is_cancelled = super::cancel::is_cancelled;
        let core = ProviderCore {
            tool_choice,
            http: &self.http,
            rate: &self.rate,
            api_key: &self.api_key,
            base_url: OPENAI_API_BASE,
            model: &self.model,
            extra_headers: &extra_headers,
            label: "OpenAI",
            // Same ceiling as OpenRouter: reasoning models share this budget
            // between thinking and the answer.
            max_tokens,
            // `reasoning` and `provider` are OpenRouter extensions.
            // `api.openai.com` rejects unknown top-level body arguments, so
            // sending either here is a 400 on every request (Claude F8).
            reasoning_effort: None,
            supports_provider_prefs: false,
            require_tool_endpoints: tools.is_some(),
            provider_sort: None,
            response_format: None,
            request_timeout,
            max_retries: 2,
            stream_usage: true,
            is_cancelled: &is_cancelled,
        };
        let message = send_chat(core, messages, tools)?;
        // Persist usage
        {
            let rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
            self.persist_usage(&rate);
        }
        Ok(message)
    }

    fn persist_usage(&self, rate: &RateTracker) {
        persist_usage(self.usage_path.as_deref(), rate);
    }
}

impl super::tool_loop::TurnDriver for OpenAiClient {
    type Conv = Vec<Message>;
    type Err = LlmError;

    fn open(&self, prompt: &str) -> Vec<Message> {
        let mut conv = Vec::new();
        super::openai_compat::push_user(&mut conv, prompt);
        conv
    }
    fn trim(&self, conv: &mut Vec<Message>) {
        trim_openai_messages(conv, super::trim::SAFE_PROMPT_BUDGET_TOKENS);
    }
    fn turn(
        &self,
        conv: &mut Vec<Message>,
        tools: Option<&[ToolDefinition]>,
        mode: super::tool_loop::TurnMode,
    ) -> Result<super::tool_loop::Turn, LlmError> {
        let message = match (mode, tools) {
            (super::tool_loop::TurnMode::Explore, tools) => self.send_chat(conv, tools)?,
            (super::tool_loop::TurnMode::Closing, _) => self.send_closing(conv)?,
        };
        Ok(super::openai_compat::absorb_turn(conv, message))
    }
    fn push_tool_results(
        &self,
        conv: &mut Vec<Message>,
        results: &[(super::tool_loop::ToolCall, Value)],
    ) {
        super::openai_compat::push_tool_results(conv, results);
    }
    fn push_user(&self, conv: &mut Vec<Message>, text: &str) {
        super::openai_compat::push_user(conv, text);
    }
    fn cancelled(&self) -> LlmError {
        LlmError::Unavailable(super::cancel::CANCELLED.to_string())
    }
    fn no_answer(&self, detail: String) -> LlmError {
        LlmError::Parse(format!("OpenAI: {detail}"))
    }
    fn is_deadline(&self, err: &LlmError) -> bool {
        super::openai_compat::is_deadline(err)
    }
    fn is_function_call_failure(&self, err: &LlmError) -> bool {
        super::openai_compat::is_function_call_failure(err)
    }
}

impl LlmClient for OpenAiClient {
    fn provider_name(&self) -> &str {
        "OpenAI"
    }

    fn validate_key(&self) -> Result<(), LlmError> {
        let url = format!("{}/models", OPENAI_API_BASE);
        let resp = self
            .http
            .get(&url)
            .timeout(METADATA_TIMEOUT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .map_err(|e| LlmError::Http(e.to_string()))?;

        match resp.status().as_u16() {
            200 => Ok(()),
            401 => Err(LlmError::InvalidKey),
            429 => Ok(()), // Rate limited means key is valid
            status => {
                let body = read_body_capped(resp);
                // Billing/quota errors mean the key is valid but account has issues
                if body.contains("billing")
                    || body.contains("quota")
                    || body.contains("exceeded")
                    || body.contains("insufficient")
                {
                    Ok(())
                } else {
                    Err(LlmError::Api {
                        status,
                        message: body,
                    })
                }
            }
        }
    }

    fn validate_key_detailed(&self) -> KeyValidationResult {
        let url = format!("{}/models", OPENAI_API_BASE);
        let resp = match self
            .http
            .get(&url)
            .timeout(METADATA_TIMEOUT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                return KeyValidationResult {
                    valid: false,
                    message: "Cannot connect to OpenAI API. Check your internet connection.".into(),
                    warning: Some(e.to_string()),
                };
            }
        };

        let status = resp.status().as_u16();
        let body = read_body_capped(resp);

        match status {
            200 => KeyValidationResult {
                valid: true,
                message: "OpenAI key validated successfully!".into(),
                warning: None,
            },
            401 => KeyValidationResult {
                valid: false,
                message: "Invalid OpenAI API key. Check that you copied the full key from platform.openai.com/api-keys.".into(),
                warning: None,
            },
            429 => {
                let warning = if super::has_billing_keyword(&body) {
                    "Your account has exceeded its usage limit. Check billing at platform.openai.com/account/billing."
                } else {
                    "Currently rate-limited. Try again shortly."
                };
                KeyValidationResult {
                    valid: true,
                    message: "OpenAI key is valid!".into(),
                    warning: Some(warning.into()),
                }
            }
            _ => {
                if super::has_billing_keyword(&body) {
                    KeyValidationResult {
                        valid: true,
                        message: "OpenAI key is valid!".into(),
                        warning: Some("Your account may have billing issues. Check platform.openai.com/account/billing.".into()),
                    }
                } else {
                    KeyValidationResult {
                        valid: false,
                        message: format!("OpenAI API error (HTTP {}).", status),
                        warning: if body.is_empty() { None } else { Some(body) },
                    }
                }
            }
        }
    }

    fn generate(&self, prompt: &str) -> Result<String, LlmError> {
        let messages = vec![Message {
            role: "user".to_string(),
            content: Some(prompt.to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_details: None,
        }];

        let response = self.send_chat(&messages, None)?;
        response
            .content
            .ok_or_else(|| LlmError::Parse("No response text from OpenAI".into()))
    }

    fn generate_brief(&self, prompt: &str, max_tokens: u32) -> Result<String, LlmError> {
        let messages = vec![Message {
            role: "user".to_string(),
            content: Some(prompt.to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_details: None,
        }];
        let response =
            self.send_chat_capped(&messages, None, max_tokens, CHAT_REQUEST_TIMEOUT, None)?;
        response
            .content
            .ok_or_else(|| LlmError::Parse("No response text from OpenAI".into()))
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
        let url = format!("{}/models", OPENAI_API_BASE);
        let resp = self
            .http
            .get(&url)
            .timeout(METADATA_TIMEOUT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .map_err(|e| LlmError::Http(e.to_string()))?;

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
            #[allow(dead_code)]
            created: Option<u64>,
        }

        let body: ModelsResponse = json_capped(resp)?;

        let entries = body.data.unwrap_or_default();

        // Filter to chat-capable models; exclude embeddings, image, audio, etc.
        let exclude_patterns = [
            "embedding",
            "dall-e",
            "whisper",
            "tts",
            "babbage",
            "davinci",
            "moderation",
        ];
        let include_prefixes = ["gpt-", "o1", "o3", "o4", "chatgpt-"];

        let mut models: Vec<super::ModelInfo> = entries
            .into_iter()
            .filter(|m| {
                let id = m.id.to_lowercase();
                // Must match at least one include prefix
                let included = include_prefixes.iter().any(|p| id.starts_with(p));
                // Must not match any exclude pattern
                let excluded = exclude_patterns.iter().any(|p| id.contains(p));
                included && !excluded
            })
            .map(|m| {
                let display = openai_display_name(&m.id);
                super::ModelInfo {
                    id: m.id,
                    display_name: display,
                    // OpenAI publishes no free tier and no per-model price in
                    // its catalog: every model here bills. Nor does it publish
                    // capabilities, so the rest stays at its default of "not
                    // stated" rather than being invented here.
                    free: false,
                    ..Default::default()
                }
            })
            .collect();

        // Sort: newer/better models first (gpt-4o before gpt-4o-mini, o3 before o1)
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

/// Derive a human-readable display name from an OpenAI model ID.
fn openai_display_name(id: &str) -> String {
    match id {
        "gpt-4o" => "GPT-4o".into(),
        "gpt-4o-mini" => "GPT-4o Mini".into(),
        "gpt-4-turbo" => "GPT-4 Turbo".into(),
        "gpt-4" => "GPT-4".into(),
        "gpt-3.5-turbo" => "GPT-3.5 Turbo".into(),
        "o1" => "o1 (reasoning)".into(),
        "o1-mini" => "o1-mini (reasoning)".into(),
        "o3-mini" => "o3-mini (reasoning)".into(),
        "chatgpt-4o-latest" => "ChatGPT-4o Latest".into(),
        _ => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::openai_compat::{
        FunctionCallData, OpenAiFunction, OpenAiTool, ToolCallResponse,
    };
    use super::super::sse::{read_stream, StreamedMessage};
    use super::*;

    #[test]
    fn test_provider_name() {
        let client = OpenAiClient::new("fake-key", "gpt-4o").unwrap();
        assert_eq!(client.provider_name(), "OpenAI");
    }

    /// Claude F37 — `llm::create_client` builds a brand new client for every
    /// user action, so an in-memory-only minute window always started empty
    /// and the RPM limit was never actually enforced. This drives the exact
    /// production path: `with_persistence`, one usage file, a second client
    /// over the same file, inside the same minute.
    #[test]
    fn rate_minute_window_survives_create_client() {
        let path = std::env::temp_dir().join(format!(
            "gw2bo_rate_window_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        // First client: spend the whole minute allowance and persist it the
        // way a successful send does.
        let first =
            OpenAiClient::with_persistence("fake-key", "gpt-4o", path.clone()).expect("client");
        {
            let mut rate = first.rate.lock().expect("rate");
            for _ in 0..RPM_LIMIT {
                rate.check_and_reserve().expect("within the minute limit");
            }
            first.persist_usage(&rate);
        }

        // Second client — what the next Choya message or Optimize builds.
        let second =
            OpenAiClient::with_persistence("fake-key", "gpt-4o", path.clone()).expect("client");
        let mut rate = second.rate.lock().expect("rate");
        assert_eq!(
            rate.requests_this_minute(),
            RPM_LIMIT,
            "a fresh client must inherit the minute window, not reset it"
        );
        assert!(
            matches!(rate.check_and_reserve(), Err(LlmError::RateLimited(_))),
            "the RPM limit must still bite after create_client"
        );
        drop(rate);

        let _ = std::fs::remove_file(&path);
    }

    /// A cancelled worker must leave the tool loop without opening a socket.
    /// Port 1 is unbound: any network attempt here would be a slow failure,
    /// not the instant `Unavailable` a cancelled unload needs.
    #[test]
    fn tool_loop_stops_on_cancel_without_touching_the_network() {
        let client = OpenAiClient::new("fake-key", "gpt-4o").expect("client");
        let tools = [ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let _scope = super::super::cancel::CancelScope::new(|| true);

        let started = std::time::Instant::now();
        let error = client
            .generate_with_tools_progress(
                "prompt",
                &tools,
                &mut |_, _| Value::Null,
                8,
                &mut |_, _, _| {},
            )
            .expect_err("cancelled loop must fail");

        assert!(
            matches!(error, LlmError::Unavailable(ref m) if m == super::super::cancel::CANCELLED),
            "got: {error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "cancel must be observed before any connect attempt"
        );
    }

    /// A usage file from before the minute window was persisted must still
    /// load — losing the daily counter to a strict parse would be worse.
    #[test]
    fn legacy_usage_file_still_loads() {
        let path = std::env::temp_dir().join(format!(
            "gw2bo_rate_legacy_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(
            &path,
            format!(
                "{{\"day\":{},\"requests_today\":3}}",
                super::super::rate::current_epoch_day()
            ),
        )
        .expect("write legacy usage");

        let client =
            OpenAiClient::with_persistence("fake-key", "gpt-4o", path.clone()).expect("client");
        let mut rate = client.rate.lock().expect("rate");
        assert_eq!(rate.requests_this_minute(), 0);
        assert!(rate.check_and_reserve().is_ok());
        drop(rate);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_remaining_quota_default() {
        let client = OpenAiClient::new("fake-key", "gpt-4o").unwrap();
        assert_eq!(client.remaining_quota(), 10000);
    }

    #[test]
    fn test_tool_definition_to_openai_format() {
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

        let openai_tools: Vec<OpenAiTool> = defs
            .iter()
            .map(|td| OpenAiTool {
                tool_type: "function".to_string(),
                function: OpenAiFunction {
                    name: td.name.clone(),
                    description: td.description.clone(),
                    parameters: td.parameters.clone(),
                },
            })
            .collect();

        assert_eq!(openai_tools.len(), 1);
        assert_eq!(openai_tools[0].tool_type, "function");
        assert_eq!(openai_tools[0].function.name, "get_profession_info");

        // Verify serialization format
        let json = serde_json::to_value(&openai_tools[0]).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "get_profession_info");
    }

    #[test]
    fn test_parse_tool_call_arguments_as_string() {
        // OpenAI sends arguments as a JSON string, not an object
        let tc = ToolCallResponse {
            id: "call_abc123".into(),
            call_type: "function".into(),
            function: FunctionCallData {
                name: "get_profession_info".into(),
                arguments: r#"{"profession":"Warrior"}"#.into(),
            },
        };

        let args: Value = serde_json::from_str(&tc.function.arguments).unwrap();
        assert_eq!(args["profession"], "Warrior");
    }

    #[test]
    fn test_message_serialization() {
        // User message
        let msg = Message {
            role: "user".to_string(),
            content: Some("Hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_details: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Hello");
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("tool_call_id").is_none());

        // Tool response message
        let tool_msg = Message {
            role: "tool".to_string(),
            content: Some(r#"{"result": "ok"}"#.to_string()),
            tool_calls: None,
            tool_call_id: Some("call_abc123".to_string()),
            reasoning_details: None,
        };
        let json = serde_json::to_value(&tool_msg).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_abc123");
    }

    #[test]
    fn test_tool_call_id_round_trip() {
        // Simulate a streamed server response carrying an assistant message
        // with one tool call — accumulated the same way `send_chat` does via
        // `read_stream` — then echo the id back on a tool-role follow-up, the
        // same path `generate_with_tools_progress` uses.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_fRzHUzNm7\",\"type\":\"function\",\"function\":{\"name\":\"square_number\",\"arguments\":\"{\\\"number\\\":7}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        );

        let assistant_msg = match read_stream(sse.as_bytes(), &|| false).expect("stream parses") {
            StreamedMessage::Message(m) => m,
            StreamedMessage::Empty(finish) => panic!("unexpected empty stream: {finish}"),
        };
        let tool_calls = assistant_msg.tool_calls.clone().expect("tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_fRzHUzNm7");

        let follow_up = Message {
            role: "tool".into(),
            content: Some(r#"{"result":49}"#.into()),
            tool_calls: None,
            tool_call_id: Some(tool_calls[0].id.clone()),
            reasoning_details: None,
        };
        let wire = serde_json::to_value(&follow_up).unwrap();
        assert_eq!(wire["role"], "tool");
        assert_eq!(wire["tool_call_id"], "call_fRzHUzNm7");
    }
}
