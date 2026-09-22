//! Fight-dummy clocks and kit gates used by the rotation scorer.
//! Mapping lives in `builder`; this module is the scorer contract.

use std::borrow::Cow;

use crate::scenario::{CombatKind, CombatTier};
use gw2_core::types::GameMode;

use super::{CoverKind, MobilityKind, RotationSkill, SkillEffect};

/// Simulation window 0–T in milliseconds for (scale × kind).
/// Zerg T=3s / 6–8s are derived (not log-measured).
/// WvW needs enough wall-clock for a protected opener and the enemy's answer.
/// A two-second spike remains a reported sub-window, but the exchange itself is
/// never truncated at the instant the minimum chain completes.
pub fn simulation_window_ms_for_mode(mode: &GameMode, tier: CombatTier, kind: CombatKind) -> u32 {
    if *mode != GameMode::WvW {
        return match (tier, kind) {
            (CombatTier::Solo, CombatKind::CondiRamp) => 5_000,
            (
                CombatTier::Solo,
                CombatKind::Commander | CombatKind::Support | CombatKind::Staller,
            ) => 10_000,
            (CombatTier::Solo, _) => 2_000,
            (CombatTier::Party, CombatKind::CondiRamp) => 5_000,
            (
                CombatTier::Party,
                CombatKind::Commander | CombatKind::Support | CombatKind::Staller,
            ) => 10_000,
            (CombatTier::Party, _) => 2_500,
            (CombatTier::Squad, CombatKind::StrikeSpike) => 3_000,
            (CombatTier::Squad, CombatKind::CondiRamp) => 7_000,
            (CombatTier::Squad, CombatKind::Harasser) => 2_500,
            (CombatTier::Squad, CombatKind::Disabler) => 7_000,
            (
                CombatTier::Squad,
                CombatKind::Commander | CombatKind::Support | CombatKind::Staller,
            ) => 10_000,
        };
    }
    match (tier, kind) {
        (CombatTier::Solo, CombatKind::StrikeSpike) => 5_000,
        (CombatTier::Solo, CombatKind::CondiRamp) => 20_000,
        (CombatTier::Solo, CombatKind::Harasser | CombatKind::Disabler) => 10_000,
        (CombatTier::Solo, CombatKind::Commander | CombatKind::Support | CombatKind::Staller) => {
            20_000
        }
        (CombatTier::Party, CombatKind::StrikeSpike) => 5_000,
        (CombatTier::Party, CombatKind::CondiRamp) => 20_000,
        (CombatTier::Party, CombatKind::Harasser | CombatKind::Disabler) => 10_000,
        (CombatTier::Party, CombatKind::Commander | CombatKind::Support | CombatKind::Staller) => {
            20_000
        }
        (CombatTier::Squad, CombatKind::StrikeSpike) => 5_000,
        (CombatTier::Squad, CombatKind::CondiRamp) => 20_000,
        (CombatTier::Squad, CombatKind::Harasser | CombatKind::Disabler) => 10_000,
        (CombatTier::Squad, CombatKind::Commander | CombatKind::Support | CombatKind::Staller) => {
            20_000
        }
    }
}

/// WvW exchange window retained for callers that do not carry game mode.
pub fn simulation_window_ms(tier: CombatTier, kind: CombatKind) -> u32 {
    simulation_window_ms_for_mode(&GameMode::WvW, tier, kind)
}

/// Prefer CC/strip/cover over DPCT for this long at the start of a short clock.
pub fn setup_window_ms_for_mode(duration_ms: u32, wvw: bool) -> u32 {
    if wvw || duration_ms <= 10_000 {
        2_000.min(duration_ms)
    } else {
        0
    }
}

pub fn setup_window_ms(duration_ms: u32) -> u32 {
    setup_window_ms_for_mode(duration_ms, true)
}

pub fn kit_has_mobility_out(skills: &[RotationSkill]) -> bool {
    kit_escape_kinds(skills) > 0
}

/// Distinct roam-out categories: mobility, stealth, block, invuln/aegis.
pub fn kit_escape_kinds(skills: &[RotationSkill]) -> u32 {
    let mut mobility = false;
    let mut stealth = false;
    let mut block = false;
    let mut cover = false;
    for skill in skills {
        for e in &skill.effects {
            match e {
                SkillEffect::Mobility {
                    kind: MobilityKind::Stealth,
                } => stealth = true,
                SkillEffect::Mobility { .. } => mobility = true,
                SkillEffect::Cover {
                    kind: CoverKind::Stealth,
                    ..
                } => stealth = true,
                SkillEffect::Cover {
                    kind: CoverKind::Block,
                    ..
                } => block = true,
                SkillEffect::Cover {
                    kind: CoverKind::Invulnerability | CoverKind::Aegis | CoverKind::Evade,
                    ..
                } => cover = true,
                _ => {}
            }
        }
    }
    u32::from(mobility) + u32::from(stealth) + u32::from(block) + u32::from(cover)
}

pub fn kit_has_strip(skills: &[RotationSkill]) -> bool {
    skills.iter().any(|s| {
        s.effects.iter().any(|e| {
            matches!(
                e,
                SkillEffect::StripBoons { .. }
                    | SkillEffect::StealBoons
                    | SkillEffect::CorruptBoons
            )
        })
    })
}

pub fn kit_has_corrupt(skills: &[RotationSkill]) -> bool {
    skills.iter().any(|s| {
        s.effects
            .iter()
            .any(|e| matches!(e, SkillEffect::CorruptBoons))
    })
}

pub fn kit_has_interrupt(skills: &[RotationSkill]) -> bool {
    skills.iter().any(|s| {
        s.effects
            .iter()
            .any(|e| matches!(e, SkillEffect::CrowdControl { .. }))
    })
}

pub fn kit_has_stability_cover(skills: &[RotationSkill]) -> bool {
    skills.iter().any(|s| {
        s.effects.iter().any(|e| match e {
            SkillEffect::Cover {
                kind: CoverKind::Stability,
                ..
            } => true,
            SkillEffect::ApplyBuff { buff, .. } => buff == "Stability",
            _ => false,
        })
    })
}

/// Cover that eats the incoming alpha: Stability, evade, block, invuln/aegis, stealth, or blind.
/// Leap/teleport is an *out*, not cover — that is `kit_has_mobility_out`.
pub fn kit_has_cover_answer(skills: &[RotationSkill]) -> bool {
    kit_has_stability_cover(skills)
        || skills.iter().any(|s| {
            s.effects.iter().any(|e| {
                matches!(
                    e,
                    SkillEffect::Mobility {
                        kind: MobilityKind::Evade | MobilityKind::Stealth,
                    } | SkillEffect::Cover {
                        kind: CoverKind::Stealth
                            | CoverKind::Evade
                            | CoverKind::Block
                            | CoverKind::Invulnerability
                            | CoverKind::Aegis
                            | CoverKind::Blind,
                        ..
                    }
                )
            })
        })
}

/// Which of the three BuffProfile slots (Solo / Party / Squad) this scale uses.
pub fn buff_profile_index(tier: CombatTier) -> usize {
    match tier {
        CombatTier::Solo => 0,
        CombatTier::Party => 1,
        CombatTier::Squad => 2,
    }
}

/// Nondamaging condition effects Resistance suppresses. Poison heal-reduction
/// and Terror damage are explicitly not negated (wiki Resistance, 2026-08-14).
pub fn resistance_negates(status: &str) -> bool {
    matches!(
        status,
        "Blind"
            | "Blinded"
            | "Chill"
            | "Chilled"
            | "Cripple"
            | "Crippled"
            | "Fear"
            | "Immobile"
            | "Immobilize"
            | "Immobilized"
            | "Slow"
            | "Taunt"
            | "Vulnerability"
            | "Weakness"
    )
}

/// Boon → leftover condition when corrupted.
/// Protection/Resolution/Vigor/Aegis rows from wiki Boon table (local fetch 2026-08-14);
/// Resistance → Chilled from wiki Resistance version history (same fetch);
/// remaining rows are the stable wiki Boon "Converted into" mapping.
pub fn corrupt_into(boon: &str) -> Option<&'static str> {
    Some(match boon {
        "Aegis" => "Burning",
        "Alacrity" => "Chilled",
        "Fury" => "Blinded",
        "Might" => "Weakness",
        "Protection" => "Vulnerability",
        "Quickness" => "Slow",
        "Regeneration" => "Poisoned",
        "Resistance" => "Chilled",
        "Resolution" => "Confusion",
        "Stability" => "Fear",
        "Swiftness" => "Crippled",
        "Vigor" => "Bleeding",
        _ => return None,
    })
}

/// Glass roam pick / havoc bruiser. Support and large-scale pressure have no solo outcome target.
pub fn dummy_hp(tier: CombatTier, kind: CombatKind) -> Option<f64> {
    if matches!(
        kind,
        CombatKind::Support | CombatKind::Commander | CombatKind::Staller
    ) {
        return None;
    }
    match (tier, kind) {
        (CombatTier::Solo, _) => Some(13_000.0),
        (CombatTier::Party, _) => Some(20_000.0),
        (CombatTier::Squad, CombatKind::Harasser) => Some(13_000.0),
        (CombatTier::Squad, _) => None,
    }
}

/// Enemy cover the dummy starts with. Strip/steal/corrupt clear it for the rest of the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EnemyDummy {
    pub protection: bool,
    pub stability: bool,
    /// `None` = open dummy (no encounter-outcome tracking).
    pub hp: Option<f64>,
}

/// One timed condition on the primary foe.
///
/// Shared by the flow simulator and the WvW timeline (Phase 3): one foe-condition
/// ledger, not competing `SimState.conditions` vs `outgoing_conditions`.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedFoeCondition {
    /// Canonical name; borrowed from the intern table for known conditions.
    pub name: Cow<'static, str>,
    pub stacks: u32,
    pub expires_at_ms: u32,
    pub next_tick_ms: u32,
}

/// Known GW2 condition identities as `'static` literals (same set as
/// `is_condition`). Alias input is folded first, then matched ignore-ascii-case.
pub(crate) fn intern_foe_condition_name(name: &str) -> Cow<'static, str> {
    let want = crate::data::boon_condition_formulas::canonical_condition_name(name);
    const CANONICAL: &[&str] = &[
        "Bleeding",
        "Burning",
        "Poisoned",
        "Torment",
        "Confusion",
        "Vulnerability",
        "Weakness",
        "Blinded",
        "Chilled",
        "Crippled",
        "Fear",
        "Immobile",
        "Slow",
        "Taunt",
    ];
    for &canon in CANONICAL {
        if want.eq_ignore_ascii_case(canon) {
            return Cow::Borrowed(canon);
        }
    }
    Cow::Owned(want.to_string())
}

/// Live foe state during a simulation. Seeded from [`EnemyDummy`]; mutated as
/// conditions, disables, strips and damage land. This is the resolve-time target
/// for Vulnerability and deferred vs-target modifiers (Success [5]).
#[derive(Debug, Clone, PartialEq)]
pub struct TargetState {
    pub protection: bool,
    pub stability: bool,
    /// `None` = open dummy (no HP / encounter-outcome tracking).
    pub hp: Option<f64>,
    /// Wall-clock ms until which the foe is hard-disabled (stun/daze/…).
    pub disabled_until_ms: u32,
    /// Foe conditions (damaging and non-damaging), intensity-stacked.
    pub conditions: Vec<TimedFoeCondition>,
}

impl TargetState {
    /// Seed live state from the scenario dummy. No conditions, not disabled.
    pub fn from_seed(enemy: EnemyDummy) -> Self {
        Self {
            protection: enemy.protection,
            stability: enemy.stability,
            hp: enemy.hp,
            disabled_until_ms: 0,
            conditions: Vec::new(),
        }
    }

    /// Test/helper: start with named stacks that outlive any practical window.
    pub fn with_condition_stacks(mut self, name: &str, stacks: u32) -> Self {
        if stacks > 0 {
            self.conditions.push(TimedFoeCondition {
                name: intern_foe_condition_name(name),
                stacks,
                expires_at_ms: u32::MAX,
                next_tick_ms: u32::MAX,
            });
        }
        self
    }

    /// Unexpired stacks of `name` (canonical or alias), summed across entries.
    /// Exclusive: gone when `expires_at_ms == now_ms`.
    pub fn stacks_of(&self, name: &str, now_ms: u32) -> u32 {
        let want = crate::data::boon_condition_formulas::canonical_condition_name(name);
        // Entries are interned/canonical at push; fold the query once, not each row.
        self.conditions
            .iter()
            .filter(|c| c.expires_at_ms > now_ms && c.name.eq_ignore_ascii_case(want))
            .map(|c| c.stacks)
            .sum()
    }

    /// Stacks of `name` that still apply on a pulse paid at `now_ms` (`expires_at_ms >= now`).
    /// Condition-tick payout only — strike/cap/soft-control callers use [`Self::stacks_of`].
    pub fn stacks_of_inclusive(&self, name: &str, now_ms: u32) -> u32 {
        let want = crate::data::boon_condition_formulas::canonical_condition_name(name);
        self.conditions
            .iter()
            .filter(|c| c.expires_at_ms >= now_ms && c.name.eq_ignore_ascii_case(want))
            .map(|c| c.stacks)
            .sum()
    }

    /// Incoming multiplier for a condition pulse paid at `now_ms`.
    /// Vulnerability and vs-condition TargetGates stay live through `expires_at_ms == now`.
    /// Disabled gates keep exclusive `is_disabled`. Strike pricing must not call this.
    pub fn incoming_multiplier_at_tick(
        &self,
        now_ms: u32,
        mode: &gw2_core::types::GameMode,
        deferred: &[crate::combat::DeferredTargetModifier],
    ) -> f64 {
        let stacks = self
            .stacks_of_inclusive("Vulnerability", now_ms)
            .min(crate::data::boon_condition_formulas::conditions().vulnerability_max_stacks());
        let pct = crate::data::boon_condition_formulas::conditions()
            .vulnerability_incoming_pct_per_stack(mode);
        let mut mult = 1.0 + stacks as f64 * pct;
        for spec in deferred {
            if !matches!(
                spec.axis,
                crate::combat::TargetModAxis::Condition | crate::combat::TargetModAxis::Both
            ) {
                continue;
            }
            let (holds, gate_stacks) = match &spec.gate {
                crate::combat::TargetGate::Condition(name) => {
                    (self.stacks_of_inclusive(name, now_ms) > 0, 1.0)
                }
                crate::combat::TargetGate::PerStack { condition, max } => {
                    let n = self.stacks_of_inclusive(condition, now_ms).min(*max);
                    (n > 0, n as f64)
                }
                crate::combat::TargetGate::Disabled => (self.is_disabled(now_ms), 1.0),
            };
            if holds {
                mult *= 1.0 + gate_stacks * spec.percent / 100.0;
            }
        }
        mult
    }

    pub fn is_disabled(&self, now_ms: u32) -> bool {
        self.disabled_until_ms > now_ms
    }

    /// Apply stacks under the intensity cap from `conditions.json`.
    pub fn apply_condition(
        &mut self,
        name: &str,
        stacks: u32,
        duration_ms: u32,
        now_ms: u32,
        cap: u32,
    ) {
        if stacks == 0 || duration_ms == 0 || cap == 0 {
            return;
        }
        // ponytail: intensity stacks keep independent durations — merge only
        // same-expiry rows would change next_tick_ms when now_ms differs, so
        // we never upsert; intern the name to drop the per-apply String.
        let canonical = intern_foe_condition_name(name);
        let current = self.stacks_of(&canonical, now_ms);
        let can_apply = stacks.min(cap.saturating_sub(current));
        if can_apply == 0 {
            // Duration-stacked conditions (`max_stacks` 1: Chilled, Weakness,
            // Blinded, Slow, Immobile, Crippled, Fear, Taunt, Daze) refresh the
            // live row instead of dropping the re-application. Intensity-stacked
            // conditions at cap stay dropped.
            if cap == 1 {
                let expires = now_ms.saturating_add(duration_ms);
                if let Some(row) = self.conditions.iter_mut().find(|c| {
                    c.expires_at_ms > now_ms && c.name.eq_ignore_ascii_case(canonical.as_ref())
                }) {
                    row.expires_at_ms = row.expires_at_ms.max(expires);
                }
            }
            return;
        }
        self.conditions.push(TimedFoeCondition {
            name: canonical,
            stacks: can_apply,
            expires_at_ms: now_ms.saturating_add(duration_ms),
            next_tick_ms: now_ms.saturating_add(1_000),
        });
    }

    pub fn extend_disable(&mut self, until_ms: u32) {
        self.disabled_until_ms = self.disabled_until_ms.max(until_ms);
    }

    pub fn clear_boons(&mut self) {
        self.protection = false;
        self.stability = false;
    }

    pub fn retain_active(&mut self, now_ms: u32) {
        self.conditions.retain(|c| c.expires_at_ms > now_ms);
    }

    /// Vulnerability incoming-damage multiplier from `data/formulas/conditions.json`
    /// (`incoming_damage_pct_per_stack`, cap `max_stacks`).
    pub fn vulnerability_multiplier(&self, now_ms: u32, mode: &gw2_core::types::GameMode) -> f64 {
        let stacks = self
            .stacks_of("Vulnerability", now_ms)
            .min(crate::data::boon_condition_formulas::conditions().vulnerability_max_stacks());
        let pct = crate::data::boon_condition_formulas::conditions()
            .vulnerability_incoming_pct_per_stack(mode);
        1.0 + stacks as f64 * pct
    }
}

impl EnemyDummy {
    pub fn open() -> Self {
        Self::default()
    }

    /// Zerg/havoc blobs and roam harasser targets are assumed booned in WvW;
    /// PvE/PvP never inherit that prot+stab cover. Naked roam DPS is not booned.
    pub fn for_scenario(mode: &GameMode, tier: CombatTier, kind: CombatKind) -> Self {
        let hp = dummy_hp(tier, kind);
        if *mode != GameMode::WvW {
            return Self { hp, ..Self::open() };
        }
        match (tier, kind) {
            (CombatTier::Solo, CombatKind::Harasser | CombatKind::Disabler) => Self {
                protection: true,
                stability: true,
                hp,
            },
            (CombatTier::Party, _) | (CombatTier::Squad, _) => Self {
                protection: true,
                stability: true,
                hp,
            },
            _ => Self { hp, ..Self::open() },
        }
    }
}

pub fn setup_priority(skill: &RotationSkill) -> u32 {
    let mut p = 0u32;
    for e in &skill.effects {
        p = p.max(match e {
            SkillEffect::CrowdControl {
                stops_dodge: true, ..
            } => 100,
            SkillEffect::StripBoons { .. }
            | SkillEffect::StealBoons
            | SkillEffect::CorruptBoons => 90,
            SkillEffect::Cover { .. } => 80,
            SkillEffect::CrowdControl {
                stops_dodge: false, ..
            } => 70,
            SkillEffect::Mobility {
                kind: MobilityKind::Stealth,
            } => 60,
            _ => 0,
        });
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::{ControlKind, SkillSlot};
    use crate::scenario::CombatKind;

    fn skill_with(effects: Vec<SkillEffect>) -> RotationSkill {
        RotationSkill {
            targets: 1,
            skill_id: 1,
            name: "t".into(),
            slot: SkillSlot::Utility,
            cast_time_ms: 250,
            cooldown_ms: 10_000,
            effects,
            next_chain: None,
            is_stunbreak: false,
            reaches_allies: false,
            weapon_set: 0,
            categories: Vec::new(),
            slot_name: None,
        }
    }

    #[test]
    fn roam_exchange_includes_the_minimum_chain_and_response() {
        assert_eq!(
            simulation_window_ms(CombatTier::Solo, CombatKind::StrikeSpike),
            5_000
        );
        assert_eq!(
            simulation_window_ms(CombatTier::Solo, CombatKind::Harasser),
            10_000
        );
    }

    #[test]
    fn zerg_kinds_do_not_share_a_clock() {
        assert_eq!(
            simulation_window_ms(CombatTier::Squad, CombatKind::StrikeSpike),
            5_000
        );
        assert_eq!(
            simulation_window_ms(CombatTier::Squad, CombatKind::CondiRamp),
            20_000
        );
        assert_eq!(
            simulation_window_ms(CombatTier::Squad, CombatKind::Harasser),
            10_000
        );
    }

    #[test]
    fn setup_window_keeps_the_two_second_opening() {
        assert_eq!(setup_window_ms(30_000), 2_000);
        assert_eq!(setup_window_ms(2_000), 2_000);
    }

    #[test]
    fn lock_outranks_damage_in_setup_priority() {
        let lock = skill_with(vec![SkillEffect::CrowdControl {
            kind: ControlKind::Stun,
            duration_ms: 1000,
            stops_dodge: true,
        }]);
        let dps = skill_with(vec![SkillEffect::StrikeDamage {
            hit_count: 1,
            dmg_multiplier: 10.0,
        }]);
        assert!(setup_priority(&lock) > setup_priority(&dps));
    }

    #[test]
    fn roam_out_requires_mobility_effect() {
        let burst = vec![skill_with(vec![SkillEffect::StrikeDamage {
            hit_count: 1,
            dmg_multiplier: 2.0,
        }])];
        let with_out = vec![skill_with(vec![SkillEffect::Mobility {
            kind: MobilityKind::Teleport,
        }])];
        let with_block = vec![skill_with(vec![SkillEffect::Cover {
            kind: CoverKind::Block,
            duration_ms: 1000,
            strippable: false,
        }])];
        assert!(!kit_has_mobility_out(&burst));
        assert!(kit_has_mobility_out(&with_out));
        assert!(kit_has_mobility_out(&with_block));
    }

    #[test]
    fn cover_answer_evade_not_leap() {
        let leap = vec![skill_with(vec![SkillEffect::Mobility {
            kind: MobilityKind::Leap,
        }])];
        let evade = vec![skill_with(vec![SkillEffect::Mobility {
            kind: MobilityKind::Evade,
        }])];
        assert!(!kit_has_cover_answer(&leap));
        assert!(kit_has_cover_answer(&evade));
    }

    #[test]
    fn harasser_strip_gate_accepts_strip_steal_or_corrupt() {
        assert!(kit_has_strip(&[skill_with(vec![
            SkillEffect::StripBoons {
                count_per_pulse: 1,
                interval_ms: 1000,
                window_ms: 5000,
            }
        ])]));
        assert!(kit_has_strip(&[skill_with(vec![SkillEffect::StealBoons])]));
        assert!(kit_has_strip(&[skill_with(vec![
            SkillEffect::CorruptBoons
        ])]));
        assert!(!kit_has_strip(&[skill_with(vec![
            SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }
        ])]));
    }

    /// 18 calibration rows covering the supported combat clocks and kit gates.
    #[test]
    fn calibration_eighteen_rows() {
        use CombatKind::*;
        use CombatTier::*;
        let rows: [(CombatTier, CombatKind, u32); 18] = [
            (Squad, StrikeSpike, 5_000), // 1 Reaper
            (Squad, StrikeSpike, 5_000), // 2 Untamed
            (Squad, StrikeSpike, 5_000), // 3 Evoker ranged
            (Squad, Support, 20_000),    // 4 Firebrand
            (Squad, Support, 20_000),    // 5 Druid
            (Squad, Support, 20_000),    // 6 Troubadour
            (Squad, Disabler, 10_000),   // 7 Core Necro corrupt
            (Squad, Disabler, 10_000),   // 8 Spellbreaker
            (Squad, Harasser, 10_000),   // 9 Conduit
            (Squad, Harasser, 10_000),   // 10 Dragonhunter
            (Squad, Commander, 20_000),  // 11 Luminary tag
            (Solo, Harasser, 10_000),    // 12 Willbender
            (Solo, Harasser, 10_000),    // 13 Deadeye
            (Solo, Harasser, 10_000),    // 14 Virtuoso
            (Solo, Harasser, 10_000),    // 15 Herald
            (Solo, CondiRamp, 20_000),   // 16 Soulbeast
            (Party, StrikeSpike, 5_000), // 17 Celestial Herald havoc
            (Party, StrikeSpike, 5_000), // 18 Scrapper havoc
        ];
        for (i, (tier, kind, t)) in rows.iter().enumerate() {
            assert_eq!(
                simulation_window_ms(*tier, *kind),
                *t,
                "row {} clock",
                i + 1
            );
            let require_out = *tier == Solo;
            let require_strip = *kind == Harasser;
            if require_out {
                assert!(*tier == Solo, "row {} roam must be Solo", i + 1);
            }
            if require_strip && *tier == Squad {
                assert_eq!(*kind, Harasser);
            }
        }
    }

    #[test]
    fn resistance_does_not_negate_poison_heal_or_terror() {
        assert!(resistance_negates("Immobile"));
        assert!(resistance_negates("Fear"));
        assert!(!resistance_negates("Poisoned"));
        assert!(!resistance_negates("Burning"));
        assert!(!resistance_negates("Terror"));
    }

    #[test]
    fn corrupt_resistance_becomes_chill() {
        assert_eq!(corrupt_into("Resistance"), Some("Chilled"));
        assert_eq!(corrupt_into("Protection"), Some("Vulnerability"));
        assert_eq!(corrupt_into("Aegis"), Some("Burning"));
        assert_eq!(corrupt_into("Vigor"), Some("Bleeding"));
        assert!(corrupt_into("Distortion").is_none());
    }

    #[test]
    fn buff_profile_follows_scale() {
        assert_eq!(buff_profile_index(CombatTier::Solo), 0);
        assert_eq!(buff_profile_index(CombatTier::Party), 1);
        assert_eq!(buff_profile_index(CombatTier::Squad), 2);
    }

    #[test]
    fn zerg_dummy_starts_booned_roam_dps_does_not() {
        let wvw = GameMode::WvW;
        let zerg = EnemyDummy::for_scenario(&wvw, CombatTier::Squad, CombatKind::StrikeSpike);
        assert!(zerg.protection && zerg.stability);
        assert!(zerg.hp.is_none());
        let roam_dps = EnemyDummy::for_scenario(&wvw, CombatTier::Solo, CombatKind::StrikeSpike);
        assert!(!roam_dps.protection && !roam_dps.stability);
        assert_eq!(roam_dps.hp, Some(13_000.0));
        let roam_pick = EnemyDummy::for_scenario(&wvw, CombatTier::Solo, CombatKind::Harasser);
        assert!(roam_pick.protection && roam_pick.stability);
        assert_eq!(roam_pick.hp, Some(13_000.0));
        let havoc = EnemyDummy::for_scenario(&wvw, CombatTier::Party, CombatKind::StrikeSpike);
        assert_eq!(havoc.hp, Some(20_000.0));
        assert!(havoc.protection && havoc.stability);
        let support = EnemyDummy::for_scenario(&wvw, CombatTier::Solo, CombatKind::Support);
        assert!(support.hp.is_none());
        let troll = EnemyDummy::for_scenario(&wvw, CombatTier::Squad, CombatKind::Staller);
        assert!(troll.hp.is_none());
        assert_eq!(
            simulation_window_ms(CombatTier::Solo, CombatKind::Staller),
            20_000
        );
        assert_eq!(
            simulation_window_ms(CombatTier::Squad, CombatKind::Staller),
            20_000
        );
    }

    #[test]
    fn pve_party_squad_do_not_inherit_wvw_prot_stab() {
        for tier in [CombatTier::Party, CombatTier::Squad] {
            let dummy = EnemyDummy::for_scenario(&GameMode::PvE, tier, CombatKind::StrikeSpike);
            assert!(
                !dummy.protection && !dummy.stability,
                "PvE {tier:?} must not inherit WvW prot+stab"
            );
        }
        let pvp =
            EnemyDummy::for_scenario(&GameMode::PvP, CombatTier::Squad, CombatKind::StrikeSpike);
        assert!(!pvp.protection && !pvp.stability);
        let wvw =
            EnemyDummy::for_scenario(&GameMode::WvW, CombatTier::Squad, CombatKind::StrikeSpike);
        assert!(wvw.protection && wvw.stability);
    }

    #[test]
    fn stacks_of_accepts_alias_on_overlapping_rows() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Poison", 2, 3_000, 0, 25);
        t.apply_condition("Poisoned", 3, 8_000, 0, 25);
        assert_eq!(t.conditions.len(), 2);
        assert_eq!(t.stacks_of("Poison", 0), 5);
        assert_eq!(t.stacks_of("Poisoned", 0), 5);
        assert_eq!(t.stacks_of("poisoned", 0), 5);
        assert_eq!(t.stacks_of_inclusive("Poison", 3_000), 5);
        assert_eq!(t.stacks_of("Poison", 3_000), 3);
    }

    #[test]
    fn cap_one_condition_reapply_refreshes_duration() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Chilled", 1, 3_000, 0, 1);
        t.apply_condition("Chilled", 1, 3_000, 1_000, 1);
        assert_eq!(t.conditions.len(), 1, "cap-1 refresh must not add a row");
        assert_eq!(t.stacks_of("Chilled", 2_999), 1);
        assert_eq!(t.stacks_of("Chilled", 3_999), 1, "refreshed to 4000 ms");
        assert_eq!(t.stacks_of("Chilled", 4_000), 0);
    }

    #[test]
    fn cap_one_condition_reapply_never_shortens() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Weakness", 1, 5_000, 0, 1);
        t.apply_condition("Weakness", 1, 1_000, 1_000, 1);
        assert_eq!(t.conditions[0].expires_at_ms, 5_000);
    }

    #[test]
    fn intensity_stacked_condition_at_cap_is_still_dropped() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Vulnerability", 25, 3_000, 0, 25);
        t.apply_condition("Vulnerability", 1, 9_000, 0, 25);
        assert_eq!(t.conditions.len(), 1);
        assert_eq!(t.conditions[0].expires_at_ms, 3_000);
        assert_eq!(t.stacks_of("Vulnerability", 0), 25);
    }

    #[test]
    fn apply_condition_different_expiries_tick_independently() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Burning", 2, 3_000, 0, 25);
        t.apply_condition("Burning", 3, 8_000, 0, 25);
        assert_eq!(
            t.conditions.len(),
            2,
            "independent durations must not merge"
        );
        assert_eq!(t.conditions[0].expires_at_ms, 3_000);
        assert_eq!(t.conditions[1].expires_at_ms, 8_000);
        assert_eq!(t.stacks_of("Burning", 2_999), 5);
        assert_eq!(t.stacks_of("Burning", 3_000), 3);
        assert_eq!(t.stacks_of_inclusive("Burning", 3_000), 5);
        assert_eq!(t.stacks_of("Burning", 8_000), 0);
        assert_eq!(t.stacks_of_inclusive("Burning", 8_000), 3);
        t.retain_active(3_000);
        assert_eq!(t.conditions.len(), 1);
        assert_eq!(t.conditions[0].stacks, 3);
    }

    #[test]
    fn apply_condition_canonical_name_is_interned() {
        let mut t = TargetState::from_seed(EnemyDummy::open());
        t.apply_condition("Burning", 1, 1_000, 0, 25);
        match &t.conditions[0].name {
            std::borrow::Cow::Borrowed(n) => assert_eq!(*n, "Burning"),
            std::borrow::Cow::Owned(_) => {
                panic!("canonical apply must store borrowed interned name")
            }
        }
        t.apply_condition("Poison", 1, 1_000, 0, 25);
        match &t.conditions[1].name {
            std::borrow::Cow::Borrowed(n) => assert_eq!(*n, "Poisoned"),
            std::borrow::Cow::Owned(_) => {
                panic!("alias apply must store borrowed interned canonical")
            }
        }
    }

    #[test]
    fn interrupt_is_any_crowd_control() {
        assert!(kit_has_interrupt(&[skill_with(vec![
            SkillEffect::CrowdControl {
                kind: ControlKind::Daze,
                duration_ms: 500,
                stops_dodge: false,
            }
        ])]));
        assert!(!kit_has_interrupt(&[skill_with(vec![
            SkillEffect::StrikeDamage {
                hit_count: 1,
                dmg_multiplier: 1.0,
            }
        ])]));
    }
}
