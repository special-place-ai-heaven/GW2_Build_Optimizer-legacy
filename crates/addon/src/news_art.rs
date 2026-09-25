//! Still images for news (JPEG/PNG). No video, no web pages.
//!
//! Nexus never frees textures until game exit (nexus-rs issue #138). Stills
//! share the logos discipline: a session [`MAX_TEXTURES`] cap before upload,
//! and [`MAX_CACHE_FILES`] on disk with oldest-mtime eviction after each write.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use nexus::imgui::TextureId;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::state::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(8);
/// Download cap for one still. Fails closed, so this is an admission gate,
/// not a hint: at 1_500_000 it was rejecting the official blog's 1.95 MB
/// announcement PNGs outright — the exact images that most needed shrinking
/// never reached the cache at all. 4 MB is ~2x the largest image observed
/// across the five live feeds, and every still over MAX_EDGE is downscaled
/// before it is written, so this bounds the DOWNLOAD, not what we keep.
const MAX_BYTES: usize = 4_000_000;
/// Session texture budget — Nexus never frees these (issue #138).
const MAX_TEXTURES: usize = 200;
/// Disk cap for `cache/news`; oldest by mtime evicted on write.
const MAX_CACHE_FILES: usize = 500;

#[derive(Clone)]
enum Slot {
    Pending,
    Ready { path: PathBuf, aspect: f32 },
    Failed,
}

static SLOTS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
/// Texture ids whose creation was kicked — its len IS the session budget.
static CREATED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn slots() -> std::sync::MutexGuard<'static, HashMap<String, Slot>> {
    SLOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn created() -> std::sync::MutexGuard<'static, HashSet<String>> {
    CREATED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Registrable domains the news feeds serve stills from — matched as "this
/// exact host, or a subdomain of it". Any other https host is a still the
/// overlay must not fetch.
///
/// This is a DOMAIN list, not a host list, because the feeds do not serve
/// stills from the host you fetched the feed from. Verified against the live
/// feeds on 2026-09-04: YouTube round-robins thumbnails over
/// `i1`-`i4.ytimg.com` (never bare `i.ytimg.com`), GuildJen proxies every
/// image through Jetpack Photon at `i0.wp.com`, and the official blog serves
/// from one CloudFront distribution. An exact-host list missed all three and
/// silently blanked every image in the News tab — see the regression test
/// `live_feed_image_hosts_are_allowlisted`.
///
/// The CloudFront entry is one ArenaNet distribution on purpose: plain
/// `cloudfront.net` would allowlist every AWS customer on the internet.
const STILL_DOMAINS: &[&str] = &[
    "guildwars2.com",
    "youtube.com",
    "ytimg.com",
    "guildjen.com",
    // Jetpack Photon. Only ever reached via a URL the GuildJen feed handed us,
    // and Photon fetches server-side, so this does not widen who learns the
    // player's IP beyond "the feeds we ship".
    "wp.com",
    "d3qqidoz8mm2hm.cloudfront.net",
];

pub fn url_ok(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    if u.scheme() != "https" {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    if reserved_still_host(host) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    STILL_DOMAINS.iter().any(|d| {
        // `strip_suffix` + the dot check is what keeps `evilytimg.com` out;
        // a bare `ends_with("ytimg.com")` would let it in.
        host == *d || host.strip_suffix(d).is_some_and(|pre| pre.ends_with('.'))
    })
}

/// Host-level reject shared with `radio::logos`: "localhost" or a literal IP
/// in a reserved range. Hostname DNS screening is the caller's business.
pub(crate) fn reserved_still_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip_is_reserved(ip),
        Err(_) => false,
    }
}

/// URL host text without brackets around IPv6 literals.
pub(crate) fn normalized_host(url: &reqwest::Url) -> Option<&str> {
    let host = url.host_str()?;
    Some(
        host.strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host),
    )
}

/// Shared radio stream/logo screen. DNS resolution must run on a worker.
/// An unresolved hostname is left to the transport's connection error.
pub(crate) fn url_host_is_reserved(url: &reqwest::Url) -> bool {
    use std::net::ToSocketAddrs;
    let Some(host) = normalized_host(url) else {
        return true;
    };
    if reserved_still_host(host) {
        return true;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    match (host, url.port_or_known_default().unwrap_or(443)).to_socket_addrs() {
        Ok(mut addrs) => addrs.any(|a| ip_is_reserved(a.ip())),
        Err(_) => false,
    }
}

/// At most `max` redirects (`previous().len() > max`, same cut as radio logos).
/// Each hop is re-screened; a rejected hop `stop()`s so the 3xx is returned
/// and the next host is never dialed.
pub(crate) fn screened_redirect_policy(
    max: usize,
    hop_ok: fn(&str) -> bool,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > max {
            attempt.error("too many redirects")
        } else if !hop_ok(attempt.url().as_str()) {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

/// Worker-only: allowlist plus DNS, for the original URL and every redirect hop.
fn hop_ok(url: &str) -> bool {
    url_ok(url)
        && reqwest::Url::parse(url)
            .ok()
            .is_some_and(|u| !url_host_is_reserved(&u))
}

/// Loopback/private/link-local/unspecified/ULA - addresses no community-
/// submitted URL (news image or radio stream) has any business dialing.
pub(crate) fn ip_is_reserved(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified()
        }
        std::net::IpAddr::V6(v) => {
            v.to_ipv4_mapped()
                .is_some_and(|ip| ip_is_reserved(std::net::IpAddr::V4(ip)))
                || v.is_loopback()
                || v.is_unspecified()
                || ipv6_unique_local_or_link_local(v)
        }
    }
}

/// `Ipv6Addr::is_unique_local` / `is_unicast_link_local` need a newer rustc
/// than this workspace pins; the prefix checks are the same RFCs.
fn ipv6_unique_local_or_link_local(v: std::net::Ipv6Addr) -> bool {
    let o = v.octets();
    (o[0] & 0xfe) == 0xfc || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80)
}

fn cache_id(url: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("GW2BOnews{:016x}", h.finish())
}

fn file_stem(url: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        Some("jpg")
    } else if bytes.len() >= 4
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
    {
        Some("png")
    } else {
        None
    }
}

/// Mark the next `n` unseen https URLs as in-flight so a worker can fetch them.
pub fn take_batch(urls: &[String], n: usize) -> Vec<String> {
    let mut map = slots();
    let mut out = Vec::new();
    for url in urls {
        if out.len() >= n {
            break;
        }
        if !url_ok(url) {
            continue;
        }
        match map.get(url) {
            Some(Slot::Pending | Slot::Failed) => continue,
            Some(Slot::Ready { path, .. }) if path.exists() => continue,
            _ => {}
        }
        map.insert(url.clone(), Slot::Pending);
        out.push(url.clone());
    }
    out
}

pub fn mark_ready(url: &str, path: PathBuf, aspect: f32) {
    slots().insert(
        url.to_string(),
        Slot::Ready {
            path,
            aspect: if aspect > 0.05 { aspect } else { 16.0 / 9.0 },
        },
    );
}

pub fn mark_failed(url: &str) {
    slots().insert(url.to_string(), Slot::Failed);
}

/// Drop Failed slots so Refresh can queue the URL again. Pending stays skipped.
pub fn clear_failed() {
    slots().retain(|_, slot| !matches!(slot, Slot::Failed));
}

pub fn release_pending(urls: &[String]) {
    let mut map = slots();
    for url in urls {
        if matches!(map.get(url), Some(Slot::Pending)) {
            map.remove(url);
        }
    }
}

/// Longest edge we keep. The News tab draws stills at 120px in a card
/// (`ui/news_feed.rs`), 48px in the index, and `240 * zoom` expanded — and
/// `fit_box` takes the smaller of the width- and height-fit, so in practice
/// the pane width binds long before the zoom ceiling does. 1024 draws 1:1 in a
/// news window up to ~1024px of content width and only softens past that,
/// while throwing away ~72% of a 1920x1080 source's pixels. Going higher buys
/// nothing on screen and costs encode time, which scales with OUTPUT pixels.
const MAX_EDGE: u32 = 1024;

/// Header-declared dimension we refuse before a decoder is ever allocated.
/// `MAX_BYTES` caps the COMPRESSED bytes and says nothing about the decode: a
/// 2 MB PNG can legally declare 40000x40000 and ask for 6.4 GB of RGBA inside
/// the game process.
const MAX_SRC_DIM: u32 = 8_000;

/// Belt to [`MAX_SRC_DIM`]'s braces, handed to the decoder itself. Well under
/// `Limits::default()`'s 512 MiB, which inside a game process is not a limit
/// so much as a crash.
const MAX_DECODE_ALLOC: u64 = 96 * 1024 * 1024;

/// Shrink an oversized still to [`MAX_EDGE`] and re-encode it in its own
/// format, so the Nexus texture loader is handed an image the overlay can
/// actually use. Returns the new bytes and their pixel size.
///
/// Nothing here trusts the input. The hosts are allowlisted; the CONTENT is
/// written by whoever the feed links to.
///
/// `catch_unwind` is not decoration: image's own README still lists "Many
/// decoders will panic on malicious input" under Known issues, and nexus ships
/// `panic_msgbox`, whose hook calls `MessageBoxA`. An unguarded decoder panic
/// would pop a Win32 dialog over the running game and poison the `SLOTS`
/// mutex on the way out. Same reasoning as [`texture`] below.
fn downscale(bytes: &[u8], ext: &str, src: (u32, u32)) -> Option<(Vec<u8>, u32, u32)> {
    if src.0.max(src.1) > MAX_SRC_DIM {
        return None;
    }
    let format = match ext {
        "png" => image::ImageFormat::Png,
        _ => image::ImageFormat::Jpeg,
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // `sniff` already decided the format, so set it rather than letting the
        // reader guess: PNG magic can then never be routed to another parser.
        let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
        // `Limits` is #[non_exhaustive] — a struct literal is a hard E0639, and
        // starting from `default()` means any limit image adds later arrives at
        // image's recommended value instead of unlimited. The two dimension
        // caps are the only ones image documents as STRICT; `max_alloc` is
        // explicitly non-strict and "some decoders may ignore it", so it is the
        // backstop, not the gate. Default leaves both dimensions at `None`.
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_SRC_DIM);
        limits.max_image_height = Some(MAX_SRC_DIM);
        limits.max_alloc = Some(MAX_DECODE_ALLOC);
        reader.limits(limits);
        // Must go through `ImageReader::decode`. The low-level decoders defeat
        // this: `PngDecoder::new` is `with_limits(r, Limits::no_limits())` and
        // the JPEG one hardcodes `set_max_width(usize::MAX)`. Only `decode()`
        // forwards the limits down into the decoder.
        let img = reader.decode().ok()?;
        // `resize` preserves aspect and fits inside the box, so a landscape
        // still lands at 1024 wide and a portrait at 1024 tall.
        let small = img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3);
        let (w, h) = (small.width(), small.height());
        let mut out = Vec::new();
        small
            .write_to(&mut std::io::Cursor::new(&mut out), format)
            .ok()?;
        Some((out, w, h))
    }))
    .ok()
    .flatten()
}

/// A JPEG that does not end in EOI was truncated in transit.
///
/// This has to be checked by hand: `image` hardcodes zune-jpeg's
/// `set_strict_mode(false)` and exposes no way to turn it back on, so feeding
/// it 1% of a JPEG still returns `Ok` with the full declared dimensions. A
/// connection dropped mid-body would otherwise be cached as a permanent
/// half-grey thumbnail — the disk cache never re-fetches a Ready slot.
fn jpeg_is_complete(bytes: &[u8]) -> bool {
    matches!(bytes.last_chunk::<2>(), Some([0xFF, 0xD9]))
}

fn exceeds_max_edge(w: u32, h: u32) -> bool {
    w.max(h) > MAX_EDGE
}

pub fn download(
    url: &str,
    dir: &Path,
    token: &CancellationToken,
    version: &str,
) -> Option<(PathBuf, f32)> {
    if token.is_cancelled() || !hop_ok(url) {
        return None;
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .redirect(screened_redirect_policy(2, hop_ok))
        .build()
        .ok()?;
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("GW2BuildOptimizer/{version} news")) {
        headers.insert(reqwest::header::USER_AGENT, v);
    }
    let resp = client.get(url).headers(headers).send().ok()?;
    if !resp.status().is_success() || !url_ok(resp.url().as_str()) {
        return None;
    }
    let bytes = gw2_api::transport::read_body_capped(resp, MAX_BYTES as u64).ok()?;
    let ext = sniff(&bytes)?;
    if ext == "jpg" && !jpeg_is_complete(&bytes) {
        return None;
    }
    // Header walk, no decoder allocated yet.
    let (pw, ph) = pixel_size(&bytes)?;
    // Only pay for a decode when the image is actually too big. Most stills
    // are not: YouTube already hands us a 320x180 mqdefault, and re-encoding
    // that would cost quality and time to change nothing.
    let (bytes, pw, ph) = if exceeds_max_edge(pw, ph) {
        downscale(&bytes, ext, (pw, ph))?
    } else {
        (bytes, pw, ph)
    };
    let aspect = pw as f32 / ph as f32;
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{}.{ext}", file_stem(url)));
    let tmp = path.with_extension(format!("{ext}.tmp"));
    std::fs::write(&tmp, &bytes).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    crate::cache_bounds::evict_oldest(dir, MAX_CACHE_FILES);
    Some((path, aspect))
}

pub fn pixel_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
    {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    jpeg_size(bytes)
}

fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 8 < bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        while i < bytes.len() && bytes[i] == 0xFF {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let marker = bytes[i];
        i += 1;
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        if i + 2 > bytes.len() {
            return None;
        }
        let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        if len < 2 || i + len > bytes.len() {
            return None;
        }
        if matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        ) {
            if len < 7 {
                return None;
            }
            let h = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += len;
    }
    None
}

/// Width / height. 16:9 while the file is still in flight.
pub fn aspect(url: &str) -> f32 {
    match slots().get(url) {
        Some(Slot::Ready { aspect, .. }) => *aspect,
        _ => 16.0 / 9.0,
    }
}

/// Load a cached still on the render thread. None while the file is in flight.
pub fn texture(url: &str) -> Option<TextureId> {
    if !url_ok(url) {
        return None;
    }
    let id = cache_id(url);
    let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nexus::texture::get_texture(&id)
    }))
    .ok()
    .flatten();
    if let Some(tex) = loaded {
        return Some(tex.id());
    }
    let path = {
        let map = slots();
        match map.get(url) {
            Some(Slot::Ready { path, .. }) if path.exists() => Some(path.clone()),
            _ => None,
        }
    }?;
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

#[cfg(test)]
static SLOTS_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn slots_test_guard() -> std::sync::MutexGuard<'static, ()> {
    SLOTS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Origin is a loopback 302 (reqwest screens `attempt.url()`, the *next* hop).
/// A reserved Location must `stop()`: no connect, no body, still a 3xx from origin.
#[cfg(test)]
pub(crate) fn assert_policy_stops_reserved_redirects(policy: reqwest::redirect::Policy) {
    use std::io::{BufRead, Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::{Duration, Instant};

    fn drain_headers(stream: &std::net::TcpStream) {
        let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if line.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
        }
    }

    fn serve_302(location: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind origin");
        let port = listener.local_addr().expect("addr").port();
        let loc = location.to_string();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            drain_headers(&stream);
            let body = format!(
                "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
            let mut sink = Vec::new();
            let _ = (&stream).take(64 * 1024).read_to_end(&mut sink);
            let _ = stream.shutdown(Shutdown::Both);
        });
        format!("http://127.0.0.1:{port}/ok")
    }

    let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let probe_port = probe.local_addr().expect("probe addr").port();
    let client = reqwest::blocking::Client::builder()
        .redirect(policy)
        .timeout(Duration::from_secs(2))
        .pool_max_idle_per_host(0)
        .build()
        .expect("client");

    let locations = [
        format!("http://127.0.0.1:{probe_port}/steal"),
        format!("https://127.0.0.1:{probe_port}/steal"),
        "http://10.0.0.1/".to_string(),
        "https://10.0.0.1/".to_string(),
        "http://192.168.0.1/".to_string(),
        "http://172.16.0.1/".to_string(),
        "http://169.254.169.254/latest/meta-data".to_string(),
        "https://169.254.169.254/latest/meta-data".to_string(),
        "http://[fe80::1]/".to_string(),
    ];
    for loc in &locations {
        let origin = serve_302(loc);
        let t0 = Instant::now();
        let resp = client
            .get(&origin)
            .send()
            .unwrap_or_else(|e| panic!("origin GET for {loc}: {e}"));
        assert!(
            t0.elapsed() < Duration::from_millis(1500),
            "redirect to {loc} was dialed (elapsed {:?})",
            t0.elapsed()
        );
        assert!(
            resp.status().is_redirection(),
            "expected 3xx stop for {loc}, got {}",
            resp.status()
        );
        assert_eq!(
            resp.url().host_str(),
            Some("127.0.0.1"),
            "client must not adopt reserved hop {loc}"
        );
        let bytes = resp.bytes().expect("origin body");
        assert!(
            bytes.is_empty(),
            "reserved hop body must not be read for {loc}"
        );
    }

    probe.set_nonblocking(true).expect("probe nonblocking");
    match probe.accept() {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::TimedOut
            ) => {}
        Ok(_) => panic!("loopback probe accepted a follow to 127.0.0.1"),
        Err(e) => panic!("probe accept: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(w: u32, h: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), format)
            .expect("test image encodes");
        out
    }

    /// The whole point of the feature: an image too big for any News draw site
    /// comes back at MAX_EDGE, still the right shape, and still decodable.
    #[test]
    fn downscale_fits_max_edge_and_keeps_aspect() {
        for (w, h, want) in [
            (1920u32, 1080u32, (1024u32, 576u32)), // landscape 16:9
            (1080, 1920, (576, 1024)),             // portrait
            (2000, 2000, (1024, 1024)),            // square
        ] {
            let src = encode(w, h, image::ImageFormat::Png);
            let (out, ow, oh) =
                downscale(&src, "png", (w, h)).unwrap_or_else(|| panic!("{w}x{h} downscales"));
            assert_eq!((ow, oh), want, "{w}x{h}");
            // Re-parseable, and the header agrees with the reported size — the
            // texture loader reads the file, not our return value.
            assert_eq!(pixel_size(&out), Some(want), "{w}x{h} header");
            assert_eq!(sniff(&out), Some("png"), "{w}x{h} stays PNG");
        }
    }

    /// A still already smaller than MAX_EDGE must never reach `downscale` —
    /// re-encoding YouTube's 320x180 mqdefault would cost quality to change
    /// nothing. This pins the branch condition `download` uses.
    #[test]
    fn images_at_or_under_max_edge_are_not_resized() {
        for (w, h) in [(320u32, 180u32), (MAX_EDGE, 576), (576, MAX_EDGE)] {
            assert!(
                !exceeds_max_edge(w, h),
                "{w}x{h} must take the pass-through branch"
            );
        }
        assert!(exceeds_max_edge(1920, 1080), "1920x1080 must be resized");
    }

    /// MAX_BYTES caps compressed bytes and says nothing about the decode, so a
    /// header claiming absurd dimensions is refused before a decoder is even
    /// allocated. Note the bytes here are empty: nothing parses them.
    #[test]
    fn decompression_bomb_is_refused_before_decoding() {
        assert!(downscale(&[], "png", (40_000, 40_000)).is_none());
        assert!(downscale(&[], "png", (MAX_SRC_DIM + 1, 8)).is_none());
        assert!(downscale(&[], "jpg", (8, MAX_SRC_DIM + 1)).is_none());
    }

    /// `image` hardcodes zune-jpeg's non-strict mode, so a JPEG cut off in
    /// transit decodes "successfully" as a half-grey image and would be cached
    /// forever. The EOI marker is the only thing that catches it.
    #[test]
    fn truncated_jpeg_is_rejected() {
        let full = encode(64, 64, image::ImageFormat::Jpeg);
        assert!(jpeg_is_complete(&full), "a complete JPEG ends in FFD9");
        assert!(!jpeg_is_complete(&full[..full.len() / 2]));
        assert!(!jpeg_is_complete(&full[..1]));
        assert!(!jpeg_is_complete(&[]));
    }

    /// Allowlisted host, unvetted content: garbage must return None, never
    /// panic. A panic here pops nexus's Win32 message box over the running
    /// game and poisons the SLOTS mutex.
    #[test]
    fn corrupt_input_returns_none_without_panicking() {
        assert!(downscale(b"not an image at all", "png", (100, 100)).is_none());
        assert!(downscale(&[], "png", (100, 100)).is_none());
        // Valid PNG magic and header, body replaced with noise.
        let mut wrecked = encode(1200, 900, image::ImageFormat::Png);
        let tail = wrecked.len() / 2;
        for (i, b) in wrecked[tail..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let _ = downscale(&wrecked, "png", (1200, 900));
    }

    /// Live end-to-end against the image that forced this work: the official
    /// blog's 1920x1080 announcement PNG, ~1.95 MB — which the old 1.5 MB cap
    /// rejected outright, so it never reached the cache at all.
    ///
    /// Ignored by default; it needs the network. Run it with:
    ///   cargo test -p gw2-build-optimizer live_official_still -- --ignored --nocapture
    #[test]
    #[ignore = "hits the network"]
    fn live_official_still_downscales() {
        let url = "https://d3qqidoz8mm2hm.cloudfront.net/wp-content/uploads/2026/08/51b06GW2_AnniversarySale_Social_1920x1080_EN_announcement.png";
        assert!(url_ok(url), "host must be allowlisted");
        let bytes = reqwest::blocking::get(url)
            .expect("fetch")
            .bytes()
            .expect("body")
            .to_vec();
        assert!(
            bytes.len() <= MAX_BYTES,
            "{} bytes exceeds MAX_BYTES {MAX_BYTES}",
            bytes.len()
        );
        let ext = sniff(&bytes).expect("sniffs as an image");
        let (pw, ph) = pixel_size(&bytes).expect("header parses");
        let (out, ow, oh) = downscale(&bytes, ext, (pw, ph)).expect("downscales");
        println!(
            "{pw}x{ph} {} bytes  ->  {ow}x{oh} {} bytes  ({:.1}% of original)",
            bytes.len(),
            out.len(),
            100.0 * out.len() as f32 / bytes.len() as f32
        );
        assert_eq!(ow.max(oh), MAX_EDGE);
        assert!(out.len() < bytes.len(), "must actually get smaller");
    }

    #[test]
    fn only_https_feed_hosts() {
        assert!(url_ok("https://i.ytimg.com/vi/x/hqdefault.jpg"));
        assert!(url_ok("https://www.guildwars2.com/wp-content/x.jpg"));
        assert!(url_ok("https://img.youtube.com/vi/x/mqdefault.jpg"));
        assert!(!url_ok("http://i.ytimg.com/vi/x/hqdefault.jpg"));
        assert!(!url_ok("https://localhost/x.jpg"));
        assert!(!url_ok("https://127.0.0.1/x.jpg"));
        assert!(!url_ok("https://[::1]/x.jpg"));
        assert!(!url_ok("https://10.0.0.1/x.jpg"));
        assert!(!url_ok("https://192.168.1.8/x.jpg"));
        assert!(!url_ok("https://172.16.0.1/x.jpg"));
        assert!(!url_ok("https://169.254.169.254/latest/meta-data"));
        assert!(!url_ok("https://[fe80::1]/x.jpg"));
        assert!(!url_ok("https://[fd00::1]/x.jpg"));
        assert!(!url_ok("https://evil.example/x.jpg"));
        assert!(!url_ok("javascript:alert(1)"));
    }

    #[test]
    fn hop_ok_rejects_reserved_redirect_targets() {
        for url in [
            "https://127.0.0.1/x.jpg",
            "https://10.0.0.1/x.jpg",
            "https://192.168.1.8/x.jpg",
            "https://172.16.0.1/x.jpg",
            "https://169.254.169.254/latest/meta-data",
            "https://[fe80::1]/x.jpg",
            "http://127.0.0.1/x.jpg",
            "http://10.0.0.1/x.jpg",
        ] {
            assert!(!hop_ok(url), "{url}");
        }
    }

    #[test]
    fn redirect_to_reserved_is_stopped_before_connect() {
        assert_policy_stops_reserved_redirects(screened_redirect_policy(2, hop_ok));
    }

    /// The hosts the live feeds ACTUALLY serve stills from, captured
    /// 2026-09-04. The previous exact-host allowlist passed every test above
    /// and still rejected all of these, which is how every News image went
    /// dark with CI green.
    #[test]
    fn live_feed_image_hosts_are_allowlisted() {
        // YouTube round-robins the shard; it never serves bare i.ytimg.com.
        for shard in ["i1", "i2", "i3", "i4"] {
            assert!(
                url_ok(&format!("https://{shard}.ytimg.com/vi/abc/hqdefault.jpg")),
                "{shard}.ytimg.com"
            );
        }
        // GuildJen proxies every image through Jetpack Photon.
        assert!(url_ok(
            "https://i0.wp.com/guildjen.com/wp-content/uploads/2026/08/x.jpg?fit=1280%2C720&ssl=1"
        ));
        // www.guildjen.com/feed/ 301s to the apex, and the download re-checks
        // the post-redirect URL.
        assert!(url_ok(
            "https://guildjen.com/wp-content/uploads/2026/08/x.jpg"
        ));
        // The official blog's CloudFront distribution.
        assert!(url_ok(
            "https://d3qqidoz8mm2hm.cloudfront.net/wp-content/x.jpg"
        ));
    }

    /// Suffix matching must not become "ends with these letters". Each of
    /// these is a domain an attacker can register today.
    #[test]
    fn lookalike_domains_stay_rejected() {
        for bad in [
            "https://evilytimg.com/vi/x/hqdefault.jpg",
            "https://ytimg.com.evil.example/x.jpg",
            "https://notguildjen.com/x.jpg",
            "https://wp.com.evil.example/x.jpg",
            "https://myguildwars2.com/x.jpg",
            // One ArenaNet distribution is allowlisted, not all of CloudFront.
            "https://d111111abcdef8.cloudfront.net/x.jpg",
        ] {
            assert!(!url_ok(bad), "{bad} must not be admitted");
        }
    }

    #[test]
    fn refresh_clears_failed_so_take_batch_retries() {
        let _lock = slots_test_guard();
        let url = "https://i.ytimg.com/vi/retry-failed-still/hqdefault.jpg";
        mark_failed(url);
        assert!(
            take_batch(&[url.to_string()], 1).is_empty(),
            "Failed must skip within one wave"
        );
        clear_failed();
        assert_eq!(take_batch(&[url.to_string()], 1), vec![url.to_string()]);
        release_pending(&[url.to_string()]);
    }

    #[test]
    fn release_pending_retries_stuck_pending_keeps_failed() {
        let _lock = slots_test_guard();
        let stuck = "https://i.ytimg.com/vi/release-stuck-pending/hqdefault.jpg";
        let failed = "https://i.ytimg.com/vi/release-keeps-failed/hqdefault.jpg";
        assert_eq!(take_batch(&[stuck.to_string()], 1), vec![stuck.to_string()]);
        mark_failed(failed);
        release_pending(&[stuck.to_string(), failed.to_string()]);
        assert_eq!(
            take_batch(&[stuck.to_string()], 1),
            vec![stuck.to_string()],
            "stuck Pending must be retryable after release"
        );
        assert!(
            take_batch(&[failed.to_string()], 1).is_empty(),
            "Failed must survive release_pending"
        );
        release_pending(&[stuck.to_string()]);
        clear_failed();
    }

    #[test]
    fn still_over_cap_is_rejected() {
        let err = gw2_api::transport::read_body_capped(
            std::io::Cursor::new(vec![0u8; MAX_BYTES + 1]),
            MAX_BYTES as u64,
        )
        .expect_err("over-cap must fail closed");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn sniff_jpeg_and_png_magic() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff(&[0x89, 0x50, 0x4E, 0x47, 0x0D]), Some("png"));
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(sniff(&[]), None);
    }

    #[test]
    fn png_ihdr_size() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&320u32.to_be_bytes());
        png.extend_from_slice(&180u32.to_be_bytes());
        assert_eq!(pixel_size(&png), Some((320, 180)));
    }

    #[test]
    fn stills_reuse_logo_cache_bounds() {
        let src = include_str!("news_art.rs");
        assert!(
            src.contains("crate::cache_bounds::evict_oldest(dir, MAX_CACHE_FILES)"),
            "download must evict after write"
        );
        assert!(
            src.contains("crate::cache_bounds::admit_texture(&mut set, &id, MAX_TEXTURES)"),
            "texture must gate Nexus uploads"
        );
    }
}
