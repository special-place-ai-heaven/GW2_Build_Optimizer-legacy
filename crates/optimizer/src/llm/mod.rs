//! Provider-neutral LLM abstraction layer.
//! Defines the `LlmClient` trait that all AI providers implement,
//! plus shared types (`ToolDefinition`, `LlmError`) used across providers.

pub mod anthropic;
pub(crate) mod body;
pub mod cancel;
pub mod gemini;
pub mod live;
pub mod models_dev;
pub mod openai;
pub(crate) mod openai_compat;
pub mod openrouter;
pub mod pricing;
pub mod profile;
pub(crate) mod rate;
pub(crate) mod response_cache;
pub(crate) mod sse;
pub mod tool_loop;
pub mod tools;
pub(crate) mod trim;
pub mod usage;

use serde_json::Value;

/// Model info returned by a provider's model listing API.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Model ID used in API calls (e.g. "gpt-4o", "gemini-2.5-flash", "claude-sonnet-4-6").
    pub id: String,
    /// Human-readable display name (e.g. "GPT-4o", "Gemini 2.5 Flash", "Claude Sonnet 4.6").
    pub display_name: String,
    /// Whether this model can be used without paying.
    ///
    /// Most people who install this addon will make a free account and never
    /// spend anything, so which models are free is not trivia — it is the
    /// difference between a usable model list and 400 entries to rummage
    /// through. Each provider answers it from its own data where it can:
    /// OpenRouter publishes a price per model, Google publishes a free tier
    /// per model, OpenAI and Anthropic have neither.
    pub free: bool,
    /// Whether the model takes tool definitions at all.
    ///
    /// We send tools on every chat request, so a model without them cannot
    /// serve this addon. Defaults to true where a provider does not publish
    /// it: absent is not "no".
    pub tools: bool,
    /// Whether the model answers in text. The catalog carries image, audio
    /// and video generators alongside the chat models — not alternatives to a
    /// chat model, just a different thing in the same list.
    pub text_output: bool,
    /// The largest reply the model will produce, when published.
    ///
    /// Not decoration: OpenRouter routes only to providers that can serve the
    /// `max_tokens` asked for, so asking for more than this quietly narrows
    /// the pool. `None` means unpublished — send our own figure.
    pub max_completion_tokens: Option<u32>,
    /// Reasoning efforts the model accepts. Empty means unpublished.
    ///
    /// Sending an effort a model does not list is an error we would author
    /// ourselves, and it is not hypothetical: the highest-scoring free model
    /// in OpenRouter's catalogue accepts only `xhigh` and `high`.
    pub supported_efforts: Vec<String>,
    /// Total context, when published.
    pub context_length: Option<u32>,
    /// Everything the catalog says the model's endpoints accept - `tools`,
    /// `response_format`, `structured_outputs`, `reasoning`, ... Empty means
    /// unpublished (OpenAI, Anthropic, Google do not list it).
    pub supported_parameters: Vec<String>,
    /// Whether the model holds to a JSON Schema (`response_format: json_schema`),
    /// per models.dev. `None` means nobody has said. `Some(false)` is the free
    /// tier of a model whose paid tier can - the plate then comes from the
    /// repair request, not the API, and the picker says so.
    pub structured_output: Option<bool>,
    /// Artificial Analysis' agentic score, when published.
    ///
    /// The closest published measure of what this addon asks a model to do:
    /// call tools, respect hard rules, revise when refused. It orders the
    /// picker, so the best model is the first line rather than the two
    /// hundredth.
    pub agentic_index: Option<f32>,
    /// Artificial Analysis' coding score, when published. Wider coverage than
    /// [`Self::agentic_index`], so it orders the models that lack one.
    pub coding_index: Option<f32>,
    /// The date after which the provider may remove the model, if it said so.
    pub expires: Option<String>,
}

impl Default for ModelInfo {
    fn default() -> Self {
        Self {
            id: String::new(),
            display_name: String::new(),
            free: false,
            // Absent is not "no": a provider that publishes no capability
            // data must not have every one of its models pruned as useless.
            tools: true,
            text_output: true,
            max_completion_tokens: None,
            supported_efforts: Vec::new(),
            context_length: None,
            supported_parameters: Vec::new(),
            structured_output: None,
            agentic_index: None,
            coding_index: None,
            expires: None,
        }
    }
}

impl ModelInfo {
    /// Whether this model can serve a request from this addon at all.
    ///
    /// Only reasons no request shape can fix. A model that merely needs a
    /// smaller `max_tokens` or a different reasoning effort is not unusable —
    /// it is a request we have to build correctly, and pruning those would
    /// throw away half of OpenRouter's free catalogue.
    pub fn usable(&self) -> bool {
        self.tools && self.text_output && !self.id.ends_with(":batch") && self.expires.is_none()
    }

    /// The reasoning effort to send, given what we would prefer.
    ///
    /// Never MORE thinking than we asked for. Where our own choice is not on
    /// the model's list, this takes the dearest option at or below it, and
    /// only if there is none does it take the cheapest on offer.
    ///
    /// The order matters more than it looks. `z-ai/glm-5.3` lists
    /// `["max", "high", "low"]` and has mandatory reasoning; taking the first
    /// entry — which an earlier version of this did — asked a model that must
    /// think to think as hard as it can, on every message. Sorting by cost
    /// turns that into `low`.
    pub fn effort(&self, preferred: &str) -> Option<String> {
        if self.supported_efforts.is_empty()
            || self.supported_efforts.iter().any(|e| e == preferred)
        {
            return Some(preferred.to_string());
        }
        let cost = |e: &str| EFFORT_ORDER.iter().position(|x| *x == e);
        let want = cost(preferred)?;
        let mut listed: Vec<&String> = self.supported_efforts.iter().collect();
        listed.sort_by_key(|e| cost(e).unwrap_or(usize::MAX));
        listed
            .iter()
            .rev()
            .find(|e| cost(e).is_some_and(|c| c <= want))
            .or_else(|| listed.first())
            .map(|e| (*e).clone())
    }

    /// How many completion tokens to ask for, given our own ceiling.
    /// Whether the catalog lists `param` for this model. Unpublished is
    /// `false`: a request shape the catalog cannot vouch for is not sent.
    pub fn supports(&self, param: &str) -> bool {
        self.supported_parameters.iter().any(|p| p == param)
    }

    pub fn completion_budget(&self, ours: u32) -> u32 {
        self.max_completion_tokens.map_or(ours, |cap| ours.min(cap))
    }

    /// Order for the picker: best first.
    ///
    /// Agentic where it exists, then coding, then everything unscored. The
    /// two indices are kept in separate bands rather than mixed — a coding
    /// 52.6 and an agentic 39.7 are not the same number, and interleaving
    /// them would rank by which benchmark a model happened to publish.
    pub fn rank(&self) -> (u8, i32) {
        // A model the plate can be enforced on outranks one it cannot, inside
        // the same score band; an unknown sits between.
        let plate = match self.structured_output {
            Some(true) => 0,
            None => 1,
            Some(false) => 2,
        };
        match (self.agentic_index, self.coding_index) {
            (Some(a), _) => (0, -(a * 100.0) as i32 * 4 + plate),
            (None, Some(c)) => (1, -(c * 100.0) as i32 * 4 + plate),
            (None, None) => (2, plate),
        }
    }
}

/// Reasoning efforts from cheapest to dearest.
///
/// Providers name these freely and the list is not closed, so an effort that
/// is not here simply has no rank and is never chosen over one that does.
const EFFORT_ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "max", "xhigh"];

/// Result of a detailed key validation with user-friendly messages.
#[derive(Debug, Clone)]
pub struct KeyValidationResult {
    /// Whether the key is structurally valid (authentication passed).
    pub valid: bool,
    /// User-friendly status message (e.g. "Key validated successfully!").
    pub message: String,
    /// Optional warning about billing/quota (key valid but account has issues).
    pub warning: Option<String>,
}

/// Every chat request actually put on the wire, retries included. Read by
/// the `choya_live` example to report what a run really cost; the rounds a
/// progress callback sees are not the requests a quota counts.
pub static HTTP_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Provider-neutral error type for all LLM operations.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("Invalid API key")]
    InvalidKey,
    /// Carries the provider's own words: "temporarily rate-limited upstream"
    /// (their pool) and "free-models-per-day" (the player's cap) need
    /// different advice, and both used to arrive here as the same unit.
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("LLM unavailable: {0}")]
    Unavailable(String),
}

/// `reqwest`/`io` transport failure. Their `Display` is only the top
/// wrapper, so the UI would hide TLS causes unless we walk `source()`.
pub(crate) fn http_error(err: impl std::error::Error) -> LlmError {
    LlmError::Http(gw2_core::format_error_chain(&err))
}

/// Provider-neutral tool/function definition.
/// Each provider translates this to its own wire format internally.
/// Uses JSON Schema for parameters (common to Gemini, OpenAI, and Anthropic).
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the function parameters.
    pub parameters: Value,
}

/// A tool call returned by the LLM (provider has already parsed its native format).
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Provider-specific call ID (OpenAI: tool_call_id, Anthropic: tool_use_id, Gemini: none).
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

/// The provider-neutral LLM client trait.
///
/// Implementors handle all provider-specific concerns internally:
/// - API authentication and endpoint URLs
/// - Request/response format translation
/// - Rate limiting and quota tracking
/// - Tool call wire format (Gemini functionDeclarations, OpenAI functions, Anthropic tools)
///
/// All methods are `&self` — clients manage their own internal mutability via `Mutex`.
/// `Send + Sync` required because clients are shared across background threads.
pub trait LlmClient: Send + Sync {
    /// Human-readable provider name (e.g. "Gemini", "OpenAI", "Anthropic").
    fn provider_name(&self) -> &str;

    /// Validate the API key without consuming quota.
    fn validate_key(&self) -> Result<(), LlmError>;

    /// Validate key with detailed, user-friendly result.
    /// Default implementation wraps `validate_key()` with generic messages.
    /// Providers should override for billing/quota-specific feedback.
    fn validate_key_detailed(&self) -> KeyValidationResult {
        match self.validate_key() {
            Ok(()) => KeyValidationResult {
                valid: true,
                message: format!("{} key validated successfully!", self.provider_name()),
                warning: None,
            },
            Err(LlmError::InvalidKey) => KeyValidationResult {
                valid: false,
                message: format!(
                    "Invalid {} API key. Check that you copied the full key.",
                    self.provider_name()
                ),
                warning: None,
            },
            Err(LlmError::RateLimited(_)) => KeyValidationResult {
                valid: true,
                message: format!("{} key is valid.", self.provider_name()),
                warning: Some("Currently rate-limited. Try again shortly.".into()),
            },
            Err(LlmError::Http(ref msg)) => KeyValidationResult {
                valid: false,
                message: format!(
                    "Cannot connect to {} API. Check your internet connection.",
                    self.provider_name()
                ),
                warning: Some(msg.clone()),
            },
            Err(LlmError::Api {
                status,
                ref message,
            }) => KeyValidationResult {
                valid: false,
                message: format!(
                    "{} API returned error (HTTP {}).",
                    self.provider_name(),
                    status
                ),
                warning: Some(message.clone()),
            },
            Err(e) => KeyValidationResult {
                valid: false,
                message: format!("{} validation failed: {}", self.provider_name(), e),
                warning: None,
            },
        }
    }

    /// Simple text generation (no caching, no tools).
    fn generate(&self, prompt: &str) -> Result<String, LlmError>;

    /// One short answer with a hard completion cap and no reasoning budget:
    /// the Optimize advisor (three SWAP lines) and the build explanation
    /// (200 words). Under the 64k/32k Choya ceilings a thinking model routed
    /// through OpenRouter took minutes for either, and Optimize looked hung
    /// (2026-09-05). Providers without a per-call cap fall back to `generate`.
    fn generate_brief(&self, prompt: &str, max_tokens: u32) -> Result<String, LlmError> {
        let _ = max_tokens;
        self.generate(prompt)
    }

    /// Text generation with response caching (same prompt within TTL returns cached result).
    fn generate_cached(&self, prompt: &str) -> Result<String, LlmError>;

    /// Whether requests to this model are scarce enough that a run should
    /// spend as few as it can: a free OpenRouter model (20/min, shared
    /// upstream pools), a Gemini key whose stated quota is five a minute and
    /// twenty a day. Callers cap the lookup rounds on it
    /// ([`profile::ModelProfile::max_turns`]).
    fn thrifty(&self) -> bool {
        false
    }

    /// Multi-turn generation with tool/function calling.
    ///
    /// The LLM can call tools to query game data and calculations.
    /// `execute_tool` is called for each tool invocation with (name, arguments) and returns a result.
    /// `on_progress` is called after each tool-calling round: (turn, max_turns, tool_names_called).
    /// Returns the final text response after all tool calls are resolved.
    fn generate_with_tools_progress(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        execute_tool: &mut dyn FnMut(&str, &Value) -> Value,
        max_turns: usize,
        on_progress: &mut dyn FnMut(usize, usize, &[String]),
    ) -> Result<String, LlmError>;

    /// List available models from the provider's API.
    /// Returns model IDs and display names for UI dropdowns.
    /// Falls back to an empty list on error — callers should use hardcoded defaults.
    fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError>;

    /// Requests remaining today, provider-specific.
    ///
    /// Only Gemini reports a real vendor allowance. OpenAI, Anthropic and
    /// OpenRouter have no hard daily cap, so they report against the addon's
    /// own `DISPLAY_DAILY_BUDGET` — a soft budget, not a quota the provider
    /// enforces. The Settings UI presents both through one field and reads
    /// as fact for all four (GLM F22); relabelling it belongs to whoever owns
    /// that widget, not to this trait.
    fn remaining_quota(&self) -> u32;

    /// Clear the response cache.
    fn clear_cache(&self);
}

/// Convenience: generate with tools but no progress callback.
pub fn generate_with_tools(
    client: &dyn LlmClient,
    prompt: &str,
    tools: &[ToolDefinition],
    execute_tool: &mut dyn FnMut(&str, &Value) -> Value,
    max_turns: usize,
) -> Result<String, LlmError> {
    client.generate_with_tools_progress(prompt, tools, execute_tool, max_turns, &mut |_, _, _| {})
}

/// Case-insensitive check whether an API error body mentions a billing,
/// quota, or credit-balance issue. Used by `validate_key_detailed` overrides
/// to distinguish "key is valid but account has no credits" from "key is
/// invalid". Includes language-neutral Google API status codes so Gemini's
/// non-English responses still match.
pub(crate) fn has_billing_keyword(message: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "billing",
        "quota",
        "exceeded",
        "payment",
        "credit",
        "insufficient",
        // Google API canonical status codes (stable across locales).
        "resource_exhausted",
        "failed_precondition",
    ];
    let lower = message.to_lowercase();
    KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Create an LLM client based on the current config.
/// Dispatches to the correct provider and configures persistence.
pub fn create_client(
    config: &gw2_core::config::AppConfig,
    addon_dir: &std::path::Path,
) -> Result<Box<dyn LlmClient>, LlmError> {
    use gw2_core::config::LlmProvider;

    match config.active_provider {
        LlmProvider::Gemini => {
            let key = config
                .gemini_api_key
                .as_deref()
                .ok_or_else(|| LlmError::Unavailable("No Gemini API key configured".into()))?;
            let model = config.gemini_model_id();
            let usage_path = addon_dir.join("gemini_usage.json");
            let client = gemini::GeminiLlmClient::with_persistence(key, model, usage_path)?;
            Ok(Box::new(client))
        }
        LlmProvider::OpenAI => {
            let key = config
                .openai_api_key
                .as_deref()
                .ok_or_else(|| LlmError::Unavailable("No OpenAI API key configured".into()))?;
            let model = config.openai_model_id();
            let usage_path = addon_dir.join("openai_usage.json");
            let client = openai::OpenAiClient::with_persistence(key, model, usage_path)?;
            Ok(Box::new(client))
        }
        LlmProvider::Anthropic => {
            let key = config
                .anthropic_api_key
                .as_deref()
                .ok_or_else(|| LlmError::Unavailable("No Anthropic API key configured".into()))?;
            let model = config.anthropic_model_id();
            let usage_path = addon_dir.join("anthropic_usage.json");
            let client = anthropic::AnthropicClient::with_persistence(key, model, usage_path)?;
            Ok(Box::new(client))
        }
        LlmProvider::OpenRouter => {
            let key = config
                .openrouter_api_key
                .as_deref()
                .ok_or_else(|| LlmError::Unavailable("No OpenRouter API key configured".into()))?;
            let model = config.openrouter_model_id();
            let usage_path = addon_dir.join("openrouter_usage.json");
            let client = openrouter::OpenRouterClient::with_persistence(key, model, usage_path)?;
            Ok(Box::new(client))
        }
    }
}

/// Parse tool-call argument JSON. Failure is a tool result the model can
/// retry from — never an empty-object execute.
pub(crate) fn parse_tool_arguments(raw: &str) -> Result<Value, Value> {
    serde_json::from_str(raw)
        .map_err(|e| serde_json::json!({ "error": format!("unparseable arguments: {e}") }))
}

#[cfg(test)]
pub(crate) fn run_tool_or_parse_error(
    execute_tool: &mut dyn FnMut(&str, &Value) -> Value,
    name: &str,
    raw_args: &str,
) -> Value {
    match parse_tool_arguments(raw_args) {
        Ok(args) => execute_tool(name, &args),
        Err(err) => err,
    }
}

pub(crate) fn unparseable_tool_input(input: &Value) -> bool {
    input
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|s| s.starts_with("unparseable arguments:"))
}

#[cfg(test)]
mod billing_tests {
    use super::has_billing_keyword;

    #[test]
    fn matches_english_keywords_case_insensitively() {
        assert!(has_billing_keyword("Your billing account is suspended"));
        assert!(has_billing_keyword("QUOTA exceeded for this project"));
        assert!(has_billing_keyword("Credit balance is too low"));
        assert!(has_billing_keyword("payment method required"));
        assert!(has_billing_keyword("insufficient funds"));
    }

    #[test]
    fn matches_google_status_codes() {
        assert!(has_billing_keyword(
            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED"}}"#
        ));
        assert!(has_billing_keyword(
            r#"{"error":{"status":"FAILED_PRECONDITION","message":"..."}}"#
        ));
    }

    #[test]
    fn does_not_match_generic_errors() {
        assert!(!has_billing_keyword("Bad request: missing required field"));
        assert!(!has_billing_keyword("Internal server error"));
        assert!(!has_billing_keyword(""));
    }
}

#[cfg(test)]
mod tool_arg_tests {
    use super::{parse_tool_arguments, run_tool_or_parse_error, unparseable_tool_input, ModelInfo};
    use serde_json::Value;

    #[test]
    fn truncated_json_is_error_not_empty_object() {
        let err = parse_tool_arguments(r#"{"profession":"War"#).expect_err("truncated");
        let msg = err["error"].as_str().expect("error string");
        assert!(msg.starts_with("unparseable arguments:"), "got {msg}");
        assert!(!unparseable_tool_input(&Value::Object(Default::default())));
        assert!(unparseable_tool_input(&err));
    }

    #[test]
    fn valid_args_reach_the_tool() {
        let mut ran = false;
        let result = run_tool_or_parse_error(
            &mut |name, args| {
                ran = true;
                assert_eq!(name, "square");
                assert_eq!(args["n"], 7);
                serde_json::json!({ "ok": true })
            },
            "square",
            r#"{"n":7}"#,
        );
        assert!(ran);
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn unparseable_args_do_not_execute() {
        let mut ran = false;
        let result = run_tool_or_parse_error(
            &mut |_, _| {
                ran = true;
                serde_json::json!({})
            },
            "square",
            r#"{"n":"#,
        );
        assert!(!ran, "truncated args must not execute the tool");
        assert!(unparseable_tool_input(&result));
    }
    /// Every number here is a row from OpenRouter's live catalog on
    /// 2026-09-06 — the cases that were silently wrong before the client read
    /// any of this.
    #[test]
    fn a_model_gets_a_request_it_can_actually_serve() {
        // The best free model in the catalog, and it does not take the effort
        // we used to send every model.
        let glm = ModelInfo {
            id: "z-ai/glm-5.2:free".into(),
            supported_efforts: vec!["xhigh".into(), "high".into()],
            max_completion_tokens: Some(230_400),
            agentic_index: Some(39.7),
            ..Default::default()
        };
        // Neither listed value is at or below `medium`, so it takes the
        // cheapest on offer — `high`, never the `xhigh` that merely happens
        // to be first in the array.
        assert_eq!(glm.effort("medium").as_deref(), Some("high"), "cheapest");
        assert_eq!(glm.completion_budget(65_536), 65_536, "ours is smaller");
        assert!(glm.usable());

        // Publishes a list that does include ours: ours wins.
        let nemotron = ModelInfo {
            supported_efforts: vec!["high".into(), "medium".into()],
            ..Default::default()
        };
        assert_eq!(nemotron.effort("medium").as_deref(), Some("medium"));

        // glm-5.3: mandatory reasoning, and `max` is simply first in the
        // array. Taking the first entry told a model that must think to think
        // as hard as it can, on every message.
        let glm53 = ModelInfo {
            supported_efforts: vec!["max".into(), "high".into(), "low".into()],
            ..Default::default()
        };
        assert_eq!(glm53.effort("medium").as_deref(), Some("low"), "never max");

        // Publishes no list at all: silence is not refusal, keep ours.
        let quiet = ModelInfo::default();
        assert_eq!(quiet.effort("medium").as_deref(), Some("medium"));
        assert_eq!(quiet.completion_budget(65_536), 65_536, "no cap stated");

        // Caps replies far below our ceiling. Asking for more does not
        // truncate the reply, it narrows the routing pool.
        let gemma = ModelInfo {
            max_completion_tokens: Some(32_768),
            ..Default::default()
        };
        assert_eq!(gemma.completion_budget(65_536), 32_768, "clamped to theirs");
    }

    #[test]
    fn only_models_no_request_could_fix_are_unusable() {
        let ok = ModelInfo {
            id: "z-ai/glm-5.2:free".into(),
            ..Default::default()
        };
        assert!(ok.usable());

        let no_tools = ModelInfo {
            supported_parameters: Vec::new(),
            tools: false,
            ..ok.clone()
        };
        assert!(!no_tools.usable(), "we send tools on every request");

        let audio = ModelInfo {
            supported_parameters: Vec::new(),
            text_output: false,
            ..ok.clone()
        };
        assert!(!audio.usable(), "a music model is not a chat model");

        let batch = ModelInfo {
            supported_parameters: Vec::new(),
            id: "google/gemini-3.8-flash:batch".into(),
            ..ok.clone()
        };
        assert!(!batch.usable(), "answers within 24 hours, not now");

        let retiring = ModelInfo {
            supported_parameters: Vec::new(),
            expires: Some("2026-09-10".into()),
            ..ok.clone()
        };
        assert!(!retiring.usable(), "the provider may remove it");

        // A small ceiling or an unusual effort list is ours to send
        // correctly, not a reason to hide the model — pruning these would
        // cost half the free catalogue.
        let small = ModelInfo {
            supported_parameters: Vec::new(),
            max_completion_tokens: Some(8_192),
            supported_efforts: vec!["low".into()],
            ..ok
        };
        assert!(small.usable(), "fixable by building the request properly");
    }

    #[test]
    fn the_picker_orders_by_what_this_addon_asks_a_model_to_do() {
        let agentic = |a: f32| ModelInfo {
            agentic_index: Some(a),
            ..Default::default()
        };
        let coding = |c: f32| ModelInfo {
            coding_index: Some(c),
            ..Default::default()
        };
        // A high coding score never outranks any agentic score: they are
        // different measures, and mixing them would rank by which benchmark a
        // model happened to publish.
        assert!(agentic(1.1).rank() < coding(52.6).rank());
        assert!(agentic(39.7).rank() < agentic(31.0).rank(), "higher first");
        assert!(coding(52.6).rank() < coding(39.3).rank());
        assert!(
            coding(13.8).rank() < ModelInfo::default().rank(),
            "scored first"
        );
    }
}

#[cfg(test)]
mod http_error_chain_tests {
    use super::http_error;
    use std::fmt;

    #[derive(Debug)]
    struct Cause;
    impl fmt::Display for Cause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("CERT_E_REVOCATION_FAILURE")
        }
    }
    impl std::error::Error for Cause {}

    #[derive(Debug)]
    struct Top {
        source: Cause,
    }
    impl fmt::Display for Top {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                "error sending request for url (https://generativelanguage.googleapis.com/)",
            )
        }
    }
    impl std::error::Error for Top {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn llm_http_display_includes_chained_source() {
        let err = Top { source: Cause };
        assert!(
            !err.to_string().contains("CERT_E_REVOCATION_FAILURE"),
            "reqwest Display is the top wrapper only"
        );
        let shown = http_error(err).to_string();
        assert!(
            shown.contains("CERT_E_REVOCATION_FAILURE"),
            "UI/logs must see the TLS cause, got {shown}"
        );
        assert!(
            shown.starts_with("HTTP error: "),
            "Display prefix unchanged, got {shown}"
        );
    }
}
