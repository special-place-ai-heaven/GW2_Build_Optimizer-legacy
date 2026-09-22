//! Gate 1's instrument: how much of the game has an effect record.
//!
//! `docs/sprints/008-data-driven-simulator.md` Gate 1 is a table of counts,
//! and a gate passes when the number is met. This module computes that table
//! from the API cache plus `data/normalized_effects/`, so the example, the
//! addon and any test read the same numbers instead of each deriving their
//! own (doctrine rule 8).
//!
//! Every source of every class falls in exactly one bucket, so each row sums
//! to its population:
//!
//! - **executable**: at least one record the timeline actually runs.
//! - **abstaining**: it has a payload record, but the timeline has no state
//!   for it and says so on the coverage line (a pet's on-crit, an
//!   unemitted trigger, a positional or distance gate, an unmodelled pool).
//!   Same verdict `wvw_timeline::unexecutable_reason` reaches at load, so
//!   this column cannot drift from the simulator.
//! - **coverage**: only coverage blocks — classified, never executed.
//! - **none**: no record at all.

use std::collections::{BTreeMap, HashMap};

use crate::gamedb::GameDb;

use super::normalized_effects::{effects, SourceType};

/// Which bucket a source falls in. Ordered worst-to-best so a source with
/// several records takes the best verdict any of them earns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceCoverage {
    None,
    Coverage,
    Abstaining,
    Executable,
}

/// One source class (minor traits, sigils, elite skills, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassRow {
    pub class: &'static str,
    /// Every source of this class the game publishes.
    pub population: usize,
    pub executable: usize,
    pub abstaining: usize,
    pub coverage: usize,
    pub none: usize,
}

impl ClassRow {
    /// Sources that appear in a mode file at all — the census's column.
    pub fn with_record(&self) -> usize {
        self.executable + self.abstaining + self.coverage
    }
}

/// Trait coverage for one profession. Gate 1 works profession by profession.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfessionRow {
    pub profession: String,
    pub minor_with: usize,
    pub minor_total: usize,
    pub major_with: usize,
    pub major_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageTable {
    pub classes: Vec<ClassRow>,
    pub professions: Vec<ProfessionRow>,
    /// Tier-bonus lines across the rune population (six per rune).
    pub rune_tier_lines: usize,
}

/// Every recorded source's verdict, keyed by `(source type, source id)`.
type Verdicts = HashMap<(String, u32), SourceCoverage>;

fn verdicts() -> Verdicts {
    let mut out: Verdicts = HashMap::new();
    for mode in ["PvE", "PvP", "WvW"] {
        for effect in effects().effects_for_mode(mode) {
            let verdict = if effect.coverage.is_some() {
                SourceCoverage::Coverage
            } else if crate::rotation::wvw_timeline::unexecutable_reason(effect).is_some() {
                SourceCoverage::Abstaining
            } else {
                SourceCoverage::Executable
            };
            let key = (format!("{:?}", effect.source_type), effect.source_id);
            let slot = out.entry(key).or_insert(SourceCoverage::None);
            *slot = (*slot).max(verdict);
        }
    }
    out
}

fn tally(ids: &[u32], source: SourceType, verdicts: &Verdicts) -> (usize, usize, usize) {
    let tag = format!("{source:?}");
    let mut counts = (0, 0, 0);
    for id in ids {
        match verdicts.get(&(tag.clone(), *id)) {
            Some(SourceCoverage::Executable) => counts.0 += 1,
            Some(SourceCoverage::Abstaining) => counts.1 += 1,
            Some(SourceCoverage::Coverage) => counts.2 += 1,
            _ => {}
        }
    }
    counts
}

/// Compute the Gate 1 table from the cached game data.
///
/// Populations, as the census defined them:
/// - minor / major traits: the ids each specialization publishes
/// - runes, sigils: exotic upgrade components of that detail type
/// - relics: exotic relics
/// - profession-mechanic skills: every `Profession_*` slot, including the
///   Thief's 14 stolen skills and the Revenant's two legend entries, which
///   the API publishes with an empty `professions` list. The denominator is
///   "skills a build can actually have on its bar", and a stolen skill sits
///   in the profession slot like any other.
/// - elite skills: the `Elite` slot
pub fn coverage_table(db: &GameDb) -> CoverageTable {
    let verdicts = verdicts();

    let mut minor_ids = Vec::new();
    let mut major_ids = Vec::new();
    let mut by_profession: BTreeMap<String, (Vec<u32>, Vec<u32>)> = BTreeMap::new();
    for spec in db.specializations.values() {
        let entry = by_profession.entry(spec.profession.clone()).or_default();
        entry.0.extend(spec.minor_traits.iter().copied());
        entry.1.extend(spec.major_traits.iter().copied());
        minor_ids.extend(spec.minor_traits.iter().copied());
        major_ids.extend(spec.major_traits.iter().copied());
    }

    let exotic = |id: &u32, detail: &str| {
        db.items.get(id).is_some_and(|item| {
            item.rarity == "Exotic"
                && item
                    .details
                    .as_ref()
                    .is_some_and(|d| d.detail_type.as_deref() == Some(detail))
        })
    };
    let rune_ids: Vec<u32> = db
        .runes
        .iter()
        .copied()
        .filter(|id| exotic(id, "Rune"))
        .collect();
    let sigil_ids: Vec<u32> = db
        .sigils
        .iter()
        .copied()
        .filter(|id| exotic(id, "Sigil"))
        .collect();
    let relic_ids: Vec<u32> = db
        .relics
        .iter()
        .copied()
        .filter(|id| db.items.get(id).is_some_and(|i| i.rarity == "Exotic"))
        .collect();

    let slot_is = |id: &u32, want: fn(&str) -> bool| {
        db.skills
            .get(id)
            .is_some_and(|s| s.slot.as_deref().is_some_and(want))
    };
    let skill_ids: Vec<u32> = db.skills.keys().copied().collect();
    let mechanic_ids: Vec<u32> = skill_ids
        .iter()
        .copied()
        .filter(|id| slot_is(id, |slot| slot.starts_with("Profession")))
        .collect();
    let elite_ids: Vec<u32> = skill_ids
        .iter()
        .copied()
        .filter(|id| slot_is(id, |slot| slot == "Elite"))
        .collect();

    let rows = [
        ("minor traits", &minor_ids, SourceType::Trait),
        ("major traits", &major_ids, SourceType::Trait),
        ("runes (exotic)", &rune_ids, SourceType::Rune),
        ("sigils (exotic)", &sigil_ids, SourceType::Sigil),
        ("relics (exotic)", &relic_ids, SourceType::Relic),
        ("profession skills", &mechanic_ids, SourceType::Skill),
        ("elite skills", &elite_ids, SourceType::Skill),
    ];
    let classes = rows
        .into_iter()
        .map(|(class, ids, source)| {
            let (executable, abstaining, coverage) = tally(ids, source, &verdicts);
            ClassRow {
                class,
                population: ids.len(),
                executable,
                abstaining,
                coverage,
                none: ids.len() - executable - abstaining - coverage,
            }
        })
        .collect();

    let with_record = |ids: &[u32]| {
        let (e, a, c) = tally(ids, SourceType::Trait, &verdicts);
        e + a + c
    };
    let professions = by_profession
        .into_iter()
        .map(|(profession, (minors, majors))| ProfessionRow {
            minor_with: with_record(&minors),
            minor_total: minors.len(),
            major_with: with_record(&majors),
            major_total: majors.len(),
            profession,
        })
        .collect();

    let rune_tier_lines = rune_ids
        .iter()
        .filter_map(|id| db.items.get(id))
        .filter_map(|item| item.details.as_ref())
        .map(|d| d.bonuses.len())
        .sum();

    CoverageTable {
        classes,
        professions,
        rune_tier_lines,
    }
}

impl CoverageTable {
    /// The Gate 1 table as the sprint file prints it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "| Source class | Population | With record | Executable | Abstaining | Coverage | None |\n",
        );
        out.push_str("|---|---|---|---|---|---|---|\n");
        for row in &self.classes {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                row.class,
                row.population,
                row.with_record(),
                row.executable,
                row.abstaining,
                row.coverage,
                row.none
            ));
        }
        out.push_str(&format!(
            "\nRune tier-bonus lines across the rune population: {}\n\n",
            self.rune_tier_lines
        ));
        out.push_str("| Profession | Minor traits | Major traits |\n");
        out.push_str("|---|---|---|\n");
        for row in &self.professions {
            out.push_str(&format!(
                "| {} | {}/{} | {}/{} |\n",
                row.profession, row.minor_with, row.minor_total, row.major_with, row.major_total
            ));
        }
        out
    }
}
