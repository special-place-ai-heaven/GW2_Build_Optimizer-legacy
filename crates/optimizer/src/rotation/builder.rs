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
            let skill = db.skills.get(&db.out_of_form_variant(id))?;
            Some(skill_to_rotation_for_context(skill, ctx))
        })
        .collect()
}

/// A skill's facts with the equipped traits' `traited_facts` applied: a
/// traited fact with `overrides: i` replaces base fact `i`, one without is
/// added (Eclipse's Burning on Natural Convergence). Same rule as the trait
/// loop in `combat::extract_damage_modifiers`: collect the overridden
/// indices first, then skip those base facts. `i` indexes the API's raw
/// array; `deserialize_facts` keeps every typed fact (`Range`, `NoData`
/// included), so the indices line up (Poison Volley's `overrides: 6`).
///
/// A traited Damage fact of a trait with a bare "Damage Increase" is left
/// out: that increase reaches the skill once, as the trait's mode value,
/// through [`apply_skill_strike`] (the API's traited coefficient is the
/// PvE one: Willbender Flames 0.66 = +200%, WvW +100%).
fn active_skill_facts(skill: &Skill, equipped_traits: &[u32], db: &GameDb) -> Vec<Fact> {
    let scoped = |trait_id: u32| {
        db.traits
            .get(&trait_id)
            .is_some_and(crate::combat::has_bare_damage_increase)
    };
    let active: Vec<&gw2_api::models::TraitedFact> = skill
        .traited_facts
        .iter()
        .filter(|tf| equipped_traits.contains(&tf.requires_trait))
        .filter(|tf| !(matches!(tf.fact, Fact::Damage { .. }) && scoped(tf.requires_trait)))
        .collect();
    let overridden: Vec<u32> = active.iter().filter_map(|tf| tf.overrides).collect();
    skill
        .facts
        .iter()
        .enumerate()
        .filter(|(idx, _)| !overridden.contains(&(*idx as u32)))
        .map(|(_, fact)| fact.clone())
        .chain(active.iter().map(|tf| tf.fact.clone()))
        .collect()
}

/// Re-derive the effects of every bar skill whose facts the equipped traits
/// change (`traited_facts`). Call after the bars are built and before
/// [`enrich_with_cleanse`], which adds to the effects this replaces.
/// Cooldowns stay the base value: trait recharge reductions are applied by
/// the trait's own record, not by a traited `Recharge` fact.
pub fn apply_traited_facts(
    skills: &mut [RotationSkill],
    db: &GameDb,
    ctx: &BalanceContext,
    equipped_traits: &[u32],
) {
    for rotation_skill in skills.iter_mut() {
        let Some(skill) = db.skills.get(&rotation_skill.skill_id) else {
            continue;
        };
        if !skill
            .traited_facts
            .iter()
            .any(|tf| equipped_traits.contains(&tf.requires_trait))
        {
            continue;
        }
        let facts = active_skill_facts(skill, equipped_traits, db);
        rotation_skill.effects =
            extract_effects_for_context(skill.id, &facts, skill.description.as_deref(), ctx);
    }
}

/// Scale the strikes of the skills a trait's scoped damage increase names
/// ([`crate::combat::scope_skill_damage`]); no other skill changes.
pub fn apply_skill_strike(
    skills: &mut [RotationSkill],
    scoped: &[crate::combat::SkillScopedStrike],
) {
    for scope in scoped {
        for skill in skills
            .iter_mut()
            .filter(|s| scope.skill_ids.contains(&s.skill_id))
        {
            for effect in &mut skill.effects {
                if let SkillEffect::StrikeDamage { dmg_multiplier, .. } = effect {
                    *dmg_multiplier *= scope.factor;
                }
            }
        }
    }
}

/// Bar skills whose facts list a status with three or more different
/// values ([`unresolved_alternatives`]), as `"<skill>: <status>
/// alternatives"` for the gap line: those statuses apply nothing. Then
/// single-hit strikes of a skill that publishes more impacts
/// ([`unmodelled_impacts`]), as `"<skill>: <label> impacts"`.
pub fn unresolved_alternative_names(skills: &[RotationSkill], db: &GameDb) -> Vec<String> {
    skills
        .iter()
        .filter_map(|s| db.skills.get(&s.skill_id))
        .flat_map(|skill| {
            let alternatives = unresolved_alternatives(&skill.facts)
                .into_iter()
                .map(move |status| format!("{}: {status} alternatives", skill.name));
            let impacts = unmodelled_impacts(&skill.facts)
                .into_iter()
                .map(move |label| format!("{}: {label} impacts", skill.name));
            alternatives.chain(impacts)
        })
        .collect()
}

/// Labels of single-hit `Damage` rows on a skill whose `Number of Impacts`
/// fact is larger. The impacts count the whole area (Whirling Wrath: 7
/// projectiles; the player's golem log takes about 1.75 of them per cast),
/// not the hits one foe takes, so the row stays one hit and is named.
fn unmodelled_impacts(facts: &[Fact]) -> Vec<String> {
    let impacts = facts.iter().find_map(|fact| match fact {
        Fact::Number {
            text: Some(text),
            value: Some(value),
            ..
        } if text == "Number of Impacts" => Some(*value),
        _ => None,
    });
    if !impacts.is_some_and(|n| n > 1) {
        return Vec::new();
    }
    let floors = minimum_damage_floors(facts);
    let mut labels: Vec<String> = facts
        .iter()
        .enumerate()
        .filter(|(i, fact)| matches!(fact, Fact::Damage { .. }) && !floors.contains(i))
        .map(|(_, fact)| strike(fact).0)
        .filter(|(_, hits)| *hits == 1)
        .map(|(label, _)| label.to_string())
        .collect();
    labels.dedup();
    labels
}

/// (prefix skill, status) of one application; the prefix is empty for a `Buff`.
type ApplicationKey<'a> = (&'a str, &'a str);

/// One application of a status: its key (status, and for a `PrefixedBuff`
/// the skill it is prefixed with) and its value (stacks, seconds). A bare
/// status with neither is a listing, not an application: Vine Surge names
/// the conditions it removes that way.
fn application(fact: &Fact) -> Option<(ApplicationKey<'_>, (u32, u32))> {
    let (prefix, status, apply_count, duration) = match fact {
        Fact::Buff {
            status: Some(status),
            apply_count,
            duration,
            ..
        } => ("", status, apply_count, duration),
        Fact::PrefixedBuff {
            prefix,
            status: Some(status),
            apply_count,
            duration,
            ..
        } => (
            prefix
                .as_ref()
                .and_then(|p| p.status.as_deref())
                .unwrap_or(""),
            status,
            apply_count,
            duration,
        ),
        _ => return None,
    };
    if apply_count.is_none() && duration.is_none() {
        return None;
    }
    Some((
        (prefix, status.as_str()),
        (apply_count.unwrap_or(1), duration.unwrap_or(0)),
    ))
}

/// For each status the facts apply, the indices of its distinct values in
/// first-seen order, and the indices of every fact applying it.
type ApplicationGroup<'a> = (ApplicationKey<'a>, Vec<usize>, Vec<usize>);

fn application_groups(facts: &[Fact]) -> Vec<ApplicationGroup<'_>> {
    let mut groups: Vec<ApplicationGroup<'_>> = Vec::new();
    for (idx, fact) in facts.iter().enumerate() {
        let Some((key, value)) = application(fact) else {
            continue;
        };
        let group = match groups.iter().position(|(k, _, _)| *k == key) {
            Some(g) => &mut groups[g],
            None => {
                groups.push((key, Vec::new(), Vec::new()));
                groups.last_mut().expect("just pushed")
            }
        };
        if !group
            .1
            .iter()
            .any(|&d| application(&facts[d]).map(|(_, v)| v) == Some(value))
        {
            group.1.push(idx);
        }
        group.2.push(idx);
    }
    groups
}

/// One application per status. The API lists a status more than once when
/// the wiki shows alternatives of one application, and no fact field says
/// which: a game-mode split (Glyph of Alignment's bleed, 10 s PvE / 8 s
/// WvW and PvP), a positional variant (Poison Volley 5 s, 7 s "Attack from
/// Behind"; Crossfire 3 s flanking, 2 s otherwise) or a charge tier
/// (Arcing Slice's Fury by adrenaline). Rule:
///
/// - identical duplicates are one application;
/// - two values are one application, picked as `absorb_pair` in
///   `combat::extract_damage_modifiers` picks a two-value split: the larger
///   (stacks x seconds) in PvE, where golems are defiant and the flanking
///   value applies, the smaller in PvP and WvW;
/// - three or more values cannot be told apart: the status abstains (no
///   application) and is named by [`unresolved_alternatives`].
///
/// `Damage` rows follow the same rule per strike (see [`damage_groups`]):
/// Effulgent Stance lists Minimum 0.5 and Maximum 4.0 / 2.1, one burst per
/// cast, and landed all three (6.6) before.
fn select_alternatives(facts: &[Fact], ctx: &BalanceContext) -> Vec<Fact> {
    let competitive = matches!(
        ctx.game_mode,
        gw2_core::types::GameMode::PvP | gw2_core::types::GameMode::WvW
    );
    let size =
        |i: usize| application(&facts[i]).map_or(0, |(_, (n, s))| u64::from(n) * u64::from(s));
    let mut keep = vec![true; facts.len()];
    for i in minimum_damage_floors(facts) {
        keep[i] = false;
    }
    for (_, distinct, members) in damage_groups(facts) {
        let chosen = match distinct.as_slice() {
            [only] => Some(*only),
            [a, b] => {
                let (small, large) = if strike(&facts[*b]).1 < strike(&facts[*a]).1 {
                    (*b, *a)
                } else {
                    (*a, *b)
                };
                Some(if competitive { small } else { large })
            }
            _ => None,
        };
        for i in members {
            keep[i] = Some(i) == chosen;
        }
    }
    for (_, distinct, members) in application_groups(facts) {
        let chosen = match distinct.as_slice() {
            [only] => Some(*only),
            [a, b] => {
                let (small, large) = if size(*b) < size(*a) {
                    (*b, *a)
                } else {
                    (*a, *b)
                };
                Some(if competitive { small } else { large })
            }
            _ => None,
        };
        for i in members {
            keep[i] = Some(i) == chosen;
        }
    }
    facts
        .iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(fact, _)| fact.clone())
        .collect()
}

/// Statuses whose alternatives [`select_alternatives`] cannot pick between
/// (three or more different values), which therefore apply nothing.
pub fn unresolved_alternatives(facts: &[Fact]) -> Vec<String> {
    let statuses = application_groups(facts)
        .into_iter()
        .filter(|(_, distinct, _)| distinct.len() > 2)
        .map(|((_, status), _, _)| status.to_string());
    let strikes = damage_groups(facts)
        .into_iter()
        .filter(|(_, distinct, _)| distinct.len() > 2)
        .map(|((label, _), _, _)| label.to_string());
    statuses.chain(strikes).collect()
}

/// A `Damage` row's strike key (label, hit count) and coefficient; callers
/// pass `Damage` rows only (any other fact reads as an empty key and NaN).
fn strike(fact: &Fact) -> ((&str, u32), f64) {
    match fact {
        Fact::Damage {
            text,
            hit_count,
            dmg_multiplier,
            ..
        } => (
            (text.as_deref().unwrap_or("Damage"), hit_count.unwrap_or(1)),
            dmg_multiplier.unwrap_or(1.0),
        ),
        _ => (("", 0), f64::NAN),
    }
}

/// `Minimum ...` damage rows of a skill that also lists another damage row:
/// the floor of that strike (range, charge or target-count falloff:
/// Blowtorch 2.0 / Minimum 1.0, Effulgent Stance Maximum / Minimum), not a
/// strike of its own.
fn minimum_damage_floors(facts: &[Fact]) -> Vec<usize> {
    let is_minimum = |fact: &Fact| strike(fact).0 .0.starts_with("Minimum");
    let damage = |fact: &&Fact| matches!(fact, Fact::Damage { .. });
    if !facts.iter().filter(damage).any(|f| !is_minimum(f)) {
        return Vec::new();
    }
    (0..facts.len())
        .filter(|&i| damage(&&facts[i]) && is_minimum(&facts[i]))
        .collect()
}

/// A strike's key (label, hit count), its distinct-value rows and all its rows.
type StrikeGroup<'a> = ((&'a str, u32), Vec<usize>, Vec<usize>);

/// For each strike the `Damage` rows describe, keyed by label and hit
/// count, the indices of its distinct coefficients in first-seen order and
/// of every row. Rows with the same label and hit count are alternatives of
/// one strike the API lists per game mode (Rushing Justice Impact Damage
/// 1.5 PvE / 1.2 WvW, Jurisdiction 3.0 / 0.01) or duplicates of it; a
/// different label (Explosion, Impact, Symbol Damage) or hit count is a
/// separate strike, and a multi-hit row keeps its hit count. `Minimum`
/// floors are left out ([`minimum_damage_floors`]).
fn damage_groups(facts: &[Fact]) -> Vec<StrikeGroup<'_>> {
    let floors = minimum_damage_floors(facts);
    let mut groups: Vec<StrikeGroup<'_>> = Vec::new();
    for (idx, fact) in facts.iter().enumerate() {
        if !matches!(fact, Fact::Damage { .. }) || floors.contains(&idx) {
            continue;
        }
        let (key, value) = strike(fact);
        let group = match groups.iter().position(|(k, _, _)| *k == key) {
            Some(g) => &mut groups[g],
            None => {
                groups.push((key, Vec::new(), Vec::new()));
                groups.last_mut().expect("just pushed")
            }
        };
        if !group.1.iter().any(|&d| strike(&facts[d]).1 == value) {
            group.1.push(idx);
        }
        group.2.push(idx);
    }
    groups
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
            // A flip is reached by pressing its parent, never the slot's own
            // press: a form entry's exit (Release Celestial Avatar 31411
            // sorted ahead of Celestial Avatar 31869 by id), and a
            // differently named flip of a skill of the same specialization
            // -- the effect a virtue leaves (Willbender Flames 62618 behind
            // Rushing Justice 62668), an exit (Exit Radiant Forge), a
            // follow-up. A same-name flip is the same press under a second
            // id (Legendary Renegade Stance), and a core skill's flip to an
            // elite one (Virtue of Justice to Spear of Justice) is the elite
            // replacement, so both stay candidates.
            let exits: Vec<u32> = skills
                .iter()
                .filter_map(|skill| {
                    let flip = skills.iter().find(|s| Some(s.id) == skill.flip_skill)?;
                    let effect =
                        flip.name != skill.name && flip.specialization == skill.specialization;
                    (!skill.transform_skills.is_empty() || effect).then_some(flip.id)
                })
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
    let facts = &select_alternatives(facts, ctx);
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
    if description_invents_barrier(&d, effects) {
        // The public skill endpoint commonly omits barrier coefficients.
        // Amount stays the invented 1000; prepare stamps coverage.heuristic (W151).
        effects.push(SkillEffect::Barrier { amount: 1_000.0 });
    }
    if description_invents_healing(&d, effects) {
        effects.push(SkillEffect::Healing { hit_count: 1 });
    }
}

fn description_invents_barrier(d: &str, effects: &[SkillEffect]) -> bool {
    d.contains("barrier")
        && !effects
            .iter()
            .any(|effect| matches!(effect, SkillEffect::Barrier { .. }))
}

fn description_invents_healing(d: &str, effects: &[SkillEffect]) -> bool {
    (d.contains("heal yourself") || d.contains("heals you"))
        && !effects
            .iter()
            .any(|effect| matches!(effect, SkillEffect::Healing { .. }))
}

/// Skills whose Barrier{1000} or text-fallback Healing{1} came from
/// [`push_description_effects`]. Reconstructs from the same predicates
/// plus the heal-fact guard so a `Fact::Heal { hit_count: 1 }` is not
/// flagged. // ponytail: Barrier{1000} is only invented here; a sourced
/// 1000-point fact would need a Fact arm first.
pub(crate) fn heuristic_coverage_stamps(
    skills: &[RotationSkill],
    db: &GameDb,
    equipped_traits: &[u32],
) -> Vec<String> {
    let mut out = Vec::new();
    for skill in skills {
        let Some(api) = db.skills.get(&skill.skill_id) else {
            continue;
        };
        let desc = api.description.as_deref().unwrap_or("");
        if desc.is_empty() {
            continue;
        }
        let d = desc.to_lowercase();
        let facts = active_skill_facts(api, equipped_traits, db);
        let facts_have_heal = facts
            .iter()
            .any(|f| matches!(f, Fact::Heal { .. } | Fact::HealingAdjust { .. }));
        if d.contains("barrier")
            && skill.effects.iter().any(|effect| {
                matches!(effect, SkillEffect::Barrier { amount } if (*amount - 1_000.0).abs() < 1e-9)
            })
        {
            out.push(crate::data::quality::heuristic_entry(&skill.name, "Barrier").rendered());
        }
        if !facts_have_heal
            && (d.contains("heal yourself") || d.contains("heals you"))
            && skill
                .effects
                .iter()
                .any(|effect| matches!(effect, SkillEffect::Healing { hit_count: 1 }))
        {
            out.push(crate::data::quality::heuristic_entry(&skill.name, "Healing").rendered());
        }
    }
    out.sort();
    out.dedup();
    out
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
    fn description_fallback_barrier_and_heal_keep_invented_amounts() {
        let effects = extract_effects(&[], Some("Grant barrier and heals you."));
        assert!(
            effects.iter().any(
                |e| matches!(e, SkillEffect::Barrier { amount } if (*amount - 1_000.0).abs() < 1e-9)
            ),
            "{effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, SkillEffect::Healing { hit_count: 1 })),
            "{effects:?}"
        );
        let no_double = extract_effects(
            &[Fact::Heal {
                text: None,
                icon: None,
                hit_count: Some(3),
            }],
            Some("heals you"),
        );
        assert_eq!(
            no_double
                .iter()
                .filter(|e| matches!(e, SkillEffect::Healing { .. }))
                .count(),
            1,
            "fact heal wins; description must not add a second Healing: {no_double:?}"
        );
    }

    #[test]
    fn heuristic_coverage_stamps_barrier_and_text_heal_not_fact_heal() {
        let mut barrier = make_test_skill(1, "Troll Unguent", "Heal", vec![]);
        barrier.description = Some("Grant barrier to yourself.".into());
        let mut text_heal = make_test_skill(2, "Mending", "Heal", vec![]);
        text_heal.description = Some("heals you for a small amount.".into());
        let mut fact_heal = make_test_skill(
            3,
            "Signet of Vampirism",
            "Heal",
            vec![Fact::Heal {
                text: None,
                icon: None,
                hit_count: Some(1),
            }],
        );
        fact_heal.description = Some("heals you.".into());
        let mut db = empty_db();
        for skill in [barrier, text_heal, fact_heal] {
            db.skills.insert(skill.id, skill);
        }
        let skills = build_rotation_skills(&[1, 2, 3], &db);
        let stamps = heuristic_coverage_stamps(&skills, &db, &[]);
        assert!(
            stamps
                .iter()
                .any(|s| s == "Troll Unguent (heuristic Barrier)"),
            "{stamps:?}"
        );
        assert!(
            stamps.iter().any(|s| s == "Mending (heuristic Healing)"),
            "{stamps:?}"
        );
        assert!(
            !stamps.iter().any(|s| s.contains("Signet of Vampirism")),
            "Fact::Heal must not be stamped heuristic: {stamps:?}"
        );
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

/// The facts a bar skill applies: traited facts for the equipped traits,
/// one application per status, the out-of-form variant of a form glyph.
/// Fixtures are the live API JSON (skill cache, build 207318), icons dropped.
#[cfg(test)]
mod fact_selection_tests {
    use super::*;
    use gw2_core::types::GameMode;

    const CROSSFIRE: &str = r#"{"id": 12470, "name": "Crossfire", "slot": "Weapon_1", "facts": [{"type": "Range", "text": "Range", "value": 900}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.5}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 3, "status": "Bleeding", "apply_count": 1}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 2, "status": "Bleeding", "apply_count": 1}, {"type": "ComboFinisher", "text": "Combo Finisher", "finisher_type": "Projectile", "percent": 20}], "traited_facts": [{"requires_trait": 1912, "overrides": 2, "type": "Buff", "text": "Apply Buff/Condition", "duration": 5, "status": "Bleeding", "apply_count": 1}]}"#;
    const POISON_VOLLEY: &str = r#"{"id": 12468, "name": "Poison Volley", "slot": "Weapon_2", "facts": [{"type": "Range", "text": "Range", "value": 900}, {"type": "Recharge", "text": "Recharge", "value": 8.0}, {"type": "Damage", "text": "Damage", "hit_count": 5, "dmg_multiplier": 0.3}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 5, "status": "Poisoned", "apply_count": 5}, {"type": "NoData", "text": "Pierces"}, {"type": "Number", "text": "Targets per Arrow", "value": 5}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 7, "status": "Poisoned", "apply_count": 5}], "traited_facts": [{"requires_trait": 1912, "overrides": 6, "type": "Buff", "text": "Apply Buff/Condition", "duration": 9, "status": "Poisoned", "apply_count": 5}]}"#;
    const NATURAL_CONVERGENCE: &str = r#"{"id": 31503, "name": "Natural Convergence", "slot": "Weapon_5", "facts": [{"type": "Recharge", "text": "Recharge", "value": 10.0}, {"type": "Damage", "text": "Pulse Damage", "hit_count": 1, "dmg_multiplier": 0.75}, {"type": "Damage", "text": "Final Damage", "hit_count": 1, "dmg_multiplier": 2.0}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 10, "status": "Might", "apply_count": 1}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 1, "status": "Crippled", "apply_count": 1}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 1, "status": "Slow", "apply_count": 1}, {"type": "Number", "text": "Pulses", "value": 4}, {"type": "Distance", "text": "Radius", "distance": 360}, {"type": "Number", "text": "Number of Targets", "value": 5}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 2, "status": "Immobile", "apply_count": 4}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 2, "status": "Stability", "apply_count": 2}], "traited_facts": [{"requires_trait": 2055, "overrides": null, "type": "Buff", "text": "Apply Buff/Condition", "duration": 5, "status": "Burning", "apply_count": 1}, {"requires_trait": 2055, "overrides": null, "type": "Buff", "text": "Apply Buff/Condition", "duration": 5, "status": "Burning", "apply_count": 3}]}"#;
    const ARCING_SLICE: &str = r#"{"id": 14375, "name": "Arcing Slice", "slot": "Profession_1", "facts": [{"type": "Buff", "text": "Apply Buff/Condition", "duration": 8, "status": "Fury", "apply_count": 1}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 12, "status": "Fury", "apply_count": 1}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 16, "status": "Fury", "apply_count": 1}]}"#;
    const GLYPH_PALETTE: &str = r#"{"id": 31322, "name": "Glyph of Alignment", "slot": "Utility", "specialization": 5, "facts": [{"type": "Recharge", "text": "Recharge", "value": 20.0}, {"type": "Number", "text": "Number of Targets", "value": 5}, {"type": "Distance", "text": "Radius", "distance": 300}]}"#;
    const GLYPH_OUT_OF_FORM: &str = r#"{"id": 31607, "name": "Glyph of Alignment", "slot": "Utility", "specialization": 5, "facts": [{"type": "Recharge", "text": "Recharge", "value": 20.0}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.5}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 10, "status": "Bleeding", "apply_count": 3}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 8, "status": "Bleeding", "apply_count": 3}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 2, "status": "Immobile", "apply_count": 1}, {"type": "Buff", "text": "Apply Buff/Condition", "duration": 5, "status": "Weakness", "apply_count": 1}, {"type": "Number", "text": "Number of Targets", "value": 5}, {"type": "Distance", "text": "Radius", "distance": 300}]}"#;

    fn db_with(skills: &[&str]) -> GameDb {
        let mut db = GameDb::empty_for_tests();
        for json in skills {
            let skill: Skill = serde_json::from_str(json).expect("fixture parses");
            db.skills.insert(skill.id, skill);
        }
        db
    }

    /// The bar skill as the engine prepares it: built, then the equipped
    /// traits' facts applied.
    fn bar_skill(db: &GameDb, id: u32, mode: GameMode, traits: &[u32]) -> RotationSkill {
        let ctx = BalanceContext::new(mode);
        let mut skills = build_rotation_skills_for_context(&[id], db, &ctx);
        apply_traited_facts(&mut skills, db, &ctx, traits);
        skills.pop().expect("skill on the bar")
    }

    /// (stacks, duration_ms) of every application of `status`.
    fn applications(skill: &RotationSkill, status: &str) -> Vec<(u32, u32)> {
        skill
            .effects
            .iter()
            .filter_map(|e| match e {
                SkillEffect::ApplyCondition {
                    condition,
                    stacks,
                    duration_ms,
                } if condition == status => Some((*stacks, *duration_ms)),
                SkillEffect::ApplyBuff {
                    buff,
                    stacks,
                    duration_ms,
                } if buff == status => Some((*stacks, *duration_ms)),
                _ => None,
            })
            .collect()
    }

    /// Wiki Crossfire: bleeding 3 s when flanking or against a defiant foe,
    /// 2 s otherwise (PvE). Light on your Feet overrides the flanking fact
    /// (API `overrides: 2`) with 5 s. One application per cast.
    #[test]
    fn light_on_your_feet_lengthens_crossfire_bleed() {
        let db = db_with(&[CROSSFIRE]);
        let plain = bar_skill(&db, 12470, GameMode::PvE, &[]);
        assert_eq!(applications(&plain, "Bleeding"), vec![(1, 3_000)]);
        let traited = bar_skill(&db, 12470, GameMode::PvE, &[1912]);
        assert_eq!(applications(&traited, "Bleeding"), vec![(1, 5_000)]);
    }

    /// Wiki Eclipse (trait 2055): Natural Convergence inflicts Burning 5 s
    /// (PvE). The API carries it only as the skill's traited facts.
    #[test]
    fn eclipse_makes_natural_convergence_burn() {
        let db = db_with(&[NATURAL_CONVERGENCE]);
        let plain = bar_skill(&db, 31503, GameMode::PvE, &[]);
        assert!(applications(&plain, "Burning").is_empty());
        let traited = bar_skill(&db, 31503, GameMode::PvE, &[2055]);
        let burning = applications(&traited, "Burning");
        assert_eq!(burning.len(), 1, "one Burning application: {burning:?}");
        assert_eq!(burning[0].1, 5_000);
    }

    /// Wiki Poison Volley: poison 5 stacks, 5 s from the front or 7 s
    /// "Attack from Behind" (flanking, or a defiant foe). The two facts are
    /// alternatives of one application, never 10 stacks.
    #[test]
    fn poison_volley_applies_five_stacks() {
        let db = db_with(&[POISON_VOLLEY]);
        let pve = bar_skill(&db, 12468, GameMode::PvE, &[]);
        assert_eq!(applications(&pve, "Poisoned"), vec![(5, 7_000)]);
        let wvw = bar_skill(&db, 12468, GameMode::WvW, &[]);
        assert_eq!(applications(&wvw, "Poisoned"), vec![(5, 5_000)]);
        let traited = bar_skill(&db, 12468, GameMode::PvE, &[1912]);
        assert_eq!(applications(&traited, "Poisoned"), vec![(5, 9_000)]);
    }

    /// Three different values of one status (Arcing Slice's Fury by
    /// adrenaline tier) cannot be told apart from the facts: the status
    /// abstains by name instead of applying 36 s of Fury.
    #[test]
    fn three_way_alternatives_abstain_by_name() {
        let db = db_with(&[ARCING_SLICE]);
        let skill = bar_skill(&db, 14375, GameMode::PvE, &[]);
        assert!(applications(&skill, "Fury").is_empty());
        let facts = &db.skills[&14375].facts;
        assert_eq!(unresolved_alternatives(facts), vec!["Fury".to_string()]);
    }

    /// The abstaining status reaches the gap line by skill and status.
    #[test]
    fn three_way_alternatives_are_named_for_the_gap_line() {
        let db = db_with(&[ARCING_SLICE, POISON_VOLLEY]);
        let ctx = BalanceContext::new(GameMode::PvE);
        let skills = build_rotation_skills_for_context(&[14375, 12468], &db, &ctx);
        assert_eq!(
            unresolved_alternative_names(&skills, &db),
            vec!["Arcing Slice: Fury alternatives".to_string()]
        );
    }

    const WILLBENDER_FLAMES: &str = r#"{"id": 62618, "name": "Willbender Flames", "slot": "Profession_1", "facts": [{"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.22}, {"type": "Number", "text": "Number of Targets", "value": 3}, {"type": "Number", "text": "Number of Impacts", "value": 5}, {"type": "Time", "text": "Interval", "duration": 1}, {"type": "Time", "text": "Duration", "duration": 5}], "traited_facts": [{"requires_trait": 2190, "overrides": 0, "type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.66}]}"#;
    const POWER_FOR_POWER: &str = r#"{"id": 2190, "name": "Power for Power", "description": "Gain increased power. <c=@abilitytype>Willbender Flames</c> deal increased damage to foes they strike.", "specialization": 65, "tier": 1, "order": 1, "slot": "Major", "facts": [{"percent": 100.0, "text": "Damage Increase", "type": "Percent"}, {"target": "Power", "text": null, "type": "AttributeAdjust", "value": 120}, {"percent": 200.0, "text": "Damage Increase", "type": "Percent"}]}"#;

    fn strike(skill: &RotationSkill) -> f64 {
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
    }

    /// Wiki Power for Power: Willbender Flames damage +200% in PvE, +100%
    /// in WvW and PvP. The API scopes it through Willbender Flames'
    /// traited Damage fact. It leaves the global strike multiplier, raises
    /// the Flames once (not the traited 0.66 and the percent on top), and
    /// no other skill.
    #[test]
    fn power_for_power_raises_willbender_flames_only() {
        let mut db = db_with(&[WILLBENDER_FLAMES, CROSSFIRE]);
        let t: gw2_api::models::Trait = serde_json::from_str(POWER_FOR_POWER).expect("fixture");
        db.traits.insert(t.id, t);
        for (mode, factor) in [(GameMode::PvE, 3.0), (GameMode::WvW, 2.0)] {
            let ctx = BalanceContext::new(mode);
            let mut mods = crate::combat::extract_damage_modifiers(
                &[2190],
                None,
                &[],
                None,
                &db.traits,
                &db.items,
                &ctx,
            );
            crate::combat::scope_skill_damage(&mut mods, &db.traits, &db.skills);
            assert!(
                mods.strike_pct.is_empty(),
                "global strike: {:?}",
                mods.strike_pct
            );
            assert_eq!(mods.total_strike_mult(), 1.0);
            assert_eq!(mods.skill_strike.len(), 1);
            assert_eq!(mods.skill_strike[0].skill_ids, vec![62618]);
            assert!((mods.skill_strike[0].factor - factor).abs() < 1e-9);

            let mut skills = build_rotation_skills_for_context(&[62618, 12470], &db, &ctx);
            apply_traited_facts(&mut skills, &db, &ctx, &[2190]);
            apply_skill_strike(&mut skills, &mods.skill_strike);
            assert!((strike(&skills[0]) - 0.22 * factor).abs() < 1e-9);
            assert!(
                (strike(&skills[1]) - 0.5).abs() < 1e-9,
                "Crossfire unchanged"
            );
        }
    }

    /// Wiki Glyph of Alignment (palette 4821, id 31322) is the palette
    /// entry; out of Celestial Avatar the game casts Glyph of Alignment
    /// (non-celestial), id 31607, which bleeds (3 stacks, 10 s PvE).
    #[test]
    fn druid_bar_carries_the_out_of_form_glyph() {
        let db = db_with(&[GLYPH_PALETTE, GLYPH_OUT_OF_FORM]);
        let glyph = bar_skill(&db, 31322, GameMode::PvE, &[]);
        assert_eq!(glyph.skill_id, 31607);
        assert_eq!(applications(&glyph, "Bleeding"), vec![(3, 10_000)]);
        let wvw = bar_skill(&db, 31322, GameMode::WvW, &[]);
        assert_eq!(applications(&wvw, "Bleeding"), vec![(3, 8_000)]);
    }

    /// The Willbender's virtue slots as the API publishes them: each virtue
    /// flips to the Willbender Flames it leaves (62618, 62528), sorted ahead
    /// of it by id; Crashing Courage flips to a second id of itself (62532)
    /// and the third Flames (62552) is linked from nothing. Wiki
    /// Willbender_Flames_(Rushing_Justice): `parent = Rushing Justice`,
    /// no description type; the player's golem log has zero casts of it.
    /// A core skill's flip to an elite one (Virtue of Justice 9115 to Spear
    /// of Justice 29887) stays the elite replacement.
    #[test]
    fn a_differently_named_flip_of_the_same_spec_is_not_the_slot_press() {
        let mut db = GameDb::empty_for_tests();
        let mut ids = Vec::new();
        for (id, name, slot, spec, flip) in [
            (9115, "Virtue of Justice", "Profession_1", None, Some(29887)),
            (29887, "Spear of Justice", "Profession_1", Some(27), None),
            (
                62668,
                "Rushing Justice",
                "Profession_1",
                Some(65),
                Some(62618),
            ),
            (62618, "Willbender Flames", "Profession_1", Some(65), None),
            (
                62603,
                "Flowing Resolve",
                "Profession_2",
                Some(65),
                Some(62528),
            ),
            (62528, "Willbender Flames", "Profession_2", Some(65), None),
            (
                62648,
                "Crashing Courage",
                "Profession_3",
                Some(65),
                Some(62532),
            ),
            (62532, "Crashing Courage", "Profession_3", Some(65), None),
            (62552, "Willbender Flames", "Profession_3", Some(65), None),
        ] {
            let skill: Skill = serde_json::from_value(serde_json::json!({
                "id": id, "name": name, "slot": slot, "facts": [],
                "specialization": spec, "flip_skill": flip,
            }))
            .expect("fixture");
            db.skills.insert(id, skill);
            ids.push(id);
        }
        db.skills_by_profession.insert("Guardian".into(), ids);
        let weapons = crate::validation::ValidatedWeapons::default();
        let bar = |specs: &[u32]| -> Vec<u32> {
            profession_skills_for_build(&db, "Guardian", specs, &weapons)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };
        assert_eq!(bar(&[42, 46, 65]), vec![62668, 62603, 62532]);
        assert_eq!(bar(&[42, 46, 27]), vec![29887]);
    }

    /// The player's Willbender (golem log 20260923-204811, chat code
    /// `[&DQEQPi4VQSYmDwAARwEAADYBAADYGgAAiRIAAAAAAAAAAAAAAAAAAAAAAAADNgAxADIAAA==]`):
    /// the castable bar carries the virtues and the greatsword chain, and
    /// no Willbender Flames (a zero-cooldown filler before, 26 casts a
    /// minute that the log does not have).
    #[test]
    #[ignore = "reads the live skill cache"]
    fn player_willbender_bar_has_the_virtues_and_greatsword_chain_not_the_flames() {
        let cache = gw2_api::cache::DataCache::new(
            gw2_api::dev_config::cache_dir().expect("dev.cfg cache dir"),
        );
        let db = crate::gamedb::GameDb::load(&cache).expect("cached game data");
        let plate = serde_json::json!({
            "specializations": [
                {"name": "Radiance", "traits": ["Right-Hand Strength", "Retribution", "Righteous Instincts"]},
                {"name": "Virtues", "traits": ["Unscathed Contender", "Inspiring Virtue", "Permeating Wrath"]},
                {"name": "Willbender", "traits": ["Power for Power", "Restorative Virtues", "Tyrant's Momentum"]},
            ],
            "weapons": {"set1": {"main": "Greatsword", "off": null}, "set2": {"main": "Sword", "off": "Focus"}},
            "skills": {
                "heal": "\"Receive the Light!\"",
                "utilities": ["Judge's Intervention", "Whirling Light", "\"Stand Your Ground!\""],
                "elite": "\"Feel My Wrath!\"",
            },
            "rune": "Superior Rune of the Scholar",
            "sigils": ["Superior Sigil of Force", "Superior Sigil of Fire"],
            "relic": "Relic of the Thief",
            "stat_prefix": "Berserker",
            "explanation": "player build",
        });
        let parsed = crate::prompts::parse_gemini_build(&plate.to_string()).expect("plate");
        let validated = crate::validation::validate_gemini_build(&parsed, &db, "Guardian");
        assert!(validated.errors.is_empty(), "{:?}", validated.errors);
        let ctx = BalanceContext::pve();
        let (stats, _) =
            crate::engine::calculate_validated_stats(&validated, &db, "Guardian", &ctx);
        let prepared = crate::engine::prepare_validated_rotation(&validated, &db, &stats, None)
            .expect("prepares");
        let bar: Vec<u32> = prepared.skills.iter().map(|s| s.skill_id).collect();
        for id in [62668, 62603, 9137, 9138, 9139] {
            assert!(bar.contains(&id), "{id} missing from {bar:?}");
        }
        for id in [62618, 62528, 62552] {
            assert!(!bar.contains(&id), "Willbender Flames {id} on {bar:?}");
        }
    }

    fn damage(text: &str, hit_count: u32, dmg_multiplier: f64) -> Fact {
        Fact::Damage {
            text: Some(text.into()),
            icon: None,
            hit_count: Some(hit_count),
            dmg_multiplier: Some(dmg_multiplier),
        }
    }

    fn strikes(facts: &[Fact], mode: GameMode) -> Vec<(u32, f64)> {
        extract_effects_for_context(0, facts, None, &BalanceContext::new(mode))
            .into_iter()
            .filter_map(|e| match e {
                SkillEffect::StrikeDamage {
                    hit_count,
                    dmg_multiplier,
                } => Some((hit_count, dmg_multiplier)),
                _ => None,
            })
            .collect()
    }

    /// Wiki Effulgent_Stance (read 2026-09-24): one burst of light per cast,
    /// maximum-strength coefficient 4.0 in PvE and 2.1 in WvW/PvP, 0.5 when
    /// not fully charged. The API lists Minimum 0.5, Maximum 4.0 and Maximum
    /// 2.1, and all three landed (6.6 per cast).
    #[test]
    fn effulgent_stance_lands_one_burst_per_cast() {
        let facts = [
            damage("Minimum Damage", 1, 0.5),
            damage("Maximum Damage", 1, 4.0),
            damage("Maximum Damage", 1, 2.1),
        ];
        assert_eq!(strikes(&facts, GameMode::PvE), vec![(1, 4.0)]);
        assert_eq!(strikes(&facts, GameMode::WvW), vec![(1, 2.1)]);
    }

    /// Wiki Unload: 8 strikes of 0.42 (3.36); the API lists the 8 x 0.42 row
    /// twice. Wiki Rushing_Justice: Impact Damage 1.5 PvE, 1.2 WvW/PvP.
    /// Rows with another label are other strikes (Whirling Wrath's spin and
    /// projectiles), and a three-value row abstains by name.
    #[test]
    fn damage_rows_of_one_strike_are_alternatives() {
        let unload = [damage("Damage", 8, 0.42), damage("Damage", 8, 0.42)];
        assert_eq!(strikes(&unload, GameMode::PvE), vec![(8, 0.42)]);
        let rushing = [
            damage("Impact Damage", 1, 1.5),
            damage("Impact Damage", 1, 1.2),
        ];
        assert_eq!(strikes(&rushing, GameMode::PvE), vec![(1, 1.5)]);
        assert_eq!(strikes(&rushing, GameMode::WvW), vec![(1, 1.2)]);
        let wrath = [
            damage("Damage", 7, 0.35),
            damage("Projectile Damage", 1, 0.275),
        ];
        assert_eq!(strikes(&wrath, GameMode::PvE), vec![(7, 0.35), (1, 0.275)]);
        let sword = [
            damage("Damage", 4, 0.72),
            damage("Damage", 4, 0.8),
            damage("Damage", 4, 0.45),
        ];
        assert!(strikes(&sword, GameMode::PvE).is_empty());
        assert_eq!(unresolved_alternatives(&sword), vec!["Damage".to_string()]);
    }

    /// Wiki Whirling_Wrath: 7 projectiles ("Number of Impacts: 7"); the API
    /// row is one hit, and the player's golem log takes about 1.75 per cast.
    /// The impacts count the area, so the row stays one hit and is named.
    #[test]
    fn area_impacts_are_named_not_multiplied() {
        let facts = [
            damage("Damage", 7, 0.35),
            damage("Projectile Damage", 1, 0.275),
            Fact::Number {
                text: Some("Number of Impacts".into()),
                icon: None,
                value: Some(7),
            },
        ];
        assert_eq!(
            unmodelled_impacts(&facts),
            vec!["Projectile Damage".to_string()]
        );
        assert_eq!(strikes(&facts, GameMode::PvE), vec![(7, 0.35), (1, 0.275)]);
    }

    const IMPOSSIBLE_ODDS: &str = r#"{"id": 27107, "name": "Impossible Odds", "slot": "Utility", "facts": [{"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.45}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.65}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.55}]}"#;
    const PHANTOMS_ONSLAUGHT: &str = r#"{"id": 62895, "name": "Phantom's Onslaught", "slot": "Weapon_3", "facts": [{"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 1.6}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 1.33}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 1.18}]}"#;
    const SPLINTER_WEAPON: &str = r#"{"id": 76975, "name": "Splinter Weapon", "slot": "Utility", "facts": [{"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.4}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.25}, {"type": "Damage", "text": "Damage", "hit_count": 1, "dmg_multiplier": 0.5}]}"#;
    const SWORD_OF_JUSTICE: &str = r#"{"id": 9168, "name": "Sword of Justice", "slot": "Utility", "facts": [{"type": "Damage", "text": "Damage", "hit_count": 4, "dmg_multiplier": 0.72}, {"type": "Damage", "text": "Damage", "hit_count": 4, "dmg_multiplier": 0.8}, {"type": "Damage", "text": "Damage", "hit_count": 4, "dmg_multiplier": 0.45}]}"#;

    /// Wiki Impossible Odds: per-hit follow-up strike coefficient 0.65 PvE,
    /// 0.55 WvW, 0.45 PvP (`https://wiki.guildwars2.com/wiki/Impossible_Odds`).
    /// The API lists the three as unlabelled one-hit Damage rows the builder
    /// cannot tell apart; `damage_coefficient:above_50` overrides land the
    /// per-mode value the same way a health-threshold skill does.
    #[test]
    fn impossible_odds_lands_the_wiki_coefficient_per_mode() {
        let db = db_with(&[IMPOSSIBLE_ODDS]);
        assert!((strike(&bar_skill(&db, 27107, GameMode::PvE, &[])) - 0.65).abs() < 1e-9);
        assert!((strike(&bar_skill(&db, 27107, GameMode::WvW, &[])) - 0.55).abs() < 1e-9);
        assert!((strike(&bar_skill(&db, 27107, GameMode::PvP, &[])) - 0.45).abs() < 1e-9);
    }

    /// Wiki Phantom's Onslaught: coefficient 1.6 PvE, 1.33 WvW, 1.18 PvP
    /// (`https://wiki.guildwars2.com/wiki/Phantom's_Onslaught`).
    #[test]
    fn phantoms_onslaught_lands_the_wiki_coefficient_per_mode() {
        let db = db_with(&[PHANTOMS_ONSLAUGHT]);
        assert!((strike(&bar_skill(&db, 62895, GameMode::PvE, &[])) - 1.6).abs() < 1e-9);
        assert!((strike(&bar_skill(&db, 62895, GameMode::WvW, &[])) - 1.33).abs() < 1e-9);
        assert!((strike(&bar_skill(&db, 62895, GameMode::PvP, &[])) - 1.18).abs() < 1e-9);
    }

    /// Wiki Splinter Weapon: coefficient 0.4 PvE, 0.25 WvW, 0.5 PvP
    /// (`https://wiki.guildwars2.com/wiki/Splinter_Weapon`).
    #[test]
    fn splinter_weapon_lands_the_wiki_coefficient_per_mode() {
        let db = db_with(&[SPLINTER_WEAPON]);
        assert!((strike(&bar_skill(&db, 76975, GameMode::PvE, &[])) - 0.4).abs() < 1e-9);
        assert!((strike(&bar_skill(&db, 76975, GameMode::WvW, &[])) - 0.25).abs() < 1e-9);
        assert!((strike(&bar_skill(&db, 76975, GameMode::PvP, &[])) - 0.5).abs() < 1e-9);
    }

    /// Wiki Sword of Justice hits 4 times per cast at a per-strike
    /// coefficient (0.8 PvE / 0.72 PvP / 0.45 WvW,
    /// `https://wiki.guildwars2.com/wiki/Sword_of_Justice`), but the override
    /// format has no field for a strike's hit count -- only
    /// `damage_coefficient:*`, which lands a single strike. Sourcing the
    /// coefficient alone would silently drop the summon from 4 hits to 1, so
    /// this skill is left abstaining (0 damage) until the format grows a
    /// hit-count field; documented here rather than "fixed" wrong.
    #[test]
    fn sword_of_justice_still_abstains_hit_count_not_expressible() {
        let db = db_with(&[SWORD_OF_JUSTICE]);
        assert_eq!(strike(&bar_skill(&db, 9168, GameMode::PvE, &[])), 0.0);
        let facts = &db.skills[&9168].facts;
        assert_eq!(unresolved_alternatives(facts), vec!["Damage".to_string()]);
    }
}
