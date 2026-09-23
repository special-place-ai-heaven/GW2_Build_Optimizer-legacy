//! Observables, diffs and fidelity bands: what the log measured next to what
//! our referee produced for the reconstructed kit.
//!
//! Rates, never totals: the referee's windows are fixed, the log's are not.
//! Every error is signed (ours - log) so a band can report bias; bands use
//! its absolute value for median and p90. A diff the simulator cannot
//! measure abstains (`ours: None`) and its note names why (doctrine rule 6).
//! `simulate` is the addon path and nothing else (doctrine rule 8):
//! `plate_from` -> `validate_gemini_build` -> `ScenarioSpec::for_request` ->
//! `evaluate_validated_build_with`, then the referee's own 60 s flow run
//! (`prepare_validated_rotation` -> `simulate_flow`, as `evaluate_inner`
//! calls them) for everything but the WvW timeline. The referee's gate
//! simulation is 2-20 s and would score a handful of casts against a whole
//! fight. The flow run ignores the opener; the referee still gets it, so the
//! WvW timeline follows it.
//!
//! The comparator measures the simulator, not viability: a kit that fails a
//! blocking gate is still compared and banded, its failed gates recorded in
//! [`PlayerComparison::gates`]. Only reconstruction and validator refusals
//! are `refused`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use super::ei_log::{EiLog, EiPlayer};
use super::kit::{self, ReconstructedKit};
use crate::balance::BalanceContext;
use crate::benchmark::{self, BenchmarkBuild};
use crate::engine;
use crate::gamedb::GameDb;
use crate::referee::{self, RefereeReport};
use crate::rotation::{SimulationResult, WvwCombatReport};
use crate::scenario::{RoleObjective, ScenarioSpec};

/// The 11 duration boons, by EI `buffMap` name (= our `buff_slots` name), and
/// the observable each is reported under. Might is its own observable.
const BOONS: [(&str, &str); 11] = [
    ("Aegis", "uptime:Aegis"),
    ("Alacrity", "uptime:Alacrity"),
    ("Fury", "uptime:Fury"),
    ("Protection", "uptime:Protection"),
    ("Quickness", "uptime:Quickness"),
    ("Regeneration", "uptime:Regeneration"),
    ("Resistance", "uptime:Resistance"),
    ("Resolution", "uptime:Resolution"),
    ("Stability", "uptime:Stability"),
    ("Swiftness", "uptime:Swiftness"),
    ("Vigor", "uptime:Vigor"),
];

/// Every condition tick, both sides, is one skill-share row.
const CONDITIONS: &str = "Conditions";
/// Floors for relative errors: below these the log number is noise.
const DPS_FLOOR: f64 = 100.0;
const CLEANSE_FLOOR: f64 = 0.5;
/// Might cap, so the Might error is on the same 0..1 scale as uptimes.
const MIGHT_CAP: f64 = 25.0;

/// Log-side numbers for one player, whole-fight phase `[0]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observed {
    pub active_s: f64,
    pub engaged_s: f64,
    /// `dpsAll[0].damage / engaged_s` (engaged floored at 1 s).
    pub dps_engaged: f64,
    /// `dpsAll[0].damage / active_s`.
    pub dps_active: f64,
    /// `condiDamage / damage`, 0..1.
    pub condi_fraction: f64,
    /// `totalDamageDist[0]` share by skill name; every `indirectDamage` row
    /// in one `Conditions` row. Sums to 1 when there is any damage.
    pub skill_share: BTreeMap<String, f64>,
    /// Share of strike damage whose id `db.skills` lacks (EI synthetic or
    /// custom ids). Those rows keep EI's name, else `id <n>`.
    pub unmapped_share: f64,
    /// Share of rows EI flags `isTraitProc` or `isGearProc`.
    pub proc_share: f64,
    /// Boon name -> 0..1, from every source. Duration boons read
    /// `uptime / 100`; a stacking one (Stability) reads `presence / 100`,
    /// because EI's `uptime` for an intensity buff is average stacks. Absent
    /// boon = 0. Context only: the simulator is self-only.
    pub uptime: BTreeMap<&'static str, f64>,
    /// The share of `uptime` this player generated on themself:
    /// `generated[name] / 100`, stacking boons `generatedPresence[name] / 100`.
    /// `None` when the log has the boon but no source map for it. Absent
    /// boon = `Some(0)`.
    pub uptime_self: BTreeMap<&'static str, Option<f64>>,
    /// Might `uptime`: EI's average stacks, every source.
    pub might_stacks: f64,
    /// Might `generated[name]`: average stacks this player gave themself.
    pub might_self: Option<f64>,
    /// `(condiCleanse + condiCleanseSelf) / active_s * 20`.
    pub cleanse_per_20s: f64,
    /// `stunBreak` uses.
    pub stunbreaks: u32,
    /// `damageTaken / active_s`.
    pub incoming_dps: f64,
    /// `downCount > 0`.
    pub downed: bool,
}

/// Simulator-side numbers: the 60 s flow `SimulationResult`, plus the WvW
/// timeline from the referee's gate run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Simulated {
    pub dps: f64,
    pub condi_fraction: f64,
    /// `SkillUsage.dps_contribution / total_dps` by name (strike only), plus
    /// `Conditions` = `condition_dps / total_dps`.
    pub skill_share: BTreeMap<String, f64>,
    /// Share of rotation rows named like a trait.
    pub proc_share: f64,
    pub uptime: BTreeMap<&'static str, f64>,
    pub might_stacks: f64,
    pub cleanse_per_20s: f64,
    /// True when `cleanse_per_20s` counts WvW timeline cleanse events; false
    /// when it is the kit's theoretical `cleanse_rate_per_20s`.
    pub cleanse_measured: bool,
    /// Equipped stunbreak skills, not uses.
    pub stunbreak_skills: u32,
    /// WvW timeline only: `incoming_damage / sim_s`.
    pub incoming_dps: Option<f64>,
    /// WvW timeline only: `!player_survived`.
    pub downed: Option<bool>,
    /// WvW timeline only: its length in seconds (the referee's gate window).
    pub sim_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    pub observable: &'static str,
    pub log: f64,
    /// `None` = abstain; `note` names why.
    pub ours: Option<f64>,
    /// Signed, ours minus log, in the observable's error unit.
    pub error: Option<f64>,
    pub note: String,
}

pub struct PlayerComparison {
    pub log: String,
    pub player: String,
    pub profession: String,
    pub spec: String,
    pub mode: String,
    /// `None` when the kit could not be reconstructed; `refused` says why.
    pub kit: Option<ReconstructedKit>,
    pub diffs: Vec<Diff>,
    /// Skill name, log share, our share; log share descending.
    pub share: Vec<(String, f64, f64)>,
    /// See [`Observed::unmapped_share`].
    pub unmapped_share: f64,
    /// Failed blocking gates by name; empty when all pass or not simulated.
    /// Recorded, never a refusal.
    pub gates: Vec<String>,
    /// Kit or validator refusal. Refused rows are not banded.
    pub refused: Option<String>,
}

pub fn observe(log: &EiLog, p: &EiPlayer, db: &GameDb) -> Observed {
    observe_with(log, p, |id| db.skills.get(&id).map(|s| s.name.as_str()))
}

/// `db_name(id)`: the skill's db name, `None` when the db lacks it.
fn observe_with<'a>(
    log: &EiLog,
    p: &EiPlayer,
    db_name: impl Fn(u32) -> Option<&'a str>,
) -> Observed {
    let active_s = p.active_times.first().copied().unwrap_or(0) as f64 / 1000.0;
    let engaged_s = f64::from(p.engaged_seconds());
    let dps = p.dps_all.first().cloned().unwrap_or_default();
    let damage = dps.damage as f64;
    let per_active = |x: f64| if active_s > 0.0 { x / active_s } else { 0.0 };

    let rows = p.total_damage_dist.first().map_or(&[][..], |r| &r[..]);
    let dist_total: f64 = rows.iter().map(|r| r.total_damage as f64).sum();
    let mut skill_share = BTreeMap::new();
    let (mut unmapped, mut proc) = (0.0, 0.0);
    for r in rows {
        let dmg = r.total_damage as f64;
        let name = if r.indirect_damage {
            CONDITIONS.to_string()
        } else {
            let id = u32::try_from(r.id).ok();
            let info = id.and_then(|id| log.skill_info(id));
            if info.is_some_and(|i| i.is_trait_proc || i.is_gear_proc) {
                proc += dmg;
            }
            match id.and_then(&db_name) {
                Some(n) => n.to_string(),
                None => {
                    unmapped += dmg;
                    info.map_or_else(|| format!("id {}", r.id), |i| i.name.clone())
                }
            }
        };
        *skill_share.entry(name).or_insert(0.0) += dmg;
    }
    let of_dist = |x: f64| {
        if dist_total > 0.0 {
            x / dist_total
        } else {
            0.0
        }
    };
    for v in skill_share.values_mut() {
        *v = of_dist(*v);
    }

    let mut uptime: BTreeMap<&'static str, f64> = BOONS.iter().map(|&(b, _)| (b, 0.0)).collect();
    let mut uptime_self: BTreeMap<&'static str, Option<f64>> =
        BOONS.iter().map(|&(b, _)| (b, Some(0.0))).collect();
    let (mut might_stacks, mut might_self) = (0.0, Some(0.0));
    let own = |m: &Option<BTreeMap<String, f64>>| {
        m.as_ref().map(|m| m.get(&p.name).copied().unwrap_or(0.0))
    };
    for b in &p.buff_uptimes {
        let Some(info) = u32::try_from(b.id)
            .ok()
            .and_then(|id| log.buff_map.get(&format!("b{id}")))
        else {
            continue;
        };
        let Some(d) = b.buff_data.first() else {
            continue;
        };
        if info.name == "Might" {
            might_stacks = d.uptime;
            might_self = own(&d.generated);
        } else if let Some(slot) = uptime.get_mut(info.name.as_str()) {
            *slot = if info.stacking { d.presence } else { d.uptime } / 100.0;
            let share = own(if info.stacking {
                &d.generated_presence
            } else {
                &d.generated
            });
            if let Some(s) = uptime_self.get_mut(info.name.as_str()) {
                *s = share.map(|x| x / 100.0);
            }
        }
    }

    let support = p.support.first().cloned().unwrap_or_default();
    let defenses = p.defenses.first().cloned().unwrap_or_default();
    Observed {
        active_s,
        engaged_s,
        dps_engaged: damage / engaged_s.max(1.0),
        dps_active: per_active(damage),
        condi_fraction: if damage > 0.0 {
            dps.condi_damage as f64 / damage
        } else {
            0.0
        },
        skill_share,
        unmapped_share: of_dist(unmapped),
        proc_share: of_dist(proc),
        uptime,
        uptime_self,
        might_stacks,
        might_self,
        cleanse_per_20s: per_active(f64::from(
            support.condi_cleanse + support.condi_cleanse_self,
        )) * 20.0,
        stunbreaks: support.stun_break,
        incoming_dps: per_active(defenses.damage_taken as f64),
        downed: defenses.down_count > 0,
    }
}

/// The objective `published_scenario` chose from the build's role words,
/// recovered from the scenario it returns (profile id + combat kind) instead
/// of re-parsing the words here. `combat_kind.role_objective()` alone is
/// lossy (Healer -> Buffer, Sustain -> PowerDps) and is only the fallback.
fn published_objective(build: &BenchmarkBuild) -> RoleObjective {
    use RoleObjective::*;
    let base = benchmark::published_scenario(build);
    // The objectives published_scenario can produce.
    [
        Healer, Buffer, Tank, Disabler, Sustain, WvWRoamer, CondiDps, PowerDps,
    ]
    .into_iter()
    .find(|r| {
        Some(r.profile_id_for(&base.game_mode, base.combat_tier))
            == base.objective_profile_id.as_deref()
            && r.combat_kind_for_weights(&r.to_weights_for(&base.game_mode, base.combat_tier))
                == base.combat_kind
    })
    .unwrap_or_else(|| base.combat_kind.role_objective())
}

/// The addon path for a reconstructed kit. Role from the neighbour's
/// published words, scale from the log's measured squad size.
pub fn simulate(
    kit: &ReconstructedKit,
    log: &EiLog,
    db: &GameDb,
) -> Result<(Simulated, RefereeReport), String> {
    let profession = &kit.build.profession;
    let validated = kit::validate(&kit.build, db)?;
    let (mode, tier) = (log.mode(), log.tier());
    let objective = published_objective(&kit.build);
    let ctx = BalanceContext::new(mode.clone());
    let weights = objective.to_weights_for(&mode, tier);
    let scenario = ScenarioSpec::for_request(&ctx, tier, Some(objective), &weights);
    let report = referee::evaluate_validated_build_with(
        &validated,
        db,
        profession,
        &weights,
        &ctx,
        &scenario,
        &kit.opener,
    );
    let prepared =
        engine::prepare_validated_rotation(&validated, db, &report.stats, Some(&scenario))
            .ok_or("no rotation: the bar resolved to no skills")?;
    let flow = engine::simulate_flow(&prepared, &weights, Some(&scenario));
    let timeline = report.rotation.as_ref().and_then(|r| r.wvw.as_ref());
    let trait_names: BTreeSet<&str> = db.traits.values().map(|t| t.name.as_str()).collect();
    let sim = simulated_from(&flow, timeline, |n| trait_names.contains(n));
    Ok((sim, report))
}

/// `r`: the flow run. `wvw`: the referee's WvW timeline, if any.
fn simulated_from(
    r: &SimulationResult,
    wvw: Option<&WvwCombatReport>,
    is_proc: impl Fn(&str) -> bool,
) -> Simulated {
    let of_total = |x: f64| {
        if r.total_dps > 0.0 {
            x / r.total_dps
        } else {
            0.0
        }
    };
    let mut skill_share = BTreeMap::new();
    for u in &r.skill_usage {
        *skill_share.entry(u.name.clone()).or_insert(0.0) += of_total(u.dps_contribution);
    }
    if r.condition_dps > 0.0 {
        skill_share.insert(CONDITIONS.to_string(), of_total(r.condition_dps));
    }
    // ponytail: trait-named rotation rows only; sigil/relic procs never
    // reach skill_usage. Add an id or source tag to SkillUsage if it matters.
    let proc_share = r
        .skill_usage
        .iter()
        .filter(|u| is_proc(&u.name))
        .map(|u| of_total(u.dps_contribution))
        .sum();
    let sim_s = |ms: u32| f64::from(ms.max(1)) / 1000.0;
    let (cleanse_per_20s, cleanse_measured) = match wvw {
        Some(w) => (
            f64::from(w.conditions_cleansed + w.ally_cleanses) / sim_s(w.duration_ms) * 20.0,
            true,
        ),
        None => (r.cleanse_rate_per_20s, false),
    };
    Simulated {
        dps: r.total_dps,
        condi_fraction: of_total(r.condition_dps),
        skill_share,
        proc_share,
        uptime: BOONS
            .iter()
            .map(|&(b, _)| (b, r.buff_uptime.get(b).copied().unwrap_or(0.0)))
            .collect(),
        might_stacks: r.might_stacks_avg,
        cleanse_per_20s,
        cleanse_measured,
        stunbreak_skills: r.stunbreak_count,
        incoming_dps: wvw.map(|w| w.incoming_damage / sim_s(w.duration_ms)),
        downed: wvw.map(|w| !w.player_survived),
        sim_s: wvw.map(|w| sim_s(w.duration_ms)),
    }
}

fn rel(ours: f64, log: f64, floor: f64) -> f64 {
    (ours - log) / log.max(floor)
}

/// Total variation distance over the union of names: 0 = same
/// distribution, 1 = disjoint.
fn tvd(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> f64 {
    let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    0.5 * names
        .into_iter()
        .map(|n| (a.get(n).unwrap_or(&0.0) - b.get(n).unwrap_or(&0.0)).abs())
        .sum::<f64>()
}

fn scored(observable: &'static str, log: f64, ours: f64, error: f64, note: &str) -> Diff {
    Diff {
        observable,
        log,
        ours: Some(ours),
        error: Some(error),
        note: note.to_string(),
    }
}

fn abstain(observable: &'static str, log: f64, note: String) -> Diff {
    Diff {
        observable,
        log,
        ours: None,
        error: None,
        note,
    }
}

fn diffs(o: &Observed, s: &Simulated, wvw: bool) -> Vec<Diff> {
    let mut out = vec![
        scored(
            "dps_engaged",
            o.dps_engaged,
            s.dps,
            rel(s.dps, o.dps_engaged, DPS_FLOOR),
            "",
        ),
        scored(
            "dps_active",
            o.dps_active,
            s.dps,
            rel(s.dps, o.dps_active, DPS_FLOOR),
            "",
        ),
        scored(
            "condi_fraction",
            o.condi_fraction,
            s.condi_fraction,
            s.condi_fraction - o.condi_fraction,
            "",
        ),
    ];
    let t = tvd(&o.skill_share, &s.skill_share);
    out.push(scored(
        "skill_share",
        0.0,
        t,
        t,
        &format!(
            "total variation over {} log / {} sim names",
            o.skill_share.len(),
            s.skill_share.len()
        ),
    ));
    out.push(scored(
        "proc_share",
        o.proc_share,
        s.proc_share,
        s.proc_share - o.proc_share,
        "ours: trait-named rotation rows; sigil/relic procs unattributed",
    ));
    // The sim is self-only, so it is scored against the player's own share;
    // the all-source total is printed beside it.
    let no_source = "log has no source split for this boon";
    for &(boon, observable) in &BOONS {
        let (total, ours) = (o.uptime[boon], s.uptime[boon]);
        out.push(match o.uptime_self[boon] {
            Some(l) => scored(
                observable,
                l,
                ours,
                ours - l,
                &format!("log self-generated; total {total:.3}"),
            ),
            None => abstain(observable, total, format!("{no_source}; total {total:.3}")),
        });
    }
    let total = o.might_stacks;
    out.push(match o.might_self {
        Some(l) => scored(
            "might_stacks",
            l,
            s.might_stacks,
            (s.might_stacks - l) / MIGHT_CAP,
            &format!("log self-generated; total {total:.3}; error in 25-stack units"),
        ),
        None => abstain(
            "might_stacks",
            total,
            format!("{no_source}; total {total:.3}"),
        ),
    });
    let sim_s = s.sim_s.unwrap_or(0.0);
    out.push(scored(
        "cleanse_per_20s",
        o.cleanse_per_20s,
        s.cleanse_per_20s,
        rel(s.cleanse_per_20s, o.cleanse_per_20s, CLEANSE_FLOOR),
        &if s.cleanse_measured {
            format!("ours: WvW timeline cleanse events, self + allies, sim_s {sim_s:.1}")
        } else {
            "ours: kit rate from cooldowns, not casts".to_string()
        },
    ));
    let agree = (o.stunbreaks > 0) == (s.stunbreak_skills > 0);
    out.push(abstain(
        "stunbreak",
        f64::from(o.stunbreaks),
        format!(
            "simulator counts stunbreak skills ({}), not uses; presence {}",
            s.stunbreak_skills,
            if agree { "agrees" } else { "disagrees" }
        ),
    ));
    out.push(match (wvw, s.incoming_dps) {
        (false, _) => abstain(
            "incoming_dps",
            o.incoming_dps,
            "PvE has no incoming-damage model".into(),
        ),
        (true, None) => abstain(
            "incoming_dps",
            o.incoming_dps,
            "no WvW timeline report".into(),
        ),
        (true, Some(ours)) => scored(
            "incoming_dps",
            o.incoming_dps,
            ours,
            rel(ours, o.incoming_dps, DPS_FLOOR),
            &format!("scripted scenario pressure, sim_s {sim_s:.1}, vs the real fight"),
        ),
    });
    // A yes/no over the gate window says nothing about a whole fight.
    let log_downed = f64::from(u8::from(o.downed));
    out.push(match (wvw, s.downed) {
        (false, _) => abstain(
            "downed",
            log_downed,
            "PvE sim `downed` is the target dummy, not the player".into(),
        ),
        (true, None) => abstain("downed", log_downed, "no WvW timeline report".into()),
        (true, Some(d)) => abstain(
            "downed",
            log_downed,
            format!(
                "gate window {sim_s:.0} s vs fight {:.0} s; ours downed {d}",
                o.active_s
            ),
        ),
    });
    out
}

fn share_rows(o: &Observed, s: &Simulated) -> Vec<(String, f64, f64)> {
    let names: BTreeSet<&String> = o.skill_share.keys().chain(s.skill_share.keys()).collect();
    let mut rows: Vec<(String, f64, f64)> = names
        .into_iter()
        .map(|n| {
            let get = |m: &BTreeMap<String, f64>| m.get(n).copied().unwrap_or(0.0);
            (n.clone(), get(&o.skill_share), get(&s.skill_share))
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.total_cmp(&a.2)));
    rows
}

/// Every squad player of one log. `codes`: character name -> chat code.
pub fn compare_log(
    log_name: &str,
    log: &EiLog,
    codes: &HashMap<String, String>,
    corpus: &[BenchmarkBuild],
    db: &GameDb,
) -> Vec<PlayerComparison> {
    let mode = log.mode();
    let wvw = mode == gw2_core::types::GameMode::WvW;
    log.squad()
        .map(|p| {
            let observed = observe(log, p, db);
            let mut row = PlayerComparison {
                log: log_name.to_string(),
                player: p.name.clone(),
                profession: p.profession.clone(),
                spec: p.profession.clone(),
                mode: format!("{mode:?}"),
                kit: None,
                diffs: Vec::new(),
                share: Vec::new(),
                unmapped_share: observed.unmapped_share,
                gates: Vec::new(),
                refused: None,
            };
            let code = codes.get(&p.name).map(String::as_str);
            let kit = match kit::reconstruct(log, p, code, corpus, db) {
                Ok(k) => k,
                Err(e) => {
                    row.refused = Some(format!("kit: {e}"));
                    return row;
                }
            };
            row.profession = kit.build.profession.clone();
            match simulate(&kit, log, db) {
                Ok((sim, report)) => {
                    row.diffs = diffs(&observed, &sim, wvw);
                    row.share = share_rows(&observed, &sim);
                    row.gates = report
                        .viability
                        .gates
                        .iter()
                        .filter(|g| !g.passed && !g.skipped && g.gate.blocks())
                        .map(|g| format!("{:?}", g.gate))
                        .collect();
                }
                Err(e) => row.refused = Some(e),
            }
            row.kit = Some(kit);
            row
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Band {
    pub n: usize,
    pub median_abs: f64,
    /// Nearest rank: the ceil(0.9 n)-th smallest |error|.
    pub p90_abs: f64,
    /// Mean signed error.
    pub bias: f64,
    /// Abstained diffs, counted, never scored.
    pub abstained: usize,
}

/// (profession, spec, mode, observable). Keyed by the log's spec so opposite
/// sub-archetypes of one profession (a medic Firebrand, a power
/// Dragonhunter) never share a band; the profession leads so rows group.
pub type BandKey = (String, String, String, &'static str);

/// Per [`BandKey`] over compared, unrefused players. `dps_active` is printed
/// but not banded in WvW (target availability).
pub fn bands(rows: &[PlayerComparison]) -> BTreeMap<BandKey, Band> {
    let mut errs: BTreeMap<BandKey, (Vec<f64>, usize)> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.refused.is_none()) {
        for d in &row.diffs {
            if d.observable == "dps_active" && row.mode == "WvW" {
                continue;
            }
            let e = errs
                .entry((
                    row.profession.clone(),
                    row.spec.clone(),
                    row.mode.clone(),
                    d.observable,
                ))
                .or_default();
            match d.error {
                Some(x) => e.0.push(x),
                None => e.1 += 1,
            }
        }
    }
    errs.into_iter()
        .map(|(k, (signed, abstained))| {
            let n = signed.len();
            let mut abs: Vec<f64> = signed.iter().map(|x| x.abs()).collect();
            abs.sort_by(f64::total_cmp);
            let band = if n == 0 {
                Band {
                    abstained,
                    ..Band::default()
                }
            } else {
                Band {
                    n,
                    median_abs: if n % 2 == 1 {
                        abs[n / 2]
                    } else {
                        (abs[n / 2 - 1] + abs[n / 2]) / 2.0
                    },
                    p90_abs: abs[(n * 9).div_ceil(10) - 1],
                    bias: signed.iter().sum::<f64>() / n as f64,
                    abstained,
                }
            };
            (k, band)
        })
        .collect()
}

/// Three decimals; a value that rounds to zero prints unsigned, never -0.000.
fn opt(x: Option<f64>) -> String {
    x.map_or("-".into(), |v| {
        format!("{:.3}", if v.abs() < 5e-4 { v.abs() } else { v })
    })
}

pub fn render_table(rows: &[PlayerComparison]) -> String {
    let mut out = String::new();
    out.push_str("| Log | Player | Spec | Mode | Provenance specs/traits/skills/weapons/gear | Neighbour | Unmapped | Gates | Status |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for r in rows {
        let (prov, neighbour) = match &r.kit {
            Some(k) => (
                format!(
                    "{:?}/{:?}/{:?}/{:?}/{:?}",
                    k.specs, k.traits, k.skills, k.weapons, k.gear
                ),
                k.neighbour.clone().unwrap_or_else(|| "-".into()),
            ),
            None => ("-".into(), "-".into()),
        };
        let gates = if r.refused.is_some() {
            "-".to_string()
        } else if r.gates.is_empty() {
            "all pass".to_string()
        } else {
            r.gates.join(", ")
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {:.3} | {} | {} |",
            r.log,
            r.player,
            r.spec,
            r.mode,
            prov,
            neighbour,
            r.unmapped_share,
            gates,
            r.refused.as_deref().unwrap_or("compared")
        );
    }
    for r in rows.iter().filter(|r| !r.diffs.is_empty()) {
        let _ = writeln!(out, "\n{} / {} ({} {})", r.log, r.player, r.spec, r.mode);
        if let Some(k) = &r.kit {
            for f in &k.stat_flags {
                let _ = writeln!(out, "stat flag: {f}");
            }
        }
        out.push_str("| Observable | Log | Ours | Error | Note |\n|---|---|---|---|---|\n");
        for d in &r.diffs {
            let _ = writeln!(
                out,
                "| {} | {:.3} | {} | {} | {} |",
                d.observable,
                d.log,
                opt(d.ours),
                opt(d.error),
                d.note
            );
        }
        out.push_str("| Skill | Log share | Our share |\n|---|---|---|\n");
        for (name, l, o) in &r.share {
            let _ = writeln!(out, "| {name} | {l:.3} | {o:.3} |");
        }
    }
    out
}

pub fn render_bands(b: &BTreeMap<BandKey, Band>) -> String {
    let mut out = String::from(
        "| Profession | Spec | Mode | Observable | n | Median abs | P90 abs | Bias | Abstained |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    for ((profession, spec, mode, observable), band) in b {
        let _ = writeln!(
            out,
            "| {profession} | {spec} | {mode} | {observable} | {} | {:.3} | {:.3} | {} | {} |",
            band.n,
            band.median_abs,
            band.p90_abs,
            opt(Some(band.bias)),
            band.abstained
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fidelity::ei_log::parse;

    const LRBJ: &str = include_str!("../../tests/fixtures/ei_logs/lRBj-20260604-210631_wvw.json");

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    fn diff<'a>(ds: &'a [Diff], name: &str) -> &'a Diff {
        ds.iter()
            .find(|d| d.observable == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    // Hand numbers: a Python pass (json.load) over the trimmed lRBj
    // fixture, player "Aster Menimem" (Firebrand). active_s =
    // activeTimes[0]/1000 = 71.169; engaged = damage1S[0] rises = 19;
    // dpsAll[0] damage 18473, condiDamage 2007. Shares over the sum of
    // totalDamageDist[0] (18473), strike rows named by skillMap (the test's
    // stand-in for db.skills, so nothing is unmapped), indirect rows into
    // Conditions. Boons: buffUptimes[.].buffData[0].uptime / 100, except
    // stacking Stability = presence 32.781 / 100; Might uptime 5.249 stacks.
    // Self shares: buffData[0].generated["Aster Menimem"] / 100 (Swiftness
    // 23.572, Aegis 13.346, Quickness 19.99, Fury 0: others gave it), Stability
    // generatedPresence 32.781 / 100, Might generated 3.022 stacks.
    // support[0] condiCleanse 15 + condiCleanseSelf 16; stunBreak 0;
    // defenses[0] damageTaken / active_s, downCount 1.
    #[test]
    fn observe_matches_hand_numbers_on_a_wvw_firebrand() {
        let log = parse(LRBJ).expect("parses");
        let p = log
            .players
            .iter()
            .find(|p| p.name == "Aster Menimem")
            .expect("fixture player");
        let o = observe_with(&log, p, |id| log.skill_info(id).map(|s| s.name.as_str()));
        close(o.active_s, 71.169);
        close(o.engaged_s, 19.0);
        close(o.dps_engaged, 18473.0 / 19.0);
        close(o.dps_active, 18473.0 / 71.169);
        close(o.condi_fraction, 2007.0 / 18473.0);
        close(o.skill_share["Daybreaking Slash"], 3607.0 / 18473.0);
        close(o.skill_share["Symbol of Faith"], 2712.0 / 18473.0);
        close(o.skill_share[CONDITIONS], 2007.0 / 18473.0);
        close(o.skill_share.values().sum(), 1.0);
        close(o.unmapped_share, 0.0);
        close(o.proc_share, 0.008498890272289287);
        close(o.uptime["Swiftness"], 0.77766);
        close(o.uptime["Aegis"], 0.16306);
        close(o.uptime["Stability"], 0.32781);
        close(o.uptime["Alacrity"], 0.0); // absent from the log
        close(o.might_stacks, 5.249);
        let own = |b: &str| o.uptime_self[b].expect("source map present");
        close(own("Swiftness"), 0.23572);
        close(own("Aegis"), 0.13346);
        close(own("Quickness"), 0.1999);
        close(own("Fury"), 0.0);
        close(own("Stability"), 0.32781);
        close(own("Alacrity"), 0.0);
        close(o.might_self.expect("might source map"), 3.022);
        close(o.cleanse_per_20s, 31.0 / 71.169 * 20.0);
        assert_eq!(o.stunbreaks, 0);
        close(o.incoming_dps, 2018.3225842712418);
        assert!(o.downed);

        // With an empty db every strike row is unmapped, keeps EI's name.
        let bare = observe_with(&log, p, |_| None);
        close(bare.unmapped_share, 1.0 - 2007.0 / 18473.0);
        close(bare.skill_share["Daybreaking Slash"], 3607.0 / 18473.0);
    }

    fn dist(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|&(n, v)| (n.to_string(), v)).collect()
    }

    #[test]
    fn tvd_is_half_the_l1_over_the_union() {
        let a = dist(&[("A", 0.5), ("B", 0.3), ("Conditions", 0.2)]);
        let b = dist(&[("A", 0.4), ("C", 0.4), ("Conditions", 0.2)]);
        // |0.5-0.4| + |0.3-0| + |0-0.4| + 0 = 0.8 -> 0.4
        close(tvd(&a, &b), 0.4);
        close(tvd(&a, &a), 0.0);
        close(tvd(&dist(&[("A", 1.0)]), &dist(&[("B", 1.0)])), 1.0);
    }

    #[test]
    fn relative_errors_floor_small_log_values() {
        close(rel(1200.0, 1000.0, DPS_FLOOR), 0.2);
        close(rel(50.0, 10.0, DPS_FLOOR), 0.4); // floored at 100
        close(rel(1.5, 0.0, CLEANSE_FLOOR), 3.0); // floored at 0.5
        close(rel(2.0, 4.0, CLEANSE_FLOOR), -0.5);
    }

    fn hand_pair() -> (Observed, Simulated) {
        let o = Observed {
            dps_engaged: 2000.0,
            dps_active: 1000.0,
            condi_fraction: 0.25,
            skill_share: dist(&[("A", 0.75), ("Conditions", 0.25)]),
            proc_share: 0.1,
            uptime: BOONS.iter().map(|&(b, _)| (b, 0.9)).collect(),
            uptime_self: BOONS.iter().map(|&(b, _)| (b, Some(0.5))).collect(),
            might_stacks: 25.0,
            might_self: Some(20.0),
            active_s: 60.0,
            cleanse_per_20s: 0.2,
            stunbreaks: 3,
            incoming_dps: 1500.0,
            downed: true,
            ..Observed::default()
        };
        let s = Simulated {
            dps: 2500.0,
            condi_fraction: 0.5,
            skill_share: dist(&[("A", 0.5), ("Conditions", 0.5)]),
            uptime: BOONS.iter().map(|&(b, _)| (b, 0.8)).collect(),
            might_stacks: 15.0,
            cleanse_per_20s: 1.0,
            cleanse_measured: true,
            stunbreak_skills: 0,
            incoming_dps: Some(1200.0),
            downed: Some(false),
            sim_s: Some(15.0),
            ..Simulated::default()
        };
        (o, s)
    }

    #[test]
    fn diffs_use_the_documented_error_units() {
        let (o, s) = hand_pair();
        let d = diffs(&o, &s, true);
        close(diff(&d, "dps_engaged").error.unwrap(), 0.25);
        close(diff(&d, "dps_active").error.unwrap(), 1.5);
        close(diff(&d, "condi_fraction").error.unwrap(), 0.25);
        close(diff(&d, "skill_share").error.unwrap(), 0.25);
        close(diff(&d, "proc_share").error.unwrap(), -0.1);
        // Boons score against the self share (0.5), not the total (0.9).
        let q = diff(&d, "uptime:Quickness");
        close(q.log, 0.5);
        close(q.error.unwrap(), 0.3);
        assert!(q.note.contains("total 0.900"), "{}", q.note);
        // Might path: EI self stacks vs sim average stacks, in cap units.
        let might = diff(&d, "might_stacks");
        close(might.log, 20.0);
        close(might.error.unwrap(), -5.0 / 25.0);
        assert!(might.note.contains("total 25.000"), "{}", might.note);
        // 0.2 cleanses/20 s is under the 0.5 floor.
        let cleanse = diff(&d, "cleanse_per_20s");
        close(cleanse.error.unwrap(), 0.8 / 0.5);
        assert!(cleanse.note.contains("sim_s 15.0"), "{}", cleanse.note);
        let incoming = diff(&d, "incoming_dps");
        close(incoming.error.unwrap(), -0.2);
        assert!(incoming.note.contains("sim_s 15.0"), "{}", incoming.note);
        assert_eq!(d.len(), 5 + BOONS.len() + 5);
    }

    #[test]
    fn unmodelled_observables_abstain_with_a_reason() {
        let (o, s) = hand_pair();
        let wvw = diffs(&o, &s, true);
        let sb = diff(&wvw, "stunbreak");
        assert_eq!((sb.ours, sb.error), (None, None));
        close(sb.log, 3.0);
        assert!(sb.note.contains("not uses"), "{}", sb.note);
        assert!(sb.note.contains("disagrees"), "{}", sb.note);

        let pve = diffs(&o, &s, false);
        for name in ["incoming_dps", "downed"] {
            let d = diff(&pve, name);
            assert_eq!((d.ours, d.error), (None, None), "{name}");
            assert!(d.note.contains("PvE"), "{name}: {}", d.note);
        }
        let no_timeline = Simulated {
            incoming_dps: None,
            downed: None,
            ..s
        };
        let d = diffs(&o, &no_timeline, true);
        assert!(diff(&d, "downed").note.contains("no WvW timeline"));

        // WvW downed abstains even with a timeline: 15 s window vs 60 s fight.
        let down = diff(&wvw, "downed");
        assert_eq!((down.ours, down.error), (None, None));
        assert!(
            down.note.contains("gate window 15 s vs fight 60 s"),
            "{}",
            down.note
        );

        // A boon with no source split abstains and still shows the total.
        let mut unsplit = o.clone();
        unsplit.uptime_self.insert("Fury", None);
        unsplit.might_self = None;
        let d = diffs(&unsplit, &no_timeline, true);
        for name in ["uptime:Fury", "might_stacks"] {
            let x = diff(&d, name);
            assert_eq!((x.ours, x.error), (None, None), "{name}");
            assert!(x.note.contains("no source split"), "{name}: {}", x.note);
        }
    }

    #[test]
    fn a_value_that_rounds_to_zero_prints_unsigned() {
        assert_eq!(opt(Some(-0.0)), "0.000");
        assert_eq!(opt(Some(-0.0004)), "0.000");
        assert_eq!(opt(Some(-0.0006)), "-0.001");
        assert_eq!(opt(None), "-");
        // A band's bias goes through the same formatter.
        let rows = [row("Ranger", "WvW", &[("proc_share", Some(-0.0))])];
        let out = render_bands(&bands(&rows));
        assert!(out.contains("| 0.000 | 0.000 | 0.000 | 0 |"), "{out}");
    }

    fn row(
        profession: &str,
        mode: &str,
        errors: &[(&'static str, Option<f64>)],
    ) -> PlayerComparison {
        PlayerComparison {
            log: "l".into(),
            player: "p".into(),
            profession: profession.into(),
            spec: profession.into(),
            mode: mode.into(),
            kit: None,
            diffs: errors
                .iter()
                .map(|&(observable, error)| Diff {
                    observable,
                    log: 0.0,
                    ours: error,
                    error,
                    note: String::new(),
                })
                .collect(),
            share: Vec::new(),
            unmapped_share: 0.0,
            gates: Vec::new(),
            refused: None,
        }
    }

    #[test]
    fn bands_pin_median_p90_and_bias_on_five_rows() {
        let mut rows: Vec<PlayerComparison> = [0.1, -0.3, 0.2, -0.5, 0.4]
            .iter()
            .map(|&e| {
                row(
                    "Necromancer",
                    "WvW",
                    &[
                        ("condi_fraction", Some(e)),
                        ("dps_active", Some(e)),
                        ("stunbreak", None),
                    ],
                )
            })
            .collect();
        let mut refused = row("Necromancer", "WvW", &[("condi_fraction", Some(9.0))]);
        refused.refused = Some("validator: x".into());
        rows.push(refused);
        // A failed blocking gate is recorded, never excluded.
        rows[0].gates = vec!["StunbreakCount".into()];
        rows.push(row("Guardian", "PvE", &[("dps_active", Some(0.5))]));

        // A second spec of the same profession bands on its own.
        let mut reaper = row("Necromancer", "WvW", &[("condi_fraction", Some(0.9))]);
        reaper.spec = "Reaper".into();
        rows.push(reaper);

        let b = bands(&rows);
        let key =
            |p: &str, m: &str, o: &'static str| (p.to_string(), p.to_string(), m.to_string(), o);
        let c = &b[&key("Necromancer", "WvW", "condi_fraction")];
        assert_eq!(c.n, 5); // the refused row and the Reaper are excluded
        close(c.median_abs, 0.3); // |e| sorted 0.1 0.2 0.3 0.4 0.5
        close(c.p90_abs, 0.5); // ceil(4.5) = 5th
        close(c.bias, -0.1 / 5.0);
        let r = &b[&(
            "Necromancer".to_string(),
            "Reaper".to_string(),
            "WvW".to_string(),
            "condi_fraction",
        )];
        assert_eq!(r.n, 1);
        assert!(!b.contains_key(&key("Necromancer", "WvW", "dps_active")));
        assert_eq!(b[&key("Guardian", "PvE", "dps_active")].n, 1);
        let s = &b[&key("Necromancer", "WvW", "stunbreak")];
        assert_eq!((s.n, s.abstained), (0, 5));

        let out = render_bands(&b);
        assert!(
            out.contains(
                "| Necromancer | Necromancer | WvW | condi_fraction | 5 | 0.300 | 0.500 | -0.020 | 0 |"
            ),
            "{out}"
        );
        assert!(
            out.contains("| Necromancer | Reaper | WvW | condi_fraction | 1 |"),
            "{out}"
        );
    }

    #[test]
    fn render_table_lists_refused_players_with_the_reason() {
        let mut r = row("Guardian", "PvE", &[("condi_fraction", Some(0.1))]);
        let mut gated = row("Guardian", "WvW", &[]);
        gated.player = "g".into();
        gated.gates = vec!["StunbreakCount".into(), "CleanseRate".into()];
        let mut refused = row("Guardian", "PvE", &[]);
        refused.player = "q".into();
        refused.refused = Some("kit: no published Firebrand PvE build".into());
        r.share = vec![("A".into(), 0.6, 0.5)];
        let out = render_table(&[r, gated, refused]);
        assert!(
            out.contains("| p | Guardian | PvE | - | - | 0.000 | all pass | compared |"),
            "{out}"
        );
        assert!(
            out.contains(
                "| g | Guardian | WvW | - | - | 0.000 | StunbreakCount, CleanseRate | compared |"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "| q | Guardian | PvE | - | - | 0.000 | - | kit: no published Firebrand PvE build |"
            ),
            "{out}"
        );
        assert!(
            out.contains("| condi_fraction | 0.000 | 0.100 | 0.100 |  |"),
            "{out}"
        );
        assert!(out.contains("| A | 0.600 | 0.500 |"), "{out}");
    }
}
