use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;
use thiserror::Error;

use super::{try_load, DataLoadError, EvidenceLevel};

/// Canonical JSON embedded at compile time from data/slot_budgets/level80_ascended.json.
const SLOT_BUDGETS_JSON: &str = include_str!("../../../../data/slot_budgets/level80_ascended.json");

static BUDGETS: OnceLock<SlotBudgets> = OnceLock::new();

/// Returns the globally loaded slot budgets, parsing on first access.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data, should never happen).
pub fn slot_budgets() -> &'static SlotBudgets {
    BUDGETS.get_or_init(|| {
        load_slot_budgets(SLOT_BUDGETS_JSON).expect("embedded level80_ascended.json is invalid")
    })
}

/// Try to load slot budgets from the embedded JSON, returning typed errors
/// on failure. Does NOT store in OnceLock — used for health-check validation.
pub fn try_load_slot_budgets() -> Result<SlotBudgets, Vec<DataLoadError>> {
    try_load!(
        "slot_budgets",
        load_slot_budgets(SLOT_BUDGETS_JSON),
        SlotBudgetError
    )
}

#[derive(Debug, Error)]
pub enum SlotBudgetError {
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    ValidationError(String),
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
pub enum SlotType {
    Helm,
    Shoulders,
    Coat,
    Gloves,
    Leggings,
    Boots,
    WeaponOneHand,
    WeaponTwoHand,
    Amulet,
    Accessory,
    Ring,
    BackItem,
}

impl SlotType {
    /// All 12 slot types in canonical order.
    pub const ALL: [SlotType; 12] = [
        SlotType::Helm,
        SlotType::Shoulders,
        SlotType::Coat,
        SlotType::Gloves,
        SlotType::Leggings,
        SlotType::Boots,
        SlotType::WeaponOneHand,
        SlotType::WeaponTwoHand,
        SlotType::Amulet,
        SlotType::Accessory,
        SlotType::Ring,
        SlotType::BackItem,
    ];

    /// Map a GW2 API equipment slot name to a `SlotType`.
    ///
    /// API slot names: "Helm", "Shoulders", "Coat", "Gloves", "Leggings",
    /// "Boots", "WeaponA1", "WeaponA2", "WeaponB1", "WeaponB2", "Backpack",
    /// "Accessory1", "Accessory2", "Amulet", "Ring1", "Ring2".
    ///
    /// Weapon main-hand slots (WeaponA1, WeaponB1) map to `WeaponTwoHand`
    /// because main-hand could be two-handed; off-hand slots (WeaponA2,
    /// WeaponB2) map to `WeaponOneHand`.
    pub fn from_api_slot(slot: &str) -> Option<SlotType> {
        match slot {
            "Helm" => Some(SlotType::Helm),
            "Shoulders" => Some(SlotType::Shoulders),
            "Coat" => Some(SlotType::Coat),
            "Gloves" => Some(SlotType::Gloves),
            "Leggings" => Some(SlotType::Leggings),
            "Boots" => Some(SlotType::Boots),
            "WeaponA1" | "WeaponB1" => Some(SlotType::WeaponTwoHand),
            "WeaponA2" | "WeaponB2" => Some(SlotType::WeaponOneHand),
            "Backpack" => Some(SlotType::BackItem),
            "Accessory1" | "Accessory2" => Some(SlotType::Accessory),
            "Amulet" => Some(SlotType::Amulet),
            "Ring1" | "Ring2" => Some(SlotType::Ring),
            _ => None,
        }
    }
}

/// Map an optimizer gear slot to the budget slot type it draws from.
///
/// Mirrors `SlotType::from_api_slot` over the legacy API names
/// ("WeaponA1" → WeaponSet1Main etc.) so per-slot candidates score with the
/// exact same budgets the string-keyed candidates did.
pub fn slot_type_for_gear_slot(slot: gw2_core::types::GearSlot) -> SlotType {
    use gw2_core::types::GearSlot;
    match slot {
        GearSlot::Helm => SlotType::Helm,
        GearSlot::Shoulders => SlotType::Shoulders,
        GearSlot::Coat => SlotType::Coat,
        GearSlot::Gloves => SlotType::Gloves,
        GearSlot::Leggings => SlotType::Leggings,
        GearSlot::Boots => SlotType::Boots,
        GearSlot::Back => SlotType::BackItem,
        GearSlot::Accessory1 | GearSlot::Accessory2 => SlotType::Accessory,
        GearSlot::Amulet => SlotType::Amulet,
        GearSlot::Ring1 | GearSlot::Ring2 => SlotType::Ring,
        // Main-hand could be two-handed; off-hand is always one-handed —
        // identical to from_api_slot("WeaponA1"/"WeaponA2").
        GearSlot::WeaponSet1Main | GearSlot::WeaponSet2Main => SlotType::WeaponTwoHand,
        GearSlot::WeaponSet1Off | GearSlot::WeaponSet2Off => SlotType::WeaponOneHand,
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
pub enum StatShape {
    ThreeStat,
    FourStat,
    CelestialLike,
}

impl StatShape {
    /// All 3 stat shapes.
    pub const ALL: [StatShape; 3] = [
        StatShape::ThreeStat,
        StatShape::FourStat,
        StatShape::CelestialLike,
    ];
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlotBudgetEntry {
    pub slot: SlotType,
    pub shape: StatShape,
    pub major: i32,
    pub minor: i32,
    pub evidence_level: EvidenceLevel,
}

#[derive(Debug, Deserialize)]
struct SlotBudgetFile {
    #[allow(dead_code)]
    rarity: String,
    #[allow(dead_code)]
    level: i32,
    entries: Vec<SlotBudgetEntry>,
}

/// O(1) lookup wrapper for loaded slot budgets.
#[derive(Debug)]
pub struct SlotBudgets {
    map: HashMap<(SlotType, StatShape), SlotBudgetEntry>,
}

/// Full Ascended equipment set: 16 slots with their `SlotType`.
/// Matches the layout of the old `SLOT_ADJUSTMENTS` constant:
/// 6 armor + 4 weapons (2 weapon sets × main+off) + 6 trinkets.
pub const EQUIPMENT_SLOTS: &[(SlotType, &str)] = &[
    (SlotType::Helm, "Helm"),
    (SlotType::Shoulders, "Shoulders"),
    (SlotType::Coat, "Coat"),
    (SlotType::Gloves, "Gloves"),
    (SlotType::Leggings, "Leggings"),
    (SlotType::Boots, "Boots"),
    (SlotType::WeaponTwoHand, "WeaponA1"), // main-hand = two-hand budget
    (SlotType::WeaponOneHand, "WeaponA2"), // off-hand = one-hand budget
    (SlotType::WeaponTwoHand, "WeaponB1"), // main-hand = two-hand budget
    (SlotType::WeaponOneHand, "WeaponB2"), // off-hand = one-hand budget
    (SlotType::BackItem, "Backpack"),
    (SlotType::Accessory, "Accessory1"),
    (SlotType::Accessory, "Accessory2"),
    (SlotType::Amulet, "Amulet"),
    (SlotType::Ring, "Ring1"),
    (SlotType::Ring, "Ring2"),
];

impl SlotBudgets {
    /// Look up the budget entry for a specific slot and stat shape.
    pub fn get(&self, slot: SlotType, shape: StatShape) -> Option<&SlotBudgetEntry> {
        self.map.get(&(slot, shape))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when no slot budget entries are loaded.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Look up the ThreeStat major value for an API equipment slot name.
    /// Returns the pre-computed final stat value for the major attribute.
    ///
    /// This replaces the old `attribute_adjustment_for_slot()` function.
    /// Falls back to 0 for unknown slot names.
    pub fn major_for_api_slot(&self, slot: &str) -> i32 {
        SlotType::from_api_slot(slot)
            .and_then(|st| self.get(st, StatShape::ThreeStat))
            .map(|e| e.major)
            .unwrap_or(0)
    }
}

/// Determine the stat shape from the number of attributes in an itemstat.
pub fn stat_shape_from_attr_count(attr_count: usize) -> StatShape {
    match attr_count {
        4 => StatShape::FourStat,
        7..=9 => StatShape::CelestialLike,
        _ => StatShape::ThreeStat,
    }
}

/// Parse and validate slot budgets from JSON text.
pub fn load_slot_budgets(json: &str) -> Result<SlotBudgets, SlotBudgetError> {
    let file: SlotBudgetFile = serde_json::from_str(json)?;
    validate_entries(&file.entries)?;
    let map: HashMap<(SlotType, StatShape), SlotBudgetEntry> = file
        .entries
        .into_iter()
        .map(|e| ((e.slot, e.shape), e))
        .collect();
    Ok(SlotBudgets { map })
}

fn validate_entries(entries: &[SlotBudgetEntry]) -> Result<(), SlotBudgetError> {
    // No zero values (check first — catches bad data before completeness)
    for entry in entries {
        if entry.major == 0 {
            return Err(SlotBudgetError::ValidationError(format!(
                "{:?} {:?} has zero major value",
                entry.slot, entry.shape
            )));
        }
        if entry.minor == 0 {
            return Err(SlotBudgetError::ValidationError(format!(
                "{:?} {:?} has zero minor value",
                entry.slot, entry.shape
            )));
        }
    }

    // Check for duplicates
    let mut seen = HashSet::new();
    for entry in entries {
        if !seen.insert((entry.slot, entry.shape)) {
            return Err(SlotBudgetError::ValidationError(format!(
                "duplicate entry: {:?} {:?}",
                entry.slot, entry.shape
            )));
        }
    }

    // All 12 slots must be present for each shape
    for shape in &StatShape::ALL {
        for slot in &SlotType::ALL {
            if !seen.contains(&(*slot, *shape)) {
                return Err(SlotBudgetError::ValidationError(format!(
                    "missing entry: {:?} {:?}",
                    slot, shape
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_slot_budgets_load_successfully() {
        let b = slot_budgets();
        // 12 slots * 3 shapes = 36 entries
        assert_eq!(b.len(), 36);
    }

    // Source: API:2/items/46774 — Zojja's Blade (Berserker's Ascended 1H Sword)
    // Power=125, Precision=90, CritDamage=90
    #[test]
    fn test_three_stat_1h_weapon_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::WeaponOneHand, StatShape::ThreeStat)
            .expect("WeaponOneHand ThreeStat missing");
        // API:2/items/46774 — Zojja's Blade: Power=125, Precision=90
        assert_eq!(entry.major, 125);
        assert_eq!(entry.minor, 90);
    }

    // Source: API:2/items/46762 — Zojja's Claymore (Berserker's Ascended 2H Greatsword)
    // Power=251, Precision=179, CritDamage=179
    #[test]
    fn test_three_stat_2h_weapon_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::WeaponTwoHand, StatShape::ThreeStat)
            .expect("WeaponTwoHand ThreeStat missing");
        // API:2/items/46762 — Zojja's Claymore: Power=251, Precision=179
        assert_eq!(entry.major, 251);
        assert_eq!(entry.minor, 179);
    }

    // Source: API:2/items/39273 — Mark of the Tethyos Houses (Berserker's Ascended Amulet)
    // Power=157, Precision=108, CritDamage=108
    #[test]
    fn test_three_stat_amulet_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::Amulet, StatShape::ThreeStat)
            .expect("Amulet ThreeStat missing");
        // API:2/items/39273 — Mark of the Tethyos Houses: Power=157, Precision=108
        assert_eq!(entry.major, 157);
        assert_eq!(entry.minor, 108);
    }

    // Source: API:2/items/39232 — Magister's Field Journal (Berserker's Ascended Accessory)
    // Power=110, Precision=74, CritDamage=74
    #[test]
    fn test_three_stat_accessory_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::Accessory, StatShape::ThreeStat)
            .expect("Accessory ThreeStat missing");
        // API:2/items/39232 — Magister's Field Journal: Power=110, Precision=74
        assert_eq!(entry.major, 110);
        assert_eq!(entry.minor, 74);
    }

    // Source: API:2/items/75669 — Attuned Ring of Red Death (Infused)
    //   (Berserker's Ascended Ring)
    // Power=126, Precision=85, CritDamage=85
    #[test]
    fn test_three_stat_ring_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::Ring, StatShape::ThreeStat)
            .expect("Ring ThreeStat missing");
        // API:2/items/75669 — Ring of Red Death: Power=126, Precision=85
        assert_eq!(entry.major, 126);
        assert_eq!(entry.minor, 85);
    }

    // Source: API:2/items/37039 — Beta Fractal Capacitor (Infused)
    //   (Berserker's Ascended Back Item)
    // Power=63, Precision=40, CritDamage=40
    #[test]
    fn test_three_stat_back_item_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::BackItem, StatShape::ThreeStat)
            .expect("BackItem ThreeStat missing");
        // API:2/items/37039 — Beta Fractal Capacitor: Power=63, Precision=40
        assert_eq!(entry.major, 63);
        assert_eq!(entry.minor, 40);
    }

    // Source: API:2/items/48075 — Zojja's Visor (Berserker's Ascended Heavy Helm)
    // Power=63, Precision=45, CritDamage=45
    #[test]
    fn test_three_stat_helm_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::Helm, StatShape::ThreeStat)
            .expect("Helm ThreeStat missing");
        // API:2/items/48075 — Zojja's Visor: Power=63, Precision=45
        assert_eq!(entry.major, 63);
        assert_eq!(entry.minor, 45);
    }

    // Source: API:2/items/48073 — Zojja's Breastplate (Berserker's Ascended Heavy Coat)
    // Power=141, Precision=101, CritDamage=101
    #[test]
    fn test_three_stat_coat_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::Coat, StatShape::ThreeStat)
            .expect("Coat ThreeStat missing");
        // API:2/items/48073 — Zojja's Breastplate: Power=141, Precision=101
        assert_eq!(entry.major, 141);
        assert_eq!(entry.minor, 101);
    }

    // AC:6 — Armor slot stat values are identical across weight classes.
    // Verified via GW2 API:
    //   Heavy Helm (48075, Zojja's Visor): Power=63, Precision=45
    //   Medium Helm (48087, Zojja's Visage): Power=63, Precision=45
    //   Light Helm (48081, Zojja's Masque): Power=63, Precision=45
    //   Heavy Coat (48073, Zojja's Breastplate): Power=141, Precision=101
    //   Light Coat (48079, Zojja's Doublet): Power=141, Precision=101
    //   Medium Coat (48085, Zojja's Guise): Power=141, Precision=101
    #[test]
    fn test_armor_slots_weight_invariant() {
        let b = slot_budgets();
        // All armor slots: same stat budget regardless of weight class.
        // The data file stores a single entry per slot type (not per weight).
        // This test verifies the expected values match the API-verified
        // values from all three weight classes.
        let armor_slots = [
            // (slot, expected_major, expected_minor)
            // API:2/items/48075 (Heavy), 48087 (Medium), 48081 (Light)
            (SlotType::Helm, 63, 45),
            // API:2/items/48077 (Heavy), 48089 (Medium), 48083 (Light)
            (SlotType::Shoulders, 47, 34),
            // API:2/items/48073 (Heavy), 48085 (Medium), 48079 (Light)
            (SlotType::Coat, 141, 101),
            // API:2/items/48074 (Heavy), 48086 (Medium), 48080 (Light)
            (SlotType::Gloves, 47, 34),
            // API:2/items/48076 (Heavy), 48088 (Medium), 48082 (Light)
            (SlotType::Leggings, 94, 67),
            // API:2/items/48078 (Heavy), 48090 (Medium), 48084 (Light)
            (SlotType::Boots, 47, 34),
        ];
        for (slot, expected_major, expected_minor) in &armor_slots {
            let entry = b
                .get(*slot, StatShape::ThreeStat)
                .unwrap_or_else(|| panic!("{:?} ThreeStat missing", slot));
            assert_eq!(entry.major, *expected_major, "{:?} major mismatch", slot);
            assert_eq!(entry.minor, *expected_minor, "{:?} minor mismatch", slot);
        }
    }

    // Source: API:2/items/74083 — Tizlak's Short Bow
    //   (Commander's Ascended 2H ShortBow, FourStat stat_id 1131)
    // Power=215, Precision=215, Toughness=118, BoonDuration=118
    // Validates FourStat 2H: major=215 (round(717.024*0.3)), minor=118
    //   (round(717.024*0.165))
    // Viper's uses same multipliers (0.3/0.165) as Commander's for
    //   armor/weapons (val=0).
    #[test]
    fn test_four_stat_1h_weapon_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::WeaponOneHand, StatShape::FourStat)
            .expect("WeaponOneHand FourStat missing");
        // Derived from attr_adj=358.512, Viper's stat_id 1153:
        //   major = round(358.512 * 0.3) = round(107.554) = 108
        //   minor = round(358.512 * 0.165) = round(59.154) = 59
        // Cross-verified: 2H values (adj=717.024) confirmed against
        //   API:2/items/74083 (Commander's 2H: 215/118).
        assert_eq!(entry.major, 108);
        assert_eq!(entry.minor, 59);
    }

    // Source: API:2/items/74081 — Attuned Solaria, Circle of the Sun
    //   (Celestial Ascended Ring, stat_id 588)
    // All 9 stats = 57
    // Validates CelestialLike Ring: round(268.884 * 0.165 + 13) = 57
    #[test]
    fn test_celestial_1h_weapon_values() {
        let b = slot_budgets();
        let entry = b
            .get(SlotType::WeaponOneHand, StatShape::CelestialLike)
            .expect("WeaponOneHand CelestialLike missing");
        // Derived from attr_adj=358.512, Celestial stat_id 559:
        //   all = round(358.512 * 0.165) = round(59.154) = 59
        // Cross-verified: Celestial Ring (adj=268.884) confirmed
        //   against API:2/items/74081 (all stats = 57).
        assert_eq!(entry.major, 59);
        assert_eq!(entry.minor, 59);
    }

    #[test]
    fn test_all_12_slots_present_for_each_shape() {
        let b = slot_budgets();
        for shape in &StatShape::ALL {
            for slot in &SlotType::ALL {
                assert!(
                    b.get(*slot, *shape).is_some(),
                    "missing {:?} {:?}",
                    slot,
                    shape
                );
            }
        }
    }

    #[test]
    fn test_missing_slot_rejected() {
        // Build a valid file but remove Helm ThreeStat
        let json = r#"{
            "rarity": "Ascended",
            "level": 80,
            "entries": [
                {"slot":"Shoulders","shape":"ThreeStat","major":47,"minor":34,"evidence_level":"Factual"},
                {"slot":"Coat","shape":"ThreeStat","major":141,"minor":101,"evidence_level":"Factual"},
                {"slot":"Gloves","shape":"ThreeStat","major":47,"minor":34,"evidence_level":"Factual"},
                {"slot":"Leggings","shape":"ThreeStat","major":94,"minor":67,"evidence_level":"Factual"},
                {"slot":"Boots","shape":"ThreeStat","major":47,"minor":34,"evidence_level":"Factual"},
                {"slot":"WeaponOneHand","shape":"ThreeStat","major":125,"minor":90,"evidence_level":"Factual"},
                {"slot":"WeaponTwoHand","shape":"ThreeStat","major":251,"minor":179,"evidence_level":"Factual"},
                {"slot":"Amulet","shape":"ThreeStat","major":157,"minor":108,"evidence_level":"Factual"},
                {"slot":"Accessory","shape":"ThreeStat","major":110,"minor":74,"evidence_level":"Factual"},
                {"slot":"Ring","shape":"ThreeStat","major":126,"minor":85,"evidence_level":"Factual"},
                {"slot":"BackItem","shape":"ThreeStat","major":63,"minor":40,"evidence_level":"Factual"},
                {"slot":"Helm","shape":"FourStat","major":54,"minor":30,"evidence_level":"Factual"},
                {"slot":"Shoulders","shape":"FourStat","major":40,"minor":22,"evidence_level":"Factual"},
                {"slot":"Coat","shape":"FourStat","major":121,"minor":67,"evidence_level":"Factual"},
                {"slot":"Gloves","shape":"FourStat","major":40,"minor":22,"evidence_level":"Factual"},
                {"slot":"Leggings","shape":"FourStat","major":81,"minor":44,"evidence_level":"Factual"},
                {"slot":"Boots","shape":"FourStat","major":40,"minor":22,"evidence_level":"Factual"},
                {"slot":"WeaponOneHand","shape":"FourStat","major":108,"minor":59,"evidence_level":"Factual"},
                {"slot":"WeaponTwoHand","shape":"FourStat","major":215,"minor":118,"evidence_level":"Factual"},
                {"slot":"Amulet","shape":"FourStat","major":133,"minor":71,"evidence_level":"Factual"},
                {"slot":"Accessory","shape":"FourStat","major":92,"minor":49,"evidence_level":"Factual"},
                {"slot":"Ring","shape":"FourStat","major":106,"minor":56,"evidence_level":"Factual"},
                {"slot":"BackItem","shape":"FourStat","major":52,"minor":27,"evidence_level":"Factual"},
                {"slot":"Helm","shape":"CelestialLike","major":30,"minor":30,"evidence_level":"Factual"},
                {"slot":"Shoulders","shape":"CelestialLike","major":22,"minor":22,"evidence_level":"Factual"},
                {"slot":"Coat","shape":"CelestialLike","major":67,"minor":67,"evidence_level":"Factual"},
                {"slot":"Gloves","shape":"CelestialLike","major":22,"minor":22,"evidence_level":"Factual"},
                {"slot":"Leggings","shape":"CelestialLike","major":44,"minor":44,"evidence_level":"Factual"},
                {"slot":"Boots","shape":"CelestialLike","major":22,"minor":22,"evidence_level":"Factual"},
                {"slot":"WeaponOneHand","shape":"CelestialLike","major":59,"minor":59,"evidence_level":"Factual"},
                {"slot":"WeaponTwoHand","shape":"CelestialLike","major":118,"minor":118,"evidence_level":"Factual"},
                {"slot":"Amulet","shape":"CelestialLike","major":72,"minor":72,"evidence_level":"Factual"},
                {"slot":"Accessory","shape":"CelestialLike","major":50,"minor":50,"evidence_level":"Factual"},
                {"slot":"Ring","shape":"CelestialLike","major":57,"minor":57,"evidence_level":"Factual"},
                {"slot":"BackItem","shape":"CelestialLike","major":28,"minor":28,"evidence_level":"Factual"}
            ]
        }"#;
        let err = load_slot_budgets(json).unwrap_err();
        assert!(
            err.to_string().contains("missing entry: Helm ThreeStat"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_duplicate_slot_shape_rejected() {
        // Include Helm ThreeStat twice
        let json = r#"{
            "rarity": "Ascended",
            "level": 80,
            "entries": [
                {"slot":"Helm","shape":"ThreeStat","major":63,"minor":45,"evidence_level":"Factual"},
                {"slot":"Helm","shape":"ThreeStat","major":63,"minor":45,"evidence_level":"Factual"},
                {"slot":"Shoulders","shape":"ThreeStat","major":47,"minor":34,"evidence_level":"Factual"},
                {"slot":"Coat","shape":"ThreeStat","major":141,"minor":101,"evidence_level":"Factual"},
                {"slot":"Gloves","shape":"ThreeStat","major":47,"minor":34,"evidence_level":"Factual"},
                {"slot":"Leggings","shape":"ThreeStat","major":94,"minor":67,"evidence_level":"Factual"},
                {"slot":"Boots","shape":"ThreeStat","major":47,"minor":34,"evidence_level":"Factual"},
                {"slot":"WeaponOneHand","shape":"ThreeStat","major":125,"minor":90,"evidence_level":"Factual"},
                {"slot":"WeaponTwoHand","shape":"ThreeStat","major":251,"minor":179,"evidence_level":"Factual"},
                {"slot":"Amulet","shape":"ThreeStat","major":157,"minor":108,"evidence_level":"Factual"},
                {"slot":"Accessory","shape":"ThreeStat","major":110,"minor":74,"evidence_level":"Factual"},
                {"slot":"Ring","shape":"ThreeStat","major":126,"minor":85,"evidence_level":"Factual"},
                {"slot":"BackItem","shape":"ThreeStat","major":63,"minor":40,"evidence_level":"Factual"}
            ]
        }"#;
        let err = load_slot_budgets(json).unwrap_err();
        assert!(
            err.to_string().contains("duplicate entry: Helm ThreeStat"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_zero_value_rejected() {
        let json = r#"{
            "rarity": "Ascended",
            "level": 80,
            "entries": [
                {"slot":"Helm","shape":"ThreeStat","major":0,"minor":45,"evidence_level":"Factual"}
            ]
        }"#;
        let err = load_slot_budgets(json).unwrap_err();
        assert!(
            err.to_string().contains("zero major"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_zero_minor_rejected() {
        let json = r#"{
            "rarity": "Ascended",
            "level": 80,
            "entries": [
                {"slot":"Helm","shape":"ThreeStat","major":63,"minor":0,"evidence_level":"Factual"}
            ]
        }"#;
        let err = load_slot_budgets(json).unwrap_err();
        assert!(
            err.to_string().contains("zero minor"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_malformed_shape_rejected() {
        let json = r#"{
            "rarity": "Ascended",
            "level": 80,
            "entries": [
                {"slot":"Helm","shape":"FiveStat","major":63,"minor":45,"evidence_level":"Factual"}
            ]
        }"#;
        let err = load_slot_budgets(json).unwrap_err();
        assert!(matches!(err, SlotBudgetError::ParseError(_)));
    }
}
