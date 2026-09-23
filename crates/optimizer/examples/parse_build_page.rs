//! Run a provider parser over a saved page and print what it recovered.
//!
//! Fixtures prove the shape; only a real page proves the site. Save one with
//! `fetch_build_page` (which uses the scraper's own headers), then:
//!
//!   cargo run -p gw2-optimizer --example parse_build_page -- guildjen page.html

use gw2_optimizer::providers;

fn main() {
    let mut args = std::env::args().skip(1);
    let site = args.next().unwrap_or_else(|| "guildjen".to_string());
    let Some(path) = args.next() else {
        eprintln!("usage: parse_build_page <guildjen|snowcrows|hardstuck> <file.html>");
        std::process::exit(2);
    };
    let html = std::fs::read_to_string(&path).expect("read the saved page");
    println!("{site}  {path}  ({} bytes)", html.len());

    if site == "guildjen-index" {
        let rows = providers::guildjen::index_rows(&html);
        println!("  {} distinct builds listed", rows.len());
        let mut by_class: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut roles: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut plays: std::collections::BTreeMap<&str, usize> = Default::default();
        for row in &rows {
            *by_class.entry(row.profession.as_str()).or_default() += 1;
            for role in &row.roles {
                *roles.entry(role.as_str()).or_default() += 1;
            }
            for play in &row.playstyles {
                *plays.entry(play.as_str()).or_default() += 1;
            }
        }
        println!("  professions: {by_class:?}");
        println!("  roles      : {roles:?}");
        println!("  playstyles : {plays:?}");
        for row in rows.iter().take(6) {
            println!(
                "    {:<12} {:<32} roles {:?} play {:?}",
                row.profession, row.name, row.roles, row.playstyles
            );
        }
        return;
    }

    let build = match site.as_str() {
        "guildjen" => providers::guildjen::parse(&html),
        "snowcrows" => {
            let (benchmark_dps, log_url) = providers::snowcrows::benchmark(&html);
            println!("  benchmark_dps: {benchmark_dps:?}");
            println!("  log_url    : {log_url:?}");
            providers::snowcrows::parse(&html)
        }
        "hardstuck" => {
            let name = providers::hardstuck::build_name(&html).unwrap_or_default();
            println!("  name       : {name:?}");
            println!(
                "  mode/scale : {:?}",
                providers::hardstuck::mode_and_scale(&html)
            );
            println!(
                "  class says : {:?}",
                providers::hardstuck::game_mode(&html)
            );
            println!("  role       : {:?}", providers::role_in_name(&name));
            providers::hardstuck::parse(&html)
        }
        other => {
            eprintln!("no parser for {other} yet");
            std::process::exit(2);
        }
    };

    println!("  build code : {:?}", build.build_code);
    if let Some(code) = &build.build_code {
        match gw2_optimizer::build_template::decode(code) {
            Some(t) => println!(
                "    decodes to : profession {} specs {:?} skills {:?}",
                t.profession,
                t.specs.map(|s| s.id),
                t.skills
            ),
            None => println!("    DOES NOT DECODE"),
        }
    }
    println!("  rune       : {:?}", build.rune_id);
    println!("  sigils     : {:?}", build.sigil_ids);
    println!("  relic      : {:?}", build.relic_id);
    println!("  amulet     : {:?}", build.amulet_id);
    println!("  dominant stat: {:?}", build.dominant_stat());
    println!("  specs:");
    for spec in &build.specs {
        println!("    {:>3} -> {:?}", spec.id, spec.trait_ids);
    }
    println!("  gear ({} rows):", build.gear.len());
    for row in &build.gear {
        println!(
            "    {:<12} {:<14} item {:?} upgrades {:?}",
            row.slot, row.stat, row.item_id, row.upgrade_ids
        );
    }
    // The scrape stores this text on every build; only a real page shows
    // whether the pruner keeps the rotation or throws it out with the chrome.
    let prose = gw2_optimizer::article::article_text(&html);
    println!(
        "  prose ({} bytes pruned from {}):",
        prose.len(),
        html.len()
    );
    for line in prose.lines().filter(|l| !l.trim().is_empty()).take(400) {
        println!("    | {}", line.trim());
    }
}
