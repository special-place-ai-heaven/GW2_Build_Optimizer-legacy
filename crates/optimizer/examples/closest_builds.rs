//! Show which published builds a request would be offered, and why.
//!
//! Fixtures prove the shape; only the player's own synced corpus proves the
//! matching. Reads the real benchmarks folder from `dev.cfg`.
//!
//!   cargo run -p gw2-optimizer --example closest_builds -- Guardian WvW "Support"
//!
//! `PICKS_ALL=1` prints every candidate rather than one card per site, with
//! the measured axes. `PICKS_FLOOR=1` prints the calibration instead: every
//! reference for this profession against every role objective the mode
//! offers, grouped by what the page says it is, which is how
//! [`gw2_optimizer::scoring::INTENT_ALIGNMENT_FLOOR`] is chosen.

use gw2_optimizer::picks::{self, StatedScale};
use gw2_optimizer::scenario::{CombatTier, RoleObjective, ScenarioSpec};

fn main() {
    // `scale=roam|havoc|cloud` is the scale chips, which the role label does
    // not carry. Pulled out before the positional slots so it can go
    // anywhere on the line - the weapons slot before it is usually skipped.
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let scale = argv
        .iter()
        .position(|a| a.starts_with("scale="))
        .map(|at| argv.remove(at))
        .map(|a| match a["scale=".len()..].to_lowercase().as_str() {
            "roam" | "solo" | "open world" => StatedScale::Solo,
            "havoc" | "party" | "group" | "small" => StatedScale::Small,
            "cloud" | "zerg" | "squad" | "large" => StatedScale::Large,
            other => {
                eprintln!("unknown scale '{other}' - use roam, havoc or cloud");
                std::process::exit(2);
            }
        });
    let mut args = argv.into_iter();
    let profession = args.next().unwrap_or_else(|| "Guardian".into());
    let mode = args.next().unwrap_or_else(|| "WvW".into());
    let role = args.next().unwrap_or_else(|| "Support".into());

    let addon_dir = match gw2_api::dev_config::addons_dir() {
        Ok(dir) => dir.join("gw2_build_optimizer"),
        Err(e) => {
            eprintln!("no addons_dir in dev.cfg ({e}) — copy dev.cfg.example and set it");
            std::process::exit(2);
        }
    };
    let builds = gw2_optimizer::scraper::load_benchmarks(&addon_dir);
    println!(
        "{} benchmark rows from {}",
        builds.len(),
        addon_dir.display()
    );

    let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
    let db = match gw2_optimizer::gamedb::GameDb::load(&cache) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("game data not cached yet ({e}) — sync it in-game first");
            std::process::exit(2);
        }
    };

    let mode_enum = match mode.as_str() {
        "WvW" => gw2_core::types::GameMode::WvW,
        "PvP" => gw2_core::types::GameMode::PvP,
        _ => gw2_core::types::GameMode::PvE,
    };
    let tier = match scale {
        Some(StatedScale::Solo) => CombatTier::Solo,
        Some(StatedScale::Small) => CombatTier::Party,
        _ => CombatTier::Squad,
    };
    // The role chip, resolved the way the overlay names it: the chip word
    // ("support", "damage") or a distinctive part of the long label. An
    // argument that names no chip is an error rather than a silent "no
    // role", because a run with no role measures every candidate against
    // the mode default and prints confident nonsense.
    let objective = RoleObjective::from_name(&role, &mode_enum).unwrap_or_else(|| {
        eprintln!(
            "'{role}' names no {} role chip - pick one of: {}",
            mode_enum.label(),
            RoleObjective::play_roles_for(&mode_enum)
                .iter()
                .map(|r| format!("{} ({})", r.chip_word(&mode_enum), r.label()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        std::process::exit(2);
    });
    let weights = objective.to_weights_for(&mode_enum, tier);

    let ctx = gw2_optimizer::balance::BalanceContext::new(mode_enum.clone());

    if std::env::var("PICKS_FLOOR").is_ok() {
        let candidates = picks::candidates(&builds, &profession, &mode);
        calibrate(&candidates, &db, &ctx, &mode_enum, tier);
        return;
    }

    // The addon's own entry point, arguments and all: whatever this prints
    // is what the panel shows. It builds the scenario itself, which is the
    // whole point - an example that assembled its own measured every
    // candidate against the mode default profile and printed a Berserker
    // for a Support request (2026-09-22).
    let began = std::time::Instant::now();
    let ranked = picks::rank(
        &builds,
        &db,
        &ctx,
        &profession,
        tier,
        Some(objective),
        &weights,
        &picks::Kit::default(),
        &|| false,
    );
    let elapsed = began.elapsed();

    let scenario = ScenarioSpec::for_request(&ctx, tier, Some(objective), &weights);
    let profile = scenario.objective_profile_id.clone().unwrap_or_default();
    let row = gw2_optimizer::data::objective_profiles::objective_profiles()
        .profile_by_id(&profile)
        .map(|p| p.intent.row(tier).clone())
        .unwrap_or_default();
    println!("looking for: {profession} · {mode} · {role}");
    println!("  role    : {objective:?}");
    println!("  scale   : {scale:?} ({tier:?})");
    println!("  profile : {profile}");
    println!("  focus   : {:?}  avoid: {:?}", row.focus, row.avoid);
    println!("  intent  : {:?}", weights.as_array());
    println!(
        "  scored {} of {} in {:?} ({:.1} ms each)",
        ranked.picks.len(),
        ranked.candidates.len(),
        elapsed,
        elapsed.as_secs_f64() * 1000.0 / (ranked.candidates.len().max(1) as f64),
    );
    for (site, n) in &ranked.unparsed {
        println!("  {n} {site} rows could not be evaluated");
    }
    println!();

    let (cards, silent) = ranked.cards();
    let all = std::env::var("PICKS_ALL").is_ok();
    for pick in &ranked.picks {
        let build = ranked.candidates[pick.index];
        let card = cards
            .iter()
            .any(|(b, p)| b.source_url == build.source_url && p.index == pick.index);
        if !all && !card {
            continue;
        }
        println!(
            "{:<10} {:<24} {:<14} {:<22} align {:+.3} cos {:.3} score {:.3} {} {}",
            build.source,
            build.role,
            build.spec_name,
            build.gear_prefix,
            pick.alignment.unwrap_or(f64::NAN),
            pick.cosine,
            pick.report
                .as_ref()
                .map(|r| r.ranked_direction_score)
                .unwrap_or_default(),
            if card { "CARD       " } else { "no card    " },
            if pick.viable { "viable" } else { "not viable" },
        );
        if all {
            println!(
                "           axes {:?} kit {:.2}",
                pick.axes.map(|v| (v * 100.0).round() / 100.0),
                pick.kit_overlap
            );
        }
    }
    for site in silent {
        println!("{site:<10} publishes nothing close to this intent");
    }
}

/// The floor calibration: every candidate against every role objective this
/// mode offers, split by what the PAGE claims the build is.
///
/// The labels are used here and nowhere else in the path - the question the
/// floor answers is exactly "does the measurement agree with what people
/// published", so the published word is the only ground truth available.
fn calibrate(
    candidates: &[&gw2_optimizer::benchmark::BenchmarkBuild],
    db: &gw2_optimizer::gamedb::GameDb,
    ctx: &gw2_optimizer::balance::BalanceContext,
    mode: &gw2_core::types::GameMode,
    tier: CombatTier,
) {
    let scenario = ScenarioSpec {
        combat_tier: tier,
        ..ScenarioSpec::from_balance_context(ctx)
    };
    let claims = |role: &str| {
        let r = role.to_lowercase();
        if r.contains("heal") || r.contains("support") || r.contains("medic") {
            Some("support")
        } else if r.contains("dps")
            || r.contains("damage")
            || r.contains("assassin")
            || r.contains("roaming")
        {
            Some("damage")
        } else {
            None
        }
    };
    for role in RoleObjective::play_roles_for(mode) {
        let weights = role.to_weights_for(mode, tier);
        // The alignment needs the role's own profile, so each pass names it.
        let scenario = ScenarioSpec {
            objective_profile_id: Some(role.profile_id_for(mode, tier).to_string()),
            ..scenario.clone()
        };
        let (evaluated, _) = picks::evaluate(
            candidates,
            db,
            &weights,
            ctx,
            &scenario,
            &picks::Kit::default(),
            &|| false,
        );
        let mut buckets: std::collections::BTreeMap<&str, Vec<f64>> =
            std::collections::BTreeMap::new();
        for pick in &evaluated {
            if let Some(kind) = claims(&candidates[pick.index].role) {
                buckets
                    .entry(kind)
                    .or_default()
                    .push(pick.alignment.unwrap_or(f64::MIN));
            }
        }
        // The sweep the floor is chosen from: what each candidate value
        // would keep of each population.
        for floor in [-0.05, 0.0, 0.02, 0.05, 0.08, 0.10, 0.15, 0.20] {
            let kept = |kind: &str| {
                buckets.get(kind).map(|v: &Vec<f64>| {
                    100.0 * v.iter().filter(|c| **c >= floor).count() as f64 / v.len() as f64
                })
            };
            if let (Some(sup), Some(dmg)) = (kept("support"), kept("damage")) {
                println!(
                    "  sweep {:<22} floor {:+.2}  support {:>5.1}%  damage {:>5.1}%",
                    role.label(),
                    floor,
                    sup,
                    dmg
                );
            }
        }
        for (kind, mut values) in buckets {
            values.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
            println!(
                "{:<22} {:<8} n={:<4} p10 {:.3}  median {:.3}  p90 {:.3}  over floor {:.0}%",
                role.label(),
                kind,
                values.len(),
                at(0.1),
                at(0.5),
                at(0.9),
                100.0
                    * values
                        .iter()
                        .filter(|c| **c >= gw2_optimizer::scoring::INTENT_ALIGNMENT_FLOOR)
                        .count() as f64
                    / values.len() as f64,
            );
        }
    }
}
