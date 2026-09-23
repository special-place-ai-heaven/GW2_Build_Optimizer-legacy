//! Normalized effect type system for the GW2 Build Optimizer.
//!
//! Defines the structured representation of every modifier that traits, skills,
//! runes, sigils, and relics produce. This is the **type system and schema only** —
//! data population happens in P3-10b and heuristic uptime modeling in P3-14.
//!
//! Each `NormalizedEffect` captures:
//! - What source produces it (trait, skill, rune, sigil, relic)
//! - What category of effect it is (23 variants from flat stat to triggered effect)
//! - How it stacks with other effects of the same category
//! - When it triggers (passive, on-crit, on-hit, etc.)
//! - Uptime modeling metadata
//! - Optional `StatusOperation` payload for boon/condition interaction categories

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use thiserror::Error;

use super::quality::FactualValue;
use super::{try_load, DataLoadError, EvidenceLevel};

/// Serde helper for `Option<FactualValue<T>>` with 3-state JSON mapping:
/// - field absent → None (not applicable)
/// - null → Some(Unknown) (applicable but value not yet sourced)
/// - value → Some(Resolved(v)) (factually known)
mod optional_factual {
    use super::super::quality::FactualValue;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(
        value: &Option<FactualValue<T>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_none(),
            Some(fv) => fv.serialize(serializer),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<FactualValue<T>>, D::Error> {
        // When this is called, the field IS present in JSON
        let opt = Option::<T>::deserialize(deserializer)?;
        Ok(Some(match opt {
            Some(v) => FactualValue::Resolved(v),
            None => FactualValue::Unknown,
        }))
    }
}

// Embedded baseline JSON (compile-time)

const PVE_EFFECTS_JSON: &str =
    include_str!("../../../../data/normalized_effects/2026-01-13/pve.json");
const PVP_EFFECTS_JSON: &str =
    include_str!("../../../../data/normalized_effects/2026-01-13/pvp.json");
const WVW_EFFECTS_JSON: &str =
    include_str!("../../../../data/normalized_effects/2026-01-13/wvw.json");

static EFFECTS: OnceLock<NormalizedEffectsData> = OnceLock::new();

/// Returns the globally loaded normalized effects, parsing on first access.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data, should never happen).
pub fn effects() -> &'static NormalizedEffectsData {
    EFFECTS.get_or_init(|| load_all_effects().expect("embedded normalized_effects JSON is invalid"))
}

/// Try to load all normalized effects from the embedded JSON, returning typed errors
/// on failure. Does NOT store in OnceLock — used for health-check validation.
pub fn try_load_normalized_effects() -> Result<(), Vec<DataLoadError>> {
    try_load!(
        "normalized_effects",
        load_all_effects().map(|_| ()),
        NormalizedEffectError
    )
}

#[derive(Debug, Error)]
pub enum NormalizedEffectError {
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    ValidationError(String),
}

/// The type of game entity that produces this effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SourceType {
    Trait,
    Skill,
    Rune,
    Sigil,
    Relic,
}

/// Category of effect — determines how the optimizer interprets and applies the value.
///
/// Categories 0-11: numeric modifiers (stat bonuses, damage multipliers, duration bonuses).
/// Categories 12-18: status operations (boon/condition application, removal, conversion).
/// Categories 19-21: special (defiance damage, proc effects, triggered effects).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EffectCategory {
    FlatStat,
    StatConversion,
    StrikeDamagePct,
    ConditionDamagePct,
    SpecificConditionDamagePct,
    CritDamagePct,
    BoonDurationPct,
    ConditionDurationPct,
    SpecificConditionDurationPct,
    OutgoingHealingPct,
    IncomingStrikeMultiplier,
    IncomingConditionMultiplier,
    AppliesBoon,
    AppliesCondition,
    RemovesBoon,
    CorruptsBoon,
    RemovesCondition,
    ConvertsConditionToBoon,
    TransfersCondition,
    DefianceDamage,
    ProcEffect,
    TriggeredEffect,
    // Sprint 3 (specs/007-trait-triggers)
    /// Credits a percent of the life force pool (`value` = percent).
    GainsLifeForce,
    /// Flat self heal (`value`), plus `healing_power_coefficient` x healing power.
    Heal,
    /// Critical chance in percentage points (Decimate Defenses: per stack
    /// of the foe's vulnerability).
    CritChancePct,
    /// NeedsMechanic Engine E4: payload creates a clone via IllusionState::spawn.
    SpawnClone,
    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
    /// The source unlocks a profession mechanic, or replaces one with
    /// another (Weaver's attunement remap, Bladesworn flow for adrenaline,
    /// Specter's Siphon for Steal). Recorded as a capability flag on the
    /// build, never as a stat: it carries no `value`.
    MechanicUnlock {
        mechanic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<String>,
    },
}

impl EffectCategory {
    /// Returns true if this category represents a status operation
    /// (boon/condition application, removal, conversion, or transfer).
    /// These categories should have a `status_operation` payload.
    pub fn is_status_operation(&self) -> bool {
        matches!(
            self,
            EffectCategory::AppliesBoon
                | EffectCategory::AppliesCondition
                | EffectCategory::RemovesBoon
                | EffectCategory::CorruptsBoon
                | EffectCategory::RemovesCondition
                | EffectCategory::ConvertsConditionToBoon
                | EffectCategory::TransfersCondition
        )
    }
}

/// How multiple instances of this effect stack with each other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StackingRule {
    /// Effects multiply together: (1 + a) * (1 + b).
    Multiplicative,
    /// Effects add together: a + b.
    Additive,
    /// Only the highest value applies.
    Highest,
    /// Effect does not stack — only one instance active at a time.
    NonStacking,
    /// Gaining a stack refreshes the duration of every stack already held
    /// (Lethal Tempo). Without it each stack expires on its own clock.
    RefreshAllStacks,
}

/// When this effect activates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerRule {
    /// Always active while the source is equipped/traited.
    Passive,
    /// Triggers on critical hit.
    OnCrit,
    /// Triggers on any hit (strike or condition tick).
    OnHit,
    /// Triggers on skill use.
    OnSkillUse,
    /// Triggers when health crosses a threshold.
    OnHealthThreshold,
    /// Triggers based on a custom condition (e.g., "while above 90% health").
    Conditional,
    // Sprint 3 (specs/007-trait-triggers)
    /// The player's shroud entry skill resolves (never Manifest Sand Shade).
    OnShroudEnter,
    /// The shroud ends by skill, opener, drain or damage.
    OnShroudExit,
    /// The player puts a condition on a foe (scope `Status` names it).
    OnConditionApplied,
    /// A cleanse removed at least one condition from the player (scope
    /// `Status` names the last one removed).
    OnConditionRemoved,
    /// A boon lands on the player (scope `Status` names it).
    OnBoonApplied,
    /// The player removes or corrupts a boon on a foe (scope `Status` names it).
    OnBoonStripped,
    /// Every `internal_cooldown` seconds from the fight's start.
    Periodic,
    // NeedsMechanic Engine E0 (TriggerBus)
    /// Player dodge roll resolves (EndurancePool + DodgeAction).
    OnDodge,
    /// Player disables a foe (reads TargetState disable; no second map).
    OnDisableFoe,
    /// Player elite skill resolves.
    OnElite,
    /// A modeled threshold crossed (health / similar); bus OnThreshold.
    OnThreshold,
    /// Primary attunement changed (AttunementState); bus OnAttunementSwap.
    OnAttunementSwap,
    /// Clone count rose (IllusionState spawn); bus OnCloneCreated.
    OnCloneCreated,
    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
    /// The record is classified, not executed: the only legal trigger on a
    /// `coverage` block. It makes no claim about when the source fires.
    NotApplicable,
    /// The player blocks an incoming strike.
    OnBlock,
    /// The player's Steal (or its elite-spec replacement) resolves.
    OnSteal,
    /// The player enters stealth.
    OnStealthEnter,
    /// The player leaves stealth.
    OnStealthExit,
    /// The player invokes the other legend.
    OnLegendSwap,
    /// The player enters berserk mode.
    OnBerserkEnter,
    /// One of the player's symbols strikes a foe.
    OnSymbolHit,
    /// One of the player's explosions resolves.
    OnExplosion,
    /// A boon lands on the player; `boon` narrows it to one name.
    OnBoonGained {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boon: Option<String>,
    },
    /// The player breaks a stun.
    OnStunbreak,
}

/// Health prerequisite of an `OnHealthThreshold` / `Conditional` effect,
/// read against the regular health pool (not life force).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthThreshold {
    /// `true`: active while health is above `percent`; `false`: below.
    pub above: bool,
    /// Threshold as a percentage of maximum health, in (0, 100].
    pub percent: FactualValue<f64>,
}

/// Which activating skills count for an `OnHit` / `OnSkillUse` effect.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum TriggerScope {
    /// Any landed hit or used skill.
    #[default]
    Any,
    /// Only weapon skills with a recharge or a resource cost (Relic of the
    /// Thief wording).
    WeaponSkillWithRecharge,
    // Sprint 3 (specs/007-trait-triggers)
    /// Skills whose `Skill.categories` contains the name (`{"Category":"Shout"}`).
    Category(String),
    /// Skills in the named slot: Heal, Utility, Elite, Profession (`{"Slot":"Elite"}`).
    Slot(String),
    /// A boon or condition name for the three status triggers (`{"Status":"Fear"}`).
    Status(String),
}

/// What must hold for a Sprint 3 record to fire or stay active. Members are
/// optional; an empty block is rejected by validation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Prerequisite {
    /// The primary foe carries this condition (unexpired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foe_condition: Option<String>,
    /// The player is (true) or is not (false) in shroud.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_shroud: Option<bool>,
    /// The primary foe's health against a threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foe_health: Option<HealthThreshold>,
    /// Player primary attunement name (Fire/Water/Air/Earth). E3 while-attuned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attunement: Option<String>,
}

impl Prerequisite {
    pub fn is_empty(&self) -> bool {
        self.foe_condition.is_none()
            && self.in_shroud.is_none()
            && self.foe_health.is_none()
            && self.attunement.is_none()
    }
}

/// Multiplier applied to a `GainsLifeForce` / `Heal` value at firing time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScaleBy {
    /// Times the number of conditions the same firing removed.
    ConditionsRemoved,
}

/// Which hand a [`Gate::Weapon`] reads. Absent means any hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeaponHand {
    Main,
    Off,
    TwoHand,
}

/// Where the player stands relative to the foe for a [`Gate::Positional`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Positional {
    Flank,
    Behind,
    Front,
}

/// When a [`Gate::HealthThreshold`] may fire again after it fired once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rearm {
    /// Latches: one firing for the whole fight.
    OncePerFight,
    /// Re-arms once health leaves the gated band and crosses back in.
    WhenRecovered,
    /// The record's `internal_cooldown` is the only limit.
    Icd,
}

/// State an [`Gate::Interval`] tick is checked against. The same block a
/// record's `prerequisite` uses, so there is one evaluator for both.
pub type StateGate = Prerequisite;

/// A condition that must hold for a record to fire or stay active. A record
/// carries any number; all of them must hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Gate {
    /// The player is in combat.
    InCombat,
    /// At most one firing every `every_ms`, and only while `while_state`
    /// holds (Natural Mender: astral force while *not* in celestial avatar).
    Interval {
        every_ms: u32,
        #[serde(default, rename = "while", skip_serializing_if = "Option::is_none")]
        while_state: Option<StateGate>,
    },
    /// One of `types` is equipped on the held set, in `hand` when given.
    Weapon {
        types: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hand: Option<WeaponHand>,
    },
    /// The player strikes from the named side.
    Positional(Positional),
    /// At least `min_targets` foes within `radius` of the player.
    Proximity { radius: f64, min_targets: u32 },
    /// Player health inside a band, with a re-arm rule.
    HealthThreshold {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below_pct: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above_pct: Option<f64>,
        rearm: Rearm,
    },
    /// The player carries the named boon.
    SelfBoon { boon: String },
    /// The player holds at least `min` of the named resource.
    SelfResourceStacks { resource: String, min: u32 },
}

/// Live state added to a record's `value` when it fires:
/// `effective = value + per_unit_or_stack * min(n, cap)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Scale {
    /// `n` = distance to the foe in game units (Pure of Sight).
    PerDistance {
        per_unit: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<f64>,
    },
    /// `n` = the player's stacks of the named resource (Alchemic Vigor).
    PerSelfResourceStack {
        resource: String,
        per_stack: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<f64>,
    },
    /// `n` = the player's stacks of the named boon (Reinforced Potency).
    PerSelfBoon {
        boon: String,
        per_stack: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<f64>,
    },
}

/// Whose event fires a trigger record. An on-crit record owned by a pet or a
/// clone must not fire off the player's crits (Pet's Prowess, Sharper Images).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Actor {
    #[default]
    Player,
    Pet,
    Illusion,
    /// Any actor the build fields, the player included.
    Any,
}

impl Actor {
    /// Serde skip predicate: the default needs no line in the JSON.
    pub fn is_player(&self) -> bool {
        matches!(self, Actor::Player)
    }
}

/// Why a trait is classified instead of executed (a record with `coverage`
/// carries no payload; the coverage line shows the class).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CoverageClass {
    /// The trait changes nothing the simulator measures.
    PassiveNoEffect,
    /// The trait needs a mechanic the simulator has no state for (`mechanic`).
    NeedsMechanic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageBlock {
    pub class: CoverageClass,
    /// Required for `NeedsMechanic`, forbidden otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanic: Option<String>,
}

// Uptime model

/// How the uptime value was determined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UptimeModelKind {
    /// Effect is always active (100% uptime). Typical for passive traits.
    AlwaysOn,
    /// Uptime was estimated based on typical gameplay patterns.
    Estimated,
    /// Uptime is derived from other known values (e.g., ICD + proc chance).
    Derived,
    /// Uptime is unknown — no data available.
    Unknown,
}

/// Metadata about how often an effect is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UptimeModel {
    pub kind: UptimeModelKind,
    /// Fractional uptime (0.0 to 1.0). Only meaningful for `Estimated` or `Derived` kinds.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub uptime: Option<FactualValue<f64>>,
}

// StatusOperation

/// The type of boon/condition operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OperationType {
    AppliesBoon,
    RemovesBoon,
    CorruptsBoon,
    AppliesCondition,
    RemovesCondition,
    ConvertsConditionToBoon,
    TransfersCondition,
}

/// Which side the operation targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TargetSide {
    #[serde(rename = "self")]
    Self_,
    Ally,
    Enemy,
}

/// How the amount is measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AmountMode {
    Stacks,
    DurationMs,
    Charges,
    Count,
}

/// How many targets the operation affects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TargetScope {
    #[serde(rename = "self")]
    Self_,
    SingleTarget,
    NearbyAllies,
    Party,
    Squad,
    Area,
}

/// Describes a boon or condition operation (application, removal, conversion, etc.).
///
/// Used as a payload for status-interaction effect categories (AppliesBoon through
/// TransfersCondition).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusOperation {
    /// The type of operation being performed.
    pub operation_type: OperationType,
    /// Which side the operation targets (self, ally, enemy).
    pub target_side: TargetSide,
    /// The boon or condition name (e.g., "Might", "Burning", "Protection").
    pub status_kind: String,
    /// How the amount value is interpreted.
    pub amount_mode: AmountMode,
    /// Numeric amount (stacks, duration, charges, or count depending on mode).
    pub amount_value: FactualValue<f64>,
    /// Base duration of the applied status in milliseconds, if applicable.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub base_duration_ms: Option<FactualValue<u32>>,
    /// How many/what kind of targets are affected.
    pub target_scope: TargetScope,
    /// Maximum number of targets affected. `None` means unlimited or N/A.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub target_count: Option<FactualValue<u32>>,
    /// Internal cooldown of this specific operation in milliseconds.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub internal_cooldown_ms: Option<FactualValue<u32>>,
    /// Multiplier applied to the source's boon/condition duration stat.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub source_duration_multiplier: Option<FactualValue<f64>>,
}

// NormalizedEffect

/// A single normalized effect — the structured representation of one modifier
/// produced by a trait, skill, rune, sigil, or relic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEffect {
    /// Unique identifier for this effect (e.g., "trait_214_might_on_crit").
    pub effect_id: String,
    /// Type of source entity producing this effect.
    pub source_type: SourceType,
    /// GW2 API ID of the source entity.
    pub source_id: u32,
    /// Human-readable name of the source entity.
    pub source_name: String,
    /// Category of effect — determines interpretation and stacking behavior.
    pub category: EffectCategory,
    /// Primary numeric value of the effect (meaning depends on category).
    pub value: FactualValue<f64>,
    /// How this effect stacks with other effects of the same category.
    pub stacking_rule: StackingRule,
    /// When this effect activates.
    pub trigger_rule: TriggerRule,
    /// Uptime model metadata.
    pub uptime_model: UptimeModel,
    /// Evidence level for this effect's data.
    pub evidence_level: EvidenceLevel,
    /// Optional source citation (wiki URL, patch notes, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    // Timer/cap metadata
    /// Duration of the effect in seconds, if applicable.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub effect_duration: Option<FactualValue<f64>>,
    /// Internal cooldown in seconds, if applicable.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub internal_cooldown: Option<FactualValue<f64>>,
    /// Maximum number of stacks this effect can have.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub max_stacks: Option<FactualValue<u32>>,

    // Interaction payload (for status operation categories)
    /// Detailed boon/condition operation payload. Required for categories
    /// AppliesBoon through TransfersCondition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_operation: Option<StatusOperation>,

    // TriggeredEffect inner category
    /// For `TriggeredEffect` category: the inner effect category that is triggered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_category: Option<EffectCategory>,

    // Sprint 2 (specs/005-wvw-proc-sites): prerequisites and scope
    /// Health prerequisite. Required for `OnHealthThreshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_threshold: Option<HealthThreshold>,
    /// Chance in (0, 1] that an `OnCrit` / `OnHit` trigger fires. Absent
    /// means certain.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub proc_chance: Option<FactualValue<f64>>,
    /// Which activating skills count. Absent means `Any`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_scope: Option<TriggerScope>,

    // Sprint 3 (specs/007-trait-triggers): prerequisite, scaling, coverage
    /// What must hold on the foe or the player for the record to fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisite: Option<Prerequisite>,
    /// Multiplier on a `GainsLifeForce` / `Heal` value at firing time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_by: Option<ScaleBy>,
    /// `Heal` only: added to `value` as coefficient x healing power.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_factual"
    )]
    pub healing_power_coefficient: Option<FactualValue<f64>>,
    /// Page numbers a derived `value` is computed from (Reaper's Onslaught:
    /// 300 ferocity as +20 % critical damage); the wiki check verifies these
    /// in place of `value`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<f64>,
    /// Classified, not executed: the trait's reason for the coverage line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageBlock>,
    /// NeedsMechanic Engine E2: lesser skill cast when this record fires.
    /// Ids come from the record payload (trait.skills), never a hardcoded map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast_skill_id: Option<u32>,

    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
    /// Conditions that must all hold for the record to fire or stay active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<Gate>,
    /// Live state added to `value` at firing time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
    /// Whose event fires this record. Absent means the player's.
    #[serde(default, skip_serializing_if = "Actor::is_player")]
    pub actor: Actor,
}

/// A single normalized effects file for one game mode in a specific patch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedEffectsFile {
    pub patch_id: String,
    pub mode: String,
    pub effects: Vec<NormalizedEffect>,
}

/// Container for all loaded normalized effects, keyed by (patch_id, mode).
#[derive(Debug)]
pub struct NormalizedEffectsData {
    /// Map from (patch_id, mode) to parsed effects file.
    files: HashMap<(String, String), NormalizedEffectsFile>,
}

impl NormalizedEffectsData {
    /// Look up all effects for a given patch and mode.
    pub fn effects_for(&self, patch_id: &str, mode: &str) -> Option<&[NormalizedEffect]> {
        self.files
            .get(&(patch_id.to_string(), mode.to_string()))
            .map(|f| f.effects.as_slice())
    }

    /// Exact `(patch_id, mode)` first, then walk that manifest's `inherits_from`.
    /// `sourced_patch` is the file that answered — not relabeled to `patch_id`.
    pub fn effects_for_resolved<'a>(
        &'a self,
        patch_id: &str,
        mode: &str,
    ) -> Option<(&'a [NormalizedEffect], &'a str)> {
        let manifests = super::manifests::manifests();
        let mut current = Some(patch_id);
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            if let Some(file) = self
                .files
                .values()
                .find(|f| f.patch_id == id && f.mode.eq_ignore_ascii_case(mode))
            {
                return Some((file.effects.as_slice(), file.patch_id.as_str()));
            }
            current = manifests
                .iter()
                .find(|m| m.patch_id == id)
                .and_then(|m| m.inherits_from.as_deref());
        }
        None
    }

    /// Effects for `mode` on the active manifest, then `inherits_from`.
    /// Never selects a file only because its mode matches.
    pub fn effects_for_mode(&self, mode: &str) -> &[NormalizedEffect] {
        let active = super::manifests::latest_manifest().patch_id.as_str();
        self.effects_for_resolved(active, mode)
            .map(|(effects, _)| effects)
            .unwrap_or(&[])
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// `(patch_id, mode)` for every embedded file. Historical snapshots stay labeled.
    pub fn loaded_snapshots(&self) -> Vec<(&str, &str)> {
        let mut keys: Vec<_> = self
            .files
            .values()
            .map(|f| (f.patch_id.as_str(), f.mode.as_str()))
            .collect();
        keys.sort_unstable();
        keys
    }

    /// Total number of effects across all files.
    pub fn effect_count(&self) -> usize {
        self.files.values().map(|f| f.effects.len()).sum()
    }
}

/// Parse and validate a single normalized effects file from JSON text.
pub fn load_effects_file(json: &str) -> Result<NormalizedEffectsFile, NormalizedEffectError> {
    let file: NormalizedEffectsFile = serde_json::from_str(json)?;
    validate_effects_file(&file)?;
    Ok(file)
}

fn validate_effects_file(file: &NormalizedEffectsFile) -> Result<(), NormalizedEffectError> {
    // 1. patch_id must not be empty
    if file.patch_id.is_empty() {
        return Err(NormalizedEffectError::ValidationError(
            "patch_id must not be empty".into(),
        ));
    }

    // 2. mode must be valid
    let valid_modes = ["PvE", "PvP", "WvW"];
    if !valid_modes.contains(&file.mode.as_str()) {
        return Err(NormalizedEffectError::ValidationError(format!(
            "invalid mode '{}', expected one of: PvE, PvP, WvW",
            file.mode
        )));
    }

    // 3. No duplicate effect_id within a file
    let mut seen_ids = HashSet::new();
    for effect in &file.effects {
        if !seen_ids.insert(&effect.effect_id) {
            return Err(NormalizedEffectError::ValidationError(format!(
                "duplicate effect_id: '{}'",
                effect.effect_id
            )));
        }

        // 4. If uptime_model.kind == Estimated AND evidence_level != Heuristic → error
        if effect.uptime_model.kind == UptimeModelKind::Estimated
            && effect.evidence_level != EvidenceLevel::Heuristic
        {
            return Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': Estimated uptime requires Heuristic evidence_level, got {:?}",
                effect.effect_id, effect.evidence_level
            )));
        }

        // 5. If trigger_rule == Passive AND internal_cooldown.is_some() → error
        if effect.trigger_rule == TriggerRule::Passive && effect.internal_cooldown.is_some() {
            return Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': Passive trigger_rule should not have internal_cooldown",
                effect.effect_id
            )));
        }

        // 6. Status operation categories should have status_operation payload
        if effect.category.is_status_operation() && effect.status_operation.is_none() {
            return Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': category {:?} requires status_operation payload",
                effect.effect_id, effect.category
            )));
        }

        // 7. TriggeredEffect must have inner_category
        if effect.category == EffectCategory::TriggeredEffect && effect.inner_category.is_none() {
            return Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': TriggeredEffect category requires inner_category",
                effect.effect_id
            )));
        }

        // 8. OnHealthThreshold must say which threshold
        if effect.trigger_rule == TriggerRule::OnHealthThreshold
            && effect.health_threshold.is_none()
        {
            return Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': OnHealthThreshold trigger_rule requires health_threshold",
                effect.effect_id
            )));
        }

        // 9. A stacking effect needs a duration to expire by, unless the
        // stacks are the foe's own condition stacks (Sprint 3: a Conditional
        // with a foe_condition prerequisite scales per stack of it).
        let per_foe_stack = effect.trigger_rule == TriggerRule::Conditional
            && effect
                .prerequisite
                .as_ref()
                .is_some_and(|p| p.foe_condition.is_some());
        if effect.max_stacks.is_some() && effect.effect_duration.is_none() && !per_foe_stack {
            return Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': max_stacks requires effect_duration",
                effect.effect_id
            )));
        }

        // Sprint 3 (specs/007-trait-triggers)
        let fail = |what: &str| {
            Err(NormalizedEffectError::ValidationError(format!(
                "effect '{}': {what}",
                effect.effect_id
            )))
        };
        // 10. An empty prerequisite says nothing
        if effect.prerequisite.as_ref().is_some_and(|p| p.is_empty()) {
            return fail("prerequisite must name at least one member");
        }
        // 11. A trait's on-skill-use needs to say which skills. An absent
        // scope is legal (three Sprint 1 records) and stays on the coverage
        // path unexecuted; an explicit `Any` is a mistake.
        if effect.source_type == SourceType::Trait
            && effect.trigger_rule == TriggerRule::OnSkillUse
            && matches!(effect.trigger_scope, Some(TriggerScope::Any))
        {
            return fail("Trait OnSkillUse requires a trigger_scope other than Any");
        }
        // 12. Periodic needs its period
        if effect.trigger_rule == TriggerRule::Periodic && effect.internal_cooldown.is_none() {
            return fail("Periodic trigger_rule requires internal_cooldown");
        }
        // 13. A coverage block carries no payload
        if let Some(cov) = &effect.coverage {
            if effect.status_operation.is_some()
                || effect.inner_category.is_some()
                || effect.prerequisite.is_some()
                || effect.value.is_resolved()
            {
                return fail(
                    "coverage forbids status_operation, inner_category, prerequisite and a resolved value",
                );
            }
            match (&cov.class, &cov.mechanic) {
                (CoverageClass::NeedsMechanic, None) => {
                    return fail("coverage NeedsMechanic requires mechanic")
                }
                (CoverageClass::PassiveNoEffect, Some(_)) => {
                    return fail("coverage mechanic is only for NeedsMechanic")
                }
                _ => {}
            }
        }
        // 14. Heal-only and life-force-only fields
        let payload = effect.inner_category.as_ref().unwrap_or(&effect.category);
        if effect.healing_power_coefficient.is_some() && *payload != EffectCategory::Heal {
            return fail("healing_power_coefficient is only for Heal");
        }
        if effect.scale_by.is_some()
            && !matches!(
                payload,
                EffectCategory::Heal | EffectCategory::GainsLifeForce
            )
        {
            return fail("scale_by is only for GainsLifeForce or Heal");
        }

        // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
        // 15. A coverage block makes no claim about when the source fires,
        // and NotApplicable is not a trigger anywhere else. Without this
        // pair a required field's filler value reads back as a fact.
        let is_not_applicable = effect.trigger_rule == TriggerRule::NotApplicable;
        if effect.coverage.is_some() != is_not_applicable {
            return fail(if is_not_applicable {
                "NotApplicable trigger_rule is only for a coverage block"
            } else {
                "a coverage block requires trigger_rule NotApplicable"
            });
        }
        // 16. A mechanic unlock is a capability, not a number.
        if let EffectCategory::MechanicUnlock { mechanic, .. } = payload {
            if mechanic.trim().is_empty() {
                return fail("MechanicUnlock requires a mechanic name");
            }
            if effect.value.is_resolved() {
                return fail("MechanicUnlock carries no value");
            }
        }
        // 17. Every gate says something executable.
        for gate in &effect.gates {
            match gate {
                Gate::Interval { every_ms, .. } if *every_ms == 0 => {
                    return fail("Interval gate requires every_ms > 0")
                }
                Gate::Weapon { types, .. } => {
                    if types.is_empty() {
                        return fail("Weapon gate requires at least one weapon type");
                    }
                    if let Some(bad) = types
                        .iter()
                        .find(|t| !super::weapon_hands::is_weapon_type(t))
                    {
                        return fail(&format!("Weapon gate names unknown weapon type '{bad}'"));
                    }
                }
                Gate::Proximity {
                    radius,
                    min_targets,
                } if *radius <= 0.0 || *min_targets == 0 => {
                    return fail("Proximity gate requires radius > 0 and min_targets >= 1")
                }
                Gate::HealthThreshold {
                    below_pct,
                    above_pct,
                    ..
                } => {
                    if below_pct.is_none() && above_pct.is_none() {
                        return fail("HealthThreshold gate requires below_pct or above_pct");
                    }
                    if let Some(pct) = [*below_pct, *above_pct]
                        .into_iter()
                        .flatten()
                        .find(|pct| !(1.0..=99.0).contains(pct))
                    {
                        return fail(&format!(
                            "HealthThreshold gate percent {pct} is outside 1..=99"
                        ));
                    }
                }
                // A boon the formula table does not know is a typo that
                // would otherwise read as a gate that simply never opens.
                Gate::SelfBoon { boon } if super::boons().get(boon).is_none() => {
                    return fail(&format!("SelfBoon gate names unknown boon '{boon}'"))
                }
                Gate::SelfResourceStacks { resource, min }
                    if resource.trim().is_empty() || *min == 0 =>
                {
                    return fail("SelfResourceStacks gate requires a resource and min >= 1")
                }
                _ => {}
            }
        }
        // 18. A scale that scales by nothing is a typo.
        if let Some(scale) = &effect.scale {
            let (step, cap, name) = match scale {
                Scale::PerDistance { per_unit, cap } => (*per_unit, *cap, ""),
                Scale::PerSelfResourceStack {
                    resource,
                    per_stack,
                    cap,
                } => (*per_stack, *cap, resource.as_str()),
                Scale::PerSelfBoon {
                    boon,
                    per_stack,
                    cap,
                } => (*per_stack, *cap, boon.as_str()),
            };
            if !step.is_finite() || step == 0.0 {
                return fail("scale step must be finite and non-zero");
            }
            if cap.is_some_and(|c| c <= 0.0) {
                return fail("scale cap must be positive");
            }
            if !matches!(scale, Scale::PerDistance { .. }) && name.trim().is_empty() {
                return fail("scale requires a resource or boon name");
            }
            if let Scale::PerSelfBoon { boon, .. } = scale {
                if super::boons().get(boon).is_none() {
                    return fail(&format!("PerSelfBoon scale names unknown boon '{boon}'"));
                }
            }
        }
    }

    Ok(())
}

/// Load and validate all three baseline effects files.
fn load_all_effects() -> Result<NormalizedEffectsData, NormalizedEffectError> {
    let mut files = HashMap::new();

    for (json, expected_mode) in [
        (PVE_EFFECTS_JSON, "PvE"),
        (PVP_EFFECTS_JSON, "PvP"),
        (WVW_EFFECTS_JSON, "WvW"),
    ] {
        let file = load_effects_file(json)?;

        // Validate mode matches expected
        if file.mode != expected_mode {
            return Err(NormalizedEffectError::ValidationError(format!(
                "expected mode '{}', got '{}'",
                expected_mode, file.mode
            )));
        }

        files.insert((file.patch_id.clone(), file.mode.clone()), file);
    }

    Ok(NormalizedEffectsData { files })
}

/// Test-only re-export so the alias-routing regression suite can fuzz the
/// shared condition-importance score table directly without constructing a
/// `NormalizedEffect`.
#[cfg(test)]
pub(crate) mod tests_alias_helpers {
    pub(crate) fn cond_importance_for_status_kind(raw_kind: &str) -> f64 {
        super::super::boon_condition_formulas::condition_importance(raw_kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_effect(effect_id: &str) -> NormalizedEffect {
        NormalizedEffect {
            effect_id: effect_id.to_string(),
            source_type: SourceType::Trait,
            source_id: 100,
            source_name: "Test Trait".to_string(),
            category: EffectCategory::FlatStat,
            value: FactualValue::Resolved(150.0),
            stacking_rule: StackingRule::Additive,
            trigger_rule: TriggerRule::Passive,
            uptime_model: UptimeModel {
                kind: UptimeModelKind::AlwaysOn,
                uptime: None,
            },
            evidence_level: EvidenceLevel::Factual,
            source: None,
            effect_duration: None,
            internal_cooldown: None,
            max_stacks: None,
            status_operation: None,
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
            actor: Actor::Player,
        }
    }

    fn full_effect() -> NormalizedEffect {
        NormalizedEffect {
            effect_id: "trait_214_might_on_crit".to_string(),
            source_type: SourceType::Trait,
            source_id: 214,
            source_name: "Signet of Fury".to_string(),
            category: EffectCategory::AppliesBoon,
            value: FactualValue::Resolved(1.0),
            stacking_rule: StackingRule::NonStacking,
            trigger_rule: TriggerRule::OnCrit,
            uptime_model: UptimeModel {
                kind: UptimeModelKind::Estimated,
                uptime: Some(FactualValue::Resolved(0.6)),
            },
            evidence_level: EvidenceLevel::Heuristic,
            source: Some("https://wiki.guildwars2.com/wiki/Signet_of_Fury".to_string()),
            effect_duration: Some(FactualValue::Resolved(10.0)),
            internal_cooldown: Some(FactualValue::Resolved(1.0)),
            max_stacks: Some(FactualValue::Resolved(25)),
            status_operation: Some(StatusOperation {
                operation_type: OperationType::AppliesBoon,
                target_side: TargetSide::Self_,
                status_kind: "Might".to_string(),
                amount_mode: AmountMode::Stacks,
                amount_value: FactualValue::Resolved(1.0),
                base_duration_ms: Some(FactualValue::Resolved(8000)),
                target_scope: TargetScope::Self_,
                target_count: None,
                internal_cooldown_ms: Some(FactualValue::Resolved(1000)),
                source_duration_multiplier: Some(FactualValue::Resolved(1.0)),
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
            actor: Actor::Player,
        }
    }

    // Serde round-trip for each enum

    #[test]
    fn test_serde_roundtrip_source_type() {
        let variants = vec![
            SourceType::Trait,
            SourceType::Skill,
            SourceType::Rune,
            SourceType::Sigil,
            SourceType::Relic,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: SourceType = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_all_22_effect_categories() {
        let variants = vec![
            EffectCategory::FlatStat,
            EffectCategory::StatConversion,
            EffectCategory::StrikeDamagePct,
            EffectCategory::ConditionDamagePct,
            EffectCategory::SpecificConditionDamagePct,
            EffectCategory::CritDamagePct,
            EffectCategory::BoonDurationPct,
            EffectCategory::ConditionDurationPct,
            EffectCategory::SpecificConditionDurationPct,
            EffectCategory::OutgoingHealingPct,
            EffectCategory::IncomingStrikeMultiplier,
            EffectCategory::IncomingConditionMultiplier,
            EffectCategory::AppliesBoon,
            EffectCategory::AppliesCondition,
            EffectCategory::RemovesBoon,
            EffectCategory::CorruptsBoon,
            EffectCategory::RemovesCondition,
            EffectCategory::ConvertsConditionToBoon,
            EffectCategory::TransfersCondition,
            EffectCategory::DefianceDamage,
            EffectCategory::ProcEffect,
            EffectCategory::TriggeredEffect,
            EffectCategory::GainsLifeForce,
            EffectCategory::Heal,
            EffectCategory::CritChancePct,
            EffectCategory::SpawnClone,
        ];
        assert_eq!(
            variants.len(),
            26,
            "must test all 26 EffectCategory variants"
        );
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: EffectCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed, "roundtrip failed for {:?}", v);
        }
    }

    #[test]
    fn test_serde_roundtrip_stacking_rule() {
        let variants = vec![
            StackingRule::Multiplicative,
            StackingRule::Additive,
            StackingRule::Highest,
            StackingRule::NonStacking,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: StackingRule = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_trigger_rule() {
        let variants = vec![
            TriggerRule::Passive,
            TriggerRule::OnCrit,
            TriggerRule::OnHit,
            TriggerRule::OnSkillUse,
            TriggerRule::OnHealthThreshold,
            TriggerRule::Conditional,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: TriggerRule = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_uptime_model_kind() {
        let variants = vec![
            UptimeModelKind::AlwaysOn,
            UptimeModelKind::Estimated,
            UptimeModelKind::Derived,
            UptimeModelKind::Unknown,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: UptimeModelKind = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_operation_type() {
        let variants = vec![
            OperationType::AppliesBoon,
            OperationType::RemovesBoon,
            OperationType::CorruptsBoon,
            OperationType::AppliesCondition,
            OperationType::RemovesCondition,
            OperationType::ConvertsConditionToBoon,
            OperationType::TransfersCondition,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: OperationType = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_target_side() {
        let variants = vec![TargetSide::Self_, TargetSide::Ally, TargetSide::Enemy];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: TargetSide = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_amount_mode() {
        let variants = vec![
            AmountMode::Stacks,
            AmountMode::DurationMs,
            AmountMode::Charges,
            AmountMode::Count,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: AmountMode = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_target_scope() {
        let variants = vec![
            TargetScope::Self_,
            TargetScope::SingleTarget,
            TargetScope::NearbyAllies,
            TargetScope::Party,
            TargetScope::Squad,
            TargetScope::Area,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: TargetScope = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_evidence_level() {
        let variants = vec![
            EvidenceLevel::Factual,
            EvidenceLevel::Derived,
            EvidenceLevel::Heuristic,
            EvidenceLevel::Unknown,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: EvidenceLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed);
        }
    }

    // Serde round-trip for NormalizedEffect with all fields

    #[test]
    fn test_serde_roundtrip_full_effect() {
        let effect = full_effect();
        let json = serde_json::to_string_pretty(&effect).unwrap();
        let parsed: NormalizedEffect = serde_json::from_str(&json).unwrap();
        assert_eq!(effect, parsed);
    }

    // Serde round-trip for NormalizedEffect with minimal fields

    #[test]
    fn test_serde_roundtrip_minimal_effect() {
        let effect = minimal_effect("test_minimal");
        let json = serde_json::to_string(&effect).unwrap();
        let parsed: NormalizedEffect = serde_json::from_str(&json).unwrap();
        assert_eq!(effect, parsed);

        // Verify optional fields are absent from JSON
        let json_value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            json_value.get("source").is_none(),
            "source should be skipped"
        );
        assert!(
            json_value.get("effect_duration").is_none(),
            "effect_duration should be skipped"
        );
        assert!(
            json_value.get("internal_cooldown").is_none(),
            "internal_cooldown should be skipped"
        );
        assert!(
            json_value.get("max_stacks").is_none(),
            "max_stacks should be skipped"
        );
        assert!(
            json_value.get("status_operation").is_none(),
            "status_operation should be skipped"
        );
        assert!(
            json_value.get("inner_category").is_none(),
            "inner_category should be skipped"
        );
    }

    // NormalizedEffectsFile with empty effects array

    #[test]
    fn test_effects_file_empty_effects() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvE",
            "effects": []
        }"#;
        let file = load_effects_file(json).expect("should parse");
        assert_eq!(file.patch_id, "2026-01-13");
        assert_eq!(file.mode, "PvE");
        assert!(file.effects.is_empty());
    }

    // Validation: duplicate effect_id → error

    #[test]
    fn test_validation_duplicate_effect_id() {
        let effect1 = minimal_effect("dup_id");
        let effect2 = minimal_effect("dup_id");
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect1, effect2],
        };
        let result = validate_effects_file(&file);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("duplicate effect_id"),
            "expected duplicate error, got: {}",
            err
        );
    }

    // Validation: Estimated uptime with Factual evidence → error

    #[test]
    fn test_validation_estimated_uptime_requires_heuristic() {
        let mut effect = minimal_effect("bad_uptime");
        effect.uptime_model = UptimeModel {
            kind: UptimeModelKind::Estimated,
            uptime: Some(FactualValue::Resolved(0.5)),
        };
        effect.evidence_level = EvidenceLevel::Factual; // Wrong! Should be Heuristic.
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Estimated uptime requires Heuristic"),
            "expected uptime/evidence error, got: {}",
            err
        );
    }

    // Validation: Passive trigger with ICD → error

    #[test]
    fn test_validation_passive_with_icd() {
        let mut effect = minimal_effect("passive_icd");
        effect.trigger_rule = TriggerRule::Passive;
        effect.internal_cooldown = Some(FactualValue::Resolved(1.0));
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Passive trigger_rule should not have internal_cooldown"),
            "expected passive/ICD error, got: {}",
            err
        );
    }

    // Validation: TriggeredEffect without inner_category → error

    #[test]
    fn test_validation_health_threshold_required_for_on_health_threshold() {
        let mut effect = minimal_effect("threshold_missing");
        effect.trigger_rule = TriggerRule::OnHealthThreshold;
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "WvW".to_string(),
            effects: vec![effect.clone()],
        };
        let err = validate_effects_file(&file).unwrap_err();
        assert!(
            err.to_string().contains("requires health_threshold"),
            "expected threshold error, got: {err}"
        );
        effect.health_threshold = Some(HealthThreshold {
            above: true,
            percent: FactualValue::Resolved(90.0),
        });
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "WvW".to_string(),
            effects: vec![effect],
        };
        assert!(validate_effects_file(&file).is_ok());
    }

    #[test]
    fn test_validation_max_stacks_requires_duration() {
        let mut effect = minimal_effect("stacks_no_duration");
        effect.max_stacks = Some(FactualValue::Resolved(5));
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "WvW".to_string(),
            effects: vec![effect.clone()],
        };
        let err = validate_effects_file(&file).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_stacks requires effect_duration"),
            "expected stacks/duration error, got: {err}"
        );
        effect.effect_duration = Some(FactualValue::Resolved(6.0));
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "WvW".to_string(),
            effects: vec![effect],
        };
        assert!(validate_effects_file(&file).is_ok());
    }

    #[test]
    fn test_sprint2_fields_roundtrip_and_default_absent() {
        let mut effect = full_effect();
        effect.trigger_rule = TriggerRule::OnHealthThreshold;
        effect.health_threshold = Some(HealthThreshold {
            above: true,
            percent: FactualValue::Resolved(90.0),
        });
        effect.proc_chance = Some(FactualValue::Resolved(0.5));
        effect.trigger_scope = Some(TriggerScope::WeaponSkillWithRecharge);
        let json = serde_json::to_string(&effect).unwrap();
        assert!(json.contains("\"health_threshold\""));
        assert!(json.contains("\"proc_chance\":0.5"));
        assert!(json.contains("\"WeaponSkillWithRecharge\""));
        let parsed: NormalizedEffect = serde_json::from_str(&json).unwrap();
        assert_eq!(effect, parsed);

        let plain = serde_json::to_string(&minimal_effect("plain")).unwrap();
        assert!(!plain.contains("health_threshold"));
        let parsed: NormalizedEffect = serde_json::from_str(&plain).unwrap();
        assert!(parsed.health_threshold.is_none());
        assert!(parsed.proc_chance.is_none());
        assert!(parsed.trigger_scope.is_none());
    }

    #[test]
    fn test_validation_triggered_effect_requires_inner_category() {
        let mut effect = minimal_effect("bad_triggered");
        effect.category = EffectCategory::TriggeredEffect;
        effect.inner_category = None; // Missing!
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("TriggeredEffect category requires inner_category"),
            "expected inner_category error, got: {}",
            err
        );
    }

    // Validation: AppliesBoon without status_operation → warning (error)

    #[test]
    fn test_validation_applies_boon_requires_status_operation() {
        let mut effect = minimal_effect("boon_no_op");
        effect.category = EffectCategory::AppliesBoon;
        effect.status_operation = None; // Missing!
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("requires status_operation payload"),
            "expected status_operation error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_all_status_categories_require_operation() {
        let status_categories = vec![
            EffectCategory::AppliesBoon,
            EffectCategory::AppliesCondition,
            EffectCategory::RemovesBoon,
            EffectCategory::CorruptsBoon,
            EffectCategory::RemovesCondition,
            EffectCategory::ConvertsConditionToBoon,
            EffectCategory::TransfersCondition,
        ];
        for cat in status_categories {
            let mut effect = minimal_effect("status_test");
            effect.category = cat.clone();
            effect.status_operation = None;
            let file = NormalizedEffectsFile {
                patch_id: "2026-01-13".to_string(),
                mode: "PvE".to_string(),
                effects: vec![effect],
            };
            let result = validate_effects_file(&file);
            assert!(
                result.is_err(),
                "category {:?} should require status_operation",
                cat
            );
        }
    }

    // Loader: baseline files parse successfully

    /// Sprint 2 (T043): every record that uses this sprint's fields, or the
    /// coefficient form of a proc, cites a dated wiki read.
    fn wvw_file(effects: Vec<NormalizedEffect>) -> NormalizedEffectsFile {
        NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "WvW".to_string(),
            effects,
        }
    }

    fn rejected(effect: NormalizedEffect, fragment: &str) {
        let err = validate_effects_file(&wvw_file(vec![effect])).unwrap_err();
        assert!(
            err.to_string().contains(fragment),
            "expected '{fragment}', got: {err}"
        );
    }

    /// Sprint 3: every new trigger kind and scope survives a JSON round trip
    /// in the contract's spelling, and absent fields stay absent.
    #[test]
    fn sprint3_trigger_kinds_and_scopes_roundtrip() {
        for (rule, text) in [
            (TriggerRule::OnShroudEnter, "\"OnShroudEnter\""),
            (TriggerRule::OnShroudExit, "\"OnShroudExit\""),
            (TriggerRule::OnConditionApplied, "\"OnConditionApplied\""),
            (TriggerRule::OnConditionRemoved, "\"OnConditionRemoved\""),
            (TriggerRule::OnBoonApplied, "\"OnBoonApplied\""),
            (TriggerRule::OnBoonStripped, "\"OnBoonStripped\""),
            (TriggerRule::Periodic, "\"Periodic\""),
            (TriggerRule::OnDodge, "\"OnDodge\""),
            (TriggerRule::OnDisableFoe, "\"OnDisableFoe\""),
            (TriggerRule::OnElite, "\"OnElite\""),
            (TriggerRule::OnThreshold, "\"OnThreshold\""),
            (TriggerRule::OnAttunementSwap, "\"OnAttunementSwap\""),
            (TriggerRule::OnCloneCreated, "\"OnCloneCreated\""),
        ] {
            let mut effect = minimal_effect("kind");
            effect.trigger_rule = rule.clone();
            let json = serde_json::to_string(&effect).unwrap();
            assert!(json.contains(text), "{json}");
            let parsed: NormalizedEffect = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.trigger_rule, rule);
        }
        for (scope, text) in [
            (
                TriggerScope::Category("Shout".into()),
                "{\"Category\":\"Shout\"}",
            ),
            (TriggerScope::Slot("Elite".into()), "{\"Slot\":\"Elite\"}"),
            (TriggerScope::Status("Fear".into()), "{\"Status\":\"Fear\"}"),
        ] {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(json, text);
            let parsed: TriggerScope = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, scope);
        }
        let mut effect = minimal_effect("full");
        effect.category = EffectCategory::Heal;
        effect.trigger_rule = TriggerRule::OnCrit;
        effect.prerequisite = Some(Prerequisite {
            foe_condition: Some("Chilled".into()),
            in_shroud: Some(true),
            foe_health: Some(HealthThreshold {
                above: false,
                percent: FactualValue::Resolved(50.0),
            }),
            attunement: None,
        });
        effect.scale_by = Some(ScaleBy::ConditionsRemoved);
        effect.healing_power_coefficient = Some(FactualValue::Resolved(0.1));
        let json = serde_json::to_string(&effect).unwrap();
        assert!(json.contains("\"foe_condition\":\"Chilled\""), "{json}");
        assert!(json.contains("\"in_shroud\":true"), "{json}");
        assert!(
            json.contains("\"scale_by\":\"ConditionsRemoved\""),
            "{json}"
        );
        assert!(json.contains("\"healing_power_coefficient\":0.1"), "{json}");
        let parsed: NormalizedEffect = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, effect);
        assert!(validate_effects_file(&wvw_file(vec![effect])).is_ok());

        let plain: NormalizedEffect =
            serde_json::from_str(&serde_json::to_string(&minimal_effect("plain")).unwrap())
                .unwrap();
        assert!(plain.prerequisite.is_none() && plain.coverage.is_none());
        assert!(plain.scale_by.is_none() && plain.healing_power_coefficient.is_none());
    }

    /// Sprint 3: one rejected case per validation rule, each naming its error.
    #[test]
    fn sprint3_validation_rules_reject_with_their_text() {
        let mut e = minimal_effect("empty_prereq");
        e.prerequisite = Some(Prerequisite::default());
        rejected(e, "prerequisite must name at least one member");

        let mut e = minimal_effect("trait_skill_use");
        e.trigger_rule = TriggerRule::OnSkillUse;
        assert!(validate_effects_file(&wvw_file(vec![e.clone()])).is_ok());
        e.trigger_scope = Some(TriggerScope::Any);
        rejected(
            e.clone(),
            "Trait OnSkillUse requires a trigger_scope other than Any",
        );
        e.trigger_scope = Some(TriggerScope::Category("Shout".into()));
        assert!(validate_effects_file(&wvw_file(vec![e])).is_ok());

        let mut e = minimal_effect("periodic");
        e.trigger_rule = TriggerRule::Periodic;
        rejected(
            e.clone(),
            "Periodic trigger_rule requires internal_cooldown",
        );
        e.internal_cooldown = Some(FactualValue::Resolved(3.0));
        assert!(validate_effects_file(&wvw_file(vec![e])).is_ok());

        let mut cov = minimal_effect("coverage");
        cov.value = FactualValue::Unknown;
        // Sprint 4 rule 15: a coverage block makes no claim about when the
        // source fires, so `Passive` is no longer legal filler here.
        cov.trigger_rule = TriggerRule::NotApplicable;
        cov.coverage = Some(CoverageBlock {
            class: CoverageClass::NeedsMechanic,
            mechanic: Some("minions".into()),
        });
        assert!(validate_effects_file(&wvw_file(vec![cov.clone()])).is_ok());
        let mut e = cov.clone();
        e.value = FactualValue::Resolved(1.0);
        rejected(e, "coverage forbids status_operation");
        let mut e = cov.clone();
        e.inner_category = Some(EffectCategory::Heal);
        rejected(e, "coverage forbids status_operation");
        let mut e = cov.clone();
        e.coverage.as_mut().unwrap().mechanic = None;
        rejected(e, "coverage NeedsMechanic requires mechanic");
        let mut e = cov.clone();
        e.coverage = Some(CoverageBlock {
            class: CoverageClass::PassiveNoEffect,
            mechanic: Some("minions".into()),
        });
        rejected(e, "coverage mechanic is only for NeedsMechanic");

        let mut e = minimal_effect("hp_coeff");
        e.healing_power_coefficient = Some(FactualValue::Resolved(0.1));
        rejected(e, "healing_power_coefficient is only for Heal");
        let mut e = minimal_effect("scale");
        e.scale_by = Some(ScaleBy::ConditionsRemoved);
        rejected(e.clone(), "scale_by is only for GainsLifeForce or Heal");
        e.category = EffectCategory::TriggeredEffect;
        e.inner_category = Some(EffectCategory::GainsLifeForce);
        e.trigger_rule = TriggerRule::OnShroudExit;
        assert!(validate_effects_file(&wvw_file(vec![e])).is_ok());
    }

    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)

    /// The cheapest and most load-bearing rule of the gate: a coverage block
    /// says nothing about when its source fires, so the required field cannot be
    /// filled with `Passive` and read back later as 145 passive minor traits.
    #[test]
    fn coverage_pairs_with_not_applicable_and_nothing_else() {
        let mut cov = minimal_effect("coverage");
        cov.value = FactualValue::Unknown;
        cov.trigger_rule = TriggerRule::NotApplicable;
        cov.coverage = Some(CoverageBlock {
            class: CoverageClass::NeedsMechanic,
            mechanic: Some("celestial avatar".into()),
        });
        assert!(validate_effects_file(&wvw_file(vec![cov.clone()])).is_ok());

        let mut passive = cov.clone();
        passive.trigger_rule = TriggerRule::Passive;
        rejected(
            passive,
            "a coverage block requires trigger_rule NotApplicable",
        );

        let mut loose = minimal_effect("loose");
        loose.trigger_rule = TriggerRule::NotApplicable;
        rejected(
            loose,
            "NotApplicable trigger_rule is only for a coverage block",
        );
    }

    #[test]
    fn gate_and_scale_validation_rejects_with_their_text() {
        let gated = |gate: Gate| {
            let mut e = minimal_effect("gated");
            e.gates = vec![gate];
            e
        };
        rejected(
            gated(Gate::Interval {
                every_ms: 0,
                while_state: None,
            }),
            "Interval gate requires every_ms > 0",
        );
        rejected(
            gated(Gate::Weapon {
                types: Vec::new(),
                hand: None,
            }),
            "Weapon gate requires at least one weapon type",
        );
        rejected(
            gated(Gate::Weapon {
                types: vec!["Chainsaw".into()],
                hand: None,
            }),
            "Weapon gate names unknown weapon type 'Chainsaw'",
        );
        rejected(
            gated(Gate::Proximity {
                radius: 0.0,
                min_targets: 1,
            }),
            "Proximity gate requires radius > 0",
        );
        rejected(
            gated(Gate::HealthThreshold {
                below_pct: None,
                above_pct: None,
                rearm: Rearm::Icd,
            }),
            "HealthThreshold gate requires below_pct or above_pct",
        );
        rejected(
            gated(Gate::HealthThreshold {
                below_pct: Some(100.0),
                above_pct: None,
                rearm: Rearm::Icd,
            }),
            "outside 1..=99",
        );
        rejected(
            gated(Gate::SelfResourceStacks {
                resource: "initiative".into(),
                min: 0,
            }),
            "SelfResourceStacks gate requires a resource and min >= 1",
        );
        // A real weapon on a real hand is accepted.
        assert!(validate_effects_file(&wvw_file(vec![gated(Gate::Weapon {
            types: vec!["Torch".into(), "Greatsword".into()],
            hand: Some(WeaponHand::Off),
        })]))
        .is_ok());

        let mut zero_step = minimal_effect("scale");
        zero_step.scale = Some(Scale::PerDistance {
            per_unit: 0.0,
            cap: None,
        });
        rejected(zero_step, "scale step must be finite and non-zero");
        let mut bad_cap = minimal_effect("scale");
        bad_cap.scale = Some(Scale::PerSelfBoon {
            boon: "Might".into(),
            per_stack: 1.0,
            cap: Some(0.0),
        });
        rejected(bad_cap, "scale cap must be positive");
        let mut nameless = minimal_effect("scale");
        nameless.scale = Some(Scale::PerSelfResourceStack {
            resource: "  ".into(),
            per_stack: 1.0,
            cap: None,
        });
        rejected(nameless, "scale requires a resource or boon name");

        // A boon name the formula table does not know would otherwise be a gate
        // that silently never opens.
        rejected(
            gated(Gate::SelfBoon {
                boon: "Quackness".into(),
            }),
            "SelfBoon gate names unknown boon 'Quackness'",
        );
        assert!(validate_effects_file(&wvw_file(vec![gated(Gate::SelfBoon {
            boon: "Quickness".into(),
        })]))
        .is_ok());
        let mut bad_boon = minimal_effect("scale");
        bad_boon.scale = Some(Scale::PerSelfBoon {
            boon: "Mihgt".into(),
            per_stack: 1.0,
            cap: None,
        });
        rejected(bad_boon, "PerSelfBoon scale names unknown boon 'Mihgt'");
    }

    #[test]
    fn mechanic_unlock_is_a_capability_not_a_number() {
        let mut unlock = minimal_effect("unlock");
        unlock.category = EffectCategory::MechanicUnlock {
            mechanic: "Attunement".into(),
            replaces: Some("Dual Attunement".into()),
        };
        unlock.value = FactualValue::Unknown;
        assert!(validate_effects_file(&wvw_file(vec![unlock.clone()])).is_ok());

        let mut valued = unlock.clone();
        valued.value = FactualValue::Resolved(5.0);
        rejected(valued, "MechanicUnlock carries no value");

        let mut nameless = unlock;
        nameless.category = EffectCategory::MechanicUnlock {
            mechanic: " ".into(),
            replaces: None,
        };
        rejected(nameless, "MechanicUnlock requires a mechanic name");
    }

    #[test]
    fn sprint4_fields_roundtrip_and_stay_absent_by_default() {
        let plain = minimal_effect("plain");
        let json = serde_json::to_string(&plain).expect("serialize");
        for absent in ["gates", "scale", "actor"] {
            assert!(!json.contains(absent), "{absent} must not be written");
        }
        assert_eq!(
            serde_json::from_str::<NormalizedEffect>(&json).expect("parse"),
            plain
        );

        let mut full = minimal_effect("full");
        full.actor = Actor::Illusion;
        full.stacking_rule = StackingRule::RefreshAllStacks;
        full.trigger_rule = TriggerRule::OnBoonGained {
            boon: Some("Fury".into()),
        };
        full.gates = vec![
            Gate::InCombat,
            Gate::Interval {
                every_ms: 3_000,
                while_state: Some(StateGate {
                    in_shroud: Some(false),
                    ..Default::default()
                }),
            },
            Gate::Weapon {
                types: vec!["Torch".into()],
                hand: Some(WeaponHand::Off),
            },
            Gate::Positional(Positional::Behind),
            Gate::Proximity {
                radius: 360.0,
                min_targets: 3,
            },
            Gate::HealthThreshold {
                below_pct: Some(50.0),
                above_pct: None,
                rearm: Rearm::WhenRecovered,
            },
            Gate::SelfBoon {
                boon: "Quickness".into(),
            },
            Gate::SelfResourceStacks {
                resource: "initiative".into(),
                min: 3,
            },
        ];
        full.scale = Some(Scale::PerSelfResourceStack {
            resource: "initiative".into(),
            per_stack: 2.0,
            cap: Some(10.0),
        });
        let json = serde_json::to_string(&full).expect("serialize");
        assert!(json.contains("\"while\""), "the state gate keeps its key");
        assert_eq!(
            serde_json::from_str::<NormalizedEffect>(&json).expect("parse"),
            full
        );
    }

    /// Gate 1 ratchet: coverage blocks only ever turn into real records, and
    /// a coverage block is exactly a `NotApplicable` trigger (rule 15).
    #[test]
    fn gate1_coverage_only_ratchets_down() {
        for (json, mode) in [
            (PVE_EFFECTS_JSON, "PvE"),
            (PVP_EFFECTS_JSON, "PvP"),
            (WVW_EFFECTS_JSON, "WvW"),
        ] {
            let file = load_effects_file(json).expect("embedded file loads");
            assert_eq!(file.mode, mode);
            assert!(file
                .effects
                .iter()
                .all(|e| (e.trigger_rule == TriggerRule::NotApplicable) == e.coverage.is_some()));
        }
        // 582 after the 2026-09-22 migration; 581 after the Ele/Engi/Guardian/
        // Ranger minor-trait increment (2026-09-23: 209 minors recorded, the
        // remaining blocks each name the trigger or field the format lacks).
        // Lower this, never raise it.
        let wvw = load_effects_file(WVW_EFFECTS_JSON).expect("WvW loads");
        let coverage = wvw.effects.iter().filter(|e| e.coverage.is_some()).count();
        assert!(coverage <= 581, "coverage blocks grew to {coverage}");
    }

    /// Sprint 3: the fourteen Sprint 2 WvW records load unchanged.
    #[test]
    fn sprint2_records_still_load() {
        let file: NormalizedEffectsFile = serde_json::from_str(WVW_EFFECTS_JSON).unwrap();
        validate_effects_file(&file).unwrap();
        const SPRINT2_IDS: [&str; 14] = [
            "trait:1338:0",
            "rune:24836:0",
            "rune:24836:1",
            "sigil:24615:0",
            "sigil:44944:0",
            "trait:1711:0",
            "trait:1069:0",
            "sigil:24548:0",
            "trait:681:0",
            "trait:1693:0",
            "skill:9120:0",
            "trait:2013:0",
            "trait:553:0",
            "relic:100916:0",
        ];
        let sprint2: Vec<&NormalizedEffect> = file
            .effects
            .iter()
            .filter(|e| SPRINT2_IDS.contains(&e.effect_id.as_str()))
            .collect();
        assert_eq!(sprint2.len(), 14, "the Sprint 2 regression set");
        assert!(sprint2
            .iter()
            .all(|e| e.coverage.is_none() && e.prerequisite.is_none()));
    }

    #[test]
    fn records_this_sprint_carry_read_dates() {
        let data = effects();
        for mode in ["PvE", "PvP", "WvW"] {
            for effect in data.effects_for_mode(mode) {
                let sprint2 = effect.health_threshold.is_some()
                    || effect.proc_chance.is_some()
                    || effect.trigger_scope.is_some()
                    || (effect.category == EffectCategory::ProcEffect
                        && effect.value.is_resolved()
                        && matches!(effect.value, FactualValue::Resolved(v) if v <= 2.0));
                // Sprint 3 (specs/007-trait-triggers): a prerequisite, a
                // coverage block, a scale, a healing coefficient, a new
                // trigger kind or a new category all need a dated source.
                let sprint3 = effect.prerequisite.is_some()
                    || effect.coverage.is_some()
                    || effect.scale_by.is_some()
                    || effect.healing_power_coefficient.is_some()
                    || matches!(
                        effect.trigger_rule,
                        TriggerRule::OnShroudEnter
                            | TriggerRule::OnShroudExit
                            | TriggerRule::OnConditionApplied
                            | TriggerRule::OnConditionRemoved
                            | TriggerRule::OnBoonApplied
                            | TriggerRule::OnBoonStripped
                            | TriggerRule::Periodic
                            | TriggerRule::OnDodge
                            | TriggerRule::OnDisableFoe
                            | TriggerRule::OnElite
                            | TriggerRule::OnThreshold
                            | TriggerRule::OnAttunementSwap
                            | TriggerRule::OnCloneCreated
                    )
                    || matches!(
                        effect.inner_category.as_ref().unwrap_or(&effect.category),
                        EffectCategory::GainsLifeForce | EffectCategory::Heal
                    );
                if sprint2 || sprint3 {
                    assert!(
                        effect
                            .source
                            .as_deref()
                            .is_some_and(|s| s.contains("(read 20")),
                        "{mode} {} has no dated source: {:?}",
                        effect.effect_id,
                        effect.source
                    );
                }
            }
        }
    }

    #[test]
    fn test_embedded_effects_load_successfully() {
        let data = effects();
        assert_eq!(
            data.file_count(),
            3,
            "expected 3 effects files (PvE, PvP, WvW)"
        );
        // P3-10b populates baseline with representative effects
        assert!(
            data.effect_count() >= 20,
            "expected at least 20 total effects, got {}",
            data.effect_count(),
        );
    }

    #[test]
    fn test_try_load_returns_ok() {
        let result = try_load_normalized_effects();
        assert!(
            result.is_ok(),
            "try_load should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_effects_for_returns_populated_slices() {
        let data = effects();
        let pve = data.effects_for("2026-01-13", "PvE");
        assert!(pve.is_some(), "PvE effects should exist");
        assert!(
            !pve.unwrap().is_empty(),
            "PvE should have populated effects after P3-10b"
        );

        let pvp = data.effects_for("2026-01-13", "PvP");
        assert!(pvp.is_some(), "PvP effects should exist");
        assert!(
            !pvp.unwrap().is_empty(),
            "PvP should have populated effects after P3-10b"
        );

        let wvw = data.effects_for("2026-01-13", "WvW");
        assert!(wvw.is_some(), "WvW effects should exist");
        assert!(
            !wvw.unwrap().is_empty(),
            "WvW should have populated effects after P3-10b"
        );
    }

    #[test]
    fn test_effects_for_unknown_returns_none() {
        let data = effects();
        assert!(data.effects_for("9999-99-99", "PvE").is_none());
        assert!(data.effects_for("2026-01-13", "Ranked").is_none());
        assert!(
            data.effects_for("2026-07-15", "WvW").is_none(),
            "active snapshot has no own NE file — do not invent one"
        );
        let (wvw, sourced) = data
            .effects_for_resolved("2026-07-15", "WvW")
            .expect("active patch inherits_from 2026-01-13");
        assert_eq!(sourced, "2026-01-13");
        assert!(!wvw.is_empty());
    }

    // Loader: malformed JSON → DataLoadError

    #[test]
    fn test_malformed_json_returns_error() {
        let result = load_effects_file("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("JSON parse error"),
            "expected parse error, got: {}",
            err,
        );
    }

    // Full NormalizedEffect with StatusOperation deserialization

    #[test]
    fn test_full_effect_with_status_operation_from_json() {
        let json = r#"{
            "patch_id": "2026-01-13",
            "mode": "PvE",
            "effects": [
                {
                    "effect_id": "trait_214_might_on_crit",
                    "source_type": "Trait",
                    "source_id": 214,
                    "source_name": "Signet of Fury",
                    "category": "AppliesBoon",
                    "value": 1.0,
                    "stacking_rule": "NonStacking",
                    "trigger_rule": "OnCrit",
                    "uptime_model": {
                        "kind": "Estimated",
                        "uptime": 0.6
                    },
                    "evidence_level": "Heuristic",
                    "source": "https://wiki.guildwars2.com/wiki/Signet_of_Fury",
                    "effect_duration": 10.0,
                    "internal_cooldown": 1.0,
                    "max_stacks": 25,
                    "status_operation": {
                        "operation_type": "AppliesBoon",
                        "target_side": "self",
                        "status_kind": "Might",
                        "amount_mode": "Stacks",
                        "amount_value": 1.0,
                        "base_duration_ms": 8000,
                        "target_scope": "self",
                        "target_count": null,
                        "internal_cooldown_ms": 1000,
                        "source_duration_multiplier": 1.0
                    }
                }
            ]
        }"#;
        let file = load_effects_file(json).expect("should parse");
        assert_eq!(file.effects.len(), 1);

        let effect = &file.effects[0];
        assert_eq!(effect.effect_id, "trait_214_might_on_crit");
        assert_eq!(effect.source_type, SourceType::Trait);
        assert_eq!(effect.source_id, 214);
        assert_eq!(effect.category, EffectCategory::AppliesBoon);
        assert_eq!(effect.trigger_rule, TriggerRule::OnCrit);
        assert_eq!(effect.evidence_level, EvidenceLevel::Heuristic);
        assert_eq!(effect.max_stacks, Some(FactualValue::Resolved(25)));

        let op = effect
            .status_operation
            .as_ref()
            .expect("should have status_operation");
        assert_eq!(op.operation_type, OperationType::AppliesBoon);
        assert_eq!(op.target_side, TargetSide::Self_);
        assert_eq!(op.status_kind, "Might");
        assert_eq!(op.amount_mode, AmountMode::Stacks);
        assert_eq!(op.amount_value, FactualValue::Resolved(1.0));
        assert_eq!(op.base_duration_ms, Some(FactualValue::Resolved(8000)));
        assert_eq!(op.target_scope, TargetScope::Self_);
        assert_eq!(op.target_count, Some(FactualValue::Unknown));
        assert_eq!(op.internal_cooldown_ms, Some(FactualValue::Resolved(1000)));
    }

    // TargetSide/TargetScope "self" rename

    #[test]
    fn test_self_rename_in_json() {
        // TargetSide::Self_ serializes to "self" (Rust keyword workaround)
        let json = serde_json::to_string(&TargetSide::Self_).unwrap();
        assert_eq!(json, r#""self""#);
        let parsed: TargetSide = serde_json::from_str(r#""self""#).unwrap();
        assert_eq!(parsed, TargetSide::Self_);

        // TargetScope::Self_ serializes to "self"
        let json = serde_json::to_string(&TargetScope::Self_).unwrap();
        assert_eq!(json, r#""self""#);
        let parsed: TargetScope = serde_json::from_str(r#""self""#).unwrap();
        assert_eq!(parsed, TargetScope::Self_);
    }

    // Validation: valid effects pass

    #[test]
    fn test_validation_valid_effect_passes() {
        let effect = minimal_effect("valid_effect");
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(
            result.is_ok(),
            "valid effect should pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_validation_estimated_with_heuristic_passes() {
        let mut effect = minimal_effect("estimated_ok");
        effect.uptime_model = UptimeModel {
            kind: UptimeModelKind::Estimated,
            uptime: Some(FactualValue::Resolved(0.75)),
        };
        effect.evidence_level = EvidenceLevel::Heuristic; // Correct!
                                                          // Non-passive to avoid ICD conflict
        effect.trigger_rule = TriggerRule::OnCrit;
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(
            result.is_ok(),
            "Estimated + Heuristic should pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_validation_triggered_effect_with_inner_passes() {
        let mut effect = minimal_effect("triggered_ok");
        effect.category = EffectCategory::TriggeredEffect;
        effect.inner_category = Some(EffectCategory::FlatStat); // Present!
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(
            result.is_ok(),
            "TriggeredEffect with inner should pass: {:?}",
            result.err()
        );
    }

    // is_status_operation helper

    #[test]
    fn test_is_status_operation() {
        // These 7 categories are status operations
        assert!(EffectCategory::AppliesBoon.is_status_operation());
        assert!(EffectCategory::AppliesCondition.is_status_operation());
        assert!(EffectCategory::RemovesBoon.is_status_operation());
        assert!(EffectCategory::CorruptsBoon.is_status_operation());
        assert!(EffectCategory::RemovesCondition.is_status_operation());
        assert!(EffectCategory::ConvertsConditionToBoon.is_status_operation());
        assert!(EffectCategory::TransfersCondition.is_status_operation());

        // These are NOT status operations
        assert!(!EffectCategory::FlatStat.is_status_operation());
        assert!(!EffectCategory::StrikeDamagePct.is_status_operation());
        assert!(!EffectCategory::DefianceDamage.is_status_operation());
        assert!(!EffectCategory::ProcEffect.is_status_operation());
        assert!(!EffectCategory::TriggeredEffect.is_status_operation());
    }

    // Error path: empty patch_id and invalid mode

    #[test]
    fn test_empty_patch_id_rejected() {
        let json = r#"{
            "patch_id": "",
            "mode": "PvE",
            "effects": []
        }"#;
        let result = load_effects_file(json);
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
            "effects": []
        }"#;
        let result = load_effects_file(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid mode"));
    }

    // StatusOperation serde round-trip

    #[test]
    fn test_serde_roundtrip_status_operation() {
        let op = StatusOperation {
            operation_type: OperationType::CorruptsBoon,
            target_side: TargetSide::Enemy,
            status_kind: "Stability".to_string(),
            amount_mode: AmountMode::Stacks,
            amount_value: FactualValue::Resolved(2.0),
            base_duration_ms: None,
            target_scope: TargetScope::Area,
            target_count: Some(FactualValue::Resolved(5)),
            internal_cooldown_ms: Some(FactualValue::Resolved(3000)),
            source_duration_multiplier: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let parsed: StatusOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, parsed);
    }

    // UptimeModel serde round-trip

    #[test]
    fn test_serde_roundtrip_uptime_model_always_on() {
        let model = UptimeModel {
            kind: UptimeModelKind::AlwaysOn,
            uptime: None,
        };
        let json = serde_json::to_string(&model).unwrap();
        let parsed: UptimeModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model, parsed);
    }

    #[test]
    fn test_serde_roundtrip_uptime_model_estimated() {
        let model = UptimeModel {
            kind: UptimeModelKind::Estimated,
            uptime: Some(FactualValue::Resolved(0.85)),
        };
        let json = serde_json::to_string(&model).unwrap();
        let parsed: UptimeModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model, parsed);
    }

    // Deserialization from various source types

    #[test]
    fn test_all_source_types_in_json() {
        for (source_type_str, expected) in [
            ("Trait", SourceType::Trait),
            ("Skill", SourceType::Skill),
            ("Rune", SourceType::Rune),
            ("Sigil", SourceType::Sigil),
            ("Relic", SourceType::Relic),
        ] {
            let json = format!(
                r#"{{
                    "effect_id": "test_{source_type_str}",
                    "source_type": "{source_type_str}",
                    "source_id": 1,
                    "source_name": "Test",
                    "category": "FlatStat",
                    "value": 100.0,
                    "stacking_rule": "Additive",
                    "trigger_rule": "Passive",
                    "uptime_model": {{ "kind": "AlwaysOn" }},
                    "evidence_level": "Factual"
                }}"#
            );
            let effect: NormalizedEffect = serde_json::from_str(&json)
                .unwrap_or_else(|_| panic!("should parse {}", source_type_str));
            assert_eq!(effect.source_type, expected);
        }
    }

    // TriggeredEffect with inner_category round-trip

    #[test]
    fn test_triggered_effect_with_inner_category_roundtrip() {
        let mut effect = minimal_effect("triggered_roundtrip");
        effect.category = EffectCategory::TriggeredEffect;
        effect.inner_category = Some(EffectCategory::AppliesBoon);
        // Add status_operation since we claim inner is AppliesBoon but validation
        // only checks the outer category — TriggeredEffect doesn't require status_operation
        let json = serde_json::to_string(&effect).unwrap();
        let parsed: NormalizedEffect = serde_json::from_str(&json).unwrap();
        assert_eq!(effect, parsed);
        assert_eq!(parsed.inner_category, Some(EffectCategory::AppliesBoon));
    }

    // Non-passive trigger with ICD is valid

    #[test]
    fn test_on_crit_with_icd_is_valid() {
        let mut effect = minimal_effect("crit_icd");
        effect.trigger_rule = TriggerRule::OnCrit;
        effect.internal_cooldown = Some(FactualValue::Resolved(1.0));
        let file = NormalizedEffectsFile {
            patch_id: "2026-01-13".to_string(),
            mode: "PvE".to_string(),
            effects: vec![effect],
        };
        let result = validate_effects_file(&file);
        assert!(
            result.is_ok(),
            "OnCrit + ICD should be valid: {:?}",
            result.err()
        );
    }

    // FactualValue deserialization: null → Unknown

    #[test]
    fn test_value_null_deserializes_to_unknown() {
        let json = r#"{
            "effect_id": "test_unknown_value",
            "source_type": "Trait",
            "source_id": 1,
            "source_name": "Test",
            "category": "FlatStat",
            "value": null,
            "stacking_rule": "Additive",
            "trigger_rule": "Passive",
            "uptime_model": { "kind": "AlwaysOn" },
            "evidence_level": "Unknown"
        }"#;
        let effect: NormalizedEffect = serde_json::from_str(json).unwrap();
        assert_eq!(effect.value, FactualValue::Unknown);
    }

    // 3-state Option<FactualValue<T>> test

    #[test]
    fn test_three_state_option_factual_value() {
        // State 1: field absent → None
        let json = r#"{
            "effect_id": "test_absent",
            "source_type": "Trait",
            "source_id": 1,
            "source_name": "Test",
            "category": "FlatStat",
            "value": 100.0,
            "stacking_rule": "Additive",
            "trigger_rule": "Passive",
            "uptime_model": { "kind": "AlwaysOn" },
            "evidence_level": "Factual"
        }"#;
        let effect: NormalizedEffect = serde_json::from_str(json).unwrap();
        assert_eq!(effect.effect_duration, None, "absent field → None");
        assert_eq!(effect.max_stacks, None, "absent field → None");

        // State 2: field = null → Some(Unknown)
        let json = r#"{
            "effect_id": "test_null",
            "source_type": "Trait",
            "source_id": 1,
            "source_name": "Test",
            "category": "FlatStat",
            "value": 100.0,
            "stacking_rule": "Additive",
            "trigger_rule": "Passive",
            "uptime_model": { "kind": "AlwaysOn" },
            "evidence_level": "Factual",
            "effect_duration": null,
            "max_stacks": null
        }"#;
        let effect: NormalizedEffect = serde_json::from_str(json).unwrap();
        assert_eq!(
            effect.effect_duration,
            Some(FactualValue::Unknown),
            "null → Some(Unknown)"
        );
        assert_eq!(
            effect.max_stacks,
            Some(FactualValue::Unknown),
            "null → Some(Unknown)"
        );

        // State 3: field = value → Some(Resolved(v))
        let json = r#"{
            "effect_id": "test_resolved",
            "source_type": "Trait",
            "source_id": 1,
            "source_name": "Test",
            "category": "FlatStat",
            "value": 100.0,
            "stacking_rule": "Additive",
            "trigger_rule": "Passive",
            "uptime_model": { "kind": "AlwaysOn" },
            "evidence_level": "Factual",
            "effect_duration": 5.0,
            "max_stacks": 10
        }"#;
        let effect: NormalizedEffect = serde_json::from_str(json).unwrap();
        assert_eq!(
            effect.effect_duration,
            Some(FactualValue::Resolved(5.0)),
            "value → Some(Resolved)"
        );
        assert_eq!(
            effect.max_stacks,
            Some(FactualValue::Resolved(10)),
            "value → Some(Resolved)"
        );
    }

    // StatusOperation with FactualValue fields

    #[test]
    fn test_status_operation_amount_value_unknown() {
        let json = r#"{
            "operation_type": "AppliesBoon",
            "target_side": "self",
            "status_kind": "Might",
            "amount_mode": "Stacks",
            "amount_value": null,
            "target_scope": "self"
        }"#;
        let op: StatusOperation = serde_json::from_str(json).unwrap();
        assert_eq!(op.amount_value, FactualValue::Unknown);
    }

    // P3-10b: baseline data tests

    #[test]
    fn test_baseline_data_loads_and_validates() {
        // All three baseline files load and pass validation
        let data = effects();
        assert_eq!(data.file_count(), 3);

        // PvE should have the most entries
        let pve = data.effects_for("2026-01-13", "PvE").unwrap();
        assert!(
            pve.len() >= 20,
            "PvE should have at least 20 representative entries, got {}",
            pve.len(),
        );

        // All entries should have unique effect_ids (validation already ensures this,
        // but verify it held through deserialization)
        let mut ids: HashSet<&str> = HashSet::new();
        for effect in pve {
            assert!(
                ids.insert(&effect.effect_id),
                "duplicate effect_id in PvE baseline: {}",
                effect.effect_id,
            );
        }
    }

    #[test]
    fn test_mode_split_effect() {
        // Same source should have different values in PvE vs PvP
        let data = effects();
        let pve = data.effects_for("2026-01-13", "PvE").unwrap();
        let pvp = data.effects_for("2026-01-13", "PvP").unwrap();

        // Find Sigil of Force in PvE (5% strike damage)
        let pve_force = pve
            .iter()
            .find(|e| e.effect_id == "sigil:24615:0")
            .expect("Sigil of Force should be in PvE baseline");
        // Find Sigil of Force in PvP (different value)
        let pvp_force = pvp
            .iter()
            .find(|e| e.effect_id == "sigil:24615:0")
            .expect("Sigil of Force should be in PvP baseline");

        // PvE Sigil of Force: +5% strike damage
        assert_eq!(pve_force.value, FactualValue::Resolved(5.0));
        // PvP Sigil of Force: +3% (split balance)
        assert_eq!(pvp_force.value, FactualValue::Resolved(3.0));
        // Values should differ between modes
        assert_ne!(pve_force.value, pvp_force.value);
    }

    #[test]
    fn test_proc_vs_triggered_boundary() {
        // Verify ProcEffect has inner_category and correct trigger
        let data = effects();
        let pve = data.effects_for("2026-01-13", "PvE").unwrap();

        // Find a ProcEffect entry (Sigil of Fire)
        let proc_effect = pve
            .iter()
            .find(|e| e.category == EffectCategory::ProcEffect)
            .expect("should have at least one ProcEffect in PvE baseline");

        // ProcEffect should have inner_category
        assert!(
            proc_effect.inner_category.is_some(),
            "ProcEffect should have inner_category, effect: {}",
            proc_effect.effect_id,
        );
        // ProcEffect trigger should not be Passive
        assert_ne!(
            proc_effect.trigger_rule,
            TriggerRule::Passive,
            "ProcEffect should have non-passive trigger"
        );

        // Find a TriggeredEffect entry
        let triggered = pve
            .iter()
            .find(|e| e.category == EffectCategory::TriggeredEffect)
            .expect("should have at least one TriggeredEffect in PvE baseline");

        // TriggeredEffect must have inner_category (validation enforces this)
        assert!(
            triggered.inner_category.is_some(),
            "TriggeredEffect should have inner_category"
        );
        // TriggeredEffect typically uses Conditional or OnHealthThreshold
        assert!(
            matches!(
                triggered.trigger_rule,
                TriggerRule::Conditional | TriggerRule::OnHealthThreshold
            ),
            "TriggeredEffect should have Conditional or OnHealthThreshold trigger, got {:?}",
            triggered.trigger_rule,
        );
    }

    // P3-10b: category coverage in baseline

    #[test]
    fn test_baseline_category_coverage() {
        let data = effects();
        let pve = data.effects_for("2026-01-13", "PvE").unwrap();

        // Collect all categories present in PvE baseline
        let categories: HashSet<String> = pve.iter().map(|e| format!("{:?}", e.category)).collect();

        // Must cover at least these core categories
        let required = [
            "FlatStat",
            "StrikeDamagePct",
            "ConditionDamagePct",
            "AppliesBoon",
            "AppliesCondition",
            "ProcEffect",
            "TriggeredEffect",
        ];
        for cat in &required {
            assert!(
                categories.contains(*cat),
                "PvE baseline must cover category {}, found: {:?}",
                cat,
                categories,
            );
        }
    }

    // P3-10b: source type coverage

    #[test]
    fn test_baseline_source_type_coverage() {
        let data = effects();
        let pve = data.effects_for("2026-01-13", "PvE").unwrap();

        let source_types: HashSet<String> =
            pve.iter().map(|e| format!("{:?}", e.source_type)).collect();

        // Should have Trait, Rune, Sigil at minimum
        for st in &["Trait", "Rune", "Sigil"] {
            assert!(
                source_types.contains(*st),
                "PvE baseline must include source type {}, found: {:?}",
                st,
                source_types,
            );
        }
    }
}
