//! Kit reconstruction from an Elite Insights player.
//!
//! A log carries the elite spec, the weapons it saw, what was cast and four
//! squad-relative stat ranks; it carries no traits, gear, rune, sigils or
//! relic. So the kit is assembled from three sources in priority order: the
//! log, a chat code supplied beside it, and the nearest published build of the
//! same elite spec and mode. Every field records which one it came from.

use std::collections::{BTreeMap, BTreeSet};

use crate::benchmark::{plate_from, BenchmarkBuild};
use crate::build_template::{self, BuildTemplate};
use crate::gamedb::GameDb;
use crate::providers::{GearRow, ProviderBuild, SpecLine};
use crate::validation::ValidatedBuild;

use super::ei_log::{EiLog, EiPlayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Log,
    ChatCode,
    Corpus,
    Missing,
}

#[derive(Debug, Clone)]
pub struct ReconstructedKit {
    /// Feeds `benchmark::plate_from` unchanged.
    pub build: BenchmarkBuild,
    pub specs: Provenance,
    pub traits: Provenance,
    /// The least direct source any of the five slots came from.
    pub skills: Provenance,
    pub weapons: Provenance,
    /// Prefix, rune, sigils, relic. Always the neighbour's: logs carry no gear.
    pub gear: Provenance,
    /// `source_url` of the corpus build used.
    pub neighbour: Option<String>,
    /// Stat-rank checks against the prefix. A flag, never a swap.
    pub stat_flags: Vec<String>,
    /// Opening casts from the log, handed to the referee.
    pub opener: Vec<u32>,
}

/// API weapon type ids as the SotO chat-code trailer writes them
/// (https://wiki.guildwars2.com/wiki/Chat_link_format, build templates).
/// Mirrors the addon's encoder (`ui::main_view::character::weapon_type_id`),
/// which the optimizer crate cannot reach. Land weapons only.
const WEAPON_TYPES: [(u16, &str); 17] = [
    (5, "Axe"),
    (35, "Longbow"),
    (47, "Dagger"),
    (49, "Focus"),
    (50, "Greatsword"),
    (51, "Hammer"),
    (53, "Mace"),
    (54, "Pistol"),
    (85, "Rifle"),
    (86, "Scepter"),
    (87, "Shield"),
    (89, "Staff"),
    (90, "Sword"),
    (102, "Torch"),
    (103, "Warhorn"),
    (107, "Shortbow"),
    (265, "Spear"),
];

fn is_weapon(slot: &str) -> bool {
    WEAPON_TYPES
        .iter()
        .any(|(_, n)| n.eq_ignore_ascii_case(slot))
}

/// Land set (0 or 1) of each name in a flat weapon list, `None` past set 2.
/// Packs by hand exactly as the validator packs a plate's flat list, so a
/// set taken from here lands in the same set after `plate_from`.
// ponytail: mirrors the private `validation::pack_weapon_stream`; share one
// function if either changes.
fn weapon_set_of(names: &[String], profession: &str) -> Vec<Option<usize>> {
    use crate::data::weapon_hands::{self, Hand, WeaponAccess};
    // (main taken, off taken) per set; a two-hander takes both.
    let mut sets = [(false, false); 2];
    let mut idx = 0usize;
    names
        .iter()
        .map(|name| {
            let known = weapon_hands::known_weapon(profession, name);
            let two_hand = if known {
                weapon_hands::access(profession, name, Hand::TwoHand) != WeaponAccess::None
            } else {
                crate::weapon_budget::is_two_handed(name, None)
            };
            let off_only = !two_hand
                && known
                && weapon_hands::access(profession, name, Hand::Main) == WeaponAccess::None;
            while idx < sets.len() {
                let (main, off) = &mut sets[idx];
                if two_hand {
                    if !*main && !*off {
                        (*main, *off) = (true, true);
                        idx += 1;
                        return Some(idx - 1);
                    }
                } else {
                    if !off_only && !*main {
                        *main = true;
                        return Some(idx);
                    }
                    if !*off {
                        *off = true;
                        return Some(idx);
                    }
                }
                idx += 1;
            }
            None
        })
        .collect()
}

fn split_sets(names: &[String], profession: &str) -> [Vec<String>; 2] {
    let mut out = [Vec::new(), Vec::new()];
    for (name, set) in names.iter().zip(weapon_set_of(names, profession)) {
        if let Some(k) = set {
            out[k].push(name.clone());
        }
    }
    out
}

/// The addon's gate for a build: plate, then the validator with no errors.
pub fn validate(build: &BenchmarkBuild, db: &GameDb) -> Result<ValidatedBuild, String> {
    let plate =
        plate_from(build, db).ok_or("plate_from: build has no three resolvable spec lines")?;
    let validated = crate::validation::validate_gemini_build(&plate, db, &build.profession);
    if !validated.errors.is_empty() {
        let why: Vec<&str> = validated.errors.iter().map(|e| e.detail.as_str()).collect();
        return Err(format!("validator: {}", why.join("; ")));
    }
    Ok(validated)
}

pub fn reconstruct(
    log: &EiLog,
    player: &EiPlayer,
    chat_code: Option<&str>,
    corpus: &[BenchmarkBuild],
    db: &GameDb,
) -> Result<ReconstructedKit, String> {
    reconstruct_with(log, player, chat_code, corpus, db, |b| {
        validate(b, db).map(|_| ())
    })
}

/// `neighbour_ok`: whether a corpus row may be the neighbour; production
/// passes [`validate`], so a kit never inherits a rune in a sigil seat.
fn reconstruct_with(
    log: &EiLog,
    player: &EiPlayer,
    chat_code: Option<&str>,
    corpus: &[BenchmarkBuild],
    db: &GameDb,
    neighbour_ok: impl Fn(&BenchmarkBuild) -> Result<(), String>,
) -> Result<ReconstructedKit, String> {
    let (profession, elite) = resolve_spec(&player.profession, db)?;
    let mode = if log.mode() == gw2_core::types::GameMode::WvW {
        "WvW"
    } else {
        "PvE"
    };

    let template = match chat_code {
        Some(code) => {
            let t = build_template::decode(code)
                .ok_or_else(|| format!("chat code {code} does not decode"))?;
            check_code_matches(&t, &profession, elite, &player.profession, db)?;
            Some(t)
        }
        None => None,
    };

    // Log bar: palette-equippable casts only, so flips and chains of a
    // utility (same slot, not on the palette) never take a seat.
    let mut by_slot: BTreeMap<&str, Vec<(u32, u32)>> = BTreeMap::new();
    for (id, n) in player.cast_counts() {
        if !db.skill_to_palette.contains_key(&id) {
            continue;
        }
        if let Some(slot @ ("Heal" | "Utility" | "Elite")) =
            db.skills.get(&id).and_then(|s| s.slot.as_deref())
        {
            by_slot.entry(slot).or_default().push((n, id));
        }
    }
    for v in by_slot.values_mut() {
        v.sort_by_key(|&(n, id)| (std::cmp::Reverse(n), id));
    }
    let top = |slot: &str, k: usize| -> Vec<u32> {
        by_slot
            .get(slot)
            .map(|v| v.iter().take(k).map(|&(_, id)| id).collect())
            .unwrap_or_default()
    };
    let log_bar: [Vec<u32>; 3] = [top("Heal", 1), top("Utility", 3), top("Elite", 1)];
    let log_skill_set: BTreeSet<u32> = log_bar.iter().flatten().copied().collect();

    // Land set k is weapons[2k..2k+2]; a set with no known weapon was not seen.
    let log_sets: [Vec<String>; 2] = [0, 1].map(|k| {
        player
            .weapons
            .iter()
            .skip(2 * k)
            .take(2)
            .filter(|w| is_weapon(w))
            .cloned()
            .collect()
    });
    let log_weapon_set: BTreeSet<String> = log_sets
        .iter()
        .flatten()
        .map(|w| w.to_lowercase())
        .collect();

    // Neighbour: same profession, mode and elite spec, platable and
    // validator-clean; one score. Ties go to the LAST row, which is
    // `find_best_benchmark`'s tie order with no role hint.
    let mut candidates: Vec<(f64, usize, &BenchmarkBuild)> = corpus
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            b.profession.eq_ignore_ascii_case(&profession)
                && b.mode.eq_ignore_ascii_case(mode)
                && elite_of(b.published.specs.iter().map(|l| l.id), db) == elite
                && plate_from(b, db).is_some()
        })
        .map(|(i, b)| {
            let skills: BTreeSet<u32> = b.published.slot_skills(db).into_iter().flatten().collect();
            let weapons: BTreeSet<String> = b
                .published
                .gear
                .iter()
                .filter(|g| is_weapon(&g.slot))
                .map(|g| g.slot.to_lowercase())
                .collect();
            (
                jaccard(&log_skill_set, &skills) + jaccard(&log_weapon_set, &weapons),
                i,
                b,
            )
        })
        .collect();
    if candidates.is_empty() {
        return Err(format!("no published {} {mode} build", player.profession));
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    // Best first; every better row the validator refused is named.
    let mut skipped: Vec<String> = Vec::new();
    let neighbour = candidates
        .iter()
        .find_map(|&(_, _, b)| match neighbour_ok(b) {
            Ok(()) => Some(b),
            Err(e) => {
                skipped.push(format!("neighbour {} skipped: {e}", b.source_url));
                None
            }
        })
        .ok_or_else(|| {
            format!(
                "no validator-clean published {} {mode} build ({})",
                player.profession,
                skipped.join(" | ")
            )
        })?;

    // Traits.
    let (specs, traits) = match &template {
        Some(t) => (
            t.specs
                .iter()
                .map(|s| SpecLine {
                    id: s.id,
                    trait_ids: db
                        .specializations
                        .get(&s.id)
                        .map(|spec| BuildTemplate::trait_ids(s, &spec.major_traits))
                        .unwrap_or_default(),
                })
                .collect(),
            Provenance::ChatCode,
        ),
        None => (neighbour.published.specs.clone(), Provenance::Corpus),
    };

    // Skill bar: heal, three utilities, elite; gaps from code, then neighbour.
    let code_bar: Vec<Option<u32>> = template
        .as_ref()
        .map(|t| {
            t.skills
                .iter()
                .map(|p| db.palette_to_skill.get(p).copied())
                .collect()
        })
        .unwrap_or_default();
    let corpus_bar = neighbour.published.slot_skills(db);
    let mut skills = Provenance::Log;
    let mut bar: Vec<Option<u32>> = Vec::with_capacity(5);
    for (seats, from, k) in [
        (0..1, &log_bar[0], 1),
        (1..4, &log_bar[1], 3),
        (4..5, &log_bar[2], 1),
    ] {
        let mut picked: Vec<u32> = from.clone();
        for (source, prov) in [
            (&code_bar, Provenance::ChatCode),
            (&corpus_bar, Provenance::Corpus),
        ] {
            for id in seats
                .clone()
                .filter_map(|i| source.get(i).copied().flatten())
            {
                if picked.len() < k && !picked.contains(&id) {
                    picked.push(id);
                    skills = weaker(skills, prov);
                }
            }
        }
        if picked.len() < k {
            skills = Provenance::Missing;
        }
        bar.extend(picked.into_iter().map(Some));
        bar.resize(seats.end, None);
    }
    let skill_ids: Vec<u32> = if bar.iter().all(Option::is_some) {
        bar.into_iter().flatten().collect()
    } else {
        Vec::new() // positional list cannot hold a gap; slot_skills falls back to the code
    };

    // Weapons.
    let code_weapons: Vec<String> = template
        .as_ref()
        .map(|t| {
            t.weapons
                .iter()
                .filter_map(|id| WEAPON_TYPES.iter().find(|(t, _)| t == id))
                .map(|(_, n)| n.to_string())
                .collect()
        })
        .unwrap_or_default();
    let prefix = neighbour
        .published
        .dominant_stat()
        .unwrap_or_else(|| neighbour.gear_prefix.clone());
    // Per land set: the log where it saw the set, else the chat code, else
    // the neighbour's own rows. Provenance is the weakest source used.
    let code_sets = split_sets(&code_weapons, &profession);
    let neighbour_weapons: Vec<&GearRow> = neighbour
        .published
        .gear
        .iter()
        .filter(|g| is_weapon(&g.slot))
        .collect();
    let neighbour_slots: Vec<String> = neighbour_weapons.iter().map(|g| g.slot.clone()).collect();
    let neighbour_set = weapon_set_of(&neighbour_slots, &profession);
    let new_row = |slot: &String| GearRow {
        slot: slot.clone(),
        stat: prefix.clone(),
        item_id: None,
        upgrade_ids: Vec::new(),
    };
    let mut weapon_rows: Vec<GearRow> = Vec::new();
    let mut set_sources: Vec<(usize, Provenance)> = Vec::new();
    for k in 0..2 {
        let (rows, prov): (Vec<GearRow>, Provenance) = if !log_sets[k].is_empty() {
            (log_sets[k].iter().map(new_row).collect(), Provenance::Log)
        } else if !code_sets[k].is_empty() {
            (
                code_sets[k].iter().map(new_row).collect(),
                Provenance::ChatCode,
            )
        } else {
            let rows = neighbour_weapons
                .iter()
                .zip(&neighbour_set)
                .filter(|(_, set)| **set == Some(k))
                .map(|(g, _)| (*g).clone())
                .collect();
            (rows, Provenance::Corpus)
        };
        if !rows.is_empty() {
            weapon_rows.extend(rows);
            set_sources.push((k, prov));
        }
    }
    let weapons = set_sources
        .iter()
        .map(|&(_, p)| p)
        .reduce(weaker)
        .unwrap_or(Provenance::Corpus);
    let weapon_notes: Vec<String> = if set_sources.iter().any(|&(_, p)| p == Provenance::Log) {
        set_sources
            .iter()
            .filter(|&&(_, p)| p != Provenance::Log)
            .map(|&(k, p)| format!("weapon set {} not seen in log: {p:?}", k + 1))
            .collect()
    } else {
        Vec::new()
    };
    let gear: Vec<GearRow> = neighbour
        .published
        .gear
        .iter()
        .filter(|g| !is_weapon(&g.slot))
        .cloned()
        .chain(weapon_rows)
        .collect();

    let build_code = chat_code
        .map(str::to_string)
        .or_else(|| neighbour.build_code.clone());
    let build = BenchmarkBuild {
        source: "ei_log".into(),
        profession,
        spec_name: player.profession.clone(),
        mode: mode.into(),
        role: neighbour.role.clone(),
        build_code: build_code.clone(),
        gear_prefix: neighbour.gear_prefix.clone(),
        source_url: neighbour.source_url.clone(),
        scraped_at: neighbour.scraped_at.clone(),
        published: ProviderBuild {
            build_code,
            specs,
            skill_ids,
            gear,
            rune_id: neighbour.published.rune_id,
            sigil_ids: neighbour.published.sigil_ids.clone(),
            relic_id: neighbour.published.relic_id,
            amulet_id: None,
            prose: String::new(),
        },
        benchmark_dps: None,
        log_url: None,
    };

    Ok(ReconstructedKit {
        build,
        specs: traits,
        traits,
        skills,
        weapons,
        gear: Provenance::Corpus,
        neighbour: Some(neighbour.source_url.clone()),
        stat_flags: skipped
            .into_iter()
            .chain(weapon_notes)
            .chain(stat_flags(player, &prefix, db))
            .collect(),
        opener: player
            .opener(12)
            .into_iter()
            .filter(|id| db.skills.contains_key(id))
            .collect(),
    })
}

/// Log spec name -> (profession, elite spec id). A core player's name is the
/// profession itself.
fn resolve_spec(name: &str, db: &GameDb) -> Result<(String, Option<u32>), String> {
    if let Some(s) = db
        .specializations
        .values()
        .find(|s| s.elite && s.name == name)
    {
        return Ok((s.profession.clone(), Some(s.id)));
    }
    if db.professions.contains_key(name) {
        return Ok((name.to_string(), None));
    }
    Err(format!("unknown spec {name}"))
}

fn elite_of(ids: impl IntoIterator<Item = u32>, db: &GameDb) -> Option<u32> {
    ids.into_iter()
        .find(|id| db.specializations.get(id).is_some_and(|s| s.elite))
}

fn check_code_matches(
    t: &BuildTemplate,
    profession: &str,
    elite: Option<u32>,
    log_spec: &str,
    db: &GameDb,
) -> Result<(), String> {
    let code_elite = elite_of(t.specs.iter().map(|s| s.id), db);
    let foreign = t.specs.iter().any(|s| {
        db.specializations
            .get(&s.id)
            .is_some_and(|spec| spec.profession != profession)
    });
    if foreign || code_elite != elite {
        let named = code_elite
            .and_then(|id| db.specializations.get(&id))
            .map_or("core", |s| s.name.as_str());
        return Err(format!(
            "chat code elite spec {named} contradicts log spec {log_spec}"
        ));
    }
    Ok(())
}

fn jaccard<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

fn weaker(a: Provenance, b: Provenance) -> Provenance {
    let rank = |p| match p {
        Provenance::Log => 0,
        Provenance::ChatCode => 1,
        Provenance::Corpus => 2,
        Provenance::Missing => 3,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// Flags a log rank >= 7 for an attribute the prefix lacks, or <= 2 for one
/// it carries as a major. Ranks are squad-relative; all-zero means the log
/// did not record them (every 2026 log), so nothing is checked.
fn stat_flags(player: &EiPlayer, prefix: &str, db: &GameDb) -> Vec<String> {
    let ranks = [
        (player.healing, "healing", &["Healing", "HealingPower"][..]),
        (
            player.concentration,
            "concentration",
            &["BoonDuration", "Concentration"][..],
        ),
        (player.condition, "condition", &["ConditionDamage"][..]),
        (player.toughness, "toughness", &["Toughness"][..]),
    ];
    if ranks.iter().all(|(r, _, _)| *r == 0) {
        return Vec::new();
    }
    let Some(stat) = db.itemstat_by_name(prefix) else {
        return Vec::new();
    };
    let top = stat
        .attributes
        .iter()
        .map(|a| a.multiplier)
        .fold(0.0, f64::max);
    ranks
        .iter()
        .filter_map(|&(rank, label, names)| {
            let attr = stat
                .attributes
                .iter()
                .find(|a| names.contains(&a.attribute.as_str()));
            match attr {
                None if rank >= 7 => Some(format!(
                    "prefix {prefix} lacks {label} but log {label} rank {rank}"
                )),
                Some(a) if rank <= 2 && a.multiplier >= top => Some(format!(
                    "prefix {prefix} has major {label} but log {label} rank {rank}"
                )),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fidelity::ei_log::{EiCast, EiRotation};
    use gw2_api::models::{ItemStat, Skill, Specialization, StatAttribute};

    /// Power Reaper chat code from the golem fixture's `codes.json`:
    /// Necromancer, specs 53 / 50 / 34 (Reaper).
    const CODE: &str = "[&DQg1KTIlIjbBEigPQAGBAIEAQAHxEnUBlQCVAAAAAAAAAAAAAAAAAAAAAAA=]";

    fn spec(id: u32, name: &str, elite: bool) -> Specialization {
        Specialization {
            id,
            name: name.into(),
            profession: "Necromancer".into(),
            elite,
            minor_traits: Vec::new(),
            major_traits: (id * 100 + 1..=id * 100 + 9).collect(),
            weapon_trait: None,
            icon: None,
            background: None,
            profession_icon: None,
            profession_icon_big: None,
        }
    }

    fn skill(id: u32, slot: &str) -> Skill {
        serde_json::from_value(
            serde_json::json!({"id": id, "name": format!("s{id}"), "slot": slot}),
        )
        .expect("skill json")
    }

    /// Necromancer specs 53, 50, 39 core, 34 Reaper, 62 Harbinger; heal 10,
    /// utilities 20-24, elites 30-31 on the palette; weapon skill 99 off it.
    fn db() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        for s in [
            spec(53, "Spite", false),
            spec(50, "Soul Reaping", false),
            spec(39, "Curses", false),
            spec(34, "Reaper", true),
            spec(62, "Harbinger", true),
        ] {
            db.specializations.insert(s.id, s);
        }
        for (id, slot) in [
            (10, "Heal"),
            (20, "Utility"),
            (21, "Utility"),
            (22, "Utility"),
            (23, "Utility"),
            (24, "Utility"),
            (30, "Elite"),
            (31, "Elite"),
        ] {
            db.skills.insert(id, skill(id, slot));
            db.skill_to_palette.insert(id, id + 1000);
            db.palette_to_skill.insert(id + 1000, id);
        }
        db.skills.insert(99, skill(99, "Weapon_1"));
        db.itemstats.insert(
            584,
            ItemStat {
                id: 584,
                name: "Berserker's".into(),
                attributes: ["Power", "Precision", "CritDamage"]
                    .iter()
                    .zip([0.35, 0.25, 0.25])
                    .map(|(a, m)| StatAttribute {
                        attribute: a.to_string(),
                        multiplier: m,
                        value: 0,
                    })
                    .collect(),
            },
        );
        db
    }

    fn row(url: &str, skills: [u32; 5], weapons: &[&str]) -> BenchmarkBuild {
        BenchmarkBuild {
            profession: "Necromancer".into(),
            spec_name: "Reaper".into(),
            mode: "PvE".into(),
            role: "Power DPS".into(),
            gear_prefix: "Berserker's".into(),
            source_url: url.into(),
            published: ProviderBuild {
                specs: vec![
                    SpecLine {
                        id: 53,
                        trait_ids: vec![5301],
                    },
                    SpecLine {
                        id: 50,
                        trait_ids: vec![5001],
                    },
                    SpecLine {
                        id: 34,
                        trait_ids: vec![3401],
                    },
                ],
                skill_ids: skills.to_vec(),
                gear: weapons
                    .iter()
                    .map(|w| GearRow {
                        slot: w.to_string(),
                        stat: "Berserker's".into(),
                        item_id: None,
                        upgrade_ids: Vec::new(),
                    })
                    .collect(),
                rune_id: Some(7),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn player(casts: &[(u32, usize)], weapons: &[&str]) -> EiPlayer {
        EiPlayer {
            name: "p".into(),
            profession: "Reaper".into(),
            weapons: weapons.iter().map(|w| w.to_string()).collect(),
            rotation: casts
                .iter()
                .map(|&(id, n)| EiRotation {
                    id: i64::from(id),
                    skills: (0..n)
                        .map(|i| EiCast {
                            cast_time: i as i64 * 1000 + i64::from(id),
                        })
                        .collect(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn corpus() -> Vec<BenchmarkBuild> {
        vec![
            row("near", [10, 20, 21, 22, 30], &["Greatsword", "Spear"]),
            row("far", [10, 23, 24, 22, 31], &["Axe", "Focus"]),
        ]
    }

    /// The toy db has no profession palettes or items, so the real validator
    /// refuses every row; these tests exercise the rest of the pipeline.
    fn lenient(
        log: &EiLog,
        p: &EiPlayer,
        code: Option<&str>,
        corpus: &[BenchmarkBuild],
        db: &GameDb,
    ) -> Result<ReconstructedKit, String> {
        reconstruct_with(log, p, code, corpus, db, |_| Ok(()))
    }

    #[test]
    fn a_validator_refused_neighbour_is_skipped_and_named() {
        let db = db();
        // "far" is the better match, as in the overlap test.
        let p = player(&[(23, 4), (24, 2), (31, 1)], &["Axe", "Focus"]);
        let refuse = |name: &'static str| {
            move |b: &BenchmarkBuild| {
                if b.source_url == name {
                    Err("validator: rune in a sigil seat".to_string())
                } else {
                    Ok(())
                }
            }
        };
        let kit = reconstruct_with(&EiLog::default(), &p, None, &corpus(), &db, refuse("far"))
            .expect("kit");
        assert_eq!(kit.neighbour.as_deref(), Some("near"));
        // The log saw set 1 only; "near" supplies set 2 and says so.
        assert_eq!(
            kit.stat_flags,
            [
                "neighbour far skipped: validator: rune in a sigil seat",
                "weapon set 2 not seen in log: Corpus"
            ]
        );
        // Refusing the worse row changes nothing and flags nothing.
        let kit = reconstruct_with(&EiLog::default(), &p, None, &corpus(), &db, refuse("near"))
            .expect("kit");
        assert_eq!(kit.neighbour.as_deref(), Some("far"));
        assert!(kit.stat_flags.is_empty());
        // All refused: the error names every skip.
        let err = reconstruct_with(&EiLog::default(), &p, None, &corpus(), &db, |_| {
            Err("validator: x".to_string())
        })
        .unwrap_err();
        assert_eq!(
            err,
            "no validator-clean published Reaper PvE build (neighbour far skipped: validator: x | neighbour near skipped: validator: x)"
        );
    }

    #[test]
    fn chat_code_path_takes_specs_from_decode_and_trait_ids() {
        let db = db();
        let p = player(
            &[(10, 2), (20, 3)],
            &["Greatsword", "2Hand", "Spear", "2Hand"],
        );
        let kit = lenient(&EiLog::default(), &p, Some(CODE), &corpus(), &db).expect("kit");
        let t = build_template::decode(CODE).expect("decodes");
        let want: Vec<SpecLine> = t
            .specs
            .iter()
            .map(|s| SpecLine {
                id: s.id,
                trait_ids: BuildTemplate::trait_ids(s, &db.specializations[&s.id].major_traits),
            })
            .collect();
        assert_eq!(kit.build.published.specs, want);
        assert_eq!(kit.traits, Provenance::ChatCode);
        assert_eq!(kit.specs, Provenance::ChatCode);
        assert_eq!(kit.weapons, Provenance::Log);
        assert_eq!(kit.build.build_code.as_deref(), Some(CODE));
    }

    #[test]
    fn no_code_path_picks_the_higher_overlap_neighbour() {
        let db = db();
        // Casts 23, 24 and elite 31 match "far"; weapons Axe/Focus too.
        let p = player(
            &[(5, 1), (10, 1), (23, 4), (24, 2), (31, 1), (99, 40)],
            &["Axe", "Focus", "Unknown", "Unknown"],
        );
        let kit = lenient(&EiLog::default(), &p, None, &corpus(), &db).expect("kit");
        assert_eq!(kit.neighbour.as_deref(), Some("far"));
        assert_eq!(kit.traits, Provenance::Corpus);
        assert_eq!(kit.gear, Provenance::Corpus);
        // Heal, two utilities and elite from the log; the third utility from
        // the neighbour, so the bar as a whole is Corpus.
        assert_eq!(kit.build.published.skill_ids, [10, 23, 24, 22, 31]);
        assert_eq!(kit.skills, Provenance::Corpus);
        assert_eq!(kit.build.published.rune_id, Some(7));
        // Id 5 is cast first but unknown to the db: dropped from the opener.
        assert_eq!(kit.opener.first(), Some(&10));
        assert!(!kit.opener.contains(&5));
        assert!(plate_from(&kit.build, &db).is_some());
    }

    #[test]
    fn an_unseen_weapon_set_keeps_the_neighbours_rows() {
        let db = db();
        let near = || vec![row("near", [10, 20, 21, 22, 30], &["Greatsword", "Spear"])];
        // Set 1 unseen, set 2 Axe/Focus: set 1 stays the neighbour's Greatsword.
        let p = player(&[(10, 1)], &["Unknown", "Unknown", "Axe", "Focus"]);
        let kit = lenient(&EiLog::default(), &p, None, &near(), &db).expect("kit");
        let slots: Vec<&str> = kit
            .build
            .published
            .gear
            .iter()
            .map(|g| g.slot.as_str())
            .collect();
        assert_eq!(slots, ["Greatsword", "Axe", "Focus"]);
        assert_eq!(kit.weapons, Provenance::Corpus);
        assert_eq!(kit.stat_flags, ["weapon set 1 not seen in log: Corpus"]);
        // Both sets seen: all from the log, no note.
        let p = player(&[(10, 1)], &["Axe", "Focus", "Greatsword", "2Hand"]);
        let kit = lenient(&EiLog::default(), &p, None, &near(), &db).expect("kit");
        assert_eq!(kit.weapons, Provenance::Log);
        assert!(kit.stat_flags.is_empty(), "{:?}", kit.stat_flags);
        // Nothing seen: the neighbour's weapons, untouched.
        let p = player(&[(10, 1)], &["Unknown", "Unknown", "Unknown", "Unknown"]);
        let kit = lenient(&EiLog::default(), &p, None, &near(), &db).expect("kit");
        assert_eq!(kit.weapons, Provenance::Corpus);
        assert_eq!(kit.build.published.gear.len(), 2);
        assert!(kit.stat_flags.is_empty(), "{:?}", kit.stat_flags);
    }

    #[test]
    fn weapon_sets_pack_by_hand() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Two two-handers, one set each.
        assert_eq!(
            weapon_set_of(&names(&["Greatsword", "Spear"]), "Necromancer"),
            [Some(0), Some(1)]
        );
        // Main + off, then a two-hander.
        assert_eq!(
            weapon_set_of(&names(&["Axe", "Focus", "Greatsword"]), "Necromancer"),
            [Some(0), Some(0), Some(1)]
        );
        // A third set does not exist.
        assert_eq!(
            weapon_set_of(&names(&["Greatsword", "Staff", "Axe"]), "Necromancer"),
            [Some(0), Some(1), None]
        );
    }

    #[test]
    fn chat_code_with_a_different_elite_is_rejected() {
        let db = db();
        let mut p = player(&[(10, 1)], &["Axe"]);
        p.profession = "Harbinger".into();
        let mut rows = corpus();
        for r in &mut rows {
            r.published.specs[2].id = 62;
        }
        let err = lenient(&EiLog::default(), &p, Some(CODE), &rows, &db).unwrap_err();
        assert_eq!(
            err,
            "chat code elite spec Reaper contradicts log spec Harbinger"
        );
    }

    #[test]
    fn healing_rank_nine_on_berserkers_is_flagged() {
        let db = db();
        let mut p = player(&[(10, 1)], &["Greatsword"]);
        p.healing = 9;
        p.toughness = 5;
        let kit = lenient(&EiLog::default(), &p, None, &corpus(), &db).expect("kit");
        assert_eq!(
            kit.stat_flags,
            [
                "weapon set 2 not seen in log: Corpus",
                "prefix Berserker's lacks healing but log healing rank 9"
            ]
        );
    }

    #[test]
    fn unknown_spec_and_missing_neighbour_name_why() {
        let db = db();
        let mut p = player(&[], &[]);
        p.profession = "Chronomancer".into();
        let err = lenient(&EiLog::default(), &p, None, &corpus(), &db).unwrap_err();
        assert_eq!(err, "unknown spec Chronomancer");
        p.profession = "Harbinger".into();
        let err = lenient(&EiLog::default(), &p, None, &corpus(), &db).unwrap_err();
        assert_eq!(err, "no published Harbinger PvE build");
    }

    fn cached_db() -> GameDb {
        let cache = gw2_api::cache::DataCache::new(
            gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir"),
        );
        GameDb::load(&cache).expect("game data cached \u{2014} sync it in-game first")
    }

    #[test]
    #[ignore = "needs a synced game-data cache; see dev.cfg"]
    fn fixture_squads_reconstruct_and_plate() {
        let db = cached_db();
        // The synced corpus lives in the addon's directory, beside its cache.
        let cache = gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir");
        let corpus = crate::scraper::load_benchmarks(cache.parent().expect("addon dir"));
        assert!(!corpus.is_empty(), "synced benchmark corpus");
        let dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ei_logs"
        ));
        let codes: BTreeMap<String, BTreeMap<String, String>> = serde_json::from_str(
            &std::fs::read_to_string(dir.join("codes.json")).expect("codes.json"),
        )
        .expect("codes.json parses");
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .expect("fixture dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .filter(|p| p.file_name().is_some_and(|n| n != "codes.json"))
            .collect();
        paths.sort();
        let (mut total, mut plated) = (0, 0);
        for path in paths {
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            let log = super::super::ei_log::load(&path).expect("fixture loads");
            for p in log.squad() {
                total += 1;
                let code = codes
                    .get(&file)
                    .and_then(|m| m.get(&p.name))
                    .map(String::as_str);
                match reconstruct(&log, p, code, &corpus, &db) {
                    Ok(kit) if plate_from(&kit.build, &db).is_some() => {
                        plated += 1;
                        println!(
                            "{file} {} {}: specs {:?} traits {:?} skills {:?} weapons {:?} gear {:?} opener {} flags {:?} <- {}",
                            p.name,
                            p.profession,
                            kit.specs,
                            kit.traits,
                            kit.skills,
                            kit.weapons,
                            kit.gear,
                            kit.opener.len(),
                            kit.stat_flags,
                            kit.neighbour.unwrap_or_default()
                        );
                    }
                    Ok(_) => println!("{file} {} {}: FAIL plate_from None", p.name, p.profession),
                    Err(e) => println!("{file} {} {}: FAIL {e}", p.name, p.profession),
                }
            }
        }
        println!("plated {plated}/{total}");
        assert!(plated * 5 >= total * 4, "plated {plated}/{total} < 80%");
    }
}
