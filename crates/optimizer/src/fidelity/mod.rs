//! Fidelity: validating the simulator against Elite Insights (EI) logs.
//!
//! A real fight log says what a player cast, dealt, took and kept up. This
//! module reconstructs the player's kit, runs it through the same referee the
//! addon uses (doctrine rule 8: no hand-built scenario), and reports per
//! observable how far our numbers are from the log's. The log carries no
//! build, so every reconstructed field names its provenance, and anything the
//! simulator cannot measure abstains with a reason instead of passing
//! (doctrine rule 6). Damage has opposite sub-archetypes (doctrine rule 4), so
//! errors are banded per profession and mode, never pooled.

pub mod compare;
pub mod ei_log;
pub mod fight_profile;
pub mod kit;
