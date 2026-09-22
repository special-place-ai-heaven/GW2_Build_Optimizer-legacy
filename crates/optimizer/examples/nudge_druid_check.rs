//! Empirical check of the per-slot nudge pass on real game data: druid roam,
//! user weights from the bug report. Run:
//!   cargo run --release -p gw2-optimizer --example nudge_druid_check
//! Reads the cache directory from dev.cfg (copy dev.cfg.example).
use gw2_api::cache::DataCache;
use gw2_optimizer::scoring::OptimizationWeights;
use gw2_optimizer::{engine, search_v2};

fn main() {
    let cache = DataCache::new(gw2_api::dev_config::cache_dir_or_exit());
    let db = gw2_optimizer::gamedb::GameDb::load(&cache).expect("load real GameDb");
    println!(
        "db loaded: {} items, {} itemstats",
        db.items.len(),
        db.itemstats.len()
    );

    // Radar weights: selectable via args — "condition" run favors mixing.
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bugreport".into());
    let weights = match mode.as_str() {
        // condition-heavy + sustain: classic hybrid mixing pressure
        "condition" => OptimizationWeights {
            power: 0.2,
            condition: 0.9,
            boon_support: 0.3,
            healing: 0.3,
            sustain: 0.6,
            control: 0.4,
        },
        mode => {
            let _ = mode;
            OptimizationWeights {
                power: 0.5,
                condition: 0.2,
                boon_support: 0.1,
                healing: 0.2,
                sustain: 0.5,
                control: 0.5,
            }
        }
    };
    println!("weights mode: {mode}");

    let scenario = gw2_optimizer::scenario::ScenarioSpec {
        game_mode: gw2_core::types::GameMode::WvW,
        combat_tier: gw2_optimizer::scenario::CombatTier::Solo,
        combat_kind: gw2_optimizer::scenario::CombatKind::StrikeSpike,
        target_profile: gw2_optimizer::scenario::TargetProfile::Single,
        optimization_target: gw2_optimizer::scenario::OptimizationTarget {
            label: "WvW".into(),
        },
        patch_id: None,
        objective_profile_id: None,
    };

    let on_progress = |p: gw2_optimizer::engine::OptimizeProgress| {
        println!("[{:>5.1}s] {}", 0.0, p.stage);
    };
    let _ = on_progress;
    let mut noop = |p: gw2_optimizer::engine::OptimizeProgress| {
        println!("[stage] {}", p.stage);
    };

    // Beam only (for before/after comparison).
    let beam_only = search_v2::optimize_v2_search(
        &db,
        "Ranger",
        &weights,
        &gw2_optimizer::balance::BalanceContext::new(gw2_core::types::GameMode::WvW),
        &scenario,
        &gw2_core::types::BuildLocks::default(),
        &search_v2::SearchConfig::default(),
        &mut |p| println!("[beam] {}", p.stage),
        &|| false,
    )
    .expect("beam search");

    println!("\n=== BEAM RESULT (before nudge) ===");
    for slot in gw2_core::types::GearSlot::ALL {
        if let Some(prefix) = beam_only.gear_slots.get(slot) {
            println!("  {:?}: {}", slot, prefix.name);
        }
    }
    let report = gw2_optimizer::referee::evaluate_validated_build(
        &beam_only,
        &db,
        "Ranger",
        &weights,
        &gw2_optimizer::balance::BalanceContext::new(gw2_core::types::GameMode::WvW),
        &scenario,
    );
    println!("intent score: {:.4}", report.user_intent_score);

    let t0 = std::time::Instant::now();
    let result = engine::optimize_v2(
        &db,
        "Ranger",
        &weights,
        &gw2_optimizer::balance::BalanceContext::new(gw2_core::types::GameMode::WvW),
        &scenario,
        &gw2_core::types::BuildLocks::default(),
        None,
        None,
        &mut noop,
        &|| false,
    )
    .expect("optimize_v2");
    println!(
        "\n=== FULL OPTIMIZE (with nudge, {:.1}s) ===",
        t0.elapsed().as_secs_f64()
    );
    for slot in gw2_core::types::GearSlot::ALL {
        if let Some(prefix) = result.validated.gear_slots.get(slot) {
            println!("  {:?}: {}", slot, prefix.name);
        }
    }
    let report2 = gw2_optimizer::referee::evaluate_validated_build(
        &result.validated,
        &db,
        "Ranger",
        &weights,
        &gw2_optimizer::balance::BalanceContext::new(gw2_core::types::GameMode::WvW),
        &scenario,
    );
    println!("intent score: {:.4}", report2.user_intent_score);
    println!("\nslots changed by nudge: {}", {
        let mut n = 0;
        for slot in gw2_core::types::GearSlot::ALL {
            let a = beam_only.gear_slots.get(slot).map(|p| p.itemstat_id);
            let b = result.validated.gear_slots.get(slot).map(|p| p.itemstat_id);
            if a != b {
                n += 1;
            }
        }
        n
    });

    // Diagnostic: force known nudges and dump rank components.
    let forced_names: [(&str, u32); 3] = [("Seraph's", 1), ("Vigilant's", 1), ("Harrier's", 1)];
    let mut db2 = db.clone();
    let mut next_id = 9000;
    for (name, _) in forced_names {
        if db2.itemstats.values().all(|is| is.name != name) {
            db2.itemstats.insert(
                next_id,
                gw2_api::models::ItemStat {
                    id: next_id,
                    name: name.into(),
                    // rough profiles: Seraph's/Harrier's power+conc+heal, Vigilant's power+tough+conc
                    attributes: match name {
                        "Vigilant's" => vec![
                            gw2_api::models::StatAttribute {
                                attribute: "Power".into(),
                                multiplier: 0.8,
                                value: 0,
                            },
                            gw2_api::models::StatAttribute {
                                attribute: "Toughness".into(),
                                multiplier: 0.65,
                                value: 0,
                            },
                            gw2_api::models::StatAttribute {
                                attribute: "Concentration".into(),
                                multiplier: 0.65,
                                value: 0,
                            },
                        ],
                        _ => vec![
                            gw2_api::models::StatAttribute {
                                attribute: "Power".into(),
                                multiplier: 0.6,
                                value: 0,
                            },
                            gw2_api::models::StatAttribute {
                                attribute: "Concentration".into(),
                                multiplier: 0.8,
                                value: 0,
                            },
                            gw2_api::models::StatAttribute {
                                attribute: "HealingPower".into(),
                                multiplier: 0.5,
                                value: 0,
                            },
                        ],
                    },
                },
            );
            next_id += 1;
        }
    }
    let prefix_id = |db2: &gw2_optimizer::gamedb::GameDb, name: &str| {
        db2.itemstats
            .values()
            .find(|is| is.name == name)
            .map(|is| is.id)
            .unwrap_or(0)
    };
    for (slot_hint, prefix_name) in [
        ("Coat", "Seraph's"),
        ("Leggings", "Vigilant's"),
        ("Amulet", "Harrier's"),
    ] {
        let mut forced = beam_only.clone();
        let pid = prefix_id(&db2, prefix_name);
        let slot = match slot_hint {
            "Coat" => gw2_core::types::GearSlot::Coat,
            "Leggings" => gw2_core::types::GearSlot::Leggings,
            _ => gw2_core::types::GearSlot::Amulet,
        };
        forced.gear_slots.set(
            slot,
            gw2_core::types::PrefixRef {
                itemstat_id: pid,
                name: prefix_name.into(),
            },
        );
        let rep = gw2_optimizer::referee::evaluate_validated_build(
            &forced,
            &db2,
            "Ranger",
            &weights,
            &gw2_optimizer::balance::BalanceContext::new(gw2_core::types::GameMode::WvW),
            &scenario,
        );
        println!(
            "forced {:<10} {:<11} intent={:.4} exec={:?}",
            slot_hint,
            prefix_name,
            rep.user_intent_score,
            gw2_optimizer::referee::search_rank(&rep)[4],
        );
    }
}
