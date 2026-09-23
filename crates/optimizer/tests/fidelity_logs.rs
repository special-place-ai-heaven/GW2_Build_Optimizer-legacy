//! The Elite Insights (EI) log fixtures, and the simulator's fidelity against
//! them.
//!
//! Plain CI: every fixture parses and is a fixed point of parse then
//! serialize, `codes.json` names real fixture players, and the fight profile
//! stays print-only. The db-backed tests are `#[ignore]`: comparing needs the
//! synced game data and the published corpus.
//!
//!   cargo test -p gw2-optimizer --test fidelity_logs -- --ignored

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use gw2_optimizer::fidelity::compare::{self, PlayerComparison};
use gw2_optimizer::fidelity::ei_log::{self, EiLog};
use gw2_optimizer::gamedb::GameDb;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ei_logs");

type Codes = BTreeMap<String, BTreeMap<String, String>>;

fn fixtures() -> Vec<(String, EiLog, String)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(FIXTURES)
        .expect("fixture dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter(|p| p.file_name().is_some_and(|n| n != "codes.json"))
        .collect();
    paths.sort();
    assert!(paths.len() >= 3, "expected >= 3 EI fixtures in {FIXTURES}");
    paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).expect("read fixture");
            let log =
                ei_log::parse(&text).unwrap_or_else(|e| panic!("{} parses: {e}", p.display()));
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, log, text)
        })
        .collect()
}

fn codes() -> Codes {
    let text = std::fs::read_to_string(Path::new(FIXTURES).join("codes.json")).expect("codes.json");
    serde_json::from_str(&text).expect("codes.json parses")
}

#[test]
fn every_fixture_parses_and_is_a_fixed_point() {
    for (name, log, text) in fixtures() {
        assert!(log.squad().count() > 0, "{name} has no squad players");
        let out = serde_json::to_string(&log).expect("serializes");
        assert_eq!(out, text.trim_end(), "{name} is not a fixed point");
    }
}

#[test]
fn codes_name_players_that_exist_in_their_fixture() {
    let logs: BTreeMap<String, EiLog> = fixtures().into_iter().map(|(n, l, _)| (n, l)).collect();
    for (file, by_name) in codes() {
        let log = logs
            .get(&file)
            .unwrap_or_else(|| panic!("codes.json names {file}, which is not a fixture"));
        for (name, code) in by_name {
            assert!(
                log.squad().any(|p| p.name == name),
                "codes.json: {file} has no squad player {name}"
            );
            assert!(
                code.starts_with("[&") && code.ends_with(']'),
                "codes.json: {file} / {name} is not a chat code"
            );
        }
    }
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                rs_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The fight profile is measured and printed in this increment, never
/// consumed: nothing outside `src/fidelity/` may name the type.
#[test]
fn nothing_outside_fidelity_reads_a_fight_profile() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates = manifest.parent().expect("crates dir");
    let fidelity = manifest.join("src").join("fidelity");
    let this_file = manifest.join("tests").join("fidelity_logs.rs");
    let mut files = Vec::new();
    rs_files(crates, &mut files);
    assert!(files.len() > 50, "walked only {} .rs files", files.len());
    let offenders: Vec<String> = files
        .iter()
        .filter(|p| !p.starts_with(&fidelity) && **p != this_file)
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("FightProfile")))
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "FightProfile read outside fidelity: {offenders:?}"
    );
}

/// (profession, spec, mode, observable, max p90 |error|, reason), keyed like
/// `compare::bands`: spec is the log's elite spec or core profession name.
/// Ships empty: the baseline run is recorded in the sprint doc first, then
/// rows are seeded for `skill_share`, `condi_fraction`, boon uptimes and
/// `cleanse_per_20s` only. WvW DPS stays out until the fight profile is
/// consumed.
const EXPECTED_FIDELITY: &[(&str, &str, &str, &str, f64, &str)] = &[];

/// Ratchet slack: a budget more than this above the measured p90 is stale.
const STALE_BY: f64 = 0.05;

const PROFESSIONS: [&str; 9] = [
    "Elementalist",
    "Engineer",
    "Guardian",
    "Mesmer",
    "Necromancer",
    "Ranger",
    "Revenant",
    "Thief",
    "Warrior",
];

#[test]
fn the_fidelity_budget_table_is_well_formed() {
    let modes: Vec<String> = fixtures()
        .iter()
        .map(|(_, l, _)| format!("{:?}", l.mode()))
        .collect();
    let mut seen: Vec<(&str, &str, &str, &str)> = Vec::new();
    for &(profession, spec, mode, observable, budget, reason) in EXPECTED_FIDELITY {
        let key = (profession, spec, mode, observable);
        assert!(
            PROFESSIONS.contains(&profession),
            "EXPECTED_FIDELITY names {profession}, not a profession"
        );
        assert!(
            !spec.trim().is_empty(),
            "EXPECTED_FIDELITY: {key:?} has no spec"
        );
        assert!(
            modes.iter().any(|m| m == mode),
            "EXPECTED_FIDELITY names {mode}, which no fixture has"
        );
        assert!(
            !observable.is_empty(),
            "EXPECTED_FIDELITY: {key:?} has no observable"
        );
        assert!(
            budget.is_finite() && budget > 0.0,
            "EXPECTED_FIDELITY: {key:?} budgets {budget}"
        );
        assert!(
            !reason.trim().is_empty(),
            "EXPECTED_FIDELITY: {key:?} has no reason"
        );
        assert!(
            !seen.contains(&key),
            "EXPECTED_FIDELITY: {key:?} appears twice"
        );
        seen.push(key);
    }
}

/// Log-side burst observables of the golem fixture, facts of the file.
/// Python over `damage1S[0]`: per-second = [s1, s2 - s1, ...] (95 seconds,
/// 4 030 026 total); best 5 s sum 285 462, best 10 s sum 515 818. The
/// fixture was re-downloaded and re-trimmed 2026-09-23 to keep `states`,
/// `conditionDamage1S` and the `dpsAll` actor split, so overlap, condition
/// share and ramp are now measured; these are the values `log_compare`
/// reports for this log, pinned as facts of the file.
#[test]
fn golem_fixture_burst_observables_are_facts_of_the_file() {
    let (_, log, _) = fixtures()
        .into_iter()
        .find(|(n, _, _)| n == "1f33-20260720-163045_golem.json")
        .expect("golem fixture");
    let p = log.squad().next().expect("one player");
    let o = compare::observe(&log, p, &GameDb::empty_for_tests());
    let close = |a: Option<f64>, b: f64| {
        let a = a.expect("measured");
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    };
    close(o.burst_peak_5s, 285_462.0 / 5.0);
    close(o.burst_peak_10s, 515_818.0 / 10.0);
    assert!(o.burst_peak_5s > Some(o.dps_engaged));
    close(o.burst_overlap_share, 1.0);
    close(o.condition_share, 0.0029288346379509602);
    close(o.condition_ramp_s, 11.0);
}

fn cached_db() -> GameDb {
    let cache = gw2_api::cache::DataCache::new(
        gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir"),
    );
    GameDb::load(&cache).expect("game data cached \u{2014} sync it in-game first")
}

/// Every fixture compared, codes from `codes.json`.
fn compare_all() -> Vec<PlayerComparison> {
    let db = cached_db();
    // The synced corpus lives in the addon's directory, beside its cache.
    let cache = gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir");
    let corpus = gw2_optimizer::scraper::load_benchmarks(cache.parent().expect("addon dir"));
    assert!(!corpus.is_empty(), "synced benchmark corpus");
    let codes = codes();
    fixtures()
        .into_iter()
        .flat_map(|(name, log, _)| {
            let for_log: HashMap<String, String> = codes
                .get(&name)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect();
            compare::compare_log(&name, &log, &for_log, None, &corpus, &db)
        })
        .collect()
}

#[test]
#[ignore = "needs a synced game-data cache; see dev.cfg"]
fn every_fixture_squad_player_is_compared_or_named() {
    let rows = compare_all();
    // Compared = reconstructed, validator-clean and simulated. A failed
    // blocking gate is a finding about the kit, not a refusal to compare.
    let compared = rows.iter().filter(|r| r.refused.is_none()).count();
    for r in rows
        .iter()
        .filter(|r| r.refused.is_none() && !r.gates.is_empty())
    {
        println!(
            "compared, gates failed {} / {} ({}): {}",
            r.log,
            r.player,
            r.spec,
            r.gates.join(", ")
        );
    }
    for r in rows.iter().filter(|r| r.refused.is_some()) {
        println!(
            "refused {} / {} ({}): {}",
            r.log,
            r.player,
            r.spec,
            r.refused.as_deref().unwrap_or("")
        );
    }
    println!("compared {compared}/{}", rows.len());
    assert!(
        compared * 5 >= rows.len() * 4,
        "compared {compared}/{} < 80%",
        rows.len()
    );
}

#[test]
#[ignore = "needs a synced game-data cache; see dev.cfg"]
fn no_observable_exceeds_its_fidelity_budget() {
    let bands = compare::bands(&compare_all());
    let mut failures = Vec::new();
    for &(profession, spec, mode, observable, budget, _) in EXPECTED_FIDELITY {
        let key = (
            profession.to_string(),
            spec.to_string(),
            mode.to_string(),
            observable,
        );
        match bands.get(&key).filter(|b| b.n > 0) {
            None => failures.push(format!(
                "{profession} · {spec} · {mode} · {observable}: budgeted {budget} but nothing measured"
            )),
            Some(b) if b.p90_abs > budget => failures.push(format!(
                "{profession} · {spec} · {mode} · {observable}: p90 {:.3} over budget {budget}",
                b.p90_abs
            )),
            Some(b) if b.p90_abs < budget - STALE_BY => failures.push(format!(
                "{profession} · {spec} · {mode} · {observable}: p90 {:.3} under budget {budget} by more than {STALE_BY} — ratchet it down",
                b.p90_abs
            )),
            Some(_) => {}
        }
    }
    assert!(
        failures.is_empty(),
        "{}\n\nmeasured now:\n{}",
        failures.join("\n"),
        compare::render_bands(&bands)
    );
}
