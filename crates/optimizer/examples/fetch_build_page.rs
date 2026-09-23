//! Fetch one build page exactly as the benchmark scraper sees it, and report
//! what is actually in the markup.
//!
//! Browser captures lie about this. A headless capture of
//! guildjen.com/support-troubadour-cloud-build/ on 2026-09-06 carried ten
//! `<img>` tags and zero `alt` attributes, while the rendered page shows
//! dozens of gear icons with their names beside them — the images are
//! lazy-loaded and the capture tool strips attributes. Neither tells us what
//! `reqwest` gets, which is the only thing the scraper can extract from.
//!
//!   cargo run -p gw2-optimizer --example fetch_build_page -- <url> [out.html]

use std::io::Write;

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "https://guildjen.com/support-troubadour-cloud-build/".to_string());
    let out = args.next();

    // The scraper's own headers, so this sees what it sees.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
             image/webp,image/png,image/svg+xml,*/*;q=0.8",
        ),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        reqwest::header::HeaderValue::from_static("en-US,en;q=0.5"),
    );
    let client = reqwest::blocking::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:142.0) Gecko/20100101 Firefox/142.0",
        )
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("client");

    println!("GET {url}");
    let html = client
        .get(&url)
        .send()
        .expect("request")
        .text()
        .expect("body");
    println!("  {} bytes", html.len());

    let count = |needle: &str| html.matches(needle).count();
    println!("  <img       : {}", count("<img"));
    println!("  alt=       : {}", count("alt="));
    println!("  data-src   : {}", count("data-src"));
    println!("  <table     : {}", count("<table"));
    println!("  <aside     : {}", count("<aside"));
    println!("  [&         : {}", count("[&"));

    for label in ["Superior Rune", "Superior Sigil", "Relic of", "Minstrel"] {
        println!("  {label:<14}: {}", count(label));
    }

    let pruned = gw2_optimizer::article::prune_to_article(&html);
    println!(
        "  pruned     : {} bytes ({:.0}% kept)",
        pruned.len(),
        100.0 * pruned.len() as f64 / html.len() as f64
    );
    for label in [
        "Superior Rune",
        "Superior Sigil",
        "Relic of",
        "Dragonhunter",
    ] {
        println!(
            "    after prune {label:<14}: {}",
            pruned.matches(label).count()
        );
    }

    // Every chat link on the page, by kind. The rendered page shows rune and
    // sigil names that are nowhere in the markup, so the suspicion is that
    // the page ships them as item links and the browser expands them.
    let mut kinds: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut item_ids: Vec<u32> = Vec::new();
    let mut pos = 0;
    while let Some(rel) = html[pos..].find("[&") {
        let start = pos + rel;
        let Some(end_rel) = html[start..].find(']') else {
            break;
        };
        let end = start + end_rel + 1;
        let body = &html[start + 2..end - 1];
        pos = end;
        let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, body)
        else {
            *kinds.entry("not base64").or_default() += 1;
            continue;
        };
        match bytes.first() {
            Some(0x02) => *kinds.entry("coin").or_default() += 1,
            Some(0x04) => {
                *kinds.entry("ITEM").or_default() += 1;
                if bytes.len() >= 5 {
                    item_ids.push(u32::from_le_bytes([bytes[2], bytes[3], bytes[4], 0]));
                }
            }
            Some(0x06) => *kinds.entry("skill").or_default() += 1,
            Some(0x07) => *kinds.entry("trait").or_default() += 1,
            Some(0x0D) => *kinds.entry("BUILD TEMPLATE").or_default() += 1,
            Some(other) => {
                *kinds
                    .entry(Box::leak(format!("0x{other:02X}").into_boxed_str()))
                    .or_default() += 1
            }
            None => {}
        }
    }
    println!("  chat links by kind:");
    for (kind, n) in &kinds {
        println!("    {kind:<16}: {n}");
    }
    item_ids.sort_unstable();
    item_ids.dedup();
    println!(
        "  distinct item ids: {} -> {:?}",
        item_ids.len(),
        &item_ids[..item_ids.len().min(12)]
    );

    // Which convention this page publishes ids in. Each site has its own,
    // and a count of zero is the first thing to check when a parser that
    // works elsewhere returns nothing here.
    println!("  id conventions:");
    for (label, needle) in [
        ("data-gw2-embed  (GuildJen)", "data-gw2-embed"),
        ("data-armory-embed (Snowcrows)", "data-armory-embed"),
        ("<gw2object       (Hardstuck)", "<gw2object"),
    ] {
        println!("    {label}: {}", count(needle));
    }
    println!(
        "  parsed as guildjen: {:?}",
        gw2_optimizer::providers::guildjen::parse(&html)
    );
    let (benchmark_dps, log_url) = gw2_optimizer::providers::snowcrows::benchmark(&html);
    println!("  benchmark_dps (snowcrows): {benchmark_dps:?}");
    println!("  log_url       (snowcrows): {log_url:?}");

    if let Some(path) = out {
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(html.as_bytes()).expect("write");
        println!("  saved raw html to {path}");
    }
}
