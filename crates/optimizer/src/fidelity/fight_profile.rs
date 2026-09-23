//! Fight profile measured from an Elite Insights log: how available the
//! target was, how often hits landed, and what the squad took.
//!
//! Nothing outside this module reads a `FightProfile` in this increment; it is
//! extracted and printed only. Its fields are the measured values meant to
//! replace these baked-in constants later:
//! - `target_availability`: every hit lands on an always-present target
//!   (simulator + WvW timeline), and the sim windows
//!   `combat_model::simulation_window_ms_for_mode` (rotation/combat_model.rs:16)
//!   and `engine::FLOW_WINDOW_MS` (engine.rs:1193).
//! - `hit_rate` / `avoided_rate`: no miss, blind, evade or block anywhere in
//!   the simulator.
//! - `incoming_dps`, `incoming_cc_*`, `strips_received_per_min`, `downs_per_player_min`:
//!   the scripted enemy pressure of `WvwProfile::for_scenario`
//!   (rotation/wvw_timeline.rs:449): 10 s burst cycle, CC 900 + 1100 ms, one
//!   boon strip per cycle.
//! - `tier`: `FightPopulation::for_tier` (data/fight_population.rs:31).
//!
//! Rates are means of per-player rates over the squad, each over that
//! player's own active time, so a player downed early is not diluted by the
//! full log length.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::ei_log::{EiLog, EiSkillInfo};
use crate::gamedb::GameDb;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FightProfile {
    pub mode: String,
    pub tier: String,
    pub duration_s: f64,
    /// Mean over squad of engaged seconds / active seconds, capped at 1.
    pub target_availability: f64,
    /// Skill class -> connectedHits / hits, strike rows only.
    pub hit_rate: BTreeMap<String, f64>,
    /// Skill class -> (evaded + blocked + missed + invulned) / hits.
    pub avoided_rate: BTreeMap<String, f64>,
    pub incoming_dps: f64,
    pub incoming_cc_per_min: f64,
    pub incoming_cc_ms_per_min: f64,
    pub downs_per_player_min: f64,
    pub strips_received_per_min: f64,
}

pub fn extract(log: &EiLog, db: &GameDb) -> FightProfile {
    extract_with(log, |id| {
        db.skills.get(&id).map(|s| s.slot.as_deref().unwrap_or(""))
    })
}

/// `slot_of(id)`: `None` when the db lacks the skill, else its API slot
/// (`""` when the skill has none).
fn extract_with<'a>(log: &EiLog, slot_of: impl Fn(u32) -> Option<&'a str>) -> FightProfile {
    let mut per_player: Vec<[f64; 6]> = Vec::new();
    // class -> (hits, connected, avoided)
    let mut strikes: BTreeMap<&'static str, (u64, u64, u64)> = BTreeMap::new();
    for p in log.squad() {
        let active_s = p.active_times.first().copied().unwrap_or(0) as f64 / 1000.0;
        if active_s <= 0.0 {
            continue;
        }
        let min = active_s / 60.0;
        let d = p.defenses.first().cloned().unwrap_or_default();
        per_player.push([
            // ponytail: the last damage1S interval is partial (the golem's
            // 95 intervals cover 94.808 s), so a player engaged to the end
            // can exceed active_s by under a second; capped, not modelled.
            (p.engaged_seconds() as f64 / active_s).min(1.0),
            d.damage_taken as f64 / active_s,
            d.received_crowd_control as f64 / min,
            d.received_crowd_control_duration / min,
            d.down_count as f64 / min,
            d.boon_strips as f64 / min,
        ]);
        for r in p.total_damage_dist.first().into_iter().flatten() {
            if r.indirect_damage || r.hits == 0 {
                continue;
            }
            let class = match u32::try_from(r.id) {
                Ok(id) => classify(slot_of(id), log.skill_info(id)),
                Err(_) => "unknown",
            };
            let e = strikes.entry(class).or_default();
            e.0 += u64::from(r.hits);
            e.1 += u64::from(r.connected_hits);
            e.2 += u64::from(r.evaded + r.blocked + r.missed + r.invulned);
        }
    }
    let mean = |i: usize| {
        if per_player.is_empty() {
            0.0
        } else {
            per_player.iter().map(|r| r[i]).sum::<f64>() / per_player.len() as f64
        }
    };
    FightProfile {
        mode: format!("{:?}", log.mode()),
        tier: format!("{:?}", log.tier()),
        duration_s: log.duration_ms as f64 / 1000.0,
        target_availability: mean(0),
        hit_rate: strikes
            .iter()
            .map(|(c, &(h, k, _))| (c.to_string(), k as f64 / h as f64))
            .collect(),
        avoided_rate: strikes
            .iter()
            .map(|(c, &(h, _, a))| (c.to_string(), a as f64 / h as f64))
            .collect(),
        incoming_dps: mean(1),
        incoming_cc_per_min: mean(2),
        incoming_cc_ms_per_min: mean(3),
        downs_per_player_min: mean(4),
        strips_received_per_min: mean(5),
    }
}

/// Proc flags win, then EI's auto-attack flag, then the API slot family
/// (`Weapon_2` -> "weapon", `Profession_1` -> "profession", `Heal`, `Utility`,
/// `Elite`, `Downed_*`, `Transform_*`, ...). A skill without a slot is
/// "other"; one the db lacks is "unknown".
fn classify(slot: Option<&str>, info: Option<&EiSkillInfo>) -> &'static str {
    if info.is_some_and(|i| i.is_trait_proc || i.is_gear_proc) {
        return "proc";
    }
    if info.is_some_and(|i| i.auto_attack) {
        return "auto";
    }
    let Some(slot) = slot else {
        return "unknown";
    };
    match slot.split('_').next().unwrap_or("") {
        "Weapon" => "weapon",
        "Heal" => "heal",
        "Utility" => "utility",
        "Elite" => "elite",
        "Profession" => "profession",
        "Downed" => "downed",
        "Transform" => "transform",
        "Pet" => "pet",
        "Toolbelt" => "toolbelt",
        _ => "other",
    }
}

pub fn render(p: &FightProfile) -> String {
    let rates = |m: &BTreeMap<String, f64>| {
        m.iter()
            .map(|(c, v)| format!("{c} {:.0}%", v * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "fight profile: mode {} tier {} duration_s {:.1} target_availability {:.2}",
        p.mode, p.tier, p.duration_s, p.target_availability
    );
    let _ = writeln!(s, "  hit_rate: {}", rates(&p.hit_rate));
    let _ = writeln!(s, "  avoided_rate: {}", rates(&p.avoided_rate));
    let _ = writeln!(
        s,
        "  incoming_dps {:.0} incoming_cc_per_min {:.2} incoming_cc_ms_per_min {:.0} downs_per_player_min {:.2} strips_received_per_min {:.2}",
        p.incoming_dps,
        p.incoming_cc_per_min,
        p.incoming_cc_ms_per_min,
        p.downs_per_player_min,
        p.strips_received_per_min
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fidelity::ei_log::parse;

    const GOLEM: &str =
        include_str!("../../tests/fixtures/ei_logs/1f33-20260720-163045_golem.json");
    const LRBJ: &str = include_str!("../../tests/fixtures/ei_logs/lRBj-20260604-210631_wvw.json");
    const ABTD: &str = include_str!("../../tests/fixtures/ei_logs/aBtd-20260604-211449_wvw.json");

    fn assert_invariants(name: &str, p: &FightProfile) {
        assert!(
            p.target_availability > 0.0 && p.target_availability <= 1.0,
            "{name}: availability {}",
            p.target_availability
        );
        assert!(!p.hit_rate.is_empty(), "{name}: no strike classes");
        for (c, hit) in &p.hit_rate {
            let avoided = p.avoided_rate[c];
            // EI counts a glance as connected, so the two partition hits.
            assert!(hit + avoided <= 1.0 + 1e-9, "{name} {c}: {hit} + {avoided}");
        }
    }

    #[test]
    fn classify_orders_proc_then_auto_then_slot() {
        let proc = EiSkillInfo {
            is_trait_proc: true,
            auto_attack: true,
            ..Default::default()
        };
        let auto = EiSkillInfo {
            auto_attack: true,
            ..Default::default()
        };
        assert_eq!(classify(Some("Weapon_1"), Some(&proc)), "proc");
        assert_eq!(classify(Some("Weapon_1"), Some(&auto)), "auto");
        assert_eq!(classify(Some("Weapon_3"), None), "weapon");
        assert_eq!(classify(Some("Profession_2"), None), "profession");
        assert_eq!(classify(Some("Elite"), None), "elite");
        assert_eq!(classify(Some(""), None), "other");
        assert_eq!(classify(None, None), "unknown");
    }

    // Hand numbers: a Python pass (json.load) over the aBtd fixture. For each of
    // the 5 squad players, active_s = activeTimes[0] / 1000 and the rate is
    // defenses[0].<field> / active_s (per second) or / (active_s / 60) (per
    // minute); the profile is the plain mean of the 5. damageTaken
    // 143107/59.522, 90979/65.956, 35429/83.704, 238288/83.704,
    // 112990/54.023 -> 1829.047. receivedCrowdControl 7, 2, 0, 18, 4 ->
    // 5.2442 per min. Strike rows (indirectDamage false) with no db:
    // auto 164/205 connected, proc 42/49, unknown 264/359.
    #[test]
    fn abtd_profile_matches_hand_numbers() {
        let log = parse(ABTD).expect("parses");
        let p = extract_with(&log, |_| None);
        assert_invariants("aBtd", &p);
        assert_eq!(p.mode, "WvW");
        assert_eq!(p.tier, "Squad"); // untrimmed squad of 30
        assert!((p.duration_s - 83.739).abs() < 1e-9);
        assert!(
            (p.incoming_dps - 1829.047).abs() < 1e-3,
            "{}",
            p.incoming_dps
        );
        assert!((p.incoming_cc_per_min - 5.2442).abs() < 1e-4);
        assert!((p.incoming_cc_ms_per_min - 6889.2035).abs() < 1e-3);
        assert!((p.downs_per_player_min - 0.8924).abs() < 1e-4);
        assert!((p.strips_received_per_min - 11.4469).abs() < 1e-4);
        assert!((p.target_availability - 0.46465).abs() < 1e-4);
        assert_eq!(
            p.hit_rate.keys().collect::<Vec<_>>(),
            ["auto", "proc", "unknown"]
        );
        assert!((p.hit_rate["auto"] - 0.8).abs() < 1e-12);
        assert!((p.avoided_rate["proc"] - 7.0 / 49.0).abs() < 1e-12);
        assert!((p.hit_rate["unknown"] - 264.0 / 359.0).abs() < 1e-12);
    }

    #[test]
    fn render_prints_every_field_once() {
        let log = parse(ABTD).expect("parses");
        let out = render(&extract_with(&log, |_| None));
        for field in [
            "mode ",
            "tier ",
            "duration_s ",
            "target_availability ",
            "hit_rate:",
            "avoided_rate:",
            "incoming_dps ",
            "incoming_cc_per_min ",
            "incoming_cc_ms_per_min ",
            "downs_per_player_min ",
            "strips_received_per_min ",
        ] {
            assert_eq!(out.matches(field).count(), 1, "{field} in\n{out}");
        }
    }

    #[test]
    fn empty_log_is_all_zero_not_nan() {
        let p = extract_with(&parse("{}").expect("parses"), |_| None);
        assert_eq!(p.incoming_dps, 0.0);
        assert!(p.hit_rate.is_empty());
    }

    #[test]
    #[ignore = "needs a synced game-data cache; see dev.cfg"]
    fn fixtures_with_real_db() {
        let cache = gw2_api::cache::DataCache::new(
            gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir"),
        );
        let db = GameDb::load(&cache).expect("game data cached \u{2014} sync it in-game first");
        for (name, text) in [("golem", GOLEM), ("lRBj", LRBJ), ("aBtd", ABTD)] {
            let p = extract(&parse(text).expect("parses"), &db);
            println!("{name}\n{}", render(&p));
            assert_invariants(name, &p);
        }
    }
}
