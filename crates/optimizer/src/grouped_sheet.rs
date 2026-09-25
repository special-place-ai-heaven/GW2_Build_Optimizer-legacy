//! Grouped-gear Hero-panel fixture and roam weight-ranking regression.
//!
//! Empty `gear_groups` inherit `gear_prefix`. The v1.4.18 screenshot
//! (Power 3058 / Precision 2544) counted both weapon sets; that path is gone.

use std::collections::HashMap;

use gw2_api::models::{Fact, Item, ItemDetails, ItemStat, StatAttribute, Trait};
use gw2_core::types::{GameMode, GearPrefixGroups};

use crate::balance::BalanceContext;
use crate::combat::{CombatPerformance, DamageModifiers};
use crate::data::quality::DataQuality;
use crate::engine::calculate_validated_stats;
use crate::gamedb::GameDb;
use crate::referee::{self, GateResult, RefereeReport, ViabilityGate, ViabilityReport};
use crate::rotation::{SimulationResult, WvwCombatReport};
use crate::scenario::{CombatKind, CombatTier, OptimizationTarget, ScenarioSpec, TargetProfile};
use crate::scoring::{score_with_weights, OptimizationWeights};
use crate::stats;
use crate::validation::{
    ValidatedBuild, ValidatedItem, ValidatedSpec, ValidatedWeaponSet, ValidatedWeapons,
    ARMOR_SLOTS, TRINKET_SLOTS, WEAPON_SET1_SLOTS,
};
use gw2_core::types::{GearSlot, PrefixRef};

const STRONG_ID: u32 = 142;
const RITUALIST_ID: u32 = 1549;
const INFILTRATION_RUNE: u32 = 24703;
const WELLSPRING: u32 = 978;
const LINGERING_MAGIC: u32 = 1059;
const NATURAL_FORTITUDE: u32 = 2286;
const PRECISE_STRIKE: u32 = 1011;

/// Uniform-apply group overrides on top of the build-wide slot fill.
#[derive(Default)]
struct Groups {
    armor: Option<PrefixRef>,
    trinkets: Option<PrefixRef>,
    weapons: Option<PrefixRef>,
}

fn prefix(id: u32, name: &str) -> PrefixRef {
    PrefixRef {
        itemstat_id: id,
        name: name.into(),
    }
}

fn three_stat(id: u32, name: &str, major: &str, minor_a: &str, minor_b: &str) -> ItemStat {
    ItemStat {
        id,
        name: name.into(),
        attributes: vec![
            StatAttribute {
                attribute: major.into(),
                multiplier: 0.35,
                value: 0,
            },
            StatAttribute {
                attribute: minor_a.into(),
                multiplier: 0.25,
                value: 0,
            },
            StatAttribute {
                attribute: minor_b.into(),
                multiplier: 0.25,
                value: 0,
            },
        ],
    }
}

fn attr_adjust(target: &str, value: i32) -> Fact {
    Fact::AttributeAdjust {
        text: None,
        icon: None,
        value: Some(value),
        target: Some(target.into()),
    }
}

fn trait_entry(id: u32, name: &str, facts: Vec<Fact>) -> Trait {
    Trait {
        id,
        name: name.into(),
        icon: None,
        description: None,
        specialization: 8,
        tier: 1,
        order: 0,
        slot: "Major".into(),
        facts,
        traited_facts: vec![],
        fact_parse_drops: 0,
        skills: vec![],
    }
}

fn rune_item(id: u32, bonuses: Vec<&str>) -> Item {
    Item {
        id,
        name: "Superior Rune of Infiltration".into(),
        description: None,
        icon: None,
        item_type: "UpgradeComponent".into(),
        rarity: "Exotic".into(),
        level: 60,
        vendor_value: Some(1),
        chat_link: None,
        default_skin: None,
        flags: vec![],
        game_types: vec!["WvW".into()],
        restrictions: vec![],
        details: Some(ItemDetails {
            description: None,
            detail_type: Some("Rune".into()),
            weight_class: None,
            defense: None,
            damage_type: None,
            min_power: None,
            max_power: None,
            suffix: Some("of Infiltration".into()),
            bonuses: bonuses.into_iter().map(str::to_string).collect(),
            infusion_upgrade_flags: vec![],
            infusion_slots: vec![],
            attribute_adjustment: None,
            infix_upgrade: None,
            suffix_item_id: None,
            secondary_suffix_item_id: None,
            stat_choices: vec![],
        }),
    }
}

fn sheet_db() -> GameDb {
    let mut db = GameDb::empty_for_tests();
    db.itemstats.insert(
        STRONG_ID,
        ItemStat {
            id: STRONG_ID,
            name: "Strong".into(),
            attributes: vec![
                StatAttribute {
                    attribute: "Power".into(),
                    multiplier: 0.35,
                    value: 0,
                },
                StatAttribute {
                    attribute: "Precision".into(),
                    multiplier: 0.25,
                    value: 0,
                },
            ],
        },
    );
    db.itemstats.insert(
        RITUALIST_ID,
        three_stat(
            RITUALIST_ID,
            "Ritualist's",
            "ConditionDamage",
            "Expertise",
            "Concentration",
        ),
    );
    db.items.insert(
        INFILTRATION_RUNE,
        rune_item(
            INFILTRATION_RUNE,
            vec![
                "+25 Power",
                "+35 Precision",
                "+50 Power",
                "+65 Precision",
                "+100 Power",
                "+125 Precision",
            ],
        ),
    );
    db.traits.insert(
        WELLSPRING,
        trait_entry(
            WELLSPRING,
            "Wellspring",
            vec![Fact::BuffConversion {
                text: None,
                icon: None,
                source: Some("Power".into()),
                percent: Some(7.0),
                target: Some("Healing".into()),
            }],
        ),
    );
    db.traits.insert(
        LINGERING_MAGIC,
        trait_entry(
            LINGERING_MAGIC,
            "Lingering Magic",
            vec![
                attr_adjust("BoonDuration", 240),
                attr_adjust("BoonDuration", 120),
            ],
        ),
    );
    db.traits.insert(
        NATURAL_FORTITUDE,
        trait_entry(
            NATURAL_FORTITUDE,
            "Natural Fortitude",
            vec![attr_adjust("Vitality", 240)],
        ),
    );
    db.traits.insert(
        PRECISE_STRIKE,
        trait_entry(
            PRECISE_STRIKE,
            "Precise Strike",
            vec![Fact::Percent {
                text: Some("Critical Chance Increase".into()),
                icon: None,
                percent: Some(100.0),
            }],
        ),
    );
    db
}

fn ranger_grouped(groups: Groups, set1: ValidatedWeaponSet) -> ValidatedBuild {
    let mut build = ValidatedBuild {
        rune: Some(ValidatedItem {
            id: INFILTRATION_RUNE,
            name: "Superior Rune of Infiltration".into(),
        }),
        weapons: ValidatedWeapons {
            set1,
            set2: ValidatedWeaponSet {
                main_hand: Some("Greatsword".into()),
                off_hand: None,
            },
        },
        specializations: vec![ValidatedSpec {
            spec_id: 8,
            name: "Marksmanship".into(),
            elite: false,
            trait_ids: vec![
                WELLSPRING,
                LINGERING_MAGIC,
                NATURAL_FORTITUDE,
                PRECISE_STRIKE,
            ],
            trait_names: vec![
                "Wellspring".into(),
                "Lingering Magic".into(),
                "Natural Fortitude".into(),
                "Precise Strike".into(),
            ],
            all_trait_ids: vec![
                WELLSPRING,
                LINGERING_MAGIC,
                NATURAL_FORTITUDE,
                PRECISE_STRIKE,
            ],
        }],
        ..ValidatedBuild::default()
    };
    // Build-wide prefix first, then category overrides — the same expansion
    // the pre-slot `group.or(build-wide)` chain performed at read time.
    build.fill_gear_slots(prefix(STRONG_ID, "Strong"));
    if let Some(p) = &groups.armor {
        for &slot in &ARMOR_SLOTS {
            build.gear_slots.set(slot, p.clone());
        }
    }
    if let Some(p) = &groups.trinkets {
        for &slot in &TRINKET_SLOTS {
            build.gear_slots.set(slot, p.clone());
        }
    }
    if let Some(p) = &groups.weapons {
        for &slot in &WEAPON_SET1_SLOTS {
            build.gear_slots.set(slot, p.clone());
        }
    }
    build
}

fn all_strong_groups() -> Groups {
    Groups {
        armor: Some(prefix(STRONG_ID, "Strong")),
        trinkets: Some(prefix(STRONG_ID, "Strong")),
        weapons: Some(prefix(STRONG_ID, "Strong")),
    }
}

fn axe_axe() -> ValidatedWeaponSet {
    ValidatedWeaponSet {
        main_hand: Some("Axe".into()),
        off_hand: Some("Axe".into()),
    }
}

fn display_groups(build: &ValidatedBuild) -> GearPrefixGroups {
    // Category representatives on the slot map: helm for armor, amulet for
    // trinkets, set-1 main hand for weapons.
    let fallback = build
        .primary_prefix()
        .map(|prefix| prefix.name.clone())
        .unwrap_or_else(|| "Unknown".into());
    GearPrefixGroups {
        armor: build
            .prefix_for(GearSlot::Helm)
            .map(|prefix| prefix.name.clone())
            .unwrap_or_else(|| fallback.clone()),
        trinkets: build
            .prefix_for(GearSlot::Amulet)
            .map(|prefix| prefix.name.clone())
            .unwrap_or_else(|| fallback.clone()),
        weapons: build
            .prefix_for(GearSlot::WeaponSet1Main)
            .map(|prefix| prefix.name.clone())
            .unwrap_or_else(|| fallback.clone()),
    }
}

fn wvw_roam() -> ScenarioSpec {
    ScenarioSpec {
        game_mode: GameMode::WvW,
        combat_tier: CombatTier::Solo,
        combat_kind: CombatKind::StrikeSpike,
        target_profile: TargetProfile::Single,
        optimization_target: OptimizationTarget {
            label: "Roam".into(),
        },
        patch_id: None,
        objective_profile_id: None,
    }
}

fn dummy_rotation() -> SimulationResult {
    SimulationResult {
        duration_ms: 8_000,
        strike_dps: 1_000.0,
        condition_dps: 0.0,
        total_dps: 1_000.0,
        condition_uptime: HashMap::new(),
        buff_uptime: HashMap::new(),
        skill_usage: Vec::new(),
        stunbreak_count: 1,
        has_stability: true,
        stability_uptime: 0.5,
        cleanse_count: 1,
        cleanse_rate_per_20s: 2.0,
        healing_per_second: 0.0,
        control_uptime: 0.0,
        might_stacks_avg: 0.0,
        boon_equivalents: 0.0,
        has_mobility_out: true,
        escape_kinds: 1,
        has_strip: false,
        has_corrupt: false,
        downed: true,
        finished: true,
        has_interrupt: true,
        has_cover_answer: true,
        damage_per_second: Vec::new(),
        buff_presence_per_second: HashMap::new(),
        honesty: Default::default(),
        wvw: Some(WvwCombatReport {
            duration_ms: 8_000,
            target_reached_at_ms: Some(6_000),
            chain_completed: true,
            target_reached: true,
            peak_protected_damage_2s: 4_000.0,
            remaining_health_ratio: 0.4,
            repeatable: true,
            ..WvwCombatReport::default()
        }),
    }
}

fn ranked_report(intent: f64, combat: CombatPerformance) -> RefereeReport {
    RefereeReport {
        scenario: wvw_roam(),
        stats: stats::StatBlock::default(),
        modifiers: DamageModifiers::default(),
        combat_solo: combat.clone(),
        combat_party: combat.clone(),
        combat_squad: combat.clone(),
        primary_combat: combat,
        rotation: Some(dummy_rotation()),
        viability: ViabilityReport {
            is_viable: true,
            shortfall: 0.0,
            gates: vec![GateResult {
                gate: ViabilityGate::StabilityAccess,
                skipped: false,
                passed: true,
                note: "equalized".into(),
            }],
        },
        user_intent_score: intent,
        raw_direction_score: -1.0,
        ranked_direction_score: -1.0,
        intent_similarity: None,
        intent_alignment: None,
        realized: Default::default(),
        stat_direction_score: -1.0,
        quality: DataQuality::Verified,
        quality_reasons: Vec::new(),
    }
}

#[test]
fn grouped_active_set_locks_source_backed_ranger_sheet() {
    let db = sheet_db();
    let build = ranger_grouped(all_strong_groups(), axe_axe());
    let (stats, _) = calculate_validated_stats(&build, &db, "Ranger", &BalanceContext::wvw());
    let derived = stats::compute_derived(&stats, "Ranger");

    // Armor 439/315 + trinkets 692/466 + axe/axe 250/180 + rune 175/225 + base 1000.
    assert_eq!(stats.power.round() as i32, 2556);
    assert_eq!(stats.precision.round() as i32, 2186);
    assert_eq!(stats.toughness.round() as i32, 1000);
    assert_eq!(stats.vitality.round() as i32, 1240);
    assert_eq!(stats.concentration.round() as i32, 120);
    assert_eq!(stats.healing_power.round() as i32, 179);
    assert_eq!(stats.ferocity.round() as i32, 0);
    assert_eq!(derived.health.round() as i32, 18_322);
    assert_eq!(derived.armor.round() as i32, 2_118);
    let expected_crit = crate::data::universal_formulas::formulas()
        .crit_chance(stats.precision)
        .clamp(0.0, 100.0);
    assert!(
        (derived.crit_chance - expected_crit).abs() < 1e-9,
        "Precise Strike 100% tooltip must not enter standing crit: {} vs {}",
        derived.crit_chance,
        expected_crit
    );
}

#[test]
fn inactive_weapon_set_does_not_inflate_grouped_sheet() {
    let db = sheet_db();
    let mut with_set2 = ranger_grouped(all_strong_groups(), axe_axe());
    let mut without_set2 = with_set2.clone();
    without_set2.weapons.set2 = ValidatedWeaponSet::default();
    with_set2.weapons.set2 = ValidatedWeaponSet {
        main_hand: Some("Greatsword".into()),
        off_hand: None,
    };
    let (a, _) = calculate_validated_stats(&with_set2, &db, "Ranger", &BalanceContext::wvw());
    let (b, _) = calculate_validated_stats(&without_set2, &db, "Ranger", &BalanceContext::wvw());
    assert_eq!(a.power, b.power);
    assert_eq!(a.precision, b.precision);
}

#[test]
fn mixed_groups_spend_each_prefix_not_the_fallback() {
    let db = sheet_db();
    let mixed = ranger_grouped(
        Groups {
            armor: Some(prefix(STRONG_ID, "Strong")),
            trinkets: Some(prefix(RITUALIST_ID, "Ritualist's")),
            weapons: Some(prefix(STRONG_ID, "Strong")),
        },
        axe_axe(),
    );
    let uniform = ranger_grouped(all_strong_groups(), axe_axe());
    let (mixed_stats, _) = calculate_validated_stats(&mixed, &db, "Ranger", &BalanceContext::wvw());
    let (uniform_stats, _) =
        calculate_validated_stats(&uniform, &db, "Ranger", &BalanceContext::wvw());
    assert!(
        mixed_stats.power < uniform_stats.power,
        "Ritualist trinkets must drop Power below all-Strong"
    );
    assert!(mixed_stats.condition_damage > 0.0);
    assert_eq!(display_groups(&mixed).trinkets, "Ritualist's");
}

#[test]
fn inherit_fallback_matches_explicit_group_and_search_identity() {
    let explicit = ranger_grouped(all_strong_groups(), axe_axe());
    let inherited = ranger_grouped(
        Groups {
            armor: None,
            trinkets: Some(prefix(STRONG_ID, "Strong")),
            weapons: Some(prefix(STRONG_ID, "Strong")),
        },
        axe_axe(),
    );
    assert_eq!(explicit.gear_identity(), inherited.gear_identity());
    assert_eq!(display_groups(&inherited).armor, "Strong");

    let db = sheet_db();
    let (a, _) = calculate_validated_stats(&explicit, &db, "Ranger", &BalanceContext::wvw());
    let (b, _) = calculate_validated_stats(&inherited, &db, "Ranger", &BalanceContext::wvw());
    assert_eq!(a.power, b.power);
    assert_eq!(a.precision, b.precision);

    let empty_groups = ranger_grouped(Groups::default(), axe_axe());
    assert_eq!(empty_groups.gear_identity(), explicit.gear_identity());
    let (empty_stats, _) =
        calculate_validated_stats(&empty_groups, &db, "Ranger", &BalanceContext::wvw());
    assert_eq!(empty_stats.power.round() as i32, 2556);
    assert_eq!(empty_stats.precision.round() as i32, 2186);
    assert_eq!(a.power.round() as i32, 2556);
    assert_eq!(a.precision.round() as i32, 2186);
}

#[test]
fn grouped_prefixes_round_trip_through_saved_build_shape() {
    let groups = GearPrefixGroups {
        armor: "Strong".into(),
        trinkets: "Ritualist's".into(),
        weapons: "Strong".into(),
    };
    let json = serde_json::to_string(&groups).unwrap();
    let loaded: GearPrefixGroups = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded, groups);
}

#[test]
fn wvw_roam_weights_flip_direct_vs_condition_control() {
    let direct = CombatPerformance {
        strike_dps_index: 3_000.0,
        ..CombatPerformance::default()
    };
    let condition_control = CombatPerformance {
        strike_dps_index: 600.0,
        condition_dps_index: 3_500.0,
        condi_duration_pct: 100.0,
        boon_duration_pct: 50.0,
        ..CombatPerformance::default()
    };
    let power_weights = OptimizationWeights {
        power: 0.9,
        condition: 0.1,
        boon_support: 0.0,
        healing: 0.0,
        sustain: 0.0,
        control: 0.1,
    };
    let condi_weights = OptimizationWeights {
        power: 0.1,
        condition: 0.8,
        boon_support: 0.0,
        healing: 0.0,
        sustain: 0.0,
        control: 0.8,
    };

    let power_a = ranked_report(score_with_weights(&direct, &power_weights), direct.clone());
    let power_b = ranked_report(
        score_with_weights(&condition_control, &power_weights),
        condition_control.clone(),
    );
    assert!(
        referee::search_rank(&power_a) > referee::search_rank(&power_b),
        "direct weights must prefer candidate A"
    );

    let condi_a = ranked_report(score_with_weights(&direct, &condi_weights), direct);
    let condi_b = ranked_report(
        score_with_weights(&condition_control, &condi_weights),
        condition_control,
    );
    assert!(
        referee::search_rank(&condi_b) > referee::search_rank(&condi_a),
        "raised Condition+Control must prefer candidate B"
    );
}
