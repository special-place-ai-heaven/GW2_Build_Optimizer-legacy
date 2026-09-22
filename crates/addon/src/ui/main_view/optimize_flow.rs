use super::optimization::{
    apply_gemini_response, candidate_to_suggestion, humanize_tool_names, keep_loadout_pets,
    simulate_suggestion_rotation, summarize_resolved_build, synergy_result_to_suggestion,
};
use crate::state::AddonState;
use crate::ui::gear_sheet::piece_gear_slot;
use gw2_core::i18n::{t, tf};
use gw2_core::types::GearSlot;
use gw2_optimizer::balance::BalanceContext;
use gw2_optimizer::scoring::OptimizationWeights;
use gw2_optimizer::ScenarioSpec;

/// Which button started this run.
///
/// Carried explicitly from the entry point down to the worker instead of being
/// inferred from `state.main.current_build`. The old inference gated New Build
/// runs against whatever gear the character happened to be wearing, which is
/// exactly what the player did *not* ask for when they pressed "Create build".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OptimizeEntry {
    /// "Create build" — no baseline, nothing to beat.
    NewBuild,
    /// "Improve build" — the player's current gear is the bar to clear.
    Improve,
}

impl OptimizeEntry {
    /// Tab to blink when this run's result lands.
    fn result_tab(self) -> crate::state::MainTab {
        match self {
            OptimizeEntry::NewBuild => crate::state::MainTab::NewBuild,
            OptimizeEntry::Improve => crate::state::MainTab::Improve,
        }
    }

    /// Whether this entry point wants the always-better baseline gate at all.
    fn wants_baseline(self) -> bool {
        matches!(self, OptimizeEntry::Improve)
    }
}

/// New Build entry point (S11-T01, S11-T02, S11-T03).
///
/// The only caller is the left-panel action button in `main_view::mod.rs`, on
/// the branch where the Improve tab is *not* active. The entry point is bound
/// here, not derived from state further down.
pub(super) fn start_optimization(state: &mut AddonState) {
    // Profession still comes from the resolved character — a new build is built
    // for whoever is selected. That is a profession lookup, not a baseline.
    let profession_name = state
        .main
        .current_build
        .as_ref()
        .map(|b| b.profession.clone())
        .unwrap_or_default();

    start_optimization_inner(state, &profession_name, OptimizeEntry::NewBuild);
}

/// Improve entry point: start with an explicit profession name (avoids borrow
/// conflicts). Uses `state.main.build_locks` for spec/trait lock constraints.
///
/// The only caller is the left-panel action button in `main_view::mod.rs`, on
/// the Improve branch. The historical name is kept because `mod.rs` is not in
/// this leaf's write set.
// ponytail: rename to `start_improve_optimization` when `main_view/mod.rs` is
// next open — the name predates the explicit entry flag.
pub(super) fn start_optimization_with_profession(state: &mut AddonState, profession_name: &str) {
    start_optimization_inner(state, profession_name, OptimizeEntry::Improve);
}

fn start_optimization_inner(state: &mut AddonState, profession_name: &str, entry: OptimizeEntry) {
    // Guard against concurrent optimization.
    if state.main.optimizing {
        return;
    }

    if let Some(reason) = state
        .main
        .data_state
        .as_ref()
        .and_then(|s| s.optimize_block_reason())
    {
        state.main.error = Some(reason);
        return;
    }

    if state.main.game_db.is_none() {
        state.main.error = Some(t("err.no_gamedb"));
        return;
    }

    if state.main.chat.waiting {
        state.main.error = Some(t("err.chat_busy"));
        return;
    }

    if profession_name.is_empty() {
        state.main.error = Some(t("err.no_character"));
        return;
    }

    let db = state.main.game_db.clone();
    let profession_name = profession_name.to_string();
    let config = state.config.clone();
    let game_mode = state.main.game_mode.clone();
    let game_mode_label = game_mode.label().to_string();
    let balance_ctx = BalanceContext::new(game_mode.clone());
    let current_build_summary = state
        .main
        .current_build
        .as_ref()
        .map(summarize_resolved_build);
    // Current character build for the Improve always-better baseline gate.
    // Cloned only when this run is actually gated: a New Build run has no
    // baseline, so carrying the loadout into the worker would just tempt the
    // next reader into inferring the entry point from it again.
    let loadout = if entry.wants_baseline() {
        state.main.current_build.clone()
    } else {
        None
    };
    let current_pets = state
        .main
        .current_build
        .as_ref()
        .map(|b| b.pets.clone())
        .unwrap_or_default();
    let addon_dir = state.addon_dir.clone();
    let weights = state.main.weights.clone();
    let selected_role = state.main.selected_role;
    let build_locks = state.main.build_locks.clone();
    let combat_tier = combat_tier_for(&game_mode, state.main.combat_tier);
    // Capture locked elite spec name for the Improve Build label.
    let locked_spec_name: Option<String> =
        build_locks.specs.get(2).and_then(|s| *s).and_then(|id| {
            state
                .main
                .game_db
                .as_ref()
                .and_then(|db| db.spec(id))
                .map(|s| s.name.clone())
        });
    // Capture selection snapshot so results can be discarded if the user switches
    // character or build tab while optimization is running (TOCTOU guard).
    let optimizing_for_char = state.main.selected_character;
    let optimizing_for_build_tab = state.main.selected_build_tab;
    let optimizing_for_equip_tab = state.main.selected_equipment_tab;

    state.main.optimizing = true;
    state.main.optimize_stage = t("status.starting");

    // Log the weights and deterministic gear prefix for debugging
    let gear_match = gw2_optimizer::scoring::select_gear_prefix(&weights);
    let tier_label = combat_tier.label().to_string();
    nexus::log::log(
        nexus::log::LogLevel::Info,
        "GW2BuildOpt",
        format!(
            "Optimizing {}/{}{}: weights P={:.2} C={:.2} B={:.2} H={:.2} S={:.2} Ctrl={:.2} ({}) -> gear: {} (sim={:.3})",
            profession_name,
            game_mode_label,
            if tier_label.is_empty() { String::new() } else { format!(" [{}]", tier_label) },
            weights.power, weights.condition, weights.boon_support, weights.healing, weights.sustain, weights.control,
            weights.summary_label(),
            gear_match.primary, gear_match.similarity,
        ),
    );
    state.main.comparison.suggestions.clear();
    state.main.comparison.loading = true;
    state.main.comparison.error = None;

    // `spawn_worker` is the addon's only production thread launch: it names the
    // thread, registers the `JoinHandle` so `on_unload` can wait for it, binds
    // the LLM transports to this run's cancel token, and hands the body its own
    // token clone.
    let started = state.spawn_worker("optimize", move |token| {
        let panic_token = token.clone();
        let thread_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let result = (|| -> Result<Vec<crate::ui::comparison::BuildSuggestion>, String> {
                if token.is_cancelled() {
                    return Err("Cancelled".into());
                }

                let db = db.ok_or("GameDb not loaded")?;

                // Build a mode + tier-aware scenario for the referee and optimize_v2.
                let scenario =
                    scenario_for_run(&balance_ctx, combat_tier, selected_role, &weights);

                // Improve always-better baseline (spec §12.4): rank the
                // user's OWN current gear under this run's weights so a worse
                // optimizer result is refused. New Build has no baseline.
                let improve_baseline = capture_improve_baseline(
                    entry,
                    loadout.as_ref(),
                    &db,
                    &profession_name,
                    &weights,
                    &balance_ctx,
                    &scenario,
                );

                // Primary: optimize_v2 — beam search over complete build states
                {
                    let token_v2 = token.clone();
                    // Create LLM client for the advisor pass (optional — errors silently skip).
                    let llm_for_advisor: Option<Box<dyn gw2_optimizer::llm::LlmClient>> =
                        gw2_optimizer::llm::create_client(&config, &addon_dir).ok();
                    let llm_ref = llm_for_advisor.as_ref().map(|c| c.as_ref());
                    match gw2_optimizer::engine::optimize_v2(
                        &db,
                        &profession_name,
                        &weights,
                        &balance_ctx,
                        &scenario,
                        &build_locks,
                        llm_ref,
                        // Proven combinations seed the beam alongside our own
                        // seed. Never synced means the search is unchanged.
                        Some(addon_dir.as_path()),
                        &mut |progress: gw2_optimizer::engine::OptimizeProgress| {
                            if token_v2.is_cancelled() {
                                return;
                            }
                            // The search's budget receipt is a diagnostic, not a
                            // status: log it and keep it out of the banner. The
                            // default 1500-eval / 10-wide budget stops the beam
                            // after three generations, so this is how we find out
                            // whether that ceiling is real or whether the search
                            // is idling inside its 45s allowance.
                            if progress.stage.starts_with("search_v2") {
                                nexus::log::log(
                                    nexus::log::LogLevel::Info,
                                    "GW2BuildOpt",
                                    progress.stage.clone(),
                                );
                                return;
                            }
                            crate::state::with_state(|s| {
                                s.main.optimize_stage = progress.stage.clone();
                            });
                        },
                        &|| token.is_cancelled(),
                    ) {
                        Ok(synergy_result) => {
                            if token.is_cancelled() {
                                return Err("Cancelled".into());
                            }
                            let (served, outcome) = apply_improve_baseline_gate(
                                synergy_result,
                                &improve_baseline,
                                &db,
                                &profession_name,
                                &weights,
                                &balance_ctx,
                                &scenario,
                            );
                            let mut suggestion = synergy_result_to_suggestion(
                                &served,
                                &db,
                                &profession_name,
                                &scenario,
                                selected_role,
                                improve_label_override(outcome, locked_spec_name.as_deref()),
                                Some(&addon_dir),
                                &weights,
                                &balance_ctx,
                            );
                            keep_loadout_pets(&mut suggestion, &current_pets);
                            return Ok(vec![suggestion]);
                        }
                        Err(e) => {
                            nexus::log::log(
                                nexus::log::LogLevel::Warning,
                                "GW2 Build Optimizer",
                                format!(
                                    "optimize_v2 failed, falling back to synergy engine: {}",
                                    e
                                ),
                            );
                            // Fall through to legacy synergy engine
                        }
                    }
                }

                // Fallback 1: Deterministic synergy engine (no LLM for build selection)
                {
                    let llm_client_opt: Option<Box<dyn gw2_optimizer::llm::LlmClient>> =
                        gw2_optimizer::llm::create_client(&config, &addon_dir).ok();

                    let token_det = token.clone();
                    let llm_ref: Option<&dyn gw2_optimizer::llm::LlmClient> =
                        llm_client_opt.as_ref().map(|c| c.as_ref());
                    match run_deterministic_tier(
                        &db,
                        &profession_name,
                        &weights,
                        &balance_ctx,
                        llm_ref,
                        current_build_summary.as_deref(),
                        &build_locks,
                        &scenario,
                        &mut |progress: gw2_optimizer::engine::OptimizeProgress| {
                            if token_det.is_cancelled() {
                                return;
                            }
                            crate::state::with_state(|s| {
                                s.main.optimize_stage = progress.stage.clone();
                            });
                        },
                        &|| token.is_cancelled(),
                    ) {
                        Ok(synergy_result) => {
                            if token.is_cancelled() {
                                return Err("Cancelled".into());
                            }
                            let (served, outcome) = apply_improve_baseline_gate(
                                synergy_result,
                                &improve_baseline,
                                &db,
                                &profession_name,
                                &weights,
                                &balance_ctx,
                                &scenario,
                            );
                            let mut suggestion = synergy_result_to_suggestion(
                                &served,
                                &db,
                                &profession_name,
                                &scenario,
                                selected_role,
                                improve_label_override(outcome, locked_spec_name.as_deref()),
                                Some(&addon_dir),
                                &weights,
                                &balance_ctx,
                            );
                            keep_loadout_pets(&mut suggestion, &current_pets);
                            return Ok(vec![suggestion]);
                        }
                        Err(e) => {
                            nexus::log::log(
                                nexus::log::LogLevel::Warning,
                                "GW2 Build Optimizer",
                                format!(
                                    "Deterministic engine failed, falling back to legacy: {}",
                                    e
                                ),
                            );
                        }
                    }
                }

                // Fallback 2: Legacy pipeline (no LLM invent-a-build)
                //
                // Tier 3 returns `BuildCandidate`s, which the referee never
                // ranks, so this tier cannot show that it beat the player's own
                // gear. When the Improve gate is armed we therefore do not run
                // it at all: an unranked legacy build served under an "improved"
                // banner is exactly the silent ungate this gate exists to stop.
                // (New Build, and an Improve run whose baseline could not be
                // captured, still fall through — the latter loudly.)
                let legacy = improve_baseline.legacy_tier();
                if let LegacyTier::ServeBaseline(baseline) = legacy {
                    gate_log(
                        nexus::log::LogLevel::Warning,
                        "Improve: optimize_v2 and the deterministic engine both failed; the legacy tier cannot be ranked against your gear, serving your current build".to_string(),
                    );
                    let kept = kept_baseline_result(
                        baseline,
                        &db,
                        &profession_name,
                        &balance_ctx,
                        &scenario,
                    );
                    let mut suggestion = synergy_result_to_suggestion(
                        &kept,
                        &db,
                        &profession_name,
                        &scenario,
                        selected_role,
                        improve_label_override(
                            ImproveOutcome::KeptCurrentGear,
                            locked_spec_name.as_deref(),
                        ),
                        Some(&addon_dir),
                        &weights,
                        &balance_ctx,
                    );
                    keep_loadout_pets(&mut suggestion, &current_pets);
                    return Ok(vec![suggestion]);
                }

                let profession = db.profession(&profession_name).ok_or_else(|| {
                    format!("Profession '{}' not found in GameDb", profession_name)
                })?;

                let token_progress = token.clone();
                let candidates = run_legacy_tier(
                    &db,
                    profession,
                    &weights,
                    &balance_ctx,
                    &build_locks,
                    5,
                    &mut |progress| {
                        if token_progress.is_cancelled() {
                            return;
                        }
                        crate::state::with_state(|s| {
                            s.main.optimize_stage = progress.stage.clone();
                        });
                    },
                    &|| token.is_cancelled(),
                )?;

                if token.is_cancelled() {
                    return Err("Cancelled".into());
                }

                let mut suggestions: Vec<crate::ui::comparison::BuildSuggestion> = candidates
                    .iter()
                    .map(|c| candidate_to_suggestion(c, &db, &balance_ctx))
                    .collect();
                for suggestion in suggestions.iter_mut() {
                    keep_loadout_pets(suggestion, &current_pets);
                }

                // An Improve run only reaches here with no comparable baseline.
                // Say so on the result instead of letting the always-better
                // promise quietly lapse.
                if let LegacyTier::RunUngated(why) = legacy {
                    for suggestion in suggestions.iter_mut() {
                        suggestion.quality_reasons.push(why.to_string());
                    }
                }

                // Enrich top suggestion with LLM reasoning (legacy path)
                if config.has_active_llm_key() {
                    if token.is_cancelled() {
                        return Err("Cancelled".into());
                    }

                    crate::state::with_state(|s| {
                        s.main.optimize_stage = t("status.consulting");
                    });

                    match enrich_with_llm(
                        entry,
                        &config,
                        &profession_name,
                        &weights,
                        &game_mode_label,
                        &candidates,
                        &db,
                        current_build_summary.as_deref(),
                        &mut suggestions,
                        &addon_dir,
                        &balance_ctx,
                        &scenario,
                    ) {
                        Ok(()) => {}
                        Err(e) => {
                            nexus::log::log(
                                nexus::log::LogLevel::Warning,
                                "GW2 Build Optimizer",
                                format!("LLM enrichment skipped: {}", e),
                            );
                        }
                    }
                }

                if suggestions
                    .iter()
                    .all(|s| s.weapons.is_empty() && s.skills.is_empty())
                {
                    return Err("legacy leftover kit is empty".into());
                }

                Ok(suggestions)
            })();

            if !token.is_cancelled() {
                crate::state::with_state(|s| {
                    s.main.optimizing = false;
                    s.main.comparison.loading = false;
                    // Discard results if the user switched character or build tab while
                    // optimization was running — showing results for a different context
                    // would silently corrupt the comparison panel.
                    let context_changed = s.main.selected_character != optimizing_for_char
                        || s.main.selected_build_tab != optimizing_for_build_tab
                        || s.main.selected_equipment_tab != optimizing_for_equip_tab;
                    if context_changed {
                        return;
                    }
                    match result {
                        Ok(suggestions) => {
                            s.main.comparison.suggestions = suggestions;
                            s.main.comparison.selected_suggestion = 0;
                            s.main.comparison.show_optimized = true;
                            s.main.tab_alert = Some(entry.result_tab());
                            s.main.provider_issue = None;
                        }
                        Err(e) => {
                            s.main.comparison.error = Some(e);
                        }
                    }
                });
            } else {
                // Cancelled mid-flight. Without resetting these flags the UI stays stuck
                // on the "Optimizing…" spinner until another optimization completes.
                crate::state::with_state(|s| {
                    s.main.optimizing = false;
                    s.main.comparison.loading = false;
                    // Leaving "Stopping..." behind would outlive the stop.
                    s.main.optimize_stage.clear();
                });
            }
        }));

        // If the thread panicked, recover and show error
        if let Err(panic_info) = thread_result {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                tf("fmt.internal_panic", &[("msg", s)])
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                tf("fmt.internal_panic", &[("msg", s)])
            } else {
                t("err.opt_panic")
            };
            if !panic_token.is_cancelled() {
                crate::state::with_state(|s| {
                    s.main.optimizing = false;
                    s.main.comparison.loading = false;
                    s.main.comparison.error = Some(msg);
                });
            }
        }
    });

    // The OS refused the thread: the work never started, so clear the flags we
    // set above or the overlay spins on "Optimizing…" forever.
    if !started {
        state.main.optimizing = false;
        state.main.comparison.loading = false;
        // Not `err.opt_panic`: nothing panicked, the thread never existed.
        // ponytail: English, like BASELINE_KEPT_REASON below — needs a
        // catalogue key in `locales/`, which is not in this leaf's write set.
        state.main.comparison.error = Some(WORKER_REFUSED_ERROR.to_string());
    }
}

/// Shown when the OS refuses to create the optimizer thread.
const WORKER_REFUSED_ERROR: &str =
    "Could not start the optimizer thread - the system refused it. Try again.";

/// Tier 2 — the deterministic synergy engine.
///
/// Split out of the worker body so a test can prove the call site really hands
/// the engine a live cancellation probe. `optimize_deterministic_cancellable`
/// returns `Err("Cancelled")` the moment the probe fires; the non-cancellable
/// twin drops the probe on the floor and runs to completion, which is what made
/// production tier 2 uncancellable.
#[allow(clippy::too_many_arguments)]
fn run_deterministic_tier(
    db: &gw2_optimizer::gamedb::GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    llm_client: Option<&dyn gw2_optimizer::llm::LlmClient>,
    current_build_summary: Option<&str>,
    locks: &gw2_core::types::BuildLocks,
    scenario: &ScenarioSpec,
    on_progress: &mut dyn FnMut(gw2_optimizer::engine::OptimizeProgress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<gw2_optimizer::engine::SynergyResult, String> {
    gw2_optimizer::engine::optimize_deterministic_cancellable(
        db,
        profession_name,
        weights,
        ctx,
        llm_client,
        current_build_summary,
        locks,
        Some(scenario),
        on_progress,
        is_cancelled,
    )
}

/// Tier 3 — the legacy gear + spec search.
///
/// Same seam as [`run_deterministic_tier`]: the cancellable engine entry point
/// is the only one this flow may call, and a test pins that by driving this
/// wrapper with a probe that is already firing.
#[allow(clippy::too_many_arguments)]
fn run_legacy_tier(
    db: &gw2_optimizer::gamedb::GameDb,
    profession: &gw2_api::models::Profession,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    locks: &gw2_core::types::BuildLocks,
    top_n: usize,
    on_progress: &mut dyn FnMut(gw2_optimizer::engine::OptimizeProgress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<gw2_optimizer::engine::BuildCandidate>, String> {
    gw2_optimizer::engine::optimize_cancellable(
        profession,
        weights,
        None,
        &db.items,
        &db.itemstats,
        &db.specializations,
        &db.traits,
        on_progress,
        top_n,
        ctx,
        locks,
        &db.pvp_amulets,
        is_cancelled,
    )
}

/// Call the active LLM provider to enrich the top optimizer suggestion with AI reasoning.
/// Uses function calling (tool use) so the LLM can query game data and simulate builds.
// LLM enrichment call; config, profession, weights, mode, and candidates are
// independent inputs — grouping them adds indirection without clarity.
#[allow(clippy::too_many_arguments)]
fn enrich_with_llm(
    entry: OptimizeEntry,
    config: &gw2_core::config::AppConfig,
    profession_name: &str,
    weights: &OptimizationWeights,
    game_mode: &str,
    candidates: &[gw2_optimizer::engine::BuildCandidate],
    db: &gw2_optimizer::gamedb::GameDb,
    current_build_summary: Option<&str>,
    suggestions: &mut [crate::ui::comparison::BuildSuggestion],
    addon_dir: &std::path::Path,
    balance_ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> Result<(), String> {
    let client = gw2_optimizer::llm::create_client(config, addon_dir).map_err(|e| e.to_string())?;

    // Build tool-aware prompt. Which prompt to use is the entry point's call,
    // not something to re-derive from whether a build summary happens to be
    // non-empty: a New Build run on a geared character has a summary too.
    let mut prompt = match entry {
        OptimizeEntry::Improve => gw2_optimizer::prompts::improve_build_prompt_with_tools(
            profession_name,
            weights,
            game_mode,
        ),
        OptimizeEntry::NewBuild => {
            gw2_optimizer::prompts::new_build_prompt_with_tools(profession_name, weights, game_mode)
        }
    };
    // Handed over, not fetched — same reason as the chat path. See
    // `profession_reference`.
    prompt.push_str(&gw2_optimizer::gemini_tools::profession_reference(
        db,
        profession_name,
    ));

    let tools = gw2_optimizer::llm::tools::tool_definitions();
    let build_summary_owned = current_build_summary.map(|s| s.to_string());
    let ctx = gw2_optimizer::gemini_tools::ToolContext {
        db,
        profession_name,
        candidates,
        current_build_summary: build_summary_owned.as_deref(),
        weights: weights.clone(),
        balance_ctx,
        scenario: scenario.clone(),
    };

    let response = client
        .generate_with_tools_progress(
            &prompt,
            &tools,
            &mut |name: &str, args: &serde_json::Value| {
                gw2_optimizer::gemini_tools::execute_tool(name, args, &ctx)
            },
            // Same quota policy as the chat: a free or five-a-minute model
            // gets two lookups, then the plate.
            if client.thrifty() {
                gw2_optimizer::llm::profile::THRIFTY_TURNS
            } else {
                8
            },
            &mut |turn: usize, max_turns: usize, tool_names: &[String]| {
                let tools_str = humanize_tool_names(tool_names);
                crate::state::with_state(|s| {
                    s.main.optimize_stage = tf(
                        "fmt.ai_thinking",
                        &[
                            ("turn", &turn.to_string()),
                            ("max", &max_turns.to_string()),
                            ("tools", &tools_str),
                        ],
                    );
                });
            },
        )
        .map_err(|e| e.to_string())?;

    let gemini_build = gw2_optimizer::prompts::parse_gemini_build(&response)
        .map_err(|e| format!("Parse failed: {}", e))?;

    // Validate LLM's output against GameDb before applying
    let validated =
        gw2_optimizer::validation::validate_gemini_build(&gemini_build, db, profession_name);
    if !validated.errors.is_empty() {
        nexus::log::log(
            nexus::log::LogLevel::Warning,
            "GW2BuildOpt",
            format!(
                "Legacy enrichment validation errors: {}",
                validated
                    .errors
                    .iter()
                    .map(|e| e.detail.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        );
        // Keep the engine candidate. Chat plates already sit behind
        // plate_is_servable; this leftover path must not stamp a raw
        // invalid plate onto a ranked suggestion.
        return Ok(());
    }

    crate::state::with_state(|s| {
        s.main.optimize_stage = t("status.applying");
    });

    if let Some(first) = suggestions.first_mut() {
        apply_gemini_response(first, &gemini_build);
        // Validator-resolved per-slot prefixes are the authoritative gear data.
        first.slot_prefixes = Some(validated.gear_slots.clone());
        // Run rotation simulation now that LLM has populated skills
        simulate_suggestion_rotation(first, db, balance_ctx);
    }

    Ok(())
}

// Improve always-better baseline gate (spec §12.4: Improve locks existing gear)

/// Diagnostics from the always-better gate.
///
/// `nexus::log::log` needs the Nexus API table, which unit tests do not have
/// (same reason `state::worker_log` exists), and the gate is unit-tested, so
/// test builds go to stderr.
fn gate_log(level: nexus::log::LogLevel, message: String) {
    #[cfg(test)]
    {
        let _ = level;
        eprintln!("[GW2BuildOpt] {}", message);
    }
    #[cfg(not(test))]
    nexus::log::log(level, "GW2BuildOpt", message);
}

/// The user's own current build, once evaluated under this run's weights.
#[derive(Debug)]
struct ImproveBaseline {
    validated: gw2_optimizer::validation::ValidatedBuild,
    report: gw2_optimizer::referee::RefereeReport,
}

/// What the always-better gate found. Three states, not `Option`: "New Build,
/// no baseline wanted" and "Improve, baseline wanted but not obtainable" have
/// to be told apart, because only the second one owes the player an
/// explanation. Collapsing them into `None` is how the always-better promise
/// used to lapse in silence on any character with an unresolvable piece.
enum BaselineCapture {
    /// New Build — nothing to beat, by design.
    NotRequested,
    /// Improve — the player's gear ranked and ready to gate against.
    Ranked(Box<ImproveBaseline>),
    /// Improve — the gear could not be ranked. The run is ungated and the
    /// carried reason says so on the served result.
    Unavailable(gw2_optimizer::data::quality::DataQualityReason),
}

/// What the legacy tier is allowed to do on this run.
///
/// Tier 3 returns `BuildCandidate`s the referee never ranks, so it cannot show
/// it beat the player's gear. An armed gate therefore means "do not serve tier
/// 3 at all" rather than "serve it and hope".
#[derive(Debug, Clone, Copy)]
enum LegacyTier<'a> {
    /// New Build — run it, nothing to be better than.
    Run,
    /// Improve with no rankable baseline — run it, but the always-better
    /// promise does not cover the result and the reason must be shown.
    RunUngated(&'a gw2_optimizer::data::quality::DataQualityReason),
    /// Improve with a ranked baseline — skip tier 3 and serve that baseline.
    ServeBaseline(&'a ImproveBaseline),
}

impl BaselineCapture {
    fn legacy_tier(&self) -> LegacyTier<'_> {
        match self {
            BaselineCapture::NotRequested => LegacyTier::Run,
            BaselineCapture::Unavailable(reason) => LegacyTier::RunUngated(reason),
            BaselineCapture::Ranked(baseline) => LegacyTier::ServeBaseline(baseline),
        }
    }
}

/// Outcome of the always-better gate for the result being served.
///
/// This is the Improve tab's headline state, not a footnote: `KeptCurrentGear`
/// means the player is looking at their own gear because nothing beat it.
///
/// It reaches the UI through [`crate::ui::comparison::BuildSuggestion::label`]
/// (see [`ImproveOutcome::headline`] / [`ImproveOutcome::from_label`]), which
/// is the one field of that struct this flow writes.
// ponytail: the real shape is an `outcome: ImproveOutcome` field on
// `BuildSuggestion`. `ui/comparison.rs` is not in this leaf's write set; add
// the field and drop `from_label` when that file is next open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImproveOutcome {
    /// No gate ran: New Build, or an Improve run with no rankable baseline.
    Ungated,
    /// The optimizer outranked the player's current gear.
    Improved,
    /// The gate refused the optimizer result and served the player's gear back.
    KeptCurrentGear,
}

/// Headline the gate stamps when it refuses the optimizer result.
///
/// A plain English constant, matching `BASELINE_KEPT_REASON` below and the
/// `"Optimized Build"` fallback in `optimization.rs`: adding a catalogue key
/// needs `locales/`, which is not in this leaf's write set.
// ponytail: move behind `t("improve.kept")` when `locales/` is next open, and
// switch `from_label` to read the outcome off the suggestion instead of its
// rendered text.
const KEPT_GEAR_HEADLINE: &str = "Kept your gear - nothing beat it";

impl ImproveOutcome {
    /// The headline this outcome owns, or `None` when the ordinary build label
    /// already says everything there is to say.
    pub(super) fn headline(self) -> Option<&'static str> {
        match self {
            ImproveOutcome::Ungated | ImproveOutcome::Improved => None,
            ImproveOutcome::KeptCurrentGear => Some(KEPT_GEAR_HEADLINE),
        }
    }

    /// Recover the gate outcome a served label encodes. `None` means the label
    /// is an ordinary build label the gate does not own.
    pub(super) fn from_label(label: &str) -> Option<Self> {
        (label == KEPT_GEAR_HEADLINE).then_some(ImproveOutcome::KeptCurrentGear)
    }
}

/// Label to stamp on the served suggestion.
///
/// `KeptCurrentGear` wins over the locked-elite-spec label: "Improved:
/// Firebrand" on the player's own unchanged gear is a lie, and blanking the
/// label (the previous behaviour) left the refusal invisible.
///
/// `Ungated` also refuses `fmt.improved`: the always-better gate did not
/// rank this result, so "Improved: {name}" would be a lie even when an
/// elite spec is locked. The ordinary build label stays.
fn improve_label_override(outcome: ImproveOutcome, locked_spec: Option<&str>) -> Option<String> {
    match outcome {
        ImproveOutcome::KeptCurrentGear => outcome.headline().map(str::to_string),
        ImproveOutcome::Improved => locked_spec.map(|name| tf("fmt.improved", &[("name", name)])),
        ImproveOutcome::Ungated => None,
    }
}

/// Message shown when the optimizer could not beat the user's current gear.
const BASELINE_KEPT_REASON: &str =
    "Your current gear already outperforms every candidate for these weights — kept your build.";

/// Why an Improve run could not be gated.
fn baseline_unavailable_reason(
    profession_name: &str,
    ctx: &BalanceContext,
    detail: &str,
) -> gw2_optimizer::data::quality::DataQualityReason {
    gw2_optimizer::data::quality::DataQualityReason {
        field: "improve.baseline".into(),
        entity: profession_name.into(),
        modes: vec![ctx.game_mode.label().to_string()],
        explanation: format!(
            "Could not rank your current gear, so this result is NOT guaranteed to beat it: {}",
            detail
        ),
    }
}

/// Capture and rank the user's current build for the Improve entry point.
///
/// The entry point is passed in, never inferred: a New Build run is
/// [`BaselineCapture::NotRequested`] even when the character is wearing a full
/// kit. An Improve run that cannot be validated is
/// [`BaselineCapture::Unavailable`] — logged, and carried to the player — never
/// a silent ungate.
#[allow(clippy::too_many_arguments)]
fn capture_improve_baseline(
    entry: OptimizeEntry,
    loadout: Option<&gw2_core::types::ResolvedBuild>,
    db: &gw2_optimizer::gamedb::GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> BaselineCapture {
    if !entry.wants_baseline() {
        return BaselineCapture::NotRequested;
    }
    let Some(loadout) = loadout else {
        let reason = baseline_unavailable_reason(
            profession_name,
            ctx,
            "your current build has not been resolved yet",
        );
        gate_log(
            nexus::log::LogLevel::Warning,
            format!("Improve baseline gate disabled: {}", reason.explanation),
        );
        return BaselineCapture::Unavailable(reason);
    };
    let plate = baseline_plate_from_loadout(loadout);
    let validated = gw2_optimizer::validation::validate_gemini_build(&plate, db, profession_name);
    if !validated.errors.is_empty() {
        let detail = validated
            .errors
            .iter()
            .map(|e| e.detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let reason = baseline_unavailable_reason(profession_name, ctx, &detail);
        gate_log(
            nexus::log::LogLevel::Warning,
            format!("Improve baseline gate disabled: {}", reason.explanation),
        );
        return BaselineCapture::Unavailable(reason);
    }
    let report = gw2_optimizer::referee::evaluate_validated_build(
        &validated,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
    );
    BaselineCapture::Ranked(Box::new(ImproveBaseline { validated, report }))
}

/// Cancel the optimize run in flight.
///
/// Same mechanism the chat's Stop uses: `cancel_and_renew` cancels the token
/// every worker on this run holds and hands out a fresh one, so the next run
/// is unaffected. The pick-evaluation token is deliberately NOT touched -
/// measuring a published reference is not the run the player asked to stop.
///
/// The flags stay set: the worker's own cancelled path clears `optimizing`
/// and `comparison.loading` when it actually winds down, which is what keeps
/// the banner honest. It adds no suggestion and shows no error.
pub(super) fn stop_optimization(state: &mut AddonState) {
    if !state.main.optimizing {
        return;
    }
    state.cancel_and_renew();
    // The progress callbacks all return early once the token is cancelled, so
    // nothing overwrites this until the worker clears the flag.
    state.main.optimize_stage = t("status.stopping");
    nexus::log::log(
        nexus::log::LogLevel::Warning,
        "GW2 Build Optimizer",
        "optimization stopped by the player",
    );
}

/// The scenario an optimize run is judged under.
///
/// Shared with the community-reference tabs in `provider_picks`: a published
/// build is only comparable to the player's own once both were measured in
/// the same scenario, so there is exactly one place that builds it.
pub(super) fn scenario_for_run(
    ctx: &BalanceContext,
    combat_tier: gw2_optimizer::scenario::CombatTier,
    selected_role: Option<gw2_optimizer::scenario::RoleObjective>,
    weights: &OptimizationWeights,
) -> ScenarioSpec {
    // One builder, in the library, so the picks panel, the example and the
    // corpus test cannot drift into asking different questions.
    ScenarioSpec::for_request(ctx, combat_tier, selected_role, weights)
}

/// The combat scale a mode is judged at.
pub(super) fn combat_tier_for(
    game_mode: &gw2_core::types::GameMode,
    wvw_tier: gw2_optimizer::scenario::CombatTier,
) -> gw2_optimizer::scenario::CombatTier {
    match game_mode {
        gw2_core::types::GameMode::WvW => wvw_tier,
        gw2_core::types::GameMode::PvP => gw2_optimizer::scenario::CombatTier::Solo,
        gw2_core::types::GameMode::PvE => gw2_optimizer::scenario::CombatTier::Party,
    }
}

/// Re-materialise the player's own ranked build as something servable.
fn kept_baseline_result(
    baseline: &ImproveBaseline,
    db: &gw2_optimizer::gamedb::GameDb,
    profession_name: &str,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> gw2_optimizer::engine::SynergyResult {
    let mut kept = gw2_optimizer::engine::synergy_result_from_validated(
        baseline.validated.clone(),
        db,
        profession_name,
        ctx,
        Some(scenario),
    );
    kept.quality_reasons
        .push(gw2_optimizer::data::quality::DataQualityReason {
            field: "improve.baseline".into(),
            entity: profession_name.into(),
            modes: vec![ctx.game_mode.label().to_string()],
            explanation: BASELINE_KEPT_REASON.to_string(),
        });
    kept
}

/// Serve-time gate: whichever result outranks wins lexicographically; equality
/// keeps the user's gear (no churn without a measurable win). Returns the
/// SynergyResult to serve plus the outcome the UI has to show for it.
#[allow(clippy::too_many_arguments)]
fn apply_improve_baseline_gate(
    result: gw2_optimizer::engine::SynergyResult,
    baseline: &BaselineCapture,
    db: &gw2_optimizer::gamedb::GameDb,
    profession_name: &str,
    weights: &OptimizationWeights,
    ctx: &BalanceContext,
    scenario: &ScenarioSpec,
) -> (gw2_optimizer::engine::SynergyResult, ImproveOutcome) {
    // Ranked, not the search entry point: the serve path is not the beam's
    // hot loop, and a refused result still owes the player a real similarity
    // number instead of the collapsed axes' `None`.
    let result_report = gw2_optimizer::referee::evaluate_validated_build_ranked(
        &result.validated,
        db,
        profession_name,
        weights,
        ctx,
        scenario,
    );
    let floor = gw2_optimizer::scoring::INTENT_ALIGNMENT_FLOOR;
    let off_intent = off_intent(result_report.intent_alignment, floor);

    let baseline = match baseline {
        BaselineCapture::NotRequested => {
            let mut result = result;
            if let Some(sim) = off_intent {
                note_off_intent(&mut result, sim, floor, profession_name, ctx);
            }
            return (result, ImproveOutcome::Ungated);
        }
        BaselineCapture::Unavailable(reason) => {
            let mut result = result;
            // Not only a quality reason: those render in a hover tooltip on
            // the quality badge, so an Improve run whose always-better
            // guarantee lapsed looked exactly like one where it held. The
            // player is being served an ungated build and has to be told in
            // text they cannot miss.
            push_explanation(&mut result, &reason.explanation);
            result.quality_reasons.push(reason.clone());
            gate_log(
                nexus::log::LogLevel::Warning,
                format!(
                    "Improve baseline gate disabled, serving an ungated result: {}",
                    reason.explanation
                ),
            );
            // No baseline to keep, so an off-intent result is still served -
            // with the number on it, so the player can see what they got.
            if let Some(sim) = off_intent {
                note_off_intent(&mut result, sim, floor, profession_name, ctx);
            }
            return (result, ImproveOutcome::Ungated);
        }
        BaselineCapture::Ranked(baseline) => baseline.as_ref(),
    };
    // Intent before rank: a result that produces almost none of what was
    // asked for is the wrong build, not a lesser one, so it never replaces
    // gear that works. The always-better gate below cannot see this - it
    // compares HOW WELL, and a hammer build with 30 Healing Power can
    // outrank a Minstrel healer while producing no support output at all.
    if let Some(sim) = refuses_for_intent(off_intent, baseline.report.viability.is_viable) {
        gate_log(
                nexus::log::LogLevel::Warning,
                format!(
                    "Improve role-fit gate: result aligns {sim:.2} with the requested intent (floor {floor:.2}); the player's current build aligns {}; serving their build",
                    baseline
                        .report
                        .intent_alignment
                        .map(|a| format!("{a:.2}"))
                        .unwrap_or_else(|| "unmeasured".into()),
                ),
            );
        let mut kept = kept_baseline_result(baseline, db, profession_name, ctx, scenario);
        let text = tf(
            "improve.intent_mismatch_kept",
            &[
                ("sim", &format!("{sim:.2}")),
                ("floor", &format!("{floor:.2}")),
            ],
        );
        push_explanation(&mut kept, &text);
        kept.quality_reasons
            .push(gw2_optimizer::data::quality::DataQualityReason {
                field: "improve.intent".into(),
                entity: profession_name.into(),
                modes: vec![ctx.game_mode.label().to_string()],
                explanation: text,
            });
        return (kept, ImproveOutcome::KeptCurrentGear);
    }
    if beats_baseline(
        &gw2_optimizer::referee::search_rank(&result_report),
        &gw2_optimizer::referee::search_rank(&baseline.report),
    ) {
        let mut result = result;
        result
            .quality_reasons
            .push(gw2_optimizer::data::quality::DataQualityReason {
                field: "improve.baseline".into(),
                entity: profession_name.into(),
                modes: vec![ctx.game_mode.label().to_string()],
                explanation: improve_quality_reason(
                    result_report.user_intent_score,
                    baseline.report.user_intent_score,
                ),
            });
        (result, ImproveOutcome::Improved)
    } else {
        gate_log(
            nexus::log::LogLevel::Info,
            "Improve baseline gate: optimizer did not outrank the user's current gear; serving their build".to_string(),
        );
        let kept = kept_baseline_result(baseline, db, profession_name, ctx, scenario);
        (kept, ImproveOutcome::KeptCurrentGear)
    }
}

/// The measured alignment of a result that delivers too little of what the
/// role exists for to serve, or `None` when it clears the floor.
///
/// `intent_alignment`, not a cosine and not a weighted total: both are
/// dominated by what every build has in common. Measured over the 740-row
/// corpus the angle ranked damage references ABOVE support ones for a support
/// request, because sustain is large for everything. Scoring only the focus
/// axes takes that common mode out, and the avoid term makes the measure
/// signed - a build can be worse than nothing for a role.
///
/// `None` in means `None` out: no objective profile, or axes that were never
/// measured. Refusing a build for having no data would be the wrong refusal.
fn off_intent(alignment: Option<f64>, floor: f64) -> Option<f64> {
    alignment.filter(|a| *a < floor)
}

/// The similarity to refuse a result at, or `None` to let the rank gate decide.
///
/// Both halves matter. An off-intent result is only REFUSED when there is
/// working gear to keep instead: with no viable baseline the off-intent
/// result is the best there is, and withholding it would leave the player
/// with nothing rather than with a build plus a caveat.
fn refuses_for_intent(off_intent: Option<f64>, baseline_viable: bool) -> Option<f64> {
    off_intent.filter(|_| baseline_viable)
}

/// Append `text` to a served result's explanation, once.
///
/// The explanation is the one place the player cannot miss. Quality reasons
/// render in a hover tooltip on the quality badge, and a tooltip nobody hovers
/// is not being told.
fn push_explanation(result: &mut gw2_optimizer::engine::SynergyResult, text: &str) {
    let explanation = &mut result.validated.explanation;
    if explanation.contains(text) {
        return;
    }
    if !explanation.is_empty() {
        explanation.push_str(
            "

",
        );
    }
    explanation.push_str(text);
}

/// Mark a served result that points away from what was asked for.
///
/// Only reached when there is no viable baseline to keep instead: the result
/// is the best there is, and the player is told what it measures rather than
/// being handed it silently.
fn note_off_intent(
    result: &mut gw2_optimizer::engine::SynergyResult,
    sim: f64,
    floor: f64,
    profession_name: &str,
    ctx: &BalanceContext,
) {
    let text = tf(
        "improve.intent_mismatch",
        &[
            ("sim", &format!("{sim:.2}")),
            ("floor", &format!("{floor:.2}")),
        ],
    );
    push_explanation(result, &text);
    result
        .quality_reasons
        .push(gw2_optimizer::data::quality::DataQualityReason {
            field: "improve.intent".into(),
            entity: profession_name.into(),
            modes: vec![ctx.game_mode.label().to_string()],
            explanation: text,
        });
}

/// Lexicographic strictly-greater comparison of referee ranks. Equal ranks
/// mean the optimizer matched but did not beat the current gear.
pub(super) fn beats_baseline(result_rank: &[i64; 10], baseline_rank: &[i64; 10]) -> bool {
    result_rank > baseline_rank
}

/// Quality-reason text for a result that beat the current gear.
fn improve_quality_reason(result_intent: f64, baseline_intent: f64) -> String {
    if baseline_intent > 0.0 && result_intent >= 0.0 {
        format!(
            "Improves on your current gear (intent {:+.0}% vs your current build)",
            (result_intent - baseline_intent) / baseline_intent * 100.0
        )
    } else {
        // Baseline carried no viable intent signal — skip the percentage.
        "Improves on your current gear".into()
    }
}

/// Encode the user's CURRENT character into a Gemini plate so
/// [`gw2_optimizer::validation::validate_gemini_build`] can resolve it back
/// into a validated build — the same converter Chat plates go through.
/// Weapon sets follow `lock_panel::resolved_gear_names` indexing (set 0 →
/// Set 1 slots, the rest → Set 2); pieces use the shared `piece_gear_slot`
/// mapping. PvP has no gear pieces, so its amulet surfaces via `stat_prefix`.
pub(super) fn baseline_plate_from_loadout(
    loadout: &gw2_core::types::ResolvedBuild,
) -> gw2_optimizer::prompts::GeminiBuildResponse {
    let set_label = |index: usize| if index == 0 { 1 } else { 2 };

    let specializations: Vec<(String, Vec<String>)> = loadout
        .specializations
        .iter()
        .map(|spec| (spec.name.clone(), selected_trait_names(spec)))
        .collect();

    // "Set N: Main / Off" strings — the validator re-parses this shape.
    let mut weapons = Vec::new();
    for (index, set) in loadout.weapons.iter().enumerate() {
        let Some(main_hand) = set.main_hand.as_ref().map(|w| w.name.clone()) else {
            continue;
        };
        match set.off_hand.as_ref().map(|w| w.name.as_str()) {
            Some(off_hand) => weapons.push(format!(
                "Set {}: {} / {}",
                set_label(index),
                main_hand,
                off_hand
            )),
            None => weapons.push(format!("Set {}: {}", set_label(index), main_hand)),
        }
    }

    let mut skills = Vec::new();
    if !loadout.pets.is_empty() {
        skills.push(format!("Pets: {}", loadout.pets.join(" / ")));
    }
    if let Some(skill) = &loadout.skills.heal {
        skills.push(format!("Heal: {}", skill.name));
    }
    for skill in loadout.skills.utilities.iter().flatten() {
        skills.push(format!("Utility: {}", skill.name));
    }
    if let Some(skill) = &loadout.skills.elite {
        skills.push(format!("Elite: {}", skill.name));
    }

    // Sigils per position: [set1_main, set1_off, set2_main, set2_off].
    let mut sigils_map = std::collections::HashMap::new();
    for (index, set) in loadout.weapons.iter().enumerate() {
        let keys = [
            format!("set{}_main", set_label(index)),
            format!("set{}_off", set_label(index)),
        ];
        for (key, sigil) in keys.iter().zip(set.sigils.iter()) {
            sigils_map.insert(key.clone(), sigil.name.clone());
        }
    }

    // Per-piece prefixes over armor, trinkets, and both weapon sets.
    let mut gear_slots = std::collections::HashMap::new();
    for piece in loadout.armor.iter().chain(loadout.trinkets.iter()) {
        let prefix = piece.stat_prefix.trim();
        if prefix.is_empty() {
            continue;
        }
        if let Some(slot) = piece_gear_slot(&piece.slot) {
            gear_slots.insert(slot.kebab_name().to_string(), prefix.to_string());
        }
    }
    for (index, set) in loadout.weapons.iter().enumerate() {
        let prefix = set.stat_prefix.trim();
        if prefix.is_empty() || (set.main_hand.is_none() && set.off_hand.is_none()) {
            continue;
        }
        let prefix = prefix.to_string();
        let (main_slot, off_slot) = if index == 0 {
            (GearSlot::WeaponSet1Main, GearSlot::WeaponSet1Off)
        } else {
            (GearSlot::WeaponSet2Main, GearSlot::WeaponSet2Off)
        };
        if set.main_hand.is_some() {
            gear_slots.insert(main_slot.kebab_name().to_string(), prefix.clone());
        }
        if set.off_hand.is_some() {
            gear_slots.insert(off_slot.kebab_name().to_string(), prefix.clone());
        }
    }

    // Primary weapon-set prefix first, then any piece, then PvP amulet stem.
    let stat_prefix = loadout
        .weapons
        .iter()
        .map(|s| s.stat_prefix.as_str())
        .chain(loadout.armor.iter().map(|p| p.stat_prefix.as_str()))
        .chain(loadout.trinkets.iter().map(|p| p.stat_prefix.as_str()))
        .map(str::trim)
        .find(|prefix| !prefix.is_empty())
        .map(str::to_string)
        .or_else(|| {
            loadout
                .pvp_amulet
                .as_ref()
                .map(|a| a.name.trim_end_matches(" Amulet").trim().to_string())
        });

    gw2_optimizer::prompts::GeminiBuildResponse {
        specializations,
        weapons,
        skills,
        rune: loadout
            .rune
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_default(),
        relic: loadout
            .relic
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_default(),
        stat_prefix: stat_prefix.unwrap_or_default(),
        sigils_map: Some(sigils_map),
        gear_slots: Some(gear_slots),
        ..gw2_optimizer::prompts::GeminiBuildResponse::default()
    }
}

/// The three selected major traits per column, falling back to any column
/// option marked selected — same sourcing as Chat plates.
fn selected_trait_names(spec: &gw2_core::types::ResolvedSpec) -> Vec<String> {
    let mut traits: Vec<(usize, String)> = spec
        .traits_selected
        .iter()
        .filter(|trait_| trait_.selected && trait_.column < 3)
        .map(|trait_| (trait_.column, trait_.name.clone()))
        .collect();
    for (column, options) in spec.traits_available.iter().enumerate() {
        if traits.iter().any(|(c, _)| *c == column) {
            continue;
        }
        if let Some(option) = options.iter().find(|o| o.selected) {
            traits.push((column, option.name.clone()));
        }
    }
    traits.sort_by_key(|(column, _)| *column);
    traits.into_iter().take(3).map(|(_, name)| name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scenario() -> ScenarioSpec {
        use gw2_optimizer::scenario::{CombatKind, CombatTier, OptimizationTarget, TargetProfile};
        ScenarioSpec {
            game_mode: gw2_core::types::GameMode::PvE,
            combat_tier: CombatTier::Party,
            combat_kind: CombatKind::StrikeSpike,
            target_profile: TargetProfile::Single,
            optimization_target: OptimizationTarget {
                label: "PvE".to_string(),
            },
            patch_id: None,
            objective_profile_id: None,
        }
    }

    fn test_ctx() -> BalanceContext {
        BalanceContext::new(gw2_core::types::GameMode::PvE)
    }

    /// A loadout with enough shape for `baseline_plate_from_loadout`, but no
    /// names the empty test GameDb can resolve — the "incomplete plate" case
    /// that used to ungate Improve in silence.
    fn unresolvable_loadout() -> gw2_core::types::ResolvedBuild {
        use gw2_core::types::{ResolvedBuild, ResolvedGearPiece};
        ResolvedBuild {
            profession: "Guardian".into(),
            armor: vec![ResolvedGearPiece {
                slot: "Helm".into(),
                stat_prefix: "Berserker's".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The player's own gear, ranked. `RefereeReport` has no public
    /// constructor, so it comes from the real referee over an empty build.
    fn ranked_baseline(
        db: &gw2_optimizer::gamedb::GameDb,
        ctx: &BalanceContext,
        scenario: &ScenarioSpec,
    ) -> BaselineCapture {
        let validated = gw2_optimizer::validation::ValidatedBuild::default();
        let report = gw2_optimizer::referee::evaluate_validated_build(
            &validated,
            db,
            "Guardian",
            &OptimizationWeights::preset_power_dps(),
            ctx,
            scenario,
        );
        BaselineCapture::Ranked(Box::new(ImproveBaseline { validated, report }))
    }

    fn empty_synergy_result(
        db: &gw2_optimizer::gamedb::GameDb,
        ctx: &BalanceContext,
        scenario: &ScenarioSpec,
    ) -> gw2_optimizer::engine::SynergyResult {
        gw2_optimizer::engine::synergy_result_from_validated(
            gw2_optimizer::validation::ValidatedBuild::default(),
            db,
            "Guardian",
            ctx,
            Some(scenario),
        )
    }

    #[test]
    fn improve_entry_is_explicit() {
        let db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let ctx = test_ctx();
        let scenario = test_scenario();
        let weights = OptimizationWeights::preset_power_dps();
        let loadout = unresolvable_loadout();

        // The character IS wearing gear. New Build must still refuse the
        // baseline: the entry point decides, not `current_build`.
        assert!(
            matches!(
                capture_improve_baseline(
                    OptimizeEntry::NewBuild,
                    Some(&loadout),
                    &db,
                    "Guardian",
                    &weights,
                    &ctx,
                    &scenario,
                ),
                BaselineCapture::NotRequested
            ),
            "New Build must not be gated against the character's current gear"
        );

        // Same inputs, Improve entry: the gate is asked for.
        assert!(
            !matches!(
                capture_improve_baseline(
                    OptimizeEntry::Improve,
                    Some(&loadout),
                    &db,
                    "Guardian",
                    &weights,
                    &ctx,
                    &scenario,
                ),
                BaselineCapture::NotRequested
            ),
            "Improve must ask for a baseline"
        );

        // The result alert follows the entry point too, not the loadout.
        assert_eq!(
            OptimizeEntry::NewBuild.result_tab(),
            crate::state::MainTab::NewBuild
        );
        assert_eq!(
            OptimizeEntry::Improve.result_tab(),
            crate::state::MainTab::Improve
        );
    }

    /// A build the referee refused must never be served over the player's
    /// working gear. `search_rank` carries viability as key 0, so the
    /// lexicographic compare in [`beats_baseline`] already guarantees it -
    /// this pins that, because the guarantee lives in the KEY ORDER and a
    /// later key added above viability would silently remove it.
    #[test]
    fn a_refused_result_can_never_replace_a_viable_baseline() {
        let db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let ctx = test_ctx();
        let scenario = test_scenario();
        let report = gw2_optimizer::referee::evaluate_validated_build(
            &gw2_optimizer::validation::ValidatedBuild::default(),
            &db,
            "Guardian",
            &OptimizationWeights::preset_power_dps(),
            &ctx,
            &scenario,
        );
        assert_eq!(
            gw2_optimizer::referee::search_rank(&report)[0],
            i64::from(report.viability.is_viable),
            "viability must stay key 0 of the rank, or the gate below is empty"
        );

        // Refused, and better on every other key there is.
        let refused = [
            0,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
        ];
        // Viable, and worse on every other key there is.
        let viable = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(
            !beats_baseline(&refused, &viable),
            "a refused result outranked working gear"
        );
        assert!(beats_baseline(&viable, &refused));
    }

    /// A result that delivers too little of what the role exists for never
    /// replaces working gear, and is served with the number on it when there
    /// is no working gear to keep. This is the in-game case the invariant
    /// exists for: a WvW Havoc Support Improve served a hammer build at 30
    /// Healing Power with no cleanse, which outranked a Minstrel healer on
    /// how WELL it performed while delivering none of the support the role is
    /// written for.
    #[test]
    fn an_off_intent_result_never_replaces_working_gear() {
        let floor = gw2_optimizer::scoring::INTENT_ALIGNMENT_FLOOR;

        // Below the floor is off-intent; at or above it is not.
        assert_eq!(off_intent(Some(floor - 0.01), floor), Some(floor - 0.01));
        assert_eq!(off_intent(Some(floor), floor), None);
        assert_eq!(off_intent(Some(floor + 1.0), floor), None);
        // Signed: delivering more of what the role avoids than of what it is
        // for is worse than delivering nothing, and must read that way.
        assert_eq!(off_intent(Some(-0.4), floor), Some(-0.4));
        // Never measured is not the same as measured badly. No objective
        // profile, or no rotation, is not grounds to refuse a build.
        assert_eq!(off_intent(None, floor), None);

        // Refused only when there is working gear to keep instead.
        assert_eq!(refuses_for_intent(Some(-0.4), true), Some(-0.4));
        assert_eq!(
            refuses_for_intent(Some(-0.4), false),
            None,
            "with no viable baseline the off-intent result is all there is -              serve it with the caveat rather than leaving the player nothing"
        );
        assert_eq!(refuses_for_intent(None, true), None);
    }

    /// The alignment key sits directly under viability, so a build written
    /// for a different role loses however much better it performs.
    #[test]
    fn intent_alignment_outranks_every_performance_key() {
        let on_intent = [1, 900_000, 0, 0, 0, 0, 0, 0, 0, 0];
        let off_intent_but_better = [
            1,
            -400_000,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
        ];
        assert!(!beats_baseline(&off_intent_but_better, &on_intent));
        assert!(beats_baseline(&on_intent, &off_intent_but_better));
    }

    #[test]
    fn failed_baseline_is_visible() {
        let db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let ctx = test_ctx();
        let scenario = test_scenario();
        let weights = OptimizationWeights::preset_power_dps();

        // An Improve run whose gear cannot be validated.
        let capture = capture_improve_baseline(
            OptimizeEntry::Improve,
            Some(&unresolvable_loadout()),
            &db,
            "Guardian",
            &weights,
            &ctx,
            &scenario,
        );
        let BaselineCapture::Unavailable(reason) = &capture else {
            panic!("an unresolvable loadout must report Unavailable, not vanish");
        };
        assert_eq!(reason.field, "improve.baseline");
        assert!(
            reason.explanation.contains("NOT guaranteed"),
            "the player has to be told the guarantee lapsed: {}",
            reason.explanation
        );

        // …and that reason has to ride along on whatever gets served.
        let (served, outcome) = apply_improve_baseline_gate(
            empty_synergy_result(&db, &ctx, &scenario),
            &capture,
            &db,
            "Guardian",
            &weights,
            &ctx,
            &scenario,
        );
        assert_eq!(outcome, ImproveOutcome::Ungated);
        assert!(
            served
                .quality_reasons
                .iter()
                .any(|r| r.field == "improve.baseline"),
            "served result carries no improve.baseline reason: {:?}",
            served.quality_reasons
        );
        // A tooltip the player never hovers is not being told. The lapse has
        // to reach the explanation text too.
        assert!(
            served.validated.explanation.contains("NOT guaranteed"),
            "the ungate reason never reached the explanation: {:?}",
            served.validated.explanation
        );

        // A New Build run has nothing to explain and must stay quiet.
        let (clean, clean_outcome) = apply_improve_baseline_gate(
            empty_synergy_result(&db, &ctx, &scenario),
            &BaselineCapture::NotRequested,
            &db,
            "Guardian",
            &weights,
            &ctx,
            &scenario,
        );
        assert_eq!(clean_outcome, ImproveOutcome::Ungated);
        assert!(
            !clean
                .quality_reasons
                .iter()
                .any(|r| r.field == "improve.baseline"),
            "New Build must not be told its baseline failed"
        );
        assert!(
            !clean.validated.explanation.contains("NOT guaranteed"),
            "New Build must not carry an ungate note either"
        );
    }

    #[test]
    fn legacy_optimize_is_gated() {
        let db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let ctx = test_ctx();
        let scenario = test_scenario();

        // New Build: tier 3 runs, nothing to be better than.
        assert!(matches!(
            BaselineCapture::NotRequested.legacy_tier(),
            LegacyTier::Run
        ));

        // Improve with a ranked baseline: tier 3 has no referee ranking of its
        // own, so it must NOT be served — this is the ungated escape hatch.
        assert!(
            matches!(
                ranked_baseline(&db, &ctx, &scenario).legacy_tier(),
                LegacyTier::ServeBaseline(_)
            ),
            "a gated Improve run must not fall through to an unranked legacy result"
        );

        // Improve whose baseline failed: tier 3 runs, but loudly.
        let unavailable = BaselineCapture::Unavailable(baseline_unavailable_reason(
            "Guardian",
            &ctx,
            "test detail",
        ));
        let LegacyTier::RunUngated(reason) = unavailable.legacy_tier() else {
            panic!("an unavailable baseline must still run tier 3, with a reason attached");
        };
        assert_eq!(reason.field, "improve.baseline");
    }

    #[test]
    fn kept_gear_is_first_class_state() {
        let db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let ctx = test_ctx();
        let scenario = test_scenario();
        let weights = OptimizationWeights::preset_power_dps();

        // Baseline and result are the same empty build, so the result ties and
        // the gate keeps the player's gear (equality is not a win).
        let (kept, outcome) = apply_improve_baseline_gate(
            empty_synergy_result(&db, &ctx, &scenario),
            &ranked_baseline(&db, &ctx, &scenario),
            &db,
            "Guardian",
            &weights,
            &ctx,
            &scenario,
        );
        assert_eq!(
            outcome,
            ImproveOutcome::KeptCurrentGear,
            "a tie must keep the player's gear"
        );

        // The outcome is a state the UI can render, not only a footnote: it
        // owns a headline, that headline is what the served suggestion is
        // labelled with, and the label decodes back to the same state.
        let headline = outcome
            .headline()
            .expect("the refusal has to say something");
        assert!(!headline.trim().is_empty());
        let label = improve_label_override(outcome, Some("Firebrand"))
            .expect("kept gear must not be served with a blank label");
        assert_eq!(
            label, headline,
            "kept gear must not be relabelled 'Improved: Firebrand'"
        );
        assert_eq!(
            ImproveOutcome::from_label(&label),
            Some(ImproveOutcome::KeptCurrentGear)
        );
        // An ordinary build label is not the gate's business.
        assert_eq!(ImproveOutcome::from_label("Optimized Build"), None);
        assert_eq!(ImproveOutcome::from_label(""), None);

        // The footnote is still there — it is just no longer the only signal.
        assert!(kept
            .quality_reasons
            .iter()
            .any(|r| r.explanation == BASELINE_KEPT_REASON));
    }

    #[test]
    fn ungated_elite_lock_does_not_stamp_improved() {
        // A18-1: Improve/Ungated + elite lock must not claim the kit was
        // improved. The gate did not rank this result; "Improved: Firebrand"
        // is the same lie KeptCurrentGear already refuses.
        assert_eq!(
            improve_label_override(ImproveOutcome::Ungated, Some("Firebrand")),
            None,
            "Ungated + lock must not stamp fmt.improved"
        );
        assert_eq!(improve_label_override(ImproveOutcome::Ungated, None), None);

        // Improved + lock remains the honest use of fmt.improved. Pin the
        // production arm so dropping the stamp cannot hide behind Ungated.
        let src = include_str!("optimize_flow.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");
        let start = production
            .find("fn improve_label_override(")
            .expect("improve_label_override gone");
        let after = &production[start..];
        let end = after[1..]
            .find("\nfn ")
            .map(|i| i + 1)
            .expect("improve_label_override has no following fn");
        let body = &after[..end];
        assert!(
            body.contains("fmt.improved"),
            "Improved + lock must still stamp fmt.improved"
        );
        assert!(
            !body.contains("ImproveOutcome::Improved | ImproveOutcome::Ungated")
                && !body.contains("ImproveOutcome::Ungated | ImproveOutcome::Improved"),
            "Ungated must not share the fmt.improved arm"
        );
    }

    #[test]
    fn leftover_enrich_skips_apply_when_validation_has_errors() {
        // A18-2: leftover enrich_with_llm must not apply_gemini_response on
        // the raw plate when validate_gemini_build reported errors. Keep the
        // engine candidate. Chat already sits behind plate_is_servable; this
        // path does not.
        let src = include_str!("optimize_flow.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");
        let start = production
            .find("fn enrich_with_llm(")
            .expect("enrich_with_llm gone");
        let after = &production[start..];
        let end = after[1..]
            .find("\nfn ")
            .map(|i| i + 1)
            .expect("enrich_with_llm has no following fn");
        let body = &after[..end];
        let errors_at = body
            .find("validated.errors")
            .expect("enrich_with_llm no longer checks validated.errors");
        let apply_at = body
            .find("apply_gemini_response")
            .expect("enrich_with_llm no longer applies a plate");
        assert!(
            errors_at < apply_at,
            "errors check must precede apply_gemini_response"
        );
        let between = &body[errors_at..apply_at];
        assert!(
            between.contains("return Ok(())") || between.contains("return Err("),
            "non-empty validated.errors must return before apply_gemini_response; keep the engine candidate"
        );
    }

    #[test]
    fn leftover_empty_kit_is_not_served() {
        let src = include_str!("optimize_flow.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");
        assert!(
            production.contains("legacy leftover kit is empty"),
            "New Build leftover must not serve an empty Verified kit"
        );
    }

    #[test]
    fn optimize_flow_calls_cancellable_entry_points() {
        let db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let ctx = test_ctx();
        let scenario = test_scenario();
        let weights = OptimizationWeights::preset_power_dps();
        let locks = gw2_core::types::BuildLocks::default();
        let cancelled = || true;
        let mut progress = |_: gw2_optimizer::engine::OptimizeProgress| {};

        // Tier 2: a probe that is already firing must come straight back out.
        // The non-cancellable twin ignores the probe and runs the pipeline,
        // which on this empty GameDb fails with a different message.
        assert_eq!(
            run_deterministic_tier(
                &db,
                "Guardian",
                &weights,
                &ctx,
                None,
                None,
                &locks,
                &scenario,
                &mut progress,
                &cancelled,
            )
            .err(),
            Some("Cancelled".to_string()),
            "tier 2 does not observe cancellation"
        );

        // Tier 3: same.
        let profession = gw2_api::models::Profession {
            id: "Guardian".into(),
            name: "Guardian".into(),
            code: Some(1),
            specializations: vec![],
            weapons: std::collections::HashMap::new(),
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        };
        assert_eq!(
            run_legacy_tier(
                &db,
                &profession,
                &weights,
                &ctx,
                &locks,
                5,
                &mut progress,
                &cancelled,
            )
            .err(),
            Some("Cancelled".to_string()),
            "tier 3 does not observe cancellation"
        );

        // The worker body must reach the engine through those wrappers. Cut the
        // test module off the haystack first: the literals in THIS assertion
        // live in the same file, and a whole-file search would match them and
        // green the gate on reverted production code.
        let src = include_str!("optimize_flow.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first chunk");
        assert!(
            !production.contains("engine::optimize_deterministic("),
            "tier 2 call site reverted to the non-cancellable entry point"
        );
        assert!(
            !production.contains("engine::optimize("),
            "tier 3 call site reverted to the non-cancellable entry point"
        );
        // The `match `/`= ` prefixes pin the CALL SITES: the wrapper
        // definitions further up would satisfy a bare name search on their own.
        assert!(
            production.contains("match run_deterministic_tier("),
            "the tier 2 call site no longer goes through the cancellable wrapper"
        );
        assert!(
            production.contains("= run_legacy_tier("),
            "the tier 3 call site no longer goes through the cancellable wrapper"
        );
    }

    #[test]
    fn equal_rank_keeps_user_gear() {
        let rank = [
            1i64, 900_000, 6, 400_000, 500_000, 200_000, 700, 1_000_000, 10, 5,
        ];
        assert!(!beats_baseline(&rank, &rank));
    }

    #[test]
    fn lower_rank_keeps_user_gear() {
        // Losing on an early key is decisive regardless of later dominance.
        assert!(!beats_baseline(
            &[1, 900_000, 5, 999_999, 999_999, 999_999, 999_999, 999_999, 999_999, 999],
            &[1, 900_000, 6, 0, 0, 0, 0, 0, 0, 0]
        ));
    }

    #[test]
    fn later_key_breaks_leading_tie() {
        // WvW-shaped ranks: intent tied, raw direction decides.
        assert!(beats_baseline(
            &[1, 900_000, 6, 1, 1, 500_000, 250_000, 700, 1_000_000, 10],
            &[1, 900_000, 6, 1, 1, 500_000, 240_000, 700, 1_000_000, 9]
        ));
    }

    #[test]
    fn earlier_key_wins_over_bigger_later_key() {
        assert!(beats_baseline(
            &[1, 900_000, 6, 300_000, -50, 0, 100, 0, 0, -50],
            &[1, 900_000, 6, 299_000, 0, 0, 999_999, 0, 0, 999]
        ));
    }

    #[test]
    fn improve_reason_reports_percent_delta() {
        assert_eq!(
            improve_quality_reason(0.12, 0.10),
            "Improves on your current gear (intent +20% vs your current build)"
        );
    }

    #[test]
    fn improve_reason_skips_percentage_without_baseline_signal() {
        assert_eq!(
            improve_quality_reason(-1.0, -1.0),
            "Improves on your current gear"
        );
    }

    #[test]
    fn plate_encodes_loadout_for_validation() {
        use gw2_core::types::{
            ResolvedGearPiece, ResolvedSkills, ResolvedSpec, ResolvedTrait, ResolvedUpgrade,
            ResolvedWeaponSet, SkillInfo, TraitOption, UpgradeInfo, WeaponInfo,
        };

        let mut spec = ResolvedSpec {
            id: 52,
            name: "Firebrand".into(),
            elite: true,
            ..Default::default()
        };
        spec.traits_selected = vec![
            ResolvedTrait {
                id: 11,
                name: "Stalwart Might".into(),
                column: 1,
                selected: true,
                ..Default::default()
            },
            ResolvedTrait {
                id: 10,
                name: "Unbroken Lines".into(),
                column: 0,
                selected: true,
                ..Default::default()
            },
        ];
        spec.traits_available = vec![
            Vec::new(),
            Vec::new(),
            vec![TraitOption {
                id: 12,
                name: "Tome of Courage".into(),
                selected: true,
            }],
        ];

        let loadout = gw2_core::types::ResolvedBuild {
            specializations: vec![spec],
            skills: ResolvedSkills {
                heal: Some(SkillInfo {
                    id: 1,
                    name: "Mantra of Flame".into(),
                }),
                utilities: vec![Some(SkillInfo {
                    id: 2,
                    name: "Purging Flames".into(),
                })],
                elite: None,
            },
            weapons: vec![ResolvedWeaponSet {
                label: "Set 1".into(),
                stat_prefix: "Berserker's".into(),
                main_hand: Some(WeaponInfo {
                    name: "Greatsword".into(),
                    ..Default::default()
                }),
                off_hand: None,
                sigils: vec![UpgradeInfo {
                    id: 7,
                    name: "Superior Sigil of Air".into(),
                }],
            }],
            armor: vec![ResolvedGearPiece {
                slot: "Helm".into(),
                stat_prefix: "Valkyrie".into(),
                ..Default::default()
            }],
            trinkets: vec![
                ResolvedGearPiece {
                    slot: "Amulet".into(),
                    stat_prefix: "Marauder's".into(),
                    ..Default::default()
                },
                ResolvedGearPiece {
                    slot: "Ring1".into(),
                    stat_prefix: String::new(),
                    ..Default::default()
                },
            ],
            rune: Some(ResolvedUpgrade {
                id: 3,
                name: "Superior Rune of the Thief".into(),
            }),
            ..Default::default()
        };

        let plate = baseline_plate_from_loadout(&loadout);

        // Traits sorted by column with the availability fallback applied.
        assert_eq!(
            plate.specializations[0].0, "Firebrand",
            "spec display name must pass through unchanged"
        );
        assert_eq!(
            plate.specializations[0].1,
            vec!["Unbroken Lines", "Stalwart Might", "Tome of Courage"]
        );
        assert_eq!(plate.weapons, vec!["Set 1: Greatsword"]);
        assert_eq!(
            plate.skills,
            vec!["Heal: Mantra of Flame", "Utility: Purging Flames"]
        );
        assert_eq!(plate.rune, "Superior Rune of the Thief");
        assert_eq!(plate.relic, "");
        assert_eq!(plate.stat_prefix, "Berserker's");

        let gear = plate.gear_slots.expect("per-slot map present");
        assert_eq!(gear.get("helm").map(String::as_str), Some("Valkyrie"));
        assert_eq!(gear.get("amulet").map(String::as_str), Some("Marauder's"));
        assert_eq!(
            gear.get("weapon-set-1-main").map(String::as_str),
            Some("Berserker's")
        );
        assert!(
            !gear.contains_key("ring-1"),
            "empty prefix pieces are skipped"
        );
        assert!(!gear.contains_key("weapon-set-1-off"));

        let sigils = plate.sigils_map.expect("sigil position map present");
        assert_eq!(
            sigils.get("set1_main").map(String::as_str),
            Some("Superior Sigil of Air")
        );
    }

    #[test]
    fn pvp_amulet_seeds_stat_prefix_when_no_pieces() {
        let loadout = gw2_core::types::ResolvedBuild {
            pvp_amulet: Some(gw2_core::types::ResolvedPvpAmulet {
                name: "Berserker Amulet".into(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let plate = baseline_plate_from_loadout(&loadout);

        assert_eq!(plate.stat_prefix, "Berserker");
        assert!(plate.specializations.is_empty());
        assert_eq!(plate.weapons, Vec::<String>::new());
        assert!(!plate.gear_slots.unwrap().contains_key("amulet"));
    }
}
