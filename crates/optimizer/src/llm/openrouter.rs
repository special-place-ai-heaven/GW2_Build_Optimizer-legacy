//! OpenRouter provider — implements `LlmClient` for any model hosted on
//! https://openrouter.ai. OpenRouter exposes an OpenAI-compatible Chat
//! Completions API (with tools/function calling) and a `/models` endpoint
//! that lists every supported model across providers (Anthropic, OpenAI,
//! Google, Mistral, Meta, etc.). One API key, many models.
//!
//! Differences from the OpenAI provider:
//!  - Base URL: `https://openrouter.ai/api/v1`
//!  - Optional `HTTP-Referer` + `X-Title` headers identify this app to
//!    OpenRouter's analytics/leaderboards.
//!  - Model IDs are slash-prefixed (`anthropic/claude-sonnet-4-5`).
//!
//! API key is sent via `Authorization: Bearer <key>` header, same as OpenAI.
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value;

use super::body::{json_capped, read_body_capped};
use super::openai_compat::{
    http_client, send_chat, Message, ProviderCore, CHAT_REQUEST_TIMEOUT, MAX_COMPLETION_TOKENS,
    METADATA_TIMEOUT, REASONING_EFFORT,
};
use super::rate::{persist_usage, PersistedUsage, RateTracker};
use super::trim::trim_openai_messages;
use super::{http_error, KeyValidationResult, LlmClient, LlmError, ToolDefinition};

/// OpenRouter's conservative default requests-per-minute ceiling.
const RPM_LIMIT: u32 = 60;
/// Deadline for one lookup round on a free model. See `send_chat`.
const FREE_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";
/// Identify this app in OpenRouter's request logs / leaderboards. Optional
/// but recommended by OpenRouter docs.
const OPENROUTER_HTTP_REFERER: &str =
    "https://github.com/special-place-ai-heaven/GW2_Build_Optimizer";
const OPENROUTER_X_TITLE: &str = "GW2 Build Optimizer";

pub struct OpenRouterClient {
    api_key: String,
    model: String,
    http: reqwest::blocking::Client,
    cache: crate::llm::response_cache::ResponseCache,
    rate: Mutex<RateTracker>,
    usage_path: Option<PathBuf>,
    /// What the catalog says this model will accept, fetched once.
    ///
    /// Every model in OpenRouter's catalog answers differently, and until
    /// this existed we sent the same request to all of them: a completion
    /// budget 36% of them cannot serve, and a reasoning effort 34 of them
    /// reject outright — including the highest-scoring free model there is.
    caps: std::sync::OnceLock<super::ModelInfo>,
}

impl OpenRouterClient {
    pub fn new(api_key: &str, model: &str) -> Result<Self, LlmError> {
        let http = http_client()?;
        Ok(Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            http,
            cache: crate::llm::response_cache::ResponseCache::new(1800, 64),
            rate: Mutex::new(RateTracker::new(RPM_LIMIT)),
            usage_path: None,
            caps: std::sync::OnceLock::new(),
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
            caps: std::sync::OnceLock::new(),
        })
    }

    /// What the catalog says about the model in use, fetched once per client.
    ///
    /// A catalog we cannot reach yields the default, which claims nothing:
    /// the request then goes out exactly as it did before this existed. A
    /// model missing from the catalog does the same. Neither is worth failing
    /// a chat over — the point is to send a *better* request when we know
    /// enough to, not to make knowing a precondition for talking at all.
    fn caps(&self) -> &super::ModelInfo {
        self.caps.get_or_init(|| {
            self.list_models()
                .ok()
                .and_then(|models| models.into_iter().find(|m| m.id == self.model))
                .unwrap_or_default()
        })
    }

    fn send_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message, LlmError> {
        // A free endpoint that takes three minutes on one lookup round is
        // the run (openrouter/free in-game 2026-09-07: 36 s, 48 s, 179 s,
        // then the closing request timed out at seven minutes total). A
        // lookup that slow is abandoned; the loop closes on what it has.
        let timeout = if self.thrifty() {
            FREE_LOOKUP_TIMEOUT
        } else {
            CHAT_REQUEST_TIMEOUT
        };
        self.send_chat_capped(
            messages,
            tools,
            MAX_COMPLETION_TOKENS,
            Some(REASONING_EFFORT),
            timeout,
            None,
            None,
        )
    }

    /// The request that writes the plate from what the tool rounds gathered:
    /// small cap, no reasoning budget, its own deadline — and, where the
    /// catalog says this model takes it, the plate's JSON Schema as
    /// `response_format`, so the API holds the shape instead of the prompt
    /// asking for it. See [`super::openai_compat::CLOSING_MAX_TOKENS`].
    fn send_closing(&self, messages: &[Message]) -> Result<Message, LlmError> {
        // OpenRouter's parameter docs: `structured_outputs` means the model
        // takes a JSON Schema and holds to it; `response_format` alone means
        // JSON mode - valid JSON guaranteed, shape not. minimax-m3:free lists
        // the second and not the first (catalog read 2026-09-07), and given
        // the schema anyway it answered the conversational shape with the
        // whole build inside "explanation". Ask each model for what it can
        // actually do.
        let caps = self.caps();
        let schema_ok = caps.supports("structured_outputs")
            || super::models_dev::facts("openrouter", &self.model)
                .is_some_and(|f| f.structured_output);
        let response_format = if schema_ok {
            Some(super::openai_compat::plate_response_format())
        } else if caps.supports("response_format") {
            Some(serde_json::json!({ "type": "json_object" }))
        } else {
            None
        };
        self.send_chat_capped(
            messages,
            None,
            super::openai_compat::CLOSING_MAX_TOKENS,
            None,
            super::openai_compat::CLOSING_REQUEST_TIMEOUT,
            response_format,
            None,
        )
    }

    /// `send_chat` with an explicit completion budget. `generate_brief` passes
    /// a small cap and no reasoning budget so a three-line answer cannot think
    /// for minutes.
    // Eight knobs because eight things vary per request; a struct would be
    // the same eight lines at every call site.
    #[allow(clippy::too_many_arguments)]
    fn send_chat_capped(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        max_tokens: u32,
        reasoning_effort: Option<&'static str>,
        request_timeout: std::time::Duration,
        response_format: Option<Value>,
        tool_choice: Option<&'static str>,
    ) -> Result<Message, LlmError> {
        let extra_headers = [
            ("HTTP-Referer", OPENROUTER_HTTP_REFERER.to_string()),
            ("X-Title", OPENROUTER_X_TITLE.to_string()),
        ];
        let is_cancelled = super::cancel::is_cancelled;
        let caps = self.caps();
        // Asking for more than the model can produce does not get truncated,
        // it narrows the routing pool: OpenRouter only routes to providers
        // that can serve the `max_tokens` requested. 153 of 431 models
        // publish a ceiling below the one we used to send unconditionally.
        let max_tokens = caps.completion_budget(max_tokens);
        // And the effort has to be one this model lists. `glm-5.2:free`, the
        // highest-scoring free model in the catalog, accepts only `xhigh` and
        // `high` — our old constant `medium` was simply invalid there.
        // Free models think at "low". Given "medium", a free reasoning model
        // spent its whole closing budget on reasoning and returned no content
        // (cohere/north-mini-code via openrouter/free, 2026-09-07).
        let reasoning_effort = reasoning_effort
            .map(|preferred| if caps.free { "low" } else { preferred })
            .and_then(|preferred| caps.effort(preferred));
        let core = ProviderCore {
            tool_choice,
            http: &self.http,
            rate: &self.rate,
            api_key: &self.api_key,
            base_url: OPENROUTER_API_BASE,
            model: &self.model,
            extra_headers: &extra_headers,
            label: "OpenRouter",
            max_tokens,
            reasoning_effort,
            // OpenRouter is the one base URL that understands the top-level
            // `provider` routing block.
            supports_provider_prefs: true,
            require_tool_endpoints: tools.is_some(),
            // A free model has no price to sort by; sort its hosts by speed.
            provider_sort: caps.free.then_some("throughput"),
            response_format,
            request_timeout,
            max_retries: 2,
            stream_usage: false,
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

impl super::tool_loop::TurnDriver for OpenRouterClient {
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
        LlmError::Parse(format!("OpenRouter: {detail}"))
    }
    fn is_deadline(&self, err: &LlmError) -> bool {
        super::openai_compat::is_deadline(err)
    }
    fn is_function_call_failure(&self, err: &LlmError) -> bool {
        super::openai_compat::is_function_call_failure(err)
    }
}

impl LlmClient for OpenRouterClient {
    fn thrifty(&self) -> bool {
        // The catalog's price-is-zero rule first (three free models carry
        // no `:free` suffix); the id second, because `caps()` on a catalog
        // miss is `ModelInfo::default()` with `free: false`, and nine
        // requests is a dear way to discover you were poor.
        self.caps().free || self.model.ends_with(":free") || self.model == "openrouter/free"
    }
    fn provider_name(&self) -> &str {
        "OpenRouter"
    }

    fn validate_key(&self) -> Result<(), LlmError> {
        let url = format!("{}/models", OPENROUTER_API_BASE);
        let resp = self
            .http
            .get(&url)
            .timeout(METADATA_TIMEOUT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .map_err(http_error)?;

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
        let url = format!("{}/models", OPENROUTER_API_BASE);
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
                    message: "Cannot connect to OpenRouter API. Check your internet connection."
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
                message: "OpenRouter key validated successfully!".into(),
                warning: None,
            },
            401 => KeyValidationResult {
                valid: false,
                message: "Invalid OpenRouter API key. Check that you copied the full key from openrouter.ai/keys.".into(),
                warning: None,
            },
            429 => {
                let warning = if super::has_billing_keyword(&body) {
                    "Your account has exceeded its usage limit. Check credits at openrouter.ai/credits."
                } else {
                    "Currently rate-limited. Try again shortly."
                };
                KeyValidationResult {
                    valid: true,
                    message: "OpenRouter key is valid!".into(),
                    warning: Some(warning.into()),
                }
            }
            _ => {
                if super::has_billing_keyword(&body) {
                    KeyValidationResult {
                        valid: true,
                        message: "OpenRouter key is valid!".into(),
                        warning: Some("Your account may be out of credits. Top up at openrouter.ai/credits.".into()),
                    }
                } else {
                    KeyValidationResult {
                        valid: false,
                        message: format!("OpenRouter API error (HTTP {}).", status),
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
            .ok_or_else(|| LlmError::Parse("No response text from OpenRouter".into()))
    }

    fn generate_brief(&self, prompt: &str, max_tokens: u32) -> Result<String, LlmError> {
        let messages = vec![Message {
            role: "user".to_string(),
            content: Some(prompt.to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_details: None,
        }];
        let response = self.send_chat_capped(
            &messages,
            None,
            max_tokens,
            None,
            CHAT_REQUEST_TIMEOUT,
            None,
            None,
        )?;
        response
            .content
            .ok_or_else(|| LlmError::Parse("No response text from OpenRouter".into()))
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
        // OpenRouter `/models` endpoint returns ALL hosted models across every
        // upstream provider (Anthropic, OpenAI, Google, Mistral, Meta, etc.).
        // Unlike OpenAI's `/models` (which mixes in audio/image/embedding
        // endpoints under one account), OpenRouter's catalog is already
        // pre-filtered to chat-capable LLMs — no OpenAI-style include/exclude
        // prefix list needed. We just deserialize, sort, and surface them.
        let url = format!("{}/models", OPENROUTER_API_BASE);
        let resp = self
            .http
            .get(&url)
            .timeout(METADATA_TIMEOUT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", OPENROUTER_HTTP_REFERER)
            .header("X-Title", OPENROUTER_X_TITLE)
            .send()
            .map_err(http_error)?;

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
            /// Human-readable name when OpenRouter provides one (e.g.
            /// "Claude Sonnet 4.5"). Falls back to the slug when missing.
            #[serde(default)]
            name: Option<String>,
            /// Absent on nothing in the live catalog, but absent means
            /// unknown, and unknown must not read as free.
            #[serde(default)]
            pricing: Option<Pricing>,
            #[serde(default)]
            supported_parameters: Option<Vec<String>>,
            #[serde(default)]
            architecture: Option<Architecture>,
            #[serde(default)]
            top_provider: Option<TopProvider>,
            #[serde(default)]
            reasoning: Option<Reasoning>,
            #[serde(default)]
            context_length: Option<u32>,
            #[serde(default)]
            benchmarks: Option<Benchmarks>,
            /// "The date after which the model may be removed."
            #[serde(default)]
            expiration_date: Option<String>,
        }
        #[derive(Deserialize)]
        struct Architecture {
            #[serde(default)]
            output_modalities: Option<Vec<String>>,
        }
        /// The primary provider's numbers, not the model's — endpoints
        /// disagree, and this is the one OpenRouter puts forward.
        #[derive(Deserialize)]
        struct TopProvider {
            #[serde(default)]
            max_completion_tokens: Option<u32>,
        }
        #[derive(Deserialize)]
        struct Reasoning {
            #[serde(default)]
            supported_efforts: Option<Vec<String>>,
        }
        #[derive(Deserialize)]
        struct Benchmarks {
            #[serde(default)]
            artificial_analysis: Option<ArtificialAnalysis>,
        }
        #[derive(Deserialize)]
        struct ArtificialAnalysis {
            #[serde(default)]
            agentic_index: Option<f32>,
            #[serde(default)]
            coding_index: Option<f32>,
        }
        /// Prices arrive as decimal STRINGS — `"0"`, `"0.00001"` — not
        /// numbers, so they are parsed rather than compared as text: `"0.0"`
        /// and `"0"` both mean free.
        #[derive(Deserialize)]
        struct Pricing {
            #[serde(default)]
            prompt: Option<String>,
            #[serde(default)]
            completion: Option<String>,
        }

        let body: ModelsResponse = json_capped(resp)?;
        let entries = body.data.unwrap_or_default();

        let mut models: Vec<super::ModelInfo> = entries
            .into_iter()
            .map(|m| super::ModelInfo {
                // Priced at zero both ways. Measured against the live
                // catalogue on 2026-09-06: 22 of 431 models, of which 19
                // carry the `:free` suffix and three do not — `openrouter/free`
                // and two Lyria previews. So the price is the fact and the
                // suffix is only a habit; reading the suffix would have
                // missed three and would break the day they rename one.
                supported_parameters: m.supported_parameters.clone().unwrap_or_default(),
                structured_output: None,
                free: m.pricing.as_ref().is_some_and(|p| {
                    let zero = |v: &Option<String>| {
                        v.as_deref()
                            .and_then(|s| s.parse::<f64>().ok())
                            .is_some_and(|n| n == 0.0)
                    };
                    zero(&p.prompt) && zero(&p.completion)
                }),
                // Model-level `supported_parameters` is the UNION across this
                // model's provider endpoints, and OpenRouter calls tool
                // routing "best effort" — so this is a necessary condition,
                // not a guarantee. Absent, though, is a definite no.
                tools: m
                    .supported_parameters
                    .as_ref()
                    .is_some_and(|p| p.iter().any(|x| x == "tools")),
                text_output: m
                    .architecture
                    .as_ref()
                    .and_then(|a| a.output_modalities.as_ref())
                    .is_some_and(|out| out.iter().any(|x| x == "text")),
                max_completion_tokens: m.top_provider.and_then(|t| t.max_completion_tokens),
                supported_efforts: m
                    .reasoning
                    .and_then(|r| r.supported_efforts)
                    .unwrap_or_default(),
                context_length: m.context_length,
                agentic_index: m
                    .benchmarks
                    .as_ref()
                    .and_then(|b| b.artificial_analysis.as_ref())
                    .and_then(|a| a.agentic_index),
                coding_index: m
                    .benchmarks
                    .as_ref()
                    .and_then(|b| b.artificial_analysis.as_ref())
                    .and_then(|a| a.coding_index),
                expires: m.expiration_date,
                display_name: m.name.unwrap_or_else(|| m.id.clone()),
                id: m.id,
            })
            .collect();

        // Best first, then alphabetically within a band. Alphabetical alone
        // put `anthropic/*` at the top of 431 rows and left the model someone
        // should actually pick two hundred lines down.
        models.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| a.id.cmp(&b.id)));

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
    use super::super::openai_compat::{
        FunctionCallData, OpenAiFunction, OpenAiTool, ToolCallResponse,
    };
    use super::super::sse::{read_stream, StreamedMessage};
    use super::*;

    #[test]
    fn test_provider_name() {
        let client = OpenRouterClient::new("fake-key", "anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(client.provider_name(), "OpenRouter");
    }

    #[test]
    fn test_remaining_quota_default() {
        let client = OpenRouterClient::new("fake-key", "gpt-4o").unwrap();
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
        assert_eq!(tool_calls[0].function.name, "square_number");
        assert_eq!(tool_calls[0].function.arguments, "{\"number\":7}");

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
