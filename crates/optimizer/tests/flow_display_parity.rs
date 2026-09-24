//! The rotation block every panel draws is the run the build was scored on.
//!
//! `engine::simulate_validated_flow` is what the addon's New Build, Improve,
//! Choya, reference and Saves tabs display (Simulated DPS, Skill Usage). It
//! must be the referee's own flow run - the one `scoring::realized_axes`
//! reads and `fidelity::compare` (log_compare) checks against logs - so the
//! panel and the score cannot drift apart again (in-game 2026-09-24: the
//! Improve panel showed a hand-built simulation with no forms, triggers or
//! scenario, every skill "x1").
//!
//! Needs the synced game-data cache (dev.cfg); without one there is nothing
//! to check and the test says so.

use gw2_core::types::GameMode;
use gw2_optimizer::balance::BalanceContext;
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::scenario::{CombatTier, ScenarioSpec};
use gw2_optimizer::scoring::{self, OptimizationWeights};
use gw2_optimizer::{benchmark, engine, referee, validation};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/benchmarks");

#[test]
fn displayed_flow_is_the_scored_flow() {
    let Ok(cache_dir) = gw2_api::dev_config::cache_dir() else {
        println!("no dev.cfg: nothing to check");
        return;
    };
    let db = GameDb::load(&gw2_api::cache::DataCache::new(cache_dir)).expect("cached GameDb");
    let corpus = gw2_optimizer::scraper::load_benchmarks_from(std::path::Path::new(FIXTURE));
    let weights = OptimizationWeights::default();
    let mut checked = 0;
    for (mode, tier) in [
        (GameMode::WvW, CombatTier::Solo),
        (GameMode::PvE, CombatTier::Party),
        (GameMode::PvP, CombatTier::Solo),
    ] {
        let ctx = BalanceContext::new(mode.clone());
        let scenario = ScenarioSpec::for_request(&ctx, tier, None, &weights);
        let rows = corpus
            .iter()
            .filter(|b| b.mode.eq_ignore_ascii_case(mode.label()))
            .take(12);
        for build in rows {
            let Some(plate) = benchmark::plate_from(build, &db) else {
                continue;
            };
            let v = validation::validate_gemini_build(&plate, &db, &build.profession);
            if !v.errors.is_empty() {
                continue;
            }
            let shown = engine::simulate_validated_flow(
                &v,
                &db,
                &build.profession,
                &weights,
                &ctx,
                &scenario,
            )
            .expect("a validated build has a bar");

            // log_compare's path: the referee's stat sheet, then its flow run.
            let report = referee::evaluate_validated_build_with(
                &v,
                &db,
                &build.profession,
                &weights,
                &ctx,
                &scenario,
                &[],
            );
            let prepared =
                engine::prepare_validated_rotation(&v, &db, &report.stats, Some(&scenario))
                    .expect("same bar");
            let verified = engine::simulate_flow(&prepared, &weights, Some(&scenario));
            let who = format!("{} {} ({mode:?})", build.profession, build.source_url);
            assert_eq!(shown.total_dps, verified.total_dps, "{who}");
            assert_eq!(shown.strike_dps, verified.strike_dps, "{who}");
            assert_eq!(shown.condition_dps, verified.condition_dps, "{who}");
            let usage = |r: &gw2_optimizer::rotation::SimulationResult| {
                r.skill_usage
                    .iter()
                    .map(|u| (u.name.clone(), u.cast_count, u.dps_contribution))
                    .collect::<Vec<_>>()
            };
            assert_eq!(usage(&shown), usage(&verified), "{who}");

            // The score's axes are read off the same run.
            let ranked = referee::evaluate_validated_build_ranked(
                &v,
                &db,
                &build.profession,
                &weights,
                &ctx,
                &scenario,
            );
            assert_eq!(
                scoring::realized_axes(&shown, &ranked.primary_combat).as_array(),
                ranked.realized.as_array(),
                "{who}"
            );
            checked += 1;
        }
    }
    assert!(checked >= 10, "only {checked} fixture builds validated");
    println!("{checked} builds: displayed flow == scored flow");
}
