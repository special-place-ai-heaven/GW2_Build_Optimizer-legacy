//! Ranking the community reference builds by what they DO.
//!
//! The old matcher read words off a page - "Havoc Medic", "Cloud Support" -
//! and filtered on them. Words are the wrong evidence: the three sites do not
//! share a vocabulary, nothing any of them publishes is labelled with the
//! addon's Disable role, and a label is a claim about a build rather than a
//! measurement of it. Every family, job word and role gate is gone from this
//! path; what is left is two vectors and the angle between them.
//!
//! * The INTENT vector is the player's own six-axis `OptimizationWeights`,
//!   exactly as the addon hands them to the optimizer: the role objective
//!   profile after the fine-tune sliders.
//! * The CANDIDATE vector is the reference's measured `realized` axes from
//!   the ranked referee, run under the player's scenario and balance
//!   context - the same referee that ranks the optimizer's own result.
//!
//! The selection rule is [`crate::scoring::intent_alignment`]: the axes the
//! role exists to deliver, weighted by the profile's own weights, minus the
//! axes it exists not to. The angle between the weights and the axes is
//! kept as a diagnostic only - measured over the corpus it ranked damage
//! references above support ones for a support request, because every build
//! sustains and that common mode dominates the direction.
//!
//! A reference that cannot be plated, cannot be validated, or produced no
//! measurement at all scores nothing and is never guessed at from its
//! label.

use std::collections::BTreeMap;

use crate::balance::BalanceContext;
use crate::benchmark::BenchmarkBuild;
use crate::gamedb::GameDb;
use crate::referee::RefereeReport;
use crate::scenario::{CombatTier, RoleObjective, ScenarioSpec};
use crate::scoring::OptimizationWeights;

/// How many people a page says the build is for.
///
/// Not a gate and not part of the similarity: a tie-break, and the line on
/// the card. The referee axes cannot recover it - the objective profiles
/// vary by tier for exactly one role in one mode, so two scales of the same
/// job measure as the same vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatedScale {
    Solo,
    Small,
    Large,
}

impl StatedScale {
    /// The tier chips, as this names them.
    pub fn from_tier(tier: CombatTier) -> Self {
        match tier {
            CombatTier::Solo => StatedScale::Solo,
            CombatTier::Party => StatedScale::Small,
            CombatTier::Squad => StatedScale::Large,
        }
    }

    /// The word a card shows.
    pub fn label(self) -> &'static str {
        match self {
            StatedScale::Solo => "Roam",
            StatedScale::Small => "Havoc",
            StatedScale::Large => "Cloud",
        }
    }
}

/// Read the scale off the page's own words.
///
/// `summary` is the opening of the article body and `role` the site's role
/// label; both are English on all three sites. Checked smallest-first so
/// "roaming havoc" reads as the roam build it says it is.
pub fn stated_scale(summary: &str, role: &str) -> Option<StatedScale> {
    // The opening paragraphs describe the build; further down is the
    // rotation and the gear, whose words say nothing about group size.
    let head: String = summary.chars().take(600).collect();
    let text = format!("{} {}", role, head).to_lowercase();
    let has = |word: &str| text.contains(word);
    if has("roam") || has("solo") || has("duel") || has("open world") || has("sidenode") {
        return Some(StatedScale::Solo);
    }
    if has("havoc") || has("party") || has("group") || has("small scale") || has("fractal") {
        return Some(StatedScale::Small);
    }
    if has("zerg") || has("cloud") || has("squad") || has("blob") || has("raid") {
        return Some(StatedScale::Large);
    }
    None
}

/// Cosine of two six-axis vectors. Zero when either has no magnitude.
pub fn cosine(a: &[f64; 6], b: &[f64; 6]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// What a build actually equips, by name.
///
/// The plate side of [`kit_overlap`]. Names rather than ids because that is
/// what a proposal has: the model answers in names and they are resolved
/// against the API on the way in.
#[derive(Debug, Clone, Default)]
pub struct Kit {
    /// Specialization names, elite included.
    pub specs: Vec<String>,
    /// Selected trait names across all three lines.
    pub traits: Vec<String>,
    /// Weapon type names, both sets.
    pub weapons: Vec<String>,
    pub rune: String,
    pub relic: String,
    pub sigils: Vec<String>,
    pub stat_prefix: String,
}

/// How much of the same KIT two builds equip, 0.0 to 1.0.
///
/// A meta build is not its role word, it is its traits, runes, relic,
/// sigils and weapon sets. Two builds that share seven of nine traits are
/// the same build with a different gear opinion, and that is worth saying
/// when two references measure the same.
///
/// Averaged over the categories the PLATE has, so a proposal with no relic
/// is not punished for the reference having one. Pure content: ids and
/// names, never a role word. It never gates and never outranks the measured
/// direction - see [`order_picks`].
pub fn kit_overlap(kit: &Kit, build: &BenchmarkBuild, db: &GameDb) -> f64 {
    let lower = |s: &str| s.to_lowercase();
    let published = &build.published;

    let their_specs: Vec<String> = published
        .specs
        .iter()
        .filter_map(|line| db.specializations.get(&line.id))
        .map(|spec| lower(&spec.name))
        .collect();
    let their_traits: Vec<String> = published
        .specs
        .iter()
        .flat_map(|line| line.trait_ids.iter())
        .filter_map(|id| db.traits.get(id))
        .map(|t| lower(&t.name))
        .collect();
    let their_weapons: Vec<String> = published.gear.iter().map(|g| lower(&g.slot)).collect();
    let named = |id: Option<u32>| {
        id.and_then(|id| db.items.get(&id))
            .map(|item| lower(&item.name))
            .unwrap_or_default()
    };
    let their_sigils: Vec<String> = published
        .sigil_ids
        .iter()
        .filter_map(|id| db.items.get(id))
        .map(|item| lower(&item.name))
        .collect();

    let share = |mine: &[String], theirs: &[String]| {
        (!mine.is_empty()).then(|| {
            let hits = mine
                .iter()
                .filter(|name| theirs.iter().any(|t| t == &lower(name)))
                .count();
            hits as f64 / mine.len() as f64
        })
    };
    let same =
        |mine: &str, theirs: String| (!mine.is_empty()).then(|| f64::from(theirs == lower(mine)));

    let parts: Vec<f64> = [
        share(&kit.specs, &their_specs),
        share(&kit.traits, &their_traits),
        share(&kit.weapons, &their_weapons),
        share(&kit.sigils, &their_sigils),
        same(&kit.rune, named(published.rune_id)),
        same(&kit.relic, named(published.relic_id)),
        same(
            &kit.stat_prefix,
            lower(
                &published
                    .dominant_stat()
                    .unwrap_or_else(|| build.gear_prefix.clone()),
            ),
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        return 0.0;
    }
    parts.iter().sum::<f64>() / parts.len() as f64
}

/// The shape a page says it is, in the addon's own chip vocabulary.
///
/// Stated intent, never measured intent: a tie-break between references
/// that measure the same, and nothing else. It cannot promote a build the
/// measurement placed below another, and it can never keep a card off the
/// panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatedArchetype {
    Assassin,
    Damage,
    Bruiser,
    Troll,
    Support,
    Heal,
    Disable,
    Commander,
}

impl StatedArchetype {
    /// The chip the player pressed, as an archetype. `None` where the chip
    /// names no shape (Hybrid, Roamer - a roamer is three jobs).
    pub fn for_role(role: RoleObjective) -> Option<Self> {
        use RoleObjective as R;
        Some(match role {
            R::PowerDps | R::CondiDps | R::WvWZergDps => StatedArchetype::Damage,
            R::PvPBurst => StatedArchetype::Assassin,
            R::Sustain => StatedArchetype::Bruiser,
            R::Staller | R::PvPSustain => StatedArchetype::Troll,
            R::Buffer | R::WvWZergSupport => StatedArchetype::Support,
            R::Healer => StatedArchetype::Heal,
            R::Disabler | R::WvWDisruptor | R::PvPDisruptor => StatedArchetype::Disable,
            R::Tank => StatedArchetype::Commander,
            R::Hybrid | R::WvWRoamer => return None,
        })
    }
}

/// Read the archetype off the page's own words.
///
/// Same evidence as [`stated_scale`] and the same rule: the opening of the
/// article plus the site's role label, English on all three sites. Ordered
/// most specific first, so "one-shot roaming bruiser" reads as the assassin
/// it opens with.
pub fn stated_archetype(summary: &str, role: &str) -> Option<StatedArchetype> {
    let head: String = summary.chars().take(600).collect();
    let text = format!("{} {}", role, head).to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if any(&["one-shot", "one shot", "glass", "assassin", "burst"]) {
        return Some(StatedArchetype::Assassin);
    }
    if any(&["commander", "frontline lead", "tag"]) {
        return Some(StatedArchetype::Commander);
    }
    if any(&["healer", "heal ", "medic"]) {
        return Some(StatedArchetype::Heal);
    }
    if any(&["support", "boon", "buffer"]) {
        return Some(StatedArchetype::Support);
    }
    if any(&["disable", "control", "cc ", "boonrip", "strip"]) {
        return Some(StatedArchetype::Disable);
    }
    if any(&["troll", "bunker", "tank", "immortal"]) {
        return Some(StatedArchetype::Troll);
    }
    if any(&["bruiser", "brawl", "sustain"]) {
        return Some(StatedArchetype::Bruiser);
    }
    if any(&["dps", "damage", "condi", "power"]) {
        return Some(StatedArchetype::Damage);
    }
    None
}

/// One published reference, measured under the player's own scenario.
pub struct PickEvaluation {
    /// Index into the candidate slice this was built from.
    pub index: usize,
    /// Referee realized axes: what this build was measured doing.
    pub axes: [f64; 6],
    /// Cosine between [`Self::axes`] and the player's weights. DIAGNOSTIC
    /// only: it cannot separate a support build from a damage one.
    pub cosine: f64,
    /// [`crate::scoring::intent_alignment`] for this build under the
    /// player's role. The rank key, and the only thing that decides whether
    /// a card exists at all. `None` when the referee measured nothing, or
    /// when the scenario names no role - both mean "no card".
    pub alignment: Option<f64>,
    /// Whether every blocking gate passed at the player's scale.
    pub viable: bool,
    /// What the page says about group size, if anything.
    pub stated_scale: Option<StatedScale>,
    /// What the page says the build IS, if anything. Tie-break only.
    pub stated_archetype: Option<StatedArchetype>,
    /// [`kit_overlap`] against the plate. Tie-break only.
    pub kit_overlap: f64,
    /// The full report, so an adopted tab does not simulate twice.
    ///
    /// `Option` only so the ordering can be tested without building a
    /// referee report; [`evaluate`] always fills it.
    pub report: Option<RefereeReport>,
}

/// How wide a band of alignment counts as "the same direction".
///
/// The alignment is a continuous number, so without a band the tie-breaks
/// below would never fire: two references that serve the request equally
/// well would be separated by the fourth decimal of a simulation. Within a
/// band the measurement has said all it can, and the kit, the page's own
/// words and the gates decide.
///
/// 0.02 is about a fiftieth of the range the metric spans on real
/// references; wider and a visibly better build loses to a familiar kit.
pub const ALIGNMENT_BAND: f64 = 0.02;

/// Put the measured references in the order they should be offered.
///
/// Descending preference:
///
/// 1. [`PickEvaluation::alignment`], banded by [`ALIGNMENT_BAND`] - how much
///    of what the role is for this build was measured delivering, minus what
///    it is written to avoid. An unmeasured build sorts last.
/// 2. [`PickEvaluation::kit_overlap`] - inside one band, the reference that
///    equips what the plate equips is the more useful thing to show.
/// 3. The page's stated archetype equals the one the chip names.
/// 4. The page's stated scale equals the one selected.
/// 5. Viable at the player's scale.
///
/// Only the first key is the question; the rest decide between builds the
/// measurement calls equal. Viability is last because a published reference
/// is written for a scale and a fight the player did not select, so a gate
/// it fails here is information rather than a disqualification - the card
/// says so in amber.
///
/// Never `raw_direction_score`: that collapses to the -1.0 sentinel for
/// every non-viable build. Never `ranked_direction_score` either: it folds
/// all six axes into one magnitude, so a build scores well for a role by
/// being good at something else.
pub fn order_picks(
    picks: &mut [PickEvaluation],
    want_scale: Option<StatedScale>,
    want_archetype: Option<StatedArchetype>,
) {
    picks.sort_by(|a, b| {
        let band = |p: &PickEvaluation| match p.alignment {
            Some(a) => (a / ALIGNMENT_BAND).round() as i64,
            None => i64::MIN,
        };
        let kit = |p: &PickEvaluation| p.kit_overlap;
        let words = |p: &PickEvaluation| {
            (
                want_archetype.is_some() && p.stated_archetype == want_archetype,
                want_scale.is_some() && p.stated_scale == want_scale,
                p.viable,
            )
        };
        band(b)
            .cmp(&band(a))
            .then_with(|| {
                kit(b)
                    .partial_cmp(&kit(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| words(b).cmp(&words(a)))
            .then_with(|| a.index.cmp(&b.index))
    });
}

/// Everything one request produced: the candidates, in offer order.
pub struct Ranked<'a> {
    /// Synced rows that were worth measuring, in the order [`rank`] found
    /// them. `PickEvaluation::index` indexes this.
    pub candidates: Vec<&'a BenchmarkBuild>,
    /// Measured and ordered. Only rows that plated and validated appear.
    pub picks: Vec<PickEvaluation>,
    /// What could not be read, per source.
    pub unparsed: UnparsedBySource,
}

impl<'a> Ranked<'a> {
    /// One card per source: each site's best, and only above the floor.
    ///
    /// Returns the winners in order and the sources that published
    /// candidates but nothing worth offering.
    pub fn cards(&self) -> (Vec<(&'a BenchmarkBuild, &PickEvaluation)>, Vec<String>) {
        let mut winners: Vec<(&BenchmarkBuild, &PickEvaluation)> = Vec::new();
        for pick in &self.picks {
            if pick
                .alignment
                .is_none_or(|a| a < crate::scoring::INTENT_ALIGNMENT_FLOOR)
            {
                continue;
            }
            let build = self.candidates[pick.index];
            if winners.iter().any(|(w, _)| w.source == build.source) {
                continue;
            }
            winners.push((build, pick));
        }
        let mut silent: Vec<String> = Vec::new();
        for build in &self.candidates {
            let offered = winners.iter().any(|(w, _)| w.source == build.source);
            if !offered && !silent.contains(&build.source) {
                silent.push(build.source.clone());
            }
        }
        (winners, silent)
    }
}

/// Answer one request: which published builds serve it, best first.
///
/// THE entry point. The addon, the corpus test and the example all call
/// this, so a card the example prints is the card the panel shows. Building
/// the scenario, resolving the objective profile, measuring, ordering and
/// the stated-intent tie-breaks all happen here rather than at each call
/// site, because a caller that assembles them slightly differently does not
/// get a slightly different order - it measures a different question.
#[allow(clippy::too_many_arguments)]
pub fn rank<'a>(
    builds: &'a [BenchmarkBuild],
    db: &GameDb,
    ctx: &BalanceContext,
    profession: &str,
    tier: CombatTier,
    role: Option<RoleObjective>,
    weights: &OptimizationWeights,
    kit: &Kit,
    cancelled: &dyn Fn() -> bool,
) -> Ranked<'a> {
    let mode = ctx.game_mode.label();
    let scenario = ScenarioSpec::for_request(ctx, tier, role, weights);
    let candidates = candidates(builds, profession, mode);
    let (mut picks, unparsed) = evaluate(&candidates, db, weights, ctx, &scenario, kit, cancelled);
    // PvP has no scale to pick - conquest is always five a side, the chips
    // are hidden and the tier is forced to Solo, so reading it would wrongly
    // favour solo-labelled references.
    let want_scale = (!matches!(ctx.game_mode, gw2_core::types::GameMode::PvP))
        .then(|| StatedScale::from_tier(tier));
    order_picks(
        &mut picks,
        want_scale,
        role.and_then(StatedArchetype::for_role),
    );
    Ranked {
        candidates,
        picks,
        unparsed,
    }
}

/// What a scoring run could not use, per source.
///
/// A site whose every row failed to plate or validate has not "published
/// nothing" - we failed to read it, and saying so is the honest difference.
pub type UnparsedBySource = BTreeMap<String, usize>;

/// Candidates for a profession and mode: every synced row with published ids.
///
/// No label filter of any kind. The measurement decides.
pub fn candidates<'a>(
    builds: &'a [BenchmarkBuild],
    profession: &str,
    mode: &str,
) -> Vec<&'a BenchmarkBuild> {
    let prof = profession.to_lowercase();
    let mode = mode.to_lowercase();
    builds
        .iter()
        .filter(|b| {
            !b.published.is_empty()
                && b.profession.to_lowercase().contains(&prof)
                && b.mode.to_lowercase() == mode
        })
        .collect()
}

/// Measure every candidate under the player's own scenario.
///
/// Roughly 0.6 ms per build in release, over the 10 to 50 rows a profession
/// and mode has, so this is worth doing live on a worker and not worth
/// caching. It must not run on the render thread.
pub fn evaluate(
    candidates: &[&BenchmarkBuild],
    db: &GameDb,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
    kit: &Kit,
    cancelled: &dyn Fn() -> bool,
) -> (Vec<PickEvaluation>, UnparsedBySource) {
    let want = weights.as_array();
    let mut out = Vec::new();
    let mut unparsed = UnparsedBySource::new();
    for (index, build) in candidates.iter().enumerate() {
        if cancelled() {
            break;
        }
        let mut failed = || {
            *unparsed.entry(build.source.clone()).or_default() += 1;
        };
        let Some(plate) = crate::benchmark::plate_from(build, db) else {
            failed();
            continue;
        };
        let validated = crate::validation::validate_gemini_build(&plate, db, &build.profession);
        if !validated.errors.is_empty() {
            failed();
            continue;
        }
        // Ranked, not search: a reference that fails a gate still needs real
        // axes, or every refused row comes back as the same sustain-only
        // vector and the ordering sorts an artifact.
        let report = crate::referee::evaluate_validated_build_ranked(
            &validated,
            db,
            &build.profession,
            weights,
            ctx,
            scenario,
        );
        let axes = report.realized.as_array();
        // A report that measured nothing carries no alignment, so it sorts
        // last and never clears the floor. It is never rescued by reading
        // its label.
        out.push(PickEvaluation {
            index,
            axes,
            cosine: cosine(&axes, &want),
            alignment: report.intent_alignment,
            viable: report.viability.is_viable,
            stated_scale: stated_scale(&build.published.prose, &build.role),
            stated_archetype: stated_archetype(&build.published.prose, &build.role),
            kit_overlap: kit_overlap(kit, build, db),
            report: Some(report),
        });
    }
    (out, unparsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axes(v: [f64; 6]) -> [f64; 6] {
        v
    }

    #[test]
    fn cosine_is_one_for_parallel_and_zero_for_orthogonal() {
        let a = axes([1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let b = axes([3.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let c = axes([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert!(
            (cosine(&a, &b) - 1.0).abs() < 1e-9,
            "direction, not magnitude"
        );
        assert!(cosine(&a, &c).abs() < 1e-9);
        assert_eq!(cosine(&a, &axes([0.0; 6])), 0.0, "no magnitude, no angle");
    }

    #[test]
    fn stated_scale_reads_the_three_shapes_the_sites_publish() {
        assert_eq!(
            stated_scale("A roaming bruiser with strong sustain for solo play.", ""),
            Some(StatedScale::Solo)
        );
        assert_eq!(
            stated_scale("Zerg backline boon support for large scale fights.", ""),
            Some(StatedScale::Large)
        );
        assert_eq!(
            stated_scale("", "Havoc Medic"),
            Some(StatedScale::Small),
            "the role label counts when the page says nothing"
        );
        assert_eq!(
            stated_scale(
                "A condition damage build using bleeding and torment.",
                "Condi DPS"
            ),
            None,
            "damage words are not scale words"
        );
        assert_eq!(
            stated_scale("A roaming build that also works in havoc groups.", ""),
            Some(StatedScale::Solo),
            "smallest first: it says what it is before what it also does"
        );
    }

    fn pick(index: usize, viable: bool, alignment: f64) -> PickEvaluation {
        PickEvaluation {
            index,
            axes: [0.0; 6],
            cosine: 0.0,
            alignment: Some(alignment),
            viable,
            stated_scale: None,
            stated_archetype: None,
            kit_overlap: 0.0,
            report: None,
        }
    }

    #[test]
    fn the_alignment_decides_and_nothing_else_can_overturn_it() {
        let mut picks = vec![
            pick(0, true, 0.80),
            pick(1, false, 0.95),
            pick(2, true, 0.90),
        ];
        order_picks(&mut picks, None, None);
        assert_eq!(
            picks.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 2, 0],
            "a non-viable build that delivers more of the role still leads"
        );
    }

    #[test]
    fn the_stated_scale_then_viability_break_a_tie() {
        let mut wrong_scale = pick(0, true, 0.90);
        wrong_scale.stated_scale = Some(StatedScale::Large);
        let mut right_scale = pick(1, false, 0.90);
        right_scale.stated_scale = Some(StatedScale::Small);
        let mut picks = vec![wrong_scale, right_scale];
        order_picks(&mut picks, Some(StatedScale::Small), None);
        assert_eq!(picks[0].index, 1, "same scale first when the angle ties");

        let mut both = vec![pick(0, false, 0.5), pick(1, true, 0.5)];
        order_picks(&mut both, None, None);
        assert_eq!(both[0].index, 1, "then the one that survives its gates");
    }

    /// The measurement this whole path rests on: two references written for
    /// different jobs must come back as different vectors.
    ///
    /// The bug it was written around: a reference that fails a gate used to
    /// be handed `realized_axes_no_rotation`, which fills only sustain, so
    /// two refused rows measured the same `[0, 0, 0, 0, .63, 0]` and scored
    /// the same cosine against every request.
    ///
    /// Fixtures are GuildJen's Tempest "Havoc Support" and its Tempest
    /// "Cloud Support". Both are refused today on `StunbreakCount` and
    /// nothing else - neither page slots a stunbreak - so the pair is the
    /// exact case this test is about: same profession, same elite spec, same
    /// single refusing gate, different jobs. The gate is asserted BY NAME:
    /// if it is ever fixed, this fails loudly and asks for a new fixture
    /// rather than quietly passing on a viable build.
    ///
    /// Needs the player's own synced corpus and cached `GameDb`: a published
    /// plate cannot be validated against a fixture database, so there is
    /// nothing to simulate without one. Prints and returns when `dev.cfg`
    /// is absent, the same as the other cache-backed checks in this crate.
    #[test]
    fn refused_references_measure_apart_instead_of_collapsing_to_sustain() {
        let Ok(addon_dir) = gw2_api::dev_config::addons_dir() else {
            println!("no dev.cfg: nothing to check");
            return;
        };
        let addon_dir = addon_dir.join("gw2_build_optimizer");
        let builds = crate::scraper::load_benchmarks(&addon_dir);
        let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
        let Ok(db) = crate::gamedb::GameDb::load(&cache) else {
            println!("game data not cached: nothing to check");
            return;
        };
        let rows: Vec<&crate::benchmark::BenchmarkBuild> = ["Havoc Support", "Cloud Support"]
            .iter()
            .filter_map(|role| {
                builds.iter().find(|b| {
                    b.source == "guildjen"
                        && b.profession == "Elementalist"
                        && b.spec_name == "Tempest"
                        && b.mode == "WvW"
                        && b.role == *role
                        && !b.published.is_empty()
                })
            })
            .collect();
        if rows.len() < 2 {
            println!("corpus has not been synced with both GuildJen Tempest rows");
            return;
        }

        let mode = gw2_core::types::GameMode::WvW;
        let ctx = crate::balance::BalanceContext::new(mode.clone());
        let scenario = crate::scenario::ScenarioSpec {
            combat_tier: crate::scenario::CombatTier::Party,
            ..crate::scenario::ScenarioSpec::from_balance_context(&ctx)
        };
        let weights = crate::scenario::RoleObjective::play_roles_for(&mode)
            .iter()
            .find(|r| r.label().to_lowercase().contains("support"))
            .map(|r| r.to_weights_for(&mode, scenario.combat_tier))
            .expect("WvW publishes a support role objective");

        let (evaluated, _) = evaluate(
            &rows,
            &db,
            &weights,
            &ctx,
            &scenario,
            &Kit::default(),
            &|| false,
        );
        assert_eq!(evaluated.len(), 2, "both rows must plate and validate");

        // Both still fail a blocking gate at the player's scale, and it is
        // the gate this fixture was chosen for. The axes are real anyway.
        for pick in &evaluated {
            let report = pick.report.as_ref().expect("evaluate always fills it");
            assert_eq!(
                blocking_failures(&report.viability),
                vec!["StunbreakCount".to_string()],
                "fixture must still be refused on exactly that gate - if the \
                 gate was fixed, pick a new refused pair rather than deleting \
                 this assertion"
            );
            assert!(!pick.viable);
        }
        let [havoc, cloud] = [&evaluated[0], &evaluated[1]];
        let differing = (0..6)
            .filter(|&axis| (havoc.axes[axis] - cloud.axes[axis]).abs() > 1e-6)
            .count();
        assert!(
            differing >= 2,
            "a havoc healer and a zerg healer must differ on more than \
             sustain: havoc {:?} cloud {:?}",
            havoc.axes,
            cloud.axes
        );
        assert!(
            (havoc.cosine - cloud.cosine).abs() > 1e-6,
            "distinct axes must produce distinct cosines: {} vs {}",
            havoc.cosine,
            cloud.cosine
        );

        // The search path must NOT have paid for this. It still sees the
        // sustain-only vector, and its scores still carry the sentinel.
        let plate = crate::benchmark::plate_from(rows[0], &db).expect("the havoc row plates");
        let validated = crate::validation::validate_gemini_build(&plate, &db, "Elementalist");
        let searched = crate::referee::evaluate_validated_build(
            &validated,
            &db,
            "Elementalist",
            &weights,
            &ctx,
            &scenario,
        );
        assert!(!searched.viability.is_viable);
        let axes = searched.realized.as_array();
        assert!(
            axes.iter().enumerate().all(|(i, v)| i == 4 || *v == 0.0),
            "the search path must keep the cheap fallback: {axes:?}"
        );
        assert_eq!(searched.raw_direction_score, -1.0);
        assert_eq!(
            searched.ranked_direction_score, searched.raw_direction_score,
            "non-ranked entry points leave the field alone"
        );

        let ranked = crate::referee::evaluate_validated_build_ranked(
            &validated,
            &db,
            "Elementalist",
            &weights,
            &ctx,
            &scenario,
        );
        assert_eq!(
            ranked.raw_direction_score, -1.0,
            "the sentinel is unchanged"
        );
        assert!(
            ranked.ranked_direction_score > 0.0,
            "the ranked score is measured, not gated: {}",
            ranked.ranked_direction_score
        );
        assert_eq!(ranked.realized.as_array(), havoc.axes);

        // The angle only exists where the axes were measured. The search path
        // collapsed them, so it reports no direction at all rather than the
        // direction of a vector that is sustain and five zeroes.
        assert_eq!(
            searched.intent_similarity, None,
            "a collapsed axis vector has no measured direction"
        );
        let sim = ranked
            .intent_similarity
            .expect("the ranked path measured the axes, so it has an angle");
        assert!(
            (0.0..=1.0).contains(&sim),
            "a cosine of two non-negative vectors is in [0, 1]: {sim}"
        );
        assert!(
            (sim - cosine(&havoc.axes, &weights.as_array())).abs() < 1e-9,
            "intent_similarity must be the same cosine the picks card shows"
        );
    }

    /// Names of the blocking gates a report failed, so a fixture can assert
    /// WHICH gate refused it and fail loudly when that gate is fixed.
    fn blocking_failures(report: &crate::referee::ViabilityReport) -> Vec<String> {
        report
            .gates
            .iter()
            .filter(|g| !g.passed && g.gate.blocks())
            .map(|g| format!("{:?}", g.gate))
            .collect()
    }
    /// The kit decides inside a band and never across one.
    #[test]
    fn the_kit_breaks_a_tie_inside_the_band_and_never_across_it() {
        let mut familiar = pick(0, true, 0.500);
        familiar.kit_overlap = 0.9;
        let mut stranger = pick(1, true, 0.508);
        stranger.kit_overlap = 0.0;
        let mut picks = vec![stranger, familiar];
        order_picks(&mut picks, None, None);
        assert_eq!(
            picks[0].index, 0,
            "inside one band the measurement has said all it can"
        );

        let mut better = pick(1, true, 0.80);
        better.kit_overlap = 0.0;
        let mut familiar = pick(0, true, 0.50);
        familiar.kit_overlap = 1.0;
        let mut picks = vec![familiar, better];
        order_picks(&mut picks, None, None);
        assert_eq!(
            picks[0].index, 1,
            "a whole different direction is not overturned by a shared kit"
        );
    }

    #[test]
    fn stated_archetype_reads_the_words_the_sites_use() {
        assert_eq!(
            stated_archetype("A one-shot Deadeye build for roaming.", ""),
            Some(StatedArchetype::Assassin)
        );
        assert_eq!(
            stated_archetype("", "Havoc Medic"),
            Some(StatedArchetype::Heal)
        );
        assert_eq!(
            stated_archetype("Zerg backline boon support.", ""),
            Some(StatedArchetype::Support)
        );
        assert_eq!(
            stated_archetype("A bruiser that brawls on the point.", ""),
            Some(StatedArchetype::Bruiser)
        );
        assert_eq!(stated_archetype("A build.", ""), None);
        assert_eq!(
            StatedArchetype::for_role(RoleObjective::WvWRoamer),
            None,
            "a roamer is three jobs, so the chip names no shape"
        );
    }
}
