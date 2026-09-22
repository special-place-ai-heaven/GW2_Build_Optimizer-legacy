//! Benchmark build type — normalized reference build data from community sources.
//!
//! Scraped from Snowcrows (PvE), Hardstuck (general), and GuildJen (WvW/PvP).
//! Stored as JSON files in `{addon_dir}/benchmarks/`.

use serde::{Deserialize, Serialize};

use crate::picks;
use crate::providers::ProviderBuild;
use crate::scoring::OptimizationWeights;

/// A single normalized reference build from a community build site.
///
/// `Default` is derived so adding a field is a one-line diff at every
/// construction site instead of a compile error at each one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchmarkBuild {
    /// Source site: "snowcrows", "hardstuck", or "guildjen".
    pub source: String,
    /// Profession name (e.g. "Guardian", "Necromancer").
    pub profession: String,
    /// Elite spec name (e.g. "Firebrand", "Scourge").
    pub spec_name: String,
    /// Game mode: "PvE", "WvW", "PvP".
    pub mode: String,
    /// Role description (e.g. "Power DPS", "Condi DPS", "Heal Support").
    pub role: String,
    /// GW2 build template code if found.
    pub build_code: Option<String>,
    /// The stat prefix most of the gear uses (e.g. "Berserker's", "Viper's").
    ///
    /// A reduction, not the whole truth — most builds mix prefixes, and the
    /// per-slot detail is on [`BenchmarkBuild::published`]. This one string
    /// is what the scorer compares against.
    pub gear_prefix: String,
    /// Page URL this was scraped from.
    pub source_url: String,
    /// ISO date when this was scraped (e.g. "2026-03-30").
    pub scraped_at: String,
    /// What the page published, in GW2 API ids: gear, upgrades, traits and
    /// the skill bar.
    ///
    /// This replaced six name fields — `rune`, `sigils`, `relic`, `traits`,
    /// `skills` and `notes` — which no code ever read and which could not be
    /// filled honestly: none of the three sites writes gear names into its
    /// markup, so the text extractors returned empty strings 570 to 739
    /// times out of 739, and where they did return something it was page
    /// furniture. 431 of 739 rows recorded the same three elite specs
    /// whatever the profession, taken from a navigation menu.
    ///
    /// Ids do not have that failure mode: they need no fuzzy matching, they
    /// do not move with the site's wording, and they are the numbers
    /// `GameDb` is already keyed on — so a name is a read-time lookup, not a
    /// scrape-time guess.
    #[serde(skip_serializing_if = "ProviderBuild::is_empty")]
    pub published: ProviderBuild,
}

/// Score delta between the optimizer's result and a community reference build.
#[derive(Debug, Clone)]
pub struct BenchmarkDelta {
    /// Source site the reference came from.
    pub source: String,
    /// Profession name of the reference.
    pub profession: String,
    /// Role of the reference (e.g. "Power DPS").
    pub role: String,
    /// The role the caller asked for, which is not always one any site
    /// publishes for this profession.
    pub requested_role: String,
    /// Whether [`BenchmarkDelta::role`] shares any word with
    /// [`BenchmarkDelta::requested_role`].
    ///
    /// False means the closest published build does a different job, so the
    /// percentage is a fair score under the player's weights but is NOT a
    /// comparison against a build meant for the same job - and the UI has to
    /// say which it is showing.
    pub role_matched: bool,
    /// Gear prefix of the reference build.
    pub ref_gear_prefix: String,
    /// The reference build's `RefereeReport::ranked_direction_score`, produced
    /// by the same referee, weights, balance context and scenario as
    /// `our_score`.
    pub ref_score: f64,
    /// Our build's `RefereeReport::ranked_direction_score`.
    pub our_score: f64,
    /// Whether the reference passed every blocking gate at the player's scale.
    ///
    /// A published page is written for its own scale and fight, so `false` is
    /// common and does not invalidate `ref_score` — both sides are measured
    /// output either way. The UI says so rather than hiding the comparison.
    pub ref_viable: bool,
    /// Whether our own build passed every blocking gate.
    pub our_viable: bool,
    /// `our_score / ref_score` as a percentage (100 = on-par, >100 = better).
    pub pct_of_ref: f64,
    /// URL of the reference page.
    pub ref_url: String,
}

/// Result from scraping one source site.
#[derive(Debug, Clone)]
pub struct ScrapeResult {
    /// Source identifier.
    pub source: String,
    /// Successfully parsed builds.
    pub builds: Vec<BenchmarkBuild>,
    /// Error message if the scrape failed or partially failed.
    pub error: Option<String>,
    /// Pages that were listed but produced no build.
    ///
    /// Separate from `error`, which is about the source as a whole. A run
    /// can list 157 builds, return 148 and be entirely healthy apart from
    /// nine pages whose layout the extractor did not recognise - and saying
    /// "done 148" alone hides that nine went missing.
    pub failed: usize,
}

// Matching

/// Find the best-matching benchmark build for a given profession, mode, and role hint.
///
/// Matching priority:
/// 1. Profession (case-insensitive contains match)
/// 2. Mode (exact, case-insensitive)
/// 3. Role hint similarity (word overlap score)
///
/// Returns `None` if no builds match the profession+mode criteria.
pub fn find_best_benchmark<'a>(
    builds: &'a [BenchmarkBuild],
    profession: &str,
    mode: &str,
    role_hint: &str,
) -> Option<&'a BenchmarkBuild> {
    ranked_benchmarks(builds, profession, mode, role_hint)
        .into_iter()
        .next()
}

/// Every profession+mode match, closest role first.
///
/// Same criteria as [`find_best_benchmark`], but the whole list: a caller
/// that can reject a candidate (no published ids, does not validate, scores
/// nothing) needs the next one rather than nothing.
fn ranked_benchmarks<'a>(
    builds: &'a [BenchmarkBuild],
    profession: &str,
    mode: &str,
    role_hint: &str,
) -> Vec<&'a BenchmarkBuild> {
    let prof_lower = profession.to_lowercase();
    let mode_lower = mode.to_lowercase();
    let role_lower = role_hint.to_lowercase();

    let mut candidates: Vec<&BenchmarkBuild> = builds
        .iter()
        .filter(|b| {
            b.profession.to_lowercase().contains(&prof_lower) && b.mode.to_lowercase() == mode_lower
        })
        .collect();

    // `max_by_key` returns the LAST maximum; a stable descending sort returns
    // the first. Reversing before sorting keeps the head of this list the
    // build `find_best_benchmark` has always picked.
    candidates.reverse();
    candidates
        .sort_by_key(|b| std::cmp::Reverse(role_similarity(&b.role.to_lowercase(), &role_lower)));
    candidates
}

/// The coarse job a role name describes, when it describes one.
///
/// Not a taxonomy of the eleven words the three sites use between them —
/// only the distinction that decides whether a build is the WRONG ANSWER
/// rather than a worse one. A healer and a DPS are not near neighbours; they
/// are opposite jobs, and offering one in place of the other is not a
/// suggestion, it is a wrong answer with a card around it.
///
/// Order matters and is the whole trick. "Zerg Boon DPS" is a DPS that
/// happens to give boons, so damage is tested before support; "Offensive
/// Support" is a support that happens to do damage, so `heal` is tested
/// before both. `None` means the words say nothing either way and nothing is
/// ruled out on their account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobFamily {
    /// Takes the fight away from the other side rather than killing it:
    /// disables, boon strips, immobilises. Nothing the three sites publish
    /// is labelled this way today, which is exactly why `narrow` exists -
    /// the request has the word, the corpus does not.
    Disabler,
    /// Keeps other people alive: healing, cleansing, boons, stability.
    Support,
    /// Sustained damage — the thing that dies to it dies over a fight.
    Damage,
    /// Burst. Not a Damage build with better numbers: it opens with a
    /// disable and spends everything inside a window of a few seconds, and
    /// it is a glass cannon that must disengage if the window closes with
    /// the target alive. Asking for one and being handed a zerg DPS is the
    /// wrong build, not a lesser one.
    Assassin,
    /// Wins one fight at a time — medium damage, medium sustain, cleanse
    /// and control.
    Duelist,
    /// Stands in it. Front line, outnumbered, still alive.
    Bruiser,
}

/// See [`JobFamily`].
pub fn job_family(role: &str) -> Option<JobFamily> {
    let role = role.to_lowercase();
    let has = |word: &str| role.contains(word);
    // Order is the whole trick. The specific words go first, because the
    // generic ones appear inside them: "Roaming Assassin" is an assassin
    // before it is anything else, and "Offensive Support" is a support even
    // though it does damage.
    if has("disab") || has("strip") || has("boonrip") || has("control") || has(" cc") {
        return Some(JobFamily::Disabler);
    }
    if has("heal") || has("medic") {
        return Some(JobFamily::Support);
    }
    if has("assassin") {
        return Some(JobFamily::Assassin);
    }
    if has("duelist") {
        return Some(JobFamily::Duelist);
    }
    // A PvP sidenoder holds a point alone against whoever walks onto it —
    // that is the duelist's job under another name.
    if has("sidenoder") {
        return Some(JobFamily::Duelist);
    }
    if has("dps") || has("damage") || has("hybrid") {
        return Some(JobFamily::Damage);
    }
    if has("support") {
        return Some(JobFamily::Support);
    }
    if has("bruiser") || has("tank") || has("troll") {
        return Some(JobFamily::Bruiser);
    }
    None
}

/// Power or condition, when the role name says.
///
/// Cross-referenced as a disqualifier, not a preference. A condition build
/// is not a power build with different numbers — different stats, different
/// runes, different sigils, different traits, and a different way of
/// killing something. Offered in place of one another they are simply the
/// wrong build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Power,
    Condi,
    /// Says it wants both, so neither rules it out.
    Hybrid,
}

/// What a stat prefix is FOR, read off the attributes it grants.
///
/// The role name is a label somebody typed; the prefix is what the build
/// wears. GuildJen publishes a Reaper as "Roaming DPS" and says nothing
/// about power or condition — but it is Marauder, and Marauder grants Power,
/// Precision, Ferocity and Vitality, so the build is a power build whatever
/// the label omits. Matching on the label alone offered that build to
/// someone asking for condition damage.
///
/// This answers "power or condition", not "what job is this". Celestial
/// grants both and therefore reads as Hybrid here — but nobody wears
/// Celestial to be a damage build; it is a bruiser's prefix, moderate at
/// everything and excellent at nothing. That judgement belongs to
/// [`prefix_job`], which is about the job, not the damage type.
pub fn prefix_flavour(prefix: &str, db: &crate::gamedb::GameDb) -> Option<Flavour> {
    if prefix.trim().is_empty() {
        return None;
    }
    let wanted = prefix.trim().trim_end_matches("'s").to_lowercase();
    let stat = db.itemstats.values().find(|stat| {
        let name = stat.name.trim().trim_end_matches("'s").to_lowercase();
        name == wanted
    })?;

    let grants = |attribute: &str| {
        stat.attributes
            .iter()
            .any(|a| a.attribute.eq_ignore_ascii_case(attribute) && a.value + 1 > 0)
    };
    let condi = grants("ConditionDamage") || grants("ConditionDuration");
    let power = grants("Power") || grants("CritDamage");
    match (power, condi) {
        // Everything at once is the definition of Celestial, and of Hybrid.
        (true, true) => Some(Flavour::Hybrid),
        (true, false) => Some(Flavour::Power),
        (false, true) => Some(Flavour::Condi),
        // Minstrel's and Harrier's grant neither: they are not damage
        // prefixes at all, and have no flavour to disagree about.
        (false, false) => None,
    }
}

/// The job a stat prefix is dressed for, when the role name did not say.
///
/// Gear is a statement of intent. Nobody wears Minstrel's to deal damage or
/// Berserker's to hold a door, so where a site publishes a build as nothing
/// more specific than "Roamer" or "DPS", the prefix still says what it is
/// for.
///
/// Deliberately only three answers and only from the extremes, because this
/// is a fallback for silence and a confident wrong guess is worse than none:
///
/// - grants no offence at all but grants healing → Support (Minstrel's,
///   Harrier's)
/// - grants everything → Bruiser. Celestial is moderate at all nine
///   attributes, which is a bruiser's shape: enough damage to threaten,
///   enough sustain to stay. It is not a damage prefix.
/// - grants offence and no defence → Damage (Berserker's, Viper's)
///
/// Anything in between — Marauder, Trailblazer's, Demolisher — says both
/// things and is left to the label.
pub fn prefix_job(prefix: &str, db: &crate::gamedb::GameDb) -> Option<JobFamily> {
    if prefix.trim().is_empty() {
        return None;
    }
    let wanted = prefix.trim().trim_end_matches("'s").to_lowercase();
    let stat = db.itemstats.values().find(|stat| {
        let name = stat.name.trim().trim_end_matches("'s").to_lowercase();
        name == wanted
    })?;
    let grants = |attribute: &str| {
        stat.attributes
            .iter()
            .any(|a| a.attribute.eq_ignore_ascii_case(attribute) && a.value + 1 > 0)
    };
    let offence = grants("Power") || grants("ConditionDamage") || grants("CritDamage");
    let defence = grants("Toughness") || grants("Vitality");
    let healing = grants("Healing") || grants("BoonDuration");

    match (offence, defence, healing) {
        (false, _, true) => Some(JobFamily::Support),
        // Everything at once: Celestial.
        (true, true, true) => Some(JobFamily::Bruiser),
        (true, false, false) => Some(JobFamily::Damage),
        _ => None,
    }
}

/// See [`Flavour`]. `None` where the words do not say.
pub fn damage_flavour(role: &str) -> Option<Flavour> {
    let role = role.to_lowercase();
    if role.contains("hybrid") {
        return Some(Flavour::Hybrid);
    }
    if role.contains("condi") {
        return Some(Flavour::Condi);
    }
    if role.contains("power") {
        return Some(Flavour::Power);
    }
    None
}

/// How many people are around, as the sites name it.
///
/// The player put it plainly: the smaller the group, the more self-sufficient
/// a build has to be, because a large group covers for it with overlapping
/// boons and a lone one has nobody. A zerg healer dropped into a roaming
/// fight dies to the first assassin that looks at it. So scale is not
/// decoration on a role name; it changes what the job IS.
///
/// It disqualifies, but ASYMMETRICALLY, because self-reliance only runs one
/// way. A roaming build can walk into a zerg — it carries its own sustain
/// and never knew who it would meet, and roamers adapt by swapping utility
/// skills rather than rebuilding. A zerg build cannot go roaming: it leans
/// on twenty people's overlapping boons, and alone it dies to the first
/// assassin that looks at it.
///
/// So a candidate is allowed when it is at least as self-reliant as the
/// scale asked for — see [`Scale::self_reliance`]. Where that leaves
/// nothing, nothing is offered: the sites have simply not published that
/// build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Solo,
    Small,
    Large,
}

impl Scale {
    /// How much a build made for this scale has to carry alone, smallest
    /// group first. A build may always be offered to a LARGER group than it
    /// was written for, never to a smaller one.
    pub fn self_reliance(self) -> u8 {
        match self {
            Scale::Solo => 0,
            Scale::Small => 1,
            Scale::Large => 2,
        }
    }
}

/// See [`Scale`]. `None` where the words do not say.
pub fn role_scale(role: &str) -> Option<Scale> {
    let role = role.to_lowercase();
    let has = |word: &str| role.contains(word);
    if has("roam") || has("solo") || has("duel") || has("open world") {
        return Some(Scale::Solo);
    }
    if has("havoc") || has("party") || has("group") || has("fractal") {
        return Some(Scale::Small);
    }
    if has("zerg") || has("cloud") || has("squad") || has("raid") || has("strike") {
        return Some(Scale::Large);
    }
    None
}

/// The scenario a published build was written for, as near as its own labels
/// say: its mode, the job its role words name, and the group size they name.
///
/// Reading a reference under the player's scenario answers "how well does
/// this serve what I asked for". Reading it under its OWN scenario answers
/// "does this build work at all", which is the question the viability gates
/// are calibrated against — a zerg healer judged as a solo roamer is refused
/// for not being self-reliant, which says nothing about the build.
///
/// The word lists are deliberately its own rather than [`role_scale`]'s:
/// this one has no `None` to fall back on, so it defaults to Solo and to
/// `StrikeSpike` where the label says nothing, and the two would diverge if
/// either moved to serve the other.
pub fn published_scenario(build: &BenchmarkBuild) -> crate::scenario::ScenarioSpec {
    use crate::scenario::{CombatTier, RoleObjective, ScenarioSpec};

    let mode = match build.mode.as_str() {
        "WvW" => gw2_core::types::GameMode::WvW,
        "PvP" => gw2_core::types::GameMode::PvP,
        _ => gw2_core::types::GameMode::PvE,
    };
    let role = build.role.to_lowercase();
    let has = |word: &str| role.contains(word);
    // The site's own words, as a role chip. Word lists deliberately its own
    // rather than the picks path's: this one has no `None` to fall back on.
    let objective = if has("heal") || has("medic") {
        RoleObjective::Healer
    } else if has("support") || has("boon") {
        RoleObjective::Buffer
    } else if has("commander") {
        RoleObjective::Tank
    } else if has("disable") || has("boonstrip") {
        RoleObjective::Disabler
    } else if has("bruiser") || has("tank") {
        RoleObjective::Sustain
    } else if has("roam") || has("duel") || has("assassin") {
        RoleObjective::WvWRoamer
    } else if has("condi") {
        RoleObjective::CondiDps
    } else {
        RoleObjective::PowerDps
    };
    let tier = if has("zerg") || has("cloud") || has("raid") {
        CombatTier::Squad
    } else if has("havoc") || has("party") || has("fractal") {
        CombatTier::Party
    } else {
        CombatTier::Solo
    };
    // The same constructor the addon and the picks path use, so a reference
    // judged on its own terms is judged the way a request is - including
    // the objective profile id, without which the referee falls back to the
    // mode default and measures every build as a DPS.
    let ctx = crate::balance::BalanceContext::new(mode.clone());
    let weights = objective.to_weights_for(&mode, tier);
    ScenarioSpec::for_request(&ctx, tier, Some(objective), &weights)
}

/// Compute a simple word-overlap similarity score between two role strings.
fn role_similarity(a: &str, b: &str) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let words_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = b.split_whitespace().collect();
    words_a.intersection(&words_b).count()
}

// Reference scoring

/// A published build as a plate, in the same shape the model answers in.
///
/// Lives here rather than in a calibration example because the "vs meta"
/// meter needs it too: a reference build is only comparable to ours once it
/// has been through the same validator and the same referee.
pub fn plate_from(
    build: &BenchmarkBuild,
    db: &crate::gamedb::GameDb,
) -> Option<crate::prompts::GeminiBuildResponse> {
    let p = &build.published;
    if p.specs.is_empty() {
        return None;
    }
    let specializations: Vec<(String, Vec<String>)> = p
        .specs
        .iter()
        .filter_map(|line| {
            let spec = db.specializations.get(&line.id)?;
            let traits: Vec<String> = line
                .trait_ids
                .iter()
                .filter_map(|id| db.traits.get(id).map(|t| t.name.clone()))
                .collect();
            Some((spec.name.clone(), traits))
        })
        .collect();
    if specializations.len() != 3 {
        return None;
    }
    let name = |id: Option<u32>| {
        id.and_then(|id| db.items.get(&id))
            .map(|i| i.name.clone())
            .unwrap_or_default()
    };
    const WEAPONS: [&str; 17] = [
        "axe",
        "dagger",
        "mace",
        "pistol",
        "scepter",
        "sword",
        "focus",
        "shield",
        "torch",
        "warhorn",
        "greatsword",
        "hammer",
        "longbow",
        "rifle",
        "shortbow",
        "staff",
        "spear",
    ];
    let weapons: Vec<String> = p
        .gear
        .iter()
        .map(|g| g.slot.clone())
        .filter(|s| WEAPONS.contains(&s.to_lowercase().as_str()))
        .collect();
    // Heal, three utilities and elite. Sites vary on whether they mark these
    // up at all - GuildJen mostly does not - but nearly every page publishes
    // a chat code, and the code carries them as palette ids. Weapons decide
    // skills 1-5 and are resolved from the profession; 6-0 are chosen, and
    // this is where the choice is written down.
    let skills = published_skills(p, db);
    Some(crate::prompts::GeminiBuildResponse {
        specializations,
        weapons,
        skills,
        rune: name(p.rune_id),
        sigils: p
            .sigil_ids
            .iter()
            .filter_map(|id| db.items.get(id).map(|i| i.name.clone()))
            .collect(),
        relic: name(p.relic_id),
        stat_prefix: p
            .dominant_stat()
            .unwrap_or_else(|| build.gear_prefix.clone()),
        ..Default::default()
    })
}

/// The slot bar, labelled the way a plate labels it.
///
/// `validation::parse_skill_names_from_response` reads `Heal: `, `Utils: `
/// and `Elite: ` prefixes rather than a bare list, so this has to speak the
/// same shape. Where the ids come from is `ProviderBuild::slot_skills`.
fn published_skills(
    p: &crate::providers::ProviderBuild,
    db: &crate::gamedb::GameDb,
) -> Vec<String> {
    let slots = p.slot_skills(db);
    let name = |slot: Option<&Option<u32>>| {
        slot.and_then(|s| *s)
            .and_then(|id| db.skills.get(&id))
            .map(|s| s.name.clone())
    };
    let mut lines = Vec::new();
    if let Some(heal) = name(slots.first()) {
        lines.push(format!("Heal: {heal}"));
    }
    let utils: Vec<String> = slots
        .iter()
        .skip(1)
        .take(3)
        .filter_map(|slot| name(Some(slot)))
        .collect();
    if !utils.is_empty() {
        lines.push(format!("Utils: {}", utils.join(", ")));
    }
    if let Some(elite) = name(slots.get(4)) {
        lines.push(format!("Elite: {elite}"));
    }
    lines
}

/// Our score as a percentage of theirs, capped at 200% so a reference that
/// barely scores cannot print an absurd number.
fn pct_of(ours: f64, theirs: f64) -> f64 {
    (ours / theirs * 100.0).clamp(0.0, 200.0)
}

/// Compute a `BenchmarkDelta` comparing the optimizer's scored result to the
/// closest community reference build.
///
/// Both sides are `RefereeReport::ranked_direction_score` under the SAME
/// `weights`, `ctx` and `scenario` the optimized build was scored with, so
/// the percentage means what a player reads it as. Uncapped, rather than
/// `user_intent_score`: per-axis saturation flattens two builds that are far
/// apart into the same number, and a ratio of two flattened numbers says
/// nothing. `ranked_direction_score` rather than `raw_direction_score`
/// because that one collapses to the -1.0 non-viability sentinel, and ~42% of
/// synced WvW references fail a gate at the player's scale — the meter used
/// to silently skip past them to a worse reference, or report none at all.
/// Gate results are carried on `ref_viable`/`our_viable` instead.
///
/// The reference is chosen by the SAME rule the cards are - see
/// [`crate::picks`]: every synced row with published ids is refereed under
/// the player's own scenario, and the one whose measured axes sit nearest
/// the player's weights wins, provided it clears
/// [`crate::scoring::INTENT_ALIGNMENT_FLOOR`]. Role words decide nothing. `None` when our
/// own score is not positive, or when nothing published measures near what
/// was asked for - the UI already says so.
#[allow(clippy::too_many_arguments)]
pub fn compute_benchmark_delta(
    builds: &[BenchmarkBuild],
    profession: &str,
    mode: &str,
    role_hint: &str,
    weights: &OptimizationWeights,
    our_score: f64,
    our_viable: bool,
    db: &crate::gamedb::GameDb,
    ctx: &crate::balance::BalanceContext,
    scenario: &crate::scenario::ScenarioSpec,
) -> Option<BenchmarkDelta> {
    // A build the simulator could not drive at all has no output to compare.
    if !our_score.is_finite() || our_score <= 0.0 {
        return None;
    }
    let candidates = picks::candidates(builds, profession, mode);
    // No kit: the meter compares against our own result, which this
    // function is not handed, so the kit tie-break has nothing to compare
    // and stays inert. The measured direction decides on its own.
    let (mut evaluated, _) = picks::evaluate(
        &candidates,
        db,
        weights,
        ctx,
        scenario,
        &picks::Kit::default(),
        &|| false,
    );
    picks::order_picks(
        &mut evaluated,
        Some(picks::StatedScale::from_tier(scenario.combat_tier)),
        None,
    );
    let best = evaluated.into_iter().find(|p| {
        p.alignment
            .is_some_and(|a| a >= crate::scoring::INTENT_ALIGNMENT_FLOOR)
    })?;
    let reference = candidates[best.index];
    let ref_report = best.report.as_ref()?;
    let ref_score = ref_report.ranked_direction_score;
    // A reference that produced NOTHING measurable is no yardstick; dividing
    // by zero is not a percentage. A failed gate is reported, not hidden.
    if !ref_score.is_finite() || ref_score <= 0.0 {
        return None;
    }
    Some(BenchmarkDelta {
        source: reference.source.clone(),
        profession: reference.profession.clone(),
        role: reference.role.clone(),
        requested_role: role_hint.to_string(),
        role_matched: role_hint.trim().is_empty()
            || role_similarity(&role_hint.to_lowercase(), &reference.role.to_lowercase()) > 0,
        ref_gear_prefix: reference.gear_prefix.clone(),
        ref_score,
        our_score,
        ref_viable: ref_report.viability.is_viable,
        our_viable,
        pct_of_ref: pct_of(our_score, ref_score),
        ref_url: reference.source_url.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_build(profession: &str, mode: &str, role: &str, gear: &str) -> BenchmarkBuild {
        BenchmarkBuild {
            source: "test".into(),
            profession: profession.into(),
            spec_name: String::new(),
            mode: mode.into(),
            role: role.into(),
            build_code: None,
            gear_prefix: gear.into(),
            source_url: "https://example.com".into(),
            scraped_at: "2026-01-01".into(),
            ..Default::default()
        }
    }

    /// A row exactly as the store held it before ids existed, verbatim from
    /// `guildjen_elementalist_pve.json` on 2026-09-05 — six fields that no
    /// longer exist among them.
    ///
    /// This test is the guard on the one mistake that would be invisible.
    /// Both load paths swallow a parse error (`if let Ok(v)` in
    /// `load_benchmarks`, `let Ok(..) else { continue }` in
    /// `load_todays_builds`), so a field added without `#[serde(default)]`
    /// would drop all 739 rows at once with no log line: the Settings tab
    /// would read "never synced", the Improve tab would show "no benchmark
    /// data", and the next sync would refetch every page.
    #[test]
    fn a_row_written_before_ids_existed_still_loads() {
        const LEGACY: &str = r#"{
            "source": "guildjen",
            "profession": "Elementalist",
            "spec_name": "Evoker",
            "mode": "PvE",
            "role": "WvW Roaming",
            "build_code": "[&BPcAAAA=]",
            "gear_prefix": "Plaguedoctor's",
            "rune": "",
            "sigils": [],
            "relic": "",
            "traits": ["Firebrand", "Willbender", "Dragonhunter"],
            "skills": [],
            "source_url": "https://guildjen.com/heal-dps-evoker-build/",
            "scraped_at": "2026-09-05",
            "notes": ""
        }"#;

        let build: BenchmarkBuild = serde_json::from_str(LEGACY).expect("a legacy row must load");
        // The fields every reader actually touches survive untouched.
        assert_eq!(build.profession, "Elementalist");
        assert_eq!(build.mode, "PvE");
        assert_eq!(build.gear_prefix, "Plaguedoctor's");
        assert_eq!(
            build.source_url,
            "https://guildjen.com/heal-dps-evoker-build/"
        );
        // The six deleted fields are ignored rather than fatal, and the row
        // reports honestly that it carries no ids.
        assert!(
            build.published.is_empty(),
            "a pre-ids row must not pretend to have published any"
        );

        // A whole file of them, which is what load_benchmarks actually reads.
        let file = format!("[{LEGACY},{LEGACY}]");
        let rows: Vec<BenchmarkBuild> = serde_json::from_str(&file).expect("a legacy file");
        assert_eq!(rows.len(), 2);
    }

    /// Rolling back to an older build must not brick the store either: there
    /// is no `deny_unknown_fields`, so a row carrying ids loads anywhere.
    #[test]
    fn a_row_carrying_ids_round_trips() {
        let mut build = make_build("Necromancer", "PvE", "Condi DPS", "Viper's");
        build.published.rune_id = Some(24762);
        build.published.sigil_ids = vec![44944, 24560];
        build.published.specs = vec![crate::providers::SpecLine {
            id: 39,
            trait_ids: vec![815, 816, 801],
        }];

        let json = serde_json::to_string(&build).expect("serialise");
        let back: BenchmarkBuild = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back.published, build.published);
        assert!(
            !json.contains("\"gear\""),
            "empty id fields are not written, keeping the store small: {json}"
        );
    }

    #[test]
    fn find_best_benchmark_matches_profession_and_mode() {
        let builds = vec![
            make_build("Guardian", "PvE", "Power DPS", "Berserker's"),
            make_build("Necromancer", "PvE", "Condi DPS", "Viper's"),
            make_build("Guardian", "WvW", "WvW Roaming", "Marauder"),
        ];

        let result = find_best_benchmark(&builds, "Guardian", "PvE", "Power DPS");
        assert!(result.is_some());
        let b = result.unwrap();
        assert_eq!(b.profession, "Guardian");
        assert_eq!(b.mode, "PvE");
        assert_eq!(b.gear_prefix, "Berserker's");
    }

    #[test]
    fn find_best_benchmark_returns_none_unknown_profession() {
        let builds = vec![make_build("Guardian", "PvE", "Power DPS", "Berserker's")];
        assert!(find_best_benchmark(&builds, "Thief", "PvE", "Power DPS").is_none());
    }

    #[test]
    fn find_best_benchmark_prefers_role_match() {
        let builds = vec![
            make_build("Guardian", "WvW", "WvW Zerg DPS", "Berserker's"),
            make_build("Guardian", "WvW", "WvW Roaming", "Marauder"),
        ];

        // Asking for Roaming should return Marauder build
        let result = find_best_benchmark(&builds, "Guardian", "WvW", "WvW Roaming");
        assert!(result.is_some());
        assert_eq!(result.unwrap().gear_prefix, "Marauder");
    }

    #[test]
    fn find_best_benchmark_case_insensitive_profession() {
        let builds = vec![make_build("Guardian", "PvE", "Power DPS", "Berserker's")];
        assert!(find_best_benchmark(&builds, "guardian", "PvE", "Power DPS").is_some());
    }

    #[test]
    fn role_similarity_counts_overlap() {
        assert_eq!(role_similarity("power dps", "power dps"), 2);
        assert_eq!(role_similarity("power dps", "condi dps"), 1);
        assert_eq!(role_similarity("power dps", "healer"), 0);
    }

    /// An empty db can plate nothing, so nothing is scorable and the meter
    /// stays honest by saying nothing at all.
    #[test]
    fn compute_benchmark_delta_without_a_scorable_reference_is_none() {
        let builds = vec![make_build("Guardian", "PvE", "Power DPS", "Berserker's")];
        let w = OptimizationWeights::preset_power_dps();
        let db = crate::gamedb::GameDb::empty_for_tests();
        let ctx = crate::balance::BalanceContext::new(gw2_core::types::GameMode::PvE);
        let scenario = crate::scenario::ScenarioSpec::from_balance_context(&ctx);
        assert!(compute_benchmark_delta(
            &builds,
            "Guardian",
            "PvE",
            "Power DPS",
            &w,
            0.7,
            true,
            &db,
            &ctx,
            &scenario,
        )
        .is_none());
    }

    /// A reference for a different job is still scored honestly, but it is
    /// not the same job, and the UI says so on this flag alone.
    #[test]
    fn role_matched_is_word_overlap_with_the_requested_role() {
        let matched = |hint: &str, role: &str| {
            role_similarity(&hint.to_lowercase(), &role.to_lowercase()) > 0
        };
        // The in-game case: no Disable reference exists, so a Roaming Condi
        // DPS build is offered and shares no word with what was asked for.
        assert!(!matched("Disable", "Roaming Condi DPS"));
        assert!(!matched("Support", "Power DPS"));
        assert!(matched("Power DPS", "Roaming Power DPS"));
        assert!(matched("Condi DPS", "Roaming Condi DPS"));
    }

    /// A gate failure no longer zeroes our own score - `ranked_direction_score`
    /// is measured either way - but a build the simulator produced NOTHING for
    /// still has no honest percentage, so the meter says nothing.
    #[test]
    fn compute_benchmark_delta_without_measurable_own_output_is_none() {
        let builds = vec![make_build("Guardian", "PvE", "Power DPS", "Berserker's")];
        let w = OptimizationWeights::preset_power_dps();
        let db = crate::gamedb::GameDb::empty_for_tests();
        let ctx = crate::balance::BalanceContext::new(gw2_core::types::GameMode::PvE);
        let scenario = crate::scenario::ScenarioSpec::from_balance_context(&ctx);
        assert!(compute_benchmark_delta(
            &builds,
            "Guardian",
            "PvE",
            "Power DPS",
            &w,
            0.0,
            false,
            &db,
            &ctx,
            &scenario,
        )
        .is_none());
    }

    /// The meter used to skip every reference whose direction score was the
    /// -1.0 non-viability sentinel, and a large share of synced WvW rows fail
    /// a gate at the player's scale - so it walked past the right reference
    /// to a worse one, or reported nothing at all. Now it scores them and
    /// says which side was refused.
    ///
    /// No named fixture. The meter needs a row that is BOTH refused and above
    /// `crate::scoring::INTENT_ALIGNMENT_FLOOR`, and which row that is moves
    /// every time a gate floor or an objective profile's focus axes are
    /// recalibrated - this test was pinned to a named row three times and
    /// drifted off it three times. So it searches the player's own corpus for
    /// one, names what it found, and fails only when the corpus has none at
    /// all, which is itself worth knowing.
    ///
    /// Cache-backed: a published plate needs the real `GameDb`. Prints and
    /// returns without `dev.cfg`.
    #[test]
    fn a_non_viable_reference_is_reported_not_skipped() {
        let Ok(addon_dir) = gw2_api::dev_config::addons_dir() else {
            println!("no dev.cfg: nothing to check");
            return;
        };
        let addon_dir = addon_dir.join("gw2_build_optimizer");
        let all = crate::scraper::load_benchmarks(&addon_dir);
        let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
        let Ok(db) = crate::gamedb::GameDb::load(&cache) else {
            println!("game data not cached: nothing to check");
            return;
        };
        if all.is_empty() {
            println!("no corpus synced: nothing to check");
            return;
        }

        let mode = gw2_core::types::GameMode::WvW;
        let ctx = crate::balance::BalanceContext::new(mode.clone());
        // Every role and scale the mode offers, because which combination
        // still has a refused-but-aligned row is exactly what keeps moving.
        let mut found = None;
        'search: for role in crate::scenario::RoleObjective::play_roles_for(&mode) {
            for tier in [
                crate::scenario::CombatTier::Solo,
                crate::scenario::CombatTier::Party,
                crate::scenario::CombatTier::Squad,
            ] {
                let w = role.to_weights_for(&mode, tier);
                let scenario = crate::scenario::ScenarioSpec {
                    combat_tier: tier,
                    combat_kind: role.combat_kind_for_weights(&w),
                    objective_profile_id: Some(role.profile_id_for(&mode, tier).to_string()),
                    ..crate::scenario::ScenarioSpec::from_balance_context(&ctx)
                };
                for build in all
                    .iter()
                    .filter(|b| b.mode == "WvW" && !b.published.is_empty())
                {
                    let Some(plate) = plate_from(build, &db) else {
                        continue;
                    };
                    let validated =
                        crate::validation::validate_gemini_build(&plate, &db, &build.profession);
                    if !validated.errors.is_empty() {
                        continue;
                    }
                    let report = crate::referee::evaluate_validated_build_ranked(
                        &validated,
                        &db,
                        &build.profession,
                        &w,
                        &ctx,
                        &scenario,
                    );
                    if report.viability.is_viable {
                        continue;
                    }
                    let aligned = report
                        .intent_alignment
                        .filter(|a| *a >= crate::scoring::INTENT_ALIGNMENT_FLOOR);
                    let Some(aligned) = aligned else { continue };
                    let refusing: Vec<String> = report
                        .viability
                        .gates
                        .iter()
                        .filter(|g| !g.passed && g.gate.blocks())
                        .map(|g| format!("{:?}", g.gate))
                        .collect();
                    found = Some((build.clone(), *role, w, scenario, aligned, refusing));
                    break 'search;
                }
            }
        }
        let (reference, role, w, scenario, aligned, refusing) = found.expect(
            "no synced WvW reference is both refused and above the alignment floor -              the meter cannot be exercised, which means either every reference now              passes every gate or the floor excludes every refused one",
        );
        println!(
            "fixture: {} {} {} ({:?}) aligned {aligned:.3}, refused on {refusing:?}",
            reference.source, reference.profession, reference.role, role,
        );

        let delta = compute_benchmark_delta(
            std::slice::from_ref(&reference),
            &reference.profession,
            "WvW",
            &reference.role,
            &w,
            0.7,
            true,
            &db,
            &ctx,
            &scenario,
        )
        .expect("a refused reference is still a reference");
        assert!(!delta.ref_viable, "this row fails a gate at this scale");
        assert!(delta.our_viable);
        assert!(
            delta.ref_score > 0.0,
            "the reference score is measured, not the sentinel: {}",
            delta.ref_score
        );
    }

    /// `plate_from` needs published ids; a row that has none cannot be
    /// compared and must not be guessed at.
    #[test]
    fn plate_from_without_published_specs_is_none() {
        let build = make_build("Guardian", "PvE", "Power DPS", "Berserker's");
        assert!(build.published.specs.is_empty());
        assert!(plate_from(&build, &crate::gamedb::GameDb::empty_for_tests()).is_none());
    }

    /// Equal scores read 100%; twice the reference is capped at 200%.
    #[test]
    fn pct_of_ref_is_a_ratio_capped_at_200() {
        assert!((pct_of(0.7, 0.7) - 100.0).abs() < 1e-9);
        assert!((pct_of(1.4, 0.7) - 200.0).abs() < 1e-9);
        assert!((pct_of(7.0, 0.7) - 200.0).abs() < 1e-9);
        assert!((pct_of(0.35, 0.7) - 50.0).abs() < 1e-9);
    }

    /// Every role string here is one the three sites actually publish.
    #[test]
    fn a_healer_and_a_dps_are_never_the_same_job() {
        use super::{job_family, JobFamily::*};

        // The case that produced a wrong card: asked for WvW Support on a
        // Necromancer, offered GuildJen's "Roaming DPS". GuildJen publishes
        // no Necromancer WvW support build at all, so the honest answer from
        // that site is nothing.
        assert_eq!(job_family("Support"), Some(Support));
        assert_eq!(job_family("Roaming DPS"), Some(Damage));
        assert_ne!(job_family("Support"), job_family("Roaming DPS"));

        // A DPS that hands out boons is a DPS. Damage is tested first for
        // exactly this reason.
        assert_eq!(job_family("Zerg Boon DPS"), Some(Damage));
        assert_eq!(job_family("Group Boon DPS"), Some(Damage));
        // A support that does damage is a support, and `heal` and `medic`
        // are tested before damage so this stays true.
        assert_eq!(job_family("Offensive Support"), Some(Support));
        assert_eq!(job_family("Havoc Medic"), Some(Support));
        assert_eq!(job_family("Fractal Healer"), Some(Support));
        assert_eq!(job_family("Zerg Support"), Some(Support));

        assert_eq!(job_family("Roaming Bruiser"), Some(Bruiser));
        assert_eq!(job_family("Cloud Tank"), Some(Bruiser));
        // An assassin is its own job, not a DPS with better numbers: ask
        // for one and a Zerg Power DPS is the wrong answer.
        assert_eq!(job_family("Roaming Assassin"), Some(Assassin));
        assert_eq!(job_family("Havoc Assassin"), Some(Assassin));
        assert_ne!(job_family("Roaming Assassin"), job_family("Zerg Power DPS"));
        assert_eq!(job_family("Duelist"), Some(Duelist));
        assert_ne!(job_family("Duelist"), job_family("Roaming DPS"));

        // Words that say nothing about the job rule nothing out: a Roamer
        // may be any of these, and refusing to guess is not the same as
        // guessing wrong.
        assert_eq!(
            job_family("Open World Hybrid"),
            Some(Damage),
            "hybrid deals damage"
        );
        assert_eq!(
            job_family("Sidenoder"),
            Some(Duelist),
            "holds a point alone"
        );
        // These say nothing about the job and are left saying nothing: a PvP
        // Roamer may be any of them, and refusing to guess is not the same
        // as guessing wrong.
        assert_eq!(job_family("Roamer"), None);
        assert_eq!(job_family("Raid"), None);
        assert_eq!(job_family("Group Niche"), None);
        assert_eq!(job_family(""), None);
    }
}
