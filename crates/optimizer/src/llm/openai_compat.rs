//! Shared request core for OpenAI-compatible chat-completions providers.
//!
//! OpenAI and OpenRouter speak the same wire format; only the base URL,
//! identity headers, and OpenRouter-specific extras (reasoning caps,
//! provider routing preferences) differ. One streaming implementation, one
//! retry policy, one rate-tracker handshake — the wrappers add only what
//! makes them distinct.

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::body::read_body_capped;
use super::cancel::{sleep_observing, CANCELLED};
use super::rate::RateTracker;
use super::sse::{read_stream, StreamedMessage};
use super::{LlmError, ToolDefinition};
use serde_json::Value;

/// Client-level ceiling. Streams flow continuously (OpenRouter interleaves
/// `: OPENROUTER PROCESSING` keep-alive comments), so a reasoning model that
/// thinks for minutes no longer trips a short wall clock that would abort
/// valid requests mid-generation. Every call sets a tighter per-request
/// timeout on top; this is only the outer bound.
pub(crate) const REQUEST_TIMEOUT_SECS: u64 = 900;
pub(crate) const CONNECT_TIMEOUT_SECS: u64 = 15;
/// Per-request wall clock for one streamed chat completion. Shared by every
/// provider so the worst-case unload wait does not depend on which provider
/// the user picked (Claude F32: 420 s / 420 s / 900 s / 180 s before).
///
/// This is a reqwest *total* deadline — connect through last body byte — not
/// an idle timeout. Keep-alives hold the server side open; they do not extend
/// the client deadline (GLM F14). 420 s is the budget for one completion.
// 120 s, down from 420: a lookup round on google/gemini-3.8-flash hung for
// over five minutes in-game (2026-09-07, round 8 after seven rounds of
// context) and the player's screen read "thinking" until the UI backstop
// gave up with nothing. A lookup that has not answered in two minutes is
// not answering; the loop then closes on what it has and the plate is
// served. The closing request has its own deadline.
pub(crate) const CHAT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-request wall clock for the metadata endpoints — key validation and the
/// model catalog. These are small, fast calls made from the Settings UI; they
/// used to ride the 900 s client default, so one hung endpoint stalled the
/// worker for fifteen minutes (GLM F14).
pub(crate) const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
/// First retry backoff, doubled per attempt and clamped to
/// [`MAX_RETRY_DELAY`].
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Ceiling for a retry backoff, including a provider-supplied `Retry-After`.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// The turn that closes a tool loop which ran out of rounds.
///
/// Withholding the tool declarations is not enough on its own: the system
/// prompt still orders the model to call `get_spec_traits` and friends, so a
/// model that obeys emits another call and returns no prose — which is exactly
/// the "no answer" a caller then reports. Measured in-game 2026-09-05:
/// gemini-flash-latest used all its rounds and the tool-free closing request
/// still came back empty. This message countermands the standing instruction
/// for that one request.
pub(crate) const CLOSING_TURN: &str = "Stop calling tools. You have every tool \
     result you are going to get, and no tools are available on this request. \
     Serve the finished plate now, using only what you have already gathered: \
     ONLY the JSON build object your instructions describe - specializations \
     as objects with name and traits, weapons, skills, rune, sigils, relic, \
     stat_prefix, explanation. No prose outside the JSON. A build described \
     in sentences is not an answer; the JSON is.";

/// The turn that answers a model which narrated its plan instead of acting
/// on it. In-game 2026-09-07 (minimax-m3:free): "I'll start by checking what
/// specs Necromancer has and pulling the Ritualist trait list" — no tool
/// call, no plate, and a text-only turn is otherwise the final answer.
pub(crate) const CONTINUE_TURN: &str = "Do it now. Call the tools you need, or \
     if you already have what you need, serve the finished plate as the JSON \
     build object. Do not describe what you are about to do.";

/// Whether a text-only turn is the model announcing what it will do rather
/// than an answer: short, first person, forward-looking, no JSON. A real
/// prose answer to a question ("what does Dread do?") is none of those and
/// must never be nudged into a loop.
pub(crate) fn is_narration(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > 600 || trimmed.contains('{') {
        return false;
    }
    let lower = trimmed.to_lowercase();
    [
        "i'll ",
        "i will ",
        "let me ",
        "start by",
        "i'm going to",
        "i am going to",
        "going to check",
        "going to pull",
        "first, i",
        "next, i",
        "i need to check",
        "i need to look",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Fold one assistant message into the conversation and read it as a turn.
/// The message is pushed whole, reasoning details and all: OpenRouter's
/// "Preserving Reasoning" contract wants the blocks back untouched.
pub(crate) fn absorb_turn(conv: &mut Vec<Message>, message: Message) -> super::tool_loop::Turn {
    let calls = message
        .tool_calls
        .iter()
        .flatten()
        .map(|tc| super::tool_loop::ToolCall {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            // A JSON string on this wire; unparseable ones travel as an
            // error object the loop hands straight back to the model.
            args: match super::parse_tool_arguments(&tc.function.arguments) {
                Ok(v) => v,
                Err(e) => e,
            },
        })
        .collect();
    let text = message.content.clone();
    conv.push(message);
    super::tool_loop::Turn { text, calls }
}

pub(crate) fn push_tool_results(
    conv: &mut Vec<Message>,
    results: &[(super::tool_loop::ToolCall, Value)],
) {
    for (call, value) in results {
        conv.push(Message {
            role: "tool".to_string(),
            content: Some(serde_json::to_string(value).unwrap_or_default()),
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            reasoning_details: None,
        });
    }
}

pub(crate) fn push_user(conv: &mut Vec<Message>, text: &str) {
    conv.push(Message {
        role: "user".to_string(),
        content: Some(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
        reasoning_details: None,
    });
}

/// Whether a failure means "this model could not produce a usable function
/// call", rather than a transport, auth or quota problem.
///
/// Google answers a failed function call with HTTP 200 and
/// `native_finish_reason: MALFORMED_FUNCTION_CALL`, and OpenRouter attaches no
/// top-level `error` object because the provider itself succeeded. The stream
/// therefore ends empty and [`sse::read_stream`] reports it as a parse failure
/// carrying that reason.
///
/// This is a last resort, not the explanation. The failures measured in-game on
/// 2026-09-05 were caused by asking for tool calls while sending no tool
/// declarations, and the glm-5.3-flash comparison that looked like a
/// model difference was confounded: the branch keyed on whether a character was
/// loaded, not on the model, so glm took the tools path and gemini did not.
/// Google's own documentation gives a second, real trigger — requiring
/// structured text immediately before a tool call — which the build prompt
/// still does. Dropping the tools only salvages a reply; it does not fix
/// either cause.
pub(crate) fn is_function_call_failure(err: &LlmError) -> bool {
    let LlmError::Parse(message) = err else {
        return false;
    };
    message
        .to_ascii_uppercase()
        .contains("MALFORMED_FUNCTION_CALL")
}

/// Marker written into the error a per-request deadline produces, so a tool
/// loop can tell "this round ran out of clock" from any other transport
/// failure. See [`is_deadline`].
pub(crate) const DEADLINE_MARKER: &str = "(no reply within";

/// Whether `err` is a per-request deadline rather than a transport failure.
pub(crate) fn is_deadline(err: &LlmError) -> bool {
    matches!(err, LlmError::Http(message) if message.contains(DEADLINE_MARKER))
}

/// Wall clock for the tool-*gathering* phase of one logical call, after which
/// the loop stops looking things up and writes the answer from what it has.
///
/// [`CHAT_REQUEST_TIMEOUT`] bounds one request. Nothing bounded the run: a
/// chat message is up to `max_turns` tool rounds plus a closing request, and
/// the chat flow gives a refused plate a second go, so one message was up to
/// 18 sequential completions of 420 s each. In-game 2026-09-06 the player's
/// usage counter recorded 8 requests in one minute for a single message on
/// `minimax/minimax-m3:free` and the run ended on the deadline with nothing
/// served - which is what "no free model gives any result" was.
///
/// A free model answers a tool round in 20-90 s, so 150 s buys two or three
/// rounds and a paid model still finishes its five in under thirty. The point
/// is not to make slow models fast; it is that a run must end in an answer
/// rather than in a stopwatch.
pub(crate) const TOOL_PHASE_BUDGET: Duration = Duration::from_secs(150);

/// Completion cap for the closing request — the one that writes the plate.
///
/// It went out with the lookup rounds' [`MAX_COMPLETION_TOKENS`] and
/// [`REASONING_EFFORT`], which invites a reasoning model to deliberate over
/// eight rounds of tool results before writing a few thousand tokens of
/// JSON. Measured 2026-09-07 on `minimax/minimax-m3:free`: tool phase done in
/// 65 s, closing request still silent at 77 s when the player gave up, and
/// the run before it ran the whole 420 s deadline out and served nothing.
/// Writing a plate is not a 32k job; it is not a thinking job either.
pub(crate) const CLOSING_MAX_TOKENS: u32 = 8_192;

/// Ceiling for the closing request. It was 150 s and it ended the player's
/// wait with "no reply within 150s" while the model was still writing
/// (in-game 2026-09-08). The wait is now open-ended from the player's side:
/// the thinking bubble shows what is arriving and a stall line when nothing
/// is, and Stop is theirs to press (specs/006 US5). This only bounds a
/// runaway nobody is watching. (`reqwest::blocking` has no per-read idle
/// timeout; the stall line is the idle signal.)
pub(crate) const CLOSING_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// Completion ceiling per chat completion, hidden thinking included, so a
/// reasoning model cannot spend the budget deliberating and have nothing left
/// to answer with.
///
/// This was 16_384, then 65_536 on the argument that designing a build from
/// live tool data is not a 16k job. That argument stands and 16k is not
/// coming back — but 65_536 cannot be *delivered* inside
/// [`CHAT_REQUEST_TIMEOUT`], which is a total deadline, not an idle one. At
/// 100 tokens/s a full budget needs 655 s against a 420 s deadline; at 50
/// tok/s, 1311 s. A model that took us at our word could not finish, and two
/// unrelated models timed out in exactly that way.
///
/// 32_768 fits at 100 tok/s with room to spare and keeps twice the ceiling
/// the 16k argument was made against. It is a ceiling, not a target: what
/// stops it being reached is choosing a reasoning effort the model actually
/// lists (see [`crate::llm::ModelInfo::effort`]) instead of the dearest one
/// on offer. It also widens routing, since OpenRouter routes only to
/// providers that can serve the `max_tokens` asked for.
pub(crate) const MAX_COMPLETION_TOKENS: u32 = 32_768;
/// How hard a thinking model may think (OpenRouter `reasoning.effort`;
/// ignored by providers without thinking support).
///
/// "medium" is half the completion budget on Anthropic
/// (`budget_tokens = max_tokens * effort_ratio`, 0.5 at medium), which is
/// what the old raw `reasoning.max_tokens: 32_768` against
/// [`MAX_COMPLETION_TOKENS`] spelled out by hand — so nothing changes where
/// that already worked. It is also the only knob Gemini 3 honours: those
/// models take Google's `thinkingLevel`, and a raw token budget is remapped
/// to a level Google picks, which bought minutes of thinking and no control
/// (OpenRouter reasoning-tokens docs, "Google Gemini 3 Models with Thinking
/// Levels").
pub(crate) const REASONING_EFFORT: &str = "medium";

pub(crate) fn http_client() -> Result<reqwest::blocking::Client, LlmError> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .map_err(|e| LlmError::Http(e.to_string()))
}

/// Holds the rate slot taken by `check_and_reserve` and gives it back on drop
/// unless the request actually succeeded.
///
/// Every early return used to repeat the undo by hand, and the providers that
/// were copied from this one each missed a path — Anthropic and Gemini leak a
/// slot on mid-stream failure (GLM F16). A guard cannot miss a path.
pub(crate) struct RateReserve<'a> {
    rate: Option<&'a Mutex<RateTracker>>,
}

impl<'a> RateReserve<'a> {
    /// Wrap a slot that `check_and_reserve` has already taken.
    pub(crate) fn held(rate: &'a Mutex<RateTracker>) -> Self {
        Self { rate: Some(rate) }
    }

    /// The request succeeded: the slot stays spent.
    pub(crate) fn keep(&mut self) {
        self.rate = None;
    }
}

impl Drop for RateReserve<'_> {
    fn drop(&mut self) {
        if let Some(rate) = self.rate.take() {
            rate.lock()
                .unwrap_or_else(|e| e.into_inner())
                .undo_reserve();
        }
    }
}

/// `Retry-After` in delta-seconds, clamped to [`MAX_RETRY_DELAY`].
///
/// The HTTP-date form is not honored: none of these APIs send it, and a
/// mis-parsed date would be worse than the default backoff.
pub(crate) fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs = headers
        .get("Retry-After")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_DELAY))
}

/// Next backoff after one more failed attempt: doubled, never past the
/// ceiling. Shared so no provider grows an unbounded wait of its own.
pub(crate) fn doubled_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_RETRY_DELAY)
}

/// Whether an HTTP status is worth another attempt.
///
/// 429 is in the set: a rate limit with `Retry-After` is a normal traffic
/// shape on OpenRouter, and returning immediately turned one transient burst
/// into a user-visible failure (Grok F5, GLM F21). 408/504 are gateway
/// "upstream didn't respond in time"; 529 is Anthropic overloaded normalized
/// through the router.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 408 | 500 | 502 | 503 | 504 | 529)
}

/// Normalize a transport failure so a rate limit reads the same to the UI
/// whether it arrived as an HTTP status or inside a 200 body.
pub(crate) fn as_transport_error(status: u16, message: String) -> LlmError {
    if status == 429 {
        LlmError::RateLimited(message)
    } else {
        LlmError::Api { status, message }
    }
}

// OpenAI wire types

#[derive(Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    /// Always streamed: keep-alive comments hold the connection open while
    /// reasoning models think, and the first bytes land in seconds instead
    /// of after the whole generation.
    pub(crate) stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<ProviderPrefs>,
    /// OpenAI/OpenRouter structured output: `{"type":"json_schema", ...}`.
    /// The API's own way to get the plate as JSON, instead of asking nicely
    /// in the prompt and repairing prose afterwards. Sent only where the
    /// catalog lists `response_format` or `structured_outputs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_format: Option<Value>,
    /// `none` / `auto` / `required`. `required` on the first lookup round
    /// stops a model narrating its plan instead of calling anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<String>,
    /// `{"include_usage": true}` where the provider needs asking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<Value>,
}

/// The plate, as the JSON Schema the API enforces on the closing request.
/// Mirrors the shape in `prompts.rs`; `strict` is off so a model that adds a
/// field is not refused, and every field the parser can do without is
/// optional.
pub(crate) fn plate_response_format() -> Value {
    let name_list = |desc: &str| serde_json::json!({ "type": "array", "items": { "type": "string" }, "description": desc });
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "build_plate",
            "strict": false,
            "schema": {
                "type": "object",
                "properties": {
                    "specializations": {
                        "type": "array",
                        "minItems": 3,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "elite": { "type": "boolean" },
                                "traits": { "type": "array", "items": { "type": "string" }, "minItems": 3, "maxItems": 3 }
                            },
                            "required": ["name", "traits"]
                        }
                    },
                    "weapons": {
                        "type": "object",
                        "properties": {
                            "set1": { "type": "object", "properties": { "main": { "type": ["string", "null"] }, "off": { "type": ["string", "null"] } } },
                            "set2": { "type": "object", "properties": { "main": { "type": ["string", "null"] }, "off": { "type": ["string", "null"] } } }
                        }
                    },
                    "skills": {
                        "type": "object",
                        "properties": {
                            "heal": { "type": "string" },
                            "utilities": name_list("three utility skills"),
                            "elite": { "type": "string" }
                        }
                    },
                    "rune": { "type": "string" },
                    "sigils": { "type": "object" },
                    "relic": { "type": "string" },
                    "pets": { "type": "object" },
                    "legends": name_list("revenant legends"),
                    "stat_prefix": { "type": "string" },
                    "gear_slots": { "type": "object" },
                    "changes_made": name_list("what changed"),
                    "explanation": { "type": "string" }
                },
                "required": ["specializations", "weapons", "skills", "stat_prefix", "explanation"]
            }
        }
    })
}

/// OpenRouter `reasoning` parameter — caps hidden thinking so the completion
/// budget survives for the actual answer. Providers without thinking support
/// ignore unknown parameters (per OpenRouter's parameter docs).
#[derive(Serialize, Debug, Clone)]
pub(crate) struct ReasoningConfig {
    pub(crate) effort: String,
}

/// OpenRouter `provider` routing preferences.
#[derive(Serialize, Debug, Clone)]
pub(crate) struct ProviderPrefs {
    /// Only route to endpoints that natively support every parameter in the
    /// request — never to one that fakes tools through a prompt template.
    pub(crate) require_parameters: bool,
    /// OpenRouter `provider.sort`: `"throughput"` sends a free model to the
    /// host that streams it fastest instead of the default price ordering,
    /// which for a free model is a tie broken by nothing useful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sort: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Message {
    pub(crate) role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls: Option<Vec<ToolCallResponse>>,
    /// For role="tool" messages: the ID of the tool call being responded to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    /// OpenRouter reasoning blocks, carried back verbatim on the assistant
    /// turn they came from. A tool loop is one continuous thought interrupted
    /// by lookups, so the provider needs its own blocks back to resume it:
    /// OpenRouter's "Preserving Reasoning" contract requires the sequence to
    /// match what the model produced, neither rearranged nor modified, and
    /// Gemini 3 rejects a continued loop whose blocks were dropped. Opaque by
    /// design: we never look inside one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_details: Option<Vec<Value>>,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct OpenAiTool {
    #[serde(rename = "type")]
    pub(crate) tool_type: String,
    pub(crate) function: OpenAiFunction,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct OpenAiFunction {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ToolCallResponse {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) call_type: String,
    pub(crate) function: FunctionCallData,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct FunctionCallData {
    pub(crate) name: String,
    /// OpenAI sends arguments as a JSON *string*, not an object.
    pub(crate) arguments: String,
}

/// Everything the shared core needs to reach one provider.
pub(crate) struct ProviderCore<'a> {
    pub(crate) http: &'a reqwest::blocking::Client,
    pub(crate) rate: &'a Mutex<RateTracker>,
    pub(crate) api_key: &'a str,
    pub(crate) base_url: &'a str,
    pub(crate) model: &'a str,
    /// Static identity headers (OpenRouter's HTTP-Referer / X-Title).
    pub(crate) extra_headers: &'a [(&'static str, String)],
    /// Provider name for error strings ("OpenRouter", "OpenAI").
    pub(crate) label: &'a str,
    pub(crate) max_tokens: u32,
    /// OpenRouter `reasoning.effort`. `None` omits the field.
    /// Owned, because it is chosen per model from what the catalog says the
    /// model accepts — not a constant we picked once. See
    /// [`crate::llm::ModelInfo::effort`].
    pub(crate) reasoning_effort: Option<String>,
    /// Whether this base URL understands the OpenRouter-only top-level
    /// `provider` block. `api.openai.com` rejects unknown top-level body
    /// arguments, so sending it there breaks the OpenAI provider outright
    /// (Claude F8). Capability, not a URL sniff: a self-hosted
    /// OpenAI-compatible gateway sets this from its own knowledge.
    pub(crate) supports_provider_prefs: bool,
    /// OpenRouter `provider.require_parameters` when tools are present.
    pub(crate) require_tool_endpoints: bool,
    /// OpenRouter `provider.sort`; `None` keeps the default ordering.
    pub(crate) provider_sort: Option<&'static str>,
    /// `response_format` for this request; `None` omits it.
    pub(crate) response_format: Option<Value>,
    /// `tool_choice` for this request; `None` omits it.
    pub(crate) tool_choice: Option<&'static str>,
    /// Per-request wall-clock cap. This is a reqwest *total* deadline, not an
    /// idle timeout: provider keep-alives hold the connection open but do not
    /// extend it. See [`CHAT_REQUEST_TIMEOUT`].
    pub(crate) request_timeout: std::time::Duration,
    pub(crate) max_retries: u32,
    /// Ask for the closing usage chunk (`stream_options.include_usage`).
    /// OpenAI streams no usage without it. OpenRouter always sends usage and
    /// documents the parameter as deprecated, so it stays off there rather
    /// than become one more parameter `require_parameters` routes on.
    pub(crate) stream_usage: bool,
    /// Polled between attempts, between backoff slices, and between stream
    /// lines so an unload does not have to wait out `request_timeout`.
    /// `&|| false` where cancellation is not meaningful.
    pub(crate) is_cancelled: &'a dyn Fn() -> bool,
}

/// One streamed chat completion with the shared retry policy.
///
/// Rate tracker handshake (reserve on entry, undo on every failure path)
/// lives here; response persistence stays with the caller on success.
pub(crate) fn send_chat(
    core: ProviderCore<'_>,
    messages: &[Message],
    tools: Option<&[ToolDefinition]>,
) -> Result<Message, LlmError> {
    // Retries and backoff sleeps included: that is time the run waited on the model.
    let _wait = super::usage::WaitTimer::start();
    core.rate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .check_and_reserve()?;

    // Every early return past this point owes the tracker an undo, so the
    // reserve is released exactly once by the guard's drop.
    let mut reserve = RateReserve::held(core.rate);

    let openai_tools = tools.map(|defs| {
        defs.iter()
            .map(|td| OpenAiTool {
                tool_type: "function".to_string(),
                function: OpenAiFunction {
                    name: td.name.clone(),
                    description: td.description.clone(),
                    parameters: td.parameters.clone(),
                },
            })
            .collect::<Vec<_>>()
    });

    let request = ChatRequest {
        model: core.model.to_string(),
        messages: messages.to_vec(),
        tools: openai_tools,
        max_tokens: Some(core.max_tokens),
        stream: Some(true),
        reasoning: core
            .reasoning_effort
            .clone()
            .map(|effort| ReasoningConfig { effort }),
        // OpenRouter-only body field. `api.openai.com` rejects unknown
        // top-level arguments, so posting it to every OpenAI-compatible base
        // URL made the OpenAI provider fail outright (Claude F8).
        provider: core.supports_provider_prefs.then_some(ProviderPrefs {
            require_parameters: core.require_tool_endpoints,
            sort: core.provider_sort.map(str::to_string),
        }),
        response_format: core.response_format.clone(),
        tool_choice: core.tool_choice.map(str::to_string),
        stream_options: core
            .stream_usage
            .then(|| serde_json::json!({ "include_usage": true })),
    };

    let url = format!("{}/chat/completions", core.base_url);
    let mut last_error: Option<LlmError> = None;
    let mut next_delay = INITIAL_RETRY_DELAY;

    for attempt in 0..core.max_retries {
        if (core.is_cancelled)() {
            return Err(LlmError::Unavailable(CANCELLED.to_string()));
        }
        if attempt > 0 {
            let _waiting = super::usage::Waiting::new(next_delay, super::usage::WaitReason::Retry);
            if !sleep_observing(next_delay, core.is_cancelled) {
                return Err(LlmError::Unavailable(CANCELLED.to_string()));
            }
            next_delay = doubled_backoff(next_delay);
        }

        let mut req = core
            .http
            .post(&url)
            .timeout(core.request_timeout)
            .header("Authorization", format!("Bearer {}", core.api_key))
            .header("Content-Type", "application/json");
        for (name, value) in core.extra_headers {
            req = req.header(*name, value);
        }

        super::HTTP_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let resp = match req.json(&request).send() {
            Ok(r) => r,
            // A timeout is not a transport hiccup and must not be retried.
            // `request_timeout` is a reqwest TOTAL deadline — connect through
            // last byte — so a request that hit it did not fail to start, it
            // ran out the whole budget. Handing it an identical budget cannot
            // end differently; it only doubles the silence. With two attempts
            // and a 420s deadline that was 845s per logical call, and the
            // chat loop runs up to eight of them.
            Err(e) if e.is_timeout() => {
                return Err(LlmError::Http(format!(
                    "{e} {DEADLINE_MARKER} {}s)",
                    core.request_timeout.as_secs()
                )));
            }
            Err(e) => {
                if attempt == core.max_retries - 1 {
                    return Err(LlmError::Http(e.to_string()));
                }
                last_error = Some(LlmError::Http(e.to_string()));
                continue;
            }
        };

        let status = resp.status().as_u16();
        match status {
            200 => {
                match read_stream(resp, core.is_cancelled) {
                    Ok(StreamedMessage::Message(message)) => {
                        reserve.keep();
                        return Ok(message);
                    }
                    // Nothing usable in a 200 — the held reserve is released
                    // on drop so this dead trip doesn't count against quota.
                    Ok(StreamedMessage::Empty(finish)) => {
                        return Err(LlmError::Parse(format!(
                            "Empty response from {label} (finish_reason: {finish})",
                            label = core.label
                        )));
                    }
                    // Measured OpenRouter behaviour (2026-08-27): an upstream
                    // rate limit arrives as HTTP **200** carrying an unframed
                    // `{"error":{…,"code":429}}` body. Retrying only on the
                    // HTTP status would never fire on the provider that needs
                    // it most, so the in-band status gets the same policy.
                    Err(LlmError::Api { status, message })
                        if is_retryable_status(status) && attempt + 1 < core.max_retries =>
                    {
                        last_error = Some(as_transport_error(status, message));
                        continue;
                    }
                    Err(LlmError::Api { status, message }) => {
                        return Err(as_transport_error(status, message))
                    }
                    Err(e) => return Err(e),
                }
            }
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
        message: format!("{} server error after retries", core.label),
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn user(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_details: None,
        }
    }

    /// Build one raw HTTP/1.1 response. `Connection: close` so each attempt
    /// gets its own connection and the script stays in lockstep with the
    /// retry loop.
    fn http_response(status_line: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut out = format!("HTTP/1.1 {status_line}\r\n");
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        out
    }

    /// Read one request fully — headers plus body, `Content-Length` or
    /// chunked — before answering. A half-read request plus a closed socket
    /// is an RST that discards the response, which surfaces as a transport
    /// error instead of the status the script meant to send.
    /// Returns the request body, so a test can assert on what was actually
    /// posted rather than on a struct it built itself.
    fn drain_request(stream: &TcpStream) -> std::io::Result<String> {
        let mut reader = std::io::BufReader::new(stream.try_clone()?);
        let mut content_length = 0usize;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(String::new());
            }
            if line.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if let Some(value) = lower.strip_prefix("transfer-encoding:") {
                chunked = value.contains("chunked");
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                let mut size_line = String::new();
                if reader.read_line(&mut size_line)? == 0 {
                    break;
                }
                let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                // Chunk payload plus its trailing CRLF.
                let mut chunk = vec![0u8; size + 2];
                reader.read_exact(&mut chunk)?;
                if size == 0 {
                    break;
                }
                chunk.truncate(size);
                body.extend_from_slice(&chunk);
            }
        } else {
            body = vec![0u8; content_length];
            reader.read_exact(&mut body)?;
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Serve one scripted response on one connection.
    ///
    /// Nothing is read after the response: the request was already consumed in
    /// full, so half-closing is a clean FIN, and waiting for the client to
    /// close would park this thread while the next attempt is already
    /// connecting.
    fn serve_one(
        mut stream: TcpStream,
        response: &str,
        served: &AtomicUsize,
        bodies: &Mutex<Vec<String>>,
    ) -> std::io::Result<()> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let body = drain_request(&stream)?;
        // Recorded BEFORE the response is written, not after. The request is
        // already fully read here, so the count is honest — and the client
        // cannot see a byte of the reply until the write below, which means
        // it cannot return from `send_chat` and assert on `served()` before
        // this has happened.
        //
        // Recording after the flush was a race with no ordering between the
        // two threads: the client could receive, parse and return while this
        // thread was still between `flush` and `fetch_add`, and the test read
        // a count of zero. Narrow enough to pass twelve runs on a developer
        // machine and still redden CI, which is where it was caught —
        // `permanent_failures_are_not_retried` on the 1.12.0 merge commit,
        // whose tree is byte-identical to one that had passed three times.
        bodies.lock().unwrap_or_else(|e| e.into_inner()).push(body);
        served.fetch_add(1, Ordering::SeqCst);
        stream.write_all(response.as_bytes())?;
        stream.flush()?;
        // Wait for the client to close before dropping the socket. Closing a
        // Windows socket that still has unread inbound data sends an RST, and
        // the client reports that as a send failure rather than the status we
        // just wrote (WSAECONNRESET, reproduced at ~20% without this). Safe to
        // park here: every connection has its own thread.
        let mut sink = Vec::new();
        let _ = (&stream).take(64 * 1024).read_to_end(&mut sink);
        let _ = stream.shutdown(Shutdown::Both);
        Ok(())
    }

    /// A scripted loopback HTTP server.
    ///
    /// Not a live API call and not a network dependency: it binds
    /// `127.0.0.1:0`, replays a fixed list of raw responses, and dies with
    /// the test process. `leaf-1.1.2` is removing real-network calls from the
    /// gw2api tests; this plants none.
    struct ScriptedServer {
        base_url: String,
        served: Arc<AtomicUsize>,
        /// Request bodies in the order they arrived.
        bodies: Arc<Mutex<Vec<String>>>,
        /// Held so the port stays bound for the whole test even after the
        /// script is exhausted. A refused connect would come back as a
        /// transport error and read like a retry that never happened.
        _listener: Arc<TcpListener>,
    }

    impl ScriptedServer {
        fn start(responses: Vec<String>) -> Self {
            let listener = Arc::new(TcpListener::bind("127.0.0.1:0").expect("bind loopback"));
            let port = listener.local_addr().expect("local addr").port();
            let served = Arc::new(AtomicUsize::new(0));
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let counter = Arc::clone(&served);
            let recorder = Arc::clone(&bodies);
            let accepting = Arc::clone(&listener);
            // Detached: if the client stops early, the accept simply parks and
            // is reaped at process exit, so a failing assertion reports the
            // real problem instead of hanging the suite on a join.
            std::thread::spawn(move || {
                for response in responses {
                    let Ok((stream, _)) = accepting.accept() else {
                        return;
                    };
                    // One thread per connection: a client socket that lingers
                    // must never delay the accept for the next attempt.
                    let counter = Arc::clone(&counter);
                    let recorder = Arc::clone(&recorder);
                    std::thread::spawn(move || {
                        let _ = serve_one(stream, &response, &counter, &recorder);
                    });
                }
            });
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                served,
                bodies,
                _listener: listener,
            }
        }

        /// The JSON body of the Nth request that reached the server.
        fn posted_body(&self, index: usize) -> serde_json::Value {
            let bodies = self.bodies.lock().expect("bodies");
            let raw = bodies.get(index).expect("request was posted");
            serde_json::from_str(raw).expect("posted body is JSON")
        }

        fn served(&self) -> usize {
            self.served.load(Ordering::SeqCst)
        }
    }

    /// The exact body OpenRouter returns when an upstream provider rate-limits
    /// a stream it had already accepted. Measured 2026-08-27: HTTP **200**,
    /// `content-type: text/event-stream`, and a bare JSON error object with no
    /// `data:` framing anywhere in it. `metadata.raw` is a String carrying
    /// embedded JSON, not a nested object.
    const OPENROUTER_INBAND_429: &str = concat!(
        "{\"error\":{\"message\":\"Provider returned error\",\"code\":429,",
        "\"metadata\":{\"raw\":\"{\\\"detail\\\":\\\"temporarily rate-limited upstream\\\"}\",",
        "\"provider_name\":\"Io Net\",\"is_byok\":false,",
        "\"limit_source\":\"upstream_provider_shared_pool\"}}}"
    );

    fn inband_429() -> String {
        http_response(
            "200 OK",
            &[("Content-Type", "text/event-stream")],
            OPENROUTER_INBAND_429,
        )
    }

    /// The gateway-level shape: OpenRouter refuses before any stream starts,
    /// with a proper status and `application/json`. `Retry-After: 0` is what
    /// keeps this test instant — it pins the backoff at zero, and the
    /// doubling stays at zero from there.
    fn http_429() -> String {
        http_response(
            "429 Too Many Requests",
            &[("Retry-After", "0"), ("Content-Type", "application/json")],
            "{\"error\":{\"message\":\"rate limit exceeded\",\"code\":429}}",
        )
    }

    /// A healthy stream, including the keep-alive comments OpenRouter
    /// interleaves on every real response.
    fn good_stream() -> String {
        http_response(
            "200 OK",
            &[("Content-Type", "text/event-stream")],
            concat!(
                ": OPENROUTER PROCESSING\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
                "data: [DONE]\n",
            ),
        )
    }

    fn test_core<'a>(
        http: &'a reqwest::blocking::Client,
        rate: &'a Mutex<RateTracker>,
        base_url: &'a str,
        is_cancelled: &'a dyn Fn() -> bool,
    ) -> ProviderCore<'a> {
        ProviderCore {
            tool_choice: None,
            response_format: None,
            provider_sort: None,
            http,
            rate,
            api_key: "test-key",
            base_url,
            model: "test/model",
            extra_headers: &[],
            label: "OpenRouter",
            max_tokens: MAX_COMPLETION_TOKENS,
            reasoning_effort: None,
            supports_provider_prefs: true,
            require_tool_endpoints: false,
            // Short: a hung mock must fail the test, not stall it for 420 s.
            request_timeout: Duration::from_secs(10),
            max_retries: 3,
            stream_usage: false,
            is_cancelled,
        }
    }

    /// Grok F5 / GLM F21 — 429 used to return immediately. Both real 429
    /// shapes must be retried, for the *same* provider: the gateway-level
    /// HTTP status, and the in-band error OpenRouter carries inside a 200
    /// `text/event-stream` after it has already accepted the stream.
    #[test]
    fn http_429_retries() {
        let http = http_client().expect("client");
        let no_cancel = || false;

        let server = ScriptedServer::start(vec![http_429(), inband_429(), good_stream()]);
        let rate = Mutex::new(RateTracker::new(60));
        let core = test_core(&http, &rate, &server.base_url, &no_cancel);
        let message = send_chat(core, &[user("hi")], None).expect("third attempt must succeed");

        assert_eq!(message.content.as_deref(), Some("done"));
        assert_eq!(
            server.served(),
            3,
            "both the HTTP 429 and the in-band 200/429 must have been retried"
        );
        assert_eq!(
            rate.lock().expect("rate").requests_this_minute(),
            1,
            "three round trips are still one logical request"
        );

        // Exhausting the attempts reports a rate limit, not a raw 502, and
        // hands the reserved slot back.
        let server = ScriptedServer::start(vec![http_429(), inband_429(), inband_429()]);
        let rate = Mutex::new(RateTracker::new(60));
        let core = test_core(&http, &rate, &server.base_url, &no_cancel);
        let error = send_chat(core, &[user("hi")], None).expect_err("all attempts rate limited");

        assert!(
            matches!(error, LlmError::RateLimited(_)),
            "an in-band 429 must surface as RateLimited, got: {error}"
        );
        assert_eq!(server.served(), 3);
        assert_eq!(
            rate.lock().expect("rate").requests_this_minute(),
            0,
            "a failed request must not spend a rate slot"
        );
    }

    /// OpenRouter fails in two shapes and only one of them is worth another
    /// attempt. `403` (model access restricted), `404` (no endpoint matching
    /// the guardrails) and `400` (unknown model id) are permanent: retrying
    /// them spends the whole backoff window and reads to the user as a hang.
    /// Measured against `:free` models, where outright failure is common.
    #[test]
    fn permanent_failures_are_not_retried() {
        let http = http_client().expect("client");
        let no_cancel = || false;

        for (status_line, expected) in [
            ("403 Forbidden", 403u16),
            ("404 Not Found", 404),
            ("400 Bad Request", 400),
        ] {
            // Script three responses; only the first may be consumed.
            let refusal = http_response(
                status_line,
                &[("Content-Type", "application/json")],
                "{\"error\":{\"message\":\"No endpoints available\"}}",
            );
            let server = ScriptedServer::start(vec![refusal.clone(), refusal, good_stream()]);
            let rate = Mutex::new(RateTracker::new(60));
            let core = test_core(&http, &rate, &server.base_url, &no_cancel);

            let error = send_chat(core, &[user("hi")], None).expect_err("permanent failure");
            match error {
                LlmError::Api { status, .. } => assert_eq!(status, expected),
                other => panic!("expected Api {expected}, got: {other}"),
            }
            assert_eq!(
                server.served(),
                1,
                "{status_line} must fail on the first attempt, not burn the backoff"
            );
            assert_eq!(rate.lock().expect("rate").requests_this_minute(), 0);
        }
    }

    /// Claude F8, proved on the wire rather than on a struct: the flag has to
    /// change the bytes that actually leave the process. `OpenAiClient` sets
    /// `supports_provider_prefs: false`, `OpenRouterClient` sets `true`.
    #[test]
    fn provider_prefs_flag_controls_the_posted_body() {
        let http = http_client().expect("client");
        let no_cancel = || false;

        // OpenAI-shaped: no OpenRouter extensions may appear.
        let server = ScriptedServer::start(vec![good_stream()]);
        let rate = Mutex::new(RateTracker::new(60));
        let mut core = test_core(&http, &rate, &server.base_url, &no_cancel);
        core.supports_provider_prefs = false;
        core.reasoning_effort = None;
        core.stream_usage = true;
        send_chat(core, &[user("hi")], None).expect("ok");

        let body = server.posted_body(0);
        assert_eq!(
            body["stream_options"]["include_usage"],
            serde_json::json!(true),
            "OpenAI streams no usage unless asked"
        );
        assert!(
            body.get("provider").is_none(),
            "OpenAI must not receive the OpenRouter `provider` block: {body}"
        );
        assert!(
            body.get("reasoning").is_none(),
            "OpenAI must not receive the OpenRouter `reasoning` block: {body}"
        );
        assert_eq!(body["stream"], serde_json::json!(true));

        // OpenRouter-shaped: both extensions ride along.
        let server = ScriptedServer::start(vec![good_stream()]);
        let rate = Mutex::new(RateTracker::new(60));
        let mut core = test_core(&http, &rate, &server.base_url, &no_cancel);
        core.supports_provider_prefs = true;
        core.require_tool_endpoints = true;
        core.reasoning_effort = Some(REASONING_EFFORT.to_string());
        send_chat(core, &[user("hi")], None).expect("ok");

        let body = server.posted_body(0);
        assert_eq!(
            body["provider"]["require_parameters"],
            serde_json::json!(true)
        );
        assert_eq!(
            body["reasoning"]["effort"],
            serde_json::json!(REASONING_EFFORT)
        );
        assert!(
            body.get("stream_options").is_none(),
            "OpenRouter always sends usage; the parameter is deprecated there: {body}"
        );
    }

    /// Claude F8 — the OpenRouter-only `provider` block was posted to
    /// `api.openai.com` too, where an unknown top-level argument is a 400.
    #[test]
    fn openai_request_omits_the_openrouter_provider_block() {
        let base = ChatRequest {
            tool_choice: None,
            stream_options: None,
            response_format: None,
            model: "gpt-4o".into(),
            messages: vec![user("hi")],
            tools: None,
            max_tokens: Some(MAX_COMPLETION_TOKENS),
            stream: Some(true),
            reasoning: None,
            provider: None,
        };
        let body = serde_json::to_value(&base).expect("serializes");
        assert!(
            body.get("provider").is_none(),
            "OpenAI body must not carry `provider`: {body}"
        );
        assert!(body.get("reasoning").is_none());

        let routed = ChatRequest {
            tool_choice: None,
            stream_options: None,
            response_format: None,
            provider: Some(ProviderPrefs {
                sort: None,
                require_parameters: true,
            }),
            ..base
        };
        let body = serde_json::to_value(&routed).expect("serializes");
        assert_eq!(
            body["provider"]["require_parameters"],
            serde_json::json!(true),
            "OpenRouter still gets its routing preferences"
        );
    }

    #[test]
    fn retry_after_is_read_and_clamped() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_delay(&headers), None);

        headers.insert("Retry-After", "7".parse().expect("header"));
        assert_eq!(retry_after_delay(&headers), Some(Duration::from_secs(7)));

        headers.insert("Retry-After", "99999".parse().expect("header"));
        assert_eq!(retry_after_delay(&headers), Some(MAX_RETRY_DELAY));

        // HTTP-date form is not parsed; the default backoff beats a guess.
        headers.insert(
            "Retry-After",
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().expect("header"),
        );
        assert_eq!(retry_after_delay(&headers), None);
    }

    #[test]
    fn cancellation_aborts_before_the_request_is_sent() {
        let http = http_client().expect("client");
        let rate = Mutex::new(RateTracker::new(60));
        let cancelled = || true;
        // Port 1: reaching the socket at all would be the bug.
        let core = test_core(&http, &rate, "http://127.0.0.1:1", &cancelled);

        let error = send_chat(core, &[user("hi")], None).expect_err("cancel must abort");
        assert!(matches!(error, LlmError::Unavailable(ref m) if m == CANCELLED));
        assert_eq!(
            rate.lock().expect("rate").requests_this_minute(),
            0,
            "a cancelled request must give its rate slot back"
        );
    }

    /// Live repro of the in-game Choya hang: the real chat prompt builder at
    /// kitchen-brief scale, streamed with the production request shape.
    /// Ignored by default; run with OPENROUTER_API_KEY set:
    ///   cargo test -p gw2-optimizer live_hang_repro -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_hang_repro_big_prompt_streaming() {
        use std::time::Instant;

        let key = std::env::var("OPENROUTER_API_KEY").expect("set OPENROUTER_API_KEY");
        let rate = Mutex::new(RateTracker::new(60));
        let http = http_client().expect("client");

        let mut kitchen = String::from(
            "Mode: WvW \u{b7} Scale: Roam\nRole: Roamer \u{b7} Damage, Bruiser, Troll\nProfession: Druid\n\n",
        );
        for i in 0..200 {
            kitchen.push_str(&format!(
                "- Pantry item {i}: stat prefix notes, rune and sigil interactions,\n relic timing, trait synergy hints, rotation considerations.\n"
            ));
        }
        kitchen.push_str("\nRecent chat:\n- player: hello\n");
        let message = "I want to make a perfect druid roaming build which is a cross between a roamer and a troll build, prioritizing pure condition damage, lots of disable CC and great survivability/sustain.";
        let prompt = crate::prompts::chat_refinement_prompt_with_tools(
            "Druid", "WvW", message, &kitchen, "Choya",
        );
        println!("prompt bytes: {}", prompt.len());

        let messages = vec![user(&prompt)];
        let no_cancel = || false;
        let core = ProviderCore {
            tool_choice: None,
            response_format: None,
            provider_sort: None,
            http: &http,
            rate: &rate,
            api_key: &key,
            base_url: "https://openrouter.ai/api/v1",
            model: "z-ai/glm-5.3-flash",
            extra_headers: &[],
            label: "OpenRouter",
            max_tokens: MAX_COMPLETION_TOKENS,
            reasoning_effort: Some(REASONING_EFFORT.to_string()),
            supports_provider_prefs: true,
            require_tool_endpoints: false,
            request_timeout: CHAT_REQUEST_TIMEOUT,
            max_retries: 2,
            stream_usage: false,
            is_cancelled: &no_cancel,
        };

        let t0 = Instant::now();
        match send_chat(core, &messages, None) {
            Ok(msg) => println!(
                "OK in {:.1}s — content {} chars",
                t0.elapsed().as_secs_f64(),
                msg.content.as_deref().map(str::len).unwrap_or(0)
            ),
            Err(e) => {
                println!("ERR after {:.1}s: {e:?}", t0.elapsed().as_secs_f64());
            }
        }
    }
}

#[cfg(test)]
mod narration_tests {
    use super::is_narration;

    #[test]
    fn a_plan_is_narration_and_an_answer_is_not() {
        assert!(is_narration(
            "I'll start by checking what specs Necromancer has and pulling the Ritualist trait list - that's the centerpiece."
        ));
        assert!(is_narration("Let me look up the Reaper traits first."));
        // A real answer to a question: not chased.
        assert!(!is_narration(
            "Dread grants fury when you inflict fear and increases damage against feared foes."
        ));
        // A plate is never narration, however it opens.
        assert!(!is_narration("I'll serve it: {\"specializations\": []}"));
        assert!(!is_narration(""));
    }
}
