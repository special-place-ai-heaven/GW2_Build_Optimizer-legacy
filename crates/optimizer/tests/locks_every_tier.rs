//! A locked elite specialization is honoured by every optimizer tier the
//! Improve flow can serve from: optimize_v2's beam (synergy seed, seed repair,
//! community meta seeds, neighbour operators), the deterministic synergy
//! pipeline, and the legacy search.
//!
//! Player report 2026-09-24: "Locked: Willbender" sat beside a Luminary
//! result. The engine was not the leak - the Improve tab prints the live
//! lock, not the one the run was sent with - and this pins that down so a
//! future tier cannot become one.
//!
//! Cache-backed: needs `dev.cfg` and synced game data; prints and returns
//! without them.

use gw2_core::types::{BuildLocks, GameMode};
use gw2_optimizer::balance::BalanceContext;
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::scenario::{CombatTier, RoleObjective, ScenarioSpec};
use gw2_optimizer::scoring::OptimizationWeights;
use gw2_optimizer::search_v2::SearchConfig;

#[test]
fn a_locked_elite_spec_survives_every_tier() {
    let Ok(addon_dir) = gw2_api::dev_config::addons_dir() else {
        println!("no dev.cfg: nothing to check");
        return;
    };
    let addon_dir = addon_dir.join("gw2_build_optimizer");
    let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
    let Ok(db) = GameDb::load(&cache) else {
        println!("game data not cached: nothing to check");
        return;
    };
    let profession = "Guardian";
    let willbender = db
        .specializations
        .values()
        .find(|s| s.name == "Willbender")
        .map(|s| s.id)
        .expect("Willbender in cached data");
    let mut locks = BuildLocks::default();
    locks.specs[2] = Some(willbender);

    // The report's radar: Power 100 %, Boon 33 %, Sustain 48 %, Control 11 %.
    // Unlocked, 1.14.42 served a Luminary tank under exactly these weights.
    let mut weights = OptimizationWeights {
        power: 0.0,
        condition: 0.0,
        boon_support: 0.0,
        healing: 0.0,
        sustain: 0.0,
        control: 0.0,
    };
    for (axis, v) in [(0, 1.0), (2, 0.33), (4, 0.48), (5, 0.11)] {
        weights.set_constrained(axis, v);
    }
    let ctx = BalanceContext::new(GameMode::WvW);
    let scenario = ScenarioSpec::for_request(
        &ctx,
        CombatTier::Solo,
        Some(RoleObjective::PowerDps),
        &weights,
    );

    // Tier 1: the beam, with the synced references seeding it, on a small
    // budget - every operator still gets a generation.
    let config = SearchConfig {
        beam_width: 4,
        eval_budget: 80,
        patience: 1,
        time_limit_secs: 30,
        benchmarks_dir: Some(addon_dir.clone()),
    };
    let v2 = gw2_optimizer::search_v2::optimize_v2_search(
        &db,
        profession,
        &weights,
        &ctx,
        &scenario,
        &locks,
        &config,
        &mut |_| {},
        &|| false,
    )
    .expect("optimize_v2 search");
    assert_eq!(
        v2.specializations.get(2).map(|s| s.spec_id),
        Some(willbender),
        "optimize_v2 left the lock: {:?}",
        v2.specializations
            .iter()
            .map(|s| &s.name)
            .collect::<Vec<_>>()
    );

    // Tier 2: the deterministic synergy pipeline.
    let det = gw2_optimizer::engine::optimize_deterministic_cancellable(
        &db,
        profession,
        &weights,
        &ctx,
        None,
        None,
        &locks,
        Some(&scenario),
        &mut |_| {},
        &|| false,
    )
    .expect("deterministic tier");
    assert_eq!(
        det.validated.specializations.get(2).map(|s| s.spec_id),
        Some(willbender),
        "deterministic tier left the lock"
    );

    // Tier 3: the legacy search, every candidate it would offer.
    let prof = db.profession(profession).expect("Guardian");
    let legacy = gw2_optimizer::engine::optimize_cancellable(
        prof,
        &weights,
        None,
        &db.items,
        &db.itemstats,
        &db.specializations,
        &db.traits,
        |_| {},
        5,
        &ctx,
        &locks,
        &db.pvp_amulets,
        &|| false,
    )
    .expect("legacy tier");
    assert!(!legacy.is_empty());
    for c in &legacy {
        assert_eq!(c.elite_spec, Some(willbender), "legacy tier left the lock");
    }
}
