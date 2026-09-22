//! New Build tab — scenario summary + comparison view + chat refinement.
//! Role lives in the left menu (shared across modes).

use nexus::imgui::Ui;

use crate::state::AddonState;
use crate::ui::theme;
use gw2_core::i18n::t;
use gw2_optimizer::scenario::CombatTier;

use super::render_optimization_progress;

fn render_scenario_ready(ui: &Ui, state: &AddonState) {
    let mode = state.main.game_mode.label();
    let role = state
        .main
        .selected_role
        .map(|role| super::super::role_i18n_key(&state.main.game_mode, role))
        .map(t)
        .unwrap_or_else(|| t("label.pick_role"));
    let line = if state.main.game_mode == gw2_core::types::GameMode::WvW {
        let scale = match state.main.combat_tier {
            CombatTier::Solo => t("scale.roam"),
            CombatTier::Party => t("scale.havoc"),
            CombatTier::Squad => t("scale.cloud"),
        };
        format!("{} · {} · {}", mode, scale, role)
    } else {
        format!("{} · {}", mode, role)
    };
    theme::wrapped(ui, theme::pal().gold, &line);
    ui.spacing();
    if state.main.selected_role.is_none() {
        theme::wrapped(ui, theme::pal().muted, &t("new_build.pick_role"));
    } else {
        theme::wrapped(ui, theme::pal().muted, &t("new_build.family_hint"));
        ui.spacing();
        theme::wrapped(ui, theme::pal().muted, &t("new_build.click_optimize"));
    }
}

pub(in crate::ui::main_view) fn render_new_build_tab(ui: &Ui, state: &mut AddonState) {
    // A plate Choya cooked with no character selected is still a build to
    // look at; only the empty case needs the hint.
    if state.main.selected_character.is_none() && state.main.comparison.suggestions.is_empty() {
        theme::wrapped(ui, theme::pal().muted, &t("new_build.select_character"));
        return;
    }

    if let Some(err) = state.main.comparison.error.clone() {
        ui.text_colored(theme::ERR, format!("[!] {}", err));
        ui.same_line();
        if ui.small_button(format!("{}##opt_err", t("btn.dismiss"))) {
            state.main.comparison.error = None;
        }
        ui.spacing();
    }

    if state.main.optimizing {
        let stopping = state.main.optimize_stage == t("status.stopping");
        if render_optimization_progress(ui, &state.main.optimize_stage, ui.frame_count(), !stopping)
        {
            crate::ui::main_view::optimize_flow::stop_optimization(state);
        }
    }

    if state.main.comparison.suggestions.is_empty() && !state.main.optimizing {
        render_scenario_ready(ui, state);
    }

    if !state.main.comparison.suggestions.is_empty() {
        crate::ui::main_view::provider_picks::refresh_provider_picks(state);
        // Set inside the scroll child, acted on after it: switching tabs or
        // pushing a suggestion mid-render would disturb the window being
        // drawn.
        let mut go_to_settings = false;
        let mut picked: Option<usize> = None;
        let footer = ui.current_font_size() + 22.0;
        let scroll_h = (ui.content_region_avail()[1] - footer).max(64.0);
        nexus::imgui::ChildWindow::new("##new_build_scroll")
            .size([0.0, scroll_h])
            .build(ui, || {
                let stats = state.main.current_stats.clone();
                crate::ui::comparison::render_comparison(
                    ui,
                    state.main.current_build.clone().as_ref(),
                    stats.as_ref(),
                    &mut state.main.comparison,
                    state.main.game_db.as_deref(),
                );
                picked = crate::ui::main_view::provider_picks::render_provider_picks(ui, state);
                if crate::ui::main_view::provider_picks::take_sync_invite(ui, state) {
                    go_to_settings = true;
                }
            });

        if let Some(i) = picked {
            crate::ui::main_view::provider_picks::adopt_provider_pick(state, i);
        }
        if go_to_settings {
            state.main.active_tab = crate::state::MainTab::Settings;
        }

        super::saveload::render_save_build_ui(ui, state);
        ui.same_line();
        if ui.small_button(t("btn.clear_results")) {
            state.main.comparison.suggestions.clear();
            state.main.comparison.error = None;
        }
    }
}
