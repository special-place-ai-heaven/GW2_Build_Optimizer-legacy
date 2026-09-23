//! Every card the overlay can ask for, against a snapshot of the real corpus.
//!
//! The unit tests in `benchmark.rs` and `picks.rs` prove the pieces on
//! hand-made rows. They cannot prove the thing that actually broke in-game:
//! that a request the player can make with the chips on screen comes back
//! with a card that does what was asked for. That needs the corpus, so the
//! corpus is checked in.
//!
//! `tests/fixtures/benchmarks/` is a copy of the player's synced
//! `{addon_dir}/benchmarks/` (scraped 2026-09-20, 740 rows, 62 files).
//! `published.prose` is truncated to its first 600 characters because
//! [`picks::stated_scale`] reads `chars().take(600)` and nothing else in the
//! matching path reads the page text at all; ids, roles, gear and chat codes
//! are copied verbatim.
//!
//! Every test that judges a card is `#[ignore]`: a card is now decided by
//! MEASURING the reference - plate, validate, referee, compare the realized
//! axes to the player's weights - and none of that can happen without a
//! synced game-data cache.
//!
//!   cargo test -p gw2-optimizer --test corpus_matching -- --ignored

use std::collections::BTreeMap;

use gw2_core::types::GameMode;
use gw2_optimizer::benchmark::{self, BenchmarkBuild, Scale};
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::picks;
use gw2_optimizer::scenario::{CombatTier, RoleObjective};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/benchmarks");

fn corpus() -> Vec<BenchmarkBuild> {
    let builds = gw2_optimizer::scraper::load_benchmarks_from(std::path::Path::new(FIXTURE));
    assert!(
        builds.len() > 700,
        "the fixture corpus did not load from {FIXTURE} (got {} rows)",
        builds.len()
    );
    builds
}

fn cached_db() -> GameDb {
    let cache = gw2_api::cache::DataCache::new(
        gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir"),
    );
    GameDb::load(&cache).expect("game data cached \u{2014} sync it in-game first")
}

/// The scale chips, as `provider_picks::selected_scale` reads them. PvP has
/// none: conquest is always five a side and the chips are hidden.
fn selected_scale(mode: &GameMode, tier: CombatTier) -> Option<Scale> {
    if matches!(mode, GameMode::PvP) {
        return None;
    }
    Some(match tier {
        CombatTier::Solo => Scale::Solo,
        CombatTier::Party => Scale::Small,
        CombatTier::Squad => Scale::Large,
    })
}

/// One card per source, exactly as `provider_picks` builds them: measure
/// every candidate under the player's own scenario, order by similarity,
/// take each site's best and only if it clears the floor.
fn cards(
    db: &GameDb,
    profession: &str,
    mode: GameMode,
    role: RoleObjective,
    tier: CombatTier,
) -> (Vec<(String, String, f64)>, Vec<String>) {
    let builds = corpus();
    let ctx = gw2_optimizer::balance::BalanceContext::new(mode);
    // The addon's own entry point, so this test cannot pass on a question
    // the panel does not ask. Everything the request needs - the scenario,
    // the objective profile the focus and avoid axes come from, the
    // ordering and its tie-breaks - is decided in there.
    let ranked = picks::rank(
        &builds,
        db,
        &ctx,
        profession,
        tier,
        Some(role),
        &role.to_weights_for(&ctx.game_mode, tier),
        &picks::Kit::default(),
        &|| false,
    );
    let (winners, silent) = ranked.cards();
    let chosen = winners
        .into_iter()
        .map(|(build, pick)| {
            (
                build.source.clone(),
                build.role.clone(),
                pick.alignment.unwrap_or_default(),
            )
        })
        .collect();
    (chosen, silent)
}

/// The six requests that were wrong in-game, frozen by what they measure.
///
/// Each row is (profession, mode, role, tier, expected card roles per source
/// in order). An empty expectation means the honest answer is no card at all
/// and a "nothing close" line instead. The point of the frozen list is that a
/// change to the axes, the weights or the floor has to be looked at rather
/// than absorbed.
#[test]
#[ignore = "needs a synced game-data cache; see dev.cfg"]
fn similar_cards_only() {
    let db = cached_db();
    let mut report = Vec::new();
    let mut failures = Vec::new();
    for (profession, mode, role, tier, expected) in CASES {
        let (chosen, silent) = cards(&db, profession, mode.clone(), *role, *tier);
        let got: Vec<String> = chosen
            .iter()
            .map(|(site, role, _)| format!("{site}:{role}"))
            .collect();
        report.push(format!(
            "{profession} \u{2022} {} \u{2022} {} \u{2022} {}: {} | none: {}",
            mode.label(),
            role.label(),
            tier.label(),
            chosen
                .iter()
                .map(|(site, role, cos)| format!("{site} \"{role}\" {cos:.3}"))
                .collect::<Vec<_>>()
                .join(", "),
            silent.join(", ")
        ));
        for (_, _, aligned) in &chosen {
            assert!(
                *aligned >= gw2_optimizer::scoring::INTENT_ALIGNMENT_FLOOR,
                "a card below the floor: {aligned}"
            );
        }
        let want: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
        if got != want {
            failures.push(format!(
                "{profession} \u{2022} {} \u{2022} {}: wanted {want:?}, got {got:?}",
                mode.label(),
                role.label()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}\n\nmeasured now:\n{}",
        failures.join("\n"),
        report.join("\n")
    );
}

type Case = (
    &'static str,
    GameMode,
    RoleObjective,
    CombatTier,
    &'static [&'static str],
);

const CASES: &[Case] = &[
    // The card that started this: a Support request whose panel offered a
    // Berserker.
    (
        "Warrior",
        GameMode::WvW,
        RoleObjective::Buffer,
        CombatTier::Party,
        &["guildjen:Cloud Support"],
    ),
    (
        "Revenant",
        GameMode::WvW,
        RoleObjective::Buffer,
        CombatTier::Party,
        &["hardstuck:Zerg Support", "guildjen:Havoc Support"],
    ),
    (
        "Mesmer",
        GameMode::PvE,
        RoleObjective::Buffer,
        CombatTier::Party,
        &[],
    ),
    (
        "Elementalist",
        GameMode::WvW,
        RoleObjective::Sustain,
        CombatTier::Solo,
        &["guildjen:Roaming Bruiser", "hardstuck:Roaming Condi DPS"],
    ),
    (
        "Necromancer",
        GameMode::WvW,
        RoleObjective::PowerDps,
        CombatTier::Solo,
        &["hardstuck:Zerg Power DPS", "guildjen:Roaming DPS"],
    ),
    // Re-frozen in sprint 008 (forms), measured: the flow now plays
    // Harbinger Shroud, and while in it the hardstuck Roaming Condi DPS
    // Harbinger's weapon bar (Deathly Swarm, Enfeebling Blood, its chills
    // and immobilizes) is stowed. Its control axis falls from 0.288 to
    // 0.194 and its alignment from 0.353 to 0.286, below hardstuck's
    // Scourge Zerg Support (0.348, no form: Scourge has no shroud). The
    // shroud skills are sourced (Devouring Cut, Voracious Arc with its
    // daze) and the aquatic twins are off the bar; the drop stands.
    (
        "Necromancer",
        GameMode::WvW,
        RoleObjective::Disabler,
        CombatTier::Solo,
        &["guildjen:Roaming Bruiser", "hardstuck:Zerg Support"],
    ),
];

// Part 3: the vocabulary, guarded by the corpus itself.

/// Role strings that name no job, with why. A scale word or a placement is
/// not a job, and guessing one from it would be a confident wrong answer.
const KNOWN_UNJOBBED: &[(&str, &str)] = &[
    ("Group Niche", "a utility slot, not a job — the page says what the niche is, the label does not"),
    ("Raid", "Snowcrows' bare category label; the job is in the page, not the role string"),
    ("Roamer", "a scale word: a roamer is a duellist, a bruiser or an assassin depending on the build"),
    ("Roaming Roamer", "same word twice — the site's own category and its role label agree on the scale and say nothing about the job"),
];

/// Role strings in PvE or WvW that name no scale. PvP is excluded: conquest
/// has no scale to name and the overlay never asks for one there.
const KNOWN_UNSCALED: &[(&str, &str)] = &[];

#[test]
fn every_published_role_word_maps_to_a_job_or_is_allowlisted() {
    let builds = corpus();
    let mut roles: Vec<String> = builds.iter().map(|b| b.role.clone()).collect();
    roles.sort();
    roles.dedup();
    assert!(roles.len() > 40, "only {} distinct roles", roles.len());

    let missing: Vec<&String> = roles
        .iter()
        .filter(|role| {
            benchmark::job_family(role).is_none()
                && !KNOWN_UNJOBBED.iter().any(|(word, _)| word == *role)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "published role words with no job family — add the word to `job_family` or \
         to KNOWN_UNJOBBED with a reason: {missing:?}"
    );

    for (word, _) in KNOWN_UNJOBBED {
        assert!(
            roles.iter().any(|r| r == word),
            "KNOWN_UNJOBBED entry {word:?} is not in the corpus any more — delete it"
        );
    }
}

#[test]
fn every_published_role_word_maps_to_a_scale_or_is_allowlisted() {
    let builds = corpus();
    let mut roles: Vec<String> = builds
        .iter()
        .filter(|b| b.mode != "PvP")
        .map(|b| b.role.clone())
        .collect();
    roles.sort();
    roles.dedup();

    let missing: Vec<&String> = roles
        .iter()
        .filter(|role| {
            benchmark::role_scale(role).is_none()
                && !KNOWN_UNSCALED.iter().any(|(word, _)| word == *role)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "published role words with no scale — add the word to `role_scale` or \
         to KNOWN_UNSCALED with a reason: {missing:?}"
    );

    for (word, _) in KNOWN_UNSCALED {
        assert!(
            roles.iter().any(|r| r == word),
            "KNOWN_UNSCALED entry {word:?} is not in the corpus any more — delete it"
        );
    }
}

/// The tier chips and the scale words have to name the same three sizes.
#[test]
fn every_tier_chip_has_a_scale_word_the_corpus_uses() {
    let builds = corpus();
    for tier in [CombatTier::Solo, CombatTier::Party, CombatTier::Squad] {
        let want = selected_scale(&GameMode::WvW, tier).expect("WvW has scale chips");
        assert!(
            builds
                .iter()
                .any(|b| b.mode == "WvW" && benchmark::role_scale(&b.role) == Some(want)),
            "no WvW row reads as {want:?}, so the {} chip can never match a scale",
            tier.label()
        );
    }
}

// The viability gates, calibrated against the corpus.
//
// These builds are the ground truth: people play them and sites publish
// them, so a blocking gate that refuses one is describing our model, not the
// build. `examples/calibrate_viability.rs` prints the whole picture; this
// budgets it, so a gate change that starts refusing published builds is a
// failing test rather than a number nobody re-ran.

/// Published references grouped by profession and mode.
type Table = BTreeMap<(String, String), Vec<Refusal>>;

/// One published build we could not use, and why: a blocking gate refused it,
/// or it never got that far.
struct Refusal {
    source: String,
    role: String,
    elite: String,
    notes: Vec<String>,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} \"{}\" [{}] — {}",
            self.source,
            self.role,
            if self.elite.is_empty() {
                "no elite"
            } else {
                &self.elite
            },
            self.notes.join("; ")
        )
    }
}

/// What the corpus scored, and who was refused, keyed by profession and mode.
///
/// Each reference is judged under [`benchmark::published_scenario`] — its own
/// mode, job and group size — because the question here is whether the build
/// works at all, not whether it serves some other request.
fn refusals(db: &GameDb) -> (usize, Table, Table) {
    let builds = corpus();
    let weights = gw2_optimizer::scoring::OptimizationWeights::default();
    let mut scored = 0;
    let mut refused: Table = BTreeMap::new();
    let mut unplatable: Table = BTreeMap::new();
    let note = |table: &mut Table, build: &BenchmarkBuild, notes: Vec<String>| {
        table
            .entry((build.profession.clone(), build.mode.clone()))
            .or_default()
            .push(Refusal {
                source: build.source.clone(),
                role: build.role.clone(),
                elite: build.spec_name.clone(),
                notes,
            });
    };

    for build in &builds {
        let Some(plate) = benchmark::plate_from(build, db) else {
            note(
                &mut unplatable,
                build,
                vec!["no three-specialization plate".into()],
            );
            continue;
        };
        let validated =
            gw2_optimizer::validation::validate_gemini_build(&plate, db, &build.profession);
        if !validated.errors.is_empty() {
            let notes = validated.errors.iter().map(|e| e.detail.clone()).collect();
            note(&mut unplatable, build, notes);
            continue;
        }
        let scenario = benchmark::published_scenario(build);
        let ctx = gw2_optimizer::balance::BalanceContext::new(scenario.game_mode.clone());
        let report = gw2_optimizer::referee::evaluate_validated_build_ranked(
            &validated,
            db,
            &build.profession,
            &weights,
            &ctx,
            &scenario,
        );
        scored += 1;
        if report.viability.is_viable {
            continue;
        }
        note(
            &mut refused,
            build,
            report
                .viability
                .gates
                .iter()
                .filter(|g| !g.passed && g.gate.blocks())
                .map(|g| format!("{:?}: {}", g.gate, g.note))
                .collect(),
        );
    }
    (scored, refused, unplatable)
}

/// How many published builds our blocking gates are allowed to refuse, per
/// profession and mode, and why that many is understood rather than tolerated.
///
/// A budget is a ceiling with a floor: over it and a gate started refusing
/// builds people demonstrably play, under it by more than one and the budget
/// is stale — the gate improved and the number has to come down with it, so
/// the table ratchets instead of drifting.
const EXPECTED_REFUSALS: &[(&str, &str, usize, &str)] = &[
    ("Elementalist", "PvE", 6, "light-armour raid and open-world DPS sit under the 11000 effective-health floor, which is a WvW number applied to a golem fight"),
    ("Elementalist", "PvP", 4, "three published Tempest support bars take no stunbreak utility at all, and one Evoker duelist publishes no cleanse"),
    ("Elementalist", "WvW", 4, "the same stunbreak-less Tempest support bars, one light-armour roamer under the 15000 solo floor, one condi build the simulator never lets recover"),
    ("Engineer", "PvP", 5, "published bars carry no stunbreak utility, which then also empties the soft-control gate that reads off it; one more since PvP scenarios resolve their data profile instead of the hardcoded floors"),
    ("Engineer", "WvW", 4, "same stunbreak-less bars; Scrapper takes its stability from the gyro toolbelt, which the timeline does not model as cover"),
    ("Guardian", "PvP", 1, "one Dragonhunter measures 1.6 cleanses/20s against the PvP profile's 2.0 floor, which now applies where a hardcoded floor used to"),
    ("Mesmer", "PvP", 2, "Troubadour support measures 3.9 cleanses/20s against a 4.0 floor - one tenth short, not a broken build - plus one Chrono bar that publishes no cleanse"),
    ("Mesmer", "WvW", 3, "light-armour roamers under the 15000 solo health floor; the clone economy no longer reads as starved now that generation is keyed to the API's Clone and Phantasm categories"),
    ("Necromancer", "WvW", 1, "one Reaper zerg bar with neither stunbreak nor cleanse, which cascades into the control-coverage gate"),
    ("Ranger", "WvW", 2, "Soulbeast and Galeshot roamers under the 15000 health floor; the Druid Celestial-Avatar stability cases cleared once the avatar bar joined the kit (sprint 008 forms)"),
    ("Revenant", "PvP", 3, "Herald publishes no stunbreak utility because its stunbreak is a legend swap, which the bar-only gates cannot see"),
    ("Revenant", "WvW", 4, "Herald and Vindicator bars whose stunbreak is the legend swap; the energy economy itself no longer refuses them now that the swap refills the pool, upkeep bends the regen rate, and a stunbreak scan counts one decision instead of one per candidate"),
    ("Thief", "PvE", 8, "every Thief in the game has 10249 effective health against an 11000 floor - the floor is wrong for PvE Thief, the builds are not"),
    ("Thief", "WvW", 1, "the same 10249, here against the 13000 havoc floor"),
    ("Warrior", "WvW", 4, "zerg and havoc power Warriors publish no cleanse at all because cleanse comes from the party, which a single-build gate cannot see"),
];

/// A measured table against its budget, both ways.
///
/// Over the budget and something started rejecting builds people play; under
/// it by more than one and the budget is stale, so it ratchets down as the
/// parsing and the gates improve instead of drifting upward unnoticed.
fn against_budget(
    measured: &Table,
    expected: &[(&str, &str, usize, &str)],
    noun: &str,
) -> Vec<String> {
    let budget = |profession: &str, mode: &str| {
        expected
            .iter()
            .find(|(p, m, _, _)| *p == profession && *m == mode)
            .map(|(_, _, n, _)| *n)
            .unwrap_or(0)
    };
    let mut failures = Vec::new();
    for ((profession, mode), list) in measured {
        let allowed = budget(profession, mode);
        if list.len() > allowed {
            failures.push(format!(
                "{profession} · {mode}: {} {noun}, {allowed} budgeted
{}",
                list.len(),
                list.iter()
                    .map(|r| format!("      {r}"))
                    .collect::<Vec<_>>()
                    .join(
                        "
"
                    )
            ));
        }
    }
    for (profession, mode, allowed, _) in expected {
        let actual = measured
            .get(&((*profession).to_string(), (*mode).to_string()))
            .map_or(0, Vec::len);
        if *allowed > actual + 1 {
            failures.push(format!(
                "{profession} · {mode}: budget {allowed} but only {actual} {noun} — ratchet the budget down"
            ));
        }
    }
    failures
}

#[test]
#[ignore = "needs a synced game-data cache; see dev.cfg"]
fn no_published_build_is_refused_beyond_its_budget() {
    let cache = gw2_api::cache::DataCache::new(
        gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir"),
    );
    let db = GameDb::load(&cache).expect("game data cached — sync it in-game first");
    let (scored, refused, _) = refusals(&db);
    assert!(
        scored > 400,
        "only {scored} references plated and validated"
    );
    let failures = against_budget(&refused, EXPECTED_REFUSALS, "refused");
    assert!(
        failures.is_empty(),
        "{scored} references scored\n{}\n\nmeasured now:\n{}",
        failures.join("\n"),
        measured_table(&refused)
    );
}

/// The measured numbers in the table's own shape, so a failing run can be
/// pasted into `EXPECTED_REFUSALS` after the refusals are understood.
fn measured_table(table: &Table) -> String {
    table
        .iter()
        .map(|((p, m), list)| format!("    (\"{p}\", \"{m}\", {}, \"\"),", list.len()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_refusal_budget_table_is_well_formed() {
    well_formed(EXPECTED_REFUSALS, "EXPECTED_REFUSALS");
}

/// A budget table without a game-data cache: one row per profession and mode,
/// every row naming a real one and carrying a reason.
fn well_formed(table: &[(&str, &str, usize, &str)], name: &str) {
    let builds = corpus();
    let real: Vec<(String, String)> = {
        let mut v: Vec<(String, String)> = builds
            .iter()
            .map(|b| (b.profession.clone(), b.mode.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for (profession, mode, allowed, reason) in table {
        assert!(
            real.iter().any(|(p, m)| p == profession && m == mode),
            "{name} names {profession} · {mode}, which the corpus does not have"
        );
        assert!(
            *allowed > 0,
            "{name}: {profession} · {mode} budgets zero — delete the row instead"
        );
        assert!(
            !reason.trim().is_empty(),
            "{name}: {profession} · {mode} budgets {allowed} with no reason"
        );
        assert!(
            !seen.contains(&(profession, mode)),
            "{name}: {profession} · {mode} appears twice"
        );
        seen.push((profession, mode));
    }
}

/// How many published references we cannot turn into a plate at all, per
/// profession and mode, and whose fault each group is.
///
/// Distinct from [`EXPECTED_REFUSALS`]: those are builds we understood and
/// our gates then refused. These never got that far — the page did not
/// publish enough, or we did not read what it published. Same ratchet, so a
/// scraper or validator regression cannot grow this number quietly.
const EXPECTED_UNPLATABLE: &[(&str, &str, usize, &str)] = &[
    ("Elementalist", "PvE", 1, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Elementalist", "PvP", 1, "site markup: a rune or a relic published in a sigil seat, so the sigil lookup is handed a rune name"),
    ("Engineer", "PvE", 4, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Engineer", "WvW", 1, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Guardian", "PvE", 3, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Guardian", "WvW", 1, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Mesmer", "PvE", 2, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Necromancer", "PvE", 5, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
    ("Necromancer", "WvW", 2, "site markup: a rune or a relic published in a sigil seat, so the sigil lookup is handed a rune name"),
    ("Thief", "WvW", 1, "site markup: a rune or a relic published in a sigil seat, so the sigil lookup is handed a rune name"),
    ("Warrior", "PvE", 1, "site data: the page publishes fewer than heal, three utilities and an elite, usually a chat code whose palette ids do not all resolve"),
];

#[test]
#[ignore = "needs a synced game-data cache; see dev.cfg"]
fn no_published_reference_becomes_unplatable_beyond_its_budget() {
    let cache = gw2_api::cache::DataCache::new(
        gw2_api::dev_config::cache_dir().expect("dev.cfg with addons_dir"),
    );
    let db = GameDb::load(&cache).expect("game data cached — sync it in-game first");
    let (scored, _, unplatable) = refusals(&db);
    let total: usize = unplatable.values().map(Vec::len).sum();
    let failures = against_budget(&unplatable, EXPECTED_UNPLATABLE, "unplatable");
    assert!(
        failures.is_empty(),
        "{scored} references scored, {total} unplatable
{}

measured now:
{}",
        failures.join(
            "
"
        ),
        measured_table(&unplatable)
    );
}

#[test]
fn the_unplatable_budget_table_is_well_formed() {
    well_formed(EXPECTED_UNPLATABLE, "EXPECTED_UNPLATABLE");
}
