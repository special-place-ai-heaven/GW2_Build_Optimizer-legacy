pub mod article;
pub mod balance;
pub mod benchmark;
pub mod build_template;
pub mod combat;
pub mod consumables;
pub mod data;
pub mod engine;
pub mod fidelity;
pub mod gamedb;
pub mod gemini;
pub mod gemini_tools;
#[cfg(test)]
mod grouped_sheet;
pub mod infusions;
pub mod itemstat_pool;
pub mod llm;
#[cfg(test)]
pub mod parser_consistency_tests;
pub mod picks;
pub mod prompts;
pub mod providers;
pub mod referee;
pub mod rotation;
pub mod scenario;
pub mod scoring;
pub mod scraper;
pub mod search;
pub mod search_v2;
pub mod sigil_slots;
pub mod stats;
pub mod synergy;
pub mod synergy_pipeline;
pub mod text_util;
pub mod upgrade_graph;
pub mod validation;
pub mod weapon_budget;

// Re-export viability types for downstream consumers (S07 Trust UI).
pub use referee::{GateResult, ViabilityGate, ViabilityReport};
// Re-export scenario types for addon UI (S03).
pub use scenario::{CombatKind, CombatTier, RoleObjective, ScenarioSpec};
