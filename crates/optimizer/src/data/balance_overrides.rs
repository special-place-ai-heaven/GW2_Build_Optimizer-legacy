use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use thiserror::Error;

use super::{try_load, DataLoadError, EvidenceLevel};

// Embedded baseline JSON (compile-time)

const PVE_OVERRIDES_JSON: &str =
    include_str!("../../../../data/balance_overrides/2026-07-15/pve.json");
const PVP_OVERRIDES_JSON: &str =
    include_str!("../../../../data/balance_overrides/2026-07-15/pvp.json");
const WVW_OVERRIDES_JSON: &str =
    include_str!("../../../../data/balance_overrides/2026-07-15/wvw.json");

static OVERRIDES: OnceLock<BalanceOverrides> = OnceLock::new();

/// Returns the globally loaded balance overrides, parsing on first access.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data, should never happen).
pub fn overrides() -> &'static BalanceOverrides {
    OVERRIDES
        .get_or_init(|| load_all_overrides().expect("embedded balance_overrides JSON is invalid"))
}

/// Try to load all balance overrides from the embedded JSON, returning typed errors
/// on failure. Does NOT store in OnceLock — used for health-check validation.
pub fn try_load_balance_overrides() -> Result<BalanceOverrides, Vec<DataLoadError>> {
    try_load!(
        "balance_overrides",
        load_all_overrides(),
        BalanceOverrideError
    )
}

#[derive(Debug, Error)]
pub enum BalanceOverrideError {
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    ValidationError(String),
}

// Override file schema

/// A single override file for one game mode in a specific patch.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideFile {
    pub patch_id: String,
    pub mode: String,
    pub entities: Vec<OverrideEntity>,
}

/// An entity (skill, trait, profession mechanic, etc.) with field overrides.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideEntity {
    /// Type of the source entity (e.g., "Skill", "Trait", "Profession").
    pub source_type: String,
    /// GW2 API ID of the entity.
    pub source_id: u32,
    /// Human-readable name for logging/debugging.
    pub name: String,
    /// Per-field overrides. Keys are field names (e.g., "coefficient",
    /// "hit_count", "damage_coefficient:above_50").
    pub overrides: HashMap<String, OverrideEntry>,
}

/// A single field override with its value, evidence level, and optional source.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideEntry {
    /// The override value. `None` means explicitly Unknown (value cannot be determined).
    /// `Some(f64)` means a known override value.
    pub value: Option<f64>,
    /// Evidence level for this override.
    pub evidence_level: EvidenceLevel,
    /// Optional source citation (wiki URL, patch notes, etc.).
    #[serde(default)]
    pub source: Option<String>,
}

// Lookup result

/// Result of looking up an override. Distinguishes between:
/// - No override exists (None from lookup) → no quality degradation
/// - Override exists with a known value → OverrideResult::Value
/// - Override exists but value is Unknown → OverrideResult::Unknown
#[derive(Debug, Clone, PartialEq)]
pub enum OverrideResult {
    /// A known override value with its evidence level.
    Value {
        value: f64,
        evidence_level: EvidenceLevel,
    },
    /// The value is explicitly unknown — degrades DataQuality.
    Unknown { evidence_level: EvidenceLevel },
}

// BalanceOverrides container

/// Container for all loaded balance overrides, keyed by (patch_id, mode).
#[derive(Debug)]
pub struct BalanceOverrides {
    /// Map from (patch_id, mode) to parsed override file.
    files: HashMap<(String, String), OverrideFile>,
}

impl BalanceOverrides {
    /// Look up a specific override.
    ///
    /// Returns:
    /// - `None` if no override exists for this entity/field combination.
    ///   This means the default value should be used with no quality degradation.
    /// - `Some(OverrideResult::Value { .. })` if an override with a known value exists.
    /// - `Some(OverrideResult::Unknown { .. })` if the value is explicitly unknown.
    pub fn lookup(
        &self,
        patch_id: &str,
        mode: &str,
        source_type: &str,
        source_id: u32,
        field: &str,
    ) -> Option<OverrideResult> {
        let file = self.files.get(&(patch_id.to_string(), mode.to_string()))?;
        let entity = file
            .entities
            .iter()
            .find(|e| e.source_type == source_type && e.source_id == source_id)?;
        let entry = entity.overrides.get(field)?;

        Some(match entry.value {
            Some(v) => OverrideResult::Value {
                value: v,
                evidence_level: entry.evidence_level.clone(),
            },
            None => OverrideResult::Unknown {
                evidence_level: entry.evidence_level.clone(),
            },
        })
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Total number of entities across all override files.
    pub fn entity_count(&self) -> usize {
        self.files.values().map(|f| f.entities.len()).sum()
    }
}

/// Parse and validate a single override file from JSON text.
pub fn load_override_file(json: &str) -> Result<OverrideFile, BalanceOverrideError> {
    let file: OverrideFile = serde_json::from_str(json)?;
    validate_override_file(&file)?;
    Ok(file)
}

fn validate_override_file(file: &OverrideFile) -> Result<(), BalanceOverrideError> {
    if file.patch_id.is_empty() {
        return Err(BalanceOverrideError::ValidationError(
            "patch_id must not be empty".into(),
        ));
    }

    let valid_modes = ["PvE", "PvP", "WvW"];
    if !valid_modes.contains(&file.mode.as_str()) {
        return Err(BalanceOverrideError::ValidationError(format!(
            "invalid mode '{}', expected one of: PvE, PvP, WvW",
            file.mode
        )));
    }

    // Validate no duplicate entity (source_type, source_id) pairs
    let mut seen = std::collections::HashSet::new();
    for entity in &file.entities {
        let key = (entity.source_type.clone(), entity.source_id);
        if !seen.insert(key) {
            return Err(BalanceOverrideError::ValidationError(format!(
                "duplicate entity: {} (id {})",
                entity.source_type, entity.source_id
            )));
        }
        if entity.name.is_empty() {
            return Err(BalanceOverrideError::ValidationError(format!(
                "entity {} (id {}) has empty name",
                entity.source_type, entity.source_id
            )));
        }
    }

    Ok(())
}

/// Load and merge all three baseline override files.
fn load_all_overrides() -> Result<BalanceOverrides, BalanceOverrideError> {
    let mut files = HashMap::new();

    for (json, expected_mode) in [
        (PVE_OVERRIDES_JSON, "PvE"),
        (PVP_OVERRIDES_JSON, "PvP"),
        (WVW_OVERRIDES_JSON, "WvW"),
    ] {
        let file = load_override_file(json)?;

        // Validate mode matches expected
        if file.mode != expected_mode {
            return Err(BalanceOverrideError::ValidationError(format!(
                "expected mode '{}', got '{}'",
                expected_mode, file.mode
            )));
        }

        files.insert((file.patch_id.clone(), file.mode.clone()), file);
    }

    Ok(BalanceOverrides { files })
}

// Known Mode Splits

/// A coefficient known to differ between game modes.
/// Used to detect when a WvW-specific value is missing from the override system.
#[derive(Debug, Clone)]
pub struct KnownModeSplit {
    /// Type of entity (e.g., "Boon", "Trait", "Skill").
    pub source_type: &'static str,
    /// Entity name for human-readable output.
    pub entity_name: &'static str,
    /// Field that differs between modes.
    pub field: &'static str,
    /// Brief description of the split.
    pub description: &'static str,
    /// Whether the split is already handled in Phase A data (boon_condition_formulas, etc.).
    /// If true, the override system does not need to carry this value.
    pub handled_in_phase_a: bool,
}

/// Returns the list of coefficients known to differ between PvE and PvP/WvW.
///
/// This is the canonical reference for mode-split awareness. Each entry documents
/// a coefficient where ArenaNet has confirmed different values per mode.
///
/// Entries marked `handled_in_phase_a: true` are already resolved in the boon/condition
/// formula data files and do not require balance_overrides entries.
pub fn known_mode_splits() -> &'static [KnownModeSplit] {
    static SPLITS: &[KnownModeSplit] = &[
        KnownModeSplit {
            source_type: "Boon",
            entity_name: "Fury",
            field: "crit_chance_bonus",
            description: "PvE: 25%, PvP/WvW: 20%",
            handled_in_phase_a: true,
        },
        KnownModeSplit {
            source_type: "Condition",
            entity_name: "Torment",
            field: "base_per_tick",
            description: "PvE stationary: 31.8, PvP/WvW stationary: 26.0",
            handled_in_phase_a: true,
        },
        KnownModeSplit {
            source_type: "Condition",
            entity_name: "Torment",
            field: "condition_damage_coeff",
            description: "PvE stationary: 0.09, PvP/WvW stationary: 0.07",
            handled_in_phase_a: true,
        },
        KnownModeSplit {
            source_type: "Condition",
            entity_name: "Confusion",
            field: "base_per_tick",
            description: "PvE over_time: 18.25, PvP/WvW over_time: 10.0 (flat)",
            handled_in_phase_a: true,
        },
        KnownModeSplit {
            source_type: "Condition",
            entity_name: "Confusion",
            field: "condition_damage_coeff",
            description: "PvE on_skill_use: 0.0325, PvP/WvW on_skill_use: 0.0975",
            handled_in_phase_a: true,
        },
        // Future entries for trait/skill coefficient splits discovered in P3-13
        // would be added here with handled_in_phase_a: false.
    ];
    SPLITS
}

#[cfg(test)]
mod tests {
    use super::*;

    // Embedded baseline loads successfully

    #[test]
    fn test_embedded_overrides_load_successfully() {
        let o = overrides();
        assert_eq!(
            o.file_count(),
            3,
            "expected 3 override files (PvE, PvP, WvW)"
        );
        assert_eq!(
            o.entity_count(),
            54,
            "twelve sourced skills and six sourced traits per mode"
        );

        assert!(matches!(
            o.lookup("2026-07-15", "WvW", "Skill", 13113, "initiative_cost"),
            Some(OverrideResult::Value { value: 7.0, .. })
        ));
        assert!(matches!(
            o.lookup("2026-07-15", "PvE", "Skill", 9168, "hit_count"),
            Some(OverrideResult::Value { value: 4.0, .. })
        ));
        assert!(matches!(
            o.lookup(
                "2026-07-15",
                "PvE",
                "Skill",
                9168,
                "damage_coefficient:above_50"
            ),
            Some(OverrideResult::Value { value: 0.8, .. })
        ));
        for mode in ["PvE", "PvP", "WvW"] {
            assert!(
                matches!(
                    o.lookup("2026-07-15", mode, "Skill", 9081, "hit_count"),
                    Some(OverrideResult::Value { value: 2.0, .. })
                ),
                "{mode} Whirling Wrath hit_count"
            );
        }
    }

    #[test]
    fn test_try_load_returns_ok() {
        let result = try_load_balance_overrides();
        assert!(
            result.is_ok(),
            "try_load should succeed: {:?}",
            result.err()
        );
    }

    // Lookup on empty baseline returns None

    #[test]
    fn test_lookup_empty_baseline_returns_none() {
        let o = overrides();
        assert_eq!(
            o.lookup("2026-01-13", "PvE", "Skill", 1234, "coefficient"),
            None,
            "empty baseline should return None for any lookup"
        );
    }

    #[test]
    fn test_lookup_unknown_patch_returns_none() {
        let o = overrides();
        assert_eq!(
            o.lookup("9999-99-99", "PvE", "Skill", 1234, "coefficient"),
            None,
        );
    }

    #[test]
    fn test_lookup_unknown_mode_returns_none() {
        let o = overrides();
        assert_eq!(
            o.lookup("2026-01-13", "UnknownMode", "Skill", 1234, "coefficient"),
            None,
        );
    }

    // Parsing tests with inline JSON

    #[test]
    fn test_parse_valid_override_file() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvP",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 5678,
                    "name": "Fireball",
                    "overrides": {
                        "coefficient": {
                            "value": 0.75,
                            "evidence_level": "Factual",
                            "source": "https://wiki.guildwars2.com/wiki/Fireball"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(json).expect("should parse");
        assert_eq!(file.patch_id, "2026-01-13");
        assert_eq!(file.mode, "PvP");
        assert_eq!(file.entities.len(), 1);
        assert_eq!(file.entities[0].name, "Fireball");
        assert_eq!(file.entities[0].source_id, 5678);
        let coeff = file.entities[0].overrides.get("coefficient").unwrap();
        assert_eq!(coeff.value, Some(0.75));
        assert_eq!(coeff.evidence_level, EvidenceLevel::Factual);
    }

    #[test]
    fn test_parse_unknown_value_override() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "WvW",
            "entities": [
                {
                    "source_type": "Trait",
                    "source_id": 999,
                    "name": "Unknown Trait",
                    "overrides": {
                        "coefficient": {
                            "value": null,
                            "evidence_level": "Unknown"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(json).expect("should parse");
        let coeff = file.entities[0].overrides.get("coefficient").unwrap();
        assert_eq!(coeff.value, None);
        assert_eq!(coeff.evidence_level, EvidenceLevel::Unknown);
    }

    // Lookup with populated overrides

    #[test]
    fn test_lookup_value_override() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvP",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 5678,
                    "name": "Fireball",
                    "overrides": {
                        "coefficient": {
                            "value": 0.75,
                            "evidence_level": "Factual"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(json).unwrap();
        let mut files = HashMap::new();
        files.insert((file.patch_id.clone(), file.mode.clone()), file);
        let overrides = BalanceOverrides { files };

        // Existing override returns Value
        let result = overrides.lookup("2026-01-13", "PvP", "Skill", 5678, "coefficient");
        assert_eq!(
            result,
            Some(OverrideResult::Value {
                value: 0.75,
                evidence_level: EvidenceLevel::Factual,
            }),
        );

        // Different field returns None (no override, no degradation)
        assert_eq!(
            overrides.lookup("2026-01-13", "PvP", "Skill", 5678, "base_damage"),
            None,
        );

        // Different entity returns None
        assert_eq!(
            overrides.lookup("2026-01-13", "PvP", "Skill", 9999, "coefficient"),
            None,
        );
    }

    #[test]
    fn test_lookup_unknown_override() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "WvW",
            "entities": [
                {
                    "source_type": "Trait",
                    "source_id": 999,
                    "name": "Mystery Trait",
                    "overrides": {
                        "coefficient": {
                            "value": null,
                            "evidence_level": "Unknown"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(json).unwrap();
        let mut files = HashMap::new();
        files.insert((file.patch_id.clone(), file.mode.clone()), file);
        let overrides = BalanceOverrides { files };

        // Unknown override returns OverrideResult::Unknown (degrades quality)
        let result = overrides.lookup("2026-01-13", "WvW", "Trait", 999, "coefficient");
        assert_eq!(
            result,
            Some(OverrideResult::Unknown {
                evidence_level: EvidenceLevel::Unknown,
            }),
        );
    }

    // Error paths

    #[test]
    fn test_malformed_json_returns_error() {
        let result = load_override_file("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    #[test]
    fn test_empty_patch_id_rejected() {
        let json = r#"{
            "patch_id": "",
            "mode": "PvE",
            "entities": []
        }"#;
        let result = load_override_file(json);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("patch_id must not be empty"));
    }

    #[test]
    fn test_invalid_mode_rejected() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "Ranked",
            "entities": []
        }"#;
        let result = load_override_file(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid mode"));
    }

    #[test]
    fn test_duplicate_entity_rejected() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvE",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 100,
                    "name": "Skill A",
                    "overrides": {}
                },
                {
                    "source_type": "Skill",
                    "source_id": 100,
                    "name": "Skill A Duplicate",
                    "overrides": {}
                }
            ]
        }"#;
        let result = load_override_file(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate entity"));
    }

    #[test]
    fn test_empty_entity_name_rejected() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvE",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 100,
                    "name": "",
                    "overrides": {}
                }
            ]
        }"#;
        let result = load_override_file(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty name"));
    }

    // None vs Unknown semantics

    #[test]
    fn test_none_vs_unknown_semantics() {
        // None from lookup = no override exists, use default, no quality degradation
        // Some(Unknown) = override exists but value can't be determined, degrades quality
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvP",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 42,
                    "name": "Test Skill",
                    "overrides": {
                        "known_field": {
                            "value": 1.5,
                            "evidence_level": "Factual"
                        },
                        "unknown_field": {
                            "value": null,
                            "evidence_level": "Unknown"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(json).unwrap();
        let mut files = HashMap::new();
        files.insert((file.patch_id.clone(), file.mode.clone()), file);
        let o = BalanceOverrides { files };

        // Known override → Value
        assert!(matches!(
            o.lookup("2026-01-13", "PvP", "Skill", 42, "known_field"),
            Some(OverrideResult::Value { value: 1.5, .. }),
        ));

        // Explicitly unknown override → Unknown (degrades quality)
        assert!(matches!(
            o.lookup("2026-01-13", "PvP", "Skill", 42, "unknown_field"),
            Some(OverrideResult::Unknown { .. }),
        ));

        // No override at all → None (no degradation, use default)
        assert_eq!(
            o.lookup("2026-01-13", "PvP", "Skill", 42, "unset_field"),
            None,
        );
    }

    // P3-12: WvW Non-Fallback Integration Tests

    /// WvW lookup with a WvW-specific override returns the WvW value.
    #[test]
    fn test_wvw_uses_wvw_specific_override() {
        let wvw_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "WvW",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 1001,
                    "name": "Whirling Axe",
                    "overrides": {
                        "coefficient": {
                            "value": 0.55,
                            "evidence_level": "Factual"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(wvw_json).unwrap();
        let mut files = HashMap::new();
        files.insert((file.patch_id.clone(), file.mode.clone()), file);
        let overrides = BalanceOverrides { files };

        let result = overrides.lookup("2026-01-13", "WvW", "Skill", 1001, "coefficient");
        assert_eq!(
            result,
            Some(OverrideResult::Value {
                value: 0.55,
                evidence_level: EvidenceLevel::Factual,
            }),
            "WvW lookup should return the WvW-specific value",
        );
    }

    /// When WvW has no known split, lookup returns None and the caller keeps the base value.
    #[test]
    fn test_wvw_no_known_split_uses_base_value() {
        let wvw_json = r#"{
            "patch_id": "test-patch",
            "mode": "WvW",
            "entities": []
        }"#;
        let file = load_override_file(wvw_json).unwrap();
        let mut files = HashMap::new();
        files.insert((file.patch_id.clone(), file.mode.clone()), file);
        let overrides = BalanceOverrides { files };

        let result = overrides.lookup("test-patch", "WvW", "Skill", 9999, "coefficient");
        assert_eq!(result, None, "non-split coefficient should return None");
    }

    /// When a WvW override exists but the value is explicitly Unknown, quality degrades.
    #[test]
    fn test_wvw_explicit_unknown_override_degrades_quality() {
        let wvw_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "WvW",
            "entities": [
                {
                    "source_type": "Trait",
                    "source_id": 500,
                    "name": "Radiant Power",
                    "overrides": {
                        "coefficient": {
                            "value": null,
                            "evidence_level": "Unknown"
                        }
                    }
                }
            ]
        }"#;
        let file = load_override_file(wvw_json).unwrap();
        let mut files = HashMap::new();
        files.insert((file.patch_id.clone(), file.mode.clone()), file);
        let overrides = BalanceOverrides { files };

        // The override is explicitly Unknown — should degrade quality
        let result = overrides.lookup("2026-01-13", "WvW", "Trait", 500, "coefficient");
        assert_eq!(
            result,
            Some(OverrideResult::Unknown {
                evidence_level: EvidenceLevel::Unknown,
            }),
            "explicitly Unknown WvW override should return Unknown variant",
        );
    }

    /// WvW lookup NEVER falls back to PvE data. PvE override exists but WvW doesn't.
    #[test]
    fn test_wvw_never_falls_back_to_pve() {
        // Set up PvE override only — no WvW override
        let pve_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvE",
            "entities": [
                {
                    "source_type": "Skill",
                    "source_id": 2001,
                    "name": "Meteor Shower",
                    "overrides": {
                        "coefficient": {
                            "value": 1.25,
                            "evidence_level": "Factual"
                        }
                    }
                }
            ]
        }"#;
        let wvw_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "WvW",
            "entities": []
        }"#;

        let pve_file = load_override_file(pve_json).unwrap();
        let wvw_file = load_override_file(wvw_json).unwrap();
        let mut files = HashMap::new();
        files.insert((pve_file.patch_id.clone(), pve_file.mode.clone()), pve_file);
        files.insert((wvw_file.patch_id.clone(), wvw_file.mode.clone()), wvw_file);
        let overrides = BalanceOverrides { files };

        // PvE should find the override
        let pve_result = overrides.lookup("2026-01-13", "PvE", "Skill", 2001, "coefficient");
        assert_eq!(
            pve_result,
            Some(OverrideResult::Value {
                value: 1.25,
                evidence_level: EvidenceLevel::Factual,
            }),
            "PvE lookup should find the PvE override",
        );

        // WvW must NOT fall back to PvE — should return None
        let wvw_result = overrides.lookup("2026-01-13", "WvW", "Skill", 2001, "coefficient");
        assert_eq!(
            wvw_result, None,
            "WvW lookup must return None, NOT the PvE value — no cross-mode fallback",
        );
    }

    /// Verify that lookup is strictly keyed by (patch_id, mode) — no cross-mode contamination.
    #[test]
    fn test_override_lookup_is_mode_isolated() {
        let pve_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvE",
            "entities": [
                {
                    "source_type": "Trait",
                    "source_id": 100,
                    "name": "PvE Trait",
                    "overrides": {
                        "damage_mult": { "value": 1.5, "evidence_level": "Factual" }
                    }
                }
            ]
        }"#;
        let pvp_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvP",
            "entities": [
                {
                    "source_type": "Trait",
                    "source_id": 100,
                    "name": "PvP Trait",
                    "overrides": {
                        "damage_mult": { "value": 0.75, "evidence_level": "Factual" }
                    }
                }
            ]
        }"#;
        let wvw_json = r#"{
            "patch_id": "2026-01-13",
            "mode": "WvW",
            "entities": [
                {
                    "source_type": "Trait",
                    "source_id": 100,
                    "name": "WvW Trait",
                    "overrides": {
                        "damage_mult": { "value": 0.80, "evidence_level": "Factual" }
                    }
                }
            ]
        }"#;

        let pve = load_override_file(pve_json).unwrap();
        let pvp = load_override_file(pvp_json).unwrap();
        let wvw = load_override_file(wvw_json).unwrap();
        let mut files = HashMap::new();
        files.insert((pve.patch_id.clone(), pve.mode.clone()), pve);
        files.insert((pvp.patch_id.clone(), pvp.mode.clone()), pvp);
        files.insert((wvw.patch_id.clone(), wvw.mode.clone()), wvw);
        let overrides = BalanceOverrides { files };

        // Each mode returns its own value, never another mode's
        assert_eq!(
            overrides.lookup("2026-01-13", "PvE", "Trait", 100, "damage_mult"),
            Some(OverrideResult::Value {
                value: 1.5,
                evidence_level: EvidenceLevel::Factual
            }),
        );
        assert_eq!(
            overrides.lookup("2026-01-13", "PvP", "Trait", 100, "damage_mult"),
            Some(OverrideResult::Value {
                value: 0.75,
                evidence_level: EvidenceLevel::Factual
            }),
        );
        assert_eq!(
            overrides.lookup("2026-01-13", "WvW", "Trait", 100, "damage_mult"),
            Some(OverrideResult::Value {
                value: 0.80,
                evidence_level: EvidenceLevel::Factual
            }),
        );
    }

    /// Verify known_mode_splits returns a non-empty list with all Phase A entries handled.
    #[test]
    fn test_known_mode_splits_baseline() {
        let splits = known_mode_splits();
        assert!(
            !splits.is_empty(),
            "known_mode_splits should contain at least the Fury/Torment/Confusion entries",
        );
        // All baseline entries should be handled_in_phase_a
        for split in splits {
            assert!(
                split.handled_in_phase_a,
                "Baseline split {}.{} should be handled_in_phase_a",
                split.entity_name, split.field,
            );
        }
    }
}
