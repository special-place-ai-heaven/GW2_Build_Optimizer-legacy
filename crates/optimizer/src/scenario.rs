use gw2_core::types::GameMode;

use crate::balance::BalanceContext;
use crate::scoring::OptimizationWeights;

/// Scenario is part of the optimization input, not implicit engine state.
/// A build can only be called "best" relative to a declared scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioSpec {
    pub game_mode: GameMode,
    pub combat_tier: CombatTier,
    pub combat_kind: CombatKind,
    pub target_profile: TargetProfile,
    pub optimization_target: OptimizationTarget,
    pub patch_id: Option<String>,
    /// Role profile id for viability floors. `None` keeps hardcoded mode/tier defaults.
    pub objective_profile_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CombatTier {
    Solo,
    Party,
    #[default]
    Squad,
}

/// What job the dummy is scoring. Independent of [`CombatTier`] (scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CombatKind {
    #[default]
    StrikeSpike,
    CondiRamp,
    Harasser,
    Support,
    Disabler,
    Commander,
    /// Occupy / escape — survive until backup. Not a kill job.
    Staller,
}

impl CombatKind {
    /// The job this fight shape implies, so a scenario that never named an
    /// `objective_profile_id` (references, tests, any caller without a role
    /// chip) still resolves to its data profile instead of falling back to
    /// hardcoded gate floors. Inverse of [`RoleObjective::combat_kind`].
    pub fn role_objective(&self) -> RoleObjective {
        match self {
            CombatKind::StrikeSpike => RoleObjective::PowerDps,
            CombatKind::CondiRamp => RoleObjective::CondiDps,
            CombatKind::Harasser => RoleObjective::WvWRoamer,
            CombatKind::Support => RoleObjective::Buffer,
            CombatKind::Disabler => RoleObjective::Disabler,
            CombatKind::Commander => RoleObjective::Tank,
            CombatKind::Staller => RoleObjective::Staller,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            CombatKind::StrikeSpike => "Strike spike",
            CombatKind::CondiRamp => "Condi ramp",
            CombatKind::Harasser => "Harasser",
            CombatKind::Support => "Support",
            CombatKind::Disabler => "Disabler",
            CombatKind::Commander => "Commander",
            CombatKind::Staller => "Staller",
        }
    }
}

impl CombatTier {
    /// Human-readable label used in UI and logs.
    pub fn label(&self) -> &'static str {
        match self {
            CombatTier::Solo => "Roam",
            CombatTier::Party => "Havoc",
            CombatTier::Squad => "Cloud/Zerg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetProfile {
    Single,
    Cleave,
    AoE,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationTarget {
    pub label: String,
}

impl ScenarioSpec {
    pub fn from_balance_context(ctx: &BalanceContext) -> Self {
        Self {
            game_mode: ctx.game_mode.clone(),
            combat_tier: CombatTier::Solo,
            combat_kind: CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: ctx.game_mode.label().to_string(),
            },
            patch_id: Some(ctx.patch_id.clone()),
            objective_profile_id: None,
        }
    }

    /// The scenario a REQUEST names: the player's mode, their scale chip and
    /// their role chip.
    ///
    /// The one place a request becomes a scenario. `objective_profile_id` is
    /// what `intent_alignment` reads the focus and avoid axes from, so a
    /// caller that forgets it does not get a slightly different answer, it
    /// gets a different QUESTION: the referee falls back to the scenario's
    /// combat kind, which for a freshly built spec is the mode default, and
    /// every candidate is then measured against a DPS direction. That is a
    /// silent wrong answer, which is why this lives here and not at three
    /// call sites (in-game 2026-09-22: the example offered a Berserker for a
    /// Support request while the addon offered a healer).
    pub fn for_request(
        ctx: &crate::balance::BalanceContext,
        combat_tier: CombatTier,
        role: Option<RoleObjective>,
        weights: &OptimizationWeights,
    ) -> Self {
        let combat_kind = role
            .map(|r| r.combat_kind_for_weights(weights))
            .unwrap_or_else(|| {
                if weights.condition > weights.power {
                    CombatKind::CondiRamp
                } else {
                    CombatKind::StrikeSpike
                }
            });
        Self {
            combat_tier,
            combat_kind,
            objective_profile_id: role
                .map(|r| r.profile_id_for(&ctx.game_mode, combat_tier).to_string()),
            ..Self::from_balance_context(ctx)
        }
    }

    /// Construct scenario with explicit combat tier (used by UI when WvW sub-role is selected).
    pub fn with_combat_tier(mut self, tier: CombatTier) -> Self {
        self.combat_tier = tier;
        self
    }
}

// Role Objectives

/// Job the player picked. Mode remaps weights via [`profile_id_for`].
/// Overlay chips are families ([`PLAY_ROLES`]); conversation picks the lean
/// (power vs condi, celestial fight-support vs zerg stab specialist).
/// Scale retunes WvW Support: Roam/Havoc is self-reliant; Cloud/Zerg specializes.
///
/// Legacy WvW/PvP variants stay so old mappings and tests still compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RoleObjective {
    PowerDps,
    CondiDps,
    Sustain,
    Tank,
    Healer,
    Disabler,
    Buffer,
    /// Celestial-style all-rounder. Occupies; not a specialist.
    Hybrid,
    /// Stall until backup. Survive via tank/heal/evade/port/stealth — not DPS.
    Staller,
    WvWRoamer,
    WvWZergDps,
    WvWZergSupport,
    WvWDisruptor,
    PvPBurst,
    PvPSustain,
    PvPDisruptor,
}

impl RoleObjective {
    /// Shared overlay chips. Families, not finished jobs. Conversation picks the lean.
    ///
    /// The WvW set, and the historical set for every mode. Prefer
    /// [`RoleObjective::play_roles_for`], which offers a mode only the roles
    /// it has.
    pub const PLAY_ROLES: [RoleObjective; 7] = [
        RoleObjective::WvWRoamer,
        RoleObjective::PowerDps,
        RoleObjective::Sustain,
        RoleObjective::Staller,
        RoleObjective::Buffer,
        RoleObjective::Disabler,
        RoleObjective::Tank,
    ];

    /// Roles for PvE. Damage split by kind, plus the three support jobs a
    /// group or squad actually slots.
    const PVE_ROLES: [RoleObjective; 5] = [
        RoleObjective::PowerDps,
        RoleObjective::CondiDps,
        RoleObjective::Buffer,
        RoleObjective::Healer,
        RoleObjective::Tank,
    ];

    /// Roles for PvP: conquest jobs, which are about holding and taking
    /// points rather than about squad composition.
    const PVP_ROLES: [RoleObjective; 5] = [
        RoleObjective::PowerDps,
        RoleObjective::WvWRoamer,
        RoleObjective::Sustain,
        RoleObjective::Buffer,
        RoleObjective::Disabler,
    ];

    /// The roles a mode actually has.
    ///
    /// The three modes do not share a vocabulary, and the build sites agree
    /// on that: measured across every build GuildJen lists, PvE roles are
    /// dps, support, tank and kiter; WvW's are bruiser, assassin, medic,
    /// support, dps and tank; PvP's are dps, roamer, duelist and support.
    ///
    /// One list for all three offered PvE a "Roamer" and a "Troll", which no
    /// PvE reference build is, and withheld the Condi and Heal chips, which
    /// most of them are. That is not only a wrong menu: the chosen role is
    /// matched against the stored one by word overlap, so a chip no source
    /// ever writes can never match anything, and the reference shown falls
    /// back to whichever row happened to sort first.
    pub fn play_roles_for(game_mode: &GameMode) -> &'static [RoleObjective] {
        match game_mode {
            GameMode::PvE => &Self::PVE_ROLES,
            GameMode::PvP => &Self::PVP_ROLES,
            GameMode::WvW => &Self::PLAY_ROLES,
        }
    }

    /// Display label shown in the UI.
    pub fn label(&self) -> &'static str {
        match self {
            RoleObjective::PowerDps => "Power DPS",
            RoleObjective::CondiDps => "Condi DPS",
            RoleObjective::Sustain => "Sustain / Bruiser",
            RoleObjective::Tank => "Tank",
            RoleObjective::Healer => "Healer",
            RoleObjective::Disabler => "Disabler / CC",
            RoleObjective::Buffer => "Buffer / Support",
            RoleObjective::Hybrid => "Hybrid",
            RoleObjective::Staller => "Troll",
            RoleObjective::WvWRoamer => "Roamer",
            RoleObjective::WvWZergDps => "WvW Zerg DPS",
            RoleObjective::WvWZergSupport => "WvW Zerg Support",
            RoleObjective::WvWDisruptor => "WvW Disruptor",
            RoleObjective::PvPBurst => "PvP Burst",
            RoleObjective::PvPSustain => "PvP Sustain",
            RoleObjective::PvPDisruptor => "PvP Disruptor",
        }
    }

    /// Short chip label (same as [`label`] for play roles).
    pub fn play_label(&self) -> &'static str {
        match self {
            RoleObjective::PowerDps => "Damage",
            RoleObjective::CondiDps => "Condi",
            RoleObjective::Sustain => "Bruiser",
            RoleObjective::Tank => "Commander",
            RoleObjective::Healer => "Heal",
            RoleObjective::Disabler => "Disable",
            RoleObjective::Buffer => "Support",
            RoleObjective::Hybrid => "Hybrid",
            RoleObjective::Staller => "Troll",
            RoleObjective::WvWRoamer => "Roamer",
            other => other.label(),
        }
    }

    /// How this family plays at the given scale. Conversation still picks the lean.
    pub fn family_brief(&self, game_mode: &GameMode, tier: CombatTier) -> &'static str {
        match self {
            RoleObjective::PowerDps => {
                "Damage family: power, condi, or hybrid from the player's words."
            }
            RoleObjective::Buffer => match (game_mode, tier) {
                (GameMode::WvW, CombatTier::Squad) => {
                    "Large-group Support: several supports specialize — one stab uptime so the blob cannot be disabled, one heal/cleanse, one boon duration. Player's words pick which. Not a lone Magi's/Minstrel heal-bot unless they asked."
                }
                (GameMode::WvW, _) => {
                    "Small-group Support: self-reliant. If focused, teammates are busy — prot, invuln, stunbreaks, some fight. Dead support is not support. Player's words pick the lean (celestial hybrid, boon, cleanse, disable)."
                }
                _ => {
                    "Support: force multiplier (boons, prot, cleanse, disable, some fight). Not a dedicated healer unless they asked. Player's words pick the lean."
                }
            },
            RoleObjective::Disabler => {
                "Disable family: CC, strip, interrupt. Player's words pick the lean."
            }
            RoleObjective::Sustain => "Bruiser: fights and lives. Player's words pick the lean.",
            RoleObjective::Staller => {
                "Troll: stall, don't kill. Evade, port, stealth until backup."
            }
            RoleObjective::WvWRoamer => {
                "Roamer: outnumbered. Dive, blender, or trickster from the player's words."
            }
            RoleObjective::Tank => "Commander: frontline presence, toughness, stability.",
            RoleObjective::Healer => "Dedicated healer. Only if they asked for a heal-bot.",
            _ => "Infer the job from the player's words.",
        }
    }

    /// The `objective_profile_id` this role maps to for the given game mode.
    /// Scale defaults to Cloud/Zerg — use [`profile_id_for`] when Scale is known.
    pub fn profile_id(&self, game_mode: &GameMode) -> &'static str {
        self.profile_id_for(game_mode, CombatTier::default())
    }

    /// Like [`profile_id`], but WvW Support splits on Scale:
    /// Roam/Havoc → self-reliant `WvW_Support`; Cloud/Zerg → specialist `WvW_Zerg_Support`.
    pub fn profile_id_for(&self, game_mode: &GameMode, tier: CombatTier) -> &'static str {
        if matches!(self, RoleObjective::Buffer) && *game_mode == GameMode::WvW {
            return match tier {
                CombatTier::Squad => "WvW_Zerg_Support",
                CombatTier::Solo | CombatTier::Party => "WvW_Support",
            };
        }
        match (self, game_mode) {
            (RoleObjective::WvWRoamer, GameMode::WvW) => "WvW_Roamer",
            (RoleObjective::WvWRoamer, GameMode::PvP) => "PvP_Harasser",
            (RoleObjective::WvWRoamer, GameMode::PvE) => "PvE_Harasser",
            (RoleObjective::PowerDps, GameMode::PvE) => "PvE_Power_DPS",
            (RoleObjective::PowerDps, GameMode::WvW) => "WvW_Zerg_DPS",
            (RoleObjective::PowerDps, GameMode::PvP) => "PvP_Burst",
            (RoleObjective::CondiDps, GameMode::PvE) => "PvE_Condi_DPS",
            (RoleObjective::CondiDps, GameMode::WvW) => "WvW_Condi",
            (RoleObjective::CondiDps, GameMode::PvP) => "PvP_Condi",
            (RoleObjective::Hybrid, GameMode::PvE) => "PvE_Hybrid",
            (RoleObjective::Hybrid, GameMode::WvW) => "WvW_Hybrid",
            (RoleObjective::Hybrid, GameMode::PvP) => "PvP_Hybrid",
            (RoleObjective::Sustain, GameMode::PvE) => "PvE_Bruiser",
            (RoleObjective::Sustain, GameMode::WvW) => "WvW_Bruiser",
            (RoleObjective::Sustain, GameMode::PvP) => "PvP_Bruiser",
            (RoleObjective::Staller, GameMode::PvE) => "PvE_Staller",
            (RoleObjective::Staller, GameMode::WvW) => "WvW_Staller",
            (RoleObjective::Staller, GameMode::PvP) => "PvP_Staller",
            (RoleObjective::Healer, GameMode::PvE) => "PvE_Healer",
            (RoleObjective::Healer, GameMode::WvW) => "WvW_Heal",
            (RoleObjective::Healer, GameMode::PvP) => "PvP_Sustain",
            (RoleObjective::Buffer, GameMode::PvE) => "PvE_Boon_Support",
            (RoleObjective::Buffer, GameMode::WvW) => "WvW_Zerg_Support",
            (RoleObjective::Buffer, GameMode::PvP) => "PvP_Boon_Pressure",
            (RoleObjective::Disabler, GameMode::PvE) => "PvE_Disabler",
            (RoleObjective::Disabler, GameMode::WvW) => "WvW_Disruptor",
            (RoleObjective::Disabler, GameMode::PvP) => "PvP_Control_Disruptor",
            (RoleObjective::Tank, GameMode::PvE) => "PvE_Commander",
            (RoleObjective::Tank, GameMode::WvW) => "WvW_Commander",
            (RoleObjective::Tank, GameMode::PvP) => "PvP_Commander",
            (RoleObjective::WvWZergDps, _) => "WvW_Zerg_DPS",
            (RoleObjective::WvWZergSupport, _) => "WvW_Zerg_Support",
            (RoleObjective::WvWDisruptor, _) => "WvW_Disruptor",
            (RoleObjective::PvPBurst, _) => "PvP_Burst",
            (RoleObjective::PvPSustain, _) => "PvP_Sustain",
            (RoleObjective::PvPDisruptor, _) => "PvP_Control_Disruptor",
        }
    }

    /// The word the overlay's chip is spelled with, in English.
    ///
    /// The same vocabulary the addon's `role_i18n_key` uses (`role.damage`,
    /// `role.support`, ...), so a tool outside the addon can name a chip the
    /// way the player sees it. `label()` is the long form - "Power DPS",
    /// "Buffer / Support" - and nobody types that.
    pub fn chip_word(&self, game_mode: &GameMode) -> &'static str {
        match self {
            RoleObjective::WvWRoamer => "roamer",
            RoleObjective::PowerDps | RoleObjective::WvWZergDps | RoleObjective::PvPBurst => {
                "damage"
            }
            RoleObjective::CondiDps => "condi",
            RoleObjective::Healer => "heal",
            RoleObjective::Sustain | RoleObjective::PvPSustain => "bruiser",
            RoleObjective::Staller => "troll",
            RoleObjective::Buffer | RoleObjective::WvWZergSupport => "support",
            RoleObjective::Disabler | RoleObjective::WvWDisruptor | RoleObjective::PvPDisruptor => {
                "disable"
            }
            RoleObjective::Hybrid => "hybrid",
            // The same split the chip label makes: a WvW or PvE tank is a
            // commander, a PvE one is a tank.
            RoleObjective::Tank => match game_mode {
                GameMode::PvE => "tank",
                _ => "commander",
            },
        }
    }

    /// Resolve a typed name to a chip this mode offers.
    ///
    /// Accepts the chip word (`support`) or any distinctive part of
    /// [`Self::label`] (`Buffer`, `Power DPS`). Case-insensitive. `None`
    /// when the mode offers no such chip - which a caller must treat as an
    /// error rather than as "no role", because a request with no role is
    /// measured against the mode default and answers a question nobody
    /// asked.
    pub fn from_name(name: &str, game_mode: &GameMode) -> Option<Self> {
        let name = name.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        let offered = Self::play_roles_for(game_mode);
        offered
            .iter()
            .find(|r| r.chip_word(game_mode) == name)
            .or_else(|| {
                offered
                    .iter()
                    .find(|r| r.label().to_lowercase().contains(&name))
            })
            .copied()
    }

    /// The combat tier this role implies (used to construct ScenarioSpec).
    pub fn combat_tier(&self) -> CombatTier {
        match self {
            RoleObjective::WvWRoamer => CombatTier::Solo,
            RoleObjective::WvWDisruptor => CombatTier::Solo,
            RoleObjective::WvWZergDps => CombatTier::Squad,
            RoleObjective::WvWZergSupport => CombatTier::Squad,
            RoleObjective::PvPBurst => CombatTier::Solo,
            RoleObjective::PvPSustain => CombatTier::Solo,
            RoleObjective::PvPDisruptor => CombatTier::Solo,
            RoleObjective::Hybrid | RoleObjective::Staller => CombatTier::Solo,
            // Generic archetypes default to Party (sensible middle ground)
            _ => CombatTier::Party,
        }
    }

    /// Task axis — independent of [`combat_tier`] (scale).
    pub fn combat_kind(&self) -> CombatKind {
        match self {
            RoleObjective::CondiDps => CombatKind::CondiRamp,
            RoleObjective::Healer | RoleObjective::Buffer | RoleObjective::WvWZergSupport => {
                CombatKind::Support
            }
            RoleObjective::Disabler | RoleObjective::WvWDisruptor | RoleObjective::PvPDisruptor => {
                CombatKind::Disabler
            }
            RoleObjective::WvWRoamer => CombatKind::StrikeSpike,
            RoleObjective::Hybrid | RoleObjective::Staller => CombatKind::Staller,
            RoleObjective::Tank => CombatKind::Commander,
            RoleObjective::PowerDps
            | RoleObjective::Sustain
            | RoleObjective::WvWZergDps
            | RoleObjective::PvPBurst
            | RoleObjective::PvPSustain => CombatKind::StrikeSpike,
        }
    }

    /// Roamers are three jobs, not one 1v1 dummy.
    /// Dive: 2s port-in burst. Trickster: 5s peck/attrition. Bruiser: 10s cover fight.
    pub fn combat_kind_for_weights(&self, weights: &OptimizationWeights) -> CombatKind {
        if !matches!(self, RoleObjective::WvWRoamer) {
            return self.combat_kind();
        }
        let power = weights.power;
        let condi = weights.condition;
        let sustain = weights.sustain;
        if sustain > power && sustain > condi {
            CombatKind::Staller
        } else if condi > power {
            CombatKind::CondiRamp
        } else {
            CombatKind::StrikeSpike
        }
    }

    /// Convert this role to `OptimizationWeights` by reading the objective profile data.
    /// Falls back to mode default weights if the profile is not found.
    pub fn to_weights(&self, game_mode: &GameMode) -> OptimizationWeights {
        self.to_weights_for(game_mode, CombatTier::default())
    }

    /// Like [`to_weights`], passing Scale so WvW Support can split roam vs zerg.
    pub fn to_weights_for(&self, game_mode: &GameMode, tier: CombatTier) -> OptimizationWeights {
        let id = self.profile_id_for(game_mode, tier);
        let profiles = crate::data::objective_profiles::objective_profiles();
        if let Some(profile) = profiles.profile_by_id(id) {
            let aw = &profile.axis_weights;
            OptimizationWeights {
                power: aw.power,
                condition: aw.condition,
                boon_support: aw.boon_support,
                healing: aw.healing,
                sustain: aw.sustain,
                control: aw.control,
            }
        } else {
            OptimizationWeights::default_for_mode(game_mode.label())
        }
    }
}

#[cfg(test)]
mod tests {

    /// Every chip a mode offers is reachable by the word it is spelled
    /// with, and no two chips in one mode answer to the same word.
    ///
    /// Without this the two tables drift: `chip_word` says "damage", the
    /// mode offers `PowerDps`, and a tool that typed "Damage" silently got
    /// no role and measured against the mode default (2026-09-22).
    #[test]
    fn every_chip_resolves_from_its_own_word() {
        for mode in GameMode::ALL {
            let offered = RoleObjective::play_roles_for(&mode);
            let mut words: Vec<&str> = Vec::new();
            for role in offered {
                let word = role.chip_word(&mode);
                assert!(
                    !words.contains(&word),
                    "{} offers two chips spelled {word:?}",
                    mode.label()
                );
                words.push(word);
                assert_eq!(
                    RoleObjective::from_name(word, &mode),
                    Some(*role),
                    "{word:?} in {}",
                    mode.label()
                );
                assert_eq!(
                    RoleObjective::from_name(role.label(), &mode),
                    Some(*role),
                    "the long label must work too: {}",
                    role.label()
                );
            }
            assert_eq!(RoleObjective::from_name("nonsense", &mode), None);
            assert_eq!(RoleObjective::from_name("", &mode), None);
        }
        // A chip another mode offers is still not a chip THIS mode offers.
        assert_eq!(RoleObjective::from_name("heal", &GameMode::WvW), None);
        assert_eq!(
            RoleObjective::from_name("heal", &GameMode::PvE),
            Some(RoleObjective::Healer)
        );
    }

    /// The scenario a request builds must name the profile the chip maps to,
    /// at every mode and every scale.
    ///
    /// The bug this exists for: `scenario_for_run` used to leave
    /// `objective_profile_id` unset and let the referee recover it from
    /// `combat_kind`, which is a lossy round trip - Healer becomes
    /// `CombatKind::Support` and comes back as Buffer, so the Healer chip
    /// was scored against `WvW_Support` rather than `WvW_Heal`, and
    /// `WvW_Roamer` was unreachable from any chip at all. The id is set
    /// explicitly now; this pins it for every combination, and the match
    /// below fails to compile when a role is added so a new chip cannot
    /// drift back into the fallback.
    #[test]
    fn every_role_and_scale_names_its_own_profile() {
        let all = [
            RoleObjective::PowerDps,
            RoleObjective::CondiDps,
            RoleObjective::Hybrid,
            RoleObjective::Sustain,
            RoleObjective::Staller,
            RoleObjective::Healer,
            RoleObjective::Buffer,
            RoleObjective::Disabler,
            RoleObjective::Tank,
            RoleObjective::WvWRoamer,
            RoleObjective::WvWZergDps,
            RoleObjective::WvWZergSupport,
            RoleObjective::WvWDisruptor,
            RoleObjective::PvPBurst,
            RoleObjective::PvPSustain,
            RoleObjective::PvPDisruptor,
        ];
        // Exhaustive: adding a `RoleObjective` breaks this match, which is
        // the reminder to add it to `all` above.
        for role in all {
            match role {
                RoleObjective::PowerDps
                | RoleObjective::CondiDps
                | RoleObjective::Hybrid
                | RoleObjective::Sustain
                | RoleObjective::Staller
                | RoleObjective::Healer
                | RoleObjective::Buffer
                | RoleObjective::Disabler
                | RoleObjective::Tank
                | RoleObjective::WvWRoamer
                | RoleObjective::WvWZergDps
                | RoleObjective::WvWZergSupport
                | RoleObjective::WvWDisruptor
                | RoleObjective::PvPBurst
                | RoleObjective::PvPSustain
                | RoleObjective::PvPDisruptor => {}
            }
        }
        for mode in GameMode::ALL {
            let ctx = crate::balance::BalanceContext::new(mode.clone());
            for tier in [CombatTier::Solo, CombatTier::Party, CombatTier::Squad] {
                for role in all {
                    let weights = role.to_weights_for(&mode, tier);
                    let scenario = ScenarioSpec::for_request(&ctx, tier, Some(role), &weights);
                    assert_eq!(
                        scenario.objective_profile_id.as_deref(),
                        Some(role.profile_id_for(&mode, tier)),
                        "{role:?} in {} at {tier:?}",
                        mode.label()
                    );
                    assert_eq!(scenario.combat_tier, tier);
                }
            }
        }
    }

    /// A request with no chip still names no profile: the caller asked for
    /// nothing in particular, and inventing a role for them would score
    /// their build against a job they did not pick.
    #[test]
    fn a_request_without_a_role_names_no_profile() {
        let ctx = crate::balance::BalanceContext::new(GameMode::WvW);
        let weights = crate::scoring::OptimizationWeights::default();
        let scenario = ScenarioSpec::for_request(&ctx, CombatTier::Party, None, &weights);
        assert_eq!(scenario.objective_profile_id, None);
    }
    use super::{CombatKind, CombatTier, RoleObjective, ScenarioSpec, TargetProfile};
    use crate::balance::BalanceContext;
    use gw2_core::types::GameMode;

    #[test]
    fn scenario_from_balance_context_captures_mode_and_patch() {
        let ctx = BalanceContext::new(GameMode::WvW);
        let scenario = ScenarioSpec::from_balance_context(&ctx);
        assert_eq!(scenario.game_mode, GameMode::WvW);
        assert_eq!(scenario.combat_tier, CombatTier::Solo);
        assert_eq!(scenario.target_profile, TargetProfile::Single);
        assert_eq!(scenario.patch_id.as_deref(), Some(ctx.patch_id.as_str()));
        assert_eq!(scenario.objective_profile_id, None);
        assert_eq!(scenario.optimization_target.label, "WvW");
    }

    // RoleObjective profile mapping

    #[test]
    fn role_wvw_roamer_maps_to_correct_profile_and_tier() {
        assert_eq!(
            RoleObjective::WvWRoamer.profile_id(&GameMode::WvW),
            "WvW_Roamer"
        );
        assert_eq!(RoleObjective::WvWRoamer.combat_tier(), CombatTier::Solo);
    }

    #[test]
    fn role_wvw_zerg_dps_maps_to_squad_tier() {
        assert_eq!(
            RoleObjective::WvWZergDps.profile_id(&GameMode::WvW),
            "WvW_Zerg_DPS"
        );
        assert_eq!(RoleObjective::WvWZergDps.combat_tier(), CombatTier::Squad);
    }

    #[test]
    fn role_wvw_zerg_support_maps_to_squad_tier() {
        assert_eq!(
            RoleObjective::WvWZergSupport.profile_id(&GameMode::WvW),
            "WvW_Zerg_Support"
        );
        assert_eq!(
            RoleObjective::WvWZergSupport.combat_tier(),
            CombatTier::Squad
        );
    }

    #[test]
    fn role_wvw_disruptor_maps_to_solo_tier() {
        assert_eq!(
            RoleObjective::WvWDisruptor.profile_id(&GameMode::WvW),
            "WvW_Disruptor"
        );
        assert_eq!(RoleObjective::WvWDisruptor.combat_tier(), CombatTier::Solo);
    }

    #[test]
    fn role_pve_power_dps_profile_id() {
        assert_eq!(
            RoleObjective::PowerDps.profile_id(&GameMode::PvE),
            "PvE_Power_DPS"
        );
    }

    #[test]
    fn role_to_weights_wvw_roamer_differs_from_pve_power_dps() {
        let roamer_w = RoleObjective::WvWRoamer.to_weights(&GameMode::WvW);
        let pve_w = RoleObjective::PowerDps.to_weights(&GameMode::PvE);
        // Roamer should have meaningful sustain; PvE power DPS should be pure power
        assert!(
            roamer_w.sustain > pve_w.sustain,
            "WvW Roamer sustain ({}) should exceed PvE Power DPS sustain ({})",
            roamer_w.sustain,
            pve_w.sustain
        );
        assert!(
            pve_w.power > roamer_w.power,
            "PvE Power DPS power ({}) should exceed WvW Roamer power ({})",
            pve_w.power,
            roamer_w.power
        );
    }

    #[test]
    fn role_to_weights_uses_profile_data_not_defaults() {
        // WvW Roamer profile has specific weights in wvw.json — verify they loaded correctly
        let w = RoleObjective::WvWRoamer.to_weights(&GameMode::WvW);
        // From wvw.json WvW_Roamer: power=0.5, sustain=0.5, control=0.5
        assert!(
            (w.power - 0.5).abs() < 0.01,
            "WvW_Roamer power should be 0.5, got {}",
            w.power
        );
        assert!(
            (w.sustain - 0.5).abs() < 0.01,
            "WvW_Roamer sustain should be 0.5, got {}",
            w.sustain
        );
        assert!(
            (w.control - 0.5).abs() < 0.01,
            "WvW_Roamer control should be 0.5, got {}",
            w.control
        );
    }

    #[test]
    fn combat_tier_label_is_non_empty() {
        for tier in [CombatTier::Solo, CombatTier::Party, CombatTier::Squad] {
            assert!(!tier.label().is_empty());
        }
    }

    #[test]
    fn role_combat_kind_is_independent_of_scale() {
        assert_eq!(
            RoleObjective::WvWRoamer.combat_kind(),
            CombatKind::StrikeSpike
        );
        assert_eq!(
            RoleObjective::WvWZergSupport.combat_kind(),
            CombatKind::Support
        );
        assert_eq!(
            RoleObjective::WvWDisruptor.combat_kind(),
            CombatKind::Disabler
        );
        assert_eq!(RoleObjective::Tank.combat_kind(), CombatKind::Commander);
        assert_eq!(RoleObjective::CondiDps.combat_kind(), CombatKind::CondiRamp);
        assert_eq!(
            RoleObjective::WvWZergDps.combat_kind(),
            CombatKind::StrikeSpike
        );
    }

    #[test]
    fn roam_weights_pick_dive_trickster_or_bruiser_clock() {
        let dive = crate::scoring::OptimizationWeights {
            power: 0.8,
            condition: 0.2,
            sustain: 0.3,
            ..crate::scoring::OptimizationWeights::default()
        };
        assert_eq!(
            RoleObjective::WvWRoamer.combat_kind_for_weights(&dive),
            CombatKind::StrikeSpike
        );
        let trickster = crate::scoring::OptimizationWeights {
            power: 0.2,
            condition: 0.8,
            sustain: 0.3,
            ..crate::scoring::OptimizationWeights::default()
        };
        assert_eq!(
            RoleObjective::WvWRoamer.combat_kind_for_weights(&trickster),
            CombatKind::CondiRamp
        );
        let bruiser = crate::scoring::OptimizationWeights {
            power: 0.4,
            condition: 0.2,
            sustain: 0.9,
            ..crate::scoring::OptimizationWeights::default()
        };
        assert_eq!(
            RoleObjective::WvWRoamer.combat_kind_for_weights(&bruiser),
            CombatKind::Staller
        );
    }

    #[test]
    fn play_roles_are_shared_and_remap_per_mode() {
        assert_eq!(RoleObjective::PLAY_ROLES.len(), 7);
        assert_eq!(RoleObjective::PowerDps.play_label(), "Damage");
        assert_eq!(RoleObjective::Buffer.play_label(), "Support");
        assert_eq!(RoleObjective::Disabler.play_label(), "Disable");
        assert_eq!(RoleObjective::Healer.play_label(), "Heal");
        assert_eq!(RoleObjective::Staller.play_label(), "Troll");
        assert_eq!(RoleObjective::WvWRoamer.play_label(), "Roamer");
        assert_eq!(
            RoleObjective::Healer.profile_id(&GameMode::PvE),
            "PvE_Healer"
        );
        assert_eq!(RoleObjective::Healer.profile_id(&GameMode::WvW), "WvW_Heal");
        assert_eq!(
            RoleObjective::Healer.profile_id(&GameMode::PvP),
            "PvP_Sustain"
        );
        assert_eq!(
            RoleObjective::Hybrid.profile_id(&GameMode::WvW),
            "WvW_Hybrid"
        );
        assert_eq!(
            RoleObjective::Staller.profile_id(&GameMode::WvW),
            "WvW_Staller"
        );
        let pve = RoleObjective::Healer.to_weights(&GameMode::PvE);
        let wvw = RoleObjective::Healer.to_weights(&GameMode::WvW);
        assert!(
            (pve.healing - wvw.healing).abs() > 0.01 || (pve.sustain - wvw.sustain).abs() > 0.01,
            "Heal in PvE vs WvW should not share the same weight vector"
        );
    }

    /// The three modes offer different roles, and a mode must never offer
    /// one belonging to another: no PvE reference build is a Roamer or a
    /// Troll, and no source writes those words for PvE, so such a chip could
    /// never match a stored benchmark.
    #[test]
    fn each_mode_offers_only_roles_it_has() {
        for mode in [GameMode::PvE, GameMode::PvP, GameMode::WvW] {
            let roles = RoleObjective::play_roles_for(&mode);
            assert!(!roles.is_empty(), "{mode:?} must offer some role");
            let mut unique = roles.to_vec();
            unique.sort_by_key(|role| format!("{role:?}"));
            unique.dedup();
            assert_eq!(unique.len(), roles.len(), "{mode:?} lists a role twice");
        }

        let pve = RoleObjective::play_roles_for(&GameMode::PvE);
        for absent in [RoleObjective::WvWRoamer, RoleObjective::Staller] {
            assert!(
                !pve.contains(&absent),
                "{absent:?} is a WvW job and has no PvE reference build"
            );
        }
        for present in [RoleObjective::CondiDps, RoleObjective::Healer] {
            assert!(
                pve.contains(&present),
                "{present:?} is most of what PvE sources publish"
            );
        }
        assert!(
            !RoleObjective::play_roles_for(&GameMode::PvP).contains(&RoleObjective::Staller),
            "conquest has no troll job"
        );
    }

    #[test]
    fn play_roles_have_unique_weights_in_every_mode() {
        for mode in [GameMode::PvE, GameMode::PvP, GameMode::WvW] {
            for tier in [CombatTier::Solo, CombatTier::Party, CombatTier::Squad] {
                let mut seen: Vec<(RoleObjective, [i32; 6])> = Vec::new();
                for &role in RoleObjective::play_roles_for(&mode) {
                    let w = role.to_weights_for(&mode, tier);
                    let key = [
                        (w.power * 100.0).round() as i32,
                        (w.condition * 100.0).round() as i32,
                        (w.boon_support * 100.0).round() as i32,
                        (w.healing * 100.0).round() as i32,
                        (w.sustain * 100.0).round() as i32,
                        (w.control * 100.0).round() as i32,
                    ];
                    if let Some((other, _)) = seen.iter().find(|(_, k)| *k == key) {
                        panic!(
                            "{:?} and {:?} share weights {:?} in {:?} {:?}",
                            role, other, key, mode, tier
                        );
                    }
                    seen.push((role, key));
                }
            }
        }
    }

    #[test]
    fn wvw_support_splits_on_scale() {
        assert_eq!(
            RoleObjective::Buffer.profile_id_for(&GameMode::WvW, CombatTier::Solo),
            "WvW_Support"
        );
        assert_eq!(
            RoleObjective::Buffer.profile_id_for(&GameMode::WvW, CombatTier::Party),
            "WvW_Support"
        );
        assert_eq!(
            RoleObjective::Buffer.profile_id_for(&GameMode::WvW, CombatTier::Squad),
            "WvW_Zerg_Support"
        );
        let roam = RoleObjective::Buffer.to_weights_for(&GameMode::WvW, CombatTier::Solo);
        let zerg = RoleObjective::Buffer.to_weights_for(&GameMode::WvW, CombatTier::Squad);
        assert!(
            roam.power > zerg.power,
            "roam support must fight, not wet-noodle"
        );
        assert!(
            roam.control > zerg.control,
            "roam support must fend off focus"
        );
        assert!(zerg.boon_support > roam.boon_support || zerg.healing > roam.healing);
    }

    #[test]
    fn hybrid_and_troll_are_occupy_jobs_not_kills() {
        assert_eq!(RoleObjective::Hybrid.combat_kind(), CombatKind::Staller);
        assert_eq!(RoleObjective::Staller.combat_kind(), CombatKind::Staller);
        let hybrid = RoleObjective::Hybrid.to_weights(&GameMode::WvW);
        let troll = RoleObjective::Staller.to_weights(&GameMode::WvW);
        assert!(
            troll.power + troll.condition < hybrid.power + hybrid.condition,
            "troll should dump damage vs hybrid celestial"
        );
        assert!(troll.sustain >= 0.9, "troll sustain {}", troll.sustain);
    }
}
