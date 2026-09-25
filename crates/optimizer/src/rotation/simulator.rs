//! Time-step rotation simulator with DPCT-optimal skill scheduling.
//!
//! Runs a skill rotation over a configurable duration, selecting skills by
//! **damage per cast time (DPCT)** — the skill that deals the most damage per
//! second of animation lock is always used first. Auto-attacks fill gaps.
//!
//! Supports weapon swapping: skills tagged with `weapon_set` 1 or 2 are only
//! usable from the active weapon set, with a 10-second swap cooldown.
//!
//! Design philosophy: Pure damage output is NOT the only goal. Build quality
//! is measured by the ability to DELIVER damage (CC, survivability, control)
//! as much as the raw DPS number. The simulator captures:
//! - Raw DPS potential (strike + condition)
//! - Self-buff uptime (can you maintain your own damage buffs?)
//! - Condition uptime (are conditions actually applied consistently?)
//! - CC availability (stunbreaks, stability sources)

use std::collections::HashMap;

use gw2_core::types::GameMode;

#[cfg(test)]
use super::combat_model::TimedFoeCondition;
use super::combat_model::{
    kit_escape_kinds, kit_has_corrupt, kit_has_cover_answer, kit_has_interrupt,
    kit_has_mobility_out, kit_has_stability_cover, kit_has_strip, setup_priority,
    setup_window_ms_for_mode, EnemyDummy, TargetState,
};
use super::combo::{ComboEngine, ComboOutcome, ComboOutcomeEffect, ComboSite, SELF_COMBATANT_ID};
use super::skill_timings::{HUMAN_DELAY_MS, MIN_SKILL_GAP_MS};
use super::{CoverKind, RotationSkill, SimulationResult, SkillEffect, SkillSlot, SkillUsage};
use crate::scoring::{
    OptimizationWeights, PROTECTION_REDUCTION, REALIZED_BOON_NORM, REALIZED_CONDI_DPS_NORM,
    REALIZED_CONTROL_NORM, REALIZED_HEALING_NORM, REALIZED_STRIKE_DPS_NORM,
};

/// Tick resolution for the simulation (ms per step).
const TICK_MS: u32 = 100;

/// Default simulation duration (30 seconds — standard GW2 benchmark window).
pub const DEFAULT_DURATION_MS: u32 = 30_000;

/// GW2 weapon swap cooldown (10 seconds in-combat).
const WEAPON_SWAP_COOLDOWN_MS: u32 = 10_000;

/// Brief invulnerability on falling down (wiki Downed / Invulnerability).
const DOWNED_INVULN_MS: u32 = 1_000;

/// WvW/PvP finisher channel. Interruptible; Quickness/Slow do not apply.
const STOMP_MS: u32 = 3_500;

/// Conditions tick every 1 second.
const CONDITION_TICK_INTERVAL_MS: u32 = 1000;

/// Reference armor value for damage calculation, loaded from data.
/// Source: https://wiki.guildwars2.com/wiki/Damage
pub(super) fn reference_armor() -> f64 {
    crate::data::universal_formulas::formulas().tooltip_reference_armor
}

/// One distinct condition name in a sim, with its tick formula resolved once.
/// Tick damage is linear in condition damage (wiki), so a 60s run never
/// looks the formula up again: 25 Bleeding stacks x 600 ticks did, and cost
/// more than the whole gate simulation.
#[derive(Debug, Clone)]
struct ConditionSlot {
    name: String,
}

/// Active buff being tracked.
#[derive(Debug, Clone)]
struct BuffInstance {
    remaining_ms: u32,
    kind: BuffKind,
    /// Index into `SimState::buff_slots`.
    slot: usize,
    /// The skill that applied it reaches allies (see `skill_reaches_allies`).
    /// A signet that mights only its owner is not boon support.
    ally_facing: bool,
}

/// What the scheduler asks of a buff every tick, resolved once at push time
/// instead of a string compare per instance per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuffKind {
    Might,
    Fury,
    Quickness,
    Alacrity,
    /// Index into `SOFT_CONTROL`.
    SoftControl(u8),
    Other,
}

fn buff_kind(name: &str) -> BuffKind {
    if name.eq_ignore_ascii_case("Might") {
        return BuffKind::Might;
    }
    if name.eq_ignore_ascii_case("Fury") {
        return BuffKind::Fury;
    }
    if name.eq_ignore_ascii_case("Quickness") {
        return BuffKind::Quickness;
    }
    if name.eq_ignore_ascii_case("Alacrity") {
        return BuffKind::Alacrity;
    }
    let canonical = crate::data::boon_condition_formulas::canonical_condition_name(name);
    match SOFT_CONTROL
        .iter()
        .position(|s| s.eq_ignore_ascii_case(canonical))
    {
        Some(i) => BuffKind::SoftControl(i as u8),
        None => BuffKind::Other,
    }
}

/// A skill's cast value with everything that does not change during a sim
/// folded in advance. Only the strike term depends on live Might and Fury.
#[derive(Debug, Clone, Copy)]
struct StaticCast {
    /// Strike damage per unit of effective power at crit factor 1.0.
    strike_coeff: f64,
    /// Every other axis of `CastValue`, strike zeroed.
    rest: CastValue,
    cast_time_s: f64,
}

impl StaticCast {
    /// `enemy_stability`: hard CC lands nothing through Stability, so the
    /// scheduler must not spend casts on it; soft control still applies.
    fn new(skill: &RotationSkill, params: &SimParams, enemy_stability: bool) -> Self {
        let strike_coeff: f64 = skill
            .effects
            .iter()
            .map(|effect| match effect {
                SkillEffect::StrikeDamage {
                    hit_count,
                    dmg_multiplier,
                } => {
                    params.weapon_strength / reference_armor()
                        * dmg_multiplier
                        * (*hit_count as f64)
                        * params.strike_mult
                }
                _ => 0.0,
            })
            .sum();
        let mut rest = skill_cast_value(
            skill,
            params.power,
            params.condition_damage,
            params.weapon_strength,
            params,
            0.0,
            false,
        );
        rest.strike = 0.0;
        if enemy_stability {
            let hard_control_s: f64 = skill
                .effects
                .iter()
                .map(|effect| match effect {
                    SkillEffect::CrowdControl { duration_ms, .. } => *duration_ms as f64 / 1000.0,
                    _ => 0.0,
                })
                .sum();
            rest.control_s = (rest.control_s - hard_control_s).max(0.0);
        }
        Self {
            strike_coeff,
            rest,
            cast_time_s: (skill.cast_time_ms + HUMAN_DELAY_MS + MIN_SKILL_GAP_MS) as f64 / 1000.0,
        }
    }
}

/// Internal state for a skill's cooldown.
#[derive(Debug, Clone)]
struct SkillState {
    /// Milliseconds remaining before this skill can be used again.
    cooldown_remaining_ms: u32,
}

/// Combat inputs for a rotation sim. `precision == 0` skips the crit term so
/// existing CC-only tests keep the same strike numbers.
#[derive(Debug, Clone)]
pub struct SimParams {
    pub power: f64,
    pub condition_damage: f64,
    pub weapon_strength: f64,
    pub precision: f64,
    pub ferocity: f64,
    pub crit_chance_bonus: f64,
    /// Fury critical-chance bonus in percentage points for the active mode.
    pub fury_crit_chance_bonus: f64,
    pub strike_mult: f64,
    pub condition_mult: f64,
    pub condition_duration_mult: f64,
    pub boon_duration_mult: f64,
    pub healing_power: f64,
    pub healing_mult: f64,
    pub max_health: f64,
    pub armor: f64,
    pub mode: GameMode,
    /// When set, the scheduler ranks casts by radar-weighted worth instead
    /// of damage per cast time, so heals, boons and control get cast on an
    /// open dummy. `None` is the gate simulation's pure DPCT.
    pub intent: Option<OptimizationWeights>,
    /// Target-conditional percents evaluated at land against live TargetState.
    pub deferred_target: Vec<crate::combat::DeferredTargetModifier>,
    /// Elite spec 56 (Weaver): dual attunement stashes outgoing primary.
    pub weaver: bool,
    /// The build's profession form, if it has one the data describes.
    /// `None` leaves the [`super::SHROUD_SET`] bar stowed for the whole run.
    pub form: Option<FormSpec>,
    /// Trait records fired by an event, with or without a form. Built by
    /// `engine::trait_procs_for_build` from record fields; a record's
    /// `in_shroud` prerequisite is [`TriggeredProc::in_form`], which only
    /// a form satisfies.
    pub triggered: Vec<TriggeredProc>,
    /// Additive-bucket sums inside `strike_mult` / `condition_mult`
    /// (fractions; `data/formulas/modifier_buckets.json`), so a timed
    /// modifier from an additive source joins that bucket.
    pub strike_add: f64,
    pub condition_add: f64,
    /// Per-condition damage factors by canonical name (Hidden Barbs:
    /// `Bleeding` 1.20 in PvE), on top of `condition_mult`, multiplicative
    /// as `DamageModifiers::total_condi_mult_for`. Absent means 1.
    pub condition_type_mults: HashMap<String, f64>,
    /// Always-on shares the fact parser folded into the multipliers for
    /// traits whose records this simulation plays on their own clock.
    pub folded: FoldedShares,
}

/// Always-on shares taken out of [`SimParams`] at the start of a flow run,
/// because a record now plays the same trait effect timed or in form (Soul
/// Barbs' parsed "Damage Increase", Death Perception's crit damage). The
/// WvW timeline never reads this.
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedShares {
    /// Product of the removed multiplicative strike factors.
    pub strike_mult: f64,
    /// Sum of the removed additive-bucket strike fractions.
    pub strike_add: f64,
    pub condition_mult: f64,
    pub condition_add: f64,
    /// Ferocity removed (15 per critical-damage percentage point).
    pub ferocity: f64,
}

impl Default for FoldedShares {
    fn default() -> Self {
        Self {
            strike_mult: 1.0,
            strike_add: 0.0,
            condition_mult: 1.0,
            condition_add: 0.0,
            ferocity: 0.0,
        }
    }
}

/// The damage term a record's modifier changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModAxis {
    Strike,
    Condition,
    /// Critical damage percentage points.
    CritDamage,
}

/// One percent modifier from a record: `percent` points on `axis`,
/// `additive` when its source is in the additive bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct DamageMod {
    pub axis: ModAxis,
    pub percent: f64,
    pub additive: bool,
}

/// A fired [`FormProc::Modifier`]: live while any stack is unexpired.
#[derive(Debug, Clone)]
struct LiveMod {
    source: String,
    modifier: DamageMod,
    expiries: Vec<u32>,
}

/// A profession form played as a timed state (Necromancer shroud, Druid
/// Celestial Avatar): while in it the weapon bar is stowed and the
/// [`super::SHROUD_SET`] bar is out; utilities stay usable. Every amount is
/// in pool units. Built by `engine::form_for_build` from data
/// (`data/formulas/shroud.json`, `data/formulas/forms.json`, the entry
/// skill's API `cost` and `Duration` facts, the build's trait records), so
/// the simulator holds no profession branch.
///
/// Scheduler policy: enter when the entry is off recharge, the pool meets
/// the floor, and either the pool is full (further gains would be lost) or
/// the form bar's best ready skill outranks the weapon bar's. Leave when the
/// pool is empty, or by the exit skill once every non-auto form skill is
/// recharging and a ready weapon skill outranks the form's best.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormSpec {
    pub name: String,
    /// Entry skill: one cast is one entry.
    pub entry_skill_id: u32,
    pub pool_cap: f64,
    /// Pool at the fight's start: full when the pool persists out of
    /// combat (players open a fight, a golem benchmark above all, with it
    /// full), else empty.
    pub initial_pool: f64,
    /// Minimum pool to enter.
    pub entry_floor: f64,
    pub drain_per_second: f64,
    /// Entry recharge, started when the form ends.
    pub recharge_ms: u32,
    /// Pool per landed strike (astral force).
    pub gain_per_strike: f64,
    /// Whether gains land while in the form (life force: yes; astral
    /// force: no).
    pub gains_in_form: bool,
    /// Fraction of the remaining pool kept on a voluntary exit.
    pub exit_keep: f64,
    /// The pool is life force (`GainsLifeForce` records credit it).
    pub life_force: bool,
    /// `(skill id, pool on use, pool per landed strike)`: the API's
    /// `Life Force` and `Life Force Per Hit` facts.
    pub skill_gains: Vec<(u32, f64, f64)>,
    pub on_enter: Vec<FormProc>,
    pub on_exit: Vec<FormProc>,
    /// `(interval ms, proc)`, fired on entry and every interval while in.
    pub periodic: Vec<(u32, FormProc)>,
    /// Modifiers live for as long as the form stands (`Conditional`
    /// records with the `in_shroud` prerequisite).
    pub while_in: Vec<DamageMod>,
    /// What the form does that is not played, named for the gap line.
    pub unmodelled: Vec<String>,
}

/// What just happened, for [`SimParams::triggered`].
enum ProcEvent<'a> {
    /// The skill at this index was cast.
    Cast(usize),
    /// This condition was inflicted on the foe.
    Condition(&'a str),
    /// A strike landed on the foe with this critical chance (0..=1).
    Hit { crit: f64 },
    /// A simulation tick passed.
    Tick,
}

/// A [`FormProc`] fired by an event rather than by the form's own entry,
/// exit or timer.
#[derive(Debug, Clone, PartialEq)]
pub struct TriggeredProc {
    pub on: ProcTrigger,
    /// Internal cooldown; 0 fires on every event. For
    /// [`ProcTrigger::Periodic`] the interval.
    pub icd_ms: u32,
    /// The record's `in_shroud` prerequisite: `Some(true)` only in the
    /// form, `Some(false)` only out of it.
    pub in_form: Option<bool>,
    /// The weapon set the source is socketed on (a sigil): live only while
    /// that set is held, or was held when the form was entered. 0: any.
    pub weapon_set: u8,
    /// `SelfBoon` / `SelfBoonAbsent` gates: `(boon, must carry)`.
    pub self_boons: Vec<(String, bool)>,
    pub proc_: FormProc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcTrigger {
    /// A cast of a skill this scope admits (`OnSkillUse`).
    SkillUse(crate::data::normalized_effects::TriggerScope),
    /// A cast of this skill: a skill's own `OnSkillUse` record.
    OwnCast(u32),
    /// A landed strike that crits (`OnCrit`). The flow sim averages crits,
    /// so each strike adds its crit chance as probability mass and the
    /// record fires once a whole proc's worth has gathered (the WvW
    /// timeline's expected-value on-crit reading).
    Crit,
    /// Inflicting the named condition on the foe, any when `None`
    /// (`OnConditionApplied`).
    ConditionApplied(Option<String>),
    /// A landed strike (`OnHit`).
    // ponytail: condition ticks do not fire it (the record's "strike or
    // condition tick"); add a tick event if a condition build's ICD-free
    // on-hit record matters.
    Hit,
    /// Every `icd_ms` from the fight's start (`Periodic`).
    Periodic,
}

/// What a trait record does when it fires: by the form (`OnShroudEnter`,
/// `OnShroudExit`, `Periodic` with `in_shroud`) or by an event
/// ([`TriggeredProc`]).
#[derive(Debug, Clone, PartialEq)]
pub enum FormProc {
    Buff {
        name: String,
        stacks: u32,
        duration_ms: u32,
        ally: bool,
    },
    /// Pool units.
    Gain(f64),
    /// A condition on the foe, through the skill path (condition duration
    /// applies).
    Condition {
        name: String,
        stacks: u32,
        duration_ms: u32,
    },
    /// A timed damage modifier: each firing adds a stack expiring after
    /// `duration_ms`, at most `max_stacks` held; `refresh_all` renews every
    /// held stack (`StackingRule::RefreshAllStacks`). One live entry per
    /// `source` and axis.
    Modifier {
        source: String,
        modifier: DamageMod,
        duration_ms: u32,
        max_stacks: u32,
        refresh_all: bool,
    },
}

impl SimParams {
    /// Test convenience: PvE, precision/ferocity 0 (skips crit), fury 25,
    /// 20k health / 2k armor. Shipping paths must build a full [`SimParams`]
    /// and call [`simulate_with`].
    pub fn basic(power: f64, condition_damage: f64, weapon_strength: f64) -> Self {
        Self {
            power,
            condition_damage,
            weapon_strength,
            precision: 0.0,
            ferocity: 0.0,
            crit_chance_bonus: 0.0,
            fury_crit_chance_bonus: 25.0,
            strike_mult: 1.0,
            condition_mult: 1.0,
            condition_duration_mult: 1.0,
            boon_duration_mult: 1.0,
            healing_power: 0.0,
            healing_mult: 1.0,
            max_health: 20_000.0,
            armor: 2_000.0,
            mode: GameMode::PvE,
            intent: None,
            deferred_target: Vec::new(),
            weaver: false,
            form: None,
            triggered: Vec::new(),
            strike_add: 0.0,
            condition_add: 0.0,
            condition_type_mults: HashMap::new(),
            folded: FoldedShares::default(),
        }
    }
}

impl SimParams {
    /// The per-condition factor for `condition` ([`Self::condition_type_mults`]).
    pub fn condition_type_mult(&self, condition: &str) -> f64 {
        let name = crate::data::boon_condition_formulas::canonical_condition_name(condition);
        self.condition_type_mults.get(name).copied().unwrap_or(1.0)
    }
}

#[cfg(test)]
/// Run a rotation simulation with the given skills and parameters.
///
/// Skills should have their `weapon_set` field set:
/// - 0 = always available (heal, utility, elite, profession)
/// - 1 = weapon set 1
/// - 2 = weapon set 2
///
/// The simulator uses DPCT (damage per cast time) to optimally schedule skills,
/// and automatically weapon-swaps when the active set's skills are on cooldown.
pub fn simulate(
    skills: &[RotationSkill],
    duration_ms: u32,
    power: f64,
    condition_damage: f64,
    weapon_strength: f64,
) -> SimulationResult {
    simulate_with(
        skills,
        duration_ms,
        &SimParams::basic(power, condition_damage, weapon_strength),
        EnemyDummy::open(),
    )
}

#[cfg(test)]
/// Like [`simulate`], but the dummy can start with Protection/Stability.
pub fn simulate_against(
    skills: &[RotationSkill],
    duration_ms: u32,
    power: f64,
    condition_damage: f64,
    weapon_strength: f64,
    enemy: EnemyDummy,
) -> SimulationResult {
    simulate_with(
        skills,
        duration_ms,
        &SimParams::basic(power, condition_damage, weapon_strength),
        enemy,
    )
}

/// Full combat-aware simulation (mode, crit, strike modifiers).
pub fn simulate_with(
    skills: &[RotationSkill],
    duration_ms: u32,
    params: &SimParams,
    enemy: EnemyDummy,
) -> SimulationResult {
    simulate_with_target(skills, duration_ms, params, TargetState::from_seed(enemy))
}

/// Like [`simulate_with`], but the caller supplies a live [`TargetState`]
/// (Phase 3 Kent probes: pre-stacked Vulnerability, etc.).
pub fn simulate_with_target(
    skills: &[RotationSkill],
    duration_ms: u32,
    params: &SimParams,
    target: TargetState,
) -> SimulationResult {
    let duration = if duration_ms == 0 {
        DEFAULT_DURATION_MS
    } else {
        duration_ms
    };

    let mut sim = SimState::new(skills, duration, target, params.clone());
    sim.setup_until_ms = setup_window_ms_for_mode(duration, params.mode == GameMode::WvW);
    sim.run();
    sim.into_result()
}

/// Internal simulation state.
/// One strike of a cast, waiting for its moment.
#[derive(Debug, Clone)]
struct ScheduledStrike {
    at_ms: u32,
    skill_id: u32,
    dmg_multiplier: f64,
}

/// Flow `run` does not tick dodge. Flip to `false` only when `run` calls
/// `dodge_action.try_dodge`. Deleting this without wiring fails
/// `kent_flow_e0_dodge_emits_or_abstains`.
#[cfg(test)]
const FLOW_E0_DODGE_UNWIRED: bool = true;
#[cfg(test)]
const FLOW_E0_DODGE_UNWIRED_WHY: &str =
    "OnDodge is WvW-timeline-only; SimState holds Endurance/Dodge for a follow-up tick";

/// Flow `run` does not spawn or consume clones. Flip to `false` only when
/// `run` calls `spawn_clone`. Deleting this without wiring fails
/// `kent_flow_e4_illusion_not_dead`.
#[cfg(test)]
const FLOW_E4_ILLUSION_UNWIRED: bool = true;
#[cfg(test)]
const FLOW_E4_ILLUSION_UNWIRED_WHY: &str =
    "IllusionState is constructed but never spawn_clone/consume; clone bus is WvW-timeline-only";

struct SimState {
    /// E0: shared TriggerBus + Endurance/Dodge types with wvw_timeline.
    /// OnDisableFoe is wired here (E1 landed-disable emit). Live try_dodge / OnDodge
    /// remains WvW-only today; see `FLOW_E0_DODGE_UNWIRED`.
    trigger_bus: super::trigger_bus::TriggerBus,
    #[allow(dead_code)]
    endurance: super::trigger_bus::EndurancePool,
    #[allow(dead_code)]
    dodge_action: super::trigger_bus::DodgeAction,
    /// E3: shared AttunementState with wvw_timeline (sole current writer).
    attunement: super::attunement::AttunementState,
    /// One attune swap per flow-sim run (first-swap setup). After this, attunes never beat filler.
    attunement_swapped: bool,
    /// E4: shared IllusionState with wvw_timeline (sole clone-count writer).
    /// Unread in `run`; see `FLOW_E4_ILLUSION_UNWIRED`.
    #[allow(dead_code)]
    illusion: super::illusion::IllusionState,

    skills: Vec<RotationSkill>,
    skill_states: Vec<SkillState>,
    /// Auto-attack chain cursor (E11).
    auto_chain: super::AutoChain,
    duration_ms: u32,
    current_time_ms: u32,
    /// Time when the character is free to use the next skill.
    next_action_ms: u32,
    /// Strikes queued by `use_skill`, landed by `land_scheduled_strikes`.
    scheduled_hits: Vec<ScheduledStrike>,

    // Weapon swap
    active_weapon_set: u8,
    weapon_swap_cooldown_ms: u32,
    has_weapon_sets: bool,
    /// Prefer CC/strip/cover over DPCT until this time (0 = never).
    setup_until_ms: u32,
    /// Live foe ledger (conditions / disable / prot / stab / hp). Seeded from EnemyDummy.
    target: TargetState,
    downed: bool,
    invuln_until_ms: u32,
    stomp_ends_ms: u32,

    // Tracking
    condition_slots: Vec<ConditionSlot>,
    buffs: Vec<BuffInstance>,
    buff_slots: Vec<String>,
    /// Scratch: which buff slots had a live instance this tick.
    buff_seen: Vec<bool>,
    static_casts: Vec<StaticCast>,
    total_strike_damage: f64,
    total_condition_damage: f64,
    skill_casts: HashMap<u32, u32>,  // skill_id → cast count
    skill_damage: HashMap<u32, f64>, // skill_id → total damage
    /// Per condition slot: paid tick units (fractional last pulse).
    condition_ticks: Vec<f64>,
    /// Per buff slot: total ms active, summed over instances.
    buff_active_ms: Vec<u32>,
    total_healing: f64,
    /// Control-seconds in ms: hard CC non-overlapping, soft control at half.
    control_ms: f64,
    might_stack_ms: f64,
    /// Might seconds from ally-facing sources only.
    ally_might_stack_ms: f64,
    /// Healing and barrier this build put on OTHER people.
    ally_healing: f64,
    /// Per-slot boon uptime from ally-facing sources only.
    ally_buff_active_ms: Vec<u32>,
    params: SimParams,
    /// Live combo fields (Phase 4). World/sim state, not TargetState.
    combo: ComboEngine,
    /// Form state (see [`FormSpec`]); a copy of `params.form`.
    form: Option<FormSpec>,
    form_pool: f64,
    /// `Some(entered at)` while in the form.
    form_entered_ms: Option<u32>,
    weapon_set_before_form: u8,
    form_recharge_ms: u32,
    /// Time spent in the form.
    form_active_ms: u32,
    /// Next firing time per `FormSpec::periodic` entry.
    form_periodic_due: Vec<u32>,
    /// Capture only, never read by scheduling: (strike, condition) damage
    /// landed per second `[k, k+1)`.
    damage_seconds: Vec<(f64, f64)>,
    /// Capture only: `buff_seen` at each second's midpoint tick.
    buff_seen_mid_second: Vec<Vec<bool>>,
    /// Earliest next firing per `SimParams::triggered` entry (its ICD).
    triggered_ready_ms: Vec<u32>,
    /// Crit probability mass gathered per [`ProcTrigger::Crit`] entry
    /// since it last fired.
    triggered_mass: Vec<f64>,
    /// Fired [`FormProc::Modifier`]s.
    live_mods: Vec<LiveMod>,
    /// Nesting of `fire_triggered`: a proc's condition may fire records on
    /// that condition, but not without end.
    proc_depth: u8,
}

impl SimState {
    fn new(
        skills: &[RotationSkill],
        duration_ms: u32,
        target: TargetState,
        mut params: SimParams,
    ) -> Self {
        // Each folded share counts once: the record plays it now.
        let folded = std::mem::take(&mut params.folded);
        params.strike_mult *= (1.0 + params.strike_add - folded.strike_add)
            / (1.0 + params.strike_add)
            / folded.strike_mult;
        params.strike_add -= folded.strike_add;
        params.condition_mult *= (1.0 + params.condition_add - folded.condition_add)
            / (1.0 + params.condition_add)
            / folded.condition_mult;
        params.condition_add -= folded.condition_add;
        params.ferocity -= folded.ferocity;
        let skill_states = skills
            .iter()
            .map(|_| SkillState {
                cooldown_remaining_ms: 0,
            })
            .collect();

        let has_weapon_sets = skills.iter().any(|s| s.weapon_set > 0);
        let static_casts = skills
            .iter()
            .map(|skill| StaticCast::new(skill, &params, target.stability))
            .collect();

        Self {
            trigger_bus: super::trigger_bus::TriggerBus::new(),
            endurance: super::trigger_bus::EndurancePool::new_full(),
            dodge_action: super::trigger_bus::DodgeAction::new(),
            attunement: super::attunement::AttunementState::for_build(params.weaver),
            attunement_swapped: false,
            illusion: super::illusion::IllusionState::new(),
            skills: skills.to_vec(),
            skill_states,
            auto_chain: super::AutoChain::new(skills),
            duration_ms,
            current_time_ms: 0,
            next_action_ms: 0,
            scheduled_hits: Vec::new(),
            active_weapon_set: 1,
            weapon_swap_cooldown_ms: 0,
            has_weapon_sets,
            setup_until_ms: 0,
            downed: false,
            invuln_until_ms: 0,
            stomp_ends_ms: 0,
            target,
            condition_slots: Vec::new(),
            buffs: Vec::new(),
            buff_slots: Vec::new(),
            buff_seen: Vec::new(),
            static_casts,
            total_strike_damage: 0.0,
            total_condition_damage: 0.0,
            skill_casts: HashMap::new(),
            skill_damage: HashMap::new(),
            condition_ticks: Vec::new(),
            buff_active_ms: Vec::new(),
            total_healing: 0.0,
            control_ms: 0.0,
            might_stack_ms: 0.0,
            ally_might_stack_ms: 0.0,
            ally_healing: 0.0,
            ally_buff_active_ms: Vec::new(),
            form: params.form.clone(),
            form_pool: params.form.as_ref().map_or(0.0, |form| form.initial_pool),
            form_entered_ms: None,
            weapon_set_before_form: 1,
            form_recharge_ms: 0,
            form_active_ms: 0,
            form_periodic_due: Vec::new(),
            damage_seconds: vec![(0.0, 0.0); duration_ms.div_ceil(1000) as usize],
            buff_seen_mid_second: Vec::new(),
            triggered_ready_ms: vec![0; params.triggered.len()],
            triggered_mass: vec![0.0; params.triggered.len()],
            live_mods: Vec::new(),
            proc_depth: 0,
            params,
            combo: ComboEngine::new(),
        }
    }

    fn run(&mut self) {
        let power = self.params.power;
        let condition_damage = self.params.condition_damage;
        let weapon_strength = self.params.weapon_strength;
        while self.current_time_ms < self.duration_ms {
            let landed_before = (self.total_strike_damage, self.total_condition_damage);
            // Periodic records first: what fires at t counts for hits at t.
            self.fire_triggered(ProcEvent::Tick);
            // Tick conditions and buffs
            self.tick_conditions(condition_damage);
            self.tick_buffs();
            if self.current_time_ms % 1000 == 500 {
                self.buff_seen_mid_second.push(self.buff_seen.clone());
            }
            self.land_scheduled_strikes(power, weapon_strength);
            self.might_stack_ms += self.live_might_stacks() * TICK_MS as f64;
            self.ally_might_stack_ms += self.live_ally_might_stacks() * TICK_MS as f64;
            self.control_ms += self.soft_control_weight() * TICK_MS as f64;

            // Alacrity: +25% recharge (wiki 2018). 100ms wall = 125ms CD. 10s → 8s.
            let cd_tick = alacrity_cd_advance_ms(
                TICK_MS,
                self.buffs.iter().any(|b| b.kind == BuffKind::Alacrity),
            );
            for state in &mut self.skill_states {
                state.cooldown_remaining_ms = state.cooldown_remaining_ms.saturating_sub(cd_tick);
            }
            self.weapon_swap_cooldown_ms = self.weapon_swap_cooldown_ms.saturating_sub(TICK_MS);
            self.tick_form();

            if self.current_time_ms >= self.next_action_ms {
                self.decide_form(power);
                if self.has_weapon_sets && self.should_weapon_swap(power) {
                    self.weapon_swap();
                }

                if let Some(idx) = self.pick_skill(power) {
                    self.use_skill(idx, power, weapon_strength);
                }
            }

            self.capture_damage_since(landed_before);
            self.current_time_ms += TICK_MS;
        }
        // A cast that finishes as the window closes still delivered its hits.
        let landed_before = (self.total_strike_damage, self.total_condition_damage);
        self.current_time_ms = self.duration_ms;
        self.land_scheduled_strikes(power, weapon_strength);
        self.capture_damage_since(landed_before);
    }

    /// Adds the damage landed since `before` to the current second's bucket
    /// (the last one for the window-close landing).
    fn capture_damage_since(&mut self, before: (f64, f64)) {
        let Some(last) = self.damage_seconds.len().checked_sub(1) else {
            return;
        };
        let k = ((self.current_time_ms / 1000) as usize).min(last);
        self.damage_seconds[k].0 += self.total_strike_damage - before.0;
        self.damage_seconds[k].1 += self.total_condition_damage - before.1;
    }

    /// Pick the skill with the highest DPS-per-cast-time (DPCT) that is available.
    ///
    /// Availability means: off cooldown AND (weapon_set==0 OR weapon_set==active_set).
    /// Auto-attack (Weapon1 with cooldown 0) is used as filler when nothing else is ready.
    fn pick_skill(&self, power: f64) -> Option<usize> {
        let (effective_power, crit_factor) = self.strike_terms(power);
        let mut best_idx = None;
        let mut best_dpct = 0.0f64;
        let mut filler_idx = None;
        let mut attune_idx = None;

        for (i, skill) in self.skills.iter().enumerate() {
            if self.skill_states[i].cooldown_remaining_ms > 0 {
                continue;
            }

            if !self.is_skill_available(skill) {
                continue;
            }

            // Auto-attack = filler (always available, pick last).
            // Prefer the filler from the active weapon set over weapon_set==0.
            if skill.is_auto_attack() {
                if self.auto_chain.is_follow_up(i) {
                    continue;
                }
                match filler_idx {
                    None => filler_idx = Some(i),
                    Some(prev) => {
                        // Upgrade to active-set filler if current filler is generic
                        if self.skills[prev].weapon_set == 0
                            && skill.weapon_set == self.active_weapon_set
                        {
                            filler_idx = Some(i);
                        }
                    }
                }
                continue;
            }

            if let Some(element) = super::attunement::Element::from_skill_name(&skill.name) {
                if element != self.attunement.current && attune_idx.is_none() {
                    attune_idx = Some(i);
                }
                continue;
            }

            let dpct = self.priority(i, effective_power, crit_factor);
            if dpct > best_dpct {
                best_dpct = dpct;
                best_idx = Some(i);
            }
        }

        let filler_idx = filler_idx.map(|head| self.auto_chain.step(head, self.current_time_ms));

        if self.current_time_ms < self.setup_until_ms {
            let mut best_setup = None;
            let mut best_p = 0u32;
            for (i, skill) in self.skills.iter().enumerate() {
                if self.skill_states[i].cooldown_remaining_ms > 0 || !self.is_skill_available(skill)
                {
                    continue;
                }
                if skill.slot == SkillSlot::Weapon1 && skill.cooldown_ms == 0 {
                    continue;
                }
                let p = setup_priority(skill);
                if p > best_p {
                    best_p = p;
                    best_setup = Some(i);
                }
            }
            if best_p > 0 {
                return best_setup.or(filler_idx);
            }
        }

        // One attune swap per run (setup). After that attunes never beat filler.
        if !self.attunement_swapped {
            if let Some(i) = attune_idx {
                return Some(i);
            }
        }
        best_idx.or(filler_idx)
    }

    /// Check if a skill is usable given the current weapon set.
    fn is_skill_available(&self, skill: &RotationSkill) -> bool {
        if !self.has_weapon_sets || skill.weapon_set == 0 {
            return true; // non-weapon skill or no weapon sets in this sim
        }
        skill.weapon_set == self.active_weapon_set
    }

    /// Decide if we should weapon swap: all active weapon skills on CD,
    /// swap is available, and the other set has usable skills.
    fn should_weapon_swap(&self, power: f64) -> bool {
        // wiki `Death Shroud`: no weapon swap while in a form.
        if self.weapon_swap_cooldown_ms > 0 || self.form_entered_ms.is_some() {
            return false;
        }

        let other_set = if self.active_weapon_set == 1 { 2 } else { 1 };

        let (effective_power, crit_factor) = self.strike_terms(power);

        // Check: does the active set still have a skill worth casting? A
        // skill `pick_skill` would never cast (priority 0 — e.g. a
        // condition skill under a healer radar) is not a reason to stay.
        let has_active_weapon_ready = self.skills.iter().enumerate().any(|(i, s)| {
            s.weapon_set == self.active_weapon_set
                && s.slot != SkillSlot::Weapon1
                && self.skill_states[i].cooldown_remaining_ms == 0
                && self.priority(i, effective_power, crit_factor) > 0.0
        });

        if has_active_weapon_ready {
            return false; // still have skills to use on current set
        }

        // Check: does the other set have off-cooldown skills with DPCT > 0?
        self.skills.iter().enumerate().any(|(i, s)| {
            s.weapon_set == other_set
                && s.slot != SkillSlot::Weapon1
                && self.skill_states[i].cooldown_remaining_ms == 0
                && self.priority(i, effective_power, crit_factor) > 0.0
        })
    }

    /// Live strike terms shared by every candidate this decision: effective
    /// power with Might, and the crit factor with Fury.
    fn strike_terms(&self, power: f64) -> (f64, f64) {
        let fury_bonus = if self.buffs.iter().any(|b| b.kind == BuffKind::Fury) {
            self.params.fury_crit_chance_bonus
        } else {
            0.0
        };
        (
            power + self.live_might_stacks() * 30.0,
            strike_crit_factor_with_bonus(
                self.params.precision,
                self.params.ferocity,
                self.params.crit_chance_bonus + fury_bonus,
            ),
        )
    }

    /// Scheduling priority of skill `idx` per second of cast time, from its
    /// precomputed cast value. Same formula as `skill_dps_efficiency`.
    fn priority(&self, idx: usize, effective_power: f64, crit_factor: f64) -> f64 {
        let sc = &self.static_casts[idx];
        if sc.cast_time_s <= 0.0 {
            return 0.0;
        }
        let strike = sc.strike_coeff * effective_power * crit_factor;
        match &self.params.intent {
            Some(weights) => {
                intent_value(&CastValue { strike, ..sc.rest }, weights) / sc.cast_time_s
            }
            None => (strike + sc.rest.condition + sc.rest.boon_dps) / sc.cast_time_s,
        }
    }

    fn condition_slot(&mut self, name: &str) -> usize {
        let name = crate::data::boon_condition_formulas::canonical_condition_name(name);
        if let Some(i) = self.condition_slots.iter().position(|c| c.name == name) {
            return i;
        }
        self.condition_slots.push(ConditionSlot {
            name: name.to_string(),
        });
        self.condition_ticks.push(0.0);
        self.condition_slots.len() - 1
    }

    fn buff_slot(&mut self, name: &str) -> usize {
        if let Some(i) = self.buff_slots.iter().position(|b| b == name) {
            return i;
        }
        self.buff_slots.push(name.to_string());
        self.buff_active_ms.push(0);
        self.ally_buff_active_ms.push(0);
        self.buff_seen.push(false);
        self.buff_slots.len() - 1
    }

    /// Drain, periodic procs and recharge, once per tick.
    fn tick_form(&mut self) {
        self.form_recharge_ms = self.form_recharge_ms.saturating_sub(TICK_MS);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.form_entered_ms.is_none() {
            return;
        }
        self.form_active_ms += TICK_MS;
        let mut due = Vec::new();
        for (i, (interval, proc_)) in form.periodic.iter().enumerate() {
            if self.form_periodic_due[i] <= self.current_time_ms {
                self.form_periodic_due[i] += (*interval).max(TICK_MS);
                due.push(proc_.clone());
            }
        }
        self.form_pool -= form.drain_per_second * TICK_MS as f64 / 1_000.0;
        for proc_ in &due {
            self.fire_form_proc(proc_);
        }
        if self.form_pool <= 0.0 {
            self.form_pool = 0.0;
            self.exit_form(false);
        }
    }

    /// Best ready priority on bar `set` (its auto-attacks included), and
    /// whether any ready skill there is not an auto-attack.
    fn bar_best(&self, set: u8, power: f64) -> (f64, bool) {
        let (effective_power, crit_factor) = self.strike_terms(power);
        let mut best = 0.0f64;
        let mut non_auto_ready = false;
        for (i, skill) in self.skills.iter().enumerate() {
            if skill.weapon_set != set || self.skill_states[i].cooldown_remaining_ms > 0 {
                continue;
            }
            let priority = self.priority(i, effective_power, crit_factor);
            if priority <= 0.0 {
                continue;
            }
            best = best.max(priority);
            if !(skill.slot == SkillSlot::Weapon1 && skill.cooldown_ms == 0) {
                non_auto_ready = true;
            }
        }
        (best, non_auto_ready)
    }

    /// Enter or leave the form per the [`FormSpec`] policy.
    fn decide_form(&mut self, power: f64) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let (form_best, form_non_auto) = self.bar_best(super::SHROUD_SET, power);
        if self.form_entered_ms.is_some() {
            let (weapon_best, weapon_non_auto) = self.bar_best(self.weapon_set_before_form, power);
            if !form_non_auto && weapon_non_auto && weapon_best > form_best {
                self.exit_form(true);
            }
            return;
        }
        let full = self.form_pool >= form.pool_cap - 1e-9;
        if self.form_recharge_ms > 0
            || self.form_pool <= 0.0
            || self.form_pool < form.entry_floor
            || form_best <= 0.0
        {
            return;
        }
        let (weapon_best, _) = self.bar_best(self.active_weapon_set, power);
        if full || form_best > weapon_best {
            self.enter_form();
        }
    }

    fn enter_form(&mut self) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let on_enter = form.on_enter.clone();
        let entry = form.entry_skill_id;
        self.form_periodic_due = vec![self.current_time_ms; form.periodic.len()];
        self.weapon_set_before_form = self.active_weapon_set;
        self.active_weapon_set = super::SHROUD_SET;
        self.form_entered_ms = Some(self.current_time_ms);
        *self.skill_casts.entry(entry).or_insert(0) += 1;
        // Entry is instant, like a weapon swap.
        self.next_action_ms = self.current_time_ms.saturating_add(MIN_SKILL_GAP_MS);
        for proc_ in &on_enter {
            self.fire_form_proc(proc_);
        }
    }

    /// Leave the form: `voluntary` is the exit skill (keeps
    /// `exit_keep` of the pool), else the pool ran dry.
    fn exit_form(&mut self, voluntary: bool) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.form_entered_ms.is_none() {
            return;
        }
        let on_exit = form.on_exit.clone();
        let keep = form.exit_keep;
        let recharge = form.recharge_ms;
        // Exit records fire while the form still stands.
        for proc_ in &on_exit {
            self.fire_form_proc(proc_);
        }
        self.form_entered_ms = None;
        self.active_weapon_set = self.weapon_set_before_form;
        self.form_recharge_ms = recharge;
        if voluntary {
            self.form_pool *= keep;
            self.next_action_ms = self.current_time_ms.saturating_add(MIN_SKILL_GAP_MS);
        }
    }

    /// Grant `stacks` of `name`, each `remaining_ms` long. A boon that
    /// stacks in duration (`data/formulas/boons.json` `stacking_mode`; wiki
    /// `Effect stacking`: Quickness, Fury, Protection, ...) adds its time to
    /// what is already running, up to `max_duration`, instead of running
    /// beside it. One instance per audience: the self ledger is the longer
    /// of the two, so an ally-facing grant extends both and a self-only
    /// grant queues behind whatever the player already has.
    fn add_buff(&mut self, name: &str, stacks: u32, remaining_ms: u32, ally_facing: bool) {
        let kind = buff_kind(name);
        let slot = self.buff_slot(name);
        let cap_ms = crate::data::boon_condition_formulas::boons()
            .get(name)
            .filter(|b| {
                b.stacking_mode == crate::data::boon_condition_formulas::StackingMode::Duration
            })
            .map(|b| b.max_duration.map_or(u32::MAX, |s| s.saturating_mul(1_000)));
        let Some(cap_ms) = cap_ms else {
            for _ in 0..stacks {
                self.buffs.push(BuffInstance {
                    remaining_ms,
                    kind,
                    slot,
                    ally_facing,
                });
            }
            return;
        };
        let add = remaining_ms.saturating_mul(stacks);
        if add == 0 {
            return;
        }
        let live = |buffs: &[BuffInstance], ally: bool| {
            buffs
                .iter()
                .position(|b| b.slot == slot && b.ally_facing == ally && b.remaining_ms > 0)
        };
        let ally_ms = live(&self.buffs, true).map_or(0, |i| self.buffs[i].remaining_ms);
        let own = live(&self.buffs, false);
        let own_ms = own.map_or(0, |i| self.buffs[i].remaining_ms);
        let (audience, until) = if ally_facing {
            if let Some(i) = own {
                self.buffs[i].remaining_ms = own_ms.saturating_add(add).min(cap_ms);
            }
            (live(&self.buffs, true), ally_ms.saturating_add(add))
        } else {
            (own, own_ms.max(ally_ms).saturating_add(add))
        };
        let until = until.min(cap_ms);
        match audience {
            Some(i) => self.buffs[i].remaining_ms = until,
            None => self.buffs.push(BuffInstance {
                remaining_ms: until,
                kind,
                slot,
                ally_facing,
            }),
        }
    }

    fn fire_form_proc(&mut self, proc_: &FormProc) {
        match proc_ {
            FormProc::Buff {
                name,
                stacks,
                duration_ms,
                ally,
            } => {
                let remaining_ms =
                    (*duration_ms as f64 * self.params.boon_duration_mult).round() as u32;
                self.add_buff(name, *stacks, remaining_ms, *ally);
            }
            FormProc::Gain(amount) => self.gain_form_pool(*amount, true),
            FormProc::Condition {
                name,
                stacks,
                duration_ms,
            } => {
                self.inflict_condition(name, *stacks, *duration_ms);
                self.fire_triggered(ProcEvent::Condition(name));
            }
            FormProc::Modifier {
                source,
                modifier,
                duration_ms,
                max_stacks,
                refresh_all,
            } => {
                let now = self.current_time_ms;
                let until = now.saturating_add(*duration_ms);
                let live = match self
                    .live_mods
                    .iter_mut()
                    .position(|m| m.source == *source && m.modifier.axis == modifier.axis)
                {
                    Some(i) => &mut self.live_mods[i],
                    None => {
                        self.live_mods.push(LiveMod {
                            source: source.clone(),
                            modifier: modifier.clone(),
                            expiries: Vec::new(),
                        });
                        self.live_mods.last_mut().expect("just pushed")
                    }
                };
                live.expiries.retain(|at| *at > now);
                if *refresh_all {
                    live.expiries.iter_mut().for_each(|at| *at = until);
                }
                live.expiries.push(until);
                let excess = live
                    .expiries
                    .len()
                    .saturating_sub((*max_stacks).max(1) as usize);
                live.expiries.sort_unstable();
                live.expiries.drain(..excess);
            }
        }
    }

    /// A condition on the foe with the skill path's duration and stack cap.
    fn inflict_condition(&mut self, name: &str, stacks: u32, duration_ms: u32) {
        let cap = condition_stack_cap(name, &self.params.mode);
        let _ = self.condition_slot(name);
        let duration = (duration_ms as f64 * self.params.condition_duration_mult).round() as u32;
        self.target
            .apply_condition(name, stacks, duration, self.current_time_ms, cap as u32);
    }

    /// Live modifiers on `axis` with their stack counts, plus the form's
    /// `while_in` ones while it stands.
    fn live_mods_on(&self, axis: ModAxis) -> impl Iterator<Item = (&DamageMod, f64)> {
        let now = self.current_time_ms;
        let timed = self.live_mods.iter().map(move |m| {
            let stacks = m.expiries.iter().filter(|at| **at > now).count();
            (&m.modifier, stacks as f64)
        });
        let in_form = self
            .form
            .as_ref()
            .filter(|_| self.form_entered_ms.is_some())
            .map(|form| form.while_in.iter().map(|m| (m, 1.0)))
            .into_iter()
            .flatten();
        timed
            .chain(in_form)
            .filter(move |(m, stacks)| m.axis == axis && *stacks > 0.0)
    }

    /// Factor on `strike_mult` / `condition_mult` from live record
    /// modifiers: multiplicative sources multiply, additive ones join the
    /// bucket already inside the base multiplier.
    fn live_mod_factor(&self, axis: ModAxis) -> f64 {
        let bucket = match axis {
            ModAxis::Strike => self.params.strike_add,
            ModAxis::Condition => self.params.condition_add,
            ModAxis::CritDamage => return 1.0,
        };
        let (mut product, mut added) = (1.0, 0.0);
        for (m, stacks) in self.live_mods_on(axis) {
            let fraction = m.percent / 100.0 * stacks;
            if m.additive {
                added += fraction;
            } else {
                product *= 1.0 + fraction;
            }
        }
        product * (1.0 + bucket + added) / (1.0 + bucket)
    }

    /// Fire every [`SimParams::triggered`] record `event` admits that is off
    /// its internal cooldown and in the form state it asks for.
    fn fire_triggered(&mut self, event: ProcEvent<'_>) {
        if self.params.triggered.is_empty() || self.proc_depth >= 2 {
            return;
        }
        let in_form = self.form_entered_ms.is_some();
        // Sigils on the stowed weapon keep working in a form.
        let held_set = if in_form {
            self.weapon_set_before_form
        } else {
            self.active_weapon_set
        };
        let mut due = Vec::new();
        for (i, triggered) in self.params.triggered.iter().enumerate() {
            let admits = match (&triggered.on, &event) {
                (ProcTrigger::SkillUse(scope), ProcEvent::Cast(idx)) => {
                    super::wvw_timeline::skill_scope_admits(scope, &self.skills[*idx], &self.skills)
                }
                (ProcTrigger::OwnCast(id), ProcEvent::Cast(idx)) => {
                    self.skills[*idx].skill_id == *id
                }
                (ProcTrigger::ConditionApplied(want), ProcEvent::Condition(name)) => want
                    .as_deref()
                    .is_none_or(|want| super::wvw_timeline::foe_condition_name_eq(name, want)),
                (ProcTrigger::Hit, ProcEvent::Hit { .. })
                | (ProcTrigger::Crit, ProcEvent::Hit { .. })
                | (ProcTrigger::Periodic, ProcEvent::Tick) => true,
                _ => false,
            };
            let carries = |boon: &str| {
                self.buffs.iter().any(|b| {
                    b.remaining_ms > 0 && self.buff_slots[b.slot].eq_ignore_ascii_case(boon)
                })
            };
            if !admits
                || self.triggered_ready_ms[i] > self.current_time_ms
                || triggered.in_form.is_some_and(|want| want != in_form)
                || (triggered.weapon_set != 0 && triggered.weapon_set != held_set)
                || triggered
                    .self_boons
                    .iter()
                    .any(|(boon, want)| carries(boon) != *want)
            {
                continue;
            }
            if let (ProcTrigger::Crit, ProcEvent::Hit { crit }) = (&triggered.on, &event) {
                self.triggered_mass[i] += crit;
                if self.triggered_mass[i] < 1.0 {
                    continue;
                }
                self.triggered_mass[i] = 0.0;
            }
            self.triggered_ready_ms[i] = self.current_time_ms.saturating_add(triggered.icd_ms);
            due.push(triggered.proc_.clone());
        }
        self.proc_depth += 1;
        for proc_ in &due {
            self.fire_form_proc(proc_);
        }
        self.proc_depth -= 1;
    }

    /// Credit the form pool; `from_proc` gains land even in a form that
    /// gains nothing from attacks (a trait says so explicitly).
    fn gain_form_pool(&mut self, amount: f64, from_proc: bool) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if amount <= 0.0 || (!from_proc && self.form_entered_ms.is_some() && !form.gains_in_form) {
            return;
        }
        self.form_pool = (self.form_pool + amount).min(form.pool_cap);
    }

    /// Pool a skill's strike or cast earns: `(on use, per strike)`.
    fn form_gain_for(&self, skill_id: u32) -> (f64, f64) {
        let Some(form) = self.form.as_ref() else {
            return (0.0, 0.0);
        };
        let (on_use, per_hit) = form
            .skill_gains
            .iter()
            .find(|(id, _, _)| *id == skill_id)
            .map(|(_, on_use, per_hit)| (*on_use, *per_hit))
            .unwrap_or((0.0, 0.0));
        (on_use, per_hit + form.gain_per_strike)
    }

    /// Perform a weapon swap.
    fn weapon_swap(&mut self) {
        self.active_weapon_set = if self.active_weapon_set == 1 { 2 } else { 1 };
        self.weapon_swap_cooldown_ms = WEAPON_SWAP_COOLDOWN_MS;
        // Weapon swap is instant in GW2 (no cast time), but add minimal delay
        self.next_action_ms = self.current_time_ms.saturating_add(MIN_SKILL_GAP_MS);
    }

    /// Use a skill: apply effects, set cooldown, advance next_action time.
    /// One strike in flight: lands at `at_ms` with its per-strike coefficient,
    /// priced with the buffs live at that moment.
    fn land_scheduled_strikes(&mut self, power: f64, weapon_strength: f64) {
        let mut i = 0;
        while i < self.scheduled_hits.len() {
            if self.scheduled_hits[i].at_ms > self.current_time_ms {
                i += 1;
                continue;
            }
            let hit = self.scheduled_hits.remove(i);
            let effective_power = power + self.live_might_stacks() * 30.0;
            let fury_bonus = if self.buffs.iter().any(|b| b.kind == BuffKind::Fury) {
                self.params.fury_crit_chance_bonus
            } else {
                0.0
            };
            let crit_damage_pts: f64 = self
                .live_mods_on(ModAxis::CritDamage)
                .map(|(m, stacks)| m.percent * stacks)
                .sum();
            let mut damage =
                weapon_strength * effective_power / reference_armor() * hit.dmg_multiplier;
            damage *= strike_crit_factor_with_bonus(
                self.params.precision,
                self.params.ferocity
                    + crit_damage_pts
                        * crate::data::universal_formulas::formulas().ferocity_per_crit_damage_pct,
                self.params.crit_chance_bonus + fury_bonus,
            ) * self.params.strike_mult
                * self.live_mod_factor(ModAxis::Strike);
            if self.target.protection {
                damage *= crate::data::boon_condition_formulas::boons().protection_multiplier();
            }
            damage *= self
                .target
                .vulnerability_multiplier(self.current_time_ms, &self.params.mode);
            damage *= crate::combat::deferred_target_multiplier(
                &self.params.deferred_target,
                &self.target,
                self.current_time_ms,
                crate::combat::TargetModAxis::Strike,
            );
            self.total_strike_damage += damage;
            *self.skill_damage.entry(hit.skill_id).or_insert(0.0) += damage;
            self.apply_dummy_damage(damage);
            let (_, per_hit) = self.form_gain_for(hit.skill_id);
            self.gain_form_pool(per_hit, false);
            let crit = crit_chance_fraction(
                self.params.precision,
                self.params.crit_chance_bonus + fury_bonus,
            );
            self.fire_triggered(ProcEvent::Hit { crit });
        }
    }

    fn use_skill(&mut self, idx: usize, power: f64, weapon_strength: f64) {
        let skill = &self.skills[idx];
        let skill_id = skill.skill_id;
        let skill_name = skill.name.clone();
        let cast_time = skill.cast_time_ms;
        let cooldown = skill.cooldown_ms;
        let effects = skill.effects.clone();
        let ally_facing = skill.reaches_allies;
        // Sole caller of the strike pricing until hits landed on a schedule;
        // keep the parameters flowing to the landing site.
        let _ = (power, weapon_strength);

        // Record the cast
        *self.skill_casts.entry(skill_id).or_insert(0) += 1;
        let (on_use, _) = self.form_gain_for(skill_id);
        self.gain_form_pool(on_use, false);
        self.fire_triggered(ProcEvent::Cast(idx));

        // E3: profession attune skills mutate shared AttunementState + bus.
        if super::attunement::apply_attunement_skill(
            &mut self.attunement,
            &mut self.trigger_bus,
            self.current_time_ms,
            &skill_name,
        )
        .is_some()
        {
            self.attunement_swapped = true;
        }

        for effect in &effects {
            match effect {
                SkillEffect::StrikeDamage {
                    hit_count,
                    dmg_multiplier,
                } => {
                    // Hits land across the activation (measured spacing from
                    // data/formulas/hit_timing.json, even spread otherwise),
                    // each priced at the Might and Fury of its own moment.
                    // `dmg_multiplier` is already per strike (wiki Soul
                    // Spiral: 12 x 0.7 = 8.4), so each hit carries all of it.
                    for offset in
                        crate::data::hit_timing::hit_schedule(&skill_name, cast_time, *hit_count)
                    {
                        self.scheduled_hits.push(ScheduledStrike {
                            at_ms: self.current_time_ms.saturating_add(offset),
                            skill_id,
                            dmg_multiplier: *dmg_multiplier,
                        });
                    }
                }
                SkillEffect::ApplyCondition {
                    condition,
                    stacks,
                    duration_ms,
                } => {
                    self.inflict_condition(condition, *stacks, *duration_ms);
                    self.fire_triggered(ProcEvent::Condition(condition));
                }
                SkillEffect::ApplyBuff {
                    buff,
                    stacks,
                    duration_ms,
                } if crate::data::boon_condition_formulas::is_condition(buff) => {
                    // Non-damaging foe conditions (Vulnerability, Chilled, …):
                    // one shared TargetState ledger with WvW (Phase 3).
                    self.inflict_condition(buff, *stacks, *duration_ms);
                    self.fire_triggered(ProcEvent::Condition(buff));
                }
                SkillEffect::ApplyBuff {
                    buff,
                    stacks,
                    duration_ms,
                } => {
                    let remaining_ms =
                        (*duration_ms as f64 * self.params.boon_duration_mult).round() as u32;
                    self.add_buff(buff, *stacks, remaining_ms, ally_facing);
                }
                SkillEffect::ComboField {
                    field_type,
                    duration_ms,
                } => {
                    // Fields before finishers: placed here in effect order; skills
                    // that list both should list the field first (ledger ordering).
                    self.combo.place_field(
                        field_type,
                        *duration_ms,
                        self.current_time_ms,
                        ComboSite::Ground,
                        SELF_COMBATANT_ID,
                    );
                }
                SkillEffect::ComboFinisher {
                    finisher_type,
                    percent,
                } => {
                    // Interrupted leap = no finisher: flow open-dummy has no CC
                    // interrupt of the pending cast yet; pass interrupted=false.
                    // Engine still honors the flag for Kent / future CC wiring.
                    if let Some(outcome) = self.combo.try_finisher(
                        finisher_type,
                        *percent,
                        self.current_time_ms,
                        SELF_COMBATANT_ID,
                        ComboSite::Ground,
                        false,
                    ) {
                        self.apply_combo_outcome(outcome);
                    }
                }
                SkillEffect::Healing { hit_count } => {
                    // Same conservative model as the WvW timeline. The open
                    // dummy has no incoming pressure, so this is output the
                    // scheduler chose to produce, not survival.
                    let healed = (1_200.0 + self.params.healing_power * 0.45)
                        * *hit_count as f64
                        * self.params.healing_mult;
                    self.total_healing += healed;
                    if ally_facing {
                        self.ally_healing += healed;
                    }
                }
                SkillEffect::Barrier { amount } => {
                    let granted = amount + self.params.healing_power * 0.30;
                    self.total_healing += granted;
                    if ally_facing {
                        self.ally_healing += granted;
                    }
                }
                SkillEffect::RemovesCondition { .. } => {
                    // Cleanse effects are tracked at the roster level (cleanse_count / cleanse_rate_per_20s),
                    // not as in-sim condition deletion (no enemy condi bar).
                }
                SkillEffect::CrowdControl {
                    kind, duration_ms, ..
                } => {
                    // Non-overlapping disabled time; nothing lands through
                    // Stability. Same landed-disable emit as the WvW timeline.
                    let added = super::trigger_bus::land_foe_disable(
                        &mut self.target,
                        &mut self.trigger_bus,
                        self.current_time_ms,
                        *duration_ms,
                    );
                    self.control_ms += added as f64;
                    // Fear and Taunt are conditions too (wiki `Fear`), as
                    // the WvW timeline's `apply_outgoing_condition` has it.
                    if matches!(kind, super::ControlKind::Fear | super::ControlKind::Taunt) {
                        self.fire_triggered(ProcEvent::Condition(&format!("{kind:?}")));
                    }
                }
                SkillEffect::ConvertConditions
                | SkillEffect::Cover { .. }
                | SkillEffect::Mobility { .. } => {}
                SkillEffect::StripBoons { .. }
                | SkillEffect::CorruptBoons
                | SkillEffect::StealBoons => {
                    self.target.clear_boons();
                }
            }
        }

        // Auto-attacks have 0 cooldown.
        self.skill_states[idx].cooldown_remaining_ms = cooldown;

        // Quickness reduces cast time: skills execute at 1.5× speed (66.7% of normal time).
        // GW2 mechanic: "skills and actions execute 50% faster" = cast * 2/3.
        // Quickness does NOT reduce cooldowns.
        // Round-half-up via (n*2 + 1)/3 to avoid systematic floor bias from integer division.
        let quickness_active = self.buffs.iter().any(|b| b.kind == BuffKind::Quickness);
        let effective_cast = if quickness_active {
            (cast_time * 2 + 1) / 3
        } else {
            cast_time
        };

        self.auto_chain.on_cast(
            idx,
            self.skills[idx].is_auto_attack(),
            self.current_time_ms.saturating_add(effective_cast),
        );
        // Next action = now + effective_cast + human delay
        self.next_action_ms = self
            .current_time_ms
            .saturating_add(effective_cast)
            .saturating_add(HUMAN_DELAY_MS)
            .saturating_add(MIN_SKILL_GAP_MS);
    }

    /// Soft control present this tick: half weight per distinct condition.
    /// Soft control lives on the shared foe TargetState ledger (Phase 3).
    fn soft_control_weight(&self) -> f64 {
        soft_control_weight_on(&self.target, self.current_time_ms)
    }

    fn apply_combo_outcome(&mut self, outcome: ComboOutcome) {
        let scale = outcome.proc_scale;
        if scale <= 0.0 {
            return;
        }
        match outcome.effect {
            ComboOutcomeEffect::Buff {
                name,
                stacks,
                duration_ms,
            } => {
                let duration =
                    ((duration_ms as f64) * scale * self.params.boon_duration_mult).round() as u32;
                if duration == 0 || stacks == 0 {
                    return;
                }
                // Area effect: the field's boon lands on allies in it.
                self.add_buff(&name, stacks, duration, true);
            }
            ComboOutcomeEffect::Condition {
                name,
                stacks,
                duration_ms,
            } => {
                let duration = ((duration_ms as f64) * scale * self.params.condition_duration_mult)
                    .round() as u32;
                if duration == 0 || stacks == 0 {
                    return;
                }
                let cap = condition_stack_cap(&name, &self.params.mode);
                let _ = self.condition_slot(&name);
                self.target.apply_condition(
                    &name,
                    stacks,
                    duration,
                    self.current_time_ms,
                    cap as u32,
                );
            }
            ComboOutcomeEffect::Healing {
                base,
                healing_power_coef,
            } => {
                let amount = (base + self.params.healing_power * healing_power_coef)
                    * scale
                    * self.params.healing_mult;
                // Wiki `Combo`: a finisher in a field produces an AREA
                // effect, so a water blast heals the allies standing in it,
                // not only the finisher.
                self.total_healing += amount;
                self.ally_healing += amount;
            }
            ComboOutcomeEffect::LifeSteal {
                damage_base,
                damage_power_coef,
                heal_base,
                heal_healing_power_coef,
            } => {
                let damage = (damage_base + damage_power_coef * self.params.power) * scale;
                let healing =
                    (heal_base + heal_healing_power_coef * self.params.healing_power) * scale;
                self.total_strike_damage += damage;
                self.total_healing += healing;
            }
            ComboOutcomeEffect::ConditionCleanse { count } => {
                // Roster-level cleanse accounting matches RemovesCondition.
                let _ = ((count as f64) * scale).round() as u32;
            }
            ComboOutcomeEffect::Cover { kind, duration_ms } => {
                // Flow open-dummy: model Stealth/Blind as timed self buffs when
                // CoverKind has no dedicated ledger here.
                let name = match kind {
                    CoverKind::Stealth => "Stealth",
                    CoverKind::Blind => "Blinded",
                    _ => return,
                };
                let duration =
                    ((duration_ms as f64) * scale * self.params.boon_duration_mult).round() as u32;
                if duration == 0 {
                    return;
                }
                let bkind = buff_kind(name);
                let slot = self.buff_slot(name);
                self.buffs.push(BuffInstance {
                    remaining_ms: duration,
                    kind: bkind,
                    slot,
                    ally_facing: false,
                });
            }
            ComboOutcomeEffect::CrowdControl { duration_ms } => {
                let duration = ((duration_ms as f64) * scale).round() as u32;
                if duration == 0 {
                    return;
                }
                let added = super::trigger_bus::land_foe_disable(
                    &mut self.target,
                    &mut self.trigger_bus,
                    self.current_time_ms,
                    duration,
                );
                self.control_ms += added as f64;
            }
            ComboOutcomeEffect::Unmodeled { .. } => {}
        }
    }

    fn live_might_stacks(&self) -> f64 {
        self.buffs
            .iter()
            .filter(|buff| buff.kind == BuffKind::Might)
            .count()
            .min(25) as f64
    }

    fn live_ally_might_stacks(&self) -> f64 {
        self.buffs
            .iter()
            .filter(|buff| buff.kind == BuffKind::Might && buff.ally_facing)
            .count()
            .min(25) as f64
    }

    /// Tick all active conditions — apply damage for each stack, remove expired.
    ///
    /// Wiki Condition: full seconds tick normally; the leftover fraction of a
    /// second pays that fraction of one tick. Per-application `next_tick_ms`.
    /// Foe ledger is shared [`TargetState`] (Phase 3).
    fn tick_conditions(&mut self, condition_damage: f64) {
        let condition_damage = condition_damage
            + self.live_might_stacks()
                * crate::data::boon_condition_formulas::boons().might_condi_per_stack();

        let now = self.current_time_ms;
        let incoming_mult = self.target.incoming_multiplier_at_tick(
            now,
            &self.params.mode,
            &self.params.deferred_target,
        ) * self.live_mod_factor(ModAxis::Condition);

        let mut tick_total = 0.0;
        for condition in &mut self.target.conditions {
            let tick_dmg =
                condition_tick_damage(&condition.name, condition_damage, &self.params.mode)
                    * condition.stacks as f64
                    * self.params.condition_mult
                    * self.params.condition_type_mult(&condition.name)
                    * incoming_mult;

            while condition.next_tick_ms <= now {
                // Remaining duration budget from the prior boundary (apply or last
                // full tick) - same as the pre-Phase-3 remaining_ms countdown.
                let last_boundary = condition
                    .next_tick_ms
                    .saturating_sub(CONDITION_TICK_INTERVAL_MS);
                let remaining_budget = condition.expires_at_ms.saturating_sub(last_boundary);
                if remaining_budget < CONDITION_TICK_INTERVAL_MS {
                    break;
                }
                self.total_condition_damage += tick_dmg;
                tick_total += tick_dmg;
                if let Some(slot) = self
                    .condition_slots
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&condition.name))
                {
                    // Intensity ledger stores stacks on the entry; pre-Phase-3
                    // had one ConditionStack per stack so += 1.0 was correct.
                    self.condition_ticks[slot] += condition.stacks as f64;
                }
                condition.next_tick_ms = condition
                    .next_tick_ms
                    .saturating_add(CONDITION_TICK_INTERVAL_MS);
            }

            if condition.expires_at_ms <= now {
                let last_boundary = condition
                    .next_tick_ms
                    .saturating_sub(CONDITION_TICK_INTERVAL_MS);
                let remaining = condition.expires_at_ms.saturating_sub(last_boundary);
                if remaining > 0 && remaining < CONDITION_TICK_INTERVAL_MS {
                    let frac = remaining as f64 / CONDITION_TICK_INTERVAL_MS as f64;
                    let frac_dmg = tick_dmg * frac;
                    self.total_condition_damage += frac_dmg;
                    tick_total += frac_dmg;
                    if let Some(slot) = self
                        .condition_slots
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(&condition.name))
                    {
                        self.condition_ticks[slot] += condition.stacks as f64 * frac;
                    }
                }
            }
        }

        self.apply_dummy_damage(tick_total);
        self.target.retain_active(now);
    }

    fn apply_dummy_damage(&mut self, damage: f64) {
        if self.downed || self.current_time_ms < self.invuln_until_ms {
            return;
        }
        let Some(hp) = self.target.hp.as_mut() else {
            return;
        };
        *hp -= damage;
        if *hp <= 0.0 {
            *hp = 0.0;
            self.downed = true;
            self.invuln_until_ms = self.current_time_ms.saturating_add(DOWNED_INVULN_MS);
            self.stomp_ends_ms = self.invuln_until_ms.saturating_add(STOMP_MS);
        }
    }

    /// Tick buffs — track uptime, remove expired.
    ///
    /// Uptime is presence: a slot counts once per tick however many instances
    /// are live, so five stacks of Stability for 5 s are 5 s of Stability,
    /// not 25. Might's stack count is tracked separately (`might_stack_ms`).
    fn tick_buffs(&mut self) {
        self.buff_seen.iter_mut().for_each(|seen| *seen = false);
        for buff in &mut self.buffs {
            if buff.remaining_ms > 0 {
                if !self.buff_seen[buff.slot] {
                    self.buff_seen[buff.slot] = true;
                    self.buff_active_ms[buff.slot] += TICK_MS;
                    if buff.ally_facing {
                        self.ally_buff_active_ms[buff.slot] += TICK_MS;
                    }
                }
                buff.remaining_ms = buff.remaining_ms.saturating_sub(TICK_MS);
            }
        }

        self.buffs.retain(|b| b.remaining_ms > 0);
    }

    /// Convert internal state into SimulationResult.
    fn into_result(self) -> SimulationResult {
        let duration_secs = self.duration_ms as f64 / 1000.0;
        let strike_dps = self.total_strike_damage / duration_secs;
        let condition_dps = self.total_condition_damage / duration_secs;

        // Average condition stacks: total ticks / duration in seconds
        let condition_uptime: HashMap<String, f64> = self
            .condition_slots
            .iter()
            .zip(&self.condition_ticks)
            .filter(|(_, ticks)| **ticks > 0.0)
            .map(|(slot, ticks)| (slot.name.clone(), ticks / duration_secs))
            .collect();

        // Buff uptime as fraction of total duration (presence, see tick_buffs).
        // Only names that were ever active, so a zero-duration tooltip buff
        // does not surface as a 0% row.
        let buff_uptime: HashMap<String, f64> = self
            .buff_slots
            .iter()
            .zip(&self.buff_active_ms)
            .filter(|(_, ms)| **ms > 0)
            .map(|(name, ms)| {
                let fraction = *ms as f64 / self.duration_ms as f64;
                (name.clone(), fraction.min(1.0))
            })
            .collect();
        let buff_presence_per_second: HashMap<String, Vec<bool>> = self
            .buff_slots
            .iter()
            .zip(&self.buff_active_ms)
            .enumerate()
            .filter(|(_, (_, ms))| **ms > 0)
            .map(|(slot, (name, _))| {
                let seen = self
                    .buff_seen_mid_second
                    .iter()
                    .map(|tick| tick.get(slot).copied().unwrap_or(false))
                    .collect();
                (name.clone(), seen)
            })
            .collect();

        // Per-skill usage
        let skill_usage: Vec<SkillUsage> = self
            .skills
            .iter()
            .filter_map(|s| {
                let casts = self.skill_casts.get(&s.skill_id).copied().unwrap_or(0);
                if casts == 0 {
                    return None;
                }
                let dmg = self.skill_damage.get(&s.skill_id).copied().unwrap_or(0.0);
                Some(SkillUsage {
                    name: s.name.clone(),
                    cast_count: casts,
                    dps_contribution: dmg / duration_secs,
                })
            })
            .collect();

        // Control/survivability metrics
        let stunbreak_count = self.skills.iter().filter(|s| s.is_stunbreak).count() as u32;
        let has_stability = kit_has_stability_cover(&self.skills);
        let stability_uptime = buff_uptime.get("Stability").copied().unwrap_or(0.0);

        // Cleanse metrics: count skills with ≥1 RemovesCondition effect; estimate rate per 20s.
        // Rate per 20s: for each cleanse skill, sum conditions_removed × (20s / cooldown_s).
        // Skills with cooldown=0 (auto-attacks) are excluded (would be infinite — ignore them).
        let cleanse_count = self
            .skills
            .iter()
            .filter(|s| {
                s.effects
                    .iter()
                    .any(|e| matches!(e, SkillEffect::RemovesCondition { .. }))
            })
            .count() as u32;

        let cleanse_rate_per_20s: f64 = self
            .skills
            .iter()
            .filter_map(|s| {
                let conditions_removed: u32 = s
                    .effects
                    .iter()
                    .filter_map(|e| {
                        if let SkillEffect::RemovesCondition { conditions_removed } = e {
                            Some(*conditions_removed)
                        } else {
                            None
                        }
                    })
                    .sum();
                if conditions_removed == 0 || s.cooldown_ms == 0 {
                    return None;
                }
                let cooldown_s = s.cooldown_ms as f64 / 1000.0;
                // uptime_factor = 20s / cooldown_s (capped at 1 use per cooldown)
                let casts_in_20s = 20.0 / cooldown_s;
                Some(conditions_removed as f64 * casts_in_20s)
            })
            // Not `.sum()`: the empty f64 sum is -0.0 and the gate note
            // printed "rate=-0.0/20s" for a kit with no cleanse at all.
            .fold(0.0, |acc, r| acc + r);

        // Ally-facing, not self-facing: the healing axis is what this build
        // does FOR OTHER PEOPLE. A Healing Signet ticking on its owner is
        // survival, and `sustain` is where survival is scored; counting it
        // here made every self-sustain bruiser read as a healer.
        let healing_per_second = self.ally_healing / duration_secs;
        let control_uptime = self.control_ms / self.duration_ms as f64;
        let might_stacks_avg = self.might_stack_ms / self.duration_ms as f64;
        let ally_might_stacks_avg = self.ally_might_stack_ms / self.duration_ms as f64;
        // From the slot vectors (insertion order), not the map: float sums
        // are order-dependent and the rank compares exact micro-units.
        // Same rule as healing: boon SUPPORT is boons on other people. A
        // signet that mights its owner is a damage modifier, not support.
        let boon_equivalents = self
            .buff_slots
            .iter()
            .zip(&self.ally_buff_active_ms)
            .filter(|(name, _)| !name.eq_ignore_ascii_case("Might"))
            .map(|(name, ms)| boon_value(name) * (*ms as f64 / self.duration_ms as f64).min(1.0))
            .sum::<f64>()
            + ally_might_stacks_avg / 25.0;

        SimulationResult {
            duration_ms: self.duration_ms,
            strike_dps,
            condition_dps,
            total_dps: strike_dps + condition_dps,
            condition_uptime,
            buff_uptime,
            skill_usage,
            stunbreak_count,
            has_stability,
            stability_uptime,
            cleanse_count,
            cleanse_rate_per_20s,
            healing_per_second,
            control_uptime,
            might_stacks_avg,
            boon_equivalents,
            has_mobility_out: kit_has_mobility_out(&self.skills),
            escape_kinds: kit_escape_kinds(&self.skills),
            has_strip: kit_has_strip(&self.skills),
            has_corrupt: kit_has_corrupt(&self.skills),
            downed: self.downed,
            finished: self.downed && self.duration_ms >= self.stomp_ends_ms,
            has_interrupt: kit_has_interrupt(&self.skills),
            has_cover_answer: kit_has_cover_answer(&self.skills),
            wvw: None,
            honesty: Default::default(),
            damage_per_second: self.damage_seconds,
            buff_presence_per_second,
        }
    }
}

/// Boons, as named by the rotation builder. Anything else in `buffs` is a
/// non-damaging status the builder filed there (Chilled, Vulnerability, ...).
const BOONS: [&str; 12] = [
    "Might",
    "Fury",
    "Quickness",
    "Alacrity",
    "Protection",
    "Regeneration",
    "Swiftness",
    "Vigor",
    "Stability",
    "Resolution",
    "Resistance",
    "Aegis",
];

/// Non-damaging conditions that hamper the target: counted as control at
/// half the weight of a hard disable while present.
const SOFT_CONTROL: [&str; 5] = ["Chilled", "Crippled", "Weakness", "Slow", "Blinded"];

/// One ledger pass: bit per distinct unexpired soft-control name, ×0.5.
/// Same output as five `stacks_of` scans (exclusive expiry).
fn soft_control_weight_on(target: &crate::rotation::combat_model::TargetState, now_ms: u32) -> f64 {
    let mut present = 0u8;
    for c in &target.conditions {
        if c.stacks == 0 || c.expires_at_ms <= now_ms {
            continue;
        }
        // One alias walk per live row — same as stacks_of folding the stored name.
        let stored = crate::data::boon_condition_formulas::canonical_condition_name(&c.name);
        for (i, name) in SOFT_CONTROL.iter().enumerate() {
            let bit = 1u8 << i;
            if present & bit != 0 {
                continue;
            }
            if stored.eq_ignore_ascii_case(name) {
                present |= bit;
                break;
            }
        }
        if present == 0b1_1111 {
            break;
        }
    }
    present.count_ones() as f64 * 0.5
}

fn is_boon(status: &str) -> bool {
    BOONS.iter().any(|b| b.eq_ignore_ascii_case(status))
}

/// How much one second of a boon is worth on the boon-support axis, relative
/// to a second of Quickness. Heuristic, deliberately coarse: the damage
/// boons and the two that decide fights (Protection, Stability) count in
/// full, the comfort boons half, Swiftness a quarter. Might is scaled by
/// stacks/25 on top of this by the callers.
fn boon_value(status: &str) -> f64 {
    let canonical = BOONS
        .iter()
        .find(|b| b.eq_ignore_ascii_case(status))
        .copied()
        .unwrap_or("");
    match canonical {
        "Might" | "Fury" | "Quickness" | "Alacrity" | "Protection" | "Stability" => 1.0,
        "Aegis" => 0.75,
        "Regeneration" | "Resolution" | "Vigor" | "Resistance" => 0.5,
        "Swiftness" => 0.25,
        _ => 0.0,
    }
}

fn is_soft_control(status: &str) -> bool {
    let canonical = crate::data::boon_condition_formulas::canonical_condition_name(status);
    SOFT_CONTROL
        .iter()
        .any(|s| s.eq_ignore_ascii_case(canonical))
}

/// What one cast produces, per radar axis, before dividing by cast time.
#[derive(Debug, Default, Clone, Copy)]
struct CastValue {
    strike: f64,
    condition: f64,
    /// DPS-equivalent of Might/Fury/Quickness over their duration.
    boon_dps: f64,
    healing: f64,
    /// Seconds of control: hard CC at full weight, soft control at half.
    control_s: f64,
    /// Boon-equivalent seconds (Might as stacks/25).
    boon_s: f64,
    protection_s: f64,
}

fn skill_cast_value(
    skill: &RotationSkill,
    power: f64,
    condition_damage: f64,
    weapon_strength: f64,
    params: &SimParams,
    live_might_stacks: f64,
    fury_active: bool,
) -> CastValue {
    let mut v = CastValue::default();
    for effect in &skill.effects {
        match effect {
            SkillEffect::StrikeDamage {
                hit_count,
                dmg_multiplier,
            } => {
                // Same power / crit / strike_mult fold as `use_skill`.
                let effective_power = power + live_might_stacks * 30.0;
                let fury_bonus = if fury_active {
                    params.fury_crit_chance_bonus
                } else {
                    0.0
                };
                v.strike += weapon_strength * effective_power / reference_armor()
                    * dmg_multiplier
                    * (*hit_count as f64)
                    * strike_crit_factor_with_bonus(
                        params.precision,
                        params.ferocity,
                        params.crit_chance_bonus + fury_bonus,
                    )
                    * params.strike_mult;
            }
            SkillEffect::ApplyCondition {
                condition,
                stacks,
                duration_ms,
            } => {
                // Total condition damage over the full duration of all stacks
                let tick_dmg = condition_tick_damage(condition, condition_damage, &params.mode);
                let duration_s = *duration_ms as f64 / 1000.0;
                v.condition += tick_dmg * (*stacks as f64) * duration_s;
            }
            SkillEffect::ApplyBuff {
                buff,
                stacks,
                duration_ms,
            } => {
                let duration_s = *duration_ms as f64 / 1000.0;
                v.boon_dps += estimate_buff_dps_value(
                    buff,
                    *stacks,
                    *duration_ms,
                    power,
                    weapon_strength,
                    &params.mode,
                );
                if is_boon(buff) {
                    let equivalents = if buff.eq_ignore_ascii_case("Might") {
                        (*stacks as f64 / 25.0).min(1.0)
                    } else {
                        boon_value(buff)
                    };
                    v.boon_s += equivalents * duration_s;
                    if buff.eq_ignore_ascii_case("Protection") {
                        v.protection_s += duration_s;
                    }
                } else if is_soft_control(buff) {
                    v.control_s += 0.5 * duration_s;
                }
            }
            SkillEffect::Healing { hit_count } => {
                v.healing += (1_200.0 + params.healing_power * 0.45)
                    * *hit_count as f64
                    * params.healing_mult;
            }
            SkillEffect::Barrier { amount } => {
                v.healing += amount + params.healing_power * 0.30;
            }
            SkillEffect::CrowdControl { duration_ms, .. } => {
                v.control_s += *duration_ms as f64 / 1000.0;
            }
            _ => {}
        }
    }
    v
}

/// Radar-weighted worth of one cast. Every axis is expressed in seconds at
/// its realized norm, so a stun, a heal and a cleave are commensurable, then
/// weighted by what the user asked for. A zero weight leaves an axis
/// worthless, as it should.
fn intent_value(v: &CastValue, w: &OptimizationWeights) -> f64 {
    w.power * v.strike / REALIZED_STRIKE_DPS_NORM
        + w.power.max(w.condition) * v.boon_dps / REALIZED_STRIKE_DPS_NORM
        + w.condition * v.condition / REALIZED_CONDI_DPS_NORM
        + w.boon_support * v.boon_s / REALIZED_BOON_NORM
        + w.healing * v.healing / REALIZED_HEALING_NORM
        + w.sustain * v.protection_s * PROTECTION_REDUCTION
        + w.control * v.control_s / REALIZED_CONTROL_NORM
}

/// Scheduling priority per second of cast time: the reference formula the
/// tests pin. `SimState::priority` is the same arithmetic over a precomputed
/// `StaticCast`, which is what the simulation itself runs.
///
/// Without `SimParams::intent` this is DPCT: damage per cast time with buffs
/// as DPS-equivalents (the gate simulation). With intent it is
/// `intent_value` per cast time: the flow simulation schedules toward the
/// radar, so a healer casts heals and a controller lands its stuns even on
/// an open dummy that never hits back.
#[cfg(test)]
fn skill_dps_efficiency(
    skill: &RotationSkill,
    power: f64,
    condition_damage: f64,
    weapon_strength: f64,
    params: &SimParams,
    live_might_stacks: f64,
    fury_active: bool,
) -> f64 {
    let cast_time_s = (skill.cast_time_ms + HUMAN_DELAY_MS + MIN_SKILL_GAP_MS) as f64 / 1000.0;
    if cast_time_s <= 0.0 {
        return 0.0;
    }
    let v = skill_cast_value(
        skill,
        power,
        condition_damage,
        weapon_strength,
        params,
        live_might_stacks,
        fury_active,
    );
    match &params.intent {
        Some(weights) => intent_value(&v, weights) / cast_time_s,
        None => (v.strike + v.condition + v.boon_dps) / cast_time_s,
    }
}

/// Estimate the total DPS value of applying a buff.
///
/// Buffs don't deal direct damage, but they increase DPS over their duration.
/// This heuristic lets the scheduler properly value buff skills alongside
/// damage skills in the DPCT priority.
fn estimate_buff_dps_value(
    buff: &str,
    stacks: u32,
    duration_ms: u32,
    power: f64,
    weapon_strength: f64,
    mode: &GameMode,
) -> f64 {
    let duration_s = duration_ms as f64 / 1000.0;
    match buff {
        "Might" => {
            // Might per-stack values loaded from data/formulas/boons.json.
            let b = crate::data::boons();
            let stacks_f = stacks as f64;
            let might_power = b.might_power_per_stack();
            let might_condi = b.might_condi_per_stack();
            // Power contribution: extra_power * weapon_strength / armor
            let power_value =
                might_power * stacks_f * weapon_strength / reference_armor() * duration_s;
            // Condition Damage contribution: condi_per_stack * CD_coefficient * duration.
            // Use bleeding coefficient (0.06) as a conservative typical-condition estimate.
            let condi_value = might_condi * stacks_f * 0.06 * duration_s;
            power_value + condi_value
        }
        "Fury" => {
            // Fury: mode-dependent crit chance bonus (loaded from data).
            // +25pp in PvE, +20pp in PvP/WvW.
            let base_hit = power * weapon_strength / reference_armor();
            let fury = crate::data::boons().fury_crit_bonus(mode.clone());
            base_hit * fury * duration_s * (stacks.min(1) as f64)
        }
        "Quickness" => {
            // Quickness = +50% attack speed → massive DPS multiplier.
            let base_hit = power * weapon_strength / reference_armor();
            base_hit * 0.5 * duration_s * (stacks.min(1) as f64)
        }
        _ => 0.0, // Stability, Resistance, etc. don't directly increase DPS
    }
}

/// Expected-value crit multiplier with a direct mode-specific critical-chance
/// bonus (Fury is +25 percentage points in PvE, +20 in PvP/WvW).
pub(super) fn strike_crit_factor_with_bonus(
    precision: f64,
    ferocity: f64,
    crit_chance_bonus_pct: f64,
) -> f64 {
    if precision <= 0.0 {
        return 1.0;
    }
    let f = crate::data::universal_formulas::formulas();
    let chance = crit_chance_fraction(precision, crit_chance_bonus_pct);
    let crit_mult = f.crit_damage(ferocity) / 100.0;
    1.0 + chance * (crit_mult - 1.0)
}

/// Critical chance as a fraction in [0, 1], the same number the expected-crit
/// damage factor above uses; zero precision means zero chance (the factor
/// short-circuits to 1.0 there too).
pub(super) fn crit_chance_fraction(precision: f64, crit_chance_bonus_pct: f64) -> f64 {
    if precision <= 0.0 {
        return 0.0;
    }
    let f = crate::data::universal_formulas::formulas();
    ((f.crit_chance(precision) + crit_chance_bonus_pct) / 100.0).clamp(0.0, 1.0)
}

/// Intensity-stack cap from `data/formulas/conditions.json` `max_stacks`.
/// Wiki Condition (2026-08-29): intensity shares a 1500 cap in every mode;
/// Vulnerability is 25. No sourced competitive 100-stack ceiling — do not
/// clamp PvP/WvW below the JSON row.
pub(crate) fn condition_stack_cap(condition: &str, mode: &GameMode) -> usize {
    let conds = crate::data::conditions();
    let canonical = crate::data::boon_condition_formulas::canonical_condition_name(condition);
    let cap = conds.max_stacks(canonical).unwrap_or(1500) as usize;
    match mode {
        GameMode::PvE | GameMode::PvP | GameMode::WvW => cap,
    }
}

/// Condition tick damage formula (GW2 level 80, per 1s pulse).
/// Torment uses stationary baseline; Confusion uses over-time, not on-skill-use.
pub(super) fn condition_tick_damage(
    condition: &str,
    condition_damage: f64,
    mode: &GameMode,
) -> f64 {
    let conds = crate::data::conditions();
    let mode = mode.clone();
    match condition {
        "Torment" => conds.torment_tick(condition_damage, mode, false),
        "Confusion" => conds.confusion_tick(condition_damage, mode, false),
        _ => conds.tick_damage(condition, condition_damage, mode),
    }
}

/// Cooldown consumed per wall-clock tick. Wiki Alacrity: +25% recharge
/// (skills recharge in 80% of original time). 100ms wall → 125ms CD.
/// Time Marches On (50%) is not modeled.
pub(super) fn alacrity_cd_advance_ms(tick_ms: u32, has_alacrity: bool) -> u32 {
    if has_alacrity {
        tick_ms + tick_ms / 4
    } else {
        tick_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::{RotationSkill, SkillEffect, SkillSlot};

    fn auto_attack() -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 1,
            name: "Auto Attack".into(),
            slot: SkillSlot::Weapon1,
            cast_time_ms: 500,
            cooldown_ms: 0,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    fn weapon_skill() -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 2,
            name: "Whirling Axe".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 750,
            cooldown_ms: 8000,
            effects: vec![
                SkillEffect::StrikeDamage {
                    hit_count: 5,
                    dmg_multiplier: 0.5,
                },
                SkillEffect::ApplyCondition {
                    condition: "Bleeding".into(),
                    stacks: 3,
                    duration_ms: 6000,
                },
            ],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    fn buff_skill() -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 3,
            name: "For Great Justice!".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 250,
            cooldown_ms: 25000,
            effects: vec![SkillEffect::ApplyBuff {
                buff: "Might".into(),
                stacks: 6,
                duration_ms: 10000,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: true,
            weapon_set: 0,
        }
    }

    fn heal_skill() -> RotationSkill {
        RotationSkill {
            targets: 1,
            skill_id: 40,
            name: "Heal".into(),
            slot: SkillSlot::Heal,
            cast_time_ms: 1000,
            cooldown_ms: 20_000,
            effects: vec![SkillEffect::Healing { hit_count: 1 }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: true,
            weapon_set: 0,
            categories: Vec::new(),
            slot_name: None,
        }
    }

    fn stun_skill() -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 41,
            name: "Stun".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 500,
            cooldown_ms: 10_000,
            effects: vec![SkillEffect::CrowdControl {
                kind: super::super::ControlKind::Stun,
                duration_ms: 2_000,
                stops_dodge: true,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    /// On an open dummy nothing hits back, so pure DPCT never casts a heal
    /// or a stun. With intent the scheduler plays the role the radar asks
    /// for, and the result reports what it produced.
    #[test]
    fn intent_schedules_heals_and_control_on_an_open_dummy() {
        let skills = vec![auto_attack(), heal_skill(), stun_skill()];
        let params = SimParams::basic(2_000.0, 0.0, 1_000.0);
        let dpct = simulate_with(&skills, 30_000, &params, EnemyDummy::open());
        assert_eq!(dpct.healing_per_second, 0.0, "DPCT never heals a full bar");
        assert_eq!(dpct.control_uptime, 0.0, "DPCT never stuns a dummy");

        let mut wanted = params.clone();
        wanted.intent = Some(OptimizationWeights {
            power: 0.2,
            condition: 0.0,
            boon_support: 0.0,
            healing: 1.0,
            sustain: 0.0,
            control: 1.0,
        });
        let flow = simulate_with(&skills, 30_000, &wanted, EnemyDummy::open());
        // Two heals in 30s at 1200 base: 2400 / 30 = 80 per second.
        assert!(
            (flow.healing_per_second - 80.0).abs() < 1.0,
            "heals cast on cooldown: {}",
            flow.healing_per_second
        );
        // Three stuns of 2s in 30s: 6 / 30 = 0.2 of the window.
        assert!(
            (flow.control_uptime - 0.2).abs() < 0.02,
            "stuns land on cooldown: {}",
            flow.control_uptime
        );
        assert!(
            flow.strike_dps > 0.0,
            "the filler still swings between casts"
        );
    }

    /// A stunned dummy with Stability takes nothing, so the scheduler must
    /// not spend the cast either: on an open dummy the same stun is cast.
    #[test]
    fn stability_on_the_dummy_blocks_control_and_the_scheduler_knows() {
        let skills = vec![auto_attack(), stun_skill()];
        let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
        params.intent = Some(OptimizationWeights {
            control: 1.0,
            ..OptimizationWeights::default()
        });
        let stable = EnemyDummy {
            stability: true,
            ..EnemyDummy::open()
        };
        let flow = simulate_with(&skills, 30_000, &params, stable);
        assert_eq!(flow.control_uptime, 0.0);
        assert!(
            !flow.skill_usage.iter().any(|u| u.name == "Stun"),
            "no cast wasted into Stability: {:?}",
            flow.skill_usage
        );
        let open = simulate_with(&skills, 30_000, &params, EnemyDummy::open());
        assert!(open
            .skill_usage
            .iter()
            .any(|u| u.name == "Stun" && u.cast_count >= 1));
    }

    fn status_skill(id: u32, status: &str, stacks: u32, duration_ms: u32) -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: id,
            name: status.into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 500,
            cooldown_ms: 30_000,
            effects: vec![SkillEffect::ApplyBuff {
                buff: status.into(),
                stacks,
                duration_ms,
            }],
            next_chain: None,
            // The boon half of this helper stands for a support skill that
            // reaches allies; the condition half applies to the target, where
            // the flag is not read at all.
            is_stunbreak: false,
            reaches_allies: true,
            weapon_set: 0,
        }
    }

    /// Chill is on the enemy: Expertise lengthens it, Concentration does not.
    #[test]
    fn soft_control_follows_condition_duration_not_boon_duration() {
        let skills = vec![auto_attack(), status_skill(50, "Chilled", 1, 4_000)];
        let mut base = SimParams::basic(2_000.0, 0.0, 1_000.0);
        base.intent = Some(OptimizationWeights {
            control: 1.0,
            ..OptimizationWeights::default()
        });
        let plain = simulate_with(&skills, 30_000, &base, EnemyDummy::open());
        let mut concentration = base.clone();
        concentration.boon_duration_mult = 1.5;
        let mut expertise = base.clone();
        expertise.condition_duration_mult = 1.5;
        let with_conc = simulate_with(&skills, 30_000, &concentration, EnemyDummy::open());
        let with_exp = simulate_with(&skills, 30_000, &expertise, EnemyDummy::open());
        assert!(plain.control_uptime > 0.0);
        assert_eq!(
            with_conc.control_uptime, plain.control_uptime,
            "Concentration must not touch Chill"
        );
        assert!(
            with_exp.control_uptime > plain.control_uptime,
            "Expertise must lengthen Chill: {} vs {}",
            with_exp.control_uptime,
            plain.control_uptime
        );
    }

    /// Five stacks of Stability for five seconds are five seconds of
    /// Stability, whatever the stack count.
    #[test]
    fn stacked_stability_counts_once_in_uptime_and_boons() {
        let skills = vec![auto_attack(), status_skill(51, "Stability", 5, 5_000)];
        let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
        // Pure DPCT never casts a Stability skill; a boon radar does.
        params.intent = Some(OptimizationWeights {
            boon_support: 1.0,
            ..OptimizationWeights::default()
        });
        // Cast at 0 s and 30 s in a 60 s window: 10 s of presence.
        let result = simulate_with(&skills, 60_000, &params, EnemyDummy::open());
        let uptime = result.buff_uptime["Stability"];
        assert!(
            (uptime - 10.0 / 60.0).abs() < 0.02,
            "presence uptime: {uptime}"
        );
        assert!(
            (result.boon_equivalents - uptime).abs() < 1e-9,
            "Stability is a full-value boon: {}",
            result.boon_equivalents
        );
    }

    /// A set-1 skill the radar makes worthless must not pin the sim to set 1.
    #[test]
    fn zero_priority_skills_do_not_block_weapon_swap() {
        let mut set1_auto = auto_attack();
        set1_auto.weapon_set = 1;
        let mut set1_bleed = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 60,
            name: "Bleed".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 500,
            cooldown_ms: 8_000,
            effects: vec![SkillEffect::ApplyCondition {
                condition: "Bleeding".into(),
                stacks: 3,
                duration_ms: 6_000,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 1,
        };
        set1_bleed.weapon_set = 1;
        let mut set2_auto = auto_attack();
        set2_auto.skill_id = 61;
        set2_auto.weapon_set = 2;
        let set2_heal = RotationSkill {
            targets: 1,
            skill_id: 62,
            name: "Staff Heal".into(),
            slot: SkillSlot::Weapon3,
            cast_time_ms: 750,
            cooldown_ms: 5_000,
            effects: vec![SkillEffect::Healing { hit_count: 1 }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 2,
            categories: Vec::new(),
            slot_name: None,
        };
        let skills = vec![set1_auto, set1_bleed, set2_auto, set2_heal];
        let mut params = SimParams::basic(2_000.0, 1_000.0, 1_000.0);
        params.intent = Some(OptimizationWeights {
            power: 0.0,
            condition: 0.0,
            boon_support: 0.0,
            healing: 1.0,
            sustain: 0.0,
            control: 0.0,
        });
        let flow = simulate_with(&skills, 30_000, &params, EnemyDummy::open());
        assert!(
            flow.skill_usage
                .iter()
                .any(|u| u.name == "Staff Heal" && u.cast_count >= 1),
            "must swap to the set that heals: {:?}",
            flow.skill_usage
        );
    }

    /// The healing and boon_support axes measure what a build does FOR
    /// OTHER PEOPLE. Two kits with identical stats and identical numbers on
    /// the meter score differently when one of them only helps itself.
    #[test]
    fn the_support_axes_count_allies_not_the_caster() {
        let mut self_heal = heal_skill();
        self_heal.reaches_allies = false;
        let ally_heal = heal_skill();
        let params = {
            let mut p = SimParams::basic(2_000.0, 0.0, 1_000.0);
            p.intent = Some(OptimizationWeights {
                healing: 1.0,
                boon_support: 1.0,
                ..OptimizationWeights::default()
            });
            p
        };
        let selfish = simulate_with(
            &[auto_attack(), self_heal],
            30_000,
            &params,
            EnemyDummy::open(),
        );
        let ally = simulate_with(
            &[auto_attack(), ally_heal],
            30_000,
            &params,
            EnemyDummy::open(),
        );
        assert_eq!(
            selfish.healing_per_second, 0.0,
            "a self-heal is survival, not the healing axis"
        );
        assert!(
            ally.healing_per_second > selfish.healing_per_second,
            "the ally kit heals: {} vs {}",
            ally.healing_per_second,
            selfish.healing_per_second
        );

        // Same rule for boons: a self-might signet is a damage modifier.
        let mut signet = buff_skill();
        signet.name = "Signet of Might".into();
        signet.reaches_allies = false;
        let shout = buff_skill();
        let selfish_boons = simulate_with(
            &[auto_attack(), signet],
            30_000,
            &params,
            EnemyDummy::open(),
        );
        let shouted = simulate_with(&[auto_attack(), shout], 30_000, &params, EnemyDummy::open());
        assert_eq!(selfish_boons.boon_equivalents, 0.0);
        assert!(
            shouted.boon_equivalents > selfish_boons.boon_equivalents,
            "the shout supports: {} vs {}",
            shouted.boon_equivalents,
            selfish_boons.boon_equivalents
        );
        // The self-buff is still on the meter; it is just not support.
        assert!(selfish_boons.might_stacks_avg > 0.0);
    }

    #[test]
    fn might_average_keeps_the_stacks_the_uptime_cap_loses() {
        // 6 Might for 10s every 25s over 50s: 12 stack-seconds x 5 casts... the
        // skill is cast at 0s and 25s, so 2 x 6 x 10 = 120 stack-seconds / 50s.
        let skills = vec![auto_attack(), buff_skill()];
        let params = SimParams::basic(2_000.0, 0.0, 1_000.0);
        let result = simulate_with(&skills, 50_000, &params, EnemyDummy::open());
        assert!(result.buff_uptime["Might"] <= 1.0);
        assert!(
            (result.might_stacks_avg - 2.4).abs() < 0.2,
            "average stacks: {}",
            result.might_stacks_avg
        );
        assert!(
            (result.boon_equivalents - 2.4 / 25.0).abs() < 0.01,
            "Might alone counts as stacks/25: {}",
            result.boon_equivalents
        );
    }

    fn dpct(skill: &RotationSkill, power: f64, condition_damage: f64, weapon_strength: f64) -> f64 {
        skill_dps_efficiency(
            skill,
            power,
            condition_damage,
            weapon_strength,
            &SimParams::basic(power, condition_damage, weapon_strength),
            0.0,
            false,
        )
    }

    /// A three-step auto chain 101 -> 102 -> 103 (E11).
    fn chain_step(id: u32, name: &str, next: Option<u32>) -> RotationSkill {
        RotationSkill {
            skill_id: id,
            name: name.into(),
            next_chain: next,
            ..auto_attack()
        }
    }

    fn three_step_chain() -> Vec<RotationSkill> {
        vec![
            chain_step(101, "Step One", Some(102)),
            chain_step(102, "Step Two", Some(103)),
            chain_step(103, "Step Three", None),
        ]
    }

    fn chain_casts(result: &crate::rotation::SimulationResult, name: &str) -> u32 {
        result
            .skill_usage
            .iter()
            .find(|usage| usage.name == name)
            .map_or(0, |usage| usage.cast_count)
    }

    #[test]
    fn auto_chain_cursor_cycles_and_resets() {
        let skills = three_step_chain();
        let mut chain = crate::rotation::AutoChain::new(&skills);
        assert!(!chain.is_follow_up(0) && chain.is_follow_up(1) && chain.is_follow_up(2));
        let mut order = Vec::new();
        let mut now = 0;
        for _ in 0..4 {
            let step = chain.step(0, now);
            order.push(skills[step].skill_id);
            now += 500;
            chain.on_cast(step, true, now);
        }
        assert_eq!(order, vec![101, 102, 103, 101]);
        // A non-auto cast resets to step 1.
        chain.on_cast(0, true, now);
        assert_eq!(chain.step(0, now), 1);
        chain.on_cast(5, false, now);
        assert_eq!(chain.step(0, now), 0);
        // A pause past the continue window resets to step 1.
        chain.on_cast(0, true, 1_000);
        let deadline = 1_000 + crate::rotation::CHAIN_CONTINUE_SLACK_MS;
        assert_eq!(chain.step(0, deadline), 1);
        assert_eq!(chain.step(0, deadline + 1), 0);
    }

    #[test]
    fn flow_sim_cycles_the_auto_chain() {
        let result = simulate(&three_step_chain(), 10_000, 2000.0, 0.0, 1100.0);
        let counts = [
            chain_casts(&result, "Step One"),
            chain_casts(&result, "Step Two"),
            chain_casts(&result, "Step Three"),
        ];
        assert!(counts[2] > 0, "{counts:?}");
        assert!(counts[0] - counts[2] <= 1, "1-2-3-1 cycle: {counts:?}");
    }

    #[test]
    fn a_non_auto_cast_restarts_the_chain_in_the_flow_sim() {
        let mut skills = three_step_chain();
        skills.push(RotationSkill {
            cooldown_ms: 1_500,
            ..weapon_skill()
        });
        let result = simulate(&skills, 10_000, 2000.0, 0.0, 1100.0);
        let (one, three) = (
            chain_casts(&result, "Step One"),
            chain_casts(&result, "Step Three"),
        );
        assert!(
            one > three,
            "step 1 after every weapon skill: one {one} three {three}"
        );
    }

    #[test]
    fn a_chain_with_a_missing_step_abstains_by_name() {
        let skills = vec![chain_step(101, "Step One", Some(999))];
        assert_eq!(
            crate::rotation::missing_chain_steps(&skills),
            vec!["Step One auto chain (step 999 missing)".to_string()]
        );
        assert!(crate::rotation::missing_chain_steps(&three_step_chain()).is_empty());
        let result = simulate(&skills, 5_000, 2000.0, 0.0, 1100.0);
        assert!(chain_casts(&result, "Step One") > 0);
    }

    #[test]
    fn test_simulate_auto_attack_only() {
        let skills = vec![auto_attack()];
        let result = simulate(&skills, 5000, 2000.0, 0.0, 1100.0);

        assert_eq!(result.duration_ms, 5000);
        assert!(result.strike_dps > 0.0, "Should have non-zero strike DPS");
        assert_eq!(result.condition_dps, 0.0, "No conditions in auto-attack");
        assert_eq!(result.skill_usage.len(), 1);
        assert!(result.skill_usage[0].cast_count > 0);
    }

    #[test]
    fn test_simulate_with_conditions() {
        let skills = vec![auto_attack(), weapon_skill()];
        let result = simulate(&skills, 10000, 2000.0, 1500.0, 1100.0);

        assert!(result.strike_dps > 0.0);
        assert!(
            result.condition_dps > 0.0,
            "Should have condition DPS from Bleeding"
        );
        assert!(
            result.condition_uptime.contains_key("Bleeding"),
            "Bleeding uptime should be tracked"
        );
        assert!(*result.condition_uptime.get("Bleeding").unwrap() > 0.0);
    }

    #[test]
    fn per_second_capture_adds_up_to_the_totals() {
        let skills = vec![auto_attack(), weapon_skill(), buff_skill()];
        let r = simulate(&skills, 10_000, 2000.0, 1500.0, 1100.0);
        assert_eq!(r.damage_per_second.len(), 10);
        let strike: f64 = r.damage_per_second.iter().map(|(s, _)| s).sum();
        let condi: f64 = r.damage_per_second.iter().map(|(_, c)| c).sum();
        assert!((strike / 10.0 - r.strike_dps).abs() < 1e-6);
        assert!((condi / 10.0 - r.condition_dps).abs() < 1e-6);
        // Midpoint samples agree with the tick-counted uptime to a second.
        let might = &r.buff_presence_per_second["Might"];
        assert_eq!(might.len(), 10);
        let sampled = might.iter().filter(|&&p| p).count() as f64 / 10.0;
        assert!((sampled - r.buff_uptime["Might"]).abs() <= 0.1 + 1e-9);
    }

    #[test]
    fn test_simulate_with_buffs() {
        let skills = vec![auto_attack(), buff_skill()];
        let result = simulate(&skills, 10000, 2000.0, 0.0, 1100.0);

        assert!(
            result.buff_uptime.contains_key("Might"),
            "Might should be tracked"
        );
        assert!(*result.buff_uptime.get("Might").unwrap() > 0.0);
    }

    #[test]
    fn test_simulate_default_duration() {
        let skills = vec![auto_attack()];
        let result = simulate(&skills, 0, 2000.0, 0.0, 1100.0);
        assert_eq!(result.duration_ms, DEFAULT_DURATION_MS);
    }

    #[test]
    fn test_condition_tick_damage_formulas() {
        // Formulas loaded from data/formulas/conditions.json (PvE default)
        let cd = 1000.0;
        let pve = GameMode::PvE;
        assert!((condition_tick_damage("Bleeding", cd, &pve) - 82.0).abs() < 0.1);
        // Burning: 0.155*1000 + 131.0 = 286.0 (L1: base=131.0)
        assert!((condition_tick_damage("Burning", cd, &pve) - 286.0).abs() < 0.1);
        assert!((condition_tick_damage("Poison", cd, &pve) - 93.5).abs() < 0.1);
        // Torment PvE stationary: 0.09*1000 + 31.8 = 121.8 (L2 verified)
        assert!((condition_tick_damage("Torment", cd, &pve) - 121.8).abs() < 0.1);
        // Confusion 1s pulse is PvE over-time, not on-skill-use.
        // Wiki Confusion (2026-08-29): (0.05 * CD) + 18.25 at L80 → 68.25.
        // On-skill-use remains (0.0325 * CD) + 16.24 = 48.74 and is not the pulse.
        assert!((condition_tick_damage("Confusion", cd, &pve) - 68.25).abs() < 0.1);
        let on_use = crate::data::conditions().confusion_tick(cd, pve.clone(), true);
        assert!((on_use - 48.74).abs() < 0.1);
        assert_eq!(condition_tick_damage("Vulnerability", cd, &pve), 0.0);
    }

    #[test]
    fn test_live_might_raises_condition_ticks() {
        // Wiki Might: +30 Condition Damage/stack; already-applied conditions scale.
        fn bleed_only() -> RotationSkill {
            RotationSkill {
                targets: 1,
                categories: Vec::new(),
                slot_name: None,
                skill_id: 40,
                name: "Bleed".into(),
                slot: SkillSlot::Weapon2,
                cast_time_ms: 100,
                cooldown_ms: 30_000,
                effects: vec![SkillEffect::ApplyCondition {
                    condition: "Bleeding".into(),
                    stacks: 1,
                    duration_ms: 10_000,
                }],
                next_chain: None,
                is_stunbreak: false,
                reaches_allies: false,
                weapon_set: 0,
            }
        }
        let mut with_might = bleed_only();
        with_might.skill_id = 41;
        with_might.effects.push(SkillEffect::ApplyBuff {
            buff: "Might".into(),
            stacks: 10,
            duration_ms: 20_000,
        });

        let bare = simulate(&[bleed_only()], 2_000, 1_000.0, 1_000.0, 1_100.0);
        let boosted = simulate(&[with_might], 2_000, 1_000.0, 1_000.0, 1_100.0);
        let tick_bare = condition_tick_damage("Bleeding", 1_000.0, &GameMode::PvE);
        let tick_boosted = condition_tick_damage(
            "Bleeding",
            1_000.0 + 10.0 * crate::data::boon_condition_formulas::boons().might_condi_per_stack(),
            &GameMode::PvE,
        );
        assert!((tick_bare - 82.0).abs() < 0.1);
        assert!((tick_boosted - 100.0).abs() < 0.1);
        // One 1s pulse after the t=0 apply in a 2s window.
        assert!((bare.condition_dps - tick_bare / 2.0).abs() < 0.1);
        assert!((boosted.condition_dps - tick_boosted / 2.0).abs() < 0.1);
    }

    #[test]
    fn test_condition_stack_caps() {
        assert_eq!(condition_stack_cap("Bleeding", &GameMode::PvE), 1500);
        assert_eq!(condition_stack_cap("Bleeding", &GameMode::PvP), 1500);
        assert_eq!(condition_stack_cap("Bleeding", &GameMode::WvW), 1500);
        assert_eq!(condition_stack_cap("Vulnerability", &GameMode::PvE), 25);
        assert_eq!(condition_stack_cap("Vulnerability", &GameMode::PvP), 25);
        assert_eq!(condition_stack_cap("Burning", &GameMode::PvP), 1500);
    }

    #[test]
    fn test_dpct_prefers_high_damage_skill() {
        // A high-damage weapon skill should be picked over a low-damage utility
        let high_dmg = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 10,
            name: "Big Hit".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 500,
            cooldown_ms: 5000,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 3,
                dmg_multiplier: 2.0,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };
        let low_dmg = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 11,
            name: "Weak Poke".into(),
            slot: SkillSlot::Elite, // Elite slot but low damage
            cast_time_ms: 1500,
            cooldown_ms: 30000,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 0.1,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };

        let high_dpct = dpct(&high_dmg, 2000.0, 0.0, 1100.0);
        let low_dpct = dpct(&low_dmg, 2000.0, 0.0, 1100.0);
        assert!(
            high_dpct > low_dpct,
            "High-damage skill ({:.1}) should have higher DPCT than slow weak skill ({:.1})",
            high_dpct,
            low_dpct
        );
    }

    #[test]
    fn test_dpct_values_condition_skills() {
        // A condition skill's DPCT should account for total lifetime damage
        let condi_skill = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 20,
            name: "Burning Blade".into(),
            slot: SkillSlot::Weapon3,
            cast_time_ms: 500,
            cooldown_ms: 8000,
            effects: vec![SkillEffect::ApplyCondition {
                condition: "Burning".into(),
                stacks: 2,
                duration_ms: 5000,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };

        let dpct = dpct(&condi_skill, 1000.0, 1500.0, 1100.0);
        assert!(dpct > 0.0, "Condition skill should have positive DPCT");

        // Burning at 1500 CD = 0.155*1500+131 = 363.5 per tick
        // 2 stacks * 5 seconds = 3635.0 total damage
        // cast_time = (500 + 80 + 100) / 1000 = 0.68s
        // DPCT ≈ 3635 / 0.68 ≈ 5345
        assert!(
            dpct > 5000.0,
            "Burning DPCT should be substantial: {:.1}",
            dpct
        );
    }

    #[test]
    fn test_dpct_values_buff_skills() {
        let buff = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 30,
            name: "For Great Justice!".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 250,
            cooldown_ms: 25000,
            effects: vec![SkillEffect::ApplyBuff {
                buff: "Might".into(),
                stacks: 6,
                duration_ms: 10000,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };

        let dpct = dpct(&buff, 2000.0, 0.0, 1100.0);
        assert!(dpct > 0.0, "Might buff should have positive DPCT value");
    }

    #[test]
    fn test_weapon_swap() {
        // Set 1: fast skill, Set 2: different fast skill, plus auto + utility
        let set1_skill = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 100,
            name: "Axe Throw".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 500,
            cooldown_ms: 5000,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.5,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 1,
        };
        let set1_auto = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 101,
            name: "Chop".into(),
            slot: SkillSlot::Weapon1,
            cast_time_ms: 500,
            cooldown_ms: 0,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 1,
        };
        let set2_skill = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 200,
            name: "Greatsword Swing".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 600,
            cooldown_ms: 6000,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 2,
                dmg_multiplier: 1.2,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 2,
        };
        let set2_auto = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 201,
            name: "GS Auto".into(),
            slot: SkillSlot::Weapon1,
            cast_time_ms: 500,
            cooldown_ms: 0,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 0.9,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 2,
        };

        let skills = vec![set1_skill, set1_auto, set2_skill, set2_auto];
        let result = simulate(&skills, 30000, 2000.0, 0.0, 1100.0);

        // Both weapon sets should be used
        let used_names: Vec<&str> = result
            .skill_usage
            .iter()
            .map(|su| su.name.as_str())
            .collect();
        assert!(
            used_names.contains(&"Axe Throw") || used_names.contains(&"Greatsword Swing"),
            "Should use skills from at least one weapon set: {:?}",
            used_names
        );
        assert!(result.total_dps > 0.0);

        // With 30s duration and 10s swap CD, should swap at least once
        // Both autos should appear (meaning both sets were active at some point)
        let has_set1 = used_names.iter().any(|n| *n == "Axe Throw" || *n == "Chop");
        let has_set2 = used_names
            .iter()
            .any(|n| *n == "Greatsword Swing" || *n == "GS Auto");
        assert!(
            has_set1 && has_set2,
            "Should use both weapon sets in 30s: {:?}",
            used_names
        );
    }

    #[test]
    fn test_full_rotation() {
        let skills = vec![auto_attack(), weapon_skill(), buff_skill()];
        let result = simulate(&skills, 30000, 2500.0, 1500.0, 1100.0);

        // Full 30s rotation should produce meaningful DPS
        assert!(result.total_dps > 100.0, "Total DPS should be non-trivial");
        assert!(result.strike_dps > 0.0);
        assert!(result.condition_dps > 0.0);

        // Should have used all skills
        assert!(
            result.skill_usage.len() >= 2,
            "Should use at least AA + weapon skill"
        );
    }

    #[test]
    fn test_no_weapon_set_backward_compat() {
        // All weapon_set=0 (legacy/untagged) — should work like before
        let skills = vec![auto_attack(), weapon_skill(), buff_skill()];
        let result = simulate(&skills, 10000, 2000.0, 1500.0, 1100.0);
        assert!(result.total_dps > 0.0);
        assert!(result.skill_usage.len() >= 2);
    }

    // Cleanse detection tests

    fn cleanse_skill(cooldown_ms: u32, conditions: u32) -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 9999,
            name: "Mending".into(),
            slot: SkillSlot::Heal,
            cast_time_ms: 750,
            cooldown_ms,
            effects: vec![SkillEffect::RemovesCondition {
                conditions_removed: conditions,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    #[test]
    fn test_cleanse_count_zero_without_cleanse_skill() {
        // No cleanse skill → cleanse_count == 0, cleanse_rate_per_20s == 0.0
        let skills = vec![auto_attack()];
        let result = simulate(&skills, 5000, 2000.0, 0.0, 1100.0);
        assert_eq!(result.cleanse_count, 0);
        assert!(
            result.cleanse_rate_per_20s.is_sign_positive(),
            "an empty kit reports 0.0, never -0.0"
        );
        assert_eq!(result.cleanse_rate_per_20s, 0.0);
    }

    #[test]
    fn test_cleanse_count_detects_cleanse_skill() {
        // One cleanse skill with 20s CD removing 3 conditions.
        let skills = vec![auto_attack(), cleanse_skill(20000, 3)];
        let result = simulate(&skills, 5000, 2000.0, 0.0, 1100.0);
        assert_eq!(result.cleanse_count, 1, "one skill has cleanse effect");
        // 3 conditions × (20s / 20s) = 3.0 per 20s
        assert!(
            (result.cleanse_rate_per_20s - 3.0).abs() < 0.01,
            "cleanse_rate_per_20s should be ~3.0, got {}",
            result.cleanse_rate_per_20s
        );
    }

    #[test]
    fn test_cleanse_rate_scales_with_cooldown() {
        // 10s CD, 2 conditions → 2 × (20/10) = 4.0 per 20s
        let skills = vec![auto_attack(), cleanse_skill(10000, 2)];
        let result = simulate(&skills, 5000, 2000.0, 0.0, 1100.0);
        assert_eq!(result.cleanse_count, 1);
        assert!(
            (result.cleanse_rate_per_20s - 4.0).abs() < 0.01,
            "cleanse_rate_per_20s should be ~4.0, got {}",
            result.cleanse_rate_per_20s
        );
    }

    #[test]
    fn test_cleanse_count_multiple_cleanse_skills() {
        // Two cleanse skills → cleanse_count = 2
        let mut second_cleanse = cleanse_skill(30000, 1);
        second_cleanse.skill_id = 9998;
        let skills = vec![auto_attack(), cleanse_skill(20000, 2), second_cleanse];
        let result = simulate(&skills, 5000, 2000.0, 0.0, 1100.0);
        assert_eq!(result.cleanse_count, 2, "both cleanse skills counted");
        // 2×(20/20) + 1×(20/30) ≈ 2.0 + 0.667 = 2.667
        let expected = 2.0 + 20.0_f64 / 30.0;
        assert!(
            (result.cleanse_rate_per_20s - expected).abs() < 0.01,
            "cleanse_rate_per_20s should be ~{:.3}, got {}",
            expected,
            result.cleanse_rate_per_20s
        );
    }

    #[test]
    fn test_cleanse_auto_attack_excluded_from_rate() {
        // A cleanse on an auto-attack (cooldown=0) should not blow up the rate.
        // The rate calculation skips skills with cooldown=0.
        let auto_with_cleanse = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 1,
            name: "Cleansing Auto".into(),
            slot: SkillSlot::Weapon1,
            cast_time_ms: 500,
            cooldown_ms: 0,
            effects: vec![
                SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 1.0,
                },
                SkillEffect::RemovesCondition {
                    conditions_removed: 1,
                },
            ],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };
        let result = simulate(&[auto_with_cleanse], 5000, 2000.0, 0.0, 1100.0);
        // cleanse_count = 1 (skill has cleanse effect), but rate = 0 (cooldown=0 excluded)
        assert_eq!(
            result.cleanse_count, 1,
            "cleanse_count counts the auto-attack"
        );
        assert_eq!(
            result.cleanse_rate_per_20s, 0.0,
            "rate excludes auto-attacks to avoid division by zero"
        );
    }

    #[test]
    fn protection_dummy_cuts_strike_by_a_third() {
        let skills = vec![auto_attack()];
        let open = simulate_against(&skills, 2000, 2000.0, 0.0, 1100.0, EnemyDummy::open());
        let prot = simulate_against(
            &skills,
            2000,
            2000.0,
            0.0,
            1100.0,
            EnemyDummy {
                protection: true,
                stability: true,
                hp: None,
            },
        );
        let ratio = prot.strike_dps / open.strike_dps;
        assert!((ratio - 0.67).abs() < 0.02, "expected ~0.67, got {ratio}");
    }

    #[test]
    fn strip_clears_protection_so_later_hits_are_full() {
        let strip = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 90,
            name: "Strip".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 250,
            cooldown_ms: 10_000,
            effects: vec![SkillEffect::StripBoons {
                count_per_pulse: 1,
                interval_ms: 1000,
                window_ms: 1000,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };
        let dummy = EnemyDummy {
            protection: true,
            stability: true,
            hp: None,
        };
        // Four seconds: hits land at cast end now, so a two-second window
        // cannot fit the strip's 250 ms plus the autos it buys.
        let with_strip =
            simulate_against(&[auto_attack(), strip], 4000, 2000.0, 0.0, 1100.0, dummy);
        let no_strip = simulate_against(&[auto_attack()], 4000, 2000.0, 0.0, 1100.0, dummy);
        assert!(
            with_strip.strike_dps > no_strip.strike_dps,
            "strip should raise delivered DPS vs a Protection dummy: with {} vs without {}",
            with_strip.strike_dps,
            no_strip.strike_dps
        );
    }

    #[test]
    fn dummy_hp_downs_then_stomps_after_invuln() {
        let burst = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 99,
            name: "Burst".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 250,
            cooldown_ms: 10_000,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 20,
                dmg_multiplier: 10.0,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };
        let dummy = EnemyDummy {
            hp: Some(500.0),
            ..EnemyDummy::open()
        };
        let short = simulate_against(
            &[auto_attack(), burst.clone()],
            2_000,
            2500.0,
            0.0,
            1100.0,
            dummy,
        );
        assert!(short.downed, "500 HP dummy should drop in a 2s burst");
        assert!(
            !short.finished,
            "2s window cannot fit 1s invuln + 3.5s stomp"
        );

        let long = simulate_against(&[auto_attack(), burst], 10_000, 2500.0, 0.0, 1100.0, dummy);
        assert!(long.downed);
        assert!(long.finished, "10s window covers stomp after invuln");
    }

    #[test]
    fn open_dummy_does_not_track_downstate() {
        let result = simulate(&[auto_attack()], 5_000, 2000.0, 0.0, 1100.0);
        assert!(!result.downed);
        assert!(!result.finished);
    }

    #[test]
    fn crowd_control_sets_has_interrupt() {
        let cc = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 7,
            name: "Daze".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 0,
            cooldown_ms: 10_000,
            effects: vec![SkillEffect::CrowdControl {
                kind: crate::rotation::ControlKind::Daze,
                duration_ms: 500,
                stops_dodge: false,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };
        let result = simulate(&[auto_attack(), cc], 2_000, 2000.0, 0.0, 1100.0);
        assert!(result.has_interrupt);
        assert!(!simulate(&[auto_attack()], 2_000, 2000.0, 0.0, 1100.0).has_interrupt);
    }

    #[test]
    fn fury_buff_value_follows_game_mode() {
        // Fury is +25pp crit in PvE but +20pp in PvP/WvW, so the same Fury
        // application must be worth less outside PvE.
        let pve = estimate_buff_dps_value("Fury", 1, 6000, 2000.0, 1100.0, &GameMode::PvE);
        let wvw = estimate_buff_dps_value("Fury", 1, 6000, 2000.0, 1100.0, &GameMode::WvW);
        let pvp = estimate_buff_dps_value("Fury", 1, 6000, 2000.0, 1100.0, &GameMode::PvP);
        assert!(pve > 0.0, "pve fury value should be positive: {pve}");
        assert!(
            wvw < pve,
            "wvw fury ({wvw}) must be worth less than pve ({pve})"
        );
        assert!((wvw - pvp).abs() < f64::EPSILON, "pvp and wvw share 0.20");
        assert!(
            (wvw / pve - 0.8).abs() < 1e-9,
            "0.20/0.25 = 0.8, got {}",
            wvw / pve
        );
    }

    #[test]
    fn alacrity_recharges_a_ten_second_skill_in_eight() {
        // Wiki Alacrity (2026-08-29): +25% recharge, 10s CD → 8s while Alacrity lasts.
        // 33% (TICK_MS/3) recasts a third time inside 15.5s; 25% does not.
        let skill = RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 9,
            name: "Alacrity Skill".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 100,
            cooldown_ms: 10_000,
            effects: vec![
                SkillEffect::ApplyBuff {
                    buff: "Alacrity".into(),
                    stacks: 1,
                    duration_ms: 30_000,
                },
                SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 1.0,
                },
            ],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        };
        let result = simulate(&[skill], 15_500, 2000.0, 0.0, 1100.0);
        let casts = result
            .skill_usage
            .iter()
            .find(|u| u.name == "Alacrity Skill")
            .map(|u| u.cast_count)
            .unwrap_or(0);
        assert_eq!(
            casts, 2,
            "10s CD with Alacrity 25% is two casts in 15.5s; 33% sneaks a third (got {casts})"
        );
    }

    fn paid_bleed_ticks(remaining_ms: u32, window_ms: u32) -> (f64, f64) {
        let params = SimParams::basic(1_000.0, 1_000.0, 1_100.0);
        let mut sim = SimState::new(
            &[],
            window_ms,
            TargetState::from_seed(EnemyDummy::open()),
            params,
        );
        let slot = sim.condition_slot("Bleeding");
        sim.target.conditions.push(TimedFoeCondition {
            name: "Bleeding".into(),
            stacks: 1,
            expires_at_ms: remaining_ms,
            next_tick_ms: CONDITION_TICK_INTERVAL_MS,
        });
        while sim.current_time_ms < window_ms {
            sim.tick_conditions(1_000.0);
            sim.current_time_ms += TICK_MS;
        }
        let ticks = sim.condition_ticks[slot];
        (sim.total_condition_damage, ticks)
    }

    #[test]
    fn fractional_tick_500ms_is_half_not_full() {
        // Wiki Condition: leftover fraction of a second pays that fraction.
        // Old wall-clock pulse paid a full 1s tick whenever remaining_ms > 0.
        let window_ms = 2_500;
        let (damage, ticks) = paid_bleed_ticks(500, window_ms);
        let tick = condition_tick_damage("Bleeding", 1_000.0, &GameMode::PvE);
        assert!(
            (ticks - 0.5).abs() < 1e-9,
            "500ms must pay 0.5 ticks, got {ticks}"
        );
        assert!(
            (damage - 0.5 * tick).abs() < 1e-6,
            "500ms damage {damage} != 0.5*{tick}"
        );
        assert!(
            (ticks - 1.0).abs() > 0.1,
            "500ms must not pay a full 1s tick"
        );
    }

    #[test]
    fn fractional_tick_1500ms_is_one_and_a_half_not_two() {
        // 1500ms = 1 full + 0.5 leftover. Window includes t=2000 so the old
        // boundary clock would have paid a second full tick.
        let window_ms = 2_500;
        let (damage, ticks) = paid_bleed_ticks(1_500, window_ms);
        let tick = condition_tick_damage("Bleeding", 1_000.0, &GameMode::PvE);
        assert!(
            (ticks - 1.5).abs() < 1e-9,
            "1500ms must pay 1.5 ticks, got {ticks}"
        );
        assert!(
            (damage - 1.5 * tick).abs() < 1e-6,
            "1500ms damage {damage} != 1.5*{tick}"
        );
        assert!(
            (ticks - 2.0).abs() > 0.1,
            "1500ms must not pay 2 full ticks"
        );
    }

    #[test]
    fn shared_expiry_vulnerability_applies_on_final_bleed_tick() {
        let params = SimParams::basic(1_000.0, 1_000.0, 1_100.0);
        let mut sim = SimState::new(
            &[],
            2_500,
            TargetState::from_seed(EnemyDummy::open()),
            params,
        );
        sim.target.conditions.push(TimedFoeCondition {
            name: "Bleeding".into(),
            stacks: 1,
            expires_at_ms: 2_000,
            next_tick_ms: CONDITION_TICK_INTERVAL_MS,
        });
        sim.target.conditions.push(TimedFoeCondition {
            name: "Vulnerability".into(),
            stacks: 25,
            expires_at_ms: 2_000,
            next_tick_ms: CONDITION_TICK_INTERVAL_MS,
        });
        while sim.current_time_ms < 2_500 {
            sim.tick_conditions(1_000.0);
            sim.current_time_ms += TICK_MS;
        }
        let tick = condition_tick_damage("Bleeding", 1_000.0, &GameMode::PvE);
        let expected = 2.0 * tick * 1.25;
        assert!(
            (sim.total_condition_damage - expected).abs() < 1e-6,
            "1 bleed + 25 vuln expiring at 2000ms must pay 2*tick*1.25={expected}, got {}",
            sim.total_condition_damage
        );
    }

    #[test]
    fn applybuff_chilled_and_applycondition_poison_both_in_condition_uptime() {
        let skills = [RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 80,
            name: "Alias Mix".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 500,
            cooldown_ms: 30_000,
            effects: vec![
                SkillEffect::ApplyBuff {
                    buff: "Chilled".into(),
                    stacks: 1,
                    duration_ms: 5_000,
                },
                SkillEffect::ApplyCondition {
                    condition: "Poison".into(),
                    stacks: 1,
                    duration_ms: 5_000,
                },
            ],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }];
        let result = simulate(&skills, 8_000, 1_000.0, 1_000.0, 1_100.0);
        assert!(
            result.condition_uptime.contains_key("Chilled"),
            "ApplyBuff Chilled must appear in condition_uptime, got {:?}",
            result.condition_uptime.keys().collect::<Vec<_>>()
        );
        assert!(
            result.condition_uptime.contains_key("Poisoned"),
            "ApplyCondition Poison must canonicalize to Poisoned, got {:?}",
            result.condition_uptime.keys().collect::<Vec<_>>()
        );
        assert!(
            !result.condition_uptime.contains_key("Poison"),
            "aliased Poison must not remain as a raw uptime key"
        );
        assert!(*result.condition_uptime.get("Chilled").unwrap() > 0.0);
        assert!(*result.condition_uptime.get("Poisoned").unwrap() > 0.0);
    }

    #[test]
    fn deferred_target_strike_vs_vulnerability_adds_ten_percent() {
        let skills = [auto_attack()];
        let mut params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        let target =
            TargetState::from_seed(EnemyDummy::open()).with_condition_stacks("Vulnerability", 10);
        let baseline = simulate_with_target(&skills, 5_000, &params, target.clone());
        params.deferred_target = vec![crate::combat::DeferredTargetModifier {
            gate: crate::combat::TargetGate::Condition("Vulnerability"),
            percent: 10.0,
            axis: crate::combat::TargetModAxis::Strike,
        }];
        let boosted = simulate_with_target(&skills, 5_000, &params, target);
        assert!(baseline.strike_dps > 0.0);
        let ratio = boosted.strike_dps / baseline.strike_dps;
        assert!(
            (ratio - 1.10).abs() < 1e-6,
            "+10% strike vs Vulnerability must raise strike 1.10x, got {ratio}"
        );
    }

    #[test]
    fn soft_control_weight_single_pass_matches_per_name_scans() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Chilled", 1, 5_000, 0, 5);
        t.apply_condition("Chilled", 1, 8_000, 0, 5);
        t.apply_condition("Burning", 3, 5_000, 0, 25);
        t.apply_condition("Weakness", 1, 2_000, 0, 5);
        t.apply_condition("Slow", 1, 1_000, 0, 5);
        t.apply_condition("Blind", 1, 4_000, 0, 5);
        for now in [0, 1_000, 1_500, 2_000, 5_000] {
            let expected = {
                let mut present = 0u8;
                for (i, name) in super::SOFT_CONTROL.iter().enumerate() {
                    if t.stacks_of(name, now) > 0 {
                        present |= 1 << i;
                    }
                }
                present.count_ones() as f64 * 0.5
            };
            assert_eq!(
                super::soft_control_weight_on(&t, now),
                expected,
                "single-pass weight must match five stacks_of scans at {now}"
            );
        }
        assert_eq!(super::soft_control_weight_on(&t, 1_500), 1.5);
        assert_eq!(super::soft_control_weight_on(&t, 2_000), 1.0);

        let mut raw = TargetState::from_seed(EnemyDummy::open());
        raw.conditions.push(TimedFoeCondition {
            name: "Blind".into(),
            stacks: 1,
            expires_at_ms: 4_000,
            next_tick_ms: 1_000,
        });
        // Pre-FCR-010 stacks_of: canonicalize each stored row, then match.
        // Live stacks_of (FCR-010) folds the query only — raw "Blind" would miss.
        let raw_expected = {
            let mut present = 0u8;
            for (i, name) in super::SOFT_CONTROL.iter().enumerate() {
                let want = crate::data::boon_condition_formulas::canonical_condition_name(name);
                if raw.conditions.iter().any(|c| {
                    c.stacks > 0
                        && c.expires_at_ms > 0
                        && crate::data::boon_condition_formulas::canonical_condition_name(&c.name)
                            .eq_ignore_ascii_case(want)
                }) {
                    present |= 1 << i;
                }
            }
            present.count_ones() as f64 * 0.5
        };
        assert_eq!(
            super::soft_control_weight_on(&raw, 0),
            raw_expected,
            "raw alias row Blind must match five canonicalize-stored scans"
        );
        assert_eq!(raw_expected, 0.5);

        let mut zero = TargetState::from_seed(EnemyDummy::open());
        zero.conditions.push(TimedFoeCondition {
            name: "Blind".into(),
            stacks: 0,
            expires_at_ms: 4_000,
            next_tick_ms: 1_000,
        });
        let zero_expected = {
            let mut present = 0u8;
            for (i, name) in super::SOFT_CONTROL.iter().enumerate() {
                let want = crate::data::boon_condition_formulas::canonical_condition_name(name);
                if zero.conditions.iter().any(|c| {
                    c.stacks > 0
                        && c.expires_at_ms > 0
                        && crate::data::boon_condition_formulas::canonical_condition_name(&c.name)
                            .eq_ignore_ascii_case(want)
                }) {
                    present |= 1 << i;
                }
            }
            present.count_ones() as f64 * 0.5
        };
        assert_eq!(
            super::soft_control_weight_on(&zero, 0),
            zero_expected,
            "zero-stack Blind row must match old-scan oracle"
        );
        assert_eq!(zero_expected, 0.0);
        assert_eq!(super::soft_control_weight_on(&zero, 0), 0.0);
    }

    fn strike_skill() -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 60,
            name: "Power Hit".into(),
            slot: SkillSlot::Weapon2,
            cast_time_ms: 500,
            cooldown_ms: 20_000,
            effects: vec![SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    fn bleed_skill(duration_ms: u32) -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 61,
            name: "Bleed Hit".into(),
            slot: SkillSlot::Weapon3,
            cast_time_ms: 500,
            cooldown_ms: 20_000,
            effects: vec![SkillEffect::ApplyCondition {
                condition: "Bleeding".into(),
                stacks: 1,
                duration_ms,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    #[test]
    fn high_crit_params_flip_strike_vs_condi_pick() {
        let power = 2_000.0;
        let condition_damage = 1_000.0;
        let weapon_strength = 1_100.0;
        let old_strike = weapon_strength * power / reference_armor();
        let tick = condition_tick_damage("Bleeding", condition_damage, &GameMode::PvE);
        // One extra second so the old (no-crit) strike term loses to condi.
        let bleed_ms = ((old_strike / tick).ceil() as u32 + 1) * 1_000;
        let strike = strike_skill();
        let condi = bleed_skill(bleed_ms);
        let old_condi = tick * (bleed_ms as f64 / 1_000.0);
        assert!(
            old_condi > old_strike,
            "old formula must prefer condi ({old_condi} > {old_strike})"
        );

        let no_crit = SimParams::basic(power, condition_damage, weapon_strength);
        let no_crit_strike = skill_dps_efficiency(
            &strike,
            power,
            condition_damage,
            weapon_strength,
            &no_crit,
            0.0,
            false,
        );
        let no_crit_condi = skill_dps_efficiency(
            &condi,
            power,
            condition_damage,
            weapon_strength,
            &no_crit,
            0.0,
            false,
        );
        assert!(
            no_crit_condi > no_crit_strike,
            "precision=0 must match old formula (condi wins)"
        );

        let mut high_crit = no_crit.clone();
        high_crit.precision = 2_500.0;
        high_crit.ferocity = 2_000.0;
        let high_strike = skill_dps_efficiency(
            &strike,
            power,
            condition_damage,
            weapon_strength,
            &high_crit,
            0.0,
            false,
        );
        let high_condi = skill_dps_efficiency(
            &condi,
            power,
            condition_damage,
            weapon_strength,
            &high_crit,
            0.0,
            false,
        );
        assert!(
            high_strike > high_condi,
            "high crit must flip DPCT to strike ({high_strike} vs {high_condi})"
        );

        let skills = vec![strike, condi];
        let sim_old = SimState::new(
            &skills,
            5_000,
            TargetState::from_seed(EnemyDummy::open()),
            no_crit,
        );
        assert_eq!(
            sim_old.pick_skill(power),
            Some(1),
            "old formula / no-crit params pick condi"
        );
        let sim_new = SimState::new(
            &skills,
            5_000,
            TargetState::from_seed(EnemyDummy::open()),
            high_crit,
        );
        assert_eq!(
            sim_new.pick_skill(power),
            Some(0),
            "high-crit params pick strike"
        );
    }
    // Reaper slice interaction pair (specs/004-simulator-trust)

    /// 2 × 2 over Might stacks {0, 25} and `strike_mult` {1.0, 1.1} on total
    /// strike damage. Under the multiplicative model
    /// `damage = (power + might·30) · strike_mult · …` the interaction term
    /// `f(A+B) − f(A) − f(B) + f(base)` equals `might·30·0.1·hits`, so its
    /// sign is positive. eps 1.0 (damage units) absorbs f64 rounding.
    #[test]
    fn reaper_interaction_might_times_strike_modifier() {
        fn strike() -> RotationSkill {
            RotationSkill {
                targets: 1,
                categories: Vec::new(),
                slot_name: None,
                skill_id: 1,
                name: "strike".into(),
                slot: SkillSlot::Weapon1,
                cast_time_ms: 500,
                cooldown_ms: 0,
                effects: vec![SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 1.0,
                }],
                next_chain: None,
                is_stunbreak: false,
                reaches_allies: false,
                weapon_set: 0,
            }
        }
        fn might(stacks: u32) -> RotationSkill {
            RotationSkill {
                targets: 1,
                categories: Vec::new(),
                slot_name: None,
                skill_id: 2,
                name: "might".into(),
                slot: SkillSlot::Utility,
                cast_time_ms: 250,
                cooldown_ms: 60_000,
                effects: vec![SkillEffect::ApplyBuff {
                    buff: "Might".into(),
                    stacks,
                    duration_ms: 60_000,
                }],
                next_chain: None,
                is_stunbreak: false,
                reaches_allies: false,
                weapon_set: 0,
            }
        }
        let total = |stacks: u32, strike_mult: f64| -> f64 {
            let mut params = SimParams::basic(2_000.0, 0.0, 1_100.0);
            params.strike_mult = strike_mult;
            let result = simulate_with(
                &[strike(), might(stacks)],
                30_000,
                &params,
                EnemyDummy::default(),
            );
            result.strike_dps * 30.0
        };
        let base = total(0, 1.0);
        let a = total(25, 1.0);
        let b = total(0, 1.1);
        let ab = total(25, 1.1);
        let interaction = ab - a - b + base;
        assert!(
            a > base && b > base,
            "each factor alone raises damage: {base} {a} {b}"
        );

        assert!(
            interaction > 1.0,
            "multiplicative model predicts a positive interaction; got {interaction} (base {base}, might {a}, mult {b}, both {ab})"
        );
    }

    // --- Phase 3 Kent probes (Success [5]) ---

    #[test]
    fn kent_a_prestacked_vulnerability_boosts_strike_cap_25() {
        let skills = [auto_attack()];
        let params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        let open = simulate_with_target(
            &skills,
            5_000,
            &params,
            TargetState::from_seed(EnemyDummy::open()),
        );
        let vuln10 = simulate_with_target(
            &skills,
            5_000,
            &params,
            TargetState::from_seed(EnemyDummy::open()).with_condition_stacks("Vulnerability", 10),
        );
        let vuln25 = simulate_with_target(
            &skills,
            5_000,
            &params,
            TargetState::from_seed(EnemyDummy::open()).with_condition_stacks("Vulnerability", 25),
        );
        let vuln30 = simulate_with_target(
            &skills,
            5_000,
            &params,
            TargetState::from_seed(EnemyDummy::open()).with_condition_stacks("Vulnerability", 30),
        );
        assert!(
            vuln10.strike_dps > open.strike_dps,
            "10 Vulnerability stacks must raise strike vs open seed"
        );
        let ratio10 = vuln10.strike_dps / open.strike_dps;
        assert!(
            (ratio10 - 1.10).abs() < 1e-6,
            "10 stacks = +10% incoming; got ratio {ratio10}"
        );
        let ratio25 = vuln25.strike_dps / open.strike_dps;
        assert!(
            (ratio25 - 1.25).abs() < 1e-6,
            "25 stacks = +25% incoming; got ratio {ratio25}"
        );
        assert!(
            (vuln30.strike_dps - vuln25.strike_dps).abs() < 1e-6,
            "Vulnerability intensity caps at 25"
        );
    }

    #[test]
    fn kent_c_enemy_dummy_seed_only_prot_stab_hp() {
        let seed = EnemyDummy {
            protection: true,
            stability: true,
            hp: Some(13_000.0),
        };
        let live = TargetState::from_seed(seed);
        assert!(live.protection);
        assert!(live.stability);
        assert_eq!(live.hp, Some(13_000.0));
        assert_eq!(live.disabled_until_ms, 0);
        assert!(live.conditions.is_empty());
        // Seed shape stays three fields — from_seed copies them and adds live ledger fields.
        let open = EnemyDummy::open();
        assert!(!open.protection && !open.stability && open.hp.is_none());
    }

    #[test]
    fn kent_d_flow_and_wvw_share_one_target_state_ledger() {
        // Both flow sim and WvW timeline import TargetState / TimedFoeCondition from
        // combat_model; WvW aliases TimedCondition = TimedFoeCondition (no second foe model).
        let mut ledger = TargetState::from_seed(EnemyDummy::open());
        ledger.apply_condition("Vulnerability", 5, 10_000, 0, 25);
        ledger.extend_disable(3_000);
        assert_eq!(ledger.stacks_of("Vulnerability", 0), 5);
        assert!(ledger.is_disabled(1_000));
        assert!(!ledger.is_disabled(3_000));
        assert_eq!(ledger.conditions.len(), 1);
        assert_eq!(ledger.conditions[0].name, "Vulnerability");
        assert_eq!(ledger.conditions[0].stacks, 5);
        // Same TimedFoeCondition shape the WvW timeline pushes onto target.conditions.
        let shared = TimedFoeCondition {
            name: "Burning".into(),
            stacks: 2,
            expires_at_ms: 8_000,
            next_tick_ms: 1_000,
        };
        ledger.conditions.push(shared);
        assert_eq!(ledger.stacks_of("Burning", 0), 2);
    }

    /// E0 Kent: flow sim and WvW timeline share one TriggerBus / Endurance / Dodge family.
    #[test]
    fn kent_e0_flow_and_wvw_share_trigger_bus_family() {
        use super::super::trigger_bus::{
            BusEvent, DodgeAction, EndurancePool, TriggerBus, DODGE_COST,
        };
        let mut pool = EndurancePool::new_full();
        let mut bus = TriggerBus::new();
        let mut dodge = DodgeAction::new();
        assert!(dodge.try_dodge(&mut pool, &mut bus, 0));
        assert_eq!(bus.count(BusEvent::OnDodge), 1);
        assert!((pool.spent - DODGE_COST).abs() < 1e-9);
        // Same types the timeline holds — not a second dodge path.
        let _also: TriggerBus = TriggerBus::new();
        let _also_pool: EndurancePool = EndurancePool::new_full();
        let _also_attune: super::super::attunement::AttunementState =
            super::super::attunement::AttunementState::new();
        let _also_illusion: super::super::illusion::IllusionState =
            super::super::illusion::IllusionState::new();
    }

    fn run_flow_auto() -> SimState {
        let skills = vec![auto_attack()];
        let mut sim = SimState::new(
            &skills,
            5_000,
            TargetState::from_seed(EnemyDummy::open()),
            SimParams::basic(2_000.0, 0.0, 1_000.0),
        );
        sim.run();
        sim
    }

    /// E0 Kent: flow either emits OnDodge from `run`, or the named abstain
    /// stays in place. Deleting the abstain without wiring `try_dodge` fails.
    #[test]
    fn kent_flow_e0_dodge_emits_or_abstains() {
        use super::super::trigger_bus::{BusEvent, DODGE_COST};

        let sim = run_flow_auto();
        let dodges = sim.trigger_bus.count(BusEvent::OnDodge);
        if FLOW_E0_DODGE_UNWIRED {
            assert!(
                !FLOW_E0_DODGE_UNWIRED_WHY.trim().is_empty(),
                "dodge abstain needs a reason"
            );
            assert_eq!(
                dodges, 0,
                "{}: flow run emitted OnDodge; set FLOW_E0_DODGE_UNWIRED=false if wired",
                FLOW_E0_DODGE_UNWIRED_WHY
            );
            assert_eq!(
                sim.dodge_action.dodges, 0,
                "unwired flow must not increment DodgeAction"
            );
            assert!(
                sim.endurance.can_dodge(DODGE_COST),
                "unwired flow must not spend endurance"
            );
        } else {
            assert!(
                dodges >= 1,
                "FLOW_E0_DODGE_UNWIRED is false: run must emit OnDodge (wire try_dodge)"
            );
            assert!(sim.dodge_action.dodges >= 1);
        }
    }

    /// E4 Kent: flow either uses IllusionState, or the named abstain stays.
    /// Deleting the abstain without calling spawn_clone fails; dropping the
    /// field without wiring fails compile (this test reads it).
    #[test]
    fn kent_flow_e4_illusion_not_dead() {
        use super::super::illusion::CLONE_CAP;
        use super::super::trigger_bus::BusEvent;

        let sim = run_flow_auto();
        let spawned = sim.trigger_bus.count(BusEvent::OnCloneCreated);
        if FLOW_E4_ILLUSION_UNWIRED {
            assert!(
                !FLOW_E4_ILLUSION_UNWIRED_WHY.trim().is_empty(),
                "illusion abstain needs a reason"
            );
            assert_eq!(
                sim.illusion.count, 0,
                "{}: flow run spawned clones; set FLOW_E4_ILLUSION_UNWIRED=false if wired",
                FLOW_E4_ILLUSION_UNWIRED_WHY
            );
            assert_eq!(
                spawned, 0,
                "{}: flow run emitted OnCloneCreated; set FLOW_E4_ILLUSION_UNWIRED=false if wired",
                FLOW_E4_ILLUSION_UNWIRED_WHY
            );
            assert_eq!(sim.illusion.cap, CLONE_CAP);
        } else {
            assert!(
                spawned >= 1 || sim.illusion.count >= 1,
                "FLOW_E4_ILLUSION_UNWIRED is false: run must spawn_clone or raise count"
            );
        }
    }

    /// E1 Kent: flow CrowdControl that lands emits OnDisableFoe; Stability does not.
    #[test]
    fn kent_e1_flow_landed_disable_emits_on_disable_foe() {
        use super::super::trigger_bus::BusEvent;

        let skills = vec![auto_attack(), stun_skill()];
        let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
        params.intent = Some(OptimizationWeights {
            power: 0.2,
            condition: 0.0,
            boon_support: 0.0,
            healing: 0.0,
            sustain: 0.0,
            control: 1.0,
        });

        let mut open = SimState::new(
            &skills,
            10_000,
            TargetState::from_seed(EnemyDummy::open()),
            params.clone(),
        );
        open.run();
        let landed = open.trigger_bus.count(BusEvent::OnDisableFoe);
        assert!(
            landed >= 1,
            "flow must emit OnDisableFoe when a disable lands; got {landed}"
        );
        assert!(
            open.target.disabled_until_ms > 0,
            "landed disable must extend TargetState.disabled_until_ms"
        );

        let mut stab = EnemyDummy::open();
        stab.stability = true;
        let mut blocked = SimState::new(&skills, 10_000, TargetState::from_seed(stab), params);
        blocked.run();
        assert_eq!(
            blocked.trigger_bus.count(BusEvent::OnDisableFoe),
            0,
            "Stability must block flow OnDisableFoe emit"
        );
        assert_eq!(blocked.target.disabled_until_ms, 0);
    }

    /// E2 Kent: flow sim and WvW share resolve_trait_skill (cast scheduler).
    #[test]
    fn kent_e2_flow_shares_resolve_trait_skill() {
        use super::super::trait_skill::{resolve_trait_skill, skill_effects_from_status_operation};
        use crate::data::normalized_effects::{
            AmountMode, OperationType, StatusOperation, TargetScope, TargetSide,
        };
        use crate::data::quality::FactualValue;
        use std::collections::HashMap;

        let op = StatusOperation {
            operation_type: OperationType::AppliesBoon,
            target_side: TargetSide::Self_,
            status_kind: "Arcane Shield".into(),
            amount_mode: AmountMode::Stacks,
            amount_value: FactualValue::Resolved(3.0),
            base_duration_ms: Some(FactualValue::Resolved(5_000)),
            target_scope: TargetScope::Self_,
            target_count: None,
            internal_cooldown_ms: None,
            source_duration_multiplier: None,
        };
        let effects = skill_effects_from_status_operation(&op);
        let mut catalog = HashMap::new();
        catalog.insert(25_579u32, effects);
        let resolved = resolve_trait_skill(25_579, &catalog).expect("shared resolve");
        assert!(
            !resolved.is_empty(),
            "resolve_trait_skill must return lesser SkillEffects"
        );
        assert!(resolve_trait_skill(1, &catalog).is_none());
    }

    fn phase4_combo_skill(
        id: u32,
        name: &str,
        effects: Vec<SkillEffect>,
        cast_ms: u32,
        cd_ms: u32,
    ) -> RotationSkill {
        RotationSkill {
            skill_id: id,
            name: name.into(),
            slot: SkillSlot::Utility,
            cast_time_ms: cast_ms,
            cooldown_ms: cd_ms,
            effects,
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
            categories: vec![],
            slot_name: Some("Utility".into()),
            targets: 1,
        }
    }

    #[test]
    fn phase4_fire_blast_might_requires_field() {
        let field = phase4_combo_skill(
            9001,
            "Fire Field",
            vec![
                SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 0.05,
                },
                SkillEffect::ComboField {
                    field_type: "Fire".into(),
                    duration_ms: 5_000,
                },
            ],
            300,
            20_000,
        );
        let blast = phase4_combo_skill(
            9002,
            "Blast Finisher",
            vec![
                SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 0.04,
                },
                SkillEffect::ComboFinisher {
                    finisher_type: "Blast".into(),
                    percent: 100,
                },
            ],
            300,
            20_000,
        );
        let with_field = simulate(
            &[field.clone(), blast.clone()],
            2_500,
            2_000.0,
            0.0,
            1_000.0,
        );
        let finisher_only = simulate(&[blast], 2_500, 2_000.0, 0.0, 1_000.0);
        assert!(
            with_field.might_stacks_avg > 0.5,
            "Fire+Blast Might avg={}",
            with_field.might_stacks_avg
        );
        assert!(
            finisher_only.might_stacks_avg < 0.05,
            "Blast alone Might avg={}",
            finisher_only.might_stacks_avg
        );
    }

    #[test]
    fn phase4_water_blast_heal_scales_with_healing_power() {
        let mk = |hp: f64| {
            let field = phase4_combo_skill(
                9011,
                "Water Field",
                vec![
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 0.05,
                    },
                    SkillEffect::ComboField {
                        field_type: "Water".into(),
                        duration_ms: 5_000,
                    },
                ],
                300,
                20_000,
            );
            let blast = phase4_combo_skill(
                9012,
                "Water Blast",
                vec![
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 0.04,
                    },
                    SkillEffect::ComboFinisher {
                        finisher_type: "Blast".into(),
                        percent: 100,
                    },
                ],
                300,
                20_000,
            );
            let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
            params.healing_power = hp;
            simulate_with(&[field, blast], 2_500, &params, EnemyDummy::open())
        };
        let low = mk(0.0);
        let high = mk(1_000.0);
        assert!(
            high.healing_per_second > low.healing_per_second + 50.0,
            "heal must rise with healing_power: {} vs {}",
            high.healing_per_second,
            low.healing_per_second
        );
    }

    #[test]
    fn phase4_fire_projectile_burning_via_target_state() {
        let field = phase4_combo_skill(
            9021,
            "Fire Field",
            vec![
                SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 0.05,
                },
                SkillEffect::ComboField {
                    field_type: "Fire".into(),
                    duration_ms: 5_000,
                },
            ],
            300,
            20_000,
        );
        let proj = phase4_combo_skill(
            9022,
            "Fire Projectile",
            vec![
                SkillEffect::StrikeDamage {
                    hit_count: 1,
                    dmg_multiplier: 0.04,
                },
                SkillEffect::ComboFinisher {
                    finisher_type: "Projectile".into(),
                    percent: 100,
                },
            ],
            300,
            20_000,
        );
        let result = simulate(&[field, proj], 2_500, 2_000.0, 0.0, 1_000.0);
        let burning = result
            .condition_uptime
            .get("Burning")
            .copied()
            .unwrap_or(0.0);
        assert!(
            burning > 0.0,
            "Burning missing: {:?}",
            result.condition_uptime
        );
    }

    #[test]
    fn phase4_projectile_ev_scales_20_vs_100() {
        let mk = |percent: u32| {
            let field = phase4_combo_skill(
                9031,
                "Fire Field",
                vec![
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 0.05,
                    },
                    SkillEffect::ComboField {
                        field_type: "Fire".into(),
                        duration_ms: 5_000,
                    },
                ],
                300,
                20_000,
            );
            let proj = phase4_combo_skill(
                9032,
                "Proj",
                vec![
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 0.04,
                    },
                    SkillEffect::ComboFinisher {
                        finisher_type: "Projectile".into(),
                        percent,
                    },
                ],
                300,
                20_000,
            );
            simulate(&[field, proj], 2_500, 2_000.0, 0.0, 1_000.0)
        };
        let b100 = mk(100)
            .condition_uptime
            .get("Burning")
            .copied()
            .unwrap_or(0.0);
        let b20 = mk(20)
            .condition_uptime
            .get("Burning")
            .copied()
            .unwrap_or(0.0);
        assert!(b100 > 0.0, "100% burning");
        assert!(
            (b20 / b100 - 0.20).abs() < 0.12,
            "20% EV ~0.2x: {b20} vs {b100}"
        );
    }

    fn attune_skill(skill_id: u32, name: &str) -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: Some("Profession_2".into()),
            skill_id,
            name: name.into(),
            slot: SkillSlot::Profession,
            cast_time_ms: 0,
            cooldown_ms: 10_000,
            effects: vec![],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    #[test]
    fn fcr006_weaver_simstate_stashes_outgoing_primary() {
        use super::super::attunement::{apply_attunement_skill, Element};
        use super::super::trigger_bus::BusEvent;

        let skills = vec![attune_skill(5493, "Water Attunement")];
        let mut params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        params.weaver = true;
        let mut sim = SimState::new(
            &skills,
            5_000,
            TargetState::from_seed(EnemyDummy::open()),
            params,
        );
        assert!(sim.attunement.weaver);
        let outgoing = sim.attunement.current;
        let landed = apply_attunement_skill(
            &mut sim.attunement,
            &mut sim.trigger_bus,
            0,
            "Water Attunement",
        );
        assert_eq!(landed, Some(Element::Water));
        assert_eq!(sim.attunement.secondary, Some(outgoing));
        assert_eq!(sim.trigger_bus.count(BusEvent::OnAttunementSwap), 1);
    }

    #[test]
    fn fcr006_core_ele_simstate_secondary_stays_none() {
        use super::super::attunement::{apply_attunement_skill, Element};

        let skills = vec![attune_skill(5493, "Water Attunement")];
        let mut sim = SimState::new(
            &skills,
            5_000,
            TargetState::from_seed(EnemyDummy::open()),
            SimParams::basic(2_000.0, 0.0, 1_100.0),
        );
        assert!(!sim.attunement.weaver);
        apply_attunement_skill(
            &mut sim.attunement,
            &mut sim.trigger_bus,
            0,
            "Water Attunement",
        );
        assert_eq!(sim.attunement.current, Element::Water);
        assert_eq!(sim.attunement.secondary, None);
    }

    #[test]
    fn fcr006_weaver_field_enables_core_attune_names() {
        let skills = vec![
            attune_skill(5492, "Fire Attunement"),
            attune_skill(5493, "Water Attunement"),
        ];
        assert!(skills
            .iter()
            .all(|s| !super::super::attunement::is_weaver_name(&s.name)));
        let mut params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        params.weaver = true;
        let sim = SimState::new(
            &skills,
            1_000,
            TargetState::from_seed(EnemyDummy::open()),
            params,
        );
        assert!(
            sim.attunement.weaver,
            "SimParams.weaver must enable Weaver with only core attune names"
        );
    }

    #[test]
    fn fcr007_flow_sim_selects_attunement_swap() {
        use super::super::attunement::Element;
        use super::super::trigger_bus::BusEvent;

        let skills = vec![auto_attack(), attune_skill(5493, "Water Attunement")];
        let mut sim = SimState::new(
            &skills,
            10_000,
            TargetState::from_seed(EnemyDummy::open()),
            SimParams::basic(2_000.0, 0.0, 1_100.0),
        );
        sim.run();
        let swaps = sim.trigger_bus.count(BusEvent::OnAttunementSwap);
        assert!(
            swaps >= 1,
            "flow must cast an attune swap; OnAttunementSwap={swaps}"
        );
        assert_ne!(
            sim.attunement.current,
            Element::Fire,
            "attune pick must leave Fire"
        );
        assert!(
            sim.skill_casts.get(&5493).copied().unwrap_or(0) >= 1,
            "Water Attunement must appear in the cast log"
        );
    }

    #[test]
    fn fcr007_flow_sim_attune_swaps_once_then_autos() {
        use super::super::trigger_bus::BusEvent;

        let attunes = vec![
            attune_skill(5492, "Fire Attunement"),
            attune_skill(5493, "Water Attunement"),
            attune_skill(5494, "Air Attunement"),
            attune_skill(5495, "Earth Attunement"),
        ];
        let mut with = vec![auto_attack()];
        with.extend(attunes);
        let without = vec![auto_attack()];
        let params = SimParams::basic(2_000.0, 0.0, 1_100.0);
        let dummy = TargetState::from_seed(EnemyDummy::open());
        let mut sim_with = SimState::new(&with, 10_000, dummy.clone(), params.clone());
        sim_with.run();
        let mut sim_without = SimState::new(&without, 10_000, dummy, params);
        sim_without.run();
        assert_eq!(
            sim_with.trigger_bus.count(BusEvent::OnAttunementSwap),
            1,
            "four attunes must not cycle; exactly one swap per run"
        );
        let auto_with = sim_with.skill_casts.get(&1).copied().unwrap_or(0);
        let auto_without = sim_without.skill_casts.get(&1).copied().unwrap_or(0);
        assert!(
            auto_with.abs_diff(auto_without) <= 1,
            "autos with attunes ({auto_with}) must stay within 1 of autos-only ({auto_without})"
        );
    }

    // Forms (sprint 008, shroud / Celestial Avatar in the flow simulation)

    fn bar_skill(
        id: u32,
        name: &str,
        slot: SkillSlot,
        set: u8,
        cast_time_ms: u32,
        cooldown_ms: u32,
        effects: Vec<SkillEffect>,
    ) -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: id,
            name: name.into(),
            slot,
            cast_time_ms,
            cooldown_ms,
            effects,
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: set,
        }
    }

    fn strike(hit_count: u32, dmg_multiplier: f64) -> Vec<SkillEffect> {
        vec![SkillEffect::StrikeDamage {
            hit_count,
            dmg_multiplier,
        }]
    }

    /// Coefficients of the strikes one cast of `skill` schedules.
    fn scheduled_coefficients(skill: RotationSkill) -> Vec<f64> {
        let skills = [skill];
        let mut sim = SimState::new(
            &skills,
            10_000,
            TargetState::from_seed(EnemyDummy::open()),
            SimParams::basic(2_000.0, 0.0, 1_000.0),
        );
        sim.use_skill(0, 2_000.0, 1_000.0);
        sim.scheduled_hits
            .iter()
            .map(|h| h.dmg_multiplier)
            .collect()
    }

    /// The API `dmg_multiplier` is the coefficient of ONE strike; a cast
    /// lands `hit_count` of them (regression: 2090739 divided it by the
    /// hit count, so multi-hit skills dealt 1/hit_count of their damage).
    #[test]
    fn a_multi_hit_cast_lands_hit_count_strikes_of_the_full_coefficient() {
        let skill = bar_skill(1, "Triple", SkillSlot::Weapon2, 0, 900, 0, strike(3, 0.5));
        let hits = scheduled_coefficients(skill);
        assert_eq!(hits, vec![0.5; 3]);
        assert!((hits.iter().sum::<f64>() - 1.5).abs() < 1e-9);
    }

    /// Wiki Soul Spiral (fetched 2026-09-24, PvE):
    /// "Damage (12x): 2,232 (8.4)" and "Increased power coefficient from 0.6
    /// to 0.7 in PvE only." The API fact (skill 30504) is hit_count 12,
    /// dmg_multiplier 0.7: 12 strikes x 0.7 = 8.4 over the cast.
    #[test]
    fn soul_spiral_lands_twelve_strikes_totalling_the_wiki_coefficient() {
        let skill = bar_skill(
            30504,
            "Soul Spiral",
            SkillSlot::Weapon2,
            0,
            3_000,
            0,
            strike(12, 0.7),
        );
        let hits = scheduled_coefficients(skill);
        assert_eq!(hits.len(), 12);
        assert!(hits.iter().all(|c| (c - 0.7).abs() < 1e-9));
        assert!((hits.iter().sum::<f64>() - 8.4).abs() < 1e-9);
    }

    const FORM_ENTRY: u32 = 900;

    /// A 100-point pool that lasts 10 s, 10 s recharge, no gains.
    fn test_form(initial_pool: f64) -> FormSpec {
        FormSpec {
            name: "Test Form".into(),
            entry_skill_id: FORM_ENTRY,
            pool_cap: 100.0,
            initial_pool,
            entry_floor: 10.0,
            drain_per_second: 10.0,
            recharge_ms: 10_000,
            gains_in_form: true,
            exit_keep: 1.0,
            ..Default::default()
        }
    }

    fn run_form_sim(skills: &[RotationSkill], duration_ms: u32, form: FormSpec) -> SimState {
        run_sim(skills, duration_ms, Some(form), Vec::new())
    }

    fn run_sim(
        skills: &[RotationSkill],
        duration_ms: u32,
        form: Option<FormSpec>,
        triggered: Vec<TriggeredProc>,
    ) -> SimState {
        let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
        params.form = form;
        params.triggered = triggered;
        let mut sim = SimState::new(
            skills,
            duration_ms,
            TargetState::from_seed(EnemyDummy::open()),
            params,
        );
        sim.run();
        sim
    }

    fn casts(sim: &SimState, id: u32) -> u32 {
        sim.skill_casts.get(&id).copied().unwrap_or(0)
    }

    #[test]
    fn a_full_pool_enters_the_form_and_an_empty_one_leaves_it() {
        let skills = vec![
            bar_skill(
                1,
                "Weapon Auto",
                SkillSlot::Weapon1,
                1,
                500,
                0,
                strike(1, 1.0),
            ),
            bar_skill(
                2,
                "Form Auto",
                SkillSlot::Weapon1,
                crate::rotation::SHROUD_SET,
                500,
                0,
                strike(1, 0.5),
            ),
        ];
        let sim = run_form_sim(&skills, 30_000, test_form(100.0));
        // Full pool: entered at once although the form's auto is weaker;
        // 100 points at 10/s is 10 s; no gains, so no second entry.
        assert_eq!(casts(&sim, FORM_ENTRY), 1);
        assert_eq!(sim.form_active_ms, 10_000);
        assert!(sim.form_entered_ms.is_none());
        assert_eq!(sim.form_pool, 0.0);
        assert_eq!(sim.active_weapon_set, 1, "the weapon bar comes back");
        assert!(casts(&sim, 1) > 0 && casts(&sim, 2) > 0);
    }

    #[test]
    fn the_weapon_bar_is_stowed_while_in_the_form() {
        let skills = vec![
            bar_skill(
                1,
                "Weapon Auto",
                SkillSlot::Weapon1,
                1,
                500,
                0,
                strike(1, 1.0),
            ),
            bar_skill(
                3,
                "Weapon Burst",
                SkillSlot::Weapon2,
                1,
                500,
                4_000,
                strike(1, 9.0),
            ),
            bar_skill(
                4,
                "Swap Skill",
                SkillSlot::Weapon2,
                2,
                500,
                4_000,
                strike(1, 9.0),
            ),
            // Always ready and not an auto: the form never runs out of a
            // reason to stay, so only the stowing rule keeps the far
            // stronger weapon skills off the bar.
            bar_skill(
                2,
                "Form Skill",
                SkillSlot::Weapon2,
                crate::rotation::SHROUD_SET,
                500,
                0,
                strike(1, 0.5),
            ),
        ];
        // The pool outlasts the window: nothing from sets 1 or 2 is cast
        // and no weapon swap happens.
        let sim = run_form_sim(&skills, 8_000, test_form(100.0));
        assert_eq!(casts(&sim, FORM_ENTRY), 1);
        assert_eq!(casts(&sim, 1) + casts(&sim, 3) + casts(&sim, 4), 0);
        assert!(casts(&sim, 2) >= 10);
        assert_eq!(sim.active_weapon_set, crate::rotation::SHROUD_SET);
    }

    #[test]
    fn the_form_needs_its_entry_floor_and_fills_from_strikes() {
        let skills = vec![
            bar_skill(
                1,
                "Weapon Auto",
                SkillSlot::Weapon1,
                1,
                500,
                0,
                strike(1, 1.0),
            ),
            bar_skill(
                2,
                "Form Auto",
                SkillSlot::Weapon1,
                crate::rotation::SHROUD_SET,
                500,
                0,
                strike(1, 2.0),
            ),
        ];
        let mut form = test_form(0.0);
        form.entry_floor = 30.0;
        // Below the floor the better form bar stays stowed.
        let empty = run_form_sim(&skills, 30_000, form.clone());
        assert_eq!(casts(&empty, FORM_ENTRY), 0);
        assert_eq!(casts(&empty, 2), 0);
        // 10 per landed strike: the third weapon hit meets the floor and
        // the better form bar is taken; no gains inside (astral force).
        form.gain_per_strike = 10.0;
        form.gains_in_form = false;
        let filled = run_form_sim(&skills, 3_000, form);
        assert_eq!(casts(&filled, FORM_ENTRY), 1);
        assert_eq!(casts(&filled, 1), 3);
        assert!(casts(&filled, 2) > 0);
    }

    #[test]
    fn the_exit_skill_leaves_early_and_keeps_its_share_of_the_pool() {
        let skills = vec![
            bar_skill(
                1,
                "Weapon Auto",
                SkillSlot::Weapon1,
                1,
                500,
                0,
                strike(1, 1.0),
            ),
            bar_skill(
                3,
                "Weapon Burst",
                SkillSlot::Weapon2,
                1,
                500,
                20_000,
                strike(1, 6.0),
            ),
            bar_skill(
                2,
                "Form Auto",
                SkillSlot::Weapon1,
                crate::rotation::SHROUD_SET,
                500,
                0,
                strike(1, 0.5),
            ),
            bar_skill(
                5,
                "Form Burst",
                SkillSlot::Weapon2,
                crate::rotation::SHROUD_SET,
                500,
                20_000,
                strike(1, 9.0),
            ),
        ];
        let mut form = test_form(100.0);
        form.exit_keep = 0.5;
        let sim = run_form_sim(&skills, 2_000, form);
        // In at 0 (full), Form Burst, then the form has only its auto and
        // Weapon Burst outranks it: the exit skill, 50 % of the pool kept.
        assert_eq!(casts(&sim, FORM_ENTRY), 1);
        assert_eq!(casts(&sim, 5), 1);
        assert_eq!(casts(&sim, 3), 1);
        assert!(sim.form_entered_ms.is_none());
        assert!(sim.form_active_ms < 1_000, "{}", sim.form_active_ms);
        assert!(
            (40.0..50.0).contains(&sim.form_pool),
            "half of ~93 kept: {}",
            sim.form_pool
        );
    }

    #[test]
    fn form_trait_records_fire_on_entry_exit_and_on_their_period() {
        let skills = vec![
            bar_skill(
                1,
                "Weapon Auto",
                SkillSlot::Weapon1,
                1,
                500,
                0,
                strike(1, 1.0),
            ),
            bar_skill(
                2,
                "Form Auto",
                SkillSlot::Weapon1,
                crate::rotation::SHROUD_SET,
                500,
                0,
                strike(1, 0.5),
            ),
        ];
        let buff = |name: &str, stacks: u32, duration_ms: u32| FormProc::Buff {
            name: name.into(),
            stacks,
            duration_ms,
            ally: false,
        };
        let mut form = test_form(100.0);
        form.on_enter = vec![buff("Might", 5, 5_000)];
        form.on_exit = vec![buff("Fury", 1, 4_000), FormProc::Gain(20.0)];
        form.periodic = vec![(3_000, buff("Quickness", 1, 3_000))];
        let sim = run_form_sim(&skills, 20_000, form);
        let result = sim.into_result();
        // Might 5 stacks for 5 s of 20 s; Quickness every 3 s through the
        // 10 s form (0, 3, 6, 9 s: covered to 12 s); Fury 4 s after exit.
        assert!((result.might_stacks_avg - 5.0 * 5.0 / 20.0).abs() < 0.05);
        assert!((result.buff_uptime["Quickness"] - 12.0 / 20.0).abs() < 0.01);
        assert!((result.buff_uptime["Fury"] - 4.0 / 20.0).abs() < 0.01);
    }

    /// `OnSkillUse` / `OnConditionApplied` records with a form: a
    /// `Shroud_1` scope admits the form bar's slot 1 only (not the weapon
    /// auto in the same slot), a Fear scope fires on a landed fear, and the
    /// `in_shroud` prerequisite and the internal cooldown hold.
    #[test]
    fn triggered_trait_records_fire_on_their_scope_only() {
        use crate::data::normalized_effects::TriggerScope;
        let buff = |name: &str, duration_ms: u32| FormProc::Buff {
            name: name.into(),
            stacks: 1,
            duration_ms,
            ally: false,
        };
        let mut weapon_auto = bar_skill(
            1,
            "Weapon Auto",
            SkillSlot::Weapon1,
            1,
            500,
            0,
            strike(1, 1.0),
        );
        weapon_auto.slot_name = Some("Weapon_1".into());
        let mut form_auto = bar_skill(
            2,
            "Form Auto",
            SkillSlot::Weapon1,
            crate::rotation::SHROUD_SET,
            500,
            0,
            strike(1, 0.5),
        );
        form_auto.slot_name = Some("Weapon_1".into());
        let mut fear = strike(1, 10.0);
        fear.push(SkillEffect::CrowdControl {
            kind: crate::rotation::ControlKind::Fear,
            duration_ms: 1_000,
            stops_dodge: true,
        });
        let fear_skill = bar_skill(3, "Fear", SkillSlot::Utility, 0, 500, 30_000, fear);
        let skills = vec![weapon_auto, form_auto, fear_skill];
        let triggered = |on, in_form, proc_| TriggeredProc {
            on,
            icd_ms: 1_000,
            in_form,
            weapon_set: 0,
            self_boons: Vec::new(),
            proc_,
        };
        let records = vec![
            triggered(
                ProcTrigger::SkillUse(TriggerScope::Slot("Shroud_1".into())),
                None,
                buff("Might", 15_000),
            ),
            triggered(
                ProcTrigger::ConditionApplied(Some("Fear".into())),
                None,
                buff("Quickness", 5_000),
            ),
            triggered(
                ProcTrigger::ConditionApplied(Some("Fear".into())),
                Some(true),
                buff("Fury", 5_000),
            ),
            triggered(
                ProcTrigger::ConditionApplied(Some("Chilled".into())),
                None,
                buff("Protection", 5_000),
            ),
        ];
        let run = |initial_pool| {
            run_sim(
                &skills,
                20_000,
                Some(test_form(initial_pool)),
                records.clone(),
            )
        };

        // Never enters the form: the weapon auto is not shroud skill 1 and
        // the in-form Fury stays shut; the one fear gives 5 s of Quickness.
        let result = run(0.0).into_result();
        assert_eq!(result.might_stacks_avg, 0.0);
        assert!((result.buff_uptime["Quickness"] - 5.0 / 20.0).abs() < 0.01);
        assert!(!result.buff_uptime.contains_key("Fury"));
        assert!(!result.buff_uptime.contains_key("Protection"));

        // Full pool: the form's auto fires Might (one stack per second at
        // the 1 s cooldown), and the fear cast in the form opens Fury.
        let result = run(100.0).into_result();
        assert!(result.might_stacks_avg > 1.0, "{}", result.might_stacks_avg);
        assert!((result.buff_uptime["Fury"] - 5.0 / 20.0).abs() < 0.01);
    }

    fn auto_only() -> Vec<RotationSkill> {
        vec![bar_skill(
            1,
            "Weapon Auto",
            SkillSlot::Weapon1,
            1,
            500,
            0,
            strike(1, 1.0),
        )]
    }

    fn every(interval_ms: u32, in_form: Option<bool>, proc_: FormProc) -> TriggeredProc {
        TriggeredProc {
            on: ProcTrigger::Periodic,
            icd_ms: interval_ms,
            in_form,
            weapon_set: 0,
            self_boons: Vec::new(),
            proc_,
        }
    }

    fn quickness(duration_ms: u32) -> FormProc {
        FormProc::Buff {
            name: "Quickness".into(),
            stacks: 1,
            duration_ms,
            ally: false,
        }
    }

    /// "Feel My Wrath!" (wiki: the quickness you grant yourself is doubled):
    /// the skill's 3 s ally grant plus its own-cast record's 3 s self grant
    /// run back to back, because Quickness stacks in duration.
    #[test]
    fn an_own_cast_record_doubles_self_quickness() {
        let mut shout = bar_skill(
            7,
            "Shout",
            SkillSlot::Elite,
            0,
            250,
            30_000,
            vec![SkillEffect::ApplyBuff {
                buff: "Quickness".into(),
                stacks: 1,
                duration_ms: 3_000,
            }],
        );
        shout.reaches_allies = true;
        let skills = [auto_only(), vec![shout]].concat();
        let record = |id| TriggeredProc {
            on: ProcTrigger::OwnCast(id),
            icd_ms: 0,
            in_form: None,
            weapon_set: 0,
            self_boons: Vec::new(),
            proc_: quickness(3_000),
        };
        let plain = run_sim(&skills, 30_000, None, Vec::new());
        assert_eq!(casts(&plain, 7), 1);
        let plain = plain.into_result().buff_uptime["Quickness"];
        assert!((plain - 3.0 / 30.0).abs() < 0.01, "{plain}");
        let doubled = run_sim(&skills, 30_000, None, vec![record(7)]).into_result();
        let doubled = doubled.buff_uptime["Quickness"];
        assert!((doubled - 6.0 / 30.0).abs() < 0.01, "{doubled}");
        // Another skill's cast does not fire it.
        let other = run_sim(&skills, 30_000, None, vec![record(1)]).into_result();
        assert!(
            other.buff_uptime["Quickness"] > 0.5,
            "the auto's own record fires"
        );
    }

    /// Sigil of Rage shape: on crit, 20 s cooldown, not while the player
    /// has Quickness, live only on its weapon set. The payload here is Fury
    /// so the gate's Quickness can come from elsewhere.
    #[test]
    fn an_on_crit_record_fires_after_its_cooldown_only_when_the_gate_holds() {
        let rage = |weapon_set, crit_bonus: f64, extra: Vec<TriggeredProc>| {
            let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
            params.precision = 1_000.0;
            params.crit_chance_bonus = crit_bonus;
            params.fury_crit_chance_bonus = 0.0;
            params.triggered = [
                vec![TriggeredProc {
                    on: ProcTrigger::Crit,
                    icd_ms: 20_000,
                    in_form: None,
                    weapon_set,
                    self_boons: vec![("Quickness".into(), false)],
                    proc_: FormProc::Buff {
                        name: "Fury".into(),
                        stacks: 1,
                        duration_ms: 3_000,
                        ally: false,
                    },
                }],
                extra,
            ]
            .concat();
            let mut sim = SimState::new(
                &auto_only(),
                60_000,
                TargetState::from_seed(EnemyDummy::open()),
                params,
            );
            sim.run();
            sim.into_result()
                .buff_uptime
                .get("Fury")
                .copied()
                .unwrap_or(0.0)
        };
        // Certain crits: fires on the first hit, then each 20 s, 3 s each.
        let fury = rage(1, 100.0, Vec::new());
        assert!((fury - 9.0 / 60.0).abs() < 0.01, "{fury}");
        // Averaged crits: the mass takes a few hits after each cooldown, so
        // the same three firings, never more.
        let fury = rage(1, 0.0, Vec::new());
        assert!(fury > 0.0 && fury <= 9.0 / 60.0 + 1e-9, "{fury}");
        // Quickness always up: the gate stays shut.
        let fury = rage(1, 100.0, vec![every(1_000, None, quickness(2_000))]);
        assert_eq!(fury, 0.0);
        // Socketed on the set the player never holds: never live.
        let fury = rage(2, 100.0, Vec::new());
        assert_eq!(fury, 0.0);
    }

    /// E15: event records ride on `SimParams`, so a build with no form fires
    /// them; one gated on the form (`in_shroud`) never fires without one.
    #[test]
    fn skill_use_records_fire_without_a_form_and_form_gated_ones_do_not() {
        use crate::data::normalized_effects::TriggerScope;
        let on_auto = |in_form| TriggeredProc {
            on: ProcTrigger::SkillUse(TriggerScope::Any),
            icd_ms: 10_000,
            in_form,
            weapon_set: 0,
            self_boons: Vec::new(),
            proc_: quickness(5_000),
        };
        // Casts at 0 s and 10 s (the internal cooldown): 10 s of 20 s.
        let result = run_sim(&auto_only(), 20_000, None, vec![on_auto(None)]).into_result();
        assert!((result.buff_uptime["Quickness"] - 0.5).abs() < 0.01);
        let result = run_sim(&auto_only(), 20_000, None, vec![on_auto(Some(true))]).into_result();
        assert!(!result.buff_uptime.contains_key("Quickness"));
    }

    /// E16: a timed strike modifier multiplies only the hits that land
    /// inside its window; an additive one joins the bucket already in
    /// `strike_mult`; stacks add up to the record's cap.
    #[test]
    fn a_timed_strike_modifier_raises_damage_only_in_its_window() {
        let modifier = |percent, additive, max_stacks| FormProc::Modifier {
            source: "Test Modifier".into(),
            modifier: DamageMod {
                axis: ModAxis::Strike,
                percent,
                additive,
            },
            duration_ms: 2_000,
            max_stacks,
            refresh_all: false,
        };
        let seconds = |triggered: Vec<TriggeredProc>, strike_add: f64| {
            let mut params = SimParams::basic(2_000.0, 0.0, 1_000.0);
            params.triggered = triggered;
            params.strike_add = strike_add;
            params.strike_mult = 1.0 + strike_add;
            let mut sim = SimState::new(
                &auto_only(),
                20_000,
                TargetState::from_seed(EnemyDummy::open()),
                params,
            );
            sim.run();
            sim.into_result().damage_per_second
        };
        let base = seconds(Vec::new(), 0.0);
        // Fires at 0 s and 10 s, 2 s each.
        let boosted = seconds(vec![every(10_000, None, modifier(20.0, false, 1))], 0.0);
        for (k, (b, w)) in base.iter().zip(&boosted).enumerate() {
            let want = if k % 10 < 2 { 1.2 } else { 1.0 };
            assert!(
                (w.0 / b.0 - want).abs() < 1e-9,
                "second {k}: {} vs {}",
                w.0,
                b.0
            );
        }
        assert!(base.iter().all(|(strike, _)| *strike > 0.0));
        // Additive: (1 + 0.1 + 0.2) / (1 + 0.1) on a +10 % bucket.
        let bucket = seconds(Vec::new(), 0.1);
        let additive = seconds(vec![every(10_000, None, modifier(20.0, true, 1))], 0.1);
        assert!((additive[0].0 / bucket[0].0 - 1.3 / 1.1).abs() < 1e-9);
        assert!((additive[5].0 / bucket[5].0 - 1.0).abs() < 1e-9);
        // A stack every second, each 2 s, cap 3: two stacks from 1 s on.
        let stacked = seconds(vec![every(1_000, None, modifier(10.0, false, 3))], 0.0);
        assert!((stacked[5].0 / base[5].0 - 1.2).abs() < 1e-9);
    }

    /// E16: a condition proc applies its record's stacks and duration
    /// through the skill path, and the condition it applies fires the
    /// records that listen for it.
    #[test]
    fn a_condition_proc_applies_its_stacks() {
        let records = vec![
            every(
                60_000,
                None,
                FormProc::Condition {
                    name: "Bleeding".into(),
                    stacks: 3,
                    duration_ms: 5_000,
                },
            ),
            TriggeredProc {
                on: ProcTrigger::ConditionApplied(Some("Bleeding".into())),
                icd_ms: 0,
                in_form: None,
                weapon_set: 0,
                self_boons: Vec::new(),
                proc_: quickness(4_000),
            },
        ];
        let result = run_sim(&auto_only(), 20_000, None, records).into_result();
        // 3 stacks for 5 s of 20 s.
        assert!((result.condition_uptime["Bleeding"] - 0.75).abs() < 0.05);
        assert!(result.condition_dps > 0.0);
        assert!((result.buff_uptime["Quickness"] - 0.2).abs() < 0.01);
    }

    /// Reaper's Shroud with the shipped numbers: `data/formulas/shroud.json`
    /// (wiki `Life force`: pool 69 % of health; wiki `Reaper's Shroud`: 4 %
    /// per second in PvE, 10 s recharge on exit, read 2026-09-08). A full
    /// pool at 20,000 health is 13,800 life force and lasts 25 s. The bar
    /// is the API's (skills 29442, 30825, 30504, 30557 as the builder
    /// prepares them, Executioner's Scythe with its wiki 1.25 s activation
    /// and 30 s recharge); the weapon bar is an auto only, so nothing
    /// outranks the shroud and it runs until the pool is dry.
    #[test]
    fn reaper_fixture_pins_shroud_time_and_casts_to_the_wiki_numbers() {
        let table = crate::data::shroud::table();
        let row = table.row(30792).expect("Reaper's Shroud row");
        let cap = table.pool_for(20_000.0);
        let form = FormSpec {
            name: row.name.clone(),
            entry_skill_id: 30792,
            pool_cap: cap,
            initial_pool: cap,
            entry_floor: table.entry_floor_pct / 100.0 * cap,
            drain_per_second: row
                .drain_pct_per_s
                .as_ref()
                .unwrap()
                .for_mode(GameMode::PvE)
                / 100.0
                * cap,
            recharge_ms: (table.recharge_on_exit_s * 1_000.0) as u32,
            gains_in_form: true,
            exit_keep: 1.0,
            ..Default::default()
        };
        assert_eq!(cap, 13_800.0);
        assert_eq!(form.drain_per_second, 552.0);
        let skills = vec![
            bar_skill(
                29_421,
                "Greatsword Auto",
                SkillSlot::Weapon1,
                1,
                500,
                0,
                strike(1, 0.8),
            ),
            bar_skill(
                29_442,
                "Life Rend",
                SkillSlot::Weapon1,
                crate::rotation::SHROUD_SET,
                500,
                0,
                strike(1, 1.4),
            ),
            bar_skill(
                30_825,
                "Death's Charge",
                SkillSlot::Weapon2,
                crate::rotation::SHROUD_SET,
                250,
                6_000,
                vec![
                    SkillEffect::StrikeDamage {
                        hit_count: 9,
                        dmg_multiplier: 0.25,
                    },
                    SkillEffect::StrikeDamage {
                        hit_count: 1,
                        dmg_multiplier: 1.625,
                    },
                ],
            ),
            bar_skill(
                30_504,
                "Soul Spiral",
                SkillSlot::Weapon4,
                crate::rotation::SHROUD_SET,
                500,
                30_000,
                strike(12, 0.7),
            ),
            bar_skill(
                30_557,
                "Executioner's Scythe",
                SkillSlot::Weapon5,
                crate::rotation::SHROUD_SET,
                1_250,
                30_000,
                strike(1, 4.0),
            ),
        ];
        let sim = run_form_sim(&skills, 60_000, form);
        // One entry, 25 s in shroud, then no life force to come back with.
        assert_eq!(casts(&sim, 30_792), 1);
        assert!(
            (25_000..=25_100).contains(&sim.form_active_ms),
            "{}",
            sim.form_active_ms
        );
        // 30 s recharges: once each in the 25 s window. Death's Charge's
        // 6 s recharge (API) runs from each cast, and each comes up
        // mid-cast of something else (Soul Spiral first at 0 s, then it
        // at 0.7 s, 6.7+, 12.7+, 18.7+): the fifth would land past 25 s.
        assert_eq!(casts(&sim, 30_557), 1);
        assert_eq!(casts(&sim, 30_504), 1);
        assert_eq!(casts(&sim, 30_825), 4);
        assert!(casts(&sim, 29_442) > 20);
        assert!(casts(&sim, 29_421) > 40);
    }
}

/// Per-condition damage factors (`SimParams::condition_type_mults`).
#[cfg(test)]
mod condition_type_mult_tests {
    use super::*;
    use crate::rotation::combat_model::EnemyDummy;

    fn applier(condition: &str) -> RotationSkill {
        RotationSkill {
            targets: 1,
            categories: Vec::new(),
            slot_name: None,
            skill_id: 70,
            name: condition.into(),
            slot: SkillSlot::Weapon1,
            cast_time_ms: 1_000,
            cooldown_ms: 0,
            effects: vec![SkillEffect::ApplyCondition {
                condition: condition.into(),
                stacks: 1,
                duration_ms: 5_000,
            }],
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
        }
    }

    fn condition_damage(condition: &str, mults: &[(&str, f64)]) -> f64 {
        let mut params = SimParams::basic(1_000.0, 1_500.0, 1_000.0);
        params.condition_type_mults = mults.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let result = simulate_with(
            &[applier(condition)],
            30_000,
            &params,
            EnemyDummy::default(),
        );
        result.condition_dps
    }

    /// Wiki Hidden Barbs: +20% bleeding damage in PvE. The stat sheet's
    /// `Bleeding` factor 1.20 raises bleeding ticks by exactly that and
    /// leaves every other condition alone.
    #[test]
    fn hidden_barbs_raises_only_bleeding_ticks() {
        let barbs = [("Bleeding", 1.20)];
        let bleed = condition_damage("Bleeding", &[]);
        assert!(bleed > 0.0);
        let ratio = condition_damage("Bleeding", &barbs) / bleed;
        assert!((ratio - 1.20).abs() < 1e-9, "bleeding ratio {ratio}");
        let burn = condition_damage("Burning", &[]);
        assert!(burn > 0.0);
        assert_eq!(condition_damage("Burning", &barbs), burn);
    }
}
