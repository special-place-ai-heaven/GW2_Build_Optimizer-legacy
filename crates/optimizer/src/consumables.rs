//! Phase 2 PR-C: food (Nourishment) and utility (Enhancement) consumables.
//!
//! Ownership matches rune/relic: [`ValidatedBuild::food`] / [`ValidatedBuild::utility`]
//! are `Option<ValidatedItem>`. The catalog lives on [`GameDb`] (`nourishments_for` /
//! `enhancements_for`) and is filtered by `game_types` for the active mode. There is
//! no new service and no search-neighbor / beam dimension.
//!
//! Static standing stats and proven static percent multipliers fold into
//! [`crate::engine::calculate_validated_stats`]. Timing, chance, on-hit, on-crit,
//! health-gated, and other ordered/sim-state effects are catalogued as skipped
//! (see [`NON_STATIC_SKIPPED`]) and never enter the stat sheet.
//!
//! After a kit is complete, [`assign_best_consumables`] is the cheap inner argmax
//! on the evaluate path. Chosen ids are written onto the candidate so the winner
//! serializes them. Locked food/utility are never overwritten.

use gw2_api::models::Item;
use gw2_core::types::{BuildLocks, GameMode};

use crate::balance::BalanceContext;
use crate::combat::{self, DamageModifiers};
use crate::engine;
use crate::gamedb::GameDb;
use crate::scenario::ScenarioSpec;
use crate::scoring::{self, OptimizationWeights};
use crate::stats::StatBlock;
use crate::validation::{ValidatedBuild, ValidatedItem};

/// Non-static catalogued effects this phase documents and skips.
///
/// These appear on real Nourishment / Enhancement tooltips. They are not
/// standing attribute or unconditional percent bonuses, so they are not
/// folded into `calculate_validated_stats` and they do not move the inner
/// argmax except by being absent from the cheap score.
pub const NON_STATIC_SKIPPED: &[&str] = &[
    "chance-based procs (N percent chance to ...)",
    "on-crit / on-dodge / on-kill / on-weapon-swap / when-you / after-you triggers",
    "health-gated bonuses (below / while under / above a health threshold)",
    "duration-limited bursts that are not a standing Hero-panel bonus",
    "combo-field, finisher, leap, and other ordered/sim-state effects",
    "infusions (Phase 2 infusion reconnect is out of PR-C scope)",
];

/// Standing Nourishment / Enhancement rows the catalog will consider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumableKind {
    Nourishment,
    Enhancement,
}

/// Is this item a Nourishment (food) or Enhancement (utility consumable)?
pub fn kind_of(item: &Item) -> Option<ConsumableKind> {
    if item.item_type != "Consumable" {
        return None;
    }
    let detail = item
        .details
        .as_ref()
        .and_then(|d| d.detail_type.as_deref())?;
    if eq_ignore(detail, "Food") || eq_ignore(detail, "Nourishment") {
        Some(ConsumableKind::Nourishment)
    } else if eq_ignore(detail, "Utility") || eq_ignore(detail, "Enhancement") {
        Some(ConsumableKind::Enhancement)
    } else {
        None
    }
}

/// `game_types` legality for the active mode. Empty `game_types` (test fixtures)
/// is legal in every mode; live API rows always carry the list.
pub fn item_legal_for_mode(item: &Item, mode: &GameMode) -> bool {
    if item.game_types.is_empty() {
        return true;
    }
    let token = match mode {
        GameMode::PvE => "pve",
        GameMode::PvP => "pvp",
        GameMode::WvW => "wvw",
    };
    item.game_types
        .iter()
        .any(|entry| entry.eq_ignore_ascii_case(token))
}

fn eq_ignore(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Standing flat attributes from infix rows and `+N Attribute` prose.
///
/// Triggered / chance / health-gated sentences are ignored. Percent clauses
/// are not applied here — see [`fold_static_modifiers`].
pub fn static_stat_bonus(item: &Item) -> StatBlock {
    let mut stats = StatBlock::default();
    if let Some(details) = item.details.as_ref() {
        if let Some(infix) = details.infix_upgrade.as_ref() {
            for attr in &infix.attributes {
                apply_named_stat(&mut stats, &attr.attribute, attr.modifier as f64);
            }
            if let Some(desc) = infix.buff.as_ref().and_then(|b| b.description.as_deref()) {
                apply_stat_prose(&mut stats, desc);
            }
        }
    }
    if let Some(desc) = item.description.as_deref() {
        apply_stat_prose(&mut stats, desc);
    }
    for line in detail_lines(item) {
        if !consumable_text_is_non_static(line) {
            apply_stat_prose(&mut stats, line);
        }
    }
    stats
}

/// `details.description` of a Food / Utility row, one tooltip line each
/// ("+100 Power", "+10% Experience from Kills").
pub fn detail_lines(item: &Item) -> impl Iterator<Item = &str> {
    item.details
        .as_ref()
        .and_then(|d| d.description.as_deref())
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

/// Reward-only lines (experience, magic find, karma, gold): no combat effect,
/// so neither applied nor reported as a gap.
fn line_is_non_combat(line: &str) -> bool {
    let l = line.to_lowercase();
    ["experience", "magic find", "karma", "gold"]
        .iter()
        .any(|w| l.contains(w))
}

/// A standing stat-to-stat conversion: `target += source * fraction`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatConversion {
    pub target: &'static str,
    pub fraction: f64,
    pub source: &'static str,
}

/// "Gain Power Equal to 3% of Your Precision" -> Power += 0.03 * Precision.
pub fn parse_conversion(line: &str) -> Option<StatConversion> {
    let l = line.trim().trim_end_matches('.').to_lowercase();
    let rest = l.strip_prefix("gain ")?;
    let (target, rest) = rest.split_once(" equal to ")?;
    let (pct, source) = rest.split_once("% of your ")?;
    Some(StatConversion {
        target: attr_ci(target)?,
        fraction: pct.trim().parse::<f64>().ok()? / 100.0,
        source: attr_ci(source)?,
    })
}

/// Case-insensitive attribute name -> `StatBlock` key.
fn attr_ci(name: &str) -> Option<&'static str> {
    match name.trim() {
        "power" => Some("Power"),
        "precision" => Some("Precision"),
        "toughness" => Some("Toughness"),
        "vitality" => Some("Vitality"),
        "ferocity" => Some("Ferocity"),
        "condition damage" => Some("ConditionDamage"),
        "expertise" => Some("Expertise"),
        "concentration" => Some("Concentration"),
        "healing power" => Some("Healing"),
        _ => None,
    }
}

/// Every conversion line of `item`'s tooltip.
pub fn conversions(item: &Item) -> Vec<StatConversion> {
    detail_lines(item).filter_map(parse_conversion).collect()
}

/// Tooltip lines the stat sheet does not apply: triggered / chance / gated
/// lines and lines no parser recognises. Reward-only lines are not gaps.
pub fn unmodeled_lines(item: &Item) -> Vec<String> {
    detail_lines(item)
        .filter(|line| {
            if line_is_non_combat(line) {
                return false;
            }
            if consumable_text_is_non_static(line) {
                return true;
            }
            if parse_conversion(line).is_some() {
                return false;
            }
            let mut stats = StatBlock::default();
            apply_stat_prose(&mut stats, line);
            if !stats.is_zero() {
                return false;
            }
            let mut mods = DamageModifiers::default();
            !(line.contains('%') && combat::parse_percent_clauses(&mut mods, line))
        })
        .map(str::to_string)
        .collect()
}

fn apply_named_stat(stats: &mut StatBlock, raw: &str, value: f64) {
    let key = normalize_attr_name(raw);
    stats.add(&key, value);
}

fn normalize_attr_name(raw: &str) -> String {
    match raw.trim() {
        "Power" => "Power".into(),
        "Precision" => "Precision".into(),
        "Toughness" => "Toughness".into(),
        "Vitality" => "Vitality".into(),
        "Ferocity" | "CritDamage" => "Ferocity".into(),
        "Condition Damage" | "ConditionDamage" => "ConditionDamage".into(),
        "Expertise" | "ConditionDuration" => "Expertise".into(),
        "Concentration" | "BoonDuration" => "Concentration".into(),
        "Healing Power" | "Healing" | "HealingPower" => "Healing".into(),
        other => other.to_string(),
    }
}

/// Pull every `+N Attribute` standing bonus out of tooltip prose.
fn apply_stat_prose(stats: &mut StatBlock, text: &str) {
    let mut i = 0;
    let chars: Vec<char> = text.chars().collect();
    while i < chars.len() {
        if chars[i] == '+' || chars[i].is_ascii_digit() {
            let start = i;
            if chars[i] == '+' {
                i += 1;
            }
            let num_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i == num_start {
                i = start + 1;
                continue;
            }
            if i < chars.len() && chars[i] == '%' {
                i += 1;
                continue;
            }
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let name_start = i;
            while i < chars.len() && (chars[i].is_ascii_alphabetic() || chars[i] == ' ') {
                i += 1;
            }
            let name: String = chars[name_start..i].iter().collect();
            let name = name.trim();
            if let Some(key) = known_attr(name) {
                let num: String = chars[num_start..name_start].iter().collect();
                if let Ok(value) = num.trim().parse::<f64>() {
                    apply_named_stat(stats, key, value);
                }
            }
            continue;
        }
        i += 1;
    }
}

fn known_attr(name: &str) -> Option<&'static str> {
    match name {
        "Power" => Some("Power"),
        "Precision" => Some("Precision"),
        "Toughness" => Some("Toughness"),
        "Vitality" => Some("Vitality"),
        "Ferocity" => Some("Ferocity"),
        "Condition Damage" | "ConditionDamage" => Some("ConditionDamage"),
        "Expertise" => Some("Expertise"),
        "Concentration" => Some("Concentration"),
        "Healing Power" | "Healing" => Some("Healing"),
        _ => None,
    }
}

/// True when tooltip prose is a triggered / chance / gated effect, not a
/// standing Hero-panel bonus. Used to skip percent folding.
pub fn consumable_text_is_non_static(text: &str) -> bool {
    let t = text.to_lowercase();
    combat::upgrade_unreliable(&t)
        || t.contains("chance")
        || t.contains("when you")
        || t.contains("when the")
        || t.contains("after you")
        || t.contains("after using")
        || t.contains("upon ")
        || t.contains("on crit")
        || t.contains("on dodge")
        || t.contains("on evade")
        || t.contains("on kill")
        || t.contains("on weapon swap")
        || t.contains("below ")
        || t.contains("while under")
        || t.contains("combo")
        || t.contains("finisher")
}

/// Fold proven static percent multipliers from a consumable into `mods`.
///
/// Reuses the upgrade-text parser (same standing `+N% damage` path as runes)
/// only when the tooltip is not a non-static effect. Unparsed / triggered
/// clauses stay out of the sheet.
pub fn fold_static_modifiers(mods: &mut DamageModifiers, item: &Item) {
    let mut texts: Vec<&str> = Vec::new();
    if let Some(desc) = combat::item_buff_description(item) {
        texts.push(desc);
    }
    if let Some(desc) = item.description.as_deref() {
        texts.push(desc);
    }
    texts.extend(
        detail_lines(item).filter(|l| parse_conversion(l).is_none() && !line_is_non_combat(l)),
    );
    for text in texts {
        if consumable_text_is_non_static(text) {
            continue;
        }
        combat::apply_upgrade_text(mods, text);
    }
}

/// Standing flat, percent, or stat conversion.
///
/// Conversion-only utilities (Superior Sharpening Stone) have neither a flat
/// nor a percent, but they still move the sheet. They stay eligible for
/// [`assign_best_consumables`] (E20a). The conversion reads the pre-conversion
/// sheet, same as trait conversions (E20b).
pub fn has_static_effect(item: &Item) -> bool {
    if !static_stat_bonus(item).is_zero()
        || detail_lines(item).any(|line| parse_conversion(line).is_some())
    {
        return true;
    }
    let mut mods = DamageModifiers::default();
    fold_static_modifiers(&mut mods, item);
    !mods.strike_pct.is_empty()
        || !mods.strike_add_pct.is_empty()
        || !mods.condition_pct.is_empty()
        || !mods.condition_add_pct.is_empty()
        || !mods.crit_damage_pct.is_empty()
        || !mods.condi_duration_pct.is_empty()
        || !mods.boon_duration_pct.is_empty()
        || !mods.healing_pct.is_empty()
        || !mods.crit_chance_pct.is_empty()
        || !mods.specific_condi.is_empty()
}

fn item_to_validated(item: &Item) -> ValidatedItem {
    ValidatedItem {
        id: item.id,
        name: item.name.clone(),
    }
}

fn locked_item(db: &GameDb, id: u32) -> Option<ValidatedItem> {
    db.items.get(&id).map(item_to_validated).or_else(|| {
        Some(ValidatedItem {
            id,
            name: format!("locked-{id}"),
        })
    })
}

/// Cheap inner argmax: pick legal food x utility (or none) after the kit is
/// complete, write the ids onto `validated`, honor locks.
///
/// Scoring uses the standing numbers `calculate_validated_stats` folds
/// (flats, static percents, stat conversions) via closed-form combat /
/// realized axes. No rotation, no neighbor operator, no search_rank key change.
pub fn assign_best_consumables(
    validated: &mut ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
    locks: &BuildLocks,
) {
    if let Some(id) = locks.food {
        validated.food = locked_item(db, id);
    }
    if let Some(id) = locks.utility {
        validated.utility = locked_item(db, id);
    }

    let food_locked = locks.food.is_some();
    let util_locked = locks.utility.is_some();
    if food_locked && util_locked {
        return;
    }

    let foods = if food_locked {
        Vec::new()
    } else {
        db.nourishments_for(&ctx.game_mode)
            .into_iter()
            .filter(|item| has_static_effect(item))
            .collect::<Vec<_>>()
    };
    let utils = if util_locked {
        Vec::new()
    } else {
        db.enhancements_for(&ctx.game_mode)
            .into_iter()
            .filter(|item| has_static_effect(item))
            .collect::<Vec<_>>()
    };

    if foods.is_empty() && utils.is_empty() {
        if !food_locked {
            validated.food = None;
        }
        if !util_locked {
            validated.utility = None;
        }
        return;
    }

    // One-sided locks must pin the locked id as Some(&Item) so scoring
    // includes its contribution (None would treat the lock as zero).
    let food_choices: Vec<Option<&Item>> = if food_locked {
        vec![validated.food.as_ref().and_then(|v| db.items.get(&v.id))]
    } else {
        let mut v: Vec<Option<&Item>> = Vec::with_capacity(foods.len() + 1);
        v.push(None);
        v.extend(foods.iter().copied().map(Some));
        v
    };
    let util_choices: Vec<Option<&Item>> = if util_locked {
        vec![validated.utility.as_ref().and_then(|v| db.items.get(&v.id))]
    } else {
        let mut v: Vec<Option<&Item>> = Vec::with_capacity(utils.len() + 1);
        v.push(None);
        v.extend(utils.iter().copied().map(Some));
        v
    };

    let scoring = JointArgmaxCtx {
        db,
        profession_name,
        weights,
        ctx,
        scenario,
    };

    let product = food_choices.len().saturating_mul(util_choices.len());
    if product <= 64 {
        let (food, utility) = joint_argmax(validated, &food_choices, &util_choices, &scoring);
        if !food_locked {
            validated.food = food.map(item_to_validated);
        }
        if !util_locked {
            validated.utility = utility.map(item_to_validated);
        }
        return;
    }

    // Large catalog: independent passes stay additive and cheap.
    // Approximation: food is chosen without free utilities (or with a pinned
    // locked utility), then utility is chosen with the selected food pinned.
    // Cross terms between free food and free utility are not jointly optimized.
    if !food_locked {
        let pinned_utils = [if util_locked {
            validated.utility.as_ref().and_then(|v| db.items.get(&v.id))
        } else {
            None
        }];
        let (food, _) = joint_argmax(validated, &food_choices, &pinned_utils, &scoring);
        validated.food = food.map(item_to_validated);
    }
    if !util_locked {
        let pinned_food = [validated.food.as_ref().and_then(|v| db.items.get(&v.id))];
        let (_, utility) = joint_argmax(validated, &pinned_food, &util_choices, &scoring);
        validated.utility = utility.map(item_to_validated);
    }
}

/// Scoring inputs for the food/utility joint argmax (keeps the helper under
/// clippy's `too_many_arguments` limit without a blanket allow).
struct JointArgmaxCtx<'b> {
    db: &'b GameDb,
    profession_name: &'b str,
    weights: &'b OptimizationWeights,
    ctx: &'b BalanceContext,
    scenario: &'b ScenarioSpec,
}

fn joint_argmax<'a>(
    validated: &ValidatedBuild,
    foods: &[Option<&'a Item>],
    utils: &[Option<&'a Item>],
    scoring: &JointArgmaxCtx<'_>,
) -> (Option<&'a Item>, Option<&'a Item>) {
    let mut scratch = validated.clone();
    let mut best_key = cheap_key(
        &scratch,
        scoring.db,
        scoring.profession_name,
        scoring.weights,
        scoring.ctx,
        scoring.scenario,
    );
    let mut best: (Option<&Item>, Option<&Item>) = (None, None);
    for food in foods {
        for util in utils {
            scratch.food = food.map(item_to_validated);
            scratch.utility = util.map(item_to_validated);
            let key = cheap_key(
                &scratch,
                scoring.db,
                scoring.profession_name,
                scoring.weights,
                scoring.ctx,
                scoring.scenario,
            );
            if key > best_key {
                best_key = key;
                best = (*food, *util);
            }
        }
    }
    best
}

fn cheap_key(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> (i64, i64, i64) {
    let (stats, modifiers) = engine::calculate_validated_stats(validated, db, profession_name, ctx);
    let derived = crate::stats::compute_derived(&stats, profession_name);
    let buffs = combat::buff_profiles_for_profession(profession_name, ctx);
    let cond_w = combat::condition_weights_for_profession(profession_name, ctx);
    let idx = match scenario.combat_tier {
        crate::scenario::CombatTier::Solo => 0,
        crate::scenario::CombatTier::Party => 1,
        crate::scenario::CombatTier::Squad => 2,
    };
    let Some(buff) = buffs.get(idx).or_else(|| buffs.first()) else {
        return (0, 0, 0);
    };
    let perf = combat::calculate_combat_performance(
        &stats,
        &derived,
        &modifiers,
        buff,
        &cond_w,
        profession_name,
        ctx,
    );
    let realized = scoring::realized_axes_no_rotation(&perf);
    let user = scoring::score_realized(&realized, weights);
    let raw = scoring::raw_realized(&realized, weights);
    let stat = scoring::raw_direction_score(&perf, weights);
    (
        (user * 1_000_000.0).round() as i64,
        (raw * 1_000_000.0).round() as i64,
        (stat * 1_000_000.0).round() as i64,
    )
}

fn equipped_consumable_items<'a>(validated: &'a ValidatedBuild, db: &'a GameDb) -> Vec<&'a Item> {
    validated
        .food
        .as_ref()
        .and_then(|v| db.items.get(&v.id))
        .into_iter()
        .chain(validated.utility.as_ref().and_then(|v| db.items.get(&v.id)))
        .collect()
}

/// Standing flats and static percents. Call before trait conversions so the
/// shared pre-conversion sheet includes food and utility flats.
pub(crate) fn fold_standing_flats(
    stats: &mut StatBlock,
    mods: &mut DamageModifiers,
    validated: &ValidatedBuild,
    db: &GameDb,
) {
    for item in equipped_consumable_items(validated, db) {
        *stats += &static_stat_bonus(item);
        fold_static_modifiers(mods, item);
    }
}

/// Stat conversions from `base`, which must be the pre-conversion sheet
/// (flats in, no trait-conversion output).
pub(crate) fn fold_stat_conversions(
    stats: &mut StatBlock,
    validated: &ValidatedBuild,
    db: &GameDb,
    base: &StatBlock,
) {
    for c in equipped_consumable_items(validated, db)
        .iter()
        .flat_map(|item| conversions(item))
    {
        stats.add(c.target, base.get(c.source) * c.fraction);
    }
}

/// Apply standing consumable stats + static percents onto the validated sheet.
///
/// Conversions read `stats` after the flats in this call are added. Trait
/// conversions belong on that same sheet and must already have been excluded
/// from it; [`crate::engine::calculate_validated_stats`] snapshots before
/// `apply_trait_conversions`.
pub fn fold_into_validated_stats(
    stats: &mut StatBlock,
    mods: &mut DamageModifiers,
    validated: &ValidatedBuild,
    db: &GameDb,
) {
    fold_standing_flats(stats, mods, validated, db);
    let snapshot = stats.clone();
    fold_stat_conversions(stats, validated, db, &snapshot);
}

/// Test helper: infix-attribute consumable with explicit `game_types`.
#[cfg(test)]
pub fn test_consumable(
    id: u32,
    name: &str,
    kind: ConsumableKind,
    attrs: &[(&str, i32)],
    game_types: &[&str],
    description: Option<&str>,
) -> Item {
    use gw2_api::models::{InfixAttribute, InfixUpgrade, ItemDetails};
    let detail_type = match kind {
        ConsumableKind::Nourishment => "Food",
        ConsumableKind::Enhancement => "Utility",
    };
    Item {
        id,
        name: name.to_string(),
        description: description.map(str::to_string),
        icon: None,
        item_type: "Consumable".into(),
        rarity: "Ascended".into(),
        level: 80,
        vendor_value: None,
        chat_link: None,
        default_skin: None,
        flags: Vec::new(),
        game_types: game_types.iter().map(|s| s.to_string()).collect(),
        restrictions: Vec::new(),
        details: Some(ItemDetails {
            description: None,
            detail_type: Some(detail_type.into()),
            weight_class: None,
            defense: None,
            damage_type: None,
            min_power: None,
            max_power: None,
            suffix: None,
            bonuses: Vec::new(),
            infusion_upgrade_flags: Vec::new(),
            infusion_slots: Vec::new(),
            attribute_adjustment: None,
            infix_upgrade: Some(InfixUpgrade {
                id: None,
                attributes: attrs
                    .iter()
                    .map(|(attr, modifier)| InfixAttribute {
                        attribute: (*attr).into(),
                        modifier: *modifier,
                    })
                    .collect(),
                buff: None,
            }),
            suffix_item_id: None,
            secondary_suffix_item_id: None,
            stat_choices: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::ValidatedBuild;

    fn pve_ctx() -> BalanceContext {
        BalanceContext::pve()
    }

    fn pve_scenario() -> ScenarioSpec {
        ScenarioSpec::from_balance_context(&pve_ctx())
    }

    fn power_weights() -> OptimizationWeights {
        OptimizationWeights {
            power: 1.0,
            condition: 0.0,
            boon_support: 0.0,
            healing: 0.0,
            sustain: 0.0,
            control: 0.0,
        }
    }

    fn db_with(items: Vec<Item>) -> GameDb {
        let mut db = GameDb::empty_for_tests();
        for item in items {
            db.items_by_type
                .entry(item.item_type.clone())
                .or_default()
                .push(item.id);
            db.items.insert(item.id, item);
        }
        db
    }

    #[test]
    fn legality_filters_game_types_and_kind() {
        let pve_food = test_consumable(
            1,
            "Bowl of PvE Stew",
            ConsumableKind::Nourishment,
            &[("Power", 100)],
            &["Pve", "Wvw"],
            None,
        );
        let pvp_food = test_consumable(
            2,
            "PvP Omelet",
            ConsumableKind::Nourishment,
            &[("Power", 100)],
            &["Pvp", "PvpLobby"],
            None,
        );
        let util = test_consumable(
            3,
            "Sharpening Stone",
            ConsumableKind::Enhancement,
            &[("Power", 100)],
            &["Pve"],
            None,
        );
        let db = db_with(vec![pve_food, pvp_food, util]);
        let nourish = db.nourishments_for(&GameMode::PvE);
        let ids: Vec<u32> = nourish.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![1], "PvP food must not be legal in PvE: {ids:?}");
        let enhance = db.enhancements_for(&GameMode::PvE);
        assert_eq!(enhance.iter().map(|i| i.id).collect::<Vec<_>>(), vec![3]);
        assert!(db
            .nourishments_for(&GameMode::PvP)
            .iter()
            .any(|i| i.id == 2));
        assert_eq!(kind_of(&db.items[&1]), Some(ConsumableKind::Nourishment));
        assert_eq!(kind_of(&db.items[&3]), Some(ConsumableKind::Enhancement));
    }

    #[test]
    fn serialization_writes_chosen_ids_onto_the_build() {
        let food = test_consumable(
            11,
            "Bowl of Sweet and Spicy Butternut Squash Soup",
            ConsumableKind::Nourishment,
            &[("Power", 100), ("Ferocity", 70)],
            &["Pve"],
            None,
        );
        let util = test_consumable(
            12,
            "Superior Sharpening Stone",
            ConsumableKind::Enhancement,
            &[("Power", 100)],
            &["Pve"],
            None,
        );
        let db = db_with(vec![food, util]);
        let mut build = ValidatedBuild::default();
        assign_best_consumables(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pve_ctx(),
            &pve_scenario(),
            &BuildLocks::default(),
        );
        assert_eq!(
            build.food.as_ref().map(|i| (i.id, i.name.as_str())),
            Some((11, "Bowl of Sweet and Spicy Butternut Squash Soup"))
        );
        assert_eq!(
            build.utility.as_ref().map(|i| (i.id, i.name.as_str())),
            Some((12, "Superior Sharpening Stone"))
        );
        let clone = build.clone();
        assert_eq!(clone.food, build.food);
        assert_eq!(clone.utility, build.utility);
    }

    #[test]
    fn locked_food_and_utility_are_not_overwritten() {
        let better = test_consumable(
            21,
            "Huge Power Stew",
            ConsumableKind::Nourishment,
            &[("Power", 1000)],
            &["Pve"],
            None,
        );
        let worse = test_consumable(
            22,
            "Locked Toast",
            ConsumableKind::Nourishment,
            &[("Power", 1)],
            &["Pve"],
            None,
        );
        let util_best = test_consumable(
            23,
            "Best Oil",
            ConsumableKind::Enhancement,
            &[("Power", 500)],
            &["Pve"],
            None,
        );
        let util_lock = test_consumable(
            24,
            "Locked Oil",
            ConsumableKind::Enhancement,
            &[("Power", 1)],
            &["Pve"],
            None,
        );
        let db = db_with(vec![better, worse, util_best, util_lock]);
        let locks = BuildLocks {
            food: Some(22),
            utility: Some(24),
            ..Default::default()
        };
        let mut build = ValidatedBuild::default();
        assign_best_consumables(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pve_ctx(),
            &pve_scenario(),
            &locks,
        );
        assert_eq!(build.food.as_ref().map(|i| i.id), Some(22));
        assert_eq!(build.utility.as_ref().map(|i| i.id), Some(24));
    }

    #[test]
    fn rank_and_axes_move_when_consumable_choice_changes() {
        let power_food = test_consumable(
            31,
            "Power Stew",
            ConsumableKind::Nourishment,
            &[("Power", 800)],
            &["Pve"],
            None,
        );
        let tough_food = test_consumable(
            32,
            "Tough Stew",
            ConsumableKind::Nourishment,
            &[("Toughness", 800)],
            &["Pve"],
            None,
        );
        let db = db_with(vec![power_food, tough_food]);
        let ctx = pve_ctx();
        let scenario = pve_scenario();
        let weights = power_weights();

        let mut none_build = ValidatedBuild::default();
        let none_report = crate::referee::evaluate_validated_build(
            &none_build,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
        );

        let power_build = ValidatedBuild {
            food: Some(ValidatedItem {
                id: 31,
                name: "Power Stew".into(),
            }),
            ..Default::default()
        };
        let power_report = crate::referee::evaluate_validated_build(
            &power_build,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
        );

        let tough_build = ValidatedBuild {
            food: Some(ValidatedItem {
                id: 32,
                name: "Tough Stew".into(),
            }),
            ..Default::default()
        };
        let tough_report = crate::referee::evaluate_validated_build(
            &tough_build,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
        );

        let none_rank = crate::referee::search_rank(&none_report);
        let power_rank = crate::referee::search_rank(&power_report);
        let tough_rank = crate::referee::search_rank(&tough_report);
        assert_ne!(
            power_rank, none_rank,
            "power food must move rank/axes vs empty: {power_rank:?} vs {none_rank:?}"
        );
        assert_ne!(
            power_rank, tough_rank,
            "power vs toughness food must move rank/axes: {power_rank:?} vs {tough_rank:?}"
        );
        assert!(
            power_report.stats.power > none_report.stats.power,
            "power food must fold into calculate_validated_stats"
        );
        // Empty kit: flow realized power stays 0 (no skills). The closed-form
        // combat / stat-direction axes are what a standing consumable moves.
        assert_ne!(
            power_report.stat_direction_score, none_report.stat_direction_score,
            "stat-direction axis must move with power food"
        );
        assert!(
            power_report.primary_combat.strike_dps_index
                > none_report.primary_combat.strike_dps_index
                || power_report.primary_combat.effective_health
                    != none_report.primary_combat.effective_health,
            "combat axes must move with consumable choice"
        );
        assert!(
            tough_report.stats.toughness > none_report.stats.toughness,
            "toughness food must fold into calculate_validated_stats"
        );

        // Inner solve forced empty/none: empty catalog -> same as no consumables.
        let empty = GameDb::empty_for_tests();
        assign_best_consumables(
            &mut none_build,
            &empty,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &BuildLocks::default(),
        );
        assert!(none_build.food.is_none() && none_build.utility.is_none());
        let empty_report = crate::referee::evaluate_validated_build(
            &none_build,
            &empty,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
        );
        assert_eq!(
            crate::referee::search_rank(&empty_report),
            none_rank,
            "forced-empty inner solve must match the no-consumable rank"
        );
        assert_eq!(empty_report.realized, none_report.realized);
    }

    #[test]
    fn non_static_percent_is_skipped() {
        let item = test_consumable(
            41,
            "Proc Oil",
            ConsumableKind::Enhancement,
            &[],
            &["Pve"],
            Some("30% chance to gain 100 Power when you dodge."),
        );
        let mut mods = DamageModifiers::default();
        fold_static_modifiers(&mut mods, &item);
        assert!(
            mods.strike_pct.is_empty() && mods.crit_chance_pct.is_empty(),
            "triggered chance text must not fold: {mods:?}"
        );
        assert!(static_stat_bonus(&item).is_zero());
        assert!(!has_static_effect(&item));
    }

    fn api_item(json: &str) -> Item {
        serde_json::from_str(json).unwrap()
    }

    /// Live /v2/items rows 41569 and 9443 (2026-09-23), trimmed.
    const SOUP: &str = r#"{"id":41569,"name":"Bowl of Sweet and Spicy Butternut Squash Soup","type":"Consumable","level":80,"rarity":"Fine","game_types":["Wvw","Dungeon","Pve"],"details":{"type":"Food","duration_ms":1800000,"apply_count":1,"name":"Nourishment","description":"+100 Power\n+70 Ferocity\n+10% Experience from Kills"}}"#;
    const STONE: &str = r#"{"id":9443,"name":"Superior Sharpening Stone","type":"Consumable","level":80,"rarity":"Fine","game_types":["Wvw","Dungeon","Pve"],"details":{"type":"Utility","duration_ms":1800000,"apply_count":1,"name":"Enhancement","description":"Gain Power Equal to 3% of Your Precision\nGain Power Equal to 6% of Your Ferocity\n+10% Experience from Kills"}}"#;

    #[test]
    fn api_food_and_utility_lines_parse() {
        let soup = api_item(SOUP);
        let s = static_stat_bonus(&soup);
        assert_eq!((s.power, s.ferocity), (100.0, 70.0));
        assert!(unmodeled_lines(&soup).is_empty());

        let stone = api_item(STONE);
        assert!(static_stat_bonus(&stone).is_zero());
        assert_eq!(
            conversions(&stone),
            vec![
                StatConversion {
                    target: "Power",
                    fraction: 0.03,
                    source: "Precision"
                },
                StatConversion {
                    target: "Power",
                    fraction: 0.06,
                    source: "Ferocity"
                },
            ]
        );
        let mut mods = DamageModifiers::default();
        fold_static_modifiers(&mut mods, &stone);
        assert!(
            mods.strike_pct.is_empty() && mods.strike_add_pct.is_empty(),
            "a conversion line is not a damage percent: {mods:?}"
        );
        assert!(unmodeled_lines(&stone).is_empty());

        let odd = api_item(&STONE.replace(
            "Gain Power Equal to 3% of Your Precision",
            "Gain a Spooky Aura",
        ));
        assert_eq!(
            unmodeled_lines(&odd),
            vec!["Gain a Spooky Aura".to_string()]
        );
    }

    #[test]
    fn stone_converts_on_the_stat_sheet() {
        let db = db_with(vec![api_item(SOUP), api_item(STONE)]);
        let validated = ValidatedBuild {
            food: Some(ValidatedItem {
                id: 41569,
                name: "soup".into(),
            }),
            utility: Some(ValidatedItem {
                id: 9443,
                name: "stone".into(),
            }),
            ..ValidatedBuild::default()
        };
        let mut stats = StatBlock {
            power: 1000.0,
            precision: 1000.0,
            ferocity: 200.0,
            ..StatBlock::default()
        };
        fold_into_validated_stats(&mut stats, &mut DamageModifiers::default(), &validated, &db);
        // 1000 + 100 soup + 3% of 1000 precision + 6% of (200 + 70) ferocity
        assert!(
            (stats.power - (1100.0 + 30.0 + 16.2)).abs() < 1e-9,
            "{stats:?}"
        );
        assert_eq!(stats.ferocity, 270.0);
    }

    /// E20b. Oracle: https://wiki.guildwars2.com/wiki/Gain_X_Based_on_Y
    /// (read 2026-09-25): "All conversions are done before other conversions
    /// are taken into account so the value gained from them is not factored
    /// in to any other conversions." The same page lists flat food and
    /// utility bonuses as conversion sources.
    ///
    /// Soup is +100 Power / +70 Ferocity. The fixture trait adds 10% of Power
    /// as Ferocity (`round(1100 * 0.10) = 110`). Stone 9443 adds 3% Precision
    /// and 6% Ferocity as Power from the pre-conversion sheet (ferocity 70),
    /// not from the post-trait ferocity (180).
    #[test]
    fn conversion_reads_pre_trait_sheet_per_gain_x_based_on_y() {
        use gw2_api::models::{Fact, Trait};

        let mut db = db_with(vec![api_item(SOUP), api_item(STONE)]);
        db.traits.insert(
            9001,
            Trait {
                id: 9001,
                name: "Power to Ferocity".into(),
                icon: None,
                description: None,
                specialization: 1,
                tier: 1,
                order: 0,
                slot: "Major".into(),
                facts: vec![Fact::BuffConversion {
                    text: None,
                    icon: None,
                    source: Some("Power".into()),
                    percent: Some(10.0),
                    target: Some("Ferocity".into()),
                }],
                traited_facts: vec![],
                fact_parse_drops: 0,
                skills: vec![],
            },
        );
        let validated = ValidatedBuild {
            specializations: vec![crate::validation::ValidatedSpec {
                spec_id: 1,
                name: "fixture".into(),
                elite: false,
                trait_ids: vec![9001],
                trait_names: vec!["Power to Ferocity".into()],
                all_trait_ids: vec![9001],
            }],
            food: Some(ValidatedItem {
                id: 41569,
                name: "soup".into(),
            }),
            utility: Some(ValidatedItem {
                id: 9443,
                name: "stone".into(),
            }),
            ..ValidatedBuild::default()
        };
        let (stats, _) = engine::calculate_validated_stats(&validated, &db, "Warrior", &pve_ctx());
        let pre_trait_power = 1100.0 + 0.03 * 1000.0 + 0.06 * 70.0;
        let post_trait_power = 1100.0 + 0.03 * 1000.0 + 0.06 * 180.0;
        assert!(
            (stats.power - pre_trait_power).abs() < 1e-9,
            "power {} must be the pre-conversion reading {pre_trait_power}, not the post-trait reading {post_trait_power}",
            stats.power
        );
        assert_eq!(
            stats.ferocity, 180.0,
            "trait conversion must see the food's flat power"
        );
    }

    /// E20a: a StatConversion-only utility is picker-eligible and wins when
    /// the conversion adds more power than a flat alternative on the fixture
    /// sheet. Fold order is not under test (`stone_converts_on_the_stat_sheet`).
    #[test]
    fn conversion_only_utility_selected_when_it_beats_flat() {
        let stone = api_item(STONE);
        assert!(
            static_stat_bonus(&stone).is_zero(),
            "stone-class item must have no flat bonus"
        );
        assert!(
            has_static_effect(&stone),
            "conversion-only utility must not be filtered out of the picker"
        );
        let flat = test_consumable(
            9450,
            "Flat Power Oil",
            ConsumableKind::Enhancement,
            &[("Power", 40)],
            &["Pve"],
            None,
        );
        // Locked feast is the fixture sheet: precision and ferocity the stone
        // converts. Base precision alone is only +30 power, which would lose
        // to a larger flat; this sheet makes the conversion the better score.
        let feast = test_consumable(
            9451,
            "Crit Feast",
            ConsumableKind::Nourishment,
            &[("Precision", 2000), ("Ferocity", 1500)],
            &["Pve"],
            None,
        );
        let db = db_with(vec![stone, flat, feast]);
        let weights = power_weights();
        let ctx = pve_ctx();
        let scenario = pve_scenario();
        let locks = BuildLocks {
            food: Some(9451),
            ..Default::default()
        };

        let mut picked = ValidatedBuild::default();
        assign_best_consumables(
            &mut picked,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks,
        );
        assert_eq!(picked.food.as_ref().map(|i| i.id), Some(9451));
        assert_eq!(
            picked.utility.as_ref().map(|i| i.id),
            Some(9443),
            "Superior Sharpening Stone must be selected over the flat oil"
        );

        let sheet = |utility_id: u32, utility_name: &str| ValidatedBuild {
            food: Some(ValidatedItem {
                id: 9451,
                name: "Crit Feast".into(),
            }),
            utility: Some(ValidatedItem {
                id: utility_id,
                name: utility_name.into(),
            }),
            ..Default::default()
        };
        let stone_report = crate::referee::evaluate_validated_build(
            &sheet(9443, "Superior Sharpening Stone"),
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
        );
        let flat_report = crate::referee::evaluate_validated_build(
            &sheet(9450, "Flat Power Oil"),
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
        );
        assert!(
            stone_report.stats.power > flat_report.stats.power,
            "stone power {} must beat flat power {}",
            stone_report.stats.power,
            flat_report.stats.power
        );
        assert!(
            stone_report.primary_combat.strike_dps_index
                > flat_report.primary_combat.strike_dps_index,
            "stone strike index {} must beat flat {}",
            stone_report.primary_combat.strike_dps_index,
            flat_report.primary_combat.strike_dps_index
        );
        assert!(
            stone_report.stat_direction_score > flat_report.stat_direction_score,
            "stone score {} must beat flat score {}",
            stone_report.stat_direction_score,
            flat_report.stat_direction_score
        );
    }

    #[test]
    fn standing_percent_folds() {
        let item = test_consumable(
            42,
            "Force Oil",
            ConsumableKind::Enhancement,
            &[],
            &["Pve"],
            Some("+5% Damage"),
        );
        let mut mods = DamageModifiers::default();
        fold_static_modifiers(&mut mods, &item);
        assert!(
            !mods.strike_pct.is_empty() || !mods.strike_add_pct.is_empty(),
            "standing +5% Damage must fold: {mods:?}"
        );
    }

    /// One-sided food lock must pin the locked item into scoring so the free
    /// utility pick can change with locked food power (joint product <= 64).
    #[test]
    fn one_sided_food_lock_changes_free_utility_pick() {
        let flat_util = test_consumable(
            51,
            "Flat Power Oil",
            ConsumableKind::Enhancement,
            &[("Power", 120)],
            &["Pve"],
            None,
        );
        let pct_util = test_consumable(
            52,
            "Percent Damage Oil",
            ConsumableKind::Enhancement,
            &[],
            &["Pve"],
            Some("+10% Damage"),
        );
        let tiny_food = test_consumable(
            53,
            "Tiny Locked Toast",
            ConsumableKind::Nourishment,
            &[("Power", 1)],
            &["Pve"],
            None,
        );
        let huge_food = test_consumable(
            54,
            "Huge Locked Stew",
            ConsumableKind::Nourishment,
            &[("Power", 20_000)],
            &["Pve"],
            None,
        );
        let db = db_with(vec![flat_util, pct_util, tiny_food, huge_food]);
        let weights = power_weights();
        let ctx = pve_ctx();
        let scenario = pve_scenario();

        let locks_tiny = BuildLocks {
            food: Some(53),
            ..Default::default()
        };
        let mut build_tiny = ValidatedBuild::default();
        assign_best_consumables(
            &mut build_tiny,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks_tiny,
        );
        assert_eq!(build_tiny.food.as_ref().map(|i| i.id), Some(53));
        let tiny_util = build_tiny.utility.as_ref().map(|i| i.id);

        let locks_huge = BuildLocks {
            food: Some(54),
            ..Default::default()
        };
        let mut build_huge = ValidatedBuild::default();
        assign_best_consumables(
            &mut build_huge,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks_huge,
        );
        assert_eq!(build_huge.food.as_ref().map(|i| i.id), Some(54));
        let huge_util = build_huge.utility.as_ref().map(|i| i.id);

        assert_eq!(
            tiny_util,
            Some(51),
            "tiny locked food: flat power util should win, got {tiny_util:?}"
        );
        assert_eq!(
            huge_util,
            Some(52),
            "huge locked food: percent damage util should win, got {huge_util:?}"
        );
        assert_ne!(
            tiny_util, huge_util,
            "free-slot util pick must change with locked food power"
        );
    }

    /// Large-catalog path (product > 64): locked utility must be pinned while
    /// choosing food, same interaction as the joint one-sided lock case.
    #[test]
    fn large_catalog_one_sided_util_lock_changes_free_food_pick() {
        let flat_food = test_consumable(
            61,
            "Flat Power Stew",
            ConsumableKind::Nourishment,
            &[("Power", 120)],
            &["Pve"],
            None,
        );
        let pct_food = test_consumable(
            62,
            "Percent Damage Stew",
            ConsumableKind::Nourishment,
            &[],
            &["Pve"],
            Some("+10% Damage"),
        );
        let tiny_util = test_consumable(
            63,
            "Tiny Locked Oil",
            ConsumableKind::Enhancement,
            &[("Power", 1)],
            &["Pve"],
            None,
        );
        let huge_util = test_consumable(
            64,
            "Huge Locked Oil",
            ConsumableKind::Enhancement,
            &[("Power", 20_000)],
            &["Pve"],
            None,
        );
        // 70 filler foods => food_choices = 73 (None + flat + pct + 70), util locked
        // => product = 73 > 64, forcing the independent large-catalog path.
        let mut items = vec![flat_food, pct_food, tiny_util, huge_util];
        for i in 0..70 {
            items.push(test_consumable(
                1000 + i,
                &format!("Filler Food {i}"),
                ConsumableKind::Nourishment,
                &[("Toughness", 1)],
                &["Pve"],
                None,
            ));
        }
        let db = db_with(items);
        let weights = power_weights();
        let ctx = pve_ctx();
        let scenario = pve_scenario();

        let locks_tiny = BuildLocks {
            utility: Some(63),
            ..Default::default()
        };
        let mut build_tiny = ValidatedBuild::default();
        assign_best_consumables(
            &mut build_tiny,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks_tiny,
        );
        assert_eq!(build_tiny.utility.as_ref().map(|i| i.id), Some(63));
        let tiny_food = build_tiny.food.as_ref().map(|i| i.id);

        let locks_huge = BuildLocks {
            utility: Some(64),
            ..Default::default()
        };
        let mut build_huge = ValidatedBuild::default();
        assign_best_consumables(
            &mut build_huge,
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks_huge,
        );
        assert_eq!(build_huge.utility.as_ref().map(|i| i.id), Some(64));
        let huge_food = build_huge.food.as_ref().map(|i| i.id);

        assert_eq!(
            tiny_food,
            Some(61),
            "tiny locked util: flat power food should win, got {tiny_food:?}"
        );
        assert_eq!(
            huge_food,
            Some(62),
            "huge locked util: percent damage food should win, got {huge_food:?}"
        );
        assert_ne!(
            tiny_food, huge_food,
            "free-slot food pick must change with locked utility power"
        );
    }
}
