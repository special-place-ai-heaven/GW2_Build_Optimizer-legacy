//! Counterplay-aware WvW combat timeline.
//!
//! The legacy rotation simulator answers "what is the average damage of this
//! skill roster against a dummy?"  This module answers the WvW question: "can
//! the build establish control of a real exchange long enough to finish its
//! chain, survive the answer, recover, and do it again?"

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::data::fight_population::FightPopulation;
use crate::data::normalized_effects::{
    Actor, Gate, Prerequisite, Rearm, Scale, ScaleBy, StackingRule, WeaponHand,
};
use crate::data::normalized_effects::{
    EffectCategory, NormalizedEffect, OperationType, SourceType, TargetSide, TriggerRule,
};
use crate::data::quality::{CoverageEntry, FactualValue};
use crate::scenario::{CombatKind, CombatTier, ScenarioSpec};

use super::attunement::{apply_attunement_skill, AttunementState, Element};
use super::combat_model::{corrupt_into, EnemyDummy, TargetState, TimedFoeCondition};
use super::illusion::{spawn_clone, IllusionState};
use super::simulator::{
    alacrity_cd_advance_ms, condition_tick_damage, crit_chance_fraction, reference_armor, SimParams,
};
use super::skill_timings::{HUMAN_DELAY_MS, MIN_SKILL_GAP_MS};
use super::trait_skill::{
    catalog_from_skills, resolve_trait_skill, skill_effects_from_status_operation,
};
use super::trigger_bus::{
    land_foe_disable, BusEvent, DodgeAction, EndurancePool, TriggerBus, DODGE_COST,
};
use super::{CoverKind, MobilityKind, RotationSkill, SkillEffect};

const TIMELINE_TICK_MS: u32 = 50;

/// One bar of Warrior adrenaline, in strikes (wiki `Adrenaline`).
const ADRENALINE_BAR_STRIKES: f64 = 10.0;
/// Revenant energy regeneration, percent per second (wiki `Energy`).
const ENERGY_REGEN_PER_SECOND: f64 = 5.0;
/// Total upkeep the game allows at once (wiki `Energy`: -10, net -5/s).
const MAX_UPKEEP: f64 = 10.0;
/// Energy a legend invocation resets the pool to (wiki `Energy`).
const LEGEND_SWAP_ENERGY: f64 = 50.0;
/// Recharge on invoking a legend (wiki `Legend`: always 10 seconds).
const LEGEND_SWAP_RECHARGE_MS: u32 = 10_000;
/// GW2 dodge evade frame (~750 ms). No prior evade-cover duration in this file.
const DODGE_EVADE_MS: u32 = 750;
pub const MIN_PROTECTED_WINDOW_MS: u32 = 2_000;

#[cfg(test)]
thread_local! {
    static TRACE_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// How often the enemy can open on you again.
///
/// The event script below is a burst: control, strike, condition, boon strip,
/// control, a bigger strike, an unblockable finisher, over 4.4 s. It used to
/// repeat every 5 s, which left 600 ms between the finisher and the next
/// opener - a metronome, not a fight. Pressure in WvW is never sustained. It
/// oscillates: someone spends their cooldowns on you, and then they do not
/// have them for a while.
///
/// That is not a cosmetic difference, it decides what the sustain gates can
/// measure. Under a metronome the only thing that survives is raw mitigation
/// per second, because a heal on a 25 s cooldown can never catch up with a
/// drip - so the test became an accumulation race that longer windows always
/// won. Support and CondiRamp get a 20 s window against StrikeSpike's 5 s, so
/// they ate four uninterrupted bursts and died: measured on the synced corpus
/// 2026-09-07, `SustainRecovery` passed 100% of StrikeSpike, 93% of Harasser
/// (10 s), 50% of CondiRamp and 14% of Support - monotonic in window length,
/// which means the gate was measuring the clock.
///
/// With a real lull the same window measures the thing a support build is
/// actually for: eat the spike, recover before the next one.
const BURST_PERIOD_MS: u32 = 10_000;

/// The burst's shape, as multiples of the base strike: damage ramps up,
/// reaches a peak, and whatever is not evaded or blocked at the peak has to be
/// healed before the next one.
///
/// These four sum to 2.90, which is exactly what the flat 1.00/1.20/0.70
/// script totalled, so this redistributes the burst rather than sharpening it.
/// The peak is now 4.6x the opening chip instead of 1.2x. That ratio is the
/// point: `receive_strike` drops a blockable hit entirely on evade, block or
/// invulnerability, so a peak barely above the chip made active defence worth
/// almost nothing and left raw mitigation per second as the only thing the
/// sustain gates could see.
/// The peak magnitude is the one free number here, and it is calibrated, not
/// chosen. Swept against the synced WvW corpus 2026-09-07, `SustainRecovery`
/// pass rate per combat kind:
///
/// | peak | Support | StrikeSpike | Harasser | spread |
/// |---|---|---|---|---|
/// | flat 1.20 (old) | 14% | 100% | 93% | 86 pts |
/// | 1.60 | 100% | 100% | 100% | 0, but nothing fails |
/// | **2.60** | **93%** | **91%** | **99%** | **8 pts** |
/// | 4.00 | 57% | 70% | 96% | 39 pts |
/// | 5.50 | 7% | 43% | 82% | 75 pts |
///
/// The old script's spread was the window length, not the build. 2.60 is
/// where that bias disappears while the gate still refuses real builds - five
/// of the corpus - so it is measuring sustain rather than the clock or
/// nothing at all. Too high and the bias returns inverted, because a support
/// carries less active defence than a roamer and starts eating peaks.
const RAMP_OPEN: f64 = 0.35;
const RAMP_BUILD: f64 = 0.55;
const PEAK: f64 = 2.60;
const RESIDUAL: f64 = 0.40;

/// How long an applied condition sits on you.
///
/// This was 4,000 ms against a burst period that is now 10,000 ms, which
/// meant every condition expired during the lull. Nothing had to be cleansed:
/// waiting was a complete answer, so `CleanseRate` was a rule about the skill
/// bar with no consequence anywhere in the simulation, and a build that
/// brought no cleanse at all measured the same as one built around it.
///
/// Condition damage is not strike damage. Armour does not reduce it,
/// Protection does not reduce it, and an evade or a block cannot avoid what
/// is already ticking - the tick is `(base + coefficient x Condition Damage)`
/// per stack per second, for as long as the duration the attacker's Expertise
/// bought. Removal is the only counter. So the duration has to reach the next
/// burst: uncleansed stacks then overlap the new ones and the pressure
/// compounds, which is the thing that makes cleansing worth a utility slot.
const CONDITION_DURATION_MS: u32 = 10_000;

/// Chilled: skills recharge at 34% of the normal rate.
///
/// The enemy's answer to Alacrity, and the reason a condition that deals no
/// damage at all can still be the one that kills you.
// Wiki `Chilled` (read 2026-09-07): "for every 1.66 seconds chilled, only 1
// second of cooldown will have expired" — a 60% recharge rate. The tooltip's
// "cooldown increased by 66%" is the same fact from the other side; it is
// not a 34% rate.
const CHILLED_RECHARGE_PERCENT: u32 = 60;

/// How long the opener's Chill sits on you.
///
/// Shorter than [`CONDITION_DURATION_MS`]: a damaging condition ticks for as
/// long as it lasts, but Chill only has to cover the recovery window to do its
/// job, and a chill that outlasted the lull would mean the heal never comes
/// back at all rather than comes back late.
const CHILL_DURATION_MS: u32 = 4_000;
pub const TARGET_PROTECTED_WINDOW_MS: u32 = 5_000;

/// Wiki Barrier: disappears 5s after applied; WvW cap is 25% of max health.
const BARRIER_LIFETIME_MS: u32 = 5_000;
const WVW_BARRIER_HEALTH_FRACTION: f64 = 0.25;
/// Wiki Interrupt: interrupted skills get a 5 second cooldown.
// Wiki `Activation time` (edited 2026-07-30) and `Channeled skill` say an
// interrupted activation puts the skill on a 4-second recharge; `Skill` and
// `Interrupt` (edited 2026-02-19) say 5. The wiki disagrees with itself; the
// newest page wins until someone measures it.
const INTERRUPT_COOLDOWN_MS: u32 = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ResourceKind {
    #[default]
    Initiative,
    Energy,
    Adrenaline,
    Illusions,
    Blades,
    /// Necromancer life force, in absolute units; the cap is 69 % of max
    /// health (`data/formulas/shroud.json`).
    LifeForce,
    /// Bladesworn flow. Wiki `Flow`: gained at a constant rate while in
    /// combat (2/s base), never from attacking, maximum 100, and it cannot
    /// fuel a core Warrior burst.
    Flow,
}

#[derive(Debug, Clone, Default)]
pub struct SkillResourceRule {
    pub skill_id: u32,
    pub kind: ResourceKind,
    pub cost: f64,
    pub gain_on_hit: f64,
    pub spend_all: bool,
    // Sprint 2 (specs/005-wvw-proc-sites): life force and shroud
    /// Resource credited when the cast resolves (Percent fact "Life Force").
    pub gain_on_use: f64,
    /// Minimum pool to start the cast (shroud entry: 10 % of the cap).
    pub entry_floor: f64,
    /// Pool lost per second while this skill's shroud is active.
    pub drain_per_second: f64,
    /// Incoming damage taken by the pool while in this shroud, as a
    /// fraction after reduction (WvW: 0.5).
    pub shroud_damage_factor: f64,
    /// The health pool stays exposed in this shroud and healing lands
    /// (Harbinger Shroud). `false` is the Death / Reaper's / Ritualist's
    /// shape where the pool takes the hit and nothing heals.
    pub shroud_health_exposed: bool,
    /// This skill enters / exits shroud.
    pub enters_shroud: bool,
    pub exits_shroud: bool,
    /// Pool ceiling when the build changes it (Thief Preparedness: 15
    /// initiative instead of 12). `0.0` keeps the profession default.
    pub pool_cap: f64,
    /// Pool credited every second regardless of what the player does
    /// (Bladesworn flow). `0.0` for pools that only fill from actions.
    pub pool_regen_per_second: f64,
    /// Energy regeneration this skill removes while it is maintained
    /// (Revenant upkeep, wiki `Energy`: a negative modifier on the +5/s
    /// rate, capped at -10 upkeep, i.e. -5/s net).
    pub upkeep: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WvwCombatReport {
    pub duration_ms: u32,
    pub target_health: Option<f64>,
    pub target_reached_at_ms: Option<u32>,
    pub longest_protected_window_ms: u32,
    pub protected_action_count: u32,
    pub successful_action_count: u32,
    pub interrupted_casts: u32,
    pub protected_damage: f64,
    pub peak_protected_damage_2s: f64,
    pub peak_protected_damage_5s: f64,
    pub total_damage: f64,
    pub control_landed_ms: u32,
    pub incoming_damage: f64,
    pub avoided_damage: f64,
    pub healing: f64,
    pub barrier_absorbed: f64,
    pub conditions_cleansed: u32,
    pub combo_activations: u32,
    pub remaining_health_ratio: f64,
    /// Positive means recovery/avoidance exceeded incoming pressure.
    pub sustain_margin: f64,
    pub player_survived: bool,
    pub target_reached: bool,
    pub chain_completed: bool,
    /// Pressure and control that occurred inside the qualifying secured
    /// sequence, not totals collected from unrelated moments.
    pub secured_sequence_damage: f64,
    pub secured_sequence_control_ms: u32,
    pub repeatable: bool,
    pub resource_blocked_actions: u32,
    pub resource_legal: bool,
    /// Priority decisions whose top pick could not be paid for, over all
    /// priority decisions. Opening a fight unable to pay is how the game
    /// starts (a Mesmer has no clones, a Warrior has no adrenaline); only
    /// sustained blocking says the bar cannot be played.
    pub resource_blocked_ratio: f64,
    /// Skills whose cost can never be paid because it exceeds the resource
    /// cap. Unplayable for the whole fight, not just its opening.
    pub resource_unpayable_skills: Vec<String>,
    /// False when the active profession mechanic needs a state model that this
    /// bounded resource ledger does not yet provide.
    pub resource_model_complete: bool,
    /// What the ledger does not model for this build, named: an unmodelled
    /// profession or elite mechanic, or a skill that spends a resource no
    /// rule prices. A refusal quotes these instead of passing silently.
    pub resource_model_gaps: Vec<String>,
    /// False when this profession's resource was never simulated at all --
    /// no rule priced anything, so there is nothing to judge and the
    /// legality gate abstains rather than passing the build for free.
    pub resource_simulated: bool,
    /// Whose resource this is, for the abstention note.
    pub profession: String,
    /// Every equipped or triggered effect source the timeline did not
    /// simulate, as `"{name} ({why})"` — `no record`, `on-crit`,
    /// `on-skill-use`, `on-health-threshold`, `conditional`, `unresolved
    /// value`, `unsupported proc`, `partial combo`, `dark field`. Deduplicated.
    /// This never silently becomes verified data: the referee turns it into
    /// the `wvw_timeline.effects` coverage reason.
    pub unmodeled_sources: Vec<String>,
    /// The same list with its classes (specs/007-trait-triggers, US4): one
    /// entry per skipped source; empty iff the coverage reason is absent.
    pub coverage: Vec<CoverageEntry>,
    /// Trait records that fired at least once, by source name (US1).
    pub trait_fire_counts: BTreeMap<String, u32>,
    // Fight population (FR-003a): counted onto extra foes and allies.
    /// Strike damage credited to secondary foes; already in `total_damage`
    /// and, when protected, in `protected_damage`.
    pub cleave_damage: f64,
    /// Condition stack-seconds credited to secondary foes.
    pub cleave_condition_stack_seconds: f64,
    /// Boon stack-seconds credited to allies (0 in every Solo fight).
    pub ally_boon_stack_seconds: f64,
    /// Healing credited to allies.
    pub ally_healing: f64,
    /// Conditions cleansed from allies (0 in every Solo fight).
    pub ally_cleanses: u32,
    /// Bounded event trace. Empty unless [`WvwTimelineInput::trace`] was set;
    /// capped at [`TRACE_CAP`] events.
    pub trace: Vec<TraceEvent>,
    /// True when an event past the cap was dropped.
    pub trace_truncated: bool,
    /// Seeded on-crit trials, one entry per proc source. Empty unless
    /// [`WvwTimelineInput::trace`] was set.
    pub proc_trials: Vec<ProcTrial>,
    /// Readable reasons a shroud entry was refused, e.g. `Reaper's Shroud
    /// needs 10% life force, had 4%`. The referee appends them to the
    /// quality reasons.
    pub shroud_refusals: Vec<String>,
    // NeedsMechanic Engine E0
    /// Dodges performed (EndurancePool + DodgeAction).
    pub dodge_count: u32,
    /// Bus OnDodge emissions this fight.
    pub bus_on_dodge: u32,
    /// Bus OnDisableFoe emissions this fight (landed foe disables).
    pub bus_on_disable_foe: u32,
    /// Bus OnAttunementSwap emissions this fight.
    pub bus_on_attunement_swap: u32,
    /// Bus OnCloneCreated emissions this fight.
    pub bus_on_clone_created: u32,
}

/// Upper bound on [`WvwCombatReport::trace`]. The 513th event is dropped and
/// `trace_truncated` is set.
pub const TRACE_CAP: usize = 512;

/// One observable runtime event, for the causal experiments in
/// `specs/004-simulator-trust`. Never serialized into a prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    pub t_ms: u32,
    pub kind: TraceKind,
    /// The skill, trait, sigil or relic name the event belongs to.
    pub source: String,
    /// Kind-specific detail: landed damage, proc category, hits lost, set.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceKind {
    HitLanded,
    ProcFired,
    ProcSkippedIcd,
    ProcUnmodeled,
    CastInterrupted,
    WeaponSwap,
    // Sprint 2 (specs/005-wvw-proc-sites)
    ConditionalActivated,
    ConditionalExpired,
    StackGained,
    ComboResolved,
    ShroudEntered,
    ShroudExited,
    ShroudRefused,
    LifeForceGained,
    // Sprint 3 (specs/007-trait-triggers)
    /// A trait record fired: `{category} ×{weight}` plus `at entry`,
    /// `at exit ({why})` or `periodic`.
    TraitFired,
    /// A record's prerequisite did not hold at its trigger.
    ProcSkippedPrerequisite,
    /// An effect was counted onto extra foes or allies.
    PopulationApplied,
    /// An in-shroud conditional bonus turned on / off.
    ShroudBonusActive,
    ShroudBonusEnded,
    // NeedsMechanic Engine E0
    /// EndurancePool + DodgeAction spent a dodge.
    Dodged,
}

/// Seeded-trial summary for one proc source, trace mode only
/// (`specs/005-wvw-proc-sites`, R1): how often the proc fired across the
/// fixed seeds, beside the expected-value count the ranking uses.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProcTrial {
    pub source: String,
    pub mean: f64,
    pub min: u32,
    pub max: u32,
}

pub struct WvwTimelineInput<'a> {
    pub skills: &'a [RotationSkill],
    /// The published rotation, as skill ids in press order. Followed while
    /// it can be; the timeline improvises once it runs out or cannot comply.
    pub opener: &'a [u32],
    pub duration_ms: u32,
    pub params: &'a SimParams,
    pub enemy: EnemyDummy,
    pub scenario: &'a ScenarioSpec,
    pub active_effects: &'a [&'a NormalizedEffect],
    pub resource_rules: &'a [SkillResourceRule],
    pub resource_model_complete: bool,
    /// Mechanics this build spends that the ledger does not price.
    pub resource_model_gaps: Vec<String>,
    /// Whose bar this is, so an abstaining gate can name it.
    pub profession: String,
    /// Equipped sources the timeline will not simulate, already classified
    /// by the caller (`engine::active_normalized_effects`).
    pub coverage: Vec<CoverageEntry>,
    /// Each socketed sigil's weapon set (1 or 2; 0 = on both), so a sigil's
    /// procs fire only while its set is held (CONN-00-07).
    pub sigil_sets: HashMap<u32, u8>,
    /// Foes and allies for the scenario's scale (FR-003a): target-facing
    /// effects are counted onto them within each record's cap.
    pub population: FightPopulation,
    /// Exact in-combat weapon swap cooldown for this profession. `None`
    /// means the active specialization cannot swap weapons in combat.
    pub weapon_swap_cooldown_ms: Option<u32>,
    /// What the build wears in each hand of each set, so a record's
    /// `Gate::Weapon` can read the held kit (Destructive Impulses).
    pub equipped_weapons: Vec<EquippedWeapon>,
    /// Record a bounded [`TraceEvent`] log on the report. Diagnostics only:
    /// no production caller sets this — `engine::simulate_prepared` passes
    /// `false`, the search and the Choya tools go through it, and nothing
    /// appends the trace to a prompt (`trace_is_empty_unless_requested`).
    pub trace: bool,
}

#[derive(Debug, Clone)]
enum EnemyEventKind {
    Strike {
        damage: f64,
        unblockable: bool,
    },
    Control {
        duration_ms: u32,
        unblockable: bool,
    },
    Condition {
        condition: String,
        stacks: u32,
        duration_ms: u32,
    },
    BoonStrip {
        count: u32,
    },
}

#[derive(Debug, Clone)]
struct EnemyEvent {
    at_ms: u32,
    kind: EnemyEventKind,
}

#[derive(Debug, Clone)]
struct WvwProfile {
    duration_ms: u32,
    target_health: Option<f64>,
    enemy_events: VecDeque<EnemyEvent>,
    required_window_ms: u32,
    desired_window_ms: u32,
}

impl WvwProfile {
    fn for_scenario(
        scenario: &ScenarioSpec,
        enemy: &EnemyDummy,
        params: &SimParams,
        duration_ms: u32,
    ) -> Self {
        let duration_ms = duration_ms.max(MIN_PROTECTED_WINDOW_MS);
        let tier_pressure = match scenario.combat_tier {
            CombatTier::Solo => 1.0,
            CombatTier::Party => 1.20,
            CombatTier::Squad => 1.45,
        };
        let kind_pressure = match scenario.combat_kind {
            CombatKind::StrikeSpike | CombatKind::Harasser | CombatKind::Disabler => 1.10,
            CombatKind::CondiRamp => 1.0,
            CombatKind::Support | CombatKind::Commander | CombatKind::Staller => 0.90,
        };
        let pressure = tier_pressure * kind_pressure;
        let enemy_power = 2_500.0 * pressure;
        let strike = 1_100.0 * enemy_power / params.armor.max(1_000.0) * 2.0;
        let mut events = Vec::new();
        let mut cycle = 0;
        while cycle < duration_ms {
            events.push(EnemyEvent {
                at_ms: cycle + 450,
                kind: EnemyEventKind::Control {
                    duration_ms: 900,
                    unblockable: false,
                },
            });
            // The ramp. Chip damage while they build to the thing that
            // actually kills you - small enough that a real build's healing
            // covers it, which is what makes healing worth having.
            events.push(EnemyEvent {
                at_ms: cycle + 850,
                kind: EnemyEventKind::Strike {
                    damage: strike * RAMP_OPEN,
                    unblockable: false,
                },
            });
            events.push(EnemyEvent {
                at_ms: cycle + 1_650,
                kind: EnemyEventKind::Condition {
                    condition: if scenario.combat_kind == CombatKind::CondiRamp {
                        "Burning".into()
                    } else {
                        "Bleeding".into()
                    },
                    stacks: if scenario.combat_kind == CombatKind::CondiRamp {
                        3
                    } else {
                        2
                    },
                    duration_ms: CONDITION_DURATION_MS,
                },
            });
            events.push(EnemyEvent {
                at_ms: cycle + 2_350,
                kind: EnemyEventKind::Strike {
                    damage: strike * RAMP_BUILD,
                    unblockable: false,
                },
            });
            // Strip the cover, then land the CC, then hit. That order is the
            // whole of a WvW opener and it is why the peak is worth spending
            // an evade on.
            events.push(EnemyEvent {
                at_ms: cycle + 2_600,
                kind: EnemyEventKind::BoonStrip { count: 1 },
            });
            // Chill goes on before the peak, so the recovery window after it
            // is spent waiting rather than healing. It deals no damage; it
            // costs you the answer to the damage.
            events.push(EnemyEvent {
                at_ms: cycle + 2_800,
                kind: EnemyEventKind::Condition {
                    condition: "Chilled".into(),
                    stacks: 1,
                    duration_ms: CHILL_DURATION_MS,
                },
            });
            events.push(EnemyEvent {
                at_ms: cycle + 3_050,
                kind: EnemyEventKind::Control {
                    duration_ms: 1_100,
                    unblockable: false,
                },
            });
            // The peak. Blockable on purpose: this is the hit evades, blocks
            // and invulnerability exist for, and a model where the biggest
            // number of the fight cannot be answered cannot tell a build that
            // brought an answer from one that did not.
            events.push(EnemyEvent {
                at_ms: cycle + 3_550,
                kind: EnemyEventKind::Strike {
                    damage: strike * PEAK,
                    unblockable: false,
                },
            });
            // What is left after the peak, unblockable, so surviving is never
            // purely a matter of holding one button at the right moment.
            events.push(EnemyEvent {
                at_ms: cycle + 4_400,
                kind: EnemyEventKind::Strike {
                    damage: strike * RESIDUAL,
                    unblockable: true,
                },
            });
            cycle += BURST_PERIOD_MS;
        }
        events.retain(|event| event.at_ms < duration_ms);
        events.sort_by_key(|event| event.at_ms);

        Self {
            duration_ms,
            target_health: enemy.hp,
            enemy_events: events.into(),
            required_window_ms: MIN_PROTECTED_WINDOW_MS,
            desired_window_ms: TARGET_PROTECTED_WINDOW_MS.min(duration_ms),
        }
    }
}

#[derive(Debug, Clone)]
struct TimedDefense {
    kind: CoverKind,
    expires_at_ms: u32,
    stacks: u32,
    strippable: bool,
    /// First application. Extensions do not touch it — strips go by this.
    applied_at_ms: u32,
}

/// One hit of a cast in flight, landing at `at_ms` with its share of the
/// skill's damage. Cancelled with the cast if the cast is interrupted.
#[derive(Debug, Clone)]
struct ScheduledHit {
    at_ms: u32,
    skill_id: u32,
    dmg_multiplier: f64,
}

#[derive(Debug, Clone)]
struct TimedBuff {
    name: String,
    stacks: u32,
    expires_at_ms: u32,
}

/// Player-incoming conditions share the foe ledger shape (Phase 3).
type TimedCondition = TimedFoeCondition;

struct BarrierLayer {
    amount: f64,
    expires_at_ms: u32,
}

#[derive(Debug, Clone)]
struct PendingCast {
    skill_idx: usize,
    started_at_ms: u32,
    resolves_at_ms: u32,
    protected_at_start: bool,
    saved_by_charge: bool,
}

#[derive(Debug, Clone)]
struct DamageEvent {
    at_ms: u32,
    amount: f64,
    protected: bool,
}

#[derive(Debug, Clone)]
struct ProtectedActionEvent {
    at_ms: u32,
    skill_id: u32,
    control_ms: u32,
    applies_condition: bool,
    /// Healed, barriered, cleansed, or handed out a boon.
    ///
    /// A protected window is worth having because something happened inside
    /// it. For a damage build that is damage; for a healer it is the healing
    /// and the cleansing, which are not lesser outcomes — they are the job.
    supports_allies: bool,
}

#[derive(Debug, Clone, Default)]
struct SecuredSequenceSummary {
    completed: bool,
    damage: f64,
    control_ms: u32,
    skill_ids: HashSet<u32>,
}

#[derive(Debug, Clone)]
struct ProcSpec {
    source_type: SourceType,
    source_id: u32,
    source_name: String,
    trigger: TriggerRule,
    category: EffectCategory,
    value: f64,
    duration_ms: u32,
    internal_cooldown_ms: u32,
    next_ready_ms: u32,
    operation: Option<crate::data::normalized_effects::StatusOperation>,
    // Sprint 2 (specs/005-wvw-proc-sites)
    /// 0 for traits, runes, relics and skills; 1 or 2 for a sigil's seat.
    /// A sigil fires only while its set is held.
    weapon_set: u8,
    /// Record proc chance, 1.0 when absent.
    proc_chance: f64,
    /// Expected-value probability mass accumulated since the last cooldown
    /// start (R1): the cooldown begins when it reaches 1.0.
    mass: f64,
    scope: crate::data::normalized_effects::TriggerScope,
    // Sprint 3 (specs/007-trait-triggers)
    /// What must hold on the foe or the player at the trigger.
    prerequisite: Option<Prerequisite>,
    /// `GainsLifeForce` / `Heal`: multiplier applied at firing time.
    scale_by: Option<ScaleBy>,
    /// `Heal`: added to `value` as coefficient x healing power.
    healing_power_coefficient: f64,
    /// E2: lesser skill to cast when this proc fires (cast scheduler).
    cast_skill_id: Option<u32>,
    /// Max stacks for timed stacking damage buffs (Compounding Power).
    max_stacks: u32,
    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
    /// Every gate must hold at the trigger; a gate that carries state
    /// (interval, health re-arm) keeps it here.
    gates: Vec<GateState>,
    /// Live state added to `value` when the record fires.
    scale: Option<Scale>,
    /// How a stack gained from this record treats the stacks already held.
    stacking_rule: StackingRule,
}

/// One of a record's gates plus the state it needs across firings.
#[derive(Debug, Clone)]
struct GateState {
    gate: Gate,
    /// `Gate::Interval`: the next moment the gate opens.
    next_ms: u32,
    /// `Gate::HealthThreshold`: the gate has fired and is waiting to re-arm.
    latched: bool,
}

/// A rune or relic strike bonus that holds only while its prerequisite does
/// (specs/005-wvw-proc-sites, US3): a health threshold read against the
/// regular health pool, or a stack count fed by qualifying hits.
struct ConditionalSpec {
    source_name: String,
    kind: ConditionalKind,
    /// Percent per activation or per stack (record `value`).
    percent: f64,
    /// `true`: the percent is critical damage, not strike damage (Death
    /// Perception's in-shroud half). Sprint 3.
    crit_damage: bool,
    /// `true`: the percent is critical chance (Decimate Defenses). Sprint 3.
    crit_chance: bool,
    /// `true`: the percent is outgoing condition damage (Compounding Power). E4.
    condition_damage: bool,
    /// Threshold state as of the last evaluation.
    active: bool,
    stacks: u32,
    expires_at_ms: u32,
    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
    /// One expiry per held stack. `StackingRule::RefreshAllStacks` resets
    /// every entry when a stack is gained; any other rule lets each stack
    /// run out on its own clock (wiki `Effect stacking`, intensity).
    stack_expiries: Vec<u32>,
    stacking_rule: StackingRule,
}

enum ConditionalKind {
    Threshold {
        above: bool,
        percent: f64,
    },
    Stacking {
        max: u32,
        duration_ms: u32,
        scope: crate::data::normalized_effects::TriggerScope,
        /// When true, landed hits feed stacks via gain_conditional_stacks.
        /// Compounding Power (723) is false: stacks only on OnCloneCreated.
        hit_fed: bool,
    },
    /// Holds while the player is in shroud (Sprint 3, US1).
    InShroud,
    /// Holds while the record's prerequisite does (foe condition, foe
    /// health), re-read at every strike (Sprint 3, US3: Cold Shoulder,
    /// Close to Death).
    Prerequisite(Prerequisite),
    /// A timed bonus a proc switched on (Sprint 3, US3: Soul Barbs, Dread);
    /// holds until `until_ms`, refreshed by the next firing.
    Timed {
        until_ms: u32,
    },
    /// Scales with the foe's stacks of `condition`, capped at `max`
    /// (Sprint 3, US3: Decimate Defenses).
    PerFoeStack {
        condition: String,
        max: u32,
    },
}

/// The Necromancer in shroud (`specs/005-wvw-proc-sites`, US6): the weapon
/// bar is stowed for the shroud bar, life force drains, incoming damage goes
/// to the pool at the mode's reduction, healing does nothing, and the shroud
/// ends at zero or on the exit skill (wiki `Death Shroud`, read 2026-09-08).
struct ShroudState {
    entry_skill_id: u32,
    drain_per_second: f64,
    /// Share of incoming damage the pool takes (WvW: 0.5).
    damage_factor: f64,
    /// Damage hits health and healing lands (Harbinger Shroud).
    health_exposed: bool,
    /// Recharge the entry skill gets when the shroud ends.
    exit_recharge_ms: u32,
}

/// Unequipped weapon strength (wiki `Weapon strength`, read 2026-09-08): the
/// value sigil flame blasts and similar procs use instead of the held weapon.
const UNEQUIPPED_WEAPON_STRENGTH: f64 = 690.5;

/// How an on-crit proc's chance enters the result (R1): expected value in
/// ranking, Bernoulli draws from a fixed seed in the diagnostic trials.
enum CritMode {
    Expected,
    Seeded(XorShift64Star),
}

/// Ten-line PRNG for the trace-only trials; no dependency.
struct XorShift64Star(u64);

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Fixed trial seeds: reproducible, and eight is enough to bracket the
/// expected value on a 30 s opener.
const TRIAL_SEEDS: [u64; 8] = [
    0x9E37_79B9_7F4A_7C15,
    0x9E37_79B9_7F4A_7C16,
    0x9E37_79B9_7F4A_7C17,
    0x9E37_79B9_7F4A_7C18,
    0x9E37_79B9_7F4A_7C19,
    0x9E37_79B9_7F4A_7C1A,
    0x9E37_79B9_7F4A_7C1B,
    0x9E37_79B9_7F4A_7C1C,
];

struct Timeline<'a> {
    skills: &'a [RotationSkill],
    params: &'a SimParams,
    profile: WvwProfile,
    now_ms: u32,
    next_action_ms: u32,
    disabled_until_ms: u32,
    active_weapon_set: u8,
    weapon_swap_ready_ms: u32,
    weapon_swap_cooldown_ms: Option<u32>,
    opener: &'a [u32],
    opener_cursor: usize,
    cooldown_ready_ms: Vec<u32>,
    /// Auto-attack chain cursor (E11).
    auto_chain: super::AutoChain,
    pending: Option<PendingCast>,
    scheduled_hits: Vec<ScheduledHit>,
    defenses: Vec<TimedDefense>,
    buffs: Vec<TimedBuff>,
    /// Shared live foe ledger (Phase 3 TargetState).
    target: TargetState,
    incoming_conditions: Vec<TimedCondition>,
    /// Phase 4 shared ComboEngine (replaces Option<ComboFieldState>).
    combo: super::combo::ComboEngine,
    target_reached_at_ms: Option<u32>,
    player_health: f64,
    barrier: VecDeque<BarrierLayer>,
    protected_run_ms: u32,
    longest_protected_window_ms: u32,
    charge_cover_consumed_this_tick: bool,
    secured_tick_times: Vec<u32>,
    protected_actions: Vec<ProtectedActionEvent>,
    damage_events: Vec<DamageEvent>,
    protected_action_count: u32,
    successful_action_count: u32,
    interrupted_casts: u32,
    control_landed_ms: u32,
    incoming_damage: f64,
    avoided_damage: f64,
    healing: f64,
    barrier_absorbed: f64,
    conditions_cleansed: u32,
    combo_activations: u32,
    proc_specs: Vec<ProcSpec>,
    conditional_specs: Vec<ConditionalSpec>,
    unmodeled_proc_keys: HashSet<(u8, u32)>,
    /// Every source the timeline itself could not simulate, `"{name} ({why})"`,
    /// deduplicated. Reported first: these are the mechanics a player would
    /// expect to see executed.
    unmodeled_names: Vec<String>,
    /// Equipped sources with no record for this mode, named by the caller.
    /// Reported after `unmodeled_names`: most traits and every weapon skill
    /// have no proc record, so this list is long and least specific
    /// (CONN-01-06).
    /// Sources the caller classified before the run (US4).
    no_record_entries: Vec<CoverageEntry>,
    trace_enabled: bool,
    trace: Vec<TraceEvent>,
    trace_truncated: bool,
    shroud_refusals: Vec<String>,
    crit_mode: CritMode,
    /// `ProcFired` count per proc source, for the trials (trace only).
    proc_fire_counts: HashMap<String, u32>,
    // Sprint 3 (specs/007-trait-triggers)
    trait_fire_counts: BTreeMap<String, u32>,
    /// `why` of the shroud exit in progress, for the `TraitFired` detail.
    shroud_exit_why: Option<String>,
    /// The boon or condition name of the status trigger in progress, for
    /// `TriggerScope::Status` (US2).
    trigger_status: Option<String>,
    /// Depth of status-triggered proc evaluation, so a record that applies
    /// a boon on boon-applied cannot recurse.
    status_trigger_depth: u8,
    /// Records whose prerequisite refused at least once (end-of-fight
    /// `prerequisite never met` summary).
    prerequisite_refused: HashSet<String>,
    /// Last refusal reason traced per record, so the trace carries changes.
    last_prerequisite_skip: HashMap<String, String>,
    /// [`TRACE_CAP`] in production; a diagnostic test may widen it.
    trace_cap: usize,
    /// Any `Periodic` record loaded: the tick calls the site only then.
    has_periodic: bool,
    /// Fight population (FR-003a); Solo unless the caller says otherwise.
    population: FightPopulation,
    cleave_damage: f64,
    cleave_condition_stack_seconds: f64,
    ally_boon_stack_seconds: f64,
    ally_healing: f64,
    ally_cleanses: u32,
    /// Any shroud entry happened this fight (shroud records that never fired
    /// otherwise get the shroud-floor reason at the end).
    shroud_entered_once: bool,
    /// E0: shared TriggerBus + Endurance/Dodge family.
    trigger_bus: TriggerBus,
    endurance: EndurancePool,
    dodge_action: DodgeAction,
    /// E3: sole attunement owner (current + Weaver secondary).
    attunement: AttunementState,
    /// E4: sole clone-count owner.
    illusion: IllusionState,
    /// E2: lesser skill SkillEffects keyed by cast_skill_id.
    cast_skill_catalog: HashMap<u32, Vec<SkillEffect>>,
    /// E0: OnThreshold fired once for the 50% health crossing.
    threshold_50_emitted: bool,
    protection_multiplier: f64,
    resource_rules: HashMap<u32, SkillResourceRule>,
    resources: HashMap<ResourceKind, f64>,
    resource_blocked_skills: HashSet<u32>,
    /// Priority decision points, and how many of them wanted a skill the
    /// build could not pay for (specs/008: the latch alone refused builds
    /// that were merely mid-ramp).
    resource_priority_actions: u32,
    resource_blocked_events: u32,
    /// Per-pool ceilings and constant regeneration, read off the rules.
    pool_caps: HashMap<ResourceKind, f64>,
    pool_regen: HashMap<ResourceKind, f64>,
    /// Revenant upkeep currently maintained, in energy per second.
    active_upkeep: f64,
    /// Next moment a legend can be invoked (wiki `Legend`: always a 10 s
    /// recharge, and the swap resets energy to 50).
    legend_swap_ready_ms: u32,
    /// Profession mechanics this resource ledger does not model, named by
    /// the caller so a refusal can say what is missing instead of passing
    /// silently.
    resource_model_gaps: Vec<String>,
    profession: String,
    resource_model_complete: bool,
    in_shroud: Option<ShroudState>,
    weapon_set_before_shroud: u8,
    /// Entry skills already refused once (one reason line each).
    shroud_refused: HashSet<u32>,
    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1)
    /// When the player entered combat: the first cast started or the
    /// first enemy event landed. `None` until then, which is what a
    /// record's `Gate::InCombat` reads.
    combat_started_ms: Option<u32>,
    /// The build's weapons, for `Gate::Weapon`.
    equipped_weapons: Vec<EquippedWeapon>,
    /// Any loaded record carries a gate that changes between ticks, so
    /// the tick only walks the specs when there is something to walk.
    has_gate_state: bool,
}

/// Run the WvW exchange model. Effects resolve at cast completion, so incoming
/// control can genuinely cancel the action instead of merely lowering a score.
pub fn evaluate_wvw_timeline(input: WvwTimelineInput<'_>) -> WvwCombatReport {
    let WvwTimelineInput {
        skills,
        opener,
        duration_ms,
        params,
        enemy,
        scenario,
        active_effects,
        resource_rules,
        resource_model_complete,
        resource_model_gaps,
        profession,
        coverage,
        population,
        sigil_sets,
        weapon_swap_cooldown_ms,
        equipped_weapons,
        trace,
    } = input;
    let profile = WvwProfile::for_scenario(scenario, &enemy, params, duration_ms);
    let mut timeline = Timeline::new(
        skills,
        params,
        profile,
        enemy,
        active_effects,
        resource_rules,
        resource_model_complete,
        coverage,
    );
    timeline.weapon_swap_cooldown_ms = weapon_swap_cooldown_ms;
    timeline.equipped_weapons = equipped_weapons;
    timeline.resource_model_gaps = resource_model_gaps;
    timeline.profession = profession;
    timeline.assign_sigil_sets(&sigil_sets);
    timeline.population = population;
    timeline.opener = opener;
    timeline.trace_enabled = trace;
    timeline.trace_loaded_unmodeled();
    timeline.run();
    let mut report = timeline.report();
    if trace {
        // Diagnostics only (R1): the same fight eight more times with real
        // on-crit draws, so the trace can say how far the expected-value
        // process is from a proc that either fires or does not.
        let sources: Vec<String> = timeline
            .proc_specs
            .iter()
            .map(|spec| spec.source_name.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut counts: HashMap<String, Vec<u32>> = HashMap::new();
        for seed in TRIAL_SEEDS {
            let mut trial = Timeline::new(
                skills,
                params,
                timeline.profile.clone(),
                enemy,
                active_effects,
                resource_rules,
                resource_model_complete,
                Vec::new(),
            );
            trial.weapon_swap_cooldown_ms = weapon_swap_cooldown_ms;
            trial.equipped_weapons = timeline.equipped_weapons.clone();
            trial.assign_sigil_sets(&sigil_sets);
            trial.population = population;
            trial.opener = opener;
            trial.crit_mode = CritMode::Seeded(XorShift64Star::new(seed));
            trial.run();
            for source in &sources {
                counts
                    .entry(source.clone())
                    .or_default()
                    .push(trial.proc_fire_counts.get(source).copied().unwrap_or(0));
            }
        }
        let mut sources = sources;
        sources.sort();
        report.proc_trials = sources
            .into_iter()
            .map(|source| {
                let runs = &counts[&source];
                ProcTrial {
                    mean: runs.iter().map(|&n| f64::from(n)).sum::<f64>() / runs.len() as f64,
                    min: runs.iter().copied().min().unwrap_or(0),
                    max: runs.iter().copied().max().unwrap_or(0),
                    source,
                }
            })
            .collect();
    }
    report
}

impl<'a> Timeline<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        skills: &'a [RotationSkill],
        params: &'a SimParams,
        profile: WvwProfile,
        enemy: EnemyDummy,
        active_effects: &[&NormalizedEffect],
        resource_rules: &[SkillResourceRule],
        resource_model_complete: bool,
        coverage: Vec<CoverageEntry>,
    ) -> Self {
        let mut state = Self {
            skills,
            params,
            // ponytail: no-target dummy uses +inf so events keep firing and target_reached stays false
            target: {
                let mut t = TargetState::from_seed(enemy);
                if let Some(hp) = profile.target_health {
                    t.hp = Some(hp);
                } else if t.hp.is_none() {
                    t.hp = Some(f64::INFINITY);
                }
                t
            },
            player_health: params.max_health,
            profile,
            now_ms: 0,
            next_action_ms: 0,
            disabled_until_ms: 0,
            active_weapon_set: 1,
            weapon_swap_ready_ms: 0,
            opener: &[],
            opener_cursor: 0,
            weapon_swap_cooldown_ms: Some(10_000),
            cooldown_ready_ms: vec![0; skills.len()],
            auto_chain: super::AutoChain::new(skills),
            pending: None,
            scheduled_hits: Vec::new(),
            defenses: Vec::new(),
            buffs: Vec::new(),
            incoming_conditions: Vec::new(),
            combo: super::combo::ComboEngine::new(),
            target_reached_at_ms: None,
            barrier: VecDeque::new(),
            protected_run_ms: 0,
            longest_protected_window_ms: 0,
            charge_cover_consumed_this_tick: false,
            secured_tick_times: Vec::new(),
            protected_actions: Vec::new(),
            damage_events: Vec::new(),
            protected_action_count: 0,
            successful_action_count: 0,
            interrupted_casts: 0,
            control_landed_ms: 0,
            incoming_damage: 0.0,
            avoided_damage: 0.0,
            healing: 0.0,
            barrier_absorbed: 0.0,
            conditions_cleansed: 0,
            combo_activations: 0,
            proc_specs: Vec::new(),
            conditional_specs: Vec::new(),
            unmodeled_proc_keys: HashSet::new(),
            unmodeled_names: Vec::new(),
            no_record_entries: coverage,
            trace_enabled: false,
            trace: Vec::new(),
            trace_truncated: false,
            shroud_refusals: Vec::new(),
            crit_mode: CritMode::Expected,
            proc_fire_counts: HashMap::new(),
            trait_fire_counts: BTreeMap::new(),
            shroud_exit_why: None,
            trigger_status: None,
            status_trigger_depth: 0,
            prerequisite_refused: HashSet::new(),
            last_prerequisite_skip: HashMap::new(),
            trace_cap: TRACE_CAP,
            has_periodic: false,
            population: FightPopulation::solo(),
            cleave_damage: 0.0,
            cleave_condition_stack_seconds: 0.0,
            ally_boon_stack_seconds: 0.0,
            ally_healing: 0.0,
            ally_cleanses: 0,
            shroud_entered_once: false,
            trigger_bus: TriggerBus::new(),
            endurance: EndurancePool::new_full(),
            dodge_action: DodgeAction::new(),
            attunement: AttunementState::for_build(params.weaver),
            illusion: IllusionState::new(),
            cast_skill_catalog: catalog_from_skills(skills),
            threshold_50_emitted: false,
            protection_multiplier: crate::data::boon_condition_formulas::boons()
                .protection_multiplier(),
            resource_rules: resource_rules
                .iter()
                .map(|rule| (rule.skill_id, rule.clone()))
                .collect(),
            resources: initial_resources(resource_rules),
            resource_blocked_skills: HashSet::new(),
            resource_priority_actions: 0,
            resource_blocked_events: 0,
            pool_caps: resource_rules
                .iter()
                .filter(|rule| rule.pool_cap > 0.0)
                .map(|rule| (rule.kind, rule.pool_cap))
                .collect(),
            pool_regen: resource_rules
                .iter()
                .filter(|rule| rule.pool_regen_per_second > 0.0)
                .map(|rule| (rule.kind, rule.pool_regen_per_second))
                .collect(),
            active_upkeep: 0.0,
            legend_swap_ready_ms: 0,
            resource_model_gaps: Vec::new(),
            profession: String::new(),
            resource_model_complete,
            in_shroud: None,
            weapon_set_before_shroud: 1,
            shroud_refused: HashSet::new(),
            combat_started_ms: None,
            equipped_weapons: Vec::new(),
            has_gate_state: false,
        };
        state.load_normalized_effects(active_effects);
        state.has_periodic = state
            .proc_specs
            .iter()
            .any(|spec| matches!(spec.trigger, TriggerRule::Periodic));
        state.has_gate_state = state.proc_specs.iter().any(|spec| {
            spec.gates
                .iter()
                .any(|g| matches!(g.gate, Gate::HealthThreshold { .. }))
        });
        state
    }

    fn load_normalized_effects(&mut self, effects: &[&NormalizedEffect]) {
        for effect in effects {
            if matches!(effect.trigger_rule, TriggerRule::NotApplicable) {
                // A coverage block claims nothing: the caller already
                // put its class on the coverage line.
                continue;
            }
            // Doctrine rule 6: a record the timeline has no state for
            // abstains naming the missing piece, never silently.
            if let Some(reason) = unexecutable_reason(effect) {
                self.note_unmodeled(format!("{} ({reason})", effect.source_name));
                continue;
            }
            if matches!(effect.trigger_rule, TriggerRule::Passive) {
                // Standing modifiers are already folded into SimParams by the
                // shared combat parser. Applying them here would count the same
                // trait/rune/sigil a second time.
                continue;
            }

            if matches!(effect.source_type, SourceType::Skill)
                && self.skill_directly_models_effect(effect)
            {
                continue;
            }

            // Conditional strike bonuses (US3): a health threshold or a
            // stacking bonus on strike damage becomes a ConditionalSpec.
            let strike_bonus = matches!(effect.category, EffectCategory::StrikeDamagePct)
                || matches!(effect.inner_category, Some(EffectCategory::StrikeDamagePct));
            let crit_bonus = matches!(effect.category, EffectCategory::CritDamagePct)
                || matches!(effect.inner_category, Some(EffectCategory::CritDamagePct));
            let crit_chance = matches!(effect.category, EffectCategory::CritChancePct)
                || matches!(effect.inner_category, Some(EffectCategory::CritChancePct));
            // Per-stack bonus on a foe condition (Sprint 3, US3).
            if (strike_bonus || crit_bonus || crit_chance)
                && matches!(effect.trigger_rule, TriggerRule::Conditional)
                && effect.max_stacks.is_some()
                && effect
                    .prerequisite
                    .as_ref()
                    .is_some_and(|p| p.foe_condition.is_some())
            {
                let (Some(condition), Some(&max), Some(&percent)) = (
                    effect
                        .prerequisite
                        .as_ref()
                        .and_then(|p| p.foe_condition.clone()),
                    effect.max_stacks.as_ref().and_then(resolved),
                    resolved(&effect.value),
                ) else {
                    self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                    continue;
                };
                self.conditional_specs.push(ConditionalSpec {
                    source_name: effect.source_name.clone(),
                    kind: ConditionalKind::PerFoeStack { condition, max },
                    percent,
                    crit_damage: crit_bonus,
                    crit_chance,
                    condition_damage: false,
                    active: false,
                    stacks: 0,
                    expires_at_ms: 0,
                    stack_expiries: Vec::new(),
                    stacking_rule: effect.stacking_rule.clone(),
                });
                continue;
            }
            // In-shroud bonuses (Sprint 3, US1): active while in shroud.
            if (strike_bonus || crit_bonus || crit_chance)
                && matches!(effect.trigger_rule, TriggerRule::Conditional)
                && effect
                    .prerequisite
                    .as_ref()
                    .is_some_and(|p| p.in_shroud.is_none())
            {
                let (Some(prerequisite), Some(&percent)) =
                    (effect.prerequisite.clone(), resolved(&effect.value))
                else {
                    self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                    continue;
                };
                self.conditional_specs.push(ConditionalSpec {
                    source_name: effect.source_name.clone(),
                    kind: ConditionalKind::Prerequisite(prerequisite),
                    percent,
                    crit_damage: crit_bonus,
                    crit_chance,
                    condition_damage: false,
                    active: false,
                    stacks: 0,
                    expires_at_ms: 0,
                    stack_expiries: Vec::new(),
                    stacking_rule: effect.stacking_rule.clone(),
                });
                continue;
            }
            if (strike_bonus || crit_bonus || crit_chance)
                && matches!(effect.trigger_rule, TriggerRule::Conditional)
                && effect
                    .prerequisite
                    .as_ref()
                    .is_some_and(|p| p.in_shroud == Some(true))
            {
                let Some(&percent) = resolved(&effect.value) else {
                    self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                    continue;
                };
                self.conditional_specs.push(ConditionalSpec {
                    source_name: effect.source_name.clone(),
                    kind: ConditionalKind::InShroud,
                    percent,
                    crit_damage: crit_bonus,
                    crit_chance,
                    condition_damage: false,
                    active: false,
                    stacks: 0,
                    expires_at_ms: 0,
                    stack_expiries: Vec::new(),
                    stacking_rule: effect.stacking_rule.clone(),
                });
                continue;
            }
            if strike_bonus && matches!(effect.trigger_rule, TriggerRule::OnHealthThreshold) {
                let (Some(threshold), Some(&percent)) =
                    (effect.health_threshold.as_ref(), resolved(&effect.value))
                else {
                    self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                    continue;
                };
                let Some(&line) = resolved(&threshold.percent) else {
                    self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                    continue;
                };
                self.conditional_specs.push(ConditionalSpec {
                    source_name: effect.source_name.clone(),
                    kind: ConditionalKind::Threshold {
                        above: threshold.above,
                        percent: line,
                    },
                    percent,
                    crit_damage: false,
                    crit_chance: false,
                    condition_damage: false,
                    active: false,
                    stacks: 0,
                    expires_at_ms: 0,
                    stack_expiries: Vec::new(),
                    stacking_rule: effect.stacking_rule.clone(),
                });
                continue;
            }
            if strike_bonus
                && matches!(effect.trigger_rule, TriggerRule::OnHit)
                && effect.max_stacks.is_some()
            {
                let (Some(&max), Some(&duration), Some(&percent)) = (
                    effect.max_stacks.as_ref().and_then(resolved),
                    effect.effect_duration.as_ref().and_then(resolved),
                    resolved(&effect.value),
                ) else {
                    self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                    continue;
                };
                self.conditional_specs.push(ConditionalSpec {
                    source_name: effect.source_name.clone(),
                    kind: ConditionalKind::Stacking {
                        max,
                        duration_ms: (duration * 1_000.0).round() as u32,
                        scope: effect.trigger_scope.clone().unwrap_or_default(),
                        hit_fed: true,
                    },
                    percent,
                    crit_damage: false,
                    crit_chance: false,
                    condition_damage: false,
                    active: false,
                    stacks: 0,
                    expires_at_ms: 0,
                    stack_expiries: Vec::new(),
                    stacking_rule: effect.stacking_rule.clone(),
                });
                continue;
            }

            let scoped = !matches!(
                effect.trigger_scope,
                None | Some(crate::data::normalized_effects::TriggerScope::Any)
            );
            let supported = matches!(
                effect.trigger_rule,
                TriggerRule::OnHit
                    | TriggerRule::OnCrit
                    | TriggerRule::OnShroudEnter
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
                    | TriggerRule::OnLegendSwap
                    | TriggerRule::OnStunbreak
                    | TriggerRule::OnBoonGained { .. }
            ) || (matches!(effect.trigger_rule, TriggerRule::OnSkillUse)
                // A skill's own record, or a trait's that names its skills (US2).
                && (matches!(effect.source_type, SourceType::Skill) || scoped));
            if !supported {
                self.note_unmodeled(format!(
                    "{} ({})",
                    effect.source_name,
                    trigger_label(&effect.trigger_rule)
                ));
                continue;
            }
            let Some(&value) = resolved(&effect.value) else {
                self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                continue;
            };
            // A duration the page did not give for this mode is unresolved,
            // never zero (Sprint 3: Soul Barbs' competitive duration).
            if effect
                .effect_duration
                .as_ref()
                .is_some_and(|d| !d.is_resolved())
            {
                self.note_unmodeled(format!("{} (unresolved value)", effect.source_name));
                continue;
            }

            if let Some(cast_id) = effect.cast_skill_id {
                if let Some(op) = effect.status_operation.as_ref() {
                    let effects = skill_effects_from_status_operation(op);
                    if !effects.is_empty() {
                        self.cast_skill_catalog.entry(cast_id).or_insert(effects);
                    }
                }
            }
            self.proc_specs.push(ProcSpec {
                source_type: effect.source_type.clone(),
                source_id: effect.source_id,
                source_name: effect.source_name.clone(),
                trigger: effect.trigger_rule.clone(),
                category: effect
                    .inner_category
                    .clone()
                    .unwrap_or_else(|| effect.category.clone()),
                value,
                duration_ms: effect
                    .effect_duration
                    .as_ref()
                    .and_then(resolved)
                    .map(|seconds| (seconds * 1_000.0).round() as u32)
                    .unwrap_or(0),
                internal_cooldown_ms: effect
                    .internal_cooldown
                    .as_ref()
                    .and_then(resolved)
                    .map(|seconds| (seconds * 1_000.0).round() as u32)
                    .or_else(|| {
                        effect
                            .status_operation
                            .as_ref()
                            .and_then(|op| op.internal_cooldown_ms.as_ref())
                            .and_then(resolved)
                            .copied()
                    })
                    .unwrap_or(0),
                next_ready_ms: 0,
                operation: effect.status_operation.clone(),
                weapon_set: 0,
                proc_chance: effect
                    .proc_chance
                    .as_ref()
                    .and_then(resolved)
                    .copied()
                    .unwrap_or(1.0),
                mass: 0.0,
                scope: effect.trigger_scope.clone().unwrap_or_default(),
                prerequisite: effect.prerequisite.clone(),
                scale_by: effect.scale_by.clone(),
                healing_power_coefficient: effect
                    .healing_power_coefficient
                    .as_ref()
                    .and_then(resolved)
                    .copied()
                    .unwrap_or(0.0),
                cast_skill_id: effect.cast_skill_id,
                max_stacks: effect
                    .max_stacks
                    .as_ref()
                    .and_then(resolved)
                    .copied()
                    .unwrap_or(0),
                gates: effect
                    .gates
                    .iter()
                    .map(|gate| GateState {
                        // An interval opens after one full period, not
                        // at t=0: three firings in ten seconds at 3 s.
                        next_ms: match gate {
                            Gate::Interval { every_ms, .. } => *every_ms,
                            _ => 0,
                        },
                        gate: gate.clone(),
                        latched: false,
                    })
                    .collect(),
                scale: effect.scale.clone(),
                stacking_rule: effect.stacking_rule.clone(),
            });
        }
    }

    fn run(&mut self) {
        while self.now_ms < self.profile.duration_ms && self.player_health > 0.0 {
            self.charge_cover_consumed_this_tick = false;
            if self.has_gate_state {
                self.rearm_health_gates();
            }
            self.expire_timed_state();
            self.regenerate_resources();
            self.tick_endurance_and_dodge();
            self.tick_health_threshold_bus();
            self.land_scheduled_hits();
            self.resolve_pending_cast();
            self.tick_conditions();
            self.process_enemy_events();
            self.track_protected_window();
            if self.has_periodic {
                self.trigger_procs(TriggerRule::Periodic, None, false, 1.0);
            }

            if self.pending.is_none()
                && self.now_ms >= self.next_action_ms
                && self.now_ms >= self.disabled_until_ms
            {
                self.try_legend_swap();
                if let Some(skill_idx) = self.pick_skill() {
                    self.start_cast(skill_idx);
                } else {
                    self.try_weapon_swap();
                }
            } else if self.pending.is_none() {
                self.try_stunbreak();
            }

            self.now_ms = self.now_ms.saturating_add(TIMELINE_TICK_MS);
            self.tick_recharge_rate();
        }
        // A cast that finishes as the window closes still delivered its hits.
        if self.player_health > 0.0 {
            self.now_ms = self.now_ms.min(self.profile.duration_ms);
            self.land_scheduled_hits();
        }
        self.note_never_fired();
    }

    /// How fast skills come back this tick.
    ///
    /// Dummy clock: 100ms wall consumes 125ms CD under Alacrity, and 34ms
    /// under Chilled. Apply *4/5 at set is the leftover snapshot.
    ///
    /// Only the Alacrity half of this existed. Nothing the enemy did could
    /// touch skill availability, which leaves out the thing that actually
    /// kills a support: Chilled does not have to out-damage your healing, it
    /// only has to keep your heal on cooldown until the next burst lands. It
    /// is also what makes cleansing existential rather than a damage tax -
    /// the cleanse is buying back the heal, not the 130/s tick.
    fn tick_recharge_rate(&mut self) {
        if !self.now_ms.is_multiple_of(100) {
            return;
        }
        // Chilled wins: the wiki is explicit that Alacrity and Chilled are
        // both recharge-rate modifiers and the slow applies to the already
        // hastened rate, but a build that is Chilled through its whole
        // recovery window is in the situation this models either way.
        if self.has_condition("Chilled") {
            let lost = 100 - CHILLED_RECHARGE_PERCENT;
            for ready in &mut self.cooldown_ready_ms {
                if *ready > self.now_ms {
                    *ready = ready.saturating_add(lost);
                }
            }
            return;
        }
        let extra = alacrity_cd_advance_ms(100, self.has_buff("Alacrity")).saturating_sub(100);
        if extra == 0 {
            return;
        }
        for ready in &mut self.cooldown_ready_ms {
            if *ready > self.now_ms {
                *ready = ready.saturating_sub(extra);
            }
        }
    }

    /// Whether a condition the enemy applied is currently on the player.
    fn has_condition(&self, name: &str) -> bool {
        self.incoming_conditions
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(name) && c.expires_at_ms > self.now_ms)
    }

    fn expire_timed_state(&mut self) {
        self.update_conditionals();
        self.defenses
            .retain(|defense| defense.expires_at_ms > self.now_ms);
        self.buffs.retain(|buff| buff.expires_at_ms > self.now_ms);
        self.barrier
            .retain(|layer| layer.expires_at_ms > self.now_ms);
        self.combo.tick_expiry(self.now_ms);
    }

    /// Land every scheduled hit that is due, in the order they were queued.
    /// Protection is judged when the hit lands, the same way a resolved cast
    /// judges it, so a channel that starts inside a window and runs out of it
    /// splits its hits between the two.
    fn land_scheduled_hits(&mut self) {
        if self.scheduled_hits.is_empty() {
            return;
        }
        let protected = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.protected_at_start || pending.saved_by_charge)
            || self.control_owned();
        let mut i = 0;
        while i < self.scheduled_hits.len() {
            if self.scheduled_hits[i].at_ms > self.now_ms {
                i += 1;
                continue;
            }
            let hit = self.scheduled_hits.remove(i);
            self.apply_skill_effect(
                hit.skill_id,
                &SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: hit.dmg_multiplier,
                },
                protected,
            );
        }
    }

    fn resolve_pending_cast(&mut self) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        if pending.resolves_at_ms > self.now_ms {
            return;
        }
        // The final hit is scheduled for this very tick; land it before the
        // cast's other effects so the order matches the game.
        self.land_scheduled_hits();
        self.pending = None;
        // Wiki `Skill` (read 2026-09-07): "Once the activation is complete a
        // skill will enter a recharge time before it may be used again."
        // ponytail: channels really start recharging at the start of their
        // active phase (wiki `Channeled skill`); we have no phase data, so a
        // channel's recharge runs late by its channel length here.
        {
            let skill = &self.skills[pending.skill_idx];
            let (skill_id, cooldown_ms) = (skill.skill_id, skill.cooldown_ms);
            self.set_skill_cooldown(skill_id, cooldown_ms);
        }
        self.successful_action_count += 1;
        self.gain_resource_on_use(self.skills[pending.skill_idx].skill_id);
        let protected_before = pending.protected_at_start || pending.saved_by_charge;
        let first_damage_event = self.damage_events.len();
        let control_before = self.control_landed_ms;
        let skill_id = self.skills[pending.skill_idx].skill_id;
        let effects = self.skills[pending.skill_idx].effects.clone();
        let applies_condition = effects
            .iter()
            .any(|effect| matches!(effect, SkillEffect::ApplyCondition { .. }));
        let supports_allies = effects.iter().any(|effect| {
            matches!(
                effect,
                SkillEffect::Healing { .. }
                    | SkillEffect::Barrier { .. }
                    | SkillEffect::RemovesCondition { .. }
                    | SkillEffect::ConvertConditions
                    | SkillEffect::ApplyBuff { .. }
            )
        });
        for effect in effects {
            // Strikes were scheduled at cast start and have landed by now.
            if matches!(effect, SkillEffect::StrikeDamage { .. }) {
                continue;
            }
            self.apply_skill_effect(skill_id, &effect, protected_before);
        }
        self.trigger_procs(
            TriggerRule::OnSkillUse,
            Some(skill_id),
            protected_before,
            1.0,
        );
        let protected = protected_before || self.control_owned();
        if protected {
            self.protected_action_count += 1;
            for event in &mut self.damage_events[first_damage_event..] {
                event.protected = true;
            }
            self.protected_actions.push(ProtectedActionEvent {
                at_ms: self.now_ms,
                skill_id,
                control_ms: self.control_landed_ms.saturating_sub(control_before),
                applies_condition,
                supports_allies,
            });
        }
    }

    fn start_cast(&mut self, skill_idx: usize) {
        self.combat_started_ms.get_or_insert(self.now_ms);
        let skill = &self.skills[skill_idx];
        self.pay_resource(skill.skill_id);
        if matches!(skill.slot, super::SkillSlot::Elite) {
            self.trigger_bus.emit(BusEvent::OnElite, self.now_ms);
            self.trigger_procs(TriggerRule::OnElite, Some(skill.skill_id), false, 1.0);
        }
        // E3: profession attune skills mutate AttunementState and emit OnAttunementSwap.
        let attune_name = skill.name.clone();
        let attune_id = skill.skill_id;
        if let Some(element) = apply_attunement_skill(
            &mut self.attunement,
            &mut self.trigger_bus,
            self.now_ms,
            &attune_name,
        ) {
            self.trigger_status = Some(element.as_str().to_string());
            self.trigger_procs(TriggerRule::OnAttunementSwap, Some(attune_id), false, 1.0);
            self.trigger_status = None;
        }
        self.resource_blocked_skills.remove(&skill.skill_id);
        if let Some(rule) = self.resource_rules.get(&skill.skill_id).cloned() {
            if rule.enters_shroud {
                self.enter_shroud(skill.skill_id, &rule);
            } else if rule.exits_shroud {
                self.exit_shroud("exit skill");
            }
        }
        let skill = &self.skills[skill_idx];
        self.apply_incoming_confusion_on_skill_use();
        let quickness = self.has_buff("Quickness");
        let cast_ms = if quickness {
            (skill.cast_time_ms * 2 + 1) / 3
        } else {
            skill.cast_time_ms
        }
        .max(TIMELINE_TICK_MS);
        self.auto_chain
            .on_cast(skill_idx, skill.is_auto_attack(), self.at(cast_ms));
        // Strikes land across the activation, not as one lump at the end:
        // measured spacing where data/formulas/hit_timing.json has it, an
        // even spread otherwise. Everything else the skill does resolves at
        // cast end as before.
        let hits: Vec<ScheduledHit> = skill
            .effects
            .iter()
            .filter_map(|effect| match effect {
                SkillEffect::StrikeDamage {
                    hit_count,
                    dmg_multiplier,
                } => Some((*hit_count, *dmg_multiplier)),
                _ => None,
            })
            .flat_map(|(hit_count, dmg_multiplier)| {
                let per_hit = dmg_multiplier / hit_count.max(1) as f64;
                crate::data::hit_timing::hit_schedule(&skill.name, cast_ms, hit_count)
                    .into_iter()
                    .map(move |offset| (offset, per_hit))
            })
            .map(|(offset, per_hit)| ScheduledHit {
                at_ms: self.at(offset),
                skill_id: skill.skill_id,
                dmg_multiplier: per_hit,
            })
            .collect();
        self.scheduled_hits.extend(hits);
        // Recharge starts when the cast resolves, not here — see
        // `resolve_pending_cast`. An interrupted cast gets the short interrupt
        // recharge instead, which only works if the full one is not yet set.
        self.pending = Some(PendingCast {
            skill_idx,
            started_at_ms: self.now_ms,
            resolves_at_ms: self.at(cast_ms),
            protected_at_start: self.control_owned(),
            saved_by_charge: false,
        });
        self.next_action_ms = self.at(cast_ms
            .saturating_add(HUMAN_DELAY_MS)
            .saturating_add(MIN_SKILL_GAP_MS));
        // A maintained skill starts draining energy now. Wiki `Energy`:
        // total upkeep is capped at 10 (-5 %/s net).
        if let Some(upkeep) = self
            .resource_rules
            .get(&skill.skill_id)
            .map(|rule| rule.upkeep)
            .filter(|upkeep| *upkeep > 0.0)
        {
            self.active_upkeep = (self.active_upkeep + upkeep).min(MAX_UPKEEP);
        }
    }

    fn pick_skill(&mut self) -> Option<usize> {
        self.resource_priority_actions += 1;
        // Follow the published rotation while it can be followed. A skill
        // already on recharge was pressed; one on the other set asks for a
        // swap when the swap is ready and is skipped when it is not; one the
        // build cannot pay for hands over to the scorer below.
        while let Some(&want) = self.opener.get(self.opener_cursor) {
            let Some(idx) = self.skills.iter().position(|s| s.skill_id == want) else {
                self.opener_cursor += 1;
                continue;
            };
            if self.cooldown_ready_ms[idx] > self.now_ms {
                self.opener_cursor += 1;
                continue;
            }
            if !self.skill_available(&self.skills[idx]) {
                if matches!(self.skills[idx].weapon_set, 1 | 2)
                    && self.weapon_swap_cooldown_ms.is_some()
                    && self.now_ms >= self.weapon_swap_ready_ms
                {
                    return None;
                }
                self.opener_cursor += 1;
                continue;
            }
            if !self.can_pay_resource(want) {
                if self.note_shroud_refusal(want) {
                    // A shroud entry the pool cannot afford is skipped with a
                    // readable reason; the rest of the opener goes on.
                    self.opener_cursor += 1;
                    continue;
                }
                break;
            }
            self.opener_cursor += 1;
            return Some(idx);
        }
        let health_ratio = self.player_health / self.params.max_health.max(1.0);
        let cover_remaining = self.control_cover_remaining_ms();
        let enemy_event_soon = self
            .profile
            .enemy_events
            .front()
            .is_some_and(|event| event.at_ms <= self.at(900));

        let mut best: Option<(usize, f64)> = None;
        let mut filler = None;
        let mut highest_unpaid: Option<(u32, f64)> = None;
        let mut refused_entries: Vec<u32> = Vec::new();
        for (idx, skill) in self.skills.iter().enumerate() {
            if self.cooldown_ready_ms[idx] > self.now_ms || !self.skill_available(skill) {
                continue;
            }
            if skill.is_auto_attack() {
                if !self.auto_chain.is_follow_up(idx) {
                    filler = Some(idx);
                }
                continue;
            }
            let has_heal = skill
                .effects
                .iter()
                .any(|effect| matches!(effect, SkillEffect::Healing { .. }));
            if has_heal && health_ratio > 0.72 {
                continue;
            }
            let has_cover = skill.effects.iter().any(is_control_cover);
            let has_strip = skill.effects.iter().any(|effect| {
                matches!(
                    effect,
                    SkillEffect::StripBoons { .. }
                        | SkillEffect::StealBoons
                        | SkillEffect::CorruptBoons
                )
            });
            let has_control = skill
                .effects
                .iter()
                .any(|effect| matches!(effect, SkillEffect::CrowdControl { .. }));

            let mut priority = self.skill_damage_value(skill);
            if has_heal {
                priority += (1.0 - health_ratio) * 1_000_000.0;
            }
            if has_cover && (cover_remaining < self.profile.required_window_ms || enemy_event_soon)
            {
                priority += 900_000.0;
            }
            // Strip Stability/Protection before trying to CC or dump damage.
            if has_strip && (self.target.stability || self.target.protection) {
                priority += 800_000.0;
            }
            if has_control && !self.target.stability {
                priority += 700_000.0;
            }
            if has_control && self.target.stability {
                priority -= 500_000.0;
            }
            // Sprint 3 (specs/007-trait-triggers): the shroud is the build.
            // An affordable entry outranks weapon damage; the improviser
            // used to leave a Reaper out of shroud for the whole fight.
            if self.is_shroud_entry(skill.skill_id)
                && self.in_shroud.is_none()
                && self.can_pay_resource(skill.skill_id)
            {
                priority += 600_000.0;
            }
            if !self.can_pay_resource(skill.skill_id) {
                if self.is_shroud_entry(skill.skill_id) {
                    refused_entries.push(skill.skill_id);
                    continue;
                }
                if highest_unpaid
                    .as_ref()
                    .is_none_or(|(_, blocked_priority)| priority > *blocked_priority)
                {
                    highest_unpaid = Some((skill.skill_id, priority));
                }
                continue;
            }
            if best.as_ref().is_none_or(|(_, score)| priority > *score) {
                best = Some((idx, priority));
            }
        }
        for skill_id in refused_entries {
            self.note_shroud_refusal(skill_id);
        }
        let legal_priority = best.as_ref().map(|(_, score)| *score).unwrap_or(0.0);
        if let Some((skill_id, blocked_priority)) = highest_unpaid {
            if blocked_priority > legal_priority {
                self.resource_blocked_skills.insert(skill_id);
                // A decision counts as resource-blocked only when the pool
                // left NOTHING to press. Wanting a 50-energy elite while
                // pressing an affordable skill is how every resource
                // profession plays; it is not the bar failing.
                if best.is_none() {
                    self.resource_blocked_events += 1;
                }
            }
        }
        best.map(|(idx, _)| idx)
            .or(filler.map(|head| self.auto_chain.step(head, self.now_ms)))
    }

    fn skill_available(&self, skill: &RotationSkill) -> bool {
        skill.weapon_set == 0 || skill.weapon_set == self.active_weapon_set
    }

    fn try_weapon_swap(&mut self) {
        if self.in_shroud.is_some() {
            // wiki `Death Shroud`: no weapon swap while in shroud.
            return;
        }
        let Some(cooldown_ms) = self.weapon_swap_cooldown_ms else {
            return;
        };
        if self.now_ms < self.weapon_swap_ready_ms
            || !self.skills.iter().any(|skill| skill.weapon_set > 0)
        {
            return;
        }
        let other = if self.active_weapon_set == 1 { 2 } else { 1 };
        if self.skills.iter().enumerate().any(|(idx, skill)| {
            skill.weapon_set == other && self.cooldown_ready_ms[idx] <= self.now_ms
        }) {
            self.active_weapon_set = other;
            self.weapon_swap_ready_ms = self.at(cooldown_ms);
            self.next_action_ms = self.at(MIN_SKILL_GAP_MS);
            self.trace(TraceKind::WeaponSwap, "weapon swap", format!("set {other}"));
        }
    }

    fn try_stunbreak(&mut self) {
        if self.now_ms >= self.disabled_until_ms {
            return;
        }
        self.resource_priority_actions += 1;
        let candidates: Vec<usize> = self
            .skills
            .iter()
            .enumerate()
            .filter(|(idx, skill)| {
                skill.is_stunbreak
                    && self.cooldown_ready_ms[*idx] <= self.now_ms
                    && self.skill_available(skill)
            })
            .map(|(idx, _)| idx)
            .collect();
        let blocked_before = self.resource_blocked_skills.len();
        let Some(idx) = candidates.into_iter().find(|idx| {
            let skill_id = self.skills[*idx].skill_id;
            let can_pay = self.can_pay_resource(skill_id);
            if !can_pay {
                self.resource_blocked_skills.insert(skill_id);
            }
            can_pay
        }) else {
            // One decision, one count: scanning three unaffordable
            // stunbreaks before giving up is still a single moment where
            // the pool left nothing to press, exactly as in `pick_skill`.
            if self.resource_blocked_skills.len() > blocked_before {
                self.resource_blocked_events += 1;
            }
            return;
        };
        let skill_id = self.skills[idx].skill_id;
        self.pay_resource(skill_id);
        self.resource_blocked_skills.remove(&skill_id);
        self.set_skill_cooldown(skill_id, self.skills[idx].cooldown_ms);
        self.disabled_until_ms = self.now_ms;
        self.trigger_procs(TriggerRule::OnStunbreak, Some(skill_id), false, 1.0);
        self.next_action_ms = self.at(MIN_SKILL_GAP_MS);
        self.successful_action_count += 1;
        let first_damage_event = self.damage_events.len();
        let control_before = self.control_landed_ms;
        let applies_condition = self.skills[idx]
            .effects
            .iter()
            .any(|effect| matches!(effect, SkillEffect::ApplyCondition { .. }));
        let supports_allies = self.skills[idx].effects.iter().any(|effect| {
            matches!(
                effect,
                SkillEffect::Healing { .. }
                    | SkillEffect::Barrier { .. }
                    | SkillEffect::RemovesCondition { .. }
                    | SkillEffect::ConvertConditions
                    | SkillEffect::ApplyBuff { .. }
            )
        });
        for effect in self.skills[idx].effects.clone() {
            self.apply_skill_effect(skill_id, &effect, true);
        }
        self.trigger_procs(TriggerRule::OnSkillUse, Some(skill_id), true, 1.0);
        for event in &mut self.damage_events[first_damage_event..] {
            event.protected = true;
        }
        self.protected_action_count += 1;
        self.protected_actions.push(ProtectedActionEvent {
            at_ms: self.now_ms,
            skill_id,
            control_ms: self.control_landed_ms.saturating_sub(control_before),
            applies_condition,
            supports_allies,
        });
    }

    fn process_enemy_events(&mut self) {
        if !self.enemy_hp_alive() {
            return;
        }
        while self
            .profile
            .enemy_events
            .front()
            .is_some_and(|event| event.at_ms <= self.now_ms)
        {
            let event = self
                .profile
                .enemy_events
                .pop_front()
                .expect("front checked");
            // A disabled opponent cannot continue a queued attack/cast. Existing
            // conditions still tick separately, but new strikes, CC, strips and
            // condition applications are lost during the control window.
            if self.target.disabled_until_ms > self.now_ms {
                continue;
            }
            self.combat_started_ms.get_or_insert(self.now_ms);
            match event.kind {
                EnemyEventKind::Strike {
                    damage,
                    unblockable,
                } => self.receive_strike(damage, unblockable),
                EnemyEventKind::Control {
                    duration_ms,
                    unblockable,
                } => self.receive_control(duration_ms, unblockable),
                EnemyEventKind::Condition {
                    condition,
                    stacks,
                    duration_ms,
                } => self.receive_condition(condition, stacks, duration_ms),
                EnemyEventKind::BoonStrip { count } => self.receive_boon_strip(count),
            }
        }
    }

    fn receive_strike(&mut self, raw_damage: f64, unblockable: bool) {
        if self.avoids_attack(unblockable) {
            self.avoided_damage += raw_damage;
            return;
        }
        let mut damage = raw_damage;
        if self.has_defense(CoverKind::Protection) {
            damage *= self.protection_multiplier;
        }
        // wiki `Death's Carapace` (read 2026-09-08): 20 toughness per stack
        // in WvW, 30 stacks at most; a strike scales with 1 / armor, so the
        // profile's strike (built on `params.armor`) shrinks by that ratio.
        let carapace = 20.0 * self.buff_stacks("Death's Carapace").min(30) as f64;
        if carapace > 0.0 {
            let armor = self.params.armor.max(1_000.0);
            damage *= armor / (armor + carapace);
        }
        self.absorb_damage(damage);
    }

    fn receive_control(&mut self, duration_ms: u32, unblockable: bool) {
        if self.avoids_attack(unblockable) || self.consume_stability() {
            return;
        }
        self.cancel_pending_cast();
        self.disabled_until_ms = self.disabled_until_ms.max(self.at(duration_ms));
        self.protected_run_ms = 0;
    }

    /// The pending cast dies: its unlanded hits are lost and the skill gets
    /// the interrupt recharge (control, or a forced shroud exit).
    fn cancel_pending_cast(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.auto_chain.reset();
            if pending.started_at_ms < self.now_ms {
                self.interrupted_casts += 1;
            }
            // Hits that had not landed yet die with the cast.
            let lost = self.scheduled_hits.len();
            self.scheduled_hits.clear();
            let skill_id = self.skills[pending.skill_idx].skill_id;
            if self.trace_enabled {
                let name = self.skill_name(skill_id);
                self.trace(
                    TraceKind::CastInterrupted,
                    &name,
                    format!("{lost} hits lost"),
                );
            }
            self.set_skill_cooldown(skill_id, INTERRUPT_COOLDOWN_MS);
        }
    }

    fn is_shroud_entry(&self, skill_id: u32) -> bool {
        self.resource_rules
            .get(&skill_id)
            .is_some_and(|rule| rule.enters_shroud)
    }

    /// A shroud entry the pool cannot afford: one readable reason per
    /// entry skill and a trace event. Returns whether `skill_id` is one.
    fn note_shroud_refusal(&mut self, skill_id: u32) -> bool {
        let Some(rule) = self.resource_rules.get(&skill_id).cloned() else {
            return false;
        };
        if !rule.enters_shroud {
            return false;
        }
        if self.shroud_refused.insert(skill_id) {
            let cap = resource_cap(ResourceKind::LifeForce, self.params.max_health).max(1.0);
            let have = self
                .resources
                .get(&ResourceKind::LifeForce)
                .copied()
                .unwrap_or(0.0);
            let name = self.skill_name(skill_id);
            let reason = format!(
                "{name} needs {:.0}% life force, had {:.0}%",
                rule.entry_floor / cap * 100.0,
                have / cap * 100.0
            );
            self.trace(TraceKind::ShroudRefused, &name, reason.clone());
            self.shroud_refusals.push(reason);
        }
        true
    }

    fn enter_shroud(&mut self, skill_id: u32, rule: &SkillResourceRule) {
        let exit_recharge_ms = self
            .skills
            .iter()
            .find(|s| s.skill_id == skill_id)
            .map(|s| s.cooldown_ms)
            .unwrap_or(10_000);
        self.weapon_set_before_shroud = self.active_weapon_set;
        self.active_weapon_set = super::SHROUD_SET;
        self.in_shroud = Some(ShroudState {
            entry_skill_id: skill_id,
            drain_per_second: rule.drain_per_second.max(0.0),
            damage_factor: if rule.shroud_damage_factor.is_nan() {
                1.0
            } else {
                rule.shroud_damage_factor
            },
            health_exposed: rule.shroud_health_exposed,
            exit_recharge_ms,
        });
        let cap = resource_cap(ResourceKind::LifeForce, self.params.max_health).max(1.0);
        let have = self
            .resources
            .get(&ResourceKind::LifeForce)
            .copied()
            .unwrap_or(0.0);
        let name = self.skill_name(skill_id);
        self.trace(
            TraceKind::ShroudEntered,
            &name,
            format!("{:.0}% life force", have / cap * 100.0),
        );
        self.shroud_entered_once = true;
        // Sprint 3 (US1): the entry is a firing site, then the in-shroud
        // bonuses switch on.
        self.trigger_procs(TriggerRule::OnShroudEnter, Some(skill_id), false, 1.0);
        self.update_conditionals();
    }

    /// Leave shroud: `why` is `exit skill`, `opener` or `life force 0`; the
    /// forced exit cancels the pending cast the way an interrupt does.
    fn exit_shroud(&mut self, why: &str) {
        if self.in_shroud.is_none() {
            return;
        }
        // Sprint 3 (US1): exit records fire while the state still stands, so
        // an in-shroud prerequisite on them holds; `why` reaches the trace.
        self.shroud_exit_why = Some(why.to_string());
        self.trigger_procs(TriggerRule::OnShroudExit, None, false, 1.0);
        self.shroud_exit_why = None;
        let Some(state) = self.in_shroud.take() else {
            return;
        };
        self.active_weapon_set = self.weapon_set_before_shroud;
        if why == "life force 0" {
            self.cancel_pending_cast();
        }
        self.set_skill_cooldown(state.entry_skill_id, state.exit_recharge_ms);
        let name = self.skill_name(state.entry_skill_id);
        self.trace(TraceKind::ShroudExited, &name, why);
        self.update_conditionals();
    }

    /// Life force credited when a cast resolves (Percent fact "Life Force").
    fn gain_resource_on_use(&mut self, skill_id: u32) {
        let Some(rule) = self.resource_rules.get(&skill_id).cloned() else {
            return;
        };
        if rule.gain_on_use <= 0.0 {
            return;
        }
        let cap = resource_cap(rule.kind, self.params.max_health);
        let resource = self.resources.entry(rule.kind).or_default();
        *resource = (*resource + rule.gain_on_use).min(cap);
        let pool = *resource;
        if rule.kind == ResourceKind::LifeForce {
            let name = self.skill_name(skill_id);
            self.trace(
                TraceKind::LifeForceGained,
                &name,
                format!(
                    "{:.0}% → {:.0}%",
                    rule.gain_on_use / cap.max(1.0) * 100.0,
                    pool / cap.max(1.0) * 100.0
                ),
            );
        }
    }

    fn receive_condition(&mut self, condition: String, stacks: u32, duration_ms: u32) {
        if self.has_defense(CoverKind::Invulnerability)
            || (self.has_defense(CoverKind::Resistance) && !condition_is_damaging(&condition))
        {
            return;
        }
        self.incoming_conditions.push(TimedCondition {
            name: super::combat_model::intern_foe_condition_name(&condition),
            stacks,
            expires_at_ms: self.at(duration_ms),
            next_tick_ms: self.at(1_000),
        });
    }

    fn receive_boon_strip(&mut self, count: u32) {
        // Generic strips are last-in-first-out over first application
        // (community-tested, not dev-stated: MetaBattle WvW Firebrand guide,
        // forum 153106, read 2026-09-07). Corrupts are random since the
        // 2015-06-23 patch; the enemy script only strips, so no random path.
        for _ in 0..count {
            let Some((idx, _)) = self
                .defenses
                .iter()
                .enumerate()
                .filter(|(_, defense)| defense.strippable)
                .max_by_key(|(_, defense)| defense.applied_at_ms)
            else {
                break;
            };
            self.defenses.remove(idx);
        }
    }

    fn remove_enemy_boons(&mut self, count: u32) -> Vec<&'static str> {
        let mut stripped = Vec::new();
        for _ in 0..count {
            if self.target.stability {
                self.target.stability = false;
                stripped.push("Stability");
            } else if self.target.protection {
                self.target.protection = false;
                stripped.push("Protection");
            } else {
                break;
            }
        }
        for boon in &stripped {
            self.status_trigger(TriggerRule::OnBoonStripped, boon, None, false);
        }
        stripped
    }

    fn avoids_attack(&mut self, unblockable: bool) -> bool {
        if self.has_defense(CoverKind::Invulnerability) || self.has_defense(CoverKind::Evade) {
            return true;
        }
        if !unblockable {
            if self.consume_defense(CoverKind::Aegis) {
                self.mark_charge_cover_consumed();
                return true;
            }
            if self.has_defense(CoverKind::Block) {
                return true;
            }
        }
        if !unblockable && self.consume_defense(CoverKind::Blind) {
            self.mark_charge_cover_consumed();
            return true;
        }
        false
    }

    fn mark_charge_cover_consumed(&mut self) {
        self.charge_cover_consumed_this_tick = true;
        if let Some(pending) = self.pending.as_mut() {
            pending.saved_by_charge = true;
        }
    }

    fn apply_incoming_confusion_on_skill_use(&mut self) {
        let stacks: u32 = self
            .incoming_conditions
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case("Confusion") && c.expires_at_ms > self.now_ms)
            .map(|c| c.stacks)
            .sum();
        if stacks == 0 {
            return;
        }
        let dmg = crate::data::conditions().confusion_tick(1_800.0, self.params.mode.clone(), true)
            * stacks as f64;
        self.absorb_damage(dmg);
    }

    fn absorb_damage(&mut self, damage: f64) {
        let mut remaining = damage;
        let mut absorbed = 0.0;
        while remaining > 0.0 {
            let Some(layer) = self.barrier.front_mut() else {
                break;
            };
            let take = layer.amount.min(remaining);
            layer.amount -= take;
            remaining -= take;
            absorbed += take;
            if layer.amount <= 0.0 {
                self.barrier.pop_front();
            }
        }
        self.barrier_absorbed += absorbed;
        if let Some(factor) = self
            .in_shroud
            .as_ref()
            .filter(|s| !s.health_exposed)
            .map(|s| s.damage_factor)
        {
            // In shroud the pool takes the (reduced) hit; what the pool
            // cannot cover overflows to health. Harbinger Shroud is the
            // exception: its health stays exposed and the pool only drains.
            let to_pool = remaining * factor;
            let pool = self.resources.entry(ResourceKind::LifeForce).or_default();
            let taken = to_pool.min(*pool);
            *pool -= taken;
            self.incoming_damage += taken;
            remaining = to_pool - taken;
            if *pool <= 0.0 {
                self.exit_shroud("life force 0");
            }
        }
        self.incoming_damage += remaining;
        self.player_health = (self.player_health - remaining).max(0.0);
    }

    fn apply_barrier(&mut self, amount: f64) {
        if amount <= 0.0 {
            return;
        }
        let cap = self.params.max_health * WVW_BARRIER_HEALTH_FRACTION;
        let current: f64 = self.barrier.iter().map(|layer| layer.amount).sum();
        let applied = amount.min((cap - current).max(0.0));
        if applied > 0.0 {
            self.barrier.push_back(BarrierLayer {
                amount: applied,
                expires_at_ms: self.now_ms.saturating_add(BARRIER_LIFETIME_MS),
            });
        }
    }

    fn tick_conditions(&mut self) {
        let incoming_mult = self.target.incoming_multiplier_at_tick(
            self.now_ms,
            &self.params.mode,
            &self.params.deferred_target,
        );
        let mut outgoing_damage = 0.0;
        let might = self.buff_stacks("Might") as f64;
        let condition_damage = self.params.condition_damage
            + might * crate::data::boon_condition_formulas::boons().might_condi_per_stack();
        let condi_mult = self.params.condition_mult * self.condition_conditional_mult();
        for condition in &mut self.target.conditions {
            if condition.next_tick_ms <= self.now_ms
                && condition.next_tick_ms <= condition.expires_at_ms
            {
                let tick =
                    condition_tick_damage(&condition.name, condition_damage, &self.params.mode)
                        * condition.stacks as f64
                        * condi_mult;
                outgoing_damage += tick;
                condition.next_tick_ms += 1_000;
            }
            if condition.expires_at_ms <= self.now_ms {
                let frac = leftover_condition_fraction(condition);
                if frac > 0.0 {
                    outgoing_damage +=
                        condition_tick_damage(&condition.name, condition_damage, &self.params.mode)
                            * condition.stacks as f64
                            * condi_mult
                            * frac;
                }
            }
        }
        if outgoing_damage > 0.0 {
            self.record_damage(outgoing_damage * incoming_mult, self.control_owned());
        }

        let mut incoming_damage = 0.0;
        for condition in &mut self.incoming_conditions {
            if condition.next_tick_ms <= self.now_ms
                && condition.next_tick_ms <= condition.expires_at_ms
            {
                incoming_damage +=
                    condition_tick_damage(&condition.name, 1_800.0, &self.params.mode)
                        * condition.stacks as f64;
                condition.next_tick_ms += 1_000;
            }
            if condition.expires_at_ms <= self.now_ms {
                let frac = leftover_condition_fraction(condition);
                if frac > 0.0 {
                    incoming_damage +=
                        condition_tick_damage(&condition.name, 1_800.0, &self.params.mode)
                            * condition.stacks as f64
                            * frac;
                }
            }
        }
        if incoming_damage > 0.0 {
            // wiki `Dark Aura` (read 2026-09-08): incoming condition damage
            // reduced by 20 %. Its torment-on-strike retaliation is not
            // modeled (the dummy has no incoming strike ledger for it).
            if self.has_buff("Dark Aura") {
                incoming_damage *= 0.8;
            }
            self.absorb_damage(incoming_damage);
        }
        self.target
            .conditions
            .retain(|condition| condition.expires_at_ms > self.now_ms);
        self.incoming_conditions
            .retain(|condition| condition.expires_at_ms > self.now_ms);
    }

    fn apply_skill_effect(&mut self, skill_id: u32, effect: &SkillEffect, protected: bool) {
        match effect {
            SkillEffect::StrikeDamage {
                hit_count,
                dmg_multiplier,
            } => {
                let might = self.buff_stacks("Might") as f64;
                let power = self.params.power
                    + might * crate::data::boon_condition_formulas::boons().might_power_per_stack();
                let fury_bonus = if self.has_buff("Fury") {
                    self.params.fury_crit_chance_bonus
                } else {
                    0.0
                };
                // Conditional bonuses (US3), evaluated at the strike.
                self.update_conditionals();
                let fury_bonus = fury_bonus + self.crit_chance_conditional_pct();
                let mut damage = self.params.weapon_strength * power / reference_armor()
                    * dmg_multiplier
                    * *hit_count as f64
                    * strike_crit_factor_with_crit_damage(
                        self.params.precision,
                        self.params.ferocity,
                        self.params.crit_chance_bonus + fury_bonus,
                        self.crit_damage_conditional_pct(),
                    )
                    * self.params.strike_mult;
                if self.target.protection {
                    damage *= self.protection_multiplier;
                }
                damage *= self
                    .target
                    .vulnerability_multiplier(self.now_ms, &self.params.mode);
                damage *= crate::combat::deferred_target_multiplier(
                    &self.params.deferred_target,
                    &self.target,
                    self.now_ms,
                    crate::combat::TargetModAxis::Strike,
                );
                damage *= self.strike_conditional_mult();
                self.record_damage(damage, protected);
                if self.trace_enabled {
                    let name = self.skill_name(skill_id);
                    self.trace(TraceKind::HitLanded, &name, format!("{damage:.1}"));
                }
                // Fight population (FR-003a): the same strike on every
                // secondary foe in range, counted.
                let targets = self.skill_targets(skill_id);
                if targets > 1 {
                    let name = self.skill_name(skill_id);
                    let extra = self.foe_fan_out(targets, &name) - 1;
                    if extra > 0 {
                        let cleave = damage * extra as f64;
                        self.cleave_damage += cleave;
                        self.record_damage(cleave, protected);
                    }
                }
                self.remove_defense(CoverKind::Stealth);
                self.gain_resource_on_hit(skill_id);
                self.gain_conditional_stacks(skill_id);
                self.trigger_procs(TriggerRule::OnHit, Some(skill_id), protected, 1.0);
                // On-crit procs (CONN-00-06): one chance per landed hit at the
                // crit chance the damage line above already priced in.
                let crit = crit_chance_fraction(
                    self.params.precision,
                    self.params.crit_chance_bonus + fury_bonus,
                );
                if crit > 0.0 {
                    for _ in 0..*hit_count {
                        self.trigger_procs(TriggerRule::OnCrit, Some(skill_id), protected, crit);
                    }
                }
            }
            SkillEffect::ApplyCondition {
                condition,
                stacks,
                duration_ms,
            } => {
                let duration =
                    (*duration_ms as f64 * self.params.condition_duration_mult).round() as u32;
                self.apply_outgoing_condition(
                    condition,
                    *stacks,
                    duration,
                    Some(skill_id),
                    protected,
                );
                self.count_condition_cleave(skill_id, *stacks, duration);
            }
            SkillEffect::ApplyBuff {
                buff,
                stacks,
                duration_ms,
            } if crate::data::boon_condition_formulas::conditions()
                .get(buff)
                .is_some() =>
            {
                // Sprint 3 (US2): the builder publishes a non-damaging
                // condition on a skill fact (Chilled, Crippled, Weakness,
                // Vulnerability, ...) as a Buff; on a foe-facing skill it is
                // an outgoing condition here, never a self-buff. PvE/PvP
                // keep the builder's shape (FR-009).
                let duration =
                    (*duration_ms as f64 * self.params.condition_duration_mult).round() as u32;
                self.apply_outgoing_condition(buff, *stacks, duration, Some(skill_id), protected);
                self.count_condition_cleave(skill_id, *stacks, duration);
            }
            SkillEffect::ApplyBuff {
                buff,
                stacks,
                duration_ms,
            } => {
                self.apply_buff(buff, *stacks, *duration_ms, true);
                let targets = self.skill_targets(skill_id);
                if targets > 1 {
                    let name = self.skill_name(skill_id);
                    let extra = self.ally_fan_out(targets, &name) - 1;
                    let seconds =
                        (*duration_ms as f64 * self.params.boon_duration_mult).round() / 1_000.0;
                    self.ally_boon_stack_seconds += extra as f64 * *stacks as f64 * seconds;
                }
            }
            SkillEffect::ComboField {
                field_type,
                duration_ms,
            } => {
                self.combo.place_field(
                    field_type,
                    *duration_ms,
                    self.now_ms,
                    super::combo::ComboSite::Ground,
                    super::combo::SELF_COMBATANT_ID,
                );
            }
            SkillEffect::ComboFinisher {
                finisher_type,
                percent,
            } => self.resolve_combo(skill_id, finisher_type, *percent),
            SkillEffect::Healing { hit_count } => {
                let amount = (1_200.0 + self.params.healing_power * 0.45)
                    * *hit_count as f64
                    * self.params.healing_mult;
                self.heal(amount);
                let targets = self.skill_targets(skill_id);
                if targets > 1 {
                    let name = self.skill_name(skill_id);
                    let extra = self.ally_fan_out(targets, &name) - 1;
                    self.ally_healing += extra as f64 * amount;
                }
            }
            SkillEffect::Barrier { amount } => {
                self.apply_barrier(amount + self.params.healing_power * 0.30);
            }
            SkillEffect::RemovesCondition { conditions_removed } => {
                self.cleanse(*conditions_removed);
                let targets = self.skill_targets(skill_id);
                if targets > 1 {
                    let name = self.skill_name(skill_id);
                    let extra = self.ally_fan_out(targets, &name) - 1;
                    self.ally_cleanses += extra * *conditions_removed;
                }
            }
            SkillEffect::CrowdControl {
                kind, duration_ms, ..
            } => {
                if !self.target.stability {
                    let added = land_foe_disable(
                        &mut self.target,
                        &mut self.trigger_bus,
                        self.now_ms,
                        *duration_ms,
                    );
                    self.control_landed_ms += added;
                    if added > 0 {
                        self.trigger_procs(TriggerRule::OnDisableFoe, Some(skill_id), false, 1.0);
                    }
                }
                // Wiki `Fear` (read 2026-09-08): "Fear is a condition ... Fear
                // counts as a control effect"; Taunt likewise. The disable above
                // stands; status triggers and cleanse counts see the condition (US2).
                if matches!(kind, super::ControlKind::Fear | super::ControlKind::Taunt) {
                    let name = format!("{kind:?}");
                    self.apply_outgoing_condition(
                        &name,
                        1,
                        *duration_ms,
                        Some(skill_id),
                        protected,
                    );
                }
            }
            SkillEffect::StripBoons {
                count_per_pulse,
                interval_ms,
                window_ms,
            } => {
                let _ = self.remove_enemy_boons(if *interval_ms == 0 {
                    // Zero interval = one immediate pulse, not a division.
                    *count_per_pulse
                } else {
                    *count_per_pulse
                        * ((*window_ms).max(*interval_ms))
                            .checked_div(*interval_ms)
                            .unwrap_or(0)
                });
            }
            SkillEffect::CorruptBoons => {
                for boon in self.remove_enemy_boons(1) {
                    if let Some(condition) = corrupt_into(boon) {
                        self.apply_outgoing_condition(
                            condition,
                            1,
                            1_000,
                            Some(skill_id),
                            protected,
                        );
                    }
                }
            }
            SkillEffect::StealBoons => {
                for boon in self.remove_enemy_boons(1) {
                    self.apply_buff(boon, 1, 1_000, true);
                }
            }
            SkillEffect::ConvertConditions => {
                let count = self.incoming_conditions.len() as u32;
                self.cleanse(count.max(1));
                self.apply_buff("Protection", 1, 3_000, true);
            }
            SkillEffect::Cover {
                kind,
                duration_ms,
                strippable,
            } => self.apply_defense(*kind, *duration_ms, 1, *strippable),
            // Mobility is a capability tag, not a duration source. Quantitative
            // cover must arrive as a mode-aware `Cover` fact.
            SkillEffect::Mobility { .. } => {}
        }
    }

    fn resolve_combo(&mut self, skill_id: u32, finisher_type: &str, percent: u32) {
        // Interrupted leap: effects only run on successful cast resolve, so the
        // existing cast/CC interrupt path already suppresses finishers. Pass
        // interrupted=false here; ComboEngine still honors the flag for tests.
        let Some(outcome) = self.combo.try_finisher(
            finisher_type,
            percent,
            self.now_ms,
            super::combo::SELF_COMBATANT_ID,
            super::combo::ComboSite::Ground,
            false,
        ) else {
            return;
        };
        self.apply_combo_outcome(skill_id, outcome);
    }

    fn apply_combo_outcome(&mut self, skill_id: u32, outcome: super::combo::ComboOutcome) {
        use super::combo::ComboOutcomeEffect;
        let scale = outcome.proc_scale;
        if scale <= 0.0 {
            return;
        }
        let field_name = outcome.field_type.clone();
        let finisher_type = outcome.finisher_type.clone();
        self.combo_activations += 1;
        match outcome.effect {
            ComboOutcomeEffect::Buff {
                name,
                stacks,
                duration_ms,
            } => {
                let duration = ((duration_ms as f64) * scale).round() as u32;
                if duration > 0 && stacks > 0 {
                    self.apply_buff(&name, stacks, duration, true);
                    let detail = format!("{field_name} field + {finisher_type} finisher -> {name}");
                    self.trace(TraceKind::ComboResolved, &self.skill_name(skill_id), detail);
                }
            }
            ComboOutcomeEffect::Condition {
                name,
                stacks,
                duration_ms,
            } => {
                let duration = ((duration_ms as f64) * scale * self.params.condition_duration_mult)
                    .round() as u32;
                if duration > 0 && stacks > 0 {
                    // Phase 3: foe conditions MUST use TargetState::apply_condition.
                    let cap =
                        crate::rotation::simulator::condition_stack_cap(&name, &self.params.mode);
                    self.target
                        .apply_condition(&name, stacks, duration, self.now_ms, cap as u32);
                    let detail = format!("{field_name} field + {finisher_type} finisher -> {name}");
                    self.trace(TraceKind::ComboResolved, &self.skill_name(skill_id), detail);
                }
            }
            ComboOutcomeEffect::Healing {
                base,
                healing_power_coef,
            } => {
                let amount = (base + self.params.healing_power * healing_power_coef) * scale;
                self.heal(amount);
                self.trace(
                    TraceKind::ComboResolved,
                    &self.skill_name(skill_id),
                    format!("{field_name} field + {finisher_type} finisher -> heal"),
                );
            }
            ComboOutcomeEffect::LifeSteal {
                damage_base,
                damage_power_coef,
                heal_base,
                heal_healing_power_coef,
            } => {
                let protected = self.control_owned();
                let damage = (damage_base + damage_power_coef * self.params.power) * scale;
                let healing =
                    (heal_base + heal_healing_power_coef * self.params.healing_power) * scale;
                self.record_damage(damage, protected);
                self.heal(healing);
                let name = self.skill_name(skill_id);
                self.trace(
                    TraceKind::ComboResolved,
                    &name,
                    format!("{field_name} field + {finisher_type} finisher -> leeching bolt"),
                );
            }
            ComboOutcomeEffect::ConditionCleanse { count } => {
                let n = ((count as f64) * scale).round() as u32;
                if n > 0 {
                    self.cleanse(n);
                    self.trace(
                        TraceKind::ComboResolved,
                        &self.skill_name(skill_id),
                        format!("{field_name} field + {finisher_type} finisher -> cleanse"),
                    );
                }
            }
            ComboOutcomeEffect::Cover { kind, duration_ms } => {
                let duration = ((duration_ms as f64) * scale).round() as u32;
                if duration > 0 {
                    self.apply_defense(kind, duration, 1, false);
                    self.trace(
                        TraceKind::ComboResolved,
                        &self.skill_name(skill_id),
                        format!("{field_name} field + {finisher_type} finisher -> cover"),
                    );
                }
            }
            ComboOutcomeEffect::CrowdControl { duration_ms } => {
                let duration = ((duration_ms as f64) * scale).round() as u32;
                if duration > 0 && !self.target.stability {
                    let added = land_foe_disable(
                        &mut self.target,
                        &mut self.trigger_bus,
                        self.now_ms,
                        duration,
                    );
                    self.control_landed_ms += added;
                    if added > 0 {
                        self.trigger_procs(TriggerRule::OnDisableFoe, Some(skill_id), false, 1.0);
                    }
                    self.trace(
                        TraceKind::ComboResolved,
                        &self.skill_name(skill_id),
                        format!("{field_name} field + {finisher_type} finisher -> daze"),
                    );
                }
            }
            ComboOutcomeEffect::Unmodeled { reason } => {
                // Still counts as an activation attempt against a live field.
                self.note_unmodeled(reason);
            }
        }
    }

    fn apply_buff(&mut self, name: &str, stacks: u32, duration_ms: u32, scale_duration: bool) {
        let duration = if scale_duration {
            (duration_ms as f64 * self.params.boon_duration_mult).round() as u32
        } else {
            duration_ms
        };
        // Wiki `Effect stacking`: most boons cap at 30 s remaining, Swiftness
        // at 60 s, Might/Aegis/Regeneration uncapped — data/formulas/boons.json.
        let duration = match crate::data::boons().get(name).and_then(|b| b.max_duration) {
            Some(cap_s) => duration.min(cap_s * 1_000),
            None => duration,
        };
        self.buffs.push(TimedBuff {
            name: name.into(),
            stacks,
            expires_at_ms: self.at(duration),
        });
        if let Some(kind) = boon_cover_kind(name) {
            self.apply_defense(kind, duration, stacks, true);
        }
        self.status_trigger(TriggerRule::OnBoonApplied, name, None, false);
        self.status_trigger(TriggerRule::OnBoonGained { boon: None }, name, None, false);
    }

    fn apply_defense(&mut self, kind: CoverKind, duration_ms: u32, stacks: u32, strippable: bool) {
        if let Some(existing) = self
            .defenses
            .iter_mut()
            .find(|defense| defense.kind == kind)
        {
            existing.expires_at_ms = existing
                .expires_at_ms
                .max(self.now_ms.saturating_add(duration_ms));
            existing.stacks = existing.stacks.max(stacks);
            existing.strippable &= strippable;
        } else {
            self.defenses.push(TimedDefense {
                kind,
                expires_at_ms: self.at(duration_ms),
                stacks,
                strippable,
                applied_at_ms: self.now_ms,
            });
        }
    }

    /// `scale` is the expected-value weight of the firing (1.0 for a certain
    /// proc). Stacks and counts are whole numbers in the game, so the scaled
    /// amount rounds to nearest and a rounding to zero applies nothing.
    // ponytail: fractional boon stacks would need a fractional ledger; the
    // rounding is the approximation the trace's trials measure against.
    fn apply_operation(
        &mut self,
        operation: Option<&crate::data::normalized_effects::StatusOperation>,
        scale: f64,
        source: &str,
    ) {
        let Some(operation) = operation else {
            return;
        };
        let targets = operation
            .target_count
            .as_ref()
            .and_then(resolved)
            .copied()
            .unwrap_or(1);
        let amount = (resolved(&operation.amount_value)
            .copied()
            .unwrap_or(1.0)
            .max(1.0)
            * scale)
            .round() as u32;
        if amount == 0 {
            return;
        }
        let duration = operation
            .base_duration_ms
            .as_ref()
            .and_then(resolved)
            .copied()
            .unwrap_or(1_000);
        match (&operation.operation_type, &operation.target_side) {
            (OperationType::AppliesBoon, TargetSide::Self_) => {
                self.apply_buff(&operation.status_kind, amount, duration, true)
            }
            (OperationType::AppliesBoon, TargetSide::Ally) => {
                self.apply_buff(&operation.status_kind, amount, duration, true);
                let extra = self.ally_fan_out(targets, source) - 1;
                let seconds = (duration as f64 * self.params.boon_duration_mult).round() / 1_000.0;
                self.ally_boon_stack_seconds += extra as f64 * amount as f64 * seconds;
            }
            (OperationType::AppliesCondition, TargetSide::Enemy) => {
                let name = operation.status_kind.clone();
                self.apply_outgoing_condition(&name, amount, duration, None, false);
                let extra = self.foe_fan_out(targets, source) - 1;
                self.cleave_condition_stack_seconds +=
                    extra as f64 * amount as f64 * duration as f64 / 1_000.0;
            }
            (OperationType::RemovesCondition, TargetSide::Self_ | TargetSide::Ally)
            | (OperationType::ConvertsConditionToBoon, TargetSide::Self_ | TargetSide::Ally) => {
                self.cleanse(amount);
                if matches!(operation.target_side, TargetSide::Ally) {
                    let extra = self.ally_fan_out(targets, source) - 1;
                    self.ally_cleanses += extra * amount;
                }
            }
            (OperationType::RemovesBoon | OperationType::CorruptsBoon, TargetSide::Enemy) => {
                // Both flags, as before; each stripped boon is a firing site.
                self.remove_enemy_boons(2);
            }
            _ => {}
        }
    }

    fn skill_directly_models_effect(&self, effect: &NormalizedEffect) -> bool {
        let Some(skill) = self
            .skills
            .iter()
            .find(|skill| skill.skill_id == effect.source_id)
        else {
            return false;
        };
        match effect.category {
            EffectCategory::RemovesCondition => skill
                .effects
                .iter()
                .any(|item| matches!(item, SkillEffect::RemovesCondition { .. })),
            _ => false,
        }
    }

    fn at(&self, offset_ms: u32) -> u32 {
        self.now_ms.saturating_add(offset_ms)
    }

    /// How many foes a foe-facing effect with `n` targets reaches on this
    /// scale (the primary included), traced when more than one.
    fn foe_fan_out(&mut self, n: u32, source: &str) -> u32 {
        let applied = n.max(1).min(self.population.foes.max(1));
        if applied > 1 {
            let foes = self.population.foes;
            self.trace(
                TraceKind::PopulationApplied,
                source,
                format!("{applied} of {foes} foes ({n})"),
            );
        }
        applied
    }

    /// How many people an ally-facing effect with `n` targets reaches (the
    /// player included), traced when more than one.
    fn ally_fan_out(&mut self, n: u32, source: &str) -> u32 {
        let applied = 1 + (n.max(1) - 1).min(self.population.allies);
        if applied > 1 {
            self.trace(
                TraceKind::PopulationApplied,
                source,
                format!("{applied} of allies ({n})"),
            );
        }
        applied
    }

    fn skill_targets(&self, skill_id: u32) -> u32 {
        self.skills
            .iter()
            .find(|s| s.skill_id == skill_id)
            .map(|s| s.targets)
            .unwrap_or(1)
    }

    /// A skill fact's condition on every secondary foe in range, counted.
    fn count_condition_cleave(&mut self, skill_id: u32, stacks: u32, duration_ms: u32) {
        let targets = self.skill_targets(skill_id);
        if targets > 1 {
            let name = self.skill_name(skill_id);
            let extra = self.foe_fan_out(targets, &name) - 1;
            self.cleave_condition_stack_seconds +=
                extra as f64 * stacks as f64 * duration_ms as f64 / 1_000.0;
        }
    }

    /// The one site every outgoing condition passes through (US2): push it,
    /// then let `OnConditionApplied` records see its name.
    fn apply_outgoing_condition(
        &mut self,
        name: &str,
        stacks: u32,
        duration_ms: u32,
        source_skill: Option<u32>,
        protected: bool,
    ) {
        let cap = crate::rotation::simulator::condition_stack_cap(name, &self.params.mode) as u32;
        // `apply_condition` clamps to `cap` for the ledger; the trigger fires on
        // every application because GW2 on-apply effects also fire on a refresh
        // (Vulnerability at 25, or any max_stacks-1 control already running).
        self.target
            .apply_condition(name, stacks, duration_ms, self.now_ms, cap);
        self.status_trigger(
            TriggerRule::OnConditionApplied,
            name,
            source_skill,
            protected,
        );
    }

    /// Run a status trigger with `name` visible to `TriggerScope::Status`;
    /// never nested, so a record that applies a status on a status cannot
    /// feed itself.
    fn status_trigger(
        &mut self,
        trigger: TriggerRule,
        name: &str,
        source_skill: Option<u32>,
        protected: bool,
    ) {
        if self.status_trigger_depth > 0 {
            return;
        }
        self.status_trigger_depth += 1;
        self.trigger_status = Some(name.to_string());
        self.trigger_procs(trigger, source_skill, protected, 1.0);
        self.trigger_status = None;
        self.status_trigger_depth -= 1;
    }

    /// Credit `percent` of the life force pool (trait records, US2).
    fn gain_life_force_percent(&mut self, percent: f64, source: &str) {
        if percent <= 0.0 {
            return;
        }
        let cap = resource_cap(ResourceKind::LifeForce, self.params.max_health).max(1.0);
        let pool = self.resources.entry(ResourceKind::LifeForce).or_default();
        *pool = (*pool + cap * percent / 100.0).min(cap);
        let after = *pool / cap * 100.0;
        self.trace(
            TraceKind::LifeForceGained,
            source,
            format!("{percent:.0}% → {after:.0}%"),
        );
    }

    fn note_unmodeled_proc(&mut self, source_type: &SourceType, source_id: u32, name: &str) {
        let tag = match source_type {
            SourceType::Trait => 0,
            SourceType::Skill => 1,
            SourceType::Rune => 2,
            SourceType::Sigil => 3,
            SourceType::Relic => 4,
        };
        // Deduplicated by source key, not by name: two sources may share a
        // name and still be two lines.
        if self.unmodeled_proc_keys.insert((tag, source_id)) {
            self.unmodeled_names
                .push(format!("{name} (unsupported proc)"));
            self.trace(TraceKind::ProcUnmodeled, name, "unsupported proc");
        }
    }

    /// Record one source the timeline is not simulating. Deduplicated by the
    /// full `"{name} ({why})"` string, so a combo that resolves every cast
    /// or a proc that is skipped every hit is one line, not one per tick.
    fn note_unmodeled(&mut self, name: String) {
        if !self.unmodeled_names.contains(&name) {
            self.unmodeled_names.push(name);
        }
    }

    /// Append one trace event when tracing is on; the 513th sets
    /// `trace_truncated` and is dropped.
    fn trace(&mut self, kind: TraceKind, source: &str, detail: impl Into<String>) {
        #[cfg(test)]
        TRACE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
        if !self.trace_enabled {
            return;
        }
        if self.trace.len() >= self.trace_cap {
            self.trace_truncated = true;
            return;
        }
        self.trace.push(TraceEvent {
            t_ms: self.now_ms,
            kind,
            source: source.into(),
            detail: detail.into(),
        });
    }

    /// Re-read every threshold against the regular health pool and expire
    /// stacks, tracing each state change (US3).
    fn update_conditionals(&mut self) {
        let ratio = self.player_health / self.params.max_health.max(1.0);
        let now = self.now_ms;
        let in_shroud = self.in_shroud.is_some();
        let prerequisite_holds = PrerequisiteView::of(self);
        let mut changes = Vec::new();
        for spec in &mut self.conditional_specs {
            match spec.kind {
                ConditionalKind::Threshold { above, percent } => {
                    let holds = if above {
                        ratio > percent / 100.0
                    } else {
                        ratio < percent / 100.0
                    };
                    if holds != spec.active {
                        spec.active = holds;
                        changes.push((
                            spec.source_name.clone(),
                            holds,
                            format!("health {:.0}%", ratio * 100.0),
                        ));
                    }
                }
                ConditionalKind::Stacking { .. } => {
                    let before = spec.stacks;
                    spec.stack_expiries.retain(|at| *at > now);
                    spec.stacks = spec.stack_expiries.len() as u32;
                    spec.expires_at_ms = spec.stack_expiries.iter().copied().max().unwrap_or(0);
                    if spec.stacks < before {
                        changes.push((
                            spec.source_name.clone(),
                            spec.stacks > 0,
                            format!("{} of {before} stacks expired", before - spec.stacks),
                        ));
                    }
                }
                ConditionalKind::Prerequisite(ref prerequisite) => {
                    let holds = prerequisite_holds.is_ok_with(prerequisite);
                    if holds != spec.active {
                        spec.active = holds;
                        changes.push((
                            spec.source_name.clone(),
                            holds,
                            if holds {
                                "prerequisite holds".to_string()
                            } else {
                                "prerequisite lapsed".to_string()
                            },
                        ));
                    }
                }
                ConditionalKind::PerFoeStack { ref condition, max } => {
                    let stacks = prerequisite_holds.foe_stacks(condition).min(max);
                    let holds = stacks > 0;
                    spec.stacks = stacks;
                    if holds != spec.active {
                        spec.active = holds;
                        changes.push((
                            spec.source_name.clone(),
                            holds,
                            format!("{stacks} stacks of {condition}"),
                        ));
                    }
                }
                ConditionalKind::Timed { until_ms } => {
                    let holds = now < until_ms;
                    if holds != spec.active {
                        spec.active = holds;
                        changes.push((
                            spec.source_name.clone(),
                            holds,
                            if holds {
                                "timed bonus on".to_string()
                            } else {
                                "timed bonus expired".to_string()
                            },
                        ));
                    }
                }
                ConditionalKind::InShroud => {
                    let holds = in_shroud;
                    if holds != spec.active {
                        spec.active = holds;
                        changes.push((
                            spec.source_name.clone(),
                            holds,
                            format!(
                                "×{:.2} {}",
                                1.0 + spec.percent / 100.0,
                                if spec.crit_damage {
                                    "crit damage"
                                } else {
                                    "strike"
                                }
                            ),
                        ));
                    }
                }
            }
        }
        for (name, on, detail) in changes {
            let shroud = detail.starts_with('×');
            let kind = match (shroud, on) {
                (true, true) => TraceKind::ShroudBonusActive,
                (true, false) => TraceKind::ShroudBonusEnded,
                (false, true) => TraceKind::ConditionalActivated,
                (false, false) => TraceKind::ConditionalExpired,
            };
            self.trace(kind, &name, detail);
        }
    }

    /// The strike multiplier of every conditional bonus that holds now.
    fn strike_conditional_mult(&self) -> f64 {
        self.conditional_specs
            .iter()
            .filter(|spec| !spec.crit_damage && !spec.crit_chance && !spec.condition_damage)
            .map(|spec| match spec.kind {
                ConditionalKind::Threshold { .. }
                | ConditionalKind::InShroud
                | ConditionalKind::Prerequisite(_)
                | ConditionalKind::Timed { .. }
                    if spec.active =>
                {
                    1.0 + spec.percent / 100.0
                }
                ConditionalKind::Stacking { .. } | ConditionalKind::PerFoeStack { .. } => {
                    1.0 + spec.stacks as f64 * spec.percent / 100.0
                }
                _ => 1.0,
            })
            .product()
    }

    /// The condition-damage multiplier of every conditional bonus that holds now.
    fn condition_conditional_mult(&self) -> f64 {
        self.conditional_specs
            .iter()
            .filter(|spec| spec.condition_damage)
            .map(|spec| match spec.kind {
                ConditionalKind::Timed { .. } if spec.active => 1.0 + spec.percent / 100.0,
                ConditionalKind::Stacking { .. } => 1.0 + spec.stacks as f64 * spec.percent / 100.0,
                _ => 1.0,
            })
            .product()
    }

    /// Critical chance percentage points of every conditional bonus that
    /// holds now (Sprint 3: Decimate Defenses per vulnerability stack).
    fn crit_chance_conditional_pct(&self) -> f64 {
        self.conditional_specs
            .iter()
            .filter(|spec| spec.crit_chance && spec.active)
            .map(|spec| match spec.kind {
                ConditionalKind::PerFoeStack { .. } | ConditionalKind::Stacking { .. } => {
                    spec.stacks as f64 * spec.percent
                }
                _ => spec.percent,
            })
            .sum()
    }

    /// Critical damage percentage points of every conditional bonus that
    /// holds now (Sprint 3: the in-shroud half of Death Perception).
    fn crit_damage_conditional_pct(&self) -> f64 {
        self.conditional_specs
            .iter()
            .filter(|spec| spec.crit_damage && spec.active)
            .map(|spec| match spec.kind {
                ConditionalKind::PerFoeStack { .. } => spec.stacks as f64 * spec.percent,
                _ => spec.percent,
            })
            .sum()
    }

    /// Whether a record's prerequisite holds now; `Err` is formatted into the
    /// `ProcSkippedPrerequisite` trace only when tracing is on.
    fn prerequisite_holds(&self, prerequisite: &Prerequisite) -> Result<(), PrerequisiteFail> {
        if let Some(want) = prerequisite.in_shroud {
            if self.in_shroud.is_some() != want {
                return Err(if want {
                    PrerequisiteFail::NotInShroud
                } else {
                    PrerequisiteFail::InShroud
                });
            }
        }
        if let Some(condition) = &prerequisite.foe_condition {
            let carried = self.target.conditions.iter().any(|c| {
                foe_condition_name_eq(&c.name, condition) && c.expires_at_ms > self.now_ms
            });
            if !carried {
                return Err(PrerequisiteFail::FoeNotCondition);
            }
        }
        if let Some(gate) = &prerequisite.foe_health {
            // ponytail: an open dummy has no bar, so a foe-health gate never
            // holds there; the summary trace says so at the end.
            let Some(target) = self.profile.target_health.filter(|h| *h > 0.0) else {
                return Err(PrerequisiteFail::FoeHealthUnknown);
            };
            let Some(&percent) = resolved(&gate.percent) else {
                return Err(PrerequisiteFail::FoeHealthUnresolved);
            };
            let ratio = self.enemy_hp() / target;
            let holds = if gate.above {
                ratio > percent / 100.0
            } else {
                ratio < percent / 100.0
            };
            if !holds {
                return Err(PrerequisiteFail::FoeHealth);
            }
        }
        if let Some(want) = &prerequisite.attunement {
            let Some(element) = Element::parse(want) else {
                return Err(PrerequisiteFail::UnknownAttunement);
            };
            if !self.attunement.is_attuned(element) {
                return Err(PrerequisiteFail::NotAttuned);
            }
        }
        Ok(())
    }

    /// Every gate on a record must hold at the trigger. Read-only on purpose:
    /// a gate that keeps state (an interval slot, a threshold latch) only
    /// advances in [`Self::commit_gates`], once the record has actually fired,
    /// so a refusal elsewhere does not burn the slot.
    fn gates_hold(&self, gates: &[GateState], held_set: u8) -> Result<(), String> {
        for state in gates {
            match &state.gate {
                Gate::InCombat => {
                    if self.combat_started_ms.is_none() {
                        return Err("out of combat".to_string());
                    }
                }
                Gate::Interval { while_state, .. } => {
                    if self.now_ms < state.next_ms {
                        return Err(format!("interval not due until {} ms", state.next_ms));
                    }
                    if let Some(want) = while_state {
                        if let Err(reason) = self.prerequisite_holds(want) {
                            return Err(reason.detail(want));
                        }
                    }
                }
                Gate::Weapon { types, hand } => {
                    let wanted: Vec<String> = types
                        .iter()
                        .map(|t| gw2_core::i18n::weapon_type_key(t))
                        .collect();
                    let held = self.equipped_weapons.iter().any(|w| {
                        w.set == held_set
                            && hand.is_none_or(|want| w.hand == want)
                            && wanted.contains(&w.weapon_type)
                    });
                    if !held {
                        return Err(format!("no {} on the held set", types.join("/")));
                    }
                }
                // Abstentions, not passes: `unexecutable_reason` keeps these
                // records off the proc list, and this arm is the backstop.
                Gate::Positional(side) => {
                    return Err(format!("gate not yet modelled: {side:?} facing"))
                }
                Gate::Proximity { .. } => {
                    return Err("gate not yet modelled: foe distance".to_string())
                }
                Gate::HealthThreshold {
                    below_pct,
                    above_pct,
                    rearm,
                } => {
                    if state.latched && !matches!(rearm, Rearm::Icd) {
                        return Err("threshold already fired".to_string());
                    }
                    let pct = 100.0 * self.player_health / self.params.max_health.max(1.0);
                    if !health_in_band(pct, *below_pct, *above_pct) {
                        return Err(format!("health {pct:.0}% outside the gate"));
                    }
                }
                Gate::SelfBoon { boon } => {
                    let carried = self.buffs.iter().any(|b| {
                        b.name.eq_ignore_ascii_case(boon) && b.expires_at_ms > self.now_ms
                    });
                    if !carried {
                        return Err(format!("no {boon}"));
                    }
                }
                Gate::SelfBoonAbsent { boon } => {
                    let carried = self.buffs.iter().any(|b| {
                        b.name.eq_ignore_ascii_case(boon) && b.expires_at_ms > self.now_ms
                    });
                    if carried {
                        return Err(format!("already has {boon}"));
                    }
                }
                Gate::SelfResourceStacks { resource, min } => {
                    let Some(kind) = resource_kind_by_name(resource) else {
                        return Err(format!("resource not yet modelled: {resource}"));
                    };
                    if self.resources.get(&kind).copied().unwrap_or(0.0) < f64::from(*min) {
                        return Err(format!("under {min} {resource}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Advance the state of every gate a firing consumed.
    fn commit_gates(&mut self, idx: usize) {
        let now = self.now_ms;
        for state in self.proc_specs[idx].gates.iter_mut() {
            match &state.gate {
                Gate::Interval { every_ms, .. } => {
                    state.next_ms = now.saturating_add(*every_ms);
                }
                // `Icd` leaves the record's own cooldown as the only limit.
                Gate::HealthThreshold { rearm, .. } => {
                    state.latched = !matches!(rearm, Rearm::Icd);
                }
                _ => {}
            }
        }
    }

    /// `Rearm::WhenRecovered`: a latched health gate re-arms once health leaves
    /// the band it fired in. Without this a gate that fires on every crossing is
    /// indistinguishable from one that latches for the fight.
    fn rearm_health_gates(&mut self) {
        let pct = 100.0 * self.player_health / self.params.max_health.max(1.0);
        for spec in self.proc_specs.iter_mut() {
            for state in spec.gates.iter_mut() {
                let Gate::HealthThreshold {
                    below_pct,
                    above_pct,
                    rearm: Rearm::WhenRecovered,
                } = &state.gate
                else {
                    continue;
                };
                if state.latched && !health_in_band(pct, *below_pct, *above_pct) {
                    state.latched = false;
                }
            }
        }
    }

    /// A record's `value` plus what the live state adds at firing time.
    fn scaled_value(&self, value: f64, scale: Option<&Scale>) -> f64 {
        let (step, cap, n) = match scale {
            None => return value,
            // Never loaded: `unexecutable_reason` abstains on it.
            Some(Scale::PerDistance { .. }) => return value,
            Some(Scale::PerSelfResourceStack {
                resource,
                per_stack,
                cap,
            }) => {
                let held = resource_kind_by_name(resource)
                    .and_then(|kind| self.resources.get(&kind).copied())
                    .unwrap_or(0.0);
                (*per_stack, *cap, held)
            }
            Some(Scale::PerSelfBoon {
                boon,
                per_stack,
                cap,
            }) => {
                let held: f64 = self
                    .buffs
                    .iter()
                    .filter(|b| b.name.eq_ignore_ascii_case(boon) && b.expires_at_ms > self.now_ms)
                    .map(|b| f64::from(b.stacks))
                    .sum();
                (*per_stack, *cap, held)
            }
        };
        value + step * cap.map_or(n, |c| n.min(c))
    }

    /// End of fight: a shroud record that never fired because no shroud was
    /// entered goes on the coverage line with that reason (spec edge case).
    fn note_never_fired(&mut self) {
        let never_met: Vec<String> = self
            .prerequisite_refused
            .iter()
            .filter(|name| !self.proc_fire_counts.contains_key(*name))
            .cloned()
            .collect();
        for name in never_met {
            self.trace(
                TraceKind::ProcSkippedPrerequisite,
                &name,
                "prerequisite never met",
            );
        }
        if self.shroud_entered_once {
            return;
        }
        let names: Vec<String> = self
            .proc_specs
            .iter()
            .filter(|spec| {
                matches!(
                    spec.trigger,
                    TriggerRule::OnShroudEnter | TriggerRule::OnShroudExit
                ) && !self.proc_fire_counts.contains_key(&spec.source_name)
            })
            .map(|spec| spec.source_name.clone())
            .collect();
        for name in names {
            self.note_unmodeled(format!("{name} (shroud never entered)"));
        }
    }

    /// Whether `skill_id` counts for a trigger scope.
    fn scope_admits(
        &self,
        scope: &crate::data::normalized_effects::TriggerScope,
        skill_id: Option<u32>,
    ) -> bool {
        match scope {
            crate::data::normalized_effects::TriggerScope::Any => true,
            crate::data::normalized_effects::TriggerScope::Status(status) => self
                .trigger_status
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(status)),
            _ => skill_id
                .and_then(|id| self.skills.iter().find(|s| s.skill_id == id))
                .is_some_and(|s| skill_scope_admits(scope, s, self.skills)),
        }
    }

    /// A landed hit from `skill_id` feeds every stacking bonus in scope.
    fn gain_conditional_stacks(&mut self, skill_id: u32) {
        let now = self.now_ms;
        let mut gained = Vec::new();
        for idx in 0..self.conditional_specs.len() {
            let ConditionalKind::Stacking {
                max,
                duration_ms,
                ref scope,
                hit_fed,
            } = self.conditional_specs[idx].kind
            else {
                continue;
            };
            // Spawn-only stackers (723 Compounding Power) are not hit-fed.
            if !hit_fed {
                continue;
            }
            if !self.scope_admits(scope, Some(skill_id)) {
                continue;
            }
            let spec = &mut self.conditional_specs[idx];
            gain_stack(spec, now, duration_ms, max);
            gained.push((spec.source_name.clone(), format!("{}/{max}", spec.stacks)));
        }
        for (name, detail) in gained {
            self.trace(TraceKind::StackGained, &name, detail);
        }
    }

    /// Tag each sigil's procs with the weapon set it is socketed on.
    fn assign_sigil_sets(&mut self, sigil_sets: &HashMap<u32, u8>) {
        for spec in &mut self.proc_specs {
            if matches!(spec.source_type, SourceType::Sigil) {
                spec.weapon_set = sigil_sets.get(&spec.source_id).copied().unwrap_or(0);
            }
        }
    }

    /// The set a hit from `skill_id` was cast on: a weapon skill's own set,
    /// which is also right for a channel that finishes after a swap; the
    /// held set for everything else.
    fn held_set_for(&self, skill_id: Option<u32>) -> u8 {
        skill_id
            .and_then(|id| self.skills.iter().find(|s| s.skill_id == id))
            .map(|s| s.weapon_set)
            .filter(|set| matches!(set, 1 | 2))
            .unwrap_or(if self.active_weapon_set == super::SHROUD_SET {
                // Sigils on the stowed weapon keep working in shroud.
                self.weapon_set_before_shroud
            } else {
                self.active_weapon_set
            })
    }

    /// The sources `load_normalized_effects` could not model are known before
    /// tracing is switched on; replay them as events once it is.
    fn trace_loaded_unmodeled(&mut self) {
        if !self.trace_enabled {
            return;
        }
        for name in self.unmodeled_names.clone() {
            self.trace(TraceKind::ProcUnmodeled, &name, "no firing site");
        }
        for entry in self.no_record_entries.clone() {
            self.trace(
                TraceKind::ProcUnmodeled,
                &entry.rendered(),
                entry.class.suffix(),
            );
        }
    }

    fn skill_name(&self, skill_id: u32) -> String {
        self.skills
            .iter()
            .find(|skill| skill.skill_id == skill_id)
            .map(|skill| skill.name.clone())
            .unwrap_or_else(|| format!("skill {skill_id}"))
    }

    /// `weight` is the probability that this event is the proc's trigger:
    /// 1.0 for a landed hit or a skill use, the critical chance for the
    /// on-crit call. Expected-value mode applies `weight × proc_chance` of
    /// the effect and starts the cooldown once a whole proc's worth of mass
    /// has accumulated (R1); seeded mode draws.
    fn trigger_procs(
        &mut self,
        trigger: TriggerRule,
        activating_skill_id: Option<u32>,
        protected: bool,
        weight: f64,
    ) {
        let mut ready = Vec::new();
        let mut on_cooldown = Vec::new();
        let mut skipped_prerequisite = Vec::new();
        let mut skipped_gate: Vec<(usize, String)> = Vec::new();
        let held_set = self.held_set_for(activating_skill_id);
        for (idx, proc_spec) in self.proc_specs.iter().enumerate() {
            let source_matches = !matches!(proc_spec.source_type, SourceType::Skill)
                || activating_skill_id == Some(proc_spec.source_id);
            let set_held = proc_spec.weapon_set == 0 || proc_spec.weapon_set == held_set;
            let scope_ok = self.scope_admits(&proc_spec.scope, activating_skill_id);
            // `OnBoonGained { boon }` narrows which boon fires it; the
            // emitting site names the boon in `trigger_status`.
            let boon_ok = match &proc_spec.trigger {
                TriggerRule::OnBoonGained { boon: Some(want) } => self
                    .trigger_status
                    .as_deref()
                    .is_some_and(|got| got.eq_ignore_ascii_case(want)),
                _ => true,
            };
            if !same_trigger(&proc_spec.trigger, &trigger)
                || !source_matches
                || !set_held
                || !scope_ok
                || !boon_ok
            {
                continue;
            }
            if proc_spec.next_ready_ms > self.now_ms {
                if self.trace_enabled {
                    let prereq_ok = proc_spec
                        .prerequisite
                        .as_ref()
                        .is_none_or(|p| self.prerequisite_holds(p).is_ok());
                    if prereq_ok {
                        on_cooldown.push(idx);
                    }
                }
                continue;
            }
            if let Err(reason) = proc_spec
                .prerequisite
                .as_ref()
                .map_or(Ok(()), |p| self.prerequisite_holds(p))
            {
                skipped_prerequisite.push((idx, reason));
            } else if let Err(why) = self.gates_hold(&proc_spec.gates, held_set) {
                skipped_gate.push((idx, why));
            } else {
                ready.push(idx);
            }
        }
        for (idx, why) in skipped_gate {
            // Same rhythm as a refused prerequisite: a periodic record
            // re-checks at its next interval, and the trace carries one
            // line per change of reason.
            if matches!(trigger, TriggerRule::Periodic) {
                let spec = &mut self.proc_specs[idx];
                spec.next_ready_ms = self
                    .now_ms
                    .saturating_add(spec.internal_cooldown_ms.max(TIMELINE_TICK_MS));
            }
            if !self.trace_enabled {
                continue;
            }
            let name = self.proc_specs[idx].source_name.clone();
            self.prerequisite_refused.insert(name.clone());
            if self.last_prerequisite_skip.get(&name) != Some(&why) {
                self.last_prerequisite_skip
                    .insert(name.clone(), why.clone());
                self.trace(TraceKind::ProcSkippedPrerequisite, &name, why);
            }
        }
        for (idx, reason) in skipped_prerequisite {
            // A periodic record re-checks its prerequisite at its next
            // interval, not every tick (Shrouded Removal: every 3 s while in
            // shroud), so the refusal is one trace per period.
            if matches!(trigger, TriggerRule::Periodic) {
                let spec = &mut self.proc_specs[idx];
                spec.next_ready_ms = self
                    .now_ms
                    .saturating_add(spec.internal_cooldown_ms.max(TIMELINE_TICK_MS));
            }
            if !self.trace_enabled {
                continue;
            }
            let name = self.proc_specs[idx].source_name.clone();
            let reason = self.proc_specs[idx]
                .prerequisite
                .as_ref()
                .map(|p| reason.detail(p))
                .unwrap_or_default();
            self.prerequisite_refused.insert(name.clone());
            // One trace per reason change, not one per hit: the refusal is a
            // state the reader needs once, until the record fires or the
            // reason moves (`foe not Chilled` -> `not in shroud`).
            if self.last_prerequisite_skip.get(&name) != Some(&reason) {
                self.last_prerequisite_skip
                    .insert(name.clone(), reason.clone());
                self.trace(TraceKind::ProcSkippedPrerequisite, &name, reason);
            }
        }
        if self.trace_enabled {
            for idx in on_cooldown {
                let name = self.proc_specs[idx].source_name.clone();
                let ready_at = self.proc_specs[idx].next_ready_ms;
                self.trace(
                    TraceKind::ProcSkippedIcd,
                    &name,
                    format!("ready at {ready_at} ms"),
                );
            }
        }
        let on_crit = matches!(trigger, TriggerRule::OnCrit);
        let cleansed_before = self.conditions_cleansed;
        for idx in ready {
            let chance = weight * self.proc_specs[idx].proc_chance;
            let p = match (&mut self.crit_mode, on_crit) {
                (CritMode::Seeded(rng), true) => {
                    if rng.next_f64() < chance {
                        1.0
                    } else {
                        0.0
                    }
                }
                _ => chance,
            };
            if p <= 0.0 {
                continue;
            }
            let scale = self.proc_specs[idx].scale.clone();
            let scaled = self.scaled_value(self.proc_specs[idx].value, scale.as_ref());
            let (category, value, duration_ms, operation, name, cast_skill_id) = {
                let proc_spec = &mut self.proc_specs[idx];
                proc_spec.mass += p;
                if proc_spec.mass >= 1.0 - 1e-9 {
                    proc_spec.mass -= 1.0;
                    proc_spec.next_ready_ms =
                        self.now_ms.saturating_add(proc_spec.internal_cooldown_ms);
                }
                (
                    proc_spec.category.clone(),
                    scaled,
                    proc_spec.duration_ms,
                    proc_spec.operation.clone(),
                    proc_spec.source_name.clone(),
                    proc_spec.cast_skill_id,
                )
            };
            self.commit_gates(idx);
            // E2 cast scheduler: apply lesser SkillEffects; do not emit OnElite
            // here — elite already emitted at cast start. CrowdControl inside
            // the lesser still goes through land_foe_disable.
            let fired = if let Some(cast_id) = cast_skill_id {
                if let Some(effects) = resolve_trait_skill(cast_id, &self.cast_skill_catalog) {
                    let effects = effects.to_vec();
                    for effect in &effects {
                        self.apply_skill_effect(cast_id, effect, protected);
                    }
                    !effects.is_empty()
                } else {
                    self.note_unmodeled(format!("{name} (trait skill {cast_id} unresolved)"));
                    false
                }
            } else {
                match category {
                    EffectCategory::SpawnClone => {
                        // E4: payload calls spawn_clone; emit+OnCloneCreated procs only on rise.
                        let rose =
                            spawn_clone(&mut self.illusion, &mut self.trigger_bus, self.now_ms);
                        if rose {
                            self.trigger_procs(
                                TriggerRule::OnCloneCreated,
                                activating_skill_id,
                                protected,
                                1.0,
                            );
                        }
                        rose
                    }
                    EffectCategory::StrikeDamagePct | EffectCategory::ConditionDamagePct
                        if duration_ms > 0 && self.proc_specs[idx].max_stacks > 0 =>
                    {
                        // E4 Compounding Power: stacking buff on successful clone create.
                        let max = self.proc_specs[idx].max_stacks;
                        let is_condi = matches!(category, EffectCategory::ConditionDamagePct);
                        let until = self.now_ms.saturating_add(duration_ms);
                        match self.conditional_specs.iter_mut().find(|spec| {
                            spec.source_name == name && spec.condition_damage == is_condi
                        }) {
                            Some(spec) => {
                                let cap = match spec.kind {
                                    ConditionalKind::Stacking { max: m, .. } => m,
                                    _ => {
                                        spec.kind = ConditionalKind::Stacking {
                                            max,
                                            duration_ms,
                                            scope: Default::default(),
                                            hit_fed: false,
                                        };
                                        max
                                    }
                                };
                                gain_stack(spec, self.now_ms, duration_ms, cap);
                                spec.active = true;
                            }
                            None => self.conditional_specs.push(ConditionalSpec {
                                source_name: name.clone(),
                                kind: ConditionalKind::Stacking {
                                    max,
                                    duration_ms,
                                    scope: Default::default(),
                                    hit_fed: false,
                                },
                                percent: value,
                                crit_damage: false,
                                crit_chance: false,
                                condition_damage: is_condi,
                                active: true,
                                stacks: 1,
                                expires_at_ms: until,
                                stack_expiries: vec![until],
                                stacking_rule: self.proc_specs[idx].stacking_rule.clone(),
                            }),
                        }
                        self.update_conditionals();
                        true
                    }
                    EffectCategory::StrikeDamagePct if value > 2.0 && duration_ms > 0 => {
                        // Sprint 3 (US3): a percent with a duration is a timed
                        // strike bonus (Soul Barbs, Dread), one spec per source,
                        // refreshed by every firing.
                        let until_ms = self.now_ms.saturating_add(duration_ms);
                        match self
                            .conditional_specs
                            .iter_mut()
                            .find(|spec| spec.source_name == name)
                        {
                            Some(spec) => spec.kind = ConditionalKind::Timed { until_ms },
                            None => self.conditional_specs.push(ConditionalSpec {
                                source_name: name.clone(),
                                kind: ConditionalKind::Timed { until_ms },
                                percent: value,
                                crit_damage: false,
                                crit_chance: false,
                                condition_damage: false,
                                active: false,
                                stacks: 0,
                                expires_at_ms: 0,
                                stack_expiries: Vec::new(),
                                stacking_rule: self.proc_specs[idx].stacking_rule.clone(),
                            }),
                        }
                        self.update_conditionals();
                        true
                    }
                    EffectCategory::StrikeDamagePct => {
                        // A coefficient (≤ 2.0) is a flame-blast style proc on
                        // the unequipped weapon strength that cannot crit (wiki
                        // `Superior Sigil of Fire`, read 2026-09-08); a percent
                        // is a share of the held weapon's strike as before.
                        let proc_damage = if value <= 2.0 {
                            UNEQUIPPED_WEAPON_STRENGTH * self.params.power / reference_armor()
                                * value
                                * self.params.strike_mult
                        } else {
                            self.params.weapon_strength * self.params.power / reference_armor()
                                * as_ratio(value)
                        };
                        self.record_damage(proc_damage * p, protected);
                        true
                    }
                    EffectCategory::AppliesBoon
                    | EffectCategory::AppliesCondition
                    | EffectCategory::RemovesBoon
                    | EffectCategory::CorruptsBoon
                    | EffectCategory::RemovesCondition
                    | EffectCategory::ConvertsConditionToBoon
                    | EffectCategory::TransfersCondition => {
                        self.apply_operation(operation.as_ref(), p, &name);
                        true
                    }
                    EffectCategory::OutgoingHealingPct if duration_ms > 0 => {
                        self.heal(value.max(0.0) * p);
                        true
                    }
                    // Sprint 3 (US2): life force and healing from trait records.
                    EffectCategory::GainsLifeForce | EffectCategory::Heal => {
                        let spec = &self.proc_specs[idx];
                        let scale = match spec.scale_by {
                            Some(ScaleBy::ConditionsRemoved) => {
                                (self.conditions_cleansed - cleansed_before) as f64
                            }
                            None => 1.0,
                        };
                        let coefficient = spec.healing_power_coefficient;
                        if matches!(category, EffectCategory::GainsLifeForce) {
                            self.gain_life_force_percent(value * scale * p, &name);
                        } else {
                            let amount = (value + coefficient * self.params.healing_power) * scale;
                            self.heal(amount.max(0.0) * p);
                            // An area heal (Life from Death): the allies in range
                            // are counted (FR-003a).
                            if let Some(op) = operation
                                .as_ref()
                                .filter(|op| matches!(op.target_side, TargetSide::Ally))
                            {
                                let targets = op
                                    .target_count
                                    .as_ref()
                                    .and_then(resolved)
                                    .copied()
                                    .unwrap_or(1);
                                let extra = self.ally_fan_out(targets, &name) - 1;
                                self.ally_healing += extra as f64 * amount.max(0.0) * p;
                            }
                        }
                        true
                    }
                    _ => {
                        let source_type = self.proc_specs[idx].source_type.clone();
                        let source_id = self.proc_specs[idx].source_id;
                        self.note_unmodeled_proc(&source_type, source_id, &name);
                        false
                    }
                }
            };
            if fired {
                self.last_prerequisite_skip.remove(&name);
                *self.proc_fire_counts.entry(name.clone()).or_default() += 1;
                self.trace(TraceKind::ProcFired, &name, format!("{category:?} ×{p:.2}"));
                // Sprint 3 (US1): a trait record also says it is a trait
                // and when it fired, on top of the ProcFired every proc gets.
                if matches!(self.proc_specs[idx].source_type, SourceType::Trait) {
                    *self.trait_fire_counts.entry(name.clone()).or_default() += 1;
                    let when = match trigger {
                        TriggerRule::OnShroudEnter => " at entry".to_string(),
                        TriggerRule::OnShroudExit => format!(
                            " at exit ({})",
                            self.shroud_exit_why.as_deref().unwrap_or("shroud ended")
                        ),
                        TriggerRule::Periodic => " periodic".to_string(),
                        TriggerRule::OnDodge => " on dodge".to_string(),
                        TriggerRule::OnDisableFoe => " on disable foe".to_string(),
                        TriggerRule::OnElite => " on elite".to_string(),
                        TriggerRule::OnThreshold => " on threshold".to_string(),
                        TriggerRule::OnAttunementSwap => " on attunement swap".to_string(),
                        TriggerRule::OnCloneCreated => " on clone created".to_string(),
                        _ => String::new(),
                    };
                    self.trace(
                        TraceKind::TraitFired,
                        &name,
                        format!("{category:?} ×{p:.2}{when}"),
                    );
                }
            }
        }
    }

    fn track_protected_window(&mut self) {
        if self.control_owned() || self.charge_cover_consumed_this_tick {
            self.secured_tick_times.push(self.now_ms);
            self.protected_run_ms += TIMELINE_TICK_MS;
            self.longest_protected_window_ms =
                self.longest_protected_window_ms.max(self.protected_run_ms);
        } else {
            self.protected_run_ms = 0;
        }
    }

    fn control_owned(&self) -> bool {
        self.target.disabled_until_ms > self.now_ms
            || self.has_defense(CoverKind::Stability)
            || self.has_defense(CoverKind::Invulnerability)
            || self.has_defense(CoverKind::Evade)
            || self.has_defense(CoverKind::Stealth)
            || self.has_defense(CoverKind::Block)
    }

    fn control_cover_remaining_ms(&self) -> u32 {
        let defense = self
            .defenses
            .iter()
            .filter(|defense| is_interrupt_cover_kind(&defense.kind))
            .map(|defense| defense.expires_at_ms.saturating_sub(self.now_ms))
            .max()
            .unwrap_or(0);
        defense.max(self.target.disabled_until_ms.saturating_sub(self.now_ms))
    }

    fn has_defense(&self, kind: CoverKind) -> bool {
        self.defenses
            .iter()
            .any(|defense| defense.kind == kind && defense.expires_at_ms > self.now_ms)
    }

    fn consume_defense(&mut self, kind: CoverKind) -> bool {
        let Some(idx) = self
            .defenses
            .iter()
            .position(|defense| defense.kind == kind && defense.expires_at_ms > self.now_ms)
        else {
            return false;
        };
        if self.defenses[idx].stacks > 1 {
            self.defenses[idx].stacks -= 1;
        } else {
            self.defenses.remove(idx);
        }
        true
    }

    fn remove_defense(&mut self, kind: CoverKind) {
        self.defenses.retain(|defense| defense.kind != kind);
    }

    /// The game tracks recharge by skill, not by rendered bar position. The
    /// same skill equipped in both weapon sets therefore shares one timer.
    fn set_skill_cooldown(&mut self, skill_id: u32, cooldown_ms: u32) {
        let ready_ms = self.at(cooldown_ms);
        for (idx, skill) in self.skills.iter().enumerate() {
            if skill.skill_id == skill_id {
                self.cooldown_ready_ms[idx] = ready_ms;
            }
        }
    }

    fn consume_stability(&mut self) -> bool {
        self.consume_defense(CoverKind::Stability)
    }

    fn has_buff(&self, name: &str) -> bool {
        self.buffs
            .iter()
            .any(|buff| buff.name.eq_ignore_ascii_case(name) && buff.expires_at_ms > self.now_ms)
    }

    fn buff_stacks(&self, name: &str) -> u32 {
        let total: u32 = self
            .buffs
            .iter()
            .filter(|buff| buff.name.eq_ignore_ascii_case(name) && buff.expires_at_ms > self.now_ms)
            .map(|buff| buff.stacks)
            .sum();
        // Stack caps come from data/formulas/boons.json (wiki: Might and
        // Stability 25, duration-stacking boons 1) rather than an inline 25.
        match crate::data::boons().get(name) {
            Some(def) => total.min(def.max_stacks),
            None => total,
        }
    }

    fn cleanse(&mut self, count: u32) {
        let removed = count.min(self.incoming_conditions.len() as u32);
        let mut last = None;
        for _ in 0..removed {
            last = self.incoming_conditions.pop();
        }
        self.conditions_cleansed += removed;
        // Sprint 3 convergence (Shrouded Removal): "when removing conditions
        // from yourself" is a removal that found one; a cleanse of nothing
        // is not a firing site.
        if let Some(gone) = last {
            self.status_trigger(TriggerRule::OnConditionRemoved, &gone.name, None, false);
        }
    }

    fn heal(&mut self, amount: f64) {
        if self.in_shroud.as_ref().is_some_and(|s| !s.health_exposed) {
            // wiki `Death Shroud`: necromancers cannot be healed in shroud.
            // wiki `Harbinger Shroud`: they can, there.
            return;
        }
        let before = self.player_health;
        self.player_health = (self.player_health + amount).min(self.params.max_health);
        self.healing += self.player_health - before;
    }

    fn enemy_hp(&self) -> f64 {
        self.target.hp.unwrap_or(0.0)
    }

    fn enemy_hp_alive(&self) -> bool {
        self.target.hp.map(|h| h > 0.0).unwrap_or(false)
    }

    fn apply_to_enemy_hp(&mut self, amount: f64) -> f64 {
        let Some(hp) = self.target.hp.as_mut() else {
            return 0.0;
        };
        if *hp <= 0.0 {
            return 0.0;
        }
        let applied = amount.min(*hp);
        *hp -= applied;
        applied
    }

    fn record_damage(&mut self, amount: f64, protected: bool) {
        if amount <= 0.0 || !self.enemy_hp_alive() {
            return;
        }
        let applied = self.apply_to_enemy_hp(amount);
        if applied <= 0.0 {
            return;
        }
        if !self.enemy_hp_alive() && self.target_reached_at_ms.is_none() {
            self.target_reached_at_ms = Some(self.now_ms);
        }
        self.damage_events.push(DamageEvent {
            at_ms: self.now_ms,
            amount: applied,
            protected,
        });
    }

    fn skill_damage_value(&self, skill: &RotationSkill) -> f64 {
        let mut value = 0.0;
        for effect in &skill.effects {
            match effect {
                SkillEffect::StrikeDamage {
                    hit_count,
                    dmg_multiplier,
                } => {
                    value += self.params.weapon_strength * self.params.power / reference_armor()
                        * *dmg_multiplier
                        * *hit_count as f64;
                }
                SkillEffect::ApplyCondition {
                    condition,
                    stacks,
                    duration_ms,
                } => {
                    value += condition_tick_damage(
                        condition,
                        self.params.condition_damage,
                        &self.params.mode,
                    ) * *stacks as f64
                        * (*duration_ms as f64 / 1_000.0);
                }
                _ => {}
            }
        }
        value / (skill.cast_time_ms.max(100) as f64 / 1_000.0)
    }

    /// This build's ceiling for a pool: the rule's override, else the
    /// profession default.
    fn cap_of(&self, kind: ResourceKind) -> f64 {
        self.pool_caps
            .get(&kind)
            .copied()
            .unwrap_or_else(|| resource_cap(kind, self.params.max_health))
    }

    fn can_pay_resource(&self, skill_id: u32) -> bool {
        let Some(rule) = self.resource_rules.get(&skill_id) else {
            return true;
        };
        self.resources.get(&rule.kind).copied().unwrap_or(0.0) >= rule.cost.max(rule.entry_floor)
    }

    /// Skills the build can never pay for: the cost is above the pool's
    /// cap, so no amount of ramp makes them castable.
    fn unpayable_skills(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .skills
            .iter()
            .filter(|skill| {
                self.resource_rules
                    .get(&skill.skill_id)
                    .is_some_and(|rule| rule.cost.max(rule.entry_floor) > self.cap_of(rule.kind))
            })
            .map(|skill| skill.name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn pay_resource(&mut self, skill_id: u32) {
        let Some(rule) = self.resource_rules.get(&skill_id) else {
            return;
        };
        let spend_all = rule.spend_all;
        let cost = rule.cost;
        let kind = rule.kind;
        let resource = self.resources.entry(kind).or_default();
        if *resource >= cost {
            if spend_all && kind == ResourceKind::Adrenaline {
                // Wiki `Adrenaline`: a burst expends every FULL bar, so the
                // strikes above the last full bar stay on the meter.
                *resource %= ADRENALINE_BAR_STRIKES;
            } else if spend_all {
                *resource = 0.0;
            } else {
                *resource -= cost;
            }
        }
    }

    fn gain_resource_on_hit(&mut self, skill_id: u32) {
        if let Some(rule) = self.resource_rules.get(&skill_id) {
            if rule.gain_on_hit > 0.0 {
                let resource = self.resources.entry(rule.kind).or_default();
                *resource = (*resource + rule.gain_on_hit)
                    .min(resource_cap(rule.kind, self.params.max_health));
            }
        }
        if self.resources.contains_key(&ResourceKind::Adrenaline) {
            // Wiki `Adrenaline`: one strike per non-burst attack that
            // connects. A burst pays for itself, so it does not feed the bar.
            let is_burst = self
                .resource_rules
                .get(&skill_id)
                .is_some_and(|rule| rule.kind == ResourceKind::Adrenaline && rule.cost > 0.0);
            if !is_burst {
                let cap = self.cap_of(ResourceKind::Adrenaline);
                let adrenaline = self.resources.entry(ResourceKind::Adrenaline).or_default();
                *adrenaline = (*adrenaline + 1.0).min(cap);
            }
        }
    }

    /// E0: emit OnThreshold when health drops to 50% or below; the latch
    /// re-arms once health recovers above the threshold (record ICDs, not this
    /// latch, are what rate-limits repeated crossings).
    fn tick_health_threshold_bus(&mut self) {
        if self.params.max_health <= 0.0 {
            return;
        }
        let pct = self.player_health / self.params.max_health * 100.0;
        if pct > 50.0 {
            self.threshold_50_emitted = false;
            return;
        }
        if self.threshold_50_emitted {
            return;
        }
        self.threshold_50_emitted = true;
        self.trigger_bus.emit(BusEvent::OnThreshold, self.now_ms);
        self.trigger_procs(TriggerRule::OnThreshold, None, false, 1.0);
    }

    /// E0: regen EndurancePool; the dodge is reactive — endurance is held until
    /// an incoming strike lands inside the evade window, then spent so the
    /// evade actually covers that strike. The bus emits OnDodge on each spend, so
    /// dodge-tagged trait records still fire.
    fn tick_endurance_and_dodge(&mut self) {
        self.endurance.tick(TIMELINE_TICK_MS);
        if !self.endurance.can_dodge(DODGE_COST) {
            return;
        }
        // Immobilize / hard lock stops dodges (wiki Control effect / Dodge).
        if self.disabled_until_ms > self.now_ms {
            return;
        }
        if self.incoming_conditions.iter().any(|c| {
            crate::data::boon_condition_formulas::canonical_condition_name(&c.name)
                .eq_ignore_ascii_case("Immobile")
                && c.expires_at_ms > self.now_ms
        }) {
            return;
        }
        // Already evading: a second dodge covers nothing the first does not.
        if self.has_defense(CoverKind::Evade) {
            return;
        }
        if !self.strike_within_evade_window() {
            return;
        }
        if self
            .dodge_action
            .try_dodge(&mut self.endurance, &mut self.trigger_bus, self.now_ms)
        {
            self.apply_defense(CoverKind::Evade, DODGE_EVADE_MS, 1, false);
            self.trace(TraceKind::Dodged, "dodge", "endurance spent");
            self.trigger_procs(TriggerRule::OnDodge, None, false, 1.0);
        }
    }

    /// True when an unprocessed enemy strike lands before the evade window a
    /// dodge started now would close. `enemy_events` is sorted by `at_ms` and
    /// events due this tick are still queued (`process_enemy_events` runs after
    /// the dodge), so the scan starts at the front.
    // ponytail: covers the first strike inside the window, not the biggest one
    // in the burst; make it peak-aware if sustain scoring starts caring which
    // hit an evade answered.
    fn strike_within_evade_window(&self) -> bool {
        let window_end = self.now_ms.saturating_add(DODGE_EVADE_MS);
        self.profile
            .enemy_events
            .iter()
            .take_while(|event| event.at_ms < window_end)
            .any(|event| matches!(event.kind, EnemyEventKind::Strike { .. }))
    }

    fn regenerate_resources(&mut self) {
        let seconds = TIMELINE_TICK_MS as f64 / 1_000.0;
        if let Some(drain) = self.in_shroud.as_ref().map(|s| s.drain_per_second) {
            let pool = self.resources.entry(ResourceKind::LifeForce).or_default();
            *pool = (*pool - drain * seconds).max(0.0);
            if *pool <= 0.0 {
                self.exit_shroud("life force 0");
            }
        }
        let initiative_cap = self.cap_of(ResourceKind::Initiative);
        if let Some(initiative) = self.resources.get_mut(&ResourceKind::Initiative) {
            *initiative = (*initiative + seconds).min(initiative_cap);
        }
        // Wiki `Energy`: +5 % per second, and upkeep is a negative modifier
        // on that rate. Upkeep is capped at -10, so the floor is -5 %/s.
        let energy_rate =
            (ENERGY_REGEN_PER_SECOND - self.active_upkeep).max(-ENERGY_REGEN_PER_SECOND);
        if let Some(energy) = self.resources.get_mut(&ResourceKind::Energy) {
            *energy = (*energy + energy_rate * seconds).clamp(0.0, 100.0);
            // Wiki `Energy`: at 0 % every upkeep skill ends, even when total
            // regeneration is positive. Without this the ledger holds a
            // Herald at -5 %/s for the whole fight and calls the bar
            // unplayable; in game the facets simply drop.
            if *energy <= 0.0 {
                self.active_upkeep = 0.0;
            }
        }
        // Pools that fill on the clock rather than from actions (flow).
        let flow_rate = self
            .pool_regen
            .get(&ResourceKind::Flow)
            .copied()
            .unwrap_or(0.0);
        let flow_cap = self.cap_of(ResourceKind::Flow);
        if let Some(flow) = self.resources.get_mut(&ResourceKind::Flow) {
            *flow = (*flow + flow_rate * seconds).min(flow_cap);
        }
    }

    /// Invoke the other legend. Wiki `Legend`: a 10 s recharge, and the swap
    /// resets energy to 50 -- the revenant's only way to refill mid-fight, so
    /// the timeline takes it once the pool is spent rather than standing
    /// there unable to pay. Any maintained upkeep ends with the legend.
    fn try_legend_swap(&mut self) -> bool {
        if !self.resources.contains_key(&ResourceKind::Energy)
            || self.now_ms < self.legend_swap_ready_ms
        {
            return false;
        }
        let energy = self
            .resources
            .get(&ResourceKind::Energy)
            .copied()
            .unwrap_or(0.0);
        // Swapping on a full pool throws the reset away and puts the only
        // refill on a 10 s clock; wait until half of it is gone.
        if energy >= LEGEND_SWAP_ENERGY / 2.0 {
            return false;
        }
        self.resources
            .insert(ResourceKind::Energy, LEGEND_SWAP_ENERGY);
        self.legend_swap_ready_ms = self.at(LEGEND_SWAP_RECHARGE_MS);
        self.active_upkeep = 0.0;
        self.trigger_procs(TriggerRule::OnLegendSwap, None, false, 1.0);
        true
    }

    fn report(&self) -> WvwCombatReport {
        let protected_damage: f64 = self
            .damage_events
            .iter()
            .filter(|event| event.protected)
            .map(|event| event.amount)
            .sum();
        let peak_2s = peak_damage(&self.damage_events, MIN_PROTECTED_WINDOW_MS, true);
        let peak_5s = peak_damage(&self.damage_events, self.profile.desired_window_ms, true);
        let total_damage: f64 = self.damage_events.iter().map(|event| event.amount).sum();
        let remaining_health_ratio = self.player_health / self.params.max_health.max(1.0);
        let sustain_margin = (self.healing + self.barrier_absorbed + self.avoided_damage
            - self.incoming_damage)
            / (self.profile.duration_ms as f64 / 1_000.0).max(1.0);
        let target_reached = self.profile.target_health.is_some() && !self.enemy_hp_alive();
        let sequence = secured_sequence_summary(
            &self.secured_tick_times,
            &self.protected_actions,
            &self.damage_events,
            self.profile.desired_window_ms,
            self.profile.required_window_ms,
        );
        let chain_completed = sequence.completed;
        // Repeatable = the player leaves the exchange able to fight again:
        // alive, resources back, and either the target went down, the fight
        // was net-positive, or half the bar is left. Skill cooldowns are NOT
        // part of it: the timeline already refuses to recast a skill that is
        // on cooldown inside the fight, and demanding every skill of the best
        // secured window be ready again 5s after the window closed failed
        // every heal (20-30s) and every elite (60-180s) in the game. In-game
        // 2026-09-05 a Roam/Support Scourge at 87% health after the fight was
        // non-viable on that rule alone and 32k search evaluations found
        // nothing viable, because nothing could be.
        let resource_recovery = self.sequence_resources_recovered(&sequence.skill_ids);
        let repeatable = self.player_health > 0.0
            && chain_completed
            && resource_recovery
            && (target_reached || sustain_margin >= 0.0 || remaining_health_ratio >= 0.50);

        let mut coverage: Vec<CoverageEntry> = self
            .unmodeled_names
            .iter()
            .map(|note| CoverageEntry::from_runtime_note(note))
            .collect();
        coverage.extend(self.no_record_entries.iter().cloned());

        WvwCombatReport {
            duration_ms: self.profile.duration_ms,
            target_health: self.profile.target_health,
            target_reached_at_ms: self.target_reached_at_ms,
            longest_protected_window_ms: self.longest_protected_window_ms,
            protected_action_count: self.protected_action_count,
            successful_action_count: self.successful_action_count,
            interrupted_casts: self.interrupted_casts,
            protected_damage,
            peak_protected_damage_2s: peak_2s,
            peak_protected_damage_5s: peak_5s,
            total_damage,
            control_landed_ms: self.control_landed_ms,
            incoming_damage: self.incoming_damage,
            avoided_damage: self.avoided_damage,
            healing: self.healing,
            barrier_absorbed: self.barrier_absorbed,
            conditions_cleansed: self.conditions_cleansed,
            combo_activations: self.combo_activations,
            remaining_health_ratio,
            sustain_margin,
            player_survived: self.player_health > 0.0,
            target_reached,
            chain_completed,
            secured_sequence_damage: sequence.damage,
            secured_sequence_control_ms: sequence.control_ms,
            repeatable,
            resource_blocked_actions: self.resource_blocked_skills.len() as u32,
            resource_legal: self.resource_blocked_skills.is_empty(),
            resource_blocked_ratio: self.resource_blocked_events as f64
                / self.resource_priority_actions.max(1) as f64,
            resource_unpayable_skills: self.unpayable_skills(),
            resource_model_complete: self.resource_model_complete,
            resource_model_gaps: self.resource_model_gaps.clone(),
            resource_simulated: !self.resource_rules.is_empty(),
            profession: self.profession.clone(),
            unmodeled_sources: coverage.iter().map(CoverageEntry::rendered).collect(),
            coverage,
            trait_fire_counts: self.trait_fire_counts.clone(),
            cleave_damage: self.cleave_damage,
            cleave_condition_stack_seconds: self.cleave_condition_stack_seconds,
            ally_boon_stack_seconds: self.ally_boon_stack_seconds,
            ally_healing: self.ally_healing,
            ally_cleanses: self.ally_cleanses,
            trace: self.trace.clone(),
            trace_truncated: self.trace_truncated,
            proc_trials: Vec::new(),
            shroud_refusals: self.shroud_refusals.clone(),
            dodge_count: self.dodge_action.dodges,
            bus_on_dodge: self.trigger_bus.count(BusEvent::OnDodge),
            bus_on_disable_foe: self.trigger_bus.count(BusEvent::OnDisableFoe),
            bus_on_attunement_swap: self.trigger_bus.count(BusEvent::OnAttunementSwap),
            bus_on_clone_created: self.trigger_bus.count(BusEvent::OnCloneCreated),
        }
    }

    fn sequence_resources_recovered(&self, skill_ids: &HashSet<u32>) -> bool {
        let mut required: HashMap<ResourceKind, f64> = HashMap::new();
        for skill_id in skill_ids {
            let Some(rule) = self.resource_rules.get(skill_id) else {
                continue;
            };
            let entry = required.entry(rule.kind).or_default();
            if rule.spend_all {
                *entry = (*entry).max(rule.cost);
            } else {
                *entry += rule.cost;
            }
        }
        required
            .into_iter()
            .all(|(kind, cost)| self.resources.get(&kind).copied().unwrap_or(0.0) >= cost)
    }
}

fn secured_sequence_summary(
    secured_tick_times: &[u32],
    protected_actions: &[ProtectedActionEvent],
    damage_events: &[DamageEvent],
    max_span_ms: u32,
    required_secured_ms: u32,
) -> SecuredSequenceSummary {
    let mut summary = SecuredSequenceSummary::default();
    for (left, start_ms) in secured_tick_times.iter().copied().enumerate() {
        let end_ms = start_ms + max_span_ms;
        let secured_ticks = secured_tick_times[left..]
            .iter()
            .take_while(|at_ms| **at_ms <= end_ms)
            .count() as u32;
        if secured_ticks * TIMELINE_TICK_MS < required_secured_ms {
            continue;
        }
        let actions: Vec<&ProtectedActionEvent> = protected_actions
            .iter()
            .filter(|action| action.at_ms >= start_ms && action.at_ms <= end_ms)
            .collect();
        if actions.len() < 2 {
            continue;
        }
        let damage: f64 = damage_events
            .iter()
            .filter(|event| event.protected && event.at_ms >= start_ms && event.at_ms <= end_ms)
            .map(|event| event.amount)
            .sum();
        let control_ms = actions.iter().map(|action| action.control_ms).sum();
        let applies_condition = actions.iter().any(|action| action.applies_condition);
        let supports_allies = actions.iter().any(|action| action.supports_allies);
        // A window in which nothing happened is not a sequence. A window in
        // which the player healed and cleansed under pressure is — and it
        // used to be discarded, because the test asked only for damage,
        // control or a condition. That made every support build unable to
        // complete a chain and therefore unable to pass ProtectedExecution,
        // whatever else it did: a WvW healer failed the gate by doing its job.
        if damage <= 0.0 && control_ms == 0 && !applies_condition && !supports_allies {
            continue;
        }
        if !summary.completed
            || damage > summary.damage
            || (damage == summary.damage && control_ms > summary.control_ms)
        {
            summary.completed = true;
            summary.damage = damage;
            summary.control_ms = control_ms;
            summary.skill_ids = actions.iter().map(|action| action.skill_id).collect();
        }
    }
    summary
}

fn peak_damage(events: &[DamageEvent], window_ms: u32, protected_only: bool) -> f64 {
    let mut best: f64 = 0.0;
    let mut left = 0usize;
    let mut total = 0.0;
    for right in 0..events.len() {
        if !protected_only || events[right].protected {
            total += events[right].amount;
        }
        while events[right].at_ms.saturating_sub(events[left].at_ms) > window_ms {
            if !protected_only || events[left].protected {
                total -= events[left].amount;
            }
            left += 1;
        }
        best = best.max(total);
    }
    best
}

fn is_control_cover(effect: &SkillEffect) -> bool {
    matches!(
        effect,
        SkillEffect::Cover {
            kind: CoverKind::Stability
                | CoverKind::Invulnerability
                | CoverKind::Evade
                | CoverKind::Stealth
                | CoverKind::Aegis
                | CoverKind::Blind
                | CoverKind::Block,
            ..
        } | SkillEffect::Mobility {
            kind: MobilityKind::Evade | MobilityKind::Stealth
        }
    )
}

fn is_interrupt_cover_kind(kind: &CoverKind) -> bool {
    matches!(
        kind,
        CoverKind::Stability
            | CoverKind::Invulnerability
            | CoverKind::Evade
            | CoverKind::Stealth
            | CoverKind::Block
    )
}

fn boon_cover_kind(name: &str) -> Option<CoverKind> {
    if name.eq_ignore_ascii_case("Stability") {
        Some(CoverKind::Stability)
    } else if name.eq_ignore_ascii_case("Aegis") {
        Some(CoverKind::Aegis)
    } else if name.eq_ignore_ascii_case("Protection") {
        Some(CoverKind::Protection)
    } else if name.eq_ignore_ascii_case("Resistance") {
        Some(CoverKind::Resistance)
    } else {
        None
    }
}

fn resolved<T>(value: &FactualValue<T>) -> Option<&T> {
    match value {
        FactualValue::Resolved(value) => Some(value),
        FactualValue::Unknown => None,
    }
}

fn as_ratio(value: f64) -> f64 {
    if value.abs() > 2.0 {
        value / 100.0
    } else {
        value
    }
}

fn leftover_condition_fraction(condition: &TimedCondition) -> f64 {
    let period_start = condition.next_tick_ms.saturating_sub(1_000);
    if condition.expires_at_ms <= period_start {
        return 0.0;
    }
    let remaining_ms = condition.expires_at_ms - period_start;
    if remaining_ms >= 1_000 {
        return 0.0;
    }
    remaining_ms as f64 / 1_000.0
}

/// Human label for the coverage line: `Superior Sigil of Fire (on-crit)`.
pub(crate) fn trigger_label(trigger: &TriggerRule) -> &'static str {
    match trigger {
        TriggerRule::Passive => "passive",
        TriggerRule::OnCrit => "on-crit",
        TriggerRule::OnHit => "on-hit",
        TriggerRule::OnSkillUse => "on-skill-use",
        TriggerRule::OnHealthThreshold => "on-health-threshold",
        TriggerRule::Conditional => "conditional",
        TriggerRule::OnShroudEnter => "on-shroud-enter",
        TriggerRule::OnShroudExit => "on-shroud-exit",
        TriggerRule::OnConditionApplied => "on-condition-applied",
        TriggerRule::OnConditionRemoved => "on-condition-removed",
        TriggerRule::OnBoonApplied => "on-boon-applied",
        TriggerRule::OnBoonStripped => "on-boon-stripped",
        TriggerRule::Periodic => "periodic",
        TriggerRule::OnDodge => "on-dodge",
        TriggerRule::OnDisableFoe => "on-disable-foe",
        TriggerRule::OnElite => "on-elite",
        TriggerRule::OnThreshold => "on-threshold",
        TriggerRule::OnAttunementSwap => "on-attunement-swap",
        TriggerRule::OnCloneCreated => "on-clone-created",
        TriggerRule::NotApplicable => "not-applicable",
        TriggerRule::OnBlock => "on-block",
        TriggerRule::OnSteal => "on-steal",
        TriggerRule::OnStealthEnter => "on-stealth-enter",
        TriggerRule::OnStealthExit => "on-stealth-exit",
        TriggerRule::OnLegendSwap => "on-legend-swap",
        TriggerRule::OnBerserkEnter => "on-berserk-enter",
        TriggerRule::OnSymbolHit => "on-symbol-hit",
        TriggerRule::OnExplosion => "on-explosion",
        TriggerRule::OnBoonGained { .. } => "on-boon-gained",
        TriggerRule::OnStunbreak => "on-stunbreak",
    }
}

/// `strike_crit_factor_with_bonus` with extra critical damage percentage
/// points from an active conditional (Sprint 3). Equal to it at 0.
fn strike_crit_factor_with_crit_damage(
    precision: f64,
    ferocity: f64,
    crit_chance_bonus_pct: f64,
    crit_damage_bonus_pct: f64,
) -> f64 {
    if precision <= 0.0 {
        return 1.0;
    }
    let chance = crit_chance_fraction(precision, crit_chance_bonus_pct);
    let crit_mult = crate::data::universal_formulas::formulas().crit_damage(ferocity) / 100.0
        + crit_damage_bonus_pct / 100.0;
    1.0 + chance * (crit_mult - 1.0)
}

/// Whether a cast of `skill` counts for a skill-use `scope`. `skills` is the
/// whole bar (a `Shroud_N` scope reads whether the build has a shroud bar).
/// `Status` is a status-trigger scope, never a skill-use one. Shared by the
/// timeline and the flow simulation so the two cannot disagree.
pub(crate) fn skill_scope_admits(
    scope: &crate::data::normalized_effects::TriggerScope,
    skill: &RotationSkill,
    skills: &[RotationSkill],
) -> bool {
    use crate::data::normalized_effects::TriggerScope;
    match scope {
        TriggerScope::Any => true,
        TriggerScope::WeaponSkillWithRecharge => skill.weapon_set != 0 && skill.cooldown_ms > 0,
        // Sprint 3 (US2): trait-owned skill-use by category or slot.
        TriggerScope::Category(category) => skill
            .categories
            .iter()
            .any(|c| c.eq_ignore_ascii_case(category)),
        TriggerScope::Slot(slot) => {
            // `Shroud_N`: the Nth skill of the shroud bar (wiki
            // "shroud skill N"); otherwise the slot head before `_`.
            if let Some(n) = slot.strip_prefix("Shroud_") {
                // wiki `Shade`: a Scourge has no shroud bar and its
                // shade skills (F1..F5) count as its shroud skills.
                let has_shroud_bar = skills.iter().any(|k| k.weapon_set == super::SHROUD_SET);
                let (set, head) = if has_shroud_bar {
                    (super::SHROUD_SET, "Weapon")
                } else {
                    (skill.weapon_set, "Profession")
                };
                return skill.weapon_set == set
                    && skill.slot_name.as_deref() == Some(format!("{head}_{n}").as_str());
            }
            skill.slot_name.as_deref().is_some_and(|name| {
                name.split('_')
                    .next()
                    .is_some_and(|head| head.eq_ignore_ascii_case(slot))
            })
        }
        TriggerScope::Status(_) => false,
    }
}

/// Alias-safe compare of a stored foe-condition name against a prerequisite.
pub(crate) fn foe_condition_name_eq(stored: &str, want: &str) -> bool {
    stored.eq_ignore_ascii_case(want)
        || crate::data::boon_condition_formulas::canonical_condition_name(stored)
            .eq_ignore_ascii_case(
                crate::data::boon_condition_formulas::canonical_condition_name(want),
            )
}

/// A snapshot of what `prerequisite_holds` reads, so `update_conditionals`
/// can evaluate foe prerequisites while it holds `&mut self.conditional_specs`.
struct PrerequisiteView {
    in_shroud: bool,
    foe_conditions: Vec<String>,
    foe_stacks: Vec<(String, u32)>,
    foe_ratio: Option<f64>,
    attunement: AttunementState,
}

impl PrerequisiteView {
    fn of(timeline: &Timeline<'_>) -> Self {
        Self {
            in_shroud: timeline.in_shroud.is_some(),
            foe_conditions: timeline
                .target
                .conditions
                .iter()
                .filter(|c| c.expires_at_ms > timeline.now_ms)
                .map(|c| c.name.to_string())
                .collect(),
            foe_stacks: timeline
                .target
                .conditions
                .iter()
                .filter(|c| c.expires_at_ms > timeline.now_ms)
                .map(|c| (c.name.to_string(), c.stacks))
                .collect(),
            foe_ratio: timeline
                .profile
                .target_health
                .filter(|h| *h > 0.0)
                .map(|target| timeline.enemy_hp() / target),
            attunement: timeline.attunement.clone(),
        }
    }

    /// Unexpired stacks of `condition` on the primary foe.
    fn foe_stacks(&self, condition: &str) -> u32 {
        self.foe_stacks
            .iter()
            .filter(|(name, _)| foe_condition_name_eq(name, condition))
            .map(|(_, stacks)| *stacks)
            .sum()
    }

    fn is_ok_with(&self, prerequisite: &Prerequisite) -> bool {
        if prerequisite
            .in_shroud
            .is_some_and(|want| want != self.in_shroud)
        {
            return false;
        }
        if let Some(condition) = &prerequisite.foe_condition {
            if !self
                .foe_conditions
                .iter()
                .any(|c| foe_condition_name_eq(c, condition))
            {
                return false;
            }
        }
        if let Some(gate) = &prerequisite.foe_health {
            let (Some(ratio), Some(&percent)) = (self.foe_ratio, resolved(&gate.percent)) else {
                return false;
            };
            let holds = if gate.above {
                ratio > percent / 100.0
            } else {
                ratio < percent / 100.0
            };
            if !holds {
                return false;
            }
        }
        if let Some(want) = &prerequisite.attunement {
            let Some(element) = Element::parse(want) else {
                return false;
            };
            if !self.attunement.is_attuned(element) {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Copy)]
enum PrerequisiteFail {
    NotInShroud,
    InShroud,
    FoeNotCondition,
    FoeHealthUnknown,
    FoeHealthUnresolved,
    FoeHealth,
    UnknownAttunement,
    NotAttuned,
}

impl PrerequisiteFail {
    fn detail(self, prerequisite: &Prerequisite) -> String {
        match self {
            Self::NotInShroud => "not in shroud".into(),
            Self::InShroud => "in shroud".into(),
            Self::FoeNotCondition => format!(
                "foe not {}",
                prerequisite.foe_condition.as_deref().unwrap_or("")
            ),
            Self::FoeHealthUnknown => "foe health unknown".into(),
            Self::FoeHealthUnresolved => "foe health gate unresolved".into(),
            Self::FoeHealth => {
                let (above, percent) = prerequisite
                    .foe_health
                    .as_ref()
                    .and_then(|gate| resolved(&gate.percent).map(|&percent| (gate.above, percent)))
                    .unwrap_or((false, 0.0));
                format!(
                    "foe {} {percent:.0}%",
                    if above { "below" } else { "above" }
                )
            }
            Self::UnknownAttunement => format!(
                "unknown attunement {}",
                prerequisite.attunement.as_deref().unwrap_or("")
            ),
            Self::NotAttuned => format!(
                "not attuned to {}",
                prerequisite.attunement.as_deref().unwrap_or("")
            ),
        }
    }
}

/// A weapon the build wears, as a `Gate::Weapon` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquippedWeapon {
    /// Weapon set 1 or 2.
    pub set: u8,
    pub hand: WeaponHand,
    /// `gw2_core::i18n::weapon_type_key` of the weapon's type.
    pub weapon_type: String,
}

/// Health percentage inside a gate's band. An absent side is open.
fn health_in_band(pct: f64, below: Option<f64>, above: Option<f64>) -> bool {
    below.is_none_or(|b| pct < b) && above.is_none_or(|a| pct > a)
}

/// Add one stack to a stacking conditional at `now`.
///
/// `StackingRule::RefreshAllStacks` (Lethal Tempo) resets every stack the
/// spec already holds; any other rule leaves each earlier stack on its own
/// clock, which is how intensity stacking expires in game. At the cap the
/// stack closest to expiring is the one replaced.
fn gain_stack(spec: &mut ConditionalSpec, now: u32, duration_ms: u32, max: u32) {
    let until = now.saturating_add(duration_ms);
    if spec.stacking_rule == StackingRule::RefreshAllStacks {
        for at in spec.stack_expiries.iter_mut() {
            *at = until;
        }
    }
    spec.stack_expiries.push(until);
    if spec.stack_expiries.len() > max as usize {
        spec.stack_expiries.sort_unstable();
        let excess = spec.stack_expiries.len() - max as usize;
        spec.stack_expiries.drain(..excess);
    }
    spec.stacks = spec.stack_expiries.len() as u32;
    spec.expires_at_ms = spec.stack_expiries.iter().copied().max().unwrap_or(until);
}

/// The one resource-name table a record's `Gate::SelfResourceStacks` and
/// `Scale::PerSelfResourceStack` are read against. A name that is not here
/// is a pool the timeline does not keep, and the record abstains saying so.
fn resource_kind_by_name(name: &str) -> Option<ResourceKind> {
    match name
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "")
        .as_str()
    {
        "initiative" => Some(ResourceKind::Initiative),
        "energy" => Some(ResourceKind::Energy),
        "adrenaline" => Some(ResourceKind::Adrenaline),
        "illusions" | "clones" => Some(ResourceKind::Illusions),
        "blades" | "charges" => Some(ResourceKind::Blades),
        "lifeforce" => Some(ResourceKind::LifeForce),
        "flow" => Some(ResourceKind::Flow),
        _ => None,
    }
}

/// Why `load_normalized_effects` cannot execute a record yet, worded so the
/// coverage line names the missing piece rather than going quiet.
///
/// Doctrine rule 6: a gate the timeline has no state for abstains and says
/// which mechanic it is waiting on. It never passes silently.
pub(crate) fn unexecutable_reason(effect: &NormalizedEffect) -> Option<String> {
    if !matches!(effect.actor, Actor::Player | Actor::Any) {
        return Some(format!("event not yet emitted: {:?} actor", effect.actor));
    }
    let missing_event = match &effect.trigger_rule {
        TriggerRule::OnBlock => Some("on-block"),
        TriggerRule::OnSteal => Some("on-steal"),
        TriggerRule::OnStealthEnter => Some("on-stealth-enter"),
        TriggerRule::OnStealthExit => Some("on-stealth-exit"),
        TriggerRule::OnBerserkEnter => Some("on-berserk-enter"),
        TriggerRule::OnSymbolHit => Some("on-symbol-hit"),
        TriggerRule::OnExplosion => Some("on-explosion"),
        _ => None,
    };
    if let Some(event) = missing_event {
        return Some(format!("event not yet emitted: {event}"));
    }
    for gate in &effect.gates {
        let missing = match gate {
            Gate::Positional(_) => Some("gate not yet modelled: player facing".to_string()),
            Gate::Proximity { .. } => Some("gate not yet modelled: foe distance".to_string()),
            Gate::SelfResourceStacks { resource, .. }
                if resource_kind_by_name(resource).is_none() =>
            {
                Some(format!("resource not yet modelled: {resource}"))
            }
            // A boon the buff model never tracks would leave the gate shut
            // for the whole fight with nothing on the coverage line.
            Gate::SelfBoon { boon } | Gate::SelfBoonAbsent { boon }
                if crate::data::boons().get(boon).is_none() =>
            {
                Some(format!("boon not yet modelled: {boon}"))
            }
            _ => None,
        };
        if missing.is_some() {
            return missing;
        }
    }
    match &effect.scale {
        Some(Scale::PerDistance { .. }) => Some("scale not yet modelled: foe distance".to_string()),
        Some(Scale::PerSelfResourceStack { resource, .. })
            if resource_kind_by_name(resource).is_none() =>
        {
            Some(format!("resource not yet modelled: {resource}"))
        }
        Some(Scale::PerSelfBoon { boon, .. }) if crate::data::boons().get(boon).is_none() => {
            Some(format!("boon not yet modelled: {boon}"))
        }
        _ => None,
    }
}

/// Two triggers are the same event. `OnBoonGained`'s optional boon narrows
/// *which* boon fires it, not which event it is, so the discriminant is the
/// whole answer and the boon is checked beside the scope.
fn same_trigger(left: &TriggerRule, right: &TriggerRule) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn initial_resources(rules: &[SkillResourceRule]) -> HashMap<ResourceKind, f64> {
    let mut resources = HashMap::new();
    for rule in rules {
        resources
            .entry(rule.kind)
            .or_insert_with(|| match rule.kind {
                ResourceKind::Initiative => 12.0,
                ResourceKind::Energy => 50.0,
                ResourceKind::Adrenaline => 10.0,
                ResourceKind::Illusions
                | ResourceKind::Blades
                | ResourceKind::LifeForce
                | ResourceKind::Flow => 0.0,
            });
    }
    resources
}

fn resource_cap(kind: ResourceKind, max_health: f64) -> f64 {
    match kind {
        ResourceKind::Initiative => 12.0,
        ResourceKind::Energy => 100.0,
        ResourceKind::Adrenaline => 30.0,
        ResourceKind::Illusions => 3.0,
        ResourceKind::Blades => 5.0,
        ResourceKind::LifeForce => crate::data::shroud::table().pool_for(max_health),
        ResourceKind::Flow => 100.0,
    }
}

fn condition_is_damaging(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bleeding" | "burning" | "confusion" | "poison" | "torment"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combat::CombatPerformance;
    use crate::data::normalized_effects::Positional;
    use crate::referee::evaluate_viability_gates;
    use crate::rotation::{ControlKind, SkillSlot};
    use crate::scenario::{OptimizationTarget, TargetProfile};
    use gw2_core::types::GameMode;

    fn skill(
        skill_id: u32,
        slot: SkillSlot,
        cast_time_ms: u32,
        cooldown_ms: u32,
        effects: Vec<SkillEffect>,
    ) -> RotationSkill {
        RotationSkill {
            targets: 1,
            skill_id,
            name: format!("test-{skill_id}"),
            slot,
            cast_time_ms,
            cooldown_ms,
            effects,
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
            categories: Vec::new(),
            slot_name: None,
        }
    }

    fn params() -> SimParams {
        let mut params = SimParams::basic(2_000.0, 1_500.0, 1_100.0);
        params.max_health = 20_000.0;
        params.armor = 2_500.0;
        params.mode = GameMode::WvW;
        params
    }

    fn profile(duration_ms: u32, events: Vec<EnemyEvent>) -> WvwProfile {
        WvwProfile {
            duration_ms,
            target_health: Some(18_000.0),
            enemy_events: events.into(),
            required_window_ms: MIN_PROTECTED_WINDOW_MS,
            desired_window_ms: TARGET_PROTECTED_WINDOW_MS.min(duration_ms),
        }
    }

    /// A strike every `period_ms` for the whole fight. Chip damage on purpose:
    /// the reactive dodge needs something incoming to spend endurance on, and
    /// these fixtures must not die to the pressure they add.
    fn strike_script(duration_ms: u32, period_ms: u32, damage: f64) -> Vec<EnemyEvent> {
        (0..duration_ms / period_ms)
            .map(|i| EnemyEvent {
                at_ms: i * period_ms,
                kind: EnemyEventKind::Strike {
                    damage,
                    unblockable: false,
                },
            })
            .collect()
    }

    fn run_report(
        skills: &[RotationSkill],
        rules: &[SkillResourceRule],
        enemy: EnemyDummy,
        profile: WvwProfile,
        params: &SimParams,
    ) -> WvwCombatReport {
        let mut timeline =
            Timeline::new(skills, params, profile, enemy, &[], rules, true, Vec::new());
        // Scenario fixtures measure the kit under test, not the opening dodges:
        // a full pool evades the first 1.5s of the script (see
        // `tick_endurance_and_dodge`), which would mask what they assert.
        timeline.endurance.current = 0.0;
        timeline.run();
        timeline.report()
    }

    fn run_report_with_effects(
        skills: &[RotationSkill],
        rules: &[SkillResourceRule],
        enemy: EnemyDummy,
        profile: WvwProfile,
        params: &SimParams,
        active_effects: &[&NormalizedEffect],
    ) -> WvwCombatReport {
        let mut timeline = Timeline::new(
            skills,
            params,
            profile,
            enemy,
            active_effects,
            rules,
            true,
            Vec::new(),
        );
        timeline.run();
        timeline.report()
    }

    /// E0 Kent: EndurancePool → DodgeAction → bus OnDodge → dodge traits execute.
    #[test]
    fn kent_e0_causal_dodge_fires_expeditious_dodger() {
        use crate::data::normalized_effects::{effects, SourceType};

        let effects_wvw = effects().effects_for_mode("WvW");
        let dodge_records: Vec<&_> = effects_wvw
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait
                    && e.trigger_rule == TriggerRule::OnDodge
                    && e.coverage.is_none()
                    && matches!(e.source_id, 1240 | 1289 | 1379 | 1782)
            })
            .collect();
        assert!(
            dodge_records.len() >= 3,
            "E0 must ship >=3 executable OnDodge trait records; got {}",
            dodge_records.len()
        );

        let skills = [skill(
            1,
            SkillSlot::Weapon1,
            1_000,
            0,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let active: Vec<&_> = dodge_records;
        let report = run_report_with_effects(
            &skills,
            &[],
            open_enemy(false),
            profile(25_000, strike_script(25_000, 1_000, 10.0)),
            &params,
            &active,
        );

        assert!(
            report.dodge_count >= 2,
            "endurance regen must yield >=2 dodges over 25s; got {}",
            report.dodge_count
        );
        assert_eq!(
            report.bus_on_dodge, report.dodge_count,
            "bus OnDodge must equal dodge_count"
        );
        let fired = report
            .trait_fire_counts
            .get("Expeditious Dodger")
            .copied()
            .unwrap_or(0);
        assert!(
            fired >= 1,
            "Expeditious Dodger must execute via OnDodge; fires={fired}; counts={:?}",
            report.trait_fire_counts
        );
        let executing = [
            "Expeditious Dodger",
            "Pumping Up",
            "Resilient Roll",
            "Resolute Evasion",
        ]
        .iter()
        .filter(|n| report.trait_fire_counts.get(**n).copied().unwrap_or(0) >= 1)
        .count();
        assert!(
            executing >= 3,
            ">=3 dodge-family traits must execute; got {executing}; {:?}",
            report.trait_fire_counts
        );
    }

    /// E1 Kent: disable inactive vs active changes Dazzling Vulnerability via OnDisableFoe.
    #[test]
    fn kent_e1_causal_disable_inactive_vs_active_changes_dazzling() {
        use crate::data::normalized_effects::{effects, SourceType};

        let effects_wvw = effects().effects_for_mode("WvW");
        let disable_records: Vec<&_> = effects_wvw
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait
                    && e.trigger_rule == TriggerRule::OnDisableFoe
                    && e.coverage.is_none()
                    && matches!(e.source_id, 694 | 1838 | 1983)
            })
            .collect();
        assert!(
            disable_records.len() >= 3,
            "E1 must ship >=3 executable OnDisableFoe trait records; got {}",
            disable_records.len()
        );

        let skills = [skill(
            1,
            SkillSlot::Utility,
            250,
            4_000,
            vec![SkillEffect::CrowdControl {
                kind: ControlKind::Stun,
                duration_ms: 2_000,
                stops_dodge: true,
            }],
        )];
        let params = params();
        let active: Vec<&_> = disable_records;

        let mut live = Timeline::new(
            &skills,
            &params,
            profile(3_000, vec![]),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );
        live.run();
        let live_report = live.report();
        let live_vuln = live.target.stacks_of("Vulnerability", 1_000);
        assert!(
            live.trigger_bus.count(BusEvent::OnDisableFoe) >= 1,
            "landed disable must emit OnDisableFoe"
        );
        assert_eq!(
            live_report.bus_on_disable_foe,
            live.trigger_bus.count(BusEvent::OnDisableFoe)
        );
        let fired = live_report
            .trait_fire_counts
            .get("Dazzling")
            .copied()
            .unwrap_or(0);
        assert!(
            fired >= 1,
            "Dazzling must execute via OnDisableFoe; fires={fired}; counts={:?}",
            live_report.trait_fire_counts
        );
        assert!(
            live_vuln >= 5,
            "Dazzling must apply 5 Vulnerability; got {live_vuln}"
        );

        let mut blocked = Timeline::new(
            &skills,
            &params,
            profile(3_000, vec![]),
            open_enemy(true),
            &active,
            &[],
            true,
            Vec::new(),
        );
        blocked.run();
        let blocked_report = blocked.report();
        let blocked_vuln = blocked.target.stacks_of("Vulnerability", 1_000);
        assert_eq!(
            blocked.trigger_bus.count(BusEvent::OnDisableFoe),
            0,
            "Stability must block OnDisableFoe emit"
        );
        assert_eq!(blocked_report.bus_on_disable_foe, 0);
        assert_eq!(
            blocked_report
                .trait_fire_counts
                .get("Dazzling")
                .copied()
                .unwrap_or(0),
            0
        );
        assert_eq!(
            blocked_vuln, 0,
            "inactive disable must not apply Dazzling Vulnerability"
        );
        assert!(
            live_vuln > blocked_vuln,
            "disable active vs inactive must change measured Vulnerability ({live_vuln} vs {blocked_vuln})"
        );

        let executing = ["Dazzling", "Delayed Reactions", "Dulled Senses"]
            .iter()
            .filter(|n| live_report.trait_fire_counts.get(**n).copied().unwrap_or(0) >= 1)
            .count();
        assert!(
            executing >= 3,
            ">=3 disable-trigger traits must execute; got {executing}; {:?}",
            live_report.trait_fire_counts
        );
    }

    /// E2 Kent: elite + Final Shielding casts Lesser Arcane Shield; inactive has none.
    #[test]
    fn kent_e2_causal_trait_skill_inactive_vs_active_changes_arcane_shield() {
        use crate::data::normalized_effects::{effects, SourceType};

        let effects_wvw = effects().effects_for_mode("WvW");
        let trait_skill_records: Vec<&_> = effects_wvw
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait
                    && e.cast_skill_id.is_some()
                    && e.coverage.is_none()
                    && matches!(e.source_id, 257 | 1368 | 654)
            })
            .collect();
        assert!(
            trait_skill_records.len() >= 3,
            "E2 must ship >=3 executable trait-skill records; got {}",
            trait_skill_records.len()
        );

        let skills = [
            skill(1, SkillSlot::Elite, 250, 1_000, vec![]),
            skill(2, SkillSlot::Heal, 250, 1_000, vec![]),
        ];
        let mut heal = skills[1].clone();
        heal.slot_name = Some("Heal".into());
        let skills = [skills[0].clone(), heal];
        let params = params();
        let active: Vec<&_> = trait_skill_records;

        let mut live = Timeline::new(
            &skills,
            &params,
            profile(4_000, vec![]),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );
        live.run();
        let live_report = live.report();
        let fired = live_report
            .trait_fire_counts
            .get("Final Shielding")
            .copied()
            .unwrap_or(0);
        assert!(
            fired >= 1,
            "Final Shielding must execute via OnElite cast; fires={fired}; counts={:?}",
            live_report.trait_fire_counts
        );
        assert!(
            live.has_buff("Arcane Shield") || live.has_defense(CoverKind::Block),
            "lesser Arcane Shield effects must land"
        );

        let mut inactive = Timeline::new(
            &skills,
            &params,
            profile(4_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        inactive.run();
        let inactive_report = inactive.report();
        assert_eq!(
            inactive_report
                .trait_fire_counts
                .get("Final Shielding")
                .copied()
                .unwrap_or(0),
            0
        );
        assert!(
            !inactive.has_buff("Arcane Shield") && !inactive.has_defense(CoverKind::Block),
            "no trait => no lesser Arcane Shield"
        );

        // ICD: two elites inside 300 s ICD => one Final Shielding fire.
        assert_eq!(
            fired, 1,
            "Final Shielding ICD 300 s must hold to one fire; got {fired}"
        );

        let executing = ["Final Shielding", "Defy Pain", "Protector's Restoration"]
            .iter()
            .filter(|n| live_report.trait_fire_counts.get(**n).copied().unwrap_or(0) >= 1)
            .count();
        assert!(
            executing >= 3,
            ">=3 formerly NeedsMechanic trait-skill traits must execute; got {executing}; {:?}",
            live_report.trait_fire_counts
        );
    }

    /// E3 Kent: Fire->Water changes current; swap-to-Air fires One with Air only;
    /// while-Earth off in Fire / on in Earth; no-trait = 0.
    #[test]
    fn kent_e3_causal_attunement_swap_fires_scoped_traits() {
        use crate::data::normalized_effects::{effects, SourceType};

        let effects_wvw = effects().effects_for_mode("WvW");
        let attune_records: Vec<&_> = effects_wvw
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait
                    && e.trigger_rule == TriggerRule::OnAttunementSwap
                    && e.coverage.is_none()
                    && matches!(e.source_id, 224 | 268 | 281)
            })
            .collect();
        assert!(
            attune_records.len() >= 3,
            "E3 must ship >=3 executable OnAttunementSwap trait records; got {}",
            attune_records.len()
        );

        // Micro-proof on AttunementState alone (Kent bars).
        let mut state = AttunementState::new();
        let mut bus = TriggerBus::new();
        assert!(state.is(Element::Fire));
        assert!(!state.is(Element::Earth));
        assert!(apply_attunement_skill(&mut state, &mut bus, 0, "Water Attunement").is_some());
        assert!(state.is(Element::Water));
        assert_eq!(bus.count(BusEvent::OnAttunementSwap), 1);
        assert!(!state.is(Element::Earth));
        assert!(apply_attunement_skill(&mut state, &mut bus, 50, "Earth Attunement").is_some());
        assert!(state.is(Element::Earth));

        let skills = [
            skill(5493, SkillSlot::Profession, 0, 0, vec![]),
            skill(5494, SkillSlot::Profession, 0, 1_000, vec![]),
            skill(5495, SkillSlot::Profession, 0, 1_000, vec![]),
            skill(
                1,
                SkillSlot::Weapon1,
                250,
                0,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 1.0,
                }],
            ),
        ];
        let mut named = skills;
        named[0].name = "Water Attunement".into();
        named[1].name = "Air Attunement".into();
        named[2].name = "Earth Attunement".into();
        let params = params();
        let active: Vec<&_> = attune_records;

        let opener_live = [5493u32, 5494, 5495];
        let mut live = Timeline::new(
            &named,
            &params,
            profile(6_000, vec![]),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );
        live.opener = &opener_live;
        live.run();
        let live_report = live.report();
        assert!(
            live.trigger_bus.count(BusEvent::OnAttunementSwap) >= 1,
            "attune skills must emit OnAttunementSwap"
        );
        assert_eq!(
            live_report.bus_on_attunement_swap,
            live.trigger_bus.count(BusEvent::OnAttunementSwap)
        );

        let air_fires = live_report
            .trait_fire_counts
            .get("One with Air")
            .copied()
            .unwrap_or(0);
        assert!(
            air_fires >= 1,
            "One with Air must fire on swap-to-Air; fires={air_fires}; counts={:?}",
            live_report.trait_fire_counts
        );
        // Buff durations (2-3 s) may expire before fight end; causal proof is
        // trait_fire_counts + bus emissions, not leftover buff leftovers.

        // Swap-to-Air scope: Rock Solid (Earth) must not fire from Air-only swap path
        // when Earth was never entered — but this opener enters Water/Air/Earth.
        // Prove Status("Air") gate: Water swap alone must not fire One with Air.
        let water_only = [named[0].clone()];
        let opener_water = [5493u32];
        let mut water_run = Timeline::new(
            &water_only,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );
        water_run.opener = &opener_water;
        water_run.run();
        let water_report = water_run.report();
        assert_eq!(
            water_report
                .trait_fire_counts
                .get("One with Air")
                .copied()
                .unwrap_or(0),
            0,
            "One with Air must not fire on Water swap"
        );
        assert!(
            water_report
                .trait_fire_counts
                .get("Arcane Prowess")
                .copied()
                .unwrap_or(0)
                >= 1,
            "Arcane Prowess (Any) must fire on Water swap"
        );

        let opener_inactive = [5493u32, 5494, 5495];
        let mut inactive = Timeline::new(
            &named,
            &params,
            profile(6_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        inactive.opener = &opener_inactive;
        inactive.run();
        let inactive_report = inactive.report();
        assert_eq!(
            inactive_report
                .trait_fire_counts
                .get("One with Air")
                .copied()
                .unwrap_or(0),
            0
        );
        assert_eq!(
            inactive_report
                .trait_fire_counts
                .get("Arcane Prowess")
                .copied()
                .unwrap_or(0),
            0
        );
        assert_eq!(
            inactive_report
                .trait_fire_counts
                .get("Rock Solid")
                .copied()
                .unwrap_or(0),
            0,
            "no-trait = 0"
        );

        let executing = ["One with Air", "Rock Solid", "Arcane Prowess"]
            .iter()
            .filter(|n| live_report.trait_fire_counts.get(**n).copied().unwrap_or(0) >= 1)
            .count();
        assert!(
            executing >= 3,
            ">=3 attunement traits must execute; got {executing}; {:?}",
            live_report.trait_fire_counts
        );
    }

    #[test]
    fn fcr006_weaver_timeline_stashes_outgoing_primary() {
        let mut water = skill(5493, SkillSlot::Profession, 0, 8_000, vec![]);
        water.name = "Water Attunement".into();
        let skills = [water];
        let mut params = params();
        params.weaver = true;
        let mut tl = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        assert!(tl.attunement.weaver);
        let outgoing = tl.attunement.current;
        let landed = apply_attunement_skill(
            &mut tl.attunement,
            &mut tl.trigger_bus,
            0,
            "Water Attunement",
        );
        assert_eq!(landed, Some(Element::Water));
        assert_eq!(tl.attunement.secondary, Some(outgoing));
    }

    /// FCR-004: `secondary` is read, not just written. A Weaver whose dual
    /// attunement still holds Fire can use a Fire-gated record after swapping
    /// to Water; a core Elementalist with the same swap history cannot. Both
    /// prerequisite surfaces (`prerequisite_holds` and the `PrerequisiteView`
    /// snapshot `update_conditionals` reads) must agree.
    #[test]
    fn fcr004_weaver_prerequisite_accepts_secondary_attunement() {
        let fire_gate = Prerequisite {
            attunement: Some("Fire".into()),
            ..Default::default()
        };
        let check = |weaver: bool| {
            let mut params = params();
            params.weaver = weaver;
            let mut tl = Timeline::new(
                &[],
                &params,
                profile(1_000, vec![]),
                open_enemy(false),
                &[],
                &[],
                true,
                Vec::new(),
            );
            assert!(tl.prerequisite_holds(&fire_gate).is_ok(), "starts on Fire");
            apply_attunement_skill(
                &mut tl.attunement,
                &mut tl.trigger_bus,
                0,
                "Water Attunement",
            );
            assert_eq!(tl.attunement.current, Element::Water);
            (
                tl.prerequisite_holds(&fire_gate).is_ok(),
                PrerequisiteView::of(&tl).is_ok_with(&fire_gate),
            )
        };

        assert_eq!(
            check(true),
            (true, true),
            "a Weaver is still attuned to the stashed Fire half"
        );
        assert_eq!(
            check(false),
            (false, false),
            "a core Elementalist dropped Fire when it swapped"
        );
    }

    #[test]
    fn fcr006_core_ele_timeline_secondary_stays_none() {
        let mut water = skill(5493, SkillSlot::Profession, 0, 8_000, vec![]);
        water.name = "Water Attunement".into();
        let skills = [water];
        let params = params();
        let mut tl = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        assert!(!tl.attunement.weaver);
        apply_attunement_skill(
            &mut tl.attunement,
            &mut tl.trigger_bus,
            0,
            "Water Attunement",
        );
        assert_eq!(tl.attunement.current, Element::Water);
        assert_eq!(tl.attunement.secondary, None);
    }

    /// E4 Kent: dodge 0->1 + emit; 4th spawn at cap no-op/no emit; heal-slot
    /// Ego Restoration; Compounding Power only on successful spawn; 710 NM.
    #[test]
    fn kent_e4_causal_mes_clones_spawn_and_traits() {
        use crate::data::normalized_effects::{effects, SourceType};
        use crate::rotation::illusion::{spawn_clone, IllusionState};

        let effects_wvw = effects().effects_for_mode("WvW");
        let clone_records: Vec<&_> = effects_wvw
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait
                    && e.coverage.is_none()
                    && matches!(e.source_id, 704 | 740 | 723)
            })
            .collect();
        assert!(
            clone_records.len() >= 3,
            "E4 must ship >=3 executable Mes clone trait records; got {}",
            clone_records.len()
        );
        let sharper = effects_wvw.iter().find(|e| e.source_id == 710);
        assert!(
            sharper.is_some_and(|e| {
                e.coverage.as_ref().is_some_and(|c| {
                    matches!(
                        c.class,
                        crate::data::normalized_effects::CoverageClass::NeedsMechanic
                    ) && c.mechanic.as_deref() == Some("clones")
                })
            }),
            "710 Sharper Images must remain NeedsMechanic: clones"
        );

        // Micro-proof: 0->1 emit; 4th at cap no-op/no emit.
        let mut state = IllusionState::new();
        let mut bus = TriggerBus::new();
        assert!(spawn_clone(&mut state, &mut bus, 0));
        assert_eq!(state.count, 1);
        assert_eq!(bus.count(BusEvent::OnCloneCreated), 1);
        assert!(spawn_clone(&mut state, &mut bus, 10));
        assert!(spawn_clone(&mut state, &mut bus, 20));
        assert_eq!(state.count, 3);
        assert!(!spawn_clone(&mut state, &mut bus, 30));
        assert_eq!(state.count, 3);
        assert_eq!(bus.count(BusEvent::OnCloneCreated), 3);

        let params = params();
        let active: Vec<&_> = clone_records;

        // Dodge path: Deceptive Evasion OnDodge -> spawn -> Compounding Power.
        let skills_dodge = [skill(
            1,
            SkillSlot::Weapon1,
            250,
            0,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let mut dodge_run = Timeline::new(
            &skills_dodge,
            &params,
            profile(12_000, strike_script(12_000, 1_000, 10.0)),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );
        dodge_run.run();
        let dodge_report = dodge_run.report();
        assert!(
            dodge_run.trigger_bus.count(BusEvent::OnDodge) >= 1,
            "endurance dodge must fire"
        );
        assert!(
            dodge_run.illusion.count >= 1,
            "704 must spawn a clone on dodge; count={}",
            dodge_run.illusion.count
        );
        assert!(
            dodge_run.trigger_bus.count(BusEvent::OnCloneCreated) >= 1,
            "successful spawn must emit OnCloneCreated"
        );
        assert_eq!(
            dodge_report.bus_on_clone_created,
            dodge_run.trigger_bus.count(BusEvent::OnCloneCreated)
        );
        let deceptive = dodge_report
            .trait_fire_counts
            .get("Deceptive Evasion")
            .copied()
            .unwrap_or(0);
        assert!(
            deceptive >= 1,
            "Deceptive Evasion must execute; fires={deceptive}; {:?}",
            dodge_report.trait_fire_counts
        );
        let compounding = dodge_report
            .trait_fire_counts
            .get("Compounding Power")
            .copied()
            .unwrap_or(0);
        assert!(
            compounding >= 1,
            "723 must buff only on successful spawn; fires={compounding}; {:?}",
            dodge_report.trait_fire_counts
        );

        // Cap no-op: force count=3 then spawn_clone must not emit / not fire 723 extra.
        let mut capped = IllusionState::new();
        capped.count = 3;
        let mut bus2 = TriggerBus::new();
        let before = bus2.count(BusEvent::OnCloneCreated);
        assert!(!spawn_clone(&mut capped, &mut bus2, 0));
        assert_eq!(bus2.count(BusEvent::OnCloneCreated), before);

        // Heal-slot: Ego Restoration OnSkillUse + Slot Heal -> spawn.
        let mut heal = skill(
            2,
            SkillSlot::Heal,
            500,
            1_000,
            vec![SkillEffect::Healing { hit_count: 1 }],
        );
        heal.name = "Mirror".into();
        heal.slot_name = Some("Heal".into());
        let skills_heal = [heal];
        let opener_heal = [2u32];
        let mut heal_run = Timeline::new(
            &skills_heal,
            &params,
            profile(4_000, vec![]),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );
        heal_run.opener = &opener_heal;
        heal_run.run();
        let heal_report = heal_run.report();
        assert!(
            heal_run.illusion.count >= 1,
            "740 must spawn on heal-slot use; count={}",
            heal_run.illusion.count
        );
        let ego = heal_report
            .trait_fire_counts
            .get("Ego Restoration")
            .copied()
            .unwrap_or(0);
        assert!(
            ego >= 1,
            "Ego Restoration must execute; fires={ego}; {:?}",
            heal_report.trait_fire_counts
        );

        let executing = ["Deceptive Evasion", "Ego Restoration", "Compounding Power"]
            .iter()
            .filter(|n| {
                dodge_report
                    .trait_fire_counts
                    .get(**n)
                    .copied()
                    .unwrap_or(0)
                    + heal_report.trait_fire_counts.get(**n).copied().unwrap_or(0)
                    > 0
            })
            .count();
        assert!(
            executing >= 3,
            ">=3 Mes clone traits must execute; dodge={:?} heal={:?}",
            dodge_report.trait_fire_counts,
            heal_report.trait_fire_counts
        );
    }

    /// E4 Kent: one successful clone + N ordinary hits must leave Compounding
    /// Power at 1 stack until the next successful spawn (not hit-rate fed).
    #[test]
    fn kent_e4_compounding_power_stacks_only_on_clone_spawn() {
        use crate::data::normalized_effects::{effects, SourceType};
        use crate::rotation::illusion::spawn_clone;

        let effects_wvw = effects().effects_for_mode("WvW");
        let active: Vec<&_> = effects_wvw
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait && e.coverage.is_none() && e.source_id == 723
            })
            .collect();
        assert!(!active.is_empty(), "723 Compounding Power must be present");

        let skills = [skill(
            1,
            SkillSlot::Weapon1,
            250,
            0,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut run = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &active,
            &[],
            true,
            Vec::new(),
        );

        let cp_stacks = |run: &Timeline<'_>| -> u32 {
            run.conditional_specs
                .iter()
                .filter(|s| s.source_name.eq_ignore_ascii_case("Compounding Power"))
                .map(|s| s.stacks)
                .max()
                .unwrap_or(0)
        };

        // First successful spawn installs 723 at 1 stack.
        assert!(spawn_clone(&mut run.illusion, &mut run.trigger_bus, 0));
        run.now_ms = 0;
        run.trigger_procs(TriggerRule::OnCloneCreated, Some(1), false, 1.0);
        assert_eq!(cp_stacks(&run), 1, "first spawn must install 1 stack");
        assert!(
            run.conditional_specs.iter().any(|s| {
                s.source_name.eq_ignore_ascii_case("Compounding Power")
                    && matches!(s.kind, ConditionalKind::Stacking { hit_fed: false, .. })
            }),
            "723 must be hit_fed=false"
        );

        // N ordinary hits must not mutate 723 stacks.
        for i in 1..=25 {
            run.now_ms = i * 100;
            run.gain_conditional_stacks(1);
        }
        assert_eq!(
            cp_stacks(&run),
            1,
            "ordinary hits must not raise 723; fires={:?}",
            run.report().trait_fire_counts
        );

        // Next successful spawn raises stacks to 2 (PIN: OnCloneCreated only).
        assert!(spawn_clone(&mut run.illusion, &mut run.trigger_bus, 3_000));
        run.now_ms = 3_000;
        run.trigger_procs(TriggerRule::OnCloneCreated, Some(1), false, 1.0);
        assert_eq!(
            cp_stacks(&run),
            2,
            "second successful spawn must raise 723 to 2 stacks"
        );
    }

    /// Immobilize / Immobilized must block dodge the same as canonical Immobile.
    #[test]
    fn immobile_aliases_block_dodge() {
        let params = params();
        for name in ["Immobile", "Immobilize", "Immobilized"] {
            let mut timeline = Timeline::new(
                &[],
                &params,
                // Reactive dodge: something has to be incoming to spend on.
                profile(1_000, strike_script(1_000, 500, 10.0)),
                open_enemy(false),
                &[],
                &[],
                true,
                Vec::new(),
            );
            assert!(timeline.endurance.can_dodge(DODGE_COST));
            timeline.incoming_conditions.push(TimedCondition {
                name: name.into(),
                stacks: 1,
                expires_at_ms: 5_000,
                next_tick_ms: 1_000,
            });
            timeline.tick_endurance_and_dodge();
            assert_eq!(timeline.dodge_action.dodges, 0, "{name} must block dodge");
            assert_eq!(
                timeline.trigger_bus.count(BusEvent::OnDodge),
                0,
                "{name} must not emit OnDodge"
            );
        }
        // Control: no immobilize -> dodge fires once.
        let mut clear = Timeline::new(
            &[],
            &params,
            profile(1_000, strike_script(1_000, 500, 10.0)),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        clear.tick_endurance_and_dodge();
        assert_eq!(clear.dodge_action.dodges, 1);
        assert_eq!(clear.trigger_bus.count(BusEvent::OnDodge), 1);
    }

    fn open_enemy(stability: bool) -> EnemyDummy {
        EnemyDummy {
            protection: false,
            stability,
            hp: Some(18_000.0),
        }
    }

    fn rule(
        skill_id: u32,
        kind: ResourceKind,
        cost: f64,
        gain_on_hit: f64,
        spend_all: bool,
    ) -> SkillResourceRule {
        SkillResourceRule {
            skill_id,
            kind,
            cost,
            gain_on_hit,
            spend_all,
            ..Default::default()
        }
    }

    #[test]
    fn short_block_does_not_complete_minimum_window() {
        let skills = vec![
            skill(
                1,
                SkillSlot::Utility,
                50,
                10_000,
                vec![SkillEffect::Cover {
                    kind: CoverKind::Block,
                    duration_ms: 1_000,
                    strippable: false,
                }],
            ),
            skill(
                2,
                SkillSlot::Weapon2,
                200,
                250,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 4.0,
                }],
            ),
        ];
        let params = params();
        let report = run_report(
            &skills,
            &[],
            open_enemy(false),
            profile(3_000, vec![]),
            &params,
        );

        assert!(report.longest_protected_window_ms < MIN_PROTECTED_WINDOW_MS);
        assert!(!report.chain_completed);
    }

    #[test]
    fn charge_cover_only_preserves_the_consumed_event_tick() {
        let skills = vec![skill(
            1,
            SkillSlot::Utility,
            50,
            10_000,
            vec![SkillEffect::Cover {
                kind: CoverKind::Aegis,
                duration_ms: 5_000,
                strippable: true,
            }],
        )];
        let events = vec![EnemyEvent {
            at_ms: 450,
            kind: EnemyEventKind::Strike {
                damage: 2_000.0,
                unblockable: false,
            },
        }];
        let params = params();
        let report = run_report(
            &skills,
            &[],
            open_enemy(false),
            profile(1_000, events),
            &params,
        );

        assert_eq!(report.longest_protected_window_ms, TIMELINE_TICK_MS);
        assert!(!report.chain_completed);
        assert_eq!(report.incoming_damage, 0.0);
    }

    #[test]
    fn target_stability_requires_strip_before_control() {
        let control = skill(
            2,
            SkillSlot::Utility,
            50,
            10_000,
            vec![SkillEffect::CrowdControl {
                kind: ControlKind::Stun,
                duration_ms: 1_000,
                stops_dodge: true,
            }],
        );
        let params = params();
        let blocked = run_report(
            std::slice::from_ref(&control),
            &[],
            open_enemy(true),
            profile(1_000, vec![]),
            &params,
        );
        assert_eq!(blocked.control_landed_ms, 0);

        let strip = skill(
            1,
            SkillSlot::Utility,
            50,
            10_000,
            vec![SkillEffect::StripBoons {
                count_per_pulse: 1,
                interval_ms: 0,
                window_ms: 0,
            }],
        );
        let opened = run_report(
            &[strip, control],
            &[],
            open_enemy(true),
            profile(1_000, vec![]),
            &params,
        );
        assert_eq!(opened.control_landed_ms, 1_000);
    }

    #[test]
    fn incoming_control_cancels_a_pending_cast() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            10_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 8.0,
            }],
        )];
        let events = vec![EnemyEvent {
            at_ms: 450,
            kind: EnemyEventKind::Control {
                duration_ms: 900,
                unblockable: false,
            },
        }];
        let params = params();
        let report = run_report(
            &skills,
            &[],
            open_enemy(false),
            profile(2_000, events),
            &params,
        );

        assert_eq!(report.interrupted_casts, 1);
        assert_eq!(report.total_damage, 0.0);
    }

    /// Chilled has to cost the player skill uptime, or it is just a word.
    ///
    /// It deals no damage at all, so nothing in the damage model can see it.
    /// What it does is keep the heal on cooldown until the next burst lands,
    /// which is the actual way a support dies, and the reason cleansing is
    /// worth a utility slot rather than a damage tax.
    #[test]
    fn chilled_keeps_a_skill_on_cooldown_longer() {
        // One skill, short cooldown, nothing else to do but recast it.
        let bar = vec![skill(1, SkillSlot::Weapon1, 200, 2_000, vec![])];
        let params = params();

        let clear = run_report(
            &bar,
            &[],
            open_enemy(false),
            profile(12_000, vec![]),
            &params,
        );
        let chilled = run_report(
            &bar,
            &[],
            open_enemy(false),
            profile(
                12_000,
                vec![EnemyEvent {
                    at_ms: 100,
                    kind: EnemyEventKind::Condition {
                        condition: "Chilled".into(),
                        stacks: 1,
                        duration_ms: 10_000,
                    },
                }],
            ),
            &params,
        );

        assert!(
            chilled.successful_action_count < clear.successful_action_count,
            "Chilled must cost casts over the same window - clear {}, chilled {}",
            clear.successful_action_count,
            chilled.successful_action_count
        );
    }

    /// Conditions have to outlast the lull, or cleansing is decoration.
    ///
    /// The applied duration used to be 4,000 ms against a burst period that is
    /// now 10,000 ms, so every condition expired on its own before the next
    /// burst. Waiting was a complete answer and a build with no cleanse
    /// measured exactly the same as one built around cleansing, which is the
    /// opposite of how condition damage works: armour does not reduce it,
    /// Protection does not reduce it, and an evade cannot dodge what is
    /// already ticking. Removal is the only counter.
    #[test]
    fn an_uncleansed_condition_costs_health_a_cleanse_saves() {
        let events = vec![EnemyEvent {
            at_ms: 200,
            kind: EnemyEventKind::Condition {
                condition: "Bleeding".into(),
                stacks: 8,
                duration_ms: CONDITION_DURATION_MS,
            },
        }];
        let params = params();

        let exposed = run_report(
            &[],
            &[],
            open_enemy(false),
            profile(CONDITION_DURATION_MS, events.clone()),
            &params,
        );
        assert!(
            exposed.incoming_damage > 0.0,
            "a condition nobody removes has to actually tick: {exposed:?}"
        );

        let cleanse = vec![skill(
            1,
            SkillSlot::Utility,
            50,
            600,
            vec![SkillEffect::RemovesCondition {
                conditions_removed: 3,
            }],
        )];
        let cleansed = run_report(
            &cleanse,
            &[],
            open_enemy(false),
            profile(CONDITION_DURATION_MS, events),
            &params,
        );

        assert!(
            cleansed.incoming_damage < exposed.incoming_damage,
            "cleansing has to cost the enemy damage - uncleansed {:.0}, cleansed {:.0}",
            exposed.incoming_damage,
            cleansed.incoming_damage
        );
        assert!(
            cleansed.remaining_health_ratio > exposed.remaining_health_ratio,
            "and has to show up as health left on the bar - uncleansed {:.0}%, cleansed {:.0}%",
            exposed.remaining_health_ratio * 100.0,
            cleansed.remaining_health_ratio * 100.0
        );
    }

    /// Pressure oscillates: ramp, peak, then a lull long enough to recover in.
    ///
    /// A flat script makes raw mitigation per second the only thing the
    /// sustain gates can see, and turns a longer window into nothing but more
    /// total damage - which is how a 20 s support window came to fail
    /// `SustainRecovery` for 86% of the published corpus while a 5 s DPS
    /// window passed 100% of it.
    #[test]
    fn the_generated_burst_peaks_and_then_lets_go() {
        let scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: CombatTier::Solo,
            combat_kind: CombatKind::Support,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "burst shape".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let params = params();
        let profile = WvwProfile::for_scenario(&scenario, &open_enemy(false), &params, 20_000);

        let strikes: Vec<(u32, f64)> = profile
            .enemy_events
            .iter()
            .filter_map(|e| match e.kind {
                EnemyEventKind::Strike { damage, .. } => Some((e.at_ms, damage)),
                _ => None,
            })
            .collect();
        assert!(
            strikes.len() >= 4,
            "a 20 s window has to carry more than one burst: {strikes:?}"
        );

        let biggest = strikes.iter().map(|(_, d)| *d).fold(0.0_f64, f64::max);
        let smallest = strikes.iter().map(|(_, d)| *d).fold(f64::MAX, f64::min);
        assert!(
            biggest >= smallest * 4.0,
            "the peak has to dwarf the chip or an evade spent on it buys nothing:              {smallest:.0} .. {biggest:.0}"
        );

        // The lull. Consecutive event gaps must include one long enough to
        // land a heal and have it matter.
        let mut times: Vec<u32> = profile.enemy_events.iter().map(|e| e.at_ms).collect();
        times.sort_unstable();
        let longest_gap = times.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
        assert!(
            longest_gap >= MIN_PROTECTED_WINDOW_MS,
            "no recovery window in the script: longest gap {longest_gap}ms"
        );
    }

    #[test]
    fn recovery_kit_outlasts_the_same_pressure_script() {
        let events = vec![
            EnemyEvent {
                at_ms: 400,
                kind: EnemyEventKind::Strike {
                    damage: 7_000.0,
                    unblockable: false,
                },
            },
            EnemyEvent {
                at_ms: 900,
                kind: EnemyEventKind::Strike {
                    damage: 7_000.0,
                    unblockable: false,
                },
            },
            EnemyEvent {
                at_ms: 1_400,
                kind: EnemyEventKind::Strike {
                    damage: 7_000.0,
                    unblockable: false,
                },
            },
        ];
        let params = params();
        let exposed = run_report(
            &[],
            &[],
            open_enemy(false),
            profile(2_000, events.clone()),
            &params,
        );
        assert!(!exposed.player_survived);

        let recovery = vec![
            skill(
                1,
                SkillSlot::Utility,
                50,
                600,
                vec![SkillEffect::Barrier { amount: 8_000.0 }],
            ),
            skill(
                2,
                SkillSlot::Heal,
                50,
                600,
                vec![
                    SkillEffect::Healing { hit_count: 2 },
                    SkillEffect::RemovesCondition {
                        conditions_removed: 2,
                    },
                ],
            ),
        ];
        let recovered = run_report(
            &recovery,
            &[],
            open_enemy(false),
            profile(2_000, events),
            &params,
        );
        assert!(recovered.player_survived);
        assert!(recovered.sustain_margin > 0.0);
    }

    fn published_fixture_profile() -> WvwProfile {
        profile(
            5_000,
            vec![
                EnemyEvent {
                    at_ms: 450,
                    kind: EnemyEventKind::Control {
                        duration_ms: 900,
                        unblockable: false,
                    },
                },
                EnemyEvent {
                    at_ms: 850,
                    kind: EnemyEventKind::Strike {
                        damage: 3_000.0,
                        unblockable: false,
                    },
                },
                EnemyEvent {
                    at_ms: 1_650,
                    kind: EnemyEventKind::Condition {
                        condition: "Bleeding".into(),
                        stacks: 2,
                        duration_ms: 3_000,
                    },
                },
                EnemyEvent {
                    at_ms: 2_350,
                    kind: EnemyEventKind::BoonStrip { count: 1 },
                },
                EnemyEvent {
                    at_ms: 3_050,
                    kind: EnemyEventKind::Control {
                        duration_ms: 1_100,
                        unblockable: false,
                    },
                },
            ],
        )
    }

    fn paper_pressure_sibling(skills: &[RotationSkill]) -> Vec<RotationSkill> {
        skills
            .iter()
            .cloned()
            .filter_map(|mut skill| {
                skill.effects.retain(|effect| {
                    !matches!(
                        effect,
                        SkillEffect::Cover { .. }
                            | SkillEffect::Mobility { .. }
                            | SkillEffect::CrowdControl { .. }
                    )
                });
                (!skill.effects.is_empty()).then_some(skill)
            })
            .collect()
    }

    fn assert_published_fixture(
        label: &str,
        skills: Vec<RotationSkill>,
        rules: Vec<SkillResourceRule>,
        target_starts_stable: bool,
    ) {
        let params = params();
        let report = run_report(
            &skills,
            &rules,
            open_enemy(target_starts_stable),
            published_fixture_profile(),
            &params,
        );
        let paper = paper_pressure_sibling(&skills);
        let paper_report = run_report(
            &paper,
            &rules,
            open_enemy(target_starts_stable),
            published_fixture_profile(),
            &params,
        );

        assert!(
            report.chain_completed,
            "{label} should complete its secured sequence: {report:?}"
        );
        assert!(
            report.resource_legal,
            "{label} should obey its resource ledger"
        );
        assert!(
            report.protected_damage > paper_report.protected_damage,
            "{label} should outperform its unprotected pressure sibling"
        );

        let rotation = super::super::SimulationResult {
            duration_ms: report.duration_ms,
            strike_dps: report.total_damage / 5.0,
            condition_dps: 0.0,
            total_dps: report.total_damage / 5.0,
            condition_uptime: HashMap::new(),
            buff_uptime: HashMap::new(),
            skill_usage: Vec::new(),
            stunbreak_count: 1,
            has_stability: false,
            stability_uptime: 0.0,
            cleanse_count: 1,
            cleanse_rate_per_20s: 4.0,
            healing_per_second: 0.0,
            control_uptime: 0.0,
            might_stacks_avg: 0.0,
            boon_equivalents: 0.0,
            has_mobility_out: true,
            escape_kinds: 1,
            has_strip: true,
            has_corrupt: false,
            downed: report.target_reached,
            finished: report.target_reached,
            has_interrupt: true,
            has_cover_answer: true,
            damage_per_second: Vec::new(),
            buff_presence_per_second: Default::default(),
            wvw: Some(report),
        };
        let combat = CombatPerformance {
            effective_health: 20_000.0,
            ..CombatPerformance::default()
        };
        let scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: CombatTier::Solo,
            combat_kind: CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: label.into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let viability = evaluate_viability_gates(Some(&rotation), &combat, &scenario);
        assert!(
            viability.is_viable,
            "{label} should pass the WvW gates: {:?}",
            viability.gates
        );
    }

    #[test]
    fn mobility_label_without_timed_cover_does_not_complete_sequence() {
        // A mobility tag says what a skill can do, not how long it protects an
        // action. The sourced D/P fixture belongs after its explicit WvW facts
        // and instant-during-cast ordering are represented.
        let skills = vec![
            skill(
                100,
                SkillSlot::Profession,
                50,
                3_000,
                vec![
                    SkillEffect::StealBoons,
                    SkillEffect::CrowdControl {
                        kind: ControlKind::Daze,
                        duration_ms: 750,
                        stops_dodge: false,
                    },
                ],
            ),
            skill(
                101,
                SkillSlot::Utility,
                50,
                10_000,
                vec![SkillEffect::Mobility {
                    kind: MobilityKind::Stealth,
                }],
            ),
            skill(
                102,
                SkillSlot::Weapon2,
                200,
                1_200,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 8.0,
                }],
            ),
        ];
        let report = run_report(
            &skills,
            &[rule(102, ResourceKind::Initiative, 3.0, 0.0, false)],
            open_enemy(false),
            published_fixture_profile(),
            &params(),
        );
        assert!(!report.chain_completed);
    }

    #[test]
    fn published_spellbreaker_fixture_beats_unprotected_pressure() {
        // MetaBattle Magebane/Spearbreaker roamers: remove target cover, then
        // combine Full Counter's block/control with weapon pressure.
        let skills = vec![
            skill(
                200,
                SkillSlot::Utility,
                50,
                10_000,
                vec![SkillEffect::StripBoons {
                    count_per_pulse: 2,
                    interval_ms: 0,
                    window_ms: 0,
                }],
            ),
            skill(
                201,
                SkillSlot::Profession,
                50,
                5_000,
                vec![
                    SkillEffect::Cover {
                        kind: CoverKind::Block,
                        duration_ms: 2_500,
                        strippable: false,
                    },
                    SkillEffect::CrowdControl {
                        kind: ControlKind::Stun,
                        duration_ms: 1_000,
                        stops_dodge: true,
                    },
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 8.0,
                    },
                ],
            ),
            skill(
                202,
                SkillSlot::Weapon2,
                200,
                700,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 8.0,
                }],
            ),
        ];
        assert_published_fixture(
            "Spellbreaker",
            skills,
            vec![rule(201, ResourceKind::Adrenaline, 10.0, 0.0, false)],
            true,
        );
    }

    #[test]
    fn long_cooldown_in_the_secured_sequence_is_still_repeatable() {
        // Seen in-game 2026-09-05 on 1.11.25: a Roam/Support Scourge left the
        // exchange alive at 87% health and was still judged not repeatable,
        // because its heal (25s) and elite (120s) sat inside the best secured
        // window and were not off cooldown 5s after a 20s fight. Every heal and
        // elite in the game trips that rule. Repeatable is about the player
        // coming out of the exchange able to fight again, not about the burst
        // being castable again 5s later.
        let skills = vec![
            skill(
                201,
                SkillSlot::Elite,
                50,
                120_000,
                vec![
                    SkillEffect::Cover {
                        kind: CoverKind::Block,
                        duration_ms: 2_500,
                        strippable: false,
                    },
                    SkillEffect::CrowdControl {
                        kind: ControlKind::Stun,
                        duration_ms: 1_000,
                        stops_dodge: true,
                    },
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 8.0,
                    },
                ],
            ),
            skill(
                202,
                SkillSlot::Weapon2,
                200,
                700,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 8.0,
                }],
            ),
        ];
        let params = params();
        let report = run_report(
            &skills,
            &[],
            open_enemy(false),
            published_fixture_profile(),
            &params,
        );
        assert!(report.chain_completed, "{report:?}");
        assert!(report.player_survived, "{report:?}");
        assert!(report.remaining_health_ratio >= 0.5, "{report:?}");
        assert!(
            report.repeatable,
            "alive at {:.0}% after a completed sequence must be repeatable: {report:?}",
            report.remaining_health_ratio * 100.0
        );
    }

    #[test]
    fn published_mirage_fixture_beats_unprotected_pressure() {
        // MetaBattle Shatter/Celestial Mirage roamers: generate an illusion,
        // use Distortion as duration cover, then apply the weapon sequence.
        let skills = vec![
            skill(
                300,
                SkillSlot::Weapon2,
                200,
                700,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 8.0,
                }],
            ),
            skill(
                301,
                SkillSlot::Profession,
                50,
                5_000,
                vec![SkillEffect::Cover {
                    kind: CoverKind::Invulnerability,
                    duration_ms: 2_500,
                    strippable: false,
                }],
            ),
        ];
        assert_published_fixture(
            "Mirage",
            skills,
            vec![
                rule(300, ResourceKind::Illusions, 0.0, 1.0, false),
                rule(301, ResourceKind::Illusions, 1.0, 0.0, true),
            ],
            false,
        );
    }

    #[test]
    fn published_virtuoso_fixture_beats_unprotected_pressure() {
        // MetaBattle Power Speartuoso: invulnerability provides the opening;
        // a blade builder makes the bladesong legal before the pressure lands.
        let skills = vec![
            skill(
                400,
                SkillSlot::Utility,
                50,
                10_000,
                vec![SkillEffect::Cover {
                    kind: CoverKind::Invulnerability,
                    duration_ms: 2_500,
                    strippable: false,
                }],
            ),
            skill(
                401,
                SkillSlot::Weapon2,
                200,
                700,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 8.0,
                }],
            ),
            skill(
                402,
                SkillSlot::Profession,
                200,
                5_000,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 10.0,
                }],
            ),
        ];
        assert_published_fixture(
            "Virtuoso",
            skills,
            vec![
                rule(401, ResourceKind::Blades, 0.0, 1.0, false),
                rule(402, ResourceKind::Blades, 1.0, 0.0, true),
            ],
            false,
        );
    }

    #[test]
    fn initiative_stops_after_the_available_pool_is_spent() {
        let rules = [rule(1, ResourceKind::Initiative, 4.0, 0.0, false)];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        for _ in 0..3 {
            assert!(timeline.can_pay_resource(1));
            timeline.pay_resource(1);
        }
        assert!(!timeline.can_pay_resource(1));
    }

    #[test]
    fn exact_duration_conditions_receive_their_final_tick() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.target.conditions.push(TimedFoeCondition {
            name: "Bleeding".into(),
            stacks: 1,
            expires_at_ms: 4_000,
            next_tick_ms: 1_000,
        });
        let one_tick = condition_tick_damage("Bleeding", params.condition_damage, &params.mode);
        for second in 1..=4 {
            timeline.now_ms = second * 1_000;
            timeline.tick_conditions();
        }

        let total: f64 = timeline
            .damage_events
            .iter()
            .map(|event| event.amount)
            .sum();
        assert!((total - one_tick * 4.0).abs() < 0.001);
        assert!(timeline.target.conditions.is_empty());
    }

    #[test]
    fn shared_expiry_vulnerability_applies_on_final_bleed_tick() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.target.conditions.push(TimedFoeCondition {
            name: "Bleeding".into(),
            stacks: 1,
            expires_at_ms: 2_000,
            next_tick_ms: 1_000,
        });
        timeline.target.conditions.push(TimedFoeCondition {
            name: "Vulnerability".into(),
            stacks: 25,
            expires_at_ms: 2_000,
            next_tick_ms: 1_000,
        });
        let one_tick = condition_tick_damage("Bleeding", params.condition_damage, &params.mode);
        let incoming = 1.0
            + 25.0
                * crate::data::boon_condition_formulas::conditions()
                    .vulnerability_incoming_pct_per_stack(&params.mode);
        for second in 1..=2 {
            timeline.now_ms = second * 1_000;
            timeline.tick_conditions();
        }
        let total: f64 = timeline
            .damage_events
            .iter()
            .map(|event| event.amount)
            .sum();
        assert!(
            (total - one_tick * 2.0 * incoming).abs() < 0.001,
            "1 bleed + 25 vuln expiring at 2000ms must pay 2*tick*incoming ({}) got {total}",
            one_tick * 2.0 * incoming
        );
        assert!(timeline.target.conditions.is_empty());
    }

    #[test]
    fn expired_vulnerability_does_not_buff_strike_landing_at_expiry() {
        let params = params();
        let land = |with_vuln: bool| {
            let mut timeline = Timeline::new(
                &[],
                &params,
                profile(5_000, vec![]),
                open_enemy(false),
                &[],
                &[],
                true,
                Vec::new(),
            );
            if with_vuln {
                timeline.target.conditions.push(TimedFoeCondition {
                    name: "Vulnerability".into(),
                    stacks: 25,
                    expires_at_ms: 1_000,
                    next_tick_ms: 1_000,
                });
            }
            timeline.scheduled_hits.push(ScheduledHit {
                at_ms: 1_000,
                skill_id: 1,
                dmg_multiplier: 1.0,
            });
            timeline.now_ms = 1_000;
            timeline.land_scheduled_hits();
            timeline
                .damage_events
                .iter()
                .map(|event| event.amount)
                .sum::<f64>()
        };
        let with_vuln = land(true);
        let bare = land(false);
        assert!(
            (with_vuln - bare).abs() < 0.001,
            "strike landing at Vulnerability.expires_at_ms must equal unbuffed strike: with_vuln={with_vuln} bare={bare}"
        );
        assert!(bare > 0.0);
    }

    #[test]
    fn deferred_target_condition_vs_vulnerability_adds_ten_percent() {
        let mut params = params();
        let seed = |timeline: &mut Timeline| {
            timeline.target.conditions.push(TimedFoeCondition {
                name: "Bleeding".into(),
                stacks: 1,
                expires_at_ms: 4_000,
                next_tick_ms: 1_000,
            });
            timeline.target.conditions.push(TimedFoeCondition {
                name: "Vulnerability".into(),
                stacks: 1,
                expires_at_ms: 4_000,
                next_tick_ms: 1_000,
            });
        };
        let run = |timeline: &mut Timeline| {
            for second in 1..=4 {
                timeline.now_ms = second * 1_000;
                timeline.tick_conditions();
            }
            timeline
                .damage_events
                .iter()
                .map(|event| event.amount)
                .sum::<f64>()
        };
        let mut baseline = Timeline::new(
            &[],
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        seed(&mut baseline);
        let base_total = run(&mut baseline);
        params.deferred_target = vec![crate::combat::DeferredTargetModifier {
            gate: crate::combat::TargetGate::Condition("Vulnerability"),
            percent: 10.0,
            axis: crate::combat::TargetModAxis::Condition,
        }];
        let mut boosted = Timeline::new(
            &[],
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        seed(&mut boosted);
        let boosted_total = run(&mut boosted);
        assert!(base_total > 0.0);
        assert!(
            (boosted_total - base_total * 1.10).abs() < 0.001,
            "+10% condition vs Vulnerability must raise condi 1.10x: base {base_total} boosted {boosted_total}"
        );
    }

    #[test]
    fn apply_outgoing_condition_respects_burning_stack_cap() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        let cap = crate::rotation::simulator::condition_stack_cap("Burning", &params.mode) as u32;
        timeline.apply_outgoing_condition("Burning", cap + 1, 2_000, None, false);
        assert_eq!(timeline.target.stacks_of("Burning", timeline.now_ms), cap);
    }

    /// FCR-001: an application that the ledger clamps away (a cap-1 refresh, or
    /// Vulnerability past 25) must still fire `OnConditionApplied` records.
    #[test]
    fn apply_outgoing_condition_fires_trigger_at_stack_cap() {
        use crate::data::normalized_effects::TriggerScope;
        let params = params();
        let scoped = |id: u32, name: &str, status: &str| {
            let mut effect = crate::rotation::reaper_fixture::record(
                SourceType::Trait,
                id,
                name,
                EffectCategory::AppliesBoon,
                1.0,
                TriggerRule::OnConditionApplied,
            );
            effect.trigger_scope = Some(TriggerScope::Status(status.into()));
            effect
        };
        let chill_rec = scoped(50_010, "On Chill", "Chilled");
        let vuln_rec = scoped(50_011, "On Vuln", "Vulnerability");
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[&chill_rec, &vuln_rec],
            &[],
            true,
            Vec::new(),
        );
        timeline.trace_enabled = true;
        let fired = |t: &Timeline, name: &str| {
            t.trace
                .iter()
                .filter(|e| e.kind == TraceKind::TraitFired && e.source == name)
                .count()
        };
        // Chilled is max_stacks 1: the second apply is a refresh, not a no-op.
        timeline.apply_outgoing_condition("Chilled", 1, 3_000, None, false);
        timeline.apply_outgoing_condition("Chilled", 1, 3_000, None, false);
        assert_eq!(
            fired(&timeline, "On Chill"),
            2,
            "a refresh at the cap still fires OnConditionApplied: {:?}",
            timeline.trace
        );
        let vuln_cap =
            crate::rotation::simulator::condition_stack_cap("Vulnerability", &params.mode) as u32;
        timeline.apply_outgoing_condition("Vulnerability", vuln_cap, 3_000, None, false);
        timeline.apply_outgoing_condition("Vulnerability", 1, 3_000, None, false);
        assert_eq!(
            fired(&timeline, "On Vuln"),
            2,
            "the apply past 25 stacks still fires: {:?}",
            timeline.trace
        );
        assert_eq!(
            timeline.target.stacks_of("Vulnerability", timeline.now_ms),
            vuln_cap,
            "the ledger stays clamped at the cap"
        );
    }

    #[test]
    fn apply_outgoing_condition_chill_alias_satisfies_foe_prereq() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.apply_outgoing_condition("Chill", 1, 2_000, None, false);
        let chill = Prerequisite {
            foe_condition: Some("Chill".into()),
            ..Default::default()
        };
        let chilled = Prerequisite {
            foe_condition: Some("Chilled".into()),
            ..Default::default()
        };
        assert!(
            timeline.prerequisite_holds(&chill).is_ok(),
            "alias 'Chill' must match stored canonical Chill"
        );
        assert!(
            timeline.prerequisite_holds(&chilled).is_ok(),
            "canonical 'Chilled' must match after apply_outgoing_condition('Chill')"
        );
        assert!(
            timeline
                .prerequisite_holds(&Prerequisite {
                    foe_condition: Some("chilled".into()),
                    ..Default::default()
                })
                .is_ok(),
            "lowercase canonical 'chilled' must keep matching"
        );
        let view = PrerequisiteView::of(&timeline);
        assert!(view.is_ok_with(&chill));
        assert!(view.is_ok_with(&chilled));
        assert!(view.foe_stacks("Chill") >= 1);
        assert!(view.foe_stacks("Chilled") >= 1);
    }

    /// FCR-003: the endurance dodge must mitigate on the profile the engine
    /// actually ships (`WvwProfile::for_scenario`), not only on a hand-built
    /// script. The control run starts with an empty pool, so the only
    /// difference between the two is whether dodges were affordable.
    #[test]
    fn production_profile_dodge_avoids_damage() {
        let scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: CombatTier::Solo,
            combat_kind: CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "dodge mitigation".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let params = params();
        let run = |endurance: f64| {
            let mut timeline = Timeline::new(
                &[],
                &params,
                WvwProfile::for_scenario(&scenario, &open_enemy(false), &params, 12_000),
                open_enemy(false),
                &[],
                &[],
                true,
                Vec::new(),
            );
            timeline.endurance.current = endurance;
            timeline.trace_enabled = true;
            timeline.run();
            timeline
        };

        let dodging = run(100.0);
        let drained = run(0.0);

        assert!(
            dodging.dodge_action.dodges > 0,
            "the production profile must give the reactive dodge something to spend on"
        );
        assert!(
            dodging.trace.iter().any(|e| e.kind == TraceKind::Dodged),
            "a spent dodge must be traced"
        );
        assert!(
            dodging.avoided_damage > drained.avoided_damage,
            "dodges must credit avoided_damage: {} with endurance vs {} without",
            dodging.avoided_damage,
            drained.avoided_damage
        );
        assert!(
            dodging.incoming_damage < drained.incoming_damage,
            "an evaded strike must not be taken: {} vs {}",
            dodging.incoming_damage,
            drained.incoming_damage
        );
    }

    /// FCR-003: endurance is held while nothing is incoming — a dodge spent on
    /// an empty window buys no evade frames.
    #[test]
    fn dodge_is_held_when_nothing_is_incoming() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.run();
        assert_eq!(
            timeline.dodge_action.dodges, 0,
            "no enemy events means no reason to spend endurance"
        );
    }

    #[test]
    fn dodge_evade_avoids_strike_on_dodge_tick() {
        let params = params();
        let strike = 1_000.0;
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(
                100,
                vec![EnemyEvent {
                    at_ms: 0,
                    kind: EnemyEventKind::Strike {
                        damage: strike,
                        unblockable: false,
                    },
                }],
            ),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.run();
        assert!(
            timeline.avoided_damage > 0.0,
            "strike on dodge tick must credit avoided_damage; got {}",
            timeline.avoided_damage
        );
        assert_eq!(
            timeline.incoming_damage, 0.0,
            "evaded strike must not increment incoming_damage"
        );
    }

    #[test]
    fn kent_e0_causal_health_threshold_re_arms_above_threshold() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.player_health = params.max_health;
        timeline.tick_health_threshold_bus();
        assert_eq!(timeline.trigger_bus.count(BusEvent::OnThreshold), 0);
        timeline.player_health = params.max_health * 0.50;
        timeline.tick_health_threshold_bus();
        assert_eq!(
            timeline.trigger_bus.count(BusEvent::OnThreshold),
            1,
            "crossing 50% must emit OnThreshold once"
        );
        timeline.tick_health_threshold_bus();
        timeline.player_health = params.max_health * 0.10;
        timeline.tick_health_threshold_bus();
        assert_eq!(
            timeline.trigger_bus.count(BusEvent::OnThreshold),
            1,
            "OnThreshold must not re-emit while health stays below the threshold"
        );
        // FCR-010: healing back above 50% re-arms the latch.
        timeline.player_health = params.max_health * 0.90;
        timeline.tick_health_threshold_bus();
        assert_eq!(
            timeline.trigger_bus.count(BusEvent::OnThreshold),
            1,
            "recovering above the threshold must not emit"
        );
        timeline.player_health = params.max_health * 0.40;
        timeline.tick_health_threshold_bus();
        assert_eq!(
            timeline.trigger_bus.count(BusEvent::OnThreshold),
            2,
            "a later drop below the threshold emits again"
        );
    }

    #[test]
    fn resistance_does_not_remove_damaging_conditions() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.apply_defense(CoverKind::Resistance, 2_000, 1, false);
        timeline.receive_condition("Burning".into(), 1, 1_000);
        timeline.receive_condition("Crippled".into(), 1, 1_000);

        assert_eq!(timeline.incoming_conditions.len(), 1);
        assert_eq!(timeline.incoming_conditions[0].name, "Burning");
    }

    #[test]
    fn reactive_stunbreak_must_pay_its_resource_cost() {
        let mut stunbreak = skill(1, SkillSlot::Utility, 0, 10_000, vec![]);
        stunbreak.is_stunbreak = true;
        let skills = [stunbreak];
        let rules = [rule(1, ResourceKind::Energy, 60.0, 0.0, false)];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        timeline.disabled_until_ms = 1_000;
        timeline.try_stunbreak();

        assert_eq!(timeline.disabled_until_ms, 1_000);
        assert_eq!(timeline.resources[&ResourceKind::Energy], 50.0);
        assert!(timeline.resource_blocked_skills.contains(&1));
    }

    #[test]
    fn skill_owned_proc_only_runs_for_its_source_skill() {
        let data = crate::data::normalized_effects::effects();
        let effect = data
            .effects_for_mode("WvW")
            .iter()
            .find(|effect| effect.source_id == 9120)
            .expect("Virtue of Resolve normalized effect");
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[effect],
            &[],
            true,
            Vec::new(),
        );
        timeline.incoming_conditions.push(TimedCondition {
            name: "Burning".into(),
            stacks: 1,
            expires_at_ms: 2_000,
            next_tick_ms: 1_000,
        });

        timeline.trigger_procs(TriggerRule::OnSkillUse, Some(1), false, 1.0);
        assert_eq!(timeline.incoming_conditions.len(), 1);
        timeline.trigger_procs(TriggerRule::OnSkillUse, Some(9120), false, 1.0);
        assert!(timeline.incoming_conditions.is_empty());
    }

    #[test]
    fn unsupported_normalized_trigger_degrades_coverage() {
        // A `Conditional` record without a health threshold has no firing
        // site (on-crit gained one in Sprint 2): it is named, never fired.
        let data = crate::data::normalized_effects::effects();
        let mut effect = data
            .effects_for_mode("WvW")
            .iter()
            .find(|effect| matches!(effect.trigger_rule, TriggerRule::OnCrit))
            .expect("OnCrit normalized effect")
            .clone();
        effect.trigger_rule = TriggerRule::Conditional;
        effect.health_threshold = None;
        let params = params();
        let timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[&effect],
            &[],
            true,
            Vec::new(),
        );

        assert_eq!(timeline.unmodeled_names.len(), 1);
        assert!(timeline.proc_specs.is_empty());
    }

    fn test_proc_effect(
        source_id: u32,
        category: EffectCategory,
        duration: Option<f64>,
    ) -> NormalizedEffect {
        use crate::data::normalized_effects::{StackingRule, UptimeModel, UptimeModelKind};
        use crate::data::EvidenceLevel;
        NormalizedEffect {
            effect_id: format!("test-proc-{source_id}"),
            source_type: SourceType::Relic,
            source_id,
            source_name: "test".into(),
            category,
            value: FactualValue::Resolved(10.0),
            stacking_rule: StackingRule::NonStacking,
            trigger_rule: TriggerRule::OnHit,
            uptime_model: UptimeModel {
                kind: UptimeModelKind::Unknown,
                uptime: None,
            },
            evidence_level: EvidenceLevel::Unknown,
            source: None,
            effect_duration: duration.map(FactualValue::Resolved),
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

    // Sprint 4 (sprints/008-data-driven-simulator, Gate 1): gates, scaling,
    // actors and stacking, each on a synthetic record over a synthetic fight.

    /// A heal record: `Heal` is the shortest category with a number the fight
    /// report carries, so "did it fire, and with what value" is one assertion.
    fn gated_heal(source_id: u32, trigger: TriggerRule, gates: Vec<Gate>) -> NormalizedEffect {
        let mut effect = test_proc_effect(source_id, EffectCategory::Heal, None);
        effect.source_name = format!("Gated {source_id}");
        effect.trigger_rule = trigger;
        effect.gates = gates;
        effect
    }

    fn gate_timeline<'a>(
        params: &'a SimParams,
        effects: &[&'a NormalizedEffect],
        duration_ms: u32,
        events: Vec<EnemyEvent>,
    ) -> Timeline<'a> {
        let mut timeline = Timeline::new(
            &[],
            params,
            profile(duration_ms, events),
            open_enemy(true),
            effects,
            &[],
            true,
            Vec::new(),
        );
        timeline.trace_enabled = true;
        timeline
    }

    fn fires(timeline: &Timeline<'_>, source_id: u32) -> u32 {
        timeline
            .proc_fire_counts
            .get(&format!("Gated {source_id}"))
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn in_combat_gate_holds_until_the_fight_starts() {
        let params = params();
        // Nothing is cast and nothing lands: the player never enters combat.
        let quiet = gated_heal(1, TriggerRule::Periodic, vec![Gate::InCombat]);
        let mut timeline = gate_timeline(&params, &[&quiet], 4_000, vec![]);
        timeline.run();
        assert_eq!(
            fires(&timeline, 1),
            0,
            "an out-of-combat gate must not fire"
        );

        // Positive control: the same record once the enemy opens.
        let loud = gated_heal(2, TriggerRule::Periodic, vec![Gate::InCombat]);
        let mut timeline = gate_timeline(
            &params,
            &[&loud],
            4_000,
            vec![EnemyEvent {
                at_ms: 1_000,
                kind: EnemyEventKind::Strike {
                    damage: 100.0,
                    unblockable: false,
                },
            }],
        );
        timeline.run();
        assert!(
            fires(&timeline, 2) > 0,
            "the gate must open once the fight starts"
        );
    }

    #[test]
    fn interval_gate_fires_three_times_in_ten_seconds() {
        let params = params();
        let every_3s = gated_heal(
            3,
            TriggerRule::Periodic,
            vec![Gate::Interval {
                every_ms: 3_000,
                while_state: None,
            }],
        );
        let mut timeline = gate_timeline(&params, &[&every_3s], 10_000, vec![]);
        timeline.run();
        // The gate opens after one full period, so 3 s, 6 s and 9 s.
        assert_eq!(fires(&timeline, 3), 3);
    }

    #[test]
    fn weapon_gate_reads_the_held_set() {
        let params = params();
        let with_torch = gated_heal(
            4,
            TriggerRule::OnHit,
            vec![Gate::Weapon {
                types: vec!["Torch".into()],
                hand: Some(WeaponHand::Off),
            }],
        );
        let mut timeline = gate_timeline(&params, &[&with_torch], 2_000, vec![]);
        timeline.equipped_weapons = vec![EquippedWeapon {
            set: 1,
            hand: WeaponHand::TwoHand,
            weapon_type: "greatsword".into(),
        }];
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 4), 0, "a greatsword is not a torch");

        timeline.equipped_weapons.push(EquippedWeapon {
            set: 1,
            hand: WeaponHand::Off,
            weapon_type: "torch".into(),
        });
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 4), 1, "the torch opens the gate");
    }

    /// The producer and the gate, end to end: a build's weapon slots become
    /// `EquippedWeapon` rows, and a record gated on an off-hand torch opens only
    /// while the set carrying it is held.
    #[test]
    fn equipped_weapons_feed_the_weapon_gate() {
        use crate::validation::ValidatedBuild;

        let mut build = ValidatedBuild::default();
        build.weapons.set1.main_hand = Some("Greatsword".into());
        build.weapons.set2.main_hand = Some("Sword".into());
        build.weapons.set2.off_hand = Some("Torch".into());

        let worn = crate::engine::equipped_weapons(&build, None);
        assert_eq!(
            worn,
            vec![
                EquippedWeapon {
                    set: 1,
                    hand: WeaponHand::TwoHand,
                    weapon_type: "greatsword".into(),
                },
                EquippedWeapon {
                    set: 2,
                    hand: WeaponHand::Main,
                    weapon_type: "sword".into(),
                },
                EquippedWeapon {
                    set: 2,
                    hand: WeaponHand::Off,
                    weapon_type: "torch".into(),
                },
            ],
            "a two-hander fills the main-hand cell and reports as TwoHand; an \
             empty off-hand produces no row"
        );

        let params = params();
        let with_torch = gated_heal(
            33,
            TriggerRule::OnHit,
            vec![Gate::Weapon {
                types: vec!["Torch".into()],
                hand: Some(WeaponHand::Off),
            }],
        );
        let mut timeline = gate_timeline(&params, &[&with_torch], 2_000, vec![]);
        timeline.equipped_weapons = worn;

        timeline.active_weapon_set = 1;
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 33), 0, "the greatsword set has no torch");

        timeline.active_weapon_set = 2;
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(
            fires(&timeline, 33),
            1,
            "the sword/torch set opens the gate"
        );
    }

    /// Two-handedness is the profession's answer, not a global type list:
    /// Bladesworn holds its Sword in both hands where core Warrior does not.
    #[test]
    fn a_bladesworn_sword_is_two_handed_for_the_gate() {
        use crate::validation::ValidatedBuild;
        use gw2_api::models::{Profession, WeaponInfo};

        let bladesworn = Profession {
            id: "Warrior".into(),
            name: "Warrior".into(),
            code: None,
            specializations: vec![],
            weapons: std::iter::once((
                "Sword".to_string(),
                WeaponInfo {
                    specialization: Some(68),
                    flags: vec!["TwoHand".into(), "Mainhand".into()],
                    skills: vec![],
                },
            ))
            .collect(),
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };

        let mut build = ValidatedBuild::default();
        build.weapons.set1.main_hand = Some("Sword".into());

        assert_eq!(
            crate::engine::equipped_weapons(&build, Some(&bladesworn)),
            vec![EquippedWeapon {
                set: 1,
                hand: WeaponHand::TwoHand,
                weapon_type: "sword".into(),
            }]
        );
        assert_eq!(
            crate::engine::equipped_weapons(&build, None)[0].hand,
            WeaponHand::Main,
            "without the profession table a Sword is the one-handed default"
        );
    }

    #[test]
    fn health_gate_with_when_recovered_fires_on_each_crossing() {
        let params = params();
        let low = gated_heal(
            5,
            TriggerRule::OnHit,
            vec![Gate::HealthThreshold {
                below_pct: Some(50.0),
                above_pct: None,
                rearm: Rearm::WhenRecovered,
            }],
        );
        let mut timeline = gate_timeline(&params, &[&low], 2_000, vec![]);

        timeline.player_health = params.max_health * 0.4;
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 5), 1, "latched after the first crossing");

        // Back above the line, then below it again.
        timeline.player_health = params.max_health;
        timeline.rearm_health_gates();
        timeline.player_health = params.max_health * 0.4;
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 5), 2, "re-armed by the recovery");
    }

    #[test]
    fn health_gate_once_per_fight_never_rearms() {
        let params = params();
        let once = gated_heal(
            6,
            TriggerRule::OnHit,
            vec![Gate::HealthThreshold {
                below_pct: Some(50.0),
                above_pct: None,
                rearm: Rearm::OncePerFight,
            }],
        );
        let mut timeline = gate_timeline(&params, &[&once], 2_000, vec![]);
        timeline.player_health = params.max_health * 0.4;
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        timeline.player_health = params.max_health;
        timeline.rearm_health_gates();
        timeline.player_health = params.max_health * 0.4;
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 6), 1);
    }

    #[test]
    fn self_resource_stacks_gate_and_scale_read_the_pool() {
        let params = params();
        let mut record = gated_heal(
            7,
            TriggerRule::OnHit,
            vec![Gate::SelfResourceStacks {
                resource: "initiative".into(),
                min: 3,
            }],
        );
        record.value = FactualValue::Resolved(0.0);
        record.scale = Some(Scale::PerSelfResourceStack {
            resource: "initiative".into(),
            per_stack: 100.0,
            cap: Some(5.0),
        });
        let mut timeline = gate_timeline(&params, &[&record], 2_000, vec![]);

        timeline.resources.insert(ResourceKind::Initiative, 2.0);
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 7), 0, "two initiative is under the gate");

        timeline.player_health = params.max_health / 2.0;
        timeline.resources.insert(ResourceKind::Initiative, 8.0);
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 7), 1);
        // Eight initiative, capped at five: 0 + 5 x 100.
        assert!(
            (timeline.healing - 500.0).abs() < 1e-6,
            "scaled heal was {}",
            timeline.healing
        );
    }

    #[test]
    fn self_boon_gate_and_per_boon_scale_read_the_buff_bar() {
        let params = params();
        let mut record = gated_heal(
            8,
            TriggerRule::OnHit,
            vec![Gate::SelfBoon {
                boon: "Might".into(),
            }],
        );
        record.value = FactualValue::Resolved(0.0);
        record.scale = Some(Scale::PerSelfBoon {
            boon: "Might".into(),
            per_stack: 10.0,
            cap: None,
        });
        let mut timeline = gate_timeline(&params, &[&record], 2_000, vec![]);
        timeline.player_health = params.max_health / 2.0;

        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 8), 0, "no Might, no fire");

        timeline.buffs.push(TimedBuff {
            name: "Might".into(),
            stacks: 7,
            expires_at_ms: 10_000,
        });
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert_eq!(fires(&timeline, 8), 1);
        assert!((timeline.healing - 70.0).abs() < 1e-6);
    }

    #[test]
    fn a_pet_record_never_fires_off_a_player_event() {
        let params = params();
        let mut pet = gated_heal(9, TriggerRule::OnCrit, vec![]);
        pet.actor = Actor::Pet;
        let mut timeline = gate_timeline(&params, &[&pet], 2_000, vec![]);
        assert!(
            timeline.proc_specs.is_empty(),
            "a pet record must not join the player's proc list"
        );
        timeline.trigger_procs(TriggerRule::OnCrit, None, false, 1.0);
        assert_eq!(fires(&timeline, 9), 0);
        assert!(
            timeline
                .unmodeled_names
                .iter()
                .any(|n| n.contains("event not yet emitted: Pet actor")),
            "the abstention must name the missing event: {:?}",
            timeline.unmodeled_names
        );
    }

    #[test]
    fn triggers_without_an_event_source_abstain_by_name() {
        let params = params();
        for (id, trigger, wanted) in [
            (20, TriggerRule::OnBlock, "on-block"),
            (21, TriggerRule::OnSteal, "on-steal"),
            (22, TriggerRule::OnStealthEnter, "on-stealth-enter"),
            (23, TriggerRule::OnStealthExit, "on-stealth-exit"),
            (24, TriggerRule::OnBerserkEnter, "on-berserk-enter"),
            (25, TriggerRule::OnSymbolHit, "on-symbol-hit"),
            (26, TriggerRule::OnExplosion, "on-explosion"),
        ] {
            let record = gated_heal(id, trigger, vec![]);
            let timeline = gate_timeline(&params, &[&record], 1_000, vec![]);
            assert!(
                timeline.proc_specs.is_empty(),
                "{wanted} has no firing site yet"
            );
            assert!(
                timeline
                    .unmodeled_names
                    .iter()
                    .any(|n| n.contains(&format!("event not yet emitted: {wanted}"))),
                "{wanted} must abstain by name: {:?}",
                timeline.unmodeled_names
            );
        }
    }

    #[test]
    fn positional_and_distance_abstain_rather_than_pass() {
        let params = params();
        let flank = gated_heal(
            27,
            TriggerRule::OnHit,
            vec![Gate::Positional(Positional::Flank)],
        );
        let timeline = gate_timeline(&params, &[&flank], 1_000, vec![]);
        assert!(timeline
            .unmodeled_names
            .iter()
            .any(|n| n.contains("gate not yet modelled: player facing")));

        let mut ranged = gated_heal(28, TriggerRule::OnHit, vec![]);
        ranged.scale = Some(Scale::PerDistance {
            per_unit: 0.01,
            cap: Some(600.0),
        });
        let timeline = gate_timeline(&params, &[&ranged], 1_000, vec![]);
        assert!(timeline
            .unmodeled_names
            .iter()
            .any(|n| n.contains("scale not yet modelled: foe distance")));

        let unknown_pool = gated_heal(
            29,
            TriggerRule::OnHit,
            vec![Gate::SelfResourceStacks {
                resource: "blight".into(),
                min: 1,
            }],
        );
        let timeline = gate_timeline(&params, &[&unknown_pool], 1_000, vec![]);
        assert!(
            timeline
                .unmodeled_names
                .iter()
                .any(|n| n.contains("resource not yet modelled: blight")),
            "{:?}",
            timeline.unmodeled_names
        );
    }

    #[test]
    fn refresh_all_stacks_refreshes_every_stack_expiry() {
        let mut spec = ConditionalSpec {
            source_name: "Lethal Tempo".into(),
            kind: ConditionalKind::Stacking {
                max: 5,
                duration_ms: 4_000,
                scope: Default::default(),
                hit_fed: true,
            },
            percent: 2.0,
            crit_damage: false,
            crit_chance: false,
            condition_damage: false,
            active: false,
            stacks: 0,
            expires_at_ms: 0,
            stack_expiries: Vec::new(),
            stacking_rule: StackingRule::RefreshAllStacks,
        };
        gain_stack(&mut spec, 0, 4_000, 5);
        gain_stack(&mut spec, 2_000, 4_000, 5);
        assert_eq!(spec.stacks, 2);
        assert_eq!(
            spec.stack_expiries,
            vec![6_000, 6_000],
            "the earlier stack must be pushed out with the new one"
        );

        // Any other rule leaves each stack on its own clock.
        let mut own_clock = spec;
        own_clock.stacking_rule = StackingRule::Multiplicative;
        own_clock.stacks = 0;
        own_clock.stack_expiries.clear();
        gain_stack(&mut own_clock, 0, 4_000, 5);
        gain_stack(&mut own_clock, 2_000, 4_000, 5);
        assert_eq!(own_clock.stack_expiries, vec![4_000, 6_000]);
    }

    /// Wiki `Effect stacking`, stacking intensity: "each stack is applied
    /// separately and has its own duration", so stacks drop one at a time.
    /// Before Sprint 4 the model kept one shared expiry, which made five stacks
    /// live as long as the newest and then vanish together — Relic of the Thief
    /// measured 0.6 % more damage than it should have on the Reaper fixture.
    /// Only `RefreshAllStacks` (Lethal Tempo, whose wiki line says gaining a
    /// stack refreshes the others) gets the old behaviour, and it must say so.
    #[test]
    fn stacks_expire_one_at_a_time_unless_the_record_says_otherwise() {
        let mut spec = ConditionalSpec {
            source_name: "Relic of the Thief".into(),
            kind: ConditionalKind::Stacking {
                max: 5,
                duration_ms: 6_000,
                scope: Default::default(),
                hit_fed: true,
            },
            percent: 3.0,
            crit_damage: false,
            crit_chance: false,
            condition_damage: false,
            active: false,
            stacks: 0,
            expires_at_ms: 0,
            stack_expiries: Vec::new(),
            // The shipped record: Multiplicative is how the percent combines,
            // and it says nothing about duration, so each stack runs its own.
            stacking_rule: StackingRule::Multiplicative,
        };
        for at in [0, 1_000, 2_000] {
            gain_stack(&mut spec, at, 6_000, 5);
        }
        assert_eq!(spec.stack_expiries, vec![6_000, 7_000, 8_000]);

        // Decay, as `expire_timed_state` does it: drop what has run out.
        let alive = |spec: &ConditionalSpec, now: u32| {
            spec.stack_expiries.iter().filter(|at| **at > now).count()
        };
        assert_eq!(alive(&spec, 5_999), 3);
        assert_eq!(alive(&spec, 6_500), 2, "the first stack is gone alone");
        assert_eq!(alive(&spec, 7_500), 1);
        assert_eq!(alive(&spec, 8_000), 0);

        // The reviewer's shape: three stacks at 0/1/2 s with a 5 s duration.
        let mut five = ConditionalSpec {
            stacks: 0,
            stack_expiries: Vec::new(),
            ..spec
        };
        for at in [0, 1_000, 2_000] {
            gain_stack(&mut five, at, 5_000, 5);
        }
        assert_eq!(alive(&five, 5_500), 2, "only the 5 s stack has run out");
        assert_eq!(alive(&five, 7_500), 0, "all three are gone");
    }

    /// Driven through `try_legend_swap`, not `trigger_procs`: deleting the
    /// emission line inside the swap must fail this test.
    #[test]
    fn a_legend_swap_fires_on_legend_swap_records() {
        let params = params();
        let record = gated_heal(30, TriggerRule::OnLegendSwap, vec![]);
        let mut timeline = gate_timeline(&params, &[&record], 1_000, vec![]);
        assert_eq!(timeline.proc_specs.len(), 1, "the record must load");
        timeline.player_health = params.max_health / 2.0;
        // Half the pool spent, the recharge over: the swap is taken.
        timeline
            .resources
            .insert(ResourceKind::Energy, LEGEND_SWAP_ENERGY / 4.0);
        assert!(timeline.try_legend_swap(), "the swap must happen");
        assert_eq!(fires(&timeline, 30), 1);
    }

    /// Driven through `try_stunbreak`, so the emission site is the thing
    /// under test rather than the call the test makes itself.
    #[test]
    fn breaking_a_stun_fires_on_stunbreak_records() {
        let params = params();
        let record = gated_heal(31, TriggerRule::OnStunbreak, vec![]);
        let breaker = {
            let mut skill = skill(9_001, SkillSlot::Utility, 0, 20_000, Vec::new());
            skill.is_stunbreak = true;
            skill
        };
        let skills = vec![breaker];
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(true),
            &[&record],
            &[],
            true,
            Vec::new(),
        );
        timeline.trace_enabled = true;
        assert_eq!(timeline.proc_specs.len(), 1, "the record must load");
        timeline.player_health = params.max_health / 2.0;
        timeline.disabled_until_ms = 1_000;
        timeline.try_stunbreak();
        assert_eq!(timeline.disabled_until_ms, 0, "the stun must be broken");
        assert_eq!(fires(&timeline, 31), 1);
    }

    /// Driven through `apply_buff`, the site that actually grants a boon.
    #[test]
    fn a_boon_landing_fires_only_the_records_narrowed_to_it() {
        let params = params();
        let fury_only = gated_heal(
            32,
            TriggerRule::OnBoonGained {
                boon: Some("Fury".into()),
            },
            vec![],
        );
        let mut timeline = gate_timeline(&params, &[&fury_only], 1_000, vec![]);
        timeline.player_health = params.max_health / 2.0;
        timeline.apply_buff("Might", 3, 5_000, false);
        assert_eq!(fires(&timeline, 32), 0, "Might is not Fury");
        timeline.apply_buff("Fury", 1, 5_000, false);
        assert_eq!(fires(&timeline, 32), 1);
    }

    #[test]
    fn unsupported_proc_category_counts_once_per_source() {
        let flat = test_proc_effect(11, EffectCategory::FlatStat, None);
        let heal = test_proc_effect(22, EffectCategory::OutgoingHealingPct, None);
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[&flat, &heal],
            &[],
            true,
            Vec::new(),
        );
        assert_eq!(timeline.unmodeled_names.len(), 0);
        assert_eq!(timeline.proc_specs.len(), 2);

        for _ in 0..5 {
            timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        }

        assert_eq!(timeline.unmodeled_names.len(), 2);
        assert_eq!(timeline.healing, 0.0);
    }

    #[test]
    fn timeline_at_saturates_near_u32_max() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.now_ms = u32::MAX - 10;
        assert_eq!(timeline.at(20), u32::MAX);
        assert_eq!(timeline.at(5), u32::MAX - 5);
    }

    #[test]
    fn protection_uses_formula_multiplier() {
        let params = params();
        let mut incoming = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        incoming.apply_skill_effect(
            1,
            &SkillEffect::Cover {
                kind: CoverKind::Protection,
                duration_ms: 5_000,
                strippable: true,
            },
            false,
        );
        incoming.receive_strike(1_000.0, true);
        let expected = 1_000.0 * incoming.protection_multiplier;
        assert!((incoming.incoming_damage - expected).abs() < 1e-9);

        let mut open = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        let mut prot = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            EnemyDummy {
                protection: true,
                stability: false,
                hp: Some(18_000.0),
            },
            &[],
            &[],
            true,
            Vec::new(),
        );
        let strike = SkillEffect::StrikeDamage {
            hit_count: 1,
            dmg_multiplier: 1.0,
        };
        open.apply_skill_effect(1, &strike, false);
        prot.apply_skill_effect(1, &strike, false);
        let ratio = prot.damage_events[0].amount / open.damage_events[0].amount;
        assert!((ratio - prot.protection_multiplier).abs() < 1e-9);
    }

    /// A cost the pool can never reach is named; the blocked ratio is
    /// blocked decisions over decisions, not a latch on the first one.
    #[test]
    fn unpayable_cost_is_named_and_blocked_ratio_is_a_share() {
        let rules = [
            rule(1, ResourceKind::Energy, 25.0, 0.0, false),
            rule(2, ResourceKind::Energy, 150.0, 0.0, false),
        ];
        let skills = vec![
            skill(1, SkillSlot::Utility, 200, 5_000, Vec::new()),
            skill(2, SkillSlot::Utility, 200, 5_000, Vec::new()),
        ];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        // Energy caps at 100: skill 2 can never be cast, skill 1 always can.
        assert_eq!(timeline.unpayable_skills(), vec!["test-2".to_string()]);

        timeline.resource_priority_actions = 4;
        timeline.resource_blocked_events = 1;
        let report = timeline.report();
        assert!((report.resource_blocked_ratio - 0.25).abs() < 1e-9);
        assert_eq!(report.resource_unpayable_skills, vec!["test-2".to_string()]);
    }

    #[test]
    fn energy_respects_its_cap_and_cost() {
        let rules = [rule(1, ResourceKind::Energy, 25.0, 0.0, false)];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        for _ in 0..2 {
            assert!(timeline.can_pay_resource(1));
            timeline.pay_resource(1);
        }
        assert!(!timeline.can_pay_resource(1));
        timeline.resources.insert(ResourceKind::Energy, 99.9);
        timeline.regenerate_resources();
        assert_eq!(timeline.resources[&ResourceKind::Energy], 100.0);
    }

    #[test]
    fn adrenaline_action_waits_for_landed_hits() {
        let rules = [rule(1, ResourceKind::Adrenaline, 10.0, 0.0, false)];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        timeline.resources.insert(ResourceKind::Adrenaline, 0.0);
        assert!(!timeline.can_pay_resource(1));
        // Wiki `Adrenaline`: one strike per connecting non-burst hit, so a
        // full bar is ten hits away, not two.
        for _ in 0..9 {
            timeline.gain_resource_on_hit(99);
        }
        assert!(!timeline.can_pay_resource(1), "nine strikes is not a bar");
        timeline.gain_resource_on_hit(99);
        assert!(timeline.can_pay_resource(1));
        // The burst itself does not feed the bar it spends.
        timeline.gain_resource_on_hit(1);
        assert_eq!(timeline.resources[&ResourceKind::Adrenaline], 10.0);
    }

    /// Wiki `Adrenaline`: a burst expends every FULL bar and leaves the
    /// strikes above the last full bar on the meter.
    /// One decision, one count. Scanning three unaffordable stunbreaks
    /// before finding a payable one is still a single moment, and it is not
    /// a starved one; only the moment where nothing could pay is.
    #[test]
    fn a_stunbreak_scan_counts_one_decision_not_one_per_candidate() {
        let rules = [
            rule(1, ResourceKind::Energy, 90.0, 0.0, false),
            rule(2, ResourceKind::Energy, 90.0, 0.0, false),
            rule(3, ResourceKind::Energy, 90.0, 0.0, false),
            rule(4, ResourceKind::Energy, 10.0, 0.0, false),
        ];
        let mut skills: Vec<RotationSkill> = (1..=4)
            .map(|id| skill(id, SkillSlot::Utility, 200, 5_000, Vec::new()))
            .collect();
        for entry in skills.iter_mut() {
            entry.is_stunbreak = true;
        }
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        timeline.disabled_until_ms = 5_000;
        timeline.try_stunbreak();
        assert_eq!(timeline.resource_priority_actions, 1);
        assert_eq!(
            timeline.resource_blocked_events, 0,
            "the fourth candidate paid"
        );

        // Nothing payable: one decision, one blocked decision, ratio <= 1.
        let mut starved = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        starved.disabled_until_ms = 5_000;
        starved.resources.insert(ResourceKind::Energy, 0.0);
        starved.try_stunbreak();
        assert_eq!(starved.resource_priority_actions, 1);
        assert_eq!(starved.resource_blocked_events, 1);
        assert!(starved.report().resource_blocked_ratio <= 1.0);
    }

    /// Wiki `Legend`: invoking a legend resets energy to 50 on a 10 s
    /// recharge -- the only mid-fight refill. A revenant holding two
    /// 50-energy skills casts both inside the window and is never left
    /// unable to act.
    #[test]
    fn a_legend_swap_refills_the_energy_pool() {
        let rules = [
            rule(1, ResourceKind::Energy, 50.0, 0.0, false),
            rule(2, ResourceKind::Energy, 50.0, 0.0, false),
        ];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        assert!(timeline.can_pay_resource(1));
        timeline.pay_resource(1);
        assert!(!timeline.can_pay_resource(2), "the pool is spent");
        assert!(timeline.try_legend_swap(), "the swap is the refill");
        assert_eq!(timeline.resources[&ResourceKind::Energy], 50.0);
        assert!(timeline.can_pay_resource(2));
        timeline.pay_resource(2);
        assert!(
            !timeline.try_legend_swap(),
            "one invocation per 10 s recharge"
        );
        assert_eq!(timeline.report().resource_blocked_actions, 0);
    }

    /// Wiki `Energy`: upkeep is a negative modifier on the +5 %/s rate,
    /// capped at -10 (net -5 %/s), and every upkeep skill ends the moment
    /// the pool reaches 0.
    #[test]
    fn upkeep_bends_the_energy_rate_and_ends_at_zero() {
        let rules = [SkillResourceRule {
            skill_id: 1,
            kind: ResourceKind::Energy,
            cost: 0.0,
            upkeep: 10.0,
            ..Default::default()
        }];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        timeline.active_upkeep = 10.0;
        timeline.resources.insert(ResourceKind::Energy, 50.0);
        // One second of -5 %/s.
        for _ in 0..(1_000 / TIMELINE_TICK_MS) {
            timeline.regenerate_resources();
        }
        assert!(
            (timeline.resources[&ResourceKind::Energy] - 45.0).abs() < 1e-6,
            "net regen is -5/s: {}",
            timeline.resources[&ResourceKind::Energy]
        );
        timeline.resources.insert(ResourceKind::Energy, 0.0);
        timeline.regenerate_resources();
        assert_eq!(timeline.active_upkeep, 0.0, "upkeep ends at 0 energy");
    }

    /// Wiki `Flow`: 2 per second while in combat, never from attacking,
    /// maximum 100. Dragon Trigger converts 5 flow into a charge.
    #[test]
    fn flow_fills_on_the_clock_and_fires_dragon_trigger() {
        let rules = [SkillResourceRule {
            skill_id: 62803,
            kind: ResourceKind::Flow,
            cost: 5.0,
            pool_regen_per_second: 2.0,
            ..Default::default()
        }];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        assert_eq!(timeline.resources[&ResourceKind::Flow], 0.0);
        assert!(!timeline.can_pay_resource(62803), "flow starts empty");
        // Standing in combat for 3 s is 6 flow, past the 5 a charge costs.
        for _ in 0..(3_000 / TIMELINE_TICK_MS) {
            timeline.regenerate_resources();
        }
        assert!(timeline.can_pay_resource(62803));
        // Attacking does not generate flow, and the pool stops at 100.
        let before = timeline.resources[&ResourceKind::Flow];
        timeline.gain_resource_on_hit(62803);
        assert_eq!(timeline.resources[&ResourceKind::Flow], before);
        timeline.resources.insert(ResourceKind::Flow, 99.9);
        timeline.regenerate_resources();
        assert_eq!(timeline.resources[&ResourceKind::Flow], 100.0);
    }

    /// Wiki `Preparedness`: +3 maximum initiative. The rule carries the
    /// build's ceiling, so regeneration fills to 15 instead of 12.
    #[test]
    fn preparedness_raises_the_initiative_ceiling() {
        let rules = [SkillResourceRule {
            skill_id: 1,
            kind: ResourceKind::Initiative,
            cost: 3.0,
            pool_cap: 15.0,
            ..Default::default()
        }];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        timeline.resources.insert(ResourceKind::Initiative, 14.9);
        for _ in 0..4 {
            timeline.regenerate_resources();
        }
        assert_eq!(timeline.resources[&ResourceKind::Initiative], 15.0);
    }

    #[test]
    fn a_burst_spends_full_bars_and_keeps_the_remainder() {
        let rules = [rule(1, ResourceKind::Adrenaline, 10.0, 0.0, true)];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        timeline.resources.insert(ResourceKind::Adrenaline, 23.0);
        timeline.pay_resource(1);
        assert_eq!(timeline.resources[&ResourceKind::Adrenaline], 3.0);
        timeline.resources.insert(ResourceKind::Adrenaline, 25.0);
        timeline.pay_resource(1);
        assert_eq!(
            timeline.resources[&ResourceKind::Adrenaline],
            5.0,
            "two full bars spent, five strikes left"
        );
        timeline.resources.insert(ResourceKind::Adrenaline, 30.0);
        timeline.pay_resource(1);
        assert_eq!(timeline.resources[&ResourceKind::Adrenaline], 0.0);
    }

    #[test]
    fn illusion_action_spends_the_current_stack() {
        let rules = [
            rule(1, ResourceKind::Illusions, 0.0, 1.0, false),
            rule(2, ResourceKind::Illusions, 1.0, 0.0, true),
        ];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        assert!(!timeline.can_pay_resource(2));
        timeline.gain_resource_on_hit(1);
        assert!(timeline.can_pay_resource(2));
        timeline.pay_resource(2);
        assert!(!timeline.can_pay_resource(2));
    }

    #[test]
    fn blade_action_spends_the_current_stack() {
        let rules = [
            rule(1, ResourceKind::Blades, 0.0, 1.0, false),
            rule(2, ResourceKind::Blades, 1.0, 0.0, true),
        ];
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &rules,
            true,
            Vec::new(),
        );
        assert!(!timeline.can_pay_resource(2));
        timeline.gain_resource_on_hit(1);
        assert!(timeline.can_pay_resource(2));
        timeline.pay_resource(2);
        assert!(!timeline.can_pay_resource(2));
    }

    #[test]
    fn duplicate_skill_ids_share_one_recharge_timer() {
        let mut first = skill(42, SkillSlot::Weapon2, 100, 5_000, vec![]);
        first.weapon_set = 1;
        let mut second = first.clone();
        second.weapon_set = 2;
        let skills = [first, second];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );

        timeline.set_skill_cooldown(42, 5_000);

        assert_eq!(timeline.cooldown_ready_ms, vec![5_000, 5_000]);
    }

    #[test]
    fn profession_swap_policy_controls_the_timeline_timer() {
        let mut first = skill(1, SkillSlot::Weapon2, 100, 1_000, vec![]);
        first.weapon_set = 1;
        let mut second = skill(2, SkillSlot::Weapon2, 100, 1_000, vec![]);
        second.weapon_set = 2;
        let skills = [first, second];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(6_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.weapon_swap_cooldown_ms = Some(5_000);

        timeline.try_weapon_swap();
        assert_eq!(timeline.active_weapon_set, 2);
        assert_eq!(timeline.weapon_swap_ready_ms, 5_000);
        timeline.now_ms = 4_999;
        timeline.try_weapon_swap();
        assert_eq!(timeline.active_weapon_set, 2);
        timeline.now_ms = 5_000;
        timeline.try_weapon_swap();
        assert_eq!(timeline.active_weapon_set, 1);

        timeline.weapon_swap_cooldown_ms = None;
        timeline.now_ms = 10_000;
        timeline.try_weapon_swap();
        assert_eq!(timeline.active_weapon_set, 1);
    }

    #[test]
    fn stealth_breaks_on_landed_strike_but_is_not_full_immunity() {
        let strike = skill(
            1,
            SkillSlot::Weapon2,
            100,
            1_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        );
        let skills = [strike];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.apply_defense(CoverKind::Stealth, 3_000, 1, false);

        timeline.receive_strike(1_000.0, false);
        assert!(timeline.incoming_damage > 0.0);
        assert!(timeline.has_defense(CoverKind::Stealth));

        timeline.apply_skill_effect(1, &skills[0].effects[0], true);
        assert!(!timeline.has_defense(CoverKind::Stealth));
    }

    #[test]
    fn alacrity_shortens_recharge_to_eighty_percent() {
        // Wiki Alacrity (2026-08-29): 10s CD recharges in 8s while Alacrity is up.
        let skills = [skill(
            1,
            SkillSlot::Utility,
            200,
            10_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(20_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.apply_buff("Alacrity", 1, 30_000, false);
        timeline.set_skill_cooldown(1, 10_000);
        assert_eq!(
            timeline.cooldown_ready_ms[0], 10_000,
            "store full CD; dummy clock consumes 1.25x per 100ms wall"
        );
        for _ in 0..160 {
            timeline.now_ms += TIMELINE_TICK_MS;
            timeline.tick_recharge_rate();
        }
        assert!(
            timeline.cooldown_ready_ms[0] <= timeline.now_ms,
            "10s CD ready after 8s wall with Alacrity, ready={} now={}",
            timeline.cooldown_ready_ms[0],
            timeline.now_ms
        );
    }

    #[test]
    fn alacrity_mid_cooldown_still_uses_dummy_clock() {
        let skills = [skill(
            1,
            SkillSlot::Utility,
            200,
            10_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(20_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.set_skill_cooldown(1, 10_000);
        for _ in 0..40 {
            timeline.now_ms += TIMELINE_TICK_MS;
            timeline.tick_recharge_rate();
        }
        assert_eq!(timeline.cooldown_ready_ms[0], 10_000);
        timeline.apply_buff("Alacrity", 1, 30_000, false);
        for _ in 0..128 {
            timeline.now_ms += TIMELINE_TICK_MS;
            timeline.tick_recharge_rate();
        }
        assert!(
            timeline.cooldown_ready_ms[0] <= timeline.now_ms,
            "Alacrity after 2s must still eat the remaining 8s in 6.4s wall"
        );
    }

    #[test]
    fn confusion_on_skill_use_is_not_the_one_second_pulse() {
        // Wiki Confusion (2026-08-29): DoT each second AND extra on skill activation.
        let skills = [skill(
            1,
            SkillSlot::Weapon2,
            200,
            5_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(4_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.receive_condition("Confusion".into(), 1, 10_000);
        let before = timeline.incoming_damage;
        timeline.start_cast(0);
        let on_use = crate::data::conditions().confusion_tick(1_800.0, GameMode::WvW, true);
        assert!(
            (timeline.incoming_damage - before - on_use).abs() < 0.01,
            "start_cast must apply on-skill-use Confusion, expected {on_use}, got {}",
            timeline.incoming_damage - before
        );
        let after_cast = timeline.incoming_damage;
        timeline.now_ms = 1_000;
        timeline.tick_conditions();
        let pulse = timeline.incoming_damage - after_cast;
        let dot = crate::data::conditions().confusion_tick(1_800.0, GameMode::WvW, false);
        assert!(
            (pulse - dot).abs() < 0.01,
            "1s pulse must be DoT {dot}, not on-skill-use {on_use}; got {pulse}"
        );
    }

    #[test]
    fn barrier_expires_after_five_seconds_and_caps_at_quarter_health() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(10_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        let cap = params.max_health * WVW_BARRIER_HEALTH_FRACTION;
        timeline.apply_barrier(params.max_health);
        let total: f64 = timeline.barrier.iter().map(|layer| layer.amount).sum();
        assert!((total - cap).abs() < 0.001, "cap {cap}, got {total}");
        timeline.apply_barrier(10_000.0);
        let total: f64 = timeline.barrier.iter().map(|layer| layer.amount).sum();
        assert!(total <= cap + 0.001);

        timeline.now_ms = 8_000;
        timeline.expire_timed_state();
        let absorbed_before = timeline.barrier_absorbed;
        let incoming_before = timeline.incoming_damage;
        timeline.absorb_damage(1_000.0);
        assert_eq!(timeline.barrier_absorbed, absorbed_before);
        assert!((timeline.incoming_damage - incoming_before - 1_000.0).abs() < 0.001);
    }

    #[test]
    fn interrupt_sets_four_second_cooldown() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            30_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.start_cast(0);
        assert_eq!(
            timeline.cooldown_ready_ms[0], 0,
            "no recharge until the cast resolves"
        );
        timeline.now_ms = 200;
        timeline.receive_control(900, false);
        assert_eq!(timeline.interrupted_casts, 1);
        assert!(timeline.pending.is_none());
        assert_eq!(timeline.cooldown_ready_ms[0], 4_200);
    }

    #[test]
    fn a_channel_lands_its_hits_across_the_cast_and_loses_the_rest_on_interrupt() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            30_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 4,
                dmg_multiplier: 2.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.start_cast(0);
        assert_eq!(timeline.scheduled_hits.len(), 4);
        timeline.now_ms = 500;
        timeline.land_scheduled_hits();
        assert_eq!(timeline.damage_events.len(), 2, "hits at 250 and 500 ms");
        timeline.receive_control(900, true);
        assert!(timeline.pending.is_none());
        assert!(
            timeline.scheduled_hits.is_empty(),
            "the last two hits died with the cast"
        );
        assert_eq!(timeline.damage_events.len(), 2);
    }

    #[test]
    fn the_published_opener_is_pressed_in_order_then_the_scorer_takes_over() {
        let strike = |id: u32, slot: SkillSlot, mult: f64| {
            skill(
                id,
                slot,
                500,
                10_000,
                vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: mult,
                }],
            )
        };
        // The scorer alone would open with the 3.0x skill.
        let skills = vec![
            strike(1, SkillSlot::Weapon2, 3.0),
            strike(2, SkillSlot::Weapon3, 1.0),
            strike(3, SkillSlot::Weapon4, 1.0),
        ];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        let opener = [3u32, 2];
        timeline.opener = &opener;
        assert_eq!(timeline.pick_skill(), Some(2), "page says skill 3 first");
        timeline.set_skill_cooldown(3, 10_000);
        assert_eq!(timeline.pick_skill(), Some(1), "then skill 2");
        timeline.set_skill_cooldown(2, 10_000);
        assert_eq!(
            timeline.pick_skill(),
            Some(0),
            "opener spent: scorer picks the 3.0x"
        );
    }

    #[test]
    fn recharge_starts_when_the_cast_resolves() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            30_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.start_cast(0);
        timeline.now_ms = 1_000;
        timeline.resolve_pending_cast();
        assert!(timeline.pending.is_none());
        assert_eq!(timeline.cooldown_ready_ms[0], 31_000);
    }

    #[test]
    fn a_strip_takes_the_most_recently_applied_boon() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            30_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        // Stability first with a long duration, Protection second and short:
        // the soonest-expiring rule would take Protection, LIFO takes it too —
        // so extend Stability afterwards to prove extension does not re-sort.
        timeline.apply_buff("Stability", 3, 20_000, false);
        timeline.now_ms = 1_000;
        timeline.apply_buff("Protection", 1, 5_000, false);
        timeline.now_ms = 2_000;
        timeline.apply_buff("Stability", 3, 20_000, false);
        timeline.receive_boon_strip(1);
        let kinds: Vec<CoverKind> = timeline.defenses.iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&CoverKind::Stability), "{kinds:?}");
        assert!(!kinds.contains(&CoverKind::Protection), "{kinds:?}");
    }

    #[test]
    fn boon_duration_caps_follow_the_wiki() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            30_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.apply_buff("Fury", 1, 90_000, false);
        timeline.apply_buff("Swiftness", 1, 90_000, false);
        timeline.apply_buff("Might", 1, 90_000, false);
        let expires = |t: &Timeline, name: &str| {
            t.buffs
                .iter()
                .find(|b| b.name == name)
                .map(|b| b.expires_at_ms)
                .unwrap()
        };
        assert_eq!(expires(&timeline, "Fury"), 30_000);
        assert_eq!(expires(&timeline, "Swiftness"), 60_000);
        assert_eq!(expires(&timeline, "Might"), 90_000);
    }

    #[test]
    fn might_stacks_cap_at_the_wiki_twenty_five() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            1_000,
            30_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
        )];
        let params = params();
        let mut timeline = Timeline::new(
            &skills,
            &params,
            profile(2_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.apply_buff("Might", 20, 10_000, false);
        timeline.apply_buff("Might", 10, 10_000, false);
        assert_eq!(timeline.buff_stacks("Might"), 25);
        timeline.apply_buff("Fury", 1, 10_000, false);
        timeline.apply_buff("Fury", 1, 10_000, false);
        assert_eq!(timeline.buff_stacks("Fury"), 1);
    }

    #[test]
    fn killed_dummy_stops_incoming_after_one_second() {
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            50,
            1_000,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 8.0,
            }],
        )];
        let events = vec![
            EnemyEvent {
                at_ms: 2_000,
                kind: EnemyEventKind::Strike {
                    damage: 3_000.0,
                    unblockable: true,
                },
            },
            EnemyEvent {
                at_ms: 2_500,
                kind: EnemyEventKind::Control {
                    duration_ms: 1_100,
                    unblockable: true,
                },
            },
            EnemyEvent {
                at_ms: 3_000,
                kind: EnemyEventKind::Condition {
                    condition: "Bleeding".into(),
                    stacks: 5,
                    duration_ms: 4_000,
                },
            },
            EnemyEvent {
                at_ms: 3_500,
                kind: EnemyEventKind::BoonStrip { count: 1 },
            },
        ];
        let mut fight = profile(5_000, events);
        fight.target_health = Some(1.0);
        let params = params();
        let report = run_report(&skills, &[], open_enemy(false), fight, &params);
        assert!(report.target_reached);
        assert!(report.target_reached_at_ms.expect("kill time") <= 1_000);
        assert_eq!(report.incoming_damage, 0.0);
        assert_eq!(report.interrupted_casts, 0);
    }

    #[test]
    fn fractional_condition_pays_half_tick_on_expiry() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(5_000, vec![]),
            open_enemy(false),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.target.conditions.push(TimedFoeCondition {
            name: "Bleeding".into(),
            stacks: 1,
            expires_at_ms: 1_500,
            next_tick_ms: 1_000,
        });
        let one_tick = condition_tick_damage("Bleeding", params.condition_damage, &params.mode);
        timeline.now_ms = 1_000;
        timeline.tick_conditions();
        timeline.now_ms = 1_500;
        timeline.tick_conditions();
        let total: f64 = timeline
            .damage_events
            .iter()
            .map(|event| event.amount)
            .sum();
        assert!(
            (total - one_tick * 1.5).abs() < 0.001,
            "expected 1.5 ticks ({}) got {total}",
            one_tick * 1.5
        );
        assert!(timeline.target.conditions.is_empty());
    }

    #[test]
    fn no_outcome_target_never_reaches() {
        let params = params();
        let enemy = EnemyDummy {
            protection: false,
            stability: false,
            hp: None,
        };
        let scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: CombatTier::Squad,
            combat_kind: CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "no-target".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let built = WvwProfile::for_scenario(&scenario, &enemy, &params, 5_000);
        assert!(built.target_health.is_none());
        let skills = vec![skill(
            1,
            SkillSlot::Weapon2,
            50,
            200,
            vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 20.0,
            }],
        )];
        let report = run_report(&skills, &[], enemy, built, &params);
        assert!(!report.target_reached);
        assert!(report.target_reached_at_ms.is_none());
        assert!(report.total_damage > 0.0);
    }

    #[test]
    fn unknown_strike_pct_is_unmodeled_not_one_percent() {
        use crate::data::normalized_effects::{StackingRule, UptimeModel, UptimeModelKind};
        use crate::data::EvidenceLevel;
        let effect = NormalizedEffect {
            effect_id: "test-unknown-strike".into(),
            source_type: SourceType::Relic,
            source_id: 1,
            source_name: "test".into(),
            category: EffectCategory::StrikeDamagePct,
            value: FactualValue::Unknown,
            stacking_rule: StackingRule::NonStacking,
            trigger_rule: TriggerRule::OnHit,
            uptime_model: UptimeModel {
                kind: UptimeModelKind::Unknown,
                uptime: None,
            },
            evidence_level: EvidenceLevel::Unknown,
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
        };
        let params = params();
        let timeline = Timeline::new(
            &[],
            &params,
            profile(1_000, vec![]),
            open_enemy(false),
            &[&effect],
            &[],
            true,
            Vec::new(),
        );
        assert_eq!(timeline.unmodeled_names.len(), 1);
        assert!(timeline.proc_specs.is_empty());
    }

    #[test]
    fn corrupt_maps_condition_steal_grants_boon() {
        let params = params();
        let mut timeline = Timeline::new(
            &[],
            &params,
            profile(2_000, vec![]),
            open_enemy(true),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.target.protection = true;
        timeline.apply_skill_effect(1, &SkillEffect::CorruptBoons, false);
        assert!(!timeline.target.stability);
        assert_eq!(timeline.target.conditions[0].name, "Fear");
        timeline.apply_skill_effect(1, &SkillEffect::StealBoons, false);
        assert!(!timeline.target.protection);
        assert!(timeline.has_buff("Protection"));
        let conditions = timeline.target.conditions.len();
        let buffs = timeline.buffs.len();
        timeline.apply_skill_effect(1, &SkillEffect::CorruptBoons, false);
        timeline.apply_skill_effect(1, &SkillEffect::StealBoons, false);
        assert_eq!(timeline.target.conditions.len(), conditions);
        assert_eq!(timeline.buffs.len(), buffs);
    }
}

/// Causal experiments on the hand-authored Reaper slice
/// (`specs/004-simulator-trust`, audit section 6). Each test names its kind.
/// Every one was seen failing once under the disabling change recorded in
/// `docs/simulator-connection-audit.md`.
#[cfg(test)]
mod reaper_experiments {
    use super::*;
    use crate::engine;
    use crate::rotation::reaper_fixture as fx;
    use crate::sigil_slots::SigilSlots;
    use crate::validation::ValidatedBuild;

    pub(super) fn prepared() -> engine::PreparedRotation {
        let db = fx::db();
        let build = fx::build();
        let (ctx, scenario) = fx::scenario();
        let (stats, _) = engine::calculate_validated_stats(&build, &db, "Necromancer", &ctx);
        let mut prepared = engine::prepare_validated_rotation(&build, &db, &stats, Some(&scenario))
            .expect("the fixture prepares a rotation");
        prepared.opener = fx::opener();
        prepared
    }

    // US4 (specs/007-trait-triggers): the coverage line says what was skipped
    // and why. Seen failing before the executed-source subtraction existed.

    /// A weapon skill the builder produced effects for is executed by the
    /// timeline, so it never sits on the "Not simulated" line.
    #[test]
    fn coverage_line_never_names_executed_weapon_skills() {
        let report = traced(&fx::build(), None);
        assert!(
            !report
                .unmodeled_sources
                .iter()
                .any(|s| s.starts_with("Gravedigger")),
            "executed weapon skills leave the coverage line: {:?}",
            report.unmodeled_sources
        );
    }

    /// A record that carries a `coverage` block puts its class on the line
    /// in place of "(no record)".
    #[test]
    fn coverage_entry_carries_its_class() {
        use crate::data::normalized_effects::{
            CoverageBlock, CoverageClass, EffectCategory, TriggerRule,
        };
        use crate::data::quality::ReasonClass;
        let p = prepared();
        let db = fx::db();
        let build = fx::build();
        let trait_id = build.specializations[0].all_trait_ids[0];
        let mut classified = fx::path_of_corruption();
        classified.effect_id = "test:coverage".into();
        classified.source_id = trait_id;
        classified.source_name = "Flesh of the Master".into();
        classified.category = EffectCategory::FlatStat;
        classified.value = FactualValue::Unknown;
        classified.trigger_rule = TriggerRule::Passive;
        classified.status_operation = None;
        classified.internal_cooldown = None;
        classified.coverage = Some(CoverageBlock {
            class: CoverageClass::NeedsMechanic,
            mechanic: Some("minions".into()),
        });
        let records = vec![classified];
        let consumed: HashSet<u32> = p.consumed_trait_ids.iter().copied().collect();
        let (_, coverage, _) =
            engine::active_normalized_effects(&build, &p.skills, &db, &records, &consumed);
        let entry = coverage
            .iter()
            .find(|e| e.name == "Flesh of the Master")
            .expect("the classified trait is on the list");
        assert_eq!(entry.class, ReasonClass::NeedsMechanic("minions".into()));
        assert_eq!(entry.rendered(), "Flesh of the Master (needs: minions)");
        assert!(
            coverage.iter().all(|e| e.class != ReasonClass::NoRecord
                || !consumed.contains(&trait_id)
                || e.name != "Flesh of the Master"),
            "one entry per source"
        );
    }

    /// Nothing skipped: empty coverage, empty line, no coverage reason.
    #[test]
    fn nothing_skipped_is_verified() {
        let p = prepared();
        let report = run(
            &p.skills,
            &p.params,
            &[fx::GRAVEDIGGER],
            &[],
            open_profile(2_000, vec![]),
        );
        assert!(report.coverage.is_empty(), "{:?}", report.coverage);
        assert!(report.unmodeled_sources.is_empty());
        assert!(
            crate::data::quality::coverage_reason(
                "Necromancer",
                &gw2_core::types::GameMode::WvW,
                &report.unmodeled_sources
            )
            .is_none(),
            "an empty list leaves the build Verified"
        );
    }

    /// The fixture through the production entry point, with the trace on.
    fn traced(build: &ValidatedBuild, precision: Option<f64>) -> WvwCombatReport {
        traced_opener(build, fx::opener(), precision)
    }

    /// `traced` with a custom press order.
    fn traced_opener(
        build: &ValidatedBuild,
        opener: Vec<u32>,
        precision: Option<f64>,
    ) -> WvwCombatReport {
        let db = fx::db();
        let (ctx, scenario) = fx::scenario();
        let (stats, _) = engine::calculate_validated_stats(build, &db, "Necromancer", &ctx);
        let mut prepared = engine::prepare_validated_rotation(build, &db, &stats, Some(&scenario))
            .expect("the fixture prepares a rotation");
        prepared.opener = opener;
        if let Some(precision) = precision {
            prepared.params.precision = precision;
        }
        engine::simulate_prepared_traced(&prepared, build, &db, Some(&scenario))
            .wvw
            .expect("WvW scenario runs the timeline")
    }

    /// The fixture with the engine's resource rules on an open profile (no
    /// enemy pressure), trace on: the shroud experiments need casts that
    /// resolve, and the production WvW profile interrupts the opener.
    fn traced_open(build: &ValidatedBuild, opener: &[u32], duration_ms: u32) -> WvwCombatReport {
        let db = fx::db();
        let (ctx, scenario) = fx::scenario();
        let (stats, _) = engine::calculate_validated_stats(build, &db, "Necromancer", &ctx);
        let prepared = engine::prepare_validated_rotation(build, &db, &stats, Some(&scenario))
            .expect("the fixture prepares a rotation");
        let (rules, complete, _) = engine::wvw_resource_rules(
            build,
            &prepared.skills,
            &db,
            "Necromancer",
            &ctx,
            prepared.params.max_health,
        );
        let mut timeline = Timeline::new(
            &prepared.skills,
            &prepared.params,
            open_profile(duration_ms, vec![]),
            still_enemy(),
            &[],
            &rules,
            complete,
            Vec::new(),
        );
        // Record fixtures script one or two strikes at most; a full
        // endurance pool would evade them outright (reactive dodge, see
        // `tick_endurance_and_dodge`) and leave nothing to measure.
        timeline.endurance.current = 0.0;
        timeline.opener = opener;
        timeline.trace_enabled = true;
        timeline.run();
        timeline.report()
    }

    /// Copies with the shroud bar always held, for experiments that press
    /// a shroud skill without modelling the shroud itself.
    fn unshrouded(skills: &[RotationSkill]) -> Vec<RotationSkill> {
        skills
            .iter()
            .cloned()
            .map(|mut skill| {
                if skill.weapon_set == super::super::SHROUD_SET {
                    skill.weapon_set = 0;
                }
                skill
            })
            .collect()
    }

    fn first_swap_ms(report: &WvwCombatReport) -> Option<u32> {
        report
            .trace
            .iter()
            .find(|event| event.kind == TraceKind::WeaponSwap)
            .map(|event| event.t_ms)
    }

    // US2: swapping weapons swaps sigils (T020, T021)

    /// US2 positive and negative control: a sigil on set 2 fires only after
    /// the swap; moved to set 1 it fires only before.
    #[test]
    fn reaper_swap_loads_set_two_sigils() {
        let on_two = traced_opener(
            &fx::build_with_set_two_fire(),
            fx::opener_with_swap(),
            Some(3_000.0),
        );
        let swap = first_swap_ms(&on_two).expect("the opener crosses to set 2");
        let fired = events(&on_two, TraceKind::ProcFired, "Superior Sigil of Fire");
        assert!(
            !fired.is_empty() && fired.iter().all(|e| e.t_ms >= swap),
            "set-2 sigil fires only after the swap at {swap} ms: {fired:?}"
        );

        let on_one = traced_opener(&fx::build(), fx::opener_with_swap(), Some(3_000.0));
        let swap = first_swap_ms(&on_one).expect("the opener crosses to set 2");
        let fired = events(&on_one, TraceKind::ProcFired, "Superior Sigil of Fire");
        assert!(
            !fired.is_empty() && fired.iter().all(|e| e.t_ms < swap),
            "set-1 sigil fires only before the swap at {swap} ms: {fired:?}"
        );
    }

    /// US2 timing control: the same sigil on both sets keeps one cooldown
    /// across the swap.
    #[test]
    fn reaper_swap_keeps_icd_across_sets() {
        let mut both = fx::build();
        both.set_sigil_seats([
            Some(crate::validation::ValidatedItem {
                id: fx::SIGIL_OF_FIRE,
                name: "Superior Sigil of Fire".into(),
            }),
            None,
            Some(crate::validation::ValidatedItem {
                id: fx::SIGIL_OF_FIRE,
                name: "Superior Sigil of Fire".into(),
            }),
            None,
        ]);
        let report = traced_opener(&both, fx::opener_with_swap(), Some(3_000.0));
        let swap = first_swap_ms(&report).expect("the opener crosses to set 2");
        let fired = events(&report, TraceKind::ProcFired, "Superior Sigil of Fire");
        let skipped = events(&report, TraceKind::ProcSkippedIcd, "Superior Sigil of Fire");
        assert!(
            fired.iter().any(|e| e.t_ms < swap),
            "fires on set 1 before the swap: {fired:?}"
        );
        assert!(
            skipped
                .iter()
                .any(|e| e.t_ms >= swap && e.t_ms < swap + 5_000),
            "a set-2 hit inside the cooldown started on set 1 is skipped: {skipped:?}"
        );
        assert!(
            !fired
                .iter()
                .any(|e| e.t_ms >= swap && e.t_ms < fired[0].t_ms + 5_000),
            "no second fire inside the first 5 s cooldown because the set changed: {fired:?}"
        );
    }

    /// US2 scenario 4: a set-2 sigil with a record is modeled, not listed.
    #[test]
    fn reaper_set_two_sigil_leaves_coverage_line() {
        let report = traced(&fx::build_with_set_two_fire(), None);
        assert!(
            !report
                .unmodeled_sources
                .iter()
                .any(|s| s.contains("Sigil of Fire")),
            "the stowed sigil's record loads and it leaves the coverage line: {:?}",
            report.unmodeled_sources
        );
        assert!(
            report
                .proc_trials
                .iter()
                .any(|trial| trial.source == "Superior Sigil of Fire"),
            "the stowed sigil's record is loaded (it has a trial entry): {:?}",
            report.proc_trials
        );
    }

    pub(super) fn open_profile(duration_ms: u32, events: Vec<EnemyEvent>) -> WvwProfile {
        WvwProfile {
            duration_ms,
            target_health: None,
            enemy_events: events.into(),
            required_window_ms: MIN_PROTECTED_WINDOW_MS,
            desired_window_ms: TARGET_PROTECTED_WINDOW_MS.min(duration_ms),
        }
    }

    pub(super) fn still_enemy() -> EnemyDummy {
        EnemyDummy {
            protection: false,
            stability: true,
            hp: None,
        }
    }

    /// A pinned run: `opener` pressed in order, no enemy pressure unless the
    /// profile says so, trace on.
    fn run(
        skills: &[RotationSkill],
        params: &SimParams,
        opener: &[u32],
        effects: &[&NormalizedEffect],
        profile: WvwProfile,
    ) -> WvwCombatReport {
        let mut timeline = Timeline::new(
            skills,
            params,
            profile,
            still_enemy(),
            effects,
            &[],
            false,
            Vec::new(),
        );
        // Record fixtures script one or two strikes at most; a full
        // endurance pool would evade them outright (reactive dodge, see
        // `tick_endurance_and_dodge`) and leave nothing to measure.
        timeline.endurance.current = 0.0;
        timeline.opener = opener;
        timeline.trace_enabled = true;
        timeline.trace_loaded_unmodeled();
        timeline.run();
        timeline.report()
    }

    fn only(skills: &[RotationSkill], ids: &[u32]) -> Vec<RotationSkill> {
        skills
            .iter()
            .filter(|skill| ids.contains(&skill.skill_id))
            .cloned()
            .collect()
    }

    pub(super) fn events<'a>(
        report: &'a WvwCombatReport,
        kind: TraceKind,
        source: &str,
    ) -> Vec<&'a TraceEvent> {
        report
            .trace
            .iter()
            .filter(|event| event.kind == kind && event.source.starts_with(source))
            .collect()
    }

    pub(super) fn landed(report: &WvwCombatReport, skill: &str) -> Vec<f64> {
        events(report, TraceKind::HitLanded, skill)
            .iter()
            .map(|event| event.detail.parse::<f64>().expect("damage detail"))
            .collect()
    }

    // Sprint 2 fixture variants (specs/005-wvw-proc-sites, T002)

    #[test]
    fn sprint2_fixture_variants_are_consistent() {
        let set_two = fx::build_with_set_two_fire();
        assert_eq!(set_two.active_sigil_ids(), vec![fx::SIGIL_OF_FORCE]);
        assert_eq!(set_two.sigils.len(), 2);

        let db = fx::db();
        let (ctx, _) = fx::scenario();
        let (marauder, _) =
            engine::calculate_validated_stats(&fx::build(), &db, "Necromancer", &ctx);
        let (soldier, _) = engine::calculate_validated_stats(
            &fx::build_with_zero_precision(),
            &db,
            "Necromancer",
            &ctx,
        );
        assert!(
            soldier.get("Precision") < marauder.get("Precision"),
            "Soldier carries no precision"
        );

        let records = fx::records_with_threshold_and_stack();
        assert!(records[0].health_threshold.is_some());
        assert_eq!(
            records[1].trigger_scope,
            Some(crate::data::normalized_effects::TriggerScope::WeaponSkillWithRecharge)
        );
        assert_eq!(
            records[1].max_stacks,
            Some(crate::data::FactualValue::Resolved(5))
        );

        let p = prepared();
        let sets: Vec<u8> = fx::opener_with_swap()
            .iter()
            .map(|id| {
                p.skills
                    .iter()
                    .find(|s| s.skill_id == *id)
                    .unwrap()
                    .weapon_set
            })
            .collect();
        assert_eq!(
            sets,
            vec![1, 2],
            "the swap opener crosses from set 1 to set 2"
        );
    }

    // US3: conditional bonuses (T027, T028)

    fn strike_at(at_ms: u32, damage: f64) -> EnemyEvent {
        EnemyEvent {
            at_ms,
            kind: EnemyEventKind::Strike {
                damage,
                unblockable: true,
            },
        }
    }

    /// US3 scenarios 1 and 2: the Scholar bonus applies from t = 0 while the
    /// player stays above 90 % health and drops out at the crossing.
    #[test]
    fn reaper_scholar_applies_only_above_threshold() {
        let p = prepared();
        let records = fx::records_with_threshold_and_stack();
        let scholar = &records[0];
        let opener = [fx::GRAVEDIGGER, fx::DEATH_SPIRAL];
        let bare = run(
            &p.skills,
            &p.params,
            &opener,
            &[],
            open_profile(3_000, vec![]),
        );
        let healthy = run(
            &p.skills,
            &p.params,
            &opener,
            &[scholar],
            open_profile(3_000, vec![]),
        );
        let activated = events(
            &healthy,
            TraceKind::ConditionalActivated,
            "Superior Rune of the Scholar",
        );
        assert!(
            activated.first().is_some_and(|e| e.t_ms == 0),
            "the threshold is true at the start of the fight: {activated:?}"
        );
        let bare_hits = landed(&bare, "Gravedigger");
        let boosted_hits = landed(&healthy, "Gravedigger");
        assert_eq!(bare_hits.len(), boosted_hits.len());
        for (bare_hit, boosted) in bare_hits.iter().zip(&boosted_hits) {
            // The trace prints one decimal.
            assert!(
                (boosted - bare_hit * 1.05).abs() < 0.06,
                "every strike above the threshold carries +5 %: {bare_hit} → {boosted}"
            );
        }

        // Incoming damage takes the player to 75 % at 1 000 ms.
        let hit = p.params.max_health * 0.25;
        let wounded = run(
            &p.skills,
            &p.params,
            &opener,
            &[scholar],
            open_profile(3_000, vec![strike_at(1_000, hit)]),
        );
        let expired = events(
            &wounded,
            TraceKind::ConditionalExpired,
            "Superior Rune of the Scholar",
        );
        assert!(
            expired
                .first()
                .is_some_and(|e| (1_000..=1_000 + TIMELINE_TICK_MS).contains(&e.t_ms)),
            "the bonus expires at the crossing: {expired:?}"
        );
        let crossing = expired[0].t_ms;
        let late_bare: Vec<f64> = events(&bare, TraceKind::HitLanded, "Death Spiral")
            .iter()
            .filter(|e| e.t_ms >= crossing)
            .map(|e| e.detail.parse::<f64>().unwrap())
            .collect();
        let late_wounded: Vec<f64> = events(&wounded, TraceKind::HitLanded, "Death Spiral")
            .iter()
            .filter(|e| e.t_ms >= crossing)
            .map(|e| e.detail.parse::<f64>().unwrap())
            .collect();
        assert!(!late_wounded.is_empty(), "hits land after the crossing");
        for (a, b) in late_bare.iter().zip(&late_wounded) {
            assert!(
                (a - b).abs() < 1e-6,
                "strikes below the threshold carry no bonus: {a} vs {b}"
            );
        }
    }

    /// US3 scenario 3: the Thief stacks cap at five, a sixth qualifying hit
    /// refreshes, and the stacks expire 6 s after the last one.
    #[test]
    fn reaper_thief_stacks_cap_and_expire() {
        let p = prepared();
        let records = fx::records_with_threshold_and_stack();
        let thief = &records[1];
        let skills = only(
            &p.skills,
            &[
                fx::GRAVEDIGGER,
                fx::DEATH_SPIRAL,
                fx::GRASPING_DARKNESS,
                fx::GS_AUTO,
            ],
        );
        let report = run(
            &skills,
            &p.params,
            &[fx::GRAVEDIGGER, fx::DEATH_SPIRAL, fx::GRASPING_DARKNESS],
            &[thief],
            open_profile(12_000, vec![]),
        );
        let gained = events(&report, TraceKind::StackGained, "Relic of the Thief");
        assert!(
            gained.len() >= 6,
            "six qualifying weapon-skill hits gain or refresh: {gained:?}"
        );
        assert!(
            gained.iter().all(|e| !e.detail.starts_with("6/")),
            "never a sixth stack: {gained:?}"
        );
        assert!(
            gained
                .iter()
                .filter(|e| e.detail.starts_with("5/5"))
                .count()
                >= 2,
            "the cap is reached and then refreshed: {gained:?}"
        );
        let expired = events(&report, TraceKind::ConditionalExpired, "Relic of the Thief");
        assert!(
            expired.iter().any(|e| {
                let last_gain_before = gained
                    .iter()
                    .filter(|g| g.t_ms < e.t_ms)
                    .map(|g| g.t_ms)
                    .max()
                    .unwrap_or(0);
                (6_000 - TIMELINE_TICK_MS..=6_000 + TIMELINE_TICK_MS)
                    .contains(&(e.t_ms - last_gain_before))
            }),
            "stacks expire 6 s after the last qualifying hit: {gained:?}, {expired:?}"
        );
        assert!(
            events(&report, TraceKind::StackGained, "Relic of the Thief")
                .iter()
                .all(|e| e.source != "Dusk Strike"),
            "auto-attacks without a recharge do not stack"
        );
    }

    /// US3 scenario 4 (FR-008): an unresolved threshold stays on the
    /// coverage line and never executes.
    #[test]
    fn reaper_unresolved_conditional_stays_named() {
        let p = prepared();
        let mut records = fx::records_with_threshold_and_stack();
        records[0].health_threshold = Some(crate::data::normalized_effects::HealthThreshold {
            above: true,
            percent: FactualValue::Unknown,
        });
        let report = run(
            &p.skills,
            &p.params,
            &[fx::GRAVEDIGGER],
            &[&records[0]],
            open_profile(2_000, vec![]),
        );
        assert!(
            report
                .unmodeled_sources
                .iter()
                .any(|s| s == "Superior Rune of the Scholar (unresolved value)"),
            "named as unresolved: {:?}",
            report.unmodeled_sources
        );
        assert!(
            events(
                &report,
                TraceKind::ConditionalActivated,
                "Superior Rune of the Scholar"
            )
            .is_empty(),
            "never executed with an invented number"
        );
    }

    // US4: dark field combos (T035, T036)

    /// US4 scenario 1: Soul Spiral (whirl) inside Nightfall (dark field)
    /// resolves to leeching bolts: damage plus healing, traced, and no
    /// longer counted as a degraded combo.
    #[test]
    fn reaper_dark_whirl_life_steals() {
        let p = prepared();
        let skills = unshrouded(&only(
            &p.skills,
            &[fx::NIGHTFALL, fx::SHROUD_4, fx::GS_AUTO],
        ));
        // A wound first, so the leeching heal has something to fill.
        let wound = p.params.max_health * 0.3;
        let with_field = run(
            &skills,
            &p.params,
            &[fx::NIGHTFALL, fx::SHROUD_4],
            &[],
            open_profile(4_000, vec![strike_at(100, wound)]),
        );
        let without_field = run(
            &skills,
            &p.params,
            &[fx::SHROUD_4],
            &[],
            open_profile(4_000, vec![strike_at(100, wound)]),
        );
        let resolved = events(&with_field, TraceKind::ComboResolved, "Soul Spiral");
        assert!(
            resolved
                .iter()
                .any(|e| e.detail.contains("Dark field + Whirl finisher")),
            "the dark whirl combo resolves: {resolved:?}"
        );
        assert!(
            with_field.healing > without_field.healing,
            "leeching bolts heal: {} vs {}",
            with_field.healing,
            without_field.healing
        );
        assert!(
            with_field.total_damage > without_field.total_damage,
            "leeching bolts damage: {} vs {}",
            with_field.total_damage,
            without_field.total_damage
        );
        assert!(
            !with_field
                .unmodeled_sources
                .iter()
                .any(|s| s.contains("dark field")),
            "no longer degraded: {:?}",
            with_field.unmodeled_sources
        );
        assert!(with_field.combo_activations >= 1);
    }

    /// US4 scenario 2 (regression guard): a finisher after the field has
    /// expired makes no combo.
    #[test]
    fn reaper_expired_field_makes_no_combo() {
        let p = prepared();
        let skills = unshrouded(&only(
            &p.skills,
            &[
                fx::NIGHTFALL,
                fx::SHROUD_4,
                fx::GRAVEDIGGER,
                fx::DEATH_SPIRAL,
                fx::GRASPING_DARKNESS,
                fx::YOU_ARE_ALL_WEAKLINGS,
                fx::SIGNET_OF_VAMPIRISM,
                fx::GS_AUTO,
            ],
        ));
        // Nightfall lasts 5 s; five casts push Soul Spiral past it.
        let report = run(
            &skills,
            &p.params,
            &[
                fx::NIGHTFALL,
                fx::GRAVEDIGGER,
                fx::DEATH_SPIRAL,
                fx::GRASPING_DARKNESS,
                fx::YOU_ARE_ALL_WEAKLINGS,
                fx::SIGNET_OF_VAMPIRISM,
                fx::SHROUD_4,
            ],
            &[],
            open_profile(12_000, vec![]),
        );
        let spiral_cast = events(&report, TraceKind::HitLanded, "Soul Spiral")
            .first()
            .map(|e| e.t_ms)
            .expect("Soul Spiral lands");
        assert!(
            spiral_cast > 5_000,
            "the finisher comes after the field: {spiral_cast}"
        );
        assert!(
            events(&report, TraceKind::ComboResolved, "Soul Spiral").is_empty(),
            "no combo without a live field: {:?}",
            events(&report, TraceKind::ComboResolved, "Soul Spiral")
        );
        assert_eq!(report.combo_activations, 0);
    }

    // US6: life force and shroud (T044, T045)

    /// US6 scenario 2: with no life force, shroud entry is refused with a
    /// readable reason and the shroud skills never land.
    #[test]
    fn reaper_shroud_refused_without_life_force() {
        let report = traced_open(
            &fx::build(),
            &[fx::REAPER_SHROUD, fx::SHROUD_4, fx::SHROUD_1],
            6_000,
        );
        assert!(
            !events(&report, TraceKind::ShroudRefused, "Reaper's Shroud").is_empty(),
            "entry refused: {:?}",
            report.trace.iter().take(12).collect::<Vec<_>>()
        );
        // The scheduler may bank life force with the generators and enter
        // later; the refusal comes first and nothing from the shroud bar
        // lands before that entry.
        let refused_at = events(&report, TraceKind::ShroudRefused, "Reaper's Shroud")[0].t_ms;
        let entered_at = events(&report, TraceKind::ShroudEntered, "Reaper's Shroud")
            .first()
            .map(|e| e.t_ms)
            .unwrap_or(u32::MAX);
        assert!(
            refused_at < entered_at,
            "refused at {refused_at} before any entry at {entered_at}"
        );
        assert!(
            report
                .shroud_refusals
                .iter()
                .any(|r| r.contains("needs 10% life force")),
            "a readable reason: {:?}",
            report.shroud_refusals
        );
        assert!(
            landed(&report, "Soul Spiral").is_empty() && landed(&report, "Life Rend").is_empty()
                || events(&report, TraceKind::HitLanded, "Soul Spiral")
                    .iter()
                    .chain(&events(&report, TraceKind::HitLanded, "Life Rend"))
                    .all(|e| e.t_ms >= entered_at),
            "shroud skills never land outside shroud"
        );
    }

    /// US6 scenario 1: a skill's Life Force fact raises the pool by that
    /// share of the cap, and the pool never passes the cap.
    #[test]
    fn reaper_life_force_gain_capped() {
        let report = traced_open(&fx::build(), &[fx::GRAVEDIGGER], 3_000);
        let gained = events(&report, TraceKind::LifeForceGained, "Gravedigger");
        assert!(
            gained.first().is_some_and(|e| e.detail.starts_with("8%")),
            "Gravedigger's 8 % fact is credited: {gained:?}"
        );

        let p = prepared();
        let rule = SkillResourceRule {
            skill_id: fx::GRAVEDIGGER,
            kind: ResourceKind::LifeForce,
            gain_on_use: 1.0e9,
            ..Default::default()
        };
        let skills = only(&p.skills, &[fx::GRAVEDIGGER, fx::GS_AUTO]);
        let mut timeline = Timeline::new(
            &skills,
            &p.params,
            open_profile(3_000, vec![]),
            still_enemy(),
            &[],
            &[rule],
            true,
            Vec::new(),
        );
        timeline.opener = &[fx::GRAVEDIGGER];
        timeline.run();
        let pool = timeline.resources[&ResourceKind::LifeForce];
        let cap = resource_cap(ResourceKind::LifeForce, p.params.max_health);
        assert!(
            (pool - cap).abs() < 1e-6,
            "gain past the cap is discarded: pool {pool} cap {cap}"
        );
    }

    /// US6 scenario 3: shroud entered with the generators' life force
    /// drains at the Reaper rate and ends at zero; weapon skills do not
    /// land while it is up.
    #[test]
    fn reaper_shroud_drains_and_exits() {
        let report = traced_open(
            &fx::build(),
            &[
                fx::GRAVEDIGGER,
                fx::DEATH_SPIRAL,
                fx::REAPER_SHROUD,
                fx::SHROUD_4,
                fx::SHROUD_1,
            ],
            15_000,
        );
        let entered = events(&report, TraceKind::ShroudEntered, "Reaper's Shroud");
        let entry = entered
            .first()
            .map(|e| e.t_ms)
            .unwrap_or_else(|| panic!("entered after the generators: {:?}", report.trace));
        let exited = events(&report, TraceKind::ShroudExited, "Reaper's Shroud");
        let exit = exited
            .iter()
            .find(|e| e.detail.contains("life force 0"))
            .map(|e| e.t_ms)
            .unwrap_or_else(|| panic!("exits at zero: {exited:?}"));
        // 8 % + 6 % of the pool at 5 %/s (WvW Reaper drain) ≈ 2.8 s, minus
        // whatever shroud skills do not refill.
        assert!(
            (entry + 2_000..=entry + 3_500).contains(&exit),
            "drain at 5 %/s from 14 %: entered {entry}, exited {exit}"
        );
        for skill in ["Gravedigger", "Death Spiral", "Dusk Strike"] {
            assert!(
                events(&report, TraceKind::HitLanded, skill)
                    .iter()
                    .all(|e| e.t_ms < entry || e.t_ms > exit),
                "{skill} never lands inside shroud"
            );
        }
        assert!(
            !landed(&report, "Soul Spiral").is_empty() || !landed(&report, "Life Rend").is_empty(),
            "shroud skills land inside shroud"
        );
    }

    // Polish (T056): determinism and the trace cap

    /// SC-004: ten evaluations of the fixture, trials included, are identical.
    #[test]
    fn reaper_results_repeat_identically() {
        let first = traced(&fx::build(), None);
        for _ in 0..9 {
            let again = traced(&fx::build(), None);
            assert_eq!(again.total_damage, first.total_damage);
            assert_eq!(again.healing, first.healing);
            assert_eq!(again.unmodeled_sources, first.unmodeled_sources);
            assert_eq!(again.shroud_refusals, first.shroud_refusals);
            assert_eq!(again.trace, first.trace);
            assert_eq!(again.proc_trials, first.proc_trials);
        }
        assert!(
            first
                .proc_trials
                .iter()
                .any(|t| t.source == "Superior Sigil of Fire"),
            "the trials ran: {:?}",
            first.proc_trials
        );
    }

    /// The fixture opener with every Sprint 2 event kind on stays under the
    /// 512-event cap.
    #[test]
    fn reaper_trace_fits_under_cap() {
        let report = traced_open(&fx::build(), &fx::opener(), 20_000);
        assert!(
            !report.trace_truncated,
            "{} events, cap {TRACE_CAP}",
            report.trace.len()
        );
        let production = traced(&fx::build(), None);
        assert!(
            !production.trace_truncated,
            "{} events on the production profile, cap {TRACE_CAP}",
            production.trace.len()
        );
    }

    // Diagnostics (T022)

    #[test]
    fn trace_is_empty_unless_requested() {
        let db = fx::db();
        let build = fx::build();
        let (_, scenario) = fx::scenario();
        let prepared = prepared();
        let production = engine::simulate_prepared(&prepared, &build, &db, Some(&scenario))
            .wvw
            .expect("WvW");
        assert!(
            production.trace.is_empty() && !production.trace_truncated,
            "the production entry point never traces"
        );
        let traced = engine::simulate_prepared_traced(&prepared, &build, &db, Some(&scenario))
            .wvw
            .expect("WvW");
        assert!(!traced.trace.is_empty(), "the test entry point does");
    }

    #[test]
    fn trace_caps_at_512_and_flags_truncation() {
        let params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        let mut timeline = Timeline::new(
            &[],
            &params,
            open_profile(1_000, vec![]),
            still_enemy(),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.trace_enabled = true;
        for _ in 0..600 {
            timeline.trace(TraceKind::HitLanded, "x", "");
        }
        assert_eq!(timeline.trace.len(), TRACE_CAP);
        assert!(timeline.trace_truncated);
        let report = timeline.report();
        assert_eq!(report.trace.len(), TRACE_CAP);
        assert!(report.trace_truncated);
    }

    #[test]
    fn trigger_procs_icd_skip_does_not_format_when_trace_off() {
        let params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        let mut timeline = Timeline::new(
            &[],
            &params,
            open_profile(2_000, vec![]),
            still_enemy(),
            &[],
            &[],
            true,
            Vec::new(),
        );
        timeline.trace_enabled = false;
        timeline.proc_specs.push(ProcSpec {
            source_type: SourceType::Trait,
            source_id: 1,
            source_name: "IcdSkip".into(),
            trigger: TriggerRule::OnHit,
            category: EffectCategory::StrikeDamagePct,
            value: 10.0,
            duration_ms: 0,
            internal_cooldown_ms: 10_000,
            next_ready_ms: 10_000,
            operation: None,
            weapon_set: 0,
            proc_chance: 1.0,
            mass: 0.0,
            scope: Default::default(),
            prerequisite: Some(Prerequisite {
                foe_condition: Some("Chilled".into()),
                ..Default::default()
            }),
            scale_by: None,
            healing_power_coefficient: 0.0,
            cast_skill_id: None,
            max_stacks: 0,
            gates: Vec::new(),
            scale: None,
            stacking_rule: StackingRule::NonStacking,
        });
        TRACE_CALLS.with(|c| c.set(0));
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert!(
            timeline.trace.is_empty(),
            "ICD-skip with tracing off must not push events"
        );
        assert_eq!(
            TRACE_CALLS.with(|c| c.get()),
            0,
            "ICD-skip must not call trace when tracing is off"
        );
        assert!(timeline.prerequisite_refused.is_empty());
        assert_eq!(timeline.proc_specs[0].next_ready_ms, 10_000);

        // Same ICD spec, prereq dropped so the skip is actually traced.
        timeline.proc_specs[0].prerequisite = None;
        timeline.trace_enabled = true;
        TRACE_CALLS.with(|c| c.set(0));
        timeline.trigger_procs(TriggerRule::OnHit, None, false, 1.0);
        assert!(
            TRACE_CALLS.with(|c| c.get()) > 0,
            "counter must increment when tracing is on"
        );
    }

    // Positive control

    #[test]
    fn reaper_positive_control_onhit_proc_changes_events() {
        let p = prepared();
        let record = fx::path_of_corruption();
        let with = run(
            &p.skills,
            &p.params,
            &p.opener,
            &[&record],
            open_profile(15_000, vec![]),
        );
        let without = run(
            &p.skills,
            &p.params,
            &p.opener,
            &[],
            open_profile(15_000, vec![]),
        );

        let fired = events(&with, TraceKind::ProcFired, "Path of Corruption");
        assert!(
            !fired.is_empty(),
            "Path of Corruption fires on the opener's landed hits when its record is active; trace: {:?}",
            with.trace.iter().take(12).collect::<Vec<_>>()
        );
        assert!(
            events(&without, TraceKind::ProcFired, "Path of Corruption").is_empty(),
            "nothing fires without the record"
        );
        assert!(
            events(&with, TraceKind::ProcUnmodeled, "Path of Corruption").is_empty(),
            "an executed OnHit record is never reported as unmodeled"
        );
        assert_eq!(with.unmodeled_sources, without.unmodeled_sources);
    }

    // Negative control

    #[test]
    fn reaper_negative_control_wrong_mode_and_stowed_set() {
        // (a) A record that exists only in the PvE file is not selected for
        // the WvW scenario. Signet of Undeath (skill 10611, OnSkillUse) is
        // such a record on the 2026-01-13 manifest.
        let data = crate::data::normalized_effects::effects();
        assert!(
            data.effects_for_mode("PvE")
                .iter()
                .any(|e| e.source_id == 10611),
            "control premise: the record exists in the PvE file"
        );
        assert!(
            !data
                .effects_for_mode("WvW")
                .iter()
                .any(|e| e.source_id == 10611),
            "control premise: and not in the WvW file"
        );
        let mut db = fx::db();
        let signet: gw2_api::models::Skill = serde_json::from_value(serde_json::json!({
            "id": 10611, "name": "Signet of Undeath", "slot": "Utility",
            "professions": ["Necromancer"],
            "facts": [{"type": "Recharge", "value": 60.0}]
        }))
        .expect("fixture skill");
        db.skills.insert(10611, signet);
        let mut build = fx::build();
        build.skills.utilities[1] = Some((10611, "Signet of Undeath".into()));
        let (ctx, scenario) = fx::scenario();
        let (stats, _) = engine::calculate_validated_stats(&build, &db, "Necromancer", &ctx);
        let mut prepared = engine::prepare_validated_rotation(&build, &db, &stats, Some(&scenario))
            .expect("prepares");
        prepared.opener = vec![10611, fx::GRAVEDIGGER];
        let fight = engine::simulate_prepared_traced(&prepared, &build, &db, Some(&scenario))
            .wvw
            .expect("WvW");
        let record_events: Vec<&TraceEvent> = fight
            .trace
            .iter()
            .filter(|e| {
                e.source.starts_with("Signet of Undeath")
                    && (matches!(e.kind, TraceKind::ProcFired | TraceKind::ProcSkippedIcd)
                        || (e.kind == TraceKind::ProcUnmodeled && e.detail == "unsupported proc"))
            })
            .collect();
        assert!(
            record_events.is_empty(),
            "a PvE-only record is never loaded as a proc under WvW: {record_events:?}"
        );
        assert!(
            fight
                .unmodeled_sources
                .iter()
                .any(|s| s == "Signet of Undeath (no record)"),
            "under WvW the skill is an equipped source with no record, not a loaded proc: {:?}",
            fight.unmodeled_sources
        );

        // (b) Sigil of Fire stowed on weapon set 2 while set 1 is worn is
        // absent from the fight entirely: not fired, not counted, not named.
        let worn = traced(&fx::build(), None);
        let mut stowed_build = fx::build();
        stowed_build.sigil_seats = SigilSlots::new([
            None,
            Some(fx::SIGIL_OF_FORCE),
            Some(fx::SIGIL_OF_FIRE),
            None,
        ]);
        let stowed = traced(&stowed_build, None);
        assert!(
            !events(&worn, TraceKind::ProcFired, "Superior Sigil of Fire").is_empty(),
            "worn: the on-crit sigil fires (Sprint 2)"
        );
        let first_swap = first_swap_ms(&stowed).unwrap_or(u32::MAX);
        assert!(
            events(&stowed, TraceKind::ProcUnmodeled, "Superior Sigil of Fire").is_empty()
                && events(&stowed, TraceKind::ProcFired, "Superior Sigil of Fire")
                    .iter()
                    .all(|e| e.t_ms >= first_swap),
            "stowed: the sigil is absent from the fight before a swap"
        );
        assert!(
            !worn
                .unmodeled_sources
                .iter()
                .chain(&stowed.unmodeled_sources)
                .any(|s| s.starts_with("Superior Sigil of Fire")),
            "the sigil is modeled worn and stowed alike; neither names it: {:?} vs {:?}",
            stowed.unmodeled_sources,
            worn.unmodeled_sources
        );
    }

    // Timing

    #[test]
    fn reaper_timing_icd_interrupt_and_late_buff() {
        let p = prepared();

        // (1) Internal cooldown: consecutive procs are at least 10 s apart.
        let record = fx::path_of_corruption();
        let report = run(
            &p.skills,
            &p.params,
            &p.opener,
            &[&record],
            open_profile(30_000, vec![]),
        );
        let fired: Vec<u32> = events(&report, TraceKind::ProcFired, "Path of Corruption")
            .iter()
            .map(|e| e.t_ms)
            .collect();
        assert!(
            fired.len() >= 2,
            "need two procs in 30 s to test spacing: {fired:?}"
        );
        for pair in fired.windows(2) {
            assert!(
                pair[1] - pair[0] >= 10_000,
                "procs {} ms and {} ms are inside the 10 s ICD",
                pair[0],
                pair[1]
            );
        }
        assert!(
            !events(&report, TraceKind::ProcSkippedIcd, "Path of Corruption").is_empty(),
            "hits inside the ICD are recorded as skipped, not silently dropped"
        );

        // (2) Interrupt: a control landing mid-channel loses the later hit.
        // Gravedigger (fixture): 500 ms cast, two hits at 250 and 500 ms.
        let gravedigger = only(&p.skills, &[fx::GRAVEDIGGER]);
        let calm = run(
            &gravedigger,
            &p.params,
            &[fx::GRAVEDIGGER],
            &[],
            open_profile(1_000, vec![]),
        );
        let control = EnemyEvent {
            at_ms: 300,
            kind: EnemyEventKind::Control {
                duration_ms: 1_000,
                unblockable: true,
            },
        };
        let cut = run(
            &gravedigger,
            &p.params,
            &[fx::GRAVEDIGGER],
            &[],
            open_profile(1_000, vec![control]),
        );
        assert!(
            !events(&cut, TraceKind::CastInterrupted, "Gravedigger").is_empty(),
            "the control interrupted the cast: {:?}",
            cut.trace
        );
        assert!(
            landed(&cut, "Gravedigger").len() < landed(&calm, "Gravedigger").len(),
            "interrupted {} hits vs calm {} hits",
            landed(&cut, "Gravedigger").len(),
            landed(&calm, "Gravedigger").len()
        );

        // (3) Late buff: Might applied after a hit does not raise that hit;
        // it raises the next one.
        let pair = only(&p.skills, &[fx::GRAVEDIGGER, fx::YOU_ARE_ALL_WEAKLINGS]);
        let plain = run(
            &gravedigger,
            &p.params,
            &[fx::GRAVEDIGGER],
            &[],
            open_profile(12_000, vec![]),
        );
        let late = run(
            &pair,
            &p.params,
            &[fx::GRAVEDIGGER, fx::YOU_ARE_ALL_WEAKLINGS],
            &[],
            open_profile(12_000, vec![]),
        );
        let plain_hits = landed(&plain, "Gravedigger");
        let late_hits = landed(&late, "Gravedigger");
        assert!(
            plain_hits.len() >= 3 && late_hits.len() >= 3,
            "{plain_hits:?} {late_hits:?}"
        );
        assert_eq!(
            plain_hits[..2],
            late_hits[..2],
            "the hits before the buff are priced the same"
        );
        assert!(
            late_hits.last().unwrap() > plain_hits.last().unwrap(),
            "the recast under Might hits harder: {late_hits:?} vs {plain_hits:?}"
        );
    }

    // Ablation

    #[test]
    fn reaper_ablation_enabler_and_payoff() {
        let p = prepared();
        let record = fx::path_of_corruption();
        let complete_kit = only(&p.skills, &[fx::GRAVEDIGGER, fx::WELL_OF_DARKNESS]);
        let no_enabler_kit = only(&p.skills, &[fx::WELL_OF_DARKNESS]);
        let opener = [fx::WELL_OF_DARKNESS, fx::GRAVEDIGGER];
        // Expected event difference, stated before the assertion: the
        // complete kit lands Gravedigger and the OnHit record fires at least
        // once; without the strike (enabler) there is no landed hit to fire
        // on; without the record (payoff) there is nothing to fire. Equal
        // counts are allowed only when the ICD saturates, which needs at
        // least one proc on both sides — impossible for either ablation.
        let complete = run(
            &complete_kit,
            &p.params,
            &opener,
            &[&record],
            open_profile(5_000, vec![]),
        );
        let missing_enabler = run(
            &no_enabler_kit,
            &p.params,
            &opener,
            &[&record],
            open_profile(5_000, vec![]),
        );
        let missing_payoff = run(
            &complete_kit,
            &p.params,
            &opener,
            &[],
            open_profile(5_000, vec![]),
        );
        let procs =
            |r: &WvwCombatReport| events(r, TraceKind::ProcFired, "Path of Corruption").len();
        assert!(
            procs(&complete) >= 1,
            "complete mechanism fires: {:?}",
            complete.trace
        );
        assert!(
            procs(&complete) > procs(&missing_enabler),
            "complete {} vs missing enabler {}",
            procs(&complete),
            procs(&missing_enabler)
        );
        assert!(
            procs(&complete) > procs(&missing_payoff),
            "complete {} vs missing payoff {}",
            procs(&complete),
            procs(&missing_payoff)
        );
    }

    // Unsupported control (regression test of the coverage remedy)

    /// US1 positive control (was Sprint 1's `reaper_unsupported_oncrit_is_named_not_zeroed`,
    /// inverted): with the on-crit firing site the shipped Sigil of Fire
    /// record fires from the fixture's critical hits and leaves the coverage
    /// line.
    #[test]
    fn reaper_oncrit_positive_control_fires_from_crits() {
        // Precision forced to a 100 % critical chance.
        let with_fire = traced(&fx::build(), Some(3_000.0));
        assert!(
            !events(&with_fire, TraceKind::ProcFired, "Superior Sigil of Fire").is_empty(),
            "the on-crit sigil fires from the opener's critical hits; trace: {:?}",
            with_fire.trace.iter().take(16).collect::<Vec<_>>()
        );
        assert!(
            !with_fire
                .unmodeled_sources
                .iter()
                .any(|s| s.contains("Sigil of Fire")),
            "an executed on-crit record leaves the coverage line: {:?}",
            with_fire.unmodeled_sources
        );

        let mut bare_build = fx::build();
        bare_build.sigils.retain(|s| s.id != fx::SIGIL_OF_FIRE);
        bare_build.sigil_seats = SigilSlots::new([None, Some(fx::SIGIL_OF_FORCE), None, None]);
        let bare = traced(&bare_build, Some(3_000.0));
        assert!(
            with_fire.total_damage > bare.total_damage,
            "the sigil now adds to the fight: {} vs {}",
            with_fire.total_damage,
            bare.total_damage
        );
    }

    /// US1 negative control: no critical chance, no on-crit proc, and the
    /// sigil is worth nothing.
    #[test]
    fn reaper_oncrit_zero_precision_never_fires() {
        let with_fire = traced(&fx::build_with_zero_precision(), Some(0.0));
        assert!(
            events(&with_fire, TraceKind::ProcFired, "Superior Sigil of Fire").is_empty(),
            "no crit chance, no proc: {:?}",
            events(&with_fire, TraceKind::ProcFired, "Superior Sigil of Fire")
        );
        let mut bare_build = fx::build_with_zero_precision();
        bare_build.sigils.retain(|s| s.id != fx::SIGIL_OF_FIRE);
        bare_build.sigil_seats = SigilSlots::new([None, Some(fx::SIGIL_OF_FORCE), None, None]);
        let bare = traced(&bare_build, Some(0.0));
        assert!(
            (with_fire.total_damage - bare.total_damage).abs() < 1e-9,
            "the sigil adds nothing without crits: {} vs {}",
            with_fire.total_damage,
            bare.total_damage
        );
    }

    /// US1 timing control: the internal cooldown bounds the rate. With a
    /// certain crit, the first hit fires and every hit inside the next 5 s
    /// is skipped for the cooldown.
    #[test]
    fn reaper_oncrit_icd_bounds_rate() {
        let p = prepared();
        let mut params = p.params.clone();
        params.precision = 3_000.0;
        let record = fx::sigil_of_fire();
        let report = run(
            &p.skills,
            &params,
            &[fx::GRAVEDIGGER, fx::DEATH_SPIRAL, fx::GS_AUTO],
            &[&record],
            open_profile(4_500, vec![]),
        );
        let fired = events(&report, TraceKind::ProcFired, "Superior Sigil of Fire");
        let skipped = events(&report, TraceKind::ProcSkippedIcd, "Superior Sigil of Fire");
        assert_eq!(
            fired.len(),
            1,
            "one fire inside one 5 s cooldown window; trace: {:?}",
            report.trace
        );
        assert!(
            !skipped.is_empty(),
            "the later hits are skipped for the cooldown and say so"
        );
        assert!(
            landed(&report, "Gravedigger").len() + landed(&report, "Death Spiral").len() >= 3,
            "the opener landed several hits inside the window"
        );
    }

    /// US1 scenario 5 (SC-009): the trace's seeded trials bracket the
    /// expected-value count the ranking uses.
    #[test]
    fn reaper_oncrit_trials_bracket_expected_value() {
        let report = traced(&fx::build(), Some(3_000.0));
        let expected: f64 = events(&report, TraceKind::ProcFired, "Superior Sigil of Fire")
            .iter()
            .map(|event| {
                event
                    .detail
                    .rsplit('×')
                    .next()
                    .and_then(|p| p.trim().parse::<f64>().ok())
                    .unwrap_or(1.0)
            })
            .sum();
        let trial = report
            .proc_trials
            .iter()
            .find(|trial| trial.source == "Superior Sigil of Fire")
            .unwrap_or_else(|| panic!("trials exist for the sigil: {:?}", report.proc_trials));
        assert!(
            (trial.mean - expected).abs() <= 1.0,
            "trial mean {} within one proc of the expected count {}",
            trial.mean,
            expected
        );
        assert!(f64::from(trial.min) <= trial.mean && trial.mean <= f64::from(trial.max));
    }
    // Fixture records against the runtime's own semantics

    #[test]
    fn reaper_fixture_records_follow_runtime_semantics() {
        let p = prepared();
        let records = fx::records();
        let refs: Vec<&NormalizedEffect> = records.iter().collect();
        let report = run(
            &p.skills,
            &p.params,
            &p.opener,
            &refs,
            open_profile(15_000, vec![]),
        );
        assert!(
            !events(&report, TraceKind::ProcFired, "Path of Corruption").is_empty(),
            "the OnHit record fires"
        );
        assert!(
            !events(&report, TraceKind::ProcFired, "Superior Sigil of Fire").is_empty()
                && !report
                    .unmodeled_sources
                    .iter()
                    .any(|s| s.starts_with("Superior Sigil of Fire")),
            "the OnCrit record fires (Sprint 2) and is not named: {:?}",
            report.unmodeled_sources
        );
        assert!(
            report
                .unmodeled_sources
                .iter()
                .all(|s| !s.starts_with("Superior Sigil of Force")),
            "the passive record is folded into SimParams upstream and never listed: {:?}",
            report.unmodeled_sources
        );
    }
}

/// Sprint 3 (specs/007-trait-triggers): Necromancer trait triggers on the
/// Reaper fixture. Records are test-local (never in `data/`); each firing
/// site is seen failing under `docs/audit/disable_and_run.py` before it
/// lands, and the quoted failures live in `docs/audit/sprint3-failures.md`.
#[cfg(test)]
mod necro_experiments {
    use super::reaper_experiments::{events, landed, open_profile, prepared, still_enemy};
    use super::*;
    use crate::data::normalized_effects::{
        AmountMode, OperationType, StatusOperation, TargetScope, TargetSide, TriggerScope,
    };
    use crate::data::quality::ReasonClass;
    use crate::engine;
    use crate::rotation::reaper_fixture as fx;

    // Wiki trait ids (read 2026-09-08).
    const SPEED_OF_SHADOWS: u32 = 888;
    const DEATH_PERCEPTION: u32 = 893;
    const SOUL_BARBS: u32 = 894;
    // Synthetic Scourge skills for the entry rule test.
    const DESERT_SHROUD: u32 = 40_001;
    const MANIFEST_SAND_SHADE: u32 = 40_002;
    // Synthetic skills for the status sites.
    const FEAR_SKILL: u32 = 40_003;
    const FURY_SKILL: u32 = 40_004;
    const CORRUPT_SKILL: u32 = 40_005;

    fn operation(
        operation_type: OperationType,
        target_side: TargetSide,
        status: &str,
        amount: f64,
        duration_ms: u32,
    ) -> StatusOperation {
        StatusOperation {
            operation_type,
            target_side,
            status_kind: status.into(),
            amount_mode: AmountMode::Stacks,
            amount_value: FactualValue::Resolved(amount),
            base_duration_ms: Some(FactualValue::Resolved(duration_ms)),
            target_scope: TargetScope::Self_,
            target_count: None,
            internal_cooldown_ms: None,
            source_duration_multiplier: None,
        }
    }

    fn trait_record(
        id: u32,
        name: &str,
        category: EffectCategory,
        value: f64,
        trigger: TriggerRule,
    ) -> NormalizedEffect {
        fx::record(SourceType::Trait, id, name, category, value, trigger)
    }

    fn might_on(trigger: TriggerRule, name: &str, icd_s: f64) -> NormalizedEffect {
        let mut record = trait_record(50_000, name, EffectCategory::AppliesBoon, 1.0, trigger);
        record.status_operation = Some(operation(
            OperationType::AppliesBoon,
            TargetSide::Self_,
            "Might",
            1.0,
            10_000,
        ));
        record.internal_cooldown = Some(FactualValue::Resolved(icd_s));
        record
    }

    fn life_force_on(trigger: TriggerRule, name: &str, percent: f64) -> NormalizedEffect {
        trait_record(
            50_001,
            name,
            EffectCategory::GainsLifeForce,
            percent,
            trigger,
        )
    }

    /// A skill cloned from the fixture bar with its own id, name, effects,
    /// no recharge, always available.
    fn synthetic_skill(
        base: &[RotationSkill],
        id: u32,
        name: &str,
        effects: Vec<SkillEffect>,
    ) -> RotationSkill {
        let mut skill = base
            .iter()
            .find(|s| s.skill_id == fx::GRAVEDIGGER)
            .expect("fixture skill")
            .clone();
        skill.skill_id = id;
        skill.name = name.into();
        skill.effects = effects;
        skill.cooldown_ms = 0;
        skill.cast_time_ms = 300;
        skill.weapon_set = 0;
        skill.categories.clear();
        skill
    }

    fn enemy_condition(at_ms: u32, condition: &str) -> EnemyEvent {
        EnemyEvent {
            at_ms,
            kind: EnemyEventKind::Condition {
                condition: condition.into(),
                stacks: 1,
                duration_ms: 10_000,
            },
        }
    }

    /// US2 positive/negative control: a Chilling Nova-shaped record fires
    /// only once the foe is chilled; the earlier crits are refused with the
    /// reason.
    #[test]
    fn necro_chilled_prerequisite_gates_chilling_nova() {
        let mut nova = trait_record(
            2020,
            "Chilling Nova",
            EffectCategory::AppliesCondition,
            1.0,
            TriggerRule::OnCrit,
        );
        nova.status_operation = Some(operation(
            OperationType::AppliesCondition,
            TargetSide::Enemy,
            "Chilled",
            1.0,
            2_000,
        ));
        nova.internal_cooldown = Some(FactualValue::Resolved(3.0));
        nova.prerequisite = Some(Prerequisite {
            foe_condition: Some("Chilled".into()),
            ..Default::default()
        });
        let (report, _) = open_with(
            &[
                fx::GRAVEDIGGER,
                fx::GRASPING_DARKNESS,
                fx::DEATH_SPIRAL,
                fx::GRAVEDIGGER,
            ],
            &[&nova],
            8_000,
            vec![],
        );
        let skipped = events(&report, TraceKind::ProcSkippedPrerequisite, "Chilling Nova");
        let fired = events(&report, TraceKind::TraitFired, "Chilling Nova");
        assert!(
            !skipped.is_empty() && skipped[0].detail == "foe not Chilled",
            "the pre-chill crits are refused with the reason: {:?}",
            report.trace
        );
        assert!(
            !fired.is_empty(),
            "fires once the opener's chill lands: {:?}",
            report.trace
        );
        // Grasping Darkness lands its strike at 2000 ms and its chill when
        // the cast resolves; the record fires from the next crits and is
        // refused again once the chill has expired.
        assert!(
            fired[0].t_ms > 2_000 && skipped.iter().any(|s| s.t_ms < fired[0].t_ms),
            "never before the chill: skipped {skipped:?}, fired {fired:?}"
        );

        let (unchilled, _) = open_with(
            &[fx::GRAVEDIGGER, fx::DEATH_SPIRAL],
            &[&nova],
            4_000,
            vec![],
        );
        assert!(
            events(&unchilled, TraceKind::TraitFired, "Chilling Nova").is_empty(),
            "an unchilled foe never triggers it: {:?}",
            unchilled.trace
        );
        assert!(!events(
            &unchilled,
            TraceKind::ProcSkippedPrerequisite,
            "Chilling Nova"
        )
        .is_empty());
    }

    /// US2: a trait's on-skill-use scoped to shouts fires per shout cast,
    /// honours its cooldown and ignores every other skill.
    #[test]
    fn necro_shout_scope_fires_on_shouts_only() {
        let mut record = might_on(TriggerRule::OnSkillUse, "Shout Trait", 30.0);
        record.trigger_scope = Some(TriggerScope::Category("Shout".into()));
        let p = prepared();
        let opener = [
            fx::YOU_ARE_ALL_WEAKLINGS,
            fx::GRAVEDIGGER,
            fx::CHILLED_TO_THE_BONE,
        ];
        let mut skills = p.skills.clone();
        for skill in &mut skills {
            if [fx::YOU_ARE_ALL_WEAKLINGS, fx::CHILLED_TO_THE_BONE].contains(&skill.skill_id) {
                skill.categories = vec!["Shout".into()];
            }
        }
        let (with, _) = open_with_skills(Some(skills), &opener, &[&record], 8_000, vec![]);
        let fired = events(&with, TraceKind::TraitFired, "Shout Trait");
        assert_eq!(
            fired.len(),
            1,
            "one fire inside the 30 s cooldown: {:?}",
            with.trace
        );
        assert!(
            !events(&with, TraceKind::ProcSkippedIcd, "Shout Trait").is_empty(),
            "the second shout is refused by the cooldown: {:?}",
            with.trace
        );
        let (without, _) = open_with(&opener, &[&record], 8_000, vec![]);
        assert!(
            events(&without, TraceKind::TraitFired, "Shout Trait").is_empty(),
            "no shout category, no fire: {:?}",
            without.trace
        );
    }

    /// US2: a slot scope fires on the elite only.
    #[test]
    fn necro_slot_scope_fires_on_elite_only() {
        let mut elite = might_on(TriggerRule::OnSkillUse, "Elite Trait", 0.0);
        elite.trigger_scope = Some(TriggerScope::Slot("Elite".into()));
        let mut heal = might_on(TriggerRule::OnSkillUse, "Heal Trait", 0.0);
        heal.trigger_scope = Some(TriggerScope::Slot("Heal".into()));
        let (report, _) = open_with(
            &[
                fx::YOU_ARE_ALL_WEAKLINGS,
                fx::CHILLED_TO_THE_BONE,
                fx::GRAVEDIGGER,
            ],
            &[&elite, &heal],
            8_000,
            vec![],
        );
        assert_eq!(
            events(&report, TraceKind::TraitFired, "Elite Trait").len(),
            1,
            "{:?}",
            report.trace
        );
        assert!(events(&report, TraceKind::TraitFired, "Heal Trait").is_empty());
    }

    /// US2: `OnConditionApplied` scoped by status fires on that condition
    /// only, from a skill fact.
    #[test]
    fn necro_fear_applied_fires_dread() {
        let mut dread = might_on(TriggerRule::OnConditionApplied, "Dread", 1.0);
        dread.trigger_scope = Some(TriggerScope::Status("Fear".into()));
        let mut chill = might_on(TriggerRule::OnConditionApplied, "On Chill", 1.0);
        chill.trigger_scope = Some(TriggerScope::Status("Chilled".into()));
        let p = prepared();
        let mut skills = p.skills.clone();
        skills.push(synthetic_skill(
            &p.skills,
            FEAR_SKILL,
            "Fear Skill",
            vec![SkillEffect::ApplyCondition {
                condition: "Fear".into(),
                stacks: 1,
                duration_ms: 1_000,
            }],
        ));
        let (report, _) = open_with_skills(
            Some(skills),
            &[FEAR_SKILL, fx::GRASPING_DARKNESS],
            &[&dread, &chill],
            6_000,
            vec![],
        );
        let dread_fired = events(&report, TraceKind::TraitFired, "Dread");
        let chill_fired = events(&report, TraceKind::TraitFired, "On Chill");
        assert_eq!(dread_fired.len(), 1, "{:?}", report.trace);
        assert_eq!(chill_fired.len(), 1, "{:?}", report.trace);
        assert!(
            dread_fired[0].t_ms < chill_fired[0].t_ms,
            "the fear lands first"
        );
    }

    /// US2: a boon landing on the player and a boon stripped from the foe are
    /// firing sites; both route into the life force ledger.
    #[test]
    fn necro_boon_applied_and_stripped_fire() {
        let applied = life_force_on(TriggerRule::OnBoonApplied, "Blighter's Boon", 1.0);
        let mut stripped = life_force_on(TriggerRule::OnBoonStripped, "Blighter's Strip", 1.0);
        stripped.source_id = 50_002;
        let p = prepared();
        let mut skills = p.skills.clone();
        skills.push(synthetic_skill(
            &p.skills,
            FURY_SKILL,
            "Fury Skill",
            vec![SkillEffect::ApplyBuff {
                buff: "Fury".into(),
                stacks: 1,
                duration_ms: 5_000,
            }],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            CORRUPT_SKILL,
            "Corrupt Skill",
            vec![SkillEffect::CorruptBoons],
        ));
        let (report, _) = open_with_skills(
            Some(skills),
            &[FURY_SKILL, CORRUPT_SKILL],
            &[&applied, &stripped],
            4_000,
            vec![],
        );
        assert_eq!(
            events(&report, TraceKind::TraitFired, "Blighter's Boon").len(),
            1,
            "{:?}",
            report.trace
        );
        assert_eq!(
            events(&report, TraceKind::TraitFired, "Blighter's Strip").len(),
            1,
            "{:?}",
            report.trace
        );
        assert!(
            events(&report, TraceKind::LifeForceGained, "Blighter's Boon")
                .iter()
                .any(|e| e.detail.starts_with("1% →")),
            "{:?}",
            report.trace
        );
        assert!(!events(&report, TraceKind::LifeForceGained, "Blighter's Strip").is_empty());
    }

    /// US2: a periodic record ticks from t=0 every period; an exit record
    /// scaled by the conditions the same firing removed credits 7 % each.
    #[test]
    fn necro_periodic_and_exit_life_force() {
        let mut periodic = life_force_on(TriggerRule::Periodic, "Periodic Life Force", 1.0);
        periodic.internal_cooldown = Some(FactualValue::Resolved(3.0));
        let mut cleanse = trait_record(
            1692,
            "Unholy Martyr cleanse",
            EffectCategory::RemovesCondition,
            3.0,
            TriggerRule::OnShroudExit,
        );
        cleanse.status_operation = Some(operation(
            OperationType::RemovesCondition,
            TargetSide::Self_,
            "condition",
            3.0,
            0,
        ));
        let mut gain = life_force_on(TriggerRule::OnShroudExit, "Unholy Martyr", 7.0);
        gain.source_id = 1692;
        gain.scale_by = Some(ScaleBy::ConditionsRemoved);
        let (report, _) = open_with(
            &fx::opener(),
            &[&periodic, &cleanse, &gain],
            10_000,
            vec![
                enemy_condition(3_000, "Bleeding"),
                enemy_condition(3_000, "Poison"),
            ],
        );
        let ticks: Vec<u32> = events(&report, TraceKind::TraitFired, "Periodic Life Force")
            .iter()
            .map(|e| e.t_ms)
            .collect();
        assert_eq!(ticks, vec![0, 3_000, 6_000, 9_000], "{:?}", report.trace);
        let exit = events(&report, TraceKind::ShroudExited, "Reaper's Shroud");
        assert_eq!(exit.len(), 1, "{:?}", report.trace);
        let gained = events(&report, TraceKind::LifeForceGained, "Unholy Martyr");
        assert_eq!(gained.len(), 1, "{:?}", report.trace);
        assert_eq!(gained[0].t_ms, exit[0].t_ms);
        assert!(
            gained[0].detail.starts_with("14% →"),
            "7 % per condition removed, two removed: {}",
            gained[0].detail
        );
        assert_eq!(report.conditions_cleansed, 2);
    }

    /// US2: the heal route adds the coefficient times healing power.
    #[test]
    fn necro_heal_route_uses_healing_power() {
        let mut record = trait_record(
            1932,
            "Blighter's Heal",
            EffectCategory::Heal,
            133.0,
            TriggerRule::OnHit,
        );
        record.healing_power_coefficient = Some(FactualValue::Resolved(0.1));
        record.internal_cooldown = Some(FactualValue::Resolved(100.0));
        let hit = vec![EnemyEvent {
            at_ms: 100,
            kind: EnemyEventKind::Strike {
                damage: 3_000.0,
                unblockable: true,
            },
        }];
        let (with, _) = open_with(&[fx::GRAVEDIGGER], &[&record], 2_000, hit.clone());
        let (without, _) = open_with(&[fx::GRAVEDIGGER], &[], 2_000, hit);
        let p = prepared();
        let expected = 133.0 + 0.1 * p.params.healing_power;
        assert_eq!(
            events(&with, TraceKind::TraitFired, "Blighter's Heal").len(),
            1
        );
        assert!(
            (with.healing - without.healing - expected).abs() < 1e-6,
            "heal {} - {} = {expected}",
            with.healing,
            without.healing
        );
    }

    /// US2: a prerequisite that never holds is traced at the end, not listed
    /// on the coverage line.
    #[test]
    fn necro_prerequisite_never_met_is_traced_not_listed() {
        let mut record = might_on(TriggerRule::OnHit, "Low Health Trait", 0.0);
        record.prerequisite = Some(Prerequisite {
            foe_health: Some(crate::data::normalized_effects::HealthThreshold {
                above: false,
                percent: FactualValue::Resolved(50.0),
            }),
            ..Default::default()
        });
        let (report, _) = open_with(&[fx::GRAVEDIGGER], &[&record], 2_000, vec![]);
        assert!(events(&report, TraceKind::TraitFired, "Low Health Trait").is_empty());
        let skipped = events(
            &report,
            TraceKind::ProcSkippedPrerequisite,
            "Low Health Trait",
        );
        assert!(
            skipped
                .last()
                .is_some_and(|e| e.detail == "prerequisite never met"),
            "{skipped:?}"
        );
        assert!(
            !report.coverage.iter().any(|e| e.name == "Low Health Trait"),
            "{:?}",
            report.coverage
        );
    }

    // ---- Fight population (specs/007-trait-triggers, FR-003a)

    /// `open_with` on a chosen scale.
    fn open_population(
        tier: crate::scenario::CombatTier,
        skills: Option<Vec<RotationSkill>>,
        opener: &[u32],
        effects: &[&NormalizedEffect],
        duration_ms: u32,
    ) -> WvwCombatReport {
        let db = fx::db();
        let build = fx::build();
        let (ctx, _) = fx::scenario();
        let p = prepared();
        let (rules, complete, _) = engine::wvw_resource_rules(
            &build,
            &p.skills,
            &db,
            "Necromancer",
            &ctx,
            p.params.max_health,
        );
        let skills = skills.unwrap_or_else(|| p.skills.clone());
        let mut timeline = Timeline::new(
            &skills,
            &p.params,
            open_profile(duration_ms, vec![]),
            still_enemy(),
            effects,
            &rules,
            complete,
            Vec::new(),
        );
        timeline.population = crate::data::fight_population::FightPopulation::for_tier(tier);
        // Record fixtures script one or two strikes at most; a full
        // endurance pool would evade them outright (reactive dodge, see
        // `tick_endurance_and_dodge`) and leave nothing to measure.
        timeline.endurance.current = 0.0;
        timeline.opener = opener;
        timeline.trace_enabled = true;
        timeline.run();
        timeline.report()
    }

    /// An ally-facing Might record, five targets, fired once per fight.
    fn party_might(target_count: u32) -> NormalizedEffect {
        let mut record = might_on(TriggerRule::OnHit, "Party Might", 100.0);
        let op = record.status_operation.as_mut().expect("operation");
        op.target_side = TargetSide::Ally;
        op.target_scope = TargetScope::Party;
        op.target_count = Some(FactualValue::Resolved(target_count));
        record
    }

    /// A foe-facing Bleeding record, five targets, fired once per fight.
    fn cleave_bleed(target_count: u32) -> NormalizedEffect {
        let mut record = trait_record(
            50_010,
            "Cleave Bleed",
            EffectCategory::AppliesCondition,
            1.0,
            TriggerRule::OnHit,
        );
        let mut op = operation(
            OperationType::AppliesCondition,
            TargetSide::Enemy,
            "Bleeding",
            2.0,
            4_000,
        );
        op.target_scope = TargetScope::Area;
        op.target_count = Some(FactualValue::Resolved(target_count));
        record.status_operation = Some(op);
        record.internal_cooldown = Some(FactualValue::Resolved(100.0));
        record
    }

    fn boon_seconds(p: &engine::PreparedRotation, duration_ms: u32) -> f64 {
        (duration_ms as f64 * p.params.boon_duration_mult).round() / 1_000.0
    }

    /// Havoc: the player plus four allies are credited, within the cap.
    #[test]
    fn population_havoc_credits_five_or_cap() {
        let p = prepared();
        let record = party_might(5);
        let report = open_population(
            crate::scenario::CombatTier::Party,
            None,
            &[fx::GRAVEDIGGER],
            &[&record],
            2_000,
        );
        assert_eq!(
            events(&report, TraceKind::TraitFired, "Party Might").len(),
            1
        );
        let expected = 4.0 * 1.0 * boon_seconds(&p, 10_000);
        assert!(
            (report.ally_boon_stack_seconds - expected).abs() < 1e-9,
            "four allies × 1 stack × {} s: got {}",
            boon_seconds(&p, 10_000),
            report.ally_boon_stack_seconds
        );
        assert!(
            events(&report, TraceKind::PopulationApplied, "Party Might")
                .iter()
                .any(|e| e.detail == "5 of allies (5)"),
            "{:?}",
            report.trace
        );
    }

    /// Roam: only the player and the one foe.
    #[test]
    fn population_roam_credits_one() {
        let might = party_might(5);
        let bleed = cleave_bleed(5);
        let p = prepared();
        let mut skills = p.skills.clone();
        for skill in &mut skills {
            skill.targets = 5;
        }
        let report = open_population(
            crate::scenario::CombatTier::Solo,
            Some(skills),
            &[fx::GRAVEDIGGER],
            &[&might, &bleed],
            2_000,
        );
        assert_eq!(report.ally_boon_stack_seconds, 0.0);
        assert_eq!(report.ally_healing, 0.0);
        assert_eq!(report.ally_cleanses, 0);
        assert_eq!(report.cleave_damage, 0.0);
        assert_eq!(report.cleave_condition_stack_seconds, 0.0);
        assert!(events(&report, TraceKind::PopulationApplied, "").is_empty());
    }

    /// Cloud: the record's target count caps the credit, not the squad size.
    #[test]
    fn population_cloud_caps_at_record() {
        let p = prepared();
        let might = party_might(5);
        let bleed = cleave_bleed(5);
        let report = open_population(
            crate::scenario::CombatTier::Squad,
            None,
            &[fx::GRAVEDIGGER],
            &[&might, &bleed],
            2_000,
        );
        let expected = 4.0 * boon_seconds(&p, 10_000);
        assert!(
            (report.ally_boon_stack_seconds - expected).abs() < 1e-9,
            "four allies, not nine: {}",
            report.ally_boon_stack_seconds
        );
        let expected_bleed = 4.0 * 2.0 * 4.0;
        assert!(
            (report.cleave_condition_stack_seconds - expected_bleed).abs() < 1e-9,
            "four secondary foes × 2 stacks × 4 s: {}",
            report.cleave_condition_stack_seconds
        );
    }

    /// Cleave strikes join the damage totals.
    #[test]
    fn population_cleave_damage_joins_totals() {
        let p = prepared();
        let mut skills = p.skills.clone();
        for skill in &mut skills {
            if skill.skill_id == fx::GRAVEDIGGER {
                skill.targets = 5;
            }
        }
        let party = open_population(
            crate::scenario::CombatTier::Party,
            Some(skills.clone()),
            &[fx::GRAVEDIGGER],
            &[],
            1_000,
        );
        let solo = open_population(
            crate::scenario::CombatTier::Solo,
            Some(skills),
            &[fx::GRAVEDIGGER],
            &[],
            1_000,
        );
        assert!(party.cleave_damage > 0.0, "{:?}", party.trace);
        assert!(
            (party.total_damage - solo.total_damage - party.cleave_damage).abs() < 1e-6,
            "party {} = solo {} + cleave {}",
            party.total_damage,
            solo.total_damage,
            party.cleave_damage
        );
        assert!(
            (party.cleave_damage - 4.0 * solo.total_damage).abs() < 1e-6,
            "four secondary foes take the same strike"
        );
    }

    /// A skill fact's `Number of Targets` reaches the same counting path.
    #[test]
    fn population_skill_fact_targets_feed_the_same_path() {
        let skill: gw2_api::models::Skill = serde_json::from_value(serde_json::json!({
            "id": 41_000,
            "name": "Well of Blood",
            "slot": "Heal",
            "professions": ["Necromancer"],
            "facts": [
                {"type": "Number", "text": "Number of Targets", "value": 5},
                {"type": "Buff", "status": "Regeneration", "duration": 5, "apply_count": 1},
                {"type": "Recharge", "value": 30.0}
            ]
        }))
        .expect("skill json");
        let rotation = crate::rotation::builder::skill_to_rotation(&skill);
        assert_eq!(rotation.targets, 5);
        let p = prepared();
        let mut skills = p.skills.clone();
        let mut well = rotation;
        well.weapon_set = 0;
        skills.push(well);
        let report = open_population(
            crate::scenario::CombatTier::Party,
            Some(skills),
            &[41_000],
            &[],
            2_000,
        );
        let expected = 4.0 * boon_seconds(&p, 5_000);
        assert!(
            (report.ally_boon_stack_seconds - expected).abs() < 1e-9,
            "{} vs {expected}",
            report.ally_boon_stack_seconds
        );
    }

    // ---- The Necromancer catalogue (US3): every executable record in
    // data/normalized_effects/2026-01-13/wvw.json fires on one press order.

    /// A record of the shipped catalogue by effect id.
    fn catalogue_record(effect_id: &str) -> NormalizedEffect {
        crate::data::normalized_effects::effects()
            .effects_for_mode("WvW")
            .iter()
            .find(|e| e.effect_id == effect_id)
            .cloned()
            .unwrap_or_else(|| panic!("{effect_id} is in the WvW catalogue"))
    }

    /// Convergence (SC-001, T083): Death's Carapace is a stacking toughness
    /// effect. Armored Shroud's 5 stacks at the entry scale a later strike
    /// by armor / (armor + 100) (wiki `Death's Carapace`: 20 per stack).
    #[test]
    fn necro_carapace_scales_incoming_strikes_by_armor() {
        let record = catalogue_record("trait:856:0");
        let strike = || {
            vec![EnemyEvent {
                at_ms: 4_000,
                kind: EnemyEventKind::Strike {
                    damage: 1_000.0,
                    unblockable: true,
                },
            }]
        };
        let (with, buffs) = open_with(&[fx::REAPER_SHROUD], &[&record], 5_000, strike());
        let (without, _) = open_with(&[fx::REAPER_SHROUD], &[], 5_000, strike());
        assert_eq!(
            events(&with, TraceKind::TraitFired, "Armored Shroud").len(),
            1,
            "{:?}",
            with.trace
        );
        assert!(buffs.iter().any(|b| b == "Death's Carapace"), "{buffs:?}");
        let armor = prepared().params.armor.max(1_000.0);
        let expected = armor / (armor + 100.0);
        let ratio = with.incoming_damage / without.incoming_damage;
        assert!(
            (ratio - expected).abs() < 1e-6,
            "incoming {} vs {}: ratio {ratio} expected {expected}",
            with.incoming_damage,
            without.incoming_damage
        );
    }

    /// Convergence (T083): `OnConditionRemoved` fires on a cleanse that
    /// removed something and never on a cleanse of nothing.
    #[test]
    fn necro_condition_removed_needs_a_removed_condition() {
        let cleanse = catalogue_record("trait:1922:0");
        let gain = catalogue_record("trait:1922:2");
        assert_eq!(gain.trigger_rule, TriggerRule::OnConditionRemoved);
        let crippled = vec![EnemyEvent {
            at_ms: 0,
            kind: EnemyEventKind::Condition {
                condition: "Crippled".into(),
                stacks: 1,
                duration_ms: 30_000,
            },
        }];
        let gains = |r: &WvwCombatReport| {
            events(r, TraceKind::TraitFired, "Shrouded Removal")
                .into_iter()
                .filter(|e| e.detail.starts_with("AppliesBoon"))
                .count()
        };
        let (clean, _) = open_with(&[fx::REAPER_SHROUD], &[&cleanse, &gain], 3_000, vec![]);
        assert_eq!(gains(&clean), 0, "nothing to remove: {:?}", clean.trace);
        let (afflicted, buffs) =
            open_with(&[fx::REAPER_SHROUD], &[&cleanse, &gain], 3_000, crippled);
        assert!(gains(&afflicted) >= 1, "{:?}", afflicted.trace);
        assert!(buffs.iter().any(|b| b == "Death's Carapace"), "{buffs:?}");
    }

    /// Spec edge case (T085): a Scourge with no shade out. With no shroud
    /// bar, the shade skill in F1 is "shroud skill 1" for a `Shroud_1`
    /// record; a bar with a shroud keeps Weapon_1 of the shroud bar.
    #[test]
    fn necro_scourge_shade_skill_is_shroud_skill_one() {
        const SHADE: u32 = 40_006;
        let p = prepared();
        let mut skills: Vec<RotationSkill> = p
            .skills
            .iter()
            .filter(|s| s.weapon_set != super::super::SHROUD_SET)
            .cloned()
            .collect();
        let mut shade = synthetic_skill(&p.skills, SHADE, "Manifest Sand Shade", vec![]);
        shade.weapon_set = 0;
        shade.slot_name = Some("Profession_1".into());
        skills.push(shade);
        let record = catalogue_record("trait:875:0");
        let (report, _) = open_with_skills(Some(skills), &[SHADE], &[&record], 2_000, vec![]);
        assert!(
            !events(&report, TraceKind::TraitFired, "Unyielding Blast").is_empty(),
            "the shade is shroud skill 1: {:?}",
            report.trace
        );
        // With the shroud bar present the same slot name is not a shroud skill.
        let mut with_bar = p.skills.clone();
        let mut shade = synthetic_skill(&p.skills, SHADE, "Manifest Sand Shade", vec![]);
        shade.weapon_set = 0;
        shade.slot_name = Some("Profession_1".into());
        with_bar.push(shade);
        let (report, _) = open_with_skills(Some(with_bar), &[SHADE], &[&record], 2_000, vec![]);
        assert!(
            events(&report, TraceKind::TraitFired, "Unyielding Blast").is_empty(),
            "{:?}",
            report.trace
        );
    }

    /// One press order that reaches every trigger kind and scope the
    /// catalogue uses: a chill before the crits, a fear, a blind, a burn, a
    /// torment, a boon on the player, a corrupt, an elixir, a shout, a
    /// signet, the generators, the shroud (skills 1 and 2) and the exit.
    fn catalogue_run(low_health: bool) -> WvwCombatReport {
        const FEAR: u32 = 41_010;
        const BLIND: u32 = 41_011;
        const BURN: u32 = 41_012;
        const TORMENT: u32 = 41_013;
        const FURY: u32 = 41_014;
        const CORRUPT: u32 = 41_015;
        const ELIXIR: u32 = 41_016;
        const POISON: u32 = 41_017;
        let p = prepared();
        let condition = |name: &str| SkillEffect::ApplyCondition {
            condition: name.into(),
            stacks: 1,
            duration_ms: 6_000,
        };
        let mut skills = p.skills.clone();
        for skill in &mut skills {
            if [fx::YOU_ARE_ALL_WEAKLINGS, fx::CHILLED_TO_THE_BONE].contains(&skill.skill_id) {
                skill.categories = vec!["Shout".into()];
            }
            if skill.skill_id == fx::SIGNET_OF_VAMPIRISM {
                skill.categories = vec!["Signet".into()];
            }
            // The fixture's shroud bar uses placeholder slots; the API
            // publishes shroud skills as Weapon_1..Weapon_5.
            if skill.weapon_set == super::super::SHROUD_SET {
                skill.slot_name = Some(format!("Weapon_{}", skill.skill_id - fx::REAPER_SHROUD));
            }
        }
        skills.push(synthetic_skill(
            &p.skills,
            FEAR,
            "Fear Skill",
            vec![condition("Fear")],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            BLIND,
            "Blind Skill",
            vec![condition("Blinded")],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            BURN,
            "Burn Skill",
            vec![condition("Burning")],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            TORMENT,
            "Torment Skill",
            vec![condition("Torment")],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            POISON,
            "Poison Skill",
            vec![condition("Poisoned")],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            FURY,
            "Fury Skill",
            vec![SkillEffect::ApplyBuff {
                buff: "Fury".into(),
                stacks: 1,
                duration_ms: 5_000,
            }],
        ));
        skills.push(synthetic_skill(
            &p.skills,
            CORRUPT,
            "Corrupt Skill",
            vec![SkillEffect::CorruptBoons],
        ));
        let mut elixir = synthetic_skill(&p.skills, ELIXIR, "Elixir Skill", vec![]);
        elixir.categories = vec!["Elixir".into()];
        skills.push(elixir);
        let mut exit = p
            .skills
            .iter()
            .find(|s| s.skill_id == fx::REAPER_SHROUD)
            .expect("entry skill")
            .clone();
        exit.skill_id = fx::EXIT_SHROUD;
        exit.name = "Exit Reaper's Shroud".into();
        exit.weapon_set = super::super::SHROUD_SET;
        exit.cooldown_ms = 0;
        exit.effects.clear();
        skills.push(exit);
        // The fixture's catalogue press order (T050) with the synthetic
        // status skills after the chill, the signet before the generators
        // and the exit at the end.
        let base = fx::opener_catalogue();
        let mut opener = vec![
            base[0], base[1], FEAR, BLIND, BURN, TORMENT, POISON, FURY, CORRUPT, ELIXIR,
        ];
        opener.extend_from_slice(&base[2..4]);
        opener.push(fx::SIGNET_OF_VAMPIRISM);
        opener.extend_from_slice(&base[4..]);
        opener.push(fx::SHROUD_2);
        opener.push(fx::EXIT_SHROUD);
        let db = fx::db();
        let build = fx::build();
        let (ctx, _) = fx::scenario();
        let (rules, complete, _) = engine::wvw_resource_rules(
            &build,
            &p.skills,
            &db,
            "Necromancer",
            &ctx,
            p.params.max_health,
        );
        let records: Vec<&NormalizedEffect> = crate::data::normalized_effects::effects()
            .effects_for_mode("WvW")
            .iter()
            .filter(|e| e.source_type == SourceType::Trait && e.coverage.is_none())
            .collect();
        let mut profile = open_profile(40_000, vec![]);
        if low_health {
            profile.target_health = Some(30_000.0);
        }
        let mut timeline = Timeline::new(
            &skills,
            &p.params,
            profile,
            still_enemy(),
            &records,
            &rules,
            complete,
            Vec::new(),
        );
        if low_health {
            // The foe starts under the 50 % gates the Spite records read.
            timeline.target.hp = Some(10_000.0);
        }
        // Diagnostic run: the production cap would cut a 40 s catalogue
        // fight short of its shroud exit.
        timeline.trace_cap = 8_192;
        // Convergence: the cleanse records need conditions on the player so
        // `OnConditionRemoved` has something to fire on (non-damaging ones).
        for name in ["Crippled", "Weakness", "Vulnerability"] {
            timeline.incoming_conditions.push(TimedCondition {
                name: name.into(),
                stacks: 1,
                expires_at_ms: 60_000,
                next_tick_ms: 1_000,
            });
        }
        timeline.opener = &opener;
        timeline.trace_enabled = true;
        timeline.trace_loaded_unmodeled();
        timeline.run();
        timeline.report()
    }

    /// US3: every executable Necromancer record fires at least once on the
    /// catalogue press order (or the low-health variant), by trait name and
    /// payload category; unresolved records sit on the line as unresolved.
    #[test]
    fn necro_catalogue_every_record_fires_on_its_scenario() {
        let runs = [catalogue_run(false), catalogue_run(true)];
        let records: Vec<&NormalizedEffect> = crate::data::normalized_effects::effects()
            .effects_for_mode("WvW")
            .iter()
            .filter(|e| {
                e.source_type == SourceType::Trait
                    && e.coverage.is_none()
                    && e.source
                        .as_deref()
                        .unwrap_or("")
                        .contains("read 2026-09-08")
            })
            .collect();
        assert!(
            records.len() >= 50,
            "the catalogue is loaded: {}",
            records.len()
        );
        let mut failures = Vec::new();
        for record in &records {
            let name = record.source_name.as_str();
            let payload = record.inner_category.as_ref().unwrap_or(&record.category);
            let payload = format!("{payload:?}");
            let unresolved = !record.value.is_resolved();
            let ok = if unresolved {
                runs.iter().any(|r| {
                    r.unmodeled_sources
                        .iter()
                        .any(|s| s == &format!("{name} (unresolved value)"))
                })
            } else if matches!(record.trigger_rule, TriggerRule::Conditional) {
                runs.iter().any(|r| {
                    r.trace.iter().any(|e| {
                        e.source == name
                            && matches!(
                                e.kind,
                                TraceKind::ShroudBonusActive | TraceKind::ConditionalActivated
                            )
                    })
                })
            } else {
                runs.iter().any(|r| {
                    r.trace.iter().any(|e| {
                        e.kind == TraceKind::TraitFired
                            && e.source == name
                            && e.detail.starts_with(&payload)
                    })
                })
            };
            if !ok {
                failures.push(format!(
                    "{} [{}] {:?} {}",
                    record.effect_id, name, record.trigger_rule, payload
                ));
            }
        }
        let shroud: Vec<String> = runs[0]
            .trace
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    TraceKind::ShroudEntered | TraceKind::ShroudRefused | TraceKind::ShroudExited
                ) || e.source.contains("Reaper's Shroud")
            })
            .map(|e| format!("{} {:?} {} {}", e.t_ms, e.kind, e.source, e.detail))
            .collect();
        let tail: Vec<String> = runs[0]
            .trace
            .iter()
            .rev()
            .take(6)
            .map(|e| format!("{} {:?} {} {}", e.t_ms, e.kind, e.source, e.detail))
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} records never fired:\n{}\nshroud events: {:?}\ntrace tail: {:?}\nactions {} refusals {:?}",
            failures.len(),
            records.len(),
            failures.join("\n"),
            shroud,
            tail,
            runs[0].successful_action_count,
            runs[0].shroud_refusals
        );
        // Nothing an executable record covers is left on the line as "no record".
        for run in &runs {
            for record in &records {
                assert!(
                    !run.unmodeled_sources
                        .iter()
                        .any(|s| s == &format!("{} (no record)", record.source_name)),
                    "{} has a record",
                    record.source_name
                );
            }
        }
    }

    /// US2 timing: a long cooldown fires once and refuses the rest.
    #[test]
    fn necro_long_cooldown_fires_once_and_traces_refusal() {
        let record = might_on(TriggerRule::OnHit, "Slow Trait", 60.0);
        let (report, _) = open_with(&fx::opener(), &[&record], 8_000, vec![]);
        assert_eq!(
            events(&report, TraceKind::TraitFired, "Slow Trait").len(),
            1
        );
        assert!(!events(&report, TraceKind::ProcSkippedIcd, "Slow Trait").is_empty());
    }

    fn self_boon(boon: &str, duration_ms: u32) -> StatusOperation {
        StatusOperation {
            operation_type: OperationType::AppliesBoon,
            target_side: TargetSide::Self_,
            status_kind: boon.into(),
            amount_mode: AmountMode::Stacks,
            amount_value: FactualValue::Resolved(1.0),
            base_duration_ms: Some(FactualValue::Resolved(duration_ms)),
            target_scope: TargetScope::Self_,
            target_count: None,
            internal_cooldown_ms: None,
            source_duration_multiplier: None,
        }
    }

    /// Speed of Shadows-shaped: Swiftness 10 s on shroud entry.
    fn speed_of_shadows() -> NormalizedEffect {
        let mut record = fx::record(
            SourceType::Trait,
            SPEED_OF_SHADOWS,
            "Speed of Shadows",
            EffectCategory::AppliesBoon,
            1.0,
            TriggerRule::OnShroudEnter,
        );
        record.status_operation = Some(self_boon("Swiftness", 10_000));
        record
    }

    /// An exit-shaped record: Fury 5 s when the shroud ends.
    fn fury_on_exit() -> NormalizedEffect {
        let mut record = fx::record(
            SourceType::Trait,
            SOUL_BARBS,
            "Fury on exit",
            EffectCategory::AppliesBoon,
            1.0,
            TriggerRule::OnShroudExit,
        );
        record.status_operation = Some(self_boon("Fury", 5_000));
        record
    }

    /// Death Perception's in-shroud half: +15 % critical damage while in shroud.
    fn death_perception_in_shroud() -> NormalizedEffect {
        let mut record = fx::record(
            SourceType::Trait,
            DEATH_PERCEPTION,
            "Death Perception",
            EffectCategory::TriggeredEffect,
            15.0,
            TriggerRule::Conditional,
        );
        record.inner_category = Some(EffectCategory::CritDamagePct);
        record.prerequisite = Some(Prerequisite {
            in_shroud: Some(true),
            ..Default::default()
        });
        record
    }

    /// A strike burst on entry (coefficient form), so totals move.
    fn entry_burst() -> NormalizedEffect {
        fx::record(
            SourceType::Trait,
            SOUL_BARBS,
            "Soul Barbs",
            EffectCategory::StrikeDamagePct,
            0.5,
            TriggerRule::OnShroudEnter,
        )
    }

    /// The fixture with the engine's resource rules on an open profile, the
    /// given records loaded, trace on. Returns the report and the names of
    /// the buffs still on the player when the fight ends.
    pub(super) fn open_with(
        opener: &[u32],
        effects: &[&NormalizedEffect],
        duration_ms: u32,
        enemy_events: Vec<EnemyEvent>,
    ) -> (WvwCombatReport, Vec<String>) {
        open_with_skills(None, opener, effects, duration_ms, enemy_events)
    }

    /// `open_with` on `skills` when given (the fixture's prepared bar plus
    /// whatever the test appends); the resource rules stay the engine's.
    fn open_with_skills(
        skills: Option<Vec<RotationSkill>>,
        opener: &[u32],
        effects: &[&NormalizedEffect],
        duration_ms: u32,
        enemy_events: Vec<EnemyEvent>,
    ) -> (WvwCombatReport, Vec<String>) {
        let db = fx::db();
        let build = fx::build();
        let (ctx, _) = fx::scenario();
        let p = prepared();
        let (rules, complete, _) = engine::wvw_resource_rules(
            &build,
            &p.skills,
            &db,
            "Necromancer",
            &ctx,
            p.params.max_health,
        );
        let skills = skills.unwrap_or_else(|| p.skills.clone());
        let mut timeline = Timeline::new(
            &skills,
            &p.params,
            open_profile(duration_ms, enemy_events),
            still_enemy(),
            effects,
            &rules,
            complete,
            Vec::new(),
        );
        // Record fixtures script one or two strikes at most; a full
        // endurance pool would evade them outright (reactive dodge, see
        // `tick_endurance_and_dodge`) and leave nothing to measure.
        timeline.endurance.current = 0.0;
        timeline.opener = opener;
        timeline.trace_enabled = true;
        timeline.trace_loaded_unmodeled();
        timeline.run();
        let buffs = timeline.buffs.iter().map(|b| b.name.clone()).collect();
        (timeline.report(), buffs)
    }

    /// US1 positive control: the entry record fires exactly once, at the
    /// entry instant, and its boon is on the player.
    #[test]
    fn necro_shroud_enter_fires_once_at_entry() {
        let record = speed_of_shadows();
        let (report, buffs) = open_with(&fx::opener(), &[&record], 8_000, vec![]);
        let entry = events(&report, TraceKind::ShroudEntered, "Reaper's Shroud")
            .first()
            .map(|e| e.t_ms)
            .expect("the opener enters shroud");
        let fired = events(&report, TraceKind::TraitFired, "Speed of Shadows");
        assert_eq!(
            fired.len(),
            1,
            "the entry record fires once at the entry; trace: {:?}",
            report.trace
        );
        assert_eq!(fired[0].t_ms, entry, "fires at the entry instant");
        assert!(fired[0].detail.ends_with("at entry"), "{}", fired[0].detail);
        assert!(
            buffs.iter().any(|b| b == "Swiftness"),
            "Swiftness on the player: {buffs:?}"
        );
        assert_eq!(report.trait_fire_counts.get("Speed of Shadows"), Some(&1));
        assert!(
            !report
                .unmodeled_sources
                .iter()
                .any(|s| s.starts_with("Speed of Shadows")),
            "a fired record is off the coverage line: {:?}",
            report.unmodeled_sources
        );
    }

    /// US1: the exit record fires once for each way the shroud can end.
    #[test]
    fn necro_shroud_exit_fires_for_every_why() {
        let record = fury_on_exit();

        // The builder does not put the flip skill on the bar; the exit rule
        // for it exists (engine::wvw_resource_rules), so append the skill.
        let p = prepared();
        let mut exit = p
            .skills
            .iter()
            .find(|s| s.skill_id == fx::REAPER_SHROUD)
            .expect("entry skill")
            .clone();
        exit.skill_id = fx::EXIT_SHROUD;
        exit.name = "Exit Reaper's Shroud".into();
        exit.weapon_set = super::super::SHROUD_SET;
        exit.cooldown_ms = 0;
        exit.effects.clear();
        let mut skills = p.skills.clone();
        skills.push(exit);
        let (by_skill, _) = open_with_skills(
            Some(skills),
            &fx::opener_shroud_exit(),
            &[&record],
            8_000,
            vec![],
        );
        let fired = events(&by_skill, TraceKind::TraitFired, "Fury on exit");
        assert_eq!(fired.len(), 1, "exit by skill: {:?}", by_skill.trace);
        assert!(
            fired[0].detail.contains("at exit (exit skill)"),
            "{}",
            fired[0].detail
        );

        let (by_drain, _) = open_with(&fx::opener(), &[&record], 15_000, vec![]);
        let fired = events(&by_drain, TraceKind::TraitFired, "Fury on exit");
        let exited = events(&by_drain, TraceKind::ShroudExited, "Reaper's Shroud");
        assert_eq!(fired.len(), 1, "exit by drain: {:?}", by_drain.trace);
        assert!(fired[0].detail.contains("at exit (life force 0)"));
        assert_eq!(fired[0].t_ms, exited[0].t_ms, "fires at the exit instant");

        let (by_damage, _) = open_with(
            &fx::opener(),
            &[&record],
            6_000,
            vec![EnemyEvent {
                at_ms: 3_000,
                kind: EnemyEventKind::Strike {
                    damage: 6_000.0,
                    unblockable: true,
                },
            }],
        );
        let fired = events(&by_damage, TraceKind::TraitFired, "Fury on exit");
        let exited = events(&by_damage, TraceKind::ShroudExited, "Reaper's Shroud");
        assert_eq!(fired.len(), 1, "exit by damage: {:?}", by_damage.trace);
        assert_eq!(fired[0].t_ms, exited[0].t_ms);
        assert!(
            exited[0].t_ms <= 3_100,
            "the strike empties the pool before the drain would: {exited:?}"
        );
    }

    /// US1: an in-shroud conditional raises shroud-skill strikes only, and
    /// the shroud-bonus traces bracket the shroud.
    #[test]
    fn necro_in_shroud_bonus_active_only_inside() {
        let record = death_perception_in_shroud();
        let (with, _) = open_with(&fx::opener(), &[&record], 15_000, vec![]);
        let (without, _) = open_with(&fx::opener(), &[], 15_000, vec![]);
        assert_eq!(
            landed(&with, "Gravedigger"),
            landed(&without, "Gravedigger"),
            "outside shroud nothing changes"
        );
        let inside = |report: &WvwCombatReport| -> f64 {
            landed(report, "Life Rend")
                .iter()
                .chain(landed(report, "Soul Spiral").iter())
                .sum()
        };
        let (bonus, plain) = (inside(&with), inside(&without));
        assert!(plain > 0.0, "shroud skills land: {:?}", without.trace);
        assert!(
            bonus > plain * 1.01,
            "shroud strikes rise with the bonus: {bonus} vs {plain}"
        );
        let on = events(&with, TraceKind::ShroudBonusActive, "Death Perception");
        let off = events(&with, TraceKind::ShroudBonusEnded, "Death Perception");
        let entered = events(&with, TraceKind::ShroudEntered, "Reaper's Shroud");
        let exited = events(&with, TraceKind::ShroudExited, "Reaper's Shroud");
        assert_eq!(on.len(), 1, "{:?}", with.trace);
        assert_eq!(off.len(), 1, "{:?}", with.trace);
        assert_eq!(on[0].t_ms, entered[0].t_ms);
        assert_eq!(off[0].t_ms, exited[0].t_ms);
        assert_eq!(on[0].detail, "×1.15 crit damage");
        assert!(
            !with
                .unmodeled_sources
                .iter()
                .any(|s| s.starts_with("Death Perception")),
            "{:?}",
            with.unmodeled_sources
        );
    }

    /// US1 (Scourge rule): Desert Shroud is the entry; Manifest Sand Shade
    /// costs life force but is not a shroud, so entry records ignore it.
    #[test]
    fn necro_desert_shroud_is_the_scourge_entry() {
        let db = fx::db();
        let build = fx::build();
        let (ctx, _) = fx::scenario();
        let p = prepared();
        let (rules, _, _) = engine::wvw_resource_rules(
            &build,
            &p.skills,
            &db,
            "Necromancer",
            &ctx,
            p.params.max_health,
        );
        let template = p
            .skills
            .iter()
            .find(|s| s.skill_id == fx::REAPER_SHROUD)
            .expect("the fixture's entry skill")
            .clone();
        let mut desert = template.clone();
        desert.skill_id = DESERT_SHROUD;
        desert.name = "Desert Shroud".into();
        let mut shade = template;
        shade.skill_id = MANIFEST_SAND_SHADE;
        shade.name = "Manifest Sand Shade".into();
        let skills = vec![shade, desert];
        let mut entry_rule = rules
            .iter()
            .find(|r| r.skill_id == fx::REAPER_SHROUD)
            .expect("the entry rule")
            .clone();
        entry_rule.skill_id = DESERT_SHROUD;
        entry_rule.entry_floor = 0.0;
        entry_rule.cost = 0.0;
        let shade_rule = SkillResourceRule {
            skill_id: MANIFEST_SAND_SHADE,
            kind: ResourceKind::LifeForce,
            ..Default::default()
        };
        let rules = vec![shade_rule, entry_rule];
        let record = speed_of_shadows();
        let mut timeline = Timeline::new(
            &skills,
            &p.params,
            open_profile(4_000, vec![]),
            still_enemy(),
            &[&record],
            &rules,
            true,
            Vec::new(),
        );
        timeline.opener = &[MANIFEST_SAND_SHADE, DESERT_SHROUD];
        timeline.trace_enabled = true;
        timeline.run();
        let report = timeline.report();
        assert!(
            events(&report, TraceKind::ShroudEntered, "Manifest Sand Shade").is_empty(),
            "the shade is not an entry: {:?}",
            report.trace
        );
        let entered = events(&report, TraceKind::ShroudEntered, "Desert Shroud");
        assert_eq!(entered.len(), 1, "{:?}", report.trace);
        let fired = events(&report, TraceKind::TraitFired, "Speed of Shadows");
        assert_eq!(fired.len(), 1, "{:?}", report.trace);
        assert_eq!(fired[0].t_ms, entered[0].t_ms);
        assert!(fired[0].t_ms > 0, "the shade cast resolved first");
    }

    /// US1: removing the trait record changes the totals.
    #[test]
    fn necro_removed_trait_changes_results() {
        let record = entry_burst();
        let (with, _) = open_with(&fx::opener(), &[&record], 8_000, vec![]);
        let (without, _) = open_with(&fx::opener(), &[], 8_000, vec![]);
        assert_eq!(events(&with, TraceKind::TraitFired, "Soul Barbs").len(), 1);
        assert!(events(&without, TraceKind::TraitFired, "Soul Barbs").is_empty());
        assert!(
            with.total_damage > without.total_damage,
            "{} vs {}",
            with.total_damage,
            without.total_damage
        );
    }

    /// US1: the same records, the same fight, ten times.
    #[test]
    fn necro_results_repeat_identically() {
        let records = [
            speed_of_shadows(),
            death_perception_in_shroud(),
            entry_burst(),
        ];
        let refs: Vec<&NormalizedEffect> = records.iter().collect();
        let (first, _) = open_with(&fx::opener(), &refs, 10_000, vec![]);
        for _ in 0..9 {
            let (again, _) = open_with(&fx::opener(), &refs, 10_000, vec![]);
            assert_eq!(again.total_damage, first.total_damage);
            assert_eq!(again.trace, first.trace);
            assert_eq!(again.coverage, first.coverage);
            assert_eq!(again.trait_fire_counts, first.trait_fire_counts);
        }
    }

    /// Spec edge case: a build that cannot enter shroud leaves the entry
    /// record on the coverage line with the shroud-floor reason.
    #[test]
    fn necro_shroud_trigger_without_shroud_floor_never_fires() {
        let record = speed_of_shadows();
        let (report, _) = open_with(&[fx::REAPER_SHROUD], &[&record], 1_000, vec![]);
        assert!(events(&report, TraceKind::TraitFired, "Speed of Shadows").is_empty());
        assert!(
            !report.shroud_refusals.is_empty(),
            "the entry was refused: {:?}",
            report.trace
        );
        assert!(
            report
                .unmodeled_sources
                .iter()
                .any(|s| s == "Speed of Shadows (shroud never entered)"),
            "{:?}",
            report.unmodeled_sources
        );
        assert!(report
            .coverage
            .iter()
            .any(|e| { e.name == "Speed of Shadows" && e.class == ReasonClass::NoFiringSite }));
    }
}
