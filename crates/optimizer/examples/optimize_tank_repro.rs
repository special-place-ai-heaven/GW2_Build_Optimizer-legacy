//! Reproduce the 2026-09-24 Improve Build report: a Willbender power build
//! (Dragon's/Marauder) run under WvW / Roam / Damage with Power 100 %,
//! Control 11 %, Sustain 48 %, Boon 33 % came back as a Hearty/Sentinel
//! Luminary tank.
//!
//! Same entry point the Improve flow uses (`engine::optimize_v2`, benchmarks
//! seeding on, no LLM advisor), the same scenario builder
//! (`ScenarioSpec::for_request`) and the same serve gate inputs (referee
//! rank of the result against the player's own gear). Prints per-axis scores
//! for the player's build, a tank kit, a Marauder power roamer and the
//! optimizer's result.
//!
//!   cargo run -p gw2-optimizer --release --example optimize_tank_repro
//!   cargo run -p gw2-optimizer --release --example optimize_tank_repro -- locked
//!   (`noopt` skips the ~50 s search and prints the fixed kits only)

use std::collections::HashMap;

use gw2_core::types::{BuildLocks, GameMode, GearSlot};
use gw2_optimizer::balance::BalanceContext;
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::prompts::GeminiBuildResponse;
use gw2_optimizer::referee::{evaluate_validated_build_ranked, search_rank, RefereeReport};
use gw2_optimizer::scenario::{CombatTier, RoleObjective, ScenarioSpec};
use gw2_optimizer::scoring::OptimizationWeights;
use gw2_optimizer::validation::{validate_gemini_build, ValidatedBuild};

const PROFESSION: &str = "Guardian";

// One argument per plate section; a struct would only rename them.
#[allow(clippy::too_many_arguments)]
fn plate(
    specs: [(&str, [&str; 3]); 3],
    weapons: &[&str],
    skills: &[&str],
    rune: &str,
    sigils: [(&str, &str); 4],
    armor: &str,
    trinkets: [(&str, &str); 6],
    weapon_prefix: [(&str, &str); 3],
) -> GeminiBuildResponse {
    let mut gear = HashMap::new();
    for slot in ["helm", "shoulders", "coat", "gloves", "leggings", "boots"] {
        gear.insert(slot.to_string(), armor.to_string());
    }
    for (slot, prefix) in trinkets.iter().chain(weapon_prefix.iter()) {
        gear.insert(slot.to_string(), prefix.to_string());
    }
    GeminiBuildResponse {
        specializations: specs
            .iter()
            .map(|(s, t)| (s.to_string(), t.iter().map(|x| x.to_string()).collect()))
            .collect(),
        weapons: weapons.iter().map(|s| s.to_string()).collect(),
        skills: skills.iter().map(|s| s.to_string()).collect(),
        rune: rune.to_string(),
        sigils_map: Some(
            sigils
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        gear_slots: Some(gear),
        ..Default::default()
    }
}

/// The active tabs of `char_hammerhand_the_bold_{buildtabs,equiptabs}.json`
/// ("AOE DAMAGE"), names resolved from the cached API data.
fn current(prefix_all: Option<&str>) -> GeminiBuildResponse {
    let p = |name: &'static str| prefix_all.unwrap_or(name).to_string();
    let (dr, ma, as_, be, di) = (
        p("Dragon's"),
        p("Marauder"),
        p("Assassin's"),
        p("Berserker's"),
        p("Diviner's"),
    );
    plate(
        [
            (
                "Radiance",
                ["Right-Hand Strength", "Retribution", "Righteous Instincts"],
            ),
            (
                "Virtues",
                [
                    "Unscathed Contender",
                    "Inspiring Virtue",
                    "Permeating Wrath",
                ],
            ),
            (
                "Willbender",
                [
                    "Power for Power",
                    "Restorative Virtues",
                    "Tyrant's Momentum",
                ],
            ),
        ],
        &["Set 1: Greatsword", "Set 2: Pistol / Focus"],
        &[
            "Heal: Litany of Wrath",
            "Utility: Bane Signet",
            "Utility: \"Stand Your Ground!\"",
            "Utility: Whirling Light",
            "Elite: \"Feel My Wrath!\"",
        ],
        "Superior Rune of the Scholar",
        [
            ("set1_main", "Superior Sigil of Hydromancy"),
            ("set1_off", "Superior Sigil of Rage"),
            ("set2_main", "Superior Sigil of Bloodlust"),
            ("set2_off", "Superior Sigil of Fire"),
        ],
        &dr,
        [
            ("back", &as_),
            ("accessory-1", &ma),
            ("accessory-2", &ma),
            ("ring-1", &be),
            ("ring-2", &ma),
            ("amulet", &di),
        ],
        [
            ("weapon-set-1-main", &dr),
            ("weapon-set-2-main", &dr),
            ("weapon-set-2-off", &ma),
        ],
    )
}

/// What 1.14.37 served for the same request (bisect, 2026-09-24): a viable
/// Dragon's/Marauder Willbender, Power 2667, Ferocity 995.
fn roamer_1_14_37() -> GeminiBuildResponse {
    plate(
        [
            (
                "Radiance",
                ["Right-Hand Strength", "Retribution", "Amplified Wrath"],
            ),
            ("Zeal", ["Fiery Wrath", "Zealous Blade", "Furious Focus"]),
            (
                "Willbender",
                ["Power for Power", "Holy Reckoning", "Tyrant's Momentum"],
            ),
        ],
        &["Set 1: Hammer", "Set 2: Spear"],
        &[
            "Heal: Litany of Wrath",
            "Utility: \"Save Yourselves!\"",
            "Utility: Sword of Justice",
            "Utility: \"Stand Your Ground!\"",
            "Elite: \"Feel My Wrath!\"",
        ],
        "Superior Rune of Strength",
        [
            ("set1_main", "Superior Sigil of the Night"),
            ("set1_off", "Superior Sigil of Force"),
            ("set2_main", "Superior Sigil of the Night"),
            ("set2_off", "Superior Sigil of Force"),
        ],
        "Dragon's",
        [
            ("back", "Diviner's"),
            ("accessory-1", "Dragon's"),
            ("accessory-2", "Marauder"),
            ("ring-1", "Marauder"),
            ("ring-2", "Marauder"),
            ("amulet", "Marauder"),
        ],
        [
            ("weapon-set-1-main", "Marauder"),
            ("weapon-set-2-main", "Marauder"),
            ("weapon-set-2-off", "Marauder"),
        ],
    )
}

/// The served result from the screenshots (25.png).
fn tank() -> GeminiBuildResponse {
    plate(
        [
            (
                "Honor",
                ["Protective Reviver", "Pure of Heart", "Force of Will"],
            ),
            (
                "Virtues",
                [
                    "Resolute Subconscious",
                    "Absolute Resolve",
                    "Indomitable Courage",
                ],
            ),
            (
                "Luminary",
                ["Shimmering Stances", "Purging Light", "Sovereign of Light"],
            ),
        ],
        &["Set 1: Sword / Focus", "Set 2: Spear"],
        &[
            "Heal: Resolute Stance",
            "Utility: Piercing Stance",
            "Utility: Effulgent Stance",
            "Utility: Stalwart Stance",
            "Elite: Daring Advance",
        ],
        "Superior Rune of Radiance",
        [
            ("set1_main", "Superior Sigil of Energy"),
            ("set1_off", "Superior Sigil of Cleansing"),
            ("set2_main", "Superior Sigil of Energy"),
            ("set2_off", "Superior Sigil of Bounty"),
        ],
        "Hearty",
        [
            ("back", "Hearty"),
            ("accessory-1", "Hearty"),
            ("accessory-2", "Hearty"),
            ("ring-1", "Hearty"),
            ("ring-2", "Hearty"),
            ("amulet", "Sentinel's"),
        ],
        [
            ("weapon-set-1-main", "Hearty"),
            ("weapon-set-1-off", "Hearty"),
            ("weapon-set-2-main", "Hearty"),
        ],
    )
}

fn prefixes(v: &ValidatedBuild) -> String {
    let mut count: Vec<(String, u32)> = Vec::new();
    for slot in GearSlot::ALL {
        if let Some(p) = v.gear_slots.get(slot) {
            match count.iter_mut().find(|(n, _)| *n == p.name) {
                Some((_, c)) => *c += 1,
                None => count.push((p.name.clone(), 1)),
            }
        }
    }
    count
        .iter()
        .map(|(n, c)| format!("{n}x{c}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn show(label: &str, v: &ValidatedBuild, r: &RefereeReport) {
    let a = &r.realized;
    let specs: Vec<&str> = v.specializations.iter().map(|s| s.name.as_str()).collect();
    println!("== {label}: {} | {}", specs.join("/"), prefixes(v));
    println!(
        "   Power {:.0} Precision {:.0} Ferocity {:.0} Toughness {:.0} Vitality {:.0} | EHP {:.0}",
        r.stats.get("Power"),
        r.stats.get("Precision"),
        r.stats.get("Ferocity"),
        r.stats.get("Toughness"),
        r.stats.get("Vitality"),
        r.primary_combat.effective_health,
    );
    if let Some(rot) = &r.rotation {
        println!(
            "   timeline strike_dps {:.0} condi_dps {:.0} over {} ms",
            rot.strike_dps, rot.condition_dps, rot.duration_ms
        );
    }
    println!(
        "   realized axes P {:.4} C {:.4} B {:.4} H {:.4} S {:.4} Ctl {:.4}",
        a.power, a.condition, a.boon_support, a.healing, a.sustain, a.control
    );
    println!(
        "   viable {} alignment {:?} user_intent {:.4} raw {:.4} ranked_dir {:.4}",
        r.viability.is_viable,
        r.intent_alignment,
        r.user_intent_score,
        r.raw_direction_score,
        r.ranked_direction_score
    );
    println!("   rank {:?}", search_rank(r));
    if !r.viability.is_viable {
        let failed: Vec<String> = r
            .viability
            .gates
            .iter()
            .filter(|g| !g.passed)
            .map(|g| format!("{:?} ({})", g.gate, g.note))
            .collect();
        println!("   failed gates: {}", failed.join(", "));
    }
}

/// The "vs meta" meter the Improve tab prints for `r`, and what the old
/// quantity (uncapped `ranked_direction_score` on both sides) read against
/// the same reference - the reference is picked by alignment, not by our
/// score, so it is the same row either way.
fn meter(
    label: &str,
    r: &RefereeReport,
    builds: &[gw2_optimizer::benchmark::BenchmarkBuild],
    db: &GameDb,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) {
    let Some(d) = gw2_optimizer::benchmark::compute_benchmark_delta(
        builds, PROFESSION, "WvW", "Damage", weights, r, db, ctx, scenario,
    ) else {
        println!("   meter [{label}]: none");
        return;
    };
    let old_ref = builds
        .iter()
        .filter(|b| b.source_url == d.ref_url && b.role == d.role)
        .find_map(|b| {
            let plate = gw2_optimizer::benchmark::plate_from(b, db)?;
            let v = validate_gemini_build(&plate, db, PROFESSION);
            v.errors.is_empty().then(|| {
                evaluate_validated_build_ranked(&v, db, PROFESSION, weights, ctx, scenario)
            })
        });
    if let Some(o) = &old_ref {
        let a = &o.realized;
        println!(
            "   reference axes P {:.4} C {:.4} B {:.4} H {:.4} S {:.4} Ctl {:.4} alignment {:?} rank {:?}",
            a.power, a.condition, a.boon_support, a.healing, a.sustain, a.control,
            o.intent_alignment, search_rank(o)
        );
    }
    let old = old_ref
        .map(|o| o.ranked_direction_score)
        .map(|o| {
            format!(
                "{:.0} % (reference {o:.2}, yours {:.2})",
                (r.ranked_direction_score / o * 100.0).clamp(0.0, 200.0),
                r.ranked_direction_score
            )
        })
        .unwrap_or_else(|| "?".into());
    println!(
        "   meter [{label}] vs {} {} ({}): new {:.0} % (reference {:.3}, yours {:.3}) | old {old}",
        d.source, d.role, d.ref_gear_prefix, d.pct_of_ref, d.ref_score, d.our_score
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let locked = args.iter().any(|a| a == "locked");
    let noopt = args.iter().any(|a| a == "noopt");
    let addon_dir = gw2_api::dev_config::addons_dir()
        .expect("dev.cfg addons_dir")
        .join("gw2_build_optimizer");
    let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
    let db = GameDb::load(&cache).expect("GameDb cached");

    // Radar as the player left it; total 1.92 <= WEIGHT_BUDGET, so
    // set_constrained leaves every axis as dragged.
    let mut weights = OptimizationWeights {
        power: 0.0,
        condition: 0.0,
        boon_support: 0.0,
        healing: 0.0,
        sustain: 0.0,
        control: 0.0,
    };
    for (axis, v) in [
        (0, 1.0),
        (1, 0.0),
        (2, 0.33),
        (3, 0.0),
        (4, 0.48),
        (5, 0.11),
    ] {
        weights.set_constrained(axis, v);
    }
    println!("weights {:?}", weights.as_array());

    let ctx = BalanceContext::new(GameMode::WvW);
    let scenario = ScenarioSpec::for_request(
        &ctx,
        CombatTier::Solo,
        Some(RoleObjective::PowerDps),
        &weights,
    );
    println!(
        "scenario kind {:?} tier {:?} profile {:?}",
        scenario.combat_kind, scenario.combat_tier, scenario.objective_profile_id
    );

    let eval = |label: &str, p: &GeminiBuildResponse| -> Option<(ValidatedBuild, RefereeReport)> {
        let v = validate_gemini_build(p, &db, PROFESSION);
        if !v.errors.is_empty() {
            println!(
                "== {label}: validator errors {:?}",
                v.errors.iter().map(|e| &e.detail).collect::<Vec<_>>()
            );
            return None;
        }
        let r = evaluate_validated_build_ranked(&v, &db, PROFESSION, &weights, &ctx, &scenario);
        show(label, &v, &r);
        Some((v, r))
    };

    let builds = gw2_optimizer::scraper::load_benchmarks(&addon_dir);
    let base = eval("current (AOE DAMAGE)", &current(None));
    eval("power roamer (all Marauder)", &current(Some("Marauder")));
    if let Some((_, r)) = eval("power roamer (1.14.37 result)", &roamer_1_14_37()) {
        meter(
            "1.14.37 roamer",
            &r,
            &builds,
            &db,
            &weights,
            &ctx,
            &scenario,
        );
    }
    if let Some((_, r)) = eval("tank (served 2026-09-24)", &tank()) {
        meter("tank", &r, &builds, &db, &weights, &ctx, &scenario);
    }

    let mut locks = BuildLocks::default();
    if locked {
        locks.specs[2] = db
            .specializations
            .values()
            .find(|s| s.name == "Willbender")
            .map(|s| s.id);
    }
    println!("locks {:?}", locks.specs);
    if noopt {
        return;
    }
    let t = std::time::Instant::now();
    let result = gw2_optimizer::engine::optimize_v2(
        &db,
        PROFESSION,
        &weights,
        &ctx,
        &scenario,
        &locks,
        None,
        Some(addon_dir.as_path()),
        &mut |_| {},
        &|| false,
    );
    match result {
        Ok(res) => {
            println!("optimize_v2 took {:.1}s", t.elapsed().as_secs_f64());
            let r = evaluate_validated_build_ranked(
                &res.validated,
                &db,
                PROFESSION,
                &weights,
                &ctx,
                &scenario,
            );
            show("optimize_v2 result", &res.validated, &r);
            meter(
                "optimize_v2 result",
                &r,
                &builds,
                &db,
                &weights,
                &ctx,
                &scenario,
            );
            println!(
                "   skills heal {:?} utils {:?} elite {:?} rune {:?}",
                res.validated.skills.heal.as_ref().map(|s| &s.1),
                res.validated
                    .skills
                    .utilities
                    .iter()
                    .flatten()
                    .map(|s| &s.1)
                    .collect::<Vec<_>>(),
                res.validated.skills.elite.as_ref().map(|s| &s.1),
                res.validated.rune.as_ref().map(|s| &s.name),
            );
            for s in &res.validated.specializations {
                println!("   spec {} {:?}", s.name, s.trait_names);
            }
            let w = &res.validated.weapons;
            println!(
                "   weapons {:?}/{:?} + {:?}/{:?} sigils {:?} relic {:?}",
                w.set1.main_hand,
                w.set1.off_hand,
                w.set2.main_hand,
                w.set2.off_hand,
                res.validated
                    .sigils
                    .iter()
                    .map(|s| &s.name)
                    .collect::<Vec<_>>(),
                res.validated.relic.as_ref().map(|s| &s.name),
            );
            for slot in GearSlot::ALL {
                if let Some(p) = res.validated.gear_slots.get(slot) {
                    print!(" {}={}", slot.kebab_name(), p.name);
                }
            }
            println!();
            if let Some((_, b)) = &base {
                let beats = search_rank(&r) > search_rank(b);
                println!("serve gate: result beats current = {beats}");
            }
        }
        Err(e) => println!("optimize_v2 failed: {e}"),
    }
}
