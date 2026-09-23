//! Form pools other than life force (`data/formulas/forms.json`): the
//! resource a profession form (Druid Celestial Avatar) fills and burns.
//! Entry threshold and duration are the entry skill's own API `cost` and
//! `Duration` fact; this table holds what the API does not publish.
//! Shroud forms read `data/formulas/shroud.json`.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const FORMS_JSON: &str = include_str!("../../../../data/formulas/forms.json");

static TABLE: OnceLock<FormTable> = OnceLock::new();

/// One form's pool, keyed by its entry skill id.
#[derive(Debug, Clone, Deserialize)]
pub struct FormRow {
    pub name: String,
    /// The pool's name as the wiki gives it (`astral force`).
    pub resource: String,
    pub pool: f64,
    /// Percent of the pool per landed strike.
    pub gain_pct_per_strike: f64,
    /// Percent of the pool per heal on a damaged target.
    pub gain_pct_per_heal: f64,
    pub gains_in_form: bool,
    /// The pool keeps its fill out of combat, so a fight can open full.
    #[serde(default)]
    pub persists_out_of_combat: bool,
    /// Percent of the remaining pool kept on a voluntary exit.
    pub early_exit_keep_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FormTable {
    pub source: String,
    pub forms: HashMap<u32, FormRow>,
}

/// The embedded table, parsed once.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data).
pub fn table() -> &'static FormTable {
    TABLE.get_or_init(|| serde_json::from_str(FORMS_JSON).expect("embedded forms.json is invalid"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn celestial_avatar_row_carries_the_wiki_astral_force_numbers() {
        let t = super::table();
        let avatar = &t.forms[&31869];
        assert_eq!(avatar.name, "Celestial Avatar");
        assert_eq!(avatar.gain_pct_per_strike, 0.75);
        assert_eq!(avatar.gain_pct_per_heal, 1.5);
        assert!(!avatar.gains_in_form);
        assert!(avatar.persists_out_of_combat);
        assert_eq!(avatar.early_exit_keep_pct, 50.0);
        assert!(t.source.contains("Astral_force"));
    }
}
