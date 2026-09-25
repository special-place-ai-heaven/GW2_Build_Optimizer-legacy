//! Gate 1's instrument: how much of the game has an effect record the
//! named engine can run.
//!
//! Verdicts are engine-true. **Executable** means a record
//! `wvw_timeline::unexecutable_reason` leaves runnable at load: the engine
//! has state for it and a consumer for its payload. Schema shape alone is
//! not a run. A `ProcEffect` with no inner payload is a multi-impact field
//! (impacts over an interval). The flow sim and the timeline have no
//! consumer for that, so the record is **Abstaining** and the reason names
//! the missing impacts/interval consumer.
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
//! - **executable**: at least one record the named engine actually runs.
//! - **abstaining**: it has a payload record, but the engine has no state
//!   or no consumer for it and says so (a pet's on-crit, an unemitted
//!   trigger, a positional or distance gate, an unmodelled pool, a
//!   `ProcEffect` that needs an impacts/interval consumer).
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
    pub minor_executable: usize,
    pub minor_abstaining: usize,
    pub minor_coverage: usize,
    pub minor_none: usize,
    pub minor_total: usize,
    pub major_executable: usize,
    pub major_abstaining: usize,
    pub major_coverage: usize,
    pub major_none: usize,
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
    verdicts_detailed()
        .into_iter()
        .map(|(key, (verdict, _detail))| (key, verdict))
        .collect()
}

/// Every recorded source's verdict plus the detail behind it: the
/// `unexecutable_reason` text for an abstaining record, or the coverage
/// class/mechanic for a coverage-only one. `verdicts()` is this with the
/// detail dropped, so the two can never disagree (doctrine rule 8).
fn verdicts_detailed() -> HashMap<(String, u32), (SourceCoverage, Option<String>)> {
    let mut out: HashMap<(String, u32), (SourceCoverage, Option<String>)> = HashMap::new();
    for mode in ["PvE", "PvP", "WvW"] {
        for effect in effects().effects_for_mode(mode) {
            let (verdict, detail) = if let Some(block) = &effect.coverage {
                let detail = match &block.mechanic {
                    Some(mechanic) => format!("{:?}: {mechanic}", block.class),
                    None => format!("{:?}", block.class),
                };
                (SourceCoverage::Coverage, Some(detail))
            } else if let Some(reason) = crate::rotation::wvw_timeline::unexecutable_reason(effect)
            {
                (SourceCoverage::Abstaining, Some(reason))
            } else {
                (SourceCoverage::Executable, None)
            };
            let key = (format!("{:?}", effect.source_type), effect.source_id);
            let entry = out.entry(key).or_insert((SourceCoverage::None, None));
            if verdict > entry.0 {
                *entry = (verdict, detail);
            }
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

    let buckets = |ids: &[u32]| {
        let (executable, abstaining, coverage) = tally(ids, SourceType::Trait, &verdicts);
        (
            executable,
            abstaining,
            coverage,
            ids.len() - executable - abstaining - coverage,
        )
    };
    let professions = by_profession
        .into_iter()
        .map(|(profession, (minors, majors))| {
            let (minor_executable, minor_abstaining, minor_coverage, minor_none) = buckets(&minors);
            let (major_executable, major_abstaining, major_coverage, major_none) = buckets(&majors);
            ProfessionRow {
                profession,
                minor_executable,
                minor_abstaining,
                minor_coverage,
                minor_none,
                minor_total: minors.len(),
                major_executable,
                major_abstaining,
                major_coverage,
                major_none,
                major_total: majors.len(),
            }
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

/// One trait's verdict, for one profession. Sorted by specialization, then
/// tier and id — the same order a spec's trait grid reads in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceVerdict {
    pub id: u32,
    pub name: String,
    /// "Minor" or "Major", straight from the trait's own `slot` field.
    pub slot: String,
    pub specialization: String,
    pub verdict: SourceCoverage,
    /// `unexecutable_reason` text for Abstaining, coverage class/mechanic
    /// for Coverage, empty otherwise.
    pub reason: String,
}

/// Every minor and major trait of `profession`, with its Gate 1 verdict and
/// the reason an abstaining or coverage-only trait earned it — the detail
/// behind one profession row of `coverage_table`.
pub fn profession_sources(db: &GameDb, profession: &str) -> Vec<SourceVerdict> {
    let details = verdicts_detailed();
    let trait_tag = format!("{:?}", SourceType::Trait);

    let mut specs: Vec<&gw2_api::models::Specialization> = db
        .specializations
        .values()
        .filter(|spec| spec.profession == profession)
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = Vec::new();
    for spec in specs {
        let mut ids: Vec<u32> = spec
            .minor_traits
            .iter()
            .chain(spec.major_traits.iter())
            .copied()
            .collect();
        ids.sort_by_key(|id| db.traits.get(id).map(|t| (t.tier, t.id)));
        for id in ids {
            let Some(t) = db.traits.get(&id) else {
                continue;
            };
            let (verdict, detail) = details
                .get(&(trait_tag.clone(), id))
                .cloned()
                .unwrap_or((SourceCoverage::None, None));
            out.push(SourceVerdict {
                id,
                name: t.name.clone(),
                slot: t.slot.clone(),
                specialization: spec.name.clone(),
                verdict,
                reason: detail.unwrap_or_default(),
            });
        }
    }
    out
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
        out.push_str("| Profession | Minor exec/abst/cov/none of total | Major exec/abst/cov/none of total |\n");
        out.push_str("|---|---|---|\n");
        for row in &self.professions {
            out.push_str(&format!(
                "| {} | {}/{}/{}/{} of {} | {}/{}/{}/{} of {} |\n",
                row.profession,
                row.minor_executable,
                row.minor_abstaining,
                row.minor_coverage,
                row.minor_none,
                row.minor_total,
                row.major_executable,
                row.major_abstaining,
                row.major_coverage,
                row.major_none,
                row.major_total,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::normalized_effects::SourceType;

    #[test]
    fn profession_row_buckets_sum_to_totals() {
        let row = ProfessionRow {
            profession: "Test".into(),
            minor_executable: 2,
            minor_abstaining: 1,
            minor_coverage: 0,
            minor_none: 0,
            minor_total: 3,
            major_executable: 5,
            major_abstaining: 2,
            major_coverage: 1,
            major_none: 1,
            major_total: 9,
        };
        assert_eq!(
            row.minor_executable + row.minor_abstaining + row.minor_coverage + row.minor_none,
            row.minor_total
        );
        assert_eq!(
            row.major_executable + row.major_abstaining + row.major_coverage + row.major_none,
            row.major_total
        );
    }

    /// E28: Rushing Justice / Flowing Resolve / Crashing Courage flames
    /// (`skill:62668/62603/62648:0`) are PvE `ProcEffect` records whose
    /// value is coefficient × impacts. Neither engine plays that field, so
    /// the tally must not call them Executable.
    #[test]
    fn rushing_justice_flames_abstain_without_impacts_interval_consumer() {
        const FLAMES: [(&str, u32); 3] = [
            ("skill:62668:0", 62668),
            ("skill:62603:0", 62603),
            ("skill:62648:0", 62648),
        ];
        let data = crate::data::normalized_effects::effects();
        let details = verdicts_detailed();
        let counts = verdicts();
        let tag = format!("{:?}", SourceType::Skill);
        for (effect_id, id) in FLAMES {
            let effect = data
                .effects_for_mode("PvE")
                .iter()
                .find(|e| e.effect_id == effect_id)
                .unwrap_or_else(|| panic!("{effect_id} missing"));
            let reason = crate::rotation::wvw_timeline::unexecutable_reason(effect)
                .unwrap_or_else(|| panic!("{effect_id} claimed executable"));
            assert!(
                reason.contains("impacts") && reason.contains("interval"),
                "{effect_id} reason {reason:?} does not name the missing consumer"
            );
            let (verdict, detail) = details
                .get(&(tag.clone(), id))
                .cloned()
                .unwrap_or_else(|| panic!("{effect_id} has no verdict"));
            assert_eq!(verdict, SourceCoverage::Abstaining, "{effect_id}");
            assert_eq!(detail.as_deref(), Some(reason.as_str()));
            let (executable, abstaining, coverage) = tally(&[id], SourceType::Skill, &counts);
            assert_eq!(
                (executable, abstaining, coverage),
                (0, 1, 0),
                "{effect_id} tallied Executable"
            );
        }
        // A coefficient ProcEffect the timeline does run stays Executable.
        let fire = data
            .effects_for_mode("PvE")
            .iter()
            .find(|e| e.effect_id == "sigil:24548:0")
            .expect("sigil of fire");
        assert!(fire.inner_category.is_some());
        assert_eq!(
            crate::rotation::wvw_timeline::unexecutable_reason(fire),
            None
        );
        let (executable, _, _) = tally(&[24548], SourceType::Sigil, &counts);
        assert_eq!(executable, 1);
    }
}
