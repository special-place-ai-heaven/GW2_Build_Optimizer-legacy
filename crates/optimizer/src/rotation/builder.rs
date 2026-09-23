//! Builds a rotation skill set from a build's skill IDs and GameDb data.
//! Extracts timing, cooldown, and effect information from API skill facts.

use gw2_api::models::facts::Fact;
use gw2_api::models::Skill;
use std::collections::HashMap;

use crate::balance::BalanceContext;
use crate::data::balance_overrides::{overrides, OverrideResult};
use crate::data::normalized_effects::{EffectCategory, NormalizedEffect};
use crate::gamedb::GameDb;
use crate::text_util::{
    text_describes_block, text_describes_condition_cleanse, text_describes_stability,
};

use super::skill_timings::timing_for;
use super::{ControlKind, CoverKind, MobilityKind, RotationSkill, SkillEffect, SkillSlot};

/// Build a list of RotationSkills from skill IDs, looking up data from GameDb.
pub fn build_rotation_skills(skill_ids: &[u32], db: &GameDb) -> Vec<RotationSkill> {
    build_rotation_skills_for_context(skill_ids, db, &BalanceContext::pve())
}

/// Build mode-aware rotation skills. Sourced overrides take precedence over
/// the public API because the API does not label competitive mode splits.
pub fn build_rotation_skills_for_context(
    skill_ids: &[u32],
    db: &GameDb,
    ctx: &BalanceContext,
) -> Vec<RotationSkill> {
    skill_ids
        .iter()
        .filter_map(|&id| {
            let skill = db.skills.get(&id)?;
            Some(skill_to_rotation_for_context(skill, ctx))
        })
        .collect()
}

/// Enrich rotation skills with `RemovesCondition` effects: the cleanse registry
/// (`data/cleanse_sources.json`) first, then NormalizedEffects data, then the
/// description text as a fallback heuristic.
///
/// Registry path: `cleanse_sources::registry().skill(id)` decides for every id
/// it knows; `gate_count_with(equipped_traits)` is the count (0 for ally-only
/// sources and for traited cleanses whose trait the build does not run).
///
/// NormalizedEffects path: scan `ne_effects` for entries with `category == RemovesCondition`
/// whose `source_id` matches a skill's `skill_id`. When found, add a
/// `RemovesCondition` effect carrying the `conditions_removed` count from
/// `status_operation.amount_value` (floored to u32, minimum 1).
///
/// Fallback path: if no NormalizedEffects entry is found for a skill, check
/// the skill's API description (from `db.skills`) for condition-cleanse language:
/// ("remov" AND "condit") OR ("cure" AND "condit"). If matched, add
/// `RemovesCondition { conditions_removed: 1 }` as a heuristic estimate.
///
/// Idempotent per-call: skips any skill that already carries a `RemovesCondition` effect.
pub fn enrich_with_cleanse(
    skills: &mut [RotationSkill],
    ne_effects: &[NormalizedEffect],
    db: &GameDb,
    equipped_traits: &[u32],
) {
    use crate::data::quality::FactualValue;

    // Build a fast lookup: source_id → max conditions_removed from NormalizedEffects.
    // A skill may appear as multiple RemovesCondition entries; take the largest amount.
    let mut ne_cleanse: HashMap<u32, u32> = HashMap::new();
    for effect in ne_effects {
        if effect.category == EffectCategory::RemovesCondition {
            let count = match &effect.status_operation {
                Some(op) => match op.amount_value {
                    FactualValue::Resolved(v) => (v.floor() as u32).max(1),
                    FactualValue::Unknown => 1,
                },
                None => 1,
            };
            let entry = ne_cleanse.entry(effect.source_id).or_insert(0);
            *entry = (*entry).max(count);
        }
    }

    for skill in skills.iter_mut() {
        // Skip if already carries a RemovesCondition effect (idempotency guard).
        if skill
            .effects
            .iter()
            .any(|e| matches!(e, SkillEffect::RemovesCondition { .. }))
        {
            continue;
        }

        // The registry (data/cleanse_sources.json) is authoritative for every
        // id it knows, including "known, and it only cleanses allies".
        let reg = crate::data::cleanse_sources::registry();
        if let Some(src) = reg.skill(skill.skill_id) {
            let count = src.gate_count_with(equipped_traits);
            if count > 0 {
                skill.effects.push(SkillEffect::RemovesCondition {
                    conditions_removed: count,
                });
            }
            continue;
        }
        if reg.knows_skill(skill.skill_id) {
            continue; // read by a cataloguer and judged not to cleanse
        }

        if let Some(&count) = ne_cleanse.get(&skill.skill_id) {
            // NormalizedEffects data matched by source_id.
            skill.effects.push(SkillEffect::RemovesCondition {
                conditions_removed: count,
            });
        } else {
            // Fallback: description text heuristic.
            let description = db
                .skills
                .get(&skill.skill_id)
                .and_then(|s| s.description.as_deref())
                .unwrap_or("")
                .to_lowercase();

            if text_describes_condition_cleanse(&description) {
                skill.effects.push(SkillEffect::RemovesCondition {
                    conditions_removed: 1, // HEURISTIC: assume 1 condition removed
                });
            }
        }
    }
}

/// Convert a GW2 API Skill into a RotationSkill with extracted timing and effects.
#[cfg(test)]
pub(crate) fn skill_to_rotation(skill: &Skill) -> RotationSkill {
    skill_to_rotation_for_context(skill, &BalanceContext::pve())
}

#[cfg(test)]
mod stunbreak_tests {
    use super::skill_breaks_stun;
    use gw2_api::models::Skill;

    fn skill(description: Option<&str>) -> Skill {
        skill_with_id(1, description)
    }

    fn skill_with_id(id: u32, description: Option<&str>) -> Skill {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": "test",
            "description": description,
            "facts": [],
        }))
        .expect("test skill")
    }

    /// "Never Surrender!", Banner of Tactics and Mantra of Concentration all
    /// carry the words and not the `StunBreak` fact.
    #[test]
    fn description_alone_counts_as_a_stunbreak() {
        for text in [
            "Shout. Break stun and grant resolution to allies.",
            "Mantra. Breaks stun for you and nearby allies.",
            "Banner. Stun break.",
            "Glyph. Stunbreak and daze foes.",
        ] {
            assert!(skill_breaks_stun(&skill(Some(text))), "missed: {text}");
        }
    }

    #[test]
    fn a_skill_with_neither_fact_nor_words_does_not_count() {
        assert!(!skill_breaks_stun(&skill(Some(
            "Cantrip. Teleport to target area."
        ))));
        assert!(!skill_breaks_stun(&skill(None)));
    }

    /// Gladiator's Defense (77291) carries an untyped fact and description
    /// wording ("Break out of stun") the fixed text needles miss; it counts
    /// only via `data::stunbreak_sources`. Lightning Flash (5536), by
    /// contrast, is not a stun break at all per the wiki ("You can still use
    /// this skill to teleport when stunned, but it will not break stun") and
    /// must not be in the override table.
    #[test]
    fn the_override_table_catches_what_fact_and_text_miss() {
        assert!(skill_breaks_stun(&skill_with_id(
            77291,
            Some(
                "Break out of stun, damaging and weakening enemies close to you. \
                 Gain boons if this skill strikes at least one enemy."
            )
        )));
        assert!(!skill_breaks_stun(&skill_with_id(
            5536,
            Some("Cantrip. Teleport to target area.")
        )));
    }
}

/// Whether this skill reaches ALLIES rather than only its owner.
///
/// The API says so itself: 147 skills publish a `Number of Allied
/// Targets`, `Allied Healing`, `Allied Heal per Pulse`, `Maximum Number
/// of Allied Targets` or `Allied Target Radius` fact. Anything else heals
/// or buffs the caster, which is survival (the `sustain` axis), not
/// support.
///
/// Categories are deliberately not consulted: not every shout helps
/// allies ("Nothing Can Save You!" is aimed at foes), so reading the
/// category would credit a foe skill as boon support.
pub(crate) fn skill_reaches_allies(skill: &Skill) -> bool {
    skill.facts.iter().any(|fact| {
        let text = match fact {
            Fact::Number { text, .. }
            | Fact::Time { text, .. }
            | Fact::AttributeAdjust { text, .. }
            | Fact::Radius { text, .. } => text.as_deref(),
            _ => None,
        };
        text.is_some_and(|text| text.to_lowercase().contains("allied"))
    })
}

/// A skill breaks stun when the API says so in a `StunBreak` fact, in its
/// own description text, or in `data::stunbreak_sources`. Twelve skills
/// ("Never Surrender!", Banner of Tactics, Mantra of Concentration, Glyph of
/// Equality among them) carry the words and not the fact, and reading them
/// as unarmed by the StunbreakCount gate refused builds that break stun in
/// game. A further handful (Gladiator's Defense, Toss Elixir U) carry
/// neither: an untyped fact or description wording the fixed text needles
/// miss. The table is catalogued against the wiki, not inferred.
pub(crate) fn skill_breaks_stun(skill: &Skill) -> bool {
    if skill.facts.iter().any(|f| {
        matches!(
            f,
            Fact::StunBreak {
                value: Some(true),
                ..
            }
        )
    }) {
        return true;
    }
    let description = skill.description.as_deref().unwrap_or("").to_lowercase();
    if ["break stun", "breaks stun", "stun break", "stunbreak"]
        .iter()
        .any(|needle| description.contains(needle))
    {
        return true;
    }
    crate::data::stunbreak_sources::is_override(skill.id)
}

fn skill_to_rotation_for_context(skill: &Skill, ctx: &BalanceContext) -> RotationSkill {
    let slot = skill
        .slot
        .as_deref()
        .and_then(SkillSlot::from_api)
        .unwrap_or(SkillSlot::Utility);

    let timing = timing_for(skill.id, slot);
    let cast_time_ms =
        sourced_skill_u32(ctx, skill.id, "activation_ms").unwrap_or_else(|| timing.total_ms());
    let cooldown_ms = sourced_skill_u32(ctx, skill.id, "recharge_ms")
        .unwrap_or_else(|| extract_cooldown(&skill.facts));
    let effects =
        extract_effects_for_context(skill.id, &skill.facts, skill.description.as_deref(), ctx);
    let is_stunbreak = skill_breaks_stun(skill);

    RotationSkill {
        skill_id: skill.id,
        name: skill.name.clone(),
        slot,
        cast_time_ms,
        cooldown_ms,
        effects,
        next_chain: skill.next_chain,
        is_stunbreak,
        reaches_allies: skill_reaches_allies(skill),
        weapon_set: 0, // default; caller can tag with set 1/2 via tag_weapon_set()
        categories: skill.categories.clone(),
        slot_name: skill.slot.clone(),
        targets: skill
            .facts
            .iter()
            .find_map(|fact| match fact {
                Fact::Number {
                    text: Some(text),
                    value: Some(value),
                    ..
                } if text.eq_ignore_ascii_case("Number of Targets") => Some((*value).max(1) as u32),
                _ => None,
            })
            .unwrap_or(1),
    }
}

pub(crate) fn sourced_skill_value(ctx: &BalanceContext, skill_id: u32, field: &str) -> Option<f64> {
    match overrides().lookup(
        &ctx.patch_id,
        ctx.game_mode.label(),
        "Skill",
        skill_id,
        field,
    ) {
        Some(OverrideResult::Value { value, .. }) => Some(value),
        Some(OverrideResult::Unknown { .. }) | None => None,
    }
}

pub(crate) fn sourced_skill_u32(ctx: &BalanceContext, skill_id: u32, field: &str) -> Option<u32> {
    sourced_skill_value(ctx, skill_id, field)
        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= u32::MAX as f64)
        .map(|value| value.round() as u32)
}

/// Sourced coefficients for a skill whose damage changes with target health.
///
/// Rotation construction has no target-health input, so it cannot choose a
/// threshold dynamically. `above_50` is the exact initial-target coefficient
/// emitted into the rotation. Lower-health tiers remain recorded here and make
/// that selection explicitly provisional rather than being averaged or summed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SourcedDamageCoefficientProfile {
    above_50: f64,
    below_50: Option<f64>,
    below_25: Option<f64>,
    threshold_selection_is_provisional: bool,
}

impl SourcedDamageCoefficientProfile {
    fn initial_target_coefficient(self) -> f64 {
        debug_assert_eq!(
            self.threshold_selection_is_provisional,
            self.below_50.is_some() || self.below_25.is_some()
        );
        self.above_50
    }
}

fn sourced_damage_coefficient_profile(
    ctx: &BalanceContext,
    skill_id: u32,
) -> Option<SourcedDamageCoefficientProfile> {
    let valid_coefficient = |field| {
        sourced_skill_value(ctx, skill_id, field).filter(|value| value.is_finite() && *value >= 0.0)
    };
    let above_50 = valid_coefficient("damage_coefficient:above_50")?;
    let below_50 = valid_coefficient("damage_coefficient:below_50");
    let below_25 = valid_coefficient("damage_coefficient:below_25");

    Some(SourcedDamageCoefficientProfile {
        above_50,
        below_50,
        below_25,
        threshold_selection_is_provisional: below_50.is_some() || below_25.is_some(),
    })
}

/// Tag weapon skills in a rotation with their weapon set number.
/// Non-weapon skills (heal/utility/elite/profession) are left at set 0.
pub fn tag_weapon_set(skills: &mut [RotationSkill], weapon_set: u8) {
    for skill in skills.iter_mut() {
        if skill.slot.is_weapon() {
            skill.weapon_set = weapon_set;
        }
    }
}

/// Join the two tagged weapon sets. A skill carried by both sets (an off-hand
/// dagger on each set) shares one cooldown in game, so it becomes ONE skill
/// usable on either set (`weapon_set` 0) instead of two independent copies.
/// Measured 2026-09-05: a Necromancer with Dagger/Dagger + Scepter/Dagger
/// simulated three Deathly Swarms, tripling its damage and cleanse credit.
pub fn merge_weapon_sets(
    mut set1: Vec<RotationSkill>,
    set2: Vec<RotationSkill>,
) -> Vec<RotationSkill> {
    let set1_ids: Vec<u32> = set1.iter().map(|s| s.skill_id).collect();
    let (shared, own): (Vec<_>, Vec<_>) = set2
        .into_iter()
        .partition(|s2| set1_ids.contains(&s2.skill_id));
    for s1 in set1.iter_mut() {
        if shared.iter().any(|s2| s2.skill_id == s1.skill_id) {
            s1.weapon_set = 0;
        }
    }
    set1.extend(own);
    set1
}

/// Resolve the F1-F5 mechanic bar for the equipped specialization set and
/// the weapons in hand. Elite-spec replacements win over their core skill in
/// the same profession slot; deterministic ID ordering breaks ties in
/// incomplete API data.
///
/// Weapon-dependent slots (every Warrior burst, every Berserker primal burst,
/// every Bladesworn dragon trigger) are filtered by `skill.weapon_type`
/// against the equipped sets: without it the id sort handed a Spear Warrior
/// Eviscerate, an Axe skill it cannot press. Slots whose candidates are
/// weapon-independent (Spellbreaker's Full Counter, Thief steal, shatters)
/// carry `weapon_type: "None"` and are unaffected, as is a build with no
/// weapons resolved yet.
pub fn profession_skills_for_build(
    db: &GameDb,
    profession_name: &str,
    equipped_spec_ids: &[u32],
    weapons: &crate::validation::ValidatedWeapons,
) -> Vec<(u32, String)> {
    let equipped: Vec<&str> = [&weapons.set1, &weapons.set2]
        .into_iter()
        .flat_map(|set| [set.main_hand.as_deref(), set.off_hand.as_deref()])
        .flatten()
        .collect();
    let mut by_slot: HashMap<String, Vec<&gw2_api::models::Skill>> = HashMap::new();
    for skill_id in db
        .skills_by_profession
        .get(profession_name)
        .into_iter()
        .flatten()
    {
        let Some(skill) = db.skills.get(skill_id) else {
            continue;
        };
        let Some(slot) = skill.slot.as_deref() else {
            continue;
        };
        if !slot.starts_with("Profession_")
            || skill
                .specialization
                .is_some_and(|required| !equipped_spec_ids.contains(&required))
        {
            continue;
        }
        by_slot.entry(slot.to_string()).or_default().push(skill);
    }

    let mut slots: Vec<_> = by_slot.into_iter().collect();
    slots.sort_by(|a, b| a.0.cmp(&b.0));
    slots
        .into_iter()
        .filter_map(|(_, mut skills)| {
            // A form entry's flip is the form's exit, not the press
            // (Release Celestial Avatar 31411 sorted ahead of Celestial
            // Avatar 31869 by id). Other flips stay candidates: Legendary
            // Renegade Stance flips to a second id of itself.
            let exits: Vec<u32> = skills
                .iter()
                .filter(|skill| !skill.transform_skills.is_empty())
                .filter_map(|skill| skill.flip_skill)
                .collect();
            skills.retain(|skill| !exits.contains(&skill.id));
            skills.sort_by_key(|skill| (u8::from(skill.specialization.is_none()), skill.id));
            // First candidate whose weapon is in hand, else the first
            // candidate: an unresolved weapon set must not empty the bar.
            let picked = skills
                .iter()
                .position(|skill| {
                    skill.weapon_type.as_deref().is_some_and(|weapon| {
                        equipped
                            .iter()
                            .any(|held| held.eq_ignore_ascii_case(weapon))
                    })
                })
                .unwrap_or(0);
            skills
                .get(picked)
                .map(|skill| (skill.id, skill.name.clone()))
        })
        .collect()
}

/// The form bar for the equipped specialisations: the `transform_skills`
/// of every profession-slot skill the build owns (`specs/005-wvw-proc-sites`,
/// R6). The API lists every shroud's skills in the core entry skill's
/// `transform_skills` (Death Shroud 10574 carries all 57) with misleading
/// slots — `Downed_1..4` and `Weapon_5` — and tags each with its
/// specialisation; Druid's Celestial Avatar lists its own. An equipped
/// elite's skills replace the core ones; a specialisation without an entry
/// skill (Scourge) plays no form, which the entry lookup decides. A bar slot
/// with a `NoUnderwater` skill keeps only its land skills: the others are
/// the aquatic palette.
pub fn form_bar_for_build(
    db: &GameDb,
    profession_name: &str,
    equipped_spec_ids: &[u32],
) -> Vec<(u32, String)> {
    let mut candidates: Vec<&gw2_api::models::Skill> = Vec::new();
    for skill_id in db
        .skills_by_profession
        .get(profession_name)
        .into_iter()
        .flatten()
    {
        let Some(entry) = db.skills.get(skill_id) else {
            continue;
        };
        let owned_entry = entry
            .specialization
            .is_none_or(|spec| equipped_spec_ids.contains(&spec));
        if !entry
            .slot
            .as_deref()
            .is_some_and(|slot| slot.starts_with("Profession_"))
            || !owned_entry
        {
            continue;
        }
        for transform_id in &entry.transform_skills {
            let Some(skill) = db.skills.get(transform_id) else {
                continue;
            };
            let bar_slot = skill
                .slot
                .as_deref()
                .is_some_and(|slot| !slot.starts_with("Profession_"));
            let owned = skill
                .specialization
                .is_none_or(|spec| equipped_spec_ids.contains(&spec));
            if bar_slot && owned {
                candidates.push(skill);
            }
        }
    }
    let elite_owned = candidates
        .iter()
        .any(|skill| skill.specialization.is_some());
    let mut bar: Vec<&gw2_api::models::Skill> = candidates
        .into_iter()
        .filter(|skill| skill.specialization.is_some() == elite_owned)
        .collect();
    let land_only = |skill: &gw2_api::models::Skill| {
        skill
            .flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("NoUnderwater"))
    };
    // A bar slot holding a land-only skill is the land slot; its other
    // skills are the underwater palette, renamed or not (Voracious Dive
    // beside Voracious Arc, Plague Blast beside Life Blast).
    let land_slots: std::collections::HashSet<Option<SkillSlot>> = bar
        .iter()
        .filter(|skill| land_only(skill))
        .map(|skill| form_bar_slot(skill.slot.as_deref()))
        .collect();
    bar.retain(|skill| {
        land_only(skill) || !land_slots.contains(&form_bar_slot(skill.slot.as_deref()))
    });
    // A flip that is not the auto chain's next step (Terrify after Infusing
    // Terror) is pressed only after its parent, not free from the bar.
    let flips_off_chain: Vec<u32> = bar
        .iter()
        .filter_map(|skill| {
            skill
                .flip_skill
                .filter(|flip| skill.next_chain != Some(*flip))
        })
        .collect();
    bar.retain(|skill| !flips_off_chain.contains(&skill.id));
    let position = |slot: Option<&str>| form_bar_slot(slot).map_or(9, |slot| slot as u8 + 1);
    bar.sort_by_key(|skill| (position(skill.slot.as_deref()), skill.id));
    bar.dedup_by_key(|skill| skill.id);
    bar.into_iter()
        .map(|skill| (skill.id, skill.name.clone()))
        .collect()
}

/// A form bar skill's place on the bar. The API files the shroud bar under
/// `Downed_1..4` and `Weapon_5`; on the bar they are slots 1-5.
pub fn form_bar_slot(api_slot: Option<&str>) -> Option<SkillSlot> {
    let slot = api_slot?;
    let n = slot
        .strip_prefix("Downed_")
        .or_else(|| slot.strip_prefix("Weapon_"))?;
    SkillSlot::from_api(&format!("Weapon_{n}"))
}

/// Extract cooldown from Fact::Recharge (seconds → milliseconds).
/// How long the rotation must wait before pressing this skill again.
///
/// For an ordinary skill that is the `Recharge` fact. For an AMMUNITION
/// skill (ammo utilities, mantras) `Recharge` is only the delay between two
/// casts of a charge already banked -- Combat Stimulant 62978 publishes 1 s
/// -- while the skill really returns one charge every `Count Recharge`
/// seconds. Reading the first fact made a 20 s heal look like a 1 s one and
/// the timeline pressed it twenty times a fight. The sustainable rate is
/// `count recharge / maximum count`, and the inter-cast delay is the floor.
fn extract_cooldown(facts: &[Fact]) -> u32 {
    let recharge_ms = facts
        .iter()
        .find_map(|fact| match fact {
            Fact::Recharge { value: Some(v), .. } => Some((*v * 1_000.0) as u32),
            _ => None,
        })
        .unwrap_or(0); // no cooldown = auto-attack or instant
    let max_count = facts.iter().find_map(|fact| match fact {
        Fact::Number {
            text: Some(text),
            value: Some(value),
            ..
        } if text.eq_ignore_ascii_case("Maximum Count") && *value > 0 => Some(*value as u32),
        _ => None,
    });
    // The first Count Recharge is the live one; later duplicates are the
    // competitive-mode splits.
    let count_recharge_ms = facts.iter().find_map(|fact| match fact {
        Fact::Time {
            text: Some(text),
            duration: Some(duration),
            ..
        } if text.eq_ignore_ascii_case("Count Recharge") => Some(*duration * 1_000),
        _ => None,
    });
    match (max_count, count_recharge_ms) {
        (Some(count), Some(count_recharge)) => recharge_ms.max(count_recharge / count),
        _ => recharge_ms,
    }
}

/// Extract all combat-relevant effects from skill facts (+ description for corrupt/mobility).
#[cfg(test)]
fn extract_effects(facts: &[Fact], description: Option<&str>) -> Vec<SkillEffect> {
    extract_effects_for_context(0, facts, description, &BalanceContext::pve())
}

fn extract_effects_for_context(
    skill_id: u32,
    facts: &[Fact],
    description: Option<&str>,
    ctx: &BalanceContext,
) -> Vec<SkillEffect> {
    let mut effects = Vec::new();
    let (interval_ms, window_ms) = pulse_window_ms(facts);
    let sourced_damage = sourced_damage_coefficient_profile(ctx, skill_id);

    if let Some(profile) = sourced_damage {
        // Threshold rows are mutually exclusive outcomes of one hit. The
        // current rotation representation cannot switch coefficients as target
        // health changes, so emit the exact above-50 initial-target value once.
        // Never add the API's threshold rows as simultaneous strikes.
        let hit_count = facts
            .iter()
            .find_map(|fact| match fact {
                Fact::Damage { hit_count, .. } => *hit_count,
                _ => None,
            })
            .unwrap_or(1);
        effects.push(SkillEffect::StrikeDamage {
            hit_count,
            dmg_multiplier: profile.initial_target_coefficient(),
        });
    }

    for fact in facts {
        match fact {
            Fact::Damage {
                hit_count,
                dmg_multiplier,
                ..
            } if sourced_damage.is_none() => {
                effects.push(SkillEffect::StrikeDamage {
                    hit_count: hit_count.unwrap_or(1),
                    dmg_multiplier: dmg_multiplier.unwrap_or(1.0),
                });
            }
            Fact::Damage { .. } => {}
            Fact::Buff {
                status: Some(status),
                duration,
                apply_count,
                ..
            } => {
                let field = format!("status_duration_ms:{}", status.to_lowercase());
                let duration_ms = sourced_skill_u32(ctx, skill_id, &field)
                    .unwrap_or_else(|| duration.unwrap_or(0).saturating_mul(1000));
                push_status_effect(&mut effects, status, apply_count.unwrap_or(1), duration_ms);
            }
            Fact::PrefixedBuff {
                status: Some(status),
                duration,
                apply_count,
                ..
            } => {
                let field = format!("status_duration_ms:{}", status.to_lowercase());
                let duration_ms = sourced_skill_u32(ctx, skill_id, &field)
                    .unwrap_or_else(|| duration.unwrap_or(0).saturating_mul(1000));
                push_status_effect(&mut effects, status, apply_count.unwrap_or(1), duration_ms);
            }
            Fact::ComboField {
                field_type: Some(ft),
                ..
            } => {
                effects.push(SkillEffect::ComboField {
                    field_type: ft.clone(),
                    duration_ms: sourced_skill_u32(ctx, skill_id, "combo_field_duration_ms")
                        .unwrap_or(window_ms),
                });
            }
            Fact::ComboFinisher {
                finisher_type: Some(finisher_type),
                percent,
                ..
            } => {
                effects.push(SkillEffect::ComboFinisher {
                    finisher_type: finisher_type.clone(),
                    percent: percent.unwrap_or(100),
                });
            }
            Fact::Heal { hit_count, .. } | Fact::HealingAdjust { hit_count, .. } => {
                effects.push(SkillEffect::Healing {
                    hit_count: hit_count.unwrap_or(1),
                });
            }
            Fact::Number {
                text: Some(text),
                value,
                ..
            } => {
                let count = value.unwrap_or(1).max(1) as u32;
                if text_describes_boon_strip(text) {
                    effects.push(SkillEffect::StripBoons {
                        count_per_pulse: count,
                        interval_ms,
                        window_ms,
                    });
                } else if crate::data::cleanse_sources::registry().knows_skill(skill_id) {
                    // The registry decides this skill's cleanse in
                    // `enrich_with_cleanse`; a fact-derived effect here would
                    // pre-empt it through that function's idempotency guard.
                } else if let Some(conditions_removed) =
                    condition_cleanse_count_from_text(text, *value)
                {
                    let pulses = window_ms
                        .max(interval_ms)
                        .checked_div(interval_ms)
                        .unwrap_or(1);
                    effects.push(SkillEffect::RemovesCondition {
                        conditions_removed: conditions_removed * pulses,
                    });
                }
            }
            Fact::Distance {
                distance: Some(d), ..
            } if *d >= 200
                && !effects
                    .iter()
                    .any(|e| matches!(e, SkillEffect::Mobility { .. })) =>
            {
                // Displacement this large is usually a leap/teleport, not a pull tick.
                effects.push(SkillEffect::Mobility {
                    kind: MobilityKind::Leap,
                });
            }
            _ => {}
        }
    }

    // A control the API omits but the wiki publishes (Voracious Arc's daze):
    // its `status_duration_ms:<status>` override supplies it when no fact did.
    for status in CONTROL_STATUSES {
        let Some((kind, _)) = control_kind(status) else {
            continue;
        };
        let present = effects
            .iter()
            .any(|e| matches!(e, SkillEffect::CrowdControl { kind: k, .. } if *k == kind));
        let field = format!("status_duration_ms:{}", status.to_lowercase());
        if let (false, Some(duration_ms)) = (present, sourced_skill_u32(ctx, skill_id, &field)) {
            push_status_effect(&mut effects, status, 1, duration_ms);
        }
    }

    if let Some(desc) = description {
        push_description_effects(&mut effects, desc);
    }

    effects
}

/// Every status [`control_kind`] reads, by its API name.
const CONTROL_STATUSES: [&str; 11] = [
    "Stun",
    "Knockdown",
    "Launch",
    "Knockback",
    "Pull",
    "Fear",
    "Taunt",
    "Daze",
    "Float",
    "Sink",
    "Immobilize",
];

fn pulse_window_ms(facts: &[Fact]) -> (u32, u32) {
    let mut interval_ms = 0u32;
    let mut window_ms = 0u32;
    for fact in facts {
        match fact {
            Fact::Time {
                text: Some(text),
                duration: Some(d),
                ..
            } => {
                let ms = d.saturating_mul(1000);
                let t = text.to_lowercase();
                if t.contains("interval") || t.contains("pulse") {
                    interval_ms = ms;
                } else if t.contains("duration") {
                    window_ms = ms;
                }
            }
            Fact::Duration {
                duration: Some(d), ..
            } => {
                window_ms = d.saturating_mul(1000);
            }
            _ => {}
        }
    }
    (interval_ms, window_ms)
}

fn push_status_effect(effects: &mut Vec<SkillEffect>, status: &str, stacks: u32, duration_ms: u32) {
    if let Some((kind, stops_dodge)) = control_kind(status) {
        effects.push(SkillEffect::CrowdControl {
            kind,
            duration_ms,
            stops_dodge,
        });
        return;
    }
    if let Some((kind, strippable)) = cover_kind(status) {
        effects.push(SkillEffect::Cover {
            kind,
            duration_ms,
            strippable,
        });
        if kind == CoverKind::Stability {
            effects.push(SkillEffect::ApplyBuff {
                buff: "Stability".into(),
                stacks,
                duration_ms,
            });
        }
        return;
    }
    if status.eq_ignore_ascii_case("Superspeed") {
        effects.push(SkillEffect::Mobility {
            kind: MobilityKind::Superspeed,
        });
        return;
    }
    if status.eq_ignore_ascii_case("Stealth") {
        effects.push(SkillEffect::Cover {
            kind: CoverKind::Stealth,
            duration_ms,
            strippable: false,
        });
        effects.push(SkillEffect::Mobility {
            kind: MobilityKind::Stealth,
        });
        return;
    }
    if is_damaging_condition(status) {
        effects.push(SkillEffect::ApplyCondition {
            condition: status.to_string(),
            stacks,
            duration_ms,
        });
    } else {
        effects.push(SkillEffect::ApplyBuff {
            buff: status.to_string(),
            stacks,
            duration_ms,
        });
    }
}

fn control_kind(status: &str) -> Option<(ControlKind, bool)> {
    Some(match status {
        "Stun" => (ControlKind::Stun, true),
        "Knockdown" => (ControlKind::Knockdown, true),
        "Launch" => (ControlKind::Launch, true),
        "Knockback" => (ControlKind::Knockback, true),
        "Pull" => (ControlKind::Pull, true),
        "Fear" => (ControlKind::Fear, true),
        "Taunt" => (ControlKind::Taunt, true),
        "Daze" => (ControlKind::Daze, false),
        "Float" => (ControlKind::Float, true),
        "Sink" => (ControlKind::Sink, true),
        "Immobile" | "Immobilize" | "Immobilized" => (ControlKind::Immobilize, true),
        _ => return None,
    })
}

fn cover_kind(status: &str) -> Option<(CoverKind, bool)> {
    Some(match status {
        "Distortion" | "Invulnerability" | "Determined" => (CoverKind::Invulnerability, false),
        "Aegis" => (CoverKind::Aegis, true),
        "Stability" => (CoverKind::Stability, true),
        "Resistance" => (CoverKind::Resistance, true),
        "Protection" => (CoverKind::Protection, true),
        "Blind" | "Blinded" | "Blindness" => (CoverKind::Blind, false),
        _ => return None,
    })
}

fn text_describes_boon_strip(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("boons removed")
        || (t.contains("boon") && t.contains("remov") && !t.contains("condit"))
}

fn push_description_effects(effects: &mut Vec<SkillEffect>, description: &str) {
    let d = description.to_lowercase();
    let has_corrupt = effects
        .iter()
        .any(|e| matches!(e, SkillEffect::CorruptBoons));
    if !has_corrupt && describes_corrupt(&d) {
        effects.push(SkillEffect::CorruptBoons);
    }
    if !effects.iter().any(|e| matches!(e, SkillEffect::StealBoons)) && describes_steal(&d) {
        effects.push(SkillEffect::StealBoons);
    }
    if !effects
        .iter()
        .any(|e| matches!(e, SkillEffect::ConvertConditions))
        && describes_convert_conditions(&d)
    {
        effects.push(SkillEffect::ConvertConditions);
    }
    if !effects
        .iter()
        .any(|e| matches!(e, SkillEffect::Mobility { .. }))
    {
        if let Some(kind) = mobility_from_text(&d) {
            effects.push(SkillEffect::Mobility { kind });
        }
    }
    let has_block_cover = effects.iter().any(|e| {
        matches!(
            e,
            SkillEffect::Cover {
                kind: CoverKind::Block,
                ..
            }
        )
    });
    if !has_block_cover && text_describes_block(&d) {
        effects.push(SkillEffect::Cover {
            kind: CoverKind::Block,
            duration_ms: 0,
            strippable: false,
        });
    }
    let has_stability = effects.iter().any(|e| match e {
        SkillEffect::Cover {
            kind: CoverKind::Stability,
            ..
        } => true,
        SkillEffect::ApplyBuff { buff, .. } => buff.eq_ignore_ascii_case("Stability"),
        _ => false,
    });
    if !has_stability && text_describes_stability(&d) {
        effects.push(SkillEffect::Cover {
            kind: CoverKind::Stability,
            duration_ms: 0,
            strippable: true,
        });
        effects.push(SkillEffect::ApplyBuff {
            buff: "Stability".into(),
            stacks: 1,
            duration_ms: 0,
        });
    }
    if d.contains("barrier")
        && !effects
            .iter()
            .any(|effect| matches!(effect, SkillEffect::Barrier { .. }))
    {
        // The public skill endpoint commonly omits barrier coefficients.
        // Heuristic Barrier/Healing are simulated at full value with no report flag.
        effects.push(SkillEffect::Barrier { amount: 1_000.0 });
    }
    if (d.contains("heal yourself") || d.contains("heals you"))
        && !effects
            .iter()
            .any(|effect| matches!(effect, SkillEffect::Healing { .. }))
    {
        effects.push(SkillEffect::Healing { hit_count: 1 });
    }
}

fn describes_corrupt(d: &str) -> bool {
    (d.contains("converting boons") || d.contains("convert boons") || d.contains("corrupts"))
        && d.contains("condition")
}

fn describes_steal(d: &str) -> bool {
    (d.contains("steal") || d.contains("transfer")) && d.contains("boon")
}

fn describes_convert_conditions(d: &str) -> bool {
    d.contains("convert") && d.contains("condition") && d.contains("boon") && !describes_corrupt(d)
}

fn mobility_from_text(d: &str) -> Option<MobilityKind> {
    if d.contains("shadowstep") || d.contains("teleport") {
        Some(MobilityKind::Teleport)
    } else if d.contains("stealth") {
        Some(MobilityKind::Stealth)
    } else if d.contains("superspeed") {
        Some(MobilityKind::Superspeed)
    } else if d.contains("evade") || d.contains("dodge") {
        Some(MobilityKind::Evade)
    } else if d.contains("leap") || d.contains("dash") || d.contains("retreat") {
        Some(MobilityKind::Leap)
    } else {
        None
    }
}

fn condition_cleanse_count_from_text(text: &str, value: Option<i32>) -> Option<u32> {
    if text_describes_condition_cleanse(text) {
        Some(value.unwrap_or(1).max(1) as u32)
    } else {
        None
    }
}

/// GW2 damaging conditions (the damaging subset of `is_condition`).
///
/// Accepts either verb-form (Poison) or canonical (Poisoned) — input is
/// normalized via `canonical_condition_name` so the arms only list
/// canonical form.
fn is_damaging_condition(status: &str) -> bool {
    let canonical = crate::data::boon_condition_formulas::canonical_condition_name(status);
    matches!(
        canonical,
        "Bleeding" | "Burning" | "Poisoned" | "Torment" | "Confusion"
    )
}

/// Test-only thin wrapper so the alias-routing regression suite can fuzz
/// the private `is_damaging_condition` helper without changing its
/// visibility.
#[cfg(test)]
pub(crate) mod tests_alias_helpers {
    pub(crate) fn is_damaging_condition(status: &str) -> bool {
        super::is_damaging_condition(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::{strip_total, ControlKind, CoverKind, MobilityKind};

    fn make_test_skill(id: u32, name: &str, slot: &str, facts: Vec<Fact>) -> Skill {
        Skill {
            id,
            name: name.to_string(),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: None,
            professions: vec!["Warrior".to_string()],
            slot: Some(slot.to_string()),
            facts,
            traited_facts: vec![],
            categories: vec![],
            attunement: None,
            cost: None,
            dual_wield: None,
            flip_skill: None,
            initiative: None,
            next_chain: None,
            prev_chain: None,
            transform_skills: vec![],
            bundle_skills: vec![],
            toolbelt_skill: None,
            flags: vec![],
            specialization: None,
        }
    }

    /// Combat Stimulant 62978 publishes Recharge 1 s, Maximum Count 2 and
    /// Count Recharge 20 s: the sustainable rate is one press per 10 s, not
    /// one per second. Facts copied from the live API.
    #[test]
    fn an_ammo_skill_amortizes_its_count_recharge() {
        let facts: Vec<Fact> = serde_json::from_value(serde_json::json!([
            {"type": "Recharge", "text": "Recharge", "value": 1.0},
            {"type": "Number", "text": "Maximum Count", "value": 2},
            {"type": "Time", "text": "Count Recharge", "duration": 20},
            {"type": "Time", "text": "Count Recharge", "duration": 25}
        ]))
        .expect("facts");
        assert_eq!(extract_cooldown(&facts), 10_000);

        // No ammunition: the recharge stands on its own.
        let plain: Vec<Fact> = serde_json::from_value(serde_json::json!([
            {"type": "Recharge", "text": "Recharge", "value": 24.0}
        ]))
        .expect("facts");
        assert_eq!(extract_cooldown(&plain), 24_000);

        // A long inter-cast delay is the floor, not the amortized rate.
        let slow_cast: Vec<Fact> = serde_json::from_value(serde_json::json!([
            {"type": "Recharge", "text": "Recharge", "value": 15.0},
            {"type": "Number", "text": "Maximum Count", "value": 3},
            {"type": "Time", "text": "Count Recharge", "duration": 30}
        ]))
        .expect("facts");
        assert_eq!(extract_cooldown(&slow_cast), 15_000);
    }

    /// Every ammunition skill in the live database resolves to at least its
    /// sustainable rate. A blanket "no slotted skill is faster than 3 s"
    /// sweep was the original ask, but 110 slotted skills are legitimately
    /// faster than that -- engineer kits and their stow skills at 0, spirit
    /// weapon and Ventari tablet actives at 1-2 s, flip skills -- so the
    /// allowlist would be the test. This invariant needs none.
    #[test]
    #[ignore = "reads the live skill cache"]
    fn no_ammo_skill_in_the_cache_resolves_faster_than_its_count_recharge() {
        let cache = gw2_api::cache::DataCache::new(
            gw2_api::dev_config::cache_dir().expect("dev.cfg cache dir"),
        );
        let db = crate::gamedb::GameDb::load(&cache).expect("cached game data");
        let mut checked = 0;
        for (id, skill) in &db.skills {
            let ammo = skill.facts.iter().find_map(|fact| match fact {
                Fact::Number {
                    text: Some(text),
                    value: Some(value),
                    ..
                } if text.eq_ignore_ascii_case("Maximum Count") && *value > 0 => {
                    Some(*value as u32)
                }
                _ => None,
            });
            let count_recharge = skill.facts.iter().find_map(|fact| match fact {
                Fact::Time {
                    text: Some(text),
                    duration: Some(duration),
                    ..
                } if text.eq_ignore_ascii_case("Count Recharge") => Some(*duration * 1_000),
                _ => None,
            });
            let (Some(count), Some(recharge)) = (ammo, count_recharge) else {
                continue;
            };
            checked += 1;
            assert!(
                extract_cooldown(&skill.facts) >= recharge / count,
                "{} ({id}) resolves to {} ms, under its {} ms sustainable rate",
                skill.name,
                extract_cooldown(&skill.facts),
                recharge / count
            );
        }
        assert!(checked > 20, "only {checked} ammunition skills found");
    }

    #[test]
    fn test_extract_cooldown() {
        let facts = vec![Fact::Recharge {
            text: Some("Recharge".into()),
            icon: None,
            value: Some(8.0),
        }];
        assert_eq!(extract_cooldown(&facts), 8000);
    }

    #[test]
    fn test_extract_cooldown_missing() {
        let facts = vec![Fact::Damage {
            text: None,
            icon: None,
            hit_count: Some(1),
            dmg_multiplier: Some(1.0),
        }];
        assert_eq!(extract_cooldown(&facts), 0);
    }

    #[test]
    fn test_extract_effects_damage() {
        let facts = vec![Fact::Damage {
            text: None,
            icon: None,
            hit_count: Some(3),
            dmg_multiplier: Some(1.5),
        }];
        let effects = extract_effects(&facts, None);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            SkillEffect::StrikeDamage {
                hit_count,
                dmg_multiplier,
            } => {
                assert_eq!(*hit_count, 3);
                assert!((dmg_multiplier - 1.5).abs() < 0.01);
            }
            _ => panic!("Expected StrikeDamage"),
        }
    }

    #[test]
    fn test_extract_effects_condition() {
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Bleeding".into()),
            duration: Some(6),
            apply_count: Some(2),
            description: None,
        }];
        let effects = extract_effects(&facts, None);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            SkillEffect::ApplyCondition {
                condition,
                stacks,
                duration_ms,
            } => {
                assert_eq!(condition, "Bleeding");
                assert_eq!(*stacks, 2);
                assert_eq!(*duration_ms, 6000);
            }
            _ => panic!("Expected ApplyCondition"),
        }
    }

    #[test]
    fn test_extract_effects_buff() {
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Might".into()),
            duration: Some(10),
            apply_count: Some(3),
            description: None,
        }];
        let effects = extract_effects(&facts, None);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            SkillEffect::ApplyBuff {
                buff,
                stacks,
                duration_ms,
            } => {
                assert_eq!(buff, "Might");
                assert_eq!(*stacks, 3);
                assert_eq!(*duration_ms, 10000);
            }
            _ => panic!("Expected ApplyBuff"),
        }
    }

    #[test]
    fn duration_saturating_mul() {
        // A duration this large never comes from the live API — it exists to prove
        // saturation, not to model real GW2 data. Before the fix, `duration.unwrap_or(0)
        // * 1000` on a `Fact::Buff`/`Fact::PrefixedBuff` duration overflowed u32
        // (panic in debug, silent wraparound in release) for any value over
        // u32::MAX / 1000. `saturating_mul` must clamp to u32::MAX instead.
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Might".into()),
            duration: Some(u32::MAX),
            apply_count: Some(1),
            description: None,
        }];
        let effects = extract_effects(&facts, None);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            SkillEffect::ApplyBuff { duration_ms, .. } => {
                assert_eq!(*duration_ms, u32::MAX);
            }
            _ => panic!("Expected ApplyBuff"),
        }
    }

    #[test]
    fn stun_is_lock_not_buff() {
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Stun".into()),
            duration: Some(1),
            apply_count: Some(1),
            description: None,
        }];
        let effects = extract_effects(&facts, None);
        match &effects[0] {
            SkillEffect::CrowdControl {
                kind: ControlKind::Stun,
                stops_dodge: true,
                duration_ms: 1000,
            } => {}
            other => panic!("expected lock Stun, got {other:?}"),
        }
    }

    #[test]
    fn daze_is_interrupt_not_lock() {
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Daze".into()),
            duration: Some(1),
            apply_count: Some(1),
            description: None,
        }];
        let effects = extract_effects(&facts, None);
        match &effects[0] {
            SkillEffect::CrowdControl {
                kind: ControlKind::Daze,
                stops_dodge: false,
                ..
            } => {}
            other => panic!("expected interrupt Daze, got {other:?}"),
        }
    }

    #[test]
    fn immobilize_stops_dodge() {
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Immobile".into()),
            duration: Some(2),
            apply_count: Some(1),
            description: None,
        }];
        let effects = extract_effects(&facts, None);
        match &effects[0] {
            SkillEffect::CrowdControl {
                kind: ControlKind::Immobilize,
                stops_dodge: true,
                ..
            } => {}
            other => panic!("expected immobilize lock, got {other:?}"),
        }
    }

    #[test]
    fn wod_strip_is_rate_not_one_boon() {
        let facts = vec![
            Fact::Number {
                text: Some("Boons Removed".into()),
                icon: None,
                value: Some(1),
            },
            Fact::Time {
                text: Some("Interval".into()),
                icon: None,
                duration: Some(1),
            },
            Fact::Time {
                text: Some("Duration".into()),
                icon: None,
                duration: Some(5),
            },
        ];
        let effects = extract_effects(&facts, None);
        let strip = effects
            .iter()
            .find(|e| matches!(e, SkillEffect::StripBoons { .. }))
            .expect("strip effect");
        assert_eq!(strip_total(strip), 5, "WoD is 1/s × 5s, not 1");
    }

    #[test]
    fn well_of_corruption_corrupt_from_description() {
        let facts = vec![Fact::Damage {
            text: None,
            icon: None,
            hit_count: Some(1),
            dmg_multiplier: Some(0.5),
        }];
        let desc = Some("Target area pulses, converting boons on foes into conditions.");
        let effects = extract_effects(&facts, desc);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, SkillEffect::CorruptBoons)),
            "WoC must not look like a pulse damage field"
        );
    }

    #[test]
    fn teleport_description_is_roam_out() {
        let effects = extract_effects(
            &[],
            Some("Shadowstep to the target. Teleport to a nearby ally."),
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            SkillEffect::Mobility {
                kind: MobilityKind::Teleport
            }
        )));
    }

    #[test]
    fn evade_description_is_roam_out() {
        let effects = extract_effects(&[], Some("Evade backward. Dodge incoming attacks."));
        assert!(effects.iter().any(|e| matches!(
            e,
            SkillEffect::Mobility {
                kind: MobilityKind::Evade
            }
        )));
    }

    #[test]
    fn block_description_is_roam_out() {
        let effects = extract_effects(&[], Some("Block the next attack."));
        assert!(effects.iter().any(|e| matches!(
            e,
            SkillEffect::Cover {
                kind: CoverKind::Block,
                duration_ms: 0,
                ..
            }
        )));
        let unblockable = extract_effects(&[], Some("This attack is unblockable."));
        assert!(!unblockable.iter().any(|e| matches!(
            e,
            SkillEffect::Cover {
                kind: CoverKind::Block,
                ..
            }
        )));
    }

    #[test]
    fn description_only_cover_never_invents_a_duration() {
        let effects = extract_effects(
            &[],
            Some("Block attacks and gain stability while channeling."),
        );
        assert!(effects
            .iter()
            .filter_map(|effect| match effect {
                SkillEffect::Cover { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .all(|duration_ms| duration_ms == 0));
    }

    #[test]
    fn sourced_skill_values_are_isolated_by_mode() {
        let pve = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let pvp = BalanceContext::new(gw2_core::types::GameMode::PvP);
        let wvw = BalanceContext::new(gw2_core::types::GameMode::WvW);

        assert_eq!(sourced_skill_u32(&pve, 13113, "initiative_cost"), Some(6));
        assert_eq!(sourced_skill_u32(&pvp, 13113, "initiative_cost"), Some(6));
        assert_eq!(sourced_skill_u32(&wvw, 13113, "initiative_cost"), Some(7));
        assert_eq!(
            sourced_skill_u32(&wvw, 13113, "combo_field_duration_ms"),
            Some(4_000)
        );
        assert_eq!(
            sourced_skill_value(&pve, 13097, "damage_coefficient:below_25"),
            Some(2.2)
        );
        assert_eq!(
            sourced_skill_value(&wvw, 13097, "damage_coefficient:below_25"),
            Some(2.0)
        );
        assert_eq!(sourced_skill_u32(&pve, 13097, "activation_ms"), Some(750));
        assert_eq!(sourced_skill_u32(&pvp, 13097, "activation_ms"), Some(750));
        assert_eq!(sourced_skill_u32(&wvw, 13097, "activation_ms"), Some(750));
    }

    #[test]
    fn sourced_damage_profiles_keep_exact_mode_specific_thresholds() {
        let pve = sourced_damage_coefficient_profile(
            &BalanceContext::new(gw2_core::types::GameMode::PvE),
            13097,
        )
        .expect("PvE Heartseeker profile");
        let pvp = sourced_damage_coefficient_profile(
            &BalanceContext::new(gw2_core::types::GameMode::PvP),
            13097,
        )
        .expect("PvP Heartseeker profile");
        let wvw = sourced_damage_coefficient_profile(
            &BalanceContext::new(gw2_core::types::GameMode::WvW),
            13097,
        )
        .expect("WvW Heartseeker profile");

        assert_eq!(
            (pve.above_50, pve.below_50, pve.below_25),
            (1.0, Some(1.6), Some(2.2))
        );
        assert_eq!(
            (pvp.above_50, pvp.below_50, pvp.below_25),
            (1.0, Some(1.5), Some(2.0))
        );
        assert_eq!(
            (wvw.above_50, wvw.below_50, wvw.below_25),
            (1.0, Some(1.5), Some(2.0))
        );
        assert!(pve.threshold_selection_is_provisional);
        assert!(pvp.threshold_selection_is_provisional);
        assert!(wvw.threshold_selection_is_provisional);
    }

    #[test]
    fn sourced_threshold_rows_emit_one_initial_target_strike() {
        let facts = vec![
            Fact::Damage {
                text: Some("Damage".into()),
                icon: None,
                hit_count: Some(1),
                dmg_multiplier: Some(1.0),
            },
            Fact::Damage {
                text: Some("Damage below 50%".into()),
                icon: None,
                hit_count: Some(1),
                dmg_multiplier: Some(1.75),
            },
            Fact::Damage {
                text: Some("Damage below 25%".into()),
                icon: None,
                hit_count: Some(1),
                dmg_multiplier: Some(2.5),
            },
        ];

        for mode in [
            gw2_core::types::GameMode::PvE,
            gw2_core::types::GameMode::PvP,
            gw2_core::types::GameMode::WvW,
        ] {
            let mode_label = format!("{mode:?}");
            let effects =
                extract_effects_for_context(13097, &facts, None, &BalanceContext::new(mode));
            let strikes: Vec<_> = effects
                .iter()
                .filter_map(|effect| match effect {
                    SkillEffect::StrikeDamage {
                        hit_count,
                        dmg_multiplier,
                    } => Some((*hit_count, *dmg_multiplier)),
                    _ => None,
                })
                .collect();

            assert_eq!(
                strikes,
                vec![(1, 1.0)],
                "threshold rows stacked in {mode_label}"
            );
        }
    }

    #[test]
    fn distortion_is_unstrippable_cover() {
        let facts = vec![Fact::Buff {
            text: None,
            icon: None,
            status: Some("Distortion".into()),
            duration: Some(1),
            apply_count: Some(1),
            description: Some("Immune to conditions and damage.".into()),
        }];
        let effects = extract_effects(&facts, None);
        match &effects[0] {
            SkillEffect::Cover {
                kind: CoverKind::Invulnerability,
                strippable: false,
                ..
            } => {}
            other => panic!("expected invuln cover, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_effects_prefixed_buff_condition() {
        // PrefixedBuff is used by AoE and on-hit effects; must be handled same as Buff.
        use gw2_api::models::facts::BuffPrefix;
        let facts = vec![Fact::PrefixedBuff {
            text: None,
            icon: None,
            status: Some("Bleeding".into()),
            duration: Some(5),
            apply_count: Some(3),
            description: None,
            prefix: Some(BuffPrefix {
                text: Some("To nearby enemies".into()),
                icon: None,
                status: None,
                description: None,
            }),
        }];
        let effects = extract_effects(&facts, None);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            SkillEffect::ApplyCondition {
                condition,
                stacks,
                duration_ms,
            } => {
                assert_eq!(condition, "Bleeding");
                assert_eq!(*stacks, 3);
                assert_eq!(*duration_ms, 5000);
            }
            _ => panic!("Expected ApplyCondition from PrefixedBuff"),
        }
    }

    #[test]
    fn test_skill_to_rotation() {
        let skill = make_test_skill(
            100,
            "Chop",
            "Weapon_1",
            vec![
                Fact::Damage {
                    text: None,
                    icon: None,
                    hit_count: Some(1),
                    dmg_multiplier: Some(0.8),
                },
                Fact::Recharge {
                    text: None,
                    icon: None,
                    value: Some(0.0),
                },
            ],
        );
        let rs = skill_to_rotation(&skill);
        assert_eq!(rs.skill_id, 100);
        assert_eq!(rs.name, "Chop");
        assert_eq!(rs.slot, SkillSlot::Weapon1);
        assert_eq!(rs.cooldown_ms, 0);
        assert_eq!(rs.effects.len(), 1);
    }

    #[test]
    fn test_stunbreak_detection() {
        let skill = make_test_skill(
            200,
            "Shake It Off!",
            "Utility",
            vec![Fact::StunBreak {
                text: Some("Stun Break".into()),
                icon: None,
                value: Some(true),
            }],
        );
        let rs = skill_to_rotation(&skill);
        assert!(rs.is_stunbreak);
    }

    // Tests for enrich_with_cleanse

    use crate::data::normalized_effects::{
        AmountMode, EffectCategory, NormalizedEffect, OperationType, SourceType, StackingRule,
        StatusOperation, TargetScope, TargetSide, TriggerRule, UptimeModel, UptimeModelKind,
    };
    use crate::data::quality::FactualValue;
    use crate::data::EvidenceLevel;
    use crate::gamedb::GameDb;
    use std::collections::HashMap;

    /// Build a minimal empty GameDb for test purposes.
    fn empty_db() -> GameDb {
        GameDb {
            items: HashMap::new(),
            itemstats: HashMap::new(),
            skills: HashMap::new(),
            traits: HashMap::new(),
            specializations: HashMap::new(),
            professions: HashMap::new(),
            legends: HashMap::new(),
            pvp_amulets: HashMap::new(),
            pets: HashMap::new(),
            skills_by_profession: HashMap::new(),
            traits_by_spec: HashMap::new(),
            items_by_type: HashMap::new(),
            runes: vec![],
            sigils: vec![],
            relics: vec![],
            skill_to_palette: HashMap::new(),
            palette_to_skill: HashMap::new(),
            traits_by_condition: HashMap::new(),
            skills_by_condition: HashMap::new(),
            traits_by_buff: HashMap::new(),
            skills_by_buff: HashMap::new(),
            localized: None,
        }
    }

    /// Build a minimal `NormalizedEffect` with `RemovesCondition` for a given skill ID and count.
    fn cleanse_ne(source_id: u32, count: f64) -> NormalizedEffect {
        NormalizedEffect {
            effect_id: format!("skill:{}:cleanse", source_id),
            source_type: SourceType::Skill,
            source_id,
            source_name: format!("Skill {}", source_id),
            category: EffectCategory::RemovesCondition,
            value: FactualValue::Resolved(count),
            stacking_rule: StackingRule::NonStacking,
            trigger_rule: TriggerRule::OnSkillUse,
            uptime_model: UptimeModel {
                kind: UptimeModelKind::Unknown,
                uptime: None,
            },
            evidence_level: EvidenceLevel::Factual,
            source: None,
            effect_duration: None,
            internal_cooldown: None,
            max_stacks: None,
            status_operation: Some(StatusOperation {
                operation_type: OperationType::RemovesCondition,
                target_side: TargetSide::Self_,
                status_kind: "Any".to_string(),
                amount_mode: AmountMode::Count,
                amount_value: FactualValue::Resolved(count),
                base_duration_ms: None,
                target_scope: TargetScope::Self_,
                target_count: None,
                internal_cooldown_ms: None,
                source_duration_multiplier: None,
            }),
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

    /// Build a minimal rotation skill for cleanse tests.
    fn cleanse_test_skill(id: u32) -> RotationSkill {
        RotationSkill {
            targets: 1,
            skill_id: id,
            name: format!("Skill {}", id),
            slot: SkillSlot::Utility,
            cast_time_ms: 500,
            cooldown_ms: 20000,
            effects: vec![],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
            categories: Vec::new(),
            slot_name: None,
        }
    }

    #[test]
    fn test_enrich_with_cleanse_ne_primary() {
        // NormalizedEffects has a RemovesCondition entry → should add effect.
        // (990_001: an id the cleanse registry does not know; 9158 Signet of
        // Resolve is in the table and the table decides before NE data.)
        let mut skills = vec![cleanse_test_skill(990_001)];
        let ne = vec![cleanse_ne(990_001, 3.0)];
        let db = empty_db();

        enrich_with_cleanse(&mut skills, &ne, &db, &[]);

        let cleanse_effects: Vec<_> = skills[0]
            .effects
            .iter()
            .filter_map(|e| {
                if let SkillEffect::RemovesCondition { conditions_removed } = e {
                    Some(*conditions_removed)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            cleanse_effects,
            vec![3],
            "should detect 3 conditions removed from NE data"
        );
    }

    #[test]
    fn test_enrich_with_cleanse_description_fallback() {
        // No NE entry, but skill description contains "removes condition" phrasing.
        let mut skill_entry = make_test_skill(500, "Mending", "Heal", vec![]);
        skill_entry.description = Some("Cure conditions affecting you.".to_string());

        let mut db = empty_db();
        db.skills.insert(500, skill_entry);

        let mut skills = vec![cleanse_test_skill(500)];
        enrich_with_cleanse(&mut skills, &[], &db, &[]);

        let has_cleanse = skills[0].effects.iter().any(|e| {
            matches!(
                e,
                SkillEffect::RemovesCondition {
                    conditions_removed: 1
                }
            )
        });
        assert!(
            has_cleanse,
            "description heuristic should detect cleanse from 'cure...condition'"
        );
    }

    /// An off-hand dagger on both sets: Deathly Swarm is one skill with one
    /// cooldown, usable on either set; the sets' own skills keep their tags.
    #[test]
    fn merge_weapon_sets_keeps_one_copy_of_a_shared_skill() {
        let weapon = |id: u32, set: u8| RotationSkill {
            targets: 1,
            skill_id: id,
            name: format!("Skill {id}"),
            slot: SkillSlot::Weapon4,
            cast_time_ms: 500,
            cooldown_ms: 16_000,
            effects: vec![],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: set,
            categories: Vec::new(),
            slot_name: None,
        };
        let set1 = vec![weapon(1, 1), weapon(10705, 1)];
        let set2 = vec![weapon(10705, 2), weapon(2, 2)];
        let merged = merge_weapon_sets(set1, set2);
        let sets: Vec<(u32, u8)> = merged.iter().map(|s| (s.skill_id, s.weapon_set)).collect();
        assert_eq!(sets, vec![(1, 1), (10705, 0), (2, 2)]);
    }

    /// The registry decides before any text: "Suffer!" (30670) has no
    /// description in this fixture and still counts its two transfers, and a
    /// Cleansing Ire burst counts only with the trait (1649) equipped.
    #[test]
    fn registry_decides_before_text_and_honours_required_traits() {
        let db = empty_db();
        let removed = |s: &RotationSkill| {
            s.effects.iter().find_map(|e| match e {
                SkillEffect::RemovesCondition { conditions_removed } => Some(*conditions_removed),
                _ => None,
            })
        };
        let mut skills = vec![cleanse_test_skill(30670), cleanse_test_skill(14422)];
        enrich_with_cleanse(&mut skills, &[], &db, &[]);
        assert_eq!(
            removed(&skills[0]),
            Some(2),
            "\"Suffer!\" transfers 1 + 1 additional"
        );
        assert_eq!(
            removed(&skills[1]),
            None,
            "Eviscerate without Cleansing Ire"
        );
        let mut traited = vec![cleanse_test_skill(14422)];
        enrich_with_cleanse(&mut traited, &[], &db, &[1649]);
        assert!(
            removed(&traited[0]).is_some(),
            "Eviscerate with Cleansing Ire"
        );
    }

    #[test]
    fn test_enrich_with_cleanse_no_match() {
        // No NE entry, no matching description → no cleanse effect added.
        let mut db = empty_db();
        let mut skill_entry = make_test_skill(999, "Fireball", "Utility", vec![]);
        skill_entry.description = Some("Deal fire damage.".to_string());
        db.skills.insert(999, skill_entry);

        let mut skills = vec![cleanse_test_skill(999)];
        enrich_with_cleanse(&mut skills, &[], &db, &[]);

        let has_cleanse = skills[0]
            .effects
            .iter()
            .any(|e| matches!(e, SkillEffect::RemovesCondition { .. }));
        assert!(!has_cleanse, "no cleanse effect for non-cleanse skill");
    }

    #[test]
    fn test_enrich_with_cleanse_idempotent() {
        // Calling enrich twice should not add duplicate cleanse effects.
        let mut skills = vec![cleanse_test_skill(9158)];
        let ne = vec![cleanse_ne(9158, 3.0)];
        let db = empty_db();

        enrich_with_cleanse(&mut skills, &ne, &db, &[]);
        enrich_with_cleanse(&mut skills, &ne, &db, &[]);

        let cleanse_count = skills[0]
            .effects
            .iter()
            .filter(|e| matches!(e, SkillEffect::RemovesCondition { .. }))
            .count();
        assert_eq!(
            cleanse_count, 1,
            "idempotency: only one RemovesCondition effect"
        );
    }

    #[test]
    fn test_enrich_with_cleanse_max_across_multiple_ne_entries() {
        // Multiple NE entries for same source_id → take maximum conditions_removed.
        let mut skills = vec![cleanse_test_skill(100)];
        let ne = vec![
            cleanse_ne(100, 1.0),
            cleanse_ne(100, 5.0),
            cleanse_ne(100, 2.0),
        ];
        let db = empty_db();

        enrich_with_cleanse(&mut skills, &ne, &db, &[]);

        let max_count = skills[0].effects.iter().find_map(|e| {
            if let SkillEffect::RemovesCondition { conditions_removed } = e {
                Some(*conditions_removed)
            } else {
                None
            }
        });
        assert_eq!(
            max_count,
            Some(5),
            "should take maximum conditions_removed across entries"
        );
    }

    /// Sprint 2 (T046): the shroud bar comes from the core entry skill's
    /// `transform_skills`, filtered by the equipped elite, in slot order.
    #[test]
    fn form_bar_is_prepared_from_transform_skills() {
        let mut db = empty_db();
        let mut death_shroud = make_test_skill(10574, "Death Shroud", "Profession_1", vec![]);
        death_shroud.flip_skill = Some(10585);
        death_shroud.transform_skills = vec![
            29442, 29458, 30278, 30825, 29958, 30504, 30557, // Reaper
            10554, 10604, 10645, 10643, 19504, // core
            62611, // Harbinger
            30961, // an exit skill with a Profession_ slot: never a bar skill
        ];
        let mut reapers_shroud = make_test_skill(30792, "Reaper's Shroud", "Profession_1", vec![]);
        reapers_shroud.flip_skill = Some(30961);
        let mut skills = vec![death_shroud, reapers_shroud];
        for (id, name, slot, spec) in [
            (29442, "Life Rend", "Downed_1", Some(34)),
            (29458, "Life Slash", "Downed_1", Some(34)),
            (30278, "Life Reap", "Downed_1", Some(34)),
            (30825, "Death's Charge", "Downed_2", Some(34)),
            (29958, "Infusing Terror", "Downed_3", Some(34)),
            (30504, "Soul Spiral", "Downed_4", Some(34)),
            (30557, "Executioner's Scythe", "Weapon_5", Some(34)),
            (10554, "Life Blast", "Downed_1", None),
            (10604, "Dark Path", "Downed_2", None),
            (10645, "Wave of Fear", "Downed_3", None),
            (10643, "Gathering Plague", "Downed_4", None),
            (19504, "Tainted Shackles", "Weapon_5", None),
            (62611, "Tainted Bolts", "Downed_1", Some(64)),
            (30961, "Exit Reaper's Shroud", "Profession_1", None),
        ] {
            let mut skill = make_test_skill(id, name, slot, vec![]);
            skill.specialization = spec;
            skills.push(skill);
        }
        let ids: Vec<u32> = skills.iter().map(|s| s.id).collect();
        for skill in skills {
            db.skills.insert(skill.id, skill);
        }
        db.skills_by_profession.insert("Necromancer".into(), ids);

        let reaper: Vec<u32> = form_bar_for_build(&db, "Necromancer", &[53, 2, 34])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            reaper,
            vec![29442, 29458, 30278, 30825, 29958, 30504, 30557]
        );

        let core: Vec<u32> = form_bar_for_build(&db, "Necromancer", &[53, 2, 19])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(core, vec![10554, 10604, 10645, 10643, 19504]);

        assert!(form_bar_for_build(&db, "Warrior", &[34]).is_empty());
    }

    /// Celestial Avatar's shape in the API: the entry (Profession_5, spec
    /// Druid) lists its own bar, each skill with an aquatic twin of the same
    /// name, and its flip (Release) sorts ahead of it by id. Off-chain flips
    /// on the bar (Terrify after Infusing Terror) are not free presses.
    #[test]
    fn druid_form_bar_is_the_land_bar_and_f5_is_the_entry() {
        let mut db = empty_db();
        let mut avatar = make_test_skill(31869, "Celestial Avatar", "Profession_5", vec![]);
        avatar.specialization = Some(5);
        avatar.flip_skill = Some(31411);
        avatar.transform_skills = vec![31796, 33387, 31503, 34070, 9, 7, 8];
        let mut release =
            make_test_skill(31411, "Release Celestial Avatar", "Profession_5", vec![]);
        release.specialization = Some(5);
        let mut skills = vec![avatar, release];
        for (id, name, slot, land) in [
            (31796, "Cosmic Ray", "Weapon_1", true),
            (33387, "Cosmic Ray", "Weapon_1", false),
            (31503, "Natural Convergence", "Weapon_5", true),
            (34070, "Natural Convergence", "Weapon_5", false),
            // A renamed aquatic twin (Voracious Dive beside Voracious Arc):
            // its slot holds a land-only skill, so it is off the land bar.
            (9, "Renamed Dive", "Weapon_5", false),
        ] {
            let mut skill = make_test_skill(id, name, slot, vec![]);
            if land {
                skill.flags = vec!["NoUnderwater".into()];
            }
            skills.push(skill);
        }
        let mut parent = make_test_skill(7, "Infusing Terror", "Weapon_3", vec![]);
        parent.flip_skill = Some(8);
        skills.push(parent);
        skills.push(make_test_skill(8, "Terrify", "Weapon_3", vec![]));
        let ids: Vec<u32> = skills.iter().map(|s| s.id).collect();
        for skill in skills {
            db.skills.insert(skill.id, skill);
        }
        db.skills_by_profession.insert("Ranger".into(), ids);

        let bar: Vec<u32> = form_bar_for_build(&db, "Ranger", &[5])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(bar, vec![31796, 7, 31503]);
        assert!(form_bar_for_build(&db, "Ranger", &[55]).is_empty());
        assert_eq!(form_bar_slot(Some("Downed_2")), Some(SkillSlot::Weapon2));
        assert_eq!(form_bar_slot(Some("Weapon_5")), Some(SkillSlot::Weapon5));
        assert_eq!(form_bar_slot(Some("Profession_1")), None);

        let f5 = profession_skills_for_build(
            &db,
            "Ranger",
            &[5],
            &crate::validation::ValidatedWeapons::default(),
        );
        assert_eq!(f5, vec![(31869, "Celestial Avatar".into())]);
    }

    /// Harbinger shroud skills the API publishes without damage or daze
    /// facts are sourced from the wiki per mode (balance overrides), so
    /// they are never silent zeros on the bar; the Scythe's activation too.
    #[test]
    fn sourced_shroud_skills_carry_the_wiki_numbers() {
        use gw2_core::types::GameMode;
        let mut db = empty_db();
        let recharge = |s: f64| Fact::Recharge {
            text: None,
            icon: None,
            value: Some(s),
        };
        for (id, name, slot, rech) in [
            (62672, "Devouring Cut", "Downed_3", 8.0),
            (62539, "Voracious Arc", "Downed_4", 10.0),
            (30557, "Executioner's Scythe", "Weapon_5", 30.0),
        ] {
            db.skills
                .insert(id, make_test_skill(id, name, slot, vec![recharge(rech)]));
        }
        let get = |mode: GameMode, id: u32| {
            build_rotation_skills_for_context(&[id], &db, &BalanceContext::new(mode))
                .pop()
                .expect("skill")
        };
        let strike = |skill: &RotationSkill| -> f64 {
            skill
                .effects
                .iter()
                .map(|e| match e {
                    SkillEffect::StrikeDamage {
                        hit_count,
                        dmg_multiplier,
                    } => *hit_count as f64 * dmg_multiplier,
                    _ => 0.0,
                })
                .sum()
        };
        let arc_wvw = get(GameMode::WvW, 62539);
        assert_eq!(arc_wvw.cooldown_ms, 18_000);
        assert_eq!(arc_wvw.cast_time_ms, 750);
        assert!((strike(&arc_wvw) - 1.0).abs() < 1e-9);
        assert!(arc_wvw.effects.iter().any(|e| matches!(
            e,
            SkillEffect::CrowdControl {
                kind: ControlKind::Daze,
                duration_ms: 500,
                ..
            }
        )));
        assert!((strike(&get(GameMode::PvE, 62539)) - 1.4).abs() < 1e-9);
        assert_eq!(get(GameMode::PvE, 62539).cooldown_ms, 10_000);
        assert!((strike(&get(GameMode::WvW, 62672)) - 0.85).abs() < 1e-9);
        assert!((strike(&get(GameMode::PvE, 62672)) - 1.0).abs() < 1e-9);
        assert_eq!(get(GameMode::PvP, 62672).cooldown_ms, 10_000);
        assert_eq!(get(GameMode::PvE, 30557).cast_time_ms, 1_250);
    }

    #[test]
    fn profession_bar_prefers_the_equipped_elite_replacement() {
        let mut db = empty_db();
        let core_f1 = make_test_skill(1, "Core F1", "Profession_1", vec![]);
        let mut elite_f1 = make_test_skill(2, "Elite F1", "Profession_1", vec![]);
        elite_f1.specialization = Some(77);
        let mut other_elite_f1 = make_test_skill(3, "Other Elite F1", "Profession_1", vec![]);
        other_elite_f1.specialization = Some(88);
        let core_f2 = make_test_skill(4, "Core F2", "Profession_2", vec![]);

        for skill in [core_f1, elite_f1, other_elite_f1, core_f2] {
            db.skills.insert(skill.id, skill);
        }
        db.skills_by_profession
            .insert("Warrior".into(), vec![1, 2, 3, 4]);

        let selected = profession_skills_for_build(
            &db,
            "Warrior",
            &[77],
            &crate::validation::ValidatedWeapons::default(),
        );
        assert_eq!(
            selected,
            vec![(2, "Elite F1".into()), (4, "Core F2".into())]
        );
    }

    /// The burst belongs to the weapon in hand. Eviscerate is an Axe skill:
    /// a Spear Warrior presses the Spear burst, and the id sort only decides
    /// between candidates the build can actually hold.
    #[test]
    fn the_profession_bar_follows_the_equipped_weapon() {
        use crate::validation::{ValidatedWeaponSet, ValidatedWeapons};
        let mut db = empty_db();
        let mut axe = make_test_skill(14353, "Eviscerate", "Profession_1", vec![]);
        axe.weapon_type = Some("Axe".into());
        let mut spear = make_test_skill(14443, "Whirling Strike", "Profession_1", vec![]);
        spear.weapon_type = Some("Spear".into());
        let mut counter = make_test_skill(44165, "Full Counter", "Profession_2", vec![]);
        counter.weapon_type = Some("None".into());
        for skill in [axe, spear, counter] {
            db.skills.insert(skill.id, skill);
        }
        db.skills_by_profession
            .insert("Warrior".into(), vec![14353, 14443, 44165]);

        let spear_build = ValidatedWeapons {
            set1: ValidatedWeaponSet {
                main_hand: Some("Spear".into()),
                off_hand: None,
            },
            set2: ValidatedWeaponSet::default(),
        };
        assert_eq!(
            profession_skills_for_build(&db, "Warrior", &[], &spear_build),
            vec![
                (14443, "Whirling Strike".into()),
                (44165, "Full Counter".into())
            ],
            "Spear Warrior presses the Spear burst, and Full Counter is weapon-independent"
        );

        let axe_build = ValidatedWeapons {
            set1: ValidatedWeaponSet {
                main_hand: Some("Axe".into()),
                off_hand: Some("Shield".into()),
            },
            set2: ValidatedWeaponSet::default(),
        };
        assert_eq!(
            profession_skills_for_build(&db, "Warrior", &[], &axe_build)[0],
            (14353, "Eviscerate".into())
        );

        // No weapons resolved yet: the bar still fills, by id order.
        assert_eq!(
            profession_skills_for_build(&db, "Warrior", &[], &ValidatedWeapons::default())[0],
            (14353, "Eviscerate".into())
        );
    }
}
