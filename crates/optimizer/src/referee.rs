use crate::balance::BalanceContext;
use crate::combat::{self, CombatPerformance, DamageModifiers};
use crate::data::{DataQuality, DataQualityReason};
use crate::engine;
use crate::gamedb::GameDb;
use crate::rotation;
use crate::rotation::SimulationResult;
use crate::scenario::{CombatKind, CombatTier, ScenarioSpec};
use crate::scoring::{self, raw_direction_score, OptimizationWeights};
use crate::stats;
use crate::validation::ValidatedBuild;
use gw2_core::types::GameMode;

// Viability Gate Thresholds

/// Minimum stunbreak skills required for PvP/WvW viability. // HEURISTIC
const MIN_STUNBREAKS: u32 = 1;

/// Minimum cleanse count (distinct skills with cleanse) for PvP/WvW viability. // HEURISTIC
const MIN_CLEANSE_COUNT: u32 = 1;
const MIN_CLEANSE_RATE_PER_20S: f64 = 2.0;

/// Share of decisions the build may spend resource-starved before the bar
/// counts as unplayable. A decision is starved only when the pool left
/// nothing to press but the autoattack; wanting an unaffordable elite while
/// pressing something else is how every resource profession plays, and
/// opening a fight unable to pay is how they all start.
///
/// Measured over the 140 plated WvW references, starved-decision share:
/// 0.10 refuses 8 builds, 0.25 refuses 3, 0.50 and above refuse none. The
/// three at 0.25 are Revenant supports whose energy this ledger models
/// coarsely (no tablet, no upkeep toggling policy), so refusing them would
/// describe our model rather than the build. The gate's remaining teeth:
/// a cost the pool can never reach fails regardless of this ratio, and a
/// build starved for more than half the fight fails on it.
const MAX_RESOURCE_BLOCKED_RATIO: f64 = 0.50;

fn stunbreak_floor(profile: Option<&crate::data::ObjectiveProfile>) -> u32 {
    profile
        .and_then(|p| p.viability_gates.min_stunbreaks)
        .unwrap_or(MIN_STUNBREAKS)
}

fn cleanse_count_floor(profile: Option<&crate::data::ObjectiveProfile>) -> u32 {
    profile
        .and_then(|p| p.viability_gates.min_cleanse_count)
        .unwrap_or(MIN_CLEANSE_COUNT)
}

fn cleanse_rate_floor(
    scenario: &ScenarioSpec,
    profile: Option<&crate::data::ObjectiveProfile>,
) -> f64 {
    profile
        .and_then(|p| p.viability_gates.min_cleanse_rate_per_20s)
        .unwrap_or_else(|| required_cleanse_rate(scenario))
}

/// Minimum effective health for PvE viability.
/// Evidence: a glass Berserker Guardian (vit~1000, no toughness investment) computes
/// ~18,030 blended EHP (65% strike / 35% condition). A minimal test build with empty
/// gear (~1099) is far below any real build. This floor screens out obviously
/// under-geared or broken builds while passing all real ascended/exotic builds.
const EHP_FLOOR_PVE: f64 = 11_000.0;

/// EHP floor for WvW Roaming / Solo play.
/// Evidence: a Berserker Guardian (marauder variant) with Vitality gear reaches ~20,473
/// blended EHP; a Trailblazer Scourge reaches ~32,542. Glass Berserker Guardian solo
/// is ~18,030. Floor set to 15,000 — below any viable roaming gear set but high enough
/// to reject near-naked builds. Without a healer, you need this floor to survive burst.
pub const EHP_FLOOR_WVW_ROAM: f64 = 15_000.0;

/// EHP floor for WvW Havoc / small group play (5-15 players).
/// Evidence: small groups have a healer or support but you're still frequently 1-targeted.
/// Celestial Ele at ~18,680; glass Warrior at ~24,587. Floor set to 13,000 — accepts any
/// reasonable stat investment while rejecting pure paper builds.
pub const EHP_FLOOR_WVW_HAVOC: f64 = 13_000.0;

/// EHP floor for WvW Zerg / Squad play.
/// Evidence: zerg play has dedicated healers (Minstrel Firebrand ~24,866 EHP pre-healing).
/// Even a glass Berserker Warrior gets ~24,587 and is viable in a zerg. Floor set to 10,000
/// — loose enough that any remotely geared build passes; screens out completely naked builds.
pub const EHP_FLOOR_WVW_ZERG: f64 = 10_000.0;

/// Legacy alias kept for test backward-compatibility. Equals the havoc (party) floor. // HEURISTIC
pub const EHP_FLOOR_WVW: f64 = EHP_FLOOR_WVW_HAVOC;

/// EHP floor for sPvP / structured PvP. PvP uses amulet-based stat allocation
/// with a smaller total stat budget than ascended WvW gear, so EHP at level 80
/// is materially lower. Setting the floor to WvW levels would systematically
/// fail viable PvP builds and score them with the non-viable -1.0 sentinel.
///
/// Evidence: a Marauder amulet on a medium-armor profession with no toughness
/// investment lands ~12-14k blended EHP; tankier amulets (Cleric / Paladin)
/// hover at 18-22k. Floor at 8,000 rejects clearly broken builds while leaving
/// every real amulet/rune combo viable. // HEURISTIC
pub const EHP_FLOOR_PVP: f64 = 8_000.0;

// Viability Gate Types

/// Which gate a `GateResult` describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViabilityGate {
    /// Build must have ≥ MIN_STUNBREAKS stunbreak skills (WvW/PvP only).
    StunbreakCount,
    /// WvW/PvP: personal cover vs CC — Stability *or* evade/block/invuln/stealth.
    /// Roam also accepts interrupt/disable-first (cut their cast before yours).
    StabilityAccess,
    /// Damaging-condition cleanse rate/count floor (WvW/PvP only).
    /// Enum name kept as `CleanseRate`; Resistance must NOT discount this floor.
    CleanseRate,
    /// Soft-control answer for chill/weakness/slow/immobilize/blind/cripple
    /// (WvW/PvP only). Passes if the kit has cleanse OR Resistance OR a
    /// stunbreak (OR, not AND). Hard CC stays on `StunbreakCount`.
    ControlCoverage,
    /// Build effective health must meet the mode-specific EHP floor.
    EffectiveHealth,
    /// WvW roam (Solo): stealth, evade, block, or mobility to disengage a group.
    MobilityOut,
    /// Harasser: strip/steal/corrupt before dump (Stability strip-all + Protection).
    HarasserStrip,
    /// Harasser / PvP duel: the modeled target threshold was reached inside the clock.
    EncounterOutcome,
    /// Roam / harasser: the kit can interrupt the target's recovery action.
    SecureCompletion,
    /// WvW: an ordered offensive/control chain actually completed under
    /// continuous anti-interrupt cover for at least two seconds.
    ProtectedExecution,
    /// WvW: the player survived the modeled retaliation and can recover or
    /// repeat the exchange instead of winning only on a paper opener.
    SustainRecovery,
    /// WvW: priority actions obey the modeled profession-resource paywall.
    ResourceLegality,
    /// Profile `boon_uptime_floors` — only emitted when that map is non-empty.
    BoonUptime,
}

impl ViabilityGate {
    /// Whether failing this gate should refuse a build outright.
    ///
    /// Measured, not decided. `cargo run -p gw2-optimizer --example
    /// calibrate_viability` runs every synced community build through these
    /// gates, and a gate that the published meta fails is not describing the
    /// game — it is describing us. Rates on 582 builds, 2026-09-06:
    ///
    /// | gate | meta passes |
    /// |---|---|
    /// | MobilityOut | 100% |
    /// | EffectiveHealth | 96% |
    /// | StabilityAccess | 96% |
    /// | StunbreakCount | 88% |
    /// | SustainRecovery | 82% |
    /// | CleanseRate | 80% |
    /// | ResourceLegality | 77% |
    /// | SecureCompletion | 63% |
    /// | EncounterOutcome | 37% |
    /// | ProtectedExecution | 27% |
    /// | HarasserStrip | 11% |
    ///
    /// The top group is a floor real builds clear, so failing one is a real
    /// fault and blocks. The bottom four reject most of what people actually
    /// play — HarasserStrip refuses 89% of published roamers — so they are
    /// reported as concerns and do not veto. That is not leniency: a rule
    /// contradicted by the entire body of evidence it claims to describe has
    /// no authority to reject anything.
    ///
    /// This table is a measurement with a date on it. When a gate is fixed,
    /// rerun the harness and move it.
    pub fn blocks(&self) -> bool {
        !matches!(
            self,
            ViabilityGate::HarasserStrip
                | ViabilityGate::ProtectedExecution
                | ViabilityGate::EncounterOutcome
                | ViabilityGate::SecureCompletion
        )
    }
}

/// Result of a single viability gate check.
#[derive(Debug, Clone)]
pub struct GateResult {
    /// Which gate was checked.
    pub gate: ViabilityGate,
    /// Whether the gate passed. A skipped gate reports `true` so that every
    /// "list the failures" caller stays correct; read `skipped` to tell an
    /// abstention from a verdict.
    pub passed: bool,
    /// The gate did not judge this build: the simulator does not model what
    /// the gate reads, so it is neither a pass nor a fail and carries no
    /// weight in `is_viable` or `shortfall`. The note says what is missing.
    pub skipped: bool,
    /// Human-readable explanation (threshold, actual value, or reason for skip/fail).
    pub note: String,
}

/// Aggregate viability report produced by `evaluate_viability_gates`.
///
/// A build is viable if all gates pass. A single failure marks the build
/// non-viable and the referee assigns it a sentinel score of -1.0.
#[derive(Debug, Clone)]
pub struct ViabilityReport {
    /// Sum over failed gates of how far below threshold each sits, normalized
    /// to 0..=1 per gate (1.0 for gates with no graded metric). Zero when
    /// viable. `search_rank` cannot see a cleanse rate climbing 0.5 -> 3.9
    /// while it still fails; this can, so seed repair and the beam tie-break
    /// can climb a gate gradually instead of only on the flip.
    pub shortfall: f64,
    /// Results for each gate that was evaluated.
    pub gates: Vec<GateResult>,
    /// Whether every *blocking* gate passed - see [`ViabilityGate::blocks`].
    ///
    /// It used to mean every gate, full stop, and that made it a constant.
    /// Measured on the synced WvW corpus 2026-09-07: **0 of 124** published
    /// builds were viable under the old rule, 61 under this one. Nothing reads
    /// `is_viable` gently - a false collapses `user_intent_score`,
    /// `raw_direction_score` and `stat_direction_score` to the -1.0 sentinel
    /// and skips `simulate_flow` entirely - so the search's primary score was
    /// the same number for every build a real player has ever published,
    /// decided by gates the table in `blocks` already declares have no
    /// authority to refuse anything.
    ///
    /// The non-blocking failures are not discarded; they are the concerns
    /// written onto a served build.
    pub is_viable: bool,
}

/// Whether every gate with the authority to refuse a build passed.
///
/// A gate `blocks()` excludes still reports, still grades into `shortfall`,
/// and still becomes a caveat on a served build - it just cannot be the reason
/// a build scores -1.0.
fn gates_all_blocking_passed(gates: &[GateResult]) -> bool {
    gates
        .iter()
        .all(|g| g.passed || g.skipped || !g.gate.blocks())
}

impl ViabilityReport {
    /// Returns the first failing gate, if any. An abstention is not one.
    pub fn first_failure(&self) -> Option<&GateResult> {
        self.gates.iter().find(|g| !g.passed && !g.skipped)
    }

    /// Gates that could not judge this build, with their reasons.
    pub fn skipped_gates(&self) -> impl Iterator<Item = &GateResult> {
        self.gates.iter().filter(|g| g.skipped)
    }
}

/// WvW roam and stallers: rank fight DPS and escape kit, not paper zerg indices.
pub(crate) fn is_roam_objective(scenario: &ScenarioSpec) -> bool {
    needs_outcome_clock(scenario) || needs_mobility_out(scenario)
}

/// Whether `search_rank` ranks on the WvW counterplay timeline.
///
/// Every WvW scenario does. `is_roam_objective` answers a narrower question
/// -- does this build have to finish a kill or escape -- and it is false for
/// Support, Commander and Staller, and for any tier above Solo. Ranking off
/// it meant a WvW/Party/Support search compared builds on four trailing
/// zeros: the ally-facing keys (sequence, outcome, execution, tempo) never
/// ran for the one role that is made of them. PvP solo harassers keep the
/// timeline keys they already had.
fn ranks_on_the_wvw_timeline(scenario: &ScenarioSpec) -> bool {
    scenario.game_mode == GameMode::WvW || is_roam_objective(scenario)
}

/// Higher is better. WvW first requires a viable, completed exchange, then
/// honors the player's radar weights before ranking surplus role execution.
/// PvE/PvP: realized capped score, realized uncapped direction, then the
/// closed-form stat direction as the last word when output ties exactly.
pub fn search_rank(report: &RefereeReport) -> [i64; 10] {
    let viable = i64::from(report.viability.is_viable);
    // Key 1, above every measure of HOW WELL a build performs: a build that
    // delivers what another role exists for is the wrong build, not a lesser
    // one. Signed, so a build can sort below one that does nothing at all.
    // Unmeasurable (no profile, or the collapsed axes) sorts under every real
    // alignment, which only ever ties builds that already lost on key 0.
    //
    // Clamped at the floor: alignment says WHETHER a build serves the role,
    // not how well. Above the floor every on-intent build ties here and the
    // exchange keys and the player's radar decide. Unclamped, it outranked
    // both: a WvW Roam Damage run with Power 100 % served a Hearty tank over
    // a Dragon's/Marauder roamer because the profile's solo row also focuses
    // sustain, and sustain runs past 1.0 (2026-09-24, first bad 1.14.39).
    let alignment = report
        .intent_alignment
        .map(|a| (a.min(scoring::INTENT_ALIGNMENT_FLOOR) * 1_000_000.0).round() as i64)
        .unwrap_or(-2_000_000);
    let gates = report.viability.gates.iter().filter(|g| g.passed).count() as i64;
    if ranks_on_the_wvw_timeline(&report.scenario) {
        let rot = report.rotation.as_ref();
        let wvw = rot.and_then(|rotation| rotation.wvw.as_ref());
        let sequence = wvw
            .map(|fight| i64::from(fight.chain_completed))
            .unwrap_or(0);
        let outcome = wvw
            .map(|fight| match report.scenario.combat_kind {
                CombatKind::StrikeSpike | CombatKind::CondiRamp | CombatKind::Harasser => {
                    i64::from(fight.target_reached)
                }
                // Survival, not `repeatable`. This is the search's objective
                // key, ranked second only to viability, and `repeatable` is
                // reached by 0% of the published Support builds in the corpus
                // - so it was a constant zero, and every support build tied
                // on the key meant to separate them. Survival is reached by
                // about half of them, which is what a ranking key has to do.
                // Key 7 still carries `repeatable` as a tiebreak for the
                // kinds that do reach it.
                CombatKind::Disabler
                | CombatKind::Support
                | CombatKind::Commander
                | CombatKind::Staller => i64::from(fight.player_survived),
            })
            .unwrap_or(0);
        let execution = wvw
            .map(|fight| match report.scenario.combat_kind {
                CombatKind::StrikeSpike | CombatKind::Harasser => fight.peak_protected_damage_2s,
                CombatKind::CondiRamp => fight.protected_damage,
                CombatKind::Disabler => fight.control_landed_ms as f64 * 10.0,
                CombatKind::Support | CombatKind::Commander | CombatKind::Staller => {
                    // Sprint 3 (specs/007-trait-triggers): ally-facing output
                    // counts for the support kinds; the only rank-key change.
                    fight.sustain_margin.max(0.0) + fight.ally_boon_stack_seconds / 1_000.0
                }
            })
            .unwrap_or(0.0)
            .round() as i64;
        let repeatable = wvw.map(|fight| i64::from(fight.repeatable)).unwrap_or(0);
        let sustain = wvw
            .map(|fight| (fight.remaining_health_ratio * 100_000.0) as i64)
            .unwrap_or(0);
        let tempo = wvw
            .and_then(|fight| {
                fight
                    .target_reached_at_ms
                    .map(|at| fight.duration_ms.saturating_sub(at))
            })
            .unwrap_or(0) as i64;
        let intent = (report.user_intent_score * 1_000_000.0).round() as i64;
        let raw = (report.raw_direction_score * 1_000_000.0).round() as i64;
        [
            viable,
            alignment,
            gates,
            sequence,
            outcome,
            intent,
            execution,
            tempo,
            repeatable * 1_000_000 + sustain,
            raw,
        ]
    } else {
        let score = (report.user_intent_score * 1_000_000.0) as i64;
        let raw = (report.raw_direction_score * 1_000_000.0) as i64;
        let stats = (report.stat_direction_score * 1_000_000.0) as i64;
        [viable, alignment, gates, score, raw, stats, 0, 0, 0, 0]
    }
}

/// The "vs meta" meter's number: [`search_rank`] folded into one scalar so a
/// ratio of two of them means what the ranking means.
///
/// - radar: the capped, neglect-penalised radar score (`user_intent_score`
///   without the non-viability sentinel). Uncapped direction let a tank's
///   sustain past 1.0 read 144 % of a power reference (2026-09-24).
/// - checks: the share of the rank's pass/fail keys that passed - gates, and
///   on the WvW timeline the completed sequence and the landed outcome.
/// - role fit: alignment clamped at [`scoring::INTENT_ALIGNMENT_FLOOR`] as
///   in `search_rank`: at or above it adds nothing, below it subtracts.
pub fn meter_score(report: &RefereeReport, weights: &OptimizationWeights) -> f64 {
    let rank = search_rank(report);
    let judged = report
        .viability
        .gates
        .iter()
        .filter(|g| g.passed || !g.skipped)
        .count();
    let (passed, checks) = if ranks_on_the_wvw_timeline(&report.scenario) {
        (rank[2] + rank[3] + rank[4], judged + 2)
    } else {
        (rank[2], judged)
    };
    let checks_passed = if checks == 0 {
        1.0
    } else {
        passed as f64 / checks as f64
    };
    let radar = scoring::score_realized(&report.realized, weights).max(0.0);
    let role_fit = report
        .intent_alignment
        .map_or(0.0, |a| a.min(scoring::INTENT_ALIGNMENT_FLOOR));
    radar * checks_passed + role_fit
}

/// Failed-gate notes for the optimize error path.
pub fn viability_failure_summary(report: &ViabilityReport) -> String {
    let fails: Vec<&str> = report
        .gates
        .iter()
        .filter(|g| !g.passed)
        .map(|g| g.note.as_str())
        .collect();
    if fails.is_empty() {
        "unknown gate failure".into()
    } else {
        fails.join("; ")
    }
}

/// Evaluate all mode-appropriate viability gates for a build.
///
/// - WvW/PvP: StunbreakCount, StabilityAccess, CleanseRate, ControlCoverage, EffectiveHealth
/// - PvE: EffectiveHealth only (neither CleanseRate nor ControlCoverage)
///
/// When `rotation` is `None` (simulation unavailable), rotation-dependent
/// gates (StunbreakCount, StabilityAccess, CleanseRate, ControlCoverage) fail with
/// `note = "rotation unavailable"` in WvW/PvP. They are skipped entirely for PvE.
pub fn evaluate_viability_gates(
    rotation: Option<&SimulationResult>,
    combat_perf: &CombatPerformance,
    scenario: &ScenarioSpec,
) -> ViabilityReport {
    evaluate_viability_gates_for(rotation, combat_perf, scenario, None)
}

/// Same as [`evaluate_viability_gates`], applying profile `viability_gates`
/// when a field is set. Unset fields keep the hardcoded defaults
/// (`MIN_STUNBREAKS`, cover-or-stab, `MIN_CLEANSE_*`, mode/tier EHP).
/// `boon_uptime_floors` adds [`ViabilityGate::BoonUptime`] only when non-empty.
pub fn evaluate_viability_gates_for(
    rotation: Option<&SimulationResult>,
    combat_perf: &CombatPerformance,
    scenario: &ScenarioSpec,
    profile: Option<&crate::data::ObjectiveProfile>,
) -> ViabilityReport {
    let mut gates: Vec<GateResult> = Vec::new();
    // Graded distance below threshold, per gate that has a metric. Gates that
    // fail without a graded entry count 1.0 in the final pass below.
    let mut shortfall = 0.0f64;
    let mut graded: Vec<ViabilityGate> = Vec::new();

    let requires_pvp_gates = matches!(scenario.game_mode, GameMode::WvW | GameMode::PvP);
    let need_stunbreaks = stunbreak_floor(profile);
    let need_cleanses = cleanse_count_floor(profile);

    if requires_pvp_gates {
        // Stunbreak gate
        gates.push(match rotation {
            Some(rot) => {
                let passed = rot.stunbreak_count >= need_stunbreaks;
                if !passed {
                    shortfall += (need_stunbreaks.saturating_sub(rot.stunbreak_count)) as f64
                        / need_stunbreaks.max(1) as f64;
                    graded.push(ViabilityGate::StunbreakCount);
                }
                GateResult {
                    gate: ViabilityGate::StunbreakCount,
                    skipped: false,
                    passed,
                    note: format!(
                        "stunbreak_count={} (required >={})",
                        rot.stunbreak_count, need_stunbreaks
                    ),
                }
            }
            None => GateResult {
                gate: ViabilityGate::StunbreakCount,
                skipped: false,
                passed: false,
                note: "rotation unavailable".into(),
            },
        });

        // Cover, not Stability-only — unless the profile sets requires_stability.
        // None: stab OR cover OR (roam AND interrupt). Some(true): stab only.
        // Some(false): skip the gate.
        if profile.and_then(|p| p.viability_gates.requires_stability) != Some(false) {
            let require_stab =
                profile.and_then(|p| p.viability_gates.requires_stability) == Some(true);
            gates.push(match rotation {
                Some(rot) => {
                    let roam = scenario.combat_tier == CombatTier::Solo;
                    let passed = if require_stab {
                        rot.has_stability
                    } else {
                        rot.has_stability || rot.has_cover_answer || (roam && rot.has_interrupt)
                    };
                    let note = if rot.has_stability {
                        "stability available".into()
                    } else if require_stab {
                        "stability required by profile".into()
                    } else if rot.has_cover_answer {
                        "cover: evade/block/invuln/stealth".into()
                    } else if roam && rot.has_interrupt {
                        "interrupt/disable before incoming CC".into()
                    } else {
                        "no cover (stability, evade, block, invuln, stealth) and no interrupt"
                            .into()
                    };
                    GateResult {
                        gate: ViabilityGate::StabilityAccess,
                        skipped: false,
                        passed,
                        note,
                    }
                }
                None => GateResult {
                    gate: ViabilityGate::StabilityAccess,
                    skipped: false,
                    passed: false,
                    note: "rotation unavailable".into(),
                },
            });
        }

        // Cleanse gate
        gates.push(match rotation {
            Some(rot) => {
                let required_rate = effective_cleanse_requirement(scenario, rot, profile);
                // The rate supersedes the count. Sigils, runes, relics and
                // traits cleanse without occupying a skill slot, so a kit can
                // out-cleanse the floor with `cleanse_count == 0`; requiring
                // both refused builds that cleanse entirely off-bar.
                let passed = rot.cleanse_rate_per_20s >= required_rate;
                if !passed {
                    let rate_short =
                        ((required_rate - rot.cleanse_rate_per_20s) / required_rate).clamp(0.0, 1.0);
                    shortfall += rate_short;
                    graded.push(ViabilityGate::CleanseRate);
                }
                GateResult {
                    gate: ViabilityGate::CleanseRate,
                    skipped: false,
                    passed,
                    note: format!(
                        "cleanse_count={}, rate={:.1}/20s (required count >={}, rate >={required_rate:.1}/20s)",
                        rot.cleanse_count, rot.cleanse_rate_per_20s, need_cleanses
                    ),
                }
            }
            None => GateResult {
                gate: ViabilityGate::CleanseRate,
                skipped: false,
                passed: false,
                note: "rotation unavailable".into(),
            },
        });

        // Soft-control coverage: chill/weakness/slow/immobilize/blind/cripple.
        // Cleanse, Resistance, or stunbreak on the actual kit (OR, not AND).
        gates.push(match rotation {
            Some(rot) => {
                let has_cleanse = rot.cleanse_count > 0 || rot.cleanse_rate_per_20s > 0.0;
                let has_resistance =
                    rot.buff_uptime.get("Resistance").copied().unwrap_or(0.0) > 0.0;
                let has_stunbreak = rot.stunbreak_count > 0;
                let passed = has_cleanse || has_resistance || has_stunbreak;
                if !passed {
                    shortfall += 1.0;
                    graded.push(ViabilityGate::ControlCoverage);
                }
                let limb = if has_cleanse {
                    "cleanse"
                } else if has_resistance {
                    "Resistance"
                } else if has_stunbreak {
                    "stunbreak"
                } else {
                    "none"
                };
                GateResult {
                    gate: ViabilityGate::ControlCoverage,
                    skipped: false,
                    passed,
                    note: format!(
                        "soft-control coverage via {limb} (need cleanse OR Resistance OR stunbreak)"
                    ),
                }
            }
            None => GateResult {
                gate: ViabilityGate::ControlCoverage,
                skipped: false,
                passed: false,
                note: "rotation unavailable".into(),
            },
        });

        if scenario.game_mode == GameMode::WvW {
            gates.push(match rotation.and_then(|rotation| rotation.wvw.as_ref()) {
                Some(fight) => {
                    let damage_route = match scenario.combat_kind {
                        CombatKind::StrikeSpike => {
                            fight.target_reached
                                || fight.target_health.is_some_and(|hp| {
                                    fight.peak_protected_damage_2s >= hp * 0.30
                                })
                        }
                        CombatKind::CondiRamp => fight
                            .target_health
                            .is_some_and(|hp| fight.protected_damage >= hp * 0.20),
                        CombatKind::Harasser => {
                            fight.target_health.is_some_and(|hp| {
                                fight.peak_protected_damage_2s >= hp * 0.15
                            }) || fight.secured_sequence_control_ms >= 750
                        }
                        CombatKind::Disabler => fight.secured_sequence_control_ms >= 750,
                        CombatKind::Support | CombatKind::Commander | CombatKind::Staller => true,
                    };
                    let passed = fight.chain_completed && damage_route;
                    GateResult {
                        gate: ViabilityGate::ProtectedExecution,
                        skipped: false,
                        passed,
                        // `chain_completed` decides this gate — for a
                        // Support build it decides it alone, since the damage
                        // route is unconditional there — so it is named. It
                        // used to be omitted, leaving a note whose every
                        // number looked healthy above its stated minimum
                        // while the gate failed, which reads as a broken
                        // calculation rather than an unfinished chain.
                        note: format!(
                            "chain completed={}, damage route={}, protected={}ms, actions={}, 2s spike={:.0}, sequence control={}ms, interrupted={} (minimum {}ms secured inside the sequence)",
                            fight.chain_completed,
                            damage_route,
                            fight.longest_protected_window_ms,
                            fight.protected_action_count,
                            fight.peak_protected_damage_2s,
                            fight.secured_sequence_control_ms,
                            fight.interrupted_casts,
                            crate::rotation::wvw_timeline::MIN_PROTECTED_WINDOW_MS,
                        ),
                    }
                }
                None => GateResult {
                    gate: ViabilityGate::ProtectedExecution,
                    skipped: false,
                    passed: false,
                    note: "WvW counterplay timeline unavailable".into(),
                },
            });

            gates.push(match rotation.and_then(|rotation| rotation.wvw.as_ref()) {
                Some(fight) => {
                    // Survival, and only survival.
                    //
                    // This used to also demand `repeatable` of CondiRamp,
                    // Support, Commander and Staller. Measured on the synced
                    // corpus 2026-09-07, keyed on `combat_kind`, the clause
                    // has no support in the evidence at all:
                    //
                    // | kind | n | gate passes | repeatable |
                    // |---|---|---|---|
                    // | Support | 14 | 0% | 0% |
                    // | CondiRamp | 2 | 0% | 0% |
                    // | StrikeSpike | 23 | 100% | 4% |
                    // | Harasser | 85 | 93% | 28% |
                    //
                    // Every kind the clause applied to failed, every kind it
                    // did not apply to passed. `repeatable` is rare for
                    // everyone - it wants the exchange to end at half health,
                    // net-positive sustain, or a kill - and it was demanded
                    // only of the roles that never reach it.
                    //
                    // They never reach it because of the clock, not the
                    // build. `simulation_window_ms_for_mode` gives these
                    // kinds 20 s where StrikeSpike gets 5 s, and
                    // `WvwProfile::for_scenario` runs a constant-rate
                    // pressure loop for the whole of it, so a support soaks
                    // roughly 3.3x the total damage of a DPS - alone, with no
                    // allies modelled, which is the one thing a support's
                    // survival actually depends on. They pass
                    // `EffectiveHealth` 100%: the tankiest builds in the
                    // corpus, dying to the window.
                    let passed = fight.player_survived;
                    if !passed {
                        // Dead is dead; `remaining_health_ratio` is already 0
                        // here, so there is no gradient left to grade on.
                        shortfall += 1.0;
                        graded.push(ViabilityGate::SustainRecovery);
                    }
                    GateResult {
                        gate: ViabilityGate::SustainRecovery,
                        skipped: false,
                        passed,
                        note: format!(
                            "survived={}, health={:.0}%, margin={:+.0}/s, repeatable={}",
                            fight.player_survived,
                            fight.remaining_health_ratio * 100.0,
                            fight.sustain_margin,
                            fight.repeatable,
                        ),
                    }
                }
                None => GateResult {
                    gate: ViabilityGate::SustainRecovery,
                    skipped: false,
                    passed: false,
                    note: "WvW counterplay timeline unavailable".into(),
                },
            });

            gates.push(match rotation.and_then(|rotation| rotation.wvw.as_ref()) {
                // The ledger did not price this bar: either nothing was
                // simulated at all (Guardian virtues, Elementalist
                // attunements, Engineer toolbelt) or a skill on it spends
                // a resource no rule covers. Either way the gate has not
                // read enough to judge, so it abstains instead of handing
                // out a free pass or manufacturing a refusal. The ratio
                // and the unpayable check decide only on a complete model.
                Some(fight) if !fight.resource_model_complete => GateResult {
                    gate: ViabilityGate::ResourceLegality,
                    skipped: true,
                    passed: true,
                    note: format!(
                        "not simulated: {}",
                        if fight.resource_model_gaps.is_empty() {
                            format!("{} resource", fight.profession)
                        } else {
                            format!(
                                "{} ({})",
                                fight.resource_model_gaps.join(", "),
                                fight.profession
                            )
                        }
                    ),
                },
                Some(fight) => {
                    // Opening a fight unable to pay is how a resource
                    // profession starts; only sustained blocking, or a cost
                    // the pool can never reach, means the bar cannot be
                    // played.
                    let unpayable = !fight.resource_unpayable_skills.is_empty();
                    let passed =
                        !unpayable && fight.resource_blocked_ratio <= MAX_RESOURCE_BLOCKED_RATIO;
                    GateResult {
                        gate: ViabilityGate::ResourceLegality,
                        skipped: false,
                        passed,
                        note: if unpayable {
                            format!(
                                "cost above the resource cap, never castable: {}",
                                fight.resource_unpayable_skills.join(", ")
                            )
                        } else {
                            let mut note = format!(
                                "resource-blocked priority actions={} ({:.0}% of decisions, allowed <={:.0}%)",
                                fight.resource_blocked_actions,
                                fight.resource_blocked_ratio * 100.0,
                                MAX_RESOURCE_BLOCKED_RATIO * 100.0
                            );
                            // Never a silent pass: say what the ledger does
                            // not model, whether the gate passed or failed.
                            if !fight.resource_model_gaps.is_empty() {
                                note.push_str(&format!(
                                    "; resource model incomplete: {} not modelled",
                                    fight.resource_model_gaps.join(", ")
                                ));
                            }
                            note
                        },
                    }
                }
                None => GateResult {
                    gate: ViabilityGate::ResourceLegality,
                    skipped: false,
                    passed: false,
                    note: "WvW timeline unavailable".into(),
                },
            });
        }

        if needs_mobility_out(scenario) {
            let staller = scenario.combat_kind == CombatKind::Staller;
            gates.push(match rotation {
                Some(rot) => GateResult {
                    gate: ViabilityGate::MobilityOut,
                    skipped: false,
                    passed: rot.has_mobility_out,
                    note: if rot.has_mobility_out {
                        "escape kit present".into()
                    } else if staller {
                        "no stealth/evade/block/mobility — cannot evade a group".into()
                    } else {
                        "no stealth/evade/block/mobility — cannot disengage a group".into()
                    },
                },
                None => GateResult {
                    gate: ViabilityGate::MobilityOut,
                    skipped: false,
                    passed: false,
                    note: "rotation unavailable".into(),
                },
            });
        }

        if needs_harasser_strip(scenario) {
            gates.push(match rotation {
                Some(rot) => GateResult {
                    gate: ViabilityGate::HarasserStrip,
                    skipped: false,
                    passed: rot.has_strip,
                    note: if rot.has_strip {
                        "strip/steal/corrupt present".into()
                    } else {
                        "harasser/roam without cover-crack (strip/steal/corrupt)".into()
                    },
                },
                None => GateResult {
                    gate: ViabilityGate::HarasserStrip,
                    skipped: false,
                    passed: false,
                    note: "rotation unavailable".into(),
                },
            });
        }

        if needs_outcome_clock(scenario) {
            gates.push(match rotation {
                Some(rot) => {
                    let target_reached = if scenario.game_mode == GameMode::WvW {
                        rot.wvw.as_ref().is_some_and(|fight| fight.target_reached)
                    } else {
                        rot.downed
                    };
                    GateResult {
                        gate: ViabilityGate::EncounterOutcome,
                        skipped: false,
                        passed: target_reached,
                        note: if target_reached {
                            "target threshold reached in window".into()
                        } else {
                            "target threshold not reached by end of clock".into()
                        },
                    }
                }
                None => GateResult {
                    gate: ViabilityGate::EncounterOutcome,
                    skipped: false,
                    passed: false,
                    note: "rotation unavailable".into(),
                },
            });
            gates.push(match rotation {
                Some(rot) => GateResult {
                    gate: ViabilityGate::SecureCompletion,
                    skipped: false,
                    passed: rot.has_interrupt,
                    note: if rot.has_interrupt {
                        "interrupt available for the target's recovery action".into()
                    } else {
                        "no interrupt available for the target's recovery action".into()
                    },
                },
                None => GateResult {
                    gate: ViabilityGate::SecureCompletion,
                    skipped: false,
                    passed: false,
                    note: "rotation unavailable".into(),
                },
            });
        }
    }

    // Effective health gate (always runs)
    // WvW floor varies by combat tier: Roamers need more personal sustain than Zerg players.
    // PvP uses its own (lower) floor — amulet-based gear has a smaller stat budget than
    // ascended WvW, so reusing WvW floors here would non-viably score most real PvP builds.
    let default_ehp_floor = if scenario.combat_kind == CombatKind::Staller {
        match scenario.game_mode {
            GameMode::WvW => EHP_FLOOR_WVW_ROAM,
            GameMode::PvP => EHP_FLOOR_PVP,
            GameMode::PvE => EHP_FLOOR_PVE,
        }
    } else {
        match scenario.game_mode {
            GameMode::WvW => match scenario.combat_tier {
                crate::scenario::CombatTier::Solo => EHP_FLOOR_WVW_ROAM,
                crate::scenario::CombatTier::Party => EHP_FLOOR_WVW_HAVOC,
                crate::scenario::CombatTier::Squad => EHP_FLOOR_WVW_ZERG,
            },
            GameMode::PvP => EHP_FLOOR_PVP,
            GameMode::PvE => EHP_FLOOR_PVE,
        }
    };
    let ehp_floor = profile
        .and_then(|p| p.viability_gates.ehp_floor)
        .unwrap_or(default_ehp_floor);
    let passed = combat_perf.effective_health >= ehp_floor;
    if !passed && ehp_floor > 0.0 {
        shortfall += ((ehp_floor - combat_perf.effective_health) / ehp_floor).clamp(0.0, 1.0);
        graded.push(ViabilityGate::EffectiveHealth);
    }
    gates.push(GateResult {
        gate: ViabilityGate::EffectiveHealth,
        skipped: false,
        passed,
        note: format!(
            "effective_health={:.0} (required >={:.0})",
            combat_perf.effective_health, ehp_floor
        ),
    });

    let floors = profile
        .map(|p| &p.viability_gates.boon_uptime_floors)
        .filter(|m| !m.is_empty());
    if let Some(floors) = floors {
        gates.push(match rotation {
            Some(rot) => {
                let mut missed: Vec<String> = floors
                    .iter()
                    .filter_map(|(boon, floor)| {
                        let have = rot.buff_uptime.get(boon).copied().unwrap_or(0.0);
                        (have < *floor).then(|| format!("{boon}={have:.2}<{floor:.2}"))
                    })
                    .collect();
                missed.sort();
                let passed = missed.is_empty();
                if !passed {
                    shortfall += 1.0;
                    graded.push(ViabilityGate::BoonUptime);
                }
                GateResult {
                    gate: ViabilityGate::BoonUptime,
                    skipped: false,
                    passed,
                    note: if passed {
                        "boon uptime floors met".into()
                    } else {
                        format!("boon floors missed: {}", missed.join(", "))
                    },
                }
            }
            None => GateResult {
                gate: ViabilityGate::BoonUptime,
                skipped: false,
                passed: false,
                note: "rotation unavailable".into(),
            },
        });
    }

    let is_viable = gates_all_blocking_passed(&gates);
    for g in &gates {
        if !g.passed && !graded.contains(&g.gate) {
            shortfall += 1.0;
        }
    }
    ViabilityReport {
        gates,
        is_viable,
        shortfall,
    }
}

/// Cleanse rate the scenario demands per 20s. Sustained/support kinds need
/// double the floor. Shared by the gate and [`apply_offbar_cleanse`] so the
/// two cannot drift.
pub fn required_cleanse_rate(scenario: &ScenarioSpec) -> f64 {
    if matches!(
        scenario.combat_kind,
        CombatKind::CondiRamp | CombatKind::Support | CombatKind::Commander | CombatKind::Staller
    ) {
        MIN_CLEANSE_RATE_PER_20S * 2.0
    } else {
        MIN_CLEANSE_RATE_PER_20S
    }
}

/// Damaging-condition cleanse rate floor for `CleanseRate`.
///
/// Resistance must NOT discount this floor: Resistance ignores soft/control
/// conditions but damaging conditions still tick through it. Soft-control
/// answers live on [`ViabilityGate::ControlCoverage`]. Shared by the gate and
/// the off-bar pass. `_rot` retained so call sites stay stable.
pub fn effective_cleanse_requirement(
    scenario: &ScenarioSpec,
    _rot: &SimulationResult,
    profile: Option<&crate::data::ObjectiveProfile>,
) -> f64 {
    cleanse_rate_floor(scenario, profile)
}

/// Seconds after "cooldown" in gear/trait tooltip text ("(Cooldown: 9
/// Seconds)"). Skill facts use "recharge"; upgrades use "cooldown".
fn cooldown_seconds_in_text(text: &str) -> Option<f64> {
    let lower = text.to_lowercase();
    let idx = lower.find("cooldown")?;
    let rest = &lower[idx..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let n: f64 = digits.parse().ok()?;
    (n > 0.0).then_some(n)
}

/// Leading count before the remove/cleanse verb ("Remove 2 conditions").
/// Defaults to 1 when the text only says "a condition".
fn cleanse_count_in_text(text: &str) -> u32 {
    let lower = text.to_lowercase();
    let verb = ["remov", "cleanse", "cure", "purg", "transfer", "consum"]
        .iter()
        .filter_map(|v| lower.find(v))
        .min();
    let Some(v) = verb else { return 1 };
    let after = &lower[v..];
    let digits: String = after
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '.')
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().map(|n: u32| n.max(1)).unwrap_or(1)
}

/// Per-20s cleanse rate one piece of tooltip text contributes: count times
/// 20 / cooldown. Without a stated cooldown the trigger is on some event
/// (weapon swap, heal use) whose cadence is unknown here, so it is credited
/// at ONE cleanse per 20s — deliberately conservative.
fn cleanse_rate_from_text(text: &str) -> f64 {
    if !crate::text_util::text_describes_condition_cleanse(text) {
        return 0.0;
    }
    let count = cleanse_count_in_text(text) as f64;
    match cooldown_seconds_in_text(text) {
        Some(cd) => count * 20.0 / cd,
        None => count,
    }
}

fn item_cleanse_rate(db: &GameDb, id: u32) -> f64 {
    let Some(item) = db.items.get(&id) else {
        return 0.0;
    };
    let mut rate = cleanse_rate_from_text(&item.name);
    if let Some(d) = item.description.as_deref() {
        rate = rate.max(cleanse_rate_from_text(d));
    }
    if let Some(details) = &item.details {
        for b in &details.bonuses {
            rate = rate.max(cleanse_rate_from_text(b));
        }
        if let Some(d) = details
            .infix_upgrade
            .as_ref()
            .and_then(|u| u.buff.as_ref())
            .and_then(|b| b.description.as_deref())
        {
            rate = rate.max(cleanse_rate_from_text(d));
        }
    }
    rate
}

/// Cleanses per 20s the kit gets from OFF the skill bar: active sigils, the
/// rune, the relic, and traits. The rotation only sees skills, so Sigil of
/// Cleansing (1 condition per weapon swap, 9s ICD, ~2.2/20s on its own) and
/// every cleanse trait counted for nothing at the gate — a WvW-roam seed was
/// measured stuck at 1.0/20s against a required 2.0 with no way up. Every
/// profession runs through this; nothing here is class-specific.
pub fn kit_cleanse_rate_from_gear(validated: &ValidatedBuild, db: &GameDb) -> f64 {
    let reg = crate::data::cleanse_sources::registry();
    // Registry first (data/cleanse_sources.json); tooltip text only for ids
    // the registry does not know.
    let item_rate = |id: u32| match reg.item(id) {
        Some(s) => s.rate_per_20s(),
        None if reg.knows_item(id) => 0.0, // judged not to cleanse
        None => item_cleanse_rate(db, id),
    };
    let mut rate = 0.0;
    for id in validated.active_sigil_ids() {
        rate += item_rate(id);
    }
    if let Some(r) = &validated.rune {
        rate += item_rate(r.id);
    }
    if let Some(r) = &validated.relic {
        rate += item_rate(r.id);
    }
    if let Some(r) = &validated.food {
        rate += item_rate(r.id);
    }
    if let Some(r) = &validated.utility {
        rate += item_rate(r.id);
    }
    for spec in &validated.specializations {
        for &id in spec.all_trait_ids.iter().chain(spec.trait_ids.iter()) {
            if let Some(src) = reg.trait_(id) {
                rate += src.rate_per_20s();
            } else if reg.knows_trait(id) {
                // read by a cataloguer and judged not to cleanse
            } else if let Some(tr) = db.traits.get(&id) {
                let mut t = cleanse_rate_from_text(&tr.name);
                if let Some(d) = tr.description.as_deref() {
                    t = t.max(cleanse_rate_from_text(d));
                }
                rate += t;
            }
        }
    }
    rate
}

/// Re-judge the CleanseRate gate with off-bar cleanse added, keeping the
/// report's `shortfall` exact for the new total.
pub fn apply_offbar_cleanse(
    report: &mut ViabilityReport,
    rotation: Option<&SimulationResult>,
    validated: &ValidatedBuild,
    db: &GameDb,
    scenario: &ScenarioSpec,
    profile: Option<&crate::data::ObjectiveProfile>,
) {
    let Some(rot) = rotation else { return };
    let gear = kit_cleanse_rate_from_gear(validated, db);
    if gear <= 0.0 {
        return;
    }
    let required = effective_cleanse_requirement(scenario, rot, profile);
    let need = cleanse_count_floor(profile);
    // Mirrors the gate: the rate decides, the count is reporting only.
    let short = |rate: f64| ((required - rate) / required).clamp(0.0, 1.0);
    let before = rot.cleanse_rate_per_20s;
    let after = before + gear;
    let was_failing = before < required;
    let now_passes = after >= required;
    let mut changed = false;
    for g in &mut report.gates {
        if g.gate != ViabilityGate::CleanseRate {
            continue;
        }
        g.note = format!(
            "cleanse_count={}, rate={:.1}/20s incl. {:.1} from sigils/rune/relic/traits (required count >={}, rate >={required:.1}/20s)",
            rot.cleanse_count, after, gear, need
        );
        if was_failing {
            let prev = short(before);
            let next = if now_passes { 0.0 } else { short(after) };
            report.shortfall = (report.shortfall - prev + next).max(0.0);
        }
        if !g.passed && now_passes {
            g.passed = true;
            changed = true;
        }
    }
    if changed {
        report.is_viable = gates_all_blocking_passed(&report.gates);
    }
}

/// Relic, rune, or trait grants Stability even when the skill bar has none.
/// Thief/Daredevil kits often use Relic of the Cavalier for this.
pub fn kit_grants_stability(validated: &ValidatedBuild, db: &GameDb) -> bool {
    if let Some(r) = &validated.relic {
        let item = db.items.get(&r.id);
        let bonuses: &[String] = item
            .and_then(|i| i.details.as_ref())
            .map(|d| d.bonuses.as_slice())
            .unwrap_or(&[]);
        let desc = item.and_then(|i| {
            i.description.as_deref().or_else(|| {
                i.details
                    .as_ref()
                    .and_then(|d| d.infix_upgrade.as_ref())
                    .and_then(|u| u.buff.as_ref())
                    .and_then(|b| b.description.as_deref())
            })
        });
        if crate::text_util::gear_text_grants_stability(&r.name, desc, bonuses) {
            return true;
        }
    }
    if let Some(r) = &validated.rune {
        if let Some(item) = db.items.get(&r.id) {
            let bonuses = item
                .details
                .as_ref()
                .map(|d| d.bonuses.as_slice())
                .unwrap_or(&[]);
            let desc = item.description.as_deref().or_else(|| {
                item.details
                    .as_ref()
                    .and_then(|d| d.infix_upgrade.as_ref())
                    .and_then(|u| u.buff.as_ref())
                    .and_then(|b| b.description.as_deref())
            });
            if crate::text_util::gear_text_grants_stability(&item.name, desc, bonuses) {
                return true;
            }
        }
    }
    for spec in &validated.specializations {
        for &id in spec.all_trait_ids.iter().chain(spec.trait_ids.iter()) {
            if let Some(tr) = db.traits.get(&id) {
                if crate::text_util::text_describes_stability(&tr.name)
                    || tr
                        .description
                        .as_deref()
                        .is_some_and(crate::text_util::text_describes_stability)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Count relic/rune/trait Stability as passing the skill-bar gate.
pub fn apply_offbar_stability(
    report: &mut ViabilityReport,
    validated: &ValidatedBuild,
    db: &GameDb,
) {
    if !kit_grants_stability(validated, db) {
        return;
    }
    let mut changed = false;
    for g in &mut report.gates {
        if g.gate == ViabilityGate::StabilityAccess && !g.passed {
            g.passed = true;
            g.note = "stability from relic, rune, or trait".into();
            changed = true;
        }
    }
    if changed {
        report.is_viable = gates_all_blocking_passed(&report.gates);
        // StabilityAccess has no graded metric, so it counted 1.0 when failed.
        report.shortfall = (report.shortfall - 1.0).max(0.0);
    }
}

fn needs_mobility_out(scenario: &ScenarioSpec) -> bool {
    scenario.combat_kind == CombatKind::Staller
        || (scenario.game_mode == GameMode::WvW && scenario.combat_tier == CombatTier::Solo)
}

fn needs_harasser_strip(scenario: &ScenarioSpec) -> bool {
    scenario.combat_kind == CombatKind::Harasser
}

fn needs_outcome_clock(scenario: &ScenarioSpec) -> bool {
    if matches!(
        scenario.combat_kind,
        CombatKind::Support | CombatKind::Commander | CombatKind::Staller
    ) {
        return false;
    }
    scenario.combat_kind == CombatKind::Harasser
        || (scenario.game_mode == GameMode::PvP && scenario.combat_tier == CombatTier::Solo)
}

/// Deterministic build evaluation output.
///
/// The referee is the authority. Search strategies and AI advisors may generate
/// candidates, but they do not decide winners; this report does.
///
/// `viability` captures per-gate pass/fail results. When `viability.is_viable` is
/// `false`, `user_intent_score` is set to the sentinel value `-1.0` and the build
/// should be excluded from rankings.
#[derive(Debug, Clone)]
pub struct RefereeReport {
    pub scenario: ScenarioSpec,
    pub stats: stats::StatBlock,
    pub modifiers: DamageModifiers,
    pub combat_solo: CombatPerformance,
    pub combat_party: CombatPerformance,
    pub combat_squad: CombatPerformance,
    pub primary_combat: CombatPerformance,
    pub rotation: Option<rotation::SimulationResult>,
    /// Structured viability report: per-gate pass/fail with values and notes.
    pub viability: ViabilityReport,
    /// Final score for ranking. Set to `-1.0` (sentinel) when `viability.is_viable` is false.
    pub user_intent_score: f64,
    /// Uncapped radar-direction score — the final rank tie-break so
    /// post-saturation piece swaps toward the user's wished stats win ties
    /// that the capped `user_intent_score` cannot see.
    pub raw_direction_score: f64,
    /// Direction score over the axes that were actually MEASURED, not gated.
    ///
    /// Same formula as the non-sentinel branch of `raw_direction_score`, but
    /// it keeps its value when a blocking gate fails — so a refused build can
    /// still be RANKED against other refused builds. Only the
    /// [`evaluate_validated_build_ranked`] entry point pays for the flow
    /// simulation that makes this real; every other entry point sets it equal
    /// to `raw_direction_score`, sentinel included.
    pub ranked_direction_score: f64,
    /// Cosine between the player's weight vector and the realized axes.
    ///
    /// DIAGNOSTIC ONLY. Nothing gates on this: measured across the corpus the
    /// angle cannot separate a support build from a damage one, because every
    /// build sustains and that common-mode axis dominates the direction. The
    /// picks path, the meter and the serve invariant all use
    /// [`Self::intent_alignment`] against
    /// [`scoring::INTENT_ALIGNMENT_FLOOR`] instead. Kept because it is
    /// cheap and because a calibration run wants to see both numbers.
    ///
    /// `None` when the axes are the collapsed `realized_axes_no_rotation`
    /// fallback - a vector that is sustain and five zeroes has no direction.
    pub intent_similarity: Option<f64>,
    /// How well the measured axes serve the DIRECTION the role is written
    /// for: the focus axes delivered, minus what the role says to avoid.
    /// See [`scoring::intent_alignment`].
    ///
    /// The selection metric for the picks cards and the "vs meta" reference,
    /// and what the serve path should gate on. `None` when the axes are the
    /// collapsed no-rotation fallback, or when the scenario names no
    /// objective profile and its combat kind names none either - there is
    /// no direction to serve without one.
    pub intent_alignment: Option<f64>,
    /// Per-axis output of the 60s flow simulation, as fractions of the
    /// realized norms. This is what `user_intent_score` is computed from.
    pub realized: scoring::RealizedAxes,
    /// Uncapped closed-form radar direction of the stats alone. The last
    /// PvE/PvP rank key: it only speaks when realized output ties exactly,
    /// which a kit that produces nothing at all will do.
    pub stat_direction_score: f64,
    pub quality: DataQuality,
    pub quality_reasons: Vec<DataQualityReason>,
}

/// Look up the objective profile for `scenario`.
///
/// The named id wins. When no id was set -- references, tests and every
/// caller without a role chip -- the scenario's own combat kind, mode and
/// tier name the profile, so a WvW Support build is judged by
/// `WvW_Support`/`WvW_Zerg_Support` floors instead of the hardcoded ones.
/// Only an id that names nothing in the catalog still resolves to `None`.
fn objective_profile_for<'a>(
    scenario: &ScenarioSpec,
    catalog: &'a crate::data::ObjectiveProfileData,
) -> Option<&'a crate::data::ObjectiveProfile> {
    match scenario.objective_profile_id.as_deref() {
        Some(id) => catalog.profile_by_id(id),
        None => catalog.profile_by_id(
            scenario
                .combat_kind
                .role_objective()
                .profile_id_for(&scenario.game_mode, scenario.combat_tier),
        ),
    }
}

pub fn evaluate_validated_build(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> RefereeReport {
    evaluate_validated_build_with(validated, db, profession_name, weights, ctx, scenario, &[])
}

/// Evaluate a completed kit after the cheap consumable + infusion inner argmax.
///
/// Writes chosen food/utility/infusion-seat ids onto `validated` (locks are not
/// overwritten) and then ranks the exact kit. Search calls this after the kit is
/// complete and before the rank is retained. Exact-kit tests use
/// [`evaluate_validated_build`].
pub fn evaluate_validated_build_solved(
    validated: &mut ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
    locks: &gw2_core::types::BuildLocks,
) -> RefereeReport {
    crate::consumables::assign_best_consumables(
        validated,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
        locks,
    );
    crate::infusions::assign_best_infusions(
        validated,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
        locks,
    );
    evaluate_validated_build(validated, db, profession_name, weights, ctx, scenario)
}

/// Same referee, but the WvW timeline presses `opener` (skill ids, in
/// order) before improvising — the rotation a published page wrote, parsed
/// by `rotation::prose::parse_rotation`.
pub fn evaluate_validated_build_with(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
    opener: &[u32],
) -> RefereeReport {
    evaluate_inner(
        validated,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
        opener,
        false,
    )
}

/// Same referee, but the realized axes are measured even when a blocking gate
/// fails, so refused builds can be ranked against each other.
///
/// Costs one extra flow simulation per refused build (~150 µs vs ~60 µs), which
/// is why the search path does NOT use it. Ranking a published corpus does:
/// `realized_axes_no_rotation` fills only sustain, so every refused build came
/// back as the same vector and cosine ranked an artifact.
///
/// `user_intent_score` and `raw_direction_score` keep their `-1.0` sentinel;
/// read [`RefereeReport::ranked_direction_score`] for the measured number.
pub fn evaluate_validated_build_ranked(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> RefereeReport {
    evaluate_inner(
        validated,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
        &[],
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn evaluate_inner(
    validated: &ValidatedBuild,
    db: &GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
    opener: &[u32],
    always_realize: bool,
) -> RefereeReport {
    let (stats, modifiers) = engine::calculate_validated_stats(validated, db, profession_name, ctx);
    let derived = stats::compute_derived(&stats, profession_name);
    let buff_profiles = combat::buff_profiles_for_profession(profession_name, ctx);
    let condition_weights = combat::condition_weights_for_profession(profession_name, ctx);

    let combat_solo = combat::calculate_combat_performance(
        &stats,
        &derived,
        &modifiers,
        &buff_profiles[0],
        &condition_weights,
        profession_name,
        ctx,
    );
    let combat_party = combat::calculate_combat_performance(
        &stats,
        &derived,
        &modifiers,
        &buff_profiles[1],
        &condition_weights,
        profession_name,
        ctx,
    );
    let combat_squad = combat::calculate_combat_performance(
        &stats,
        &derived,
        &modifiers,
        &buff_profiles[2],
        &condition_weights,
        profession_name,
        ctx,
    );

    let primary_combat = match scenario.combat_tier {
        CombatTier::Solo => combat_solo.clone(),
        CombatTier::Party => combat_party.clone(),
        CombatTier::Squad => combat_squad.clone(),
    };
    let prepared = engine::prepare_validated_rotation(validated, db, &stats, Some(scenario)).map(
        |mut prepared| {
            prepared.opener = opener.to_vec();
            prepared
        },
    );
    let rotation = prepared
        .as_ref()
        .map(|p| engine::simulate_prepared(p, validated, db, Some(scenario)));
    // Viability gating
    // Run before score computation. Non-viable builds receive sentinel score -1.0.
    let profile = objective_profile_for(
        scenario,
        crate::data::objective_profiles::objective_profiles(),
    );
    let mut viability =
        evaluate_viability_gates_for(rotation.as_ref(), &primary_combat, scenario, profile);
    apply_offbar_stability(&mut viability, validated, db);
    apply_offbar_cleanse(
        &mut viability,
        rotation.as_ref(),
        validated,
        db,
        scenario,
        profile,
    );

    // The flow simulation is most of an evaluation's cost, and the search path
    // does not read the axes of a build the gates already sent to -1.0.
    // `always_realize` callers RANK refused builds, so they pay for it.
    let (realized, realized_from_flow) = match prepared.as_ref() {
        Some(p) if viability.is_viable || always_realize => (
            scoring::realized_axes(
                &engine::simulate_flow(p, weights, Some(scenario)),
                &primary_combat,
            ),
            true,
        ),
        _ => (scoring::realized_axes_no_rotation(&primary_combat), false),
    };
    // The angle only means something when the axes were measured. A collapsed
    // vector is sustain and five zeroes, and every build that produced no
    // rotation would share its direction.
    let intent_similarity =
        realized_from_flow.then(|| crate::picks::cosine(&realized.as_array(), &weights.as_array()));
    // The signed version: only the axes this role exists to deliver, minus
    // the ones it exists not to. Needs the profile, so a scenario built
    // without a role (references, tests) has no alignment to report.
    let intent_alignment = realized_from_flow
        .then(|| {
            objective_profile_for(
                scenario,
                crate::data::objective_profiles::objective_profiles(),
            )
            .map(|profile| scoring::intent_alignment(profile, scenario.combat_tier, &realized))
        })
        .flatten();
    // What the rotation produced, scheduled toward the radar. The closed-form
    // stat score could not see a skill at all (measured 2026-09-04: 0 of 36
    // utilities moved a PvE rank).
    let (user_intent_score, raw_direction_score, stat_direction_score) = if viability.is_viable {
        (
            scoring::score_realized(&realized, weights),
            scoring::raw_realized(&realized, weights),
            raw_direction_score(&primary_combat, weights),
        )
    } else {
        (-1.0, -1.0, -1.0)
    };
    // Measured, not gated: same formula, no sentinel. Equal to
    // `raw_direction_score` for every non-ranked caller.
    let ranked_direction_score = if always_realize {
        scoring::raw_realized(&realized, weights)
    } else {
        raw_direction_score
    };

    let mut quality = DataQuality::Verified;
    let mut quality_reasons = Vec::new();

    if !validated.warnings.is_empty() {
        quality = quality.merge(&DataQuality::Provisional);
        quality_reasons.extend(validated.warnings.iter().map(|warning| DataQualityReason {
            field: "validated_build.warning".into(),
            entity: profession_name.into(),
            modes: vec![ctx.game_mode.label().to_string()],
            explanation: warning.clone(),
        }));
    }

    if !validated.errors.is_empty() {
        quality = quality.merge(&DataQuality::Blocked);
        quality_reasons.extend(validated.errors.iter().map(|error| DataQualityReason {
            field: "validated_build.error".into(),
            entity: profession_name.into(),
            modes: vec![ctx.game_mode.label().to_string()],
            explanation: error.detail.clone(),
        }));
    }

    let honesty = rotation.as_ref().map(|result| {
        crate::data::quality::mode_honesty_reasons(
            profession_name,
            &ctx.game_mode,
            result
                .wvw
                .as_ref()
                .map(|fight| fight.unmodeled_sources.as_slice()),
            &result.honesty.unhosted,
            result.honesty.inventory_skipped,
            &result.honesty.heuristic,
        )
    });
    if let Some(reasons) = honesty {
        if !reasons.is_empty() {
            quality = quality.merge(&DataQuality::Provisional);
            quality_reasons.extend(reasons);
        }
    }
    if let Some(fight) = rotation.as_ref().and_then(|result| result.wvw.as_ref()) {
        if !fight.resource_model_complete {
            quality = quality.merge(&DataQuality::Provisional);
            quality_reasons.push(DataQualityReason {
                field: "wvw_timeline.resources".into(),
                entity: profession_name.into(),
                modes: vec![ctx.game_mode.label().to_string()],
                explanation: if fight.resource_simulated {
                    format!(
                        "resource model incomplete for {profession_name}: {} not modelled",
                        fight.resource_model_gaps.join(", ")
                    )
                } else {
                    format!(
                        "resource not simulated for {profession_name}: {} not modelled",
                        if fight.resource_model_gaps.is_empty() {
                            "the profession mechanic".to_string()
                        } else {
                            fight.resource_model_gaps.join(", ")
                        }
                    )
                },
            });
        }
        // A refused shroud entry is a rotation fact the player can act on,
        // not a data-quality downgrade.
        for refusal in &fight.shroud_refusals {
            quality_reasons.push(DataQualityReason {
                field: "wvw_timeline.resources".into(),
                entity: profession_name.into(),
                modes: vec![ctx.game_mode.label().to_string()],
                explanation: refusal.clone(),
            });
        }
    }
    engine::apply_build_fact_parse_drops(
        &mut quality,
        &mut quality_reasons,
        db,
        validated,
        profession_name,
        ctx.game_mode.label(),
    );

    RefereeReport {
        scenario: scenario.clone(),
        stats,
        modifiers,
        combat_solo,
        combat_party,
        combat_squad,
        primary_combat: primary_combat.clone(),
        rotation,
        viability,
        user_intent_score,
        raw_direction_score,
        ranked_direction_score,
        intent_similarity,
        intent_alignment,
        realized,
        stat_direction_score,
        quality,
        quality_reasons,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    // Intentional invariant tripwires.
    #![allow(clippy::assertions_on_constants)]
    use super::{
        effective_cleanse_requirement, evaluate_validated_build, evaluate_viability_gates,
        evaluate_viability_gates_for, required_cleanse_rate, search_rank, GateResult,
        RefereeReport, ViabilityGate, ViabilityReport, EHP_FLOOR_PVE, EHP_FLOOR_PVP,
        EHP_FLOOR_WVW_HAVOC, EHP_FLOOR_WVW_ROAM, EHP_FLOOR_WVW_ZERG,
    };
    use crate::balance::BalanceContext;
    use crate::combat::CombatPerformance;
    use crate::data::DataQuality;
    use crate::gamedb::GameDb;
    use crate::rotation::{wvw_timeline::WvwCombatReport, SimulationResult};
    use crate::scenario::{CombatTier, OptimizationTarget, ScenarioSpec, TargetProfile};
    use crate::scoring::OptimizationWeights;
    use crate::validation::{
        RejectCode, ValidatedBuild, ValidatedSkills, ValidatedSpec, ValidatedWeaponSet,
        ValidatedWeapons, ValidationReject,
    };
    use gw2_core::types::GameMode;
    use std::collections::HashMap;

    // Gate test helpers

    /// A `SimulationResult` that satisfies all WvW/PvP gates.
    pub(crate) fn make_viable_rotation() -> SimulationResult {
        SimulationResult {
            duration_ms: 20_000,
            strike_dps: 5_000.0,
            condition_dps: 1_000.0,
            total_dps: 6_000.0,
            condition_uptime: HashMap::new(),
            buff_uptime: HashMap::new(),
            skill_usage: vec![],
            stunbreak_count: 2,
            has_stability: true,
            stability_uptime: 0.6,
            cleanse_count: 2,
            cleanse_rate_per_20s: 4.0,
            healing_per_second: 0.0,
            control_uptime: 0.0,
            might_stacks_avg: 0.0,
            boon_equivalents: 0.0,
            has_mobility_out: true,
            escape_kinds: 1,
            has_strip: true,
            has_corrupt: false,
            downed: true,
            finished: true,
            has_interrupt: true,
            has_cover_answer: true,
            damage_per_second: Vec::new(),
            buff_presence_per_second: HashMap::new(),
            honesty: Default::default(),
            wvw: Some(WvwCombatReport {
                duration_ms: 5_000,
                target_health: Some(18_000.0),
                target_reached_at_ms: None,
                longest_protected_window_ms: 2_500,
                protected_action_count: 3,
                successful_action_count: 4,
                interrupted_casts: 0,
                protected_damage: 10_000.0,
                peak_protected_damage_2s: 8_000.0,
                peak_protected_damage_5s: 10_000.0,
                total_damage: 10_000.0,
                control_landed_ms: 1_000,
                incoming_damage: 2_000.0,
                avoided_damage: 2_000.0,
                healing: 2_000.0,
                barrier_absorbed: 0.0,
                conditions_cleansed: 2,
                combo_activations: 0,
                remaining_health_ratio: 0.9,
                sustain_margin: 400.0,
                player_survived: true,
                target_reached: true,
                chain_completed: true,
                secured_sequence_damage: 10_000.0,
                secured_sequence_control_ms: 1_000,
                repeatable: true,
                resource_blocked_actions: 0,
                resource_legal: true,
                resource_blocked_ratio: 0.0,
                resource_unpayable_skills: Vec::new(),
                resource_model_complete: true,
                resource_model_gaps: Vec::new(),
                resource_simulated: true,
                profession: "Warrior".into(),
                unmodeled_sources: Vec::new(),
                coverage: Vec::new(),
                cleave_damage: 0.0,
                cleave_condition_stack_seconds: 0.0,
                ally_boon_stack_seconds: 0.0,
                ally_healing: 0.0,
                ally_cleanses: 0,
                trait_fire_counts: std::collections::BTreeMap::new(),
                trace: Vec::new(),
                trace_truncated: false,
                proc_trials: Vec::new(),
                shroud_refusals: Vec::new(),
                dodge_count: 0,
                bus_on_dodge: 0,
                bus_on_disable_foe: 0,
                bus_on_attunement_swap: 0,
                bus_on_clone_created: 0,
            }),
        }
    }

    /// A `CombatPerformance` with sufficient effective health for WvW.
    /// A `CombatPerformance` with sufficient effective health for all WvW tiers including Roaming.
    fn make_viable_combat() -> CombatPerformance {
        CombatPerformance {
            // Use ROAM floor + buffer so this helper works across all WvW combat tiers.
            effective_health: EHP_FLOOR_WVW_ROAM + 1_000.0,
            ..CombatPerformance::default()
        }
    }

    fn sigil_item(id: u32, name: &str, bonus: &str) -> gw2_api::models::Item {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "type": "UpgradeComponent", "rarity": "Exotic",
            "level": 60, "details": { "type": "Sigil", "bonuses": [bonus] }
        }))
        .expect("test item")
    }

    /// Sigil of Cleansing: 1 condition per weapon swap, 9s ICD = 20/9 per 20s.
    /// A damage sigil contributes nothing. Rates add across seats.
    #[test]
    fn gear_cleanse_rate_reads_sigil_tooltips() {
        use super::{
            cleanse_count_in_text, cleanse_rate_from_text, cooldown_seconds_in_text,
            kit_cleanse_rate_from_gear,
        };
        use crate::validation::ValidatedItem;
        let mut db = GameDb::empty_for_tests();
        db.items.insert(1, sigil_item(1, "Superior Sigil of Cleansing",
            "Remove 1 condition when you swap to this weapon while in combat. (Cooldown: 9 Seconds)"));
        db.items
            .insert(2, sigil_item(2, "Superior Sigil of Force", "+5% Damage"));
        let mut b = ValidatedBuild {
            sigils: vec![
                ValidatedItem {
                    id: 1,
                    name: "Cleansing".into(),
                },
                ValidatedItem {
                    id: 2,
                    name: "Force".into(),
                },
            ],
            ..Default::default()
        };
        let rate = kit_cleanse_rate_from_gear(&b, &db);
        assert!((rate - 20.0 / 9.0).abs() < 1e-9, "got {rate}");
        b.sigils.push(ValidatedItem {
            id: 1,
            name: "Cleansing".into(),
        });
        // Only the two active seats count.
        assert!((kit_cleanse_rate_from_gear(&b, &db) - 20.0 / 9.0).abs() < 1e-9);
        assert_eq!(cleanse_count_in_text("Remove 2 conditions from allies"), 2);
        assert_eq!(cooldown_seconds_in_text("(Cooldown: 9 Seconds)"), Some(9.0));
        assert_eq!(cleanse_rate_from_text("Grants Might on hit"), 0.0);
    }

    /// The registry knows Superior Sigil of Cleansing (67340) even when the
    /// item is not in the database: 3 conditions per swap, 9 s cooldown.
    #[test]
    fn gear_cleanse_rate_reads_the_registry_first() {
        use super::kit_cleanse_rate_from_gear;
        use crate::validation::ValidatedItem;
        let db = GameDb::empty_for_tests();
        let b = ValidatedBuild {
            sigils: vec![ValidatedItem {
                id: 67340,
                name: "Cleansing".into(),
            }],
            ..Default::default()
        };
        let rate = kit_cleanse_rate_from_gear(&b, &db);
        assert!((rate - 3.0 * 20.0 / 9.0).abs() < 1e-9, "got {rate}");
    }

    /// A gate failing on rate alone must flip to passed once gear covers the
    /// gap, and the report's shortfall must return to exactly zero.
    #[test]
    fn offbar_cleanse_flips_the_gate_and_zeroes_shortfall() {
        use super::{apply_offbar_cleanse, effective_cleanse_requirement, MIN_CLEANSE_COUNT};
        use crate::validation::ValidatedItem;
        let mut db = GameDb::empty_for_tests();
        db.items.insert(1, sigil_item(1, "Superior Sigil of Cleansing",
            "Remove 1 condition when you swap to this weapon while in combat. (Cooldown: 9 Seconds)"));
        let b = ValidatedBuild {
            sigils: vec![ValidatedItem {
                id: 1,
                name: "Cleansing".into(),
            }],
            ..Default::default()
        };
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let mut rot = make_viable_rotation();
        rot.cleanse_count = MIN_CLEANSE_COUNT;
        // Same requirement the gate and the off-bar pass use (Resistance does not discount).
        let required = effective_cleanse_requirement(&scenario, &rot, None);
        rot.cleanse_rate_per_20s = required * 0.5; // fails on rate only
        let combat = make_viable_combat();
        let mut report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let cleanse = |r: &ViabilityReport| {
            r.gates
                .iter()
                .find(|g| g.gate == ViabilityGate::CleanseRate)
                .cloned()
                .unwrap()
        };
        assert!(!cleanse(&report).passed);
        assert!(
            (report.shortfall - 0.5).abs() < 1e-9,
            "rate shortfall is 0.5: {}",
            report.shortfall
        );
        apply_offbar_cleanse(&mut report, Some(&rot), &b, &db, &scenario, None);
        assert!(cleanse(&report).passed, "{}", cleanse(&report).note);
        assert!(report.is_viable);
        assert!(
            report.shortfall.abs() < 1e-9,
            "shortfall must be zero: {}",
            report.shortfall
        );
    }

    /// A kit that cleanses entirely off-bar (`cleanse_count == 0`) passes on
    /// its rate alone; a kit with neither bar skills nor gear still fails.
    #[test]
    fn offbar_cleanse_carries_a_bar_with_no_cleanse_skills() {
        use super::{apply_offbar_cleanse, effective_cleanse_requirement};
        use crate::validation::ValidatedItem;
        let mut db = GameDb::empty_for_tests();
        db.items.insert(1, sigil_item(1, "Superior Sigil of Cleansing",
            "Remove 1 condition when you swap to this weapon while in combat. (Cooldown: 9 Seconds)"));
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let mut rot = make_viable_rotation();
        rot.cleanse_count = 0;
        rot.cleanse_rate_per_20s = 0.0;
        let required = effective_cleanse_requirement(&scenario, &rot, None);
        let combat = make_viable_combat();
        let cleanse = |r: &ViabilityReport| {
            r.gates
                .iter()
                .find(|g| g.gate == ViabilityGate::CleanseRate)
                .cloned()
                .unwrap()
        };

        let mut with_sigil = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(!cleanse(&with_sigil).passed, "nothing cleanses yet");
        let geared = ValidatedBuild {
            sigils: vec![ValidatedItem {
                id: 1,
                name: "Cleansing".into(),
            }],
            ..Default::default()
        };
        assert!(
            super::kit_cleanse_rate_from_gear(&geared, &db) >= required,
            "fixture sigil must cover the floor"
        );
        apply_offbar_cleanse(&mut with_sigil, Some(&rot), &geared, &db, &scenario, None);
        assert!(cleanse(&with_sigil).passed, "{}", cleanse(&with_sigil).note);

        let mut bare = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        apply_offbar_cleanse(
            &mut bare,
            Some(&rot),
            &ValidatedBuild::default(),
            &db,
            &scenario,
            None,
        );
        assert!(
            !cleanse(&bare).passed,
            "no cleanse anywhere must still fail"
        );
    }

    /// One unaffordable opening burst is how a resource profession starts a
    /// fight; being blocked all fight, or carrying a cost above the pool's
    /// cap, is not.
    #[test]
    fn resource_legality_allows_ramp_and_refuses_sustained_blocking() {
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let combat = make_viable_combat();
        let gate = |ratio: f64, unpayable: Vec<String>| {
            let mut rot = make_viable_rotation();
            let fight = rot.wvw.as_mut().expect("wvw fixture");
            fight.resource_blocked_actions = 1;
            fight.resource_blocked_ratio = ratio;
            fight.resource_unpayable_skills = unpayable;
            evaluate_viability_gates(Some(&rot), &combat, &scenario)
                .gates
                .iter()
                .find(|g| g.gate == ViabilityGate::ResourceLegality)
                .cloned()
                .expect("resource gate")
        };

        assert!(gate(0.10, Vec::new()).passed, "one blocked opening burst");
        assert!(!gate(0.95, Vec::new()).passed, "blocked all fight");
        let never = gate(0.0, vec!["Deadly Blades".into()]);
        assert!(!never.passed, "cost above the cap can never be paid");
        assert!(never.note.contains("Deadly Blades"), "{}", never.note);
    }

    /// A scenario that never named a profile is still judged by the data
    /// floors its own combat kind, mode and tier imply. WvW Support resolves
    /// to `WvW_Support` at Roam/Havoc scale and `WvW_Zerg_Support` at Squad
    /// scale; references and tests used to fall back to hardcoded floors.
    #[test]
    fn an_unnamed_scenario_resolves_its_profile_from_the_combat_kind() {
        use super::objective_profile_for;
        use crate::scenario::{CombatKind, RoleObjective};
        let catalog = crate::data::objective_profiles::objective_profiles();

        let mut scenario = make_wvw_scenario();
        scenario.combat_kind = CombatKind::Support;
        scenario.objective_profile_id = None;

        scenario.combat_tier = CombatTier::Party;
        let party = objective_profile_for(&scenario, catalog).expect("WvW Support profile");
        assert_eq!(party.objective_profile_id, "WvW_Support");
        assert_eq!(
            RoleObjective::Buffer.profile_id_for(&scenario.game_mode, CombatTier::Party),
            "WvW_Support"
        );

        scenario.combat_tier = CombatTier::Squad;
        let squad = objective_profile_for(&scenario, catalog).expect("WvW zerg support profile");
        assert_eq!(squad.objective_profile_id, "WvW_Zerg_Support");

        // A named id still wins, and an id that names nothing is still None.
        scenario.objective_profile_id = Some("WvW_Roamer".into());
        assert_eq!(
            objective_profile_for(&scenario, catalog).map(|p| p.objective_profile_id.as_str()),
            Some("WvW_Roamer")
        );
        scenario.objective_profile_id = Some("not a profile".into());
        assert!(objective_profile_for(&scenario, catalog).is_none());
    }

    /// A profession whose resource this ledger never simulated gets an
    /// abstention, not a free pass: the gate is marked skipped, says whose
    /// resource is missing, and carries no weight either way.
    #[test]
    fn an_unsimulated_resource_skips_the_gate_instead_of_passing_it() {
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let combat = make_viable_combat();
        let mut rot = make_viable_rotation();
        {
            let fight = rot.wvw.as_mut().expect("wvw fixture");
            fight.resource_simulated = false;
            fight.resource_model_complete = false;
            fight.profession = "Guardian".into();
            fight.resource_model_gaps = vec!["virtues, tomes and pages".into()];
            // Even a bar the ledger would have refused stays unjudged.
            fight.resource_blocked_ratio = 1.0;
            fight.resource_unpayable_skills = vec!["Virtue of Justice".into()];
        }
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let gate = report
            .gates
            .iter()
            .find(|g| g.gate == ViabilityGate::ResourceLegality)
            .expect("the gate is still reported");
        assert!(gate.skipped, "neither pass nor fail: {}", gate.note);
        assert!(gate.note.contains("not simulated"), "{}", gate.note);
        assert!(
            gate.note.contains("virtues, tomes and pages") && gate.note.contains("Guardian"),
            "{}",
            gate.note
        );
        assert!(
            report.first_failure().is_none(),
            "an abstention is not a failure"
        );
        assert_eq!(report.skipped_gates().count(), 1);
        assert!(
            report.is_viable,
            "a skipped gate carries no weight toward viability"
        );
        assert!(
            report.shortfall.abs() < 1e-9,
            "and none toward shortfall: {}",
            report.shortfall
        );

        // A modelled bar is still judged, pass or fail, on the ratio.
        let judged = |ratio: f64| {
            let mut rot = make_viable_rotation();
            {
                let fight = rot.wvw.as_mut().expect("wvw fixture");
                fight.profession = "Revenant".into();
                fight.resource_blocked_ratio = ratio;
            }
            evaluate_viability_gates(Some(&rot), &combat, &scenario)
                .gates
                .iter()
                .find(|g| g.gate == ViabilityGate::ResourceLegality)
                .cloned()
                .expect("the gate")
        };
        let ok = judged(0.10);
        assert!(!ok.skipped && ok.passed, "{}", ok.note);
        let starved = judged(0.95);
        assert!(!starved.skipped && !starved.passed, "{}", starved.note);
    }

    pub(crate) fn make_rank_report(rotation: SimulationResult) -> RefereeReport {
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        RefereeReport {
            scenario,
            stats: crate::stats::StatBlock::default(),
            modifiers: crate::combat::DamageModifiers::default(),
            combat_solo: CombatPerformance::default(),
            combat_party: CombatPerformance::default(),
            combat_squad: CombatPerformance::default(),
            primary_combat: CombatPerformance::default(),
            rotation: Some(rotation),
            viability: ViabilityReport {
                gates: Vec::new(),
                is_viable: true,
                shortfall: 0.0,
            },
            user_intent_score: 0.0,
            raw_direction_score: -1.0,
            ranked_direction_score: -1.0,
            intent_similarity: None,
            intent_alignment: None,
            realized: Default::default(),
            stat_direction_score: -1.0,
            quality: DataQuality::Verified,
            quality_reasons: Vec::new(),
        }
    }

    /// A WvW support at party scale is ranked on the WvW timeline like
    /// every other WvW build. It used to fall through to the PvE branch
    /// because `is_roam_objective` is false for Support above Solo, so the
    /// four keys that describe an ally-facing fight were zeros.
    #[test]
    fn a_wvw_party_support_ranks_on_the_timeline_keys() {
        let mut rot = make_viable_rotation();
        {
            let fight = rot.wvw.as_mut().expect("wvw fixture");
            fight.chain_completed = true;
            fight.player_survived = true;
            fight.sustain_margin = 400.0;
            fight.ally_boon_stack_seconds = 12_000.0;
            fight.target_reached_at_ms = Some(6_000);
            fight.duration_ms = 20_000;
            fight.repeatable = true;
            fight.remaining_health_ratio = 0.9;
        }
        let mut report = make_rank_report(rot);
        report.scenario.combat_tier = CombatTier::Party;
        report.scenario.combat_kind = crate::scenario::CombatKind::Support;
        report.user_intent_score = 0.5;

        let rank = search_rank(&report);
        // Keys 3..=8 are the timeline keys: sequence, outcome, intent,
        // execution, tempo, repeatable+sustain. Key 9 is the raw direction
        // score, which this fixture leaves at the -1.0 sentinel.
        for key in 3..=8 {
            assert!(rank[key] > 0, "key {key} is a trailing zero: {rank:?}");
        }
        // Support ranks on surviving and on what it gave allies.
        assert_eq!(rank[4], 1, "survival is the outcome key for support");
        assert_eq!(rank[6], (400.0 + 12.0_f64).round() as i64);
    }

    #[test]
    fn roam_rank_prefers_completed_sequence_over_larger_uncovered_total() {
        let mut completed = make_viable_rotation();
        let completed_wvw = completed.wvw.as_mut().expect("WvW report");
        completed_wvw.target_reached = true;
        completed_wvw.target_reached_at_ms = Some(2_000);
        let protected_damage = completed_wvw.protected_damage;
        let protected_peak = completed_wvw.peak_protected_damage_2s;

        let mut uncovered = completed.clone();
        let uncovered_wvw = uncovered.wvw.as_mut().expect("WvW report");
        uncovered_wvw.chain_completed = false;
        uncovered_wvw.target_reached = false;
        uncovered_wvw.protected_damage = protected_damage * 2.0;
        uncovered_wvw.peak_protected_damage_2s = protected_peak * 2.0;

        assert!(
            search_rank(&make_rank_report(completed)) > search_rank(&make_rank_report(uncovered))
        );
    }

    /// Sprint 3 (specs/007-trait-triggers): two support builds identical but
    /// for one ally-facing boon record rank apart, the record above.
    #[test]
    fn support_builds_rank_apart_on_ally_trait() {
        let with = make_viable_rotation();
        let mut without = with.clone();
        let mut with = with;
        // The slot rounds to whole points: a Havoc minute of Might, Fury and
        // Protection on four allies is well past 1 000 stack-seconds.
        with.wvw
            .as_mut()
            .expect("WvW report")
            .ally_boon_stack_seconds = 1_000.0;
        without
            .wvw
            .as_mut()
            .expect("WvW report")
            .ally_boon_stack_seconds = 0.0;
        for kind in [
            crate::scenario::CombatKind::Support,
            crate::scenario::CombatKind::Commander,
            crate::scenario::CombatKind::Staller,
        ] {
            let mut a = make_rank_report(with.clone());
            a.scenario.combat_kind = kind;
            let mut b = make_rank_report(without.clone());
            b.scenario.combat_kind = kind;
            assert!(search_rank(&a) > search_rank(&b), "{kind:?}");
        }
    }

    /// The same record leaves two damage builds' order alone.
    #[test]
    fn damage_builds_unchanged_by_ally_slot() {
        let mut with = make_viable_rotation();
        with.wvw
            .as_mut()
            .expect("WvW report")
            .ally_boon_stack_seconds = 1_000.0;
        let without = make_viable_rotation();
        for kind in [
            crate::scenario::CombatKind::StrikeSpike,
            crate::scenario::CombatKind::CondiRamp,
            crate::scenario::CombatKind::Harasser,
            crate::scenario::CombatKind::Disabler,
        ] {
            let mut a = make_rank_report(with.clone());
            a.scenario.combat_kind = kind;
            let mut b = make_rank_report(without.clone());
            b.scenario.combat_kind = kind;
            assert_eq!(search_rank(&a), search_rank(&b), "{kind:?}");
        }
    }

    // ---- Sprint 3 (specs/007-trait-triggers, US3): cache- and network-backed
    // checks. All `#[ignore]`; they read `gw2_api::dev_config`.

    /// Professions whose trait catalogue increment has shipped: every one of
    /// their traits must have a state other than `NoRecord`.
    const SHIPPED_PROFESSIONS: [&str; 9] = [
        "Necromancer",
        "Ranger",
        "Thief",
        "Warrior",
        "Guardian",
        "Engineer",
        "Elementalist",
        "Mesmer",
        "Revenant",
    ];

    fn cache_db() -> Option<(std::path::PathBuf, GameDb)> {
        let Ok(cache_dir) = gw2_api::dev_config::cache_dir() else {
            println!("no dev.cfg: nothing to check");
            return None;
        };
        let cache = gw2_api::cache::DataCache::new(cache_dir.clone());
        let db = GameDb::load(&cache).expect("the cache holds a full GameDb");
        Some((cache_dir, db))
    }

    /// Derived state of one trait for the coverage table.
    fn trait_state(db: &GameDb, trait_id: u32, ctx: &BalanceContext) -> (String, String) {
        use crate::data::normalized_effects::{CoverageClass, SourceType};
        use gw2_api::models::Fact;
        let effects = crate::data::normalized_effects::effects().effects_for_mode("WvW");
        let records: Vec<_> = effects
            .iter()
            .filter(|e| e.source_type == SourceType::Trait && e.source_id == trait_id)
            .collect();
        let source = records
            .iter()
            .find_map(|e| e.source.clone())
            .unwrap_or_default();
        let facts = db.traits.get(&trait_id).is_some_and(|t| {
            let stat = |f: &Fact| {
                matches!(
                    f,
                    Fact::AttributeAdjust { .. } | Fact::BuffConversion { .. }
                )
            };
            t.facts.iter().any(stat)
                || t.traited_facts.iter().any(|tf| stat(&tf.fact))
                || !crate::combat::extract_damage_modifiers(
                    &[trait_id],
                    None,
                    &[],
                    None,
                    &db.traits,
                    &db.items,
                    ctx,
                )
                .consumed_trait_ids
                .is_empty()
        });
        let executable = records
            .iter()
            .any(|e| e.coverage.is_none() && e.value.is_resolved());
        let unresolved = records
            .iter()
            .any(|e| e.coverage.is_none() && !e.value.is_resolved());
        let class = records
            .iter()
            .find_map(|e| e.coverage.as_ref())
            .map(|c| match c.class {
                CoverageClass::PassiveNoEffect => "PassiveNoEffect".to_string(),
                CoverageClass::NeedsMechanic => {
                    format!("NeedsMechanic: {}", c.mechanic.clone().unwrap_or_default())
                }
            });
        let state = match (facts, executable, unresolved, class) {
            (true, true, _, _) => "facts+record".to_string(),
            (false, true, _, _) => "record".to_string(),
            (_, false, true, _) => "UnresolvedValue".to_string(),
            (_, false, false, Some(class)) => class,
            (true, false, false, None) => "facts".to_string(),
            (false, false, false, None) => "NoRecord".to_string(),
        };
        (state, source)
    }

    /// T052: every trait in the cache gets a row and a derived state;
    /// `docs/audit/trait-coverage.md` is regenerated; a shipped profession
    /// with `NoRecord > 0` fails.
    #[test]
    #[ignore]
    fn trait_coverage_audit_lists_every_trait() {
        let Some((cache_dir, db)) = cache_db() else {
            return;
        };
        let build: u64 = std::fs::read_to_string(cache_dir.join("traits.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["build"].as_u64())
            .unwrap_or(0);
        let ctx = BalanceContext::new(GameMode::WvW);
        let mut specs: Vec<_> = db.specializations.values().collect();
        specs.sort_by(|a, b| {
            (a.profession.as_str(), a.elite, &a.name).cmp(&(
                b.profession.as_str(),
                b.elite,
                &b.name,
            ))
        });
        let mut professions: Vec<String> = specs.iter().map(|s| s.profession.clone()).collect();
        professions.dedup();

        let columns = [
            "facts",
            "record",
            "facts+record",
            "PassiveNoEffect",
            "NeedsMechanic",
            "NoRecord",
            "UnresolvedValue",
        ];
        let mut summary = String::new();
        let mut sections = String::new();
        let mut total_rows = 0usize;
        for profession in &professions {
            let mut counts = std::collections::BTreeMap::new();
            let mut rows = String::new();
            let mut traits_seen = 0usize;
            for spec in specs.iter().filter(|s| &s.profession == profession) {
                let mut ids: Vec<u32> = spec
                    .minor_traits
                    .iter()
                    .chain(spec.major_traits.iter())
                    .copied()
                    .collect();
                ids.sort_by_key(|id| {
                    db.traits
                        .get(id)
                        .map(|t| (t.tier, t.order))
                        .unwrap_or((99, 99))
                });
                for id in ids {
                    let Some(t) = db.traits.get(&id) else {
                        continue;
                    };
                    let (state, source) = trait_state(&db, id, &ctx);
                    let key = state.split(':').next().unwrap_or("").to_string();
                    *counts.entry(key).or_insert(0usize) += 1;
                    rows.push_str(&format!(
                        "| {} | {} | {} | {} | {} |\n",
                        spec.name, id, t.name, state, source
                    ));
                    traits_seen += 1;
                }
            }
            total_rows += traits_seen;
            summary.push_str(&format!("| {profession} | {traits_seen} |"));
            for column in columns {
                summary.push_str(&format!(" {} |", counts.get(column).copied().unwrap_or(0)));
            }
            summary.push('\n');
            sections.push_str(&format!(
                "\n## {profession}\n\n| Line | Id | Trait | State | Source |\n|---|---|---|---|---|\n{rows}"
            ));
            let no_record = counts.get("NoRecord").copied().unwrap_or(0);
            if SHIPPED_PROFESSIONS.contains(&profession.as_str()) {
                assert_eq!(
                    no_record, 0,
                    "{profession} shipped its increment: NoRecord must be 0\n{rows}"
                );
            }
        }
        // Every trait a specialization line lists has a row; the cache also
        // holds traits no line references (retired or mechanic-only ids).
        let listed: usize = specs
            .iter()
            .map(|s| s.minor_traits.len() + s.major_traits.len())
            .sum();
        assert_eq!(total_rows, listed, "every listed trait has a row");
        println!(
            "{} cached traits are on no specialization line",
            db.traits.len().saturating_sub(listed)
        );
        let table = format!(
            "# Trait coverage — generated 2026-09-08 from cache build {build}\n\n\
             | Profession | Traits | facts | record | facts+record | PassiveNoEffect | NeedsMechanic | NoRecord | UnresolvedValue |\n\
             |---|---|---|---|---|---|---|---|---|\n{summary}{sections}"
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/audit/trait-coverage.md");
        std::fs::write(&path, table).expect("write docs/audit/trait-coverage.md");
        println!("wrote {} ({} rows)\n{summary}", path.display(), total_rows);
    }

    /// T053: every factual number of every wiki-sourced record appears in
    /// its page's wikitext (the rendered page shows the PvE column and hides
    /// the competitive facts behind mode tabs; the wikitext carries every
    /// `game mode=wvw` fact). Needs `wiki_api` in dev.cfg (the MediaWiki
    /// `api.php` URL); prints one line per record, commits no text.
    #[test]
    #[ignore]
    fn records_match_their_wiki_pages() {
        use crate::data::quality::FactualValue;
        let Ok(cfg) = gw2_api::dev_config::load() else {
            println!("no dev.cfg: nothing to check");
            return;
        };
        let Some(wiki_api) = cfg.get("wiki_api") else {
            println!("no wiki_api in dev.cfg: skipping the wiki-number check");
            return;
        };
        let client = reqwest::blocking::Client::builder()
            .user_agent("gw2-build-optimizer trait audit")
            .build()
            .expect("client");
        // The game's own facts (cached API) count as a source beside the
        // page: Spiteful Fortitude's 50 % threshold is an API fact the page
        // leaves to the tooltip.
        let db = cache_db().map(|(_, db)| db);
        let api_numbers =
            |source_type: &crate::data::normalized_effects::SourceType, id: u32| -> Vec<f64> {
                use crate::data::normalized_effects::SourceType;
                use gw2_api::models::Fact;
                let Some(db) = &db else {
                    return Vec::new();
                };
                let facts = match source_type {
                    SourceType::Trait => db.traits.get(&id).map(|t| &t.facts),
                    SourceType::Skill => db.skills.get(&id).map(|s| &s.facts),
                    _ => None,
                };
                facts
                    .into_iter()
                    .flatten()
                    .flat_map(|fact| match fact {
                        Fact::Percent { percent, .. } => vec![percent.unwrap_or(0.0)],
                        Fact::Number { value, .. } => vec![value.unwrap_or(0) as f64],
                        Fact::Recharge { value, .. } => vec![value.unwrap_or(0.0)],
                        Fact::Time { duration, .. } => vec![duration.unwrap_or(0) as f64],
                        Fact::AttributeAdjust { value, .. } => vec![value.unwrap_or(0) as f64],
                        Fact::Buff {
                            duration,
                            apply_count,
                            ..
                        }
                        | Fact::PrefixedBuff {
                            duration,
                            apply_count,
                            ..
                        } => vec![
                            duration.unwrap_or(0) as f64,
                            apply_count.unwrap_or(0) as f64,
                        ],
                        _ => Vec::new(),
                    })
                    .collect()
            };
        let mut pages: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut mismatches = 0usize;
        for effect in crate::data::normalized_effects::effects().effects_for_mode("WvW") {
            let Some(source) = effect.source.as_deref() else {
                continue;
            };
            let Some(url) = source
                .split(" (")
                .next()
                .filter(|u| u.starts_with("https://wiki.guildwars2.com/"))
            else {
                continue;
            };
            let text = pages.entry(url.to_string()).or_insert_with(|| {
                // The source URL is percent-encoded; the API wants the title.
                let title = url
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .replace("%27", "'")
                    .replace('_', " ");
                client
                    .get(wiki_api)
                    .query(&[
                        ("action", "parse"),
                        ("prop", "wikitext"),
                        ("format", "json"),
                        ("page", title.as_str()),
                    ])
                    .send()
                    .and_then(|r| r.json::<serde_json::Value>())
                    .map(|v| {
                        v["parse"]["wikitext"]["*"]
                            .as_str()
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_else(|e| format!("FETCH FAILED: {e}"))
            });
            // A heuristic record's numbers are estimates, not page claims
            // (Sprint 2's Path of Corruption cooldown).
            if effect.evidence_level != crate::data::EvidenceLevel::Factual {
                println!(
                    "skip {} {} ({:?} evidence)",
                    effect.effect_id, effect.source_name, effect.evidence_level
                );
                continue;
            }
            let mut numbers: Vec<f64> = Vec::new();
            let push = |numbers: &mut Vec<f64>, v: &FactualValue<f64>| {
                if let FactualValue::Resolved(x) = v {
                    numbers.push(*x);
                }
            };
            // A status record's value mirrors its stack count; one stack is
            // implicit on the page. A derived value is checked through the
            // page numbers it comes from.
            let single_status = effect.status_operation.is_some()
                && matches!(effect.value, FactualValue::Resolved(v) if v == 1.0);
            if !effect.derived_from.is_empty() {
                numbers.extend(effect.derived_from.iter().copied());
            } else if !single_status {
                push(&mut numbers, &effect.value);
            }
            if let Some(d) = &effect.effect_duration {
                push(&mut numbers, d);
            }
            if let Some(d) = &effect.internal_cooldown {
                push(&mut numbers, d);
            }
            if let Some(c) = &effect.healing_power_coefficient {
                push(&mut numbers, c);
            }
            if let Some(op) = &effect.status_operation {
                // A single stack or count is implicit on the page.
                if !matches!(op.amount_value, FactualValue::Resolved(v) if v == 1.0) {
                    push(&mut numbers, &op.amount_value);
                }
                if let Some(FactualValue::Resolved(ms)) = &op.base_duration_ms {
                    numbers.push(*ms as f64 / 1_000.0);
                }
                if let Some(FactualValue::Resolved(n)) = &op.target_count {
                    numbers.push(*n as f64);
                }
            }
            if let Some(gate) = effect
                .prerequisite
                .as_ref()
                .and_then(|p| p.foe_health.as_ref())
            {
                push(&mut numbers, &gate.percent);
            }
            let mut tokens: Vec<f64> = text
                .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .filter_map(|t| t.trim_matches('.').parse::<f64>().ok())
                .collect();
            tokens.extend(api_numbers(&effect.source_type, effect.source_id));
            let missing: Vec<String> = numbers
                .iter()
                .filter(|n| !tokens.iter().any(|t| (t - **n).abs() < 1e-9))
                .map(|n| n.to_string())
                .collect();
            if missing.is_empty() {
                println!("ok {} {}", effect.effect_id, effect.source_name);
            } else {
                mismatches += 1;
                println!(
                    "MISMATCH {} {}: {} not on page",
                    effect.effect_id,
                    effect.source_name,
                    missing.join(", ")
                );
            }
        }
        println!("{mismatches} mismatches");
    }

    /// The cached Reaper build (first Necromancer tab running Reaper) through
    /// the real database: returns its trait ids and the traced WvW report.
    fn cached_reaper_traced(
        db: &GameDb,
        cache_dir: &std::path::Path,
    ) -> Option<(
        Vec<u32>,
        crate::rotation::wvw_timeline::WvwCombatReport,
        RefereeReport,
    )> {
        let reaper_spec = db
            .specializations
            .values()
            .find(|s| s.name == "Reaper" && s.profession == "Necromancer")
            .map(|s| s.id)?;
        let mut found: Option<serde_json::Value> = None;
        for entry in std::fs::read_dir(cache_dir).ok()? {
            let path = entry.ok()?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if !name.starts_with("char_") || !name.ends_with("_buildtabs.json") {
                continue;
            }
            let tabs: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
            for tab in tabs.as_array()? {
                let build = &tab["build"];
                if build["profession"] == "Necromancer"
                    && build["specializations"]
                        .as_array()
                        .is_some_and(|specs| specs.iter().any(|s| s["id"] == reaper_spec))
                {
                    found = Some(build.clone());
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let build = found?;
        let trait_ids: Vec<u32> = build["specializations"]
            .as_array()?
            .iter()
            .flat_map(|s| s["traits"].as_array().cloned().unwrap_or_default())
            .filter_map(|t| t.as_u64().map(|t| t as u32))
            .collect();
        let specs: Vec<serde_json::Value> = build["specializations"]
            .as_array()?
            .iter()
            .map(|s| {
                let name = db
                    .specializations
                    .get(&(s["id"].as_u64().unwrap_or(0) as u32))
                    .map(|sp| sp.name.clone())
                    .unwrap_or_default();
                let traits: Vec<String> = s["traits"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|t| {
                        t.as_u64()
                            .and_then(|id| db.traits.get(&(id as u32)))
                            .map(|t| t.name.clone())
                    })
                    .collect();
                serde_json::json!({ "name": name, "traits": traits })
            })
            .collect();
        let skill_name = |v: &serde_json::Value| {
            v.as_u64()
                .and_then(|id| db.skills.get(&(id as u32)))
                .map(|s| s.name.clone())
        };
        let utilities: Vec<serde_json::Value> = build["skills"]["utilities"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|u| serde_json::json!(skill_name(u)))
            .collect();
        let plate = serde_json::json!({
            "specializations": specs,
            "weapons": {
                "set1": {"main": "Greatsword", "off": null},
                "set2": {"main": "Axe", "off": "Focus"},
            },
            "skills": {
                "heal": skill_name(&build["skills"]["heal"]),
                "utilities": utilities,
                "elite": skill_name(&build["skills"]["elite"]),
            },
            "rune": "Superior Rune of the Scholar",
            "sigils": ["Superior Sigil of Force", "Superior Sigil of Fire"],
            "relic": "Relic of the Thief",
            "stat_prefix": "Marauder",
            "explanation": "cached build",
        });
        let parsed = crate::prompts::parse_gemini_build(&plate.to_string()).ok()?;
        let validated = crate::validation::validate_gemini_build(&parsed, db, "Necromancer");
        assert!(validated.errors.is_empty(), "{:?}", validated.errors);
        let (bal, scenario) = crate::rotation::reaper_fixture::scenario();
        let report = super::evaluate_validated_build_with(
            &validated,
            db,
            "Necromancer",
            &OptimizationWeights::default(),
            &bal,
            &scenario,
            &[],
        );
        let (stats, _) =
            crate::engine::calculate_validated_stats(&validated, db, "Necromancer", &bal);
        let prepared =
            crate::engine::prepare_validated_rotation(&validated, db, &stats, Some(&scenario))
                .expect("prepares");
        let traced =
            crate::engine::simulate_prepared_traced(&prepared, &validated, db, Some(&scenario))
                .wvw
                .expect("WvW");
        Some((trait_ids, traced, report))
    }

    /// T054 (SC-001): the cached Reaper build's nine traits, each with its
    /// state; at most two on the coverage line, each with a class.
    #[test]
    #[ignore]
    fn reaper_cached_build_traits_are_simulated() {
        let Some((cache_dir, db)) = cache_db() else {
            return;
        };
        let Some((trait_ids, traced, report)) = cached_reaper_traced(&db, &cache_dir) else {
            println!("no Reaper build tab in the cache: nothing to check");
            return;
        };
        let ctx = BalanceContext::new(GameMode::WvW);
        let mut listed = 0usize;
        for id in &trait_ids {
            let name = db
                .traits
                .get(id)
                .map(|t| t.name.clone())
                .unwrap_or_default();
            let (state, _) = trait_state(&db, *id, &ctx);
            let on_line = traced.coverage.iter().find(|e| e.name == name);
            println!(
                "{id} {name}: {state}{}",
                on_line
                    .map(|e| format!(" — on the line: {}", e.class.suffix()))
                    .unwrap_or_default()
            );
            if let Some(entry) = on_line {
                listed += 1;
                assert!(
                    !matches!(entry.class, crate::data::quality::ReasonClass::NoRecord),
                    "{name} sits on the line without a class"
                );
            }
        }
        println!(
            "viable {} quality {:?}; fired: {:?}",
            report.viability.is_viable, report.quality, traced.trait_fire_counts
        );
        assert!(
            listed <= 2,
            "{listed} of the nine traits are on the coverage line: {:?}",
            traced.coverage
        );
    }

    /// T066 (SC-003): the published GuildJen build ranks above the same
    /// build with a trigger trait swapped for a line neighbour that only has a
    /// coverage class.
    #[test]
    #[ignore]
    fn necro_published_ranks_by_its_triggers() {
        let Some((_, db)) = cache_db() else {
            return;
        };
        let (bal, mut scenario) = crate::rotation::reaper_fixture::scenario();
        scenario.combat_tier = CombatTier::Party;
        let weights = OptimizationWeights::default();
        let rank = |validated: &ValidatedBuild| {
            let report = super::evaluate_validated_build_with(
                validated,
                &db,
                "Necromancer",
                &weights,
                &bal,
                &scenario,
                &[],
            );
            (search_rank(&report), report.quality.clone())
        };
        let published =
            crate::rotation::necro_published::build(&db).expect("the published build validates");
        let (base, quality) = rank(&published);
        println!("published: {base:?} {quality:?}");
        // Chilling Victory (record) against Decimate Defenses (coverage only).
        let swapped =
            crate::rotation::necro_published::build_with_trait(&db, 2008, 2031).expect("swap");
        let (other, _) = rank(&swapped);
        println!("Decimate Defenses instead of Chilling Victory: {other:?}");
        // The trigger trait changes the rank key; its direction is the
        // simulation's to decide (Chilling Victory's Might feeds Blighter's
        // Boon and moves the shroud cycle), so the audit records both keys.
        assert_ne!(base, other, "the trigger trait is felt in the rank key");
        // Blighter's Boon (record) against Deathly Chill (record): printed only.
        let swapped =
            crate::rotation::necro_published::build_with_trait(&db, 1932, 1919).expect("swap");
        let (other, _) = rank(&swapped);
        println!("Deathly Chill instead of Blighter's Boon: {other:?}");
    }

    #[test]
    fn roam_rank_prefers_earlier_target_threshold_when_other_terms_match() {
        let mut earlier = make_viable_rotation();
        let earlier_wvw = earlier.wvw.as_mut().expect("WvW report");
        earlier_wvw.target_reached = true;
        earlier_wvw.target_reached_at_ms = Some(1_000);

        let mut later = earlier.clone();
        later.wvw.as_mut().expect("WvW report").target_reached_at_ms = Some(4_000);

        assert!(search_rank(&make_rank_report(earlier)) > search_rank(&make_rank_report(later)));
    }
    /// Player report 2026-09-24 (Willbender, WvW Roam, Damage, Power 100 %,
    /// Sustain 48 %): Improve served a Hearty/Sentinel Luminary tank over a
    /// Dragon's/Marauder power roamer. Both are on-intent for the Damage
    /// profile; the tank only measured a higher alignment because the solo
    /// row focuses sustain too and its sustain axis runs past 1.0. Alignment
    /// is a floor: once both builds clear it, the burst that lands and the
    /// player's radar decide. Numbers are the referee's, from
    /// `examples/optimize_tank_repro.rs` on 1.14.42.
    #[test]
    fn an_on_intent_tank_does_not_outrank_a_power_roamer_on_alignment() {
        let gates = |n: usize| {
            (0..n)
                .map(|_| GateResult {
                    gate: ViabilityGate::CleanseRate,
                    passed: true,
                    skipped: false,
                    note: String::new(),
                })
                .collect::<Vec<_>>()
        };

        let mut roamer_rot = make_viable_rotation();
        {
            let fight = roamer_rot.wvw.as_mut().expect("WvW report");
            fight.chain_completed = true;
            fight.target_reached = true;
        }
        let mut roamer = make_rank_report(roamer_rot);
        roamer.viability.gates = gates(9);
        roamer.intent_alignment = Some(0.438);
        roamer.user_intent_score = 0.511;

        let mut tank_rot = make_viable_rotation();
        {
            let fight = tank_rot.wvw.as_mut().expect("WvW report");
            fight.chain_completed = false;
            fight.target_reached = false;
            fight.peak_protected_damage_2s = 626.0;
        }
        let mut tank = make_rank_report(tank_rot);
        tank.viability.gates = gates(8);
        tank.intent_alignment = Some(0.506);
        tank.user_intent_score = 0.108;

        assert!(
            search_rank(&roamer) > search_rank(&tank),
            "roamer {:?} tank {:?}",
            search_rank(&roamer),
            search_rank(&tank)
        );

        // The floor still bites: an off-intent build loses to an on-intent
        // one however much better it performs, and stays signed below it.
        let mut off = make_rank_report(make_viable_rotation());
        off.viability.gates = gates(12);
        off.intent_alignment = Some(crate::scoring::INTENT_ALIGNMENT_FLOOR - 0.2);
        off.user_intent_score = 0.99;
        let mut further_off = off.clone();
        further_off.intent_alignment = Some(crate::scoring::INTENT_ALIGNMENT_FLOOR - 0.4);
        assert!(search_rank(&tank) > search_rank(&off));
        assert!(search_rank(&off) > search_rank(&further_off));
    }

    #[test]
    fn roam_disabler_rank_honors_user_weights_after_required_exchange() {
        let mut aligned_rotation = make_viable_rotation();
        aligned_rotation
            .wvw
            .as_mut()
            .expect("WvW report")
            .control_landed_ms = 2_000;

        let mut misaligned_rotation = aligned_rotation.clone();
        misaligned_rotation
            .wvw
            .as_mut()
            .expect("WvW report")
            .control_landed_ms = 3_000;

        let mut aligned = make_rank_report(aligned_rotation);
        aligned.scenario.combat_kind = crate::scenario::CombatKind::Disabler;
        aligned.user_intent_score = 0.85;

        let mut misaligned = make_rank_report(misaligned_rotation);
        misaligned.scenario.combat_kind = crate::scenario::CombatKind::Disabler;
        misaligned.user_intent_score = 0.25;

        assert!(search_rank(&aligned) > search_rank(&misaligned));
    }

    fn make_wvw_scenario() -> ScenarioSpec {
        ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: CombatTier::Squad,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "WvW".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        }
    }

    fn make_pve_scenario() -> ScenarioSpec {
        ScenarioSpec {
            game_mode: GameMode::PvE,
            combat_tier: CombatTier::Party,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "PvE".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        }
    }

    fn gate_by_kind<'a>(gates: &'a [GateResult], kind: &ViabilityGate) -> Option<&'a GateResult> {
        gates.iter().find(|g| &g.gate == kind)
    }

    // Gate scenario tests

    /// WvW build with all gates satisfied → viable.
    #[test]
    fn gate_wvw_all_pass_is_viable() {
        let rot = make_viable_rotation();
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        assert!(
            report.is_viable,
            "expected viable; gates: {:?}",
            report.gates
        );
        assert_eq!(report.gates.len(), 8); // four bar checks + three timeline checks + effective health
        for g in &report.gates {
            assert!(
                g.passed,
                "gate {:?} should pass but failed: {}",
                g.gate, g.note
            );
        }
    }

    /// A missing dummy HP must not pass the percent-of-target damage routes.
    /// `unwrap_or(0.0)` used to make `peak >= 0` true for every StrikeSpike fight.
    #[test]
    fn gate_wvw_no_target_does_not_pass_percent_damage() {
        let mut rot = make_viable_rotation();
        let fight = rot.wvw.as_mut().expect("fixture has a timeline");
        fight.target_health = None;
        fight.target_reached = false;
        fight.peak_protected_damage_2s = 8_000.0;
        fight.protected_damage = 10_000.0;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let gate = gate_by_kind(&report.gates, &ViabilityGate::ProtectedExecution)
            .expect("WvW evaluates ProtectedExecution");
        assert!(
            !gate.passed,
            "unknown target HP must not satisfy the 30% spike route: {}",
            gate.note
        );
    }

    #[test]
    fn wvw_outcome_uses_timeline_not_legacy_dummy() {
        let mut rot = make_viable_rotation();
        rot.downed = true;
        rot.wvw.as_mut().expect("WvW report").target_reached = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        scenario.combat_kind = crate::scenario::CombatKind::Harasser;

        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let gate =
            gate_by_kind(&report.gates, &ViabilityGate::EncounterOutcome).expect("outcome gate");

        assert!(!gate.passed);
    }

    /// WvW build missing stunbreak → non-viable, stunbreak gate fails.
    #[test]
    fn gate_wvw_no_stunbreak_fails() {
        let mut rot = make_viable_rotation();
        rot.stunbreak_count = 0;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        assert!(!report.is_viable);
        let g = gate_by_kind(&report.gates, &ViabilityGate::StunbreakCount).unwrap();
        assert!(!g.passed);
    }

    /// WvW build missing stability → non-viable, stability gate fails.
    #[test]
    fn gate_wvw_no_stability_fails() {
        let mut rot = make_viable_rotation();
        rot.has_stability = false;
        rot.has_cover_answer = false;
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        assert!(!report.is_viable);
        let g = gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).unwrap();
        assert!(!g.passed);
        assert!(g.note.contains("no cover"));
    }

    #[test]
    fn gate_wvw_evade_without_stability_passes() {
        let mut rot = make_viable_rotation();
        rot.has_stability = false;
        rot.has_cover_answer = true;
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).unwrap();
        assert!(g.passed, "note={}", g.note);
        assert!(report.is_viable);
    }

    #[test]
    fn gate_roam_interrupt_without_stability_passes() {
        let mut rot = make_viable_rotation();
        rot.has_stability = false;
        rot.has_cover_answer = false;
        rot.has_interrupt = true;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).unwrap();
        assert!(g.passed, "note={}", g.note);
    }

    #[test]
    fn gate_zerg_interrupt_without_cover_fails() {
        let mut rot = make_viable_rotation();
        rot.has_stability = false;
        rot.has_cover_answer = false;
        rot.has_interrupt = true;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).unwrap();
        assert!(!g.passed);
    }

    /// WvW build that cleanses nothing → non-viable, cleanse gate fails.
    /// The rate is what the gate reads (gear can cleanse with no bar skill),
    /// so "no cleanse" means no rate either.
    #[test]
    fn gate_wvw_no_cleanse_fails() {
        let mut rot = make_viable_rotation();
        rot.cleanse_count = 0;
        rot.cleanse_rate_per_20s = 0.0;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        assert!(!report.is_viable);
        let g = gate_by_kind(&report.gates, &ViabilityGate::CleanseRate).unwrap();
        assert!(!g.passed);
    }

    /// PvE build has no stunbreak/stability/cleanse gates; only EHP gate.
    #[test]
    fn gate_pve_only_ehp_gate_runs() {
        let rot = make_viable_rotation(); // has all PvP flags set
        let mut combat = make_viable_combat();
        combat.effective_health = EHP_FLOOR_PVE + 1_000.0;
        let scenario = make_pve_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        // Only EHP gate should be present
        assert_eq!(report.gates.len(), 1);
        assert_eq!(report.gates[0].gate, ViabilityGate::EffectiveHealth);
        assert!(report.is_viable);

        // Rotation-based gates are not present
        assert!(gate_by_kind(&report.gates, &ViabilityGate::StunbreakCount).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::CleanseRate).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).is_none());
    }

    #[test]
    fn profile_ehp_floors_change_gate_outcome() {
        let combat = CombatPerformance {
            effective_health: 20_000.0,
            ..Default::default()
        };
        let scenario = make_pve_scenario();
        let mut low = crate::data::objective_profiles::objective_profiles()
            .default_for_mode("PvE")
            .expect("embedded PvE default")
            .clone();
        low.viability_gates.ehp_floor = Some(15_000.0);
        let mut high = low.clone();
        high.viability_gates.ehp_floor = Some(25_000.0);

        let pass = evaluate_viability_gates_for(None, &combat, &scenario, Some(&low));
        let fail = evaluate_viability_gates_for(None, &combat, &scenario, Some(&high));
        assert!(
            pass.is_viable,
            "20k EHP should pass a 15k floor: {:?}",
            pass.gates
        );
        assert!(
            !fail.is_viable,
            "20k EHP should fail a 25k floor: {:?}",
            fail.gates
        );
        assert_eq!(pass.gates[0].gate, ViabilityGate::EffectiveHealth);
        assert_eq!(fail.gates[0].gate, ViabilityGate::EffectiveHealth);
        // The custom floor, not the hardcoded default, must surface in the note.
        assert!(
            pass.gates[0].note.contains("15000"),
            "note should reflect the 15k override: {}",
            pass.gates[0].note
        );
        assert!(
            fail.gates[0].note.contains("25000"),
            "note should reflect the 25k override: {}",
            fail.gates[0].note
        );
    }

    fn blank_profile(mode: &str) -> crate::data::ObjectiveProfile {
        crate::data::objective_profiles::objective_profiles()
            .default_for_mode(mode)
            .expect("embedded default")
            .clone()
    }

    #[test]
    fn profile_min_stunbreaks_overrides_const() {
        let mut rot = make_viable_rotation();
        rot.stunbreak_count = 1;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let mut high = blank_profile("WvW");
        high.viability_gates.min_stunbreaks = Some(2);

        let pass = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, None);
        let fail = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&high));
        let g_pass = gate_by_kind(&pass.gates, &ViabilityGate::StunbreakCount).unwrap();
        let g_fail = gate_by_kind(&fail.gates, &ViabilityGate::StunbreakCount).unwrap();
        assert!(g_pass.passed, "default floor is 1: {}", g_pass.note);
        assert!(!g_fail.passed, "profile floor 2: {}", g_fail.note);
        assert!(g_fail.note.contains("required >=2"), "{}", g_fail.note);
    }

    #[test]
    fn profile_requires_stability_true_rejects_cover_only() {
        let mut rot = make_viable_rotation();
        rot.has_stability = false;
        rot.has_cover_answer = true;
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let mut strict = blank_profile("WvW");
        strict.viability_gates.requires_stability = Some(true);

        let cover = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, None);
        let stab = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&strict));
        assert!(
            gate_by_kind(&cover.gates, &ViabilityGate::StabilityAccess)
                .unwrap()
                .passed
        );
        let g = gate_by_kind(&stab.gates, &ViabilityGate::StabilityAccess).unwrap();
        assert!(!g.passed, "{}", g.note);
        assert!(g.note.contains("required by profile"), "{}", g.note);
    }

    #[test]
    fn profile_requires_stability_false_skips_the_gate() {
        let mut rot = make_viable_rotation();
        rot.has_stability = false;
        rot.has_cover_answer = false;
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let mut off = blank_profile("WvW");
        off.viability_gates.requires_stability = Some(false);

        let report = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&off));
        assert!(gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).is_none());
    }

    /// The rate floor decides the gate. The count floor is reporting only:
    /// runes, sigils, relics and traits cleanse without a skill slot, so a
    /// kit meeting the rate is not refused for carrying too few bar skills.
    #[test]
    fn profile_min_cleanse_count_and_rate_override_consts() {
        let mut rot = make_viable_rotation();
        rot.cleanse_count = 1;
        rot.cleanse_rate_per_20s = 4.0;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let mut count = blank_profile("WvW");
        count.viability_gates.min_cleanse_count = Some(2);
        let mut rate = blank_profile("WvW");
        rate.viability_gates.min_cleanse_rate_per_20s = Some(10.0);

        let default = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, None);
        let by_count = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&count));
        let by_rate = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&rate));
        assert!(
            gate_by_kind(&default.gates, &ViabilityGate::CleanseRate)
                .unwrap()
                .passed
        );
        let counted = gate_by_kind(&by_count.gates, &ViabilityGate::CleanseRate).unwrap();
        assert!(counted.passed, "count is reporting only: {}", counted.note);
        assert!(counted.note.contains("count >=2"), "{}", counted.note);
        let g = gate_by_kind(&by_rate.gates, &ViabilityGate::CleanseRate).unwrap();
        assert!(!g.passed, "{}", g.note);
        assert!(g.note.contains("10.0"), "{}", g.note);
    }

    #[test]
    fn profile_boon_uptime_floors_emit_a_gate() {
        let mut rot = make_viable_rotation();
        let combat = make_viable_combat();
        let scenario = make_pve_scenario();
        let mut floors = blank_profile("PvE");
        floors
            .viability_gates
            .boon_uptime_floors
            .insert("Quickness".into(), 0.9);

        let miss = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&floors));
        let g = gate_by_kind(&miss.gates, &ViabilityGate::BoonUptime).unwrap();
        assert!(!g.passed, "{}", g.note);
        assert!(g.note.contains("Quickness"), "{}", g.note);

        rot.buff_uptime.insert("Quickness".into(), 0.95);
        let hit = evaluate_viability_gates_for(Some(&rot), &combat, &scenario, Some(&floors));
        assert!(
            gate_by_kind(&hit.gates, &ViabilityGate::BoonUptime)
                .unwrap()
                .passed
        );
    }

    #[test]
    fn evaluate_validated_build_uses_scenario_profile_ehp_floor() {
        let combat = CombatPerformance {
            effective_health: 20_000.0,
            ..Default::default()
        };

        let mut high = crate::data::objective_profiles::objective_profiles()
            .default_for_mode("PvE")
            .expect("embedded PvE default")
            .clone();
        high.objective_profile_id = "test_high_ehp".into();
        high.viability_gates.ehp_floor = Some(25_000.0);
        let catalog = crate::data::ObjectiveProfileData {
            files: HashMap::from([(
                "PvE".into(),
                crate::data::ObjectiveProfileFile {
                    mode: "PvE".into(),
                    profiles: vec![high],
                },
            )]),
        };

        let mut named = make_pve_scenario();
        named.objective_profile_id = Some("test_high_ehp".into());
        let fail = evaluate_viability_gates_for(
            None,
            &combat,
            &named,
            super::objective_profile_for(&named, &catalog),
        );
        assert!(
            !fail.is_viable,
            "20k EHP should fail the scenario profile's 25k floor: {:?}",
            fail.gates
        );

        let mut none = make_pve_scenario();
        none.objective_profile_id = None;
        let pass = evaluate_viability_gates_for(
            None,
            &combat,
            &none,
            super::objective_profile_for(&none, &catalog),
        );
        assert!(
            pass.is_viable,
            "20k EHP should pass hardcoded PvE floor when profile id is unset: {:?}",
            pass.gates
        );
    }

    /// PIN: embedded JSONs carry no `viability_gates` key, so deserialized profiles
    /// have `viability_gates.ehp_floor == None` and the hardcoded mode/tier constants
    /// from `evaluate_viability_gates_for` apply. Pins the exact current mapping —
    /// if this trips, the gate defaults or the embedded JSONs changed.
    #[test]
    fn unset_viability_gates_pin_hardcoded_ehp_floors() {
        let data = crate::data::objective_profiles::objective_profiles();
        let pve_profile = data.default_for_mode("PvE").expect("embedded PvE default");
        let pvp_profile = data.default_for_mode("PvP").expect("embedded PvP default");
        let wvw_profile = data.default_for_mode("WvW").expect("embedded WvW default");
        assert!(
            pve_profile.viability_gates.ehp_floor.is_none()
                && pvp_profile.viability_gates.ehp_floor.is_none()
                && wvw_profile.viability_gates.ehp_floor.is_none(),
            "PIN setup: embedded default profiles must have no ehp_floor override"
        );

        let mut pvp_scenario = make_pve_scenario();
        pvp_scenario.game_mode = GameMode::PvP;
        pvp_scenario.optimization_target = OptimizationTarget {
            label: "PvP".into(),
        };
        let wvw_tier = |tier: CombatTier| {
            let mut s = make_wvw_scenario();
            s.combat_tier = tier;
            s
        };
        let mut wvw_staller = make_wvw_scenario();
        wvw_staller.combat_kind = crate::scenario::CombatKind::Staller;

        // (label, scenario, embedded profile, pinned hardcoded floor).
        // Mapping pinned from production: PvE→EHP_FLOOR_PVE, PvP→EHP_FLOOR_PVP,
        // WvW Solo→ROAM, WvW Party→HAVOC, WvW Squad→ZERG, WvW Staller→ROAM.
        let cases: Vec<(&str, ScenarioSpec, &crate::data::ObjectiveProfile, f64)> = vec![
            ("PvE", make_pve_scenario(), pve_profile, EHP_FLOOR_PVE),
            ("PvP", pvp_scenario, pvp_profile, EHP_FLOOR_PVP),
            (
                "WvW/Solo",
                wvw_tier(CombatTier::Solo),
                wvw_profile,
                EHP_FLOOR_WVW_ROAM,
            ),
            (
                "WvW/Party",
                wvw_tier(CombatTier::Party),
                wvw_profile,
                EHP_FLOOR_WVW_HAVOC,
            ),
            (
                "WvW/Squad",
                wvw_tier(CombatTier::Squad),
                wvw_profile,
                EHP_FLOOR_WVW_ZERG,
            ),
            ("WvW/Staller", wvw_staller, wvw_profile, EHP_FLOOR_WVW_ROAM),
        ];

        for (label, scenario, profile, floor) in &cases {
            // A no-`viability_gates` profile must behave exactly like no profile at all.
            assert_ehp_boundary(label, scenario, Some(profile), *floor);
            assert_ehp_boundary(label, scenario, None, *floor);
        }
    }

    /// Asserts that EHP exactly at `floor` passes and just below fails.
    fn assert_ehp_boundary(
        label: &str,
        scenario: &ScenarioSpec,
        profile: Option<&crate::data::ObjectiveProfile>,
        floor: f64,
    ) {
        let at = CombatPerformance {
            effective_health: floor,
            ..CombatPerformance::default()
        };
        let below = CombatPerformance {
            effective_health: floor - 0.5,
            ..CombatPerformance::default()
        };
        let at_report = evaluate_viability_gates_for(None, &at, scenario, profile);
        let below_report = evaluate_viability_gates_for(None, &below, scenario, profile);
        let at_gate = gate_by_kind(&at_report.gates, &ViabilityGate::EffectiveHealth)
            .expect("EffectiveHealth gate");
        let below_gate = gate_by_kind(&below_report.gates, &ViabilityGate::EffectiveHealth)
            .expect("EffectiveHealth gate");
        assert!(
            at_gate.passed,
            "{label}: EHP={floor} should pass pinned floor; note='{}'",
            at_gate.note
        );
        assert!(
            !below_gate.passed,
            "{label}: EHP={} should fail pinned floor {floor}",
            floor - 0.5
        );
    }

    /// WvW build with `rotation = None` → rotation-dependent gates fail with "rotation unavailable".
    #[test]
    fn gate_wvw_rotation_none_rotation_gates_fail_gracefully() {
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(None, &combat, &scenario);

        // Four rotation gates, two WvW timeline gates, and effective health.
        assert_eq!(report.gates.len(), 8);
        assert!(!report.is_viable);

        let sb = gate_by_kind(&report.gates, &ViabilityGate::StunbreakCount).unwrap();
        assert!(!sb.passed);
        assert_eq!(sb.note, "rotation unavailable");

        let stab = gate_by_kind(&report.gates, &ViabilityGate::StabilityAccess).unwrap();
        assert!(!stab.passed);
        assert_eq!(stab.note, "rotation unavailable");

        let cl = gate_by_kind(&report.gates, &ViabilityGate::CleanseRate).unwrap();
        assert!(!cl.passed);
        assert_eq!(cl.note, "rotation unavailable");

        let cc = gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).unwrap();
        assert!(!cc.passed);
        assert_eq!(cc.note, "rotation unavailable");

        // EHP gate still runs and passes (viable combat)
        let ehp = gate_by_kind(&report.gates, &ViabilityGate::EffectiveHealth).unwrap();
        assert!(ehp.passed);
    }

    /// PvE build with `rotation = None` → only EHP gate, still viable if EHP passes.
    #[test]
    fn gate_pve_rotation_none_only_ehp_gate() {
        let mut combat = make_viable_combat();
        combat.effective_health = EHP_FLOOR_PVE + 500.0;
        let scenario = make_pve_scenario();
        let report = evaluate_viability_gates(None, &combat, &scenario);

        assert_eq!(report.gates.len(), 1);
        assert!(report.is_viable);
        assert_eq!(report.gates[0].gate, ViabilityGate::EffectiveHealth);
    }

    /// EHP gate uses the WvW floor for WvW scenarios; tier-aware (Squad uses ZERG floor).
    #[test]
    fn gate_wvw_ehp_below_wvw_floor_fails() {
        let rot = make_viable_rotation();
        let mut combat = make_viable_combat();
        // Use a value below the Zerg (Squad) floor to ensure failure at Squad tier
        combat.effective_health = EHP_FLOOR_WVW_ZERG - 1.0;
        // make_wvw_scenario() uses CombatTier::Squad
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        assert!(!report.is_viable);
        let ehp = gate_by_kind(&report.gates, &ViabilityGate::EffectiveHealth).unwrap();
        assert!(!ehp.passed);
    }

    /// EHP gate uses the PvE floor (lower) for PvE scenarios.
    #[test]
    fn gate_pve_ehp_above_pve_floor_passes() {
        let combat_passes = CombatPerformance {
            effective_health: EHP_FLOOR_PVE + 1.0,
            ..CombatPerformance::default()
        };
        let scenario = make_pve_scenario();
        let report = evaluate_viability_gates(None, &combat_passes, &scenario);
        assert!(report.is_viable);
    }

    fn make_test_db() -> GameDb {
        GameDb {
            items: Default::default(),
            itemstats: Default::default(),
            skills: Default::default(),
            traits: Default::default(),
            specializations: Default::default(),
            professions: Default::default(),
            legends: Default::default(),
            pvp_amulets: Default::default(),
            pets: Default::default(),
            skills_by_profession: Default::default(),
            traits_by_spec: Default::default(),
            items_by_type: Default::default(),
            runes: Default::default(),
            sigils: Default::default(),
            relics: Default::default(),
            skill_to_palette: Default::default(),
            palette_to_skill: Default::default(),
            traits_by_condition: Default::default(),
            skills_by_condition: Default::default(),
            traits_by_buff: Default::default(),
            skills_by_buff: Default::default(),
            localized: None,
        }
    }

    fn make_minimal_validated() -> ValidatedBuild {
        ValidatedBuild {
            specializations: vec![
                ValidatedSpec {
                    spec_id: 1,
                    name: "Spec A".into(),
                    elite: false,
                    trait_ids: vec![],
                    trait_names: vec![],
                    all_trait_ids: vec![],
                },
                ValidatedSpec {
                    spec_id: 2,
                    name: "Spec B".into(),
                    elite: false,
                    trait_ids: vec![],
                    trait_names: vec![],
                    all_trait_ids: vec![],
                },
                ValidatedSpec {
                    spec_id: 3,
                    name: "Spec C".into(),
                    elite: true,
                    trait_ids: vec![],
                    trait_names: vec![],
                    all_trait_ids: vec![],
                },
            ],
            weapons: ValidatedWeapons {
                set1: ValidatedWeaponSet {
                    main_hand: None,
                    off_hand: None,
                },
                set2: ValidatedWeaponSet {
                    main_hand: None,
                    off_hand: None,
                },
            },
            skills: ValidatedSkills {
                heal: None,
                utilities: vec![],
                elite: None,
                profession: vec![],
            },
            legends: vec![],
            aquatic_legends: vec![],
            pets: None,
            rune: None,
            sigils: vec![],
            sigil_seats: Default::default(),
            relic: None,
            food: None,
            utility: None,
            infusion_seats: Vec::new(),
            gear_slots: gw2_core::types::GearSlots::default(), // itemstat 9999 intentionally absent
            explanation: String::new(),
            synergy_explanation: String::new(),
            changes: vec![],
            warnings: vec![],
            errors: vec![],
        }
    }

    /// The rank must see skills. The same build with a striking utility in a
    /// slot outranks the build with every slot empty, in PvE, where the
    /// closed-form stat score could not tell them apart (0 of 36 fillers
    /// moved the rank, measured 2026-09-04).
    #[test]
    fn pve_rank_prefers_the_bar_that_produces_more() {
        let mut db = make_test_db();
        db.skills.insert(
            777,
            gw2_api::models::Skill {
                id: 777,
                name: "Cleave".into(),
                description: None,
                icon: None,
                chat_link: None,
                skill_type: None,
                weapon_type: None,
                professions: vec!["Warrior".into()],
                slot: Some("Utility".into()),
                facts: vec![
                    gw2_api::models::Fact::Damage {
                        text: None,
                        icon: None,
                        hit_count: Some(3),
                        dmg_multiplier: Some(1.5),
                    },
                    gw2_api::models::Fact::Recharge {
                        text: None,
                        icon: None,
                        value: Some(8.0),
                    },
                ],
                traited_facts: vec![],
                fact_parse_drops: 0,
                categories: vec![],
                attunement: None,
                cost: None,
                dual_wield: None,
                flip_skill: None,
                initiative: None,
                next_chain: None,
                prev_chain: None,
                transform_skills: vec![],
                bundle_skills: vec![],
                toolbelt_skill: None,
                flags: vec![],
                specialization: None,
            },
        );
        let ctx = BalanceContext::new(GameMode::PvE);
        let mut scenario = ScenarioSpec::from_balance_context(&ctx);
        scenario.combat_tier = CombatTier::Party;
        let weights = OptimizationWeights::preset_power_dps();

        db.skills.insert(
            778,
            gw2_api::models::Skill {
                id: 778,
                name: "Second Cleave".into(),
                facts: vec![
                    gw2_api::models::Fact::Damage {
                        text: None,
                        icon: None,
                        hit_count: Some(2),
                        dmg_multiplier: Some(1.2),
                    },
                    gw2_api::models::Fact::Recharge {
                        text: None,
                        icon: None,
                        value: Some(12.0),
                    },
                ],
                ..db.skills[&777].clone()
            },
        );
        let mut full = make_minimal_validated();
        full.skills.utilities = vec![Some((777, "Cleave".into())), None, None];
        let mut fuller = make_minimal_validated();
        fuller.skills.utilities = vec![
            Some((777, "Cleave".into())),
            Some((778, "Second Cleave".into())),
            None,
        ];
        let mut hole = make_minimal_validated();
        hole.skills.utilities = vec![None, None, None];

        let full_report =
            evaluate_validated_build(&full, &db, "Warrior", &weights, &ctx, &scenario);
        let hole_report =
            evaluate_validated_build(&hole, &db, "Warrior", &weights, &ctx, &scenario);
        assert!(
            full_report.viability.is_viable && hole_report.viability.is_viable,
            "{:?} / {:?}",
            full_report.viability.gates,
            hole_report.viability.gates
        );
        assert!(
            full_report.realized.power > hole_report.realized.power,
            "cleave produced strike: {:?} vs {:?}",
            full_report.realized,
            hole_report.realized
        );
        assert!(
            search_rank(&full_report) > search_rank(&hole_report),
            "full {:?} vs hole {:?}",
            search_rank(&full_report),
            search_rank(&hole_report)
        );
        // Two real rotations: the fuller bar must outrank the one-skill bar.
        let fuller_report =
            evaluate_validated_build(&fuller, &db, "Warrior", &weights, &ctx, &scenario);
        assert!(
            search_rank(&fuller_report) > search_rank(&full_report),
            "fuller {:?} vs full {:?}",
            search_rank(&fuller_report),
            search_rank(&full_report)
        );
    }

    #[test]
    fn referee_evaluation_is_deterministic_for_same_inputs() {
        let db = make_test_db();
        let validated = make_minimal_validated();
        let ctx = BalanceContext::new(GameMode::PvE);
        let mut scenario = ScenarioSpec::from_balance_context(&ctx);
        scenario.combat_tier = CombatTier::Party;
        let weights = OptimizationWeights::default_for_mode(GameMode::PvE.label());

        let report_a =
            evaluate_validated_build(&validated, &db, "Guardian", &weights, &ctx, &scenario);
        let report_b =
            evaluate_validated_build(&validated, &db, "Guardian", &weights, &ctx, &scenario);

        assert_eq!(report_a.quality, DataQuality::Verified);
        assert_eq!(report_a.user_intent_score, report_b.user_intent_score);
        assert_eq!(
            report_a.primary_combat.total_dps_index,
            report_b.primary_combat.total_dps_index
        );
        // Viability must be deterministic: same is_viable flag across both calls.
        assert_eq!(
            report_a.viability.is_viable, report_b.viability.is_viable,
            "viability gate outcome must be deterministic"
        );
    }

    #[test]
    fn referee_marks_build_blocked_when_validation_has_errors() {
        let db = make_test_db();
        let mut validated = make_minimal_validated();
        validated.errors.push(ValidationReject {
            code: RejectCode::WeaponNotAvailable {
                slot: "Set 1".into(),
                weapon: "illegal".into(),
                profession: "Guardian".into(),
            },
            detail: "illegal weapon".into(),
        });
        let ctx = BalanceContext::new(GameMode::PvE);
        let scenario = ScenarioSpec::from_balance_context(&ctx);
        let weights = OptimizationWeights::default_for_mode(GameMode::PvE.label());

        let report =
            evaluate_validated_build(&validated, &db, "Guardian", &weights, &ctx, &scenario);

        assert_eq!(report.quality, DataQuality::Blocked);
        assert_eq!(report.quality_reasons.len(), 1);
    }

    /// WvW build with a minimal (no-gear) Guardian → rotation=None from empty DB, EHP far below
    /// WvW floor → all rotation gates fail + EHP gate fails → non-viable → sentinel score -1.0.
    /// This tests that RefereeReport.viability is populated and the sentinel is applied end-to-end.
    #[test]
    fn referee_wvw_minimal_build_is_non_viable_sentinel_score() {
        let db = make_test_db();
        let validated = make_minimal_validated();
        let ctx = BalanceContext::new(GameMode::WvW);
        let scenario = ScenarioSpec::from_balance_context(&ctx);
        let weights = OptimizationWeights::default_for_mode(GameMode::WvW.label());

        let report =
            evaluate_validated_build(&validated, &db, "Guardian", &weights, &ctx, &scenario);

        // Minimal Guardian has EHP well below WvW floor and no rotation skills → non-viable.
        assert!(
            !report.viability.is_viable,
            "minimal WvW build should be non-viable; gates: {:?}",
            report.viability.gates
        );
        assert_eq!(
            report.user_intent_score, -1.0,
            "non-viable build must receive sentinel score -1.0"
        );
        // ViabilityReport must be populated (not empty).
        assert!(
            !report.viability.gates.is_empty(),
            "viability.gates must be populated even for non-viable builds"
        );
    }

    /// PvE build with stunbreak_count=0 in the rotation → only EHP gate runs in PvE,
    /// so stunbreak absence does NOT cause non-viability. Tests the gate directly since
    /// evaluate_validated_build cannot inject a custom rotation.
    #[test]
    fn gate_pve_zero_stunbreaks_still_viable_when_ehp_passes() {
        let mut rot = make_viable_rotation();
        rot.stunbreak_count = 0; // zero stunbreaks
        let mut combat = make_viable_combat();
        combat.effective_health = EHP_FLOOR_PVE + 1_000.0; // PvE EHP floor satisfied
        let scenario = make_pve_scenario();

        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);

        // PvE only runs EHP gate — 0 stunbreaks must not cause failure.
        assert!(
            report.is_viable,
            "PvE build with 0 stunbreaks should be viable when EHP passes; gates: {:?}",
            report.gates
        );
        // Confirm rotation gates are absent in PvE.
        assert!(gate_by_kind(&report.gates, &ViabilityGate::StunbreakCount).is_none());
    }

    // CombatTier-differentiated EHP gate tests

    /// EHP between Zerg floor and Roam floor: passes Zerg (Squad), fails Roaming (Solo).
    #[test]
    fn gate_wvw_ehp_passes_zerg_but_fails_roam() {
        use crate::scenario::{OptimizationTarget, TargetProfile};
        let rot = make_viable_rotation();
        // EHP is above ZERG floor but below ROAM floor
        let mid_ehp = EHP_FLOOR_WVW_ZERG + 500.0;
        assert!(
            mid_ehp < EHP_FLOOR_WVW_ROAM,
            "test setup: mid_ehp={} must be below ROAM floor={}",
            mid_ehp,
            EHP_FLOOR_WVW_ROAM
        );

        let mut combat = make_viable_combat();
        combat.effective_health = mid_ehp;

        // Squad scenario → should pass EHP gate
        let squad_scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: crate::scenario::CombatTier::Squad,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "WvW".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let report_squad = evaluate_viability_gates(Some(&rot), &combat, &squad_scenario);
        let ehp_squad = gate_by_kind(&report_squad.gates, &ViabilityGate::EffectiveHealth).unwrap();
        assert!(
            ehp_squad.passed,
            "EHP={} should pass Squad floor={}; note='{}'",
            mid_ehp, EHP_FLOOR_WVW_ZERG, ehp_squad.note
        );

        // Solo scenario (Roaming) → should fail EHP gate
        let solo_scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: crate::scenario::CombatTier::Solo,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "WvW".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let report_solo = evaluate_viability_gates(Some(&rot), &combat, &solo_scenario);
        let ehp_solo = gate_by_kind(&report_solo.gates, &ViabilityGate::EffectiveHealth).unwrap();
        assert!(
            !ehp_solo.passed,
            "EHP={} should fail Solo/Roam floor={}; note='{}'",
            mid_ehp, EHP_FLOOR_WVW_ROAM, ehp_solo.note
        );
    }

    /// A fully equipped roaming build (EHP above ROAM floor) passes all WvW gates at Solo tier.
    #[test]
    fn gate_wvw_solo_viable_when_ehp_above_roam_floor() {
        use crate::scenario::{OptimizationTarget, TargetProfile};
        let rot = make_viable_rotation();
        let mut combat = make_viable_combat();
        combat.effective_health = EHP_FLOOR_WVW_ROAM + 1_000.0;

        let solo_scenario = ScenarioSpec {
            game_mode: GameMode::WvW,
            combat_tier: crate::scenario::CombatTier::Solo,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "WvW".into(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let report = evaluate_viability_gates(Some(&rot), &combat, &solo_scenario);
        assert!(
            report.is_viable,
            "Build with EHP above ROAM floor and all rotation gates met should be viable; gates: {:?}",
            report.gates
        );
    }

    /// EHP threshold ordering: WvW tiers are correctly graduated; PvE floor is between HAVOC and ZERG.
    /// Rationale: PvE expects real ascended gear with no healer but no CC pressure either;
    /// WvW Zerg accepts lower personal EHP because squad healers cover survival.
    #[test]
    fn ehp_floor_ordering_is_correct() {
        assert!(
            EHP_FLOOR_WVW_ROAM > EHP_FLOOR_WVW_HAVOC,
            "Roam floor should be stricter than Havoc floor"
        );
        assert!(
            EHP_FLOOR_WVW_HAVOC > EHP_FLOOR_WVW_ZERG,
            "Havoc floor should be stricter than Zerg floor"
        );
        // PvE floor sits between Havoc and Zerg — WvW Zerg has squad healers so lower personal EHP
        // is acceptable; PvE lacks those but also lacks the CC pressure that makes EHP critical.
        assert!(
            EHP_FLOOR_WVW_HAVOC > EHP_FLOOR_PVE,
            "Havoc floor should be stricter than PvE floor"
        );
        assert!(
            EHP_FLOOR_PVE > EHP_FLOOR_WVW_ZERG,
            "PvE floor should be stricter than Zerg floor (squad healers compensate)"
        );
        assert!(
            EHP_FLOOR_PVP < EHP_FLOOR_WVW_ZERG,
            "PvP floor should be the loosest — amulet stat budget is smaller than ascended"
        );
    }

    #[test]
    fn gate_pvp_uses_pvp_floor_not_wvw_floor() {
        // PvP build with 9_000 EHP: below WvW Roam (15k) but above EHP_FLOOR_PVP (8k).
        // Must pass on PvP. Before the fix, requires_pvp_gates routed PvP through
        // WvW EHP tiers and the build would non-viably score at -1.0.
        let rot = make_viable_rotation();
        let combat = CombatPerformance {
            effective_health: 9_000.0,
            ..make_viable_combat()
        };
        let pvp_scenario = ScenarioSpec {
            game_mode: GameMode::PvP,
            combat_tier: CombatTier::Solo,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "PvP".to_string(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let report = evaluate_viability_gates(Some(&rot), &combat, &pvp_scenario);
        let ehp = gate_by_kind(&report.gates, &ViabilityGate::EffectiveHealth).expect("gate");
        assert!(
            ehp.passed,
            "PvP 9k EHP should pass PvP floor (8k), not be measured against WvW Roam (15k); note: {}",
            ehp.note
        );
    }

    #[test]
    fn gate_pvp_low_ehp_still_fails_pvp_floor() {
        // Sanity check: an unreasonably low PvP EHP (e.g. 5k) still fails the PvP floor.
        let rot = make_viable_rotation();
        let combat = CombatPerformance {
            effective_health: 5_000.0,
            ..make_viable_combat()
        };
        let pvp_scenario = ScenarioSpec {
            game_mode: GameMode::PvP,
            combat_tier: CombatTier::Solo,
            combat_kind: crate::scenario::CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "PvP".to_string(),
            },
            patch_id: None,
            objective_profile_id: None,
        };
        let report = evaluate_viability_gates(Some(&rot), &combat, &pvp_scenario);
        let ehp = gate_by_kind(&report.gates, &ViabilityGate::EffectiveHealth).expect("gate");
        assert!(
            !ehp.passed,
            "PvP 5k EHP should fail the PvP floor (8k); note: {}",
            ehp.note
        );
    }

    #[test]
    fn gate_wvw_roam_fails_without_mobility_out() {
        let mut rot = make_viable_rotation();
        rot.has_mobility_out = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::MobilityOut).unwrap();
        assert!(!g.passed);
        assert!(!report.is_viable);
    }

    #[test]
    fn gate_roam_power_does_not_require_strip() {
        let mut rot = make_viable_rotation();
        rot.has_strip = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        scenario.combat_kind = crate::scenario::CombatKind::StrikeSpike;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::HarasserStrip).is_none());
    }

    #[test]
    fn gate_harasser_fails_without_strip() {
        let mut rot = make_viable_rotation();
        rot.has_strip = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_kind = crate::scenario::CombatKind::Harasser;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::HarasserStrip).unwrap();
        assert!(!g.passed, "the missing strip has to be detected");
        // ...and reported without refusing the build. 11% of published
        // roamers clear this gate, which is why `blocks` excludes it; a fault
        // it names is a caveat, not a veto.
        assert!(
            report.is_viable,
            "a non-blocking gate must not send a build to the -1.0 sentinel"
        );
    }

    #[test]
    fn gate_zerg_dps_does_not_require_roam_out() {
        let mut rot = make_viable_rotation();
        rot.has_mobility_out = false;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario(); // Squad strike
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::MobilityOut).is_none());
        assert!(report.is_viable);
    }

    #[test]
    fn gate_wvw_solo_does_not_require_outcome_window() {
        let mut rot = make_viable_rotation();
        rot.downed = false;
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::EncounterOutcome).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::SecureCompletion).is_none());
        assert!(report.is_viable);
    }

    #[test]
    fn gate_harasser_requires_the_target_threshold() {
        let mut rot = make_viable_rotation();
        rot.downed = false;
        rot.wvw.as_mut().unwrap().target_reached = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_kind = crate::scenario::CombatKind::Harasser;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::EncounterOutcome).unwrap();
        assert!(!g.passed, "the unreached threshold has to be detected");
        assert!(
            report.is_viable,
            "a non-blocking gate must not send a build to the -1.0 sentinel"
        );
    }

    #[test]
    fn gate_harasser_fails_without_interrupt() {
        let mut rot = make_viable_rotation();
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_kind = crate::scenario::CombatKind::Harasser;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::SecureCompletion).unwrap();
        assert!(!g.passed, "the missing interrupt has to be detected");
        assert!(
            report.is_viable,
            "a non-blocking gate must not send a build to the -1.0 sentinel"
        );
    }

    #[test]
    fn gate_zerg_pressure_does_not_require_outcome_window() {
        let mut rot = make_viable_rotation();
        rot.downed = false;
        rot.has_interrupt = false;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::EncounterOutcome).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::SecureCompletion).is_none());
        assert!(report.is_viable);
    }

    #[test]
    fn gate_support_does_not_require_outcome_window() {
        let mut rot = make_viable_rotation();
        rot.downed = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        scenario.combat_kind = crate::scenario::CombatKind::Support;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::EncounterOutcome).is_none());
    }

    #[test]
    fn gate_staller_skips_outcome_and_strip_even_on_roam() {
        let mut rot = make_viable_rotation();
        rot.downed = false;
        rot.has_interrupt = false;
        rot.has_strip = false;
        rot.has_mobility_out = true;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Solo;
        scenario.combat_kind = crate::scenario::CombatKind::Staller;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::EncounterOutcome).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::HarasserStrip).is_none());
        let out = gate_by_kind(&report.gates, &ViabilityGate::MobilityOut).unwrap();
        assert!(out.passed);
        assert!(report.is_viable);
    }

    #[test]
    fn gate_staller_on_zerg_still_needs_escape_kit() {
        let mut rot = make_viable_rotation();
        rot.has_mobility_out = false;
        let combat = make_viable_combat();
        let mut scenario = make_wvw_scenario();
        scenario.combat_tier = CombatTier::Squad;
        scenario.combat_kind = crate::scenario::CombatKind::Staller;
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let g = gate_by_kind(&report.gates, &ViabilityGate::MobilityOut).unwrap();
        assert!(!g.passed);
        assert!(!report.is_viable);
    }

    #[test]
    fn raw_direction_breaks_capped_score_ties() {
        // Two builds with identical capped intent but different uncapped raw
        // direction: the radar direction must win the tie at the rank tail.
        let mk = |raw: f64| {
            let mut scenario = make_wvw_scenario();
            scenario.combat_tier = CombatTier::Solo;
            RefereeReport {
                scenario,
                stats: crate::stats::StatBlock::default(),
                modifiers: crate::combat::DamageModifiers::default(),
                combat_solo: CombatPerformance::default(),
                combat_party: CombatPerformance::default(),
                combat_squad: CombatPerformance::default(),
                primary_combat: CombatPerformance::default(),
                rotation: None,
                viability: ViabilityReport {
                    gates: Vec::new(),
                    is_viable: true,
                    shortfall: 0.0,
                },
                user_intent_score: 0.5,
                raw_direction_score: raw,
                ranked_direction_score: raw,
                intent_similarity: None,
                intent_alignment: None,
                realized: Default::default(),
                stat_direction_score: -1.0,
                quality: DataQuality::Verified,
                quality_reasons: Vec::new(),
            }
        };
        assert!(search_rank(&mk(0.7)) > search_rank(&mk(0.5)));
    }
    // Reaper slice (specs/004-simulator-trust)

    #[test]
    fn reaper_fixture_evaluates_without_errors() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let build = fx::build();
        let (ctx, scenario) = fx::scenario();
        let weights = OptimizationWeights::default();
        assert!(build.errors.is_empty(), "{:?}", build.errors);
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &weights,
            &ctx,
            &scenario,
            &fx::opener(),
        );
        assert!(
            !report.viability.gates.is_empty(),
            "the referee ran its gates on the fixture"
        );
        let fight = report
            .rotation
            .as_ref()
            .and_then(|r| r.wvw.as_ref())
            .expect("a WvW scenario runs the timeline");
        assert!(fight.total_damage > 0.0, "the opener landed strikes");
        assert!(
            report.stats.power > 1_000.0,
            "Marauder gear was priced: power {}",
            report.stats.power
        );
    }
    #[test]
    fn reaper_parity_referee_matches_optimize_suggestion() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let build = fx::build();
        let (ctx, scenario) = fx::scenario();
        let weights = OptimizationWeights::default();
        // Empty opener on both sides: `synergy_result_from_validated` (the
        // Optimize exposure) has no opener parameter.
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &weights,
            &ctx,
            &scenario,
            &[],
        );
        let synergy = crate::engine::synergy_result_from_validated(
            build.clone(),
            &db,
            "Necromancer",
            &ctx,
            Some(&scenario),
        );
        let eq = |name: &str, a: f64, b: f64| {
            assert!((a - b).abs() <= 1e-9, "{name}: referee {a} vs optimize {b}");
        };
        eq("power", report.stats.power, synergy.stats.power);
        eq("precision", report.stats.precision, synergy.stats.precision);
        eq("ferocity", report.stats.ferocity, synergy.stats.ferocity);
        eq("vitality", report.stats.vitality, synergy.stats.vitality);
        eq(
            "effective_power",
            report.combat_solo.effective_power,
            synergy.combat_solo.effective_power,
        );
        eq(
            "total_dps_index",
            report.combat_solo.total_dps_index,
            synergy.combat_solo.total_dps_index,
        );
        eq(
            "effective_health",
            report.combat_solo.effective_health,
            synergy.combat_solo.effective_health,
        );
        assert_eq!(
            report.quality, synergy.data_quality,
            "quality classification"
        );
        let referee_fight = report
            .rotation
            .as_ref()
            .and_then(|r| r.wvw.as_ref())
            .expect("WvW");
        let optimize_fight = synergy
            .rotation
            .as_ref()
            .and_then(|r| r.wvw.as_ref())
            .expect("WvW");
        eq(
            "total_damage",
            referee_fight.total_damage,
            optimize_fight.total_damage,
        );
        assert_eq!(
            referee_fight.unmodeled_sources, optimize_fight.unmodeled_sources,
            "unmodeled source names"
        );
        let coverage = |reasons: &[crate::data::DataQualityReason]| {
            reasons
                .iter()
                .find(|r| r.field == "wvw_timeline.effects")
                .map(|r| r.explanation.clone())
        };
        assert_eq!(
            coverage(&report.quality_reasons),
            coverage(&synergy.quality_reasons),
            "the coverage reason is projected unchanged"
        );
        // `user_intent_score`, `realized` and `viability` are not on
        // `SynergyResult`; the Optimize exposure recomputes gates with a
        // narrower set (CONN-00-10) and never carries the rank score
        // (CONN-01-04). They cannot be compared here.
    }

    /// SC-005 (Sprint 2, T041): a real Necromancer Reaper build from the
    /// character cache, real skills and traits through the real builder,
    /// evaluated in WvW. Needs `dev.cfg`; run with `--ignored`.
    ///
    /// The cached build's own upgrades are printed with the coverage line
    /// (they may or may not have records); then the same build carries the
    /// three sources this sprint recorded and each must execute.
    #[test]
    #[ignore = "reads the game cache through dev.cfg"]
    fn reaper_cached_build_has_recorded_sources() {
        use crate::rotation::wvw_timeline::TraceKind;
        let Ok(cache_dir) = gw2_api::dev_config::cache_dir() else {
            println!("no dev.cfg: nothing to check");
            return;
        };
        let cache = gw2_api::cache::DataCache::new(cache_dir.clone());
        let db = GameDb::load(&cache).expect("the cache holds a full GameDb");
        let reaper_spec = db
            .specializations
            .values()
            .find(|s| s.name == "Reaper" && s.profession == "Necromancer")
            .map(|s| s.id)
            .expect("Reaper exists");

        // The first cached Necromancer build tab that runs Reaper.
        let mut found: Option<(String, serde_json::Value, serde_json::Value)> = None;
        for entry in std::fs::read_dir(&cache_dir).expect("cache dir") {
            let path = entry.expect("entry").path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if !name.starts_with("char_") || !name.ends_with("_buildtabs.json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read build tabs");
            let tabs: serde_json::Value = serde_json::from_str(&text).expect("json");
            let Some(tabs) = tabs.as_array() else {
                continue;
            };
            for tab in tabs {
                let build = &tab["build"];
                let is_reaper = build["profession"] == "Necromancer"
                    && build["specializations"]
                        .as_array()
                        .is_some_and(|specs| specs.iter().any(|s| s["id"] == reaper_spec));
                if is_reaper {
                    let equip_path = path.with_file_name(name.replace("_buildtabs", "_equiptabs"));
                    let equip: serde_json::Value = std::fs::read_to_string(&equip_path)
                        .ok()
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or(serde_json::Value::Null);
                    let equip_tab = equip
                        .as_array()
                        .and_then(|tabs| {
                            tabs.iter()
                                .find(|e| e["tab"] == tab["tab"])
                                .or_else(|| tabs.iter().find(|e| e["is_active"] == true))
                                .cloned()
                        })
                        .unwrap_or(serde_json::Value::Null);
                    found = Some((name.clone(), build.clone(), equip_tab));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((file, build, equip)) = found else {
            println!("no Reaper build tab in the cache: nothing to check");
            return;
        };
        println!("cached Reaper build from {file}: {}", build["name"]);

        let trait_name = |id: u64| db.traits.get(&(id as u32)).map(|t| t.name.clone());
        let specs: Vec<serde_json::Value> = build["specializations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let name = db
                    .specializations
                    .get(&(s["id"].as_u64().unwrap() as u32))
                    .map(|sp| sp.name.clone())
                    .unwrap_or_default();
                let traits: Vec<String> = s["traits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|t| t.as_u64().and_then(trait_name))
                    .collect();
                serde_json::json!({"name": name, "traits": traits})
            })
            .collect();
        let skill_name = |v: &serde_json::Value| {
            v.as_u64()
                .and_then(|id| db.skills.get(&(id as u32)))
                .map(|s| s.name.clone())
        };
        let utilities: Vec<serde_json::Value> = build["skills"]["utilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| serde_json::json!(skill_name(u)))
            .collect();

        // Equipment: weapon types and the cached upgrades.
        let slot = |name: &str| {
            equip["equipment"]
                .as_array()
                .and_then(|items| items.iter().find(|e| e["slot"] == name).cloned())
        };
        let item_type = |v: &serde_json::Value| {
            v["id"]
                .as_u64()
                .and_then(|id| db.items.get(&(id as u32)))
                .and_then(|i| i.details.as_ref().and_then(|d| d.detail_type.clone()))
        };
        let upgrade_names = |v: &serde_json::Value| -> Vec<String> {
            v["upgrades"]
                .as_array()
                .map(|u| {
                    u.iter()
                        .filter_map(|id| id.as_u64())
                        .filter_map(|id| db.items.get(&(id as u32)).map(|i| i.name.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let a1 = slot("WeaponA1").unwrap_or(serde_json::Value::Null);
        let a2 = slot("WeaponA2").unwrap_or(serde_json::Value::Null);
        let b1 = slot("WeaponB1").unwrap_or(serde_json::Value::Null);
        let b2 = slot("WeaponB2").unwrap_or(serde_json::Value::Null);
        let weapons = serde_json::json!({
            "set1": {"main": item_type(&a1), "off": item_type(&a2)},
            "set2": {"main": item_type(&b1), "off": item_type(&b2)},
        });
        let cached_rune = slot("Coat")
            .map(|coat| upgrade_names(&coat))
            .and_then(|u| u.first().cloned());
        let mut cached_sigils = upgrade_names(&a1);
        cached_sigils.extend(upgrade_names(&a2));
        cached_sigils.extend(upgrade_names(&b1));
        cached_sigils.extend(upgrade_names(&b2));
        let cached_relic = slot("Relic")
            .and_then(|r| r["id"].as_u64())
            .and_then(|id| db.items.get(&(id as u32)).map(|i| i.name.clone()));
        println!(
            "cached upgrades: rune {cached_rune:?}, sigils {cached_sigils:?}, relic {cached_relic:?}"
        );

        let (bal, scenario) = crate::rotation::reaper_fixture::scenario();
        let weights = OptimizationWeights::default();
        let evaluate = |rune: Option<String>, sigils: Vec<String>, relic: Option<String>| {
            let mut plate = serde_json::json!({
                "specializations": specs,
                "weapons": weapons,
                "skills": {
                    "heal": skill_name(&build["skills"]["heal"]),
                    "utilities": utilities,
                    "elite": skill_name(&build["skills"]["elite"]),
                },
                "rune": rune,
                "sigils": sigils,
                "relic": relic,
                "stat_prefix": "Marauder",
                "explanation": "cached build",
            });
            let parsed =
                crate::prompts::parse_gemini_build(&plate.to_string()).expect("plate parses");
            let mut validated =
                crate::validation::validate_gemini_build(&parsed, &db, "Necromancer");
            if !validated.errors.is_empty() {
                // The cached weapons may not validate (aquatic/legendary
                // quirks); fall back to a real land kit and say so.
                println!(
                    "cached weapons rejected ({:?}); using Greatsword / Axe + Focus",
                    validated.errors
                );
                plate["weapons"] = serde_json::json!({
                    "set1": {"main": "Greatsword", "off": null},
                    "set2": {"main": "Axe", "off": "Focus"},
                });
                let parsed =
                    crate::prompts::parse_gemini_build(&plate.to_string()).expect("plate parses");
                validated = crate::validation::validate_gemini_build(&parsed, &db, "Necromancer");
            }
            assert!(validated.errors.is_empty(), "{:?}", validated.errors);
            let report = super::evaluate_validated_build_with(
                &validated,
                &db,
                "Necromancer",
                &weights,
                &bal,
                &scenario,
                &[],
            );
            let (stats, _) =
                crate::engine::calculate_validated_stats(&validated, &db, "Necromancer", &bal);
            let mut prepared =
                crate::engine::prepare_validated_rotation(&validated, &db, &stats, Some(&scenario))
                    .expect("prepares");
            prepared.params.precision = 3_000.0;
            let traced = crate::engine::simulate_prepared_traced(
                &prepared,
                &validated,
                &db,
                Some(&scenario),
            )
            .wvw
            .expect("WvW");
            (report, traced)
        };

        // 1. The build as cached.
        let (report, traced) = evaluate(cached_rune, cached_sigils, cached_relic);
        println!(
            "as cached: viable {} quality {:?}
coverage: {:?}",
            report.viability.is_viable, report.quality, traced.unmodeled_sources
        );
        let equipped_without_record: Vec<&String> = traced
            .unmodeled_sources
            .iter()
            .filter(|s| s.ends_with("(no record)") && !s.contains('"'))
            .collect();
        println!("equipped triggered sources without a record: {equipped_without_record:?}");

        // 2. The same real build carrying this sprint's recorded sources.
        let (report, traced) = evaluate(
            Some("Superior Rune of the Scholar".into()),
            vec![
                "Superior Sigil of Fire".into(),
                "Superior Sigil of Force".into(),
            ],
            Some("Relic of the Thief".into()),
        );
        println!(
            "with recorded sources: viable {} quality {:?}
coverage: {:?}",
            report.viability.is_viable, report.quality, traced.unmodeled_sources
        );
        for name in [
            "Superior Sigil of Fire",
            "Superior Rune of the Scholar",
            "Relic of the Thief",
        ] {
            assert!(
                !traced.unmodeled_sources.iter().any(|s| s.starts_with(name)),
                "{name} left the coverage line: {:?}",
                traced.unmodeled_sources
            );
        }
        let has = |kind: TraceKind, source: &str| {
            traced
                .trace
                .iter()
                .any(|e| e.kind == kind && e.source.starts_with(source))
        };
        assert!(
            has(TraceKind::ProcFired, "Superior Sigil of Fire"),
            "Fire fired"
        );
        assert!(
            has(
                TraceKind::ConditionalActivated,
                "Superior Rune of the Scholar"
            ),
            "Scholar active"
        );
        assert!(
            has(TraceKind::StackGained, "Relic of the Thief"),
            "Thief stacked"
        );
    }

    /// FR-010 guard (Sprint 2, T029): tagging conditional clauses in the
    /// parser and dividing them out on the WvW path must not move a single
    /// PvE number. Pinned before the parser change (commit e531a75) and
    /// re-pinned once in T053, when the fixture's shroud bar moved from the
    /// always-available profession list to the shroud set (a fixture
    /// The build panel renders `RefereeReport.viability` from
    /// `evaluate_validated_build_ranked`, the same report the benchmark
    /// meter ranks. It used to run its own `evaluate_viability_gates` with
    /// no objective profile and without the off-bar cleanse pass, and
    /// printed NON-VIABLE over builds the referee had passed. Ranked and
    /// plain evaluation must therefore agree on the verdict.
    #[test]
    fn the_ranked_and_plain_reports_return_the_same_verdict() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let build = fx::build();
        let (ctx, scenario) = fx::pve_scenario();
        let weights = OptimizationWeights::default();
        let plain =
            super::evaluate_validated_build(&build, &db, "Necromancer", &weights, &ctx, &scenario);
        let ranked = super::evaluate_validated_build_ranked(
            &build,
            &db,
            "Necromancer",
            &weights,
            &ctx,
            &scenario,
        );
        assert_eq!(plain.viability.is_viable, ranked.viability.is_viable);
        let verdicts = |report: &RefereeReport| {
            report
                .viability
                .gates
                .iter()
                .map(|g| (format!("{:?}", g.gate), g.passed, g.skipped))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            verdicts(&plain),
            verdicts(&ranked),
            "the panel and the meter must not disagree gate by gate"
        );
    }

    /// change: the PvE simulator never held those skills for a real build).
    #[test]
    fn pve_output_unchanged_by_conditional_tagging() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let build = fx::build();
        let (ctx, scenario) = fx::pve_scenario();
        let weights = OptimizationWeights::default();
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &weights,
            &ctx,
            &scenario,
            &fx::opener(),
        );
        let got = [
            report.realized.power,
            report.realized.condition,
            report.realized.boon_support,
            report.realized.healing,
            report.realized.sustain,
            report.realized.control,
            report.user_intent_score,
        ];
        // boon_support and healing are ALLY-FACING (see `realized_axes`):
        // this Reaper buffs and heals only itself, so both are zero where
        // they used to count its own Might and its own heal. The intent
        // score moves with them. Re-pinned in sprint 008 (forms): the flow
        // now plays the fixture's shroud, whose invented bar strikes softer
        // than its greatsword and controls less. Re-pinned 2026-09-24:
        // multi-hit skills land every strike at the full per-strike
        // coefficient (was divided by hit_count), power 0.0266 -> 0.0530.
        const PINNED: [f64; 7] = [
            0.05296084297412295,
            0.0,
            0.0,
            0.0,
            0.4302897574123989,
            0.05444444444444444,
            0.08385342939517683,
        ];
        for (i, (g, p)) in got.iter().zip(&PINNED).enumerate() {
            assert!(
                (g - p).abs() < 1e-9,
                "PvE value {i} moved: got {got:?}, pinned {PINNED:?}"
            );
        }
    }

    #[test]
    fn reaper_pve_comparison_uses_adaptive_scheduler_not_opener() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let build = fx::build();
        let (ctx, scenario) = fx::pve_scenario();
        let weights = OptimizationWeights::default();
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &weights,
            &ctx,
            &scenario,
            &fx::opener(),
        );
        let rotation = report
            .rotation
            .as_ref()
            .expect("PvE runs the gate simulation");
        assert!(
            rotation.wvw.is_none(),
            "PvE never runs the timeline; the opener is not pressed and no record executes (CONN-01-03)"
        );
        assert!(
            report.realized.power > 0.0,
            "the adaptive flow scheduler produced strike damage: {:?}",
            report.realized
        );
        // Sprint 1 recorded a zero gate-sim DPS in the 2 s PvE Solo window
        // (CONN-01-05): the setup priority spent it on the elite and the
        // stability skill. With the fixture's shroud bar moved to the shroud
        // set (Sprint 2, T053) the PvE simulator no longer holds Infusing
        // Terror, and a strike lands inside the window. Audit section 8.
        // Sprint 008 (forms): the shroud bar is live at t=0 with a full pool,
        // and the setup priority spent the window on Infusing Terror's
        // stability again; self-cover no longer earns setup priority
        // (`setup_priority`), so a strike lands inside the window.
        assert!(
            rotation.total_dps > 0.0,
            "gate-sim DPS in the 2 s PvE Solo window (CONN-01-05, re-recorded in Sprint 2)"
        );
        assert_eq!(report.quality, DataQuality::Provisional);
        assert!(
            report.quality_reasons.iter().any(|r| {
                r.field == crate::data::quality::INVENTORY_FIELD
                    && r.explanation.contains("coverage inventory not run for PvE")
            }),
            "PvE with mode records and no timeline must carry the inventory-skip reason: {:?}",
            report.quality_reasons
        );
        assert!(
            !report
                .quality_reasons
                .iter()
                .any(|r| r.field == crate::data::quality::COVERAGE_FIELD),
            "PvE must not reuse the WvW coverage field: {:?}",
            report.quality_reasons
        );
    }

    #[test]
    fn reaper_pvp_comparison_marks_skipped_inventory() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let build = fx::build();
        let ctx = BalanceContext::new(GameMode::PvP);
        let scenario = ScenarioSpec::from_balance_context(&ctx);
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &OptimizationWeights::default(),
            &ctx,
            &scenario,
            &fx::opener(),
        );
        let rotation = report
            .rotation
            .as_ref()
            .expect("PvP runs the gate simulation");
        assert!(rotation.wvw.is_none(), "PvP never runs the timeline");
        assert_eq!(report.quality, DataQuality::Provisional);
        assert!(
            report.quality_reasons.iter().any(|r| {
                r.field == crate::data::quality::INVENTORY_FIELD
                    && r.explanation.contains("coverage inventory not run for PvP")
            }),
            "{:?}",
            report.quality_reasons
        );
    }

    #[test]
    fn reaper_pve_without_mode_records_is_not_blanket_provisional() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let mut build = fx::build();
        build.set_sigil_seats([None, None, None, None]);
        let (ctx, scenario) = fx::pve_scenario();
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &OptimizationWeights::default(),
            &ctx,
            &scenario,
            &fx::opener(),
        );
        assert!(
            report.rotation.as_ref().is_some_and(|r| r.wvw.is_none()),
            "still PvE"
        );
        assert!(
            !report.quality_reasons.iter().any(|r| {
                r.field == crate::data::quality::INVENTORY_FIELD
                    || r.field == crate::data::quality::UNHOSTED_FIELD
            }),
            "no mode records and no unhosted must not emit a skip reason: {:?}",
            report.quality_reasons
        );
    }

    #[test]
    fn heuristic_barrier_and_healing_survive_into_pve_quality() {
        use crate::rotation::reaper_fixture as fx;
        let mut db = fx::db();
        db.skills
            .get_mut(&fx::WELL_OF_DARKNESS)
            .unwrap()
            .description = Some("Grant barrier to nearby allies.".into());
        db.skills
            .get_mut(&fx::YOU_ARE_ALL_WEAKLINGS)
            .unwrap()
            .description = Some("heals you when the shout lands.".into());
        let build = fx::build();
        let (ctx, scenario) = fx::pve_scenario();
        let report = super::evaluate_validated_build_with(
            &build,
            &db,
            "Necromancer",
            &OptimizationWeights::default(),
            &ctx,
            &scenario,
            &fx::opener(),
        );
        let heuristic = report
            .quality_reasons
            .iter()
            .find(|r| r.field == crate::data::quality::HEURISTIC_FIELD)
            .expect("heuristic stamps must reach the scored result");
        assert!(
            heuristic
                .explanation
                .contains("Well of Darkness (heuristic Barrier)"),
            "{heuristic}"
        );
        assert!(
            heuristic.explanation.contains("You Are All Weaklings")
                && heuristic.explanation.contains("heuristic Healing"),
            "{heuristic}"
        );
        assert_eq!(report.quality, DataQuality::Provisional);
        let synergy = crate::engine::synergy_result_from_validated(
            build,
            &db,
            "Necromancer",
            &ctx,
            Some(&scenario),
        );
        assert!(
            synergy.quality_reasons.iter().any(|r| {
                r.field == crate::data::quality::HEURISTIC_FIELD
                    && r.explanation.contains("heuristic Barrier")
            }),
            "Optimize packaging must carry the same stamp: {:?}",
            synergy.quality_reasons
        );
    }

    // ---- Ada Kent suite 1-8: CleanseRate / ControlCoverage split ----
    // Resistance-heavy evidence required: Resistance answers soft control but
    // must not discount the damaging-condition CleanseRate floor.

    /// 1. Resistance uptime no longer lowers the CleanseRate floor.
    #[test]
    fn ada_kent_01_resistance_does_not_discount_cleanse_floor() {
        let scenario = make_wvw_scenario();
        let mut rot = make_viable_rotation();
        rot.buff_uptime.insert("Resistance".into(), 0.75);
        let floor = effective_cleanse_requirement(&scenario, &rot, None);
        let bare = make_viable_rotation();
        let floor_bare = effective_cleanse_requirement(&scenario, &bare, None);
        assert!(
            (floor - floor_bare).abs() < 1e-12,
            "Resistance must not discount CleanseRate floor: with={floor} bare={floor_bare}"
        );
        assert!(
            (floor - required_cleanse_rate(&scenario)).abs() < 1e-12,
            "floor should equal scenario cleanse rate: {floor}"
        );
    }

    /// 2. Resistance-heavy / cleanse-light: CleanseRate fails, ControlCoverage passes.
    #[test]
    fn ada_kent_02_resistance_heavy_split_evidence() {
        let mut rot = make_viable_rotation();
        rot.cleanse_count = 0;
        rot.cleanse_rate_per_20s = 0.0;
        rot.stunbreak_count = 0;
        rot.buff_uptime.insert("Resistance".into(), 0.80);
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let cl = gate_by_kind(&report.gates, &ViabilityGate::CleanseRate).unwrap();
        let cc = gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).unwrap();
        assert!(!cl.passed, "damaging cleanse still required: {}", cl.note);
        assert!(cc.passed, "Resistance covers soft control: {}", cc.note);
        assert!(cc.note.contains("Resistance"), "{}", cc.note);
    }

    /// 3. Cleanse alone covers ControlCoverage without Resistance or stunbreak.
    #[test]
    fn ada_kent_03_cleanse_alone_passes_control_coverage() {
        let mut rot = make_viable_rotation();
        rot.buff_uptime.remove("Resistance");
        rot.stunbreak_count = 0;
        rot.cleanse_count = 2;
        rot.cleanse_rate_per_20s = 4.0;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let cc = gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).unwrap();
        assert!(cc.passed, "{}", cc.note);
        assert!(cc.note.contains("cleanse"), "{}", cc.note);
    }

    /// 4. Stunbreak alone covers ControlCoverage; CleanseRate still independent.
    #[test]
    fn ada_kent_04_stunbreak_alone_passes_control_coverage() {
        let mut rot = make_viable_rotation();
        rot.cleanse_count = 0;
        rot.cleanse_rate_per_20s = 0.0;
        rot.buff_uptime.remove("Resistance");
        rot.stunbreak_count = 1;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let cc = gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).unwrap();
        let cl = gate_by_kind(&report.gates, &ViabilityGate::CleanseRate).unwrap();
        assert!(cc.passed, "{}", cc.note);
        assert!(cc.note.contains("stunbreak"), "{}", cc.note);
        assert!(!cl.passed, "CleanseRate remains its own gate: {}", cl.note);
    }

    /// 5. No cleanse, no Resistance, no stunbreak -> ControlCoverage fails.
    #[test]
    fn ada_kent_05_no_soft_control_answer_fails() {
        let mut rot = make_viable_rotation();
        rot.cleanse_count = 0;
        rot.cleanse_rate_per_20s = 0.0;
        rot.stunbreak_count = 0;
        rot.buff_uptime.remove("Resistance");
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let cc = gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).unwrap();
        assert!(!cc.passed, "{}", cc.note);
        assert!(!report.is_viable);
    }

    /// 6. PvE emits neither CleanseRate nor ControlCoverage.
    #[test]
    fn ada_kent_06_pve_emits_neither_cleanse_nor_control() {
        let rot = make_viable_rotation();
        let mut combat = make_viable_combat();
        combat.effective_health = EHP_FLOOR_PVE + 1_000.0;
        let scenario = make_pve_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        assert!(gate_by_kind(&report.gates, &ViabilityGate::CleanseRate).is_none());
        assert!(gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).is_none());
    }

    /// 7. search_rank is [i64;10]; ControlCoverage contributes at most +1 to key2 on WvW.
    #[test]
    fn ada_kent_07_search_rank_key1_plus_one_on_control_pass() {
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let mut with_cc = make_viable_rotation();
        with_cc.buff_uptime.insert("Resistance".into(), 0.5);
        let pass = evaluate_viability_gates(Some(&with_cc), &combat, &scenario);
        assert!(
            gate_by_kind(&pass.gates, &ViabilityGate::ControlCoverage)
                .unwrap()
                .passed
        );
        let mut fail_cc = with_cc.clone();
        fail_cc.cleanse_count = 0;
        fail_cc.cleanse_rate_per_20s = 0.0;
        fail_cc.stunbreak_count = 0;
        fail_cc.buff_uptime.clear();
        let fail = evaluate_viability_gates(Some(&fail_cc), &combat, &scenario);
        assert!(
            !gate_by_kind(&fail.gates, &ViabilityGate::ControlCoverage)
                .unwrap()
                .passed
        );

        let rank_pass = search_rank(&RefereeReport {
            scenario: scenario.clone(),
            stats: crate::stats::StatBlock::default(),
            modifiers: crate::combat::DamageModifiers::default(),
            combat_solo: combat.clone(),
            combat_party: combat.clone(),
            combat_squad: combat.clone(),
            primary_combat: combat.clone(),
            rotation: Some(with_cc),
            viability: pass.clone(),
            user_intent_score: 0.0,
            raw_direction_score: 0.0,
            ranked_direction_score: 0.0,
            intent_similarity: None,
            intent_alignment: None,
            realized: Default::default(),
            stat_direction_score: 0.0,
            quality: DataQuality::Verified,
            quality_reasons: Vec::new(),
        });
        // Key 1 is intent alignment; the passed-gate count is key 2.
        assert_eq!(rank_pass.len(), 10);
        let passed_gates = pass.gates.iter().filter(|g| g.passed).count() as i64;
        assert_eq!(rank_pass[2], passed_gates, "key2 is passed-gate count");
        assert!(
            gate_by_kind(&pass.gates, &ViabilityGate::ControlCoverage).is_some()
                && gate_by_kind(&fail.gates, &ViabilityGate::ControlCoverage).is_some(),
            "ControlCoverage present on WvW; key2 max +1 vs pre-split"
        );
    }

    /// 8. StunbreakCount remains a separate hard-CC gate from ControlCoverage.
    #[test]
    fn ada_kent_08_stunbreak_count_stays_separate_hard_cc_gate() {
        let mut rot = make_viable_rotation();
        rot.stunbreak_count = 0;
        rot.buff_uptime.insert("Resistance".into(), 0.6);
        rot.cleanse_count = 2;
        rot.cleanse_rate_per_20s = 4.0;
        let combat = make_viable_combat();
        let scenario = make_wvw_scenario();
        let report = evaluate_viability_gates(Some(&rot), &combat, &scenario);
        let cc = gate_by_kind(&report.gates, &ViabilityGate::ControlCoverage).unwrap();
        let sb = gate_by_kind(&report.gates, &ViabilityGate::StunbreakCount).unwrap();
        assert!(cc.passed, "Resistance covers soft control: {}", cc.note);
        assert!(
            !sb.passed,
            "StunbreakCount still required for hard CC: {}",
            sb.note
        );
        assert!(!report.is_viable);
    }
}
