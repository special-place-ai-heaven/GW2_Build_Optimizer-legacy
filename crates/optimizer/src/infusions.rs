//! Phase 2 PR-I: per worn-slot infusion seats (inner argmax).
//!
//! Ownership is seat-structured on [`ValidatedBuild::infusion_seats`] - not a
//! bag of infusion ids. Each seat records the gear slot, the index inside that
//! piece's `infusion_slots`, the slot flags (`Infusion` / `Enrichment`), and an
//! optional filled item. Seat layout comes from Ascended item `infusion_slots`
//! (canonical defaults when the DB has no sample item).
//!
//! Standing infix attributes fold into [`crate::engine::calculate_validated_stats`]
//! via [`crate::stats::calculate_infusion_stats`] (equipment-shaped projection of
//! the seats). Agony infusions are excluded from the free argmax; a locked seat
//! may keep a fixed agony fill.
//!
//! After a kit is complete, [`assign_best_infusions`] is the cheap inner argmax
//! on the evaluate path (same hook as PR-C consumables). Locked seats are never
//! overwritten. There is no `swap_infusion` neighbor and no coupled beam dim.

use std::collections::HashMap;

use gw2_api::models::{EquipmentPiece, Item};
use gw2_core::types::{BuildLocks, GameMode, GearSlot};

use crate::balance::BalanceContext;
use crate::combat::{self};
use crate::engine;
use crate::gamedb::GameDb;
use crate::scenario::ScenarioSpec;
use crate::scoring::{self, OptimizationWeights};
use crate::stats::{self, StatBlock};
use crate::validation::{ValidatedBuild, ValidatedItem};

/// One infusion/enrichment socket on a worn gear piece.
#[derive(Debug, Clone, PartialEq)]
pub struct InfusionSeat {
    pub slot: GearSlot,
    /// Index inside that piece's `details.infusion_slots` array.
    pub index: u8,
    /// Flags copied from the gear item's infusion slot (e.g. `Infusion`).
    pub flags: Vec<String>,
    /// Filled upgrade id, if any.
    pub item: Option<ValidatedItem>,
}

/// Canonical Ascended land-worn slot flags when no sample item is in the DB.
///
/// Matches common Ascended API rows: armour/weapons/accessories/amulet get one
/// `Infusion` seat; back and rings also get an `Enrichment` seat. Weapon set 2
/// is not worn on land and never appears here.
fn canonical_slot_flags(slot: GearSlot) -> &'static [&'static [&'static str]] {
    match slot {
        GearSlot::Helm
        | GearSlot::Shoulders
        | GearSlot::Coat
        | GearSlot::Gloves
        | GearSlot::Leggings
        | GearSlot::Boots
        | GearSlot::Accessory1
        | GearSlot::Accessory2
        | GearSlot::Amulet
        | GearSlot::WeaponSet1Main
        | GearSlot::WeaponSet1Off => &[&["Infusion"]],
        GearSlot::Back | GearSlot::Ring1 | GearSlot::Ring2 => &[&["Infusion"], &["Enrichment"]],
        GearSlot::WeaponSet2Main | GearSlot::WeaponSet2Off => &[],
    }
}

/// API equipment-table name used by [`stats::calculate_infusion_stats`].
fn api_slot_name(slot: GearSlot) -> &'static str {
    match slot {
        GearSlot::Helm => "Helm",
        GearSlot::Shoulders => "Shoulders",
        GearSlot::Coat => "Coat",
        GearSlot::Gloves => "Gloves",
        GearSlot::Leggings => "Leggings",
        GearSlot::Boots => "Boots",
        GearSlot::Back => "Backpack",
        GearSlot::Accessory1 => "Accessory1",
        GearSlot::Accessory2 => "Accessory2",
        GearSlot::Amulet => "Amulet",
        GearSlot::Ring1 => "Ring1",
        GearSlot::Ring2 => "Ring2",
        GearSlot::WeaponSet1Main => "WeaponA1",
        GearSlot::WeaponSet1Off => "WeaponA2",
        GearSlot::WeaponSet2Main => "WeaponB1",
        GearSlot::WeaponSet2Off => "WeaponB2",
    }
}

/// Land-worn slots that may hold infusion seats for the validated sheet.
pub fn land_worn_slots(validated: &ValidatedBuild) -> Vec<GearSlot> {
    GearSlot::ALL
        .into_iter()
        .filter(|slot| {
            !matches!(slot, GearSlot::WeaponSet2Main | GearSlot::WeaponSet2Off)
                && validated.wears(*slot)
        })
        .collect()
}

/// Prefer a sample Ascended item's `infusion_slots`; else canonical defaults.
pub fn slot_flag_layout(slot: GearSlot, db: &GameDb) -> Vec<Vec<String>> {
    if let Some(flags) = db.sample_infusion_slot_flags(slot) {
        return flags;
    }
    canonical_slot_flags(slot)
        .iter()
        .map(|row| row.iter().map(|s| (*s).to_string()).collect())
        .collect()
}

/// Ensure `validated.infusion_seats` matches worn-slot layout for `mode`.
///
/// PvP clears seats (no infusion sockets on the PvP amulet path). Existing
/// fills on seats that remain are preserved by (slot, index).
pub fn ensure_seats(validated: &mut ValidatedBuild, db: &GameDb, mode: &GameMode) {
    if matches!(mode, GameMode::PvP) {
        validated.infusion_seats.clear();
        return;
    }
    let previous: HashMap<(GearSlot, u8), Option<ValidatedItem>> = validated
        .infusion_seats
        .iter()
        .map(|s| ((s.slot, s.index), s.item.clone()))
        .collect();
    let mut next = Vec::new();
    for slot in land_worn_slots(validated) {
        let layout = slot_flag_layout(slot, db);
        for (index, flags) in layout.into_iter().enumerate() {
            let index = index as u8;
            next.push(InfusionSeat {
                slot,
                index,
                flags,
                item: previous.get(&(slot, index)).cloned().flatten(),
            });
        }
    }
    validated.infusion_seats = next;
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

/// True when this upgrade is an agony infusion (AgonyResistance primary).
pub fn is_agony_infusion(item: &Item) -> bool {
    let Some(details) = item.details.as_ref() else {
        return item.name.to_ascii_lowercase().contains("agony");
    };
    let has_agony = details
        .infix_upgrade
        .as_ref()
        .map(|infix| {
            infix
                .attributes
                .iter()
                .any(|a| a.attribute.eq_ignore_ascii_case("AgonyResistance"))
        })
        .unwrap_or(false);
    has_agony || item.name.to_ascii_lowercase().contains("agony")
}

/// Standing combat-stat infusion/enrichment (not agony-only).
pub fn is_stat_infusion(item: &Item) -> bool {
    if item.item_type != "UpgradeComponent" {
        return false;
    }
    let Some(details) = item.details.as_ref() else {
        return false;
    };
    if details.infusion_upgrade_flags.is_empty() {
        return false;
    }
    if is_agony_infusion(item) {
        return false;
    }
    let Some(infix) = details.infix_upgrade.as_ref() else {
        return false;
    };
    infix.attributes.iter().any(|a| {
        matches!(
            a.attribute.as_str(),
            "Power"
                | "Precision"
                | "Toughness"
                | "Vitality"
                | "ConditionDamage"
                | "Expertise"
                | "ConditionDuration"
                | "Concentration"
                | "BoonDuration"
                | "Ferocity"
                | "CritDamage"
                | "Healing"
                | "HealingPower"
        )
    })
}

/// Does this upgrade's `infusion_upgrade_flags` match the seat flags?
pub fn flags_compatible(item: &Item, seat_flags: &[String]) -> bool {
    let Some(details) = item.details.as_ref() else {
        return false;
    };
    if seat_flags.is_empty() {
        return true;
    }
    // A seat accepts an upgrade when every seat flag appears on the upgrade
    // (API: Infusion seats take Infusion-flagged upgrades; Enrichment likewise).
    seat_flags.iter().all(|need| {
        details
            .infusion_upgrade_flags
            .iter()
            .any(|have| have.eq_ignore_ascii_case(need))
    })
}

/// Mode legality for infusion upgrades. Empty `game_types` (fixtures) is legal
/// everywhere; live rows always carry the list. PvP has no seats, so callers
/// short-circuit before consulting this.
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

fn seat_locked(locks: &BuildLocks, slot: GearSlot, index: u8) -> Option<u32> {
    locks
        .infusion_locks
        .get(&slot)
        .and_then(|seats| seats.get(index as usize).copied().flatten())
}

/// Project seats into equipment pieces and reuse [`stats::calculate_infusion_stats`].
pub fn infusion_stats_from_seats(
    seats: &[InfusionSeat],
    items_cache: &HashMap<u32, Item>,
) -> StatBlock {
    let mut by_slot: HashMap<GearSlot, Vec<u32>> = HashMap::new();
    for seat in seats {
        if let Some(item) = &seat.item {
            by_slot.entry(seat.slot).or_default().push(item.id);
        }
    }
    let equipment: Vec<EquipmentPiece> = by_slot
        .into_iter()
        .map(|(slot, infusions)| EquipmentPiece {
            id: 0,
            slot: api_slot_name(slot).to_string(),
            location: None,
            skin: None,
            upgrades: Vec::new(),
            infusions,
            binding: None,
            bound_to: None,
            dyes: Vec::new(),
            stats: None,
        })
        .collect();
    stats::calculate_infusion_stats(&equipment, items_cache)
}

/// Apply seat infusions onto the validated sheet (calculate_validated_stats only).
pub fn fold_into_validated_stats(stats: &mut StatBlock, validated: &ValidatedBuild, db: &GameDb) {
    *stats += &infusion_stats_from_seats(&validated.infusion_seats, &db.items);
}

/// Cheap inner argmax: fill free seats with flag-legal stat infusions after the
/// kit is complete. Uniform fill per flag family (all free Infusion seats share
/// one pick; Enrichment seats share another). Locked seats are preserved,
/// including fixed agony fills.
pub fn assign_best_infusions(
    validated: &mut ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
    locks: &BuildLocks,
) {
    ensure_seats(validated, db, &ctx.game_mode);
    if matches!(ctx.game_mode, GameMode::PvP) {
        return;
    }

    // Apply locks first (may pin agony or any id).
    for seat in &mut validated.infusion_seats {
        if let Some(id) = seat_locked(locks, seat.slot, seat.index) {
            seat.item = locked_item(db, id);
        }
    }

    let free_indices: Vec<usize> = validated
        .infusion_seats
        .iter()
        .enumerate()
        .filter(|(_, seat)| seat_locked(locks, seat.slot, seat.index).is_none())
        .map(|(i, _)| i)
        .collect();
    if free_indices.is_empty() {
        return;
    }

    // Partition free seats by primary flag family.
    let mut infusion_free = Vec::new();
    let mut enrichment_free = Vec::new();
    for &i in &free_indices {
        let flags = &validated.infusion_seats[i].flags;
        if flags.iter().any(|f| f.eq_ignore_ascii_case("Enrichment")) {
            enrichment_free.push(i);
        } else {
            infusion_free.push(i);
        }
    }

    let catalog = db.stat_infusions_for(&ctx.game_mode);
    let infusion_choices: Vec<Option<&Item>> = {
        let mut v: Vec<Option<&Item>> = vec![None];
        v.extend(
            catalog
                .iter()
                .copied()
                .filter(|item| {
                    item.details
                        .as_ref()
                        .map(|d| {
                            d.infusion_upgrade_flags
                                .iter()
                                .any(|f| f.eq_ignore_ascii_case("Infusion"))
                        })
                        .unwrap_or(false)
                })
                .map(Some),
        );
        v
    };
    let enrichment_choices: Vec<Option<&Item>> = {
        let mut v: Vec<Option<&Item>> = vec![None];
        v.extend(
            catalog
                .iter()
                .copied()
                .filter(|item| {
                    item.details
                        .as_ref()
                        .map(|d| {
                            d.infusion_upgrade_flags
                                .iter()
                                .any(|f| f.eq_ignore_ascii_case("Enrichment"))
                        })
                        .unwrap_or(false)
                })
                .map(Some),
        );
        v
    };

    // Uniform fill: one pick per flag family applied to every free seat in
    // that family. Cross-family product stays tiny (types x types).
    //
    // Seed best_key at MIN (not the current fill). Seeding from the current
    // fill while best_* start as None meant a re-solve on an already-optimal
    // fill never claimed a winner (ties are not strict >) and writeback then
    // cleared free seats — breaking search_v2 neighbors that inherit parent fills.
    let mut best_key = (i64::MIN, i64::MIN, i64::MIN);
    let mut best_inf: Option<&Item> = None;
    let mut best_enr: Option<&Item> = None;

    let inf_iter: Vec<Option<&Item>> = if infusion_free.is_empty() {
        vec![None]
    } else {
        infusion_choices.clone()
    };
    let enr_iter: Vec<Option<&Item>> = if enrichment_free.is_empty() {
        vec![None]
    } else {
        enrichment_choices.clone()
    };

    let mut scratch = validated.clone();
    for inf in &inf_iter {
        for enr in &enr_iter {
            for &i in &infusion_free {
                scratch.infusion_seats[i].item = inf.map(item_to_validated);
            }
            for &i in &enrichment_free {
                scratch.infusion_seats[i].item = enr.map(item_to_validated);
            }
            let key = cheap_key(&scratch, db, profession_name, weights, ctx, scenario);
            if key > best_key {
                best_key = key;
                best_inf = *inf;
                best_enr = *enr;
            }
        }
    }

    for &i in &infusion_free {
        validated.infusion_seats[i].item = best_inf.map(item_to_validated);
    }
    for &i in &enrichment_free {
        validated.infusion_seats[i].item = best_enr.map(item_to_validated);
    }
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

/// Test helper: UpgradeComponent infusion/enrichment with infix attributes.
#[cfg(test)]
pub fn test_infusion(
    id: u32,
    name: &str,
    flags: &[&str],
    attrs: &[(&str, i32)],
    game_types: &[&str],
) -> Item {
    use gw2_api::models::{InfixAttribute, InfixUpgrade, ItemDetails};
    Item {
        id,
        name: name.to_string(),
        description: None,
        icon: None,
        item_type: "UpgradeComponent".into(),
        rarity: "Exotic".into(),
        level: 80,
        vendor_value: None,
        chat_link: None,
        default_skin: None,
        flags: Vec::new(),
        game_types: game_types.iter().map(|s| s.to_string()).collect(),
        restrictions: Vec::new(),
        details: Some(ItemDetails {
            description: None,
            detail_type: Some("Default".into()),
            weight_class: None,
            defense: None,
            damage_type: None,
            min_power: None,
            max_power: None,
            suffix: None,
            bonuses: Vec::new(),
            infusion_upgrade_flags: flags.iter().map(|s| (*s).to_string()).collect(),
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
    use crate::scoring::OptimizationWeights;
    use crate::validation::ValidatedBuild;

    fn pve_ctx() -> BalanceContext {
        BalanceContext::pve()
    }

    fn pvp_ctx() -> BalanceContext {
        BalanceContext::pvp()
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
            db.items.insert(item.id, item.clone());
            db.items_by_type
                .entry(item.item_type.clone())
                .or_default()
                .push(item.id);
        }
        for ids in db.items_by_type.values_mut() {
            ids.sort_unstable();
        }
        db
    }

    fn armored_build() -> ValidatedBuild {
        let mut build = ValidatedBuild::default();
        // wears() is true for armour/trinkets; give set-1 a weapon so weapon
        // seats appear when layout includes them.
        build.weapons.set1.main_hand = Some("Greatsword".into());
        build
    }

    #[test]
    fn pvp_has_no_seats() {
        let db = db_with(vec![test_infusion(
            1,
            "Mighty Infusion",
            &["Infusion"],
            &[("Power", 5)],
            &["Pve", "Wvw"],
        )]);
        let mut build = armored_build();
        assign_best_infusions(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pvp_ctx(),
            &pve_scenario(),
            &BuildLocks::default(),
        );
        assert!(build.infusion_seats.is_empty());
    }

    #[test]
    fn pve_and_wvw_create_flag_legal_seats() {
        let db = GameDb::empty_for_tests();
        let mut build = armored_build();
        ensure_seats(&mut build, &db, &GameMode::PvE);
        assert!(!build.infusion_seats.is_empty());
        assert!(build
            .infusion_seats
            .iter()
            .any(|s| s.slot == GearSlot::Helm));
        assert!(build
            .infusion_seats
            .iter()
            .all(|s| !matches!(s.slot, GearSlot::WeaponSet2Main | GearSlot::WeaponSet2Off)));
        let mut wvw = armored_build();
        ensure_seats(&mut wvw, &db, &GameMode::WvW);
        assert_eq!(wvw.infusion_seats.len(), build.infusion_seats.len());
    }

    #[test]
    fn mode_flags_filter_catalog() {
        let pve_only = test_infusion(10, "PvE Power", &["Infusion"], &[("Power", 9)], &["Pve"]);
        let wvw_only = test_infusion(11, "WvW Power", &["Infusion"], &[("Power", 9)], &["Wvw"]);
        assert!(item_legal_for_mode(&pve_only, &GameMode::PvE));
        assert!(!item_legal_for_mode(&pve_only, &GameMode::WvW));
        assert!(item_legal_for_mode(&wvw_only, &GameMode::WvW));
        assert!(!item_legal_for_mode(&wvw_only, &GameMode::PvE));
    }

    #[test]
    fn agony_excluded_from_stat_catalog_predicate() {
        let agony = test_infusion(
            20,
            "+9 Agony Infusion",
            &["Infusion"],
            &[("AgonyResistance", 9)],
            &["Pve"],
        );
        let mighty = test_infusion(
            21,
            "Mighty Infusion",
            &["Infusion"],
            &[("Power", 5)],
            &["Pve"],
        );
        assert!(is_agony_infusion(&agony));
        assert!(!is_stat_infusion(&agony));
        assert!(is_stat_infusion(&mighty));
    }

    #[test]
    fn seats_and_locks_round_trip_on_build_and_locks() {
        let mighty = test_infusion(
            30,
            "Mighty Infusion",
            &["Infusion"],
            &[("Power", 5)],
            &["Pve"],
        );
        let db = db_with(vec![mighty]);
        let mut build = armored_build();
        ensure_seats(&mut build, &db, &GameMode::PvE);
        assert!(!build.infusion_seats.is_empty());
        build.infusion_seats[0].item = Some(ValidatedItem {
            id: 30,
            name: "Mighty Infusion".into(),
        });
        let clone = build.clone();
        assert_eq!(clone.infusion_seats, build.infusion_seats);

        let mut locks = BuildLocks::default();
        locks.infusion_locks.insert(GearSlot::Helm, vec![Some(30)]);
        let json = serde_json::to_string(&locks).expect("serialize");
        let back: BuildLocks = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.infusion_locks.get(&GearSlot::Helm),
            Some(&vec![Some(30)])
        );
        let legacy = r#"{"specs":[null,null,null],"trait_locks":{}}"#;
        let old: BuildLocks = serde_json::from_str(legacy).expect("legacy");
        assert!(old.infusion_locks.is_empty());
    }

    #[test]
    fn locked_seats_not_overwritten_including_agony() {
        let better = test_infusion(40, "Huge Power", &["Infusion"], &[("Power", 50)], &["Pve"]);
        let agony = test_infusion(
            41,
            "Agony Infusion",
            &["Infusion"],
            &[("AgonyResistance", 9)],
            &["Pve"],
        );
        let db = db_with(vec![better, agony]);
        let mut locks = BuildLocks::default();
        // Lock every Infusion-flag seat to agony (fixed fill+lock policy).
        let mut probe = armored_build();
        ensure_seats(&mut probe, &db, &GameMode::PvE);
        for seat in &probe.infusion_seats {
            if seat
                .flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case("Infusion"))
            {
                let entry = locks.infusion_locks.entry(seat.slot).or_default();
                while entry.len() <= seat.index as usize {
                    entry.push(None);
                }
                entry[seat.index as usize] = Some(41);
            }
        }
        let mut build = armored_build();
        assign_best_infusions(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pve_ctx(),
            &pve_scenario(),
            &locks,
        );
        for seat in &build.infusion_seats {
            if seat
                .flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case("Infusion"))
            {
                assert_eq!(seat.item.as_ref().map(|i| i.id), Some(41));
            }
        }
    }

    #[test]
    fn argmax_prefers_power_infusion_over_empty_on_power_weights() {
        let mighty = test_infusion(
            50,
            "Mighty Infusion",
            &["Infusion"],
            &[("Power", 9)],
            &["Pve"],
        );
        let precise = test_infusion(
            51,
            "Precise Infusion",
            &["Infusion"],
            &[("Precision", 9)],
            &["Pve"],
        );
        let db = db_with(vec![mighty, precise]);
        let mut build = armored_build();
        assign_best_infusions(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pve_ctx(),
            &pve_scenario(),
            &BuildLocks::default(),
        );
        let infusion_fills: Vec<u32> = build
            .infusion_seats
            .iter()
            .filter(|s| s.flags.iter().any(|f| f.eq_ignore_ascii_case("Infusion")))
            .filter_map(|s| s.item.as_ref().map(|i| i.id))
            .collect();
        assert!(
            !infusion_fills.is_empty(),
            "argmax should fill Infusion seats"
        );
        assert!(
            infusion_fills.iter().all(|&id| id == 50),
            "power weights should pick Mighty (50), got {infusion_fills:?}"
        );
    }

    /// Re-solving an already-optimal Mighty fill must keep free seats filled.
    /// Regression for the best_key-seed / None-writeback clear bug.
    #[test]
    fn resolve_again_keeps_already_optimal_mighty_fill() {
        let mighty = test_infusion(
            50,
            "Mighty Infusion",
            &["Infusion"],
            &[("Power", 9)],
            &["Pve"],
        );
        let precise = test_infusion(
            51,
            "Precise Infusion",
            &["Infusion"],
            &[("Precision", 9)],
            &["Pve"],
        );
        let db = db_with(vec![mighty, precise]);
        let mut build = armored_build();
        assign_best_infusions(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pve_ctx(),
            &pve_scenario(),
            &BuildLocks::default(),
        );
        let first: Vec<Option<u32>> = build
            .infusion_seats
            .iter()
            .map(|s| s.item.as_ref().map(|i| i.id))
            .collect();
        assert!(
            first.contains(&Some(50)),
            "first solve should fill Mighty, got {first:?}"
        );

        assign_best_infusions(
            &mut build,
            &db,
            "Warrior",
            &power_weights(),
            &pve_ctx(),
            &pve_scenario(),
            &BuildLocks::default(),
        );
        let second: Vec<Option<u32>> = build
            .infusion_seats
            .iter()
            .map(|s| s.item.as_ref().map(|i| i.id))
            .collect();
        assert_eq!(
            first, second,
            "re-solve must not clear already-optimal free seats"
        );
        let infusion_fills: Vec<u32> = build
            .infusion_seats
            .iter()
            .filter(|s| s.flags.iter().any(|f| f.eq_ignore_ascii_case("Infusion")))
            .filter_map(|s| s.item.as_ref().map(|i| i.id))
            .collect();
        assert!(
            !infusion_fills.is_empty() && infusion_fills.iter().all(|&id| id == 50),
            "free Infusion seats must still be Mighty after re-solve, got {infusion_fills:?}"
        );
    }

    #[test]
    fn fold_uses_calculate_infusion_stats_path() {
        let mighty = test_infusion(
            60,
            "Mighty Infusion",
            &["Infusion"],
            &[("Power", 5)],
            &["Pve"],
        );
        let db = db_with(vec![mighty]);
        let mut build = armored_build();
        ensure_seats(&mut build, &db, &GameMode::PvE);
        for seat in &mut build.infusion_seats {
            if seat
                .flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case("Infusion"))
            {
                seat.item = Some(ValidatedItem {
                    id: 60,
                    name: "Mighty Infusion".into(),
                });
            }
        }
        let via_seats = infusion_stats_from_seats(&build.infusion_seats, &db.items);
        let n = build
            .infusion_seats
            .iter()
            .filter(|s| s.item.as_ref().map(|i| i.id) == Some(60))
            .count();
        assert_eq!(via_seats.power, 5.0 * n as f64);
        let mut folded = StatBlock::default();
        fold_into_validated_stats(&mut folded, &build, &db);
        assert_eq!(folded.power, via_seats.power);
    }
}
