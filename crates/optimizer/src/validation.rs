//! Validates Gemini build output against the GameDb.
//! Resolves names to IDs, checks GW2 build rules (spec slots, trait columns,
//! weapon availability, skill slots), and reports errors and warnings.
//! A ValidatedBuild is always returned — even with errors — so the caller
//! can decide whether to proceed with a partial result.

use std::collections::HashMap;

use gw2_api::models::{Item, Skill, Specialization, Trait as GW2Trait};

use gw2_core::types::{GearSlot, GearSlots, PrefixRef};

use crate::data::weapon_hands::{self, Hand, WeaponAccess};
use crate::gamedb::GameDb;
use crate::prompts::GeminiBuildResponse;
use crate::sigil_slots::{SigilSlot, SigilSlots, SIGIL_SLOT_COUNT};

/// Slots fed by the old armor group (all six armor pieces).
pub const ARMOR_SLOTS: [GearSlot; 6] = [
    GearSlot::Helm,
    GearSlot::Shoulders,
    GearSlot::Coat,
    GearSlot::Gloves,
    GearSlot::Leggings,
    GearSlot::Boots,
];

/// Slots fed by the old trinket group (back + accessories + amulet + rings).
pub const TRINKET_SLOTS: [GearSlot; 6] = [
    GearSlot::Back,
    GearSlot::Accessory1,
    GearSlot::Accessory2,
    GearSlot::Amulet,
    GearSlot::Ring1,
    GearSlot::Ring2,
];

/// Weapon-set-1 slots whose budgets are consumed by stat application.
/// Set 2 never draws slot budgets today (inactive-set invariant).
pub const WEAPON_SET1_SLOTS: [GearSlot; 2] = [GearSlot::WeaponSet1Main, GearSlot::WeaponSet1Off];

/// A fully validated and resolved build from Gemini output.
pub type ValidatedPets = (Option<u32>, Option<u32>, Option<u32>, Option<u32>);

#[derive(Debug, Clone, Default)]
pub struct ValidatedBuild {
    pub specializations: Vec<ValidatedSpec>,
    pub weapons: ValidatedWeapons,
    pub skills: ValidatedSkills,
    /// Revenant terrestrial legends (`Legend1`…), active first.
    pub legends: Vec<String>,
    /// Revenant aquatic legends. Empty → encoder copies terrestrial.
    pub aquatic_legends: Vec<String>,
    /// Ranger pet IDs: terrestrial[2] then aquatic[2].
    pub pets: Option<ValidatedPets>,
    pub rune: Option<ValidatedItem>,
    /// Every resolved sigil, in canonical seat order, holes removed.
    ///
    /// Dense on purpose: display, saves, chat links and the beam's sigil
    /// operator all index it, and a `Vec<Option<_>>` here would ripple into
    /// every one of them. What the density costs is *which seat* each sigil
    /// came from — [`ValidatedBuild::sigil_seats`] keeps that, and
    /// [`ValidatedBuild::active_sigil_ids`] is the only supported way to ask
    /// "which sigils are on the character".
    pub sigils: Vec<ValidatedItem>,
    /// The four sigil seats — `[set 1 main, set 1 off, set 2 main, set 2 off]`
    /// — holes included, when the constructor knew them.
    ///
    /// Empty means "seats not recorded", not "no sigils": the synergy seeder
    /// and the beam build dense, hole-free sigil lists where position already
    /// *is* seat order, and recording seats for them would let the beam's
    /// in-place `sigils[i] = …` swap drift out of sync with the seats and stop
    /// registering in the stat sheet.
    pub sigil_seats: SigilSlots,
    pub relic: Option<ValidatedItem>,
    /// Nourishment (food). Same ownership as rune/relic. Inner argmax writes
    /// the chosen id so the winner serializes it. None until solved or locked.
    pub food: Option<ValidatedItem>,
    /// Enhancement (utility consumable). Same ownership as rune/relic.
    pub utility: Option<ValidatedItem>,
    /// Per worn-slot infusion seats (not a bag). Layout from item infusion_slots;
    /// inner argmax writes chosen ids. Empty in PvP.
    pub infusion_seats: Vec<crate::infusions::InfusionSeat>,
    /// Per-slot gear prefixes. Population policy (Task 2): every constructor
    /// that used to write the build-wide `gear_prefix` now fills all sixteen
    /// slots; category overrides (the old `gear_groups`) overwrite their own
    /// members. There is no runtime inheritance any more.
    pub gear_slots: GearSlots,
    pub explanation: String,
    pub synergy_explanation: String,
    pub changes: Vec<ChangeEntry>,
    pub warnings: Vec<String>,
    pub errors: Vec<ValidationReject>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedSpec {
    pub spec_id: u32,
    pub name: String,
    pub elite: bool,
    /// Selected major trait IDs (3 per spec: Adept, Master, Grandmaster).
    pub trait_ids: Vec<u32>,
    pub trait_names: Vec<String>,
    /// All equipped trait IDs: minor traits + selected major traits.
    pub all_trait_ids: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidatedWeapons {
    pub set1: ValidatedWeaponSet,
    pub set2: ValidatedWeaponSet,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidatedWeaponSet {
    pub main_hand: Option<String>,
    pub off_hand: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ValidatedSkills {
    pub heal: Option<(u32, String)>,
    pub utilities: Vec<Option<(u32, String)>>,
    pub elite: Option<(u32, String)>,
    /// F1-F5 and elite-spec mechanic skills (Steal, Full Counter, shatters,
    /// bladesongs, etc.). These are part of the executable WvW chain.
    pub profession: Vec<(u32, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedItem {
    pub id: u32,
    pub name: String,
}

impl ValidatedBuild {
    /// The prefix a specific slot actually spends.
    pub fn prefix_for(&self, slot: GearSlot) -> Option<&PrefixRef> {
        self.gear_slots.get(slot)
    }

    /// The prefix that best represents the build: the one worn in the most
    /// slots, ties broken by canonical slot order.
    ///
    /// This is the closest analogue of the retired build-wide prefix, and it
    /// feeds the PvP amulet match, the displayed label, and the LLM prompt.
    /// Taking the *first* populated slot instead — which is `Helm` — meant
    /// that after the nudge pass produced a genuinely mixed build, one helm
    /// swap renamed the whole build and chose its PvP amulet. Modal is what a
    /// player means by "a Berserker's build with two Cavalier's trinkets".
    pub fn primary_prefix(&self) -> Option<&PrefixRef> {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for prefix in self.gear_slots.map.iter().flatten() {
            *counts.entry(prefix.itemstat_id).or_default() += 1;
        }
        self.gear_slots
            .map
            .iter()
            .enumerate()
            .filter_map(|(idx, cell)| cell.as_ref().map(|prefix| (idx, prefix)))
            // `max_by_key` keeps the *last* maximum, so reversing the slot
            // index makes the earliest slot in canonical order win a tie — the
            // same answer on every run, and the same one the old first-slot
            // reading gave for a uniform build.
            .max_by_key(|(idx, prefix)| {
                (
                    counts.get(&prefix.itemstat_id).copied().unwrap_or(0),
                    std::cmp::Reverse(*idx),
                )
            })
            .map(|(_, prefix)| prefix)
    }

    /// Fill every slot with one prefix. Per-slot encoding of the old
    /// build-wide assignment (`validated.gear_prefix = Some(p)`).
    ///
    /// Unconditional: this is the fixture/legacy expansion, and it writes an
    /// off-hand prefix onto a Greatsword. Production constructors want
    /// [`ValidatedBuild::fill_worn_gear_slots`], which asks the build what it
    /// is actually wearing.
    pub fn fill_gear_slots(&mut self, prefix: PrefixRef) {
        for slot in GearSlot::ALL {
            self.gear_slots.set(slot, prefix.clone());
        }
    }

    /// Fill one prefix into every slot the build actually wears.
    ///
    /// Weapon slots are wearable only when that hand holds a weapon: a
    /// two-hander has no off-hand, weapon set 2 is carried rather than worn,
    /// and a half-built draft has whatever it has. Writing a prefix into those
    /// cells anyway is not harmless bookkeeping — it churns
    /// [`ValidatedBuild::gear_identity`], so two builds with identical combat
    /// stats dedup as different candidates and each spends beam budget; it puts
    /// a stat prefix on an empty off-hand in the gear sheet; and it is the
    /// shape the Improve baseline is compared against.
    ///
    /// Armour and trinkets are always wearable — every level-80 character has
    /// six of each — so a *missing piece* on someone's live loadout is a
    /// different question, answered by whoever built the plate, not here.
    ///
    /// Call **after** the build's weapons are set. A build with no weapons at
    /// all wears no weapons, and gets no weapon prefixes.
    pub fn fill_worn_gear_slots(&mut self, prefix: PrefixRef) {
        for slot in GearSlot::ALL {
            if !self.wears(slot) {
                self.gear_slots.clear(slot);
                continue;
            }
            self.gear_slots.set(slot, prefix.clone());
        }
    }

    /// Does this build wear the given slot?
    ///
    /// True for all armour and trinkets; for a weapon slot, true exactly when
    /// that hand of that set holds a weapon.
    pub fn wears(&self, slot: GearSlot) -> bool {
        let held = |hand: &Option<String>| hand.as_deref().is_some_and(|w| !w.trim().is_empty());
        match slot {
            GearSlot::WeaponSet1Main => held(&self.weapons.set1.main_hand),
            GearSlot::WeaponSet1Off => held(&self.weapons.set1.off_hand),
            GearSlot::WeaponSet2Main => held(&self.weapons.set2.main_hand),
            GearSlot::WeaponSet2Off => held(&self.weapons.set2.off_hand),
            _ => true,
        }
    }

    /// Record the four sigil seats and the dense list together.
    ///
    /// The only supported way to set a build's sigils when the seats are known:
    /// it is what keeps [`ValidatedBuild::sigils`] and
    /// [`ValidatedBuild::sigil_seats`] describing the same build.
    pub fn set_sigil_seats(&mut self, seats: [Option<ValidatedItem>; SIGIL_SLOT_COUNT]) {
        let mut slots = SigilSlots::default();
        self.sigils.clear();
        for (seat, item) in SigilSlot::ALL.into_iter().zip(seats) {
            if let Some(item) = item {
                slots.set(seat, Some(item.id));
                self.sigils.push(item);
            }
        }
        self.sigil_seats = slots;
    }

    /// The sigils actually on the character: weapon set 1's two seats.
    ///
    /// When seats were recorded, read them — that is the whole point of
    /// recording them. Otherwise fall back to the leading entries of the dense
    /// list, which is exactly right for the constructors that do not record
    /// seats (synergy seed, beam neighbours) because they never produce holes,
    /// so dense order *is* seat order there.
    ///
    /// The old code always took `sigils[..2]`. A plate with an empty set-1 main
    /// hand, Force in the off-hand and Air on set 2 compacted to `[Force, Air]`
    /// — and the stat sheet then counted a **carried** sigil as worn and
    /// credited Force to the wrong hand.
    /// Every socketed sigil by weapon set: `[set 1, set 2]`, holes removed.
    /// Seats when recorded; otherwise the dense list's first two entries are
    /// set 1 and the rest set 2, the same reading `active_sigil_ids` uses.
    pub fn sigil_ids_by_set(&self) -> [Vec<u32>; 2] {
        if !self.sigil_seats.is_empty() {
            let seat = |slot: SigilSlot| self.sigil_seats.get(slot);
            return [
                seat(SigilSlot::Set1Main)
                    .into_iter()
                    .chain(seat(SigilSlot::Set1Off))
                    .collect(),
                seat(SigilSlot::Set2Main)
                    .into_iter()
                    .chain(seat(SigilSlot::Set2Off))
                    .collect(),
            ];
        }
        let ids: Vec<u32> = self.sigils.iter().map(|sigil| sigil.id).collect();
        [
            ids.iter().take(2).copied().collect(),
            ids.iter().skip(2).take(2).copied().collect(),
        ]
    }

    pub fn active_sigil_ids(&self) -> Vec<u32> {
        if !self.sigil_seats.is_empty() {
            return self
                .sigil_seats
                .active_set()
                .into_iter()
                .flatten()
                .collect();
        }
        self.sigils
            .iter()
            .take(2)
            .map(|sigil| sigil.id)
            .collect::<Vec<u32>>()
    }

    /// Fill every unlocked **worn** slot with one prefix; slots present in
    /// `gear_locks` keep their current value. Returns true when any slot value
    /// actually changed, so callers can skip no-op proposals without cloning
    /// first.
    ///
    /// Skips slots the build does not wear, for the reasons in
    /// [`ValidatedBuild::fill_worn_gear_slots`]. A search operator that fills a
    /// two-hander's off-hand and both of set 2 spends four evaluations per
    /// prefix on cells that cannot change a single stat.
    pub fn fill_unlocked_gear_slots(
        &mut self,
        prefix: PrefixRef,
        gear_locks: &HashMap<GearSlot, u32>,
    ) -> bool {
        let worn: [bool; 16] = std::array::from_fn(|idx| self.wears(GearSlot::ALL[idx]));
        let mut changed = false;
        for (idx, cell) in self.gear_slots.map.iter_mut().enumerate() {
            if gear_locks.contains_key(&GearSlot::ALL[idx]) {
                continue;
            }
            if !worn[idx] {
                changed |= cell.take().is_some();
                continue;
            }
            match cell {
                Some(existing) if *existing == prefix => {}
                cell_ref => {
                    *cell_ref = Some(prefix.clone());
                    changed = true;
                }
            }
        }
        changed
    }

    /// Resolve migration-produced zero itemstat ids (`GearSlots::from_legacy`
    /// stamps 0) against the game data using the shared deterministic
    /// lookup: exact name match wins (lower id tiebreak), else shortest fuzzy
    /// match. Unresolvable names keep their zero id rather than inventing one.
    // TODO(per-slot): from_legacy leaves WeaponSet1Off unpopulated for two-handed
    // Set1 weapons; pre-slot builds spent an off-hand budget there via the
    // build-wide fallback. No caller builds a ValidatedBuild from SavedBuild yet;
    // Task 3 must settle the backfill when weapon presence is known.
    /// Resolve migration-produced zero itemstat ids (`GearSlots::from_legacy`
    /// stamps 0 when it is given no resolver) against the game data using the
    /// shared deterministic lookup: exact name match wins (lower id tiebreak),
    /// else shortest fuzzy match. Unresolvable names keep their zero id rather
    /// than inventing one — and a zero id that reaches
    /// [`crate::engine::calculate_validated_stats`] is now reported as a data
    /// quality reason instead of being skipped in silence.
    ///
    /// Call this on any path that turns a `SavedBuild` into a `ValidatedBuild`.
    /// `gw2_core::types::GearSlots::from_legacy_with` is the better door — it
    /// resolves at construction, so the zero-id state never exists — but this
    /// stays for a map that has already been built.
    ///
    /// Weapons: `from_legacy` populates `WeaponSet1Main` only, because a legacy
    /// save records one weapons-group prefix and no hands. There is nothing to
    /// back-fill an off-hand *from*; inventing one would hand a migrated build
    /// a second one-hand budget it may never have had. The build's weapons
    /// decide the budget now (see `engine::land_weapon_slot_type`), so a
    /// migrated save that names no weapons is priced at zero weapon points
    /// rather than at a guess.
    pub fn resolve_slot_prefix_ids(&mut self, db: &GameDb) {
        for prefix in self.gear_slots.map.iter_mut().flatten() {
            if prefix.itemstat_id != 0 || prefix.name.is_empty() {
                continue;
            }
            if let Some(itemstat) = db.itemstat_by_name(&prefix.name) {
                prefix.itemstat_id = itemstat.id;
                prefix.name = itemstat.name.clone();
            }
        }
    }

    /// Search identity is the serialized populated slot map. `GearSlots`
    /// serializes as a sparse map keyed by canonical kebab slot names, so
    /// equal maps always serialize to equal strings and empty ones to `{}`.
    pub fn gear_identity(&self) -> String {
        serde_json::to_string(&self.gear_slots).unwrap_or_else(|_| "{}".into())
    }
}

/// A structured change entry from Gemini's output.
#[derive(Debug, Clone)]
pub struct ChangeEntry {
    pub slot: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// Machine-readable rejection code. Pair with `ValidationReject.detail`
/// (which is human-readable) so a retry loop can key off the typed code
/// and feed the LLM a precise correction instruction rather than parse
/// prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectCode {
    /// Build spec requires exactly 3 specializations; got a different count.
    WrongSpecCount { expected: usize, actual: usize },
    /// Specialization name not found for the given profession.
    SpecNotFound { spec: String, profession: String },
    /// Specialization exists but belongs to another profession.
    SpecWrongProfession {
        spec: String,
        owner: String,
        expected: String,
    },
    /// More than one elite spec in the 3-slot list.
    MultipleEliteSpecs { spec: String },
    /// The same specialization id was selected twice.
    DuplicateSpec { spec: String },
    /// Weapon not available for the profession.
    WeaponNotAvailable {
        slot: String,
        weapon: String,
        profession: String,
    },
    /// Rune/sigil/relic name not in game data (all fuzzy passes failed).
    ItemNotFound { item_type: String, name: String },
    /// Gear prefix (stat name) not in itemstats.
    GearPrefixNotFound { name: String },
    /// A specialization resolved with fewer than 3 major traits.
    IncompleteSpecTraits { spec: String, actual: usize },
    /// Heal / 3 utilities / elite bar is missing slots.
    IncompleteSkillBar {
        heal: bool,
        utilities: usize,
        elite: bool,
    },
    /// Heal/utility/elite name did not resolve (exact / alnum_key only).
    SkillNotFound { name: String },
    /// Ranger pet name did not resolve against GameDb.pets.
    PetNotFound { name: String },
}

/// Structured validator rejection. `detail` mirrors the prior flat string
/// format and is safe to show in UI (`impl Display`); `code` is the
/// stable discriminator for retry logic.
#[derive(Debug, Clone)]
pub struct ValidationReject {
    pub code: RejectCode,
    pub detail: String,
}

impl std::fmt::Display for ValidationReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Validate a parsed Gemini build response against the GameDb.
/// Always returns a ValidatedBuild, even if there are errors.
// The result is populated incrementally as each validation stage runs; a single
// struct literal would not match the staged, side-effecting validation flow.
#[allow(clippy::field_reassign_with_default)]
/// Resolve a profession from specialization names (Tempest → Elementalist).
pub fn infer_profession_from_spec_names<'a>(
    db: &GameDb,
    spec_names: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let mut spec_ids: Vec<u32> = db.specializations.keys().copied().collect();
    spec_ids.sort_unstable();
    for name in spec_names {
        let clean = name.trim_end_matches(" [E]").trim();
        for sid in &spec_ids {
            if let Some(spec) = db.specializations.get(sid) {
                if spec.name.eq_ignore_ascii_case(clean) {
                    return Some(spec.profession.clone());
                }
            }
        }
    }
    None
}

fn alnum_spaces(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect()
}

fn contains_as_words(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay = format!(" {} ", alnum_spaces(haystack));
    let n = format!(" {} ", alnum_spaces(needle).trim());
    hay.contains(&n)
}

/// Scan free text for a spec or profession name ("tempest celestial" → Elementalist).
pub fn infer_profession_from_text(db: &GameDb, text: &str) -> Option<String> {
    let mut specs: Vec<(&str, &str)> = db
        .specializations
        .values()
        .map(|s| (s.name.as_str(), s.profession.as_str()))
        .collect();
    specs.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    for (name, profession) in specs {
        if contains_as_words(text, name) {
            return Some(profession.to_string());
        }
    }
    let mut professions: Vec<&str> = db.professions.values().map(|p| p.name.as_str()).collect();
    professions.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for name in professions {
        if contains_as_words(text, name) {
            return Some(name.to_string());
        }
    }
    None
}

pub fn validate_gemini_build(
    response: &GeminiBuildResponse,
    db: &GameDb,
    profession_name: &str,
) -> ValidatedBuild {
    let inferred_profession = infer_profession_from_spec_names(
        db,
        response.specializations.iter().map(|(n, _)| n.as_str()),
    );
    let profession_name = match db.profession(profession_name) {
        Some(_) => profession_name,
        None => inferred_profession.as_deref().unwrap_or(profession_name),
    };

    let mut result = ValidatedBuild {
        explanation: response.explanation.clone(),
        synergy_explanation: response
            .synergy_explanation
            .clone()
            .unwrap_or_else(|| response.explanation.clone()),
        changes: parse_changes(response),
        ..ValidatedBuild::default()
    };

    validate_specializations(response, db, profession_name, &mut result);
    if result.specializations.len() != 3 {
        let actual = result.specializations.len();
        result.errors.push(ValidationReject {
            code: RejectCode::WrongSpecCount {
                expected: 3,
                actual,
            },
            detail: format!("Expected 3 specializations, got {}", actual),
        });
    }
    validate_weapons(response, db, profession_name, &mut result);
    validate_skills(response, db, profession_name, &mut result);
    validate_pets(response, db, &mut result);
    if !response.specializations.is_empty() {
        for spec in &result.specializations {
            if spec.trait_ids.len() != 3 {
                result.errors.push(ValidationReject {
                    code: RejectCode::IncompleteSpecTraits {
                        spec: spec.name.clone(),
                        actual: spec.trait_ids.len(),
                    },
                    detail: format!(
                        "{}: expected 3 traits, got {}",
                        spec.name,
                        spec.trait_ids.len()
                    ),
                });
            }
        }
    }
    if profession_name == "Revenant" {
        fill_revenant_legends(response, &mut result, db);
    }
    if !response.specializations.is_empty() {
        let utils = result
            .skills
            .utilities
            .iter()
            .filter(|u| u.is_some())
            .count();
        if result.skills.heal.is_some() && result.skills.elite.is_some() && utils == 3 {
            result.errors.retain(|e| {
                !matches!(
                    e.code,
                    RejectCode::IncompleteSkillBar { .. } | RejectCode::SkillNotFound { .. }
                )
            });
        } else {
            result.errors.push(ValidationReject {
                code: RejectCode::IncompleteSkillBar {
                    heal: result.skills.heal.is_some(),
                    utilities: utils,
                    elite: result.skills.elite.is_some(),
                },
                detail: format!(
                    "Need heal, 3 utilities, and elite (heal={}, utils={}, elite={})",
                    result.skills.heal.is_some(),
                    utils,
                    result.skills.elite.is_some()
                ),
            });
        }
    }
    validate_rune(response, db, &mut result);
    validate_sigils(response, db, &mut result);
    validate_relic(response, db, &mut result);
    validate_gear_prefix(response, db, &mut result);
    validate_gear_slot_map(response, db, &mut result);

    result
}

fn validate_specializations(
    response: &GeminiBuildResponse,
    db: &GameDb,
    profession_name: &str,
    result: &mut ValidatedBuild,
) {
    let prof = db.profession(profession_name);
    let prof_spec_ids: Vec<u32> = prof.map(|p| p.specializations.clone()).unwrap_or_default();

    let mut elite_count = 0;

    for (spec_name, trait_names) in &response.specializations {
        // Strip display-only " [E]" suffix that the LLM or UI may include for elite specs
        let spec_name_clean = spec_name.trim_end_matches(" [E]");
        let spec = find_spec_by_name(db, spec_name_clean, &prof_spec_ids, result);

        let Some(spec) = spec else {
            result.errors.push(ValidationReject {
                code: RejectCode::SpecNotFound {
                    spec: spec_name.clone(),
                    profession: profession_name.to_string(),
                },
                detail: format!(
                    "Specialization '{}' not found for {}",
                    spec_name, profession_name
                ),
            });
            continue;
        };

        // Check profession ownership
        if spec.profession != profession_name {
            result.errors.push(ValidationReject {
                code: RejectCode::SpecWrongProfession {
                    spec: spec.name.clone(),
                    owner: spec.profession.clone(),
                    expected: profession_name.to_string(),
                },
                detail: format!(
                    "Specialization '{}' belongs to {}, not {}",
                    spec.name, spec.profession, profession_name
                ),
            });
            continue;
        }

        // Reject a spec already accepted in this build (duplicate slot).
        if result.specializations.iter().any(|s| s.spec_id == spec.id) {
            result.errors.push(ValidationReject {
                code: RejectCode::DuplicateSpec {
                    spec: spec.name.clone(),
                },
                detail: format!("Specialization '{}' appears twice", spec.name),
            });
            continue;
        }

        if spec.elite {
            elite_count += 1;
            if elite_count > 1 {
                result.errors.push(ValidationReject {
                    code: RejectCode::MultipleEliteSpecs {
                        spec: spec.name.clone(),
                    },
                    detail: format!("Multiple elite specs selected ({})", spec.name),
                });
            }
        }

        // Resolve traits
        let spec_traits = db.spec_traits(spec.id);
        let major_traits: Vec<&GW2Trait> = spec_traits
            .iter()
            .filter(|t| t.slot == "Major")
            .copied()
            .collect();

        let mut resolved_trait_ids = Vec::new();
        let mut resolved_trait_names = Vec::new();
        let mut used_tiers: HashMap<u32, String> = HashMap::new();
        let mut unknown_trait = false;

        for trait_name in trait_names {
            if let Some(t) = find_trait_by_name(trait_name, &major_traits) {
                // Check column uniqueness
                if let Some(existing) = used_tiers.get(&t.tier) {
                    result.warnings.push(format!(
                        "Spec '{}': tier {} has '{}' and '{}' — keeping first",
                        spec.name,
                        tier_label(t.tier),
                        existing,
                        t.name
                    ));
                    continue;
                }
                used_tiers.insert(t.tier, t.name.clone());
                resolved_trait_ids.push(t.id);
                resolved_trait_names.push(t.name.clone());
            } else {
                unknown_trait = true;
                result.warnings.push(format!(
                    "Trait '{}' not found in spec '{}'",
                    trait_name, spec.name
                ));
            }
        }

        // Fill the columns the model left empty or named wrongly, but only when
        // it demonstrably knew this spec: at least one name resolved, or it
        // named none at all. A spec where NOTHING resolved is a hallucination
        // and must not be laundered into a legal build.
        //
        // The old test here was `!unknown_trait`, which threw away the whole
        // build over a single bad name out of three — and a spec stuck at 2
        // traits fails `plate_is_servable`, so the player got prose and no
        // build, permanently. Keeping the model's good picks and filling only
        // the column it fumbled is strictly closer to what it asked for than
        // discarding all three.
        let named_nothing_real = unknown_trait && resolved_trait_ids.is_empty();
        if !named_nothing_real {
            complete_major_trait_columns(
                &major_traits,
                &mut resolved_trait_ids,
                &mut resolved_trait_names,
                &mut used_tiers,
                &spec.name,
                &mut result.warnings,
            );
        }

        // Collect minor traits (always active)
        let minor_ids: Vec<u32> = spec.minor_traits.clone();
        let mut all_trait_ids = minor_ids;
        all_trait_ids.extend(&resolved_trait_ids);

        result.specializations.push(ValidatedSpec {
            spec_id: spec.id,
            name: spec.name.clone(),
            elite: spec.elite,
            trait_ids: resolved_trait_ids,
            trait_names: resolved_trait_names,
            all_trait_ids,
        });
    }
}

fn validate_weapons(
    response: &GeminiBuildResponse,
    db: &GameDb,
    profession_name: &str,
    result: &mut ValidatedBuild,
) {
    let prof = db.profession(profession_name);

    // response.weapons is Vec<String> like ["Set 1: Axe / Axe", "Set 2: Greatsword"],
    // or empty when using the newer field layout.
    let (set1, set2) = parse_weapon_sets_from_response(response, profession_name);

    result.weapons.set1 = validate_weapon_set(&set1, prof, result, "Set 1");
    result.weapons.set2 = validate_weapon_set(&set2, prof, result, "Set 2");
    // A single land set pastes as one kit. The game stores unique types, so
    // Sword+Axe alone cannot become Sword/Axe + anything. Fill a legal second
    // set when the plate omitted it (Choya often writes Set 1 only).
    if result.weapons.set2.main_hand.is_none() {
        let elite_ids: Vec<u32> = result
            .specializations
            .iter()
            .filter(|s| s.elite)
            .map(|s| s.spec_id)
            .collect();
        if let Some(set2) = complementary_weapon_set(&result.weapons.set1, prof, &elite_ids) {
            result.weapons.set2 = set2;
        }
    }
}

/// A different land combo than set 1, preferring a two-hander the elite can use.
fn complementary_weapon_set(
    set1: &ValidatedWeaponSet,
    prof: Option<&gw2_api::models::Profession>,
    elite_ids: &[u32],
) -> Option<ValidatedWeaponSet> {
    let prof = prof?;
    let set1_main = set1.main_hand.as_deref()?;
    let usable = |name: &str, info: &gw2_api::models::WeaponInfo| {
        if !info.land_usable(name) {
            return false;
        }
        match info.specialization {
            Some(req) => elite_ids.contains(&req),
            None => true,
        }
    };
    let mut two_hand: Vec<&str> = Vec::new();
    let mut mains: Vec<&str> = Vec::new();
    let mut offs: Vec<&str> = Vec::new();
    for (name, info) in &prof.weapons {
        if !usable(name, info) {
            continue;
        }
        if info.flags.iter().any(|f| f == "TwoHand") {
            two_hand.push(name.as_str());
        } else if info.flags.iter().any(|f| f == "Mainhand") {
            mains.push(name.as_str());
        }
        if info.flags.iter().any(|f| f == "Offhand") && !info.flags.iter().any(|f| f == "TwoHand") {
            offs.push(name.as_str());
        }
    }
    two_hand.sort_unstable();
    for &w in &two_hand {
        if !w.eq_ignore_ascii_case(set1_main) {
            return Some(ValidatedWeaponSet {
                main_hand: Some(w.to_string()),
                off_hand: None,
            });
        }
    }
    mains.sort_unstable();
    offs.sort_unstable();
    for &m in &mains {
        if m.eq_ignore_ascii_case(set1_main) {
            continue;
        }
        if let Some(&o) = offs.first() {
            return Some(ValidatedWeaponSet {
                main_hand: Some(m.to_string()),
                off_hand: Some(o.to_string()),
            });
        }
        return Some(ValidatedWeaponSet {
            main_hand: Some(m.to_string()),
            off_hand: None,
        });
    }
    None
}

fn weapon_is_two_hand(prof: &gw2_api::models::Profession, weapon: &str) -> bool {
    crate::weapon_budget::is_two_handed(weapon, Some(prof))
        || !matches!(
            weapon_hands::access(&prof.name, weapon, Hand::TwoHand),
            WeaponAccess::None
        )
}

fn wiki_main_hand(prof: &gw2_api::models::Profession, weapon: &str) -> Hand {
    if weapon_is_two_hand(prof, weapon) {
        Hand::TwoHand
    } else {
        Hand::Main
    }
}

fn push_weapon_not_available(
    result: &mut ValidatedBuild,
    label: &str,
    weapon: &str,
    profession: &str,
) {
    result.errors.push(ValidationReject {
        code: RejectCode::WeaponNotAvailable {
            slot: label.to_string(),
            weapon: weapon.to_string(),
            profession: profession.to_string(),
        },
        detail: format!(
            "{}: weapon '{}' not available for {}",
            label, weapon, profession
        ),
    });
}

/// Wiki table wins for known rows. `true` = keep the slot.
fn wiki_hand_allowed(
    prof: &gw2_api::models::Profession,
    weapon: &str,
    hand: Hand,
    result: &mut ValidatedBuild,
    label: &str,
) -> bool {
    if !weapon_hands::known_weapon(&prof.name, weapon) {
        return true;
    }
    if matches!(
        weapon_hands::access(&prof.name, weapon, hand),
        WeaponAccess::None
    ) {
        push_weapon_not_available(result, label, weapon, &prof.name);
        return false;
    }
    true
}

fn validate_weapon_set(
    weapons: &(Option<String>, Option<String>),
    prof: Option<&gw2_api::models::Profession>,
    result: &mut ValidatedBuild,
    label: &str,
) -> ValidatedWeaponSet {
    let mut set = ValidatedWeaponSet::default();

    let Some(prof) = prof else {
        return set;
    };

    if let Some(ref mh) = weapons.0 {
        if let Some((canonical, info)) = find_weapon(mh, prof) {
            if !info.land_usable(canonical) {
                result.errors.push(ValidationReject {
                    code: RejectCode::WeaponNotAvailable {
                        slot: label.to_string(),
                        weapon: canonical.clone(),
                        profession: prof.name.clone(),
                    },
                    detail: format!(
                        "{}: '{}' is underwater and cannot be a land weapon set",
                        label, canonical
                    ),
                });
            } else if wiki_hand_allowed(
                prof,
                canonical,
                wiki_main_hand(prof, canonical),
                result,
                label,
            ) {
                // Store canonical name so later `prof.weapons.get(...)` (case-sensitive)
                // hits — preserving the LLM's casing would bypass the elite spec gate.
                set.main_hand = Some(canonical.clone());
            }
        } else {
            push_weapon_not_available(result, label, mh, &prof.name);
        }
    }

    if let Some(ref oh) = weapons.1 {
        if let Some((canonical, info)) = find_weapon(oh, prof) {
            if !info.land_usable(canonical) {
                result.errors.push(ValidationReject {
                    code: RejectCode::WeaponNotAvailable {
                        slot: label.to_string(),
                        weapon: canonical.clone(),
                        profession: prof.name.clone(),
                    },
                    detail: format!(
                        "{}: '{}' is underwater and cannot be a land weapon set",
                        label, canonical
                    ),
                });
            } else if weapon_is_two_hand(prof, canonical) {
                push_weapon_not_available(result, label, canonical, &prof.name);
            } else if wiki_hand_allowed(prof, canonical, Hand::Off, result, label) {
                set.off_hand = Some(canonical.clone());
            }
        } else {
            push_weapon_not_available(result, label, oh, &prof.name);
        }
    }

    // No elite-spec gate: Weaponmaster Training makes every elite weapon
    // usable by every build of the profession (see `weapon_hands::is_legal`).
    set
}

fn validate_skills(
    response: &GeminiBuildResponse,
    db: &GameDb,
    profession_name: &str,
    result: &mut ValidatedBuild,
) {
    // Determine which elite spec (if any) is equipped — used to gate elite spec skills.
    let equipped_elite_spec_id: Option<u32> = result
        .specializations
        .iter()
        .find(|s| s.elite)
        .map(|s| s.spec_id);

    // Filter to only core skills (specialization == None) or skills from the equipped elite spec.
    // This prevents cross-spec skill suggestions from slipping through (e.g. Berserker using
    // a Spellbreaker utility when Spellbreaker is not equipped).
    // Racial skills are excluded from the search pools but a plate that
    // names one is legal for the right race, so resolve against everything
    // the profession can slot.
    let all_prof_skills = db.skills_usable_by(profession_name);
    let prof_skills: Vec<&Skill> = all_prof_skills
        .into_iter()
        .filter(|s| match s.specialization {
            None => true,
            Some(spec_id) => Some(spec_id) == equipped_elite_spec_id,
        })
        .filter(|s| db.skill_palette_id(s.id) != 0)
        .collect();

    let (heal_name, utility_names, elite_name) = parse_skill_names_from_response(response);

    if let Some(name) = &heal_name {
        result.skills.heal = find_skill_by_name(name, &prof_skills, Some("Heal"), result);
    }

    // Validate utilities. GW2 has exactly 3 utility slots — cap so an LLM that
    // hallucinates 4+ utilities cannot inflate the build, and dedupe by skill id
    // so a single utility is never equipped in two slots simultaneously.
    let mut seen_utility_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for name in &utility_names {
        if result.skills.utilities.len() >= 3 {
            break;
        }
        let resolved = find_skill_by_name(name, &prof_skills, Some("Utility"), result);
        if let Some((id, ref skill_name)) = resolved {
            if !seen_utility_ids.insert(id) {
                result.warnings.push(format!(
                    "Skill '{}' already equipped — duplicate utility ignored",
                    skill_name
                ));
                continue;
            }
        }
        result.skills.utilities.push(resolved);
    }

    if let Some(name) = &elite_name {
        result.skills.elite = find_skill_by_name(name, &prof_skills, Some("Elite"), result);
    }
}

fn validate_pets(response: &GeminiBuildResponse, db: &GameDb, result: &mut ValidatedBuild) {
    let Some(names) = &response.pets else {
        return;
    };
    let mut ids = [None; 4];
    for (i, slot) in names.iter().enumerate() {
        let Some(name) = slot else {
            continue;
        };
        match db.pet_by_name(name) {
            Some(pet) => ids[i] = Some(pet.id),
            None => result.errors.push(ValidationReject {
                code: RejectCode::PetNotFound { name: name.clone() },
                detail: format!("Pet '{}' not found", name),
            }),
        }
    }
    result.pets = Some((ids[0], ids[1], ids[2], ids[3]));
}

/// Revenant heal/utilities/elite are a legend bundle, not a free mix.
/// `/v2/legends` plus the swap skill's `specialization` gate which stances
/// are legal; the template byte is `Legend.code`.
fn legend_ids_from_plate(response: &GeminiBuildResponse, db: &GameDb) -> Vec<String> {
    let mut ids = Vec::new();
    let mut consider = |s: &str| {
        for token in s.split(|c: char| !c.is_ascii_alphanumeric()) {
            if db.legends.contains_key(token) && !ids.iter().any(|id| id == token) {
                ids.push(token.to_string());
            }
        }
    };
    for line in &response.skills {
        consider(line);
    }
    for line in &response.changes_made {
        consider(line);
    }
    ids
}

fn fill_revenant_legends(response: &GeminiBuildResponse, result: &mut ValidatedBuild, db: &GameDb) {
    if db.legends.is_empty() {
        return;
    }
    let spec_ids: Vec<u32> = result.specializations.iter().map(|s| s.spec_id).collect();
    let explicit: Vec<String> = legend_ids_from_plate(response, db)
        .into_iter()
        .filter(|id| db.legend_available(id, &spec_ids))
        .collect();
    let mut ids = explicit;
    let mut inferred_from_heal = false;
    if ids.is_empty() {
        if let Some((heal_id, _)) = &result.skills.heal {
            if let Some(id) = db.legends.iter().find_map(|(id, l)| {
                (l.heal == *heal_id && db.legend_available(id, &spec_ids)).then(|| id.clone())
            }) {
                ids.push(id);
                inferred_from_heal = true;
            }
        }
    }
    pad_revenant_legends(&mut ids, db, &spec_ids);
    if ids.is_empty() {
        return;
    }
    let old_utility_ids: Vec<u32> = result
        .skills
        .utilities
        .iter()
        .filter_map(|u| u.as_ref().map(|p| p.0))
        .collect();
    let old_elite = result.skills.elite.as_ref().map(|e| e.0);
    apply_legend_package(result, db, &ids[0]);
    if inferred_from_heal {
        let new_utility_ids: Vec<u32> = result
            .skills
            .utilities
            .iter()
            .filter_map(|u| u.as_ref().map(|p| p.0))
            .collect();
        let utilities_changed = old_utility_ids != new_utility_ids;
        let elite_changed = old_elite != result.skills.elite.as_ref().map(|e| e.0);
        if utilities_changed || elite_changed {
            result.warnings.push(format!(
                "Revenant utilities/elite were replaced from legend {} inferred from heal",
                ids[0]
            ));
        }
    }
    result.legends = ids.clone();
    result.aquatic_legends = ids;
}

/// Write the active legend's heal / utilities / elite onto `build`.
/// Shared by plate fill and elite-swap retarget.
pub(crate) fn apply_legend_package(build: &mut ValidatedBuild, db: &GameDb, legend_id: &str) {
    let Some(legend) = db.legends.get(legend_id) else {
        return;
    };
    let name_of = |id: u32| {
        db.skills
            .get(&id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("Skill {id}"))
    };
    build.skills.heal = Some((legend.heal, name_of(legend.heal)));
    build.skills.utilities = legend
        .utilities
        .iter()
        .take(3)
        .map(|&id| Some((id, name_of(id))))
        .collect();
    build.skills.elite = Some((legend.elite, name_of(legend.elite)));
}

/// Pad `ids` to two available legends, sorted by template code then id
/// (same order `fill_revenant_legends` has always used).
pub(crate) fn pad_revenant_legends(ids: &mut Vec<String>, db: &GameDb, spec_ids: &[u32]) {
    let mut rest: Vec<(u8, String)> = db
        .legends
        .keys()
        .filter(|id| !ids.contains(id) && db.legend_available(id, spec_ids))
        .map(|id| (db.legend_template_code(id), id.clone()))
        .collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, id) in rest {
        if ids.len() >= 2 {
            break;
        }
        ids.push(id);
    }
}

fn validate_rune(response: &GeminiBuildResponse, db: &GameDb, result: &mut ValidatedBuild) {
    if response.rune.is_empty() {
        result
            .warnings
            .push("Rune field is empty — no rune selected".into());
        return;
    }

    let runes = db.all_runes();
    result.rune = find_item_by_name(&response.rune, &runes, "Rune", result);
}

fn validate_sigils(response: &GeminiBuildResponse, db: &GameDb, result: &mut ValidatedBuild) {
    let sigils_list = db.all_sigils();

    // Handle both old format (flat array) and new format (per-slot map).
    // When `sigils_map` is provided, positions are [set1_main, set1_off, set2_main, set2_off].
    let (sigil_names, positional) = if let Some(ref map) = response.sigils_map {
        (
            vec![
                map.get("set1_main").cloned().unwrap_or_default(),
                map.get("set1_off").cloned().unwrap_or_default(),
                map.get("set2_main").cloned().unwrap_or_default(),
                map.get("set2_off").cloned().unwrap_or_default(),
            ],
            true,
        )
    } else {
        (response.sigils.clone(), false)
    };

    // GW2 forbids duplicate sigils within a single weapon set, but the same
    // sigil is allowed across sets. When positions are known (sigils_map path)
    // we can enforce this per-set. Falling back to "no dedup" for the legacy
    // flat list — that path lacks slot identity so we can't reliably split.
    let mut resolved: Vec<Option<ValidatedItem>> = Vec::with_capacity(sigil_names.len());
    for name in &sigil_names {
        if name.is_empty() {
            resolved.push(None);
            continue;
        }
        resolved.push(find_item_by_name(name, &sigils_list, "Sigil", result));
    }

    if positional && resolved.len() == SIGIL_SLOT_COUNT {
        // Set 1 = indices 0,1 | Set 2 = indices 2,3
        for (set_label, a, b) in [("Set 1", 0usize, 1usize), ("Set 2", 2usize, 3usize)] {
            if let (Some(left), Some(right)) = (&resolved[a], &resolved[b]) {
                if left.id == right.id {
                    result.warnings.push(format!(
                        "{}: duplicate sigil '{}' — GW2 forbids two of the same sigil in one weapon set; \
                         dropping the off-hand slot",
                        set_label, left.name
                    ));
                    resolved[b] = None;
                }
            }
        }

        // Publish the seats, holes and all. Compacting the array away — which
        // is what publishing only its `Some` values did — destroyed slot
        // identity: a plate with a bare set-1 main hand, Force in the off-hand
        // and Air on set 2 became `[Force, Air]`, and
        // `calculate_validated_stats` — which reads the leading pair as the
        // worn set — then scored a **carried** sigil as if it were equipped and
        // put Force in the wrong hand.
        let mut seats: [Option<ValidatedItem>; SIGIL_SLOT_COUNT] = Default::default();
        for (seat, item) in seats.iter_mut().zip(resolved) {
            *seat = item;
        }
        result.set_sigil_seats(seats);
        return;
    }

    // Legacy flat list: no slot identity to preserve, so the dense list is all
    // there is. Seats stay unrecorded, and `active_sigil_ids` reads the leading
    // pair — which is what "the first two sigils listed" meant on this path
    // before and still means now.
    //
    // Written as an explicit `let … else` so the compacting iterator adaptor
    // this function must never reach for does not appear in it at all.
    for item in resolved {
        let Some(item) = item else { continue };
        result.sigils.push(item);
    }
}

fn validate_relic(response: &GeminiBuildResponse, db: &GameDb, result: &mut ValidatedBuild) {
    if response.relic.is_empty() {
        result
            .warnings
            .push("Relic field is empty — no relic selected".into());
        return;
    }

    let relics = db.all_relics();
    result.relic = find_item_by_name(&response.relic, &relics, "Relic", result);
}

fn validate_gear_prefix(response: &GeminiBuildResponse, db: &GameDb, result: &mut ValidatedBuild) {
    if response.stat_prefix.is_empty() {
        return;
    }

    // Shared deterministic policy: exact case-insensitive match wins (lower id
    // tiebreak), else shortest substring match (lower id tiebreak). "Berserker"
    // picks "Berserker's" over "Marauder's Berserker Combo" and survives
    // HashMap reorders across runs. The exact-vs-fuzzy classification below
    // only decides whether to warn, mirroring the pre-slot validator.
    let Some(itemstat) = db.itemstat_by_name(&response.stat_prefix) else {
        result.errors.push(ValidationReject {
            code: RejectCode::GearPrefixNotFound {
                name: response.stat_prefix.clone(),
            },
            detail: format!("Gear prefix '{}' not found", response.stat_prefix),
        });
        return;
    };

    let needle_lower = response.stat_prefix.to_lowercase();
    let needle_key = gw2_core::i18n::alnum_key(&response.stat_prefix);
    let is_literal_exact = itemstat.name.to_lowercase() == needle_lower;
    let is_alnum_exact =
        !needle_key.is_empty() && gw2_core::i18n::alnum_key(&itemstat.name) == needle_key;
    if !is_literal_exact && !is_alnum_exact {
        result.warnings.push(format!(
            "Gear prefix '{}' fuzzy-matched to '{}'",
            response.stat_prefix, itemstat.name
        ));
    } else if is_alnum_exact && !is_literal_exact {
        // Alphanumeric-only comparison treats "Knight's" and "Knights" (or any
        // punctuation/spacing variant) as the same prefix by design — rejecting
        // it would be a wrong "not found" for a name that is usually right.
        // Still warn so the player notices the display strings differ.
        result.warnings.push(format!(
            "Gear prefix '{}' matched '{}' (punctuation/spacing differs)",
            response.stat_prefix, itemstat.name
        ));
    }
    // Worn slots only: `validate_weapons` has already run, so the build knows
    // whether it is holding a Greatsword (no off-hand) or a sword/focus, and a
    // prefix on a hand that holds nothing is not a gear choice.
    result.fill_worn_gear_slots(PrefixRef {
        itemstat_id: itemstat.id,
        name: itemstat.name.clone(),
    });
}

/// Per-slot plate gear map (spec §12.3): Choya proposes, the referee disposes.
///
/// Runs AFTER [`validate_gear_prefix`] so every slot already holds the
/// weight-profile prefix — that fill is the fallback for entries that cannot
/// resolve. Each plate entry is applied with strict validation:
/// - the slot name must match a known `GearSlot` kebab name (case-insensitive),
///   else the entry is rejected with a warning;
/// - the prefix name resolves via `db.itemstat_by_name`; unknown names keep
///   the profile prefix and emit one warning per unique failing name.
fn validate_gear_slot_map(
    response: &GeminiBuildResponse,
    db: &GameDb,
    result: &mut ValidatedBuild,
) {
    let Some(map) = &response.gear_slots else {
        return;
    };
    if map.is_empty() {
        return;
    }

    // Deterministic application order: sort by (slot name, prefix name) so a
    // HashMap-ordered plate can never reorder warnings or overwrites (the
    // last write wins only when two keys normalize to the same slot).
    let mut entries: Vec<(&str, &str)> = map
        .iter()
        .map(|(slot, prefix)| (slot.as_str(), prefix.as_str()))
        .collect();
    entries.sort_unstable();

    let mut rejected_slots: Vec<&str> = Vec::new();
    let mut unworn_slots: Vec<&str> = Vec::new();
    let mut fallback_names: Vec<&str> = Vec::new();
    let mut fuzzy_matches: Vec<(&str, String)> = Vec::new();

    for (raw_slot, raw_prefix) in entries {
        let wanted = raw_slot.trim().to_lowercase();
        let Some(slot) = GearSlot::ALL
            .iter()
            .copied()
            .find(|s| s.kebab_name() == wanted)
        else {
            if !rejected_slots.contains(&raw_slot) {
                rejected_slots.push(raw_slot);
            }
            continue;
        };
        // A plate may name a hand the validated weapons leave empty — an
        // off-hand prefix beside a Greatsword, or set 2 on a build that has
        // only one set. Recording it would put a stat prefix on a slot the
        // character does not wear.
        if !result.wears(slot) {
            if !unworn_slots.contains(&raw_slot) {
                unworn_slots.push(raw_slot);
            }
            continue;
        }
        let Some(itemstat) = db.itemstat_by_name(raw_prefix) else {
            // Fallback per spec §9: the slot keeps the weight-profile prefix
            // filled by validate_gear_prefix; warn once per unique name.
            if !fallback_names.contains(&raw_prefix) {
                fallback_names.push(raw_prefix);
            }
            continue;
        };
        let needle_key = gw2_core::i18n::alnum_key(raw_prefix);
        let is_exact = itemstat.name.to_lowercase() == raw_prefix.to_lowercase()
            || (!needle_key.is_empty() && gw2_core::i18n::alnum_key(&itemstat.name) == needle_key);
        if !is_exact && !fuzzy_matches.iter().any(|(_, name)| *name == itemstat.name) {
            fuzzy_matches.push((raw_prefix, itemstat.name.clone()));
        }
        result.gear_slots.set(
            slot,
            PrefixRef {
                itemstat_id: itemstat.id,
                name: itemstat.name.clone(),
            },
        );
    }

    for slot in rejected_slots {
        result.warnings.push(format!(
            "Plate gear slot '{slot}' is not a known equipment slot; entry ignored"
        ));
    }
    for slot in unworn_slots {
        result.warnings.push(format!(
            "Plate gear slot '{slot}' holds no weapon in this build; entry ignored"
        ));
    }
    for name in fallback_names {
        result.warnings.push(format!(
            "Plate gear prefix '{name}' not found; affected slots keep the profile prefix"
        ));
    }
    for (needle, matched) in fuzzy_matches {
        result.warnings.push(format!(
            "Plate gear prefix '{needle}' fuzzy-matched to '{matched}'"
        ));
    }
}

/// Find a specialization by name (case-insensitive) within a profession's spec list.
/// Apply is `names_eq` only. A substring of another spec (e.g. "Fire" → Firebrand)
/// is a warning, not an apply.
fn find_spec_by_name<'a>(
    db: &'a GameDb,
    name: &str,
    prof_spec_ids: &[u32],
    result: &mut ValidatedBuild,
) -> Option<&'a Specialization> {
    for id in prof_spec_ids {
        if let Some(spec) = db.spec(*id) {
            if names_eq(&spec.name, name) {
                return Some(spec);
            }
        }
    }

    let needle = name.to_lowercase();
    if !needle.is_empty() {
        for id in prof_spec_ids {
            if let Some(spec) = db.spec(*id) {
                if spec.name.to_lowercase().contains(&needle) {
                    result.warnings.push(format!(
                        "Specialization '{}' is a substring of '{}' — not applied",
                        name, spec.name
                    ));
                    break;
                }
            }
        }
    }
    None
}

/// Find a trait by name (case-insensitive) within a spec's major traits.
fn find_trait_by_name<'a>(name: &str, major_traits: &[&'a GW2Trait]) -> Option<&'a GW2Trait> {
    let needle = name.to_lowercase();

    // Exact match
    if let Some(t) = major_traits.iter().find(|t| names_eq(&t.name, name)) {
        return Some(t);
    }

    // Contains match: only check if trait name contains the search needle.
    // Do NOT check the reverse (needle contains trait name) — that causes
    // "Empowered" to match input "Power" or "Swift" to match "Swift Empowerment".
    // Minimum needle length guard: short needles (< 5 chars) over-match on long
    // trait names. A 4-char LLM hallucination like "swif" would otherwise silently
    // match "Swift Retribution". Exact matches (above) are exempt from this guard.
    if needle.len() < 5 {
        return None;
    }
    major_traits
        .iter()
        .find(|t| t.name.to_lowercase().contains(&needle))
        .copied()
}

/// Fill empty Adept/Master/Grandmaster columns so a nearly-complete LLM plate is still legal.
/// Picks the lowest-order major in the missing tier (top row). Orders Adept → Master → Grandmaster.
fn complete_major_trait_columns(
    major_traits: &[&GW2Trait],
    resolved_ids: &mut Vec<u32>,
    resolved_names: &mut Vec<String>,
    used_tiers: &mut HashMap<u32, String>,
    spec_name: &str,
    warnings: &mut Vec<String>,
) {
    for tier in [1u32, 2, 3] {
        if used_tiers.contains_key(&tier) {
            continue;
        }
        let Some(t) = major_traits
            .iter()
            .filter(|t| t.tier == tier)
            .min_by_key(|t| (t.order, t.id))
        else {
            continue;
        };
        used_tiers.insert(tier, t.name.clone());
        resolved_ids.push(t.id);
        resolved_names.push(t.name.clone());
        warnings.push(format!(
            "Spec '{}': filled {} with '{}'",
            spec_name,
            tier_label(tier),
            t.name
        ));
    }
    let mut ordered_ids = Vec::with_capacity(resolved_ids.len());
    let mut ordered_names = Vec::with_capacity(resolved_names.len());
    for tier in [1u32, 2, 3] {
        if let Some(t) = major_traits
            .iter()
            .find(|t| t.tier == tier && resolved_ids.contains(&t.id))
        {
            ordered_ids.push(t.id);
            ordered_names.push(t.name.clone());
        }
    }
    *resolved_ids = ordered_ids;
    *resolved_names = ordered_names;
}

/// Find a skill by name (case-insensitive) and validate its slot type.
/// Resolve is `names_eq` only (exact ignore-ascii-case or alnum_key). A
/// substring of another skill does not apply; the slot stays empty.
fn find_skill_by_name(
    name: &str,
    prof_skills: &[&Skill],
    expected_slot: Option<&str>,
    result: &mut ValidatedBuild,
) -> Option<(u32, String)> {
    let found = prof_skills.iter().find(|s| names_eq(&s.name, name));

    if let Some(skill) = found {
        if skill_is_aquatic_only(skill) {
            result.errors.push(ValidationReject {
                code: RejectCode::SkillNotFound {
                    name: name.to_string(),
                },
                detail: format!(
                    "Skill '{}' is aquatic-only and cannot be used on a land bar",
                    name
                ),
            });
            return None;
        }
        if let Some(expected) = expected_slot {
            if let Some(ref slot) = skill.slot {
                if !slot.eq_ignore_ascii_case(expected) {
                    result.warnings.push(format!(
                        "Skill '{}' has slot '{}', expected '{}'",
                        skill.name, slot, expected
                    ));
                }
            }
        }
        Some((skill.id, skill.name.clone()))
    } else {
        result.errors.push(ValidationReject {
            code: RejectCode::SkillNotFound {
                name: name.to_string(),
            },
            detail: format!("Skill '{}' not found for this profession", name),
        });
        None
    }
}

fn names_eq(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    let ka = gw2_core::i18n::alnum_key(a);
    !ka.is_empty() && ka == gw2_core::i18n::alnum_key(b)
}

fn skill_is_aquatic_only(skill: &Skill) -> bool {
    skill
        .flags
        .iter()
        .any(|f| f.eq_ignore_ascii_case("Aquatic"))
        && !skill
            .flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("NoUnderwater"))
}

/// Find an item (rune/sigil/relic) by name (case-insensitive).
fn find_item_by_name(
    name: &str,
    items: &[&Item],
    item_type: &str,
    result: &mut ValidatedBuild,
) -> Option<ValidatedItem> {
    let needle = name.to_lowercase();

    // Exact match
    let found = items.iter().find(|i| names_eq(&i.name, name));

    if let Some(item) = found {
        return Some(ValidatedItem {
            id: item.id,
            name: item.name.clone(),
        });
    }

    // Same min-length gate as trait fuzzy (>=5). Short needles ("a", "sig")
    // must not steal a different item.
    if needle.len() < 5 {
        result.errors.push(ValidationReject {
            code: RejectCode::ItemNotFound {
                item_type: item_type.to_string(),
                name: name.to_string(),
            },
            detail: format!("{} '{}' not found in game data", item_type, name),
        });
        return None;
    }

    // Item name contains search string
    let found = items.iter().find(|i| {
        let item_lower = i.name.to_lowercase();
        item_lower.contains(&needle)
    });

    if let Some(item) = found {
        result.warnings.push(format!(
            "{} '{}' fuzzy-matched to '{}'",
            item_type, name, item.name
        ));
        return Some(ValidatedItem {
            id: item.id,
            name: item.name.clone(),
        });
    }

    // Last resort: search string contains item name.
    // Require minimum 8 chars AND item name must be >= 50% of search string length
    // to prevent spurious matches on short common words (e.g. "Fire" matching inside
    // a hallucinated long name).
    let found = items.iter().find(|i| {
        let item_lower = i.name.to_lowercase();
        item_lower.len() >= 8
            && needle.contains(&item_lower)
            && item_lower.len() * 2 >= needle.len()
    });

    if let Some(item) = found {
        result.warnings.push(format!(
            "{} '{}' fuzzy-matched to '{}'",
            item_type, name, item.name
        ));
        return Some(ValidatedItem {
            id: item.id,
            name: item.name.clone(),
        });
    }

    // Try stripping "Superior Rune/Sigil of (the) " prefix from the search name
    let stripped = name
        .strip_prefix("Superior Rune of the ")
        .or_else(|| name.strip_prefix("Superior Rune of "))
        .or_else(|| name.strip_prefix("Superior Sigil of the "))
        .or_else(|| name.strip_prefix("Superior Sigil of "))
        .or_else(|| name.strip_prefix("Relic of the "))
        .or_else(|| name.strip_prefix("Relic of "));

    if let Some(short) = stripped {
        let short_lower = short.to_lowercase();
        if short_lower.len() >= 5 {
            let found = items
                .iter()
                .find(|i| i.name.to_lowercase().contains(&short_lower));
            if let Some(item) = found {
                result.warnings.push(format!(
                    "{} '{}' fuzzy-matched to '{}'",
                    item_type, name, item.name
                ));
                return Some(ValidatedItem {
                    id: item.id,
                    name: item.name.clone(),
                });
            }
        }
    }

    result.errors.push(ValidationReject {
        code: RejectCode::ItemNotFound {
            item_type: item_type.to_string(),
            name: name.to_string(),
        },
        detail: format!("{} '{}' not found in game data", item_type, name),
    });
    None
}

/// Find a weapon type in the profession's weapon list (case-insensitive).
/// Returns the canonical key + WeaponInfo so callers can store the canonical
/// name instead of the LLM-supplied casing. Otherwise a downstream
/// `prof.weapons.get(canonical_key)` (case-sensitive) misses and skips the
/// elite-spec weapon gate.
fn find_weapon<'a>(
    name: &str,
    prof: &'a gw2_api::models::Profession,
) -> Option<(&'a String, &'a gw2_api::models::WeaponInfo)> {
    // Profession/skill: Shortbow. Items: ShortBow. Models: "Short Bow".
    // Items also use Harpoon for profession Spear.
    let needle = gw2_core::i18n::weapon_type_key(name);
    if needle.is_empty() {
        return None;
    }
    prof.weapons
        .iter()
        .find(|(k, _)| gw2_core::i18n::weapon_type_key(k) == needle)
}

/// A single weapon set as (main-hand, off-hand) names.
type WeaponSlots = (Option<String>, Option<String>);

/// Pack a flat list of weapon types into two sets, by hand.
///
/// A plate built from published gear ids carries one bare weapon type per
/// equipped weapon — `["Scepter", "Warhorn"]` is *one* set, not two main
/// hands. Slotting each name into its own set put off-hand-only weapons in a
/// main hand, where the wiki table correctly says they cannot go, and threw
/// out legal published builds.
///
/// Hands come from the wiki table for this profession, so Ranger Dagger
/// (main-hand capable) and Elementalist Focus (off-hand only) land in
/// different slots. Weapons the table does not know fall back to the
/// two-handed type list and otherwise to a main hand.
fn pack_weapon_stream(names: &[String], profession: &str) -> (WeaponSlots, WeaponSlots) {
    let mut sets: [WeaponSlots; 2] = [(None, None), (None, None)];
    // A two-hander fills a set even though only the main slot holds a name.
    let mut filled = [false, false];
    let mut idx = 0usize;

    for name in names {
        let known = weapon_hands::known_weapon(profession, name);
        let two_hand = if known {
            !matches!(
                weapon_hands::access(profession, name, Hand::TwoHand),
                WeaponAccess::None
            )
        } else {
            crate::weapon_budget::is_two_handed(name, None)
        };
        let off_only = !two_hand
            && known
            && matches!(
                weapon_hands::access(profession, name, Hand::Main),
                WeaponAccess::None
            );

        while idx < sets.len() {
            let full = filled[idx];
            let (main, off) = &mut sets[idx];
            if two_hand {
                if main.is_none() && off.is_none() {
                    *main = Some(name.clone());
                    filled[idx] = true;
                    idx += 1;
                    break;
                }
            } else if !full {
                if !off_only && main.is_none() {
                    *main = Some(name.clone());
                    break;
                }
                if off.is_none() {
                    *off = Some(name.clone());
                    break;
                }
            }
            idx += 1;
        }
    }

    let [set1, set2] = sets;
    (set1, set2)
}

/// Parse weapon sets from GeminiBuildResponse.
/// Handles both old format ("Set 1: Axe / Axe") and the raw fields.
fn parse_weapon_sets_from_response(
    response: &GeminiBuildResponse,
    profession: &str,
) -> (WeaponSlots, WeaponSlots) {
    // No labels and no "/" anywhere: a flat weapon-type stream, not one set
    // per entry.
    if !response.weapons.is_empty()
        && response
            .weapons
            .iter()
            .all(|w| !w.contains(':') && !w.contains('/'))
    {
        return pack_weapon_stream(&response.weapons, profession);
    }

    let mut set1 = (None, None);
    let mut set2 = (None, None);

    for w in &response.weapons {
        let (label, rest) = if let Some(idx) = w.find(':') {
            (w[..idx].trim(), w[idx + 1..].trim())
        } else {
            ("", w.as_str())
        };

        let parts: Vec<&str> = rest.split('/').map(|s| s.trim()).collect();
        let main = parts
            .first()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let off = parts
            .get(1)
            .filter(|s| !s.is_empty() && *s != &"null" && *s != &"None")
            .map(|s| s.to_string());

        if label.contains('1') || set1.0.is_none() && label.is_empty() {
            set1 = (main, off);
        } else {
            set2 = (main, off);
        }
    }

    (set1, set2)
}

/// Parse skill names from GeminiBuildResponse.
/// Handles both old format ("Heal: Mending", "Utils: Foo, Bar, Baz") and direct fields.
///
/// Label prefix matching is case-insensitive — the LLM occasionally lowercases
/// labels ("heal:") and a case-sensitive strip silently dropped those skills.
fn parse_skill_names_from_response(
    response: &GeminiBuildResponse,
) -> (Option<String>, Vec<String>, Option<String>) {
    fn strip_label_ci<'a>(s: &'a str, label: &str) -> Option<&'a str> {
        // UTF-8 safe via `str::get` — returns None on non-char-boundary indices.
        let head = s.get(..label.len())?;
        if head.eq_ignore_ascii_case(label) {
            Some(&s[label.len()..])
        } else {
            None
        }
    }

    let mut heal = None;
    let mut utilities = Vec::new();
    let mut elite = None;

    for skill_line in &response.skills {
        if let Some(rest) = strip_label_ci(skill_line, "Heal: ") {
            heal = Some(rest.trim().to_string());
        } else if let Some(rest) = strip_label_ci(skill_line, "Utils: ") {
            utilities.extend(rest.split(',').map(|s| s.trim().to_string()));
        } else if let Some(rest) = strip_label_ci(skill_line, "Utility: ") {
            let name = rest.trim();
            if !name.is_empty() {
                utilities.push(name.to_string());
            }
        } else if let Some(rest) = strip_label_ci(skill_line, "Elite: ") {
            elite = Some(rest.trim().to_string());
        }
    }

    (heal, utilities, elite)
}

/// Parse structured changes from the response.
fn parse_changes(response: &GeminiBuildResponse) -> Vec<ChangeEntry> {
    // First try structured changes from the new format
    if let Some(ref changes) = response.changes_structured {
        return changes
            .iter()
            .filter_map(|c| {
                Some(ChangeEntry {
                    slot: c.get("slot")?.as_str()?.to_string(),
                    from: c
                        .get("from")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    to: c
                        .get("to")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    reason: c
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect();
    }

    // Fallback: convert flat string changes to ChangeEntry
    response
        .changes_made
        .iter()
        .map(|s| ChangeEntry {
            slot: String::new(),
            from: String::new(),
            to: String::new(),
            reason: s.clone(),
        })
        .collect()
}

/// Human-readable tier label.
fn tier_label(tier: u32) -> &'static str {
    match tier {
        1 => "Adept",
        2 => "Master",
        3 => "Grandmaster",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    // Test fixtures are built field-by-field for readability.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    // C17: occupancy stays honest

    /// A prefix on a hand that holds nothing is not a gear choice.
    ///
    /// The build-wide fill wrote all sixteen slots unconditionally, so a
    /// two-hander carried a stat prefix on its non-existent off-hand and both
    /// of weapon set 2 — which then churned `gear_identity` (two builds with
    /// identical combat stats dedup as different candidates, each spending beam
    /// budget), showed a prefix on an empty off-hand in the gear sheet, and
    /// shaped the plate the Improve gate compares against.
    #[test]
    fn fill_gear_slots_skips_empty() {
        let prefix = PrefixRef {
            itemstat_id: 161,
            name: "Berserker's".into(),
        };
        let armour_and_trinkets = [
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
        ];

        // A greatsword: main hand only, no off-hand, no second set.
        let mut two_hander = ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: Some("Greatsword".into()),
                    off_hand: None,
                },
                set2: ValidatedWeaponSet::default(),
            },
            ..ValidatedBuild::default()
        };
        two_hander.fill_worn_gear_slots(prefix.clone());

        for slot in armour_and_trinkets {
            assert!(
                two_hander.prefix_for(slot).is_some(),
                "{slot:?} is always worn and must carry the prefix"
            );
        }
        assert!(two_hander.prefix_for(GearSlot::WeaponSet1Main).is_some());
        for empty in [
            GearSlot::WeaponSet1Off,
            GearSlot::WeaponSet2Main,
            GearSlot::WeaponSet2Off,
        ] {
            assert!(
                two_hander.prefix_for(empty).is_none(),
                "{empty:?} holds no weapon but was given a stat prefix"
            );
        }
        // Counted, not asserted from a constant: 12 armour/trinket + 1 hand.
        let populated = GearSlot::ALL
            .iter()
            .filter(|slot| two_hander.prefix_for(**slot).is_some())
            .count();
        assert_eq!(populated, 13);

        // Dual wield on both sets: every hand is worn, so every slot fills.
        let mut dual = ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: Some("Sword".into()),
                    off_hand: Some("Focus".into()),
                },
                set2: ValidatedWeaponSet {
                    main_hand: Some("Scepter".into()),
                    off_hand: Some("Torch".into()),
                },
            },
            ..ValidatedBuild::default()
        };
        dual.fill_worn_gear_slots(prefix.clone());
        assert_eq!(
            GearSlot::ALL
                .iter()
                .filter(|slot| dual.prefix_for(**slot).is_some())
                .count(),
            16,
            "a fully armed build must still fill every slot"
        );

        // A blank string is an empty hand, not a weapon called "".
        let mut blank = ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: Some("Sword".into()),
                    off_hand: Some("   ".into()),
                },
                set2: ValidatedWeaponSet::default(),
            },
            ..ValidatedBuild::default()
        };
        blank.fill_worn_gear_slots(prefix.clone());
        assert!(blank.prefix_for(GearSlot::WeaponSet1Off).is_none());

        // The search operator's filler follows the same rule, and clears a dead
        // cell that an earlier pass had filled — otherwise a build that started
        // life fully filled would keep its ghost off-hand prefix forever.
        let mut stale = ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: Some("Greatsword".into()),
                    off_hand: None,
                },
                set2: ValidatedWeaponSet::default(),
            },
            ..ValidatedBuild::default()
        };
        stale.fill_gear_slots(prefix.clone()); // the unconditional legacy fill
        assert!(stale.prefix_for(GearSlot::WeaponSet1Off).is_some());
        let changed = stale.fill_unlocked_gear_slots(prefix.clone(), &HashMap::new());
        assert!(changed, "clearing a dead cell is a change");
        assert!(stale.prefix_for(GearSlot::WeaponSet1Off).is_none());

        // Locks still win over occupancy: a locked slot is never touched.
        let mut locked = ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: Some("Greatsword".into()),
                    off_hand: None,
                },
                set2: ValidatedWeaponSet::default(),
            },
            ..ValidatedBuild::default()
        };
        locked.gear_slots.set(
            GearSlot::WeaponSet1Off,
            PrefixRef {
                itemstat_id: 999,
                name: "Locked".into(),
            },
        );
        let mut gear_locks = HashMap::new();
        gear_locks.insert(GearSlot::WeaponSet1Off, 999u32);
        locked.fill_unlocked_gear_slots(prefix, &gear_locks);
        assert_eq!(
            locked
                .prefix_for(GearSlot::WeaponSet1Off)
                .map(|p| p.itemstat_id),
            Some(999),
            "a locked slot was cleared by the occupancy rule"
        );
    }

    /// The positional sigil map must keep its holes. `[None, Force, Air, None]`
    /// compacted to `[Force, Air]`, and the stat sheet — which reads the leading
    /// pair as the worn set — then scored a carried set-2 sigil as equipped.
    #[test]
    fn sigil_seats_keep_their_holes() {
        let db = sigil_db();
        let mut response = GeminiBuildResponse::default();
        response.sigils_map = Some(
            [
                (
                    "set1_off".to_string(),
                    "Superior Sigil of Force".to_string(),
                ),
                ("set2_main".to_string(), "Superior Sigil of Air".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        let mut result = ValidatedBuild::default();
        validate_sigils(&response, &db, &mut result);

        // Dense list still carries both, in seat order, for display and saves.
        assert_eq!(
            result.sigils.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![10, 20]
        );
        // But the worn set is only the off-hand sigil: seat 0 is empty.
        assert_eq!(result.sigil_seats.get(SigilSlot::Set1Main), None);
        assert_eq!(result.sigil_seats.get(SigilSlot::Set1Off), Some(10));
        assert_eq!(result.sigil_seats.get(SigilSlot::Set2Main), Some(20));
        assert_eq!(
            result.active_sigil_ids(),
            vec![10],
            "a weapon-set-2 sigil was counted as worn"
        );

        // A build that never recorded seats keeps the old dense reading, which
        // is correct for it: the synergy seeder and the beam build hole-free
        // lists where position already is seat order.
        let mut dense = ValidatedBuild::default();
        dense.sigils = vec![
            ValidatedItem {
                id: 10,
                name: "Superior Sigil of Force".into(),
            },
            ValidatedItem {
                id: 20,
                name: "Superior Sigil of Air".into(),
            },
            ValidatedItem {
                id: 30,
                name: "Superior Sigil of Bloodlust".into(),
            },
        ];
        assert_eq!(dense.active_sigil_ids(), vec![10, 20]);

        // Sprint 2: both sets, seats and dense alike.
        assert_eq!(result.sigil_ids_by_set(), [vec![10], vec![20]]);
        assert_eq!(dense.sigil_ids_by_set(), [vec![10, 20], vec![30]]);
    }

    fn sigil_db() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        for (id, name) in [
            (10u32, "Superior Sigil of Force"),
            (20, "Superior Sigil of Air"),
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
            db.sigils.push(id);
        }
        db
    }

    /// The modal prefix names the build, not whatever landed on the helm.
    #[test]
    fn primary_prefix_is_modal_not_first_slot() {
        let mut build = ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: Some("Staff".into()),
                    off_hand: None,
                },
                set2: ValidatedWeaponSet::default(),
            },
            ..ValidatedBuild::default()
        };
        build.fill_worn_gear_slots(PrefixRef {
            itemstat_id: 161,
            name: "Berserker's".into(),
        });
        // One nudged piece, and it is the first slot in canonical order.
        build.gear_slots.set(
            GearSlot::Helm,
            PrefixRef {
                itemstat_id: 1099,
                name: "Cavalier's".into(),
            },
        );

        assert_eq!(
            build.primary_prefix().map(|p| p.itemstat_id),
            Some(161),
            "one helm swap renamed the whole build"
        );

        // Ties resolve by canonical slot order, deterministically.
        let mut split = ValidatedBuild::default();
        for slot in [GearSlot::Helm, GearSlot::Shoulders] {
            split.gear_slots.set(
                slot,
                PrefixRef {
                    itemstat_id: 1,
                    name: "A".into(),
                },
            );
        }
        for slot in [GearSlot::Coat, GearSlot::Gloves] {
            split.gear_slots.set(
                slot,
                PrefixRef {
                    itemstat_id: 2,
                    name: "B".into(),
                },
            );
        }
        assert_eq!(split.primary_prefix().map(|p| p.itemstat_id), Some(1));
    }

    #[test]
    fn test_parse_weapon_sets_from_response() {
        let mut response = GeminiBuildResponse::default();
        response.weapons = vec!["Set 1: Axe / Axe".into(), "Set 2: Greatsword".into()];
        let (set1, set2) = parse_weapon_sets_from_response(&response, "Warrior");
        assert_eq!(set1.0.as_deref(), Some("Axe"));
        assert_eq!(set1.1.as_deref(), Some("Axe"));
        assert_eq!(set2.0.as_deref(), Some("Greatsword"));
        assert_eq!(set2.1, None);
    }

    #[test]
    fn one_land_set_gets_a_second_twohander() {
        use gw2_api::models::{Profession, WeaponInfo};
        let mut weapons = HashMap::new();
        let info = |spec: Option<u32>, flags: &[&str]| WeaponInfo {
            specialization: spec,
            flags: flags.iter().map(|s| (*s).to_string()).collect(),
            skills: vec![],
        };
        weapons.insert("Sword".into(), info(None, &["Mainhand", "Offhand"]));
        weapons.insert("Axe".into(), info(None, &["Offhand"]));
        weapons.insert("Hammer".into(), info(None, &["TwoHand"]));
        weapons.insert("Greatsword".into(), info(Some(69), &["TwoHand"]));
        let prof = Profession {
            id: "Revenant".into(),
            name: "Revenant".into(),
            code: Some(9),
            specializations: vec![52],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        let set1 = ValidatedWeaponSet {
            main_hand: Some("Sword".into()),
            off_hand: Some("Axe".into()),
        };
        let set2 = complementary_weapon_set(&set1, Some(&prof), &[]).expect("second set");
        assert_eq!(set2.main_hand.as_deref(), Some("Hammer"));
        assert_eq!(set2.off_hand, None);
        assert!(
            complementary_weapon_set(&set1, Some(&prof), &[52])
                .is_some_and(|s| s.main_hand.as_deref() == Some("Hammer")),
            "Herald must not be given Vindicator Greatsword"
        );
    }

    #[test]
    fn test_parse_skill_names_from_response() {
        let mut response = GeminiBuildResponse::default();
        response.skills = vec![
            "Heal: Mending".into(),
            "Utils: Signet of Fury, Banner of Strength, Bull's Charge".into(),
            "Elite: Signet of Rage".into(),
        ];
        let (heal, utils, elite) = parse_skill_names_from_response(&response);
        assert_eq!(heal.as_deref(), Some("Mending"));
        assert_eq!(utils.len(), 3);
        assert_eq!(utils[0], "Signet of Fury");
        assert_eq!(elite.as_deref(), Some("Signet of Rage"));
    }

    #[test]
    fn test_parse_skill_names_case_insensitive() {
        // Regression: case-sensitive strip_prefix dropped lowercase labels.
        let mut response = GeminiBuildResponse::default();
        response.skills = vec![
            "heal: Mending".into(),
            "UTILS: Signet of Fury, Banner of Strength".into(),
            "Elite: Signet of Rage".into(),
        ];
        let (heal, utils, elite) = parse_skill_names_from_response(&response);
        assert_eq!(heal.as_deref(), Some("Mending"));
        assert_eq!(utils.len(), 2);
        assert_eq!(elite.as_deref(), Some("Signet of Rage"));
    }

    #[test]
    fn test_parse_changes_structured() {
        let mut response = GeminiBuildResponse::default();
        response.changes_structured = Some(vec![serde_json::json!({
            "slot": "Adept", "from": "Trait A", "to": "Trait B", "reason": "Better synergy"
        })]);
        let changes = parse_changes(&response);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].slot, "Adept");
        assert_eq!(changes[0].to, "Trait B");
    }

    #[test]
    fn test_parse_changes_flat_fallback() {
        let mut response = GeminiBuildResponse::default();
        response.changes_made = vec!["Switched to Axe/Axe for burst".into()];
        let changes = parse_changes(&response);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].reason, "Switched to Axe/Axe for burst");
    }

    #[test]
    fn test_tier_label() {
        assert_eq!(tier_label(1), "Adept");
        assert_eq!(tier_label(2), "Master");
        assert_eq!(tier_label(3), "Grandmaster");
    }

    // find_trait_by_name() length guard

    fn make_trait(id: u32, name: &str) -> GW2Trait {
        GW2Trait {
            id,
            name: name.into(),
            icon: None,
            description: None,
            specialization: 0,
            tier: 1,
            order: 0,
            slot: "Major".into(),
            facts: vec![],
            traited_facts: vec![],
            skills: vec![],
        }
    }

    #[test]
    fn test_find_trait_short_needle_no_contains_match() {
        // needle "swif" (4 chars) is a substring of "Swift Retribution".
        // Exact match fails ("swift retribution" != "swif").
        // Length guard (< 5) must block the contains fallback → None.
        let t = make_trait(1, "Swift Retribution");
        let traits = vec![&t];
        let result = find_trait_by_name("swif", &traits);
        assert!(
            result.is_none(),
            "4-char needle must not match via contains fallback"
        );
    }

    #[test]
    fn test_find_trait_needle_ge5_contains_match() {
        // needle "valor" (5 chars) is a substring of "Valorous Recovery".
        // Exact match fails ("valorous recovery" != "valor").
        // Length guard passes (5 >= 5) → contains fires → Some.
        let t = make_trait(2, "Valorous Recovery");
        let traits = vec![&t];
        let result = find_trait_by_name("valor", &traits);
        assert!(
            result.is_some(),
            "5-char needle must match via contains fallback"
        );
        assert_eq!(result.unwrap().id, 2);
    }

    fn arcane_ele_db() -> GameDb {
        let mut db = empty_db_with_itemstats(vec![]);
        db.professions.insert(
            "Elementalist".into(),
            gw2_api::models::Profession {
                id: "Elementalist".into(),
                name: "Elementalist".into(),
                code: None,
                specializations: vec![41],
                weapons: std::collections::HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        db.specializations.insert(
            41,
            Specialization {
                id: 41,
                name: "Arcane".into(),
                profession: "Elementalist".into(),
                elite: false,
                minor_traits: vec![],
                major_traits: vec![1, 2, 3],
                weapon_trait: None,
                icon: None,
                background: None,
                profession_icon: None,
                profession_icon_big: None,
            },
        );
        let mut t1 = make_trait(1, "Arcane Precision");
        t1.tier = 1;
        t1.specialization = 41;
        let mut t2 = make_trait(2, "Arcane Resurrection");
        t2.tier = 2;
        t2.specialization = 41;
        let mut t3 = make_trait(3, "Evasive Arcana");
        t3.tier = 3;
        t3.specialization = 41;
        db.traits.insert(1, t1);
        db.traits.insert(2, t2);
        db.traits.insert(3, t3);
        db.traits_by_spec.insert(41, vec![1, 2, 3]);
        db
    }

    #[test]
    fn validate_gemini_build_fills_missing_arcane_trait_column() {
        // Choya named Arcane but only two traits (the live "got 2" reject).
        // Fill Adept from game data so the plate is legal.
        let db = arcane_ele_db();
        let response = GeminiBuildResponse {
            specializations: vec![(
                "Arcane".into(),
                vec!["Arcane Resurrection".into(), "Evasive Arcana".into()],
            )],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Elementalist");
        assert_eq!(result.specializations[0].trait_ids, vec![1, 2, 3]);
        assert_eq!(
            result.specializations[0].trait_names,
            vec![
                "Arcane Precision".to_string(),
                "Arcane Resurrection".to_string(),
                "Evasive Arcana".to_string()
            ]
        );
        assert!(
            !result
                .errors
                .iter()
                .any(|e| matches!(e.code, RejectCode::IncompleteSpecTraits { .. })),
            "filled plate must not reject traits: {:?}",
            result.errors
        );
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("Arcane Precision")));
    }

    /// The live failure: Choya names three traits and ONE of them is wrong
    /// (a minor trait, another spec's trait, a hallucination). Two good names
    /// prove it knew this spec, so the bad column must fill from game data
    /// rather than the whole build being discarded — a spec stuck at 2 traits
    /// fails `plate_is_servable`, so the player gets prose and no build at all.
    #[test]
    fn validate_gemini_build_fills_around_one_bad_trait_name() {
        let db = arcane_ele_db();
        let response = GeminiBuildResponse {
            specializations: vec![(
                "Arcane".into(),
                vec![
                    "Arcane Resurrection".into(),
                    "NotARealTrait".into(),
                    "Evasive Arcana".into(),
                ],
            )],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Elementalist");
        assert!(
            result.warnings.iter().any(|w| w.contains("NotARealTrait")),
            "the bad name must still be reported: {:?}",
            result.warnings
        );
        assert_eq!(
            result.specializations[0].trait_ids,
            vec![1, 2, 3],
            "the unresolved column fills from game data"
        );
        assert!(
            !result
                .errors
                .iter()
                .any(|e| matches!(e.code, RejectCode::IncompleteSpecTraits { .. })),
            "one bad name must not sink the build: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_gemini_build_does_not_fill_after_garbage_trait_name() {
        let db = arcane_ele_db();
        let response = GeminiBuildResponse {
            specializations: vec![("Arcane".into(), vec!["DefinitelyNotATrait".into()])],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Elementalist");
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("DefinitelyNotATrait")),
            "garbage name must warn, got {:?}",
            result.warnings
        );
        assert!(
            !result.warnings.iter().any(|w| w.contains("filled")),
            "garbage must not autofill a legal column: {:?}",
            result.warnings
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| matches!(e.code, RejectCode::IncompleteSpecTraits { .. })),
            "garbage plate must stay incomplete: {:?}",
            result.errors
        );
        assert_ne!(
            result.specializations[0].trait_ids,
            vec![1, 2, 3],
            "must not complete into Arcane Precision / Resurrection / Evasive Arcana"
        );
    }

    /// Three distinct Elementalist specs: two core (Arcane, Fire) and one
    /// elite (Tempest), each with a full major-trait column so empty trait
    /// lists autofill legally.
    fn three_spec_ele_db() -> GameDb {
        let mut db = empty_db_with_itemstats(vec![]);
        db.professions.insert(
            "Elementalist".into(),
            gw2_api::models::Profession {
                id: "Elementalist".into(),
                name: "Elementalist".into(),
                code: None,
                specializations: vec![41, 42, 43],
                weapons: std::collections::HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        for (spec_id, name, elite, trait_ids) in [
            (41, "Arcane", false, [1, 2, 3]),
            (42, "Fire", false, [4, 5, 6]),
            (43, "Tempest", true, [7, 8, 9]),
        ] {
            db.specializations.insert(
                spec_id,
                Specialization {
                    id: spec_id,
                    name: name.into(),
                    profession: "Elementalist".into(),
                    elite,
                    minor_traits: vec![],
                    major_traits: trait_ids.to_vec(),
                    weapon_trait: None,
                    icon: None,
                    background: None,
                    profession_icon: None,
                    profession_icon_big: None,
                },
            );
            for (i, trait_id) in trait_ids.iter().enumerate() {
                let mut t = make_trait(*trait_id, &format!("{} Trait {}", name, i + 1));
                t.tier = (i + 1) as u32;
                t.specialization = spec_id;
                db.traits.insert(*trait_id, t);
            }
            db.traits_by_spec.insert(spec_id, trait_ids.to_vec());
        }
        db
    }

    #[test]
    fn duplicate_spec_id_rejected() {
        let db = three_spec_ele_db();
        let response = GeminiBuildResponse {
            specializations: vec![("Arcane".into(), vec![]), ("Arcane".into(), vec![])],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Elementalist");
        assert!(
            result
                .errors
                .iter()
                .any(|e| matches!(e.code, RejectCode::DuplicateSpec { .. })),
            "duplicate spec must reject: {:?}",
            result.errors
        );
        assert_eq!(
            result.specializations.len(),
            1,
            "duplicate must not produce a second spec row: {:?}",
            result.specializations
        );
    }

    #[test]
    fn duplicate_elite_spec_id_errors_once() {
        // Two identical elite specs: rejected exactly once (DuplicateSpec wins
        // because the dup check runs before the elite check), never both.
        let db = three_spec_ele_db();
        let response = GeminiBuildResponse {
            specializations: vec![("Tempest".into(), vec![]), ("Tempest".into(), vec![])],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Elementalist");
        let spec_rejections = result
            .errors
            .iter()
            .filter(|e| {
                matches!(
                    e.code,
                    RejectCode::DuplicateSpec { .. } | RejectCode::MultipleEliteSpecs { .. }
                )
            })
            .count();
        assert_eq!(
            spec_rejections, 1,
            "duplicate elite must error exactly once: {:?}",
            result.errors
        );
        assert_eq!(result.specializations.len(), 1);
    }

    #[test]
    fn three_distinct_specs_still_valid() {
        let db = three_spec_ele_db();
        let response = GeminiBuildResponse {
            specializations: vec![
                ("Arcane".into(), vec![]),
                ("Fire".into(), vec![]),
                ("Tempest".into(), vec![]),
            ],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Elementalist");
        assert!(
            !result.errors.iter().any(|e| matches!(
                e.code,
                RejectCode::DuplicateSpec { .. }
                    | RejectCode::MultipleEliteSpecs { .. }
                    | RejectCode::WrongSpecCount { .. }
            )),
            "three distinct specs must not reject: {:?}",
            result.errors
        );
        assert_eq!(result.specializations.len(), 3);
    }

    // validate_gear_prefix() determinism + tie-break

    fn empty_db_with_itemstats(stats: Vec<(u32, &str)>) -> GameDb {
        let mut itemstats = std::collections::HashMap::new();
        for (id, name) in stats {
            itemstats.insert(
                id,
                gw2_api::models::itemstats::ItemStat {
                    id,
                    name: name.into(),
                    attributes: vec![],
                },
            );
        }
        GameDb {
            items: std::collections::HashMap::new(),
            itemstats,
            skills: std::collections::HashMap::new(),
            traits: std::collections::HashMap::new(),
            specializations: std::collections::HashMap::new(),
            professions: std::collections::HashMap::new(),
            legends: std::collections::HashMap::new(),
            pvp_amulets: std::collections::HashMap::new(),
            pets: std::collections::HashMap::new(),
            skills_by_profession: std::collections::HashMap::new(),
            traits_by_spec: std::collections::HashMap::new(),
            items_by_type: std::collections::HashMap::new(),
            runes: vec![],
            sigils: vec![],
            relics: vec![],
            skill_to_palette: std::collections::HashMap::new(),
            palette_to_skill: std::collections::HashMap::new(),
            traits_by_condition: std::collections::HashMap::new(),
            skills_by_condition: std::collections::HashMap::new(),
            traits_by_buff: std::collections::HashMap::new(),
            skills_by_buff: std::collections::HashMap::new(),
            localized: None,
        }
    }

    /// A build holding a one-handed weapon in every hand of both sets, so all
    /// sixteen slots are worn. `validate_weapons` runs before the gear
    /// validators in `validate_gemini_build`, so this is the state they see.
    fn dual_wielding_both_sets() -> ValidatedBuild {
        let hand = |name: &str| Some(name.to_string());
        ValidatedBuild {
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: hand("Sword"),
                    off_hand: hand("Focus"),
                },
                set2: ValidatedWeaponSet {
                    main_hand: hand("Scepter"),
                    off_hand: hand("Torch"),
                },
            },
            ..ValidatedBuild::default()
        }
    }

    fn run_validate_gear_prefix(prefix: &str, db: &GameDb) -> ValidatedBuild {
        let mut response = GeminiBuildResponse::default();
        response.stat_prefix = prefix.into();
        let mut result = dual_wielding_both_sets();
        validate_gear_prefix(&response, db, &mut result);
        result
    }

    #[test]
    fn test_validate_gear_prefix_exact_match_beats_substring() {
        // Two candidates contain "berserker"; exact match must win regardless of insertion order.
        let db = empty_db_with_itemstats(vec![
            (100, "Marauder's Berserker Combo"),
            (101, "Berserker's"),
        ]);
        let result = run_validate_gear_prefix("Berserker's", &db);
        let p = result.primary_prefix().expect("should match");
        assert_eq!(p.itemstat_id, 101);
        assert_eq!(p.name, "Berserker's");
        assert!(
            result.warnings.is_empty(),
            "exact match must not emit fuzzy warning"
        );
    }

    #[test]
    fn alnum_exact_warns_on_display_mismatch() {
        // "Knight's" and "Knights" share the same alphanumeric key (alnum_key
        // strips punctuation/spacing), so itemstat_by_name resolves this
        // deterministically rather than falling through to "not found". The
        // display strings still differ, so validate_gear_prefix must warn even
        // though this is an alnum-exact match, not a substring fuzzy match.
        let db = empty_db_with_itemstats(vec![(500, "Knight's")]);
        let result = run_validate_gear_prefix("Knights", &db);
        let p = result.primary_prefix().expect("should match via alnum key");
        assert_eq!(p.itemstat_id, 500);
        assert_eq!(p.name, "Knight's");
        assert_eq!(
            result.warnings.len(),
            1,
            "alnum-exact match with a differing display string must warn"
        );
        assert!(result.warnings[0].contains("Knights"));
        assert!(result.warnings[0].contains("Knight's"));
    }

    #[test]
    fn test_validate_gear_prefix_fuzzy_prefers_shortest_name() {
        // "Viper" substring matches multiple. Tie-break: shortest name wins.
        // This is the determinism fix: HashMap iteration order would otherwise
        // make this test flaky depending on hasher seed.
        let db = empty_db_with_itemstats(vec![
            (200, "Carrion-Viper Hybrid Marauder Combo"),
            (201, "Viper's"),
            (202, "Trailblazer's Viper Combo"),
        ]);
        for _ in 0..10 {
            let result = run_validate_gear_prefix("Viper", &db);
            let p = result.primary_prefix().expect("should fuzzy match");
            assert_eq!(p.itemstat_id, 201, "shortest name must always win");
            assert_eq!(p.name, "Viper's");
        }
        let result = run_validate_gear_prefix("Viper", &db);
        assert_eq!(result.warnings.len(), 1, "fuzzy match must warn once");
    }

    #[test]
    fn test_validate_gear_prefix_fuzzy_id_tiebreak_when_lengths_equal() {
        // Two equal-length names both contain needle. Lower id wins deterministically.
        let db = empty_db_with_itemstats(vec![(350, "Zerk Sample A"), (300, "Zerk Sample B")]);
        let result = run_validate_gear_prefix("Sample", &db);
        let p = result.primary_prefix().expect("should match");
        assert_eq!(p.itemstat_id, 300, "lower id must win equal-length tie");
    }

    #[test]
    fn test_validate_gear_prefix_not_found_emits_error() {
        let db = empty_db_with_itemstats(vec![(400, "Berserker's")]);
        let result = run_validate_gear_prefix("Nonexistent", &db);
        assert!(result.primary_prefix().is_none());
        assert_eq!(result.errors.len(), 1);
        assert!(matches!(
            result.errors[0].code,
            RejectCode::GearPrefixNotFound { .. }
        ));
    }

    #[test]
    fn test_validate_gear_prefix_populates_every_slot() {
        let db = empty_db_with_itemstats(vec![(101, "Berserker's")]);
        let result = run_validate_gear_prefix("Berserker's", &db);
        // Every hand of both sets holds a weapon here, so every slot is worn.
        for slot in GearSlot::ALL {
            let p = result
                .prefix_for(slot)
                .unwrap_or_else(|| panic!("slot {:?} must be populated", slot));
            assert_eq!(p.itemstat_id, 101);
            assert_eq!(p.name, "Berserker's");
        }
    }

    // validate_gear_slot_map() — per-slot plate policy (spec §12.3)

    fn run_validate_slot_map(
        stat_prefix: &str,
        gear_slots: std::collections::HashMap<String, String>,
        db: &GameDb,
    ) -> ValidatedBuild {
        let mut response = GeminiBuildResponse::default();
        response.stat_prefix = stat_prefix.into();
        response.gear_slots = Some(gear_slots);
        let mut result = dual_wielding_both_sets();
        validate_gear_prefix(&response, db, &mut result);
        validate_gear_slot_map(&response, db, &mut result);
        result
    }

    #[test]
    fn plate_full_map_overrides_each_named_slot() {
        let db = empty_db_with_itemstats(vec![(101, "Berserker's"), (102, "Cavalier's")]);
        let result = run_validate_slot_map(
            "Berserker's",
            [
                ("helm".to_string(), "Cavalier's".to_string()),
                ("coat".to_string(), "Berserker's".to_string()),
                ("ring-1".to_string(), "cavalier's".to_string()),
            ]
            .into_iter()
            .collect(),
            &db,
        );
        assert_eq!(result.prefix_for(GearSlot::Helm).unwrap().itemstat_id, 102);
        assert_eq!(result.prefix_for(GearSlot::Coat).unwrap().itemstat_id, 101);
        assert_eq!(result.prefix_for(GearSlot::Ring1).unwrap().itemstat_id, 102);
        // Unnamed slots keep the profile prefix.
        assert_eq!(result.prefix_for(GearSlot::Boots).unwrap().itemstat_id, 101);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn plate_partial_map_leaves_other_slots_at_profile_prefix() {
        let db = empty_db_with_itemstats(vec![(101, "Berserker's"), (103, "Viper's")]);
        let result = run_validate_slot_map(
            "Berserker's",
            [("weapon-set-2-main".to_string(), "Viper's".to_string())]
                .into_iter()
                .collect(),
            &db,
        );
        assert_eq!(
            result
                .prefix_for(GearSlot::WeaponSet2Main)
                .unwrap()
                .itemstat_id,
            103
        );
        for slot in [GearSlot::Helm, GearSlot::Amulet, GearSlot::WeaponSet1Main] {
            assert_eq!(result.prefix_for(slot).unwrap().itemstat_id, 101);
        }
    }

    #[test]
    fn plate_unknown_prefix_falls_back_to_profile_and_warns_once() {
        let db = empty_db_with_itemstats(vec![(101, "Berserker's")]);
        let result = run_validate_slot_map(
            "Berserker's",
            [
                ("helm".to_string(), "Nonexistentium".to_string()),
                ("boots".to_string(), "Nonexistentium".to_string()),
            ]
            .into_iter()
            .collect(),
            &db,
        );
        // Both slots keep the profile prefix.
        assert_eq!(result.prefix_for(GearSlot::Helm).unwrap().itemstat_id, 101);
        assert_eq!(result.prefix_for(GearSlot::Boots).unwrap().itemstat_id, 101);
        // One warning per unique failing name, not one per slot.
        let warnings: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.contains("not found"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", result.warnings);
        assert!(warnings[0].contains("Nonexistentium"));
        assert!(result.errors.is_empty());
    }

    #[test]
    fn plate_unknown_slot_name_is_rejected_with_warning() {
        let db = empty_db_with_itemstats(vec![(101, "Berserker's")]);
        let result = run_validate_slot_map(
            "Berserker's",
            [
                ("relic".to_string(), "Berserker's".to_string()),
                ("helm ".to_string(), "Berserker's".to_string()),
            ]
            .into_iter()
            .collect(),
            &db,
        );
        assert_eq!(result.prefix_for(GearSlot::Helm).unwrap().itemstat_id, 101);
        let rejected: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.contains("not a known equipment slot"))
            .collect();
        assert_eq!(rejected.len(), 1, "{:?}", result.warnings);
        assert!(rejected[0].contains("'relic'"));
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_resolve_slot_prefix_ids_replaces_migration_zeros() {
        let db = empty_db_with_itemstats(vec![(201, "Viper's")]);
        let groups = gw2_core::types::GearPrefixGroups {
            armor: "Vipers".into(),
            trinkets: String::new(),
            weapons: "Unresolvable".into(),
        };
        // from_legacy stamps itemstat_id = 0 — the shape a legacy save loads with.
        let mut build = ValidatedBuild::default();
        build.gear_slots = GearSlots::from_legacy("Viper's", &groups);
        assert_eq!(build.prefix_for(GearSlot::Helm).unwrap().itemstat_id, 0);

        build.resolve_slot_prefix_ids(&db);

        assert_eq!(
            build.prefix_for(GearSlot::Coat).unwrap().itemstat_id,
            201,
            "exact alnum match resolves the zero id"
        );
        assert_eq!(
            build.prefix_for(GearSlot::Amulet).unwrap().name,
            "Viper's",
            "blank group inherits the build-wide name and resolves too"
        );
        assert_eq!(
            build.prefix_for(GearSlot::WeaponSet1Main).unwrap(),
            &gw2_core::types::PrefixRef {
                itemstat_id: 0,
                name: "Unresolvable".into()
            },
            "unknown names keep the zero id instead of inventing one"
        );
    }

    // find_skill_by_name() needle-length guard

    fn make_skill(id: u32, name: &str) -> gw2_api::models::Skill {
        gw2_api::models::Skill {
            id,
            name: name.into(),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: None,
            professions: vec![],
            slot: Some("Utility".into()),
            facts: vec![],
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

    #[test]
    fn test_find_skill_short_needle_no_contains_match() {
        // needle "heal" (4 chars) is a substring of "Healing Spring".
        // Length guard (< 5) must block the contains fallback → None.
        let s = make_skill(1, "Healing Spring");
        let skills = vec![&s];
        let mut result = ValidatedBuild::default();
        let found = find_skill_by_name("heal", &skills, None, &mut result);
        assert!(
            found.is_none(),
            "4-char needle must not match via contains fallback"
        );
    }

    #[test]
    fn five_char_substring_does_not_apply_a_different_skill() {
        // "heali" is a 5-char substring of "Healing Spring". Contains used to
        // apply that other skill. Applied id must stay empty (names_eq only).
        let s = make_skill(2, "Healing Spring");
        let skills = vec![&s];
        let mut result = ValidatedBuild::default();
        let found = find_skill_by_name("heali", &skills, None, &mut result);
        assert!(
            found.is_none(),
            "5+ char substring must not apply a different skill, got {found:?}"
        );
        assert!(
            result.errors.iter().any(|e| matches!(
                &e.code,
                RejectCode::SkillNotFound { name } if name == "heali"
            )),
            "not-found must be SkillNotFound, not a warning that still fills; got {:?}",
            result.errors
        );
        assert!(
            result.warnings.iter().all(|w| !w.contains("fuzzy-matched")),
            "substring must not fuzzy-apply: {:?}",
            result.warnings
        );
    }

    #[test]
    fn names_eq_still_applies_the_named_skill() {
        let s = make_skill(2, "Healing Spring");
        let skills = vec![&s];
        let mut result = ValidatedBuild::default();
        let found = find_skill_by_name("healing spring", &skills, None, &mut result);
        assert_eq!(found.map(|(id, _)| id), Some(2));
        assert!(
            result.errors.is_empty(),
            "names_eq hit must not SkillNotFound: {:?}",
            result.errors
        );
    }

    fn firebrand_guardian_db() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        db.professions.insert(
            "Guardian".into(),
            gw2_api::models::Profession {
                id: "Guardian".into(),
                name: "Guardian".into(),
                code: None,
                specializations: vec![62],
                weapons: std::collections::HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        db.specializations.insert(
            62,
            Specialization {
                id: 62,
                name: "Firebrand".into(),
                profession: "Guardian".into(),
                elite: true,
                minor_traits: vec![],
                major_traits: vec![],
                weapon_trait: None,
                icon: None,
                background: None,
                profession_icon: None,
                profession_icon_big: None,
            },
        );
        db
    }

    #[test]
    fn fire_substring_does_not_apply_firebrand() {
        let db = firebrand_guardian_db();
        let mut result = ValidatedBuild::default();
        let found = find_spec_by_name(&db, "Fire", &[62], &mut result);
        assert!(
            found.is_none(),
            "Fire must not apply Firebrand, got {found:?}"
        );
        assert!(
            result.specializations.is_empty(),
            "substring must not apply a spec"
        );
        assert!(
            result.warnings.iter().any(|w| w.contains("Firebrand")),
            "substring must warn, got {:?}",
            result.warnings
        );
    }

    #[test]
    fn exact_firebrand_still_applies() {
        let db = firebrand_guardian_db();
        let mut result = ValidatedBuild::default();
        let found = find_spec_by_name(&db, "Firebrand", &[62], &mut result);
        assert_eq!(found.map(|s| s.id), Some(62));
        assert!(
            result.warnings.is_empty(),
            "exact must not warn: {:?}",
            result.warnings
        );
    }

    fn upgrade_item(id: u32, name: &str) -> Item {
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
        }
    }

    #[test]
    fn short_item_needle_does_not_steal() {
        let force = upgrade_item(10, "Superior Sigil of Force");
        let items = vec![&force];
        for needle in ["a", "sig"] {
            let mut result = ValidatedBuild::default();
            let found = find_item_by_name(needle, &items, "Sigil", &mut result);
            assert!(
                found.is_none(),
                "{needle} must not steal a sigil, got {found:?}"
            );
            assert!(
                result.errors.iter().any(|e| matches!(
                    &e.code,
                    RejectCode::ItemNotFound { name, .. } if name == needle
                )),
                "short needle must ItemNotFound, got {:?}",
                result.errors
            );
        }
        let mut ok = ValidatedBuild::default();
        let found = find_item_by_name("Superior Sigil of Force", &items, "Sigil", &mut ok);
        assert_eq!(found.map(|i| i.id), Some(10));
    }

    /// The PvP item table ships its own copy of every rune and sigil, filed by
    /// `GameDb` since the `details.type = "Default"` fix. The two names differ
    /// ("Sigil of Force" vs "Superior Sigil of Force"), so each resolves to its
    /// own item — and `GameDb::load` appends the PvP ids after the sort, so the
    /// PvE item is always the earlier element and wins every fuzzy pass.
    #[test]
    fn pvp_and_pve_upgrade_copies_resolve_to_their_own_item() {
        let pve = upgrade_item(10, "Superior Sigil of Force");
        let pvp = upgrade_item(21123, "Sigil of Force");
        // `all_sigils()` order: PvE first, PvP appended.
        let items = vec![&pve, &pvp];

        for (needle, want) in [("Superior Sigil of Force", 10), ("Sigil of Force", 21123)] {
            let mut result = ValidatedBuild::default();
            let found = find_item_by_name(needle, &items, "Sigil", &mut result);
            assert_eq!(found.map(|i| i.id), Some(want), "{needle}");
            assert!(result.errors.is_empty(), "{needle}: {:?}", result.errors);
        }

        // A partial needle misses both exact names and reaches the "item name
        // contains the needle" pass, which both copies satisfy. List order is
        // what decides there, so the PvE copy wins.
        let mut result = ValidatedBuild::default();
        let found = find_item_by_name("of Force", &items, "Sigil", &mut result);
        assert_eq!(
            found.map(|i| i.id),
            Some(10),
            "fuzzy must prefer the PvE copy"
        );
    }

    #[test]
    fn aquatic_only_skill_rejected_on_land() {
        let mut aquatic = make_skill(30, "Tidal Surge");
        aquatic.flags = vec!["Aquatic".into()];
        let skills = vec![&aquatic];
        let mut result = ValidatedBuild::default();
        let found = find_skill_by_name("Tidal Surge", &skills, None, &mut result);
        assert!(found.is_none(), "aquatic-only skill must not apply on land");
        assert!(
            result.errors.iter().any(|e| matches!(
                &e.code,
                RejectCode::SkillNotFound { name } if name == "Tidal Surge"
            )),
            "expected SkillNotFound, got {:?}",
            result.errors
        );
    }

    #[test]
    fn land_spear_skill_not_rejected_for_aquatic_weapon_flag() {
        let mut land = make_skill(31, "Barbed Spear");
        land.flags = vec!["NoUnderwater".into()];
        let skills = vec![&land];
        let mut result = ValidatedBuild::default();
        let found = find_skill_by_name("Barbed Spear", &skills, None, &mut result);
        assert_eq!(found.map(|(id, _)| id), Some(31));
        assert!(
            result.errors.is_empty(),
            "NoUnderwater land skill must apply: {:?}",
            result.errors
        );
    }

    fn make_prof_with_elite_axe() -> gw2_api::models::Profession {
        let mut weapons = std::collections::HashMap::new();
        weapons.insert(
            "Axe".to_string(),
            gw2_api::models::WeaponInfo {
                specialization: Some(99), // requires elite spec 99
                flags: vec!["Mainhand".into()],
                skills: vec![],
            },
        );
        gw2_api::models::Profession {
            id: "Guardian".into(),
            name: "Guardian".into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        }
    }

    /// An elite spec's weapon needs no elite spec: Weaponmaster Training
    /// hands it to the whole profession. Lowercase input still resolves to
    /// the canonical name, which is what later `prof.weapons` lookups need.
    #[test]
    fn elite_spec_weapon_is_kept_without_that_spec() {
        let prof = make_prof_with_elite_axe();
        let weapons = (Some("axe".to_string()), None);
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(&weapons, Some(&prof), &mut result, "Set 1");
        assert_eq!(set.main_hand.as_deref(), Some("Axe"));
        assert!(result.errors.is_empty(), "got {:?}", result.errors);
    }

    #[test]
    fn test_find_weapon_ignores_spaces() {
        let mut weapons = std::collections::HashMap::new();
        weapons.insert(
            "Shortbow".to_string(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["TwoHand".into()],
                skills: vec![],
            },
        );
        let prof = gw2_api::models::Profession {
            id: "Thief".into(),
            name: "Thief".into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        let (key, _) = find_weapon("Short Bow", &prof).expect("Short Bow should match Shortbow");
        assert_eq!(key, "Shortbow");
        assert!(find_weapon("shortbow", &prof).is_some());
        assert_eq!(
            find_weapon("ShortBow", &prof).map(|(k, _)| k.as_str()),
            Some("Shortbow")
        );

        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Short Bow".into()), None),
            Some(&prof),
            &mut result,
            "Set 2",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Shortbow"));
        assert!(
            result.errors.is_empty(),
            "spaced Short Bow should not reject; got {:?}",
            result.errors
        );
    }

    #[test]
    fn test_find_weapon_item_api_aliases() {
        let mut weapons = std::collections::HashMap::new();
        weapons.insert(
            "Spear".to_string(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["TwoHand".into()],
                skills: vec![],
            },
        );
        let prof = gw2_api::models::Profession {
            id: "Thief".into(),
            name: "Thief".into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        assert_eq!(
            find_weapon("Harpoon", &prof).map(|(k, _)| k.as_str()),
            Some("Spear")
        );
    }

    #[test]
    fn test_validate_weapon_set_rejects_aquatic_trident_on_land() {
        let mut weapons = std::collections::HashMap::new();
        weapons.insert(
            "Trident".to_string(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["TwoHand".into(), "Aquatic".into()],
                skills: vec![],
            },
        );
        weapons.insert(
            "Staff".to_string(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["TwoHand".into()],
                skills: vec![],
            },
        );
        let prof = gw2_api::models::Profession {
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
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Trident".into()), None),
            Some(&prof),
            &mut result,
            "Set 2",
        );
        assert!(
            set.main_hand.is_none(),
            "trident must not survive as a land set; got {:?}",
            set.main_hand
        );
        assert!(
            result.errors.iter().any(|e| matches!(
                &e.code,
                RejectCode::WeaponNotAvailable { weapon, .. } if weapon == "Trident"
            )),
            "expected WeaponNotAvailable for Trident; got {:?}",
            result.errors
        );

        let mut ok = ValidatedBuild::default();
        let staff =
            validate_weapon_set(&(Some("Staff".into()), None), Some(&prof), &mut ok, "Set 2");
        assert_eq!(staff.main_hand.as_deref(), Some("Staff"));
        assert!(ok.errors.is_empty());
    }

    #[test]
    fn test_validate_weapon_set_accepts_land_spear_with_aquatic_flag() {
        let mut weapons = std::collections::HashMap::new();
        weapons.insert(
            "Spear".to_string(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["TwoHand".into(), "Aquatic".into()],
                skills: vec![],
            },
        );
        let prof = gw2_api::models::Profession {
            id: "Thief".into(),
            name: "Thief".into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Spear".into()), None),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Spear"));
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    fn elite_spec(id: u32, name: &str) -> ValidatedSpec {
        ValidatedSpec {
            spec_id: id,
            name: name.into(),
            elite: true,
            trait_ids: vec![],
            trait_names: vec![],
            all_trait_ids: vec![],
        }
    }

    fn prof_with_weapons(
        name: &str,
        weapons: &[(&str, Option<u32>)],
    ) -> gw2_api::models::Profession {
        let mut map = std::collections::HashMap::new();
        for (ty, spec) in weapons {
            let two = crate::weapon_budget::is_two_handed(ty, None);
            map.insert(
                (*ty).to_string(),
                gw2_api::models::WeaponInfo {
                    specialization: *spec,
                    flags: if two {
                        vec!["TwoHand".into()]
                    } else {
                        vec!["Mainhand".into(), "Offhand".into()]
                    },
                    skills: vec![],
                },
            );
        }
        gw2_api::models::Profession {
            id: name.into(),
            name: name.into(),
            code: None,
            specializations: vec![],
            weapons: map,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        }
    }

    /// Guardian off-hand Sword is Willbender's, and Weaponmaster Training
    /// lets a Firebrand hold it.
    #[test]
    fn firebrand_dual_swords_ok() {
        let prof = prof_with_weapons("Guardian", &[("Sword", None)]);
        let mut result = ValidatedBuild::default();
        result.specializations = vec![elite_spec(62, "Firebrand")];
        let set = validate_weapon_set(
            &(Some("Sword".into()), Some("Sword".into())),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Sword"));
        assert_eq!(set.off_hand.as_deref(), Some("Sword"));
        assert!(result.errors.is_empty(), "got {:?}", result.errors);
    }

    #[test]
    fn herald_dual_swords_ok() {
        let prof = prof_with_weapons("Revenant", &[("Sword", None)]);
        let mut result = ValidatedBuild::default();
        result.specializations = vec![elite_spec(52, "Herald")];
        let set = validate_weapon_set(
            &(Some("Sword".into()), Some("Sword".into())),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Sword"));
        assert_eq!(set.off_hand.as_deref(), Some("Sword"));
        assert!(
            result.errors.is_empty(),
            "Herald dual swords are core; got {:?}",
            result.errors
        );
    }

    /// API lie: whole Dagger spec=55. Wiki: MH=Soulbeast, OH=core. Both
    /// hands are legal for any Ranger under Weaponmaster Training.
    #[test]
    fn ranger_dagger_dagger_without_soulbeast_keeps_both_hands() {
        let prof = prof_with_weapons("Ranger", &[("Dagger", Some(55))]);
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Dagger".into()), Some("Dagger".into())),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Dagger"));
        assert_eq!(set.off_hand.as_deref(), Some("Dagger"));
        assert!(result.errors.is_empty(), "got {:?}", result.errors);
    }

    #[test]
    fn ranger_sword_offhand_not_available() {
        let prof = prof_with_weapons("Ranger", &[("Sword", None)]);
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Sword".into()), Some("Sword".into())),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Sword"));
        assert!(
            set.off_hand.is_none(),
            "ranger off-hand sword must be rejected; got {:?}",
            set.off_hand
        );
        assert!(
            result.errors.iter().any(|e| matches!(
                &e.code,
                RejectCode::WeaponNotAvailable { weapon, .. } if weapon == "Sword"
            )),
            "expected WeaponNotAvailable for Ranger sword OH; got {:?}",
            result.errors
        );
    }

    /// A plate built from published gear ids has no "Set 1:" labels and no
    /// "/" — it is one bare weapon type per equipped weapon. Slotting each
    /// into its own set put Warhorn in a main hand and threw the build out.
    #[test]
    fn flat_weapon_list_is_one_set_not_two_main_hands() {
        let response = GeminiBuildResponse {
            weapons: vec!["Scepter".into(), "Warhorn".into()],
            ..Default::default()
        };
        let (set1, set2) = parse_weapon_sets_from_response(&response, "Elementalist");
        assert_eq!(set1.0.as_deref(), Some("Scepter"));
        assert_eq!(set1.1.as_deref(), Some("Warhorn"));
        assert_eq!(set2, (None, None));
    }

    /// Four weapons are two sets, and the hands come from the wiki table:
    /// Focus is off-hand only, a second Sword is not.
    #[test]
    fn flat_weapon_list_fills_two_sets_by_hand() {
        let response = GeminiBuildResponse {
            weapons: vec![
                "Sword".into(),
                "Focus".into(),
                "Sword".into(),
                "Dagger".into(),
            ],
            ..Default::default()
        };
        let (set1, set2) = parse_weapon_sets_from_response(&response, "Elementalist");
        assert_eq!(
            (set1.0.as_deref(), set1.1.as_deref()),
            (Some("Sword"), Some("Focus"))
        );
        assert_eq!(
            (set2.0.as_deref(), set2.1.as_deref()),
            (Some("Sword"), Some("Dagger"))
        );
    }

    /// A two-hander fills a set on its own.
    #[test]
    fn flat_weapon_list_gives_each_two_hander_its_own_set() {
        let response = GeminiBuildResponse {
            weapons: vec!["Hammer".into(), "Hammer".into()],
            ..Default::default()
        };
        let (set1, set2) = parse_weapon_sets_from_response(&response, "Elementalist");
        assert_eq!((set1.0.as_deref(), set1.1), (Some("Hammer"), None));
        assert_eq!((set2.0.as_deref(), set2.1), (Some("Hammer"), None));
    }

    /// Labelled plates keep the old reading.
    #[test]
    fn labelled_weapon_lines_still_parse_as_written() {
        let response = GeminiBuildResponse {
            weapons: vec!["Set 1: Axe / Axe".into(), "Set 2: Greatsword".into()],
            ..Default::default()
        };
        let (set1, set2) = parse_weapon_sets_from_response(&response, "Warrior");
        assert_eq!(set1.0.as_deref(), Some("Axe"));
        assert_eq!(set1.1.as_deref(), Some("Axe"));
        assert_eq!(set2.0.as_deref(), Some("Greatsword"));
        assert_eq!(set2.1, None);
    }

    /// Warhorn is Tempest's, and it is legal in an Elementalist off-hand.
    #[test]
    fn tempest_warhorn_validates() {
        let prof = prof_with_weapons("Elementalist", &[("Dagger", None), ("Warhorn", Some(48))]);
        let mut result = ValidatedBuild::default();
        result.specializations = vec![elite_spec(48, "Tempest")];
        let set = validate_weapon_set(
            &(Some("Dagger".into()), Some("Warhorn".into())),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Dagger"));
        assert_eq!(set.off_hand.as_deref(), Some("Warhorn"));
        assert!(result.errors.is_empty(), "got {:?}", result.errors);
    }

    /// Warrior Warhorn is core, with no elite spec equipped at all.
    #[test]
    fn warrior_warhorn_validates() {
        let prof = prof_with_weapons("Warrior", &[("Axe", None), ("Warhorn", None)]);
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Axe".into()), Some("Warhorn".into())),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert_eq!(set.main_hand.as_deref(), Some("Axe"));
        assert_eq!(set.off_hand.as_deref(), Some("Warhorn"));
        assert!(result.errors.is_empty(), "got {:?}", result.errors);
    }

    /// No Elementalist spec trains Greatsword, so no unlock reaches it.
    #[test]
    fn elementalist_greatsword_is_rejected() {
        let prof = prof_with_weapons("Elementalist", &[("Greatsword", None)]);
        let mut result = ValidatedBuild::default();
        let set = validate_weapon_set(
            &(Some("Greatsword".into()), None),
            Some(&prof),
            &mut result,
            "Set 1",
        );
        assert!(set.main_hand.is_none(), "got {:?}", set.main_hand);
        assert!(
            result.errors.iter().any(|e| matches!(
                &e.code,
                RejectCode::WeaponNotAvailable { weapon, .. } if weapon == "Greatsword"
            )),
            "expected WeaponNotAvailable; got {:?}",
            result.errors
        );
    }

    #[test]
    fn test_find_skill_exact_match_bypasses_guard() {
        // Exact match for a 4-char skill name must still succeed.
        let s = make_skill(3, "Bolt");
        let skills = vec![&s];
        let mut result = ValidatedBuild::default();
        let found = find_skill_by_name("Bolt", &skills, None, &mut result);
        assert_eq!(found.map(|(id, _)| id), Some(3));
    }

    #[test]
    fn test_validate_gear_prefix_empty_input_is_noop() {
        let db = empty_db_with_itemstats(vec![(500, "Berserker's")]);
        let result = run_validate_gear_prefix("", &db);
        assert!(result.primary_prefix().is_none());
        assert!(result.errors.is_empty());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn test_parse_skill_names_utility_prefix() {
        let mut response = GeminiBuildResponse::default();
        response.skills = vec![
            "Heal: Mending".into(),
            "Utility: Signet of Fury".into(),
            "utility: Banner of Strength".into(),
            "Elite: Signet of Rage".into(),
        ];
        let (heal, utils, elite) = parse_skill_names_from_response(&response);
        assert_eq!(heal.as_deref(), Some("Mending"));
        assert_eq!(utils, vec!["Signet of Fury", "Banner of Strength"]);
        assert_eq!(elite.as_deref(), Some("Signet of Rage"));
    }

    fn ele_db_with_tempest() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        db.professions.insert(
            "Elementalist".into(),
            gw2_api::models::Profession {
                id: "Elementalist".into(),
                name: "Elementalist".into(),
                code: Some(6),
                specializations: vec![48, 17, 41],
                weapons: HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        for (id, name, elite) in [
            (48u32, "Tempest", true),
            (17, "Water", false),
            (41, "Arcane", false),
        ] {
            db.specializations.insert(
                id,
                Specialization {
                    id,
                    name: name.into(),
                    profession: "Elementalist".into(),
                    elite,
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
        db
    }

    #[test]
    fn test_unknown_profession_infers_elementalist_from_tempest() {
        let db = ele_db_with_tempest();
        assert_eq!(
            infer_profession_from_text(&db, "tempest celestial support").as_deref(),
            Some("Elementalist")
        );
        assert_eq!(
            infer_profession_from_spec_names(&db, ["Tempest", "Water", "Arcane"]).as_deref(),
            Some("Elementalist")
        );

        let mut response = GeminiBuildResponse::default();
        response.specializations = vec![
            ("Tempest".into(), vec![]),
            ("Water".into(), vec![]),
            ("Arcane".into(), vec![]),
        ];
        let result = validate_gemini_build(&response, &db, "unknown");
        let names: Vec<&str> = result
            .specializations
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["Tempest", "Water", "Arcane"]);
        assert!(
            !result.errors.iter().any(|e| e.detail.contains("unknown")),
            "{result:?}"
        );
    }

    fn ranger_pet_db() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        for (id, name) in [
            (1u32, "Juvenile Jungle Stalker"),
            (2, "Juvenile Brown Bear"),
            (3, "Juvenile Shark"),
        ] {
            db.pets.insert(
                id,
                gw2_api::models::Pet {
                    id,
                    name: name.into(),
                    description: None,
                    icon: None,
                    skills: vec![],
                },
            );
        }
        db
    }

    /// RED: validate never wrote ValidatedBuild.pets.
    #[test]
    fn validate_writes_resolved_pets() {
        let db = ranger_pet_db();
        let mut response = GeminiBuildResponse::default();
        response.pets = Some([
            Some("Jungle Stalker".into()),
            Some("Brown Bear".into()),
            Some("Shark".into()),
            None,
        ]);
        let result = validate_gemini_build(&response, &db, "Ranger");
        assert_eq!(result.pets, Some((Some(1), Some(2), Some(3), None)));
        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.detail.to_lowercase().contains("pet")),
            "{:?}",
            result.errors
        );
    }

    /// RED: unknown pet name must error, not silently drop.
    #[test]
    fn validate_pets_errors_on_unknown_name() {
        let db = ranger_pet_db();
        let mut response = GeminiBuildResponse::default();
        response.pets = Some([Some("Warg".into()), None, None, None]);
        let result = validate_gemini_build(&response, &db, "Ranger");
        assert!(
            result.errors.iter().any(|e| e.detail.contains("Warg")),
            "miss must error: {:?}",
            result.errors
        );
        assert_eq!(result.pets, Some((None, None, None, None)));
    }

    fn legend_fixture_db() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        let mk = |id: &str, code: u32, heal: u32, elite: u32, utilities: [u32; 3], swap: u32| {
            gw2_api::models::Legend {
                id: id.into(),
                code: Some(code),
                swap,
                heal,
                elite,
                utilities: utilities.to_vec(),
            }
        };
        db.legends
            .insert("Legend1".into(), mk("Legend1", 1, 10, 19, [11, 12, 13], 18));
        db.legends
            .insert("Legend2".into(), mk("Legend2", 2, 20, 29, [21, 22, 23], 28));
        for (id, name) in [
            (10u32, "Heal One"),
            (11, "Util A"),
            (12, "Util B"),
            (13, "Util C"),
            (19, "Elite One"),
            (20, "Heal Two"),
            (21, "Util X"),
            (22, "Util Y"),
            (23, "Util Z"),
            (29, "Elite Two"),
        ] {
            db.skills.insert(id, make_skill(id, name));
        }
        db
    }

    /// RED: infer-from-heal replaced plate utilities/elite with no warning.
    #[test]
    fn revenant_infer_from_heal_warns_when_replacing_utilities() {
        let db = legend_fixture_db();
        let mut result = ValidatedBuild::default();
        result.skills.heal = Some((10, "Heal One".into()));
        result.skills.utilities = vec![
            Some((21, "Util X".into())),
            Some((22, "Util Y".into())),
            Some((23, "Util Z".into())),
        ];
        result.skills.elite = Some((29, "Elite Two".into()));
        fill_revenant_legends(&GeminiBuildResponse::default(), &mut result, &db);
        assert_eq!(result.legends.first().map(String::as_str), Some("Legend1"));
        assert_eq!(
            result
                .skills
                .utilities
                .iter()
                .filter_map(|u| u.as_ref().map(|p| p.0))
                .collect::<Vec<_>>(),
            vec![11, 12, 13]
        );
        assert_eq!(result.skills.elite.as_ref().map(|e| e.0), Some(19));
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("replaced") && w.contains("Legend1")),
            "silent overwrite: {:?}",
            result.warnings
        );
    }

    /// RED: plate Legend2 must win over heal-inferred Legend1.
    #[test]
    fn revenant_plate_legend_ids_are_honored() {
        let db = legend_fixture_db();
        let mut response = GeminiBuildResponse::default();
        response.skills = vec!["Legend2".into()];
        let mut result = ValidatedBuild::default();
        result.skills.heal = Some((10, "Heal One".into()));
        fill_revenant_legends(&response, &mut result, &db);
        assert_eq!(result.legends.first().map(String::as_str), Some("Legend2"));
        assert_eq!(result.skills.heal.as_ref().map(|h| h.0), Some(20));
        assert_eq!(
            result
                .skills
                .utilities
                .iter()
                .filter_map(|u| u.as_ref().map(|p| p.0))
                .collect::<Vec<_>>(),
            vec![21, 22, 23]
        );
        assert_eq!(result.skills.elite.as_ref().map(|e| e.0), Some(29));
        assert!(
            !result.warnings.iter().any(|w| w.contains("inferred")),
            "explicit legend should not warn as inferred: {:?}",
            result.warnings
        );
    }

    /// RED: IncompleteSkillBar / SkillNotFound recorded before legend fill
    /// must not survive on a plate the fill just made legal.
    #[test]
    fn revenant_legend_fill_clears_stale_skill_bar_errors() {
        let mut db = legend_fixture_db();
        db.professions.insert(
            "Revenant".into(),
            gw2_api::models::Profession {
                id: "Revenant".into(),
                name: "Revenant".into(),
                code: None,
                specializations: vec![9],
                weapons: std::collections::HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        db.specializations.insert(
            9,
            Specialization {
                id: 9,
                name: "Corruption".into(),
                profession: "Revenant".into(),
                elite: false,
                minor_traits: vec![],
                major_traits: vec![1, 2, 3],
                weapon_trait: None,
                icon: None,
                background: None,
                profession_icon: None,
                profession_icon_big: None,
            },
        );
        db.traits.insert(1, make_trait(1, "Opportunist"));
        db.traits.insert(2, make_trait(2, "Unholy Fervor"));
        db.traits.insert(3, make_trait(3, "Demonic Defiance"));
        db.traits_by_spec.insert(9, vec![1, 2, 3]);

        let response = GeminiBuildResponse {
            specializations: vec![(
                "Corruption".into(),
                vec![
                    "Opportunist".into(),
                    "Unholy Fervor".into(),
                    "Demonic Defiance".into(),
                ],
            )],
            skills: vec![
                "Heal: NotARealHeal".into(),
                "Utils: GarbageA, GarbageB, GarbageC".into(),
                "Elite: NopeElite".into(),
            ],
            ..Default::default()
        };
        let result = validate_gemini_build(&response, &db, "Revenant");
        assert_eq!(result.skills.heal.as_ref().map(|h| h.0), Some(10));
        assert_eq!(
            result
                .skills
                .utilities
                .iter()
                .filter_map(|u| u.as_ref().map(|p| p.0))
                .collect::<Vec<_>>(),
            vec![11, 12, 13]
        );
        assert_eq!(result.skills.elite.as_ref().map(|e| e.0), Some(19));
        assert!(
            !result
                .errors
                .iter()
                .any(|e| matches!(e.code, RejectCode::IncompleteSkillBar { .. })),
            "filled legend bar must not keep IncompleteSkillBar: {:?}",
            result.errors
        );
        assert!(
            !result
                .errors
                .iter()
                .any(|e| matches!(e.code, RejectCode::SkillNotFound { .. })),
            "filled legend bar must not keep SkillNotFound: {:?}",
            result.errors
        );
    }

    /// RED: rest-pad (no heal, no Legend* tokens) must not claim inferred-from-heal.
    #[test]
    fn revenant_rest_pad_does_not_warn_inferred_from_heal() {
        let db = legend_fixture_db();
        let mut result = ValidatedBuild::default();
        result.skills.utilities = vec![
            Some((21, "Util X".into())),
            Some((22, "Util Y".into())),
            Some((23, "Util Z".into())),
        ];
        result.skills.elite = Some((29, "Elite Two".into()));
        fill_revenant_legends(&GeminiBuildResponse::default(), &mut result, &db);
        assert_eq!(result.legends.first().map(String::as_str), Some("Legend1"));
        assert_eq!(result.skills.heal.as_ref().map(|h| h.0), Some(10));
        assert_eq!(
            result
                .skills
                .utilities
                .iter()
                .filter_map(|u| u.as_ref().map(|p| p.0))
                .collect::<Vec<_>>(),
            vec![11, 12, 13]
        );
        assert_eq!(result.skills.elite.as_ref().map(|e| e.0), Some(19));
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("inferred from heal")),
            "rest-pad must not claim inferred from heal: {:?}",
            result.warnings
        );
    }
}
