//! Hand-authored Necromancer Reaper slice for the simulator-trust experiments
//! (`specs/004-simulator-trust`, `docs/simulator-connection-audit.md`).
//!
//! FIXTURE ONLY. Every id, coefficient, cooldown and record value in this
//! module is invented so the experiments have something deterministic to
//! press. Nothing here is sourced game data and nothing here may be copied
//! into `data/`. The two ids that ARE real — Superior Sigil of Fire (24548)
//! and Superior Sigil of Force (24615) — are real on purpose: the referee
//! path reads the shipped `data/normalized_effects` files by source id, and
//! the unsupported-trigger control (CONN-00-06) needs the shipped on-crit
//! record to be selected for the equipped sigil. Path of Corruption keeps its
//! real trait id (1693) in `records()` for the same reason; the trait itself
//! is a Curses trait and is not in this build's specializations, so it only
//! reaches the timeline through injection.

use crate::balance::BalanceContext;
use crate::data::normalized_effects::{
    AmountMode, EffectCategory, HealthThreshold, NormalizedEffect, OperationType, SourceType,
    StackingRule, StatusOperation, TargetScope, TargetSide, TriggerRule, TriggerScope, UptimeModel,
    UptimeModelKind,
};
use crate::data::{EvidenceLevel, FactualValue};
use crate::gamedb::GameDb;
use crate::scenario::ScenarioSpec;
use crate::sigil_slots::SigilSlots;
use crate::validation::{
    ValidatedBuild, ValidatedItem, ValidatedSkills, ValidatedSpec, ValidatedWeaponSet,
    ValidatedWeapons,
};
use gw2_core::types::{GameMode, PrefixRef};
use std::collections::HashMap;

// Fixture ids (synthetic unless noted)
pub const SPEC_SPITE: u32 = 900;
pub const SPEC_SOUL_REAPING: u32 = 901;
pub const SPEC_REAPER: u32 = 902;

pub const GS_AUTO: u32 = 30000;
pub const GRAVEDIGGER: u32 = 30001;
pub const DEATH_SPIRAL: u32 = 30002;
pub const NIGHTFALL: u32 = 30003;
pub const GRASPING_DARKNESS: u32 = 30004;
pub const AXE_AUTO: u32 = 30010;
pub const GHASTLY_CLAWS: u32 = 30011;
pub const UNHOLY_FEAST: u32 = 30012;
pub const REAPERS_TOUCH: u32 = 30013;
pub const SPINAL_SHIVERS: u32 = 30014;
pub const REAPER_SHROUD: u32 = 30020;
pub const SHROUD_1: u32 = 30021;
pub const SHROUD_2: u32 = 30022;
pub const SHROUD_3: u32 = 30023;
pub const SHROUD_4: u32 = 30024;
/// The entry skill's flip: exits shroud (wiki `Death Shroud`).
pub const EXIT_SHROUD: u32 = 30025;
pub const SIGNET_OF_VAMPIRISM: u32 = 30030;
pub const WELL_OF_SUFFERING: u32 = 30031;
pub const WELL_OF_DARKNESS: u32 = 30032;
pub const YOU_ARE_ALL_WEAKLINGS: u32 = 30033;
pub const CHILLED_TO_THE_BONE: u32 = 30034;

/// Real GW2 item id, so the shipped on-crit record selects for it.
pub const SIGIL_OF_FIRE: u32 = 24548;
/// Real GW2 item id.
pub const SIGIL_OF_FORCE: u32 = 24615;
pub const RUNE_OF_THE_SCHOLAR: u32 = 99001;
pub const RELIC_OF_THE_THIEF: u32 = 99002;
pub const MARAUDER_ITEMSTAT: u32 = 99436;
/// Precision-free three-stat set for the zero-crit negative control.
pub const SOLDIER_ITEMSTAT: u32 = 99437;
/// Real GW2 trait id of Path of Corruption (Curses), used only in `records()`.
pub const PATH_OF_CORRUPTION: u32 = 1693;

fn skill_json(
    id: u32,
    name: &str,
    slot: &str,
    facts: serde_json::Value,
    description: &str,
) -> gw2_api::models::Skill {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": name,
        "description": description,
        "slot": slot,
        "professions": ["Necromancer"],
        "facts": facts,
    }))
    .expect("fixture skill")
}

fn damage(hits: u32, coefficient: f64, recharge: f64) -> serde_json::Value {
    serde_json::json!([
        {"type": "Damage", "hit_count": hits, "dmg_multiplier": coefficient},
        {"type": "Recharge", "value": recharge}
    ])
}

fn trait_json(id: u32, name: &str, spec: u32, tier: u32, slot: &str) -> gw2_api::models::Trait {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": name,
        "specialization": spec,
        "tier": tier,
        "order": 0,
        "slot": slot,
        "facts": [],
    }))
    .expect("fixture trait")
}

fn upgrade_json(id: u32, name: &str, kind: &str, bonuses: &[&str]) -> gw2_api::models::Item {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": name, "type": "UpgradeComponent", "rarity": "Exotic",
        "level": 60, "details": { "type": kind, "bonuses": bonuses }
    }))
    .expect("fixture upgrade")
}

/// Spec `id` with three minors (`id*100+1..=3`) and nine majors
/// (`id*100+11..=19`, three per tier); a hundred per spec so no spec's
/// majors collide with the next spec's minors.
fn spec_json(id: u32, name: &str, elite: bool) -> gw2_api::models::Specialization {
    gw2_api::models::Specialization {
        id,
        name: name.into(),
        profession: "Necromancer".into(),
        elite,
        minor_traits: (1..=3).map(|n| id * 100 + n).collect(),
        major_traits: (11..=19).map(|n| id * 100 + n).collect(),
        weapon_trait: None,
        icon: None,
        background: None,
        profession_icon: None,
        profession_icon_big: None,
    }
}

/// The three selected majors of a spec: tier 1, 2 and 3, first column.
fn chosen_majors(spec: u32) -> Vec<u32> {
    vec![spec * 100 + 11, spec * 100 + 14, spec * 100 + 17]
}

/// A `GameDb` holding exactly the Reaper slice: one profession, three
/// specializations with named traits, the greatsword / axe / focus weapon
/// skills, Reaper Shroud 1–4, the five slot skills, four upgrades and one
/// four-stat itemstat.
pub fn db() -> GameDb {
    let mut db = GameDb::empty_for_tests();

    for (id, name, elite) in [
        (SPEC_SPITE, "Spite", false),
        (SPEC_SOUL_REAPING, "Soul Reaping", false),
        (SPEC_REAPER, "Reaper", true),
    ] {
        let spec = spec_json(id, name, elite);
        // Word names, not "Spite 1.1": the validator's name matcher keys on
        // letters and would read every numbered major of a spec as one name.
        const TIERS: [&str; 3] = ["Adept", "Master", "Grandmaster"];
        const COLUMNS: [&str; 3] = ["Left", "Middle", "Right"];
        for (tier, minor) in spec.minor_traits.iter().enumerate() {
            db.traits.insert(
                *minor,
                trait_json(
                    *minor,
                    &format!("{name} {} Minor", TIERS[tier]),
                    id,
                    tier as u32 + 1,
                    "Minor",
                ),
            );
        }
        for (i, major) in spec.major_traits.iter().enumerate() {
            let tier = (i / 3) as u32 + 1;
            db.traits.insert(
                *major,
                trait_json(
                    *major,
                    &format!("{name} {} {}", TIERS[i / 3], COLUMNS[i % 3]),
                    id,
                    tier,
                    "Major",
                ),
            );
        }
        db.traits_by_spec.insert(
            id,
            spec.minor_traits
                .iter()
                .chain(&spec.major_traits)
                .copied()
                .collect(),
        );
        db.specializations.insert(id, spec);
    }

    let skills = vec![
        skill_json(GS_AUTO, "Dusk Strike", "Weapon_1", damage(1, 0.9, 0.0), ""),
        // Two hits so the channel-interrupt timing experiment has a later
        // hit to lose. Real Gravedigger is one hit; this is a fixture. The
        // Life Force facts feed the shroud experiments (US6).
        skill_json(
            GRAVEDIGGER,
            "Gravedigger",
            "Weapon_2",
            serde_json::json!([
                {"type": "Damage", "hit_count": 2, "dmg_multiplier": 1.2},
                {"type": "Percent", "text": "Life Force", "percent": 8.0},
                {"type": "Recharge", "value": 8.0}
            ]),
            "",
        ),
        skill_json(
            DEATH_SPIRAL,
            "Death Spiral",
            "Weapon_3",
            serde_json::json!([
                {"type": "Damage", "hit_count": 3, "dmg_multiplier": 0.55},
                {"type": "Percent", "text": "Life Force", "percent": 6.0},
                {"type": "Recharge", "value": 12.0}
            ]),
            "",
        ),
        skill_json(
            NIGHTFALL,
            "Nightfall",
            "Weapon_4",
            serde_json::json!([
                {"type": "ComboField", "field_type": "Dark"},
                {"type": "Recharge", "value": 20.0},
                {"type": "Time", "text": "Duration", "duration": 5}
            ]),
            "",
        ),
        skill_json(
            GRASPING_DARKNESS,
            "Grasping Darkness",
            "Weapon_5",
            serde_json::json!([
                {"type": "Damage", "hit_count": 1, "dmg_multiplier": 0.8},
                {"type": "Buff", "status": "Chilled", "duration": 3, "apply_count": 1},
                {"type": "Recharge", "value": 25.0}
            ]),
            "",
        ),
        skill_json(
            AXE_AUTO,
            "Rending Claws",
            "Weapon_1",
            damage(2, 0.35, 0.0),
            "",
        ),
        skill_json(
            GHASTLY_CLAWS,
            "Ghastly Claws",
            "Weapon_2",
            serde_json::json!([
                {"type": "Damage", "hit_count": 8, "dmg_multiplier": 0.2},
                {"type": "Percent", "text": "Life Force", "percent": 12.0},
                {"type": "Recharge", "value": 8.0}
            ]),
            "",
        ),
        skill_json(
            UNHOLY_FEAST,
            "Unholy Feast",
            "Weapon_3",
            damage(1, 0.6, 15.0),
            "",
        ),
        skill_json(
            REAPERS_TOUCH,
            "Reaper's Touch",
            "Weapon_4",
            damage(2, 0.5, 18.0),
            "",
        ),
        skill_json(
            SPINAL_SHIVERS,
            "Spinal Shivers",
            "Weapon_5",
            damage(1, 0.9, 25.0),
            "",
        ),
        // The shroud: entry skill with its flip, and the bar skills the
        // way the API lists them — `transform_skills` on the entry skill,
        // `Downed_*` slots and the elite specialization on each (R6).
        skill_json(
            REAPER_SHROUD,
            "Reaper's Shroud",
            "Profession_1",
            serde_json::json!([{"type": "Recharge", "value": 10.0}]),
            "",
        ),
        skill_json(
            EXIT_SHROUD,
            "Exit Reaper's Shroud",
            "Profession_1",
            serde_json::json!([]),
            "",
        ),
        skill_json(SHROUD_1, "Life Rend", "Downed_1", damage(1, 0.7, 0.0), ""),
        skill_json(
            SHROUD_2,
            "Death's Charge",
            "Downed_2",
            damage(3, 0.5, 8.0),
            "",
        ),
        skill_json(
            SHROUD_3,
            "Infusing Terror",
            "Downed_3",
            serde_json::json!([
                {"type": "Buff", "status": "Stability", "duration": 3, "apply_count": 3},
                {"type": "Recharge", "value": 20.0}
            ]),
            "",
        ),
        // Whirl finisher: the combo experiment's finisher into a dark field.
        skill_json(
            SHROUD_4,
            "Soul Spiral",
            "Downed_4",
            serde_json::json!([
                {"type": "Damage", "hit_count": 8, "dmg_multiplier": 0.3},
                {"type": "ComboFinisher", "finisher_type": "Whirl", "percent": 100},
                {"type": "Recharge", "value": 15.0}
            ]),
            "",
        ),
        skill_json(
            SIGNET_OF_VAMPIRISM,
            "Signet of Vampirism",
            "Heal",
            serde_json::json!([{"type": "Heal", "hit_count": 1}, {"type": "Recharge", "value": 25.0}]),
            "",
        ),
        // Dark field with five damage pulses.
        skill_json(
            WELL_OF_SUFFERING,
            "Well of Suffering",
            "Utility",
            serde_json::json!([
                {"type": "Damage", "hit_count": 5, "dmg_multiplier": 0.4},
                {"type": "ComboField", "field_type": "Dark"},
                {"type": "Time", "text": "Duration", "duration": 5},
                {"type": "Percent", "text": "Life Force", "percent": 5.0},
                {"type": "Recharge", "value": 35.0}
            ]),
            "",
        ),
        skill_json(
            WELL_OF_DARKNESS,
            "Well of Darkness",
            "Utility",
            serde_json::json!([
                {"type": "ComboField", "field_type": "Dark"},
                {"type": "Time", "text": "Duration", "duration": 5},
                {"type": "Recharge", "value": 40.0}
            ]),
            "",
        ),
        skill_json(
            YOU_ARE_ALL_WEAKLINGS,
            "\"You Are All Weaklings!\"",
            "Utility",
            serde_json::json!([
                {"type": "Buff", "status": "Might", "duration": 10, "apply_count": 3},
                {"type": "Recharge", "value": 25.0}
            ]),
            "",
        ),
        skill_json(
            CHILLED_TO_THE_BONE,
            "\"Chilled to the Bone!\"",
            "Elite",
            serde_json::json!([
                {"type": "Buff", "status": "Stun", "duration": 2, "apply_count": 1},
                {"type": "Recharge", "value": 90.0}
            ]),
            "",
        ),
    ];
    for mut skill in skills {
        match skill.id {
            REAPER_SHROUD => {
                skill.transform_skills = vec![SHROUD_1, SHROUD_2, SHROUD_3, SHROUD_4];
                skill.flip_skill = Some(EXIT_SHROUD);
            }
            SHROUD_1 | SHROUD_2 | SHROUD_3 | SHROUD_4 => {
                skill.specialization = Some(SPEC_REAPER);
            }
            _ => {}
        }
        db.skills_by_profession
            .entry("Necromancer".into())
            .or_default()
            .push(skill.id);
        // The validator only slots skills with a build-template palette id;
        // a synthetic 1:1 mapping is enough for that gate.
        if matches!(skill.slot.as_deref(), Some("Heal" | "Utility" | "Elite")) {
            db.skill_to_palette.insert(skill.id, skill.id);
            db.palette_to_skill.insert(skill.id, skill.id);
        }
        db.skills.insert(skill.id, skill);
    }

    let weapon = |flags: &[&str], skills: &[(u32, &str)]| gw2_api::models::WeaponInfo {
        specialization: None,
        flags: flags.iter().map(|f| f.to_string()).collect(),
        skills: skills
            .iter()
            .map(|(id, slot)| gw2_api::models::WeaponSkillRef {
                id: *id,
                slot: slot.to_string(),
            })
            .collect(),
    };
    let mut weapons = HashMap::new();
    weapons.insert(
        "Greatsword".to_string(),
        weapon(
            &["TwoHand"],
            &[
                (GS_AUTO, "Weapon_1"),
                (GRAVEDIGGER, "Weapon_2"),
                (DEATH_SPIRAL, "Weapon_3"),
                (NIGHTFALL, "Weapon_4"),
                (GRASPING_DARKNESS, "Weapon_5"),
            ],
        ),
    );
    weapons.insert(
        "Axe".to_string(),
        weapon(
            &["Mainhand"],
            &[
                (AXE_AUTO, "Weapon_1"),
                (GHASTLY_CLAWS, "Weapon_2"),
                (UNHOLY_FEAST, "Weapon_3"),
            ],
        ),
    );
    weapons.insert(
        "Focus".to_string(),
        weapon(
            &["Offhand"],
            &[(REAPERS_TOUCH, "Weapon_4"), (SPINAL_SHIVERS, "Weapon_5")],
        ),
    );
    db.professions.insert(
        "Necromancer".into(),
        gw2_api::models::Profession {
            id: "Necromancer".into(),
            name: "Necromancer".into(),
            code: Some(8),
            specializations: vec![SPEC_SPITE, SPEC_SOUL_REAPING, SPEC_REAPER],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        },
    );

    for item in [
        upgrade_json(
            SIGIL_OF_FIRE,
            "Superior Sigil of Fire",
            "Sigil",
            &["50% Chance on Critical: Trigger a Flame Blast for AoE damage. (Cooldown: 5s)"],
        ),
        upgrade_json(
            SIGIL_OF_FORCE,
            "Superior Sigil of Force",
            "Sigil",
            &["+5% Damage"],
        ),
        upgrade_json(
            RUNE_OF_THE_SCHOLAR,
            "Superior Rune of the Scholar",
            "Rune",
            &[
                "+25 Power",
                "+35 Ferocity",
                "+50 Power",
                "+65 Ferocity",
                "+100 Power",
                "+5% damage; +10% damage while your health is above 90%",
            ],
        ),
        upgrade_json(
            RELIC_OF_THE_THIEF,
            "Relic of the Thief",
            "Relic",
            &["Gain stacking damage bonus for successful hits. (Max stacks: 5)"],
        ),
    ] {
        match item.details.as_ref().and_then(|d| d.detail_type.as_deref()) {
            Some("Sigil") => db.sigils.push(item.id),
            Some("Rune") => db.runes.push(item.id),
            Some("Relic") => db.relics.push(item.id),
            _ => {}
        }
        db.items.insert(item.id, item);
    }

    for (id, name, attributes) in [
        (
            MARAUDER_ITEMSTAT,
            "Marauder",
            vec![
                ("Power", 0.35),
                ("Precision", 0.35),
                ("Vitality", 0.25),
                ("CritDamage", 0.25),
            ],
        ),
        (
            SOLDIER_ITEMSTAT,
            "Soldier",
            vec![("Power", 0.35), ("Toughness", 0.25), ("Vitality", 0.25)],
        ),
    ] {
        db.itemstats.insert(
            id,
            gw2_api::models::ItemStat {
                id,
                name: name.into(),
                attributes: attributes
                    .into_iter()
                    .map(|(attribute, multiplier)| gw2_api::models::StatAttribute {
                        attribute: attribute.into(),
                        multiplier,
                        value: 0,
                    })
                    .collect(),
            },
        );
    }

    db
}

fn spec(id: u32, name: &str, elite: bool, db: &GameDb) -> ValidatedSpec {
    let majors = chosen_majors(id);
    ValidatedSpec {
        spec_id: id,
        name: name.into(),
        elite,
        trait_names: majors.iter().map(|t| db.traits[t].name.clone()).collect(),
        all_trait_ids: (1..=3)
            .map(|n| id * 100 + n)
            .chain(majors.iter().copied())
            .collect(),
        trait_ids: majors,
    }
}

/// Greatsword on set 1 with Fire + Force, axe/focus on set 2 with no sigils,
/// Scholar rune, Thief relic, Marauder on every worn slot.
pub fn build() -> ValidatedBuild {
    let db = db();
    let mut build = ValidatedBuild {
        specializations: vec![
            spec(SPEC_SPITE, "Spite", false, &db),
            spec(SPEC_SOUL_REAPING, "Soul Reaping", false, &db),
            spec(SPEC_REAPER, "Reaper", true, &db),
        ],
        weapons: ValidatedWeapons {
            set1: ValidatedWeaponSet {
                main_hand: Some("Greatsword".into()),
                off_hand: None,
            },
            set2: ValidatedWeaponSet {
                main_hand: Some("Axe".into()),
                off_hand: Some("Focus".into()),
            },
        },
        skills: ValidatedSkills {
            heal: Some((SIGNET_OF_VAMPIRISM, "Signet of Vampirism".into())),
            utilities: vec![
                Some((WELL_OF_SUFFERING, "Well of Suffering".into())),
                Some((WELL_OF_DARKNESS, "Well of Darkness".into())),
                Some((YOU_ARE_ALL_WEAKLINGS, "\"You Are All Weaklings!\"".into())),
            ],
            elite: Some((CHILLED_TO_THE_BONE, "\"Chilled to the Bone!\"".into())),
            // The shroud bar itself comes from the entry skill's
            // `transform_skills` in the builder, as it does for real data.
            profession: vec![(REAPER_SHROUD, "Reaper's Shroud".into())],
        },
        rune: Some(ValidatedItem {
            id: RUNE_OF_THE_SCHOLAR,
            name: "Superior Rune of the Scholar".into(),
        }),
        sigils: vec![
            ValidatedItem {
                id: SIGIL_OF_FIRE,
                name: "Superior Sigil of Fire".into(),
            },
            ValidatedItem {
                id: SIGIL_OF_FORCE,
                name: "Superior Sigil of Force".into(),
            },
        ],
        sigil_seats: SigilSlots::new([Some(SIGIL_OF_FIRE), Some(SIGIL_OF_FORCE), None, None]),
        relic: Some(ValidatedItem {
            id: RELIC_OF_THE_THIEF,
            name: "Relic of the Thief".into(),
        }),
        ..ValidatedBuild::default()
    };
    build.fill_worn_gear_slots(PrefixRef {
        itemstat_id: MARAUDER_ITEMSTAT,
        name: "Marauder".into(),
    });
    build
}

// Sprint 2 variants (specs/005-wvw-proc-sites)

/// `build()` with Superior Sigil of Fire moved to set 2's main-hand seat and
/// Force kept on set 1: the weapon-swap experiments (US2).
pub fn build_with_set_two_fire() -> ValidatedBuild {
    let mut build = build();
    build.set_sigil_seats([
        Some(ValidatedItem {
            id: SIGIL_OF_FORCE,
            name: "Superior Sigil of Force".into(),
        }),
        None,
        Some(ValidatedItem {
            id: SIGIL_OF_FIRE,
            name: "Superior Sigil of Fire".into(),
        }),
        None,
    ]);
    build
}

/// `build()` on a precision-free prefix, so the critical chance is the
/// base 5 % only and an on-crit proc has almost nothing to fire from
/// (US1 negative control; the test pins precision to zero on top).
pub fn build_with_zero_precision() -> ValidatedBuild {
    let mut build = build();
    build.fill_worn_gear_slots(PrefixRef {
        itemstat_id: SOLDIER_ITEMSTAT,
        name: "Soldier".into(),
    });
    build
}

/// Scholar-shaped and Thief-shaped records on the fixture's synthetic rune
/// and relic ids (US3): +5 % strike while above 90 % health, and +1 % strike
/// per stack, five stacks, 6 s, from weapon skills with a recharge.
pub fn records_with_threshold_and_stack() -> Vec<NormalizedEffect> {
    let mut scholar = record(
        SourceType::Rune,
        RUNE_OF_THE_SCHOLAR,
        "Superior Rune of the Scholar",
        EffectCategory::TriggeredEffect,
        5.0,
        TriggerRule::OnHealthThreshold,
    );
    scholar.stacking_rule = StackingRule::Multiplicative;
    scholar.inner_category = Some(EffectCategory::StrikeDamagePct);
    scholar.health_threshold = Some(HealthThreshold {
        above: true,
        percent: FactualValue::Resolved(90.0),
    });

    let mut thief = record(
        SourceType::Relic,
        RELIC_OF_THE_THIEF,
        "Relic of the Thief",
        EffectCategory::TriggeredEffect,
        1.0,
        TriggerRule::OnHit,
    );
    thief.stacking_rule = StackingRule::Multiplicative;
    thief.inner_category = Some(EffectCategory::StrikeDamagePct);
    thief.max_stacks = Some(FactualValue::Resolved(5));
    thief.effect_duration = Some(FactualValue::Resolved(6.0));
    thief.trigger_scope = Some(TriggerScope::WeaponSkillWithRecharge);
    vec![scholar, thief]
}

/// Set-1 hit, then a set-2 weapon skill so the timeline swaps (US2).
pub fn opener_with_swap() -> Vec<u32> {
    vec![GRAVEDIGGER, GHASTLY_CLAWS]
}

/// Sprint 3 (specs/007-trait-triggers, T050): a chill before the crits, a
/// shout, a corrupt (boon strip) and the full shroud enter, so the
/// Necromancer catalogue's on-crit-while-chilled, shout-scoped,
/// boon-stripped and shroud-entry records all have a firing site in one
/// press order.
pub fn opener_catalogue() -> Vec<u32> {
    vec![
        GRASPING_DARKNESS,
        GRAVEDIGGER,
        YOU_ARE_ALL_WEAKLINGS,
        WELL_OF_DARKNESS,
        DEATH_SPIRAL,
        WELL_OF_SUFFERING,
        REAPER_SHROUD,
        SHROUD_1,
        SHROUD_4,
    ]
}

/// Sprint 3: enter shroud and leave it by the exit skill, for the exit
/// records (the bar itself does not carry the flip skill; tests append it).
pub fn opener_shroud_exit() -> Vec<u32> {
    vec![
        GRAVEDIGGER,
        DEATH_SPIRAL,
        WELL_OF_SUFFERING,
        REAPER_SHROUD,
        SHROUD_1,
        EXIT_SHROUD,
    ]
}

/// Press order for the pinned experiments. Since Sprint 2 the generators
/// come first: life force starts at zero and shroud needs 10 % of the pool
/// (Gravedigger 8 % + Death Spiral 6 % + Well 5 %), so the shroud entry
/// moved behind them. Sprint 1's audit anchors were re-recorded (section 8).
pub fn opener() -> Vec<u32> {
    vec![
        GRAVEDIGGER,
        DEATH_SPIRAL,
        WELL_OF_SUFFERING,
        REAPER_SHROUD,
        SHROUD_4,
        SHROUD_1,
    ]
}

fn unknown_uptime() -> UptimeModel {
    UptimeModel {
        kind: UptimeModelKind::Unknown,
        uptime: None,
    }
}

/// A bare record for test-local fixtures (Sprint 3 experiments build on it).
pub fn record(
    source_type: SourceType,
    source_id: u32,
    source_name: &str,
    category: EffectCategory,
    value: f64,
    trigger_rule: TriggerRule,
) -> NormalizedEffect {
    NormalizedEffect {
        effect_id: format!("fixture:{source_name}"),
        source_type,
        source_id,
        source_name: source_name.into(),
        category,
        value: FactualValue::Resolved(value),
        stacking_rule: StackingRule::NonStacking,
        trigger_rule,
        uptime_model: unknown_uptime(),
        evidence_level: EvidenceLevel::Unknown,
        source: None,
        effect_duration: None,
        internal_cooldown: None,
        max_stacks: None,
        status_operation: None,
        inner_category: None,
        health_threshold: None,
        proc_chance: None,
        trigger_scope: None,
        prerequisite: None,
        scale_by: None,
        healing_power_coefficient: None,
        derived_from: Vec::new(),
        coverage: None,
        cast_skill_id: None,
        gates: Vec::new(),
        scale: None,
        actor: crate::data::normalized_effects::Actor::Player,
    }
}

/// Path of Corruption: on hit, corrupt two boons, 10 s internal cooldown.
pub fn path_of_corruption() -> NormalizedEffect {
    let mut effect = record(
        SourceType::Trait,
        PATH_OF_CORRUPTION,
        "Path of Corruption",
        EffectCategory::CorruptsBoon,
        2.0,
        TriggerRule::OnHit,
    );
    effect.internal_cooldown = Some(FactualValue::Resolved(10.0));
    effect.status_operation = Some(StatusOperation {
        operation_type: OperationType::CorruptsBoon,
        target_side: TargetSide::Enemy,
        status_kind: "Any".into(),
        amount_mode: AmountMode::Count,
        amount_value: FactualValue::Resolved(2.0),
        base_duration_ms: None,
        target_scope: TargetScope::SingleTarget,
        target_count: None,
        internal_cooldown_ms: Some(FactualValue::Resolved(10_000)),
        source_duration_multiplier: None,
    });
    effect
}

/// Superior Sigil of Fire: on crit, a strike worth 50 % of a weapon hit,
/// 5 s internal cooldown. The runtime has no on-crit firing site
/// (CONN-00-06); this record exists to prove that.
pub fn sigil_of_fire() -> NormalizedEffect {
    let mut effect = record(
        SourceType::Sigil,
        SIGIL_OF_FIRE,
        "Superior Sigil of Fire",
        EffectCategory::ProcEffect,
        50.0,
        TriggerRule::OnCrit,
    );
    effect.internal_cooldown = Some(FactualValue::Resolved(5.0));
    effect.inner_category = Some(EffectCategory::StrikeDamagePct);
    effect
}

/// Superior Sigil of Force: passive +5 % strike damage. Passives are folded
/// into `SimParams` by the stat parser and skipped by the timeline.
pub fn sigil_of_force() -> NormalizedEffect {
    let mut effect = record(
        SourceType::Sigil,
        SIGIL_OF_FORCE,
        "Superior Sigil of Force",
        EffectCategory::StrikeDamagePct,
        5.0,
        TriggerRule::Passive,
    );
    effect.stacking_rule = StackingRule::Additive;
    effect.uptime_model = UptimeModel {
        kind: UptimeModelKind::AlwaysOn,
        uptime: Some(FactualValue::Resolved(1.0)),
    };
    effect
}

/// All three fixture records.
pub fn records() -> Vec<NormalizedEffect> {
    vec![path_of_corruption(), sigil_of_fire(), sigil_of_force()]
}

/// WvW Solo / StrikeSpike on the active patch.
pub fn scenario() -> (BalanceContext, ScenarioSpec) {
    let ctx = BalanceContext::new(GameMode::WvW);
    let scenario = ScenarioSpec::from_balance_context(&ctx);
    (ctx, scenario)
}

/// PvE Solo / StrikeSpike, the small comparison the spec asks for.
pub fn pve_scenario() -> (BalanceContext, ScenarioSpec) {
    let ctx = BalanceContext::new(GameMode::PvE);
    let scenario = ScenarioSpec::from_balance_context(&ctx);
    (ctx, scenario)
}
