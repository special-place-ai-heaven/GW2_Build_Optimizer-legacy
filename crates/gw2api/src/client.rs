//! GW2 API v2 HTTP client with rate limiting.
//! Rate limit: 300 burst, 5 tokens/sec refill. Max 200 IDs per bulk request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde::de::DeserializeOwned;

const BASE_URL: &str = "https://api.guildwars2.com/v2";
pub(crate) const MAX_BULK_IDS: usize = 200;
const BUCKET_CAPACITY: u32 = 300;
const REFILL_RATE: f64 = 5.0; // tokens per second
const MAX_RETRIES: u32 = 5;
/// Upper bound on honoring a server-sent `Retry-After`. Beyond this we
/// short-circuit to `ApiError::RateLimited` so background threads don't block
/// for minutes on an uninterruptible `thread::sleep`.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);
/// Cap on any single JSON body this client buffers. A 200-ID `/v2/items`
/// batch is ~1 MiB, so this is ~8x headroom over the largest real payload.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// Cap on one render-service icon. Real GW2 icons are 2-30 KiB; a response
/// past this is a misbehaving host, not an icon, and is rejected rather than
/// silently truncated into a corrupt PNG.
const MAX_ICON_BYTES: u64 = 4 * 1024 * 1024;
/// Attempts per icon. A refresh fetches ~10k icons, so a dropped connection
/// must not silently cost the user one — but the retry redials immediately and
/// stops at two, because a real CDN outage would otherwise turn 10k failures
/// into 10k backoff sleeps. Icons are not load-bearing: a miss is picked up by
/// the next refresh, which fetches only what is absent from disk.
const ICON_ATTEMPTS: u32 = 2;
/// Longest single uninterruptible `thread::sleep`. Every wait longer than
/// this is split into slices so `is_cancelled()` is observed within one slice.
pub(crate) const CANCEL_POLL: Duration = Duration::from_millis(100);
/// Aggregate backoff budget for one `get_with_params` call. `MAX_RETRIES`
/// honoring a 30 s `Retry-After` each would otherwise sleep 120 s inside a
/// single call; past this budget we stop retrying and surface the last error.
const MAX_TOTAL_BACKOFF: Duration = Duration::from_secs(60);

/// What a 429's `Retry-After` header tells the retry loop to do.
///
/// A pure decision so the rule can be tested without a socket: an integration
/// test over loopback cannot prove "did not retry" on a host that drops
/// connections, because the client's (correct) connection-error retry is
/// indistinguishable from a policy retry at the mock.
#[derive(Debug, PartialEq, Eq)]
enum RateLimitAction {
    /// Server named a wait we are willing to honor.
    Wait(Duration),
    /// Server named a wait past `RETRY_AFTER_CAP` — give up now instead of
    /// parking a background thread for minutes.
    GiveUp,
    /// No usable header; fall back to exponential backoff.
    Backoff,
}

fn rate_limit_action(headers: &HeaderMap) -> RateLimitAction {
    match parse_retry_after(headers) {
        Some(wait) if wait > RETRY_AFTER_CAP => RateLimitAction::GiveUp,
        Some(wait) => RateLimitAction::Wait(wait),
        None => RateLimitAction::Backoff,
    }
}

/// Parse `Retry-After` as integer seconds (RFC 7231 delta-seconds form).
/// HTTP-date is intentionally unsupported — GW2 API returns integer seconds.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}

/// UTF-8-safe truncation of response bodies to a short, loggable snippet.
/// Used by `ApiError::Api` to bound the message size; GW2 error payloads
/// are already short, but HTML from intermediaries can be arbitrarily large
/// (and is noise — we drop it entirely).
fn body_snippet(body: &str) -> String {
    const MAX: usize = 200;
    if body.contains('<') {
        return String::new();
    }
    body.chars().take(MAX).collect()
}

/// Build the comma-separated `ids` query value used by GW2 API bulk endpoints.
///
/// Accepts numeric IDs and string IDs. The caller is responsible for chunking
/// to the GW2 API's 200-ID cap before invoking this helper.
pub(crate) fn build_bulk_ids_query<T: std::fmt::Display>(ids: &[T]) -> String {
    let mut out = String::new();
    let mut first = true;
    for id in ids {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&id.to_string());
    }
    out
}

/// Convert a JSON numeric ID into `u32`, accepting JSON numbers and numeric strings.
fn value_to_u32(v: &serde_json::Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        u32::try_from(n).ok()
    } else if let Some(s) = v.as_str() {
        s.parse::<u32>().ok()
    } else {
        None
    }
}

/// Convert a GW2 endpoint ID into the string form accepted by `ids=` bulk queries.
/// Most endpoints return numeric IDs; `/v2/legends` returns IDs like `Legend1`.
fn value_to_bulk_id(v: &serde_json::Value) -> Option<String> {
    value_to_u32(v)
        .map(|n| n.to_string())
        .or_else(|| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 500 | 502 | 503 | 504)
}

fn is_retryable_api(err: &ApiError) -> bool {
    matches!(err, ApiError::Api { status, .. } if is_retryable_status(*status))
}

/// Fetch `ids` in one bulk call. On a 5xx, split the set. A singleton
/// retryable 5xx is skip-listed (not `Ok([])`) so a dump can record the
/// hole instead of writing it as success.
fn merge_bulk_fetch<T>(
    ids: &[serde_json::Value],
    fetch: &mut impl FnMut(&[serde_json::Value]) -> Result<Vec<T>, ApiError>,
) -> Result<(Vec<T>, Vec<serde_json::Value>), ApiError> {
    match fetch(ids) {
        Ok(v) => Ok((v, Vec::new())),
        Err(e) if ids.len() > 1 && is_retryable_api(&e) => {
            let mid = ids.len() / 2;
            let (mut left, mut skip_l) = merge_bulk_fetch(&ids[..mid], fetch)?;
            let (right, skip_r) = merge_bulk_fetch(&ids[mid..], fetch)?;
            left.extend(right);
            skip_l.extend(skip_r);
            Ok((left, skip_l))
        }
        Err(e) if ids.len() == 1 && is_retryable_api(&e) => Ok((Vec::new(), vec![ids[0].clone()])),
        Err(e) => Err(e),
    }
}

/// Rate-limited GW2 API client.
pub struct Gw2Client {
    http: Client,
    api_key: Option<String>,
    bucket: Mutex<TokenBucket>,
    lang: Option<String>,
    /// Override for the API root. `None` uses [`BASE_URL`]. Tests point this at
    /// a mockito loopback server so relative endpoints (`items`, `build`, …)
    /// resolve locally.
    api_root: Option<String>,
    /// Shared cancellation flag. Every interruptible wait this client performs
    /// observes it, so setting it from another thread aborts an in-flight
    /// retry ladder or rate-limit wait within `CANCEL_POLL`. It is deliberately
    /// sticky: a cancelled client stays cancelled and refuses further waits.
    cancel: Arc<AtomicBool>,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new() -> Self {
        Self {
            tokens: BUCKET_CAPACITY as f64,
            last_refill: Instant::now(),
        }
    }

    /// Take a token, returning the duration to sleep if the bucket was empty.
    /// The caller must sleep AFTER releasing the mutex lock to avoid blocking
    /// other threads that want to check/take tokens concurrently. After sleeping,
    /// the caller loops back to `take()` again to atomically consume the
    /// now-refilled token.
    fn take(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * REFILL_RATE).min(BUCKET_CAPACITY as f64);
        self.last_refill = now;

        if self.tokens < 1.0 {
            // Compute the wait until one full token has accumulated. We do NOT
            // reset `self.tokens` here — the next `take()` call (after the
            // caller sleeps and re-locks) will roll the fractional remainder
            // into the new total via the `elapsed * REFILL_RATE` accumulation
            // above. Zeroing `self.tokens` would discard the fractional progress
            // and force the caller to wait an extra (1.0 - tokens)/rate seconds.
            let wait = Duration::from_secs_f64((1.0 - self.tokens) / REFILL_RATE);
            Some(wait)
        } else {
            self.tokens -= 1.0;
            None
        }
    }
}

/// Run `body` with a watchdog thread that mirrors `cancelled()` into `client`'s
/// cancel flag — the only cancellation a `Gw2Client` can observe while it is
/// blocked inside a request. One-way and terminal: once observed, `client`
/// stays cancelled.
///
/// `thread::scope` joins the watchdog before returning, so no thread outlives
/// this call, and the done-flag is set by a `Drop` guard: a panic in `body`
/// then unwinds through the join instead of deadlocking against a watchdog
/// that was never told to stop.
pub(crate) fn with_cancel_bridge<T, C: Fn() -> bool + Sync>(
    client: &Gw2Client,
    cancelled: &C,
    body: impl FnOnce() -> T,
) -> T {
    struct StopWatchdog<'a>(&'a AtomicBool);
    impl Drop for StopWatchdog<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Polling costs up to one `CANCEL_POLL` of extra latency at the end
            // of an operation that already takes minutes.
            while !finished.load(Ordering::Relaxed) {
                if cancelled() {
                    client.cancel();
                    return;
                }
                std::thread::sleep(CANCEL_POLL);
            }
        });
        let _stop = StopWatchdog(&finished);
        body()
    })
}

/// Errors returned by the GW2 API client.
///
/// Variant conventions (see `code-review` skill for the binding rule):
/// - `Api` — GW2 API returned a non-2xx response. Always populates
///   `url_path` (the relative endpoint, e.g. `"items"`) and `body_snippet`
///   (≤200 chars, UTF-8 safe). Do NOT use for non-HTTP failures.
/// - `RateLimited` — 429 retries exhausted or `Retry-After` exceeded the
///   cap. Carries the endpoint that tripped the limit.
/// - `Cancelled` — the caller's cancellation flag was observed. Terminal:
///   the work was abandoned on request, not because anything failed.
/// - `Cache` — on-disk cache read/write failure.
/// - `Internal` — panics, invalid config, unrecoverable client state.
///   Never a sentinel for HTTP errors.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("API error {status} on {url_path}: {body_snippet}")]
    Api {
        status: u16,
        url_path: String,
        body_snippet: String,
    },
    #[error("Rate limited after {retries} retries on {url_path}")]
    RateLimited { retries: u32, url_path: String },
    #[error("cancelled")]
    Cancelled,
    #[error("invalid endpoint: {0} — must be a relative API path")]
    InvalidEndpoint(String),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

/// API key permissions the addon needs: `account` (account name for
/// feedback), `characters`, and `builds` (build and equipment tabs).
pub const REQUIRED_SCOPES: [&str; 3] = ["account", "characters", "builds"];

/// Token info returned by /v2/tokeninfo.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    pub permissions: Vec<String>,
}

/// Game build number from /v2/build.
#[derive(Debug, Clone, serde::Deserialize)]
struct BuildInfo {
    id: u32,
}

fn apply_lang_query(url: &mut String, params: &[(&str, &str)], lang: Option<&str>) {
    let Some(lang) = lang else {
        return;
    };
    if params.iter().any(|(k, _)| *k == "lang") {
        return;
    }
    if url.contains('?') {
        url.push_str("&lang=");
    } else {
        url.push_str("?lang=");
    }
    url.push_str(lang);
}

/// Absolute HTTP(S) endpoints are allowed only when the parsed host is
/// exactly loopback. A prefix check is not enough: `127.0.0.1.evil.com`
/// and `127.0.0.1@evil.com` (userinfo) both start with a loopback literal
/// but resolve off-host. Relative GW2 paths do not parse as absolute URLs
/// and pass through for `BASE_URL` joining.
fn reject_non_loopback_absolute(endpoint: &str) -> Result<(), ApiError> {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return Ok(());
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return Ok(());
    }
    match url.host_str() {
        // host_str() keeps IPv6 brackets (`[::1]`); `::1` is the parsed equivalent.
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]") => Ok(()),
        _ => Err(ApiError::InvalidEndpoint(endpoint.to_string())),
    }
}

impl Gw2Client {
    pub fn new(api_key: Option<String>) -> Result<Self, ApiError> {
        let http = Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self {
            http,
            api_key,
            bucket: Mutex::new(TokenBucket::new()),
            lang: None,
            api_root: None,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn with_key(api_key: &str) -> Result<Self, ApiError> {
        Self::new(Some(api_key.to_string()))
    }

    pub fn without_key() -> Result<Self, ApiError> {
        Self::new(None)
    }

    /// Official `/v2` locales that return translated names. `None` = English cache.
    pub fn with_lang(mut self, lang: Option<&str>) -> Self {
        self.lang = lang.and_then(|c| match c {
            "de" | "es" | "fr" | "zh" => Some(c.to_string()),
            _ => None,
        });
        self
    }

    /// Point relative `/v2/...` paths at `root` (no trailing slash). Used by
    /// mockito tests and FOLD3 fetch-count instrumentation.
    pub fn with_api_root(mut self, root: impl Into<String>) -> Self {
        let root = root.into().trim_end_matches('/').to_string();
        self.api_root = Some(root);
        self
    }

    /// Abort this client's in-flight and future waits. Terminal — there is no
    /// un-cancel, because a cancelled download must not silently resume.
    pub fn cancel(&self) {
        // Relaxed: the flag carries no data of its own, and every reader only
        // needs to observe the store eventually, within one `CANCEL_POLL`.
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested for this client.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Sleep up to `total`, waking every `CANCEL_POLL` to check cancellation.
    /// Returns `false` if cancellation was observed (the remaining time is not
    /// slept), `true` if the full duration elapsed.
    fn sleep_cancellable(&self, total: Duration) -> bool {
        let deadline = Instant::now() + total;
        loop {
            if self.is_cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::sleep((deadline - now).min(CANCEL_POLL));
        }
    }

    /// Make a GET request to the API with rate limiting and retries.
    pub fn get<T: DeserializeOwned>(&self, endpoint: &str) -> Result<T, ApiError> {
        self.get_with_params(endpoint, &[])
    }

    /// Make a GET request with query parameters.
    /// Builds query string manually to avoid URL-encoding commas in bulk ID requests.
    /// Retries on connection errors (timeouts) AND server errors (500/502/503/504).
    ///
    /// Every wait between attempts observes `is_cancelled()` within
    /// `CANCEL_POLL` and returns `ApiError::Cancelled`; the aggregate backoff
    /// per call is capped at `MAX_TOTAL_BACKOFF`. The in-flight HTTP read
    /// itself is not interruptible — the 30 s client timeout bounds it.
    pub fn get_with_params<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        // Relative path by contract. Absolute URLs are parsed and allowed
        // only for loopback (mock-server tests); a prefix check is bypassable.
        reject_non_loopback_absolute(endpoint)?;
        let base_url = if endpoint.starts_with("http") {
            endpoint.to_string()
        } else {
            let root = self.api_root.as_deref().unwrap_or(BASE_URL);
            format!("{}/{}", root, endpoint.trim_start_matches('/'))
        };
        let url_path = endpoint.to_string();

        // Build query string manually — reqwest's .query() encodes commas as %2C,
        // which triples separator length and can exceed URL limits for bulk ID requests.
        // We URL-encode values for safety but preserve commas (GW2 API uses them as
        // list separators in bulk ID requests).
        let mut url = if params.is_empty() {
            base_url
        } else {
            let query = params
                .iter()
                .map(|(k, v)| {
                    let encoded = urlencoding::encode(v).into_owned().replace("%2C", ",");
                    format!("{}={}", k, encoded)
                })
                .collect::<Vec<_>>()
                .join("&");
            format!("{}?{}", base_url, query)
        };
        apply_lang_query(&mut url, params, self.lang.as_deref());

        let mut last_error: Option<ApiError> = None;
        // When Some, the next retry waits this duration instead of exponential
        // backoff — set by a 429 response with a `Retry-After` header.
        let mut suggested_wait: Option<Duration> = None;
        // Aggregate backoff already slept in this call, bounded by
        // `MAX_TOTAL_BACKOFF` so one call cannot sit in `thread::sleep` for
        // minutes even when the server keeps asking for the maximum wait.
        let mut backoff_spent = Duration::ZERO;

        for attempt in 0..MAX_RETRIES {
            if self.is_cancelled() {
                return Err(ApiError::Cancelled);
            }

            // Backoff before retries (not before first attempt)
            if attempt > 0 {
                let wait = suggested_wait.take().unwrap_or_else(|| {
                    Duration::from_millis(
                        (2000u64.saturating_mul(2u64.saturating_pow(attempt - 1))).min(30_000),
                    )
                });
                if backoff_spent + wait > MAX_TOTAL_BACKOFF {
                    break; // budget exhausted — surface `last_error` below
                }
                if !self.sleep_cancellable(wait) {
                    return Err(ApiError::Cancelled);
                }
                backoff_spent += wait;
            }

            // Take a token — sleep OUTSIDE the lock to allow concurrent threads.
            // Loop until we actually acquire a token (sleep may not refill enough).
            loop {
                let sleep_dur = self.bucket.lock().unwrap_or_else(|e| e.into_inner()).take();
                match sleep_dur {
                    None => break,
                    Some(wait) => {
                        if !self.sleep_cancellable(wait) {
                            return Err(ApiError::Cancelled);
                        }
                    }
                }
            }

            let mut headers = HeaderMap::new();
            headers.insert(
                USER_AGENT,
                HeaderValue::from_static("GW2BuildOptimizer/0.1"),
            );
            if let Some(ref key) = self.api_key {
                let header_val = match HeaderValue::from_str(&format!("Bearer {}", key)) {
                    Ok(v) => v,
                    Err(_) => {
                        return Err(ApiError::Internal(
                            "API key contains invalid characters".into(),
                        ))
                    }
                };
                headers.insert(AUTHORIZATION, header_val);
            }

            // Connection errors (timeout, DNS, reset) are retryable — do NOT use `?`
            let resp = match self.http.get(&url).headers(headers).send() {
                Ok(resp) => resp,
                Err(e) => {
                    last_error = Some(ApiError::Http(e));
                    continue; // retry on connection failure
                }
            };

            let status = resp.status().as_u16();

            if status == 429 {
                let action = rate_limit_action(resp.headers());
                // Drain the body before dropping the response. An unread body
                // leaves the pooled connection unusable: hyper tears it down
                // asynchronously while reqwest may still hand it to the retry,
                // which then dies with ECONNRESET and burns a whole attempt.
                let _ = crate::transport::read_body_capped(resp, MAX_BODY_BYTES);
                match action {
                    RateLimitAction::GiveUp => {
                        return Err(ApiError::RateLimited {
                            retries: attempt + 1,
                            url_path,
                        })
                    }
                    RateLimitAction::Wait(wait) => suggested_wait = Some(wait),
                    RateLimitAction::Backoff => {}
                }
                last_error = Some(ApiError::RateLimited {
                    retries: attempt + 1,
                    url_path: url_path.clone(),
                });
                continue;
            }
            if is_retryable_status(status) {
                let body = {
                    crate::transport::read_body_capped(resp, MAX_BODY_BYTES)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default()
                };
                last_error = Some(ApiError::Api {
                    status,
                    url_path: url_path.clone(),
                    body_snippet: body_snippet(&body),
                });
                continue;
            }

            if !resp.status().is_success() {
                let body = {
                    crate::transport::read_body_capped(resp, MAX_BODY_BYTES)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default()
                };
                return Err(ApiError::Api {
                    status,
                    url_path,
                    body_snippet: body_snippet(&body),
                });
            }

            // Read body — connection can fail here too
            let text = match crate::transport::read_body_capped(resp, MAX_BODY_BYTES) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(ApiError::Internal(format!(
                        "JSON body exceeds {MAX_BODY_BYTES} bytes on {url_path}"
                    )));
                }
                Err(e) => {
                    last_error = Some(ApiError::Internal(format!("body read failed: {e}")));
                    continue; // retry on read failure
                }
            };

            let parsed: T = serde_json::from_str(&text)?;
            return Ok(parsed);
        }

        // Cancellation raised during the last in-flight request outranks
        // whatever that request failed with: the caller asked us to stop, so
        // report that rather than a transport error they did not cause.
        if self.is_cancelled() {
            return Err(ApiError::Cancelled);
        }

        Err(last_error.unwrap_or_else(|| {
            ApiError::Internal(format!(
                "GW2 API unavailable after {} retries on {}",
                MAX_RETRIES, url_path
            ))
        }))
    }

    /// Fetch all IDs from an endpoint root, then bulk-fetch in batches of 200.
    pub fn fetch_all<T: DeserializeOwned + Send>(
        &self,
        endpoint: &str,
    ) -> Result<Vec<T>, ApiError> {
        let ids: Vec<serde_json::Value> = self.get(endpoint)?;
        self.fetch_by_ids(endpoint, &ids)
    }

    /// Fetch items by a list of IDs in batches of 200, up to 5 concurrent.
    pub fn fetch_by_ids<T: DeserializeOwned + Send>(
        &self,
        endpoint: &str,
        ids: &[serde_json::Value],
    ) -> Result<Vec<T>, ApiError> {
        self.fetch_by_ids_with_progress(endpoint, ids, |_, _| {})
    }

    /// Like `fetch_by_ids` but calls `on_progress(fetched_so_far, total)` after each batch group.
    pub fn fetch_by_ids_with_progress<T: DeserializeOwned + Send>(
        &self,
        endpoint: &str,
        ids: &[serde_json::Value],
        on_progress: impl FnMut(usize, usize),
    ) -> Result<Vec<T>, ApiError> {
        let (items, skipped) = self.fetch_by_ids_with_skips(endpoint, ids, on_progress)?;
        if skipped.is_empty() {
            Ok(items)
        } else {
            Err(ApiError::Internal(format!(
                "bulk 5xx skipped id(s) on {endpoint}: {}",
                skipped
                    .iter()
                    .filter_map(value_to_bulk_id)
                    .collect::<Vec<_>>()
                    .join(",")
            )))
        }
    }

    /// One <=MAX_BULK_IDS bulk request (split/skip on 5xx).
    /// Install commits `items.partial` after each completed chunk.
    pub(crate) fn fetch_bulk_chunk<T: DeserializeOwned + Send>(
        &self,
        endpoint: &str,
        chunk: &[serde_json::Value],
    ) -> Result<(Vec<T>, Vec<serde_json::Value>), ApiError> {
        merge_bulk_fetch(chunk, &mut |part| {
            let ids: Vec<String> = part.iter().filter_map(value_to_bulk_id).collect();
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let joined = build_bulk_ids_query(&ids);
            self.get_with_params::<Vec<T>>(endpoint, &[("ids", &joined)])
        })
    }

    /// Like `fetch_by_ids_with_progress`, but a singleton retryable 5xx is
    /// collected into `skipped` so an items dump can skip-list the hole
    /// instead of aborting the whole `/v2/items` walk.
    pub(crate) fn fetch_by_ids_with_skips<T: DeserializeOwned + Send>(
        &self,
        endpoint: &str,
        ids: &[serde_json::Value],
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<(Vec<T>, Vec<serde_json::Value>), ApiError> {
        let batches: Vec<&[serde_json::Value]> = ids.chunks(MAX_BULK_IDS).collect();
        let total = ids.len();
        let mut results = Vec::with_capacity(total);
        let mut skipped = Vec::new();

        for group in batches.chunks(5) {
            // The /v2/items dump is ~500 batches; check between groups so a
            // cancelled download stops within one group instead of running the
            // whole ladder out.
            if self.is_cancelled() {
                return Err(ApiError::Cancelled);
            }
            #[allow(clippy::type_complexity)]
            let group_results: Vec<Result<(Vec<T>, Vec<serde_json::Value>), ApiError>> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = group
                        .iter()
                        .map(|chunk| s.spawn(|| self.fetch_bulk_chunk(endpoint, chunk)))
                        .collect();

                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or_else(|_| {
                                Err(ApiError::Internal(format!(
                                    "Batch fetch thread panicked on {}",
                                    endpoint
                                )))
                            })
                        })
                        .collect()
                });

            for batch_result in group_results {
                let (items, skips) = batch_result?;
                results.extend(items);
                skipped.extend(skips);
            }

            on_progress(results.len(), total);
        }

        Ok((results, skipped))
    }

    /// Get the current game build number (used for cache invalidation).
    pub fn get_build_number(&self) -> Result<u32, ApiError> {
        let info: BuildInfo = self.get("build")?;
        Ok(info.id)
    }

    /// Fetch raw bytes from any URL. Skips the GW2 API token bucket — used for
    /// `render.guildwars2.com` icons, which are a CDN, not the game API.
    ///
    /// The body is capped at `MAX_ICON_BYTES`. A response over the cap is an error
    /// rather than a truncated buffer: `download_missing` writes what it gets to
    /// disk, and a half-PNG would be cached as a permanent bad icon.
    ///
    /// `Content-Length` over the cap is refused from the headers, before any
    /// body byte is read. Streaming 4 MiB+ just to hit `read_body_capped` is
    /// how a mid-body reset became `Internal("icon read failed: request or
    /// response body error")` instead of the cap — the transport broke the
    /// stream before `take(max+1)` could return `InvalidData` (W268).
    ///
    /// A connection-level failure gets one immediate redial (`ICON_ATTEMPTS`),
    /// because a refresh fetches ~10k icons and a single dropped socket should
    /// not cost the user one. An HTTP error status is *not* retried: that is
    /// the CDN answering, and hammering it would not change the answer.
    pub fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        let mut last_error: Option<ApiError> = None;
        for _ in 0..ICON_ATTEMPTS {
            if self.is_cancelled() {
                return Err(ApiError::Cancelled);
            }
            let resp = match self.http.get(url).send() {
                Ok(resp) => resp,
                Err(e) => {
                    last_error = Some(ApiError::Http(e));
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                return Err(ApiError::Api {
                    status: status.as_u16(),
                    url_path: url.chars().take(80).collect(),
                    body_snippet: String::new(),
                });
            }
            if resp.content_length().is_some_and(|n| n > MAX_ICON_BYTES) {
                return Err(ApiError::Api {
                    status: status.as_u16(),
                    url_path: url.chars().take(80).collect(),
                    body_snippet: format!("icon body exceeds {} bytes", MAX_ICON_BYTES),
                });
            }
            let bytes = match crate::transport::read_body_capped(resp, MAX_ICON_BYTES) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(ApiError::Api {
                        status: status.as_u16(),
                        url_path: url.chars().take(80).collect(),
                        body_snippet: format!("icon body exceeds {} bytes", MAX_ICON_BYTES),
                    });
                }
                Err(e) => {
                    last_error = Some(ApiError::Internal(format!("icon read failed: {e}")));
                    continue;
                }
            };
            return Ok(bytes);
        }
        Err(last_error.unwrap_or_else(|| {
            ApiError::Internal(format!("icon unavailable after {} attempts", ICON_ATTEMPTS))
        }))
    }

    /// Requires an authenticated client.
    pub fn fetch_characters(&self) -> Result<Vec<String>, ApiError> {
        self.get("characters")
    }

    pub fn fetch_build_tabs(
        &self,
        character: &str,
    ) -> Result<Vec<super::models::BuildTab>, ApiError> {
        let endpoint = format!("characters/{}/buildtabs", urlencoding::encode(character));
        self.get_with_params(&endpoint, &[("tabs", "all")])
    }

    pub fn fetch_equipment_tabs(
        &self,
        character: &str,
    ) -> Result<Vec<super::models::EquipmentTab>, ApiError> {
        let endpoint = format!(
            "characters/{}/equipmenttabs",
            urlencoding::encode(character)
        );
        self.get_with_params(&endpoint, &[("tabs", "all")])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_bulk_ids_query_empty_slice() {
        // Preserves prior behavior: empty Vec joined with "," → empty string.
        assert_eq!(build_bulk_ids_query(&[] as &[u32]), "");
    }

    #[test]
    fn apply_lang_query_appends_when_absent() {
        let mut url = "https://api.guildwars2.com/v2/skills".to_string();
        apply_lang_query(&mut url, &[], Some("fr"));
        assert_eq!(url, "https://api.guildwars2.com/v2/skills?lang=fr");
        let mut url = "https://api.guildwars2.com/v2/skills?ids=1".to_string();
        apply_lang_query(&mut url, &[("ids", "1")], Some("fr"));
        assert_eq!(url, "https://api.guildwars2.com/v2/skills?ids=1&lang=fr");
    }

    #[test]
    fn apply_lang_query_skips_if_param_present() {
        let mut url = "https://api.guildwars2.com/v2/skills?ids=1".to_string();
        apply_lang_query(&mut url, &[("ids", "1"), ("lang", "es")], Some("fr"));
        assert_eq!(url, "https://api.guildwars2.com/v2/skills?ids=1");
    }

    #[test]
    fn absolute_endpoint_allows_loopback_and_relative_paths() {
        assert!(reject_non_loopback_absolute("http://127.0.0.1/x").is_ok());
        assert!(reject_non_loopback_absolute("http://127.0.0.1:9/x").is_ok());
        assert!(reject_non_loopback_absolute("https://localhost/x").is_ok());
        assert!(reject_non_loopback_absolute("http://[::1]/x").is_ok());
        assert!(reject_non_loopback_absolute("items").is_ok());
        assert!(reject_non_loopback_absolute("characters/Foo%20Bar/core").is_ok());
    }

    #[test]
    fn absolute_endpoint_rejects_dotted_suffix_and_userinfo() {
        let client = Gw2Client::without_key().unwrap();
        for bad in [
            "http://127.0.0.1.evil.com/x",
            "http://127.0.0.1@evil.com/x",
            "http://localhost.evil.com/x",
        ] {
            let err = client
                .get_with_params::<serde_json::Value>(bad, &[])
                .expect_err(bad);
            assert!(
                matches!(err, ApiError::InvalidEndpoint(ref e) if e == bad),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn build_bulk_ids_query_single_id() {
        assert_eq!(build_bulk_ids_query(&[42]), "42");
    }

    #[test]
    fn build_bulk_ids_query_exactly_200_ids() {
        // GW2 API max per bulk request. The helper itself does NOT enforce this
        // cap (chunking is upstream via `ids.chunks(MAX_BULK_IDS)`); it must
        // faithfully serialize whatever it's given.
        let ids: Vec<u32> = (1..=200).collect();
        let out = build_bulk_ids_query(&ids);
        let parts: Vec<&str> = out.split(',').collect();
        assert_eq!(parts.len(), 200);
        assert_eq!(parts[0], "1");
        assert_eq!(parts[199], "200");
        // Exactly 199 separators, no trailing comma.
        assert_eq!(out.matches(',').count(), 199);
        assert!(!out.starts_with(','));
        assert!(!out.ends_with(','));
    }

    #[test]
    fn build_bulk_ids_query_201_ids_passes_through() {
        // One over the GW2 API cap. The helper deliberately does NOT split or
        // error — chunking is the caller's responsibility (current call sites
        // chunk via `ids.chunks(MAX_BULK_IDS)` BEFORE invoking the helper).
        let ids: Vec<u32> = (1..=201).collect();
        let out = build_bulk_ids_query(&ids);
        assert_eq!(out.split(',').count(), 201);
        assert!(out.ends_with(",201"));
    }

    #[test]
    fn build_bulk_ids_query_u32_max() {
        assert_eq!(build_bulk_ids_query(&[u32::MAX]), "4294967295");
    }

    #[test]
    fn build_bulk_ids_query_matches_legacy_format_for_numeric_values() {
        // Bit-for-bit equivalence check against the previous inline construction
        // for the JSON-number IDs that real GW2 endpoints return.
        let values: Vec<serde_json::Value> =
            vec![1u32.into(), 42u32.into(), 100u32.into(), u32::MAX.into()];
        let legacy: String = values
            .iter()
            .map(|id| id.to_string().replace('"', ""))
            .collect::<Vec<_>>()
            .join(",");
        let numeric: Vec<u32> = values.iter().filter_map(value_to_u32).collect();
        assert_eq!(build_bulk_ids_query(&numeric), legacy);
    }

    #[test]
    fn value_to_u32_accepts_numbers_and_numeric_strings() {
        assert_eq!(value_to_u32(&serde_json::json!(42)), Some(42));
        assert_eq!(value_to_u32(&serde_json::json!("42")), Some(42));
        assert_eq!(value_to_u32(&serde_json::json!(u32::MAX)), Some(u32::MAX));
        assert_eq!(
            value_to_u32(&serde_json::json!((u32::MAX as u64) + 1)),
            None
        );
        assert_eq!(value_to_u32(&serde_json::json!(null)), None);
        assert_eq!(value_to_u32(&serde_json::json!("not-a-number")), None);
    }

    #[test]
    fn test_token_bucket_starts_full() {
        let mut bucket = TokenBucket::new();
        // Should not need to sleep when bucket is full
        let wait = bucket.take();
        assert!(wait.is_none(), "Full bucket should not require sleeping");
    }

    #[test]
    fn test_token_bucket_preserves_fractional_on_empty() {
        // Regression: previously `take()` set `tokens = 0.0` on the empty
        // branch, which discarded any fractional progress and forced the
        // caller to wait an extra (1.0 - tokens) / rate seconds before any
        // token was available. Verify that calling `take()` twice without
        // sleeping does NOT erase the bucket — the second call should see
        // the small elapsed time accumulate on top of the prior fractional
        // total, not on top of zero.
        let mut bucket = TokenBucket::new();
        // Drain the bucket
        while bucket.take().is_none() {}
        // Force the bucket into a deliberate fractional state by walking
        // back its `last_refill` clock so the next take() sees ~0.1s of
        // elapsed time, which at REFILL_RATE produces a fractional token.
        bucket.last_refill -= Duration::from_millis(100);
        let _first = bucket.take(); // accumulates fractional tokens
        let tokens_after_first = bucket.tokens;
        assert!(
            tokens_after_first > 0.0,
            "after a sub-token wait, take() should LEAVE the fractional progress in the bucket (was {})",
            tokens_after_first,
        );
        // Calling take() again with negligible elapsed time should keep the
        // fractional balance approximately the same (modulo a tiny refill).
        let _second = bucket.take();
        assert!(
            bucket.tokens >= tokens_after_first - 0.01,
            "consecutive take() calls must not zero out fractional tokens (before={}, after={})",
            tokens_after_first,
            bucket.tokens,
        );
    }

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn parse_retry_after_missing_header_is_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        assert_eq!(
            parse_retry_after(&headers_with_retry_after("10")),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn parse_retry_after_zero_is_zero_duration() {
        assert_eq!(
            parse_retry_after(&headers_with_retry_after("0")),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn parse_retry_after_tolerates_whitespace() {
        assert_eq!(
            parse_retry_after(&headers_with_retry_after("  7  ")),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn parse_retry_after_rejects_http_date() {
        // HTTP-date form is valid per RFC 7231 but unsupported here.
        assert_eq!(
            parse_retry_after(&headers_with_retry_after("Wed, 21 Oct 2025 07:28:00 GMT")),
            None
        );
    }

    #[test]
    fn parse_retry_after_rejects_negative() {
        assert_eq!(parse_retry_after(&headers_with_retry_after("-1")), None);
    }

    #[test]
    fn parse_retry_after_rejects_garbage() {
        assert_eq!(parse_retry_after(&headers_with_retry_after("abc")), None);
    }

    #[test]
    fn body_snippet_truncates_long_text_utf8_safely() {
        // 300 four-byte chars × "💀" — naive byte slice at 200 would panic; chars().take must cap.
        let input: String = std::iter::repeat_n('💀', 300).collect();
        let out = body_snippet(&input);
        assert_eq!(out.chars().count(), 200);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn body_snippet_strips_html_payloads() {
        // Intermediary HTML error pages are noise — keep them out of ApiError.
        let html = "<html><body>Gateway timeout</body></html>";
        assert_eq!(body_snippet(html), "");
    }

    #[test]
    fn body_snippet_preserves_short_plain_text() {
        assert_eq!(body_snippet("not found"), "not found");
        assert_eq!(body_snippet(""), "");
    }

    fn api_500() -> ApiError {
        ApiError::Api {
            status: 500,
            url_path: "items".into(),
            body_snippet: String::new(),
        }
    }

    #[test]
    fn retryable_status_includes_gw2_item_dump_500() {
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(429));
    }

    #[test]
    fn merge_bulk_fetch_splits_500_and_skips_rotten_id() {
        let ids = vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
        ];
        let mut fetch = |part: &[serde_json::Value]| -> Result<Vec<u32>, ApiError> {
            if part.len() > 1 {
                return Err(api_500());
            }
            let id = part[0].as_u64().unwrap() as u32;
            if id == 2 {
                return Err(api_500());
            }
            Ok(vec![id])
        };
        let (got, skipped) = merge_bulk_fetch(&ids, &mut fetch).unwrap();
        assert_eq!(got, vec![1, 3]);
        assert_eq!(skipped, vec![serde_json::json!(2)]);
    }

    #[test]
    fn merge_bulk_fetch_singleton_5xx_is_not_ok_empty() {
        let ids = vec![serde_json::json!(2)];
        let mut fetch =
            |_part: &[serde_json::Value]| -> Result<Vec<u32>, ApiError> { Err(api_500()) };
        let result = merge_bulk_fetch(&ids, &mut fetch);
        assert!(
            !matches!(&result, Ok((items, skipped)) if items.is_empty() && skipped.is_empty()),
            "singleton 5xx must not be Ok([])"
        );
        let (items, skipped) = result.unwrap();
        assert!(items.is_empty());
        assert_eq!(skipped, vec![serde_json::json!(2)]);
    }

    #[test]
    fn merge_bulk_fetch_still_fails_on_client_errors() {
        let ids = vec![serde_json::json!(1), serde_json::json!(2)];
        let mut fetch = |_part: &[serde_json::Value]| -> Result<Vec<u32>, ApiError> {
            Err(ApiError::Internal("nope".into()))
        };
        assert!(matches!(
            merge_bulk_fetch(&ids, &mut fetch),
            Err(ApiError::Internal(_))
        ));
    }

    /// mockito binds its listener on a background task and starts pumping the
    /// accept loop a beat later; under load the first real requests can be
    /// connection-reset before the server task is scheduled. Probe an
    /// UNMOCKED path until the server answers with any HTTP response — a
    /// plain TCP connect is not enough (the kernel accepts before the task
    /// pumps, and the late connection gets reset).
    fn wait_for_server_ready(url: &str) {
        let probe = format!("{}/__server_ready", url.trim_end_matches('/'));
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("probe client builds");
        for _ in 0..250 {
            match client.get(&probe).send() {
                Ok(_) => return, // any HTTP answer means the task is pumping
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        panic!("mock server at {url} never became ready");
    }

    /// A ready mock server plus a client that will not reuse pooled sockets.
    ///
    /// Measured cause of the historical ~30-50% flakiness in this module, and
    /// it is neither the triaged "parallel compile load" race nor a mockito
    /// path collision — a single test in isolation fails just as often:
    ///
    /// * ~5-12% of loopback connections on this Windows host die with
    ///   WSAECONNRESET (10054) on the request write. Measured against a
    ///   hand-rolled `std::net::TcpListener` HTTP server as well as mockito, so
    ///   it is the host, not the mock library and not this client.
    /// * mockito counts a hit for every one of those requests (measured:
    ///   150 requests, 128 client-visible successes, 150 mock hits), because
    ///   the reset lands after the mock matched.
    /// * the client then (correctly) retries the connection error.
    ///
    /// So `expect(1)` + `assert()` cannot hold on this host no matter what the
    /// client does. These tests therefore assert what a retry cannot fake — the
    /// value returned, and which mock answered — with `expect_at_least`, while
    /// the retry *policy* itself is unit-tested through `rate_limit_action`,
    /// which needs no socket at all.
    ///
    /// Pooling is off because a stale-socket reuse is a second, avoidable
    /// source of the same error (measured ~8-12% pooled vs ~5-7% unpooled).
    /// Production keeps the pool: a refresh makes ~500 requests.
    ///
    /// Every mockito test also owns a UNIQUE path, so a recycled pooled server
    /// can never serve one test's request from another test's mock.
    fn mock_server() -> (mockito::ServerGuard, Gw2Client) {
        let server = mockito::Server::new();
        wait_for_server_ready(&server.url());
        let http = Client::builder()
            // Production waits 30 s for the real API. A mock on loopback that
            // has not answered in 5 s is this host stalling a connection, and
            // an in-flight blocking read is the one wait cancellation cannot
            // interrupt — keep it short so it never dominates a timing bound.
            .timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .build()
            .expect("test client builds");
        let client = Gw2Client {
            http,
            api_key: None,
            bucket: Mutex::new(TokenBucket::new()),
            lang: None,
            api_root: Some(server.url()),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        (server, client)
    }

    /// Re-issue a call that died on the host's transport, and only that.
    ///
    /// `Gw2Client` already retries connection errors `MAX_RETRIES` times, but
    /// the losses documented on `mock_server` arrive in bursts that can outrun
    /// the whole ladder (observed: one run in twenty ended with
    /// `Http(ConnectionReset)` after every attempt). Every other error —
    /// `Api`, `RateLimited`, `Cancelled`, a parse failure — is returned
    /// untouched, so this never hides a client defect; it only declines to
    /// assert on this machine's loopback stack.
    fn transport_retry<T>(mut call: impl FnMut() -> Result<T, ApiError>) -> Result<T, ApiError> {
        for _ in 0..2 {
            match call() {
                Err(ApiError::Http(_)) => continue,
                other => return other,
            }
        }
        call()
    }

    /// The retry *decision* — this is the invariant a socket cannot prove
    /// (see `mock_server`), so it is tested where it lives: a pure function.
    #[test]
    fn rate_limit_action_honors_the_cap() {
        assert_eq!(
            rate_limit_action(&headers_with_retry_after("1")),
            RateLimitAction::Wait(Duration::from_secs(1))
        );
        // Exactly at the cap is still honored; one second past it is not.
        assert_eq!(
            rate_limit_action(&headers_with_retry_after("30")),
            RateLimitAction::Wait(RETRY_AFTER_CAP)
        );
        assert_eq!(
            rate_limit_action(&headers_with_retry_after("31")),
            RateLimitAction::GiveUp
        );
        assert_eq!(
            rate_limit_action(&headers_with_retry_after("3600")),
            RateLimitAction::GiveUp
        );
        // No header, an HTTP-date, or garbage: exponential backoff, never a
        // parse that lands on a huge wait.
        assert_eq!(
            rate_limit_action(&HeaderMap::new()),
            RateLimitAction::Backoff
        );
        assert_eq!(
            rate_limit_action(&headers_with_retry_after("Wed, 21 Oct 2015 07:28:00 GMT")),
            RateLimitAction::Backoff
        );
        assert_eq!(
            rate_limit_action(&headers_with_retry_after("-5")),
            RateLimitAction::Backoff
        );
    }

    #[test]
    fn get_with_params_429_then_200_succeeds_with_retry_after() {
        // Mock server: first GET returns 429 + Retry-After: 1, second returns 200.
        // Asserted: the call resolves to the 200 body, i.e. a 429 does not
        // abort the request. Hit counts are `expect_at_least` on purpose — see
        // `mock_server` for why exact counts are not decidable on this host.
        let (mut server, client) = mock_server();
        let m1 = server
            .mock("GET", "/retry-after")
            .with_status(429)
            .with_header("retry-after", "1")
            .with_body("rate limited")
            .expect_at_least(1)
            .create();
        let m2 = server
            .mock("GET", "/retry-after")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("{\"ok\":true}")
            .expect_at_least(1)
            .create();

        let url = format!("{}/retry-after", server.url());
        let resp: serde_json::Value =
            transport_retry(|| client.get_with_params(&url, &[])).unwrap();
        assert_eq!(resp["ok"], true);
        m1.assert();
        m2.assert();
    }

    #[test]
    fn get_with_params_429_over_cap_short_circuits_to_rate_limited() {
        let (mut server, client) = mock_server();
        let _m = server
            .mock("GET", "/retry-after-over-cap")
            .with_status(429)
            .with_header("retry-after", "3600") // 1h, well over RETRY_AFTER_CAP
            .with_body("rate limited")
            .expect_at_least(1)
            .create();

        let url = format!("{}/retry-after-over-cap", server.url());
        let started = Instant::now();
        let err = transport_retry(|| client.get_with_params::<serde_json::Value>(&url, &[]))
            .expect_err("an over-cap Retry-After must not resolve");
        let elapsed = started.elapsed();
        match err {
            ApiError::RateLimited { url_path, .. } => assert_eq!(url_path, url),
            other => panic!("expected RateLimited, got {:?}", other),
        }
        // The point of the cap: the 1-hour wait was never honored. (Whether
        // exactly one request was issued is `rate_limit_action`'s job — a
        // dropped connection here would add one, and that is the host, not
        // the policy.)
        assert!(
            elapsed < Duration::from_secs(60),
            "waited {elapsed:?} — the over-cap Retry-After was honored"
        );
    }

    /// Every mockito test in this module owns a unique path. Guard that this
    /// actually isolates them: a retry ladder on one path must never be
    /// answered by a neighbouring mock, not even after the neighbour's own
    /// expectations are exhausted (mockito falls back to the *last* matching
    /// mock once every match is used up, so a shared path silently
    /// cross-serves).
    #[test]
    fn mockito_unique_paths() {
        let (mut server, client) = mock_server();
        // Path A retries (429 + Retry-After) before succeeding.
        let a_429 = server
            .mock("GET", "/unique-a")
            .with_status(429)
            .with_header("retry-after", "1")
            .with_body("rate limited")
            .expect_at_least(1)
            .create();
        let a_ok = server
            .mock("GET", "/unique-a")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("{\"who\":\"a\"}")
            .expect_at_least(1)
            .create();
        // Path B is a neighbour on the same server, registered LAST — it is
        // exactly the mock mockito would fall back to if A's requests were
        // allowed to match it.
        let b_ok = server
            .mock("GET", "/unique-b")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("{\"who\":\"b\"}")
            .expect_at_least(1)
            .create();

        // Drive A twice: the second call finds every A mock exhausted, which
        // is the state in which a shared path would leak into B.
        let a_url = format!("{}/unique-a", server.url());
        for _ in 0..2 {
            let a: serde_json::Value = transport_retry(|| client.get_with_params(&a_url, &[]))
                .expect("path A resolves through its own retry");
            assert_eq!(a["who"], "a", "path A was answered by another mock");
        }

        let b_url = format!("{}/unique-b", server.url());
        let b: serde_json::Value =
            transport_retry(|| client.get_with_params(&b_url, &[])).expect("path B resolves");
        assert_eq!(b["who"], "b", "path B was answered by another mock");

        a_429.assert();
        a_ok.assert();
        b_ok.assert();
    }

    /// The invariant is the *query string*: commas must survive un-encoded and
    /// string IDs must not be dropped. `match_query(Exact)` proves it — a
    /// wrong query gets 501 and the call fails — so the hit count carries no
    /// extra information and is not asserted exactly (see `mock_server`).
    #[test]
    fn fetch_by_ids_preserves_string_ids_for_legend_endpoints() {
        let (mut server, client) = mock_server();
        let m = server
            .mock("GET", "/legends")
            .match_query(mockito::Matcher::Exact("ids=Legend1,Legend2".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"id":"Legend1","swap":28085,"heal":27220,"elite":27760,"utilities":[28379,27014,26644]},
                    {"id":"Legend2","swap":28134,"heal":26937,"elite":28406,"utilities":[29209,28231,27107]}
                ]"#,
            )
            .expect_at_least(1)
            .create();

        let ids = vec![serde_json::json!("Legend1"), serde_json::json!("Legend2")];
        let endpoint = format!("{}/legends", server.url());
        let legends: Vec<super::super::models::Legend> =
            transport_retry(|| client.fetch_by_ids(&endpoint, &ids)).unwrap();

        assert_eq!(legends.len(), 2);
        assert_eq!(legends[0].id, "Legend1");
        assert_eq!(legends[1].id, "Legend2");
        m.assert();
    }

    /// The retry sleep is the finding: 4 backoffs honoring a 30 s `Retry-After`
    /// used to be 120 s of uninterruptible `thread::sleep` per call. Cancel
    /// mid-sleep and the call must return long before the wait would end.
    #[test]
    fn retry_sleep_observes_cancel() {
        let (mut server, client) = mock_server();
        let _m = server
            .mock("GET", "/cancel-mid-sleep")
            .with_status(429)
            .with_header("retry-after", "25") // under RETRY_AFTER_CAP, so it sleeps
            .with_body("rate limited")
            .expect_at_least(1)
            .create();

        let url = format!("{}/cancel-mid-sleep", server.url());
        let started = Instant::now();
        let err = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                client.cancel();
            });
            client
                .get_with_params::<serde_json::Value>(&url, &[])
                .unwrap_err()
        });
        let elapsed = started.elapsed();

        assert!(
            matches!(err, ApiError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        // 25 s of Retry-After sleep would blow this; a stalled request cannot,
        // because `mock_server`'s client times out after 5 s and the loop then
        // sees the cancel at the top of the next attempt.
        assert!(
            elapsed < Duration::from_secs(15),
            "cancel took {elapsed:?} — the 25 s Retry-After sleep was not interrupted"
        );
    }

    #[test]
    fn get_with_params_returns_cancelled_before_the_first_request() {
        let (mut server, client) = mock_server();
        let m = server
            .mock("GET", "/never-requested")
            .with_status(200)
            .with_body("{}")
            .expect(0)
            .create();

        client.cancel();
        let err = client
            .get_with_params::<serde_json::Value>(&format!("{}/never-requested", server.url()), &[])
            .unwrap_err();
        assert!(matches!(err, ApiError::Cancelled), "got {err:?}");
        m.assert();
    }

    /// Mockito sets Content-Length from the body. `fetch_bytes` must refuse
    /// from that header — streaming 4 MiB+ is what raced into Internal
    /// ("icon read failed: request or response body error") as W268.
    #[test]
    fn fetch_bytes_rejects_a_body_over_the_icon_cap() {
        let (mut server, client) = mock_server();
        let oversized = vec![b'x'; MAX_ICON_BYTES as usize + 1024];
        let _m = server
            .mock("GET", "/huge-icon.png")
            .with_status(200)
            .with_body(oversized)
            .create();

        let url = format!("{}/huge-icon.png", server.url());
        let err = transport_retry(|| client.fetch_bytes(&url))
            .expect_err("an oversized icon must not be accepted");
        match err {
            ApiError::Api { body_snippet, .. } => {
                assert!(body_snippet.contains("exceeds"), "got {body_snippet:?}");
            }
            other => panic!("expected an Api error for the oversized body, got {other:?}"),
        }
    }

    /// The watchdog is the only path by which a cancel raised *while* a request
    /// is in flight reaches the blocked client. Prove it mirrors.
    #[test]
    fn cancel_bridge_arms_the_client_mid_flight() {
        let client = Gw2Client::without_key().unwrap();
        let token = Arc::new(AtomicBool::new(false));
        let watched = Arc::clone(&token);
        let cancelled = move || watched.load(Ordering::Relaxed);

        let mirrored = with_cancel_bridge(&client, &cancelled, || {
            assert!(!client.is_cancelled(), "client starts un-cancelled");
            token.store(true, Ordering::Relaxed); // caller cancels mid-flight
            for _ in 0..200 {
                if client.is_cancelled() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        });

        assert!(
            mirrored,
            "watchdog never mirrored the caller's cancel into the client"
        );
    }

    /// A live caller must not be cancelled by the bridge, and the watchdog must
    /// be joined (scoped) rather than left running past the call.
    #[test]
    fn cancel_bridge_leaves_a_live_client_alone() {
        let client = Gw2Client::without_key().unwrap();
        let cancelled = || false;
        let out = with_cancel_bridge(&client, &cancelled, || 7);
        assert_eq!(out, 7);
        assert!(!client.is_cancelled());
    }

    /// A panic inside the body must unwind out of the scope, not hang against a
    /// watchdog that was never told the body finished.
    #[test]
    fn cancel_bridge_survives_a_panicking_body() {
        let client = Gw2Client::without_key().unwrap();
        let cancelled = || false;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_cancel_bridge(&client, &cancelled, || panic!("progress callback blew up"));
        }));
        assert!(
            caught.is_err(),
            "the panic must propagate, not be swallowed"
        );
    }

    #[test]
    fn fetch_bytes_passes_through_a_normal_icon() {
        let (mut server, client) = mock_server();
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let _m = server
            .mock("GET", "/icon.png")
            .with_status(200)
            .with_body(png)
            .create();

        let url = format!("{}/icon.png", server.url());
        let bytes = transport_retry(|| client.fetch_bytes(&url)).expect("icon under the cap");
        assert_eq!(bytes, png);
    }

    #[test]
    #[ignore] // Requires network
    fn test_live_fetch_legends_all() {
        let client = Gw2Client::without_key().unwrap();
        let legends: Vec<super::super::models::Legend> = client.fetch_all("legends").unwrap();
        assert!(legends.len() >= 7, "expected current revenant legends");
        assert!(legends.iter().all(|l| l.id.starts_with("Legend")));
    }

    #[test]
    #[ignore] // Requires network
    fn test_live_fetch_build_number() {
        let client = Gw2Client::without_key().unwrap();
        let build = client.get_build_number().unwrap();
        assert!(build > 100000); // Build numbers are large
    }

    #[test]
    #[ignore] // Requires network
    fn test_live_fetch_berserkers_itemstat() {
        let client = Gw2Client::without_key().unwrap();
        let stats: Vec<super::super::models::ItemStat> = client
            .get_with_params("itemstats", &[("ids", "584")])
            .unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "Berserker's");
    }

    #[test]
    #[ignore] // Requires network
    fn test_live_fetch_pvp_amulets_all() {
        // Shape smoke-test for the PvP amulet endpoint. Fetches every amulet
        // in one bulk request and asserts the list deserializes and at least
        // one amulet exposes stat attributes — catches drift in a low-traffic
        // endpoint that ItemStat tests do not cover.
        let client = Gw2Client::without_key().unwrap();
        let amulets: Vec<super::super::models::PvpAmulet> = client
            .get_with_params("pvp/amulets", &[("ids", "all")])
            .unwrap();
        assert!(!amulets.is_empty(), "expected non-empty amulet list");
        assert!(
            amulets.iter().any(|a| !a.attributes.is_empty()),
            "expected at least one amulet with attributes populated"
        );
    }

    #[test]
    #[ignore] // Requires network
    fn test_live_fetch_legendary_dual_stat_weapon() {
        // Sunrise (id=30704) — canonical legendary greatsword from launch.
        // Legendary weapons expose `stat_choices` in place of a fixed
        // `infix_upgrade`; this guards the shape optimizer gear resolution
        // relies on.
        let client = Gw2Client::without_key().unwrap();
        let items: Vec<super::super::models::Item> = client
            .get_with_params("items", &[("ids", "30704")])
            .unwrap();
        assert_eq!(items.len(), 1);
        let sunrise = &items[0];
        assert_eq!(sunrise.rarity, "Legendary");
        assert_eq!(sunrise.item_type, "Weapon");
        let details = sunrise
            .details
            .as_ref()
            .expect("legendary weapon must carry details");
        assert!(
            !details.stat_choices.is_empty(),
            "legendary weapon must expose stat_choices"
        );
    }

    #[test]
    #[ignore] // Requires network
    fn test_live_fetch_relic() {
        // SotO-era relics replaced the 7th rune bonus. 100947 is Relic of the
        // Thief. If Anet ever retires this exact ID, replace it — the test
        // still earns its keep by guarding the `Relic` item_type variant,
        // which only showed up in the API post-2023.
        let client = Gw2Client::without_key().unwrap();
        let items: Vec<super::super::models::Item> = client
            .get_with_params("items", &[("ids", "100947")])
            .unwrap();
        assert_eq!(items.len(), 1);
        let relic = &items[0];
        assert_eq!(relic.item_type, "Relic");
        assert!(!relic.name.is_empty());
    }
}
