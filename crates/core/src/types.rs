//! Core domain types for resolved builds.
//! A "resolved" build has all IDs expanded to full data from the cache.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A fully resolved character build with all data expanded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedBuild {
    pub character_name: String,
    pub profession: String,
    pub game_mode: GameMode,
    pub specializations: Vec<ResolvedSpec>,
    pub skills: ResolvedSkills,
    /// Revenant stance labels (compact), active first.
    #[serde(default)]
    pub legends: Vec<String>,
    /// Ranger pet labels (terrestrial), when known.
    #[serde(default)]
    pub pets: Vec<String>,
    pub weapons: Vec<ResolvedWeaponSet>,
    pub armor: Vec<ResolvedGearPiece>,
    pub trinkets: Vec<ResolvedGearPiece>,
    pub relic: Option<ResolvedRelic>,
    pub rune: Option<ResolvedUpgrade>,
    pub pvp_amulet: Option<ResolvedPvpAmulet>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameMode {
    #[default]
    PvE,
    PvP,
    WvW,
}

impl GameMode {
    /// Canonical iteration order for `GameMode`. The slice `[PvE, PvP, WvW]`
    /// is part of the contract — downstream consumers iterate this constant
    /// to build tab order and default-lookup sequences. Reordering silently
    /// shifts UI/lookup behavior across the workspace. The
    /// `game_mode_all_order_is_pinned` test below guards the order.
    pub const ALL: [GameMode; 3] = [GameMode::PvE, GameMode::PvP, GameMode::WvW];

    pub fn label(&self) -> &str {
        match self {
            GameMode::PvE => "PvE",
            GameMode::PvP => "PvP",
            GameMode::WvW => "WvW",
        }
    }
}

/// Granular lock constraints for the optimizer.
/// Controls which specializations and trait choices are preserved vs. free to change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildLocks {
    /// Spec locks by slot (0, 1, 2). None = optimizer decides, Some(id) = must use this spec.
    ///
    /// Slot 2 is semantically the elite-eligible slot: `locked_elite_id()` and the
    /// "Locked to: <elite>" badge in `render_improve_tab` both read `specs[2]` directly.
    /// Reordering slots is therefore not supported by the UI — it would silently break
    /// that invariant for every consumer that assumes `specs[2]` is the elite slot.
    pub specs: [Option<u32>; 3],
    /// Trait locks: spec_id → [Adept, Master, Grandmaster]. None = free, Some(id) = locked trait.
    #[serde(default)]
    pub trait_locks: HashMap<u32, [Option<u32>; 3]>,
    /// Gear locks: slot → required itemstat id. A locked slot's prefix is
    /// never mutated by any search operator or advisor pass. Serde default so
    /// older lock JSON without this field still loads.
    #[serde(default)]
    pub gear_locks: HashMap<GearSlot, u32>,
    /// Locked nourishment (food) item id. Inner argmax never overwrites this.
    #[serde(default)]
    pub food: Option<u32>,
    /// Locked enhancement (utility consumable) item id. Inner argmax never overwrites this.
    #[serde(default)]
    pub utility: Option<u32>,
    /// Per gear-slot locked infusion fills. Vec index matches that piece's
    /// infusion_slots order; None = seat unlocked. Serde default for legacy JSON.
    #[serde(default)]
    pub infusion_locks: HashMap<GearSlot, Vec<Option<u32>>>,
}

impl BuildLocks {
    /// Get the locked elite spec ID (slot 2), for backward-compatible optimizer paths.
    pub fn locked_elite_id(&self) -> Option<u32> {
        self.specs[2]
    }

    /// Get locked trait for a specific spec and column (0=Adept, 1=Master, 2=Grandmaster).
    pub fn locked_trait(&self, spec_id: u32, column: usize) -> Option<u32> {
        self.trait_locks
            .get(&spec_id)
            .and_then(|cols| cols.get(column).copied().flatten())
    }

    /// Build a text description of all active locks for prompt generation.
    pub fn describe_constraints(&self) -> String {
        let mut parts = Vec::new();
        for (slot, spec) in self.specs.iter().enumerate() {
            if let Some(id) = spec {
                parts.push(format!("Slot {} spec locked to ID {}", slot + 1, id));
            }
        }
        // Sort by spec_id so the output is deterministic regardless of
        // HashMap iteration order. The string is embedded verbatim in LLM
        // prompts; nondeterministic ordering breaks prompt cache hits and
        // makes responses drift between identical user requests.
        let mut trait_entries: Vec<(&u32, &[Option<u32>; 3])> = self.trait_locks.iter().collect();
        trait_entries.sort_by_key(|(spec_id, _)| **spec_id);
        for (spec_id, cols) in trait_entries {
            for (col, trait_id) in cols.iter().enumerate() {
                if let Some(tid) = trait_id {
                    let tier = match col {
                        0 => "Adept",
                        1 => "Master",
                        2 => "Grandmaster",
                        _ => "Unknown",
                    };
                    parts.push(format!(
                        "Spec {} {} trait locked to ID {}",
                        spec_id, tier, tid
                    ));
                }
            }
        }
        // Gear locks: emit in canonical slot order (not HashMap order) for the
        // same determinism reasons as the trait locks above. Only ids are
        // available here; prompt-facing callers with GameDb access resolve
        // names themselves (`describe_lock_constraints`).
        let mut gear_entries: Vec<(&GearSlot, &u32)> = self.gear_locks.iter().collect();
        gear_entries.sort_by_key(|(slot, _)| {
            GearSlot::ALL
                .iter()
                .position(|canonical| canonical == *slot)
                .unwrap_or(usize::MAX)
        });
        for (slot, id) in gear_entries {
            parts.push(format!("Gear {} locked to ID {}", slot_name(slot), id));
        }
        if let Some(id) = self.food {
            parts.push(format!("Food locked to ID {id}"));
        }
        if let Some(id) = self.utility {
            parts.push(format!("Utility locked to ID {id}"));
        }
        let mut infusion_entries: Vec<(&GearSlot, &Vec<Option<u32>>)> =
            self.infusion_locks.iter().collect();
        infusion_entries.sort_by_key(|(slot, _)| {
            GearSlot::ALL
                .iter()
                .position(|canonical| canonical == *slot)
                .unwrap_or(usize::MAX)
        });
        for (slot, seats) in infusion_entries {
            for (idx, id) in seats.iter().enumerate() {
                if let Some(id) = id {
                    parts.push(format!(
                        "Infusion {} seat {} locked to ID {}",
                        slot_name(slot),
                        idx,
                        id
                    ));
                }
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedSpec {
    pub id: u32,
    pub name: String,
    pub elite: bool,
    pub traits_selected: Vec<ResolvedTrait>,
    pub traits_available: Vec<Vec<TraitOption>>, // 3 columns, 3 options each
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedTrait {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub column: usize, // 0, 1, 2
    pub selected: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraitOption {
    pub id: u32,
    pub name: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedSkills {
    pub heal: Option<SkillInfo>,
    pub utilities: Vec<Option<SkillInfo>>,
    pub elite: Option<SkillInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillInfo {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedWeaponSet {
    pub label: String, // "Set 1", "Set 2"
    #[serde(default)]
    pub stat_prefix: String,
    pub main_hand: Option<WeaponInfo>,
    pub off_hand: Option<WeaponInfo>,
    pub sigils: Vec<UpgradeInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeaponInfo {
    pub name: String,
    pub weapon_type: String,
    #[serde(default)]
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpgradeInfo {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedGearPiece {
    pub slot: String,
    pub name: String,
    pub stat_prefix: String,
    pub infusions: Vec<String>,
    #[serde(default)]
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedUpgrade {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedRelic {
    pub id: u32,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedPvpAmulet {
    pub id: u32,
    pub name: String,
    pub stats: std::collections::HashMap<String, i32>,
}

/// Calculated stat block for a build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatBlock {
    pub power: i32,
    pub precision: i32,
    pub toughness: i32,
    pub vitality: i32,
    pub condition_damage: i32,
    pub expertise: i32,
    pub concentration: i32,
    pub ferocity: i32,
    pub healing_power: i32,
    pub crit_chance: f64,
    pub crit_damage: f64,
    pub health: i32,
    pub armor: i32,
}

impl StatBlock {
    /// Compute derived stats from base stats.
    /// `base_health` and `base_defense` are profession-specific values from loaded
    /// profession profiles (data/profession_profiles.json). Callers get these values
    /// from the optimizer crate's stats::base_health() / stats::base_defense().
    ///
    /// NOTE: GW2 HP classes and armor classes do NOT align!
    /// HP: High (Warrior, Necromancer), Medium (Rev, Engi, Ranger, Mesmer), Low (Guardian, Thief, Ele)
    /// Armor: Heavy (Warrior, Guardian, Revenant), Medium (Ranger, Engi, Thief), Light (Ele, Mes, Necro)
    ///
    /// NOTE: The canonical source for formula constants (895, 21, 150, 15, 10) is
    /// `data/formulas/universal.json`. The active runtime paths in
    /// `crates/optimizer/src/{stats,combat}.rs` use loaded values from that file.
    /// This method retains hardcoded values because `core` cannot depend on `optimizer`.
    /// Existing optimizer callers accept this divergence; the pinning test
    /// `statblock_compute_derived_pins_formula` locks the math in so drift from
    /// `universal.json` fails loudly rather than producing quietly-wrong stats.
    pub fn compute_derived(&mut self, base_health: i32, base_defense: i32) {
        self.crit_chance = ((self.precision - 895) as f64 / 21.0).clamp(0.0, 100.0);
        self.crit_damage = 150.0 + self.ferocity as f64 / 15.0;
        self.health = self.vitality * 10 + base_health;
        self.armor = self.toughness + base_defense;
    }
}

/// Combat performance metrics for UI display.
/// Calculated from the optimizer's CombatPerformance struct,
/// rounded to display-friendly values.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CombatMetrics {
    pub effective_power: i32,
    pub strike_dps_index: i32,
    pub condition_dps_index: i32,
    pub total_dps_index: i32,
    pub healing_index: i32,
    pub crit_chance: f64,
    pub boon_duration_pct: f64,
    pub condi_duration_pct: f64,
    pub effective_health: i32,
    pub damage_reduction_pct: f64,
    pub bleeding_tick: i32,
    pub burning_tick: i32,
    pub poison_tick: i32,
    pub torment_tick: i32,
    pub confusion_tick: i32,
}

/// Rotation simulation breakdown for UI display.
/// Shows simulated DPS, condition uptimes, buff uptimes, and skill usage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RotationBreakdown {
    pub simulated_dps: i32,
    pub strike_dps: i32,
    pub condition_dps: i32,
    /// Average condition stacks (condition_name → avg_stacks).
    pub condition_uptime: Vec<(String, f64)>,
    /// Buff uptime percentages (buff_name → pct 0-100).
    pub buff_uptime: Vec<(String, f64)>,
    /// Skill usage in the rotation (name, cast_count, dps_contribution).
    pub skill_usage: Vec<(String, u32, i32)>,
    /// Number of stunbreaks in the skill bar.
    pub stunbreak_count: u32,
    /// Whether the build has stability access.
    pub has_stability: bool,
    /// Stability uptime percentage (0.0-1.0).
    pub stability_uptime: f64,
    /// Number of equipped skills that have at least one cleanse effect.
    pub cleanse_count: u32,
    /// Estimated conditions removed per 20 seconds.
    pub cleanse_rate_per_20s: f64,
}

/// A saved optimizer build for persistence (Save/Load tab).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GearPrefixGroups {
    #[serde(default)]
    pub armor: String,
    #[serde(default)]
    pub trinkets: String,
    #[serde(default)]
    pub weapons: String,
}

// Per-slot gear model

/// Every equipment slot that carries a stat prefix. A two-handed weapon fills
/// its set's Main slot; the Off slot stays `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GearSlot {
    #[serde(rename = "helm")]
    Helm,
    #[serde(rename = "shoulders")]
    Shoulders,
    #[serde(rename = "coat")]
    Coat,
    #[serde(rename = "gloves")]
    Gloves,
    #[serde(rename = "leggings")]
    Leggings,
    #[serde(rename = "boots")]
    Boots,
    #[serde(rename = "back")]
    Back,
    #[serde(rename = "accessory-1")]
    Accessory1,
    #[serde(rename = "accessory-2")]
    Accessory2,
    #[serde(rename = "amulet")]
    Amulet,
    #[serde(rename = "ring-1")]
    Ring1,
    #[serde(rename = "ring-2")]
    Ring2,
    #[serde(rename = "weapon-set-1-main")]
    WeaponSet1Main,
    #[serde(rename = "weapon-set-1-off")]
    WeaponSet1Off,
    #[serde(rename = "weapon-set-2-main")]
    WeaponSet2Main,
    #[serde(rename = "weapon-set-2-off")]
    WeaponSet2Off,
}

impl GearSlot {
    /// Canonical order — array position == `GearSlots.map` position.
    pub const ALL: [GearSlot; 16] = [
        GearSlot::Helm,
        GearSlot::Shoulders,
        GearSlot::Coat,
        GearSlot::Gloves,
        GearSlot::Leggings,
        GearSlot::Boots,
        GearSlot::Back,
        GearSlot::Accessory1,
        GearSlot::Accessory2,
        GearSlot::Amulet,
        GearSlot::Ring1,
        GearSlot::Ring2,
        GearSlot::WeaponSet1Main,
        GearSlot::WeaponSet1Off,
        GearSlot::WeaponSet2Main,
        GearSlot::WeaponSet2Off,
    ];

    /// Canonical kebab-case slot name — the same string used for serde
    /// encoding, saves, and search identity.
    pub fn kebab_name(&self) -> &'static str {
        slot_name(self)
    }

    /// The equipment budget slot type this gear slot draws from (matches the
    /// kebab names used by the optimizer's slot-budget tables).
    pub fn budget_slot(&self) -> &'static str {
        match self {
            GearSlot::Helm => "helm",
            GearSlot::Shoulders => "shoulders",
            GearSlot::Coat => "coat",
            GearSlot::Gloves => "gloves",
            GearSlot::Leggings => "leggings",
            GearSlot::Boots => "boots",
            GearSlot::Back => "back-item",
            GearSlot::Accessory1 | GearSlot::Accessory2 => "accessory",
            GearSlot::Amulet => "amulet",
            GearSlot::Ring1 | GearSlot::Ring2 => "ring",
            // Two-handed or one-handed budget selection needs weapon presence
            // knowledge, so we expose the raw slot and let the caller decide.
            GearSlot::WeaponSet1Main | GearSlot::WeaponSet2Main => "weapon-main",
            GearSlot::WeaponSet1Off | GearSlot::WeaponSet2Off => "weapon-off",
        }
    }
}

/// A resolved stat prefix reference for one gear slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixRef {
    pub itemstat_id: u32,
    pub name: String,
}

/// Per-slot gear prefixes, indexed by `GearSlot::ALL` position.
///
/// Serialization is a sparse map of populated slots (kebab-case slot name →
/// prefix), so saves stay readable and two-hander off-hands simply don't
/// appear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GearSlots {
    pub map: [Option<PrefixRef>; 16],
    /// Unknown kebab-case slot keys, preserved so a future slot is not
    /// deleted on round-trip. Known names in [`GearSlot::ALL`] never land here.
    extras: std::collections::BTreeMap<String, PrefixRef>,
}

impl Serialize for GearSlots {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut populated: std::collections::BTreeMap<&str, &PrefixRef> = GearSlot::ALL
            .iter()
            .zip(self.map.iter())
            .filter_map(|(slot, prefix)| prefix.as_ref().map(|prefix| (slot_name(slot), prefix)))
            .collect();
        for (key, prefix) in &self.extras {
            populated.insert(key.as_str(), prefix);
        }
        populated.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GearSlots {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut raw: std::collections::HashMap<String, PrefixRef> =
            std::collections::HashMap::deserialize(deserializer)?;
        let mut slots = GearSlots::default();
        for slot in GearSlot::ALL {
            if let Some(prefix) = raw.remove(slot_name(&slot)) {
                slots.map[slot as usize] = Some(prefix);
            }
        }
        slots.extras = raw.into_iter().collect();
        Ok(slots)
    }
}

fn slot_name(slot: &GearSlot) -> &'static str {
    // Serialize/Deserialize on the enum uses the same kebab-case names.
    match slot {
        GearSlot::Helm => "helm",
        GearSlot::Shoulders => "shoulders",
        GearSlot::Coat => "coat",
        GearSlot::Gloves => "gloves",
        GearSlot::Leggings => "leggings",
        GearSlot::Boots => "boots",
        GearSlot::Back => "back",
        GearSlot::Accessory1 => "accessory-1",
        GearSlot::Accessory2 => "accessory-2",
        GearSlot::Amulet => "amulet",
        GearSlot::Ring1 => "ring-1",
        GearSlot::Ring2 => "ring-2",
        GearSlot::WeaponSet1Main => "weapon-set-1-main",
        GearSlot::WeaponSet1Off => "weapon-set-1-off",
        GearSlot::WeaponSet2Main => "weapon-set-2-main",
        GearSlot::WeaponSet2Off => "weapon-set-2-off",
    }
}

impl GearSlots {
    pub fn get(&self, slot: GearSlot) -> Option<&PrefixRef> {
        self.map[slot as usize].as_ref()
    }

    pub fn set(&mut self, slot: GearSlot, prefix: PrefixRef) {
        self.map[slot as usize] = Some(prefix);
    }

    pub fn clear(&mut self, slot: GearSlot) {
        self.map[slot as usize] = None;
    }

    pub fn prefix_id(&self, slot: GearSlot) -> Option<u32> {
        self.get(slot).map(|prefix| prefix.itemstat_id)
    }

    /// Expand the legacy model (build-wide `stat_prefix` + armor/trinkets/
    /// weapons groups) into the slot vector, resolving each prefix name to a
    /// real itemstat id through `resolve`.
    ///
    /// `resolve` takes a prefix name and returns `(itemstat id, canonical
    /// name)`. The optimizer passes `GameDb::itemstat_by_name`; callers with no
    /// game data on hand pass `|_| None` via [`GearSlots::from_legacy`] and get
    /// the old placeholder id back.
    ///
    /// Legacy saves carried no off-hand concept, so weapons expand to Set1Main
    /// only — the active set — per the red-team BLOCKER-1 finding (never
    /// double-count Set2). There is nothing in a legacy save to back-fill an
    /// off-hand from: the group holds one prefix and no weapon names. A
    /// migrated build is therefore priced from the weapons it actually
    /// declares, not from a guess about the hand it might have held.
    pub fn from_legacy_with(
        stat_prefix: &str,
        groups: &GearPrefixGroups,
        mut resolve: impl FnMut(&str) -> Option<(u32, String)>,
    ) -> Self {
        let fallback = |value: &str| -> String {
            if value.trim().is_empty() {
                stat_prefix.to_string()
            } else {
                value.to_string()
            }
        };
        let mut reference = |name: &str| match resolve(name) {
            Some((itemstat_id, canonical)) => PrefixRef {
                itemstat_id,
                name: canonical,
            },
            // Unresolvable: keep the name and stamp 0. A zero id is not a real
            // itemstat and never prices a slot — the stat appliers report it as
            // a data quality reason rather than silently contributing nothing.
            None => PrefixRef {
                itemstat_id: 0,
                name: name.to_string(),
            },
        };
        let mut slots = GearSlots::default();
        let armor = reference(&fallback(&groups.armor));
        let trinkets = reference(&fallback(&groups.trinkets));
        let weapons = reference(&fallback(&groups.weapons));
        for slot in [
            GearSlot::Helm,
            GearSlot::Shoulders,
            GearSlot::Coat,
            GearSlot::Gloves,
            GearSlot::Leggings,
            GearSlot::Boots,
        ] {
            slots.set(slot, armor.clone());
        }
        for slot in [
            GearSlot::Back,
            GearSlot::Accessory1,
            GearSlot::Accessory2,
            GearSlot::Amulet,
            GearSlot::Ring1,
            GearSlot::Ring2,
        ] {
            slots.set(slot, trinkets.clone());
        }
        slots.set(GearSlot::WeaponSet1Main, weapons);
        slots
    }

    /// [`GearSlots::from_legacy_with`] with no game data to resolve against:
    /// every slot keeps its legacy prefix *name* and an `itemstat_id` of 0.
    ///
    /// Correct only for consumers that read names — display, chat codes,
    /// feedback — and read stats from the save's own `estimated_stats`. Any
    /// path that turns the result into a scored build must resolve the ids
    /// first, either by calling `from_legacy_with` instead or by running
    /// `ValidatedBuild::resolve_slot_prefix_ids`.
    pub fn from_legacy(stat_prefix: &str, groups: &GearPrefixGroups) -> Self {
        Self::from_legacy_with(stat_prefix, groups, |_| None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedBuild {
    pub name: String,
    pub timestamp: u64, // Unix epoch seconds
    pub character_name: String,
    pub game_mode: GameMode,
    /// Profession name (e.g. "Necromancer"). Empty for pre-P3-16 saves;
    /// treated as "Warrior" fallback at load time (backward-compat shim).
    #[serde(default)]
    pub profession: String,
    /// Engine version that created this save (informational, e.g. "1.0.0").
    #[serde(default)]
    pub engine_version: String,
    /// Balance manifest version used when this save was created (P3-08, future).
    #[serde(default)]
    pub balance_manifest_version: Option<String>,
    pub label: String,
    pub stat_prefix: String,
    /// Canonical per-group prefixes. Empty fields in older saves fall back to
    /// `stat_prefix` when displayed.
    #[serde(default)]
    pub gear_prefixes: GearPrefixGroups,
    /// Authoritative per-slot prefixes (v1.6.4+). `None` on legacy saves —
    /// expand `stat_prefix`/`gear_prefixes` via `GearSlots::from_legacy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_prefixes: Option<GearSlots>,
    pub specializations: Vec<(String, Vec<String>)>,
    pub weapons: Vec<String>,
    pub skills: Vec<String>,
    pub rune: String,
    pub sigils: Vec<String>,
    pub relic: String,
    pub explanation: String,
    #[serde(default)]
    pub synergy_explanation: String,
    pub changes_made: Vec<String>,
    pub estimated_stats: Option<StatBlock>,
    /// Free-form player note. Empty on older saves.
    #[serde(default)]
    pub notes: String,
}

#[cfg(test)]
mod tests {
    // Test fixtures are built field-by-field for readability.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    /// These tests snapshot the exact string format produced by
    /// `BuildLocks::describe_constraints`. Its former consumer,
    /// `crates/optimizer/src/prompts.rs::synergy_build_prompt`, was dead code
    /// (zero production callers) and was deleted; `describe_constraints`
    /// currently has no production caller either. If a prompt builder starts
    /// consuming this format again, update both the caller and these
    /// snapshots in the same change.

    #[test]
    fn describe_constraints_gear_locked_renders_canonical_slot_order() {
        // Insert out of canonical order to prove output is sorted by slot
        // position, not HashMap iteration order.
        let mut locks = BuildLocks::default();
        locks.gear_locks.insert(GearSlot::Ring2, 7);
        locks.gear_locks.insert(GearSlot::Helm, 99);
        assert_eq!(
            locks.describe_constraints(),
            "Gear helm locked to ID 99; Gear ring-2 locked to ID 7",
        );
    }

    #[test]
    fn describe_constraints_specs_traits_and_gear_all_render() {
        let mut locks = BuildLocks::default();
        locks.specs[2] = Some(34);
        locks.trait_locks.insert(34, [Some(1111), None, None]);
        locks.gear_locks.insert(GearSlot::Coat, 584);
        assert_eq!(
                locks.describe_constraints(),
                "Slot 3 spec locked to ID 34; Spec 34 Adept trait locked to ID 1111; Gear coat locked to ID 584",
            );
    }

    #[test]
    fn build_locks_json_without_gear_locks_still_loads() {
        // Older lock JSON predates gear_locks; serde(default) must fill it in.
        let json = r#"{"specs":[null,null,null],"trait_locks":{}}"#;
        let locks: BuildLocks = serde_json::from_str(json).expect("legacy JSON must load");
        assert!(locks.gear_locks.is_empty());
        assert!(locks.trait_locks.is_empty());
    }

    #[test]
    fn build_locks_gear_locks_round_trip_through_kebab_names() {
        let mut locks = BuildLocks::default();
        locks.gear_locks.insert(GearSlot::Ring1, 918);
        let json = serde_json::to_string(&locks).expect("serialize");
        let back: BuildLocks = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.gear_locks.get(&GearSlot::Ring1), Some(&918));
    }

    #[test]
    fn describe_constraints_no_locks() {
        let locks = BuildLocks::default();
        assert_eq!(locks.describe_constraints(), "");
    }

    #[test]
    fn describe_constraints_only_spec_locked() {
        let mut locks = BuildLocks::default();
        locks.specs[0] = Some(5);
        locks.specs[2] = Some(34);
        assert_eq!(
            locks.describe_constraints(),
            "Slot 1 spec locked to ID 5; Slot 3 spec locked to ID 34",
        );
    }

    #[test]
    fn describe_constraints_only_traits_locked() {
        // Use a single spec_id so HashMap iteration order can't make the
        // snapshot flaky.
        let mut locks = BuildLocks::default();
        locks
            .trait_locks
            .insert(34, [Some(1111), Some(2222), Some(3333)]);
        assert_eq!(
            locks.describe_constraints(),
            "Spec 34 Adept trait locked to ID 1111; Spec 34 Master trait locked to ID 2222; Spec 34 Grandmaster trait locked to ID 3333",
        );
    }

    #[test]
    fn describe_constraints_both_spec_and_traits_locked() {
        // Single spec_id again to avoid HashMap iteration nondeterminism.
        let mut locks = BuildLocks::default();
        locks.specs[2] = Some(34);
        locks.trait_locks.insert(34, [Some(1111), None, Some(3333)]);
        assert_eq!(
            locks.describe_constraints(),
            "Slot 3 spec locked to ID 34; Spec 34 Adept trait locked to ID 1111; Spec 34 Grandmaster trait locked to ID 3333",
        );
    }

    /// `trait_locks` is a `HashMap<u32, _>`, so its iteration order is
    /// nondeterministic. `describe_constraints` must sort by `spec_id` so the
    /// emitted string is stable across runs (these snapshots assert that).
    #[test]
    fn describe_constraints_two_specs_with_traits_sorted_by_spec_id() {
        // Insert in the reverse of the expected order to expose any reliance
        // on insertion order.
        let mut locks = BuildLocks::default();
        locks.trait_locks.insert(48, [Some(2001), None, None]);
        locks.trait_locks.insert(34, [Some(1111), Some(2222), None]);
        assert_eq!(
            locks.describe_constraints(),
            "Spec 34 Adept trait locked to ID 1111; Spec 34 Master trait locked to ID 2222; Spec 48 Adept trait locked to ID 2001",
        );
    }

    #[test]
    fn describe_constraints_three_specs_with_traits_sorted_by_spec_id() {
        // Insert out of order; expected output is sorted ascending by spec_id.
        let mut locks = BuildLocks::default();
        locks.trait_locks.insert(72, [None, None, Some(7000)]);
        locks.trait_locks.insert(5, [Some(500), None, Some(502)]);
        locks.trait_locks.insert(34, [None, Some(3400), None]);
        assert_eq!(
            locks.describe_constraints(),
            "Spec 5 Adept trait locked to ID 500; Spec 5 Grandmaster trait locked to ID 502; Spec 34 Master trait locked to ID 3400; Spec 72 Grandmaster trait locked to ID 7000",
        );
    }

    #[test]
    fn describe_constraints_mixed_spec_and_multi_spec_traits() {
        // Spec locks come first (in slot order), followed by trait locks
        // grouped by spec_id ascending. One of the trait-locked specs has no
        // matching spec lock, exercising the independence of the two maps.
        let mut locks = BuildLocks::default();
        locks.specs[0] = Some(5);
        locks.specs[2] = Some(34);
        locks.trait_locks.insert(34, [Some(3401), None, None]);
        locks.trait_locks.insert(5, [None, Some(502), Some(503)]);
        assert_eq!(
            locks.describe_constraints(),
            "Slot 1 spec locked to ID 5; Slot 3 spec locked to ID 34; Spec 5 Master trait locked to ID 502; Spec 5 Grandmaster trait locked to ID 503; Spec 34 Adept trait locked to ID 3401",
        );
    }

    #[test]
    fn game_mode_all_order_is_pinned() {
        // `GameMode::ALL` is iterated by tab rendering and default-lookup code
        // across the workspace. The order [PvE, PvP, WvW] is load-bearing — if
        // it silently flips, UI tabs and default profiles reorder with it.
        assert_eq!(GameMode::ALL, [GameMode::PvE, GameMode::PvP, GameMode::WvW],);
    }

    #[test]
    fn game_mode_default_is_pve() {
        // Pinned alongside ALL's order because several consumers rely on the
        // default game mode matching the first element of ALL.
        assert_eq!(GameMode::default(), GameMode::PvE);
        assert_eq!(GameMode::default(), GameMode::ALL[0]);
    }

    #[test]
    fn statblock_compute_derived_pins_formula() {
        // Locks the hardcoded constants (895, 21, 150, 15, 10) against silent
        // drift from `data/formulas/universal.json`. If any of these asserts
        // flip, cross-check `optimizer/src/stats.rs::compute_derived` and the
        // loaded values in `universal.json` — a mismatch means optimizer and
        // core disagree on derived stats.
        let mut s = StatBlock::default();
        s.precision = 895;
        s.ferocity = 0;
        s.vitality = 100;
        s.toughness = 50;
        s.compute_derived(1000, 1920);
        assert_eq!(s.crit_chance, 0.0, "precision == threshold → 0% crit");
        assert_eq!(s.crit_damage, 150.0, "ferocity == 0 → 150% base crit dmg");
        assert_eq!(s.health, 100 * 10 + 1000);
        assert_eq!(s.armor, 50 + 1920);

        // Non-zero ferocity / above-threshold precision.
        let mut s = StatBlock::default();
        s.precision = 895 + 21 * 50; // +50% crit chance
        s.ferocity = 150; // +10% crit damage
        s.compute_derived(0, 0);
        assert_eq!(s.crit_chance, 50.0);
        assert_eq!(s.crit_damage, 160.0);

        // Clamp: below threshold precision must floor at 0%, not go negative.
        let mut s = StatBlock::default();
        s.precision = 0;
        s.compute_derived(0, 0);
        assert_eq!(s.crit_chance, 0.0);

        // Clamp: above 100% crit chance must saturate.
        let mut s = StatBlock::default();
        s.precision = 895 + 21 * 500;
        s.compute_derived(0, 0);
        assert_eq!(s.crit_chance, 100.0);
    }

    #[test]
    fn gear_slots_round_trip_sparse() {
        let mut slots = GearSlots::default();
        slots.set(
            GearSlot::Helm,
            PrefixRef {
                itemstat_id: 7,
                name: "Berserker's".into(),
            },
        );
        let json = serde_json::to_string(&slots).unwrap();
        assert!(json.contains("berserker") || json.contains("Berserker"));
        let back: GearSlots = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get(GearSlot::Helm).unwrap().itemstat_id, 7);
        assert_eq!(back.get(GearSlot::Amulet), None);
    }

    #[test]
    fn gear_slots_unknown_key_round_trips() {
        let json = r#"{"helm":{"itemstat_id":7,"name":"Berserker's"},"relic":{"itemstat_id":99,"name":"Future Slot"}}"#;
        let slots: GearSlots = serde_json::from_str(json).unwrap();
        assert_eq!(slots.get(GearSlot::Helm).unwrap().itemstat_id, 7);
        let back = serde_json::to_string(&slots).unwrap();
        let v: serde_json::Value = serde_json::from_str(&back).unwrap();
        assert_eq!(v["relic"]["itemstat_id"], 99);
        assert_eq!(v["relic"]["name"], "Future Slot");
        assert_eq!(v["helm"]["itemstat_id"], 7);
    }

    #[test]
    fn gear_slots_from_legacy_expands_groups_set1_only() {
        let groups = GearPrefixGroups {
            armor: "Ritualist's".into(),
            trinkets: "".into(), // inherits stat_prefix
            weapons: "Cavalier's".into(),
        };
        let slots = GearSlots::from_legacy("Viper's", &groups);
        // armor group wins over stat_prefix
        assert_eq!(slots.get(GearSlot::Helm).unwrap().name, "Ritualist's");
        // blank trinket group inherits the build-wide prefix
        assert_eq!(slots.get(GearSlot::Amulet).unwrap().name, "Viper's");
        // weapons expand to Set1Main ONLY — never Set2 (red-team BLOCKER-1)
        assert_eq!(
            slots.get(GearSlot::WeaponSet1Main).unwrap().name,
            "Cavalier's"
        );
        assert_eq!(slots.get(GearSlot::WeaponSet1Off), None);
        assert_eq!(slots.get(GearSlot::WeaponSet2Main), None);
    }

    #[test]
    fn gear_slots_covers_all_positions() {
        let slots = GearSlots::default();
        assert_eq!(slots.map.len(), GearSlot::ALL.len());
    }

    /// Legacy migration must not hand the stat appliers an `itemstat_id` of 0.
    ///
    /// A zero id resolves to no itemstat, and the appliers used to skip it
    /// without a word — so a loaded legacy save could be scored as a build
    /// wearing nothing while its names all read correctly on screen.
    #[test]
    fn from_legacy_resolves_ids() {
        let groups = GearPrefixGroups {
            armor: "Berserkers".into(),
            trinkets: String::new(),
            weapons: "Marauder's".into(),
        };

        // The resolver stands in for `GameDb::itemstat_by_name`: it canonicalises
        // the name as well as supplying the id, so a save written with a
        // different spelling comes back on the game's own terms.
        let mut asked: Vec<String> = Vec::new();
        let slots = GearSlots::from_legacy_with("Cleric's", &groups, |name| {
            asked.push(name.to_string());
            match name {
                "Berserkers" => Some((161, "Berserker's".to_string())),
                "Marauder's" => Some((1128, "Marauder's".to_string())),
                "Cleric's" => Some((1039, "Cleric's".to_string())),
                _ => None,
            }
        });

        // Every populated slot carries a real id, and the id matches the name.
        let expected = |slot: GearSlot| -> (u32, &'static str) {
            match slot {
                GearSlot::Helm
                | GearSlot::Shoulders
                | GearSlot::Coat
                | GearSlot::Gloves
                | GearSlot::Leggings
                | GearSlot::Boots => (161, "Berserker's"),
                GearSlot::WeaponSet1Main => (1128, "Marauder's"),
                // Empty trinket group falls back to the build-wide prefix.
                _ => (1039, "Cleric's"),
            }
        };
        let mut populated = 0;
        for slot in GearSlot::ALL {
            let Some(prefix) = slots.get(slot) else {
                continue;
            };
            populated += 1;
            let (id, name) = expected(slot);
            assert_eq!(
                (prefix.itemstat_id, prefix.name.as_str()),
                (id, name),
                "slot {slot:?} migrated to the wrong prefix"
            );
        }
        // 6 armour + 6 trinkets + main hand. Counted, not asserted from a
        // constant lifted out of the implementation.
        assert_eq!(populated, 13);

        // The off-hand and both of set 2 stay empty: a legacy save records one
        // weapons-group prefix and no hands, so there is nothing to back-fill
        // an off-hand from, and inventing one would spend a budget the build
        // may never have had.
        for empty in [
            GearSlot::WeaponSet1Off,
            GearSlot::WeaponSet2Main,
            GearSlot::WeaponSet2Off,
        ] {
            assert!(slots.get(empty).is_none(), "{empty:?} was invented");
        }

        // One lookup per group, not one per slot.
        asked.sort();
        assert_eq!(asked, vec!["Berserkers", "Cleric's", "Marauder's"]);

        // Names the game data does not know keep their zero id rather than
        // being pointed at some other prefix.
        let unknown = GearSlots::from_legacy_with("Nonesuch", &groups, |_| None);
        assert_eq!(unknown.prefix_id(GearSlot::Helm), Some(0));
        assert_eq!(
            unknown.get(GearSlot::Helm).map(|p| p.name.as_str()),
            Some("Berserkers")
        );

        // And the no-resolver door is exactly that call.
        assert_eq!(GearSlots::from_legacy("Nonesuch", &groups), unknown);
    }
}
