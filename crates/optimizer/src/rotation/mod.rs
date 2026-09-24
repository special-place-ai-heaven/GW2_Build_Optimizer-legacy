//! Rotation simulator for GW2 builds.
//! Simulates a time-step skill rotation to estimate real DPS, condition uptime,
//! and buff uptime — validating AI build reasoning with concrete numbers.

pub mod attunement;
pub mod builder;
pub mod combat_model;
pub mod combo;
pub mod illusion;
#[cfg(test)]
pub(crate) mod necro_published;
pub mod prose;
#[cfg(test)]
pub(crate) mod reaper_fixture;
pub mod simulator;
pub mod skill_timings;
pub mod trait_skill;
pub mod trigger_bus;
pub mod wvw_timeline;

pub use attunement::{apply_attunement_skill, AttunementState, Element};
pub use combo::{ComboEngine, ComboOutcome, ComboOutcomeEffect, ComboSite, SELF_COMBATANT_ID};
pub use illusion::{spawn_clone, IllusionState, CLONE_CAP};
pub use trait_skill::resolve_trait_skill;
pub use trigger_bus::{
    land_foe_disable, BusEvent, DodgeAction, EndurancePool, TriggerBus, DODGE_COST,
};
pub use wvw_timeline::WvwCombatReport;

use std::collections::HashMap;

/// A skill prepared for rotation simulation, with all timing and effect data extracted.
#[derive(Debug, Clone)]
pub struct RotationSkill {
    pub skill_id: u32,
    pub name: String,
    pub slot: SkillSlot,
    /// Cast time in milliseconds (animation lock).
    pub cast_time_ms: u32,
    /// Cooldown in milliseconds.
    pub cooldown_ms: u32,
    /// All effects this skill applies.
    pub effects: Vec<SkillEffect>,
    /// Next skill in auto-attack chain (if any).
    pub next_chain: Option<u32>,
    /// Whether this skill is a stunbreak.
    pub is_stunbreak: bool,
    /// The skill reaches ALLIES, not just its owner: the API publishes a
    /// "Number of Allied Targets", "Allied Healing", "Allied Heal per
    /// Pulse" or "Allied Target Radius" fact for it (147 skills do).
    /// Healing and boons from a skill that does not are self-facing, which
    /// is survival rather than support. Categories are deliberately NOT
    /// used: `"Nothing Can Save You!"` is a shout aimed at foes.
    pub reaches_allies: bool,
    /// Weapon set this skill belongs to (0=always available, 1=set1, 2=set2,
    /// [`SHROUD_SET`]=only while in shroud). Non-weapon skills
    /// (heal/utility/elite) use 0.
    pub weapon_set: u8,
    // Sprint 3 (specs/007-trait-triggers): trait-owned skill-use scopes
    /// API `Skill.categories` (`Shout`, `Well`, `Signet`, ...).
    pub categories: Vec<String>,
    /// API `Skill.slot` as published (`Heal`, `Utility`, `Elite`,
    /// `Profession_1`, `Weapon_1`, ...).
    pub slot_name: Option<String>,
    /// API `Number of Targets` fact, 1 when absent (fight population, FR-003a).
    pub targets: u32,
}

/// The Necromancer shroud bar as a third "weapon set": its skills are held
/// only while in shroud, and sets 1 and 2 are stowed meanwhile (wiki
/// `Death Shroud`: the shroud skills replace the weapon bar).
// ponytail: a `Bar` enum would touch eighteen `RotationSkill` literals for
// the same three states; `weapon_set == 3` reuses the availability check.
pub const SHROUD_SET: u8 = 3;

impl RotationSkill {
    /// Slot 1 with no recharge: the auto-attack (or a step of its chain).
    pub fn is_auto_attack(&self) -> bool {
        self.slot == SkillSlot::Weapon1 && self.cooldown_ms == 0
    }
}

/// How long after a chain step's activation ends the next step may still
/// start. Wiki `Chain` gives no figure: a chain continues when each step is
/// auto-activated right after the previous one, and resets on an interrupt
/// or when another weapon skill is used mid-sequence. "Right after" is the
/// simulators' own reaction time (human delay + skill gap) plus one flow
/// tick; any longer idle resets the chain to its first step.
pub const CHAIN_CONTINUE_SLACK_MS: u32 =
    skill_timings::HUMAN_DELAY_MS + skill_timings::MIN_SKILL_GAP_MS + 100;

/// Auto-attack chain cursor (E11), shared by the flow simulation and the
/// WvW timeline. The bar keeps one slot-1 head; the follow-up steps sit in
/// the skill list and are reached only through [`AutoChain::step`], driven
/// by each skill's API `next_chain`.
#[derive(Debug, Clone, Default)]
pub struct AutoChain {
    /// Per skill: index of its `next_chain` step in the list.
    next: Vec<Option<usize>>,
    /// Per skill: its chain's first step (itself when unchained).
    head: Vec<usize>,
    /// The step the next auto cast plays, and the last moment it may start.
    cursor: Option<(usize, u32)>,
}

impl AutoChain {
    pub fn new(skills: &[RotationSkill]) -> Self {
        let n = skills.len();
        let next: Vec<Option<usize>> = skills
            .iter()
            .map(|s| {
                let id = s.next_chain?;
                skills.iter().position(|t| t.skill_id == id)
            })
            .collect();
        let mut targeted = vec![false; n];
        for j in next.iter().flatten() {
            targeted[*j] = true;
        }
        let mut head: Vec<usize> = (0..n).collect();
        for root in (0..n).filter(|i| !targeted[*i]) {
            let mut step = next[root];
            let mut hops = 0;
            while let Some(k) = step {
                if k == root || hops > n {
                    break;
                }
                head[k] = root;
                step = next[k];
                hops += 1;
            }
        }
        Self {
            next,
            head,
            cursor: None,
        }
    }

    /// A chain step after the first: never a filler candidate on its own.
    pub fn is_follow_up(&self, idx: usize) -> bool {
        self.head.get(idx).is_some_and(|head| *head != idx)
    }

    /// The step the auto on chain `head` plays at `now_ms`.
    pub fn step(&self, head: usize, now_ms: u32) -> usize {
        match self.cursor {
            Some((step, deadline)) if now_ms <= deadline && self.head[step] == head => step,
            _ => head,
        }
    }

    /// After a cast ending at `end_ms`: an auto advances its chain, any
    /// other skill resets it (wiki `Chain`).
    // ponytail: every non-auto cast resets; the wiki spares some quick
    // utilities, add a per-skill flag if a log shows one.
    pub fn on_cast(&mut self, idx: usize, is_auto: bool, end_ms: u32) {
        self.cursor = if is_auto {
            self.next[idx].map(|step| (step, end_ms.saturating_add(CHAIN_CONTINUE_SLACK_MS)))
        } else {
            None
        };
    }

    /// Interrupted (wiki `Chain`): back to the first step.
    pub fn reset(&mut self) {
        self.cursor = None;
    }
}

/// Auto chains whose next step is not in the skill list (missing from the
/// skill data), named for the gap line (doctrine rule 6).
pub fn missing_chain_steps(skills: &[RotationSkill]) -> Vec<String> {
    skills
        .iter()
        .filter(|s| s.is_auto_attack())
        .filter_map(|s| {
            let id = s.next_chain?;
            (!skills.iter().any(|t| t.skill_id == id))
                .then(|| format!("{} auto chain (step {id} missing)", s.name))
        })
        .collect()
}

/// Skill slot classification — determines priority and auto-attack behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSlot {
    Weapon1,
    Weapon2,
    Weapon3,
    Weapon4,
    Weapon5,
    Heal,
    Utility,
    Elite,
    Profession,
}

impl SkillSlot {
    /// Parse from GW2 API slot string.
    pub fn from_api(slot: &str) -> Option<Self> {
        match slot {
            "Weapon_1" => Some(Self::Weapon1),
            "Weapon_2" => Some(Self::Weapon2),
            "Weapon_3" => Some(Self::Weapon3),
            "Weapon_4" => Some(Self::Weapon4),
            "Weapon_5" => Some(Self::Weapon5),
            "Heal" => Some(Self::Heal),
            "Utility" => Some(Self::Utility),
            "Elite" => Some(Self::Elite),
            s if s.starts_with("Profession_") => Some(Self::Profession),
            _ => None,
        }
    }

    /// Is this a weapon skill (auto-attackable)?
    pub fn is_weapon(&self) -> bool {
        matches!(
            self,
            Self::Weapon1 | Self::Weapon2 | Self::Weapon3 | Self::Weapon4 | Self::Weapon5
        )
    }
}

/// Hard control vs interrupt-only. `stops_dodge` is the lock/not-lock split:
/// Daze interrupts casts but does not stop dodges; Immobilize is a condition
/// that does stop dodges (wiki Control effect / Dodge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    Stun,
    Knockdown,
    Launch,
    Knockback,
    Pull,
    Fear,
    Taunt,
    Daze,
    Float,
    Sink,
    Immobilize,
}

/// Cover that can eat the alpha answer. Boons are strippable; true invuln is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverKind {
    Invulnerability,
    /// Active evade frame. Unlike Block, this also avoids unblockable attacks.
    Evade,
    Stealth,
    Aegis,
    Stability,
    /// Suppresses nondamaging condition *effects* (Immobile, Fear, Taunt, …).
    /// Condi damage still ticks. Poison heal-reduction and Terror are exempt.
    /// Corrupts into Chill (wiki Resistance, fetched 2026-08-14).
    Resistance,
    Protection,
    Blind,
    Block,
}

/// Roam "out" — at least one of these is the mobility gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobilityKind {
    Teleport,
    Stealth,
    Superspeed,
    Leap,
    Evade,
}

/// An effect that a skill produces when used.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillEffect {
    /// Direct strike damage: `hit_count` strikes of `dmg_multiplier` each
    /// (the API coefficient is per strike; total = coefficient x hit_count).
    StrikeDamage {
        hit_count: u32,
        dmg_multiplier: f64,
    },
    /// Applies a damaging condition to the target.
    ApplyCondition {
        condition: String,
        stacks: u32,
        duration_ms: u32,
    },
    /// Applies a boon or self-buff.
    ApplyBuff {
        buff: String,
        stacks: u32,
        duration_ms: u32,
    },
    /// Combo field placement.
    ComboField {
        field_type: String,
        duration_ms: u32,
    },
    /// Combo finisher. The active field and finisher type determine the result.
    ComboFinisher {
        finisher_type: String,
        percent: u32,
    },
    /// Direct self-healing. GW2's public API exposes the hit count but not every
    /// coefficient, so the timeline uses a conservative healing-power model.
    Healing {
        hit_count: u32,
    },
    /// Barrier applied to self. `amount` is a conservative resolved estimate.
    Barrier {
        amount: f64,
    },
    /// Removes one or more conditions from self (cleanse).
    /// `conditions_removed` is the number of conditions removed per use.
    RemovesCondition {
        conditions_removed: u32,
    },
    /// Crowd control. `stops_dodge` false = interrupt only (Daze).
    CrowdControl {
        kind: ControlKind,
        duration_ms: u32,
        stops_dodge: bool,
    },
    /// Boon strip. Pair Number "Boons Removed" with Time Interval/Pulse/Duration.
    /// WoD: count_per_pulse=1, interval_ms=1000, window_ms=5000 → 5 strips, not 1.
    StripBoons {
        count_per_pulse: u32,
        interval_ms: u32,
        window_ms: u32,
    },
    /// Boon → condition. Often missing from API facts; parsed from description.
    CorruptBoons,
    /// Condition → boon (convert cleanse).
    ConvertConditions,
    /// Boon steal / transfer to self.
    StealBoons,
    Cover {
        kind: CoverKind,
        duration_ms: u32,
        strippable: bool,
    },
    Mobility {
        kind: MobilityKind,
    },
}

/// Total boons removed over a strip effect's window (not the per-pulse Number).
pub fn strip_total(effect: &SkillEffect) -> u32 {
    match effect {
        SkillEffect::StripBoons {
            count_per_pulse,
            interval_ms,
            window_ms,
        } => {
            if *interval_ms == 0 {
                *count_per_pulse
            } else {
                let window = (*window_ms).max(*interval_ms);
                *count_per_pulse * (window / *interval_ms)
            }
        }
        _ => 0,
    }
}

/// Full simulation result from running a rotation.
///
/// Design philosophy: Pure damage output is NOT the only goal.
/// Build quality = ability to DELIVER damage (DPS * uptime * control).
/// A build that can CC the enemy, maintain stability, and survive
/// delivers more REAL damage than a glass cannon that gets interrupted.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Duration of the simulation in milliseconds.
    pub duration_ms: u32,
    /// Estimated strike DPS (direct damage per second).
    pub strike_dps: f64,
    /// Estimated condition DPS from tick damage.
    pub condition_dps: f64,
    /// Total DPS (strike + condition).
    pub total_dps: f64,
    /// Average condition stacks over the simulation.
    pub condition_uptime: HashMap<String, f64>,
    /// Boon uptime as fraction (0.0 to 1.0).
    pub buff_uptime: HashMap<String, f64>,
    /// Per-skill usage: (name, cast_count, dps_contribution).
    pub skill_usage: Vec<SkillUsage>,
    /// Number of stunbreak skills available in the build.
    pub stunbreak_count: u32,
    /// Whether the build has access to Stability (from skills or traits).
    pub has_stability: bool,
    /// Estimated self-Stability uptime (fraction 0.0 to 1.0).
    pub stability_uptime: f64,
    /// Number of equipped skills that have at least one cleanse effect.
    pub cleanse_count: u32,
    /// Estimated conditions removed per 20 seconds (sum of conditions_removed × uptime_factor).
    pub cleanse_rate_per_20s: f64,
    /// Self-healing plus barrier per second over the window, from the same
    /// conservative healing-power model the WvW timeline uses. Stays zero
    /// unless the scheduler had a reason to cast heals (`SimParams::intent`).
    pub healing_per_second: f64,
    /// Control-seconds per second: hard CC as non-overlapping disabled time
    /// (at most 1.0) plus 0.5 per distinct soft control condition present
    /// (chill, cripple, weakness, slow, blind). Can exceed 1.0; the scoring
    /// norm (`REALIZED_CONTROL_NORM`) is set accordingly.
    pub control_uptime: f64,
    /// Time-averaged Might stacks (0..=25). `buff_uptime["Might"]` is capped
    /// at 1.0 and cannot tell 3 stacks from 25.
    pub might_stacks_avg: f64,
    /// Boon-equivalents at full uptime: every boon's presence uptime times
    /// its worth (Quickness/Alacrity/Fury/Protection/Stability 1.0, Aegis
    /// 0.75, Regeneration/Resolution/Vigor/Resistance 0.5, Swiftness 0.25),
    /// with Might counted as `might_stacks_avg / 25`.
    pub boon_equivalents: f64,
    /// Kit has stealth, evade, block, invuln/aegis, or mobility to disengage a group.
    pub has_mobility_out: bool,
    /// Distinct roam-out categories (mobility, stealth, block, invuln/aegis).
    pub escape_kinds: u32,
    /// Kit has strip, steal, or corrupt (harasser cover-crack gate).
    pub has_strip: bool,
    /// Kit converts enemy boons to conditions (disabler identity).
    pub has_corrupt: bool,
    /// Dummy HP hit 0 during the window (downed, not yet stomped).
    pub downed: bool,
    /// Downed and the ~3.5s interruptible stomp finished inside the window.
    pub finished: bool,
    /// Kit has any CrowdControl (secures stomp / cuts casts).
    pub has_interrupt: bool,
    /// Personal cover vs incoming CC: stab, evade, block, invuln, stealth, or blind.
    pub has_cover_answer: bool,
    /// Counterplay-aware WvW execution report. Present only for WvW scenarios.
    pub wvw: Option<WvwCombatReport>,
    /// Measurement capture, one entry per second `[k, k+1)` of the window:
    /// (strike, condition) damage landed in it. Nothing schedules on it.
    pub damage_per_second: Vec<(f64, f64)>,
    /// Buff name -> present at each second's midpoint tick, indexed like
    /// `damage_per_second`. Only names that were ever active, as
    /// `buff_uptime`.
    pub buff_presence_per_second: HashMap<String, Vec<bool>>,
}

/// Per-skill breakdown in a simulation result.
#[derive(Debug, Clone)]
pub struct SkillUsage {
    pub name: String,
    pub cast_count: u32,
    pub dps_contribution: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_slot_from_api() {
        assert_eq!(SkillSlot::from_api("Weapon_1"), Some(SkillSlot::Weapon1));
        assert_eq!(SkillSlot::from_api("Heal"), Some(SkillSlot::Heal));
        assert_eq!(
            SkillSlot::from_api("Profession_1"),
            Some(SkillSlot::Profession)
        );
        assert_eq!(SkillSlot::from_api("Unknown"), None);
    }

    #[test]
    fn test_skill_slot_is_weapon() {
        assert!(SkillSlot::Weapon1.is_weapon());
        assert!(SkillSlot::Weapon5.is_weapon());
        assert!(!SkillSlot::Heal.is_weapon());
        assert!(!SkillSlot::Elite.is_weapon());
    }
}
