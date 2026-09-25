//! Byte ceilings for every LLM response the addon reads.
//!
//! `gw2api::transport::read_body_capped` bounds every GW2 API body because a
//! hostile or broken endpoint must not stream unbounded bytes into the game
//! process. The LLM clients had no such bound, and they talk to third-party
//! aggregators (OpenRouter forwards upstream provider bytes verbatim), so a
//! single newline-free stream could grow one `String` without limit inside
//! Guild Wars 2.
//!
//! Timeouts bound *time*, not *bytes*: a fast endpoint can exhaust memory
//! well inside a 420-second budget. These caps bound bytes.

use std::io::Read;

use super::{http_error, LlmError};

/// Ceiling for one streamed completion body. `MAX_COMPLETION_TOKENS` is
/// 32_768; even at a pathological 20 bytes per token plus SSE framing that
/// is about 640 KiB, an order of magnitude under this.
pub(crate) const MAX_LLM_BODY: u64 = 8 * 1024 * 1024;

/// Ceiling for a non-streamed body: API error payloads and the `/models`
/// catalog. OpenRouter's catalog is the largest real case, a few hundred KiB.
pub(crate) const MAX_LLM_METADATA_BODY: u64 = 2 * 1024 * 1024;

/// Wrap a stream reader so it can yield at most [`MAX_LLM_BODY`] bytes.
pub(crate) fn body_capped<R: Read>(reader: R) -> std::io::Take<R> {
    reader.take(MAX_LLM_BODY)
}

/// Whether a [`body_capped`] reader stopped because it reached the ceiling
/// rather than because the provider finished sending.
pub(crate) fn hit_body_cap<R>(capped: &std::io::Take<R>) -> bool {
    capped.limit() == 0
}

/// The error a stream reader returns once it reaches [`MAX_LLM_BODY`].
pub(crate) fn body_cap_exceeded(label: &str) -> LlmError {
    LlmError::Api {
        status: 502,
        message: format!(
            "{label} response exceeded the {} MiB body cap and was dropped",
            MAX_LLM_BODY / (1024 * 1024)
        ),
    }
}

/// Read a non-streamed body with a hard byte ceiling, as lossy UTF-8.
///
/// Deliberately not `read_to_string`: the cap can land mid-codepoint, and a
/// truncated error body must still be reportable instead of turning into an
/// IO error of its own. Same reason the rest of the codebase uses
/// `chars().take(n)` and never `&text[..n]`.
pub(crate) fn read_body_capped(reader: impl Read) -> String {
    let mut buf = Vec::new();
    // A read failure mid-body still leaves whatever arrived; an error body is
    // diagnostics, so partial text beats discarding it.
    let _ = reader.take(MAX_LLM_METADATA_BODY).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read a JSON body with a hard byte ceiling and deserialize it. Replaces
/// `reqwest::blocking::Response::json`, which reads to EOF.
pub(crate) fn json_capped<T: serde::de::DeserializeOwned>(
    reader: impl Read,
) -> Result<T, LlmError> {
    let mut buf = Vec::new();
    reader
        .take(MAX_LLM_METADATA_BODY)
        .read_to_end(&mut buf)
        .map_err(http_error)?;
    serde_json::from_slice(&buf).map_err(|e| LlmError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_capped_stops_at_the_ceiling() {
        // One newline-free blob larger than the cap: the exact shape that
        // grew an unbounded String before.
        let huge = vec![b'x'; (MAX_LLM_BODY + 4096) as usize];
        let mut capped = body_capped(huge.as_slice());
        let mut sink = Vec::new();
        capped.read_to_end(&mut sink).expect("read");
        assert_eq!(sink.len() as u64, MAX_LLM_BODY);
        assert!(hit_body_cap(&capped));
    }

    #[test]
    fn body_capped_leaves_headroom_for_a_normal_stream() {
        let small = b"data: {}\n";
        let mut capped = body_capped(&small[..]);
        let mut sink = Vec::new();
        capped.read_to_end(&mut sink).expect("read");
        assert_eq!(sink, small);
        assert!(!hit_body_cap(&capped));
    }

    #[test]
    fn read_body_capped_survives_a_split_codepoint() {
        // 3-byte codepoints tile the cap so the ceiling cannot land on a
        // boundary; lossy decoding must not panic or error.
        let text = "\u{9F98}".repeat((MAX_LLM_METADATA_BODY as usize / 3) + 16);
        let out = read_body_capped(text.as_bytes());
        // Lossy decoding replaces the split tail with one U+FFFD, itself
        // three bytes, so the decoded string can end a few bytes over the
        // ceiling. What the cap bounds is the *read*, and that is what
        // matters: the input was far larger than the ceiling.
        assert!(
            out.len() as u64 <= MAX_LLM_METADATA_BODY + 4,
            "read was not bounded: {} bytes",
            out.len()
        );
        assert!(out.len() < text.len(), "oversized body must be truncated");
        assert!(out.starts_with('\u{9F98}'));
    }

    #[test]
    fn json_capped_rejects_a_truncated_document() {
        let padded = format!("{{\"data\":\"{}\"}}", "x".repeat(64));
        let ok: serde_json::Value = json_capped(padded.as_bytes()).expect("parses");
        assert!(ok.get("data").is_some());

        let truncated = &padded.as_bytes()[..padded.len() - 3];
        assert!(json_capped::<serde_json::Value>(truncated).is_err());
    }
}
