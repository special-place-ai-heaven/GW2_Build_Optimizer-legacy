//! Optimization orchestration — runs the full pipeline.
//! Combines deterministic gear search with LLM reasoning (S08).

use std::collections::HashMap;

use gw2_api::models::{
    EquipmentTab, Item, ItemStat, Profession, PvpAmulet, Specialization, Trait as GW2Trait,
};
use gw2_core::types::{GameMode, GearSlot, GearSlots, PrefixRef};

use gw2_api::models::Fact;

use crate::balance::BalanceContext;
use crate::combat::{self, CombatPerformance, DamageModifiers};
use crate::data;
use crate::gamedb::GameDb;
use crate::llm::LlmClient;
use crate::rotation;
use crate::scoring::{self, score_with_weights, OptimizationWeights, StatWeights};
use crate::search::{search_gear_prefixes, search_spec_combos, GearCandidate};
use crate::stats;
use crate::validation::{self, ValidatedBuild};
use crate::weapon_budget::{self, LandWeaponBudget};

/// Selected PvP amulet in a build candidate (PvP mode only).
#[derive(Debug, Clone)]
pub struct PvpAmuletCandidate {
    pub id: u32,
    pub name: String,
    pub stats: HashMap<String, i32>,
}

/// A complete build candidate ready for comparison or LLM evaluation.
#[derive(Debug, Clone)]
pub struct BuildCandidate {
    pub gear: GearCandidate,
    pub elite_spec: Option<u32>,
    pub core_specs: Vec<u32>,
    /// All equipped trait IDs (minor + selected major, 3 per spec column).
    pub equipped_traits: Vec<u32>,
    pub stats: stats::StatBlock,
    pub derived: stats::DerivedStats,
    pub score: f64,
    /// Combat performance metrics (Solo profile) for display and scoring.
    pub combat: CombatPerformance,
    /// Extracted damage modifiers (for recalculating with different buff profiles).
    pub modifiers: DamageModifiers,
    /// Selected PvP amulet (only set in PvP mode; None for PvE/WvW).
    pub pvp_amulet: Option<PvpAmuletCandidate>,
    /// Overall data quality assessment for this candidate's inputs.
    pub data_quality: data::DataQuality,
    /// Reasons for any data quality degradation.
    pub quality_reasons: Vec<data::DataQualityReason>,
}

/// Progress update during optimization.
#[derive(Debug, Clone)]
pub struct OptimizeProgress {
    pub stage: String,
    pub done: bool,
}

/// Run the optimization pipeline for a given profession and archetype.
/// Returns top N candidates ranked by score, or an error describing why none were found.
/// For PvP, skips gear search (stats come from amulet) and only evaluates spec/trait combos.
/// For PvE/WvW, runs full gear + spec search.
// Core optimization entry point; the caches, weights, progress callback, and
// top-N are distinct concerns — bundling them into a params struct adds
// indirection without clarifying the call site.
/// Legacy tier-3 optimizer, without a cancellation probe.
///
/// Equivalent to [`optimize_cancellable`] with a probe that never fires. Kept
/// because the addon's fallback-2 call site lives in
/// `crates/addon/src/ui/main_view/optimize_flow.rs`, which this change does not
/// own; **that call site must move to `optimize_cancellable`** so that
/// cancelling an optimization which has fallen through to tier 3 actually stops
/// it instead of letting the worker run to completion and write its result
/// back over a cancelled request.
#[allow(clippy::too_many_arguments)]
pub fn optimize(
    profession: &Profession,
    weights: &OptimizationWeights,
    current_equipment: Option<&EquipmentTab>,
    items_cache: &HashMap<u32, Item>,
    itemstats_cache: &HashMap<u32, ItemStat>,
    specs_cache: &HashMap<u32, Specialization>,
    traits_cache: &HashMap<u32, GW2Trait>,
    on_progress: impl FnMut(OptimizeProgress),
    top_n: usize,
    ctx: &BalanceContext,
    locks: &gw2_core::types::BuildLocks,
    pvp_amulets: &HashMap<u32, PvpAmulet>,
) -> Result<Vec<BuildCandidate>, String> {
    optimize_cancellable(
        profession,
        weights,
        current_equipment,
        items_cache,
        itemstats_cache,
        specs_cache,
        traits_cache,
        on_progress,
        top_n,
        ctx,
        locks,
        pvp_amulets,
        &|| false,
    )
}

/// Legacy tier-3 optimizer.
///
/// `is_cancelled` is polled at every stage boundary and once per gear candidate
/// in the combine loop — the loop is `gear_candidates × spec_combos` full combat
/// evaluations, which is where a cancelled run used to keep burning CPU inside
/// the game process. A cancelled run returns `Err("Cancelled")`; it never
/// returns a partial candidate list that a caller could mistake for a result.
#[allow(clippy::too_many_arguments)]
pub fn optimize_cancellable(
    profession: &Profession,
    weights: &OptimizationWeights,
    _current_equipment: Option<&EquipmentTab>,
    _items_cache: &HashMap<u32, Item>,
    itemstats_cache: &HashMap<u32, ItemStat>,
    specs_cache: &HashMap<u32, Specialization>,
    traits_cache: &HashMap<u32, GW2Trait>,
    mut on_progress: impl FnMut(OptimizeProgress),
    top_n: usize,
    ctx: &BalanceContext,
    locks: &gw2_core::types::BuildLocks,
    pvp_amulets: &HashMap<u32, PvpAmulet>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<BuildCandidate>, String> {
    if is_cancelled() {
        return Err("Cancelled".into());
    }
    if ctx.game_mode == GameMode::PvP {
        return optimize_pvp(
            profession,
            weights,
            specs_cache,
            traits_cache,
            &mut on_progress,
            top_n,
            locks,
            ctx,
            pvp_amulets,
            is_cancelled,
        )
        .and_then(|v| {
            if v.is_empty() {
                Err(format!(
                    "No PvP candidates found for {} / {}",
                    profession.name,
                    weights.summary_label()
                ))
            } else {
                Ok(v)
            }
        });
    }

    on_progress(OptimizeProgress {
        stage: "Searching gear combinations...".into(),
        done: false,
    });

    // 1. Find best gear prefix combinations
    let mut gear_candidates = search_gear_prefixes(weights, itemstats_cache);
    if gear_candidates.is_empty() {
        return Err(format!(
            "No gear stat prefixes found for {}. GameDb has {} itemstats loaded.",
            weights.summary_label(),
            itemstats_cache.len()
        ));
    }

    // Score each gear candidate (preliminary — no traits/modifiers yet)
    let empty_mods = DamageModifiers::default();
    let solo_profile = &combat::buff_profiles_for_profession(&profession.name, ctx)[0];
    let cw = combat::condition_weights_for_profession(&profession.name, ctx);
    for candidate in &mut gear_candidates {
        let candidate_stats = calculate_candidate_stats(candidate, itemstats_cache);
        let mut full_stats = stats::base_stats();
        full_stats += &candidate_stats;
        let derived = stats::compute_derived(&full_stats, &profession.name);
        let perf = combat::calculate_combat_performance(
            &full_stats,
            &derived,
            &empty_mods,
            solo_profile,
            &cw,
            &profession.name,
            ctx,
        );
        candidate.score = score_with_weights(&perf, weights);
    }

    gear_candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    gear_candidates.truncate(top_n * 3); // keep extra — traits can shift rankings significantly

    if is_cancelled() {
        return Err("Cancelled".into());
    }
    on_progress(OptimizeProgress {
        stage: "Evaluating specialization combinations...".into(),
        done: false,
    });

    // 2. Find valid spec combinations
    let spec_combos = search_spec_combos(&profession.specializations, specs_cache, locks);
    if spec_combos.is_empty() {
        let core_count = profession
            .specializations
            .iter()
            .filter(|id| specs_cache.get(id).is_some_and(|s| !s.elite))
            .count();
        let elite_count = profession
            .specializations
            .iter()
            .filter(|id| specs_cache.get(id).is_some_and(|s| s.elite))
            .count();
        return Err(format!(
            "No valid spec combinations for {}. Has {} core specs (need ≥3) and {} elite specs. \
             {} of {} spec IDs found in GameDb.",
            profession.name,
            core_count,
            elite_count,
            profession
                .specializations
                .iter()
                .filter(|id| specs_cache.contains_key(id))
                .count(),
            profession.specializations.len()
        ));
    }

    let stat_weights = weights.to_stat_weights();

    // 3. Combine gear + specs into full candidates
    let mut all_candidates: Vec<BuildCandidate> = Vec::new();

    // Pre-compute spec-combo invariants (trait_ids, trait stats, trait modifiers).
    // These are gear-independent — recomputing them inside the gear loop was
    // ~5x wasted work for a typical 5-gear search.
    struct PrecomputedSpec {
        elite: Option<u32>,
        cores: Vec<u32>,
        trait_ids: Vec<u32>,
        trait_stats: stats::StatBlock,
        modifiers: combat::DamageModifiers,
    }
    let precomputed_specs: Vec<PrecomputedSpec> = spec_combos
        .iter()
        .map(|(elite, cores)| {
            let spec_ids: Vec<u32> = cores.iter().copied().chain(elite.iter().copied()).collect();

            let mut trait_ids = Vec::new();
            for &spec_id in &spec_ids {
                if let Some(spec) = specs_cache.get(&spec_id) {
                    trait_ids.extend(&spec.minor_traits);
                    let best = select_best_major_traits(
                        &spec.major_traits,
                        &stat_weights,
                        traits_cache,
                        locks,
                        spec_id,
                    );
                    trait_ids.extend(best);
                }
            }

            let trait_stats =
                stats::calculate_trait_stats_for_mode(&trait_ids, traits_cache, &ctx.game_mode);
            let modifiers = combat::extract_damage_modifiers(
                &trait_ids,
                None,
                &[],
                None,
                traits_cache,
                _items_cache,
                ctx,
            );
            PrecomputedSpec {
                elite: *elite,
                cores: cores.clone(),
                trait_ids,
                trait_stats,
                modifiers,
            }
        })
        .collect();

    for gear in &gear_candidates {
        // Once per gear, not once per (gear, spec): the inner loop is cheap
        // relative to the probe, and the outer one is what makes this pass long.
        if is_cancelled() {
            return Err("Cancelled".into());
        }
        // gear_stats is spec-invariant — compute once per gear.
        let gear_stats = calculate_candidate_stats(gear, itemstats_cache);

        for spec in &precomputed_specs {
            let mut full_stats = stats::base_stats();
            full_stats += &gear_stats;
            full_stats += &spec.trait_stats;
            stats::apply_trait_conversions(&mut full_stats, &spec.trait_ids, traits_cache);

            let derived = stats::compute_derived(&full_stats, &profession.name);

            // Calculate combat performance with Solo profile
            let combat_perf = combat::calculate_combat_performance(
                &full_stats,
                &derived,
                &spec.modifiers,
                solo_profile,
                &cw,
                &profession.name,
                ctx,
            );
            let score = score_with_weights(&combat_perf, weights);
            let (mut data_quality, mut quality_reasons) =
                quality_from_modifiers(&spec.modifiers, &[], false, ctx.game_mode.label());
            append_trait_fact_parse_drops(
                &mut data_quality,
                &mut quality_reasons,
                &spec.trait_ids,
                traits_cache,
                ctx.game_mode.label(),
            );

            all_candidates.push(BuildCandidate {
                gear: gear.clone(),
                elite_spec: spec.elite,
                core_specs: spec.cores.clone(),
                equipped_traits: spec.trait_ids.clone(),
                stats: full_stats,
                derived,
                score,
                combat: combat_perf,
                modifiers: spec.modifiers.clone(),
                pvp_amulet: None,
                data_quality,
                quality_reasons,
            });
        }
    }

    if is_cancelled() {
        return Err("Cancelled".into());
    }
    on_progress(OptimizeProgress {
        stage: "Ranking candidates...".into(),
        done: false,
    });

    all_candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all_candidates.truncate(top_n);

    on_progress(OptimizeProgress {
        stage: "Done".into(),
        done: true,
    });

    if all_candidates.is_empty() {
        return Err(format!(
            "Optimization produced 0 candidates from {} gear × {} spec combos for {} / {}",
            gear_candidates.len(),
            spec_combos.len(),
            profession.name,
            weights.summary_label()
        ));
    }

    Ok(all_candidates)
}

/// PvP optimization: iterates PvP amulets × spec/trait combos (gear is replaced by amulet system).
/// PvP amulet stats REPLACE gear stats — the stat block is: base_stats + amulet + traits.
/// Slot-budget data is NOT loaded during PvP optimization.
/// Returns an error if no PvP amulet data is available (no silent zero-stat fallback).
// PvP search variant mirroring `optimize`'s caches/weights/callback shape; a
// params struct would not improve the single internal call site.
#[allow(clippy::too_many_arguments)]
fn optimize_pvp(
    profession: &Profession,
    weights: &OptimizationWeights,
    specs_cache: &HashMap<u32, Specialization>,
    traits_cache: &HashMap<u32, GW2Trait>,
    on_progress: &mut impl FnMut(OptimizeProgress),
    top_n: usize,
    locks: &gw2_core::types::BuildLocks,
    ctx: &BalanceContext,
    pvp_amulets: &HashMap<u32, PvpAmulet>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<BuildCandidate>, String> {
    if pvp_amulets.is_empty() {
        return Err("No PvP amulet data available. Download game data first.".to_string());
    }

    on_progress(OptimizeProgress {
        stage: "Evaluating PvP amulet × specialization combinations...".into(),
        done: false,
    });

    let spec_combos = search_spec_combos(&profession.specializations, specs_cache, locks);
    let mut all_candidates: Vec<BuildCandidate> = Vec::new();

    // PvP: no gear search, use empty gear candidate
    let empty_gear = GearCandidate {
        gear_slots: GearSlots::default(),
        stat_prefix_name: "(PvP Amulet)".into(),
        score: 0.0,
    };

    let solo_profile = &combat::buff_profiles_for_profession(&profession.name, ctx)[0];
    let stat_weights = weights.to_stat_weights();
    let cw = combat::condition_weights_for_profession(&profession.name, ctx);

    // Iterate amulets by id so candidates with identical scores break ties
    // deterministically. `pvp_amulets.values()` iteration order is unspecified;
    // the downstream `sort_by(...)` is stable but a stable sort preserves whatever
    // input order it got, so the "best" amulet could vary across runs on ties.
    let mut amulets_sorted: Vec<&PvpAmulet> = pvp_amulets.values().collect();
    amulets_sorted.sort_by_key(|a| a.id);

    // Pre-compute spec-combo invariants. trait_ids, trait_stats, and modifiers
    // do not depend on the chosen amulet — recomputing them per amulet was
    // ~N_amulets wasted work (often >10 amulets per profession in GW2).
    let empty_items_cache: HashMap<u32, gw2_api::models::Item> = HashMap::new();
    struct PvpPrecomputedSpec {
        elite: Option<u32>,
        cores: Vec<u32>,
        trait_ids: Vec<u32>,
        trait_stats: stats::StatBlock,
        modifiers: combat::DamageModifiers,
    }
    let precomputed_specs: Vec<PvpPrecomputedSpec> = spec_combos
        .iter()
        .map(|(elite, cores)| {
            let spec_ids: Vec<u32> = cores.iter().copied().chain(elite.iter().copied()).collect();
            let mut trait_ids = Vec::new();
            for &spec_id in &spec_ids {
                if let Some(spec) = specs_cache.get(&spec_id) {
                    trait_ids.extend(&spec.minor_traits);
                    let best = select_best_major_traits(
                        &spec.major_traits,
                        &stat_weights,
                        traits_cache,
                        locks,
                        spec_id,
                    );
                    trait_ids.extend(best);
                }
            }
            let trait_stats =
                stats::calculate_trait_stats_for_mode(&trait_ids, traits_cache, &ctx.game_mode);
            let modifiers = combat::extract_damage_modifiers(
                &trait_ids,
                None,
                &[],
                None,
                traits_cache,
                &empty_items_cache,
                ctx,
            );
            PvpPrecomputedSpec {
                elite: *elite,
                cores: cores.clone(),
                trait_ids,
                trait_stats,
                modifiers,
            }
        })
        .collect();

    for amulet in amulets_sorted {
        if is_cancelled() {
            return Err("Cancelled".into());
        }
        for spec in &precomputed_specs {
            // PvP stat block: base_stats + amulet stats + trait stats (no gear)
            let mut full_stats = stats::base_stats();

            // Apply amulet stats (replaces gear stats). Sorted keys: f64
            // addition is order-sensitive and HashMap order varies per
            // process — same determinism rule as every other accumulation.
            let mut attrs: Vec<_> = amulet.attributes.iter().collect();
            attrs.sort();
            for (attr, &value) in attrs {
                full_stats.add(attr, value as f64);
            }

            // Apply trait stats (precomputed)
            full_stats += &spec.trait_stats;
            stats::apply_trait_conversions(&mut full_stats, &spec.trait_ids, traits_cache);

            let derived = stats::compute_derived(&full_stats, &profession.name);

            let combat_perf = combat::calculate_combat_performance(
                &full_stats,
                &derived,
                &spec.modifiers,
                solo_profile,
                &cw,
                &profession.name,
                ctx,
            );
            let score = score_with_weights(&combat_perf, weights);
            let (mut data_quality, mut quality_reasons) =
                quality_from_modifiers(&spec.modifiers, &[], false, ctx.game_mode.label());
            append_trait_fact_parse_drops(
                &mut data_quality,
                &mut quality_reasons,
                &spec.trait_ids,
                traits_cache,
                ctx.game_mode.label(),
            );

            all_candidates.push(BuildCandidate {
                gear: empty_gear.clone(),
                elite_spec: spec.elite,
                core_specs: spec.cores.clone(),
                equipped_traits: spec.trait_ids.clone(),
                stats: full_stats,
                derived,
                score,
                combat: combat_perf,
                modifiers: spec.modifiers.clone(),
                pvp_amulet: Some(PvpAmuletCandidate {
                    id: amulet.id,
                    name: amulet.name.clone(),
                    stats: amulet.attributes.clone(),
                }),
                data_quality,
                quality_reasons,
            });
        }
    }

    on_progress(OptimizeProgress {
        stage: "Ranking PvP candidates...".into(),
        done: false,
    });

    all_candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all_candidates.truncate(top_n);

    on_progress(OptimizeProgress {
        stage: "Done".into(),
        done: true,
    });

    Ok(all_candidates)
}

/// Approximate stats for a legacy gear candidate from slot budget data.
///
/// A [`GearCandidate`] carries prefixes but no weapon *types*, so the land
/// weapon budget is read off slot occupancy: `search.rs` writes both set-1
/// hands when it means a one-handed pair, and only the main hand when it means
/// a two-hander. Weapon set 2 is not part of the legacy projection and is
/// skipped explicitly rather than left to happen to be empty — the old code
/// mapped both `WeaponSet1Main` *and* `WeaponSet2Main` to the two-hand budget,
/// so a filled 16-slot map billed the inactive set as well.
fn calculate_candidate_stats(
    candidate: &GearCandidate,
    itemstats_cache: &HashMap<u32, ItemStat>,
) -> stats::StatBlock {
    let mut stats = stats::StatBlock::default();
    let budgets = data::slot_budgets::slot_budgets();
    let held = |slot| candidate.gear_slots.get(slot).is_some();
    let weapons = land_budget_from_occupancy(
        held(GearSlot::WeaponSet1Main),
        held(GearSlot::WeaponSet1Off),
    );

    // Deterministic order: the candidate map is zipped against `GearSlot::ALL`
    // (canonical slot order). The pre-slot-vector version iterated a
    // HashMap, whose order was unspecified.
    for (slot, cell) in GearSlot::ALL.iter().zip(candidate.gear_slots.map.iter()) {
        let Some(prefix) = cell else { continue };
        let Some(itemstat) = itemstats_cache.get(&prefix.itemstat_id) else {
            continue;
        };
        let slot_type = match slot {
            GearSlot::WeaponSet2Main | GearSlot::WeaponSet2Off => continue,
            GearSlot::WeaponSet1Main | GearSlot::WeaponSet1Off => {
                let budget_slot = if *slot == GearSlot::WeaponSet1Main {
                    "WeaponA1"
                } else {
                    "WeaponA2"
                };
                match land_weapon_slot_type_from_occupancy(budget_slot, weapons) {
                    Some(kind) => kind,
                    None => continue,
                }
            }
            other => data::slot_budgets::slot_type_for_gear_slot(*other),
        };
        let shape = data::stat_shape_from_attr_count(itemstat.attributes.len());
        let Some(budget) = budgets.get(slot_type, shape) else {
            continue;
        };

        add_budget_stats_for_itemstat(&mut stats, itemstat, budget);
    }

    stats
}

/// The land weapon budget implied by slot-map occupancy alone.
///
/// [`weapon_budget::land_weapon_budget`] reads the weapon *type*, which is the
/// right answer whenever a build names its weapons. Legacy candidates and
/// uniform-prefix estimates name none, so occupancy is the only signal there
/// is: a filled main hand beside an empty off-hand is the two-hander the legacy
/// projection meant, and two filled hands are a one-hand pair. Never both
/// budgets at once, which is the 376-point bug this replaces.
fn land_budget_from_occupancy(main_hand: bool, off_hand: bool) -> LandWeaponBudget {
    match (main_hand, off_hand) {
        (true, true) => LandWeaponBudget::OneHandPair,
        (true, false) => LandWeaponBudget::TwoHand,
        (false, true) => LandWeaponBudget::OneHand,
        (false, false) => LandWeaponBudget::Empty,
    }
}

/// Occupancy-only sibling of [`land_weapon_slot_type`], for candidates that
/// carry no weapon names to hand it a [`validation::ValidatedWeaponSet`].
fn land_weapon_slot_type_from_occupancy(
    slot_name: &str,
    budget: LandWeaponBudget,
) -> Option<data::SlotType> {
    match (slot_name, budget) {
        ("WeaponA1", LandWeaponBudget::TwoHand) => Some(data::SlotType::WeaponTwoHand),
        ("WeaponA1", LandWeaponBudget::OneHandPair) => Some(data::SlotType::WeaponOneHand),
        ("WeaponA2", LandWeaponBudget::OneHandPair) | ("WeaponA2", LandWeaponBudget::OneHand) => {
            Some(data::SlotType::WeaponOneHand)
        }
        _ => None,
    }
}

/// One uniform prefix over a whole kit. PvE/WvW: armour, trinkets, and **one**
/// land weapon set. PvP: the matching amulet, or nothing at all.
///
/// Returns the reason the kit could not be priced, when there is one. `None`
/// means the stats are complete, not merely that nothing went wrong loudly.
///
/// **PvP is terminal on a miss.** An amulet replaces gear entirely — a legal
/// sPvP amulet is 3000 attribute points — and 53 of the 66 live named prefixes
/// (Celestial, Viper's, Trailblazer's, Minstrel's, Harrier's, …) have no amulet
/// counterpart. Falling through to the land budget handed those prefixes 3607
/// (ThreeStat) or 3944 (FourStat) points, so the *amulet-less* prefixes
/// systematically outscored every legal one and PvP optimization converged on
/// builds that cannot be equipped in PvP. A miss now leaves the block at zero
/// and says why.
///
/// **One weapon set, not two.** The static `EQUIPMENT_SLOTS` table lists
/// WeaponA1 as a two-hand budget *and* WeaponA2 as a one-hand budget, so
/// walking it billed 251 + 125 = 376 points for weapons a character can never
/// hold at once. This estimator has no weapon *types* to read — it is handed a
/// prefix id and nothing else — so it bills the shape the caller's kit
/// describes: a filled main hand and a filled off-hand, i.e. one
/// [`LandWeaponBudget::OneHandPair`] (125 + 125 = 250 ThreeStat). That is
/// within one point of the two-hander's 251 either way, where the old model was
/// 126 points over. Callers that *do* know the weapons —
/// [`apply_validated_gear_stats`], the synergy candidate scorer — take the
/// type-aware [`weapon_budget::land_weapon_budget`] path instead of this one.
pub fn apply_optimized_gear_stats(
    stats: &mut stats::StatBlock,
    db: &GameDb,
    prefix_id: Option<u32>,
    ctx: &BalanceContext,
) -> Option<data::DataQualityReason> {
    let id = prefix_id?;
    let itemstat = db.itemstats.get(&id)?;
    if ctx.game_mode == GameMode::PvP {
        if let Some(amulet) = match_pvp_amulet(db, &itemstat.name) {
            // Sorted keys — see the determinism note at the PvP candidate path.
            let mut attrs: Vec<_> = amulet.attributes.iter().collect();
            attrs.sort();
            for (attr, &value) in attrs {
                stats.add(attr, value as f64);
            }
            return None;
        } else {
            return Some(pvp_amulet_missing_reason(&itemstat.name, ctx));
        }
    }
    let budgets = data::slot_budgets::slot_budgets();
    let shape = data::stat_shape_from_attr_count(itemstat.attributes.len());
    let mut priced = true;
    for &(slot_type, slot_name) in data::EQUIPMENT_SLOTS {
        // Weapon budgets come from the land model below, never from the static
        // table: the table lists A1 as a two-hand budget *and* A2 as a one-hand
        // budget, which bills both hands of a single set.
        if matches!(slot_name, "WeaponA1" | "WeaponA2" | "WeaponB1" | "WeaponB2") {
            continue;
        }
        if let Some(budget) = budgets.get(slot_type, shape) {
            priced &= add_budget_stats_for_itemstat(stats, itemstat, budget);
        }
    }
    for &slot_type in LandWeaponBudget::OneHandPair.slots() {
        if let Some(budget) = budgets.get(slot_type, shape) {
            priced &= add_budget_stats_for_itemstat(stats, itemstat, budget);
        }
    }
    if priced {
        None
    } else {
        Some(unpriceable_prefix_reason(&itemstat.name, id, ctx))
    }
}

/// Why a PvP build carries no gear stats: its prefix has no amulet.
fn pvp_amulet_missing_reason(prefix_name: &str, ctx: &BalanceContext) -> data::DataQualityReason {
    data::DataQualityReason {
        field: "pvp_amulet".into(),
        entity: prefix_name.to_string(),
        modes: vec![ctx.game_mode.label().to_string()],
        explanation: format!(
            "'{prefix_name}' has no PvP amulet, so this build has no gear stats in PvP. \
             Scoring it against land-gear budgets would credit it with stats no amulet \
             can provide; pick a prefix that exists as an amulet instead."
        ),
    }
}

/// Why a kit carries no gear stats: the game data cannot price its prefix.
///
/// Covers both an itemstat row the slot-budget model cannot read (no positive
/// multiplier) and, at the per-slot appliers, an id that does not resolve at all
/// — including the `itemstat_id: 0` that legacy save migration stamps. The old
/// behaviour was a silent `continue`, which shipped a zeroed slot as if it were
/// a real one.
fn unpriceable_prefix_reason(
    prefix_name: &str,
    prefix_id: u32,
    ctx: &BalanceContext,
) -> data::DataQualityReason {
    data::DataQualityReason {
        field: "itemstat".into(),
        entity: if prefix_name.is_empty() {
            format!("itemstat {prefix_id}")
        } else {
            prefix_name.to_string()
        },
        modes: vec![ctx.game_mode.label().to_string()],
        explanation: format!(
            "Itemstat {prefix_id} ('{prefix_name}') carries no positive attribute multiplier, \
             so the slot-budget model cannot price it. Those rows are flat-value item stat \
             blocks, not gear prefixes; the affected slots contribute nothing."
        ),
    }
}

/// Match a PvE prefix name (e.g. "Berserker's") to a PvP amulet ("Berserker Amulet").
pub fn match_pvp_amulet<'a>(db: &'a GameDb, prefix_name: &str) -> Option<&'a PvpAmulet> {
    let needle = prefix_name.trim_end_matches("'s").trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut best: Option<(&PvpAmulet, usize)> = None;
    for a in db.pvp_amulets.values() {
        let n = a.name.to_lowercase();
        let stem = n.replace(" amulet", "");
        if !(n.contains(&needle) || stem.contains(&needle) || needle.contains(&stem)) {
            continue;
        }
        let dist = n.len().abs_diff(needle.len());
        match best {
            None => best = Some((a, dist)),
            Some((_, d)) if dist < d => best = Some((a, dist)),
            Some((prev, d)) if dist == d && a.id < prev.id => best = Some((a, dist)),
            _ => {}
        }
    }
    best.map(|(a, _)| a)
}

/// User-facing text for a stale trait lock (GLM F31): a `pub const` instead
/// of an inline literal so an addon-side regression test can assert it never
/// regresses into the multi-line-literal-joined-without-rewrapping bug that
/// produced 35-space runs in the rendered explanation.
pub const STALE_TRAIT_LOCK_EXPLANATION: &str = "The locked trait no longer \
    exists in this specialization's trait rows (stale lock after a \
    game-data refresh). The optimizer picked the best available trait in \
    that column instead.";

/// A trait lock referencing an id that no longer exists in the spec's
/// major-trait rows (stale after a game-data refresh) cannot be honored —
/// surface it instead of silently overriding the user's constraint.
fn stale_trait_lock_reasons(
    locks: &gw2_core::types::BuildLocks,
    db: &GameDb,
    ctx: &BalanceContext,
) -> Vec<data::DataQualityReason> {
    let mut reasons = Vec::new();
    let modes = vec![ctx.game_mode.label().to_string()];
    for (spec_id, columns) in &locks.trait_locks {
        let Some(spec) = db.specializations.get(spec_id) else {
            continue; // unknown spec: rejected earlier by validation
        };
        for locked in columns.iter().flatten() {
            if !spec.major_traits.contains(locked) {
                let trait_name = db
                    .traits
                    .get(locked)
                    .map(|t| t.name.as_str())
                    .unwrap_or("unknown trait");
                reasons.push(data::DataQualityReason {
                    field: "trait_lock".into(),
                    entity: format!("{} — trait {}", spec.name, trait_name),
                    modes: modes.clone(),
                    explanation: STALE_TRAIT_LOCK_EXPLANATION.into(),
                });
            }
        }
    }
    reasons
}

pub fn quality_from_modifiers(
    modifiers: &DamageModifiers,
    warnings: &[String],
    has_errors: bool,
    mode: &str,
) -> (data::DataQuality, Vec<data::DataQualityReason>) {
    let mut quality = data::DataQuality::Verified;
    let mut reasons = Vec::new();
    if !warnings.is_empty() {
        quality = quality.merge(&data::DataQuality::Provisional);
        for w in warnings {
            reasons.push(data::DataQualityReason {
                field: "validated_build.warning".into(),
                entity: mode.into(),
                modes: vec![mode.to_string()],
                explanation: w.clone(),
            });
        }
    }
    if has_errors {
        quality = quality.merge(&data::DataQuality::Blocked);
        reasons.push(data::DataQualityReason {
            field: "validated_build.error".into(),
            entity: mode.into(),
            modes: vec![mode.to_string()],
            explanation: "Validation errors present".into(),
        });
    }
    if !modifiers.unparsed.is_empty() {
        quality = quality.merge(&data::DataQuality::Provisional);
        reasons.push(data::DataQualityReason {
            field: "modifiers.unparsed".into(),
            entity: mode.into(),
            modes: vec![mode.to_string()],
            explanation: format!(
                "{} bonus string(s) had % but no known category: {}",
                modifiers.unparsed.len(),
                modifiers
                    .unparsed
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        });
    }
    (quality, reasons)
}

fn push_skill_fact_drop(out: &mut Vec<(&'static str, u32, u32)>, db: &GameDb, id: u32) {
    if let Some(skill) = db.skills.get(&id) {
        out.push(("skill", id, skill.fact_parse_drops));
    }
}

/// Skill and trait ids this score actually reads, with their parse-drop counts.
fn collect_scored_fact_drops(
    db: &GameDb,
    validated: &ValidatedBuild,
    profession_name: &str,
) -> Vec<(&'static str, u32, u32)> {
    let mut out = Vec::new();
    if let Some((id, _)) = validated.skills.heal {
        push_skill_fact_drop(&mut out, db, id);
    }
    for (id, _) in validated.skills.utilities.iter().flatten() {
        push_skill_fact_drop(&mut out, db, *id);
    }
    if let Some((id, _)) = validated.skills.elite {
        push_skill_fact_drop(&mut out, db, id);
    }
    for (id, _) in &validated.skills.profession {
        push_skill_fact_drop(&mut out, db, *id);
    }

    let spec_ids: Vec<u32> = validated
        .specializations
        .iter()
        .map(|s| s.spec_id)
        .collect();
    if validated.skills.profession.is_empty() {
        for (id, _) in rotation::builder::profession_skills_for_build(
            db,
            profession_name,
            &spec_ids,
            &validated.weapons,
        ) {
            push_skill_fact_drop(&mut out, db, id);
        }
    }
    for (id, _) in rotation::builder::form_bar_for_build(db, profession_name, &spec_ids) {
        push_skill_fact_drop(&mut out, db, id);
    }
    if let Some(profession) = db.profession(profession_name) {
        let mut weapon_ids = Vec::new();
        if let Some(ref main) = validated.weapons.set1.main_hand {
            add_weapon_skill_ids(&mut weapon_ids, profession, main, db, Hand::Main);
        }
        if let Some(ref off) = validated.weapons.set1.off_hand {
            add_weapon_skill_ids(&mut weapon_ids, profession, off, db, Hand::Off);
        }
        if let Some(ref main) = validated.weapons.set2.main_hand {
            add_weapon_skill_ids(&mut weapon_ids, profession, main, db, Hand::Main);
        }
        if let Some(ref off) = validated.weapons.set2.off_hand {
            add_weapon_skill_ids(&mut weapon_ids, profession, off, db, Hand::Off);
        }
        for id in weapon_ids {
            push_skill_fact_drop(&mut out, db, id);
        }
    }

    let mut trait_ids: Vec<u32> = validated
        .specializations
        .iter()
        .flat_map(|s| s.all_trait_ids.iter().chain(s.trait_ids.iter()).copied())
        .collect();
    trait_ids.sort_unstable();
    trait_ids.dedup();
    for id in trait_ids {
        let Some(t) = db.traits.get(&id) else {
            continue;
        };
        out.push(("trait", id, t.fact_parse_drops));
        for ts in &t.skills {
            out.push(("skill", ts.id, ts.fact_parse_drops));
        }
    }
    out
}

pub(crate) fn apply_build_fact_parse_drops(
    quality: &mut data::DataQuality,
    reasons: &mut Vec<data::DataQualityReason>,
    db: &GameDb,
    validated: &ValidatedBuild,
    profession_name: &str,
    mode: &str,
) {
    data::quality::append_fact_parse_drops(
        quality,
        reasons,
        collect_scored_fact_drops(db, validated, profession_name),
        mode,
    );
}

fn append_trait_fact_parse_drops(
    quality: &mut data::DataQuality,
    reasons: &mut Vec<data::DataQualityReason>,
    trait_ids: &[u32],
    traits_cache: &HashMap<u32, GW2Trait>,
    mode: &str,
) {
    let mut drops = Vec::new();
    for id in trait_ids {
        let Some(t) = traits_cache.get(id) else {
            continue;
        };
        drops.push(("trait", *id, t.fact_parse_drops));
        for ts in &t.skills {
            drops.push(("skill", ts.id, ts.fact_parse_drops));
        }
    }
    data::quality::append_fact_parse_drops(quality, reasons, drops, mode);
}

/// Add stat values from a slot budget entry, classifying each itemstat
/// attribute as major or minor by its multiplier relative to the highest
/// *positive* multiplier in the set.
///
/// Returns `true` when the row was priced. A row the budget model cannot price
/// contributes nothing and says so, so a caller that can report data quality
/// does not have to re-derive the reason.
///
/// "Highest positive" is the whole fix. The old reading took the plain maximum,
/// which for the legacy 1041-1052 band is `0.0` — and then every attribute
/// satisfied `(m - max).abs() < 0.001` and every attribute was paid the
/// **major** rate. Berserker's #1046 came out at 1507/1507/1507 = 4521 points
/// against the real #161's 1507/1050/1050 = 3607, strictly dominating on every
/// axis, so a search that could see it had to prefer it — and then served a
/// build labelled "Berserker's" whose sheet the player can never equip.
///
/// Those rows are not prefixes at all: their multipliers are `0.0` and their
/// numbers live in the flat `value` field, i.e. a fixed item-level stat block
/// rather than a share of a slot budget. Paying `value` here instead would be
/// just as wrong, because `value` is one item's contribution and this function
/// is called once per equipment slot. So the honest answer is to price nothing
/// and let [`crate::itemstat_pool::canonical_itemstats`] keep such rows out of
/// the prefix pool in the first place.
///
/// For CelestialLike rows every multiplier is equal *and positive*, so all
/// attributes are majors — and major == minor in that budget anyway.
pub fn add_budget_stats_for_itemstat(
    stats: &mut stats::StatBlock,
    itemstat: &ItemStat,
    budget: &data::slot_budgets::SlotBudgetEntry,
) -> bool {
    let Some(max_mult) = crate::itemstat_pool::max_positive_multiplier(itemstat) else {
        return false;
    };
    for attr in &itemstat.attributes {
        // "Major" is the highest multiplier, within a tolerance that absorbs
        // the float noise in the published table (0.35 vs 0.3500000001).
        let value = if (attr.multiplier - max_mult).abs() < 0.001 {
            budget.major as f64
        } else {
            budget.minor as f64
        };
        stats.add(&attr.attribute, value);
    }
    true
}

/// Select the best major trait from each column (Adept/Master/Grandmaster) for an archetype.
/// GW2 specialization major_traits layout: [A1, A2, A3, M1, M2, M3, G1, G2, G3]
/// Each column has 3 choices; the player picks 1 per column = 3 total.
/// This heuristic scores each trait's stat contributions + damage modifier relevance
/// against the archetype weights and picks the best per column.
fn select_best_major_traits(
    major_traits: &[u32],
    stat_weights: &StatWeights,
    traits_cache: &HashMap<u32, GW2Trait>,
    locks: &gw2_core::types::BuildLocks,
    spec_id: u32,
) -> Vec<u32> {
    if major_traits.len() != 9 {
        // Unexpected layout — return all as fallback (some specs may have fewer)
        return major_traits.to_vec();
    }

    let weights = stat_weights;
    let mut selected = Vec::with_capacity(3);
    let trait_lock = locks.trait_locks.get(&spec_id);

    // Process 3 columns: [0..3], [3..6], [6..9]
    for (col_idx, col_start) in (0..9).step_by(3).enumerate() {
        let column = &major_traits[col_start..col_start + 3];

        if let Some(locked_id) = trait_lock.and_then(|t| t[col_idx]) {
            if column.contains(&locked_id) {
                selected.push(locked_id);
                continue;
            }
        }

        let mut best_id = column[0];
        let mut best_score = f64::NEG_INFINITY;

        for &trait_id in column {
            let score = score_trait_for_archetype(trait_id, weights, traits_cache);
            if score > best_score {
                best_score = score;
                best_id = trait_id;
            }
        }
        selected.push(best_id);
    }

    selected
}

/// Score a single trait's relevance for an archetype by examining its facts.
/// Looks at AttributeAdjust (flat stat bonuses) and Percent (damage modifiers).
pub fn score_trait_for_archetype(
    trait_id: u32,
    weights: &crate::scoring::StatWeights,
    traits_cache: &HashMap<u32, GW2Trait>,
) -> f64 {
    let Some(t) = traits_cache.get(&trait_id) else {
        return 0.0;
    };

    let mut score = 0.0;

    for fact in &t.facts {
        score += score_fact(fact, weights);
    }

    // Score traited_facts — these activate when a specific other trait is equipped.
    // If the requiring trait is from the same spec, it's likely co-selected (80% credit).
    // If from a different spec, it's uncertain (30% credit).
    for tf in &t.traited_facts {
        let same_spec = traits_cache
            .get(&tf.requires_trait)
            .map(|rt| rt.specialization == t.specialization)
            .unwrap_or(false);
        let credit = if same_spec { 0.8 } else { 0.3 };
        score += score_fact(&tf.fact, weights) * credit;
    }

    score
}

/// Score a single fact's contribution to an archetype.
fn score_fact(fact: &Fact, weights: &crate::scoring::StatWeights) -> f64 {
    match fact {
        Fact::AttributeAdjust {
            text,
            value: Some(val),
            target: Some(ref target),
            ..
        } => {
            if !crate::stats::is_permanent_stat_adjust(text.as_deref()) {
                return 0.0;
            }
            let w = match target.as_str() {
                "Power" => weights.power,
                "Precision" => weights.precision,
                "Toughness" => weights.toughness,
                "Vitality" => weights.vitality,
                "ConditionDamage" => weights.condition_damage,
                "ConditionDuration" | "Expertise" => weights.expertise,
                "BoonDuration" | "Concentration" => weights.concentration,
                "CritDamage" | "Ferocity" => weights.ferocity,
                "Healing" | "HealingPower" => weights.healing_power,
                _ => 0.0,
            };
            // Normalize: +100 stat with weight 1.0 → 0.033 (similar to stat scoring)
            (*val as f64) / 3000.0 * w
        }
        Fact::Percent {
            text: Some(ref text),
            percent: Some(pct),
            ..
        } => {
            let text_lower = text.to_lowercase();
            // Damage-related percent modifiers are highly valuable for DPS archetypes
            if text_lower.contains("damage") {
                let dps_weight = (weights.power + weights.condition_damage) / 2.0;
                pct / 100.0 * dps_weight
            } else if text_lower.contains("critical") {
                pct / 100.0 * weights.ferocity.max(weights.precision)
            } else if text_lower.contains("healing") {
                pct / 100.0 * weights.healing_power
            } else if text_lower.contains("boon duration") {
                pct / 100.0 * weights.concentration
            } else if text_lower.contains("condition duration") {
                pct / 100.0 * weights.expertise
            } else {
                0.0
            }
        }
        Fact::BuffConversion {
            percent: Some(pct),
            source: Some(ref src),
            target: Some(ref tgt),
            ..
        } => {
            // Conversion is valuable if source stat is high for this archetype
            // and target stat is also weighted
            let src_w = match src.as_str() {
                "Power" => weights.power,
                "Precision" => weights.precision,
                "Toughness" => weights.toughness,
                "Vitality" => weights.vitality,
                "ConditionDamage" => weights.condition_damage,
                "Ferocity" => weights.ferocity,
                _ => 0.0,
            };
            let tgt_w = match tgt.as_str() {
                "Power" => weights.power,
                "Precision" => weights.precision,
                "Toughness" => weights.toughness,
                "Vitality" => weights.vitality,
                "ConditionDamage" => weights.condition_damage,
                "Ferocity" => weights.ferocity,
                "Healing" | "HealingPower" => weights.healing_power,
                _ => 0.0,
            };
            pct / 100.0 * src_w * tgt_w
        }
        _ => 0.0,
    }
}

/// Result of the synergy-driven optimization pipeline.
/// Contains a fully validated build with pre-computed combat metrics at 3 buff tiers.
#[derive(Debug, Clone)]
pub struct SynergyResult {
    pub validated: ValidatedBuild,
    pub stats: stats::StatBlock,
    pub combat_solo: CombatPerformance,
    pub combat_party: CombatPerformance,
    pub combat_squad: CombatPerformance,
    pub modifiers: DamageModifiers,
    pub rotation: Option<rotation::SimulationResult>,
    /// Overall data quality assessment for this result's inputs.
    pub data_quality: data::DataQuality,
    /// Reasons for any data quality degradation.
    pub quality_reasons: Vec<data::DataQualityReason>,
}

/// Stage 3 of the Gemini pipeline: assemble the final synergy prompt
/// (applies user-imposed spec/trait lock constraints).
// Prompt-assembly stage; each argument is independent prompt input, grouping
// them into a struct would only rename fields, not reduce coupling.
/// Stage 4 of the Gemini pipeline: call the LLM with tool definitions and
/// multi-turn progress reporting. Tool candidates are empty — the LLM is
/// choosing the build, not ranking candidates.
// LLM-call stage; the client, context, db, and progress callback are distinct
// dependencies passed straight through — a params struct adds no clarity.
/// Run the synergy-driven optimization pipeline.
/// Sends ALL profession data to Gemini in a single prompt for holistic synergy reasoning.
/// Returns a fully validated build with combat metrics at 3 buff tiers.
// Gemini pipeline entry point; arguments are the db, weights, balance context,
// LLM client, and callbacks — grouping them adds indirection without clarity.
/// Calculate stats from a validated build: gear prefix + trait bonuses + conversions.
pub fn calculate_validated_stats(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    ctx: &BalanceContext,
) -> (stats::StatBlock, DamageModifiers) {
    let mut full_stats = stats::base_stats();

    // Reasons are dropped here on purpose — the signature is fixed by callers
    // outside this module. `gear_quality_reasons` re-runs the same applier for
    // the paths that report them.
    apply_validated_gear_stats(&mut full_stats, db, validated, profession_name, ctx);

    // Rune and sigil flat stat bonuses (permanent stats only).
    let rune_id = validated.rune.as_ref().map(|r| r.id);
    let active_sigil_ids = validated.active_sigil_ids();
    let active_sigil_ids = &active_sigil_ids[..];
    let rune_stats = stats::calculate_rune_stats(rune_id, &db.items);
    full_stats += &rune_stats;
    let sigil_stats = stats::calculate_sigil_stats(active_sigil_ids, &db.items);
    full_stats += &sigil_stats;

    // Collect all trait IDs from validated specializations
    let all_trait_ids: Vec<u32> = validated
        .specializations
        .iter()
        .flat_map(|s| s.all_trait_ids.iter().copied())
        .collect();

    // Trait flats, then food/utility flats, then every conversion from that
    // one sheet. Wiki Gain X Based on Y (read 2026-09-25): conversion output
    // is not an input to any other conversion, and flat food/utility bonuses
    // are part of the base.
    let trait_stats =
        stats::calculate_trait_stats_for_mode(&all_trait_ids, &db.traits, &ctx.game_mode);
    full_stats += &trait_stats;

    // Extract damage modifiers from traits + rune + sigils + relic
    let relic_id = validated.relic.as_ref().map(|r| r.id);
    let mut modifiers = combat::extract_damage_modifiers(
        &all_trait_ids,
        rune_id,
        active_sigil_ids,
        relic_id,
        &db.traits,
        &db.items,
        ctx,
    );
    combat::scope_skill_damage(&mut modifiers, &db.traits, &db.skills);
    crate::consumables::fold_standing_flats(&mut full_stats, &mut modifiers, validated, db);
    let conversion_base = full_stats.clone();
    stats::apply_trait_conversions(&mut full_stats, &all_trait_ids, &db.traits);
    crate::consumables::fold_stat_conversions(&mut full_stats, validated, db, &conversion_base);
    // ponytail: infusions stay outside `conversion_base`. The wiki list names
    // gear, not infusions; move them into the snapshot if a sheet shows they convert.
    crate::infusions::fold_into_validated_stats(&mut full_stats, validated, db);

    (full_stats, modifiers)
}

/// Simulate a rotation from validated skill IDs.
/// A validated build's skills resolved into simulator inputs, so the gate
/// simulation and the flow simulation share one (expensive) fact parse.
pub struct PreparedRotation {
    pub skills: Vec<rotation::RotationSkill>,
    pub params: rotation::simulator::SimParams,
    /// Skill ids in the order a published page says to press them. Empty
    /// means the timeline improvises from the first tick.
    pub opener: Vec<u32>,
    /// Health-gated strike clauses the parser flattened into
    /// `params.strike_mult`; the WvW timeline divides out the ones whose
    /// threshold record it executes (R4).
    pub conditional_strike: Vec<combat::ConditionalClause>,
    /// Target-conditional percents (Phase 3); also on `params.deferred_target`.
    pub deferred_target: Vec<combat::DeferredTargetModifier>,
    /// Traits the parser consumed a fact from (US4): executed from facts, so
    /// the coverage line never names them.
    pub consumed_trait_ids: Vec<u32>,
    /// Each trait's share of the always-on percents in `params` (E1).
    pub trait_standing: Vec<combat::TraitStanding>,
    profession_name: String,
    /// Flow-sim records prepare could not host.
    unhosted: Vec<String>,
    /// Equipped sources have a record in this mode's effect file.
    inventory_due: bool,
    /// Description-fallback Barrier/Healing names, already rendered.
    heuristic_coverage: Vec<String>,
}

/// Window for the flow simulation. Long enough that a burst-then-nothing kit
/// averages below one that keeps its output up; the gate simulation keeps the
/// short mode/tier window from `simulation_window_ms_for_mode`.
pub const FLOW_WINDOW_MS: u32 = 60_000;

pub fn prepare_validated_rotation(
    validated: &ValidatedBuild,
    db: &GameDb,
    stats: &stats::StatBlock,
    scenario: Option<&crate::scenario::ScenarioSpec>,
) -> Option<PreparedRotation> {
    // Heal/utility/elite stay at weapon_set 0 (always available); weapon skills
    // get tagged with their actual set 1 or 2 so the simulator's weapon-swap
    // logic in `is_skill_available` and `should_weapon_swap` can decide when to
    // swap. Previously all skills defaulted to set 0, making the simulator
    // treat both weapon sets as simultaneously available — no swap, set 2
    // skills usable while set 1 active.
    let mut non_weapon_ids: Vec<u32> = Vec::new();

    if let Some((id, _)) = &validated.skills.heal {
        non_weapon_ids.push(*id);
    }
    for (id, _) in validated.skills.utilities.iter().flatten() {
        // One copy per skill: a doubled utility would get two cooldown
        // states and count twice.
        if !non_weapon_ids.contains(id) {
            non_weapon_ids.push(*id);
        }
    }
    if let Some((id, _)) = &validated.skills.elite {
        non_weapon_ids.push(*id);
    }

    // Resolve weapon skills from validated weapon types
    let profession_name = if let Some(spec) = validated.specializations.first() {
        // Use the profession from the spec
        db.specializations
            .get(&spec.spec_id)
            .map(|s| s.profession.as_str())
            .unwrap_or("")
    } else {
        ""
    };

    let equipped_spec_ids: Vec<u32> = validated
        .specializations
        .iter()
        .map(|spec| spec.spec_id)
        .collect();
    let resolved_profession_skills;
    let profession_skills = if validated.skills.profession.is_empty() {
        resolved_profession_skills = rotation::builder::profession_skills_for_build(
            db,
            profession_name,
            &equipped_spec_ids,
            &validated.weapons,
        );
        &resolved_profession_skills
    } else {
        &validated.skills.profession
    };
    non_weapon_ids.extend(profession_skills.iter().map(|(id, _)| *id));
    // The form bar (shroud, Celestial Avatar): held only in the form
    // (`SHROUD_SET`).
    let shroud_ids: Vec<u32> =
        rotation::builder::form_bar_for_build(db, profession_name, &equipped_spec_ids)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| !non_weapon_ids.contains(id))
            .collect();

    let mut set1_ids: Vec<u32> = Vec::new();
    let mut set2_ids: Vec<u32> = Vec::new();

    // Find weapon skills for each weapon set. `db.profession(name)` is an O(1)
    // HashMap lookup keyed on id (which equals the name for GW2 professions).
    if let Some(profession) = db.profession(profession_name) {
        if let Some(ref main) = validated.weapons.set1.main_hand {
            add_weapon_skill_ids(&mut set1_ids, profession, main, db, Hand::Main);
        }
        if let Some(ref off) = validated.weapons.set1.off_hand {
            add_weapon_skill_ids(&mut set1_ids, profession, off, db, Hand::Off);
        }
        if let Some(ref main) = validated.weapons.set2.main_hand {
            add_weapon_skill_ids(&mut set2_ids, profession, main, db, Hand::Main);
        }
        if let Some(ref off) = validated.weapons.set2.off_hand {
            add_weapon_skill_ids(&mut set2_ids, profession, off, db, Hand::Off);
        }
    }

    if non_weapon_ids.is_empty() && set1_ids.is_empty() && set2_ids.is_empty() {
        return None;
    }

    let mode = scenario
        .map(|s| s.game_mode.clone())
        .unwrap_or(GameMode::PvE);
    let sim_ctx = BalanceContext::new(mode.clone());

    let mut rotation_skills =
        rotation::builder::build_rotation_skills_for_context(&non_weapon_ids, db, &sim_ctx);
    let mut set1_skills =
        rotation::builder::build_rotation_skills_for_context(&set1_ids, db, &sim_ctx);
    rotation::builder::tag_weapon_set(&mut set1_skills, 1);
    let mut set2_skills =
        rotation::builder::build_rotation_skills_for_context(&set2_ids, db, &sim_ctx);
    rotation::builder::tag_weapon_set(&mut set2_skills, 2);
    rotation_skills.extend(rotation::builder::merge_weapon_sets(
        set1_skills,
        set2_skills,
    ));
    let mut shroud_skills =
        rotation::builder::build_rotation_skills_for_context(&shroud_ids, db, &sim_ctx);
    for skill in &mut shroud_skills {
        skill.weapon_set = rotation::SHROUD_SET;
        if let Some(slot) = rotation::builder::form_bar_slot(skill.slot_name.as_deref()) {
            skill.slot = slot;
        }
    }
    rotation_skills.extend(shroud_skills);
    let ne = crate::data::normalized_effects::effects().effects_for_mode(mode.label());
    // Traited cleanses (Cleansing Ire bursts, Restorative Illusions shatters)
    // count only when the build runs the trait.
    let equipped_traits: Vec<u32> = validated
        .specializations
        .iter()
        .flat_map(|s| s.all_trait_ids.iter().chain(s.trait_ids.iter()).copied())
        .collect();
    let (_, mods) = calculate_validated_stats(validated, db, profession_name, &sim_ctx);
    // The equipped traits' skill facts, then the trait increases the API
    // scopes to named skills (never both for one trait: see
    // `builder::active_skill_facts`).
    rotation::builder::apply_traited_facts(&mut rotation_skills, db, &sim_ctx, &equipped_traits);
    rotation::builder::apply_skill_strike(&mut rotation_skills, &mods.skill_strike);
    rotation::builder::enrich_with_cleanse(&mut rotation_skills, ne, db, &equipped_traits);

    if rotation_skills.is_empty() {
        return None;
    }

    let power = stats.get("Power");
    let condition_damage = stats.get("ConditionDamage");
    let precision = stats.get("Precision");
    let ferocity = stats.get("Ferocity") + mods.total_crit_damage_bonus() * 15.0;
    let expertise = stats.get("Expertise");
    let concentration = stats.get("Concentration");
    let healing_power = stats.get("HealingPower");
    let weapon_strength = 1100.0;
    let derived = stats::compute_derived(stats, profession_name);
    // An unmodelled form abstains here (its bar stays stowed) and is named
    // on the WvW resource gap line, as are the records the flow cannot play.
    let form = form_for_build(validated, &rotation_skills, db, &mode, derived.health)
        .ok()
        .flatten();
    let procs = trait_procs_for_build(
        validated,
        db,
        &mode,
        &rotation_skills,
        form.as_ref(),
        &mods.trait_standing,
    );
    let inventory_due = coverage_inventory_due(validated, &rotation_skills, &mode);
    let heuristic_coverage =
        rotation::builder::heuristic_coverage_stamps(&rotation_skills, db, &equipped_traits);
    let params = rotation::simulator::SimParams {
        power,
        condition_damage,
        weapon_strength,
        precision,
        ferocity,
        crit_chance_bonus: mods.total_crit_chance_bonus(),
        fury_crit_chance_bonus: crate::data::boon_condition_formulas::boons()
            .fury_crit_bonus(mode.clone())
            * 100.0,
        strike_mult: mods.total_strike_mult(),
        condition_mult: mods.total_condi_mult(),
        condition_duration_mult: combat::outgoing_condition_duration_mult(
            expertise, &mods, &sim_ctx,
        ),
        boon_duration_mult: combat::outgoing_boon_duration_mult(concentration, &mods, &sim_ctx),
        healing_power,
        healing_mult: mods.total_healing_mult(),
        max_health: derived.health,
        armor: derived.armor,
        mode: mode.clone(),
        intent: None,
        deferred_target: mods.deferred_target.clone(),
        weaver: equipped_spec_ids.contains(&rotation::attunement::WEAVER_SPEC_ID),
        form,
        triggered: procs.triggered,
        strike_add: mods.strike_add_pct.iter().sum(),
        condition_add: mods.condition_add_pct.iter().sum(),
        condition_type_mults: mods.specific_condi_mults(),
        folded: procs.folded,
    };

    Some(PreparedRotation {
        opener: Vec::new(),
        conditional_strike: mods.conditional_strike.clone(),
        deferred_target: mods.deferred_target.clone(),
        consumed_trait_ids: mods.consumed_trait_ids.clone(),
        trait_standing: mods.trait_standing.clone(),
        skills: rotation_skills,
        params,
        profession_name: profession_name.to_string(),
        unhosted: procs.unhosted,
        inventory_due,
        heuristic_coverage,
    })
}

pub fn simulate_validated_rotation(
    validated: &ValidatedBuild,
    db: &GameDb,
    stats: &stats::StatBlock,
    scenario: Option<&crate::scenario::ScenarioSpec>,
) -> Option<rotation::SimulationResult> {
    let prepared = prepare_validated_rotation(validated, db, stats, scenario)?;
    Some(simulate_prepared(&prepared, validated, db, scenario))
}

/// Gate simulation: the short mode/tier window on the scenario's dummy, plus
/// the counterplay-aware WvW timeline. Feeds the viability gates.
pub fn simulate_prepared(
    prepared: &PreparedRotation,
    validated: &ValidatedBuild,
    db: &GameDb,
    scenario: Option<&crate::scenario::ScenarioSpec>,
) -> rotation::SimulationResult {
    simulate_prepared_with(prepared, validated, db, scenario, false)
}

/// [`simulate_prepared`] with the WvW event trace switched on. Experiments
/// only; production never traces.
#[cfg(test)]
pub(crate) fn simulate_prepared_traced(
    prepared: &PreparedRotation,
    validated: &ValidatedBuild,
    db: &GameDb,
    scenario: Option<&crate::scenario::ScenarioSpec>,
) -> rotation::SimulationResult {
    simulate_prepared_with(prepared, validated, db, scenario, true)
}

fn simulate_prepared_with(
    prepared: &PreparedRotation,
    validated: &ValidatedBuild,
    db: &GameDb,
    scenario: Option<&crate::scenario::ScenarioSpec>,
    trace: bool,
) -> rotation::SimulationResult {
    let rotation_skills = &prepared.skills;
    let params = &prepared.params;
    let profession_name = prepared.profession_name.as_str();
    let mode = params.mode.clone();
    let sim_ctx = BalanceContext::new(mode.clone());
    let duration_ms = scenario
        .map(|s| {
            crate::rotation::combat_model::simulation_window_ms_for_mode(
                &s.game_mode,
                s.combat_tier,
                s.combat_kind,
            )
        })
        .unwrap_or(0);

    let enemy = scenario
        .map(|s| {
            crate::rotation::combat_model::EnemyDummy::for_scenario(
                &s.game_mode,
                s.combat_tier,
                s.combat_kind,
            )
        })
        .unwrap_or_default();

    let mut result =
        rotation::simulator::simulate_with(rotation_skills, duration_ms, params, enemy);

    if let Some(scenario) = scenario.filter(|scenario| scenario.game_mode == GameMode::WvW) {
        // Executed from facts (US4): a percent modifier the damage parser
        // consumed, or an attribute fact the stat sheet consumed
        // (`stats::calculate_trait_stats_for_mode` reads every
        // AttributeAdjust / BuffConversion fact of every equipped trait).
        let mut executed_traits: std::collections::HashSet<u32> =
            prepared.consumed_trait_ids.iter().copied().collect();
        executed_traits.extend(stat_consumed_trait_ids(validated, db));
        let (active_effects, coverage, sigil_sets) = active_normalized_effects(
            validated,
            rotation_skills,
            db,
            crate::data::normalized_effects::effects().effects_for_mode(mode.label()),
            &executed_traits,
        );
        let (resource_rules, resource_model_complete, resource_model_gaps) = wvw_resource_rules(
            validated,
            rotation_skills,
            db,
            profession_name,
            &sim_ctx,
            params.max_health,
        );
        let wvw_params = wvw_params_without_executed_conditionals(
            params,
            &prepared.conditional_strike,
            &prepared.trait_standing,
            &active_effects,
        );
        result.wvw = Some(rotation::wvw_timeline::evaluate_wvw_timeline(
            rotation::wvw_timeline::WvwTimelineInput {
                skills: rotation_skills,
                opener: &prepared.opener,
                duration_ms,
                params: &wvw_params,
                enemy,
                scenario,
                active_effects: &active_effects,
                resource_rules: &resource_rules,
                resource_model_complete,
                resource_model_gaps,
                profession: profession_name.to_string(),
                coverage,
                population: crate::data::fight_population::FightPopulation::for_tier(
                    scenario.combat_tier,
                ),
                sigil_sets,
                weapon_swap_cooldown_ms: wvw_weapon_swap_cooldown_ms(profession_name, validated),
                equipped_weapons: equipped_weapons(validated, db.professions.get(profession_name)),
                trace,
            },
        ));
    }

    result.honesty.unhosted = prepared.unhosted.clone();
    result.honesty.heuristic = prepared.heuristic_coverage.clone();
    result.honesty.inventory_skipped = prepared.inventory_due && result.wvw.is_none();
    result
}

/// The build's weapons as the timeline's `Gate::Weapon` reads them: one row
/// per filled hand, keyed by set. A two-hander fills the main-hand cell and
/// is reported as [`WeaponHand::TwoHand`], which is how the wiki words the
/// traits that gate on it.
///
/// Two-handedness is the profession's own answer (`weapon_budget`), not a
/// global type list: Bladesworn's Sword is held in both hands and core
/// Warrior's is not.
pub(crate) fn equipped_weapons(
    validated: &ValidatedBuild,
    profession: Option<&gw2_api::models::Profession>,
) -> Vec<crate::rotation::wvw_timeline::EquippedWeapon> {
    use crate::data::normalized_effects::WeaponHand;
    use crate::rotation::wvw_timeline::EquippedWeapon;

    let mut out = Vec::new();
    for (set, weapons) in [
        (1u8, &validated.weapons.set1),
        (2u8, &validated.weapons.set2),
    ] {
        for (raw, off_hand) in [(&weapons.main_hand, false), (&weapons.off_hand, true)] {
            let Some(name) = raw.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
                continue;
            };
            let hand = if off_hand {
                WeaponHand::Off
            } else if crate::weapon_budget::is_two_handed(name, profession) {
                WeaponHand::TwoHand
            } else {
                WeaponHand::Main
            };
            out.push(EquippedWeapon {
                set,
                hand,
                weapon_type: gw2_core::i18n::weapon_type_key(name),
            });
        }
    }
    out
}

/// The timeline's parameters: `params` with each flattened health-gated
/// clause divided out of `strike_mult` when the timeline will execute that
/// source's threshold record per strike, so the bonus is counted once
/// (CONN-01-01). A clause whose record is absent or unresolved stays
/// flattened, exactly as every other path sees it.
///
/// Likewise a trait's parser-folded strike, crit chance and crit damage
/// share is removed when the timeline runs that trait's own Conditional
/// record of the same category (review E1).
fn wvw_params_without_executed_conditionals(
    params: &rotation::simulator::SimParams,
    clauses: &[combat::ConditionalClause],
    trait_standing: &[combat::TraitStanding],
    active_effects: &[&crate::data::normalized_effects::NormalizedEffect],
) -> rotation::simulator::SimParams {
    use crate::data::normalized_effects::{EffectCategory, SourceType, TriggerRule};
    let mut out = params.clone();
    for clause in clauses {
        let executed = active_effects.iter().any(|effect| {
            effect.source_id == clause.source_id
                && effect.trigger_rule == TriggerRule::OnHealthThreshold
                && effect
                    .health_threshold
                    .as_ref()
                    .is_some_and(|t| t.percent.is_resolved())
                && effect.value.is_resolved()
        });
        if executed {
            out.strike_mult /= 1.0 + clause.value;
        }
    }
    // Mirrors the timeline's Conditional ConditionalSpec load path.
    let runs_conditional = |trait_id: u32, category: EffectCategory| {
        active_effects.iter().any(|effect| {
            effect.source_type == SourceType::Trait
                && effect.source_id == trait_id
                && effect.trigger_rule == TriggerRule::Conditional
                && (effect.category == category || effect.inner_category == Some(category.clone()))
                && effect
                    .prerequisite
                    .as_ref()
                    .is_some_and(|p| p.in_shroud != Some(false))
                && effect.value.is_resolved()
                && effect.max_stacks.as_ref().is_none_or(|m| m.is_resolved())
                && rotation::wvw_timeline::unexecutable_reason(effect).is_none()
        })
    };
    for standing in trait_standing {
        if runs_conditional(standing.trait_id, EffectCategory::StrikeDamagePct) {
            for m in &standing.strike_pct {
                out.strike_mult /= 1.0 + m;
            }
        }
        if runs_conditional(standing.trait_id, EffectCategory::CritChancePct) {
            out.crit_chance_bonus -= standing.crit_chance_pct;
        }
        if runs_conditional(standing.trait_id, EffectCategory::CritDamagePct) {
            out.ferocity -= standing.crit_damage_pct * 15.0;
        }
    }
    out
}

/// Flow simulation: `FLOW_WINDOW_MS` on the scenario's dummy with no
/// downstate, scheduled toward the user's radar weights. This is what the
/// rank scores (`scoring::realized_axes`): a bar with an empty slot now
/// produces less than a full one in every mode.
pub fn simulate_flow(
    prepared: &PreparedRotation,
    weights: &OptimizationWeights,
    scenario: Option<&crate::scenario::ScenarioSpec>,
) -> rotation::SimulationResult {
    let mut params = prepared.params.clone();
    params.intent = Some(weights.clamped());
    let mut enemy = scenario
        .map(|s| {
            crate::rotation::combat_model::EnemyDummy::for_scenario(
                &s.game_mode,
                s.combat_tier,
                s.combat_kind,
            )
        })
        .unwrap_or_default();
    enemy.hp = None;
    rotation::simulator::simulate_with(&prepared.skills, FLOW_WINDOW_MS, &params, enemy)
}

/// Closed-form Solo / Party / Squad combat for one stat sheet.
///
/// The combat half of [`measure_plated`]. Call sites that only have a sheet
/// (not a plate) use this so they do not grow a second
/// [`combat::calculate_combat_performance`] loop.
pub fn combat_tiers(
    stats: &stats::StatBlock,
    derived: &stats::DerivedStats,
    modifiers: &DamageModifiers,
    profession: &str,
    ctx: &BalanceContext,
) -> [CombatPerformance; 3] {
    let profiles = combat::buff_profiles_for_profession(profession, ctx);
    let condition_weights = combat::condition_weights_for_profession(profession, ctx);
    let tier = |index: usize| {
        combat::calculate_combat_performance(
            stats,
            derived,
            modifiers,
            &profiles[index],
            &condition_weights,
            profession,
            ctx,
        )
    };
    // `buff_profiles_for_profession` always returns three profiles.
    [tier(0), tier(1), tier(2)]
}

/// Plated-build combat and flow, once.
///
/// Stat sheet, three closed-form tiers, the 60 s flow run, and the gate
/// run's control lines. Optimizer suggestions, Generations open, Saves, and
/// Stats read this (the addon wrapper is `measure_validated`).
#[derive(Debug, Clone)]
pub struct PlatedMeasure {
    pub stats: stats::StatBlock,
    pub modifiers: DamageModifiers,
    pub combat_solo: CombatPerformance,
    pub combat_party: CombatPerformance,
    pub combat_squad: CombatPerformance,
    /// `None` when the bar resolved to no skills.
    pub flow: Option<rotation::SimulationResult>,
    /// Gate-window run. Stunbreak, stability, and cleanse on the displayed
    /// rotation come from here, beside the viability verdict.
    pub gate: Option<rotation::SimulationResult>,
}

/// Measure one validated plate. Display paths call this instead of rolling
/// their own combat or flow.
pub fn measure_plated(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &crate::scenario::ScenarioSpec,
) -> PlatedMeasure {
    let (stats, modifiers) = calculate_validated_stats(validated, db, profession_name, ctx);
    let derived = stats::compute_derived(&stats, profession_name);
    let [combat_solo, combat_party, combat_squad] =
        combat_tiers(&stats, &derived, &modifiers, profession_name, ctx);
    let flow = simulate_validated_flow(validated, db, profession_name, weights, ctx, scenario);
    let gate = simulate_validated_rotation(validated, db, &stats, Some(scenario));
    PlatedMeasure {
        stats,
        modifiers,
        combat_solo,
        combat_party,
        combat_squad,
        flow,
        gate,
    }
}

/// The flow run the referee scores a validated build on, for display: the
/// stat sheet from [`calculate_validated_stats`], then
/// [`prepare_validated_rotation`] -> [`simulate_flow`], exactly as
/// `referee::evaluate_inner` and `fidelity::compare` run it. Every panel's
/// Simulated DPS and Skill Usage read this, so no caller builds its own
/// `SimParams`. `None`: the bar resolved to no skills.
pub fn simulate_validated_flow(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &crate::scenario::ScenarioSpec,
) -> Option<rotation::SimulationResult> {
    let (stats, _) = calculate_validated_stats(validated, db, profession_name, ctx);
    let prepared = prepare_validated_rotation(validated, db, &stats, Some(scenario))?;
    Some(simulate_flow(&prepared, weights, Some(scenario)))
}

/// Equipment-budget slot name → the GearSlot whose prefix pays for it.
/// WeaponB1/B2 are inactive-set weapon slots — they never draw budgets.
fn gear_slot_for_budget_slot(slot_name: &str) -> Option<GearSlot> {
    Some(match slot_name {
        "Helm" => GearSlot::Helm,
        "Shoulders" => GearSlot::Shoulders,
        "Coat" => GearSlot::Coat,
        "Gloves" => GearSlot::Gloves,
        "Leggings" => GearSlot::Leggings,
        "Boots" => GearSlot::Boots,
        "WeaponA1" => GearSlot::WeaponSet1Main,
        "WeaponA2" => GearSlot::WeaponSet1Off,
        "Backpack" => GearSlot::Back,
        "Accessory1" => GearSlot::Accessory1,
        "Accessory2" => GearSlot::Accessory2,
        "Amulet" => GearSlot::Amulet,
        "Ring1" => GearSlot::Ring1,
        "Ring2" => GearSlot::Ring2,
        _ => return None,
    })
}

/// Per-slot gear stats for a validated build. Returns every reason a slot could
/// not be priced — an empty Vec means the sheet is complete.
fn apply_validated_gear_stats(
    stats: &mut stats::StatBlock,
    db: &GameDb,
    validated: &ValidatedBuild,
    profession_name: &str,
    ctx: &BalanceContext,
) -> Vec<data::DataQualityReason> {
    if ctx.game_mode == GameMode::PvP {
        // Amulets replace gear; match by the build's primary prefix name.
        let fallback = validated.primary_prefix().map(|prefix| prefix.itemstat_id);
        return apply_optimized_gear_stats(stats, db, fallback, ctx)
            .into_iter()
            .collect();
    }

    // Per-slot reads replace the old `group.or(build-wide)` chain: every
    // constructor expands its prefixes into the slots the build actually wears
    // (`fill_worn_gear_slots`; group overrides overwrite their own members), so
    // an unset slot means exactly what a missing group AND missing fallback
    // meant before — plus, now, a hand that holds no weapon.
    let mut reasons = Vec::new();
    let budgets = data::slot_budgets::slot_budgets();
    let set1 = &validated.weapons.set1;
    let weapons = weapon_budget::land_weapon_budget(
        set1.main_hand.as_deref(),
        set1.off_hand.as_deref(),
        db.profession(profession_name),
    );
    for &(slot_type, slot_name) in data::EQUIPMENT_SLOTS {
        let slot_type = if slot_name.starts_with("Weapon") {
            match land_weapon_slot_type(slot_name, set1, weapons) {
                Some(kind) => kind,
                None => continue,
            }
        } else {
            slot_type
        };
        let Some(slot) = gear_slot_for_budget_slot(slot_name) else {
            continue;
        };
        let Some(prefix) = validated.gear_slots.get(slot) else {
            continue;
        };
        let Some(itemstat) = db.itemstats.get(&prefix.itemstat_id) else {
            // An id that resolves to nothing used to be a silent `continue`,
            // which shipped a zeroed slot as if it were a real one. The
            // `itemstat_id: 0` that `GearSlots::from_legacy` stamps lands here.
            reasons.push(unpriceable_prefix_reason(
                &prefix.name,
                prefix.itemstat_id,
                ctx,
            ));
            continue;
        };
        let shape = data::stat_shape_from_attr_count(itemstat.attributes.len());
        if let Some(budget) = budgets.get(slot_type, shape) {
            if !add_budget_stats_for_itemstat(stats, itemstat, budget) {
                reasons.push(unpriceable_prefix_reason(&itemstat.name, itemstat.id, ctx));
            }
        }
    }
    reasons.dedup_by(|a, b| a.entity == b.entity && a.field == b.field);
    reasons
}

/// Gear-only stats for a validated build (no base attributes, no traits, no
/// rune or sigil), plus every reason a slot could not be priced.
///
/// The one place outside [`calculate_validated_stats`] that is allowed to price
/// gear. Callers that need a *whole* sheet want `calculate_validated_stats`;
/// this exists for the seed ranker, which adds its own base and trait blocks.
pub fn validated_gear_stats(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    ctx: &BalanceContext,
) -> (stats::StatBlock, Vec<data::DataQualityReason>) {
    let mut gear = stats::StatBlock::default();
    let reasons = apply_validated_gear_stats(&mut gear, db, validated, profession_name, ctx);
    (gear, reasons)
}

/// Re-run the gear appliers purely to collect their data quality reasons.
///
/// [`calculate_validated_stats`] returns stats and modifiers, and its shape is
/// fixed by callers outside this module (`referee.rs`, `grouped_sheet.rs`).
/// Rather than duplicate the "what could not be priced" predicate at the
/// reporting sites, run the one applier that owns it and throw the numbers
/// away — a few dozen HashMap lookups, once per result.
pub fn gear_quality_reasons(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    ctx: &BalanceContext,
) -> Vec<data::DataQualityReason> {
    validated_gear_stats(validated, db, profession_name, ctx).1
}

/// Which budget slot an equipment-table weapon slot draws.
///
/// Set 2 draws nothing — it is carried, not worn — and neither does a hand the
/// active set leaves empty. A two-hander bills its single `WeaponTwoHand`
/// budget at A1 and nothing at A2 even if an off-hand weapon is recorded there,
/// because a weapon beside a Greatsword is stale data, not a second budget
/// (same rule as [`weapon_budget::land_weapon_budget`], which produced
/// `budget`). A lone off-hand bills its one-hand budget at A2, where it
/// actually sits — reading the set's slot list positionally would have billed
/// it at A1 and then found A1's gear slot empty, silently zeroing it.
fn land_weapon_slot_type(
    slot_name: &str,
    set: &validation::ValidatedWeaponSet,
    budget: LandWeaponBudget,
) -> Option<data::SlotType> {
    let held = |hand: &Option<String>| hand.as_deref().is_some_and(|w| !w.trim().is_empty());
    match slot_name {
        "WeaponA1" if budget.is_two_handed() => Some(data::SlotType::WeaponTwoHand),
        "WeaponA1" if held(&set.main_hand) => Some(data::SlotType::WeaponOneHand),
        "WeaponA2" if !budget.is_two_handed() && held(&set.off_hand) => {
            Some(data::SlotType::WeaponOneHand)
        }
        _ => None,
    }
}

/// The records in `effects` selected for this build, the coverage list (US4,
/// specs/007-trait-triggers: every equipped source the timeline will not
/// simulate, each with its class, sorted by name, executed sources dropped),
/// and each sigil's weapon set (1, 2, or 0 when the same sigil is socketed on
/// both) so the timeline fires it only while that set is held (CONN-00-07).
///
/// Executed means: a trait in `executed_traits` (the parser consumed one of
/// its facts), or a skill the builder produced at least one `SkillEffect`
/// for. A record with a `coverage` block puts its class on the list; records
/// the timeline cannot execute add their own notes at load time.
pub(crate) fn active_normalized_effects<'e>(
    validated: &ValidatedBuild,
    rotation_skills: &[rotation::RotationSkill],
    db: &GameDb,
    effects: &'e [crate::data::normalized_effects::NormalizedEffect],
    executed_traits: &std::collections::HashSet<u32>,
) -> (
    Vec<&'e crate::data::normalized_effects::NormalizedEffect>,
    Vec<crate::data::quality::CoverageEntry>,
    std::collections::HashMap<u32, u8>,
) {
    use crate::data::normalized_effects::{CoverageClass, SourceType};
    use crate::data::quality::{CoverageEntry, ReasonClass};

    let trait_ids: std::collections::HashSet<u32> = validated
        .specializations
        .iter()
        .flat_map(|spec| spec.all_trait_ids.iter().copied())
        .collect();
    let skill_ids: std::collections::HashSet<u32> =
        rotation_skills.iter().map(|skill| skill.skill_id).collect();
    let rune_ids: std::collections::HashSet<u32> =
        validated.rune.iter().map(|item| item.id).collect();
    let sigil_sets = sigil_seats(validated);
    let sigil_ids: std::collections::HashSet<u32> = sigil_sets.keys().copied().collect();
    let relic_ids: std::collections::HashSet<u32> =
        validated.relic.iter().map(|item| item.id).collect();

    let selected = |source_type: &SourceType, source_id: u32| match source_type {
        SourceType::Trait => trait_ids.contains(&source_id),
        SourceType::Skill => skill_ids.contains(&source_id),
        SourceType::Rune => rune_ids.contains(&source_id),
        SourceType::Sigil => sigil_ids.contains(&source_id),
        SourceType::Relic => relic_ids.contains(&source_id),
    };

    let active: Vec<_> = effects
        .iter()
        .filter(|effect| selected(&effect.source_type, effect.source_id))
        .collect();
    let modeled: std::collections::HashSet<(u8, u32)> = active
        .iter()
        .map(|effect| (source_type_tag(&effect.source_type), effect.source_id))
        .collect();
    let mut equipped: std::collections::HashSet<(u8, u32)> = trait_ids
        .iter()
        .map(|id| (source_type_tag(&SourceType::Trait), *id))
        .collect();
    equipped.extend(
        skill_ids
            .iter()
            .map(|id| (source_type_tag(&SourceType::Skill), *id)),
    );
    equipped.extend(
        rune_ids
            .iter()
            .map(|id| (source_type_tag(&SourceType::Rune), *id)),
    );
    equipped.extend(
        sigil_ids
            .iter()
            .map(|id| (source_type_tag(&SourceType::Sigil), *id)),
    );
    equipped.extend(
        relic_ids
            .iter()
            .map(|id| (source_type_tag(&SourceType::Relic), *id)),
    );
    let name_of = |tag: u8, id: u32| -> String {
        let name = match tag {
            0 => db.traits.get(&id).map(|t| t.name.clone()),
            1 => rotation_skills
                .iter()
                .find(|skill| skill.skill_id == id)
                .map(|skill| skill.name.clone())
                .or_else(|| db.skills.get(&id).map(|s| s.name.clone())),
            _ => db.items.get(&id).map(|item| item.name.clone()),
        };
        name.unwrap_or_else(|| {
            let kind = ["trait", "skill", "rune", "sigil", "relic"][tag as usize];
            format!("{kind} {id}")
        })
    };
    let executed_skills: std::collections::HashSet<u32> = rotation_skills
        .iter()
        .filter(|skill| !skill.effects.is_empty())
        .map(|skill| skill.skill_id)
        .collect();
    let mut coverage: Vec<CoverageEntry> = active
        .iter()
        .filter_map(|effect| {
            let block = effect.coverage.as_ref()?;
            Some(CoverageEntry {
                name: effect.source_name.clone(),
                class: match block.class {
                    CoverageClass::PassiveNoEffect => ReasonClass::PassiveNoEffect,
                    CoverageClass::NeedsMechanic => {
                        ReasonClass::NeedsMechanic(block.mechanic.clone().unwrap_or_default())
                    }
                },
                detail: None,
                source_type: source_type_tag(&effect.source_type),
                source_id: effect.source_id,
            })
        })
        .collect();
    for (tag, id) in equipped.difference(&modeled) {
        let executed_from_facts = match tag {
            0 => executed_traits.contains(id),
            1 => executed_skills.contains(id),
            _ => false,
        };
        if executed_from_facts {
            continue;
        }
        coverage.push(CoverageEntry {
            name: name_of(*tag, *id),
            class: ReasonClass::NoRecord,
            detail: None,
            source_type: *tag,
            source_id: *id,
        });
    }
    crate::data::quality::dedup_inventory(&mut coverage);
    (active, coverage, sigil_sets)
}

/// Every socketed sigil with its seat: 1 or 2 for the set it sits on, 0 when
/// socketed on both (held whichever set is out). A stowed sigil grants
/// nothing until a swap brings it in; both simulators enforce that per hit.
fn sigil_seats(validated: &ValidatedBuild) -> std::collections::HashMap<u32, u8> {
    let [set_one, set_two] = validated.sigil_ids_by_set();
    let mut sigil_sets: std::collections::HashMap<u32, u8> = std::collections::HashMap::new();
    for id in &set_one {
        sigil_sets.insert(*id, 1);
    }
    for id in &set_two {
        let set = if set_one.contains(id) { 0 } else { 2 };
        sigil_sets.insert(*id, set);
    }
    sigil_sets
}

/// Equipped traits whose facts the stat sheet consumes: any AttributeAdjust
/// or BuffConversion fact, base or traited (US4, specs/007-trait-triggers).
fn stat_consumed_trait_ids(
    validated: &ValidatedBuild,
    db: &GameDb,
) -> std::collections::HashSet<u32> {
    use gw2_api::models::Fact;
    let is_stat = |fact: &Fact| {
        matches!(
            fact,
            Fact::AttributeAdjust { .. } | Fact::BuffConversion { .. }
        )
    };
    validated
        .specializations
        .iter()
        .flat_map(|spec| spec.all_trait_ids.iter().copied())
        .filter(|id| {
            db.traits.get(id).is_some_and(|t| {
                t.facts.iter().any(is_stat) || t.traited_facts.iter().any(|tf| is_stat(&tf.fact))
            })
        })
        .collect()
}

fn source_type_tag(source_type: &crate::data::normalized_effects::SourceType) -> u8 {
    use crate::data::normalized_effects::SourceType;
    match source_type {
        SourceType::Trait => 0,
        SourceType::Skill => 1,
        SourceType::Rune => 2,
        SourceType::Sigil => 3,
        SourceType::Relic => 4,
    }
}

/// A skill's life force facts: `("Life Force", "Life Force Per Hit")` shares
/// of the pool. The "per 3 Seconds" and "When Ending" variants are not
/// modeled (a known approximation, audit section 8).
fn life_force_fact_shares(skill: &gw2_api::models::Skill) -> (f64, f64) {
    use gw2_api::models::facts::Fact;
    let mut on_use = 0.0;
    let mut per_hit = 0.0;
    for fact in &skill.facts {
        if let Fact::Percent {
            text: Some(text),
            percent: Some(percent),
            ..
        } = fact
        {
            match text.as_str() {
                "Life Force" => on_use += percent / 100.0,
                "Life Force Per Hit" => per_hit += percent / 100.0,
                _ => {}
            }
        }
    }
    (on_use, per_hit)
}

/// A shroud entry skill: the profession's F1 with a flip (exit) skill.
fn is_shroud_entry(skill: &gw2_api::models::Skill) -> bool {
    skill.slot.as_deref() == Some("Profession_1") && skill.flip_skill.is_some()
}

/// Whether the rules cover every skill that names a resource
/// (`specs/005-wvw-proc-sites`, FR-015): derived from the skills and the
/// rules rather than from a list of professions. Empty rules are never a
/// complete model.
/// One bar of Warrior adrenaline, in strikes (wiki `Adrenaline`: three bars
/// of 10, 30 maximum; a burst needs one full bar).
const ADRENALINE_BAR_STRIKES: f64 = 10.0;

/// Thief initiative ceiling before traits (wiki `Initiative`).
const THIEF_INITIATIVE_CAP: f64 = 12.0;
/// Wiki `Preparedness`: Trickery minor, +3 maximum initiative.
const PREPAREDNESS_TRAIT_ID: u32 = 1232;
/// Wiki `Bladesworn`: flow accrues at 2 per second while in combat.
const FLOW_PER_SECOND_IN_COMBAT: f64 = 2.0;
/// Wiki `Bladesworn`: Dragon Trigger converts 5 flow into one charge.
const FLOW_PER_DRAGON_SLASH_CHARGE: f64 = 5.0;

/// Energy regeneration a maintained Revenant skill removes while it is up.
/// Wiki `Energy` lists every upkeep skill and its value; the API publishes
/// none of them, so the table is the only source.
fn upkeep_for(skill_name: &str) -> f64 {
    const UPKEEP: &[(&str, f64)] = &[
        ("Vengeful Hammers", 6.0),
        ("Embrace the Darkness", 6.0),
        ("Protective Solace", 8.0),
        ("Impossible Odds", 6.0),
        ("Facet of Light", 1.0),
        ("Facet of Darkness", 2.0),
        ("Facet of Elements", 1.0),
        ("Facet of Strength", 2.0),
        ("Facet of Chaos", 4.0),
        ("Facet of Nature", 2.0),
        ("Soulcleave's Summit", 5.0),
        ("Urn of Saint Viktor", 5.0),
    ];
    UPKEEP
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(skill_name))
        .map(|(_, upkeep)| *upkeep)
        .unwrap_or(0.0)
}

/// A skill that puts an illusion on the field: the API's own `Clone` and
/// `Phantasm` categories, with the `Phantasmal ...` name as the fallback for
/// rows that carry no category.
fn generates_illusion(skill: &gw2_api::models::Skill) -> bool {
    skill
        .categories
        .iter()
        .any(|category| category == "Clone" || category == "Phantasm")
        || skill.name.starts_with("Phantasmal")
}

fn resource_model_complete(
    rules: &[rotation::wvw_timeline::SkillResourceRule],
    rotation_skills: &[rotation::RotationSkill],
    db: &GameDb,
) -> bool {
    unpriced_resource_skills(rules, rotation_skills, db).is_empty()
}

/// Skills that spend a resource no rule prices. Empty rules mean the whole
/// profession mechanic is unpriced, which is reported by name instead.
fn unpriced_resource_skills(
    rules: &[rotation::wvw_timeline::SkillResourceRule],
    rotation_skills: &[rotation::RotationSkill],
    db: &GameDb,
) -> Vec<String> {
    if rules.is_empty() {
        return vec!["profession mechanic".to_string()];
    }
    let ruled: std::collections::HashSet<u32> = rules.iter().map(|rule| rule.skill_id).collect();
    let mut unpriced: Vec<String> = rotation_skills
        .iter()
        .filter(|rotation_skill| {
            let Some(skill) = db.skills.get(&rotation_skill.skill_id) else {
                return false;
            };
            let (on_use, per_hit) = life_force_fact_shares(skill);
            let names_resource = skill.initiative.is_some()
                || skill.cost.is_some()
                || is_shroud_entry(skill)
                || on_use > 0.0
                || per_hit > 0.0;
            names_resource && !ruled.contains(&rotation_skill.skill_id)
        })
        .map(|rotation_skill| rotation_skill.name.clone())
        .collect();
    unpriced.sort();
    unpriced.dedup();
    unpriced
}

/// Everything this ledger does not model for the build, named. Two kinds:
/// a skill that spends an unpriced resource, and a profession or elite
/// mechanic with no resource model at all. The second kind does not make
/// the ledger "incomplete" (those builds pay nothing from a pool the
/// timeline tracks) but it must never read as a verified pass either.
fn resource_model_gap_names(
    validated: &ValidatedBuild,
    rules: &[rotation::wvw_timeline::SkillResourceRule],
    rotation_skills: &[rotation::RotationSkill],
    db: &GameDb,
    profession_name: &str,
) -> Vec<String> {
    const PROFESSION_MECHANICS: &[(&str, &str)] = &[
        ("Elementalist", "attunement recharge"),
        ("Engineer", "toolbelt recharge"),
        ("Guardian", "virtues, tomes and pages"),
        ("Ranger", "pet swap and astral force"),
    ];
    const ELITE_MECHANICS: &[(&str, &str)] = &[
        ("Deadeye", "malice"),
        ("Harbinger", "blight"),
        ("Holosmith", "heat"),
        ("Berserker", "berserk adrenaline cap"),
        ("Spellbreaker", "two-bar adrenaline cap"),
    ];
    let mut gaps = Vec::new();
    if rules.is_empty() {
        if let Some((_, mechanic)) = PROFESSION_MECHANICS
            .iter()
            .find(|(profession, _)| *profession == profession_name)
        {
            gaps.push((*mechanic).to_string());
        } else {
            gaps.push("profession mechanic".to_string());
        }
    } else {
        gaps.extend(unpriced_resource_skills(rules, rotation_skills, db));
    }
    for spec in &validated.specializations {
        if let Some((_, mechanic)) = ELITE_MECHANICS
            .iter()
            .find(|(name, _)| spec.name.eq_ignore_ascii_case(name))
        {
            gaps.push((*mechanic).to_string());
        }
    }
    gaps.sort();
    gaps.dedup();
    gaps
}

pub(crate) fn wvw_resource_rules(
    validated: &ValidatedBuild,
    rotation_skills: &[rotation::RotationSkill],
    db: &GameDb,
    profession_name: &str,
    ctx: &BalanceContext,
    max_health: f64,
) -> (
    Vec<rotation::wvw_timeline::SkillResourceRule>,
    bool,
    Vec<String>,
) {
    use rotation::wvw_timeline::{ResourceKind, SkillResourceRule};

    let virtuoso = validated
        .specializations
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case("Virtuoso"));
    let bladesworn = validated
        .specializations
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case("Bladesworn"));
    // Wiki `Preparedness` (trait 1232, Trickery minor): +3 maximum initiative.
    let initiative_cap = if validated
        .specializations
        .iter()
        .any(|spec| spec.all_trait_ids.contains(&PREPAREDNESS_TRAIT_ID))
    {
        THIEF_INITIATIVE_CAP + 3.0
    } else {
        THIEF_INITIATIVE_CAP
    };
    let mut rules = Vec::new();
    for rotation_skill in rotation_skills {
        let Some(skill) = db.skills.get(&rotation_skill.skill_id) else {
            continue;
        };
        let profession_slot = skill
            .slot
            .as_deref()
            .is_some_and(|slot| slot.starts_with("Profession_"));

        if profession_name == "Necromancer" {
            // Life force and shroud: one shape for every specialisation, the
            // numbers from data/formulas/shroud.json and the skill facts.
            let table = crate::data::shroud::table();
            let pool = table.pool_for(max_health);
            if is_shroud_entry(skill) {
                let row = table
                    .row(skill.id)
                    .or_else(|| table.row_by_name(&skill.name));
                let numbers = row.and_then(|row| {
                    row.drain_pct_per_s
                        .as_ref()
                        .zip(row.damage_reduction_pct.as_ref())
                });
                // An unread row leaves drain and reduction at "unknown":
                // the entry still needs the floor, and the model is
                // reported incomplete below.
                let (drain, factor) = numbers
                    .map(|(drain, reduction)| {
                        (
                            drain.for_mode(ctx.game_mode.clone()) / 100.0 * pool,
                            1.0 - reduction.for_mode(ctx.game_mode.clone()) / 100.0,
                        )
                    })
                    .unwrap_or((f64::NAN, f64::NAN));
                rules.push(SkillResourceRule {
                    skill_id: skill.id,
                    kind: ResourceKind::LifeForce,
                    entry_floor: table.entry_floor_pct / 100.0 * pool,
                    drain_per_second: drain,
                    shroud_damage_factor: factor,
                    shroud_health_exposed: row.is_some_and(|r| !r.protects_health),
                    enters_shroud: true,
                    ..Default::default()
                });
                if let Some(exit) = skill.flip_skill {
                    rules.push(SkillResourceRule {
                        skill_id: exit,
                        kind: ResourceKind::LifeForce,
                        exits_shroud: true,
                        ..Default::default()
                    });
                }
                continue;
            }
            let (on_use, per_hit) = life_force_fact_shares(skill);
            if on_use > 0.0 || per_hit > 0.0 {
                rules.push(SkillResourceRule {
                    skill_id: skill.id,
                    kind: ResourceKind::LifeForce,
                    gain_on_use: on_use * pool,
                    gain_on_hit: per_hit * pool,
                    ..Default::default()
                });
                continue;
            }
            if rotation_skill.weapon_set == rotation::SHROUD_SET {
                if let Some(cost) = skill.cost {
                    rules.push(SkillResourceRule {
                        skill_id: skill.id,
                        kind: ResourceKind::LifeForce,
                        cost: f64::from(cost) / 100.0 * pool,
                        ..Default::default()
                    });
                }
                continue;
            }
        }

        let initiative_cost =
            rotation::builder::sourced_skill_value(ctx, skill.id, "initiative_cost")
                .or_else(|| skill.initiative.map(f64::from));
        if let Some(cost) = initiative_cost {
            rules.push(SkillResourceRule {
                skill_id: skill.id,
                kind: ResourceKind::Initiative,
                cost,
                gain_on_hit: 0.0,
                spend_all: false,
                pool_cap: initiative_cap,
                ..Default::default()
            });
            continue;
        }
        if profession_name == "Revenant" && skill.cost.is_some() {
            rules.push(SkillResourceRule {
                skill_id: skill.id,
                kind: ResourceKind::Energy,
                cost: skill.cost.unwrap_or(0) as f64,
                upkeep: upkeep_for(&skill.name),
                gain_on_hit: 0.0,
                spend_all: false,
                ..Default::default()
            });
            continue;
        }
        if bladesworn && profession_slot {
            // Wiki `Flow`: gained at 2/s while in combat, never from
            // attacking, maximum 100, and it cannot fuel a core burst -- so
            // a Bladesworn's profession bar is priced in flow, not
            // adrenaline. Dragon Trigger converts 5 flow into one Dragon
            // Slash charge, which is the smallest useful press.
            rules.push(SkillResourceRule {
                skill_id: skill.id,
                kind: ResourceKind::Flow,
                cost: FLOW_PER_DRAGON_SLASH_CHARGE,
                pool_regen_per_second: FLOW_PER_SECOND_IN_COMBAT,
                ..Default::default()
            });
            continue;
        }
        if profession_name == "Warrior" && profession_slot {
            rules.push(SkillResourceRule {
                skill_id: skill.id,
                kind: ResourceKind::Adrenaline,
                // Unit: STRIKES of adrenaline, the same unit the timeline
                // caps at 30. `skill.cost` on a burst is not in that unit
                // and is not self-consistent either -- the API publishes
                // 10, 30, 100 and 1000 for the same three-bar resource
                // (Eviscerate 14353 says 100, Combustive Shot 14506 says
                // 1000), so copying it raw put every core burst above the
                // cap and `can_pay_resource` could never succeed. Wiki
                // `Adrenaline`: three bars of 10 strikes, 30 maximum; a
                // burst needs one full bar and expends every full bar it
                // holds, which is `spend_all`.
                cost: ADRENALINE_BAR_STRIKES,
                gain_on_hit: 0.0,
                spend_all: true,
                ..Default::default()
            });
            continue;
        }
        if profession_name == "Mesmer" && profession_slot {
            rules.push(SkillResourceRule {
                skill_id: skill.id,
                kind: if virtuoso {
                    ResourceKind::Blades
                } else {
                    ResourceKind::Illusions
                },
                cost: skill.cost.unwrap_or(1).max(1) as f64,
                gain_on_hit: 0.0,
                spend_all: true,
                ..Default::default()
            });
            continue;
        }
        // Wiki `Illusion`: clones and phantasms come from skills MARKED as
        // Clone or Phantasm skills, which the API publishes in `categories`.
        // The description word match read 55 Mesmer skills as generators
        // when 21 generate, so a shatter was priced against illusions the
        // build never had.
        if profession_name == "Mesmer" && generates_illusion(skill) {
            rules.push(SkillResourceRule {
                skill_id: skill.id,
                kind: if virtuoso {
                    ResourceKind::Blades
                } else {
                    ResourceKind::Illusions
                },
                cost: 0.0,
                gain_on_hit: 1.0,
                spend_all: false,
                ..Default::default()
            });
        }
    }
    // An unread shroud row (NaN drain) is not a model.
    let unread_shroud = rules
        .iter()
        .any(|rule| rule.enters_shroud && rule.drain_per_second.is_nan());
    let complete = !unread_shroud && resource_model_complete(&rules, rotation_skills, db);
    let mut gaps =
        resource_model_gap_names(validated, &rules, rotation_skills, db, profession_name);
    gaps.extend(rotation::missing_chain_steps(rotation_skills));
    gaps.extend(rotation::builder::unresolved_alternative_names(
        rotation_skills,
        db,
        ctx,
    ));
    let form = match form_for_build(validated, rotation_skills, db, &ctx.game_mode, max_health) {
        Err(name) => {
            gaps.push(format!("{name} form"));
            None
        }
        Ok(form) => form,
    };
    if let Some(form) = &form {
        gaps.extend(form.unmodelled.iter().cloned());
    }
    gaps.extend(
        trait_procs_for_build(
            validated,
            db,
            &ctx.game_mode,
            rotation_skills,
            form.as_ref(),
            &[],
        )
        .unhosted,
    );
    gaps.sort();
    gaps.dedup();
    (rules, complete, gaps)
}

/// The build's profession form for the gate and flow simulations
/// ([`rotation::simulator::FormSpec`]), from data only: the pressed entry
/// skill (a profession-slot skill with a flip) that has a pool row, the
/// pool from `data/formulas/shroud.json` (life force, the row the equipped
/// elite wears) or `data/formulas/forms.json` (entry floor = the entry's API
/// `cost` percent, drain = the pool over its API `Duration` fact), and the
/// equipped traits' `OnShroudEnter` / `OnShroudExit` / in-shroud `Periodic`
/// records and in-shroud `Conditional` damage modifiers ([`flow_record`]).
/// Event-fired records are every build's, form or none
/// ([`trait_procs_for_build`]).
///
/// `Ok(None)`: no form bar, or no entry pressed (Scourge). `Err(name)`: an
/// entry that carries a bar but no pool the data can read; the form
/// abstains and the name reaches the gap line (doctrine 6).
pub(crate) fn form_for_build(
    validated: &ValidatedBuild,
    rotation_skills: &[rotation::RotationSkill],
    db: &GameDb,
    mode: &GameMode,
    max_health: f64,
) -> Result<Option<rotation::simulator::FormSpec>, String> {
    use rotation::simulator::FormSpec;

    if !rotation_skills
        .iter()
        .any(|skill| skill.weapon_set == rotation::SHROUD_SET)
    {
        return Ok(None);
    }
    let pressed: Vec<(&rotation::RotationSkill, &gw2_api::models::Skill)> = rotation_skills
        .iter()
        .filter(|skill| skill.weapon_set == 0)
        .filter_map(|rs| db.skills.get(&rs.skill_id).map(|skill| (rs, skill)))
        .filter(|(_, skill)| {
            skill
                .slot
                .as_deref()
                .is_some_and(|slot| slot.starts_with("Profession_"))
        })
        .collect();
    let shrouds = crate::data::shroud::table();
    let pools = crate::data::forms::table();
    let equipped: Vec<u32> = validated
        .specializations
        .iter()
        .map(|spec| spec.spec_id)
        .collect();

    let mut form = None;
    for (rs, entry) in &pressed {
        if entry.flip_skill.is_none() {
            continue;
        }
        // The API tags every shroud entry spec-less, so the equipped
        // elite's row wins over the pressed F1's own (Reaper resolves its F1
        // to Death Shroud 10574 by id).
        let own_row = shrouds
            .row(entry.id)
            .or_else(|| shrouds.row_by_name(&entry.name));
        if let Some(own_row) = own_row {
            let row = shrouds
                .row_for_elite(&equipped)
                .map_or(own_row, |(_, row)| row);
            let Some(drain) = row.drain_pct_per_s.as_ref() else {
                return Err(row.name.clone());
            };
            let cap = shrouds.pool_for(max_health);
            form = Some((
                FormSpec {
                    name: row.name.clone(),
                    entry_skill_id: entry.id,
                    pool_cap: cap,
                    initial_pool: if shrouds.pool_persists_out_of_combat {
                        cap
                    } else {
                        0.0
                    },
                    entry_floor: shrouds.entry_floor_pct / 100.0 * cap,
                    drain_per_second: drain.for_mode(mode.clone()) / 100.0 * cap,
                    recharge_ms: (shrouds.recharge_on_exit_s * 1_000.0) as u32,
                    gains_in_form: true,
                    exit_keep: 1.0,
                    unmodelled: row.unmodelled.clone(),
                    ..Default::default()
                },
                true,
            ));
            break;
        }
        if let Some(row) = pools.forms.get(&entry.id) {
            let duration_s = entry.facts.iter().find_map(|fact| match fact {
                Fact::Time {
                    text: Some(text),
                    duration: Some(duration),
                    ..
                } if text == "Duration" && *duration > 0 => Some(f64::from(*duration)),
                _ => None,
            });
            let (Some(cost), Some(duration_s)) = (entry.cost, duration_s) else {
                return Err(entry.name.clone());
            };
            form = Some((
                FormSpec {
                    name: row.name.clone(),
                    entry_skill_id: entry.id,
                    pool_cap: row.pool,
                    initial_pool: if row.persists_out_of_combat {
                        row.pool
                    } else {
                        0.0
                    },
                    entry_floor: f64::from(cost) / 100.0 * row.pool,
                    drain_per_second: row.pool / duration_s,
                    recharge_ms: rs.cooldown_ms,
                    gain_per_strike: row.gain_pct_per_strike / 100.0 * row.pool,
                    // ponytail: heals feed astral force only on a damaged
                    // target and the flow dummy damages no one, so
                    // `gain_pct_per_heal` is not credited; wire it when the
                    // flow sim models incoming damage.
                    gains_in_form: row.gains_in_form,
                    exit_keep: row.early_exit_keep_pct / 100.0,
                    ..Default::default()
                },
                false,
            ));
            break;
        }
    }
    let Some((mut form, life_force)) = form else {
        return match pressed
            .iter()
            .find(|(_, skill)| !skill.transform_skills.is_empty())
        {
            Some((_, skill)) => Err(skill.name.clone()),
            None => Ok(None),
        };
    };

    if life_force {
        for rs in rotation_skills {
            let Some(skill) = db.skills.get(&rs.skill_id) else {
                continue;
            };
            let (on_use, per_hit) = life_force_fact_shares(skill);
            if on_use > 0.0 || per_hit > 0.0 {
                form.skill_gains.push((
                    rs.skill_id,
                    on_use * form.pool_cap,
                    per_hit * form.pool_cap,
                ));
            }
        }
    }

    let life_force_cap = life_force.then_some(form.pool_cap);
    form.life_force = life_force;
    let stat_sheet = stat_consumed_trait_ids(validated, db);
    for effect in equipped_trait_records(validated, mode) {
        match flow_record(effect, true, life_force_cap, &stat_sheet) {
            Some(Ok(FlowRecord::OnEnter(proc_))) => form.on_enter.push(proc_),
            Some(Ok(FlowRecord::OnExit(proc_))) => form.on_exit.push(proc_),
            Some(Ok(FlowRecord::InForm(interval_ms, proc_))) => {
                form.periodic.push((interval_ms, proc_))
            }
            Some(Ok(FlowRecord::WhileIn(modifier))) => form.while_in.push(modifier),
            _ => {}
        }
    }
    Ok(Some(form))
}

/// The equipped traits' records for `mode`.
fn equipped_trait_records<'e>(
    validated: &ValidatedBuild,
    mode: &GameMode,
) -> impl Iterator<Item = &'e crate::data::normalized_effects::NormalizedEffect> {
    use crate::data::normalized_effects::SourceType;
    let trait_ids: std::collections::HashSet<u32> = validated
        .specializations
        .iter()
        .flat_map(|spec| spec.all_trait_ids.iter().copied())
        .collect();
    crate::data::normalized_effects::effects()
        .effects_for_mode(mode.label())
        .iter()
        .filter(move |effect| {
            effect.source_type == SourceType::Trait && trait_ids.contains(&effect.source_id)
        })
}

/// Where a trait record plays in the flow simulation.
enum FlowRecord {
    OnEnter(rotation::simulator::FormProc),
    OnExit(rotation::simulator::FormProc),
    /// `(interval ms, proc)` while in the form.
    InForm(u32, rotation::simulator::FormProc),
    WhileIn(rotation::simulator::DamageMod),
    Triggered(rotation::simulator::TriggeredProc),
}

/// One trait record read for the flow simulation from its fields only
/// (doctrine 5). `None`: not the flow simulation's to play (`Passive` and
/// non-form `Conditional` records are the fact parser's, `OnHealthThreshold`
/// is the WvW timeline's, a coverage block claims nothing). `Err`: why it
/// abstains, named on the gap line (doctrine 6). `life_force_cap`: the pool
/// when the build's form is life force. `stat_sheet`: traits the stat sheet
/// reads attributes from (`stat_consumed_trait_ids`).
fn flow_record(
    effect: &crate::data::normalized_effects::NormalizedEffect,
    has_form: bool,
    life_force_cap: Option<f64>,
    stat_sheet: &std::collections::HashSet<u32>,
) -> Option<Result<FlowRecord, String>> {
    use crate::data::normalized_effects::TriggerRule;
    let in_shroud = effect.prerequisite.as_ref().and_then(|p| p.in_shroud);
    match effect.trigger_rule {
        TriggerRule::Passive | TriggerRule::NotApplicable | TriggerRule::OnHealthThreshold => {
            return None
        }
        TriggerRule::Conditional if in_shroud != Some(true) => return None,
        _ => {}
    }
    let placed = place_flow_record(effect, has_form, life_force_cap, in_shroud);
    // A stat-sheet attribute (Reaper's Onslaught's 300 Ferocity) cannot be
    // taken out per trait, so its crit record would count twice.
    let on_sheet = placed
        .as_ref()
        .ok()
        .and_then(FlowRecord::modifier)
        .is_some_and(|m| {
            m.axis == rotation::simulator::ModAxis::CritDamage
                && stat_sheet.contains(&effect.source_id)
        });
    Some(if on_sheet {
        Err("crit damage on the stat sheet".into())
    } else {
        placed
    })
}

impl FlowRecord {
    /// The damage modifier the record plays, if that is its payload.
    fn modifier(&self) -> Option<&rotation::simulator::DamageMod> {
        use rotation::simulator::FormProc;
        match self {
            FlowRecord::WhileIn(modifier) => Some(modifier),
            FlowRecord::OnEnter(proc_)
            | FlowRecord::OnExit(proc_)
            | FlowRecord::InForm(_, proc_)
            | FlowRecord::Triggered(rotation::simulator::TriggeredProc { proc_, .. }) => {
                match proc_ {
                    FormProc::Modifier { modifier, .. } => Some(modifier),
                    _ => None,
                }
            }
        }
    }
}

fn place_flow_record(
    effect: &crate::data::normalized_effects::NormalizedEffect,
    has_form: bool,
    life_force_cap: Option<f64>,
    in_shroud: Option<bool>,
) -> Result<FlowRecord, String> {
    use crate::data::normalized_effects::{Actor, Gate, SourceType, TriggerRule, TriggerScope};
    use crate::data::quality::FactualValue;
    use rotation::simulator::{ProcTrigger, TriggeredProc};

    // The flow sim keeps the player's boons, so a self-boon gate plays.
    // An `Interval` with no `while` state has no other clock here: it floors
    // the page ICD. Every other gate needs state this sim does not keep.
    // ponytail: that floor is how WvW trait:2021:1 plays at 20 s instead of
    // the page's 3 s. 15 s was calibrated to pre-E18 under-shroud; E22b
    // recalibrates the ceiling now that shroud matches E18 policy. Do not
    // grow a second clock.
    let mut self_boons = Vec::new();
    let mut interval_floor_ms = 0u32;
    for gate in &effect.gates {
        match gate {
            Gate::SelfBoon { boon } => self_boons.push((boon.clone(), true)),
            Gate::SelfBoonAbsent { boon } => self_boons.push((boon.clone(), false)),
            Gate::Interval {
                every_ms,
                while_state: None,
            } => interval_floor_ms = interval_floor_ms.max(*every_ms),
            _ => return Err("gated or scaled".into()),
        }
    }
    if effect.scale.is_some() || effect.scale_by.is_some() {
        return Err("gated or scaled".into());
    }
    if effect.actor != Actor::Player {
        return Err("not the player's event".into());
    }
    if effect.cast_skill_id.is_some() {
        return Err("casts a trait skill".into());
    }
    if effect.prerequisite.as_ref().is_some_and(|p| {
        p.foe_condition.is_some() || p.foe_health.is_some() || p.attunement.is_some()
    }) {
        return Err("foe or attunement prerequisite".into());
    }
    if effect
        .proc_chance
        .as_ref()
        .is_some_and(|chance| *chance != FactualValue::Resolved(1.0))
    {
        return Err("proc chance".into());
    }
    let form_trigger = matches!(
        effect.trigger_rule,
        TriggerRule::OnShroudEnter | TriggerRule::OnShroudExit | TriggerRule::Conditional
    );
    if !has_form && (form_trigger || in_shroud == Some(true)) {
        return Err("no form".into());
    }
    let icd_ms = match &effect.internal_cooldown {
        Some(FactualValue::Resolved(seconds)) => (seconds * 1_000.0).round() as u32,
        None => 0,
        Some(_) => return Err("unresolved cooldown".into()),
    };
    let icd_ms = icd_ms.max(interval_floor_ms);
    if effect.trigger_rule == TriggerRule::Conditional {
        return match record_modifier(effect) {
            Some(Ok(modifier)) => Ok(FlowRecord::WhileIn(modifier)),
            Some(Err(reason)) => Err(reason),
            None => Err(format!("{:?} while in form", effect.category)),
        };
    }
    let proc_ = flow_payload(effect, life_force_cap)?;
    let on = match (&effect.trigger_rule, &effect.trigger_scope) {
        (TriggerRule::OnShroudEnter, _) => return Ok(FlowRecord::OnEnter(proc_)),
        (TriggerRule::OnShroudExit, _) => return Ok(FlowRecord::OnExit(proc_)),
        (TriggerRule::Periodic, _) if icd_ms == 0 => return Err("no interval".into()),
        // InForm cannot honor a self-boon gate, so a gated in-shroud
        // periodic plays as a triggered pulse (WvW Reaper's Onslaught).
        (TriggerRule::Periodic, _) if in_shroud == Some(true) && self_boons.is_empty() => {
            return Ok(FlowRecord::InForm(icd_ms, proc_))
        }
        (TriggerRule::Periodic, _) => ProcTrigger::Periodic,
        // A skill's own record fires on that skill's cast.
        (TriggerRule::OnSkillUse, None) if effect.source_type == SourceType::Skill => {
            ProcTrigger::OwnCast(effect.source_id)
        }
        // A skill-use record without a scope stays unexecuted (validation
        // rule 11); a status record without one fires on any condition.
        (TriggerRule::OnSkillUse, Some(TriggerScope::Status(_)) | None) => {
            return Err("unscoped skill use".into())
        }
        (TriggerRule::OnSkillUse, Some(scope)) => ProcTrigger::SkillUse(scope.clone()),
        (TriggerRule::OnConditionApplied, Some(TriggerScope::Status(status))) => {
            ProcTrigger::ConditionApplied(Some(status.clone()))
        }
        (TriggerRule::OnConditionApplied, None | Some(TriggerScope::Any)) => {
            ProcTrigger::ConditionApplied(None)
        }
        (TriggerRule::OnHit, None | Some(TriggerScope::Any)) => ProcTrigger::Hit,
        (TriggerRule::OnCrit, None | Some(TriggerScope::Any)) if icd_ms >= CRIT_PROC_MIN_ICD_MS => {
            ProcTrigger::Crit
        }
        (TriggerRule::OnCrit, _) => return Err("on-crit under a 5 s cooldown".into()),
        (trigger, _) => {
            return Err(format!(
                "{} not played",
                rotation::wvw_timeline::trigger_label(trigger)
            ))
        }
    };
    Ok(FlowRecord::Triggered(TriggeredProc {
        on,
        icd_ms,
        in_form: in_shroud,
        weapon_set: 0,
        self_boons,
        proc_,
    }))
}

/// The shortest internal cooldown an on-crit record plays at in the flow
/// sim. There each strike adds its crit chance as probability mass and the
/// record fires once a whole proc's worth has gathered, the WvW timeline's
/// expected-value reading. With a long cooldown the wait is the cooldown and
/// the one or two strikes the mass takes barely move it; with a short one
/// the proc rate follows crit cadence, which averaged crits do not have.
const CRIT_PROC_MIN_ICD_MS: u32 = 5_000;

/// A record's percent damage modifier, when its payload is one
/// (`StrikeDamagePct`, `ConditionDamagePct`, `CritDamagePct`, direct or
/// as a `TriggeredEffect`'s inner category).
fn record_modifier(
    effect: &crate::data::normalized_effects::NormalizedEffect,
) -> Option<Result<rotation::simulator::DamageMod, String>> {
    use crate::data::normalized_effects::EffectCategory;
    use crate::data::quality::FactualValue;
    use rotation::simulator::{DamageMod, ModAxis};
    let axis = match effect.inner_category.as_ref().unwrap_or(&effect.category) {
        EffectCategory::StrikeDamagePct => ModAxis::Strike,
        EffectCategory::ConditionDamagePct => ModAxis::Condition,
        EffectCategory::CritDamagePct => ModAxis::CritDamage,
        _ => return None,
    };
    let FactualValue::Resolved(percent) = effect.value else {
        return Some(Err("unresolved value".into()));
    };
    // The WvW timeline's reading: a strike value up to 2.0 is a
    // weapon-strength coefficient proc (Sigil of Fire), not a percent.
    if axis == ModAxis::Strike && percent <= 2.0 {
        return Some(Err("strike coefficient proc".into()));
    }
    Some(Ok(DamageMod {
        axis,
        percent,
        additive: crate::data::modifier_buckets::is_additive_modifier(&effect.source_name),
    }))
}

/// What a fired record does in the flow simulation.
fn flow_payload(
    effect: &crate::data::normalized_effects::NormalizedEffect,
    life_force_cap: Option<f64>,
) -> Result<rotation::simulator::FormProc, String> {
    use crate::data::normalized_effects::{
        EffectCategory, OperationType, StackingRule, TargetSide,
    };
    use crate::data::quality::FactualValue;
    use rotation::simulator::FormProc;

    if let Some(modifier) = record_modifier(effect) {
        let modifier = modifier?;
        let Some(FactualValue::Resolved(seconds)) = &effect.effect_duration else {
            return Err("untimed modifier".into());
        };
        let max_stacks = match &effect.max_stacks {
            Some(FactualValue::Resolved(max)) => *max,
            None => 1,
            Some(_) => return Err("unresolved stacks".into()),
        };
        return Ok(FormProc::Modifier {
            source: effect.source_name.clone(),
            modifier,
            duration_ms: (seconds * 1_000.0).round() as u32,
            max_stacks,
            refresh_all: effect.stacking_rule == StackingRule::RefreshAllStacks,
        });
    }
    let op = effect.status_operation.as_ref();
    let status = op.and_then(|op| match (&op.amount_value, &op.base_duration_ms) {
        (FactualValue::Resolved(stacks), Some(FactualValue::Resolved(duration_ms))) => {
            Some((op, stacks.round().max(1.0) as u32, *duration_ms))
        }
        _ => None,
    });
    match (&effect.category, status, &effect.value) {
        (EffectCategory::AppliesBoon, Some((op, stacks, duration_ms)), _)
            if op.operation_type == OperationType::AppliesBoon =>
        {
            Ok(FormProc::Buff {
                name: op.status_kind.clone(),
                stacks,
                duration_ms,
                ally: op.target_side == TargetSide::Ally,
            })
        }
        (EffectCategory::AppliesCondition, Some((op, stacks, duration_ms)), _)
            if op.operation_type == OperationType::AppliesCondition
                && op.target_side == TargetSide::Enemy =>
        {
            Ok(FormProc::Condition {
                name: op.status_kind.clone(),
                stacks,
                duration_ms,
            })
        }
        (EffectCategory::GainsLifeForce, _, FactualValue::Resolved(percent)) => life_force_cap
            .map(|cap| FormProc::Gain(percent / 100.0 * cap))
            .ok_or_else(|| "no life force pool".into()),
        (category, _, _) => Err(format!("{category:?}")),
    }
}

/// The build's trait records for the flow simulation's event procs
/// ([`rotation::simulator::SimParams::triggered`]), for every build, form
/// or none; the always-on shares the fact parser folded for traits whose
/// damage modifiers now play on their own clock
/// ([`rotation::simulator::FoldedShares`]); and each record the flow
/// simulation cannot play, by name, for the gap line. `form`: the build's
/// form, whose own entry, exit, timer and while-in records
/// [`form_for_build`] already attached.
pub(crate) struct TraitProcs {
    pub triggered: Vec<rotation::simulator::TriggeredProc>,
    pub folded: rotation::simulator::FoldedShares,
    pub unhosted: Vec<String>,
}

pub(crate) fn trait_procs_for_build(
    validated: &ValidatedBuild,
    db: &GameDb,
    mode: &GameMode,
    bar: &[rotation::RotationSkill],
    form: Option<&rotation::simulator::FormSpec>,
    standing: &[combat::TraitStanding],
) -> TraitProcs {
    let life_force_cap = form.filter(|f| f.life_force).map(|f| f.pool_cap);
    let stat_sheet = stat_consumed_trait_ids(validated, db);
    let mut out = TraitProcs {
        triggered: Vec::new(),
        folded: Default::default(),
        unhosted: Vec::new(),
    };
    let mut folded_axes: Vec<(u32, rotation::simulator::ModAxis)> = Vec::new();
    for (effect, seat) in equipped_trait_records(validated, mode)
        .map(|effect| (effect, 0))
        .chain(equipped_skill_and_sigil_records(validated, bar, mode))
    {
        let Some(placed) = flow_record(effect, form.is_some(), life_force_cap, &stat_sheet) else {
            continue;
        };
        match placed {
            Err(reason) => out.unhosted.push(format!(
                "{} {} (flow sim: {reason})",
                effect.source_name,
                rotation::wvw_timeline::trigger_label(&effect.trigger_rule)
            )),
            Ok(record) => {
                // The fact parser's standing shares are per trait id.
                let is_trait =
                    effect.source_type == crate::data::normalized_effects::SourceType::Trait;
                if let Some(m) = record.modifier().filter(|_| is_trait) {
                    if !folded_axes.contains(&(effect.source_id, m.axis)) {
                        folded_axes.push((effect.source_id, m.axis));
                        fold_standing(&mut out.folded, standing, effect.source_id, m.axis);
                    }
                }
                if let FlowRecord::Triggered(mut t) = record {
                    t.weapon_set = seat;
                    out.triggered.push(t);
                }
            }
        }
    }
    out.unhosted.sort();
    out.unhosted.dedup();
    out
}

/// True when this mode's effect file has a record for an equipped trait,
/// skill, or sigil — the coverage inventory would have had something to
/// classify. NoRecord leftovers without a mode record do not count.
fn coverage_inventory_due(
    validated: &ValidatedBuild,
    bar: &[rotation::RotationSkill],
    mode: &GameMode,
) -> bool {
    equipped_trait_records(validated, mode).next().is_some()
        || equipped_skill_and_sigil_records(validated, bar, mode)
            .next()
            .is_some()
}

/// The records of the skills on `bar` and of the socketed sigils for
/// `mode`, each with its source's seat ([`sigil_seats`]; 0 for a skill).
// ponytail: a sigil seat is fixed per set; a swap in the flow sim switches
// which seat is live (`TriggeredProc::weapon_set`), a kit or bundle does not.
fn equipped_skill_and_sigil_records<'e>(
    validated: &ValidatedBuild,
    bar: &[rotation::RotationSkill],
    mode: &GameMode,
) -> impl Iterator<Item = (&'e crate::data::normalized_effects::NormalizedEffect, u8)> {
    use crate::data::normalized_effects::SourceType;
    let skill_ids: std::collections::HashSet<u32> = bar.iter().map(|s| s.skill_id).collect();
    let seats = sigil_seats(validated);
    crate::data::normalized_effects::effects()
        .effects_for_mode(mode.label())
        .iter()
        .filter_map(move |effect| match effect.source_type {
            SourceType::Skill if skill_ids.contains(&effect.source_id) => Some((effect, 0)),
            SourceType::Sigil => seats.get(&effect.source_id).map(|seat| (effect, *seat)),
            _ => None,
        })
}

/// Add `trait_id`'s parsed always-on share on `axis` to `folded`.
fn fold_standing(
    folded: &mut rotation::simulator::FoldedShares,
    standing: &[combat::TraitStanding],
    trait_id: u32,
    axis: rotation::simulator::ModAxis,
) {
    use rotation::simulator::ModAxis;
    let Some(share) = standing.iter().find(|s| s.trait_id == trait_id) else {
        return;
    };
    match axis {
        ModAxis::Strike => {
            folded.strike_mult *= share.strike_pct.iter().map(|m| 1.0 + m).product::<f64>();
            folded.strike_add += share.strike_add_pct;
        }
        ModAxis::Condition => {
            folded.condition_mult *= share.condition_pct.iter().map(|m| 1.0 + m).product::<f64>();
            folded.condition_add += share.condition_add_pct;
        }
        ModAxis::CritDamage => {
            folded.ferocity += share.crit_damage_pct
                * crate::data::universal_formulas::formulas().ferocity_per_crit_damage_pct;
        }
    }
}

fn wvw_weapon_swap_cooldown_ms(profession_name: &str, validated: &ValidatedBuild) -> Option<u32> {
    let bladesworn = validated
        .specializations
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case("Bladesworn"));
    weapon_swap_cooldown_for(profession_name, bladesworn)
}

fn weapon_swap_cooldown_for(profession_name: &str, bladesworn: bool) -> Option<u32> {
    if bladesworn || matches!(profession_name, "Engineer" | "Elementalist") {
        None
    } else if profession_name == "Warrior" {
        Some(5_000)
    } else {
        Some(10_000)
    }
}

/// Add weapon skill IDs for a given weapon type from the profession's weapon data.
/// Land bar: skip the underwater palette. Weapon `Aquatic` marks that palette,
/// not a land reject — Land Spear stays, its NoUnderwater skills stay.
/// The hand a weapon is wielded in. A one-handed weapon contributes only that
/// hand's slots (main: 1-3, off: 4-5); a two-hander contributes all five.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hand {
    Main,
    Off,
}

fn add_weapon_skill_ids(
    skill_ids: &mut Vec<u32>,
    profession: &Profession,
    weapon_type: &str,
    db: &GameDb,
    hand: Hand,
) {
    if let Some(weapon_info) = profession.weapons.get(weapon_type) {
        let two_handed = weapon_info
            .flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("TwoHand"));
        for skill_ref in &weapon_info.skills {
            // The API lists every dagger skill under "Dagger"; a dagger in each
            // hand used to bring Deathly Swarm (slot 4) in twice.
            let hand_ok = two_handed
                || match hand {
                    Hand::Main => matches!(
                        skill_ref.slot.as_str(),
                        "Weapon_1" | "Weapon_2" | "Weapon_3"
                    ),
                    Hand::Off => matches!(skill_ref.slot.as_str(), "Weapon_4" | "Weapon_5"),
                };
            if !hand_ok {
                continue;
            }
            let Some(skill) = db.skills.get(&skill_ref.id) else {
                continue;
            };
            if weapon_info.is_aquatic()
                && !skill
                    .flags
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case("NoUnderwater"))
            {
                continue;
            }
            skill_ids.push(skill_ref.id);
            // The API lists only a chain's first step; its follow-ups ride
            // along for the simulators' chain cursor (E11). A step missing
            // from the db ends the walk and is named on the gap line.
            let mut step = skill.next_chain;
            while let Some(id) = step.filter(|id| !skill_ids.contains(id)) {
                let Some(follow_up) = db.skills.get(&id) else {
                    break;
                };
                skill_ids.push(id);
                step = follow_up.next_chain;
            }
        }
    }
}

/// Convert a `ValidatedBuild` into a `SynergyResult` by computing stats, combat
/// metrics, and rotation simulation.  This is used by `optimize_v2()` to package
/// the beam-search winner as the standard output type.
pub fn synergy_result_from_validated(
    validated: ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    ctx: &BalanceContext,
    scenario: Option<&crate::scenario::ScenarioSpec>,
) -> SynergyResult {
    let (full_stats, modifiers) = calculate_validated_stats(&validated, db, profession_name, ctx);
    let derived = stats::compute_derived(&full_stats, profession_name);
    let [combat_solo, combat_party, combat_squad] =
        combat_tiers(&full_stats, &derived, &modifiers, profession_name, ctx);
    let rotation = simulate_validated_rotation(&validated, db, &full_stats, scenario);
    let (mut data_quality, mut quality_reasons) = quality_from_modifiers(
        &modifiers,
        &validated.warnings,
        !validated.errors.is_empty(),
        ctx.game_mode.label(),
    );
    // A build whose gear could not be priced — a PvP prefix with no amulet, an
    // unresolved legacy itemstat id — is not a build with unlucky stats. Say so
    // instead of shipping a zeroed sheet that reads as a bad recommendation.
    let gear_reasons = gear_quality_reasons(&validated, db, profession_name, ctx);
    if !gear_reasons.is_empty() {
        data_quality = data_quality.merge(&data::DataQuality::Provisional);
        quality_reasons.extend(gear_reasons);
    }
    let honesty = rotation.as_ref().map(|result| {
        data::quality::mode_honesty_reasons(
            profession_name,
            &ctx.game_mode,
            result
                .wvw
                .as_ref()
                .map(|fight| fight.unmodeled_sources.as_slice()),
            &result.honesty.unhosted,
            result.honesty.inventory_skipped,
            &result.honesty.heuristic,
        )
    });
    if let Some(reasons) = honesty {
        if !reasons.is_empty() {
            data_quality = data_quality.merge(&data::DataQuality::Provisional);
            quality_reasons.extend(reasons);
        }
    }
    if let Some(fight) = rotation.as_ref().and_then(|result| result.wvw.as_ref()) {
        if !fight.resource_model_complete {
            data_quality = data_quality.merge(&data::DataQuality::Provisional);
            quality_reasons.push(data::DataQualityReason {
                field: "wvw_timeline.resources".into(),
                entity: profession_name.into(),
                modes: vec![ctx.game_mode.label().to_string()],
                explanation: if fight.resource_simulated {
                    format!(
                        "resource model incomplete for {profession_name}: {} not modelled",
                        fight.resource_model_gaps.join(", ")
                    )
                } else {
                    format!(
                        "resource not simulated for {profession_name}: {} not modelled",
                        if fight.resource_model_gaps.is_empty() {
                            "the profession mechanic".to_string()
                        } else {
                            fight.resource_model_gaps.join(", ")
                        }
                    )
                },
            });
        }
    }
    apply_build_fact_parse_drops(
        &mut data_quality,
        &mut quality_reasons,
        db,
        &validated,
        profession_name,
        ctx.game_mode.label(),
    );
    SynergyResult {
        validated,
        stats: full_stats,
        combat_solo,
        combat_party,
        combat_squad,
        modifiers,
        rotation,
        data_quality,
        quality_reasons,
    }
}

/// Run the fully deterministic synergy optimization pipeline.
/// No LLM calls — all selections are algorithmic via synergy scoring.
/// Optional Gemini client is used only for explanation generation (not build selection).
// Deterministic pipeline entry point; the db, weights, context, optional LLM
// client, and callbacks are distinct concerns — a params struct adds no clarity.
/// [`optimize_deterministic_cancellable`] with a probe that never fires.
///
/// Kept because the addon's fallback-1 call site lives in
/// `crates/addon/src/ui/main_view/optimize_flow.rs`, which this change does not
/// own; **that call site must move to `optimize_deterministic_cancellable`.**
#[allow(clippy::too_many_arguments)]
pub fn optimize_deterministic(
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    llm_client: Option<&dyn LlmClient>,
    current_build_summary: Option<&str>,
    locks: &gw2_core::types::BuildLocks,
    scenario: Option<&crate::scenario::ScenarioSpec>,
    on_progress: &mut dyn FnMut(OptimizeProgress),
) -> Result<SynergyResult, String> {
    optimize_deterministic_cancellable(
        db,
        profession_name,
        weights,
        ctx,
        llm_client,
        current_build_summary,
        locks,
        scenario,
        on_progress,
        &|| false,
    )
}

/// Tier-2 optimizer: deterministic prefix + the full synergy pipeline, with an
/// optional LLM explanation pass. Polls `is_cancelled` at every stage boundary
/// and before the LLM call, which is the longest single wait on this path.
#[allow(clippy::too_many_arguments)]
pub fn optimize_deterministic_cancellable(
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    llm_client: Option<&dyn LlmClient>,
    _current_build_summary: Option<&str>,
    locks: &gw2_core::types::BuildLocks,
    scenario: Option<&crate::scenario::ScenarioSpec>,
    on_progress: &mut dyn FnMut(OptimizeProgress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<SynergyResult, String> {
    if is_cancelled() {
        return Err("Cancelled".into());
    }
    on_progress(OptimizeProgress {
        stage: "Selecting gear prefix...".into(),
        done: false,
    });
    let gear_match = scoring::select_gear_prefix(weights);
    let determined_prefix = gear_match.primary;

    let mut result = crate::synergy_pipeline::optimize_synergy_cancellable(
        db,
        profession_name,
        weights,
        ctx,
        determined_prefix,
        locks,
        scenario,
        on_progress,
        is_cancelled,
    )?;

    if is_cancelled() {
        return Err("Cancelled".into());
    }
    if let Some(client) = llm_client {
        on_progress(OptimizeProgress {
            stage: "Generating build explanation...".into(),
            done: false,
        });

        let specs_summary: Vec<String> = result
            .validated
            .specializations
            .iter()
            .map(|s| {
                let traits_str = s.trait_names.join(", ");
                if s.elite {
                    format!("{} (Elite): {}", s.name, traits_str)
                } else {
                    format!("{}: {}", s.name, traits_str)
                }
            })
            .collect();

        let rune_name = result
            .validated
            .rune
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or("None");
        let sigil_names: Vec<&str> = result
            .validated
            .sigils
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let relic_name = result
            .validated
            .relic
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or("None");
        let food_name = result
            .validated
            .food
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or("None");
        let utility_name = result
            .validated
            .utility
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or("None");

        let set1 = format!(
            "{}{}",
            result
                .validated
                .weapons
                .set1
                .main_hand
                .as_deref()
                .unwrap_or("?"),
            result
                .validated
                .weapons
                .set1
                .off_hand
                .as_deref()
                .map(|o| format!(" / {}", o))
                .unwrap_or_default()
        );
        let set2 = format!(
            "{}{}",
            result
                .validated
                .weapons
                .set2
                .main_hand
                .as_deref()
                .unwrap_or("?"),
            result
                .validated
                .weapons
                .set2
                .off_hand
                .as_deref()
                .map(|o| format!(" / {}", o))
                .unwrap_or_default()
        );

        let heal = result
            .validated
            .skills
            .heal
            .as_ref()
            .map(|(_, n)| n.as_str())
            .unwrap_or("?");
        let utils: Vec<&str> = result
            .validated
            .skills
            .utilities
            .iter()
            .filter_map(|u| u.as_ref().map(|(_, n)| n.as_str()))
            .collect();
        let elite = result
            .validated
            .skills
            .elite
            .as_ref()
            .map(|(_, n)| n.as_str())
            .unwrap_or("?");

        let summary = format!(
            "Profession: {}\nGear: {}\nSpecializations:\n{}\nWeapons: Set 1: {} | Set 2: {}\n\
             Skills: Heal: {} | Utilities: {} | Elite: {}\n\
             Rune: {}\nSigils: {}\nRelic: {}\nFood: {}\nUtility: {}\n\
             Combat (Solo): Strike DPS {:.0}, Condi DPS {:.0}, Total DPS {:.0}",
            profession_name,
            determined_prefix,
            specs_summary.join("\n"),
            set1,
            set2,
            heal,
            utils.join(", "),
            elite,
            rune_name,
            sigil_names.join(", "),
            relic_name,
            food_name,
            utility_name,
            result.combat_solo.strike_dps_index,
            result.combat_solo.condition_dps_index,
            result.combat_solo.total_dps_index,
        );

        let prompt = format!(
            "You are a Guild Wars 2 build expert. Explain why the following build works well together. \
             Describe the key synergy chains between traits, rune, sigils, relic, and skills. \
             Suggest a skill rotation priority. Keep it under 200 words.\n\n{}",
            summary,
        );

        match client.generate_brief(&prompt, BRIEF_REPLY_TOKENS) {
            Ok(explanation) => {
                result.validated.synergy_explanation = explanation;
            }
            Err(_e) => {
                // LLM explanation failed, keep the template explanation
            }
        }
    }

    on_progress(OptimizeProgress {
        stage: "Done".into(),
        done: true,
    });

    result
        .quality_reasons
        .extend(stale_trait_lock_reasons(locks, db, ctx));
    Ok(result)
}

/// Run the v2 beam/evolutionary search.
///
/// Seeds from the synergy pipeline, then performs a bounded beam search over
/// complete build states using the gated referee as the fitness function.
/// If `llm_client` is Some, runs the LLM advisor post-beam to propose
/// additional candidate mutations — the referee is still the final authority.
/// Completes within `SearchConfig::time_limit_secs` (default 45 s).
// Beam-search pipeline entry point; db, weights, context, scenario, and
// optional LLM client are independent inputs — a params struct adds no clarity.
#[allow(clippy::too_many_arguments)]
pub fn optimize_v2(
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &crate::scenario::ScenarioSpec,
    locks: &gw2_core::types::BuildLocks,
    llm_client: Option<&dyn LlmClient>,
    benchmarks_dir: Option<&std::path::Path>,
    on_progress: &mut dyn FnMut(OptimizeProgress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<SynergyResult, String> {
    use crate::search_v2::SearchConfig;

    on_progress(OptimizeProgress {
        stage: "Running v2 search...".into(),
        done: false,
    });
    let config = SearchConfig {
        // Proven combinations seed the beam alongside our own seed. `None`
        // (never synced) leaves the search exactly as it was.
        benchmarks_dir: benchmarks_dir.map(|dir| dir.to_path_buf()),
        ..SearchConfig::default()
    };
    let mut best = crate::search_v2::optimize_v2_search(
        db,
        profession_name,
        weights,
        ctx,
        scenario,
        locks,
        &config,
        on_progress,
        is_cancelled,
    )?;

    if is_cancelled() {
        return Err("Cancelled".into());
    }

    // Optional: LLM advisor pass — propose mutations, referee ranks them.
    if let Some(client) = llm_client {
        if is_cancelled() {
            return Err("Cancelled".into());
        }
        on_progress(OptimizeProgress {
            stage: "LLM advisor: evaluating mutations...".into(),
            done: false,
        });
        best = llm_advisor(
            best,
            db,
            profession_name,
            weights,
            ctx,
            scenario,
            locks,
            client,
        );
    }

    if is_cancelled() {
        return Err("Cancelled".into());
    }

    // Post-beam nudge pass: hill-climb single-piece swaps so the result can
    // "replace 1–4 pieces" and nudge stats when saturated axes make mixes
    // strictly better than the best uniform prefix.
    on_progress(OptimizeProgress {
        stage: "Fine-tuning piece swaps...".into(),
        done: false,
    });
    best = crate::search_v2::refine_piece_swaps(
        best,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
        locks,
        on_progress,
        is_cancelled,
    );

    if is_cancelled() {
        return Err("Cancelled".into());
    }

    on_progress(OptimizeProgress {
        stage: "Done".into(),
        done: true,
    });
    let mut synergy_result =
        synergy_result_from_validated(best, db, profession_name, ctx, Some(scenario));
    synergy_result
        .quality_reasons
        .extend(stale_trait_lock_reasons(locks, db, ctx));
    Ok(synergy_result)
}

/// Slot keywords accepted by the advisor gear grammar (`SWAP: gear <slot>
/// <prefix>`). Keys are dash-free and lowercase so `ring-1`, `ring1`, and
/// `Ring-1` all match after normalization.
const GEAR_SLOT_KEYWORDS: &[(&str, GearSlot)] = &[
    ("helm", GearSlot::Helm),
    ("shoulders", GearSlot::Shoulders),
    ("coat", GearSlot::Coat),
    ("gloves", GearSlot::Gloves),
    ("leggings", GearSlot::Leggings),
    ("boots", GearSlot::Boots),
    ("back", GearSlot::Back),
    ("accessory1", GearSlot::Accessory1),
    ("accessory2", GearSlot::Accessory2),
    ("amulet", GearSlot::Amulet),
    ("ring1", GearSlot::Ring1),
    ("ring2", GearSlot::Ring2),
    ("weaponset1main", GearSlot::WeaponSet1Main),
    ("weaponset1off", GearSlot::WeaponSet1Off),
    ("weaponset2main", GearSlot::WeaponSet2Main),
    ("weaponset2off", GearSlot::WeaponSet2Off),
];

/// Split an advisor gear request into `(slot, prefix_text)` when its first
/// token names a slot, else `None` (the whole body is one prefix name — bare
/// uniform form). Dashes are ignored for the lookup because LLM responses
/// freely mix "ring-1" with "ring1".
fn parse_slot_qualifier(body: &str) -> Option<(GearSlot, &str)> {
    let (token, rest) = body.split_once(char::is_whitespace)?;
    let normalized = token.replace('-', "").to_ascii_lowercase();
    GEAR_SLOT_KEYWORDS
        .iter()
        .find(|(keyword, _)| *keyword == normalized)
        .map(|(_, slot)| (*slot, rest.trim()))
}

/// Post-beam LLM advisor: ask the LLM for candidate mutations, evaluate each
/// through the referee, return the best improvement found (or original if none better).
///
/// The LLM is a *search policy* — it proposes swaps. The referee decides winners.
/// LLM errors are silently logged and the original build is returned unchanged.
// Locks joined an already-wide advisory surface; every parameter is an
// independent input — mirroring `optimize`'s allowance.
#[allow(clippy::too_many_arguments)]
/// Resolve the right-hand side of an advisor `SWAP: rune=<name>` line.
///
/// Two rules, both about not inventing a choice:
///
/// * **An empty needle is not a wildcard.** `"anything".contains("")` is true,
///   so a bare `SWAP: rune=` used to match every rune in the game and equip
///   whichever one `db.runes` yielded first. `db.runes` is built from
///   `items.values()`, so "first" was a different rune from run to run and the
///   referee gate — which a rune-less build passes with almost any rune —
///   happily accepted it. Nothing to match is nothing to swap.
/// * **The match is order-independent.** Among the runes whose name contains
///   the needle, take the shortest name and then the lowest id: the same
///   shortest-match discipline `GameDb::itemstat_by_name` uses, and the same
///   answer on every machine.
fn advisor_rune_pick(db: &GameDb, raw_name: &str) -> Option<crate::validation::ValidatedItem> {
    let needle = raw_name
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_lowercase();
    if needle.is_empty() {
        return None;
    }
    db.runes
        .iter()
        .filter_map(|id| db.items.get(id))
        .filter(|item| item.name.to_lowercase().contains(&needle))
        .min_by_key(|item| (item.name.len(), item.id))
        .map(|item| crate::validation::ValidatedItem {
            id: item.id,
            name: item.name.clone(),
        })
}

/// A11-1 SWAP accept gate: every populated gear slot must be one the build
/// actually wears.
///
/// Mirrors the plate rule in `validate_gear_slot_map` (validation.rs): a plate
/// entry naming a slot the build does not wear is ignored, because "a prefix
/// on a hand that holds nothing is not a gear choice". The SWAP parser already
/// guarantees a known slot (`parse_slot_qualifier`) and a resolved prefix
/// (`db.itemstat_by_name`), but the slot-qualified form wrote the prefix
/// without asking [`ValidatedBuild::wears`], so `SWAP: gear weapon-set-1-off …`
/// on a Greatsword build recorded a prefix no plate build can carry. The
/// referee prices that phantom slot to nothing, so the rank comparison cannot
/// be trusted to keep the invalid state out — this gate can.
///
/// A shared `pub(crate)` helper in validation.rs would be the cleaner home
/// for this rule; that file is owned elsewhere, so the check reuses the
/// existing pub `ValidatedBuild::wears` here instead.
fn advisor_candidate_slots_legal(candidate: &ValidatedBuild) -> bool {
    GearSlot::ALL
        .iter()
        .zip(candidate.gear_slots.map.iter())
        .all(|(slot, cell)| cell.is_none() || candidate.wears(*slot))
}

/// Completion cap for the advisor's three SWAP lines and the 200-word build
/// explanation: thinking and answer together. A reasoning model that needs
/// more than this for either has nothing the search can use.
pub const BRIEF_REPLY_TOKENS: u32 = 2_048;

#[allow(clippy::too_many_arguments)]
pub fn llm_advisor(
    current: crate::validation::ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &crate::scenario::ScenarioSpec,
    locks: &gw2_core::types::BuildLocks,
    llm_client: &dyn LlmClient,
) -> crate::validation::ValidatedBuild {
    let current_gear = current
        .primary_prefix()
        .map(|p| p.name.as_str())
        .unwrap_or("Unknown");
    let current_rune = current
        .rune
        .as_ref()
        .map(|r| r.name.as_str())
        .unwrap_or("None");
    let current_sigils: Vec<&str> = current.sigils.iter().map(|s| s.name.as_str()).collect();

    let prompt = format!(
        "You are a Guild Wars 2 build advisor. The current optimized build for {} uses:\n\
         - Gear prefix: {}\n\
         - Rune: {}\n\
         - Sigils: {}\n\n\
         Game mode: {}. Scoring priorities: Power={:.1}, Condition={:.1}, Sustain={:.1}, Control={:.1}\n\n\
         Suggest exactly 3 alternative swaps to try that might score better given these \
         priorities. Format each suggestion as one of:\n\
         SWAP: gear [slot] [prefix]\n\
         SWAP: gear [prefix]\n\
         SWAP: rune=[name]\n\
         The first form changes only one equipment piece's stat prefix; the second changes \
         every unlocked piece to one stat prefix.\n\
         Slots: helm shoulders coat gloves leggings boots back accessory-1 accessory-2 amulet \
         ring-1 ring-2 weapon-set-1-main weapon-set-1-off weapon-set-2-main weapon-set-2-off.\n\
         Locked pieces are respected automatically — do not propose changing them.\n\
         Only suggest gear or rune changes. Do not suggest spec changes.",
        profession_name,
        current_gear,
        current_rune,
        current_sigils.join(", "),
        ctx.game_mode.label(),
        weights.power, weights.condition, weights.sustain, weights.control,
    );

    let current_report = crate::referee::evaluate_validated_build(
        &current,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
    );

    let response = match llm_client.generate_brief(&prompt, BRIEF_REPLY_TOKENS) {
        Ok(r) => r,
        Err(_) => {
            // LLM advisor failure is non-fatal — return original build silently.
            return current;
        }
    };

    let mut best_validated = current.clone();
    let mut best_rank = crate::referee::search_rank(&current_report);

    for line in response.lines() {
        let line = line.trim();
        if !line.starts_with("SWAP:") {
            continue;
        }
        let swap_part = &line["SWAP:".len()..].trim().to_lowercase();

        let mut candidate = current.clone();

        if let Some(rest) = swap_part.strip_prefix("gear ") {
            // Slot-qualified or bare gear form:
            //   SWAP: gear helm <prefix>  → change that one piece
            //   SWAP: gear <prefix>       → uniform, every unlocked piece
            let body = rest.trim().trim_matches('"').trim_matches('\'').trim();
            match parse_slot_qualifier(body) {
                Some((slot, prefix_text)) => {
                    if locks.gear_locks.contains_key(&slot) {
                        continue; // never touch a locked slot
                    }
                    let Some(item_stat) = db.itemstat_by_name(prefix_text) else {
                        continue; // Skip if prefix not found in DB
                    };
                    if candidate.gear_slots.prefix_id(slot) == Some(item_stat.id) {
                        continue; // no-op same-prefix swap
                    }
                    candidate.gear_slots.set(
                        slot,
                        PrefixRef {
                            itemstat_id: item_stat.id,
                            name: item_stat.name.clone(),
                        },
                    );
                }
                None => {
                    // Bare form — uniform across all unlocked pieces.
                    let Some(item_stat) = db.itemstat_by_name(body) else {
                        continue; // Skip if prefix not found in DB
                    };
                    if !candidate.fill_unlocked_gear_slots(
                        PrefixRef {
                            itemstat_id: item_stat.id,
                            name: item_stat.name.clone(),
                        },
                        &locks.gear_locks,
                    ) {
                        continue; // proposal would change nothing
                    }
                }
            }
        } else if let Some(rest) = swap_part.strip_prefix("gear_prefix=") {
            // Legacy grammar (`SWAP: gear_prefix=[name]`) kept for backward
            // compatibility with old prompts/models. Uniform across all
            // unlocked pieces; locked slots keep their locked prefix.
            let prefix_name = rest.trim().trim_matches('"').trim_matches('\'');
            if let Some(item_stat) = db.itemstat_by_name(prefix_name) {
                if !candidate.fill_unlocked_gear_slots(
                    PrefixRef {
                        itemstat_id: item_stat.id,
                        name: item_stat.name.clone(),
                    },
                    &locks.gear_locks,
                ) {
                    continue; // proposal would change nothing
                }
            } else {
                continue; // Skip if prefix not found in DB
            }
        } else if let Some(rest) = swap_part.strip_prefix("rune=") {
            match advisor_rune_pick(db, rest) {
                Some(r) => candidate.rune = Some(r),
                None => continue,
            }
        } else {
            continue;
        }

        // Evaluate the mutation through the referee.
        // A candidate that fails the plate slot rules is never evaluated at
        // all — rank cannot rescue it (A11-1).
        if !advisor_candidate_slots_legal(&candidate) {
            continue;
        }
        let report = crate::referee::evaluate_validated_build(
            &candidate,
            db,
            profession_name,
            weights,
            ctx,
            scenario,
        );
        if crate::referee::search_rank(&report) > best_rank {
            best_rank = crate::referee::search_rank(&report);
            best_validated = candidate;
        }
    }

    best_validated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_prepared(
        db: &GameDb,
        build: &ValidatedBuild,
    ) -> (PreparedRotation, crate::scenario::ScenarioSpec) {
        let (ctx, scenario) = rotation::reaper_fixture::pve_scenario();
        let (stats, _) = calculate_validated_stats(build, db, "Necromancer", &ctx);
        let prepared = prepare_validated_rotation(build, db, &stats, Some(&scenario))
            .expect("the fixture prepares a rotation");
        (prepared, scenario)
    }

    /// Doctrine 8: the addon's own path (prepare, flow) enters the
    /// fixture's shroud with the shipped `shroud.json` numbers (PvE
    /// Reaper's Shroud: 4 %/s of a pool 69 % of health, 10 s recharge).
    #[test]
    fn the_flow_plays_the_fixture_reapers_shroud_from_data() {
        use rotation::reaper_fixture as fx;
        let db = fx::db();
        let (prepared, scenario) = fixture_prepared(&db, &fx::build());
        let form = prepared.params.form.as_ref().expect("the shroud is a form");
        let pool = crate::data::shroud::table().pool_for(prepared.params.max_health);
        assert_eq!(form.name, "Reaper's Shroud");
        assert_eq!(form.entry_skill_id, fx::REAPER_SHROUD);
        assert!((form.pool_cap - pool).abs() < 1e-9);
        assert!((form.drain_per_second - 0.04 * pool).abs() < 1e-9);
        assert!((form.entry_floor - 0.10 * pool).abs() < 1e-9);
        assert_eq!(form.recharge_ms, 10_000);
        assert_eq!(form.initial_pool, form.pool_cap);
        let life_rend = prepared
            .skills
            .iter()
            .find(|s| s.skill_id == fx::SHROUD_1)
            .expect("Life Rend on the bar");
        assert_eq!(life_rend.weapon_set, rotation::SHROUD_SET);
        assert_eq!(life_rend.slot, rotation::SkillSlot::Weapon1);

        let flow = simulate_flow(&prepared, &OptimizationWeights::default(), Some(&scenario));
        for name in ["Life Rend", "Death's Charge", "Soul Spiral"] {
            assert!(
                flow.skill_usage
                    .iter()
                    .any(|u| u.name == name && u.cast_count > 0 && u.dps_contribution > 0.0),
                "{name} cast in shroud: {:?}",
                flow.skill_usage
            );
        }
    }

    /// E18. Pre-change fixture `skill_share` TVD against the golem log was
    /// 0.648, with Dusk Strike at 0.168 of damage (log share 0) and Life
    /// Rend at 0.019. Leaving shroud while its auto was still the cast
    /// filled the recharge with greatsword autos.
    const E18_SKILL_SHARE_TVD_BOUND: f64 = 0.648;

    #[test]
    fn weapon1_auto_is_filler_so_it_does_not_block_shroud_or_the_other_set() {
        use std::collections::BTreeMap;

        use rotation::reaper_fixture as fx;

        let db = fx::db();
        let (prepared, scenario) = fixture_prepared(&db, &fx::build());
        let flow = simulate_flow(&prepared, &OptimizationWeights::default(), Some(&scenario));
        let share = |name: &str| {
            flow.skill_usage
                .iter()
                .find(|u| u.name == name)
                .map(|u| u.dps_contribution / flow.total_dps)
                .unwrap_or(0.0)
        };
        let dusk = share("Dusk Strike");
        let life_rend = share("Life Rend");
        let ghastly = flow
            .skill_usage
            .iter()
            .find(|u| u.name == "Ghastly Claws")
            .map(|u| u.cast_count)
            .unwrap_or(0);

        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ei_logs/1f33-20260720-163045_golem.json"
        ))
        .expect("golem fixture");
        let log = crate::fidelity::ei_log::parse(&text).expect("golem fixture parses");
        let player = log.squad().next().expect("golem player");
        let observed = crate::fidelity::compare::observe(
            &log,
            player,
            &crate::gamedb::GameDb::empty_for_tests(),
        );
        assert_eq!(
            observed
                .skill_share
                .get("Dusk Strike")
                .copied()
                .unwrap_or(0.0),
            0.0,
            "the golem log casts no greatsword auto"
        );

        let mut sim_share = BTreeMap::new();
        for u in &flow.skill_usage {
            if flow.total_dps > 0.0 {
                *sim_share.entry(u.name.clone()).or_insert(0.0) +=
                    u.dps_contribution / flow.total_dps;
            }
        }
        if flow.condition_dps > 0.0 {
            sim_share.insert(
                "Conditions".to_string(),
                flow.condition_dps / flow.total_dps,
            );
        }
        let distance = crate::fidelity::compare::tvd(&observed.skill_share, &sim_share);

        assert!(
            dusk < life_rend,
            "shroud filler should outrun the greatsword auto: dusk {dusk}, life rend {life_rend}"
        );
        assert!(
            dusk < 0.10,
            "Dusk Strike share {dusk} moved away from the log's 0 (was 0.168)"
        );
        assert!(
            ghastly >= 1,
            "the other set still gets its non-auto: Ghastly Claws x{ghastly}"
        );
        assert!(
            distance <= E18_SKILL_SHARE_TVD_BOUND,
            "skill_share TVD {distance:.6} regressed past {}",
            E18_SKILL_SHARE_TVD_BOUND
        );
    }

    /// Doctrine 5 and 6 for the flow simulation's trait records: placement
    /// reads record fields only, and a trigger class the flow simulation
    /// cannot host abstains with the class named.
    #[test]
    fn flow_records_place_by_field_and_name_the_unhostable_trigger() {
        use crate::data::normalized_effects::{
            EffectCategory, Prerequisite, SourceType, TriggerRule, TriggerScope,
        };
        use crate::data::quality::FactualValue;
        use rotation::reaper_fixture::record;
        use rotation::simulator::{FormProc, ModAxis, ProcTrigger};

        let none = std::collections::HashSet::new();
        let timed_strike = |trigger| {
            let mut effect = record(
                SourceType::Trait,
                1,
                "Synthetic",
                EffectCategory::TriggeredEffect,
                20.0,
                trigger,
            );
            effect.inner_category = Some(EffectCategory::StrikeDamagePct);
            effect.effect_duration = Some(FactualValue::Resolved(2.0));
            effect
        };

        let mut on_use = timed_strike(TriggerRule::OnSkillUse);
        on_use.trigger_scope = Some(TriggerScope::Category("Virtue".into()));
        match flow_record(&on_use, false, None, &none) {
            Some(Ok(FlowRecord::Triggered(t))) => {
                assert!(matches!(t.on, ProcTrigger::SkillUse(_)));
                assert_eq!(t.in_form, None);
                assert!(matches!(
                    t.proc_,
                    FormProc::Modifier { ref modifier, duration_ms: 2_000, .. }
                        if modifier.axis == ModAxis::Strike && modifier.percent == 20.0
                ));
            }
            _ => panic!("a scoped skill-use record plays without a form"),
        }

        on_use.prerequisite = Some(Prerequisite {
            in_shroud: Some(true),
            ..Default::default()
        });
        assert!(matches!(flow_record(&on_use, false, None, &none), Some(Err(r)) if r == "no form"));

        // On-crit with a short (here no) cooldown abstains by name; with a
        // cooldown of 5 s or more it plays as a crit-weighted hit proc.
        match flow_record(&timed_strike(TriggerRule::OnCrit), false, None, &none) {
            Some(Err(reason)) => assert_eq!(reason, "on-crit under a 5 s cooldown"),
            _ => panic!("short-cooldown on-crit must abstain by name"),
        }
        let mut long_crit = timed_strike(TriggerRule::OnCrit);
        long_crit.internal_cooldown = Some(FactualValue::Resolved(4.9));
        assert!(matches!(
            flow_record(&long_crit, false, None, &none),
            Some(Err(r)) if r == "on-crit under a 5 s cooldown"
        ));
        long_crit.internal_cooldown = Some(FactualValue::Resolved(20.0));
        long_crit.gates = vec![crate::data::normalized_effects::Gate::SelfBoonAbsent {
            boon: "Quickness".into(),
        }];
        match flow_record(&long_crit, false, None, &none) {
            Some(Ok(FlowRecord::Triggered(t))) => {
                assert!(matches!(t.on, ProcTrigger::Crit));
                assert_eq!(t.icd_ms, 20_000);
                assert_eq!(t.self_boons, vec![("Quickness".to_string(), false)]);
            }
            _ => panic!("a 20 s on-crit record plays"),
        }
        long_crit.gates = vec![crate::data::normalized_effects::Gate::InCombat];
        assert!(matches!(
            flow_record(&long_crit, false, None, &none),
            Some(Err(r)) if r == "gated or scaled"
        ));
        // A skill's own unscoped skill-use record fires on that skill.
        let mut own = record(
            SourceType::Skill,
            29965,
            "Synthetic Shout",
            EffectCategory::TriggeredEffect,
            20.0,
            TriggerRule::OnSkillUse,
        );
        own.inner_category = Some(EffectCategory::StrikeDamagePct);
        own.effect_duration = Some(FactualValue::Resolved(2.0));
        assert!(matches!(
            flow_record(&own, false, None, &none),
            Some(Ok(FlowRecord::Triggered(t))) if t.on == ProcTrigger::OwnCast(29965)
        ));

        let mut in_form_crit = record(
            SourceType::Trait,
            2,
            "Synthetic Crit",
            EffectCategory::TriggeredEffect,
            10.0,
            TriggerRule::Conditional,
        );
        in_form_crit.inner_category = Some(EffectCategory::CritDamagePct);
        in_form_crit.prerequisite = Some(Prerequisite {
            in_shroud: Some(true),
            ..Default::default()
        });
        assert!(matches!(
            flow_record(&in_form_crit, true, None, &none),
            Some(Ok(FlowRecord::WhileIn(ref m))) if m.axis == ModAxis::CritDamage
        ));
        let on_sheet: std::collections::HashSet<u32> = [2].into();
        assert!(matches!(
            flow_record(&in_form_crit, true, None, &on_sheet),
            Some(Err(r)) if r == "crit damage on the stat sheet"
        ));

        let passive = record(
            SourceType::Trait,
            3,
            "Synthetic Passive",
            EffectCategory::StrikeDamagePct,
            10.0,
            TriggerRule::Passive,
        );
        assert!(flow_record(&passive, true, None, &none).is_none());
    }

    /// E22 / E22b: WvW Reaper's Onslaught plays as a gated periodic at the
    /// 20 s ceiling. PvE stays an ungated in-form pulse.
    #[test]
    fn kent_e22_wvw_onslaught_places_as_gated_periodic() {
        use crate::data::normalized_effects::effects;
        use rotation::simulator::{FormProc, ProcTrigger};

        let record = |mode: &str, id: &str| {
            effects()
                .effects_for_mode(mode)
                .iter()
                .find(|e| e.effect_id == id)
                .unwrap_or_else(|| panic!("{mode} {id}"))
                .clone()
        };
        let none = std::collections::HashSet::new();
        match flow_record(&record("WvW", "trait:2021:1"), true, None, &none) {
            Some(Ok(FlowRecord::Triggered(t))) => {
                assert!(matches!(t.on, ProcTrigger::Periodic));
                assert_eq!(t.icd_ms, 20_000);
                assert_eq!(t.in_form, Some(true));
                assert_eq!(t.self_boons, vec![("Quickness".to_string(), false)]);
                assert!(matches!(
                    t.proc_,
                    FormProc::Buff { ref name, duration_ms: 3_000, .. } if name == "Quickness"
                ));
            }
            _ => panic!("WvW Onslaught should be a gated periodic"),
        }
        assert!(matches!(
            flow_record(&record("PvE", "trait:2021:1"), true, None, &none),
            Some(Ok(FlowRecord::InForm(
                3_000,
                FormProc::Buff {
                    duration_ms: 3_000,
                    ..
                }
            )))
        ));
    }

    /// Doctrine 6: a pressed entry that brings a bar but has no pool
    /// record plays no form and is named on the gap line.
    #[test]
    fn an_unmodelled_form_abstains_by_name() {
        use rotation::reaper_fixture as fx;
        let mut db = fx::db();
        db.skills
            .get_mut(&fx::REAPER_SHROUD)
            .expect("fixture entry")
            .name = "Unread Shroud".into();
        let mut build = fx::build();
        build.skills.profession = vec![(fx::REAPER_SHROUD, "Unread Shroud".into())];
        let (prepared, _) = fixture_prepared(&db, &build);
        assert!(prepared.params.form.is_none());
        assert_eq!(
            form_for_build(
                &build,
                &prepared.skills,
                &db,
                &GameMode::PvE,
                prepared.params.max_health
            ),
            Err("Unread Shroud".to_string())
        );
        let ctx = BalanceContext::new(GameMode::WvW);
        let (_, _, gaps) = wvw_resource_rules(
            &build,
            &prepared.skills,
            &db,
            "Necromancer",
            &ctx,
            prepared.params.max_health,
        );
        assert!(gaps.contains(&"Unread Shroud form".to_string()), "{gaps:?}");
    }

    /// Sprint 2 (T047, FR-015): the derived completeness rule reproduces the
    /// profession list it replaces and adds Necromancer once life force
    /// rules exist.
    #[test]
    fn resource_model_completeness_matches_previous_list() {
        fn api_skill(
            id: u32,
            name: &str,
            slot: &str,
            extra: serde_json::Value,
        ) -> gw2_api::models::Skill {
            let mut value = serde_json::json!({
                "id": id, "name": name, "slot": slot, "facts": []
            });
            if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
                for (k, v) in extra {
                    base.insert(k.clone(), v.clone());
                }
            }
            serde_json::from_value(value).expect("skill")
        }
        fn rotation_skill(id: u32, weapon_set: u8) -> rotation::RotationSkill {
            rotation::RotationSkill {
                targets: 1,
                categories: Vec::new(),
                slot_name: None,
                skill_id: id,
                name: format!("skill {id}"),
                slot: rotation::SkillSlot::Utility,
                cast_time_ms: 500,
                cooldown_ms: 5_000,
                effects: Vec::new(),
                next_chain: None,
                is_stunbreak: false,
                reaches_allies: false,
                weapon_set,
            }
        }
        let cases: Vec<(&str, Vec<gw2_api::models::Skill>, bool)> = vec![
            (
                "Thief",
                vec![
                    api_skill(1, "Steal", "Profession_1", serde_json::json!({})),
                    api_skill(
                        2,
                        "Heartseeker",
                        "Weapon_2",
                        serde_json::json!({"initiative": 3}),
                    ),
                ],
                true,
            ),
            (
                "Revenant",
                vec![api_skill(
                    3,
                    "Inspiring Reinforcement",
                    "Utility",
                    serde_json::json!({"cost": 30}),
                )],
                true,
            ),
            (
                "Warrior",
                vec![api_skill(
                    4,
                    "Eviscerate",
                    "Profession_1",
                    serde_json::json!({"cost": 10}),
                )],
                true,
            ),
            (
                "Mesmer",
                vec![api_skill(
                    5,
                    "Mind Wrack",
                    "Profession_1",
                    serde_json::json!({"cost": 1}),
                )],
                true,
            ),
            (
                "Necromancer",
                vec![
                    api_skill(
                        6,
                        "Reaper's Shroud",
                        "Profession_1",
                        serde_json::json!({"flip_skill": 7}),
                    ),
                    api_skill(
                        8,
                        "Gravedigger",
                        "Weapon_2",
                        serde_json::json!({"facts": [{"type": "Percent", "text": "Life Force", "percent": 8.0}]}),
                    ),
                ],
                true,
            ),
            (
                "Guardian",
                vec![api_skill(
                    9,
                    "Virtue of Justice",
                    "Profession_1",
                    serde_json::json!({}),
                )],
                false,
            ),
            (
                "Elementalist",
                vec![api_skill(
                    10,
                    "Fire Attunement",
                    "Profession_1",
                    serde_json::json!({}),
                )],
                false,
            ),
            (
                "Engineer",
                vec![api_skill(
                    11,
                    "Toolbelt",
                    "Profession_1",
                    serde_json::json!({}),
                )],
                false,
            ),
            (
                "Ranger",
                vec![api_skill(
                    12,
                    "Pet Swap",
                    "Profession_1",
                    serde_json::json!({}),
                )],
                false,
            ),
        ];
        let ctx = BalanceContext::new(GameMode::WvW);
        let validated = ValidatedBuild::default();
        for (profession, skills, expected) in cases {
            let mut db = GameDb::empty_for_tests();
            let rotation: Vec<rotation::RotationSkill> =
                skills.iter().map(|s| rotation_skill(s.id, 1)).collect();
            for skill in skills {
                db.skills.insert(skill.id, skill);
            }
            let (_, complete, _) =
                wvw_resource_rules(&validated, &rotation, &db, profession, &ctx, 20_000.0);
            assert_eq!(complete, expected, "{profession}");
        }
    }

    /// Helpers for the resource-rule fixtures below.
    fn resource_case(
        profession: &str,
        specs: &[(&str, Vec<u32>)],
        skills: Vec<serde_json::Value>,
        weapon_set: u8,
    ) -> (
        Vec<rotation::wvw_timeline::SkillResourceRule>,
        bool,
        Vec<String>,
    ) {
        let mut db = GameDb::empty_for_tests();
        let mut rotation = Vec::new();
        for value in skills {
            let skill: gw2_api::models::Skill =
                serde_json::from_value(value).expect("fixture skill");
            rotation.push(rotation::RotationSkill {
                targets: 1,
                categories: skill.categories.clone(),
                slot_name: skill.slot.clone(),
                skill_id: skill.id,
                name: skill.name.clone(),
                slot: rotation::SkillSlot::Utility,
                cast_time_ms: 500,
                cooldown_ms: 5_000,
                effects: Vec::new(),
                next_chain: None,
                is_stunbreak: false,
                reaches_allies: false,
                weapon_set,
            });
            db.skills.insert(skill.id, skill);
        }
        let validated = ValidatedBuild {
            specializations: specs
                .iter()
                .map(|(name, traits)| crate::validation::ValidatedSpec {
                    spec_id: 1,
                    name: (*name).to_string(),
                    elite: true,
                    trait_ids: traits.clone(),
                    trait_names: Vec::new(),
                    all_trait_ids: traits.clone(),
                })
                .collect(),
            ..Default::default()
        };
        let ctx = BalanceContext::new(GameMode::WvW);
        wvw_resource_rules(&validated, &rotation, &db, profession, &ctx, 20_000.0)
    }

    /// Wiki `Illusion`: clones and phantasms come from skills MARKED as
    /// Clone or Phantasm skills, which the API publishes in `categories`.
    /// Mentioning a clone in prose is not generating one.
    #[test]
    fn only_marked_skills_generate_illusions() {
        use rotation::wvw_timeline::ResourceKind;
        let (rules, _, _) = resource_case(
            "Mesmer",
            &[],
            vec![
                serde_json::json!({"id": 10218, "name": "Phantasmal Berserker",
                    "slot": "Weapon_2", "facts": [], "categories": ["Phantasm"]}),
                serde_json::json!({"id": 10169, "name": "Mirror Blade",
                    "slot": "Weapon_3", "facts": [], "categories": ["Clone"]}),
                serde_json::json!({"id": 10287, "name": "Signet of Illusions",
                    "slot": "Utility", "facts": [], "categories": ["Signet"],
                    "description": "Your illusions and clones are stronger."}),
            ],
            1,
        );
        let generators: Vec<u32> = rules
            .iter()
            .filter(|rule| rule.kind == ResourceKind::Illusions && rule.gain_on_hit > 0.0)
            .map(|rule| rule.skill_id)
            .collect();
        assert_eq!(
            generators,
            vec![10218, 10169],
            "the signet only talks about illusions"
        );
    }

    /// Wiki `Flow`: a Bladesworn's profession bar spends flow, not
    /// adrenaline, and flow cannot fuel a core burst.
    #[test]
    fn a_bladesworn_profession_bar_spends_flow() {
        use rotation::wvw_timeline::ResourceKind;
        let dragon_trigger = serde_json::json!({
            "id": 62803, "name": "Dragon Trigger", "slot": "Profession_1",
            "facts": [], "cost": 100
        });
        let (flow_rules, _, _) = resource_case(
            "Warrior",
            &[("Bladesworn", vec![])],
            vec![dragon_trigger.clone()],
            1,
        );
        let flow = flow_rules.first().expect("a rule for the trigger");
        assert_eq!(flow.kind, ResourceKind::Flow);
        assert_eq!(flow.cost, FLOW_PER_DRAGON_SLASH_CHARGE);
        assert_eq!(flow.pool_regen_per_second, FLOW_PER_SECOND_IN_COMBAT);

        // Core Warrior keeps adrenaline: flow is a Bladesworn pool only.
        let (core_rules, _, _) = resource_case("Warrior", &[], vec![dragon_trigger], 1);
        assert_eq!(
            core_rules.first().expect("a rule").kind,
            ResourceKind::Adrenaline,
            "core warrior cannot spend flow"
        );
    }

    /// Wiki `Preparedness` (trait 1232): +3 maximum initiative, so the
    /// rules carry a 15-point ceiling instead of 12.
    #[test]
    fn preparedness_raises_the_initiative_cap_on_the_rules() {
        let heartseeker = serde_json::json!({
            "id": 13012, "name": "Heartseeker", "slot": "Weapon_2",
            "facts": [], "initiative": 3
        });
        let (plain, _, _) = resource_case("Thief", &[], vec![heartseeker.clone()], 1);
        assert_eq!(plain[0].pool_cap, THIEF_INITIATIVE_CAP);
        let (prepared, _, _) = resource_case(
            "Thief",
            &[("Trickery", vec![PREPAREDNESS_TRAIT_ID])],
            vec![heartseeker],
            1,
        );
        assert_eq!(prepared[0].pool_cap, THIEF_INITIATIVE_CAP + 3.0);
    }

    /// Wiki `Energy` lists every upkeep skill and its value; the API
    /// publishes none of them.
    #[test]
    fn maintained_revenant_skills_carry_their_upkeep() {
        let (rules, _, _) = resource_case(
            "Revenant",
            &[],
            vec![
                serde_json::json!({"id": 27220, "name": "Facet of Light",
                    "slot": "Heal", "facts": [], "cost": 0}),
                serde_json::json!({"id": 27107, "name": "Impossible Odds",
                    "slot": "Utility", "facts": [], "cost": 0}),
                serde_json::json!({"id": 26937, "name": "Phase Traversal",
                    "slot": "Utility", "facts": [], "cost": 10}),
            ],
            1,
        );
        let upkeep = |id: u32| {
            rules
                .iter()
                .find(|rule| rule.skill_id == id)
                .map(|rule| rule.upkeep)
                .expect("rule")
        };
        assert_eq!(upkeep(27220), 1.0);
        assert_eq!(upkeep(27107), 6.0);
        assert_eq!(upkeep(26937), 0.0, "a one-shot skill maintains nothing");
    }

    /// An unmodelled mechanic never reads as a verified pass: the gap is
    /// named, per profession and per elite specialization.
    #[test]
    fn unmodelled_mechanics_are_named() {
        let virtue = serde_json::json!({
            "id": 9152, "name": "Virtue of Justice", "slot": "Profession_1", "facts": []
        });
        let (_, complete, gaps) = resource_case("Guardian", &[], vec![virtue], 1);
        assert!(!complete);
        assert_eq!(gaps, vec!["virtues, tomes and pages".to_string()]);

        let heartseeker = serde_json::json!({
            "id": 13012, "name": "Heartseeker", "slot": "Weapon_2",
            "facts": [], "initiative": 3
        });
        let (_, complete, gaps) =
            resource_case("Thief", &[("Deadeye", vec![])], vec![heartseeker], 1);
        assert!(complete, "initiative itself is modelled");
        assert_eq!(
            gaps,
            vec!["malice".to_string()],
            "and malice is still named"
        );
    }

    /// A Warrior burst is priced in adrenaline STRIKES, whatever number the
    /// API prints in `cost`. Eviscerate publishes 100 and Combustive Shot
    /// 1000 against a 30-strike bar; copying either raw made the skill
    /// permanently unaffordable and refused every Warrior on
    /// `ResourceLegality`.
    #[test]
    fn a_warrior_burst_costs_one_bar_whatever_the_api_prints() {
        use rotation::wvw_timeline::ResourceKind;
        let ctx = BalanceContext::new(GameMode::WvW);
        let validated = ValidatedBuild::default();
        for api_cost in [10, 30, 100, 1000] {
            let skill: gw2_api::models::Skill = serde_json::from_value(serde_json::json!({
                "id": 14353, "name": "Eviscerate", "slot": "Profession_1",
                "facts": [], "cost": api_cost
            }))
            .expect("skill");
            let mut db = GameDb::empty_for_tests();
            let rotation = vec![rotation::RotationSkill {
                targets: 1,
                categories: Vec::new(),
                slot_name: None,
                skill_id: 14353,
                name: "Eviscerate".into(),
                slot: rotation::SkillSlot::Profession,
                cast_time_ms: 500,
                cooldown_ms: 8_000,
                effects: Vec::new(),
                next_chain: None,
                is_stunbreak: false,
                reaches_allies: false,
                weapon_set: 1,
            }];
            db.skills.insert(skill.id, skill);
            let (rules, _, _) =
                wvw_resource_rules(&validated, &rotation, &db, "Warrior", &ctx, 20_000.0);
            let rule = rules
                .iter()
                .find(|r| r.kind == ResourceKind::Adrenaline)
                .unwrap_or_else(|| panic!("no adrenaline rule for api cost {api_cost}"));
            assert_eq!(rule.cost, ADRENALINE_BAR_STRIKES, "api cost {api_cost}");
            // The bar opens at one full stage, so the burst is castable at
            // the start of the fight and never blocks a priority decision.
            assert!(rule.cost <= 30.0, "must sit inside the 30-strike cap");
            assert!(rule.spend_all, "a burst expends every full bar");
        }
    }

    // C16 / C17 / C18: unpriceable prefixes, PvP amulet misses, advisor

    /// The live `/v2/itemstats` cache holds ten rows (1041-1044, 1046-1048,
    /// 1050-1052) whose every multiplier is `0.0`, with the real numbers in the
    /// flat `value` field. The budget classifier used to take the plain maximum
    /// multiplier — `0.0` for those rows — and then *every* attribute matched
    /// the maximum and was paid the **major** rate.
    #[test]
    fn degenerate_itemstat_is_not_all_major() {
        use gw2_api::models::{ItemStat, StatAttribute};

        let attr = |attribute: &str, multiplier: f64, value: i32| StatAttribute {
            attribute: attribute.into(),
            multiplier,
            value,
        };
        // The real Berserker's: Power major, Precision and Ferocity minor.
        let healthy = ItemStat {
            id: 161,
            name: "Berserker's".into(),
            attributes: vec![
                attr("Power", 0.35, 0),
                attr("Precision", 0.25, 0),
                attr("CritDamage", 0.25, 0),
            ],
        };
        // #1046: same name, same three attributes, every multiplier 0.0.
        let degenerate = ItemStat {
            id: 1046,
            name: "Berserker's".into(),
            attributes: vec![
                attr("Power", 0.0, 32),
                attr("Precision", 0.0, 18),
                attr("CritDamage", 0.0, 18),
            ],
        };

        let budgets = data::slot_budgets::slot_budgets();
        let coat = budgets
            .get(data::SlotType::Coat, data::StatShape::ThreeStat)
            .expect("coat ThreeStat budget");
        assert!(
            coat.major > coat.minor,
            "fixture is not discriminating: major {} minor {}",
            coat.major,
            coat.minor
        );

        // Healthy row: one major, two minors — priced.
        let mut healthy_stats = stats::StatBlock::default();
        assert!(add_budget_stats_for_itemstat(
            &mut healthy_stats,
            &healthy,
            coat
        ));
        assert_eq!(healthy_stats.power, coat.major as f64);
        assert_eq!(healthy_stats.precision, coat.minor as f64);
        assert_eq!(healthy_stats.ferocity, coat.minor as f64);

        // Degenerate row: reported unpriced, and contributes nothing. The
        // failure this guards is "all three at the major rate", so assert that
        // shape explicitly rather than only that the block is empty.
        let mut degenerate_stats = stats::StatBlock::default();
        let priced = add_budget_stats_for_itemstat(&mut degenerate_stats, &degenerate, coat);
        // The defect first, so a regression reports what actually went wrong.
        assert_ne!(
            (
                degenerate_stats.power,
                degenerate_stats.precision,
                degenerate_stats.ferocity
            ),
            (coat.major as f64, coat.major as f64, coat.major as f64),
            "every attribute was paid the major rate"
        );
        assert!(
            !priced,
            "a row with no positive multiplier reported itself as priced"
        );
        assert_eq!(degenerate_stats.power, 0.0);
        assert_eq!(degenerate_stats.precision, 0.0);
        assert_eq!(degenerate_stats.ferocity, 0.0);

        // And the whole-kit view: the degenerate row must not out-total the
        // real prefix on any axis. Totals are measured from the appliers here,
        // never copied from a number in the review.
        let mut db = GameDb::empty_for_tests();
        db.itemstats.insert(161, healthy.clone());
        db.itemstats.insert(1046, degenerate.clone());
        let ctx = BalanceContext::pve();

        let mut healthy_kit = stats::StatBlock::default();
        assert!(apply_optimized_gear_stats(&mut healthy_kit, &db, Some(161), &ctx).is_none());
        let mut degenerate_kit = stats::StatBlock::default();
        assert!(
            apply_optimized_gear_stats(&mut degenerate_kit, &db, Some(1046), &ctx).is_some(),
            "an unpriceable kit reported no data quality problem"
        );
        assert!(
            healthy_kit.power > 0.0,
            "control kit priced nothing; the comparison below would prove nothing"
        );
        for (axis, healthy_axis, degenerate_axis) in [
            ("power", healthy_kit.power, degenerate_kit.power),
            ("precision", healthy_kit.precision, degenerate_kit.precision),
            ("ferocity", healthy_kit.ferocity, degenerate_kit.ferocity),
        ] {
            assert!(
                degenerate_axis <= healthy_axis,
                "{axis}: the flat-value row scored {degenerate_axis} against the real prefix {healthy_axis}"
            );
        }

        // The pool is the other half of the rule, and the half that has to be
        // tested on a row identity dedup would KEEP. #1046 above shares its
        // display name with #161 and loses the group to the lower id, so its
        // absence from the pool re-proves `canonical_itemstats_one_id_per_name`
        // and nothing else. The case C16 exists for is a degenerate row whose
        // display name is unique: nothing to lose a tie-break to, so it reaches
        // the pool on identity alone and is then priced as all-major.
        let orphan = ItemStat {
            id: 1049,
            name: "Settler's".into(),
            attributes: vec![
                attr("ConditionDamage", 0.0, 32),
                attr("Toughness", 0.0, 18),
                attr("HealingPower", 0.0, 18),
            ],
        };
        db.itemstats.insert(1049, orphan.clone());
        // Nothing else is named "Settler's" — state that as a precondition, so
        // the assertion below cannot pass for the dedup's reason.
        assert_eq!(
            db.itemstats
                .values()
                .filter(|stat| stat.name == "Settler's")
                .count(),
            1,
            "the orphan is no longer an orphan; this test would re-prove dedup"
        );
        let pool_ids: Vec<u32> = crate::itemstat_pool::canonical_itemstats(&db)
            .iter()
            .map(|stat| stat.id)
            .collect();
        assert_eq!(
            pool_ids,
            vec![161],
            "a uniquely-named flat-value row reached the prefix pool"
        );

        // And the reason it must not: priced as a kit it is all-major on three
        // axes, which is what the old classifier did to it.
        let mut orphan_stats = stats::StatBlock::default();
        assert!(!add_budget_stats_for_itemstat(
            &mut orphan_stats,
            &orphan,
            coat
        ));
        assert_ne!(
            (
                orphan_stats.condition_damage,
                orphan_stats.toughness,
                orphan_stats.healing_power
            ),
            (coat.major as f64, coat.major as f64, coat.major as f64)
        );
    }

    /// 53 of the 66 live named prefixes have no PvP amulet. Falling through to
    /// the land budget gave them 3607+ points where a legal amulet is 3000, so
    /// the unbuildable prefixes systematically outscored every legal one.
    #[test]
    fn pvp_unmatched_amulet_is_zero() {
        use gw2_api::models::{ItemStat, PvpAmulet, StatAttribute};

        let mut db = GameDb::empty_for_tests();
        let three_stat = |id: u32, name: &str| ItemStat {
            id,
            name: name.into(),
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
        };
        db.itemstats.insert(161, three_stat(161, "Berserker's"));
        // Viper's is one of the real amulet-less prefixes.
        db.itemstats.insert(1114, three_stat(1114, "Viper's"));

        let mut attrs = HashMap::new();
        attrs.insert("Power".to_string(), 1200);
        attrs.insert("Precision".to_string(), 900);
        attrs.insert("CritDamage".to_string(), 900);
        db.pvp_amulets.insert(
            4,
            PvpAmulet {
                id: 4,
                name: "Berserker Amulet".into(),
                icon: None,
                attributes: attrs,
            },
        );
        assert!(match_pvp_amulet(&db, "Viper's").is_none(), "fixture error");

        let pvp = BalanceContext::pvp();

        // Control: the matched prefix gets exactly its amulet.
        let mut matched = stats::StatBlock::default();
        assert!(apply_optimized_gear_stats(&mut matched, &db, Some(161), &pvp).is_none());
        assert_eq!(matched.power, 1200.0);
        assert_eq!(matched.precision, 900.0);
        assert_eq!(matched.ferocity, 900.0);

        // The defect: an unmatched prefix must score zero, not a land kit.
        let mut unmatched = stats::StatBlock::default();
        let reason = apply_optimized_gear_stats(&mut unmatched, &db, Some(1114), &pvp);
        // Stats first: the damage of this bug is the inflated block, and a
        // regression should say so rather than complain about a missing reason.
        assert_eq!(
            unmatched.power, 0.0,
            "PvP fell through to land budgets: {} power on a prefix with no amulet",
            unmatched.power
        );
        assert_eq!(unmatched.precision, 0.0);
        assert_eq!(unmatched.ferocity, 0.0);
        let reason = reason.expect("an unpriceable PvP kit must report why");
        assert_eq!(reason.entity, "Viper's");
        assert_eq!(reason.field, "pvp_amulet");
        assert_eq!(reason.modes, vec!["PvP".to_string()]);

        // The same prefix in PvE *does* draw land budgets — so the zero above
        // is the PvP rule firing, not an itemstat that prices to nothing.
        let mut pve = stats::StatBlock::default();
        assert!(
            apply_optimized_gear_stats(&mut pve, &db, Some(1114), &BalanceContext::pve()).is_none()
        );
        assert!(
            pve.power > matched.power,
            "PvE land kit ({}) should exceed the amulet ({}); otherwise this test cannot \
             distinguish terminal from nothing-to-apply",
            pve.power,
            matched.power
        );

        // End to end through the validated applier: a whole PvP build on an
        // amulet-less prefix carries no gear stats and says why.
        let mut build = ValidatedBuild {
            weapons: validation::ValidatedWeapons {
                set1: validation::ValidatedWeaponSet {
                    main_hand: Some("Greatsword".into()),
                    off_hand: None,
                },
                set2: Default::default(),
            },
            ..ValidatedBuild::default()
        };
        build.fill_worn_gear_slots(PrefixRef {
            itemstat_id: 1114,
            name: "Viper's".into(),
        });
        let (gear, reasons) = validated_gear_stats(&build, &db, "Necromancer", &pvp);
        assert_eq!(gear.power, 0.0);
        assert_eq!(gear.condition_damage, 0.0);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert_eq!(reasons[0].field, "pvp_amulet");
    }

    /// `SWAP: rune=` with nothing after the `=` used to make `contains("")`
    /// true for every rune, so whichever rune `db.runes` yielded first — a
    /// HashMap-ordered, run-dependent choice — got equipped.
    #[test]
    fn swap_rune_empty_is_not_wildcard() {
        use gw2_api::models::Item;

        let mut db = GameDb::empty_for_tests();
        for (id, name) in [
            (1u32, "Superior Rune of the Scholar"),
            (2, "Superior Rune of the Traveler"),
            (3, "Superior Rune of Balthazar"),
        ] {
            db.items.insert(
                id,
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
                    flags: Vec::new(),
                    game_types: Vec::new(),
                    restrictions: Vec::new(),
                    details: None,
                },
            );
            db.runes.push(id);
        }

        // Empty needle: no rune. Not "the first rune", not "a random rune".
        for empty in ["", "   ", "\"\"", "''"] {
            assert!(
                advisor_rune_pick(&db, empty).is_none(),
                "bare `rune={empty}` equipped something"
            );
        }

        // A real needle still resolves, and resolves to the *shortest* match so
        // the answer does not depend on `db.runes` iteration order. Reversing
        // the pool must not change the pick.
        let pick =
            advisor_rune_pick(&db, "Superior Rune of the").expect("a real needle must still match");
        assert_eq!(pick.name, "Superior Rune of the Scholar");
        db.runes.reverse();
        assert_eq!(
            advisor_rune_pick(&db, "Superior Rune of the").map(|r| r.id),
            Some(pick.id),
            "the pick depended on pool order"
        );
    }
    // A11-1: SWAP candidates must pass the plate slot rules before they can
    // win

    /// Scripted LLM client: always answers with the same SWAP lines.
    struct StubAdvisor {
        response: &'static str,
    }

    impl LlmClient for StubAdvisor {
        fn provider_name(&self) -> &str {
            "stub"
        }

        fn validate_key(&self) -> Result<(), crate::llm::LlmError> {
            Ok(())
        }

        fn generate(&self, _prompt: &str) -> Result<String, crate::llm::LlmError> {
            Ok(self.response.to_string())
        }

        fn generate_cached(&self, prompt: &str) -> Result<String, crate::llm::LlmError> {
            self.generate(prompt)
        }

        fn generate_with_tools_progress(
            &self,
            prompt: &str,
            _tools: &[crate::llm::ToolDefinition],
            _execute_tool: &mut dyn FnMut(&str, &serde_json::Value) -> serde_json::Value,
            _max_turns: usize,
            _on_progress: &mut dyn FnMut(usize, usize, &[String]),
        ) -> Result<String, crate::llm::LlmError> {
            self.generate(prompt)
        }

        fn list_models(&self) -> Result<Vec<crate::llm::ModelInfo>, crate::llm::LlmError> {
            Ok(Vec::new())
        }

        fn remaining_quota(&self) -> u32 {
            0
        }

        fn clear_cache(&self) {}
    }

    /// Two priceable three-stat prefixes: Berserker's (glass) and Soldier's
    /// (tanky). Nothing else — no runes, sigils, traits, or skills.
    fn advisor_gate_db() -> GameDb {
        use gw2_api::models::StatAttribute;

        let attr = |attribute: &str, multiplier: f64| StatAttribute {
            attribute: attribute.into(),
            multiplier,
            value: 0,
        };
        let mut db = GameDb::empty_for_tests();
        db.itemstats.insert(
            1,
            ItemStat {
                id: 1,
                name: "Berserker's".into(),
                attributes: vec![
                    attr("Power", 0.35),
                    attr("Precision", 0.25),
                    attr("CritDamage", 0.25),
                ],
            },
        );
        db.itemstats.insert(
            2,
            ItemStat {
                id: 2,
                name: "Soldier's".into(),
                attributes: vec![
                    attr("Power", 0.35),
                    attr("Toughness", 0.25),
                    attr("Vitality", 0.25),
                ],
            },
        );
        db
    }

    /// Warrior in a Greatsword, Berserker's on every worn slot. The off-hand
    /// holds nothing, so `wears(WeaponSet1Off)` is false — the slot an invalid
    /// proposal would write to.
    fn advisor_gate_build() -> ValidatedBuild {
        let mut build = ValidatedBuild::default();
        build.weapons.set1.main_hand = Some("Greatsword".into());
        build.fill_worn_gear_slots(PrefixRef {
            itemstat_id: 1,
            name: "Berserker's".into(),
        });
        build
    }

    /// PvE solo scenario (only the EHP viability gate runs) with sustain as
    /// the only scoring axis, so the tankier prefix is the better build.
    fn advisor_gate_inputs() -> (
        BalanceContext,
        crate::scenario::ScenarioSpec,
        OptimizationWeights,
        gw2_core::types::BuildLocks,
    ) {
        let ctx = BalanceContext::new(GameMode::PvE);
        let scenario = crate::scenario::ScenarioSpec::from_balance_context(&ctx);
        let weights = OptimizationWeights {
            power: 0.0,
            condition: 0.0,
            boon_support: 0.0,
            healing: 0.0,
            sustain: 1.0,
            control: 0.0,
        };
        (
            ctx,
            scenario,
            weights,
            gw2_core::types::BuildLocks::default(),
        )
    }

    /// A SWAP onto a slot the build does not wear (the off-hand beside a
    /// Greatsword) is exactly what `validate_gear_slot_map` ignores on a
    /// plate. The phantom prefix is referee-neutral — the stat path prices it
    /// to nothing — so the rank comparison alone cannot keep the invalid
    /// candidate out; the A11-1 gate must, and does.
    #[test]
    fn advisor_swap_onto_unworn_slot_cannot_win() {
        let db = advisor_gate_db();
        let current = advisor_gate_build();
        let (ctx, scenario, weights, locks) = advisor_gate_inputs();
        assert!(
            !current.wears(GearSlot::WeaponSet1Off),
            "fixture must hold a two-hander"
        );

        // The invalid candidate evaluates to the *same* rank as the current
        // build: nothing in the referee rejects it. That is why the gate
        // exists — and why this assertion doubles as proof that only the gate
        // keeps the phantom prefix out of the returned build.
        let mut phantom = current.clone();
        phantom.gear_slots.set(
            GearSlot::WeaponSet1Off,
            PrefixRef {
                itemstat_id: 2,
                name: "Soldier's".into(),
            },
        );
        let rank_of = |build: &ValidatedBuild| {
            crate::referee::search_rank(&crate::referee::evaluate_validated_build(
                build, &db, "Warrior", &weights, &ctx, &scenario,
            ))
        };
        assert_eq!(
            rank_of(&phantom),
            rank_of(&current),
            "fixture drift: the phantom prefix should be referee-neutral"
        );

        let advisor = StubAdvisor {
            response: "SWAP: gear weapon-set-1-off Soldier's",
        };
        let result = llm_advisor(
            current.clone(),
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks,
            &advisor,
        );
        assert!(
            result
                .gear_slots
                .prefix_id(GearSlot::WeaponSet1Off)
                .is_none(),
            "a prefix on a hand that holds nothing must never be accepted"
        );
        assert_eq!(
            result.gear_identity(),
            current.gear_identity(),
            "with no legal proposal on the table the build must come back unchanged"
        );
    }

    /// The gate is a floor, not a wall: a legal uniform swap that out-ranks
    /// the current build still wins exactly as before.
    #[test]
    fn advisor_legal_uniform_swap_still_wins() {
        let db = advisor_gate_db();
        let current = advisor_gate_build();
        let (ctx, scenario, weights, locks) = advisor_gate_inputs();

        let advisor = StubAdvisor {
            response: "SWAP: gear Soldier's",
        };
        let result = llm_advisor(
            current.clone(),
            &db,
            "Warrior",
            &weights,
            &ctx,
            &scenario,
            &locks,
            &advisor,
        );
        assert_eq!(
            result.gear_slots.prefix_id(GearSlot::Helm),
            Some(2),
            "the better legal prefix must win"
        );
        assert_eq!(
            result.primary_prefix().map(|p| p.name.as_str()),
            Some("Soldier's")
        );
        assert!(
            result
                .gear_slots
                .prefix_id(GearSlot::WeaponSet1Off)
                .is_none(),
            "the uniform fill never dresses an empty hand"
        );
        assert_ne!(
            result.gear_identity(),
            current.gear_identity(),
            "a winning swap changes the build"
        );
    }

    /// The gate predicate itself: clean builds pass; any populated slot the
    /// build does not wear fails — including the carried second weapon set.
    #[test]
    fn advisor_candidate_slots_legal_rejects_unworn_slots() {
        let clean = advisor_gate_build();
        assert!(advisor_candidate_slots_legal(&clean));

        for slot in [
            GearSlot::WeaponSet1Off,
            GearSlot::WeaponSet2Main,
            GearSlot::WeaponSet2Off,
        ] {
            let mut dirty = clean.clone();
            dirty.gear_slots.set(
                slot,
                PrefixRef {
                    itemstat_id: 2,
                    name: "Soldier's".into(),
                },
            );
            assert!(
                !advisor_candidate_slots_legal(&dirty),
                "a prefix on unworn {slot:?} must fail the gate"
            );
        }

        // A prefix on a worn slot (the two-hander's main hand) stays legal.
        let mut legal = clean.clone();
        legal.gear_slots.set(
            GearSlot::WeaponSet1Main,
            PrefixRef {
                itemstat_id: 2,
                name: "Soldier's".into(),
            },
        );
        assert!(advisor_candidate_slots_legal(&legal));
    }

    /// Tier 3 must stop when the user cancels.
    ///
    /// Only `optimize_v2` ever saw the cancellation token, so cancelling a run
    /// that had already fallen through to the legacy pipeline did nothing: the
    /// worker ran the whole `gear_candidates × spec_combos` combat sweep to
    /// completion and then wrote its result back over a request the user had
    /// abandoned — inside the game process, on a thread `on_unload` has to join.
    #[test]
    fn legacy_optimize_observes_cancel() {
        use gw2_api::models::{ItemStat, Specialization, StatAttribute};
        use std::cell::Cell;

        let mut itemstats: HashMap<u32, ItemStat> = HashMap::new();
        for (id, name) in [
            (161u32, "Berserker's"),
            (1099, "Cavalier's"),
            (1128, "Marauder's"),
        ] {
            itemstats.insert(
                id,
                ItemStat {
                    id,
                    name: name.into(),
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
        }

        let mut specs: HashMap<u32, Specialization> = HashMap::new();
        for id in 1u32..=4 {
            specs.insert(
                id,
                Specialization {
                    id,
                    name: format!("Spec {id}"),
                    profession: "Warrior".into(),
                    elite: id == 4,
                    minor_traits: vec![],
                    major_traits: vec![],
                    weapon_trait: None,
                    icon: None,
                    background: None,
                    profession_icon: None,
                    profession_icon_big: None,
                },
            );
        }

        let profession = Profession {
            id: "Warrior".into(),
            name: "Warrior".into(),
            code: None,
            specializations: vec![1, 2, 3, 4],
            weapons: HashMap::new(),
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };

        let traits: HashMap<u32, GW2Trait> = HashMap::new();
        let items: HashMap<u32, Item> = HashMap::new();
        let weights = OptimizationWeights::default();
        let ctx = BalanceContext::pve();
        let locks = gw2_core::types::BuildLocks::default();
        let amulets: HashMap<u32, PvpAmulet> = HashMap::new();

        let run = |is_cancelled: &dyn Fn() -> bool| {
            optimize_cancellable(
                &profession,
                &weights,
                None,
                &items,
                &itemstats,
                &specs,
                &traits,
                |_| {},
                3,
                &ctx,
                &locks,
                &amulets,
                is_cancelled,
            )
        };

        // Control: the same inputs with no cancellation produce a real result.
        // Without this, "Cancelled" could just mean "this fixture never works".
        let ok = run(&|| false).expect("uncancelled run must produce candidates");
        assert!(!ok.is_empty(), "control run produced no candidates");

        // Cancelled from the very first probe.
        assert_eq!(run(&|| true).unwrap_err(), "Cancelled");

        // Cancelled part-way: the probe fires only after it has been asked a
        // few times, so the run is genuinely under way. It must still abandon,
        // and it must never return a partial candidate list that a caller could
        // mistake for a result.
        for trip_after in [1usize, 2, 3, 4] {
            let asked = Cell::new(0usize);
            let probe = || {
                asked.set(asked.get() + 1);
                asked.get() > trip_after
            };
            assert_eq!(
                run(&probe).unwrap_err(),
                "Cancelled",
                "trip_after {trip_after} ran to completion"
            );
            assert!(
                asked.get() > trip_after,
                "the probe was never asked enough times to fire"
            );
        }

        // PvP takes a different branch inside the same function; it observes
        // cancellation too.
        let mut pvp_amulets: HashMap<u32, PvpAmulet> = HashMap::new();
        pvp_amulets.insert(
            4,
            PvpAmulet {
                id: 4,
                name: "Berserker Amulet".into(),
                icon: None,
                attributes: HashMap::from([("Power".to_string(), 1200)]),
            },
        );
        let pvp_ctx = BalanceContext::pvp();
        let pvp_run = |is_cancelled: &dyn Fn() -> bool| {
            optimize_cancellable(
                &profession,
                &weights,
                None,
                &items,
                &itemstats,
                &specs,
                &traits,
                |_| {},
                3,
                &pvp_ctx,
                &locks,
                &pvp_amulets,
                is_cancelled,
            )
        };
        assert!(
            pvp_run(&|| false).is_ok(),
            "PvP control run must produce candidates"
        );
        assert_eq!(pvp_run(&|| true).unwrap_err(), "Cancelled");

        // `optimize` is the uncancellable wrapper the addon still calls; it must
        // behave exactly like a never-cancelled `optimize_cancellable`.
        let via_wrapper = optimize(
            &profession,
            &weights,
            None,
            &items,
            &itemstats,
            &specs,
            &traits,
            |_| {},
            3,
            &ctx,
            &locks,
            &amulets,
        )
        .expect("wrapper must still work");
        assert_eq!(via_wrapper.len(), ok.len());
    }

    #[test]
    fn parse_slot_qualifier_recognises_kebab_compact_and_bare_forms() {
        use gw2_core::types::GearSlot;

        // Kebab and compact forms resolve to the same slot, prefix text follows.
        assert_eq!(
            parse_slot_qualifier("ring-1 marauder's"),
            Some((GearSlot::Ring1, "marauder's"))
        );
        assert_eq!(
            parse_slot_qualifier("RING2 Cavalier's"),
            Some((GearSlot::Ring2, "Cavalier's"))
        );
        assert_eq!(
            parse_slot_qualifier("weapon-set-2-main Zojja's Reaver"),
            Some((GearSlot::WeaponSet2Main, "Zojja's Reaver"))
        );
        // A multi-word body without a leading slot keyword is bare/uniform —
        // the whole text is the prefix name.
        assert_eq!(parse_slot_qualifier("berserker's"), None);
        // Two-hander slots are reachable too.
        assert_eq!(
            parse_slot_qualifier("weapon-set-1-off Sinister"),
            Some((GearSlot::WeaponSet1Off, "Sinister"))
        );
    }

    #[test]
    fn parse_slot_qualifier_uniform_fallback_for_unknown_first_token() {
        use gw2_core::types::GearSlot;

        // First token not a slot → uniform proposal, even when it contains
        // an inner space (multi-word prefix names).
        assert_eq!(parse_slot_qualifier("superior stuff"), None);
        // Sanity: a real keyword with the trailing prefix attached.
        let (slot, rest) = parse_slot_qualifier("amulet valkyrie").unwrap();
        assert_eq!(slot, GearSlot::Amulet);
        assert_eq!(rest, "valkyrie");
    }

    #[test]
    fn test_slot_budget_lookups() {
        // Verify slot budget data is accessible and returns expected ThreeStat major values.
        // Source: data/slot_budgets/level80_ascended.json (verified from GW2 API items)
        let budgets = data::slot_budgets::slot_budgets();
        assert_eq!(
            budgets.major_for_api_slot("Coat"),
            141,
            "Coat ThreeStat major should be 141"
        );
        assert_eq!(
            budgets.major_for_api_slot("Helm"),
            63,
            "Helm ThreeStat major should be 63"
        );
        assert_eq!(
            budgets.major_for_api_slot("Amulet"),
            157,
            "Amulet ThreeStat major should be 157"
        );
        assert_eq!(
            budgets.major_for_api_slot("Leggings"),
            94,
            "Leggings ThreeStat major should be 94"
        );
        // WeaponA1 maps to WeaponTwoHand
        assert_eq!(
            budgets.major_for_api_slot("WeaponA1"),
            251,
            "WeaponA1 (TwoHand) ThreeStat major should be 251"
        );
        // WeaponA2 maps to WeaponOneHand
        assert_eq!(
            budgets.major_for_api_slot("WeaponA2"),
            125,
            "WeaponA2 (OneHand) ThreeStat major should be 125"
        );
    }

    #[test]
    fn test_select_best_major_traits_picks_one_per_column() {
        // Create 9 traits: 3 columns × 3 rows
        // Column 0 (Adept): traits 100, 101, 102
        // Column 1 (Master): traits 200, 201, 202
        // Column 2 (Grandmaster): traits 300, 301, 302
        let major_traits = vec![100, 101, 102, 200, 201, 202, 300, 301, 302];

        // With no traits in cache, should still return 3 traits (one per column)
        let traits_cache = HashMap::new();
        let power_weights = OptimizationWeights::preset_power_dps().to_stat_weights();
        let no_locks = gw2_core::types::BuildLocks::default();
        let selected =
            select_best_major_traits(&major_traits, &power_weights, &traits_cache, &no_locks, 0);
        assert_eq!(selected.len(), 3);
        // Each should come from a different column
        assert!(major_traits[0..3].contains(&selected[0]));
        assert!(major_traits[3..6].contains(&selected[1]));
        assert!(major_traits[6..9].contains(&selected[2]));
    }

    #[test]
    fn test_select_best_major_traits_prefers_power_for_power_dps() {
        use gw2_api::models::Fact;

        let major_traits = vec![100, 101, 102, 200, 201, 202, 300, 301, 302];
        let mut traits_cache = HashMap::new();

        // Trait 100: gives +150 Power (good for PowerDPS)
        traits_cache.insert(
            100,
            GW2Trait {
                id: 100,
                name: "Power Trait".into(),
                tier: 1,
                order: 0,
                description: None,
                slot: "Major".into(),
                icon: None,
                specialization: 1,
                skills: vec![],
                facts: vec![Fact::AttributeAdjust {
                    text: Some("Power".into()),
                    icon: None,
                    value: Some(150),
                    target: Some("Power".into()),
                }],
                traited_facts: vec![],
                fact_parse_drops: 0,
            },
        );
        // Trait 101: gives +150 Vitality (bad for PowerDPS)
        traits_cache.insert(
            101,
            GW2Trait {
                id: 101,
                name: "Vitality Trait".into(),
                tier: 1,
                order: 1,
                description: None,
                slot: "Major".into(),
                icon: None,
                specialization: 1,
                skills: vec![],
                facts: vec![Fact::AttributeAdjust {
                    text: Some("Vitality".into()),
                    icon: None,
                    value: Some(150),
                    target: Some("Vitality".into()),
                }],
                traited_facts: vec![],
                fact_parse_drops: 0,
            },
        );
        // Trait 102: nothing
        traits_cache.insert(
            102,
            GW2Trait {
                id: 102,
                name: "Empty Trait".into(),
                tier: 1,
                order: 2,
                description: None,
                slot: "Major".into(),
                icon: None,
                specialization: 1,
                skills: vec![],
                facts: vec![],
                traited_facts: vec![],
                fact_parse_drops: 0,
            },
        );

        let power_weights = OptimizationWeights::preset_power_dps().to_stat_weights();
        let no_locks = gw2_core::types::BuildLocks::default();
        let selected =
            select_best_major_traits(&major_traits, &power_weights, &traits_cache, &no_locks, 1);
        // First column should select trait 100 (Power bonus)
        assert_eq!(
            selected[0], 100,
            "PowerDPS should prefer Power trait over Vitality"
        );
    }

    #[test]
    fn score_fact_ignores_tooltip_effect_amounts() {
        let tooltip_amount = Fact::AttributeAdjust {
            text: Some("Life Siphon Damage".into()),
            icon: None,
            value: Some(3517),
            target: Some("Power".into()),
        };
        let power_weights = OptimizationWeights::preset_power_dps().to_stat_weights();

        assert_eq!(score_fact(&tooltip_amount, &power_weights), 0.0);
    }

    #[test]
    fn test_optimize_returns_candidates() {
        let mut itemstats = HashMap::new();
        itemstats.insert(
            584,
            ItemStat {
                id: 584,
                name: "Berserker's".into(),
                attributes: vec![
                    gw2_api::models::StatAttribute {
                        attribute: "Power".into(),
                        multiplier: 0.35,
                        value: 32,
                    },
                    gw2_api::models::StatAttribute {
                        attribute: "Precision".into(),
                        multiplier: 0.25,
                        value: 18,
                    },
                    gw2_api::models::StatAttribute {
                        attribute: "CritDamage".into(),
                        multiplier: 0.25,
                        value: 18,
                    },
                ],
            },
        );

        let profession = Profession {
            id: "Warrior".into(),
            name: "Warrior".into(),
            code: None,
            specializations: vec![1, 2, 3, 4, 5],
            weapons: HashMap::new(),
            training: Vec::new(),
            skills_by_palette: Vec::new(),
            icon: None,
            icon_big: None,
        };

        let mut specs = HashMap::new();
        for id in 1..=5u32 {
            specs.insert(
                id,
                Specialization {
                    id,
                    name: format!("Spec{}", id),
                    profession: "Warrior".into(),
                    elite: false,
                    minor_traits: Vec::new(),
                    major_traits: Vec::new(),
                    weapon_trait: None,
                    icon: None,
                    background: None,
                    profession_icon: None,
                    profession_icon_big: None,
                },
            );
        }

        let no_locks = gw2_core::types::BuildLocks::default();
        let ctx = crate::balance::BalanceContext::pve();
        let candidates = optimize(
            &profession,
            &OptimizationWeights::preset_power_dps(),
            None,
            &HashMap::new(),
            &itemstats,
            &specs,
            &HashMap::new(),
            |_| {},
            3,
            &ctx,
            &no_locks,
            &HashMap::new(), // no PvP amulets needed for PvE
        )
        .expect("optimize() should succeed with valid data");

        assert!(!candidates.is_empty());
        // Should be sorted by score descending
        for i in 1..candidates.len() {
            assert!(candidates[i - 1].score >= candidates[i].score);
        }
    }

    /// Helper to build a minimal Warrior profession with 5 core specs for PvP tests.
    fn test_warrior_profession_and_specs() -> (Profession, HashMap<u32, Specialization>) {
        let profession = Profession {
            id: "Warrior".into(),
            name: "Warrior".into(),
            code: None,
            specializations: vec![1, 2, 3, 4, 5],
            weapons: HashMap::new(),
            training: Vec::new(),
            skills_by_palette: Vec::new(),
            icon: None,
            icon_big: None,
        };
        let mut specs = HashMap::new();
        for id in 1..=5u32 {
            specs.insert(
                id,
                Specialization {
                    id,
                    name: format!("Spec{}", id),
                    profession: "Warrior".into(),
                    elite: false,
                    minor_traits: Vec::new(),
                    major_traits: Vec::new(),
                    weapon_trait: None,
                    icon: None,
                    background: None,
                    profession_icon: None,
                    profession_icon_big: None,
                },
            );
        }
        (profession, specs)
    }

    #[test]
    fn test_pvp_mode_dispatches_to_pvp_path() {
        let (profession, specs) = test_warrior_profession_and_specs();
        let mut pvp_amulets = HashMap::new();
        pvp_amulets.insert(
            4,
            PvpAmulet {
                id: 4,
                name: "Assassin Amulet".into(),
                icon: None,
                attributes: {
                    let mut m = HashMap::new();
                    m.insert("Power".into(), 900);
                    m.insert("Precision".into(), 1200);
                    m.insert("CritDamage".into(), 900);
                    m
                },
            },
        );

        let no_locks = gw2_core::types::BuildLocks::default();
        let ctx = crate::balance::BalanceContext::pvp();
        let candidates = optimize(
            &profession,
            &OptimizationWeights::preset_power_dps(),
            None,
            &HashMap::new(),
            &HashMap::new(), // no itemstats needed for PvP
            &specs,
            &HashMap::new(),
            |_| {},
            3,
            &ctx,
            &no_locks,
            &pvp_amulets,
        )
        .expect("PvP optimize should succeed with amulet data");

        assert!(!candidates.is_empty());
        // All PvP candidates should have a pvp_amulet set
        for c in &candidates {
            assert!(
                c.pvp_amulet.is_some(),
                "PvP candidate should have pvp_amulet set"
            );
        }
    }

    #[test]
    fn test_pvp_amulet_stats_applied_to_base() {
        let (profession, specs) = test_warrior_profession_and_specs();
        let mut pvp_amulets = HashMap::new();
        pvp_amulets.insert(
            4,
            PvpAmulet {
                id: 4,
                name: "Assassin Amulet".into(),
                icon: None,
                attributes: {
                    let mut m = HashMap::new();
                    m.insert("Power".into(), 900);
                    m.insert("Precision".into(), 1200);
                    m.insert("CritDamage".into(), 900);
                    m
                },
            },
        );

        let no_locks = gw2_core::types::BuildLocks::default();
        let ctx = crate::balance::BalanceContext::pvp();
        let candidates = optimize(
            &profession,
            &OptimizationWeights::preset_power_dps(),
            None,
            &HashMap::new(),
            &HashMap::new(),
            &specs,
            &HashMap::new(),
            |_| {},
            3,
            &ctx,
            &no_locks,
            &pvp_amulets,
        )
        .expect("PvP optimize should succeed");

        // With Assassin Amulet: base 1000 + amulet 900 Power = 1900
        let c = &candidates[0];
        assert!(
            (c.stats.power - 1900.0).abs() < 1.0,
            "Power should be base 1000 + 900 amulet = 1900, got {}",
            c.stats.power
        );
        assert!(
            (c.stats.precision - 2200.0).abs() < 1.0,
            "Precision should be base 1000 + 1200 amulet = 2200, got {}",
            c.stats.precision
        );
        // CritDamage maps to ferocity (base 0 + 900)
        assert!(
            (c.stats.ferocity - 900.0).abs() < 1.0,
            "Ferocity should be 0 base + 900 amulet = 900, got {}",
            c.stats.ferocity
        );
    }

    #[test]
    fn test_pvp_error_on_empty_amulets() {
        let (profession, specs) = test_warrior_profession_and_specs();
        let no_locks = gw2_core::types::BuildLocks::default();
        let ctx = crate::balance::BalanceContext::pvp();
        let result = optimize(
            &profession,
            &OptimizationWeights::preset_power_dps(),
            None,
            &HashMap::new(),
            &HashMap::new(),
            &specs,
            &HashMap::new(),
            |_| {},
            3,
            &ctx,
            &no_locks,
            &HashMap::new(), // empty pvp_amulets
        );

        assert!(
            result.is_err(),
            "PvP optimization should error with no amulet data"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("No PvP amulet data"),
            "Error should mention missing amulet data, got: {}",
            err
        );
    }

    #[test]
    fn test_pvp_no_slot_budgets_used() {
        // PvP path should work even with no itemstats (slot budgets not used)
        let (profession, specs) = test_warrior_profession_and_specs();
        let mut pvp_amulets = HashMap::new();
        pvp_amulets.insert(
            1,
            PvpAmulet {
                id: 1,
                name: "Test Amulet".into(),
                icon: None,
                attributes: {
                    let mut m = HashMap::new();
                    m.insert("Power".into(), 500);
                    m
                },
            },
        );

        let no_locks = gw2_core::types::BuildLocks::default();
        let ctx = crate::balance::BalanceContext::pvp();
        // Pass completely empty itemstats — PvP path should not need them
        let result = optimize(
            &profession,
            &OptimizationWeights::preset_power_dps(),
            None,
            &HashMap::new(),
            &HashMap::new(), // empty itemstats
            &specs,
            &HashMap::new(),
            |_| {},
            3,
            &ctx,
            &no_locks,
            &pvp_amulets,
        );

        assert!(
            result.is_ok(),
            "PvP path should succeed without itemstats: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_pve_candidates_have_no_pvp_amulet() {
        let mut itemstats = HashMap::new();
        itemstats.insert(
            584,
            ItemStat {
                id: 584,
                name: "Berserker's".into(),
                attributes: vec![
                    gw2_api::models::StatAttribute {
                        attribute: "Power".into(),
                        multiplier: 0.35,
                        value: 32,
                    },
                    gw2_api::models::StatAttribute {
                        attribute: "Precision".into(),
                        multiplier: 0.25,
                        value: 18,
                    },
                    gw2_api::models::StatAttribute {
                        attribute: "CritDamage".into(),
                        multiplier: 0.25,
                        value: 18,
                    },
                ],
            },
        );
        let (profession, specs) = test_warrior_profession_and_specs();
        let no_locks = gw2_core::types::BuildLocks::default();
        let ctx = crate::balance::BalanceContext::pve();
        let candidates = optimize(
            &profession,
            &OptimizationWeights::preset_power_dps(),
            None,
            &HashMap::new(),
            &itemstats,
            &specs,
            &HashMap::new(),
            |_| {},
            3,
            &ctx,
            &no_locks,
            &HashMap::new(),
        )
        .expect("PvE optimize should succeed");

        for c in &candidates {
            assert!(
                c.pvp_amulet.is_none(),
                "PvE candidates should have pvp_amulet = None"
            );
        }
    }

    #[test]
    fn simulate_validated_rotation_counts_cleanse_from_skill_facts() {
        let cleanse_skill = gw2_api::models::Skill {
            id: 90_001,
            name: "Cleanse Utility".into(),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: None,
            professions: vec!["Warrior".into()],
            slot: Some("Utility".into()),
            facts: vec![
                Fact::Recharge {
                    text: Some("Recharge".into()),
                    icon: None,
                    value: Some(20.0),
                },
                Fact::Number {
                    text: Some("Conditions Removed".into()),
                    icon: None,
                    value: Some(2),
                },
            ],
            traited_facts: vec![],
            fact_parse_drops: 0,
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
        };

        let mut db = GameDb {
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
        };
        db.skills.insert(cleanse_skill.id, cleanse_skill);

        let mut validated = ValidatedBuild::default();
        validated.skills.utilities = vec![Some((90_001, "Cleanse Utility".into()))];

        let stats = stats::base_stats();
        let rotation = simulate_validated_rotation(&validated, &db, &stats, None)
            .expect("utility skill should produce a rotation");

        assert_eq!(rotation.cleanse_count, 1);
        assert!(
            rotation.cleanse_rate_per_20s > 0.0,
            "cleanse fact should contribute to cleanse rate"
        );
    }

    #[test]
    fn land_bar_skips_aquatic_palette_keeps_land_spear() {
        let land = gw2_api::models::Skill {
            id: 1,
            name: "Barbed Spear".into(),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: Some("Spear".into()),
            professions: vec!["Guardian".into()],
            slot: Some("Weapon_1".into()),
            facts: vec![],
            traited_facts: vec![],
            fact_parse_drops: 0,
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
            flags: vec!["NoUnderwater".into()],
            specialization: None,
        };
        let aquatic = gw2_api::models::Skill {
            id: 2,
            name: "Water Spear".into(),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: Some("Spear".into()),
            professions: vec!["Guardian".into()],
            slot: Some("Weapon_1".into()),
            facts: vec![],
            traited_facts: vec![],
            fact_parse_drops: 0,
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
        };
        let mut db = GameDb::empty_for_tests();
        db.skills.insert(1, land);
        db.skills.insert(2, aquatic);
        let mut weapons = HashMap::new();
        weapons.insert(
            "Spear".into(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["TwoHand".into(), "Aquatic".into()],
                skills: vec![
                    gw2_api::models::WeaponSkillRef {
                        id: 1,
                        slot: "Weapon_1".into(),
                    },
                    gw2_api::models::WeaponSkillRef {
                        id: 2,
                        slot: "Weapon_1".into(),
                    },
                ],
            },
        );
        let profession = Profession {
            id: "Guardian".into(),
            name: "Guardian".into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        let mut ids = Vec::new();
        add_weapon_skill_ids(&mut ids, &profession, "Spear", &db, Hand::Main);
        assert_eq!(
            ids,
            vec![1],
            "aquatic palette must stay off the land bar; got {ids:?}"
        );
    }

    /// The API lists all five dagger skills under "Dagger". A main-hand dagger
    /// brings slots 1-3, an off-hand dagger 4-5; Dagger/Dagger must not carry
    /// Deathly Swarm (slot 4) twice.
    #[test]
    fn one_handed_weapon_contributes_only_its_hands_slots() {
        let mut db = GameDb::empty_for_tests();
        let mut skills = Vec::new();
        for slot in 1..=5u32 {
            let s = gw2_api::models::Skill {
                id: slot,
                name: format!("Dagger {slot}"),
                description: None,
                icon: None,
                chat_link: None,
                skill_type: Some("Weapon".into()),
                weapon_type: Some("Dagger".into()),
                professions: vec!["Necromancer".into()],
                slot: Some(format!("Weapon_{slot}")),
                facts: vec![],
                traited_facts: vec![],
                fact_parse_drops: 0,
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
            };
            db.skills.insert(slot, s);
            skills.push(gw2_api::models::WeaponSkillRef {
                id: slot,
                slot: format!("Weapon_{slot}"),
            });
        }
        let mut weapons = HashMap::new();
        weapons.insert(
            "Dagger".into(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["Mainhand".into(), "Offhand".into()],
                skills,
            },
        );
        let profession = Profession {
            id: "Necromancer".into(),
            name: "Necromancer".into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        let mut ids = Vec::new();
        add_weapon_skill_ids(&mut ids, &profession, "Dagger", &db, Hand::Main);
        add_weapon_skill_ids(&mut ids, &profession, "Dagger", &db, Hand::Off);
        assert_eq!(
            ids,
            vec![1, 2, 3, 4, 5],
            "each slot exactly once; got {ids:?}"
        );
    }

    #[test]
    fn weapon_swap_policy_matches_profession_rules() {
        assert_eq!(weapon_swap_cooldown_for("Ranger", false), Some(10_000));
        assert_eq!(weapon_swap_cooldown_for("Warrior", false), Some(5_000));
        assert_eq!(weapon_swap_cooldown_for("Engineer", false), None);
        assert_eq!(weapon_swap_cooldown_for("Elementalist", false), None);
        assert_eq!(weapon_swap_cooldown_for("Warrior", true), None);
    }

    #[test]
    fn stale_trait_lock_reasons_flags_only_missing_ids() {
        let mut db = GameDb::empty_for_tests();
        db.specializations.insert(
            55,
            gw2_api::models::Specialization {
                id: 55,
                name: "Druid".into(),
                profession: "Ranger".into(),
                elite: true,
                minor_traits: Vec::new(),
                major_traits: vec![10, 11, 12, 20, 21, 22, 30, 31, 32],
                weapon_trait: None,
                icon: None,
                background: None,
                profession_icon: None,
                profession_icon_big: None,
            },
        );
        db.traits.insert(
            99,
            gw2_api::models::Trait {
                id: 99,
                name: "Ghostwritten Legacy".into(),
                icon: None,
                description: None,
                specialization: 55,
                tier: 3,
                order: 2,
                slot: "Major".into(),
                facts: Vec::new(),
                traited_facts: Vec::new(),
                fact_parse_drops: 0,
                skills: Vec::new(),
            },
        );

        let ctx = BalanceContext::new(GameMode::PvE);
        let mut locks = gw2_core::types::BuildLocks::default();
        locks.trait_locks.insert(55, [Some(10), None, Some(99)]); // col0 valid, col2 stale
        let reasons = stale_trait_lock_reasons(&locks, &db, &ctx);
        assert_eq!(reasons.len(), 1, "only the stale lock warns");
        assert_eq!(reasons[0].field, "trait_lock");
        assert!(reasons[0].entity.contains("Druid"));
        assert!(reasons[0].entity.contains("Ghostwritten Legacy"));

        // A fully valid lock set produces nothing.
        locks.trait_locks.insert(55, [Some(10), Some(21), Some(32)]);
        assert!(stale_trait_lock_reasons(&locks, &db, &ctx).is_empty());
    }

    #[test]
    fn per_slot_gear_mix_equals_independent_slot_math() {
        use gw2_api::models::{ItemStat, StatAttribute};
        use gw2_core::types::{GearSlot, PrefixRef};

        let mut db = GameDb::empty_for_tests();
        db.itemstats.insert(
            1,
            ItemStat {
                id: 1,
                name: "Berserker's".into(),
                attributes: vec![
                    StatAttribute {
                        attribute: "Power".into(),
                        multiplier: 1.0,
                        value: 0,
                    },
                    StatAttribute {
                        attribute: "Precision".into(),
                        multiplier: 0.65,
                        value: 0,
                    },
                ],
            },
        );
        db.itemstats.insert(
            2,
            ItemStat {
                id: 2,
                name: "Cavalier's".into(),
                attributes: vec![
                    StatAttribute {
                        attribute: "Toughness".into(),
                        multiplier: 1.0,
                        value: 0,
                    },
                    StatAttribute {
                        attribute: "Power".into(),
                        multiplier: 0.65,
                        value: 0,
                    },
                ],
            },
        );
        let ctx = BalanceContext::new(GameMode::PvE);

        let build = |coat_cavaliers: bool| {
            let mut validated = ValidatedBuild::default();
            let bers = PrefixRef {
                itemstat_id: 1,
                name: "Berserker's".into(),
            };
            let cav = PrefixRef {
                itemstat_id: 2,
                name: "Cavalier's".into(),
            };
            for slot in [
                GearSlot::Helm,
                GearSlot::Shoulders,
                GearSlot::Coat,
                GearSlot::Gloves,
                GearSlot::Leggings,
                GearSlot::Boots,
            ] {
                let coat = slot == GearSlot::Coat && coat_cavaliers;
                validated
                    .gear_slots
                    .set(slot, if coat { cav.clone() } else { bers.clone() });
            }
            validated
        };

        let all_bers = build(false);
        let mixed = build(true);

        let mut stats_all = stats::StatBlock::default();
        apply_validated_gear_stats(&mut stats_all, &db, &all_bers, "Guardian", &ctx);
        let mut stats_mixed = stats::StatBlock::default();
        apply_validated_gear_stats(&mut stats_mixed, &db, &mixed, "Guardian", &ctx);

        // Sanity: the mix must actually differ from the uniform build.
        assert!((stats_all.toughness - stats_mixed.toughness).abs() > 1.0);

        // Delta oracle: mixed == allBerserker + (Cavalier's coat − Berserker's
        // coat), because per-slot contributions are independent.
        let budgets = data::slot_budgets::slot_budgets();
        let coat_budget = budgets
            .get(data::SlotType::Coat, data::stat_shape_from_attr_count(2))
            .expect("coat budget for 2-attr shape");
        let bers = db.itemstats.get(&1).unwrap();
        let cav = db.itemstats.get(&2).unwrap();
        let mut expected = stats_all.clone();
        let mut coat_bers = stats::StatBlock::default();
        add_budget_stats_for_itemstat(&mut coat_bers, bers, coat_budget);
        let mut coat_cav = stats::StatBlock::default();
        add_budget_stats_for_itemstat(&mut coat_cav, cav, coat_budget);
        expected.power += coat_cav.power - coat_bers.power;
        expected.precision += coat_cav.precision - coat_bers.precision;
        expected.toughness += coat_cav.toughness - coat_bers.toughness;
        expected.vitality += coat_cav.vitality - coat_bers.vitality;
        expected.condition_damage += coat_cav.condition_damage - coat_bers.condition_damage;
        expected.expertise += coat_cav.expertise - coat_bers.expertise;
        expected.concentration += coat_cav.concentration - coat_bers.concentration;
        expected.ferocity += coat_cav.ferocity - coat_bers.ferocity;
        expected.healing_power += coat_cav.healing_power - coat_bers.healing_power;

        for (name, got, want) in [
            ("power", stats_mixed.power, expected.power),
            ("precision", stats_mixed.precision, expected.precision),
            ("toughness", stats_mixed.toughness, expected.toughness),
            ("vitality", stats_mixed.vitality, expected.vitality),
            (
                "condition_damage",
                stats_mixed.condition_damage,
                expected.condition_damage,
            ),
            ("expertise", stats_mixed.expertise, expected.expertise),
            (
                "concentration",
                stats_mixed.concentration,
                expected.concentration,
            ),
            ("ferocity", stats_mixed.ferocity, expected.ferocity),
            (
                "healing_power",
                stats_mixed.healing_power,
                expected.healing_power,
            ),
        ] {
            assert!(
                (got - want).abs() < 1e-9,
                "{name}: mixed={got} expected={want}"
            );
        }
    }

    #[test]
    fn spec_precompute_passes_game_mode_to_trait_stats() {
        // A8 leftover wrapper: optimize + optimize_pvp spec precompute had a
        // game_mode (ctx) and still called the PvE wrapper. Competitive leftover
        // must not get Lingering Magic 240.
        let src = include_str!("engine.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");
        assert!(
            !production.contains("stats::calculate_trait_stats(&"),
            "legacy PvE wrapper still used in engine spec precompute"
        );
        let for_mode_with_ctx = production
            .matches("calculate_trait_stats_for_mode(&trait_ids, traits_cache, &ctx.game_mode)")
            .count();
        assert_eq!(
            for_mode_with_ctx, 2,
            "PvE leftover and PvP leftover must both pass ctx.game_mode, got {for_mode_with_ctx}"
        );
    }

    /// CONN-01-01: flattened conditional strike is divided out of `strike_mult`
    /// when a matching resolved OnHealthThreshold effect is active (Scholar 24836).
    /// WvW Scholar-vs-bare integration is skipped: this module has no WvW fixtures.
    #[test]
    fn wvw_params_divides_out_resolved_conditional() {
        let mut params = rotation::simulator::SimParams::basic(1111.0, 222.0, 333.0);
        params.strike_mult = 1.045;
        params.condition_mult = 1.2;
        let clause = combat::ConditionalClause {
            source_id: 24836,
            value: 0.045,
            above: true,
            percent: 90.0,
        };
        let mut scholar = rotation::reaper_fixture::record(
            crate::data::normalized_effects::SourceType::Rune,
            24836,
            "Superior Rune of the Scholar",
            crate::data::normalized_effects::EffectCategory::TriggeredEffect,
            0.05,
            crate::data::normalized_effects::TriggerRule::OnHealthThreshold,
        );
        scholar.health_threshold = Some(crate::data::normalized_effects::HealthThreshold {
            above: true,
            percent: crate::data::quality::FactualValue::Resolved(90.0),
        });
        let effects = [&scholar];
        let out = wvw_params_without_executed_conditionals(&params, &[clause], &[], &effects);
        assert!(
            (out.strike_mult - 1.0).abs() < 1e-6,
            "strike_mult after divide-out: {}",
            out.strike_mult
        );
        assert_eq!(out.power, params.power);
        assert_eq!(out.condition_mult, params.condition_mult);
        assert_eq!(out.weapon_strength, params.weapon_strength);
    }

    /// E1: Symbolic Exposure's "Damage Increase 5%" fact is folded always-on
    /// by the parser, and trait:646:1 (+5% vs Vulnerability, Conditional) is
    /// run by the timeline. The timeline's params must carry it once.
    #[test]
    fn wvw_params_divides_out_trait_percent_with_executed_conditional() {
        use crate::data::normalized_effects::{EffectCategory, Prerequisite, SourceType};
        let symbolic_exposure = gw2_api::models::Trait {
            id: 646,
            name: "Symbolic Exposure".into(),
            icon: None,
            description: None,
            specialization: 0,
            tier: 0,
            order: 0,
            slot: "Minor".into(),
            facts: vec![gw2_api::models::Fact::Percent {
                text: Some("Damage Increase".into()),
                icon: None,
                percent: Some(5.0),
            }],
            traited_facts: vec![],
            fact_parse_drops: 0,
            skills: vec![],
        };
        let traits = HashMap::from([(646, symbolic_exposure)]);
        let mods = combat::extract_damage_modifiers(
            &[646],
            None,
            &[],
            None,
            &traits,
            &HashMap::new(),
            &BalanceContext::new(GameMode::WvW),
        );
        let mut params = rotation::simulator::SimParams::basic(1111.0, 222.0, 333.0);
        params.strike_mult = mods.total_strike_mult();
        assert!(
            (params.strike_mult - 1.05).abs() < 1e-9,
            "parser folds the fact"
        );

        let mut record = rotation::reaper_fixture::record(
            SourceType::Trait,
            646,
            "Symbolic Exposure",
            EffectCategory::TriggeredEffect,
            5.0,
            crate::data::normalized_effects::TriggerRule::Conditional,
        );
        record.inner_category = Some(EffectCategory::StrikeDamagePct);
        record.prerequisite = Some(Prerequisite {
            foe_condition: Some("Vulnerability".into()),
            ..Default::default()
        });
        let effects = [&record];
        let out = wvw_params_without_executed_conditionals(
            &params,
            &mods.conditional_strike,
            &mods.trait_standing,
            &effects,
        );
        assert!(
            (out.strike_mult - 1.0).abs() < 1e-9,
            "the timeline applies +5% vs Vulnerability; standing params must not: {}",
            out.strike_mult
        );
    }

    #[test]
    fn wvw_params_unchanged_without_conditional_clauses() {
        let mut params = rotation::simulator::SimParams::basic(1111.0, 222.0, 333.0);
        params.strike_mult = 1.045;
        params.condition_mult = 1.2;
        let out = wvw_params_without_executed_conditionals(&params, &[], &[], &[]);
        assert!((out.strike_mult - 1.045).abs() < 1e-6);
        assert_eq!(out.power, params.power);
        assert_eq!(out.condition_damage, params.condition_damage);
        assert_eq!(out.weapon_strength, params.weapon_strength);
        assert_eq!(out.condition_mult, params.condition_mult);
        assert_eq!(out.condition_duration_mult, params.condition_duration_mult);
        assert_eq!(out.boon_duration_mult, params.boon_duration_mult);
        assert_eq!(out.healing_mult, params.healing_mult);
        assert_eq!(out.max_health, params.max_health);
        assert_eq!(out.armor, params.armor);
        assert_eq!(out.mode, params.mode);
        assert!(out.intent.is_none());
        assert!(out.deferred_target.is_empty());
    }

    #[test]
    fn dropped_fact_is_provisional_on_the_scored_path() {
        let skill: gw2_api::models::Skill = serde_json::from_str(
            r#"{
                "id": 880014,
                "name": "Parse Drop Probe",
                "slot": "Heal",
                "facts": [
                    {"text": "Recharge", "type": "Recharge", "value": 8},
                    {"text": "Some effect", "icon": "i.png", "value": 5}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(skill.facts.len(), 1);
        assert!(matches!(skill.facts[0], Fact::Recharge { .. }));
        assert_eq!(skill.fact_parse_drops, 1);

        let tr: GW2Trait = serde_json::from_str(
            r#"{
                "id": 880015,
                "name": "Probe Trait",
                "specialization": 1,
                "tier": 1,
                "order": 0,
                "slot": "Major",
                "facts": [
                    {"type": "Number", "value": 1},
                    {"text": "no type"}
                ],
                "skills": [{
                    "id": 880016,
                    "facts": [
                        {"type": "Range", "value": 100},
                        {"text": "no type"}
                    ]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(tr.facts.len(), 1);
        assert_eq!(tr.fact_parse_drops, 1);
        assert_eq!(tr.skills[0].fact_parse_drops, 1);

        let mut db = GameDb::empty_for_tests();
        db.skills.insert(skill.id, skill);
        db.traits.insert(tr.id, tr);
        let mut validated = ValidatedBuild::default();
        validated.skills.heal = Some((880014, "Parse Drop Probe".into()));
        validated.specializations.push(validation::ValidatedSpec {
            spec_id: 1,
            name: "Probe".into(),
            elite: false,
            trait_ids: vec![880015],
            trait_names: vec!["Probe Trait".into()],
            all_trait_ids: vec![880015],
        });
        let ctx = BalanceContext::new(GameMode::PvE);
        let result = synergy_result_from_validated(validated, &db, "Guardian", &ctx, None);
        assert_eq!(result.data_quality, data::DataQuality::Provisional);
        let fields: Vec<&str> = result
            .quality_reasons
            .iter()
            .map(|r| r.field.as_str())
            .collect();
        assert!(
            fields.contains(&data::quality::FACT_DROP_FIELD),
            "scored path must name the parse drop, not look complete: {fields:?} {:?}",
            result.quality_reasons
        );
        let skill_reason = result
            .quality_reasons
            .iter()
            .find(|r| r.entity == "skill 880014")
            .expect("drop is tied to the skill id");
        assert!(skill_reason.explanation.contains('1'));
        assert!(result
            .quality_reasons
            .iter()
            .any(|r| r.entity == "trait 880015"));
        assert!(result
            .quality_reasons
            .iter()
            .any(|r| r.entity == "skill 880016"));
    }
}
