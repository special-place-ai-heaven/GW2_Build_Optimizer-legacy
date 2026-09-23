//! Serde model of an Elite Insights JSON log, restricted to the fields the
//! fidelity comparator reads. Unknown keys are ignored and missing keys take
//! their default, so any EI version parses. `Serialize` is deliberate:
//! parse-then-serialize is the fixture trimmer, and the trimmed file is a fixed
//! point of the round trip.
//!
//! Key names follow EI's camelCase except where the C# name is not plain
//! PascalCase (`triggerID`, `durationMS`, `damage1S`); those carry an
//! explicit rename.

use std::collections::BTreeMap;
use std::path::Path;

use gw2_core::types::GameMode;
use serde::{Deserialize, Serialize};

use crate::scenario::CombatTier;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiLog {
    /// Encounter trigger. 1 = WvW (arcdps EVTC README), 16199 = Kitty golem.
    #[serde(rename = "triggerID")]
    pub trigger_id: u32,
    #[serde(rename = "durationMS")]
    pub duration_ms: u64,
    /// Not an EI key: the squad size before the fixture trimmer dropped
    /// players, so [`EiLog::tier`] keeps the scale the fight was played at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trimmed_squad_size: Option<u32>,
    pub players: Vec<EiPlayer>,
    /// `"s<skill id>"` -> descriptor.
    pub skill_map: BTreeMap<String, EiSkillInfo>,
    /// `"b<buff id>"` -> descriptor.
    pub buff_map: BTreeMap<String, EiBuffInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiPlayer {
    pub name: String,
    /// Elite spec name, or the core profession name.
    pub profession: String,
    pub group: u32,
    pub not_in_squad: bool,
    pub is_fake: bool,
    /// 8 slots: 0-1 land set 1, 2-3 land set 2, 4-7 aquatic. `"Unknown"` when
    /// not seen, `"2Hand"` in the off-hand slot of a two-hander.
    pub weapons: Vec<String>,
    /// Squad-relative 0-10 ranks. 0 for every player in 2026 logs.
    pub toughness: u32,
    pub healing: u32,
    pub concentration: u32,
    pub condition: u32,
    /// Milliseconds per phase, `[0]` = whole fight.
    pub active_times: Vec<u64>,
    pub dps_all: Vec<EiDps>,
    /// `[phase][]`, all targets.
    pub total_damage_dist: Vec<Vec<EiDamageDist>>,
    pub rotation: Vec<EiRotation>,
    pub buff_uptimes: Vec<EiBuffUptime>,
    pub defenses: Vec<EiDefenses>,
    pub support: Vec<EiSupport>,
    /// `[phase][second]`, cumulative damage to all targets.
    #[serde(rename = "damage1S")]
    pub damage1_s: Vec<Vec<i64>>,
    /// `[phase][second]`, cumulative condition damage to all targets. Empty
    /// in fixtures trimmed before it was read.
    #[serde(rename = "conditionDamage1S", skip_serializing_if = "Vec::is_empty")]
    pub condition_damage1_s: Vec<Vec<i64>>,
    /// `[phase][]`, incoming damage by skill.
    pub total_damage_taken: Vec<Vec<EiDamageDist>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiSkillInfo {
    pub name: String,
    pub auto_attack: bool,
    pub is_trait_proc: bool,
    pub is_gear_proc: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiBuffInfo {
    pub name: String,
    pub stacking: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiDps {
    pub damage: i64,
    pub power_damage: i64,
    pub condi_damage: i64,
    /// The player alone, minions excluded. `None` in fixtures trimmed before
    /// it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_damage: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_condi_damage: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiDamageDist {
    /// Skill id, or a buff id when `indirect_damage` (condition tick). EI also
    /// emits negative ids for synthetic events.
    pub id: i64,
    pub total_damage: i64,
    pub hits: u32,
    pub connected_hits: u32,
    pub missed: u32,
    pub evaded: u32,
    pub blocked: u32,
    pub invulned: u32,
    pub indirect_damage: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiRotation {
    /// Skill id; negative for EI synthetic casts (-2 = weapon swap).
    pub id: i64,
    pub skills: Vec<EiCast>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiCast {
    /// Milliseconds from log start; negative = cast before the fight.
    pub cast_time: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiBuffUptime {
    pub id: i64,
    pub buff_data: Vec<EiBuffData>,
    /// Step function `[time ms from log start, stacks]`, whole fight; the
    /// last entry at or before `t` holds at `t`. Empty in fixtures trimmed
    /// before it was read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiBuffData {
    /// Percent for duration buffs, average stacks for intensity buffs.
    pub uptime: f64,
    pub presence: f64,
    /// Source player name -> the part of `uptime` that source generated,
    /// same unit as `uptime`. The fixture trimmer keeps only the wearer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated: Option<BTreeMap<String, f64>>,
    /// The same split of `presence`, percent; intensity buffs only (0 for
    /// duration buffs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_presence: Option<BTreeMap<String, f64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiDefenses {
    pub damage_taken: i64,
    pub down_count: u32,
    pub received_crowd_control: u32,
    pub received_crowd_control_duration: f64,
    /// Boons stripped from this player (received, not given).
    pub boon_strips: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EiSupport {
    pub condi_cleanse: u32,
    pub condi_cleanse_self: u32,
    pub stun_break: u32,
}

pub fn parse(json: &str) -> Result<EiLog, serde_json::Error> {
    serde_json::from_str(json)
}

pub fn load(path: &Path) -> Result<EiLog, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

impl EiLog {
    pub fn mode(&self) -> GameMode {
        if self.trigger_id == 1 {
            GameMode::WvW
        } else {
            GameMode::PvE
        }
    }

    /// Squad members with data: not a non-squad ally, not an EI fake actor.
    pub fn squad(&self) -> impl Iterator<Item = &EiPlayer> {
        self.players
            .iter()
            .filter(|p| !p.not_in_squad && !p.is_fake)
    }

    /// Scale from measured squad size: 0-1 Solo, 2-5 Party, more Squad. A
    /// trimmed fixture uses the size it was trimmed from.
    pub fn tier(&self) -> CombatTier {
        let size = self
            .trimmed_squad_size
            .map_or_else(|| self.squad().count(), |n| n as usize);
        match size {
            0..=1 => CombatTier::Solo,
            2..=5 => CombatTier::Party,
            _ => CombatTier::Squad,
        }
    }

    pub fn buff_name(&self, id: u32) -> Option<&str> {
        self.buff_map
            .get(&format!("b{id}"))
            .map(|b| b.name.as_str())
    }

    pub fn skill_info(&self, id: u32) -> Option<&EiSkillInfo> {
        self.skill_map.get(&format!("s{id}"))
    }
}

impl EiPlayer {
    /// Casts per skill id. EI synthetic ids (negative) are skipped.
    pub fn cast_counts(&self) -> BTreeMap<u32, u32> {
        let mut out = BTreeMap::new();
        for r in &self.rotation {
            if let Ok(id) = u32::try_from(r.id) {
                *out.entry(id).or_insert(0) += r.skills.len() as u32;
            }
        }
        out
    }

    /// First `n` cast ids by cast time (ties by id). Synthetic ids skipped.
    pub fn opener(&self, n: usize) -> Vec<u32> {
        let mut casts: Vec<(i64, u32)> = self
            .rotation
            .iter()
            .filter_map(|r| u32::try_from(r.id).ok().map(|id| (r, id)))
            .flat_map(|(r, id)| r.skills.iter().map(move |c| (c.cast_time, id)))
            .collect();
        casts.sort_unstable();
        casts.into_iter().take(n).map(|(_, id)| id).collect()
    }

    /// Seconds of the whole fight in which this player's cumulative damage to
    /// all targets rose. `damage1S[0][i]` is the total at `i` s, so sample 0
    /// is the instant t = 0, not a second: interval `i >= 1` counts when
    /// sample `i` exceeds sample `i-1`, and the first interval compares
    /// against 0 so damage landed at t = 0 (a pre-cast) falls into it.
    pub fn engaged_seconds(&self) -> u32 {
        let Some(series) = self.damage1_s.first() else {
            return 0;
        };
        let mut prev = 0;
        let mut n = 0;
        for &d in series.iter().skip(1) {
            if d > prev {
                n += 1;
            }
            prev = d;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLEM: &str =
        include_str!("../../tests/fixtures/ei_logs/1f33-20260720-163045_golem.json");
    const LRBJ: &str = include_str!("../../tests/fixtures/ei_logs/lRBj-20260604-210631_wvw.json");
    const ABTD: &str = include_str!("../../tests/fixtures/ei_logs/aBtd-20260604-211449_wvw.json");

    fn player<'a>(log: &'a EiLog, name: &str) -> &'a EiPlayer {
        log.players
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("fixture has player {name}"))
    }

    #[test]
    fn fixtures_are_fixed_points_of_parse_then_serialize() {
        for (name, text) in [("golem", GOLEM), ("lRBj", LRBJ), ("aBtd", ABTD)] {
            let log = parse(text).unwrap_or_else(|e| panic!("{name} parses: {e}"));
            let out = serde_json::to_string(&log).expect("serializes");
            assert_eq!(out, text.trim_end(), "{name} is not a fixed point");
        }
    }

    // Numbers below were derived with Python from the UNTRIMMED dps.report
    // downloads: triggerID and durationMS read directly; engaged = count of
    // damage1S[0][i], i >= 1, greater than the previous one (0 standing in
    // for sample 0); tier from the untrimmed squad size;
    // opener = rotation casts with id >= 0 sorted by (castTime, id), first 12;
    // cast count = len(skills) of the rotation entry with that id.

    #[test]
    fn golem_log_is_solo_pve_reaper() {
        let log = parse(GOLEM).expect("parses");
        assert_eq!(log.mode(), GameMode::PvE); // triggerID 16199
        assert_eq!(log.duration_ms, 94_808);
        assert_eq!(log.squad().count(), 1);
        assert_eq!(log.trimmed_squad_size, Some(1));
        assert_eq!(log.tier(), CombatTier::Solo);
        let p = player(&log, "Aisxka");
        assert_eq!(p.profession, "Reaper");
        // 96 samples = 95 intervals, damage rises in every one of them.
        assert_eq!(p.engaged_seconds(), 95);
        // (-399, 29855) precast Nightfall first; the tie at 82 ms orders by id.
        assert_eq!(
            p.opener(12),
            [29855, 69855, 10607, 29604, 10546, 73107, 73068, 29414, 73116, 30792, 30825, 29604]
        );
        assert_eq!(p.cast_counts().get(&29855), Some(&5)); // Nightfall
        assert_eq!(
            log.skill_info(29855).map(|s| s.name.as_str()),
            Some("Nightfall")
        );
        assert_eq!(log.buff_name(737), Some("Burning"));
    }

    #[test]
    fn lrbj_log_is_one_party_of_a_wvw_squad() {
        let log = parse(LRBJ).expect("parses");
        assert_eq!(log.mode(), GameMode::WvW); // triggerID 1
        assert_eq!(log.duration_ms, 77_209);
        // Trimmed to squad group 2 (5 players); the full squad is 20.
        assert_eq!(log.squad().count(), 5);
        assert_eq!(log.trimmed_squad_size, Some(20));
        assert_eq!(log.tier(), CombatTier::Squad);
        let p = player(&log, "Aster Menimem");
        assert_eq!(p.profession, "Firebrand");
        // 78 intervals, damage rose in 19 of them.
        assert_eq!(p.engaged_seconds(), 19);
        // Weapon swap (-2) is synthetic and skipped.
        assert_eq!(
            p.opener(12),
            [9109, 9101, 9110, 9091, 9109, 9110, 9108, 23275, 9087, 42259, 40988, 44455]
        );
        assert_eq!(p.cast_counts().get(&9109), Some(&3)); // True Strike
        assert_eq!(p.cast_counts().len(), 21); // distinct non-negative ids
    }

    #[test]
    fn abtd_log_is_one_party_of_a_wvw_squad() {
        let log = parse(ABTD).expect("parses");
        assert_eq!(log.mode(), GameMode::WvW);
        assert_eq!(log.duration_ms, 83_739);
        // Trimmed to squad group 1 (5 players); the full squad is 30.
        assert_eq!(log.squad().count(), 5);
        assert_eq!(log.trimmed_squad_size, Some(30));
        assert_eq!(log.tier(), CombatTier::Squad);
        let p = player(&log, "Piero Rosso");
        assert_eq!(p.profession, "Reaper");
        // 84 intervals, damage rose in 47 of them.
        assert_eq!(p.engaged_seconds(), 47);
        assert_eq!(
            p.opener(12),
            [62517, 62517, 62517, 23275, 62517, 62517, 29414, 29604, 30670, 62517, 62517, 30105]
        );
        assert_eq!(p.cast_counts().get(&62517), Some(&32)); // Vicious Shot
    }

    #[test]
    fn missing_keys_default_and_empty_series_is_not_engaged() {
        let log = parse(r#"{"players":[{"name":"x","isFake":true}]}"#).expect("parses");
        assert_eq!(log.squad().count(), 0);
        assert_eq!(log.tier(), CombatTier::Solo);
        assert_eq!(log.mode(), GameMode::PvE);
        assert_eq!(log.players[0].engaged_seconds(), 0);
        assert!(log.players[0].opener(12).is_empty());
        // Without the trim field the tier counts the players in the file.
        let two = parse(r#"{"players":[{"name":"a"},{"name":"b"}]}"#).expect("parses");
        assert_eq!(two.tier(), CombatTier::Party);
    }

    #[test]
    fn engaged_seconds_skips_the_t0_sample() {
        // A pre-cast lands 500 at t = 0; nothing rises after 2 s.
        let mut p = EiPlayer {
            damage1_s: vec![vec![500, 500, 900, 900]],
            ..Default::default()
        };
        // Interval 1 holds the pre-cast (500 > 0), interval 2 rises, 3 does not.
        assert_eq!(p.engaged_seconds(), 2);
        p.damage1_s = vec![vec![0]];
        assert_eq!(p.engaged_seconds(), 0);
    }
}
