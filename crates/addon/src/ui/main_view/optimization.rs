use super::stats::{compute_3tier_combat, perf_to_combat_metrics};
use gw2_core::i18n::t;
use gw2_core::types::GearSlot;
use gw2_optimizer::balance::BalanceContext;

/// Convert a SynergyResult from the new pipeline into a BuildSuggestion for display.
// Display adapter; db, profession, scenario, role, and result are distinct
// inputs threaded straight through — a params struct adds no clarity here.
/// The coverage detail of the referee's `wvw_timeline.effects` reason: the
/// names after `Not simulated: `, ready for the `quality.coverage_line`
/// locale key. The referee is the one source; every projection reads it.
pub(super) fn coverage_note_from(
    reasons: &[gw2_optimizer::data::DataQualityReason],
) -> Option<String> {
    reasons
        .iter()
        .find(|r| r.field == gw2_optimizer::data::quality::COVERAGE_FIELD)
        .map(|r| {
            r.explanation
                .strip_prefix(gw2_optimizer::data::quality::COVERAGE_PREFIX)
                .unwrap_or(&r.explanation)
                .to_string()
        })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn synergy_result_to_suggestion(
    result: &gw2_optimizer::engine::SynergyResult,
    db: &gw2_optimizer::gamedb::GameDb,
    profession_name: &str,
    scenario: &gw2_optimizer::scenario::ScenarioSpec,
    role: Option<gw2_optimizer::scenario::RoleObjective>,
    label_override: Option<String>,
    addon_dir: &std::path::Path,
    weights: &gw2_optimizer::scoring::OptimizationWeights,
) -> crate::ui::comparison::BuildSuggestion {
    use crate::ui::comparison::BuildSuggestion;

    let v = &result.validated;
    let chat_code = validated_build_to_chat_code(v, profession_name, db);

    // Specializations: (name, [trait_name1, trait_name2, trait_name3])
    let specializations: Vec<(String, Vec<String>)> = v
        .specializations
        .iter()
        .map(|s| {
            let label = if s.elite {
                format!("{} [E]", s.name)
            } else {
                s.name.clone()
            };
            (label, s.trait_names.clone())
        })
        .collect();

    // Weapons: flatten into display strings like "Set 1: Sword / Shield"
    let mut weapons = Vec::new();
    let fmt_set =
        |set: &gw2_optimizer::validation::ValidatedWeaponSet, label: &str| -> Option<String> {
            match (&set.main_hand, &set.off_hand) {
                (Some(main), Some(off)) => Some(format!("{}: {} / {}", label, main, off)),
                (Some(main), None) => Some(format!("{}: {}", label, main)),
                _ => None,
            }
        };
    if let Some(s) = fmt_set(&v.weapons.set1, "Set 1") {
        weapons.push(s);
    }
    if let Some(s) = fmt_set(&v.weapons.set2, "Set 2") {
        weapons.push(s);
    }

    let mut skills = Vec::new();
    if !v.legends.is_empty() {
        let names: Vec<String> = v
            .legends
            .iter()
            .map(|id| {
                db.legends
                    .get(id)
                    .and_then(|l| db.skills.get(&l.swap))
                    .map(|s| crate::ui::comparison::compact_stance_name(&s.name))
                    .unwrap_or_else(|| id.clone())
            })
            .collect();
        skills.push(format!("Stances: {}", names.join(" / ")));
    }
    if let Some((t1, t2, _, _)) = v.pets {
        let names: Vec<String> = [t1, t2]
            .into_iter()
            .flatten()
            .map(|id| db.pet_display_name(id))
            .collect();
        if !names.is_empty() {
            skills.push(format!("Pets: {}", names.join(" / ")));
        }
    }
    if let Some((_, name)) = &v.skills.heal {
        skills.push(format!("Heal: {}", name));
    }
    for (_, name) in v.skills.utilities.iter().flatten() {
        skills.push(format!("Utility: {}", name));
    }
    if let Some((_, name)) = &v.skills.elite {
        skills.push(format!("Elite: {}", name));
    }
    if !v.skills.profession.is_empty() {
        skills.push(format!(
            "Profession: {}",
            v.skills
                .profession
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }

    let sigils: Vec<String> = v.sigils.iter().map(|s| s.name.clone()).collect();

    // Convert stats from optimizer StatBlock (f64) to core StatBlock (i32)
    let derived = gw2_optimizer::stats::compute_derived(&result.stats, profession_name);
    let estimated_stats = Some(gw2_core::types::StatBlock {
        power: result.stats.power.round() as i32,
        precision: result.stats.precision.round() as i32,
        toughness: result.stats.toughness.round() as i32,
        vitality: result.stats.vitality.round() as i32,
        condition_damage: result.stats.condition_damage.round() as i32,
        expertise: result.stats.expertise.round() as i32,
        concentration: result.stats.concentration.round() as i32,
        ferocity: result.stats.ferocity.round() as i32,
        healing_power: result.stats.healing_power.round() as i32,
        crit_chance: derived.crit_chance,
        crit_damage: derived.crit_damage,
        health: derived.health.round() as i32,
        armor: derived.armor.round() as i32,
    });

    let combat_solo = Some(perf_to_combat_metrics(&result.combat_solo));
    let combat_party = Some(perf_to_combat_metrics(&result.combat_party));
    let combat_squad = Some(perf_to_combat_metrics(&result.combat_squad));

    let rotation = result
        .rotation
        .as_ref()
        .map(|sim| gw2_core::types::RotationBreakdown {
            simulated_dps: sim.total_dps.round() as i32,
            strike_dps: sim.strike_dps.round() as i32,
            condition_dps: sim.condition_dps.round() as i32,
            condition_uptime: sim
                .condition_uptime
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            buff_uptime: sim
                .buff_uptime
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            skill_usage: sim
                .skill_usage
                .iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        s.cast_count,
                        s.dps_contribution.round() as i32,
                    )
                })
                .collect(),
            stunbreak_count: sim.stunbreak_count,
            has_stability: sim.has_stability,
            stability_uptime: sim.stability_uptime,
            cleanse_count: sim.cleanse_count,
            cleanse_rate_per_20s: sim.cleanse_rate_per_20s,
        });

    let changes_made: Vec<String> = v
        .changes
        .iter()
        .map(|c| {
            if c.from.is_empty() {
                format!("[{}] → {} ({})", c.slot, c.to, c.reason)
            } else {
                format!("[{}] {} → {} ({})", c.slot, c.from, c.to, c.reason)
            }
        })
        .collect();

    // Warnings as additional info
    let mut explanation = v.explanation.clone();
    if !v.warnings.is_empty() {
        if !explanation.is_empty() {
            explanation.push_str("\n\n");
        }
        explanation.push_str("Warnings: ");
        explanation.push_str(&v.warnings.join("; "));
    }

    let primary_combat = match scenario.combat_tier {
        gw2_optimizer::scenario::CombatTier::Solo => &result.combat_solo,
        gw2_optimizer::scenario::CombatTier::Party => &result.combat_party,
        gw2_optimizer::scenario::CombatTier::Squad => &result.combat_squad,
    };
    let mut viability = gw2_optimizer::referee::evaluate_viability_gates(
        result.rotation.as_ref(),
        primary_combat,
        scenario,
    );
    gw2_optimizer::referee::apply_offbar_stability(&mut viability, v, db);
    let viability = Some(viability);

    // Suggestion label: label_override > role name > generic
    let label = label_override
        .or_else(|| role.map(|r| r.label().to_string()))
        .unwrap_or_else(|| "Optimized Build".to_string());

    let role_hint = role.map(|r| r.label().to_string()).unwrap_or_default();
    let our_score = {
        // Use normalised strike + condi DPS index as proxy score when referee score unavailable
        let s = &result.combat_solo;
        let strike_norm = s.strike_dps_index / 3000.0;
        let condi_norm = s.condition_dps_index / 3500.0;
        strike_norm.max(condi_norm).min(1.0)
    };
    let benchmark_delta = {
        let builds = gw2_optimizer::scraper::load_benchmarks(addon_dir);
        if builds.is_empty() {
            None
        } else {
            gw2_optimizer::benchmark::compute_benchmark_delta(
                &builds,
                profession_name,
                scenario.game_mode.label(),
                &role_hint,
                weights,
                our_score,
            )
        }
    };

    // Summary keeps the three-category shape (helm / amulet / set-1 main hand
    // as representatives); the per-piece rows read the full slot map.
    let fallback_prefix = v
        .primary_prefix()
        .map(|prefix| prefix.name.clone())
        .unwrap_or_else(|| "Unknown".into());
    let category_prefix = |slot: GearSlot| {
        v.prefix_for(slot)
            .map(|prefix| prefix.name.clone())
            .unwrap_or_else(|| fallback_prefix.clone())
    };
    let gear_summary = format!(
        "Armor: {} · Trinkets: {} · Weapons: {}",
        category_prefix(GearSlot::Helm),
        category_prefix(GearSlot::Amulet),
        category_prefix(GearSlot::WeaponSet1Main),
    );

    let mut suggestion = BuildSuggestion {
        // Ours, not published anywhere.
        source_url: String::new(),
        label,
        build_summary: format!("Gear: {gear_summary}"),
        stat_prefix: v
            .primary_prefix()
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        slot_prefixes: Some(v.gear_slots.clone()),
        specializations,
        weapons,
        skills,
        rune: v.rune.as_ref().map(|r| r.name.clone()).unwrap_or_default(),
        sigils,
        relic: v.relic.as_ref().map(|r| r.name.clone()).unwrap_or_default(),
        chat_code,
        explanation,
        synergy_explanation: v.synergy_explanation.clone(),
        changes_made,
        estimated_stats,
        combat_solo,
        combat_party,
        combat_squad,
        rotation,
        viability,
        benchmark_delta,
        data_quality: result.data_quality.clone(),
        quality_reasons: result
            .quality_reasons
            .iter()
            .map(|r| r.to_string())
            .collect(),
        coverage_note: coverage_note_from(&result.quality_reasons),
    };
    if suggestion.chat_code.is_none() {
        suggestion.chat_code = suggestion_to_chat_code(&suggestion, db);
    }
    suggestion
}

pub(super) fn validated_build_to_chat_code(
    build: &gw2_optimizer::validation::ValidatedBuild,
    profession_name: &str,
    db: &gw2_optimizer::gamedb::GameDb,
) -> Option<String> {
    let skills = gw2_api::models::SkillSelection {
        heal: build.skills.heal.as_ref().map(|(id, _)| *id),
        utilities: build
            .skills
            .utilities
            .iter()
            .take(3)
            .map(|skill| skill.as_ref().map(|(id, _)| *id))
            .collect(),
        elite: build.skills.elite.as_ref().map(|(id, _)| *id),
    };
    let pets = match build.pets {
        Some((t1, t2, a1, a2)) => Some(gw2_api::models::PetSelection {
            terrestrial: vec![t1, t2],
            aquatic: vec![a1, a2],
        }),
        // The `with_state` is here, at the call site, and not inside
        // `snapshot_ranger_pets`: see that function's note.
        None if profession_name == "Ranger" => {
            crate::state::with_state(snapshot_ranger_pets).flatten()
        }
        None => None,
    };
    let api_build = gw2_api::models::Build {
        name: None,
        profession: Some(profession_name.to_string()),
        specializations: build
            .specializations
            .iter()
            .map(|spec| gw2_api::models::SpecSelection {
                id: Some(spec.spec_id),
                traits: spec.trait_ids.iter().take(3).map(|id| Some(*id)).collect(),
            })
            .collect(),
        skills: Some(skills),
        // Land palettes in aquatic slots make GW2 reject the template.
        aquatic_skills: None,
        legends: build.legends.iter().map(|id| Some(id.clone())).collect(),
        aquatic_legends: {
            let src = if build.aquatic_legends.is_empty() {
                &build.legends
            } else {
                &build.aquatic_legends
            };
            src.iter().map(|id| Some(id.clone())).collect()
        },
        pets,
    };
    let weapons = [
        build.weapons.set1.main_hand.as_deref(),
        build.weapons.set1.off_hand.as_deref(),
        build.weapons.set2.main_hand.as_deref(),
        build.weapons.set2.off_hand.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_string)
    .collect::<Vec<_>>();

    super::character::generate_build_chat_code(&api_build, db, &weapons)
}

/// Pets from the player's selected build tab.
///
/// Takes the state it reads instead of reaching for the global `STATE` itself.
/// `STATE` is a plain `std::sync::Mutex`, so the previous `with_state` call
/// buried in this function deadlocked outright — not "contended", deadlocked —
/// the moment any caller of [`validated_build_to_chat_code`] ran inside a
/// `with_state` closure, which is the normal shape of every render-thread call
/// site in this crate. A hidden lock in a display adapter is a trap; an
/// argument is not.
fn snapshot_ranger_pets(
    state: &mut crate::state::AddonState,
) -> Option<gw2_api::models::PetSelection> {
    let index = state.main.selected_build_tab?;
    state
        .main
        .build_tabs
        .get(index)
        .and_then(|tab| tab.build.pets.clone())
}

pub(super) fn candidate_to_suggestion(
    candidate: &gw2_optimizer::engine::BuildCandidate,
    db: &gw2_optimizer::gamedb::GameDb,
    balance_ctx: &BalanceContext,
) -> crate::ui::comparison::BuildSuggestion {
    use crate::ui::comparison::BuildSuggestion;

    // Get spec names with actually selected traits (not all 9)
    let mut specializations = Vec::new();
    if let Some(elite_id) = candidate.elite_spec {
        if let Some(spec) = db.spec(elite_id) {
            let traits: Vec<String> = candidate
                .equipped_traits
                .iter()
                .filter(|tid| spec.major_traits.contains(tid))
                .filter_map(|&tid| db.traits.get(&tid).map(|t| t.name.clone()))
                .collect();
            specializations.push((format!("{} [E]", spec.name), traits));
        }
    }
    for &core_id in &candidate.core_specs {
        if let Some(spec) = db.spec(core_id) {
            let traits: Vec<String> = candidate
                .equipped_traits
                .iter()
                .filter(|tid| spec.major_traits.contains(tid))
                .filter_map(|&tid| db.traits.get(&tid).map(|t| t.name.clone()))
                .collect();
            specializations.push((spec.name.clone(), traits));
        }
    }

    let estimated_stats = Some(gw2_core::types::StatBlock {
        power: candidate.stats.power.round() as i32,
        precision: candidate.stats.precision.round() as i32,
        toughness: candidate.stats.toughness.round() as i32,
        vitality: candidate.stats.vitality.round() as i32,
        condition_damage: candidate.stats.condition_damage.round() as i32,
        expertise: candidate.stats.expertise.round() as i32,
        concentration: candidate.stats.concentration.round() as i32,
        ferocity: candidate.stats.ferocity.round() as i32,
        healing_power: candidate.stats.healing_power.round() as i32,
        crit_chance: candidate.derived.crit_chance,
        crit_damage: candidate.derived.crit_damage,
        health: candidate.derived.health.round() as i32,
        armor: candidate.derived.armor.round() as i32,
    });

    // Compute combat metrics for all 3 buff profiles.
    // Determine profession from the candidate's specs. The "Warrior" fallback
    // is only reached if the candidate has no specs at all, which a valid
    // BuildCandidate never has — kept here so combat math always has a
    // profession name. Previously this fell back to
    // `db.professions.values().next()`, whose order is unspecified.
    let prof_name = if let Some(elite_id) = candidate.elite_spec {
        db.spec(elite_id)
            .map(|s| s.profession.as_str())
            .unwrap_or("Warrior")
    } else if let Some(&core_id) = candidate.core_specs.first() {
        db.spec(core_id)
            .map(|s| s.profession.as_str())
            .unwrap_or("Warrior")
    } else {
        "Warrior"
    };

    let (combat_solo, combat_party, combat_squad) = compute_3tier_combat(
        &candidate.stats,
        &candidate.derived,
        &candidate.modifiers,
        prof_name,
        balance_ctx,
    );

    // Legacy path: no rotation available, rotation-dependent gates produce degraded state.
    // Use a simple EHP proxy from vitality for the viability check.
    let legacy_viability = {
        let scenario = gw2_optimizer::scenario::ScenarioSpec::from_balance_context(balance_ctx);
        let proxy_perf = gw2_optimizer::combat::CombatPerformance {
            effective_health: candidate.stats.vitality * 10.0,
            ..Default::default()
        };
        gw2_optimizer::referee::evaluate_viability_gates(None, &proxy_perf, &scenario)
    };

    let mut suggestion = BuildSuggestion {
        // Ours, not published anywhere.
        source_url: String::new(),
        label: format!("Score: {:.2}", candidate.score),
        build_summary: format!("Gear: {}", candidate.gear.stat_prefix_name),
        stat_prefix: candidate.gear.stat_prefix_name.clone(),
        slot_prefixes: Some(candidate.gear.gear_slots.clone()),
        specializations,
        weapons: Vec::new(),
        skills: Vec::new(),
        rune: String::new(),
        sigils: Vec::new(),
        relic: String::new(),
        chat_code: None,
        explanation: String::new(),
        synergy_explanation: String::new(),
        changes_made: Vec::new(),
        estimated_stats,
        combat_solo,
        combat_party,
        combat_squad,
        rotation: None,
        viability: Some(legacy_viability),
        benchmark_delta: None,
        data_quality: leftover_plate_quality(true),
        quality_reasons: vec!["legacy leftover kit has no weapons or skills".into()],
        coverage_note: None,
    };
    suggestion.chat_code = suggestion_to_chat_code(&suggestion, db);
    suggestion
}

/// Leftover `BuildCandidate` plates never carry weapons/skills. Do not stamp Verified.
fn leftover_plate_quality(empty_kit: bool) -> gw2_optimizer::data::DataQuality {
    if empty_kit {
        gw2_optimizer::data::DataQuality::Blocked
    } else {
        gw2_optimizer::data::DataQuality::Verified
    }
}

/// Run rotation simulation for a suggestion's skills and attach the results.
///
/// Resolves ALL build skills: weapon skills from both weapon sets (tagged for
/// weapon swap scheduling) + heal/utility/elite from the skills list.
/// The simulator uses DPCT-optimal scheduling with automatic weapon swapping.
pub(super) fn simulate_suggestion_rotation(
    suggestion: &mut crate::ui::comparison::BuildSuggestion,
    db: &gw2_optimizer::gamedb::GameDb,
    balance_ctx: &BalanceContext,
) {
    if suggestion.skills.is_empty() && suggestion.weapons.is_empty() {
        return;
    }

    let mut all_rotation_skills: Vec<gw2_optimizer::rotation::RotationSkill> = Vec::new();

    // 1. Resolve weapon skills from suggestion.weapons (format: "Set 1: Axe / Axe").
    //
    // Use the pre-built `skills_by_profession` index instead of scanning all
    // ~500 skills per (profession × weapon set × weapon type) — that scan was
    // also nondeterministic across runs because `db.skills.values()` iteration
    // order is unspecified.
    if !suggestion.weapons.is_empty() {
        let profession = infer_profession_from_specs(&suggestion.specializations, db);
        let weapon_sets = parse_weapon_sets(&suggestion.weapons);
        let prof_skill_ids = db.skills_by_profession.get(profession.as_str());

        for (set_num, weapon_types) in &weapon_sets {
            let mut set_skill_ids: Vec<u32> = Vec::new();
            if let Some(ids) = prof_skill_ids {
                for &id in ids {
                    let Some(skill) = db.skills.get(&id) else {
                        continue;
                    };
                    let matches_weapon = weapon_types.iter().any(|wt| {
                        skill.weapon_type.as_deref().is_some_and(|swt| {
                            gw2_core::i18n::weapon_type_key(swt)
                                == gw2_core::i18n::weapon_type_key(wt)
                        })
                    });
                    if !matches_weapon {
                        continue;
                    }
                    let is_weapon_slot = skill
                        .slot
                        .as_deref()
                        .map(|s| s.starts_with("Weapon_"))
                        .unwrap_or(false);
                    if !is_weapon_slot {
                        continue;
                    }
                    if !set_skill_ids.contains(&skill.id) {
                        set_skill_ids.push(skill.id);
                    }
                }
            }
            if !set_skill_ids.is_empty() {
                let mut set_skills =
                    gw2_optimizer::rotation::builder::build_rotation_skills_for_context(
                        &set_skill_ids,
                        db,
                        balance_ctx,
                    );
                gw2_optimizer::rotation::builder::tag_weapon_set(&mut set_skills, *set_num);
                all_rotation_skills.extend(set_skills);
            }
        }
    }

    // 2. Resolve heal/utility/elite from suggestion.skills.
    //    Format: "Heal: Name", "Utils: Name1, Name2, Name3", "Elite: Name".
    //
    // Walk skills_by_profession (sorted, scoped) instead of all db.skills —
    // deterministic order plus faster than the ~500-entry scan. We still need
    // exact-name match so the smaller candidate set is iterated linearly.
    let skill_names = parse_skill_names(&suggestion.skills);
    if !skill_names.is_empty() {
        let profession = infer_profession_from_specs(&suggestion.specializations, db);
        let prof_skill_ids = db.skills_by_profession.get(profession.as_str());
        // Hoist the sorted skill-id list once so the global fallback below
        // doesn't re-collect-and-sort `db.skills.keys()` (~500 ids) per skill
        // name. Only allocated when at least one name will be searched.
        let mut all_skill_ids_sorted: Option<Vec<u32>> = None;
        for name in &skill_names {
            let found_skill = prof_skill_ids.and_then(|ids| {
                ids.iter()
                    .filter_map(|id| db.skills.get(id))
                    .find(|s| s.name.eq_ignore_ascii_case(name))
            });
            // Fallback: scan all skills if the profession index missed (e.g.
            // shared utility-like skills not registered under profession).
            // Iterate by id so a name with multiple matches (e.g. "Bandage")
            // resolves to the same skill across runs — `HashMap::values()`
            // order is unspecified.
            let skill = found_skill.or_else(|| {
                let ids = all_skill_ids_sorted.get_or_insert_with(|| {
                    let mut v: Vec<u32> = db.skills.keys().copied().collect();
                    v.sort_unstable();
                    v
                });
                ids.iter()
                    .filter_map(|id| db.skills.get(id))
                    .find(|s| s.name.eq_ignore_ascii_case(name))
            });
            if let Some(skill) = skill {
                if !all_rotation_skills.iter().any(|rs| rs.skill_id == skill.id) {
                    let mut rs_vec =
                        gw2_optimizer::rotation::builder::build_rotation_skills_for_context(
                            &[skill.id],
                            db,
                            balance_ctx,
                        );
                    // Non-weapon skills stay at weapon_set=0 (always available)
                    all_rotation_skills.append(&mut rs_vec);
                }
            }
        }
    }

    if all_rotation_skills.is_empty() {
        return;
    }

    // Extract stats from estimated_stats for the simulation
    let stats = suggestion.estimated_stats.as_ref();
    let power = stats.map(|s| s.power as f64).unwrap_or(1000.0);
    let condition_damage = stats.map(|s| s.condition_damage as f64).unwrap_or(0.0);
    let weapon_strength = 1100.0; // reference weapon strength (same as combat.rs)

    let mode = balance_ctx.game_mode.clone();
    let result = gw2_optimizer::rotation::simulator::simulate_with(
        &all_rotation_skills,
        0,
        &gw2_optimizer::rotation::simulator::SimParams {
            power,
            condition_damage,
            weapon_strength,
            precision: stats.map(|s| s.precision as f64).unwrap_or(1000.0),
            ferocity: stats.map(|s| s.ferocity as f64).unwrap_or(0.0),
            crit_chance_bonus: 0.0,
            fury_crit_chance_bonus: gw2_optimizer::data::boon_condition_formulas::boons()
                .fury_crit_bonus(mode.clone())
                * 100.0,
            strike_mult: 1.0,
            condition_mult: 1.0,
            condition_duration_mult: 1.0
                + stats.map(|s| s.expertise as f64).unwrap_or(0.0) / 1500.0,
            boon_duration_mult: 1.0 + stats.map(|s| s.concentration as f64).unwrap_or(0.0) / 1500.0,
            healing_power: stats.map(|s| s.healing_power as f64).unwrap_or(0.0),
            healing_mult: 1.0,
            max_health: stats.map(|s| s.health as f64).unwrap_or(19_212.0),
            armor: stats.map(|s| s.armor as f64).unwrap_or(2_597.0),
            mode,
            intent: None,
            deferred_target: Vec::new(),
            weaver: suggestion
                .specializations
                .iter()
                .any(|(label, _)| label.replace(" [E]", "").eq_ignore_ascii_case("Weaver")),
        },
        gw2_optimizer::rotation::combat_model::EnemyDummy::default(),
    );

    suggestion.rotation = Some(gw2_core::types::RotationBreakdown {
        simulated_dps: result.total_dps.round() as i32,
        strike_dps: result.strike_dps.round() as i32,
        condition_dps: result.condition_dps.round() as i32,
        condition_uptime: result.condition_uptime.into_iter().collect(),
        buff_uptime: result.buff_uptime.into_iter().collect(),
        skill_usage: result
            .skill_usage
            .iter()
            .map(|su| {
                (
                    su.name.clone(),
                    su.cast_count,
                    su.dps_contribution.round() as i32,
                )
            })
            .collect(),
        stunbreak_count: result.stunbreak_count,
        has_stability: result.has_stability,
        stability_uptime: result.stability_uptime,
        cleanse_count: result.cleanse_count,
        cleanse_rate_per_20s: result.cleanse_rate_per_20s,
    });
}

/// Parse weapon sets from suggestion.weapons strings.
/// Input format: "Set 1: Axe / Axe", "Set 2: Greatsword"
/// Returns: [(1, ["Axe", "Axe"]), (2, ["Greatsword"])]
fn parse_weapon_sets(weapons: &[String]) -> Vec<(u8, Vec<String>)> {
    let mut sets = Vec::new();
    for w in weapons {
        let set_num = if w.starts_with("Set 1") {
            1u8
        } else if w.starts_with("Set 2") {
            2u8
        } else {
            1u8
        }; // fallback

        let rest = w.split(':').nth(1).unwrap_or(w).trim();
        let types: Vec<String> = rest
            .split('/')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "null")
            .collect();

        if !types.is_empty() {
            sets.push((set_num, types));
        }
    }
    sets
}

/// Parse individual skill names from formatted suggestion.skills strings.
/// "Heal: Mending" → "Mending"
/// "Utils: Blood Reckoning, Bull's Charge, Signet of Fury" → 3 names
/// "Elite: Head Butt" → "Head Butt"
fn parse_skill_names(skills: &[String]) -> Vec<String> {
    let parsed = crate::ui::gear_diff::parse_suggestion_skills(skills);
    let mut names = Vec::new();
    if !parsed.heal.is_empty() {
        names.push(parsed.heal);
    }
    names.extend(parsed.utilities);
    if !parsed.elite.is_empty() {
        names.push(parsed.elite);
    }
    names
}

/// Infer profession name from specialization names in the suggestion.
fn infer_profession_from_specs(
    specs: &[(String, Vec<String>)],
    db: &gw2_optimizer::gamedb::GameDb,
) -> String {
    // Walk specializations in id order so name collisions across
    // professions (defensive — GW2 currently has unique spec names but
    // data drift could introduce duplicates) resolve to the same
    // profession across runs and machines. `HashMap::values()` order is
    // unspecified.
    let mut spec_ids: Vec<u32> = db.specializations.keys().copied().collect();
    spec_ids.sort_unstable();
    for (spec_name, _) in specs {
        let clean = spec_name.replace(" [E]", "");
        for sid in &spec_ids {
            if let Some(spec) = db.specializations.get(sid) {
                if spec.name.eq_ignore_ascii_case(&clean) {
                    return spec.profession.clone();
                }
            }
        }
    }
    // Fallback: return empty string. The previous
    // `db.professions.values().next()` picked a random profession from
    // HashMap iteration order — non-deterministic and almost certainly the
    // wrong profession anyway. Callers downstream that key on the profession
    // (e.g. `skills_by_profession.get(name)`) will simply find nothing, which
    // is the correct outcome when we cannot infer.
    String::new()
}

/// Encode a displayed suggestion as a GW2 build-template chat code.
/// Save/load stores names, not IDs — resolve against GameDb at encode time.
pub(super) fn suggestion_to_chat_code(
    suggestion: &crate::ui::comparison::BuildSuggestion,
    db: &gw2_optimizer::gamedb::GameDb,
) -> Option<String> {
    use gw2_api::models::{Build, SpecSelection};

    let profession = infer_profession_from_specs(&suggestion.specializations, db);
    if profession.is_empty() {
        return None;
    }

    let mut specializations = Vec::new();
    for (spec_name, trait_names) in &suggestion.specializations {
        let Some(spec) = spec_by_display_name(db, spec_name) else {
            continue;
        };
        let trait_ids: Vec<Option<u32>> = trait_names
            .iter()
            .take(3)
            .map(|name| {
                db.traits_by_spec.get(&spec.id).and_then(|ids| {
                    ids.iter()
                        .filter_map(|id| db.traits.get(id))
                        .find(|t| t.name.eq_ignore_ascii_case(name))
                        .map(|t| t.id)
                })
            })
            .collect();
        specializations.push(SpecSelection {
            id: Some(spec.id),
            traits: trait_ids,
        });
    }

    let skills = skill_selection_from_suggestion(&suggestion.skills, db, &profession);
    let pets = pet_selection_from_suggestion(&suggestion.skills);

    let api_build = Build {
        name: None,
        profession: Some(profession),
        specializations,
        skills: Some(skills),
        // Land palettes in aquatic slots make GW2 reject the template.
        aquatic_skills: None,
        legends: vec![],
        aquatic_legends: vec![],
        pets,
    };

    let weapons: Vec<String> = parse_weapon_sets(&suggestion.weapons)
        .into_iter()
        .flat_map(|(_, types)| types)
        .collect();

    super::character::generate_build_chat_code(&api_build, db, &weapons)
}

fn spec_by_display_name<'a>(
    db: &'a gw2_optimizer::gamedb::GameDb,
    name: &str,
) -> Option<&'a gw2_api::models::Specialization> {
    let clean = name.replace(" [E]", "");
    let mut ids: Vec<u32> = db.specializations.keys().copied().collect();
    ids.sort_unstable();
    for sid in ids {
        if let Some(spec) = db.specializations.get(&sid) {
            if spec.name.eq_ignore_ascii_case(&clean) {
                return Some(spec);
            }
        }
    }
    None
}

fn skill_id_by_name(
    db: &gw2_optimizer::gamedb::GameDb,
    profession: &str,
    name: &str,
) -> Option<u32> {
    if let Some(ids) = db.skills_by_profession.get(profession) {
        for &id in ids {
            if let Some(skill) = db.skills.get(&id) {
                if skill.name.eq_ignore_ascii_case(name) {
                    return Some(skill.id);
                }
            }
        }
    }
    let mut ids: Vec<u32> = db.skills.keys().copied().collect();
    ids.sort_unstable();
    ids.iter().find_map(|id| {
        db.skills
            .get(id)
            .filter(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| s.id)
    })
}

fn skill_selection_from_suggestion(
    skills: &[String],
    db: &gw2_optimizer::gamedb::GameDb,
    profession: &str,
) -> gw2_api::models::SkillSelection {
    let parsed = crate::ui::gear_diff::parse_suggestion_skills(skills);
    let heal = if parsed.heal.is_empty() {
        None
    } else {
        skill_id_by_name(db, profession, parsed.heal.trim())
    };
    let mut utilities: Vec<Option<u32>> = parsed
        .utilities
        .iter()
        .map(|name| skill_id_by_name(db, profession, name))
        .collect();
    utilities.truncate(3);
    while utilities.len() < 3 {
        utilities.push(None);
    }
    let elite = if parsed.elite.is_empty() {
        None
    } else {
        skill_id_by_name(db, profession, parsed.elite.trim())
    };
    gw2_api::models::SkillSelection {
        heal,
        utilities,
        elite,
    }
}

fn pet_selection_from_suggestion(skills: &[String]) -> Option<gw2_api::models::PetSelection> {
    let pets = crate::ui::gear_diff::parse_suggestion_skills(skills).pets;
    if pets.is_empty() {
        return None;
    }
    let mut ids = Vec::new();
    for part in pets.split('/') {
        let t = part.trim().trim_start_matches('#');
        if let Ok(id) = t.parse::<u32>() {
            ids.push(Some(id));
        }
    }
    if ids.is_empty() {
        return None;
    }
    let t1 = ids.first().copied().flatten();
    let t2 = ids.get(1).copied().flatten();
    Some(gw2_api::models::PetSelection {
        terrestrial: vec![t1, t2],
        aquatic: vec![],
    })
}

/// Summarize a ResolvedBuild as text for LLM prompts.
pub(super) fn summarize_resolved_build(build: &gw2_core::types::ResolvedBuild) -> String {
    let mut parts = Vec::new();

    parts.push(format!("Profession: {}", build.profession));

    let specs: Vec<String> = build
        .specializations
        .iter()
        .map(|s| {
            let elite = if s.elite { " [E]" } else { "" };
            let traits: Vec<&str> = s.traits_selected.iter().map(|t| t.name.as_str()).collect();
            format!("{}{}: {}", s.name, elite, traits.join(", "))
        })
        .collect();
    if !specs.is_empty() {
        parts.push(format!("Specs: {}", specs.join(" | ")));
    }

    if let Some(ref h) = build.skills.heal {
        parts.push(format!("Heal: {}", h.name));
    }
    let utils: Vec<String> = build
        .skills
        .utilities
        .iter()
        .filter_map(|u| u.as_ref().map(|s| s.name.clone()))
        .collect();
    if !utils.is_empty() {
        parts.push(format!("Utils: {}", utils.join(", ")));
    }
    if let Some(ref e) = build.skills.elite {
        parts.push(format!("Elite: {}", e.name));
    }
    if !build.pets.is_empty() {
        parts.push(format!("Pets: {}", build.pets.join(" / ")));
    }

    for set in &build.weapons {
        let mut w = Vec::new();
        if let Some(ref mh) = set.main_hand {
            w.push(mh.weapon_type.clone());
        }
        if let Some(ref oh) = set.off_hand {
            w.push(oh.weapon_type.clone());
        }
        if !w.is_empty() {
            parts.push(format!("{}: {}", set.label, w.join(" / ")));
        }
    }

    if !build.armor.is_empty() && !build.armor[0].stat_prefix.is_empty() {
        parts.push(format!("Gear: {}", build.armor[0].stat_prefix));
    }
    if let Some(ref r) = build.rune {
        parts.push(format!("Rune: {}", r.name));
    }
    if let Some(ref r) = build.relic {
        parts.push(format!("Relic: {}", r.name));
    }

    parts.join("\n")
}

/// Apply Gemini's parsed response onto a BuildSuggestion.
pub(super) fn apply_gemini_response(
    suggestion: &mut crate::ui::comparison::BuildSuggestion,
    gemini: &gw2_optimizer::prompts::GeminiBuildResponse,
) {
    if !gemini.explanation.is_empty() {
        suggestion.explanation = gemini.explanation.clone();
    }
    if let Some(ref synergy) = gemini.synergy_explanation {
        if !synergy.is_empty() {
            suggestion.synergy_explanation = synergy.clone();
        }
    }
    if !gemini.specializations.is_empty() {
        suggestion.specializations = gemini.specializations.clone();
    }
    if !gemini.weapons.is_empty() {
        suggestion.weapons = gemini.weapons.clone();
    }
    if !gemini.skills.is_empty() {
        suggestion.skills = gemini.skills.clone();
    }
    if !gemini.rune.is_empty() {
        suggestion.rune = gemini.rune.clone();
    }
    if !gemini.sigils.is_empty() {
        suggestion.sigils = gemini.sigils.clone();
    }
    if !gemini.relic.is_empty() {
        suggestion.relic = gemini.relic.clone();
    }
    if !gemini.stat_prefix.is_empty() {
        suggestion.stat_prefix = gemini.stat_prefix.clone();
    }
    if !gemini.changes_made.is_empty() {
        suggestion.changes_made = gemini.changes_made.clone();
    }
}

pub(super) fn attach_chat_stats(
    suggestion: &mut crate::ui::comparison::BuildSuggestion,
    db: &gw2_optimizer::gamedb::GameDb,
    profession: &str,
    game_mode: &gw2_core::types::GameMode,
    validated: Option<&gw2_optimizer::validation::ValidatedBuild>,
) {
    let balance_ctx = BalanceContext::new(game_mode.clone());
    let (full, modifiers) = if let Some(validated) = validated {
        let (full, modifiers) = gw2_optimizer::engine::calculate_validated_stats(
            validated,
            db,
            profession,
            &balance_ctx,
        );
        for warning in &validated.warnings {
            if !suggestion.quality_reasons.iter().any(|r| r == warning) {
                suggestion.quality_reasons.push(warning.clone());
            }
        }
        for reason in
            gw2_optimizer::engine::gear_quality_reasons(validated, db, profession, &balance_ctx)
        {
            let text = reason.to_string();
            if !suggestion.quality_reasons.iter().any(|r| r == &text) {
                suggestion.quality_reasons.push(text);
            }
        }
        (full, modifiers)
    } else {
        if suggestion.stat_prefix.is_empty() {
            return;
        }
        let Some((_name, full, _derived)) = gw2_optimizer::gemini_tools::estimate_prefix_stats_in(
            db,
            &suggestion.stat_prefix,
            profession,
            &balance_ctx,
        ) else {
            return;
        };
        (full, gw2_optimizer::combat::DamageModifiers::default())
    };
    let derived = gw2_optimizer::stats::compute_derived(&full, profession);
    suggestion.estimated_stats = Some(gw2_core::types::StatBlock {
        power: full.power.round() as i32,
        precision: full.precision.round() as i32,
        toughness: full.toughness.round() as i32,
        vitality: full.vitality.round() as i32,
        condition_damage: full.condition_damage.round() as i32,
        expertise: full.expertise.round() as i32,
        concentration: full.concentration.round() as i32,
        ferocity: full.ferocity.round() as i32,
        healing_power: full.healing_power.round() as i32,
        crit_chance: derived.crit_chance,
        crit_damage: derived.crit_damage,
        health: derived.health.round() as i32,
        armor: derived.armor.round() as i32,
    });
    let (solo, party, squad) =
        compute_3tier_combat(&full, &derived, &modifiers, profession, &balance_ctx);
    suggestion.combat_solo = solo;
    suggestion.combat_party = party;
    suggestion.combat_squad = squad;
}

/// Merge validator-resolved names onto the raw LLM tasting so the plate is edible.
pub(super) fn gemini_from_validated(
    mut raw: gw2_optimizer::prompts::GeminiBuildResponse,
    v: &gw2_optimizer::validation::ValidatedBuild,
) -> gw2_optimizer::prompts::GeminiBuildResponse {
    if !v.specializations.is_empty() {
        raw.specializations = v
            .specializations
            .iter()
            .map(|s| (s.name.clone(), s.trait_names.clone()))
            .collect();
    }
    let mut weapons = Vec::new();
    let fmt_set =
        |set: &gw2_optimizer::validation::ValidatedWeaponSet, label: &str| -> Option<String> {
            match (&set.main_hand, &set.off_hand) {
                (Some(main), Some(off)) => Some(format!("{}: {} / {}", label, main, off)),
                (Some(main), None) => Some(format!("{}: {}", label, main)),
                _ => None,
            }
        };
    if let Some(s) = fmt_set(&v.weapons.set1, "Set 1") {
        weapons.push(s);
    }
    if let Some(s) = fmt_set(&v.weapons.set2, "Set 2") {
        weapons.push(s);
    }
    if !weapons.is_empty() {
        raw.weapons = weapons;
    }
    let mut skills = Vec::new();
    if !v.legends.is_empty() {
        skills.push(format!("Stances: {}", v.legends.join(" / ")));
    }
    // Same contract as keep_loadout_pets: a Ranger plate must not lose Pets.
    // Chat fill_holes_from_loadout inserts the row; this rebuild used to drop it.
    if let Some(row) = raw
        .skills
        .iter()
        .find(|s| s.get(..6).is_some_and(|h| h.eq_ignore_ascii_case("Pets: ")))
    {
        skills.push(row.clone());
    } else if let Some(slots) = &raw.pets {
        let names: Vec<String> = slots.iter().flatten().cloned().collect();
        if !names.is_empty() {
            skills.push(format!("Pets: {}", names.join(" / ")));
        }
    } else if let Some((t1, t2, _, _)) = v.pets {
        let ids: Vec<String> = [t1, t2]
            .into_iter()
            .flatten()
            .map(|id| format!("#{id}"))
            .collect();
        if !ids.is_empty() {
            skills.push(format!("Pets: {}", ids.join(" / ")));
        }
    }
    if let Some((_, name)) = &v.skills.heal {
        skills.push(format!("Heal: {}", name));
    }
    for (_, name) in v.skills.utilities.iter().flatten() {
        skills.push(format!("Utility: {}", name));
    }
    if let Some((_, name)) = &v.skills.elite {
        skills.push(format!("Elite: {}", name));
    }
    if !skills.is_empty() {
        raw.skills = skills;
    }
    if let Some(r) = &v.rune {
        raw.rune = r.name.clone();
    }
    if !v.sigils.is_empty() {
        raw.sigils = v.sigils.iter().map(|s| s.name.clone()).collect();
    }
    if let Some(r) = &v.relic {
        raw.relic = r.name.clone();
    }
    if let Some(p) = v.primary_prefix() {
        raw.stat_prefix = p.name.clone();
    }
    if !v.explanation.is_empty() {
        raw.explanation = v.explanation.clone();
    }
    if !v.synergy_explanation.is_empty() {
        raw.synergy_explanation = Some(v.synergy_explanation.clone());
    }
    if !v.changes.is_empty() {
        raw.changes_made = v
            .changes
            .iter()
            .map(|c| {
                if c.from.is_empty() {
                    format!("[{}] → {} ({})", c.slot, c.to, c.reason)
                } else {
                    format!("[{}] {} → {} ({})", c.slot, c.from, c.to, c.reason)
                }
            })
            .collect();
    }
    raw
}

pub(super) fn keep_equipped_weapons(msg: &str) -> bool {
    let m = msg.to_lowercase();
    if !m.contains("weapon") {
        return false;
    }
    m.contains("keep")
        || m.contains("same")
        || m.contains("don't change")
        || m.contains("dont change")
}

pub(super) fn kitchen_brief(
    game_mode: &str,
    scale: &str,
    role: &str,
    role_brief: &str,
    character: &str,
    on_the_pass: &str,
    keep_weapons: bool,
) -> String {
    let keep = if keep_weapons {
        "Keep equipped weapons — write both set1 and set2 from Character; do not omit them.\n"
    } else {
        "Always write weapons.set1 and weapons.set2. If they stay the same, copy both from Character.\n"
    };
    format!(
        "Mode: {game_mode}\nScale: {scale}\nRole: {role}\n{role_brief}\n{keep}Character:\n{character}\nOn the pass:\n{pass}\nNote: get_optimizer_results is empty unless Optimize ran; cook from this brief and the dish on the pass.",
        pass = on_the_pass,
    )
}

pub(super) fn apply_radar_prefix(
    parsed: &mut gw2_optimizer::prompts::GeminiBuildResponse,
    _weights: &gw2_optimizer::scoring::OptimizationWeights,
    order: &str,
) {
    // Choya is a conversation. A prefix the player ASKED for wins; otherwise keep
    // the LLM's pick. Radar is a starting prior for Optimize, not a cage for chat.
    //
    // `prefix_named_in_text` only inspects the single word before the stem and
    // knows four negations, so it still reports an affirmative match for
    // "don't use minstrel" or "stop suggesting minstrel". Re-check the mention
    // here: a prefix the player is pushing away must not be forced onto them.
    // "add SOME plaguedoctor in there" is not "make it all plaguedoctor".
    // Forcing `stat_prefix` here paints every worn slot (`fill_worn_gear_slots`),
    // so a partial request must leave the model's base prefix alone and let the
    // named one land only where `gear_slots` puts it.
    if let Some(named) = gw2_optimizer::scoring::prefix_named_in_text(order) {
        if prefix_request_is_affirmative(order, named) && !prefix_request_is_partial(order, named) {
            parsed.stat_prefix = named.to_string();
        }
    }
}

/// Words that only carry a request along, and never flip its sense. Walked
/// over backwards from the prefix mention so a rejection cue a few words
/// earlier ("don't *give me* minstrel") is still seen, while a cue belonging to
/// a *different* prefix ("not celestial, minstrel please") is not.
const REQUEST_FILLERS: &str = "a an any at for gear give giving go going in it me more my need nt      of on our please prefix put recommend recommending run running some stat stats suggest      suggesting t take taking that the this to us use using want wanting with";

/// Words that turn a prefix mention into a rejection. Wider than the four
/// `scoring::prefix_named_in_text` knows, which is why that function still
/// reports "don't use minstrel" as an affirmative Minstrel's.
const REQUEST_NEGATIONS: &str = "anti avoid avoiding cannot cant doesn doesnt don dont drop      exclude excluding except forget hate instead never no not skip stop than unless without";

/// Words that mark a request as PARTIAL — the named prefix goes on SOME pieces,
/// not painted over the whole kit. `some` is also a REQUEST_FILLER, and that is
/// deliberate: it must keep carrying the request along for the affirmative walk
/// ("add some plaguedoctor" is still a request FOR plaguedoctor), while
/// separately marking it as a mix. Reading it only as a filler is what turned
/// "add some plaguedoctor stats in there" into sixteen Plaguedoctor slots.
const REQUEST_MIX_CUES: &str = "bit blend couple few hybrid little mix mixed mixing partial \
     partially piece pieces several some splash sprinkle sprinkling touch";

/// Whitespace-separated word list membership.
fn listed(list: &str, word: &str) -> bool {
    list.split_whitespace().any(|entry| entry == word)
}

/// Lowercased alphanumeric words of `text`; every other character is a break.
/// Shared by the affirmative and partial checks so they cannot disagree about
/// where a mention is.
fn request_words(text: &str) -> Vec<String> {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// The singular and plural forms a prefix mention can take, normalised the same
/// way [`request_words`] normalises the order.
fn prefix_stems(prefix: &str) -> (String, String) {
    let stem = request_words(prefix.trim_end_matches("'s")).join(" ");
    let plural = format!("{stem}s");
    (stem, plural)
}

/// Is `prefix` something the player asked FOR in `order`, rather than something
/// they pushed away?
///
/// True when at least one mention of the prefix is affirmative — "not celestial,
/// give me minstrel" is a minstrel request. Matches the same normalisation
/// `gw2_optimizer::scoring::prefix_named_in_text` uses (ASCII alphanumerics
/// only, lowercased, space padded) so the two agree on where a mention is.
// ponytail: `scoring::GEAR_PROFILES` is private, so this can only re-check the
// one name `prefix_named_in_text` returned. "don't use minstrel, give me
// celestial" therefore falls back to the model's pick instead of promoting
// Celestial. Export the profile list and scan all names when `scoring.rs` is
// next open.
fn prefix_request_is_affirmative(order: &str, prefix: &str) -> bool {
    let words = request_words(order);
    let (stem, plural) = prefix_stems(prefix);
    let stem = stem.as_str();

    for (index, word) in words.iter().enumerate() {
        if word != stem && word != &plural {
            continue;
        }
        // Walk back over carrier words; the first word with meaning decides.
        let decisive = words[..index]
            .iter()
            .rev()
            .find(|w| !listed(REQUEST_FILLERS, w));
        match decisive {
            Some(word) if listed(REQUEST_NEGATIONS, word) => continue,
            _ => return true,
        }
    }
    // Every mention was a rejection — or the two normalisations disagreed on
    // where the mention is, in which case the model's own pick is the safer
    // answer than a forced overwrite.
    false
}

/// Did the player ask for the prefix on SOME pieces rather than the whole kit?
///
/// Same backwards walk as [`prefix_request_is_affirmative`], looking for a mix
/// cue instead of a negation: carrier words are stepped over, and a cue found in
/// that run marks the request partial. "add some plaguedoctor in there" is
/// partial; "make it plaguedoctor" is not.
///
/// A partial request is still affirmative — the player does want that prefix.
/// It only stops `apply_radar_prefix` force-setting `stat_prefix`, because that
/// is what paints all sixteen worn slots. Where the prefix actually lands is
/// then up to the `gear_slots` map, which `validate_gear_slot_map` already
/// applies on top of the base prefix.
fn prefix_request_is_partial(order: &str, prefix: &str) -> bool {
    let words = request_words(order);
    let (stem, plural) = prefix_stems(prefix);
    let stem = stem.as_str();

    for (index, word) in words.iter().enumerate() {
        if word != stem && word != &plural {
            continue;
        }
        // Walk back over carrier words only. A cue that belongs to a different
        // clause ("some celestial, then plaguedoctor") sits behind a
        // non-carrier word and is correctly not seen.
        for candidate in words[..index].iter().rev() {
            if listed(REQUEST_MIX_CUES, candidate) {
                return true;
            }
            if !listed(REQUEST_FILLERS, candidate) {
                break;
            }
        }
    }
    false
}

fn loadout_weapon_line(set: &gw2_core::types::ResolvedWeaponSet) -> Option<String> {
    let main = set
        .main_hand
        .as_ref()
        .map(|w| w.weapon_type.as_str())
        .filter(|s| !s.is_empty())?;
    match set.off_hand.as_ref().map(|w| w.weapon_type.as_str()) {
        Some(off) if !off.is_empty() => Some(format!("{}: {main} / {off}", set.label)),
        _ => Some(format!("{}: {main}", set.label)),
    }
}

/// Same assignment as `parse_weapon_sets_from_response`: Set 1 / unlabeled first → set1.
fn plate_weapon_slots(weapons: &[String]) -> (bool, bool) {
    let mut s1 = false;
    let mut s2 = false;
    for w in weapons {
        let label = w.split(':').next().unwrap_or("").trim();
        if label.contains('1') {
            s1 = true;
        } else if label.contains('2') {
            s2 = true;
        } else if !s1 {
            s1 = true;
        } else {
            s2 = true;
        }
    }
    (s1, s2)
}

pub(super) fn fill_holes_from_loadout(
    parsed: &mut gw2_optimizer::prompts::GeminiBuildResponse,
    current: &gw2_core::types::ResolvedBuild,
) {
    if parsed.specializations.is_empty() {
        return;
    }
    for (spec_name, traits) in &mut parsed.specializations {
        if traits.len() >= 3 {
            continue;
        }
        let clean = spec_name.replace(" [E]", "");
        let Some(cur) = current
            .specializations
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(clean.trim()))
        else {
            continue;
        };
        let mut extras: Vec<(usize, String)> = cur
            .traits_selected
            .iter()
            .filter(|t| t.selected && t.column < 3)
            .map(|t| (t.column, t.name.clone()))
            .collect();
        for (col, opts) in cur.traits_available.iter().enumerate() {
            if extras.iter().any(|(c, _)| *c == col) {
                continue;
            }
            if let Some(o) = opts.iter().find(|o| o.selected) {
                extras.push((col, o.name.clone()));
            }
        }
        extras.sort_by_key(|(col, _)| *col);
        for (_, name) in extras {
            if traits.len() >= 3 {
                break;
            }
            if !traits.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                traits.push(name);
            }
        }
    }

    let blob = parsed.skills.join("\n").to_lowercase();
    if !parsed
        .skills
        .iter()
        .any(|s| s.get(..6).is_some_and(|h| h.eq_ignore_ascii_case("Pets: ")))
        && !current.pets.is_empty()
    {
        parsed
            .skills
            .insert(0, format!("Pets: {}", current.pets.join(" / ")));
    }
    if !parsed
        .skills
        .iter()
        .any(|s| s.get(..5).is_some_and(|h| h.eq_ignore_ascii_case("Heal:")))
    {
        if let Some(h) = &current.skills.heal {
            parsed.skills.insert(0, format!("Heal: {}", h.name));
        }
    }
    for u in current.skills.utilities.iter().flatten() {
        if blob.contains(&u.name.to_lowercase()) {
            continue;
        }
        parsed.skills.push(format!("Utility: {}", u.name));
    }
    if !parsed
        .skills
        .iter()
        .any(|s| s.get(..6).is_some_and(|h| h.eq_ignore_ascii_case("Elite:")))
    {
        if let Some(e) = &current.skills.elite {
            parsed.skills.push(format!("Elite: {}", e.name));
        }
    }
    if parsed.stat_prefix.is_empty() {
        if let Some(prefix) = current
            .armor
            .iter()
            .chain(current.trinkets.iter())
            .map(|p| p.stat_prefix.as_str())
            .find(|p| !p.is_empty())
        {
            parsed.stat_prefix = prefix.to_string();
        }
    }

    // Weapons, sigils and relic. Choya often writes Set 1 only when the
    // other kit did not change; that left Set 2 empty and the chat-code
    // encoder with one land kit. Copy any missing set from the equipped
    // loadout. Named sets stay. The plate speaks weapon *types*.
    let (has1, has2) = plate_weapon_slots(&parsed.weapons);
    for (i, set) in current.weapons.iter().enumerate() {
        let Some(line) = loadout_weapon_line(set) else {
            continue;
        };
        let is_set2 = set.label.contains('2') || (!set.label.contains('1') && i >= 1);
        if (is_set2 && has2) || (!is_set2 && has1) {
            continue;
        }
        parsed.weapons.push(line);
    }
    if parsed.sigils.is_empty() {
        parsed.sigils = current
            .weapons
            .iter()
            .flat_map(|set| set.sigils.iter())
            .map(|s| s.name.clone())
            .collect();
    }
    if parsed.relic.is_empty() {
        if let Some(relic) = &current.relic {
            parsed.relic = relic.name.clone();
        }
    }
}

/// Keep the ranger's equipped pets on a plated suggestion. Search never
/// picks pets; dropping the row made Optimized look pet-less.
pub(super) fn keep_loadout_pets(
    suggestion: &mut crate::ui::comparison::BuildSuggestion,
    pets: &[String],
) {
    if pets.is_empty() {
        return;
    }
    if suggestion
        .skills
        .iter()
        .any(|s| s.get(..6).is_some_and(|h| h.eq_ignore_ascii_case("Pets: ")))
    {
        return;
    }
    suggestion
        .skills
        .insert(0, format!("Pets: {}", pets.join(" / ")));
}

pub(super) fn chat_display_text(
    explanation: &str,
    spec_count: usize,
    error_details: &[String],
) -> String {
    let mut display = if explanation.is_empty() {
        "I couldn't make a legal build from that.".to_string()
    } else {
        explanation.to_string()
    };
    // Talk replies send empty specs on purpose. Don't paste validation into the bubble.
    if spec_count > 0 && !error_details.is_empty() {
        display.push_str("\n\n(");
        display.push_str(&error_details.join("; "));
        display.push(')');
    }
    display
}

pub(super) fn result_alert_tab(has_current: bool) -> crate::state::MainTab {
    if has_current {
        crate::state::MainTab::Improve
    } else {
        crate::state::MainTab::NewBuild
    }
}

pub(super) fn format_provider_issue(err: &str, provider: &str, model: &str) -> String {
    let lower = err.to_lowercase();
    let detail = if lower.contains("data policy") || lower.contains("guardrail") {
        // OpenRouter, 404: "0 endpoints out of N requested are available
        // matching your guardrail restrictions and data policy ... Free
        // model training". The account's privacy settings exclude the
        // provider; nothing in the request can change that.
        t("err.data_policy")
    } else if lower.contains("rate limit") || lower.contains("429") {
        // The provider's own words say whose limit it is. OpenRouter:
        // "temporarily rate-limited upstream" is their pool; Google names
        // the quota ("per minute", "per day") in the body it sends.
        if lower.contains("upstream") || lower.contains("shared pool") {
            t("err.upstream_busy")
        } else if lower.contains("per day") || lower.contains("perday") || lower.contains("daily") {
            t("err.daily_quota")
        } else if lower.contains("per minute") || lower.contains("perminute") {
            t("err.minute_quota")
        } else {
            t("err.rate_limited")
        }
    } else if lower.contains("invalid api key")
        || lower.contains("401")
        || lower.contains("unauthorized")
    {
        t("err.bad_key")
    } else if lower.contains("billing")
        || lower.contains("quota")
        || lower.contains("credit")
        || lower.contains("insufficient")
    {
        t("err.billing")
    } else if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline")
    {
        t("err.timeout")
    } else if lower.contains("529") || lower.contains("overloaded") || lower.contains("unavailable")
    {
        t("err.overloaded")
    } else {
        err.chars().take(240).collect()
    };
    format!("{provider} \u{00b7} {model}: {detail}")
}

pub(super) fn summarize_suggestion(s: &crate::ui::comparison::BuildSuggestion) -> String {
    let specs: String = s
        .specializations
        .iter()
        .map(|(n, t)| format!("{} [{}]", n, t.join(", ")))
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "{} · {} · {} · {} · rune {} · relic {}",
        s.label,
        s.stat_prefix,
        specs,
        s.weapons.join(" / "),
        s.rune,
        s.relic
    )
}

/// Convert Gemini tool function names to human-readable descriptions.
pub(super) fn humanize_tool_names(tool_names: &[String]) -> String {
    let labels: Vec<&str> = tool_names
        .iter()
        .map(|n| match n.as_str() {
            "get_profession_info" => "reading profession",
            "get_spec_traits" => "checking traits",
            "get_trait_details" => "analyzing trait",
            "get_skill_info" => "checking skill",
            "list_runes" => "browsing runes",
            "list_sigils" => "browsing sigils",
            "list_relics" => "browsing relics",
            "search_upgrades" => "searching upgrades",
            "upgrade_synergies" => "upgrade synergies",
            "calculate_stats" => "calculating stats",
            "simulate_combat" => "simulating combat",
            "score_build" => "scoring build",
            "get_current_build" => "reading current build",
            "get_optimizer_results" => "reviewing candidates",
            "search_traits_by_effect" => "searching trait synergies",
            "find_condition_sources" => "finding condition sources",
            "search_skills_by_effect" => "searching skill synergies",
            "find_synergies" => "analyzing synergies",
            "get_build_synergy_report" => "building synergy report",
            "simulate_rotation" => "simulating rotation",
            _ => "working",
        })
        .collect();
    labels.join(", ")
}

/// Call the active LLM provider to enrich the top optimizer suggestion with AI reasoning.
/// Uses function calling (tool use) so the LLM can query game data and simulate builds.
// LLM enrichment call; config, profession, weights, mode, and candidates are
// independent inputs — grouping them adds indirection without clarity.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests {
    use super::super::chat_flow::plate_is_servable;
    use super::{
        apply_radar_prefix, attach_chat_stats, chat_display_text, fill_holes_from_loadout,
        format_provider_issue, gemini_from_validated, keep_equipped_weapons, keep_loadout_pets,
        kitchen_brief, leftover_plate_quality, snapshot_ranger_pets, suggestion_to_chat_code,
    };
    use crate::ui::comparison::BuildSuggestion;
    use base64::Engine as _;
    use gw2_api::models::{Profession, Specialization, Trait};
    use std::collections::HashMap;

    fn skill(id: u32, name: &str) -> gw2_api::models::Skill {
        serde_json::from_value(serde_json::json!({ "id": id, "name": name })).expect("skill")
    }

    fn chat_code_db() -> gw2_optimizer::gamedb::GameDb {
        let mut db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        db.professions.insert(
            "Thief".into(),
            Profession {
                id: "Thief".into(),
                name: "Thief".into(),
                code: Some(5),
                specializations: vec![7],
                weapons: HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        db.specializations.insert(
            7,
            Specialization {
                id: 7,
                name: "Daredevil".into(),
                profession: "Thief".into(),
                elite: true,
                minor_traits: vec![],
                major_traits: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
                weapon_trait: None,
                icon: None,
                background: None,
                profession_icon: None,
                profession_icon_big: None,
            },
        );
        for (id, name) in [
            (1u32, "Marauder's Resilience"),
            (4, "Havoc Specialist"),
            (7, "Unhindered Combatant"),
        ] {
            db.traits.insert(
                id,
                Trait {
                    id,
                    name: name.into(),
                    icon: None,
                    description: None,
                    specialization: 7,
                    tier: 1,
                    order: 0,
                    slot: "Major".into(),
                    facts: vec![],
                    traited_facts: vec![],
                    skills: vec![],
                },
            );
        }
        db.traits_by_spec.insert(7, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
        db.skills.insert(10, skill(10, "Hide in Shadows"));
        db.skills.insert(11, skill(11, "Haste"));
        db.skills.insert(12, skill(12, "Impairing Daggers"));
        db.skills.insert(13, skill(13, "Skale Venom"));
        db.skills.insert(14, skill(14, "Dagger Storm"));
        db.skills_by_profession
            .insert("Thief".into(), vec![10, 11, 12, 13, 14]);
        db.skill_to_palette.insert(10, 268);
        db.skill_to_palette.insert(11, 347);
        db.skill_to_palette.insert(12, 4905);
        db.skill_to_palette.insert(13, 318);
        db.skill_to_palette.insert(14, 415);
        db
    }

    #[test]
    fn load_style_suggestion_encodes_chat_code_from_names() {
        let db = chat_code_db();
        let suggestion = BuildSuggestion {
            specializations: vec![(
                "Daredevil [E]".into(),
                vec![
                    "Marauder's Resilience".into(),
                    "Havoc Specialist".into(),
                    "Unhindered Combatant".into(),
                ],
            )],
            weapons: vec!["Set 1: Axe / Dagger".into()],
            skills: vec![
                "Heal: Hide in Shadows".into(),
                "Utility: Haste".into(),
                "Utility: Impairing Daggers".into(),
                "Utility: Skale Venom".into(),
                "Elite: Dagger Storm".into(),
            ],
            ..Default::default()
        };
        let code = suggestion_to_chat_code(&suggestion, &db).expect("encode on load");
        assert!(code.starts_with("[&"), "{code}");
        assert!(code.ends_with(']'), "{code}");

        let inner = code
            .strip_prefix("[&")
            .and_then(|s| s.strip_suffix(']'))
            .expect("[&...] wrapper");
        let buf = base64::engine::general_purpose::STANDARD
            .decode(inner)
            .expect("base64");
        // 10×u16 palettes at byte 8: land, aqua, land, aqua, ...
        for i in 0..5 {
            let land = u16::from_le_bytes([buf[8 + i * 4], buf[9 + i * 4]]);
            let aqua = u16::from_le_bytes([buf[10 + i * 4], buf[11 + i * 4]]);
            assert_ne!(land, 0, "land palette {i} should resolve");
            assert_eq!(aqua, 0, "aquatic palette {i} must stay empty");
        }
        let rest = &buf[44..];
        if !rest.is_empty() {
            let count = rest[0] as usize;
            assert_eq!(
                rest.len(),
                1 + count * 2 + 1,
                "SotO trailer must be count+ids+override"
            );
            assert_eq!(*rest.last().unwrap(), 0);
            for i in 0..count {
                let id = u16::from_le_bytes([rest[1 + i * 2], rest[2 + i * 2]]);
                assert_ne!(id, 265, "aquatic weapon type");
            }
        }
    }

    #[test]
    fn keep_equipped_weapons_matches_keep_same() {
        assert!(keep_equipped_weapons(
            "I already have a condi build. I want to keep same weapons."
        ));
        assert!(!keep_equipped_weapons("make me a power build"));
    }

    #[test]
    fn kitchen_brief_lists_mode_scale_role() {
        let brief = kitchen_brief(
            "WvW",
            "Roam",
            "Support",
            "Small-group Support: self-reliant.",
            "Profession: Elementalist",
            "(empty)",
            false,
        );
        assert!(brief.contains("Mode: WvW"), "{brief}");
        assert!(brief.contains("Scale: Roam"), "{brief}");
        assert!(brief.contains("Role: Support"), "{brief}");
        assert!(brief.contains("self-reliant"), "{brief}");
        assert!(brief.contains("Profession: Elementalist"), "{brief}");
        assert!(!brief.contains("Radar:"), "{brief}");
        assert!(!brief.contains("Locks:"), "{brief}");
        assert!(brief.contains("On the pass:"), "{brief}");
        assert!(brief.contains("get_optimizer_results is empty"), "{brief}");
        assert!(brief.contains("weapons.set1"), "{brief}");
        assert!(brief.contains("copy both from Character"), "{brief}");
    }

    #[test]
    fn gemini_from_validated_prefers_resolved_rune() {
        let raw = gw2_optimizer::prompts::GeminiBuildResponse {
            rune: "Hallucinated Rune".into(),
            explanation: "A sharp plate.".into(),
            ..Default::default()
        };
        let v = gw2_optimizer::validation::ValidatedBuild {
            rune: Some(gw2_optimizer::validation::ValidatedItem {
                id: 1,
                name: "Scholar".into(),
            }),
            ..Default::default()
        };
        let plated = gemini_from_validated(raw, &v);
        assert_eq!(plated.rune, "Scholar");
    }

    #[test]
    fn gemini_from_validated_keeps_pets() {
        // A18-4: servable rebuild (heal+utils+elite) used to drop the Pets:
        // row fill_holes_from_loadout just inserted. Chat then plated pet-less.
        let raw = gw2_optimizer::prompts::GeminiBuildResponse {
            skills: vec![
                "Pets: Juvenile Smokescale / Juvenile Rock Gazelle".into(),
                "Heal: Troll Unguent".into(),
                "Utility: Lightning Reflexes".into(),
                "Utility: Sharpening Stone".into(),
                "Utility: Signet of Stone".into(),
                "Elite: Entangle".into(),
            ],
            ..Default::default()
        };
        let mut v = gw2_optimizer::validation::ValidatedBuild::default();
        v.skills.heal = Some((1, "Troll Unguent".into()));
        v.skills.elite = Some((9, "Entangle".into()));
        v.skills.utilities = vec![
            Some((2, "Lightning Reflexes".into())),
            Some((3, "Sharpening Stone".into())),
            Some((4, "Signet of Stone".into())),
        ];
        let plated = gemini_from_validated(raw, &v);
        assert!(
            plated
                .skills
                .iter()
                .any(|s| s == "Pets: Juvenile Smokescale / Juvenile Rock Gazelle"),
            "gemini_from_validated must keep the Pets row: {:?}",
            plated.skills
        );
        assert!(
            plated.skills.iter().any(|s| s.contains("Troll Unguent")),
            "{:?}",
            plated.skills
        );

        // Parsed plate pets (A15-6 field) also land on the rebuilt list.
        let raw_field = gw2_optimizer::prompts::GeminiBuildResponse {
            skills: vec!["Heal: Troll Unguent".into()],
            pets: Some([
                Some("Juvenile Smokescale".into()),
                Some("Juvenile Rock Gazelle".into()),
                None,
                None,
            ]),
            ..Default::default()
        };
        let mut v_field = gw2_optimizer::validation::ValidatedBuild::default();
        v_field.skills.heal = Some((1, "Troll Unguent".into()));
        let plated_field = gemini_from_validated(raw_field, &v_field);
        assert!(
            plated_field
                .skills
                .iter()
                .any(|s| s == "Pets: Juvenile Smokescale / Juvenile Rock Gazelle"),
            "{:?}",
            plated_field.skills
        );

        // Validated pet IDs also emit a Pets row when raw had none.
        let raw_no_pets = gw2_optimizer::prompts::GeminiBuildResponse {
            skills: vec!["Heal: Troll Unguent".into()],
            ..Default::default()
        };
        let mut v2 = gw2_optimizer::validation::ValidatedBuild::default();
        v2.skills.heal = Some((1, "Troll Unguent".into()));
        v2.pets = Some((Some(1), Some(2), None, None));
        let plated2 = gemini_from_validated(raw_no_pets, &v2);
        assert!(
            plated2.skills.iter().any(|s| s.starts_with("Pets:")),
            "{:?}",
            plated2.skills
        );

        let src = include_str!("optimization.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");
        let start = production
            .find("fn gemini_from_validated(")
            .expect("gemini_from_validated gone");
        let after = &production[start..];
        let end = after[1..]
            .find("\nfn ")
            .map(|i| i + 1)
            .expect("gemini_from_validated has no following fn");
        let body = &after[..end];
        let skills_at = body.find("let mut skills").expect("skills rebuild gone");
        let assign_at = body
            .find("raw.skills = skills")
            .expect("skills assign gone");
        let chunk = &body[skills_at..assign_at];
        assert!(
            chunk.contains("Pets:") || chunk.contains("v.pets"),
            "skills rebuild must keep pets from v.pets / raw Pets: row"
        );
    }

    #[test]
    fn plate_is_servable_needs_full_bar() {
        let mut v = gw2_optimizer::validation::ValidatedBuild::default();
        assert!(!plate_is_servable(&v));
        let spec = |id, name: &str| gw2_optimizer::validation::ValidatedSpec {
            spec_id: id,
            name: name.into(),
            elite: id == 3,
            trait_ids: vec![id, id + 1, id + 2],
            trait_names: vec!["a".into(), "b".into(), "c".into()],
            all_trait_ids: vec![id, id + 1, id + 2],
        };
        v.specializations = vec![spec(1, "Water"), spec(2, "Arcane"), spec(3, "Tempest")];
        v.skills.heal = Some((1, "H".into()));
        v.skills.elite = Some((9, "E".into()));
        v.skills.utilities = vec![
            Some((2, "U1".into())),
            Some((3, "U2".into())),
            Some((4, "U3".into())),
        ];
        assert!(plate_is_servable(&v));
        v.skills.utilities.pop();
        assert!(!plate_is_servable(&v));
        v.skills.utilities.push(Some((4, "U3".into())));
        assert!(plate_is_servable(&v));
        v.errors.push(gw2_optimizer::validation::ValidationReject {
            code: gw2_optimizer::validation::RejectCode::WeaponNotAvailable {
                slot: "Set 2".into(),
                weapon: "Short Bow".into(),
                profession: "Thief".into(),
            },
            detail: "Set 2: weapon 'Short Bow' not available for Thief".into(),
        });
        assert!(
            plate_is_servable(&v),
            "leftover weapon typos must not hide a complete kit"
        );
        v.specializations[1].trait_ids.pop();
        assert!(!plate_is_servable(&v));
    }

    #[test]
    fn mix_request_does_not_repaint_every_slot() {
        // The live G8 report: "Add some plaguedoctor stats in there" came back
        // as sixteen Plaguedoctor slots. `some` is a REQUEST_FILLER, so the
        // mention read as a bare affirmative, `stat_prefix` was force-set, and
        // `fill_worn_gear_slots` painted the whole kit.
        let weights = gw2_optimizer::scoring::OptimizationWeights::preset_power_dps();
        let base = "Viper's";

        for order in [
            "Add some plaguedoctor stats in there",
            "give me a few plaguedoctor pieces",
            "mix in plaguedoctor",
            "a splash of plaguedoctor please",
        ] {
            let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
                stat_prefix: base.into(),
                ..Default::default()
            };
            apply_radar_prefix(&mut parsed, &weights, order);
            assert_eq!(
                parsed.stat_prefix, base,
                "{order:?} is a partial request; forcing stat_prefix repaints \
                 every worn slot with it"
            );
        }

        // A whole-kit request must still win, or the fix has simply broken the
        // affirmative path instead of narrowing it.
        for order in [
            "make it all plaguedoctor",
            "I want plaguedoctor gear",
            "plaguedoctor please",
        ] {
            let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
                stat_prefix: base.into(),
                ..Default::default()
            };
            apply_radar_prefix(&mut parsed, &weights, order);
            assert_eq!(
                parsed.stat_prefix, "Plaguedoctor's",
                "{order:?} asks for the whole kit and must still override"
            );
        }

        // A rejection stays a rejection, partial cue or not.
        let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
            stat_prefix: base.into(),
            ..Default::default()
        };
        apply_radar_prefix(&mut parsed, &weights, "don't give me some plaguedoctor");
        assert_eq!(parsed.stat_prefix, base);
    }

    #[test]
    fn apply_radar_prefix_honors_celestial_in_order() {
        let weights = gw2_optimizer::scoring::OptimizationWeights::preset_power_dps();
        let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
            stat_prefix: "Harrier's".into(),
            ..Default::default()
        };
        apply_radar_prefix(
            &mut parsed,
            &weights,
            "I want celestial gear tempest support",
        );
        assert_eq!(parsed.stat_prefix, "Celestial");
    }

    #[test]
    fn apply_radar_prefix_keeps_llm_when_order_silent() {
        let weights = gw2_optimizer::scoring::OptimizationWeights::preset_power_dps();
        let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
            stat_prefix: "Celestial".into(),
            ..Default::default()
        };
        apply_radar_prefix(&mut parsed, &weights, "make me a power build");
        assert_eq!(parsed.stat_prefix, "Celestial");
    }

    #[test]
    fn apply_radar_prefix_skips_negated_minstrel() {
        let weights = gw2_optimizer::scoring::OptimizationWeights::preset_power_dps();
        let apply = |order: &str| {
            let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
                stat_prefix: "Harrier's".into(),
                ..Default::default()
            };
            apply_radar_prefix(&mut parsed, &weights, order);
            parsed.stat_prefix
        };

        // An affirmative prefix elsewhere in the order still wins.
        assert_eq!(apply("I said CELESTIAL support, not minstrel"), "Celestial");

        // Rejections the caller's own negation check misses: it only looks at
        // the single word before the stem, and only knows four cues. Every one
        // of these must leave the model's pick ("Harrier's") alone.
        for order in [
            "don't use minstrel",
            "stop suggesting minstrel",
            "please avoid minstrel gear",
            "anything other than minstrel",
            "give me something instead of minstrel",
            "I do not want minstrel stats",
        ] {
            assert_eq!(
                apply(order),
                "Harrier's",
                "a rejected prefix was forced onto the build: {order:?}"
            );
        }

        // …and a genuine request still lands, including right after a
        // rejection of a DIFFERENT prefix.
        for order in [
            "give me minstrel",
            "not celestial, minstrel please",
            "use minstrel stats",
        ] {
            assert_eq!(
                apply(order),
                "Minstrel's",
                "an affirmative request was dropped: {order:?}"
            );
        }
    }

    #[test]
    fn snapshot_ranger_pets_takes_mut_state() {
        use gw2_api::models::{Build, BuildTab, PetSelection};

        let build_with_pets = |first: u32, second: u32| Build {
            name: None,
            profession: Some("Ranger".into()),
            specializations: vec![],
            skills: None,
            aquatic_skills: None,
            legends: vec![],
            aquatic_legends: vec![],
            pets: Some(PetSelection {
                terrestrial: vec![Some(first), Some(second)],
                aquatic: vec![],
            }),
        };

        let _serial = crate::state::state_test_guard();
        let dir =
            std::env::temp_dir().join(format!("gw2_snapshot_ranger_pets_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::state::clear();
        crate::state::init(dir.clone());

        // Called from INSIDE a `with_state` closure — the exact shape that
        // deadlocked when this function reached for the global `STATE` itself.
        let observed = crate::state::with_state(|s| {
            s.main.build_tabs = vec![
                BuildTab {
                    tab: 1,
                    is_active: false,
                    build: build_with_pets(10, 11),
                },
                BuildTab {
                    tab: 2,
                    is_active: true,
                    build: build_with_pets(20, 21),
                },
            ];

            s.main.selected_build_tab = Some(1);
            let selected = snapshot_ranger_pets(s);
            s.main.selected_build_tab = Some(9);
            let out_of_range = snapshot_ranger_pets(s);
            s.main.selected_build_tab = None;
            let unselected = snapshot_ranger_pets(s);
            (selected, out_of_range, unselected)
        })
        .expect("state initialised");

        crate::state::clear();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            observed.0.map(|p| p.terrestrial),
            Some(vec![Some(20), Some(21)]),
            "pets must come from the SELECTED build tab"
        );
        assert!(
            observed.1.is_none(),
            "a build-tab index past the end has no pets"
        );
        assert!(observed.2.is_none(), "no selected build tab means no pets");
    }

    #[test]
    fn fill_holes_from_loadout_adds_missing_trait_and_util() {
        let current = gw2_core::types::ResolvedBuild {
            specializations: vec![gw2_core::types::ResolvedSpec {
                id: 41,
                name: "Arcane".into(),
                elite: false,
                traits_selected: vec![
                    gw2_core::types::ResolvedTrait {
                        id: 1,
                        name: "Arcane Precision".into(),
                        description: String::new(),
                        column: 0,
                        selected: true,
                    },
                    gw2_core::types::ResolvedTrait {
                        id: 2,
                        name: "Arcane Resurrection".into(),
                        description: String::new(),
                        column: 1,
                        selected: true,
                    },
                    gw2_core::types::ResolvedTrait {
                        id: 3,
                        name: "Evasive Arcana".into(),
                        description: String::new(),
                        column: 2,
                        selected: true,
                    },
                ],
                traits_available: vec![],
            }],
            skills: gw2_core::types::ResolvedSkills {
                heal: Some(gw2_core::types::SkillInfo {
                    id: 10,
                    name: "Wash the Pain Away!".into(),
                }),
                utilities: vec![
                    Some(gw2_core::types::SkillInfo {
                        id: 11,
                        name: "Aftershock!".into(),
                    }),
                    Some(gw2_core::types::SkillInfo {
                        id: 12,
                        name: "Eye of the Storm!".into(),
                    }),
                    Some(gw2_core::types::SkillInfo {
                        id: 13,
                        name: "Arcane Blast".into(),
                    }),
                ],
                elite: Some(gw2_core::types::SkillInfo {
                    id: 14,
                    name: "Rebound!".into(),
                }),
            },
            ..Default::default()
        };
        let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
            specializations: vec![(
                "Arcane".into(),
                vec!["Arcane Resurrection".into(), "Evasive Arcana".into()],
            )],
            skills: vec![
                "Heal: Wash the Pain Away!".into(),
                "Utils: Aftershock!, Eye of the Storm!".into(),
                "Elite: Rebound!".into(),
            ],
            ..Default::default()
        };
        fill_holes_from_loadout(&mut parsed, &current);
        assert_eq!(parsed.specializations[0].1.len(), 3);
        assert!(parsed.specializations[0]
            .1
            .iter()
            .any(|t| t == "Arcane Precision"));
        assert!(parsed.skills.iter().any(|s| s.contains("Arcane Blast")));
    }

    /// A plate that changes nothing about the weapons omits them, and the
    /// player then sees an empty WEAPONS column with no sigils (measured
    /// in-game 2026-09-05 on 1.11.29, heal Scourge). Weapon *types*, not
    /// item names: that is what the plate speaks.
    #[test]
    fn fill_holes_from_loadout_keeps_equipped_weapons_sigils_and_relic() {
        let weapon = |ty: &str| gw2_core::types::WeaponInfo {
            name: format!("Minstrel's {ty} of Water"),
            weapon_type: ty.into(),
            id: 0,
        };
        let current = gw2_core::types::ResolvedBuild {
            specializations: vec![gw2_core::types::ResolvedSpec {
                id: 5,
                name: "Blood Magic".into(),
                elite: false,
                traits_selected: vec![],
                traits_available: vec![],
            }],
            weapons: vec![
                gw2_core::types::ResolvedWeaponSet {
                    label: "Set 1".into(),
                    stat_prefix: "Minstrel's".into(),
                    main_hand: Some(weapon("Scepter")),
                    off_hand: Some(weapon("Focus")),
                    sigils: vec![gw2_core::types::UpgradeInfo {
                        id: 1,
                        name: "Superior Sigil of Concentration".into(),
                    }],
                },
                gw2_core::types::ResolvedWeaponSet {
                    label: "Set 2".into(),
                    stat_prefix: "Minstrel's".into(),
                    main_hand: Some(weapon("Staff")),
                    off_hand: None,
                    sigils: vec![gw2_core::types::UpgradeInfo {
                        id: 2,
                        name: "Superior Sigil of Transference".into(),
                    }],
                },
            ],
            relic: Some(gw2_core::types::ResolvedRelic {
                id: 3,
                name: "Relic of the Water".into(),
                description: String::new(),
            }),
            ..Default::default()
        };
        let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
            specializations: vec![("Blood Magic".into(), vec!["Blood Renewal".into()])],
            ..Default::default()
        };
        fill_holes_from_loadout(&mut parsed, &current);
        assert_eq!(
            parsed.weapons,
            vec![
                "Set 1: Scepter / Focus".to_string(),
                "Set 2: Staff".to_string()
            ],
            "a two-hander must not grow an off-hand"
        );
        assert_eq!(parsed.sigils.len(), 2, "sigils ride the equipped weapons");
        assert_eq!(parsed.relic, "Relic of the Water");

        // A plate that named Set 1 keeps that set and still gets Set 2.
        let mut chosen = gw2_optimizer::prompts::GeminiBuildResponse {
            specializations: vec![("Blood Magic".into(), vec!["Blood Renewal".into()])],
            weapons: vec!["Set 1: Greatsword".into()],
            relic: "Relic of Durability".into(),
            ..Default::default()
        };
        fill_holes_from_loadout(&mut chosen, &current);
        assert_eq!(
            chosen.weapons,
            vec!["Set 1: Greatsword".to_string(), "Set 2: Staff".to_string()]
        );
        assert_eq!(chosen.relic, "Relic of Durability");
    }

    #[test]
    fn fill_holes_from_loadout_copies_pets() {
        let current = gw2_core::types::ResolvedBuild {
            specializations: vec![gw2_core::types::ResolvedSpec {
                id: 5,
                name: "Skirmishing".into(),
                elite: false,
                traits_selected: vec![],
                traits_available: vec![],
            }],
            pets: vec!["Juvenile Smokescale".into(), "Juvenile Rock Gazelle".into()],
            ..Default::default()
        };
        let mut parsed = gw2_optimizer::prompts::GeminiBuildResponse {
            specializations: vec![(
                "Skirmishing".into(),
                vec!["a".into(), "b".into(), "c".into()],
            )],
            skills: vec!["Heal: Troll Unguent".into()],
            ..Default::default()
        };
        fill_holes_from_loadout(&mut parsed, &current);
        assert!(
            parsed
                .skills
                .iter()
                .any(|s| s == "Pets: Juvenile Smokescale / Juvenile Rock Gazelle"),
            "{:?}",
            parsed.skills
        );
    }

    #[test]
    fn keep_loadout_pets_inserts_once() {
        let mut suggestion = BuildSuggestion::default();
        keep_loadout_pets(
            &mut suggestion,
            &["Juvenile Smokescale".into(), "Juvenile Rock Gazelle".into()],
        );
        assert_eq!(
            suggestion.skills,
            vec!["Pets: Juvenile Smokescale / Juvenile Rock Gazelle".to_string()]
        );
        keep_loadout_pets(&mut suggestion, &["Juvenile Brown Bear".into()]);
        assert_eq!(suggestion.skills.len(), 1);
    }

    #[test]
    fn talk_reply_hides_empty_spec_validation() {
        let t = chat_display_text(
            "Hey there!",
            0,
            &["Expected 3 specializations, got 0".into()],
        );
        assert_eq!(t, "Hey there!");
    }

    #[test]
    fn illegal_plate_keeps_validation_in_chat() {
        let t = chat_display_text("Nope", 2, &["Expected 3 specializations, got 2".into()]);
        assert!(t.contains("Expected 3 specializations, got 2"));
    }

    #[test]
    fn format_provider_issue_names_model_and_classifies() {
        gw2_core::i18n::set_language("en");
        let t = format_provider_issue("HTTP 429 rate limit", "OpenRouter", "llama-tiny");
        assert!(t.contains("OpenRouter"));
        assert!(t.contains("llama-tiny"));
        assert!(t.contains("Rate limited"));
        let t = format_provider_issue("Invalid API key", "Gemini", "gemini-2.5-flash");
        assert!(t.contains("API key rejected"));
        let t = format_provider_issue("credit balance too low", "OpenAI", "gpt-4o");
        assert!(t.contains("Billing"));
    }

    #[test]
    fn leftover_empty_kit_is_blocked_not_verified() {
        assert_eq!(
            leftover_plate_quality(true),
            gw2_optimizer::data::DataQuality::Blocked
        );
        assert_eq!(
            leftover_plate_quality(false),
            gw2_optimizer::data::DataQuality::Verified
        );
    }

    fn prefix_stat(
        id: u32,
        name: &str,
        major: &str,
        minor_a: &str,
        minor_b: &str,
    ) -> gw2_api::models::ItemStat {
        gw2_api::models::ItemStat {
            id,
            name: name.into(),
            attributes: vec![
                gw2_api::models::StatAttribute {
                    attribute: major.into(),
                    multiplier: 0.35,
                    value: 0,
                },
                gw2_api::models::StatAttribute {
                    attribute: minor_a.into(),
                    multiplier: 0.25,
                    value: 0,
                },
                gw2_api::models::StatAttribute {
                    attribute: minor_b.into(),
                    multiplier: 0.25,
                    value: 0,
                },
            ],
        }
    }

    fn mixed_chat_db() -> gw2_optimizer::gamedb::GameDb {
        let mut db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        db.itemstats.insert(
            1,
            prefix_stat(1, "Berserker's", "Power", "Precision", "Ferocity"),
        );
        db.itemstats.insert(
            2,
            prefix_stat(2, "Sentinel's", "Vitality", "Toughness", "Power"),
        );
        db.pvp_amulets.insert(
            10,
            gw2_api::models::PvpAmulet {
                id: 10,
                name: "Berserker Amulet".into(),
                icon: None,
                attributes: [
                    ("Power".into(), 900),
                    ("Precision".into(), 900),
                    ("CritDamage".into(), 560),
                ]
                .into_iter()
                .collect(),
            },
        );
        db
    }

    fn mixed_validated() -> gw2_optimizer::validation::ValidatedBuild {
        let mut v = gw2_optimizer::validation::ValidatedBuild::default();
        v.fill_gear_slots(gw2_core::types::PrefixRef {
            itemstat_id: 1,
            name: "Berserker's".into(),
        });
        v.gear_slots.set(
            gw2_core::types::GearSlot::Helm,
            gw2_core::types::PrefixRef {
                itemstat_id: 2,
                name: "Sentinel's".into(),
            },
        );
        v.warnings
            .push("Helm prefix remapped from Berserker's to Sentinel's".into());
        v
    }

    #[test]
    fn attach_chat_stats_uses_validated_mixed_slots() {
        let db = mixed_chat_db();
        let validated = mixed_validated();
        let ctx = gw2_optimizer::balance::BalanceContext::pve();
        let (expected, _) =
            gw2_optimizer::engine::calculate_validated_stats(&validated, &db, "Warrior", &ctx);
        let uniform =
            gw2_optimizer::gemini_tools::estimate_prefix_stats(&db, "Berserker's", "Warrior")
                .expect("berserker prefix");
        assert!(
            expected.toughness > uniform.1.toughness,
            "Sentinel helm must add toughness the uniform Berserker sheet lacks"
        );

        let mut suggestion = BuildSuggestion {
            // Ours, not published anywhere.
            source_url: String::new(),
            stat_prefix: "Berserker's".into(),
            ..Default::default()
        };
        attach_chat_stats(
            &mut suggestion,
            &db,
            "Warrior",
            &gw2_core::types::GameMode::PvE,
            Some(&validated),
        );
        let plated = suggestion.estimated_stats.expect("stats");
        assert_eq!(plated.toughness, expected.toughness.round() as i32);
        assert_eq!(plated.vitality, expected.vitality.round() as i32);
        assert!(
            suggestion
                .quality_reasons
                .iter()
                .any(|r| r.contains("Helm prefix remapped")),
            "validator corrections must reach the plate: {:?}",
            suggestion.quality_reasons
        );
    }

    #[test]
    fn attach_chat_stats_pvp_uses_amulet_not_land_kit() {
        let db = mixed_chat_db();
        let validated = mixed_validated();
        let mut suggestion = BuildSuggestion {
            // Ours, not published anywhere.
            source_url: String::new(),
            stat_prefix: "Berserker's".into(),
            ..Default::default()
        };
        attach_chat_stats(
            &mut suggestion,
            &db,
            "Warrior",
            &gw2_core::types::GameMode::PvP,
            Some(&validated),
        );
        let plated = suggestion.estimated_stats.expect("pvp stats");
        let land =
            gw2_optimizer::gemini_tools::estimate_prefix_stats(&db, "Berserker's", "Warrior")
                .expect("land");
        let pvp_ctx = gw2_optimizer::balance::BalanceContext::pvp();
        let (expected, _) =
            gw2_optimizer::engine::calculate_validated_stats(&validated, &db, "Warrior", &pvp_ctx);
        assert_eq!(plated.power, expected.power.round() as i32);
        assert!(
            (plated.power as f64) < land.1.power,
            "PvP amulet must not be plated as a land kit: plated={} land={}",
            plated.power,
            land.1.power
        );
    }
    #[test]
    fn coverage_note_is_set_from_wvw_effects_reason() {
        use gw2_optimizer::data::quality::coverage_reason;
        use gw2_optimizer::data::DataQualityReason;
        let reason = coverage_reason(
            "Necromancer",
            &gw2_core::types::GameMode::WvW,
            &["Superior Sigil of Fire (on-crit)".to_string()],
        )
        .expect("one unmodeled name is a reason");
        let other = DataQualityReason {
            field: "validated_build.warning".into(),
            entity: "Necromancer".into(),
            modes: vec!["WvW".into()],
            explanation: "Spite column 2 filled".into(),
        };
        assert_eq!(
            super::coverage_note_from(&[other.clone(), reason]).as_deref(),
            Some("Superior Sigil of Fire (on-crit)")
        );
        assert_eq!(super::coverage_note_from(&[other]), None);
        assert_eq!(super::coverage_note_from(&[]), None);
    }
}
