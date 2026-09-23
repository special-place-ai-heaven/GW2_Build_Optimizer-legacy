//! Compare the simulator against Elite Insights (EI) JSON logs.
//!
//! For each log: the per-player diff table (kit provenance, observables, skill
//! shares), the measured fight profile, and at the end the error bands per
//! profession, mode and observable over every log. Error size never fails
//! the run; it is what the run is for.
//!
//!   cargo run -p gw2-optimizer --example log_compare -- <log.json|dir> [--code "Char=[&..]"]...
//!   cargo run -p gw2-optimizer --example log_compare -- <log.json> --trim out.json [--players N]
//!
//! A directory argument reads every `*.json` in it except `codes.json`, which
//! holds `{"<log file>": {"<character>": "<chat code>"}}` (or, per character,
//! `{"code": "<chat code>", "gear": {...}}` with stated gear); `--code` adds to it
//! for every log and wins on a clash. `--trim` parses and re-serialises one
//! log keeping only squad players (WvW: the largest group, lowest group
//! number on ties), at most N (default 10), records the untrimmed squad size
//! for the tier, and compares nothing.
//!
//! Exit: 0 ran and printed; 2 setup missing (dev.cfg, game cache, unreadable
//! log or bad arguments); 1 zero players compared.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::exit;

use gw2_core::types::GameMode;
use gw2_optimizer::fidelity::kit::CodeEntry;
use gw2_optimizer::fidelity::{compare, ei_log, fight_profile};
use gw2_optimizer::gamedb::GameDb;

type Codes = BTreeMap<String, BTreeMap<String, CodeEntry>>;

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(2);
}

fn main() {
    let mut input: Option<PathBuf> = None;
    let mut cli_codes: Vec<(String, String)> = Vec::new();
    let mut trim: Option<PathBuf> = None;
    let mut players = 10usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--code" => {
                let v = args
                    .next()
                    .unwrap_or_else(|| fail("--code needs Char=[&..]"));
                let (name, code) = v
                    .split_once('=')
                    .unwrap_or_else(|| fail("--code needs Char=[&..]"));
                cli_codes.push((name.to_string(), code.to_string()));
            }
            "--trim" => {
                trim = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--trim needs a path"))
                        .into(),
                )
            }
            "--players" => {
                players = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or_else(|| fail("--players needs a number"));
            }
            _ if input.is_none() => input = Some(a.into()),
            _ => fail(&format!("unexpected argument {a}")),
        }
    }
    let input = input.unwrap_or_else(|| fail("usage: log_compare <log.json|dir> [--code \"Char=[&..]\"]... [--trim out.json [--players N]]"));

    if let Some(out) = trim {
        let log = ei_log::load(&input).unwrap_or_else(|e| fail(&e));
        let trimmed = trim_log(log, players);
        let kept = trimmed.players.len();
        let json =
            serde_json::to_string(&trimmed).unwrap_or_else(|e| fail(&format!("serialize: {e}")));
        std::fs::write(&out, json)
            .unwrap_or_else(|e| fail(&format!("write {}: {e}", out.display())));
        println!("{} -> {} ({kept} players)", input.display(), out.display());
        return;
    }

    let (dir, logs) = if input.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&input)
            .unwrap_or_else(|e| fail(&format!("read {}: {e}", input.display())))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .filter(|p| p.file_name().is_some_and(|n| n != "codes.json"))
            .collect();
        paths.sort();
        (input.clone(), paths)
    } else {
        let dir = input.parent().map(Path::to_path_buf).unwrap_or_default();
        (dir, vec![input.clone()])
    };
    let file_codes: Codes = match std::fs::read_to_string(dir.join("codes.json")) {
        Ok(text) => {
            serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("codes.json: {e}")))
        }
        Err(_) => Codes::new(),
    };

    let addon_dir = match gw2_api::dev_config::addons_dir() {
        Ok(dir) => dir.join("gw2_build_optimizer"),
        Err(e) => fail(&format!("no addons_dir in dev.cfg ({e})")),
    };
    let corpus = gw2_optimizer::scraper::load_benchmarks(&addon_dir);
    let cache_dir = addon_dir.join("cache");
    let cache = gw2_api::cache::DataCache::new(&cache_dir);
    let db = GameDb::load(&cache).unwrap_or_else(|e| {
        fail(&format!(
            "game data not cached ({e}) — sync it in-game first"
        ))
    });
    println!(
        "{} published builds, {} skills in db, {} logs",
        corpus.len(),
        db.skills.len(),
        logs.len()
    );

    let mut all = Vec::new();
    for path in &logs {
        let log = ei_log::load(path).unwrap_or_else(|e| fail(&e));
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mut codes: HashMap<String, CodeEntry> = file_codes
            .get(&name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        codes.extend(
            cli_codes
                .iter()
                .map(|(n, c)| (n.clone(), CodeEntry::Code(c.clone()))),
        );
        let rows = compare::compare_log(&name, &log, &codes, Some(&cache_dir), &corpus, &db);
        println!(
            "\n## {name} ({:?}, {:?}, {} squad players)\n",
            log.mode(),
            log.tier(),
            rows.len()
        );
        println!("{}", compare::render_table(&rows));
        println!(
            "{}",
            fight_profile::render(&fight_profile::extract(&log, &db))
        );
        all.extend(rows);
    }

    let compared = all.iter().filter(|r| r.refused.is_none()).count();
    println!("\n## Bands over {} logs\n", logs.len());
    println!("{}", compare::render_bands(&compare::bands(&all)));
    println!("compared {compared}/{} squad players", all.len());
    for r in all.iter().filter(|r| r.refused.is_some()) {
        println!(
            "  refused {} / {} ({}): {}",
            r.log,
            r.player,
            r.spec,
            r.refused.as_deref().unwrap_or("")
        );
    }
    if compared == 0 {
        exit(1);
    }
}

/// Squad players only; in WvW the largest group (lowest number on ties);
/// at most `n` of them. The original squad size is kept so the tier does
/// not change, and boon source maps keep only the wearer's own share, so no
/// other player's name survives. Everything the model does not read was
/// already dropped by the parse.
fn trim_log(mut log: ei_log::EiLog, n: usize) -> ei_log::EiLog {
    let mut squad: Vec<ei_log::EiPlayer> = log.squad().cloned().collect();
    log.trimmed_squad_size = Some(
        log.trimmed_squad_size
            .unwrap_or(u32::try_from(squad.len()).unwrap_or(u32::MAX)),
    );
    if log.mode() == GameMode::WvW {
        let mut sizes: BTreeMap<u32, usize> = BTreeMap::new();
        for p in &squad {
            *sizes.entry(p.group).or_default() += 1;
        }
        // BTreeMap iterates groups ascending; max_by_key keeps the last max,
        // so reverse to keep the lowest group number on a tie.
        if let Some((&g, _)) = sizes.iter().rev().max_by_key(|(_, &c)| c) {
            squad.retain(|p| p.group == g);
        }
    }
    squad.truncate(n);
    for p in &mut squad {
        let own = p.name.clone();
        for d in p.buff_uptimes.iter_mut().flat_map(|b| &mut b.buff_data) {
            for m in [&mut d.generated, &mut d.generated_presence]
                .into_iter()
                .flatten()
            {
                m.retain(|k, _| *k == own);
            }
        }
    }
    log.players = squad;
    log
}
