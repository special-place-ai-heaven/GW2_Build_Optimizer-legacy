//! Station logos: favicon download + Nexus texture, letter plate while
//! loading. Modeled on `news_art` (slots map, blocking worker on
//! `spawn_worker`, tmp+rename disk cache, lazy texture creation on the render
//! thread) but tuned for favicon reality: arbitrary hosts (no allowlist),
//! plain http allowed (~15% of directory favicons; the streams themselves
//! are http too), tiny files.
//!
//! # OOM contract (non-negotiable)
//!
//! Nexus never frees textures until game exit (nexus-rs issue #138), so a
//! long session must not upload logos without bound:
//!
//! * At most [`MAX_TEXTURES`] **distinct** logo textures per session. Past
//!   the budget, new stations keep their letter plates — nothing is evicted
//!   because nothing *can* be evicted.
//! * Failed URLs go into the session failure set ([`Slot::Failed`]) and are
//!   never retried, so a dead favicon host cannot be re-dialed every scroll.
//! * The disk cache is capped at [`MAX_CACHE_FILES`] files: the worker
//!   evicts oldest-by-mtime after every write, so months of browsing cannot
//!   grow `cache/radio_logos/` forever.
//! * Download bodies are capped at [`MAX_BYTES`]; one worker at a time.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use nexus::imgui::TextureId;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::state::{AddonState, CancellationToken};

const TIMEOUT: Duration = Duration::from_secs(8);
/// Body cap. Measured directory favicons: median ~6 KB, largest seen ~205 KB.
const MAX_BYTES: usize = 256 * 1024;
/// Session texture budget — see the module-level OOM contract.
const MAX_TEXTURES: usize = 200;
/// Disk cap for `cache/radio_logos/`; oldest by mtime evicted on write.
const MAX_CACHE_FILES: usize = 500;

enum Slot {
    /// Queued or in flight on the worker.
    Pending,
    /// Cached on disk; the texture is created lazily on the render thread.
    Ready { path: PathBuf },
    /// Session-permanent: never retried (see the OOM contract).
    Failed,
}

static SLOTS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
/// URLs awaiting the next worker pass (already `Pending` in `SLOTS`).
static QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Texture ids whose creation was kicked — its len IS the session budget.
static CREATED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
/// Single-flight: one "radio-logo" worker at a time.
static FETCHING: AtomicBool = AtomicBool::new(false);

fn slots() -> std::sync::MutexGuard<'static, HashMap<String, Slot>> {
    SLOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn queue() -> std::sync::MutexGuard<'static, Vec<String>> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner())
}

fn created() -> std::sync::MutexGuard<'static, HashSet<String>> {
    CREATED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Syntactic screen for a favicon URL: http or https on any public host.
/// Unlike news stills there is no host allowlist — favicon hosts are
/// arbitrary — and plain http is allowed. "localhost" and reserved-range
/// literal IPs stay rejected (`news_art::reserved_still_host`).
fn url_ok(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    if u.scheme() != "https" && u.scheme() != "http" {
        return false;
    }
    let Some(host) = crate::news_art::normalized_host(&u) else {
        return false;
    };
    !crate::news_art::reserved_still_host(host)
}

/// DNS-level check on top of `url_ok`: with no host allowlist, a favicon
/// hostname must not *resolve* into the local network either (same rule the
/// stream connect enforces in `player`). Reserved literal IPs are also rejected;
/// an unresolvable host passes — the GET then fails with its
/// own honest error. Worker thread only: this can block on a resolve.
fn host_resolves_reserved(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return true;
    };
    crate::news_art::url_host_is_reserved(&u)
}

/// Full per-hop screen: syntax plus DNS. Used for the original URL and every
/// redirect target, so a favicon cannot bounce the client into the LAN.
/// Radio streams reuse this same predicate (SEC-RADIO).
pub(crate) fn hop_ok(url: &str) -> bool {
    url_ok(url) && !host_resolves_reserved(url)
}

fn hash16(url: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    h.finish()
}

fn cache_id(url: &str) -> String {
    format!("GW2BOradiologo{:016x}", hash16(url))
}

fn file_stem(url: &str) -> String {
    format!("{:016x}", hash16(url))
}

/// png/jpeg via `news_art::sniff`, plus gif — exactly the formats the legacy
/// stb_image loader behind Nexus decodes. .ico/.webp/.svg fail the sniff and
/// keep the letter plate.
fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if let Some(ext) = crate::news_art::sniff(bytes) {
        return Some(ext);
    }
    (bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a")).then_some("gif")
}

// Render-thread API

/// Render-thread lookup: the logo texture if it is live or creatable within
/// budget, `None` (letter plate) otherwise. First sighting of a URL enqueues
/// it for the worker — that dedupe-guarded request is the only download work
/// the render loop ever kicks, and the happy path allocates only the
/// texture-id string.
pub fn texture(url: &str) -> Option<TextureId> {
    if url.is_empty() {
        return None;
    }
    let id = cache_id(url);
    let live = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nexus::texture::get_texture(&id)
    }))
    .ok()
    .flatten();
    if let Some(tex) = live {
        return Some(tex.id());
    }
    let path = slot_path_or_enqueue(url)?;
    // Budget gate BEFORE creating: Nexus never frees textures (issue #138),
    // so past the cap this session renders letter plates instead.
    {
        let mut set = created();
        if !crate::cache_bounds::admit_texture(&mut set, &id, MAX_TEXTURES) {
            return None;
        }
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nexus::texture::get_texture_or_create_from_file(&id, &path)
    }))
    .ok()
    .flatten()
    .map(|tex| tex.id())
}

/// Slot bookkeeping, separated from the nexus calls so it is testable:
/// returns the cached path when Ready; on an unseen URL marks it Pending and
/// queues it (or Failed immediately when it cannot ever be fetched).
fn slot_path_or_enqueue(url: &str) -> Option<PathBuf> {
    let mut map = slots();
    match map.get(url) {
        None => {
            let ok = url_ok(url);
            map.insert(
                url.to_string(),
                if ok { Slot::Pending } else { Slot::Failed },
            );
            drop(map);
            if ok {
                queue().push(url.to_string());
            }
            None
        }
        Some(Slot::Pending | Slot::Failed) => None,
        // Disk eviction can delete a Ready file (cache hits do not refresh
        // mtime) — treat Ready-but-missing as a letter plate, like news_art.
        Some(Slot::Ready { path }) if path.exists() => Some(path.clone()),
        Some(Slot::Ready { .. }) => None,
    }
}

/// Start one "radio-logo" worker for everything the rows enqueued since the
/// last pass. Called once per Radio-tab frame; a no-op while idle or while a
/// worker is already in flight (queued URLs simply wait for the next pass).
pub fn kick(state: &AddonState) {
    if FETCHING.load(Ordering::Acquire) {
        return;
    }
    let batch: Vec<String> = std::mem::take(&mut *queue());
    if batch.is_empty() {
        return;
    }
    FETCHING.store(true, Ordering::Release);
    let dir = state.addon_dir.join("cache").join("radio_logos");
    let version = crate::VERSION.to_string();
    let queued = batch.clone();
    let spawned = state.spawn_worker("radio-logo", move |token| {
        let mut guard = WorkerGuard {
            leftover: batch.clone(),
        };
        for url in &batch {
            if token.is_cancelled() {
                return; // guard releases the leftovers
            }
            match download(url, &dir, &token, &version) {
                Some(path) => mark_ready(url, path),
                None => mark_failed(url),
            }
            guard.finish(url);
        }
    });
    if !spawned {
        release_pending(&queued);
        FETCHING.store(false, Ordering::Release);
    }
}

// Worker side

/// Worker drop guard: on any exit (done, cancel, unwind) the not-yet-finished
/// URLs go back to unseen so a later frame can re-request them, and the
/// single-flight flag clears so the next `kick` can spawn again.
struct WorkerGuard {
    leftover: Vec<String>,
}

impl WorkerGuard {
    fn finish(&mut self, url: &str) {
        self.leftover.retain(|u| u != url);
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        release_pending(&self.leftover);
        FETCHING.store(false, Ordering::Release);
    }
}

fn mark_ready(url: &str, path: PathBuf) {
    slots().insert(url.to_string(), Slot::Ready { path });
}

fn mark_failed(url: &str) {
    slots().insert(url.to_string(), Slot::Failed);
}

fn release_pending(urls: &[String]) {
    let mut map = slots();
    for url in urls {
        if matches!(map.get(url), Some(Slot::Pending)) {
            map.remove(url);
        }
    }
}

/// A prior session's cached file (any accepted ext) skips the network — a
/// restart does not re-download every favorite's logo.
// ponytail: cache hits do not refresh mtime, so eviction can reclaim a
// favorite's logo after ~500 newer downloads; it just re-downloads next
// session. Touching mtime needs the filetime crate — add it if this bites.
fn cached_file(dir: &Path, url: &str) -> Option<PathBuf> {
    let stem = file_stem(url);
    ["png", "jpg", "gif"]
        .iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|p| p.exists())
}

fn download(url: &str, dir: &Path, token: &CancellationToken, version: &str) -> Option<PathBuf> {
    if token.is_cancelled() || !hop_ok(url) {
        return None;
    }
    if let Some(path) = cached_file(dir, url) {
        return Some(path);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .redirect(crate::news_art::screened_redirect_policy(2, hop_ok))
        .build()
        .ok()?;
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("GW2BuildOptimizer/{version} radio-logo")) {
        headers.insert(reqwest::header::USER_AGENT, v);
    }
    let resp = client.get(url).headers(headers).send().ok()?;
    if !resp.status().is_success() || !url_ok(resp.url().as_str()) {
        return None;
    }
    let bytes = gw2_api::transport::read_body_capped(resp, MAX_BYTES as u64).ok()?;
    let ext = sniff_ext(&bytes)?;
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{}.{ext}", file_stem(url)));
    let tmp = path.with_extension(format!("{ext}.tmp"));
    std::fs::write(&tmp, &bytes).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    crate::cache_bounds::evict_oldest(dir, MAX_CACHE_FILES);
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that touch the global SLOTS/QUEUE statics.
    static LOGOS_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        LOGOS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn url_ok_allows_http_and_arbitrary_hosts_rejects_reserved() {
        assert!(url_ok("https://example.com/favicon.png"));
        assert!(url_ok("http://stream.example.net/logo.png"));
        assert!(url_ok("https://cdn.some-radio.xyz/x.jpg"));
        assert!(!url_ok("ftp://example.com/x.png"));
        assert!(!url_ok("javascript:alert(1)"));
        assert!(!url_ok(""));
        assert!(!url_ok("http://localhost/x.png"));
        assert!(!url_ok("http://127.0.0.1/x.png"));
        assert!(!url_ok("https://10.0.0.1/x.png"));
        assert!(!url_ok("https://192.168.1.5/x.png"));
        assert!(!url_ok("https://172.16.0.1/x.png"));
        assert!(!url_ok("https://169.254.169.254/latest/meta-data"));
        assert!(!url_ok("https://[::1]/x.png"));
        assert!(!url_ok("https://[fe80::1]/x.png"));
        assert!(!url_ok("https://[fd00::1]/x.png"));
    }

    #[test]
    fn hop_ok_rejects_reserved_redirect_targets() {
        for url in [
            "http://127.0.0.1/x.png",
            "http://10.0.0.1/x.png",
            "http://192.168.1.5/x.png",
            "http://172.16.0.1/x.png",
            "http://169.254.169.254/latest/meta-data",
            "http://[fe80::1]/x.png",
            "https://127.0.0.1/x.png",
        ] {
            assert!(!hop_ok(url), "{url}");
        }
    }

    #[test]
    fn redirect_to_reserved_is_stopped_before_connect() {
        crate::news_art::assert_policy_stops_reserved_redirects(
            crate::news_art::screened_redirect_policy(2, hop_ok),
        );
    }

    #[test]
    fn dns_screen_rejects_reserved_literals_and_garbage() {
        assert!(host_resolves_reserved("http://127.0.0.1/x.png"));
        assert!(host_resolves_reserved("https://[fd00::1]/x.png"));
        assert!(host_resolves_reserved("https://[::ffff:127.0.0.1]/x.png"));
        assert!(!host_resolves_reserved(
            "https://[2606:4700:4700::1111]/x.png"
        ));
        // Unparseable input fails closed.
        assert!(host_resolves_reserved("not a url"));
    }

    #[test]
    fn sniff_accepts_png_jpeg_gif_only() {
        assert_eq!(sniff_ext(b"GIF89a\x01\x00"), Some("gif"));
        assert_eq!(sniff_ext(b"GIF87a\x01\x00"), Some("gif"));
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff_ext(&[0x89, 0x50, 0x4E, 0x47, 0x0D]), Some("png"));
        // .ico, .webp, .svg: letter plate.
        assert_eq!(sniff_ext(&[0x00, 0x00, 0x01, 0x00, 0x01, 0x00]), None);
        assert_eq!(sniff_ext(b"RIFF\x00\x00\x00\x00WEBP"), None);
        assert_eq!(sniff_ext(b"<svg xmlns=\"x\">"), None);
        assert_eq!(sniff_ext(b"<?xml version"), None);
        assert_eq!(sniff_ext(b"GIF"), None);
        assert_eq!(sniff_ext(&[]), None);
    }

    #[test]
    fn first_sight_enqueues_once_and_failed_never_retries() {
        let _l = test_guard();
        let url = "https://logos-test-one.example/favicon.png";
        assert!(slot_path_or_enqueue(url).is_none());
        assert!(
            slot_path_or_enqueue(url).is_none(),
            "second sight must dedupe"
        );
        assert_eq!(std::mem::take(&mut *queue()), vec![url.to_string()]);
        mark_failed(url);
        assert!(slot_path_or_enqueue(url).is_none());
        assert!(queue().is_empty(), "Failed must never re-enqueue");
        // Unfetchable URLs fail immediately without ever queueing.
        let bad = "http://127.0.0.1/favicon.png";
        assert!(slot_path_or_enqueue(bad).is_none());
        assert!(queue().is_empty());
    }

    #[test]
    fn release_pending_allows_requeue_ready_returns_path() {
        let _l = test_guard();
        let url = "https://logos-test-two.example/favicon.png";
        assert!(slot_path_or_enqueue(url).is_none());
        let _ = std::mem::take(&mut *queue());
        release_pending(&[url.to_string()]);
        assert!(slot_path_or_enqueue(url).is_none());
        assert_eq!(
            std::mem::take(&mut *queue()),
            vec![url.to_string()],
            "released Pending must be requestable again"
        );
        let ready = "https://logos-test-three.example/favicon.png";
        // Ready with a real file returns the path; Ready whose file was
        // evicted from the disk cache degrades to the letter plate (None)
        // without re-queueing a download.
        let dir = std::env::temp_dir().join("gw2bo_logo_test");
        let _ = std::fs::create_dir_all(&dir);
        let real = dir.join("logo.png");
        std::fs::write(&real, b"png").expect("test file");
        mark_ready(ready, real.clone());
        assert_eq!(slot_path_or_enqueue(ready), Some(real.clone()));
        let _ = std::fs::remove_file(&real);
        assert_eq!(
            slot_path_or_enqueue(ready),
            None,
            "evicted Ready file must degrade to the letter plate"
        );
        assert!(queue().is_empty());
    }

    #[test]
    fn texture_budget_admits_distinct_ids_up_to_cap() {
        let mut set: HashSet<String> = HashSet::new();
        assert!(crate::cache_bounds::admit_texture(&mut set, "a", 2));
        assert!(crate::cache_bounds::admit_texture(&mut set, "b", 2));
        assert!(
            !crate::cache_bounds::admit_texture(&mut set, "c", 2),
            "over budget: letter plate"
        );
        assert!(
            crate::cache_bounds::admit_texture(&mut set, "a", 2),
            "known id stays creatable"
        );
        assert!(crate::cache_bounds::admit_texture(&mut set, "b", 2));
    }
}
