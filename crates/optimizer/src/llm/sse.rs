//! Shared SSE parsing for streaming chat completions.
//!
//! OpenAI-compatible providers (OpenAI, OpenRouter) stream `data:` lines
//! with OpenAI-shaped deltas. Keep-alive comments and blank lines are
//! skipped, content and tool-call deltas accumulate (parallel calls merged
//! by index, fragmented JSON arguments stitched back together), `[DONE]`
//! ends the stream, and a top-level `error` payload is the mid-stream
//! failure channel (the HTTP status is already 200 by then — see
//! OpenRouter's errors-and-debugging docs). Anthropic's event-based SSE
//! will need an adapter on top of these primitives.
//!
//! Three things bound what a provider can do to the game process here:
//! the total body is capped ([`super::body::MAX_LLM_BODY`]), the slot index
//! is capped ([`MAX_TOOL_CALL_INDEX`]), and the loop polls the caller's
//! cancellation predicate between lines.

use super::body::{body_cap_exceeded, body_capped, hit_body_cap};
use super::cancel::CANCELLED;
use super::openai_compat::{FunctionCallData, Message, ToolCallResponse};
use super::{http_error, LlmError};
use std::io::{BufRead, Read};

use serde::Deserialize;
use serde_json::Value;

/// Highest stream slot index any provider may address.
///
/// `tool_calls[index]` (OpenAI-compatible) and `content_block_start.index`
/// (Anthropic) are `usize` values taken verbatim from the wire and used as
/// `Vec` positions. An `index` of 10^12 is a multi-terabyte allocation —
/// which aborts the process rather than unwinding, so the `catch_unwind`
/// around the optimize worker cannot save it. Real responses use a handful
/// of slots; 32 is generous.
pub(crate) const MAX_TOOL_CALL_INDEX: usize = 31;

/// The error a stream reader returns for an out-of-range slot index.
pub(crate) fn slot_index_rejected(index: usize) -> LlmError {
    LlmError::Api {
        status: 502,
        message: format!(
            "stream slot index {index} exceeds the {MAX_TOOL_CALL_INDEX} cap; \
             refusing to allocate from a provider-supplied index"
        ),
    }
}

/// One `data:` payload from a streamed chat completion.
#[derive(Deserialize)]
pub(crate) struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// Top-level error for mid-stream failures — the HTTP status is already
    /// 200 by then, so OpenRouter ships the failure in-band.
    #[serde(default)]
    error: Option<Value>,
    /// Token usage, on the closing chunk. OpenAI sends it only when asked
    /// (`stream_options.include_usage`); OpenRouter always does, with `cost`.
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    /// OpenRouter only: what the request was charged, in USD.
    cost: Option<f64>,
}

impl From<WireUsage> for super::usage::ResponseUsage {
    fn from(u: WireUsage) -> Self {
        Self {
            prompt: u.prompt_tokens,
            completion: u.completion_tokens,
            total: u.total_tokens,
            cost_usd: u.cost,
        }
    }
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
    finish_reason: Option<String>,
    /// The upstream provider's own reason, which OpenRouter passes through
    /// untouched. This is the only field that says WHY a normalized
    /// `finish_reason: "error"` happened: Google reports
    /// `MALFORMED_FUNCTION_CALL` here when the model fails to emit a usable
    /// function call, and OpenRouter attaches no top-level `error` object for
    /// it because the provider itself answered 200. Dropping this field is how
    /// a diagnosable failure reached the player as a bare "Empty response"
    /// (gemini-3.8-flash via OpenRouter, measured 2026-09-05).
    native_finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCallDelta>>,
    /// Reasoning blocks, streamed a piece at a time. Kept opaque and in
    /// arrival order: what goes back on the next turn has to be the sequence
    /// the model produced.
    reasoning_details: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct StreamToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

/// Accumulates one streamed assistant message from ordered deltas.
#[derive(Default)]
pub(crate) struct StreamAccumulator {
    content: String,
    tool_calls: Vec<StreamToolCall>,
    reasoning_details: Vec<Value>,
}

#[derive(Default)]
struct StreamToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// What a completed stream produced.
#[derive(Debug)]
pub(crate) enum StreamedMessage {
    Message(Message),
    /// HTTP 200 but nothing usable came out; carries the finish reason plus
    /// any diagnostics the reader collected on the way.
    Empty(String),
}

pub(crate) fn apply_chunk(
    acc: &mut StreamAccumulator,
    finish: &mut Option<String>,
    chunk: StreamChunk,
) -> Result<(), LlmError> {
    for choice in chunk.choices {
        if let Some(reason) = choice.finish_reason {
            // Keep the provider's own wording when it says more than the
            // normalized one ("error" alone is not a diagnosis).
            *finish = Some(match choice.native_finish_reason {
                Some(native) if !native.eq_ignore_ascii_case(&reason) => {
                    format!("{reason}/{native}")
                }
                _ => reason,
            });
        }
        let Some(delta) = choice.delta else { continue };
        if let Some(text) = delta.content {
            super::live::content(&text);
            acc.content.push_str(&text);
        }
        let reasoning = delta.reasoning_details.unwrap_or_default();
        for block in &reasoning {
            if let Some(text) = block
                .get("text")
                .or_else(|| block.get("summary"))
                .and_then(Value::as_str)
            {
                super::live::reasoning(text);
            }
        }
        acc.reasoning_details.extend(reasoning);
        for call in delta.tool_calls.unwrap_or_default() {
            // Bound the index *before* it can size an allocation.
            if call.index > MAX_TOOL_CALL_INDEX {
                return Err(slot_index_rejected(call.index));
            }
            // At most MAX_TOOL_CALL_INDEX + 1 slots, guaranteed by the check
            // above — one push per missing slot, never a length from the wire.
            while acc.tool_calls.len() <= call.index {
                acc.tool_calls.push(StreamToolCall::default());
            }
            let slot = &mut acc.tool_calls[call.index];
            if let Some(id) = call.id {
                slot.id.push_str(&id);
            }
            if let Some(function) = call.function {
                if let Some(name) = function.name {
                    super::live::tool_call(&name);
                    slot.name.push_str(&name);
                }
                if let Some(args) = function.arguments {
                    slot.arguments.push_str(&args);
                }
            }
        }
    }
    Ok(())
}

impl StreamAccumulator {
    /// `None` when the stream carried neither content nor tool calls.
    fn into_message(self) -> Option<Message> {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .map(|call| ToolCallResponse {
                id: call.id,
                call_type: "function".to_string(),
                function: FunctionCallData {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect::<Vec<_>>();
        let content = (!self.content.is_empty()).then_some(self.content);
        if content.is_none() && tool_calls.is_empty() {
            return None;
        }
        Some(Message {
            role: "assistant".to_string(),
            content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            reasoning_details: (!self.reasoning_details.is_empty())
                .then_some(self.reasoning_details),
        })
    }
}

/// The JSON payload carried by one SSE line, or `None` when the line is
/// framing rather than content.
///
/// Three shapes are real traffic, measured against OpenRouter on 2026-08-27:
///
/// * `data: {...}` — the normal chunk. The spec also allows `data:{...}` with
///   no space; only the spaced form was accepted before, which would silently
///   drop every chunk from a provider that omits it.
/// * `: OPENROUTER PROCESSING` — keep-alive comments, 2-5 per response on
///   every model probed. Framing, never content.
/// * A bare JSON object with **no `data:` prefix at all**. OpenRouter answers
///   an upstream rate limit with HTTP **200**, `content-type:
///   text/event-stream`, and an unframed
///   `{"error":{"message":"Provider returned error","code":429,…}}`. Dropping
///   unframed lines turned that into a silent empty response instead of the
///   429 it is, so an unframed line is treated as a payload.
fn stream_payload(line: &str) -> Option<&str> {
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix("data:") {
        return Some(rest.strip_prefix(' ').unwrap_or(rest));
    }
    // Comment / keep-alive, and the remaining SSE field names, carry no chat
    // payload of their own.
    if line.starts_with(':')
        || line.starts_with("event:")
        || line.starts_with("id:")
        || line.starts_with("retry:")
    {
        return None;
    }
    Some(line)
}

/// Reads an SSE chat-completions body into one assistant message.
///
/// `is_cancelled` is polled once per line so an unload does not have to wait
/// out the request timeout; pass `&|| false` where cancellation is not
/// meaningful (the same convention as `scraper::scrape_all`).
pub(crate) fn read_stream<R: std::io::Read>(
    reader: R,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<StreamedMessage, LlmError> {
    let mut acc = StreamAccumulator::default();
    let mut finish: Option<String> = None;
    // Counted rather than logged: this crate has no logger, and a `println!`
    // from inside the game process is not one. The count rides out on the
    // Empty diagnostic, which is the only path where dropped chunks can be
    // the reason the user sees nothing.
    let mut skipped_payloads: usize = 0;
    let mut usage: Option<super::usage::ResponseUsage> = None;

    let mut capped = body_capped(reader);
    let mut buffered = std::io::BufReader::new(&mut capped);

    // Sniff before splitting into lines. A stream opens with `data:`, `:`,
    // `event:` or a blank line; a body whose first non-whitespace byte is `{`
    // is an error envelope, whatever the HTTP status and content-type claim.
    // Feeding that to the line splitter yields zero events, so the request
    // "succeeds" empty — a silent no-op with no error and no retry.
    if starts_like_json(&mut buffered)? {
        let mut body = String::new();
        buffered.read_to_string(&mut body).map_err(http_error)?;
        return Err(envelope_error(&body));
    }

    for line in buffered.lines() {
        if is_cancelled() {
            return Err(LlmError::Unavailable(CANCELLED.to_string()));
        }
        let line = line.map_err(http_error)?;
        let Some(payload) = stream_payload(line.trim_end()) else {
            continue;
        };
        if payload == "[DONE]" {
            break;
        }
        let mut chunk: StreamChunk = match serde_json::from_str(payload) {
            Ok(chunk) => chunk,
            Err(_) => {
                skipped_payloads += 1;
                continue;
            }
        };
        if let Some(err) = chunk.error {
            return Err(error_object_to_llm_error(&err));
        }
        if let Some(u) = chunk.usage.take() {
            usage = Some(u.into());
        }
        apply_chunk(&mut acc, &mut finish, chunk)?;
    }

    // Reaching the ceiling means the provider was still sending: the message
    // assembled so far is a fragment, not an answer.
    if hit_body_cap(&capped) {
        return Err(body_cap_exceeded("chat completion"));
    }
    // Billed whether or not the body held anything usable.
    super::usage::record(usage);

    match acc.into_message() {
        Some(message) => Ok(StreamedMessage::Message(message)),
        None => {
            let finish = finish.unwrap_or_else(|| "unknown".to_string());
            Ok(StreamedMessage::Empty(if skipped_payloads > 0 {
                format!("{finish}; {skipped_payloads} unparseable payload line(s) skipped")
            } else {
                finish
            }))
        }
    }
}

/// Whether the body's first non-whitespace byte is `{`, without consuming it.
fn starts_like_json<R: BufRead>(reader: &mut R) -> Result<bool, LlmError> {
    let head = reader.fill_buf().map_err(http_error)?;
    Ok(head
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{'))
}

/// Turn a whole-body error envelope into the error it describes.
fn envelope_error(body: &str) -> LlmError {
    match serde_json::from_str::<Value>(body) {
        Ok(value) => match value.get("error") {
            Some(err) => error_object_to_llm_error(err),
            // Valid JSON but not an envelope: the body was never a stream, so
            // there is nothing to accumulate either way.
            None => LlmError::Parse(format!(
                "expected an SSE stream, got a JSON document: {}",
                value.to_string().chars().take(240).collect::<String>()
            )),
        },
        Err(e) => LlmError::Parse(format!("unparseable non-stream body: {e}")),
    }
}

/// The status carried *inside* an error object. `metadata.raw` is a String
/// holding embedded JSON, not a nested object, so nothing here deserializes
/// into a struct — only `code` is read, and only as a number.
fn error_object_to_llm_error(err: &Value) -> LlmError {
    let code = err.get("code").and_then(Value::as_u64).unwrap_or(502);
    let status = u16::try_from(code).unwrap_or(502);
    LlmError::Api {
        status,
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_stream_accumulates_content_and_skips_keepalives() {
        let sse = concat!(
            ": OPENROUTER PROCESSING\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            ": OPENROUTER PROCESSING\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"total_tokens\":9}}\n",
            "data: [DONE]\n",
            "data: {\"ignored\":\"after done\"}\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Message(m) => {
                assert_eq!(m.role, "assistant");
                assert_eq!(m.content.as_deref(), Some("Hello"));
                assert!(m.tool_calls.is_none());
            }
            StreamedMessage::Empty(finish) => panic!("expected content, got empty: {finish}"),
        }
    }

    /// OpenRouter's closing chunk, as its usage-accounting docs show it:
    /// tokens plus the `cost` it charged. OpenAI's (with `include_usage`)
    /// is the same minus `cost`, on a chunk with no choices.
    #[test]
    fn read_stream_records_the_closing_usage_chunk() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":194,\"completion_tokens\":2,\"total_tokens\":196,\"cost\":0.95}}\n",
            "data: [DONE]\n",
        );
        let scope = crate::llm::usage::UsageScope::new();
        read_stream(sse.as_bytes(), &|| false).expect("ok");
        let run = scope.total();
        assert_eq!(run.tokens.prompt, 194);
        assert_eq!(run.tokens.completion, 2);
        assert_eq!(run.tokens.total, 196);
        assert_eq!(run.tokens.requests, 1);
        assert_eq!(run.reported_cost_usd, Some(0.95));
    }

    #[test]
    fn test_read_stream_merges_fragmented_parallel_tool_calls() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"a\",\"arguments\":\"{\\\"x\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Message(m) => {
                let calls = m.tool_calls.expect("tool calls");
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "c1");
                assert_eq!(calls[0].function.name, "a");
                assert_eq!(calls[0].function.arguments, "{\"x\":1}");
                assert_eq!(calls[1].id, "c2");
                assert_eq!(calls[1].function.arguments, "{}");
            }
            StreamedMessage::Empty(_) => panic!("expected tool calls"),
        }
    }

    #[test]
    fn test_read_stream_mid_stream_error_maps_to_api_error() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n",
            "data: {\"id\":\"x\",\"error\":{\"code\":429,\"message\":\"Rate limit exceeded\",\"metadata\":{\"error_type\":\"rate_limit_exceeded\"}},\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}]}\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect_err("mid-stream error must fail") {
            LlmError::Api { status, message } => {
                assert_eq!(status, 429);
                assert!(message.contains("Rate limit exceeded"));
            }
            other => panic!("expected Api error, got: {other}"),
        }
    }

    #[test]
    fn out_of_range_error_code_falls_back_to_502() {
        // 65965 as u16 wraps to 429 (RateLimited). Must not retry as 429.
        let err = serde_json::json!({"code": 65965, "message": "bogus"});
        match error_object_to_llm_error(&err) {
            LlmError::Api { status, .. } => assert_eq!(status, 502),
            other => panic!("expected Api error, got: {other}"),
        }
        let err_429 = serde_json::json!({"code": 429, "message": "rate"});
        match error_object_to_llm_error(&err_429) {
            LlmError::Api { status, .. } => assert_eq!(status, 429),
            other => panic!("expected Api error, got: {other}"),
        }
    }

    #[test]
    fn test_read_stream_empty_body_reports_finish_reason() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Empty(finish) => assert_eq!(finish, "length"),
            StreamedMessage::Message(_) => panic!("expected empty"),
        }
    }

    /// Measured OpenRouter shape (2026-09-05, generation
    /// `gen-1788632847`): gemini-3.8-flash failed to emit a usable function
    /// call, so Google answered 200 with `MALFORMED_FUNCTION_CALL` and
    /// OpenRouter normalized `finish_reason` to `"error"` — with no top-level
    /// `error` object, because the provider itself succeeded. Reporting only
    /// the normalized reason told the player "Empty response (finish_reason:
    /// error)", which names no cause they could act on.
    #[test]
    fn read_stream_empty_body_keeps_the_providers_own_finish_reason() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\",",
            "\"native_finish_reason\":\"MALFORMED_FUNCTION_CALL\"}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Empty(finish) => {
                assert_eq!(finish, "error/MALFORMED_FUNCTION_CALL");
                assert!(
                    super::super::openai_compat::is_function_call_failure(&LlmError::Parse(
                        format!("Empty response from OpenRouter (finish_reason: {finish})")
                    )),
                    "the tool loop must recognise this as a function-call failure"
                );
            }
            StreamedMessage::Message(_) => panic!("expected empty"),
        }
    }

    /// OpenRouter streams reasoning blocks a piece at a time and requires the
    /// sequence back unmodified on the next turn of a tool loop — Gemini 3
    /// rejects a loop whose blocks were dropped. The signature rides its own
    /// block, so losing arrival order loses the signature.
    #[test]
    fn streamed_reasoning_blocks_reach_the_assistant_message_in_order() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[",
            "{\"type\":\"reasoning.text\",\"text\":\"weighing prefixes\",\"index\":0}]}}]}
",
            "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[",
            "{\"type\":\"reasoning.encrypted\",\"data\":\"opaque\",\"index\":1}]}}]}
",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Minstrel.\"},\"finish_reason\":\"stop\"}]}
",
            "data: [DONE]
",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Message(m) => {
                let blocks = m.reasoning_details.clone().expect("blocks preserved");
                assert_eq!(blocks.len(), 2, "both chunks kept: {blocks:?}");
                assert_eq!(blocks[0]["text"], "weighing prefixes");
                assert_eq!(blocks[1]["data"], "opaque");
                // And they go back out on the wire under the name OpenRouter
                // reads them by.
                let wire = serde_json::to_value(&m).expect("serializes");
                assert_eq!(wire["reasoning_details"], serde_json::json!(blocks));
            }
            StreamedMessage::Empty(finish) => panic!("expected content, got empty: {finish}"),
        }
    }

    /// A provider that repeats the normalized reason verbatim must not produce
    /// "stop/stop".
    #[test]
    fn read_stream_does_not_repeat_an_identical_native_finish_reason() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\",",
            "\"native_finish_reason\":\"LENGTH\"}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Empty(finish) => assert_eq!(finish, "length"),
            StreamedMessage::Message(_) => panic!("expected empty"),
        }
    }

    /// Measured OpenRouter shape (2026-08-27): an upstream rate limit comes
    /// back as HTTP **200**, `content-type: text/event-stream`, and a bare
    /// JSON error object with no `data:` framing. Dropping unframed lines
    /// reported this as an empty response instead of the 429 it is.
    #[test]
    fn read_stream_surfaces_an_unframed_error_body_as_its_real_status() {
        let body = concat!(
            "{\"error\":{\"message\":\"Provider returned error\",\"code\":429,",
            "\"metadata\":{\"raw\":\"temporarily rate-limited upstream\",",
            "\"provider_name\":\"Io Net\",\"is_byok\":false,",
            "\"limit_source\":\"upstream_provider_shared_pool\"}}}",
        );
        match read_stream(body.as_bytes(), &|| false).expect_err("in-band 429 must fail") {
            LlmError::Api { status, message } => {
                assert_eq!(status, 429, "the code inside the body is the real status");
                assert!(message.contains("rate-limited upstream"), "got: {message}");
            }
            other => panic!("expected Api error, got: {other}"),
        }
    }

    /// Measured continuation shape (2026-08-27, several provider families).
    /// After the first fragment for a given `index`, `id`, `type` and
    /// `function.name` are **absent** — not null, not empty. Declaring any of
    /// them non-`Option` would fail deserialization on chunk 2 of every tool
    /// call the addon ever makes. Argument fragmentation is provider-
    /// dependent and includes empty-string fragments, so arguments are
    /// concatenated per index and parsed exactly once, by the caller, at the
    /// end. `native_finish_reason` is Google's un-normalized field ("STOP"
    /// for a tool call) and must not be branched on.
    #[test]
    fn read_stream_tolerates_measured_continuation_fragments() {
        let sse = concat!(
            ": OPENROUTER PROCESSING\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tool_get_weather_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n",
            // Continuations: index + arguments only, several of them empty.
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Oslo\\\"}\"}}]}}]}\n",
            // A second call, interleaved, identified only by its index.
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"tool_get_time_2\",\"type\":\"function\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{}\"}}]}}]}\n",
            // Finish chunk: no `tool_calls` key at all, and Google's
            // un-normalized reason disagreeing with the normalized one.
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"native_finish_reason\":\"STOP\"}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("stream parses") {
            StreamedMessage::Message(m) => {
                let calls = m.tool_calls.expect("tool calls");
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "tool_get_weather_1");
                assert_eq!(calls[0].function.name, "get_weather");
                assert_eq!(
                    calls[0].function.arguments, "{\"city\":\"Oslo\"}",
                    "empty fragments must concatenate away, not terminate"
                );
                assert_eq!(calls[1].id, "tool_get_time_2");
                assert_eq!(calls[1].function.name, "get_time");
                assert_eq!(calls[1].function.arguments, "{}");
            }
            StreamedMessage::Empty(finish) => panic!("expected tool calls, got empty: {finish}"),
        }
    }

    /// Slots are keyed by the wire `index`, not by arrival order, and a gap
    /// does not shift the calls that did arrive.
    #[test]
    fn read_stream_keys_tool_calls_by_index_not_arrival_order() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":2,\"id\":\"third\",\"function\":{\"name\":\"c\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"first\",\"function\":{\"name\":\"a\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":2,\"function\":{\"arguments\":\"\"}}]}}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("stream parses") {
            StreamedMessage::Message(m) => {
                let calls = m.tool_calls.expect("tool calls");
                // Index 1 never appeared, so it is not a call — but index 0
                // and 2 keep their own identities despite arriving reversed.
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "first");
                assert_eq!(calls[1].id, "third");
            }
            StreamedMessage::Empty(finish) => panic!("expected tool calls, got empty: {finish}"),
        }
    }

    /// The unframed error can also land *part-way through* an otherwise
    /// healthy stream — OpenRouter accepts the request, starts streaming, and
    /// only then hits the upstream pool limit. Detecting it at connect time
    /// alone would miss this.
    #[test]
    fn read_stream_surfaces_an_unframed_error_mid_stream() {
        let sse = concat!(
            ": OPENROUTER PROCESSING\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial ans\"}}]}\n",
            "{\"error\":{\"message\":\"Provider returned error\",\"code\":429,",
            "\"metadata\":{\"provider_name\":\"Io Net\",",
            "\"limit_source\":\"upstream_provider_shared_pool\"}}}\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect_err("mid-stream 429 must fail") {
            LlmError::Api { status, .. } => assert_eq!(status, 429),
            other => panic!("expected Api error, got: {other}"),
        }
    }

    /// GLM F2 / Claude F18: a provider-supplied index must never size a `Vec`.
    #[test]
    fn sse_tool_index_cap() {
        // Well inside `usize` so the test proves the *guard*, not an overflow.
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{},\"id\":\"c\",\"function\":{{\"name\":\"a\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n",
            MAX_TOOL_CALL_INDEX + 1
        );
        match read_stream(sse.as_bytes(), &|| false).expect_err("out-of-range index must fail") {
            LlmError::Api { status, message } => {
                assert_eq!(status, 502);
                assert!(message.contains("exceeds"), "got: {message}");
            }
            other => panic!("expected Api error, got: {other}"),
        }

        // usize::MAX is the shape that aborted the process: `index + 1`
        // overflows in debug and wraps to a 0-length Vec in release.
        let hostile = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{},\"function\":{{\"name\":\"a\"}}}}]}}}}]}}\n",
            usize::MAX
        );
        assert!(read_stream(hostile.as_bytes(), &|| false).is_err());

        // The last legal slot still works, so the cap is not off by one.
        let ok = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{},\"id\":\"c\",\"function\":{{\"name\":\"a\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\ndata: [DONE]\n",
            MAX_TOOL_CALL_INDEX
        );
        match read_stream(ok.as_bytes(), &|| false).expect("in-range index must parse") {
            StreamedMessage::Message(m) => {
                assert_eq!(m.tool_calls.expect("tool calls").len(), 1);
            }
            StreamedMessage::Empty(finish) => panic!("expected a tool call, got empty: {finish}"),
        }
    }

    #[test]
    fn read_stream_rejects_a_body_over_the_cap() {
        // A single newline-free blob: BufReader::lines() grew this without
        // limit before the cap landed.
        let huge = "x".repeat(super::super::body::MAX_LLM_BODY as usize + 1024);
        match read_stream(huge.as_bytes(), &|| false).expect_err("oversized body must fail") {
            LlmError::Api { status, message } => {
                assert_eq!(status, 502);
                assert!(message.contains("body cap"), "got: {message}");
            }
            other => panic!("expected Api error, got: {other}"),
        }
    }

    #[test]
    fn read_stream_stops_on_cancel() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
        match read_stream(sse.as_bytes(), &|| true).expect_err("cancel must fail the read") {
            LlmError::Unavailable(msg) => assert_eq!(msg, CANCELLED),
            other => panic!("expected Unavailable, got: {other}"),
        }
    }

    #[test]
    fn read_stream_accepts_data_without_a_space_and_counts_skips() {
        // Spec-legal `data:` with no space, plus one payload that is not JSON.
        let sse = concat!(
            "data:{\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n",
            "data: not json at all\n",
            "data: [DONE]\n",
        );
        match read_stream(sse.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Message(m) => assert_eq!(m.content.as_deref(), Some("ok")),
            StreamedMessage::Empty(finish) => panic!("expected content, got empty: {finish}"),
        }

        // With nothing usable, the skipped count is what explains the silence.
        let only_junk = "data: not json at all\ndata: also not json\ndata: [DONE]\n";
        match read_stream(only_junk.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Empty(finish) => {
                assert!(finish.contains('2'), "expected a skip count, got: {finish}");
                assert!(finish.contains("skipped"), "got: {finish}");
            }
            StreamedMessage::Message(_) => panic!("expected empty"),
        }

        // Keep-alive comments are framing, not dropped content: OpenRouter
        // sends 2-5 of them per response and they must not inflate the count.
        let keepalives = concat!(
            ": OPENROUTER PROCESSING\n",
            "\n",
            ": OPENROUTER PROCESSING\n",
            "event: ping\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        );
        match read_stream(keepalives.as_bytes(), &|| false).expect("ok") {
            StreamedMessage::Empty(finish) => assert_eq!(finish, "stop"),
            StreamedMessage::Message(_) => panic!("expected empty"),
        }
    }
}
