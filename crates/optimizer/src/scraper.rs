//! Community build scraper for Snowcrows, Hardstuck, and GuildJen.
//!
//! Each scraper fetches HTML pages, extracts build data via text pattern matching,
//! and returns normalized `BenchmarkBuild` records.
//!
//! All scrapers are resilient — they return partial results on failure rather than
//! panicking. HTTP errors or parse failures produce a `ScrapeResult.error` string.
//!
//! Storage: writes one JSON file per (source, profession, mode) to
//! `{addon_dir}/benchmarks/`.

use std::collections::HashMap;
use std::path::Path;

use crate::benchmark::{BenchmarkBuild, ScrapeResult};

// User agent

/// Firefox on Windows, matching the platform the addon actually runs on.
///
/// A cross-platform claim is worse than none: a macOS Safari string arriving
/// from a Windows game process is an inconsistency, not a disguise. Whatever
/// this says, the header set below has to agree with it - a browser
/// User-Agent over a bare request is more conspicuous than the honest one it
/// replaced, because no real Firefox has ever sent a navigation without
/// Accept-Language or Sec-Fetch-Mode.
///
/// This is a fixed string and will age. A years-old version is its own
/// signal; refresh it when it drifts far from current. What actually keeps
/// this welcome is [`PACE_MS`] and the retry backoff, not the header.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:142.0) Gecko/20100101 Firefox/142.0";

/// Sentinel error string set on `ScrapeResult.error` when a scrape was skipped
/// because cancellation was requested before it started.
pub const CANCELLED_ERROR: &str = "cancelled";

/// One-line status for Settings: live heartbeat, down, count, or never-synced dash.
pub fn format_source_status(
    display_name: &str,
    count: usize,
    live: Option<&str>,
    error: Option<&str>,
) -> String {
    if let Some(live) = live.filter(|s| !s.is_empty()) {
        return format!("{}  {}", display_name, live);
    }
    if let Some(err) = error {
        let short: String = err.chars().take(60).collect();
        if count == 0 {
            return format!("{}  down — {}", display_name, short);
        }
        return format!("{}  {} builds (warning: {})", display_name, count, short);
    }
    if count == 0 {
        return format!("{}  —", display_name);
    }
    format!("{}  {} builds", display_name, count)
}

/// Scrape all three community sites and save results to `{addon_dir}/benchmarks/`.
///
/// `should_cancel` is consulted before each of the three sequential HTTP scrapes.
/// When it returns `true`, the remaining sources are skipped and replaced with a
/// `ScrapeResult` carrying `error = Some(CANCELLED_ERROR)` and empty builds.
/// Pass `&|| false` from contexts where cancellation is not meaningful.
///
/// Returns one `ScrapeResult` per site regardless of whether it succeeded or was
/// skipped. Never panics — errors are captured in `ScrapeResult.error`.
pub fn scrape_all(addon_dir: &Path, should_cancel: &dyn Fn() -> bool) -> Vec<ScrapeResult> {
    scrape_all_with_progress(addon_dir, should_cancel, &|_, _| {})
}

/// Same as [`scrape_all`], plus a `(source, message)` heartbeat for the UI.
pub fn scrape_all_with_progress(
    addon_dir: &Path,
    should_cancel: &dyn Fn() -> bool,
    on_progress: &dyn Fn(&str, &str),
) -> Vec<ScrapeResult> {
    // A previous run's slowdown is not this run's problem: the pressure
    // that caused it may be hours old.
    THROTTLE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    // Cancel before any work
    if should_cancel() {
        on_progress("snowcrows", "cancelled");
        on_progress("hardstuck", "cancelled");
        on_progress("guildjen", "cancelled");
        return vec![
            cancelled_result("snowcrows"),
            cancelled_result("hardstuck"),
            cancelled_result("guildjen"),
        ];
    }

    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Failed to build HTTP client: {}", e);
            on_progress("snowcrows", &format!("down: {}", msg));
            on_progress("hardstuck", &format!("down: {}", msg));
            on_progress("guildjen", &format!("down: {}", msg));
            return vec![
                ScrapeResult {
                    source: "snowcrows".into(),
                    builds: vec![],
                    error: Some(msg.clone()),
                    failed: 0,
                },
                ScrapeResult {
                    source: "hardstuck".into(),
                    builds: vec![],
                    error: Some(msg.clone()),
                    failed: 0,
                },
                ScrapeResult {
                    source: "guildjen".into(),
                    builds: vec![],
                    error: Some(msg),
                    failed: 0,
                },
            ];
        }
    };

    let benchmarks_dir = addon_dir.join("benchmarks");
    if let Err(e) = std::fs::create_dir_all(&benchmarks_dir) {
        let msg = format!("Cannot create benchmarks dir: {}", e);
        on_progress("snowcrows", &format!("down: {}", msg));
        on_progress("hardstuck", &format!("down: {}", msg));
        on_progress("guildjen", &format!("down: {}", msg));
        return vec![
            ScrapeResult {
                source: "snowcrows".into(),
                builds: vec![],
                error: Some(msg.clone()),
                failed: 0,
            },
            ScrapeResult {
                source: "hardstuck".into(),
                builds: vec![],
                error: Some(msg.clone()),
                failed: 0,
            },
            ScrapeResult {
                source: "guildjen".into(),
                builds: vec![],
                error: Some(msg),
                failed: 0,
            },
        ];
    }

    // Before anything is written: a folder half full of files no version
    // still produces is not a store, it is a haunting.
    let pruned = prune_stale_benchmarks(&benchmarks_dir);
    if pruned > 0 {
        on_progress(
            "guildjen",
            &format!("removed {pruned} file(s) from an older format"),
        );
    }

    let today = today_string();

    // Anything read today already, so a second run in the same day only
    // fetches what is new or missing. Read once, before any source starts:
    // a source that finishes mid-run writes into this same folder, and
    // re-reading would let one source's fresh output masquerade as another's
    // prior state.
    let known = load_todays_builds(&benchmarks_dir, &today);
    if !known.is_empty() {
        on_progress(
            "snowcrows",
            &format!("{} build(s) already read today", known.len()),
        );
    }

    // Scrape #1: Snowcrows. Cancellation re-checked here so a pulse during
    // setup above still aborts before any network I/O.
    if should_cancel() {
        on_progress("snowcrows", "cancelled");
        on_progress("hardstuck", "cancelled");
        on_progress("guildjen", "cancelled");
        return vec![
            cancelled_result("snowcrows"),
            cancelled_result("hardstuck"),
            cancelled_result("guildjen"),
        ];
    }
    on_progress("snowcrows", "starting");
    let sc_result = finish_source(
        "snowcrows",
        scrape_snowcrows(&client, &today, &known, should_cancel, on_progress),
        &benchmarks_dir,
        on_progress,
    );

    // Scrape #2: Hardstuck.
    if should_cancel() {
        on_progress("hardstuck", "cancelled");
        on_progress("guildjen", "cancelled");
        return vec![
            sc_result,
            cancelled_result("hardstuck"),
            cancelled_result("guildjen"),
        ];
    }
    on_progress("hardstuck", "starting");
    let hs_result = finish_source(
        "hardstuck",
        scrape_hardstuck(&client, &today, &known, should_cancel, on_progress),
        &benchmarks_dir,
        on_progress,
    );

    // Scrape #3: GuildJen.
    if should_cancel() {
        on_progress("guildjen", "cancelled");
        return vec![sc_result, hs_result, cancelled_result("guildjen")];
    }
    on_progress("guildjen", "starting");
    let gj_result = finish_source(
        "guildjen",
        scrape_guildjen(&client, &today, &known, should_cancel, on_progress),
        &benchmarks_dir,
        on_progress,
    );

    vec![sc_result, hs_result, gj_result]
}

fn finish_source(
    source: &str,
    result: Result<(Vec<BenchmarkBuild>, bool, usize), String>,
    dir: &Path,
    on_progress: &dyn Fn(&str, &str),
) -> ScrapeResult {
    match result {
        Ok((builds, cancelled, failed)) => {
            // Cancel mid-source must not overwrite last-good on-disk groups
            // with a partial scrape. Keep the in-memory vec for this session.
            if cancelled {
                on_progress(source, "cancelled");
                return ScrapeResult {
                    source: source.into(),
                    builds,
                    error: Some(CANCELLED_ERROR.into()),
                    failed,
                };
            }
            if let Err(e) = save_builds(&builds, dir) {
                on_progress(source, &format!("save failed: {e}"));
                return ScrapeResult {
                    source: source.into(),
                    builds,
                    error: Some(e),
                    failed,
                };
            }
            on_progress(source, &format!("done {}", builds.len()));
            ScrapeResult {
                source: source.into(),
                builds,
                error: None,
                failed,
            }
        }
        Err(e) => {
            on_progress(source, &format!("down: {}", e));
            ScrapeResult {
                source: source.into(),
                builds: vec![],
                error: Some(e),
                // The source never got far enough to list a page, so nothing
                // could have failed individually - the whole thing did.
                failed: 0,
            }
        }
    }
}

/// Construct a placeholder `ScrapeResult` that records cancellation for a source.
fn cancelled_result(source: &str) -> ScrapeResult {
    ScrapeResult {
        source: source.into(),
        builds: vec![],
        error: Some(CANCELLED_ERROR.into()),
        failed: 0,
    }
}

/// Load all previously saved benchmark builds from `{addon_dir}/benchmarks/`.
///
/// Entries are sorted by path before processing so two calls with the same
/// disk contents return Vecs in identical order. `fs::read_dir` order is
/// OS-defined (Windows is alphabetical, Linux is roughly inode order), and
/// downstream `find_best_benchmark` ties broke on raw iteration order —
/// meaning the "best" benchmark could differ between machines.
pub fn load_benchmarks(addon_dir: &Path) -> Vec<BenchmarkBuild> {
    load_benchmarks_from(&addon_dir.join("benchmarks"))
}

/// [`load_benchmarks`] against an explicit directory, so a test can read a
/// checked-in snapshot of the corpus instead of the player's synced one.
pub fn load_benchmarks_from(dir: &Path) -> Vec<BenchmarkBuild> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    let mut builds = Vec::new();
    for path in paths {
        if let Ok(file) = std::fs::File::open(&path) {
            let reader = std::io::BufReader::new(file);
            if let Ok(v) = serde_json::from_reader::<_, Vec<BenchmarkBuild>>(reader) {
                builds.extend(v);
            }
        }
    }
    builds
}

// Snowcrows

/// Professions enumerated directly — Snowcrows uses server-side rendering per-profession page.
/// The top-level `/builds` page is a SPA shell with no static links.
const SC_PROFESSIONS: &[&str] = &[
    "guardian",
    "warrior",
    "engineer",
    "ranger",
    "thief",
    "elementalist",
    "mesmer",
    "necromancer",
    "revenant",
];

/// Scrape Snowcrows (PvE raid/strike meta builds).
/// Enumerates per-profession pages: https://snowcrows.com/builds/raids/{profession}
///
/// `should_cancel` is consulted before every outer (per-profession) and inner
/// (per-build) HTTP fetch. On cancellation, returns whatever builds were
/// collected so far paired with `cancelled = true`.
fn scrape_snowcrows(
    client: &reqwest::blocking::Client,
    today: &str,
    known: &HashMap<String, BenchmarkBuild>,
    should_cancel: &dyn Fn() -> bool,
    on_progress: &dyn Fn(&str, &str),
) -> Result<(Vec<BenchmarkBuild>, bool, usize), String> {
    let mut last_html: Option<String> = None;
    // Carried with the link so progress can name the class it is on, the
    // way the GuildJen path does. The profession is only known here, at
    // the index page the link came from.
    let mut all_links: Vec<(String, String)> = Vec::new();

    on_progress("snowcrows", "listing builds…");
    // Collect build links from each profession's page
    for profession in SC_PROFESSIONS {
        if should_cancel() {
            return Ok((Vec::new(), true, 0));
        }
        let prof_url = format!("https://snowcrows.com/builds/raids/{}", profession);
        on_progress("snowcrows", &format!("listing {}…", profession));
        let Ok(html) = fetch_html(client, &prof_url) else {
            continue;
        };
        last_html = Some(html.clone());
        // Build links look like: href="/builds/raids/guardian/power-dragonhunter-..."
        let links = extract_build_links(&html, "/builds/raids/", usize::MAX);
        for link in links {
            // Skip profession-index links (no build slug after the profession name)
            let parts: Vec<&str> = link.trim_matches('/').split('/').collect();
            // /builds/raids/{profession}/{slug} has 4 parts
            if parts.len() >= 4 && !parts[3].is_empty() && !parts[3].contains('?') {
                let full = format!("https://snowcrows.com{}", link);
                if !all_links.iter().any(|(_, seen)| *seen == full) {
                    all_links.push((title_case(profession), full));
                }
            }
        }
    }

    if all_links.is_empty() {
        // Distinguish "we were served a block page / wrong content" from
        // "the site genuinely listed no builds" — the former is almost always a
        // network filter (e.g. a UniFi/UDMPro content block on snowcrows.com) and
        // needs an allowlist change, not a code fix.
        if last_html
            .as_deref()
            .map(looks_like_blocked_page)
            .unwrap_or(false)
        {
            return Err(
                "Snowcrows: request was intercepted by a network block/filter page \
                 (the domain appears restricted by your gateway/firewall — allowlist \
                 snowcrows.com, e.g. on your UniFi/UDMPro content filter)"
                    .into(),
            );
        }
        return Err("Snowcrows: no build links found on any profession page".into());
    }

    let mut builds = Vec::new();
    let mut failed = 0usize;
    let total = all_links.len();
    on_progress("snowcrows", &format!("0/{}", total));
    // Every build the index lists. `all_links` is accumulated one profession
    // page at a time, so this already walks a class at a time.
    for (i, (class, url)) in all_links.into_iter().enumerate() {
        if should_cancel() {
            return Ok((builds, true, failed));
        }
        // Already read today: take it and skip both the request and the
        // wait that would have come with it.
        if let Some(b) = known.get(&url) {
            builds.push(b.clone());
            on_progress("snowcrows", &format!("{class} {}/{total}", i + 1));
            continue;
        }
        if i > 0 && !pace(should_cancel) {
            return Ok((builds, true, failed));
        }
        match scrape_snowcrows_build(client, &url, today) {
            Ok(b) => builds.push(b),
            Err(_) => failed += 1,
        }
        on_progress("snowcrows", &format!("{class} {}/{total}", i + 1));
    }
    Ok((builds, false, failed))
}

/// Build a `BenchmarkBuild` from page HTML once the per-site fields
/// (source/profession/spec/mode/role) have been derived. The gear/trait/skill
/// extraction is identical across every source, so it lives here.
// Builder over per-site fields already derived by the caller; bundling the HTML
// and metadata into a struct would just mirror the argument list.
#[allow(clippy::too_many_arguments)]
/// A per-build ceiling, so one pathological page cannot bloat the day's file.
///
/// Measured over saved pages from all three sites: 8.6 KB for a Hardstuck
/// build, 14 KB and 24 KB for GuildJen's largest, 19 KB for a Snowcrows one.
/// So this holds every real page with room to spare, and the worst case is
/// ~24 MB across a full sync — beside a 118 MB game-data cache the addon
/// already loads, that is not the thing to optimise.
///
/// It cuts from the END, which is where the rotation sits on a Hardstuck
/// page. Do not lower it without re-measuring.
const PROSE_LIMIT: usize = 32_768;

/// Truncate to at most `max_bytes`, on a character boundary.
///
/// Slicing a `&str` at a byte index panics when a multi-byte character
/// straddles it. An em dash in a build description did exactly that once and
/// killed a 328-page sync mid-run, so no byte index touches this text.
fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .map(|(at, _)| at)
        .take_while(|at| *at <= max_bytes)
        .last()
        .unwrap_or(0);
    text[..end].to_string()
}

// Eight facts about one page, each already computed by the caller and none
// derivable from the others: where it came from, when, which site, and the
// four labels the index carried. Bundling them into a struct would move the
// same eight assignments one line up and add a type nobody else uses.
#[allow(clippy::too_many_arguments)]
fn benchmark_from_html(
    html: &str,
    url: &str,
    today: &str,
    source: &str,
    profession: String,
    spec_name: String,
    mode: &str,
    role: &str,
) -> BenchmarkBuild {
    // Each site publishes ids its own way, so the parser is chosen by site
    // rather than by trying all three. An unknown source gets GuildJen's
    // reader, which finds nothing rather than inventing something.
    let mut published = match source {
        "snowcrows" => crate::providers::snowcrows::parse(html),
        "hardstuck" => crate::providers::hardstuck::parse(html),
        _ => crate::providers::guildjen::parse(html),
    };
    // The pruner runs once here and serves both readers below. It must not
    // run *before* the parsers: it deletes the ids — Snowcrows' embeds are
    // empty divs and Hardstuck's chat code lives in an `<input value>`.
    let article = crate::article::prune_to_article(html);
    let prose = crate::article::article_text(html);
    // The prefix the gear actually states, when it states one; otherwise a
    // text scan, which is what wanted the prose in the first place.
    let gear_prefix = published
        .dominant_stat()
        .unwrap_or_else(|| extract_gear_prefix(&article));
    // The rotation and the role's operating instructions live in this text.
    // Captured during the scrape because the alternative is fetching every
    // page a second time.
    published.prose = truncate_chars(&prose, PROSE_LIMIT);
    let (benchmark_dps, log_url) = match source {
        "snowcrows" => crate::providers::snowcrows::benchmark(html),
        _ => (None, None),
    };
    BenchmarkBuild {
        benchmark_dps,
        log_url,
        source: source.into(),
        profession,
        spec_name,
        mode: mode.into(),
        role: role.to_string(),
        gear_prefix,
        build_code: published.build_code.clone(),
        source_url: url.to_string(),
        scraped_at: today.to_string(),
        published,
    }
}

fn scrape_snowcrows_build(
    client: &reqwest::blocking::Client,
    url: &str,
    today: &str,
) -> Result<BenchmarkBuild, String> {
    let html = fetch_html(client, url)?;

    // URL structure: /builds/raids/{profession}/{slug}
    // slug like "power-dragonhunter-virtues-longbow-greatsword"
    let url_parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    let profession = url_parts
        .get(url_parts.len().saturating_sub(2))
        .map(|s| title_case(&s.replace('-', " ")))
        .unwrap_or_default();
    let slug = url_parts.last().copied().unwrap_or("");

    let spec_name = extract_spec_from_slug(slug);
    // The build's own heading, falling back to the slug it is derived from.
    // Both say the same thing — "Condition Reaper", `condition-reaper-…` —
    // but the heading is the site's statement rather than our reading of a
    // URL, and it goes through the same vocabulary as the other two sources
    // so the stored roles stay comparable.
    let name =
        crate::providers::snowcrows::build_name(&html).unwrap_or_else(|| slug.replace('-', " "));
    let role = crate::providers::role_label(
        crate::providers::snowcrows::scale_from_url(url),
        crate::providers::role_in_name(&name).unwrap_or_default(),
    );

    Ok(benchmark_from_html(
        &html,
        url,
        today,
        "snowcrows",
        profession,
        spec_name,
        "PvE",
        &role,
    ))
}

// Hardstuck

/// Hardstuck URL format: /gw2/builds/{profession}/{id-or-name-slug}/
/// The index page is Next.js — links appear as hrefs in the static HTML.
/// We enumerate per-profession pages to bypass the JS-rendered filter UI.
const HS_PROFESSIONS: &[&str] = &[
    "guardian",
    "warrior",
    "engineer",
    "ranger",
    "thief",
    "elementalist",
    "mesmer",
    "necromancer",
    "revenant",
];

/// Pin a Hardstuck listing href to `https://hardstuck.gg`.
///
/// Rejects `http://` and off-origin absolute URLs (same pin as GuildJen).
fn pin_hardstuck_href(link: &str) -> Option<String> {
    if link.starts_with("https://hardstuck.gg/") {
        Some(link.to_string())
    } else if link.starts_with("http") {
        None
    } else {
        Some(format!("https://hardstuck.gg{}", link))
    }
}

/// Scrape Hardstuck (multi-mode builds).
///
/// `should_cancel` is consulted before every outer (per-profession) and inner
/// (per-build) HTTP fetch. On cancellation, returns whatever builds were
/// collected so far paired with `cancelled = true`.
fn scrape_hardstuck(
    client: &reqwest::blocking::Client,
    today: &str,
    known: &HashMap<String, BenchmarkBuild>,
    should_cancel: &dyn Fn() -> bool,
    on_progress: &dyn Fn(&str, &str),
) -> Result<(Vec<BenchmarkBuild>, bool, usize), String> {
    let mut last_html: Option<String> = None;
    let mut all_links: Vec<(String, String)> = Vec::new();

    on_progress("hardstuck", "listing builds…");
    // Each profession page lists builds for that profession
    for profession in HS_PROFESSIONS {
        if should_cancel() {
            return Ok((Vec::new(), true, 0));
        }
        let prof_url = format!("https://hardstuck.gg/gw2/builds/{}/", profession);
        on_progress("hardstuck", &format!("listing {}…", profession));
        let Ok(html) = fetch_html(client, &prof_url) else {
            continue;
        };
        last_html = Some(html.clone());
        // Build links: href="/gw2/builds/{profession}/{slug}/" with a non-empty slug
        // slug can be numeric (24929) or text (blood-harbinger)
        let links = extract_build_links(&html, &format!("/gw2/builds/{}/", profession), usize::MAX);
        for link in links {
            let parts: Vec<&str> = link.trim_matches('/').split('/').collect();
            // /gw2/builds/{profession}/{slug} = exactly 4 segments
            if parts.len() == 4 && !parts[3].is_empty() && !parts[3].contains('?') {
                // Pin to the real host: an index page (compromised, or MITM'd if
                // TLS were ever bypassed) must not steer us to arbitrary absolute
                // URLs — only https://hardstuck.gg and relative paths may be followed.
                let Some(full) = pin_hardstuck_href(&link) else {
                    continue;
                };
                if !all_links.iter().any(|(_, seen)| *seen == full) {
                    all_links.push((title_case(profession), full));
                }
            }
        }
    }

    if all_links.is_empty() {
        // Distinguish "we were served a block page / wrong content" from
        // "the site genuinely listed no builds" — the former is almost always a
        // network filter (e.g. a UniFi/UDMPro content block on hardstuck.gg) and
        // needs an allowlist change, not a code fix.
        if last_html
            .as_deref()
            .map(looks_like_blocked_page)
            .unwrap_or(false)
        {
            return Err(
                "Hardstuck: request was intercepted by a network block/filter page \
                 (the domain appears restricted by your gateway/firewall — allowlist \
                 hardstuck.gg, e.g. on your UniFi/UDMPro content filter)"
                    .into(),
            );
        }
        return Err("Hardstuck: no build links found on any profession page".into());
    }

    let mut builds = Vec::new();
    let mut failed = 0usize;
    let total = all_links.len();
    on_progress("hardstuck", &format!("0/{}", total));
    // Every build, one profession page at a time.
    for (i, (class, url)) in all_links.into_iter().enumerate() {
        if should_cancel() {
            return Ok((builds, true, failed));
        }
        if let Some(b) = known.get(&url) {
            builds.push(b.clone());
            on_progress("hardstuck", &format!("{class} {}/{total}", i + 1));
            continue;
        }
        if i > 0 && !pace(should_cancel) {
            return Ok((builds, true, failed));
        }
        match scrape_hardstuck_build(client, &url, today) {
            Ok(b) => builds.push(b),
            Err(_) => failed += 1,
        }
        on_progress("hardstuck", &format!("{class} {}/{total}", i + 1));
    }
    Ok((builds, false, failed))
}

fn scrape_hardstuck_build(
    client: &reqwest::blocking::Client,
    url: &str,
    today: &str,
) -> Result<BenchmarkBuild, String> {
    let html = fetch_html(client, url)?;

    // URL: /gw2/builds/{profession}/{slug}/
    // slug like "blood-harbinger" or "24929"
    let url_parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    let profession_raw = url_parts
        .get(url_parts.len().saturating_sub(2))
        .copied()
        .unwrap_or("");
    let slug = url_parts.last().copied().unwrap_or("");
    let profession = title_case(&profession_raw.replace('-', " "));

    // Spec name from slug (same approach as Snowcrows)
    let spec_name = extract_spec_from_slug(slug);

    // Mode and role as the page states them, never scanned out of its text.
    // Both used to be `html.contains(..)` over the whole page, and both were
    // wrong nearly always: every Hardstuck page names all three modes in its
    // filter nav, so the mode scan answered PvP for all eleven pages
    // measured while the page's own class said six PvE, four PvP and one
    // WvW — hence nine `hardstuck_*_pvp.json` files and no PvE or WvW ones.
    // The role scan matched "condi" on nearly every page, giving 142 of 157
    // stored rows the role "Condi DPS".
    let (mode, scale) = crate::providers::hardstuck::mode_and_scale(&html)
        .or_else(|| crate::providers::hardstuck::game_mode(&html).map(|mode| (mode, "")))
        .unwrap_or(("PvE", ""));
    let name = crate::providers::hardstuck::build_name(&html).unwrap_or_default();
    // The name first, because it distinguishes power from condition from
    // heal where Hardstuck's own tag says only "Damage". Its tag second,
    // because it is the only thing that names the job on a build whose title
    // does not: "Blood Harbinger" is a Bruiser and nothing in that name says
    // so. 20 of 148 rows recorded no role at all before this.
    let job = crate::providers::role_in_name(&name)
        .map(str::to_string)
        .or_else(|| crate::providers::hardstuck::build_role(&html))
        .unwrap_or_default();
    let role = crate::providers::role_label(scale, &job);

    Ok(benchmark_from_html(
        &html,
        url,
        today,
        "hardstuck",
        profession,
        spec_name,
        mode,
        &role,
    ))
}

// GuildJen

/// Scrape GuildJen (WvW/PvP builds).
/// Index: https://guildjen.com/
///
/// `should_cancel` is consulted before every outer (per-index-page) and inner
/// (per-build) HTTP fetch. On cancellation, returns whatever builds were
/// collected so far paired with `cancelled = true`.
fn scrape_guildjen(
    client: &reqwest::blocking::Client,
    today: &str,
    known: &HashMap<String, BenchmarkBuild>,
    should_cancel: &dyn Fn() -> bool,
    on_progress: &dyn Fn(&str, &str),
) -> Result<(Vec<BenchmarkBuild>, bool, usize), String> {
    // The six category pages linked from the hub at /gw2-builds/, verified
    // against the live site 2026-09-05. The old `/wvw-builds/` and
    // `/pvp-builds/` addresses no longer carry the build tables, which is why
    // a sync reported "no build links found on any index page".
    // Read the category list off the hub instead of hardcoding it: GuildJen
    // adds and retires categories as well as builds, and the last hardcoded
    // pair (`/wvw-builds/`, `/pvp-builds/`) silently went stale and returned
    // nothing. The known three are only the fallback for a hub that will not
    // load.
    // Before the first request, not after: a sync cancelled at the door
    // should touch the network zero times, and fetch_html now retries with
    // backoff, so a missed check here costs seconds of dead waiting.
    if should_cancel() {
        return Ok((Vec::new(), true, 0));
    }
    on_progress("guildjen", "listing categories…");
    let index_urls = match fetch_html(client, GUILDJEN_SITEMAP) {
        Ok(sitemap) => {
            let found = guildjen_category_pages(&sitemap);
            if found.is_empty() {
                on_progress("guildjen", "sitemap listed no categories; using known set");
                guildjen_fallback_categories()
            } else {
                found
            }
        }
        Err(_) => {
            on_progress("guildjen", "sitemap unreachable; using known set");
            guildjen_fallback_categories()
        }
    };

    let mut builds = Vec::new();
    let mut failed = 0usize;
    let mut last_html: Option<String> = None;
    let mut any_success = false;
    let mut saw_link = false;

    for (index_url, mode) in &index_urls {
        if should_cancel() {
            return Ok((builds, true, failed));
        }
        let mode = *mode;
        // Name the category, not just the mode. Raid, fractal and open world
        // are all "PvE", and each index restarts the count with its own
        // total, so three PvE categories in a row read as one list whose
        // total kept changing - 53/67 then 8/48 is two pages, not a bug.
        let category = category_label(index_url);
        on_progress("guildjen", &format!("listing {category}…"));
        let Ok(html) = fetch_html(client, index_url) else {
            continue;
        };
        last_html = Some(html.clone());
        any_success = true;
        // Only the rows of the build tables. Every page also carries "Trending"
        // and "Popular Posts" sidebars holding build links from OTHER
        // categories — the WvW "Power Reaper Roaming Build" appears on the PvP
        // page — so scanning the whole document would file builds under the
        // wrong game mode.
        // Every build the table lists, a class at a time. Taking the top 15
        // of a 99-row page bought Elementalist and part of Necromancer and
        // left the other seven professions with no reference at all -
        // measured 2026-09-05, where a Necromancer in WvW was told "No
        // benchmark data available" while the store held Guardian, Mesmer,
        // Revenant, Thief and Warrior.
        // The index states the profession, role and playstyle of every build
        // it lists, so nothing below has to infer them from a slug or from
        // the body text of the build page.
        let by_class = group_rows_by_profession(crate::providers::guildjen::index_rows(&html));
        let cap: usize = by_class.iter().map(|(_, l)| l.len()).sum();
        let mut i = 0usize;

        for (class, class_rows) in by_class {
            for row in class_rows {
                if should_cancel() {
                    return Ok((builds, true, failed));
                }
                // Pin to the real host: an index page (compromised, or
                // MITM'd if TLS were ever bypassed) must not steer us to
                // arbitrary absolute URLs — only relative paths on
                // guildjen.com may be followed.
                let link = row.url.clone();
                let url = if link.starts_with("https://guildjen.com/") {
                    link
                } else if link.starts_with("http") {
                    continue;
                } else {
                    format!("https://guildjen.com{}", link)
                };
                saw_link = true;
                // Already read today: take it and skip both the request and
                // the wait that would have come with it. The mode is the
                // index page's to say, not the stored copy's - the same
                // build is listed under more than one category.
                if let Some(known_build) = known.get(&url) {
                    let mut b = known_build.clone();
                    b.mode = mode.to_string();
                    builds.push(b);
                    i += 1;
                    on_progress("guildjen", &format!("{category} {class} {i}/{cap}"));
                    continue;
                }
                if i > 0 && !pace(should_cancel) {
                    return Ok((builds, true, failed));
                }
                match scrape_guildjen_build(client, &url, &row, mode, &category, today) {
                    Ok(mut b) => {
                        b.mode = mode.to_string();
                        builds.push(b);
                    }
                    Err(_) => failed += 1,
                }
                i += 1;
                on_progress("guildjen", &format!("{category} {class} {i}/{cap}"));
            }
        }
    }

    if !any_success {
        return Err("GuildJen: failed to fetch any index pages".to_string());
    }
    if !saw_link {
        if last_html
            .as_deref()
            .map(looks_like_blocked_page)
            .unwrap_or(false)
        {
            return Err(
                "GuildJen: request was intercepted by a network block/filter page \
                 (the domain appears restricted by your gateway/firewall — allowlist \
                 guildjen.com, e.g. on your UniFi/UDMPro content filter)"
                    .into(),
            );
        }
        return Err("GuildJen: no build links found on any index page".into());
    }
    Ok((builds, false, failed))
}

fn scrape_guildjen_build(
    client: &reqwest::blocking::Client,
    url: &str,
    row: &crate::providers::guildjen::IndexRow,
    mode: &str,
    category: &str,
    today: &str,
) -> Result<BenchmarkBuild, String> {
    let html = fetch_html(client, url)?;

    // The slug names the specialization, never the profession path — GuildJen
    // build pages live at the site root. A slug that names neither, and that
    // the index did not file under a profession either, is not a build page
    // we can store.
    let slug = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let from_slug = profession_from_slug(slug);
    let profession = match (&row.profession, &from_slug) {
        (stated, _) if !stated.is_empty() => stated.clone(),
        (_, Some((profession, _))) => profession.clone(),
        _ => return Err(format!("GuildJen: no profession for {slug}")),
    };
    let spec_name = from_slug
        .map(|(_, spec)| spec)
        .unwrap_or_else(|| extract_spec_from_slug(slug));

    // The role used to be a text scan for "roam"/"zerg"/"support" over the
    // whole page, with every branch — including the fallback — returning a
    // WvW label. All 411 stored GuildJen builds read "WvW Roaming",
    // Elementalist PvE ones included. The index states it instead.
    let role = crate::providers::role_label(
        &guildjen_scale(mode, category, row),
        &guildjen_job(mode, row),
    );

    Ok(benchmark_from_html(
        &html, url, today, "guildjen", profession, spec_name, mode, &role,
    ))
}

/// Where a GuildJen build is played, in the site's own words.
///
/// PvE is split by the index it was listed on — open world, fractal, raid —
/// which is the solo, group and squad distinction. WvW is split by the
/// site's playstyle instead, since all of it is one index.
fn guildjen_scale(
    mode: &str,
    category: &str,
    row: &crate::providers::guildjen::IndexRow,
) -> String {
    match mode {
        "PvE" => category.to_string(),
        // A WvW build commonly carries two: `havoc` and `cloud` together.
        // Smallest first, because that is the one that constrains the build
        // — a havoc build can join a cloud, not the other way round.
        "WvW" => ["roaming", "havoc", "cloud"]
            .iter()
            .find(|wanted| row.playstyles.iter().any(|had| had == *wanted))
            .map(|found| crate::providers::title_case(found))
            .unwrap_or_default(),
        // PvP is always five a side; there is no scale to state.
        _ => String::new(),
    }
}

/// What a GuildJen build does, from whichever source says more in that mode.
///
/// Measured across all five indexes: PvE's role tags are only `dps`,
/// `support`, `tank`, `kiter` and `quickness`, and most open-world rows
/// carry none at all — so in PvE the build's own name is the better source,
/// and "Heal DPS Luminary" is a Healer where the tag says only `support`.
///
/// WvW and PvP are the other way round. Their tags are rich — bruiser,
/// assassin, medic, roamer, duelist — and the name is not: reading it would
/// call a "Celestial Spear Antiquary" a Hybrid rather than the Bruiser the
/// site says it is.
///
/// Whichever comes second is the fallback, and a row with neither falls back
/// to the playstyle, which on the open-world index is the distinction the
/// site actually draws.
fn guildjen_job(mode: &str, row: &crate::providers::guildjen::IndexRow) -> String {
    let named = crate::providers::role_in_name(&row.name).map(str::to_string);
    let tagged = row.roles.first().map(|role| match role.as_str() {
        // Not a word to title-case, and on its own it does not say which
        // kind of damage — so the name wins where there is one.
        "dps" => "DPS".to_string(),
        other => crate::providers::title_case(other),
    });
    let (first, second) = if mode == "PvE" {
        (named, tagged)
    } else {
        (tagged, named)
    };
    first
        .or(second)
        .or_else(|| {
            row.playstyles
                .first()
                .map(|playstyle| crate::providers::title_case(playstyle))
        })
        .unwrap_or_default()
}

/// Group index rows by the profession the index states, first-seen order.
///
/// The sync walks a class at a time, so progress names the class it is on
/// and a cancel leaves whole classes done rather than a slice of each.
///
/// A row the index files under no profession falls back to the slug, and one
/// that names neither is dropped here rather than fetched and rejected
/// later: that is a request spent on nothing.
fn group_rows_by_profession(
    rows: Vec<crate::providers::guildjen::IndexRow>,
) -> Vec<(String, Vec<crate::providers::guildjen::IndexRow>)> {
    let mut buckets: Vec<(String, Vec<crate::providers::guildjen::IndexRow>)> = Vec::new();
    for row in rows {
        let profession = if !row.profession.is_empty() {
            row.profession.clone()
        } else {
            let slug = row
                .url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("");
            match profession_from_slug(slug) {
                Some((profession, _)) => profession,
                None => continue,
            }
        };
        match buckets.iter_mut().find(|(name, _)| *name == profession) {
            Some((_, links)) => links.push(row),
            None => buckets.push((profession, vec![row])),
        }
    }
    buckets
}

// HTML extraction helpers

/// Extra milliseconds added to every gap after the site pushes back.
///
/// The point of a sync is to finish. Retrying a 429 three times at a fixed
/// backoff and then carrying on at the old rate is how a soft "slow down"
/// becomes a hard block: the site asked, we did not listen, and the next
/// several hundred requests arrive at the same cadence. This grows on every
/// throttling signal and decays on success, so a run that meets resistance
/// slows down and keeps going instead of stopping.
///
/// One sync runs at a time, so a process-wide cell is enough and keeps the
/// signal out of three scrapers' signatures.
static THROTTLE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// First slowdown step, and the floor once any pushback is seen.
const THROTTLE_STEP_MS: u64 = 4_000;
/// Ceiling, so a site that refuses everything cannot wedge the run at an
/// unbounded delay. Past this the run is failing anyway and should say so.
const THROTTLE_CEILING_MS: u64 = 30_000;

/// How much the sync is currently holding back, in milliseconds.
///
/// Zero when the run is at its normal pace. Non-zero means the site asked us
/// to slow down and we are obliging - the run has not failed and has not
/// stopped, it is waiting, and the player deserves to be told that rather
/// than watching a bar that appears stuck.
pub fn sync_backoff_ms() -> u64 {
    THROTTLE_MS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Note that the site pushed back, and slow everything down from here.
fn throttle_up() {
    use std::sync::atomic::Ordering;
    let current = THROTTLE_MS.load(Ordering::Relaxed);
    let next = (current * 2).clamp(THROTTLE_STEP_MS, THROTTLE_CEILING_MS);
    THROTTLE_MS.store(next, Ordering::Relaxed);
}

/// Ease back off after a page comes through cleanly. Three-quarters rather
/// than straight to zero: one success after a 429 is not proof the pressure
/// is gone, and sprinting again is what earns the next one.
fn throttle_down() {
    use std::sync::atomic::Ordering;
    let current = THROTTLE_MS.load(Ordering::Relaxed);
    if current > 0 {
        THROTTLE_MS.store(current * 3 / 4, Ordering::Relaxed);
    }
}

/// Gap between build pages, in milliseconds, picked fresh each time.
///
/// One steady trickle rather than bursts with rests: a burst-and-pause shape
/// is itself a pattern, and a request every second or two is both gentler on
/// the site and less distinctive than five-at-once repeated eighty times.
const PACE_MS: (u64, u64) = (800, 2_200);

/// Wait before the next build page, and report whether to keep going.
///
/// A full sync is several hundred pages. Fetched back to back that is
/// unmistakably a script, and GuildJen already answers traffic it dislikes
/// with a block page - see [`looks_like_blocked_page`], which exists because
/// that happened.
///
/// The gap is jittered because an exactly regular cadence is itself the
/// signature: 1.000s four hundred times over is more obviously automated than
/// no delay at all. Jitter comes from the clock rather than a `rand`
/// dependency - this needs irregularity, not randomness, and nothing here is
/// security sensitive.
///
/// Sleeps in slices so Cancel still lands promptly. `false` means cancelled.
fn pace(should_cancel: &dyn Fn() -> bool) -> bool {
    let (lo, hi) = PACE_MS;
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let extra = THROTTLE_MS.load(std::sync::atomic::Ordering::Relaxed);
    let mut left = std::time::Duration::from_millis(lo + jitter % (hi - lo) + extra);
    let slice = std::time::Duration::from_millis(100);
    while !left.is_zero() {
        if should_cancel() {
            return false;
        }
        let step = slice.min(left);
        std::thread::sleep(step);
        left -= step;
    }
    true
}

/// Statuses worth asking again for. 429 is the site saying "slower", not
/// "no"; 5xx and 408 are the upstream having a moment. 403 is in the set
/// because a bot filter answers with one and that IS transient - the whole
/// point is that the run recovers rather than losing every remaining page.
/// 404 is an answer, and asking twice will not change it.
fn retryable_status(status: u16) -> bool {
    matches!(status, 403 | 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Statuses that mean "you are going too fast", as opposed to "I am unwell".
fn throttling_status(status: u16) -> bool {
    matches!(status, 403 | 429 | 503)
}

/// Longest `Retry-After` honoured inside a single fetch.
///
/// This wait cannot poll cancellation, so it stays short; anything the site
/// asks for beyond it is handed to [`THROTTLE_MS`], which spreads the wait
/// across later gaps where Cancel does land.
const MAX_RETRY_AFTER_MS: u64 = 15_000;

/// The site's own `Retry-After`, in milliseconds, when it gave one in seconds.
fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|secs| secs.saturating_mul(1_000))
}

/// Attempts per page when something went wrong at the transport.
const FETCH_ATTEMPTS: u32 = 3;
/// Attempts per page when the site is rate limiting.
///
/// Higher than [`FETCH_ATTEMPTS`] on purpose. A rate limit is a wait, not a
/// refusal: the page is there and the site has told us when to come back for
/// it. Giving up after three tries turns a delay into a missing build, and a
/// stretch of 429s into a sync that quietly returns half the data. With the
/// run-wide slowdown widening every gap at the same time, these attempts are
/// spread over minutes, not hammered.
const THROTTLED_ATTEMPTS: u32 = 8;

/// One page, retried through the failures a long sync actually hits.
///
/// A full sync is several hundred requests spread over many minutes. At that
/// length a dropped connection or a single 429 is ordinary, and every caller
/// here discards a failure silently (`if let Ok(b) = ...`), so one blip used
/// to cost a build with nothing said about it. Backoff grows and is jittered,
/// for the same reason the pacing is.
fn fetch_html(client: &reqwest::blocking::Client, url: &str) -> Result<String, String> {
    let mut last = String::new();
    let mut wait_ms = 0u64;
    // Raised the moment the site says "slower": that answer means the page
    // exists and we asked too soon, so the sync waits it out rather than
    // dropping the build.
    let mut attempts = FETCH_ATTEMPTS;
    let mut attempt = 0;
    while attempt < attempts {
        if attempt > 0 {
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::from(d.subsec_nanos()) % 1_500)
                .unwrap_or(0);
            // The site's own number when it gave one, our growing backoff
            // otherwise. Asking again sooner than it said is how a soft
            // "slow down" turns into a hard "no".
            let base = wait_ms.max(2_000 * u64::from(attempt));
            std::thread::sleep(std::time::Duration::from_millis(base + jitter));
        }
        match fetch_html_once(client, url) {
            Ok(html) => {
                throttle_down();
                return Ok(html);
            }
            Err(FetchFailure {
                retryable,
                throttled,
                retry_after_ms,
                message,
            }) => {
                last = message;
                if throttled {
                    throttle_up();
                    attempts = THROTTLED_ATTEMPTS;
                }
                // Beyond what one fetch can wait for, the remainder lands on
                // the run-wide gap, where Cancel still works.
                wait_ms = retry_after_ms.unwrap_or(0).min(MAX_RETRY_AFTER_MS);
                if !retryable {
                    break;
                }
            }
        }
        attempt += 1;
    }
    Err(last)
}

/// Why one attempt failed, and what to do about it.
struct FetchFailure {
    /// Worth another attempt at this URL.
    retryable: bool,
    /// The site asked us to slow down, so the whole run should.
    throttled: bool,
    /// What it asked for, if it said.
    retry_after_ms: Option<u64>,
    message: String,
}

impl FetchFailure {
    fn permanent(message: String) -> Self {
        Self {
            retryable: false,
            throttled: false,
            retry_after_ms: None,
            message,
        }
    }

    /// A transport failure: dropped connection, DNS hiccup, timeout. Worth
    /// another try, but not a sign we are being too quick.
    fn transient(message: String) -> Self {
        Self {
            retryable: true,
            throttled: false,
            retry_after_ms: None,
            message,
        }
    }
}

/// One attempt, classified so [`fetch_html`] can tell "ask again" from "no"
/// and "you are going too fast" from "I am unwell".
fn fetch_html_once(client: &reqwest::blocking::Client, url: &str) -> Result<String, FetchFailure> {
    let resp = client
        .get(url)
        .send()
        .map_err(|e| FetchFailure::transient(format!("HTTP error fetching {}: {}", url, e)))?;

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        return Err(FetchFailure {
            retryable: retryable_status(code),
            throttled: throttling_status(code),
            retry_after_ms: retry_after_ms(resp.headers()),
            message: format!("HTTP {} fetching {} (expected 2xx)", code, url),
        });
    }

    // Cap the body: a hostile endpoint must not stream unbounded bytes into
    // the game process. Build pages are well under 1 MiB.
    let bytes = gw2_api::transport::read_body_capped(resp, 2 * 1024 * 1024)
        .map_err(|e| FetchFailure::transient(format!("error reading {}: {}", url, e)))?;
    let html = String::from_utf8(bytes)
        .map_err(|_| FetchFailure::permanent(format!("UTF-8 error reading {}", url)))?;

    // A filter that answers 200 with an interstitial is still a refusal, and
    // reading it as a build page would file its text as a benchmark.
    if looks_rate_limited(&html) {
        return Err(FetchFailure {
            retryable: true,
            throttled: true,
            retry_after_ms: None,
            message: format!("rate-limit or challenge page returned for {}", url),
        });
    }
    Ok(html)
}

/// Whether a 200 body is an anti-bot interstitial rather than the page.
///
/// Cloudflare and friends answer with a normal status and a challenge, so
/// status alone does not see this. Kept narrow and paired with a length
/// check: a real build page mentioning "rate limit" in prose is long, an
/// interstitial is not.
fn looks_rate_limited(html: &str) -> bool {
    if html.len() > 8_192 {
        return false;
    }
    let lower = html.to_lowercase();
    [
        "rate limit",
        "too many requests",
        "checking your browser",
        "cf-browser-verification",
        "attention required",
        "unusual traffic",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Heuristic: does this look like a network/security interstitial (block page,
/// captive portal, WAF challenge) rather than the real site content?
///
/// These pages return HTTP 200 with a body that mentions being blocked, so a
/// status check alone won't catch them. We only flag high-confidence markers to
/// avoid false positives on legitimate build pages.
fn looks_like_blocked_page(html: &str) -> bool {
    let lower = html.to_lowercase();
    // "blocked" + a gateway/filter vendor or "restricted"/"administrator" cue.
    let mentions_blocked = lower.contains("blocked because")
        || lower.contains("access denied")
        || lower.contains("this site is blocked")
        || lower.contains("domain is restricted");
    let mentions_gateway = lower.contains("ubiquiti")
        || lower.contains("unifi")
        || lower.contains("contact your administrator")
        || lower.contains("content filter");
    (mentions_blocked && mentions_gateway) || lower.contains("domain is restricted")
}

/// Extract hrefs containing `needle` from anchor tags in HTML.
/// GuildJen's build hub. Every category index is linked from here.
/// The site's own page index, and the only ungated list of its categories.
///
/// Not the build hub at `/gw2-builds/`: that page renders its category grid
/// inside a consent-gated embed, so a fetch without marketing cookies gets
/// prose, social links and one off-site card. Measured 2026-09-05: the
/// rendered hub carried 41 anchors and not one category among them, so
/// discovery found nothing and every sync silently ran on the fallback list.
/// The sitemap is served by Yoast, needs no consent, and is what the site
/// publishes for machines to read.
const GUILDJEN_SITEMAP: &str = "https://guildjen.com/page-sitemap.xml";
/// The hub, excluded from discovery: it is a `-builds` page that lists no
/// builds of its own.
const GUILDJEN_HUB: &str = "https://guildjen.com/gw2-builds/";

/// The categories to fall back on when the sitemap cannot be read.
///
/// Every build category the sitemap listed on 2026-09-05, minus the hub and
/// levelling. WvW and PvP are GuildJen's own beat and nothing else in this
/// scraper covers them; raid, fractal and open world are PvE.
fn guildjen_fallback_categories() -> Vec<(String, &'static str)> {
    [
        "gw2-wvw-builds",
        "gw2-pvp-builds",
        "gw2-open-world-builds",
        "gw2-raid-builds",
        "gw2-fractal-builds",
    ]
    .iter()
    .map(|slug| (format!("https://guildjen.com/{slug}/"), category_mode(slug)))
    .collect()
}

/// Short display name for a category index, from its slug.
///
/// `/gw2-open-world-builds/` reads as "Open World". The mode alone is not
/// enough: raid, fractal and open world are all PvE, so progress lines from
/// three different indexes were indistinguishable.
fn category_label(url: &str) -> String {
    let slug = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let words = slug
        .trim_start_matches("gw2-")
        .trim_end_matches("-builds")
        .replace('-', " ");
    if words.is_empty() {
        return "Builds".to_string();
    }
    words
        .split(' ')
        .map(title_case)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Game mode from a category slug. Everything that is neither WvW nor PvP is
/// PvE: raids, fractals and open world all share the PvE dummy.
fn category_mode(slug: &str) -> &'static str {
    if slug.contains("wvw") {
        "WvW"
    } else if slug.contains("pvp") {
        "PvP"
    } else {
        "PvE"
    }
}

/// Every build category the sitemap lists, paired with its game mode.
///
/// Category pages are root-level slugs of the form `/gw2-<name>-builds/`. The
/// hub itself is excluded - it lists no builds - as is the levelling category,
/// whose sub-80 kits are not valid references for a level-80 comparison.
fn guildjen_category_pages(sitemap_xml: &str) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let mut pos = 0;
    while let Some(loc_start) = find_ci(sitemap_xml, "<loc>", pos) {
        let abs = loc_start + 5;
        let Some(close) = find_ci(sitemap_xml, "</loc>", abs) else {
            break;
        };
        let href = sitemap_xml[abs..close].trim();
        pos = close + 6;

        let Some(path) = href
            .strip_prefix("https://guildjen.com")
            .or_else(|| href.strip_prefix("http://guildjen.com"))
            .or_else(|| (!href.starts_with("http")).then_some(href))
        else {
            continue;
        };
        let slug = path.trim_matches('/');
        if slug.contains('/')
            || !slug.starts_with("gw2-")
            || !slug.ends_with("-builds")
            || slug == "gw2-builds"
            || slug.contains("leveling")
        {
            continue;
        }
        let url = format!("https://guildjen.com/{slug}/");
        if url == GUILDJEN_HUB || out.iter().any(|(seen, _)| *seen == url) {
            continue;
        }
        out.push((url, category_mode(slug)));
    }
    out
}

/// Case-insensitive `str::find` for ASCII needles, from a byte offset.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let pat = needle.as_bytes();
    if pat.is_empty() || bytes.len() < pat.len() {
        return None;
    }
    (from..=bytes.len() - pat.len()).find(|&i| bytes[i..i + pat.len()].eq_ignore_ascii_case(pat))
}

fn extract_build_links(html: &str, needle: &str, max: usize) -> Vec<String> {
    let mut links = Vec::new();
    let mut pos = 0;
    while let Some(href_start) = html[pos..].find("href=\"") {
        let abs = pos + href_start + 6;
        if let Some(end) = html[abs..].find('"') {
            let href = &html[abs..abs + end];
            if href.contains(needle) && !links.contains(&href.to_string()) {
                links.push(href.to_string());
                if links.len() >= max {
                    break;
                }
            }
            pos = abs + end + 1;
        } else {
            break;
        }
    }
    links
}

/// Every elite specialization, with the profession that owns it.
///
/// This is the only reliable way to name the profession of a GuildJen build:
/// its URLs are flat slugs on the site root (`/power-hammer-luminary-roaming-build/`)
/// and carry no profession path segment at all. Reading one out of the path is
/// what produced benchmark entries whose profession was `Guildjen.com`, `Fonts`
/// or `1.0` (measured on the synced cache 2026-09-05), none of which could ever
/// match a real character.
const SPEC_PROFESSIONS: &[(&str, &str)] = &[
    ("dragonhunter", "Guardian"),
    ("firebrand", "Guardian"),
    ("willbender", "Guardian"),
    ("luminary", "Guardian"),
    ("berserker", "Warrior"),
    ("spellbreaker", "Warrior"),
    ("bladesworn", "Warrior"),
    ("paragon", "Warrior"),
    ("scrapper", "Engineer"),
    ("holosmith", "Engineer"),
    ("mechanist", "Engineer"),
    ("amalgam", "Engineer"),
    ("druid", "Ranger"),
    ("soulbeast", "Ranger"),
    ("untamed", "Ranger"),
    ("galeshot", "Ranger"),
    ("daredevil", "Thief"),
    ("deadeye", "Thief"),
    ("specter", "Thief"),
    ("antiquary", "Thief"),
    ("tempest", "Elementalist"),
    ("weaver", "Elementalist"),
    ("catalyst", "Elementalist"),
    ("evoker", "Elementalist"),
    ("chronomancer", "Mesmer"),
    ("mirage", "Mesmer"),
    ("virtuoso", "Mesmer"),
    ("troubadour", "Mesmer"),
    ("reaper", "Necromancer"),
    ("scourge", "Necromancer"),
    ("harbinger", "Necromancer"),
    ("ritualist", "Necromancer"),
    ("herald", "Revenant"),
    ("renegade", "Revenant"),
    ("vindicator", "Revenant"),
    ("conduit", "Revenant"),
];

/// The nine core professions, for slugs that name one directly
/// (`power-core-guardian-roaming-build`).
const CORE_PROFESSIONS: &[&str] = &[
    "guardian",
    "warrior",
    "engineer",
    "ranger",
    "thief",
    "elementalist",
    "mesmer",
    "necromancer",
    "revenant",
];

/// Known GW2 elite spec names for slug extraction.
const KNOWN_SPECS: &[&str] = &[
    "firebrand",
    "willbender",
    "dragonhunter",
    "berserker",
    "spellbreaker",
    "bladesworn",
    "scrapper",
    "mechanist",
    "holosmith",
    "soulbeast",
    "untamed",
    "druid",
    "daredevil",
    "specter",
    "deadeye",
    "weaver",
    "tempest",
    "catalyst",
    "chronomancer",
    "virtuoso",
    "mirage",
    "scourge",
    "harbinger",
    "reaper",
    "renegade",
    "vindicator",
    "herald",
    "luminary",
];

/// Name the profession a build slug belongs to.
///
/// Elite spec first, because "power-core-guardian" and "condition-firebrand"
/// both name a Guardian but only one of them says so. Returns the profession
/// and the spec name to display; a core build reports its profession as both.
fn profession_from_slug(slug: &str) -> Option<(String, String)> {
    let lower = slug.to_lowercase();
    for (spec, profession) in SPEC_PROFESSIONS {
        if lower.contains(spec) {
            return Some(((*profession).to_string(), title_case(spec)));
        }
    }
    for profession in CORE_PROFESSIONS {
        if lower.contains(profession) {
            return Some((title_case(profession), title_case(profession)));
        }
    }
    None
}

/// Extract elite spec name from a Snowcrows URL slug.
/// e.g. "power-dragonhunter-virtues-longbow-greatsword" → "Dragonhunter"
fn extract_spec_from_slug(slug: &str) -> String {
    let lower = slug.to_lowercase();
    for spec in KNOWN_SPECS {
        if lower.contains(spec) {
            return title_case(spec);
        }
    }
    // Fallback: take the second hyphen-segment (after the role prefix)
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() >= 2 {
        title_case(parts[1])
    } else {
        title_case(slug)
    }
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Space-padded alnum words — same boundary idea as `prefix_named_in_text`.
fn padded_alnum_words(text: &str) -> String {
    format!(
        " {} ",
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    ' '
                }
            })
            .collect::<String>()
    )
}

/// Extract gear stat prefix from HTML text.
///
/// Case-insensitive — scraped sites vary in casing (e.g. "viper's", "VIPER'S"),
/// and a case-sensitive `contains` previously silently dropped those.
fn extract_gear_prefix(html: &str) -> String {
    // Common gear prefixes — match in order of specificity
    let prefixes = [
        "Trailblazer's",
        "Plaguedoctor's",
        "Ritualist's",
        "Apothecary's",
        "Diviner's",
        "Celestial",
        "Minstrel's",
        "Harrier's",
        "Berserker's",
        "Viper's",
        "Sinister",
        "Grieving",
        "Marauder",
        "Dragon's",
        "Valkyrie",
        "Trailblazer",
        "Assassin's",
        "Knight's",
        "Nomad's",
        "Soldier's",
        "Cavalier's",
        "Dire",
        "Magi's",
        "Cleric's",
    ];
    let hay = padded_alnum_words(html);
    for p in &prefixes {
        let stem = p.trim_end_matches("'s").to_ascii_lowercase();
        if hay.contains(&format!(" {stem} ")) || hay.contains(&format!(" {stem}s ")) {
            return p.to_string();
        }
    }
    String::new()
}

/// The sources this addon scrapes. A file naming any other is not ours.
const KNOWN_SOURCES: &[&str] = &["snowcrows", "hardstuck", "guildjen"];
/// Game modes, lowercased as [`save_builds`] writes them.
const KNOWN_MODES: &[&str] = &["pve", "pvp", "wvw"];

/// Whether a benchmark filename is one the current scraper could have written.
///
/// [`save_builds`] names every file `{source}_{profession}_{mode}.json` with
/// all three lowercased, so anything else in that folder came from a version
/// that filed builds under something that is not a profession.
fn is_current_benchmark_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let parts: Vec<&str> = stem.split('_').collect();
    let [source, profession, mode] = parts[..] else {
        return false;
    };
    KNOWN_SOURCES.contains(&source)
        && CORE_PROFESSIONS.contains(&profession)
        && KNOWN_MODES.contains(&mode)
}

/// Delete benchmark files the current scraper could never have written, and
/// report how many went.
///
/// Until 1.11.30 the GuildJen scraper read the profession out of a URL path
/// segment the site no longer has, so it filed builds under whatever it found
/// there. A real install collected `guildjen_1.0_pvp.json`,
/// `guildjen_comments_wvw.json`, `guildjen_fonts_pvp.json`,
/// `guildjen_pages_pvp.json`, `guildjen_https__wvw.json` and
/// `guildjen_guildjen.com_pvp.json`. Nothing generates those names any more,
/// so nothing will ever overwrite them: they sit in the folder looking like
/// benchmark data for the life of the install, on every machine that ran a
/// sync before the fix. They are swept at the start of each run.
///
/// Only the name is judged, and only against names this code writes. A file
/// that parses as `{known source}_{known profession}_{known mode}` is kept
/// whatever is inside it - deciding a build is stale is a different question
/// from deciding a filename is impossible.
fn prune_stale_benchmarks(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") || is_current_benchmark_file(name) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Builds already on disk from a sync run today, keyed by their page URL.
///
/// A sync of every row is several hundred requests over ten minutes or more,
/// and the reasons to run one twice in a day are ordinary: it was cancelled,
/// the game was closed, a source failed and you want it again. Refetching a
/// page we already read today buys nothing and spends the politeness budget
/// that keeps the sync working at all.
///
/// Same-day is the whole freshness rule, deliberately. Anything longer and a
/// deliberate re-sync would quietly do nothing, which is worse than slow;
/// anything shorter and this does not help. So: running it twice today costs
/// what running it once did, and tomorrow it all refreshes.
fn load_todays_builds(dir: &Path, today: &str) -> HashMap<String, BenchmarkBuild> {
    let mut known = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return known;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_current_benchmark_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(builds) = serde_json::from_str::<Vec<BenchmarkBuild>>(&text) else {
            continue;
        };
        for build in builds {
            // A row is reusable only if it was written today *and* by an
            // extractor that reads ids. Without the second condition the
            // day's own cache defeats the fix: a sync run earlier today
            // wrote rows with no ids, and re-running would copy them
            // straight back to disk without refetching, so the store would
            // only heal after midnight. An id-less row is refetched instead.
            // Prose joins that condition for the same reason. Every real
            // page yields some, so an empty one means the fetch or the parse
            // failed — and a row written by a build that did not capture
            // prose at all must be refetched once, or today's own cache
            // would hold the upgrade back until midnight.
            let reusable = build.scraped_at == today
                && !build.source_url.is_empty()
                && !build.published.is_empty()
                && !build.published.prose.is_empty();
            if reusable {
                known.insert(build.source_url.clone(), build);
            }
        }
    }
    known
}

fn save_builds(builds: &[BenchmarkBuild], dir: &Path) -> Result<(), String> {
    if builds.is_empty() {
        return Ok(());
    }
    // Group by (source, profession, mode)
    let mut groups: std::collections::HashMap<String, Vec<&BenchmarkBuild>> =
        std::collections::HashMap::new();
    for b in builds {
        let key = format!(
            "{}_{}_{}.json",
            sanitize_filename_component(&b.source),
            sanitize_filename_component(&b.profession.to_lowercase().replace(' ', "_")),
            sanitize_filename_component(&b.mode.to_lowercase())
        );
        groups.entry(key).or_default().push(b);
    }
    for (filename, group) in &groups {
        // Filename components can carry scraped, attacker-influenced text
        // (profession parsed from a URL path). Windows treats '\\' as a
        // separator, so whitelist instead of trusting the URL splitter.
        let path = dir.join(sanitize_filename_component(filename));
        let json = serde_json::to_string_pretty(group).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Reduce a string to a safe single path component: keep alphanumerics,
/// '-', '_', '.', ' ' — everything else (including '\\'/''/') becomes '_'.
/// Mirrors the whitelist in crates/core/src/storage.rs.
fn sanitize_filename_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // A component of only dots (e.g. "..") must not survive.
    if cleaned.chars().all(|c| c == '.') {
        "_".repeat(cleaned.len())
    } else {
        cleaned
    }
}

/// Follow a redirect only when the next URL's host matches the original request host.
///
/// `attempt.stop()` leaves the 3xx with `fetch_html`, which then rejects non-2xx.
/// Hop cap matches reqwest's default (`previous[0]` is the original URL).
/// Mockito is a gw2api-only dev-dep; the host check is unit-tested via
/// [`redirect_stays_on_request_host`].
fn same_host_redirect(attempt: reqwest::redirect::Attempt<'_>) -> reqwest::redirect::Action {
    if attempt.previous().len() > 10 {
        return attempt.error("too many redirects");
    }
    let origin = attempt.previous().first().and_then(|u| u.host_str());
    let next = attempt.url().host_str();
    if redirect_stays_on_request_host(origin, next) {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

/// Whether a redirect target stays on the request's host (A16-7).
fn redirect_stays_on_request_host(request_host: Option<&str>, next_host: Option<&str>) -> bool {
    match (request_host, next_host) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn build_client() -> Result<reqwest::blocking::Client, reqwest::Error> {
    // What Firefox sends on a top-level navigation, because that is what
    // [`USER_AGENT`] claims to be. Half a fingerprint is worth less than
    // none: a browser string with no Accept-Language and no Sec-Fetch-Mode
    // is a mismatch any filter can read.
    let mut headers = reqwest::header::HeaderMap::new();
    let mut set = |name: reqwest::header::HeaderName, value: &'static str| {
        headers.insert(name, reqwest::header::HeaderValue::from_static(value));
    };
    set(
        reqwest::header::ACCEPT,
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
         image/webp,image/png,image/svg+xml,*/*;q=0.8",
    );
    set(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.5");
    set(reqwest::header::UPGRADE_INSECURE_REQUESTS, "1");
    set(
        reqwest::header::HeaderName::from_static("sec-fetch-dest"),
        "document",
    );
    set(
        reqwest::header::HeaderName::from_static("sec-fetch-mode"),
        "navigate",
    );
    set(
        reqwest::header::HeaderName::from_static("sec-fetch-site"),
        "none",
    );
    set(
        reqwest::header::HeaderName::from_static("sec-fetch-user"),
        "?1",
    );
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(15))
        // Origin pins at enqueue are end-to-end only if redirects cannot hop hosts.
        .redirect(reqwest::redirect::Policy::custom(same_host_redirect))
        // Certificates must validate (Schannel trust store). The old
        // danger_accept_invalid_certs(true) here let any MITM poison the
        // benchmark cache that feeds optimizer comparisons and LLM prompts.
        .build()
}

fn today_string() -> String {
    // Use chrono so the date is correct. The previous manual approximation
    // (1970 + days/365, day_of_year/30 + 1) ignored leap years and assumed
    // 30-day months — the rendered date drifted up to ~14 days off the real
    // calendar date, which propagated into the scraped benchmark filenames.
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from the live Snowcrows Power Dragonhunter page, 2026-09-23.
    const SNOWCROWS_BENCHMARK_SNIPPET: &str = r#"<div class="md:flex-grow text-left">
<a class="tab" icon="fa-file-chart-column" target="_blank" href="https://dps.report/so5x-20260418-005724_golem">
<i class="fa-solid fa-file-chart-column mr-3"></i>DPS Report</a></div>
<div class="bg-neutral-800/40 p-4 md:p-6 rounded-lg" stat="Last Benchmark Max">
<div class="text-sm text-neutral-400 mb-2">Last Benchmark Max</div>
<div class="text-lg"><i class="fas fa-angle-double-up mr-1"></i>41879</div></div>
<div class="bg-neutral-800/40 p-4 md:p-6 rounded-lg" stat="Last Benchmark Average">
<div class="text-sm text-neutral-400 mb-2">Last Benchmark Average</div>
<div class="text-lg"><i class="fas fa-tilde mr-1"></i>41400</div></div>"#;

    #[test]
    fn snowcrows_page_yields_benchmark_dps_and_log() {
        let row = |source| {
            benchmark_from_html(
                SNOWCROWS_BENCHMARK_SNIPPET,
                "https://snowcrows.com/builds/raids/guardian/x",
                "2026-09-23",
                source,
                "Guardian".into(),
                "Dragonhunter".into(),
                "PvE",
                "",
            )
        };
        let sc = row("snowcrows");
        assert_eq!(sc.benchmark_dps, Some(41879.0));
        assert_eq!(
            sc.log_url.as_deref(),
            Some("https://dps.report/so5x-20260418-005724_golem")
        );
        // Only Snowcrows publishes a benchmark; other readers leave it empty.
        let hs = row("hardstuck");
        assert_eq!((hs.benchmark_dps, hs.log_url), (None, None));
    }

    #[test]
    fn stored_row_without_benchmark_fields_still_loads() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/benchmarks/snowcrows_guardian_pve.json"
        ))
        .expect("fixture");
        let rows: Vec<BenchmarkBuild> = serde_json::from_str(&text).expect("fixture parses");
        assert!(!rows.is_empty());
        assert!(rows
            .iter()
            .all(|b| b.benchmark_dps.is_none() && b.log_url.is_none()));
        // And a row without them serialises without them.
        let json = serde_json::to_string(&rows[0]).expect("serialise");
        assert!(!json.contains("benchmark_dps") && !json.contains("log_url"));
    }

    #[test]
    fn format_source_status_shows_live_progress() {
        let line = format_source_status("Snowcrows (PvE)", 0, Some("12/45"), None);
        assert_eq!(line, "Snowcrows (PvE)  12/45");
    }

    #[test]
    fn format_source_status_shows_down_when_empty_and_errored() {
        let line = format_source_status(
            "GuildJen (WvW/PvP)",
            0,
            None,
            Some("HTTP 404 fetching https://guildjen.com/wvw-builds/"),
        );
        assert!(line.starts_with("GuildJen (WvW/PvP)  down — "));
        assert!(line.contains("HTTP 404"));
    }

    #[test]
    fn format_source_status_shows_count_when_done() {
        let line = format_source_status("Hardstuck", 42, None, None);
        assert_eq!(line, "Hardstuck  42 builds");
    }

    #[test]
    fn format_source_status_dash_when_never_touched() {
        let line = format_source_status("Snowcrows (PvE)", 0, None, None);
        assert_eq!(line, "Snowcrows (PvE)  —");
    }

    #[test]
    fn scrape_all_reports_cancelled_progress_when_cancelled_at_entry() {
        use std::sync::Mutex;

        let events = Mutex::new(Vec::<(String, String)>::new());
        let tmp = std::env::temp_dir().join("gw2_scraper_progress_cancel");
        let _ = scrape_all_with_progress(&tmp, &|| true, &|src, msg| {
            events
                .lock()
                .unwrap()
                .push((src.to_string(), msg.to_string()));
        });
        let ev = events.lock().unwrap();
        assert!(
            ev.iter()
                .any(|(s, m)| s == "snowcrows" && m.contains("cancelled")),
            "expected snowcrows cancelled event, got {:?}",
            *ev
        );
        assert!(
            ev.iter()
                .any(|(s, m)| s == "guildjen" && m.contains("cancelled")),
            "expected guildjen cancelled event, got {:?}",
            *ev
        );
    }

    #[test]
    fn test_extract_build_links_finds_hrefs() {
        let html =
            r#"<a href="/builds/guardian/firebrand">Firebrand</a><a href="/other">Other</a>"#;
        let links = extract_build_links(html, "/builds/", 10);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "/builds/guardian/firebrand");
    }

    /// Only today's builds are reusable, and only from files this version
    /// writes, and only if they carry ids AND prose. Yesterday's have to be
    /// refetched or a re-sync would silently serve stale references forever;
    /// a row from earlier today that is missing either has to be refetched
    /// too, or the day's own cache would keep handing back what the previous
    /// extractor wrote and the upgrade would not land until midnight.
    #[test]
    fn only_todays_builds_are_reused() {
        let tmp = std::env::temp_dir().join(format!(
            "gw2bo-reuse-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).expect("temp dir");

        let build = |url: &str, when: &str| BenchmarkBuild {
            source: "guildjen".into(),
            profession: "Necromancer".into(),
            spec_name: "Scourge".into(),
            mode: "WvW".into(),
            role: "WvW Zerg Support".into(),
            build_code: None,
            gear_prefix: "Minstrel's".into(),
            source_url: url.into(),
            scraped_at: when.into(),
            published: crate::providers::ProviderBuild {
                rune_id: Some(24839),
                prose: "Opener\nFrost Trap\nWeapon Swap\nMaul".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        // What the old extractor wrote: today's date, no ids.
        let id_less = |url: &str| BenchmarkBuild {
            published: crate::providers::ProviderBuild::default(),
            ..build(url, "2026-09-06")
        };
        // Ids but no rotation: written today, before the scrape kept prose.
        let prose_less = |url: &str| BenchmarkBuild {
            published: crate::providers::ProviderBuild {
                prose: String::new(),
                ..build(url, "2026-09-06").published
            },
            ..build(url, "2026-09-06")
        };

        std::fs::write(
            tmp.join("guildjen_necromancer_wvw.json"),
            serde_json::to_string(&vec![
                build("https://guildjen.com/fresh-build/", "2026-09-06"),
                build("https://guildjen.com/stale-build/", "2026-09-05"),
                // No URL is no key: it could never be matched anyway.
                build("", "2026-09-06"),
                // Today's, but written before the parsers existed.
                id_less("https://guildjen.com/written-this-morning/"),
                // Today's, with ids, but written before prose was kept.
                prose_less("https://guildjen.com/no-rotation-yet/"),
            ])
            .unwrap(),
        )
        .expect("seed");

        // An old-format file is not a source of truth even if it parses.
        std::fs::write(
            tmp.join("guildjen_comments_wvw.json"),
            serde_json::to_string(&vec![build(
                "https://guildjen.com/from-junk/",
                "2026-09-06",
            )])
            .unwrap(),
        )
        .expect("seed");

        let known = load_todays_builds(&tmp, "2026-09-06");
        assert_eq!(known.len(), 1, "one reusable entry: {known:?}");
        assert!(known.contains_key("https://guildjen.com/fresh-build/"));
        assert!(!known.contains_key("https://guildjen.com/stale-build/"));
        assert!(!known.contains_key("https://guildjen.com/from-junk/"));
        assert!(
            !known.contains_key("https://guildjen.com/written-this-morning/"),
            "an id-less row is refetched, so the day's own cache cannot \
             keep serving what the old extractor wrote"
        );
        assert!(
            !known.contains_key("https://guildjen.com/no-rotation-yet/"),
            "a row with ids but no prose is refetched too, or the sync that \
             first captures rotations would reuse its way past every page"
        );

        // A folder with nothing in it is not an error, it is a first run.
        let empty = tmp.join("empty");
        std::fs::create_dir_all(&empty).expect("temp dir");
        assert!(load_todays_builds(&empty, "2026-09-06").is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Fixture is the real thing: these are the exact names found in a
    /// player's benchmarks folder on 2026-09-05, written by the scraper
    /// before it stopped reading the profession out of a URL path.
    #[test]
    fn prune_removes_only_what_no_version_still_writes() {
        let tmp = std::env::temp_dir().join(format!(
            "gw2bo-prune-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).expect("temp dir");

        let junk = [
            "guildjen_1.0_pvp.json",
            "guildjen_10_wvw.json",
            "guildjen_comments_pvp.json",
            "guildjen_fonts_wvw.json",
            "guildjen_pages_pvp.json",
            "guildjen_https__wvw.json",
            "guildjen_guildjen.com_pvp.json",
        ];
        let keep = [
            "guildjen_guardian_pve.json",
            "guildjen_necromancer_wvw.json",
            "snowcrows_elementalist_pve.json",
            "hardstuck_revenant_pvp.json",
        ];
        // Not ours, not a benchmark, not touched.
        let bystander = "notes.txt";

        for name in junk.iter().chain(keep.iter()) {
            std::fs::write(tmp.join(name), "[]").expect("seed");
        }
        std::fs::write(tmp.join(bystander), "hello").expect("seed");

        assert_eq!(prune_stale_benchmarks(&tmp), junk.len());
        for name in junk {
            assert!(!tmp.join(name).exists(), "{name} should be gone");
        }
        for name in keep {
            assert!(tmp.join(name).exists(), "{name} must survive");
        }
        assert!(
            tmp.join(bystander).exists(),
            "a non-json file is none of our business"
        );

        // Idempotent: a second run finds nothing left to do.
        assert_eq!(prune_stale_benchmarks(&tmp), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The name test on its own, including the shapes that are nearly right.
    #[test]
    fn only_source_profession_mode_names_are_ours() {
        assert!(is_current_benchmark_file("guildjen_necromancer_wvw.json"));
        assert!(is_current_benchmark_file("snowcrows_guardian_pve.json"));

        // Right shape, wrong middle: this is the whole bug class.
        assert!(!is_current_benchmark_file("guildjen_comments_pvp.json"));
        // A specialization is not a profession; save_builds writes the class.
        assert!(!is_current_benchmark_file("guildjen_scourge_wvw.json"));
        // Unknown source, unknown mode, empty segment, missing segment.
        assert!(!is_current_benchmark_file("metabattle_guardian_pve.json"));
        assert!(!is_current_benchmark_file("guildjen_guardian_raids.json"));
        assert!(!is_current_benchmark_file("guildjen_https__wvw.json"));
        assert!(!is_current_benchmark_file("guildjen_guardian.json"));
        assert!(!is_current_benchmark_file("guildjen_guardian_pve.txt"));
    }

    /// A rate limit is a wait, not a refusal. Everything that means "slower"
    /// has to be retryable AND slow the run down; everything that means "no"
    /// must not be retried forever.
    #[test]
    fn pushback_is_told_apart_from_refusal() {
        // Slow down: retried, and the whole run eases off.
        for code in [403, 429, 503] {
            assert!(retryable_status(code), "{code} must be retried");
            assert!(throttling_status(code), "{code} must slow the run");
        }
        // Upstream having a moment: retried, but we were not the problem.
        for code in [408, 425, 500, 502, 504] {
            assert!(retryable_status(code), "{code} must be retried");
            assert!(!throttling_status(code), "{code} is not about our rate");
        }
        // An answer. Asking again will not change it.
        assert!(!retryable_status(404));
        assert!(!retryable_status(410));
    }

    /// A challenge page arrives with status 200, so only the body gives it
    /// away - and a real build page that happens to say "rate limit" in
    /// prose must not be mistaken for one.
    #[test]
    fn rate_limit_interstitials_are_recognised_by_body() {
        assert!(looks_rate_limited(
            "<html><body>Attention Required! | Cloudflare</body></html>"
        ));
        assert!(looks_rate_limited("<html>Checking your browser...</html>"));
        assert!(looks_rate_limited("<html>429 Too Many Requests</html>"));

        let real_page = format!(
            "<html><body>{}<p>Sigil of Concentration has no rate limit.</p></body></html>",
            "<div>build content</div>".repeat(500)
        );
        assert!(real_page.len() > 8_192);
        assert!(
            !looks_rate_limited(&real_page),
            "a full page is not an interstitial, whatever it mentions"
        );
    }

    /// The site's own number wins over our guess, and only when it gave one
    /// in the seconds form.
    #[test]
    fn retry_after_is_read_when_the_site_gives_one() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_ms(&headers), None);

        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("30"),
        );
        assert_eq!(retry_after_ms(&headers), Some(30_000));

        // HTTP-date form: not parsed, and must not be read as 0.
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after_ms(&headers), None);
    }

    /// The category list is read off the sitemap so a category added or
    /// retired by the site needs no code change. The hub itself, the sub-80
    /// levelling category, guide pages and anything off-site are excluded.
    ///
    /// Shape taken from the live `page-sitemap.xml` on 2026-09-05, which
    /// listed five build categories plus the hub and levelling.
    #[test]
    fn sitemap_discovery_names_categories_and_their_modes() {
        let sitemap = concat!(
            "<urlset>",
            "<url><loc>https://guildjen.com/gw2-wvw-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-pvp-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-open-world-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-raid-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-fractal-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-wvw-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-leveling-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-mount-guides/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-wvw-guides/</loc></url>",
            "<url><loc>https://aw2.help/gw2-low-intensity-builds/</loc></url>",
            "</urlset>",
        );
        assert_eq!(
            guildjen_category_pages(sitemap),
            vec![
                ("https://guildjen.com/gw2-wvw-builds/".to_string(), "WvW"),
                ("https://guildjen.com/gw2-pvp-builds/".to_string(), "PvP"),
                (
                    "https://guildjen.com/gw2-open-world-builds/".to_string(),
                    "PvE"
                ),
                ("https://guildjen.com/gw2-raid-builds/".to_string(), "PvE"),
                (
                    "https://guildjen.com/gw2-fractal-builds/".to_string(),
                    "PvE"
                ),
            ]
        );
    }

    /// The fallback has to cover what discovery would have found, or a
    /// sitemap outage quietly narrows the sync. Both lists agree on the
    /// 2026-09-05 site.
    #[test]
    fn fallback_matches_what_the_sitemap_would_discover() {
        let sitemap = concat!(
            "<url><loc>https://guildjen.com/gw2-raid-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-wvw-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-fractal-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-pvp-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-open-world-builds/</loc></url>",
            "<url><loc>https://guildjen.com/gw2-leveling-builds/</loc></url>",
        );
        let mut discovered = guildjen_category_pages(sitemap);
        let mut fallback = guildjen_fallback_categories();
        discovered.sort();
        fallback.sort();
        assert_eq!(discovered, fallback);
    }

    #[test]
    fn pin_hardstuck_href_rejects_off_origin_http() {
        assert_eq!(pin_hardstuck_href("http://evil.example/build"), None);
        assert_eq!(
            pin_hardstuck_href("https://evil.example/gw2/builds/necromancer/x/"),
            None
        );
        assert_eq!(
            pin_hardstuck_href("http://hardstuck.gg/gw2/builds/necromancer/x/"),
            None
        );
        assert_eq!(
            pin_hardstuck_href("/gw2/builds/necromancer/blood-harbinger/").as_deref(),
            Some("https://hardstuck.gg/gw2/builds/necromancer/blood-harbinger/")
        );
        assert_eq!(
            pin_hardstuck_href("https://hardstuck.gg/gw2/builds/necromancer/blood-harbinger/")
                .as_deref(),
            Some("https://hardstuck.gg/gw2/builds/necromancer/blood-harbinger/")
        );
    }

    #[test]
    fn test_looks_like_blocked_page_detects_unifi_block() {
        // Markers taken from the observed UniFi/Ubiquiti block page.
        let html = r#"<html><head><title>Blocked</title></head><body>
            <div id="info-box"><svg id="ubiquiti-logo"></svg>
            <p>This domain is restricted. Contact your administrator for more information.</p>
            <p>blocked because the domain is restricted</p></body></html>"#;
        assert!(looks_like_blocked_page(html));
    }

    #[test]
    fn test_looks_like_blocked_page_ignores_real_build_page() {
        // A normal build page that happens to use the word "blocks" must NOT trip it.
        let html = r#"<html><body>
            <a href="/gw2/builds/guardian/firebrand">Firebrand</a>
            <p>Shield of Courage blocks the next attack. Great sustain build.</p>
            </body></html>"#;
        assert!(!looks_like_blocked_page(html));
    }

    #[test]
    fn test_looks_like_blocked_page_ignores_empty() {
        assert!(!looks_like_blocked_page(""));
        assert!(!looks_like_blocked_page(
            "<html><body>No builds here yet.</body></html>"
        ));
    }

    /// GuildJen build URLs are flat slugs on the site root with no profession
    /// segment, so the profession has to come from the specialization named in
    /// the slug. Reading it from the path is what filed builds under
    /// `Guildjen.com`, `Fonts` and `1.0` (synced cache, 2026-09-05).
    #[test]
    fn profession_comes_from_the_spec_in_the_slug() {
        for (slug, profession, spec) in [
            (
                "power-hammer-luminary-roaming-build",
                "Guardian",
                "Luminary",
            ),
            ("condition-scourge-havoc-build", "Necromancer", "Scourge"),
            (
                "celestial-spear-antiquary-roaming-build",
                "Thief",
                "Antiquary",
            ),
            ("support-paragon-cloud-build", "Warrior", "Paragon"),
            ("power-conduit-roaming-build", "Revenant", "Conduit"),
        ] {
            let got = profession_from_slug(slug).expect(slug);
            assert_eq!(
                (got.0.as_str(), got.1.as_str()),
                (profession, spec),
                "{slug}"
            );
        }
    }

    /// A core build names its profession directly and has no elite spec.
    #[test]
    fn profession_falls_back_to_a_core_name() {
        let (prof, spec) = profession_from_slug("power-core-guardian-roaming-build").expect("core");
        assert_eq!(prof, "Guardian");
        assert_eq!(spec, "Guardian");
    }

    /// Anything that names neither is refused rather than stored under a
    /// profession no character has.
    #[test]
    fn profession_refuses_a_slug_that_names_neither() {
        assert!(profession_from_slug("legendary-armor-guide-open-world").is_none());
        assert!(profession_from_slug("gw2-event-timers").is_none());
    }

    #[test]
    fn test_extract_gear_prefix_berserker() {
        let html = "Use Berserker's gear for maximum DPS.";
        assert_eq!(extract_gear_prefix(html), "Berserker's");
    }

    #[test]
    fn test_extract_gear_prefix_viper() {
        let html = "Viper's stat combo is optimal for condition builds.";
        assert_eq!(extract_gear_prefix(html), "Viper's");
    }

    /// A build is only what the page published as ids. The "Related Posts"
    /// list at the foot of a GuildJen build names other builds'
    /// specializations in prose, which is how 431 of 739 scraped builds
    /// recorded Firebrand / Willbender / Dragonhunter whatever profession
    /// they were.
    ///
    /// The old defence was to truncate the page at the chat code, which
    /// helped only because the footer came after it. Reading ids instead
    /// makes the position irrelevant: prose names nothing, wherever it sits.
    #[test]
    fn prose_names_no_specializations_wherever_it_sits() {
        let build = "[&DQYAAAAqASsATgA2ADYARgBGAEYARgAAAAAAAAAAAAAAAAAAAAAAAAA=]";
        let html = format!(
            "<aside>Related Posts: Firebrand, Willbender, Dragonhunter</aside>\
             <h1>Celestial Tempest Roaming</h1><p>Tempest is the elite.</p>\
             <code>{build}</code>\
             <aside>More: Scourge, Reaper, Harbinger</aside>"
        );
        let b = benchmark_from_html(
            &html,
            "https://guildjen.com/celestial-tempest-roaming-build/",
            "2026-09-06",
            "guildjen",
            "Elementalist".into(),
            "Tempest".into(),
            "WvW",
            "WvW Roaming",
        );
        assert_eq!(
            b.build_code.as_deref(),
            Some(build),
            "the code is found even with furniture on both sides of it"
        );
        assert!(
            b.published.specs.is_empty(),
            "no traitline was embedded, so the build claims none: {:?}",
            b.published.specs
        );
        assert!(b.published.gear.is_empty(), "nor any gear");
    }

    #[test]
    fn extract_gear_prefix_directly_is_not_dire() {
        assert_eq!(extract_gear_prefix("Boons apply directly to allies."), "");
        assert_eq!(
            extract_gear_prefix("Use Dire gear for condition sustain."),
            "Dire"
        );
    }

    #[test]
    fn test_title_case() {
        assert_eq!(title_case("power-dps"), "Power-dps");
        assert_eq!(title_case("guardian firebrand"), "Guardian Firebrand");
    }

    #[test]
    fn redirect_stays_on_request_host_same_host_case_insensitive() {
        assert!(redirect_stays_on_request_host(
            Some("guildjen.com"),
            Some("GuildJen.com")
        ));
        assert!(!redirect_stays_on_request_host(
            Some("guildjen.com"),
            Some("evil.example")
        ));
        assert!(!redirect_stays_on_request_host(Some("guildjen.com"), None));
        assert!(!redirect_stays_on_request_host(None, Some("guildjen.com")));
    }

    #[test]
    fn test_today_string_format() {
        let s = today_string();
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }

    /// Cancellation requested before any work must short-circuit `scrape_all`
    /// without performing any HTTP I/O. The function returns one
    /// `ScrapeResult` per source, each tagged with `CANCELLED_ERROR`. The
    /// bounded time budget asserts no network call was attempted (each scrape
    /// has a 15s reqwest timeout, so three sequential failures would take many
    /// seconds; cancellation must return in milliseconds).
    #[test]
    fn scrape_all_returns_immediately_when_cancelled_at_entry() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        let cancelled = Arc::new(AtomicBool::new(true));
        let cancelled_clone = cancelled.clone();
        let predicate = move || cancelled_clone.load(Ordering::Relaxed);

        let tmp = std::env::temp_dir().join("gw2_scraper_cancel_test_entry");
        let start = Instant::now();
        let results = scrape_all(&tmp, &predicate);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 500,
            "scrape_all should return within 500ms when cancelled at entry, took {:?}",
            elapsed
        );
        assert_eq!(results.len(), 3, "must still emit one result per source");
        for r in &results {
            assert_eq!(
                r.error.as_deref(),
                Some(CANCELLED_ERROR),
                "{} not flagged cancelled",
                r.source
            );
            assert!(
                r.builds.is_empty(),
                "{} should have no builds when cancelled",
                r.source
            );
        }
        // Sources stay in canonical order so the UI can rely on positional access.
        assert_eq!(results[0].source, "snowcrows");
        assert_eq!(results[1].source, "hardstuck");
        assert_eq!(results[2].source, "guildjen");

        // Defensive: keep `cancelled` alive across the call so the closure's
        // Arc clone observes a live AtomicBool (clippy::redundant_clone hint
        // would otherwise tempt removal).
        assert!(cancelled.load(Ordering::Relaxed));
    }

    /// A predicate that flips from false to true after the first invocation
    /// simulates the user clicking cancel after `scrape_all` has begun but
    /// before the first network call. Because the entry-point check fires
    /// first (sees false), we then proceed past `build_client` / mkdir, hit
    /// the pre-snowcrows check (still false on second call? no — it flips on
    /// every call, so call 2 returns true), and bail out before scraping.
    /// The test asserts bounded latency and that all three results are
    /// flagged cancelled.
    #[test]
    fn scrape_all_aborts_between_phases_when_predicate_flips() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        // Predicate returns false on the first call (entry check), true on
        // every subsequent call (pre-snowcrows and beyond).
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let predicate = move || {
            let n = calls_clone.fetch_add(1, Ordering::Relaxed);
            n > 0
        };

        let tmp = std::env::temp_dir().join("gw2_scraper_cancel_test_phases");
        let start = Instant::now();
        let results = scrape_all(&tmp, &predicate);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 1000,
            "scrape_all should bail out within 1s when cancel pulses after entry, took {:?}",
            elapsed
        );
        assert_eq!(results.len(), 3);
        for r in &results {
            assert_eq!(
                r.error.as_deref(),
                Some(CANCELLED_ERROR),
                "{} expected cancelled, got {:?}",
                r.source,
                r.error
            );
        }
        // Predicate fired at least twice (entry + pre-snowcrows).
        assert!(calls.load(Ordering::Relaxed) >= 2);
    }

    /// Sanity: passing a never-cancel predicate must not change the
    /// public observable result shape — three entries in canonical order.
    /// This test does not perform network I/O reliably across CI, so we
    /// only check the no-cancel branch returns the right vector length
    /// and source ordering when scraping is allowed to attempt and fail
    /// (errors are captured, not panicked). The test is bounded by the
    /// reqwest 15s × 3 timeout cap; in practice DNS for snowcrows.com
    /// resolves and the test completes quickly even offline.
    #[test]
    #[ignore = "performs real network I/O; run with `cargo test -- --ignored`"]
    fn scrape_all_no_cancel_returns_three_results() {
        let tmp = std::env::temp_dir().join("gw2_scraper_no_cancel_test");
        let results = scrape_all(&tmp, &|| false);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].source, "snowcrows");
        assert_eq!(results[1].source, "hardstuck");
        assert_eq!(results[2].source, "guildjen");
        for r in &results {
            assert_ne!(
                r.error.as_deref(),
                Some(CANCELLED_ERROR),
                "{} unexpectedly flagged cancelled with never-cancel predicate",
                r.source
            );
        }
    }

    /// Inner-loop cancellation: each per-source scraper must consult
    /// `should_cancel` BEFORE every outer (per-profession / per-index-page)
    /// HTTP fetch. With an always-true predicate, the function must return
    /// without performing any network I/O — proven by bounded latency well
    /// below the reqwest 15s timeout. The returned tuple's `cancelled` flag
    /// must be true and `builds` must be empty (nothing collected before
    /// the very first fetch). The predicate must have been observed at
    /// least once (the top-of-loop check).
    ///
    /// Mid-loop cancellation (cancel after N successful fetches) is not
    /// asserted here because it would require mocking the HTTP layer; see
    /// NOTES in the round-3 commit. The wiring proven here is sufficient:
    /// the predicate is in scope at the inner loop and short-circuits the
    /// loop body before each iteration's HTTP work.
    #[test]
    fn scrape_snowcrows_aborts_at_inner_loop_when_cancelled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let predicate = move || {
            calls_clone.fetch_add(1, Ordering::Relaxed);
            true
        };
        let client = build_client().expect("client build must succeed");

        let start = Instant::now();
        let result = scrape_snowcrows(
            &client,
            "2026-04-16",
            &HashMap::new(),
            &predicate,
            &|_, _| {},
        );
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 500,
            "scrape_snowcrows must abort within 500ms when cancel pulses, took {:?}",
            elapsed
        );
        match result {
            Ok((builds, cancelled, _failed)) => {
                assert!(cancelled, "cancelled flag must be true");
                assert!(
                    builds.is_empty(),
                    "no builds should be collected before first fetch"
                );
            }
            Err(e) => panic!("expected Ok((empty, true)) on cancel, got Err({})", e),
        }
        assert!(
            calls.load(Ordering::Relaxed) >= 1,
            "predicate must be invoked at least once at inner loop entry"
        );
    }

    /// Inner-loop cancellation for hardstuck. Same shape as the snowcrows
    /// test — see that test's docstring for rationale.
    #[test]
    fn scrape_hardstuck_aborts_at_inner_loop_when_cancelled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let predicate = move || {
            calls_clone.fetch_add(1, Ordering::Relaxed);
            true
        };
        let client = build_client().expect("client build must succeed");

        let start = Instant::now();
        let result = scrape_hardstuck(
            &client,
            "2026-04-16",
            &HashMap::new(),
            &predicate,
            &|_, _| {},
        );
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 500,
            "scrape_hardstuck must abort within 500ms when cancel pulses, took {:?}",
            elapsed
        );
        match result {
            Ok((builds, cancelled, _failed)) => {
                assert!(cancelled, "cancelled flag must be true");
                assert!(
                    builds.is_empty(),
                    "no builds should be collected before first fetch"
                );
            }
            Err(e) => panic!("expected Ok((empty, true)) on cancel, got Err({})", e),
        }
        assert!(
            calls.load(Ordering::Relaxed) >= 1,
            "predicate must be invoked at least once at inner loop entry"
        );
    }

    /// Shape check against real GuildJen pages.
    ///
    /// Ignored by default: it needs captures from the live site, which are
    /// 50-100 KB each and go stale the moment GuildJen publishes. The unit
    /// tests above pin the parsing rules; this one answers what they cannot -
    /// does the site still look the way those rules assume?
    ///
    /// Capture with raw HTTP, not a browser: the sitemap ships an XSL
    /// stylesheet, so a rendering client hands back a transformed table with
    /// no `<loc>` left in it. `fetch_html` uses reqwest and sees the XML.
    /// Either variable may be omitted to check only the other half.
    ///
    ///   GUILDJEN_SITEMAP_XML=page-sitemap.xml \
    ///   GUILDJEN_CATEGORY_HTML=wvw.html \
    ///     cargo test -p gw2-optimizer guildjen_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn guildjen_live_pages_yield_categories_and_builds() {
        let mut checked = 0;

        if let Ok(p) = std::env::var("GUILDJEN_SITEMAP_XML") {
            let xml = std::fs::read_to_string(&p).expect("sitemap readable");
            let categories = guildjen_category_pages(&xml);
            println!("-- {} categories from the sitemap --", categories.len());
            for (url, mode) in &categories {
                println!("  {mode:4} {url}");
            }
            assert!(
                categories
                    .iter()
                    .any(|(u, m)| u.contains("wvw") && *m == "WvW"),
                "the WvW category must be discovered"
            );
            assert!(
                categories
                    .iter()
                    .any(|(u, m)| u.contains("pvp") && *m == "PvP"),
                "the PvP category must be discovered"
            );
            assert!(
                !categories.iter().any(|(u, _)| u == GUILDJEN_HUB),
                "the hub lists no builds and must not be a category"
            );
            checked += 1;
        }

        if let Ok(p) = std::env::var("GUILDJEN_CATEGORY_HTML") {
            let html = std::fs::read_to_string(&p).expect("category html readable");
            let rows = crate::providers::guildjen::index_rows(&html);
            println!("-- {} builds listed by the index --", rows.len());
            assert!(!rows.is_empty(), "a category page must list builds");

            // The index files each build under a profession itself, so the
            // slug is only the fallback. A row with neither is dropped.
            let mut unfiled = Vec::new();
            for row in &rows {
                let slug = row
                    .url
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("");
                if !row.profession.is_empty() {
                    println!(
                        "  {:12} {:24} roles {:?} play {:?}",
                        row.profession, row.name, row.roles, row.playstyles
                    );
                } else if let Some((prof, spec)) = profession_from_slug(slug) {
                    println!("  {prof:12} {spec:14} {slug}  (from slug)");
                } else {
                    unfiled.push(slug.to_string());
                }
            }
            assert!(
                unfiled.is_empty(),
                "{} of {} builds name no profession and would be dropped: {:?}",
                unfiled.len(),
                rows.len(),
                unfiled
            );

            // What the sync asks for, and the classes it covers. Every row
            // is fetched now, so this is the whole table.
            let links = rows.clone();
            let by_class = group_rows_by_profession(rows);
            let covered: Vec<String> = by_class
                .iter()
                .map(|(c, l)| format!("{c} x{}", l.len()))
                .collect();
            let fetched: usize = by_class.iter().map(|(_, l)| l.len()).sum();
            println!(
                "-- {fetched} fetched across {} classes: {} --",
                by_class.len(),
                covered.join(", ")
            );
            assert_eq!(fetched, links.len(), "every listed build is fetched");
            assert!(
                by_class.len() >= 8,
                "a full page must cover nearly every class, got {}: {covered:?}",
                by_class.len()
            );
            checked += 1;
        }

        assert!(
            checked > 0,
            "set GUILDJEN_SITEMAP_XML and/or GUILDJEN_CATEGORY_HTML"
        );
    }

    /// Inner-loop cancellation for guildjen. Same shape as the snowcrows
    /// test — see that test's docstring for rationale.
    #[test]
    fn scrape_guildjen_aborts_at_inner_loop_when_cancelled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let predicate = move || {
            calls_clone.fetch_add(1, Ordering::Relaxed);
            true
        };
        let client = build_client().expect("client build must succeed");

        let start = Instant::now();
        let result = scrape_guildjen(
            &client,
            "2026-04-16",
            &HashMap::new(),
            &predicate,
            &|_, _| {},
        );
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 500,
            "scrape_guildjen must abort within 500ms when cancel pulses, took {:?}",
            elapsed
        );
        match result {
            Ok((builds, cancelled, _failed)) => {
                assert!(cancelled, "cancelled flag must be true");
                assert!(
                    builds.is_empty(),
                    "no builds should be collected before first fetch"
                );
            }
            Err(e) => panic!("expected Ok((empty, true)) on cancel, got Err({})", e),
        }
        assert!(
            calls.load(Ordering::Relaxed) >= 1,
            "predicate must be invoked at least once at inner loop entry"
        );
    }

    /// `marker` rides on `spec_name` so a test can tell one written row from
    /// another on disk. It does not affect the filename, which is built from
    /// source, profession and mode.
    fn sample_guardian_pve(marker: &str) -> BenchmarkBuild {
        BenchmarkBuild {
            source: "snowcrows".into(),
            profession: "Guardian".into(),
            spec_name: marker.into(),
            mode: "PvE".into(),
            role: "Power DPS".into(),
            build_code: None,
            gear_prefix: "Berserker's".into(),
            source_url: "https://snowcrows.com/builds/raids/guardian/firebrand".into(),
            scraped_at: "2026-08-01".into(),
            ..Default::default()
        }
    }

    /// Mid-source cancel must not overwrite a previously good
    /// `{source}_{profession}_{mode}.json`. Inject a non-empty partial
    /// into `finish_source` with `cancelled=true` (the same Ok branch
    /// snowcrows/hardstuck/guildjen take after collecting at least one
    /// inner build). Seeded file bytes must stay unchanged when error
    /// is `CANCELLED_ERROR`.
    #[test]
    fn finish_source_cancel_does_not_overwrite_seeded_benchmark_file() {
        let tmp = std::env::temp_dir().join(format!(
            "gw2_scraper_cancel_partial_save_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("temp benchmarks dir");

        let seeded_path = tmp.join("snowcrows_guardian_pve.json");
        let seeded = serde_json::to_string_pretty(&vec![sample_guardian_pve("LAST_GOOD_SEED")])
            .expect("seed json");
        std::fs::write(&seeded_path, &seeded).expect("seed last-good file");
        let before = std::fs::read(&seeded_path).expect("read seed");

        let partial = sample_guardian_pve("SHOULD_NOT_LAND_ON_DISK");
        let result = finish_source("snowcrows", Ok((vec![partial], true, 0)), &tmp, &|_, _| {});

        assert_eq!(
            result.error.as_deref(),
            Some(CANCELLED_ERROR),
            "cancelled finish_source must tag CANCELLED_ERROR"
        );
        assert_eq!(
            result.builds.len(),
            1,
            "in-memory partial stays on ScrapeResult"
        );

        let after = std::fs::read(&seeded_path).expect("read after cancel");
        assert_eq!(
            after, before,
            "cancelled finish_source must not overwrite last-good snowcrows_guardian_pve.json"
        );
        let on_disk = std::fs::read_to_string(&seeded_path).unwrap();
        assert!(
            on_disk.contains("LAST_GOOD_SEED"),
            "seeded last-good content must remain"
        );
        assert!(
            !on_disk.contains("SHOULD_NOT_LAND_ON_DISK"),
            "the partial must not replace the last-good file"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn save_builds_reports_write_failure() {
        let missing = std::env::temp_dir().join(format!(
            "gw2bo-no-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let err = save_builds(&[sample_guardian_pve("x")], &missing).expect_err("hollow dir");
        assert!(
            err.contains("write"),
            "write failure must surface, got {err}"
        );
        let result = finish_source(
            "snowcrows",
            Ok((vec![sample_guardian_pve("x")], false, 0)),
            &missing,
            &|_, _| {},
        );
        assert!(
            result.error.as_ref().is_some_and(|e| e.contains("write")),
            "finish_source must not report done on silent write, got {:?}",
            result.error
        );
    }

    /// The stored role is where the build is played, then what it does
    /// there — in the words the player's own role chips use, because
    /// `find_best_benchmark` ranks candidates by word overlap between the
    /// two. Every one of these vocabularies was measured across GuildJen's
    /// five category indexes on 2026-09-06.
    #[test]
    fn the_role_reads_as_scale_then_job_in_the_players_own_words() {
        use crate::providers::guildjen::IndexRow;
        let row = |name: &str, roles: &[&str], playstyles: &[&str]| IndexRow {
            url: "https://guildjen.com/x-build/".into(),
            name: name.into(),
            profession: "Guardian".into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            playstyles: playstyles.iter().map(|p| p.to_string()).collect(),
            difficulty: "easy".into(),
        };

        for (mode, category, name, roles, playstyles, want) in [
            // Open world states a playstyle and usually no role, so the
            // build's own name is the whole answer.
            (
                "PvE",
                "Open World",
                "Power Vengeance Dragonhunter",
                &[][..],
                &["bossing"][..],
                "Open World Power DPS",
            ),
            (
                "PvE",
                "Open World",
                "Heal DPS Luminary",
                &["support", "tank"][..],
                &["cooperative"][..],
                "Open World Healer",
            ),
            // A raid states `dps`, which does not say which kind. The name
            // does.
            (
                "PvE",
                "Raid",
                "Condition Reaper",
                &["dps"][..],
                &[][..],
                "Raid Condi DPS",
            ),
            (
                "PvE",
                "Fractal",
                "Heal Alacrity Druid",
                &["support"][..],
                &[][..],
                "Fractal Healer",
            ),
            // WvW: the site's role words are the ones the chips use, and
            // reading the name instead would call this a Hybrid.
            (
                "WvW",
                "Wvw",
                "Celestial Spear Antiquary",
                &["bruiser"][..],
                &["roaming"][..],
                "Roaming Bruiser",
            ),
            // Smallest playstyle first: a havoc build can join a cloud.
            (
                "WvW",
                "Wvw",
                "Power Staff Daredevil",
                &["assassin"][..],
                &["cloud", "havoc"][..],
                "Havoc Assassin",
            ),
            // PvP is always five a side, so it records no scale.
            (
                "PvP",
                "Pvp",
                "Support Firebrand",
                &["support"][..],
                &[][..],
                "Support",
            ),
            (
                "PvP",
                "Pvp",
                "Power Willbender",
                &["duelist"][..],
                &[][..],
                "Duelist",
            ),
        ] {
            let row = row(name, roles, playstyles);
            let got = crate::providers::role_label(
                &guildjen_scale(mode, category, &row),
                &guildjen_job(mode, &row),
            );
            assert_eq!(got, want, "{mode} {name}");
        }
    }

    /// Every branch of the old classifier returned a WvW label, including
    /// the fallback, so all 411 stored GuildJen rows read "WvW Roaming" —
    /// Elementalist PvE ones included.
    #[test]
    fn a_pve_build_is_never_labelled_with_wvw_words() {
        use crate::providers::guildjen::IndexRow;
        let row = IndexRow {
            url: "https://guildjen.com/celestial-tempest-open-world-build/".into(),
            name: "Celestial Earthquake Tempest".into(),
            profession: "Elementalist".into(),
            roles: vec![],
            playstyles: vec!["cooperative".into()],
            difficulty: "medium".into(),
        };
        let role = crate::providers::role_label(
            &guildjen_scale("PvE", "Open World", &row),
            &guildjen_job("PvE", &row),
        );
        assert_eq!(role, "Open World Hybrid");
        for wvw_word in ["Roaming", "Havoc", "Cloud", "Zerg", "WvW"] {
            assert!(
                !role.contains(wvw_word),
                "a PvE build must not be labelled {wvw_word}: {role}"
            );
        }
    }
    /// The em dash that killed a 328-page sync was a byte-index slice into
    /// text exactly like this. Every cut point here lands mid-character.
    #[test]
    fn prose_is_cut_on_a_character_boundary() {
        let text = "Sword 2 \u{2014} Dagger 5 \u{2014} Sword 111";
        for limit in 0..text.len() + 4 {
            let cut = truncate_chars(text, limit);
            assert!(cut.len() <= limit, "{limit}: {cut:?} exceeds the cap");
            assert!(text.starts_with(&cut), "{limit}: {cut:?} is not a prefix");
        }
        assert_eq!(
            truncate_chars(text, text.len()),
            text,
            "under the cap, intact"
        );
    }
}
