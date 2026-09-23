//! One test per combat-math permutation. ArenaNet supplies facts; we interpret them.
//! Chat/UI work waits until these pass.

use std::collections::HashMap;

use gw2_api::models::{
    Fact, InfixBuff, InfixUpgrade, Item, ItemDetails, ItemStat, PvpAmulet, StatAttribute, Trait,
};
use gw2_core::types::GameMode;
use gw2_optimizer::balance::BalanceContext;
use gw2_optimizer::combat;
use gw2_optimizer::data::{self, DataQuality};
use gw2_optimizer::engine;
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::rotation::combat_model::EnemyDummy;
use gw2_optimizer::rotation::simulator::{simulate_with, SimParams};
use gw2_optimizer::rotation::{RotationSkill, SkillEffect, SkillSlot};
use gw2_optimizer::stats;
use gw2_optimizer::synergy::{self, DamageCategory, DurationKind, NormalizedEffect};

fn almost(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

fn item(id: u32, name: &str, details: Option<ItemDetails>) -> Item {
    Item {
        id,
        name: name.into(),
        description: None,
        icon: None,
        item_type: "UpgradeComponent".into(),
        rarity: "Exotic".into(),
        level: 80,
        vendor_value: None,
        chat_link: None,
        default_skin: None,
        flags: vec![],
        game_types: vec![],
        restrictions: vec![],
        details,
    }
}

fn details_bonuses(bonuses: Vec<&str>) -> ItemDetails {
    ItemDetails {
        description: None,
        detail_type: Some("Rune".into()),
        weight_class: None,
        defense: None,
        damage_type: None,
        min_power: None,
        max_power: None,
        suffix: None,
        bonuses: bonuses.into_iter().map(str::to_string).collect(),
        infusion_upgrade_flags: vec![],
        infusion_slots: vec![],
        attribute_adjustment: None,
        infix_upgrade: None,
        suffix_item_id: None,
        secondary_suffix_item_id: None,
        stat_choices: vec![],
    }
}

fn details_buff(description: &str) -> ItemDetails {
    ItemDetails {
        description: None,
        detail_type: Some("Sigil".into()),
        weight_class: None,
        defense: None,
        damage_type: None,
        min_power: None,
        max_power: None,
        suffix: None,
        bonuses: vec![],
        infusion_upgrade_flags: vec![],
        infusion_slots: vec![],
        attribute_adjustment: None,
        infix_upgrade: Some(InfixUpgrade {
            id: None,
            attributes: vec![],
            buff: Some(InfixBuff {
                skill_id: None,
                description: Some(description.into()),
            }),
        }),
        suffix_item_id: None,
        secondary_suffix_item_id: None,
        stat_choices: vec![],
    }
}

fn extract_rune(bonus: &str) -> combat::DamageModifiers {
    let rune = item(1, "Test Rune", Some(details_bonuses(vec![bonus])));
    let mut items = HashMap::new();
    items.insert(1, rune);
    combat::extract_damage_modifiers(
        &[],
        Some(1),
        &[],
        None,
        &HashMap::new(),
        &items,
        &BalanceContext::pve(),
    )
}

fn extract_sigil(name: &str, buff: Option<&str>, ctx: &BalanceContext) -> combat::DamageModifiers {
    let sigil = item(
        2,
        name,
        buff.map(details_buff)
            .or_else(|| Some(details_bonuses(vec![]))),
    );
    let mut items = HashMap::new();
    items.insert(2, sigil);
    combat::extract_damage_modifiers(&[], None, &[2], None, &HashMap::new(), &items, ctx)
}

fn extract_relic(name: &str, desc: &str) -> combat::DamageModifiers {
    let mut relic = item(3, name, None);
    relic.item_type = "Relic".into();
    relic.description = Some(desc.into());
    let mut items = HashMap::new();
    items.insert(3, relic);
    combat::extract_damage_modifiers(
        &[],
        None,
        &[],
        Some(3),
        &HashMap::new(),
        &items,
        &BalanceContext::pve(),
    )
}

fn extract_trait_percent(text: &str, pct: f64) -> combat::DamageModifiers {
    let t = Trait {
        id: 9,
        name: "Test".into(),
        icon: None,
        description: None,
        specialization: 0,
        tier: 1,
        order: 0,
        slot: "Major".into(),
        facts: vec![Fact::Percent {
            text: Some(text.into()),
            icon: None,
            percent: Some(pct),
        }],
        traited_facts: vec![],
        skills: vec![],
    };
    let mut traits = HashMap::new();
    traits.insert(9, t);
    combat::extract_damage_modifiers(
        &[9],
        None,
        &[],
        None,
        &traits,
        &HashMap::new(),
        &BalanceContext::pve(),
    )
}

fn auto(id: u32) -> RotationSkill {
    RotationSkill {
        targets: 1,
        categories: Vec::new(),
        slot_name: None,
        skill_id: id,
        name: "Auto".into(),
        slot: SkillSlot::Weapon1,
        cast_time_ms: 500,
        cooldown_ms: 0,
        effects: vec![SkillEffect::StrikeDamage {
            hit_count: 1,
            dmg_multiplier: 1.0,
        }],
        next_chain: None,
        is_stunbreak: false,
        reaches_allies: false,
        weapon_set: 0,
    }
}

#[test]
fn gear_itemstat_round_helm_major() {
    // Ascended helm Berserker's: adj 180 × 0.35 → 63 (API:2/items/48075 Zojja's Visor).
    assert!(almost(stats::itemstat_value(180.0, 0.35, 0), 63.0));
    assert!(almost(stats::itemstat_value(180.0, 0.25, 0), 45.0));
    let helm = data::slot_budgets::slot_budgets()
        .get(
            data::slot_budgets::SlotType::Helm,
            data::slot_budgets::StatShape::ThreeStat,
        )
        .expect("helm ThreeStat");
    assert_eq!(helm.major, 63);
    assert_eq!(helm.minor, 45);
}

#[test]
fn gear_itemstat_round_with_additive_value() {
    // Selectable prefix: round(141 * 0.35 + 32) = 81.
    assert!(almost(stats::itemstat_value(141.0, 0.35, 32), 81.0));
    assert!(almost(stats::itemstat_value(141.0, 0.25, 18), 53.0));
}

#[test]
fn crit_chance_from_precision() {
    let f = data::universal_formulas::formulas();
    assert!(almost(f.crit_chance(895.0), 0.0));
    assert!(almost(f.crit_chance(895.0 + 21.0), 1.0));
    assert!(almost(f.crit_chance(1000.0), (1000.0 - 895.0) / 21.0));
}

#[test]
fn crit_damage_from_ferocity() {
    let f = data::universal_formulas::formulas();
    assert!(almost(f.crit_damage(0.0), 150.0));
    assert!(almost(f.crit_damage(15.0), 151.0));
    assert!(almost(f.crit_damage(150.0), 160.0));
}

#[test]
fn condition_duration_expertise_cap() {
    let ctx = BalanceContext::pve();
    let cap = data::universal_formulas::formulas().condition_duration_cap;
    assert!(almost(
        combat::condition_duration_bonus(1500.0, 0.0, 0.0, cap, &ctx),
        1.0
    ));
    assert!(almost(
        combat::condition_duration_bonus(3000.0, 0.0, 0.0, cap, &ctx),
        1.0
    ));
    assert!(almost(
        combat::condition_duration_bonus(0.0, 0.0, 0.0, cap, &ctx),
        0.0
    ));
}

#[test]
fn burning_tick_all_modes() {
    let c = data::conditions();
    let expected = 0.155 * 1000.0 + 131.0;
    for mode in [GameMode::PvE, GameMode::PvP, GameMode::WvW] {
        assert!(
            almost(c.tick_damage("Burning", 1000.0, mode.clone()), expected),
            "{mode:?}"
        );
    }
}

#[test]
fn torment_stationary_pve_vs_wvw() {
    let c = data::conditions();
    assert!(almost(
        c.torment_tick(1000.0, GameMode::PvE, false),
        0.09 * 1000.0 + 31.8
    ));
    assert!(almost(
        c.torment_tick(1000.0, GameMode::WvW, false),
        0.07 * 1000.0 + 26.0
    ));
}

#[test]
fn confusion_on_skill_use_pve_vs_wvw() {
    let c = data::conditions();
    assert!(almost(
        c.confusion_tick(1000.0, GameMode::PvE, true),
        0.0325 * 1000.0 + 16.24
    ));
    assert!(almost(
        c.confusion_tick(1000.0, GameMode::WvW, true),
        0.0975 * 1000.0 + 49.5
    ));
}

#[test]
fn stack_cap_json_and_sim() {
    let c = data::conditions();
    assert_eq!(c.max_stacks("Bleeding"), Some(1500));
    assert_eq!(c.max_stacks("Vulnerability"), Some(25));

    let bleed_skill = RotationSkill {
        targets: 1,
        categories: Vec::new(),
        slot_name: None,
        skill_id: 10,
        name: "Bleed Dump".into(),
        slot: SkillSlot::Weapon2,
        cast_time_ms: 100,
        cooldown_ms: 60_000,
        effects: vec![SkillEffect::ApplyCondition {
            condition: "Bleeding".into(),
            stacks: 2000,
            duration_ms: 60_000,
        }],
        next_chain: None,
        is_stunbreak: false,
        reaches_allies: false,
        weapon_set: 0,
    };
    let vuln_skill = RotationSkill {
        targets: 1,
        categories: Vec::new(),
        slot_name: None,
        skill_id: 11,
        name: "Vuln Dump".into(),
        slot: SkillSlot::Weapon2,
        cast_time_ms: 100,
        cooldown_ms: 60_000,
        effects: vec![
            SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 0.01,
            },
            SkillEffect::ApplyCondition {
                condition: "Vulnerability".into(),
                stacks: 40,
                duration_ms: 60_000,
            },
        ],
        next_chain: None,
        is_stunbreak: false,
        reaches_allies: false,
        weapon_set: 0,
    };

    let pve = simulate_with(
        std::slice::from_ref(&bleed_skill),
        5_000,
        &SimParams::basic(0.0, 0.0, 1100.0),
        EnemyDummy::open(),
    );
    let mut pvp_params = SimParams::basic(0.0, 0.0, 1100.0);
    pvp_params.mode = GameMode::PvP;
    let pvp = simulate_with(&[bleed_skill], 5_000, &pvp_params, EnemyDummy::open());
    let vuln = simulate_with(
        &[vuln_skill],
        5_000,
        &SimParams::basic(2000.0, 0.0, 1100.0),
        EnemyDummy::open(),
    );

    let pve_bleed = *pve.condition_uptime.get("Bleeding").expect("pve bleed");
    let pvp_bleed = *pvp.condition_uptime.get("Bleeding").expect("pvp bleed");
    let vuln_avg = *vuln.condition_uptime.get("Vulnerability").expect("vuln");
    // First sim tick is before the dump skill fires, so average stacks is
    // cap * (ticks_after_apply / duration_s), not the raw cap. Wiki 2026-08-29:
    // intensity shares 1500 in every mode (no sourced PvP 100).
    assert!(
        (pve_bleed - 1200.0).abs() < 0.05 && (pvp_bleed - 1200.0).abs() < 0.05,
        "PvE/PvP bleed cap 1500 → avg 1200, got {pve_bleed}/{pvp_bleed}"
    );
    assert!(
        (vuln_avg / pve_bleed - 25.0 / 1500.0).abs() < 0.001,
        "vuln/bleed should match 25/1500, got {vuln_avg}/{pve_bleed}"
    );
}

#[test]
fn rune_burning_duration_without_plus() {
    let mods = extract_rune("7% Burning Duration");
    let v = &mods.specific_condi_duration["Burning"];
    assert_eq!(v.len(), 1);
    assert!(almost(v[0], 0.07));
}

#[test]
fn rune_negative_percent_is_signed() {
    let mods = extract_rune("-10% Damage");
    assert_eq!(mods.strike_pct.len(), 1);
    assert!(almost(mods.strike_pct[0], -0.10));
    let mods_u = extract_rune("−10% Damage");
    assert_eq!(mods_u.strike_pct.len(), 1);
    assert!(almost(mods_u.strike_pct[0], -0.10));
}

#[test]
fn sigil_buff_description_not_double_counted() {
    let mods = extract_sigil(
        "Superior Sigil of Force",
        Some("+5% Damage"),
        &BalanceContext::pve(),
    );
    assert_eq!(mods.strike_add_pct, vec![0.05]);

    let effects = synergy::extract_sigil_effects(
        &item(
            2,
            "Superior Sigil of Force",
            Some(details_buff("+5% Damage")),
        ),
        &BalanceContext::pve(),
    );
    let strike: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            NormalizedEffect::DamageModifier {
                category: DamageCategory::Strike,
                percent,
            } => Some(*percent),
            _ => None,
        })
        .collect();
    assert_eq!(strike, vec![5.0]);
}

#[test]
fn sigil_force_name_fallback_is_mode_split() {
    let pve = extract_sigil("Superior Sigil of Force", None, &BalanceContext::pve());
    let pvp = extract_sigil("Superior Sigil of Force", None, &BalanceContext::pvp());
    assert_eq!(pve.strike_add_pct, vec![0.05]);
    assert_eq!(pvp.strike_add_pct, vec![0.03]);
}

#[test]
fn scholar_style_90_health_is_uptime_not_skip() {
    let mods = extract_trait_percent("Damage increased while above 90% health", 10.0);
    assert_eq!(mods.strike_pct.len(), 1);
    assert!(almost(mods.strike_pct[0], 0.09));
    assert!(mods.unparsed.is_empty());
}

#[test]
fn crit_chance_percent_fact() {
    let mods = extract_trait_percent("Critical Chance increased", 7.0);
    assert_eq!(mods.crit_chance_pct, vec![7.0]);
    assert!(mods.strike_pct.is_empty());
    assert!(mods.crit_damage_pct.is_empty());
}

#[test]
fn unparsed_percent_stamps_provisional() {
    let mods = extract_rune("7% Ferocity");
    assert!(!mods.unparsed.is_empty());
    let (q, _) = engine::quality_from_modifiers(&mods, &[], false, "PvE");
    assert_eq!(q, DataQuality::Provisional);
}

#[test]
fn alacrity_recharges_skills_faster() {
    let cd_skill = RotationSkill {
        targets: 1,
        categories: Vec::new(),
        slot_name: None,
        skill_id: 20,
        name: "Big Hit".into(),
        slot: SkillSlot::Weapon2,
        cast_time_ms: 250,
        cooldown_ms: 8_000,
        effects: vec![SkillEffect::StrikeDamage {
            hit_count: 1,
            dmg_multiplier: 2.0,
        }],
        next_chain: None,
        is_stunbreak: false,
        reaches_allies: false,
        weapon_set: 0,
    };
    let mut auto_alac = auto(1);
    auto_alac.effects.push(SkillEffect::ApplyBuff {
        buff: "Alacrity".into(),
        stacks: 1,
        duration_ms: 30_000,
    });
    // 20s: 4th Big Hit with 25% alac comes up mid-auto and misses the clock.
    // 25s: no-alac also gets a 4th (t=24000). 22s is the gap (4 vs 3).
    let without = simulate_with(
        &[auto(1), cd_skill.clone()],
        22_000,
        &SimParams::basic(2000.0, 0.0, 1100.0),
        EnemyDummy::open(),
    );
    let with = simulate_with(
        &[auto_alac, cd_skill],
        22_000,
        &SimParams::basic(2000.0, 0.0, 1100.0),
        EnemyDummy::open(),
    );
    let casts = |r: &gw2_optimizer::rotation::SimulationResult| {
        r.skill_usage
            .iter()
            .find(|u| u.name == "Big Hit")
            .map(|u| u.cast_count)
            .unwrap_or(0)
    };
    assert!(
        casts(&with) > casts(&without),
        "alac {} vs no-alac {}",
        casts(&with),
        casts(&without)
    );
}

#[test]
fn strike_crit_factor_only_when_precision_positive() {
    let skills = vec![auto(1)];
    let mut none = SimParams::basic(2000.0, 0.0, 1100.0);
    none.precision = 0.0;
    let mut some = none.clone();
    some.precision = 2000.0;
    some.ferocity = 0.0;
    let a = simulate_with(&skills, 5_000, &none, EnemyDummy::open());
    let b = simulate_with(&skills, 5_000, &some, EnemyDummy::open());
    assert!(
        b.strike_dps > a.strike_dps,
        "{} vs {}",
        b.strike_dps,
        a.strike_dps
    );
}

#[test]
fn pvp_uses_amulet_not_sixteen_slot_budgets() {
    let mut db = GameDb::empty_for_tests();
    db.itemstats.insert(
        584,
        ItemStat {
            id: 584,
            name: "Berserker's".into(),
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
                StatAttribute {
                    attribute: "CritDamage".into(),
                    multiplier: 0.25,
                    value: 0,
                },
            ],
        },
    );
    let mut attrs = HashMap::new();
    attrs.insert("Power".into(), 9999);
    attrs.insert("Precision".into(), 1111);
    attrs.insert("CritDamage".into(), 1111);
    db.pvp_amulets.insert(
        4,
        PvpAmulet {
            id: 4,
            name: "Berserker Amulet".into(),
            icon: None,
            attributes: attrs,
        },
    );

    let matched = engine::match_pvp_amulet(&db, "Berserker's").expect("amulet");
    assert_eq!(matched.attributes["Power"], 9999);

    let mut pvp_stats = stats::base_stats();
    engine::apply_optimized_gear_stats(&mut pvp_stats, &db, Some(584), &BalanceContext::pvp());
    let base = data::universal_formulas::formulas().base_primary_attribute;
    assert!(
        almost(pvp_stats.power, base + 9999.0),
        "pvp power {}",
        pvp_stats.power
    );

    let mut pve_stats = stats::base_stats();
    engine::apply_optimized_gear_stats(&mut pve_stats, &db, Some(584), &BalanceContext::pve());
    assert!(
        (pve_stats.power - (base + 9999.0)).abs() > 100.0,
        "PvE must not use the amulet; got {}",
        pve_stats.power
    );
}

#[test]
fn synergy_rune_without_plus_matches_combat() {
    let rune = item(
        1,
        "Superior Rune of the Firebrand",
        Some(details_bonuses(vec!["7% Burning Duration"])),
    );
    let effects = synergy::extract_rune_effects(&rune);
    assert!(effects.iter().any(|e| matches!(
        e,
        NormalizedEffect::DurationBonus {
            kind: DurationKind::SpecificCondition(c),
            percent
        } if c == "Burning" && (*percent - 7.0).abs() < 0.01
    )));
}

#[test]
fn rune_might_duration_is_boon_duration() {
    let mods = extract_rune("+20% Might Duration");
    assert_eq!(mods.boon_duration_pct, vec![0.20]);
    assert!(mods.condi_duration_pct.is_empty());
}

#[test]
fn rune_quickness_duration_is_boon_duration() {
    let mods = extract_rune("+20% Quickness Duration");
    assert_eq!(mods.boon_duration_pct, vec![0.20]);
}

#[test]
fn rune_incoming_condition_duration_is_not_outgoing() {
    let mods = extract_rune("-10% Incoming Condition Duration");
    assert!(mods.condi_duration_pct.is_empty());
    assert!(mods.strike_pct.is_empty());
}

#[test]
fn rune_scholar_health_threshold_uses_uptime() {
    let mods = extract_rune("+5% damage while health is above 90%");
    assert_eq!(mods.strike_pct.len(), 1);
    assert!(almost(mods.strike_pct[0], 0.045));
}

#[test]
fn sigil_accuracy_buff_is_crit_chance() {
    let mods = extract_sigil(
        "Superior Sigil of Accuracy",
        Some("+7% Critical Chance"),
        &BalanceContext::pve(),
    );
    assert_eq!(mods.crit_chance_pct, vec![7.0]);
    assert!(mods.strike_pct.is_empty());
}

#[test]
fn sigil_agony_and_smoldering_inflicted_duration() {
    let agony = extract_sigil(
        "Superior Sigil of Agony",
        Some("Increase Inflicted Torment Duration: 20%"),
        &BalanceContext::pve(),
    );
    assert!(almost(agony.specific_condi_duration["Torment"][0], 0.20));
    let smoldering = extract_sigil(
        "Superior Sigil of Smoldering",
        Some("Increase Inflicted Burning Duration: 20%"),
        &BalanceContext::pve(),
    );
    assert!(almost(
        smoldering.specific_condi_duration["Burning"][0],
        0.20
    ));
}

#[test]
fn sigil_slaying_keeps_unconditional_strike_only() {
    let mods = extract_sigil(
        "Superior Sigil of Demon Slaying",
        Some("+7% Strike Damage vs. Demons. +3% Strike Damage."),
        &BalanceContext::pve(),
    );
    assert_eq!(mods.strike_pct, vec![0.03]);
}

#[test]
fn sigil_earth_buff_applies_bleeding() {
    let sigil = item(
        2,
        "Superior Sigil of Earth",
        Some(details_buff("On critical hit, inflict bleeding.")),
    );
    let effects = synergy::extract_sigil_effects(&sigil, &BalanceContext::pve());
    assert!(effects.iter().any(|e| matches!(
        e,
        NormalizedEffect::AppliesStatus {
            status,
            is_condition: true,
            ..
        } if status == "Bleeding"
    )));
}

#[test]
fn relic_fireworks_elite_strike_expected_value() {
    let mods = extract_relic(
        "Relic of Fireworks",
        "After using an elite skill, gain a stack of Fireworks. Deal increased strike damage for a duration. Refreshes duration on stack.",
    );
    assert_eq!(mods.strike_pct.len(), 1);
    // 6s buff / 40s elite CD — easy to press, too long to treat as permanent.
    assert!(almost(mods.strike_pct[0], 0.07 * (6.0 / 40.0)));
}

#[test]
fn relic_fireworks_long_recharge_weapon_skill() {
    let mods = extract_relic(
        "Relic of Fireworks",
        "After using a weapon skill with a recharge of 20 seconds or more, deal increased strike damage for a duration.",
    );
    assert_eq!(mods.strike_pct.len(), 1);
    assert!(almost(mods.strike_pct[0], 0.07 * (6.0 / 20.0)));
}

#[test]
fn relic_aristocracy_stacks_condition_duration() {
    let mods = extract_relic(
        "Relic of the Aristocracy",
        "Gain 3% condition duration when you grant a boon, up to a maximum of 5 stacks.",
    );
    assert_eq!(mods.condi_duration_pct.len(), 1);
    assert!(almost(mods.condi_duration_pct[0], 0.15));
}

#[test]
fn relic_thief_weapon_skill_strike() {
    let mods = extract_relic(
        "Relic of the Thief",
        "After using a weapon skill with a resource cost, deal increased strike damage.",
    );
    assert_eq!(mods.strike_pct.len(), 1);
    assert!(almost(mods.strike_pct[0], 0.07));
}

#[test]
fn relic_weapon_swap_is_easy_trigger() {
    let mods = extract_relic(
        "Relic of Nourys",
        "After swapping weapons, deal increased strike damage for a duration.",
    );
    assert_eq!(mods.strike_add_pct.len(), 1);
    assert!(almost(mods.strike_add_pct[0], 0.07 * (6.0 / 9.0)));
}

#[test]
fn relic_evade_is_easy_trigger() {
    let mods = extract_relic(
        "Relic of Evasion",
        "After evading an attack, deal increased strike damage for a duration.",
    );
    assert_eq!(mods.strike_pct.len(), 1);
    assert!(almost(mods.strike_pct[0], 0.07 * (6.0 / 8.0)));
}

#[test]
fn sigil_strike_disabled_is_easy_trigger() {
    let mods = extract_sigil(
        "Superior Sigil of Impact",
        Some("Deal increased strike damage when you hit a stunned or disabled foe."),
        &BalanceContext::pve(),
    );
    assert_eq!(mods.strike_add_pct.len(), 1);
    assert!(almost(mods.strike_add_pct[0], 0.07 * (6.0 / 15.0)));
}

#[test]
fn relic_kill_stacks_are_not_relied_on() {
    let mods = extract_relic(
        "Relic of Bloodlust",
        "Gain 2% strike damage when you kill a foe, up to a maximum of 25 stacks. Lost on death.",
    );
    assert!(mods.strike_pct.is_empty());
}

#[test]
fn relic_monk_healing() {
    let mods = extract_relic("Relic of the Monk", "Increase your healing effectiveness.");
    assert_eq!(mods.healing_pct.len(), 1);
    assert!(almost(mods.healing_pct[0], 0.10));
}

#[test]
fn relic_nightmare_is_not_ten_percent_condition_duration() {
    let mods = extract_relic(
        "Relic of the Nightmare",
        "Your elite skill inflicts fear and pulses poison around you.",
    );
    assert!(mods.condi_duration_pct.is_empty());
}

#[test]
fn relic_mirage_inflicts_torment() {
    let mut relic = item(3, "Relic of the Mirage", None);
    relic.item_type = "Relic".into();
    relic.description = Some("After using an elite skill, inflict torment.".into());
    let effects = synergy::extract_relic_effects(&relic);
    assert!(effects.iter().any(|e| matches!(
        e,
        NormalizedEffect::AppliesStatus {
            status,
            is_condition: true,
            ..
        } if status == "Torment"
    )));
}
