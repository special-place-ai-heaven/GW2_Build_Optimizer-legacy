//! Skills that break stun in game but carry neither the API `StunBreak` fact
//! nor "break(s) stun"/"stun break" wording in their description:
//! `data/stunbreak_sources.json`.
//!
//! `rotation::builder::skill_breaks_stun` reads the API fact and the
//! description text; this table is the third source for the ids where both
//! miss (an untyped "Breaks Stun" fact, or description wording the fixed
//! text needles don't match, e.g. "breaking stuns" vs "breaks stun"). Each
//! id is catalogued against the wiki, not inferred.

use serde::Deserialize;
use std::collections::HashSet;
use std::sync::OnceLock;
use thiserror::Error;

use super::try_load;

/// Canonical JSON embedded at compile time from data/stunbreak_sources.json.
const STUNBREAK_SOURCES_JSON: &str = include_str!("../../../../data/stunbreak_sources.json");

static OVERRIDES: OnceLock<HashSet<u32>> = OnceLock::new();

/// The override id set, parsed on first access.
///
/// # Panics
/// Panics if the embedded JSON is malformed (compile-time data; `cargo test`
/// catches it before a DLL is built).
pub fn overrides() -> &'static HashSet<u32> {
    OVERRIDES.get_or_init(|| {
        load_stunbreak_sources(STUNBREAK_SOURCES_JSON)
            .expect("embedded stunbreak_sources.json is invalid")
    })
}

/// Whether `skill_id` is a catalogued stun break the API/text detectors miss.
pub fn is_override(skill_id: u32) -> bool {
    overrides().contains(&skill_id)
}

/// Health-check loader: parses and validates without touching the `OnceLock`.
pub fn try_load_stunbreak_sources() -> Result<(), Vec<super::DataLoadError>> {
    try_load!(
        "stunbreak_sources",
        load_stunbreak_sources(STUNBREAK_SOURCES_JSON).map(|_| ()),
        StunbreakSourceError
    )
}

#[derive(Debug, Error)]
pub enum StunbreakSourceError {
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    ValidationError(String),
}

#[derive(Debug, Deserialize)]
struct StunbreakSource {
    id: u32,
    name: String,
}

#[derive(Debug, Deserialize)]
struct StunbreakSourcesFile {
    schema: u32,
    sources: Vec<StunbreakSource>,
}

fn load_stunbreak_sources(json: &str) -> Result<HashSet<u32>, StunbreakSourceError> {
    let file: StunbreakSourcesFile = serde_json::from_str(json)?;
    if file.schema != 1 {
        return Err(StunbreakSourceError::ValidationError(format!(
            "unsupported schema {}",
            file.schema
        )));
    }
    let mut ids = HashSet::with_capacity(file.sources.len());
    for s in &file.sources {
        if !ids.insert(s.id) {
            return Err(StunbreakSourceError::ValidationError(format!(
                "duplicate entry {} ({})",
                s.id, s.name
            )));
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_overrides_parse_and_validate() {
        assert!(try_load_stunbreak_sources().is_ok());
        assert!(overrides().contains(&77291), "Gladiator's Defense");
        assert!(overrides().contains(&5572), "Signet of Air");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let json = r#"{"schema":1,"sources":[
          {"id":1,"name":"A"},
          {"id":1,"name":"A"}]}"#;
        assert!(matches!(
            load_stunbreak_sources(json),
            Err(StunbreakSourceError::ValidationError(_))
        ));
    }
}
