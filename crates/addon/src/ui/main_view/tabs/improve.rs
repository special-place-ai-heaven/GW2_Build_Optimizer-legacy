//! Improve tab — current vs optimized side-by-side, lock panel.

use nexus::imgui::{ChildWindow, Ui};

use crate::state::AddonState;
use crate::ui::comparison::ResultPane;
use crate::ui::theme;
use gw2_core::i18n::{t, tf};

use super::super::optimize_flow::ImproveOutcome;
use super::{build_display, lock_panel, render_optimization_progress};

pub(in crate::ui::main_view) fn render_improve_tab(ui: &Ui, state: &mut AddonState) {
    if state.main.build_loading {
        ui.text_colored(theme::WARN, t("status.resolving_api"));
        return;
    }

    // Error display
    if let Some(err) = state.main.comparison.error.clone() {
        ui.text_colored(theme::ERR, format!("[!] {}", err));
        ui.same_line();
        if ui.small_button(format!("{}##opt_err_improve", t("btn.dismiss"))) {
            state.main.comparison.error = None;
        }
        ui.spacing();
    }

    // Optimization progress banner
    if state.main.optimizing {
        let stopping = state.main.optimize_stage == t("status.stopping");
        if render_optimization_progress(ui, &state.main.optimize_stage, ui.frame_count(), !stopping)
        {
            crate::ui::main_view::optimize_flow::stop_optimization(state);
        }
    }

    let locked_spec_name = state
        .main
        .build_locks
        .specs
        .get(2)
        .and_then(|s| *s)
        .and_then(|id| {
            state
                .main
                .game_db
                .as_ref()
                .and_then(|db| db.spec(id))
                .map(|s| s.name.clone())
        });

    // Two-panel layout: Current Build | Optimized Build
    let has_suggestion = !state.main.comparison.suggestions.is_empty();
    if has_suggestion {
        // Improve shows the community cards too. It never called this, so the
        // whole feature was invisible on the tab a player reaches by asking
        // for a better version of their own build.
        crate::ui::main_view::provider_picks::refresh_provider_picks(state);
    }
    let footer = if has_suggestion {
        ui.current_font_size() + 22.0
    } else {
        0.0
    };

    if state.main.current_build.is_some() {
        // Clone build data upfront to avoid borrow conflicts with mutable lock_panel state
        let build = state.main.current_build.clone().unwrap();
        let stats = state.main.current_stats.clone();
        let profession_name = build.profession.clone();
        let current_specs: Vec<(u32, Vec<u32>)> = build
            .specializations
            .iter()
            .map(|spec| {
                let selected_ids: Vec<u32> = spec
                    .traits_selected
                    .iter()
                    .filter(|t| t.selected)
                    .map(|t| t.id)
                    .collect();
                (spec.id, selected_ids)
            })
            .collect();

        if has_suggestion {
            // One tinted tab per build, the equipped one first (specs/006 US2).
            crate::ui::comparison::render_tab_strip(ui, &mut state.main.comparison, true);
            ui.spacing();

            // Gate outcome banner
            // "We could not beat this" is a state of the Improve tab, not a
            // line buried in the quality footnotes: the player waited through a
            // full optimization and is now looking at their own gear. Show it
            // above the panes, before anything that reads like a result.
            let selected = state
                .main
                .comparison
                .selected_suggestion
                .min(state.main.comparison.suggestions.len() - 1);
            let outcome =
                ImproveOutcome::from_label(&state.main.comparison.suggestions[selected].label);
            if let Some(headline) = outcome.and_then(ImproveOutcome::headline) {
                theme::wrapped(ui, theme::WARN, headline);
                ui.spacing();
            }

            // One horizontal row, stable across tabs: the pane pills and the
            // view toggle first, then the lock, and last the site link, which
            // only a published tab has. Whatever comes and goes sits at the
            // end so nothing before it moves (in-game 2026-09-08).
            crate::ui::comparison::render_result_pane_tabs(
                ui,
                &mut state.main.comparison.result_pane,
            );
            if state.main.comparison.result_pane == ResultPane::Build {
                ui.same_line_with_spacing(0.0, 16.0);
                crate::ui::gear_sheet::render_view_toggle(
                    ui,
                    &mut state.main.comparison.show_optimized,
                );
            }
            if let Some(spec_name) = locked_spec_name.as_deref() {
                ui.same_line_with_spacing(0.0, 16.0);
                ui.text_colored(theme::OPTIMIZED, tf("fmt.locked", &[("name", spec_name)]));
                ui.same_line();
                if ui.small_button(format!("{}##improve", t("btn.unlock"))) {
                    state.main.build_locks.specs[2] = None;
                }
            }
            crate::ui::comparison::render_source_link(
                ui,
                &state.main.comparison.suggestions[selected],
            );
            ui.spacing();

            let scroll_height = (ui.content_region_avail()[1] - footer).max(64.0);

            let idx = state
                .main
                .comparison
                .selected_suggestion
                .min(state.main.comparison.suggestions.len() - 1);
            let suggestion = state.main.comparison.suggestions[idx].clone();
            let pane = state.main.comparison.result_pane;
            let db_ref = state.main.game_db.as_deref();
            let gain = crate::ui::gear_sheet::combat_gain(
                state.main.comparison.current_combat_solo.as_ref(),
                suggestion.combat_solo.as_ref(),
            );

            ChildWindow::new("##improve_scroll")
                .size([0.0, scroll_height])
                .build(ui, || match pane {
                    ResultPane::Build => {
                        let viewing = state.main.comparison.show_optimized;
                        if viewing {
                            build_display::render_suggestion_skills(
                                ui,
                                &suggestion,
                                db_ref,
                                Some(&build),
                            );
                        } else {
                            build_display::render_build_skills(ui, &build, db_ref);
                        }
                        ui.spacing();
                        if viewing {
                            lock_panel::render_optimized_specs_panel(
                                ui,
                                db_ref.map(|db| db as &gw2_optimizer::gamedb::GameDb),
                                &suggestion.specializations,
                                &t("section.optimized_specs"),
                                Some(&crate::ui::comparison::spec_pairs_from_build(&build)),
                            );
                        } else {
                            let mut specs_open = true;
                            lock_panel::render_lock_panel(
                                ui,
                                &mut state.main.build_locks,
                                &mut specs_open,
                                db_ref.map(|db| db as &gw2_optimizer::gamedb::GameDb),
                                &profession_name,
                                &current_specs,
                                &build,
                                &mut state.main.locks_hover,
                            );
                        }
                        ui.spacing();
                        crate::ui::gear_sheet::render_current_sheet(
                            ui,
                            &build,
                            Some(&suggestion),
                            db_ref,
                            viewing,
                            gain,
                            Some(&mut state.main.build_locks),
                        );
                    }
                    ResultPane::Stats => {
                        crate::ui::comparison::render_stats_pane(
                            ui,
                            stats.as_ref(),
                            &state.main.comparison,
                            &suggestion,
                            db_ref,
                        );
                    }
                });
        } else {
            // No suggestion yet — show current build with lock panel (full width)
            let scroll_height = (ui.content_region_avail()[1] - footer).max(64.0);

            ChildWindow::new("##improve_single")
                .size([0.0, scroll_height])
                .build(ui, || {
                    build_display::render_card_header(
                        ui,
                        &t("section.current_build"),
                        theme::CURRENT,
                    );
                    build_display::render_build_skills(ui, &build, state.main.game_db.as_deref());
                    {
                        let db_ref = state.main.game_db.as_deref();
                        let mut specs_open = true;
                        lock_panel::render_lock_panel(
                            ui,
                            &mut state.main.build_locks,
                            &mut specs_open,
                            db_ref.map(|db| db as &gw2_optimizer::gamedb::GameDb),
                            &profession_name,
                            &current_specs,
                            &build,
                            &mut state.main.locks_hover,
                        );
                    }
                    crate::ui::gear_sheet::render_current_sheet(
                        ui,
                        &build,
                        None,
                        state.main.game_db.as_deref(),
                        false,
                        0,
                        Some(&mut state.main.build_locks),
                    );
                });
        }
    } else if state.main.selected_character.is_some() {
        ui.text_colored(theme::pal().muted, t("improve.loading"));
    } else {
        ui.text_colored(theme::pal().muted, t("improve.select"));
    }

    if has_suggestion {
        // Below the panes rather than inside `##improve_scroll`: that child's
        // closure holds a `&GameDb` borrowed out of state for its whole body,
        // so a `&mut AddonState` call cannot go inside it.
        ui.spacing();
        if let Some(i) = crate::ui::main_view::provider_picks::render_provider_picks(ui, state) {
            crate::ui::main_view::provider_picks::adopt_provider_pick(state, i);
        }
        if crate::ui::main_view::provider_picks::take_sync_invite(ui, state) {
            state.main.active_tab = crate::state::MainTab::Settings;
        }
        ui.spacing();
    }

    if has_suggestion {
        super::saveload::render_save_build_ui(ui, state);
        ui.same_line();
        if ui.small_button(format!("{}##improve", t("btn.clear_results"))) {
            state.main.comparison.suggestions.clear();
            state.main.comparison.error = None;
        }
    }
}
