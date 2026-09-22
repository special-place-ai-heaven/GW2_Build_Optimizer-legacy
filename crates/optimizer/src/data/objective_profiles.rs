//! Objective profiles: per-mode heuristic scoring configurations.
//!
//! Defines 6-axis optimization profiles with typed boon, condition, and
//! interaction priorities. Replaces the hardcoded `PRESETS`, `WEIGHT_BUDGET`,
//! and normalization constants in `scoring.rs` with data-driven profiles.
//!
//! Data files: `data/objective_profiles/{pve,pvp,wvw}.json`
//! Loader pattern: `include_str!` + `OnceLock` + validation (P3-07).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use thiserror::Error;

use super::boon_condition_formulas::canonical_condition_name;
use super::{try_load, DataLoadError, EvidenceLevel};

// Embedded JSON (compile-time)

const PVE_PROFILES_JSON: &str = include_str!("../../../../data/objective_profiles/pve.json");
const PVP_PROFILES_JSON: &str = include_str!("../../../../data/objective_profiles/pvp.json");
const WVW_PROFILES_JSON: &str = include_str!("../../../../data/objective_profiles/wvw.json");

static OBJECTIVE_PROFILES: OnceLock<ObjectiveProfileData> = OnceLock::new();

/// Returns the globally loaded objective profiles, parsing on first access.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data, should never happen).
pub fn objective_profiles() -> &'static ObjectiveProfileData {
    OBJECTIVE_PROFILES.get_or_init(|| {
        load_all_objective_profiles().expect("embedded objective_profiles JSON is invalid")
    })
}

/// Try to load all objective profiles from the embedded JSON, returning typed errors
/// on failure. Does NOT store in OnceLock — used for health-check validation.
pub fn try_load_objective_profiles() -> Result<(), Vec<DataLoadError>> {
    try_load!(
        "objective_profiles",
        load_all_objective_profiles().map(|_| ()),
        ObjectiveProfileError
    )
}

#[derive(Debug, Error)]
pub enum ObjectiveProfileError {
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    ValidationError(String),
}

/// Axis weights for a 6-axis objective profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisWeights {
    pub power: f64,
    pub condition: f64,
    pub boon_support: f64,
    pub healing: f64,
    pub sustain: f64,
    pub control: f64,
}

/// The direction a role serves at one group size.
///
/// The weights alone say how much each axis is worth, which is not the same
/// as what the role is for: WvW_Support weights sustain 0.55 above healing
/// 0.25, so "nearest to the support weights" measured out as a bruiser
/// across the whole corpus. Focus names the direction, avoid names what
/// delivering cannot earn credit for, and the two are read by
/// [`crate::scoring::intent_alignment`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentRow {
    /// Axis keys (`power`, `condition`, `boon_support`, `healing`,
    /// `sustain`, `control`) this role exists to deliver at this scale.
    #[serde(default)]
    pub focus: Vec<String>,
    /// Axis keys delivering which says nothing good about this role. A
    /// build that heals its party is not thereby a better zerg DPS.
    #[serde(default)]
    pub avoid: Vec<String>,
    /// How much of its own survival this role has to carry at this scale:
    /// `strict` alone, `moderate` in a havoc group, `shared` in a squad.
    /// Documentation for the gate calibration; nothing reads it yet.
    #[serde(default)]
    pub self_reliance: String,
}

/// One [`IntentRow`] per group size.
///
/// The same role is a different job at a different scale: alone you carry
/// your own cleanse and your own sustain, in a zerg the squad carries them
/// and what is left is your contribution to it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentMatrix {
    #[serde(default)]
    pub solo: IntentRow,
    #[serde(default)]
    pub party: IntentRow,
    #[serde(default)]
    pub squad: IntentRow,
}

impl IntentMatrix {
    /// The row for a group size.
    pub fn row(&self, tier: crate::scenario::CombatTier) -> &IntentRow {
        match tier {
            crate::scenario::CombatTier::Solo => &self.solo,
            crate::scenario::CombatTier::Party => &self.party,
            crate::scenario::CombatTier::Squad => &self.squad,
        }
    }

    /// The three rows with their names, for tests and dumps.
    pub fn rows(&self) -> [(&'static str, &IntentRow); 3] {
        [
            ("solo", &self.solo),
            ("party", &self.party),
            ("squad", &self.squad),
        ]
    }
}

impl AxisWeights {
    /// The six axes in [`crate::scoring::AXIS_KEYS`] order.
    pub fn as_array(&self) -> [f64; 6] {
        [
            self.power,
            self.condition,
            self.boon_support,
            self.healing,
            self.sustain,
            self.control,
        ]
    }
}

/// Normalization constants for each scoring axis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationConstants {
    pub strike_dps_norm: f64,
    pub condi_dps_norm: f64,
    pub boon_support_norm: f64,
    pub healing_power_norm: f64,
    pub effective_health_norm: f64,
    pub control_norm: f64,
}

/// Viability floor thresholds for a build archetype.
///
/// Consumed by `referee::evaluate_viability_gates_for` when the caller supplies
/// an `ObjectiveProfile`. A set `ehp_floor` overrides the hardcoded mode/tier
/// constant; unset fields keep those defaults (they do not skip the gate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViabilityGateConfig {
    /// Minimum effective health required for this role.
    #[serde(default)]
    pub ehp_floor: Option<f64>,
    /// Minimum number of stunbreak skills equipped.
    #[serde(default)]
    pub min_stunbreaks: Option<u32>,
    /// Whether self-Stability access is required.
    #[serde(default)]
    pub requires_stability: Option<bool>,
    /// Minimum number of equipped skills with a damaging-condition cleanse effect.
    /// Feeds `ViabilityGate::CleanseRate` only; soft-control is `ControlCoverage`.
    #[serde(default)]
    pub min_cleanse_count: Option<u32>,
    /// Minimum damaging conditions cleansed per 20s (`CleanseRate`).
    /// Resistance must not discount this floor; soft control is separate.
    #[serde(default)]
    pub min_cleanse_rate_per_20s: Option<f64>,
    /// Per-boon uptime floors (fraction 0.0–1.0).
    /// e.g. `{"Quickness": 0.9}` for a boon support role.
    #[serde(default)]
    pub boon_uptime_floors: HashMap<String, f64>,
}

/// A single objective profile defining scoring behavior for a build archetype.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectiveProfile {
    pub objective_profile_id: String,
    pub axis_weights: AxisWeights,
    /// What this role is FOR, by group size. See [`IntentRow`].
    #[serde(default)]
    pub intent: IntentMatrix,
    pub weight_budget: f64,
    pub normalization_constants: NormalizationConstants,
    /// Boon type → relative priority (0.0–1.0).
    pub boon_priorities: HashMap<String, f64>,
    /// Condition type → relative priority (0.0–1.0).
    pub condition_priorities: HashMap<String, f64>,
    /// Optional interaction operation type → relative priority (0.0–1.0).
    /// Keys: removes_boon, steals_boon, corrupts_boon, removes_condition,
    ///        converts_condition_to_boon, transfers_condition.
    #[serde(default)]
    pub interaction_priorities: HashMap<String, f64>,
    /// Role-specific viability floors consumed by `referee::evaluate_viability_gates_for`.
    /// A set `ehp_floor` overrides the hardcoded mode/tier constant; unset fields
    /// keep those defaults.
    #[serde(default)]
    pub viability_gates: ViabilityGateConfig,
    /// Exactly one profile per mode must be true.
    pub is_mode_default: bool,
    /// Human-readable documentation of scoring intent and assumptions.
    pub notes: String,
    pub evidence_level: EvidenceLevel,
}

/// A mode file containing all profiles for that game mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectiveProfileFile {
    pub mode: String,
    pub profiles: Vec<ObjectiveProfile>,
}

/// All loaded objective profiles across modes.
#[derive(Debug, Clone)]
pub struct ObjectiveProfileData {
    pub files: HashMap<String, ObjectiveProfileFile>,
}

impl ObjectiveProfileData {
    pub fn default_for_mode(&self, mode: &str) -> Option<&ObjectiveProfile> {
        self.files
            .get(mode)
            .and_then(|f| f.profiles.iter().find(|p| p.is_mode_default))
    }

    pub fn profile_by_id(&self, id: &str) -> Option<&ObjectiveProfile> {
        for file in self.files.values() {
            if let Some(p) = file.profiles.iter().find(|p| p.objective_profile_id == id) {
                return Some(p);
            }
        }
        None
    }

    pub fn profiles_for_mode(&self, mode: &str) -> Vec<&ObjectiveProfile> {
        self.files
            .get(mode)
            .map(|f| f.profiles.iter().collect())
            .unwrap_or_default()
    }

    pub fn all_profiles(&self) -> Vec<&ObjectiveProfile> {
        self.files
            .values()
            .flat_map(|f| f.profiles.iter())
            .collect()
    }
}

fn load_all_objective_profiles() -> Result<ObjectiveProfileData, ObjectiveProfileError> {
    let pve = load_objective_profile_file(PVE_PROFILES_JSON, "PvE")?;
    let pvp = load_objective_profile_file(PVP_PROFILES_JSON, "PvP")?;
    let wvw = load_objective_profile_file(WVW_PROFILES_JSON, "WvW")?;

    let mut files = HashMap::new();
    files.insert("PvE".to_string(), pve);
    files.insert("PvP".to_string(), pvp);
    files.insert("WvW".to_string(), wvw);

    Ok(ObjectiveProfileData { files })
}

pub fn load_objective_profile_file(
    json: &str,
    expected_mode: &str,
) -> Result<ObjectiveProfileFile, ObjectiveProfileError> {
    let mut file: ObjectiveProfileFile = serde_json::from_str(json)?;

    // Normalize condition_priorities keys to their canonical form so authored
    // verb-form names (Blind, Poison, Immobilize) match the status-effect-form
    // keys in `data/formulas/conditions.json` (Blinded, Poisoned, Immobile).
    // If both forms are present, the canonical form wins.
    for profile in file.profiles.iter_mut() {
        let original = std::mem::take(&mut profile.condition_priorities);
        let mut normalized: HashMap<String, f64> = HashMap::with_capacity(original.len());
        for (name, weight) in original {
            let canonical = canonical_condition_name(&name).to_string();
            normalized.insert(canonical, weight);
        }
        profile.condition_priorities = normalized;
    }

    if file.mode != expected_mode {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "mode '{}' does not match expected '{}'",
            file.mode, expected_mode
        )));
    }

    if file.profiles.is_empty() {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "{} has no profiles",
            expected_mode
        )));
    }

    // Validate exactly one is_mode_default per mode
    let default_count = file.profiles.iter().filter(|p| p.is_mode_default).count();
    if default_count != 1 {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "{} has {} default profiles, expected exactly 1",
            expected_mode, default_count
        )));
    }

    let mut seen_ids = std::collections::HashSet::new();
    for profile in &file.profiles {
        if !seen_ids.insert(&profile.objective_profile_id) {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "duplicate objective_profile_id '{}' in {}",
                profile.objective_profile_id, expected_mode
            )));
        }
    }

    for profile in &file.profiles {
        validate_profile(profile, expected_mode)?;
    }

    Ok(file)
}

fn validate_profile(profile: &ObjectiveProfile, mode: &str) -> Result<(), ObjectiveProfileError> {
    let id = &profile.objective_profile_id;

    let aw = &profile.axis_weights;
    for (name, val) in [
        ("power", aw.power),
        ("condition", aw.condition),
        ("boon_support", aw.boon_support),
        ("healing", aw.healing),
        ("sustain", aw.sustain),
        ("control", aw.control),
    ] {
        if !(0.0..=1.0).contains(&val) {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "{}/{}: axis_weights.{} = {} out of range [0.0, 1.0]",
                mode, id, name, val
            )));
        }
    }

    if profile.weight_budget <= 0.0 {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "{}/{}: weight_budget {} must be positive",
            mode, id, profile.weight_budget
        )));
    }

    let nc = &profile.normalization_constants;
    for (name, val) in [
        ("strike_dps_norm", nc.strike_dps_norm),
        ("condi_dps_norm", nc.condi_dps_norm),
        ("boon_support_norm", nc.boon_support_norm),
        ("healing_power_norm", nc.healing_power_norm),
        ("effective_health_norm", nc.effective_health_norm),
        ("control_norm", nc.control_norm),
    ] {
        if val <= 0.0 {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "{}/{}: normalization_constants.{} = {} must be positive",
                mode, id, name, val
            )));
        }
    }

    if profile.boon_priorities.is_empty() {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "{}/{}: boon_priorities must not be empty",
            mode, id
        )));
    }
    for (boon, val) in &profile.boon_priorities {
        if !(0.0..=1.0).contains(val) {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "{}/{}: boon_priorities['{}'] = {} out of range [0.0, 1.0]",
                mode, id, boon, val
            )));
        }
    }

    if profile.condition_priorities.is_empty() {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "{}/{}: condition_priorities must not be empty",
            mode, id
        )));
    }
    for (cond, val) in &profile.condition_priorities {
        if !(0.0..=1.0).contains(val) {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "{}/{}: condition_priorities['{}'] = {} out of range [0.0, 1.0]",
                mode, id, cond, val
            )));
        }
    }

    let valid_interaction_keys = [
        "removes_boon",
        "corrupts_boon",
        "removes_condition",
        "converts_condition_to_boon",
        "transfers_condition",
    ];
    for (key, val) in &profile.interaction_priorities {
        if !valid_interaction_keys.contains(&key.as_str()) {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "{}/{}: interaction_priorities has unknown key '{}'",
                mode, id, key
            )));
        }
        if !(0.0..=1.0).contains(val) {
            return Err(ObjectiveProfileError::ValidationError(format!(
                "{}/{}: interaction_priorities['{}'] = {} out of range [0.0, 1.0]",
                mode, id, key, val
            )));
        }
    }

    // Validate evidence_level is Heuristic
    if profile.evidence_level != EvidenceLevel::Heuristic {
        return Err(ObjectiveProfileError::ValidationError(format!(
            "{}/{}: evidence_level must be Heuristic, got {:?}",
            mode, id, profile.evidence_level
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {

    /// Every (profile, group size) row declares a direction, and the two
    /// lists never contradict each other.
    ///
    /// The axis names are the `scoring::AXIS_KEYS`, so a typo is a silently
    /// ignored axis rather than a parse error - which is exactly the failure
    /// a data test is for.
    #[test]
    fn every_intent_row_declares_a_focus_and_no_axis_is_both() {
        const AXES: [&str; 6] = [
            "power",
            "condition",
            "boon_support",
            "healing",
            "sustain",
            "control",
        ];
        /// Roles that focus healing and still hit things on purpose.
        ///
        /// A commander leads a squad: it carries boons and stays up, and
        /// what damage it does is not the reason it is in the group, but
        /// punishing it for hitting anything would file a tag behind a
        /// backline healer for a commander request. Hybrid support says in
        /// its own name that it does both.
        const MAY_FOCUS_HEALING_AND_POWER: [&str; 4] = [
            "WvW_Commander",
            "PvE_Commander",
            "PvP_Commander",
            "PvE_Hybrid_Support",
        ];
        /// Roles whose Solo row deliberately does NOT focus sustain.
        ///
        /// The glass cannons: a roamer or a burst assassin forgoes sustain
        /// on purpose and dies to anything that lands, which is the trade
        /// that makes the build. Scoring them for sustaining would file
        /// every bruiser above every assassin for an assassin request.
        const NO_SOLO_SUSTAIN: [&str; 5] = [
            "WvW_Roamer",
            "PvE_Harasser",
            "PvP_Harasser",
            "PvP_Burst",
            "PvP_Condi",
        ];
        for file in objective_profiles().files.values() {
            for profile in &file.profiles {
                let id = &profile.objective_profile_id;
                for (tier, row) in profile.intent.rows() {
                    assert!(
                        !row.focus.is_empty(),
                        "{id} [{tier}] declares no focus axis - a role with no                          direction cannot be matched against"
                    );
                    for axis in row.focus.iter().chain(row.avoid.iter()) {
                        assert!(
                            AXES.contains(&axis.as_str()),
                            "{id} [{tier}]: unknown axis {axis:?}"
                        );
                    }
                    for axis in &row.focus {
                        assert!(
                            !row.avoid.contains(axis),
                            "{id} [{tier}]: {axis} is both focus and avoid"
                        );
                    }
                    assert!(
                        !row.self_reliance.is_empty(),
                        "{id} [{tier}]: no self-reliance expectation"
                    );
                }
                // A role that exists to keep other people alive is not
                // scored for hitting things. Read off the row rather than
                // the profile's name: a name is exactly the evidence this
                // path does not trust, and PvP_Sustain is the PvP healer
                // chip while PvE_Hybrid_Support is not a support at all.
                for (tier, row) in profile.intent.rows() {
                    let heals = row.focus.iter().any(|a| a == "healing");
                    if heals && !MAY_FOCUS_HEALING_AND_POWER.contains(&id.as_str()) {
                        assert!(
                            row.avoid.iter().any(|a| a == "power"),
                            "{id} [{tier}] focuses healing and must avoid power \
                             (add it to MAY_FOCUS_HEALING_AND_POWER with a reason \
                             if it genuinely does both)"
                        );
                    }
                }
                // Alone, everything that is not a support or a declared
                // glass cannon has to survive on its own.
                let solo = profile.intent.row(crate::scenario::CombatTier::Solo);
                // Read off the row rather than the name: PvP_Sustain is the
                // PvP healer chip, and a name is exactly the evidence this
                // path does not trust.
                let supportish = solo
                    .focus
                    .iter()
                    .any(|a| a == "healing" || a == "boon_support");
                if !supportish && !NO_SOLO_SUSTAIN.contains(&id.as_str()) {
                    assert!(
                        solo.focus.iter().any(|a| a == "sustain"),
                        "{id} [solo] must focus sustain - alone there is nobody                          else to carry it (add it to NO_SOLO_SUSTAIN with a                          reason if the role forgoes sustain on purpose)"
                    );
                }
            }
        }
    }
    use super::*;

    #[test]
    fn test_load_all_profiles_succeeds() {
        let data = load_all_objective_profiles().expect("should load all profiles");
        assert_eq!(data.files.len(), 3, "should have PvE, PvP, WvW");
    }

    #[test]
    fn test_pve_has_11_profiles() {
        let data = load_all_objective_profiles().unwrap();
        assert_eq!(
            data.files["PvE"].profiles.len(),
            11,
            "PvE should have 11 profiles"
        );
    }

    #[test]
    fn test_pvp_has_10_profiles() {
        let data = load_all_objective_profiles().unwrap();
        assert_eq!(
            data.files["PvP"].profiles.len(),
            10,
            "PvP should have 10 profiles"
        );
    }

    #[test]
    fn test_wvw_has_11_profiles() {
        let data = load_all_objective_profiles().unwrap();
        assert_eq!(
            data.files["WvW"].profiles.len(),
            11,
            "WvW should have 11 profiles"
        );
    }

    #[test]
    fn test_each_mode_has_one_default() {
        let data = load_all_objective_profiles().unwrap();
        for (mode, file) in &data.files {
            let defaults: Vec<_> = file.profiles.iter().filter(|p| p.is_mode_default).collect();
            assert_eq!(
                defaults.len(),
                1,
                "{} should have exactly 1 default profile, got {}",
                mode,
                defaults.len()
            );
        }
    }

    #[test]
    fn test_default_for_mode_returns_correct_profile() {
        let data = load_all_objective_profiles().unwrap();
        let pve_default = data
            .default_for_mode("PvE")
            .expect("PvE should have default");
        assert!(pve_default.is_mode_default);
        assert_eq!(pve_default.objective_profile_id, "PvE_Power_DPS");

        let pvp_default = data
            .default_for_mode("PvP")
            .expect("PvP should have default");
        assert!(pvp_default.is_mode_default);

        let wvw_default = data
            .default_for_mode("WvW")
            .expect("WvW should have default");
        assert!(wvw_default.is_mode_default);
    }

    #[test]
    fn test_profile_by_id() {
        let data = load_all_objective_profiles().unwrap();
        let profile = data
            .profile_by_id("PvE_Condi_DPS")
            .expect("should find Condi DPS");
        assert_eq!(profile.objective_profile_id, "PvE_Condi_DPS");
        assert!(!profile.is_mode_default);
    }

    #[test]
    fn test_profile_by_id_nonexistent() {
        let data = load_all_objective_profiles().unwrap();
        assert!(data.profile_by_id("Nonexistent").is_none());
    }

    #[test]
    fn test_all_profiles_have_heuristic_evidence() {
        let data = load_all_objective_profiles().unwrap();
        for profile in data.all_profiles() {
            assert_eq!(
                profile.evidence_level,
                EvidenceLevel::Heuristic,
                "Profile {} should be Heuristic",
                profile.objective_profile_id
            );
        }
    }

    #[test]
    fn test_all_profiles_have_valid_weight_budget() {
        let data = load_all_objective_profiles().unwrap();
        for profile in data.all_profiles() {
            assert!(
                profile.weight_budget > 0.0,
                "Profile {} should have positive weight_budget",
                profile.objective_profile_id
            );
            // All current profiles use 2.0 for backward compatibility
            assert!(
                (profile.weight_budget - 2.0).abs() < 0.001,
                "Profile {} weight_budget should be 2.0 for backward compat, got {}",
                profile.objective_profile_id,
                profile.weight_budget
            );
        }
    }

    #[test]
    fn test_all_profiles_have_nonempty_boon_priorities() {
        let data = load_all_objective_profiles().unwrap();
        for profile in data.all_profiles() {
            assert!(
                !profile.boon_priorities.is_empty(),
                "Profile {} should have non-empty boon_priorities",
                profile.objective_profile_id
            );
        }
    }

    #[test]
    fn test_all_profiles_have_nonempty_condition_priorities() {
        let data = load_all_objective_profiles().unwrap();
        for profile in data.all_profiles() {
            assert!(
                !profile.condition_priorities.is_empty(),
                "Profile {} should have non-empty condition_priorities",
                profile.objective_profile_id
            );
        }
    }

    #[test]
    fn test_malformed_json() {
        let result = load_objective_profile_file("not valid json", "PvE");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err
        );
    }

    #[test]
    fn test_mode_mismatch() {
        let result = load_objective_profile_file(PVE_PROFILES_JSON, "PvP");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("does not match"),
            "expected mode mismatch, got: {}",
            err
        );
    }

    #[test]
    fn test_profiles_for_mode() {
        let data = load_all_objective_profiles().unwrap();
        let pve = data.profiles_for_mode("PvE");
        assert_eq!(pve.len(), 11);
        let unknown = data.profiles_for_mode("Unknown");
        assert!(unknown.is_empty());
    }

    #[test]
    fn test_axis_weights_within_budget() {
        let data = load_all_objective_profiles().unwrap();
        for profile in data.all_profiles() {
            let aw = &profile.axis_weights;
            let total =
                aw.power + aw.condition + aw.boon_support + aw.healing + aw.sustain + aw.control;
            assert!(
                total <= profile.weight_budget + 0.001,
                "Profile {} axis_weights total {} exceeds weight_budget {}",
                profile.objective_profile_id,
                total,
                profile.weight_budget
            );
        }
    }
}
