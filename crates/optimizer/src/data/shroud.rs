//! Shroud table: life force pool, entry floor and per-shroud drain and
//! damage reduction, one shared shape for every Necromancer specialisation
//! (`specs/005-wvw-proc-sites`, R6). Loaded from `data/formulas/shroud.json`
//! with the `include_str!` + `OnceLock` pattern of the other formula files.
//!
//! A `null` drain or reduction means the wiki number was not read yet: the
//! caller must treat the resource model as incomplete, never invent a value.

use gw2_core::types::GameMode;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const SHROUD_JSON: &str = include_str!("../../../../data/formulas/shroud.json");

static TABLE: OnceLock<ShroudTable> = OnceLock::new();

/// Per-mode percentages.
#[derive(Debug, Clone, Deserialize)]
pub struct PerMode {
    pub pve: f64,
    pub wvw: f64,
    pub pvp: f64,
}

impl PerMode {
    pub fn for_mode(&self, mode: GameMode) -> f64 {
        match mode {
            GameMode::PvE => self.pve,
            GameMode::WvW => self.wvw,
            GameMode::PvP => self.pvp,
        }
    }
}

/// One shroud entry skill's numbers. `None` is unresolved.
#[derive(Debug, Clone, Deserialize)]
pub struct ShroudRow {
    pub name: String,
    pub drain_pct_per_s: Option<PerMode>,
    pub damage_reduction_pct: Option<PerMode>,
    /// Whether the life force pool stands in for health while in this
    /// shroud (Death, Reaper's, Ritualist's). Harbinger Shroud leaves the
    /// health pool exposed and lets healing through (wiki `Harbinger
    /// Shroud`, Mechanics).
    #[serde(default = "default_true")]
    pub protects_health: bool,
    /// The elite specialisation whose shroud this is; `None` is core Death
    /// Shroud. The API tags every shroud entry skill spec-less.
    #[serde(default)]
    pub specialization: Option<u32>,
    /// What this shroud does that the simulators do not play, named for the
    /// gap line (doctrine 6).
    #[serde(default)]
    pub unmodelled: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShroudTable {
    pub source: String,
    pub life_force_pool_pct_of_health: f64,
    /// The pool keeps its fill out of combat (wiki `Life force`: stored
    /// permanently), so a fight can open with it full.
    #[serde(default)]
    pub pool_persists_out_of_combat: bool,
    pub entry_floor_pct: f64,
    pub recharge_on_exit_s: f64,
    /// Keyed by the entry skill id.
    pub shrouds: HashMap<u32, ShroudRow>,
}

impl ShroudTable {
    /// The row for a shroud entry skill, if the table knows it.
    pub fn row(&self, entry_skill_id: u32) -> Option<&ShroudRow> {
        self.shrouds.get(&entry_skill_id)
    }

    /// The shroud an equipped elite wears, if the table names one.
    pub fn row_for_elite(&self, equipped_spec_ids: &[u32]) -> Option<(u32, &ShroudRow)> {
        self.shrouds
            .iter()
            .find(|(_, row)| {
                row.specialization
                    .is_some_and(|spec| equipped_spec_ids.contains(&spec))
            })
            .map(|(id, row)| (*id, row))
    }

    /// The row whose name matches (fixtures and renamed ids).
    pub fn row_by_name(&self, name: &str) -> Option<&ShroudRow> {
        self.shrouds.values().find(|row| row.name == name)
    }

    /// Life force capacity for a health pool (wiki `Life force`: 69 %).
    pub fn pool_for(&self, max_health: f64) -> f64 {
        max_health * self.life_force_pool_pct_of_health / 100.0
    }
}

/// The embedded table, parsed once.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data).
pub fn table() -> &'static ShroudTable {
    TABLE
        .get_or_init(|| serde_json::from_str(SHROUD_JSON).expect("embedded shroud.json is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_shrouds_load_and_unread_rows_are_none() {
        let t = table();
        assert_eq!(t.shrouds.len(), 4);
        let reaper = t.row(30792).expect("Reaper's Shroud");
        assert_eq!(
            reaper
                .drain_pct_per_s
                .as_ref()
                .unwrap()
                .for_mode(GameMode::WvW),
            5.0
        );
        assert_eq!(
            reaper
                .damage_reduction_pct
                .as_ref()
                .unwrap()
                .for_mode(GameMode::PvE),
            33.0
        );
        let death = t.row(10574).expect("Death Shroud");
        assert_eq!(
            death
                .drain_pct_per_s
                .as_ref()
                .unwrap()
                .for_mode(GameMode::WvW),
            3.0
        );
        // Harbinger: 5 %/s, no reduction, health exposed (wiki, read
        // 2026-09-08). Ritualist's: 3/5/5 with a verification request,
        // 33/50/50 reduction from the API facts.
        let harbinger = t.row(62567).expect("Harbinger Shroud");
        assert!(!harbinger.protects_health);
        assert!(harbinger.unmodelled.iter().any(|g| g == "blight"));
        assert!(harbinger
            .unmodelled
            .iter()
            .any(|g| g.contains("Corrupted Talent")));
        assert_eq!(
            harbinger
                .drain_pct_per_s
                .as_ref()
                .unwrap()
                .for_mode(GameMode::WvW),
            5.0
        );
        assert_eq!(
            harbinger
                .damage_reduction_pct
                .as_ref()
                .unwrap()
                .for_mode(GameMode::WvW),
            0.0
        );
        let ritualist = t.row(77238).expect("Ritualist's Shroud");
        assert!(ritualist.protects_health && reaper.protects_health && death.protects_health);
        assert_eq!(
            ritualist
                .drain_pct_per_s
                .as_ref()
                .unwrap()
                .for_mode(GameMode::PvE),
            3.0
        );
        assert_eq!(
            ritualist
                .damage_reduction_pct
                .as_ref()
                .unwrap()
                .for_mode(GameMode::WvW),
            50.0
        );
        assert!(t.row(1).is_none());
        assert_eq!(t.row_for_elite(&[53, 2, 34]).unwrap().0, 30792);
        assert!(t.row_for_elite(&[53, 2, 19]).is_none());
        assert_eq!(t.row_for_elite(&[64]).unwrap().1.name, "Harbinger Shroud");
        assert_eq!(t.entry_floor_pct, 10.0);
        assert!((t.pool_for(20_000.0) - 13_800.0).abs() < 1e-9);
        assert!(t.source.contains("(read 20"));
        assert!(t.pool_persists_out_of_combat);
    }
}
