use serde::{Deserialize, Serialize};

pub mod balance_overrides;
pub mod boon_condition_formulas;
pub mod cleanse_sources;
pub mod combos;
#[cfg(test)]
mod consistency_tests;
pub mod effect_coverage;
pub mod fight_population;
pub mod hit_timing;
pub mod manifests;
pub mod modifier_buckets;
pub mod normalized_effects;
pub mod objective_profiles;
#[cfg(test)]
pub mod patch_ledger;
pub mod profession_profiles;
pub mod quality;
pub mod rotation_profiles;
pub mod shroud;
pub mod slot_budgets;
pub mod stunbreak_sources;
pub mod universal_formulas;
pub mod weapon_hands;

pub use balance_overrides::{known_mode_splits, BalanceOverrides, KnownModeSplit, OverrideResult};
pub use boon_condition_formulas::{boons, conditions, BoonFormulas, ConditionFormulas};
pub use cleanse_sources::{CleanseRegistry, CleanseSource, SourceKind};
pub use manifests::{check_staleness, freshness, ManifestFreshness, PatchManifest};
pub use normalized_effects::{
    EffectCategory, NormalizedEffect, SourceType, StackingRule, StatusOperation, TriggerRule,
    UptimeModel,
};
pub use objective_profiles::{ObjectiveProfile, ObjectiveProfileData, ObjectiveProfileFile};
pub use profession_profiles::ProfessionProfiles;
pub use quality::{DataQuality, DataQualityReason, FactualValue};
pub use rotation_profiles::{
    ApplicationMetrics, BuffProfileFromScenario, ConditionWeightsFromProfile, GenerationMetrics,
    RotationProfile, RotationProfileData, ScenarioProfile, TargetBehavior,
};
pub use slot_budgets::{
    stat_shape_from_attr_count, SlotBudgets, SlotType, StatShape, EQUIPMENT_SLOTS,
};
pub use universal_formulas::UniversalFormulas;

/// Evidence level for data entries. Shared across all data loaders.
/// - Factual: directly from wiki or game data with exact values.
/// - Derived: calculated from factual data using known formulas.
/// - Heuristic: empirically tuned or estimated values.
/// - Unknown: unverified or placeholder values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EvidenceLevel {
    Factual,
    Derived,
    Heuristic,
    Unknown,
}

// Error and State Types

/// Typed error for data loading failures. Used by `try_load()` functions
/// and the `initialize()` health check.
#[derive(Debug, Clone)]
pub enum DataLoadError {
    /// JSON or format parse failure.
    ParseError { source: String, detail: String },
    /// A field value fails validation constraints.
    ValidationError {
        source: String,
        field: String,
        reason: String,
    },
    /// A required data source is missing or empty.
    MissingRequired { source: String },
}

impl std::fmt::Display for DataLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError { source, detail } => {
                write!(f, "Parse error in {}: {}", source, detail)
            }
            Self::ValidationError {
                source,
                field,
                reason,
            } => {
                write!(
                    f,
                    "Validation error in {} field '{}': {}",
                    source, field, reason
                )
            }
            Self::MissingRequired { source } => {
                write!(f, "Missing required data: {}", source)
            }
        }
    }
}

/// Maps a per-dataset load error to a `Vec<DataLoadError>` with `source` tagged.
///
/// Every data loader's error enum has the same shape: `ParseError(impl ToString)`
/// and `ValidationError(String)`. This macro folds that shape into a single line
/// at each `try_load_*` call site.
macro_rules! try_load {
    ($source:literal, $inner:expr, $err_ty:ident) => {
        $inner.map_err(|e| {
            vec![match e {
                $err_ty::ParseError(pe) => $crate::data::DataLoadError::ParseError {
                    source: $source.into(),
                    detail: pe.to_string(),
                },
                $err_ty::ValidationError(msg) => $crate::data::DataLoadError::ValidationError {
                    source: $source.into(),
                    field: String::new(),
                    reason: msg,
                },
            }]
        })
    };
}
pub(crate) use try_load;

/// Overall health of loaded data, returned by `initialize()`.
#[derive(Debug, Clone)]
pub enum DataState {
    /// All required data loaded and validated successfully.
    Ready,
    /// Required data loaded but optional data missing or degraded.
    Degraded { reasons: Vec<String> },
    /// Required Phase A data failed to load — optimizer cannot function.
    Disabled { errors: Vec<DataLoadError> },
}

impl DataState {
    /// `Disabled` is a hard stop. `Degraded` is optional-data loss, not a fake Ready.
    pub fn optimize_block_reason(&self) -> Option<String> {
        match self {
            Self::Disabled { errors } => Some(format!(
                "Balance data failed to load: {}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
            Self::Ready | Self::Degraded { .. } => None,
        }
    }
}

/// Load all Phase A data and return overall health status.
///
/// Calls each loader's `try_load()` function. If any required loader fails,
/// returns `Disabled`. Currently all Phase A loaders are required, so any
/// failure is `Disabled`. Future phases (P3-08+) may introduce optional
/// loaders that produce `Degraded` instead.
pub fn initialize() -> DataState {
    let mut errors = Vec::new();

    if let Err(errs) = profession_profiles::try_load_profession_profiles() {
        errors.extend(errs);
    }
    if let Err(errs) = universal_formulas::try_load_universal_formulas() {
        errors.extend(errs);
    }
    if let Err(errs) = boon_condition_formulas::try_load_boon_formulas() {
        errors.extend(errs);
    }
    if let Err(errs) = boon_condition_formulas::try_load_condition_formulas() {
        errors.extend(errs);
    }
    if let Err(errs) = slot_budgets::try_load_slot_budgets() {
        errors.extend(errs);
    }
    if let Err(errs) = balance_overrides::try_load_balance_overrides() {
        errors.extend(errs);
    }
    if let Err(errs) = normalized_effects::try_load_normalized_effects() {
        errors.extend(errs);
    }
    if let Err(errs) = rotation_profiles::try_load_rotation_profiles() {
        errors.extend(errs);
    }
    if let Err(errs) = objective_profiles::try_load_objective_profiles() {
        errors.extend(errs);
    }
    if let Err(errs) = cleanse_sources::try_load_cleanse_sources() {
        errors.extend(errs);
    }
    if let Err(errs) = combos::try_load_combos() {
        errors.extend(errs);
    }
    if let Err(errs) = weapon_hands::try_load_weapon_hands() {
        errors.extend(errs);
    }
    if let Err(errs) = stunbreak_sources::try_load_stunbreak_sources() {
        errors.extend(errs);
    }

    if errors.is_empty() {
        DataState::Ready
    } else {
        DataState::Disabled { errors }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_returns_ready() {
        // All embedded Phase A data is valid, so initialize() should return Ready.
        let state = initialize();
        assert!(
            matches!(state, DataState::Ready),
            "expected DataState::Ready, got {:?}",
            state,
        );
        assert!(state.optimize_block_reason().is_none());
    }

    #[test]
    fn disabled_blocks_optimize_and_is_not_ready() {
        let state = DataState::Disabled {
            errors: vec![DataLoadError::MissingRequired {
                source: "test".into(),
            }],
        };
        let reason = state.optimize_block_reason().expect("Disabled must block");
        assert!(reason.contains("test"));
        assert!(DataState::Ready.optimize_block_reason().is_none());
        assert!(DataState::Degraded {
            reasons: vec!["optional".into()]
        }
        .optimize_block_reason()
        .is_none());
    }

    #[test]
    fn test_data_load_error_display_parse() {
        let err = DataLoadError::ParseError {
            source: "test_source".into(),
            detail: "bad JSON".into(),
        };
        assert_eq!(err.to_string(), "Parse error in test_source: bad JSON");
    }

    #[test]
    fn test_data_load_error_display_validation() {
        let err = DataLoadError::ValidationError {
            source: "test_source".into(),
            field: "some_field".into(),
            reason: "value out of range".into(),
        };
        assert_eq!(
            err.to_string(),
            "Validation error in test_source field 'some_field': value out of range"
        );
    }

    #[test]
    fn test_data_load_error_display_missing() {
        let err = DataLoadError::MissingRequired {
            source: "test_source".into(),
        };
        assert_eq!(err.to_string(), "Missing required data: test_source");
    }

    // Error-path tests for each loader's try_load()

    #[test]
    fn test_try_load_slot_budgets_malformed_json() {
        // Feed malformed JSON to the slot budgets loader.
        let result = slot_budgets::load_slot_budgets("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    #[test]
    fn test_try_load_profession_profiles_malformed_json() {
        let result = profession_profiles::load_profession_profiles("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    #[test]
    fn test_try_load_universal_formulas_malformed_json() {
        let result = universal_formulas::load_universal_formulas("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    #[test]
    fn test_try_load_boon_formulas_malformed_json() {
        let result = boon_condition_formulas::load_boon_formulas("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    #[test]
    fn test_try_load_condition_formulas_malformed_json() {
        let result = boon_condition_formulas::load_condition_formulas("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    #[test]
    fn test_equipment_slots_count() {
        // Full Ascended set: 6 armor + 4 weapons + 6 trinkets = 16 slots.
        assert_eq!(EQUIPMENT_SLOTS.len(), 16);
    }

    #[test]
    fn test_stat_shape_from_attr_count() {
        assert!(matches!(
            stat_shape_from_attr_count(3),
            StatShape::ThreeStat
        ));
        assert!(matches!(stat_shape_from_attr_count(4), StatShape::FourStat));
        assert!(matches!(
            stat_shape_from_attr_count(7),
            StatShape::CelestialLike
        ));
        assert!(matches!(
            stat_shape_from_attr_count(9),
            StatShape::CelestialLike
        ));
        // Edge cases fall back to ThreeStat
        assert!(matches!(
            stat_shape_from_attr_count(1),
            StatShape::ThreeStat
        ));
        assert!(matches!(
            stat_shape_from_attr_count(5),
            StatShape::ThreeStat
        ));
    }

    #[test]
    fn test_slot_type_from_api_slot() {
        assert_eq!(SlotType::from_api_slot("Helm"), Some(SlotType::Helm));
        assert_eq!(SlotType::from_api_slot("Coat"), Some(SlotType::Coat));
        assert_eq!(
            SlotType::from_api_slot("WeaponA1"),
            Some(SlotType::WeaponTwoHand)
        );
        assert_eq!(
            SlotType::from_api_slot("WeaponA2"),
            Some(SlotType::WeaponOneHand)
        );
        assert_eq!(
            SlotType::from_api_slot("WeaponB1"),
            Some(SlotType::WeaponTwoHand)
        );
        assert_eq!(
            SlotType::from_api_slot("WeaponB2"),
            Some(SlotType::WeaponOneHand)
        );
        assert_eq!(
            SlotType::from_api_slot("Backpack"),
            Some(SlotType::BackItem)
        );
        assert_eq!(
            SlotType::from_api_slot("Accessory1"),
            Some(SlotType::Accessory)
        );
        assert_eq!(
            SlotType::from_api_slot("Accessory2"),
            Some(SlotType::Accessory)
        );
        assert_eq!(SlotType::from_api_slot("Amulet"), Some(SlotType::Amulet));
        assert_eq!(SlotType::from_api_slot("Ring1"), Some(SlotType::Ring));
        assert_eq!(SlotType::from_api_slot("Ring2"), Some(SlotType::Ring));
        assert_eq!(SlotType::from_api_slot("Unknown"), None);
    }
}
