//! Run every synced community build through our own viability gates.
//!
//! These builds are the ground truth. People play them, sites publish them,
//! and they demonstrably work — so a gate that fails them is describing our
//! model, not the build. This prints the pass rate per gate so a
//! miscalibrated one is a number rather than an argument.
//!
//!   cargo run -p gw2-optimizer --example calibrate_viability
//!   cargo run -p gw2-optimizer --example calibrate_viability -- WvW

use std::collections::BTreeMap;

use gw2_optimizer::benchmark::plate_from;
use gw2_optimizer::gamedb::GameDb;

fn main() {
    let only_mode = std::env::args().nth(1);

    let addon_dir = match gw2_api::dev_config::addons_dir() {
        Ok(dir) => dir.join("gw2_build_optimizer"),
        Err(e) => {
            eprintln!("no addons_dir in dev.cfg ({e})");
            std::process::exit(2);
        }
    };
    let builds = gw2_optimizer::scraper::load_benchmarks(&addon_dir);
    let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
    let db = match GameDb::load(&cache) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("game data not cached ({e}) — sync it in-game first");
            std::process::exit(2);
        }
    };
    println!(
        "{} synced builds, {} skills, {} palette entries in db",
        builds.len(),
        db.skills.len(),
        db.palette_to_skill.len()
    );
    {
        let mut have_code = 0;
        let mut decoded = 0;
        let mut resolved = 0;
        let mut marked = 0;
        for b in &builds {
            if !b.published.skill_ids.is_empty() {
                marked += 1;
            }
            let Some(code) = b.published.build_code.as_deref() else {
                continue;
            };
            have_code += 1;
            let Some(t) = gw2_optimizer::build_template::decode(code) else {
                continue;
            };
            decoded += 1;
            if t.skills.iter().any(|pid| {
                *pid != 0
                    && db
                        .palette_to_skill
                        .get(pid)
                        .is_some_and(|id| db.skills.contains_key(id))
            }) {
                resolved += 1;
            }
        }
        println!(
            "  skills marked up on {marked}; chat code on {have_code}, decodes {decoded},              palette resolves for {resolved}"
        );
    }

    // gate -> (passed, failed); plus the first few failing notes per gate so a
    // number can be chased back to a build.
    let mut tally: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Gates that judged nothing, by gate: counted apart from pass and fail.
    let mut skipped_gates: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_role: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    // (gate, job family) -> (passed, failed). One number per gate hides the
    // thing that decides whether a gate is describing the game: a gate can
    // look healthy across the whole corpus and still be unreachable for one
    // job, which is a veto nothing of that job can ever clear.
    let mut by_gate_family: BTreeMap<(String, String), (u32, u32)> = BTreeMap::new();
    // The metrics the thresholds are set against, per family, so a floor can
    // be read off the corpus instead of chosen.
    let mut health: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut margin: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut repeatable: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut viable_all_gates = 0u32;
    let mut viable_blocking_only = 0u32;
    let mut unusable = 0u32;
    let mut no_plate = 0u32;
    let mut bad_examples: Vec<String> = Vec::new();
    // Every distinct validation error text, with counts: a rejection class
    // is either our model being wrong or the site's data being wrong, and one
    // example each cannot tell you which is the big one.
    let mut reject_tally: BTreeMap<String, u32> = BTreeMap::new();
    let mut scored = 0u32;
    let mut with_opener = 0u32;

    for build in &builds {
        if let Some(ref want) = only_mode {
            if !build.mode.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        let Some(plate) = plate_from(build, &db) else {
            no_plate += 1;
            continue;
        };
        let validated =
            gw2_optimizer::validation::validate_gemini_build(&plate, &db, &build.profession);
        if !validated.errors.is_empty() {
            unusable += 1;
            for e in &validated.errors {
                *reject_tally.entry(e.detail.clone()).or_default() += 1;
            }
            if bad_examples.len() < 5 {
                bad_examples.push(format!(
                    "{} {} [{}] {}",
                    build.source, build.profession, build.role, validated.errors[0].detail
                ));
            }
            continue;
        }
        let scenario = gw2_optimizer::benchmark::published_scenario(build);
        let weights = gw2_optimizer::scoring::OptimizationWeights::default();
        let ctx = gw2_optimizer::balance::BalanceContext::new(scenario.game_mode.clone());
        // The page's own rotation line, when it wrote one — the referee then
        // judges the build on the rotation the site plays, not on the
        // timeline's guess.
        let opener = gw2_optimizer::rotation::prose::parse_rotation(
            &build.published.prose,
            &build.profession,
            &db,
        );
        if !opener.is_empty() {
            with_opener += 1;
        }
        let report = gw2_optimizer::referee::evaluate_validated_build_with(
            &validated,
            &db,
            &build.profession,
            &weights,
            &ctx,
            &scenario,
            &opener,
        );
        scored += 1;

        // Keyed on `combat_kind`, not the published role string: that is the
        // field the referee branches on, so a table built from anything else
        // would be measuring a different question than the one the gate asks.
        let family = format!("{:?}", scenario.combat_kind);
        if let Some(fight) = report.rotation.as_ref().and_then(|r| r.wvw.as_ref()) {
            health
                .entry(family.clone())
                .or_default()
                .push(fight.remaining_health_ratio);
            margin
                .entry(family.clone())
                .or_default()
                .push(fight.sustain_margin);
            let r = repeatable.entry(family.clone()).or_default();
            if fight.repeatable {
                r.0 += 1;
            } else {
                r.1 += 1;
            }
        }

        let mut all_passed = true;
        for gate in &report.viability.gates {
            let key = format!("{:?}", gate.gate);
            // An abstention is not a pass: counting it as one inflates the
            // pass rate of a gate that judged nothing.
            if gate.skipped {
                *skipped_gates.entry(key).or_default() += 1;
                continue;
            }
            let fam = by_gate_family
                .entry((key.clone(), family.clone()))
                .or_default();
            if gate.passed {
                fam.0 += 1;
            } else {
                fam.1 += 1;
            }
            let slot = tally.entry(key.clone()).or_default();
            if gate.passed {
                slot.0 += 1;
            } else {
                slot.1 += 1;
                all_passed = false;
                let notes = examples.entry(key).or_default();
                if notes.len() < 3 {
                    notes.push(format!(
                        "{} {} [{}] {}",
                        build.source, build.profession, build.role, gate.note
                    ));
                }
            }
        }
        let role = by_role.entry(build.role.clone()).or_default();
        if all_passed {
            role.0 += 1;
        } else {
            role.1 += 1;
        }
        // `is_viable` is the blocking set only - see `ViabilityReport::is_viable`.
        // Reading it as "every gate" printed one number twice. The gap between
        // the two lines is the builds only a gate `blocks()` already publishes
        // as having no authority would refuse.
        debug_assert_eq!(
            report.viability.is_viable,
            report
                .viability
                .gates
                .iter()
                .all(|g| g.passed || g.skipped || !g.gate.blocks()),
            "is_viable is the blocking-gate set, abstentions aside"
        );
        if report.viability.is_viable {
            viable_blocking_only += 1;
        }
        if all_passed {
            viable_all_gates += 1;
        }
    }

    println!(
        "scored {scored} ({with_opener} with a published rotation line); skipped {no_plate} with no three-spec plate, \
         {unusable} that failed validation"
    );
    for note in &bad_examples {
        println!("   rejected: {note}");
    }
    if !reject_tally.is_empty() {
        let mut rows: Vec<(&String, &u32)> = reject_tally.iter().collect();
        rows.sort_by_key(|(text, count)| (std::cmp::Reverse(**count), (*text).clone()));
        println!(
            "
validation errors, most common first:"
        );
        for (text, count) in rows {
            println!("  {count:>4}  {text}");
        }
    }
    println!();
    let pct_of_scored = |n: u32| {
        if scored > 0 {
            n as f64 * 100.0 / scored as f64
        } else {
            0.0
        }
    };
    println!(
        "every gate:                  {viable_all_gates:>4}/{scored} {:>5.0}%",
        pct_of_scored(viable_all_gates)
    );
    println!(
        "is_viable (blocking gates):  {viable_blocking_only:>4}/{scored} {:>5.0}%   \
         <- the rest score -1.0; the gap above is gates blocks() cannot refuse with",
        pct_of_scored(viable_blocking_only)
    );
    println!();
    println!("{:<24} {:>7} {:>7} {:>7}", "gate", "pass", "fail", "pass%");
    for (gate, (pass, fail)) in &tally {
        let total = pass + fail;
        let pct = if total > 0 {
            *pass as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        println!("{gate:<24} {pass:>7} {fail:>7} {pct:>6.0}%");
    }

    println!("\nwhole-build pass rate by published role (worst first):");
    let mut roles: Vec<_> = by_role.into_iter().collect();
    roles.sort_by_key(|(_, (p, f))| {
        let total = p + f;
        if total == 0 {
            100
        } else {
            (*p as i64 * 100) / total as i64
        }
    });
    for (role, (pass, fail)) in roles.iter().take(14) {
        let total = pass + fail;
        let pct = if total > 0 {
            *pass as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        println!("  {role:<28} {pass:>4}/{total:<4} {pct:>5.0}%");
    }

    const FAMILIES: [&str; 6] = [
        "Support",
        "StrikeSpike",
        "CondiRamp",
        "Harasser",
        "Disabler",
        "Commander",
    ];

    println!("\npass rate per gate per job family:");
    print!("{:<24}", "gate");
    for f in FAMILIES {
        print!("{f:>11}");
    }
    println!();
    let gate_names: Vec<String> = tally.keys().cloned().collect();
    for gate in &gate_names {
        print!("{gate:<24}");
        for f in FAMILIES {
            match by_gate_family.get(&(gate.clone(), f.to_string())) {
                Some((pass, fail)) if pass + fail > 0 => {
                    print!("{:>10.0}%", *pass as f64 * 100.0 / (pass + fail) as f64)
                }
                _ => print!("{:>11}", "-"),
            }
        }
        println!();
    }

    println!("\nwhat the corpus actually reaches, per family:");
    println!(
        "{:<10} {:>5} {:>8} {:>8} {:>8} {:>8} {:>11}",
        "family", "n", "hp p10", "hp p25", "hp p50", "hp p90", "repeatable"
    );
    let quantile = |sorted: &[f64], q: f64| -> f64 {
        let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
        sorted[i]
    };
    for f in FAMILIES {
        let Some(hp) = health.get(f) else { continue };
        let mut hp = hp.clone();
        hp.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let (rp, rf) = repeatable.get(f).copied().unwrap_or((0, 0));
        println!(
            "{f:<10} {:>5} {:>7.0}% {:>7.0}% {:>7.0}% {:>7.0}% {:>10.0}%",
            hp.len(),
            quantile(&hp, 0.10) * 100.0,
            quantile(&hp, 0.25) * 100.0,
            quantile(&hp, 0.50) * 100.0,
            quantile(&hp, 0.90) * 100.0,
            if rp + rf > 0 {
                rp as f64 * 100.0 / (rp + rf) as f64
            } else {
                0.0
            },
        );
    }
    for f in FAMILIES {
        let Some(m) = margin.get(f) else { continue };
        let mut m = m.clone();
        m.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "  {f:<8} sustain margin/s   p10 {:>8.0}  p50 {:>8.0}  p90 {:>8.0}",
            quantile(&m, 0.10),
            quantile(&m, 0.50),
            quantile(&m, 0.90),
        );
    }

    if !skipped_gates.is_empty() {
        println!("\nabstained (model does not simulate what the gate reads):");
        for (gate, count) in &skipped_gates {
            println!("  {gate:<24} {count:>4} builds not judged");
        }
    }

    println!("\nfailing examples:");
    for (gate, notes) in &examples {
        println!("  == {gate}");
        for note in notes {
            println!("     {note}");
        }
    }
}
