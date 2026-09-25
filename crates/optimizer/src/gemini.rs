//! Gemini API client for LLM-powered build reasoning.
//! Uses Google AI Studio's REST API (generativelanguage.googleapis.com).
//! API key is sent via x-goog-api-key header (not URL query) for security.
//! Includes response caching to minimize quota usage.
//!
//! Transport policy is shared with the `LlmError`-based providers rather than
//! re-derived here: same HTTP client (connect timeout included), same body
//! ceilings, same backoff clamp, same reserve-and-release discipline. Gemini
//! is the addon's default pipeline, so it had the most to lose from being the
//! one client with its own rules.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::llm::body::{body_capped, hit_body_cap, json_capped, read_body_capped, MAX_LLM_BODY};
use crate::llm::cancel::{is_cancelled, sleep_observing, CANCELLED};
use crate::llm::openai_compat::{
    doubled_backoff, http_client, retry_after_delay, CHAT_REQUEST_TIMEOUT, METADATA_TIMEOUT,
};

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

#[derive(Debug, thiserror::Error)]
pub enum GeminiError {
    /// Transport failure: no usable HTTP response, or the socket died while
    /// the response body was still arriving.
    ///
    /// Carries a `String` rather than `reqwest::Error` because a mid-stream
    /// failure surfaces as `std::io::Error`, which cannot be turned into a
    /// `reqwest::Error`. Those were being reported as [`GeminiError::Parse`]
    /// — "Parse error: stream read failed" told the user their model returned
    /// garbage when the real event was a dropped connection (GLM F15).
    /// Matches `LlmError::Http(String)`, which is what this maps to at the
    /// trait boundary.
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("Invalid API key")]
    InvalidKey,
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("LLM unavailable: {0}")]
    Unavailable(String),
}

fn http_error(err: impl std::error::Error) -> GeminiError {
    GeminiError::Http(gw2_core::format_error_chain(&err))
}

pub struct GeminiClient {
    api_key: String,
    model: String,
    http: reqwest::blocking::Client,
    cache: crate::llm::response_cache::ResponseCache,
    rate: Mutex<RateTracker>,
    usage_path: Option<PathBuf>,
}

impl crate::llm::tool_loop::TurnDriver for GeminiClient {
    type Conv = Vec<Content>;
    type Err = GeminiError;

    fn open(&self, prompt: &str) -> Vec<Content> {
        vec![Content {
            role: Some("user".into()),
            parts: vec![Part::text(prompt)],
        }]
    }
    fn trim(&self, conv: &mut Vec<Content>) {
        trim_contents(conv, crate::llm::trim::SAFE_PROMPT_BUDGET_TOKENS);
    }
    fn turn(
        &self,
        conv: &mut Vec<Content>,
        tools: Option<&[crate::llm::ToolDefinition]>,
        mode: crate::llm::tool_loop::TurnMode,
    ) -> Result<crate::llm::tool_loop::Turn, GeminiError> {
        let tools = tools.map(|defs| {
            vec![Tool {
                function_declarations: defs
                    .iter()
                    .map(|d| FunctionDeclaration {
                        name: d.name.clone(),
                        description: d.description.clone(),
                        parameters: d.parameters.clone(),
                    })
                    .collect(),
            }]
        });
        let content = self.send_request(&GenerateRequest {
            contents: conv.clone(),
            tools,
            generation_config: {
                let closing = matches!(mode, crate::llm::tool_loop::TurnMode::Closing);
                let thinks = model_thinks(&self.model);
                (closing || thinks).then(|| GenerationConfig {
                    max_output_tokens: closing
                        .then_some(crate::llm::openai_compat::CLOSING_MAX_TOKENS),
                    thinking_config: thinks.then_some(ThinkingConfig {
                        include_thoughts: true,
                    }),
                })
            },
        })?;
        let text = content.parts.iter().find_map(|p| p.text.clone());
        let calls = content
            .parts
            .iter()
            .filter_map(|p| p.function_call.as_ref())
            .map(|fc| crate::llm::tool_loop::ToolCall {
                // Gemini matches responses by name, not by id.
                id: fc.name.clone(),
                name: fc.name.clone(),
                args: fc.args.clone(),
            })
            .collect();
        conv.push(content);
        Ok(crate::llm::tool_loop::Turn { text, calls })
    }
    fn push_tool_results(
        &self,
        conv: &mut Vec<Content>,
        results: &[(crate::llm::tool_loop::ToolCall, serde_json::Value)],
    ) {
        conv.push(Content {
            role: Some("user".into()),
            parts: results
                .iter()
                .map(|(call, value)| Part::function_response(&call.name, value.clone()))
                .collect(),
        });
    }
    fn push_user(&self, conv: &mut Vec<Content>, text: &str) {
        conv.push(Content {
            role: Some("user".into()),
            parts: vec![Part::text(text)],
        });
    }
    fn cancelled(&self) -> GeminiError {
        GeminiError::Unavailable(CANCELLED.to_string())
    }
    fn no_answer(&self, detail: String) -> GeminiError {
        GeminiError::Parse(format!("Gemini: {detail}"))
    }
    fn is_deadline(&self, err: &GeminiError) -> bool {
        err.to_string()
            .contains(crate::llm::openai_compat::DEADLINE_MARKER)
    }
    fn is_function_call_failure(&self, err: &GeminiError) -> bool {
        err.to_string()
            .to_ascii_uppercase()
            .contains("MALFORMED_FUNCTION_CALL")
    }
}

impl GeminiClient {
    fn stream_url(&self) -> String {
        format!(
            "{}/{}:streamGenerateContent?alt=sse",
            GEMINI_API_BASE, self.model
        )
    }

    /// One streamed `generateContent` call, wall clock and auth attached.
    ///
    /// Built here rather than inline so the request shape is assertable
    /// without a socket: `reqwest::blocking::Request::timeout` is readable
    /// after `build()`, and `reqwest::blocking::Client` is not — its `Debug`
    /// prints just `Client`.
    ///
    /// Auth is the `x-goog-api-key` header. Verified against the live API on
    /// 2026-08-27: the header returns 200, `Authorization: Bearer` returns
    /// 401. The key never goes in the URL, where it would land in logs.
    fn stream_request(&self, request: &GenerateRequest) -> reqwest::blocking::RequestBuilder {
        self.http
            .post(self.stream_url())
            .timeout(CHAT_REQUEST_TIMEOUT)
            .header("x-goog-api-key", &self.api_key)
            .json(request)
    }

    /// The `GET /v1beta/models` call shared by key validation and the model
    /// catalog. Small and Settings-tab-blocking, so it gets the short
    /// [`METADATA_TIMEOUT`], not a completion budget.
    fn models_request(&self) -> reqwest::blocking::RequestBuilder {
        self.http
            .get(GEMINI_MODELS_URL)
            .timeout(METADATA_TIMEOUT)
            .header("x-goog-api-key", &self.api_key)
    }
}

/// Length of the per-minute window.
const MINUTE: Duration = Duration::from_secs(60);
/// Requests per minute assumed for a model until Google says otherwise.
/// The older free-tier Flash models allow 10; `gemini-3.8-flash` allows 5,
/// which its first 429 teaches the tracker (`learn_rpm`).
const DEFAULT_RPM: u32 = 10;
/// Longest a per-minute quota is waited out rather than reported.
const MAX_QUOTA_WAIT: Duration = Duration::from_secs(65);

struct RateTracker {
    /// Which model's per-minute quota applies. Empty in unit tests.
    model: String,
    /// Per-minute limits Google has stated, by model, kept across runs.
    rpm_limits: HashMap<String, u32>,
    requests_this_minute: u32,
    minute_start: Instant,
    requests_today: u32,
    /// Day number (Unix epoch / 86400) for daily counter reset.
    current_day: u64,
}

/// Persisted usage data saved to disk between sessions.
#[derive(Serialize, Deserialize)]
struct PersistedUsage {
    day: u64,
    requests_today: u32,
    /// Wall-clock epoch second the current minute window opened.
    ///
    /// `#[serde(default)]` on both minute fields: a usage file written by an
    /// older build has neither, and losing the daily counter to a strict
    /// parse would be a worse bug than starting the minute window fresh.
    #[serde(default)]
    minute_start_epoch: u64,
    #[serde(default)]
    requests_this_minute: u32,
    #[serde(default)]
    rpm_limits: HashMap<String, u32>,
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn current_epoch_day() -> u64 {
    current_epoch_secs() / 86400
}

impl RateTracker {
    fn new() -> Self {
        Self {
            model: String::new(),
            rpm_limits: HashMap::new(),
            requests_this_minute: 0,
            minute_start: Instant::now(),
            requests_today: 0,
            current_day: current_epoch_day(),
        }
    }

    fn for_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    fn rpm_limit(&self) -> u32 {
        self.stated_rpm().unwrap_or(DEFAULT_RPM)
    }

    /// The per-minute quota Google has actually stated for this model, if
    /// any. `None` is not "generous": it is "not yet told", and a fresh
    /// free-tier key spends its 20-a-day finding out.
    fn stated_rpm(&self) -> Option<u32> {
        self.rpm_limits.get(&self.model).copied()
    }

    /// Google stated this model's per-minute quota in a 429; keep it.
    fn learn_rpm(&mut self, limit: u32) {
        self.rpm_limits.insert(self.model.clone(), limit.max(1));
    }

    /// How long until the window admits another request, if it is full.
    /// Pacing to this beats spending the request on a 429: a run that needs
    /// six requests on a five-a-minute tier then waits instead of failing
    /// (gemini-3.8-flash free tier, in-game 2026-09-07).
    fn wait_for_window(&self) -> Option<Duration> {
        if self.requests_this_minute < self.rpm_limit() {
            return None;
        }
        MINUTE.checked_sub(self.minute_start.elapsed())
    }

    fn from_persisted(persisted: PersistedUsage) -> Self {
        let today = current_epoch_day();
        let requests_today = if persisted.day == today {
            persisted.requests_today
        } else {
            0 // new day, reset counter
        };

        let now = Instant::now();
        // Age of the persisted minute window. `checked_sub` is the clock-went-
        // backwards guard: a saturating subtraction would read as "zero
        // seconds old" and pin a user at the limit until wall clock caught up.
        let age = current_epoch_secs().checked_sub(persisted.minute_start_epoch);
        let (requests_this_minute, minute_start) = match age {
            Some(age) if age < MINUTE.as_secs() && persisted.minute_start_epoch > 0 => (
                persisted.requests_this_minute,
                // Re-anchor the monotonic clock so the window expires when it
                // would have, not 60 s from now.
                now.checked_sub(Duration::from_secs(age)).unwrap_or(now),
            ),
            _ => (0, now),
        };

        Self {
            model: String::new(),
            rpm_limits: persisted.rpm_limits,
            requests_this_minute,
            minute_start,
            requests_today,
            current_day: today,
        }
    }

    /// Check rate limits and pre-reserve a slot (increment counters).
    ///
    /// The caller must wrap the reserved slot in a [`RateReserve`] so it is
    /// returned on every failure path. Do not call [`Self::undo_reserve`]
    /// directly: that is what missed the mid-stream failure (GLM F16).
    fn check_and_reserve(&mut self) -> Result<(), GeminiError> {
        // Reset daily counter if the day changed
        let today = current_epoch_day();
        if today != self.current_day {
            self.requests_today = 0;
            self.current_day = today;
        }

        let now = Instant::now();
        if now.duration_since(self.minute_start) >= MINUTE {
            self.requests_this_minute = 0;
            self.minute_start = now;
        }

        if self.requests_this_minute >= self.rpm_limit() {
            return Err(GeminiError::RateLimited(format!(
                "{} requests per minute on this model",
                self.rpm_limit()
            )));
        }
        if self.requests_today >= 240 {
            return Err(GeminiError::Unavailable(
                "Daily quota nearly exhausted (240/250)".into(),
            ));
        }

        // Reserve slot atomically with the check
        self.requests_this_minute += 1;
        self.requests_today += 1;
        Ok(())
    }

    /// Release a reserved slot if the request failed before reaching the API.
    fn undo_reserve(&mut self) {
        self.requests_this_minute = self.requests_this_minute.saturating_sub(1);
        self.requests_today = self.requests_today.saturating_sub(1);
    }

    /// Requests charged to the current minute window. Test-only: the
    /// transport tests use it to prove the reserve/release handshake
    /// balances instead of asserting on a socket.
    #[cfg(test)]
    fn requests_this_minute(&self) -> u32 {
        self.requests_this_minute
    }

    /// Slots left in the current 10-RPM window. Test-only: two clients
    /// built from the same usage file must agree on this number.
    #[cfg(test)]
    fn remaining_rpm(&self) -> u32 {
        10u32.saturating_sub(self.requests_this_minute)
    }

    fn remaining_today(&self) -> u32 {
        250u32.saturating_sub(self.requests_today)
    }

    fn to_persisted(&self) -> PersistedUsage {
        // Convert the monotonic anchor back to wall clock only here, at the
        // serialization boundary.
        let age = self.minute_start.elapsed().as_secs();
        PersistedUsage {
            day: self.current_day,
            requests_today: self.requests_today,
            minute_start_epoch: current_epoch_secs().saturating_sub(age),
            requests_this_minute: self.requests_this_minute,
            rpm_limits: self.rpm_limits.clone(),
        }
    }
}

/// A quota refusal, as Google states it in a 429 body:
/// `QuotaFailure.violations[].quotaId` naming `PerMinute` or `PerDay` with a
/// `quotaValue`, and `RetryInfo.retryDelay` ("39s"). Measured 2026-09-07 on
/// the free tier of gemini-3.8-flash: 5 per minute, **20 per day**.
struct QuotaRefusal {
    limit: u32,
    per_day: bool,
    retry_after: Duration,
}

impl QuotaRefusal {
    fn summary(&self) -> String {
        if self.per_day {
            format!(
                "{} requests per day on this model's free tier; it resets at midnight Pacific time",
                self.limit
            )
        } else {
            format!(
                "{} requests per minute on this model's tier; Google asked for {}s",
                self.limit,
                self.retry_after.as_secs()
            )
        }
    }
}

fn quota_refusal(body: &str) -> Option<QuotaRefusal> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let details = v["error"]["details"].as_array()?;
    let mut limit = None;
    let mut per_day = false;
    let mut retry_after = None;
    for d in details {
        if let Some(violations) = d["violations"].as_array() {
            for violation in violations {
                let id = violation["quotaId"].as_str().unwrap_or_default();
                if id.contains("PerMinute") || id.contains("PerDay") {
                    per_day = id.contains("PerDay");
                    limit = violation["quotaValue"]
                        .as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .or_else(|| violation["quotaValue"].as_u64().map(|n| n as u32));
                }
            }
        }
        if let Some(delay) = d["retryDelay"].as_str() {
            retry_after = delay
                .trim_end_matches('s')
                .parse::<f64>()
                .ok()
                .map(Duration::from_secs_f64);
        }
    }
    Some(QuotaRefusal {
        limit: limit?,
        per_day,
        retry_after: retry_after.unwrap_or(MINUTE),
    })
}

/// Holds the rate slot taken by [`RateTracker::check_and_reserve`] and gives
/// it back on drop unless the request actually succeeded.
///
/// `send_request` used to repeat the undo by hand on each early return, and
/// the one path it missed was the one that matters most: `read_gemini_stream`
/// failing after a 200. A free-tier key is 10 requests per minute and 250 per
/// day, so every leaked slot is a slot the user cannot spend (GLM F16). A
/// guard cannot miss a return path.
///
/// This is a local twin of `llm::openai_compat::RateReserve`, not a reuse of
/// it: that guard is typed on `llm::rate::RateTracker`, and Gemini
/// deliberately keeps its own tracker because it enforces a hard *provider*
/// daily cap and reports `GeminiError`. Sharing the type would make
/// [`GeminiClient::remaining_quota`] report the other tracker's 10_000-request
/// display budget — the fabricated number GLM F22 flagged.
///
// ponytail: two guards, one shape. Merging them needs a shared
// `trait RateSlot { fn undo(&mut self); }` in `llm::rate`, which is another
// leaf's file; a generic guard over one trait method is the upgrade path if a
// fourth tracker ever appears.
struct RateReserve<'a> {
    rate: Option<&'a Mutex<RateTracker>>,
}

impl<'a> RateReserve<'a> {
    /// Wrap a slot that `check_and_reserve` has already taken.
    fn held(rate: &'a Mutex<RateTracker>) -> Self {
        Self { rate: Some(rate) }
    }

    /// The request succeeded: the slot stays spent.
    fn keep(&mut self) {
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

#[derive(Serialize)]
struct GenerateRequest {
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    /// Set on the closing turn only: the plate is ~600 tokens, and a model
    /// left uncapped has reasoned for minutes on that request.
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    /// `includeThoughts` returns thought summaries in the stream for the
    /// thinking bubble. Sent only to the thinking families (2.5 and 3);
    /// older models reject the field.
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<ThinkingConfig>,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "includeThoughts")]
    include_thoughts: bool,
}

/// Whether a Gemini model id names a thinking family.
fn model_thinks(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("gemini-2.5") || m.contains("gemini-3")
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

/// A Part can carry text, a function call (from model), or a function response (from us).
/// Uses flat optional fields for robust serde with the Gemini API.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    function_call: Option<FunctionCall>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    function_response: Option<FunctionResponse>,
    /// Gemini 3 signs each function call with an opaque thought signature
    /// and refuses the next turn if the call comes back without it
    /// ("Function call is missing a thought_signature in functionCall
    /// parts", 400, in-game 2026-09-07 on gemini-3.8-flash). Carried back
    /// verbatim; never inspected.
    #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
    /// Marks a thinking part; kept so a round-tripped turn stays as it came.
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
}

impl Part {
    fn text(s: impl Into<String>) -> Self {
        Part {
            text: Some(s.into()),
            function_call: None,
            function_response: None,
            thought_signature: None,
            thought: None,
        }
    }

    fn function_response(name: impl Into<String>, response: serde_json::Value) -> Self {
        Part {
            text: None,
            function_call: None,
            function_response: Some(FunctionResponse {
                name: name.into(),
                response,
            }),
            thought_signature: None,
            thought: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

// Tool Declarations

#[derive(Serialize, Debug, Clone)]
pub struct Tool {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Serialize, Debug, Clone)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// SSE Streaming

/// One Gemini SSE `data:` payload (streamGenerateContent?alt=sse chunks are
/// full GenerateContentResponse objects with partial candidates).
#[derive(Deserialize)]
struct StreamGenerateResponse {
    candidates: Option<Vec<Candidate>>,
    /// Mid-stream failure: `{"error":{"code":…,"message":…,"status":…}}`.
    #[serde(default)]
    error: Option<StreamErrorPayload>,
    /// Running token counts; the last chunk's are the request's totals.
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    /// Billed at the output rate ("Output price (including thinking tokens)").
    thoughts_token_count: Option<u64>,
    total_token_count: Option<u64>,
}

impl From<UsageMetadata> for crate::llm::usage::ResponseUsage {
    fn from(u: UsageMetadata) -> Self {
        let completion = match (u.candidates_token_count, u.thoughts_token_count) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        Self {
            prompt: u.prompt_token_count,
            completion,
            total: u.total_token_count,
            cost_usd: None,
        }
    }
}

#[derive(Deserialize)]
struct StreamErrorPayload {
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    message: Option<String>,
}

/// Reads a Gemini `streamGenerateContent` SSE body into one merged `Content`.
///
/// Each chunk carries partial `candidates[0].content.parts`; text parts are
/// concatenated in arrival order and function-call parts are passed through
/// whole (Gemini emits a `functionCall` part complete in a single chunk).
/// An `error` payload is the mid-stream failure channel.
///
/// The read is bounded by [`MAX_LLM_BODY`]. Timeouts bound *time*, not
/// *bytes*: a fast endpoint can exhaust the game process's memory well inside
/// the request deadline, and a single newline-free stream grew one `String`
/// without limit here (GLM F6, Claude F17). `llm::gemini` already refuses an
/// oversized body at the trait boundary, but that check runs after this
/// function has finished allocating; this is where the peak actually is.
fn read_gemini_stream<R: std::io::Read>(reader: R) -> Result<Content, GeminiError> {
    use std::io::BufRead;

    let mut role: Option<String> = None;
    let mut text = String::new();
    let mut parts: Vec<Part> = Vec::new();
    // Recorded rather than returned in place: `hit_body_cap` needs `capped`
    // back, and the line iterator borrows it for the whole loop. A truncated
    // multi-byte codepoint at the ceiling arrives here as `InvalidData`, so
    // the cap has to be ruled out before this is blamed on the wire.
    let mut read_error: Option<std::io::Error> = None;
    let mut usage: Option<crate::llm::usage::ResponseUsage> = None;

    let mut capped = body_capped(reader);
    for line in std::io::BufReader::new(&mut capped).lines() {
        if is_cancelled() {
            return Err(GeminiError::Unavailable(CANCELLED.to_string()));
        }
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                read_error = Some(e);
                break;
            }
        };
        let line = line.trim();
        if !line.starts_with("data: ") {
            continue;
        }
        let chunk: StreamGenerateResponse = match serde_json::from_str(&line["data: ".len()..]) {
            Ok(chunk) => chunk,
            Err(_) => continue,
        };
        if let Some(err) = chunk.error {
            return Err(GeminiError::Api {
                status: err.code.unwrap_or(502),
                message: err
                    .message
                    .unwrap_or_else(|| "Gemini stream error".to_string()),
            });
        }
        if let Some(u) = chunk.usage_metadata {
            usage = Some(u.into());
        }
        let Some(content) = chunk
            .candidates
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.content)
        else {
            continue;
        };
        if role.is_none() {
            role = content.role.clone();
        }
        for part in content.parts {
            if let Some(t) = part.text {
                if part.thought == Some(true) {
                    crate::llm::live::reasoning(&t);
                } else {
                    crate::llm::live::content(&t);
                    text.push_str(&t);
                }
            }
            if let Some(call) = part.function_call {
                crate::llm::live::tool_call(&call.name);
                parts.push(Part {
                    text: None,
                    function_call: Some(call),
                    function_response: None,
                    thought_signature: part.thought_signature,
                    thought: None,
                });
            }
        }
    }

    // Reaching the ceiling means Gemini was still sending: whatever assembled
    // so far is a fragment, not an answer. Checked before `read_error` because
    // a cap landing mid-codepoint *is* the read error.
    if hit_body_cap(&capped) {
        return Err(body_cap_exceeded());
    }
    if let Some(e) = read_error {
        return Err(GeminiError::Http(format!(
            "Gemini stream read failed: {}",
            gw2_core::format_error_chain(&e)
        )));
    }
    // Billed whether or not the body held anything usable.
    crate::llm::usage::record(usage);

    if !text.is_empty() {
        parts.insert(0, Part::text(text));
    }
    if parts.is_empty() {
        return Err(GeminiError::Parse("No response content from Gemini".into()));
    }
    Ok(Content {
        role: role.or(Some("model".to_string())),
        parts,
    })
}

/// The error a Gemini body read returns once it reaches [`MAX_LLM_BODY`].
///
/// Wording matches `llm::body::body_cap_exceeded` and the adapter-side check
/// in `llm::gemini` so the user sees one message whichever ceiling trips.
fn body_cap_exceeded() -> GeminiError {
    GeminiError::Api {
        status: 502,
        message: format!(
            "Gemini response exceeded the {} MiB body cap and was dropped",
            MAX_LLM_BODY / (1024 * 1024)
        ),
    }
}

/// What `send_request` does with one HTTP status from Gemini.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusAction {
    /// 200 — read the streamed body.
    Read,
    /// The key is rejected; no retry will help.
    InvalidKey,
    /// 403/429 — read the body so billing/quota is not reported as a bad key.
    Denied,
    /// Transient — try again after a backoff.
    Retry,
    /// Anything else: report the status and body, once.
    Fail,
}

/// Gemini's status policy, kept socket-free so the decision is testable
/// without a live key or a mock server.
///
/// 429 is terminal here on purpose, and this is where Gemini diverges from
/// `llm::openai_compat::is_retryable_status`, which retries it. Two reasons,
/// both measured against the live API on 2026-08-27:
///
/// * Gemini reports quota exhaustion as a real HTTP 429 with a JSON error
///   body. OpenRouter reports an upstream mid-stream failure as HTTP 200 with
///   an unframed error inside an `text/event-stream` body, which is why that
///   client has to be forgiving about a 429 it may have inferred.
/// * A free-tier key is exhausted by roughly seven small requests, and a
///   retry minutes later is still 429. Retrying would burn the user's
///   remaining per-minute slots to re-learn the same answer.
fn classify_status(status: u16) -> StatusAction {
    match status {
        200 => StatusAction::Read,
        401 => StatusAction::InvalidKey,
        403 | 429 => StatusAction::Denied,
        // Server failures plus the gateway "upstream didn't answer" pair.
        408 | 500 | 502 | 503 | 504 => StatusAction::Retry,
        _ => StatusAction::Fail,
    }
}

fn denied_from_body(status: u16, body: &str) -> GeminiError {
    if crate::llm::has_billing_keyword(body) {
        return GeminiError::Api {
            status,
            message: body.to_string(),
        };
    }
    match status {
        403 => GeminiError::InvalidKey,
        429 => GeminiError::RateLimited(body.to_string()),
        _ => GeminiError::Api {
            status,
            message: body.to_string(),
        },
    }
}

/// Turn a 200 response body into `Content`, keeping the reserved rate slot
/// only if the body actually produced an answer.
///
/// The guard is taken **by value** so the compiler enforces the ordering:
/// the only route to `keep()` runs through a successful read, and every other
/// exit drops the guard, which releases the slot. That is the path the
/// hand-written `undo_reserve()` calls missed — a stream that died after the
/// 200 charged the user a slot for nothing (GLM F16). Split out of
/// `send_request` so the handshake is provable from a fixture instead of a
/// socket.
fn consume_success_body<R: std::io::Read>(
    mut reserve: RateReserve<'_>,
    body: R,
) -> Result<Content, GeminiError> {
    let content = read_gemini_stream(body)?;
    reserve.keep();
    Ok(content)
}

/// The shared blocking HTTP client, including the `connect_timeout` the
/// Gemini client was missing.
///
/// `GeminiClient` used to build its own client with a 180 s total timeout and
/// no connect timeout, so a black-holed TCP handshake fell back to the OS
/// default — minutes on Windows, with nothing in the addon bounding it. The
/// 180 s was also the *shortest* completion budget of any provider, so the
/// addon's default pipeline was the one most likely to abort a long answer
/// mid-generation (GLM F15, Claude F32). One factory, one policy: 15 s to
/// connect, [`CHAT_REQUEST_TIMEOUT`] per completion, [`METADATA_TIMEOUT`] per
/// Settings-tab call.
fn gemini_http_client() -> Result<reqwest::blocking::Client, GeminiError> {
    http_client().map_err(|e| GeminiError::Http(e.to_string()))
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<Content>,
}

/// Drop oldest tool-call turn(s) when the conversation exceeds the token
/// budget. A Gemini turn is one model Content (with function_call parts)
/// followed by one user Content (with function_response parts); pairs must
/// be dropped atomically. The initial user prompt and the most recent turn
/// are always preserved.
fn trim_contents(contents: &mut Vec<Content>, budget_tokens: usize) {
    use crate::llm::trim::estimate_tokens;

    fn part_tokens(p: &Part) -> usize {
        let text = p.text.as_deref().map(estimate_tokens).unwrap_or(0);
        let fc = p
            .function_call
            .as_ref()
            .map(|fc| estimate_tokens(&fc.args.to_string()))
            .unwrap_or(0);
        let fr = p
            .function_response
            .as_ref()
            .map(|fr| estimate_tokens(&fr.response.to_string()))
            .unwrap_or(0);
        text + fc + fr
    }
    fn content_tokens(c: &Content) -> usize {
        c.parts.iter().map(part_tokens).sum()
    }

    let mut total: usize = contents.iter().map(content_tokens).sum();
    if total <= budget_tokens {
        return;
    }

    loop {
        let turn_starts: Vec<usize> = contents
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, c)| c.parts.iter().any(|p| p.function_call.is_some()))
            .map(|(i, _)| i)
            .collect();

        if turn_starts.len() < 2 {
            return;
        }

        contents.drain(turn_starts[0]..turn_starts[1]);
        total = contents.iter().map(content_tokens).sum();
        if total <= budget_tokens {
            return;
        }
    }
}

impl GeminiClient {
    pub fn new(api_key: &str, model: &str) -> Result<Self, GeminiError> {
        Ok(Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            http: gemini_http_client()?,
            cache: crate::llm::response_cache::ResponseCache::new(1800, 64),
            rate: Mutex::new(RateTracker::new().for_model(model)),
            usage_path: None,
        })
    }

    /// Create a client with persistent rate tracking.
    /// Loads existing usage from `usage_path` and saves after each request.
    pub fn with_persistence(
        api_key: &str,
        model: &str,
        usage_path: PathBuf,
    ) -> Result<Self, GeminiError> {
        let rate = if usage_path.exists() {
            match std::fs::read_to_string(&usage_path)
                .ok()
                .and_then(|s| serde_json::from_str::<PersistedUsage>(&s).ok())
            {
                Some(persisted) => RateTracker::from_persisted(persisted),
                None => RateTracker::new(),
            }
        } else {
            RateTracker::new()
        };

        Ok(Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            http: gemini_http_client()?,
            cache: crate::llm::response_cache::ResponseCache::new(1800, 64),
            rate: Mutex::new(rate.for_model(model)),
            usage_path: Some(usage_path),
        })
    }

    /// Validate the API key using the models list endpoint (no quota consumed).
    pub fn validate_key(&self) -> Result<(), GeminiError> {
        let resp = self.models_request().send().map_err(http_error)?;

        let status = resp.status().as_u16();
        match classify_status(status) {
            StatusAction::Read => Ok(()),
            StatusAction::InvalidKey => Err(GeminiError::InvalidKey),
            StatusAction::Denied => Err(denied_from_body(status, &read_body_capped(resp))),
            // Settings-tab validation is a single shot: a 5xx here is
            // reported, not retried behind a spinner.
            StatusAction::Retry | StatusAction::Fail => Err(GeminiError::Api {
                status,
                message: read_body_capped(resp),
            }),
        }
    }

    /// List models that support content generation.
    /// Calls `GET /v1beta/models` and filters by `supportedGenerationMethods`.
    pub fn list_models(&self) -> Result<Vec<(String, String)>, GeminiError> {
        let resp = self.models_request().send().map_err(http_error)?;

        let status = resp.status().as_u16();
        match classify_status(status) {
            StatusAction::Read => {}
            StatusAction::InvalidKey => return Err(GeminiError::InvalidKey),
            StatusAction::Denied => return Err(denied_from_body(status, &read_body_capped(resp))),
            StatusAction::Retry | StatusAction::Fail => {
                return Err(GeminiError::Api {
                    status,
                    message: read_body_capped(resp),
                })
            }
        }

        #[derive(Deserialize)]
        struct ModelsResponse {
            models: Option<Vec<ModelEntry>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ModelEntry {
            name: Option<String>,
            display_name: Option<String>,
            supported_generation_methods: Option<Vec<String>>,
        }

        // `resp.json()` reads to EOF; the catalog is small but the socket is
        // not the addon's to trust. The kind is preserved on the way across:
        // a catalog that died on the wire is not a malformed catalog.
        let body: ModelsResponse = json_capped(resp).map_err(|e| match e {
            crate::llm::LlmError::Http(msg) => GeminiError::Http(msg),
            other => GeminiError::Parse(other.to_string()),
        })?;

        let models = body.models.unwrap_or_default();
        let mut result: Vec<(String, String)> = models
            .into_iter()
            .filter(|m| {
                m.supported_generation_methods
                    .as_ref()
                    .is_some_and(|methods| methods.iter().any(|m| m == "generateContent"))
            })
            .filter_map(|m| {
                let name = m.name?;
                // Strip "models/" prefix: "models/gemini-2.5-flash" → "gemini-2.5-flash"
                let id = name.strip_prefix("models/").unwrap_or(&name).to_string();
                let display = m.display_name.unwrap_or_else(|| id.clone());
                Some((id, display))
            })
            .collect();

        // Sort: models with "flash" first (cheapest), then alphabetically
        result.sort_by(|a, b| {
            let a_flash = a.0.contains("flash");
            let b_flash = b.0.contains("flash");
            match (a_flash, b_flash) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.0.cmp(&b.0),
            }
        });

        Ok(result)
    }

    /// Send a prompt to Gemini, using cache if available.
    /// Returns cached response if the same prompt was sent within 30 minutes.
    pub fn generate_cached(&self, prompt: &str) -> Result<String, GeminiError> {
        // Check cache (recover from poison)
        if let Some(text) = self.cache.get(prompt) {
            return Ok(text);
        }

        // Not cached — generate
        let text = self.generate(prompt)?;
        self.cache.insert(prompt, text.clone());

        Ok(text)
    }

    /// Whether a run should spend as few requests as it can. Yes until
    /// Google has stated a per-minute quota at or above the default: an
    /// assumed 10 is not evidence that eight lookups are affordable, and a
    /// paid key loses nothing but a few lookups the reference already
    /// covers.
    pub fn thrifty(&self) -> bool {
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stated_rpm()
            .is_none_or(|stated| stated < DEFAULT_RPM)
    }

    /// Send a prompt to Gemini (no caching). Checks rate limits first.
    pub fn generate(&self, prompt: &str) -> Result<String, GeminiError> {
        let request = GenerateRequest {
            generation_config: None,
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![Part::text(prompt)],
            }],
            tools: None,
        };

        let content = self.send_request(&request)?;
        content
            .parts
            .into_iter()
            .find_map(|p| p.text)
            .ok_or_else(|| GeminiError::Parse("No response text from Gemini".into()))
    }

    /// Multi-turn generation with function calling (tool use).
    /// Gemini can call tools to query game data and calculations.
    /// Returns the final text response after all tool calls are resolved.
    pub fn generate_with_tools(
        &self,
        prompt: &str,
        tools: Vec<Tool>,
        mut execute_tool: impl FnMut(&str, &serde_json::Value) -> serde_json::Value,
        max_turns: usize,
    ) -> Result<String, GeminiError> {
        self.generate_with_tools_progress(
            prompt,
            tools,
            &mut execute_tool,
            max_turns,
            &mut |_, _, _| {},
        )
    }

    /// Like `generate_with_tools` but calls `on_progress(turn, max_turns, tool_names)` after
    /// each tool-calling round, so the UI can show what Gemini is doing.
    pub fn generate_with_tools_progress(
        &self,
        prompt: &str,
        tools: Vec<Tool>,
        execute_tool: &mut dyn FnMut(&str, &serde_json::Value) -> serde_json::Value,
        max_turns: usize,
        on_progress: &mut dyn FnMut(usize, usize, &[String]),
    ) -> Result<String, GeminiError> {
        // The shared loop validates arguments against the declarations, so
        // it takes the provider-neutral form; the driver turns them back.
        let defs: Vec<crate::llm::ToolDefinition> = tools
            .iter()
            .flat_map(|t| t.function_declarations.iter())
            .map(|d| crate::llm::ToolDefinition {
                name: d.name.clone(),
                description: d.description.clone(),
                parameters: d.parameters.clone(),
            })
            .collect();
        crate::llm::tool_loop::run(self, prompt, &defs, execute_tool, max_turns, on_progress)
    }

    /// Low-level: send a request and return the response Content.
    ///
    /// Retries only what [`classify_status`] calls transient. The reserved
    /// rate slot is held by a [`RateReserve`] for the whole call, so every
    /// exit — including a stream that dies after the 200 — gives it back
    /// unless an answer actually arrived.
    fn send_request(&self, request: &GenerateRequest) -> Result<Content, GeminiError> {
        const MAX_RETRIES: u32 = 3;
        // Pacing, retries and the stream: all of it is the run waiting on the model.
        let _wait = crate::llm::usage::WaitTimer::start();

        // Pace to the model's stated per-minute quota before spending a
        // request on the 429 that would say the same thing.
        let wait = self
            .rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .wait_for_window();
        if let Some(wait) = wait {
            let _quota = crate::llm::usage::Waiting::new(
                wait + Duration::from_secs(1),
                crate::llm::usage::WaitReason::Quota,
            );
            if !sleep_observing(wait + Duration::from_secs(1), &is_cancelled) {
                return Err(GeminiError::Unavailable(CANCELLED.to_string()));
            }
        }
        // Atomically check rate limit and reserve a slot
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check_and_reserve()?;
        let reserve = RateReserve::held(&self.rate);

        let mut last_error: Option<GeminiError> = None;
        let mut next_delay = std::time::Duration::from_secs(5);
        // A per-minute 429 sleeps out Google's stated wait: that is quota, not a retry.
        let mut wait_reason = crate::llm::usage::WaitReason::Retry;

        for attempt in 0..MAX_RETRIES {
            if is_cancelled() {
                return Err(GeminiError::Unavailable(CANCELLED.to_string()));
            }
            if attempt > 0
                && !{
                    let _waiting = crate::llm::usage::Waiting::new(next_delay, wait_reason);
                    sleep_observing(next_delay, &is_cancelled)
                }
            {
                return Err(GeminiError::Unavailable(CANCELLED.to_string()));
            }
            if attempt > 0 {
                next_delay = doubled_backoff(next_delay);
            }

            crate::llm::HTTP_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let resp = match self.stream_request(request).send() {
                Ok(r) => r,
                Err(e) => {
                    let failure = http_error(e);
                    if attempt == MAX_RETRIES - 1 {
                        return Err(failure);
                    }
                    last_error = Some(failure);
                    continue;
                }
            };

            let status = resp.status().as_u16();
            match classify_status(status) {
                StatusAction::Read => {
                    // Moves the guard: on a mid-stream failure it drops here
                    // and the slot goes back. Both continuations return, so
                    // the loop never sees the moved value again.
                    let content = consume_success_body(reserve, resp)?;
                    let rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
                    self.persist_usage(&rate);
                    return Ok(content);
                }
                StatusAction::InvalidKey => return Err(GeminiError::InvalidKey),
                StatusAction::Denied => {
                    let body = read_body_capped(resp);
                    // A per-minute quota is weather, not a verdict: Google
                    // names the limit and the wait. Learn the one, sleep the
                    // other, try again. Per-day quotas fall through to the
                    // terminal path below, where a retry would only burn
                    // the player's remaining slots.
                    if status == 429 {
                        if let Some(quota) = quota_refusal(&body) {
                            if quota.per_day {
                                return Err(GeminiError::RateLimited(quota.summary()));
                            }
                            self.rate
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .learn_rpm(quota.limit);
                            if attempt + 1 < MAX_RETRIES && quota.retry_after <= MAX_QUOTA_WAIT {
                                next_delay = quota.retry_after + Duration::from_secs(1);
                                wait_reason = crate::llm::usage::WaitReason::Quota;
                                last_error = Some(GeminiError::RateLimited(quota.summary()));
                                continue;
                            }
                            return Err(GeminiError::RateLimited(quota.summary()));
                        }
                    }
                    return Err(denied_from_body(status, &body));
                }
                StatusAction::Retry => {
                    if let Some(delay) = retry_after_delay(resp.headers()) {
                        next_delay = delay;
                    }
                    last_error = Some(GeminiError::Api {
                        status,
                        message: read_body_capped(resp),
                    });
                    continue;
                }
                StatusAction::Fail => {
                    return Err(GeminiError::Api {
                        status,
                        message: read_body_capped(resp),
                    })
                }
            }
        }
        // All retries exhausted
        Err(last_error.unwrap_or_else(|| GeminiError::Api {
            status: 500,
            message: "Gemini server error after retries".into(),
        }))
    }

    /// Save rate tracker to disk if a persistence path is configured.
    ///
    /// Uses temp-write + atomic rename so a crash mid-write can't corrupt the
    /// usage file. A corrupted usage file makes the addon think it has zero
    /// quota left for the day and silently blocks LLM calls until the next
    /// daily rollover.
    fn persist_usage(&self, rate: &RateTracker) {
        if let Some(ref path) = self.usage_path {
            let persisted = rate.to_persisted();
            if let Ok(json) = serde_json::to_string(&persisted) {
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, &json).is_ok() {
                    let _ = std::fs::rename(&tmp, path);
                } else {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
    }

    pub fn remaining_quota(&self) -> u32 {
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remaining_today()
    }

    /// Slots left in the current 10-RPM window.
    #[cfg(test)]
    fn remaining_rpm(&self) -> u32 {
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remaining_rpm()
    }

    /// Clear the response cache.
    pub fn clear_cache(&self) {
        self.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_tracker_rpm_limit() {
        let mut tracker = RateTracker::new();
        for _ in 0..10 {
            assert!(tracker.check_and_reserve().is_ok());
        }
        // 11th check should fail (10 RPM limit)
        assert!(tracker.check_and_reserve().is_err());
    }

    #[test]
    fn test_rate_tracker_daily_limit() {
        let mut tracker = RateTracker::new();
        tracker.requests_today = 240;
        assert!(tracker.check_and_reserve().is_err());
    }

    #[test]
    fn test_rate_tracker_persistence_roundtrip_same_day() {
        let mut tracker = RateTracker::new();
        for _ in 0..5 {
            tracker.check_and_reserve().unwrap();
        }
        assert_eq!(tracker.requests_today, 5);
        assert_eq!(tracker.requests_this_minute, 5);

        let persisted = tracker.to_persisted();
        let reloaded = RateTracker::from_persisted(persisted);

        assert_eq!(reloaded.requests_today, 5);
        assert_eq!(reloaded.requests_this_minute, 5);
        assert_eq!(reloaded.remaining_rpm(), 5);
    }

    #[test]
    fn test_rate_tracker_persistence_day_rollover_resets_daily() {
        let yesterday = current_epoch_day().saturating_sub(1);
        let persisted = PersistedUsage {
            rpm_limits: HashMap::new(),
            day: yesterday,
            requests_today: 200,
            minute_start_epoch: 0,
            requests_this_minute: 0,
        };
        let reloaded = RateTracker::from_persisted(persisted);

        assert_eq!(reloaded.requests_today, 0);
        assert_eq!(reloaded.requests_this_minute, 0);
        assert_eq!(reloaded.current_day, current_epoch_day());
    }

    #[test]
    fn test_rate_tracker_minute_rollover_preserves_daily() {
        let mut tracker = RateTracker::new();
        for _ in 0..3 {
            tracker.check_and_reserve().unwrap();
        }
        assert_eq!(tracker.requests_this_minute, 3);
        assert_eq!(tracker.requests_today, 3);

        tracker.minute_start = Instant::now() - std::time::Duration::from_secs(61);

        tracker.check_and_reserve().unwrap();

        assert_eq!(tracker.requests_this_minute, 1);
        assert_eq!(tracker.requests_today, 4);
    }

    #[test]
    fn test_rate_tracker_daily_limit_enforced_after_reload() {
        // Simulate a DLL reload mid-day with the daily budget nearly exhausted.
        let persisted = PersistedUsage {
            rpm_limits: HashMap::new(),
            day: current_epoch_day(),
            requests_today: 240,
            minute_start_epoch: 0,
            requests_this_minute: 0,
        };
        let mut reloaded = RateTracker::from_persisted(persisted);
        assert_eq!(reloaded.requests_today, 240);
        assert!(reloaded.check_and_reserve().is_err());
    }

    #[test]
    fn test_remaining_quota() {
        let client = GeminiClient::new("fake-key", "gemini-2.5-flash").unwrap();
        assert_eq!(client.remaining_quota(), 250);
    }

    /// A14-2 — `create_client` builds a fresh Gemini client per Improve click.
    /// Daily 240 already survived the reload; the 10-RPM window did not.
    #[test]
    fn gemini_rpm_window_survives_create_client() {
        let path = std::env::temp_dir().join(format!(
            "gw2bo_gemini_rpm_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let first = GeminiClient::with_persistence("fake-key", "gemini-2.5-flash", path.clone())
            .expect("client");
        {
            let mut rate = first.rate.lock().expect("rate");
            for _ in 0..3 {
                rate.check_and_reserve().expect("within the minute limit");
            }
            first.persist_usage(&rate);
            assert_eq!(rate.remaining_rpm(), 7);
        }

        let second = GeminiClient::with_persistence("fake-key", "gemini-2.5-flash", path.clone())
            .expect("client");
        assert_eq!(
            second.remaining_rpm(),
            7,
            "a fresh client must inherit the minute window, not reset it to 10"
        );
        {
            let mut rate = second.rate.lock().expect("rate");
            assert_eq!(rate.requests_this_minute(), 3);
            for _ in 0..7 {
                rate.check_and_reserve().expect("remaining slots");
            }
            assert!(
                rate.check_and_reserve().is_err(),
                "the 10 RPM limit must still bite after create_client"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    /// A usage file written before the minute window was persisted must still
    /// load, and must keep the daily counter it does carry.
    #[test]
    fn legacy_usage_file_without_minute_fields_still_parses() {
        let legacy = format!("{{\"day\":{},\"requests_today\":7}}", current_epoch_day());
        let persisted: PersistedUsage = serde_json::from_str(&legacy).expect("legacy parses");
        let reloaded = RateTracker::from_persisted(persisted);
        assert_eq!(reloaded.requests_today, 7);
        assert_eq!(reloaded.requests_this_minute, 0);
        assert_eq!(reloaded.remaining_rpm(), 10);
    }

    #[test]
    fn test_function_call_name_round_trip() {
        // Gemini's protocol pairs functionCall/functionResponse by `name`
        // (no id field). The round-trip invariant: when we echo the call,
        // the name must survive serde → HTTP → serde untouched.
        let server_content_json = r#"{
            "role": "model",
            "parts": [
                {"functionCall": {"name": "square_number", "args": {"number": 7}}}
            ]
        }"#;

        let content: Content = serde_json::from_str(server_content_json).expect("parse Content");
        let call = content
            .parts
            .iter()
            .find_map(|p| p.function_call.as_ref())
            .expect("functionCall present");
        assert_eq!(call.name, "square_number");

        let result_part = Part::function_response(&call.name, serde_json::json!({"result": 49}));
        let wire = serde_json::to_value(&result_part).unwrap();
        assert_eq!(wire["functionResponse"]["name"], "square_number");
        assert!(wire["functionResponse"]["response"].is_object());
    }

    #[test]
    fn test_trim_contents_drops_oldest_turn() {
        fn user_text(s: &str) -> Content {
            Content {
                role: Some("user".into()),
                parts: vec![Part::text(s)],
            }
        }
        fn model_call(name: &str, payload: &str) -> Content {
            Content {
                role: Some("model".into()),
                parts: vec![Part {
                    thought_signature: None,
                    thought: None,
                    text: None,
                    function_call: Some(FunctionCall {
                        name: name.into(),
                        args: serde_json::json!({ "q": payload }),
                    }),
                    function_response: None,
                }],
            }
        }
        fn user_fresponse(name: &str, payload: &str) -> Content {
            Content {
                role: Some("user".into()),
                parts: vec![Part::function_response(
                    name,
                    serde_json::Value::String(payload.into()),
                )],
            }
        }

        let filler = "x".repeat(400);
        let mut contents = vec![
            user_text("initial prompt"),
            model_call("get_trait_details", &filler),
            user_fresponse("get_trait_details", &filler),
            model_call("get_trait_details", &filler),
            user_fresponse("get_trait_details", &filler),
            model_call("get_skill_info", &filler),
            user_fresponse("get_skill_info", &filler),
        ];
        let original_len = contents.len();

        trim_contents(&mut contents, 200);

        assert!(
            contents.len() < original_len,
            "expected trim, got {}",
            contents.len()
        );
        // Initial prompt preserved.
        assert_eq!(contents[0].role.as_deref(), Some("user"));
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("initial prompt"));
        // Last model call is still the most recent one (get_skill_info).
        let last_model = contents
            .iter()
            .rposition(|c| c.parts.iter().any(|p| p.function_call.is_some()))
            .expect("must retain a model call");
        let last_call_name = contents[last_model].parts[0]
            .function_call
            .as_ref()
            .map(|fc| fc.name.clone())
            .unwrap();
        assert_eq!(last_call_name, "get_skill_info");
        // Every function_response still has a preceding function_call (pair invariant).
        for (i, c) in contents.iter().enumerate() {
            if c.parts.iter().any(|p| p.function_response.is_some()) {
                assert!(
                    contents[..i]
                        .iter()
                        .any(|prev| prev.parts.iter().any(|p| p.function_call.is_some())),
                    "orphan function_response at index {}",
                    i
                );
            }
        }
    }

    #[test]
    fn test_trim_contents_noop_under_budget() {
        let mut contents = vec![Content {
            role: Some("user".into()),
            parts: vec![Part::text("short")],
        }];
        trim_contents(&mut contents, 10_000);
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn test_read_gemini_stream_merges_text_chunks() {
        let sse = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hel"}],"role":"model"},"index":0}]}
data: {"candidates":[{"content":{"parts":[{"text":"lo"}],"role":"model"},"index":0}]}

data: {"candidates":[{"content":{"parts":[{"text":"!"}],"role":"model"},"index":0}]}
"#;
        let content = read_gemini_stream(sse.as_bytes()).expect("stream parses");
        assert_eq!(content.role.as_deref(), Some("model"));
        assert_eq!(content.parts.len(), 1);
        assert_eq!(content.parts[0].text.as_deref(), Some("Hello!"));
    }

    /// Every chunk carries running `usageMetadata`; the last one is the
    /// request's. Thinking tokens bill as output.
    #[test]
    fn read_gemini_stream_records_the_last_usage_metadata() {
        let sse = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hel"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":900,"candidatesTokenCount":1,"totalTokenCount":901}}
data: {"candidates":[{"content":{"parts":[{"text":"lo"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":900,"candidatesTokenCount":20,"thoughtsTokenCount":80,"totalTokenCount":1000}}
"#;
        let scope = crate::llm::usage::UsageScope::new();
        read_gemini_stream(sse.as_bytes()).expect("stream parses");
        let run = scope.total();
        assert_eq!(run.tokens.prompt, 900);
        assert_eq!(run.tokens.completion, 100);
        assert_eq!(run.tokens.total, 1000);
        assert_eq!(run.tokens.requests, 1);
    }

    #[test]
    fn test_read_gemini_stream_passes_function_calls_through() {
        let sse = r#"data: {"candidates":[{"content":{"parts":[{"text":"Rolling"},{"functionCall":{"name":"pick","args":{"slot":"heal"}}}],"role":"model"},"index":0}]}
"#;
        let content = read_gemini_stream(sse.as_bytes()).expect("stream parses");
        assert_eq!(content.parts.len(), 2);
        assert_eq!(content.parts[0].text.as_deref(), Some("Rolling"));
        let call = content.parts[1]
            .function_call
            .as_ref()
            .expect("function call");
        assert_eq!(call.name, "pick");
        assert_eq!(call.args["slot"], "heal");
    }

    #[test]
    fn test_read_gemini_stream_error_payload_maps_to_api_error() {
        let sse = r#"data: {"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED"}}
"#;
        match read_gemini_stream(sse.as_bytes()) {
            Err(GeminiError::Api { status, message }) => {
                assert_eq!(status, 429);
                assert!(message.contains("exhausted"));
            }
            _other => panic!("expected Api error"),
        }
    }

    // Transport parity (leaf-1.1.6)

    /// A newline-free body that reports exactly how many bytes were pulled
    /// off it. The uncapped reader grew one `String` for the whole thing;
    /// counting the pull is the only way to prove the ceiling actually
    /// stopped the *read* rather than trimming afterwards.
    struct CountingReader {
        remaining: usize,
        emitted: u64,
    }

    impl std::io::Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            let n = buf.len().min(self.remaining);
            buf[..n].fill(b'x');
            self.remaining -= n;
            self.emitted += n as u64;
            Ok(n)
        }
    }

    /// A body that delivers one complete SSE frame and then the socket dies.
    struct DyingReader {
        head: Vec<u8>,
        pos: usize,
    }

    impl std::io::Read for DyingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.head.len() {
                let n = buf.len().min(self.head.len() - self.pos);
                buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ))
        }
    }

    fn dying_after_one_frame() -> DyingReader {
        DyingReader {
            head: b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}],\"role\":\"model\"}}]}\n"
                .to_vec(),
            pos: 0,
        }
    }

    /// GLM F6 / Claude F17: the socket read itself is bounded.
    ///
    /// `llm::gemini` rejects an oversized body at the trait boundary, but that
    /// check runs *after* this function has finished allocating. The peak
    /// lives here, inside the game process.
    #[test]
    fn gemini_stream_body_is_capped() {
        let slack: usize = 64 * 1024;
        let mut hostile = CountingReader {
            remaining: MAX_LLM_BODY as usize + slack,
            emitted: 0,
        };

        let err = read_gemini_stream(&mut hostile).expect_err("oversized body must be rejected");
        match err {
            GeminiError::Api {
                status,
                ref message,
            } => {
                assert_eq!(status, 502, "a body cap is a bad-gateway condition");
                assert!(
                    message.contains("body cap"),
                    "cap error must say so, got: {message}"
                );
            }
            other => panic!("expected the body-cap error, got {other:?}"),
        }

        // The load-bearing assertion: the reader was stopped at the ceiling,
        // not drained and then judged.
        assert_eq!(
            hostile.emitted, MAX_LLM_BODY,
            "read must stop exactly at the ceiling"
        );
        assert_eq!(
            hostile.remaining, slack,
            "the tail past the ceiling must never be pulled"
        );

        // Headroom: a normal stream is nowhere near the cap and still parses.
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}],\"role\":\"model\"}}]}\n";
        let content = read_gemini_stream(sse.as_bytes()).expect("normal stream still parses");
        assert_eq!(content.parts[0].text.as_deref(), Some("ok"));
    }

    /// GLM F16: a request that dies after the 200 gives its rate slot back.
    ///
    /// Drives `consume_success_body`, the real production step, rather than
    /// asserting on the guard in isolation — a guard that is never wired in
    /// proves nothing. Socket-free by construction: the failure shapes are
    /// fixtures.
    ///
    /// What keeps that honest: `consume_success_body` is private and
    /// `send_request` is its only production caller, so unwiring it fails
    /// `cargo clippy --all-targets -- -D warnings` with "function
    /// `consume_success_body` is never used" (verified by bypassing the call
    /// and running clippy). This test cannot see that on its own.
    #[test]
    fn gemini_rate_reservation_is_released_on_stream_failure() {
        fn spent(rate: &Mutex<RateTracker>) -> (u32, u32) {
            let t = rate.lock().expect("tracker");
            (t.requests_this_minute(), 250 - t.remaining_today())
        }

        // 1. Gemini's own mid-stream failure channel: HTTP 200, error payload.
        let rate = Mutex::new(RateTracker::new());
        rate.lock()
            .expect("tracker")
            .check_and_reserve()
            .expect("first slot");
        assert_eq!(spent(&rate), (1, 1), "reserve charges one slot");

        let sse = "data: {\"error\":{\"code\":503,\"message\":\"backend overloaded\"}}\n";
        let err = consume_success_body(RateReserve::held(&rate), sse.as_bytes())
            .expect_err("mid-stream error must fail the call");
        assert!(matches!(err, GeminiError::Api { status: 503, .. }));
        assert_eq!(
            spent(&rate),
            (0, 0),
            "a mid-stream failure must not charge the user a slot"
        );

        // 2. The socket dies while the body is still arriving.
        rate.lock()
            .expect("tracker")
            .check_and_reserve()
            .expect("slot after release");
        let err = consume_success_body(RateReserve::held(&rate), dying_after_one_frame())
            .expect_err("a dead socket must fail the call");
        assert!(matches!(err, GeminiError::Http(_)));
        assert_eq!(spent(&rate), (0, 0), "a dead socket must not charge a slot");

        // 3. An answer that actually arrived keeps the slot spent.
        rate.lock()
            .expect("tracker")
            .check_and_reserve()
            .expect("slot for the good call");
        let good = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}],\"role\":\"model\"}}]}\n";
        let content = consume_success_body(RateReserve::held(&rate), good.as_bytes())
            .expect("a complete body succeeds");
        assert_eq!(content.parts[0].text.as_deref(), Some("hi"));
        assert_eq!(
            spent(&rate),
            (1, 1),
            "a successful call must stay charged — releasing it would let the addon exceed the real quota"
        );

        // 4. The released slots are genuinely spendable again. On a free-tier
        //    key (10 RPM) leaked slots are what lock the user out.
        let rate = Mutex::new(RateTracker::new());
        for _ in 0..12 {
            rate.lock()
                .expect("tracker")
                .check_and_reserve()
                .expect("a released slot is reusable");
            let _ = consume_success_body(RateReserve::held(&rate), dying_after_one_frame());
        }
        assert_eq!(
            spent(&rate),
            (0, 0),
            "twelve failed calls on a 10 RPM key must leave the budget untouched"
        );
    }

    /// GLM F15 / Claude F32: transport timeouts and error classification.
    ///
    /// What this decides:
    /// * every Gemini HTTP call takes its wall clock from the shared provider
    ///   policy, and the client comes from the one factory that sets
    ///   `connect_timeout` — Gemini no longer builds a private client with a
    ///   180 s total and no connect bound;
    /// * a stream that fails on the wire is reported as a transport error,
    ///   not as `Parse` ("your model returned garbage");
    /// * 429 is terminal on Gemini, unlike the OpenAI-compatible providers.
    ///
    /// What it cannot decide: the `connect_timeout` *value* baked into
    /// `llm::openai_compat::http_client`. `reqwest::blocking::Client` has an
    /// opaque `Debug` (verified: it prints just `Client`) and the only
    /// behavioural probe is a real blackholed connect, which would be a live
    /// network call in a unit test. That constant is leaf-1.1.1's gate; the
    /// backstop for Gemini re-growing a private client is `cargo clippy
    /// --all-targets -- -D warnings`, where the orphaned `http_client` import
    /// is a hard error.
    #[test]
    fn gemini_transport_timeouts_and_error_kinds() {
        use crate::llm::openai_compat::{CONNECT_TIMEOUT_SECS, REQUEST_TIMEOUT_SECS};
        use std::time::Duration;

        // Timeouts
        gemini_http_client().expect("the shared client factory builds");
        // Compile-time: a connect bound must exist at all. Without one the OS
        // default applies, which on Windows is minutes of a frozen worker.
        const { assert!(CONNECT_TIMEOUT_SECS > 0 && CONNECT_TIMEOUT_SECS <= 30) };
        assert_eq!(
            CHAT_REQUEST_TIMEOUT,
            Duration::from_secs(120),
            "one completion budget shared with every other provider"
        );
        assert!(
            METADATA_TIMEOUT < CHAT_REQUEST_TIMEOUT,
            "a Settings-tab call must not be able to hang for a completion budget"
        );
        assert!(
            Duration::from_secs(REQUEST_TIMEOUT_SECS) >= CHAT_REQUEST_TIMEOUT,
            "the client-level bound must not cut a per-request budget short"
        );

        // The budgets the client actually attaches to a request. `new` builds
        // an HTTP client and nothing else — no key is contacted here.
        let client = GeminiClient::new("test-key-not-a-real-one", "gemini-2.5-flash")
            .expect("client builds offline");
        let generate = GenerateRequest {
            generation_config: None,
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![Part::text("hi")],
            }],
            tools: None,
        };

        let streamed = client
            .stream_request(&generate)
            .build()
            .expect("stream request builds");
        assert_eq!(
            streamed.timeout(),
            Some(&CHAT_REQUEST_TIMEOUT),
            "a completion must ride the shared budget, not Gemini's old private 180 s"
        );
        assert!(
            streamed.timeout() != Some(&Duration::from_secs(180)),
            "180 s was the shortest budget of any provider on the default pipeline"
        );
        // Live-verified 2026-08-27: this header returns 200 and
        // `Authorization: Bearer` returns 401.
        assert!(
            streamed.headers().contains_key("x-goog-api-key"),
            "Gemini authenticates by header"
        );
        assert!(
            !streamed.headers().contains_key("authorization"),
            "Bearer auth is a 401 on this API"
        );
        assert!(
            !streamed.url().as_str().contains("test-key-not-a-real-one"),
            "the key must never reach the URL, where it lands in logs"
        );

        let metadata = client
            .models_request()
            .build()
            .expect("models request builds");
        assert_eq!(
            metadata.timeout(),
            Some(&METADATA_TIMEOUT),
            "a Settings-tab call gets the short budget"
        );

        // Stream-read errors are transport, not parse
        let err = read_gemini_stream(dying_after_one_frame())
            .expect_err("a reset socket must fail the read");
        assert!(
            matches!(err, GeminiError::Http(_)),
            "a dead socket is a transport failure, not a parse failure: {err:?}"
        );
        assert!(err.to_string().contains("connection reset"));

        // A genuinely contentless body is still a parse failure — the
        // reclassification is targeted, not a blanket relabel.
        let err = read_gemini_stream(&b": keep-alive\n\n"[..]).expect_err("no content");
        assert!(
            matches!(err, GeminiError::Parse(_)),
            "an empty-but-healthy stream stays a parse failure: {err:?}"
        );

        // Status classification
        assert_eq!(classify_status(200), StatusAction::Read);
        assert_eq!(classify_status(401), StatusAction::InvalidKey);
        assert_eq!(classify_status(403), StatusAction::Denied);
        assert_eq!(
            classify_status(429),
            StatusAction::Denied,
            "403/429 read the body; billing keywords are not InvalidKey"
        );
        assert!(matches!(denied_from_body(403, ""), GeminiError::InvalidKey));
        assert!(matches!(
            denied_from_body(403, "RESOURCE_EXHAUSTED quota exceeded"),
            GeminiError::Api { status: 403, .. }
        ));
        assert!(matches!(
            denied_from_body(429, ""),
            GeminiError::RateLimited(_)
        ));
        assert!(matches!(
            denied_from_body(429, "RESOURCE_EXHAUSTED"),
            GeminiError::Api { status: 429, .. }
        ));
        for transient in [408, 500, 502, 503, 504] {
            assert_eq!(
                classify_status(transient),
                StatusAction::Retry,
                "{transient} is worth another attempt"
            );
        }
        for terminal in [400, 404, 413, 422] {
            assert_eq!(
                classify_status(terminal),
                StatusAction::Fail,
                "{terminal} is the caller's fault; retrying burns quota"
            );
        }

        // The backoff a retry uses is the shared clamped one, so no Gemini
        // retry ladder can grow past the shared ceiling.
        let mut delay = Duration::from_secs(5);
        for _ in 0..10 {
            delay = doubled_backoff(delay);
        }
        assert!(
            delay <= Duration::from_secs(60),
            "backoff must stay clamped, got {delay:?}"
        );
    }
    /// Cancel/unload must not wait out the 420s request timeout. Poll
    /// CancelScope between SSE lines the way `llm::sse::read_stream` does.
    #[test]
    fn read_gemini_stream_stops_on_cancel() {
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}],\"role\":\"model\"}}]}\n";
        let _scope = crate::llm::cancel::CancelScope::new(|| true);
        match read_gemini_stream(sse.as_bytes()).expect_err("cancel must fail the read") {
            GeminiError::Unavailable(msg) => assert_eq!(msg, crate::llm::cancel::CANCELLED),
            other => panic!("expected Unavailable, got: {other}"),
        }
    }

    /// A cancelled worker must leave `send_request` without opening a socket.
    /// Gemini's live URL is not unbound: any network attempt here waits on
    /// connect/timeout, not the instant `Unavailable` a cancelled unload needs.
    #[test]
    fn send_request_stops_on_cancel_without_touching_the_network() {
        let client = GeminiClient::new("fake-key", "gemini-2.5-flash").expect("client");
        let _scope = crate::llm::cancel::CancelScope::new(|| true);

        let started = std::time::Instant::now();
        let error = client
            .generate("hi")
            .expect_err("cancelled generate must fail");

        assert!(
            matches!(error, GeminiError::Unavailable(ref m) if m == crate::llm::cancel::CANCELLED),
            "got: {error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "cancel must be observed before any connect attempt"
        );
        assert_eq!(
            client.remaining_quota(),
            250,
            "a cancelled request must give its rate slot back"
        );
    }

    /// A cancelled worker must leave the tool loop without opening a socket.
    /// Between turns as well as inside one request: a tool loop is up to
    /// max_turns whole requests, so checking only inside one of them still
    /// leaves the worker running after the flag flips.
    #[test]
    fn tool_loop_stops_on_cancel_without_touching_the_network() {
        let client = GeminiClient::new("fake-key", "gemini-2.5-flash").expect("client");
        let tools = vec![Tool {
            function_declarations: vec![FunctionDeclaration {
                name: "t".into(),
                description: "d".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        }];
        let _scope = crate::llm::cancel::CancelScope::new(|| true);

        let started = std::time::Instant::now();
        let error = client
            .generate_with_tools_progress(
                "prompt",
                tools,
                &mut |_, _| serde_json::Value::Null,
                8,
                &mut |_, _, _| {},
            )
            .expect_err("cancelled loop must fail");

        assert!(
            matches!(error, GeminiError::Unavailable(ref m) if m == crate::llm::cancel::CANCELLED),
            "got: {error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "cancel must be observed before any connect attempt"
        );
        assert_eq!(
            client.remaining_quota(),
            250,
            "a cancelled request must give its rate slot back"
        );
    }

    /// Retry backoff cannot be driven without a 5xx HTTP fixture. Pin the
    /// three poll sites so a freeze that restores `thread::sleep` or drops
    /// a loop check fails this test.
    #[test]
    fn gemini_polls_cancelscope_at_stream_retry_and_tool_loop() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/gemini.rs"));

        let stream = src
            .split("fn read_gemini_stream")
            .nth(1)
            .and_then(|s| s.split("fn body_cap_exceeded").next())
            .expect("read_gemini_stream body");
        assert!(
            stream.contains("is_cancelled()"),
            "read_gemini_stream must poll is_cancelled between SSE lines"
        );

        let retry = src
            .split("fn send_request")
            .nth(1)
            .and_then(|s| s.split("fn persist_usage").next())
            .expect("send_request body");
        assert!(
            retry.contains("is_cancelled()"),
            "send_request must poll is_cancelled before each attempt"
        );
        assert!(
            retry.contains("sleep_observing(next_delay"),
            "retry backoff must sleep in cancel-observing slices"
        );
        assert!(
            !retry.contains("std::thread::sleep(next_delay)"),
            "retry backoff must not be an uninterruptible sleep"
        );

        let tool_loop = src
            .split("pub fn generate_with_tools_progress")
            .nth(1)
            .and_then(|s| s.split("fn send_request").next())
            .expect("tool loop body");
        // The loop itself is shared (`llm::tool_loop`); this provider only
        // drives turns. The cancel poll lives in the shared loop.
        assert!(
            tool_loop.contains("tool_loop::run("),
            "the Gemini tool loop must be the shared one"
        );
        let shared = include_str!("llm/tool_loop.rs");
        assert!(
            shared.contains("is_cancelled()"),
            "the shared tool loop must poll is_cancelled between turns"
        );
    }
}

#[cfg(test)]
mod minute_quota_tests {
    use super::*;

    /// The body gemini-3.8-flash returned in-game on 2026-09-07 20:04.
    const BODY: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.Help","links":[{"description":"Learn more about Gemini API quotas","url":"https://ai.google.dev/gemini-api/docs/rate-limits"}]},{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests","quotaId":"GenerateRequestsPerMinutePerProjectPerModel-FreeTier","quotaDimensions":{"location":"global","model":"gemini-3.8-flash"},"quotaValue":"5"}]},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"39s"}]}}"#;

    #[test]
    fn a_per_minute_quota_names_its_limit_and_wait() {
        let q = quota_refusal(BODY).expect("per-minute quota");
        assert!(!q.per_day);
        assert_eq!(q.limit, 5);
        assert_eq!(q.retry_after, Duration::from_secs(39));
        assert!(q.retry_after <= MAX_QUOTA_WAIT);
    }

    #[test]
    fn a_per_day_quota_is_named_and_not_waited_out() {
        let body = BODY.replace("PerMinutePerProjectPerModel", "PerDayPerProjectPerModel");
        let q = quota_refusal(&body).expect("per-day quota");
        assert!(q.per_day);
        assert!(q.summary().contains("per day"), "{}", q.summary());
        assert!(quota_refusal("not json").is_none());
        assert!(quota_refusal(r#"{"error":{"code":429}}"#).is_none());
    }

    #[test]
    fn a_learned_limit_paces_the_next_run_and_survives_a_reload() {
        let mut t = RateTracker::new().for_model("gemini-3.8-flash");
        assert_eq!(t.rpm_limit(), DEFAULT_RPM);
        assert_eq!(t.stated_rpm(), None, "a default is not a statement");
        t.learn_rpm(5);
        assert_eq!(t.stated_rpm(), Some(5));
        for _ in 0..5 {
            t.check_and_reserve().expect("under the learned limit");
        }
        assert!(t.wait_for_window().is_some(), "sixth request waits");
        assert!(t.check_and_reserve().is_err());

        let reloaded = RateTracker::from_persisted(t.to_persisted()).for_model("gemini-3.8-flash");
        assert_eq!(
            reloaded.rpm_limit(),
            5,
            "the stated limit is kept across runs"
        );
        let other = RateTracker::from_persisted(t.to_persisted()).for_model("gemini-flash-latest");
        assert_eq!(
            other.rpm_limit(),
            DEFAULT_RPM,
            "one model's quota is not another's"
        );
    }
}

#[cfg(test)]
mod closing_cap_tests {
    use super::*;

    /// The closing turn caps the reply; lookup turns do not send the field.
    #[test]
    fn only_the_closing_turn_carries_an_output_cap() {
        let closing = GenerateRequest {
            contents: vec![],
            tools: None,
            generation_config: Some(GenerationConfig {
                max_output_tokens: Some(crate::llm::openai_compat::CLOSING_MAX_TOKENS),
                thinking_config: None,
            }),
        };
        let json = serde_json::to_string(&closing).unwrap();
        assert!(
            json.contains("\"generationConfig\":{\"maxOutputTokens\":8192}"),
            "{json}"
        );
        let lookup = GenerateRequest {
            contents: vec![],
            tools: None,
            generation_config: None,
        };
        assert!(!serde_json::to_string(&lookup)
            .unwrap()
            .contains("generationConfig"));
    }
}
