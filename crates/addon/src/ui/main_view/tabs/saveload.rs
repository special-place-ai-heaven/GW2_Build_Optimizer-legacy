//! Save/Load tab — saved-build list, save UI, and SavedBuild ↔ BuildSuggestion conversion.

use nexus::imgui::Ui;

use crate::state::{AddonState, MainTab};
use crate::ui::theme;
use gw2_core::i18n::{t, tf};

use super::super::{optimization, stats};

/// Render the save build UI (name input + Save button) below the comparison view.
pub(in crate::ui::main_view) fn render_save_build_ui(ui: &Ui, state: &mut AddonState) {
    if state.main.comparison.suggestions.is_empty() {
        return;
    }
    ui.text(t("save.build"));
    ui.same_line();
    ui.set_next_item_width(200.0);
    ui.input_text("##save_name", &mut state.main.save_name_input)
        .build();
    ui.same_line();

    let can_save = !state.main.save_name_input.trim().is_empty();
    let save_clicked = if can_save {
        theme::gold_button_sized(ui, t("btn.save"), [60.0, 0.0])
    } else {
        let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        theme::gold_button_sized(ui, t("btn.save"), [60.0, 0.0]);
        style.pop();
        if ui.is_item_hovered() {
            ui.tooltip_text(t("save.need_name"));
        }
        false
    };
    if save_clicked {
        let idx = state
            .main
            .comparison
            .selected_suggestion
            .min(state.main.comparison.suggestions.len().saturating_sub(1));
        let suggestion = &state.main.comparison.suggestions[idx];
        let character_name = state
            .main
            .current_build
            .as_ref()
            .map(|b| b.character_name.clone())
            .unwrap_or_default();
        let profession = state
            .main
            .current_build
            .as_ref()
            .map(|b| b.profession.clone())
            .unwrap_or_default();
        let game_mode = state.main.game_mode.clone();
        // Capture the active balance patch so saved builds remember which
        // patch they were optimized against. Lets the load-side warn the user
        // when a build is loaded under a different patch.
        let balance_ctx = gw2_optimizer::balance::BalanceContext::new(game_mode.clone());
        let saved = suggestion_to_saved(
            &state.main.save_name_input,
            &character_name,
            &profession,
            &game_mode,
            Some(&balance_ctx.patch_id),
            suggestion,
        );

        let storage = gw2_core::storage::BuildStorage::new(&state.addon_dir);
        match storage.save_new(&saved) {
            Ok(()) => {
                state.main.save_status = Some(tf("fmt.saved", &[("name", &saved.name)]));
                state.main.save_status_err = false;
                state.main.save_status_frames = 0;
                state.main.save_name_input.clear();
                state.main.saved_builds_loaded = false; // force refresh
            }
            Err(e) => {
                state.main.save_status = Some(tf("fmt.save_failed", &[("err", &e.to_string())]));
                state.main.save_status_err = true;
                state.main.save_status_frames = 0;
            }
        }
    }

    if let Some(ref status) = state.main.save_status {
        ui.same_line();
        if state.main.save_status_err {
            ui.text_colored(theme::ERR, status);
        } else {
            ui.text_colored(theme::OPTIMIZED, status);
        }
    }
}

fn selected_character_name(state: &AddonState) -> Option<String> {
    state
        .main
        .selected_character
        .and_then(|i| state.main.characters.get(i).cloned())
}

fn ranch_indices(builds: &[gw2_core::types::SavedBuild], character: Option<&str>) -> Vec<usize> {
    builds
        .iter()
        .enumerate()
        .filter(|(_, b)| character.map(|c| b.character_name == c).unwrap_or(true))
        .map(|(i, _)| i)
        .collect()
}

fn persist_notes(state: &mut AddonState, name: &str) {
    let draft = state
        .main
        .note_drafts
        .get(name)
        .cloned()
        .unwrap_or_default();
    let Some(saved) = state.main.saved_builds.iter_mut().find(|b| b.name == name) else {
        return;
    };
    if saved.notes == draft {
        return;
    }
    saved.notes = draft;
    let snapshot = saved.clone();
    let storage = gw2_core::storage::BuildStorage::new(&state.addon_dir);
    if let Err(e) = storage.save_overwrite(&snapshot) {
        state.main.error = Some(tf("fmt.save_failed", &[("err", &e.to_string())]));
    }
}

fn current_suggestion(state: &AddonState) -> Option<&crate::ui::comparison::BuildSuggestion> {
    let sug = &state.main.comparison.suggestions;
    if sug.is_empty() {
        return None;
    }
    let idx = state
        .main
        .comparison
        .selected_suggestion
        .min(sug.len().saturating_sub(1));
    sug.get(idx)
}

fn action_btn_size(ui: &Ui) -> [f32; 2] {
    let labels = [
        t("btn.load"),
        t("btn.save"),
        t("btn.delete"),
        t("btn.yes"),
        t("btn.no"),
    ];
    let w = labels
        .iter()
        .map(|s| ui.calc_text_size(s)[0])
        .fold(72.0_f32, f32::max)
        + 24.0;
    [w, theme::control_height(ui)]
}

fn clip_label(ui: &Ui, text: &str, max_w: f32) -> String {
    if ui.calc_text_size(text)[0] <= max_w {
        return text.to_string();
    }
    let mut s = String::new();
    for ch in text.chars() {
        let mut probe = s.clone();
        probe.push(ch);
        probe.push_str("...");
        if ui.calc_text_size(&probe)[0] > max_w {
            break;
        }
        s.push(ch);
    }
    s.push_str("...");
    s
}

fn toss_button(ui: &Ui, label: impl AsRef<str>, size: [f32; 2]) -> bool {
    let _bg = ui.push_style_color(nexus::imgui::StyleColor::Button, [0.62, 0.22, 0.14, 0.95]);
    let _h = ui.push_style_color(
        nexus::imgui::StyleColor::ButtonHovered,
        [0.78, 0.30, 0.18, 1.0],
    );
    let _a = ui.push_style_color(
        nexus::imgui::StyleColor::ButtonActive,
        [0.50, 0.16, 0.10, 1.0],
    );
    let _t = ui.push_style_color(nexus::imgui::StyleColor::Text, theme::pal().cream);
    ui.button_with_size(label.as_ref(), size)
}

fn muted_gold(ui: &Ui, label: impl AsRef<str>, size: [f32; 2], tip: &str) {
    let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
    theme::gold_button_sized(ui, label, size);
    style.pop();
    if ui.is_item_hovered() {
        ui.tooltip_text(tip);
    }
}

fn render_ranch_hero(ui: &Ui, char_name: Option<&str>, shown: usize, total: usize) {
    const MASCOT: f32 = 96.0;
    const PAD_L: f32 = 20.0;
    const PAD_T: f32 = 38.0;
    const PAD_R: f32 = 24.0;
    const PAD_B: f32 = 8.0;
    let box_w = PAD_L + MASCOT + PAD_R;
    let box_h = PAD_T + MASCOT + PAD_B;
    let top = ui.cursor_screen_pos();
    ui.invisible_button("##ranch_mascot", [box_w, box_h]);
    let below = ui.cursor_screen_pos();
    let center = [top[0] + PAD_L + MASCOT * 0.5, top[1] + PAD_T + MASCOT * 0.5];
    if shown == 0 {
        theme::draw_choya_sleep(ui, center, MASCOT);
    } else {
        theme::draw_choya_hero(ui, center, MASCOT);
    }

    let text_x = top[0] + box_w + 8.0;
    let ty0 = top[1] + PAD_T + 10.0;
    let lh = ui.text_line_height();
    ui.set_cursor_screen_pos([text_x, ty0]);
    ui.text_colored(theme::pal().gold, t("ranch.title"));

    ui.set_cursor_screen_pos([text_x, ty0 + lh + 6.0]);
    let sub = match char_name {
        Some(name) => tf(
            "ranch.for_char",
            &[("name", name), ("n", &shown.to_string())],
        ),
        None => t("ranch.herd"),
    };
    ui.text_colored(theme::pal().cream, sub);

    ui.set_cursor_screen_pos([text_x, ty0 + lh * 2.0 + 12.0]);
    ui.text_colored(theme::pal().muted, t("ranch.quip"));

    if char_name.is_some() && total > shown {
        ui.set_cursor_screen_pos([text_x, ty0 + lh * 3.0 + 16.0]);
        ui.text_colored(
            theme::pal().muted,
            tf("ranch.others", &[("n", &(total - shown).to_string())]),
        );
    }

    let after = ui.cursor_screen_pos();
    ui.set_cursor_screen_pos([top[0], below[1].max(after[1] + 8.0)]);
}

fn render_empty_paddock(ui: &Ui, char_name: Option<&str>) {
    ui.dummy([0.0, 8.0]);
    let msg = match char_name {
        Some(name) => tf("ranch.empty_char", &[("name", name)]),
        None => t("ranch.empty"),
    };
    ui.text_colored(theme::pal().muted, msg);
    ui.text_colored(theme::pal().muted, t("ranch.empty_hint"));
}

fn render_corral_bar(ui: &Ui, state: &mut AddonState) {
    let btn = action_btn_size(ui);
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("ranch.corral_name"));
    ui.same_line_with_spacing(0.0, 10.0);
    ui.set_next_item_width(280.0);
    ui.input_text("##ranch_new_name", &mut state.main.save_name_input)
        .build();
    ui.same_line_with_spacing(0.0, 10.0);

    let named = !state.main.save_name_input.trim().is_empty();
    let has_opt = current_suggestion(state).is_some();
    let corral = if named && has_opt {
        theme::gold_button_sized(
            ui,
            format!("{}##corral", t("ranch.corral")),
            [btn[0] + 24.0, btn[1]],
        )
    } else {
        let tip = if !has_opt {
            t("ranch.need_opt")
        } else {
            t("save.need_name")
        };
        muted_gold(
            ui,
            format!("{}##corral", t("ranch.corral")),
            [btn[0] + 24.0, btn[1]],
            &tip,
        );
        false
    };
    if corral {
        corral_current(state);
    }

    if let Some(ref status) = state.main.save_status {
        ui.same_line_with_spacing(0.0, 12.0);
        if state.main.save_status_err {
            ui.text_colored(theme::ERR, status);
        } else {
            ui.text_colored(theme::OPTIMIZED, status);
        }
    }
}

fn corral_current(state: &mut AddonState) {
    let Some(suggestion) = current_suggestion(state).cloned() else {
        return;
    };
    let character_name = selected_character_name(state)
        .or_else(|| {
            state
                .main
                .current_build
                .as_ref()
                .map(|b| b.character_name.clone())
        })
        .unwrap_or_default();
    let profession = state
        .main
        .current_build
        .as_ref()
        .map(|b| b.profession.clone())
        .unwrap_or_default();
    let game_mode = state.main.game_mode.clone();
    let balance_ctx = gw2_optimizer::balance::BalanceContext::new(game_mode.clone());
    let saved = suggestion_to_saved(
        &state.main.save_name_input,
        &character_name,
        &profession,
        &game_mode,
        Some(&balance_ctx.patch_id),
        &suggestion,
    );
    let storage = gw2_core::storage::BuildStorage::new(&state.addon_dir);
    match storage.save_new(&saved) {
        Ok(()) => {
            state.main.save_status = Some(tf("fmt.saved", &[("name", &saved.name)]));
            state.main.save_status_err = false;
            state.main.save_status_frames = 0;
            state.main.save_name_input.clear();
            state.main.saved_builds_loaded = false;
        }
        Err(e) => {
            state.main.save_status = Some(tf("fmt.save_failed", &[("err", &e.to_string())]));
            state.main.save_status_err = true;
            state.main.save_status_frames = 0;
        }
    }
}

fn overwrite_named(state: &mut AddonState, name: &str) {
    let Some(existing) = state
        .main
        .saved_builds
        .iter()
        .find(|b| b.name == name)
        .cloned()
    else {
        return;
    };
    let Some(suggestion) = current_suggestion(state).cloned() else {
        state.main.save_status = Some(t("ranch.need_opt"));
        state.main.save_status_err = true;
        state.main.save_status_frames = 0;
        return;
    };
    let profession = state
        .main
        .current_build
        .as_ref()
        .map(|b| b.profession.clone())
        .unwrap_or(existing.profession.clone());
    let game_mode = state.main.game_mode.clone();
    let balance_ctx = gw2_optimizer::balance::BalanceContext::new(game_mode.clone());
    let notes = state
        .main
        .note_drafts
        .get(name)
        .cloned()
        .unwrap_or(existing.notes.clone());
    let mut saved = suggestion_to_saved(
        name,
        &existing.character_name,
        &profession,
        &game_mode,
        Some(&balance_ctx.patch_id),
        &suggestion,
    );
    saved.timestamp = existing.timestamp;
    saved.notes = notes;
    let storage = gw2_core::storage::BuildStorage::new(&state.addon_dir);
    match storage.save_overwrite(&saved) {
        Ok(()) => {
            if let Some(slot) = state.main.saved_builds.iter_mut().find(|b| b.name == name) {
                *slot = saved.clone();
            }
            state.main.save_status = Some(tf("ranch.updated", &[("name", name)]));
            state.main.save_status_err = false;
            state.main.save_status_frames = 0;
        }
        Err(e) => {
            state.main.save_status = Some(tf("fmt.save_failed", &[("err", &e.to_string())]));
            state.main.save_status_err = true;
            state.main.save_status_frames = 0;
        }
    }
}

/// SavedBuild to suggestion with rotation sim, 3-tier combat, and chat-code.
/// Ranch Load runs this on a `spawn_worker` thread, not the draw pass.
///
/// With game data the save is measured like every other tab: the engine's
/// stat sheet and flow run for its validated plate, in `scenario`.
fn suggestion_from_saved_build(
    saved: &gw2_core::types::SavedBuild,
    game_db: Option<&gw2_optimizer::gamedb::GameDb>,
    game_mode: &gw2_core::types::GameMode,
    weights: &gw2_optimizer::scoring::OptimizationWeights,
    scenario: &gw2_optimizer::scenario::ScenarioSpec,
) -> crate::ui::comparison::BuildSuggestion {
    let mut suggestion = saved_to_suggestion(saved, game_db);
    let balance_ctx = gw2_optimizer::balance::BalanceContext::new(game_mode.clone());
    if let Some(db) = game_db {
        optimization::simulate_suggestion_rotation(
            &mut suggestion,
            db,
            saved_profession(saved),
            weights,
            &balance_ctx,
            scenario,
        );
    }
    suggestion
}

fn apply_loaded_suggestion(
    state: &mut AddonState,
    suggestion: crate::ui::comparison::BuildSuggestion,
) {
    // Pushed beside the existing suggestions, never instead of them
    // (specs/006 FR-004).
    state.main.comparison.push_or_replace(suggestion);
    state.main.comparison.error = None;
    state.main.active_tab = MainTab::NewBuild;
}

/// Apply a dirty note draft in memory and return a disk snapshot.
/// Load click must not `save_overwrite` on the draw frame; the ranch-load
/// worker writes this snapshot before rotation sim.
fn pending_note_snapshot(
    state: &mut AddonState,
    name: &str,
) -> Option<gw2_core::types::SavedBuild> {
    let draft = state
        .main
        .note_drafts
        .get(name)
        .cloned()
        .unwrap_or_default();
    let saved = state
        .main
        .saved_builds
        .iter_mut()
        .find(|b| b.name == name)?;
    if saved.notes == draft {
        return None;
    }
    saved.notes = draft;
    Some(saved.clone())
}

fn write_note_snapshot(addon_dir: &std::path::Path, snapshot: &gw2_core::types::SavedBuild) {
    let storage = gw2_core::storage::BuildStorage::new(addon_dir);
    if let Err(e) = storage.save_overwrite(snapshot) {
        crate::state::with_state(|s| {
            s.main.error = Some(tf("fmt.save_failed", &[("err", &e.to_string())]));
        });
    }
}

fn load_named(state: &mut AddonState, name: &str) {
    let notes_snapshot = pending_note_snapshot(state, name);
    let Some(saved) = state
        .main
        .saved_builds
        .iter()
        .find(|b| b.name == name)
        .cloned()
    else {
        return;
    };
    // Snapshot only. Rotation sim, 3-tier combat, and chat-code run in the worker.
    // Dirty notes overwrite also runs in this worker, before sim, so a cancel
    // before CPU does not drop the draft.
    let game_db = state.main.game_db.clone();
    let game_mode = state.main.game_mode.clone();
    let weights = state.main.weights.clone();
    // The scenario the Improve button and Choya would judge in right now.
    let scenario = crate::ui::main_view::optimize_flow::scenario_for_run(
        &gw2_optimizer::balance::BalanceContext::new(game_mode.clone()),
        crate::ui::main_view::optimize_flow::combat_tier_for(&game_mode, state.main.combat_tier),
        state.main.selected_role,
        &weights,
    );
    let addon_dir = state.addon_dir.clone();
    let notes_retry = notes_snapshot.clone();
    let addon_dir_retry = addon_dir.clone();
    let spawned = state.spawn_worker("ranch-load", move |token| {
        if let Some(snapshot) = notes_snapshot {
            write_note_snapshot(&addon_dir, &snapshot);
        }
        if token.is_cancelled() {
            return;
        }
        let suggestion = suggestion_from_saved_build(
            &saved,
            game_db.as_deref(),
            &game_mode,
            &weights,
            &scenario,
        );
        if token.is_cancelled() {
            return;
        }
        crate::state::with_state(|s| apply_loaded_suggestion(s, suggestion));
    });
    if !spawned {
        // OS refused the thread; work never started. Do not fall back to inline CPU.
        state.main.comparison.error =
            Some("Could not start the load thread - the system refused it. Try again.".into());
        if let Some(snapshot) = notes_retry {
            let worker_snapshot = snapshot.clone();
            let notes_ok = state.spawn_worker("ranch-notes", move |_token| {
                write_note_snapshot(&addon_dir_retry, &worker_snapshot);
            });
            if !notes_ok {
                save_note_now(state, &snapshot);
            }
        }
    }
}

fn save_note_now(state: &mut AddonState, snapshot: &gw2_core::types::SavedBuild) {
    let storage = gw2_core::storage::BuildStorage::new(&state.addon_dir);
    if let Err(e) = storage.save_overwrite(snapshot) {
        state.main.error = Some(tf("fmt.save_failed", &[("err", &e.to_string())]));
    }
}

fn delete_named(state: &mut AddonState, name: &str) {
    let storage = gw2_core::storage::BuildStorage::new(&state.addon_dir);
    match storage.delete(name) {
        Ok(()) => {
            state.main.saved_builds.retain(|b| b.name != name);
            state.main.note_drafts.remove(name);
            if state.main.confirm_delete.as_deref() == Some(name) {
                state.main.confirm_delete = None;
            }
        }
        Err(e) => {
            state.main.error = Some(tf("fmt.err_delete", &[("err", &e.to_string())]));
        }
    }
}

fn paint_row_plate(ui: &Ui, height: f32, header: bool) {
    let p = ui.cursor_screen_pos();
    let w = ui.content_region_avail()[0];
    let fill = if header {
        theme::with_alpha(theme::pal().header_plate, 0.7)
    } else {
        theme::with_alpha(theme::pal().plate, 0.42)
    };
    ui.get_window_draw_list()
        .add_rect(p, [p[0] + w, p[1] + height], fill)
        .filled(true)
        .rounding(5.0)
        .build();
}

fn render_ranch_table(ui: &Ui, state: &mut AddonState, rows: &[usize]) {
    const GAP: f32 = 8.0;
    const ROW_H: f32 = 56.0;
    const HDR_H: f32 = 28.0;
    let avail = ui.content_region_avail()[0];
    let btn = action_btn_size(ui);
    let actions_w = btn[0] * 3.0 + GAP * 2.0 + 12.0;
    let created_w = 150.0;
    let name_w = (avail * 0.26).clamp(150.0, 260.0);
    let notes_w = (avail - name_w - created_w - actions_w - 36.0).max(140.0);

    paint_row_plate(ui, HDR_H, true);
    let origin = ui.cursor_screen_pos();
    let y = origin[1] + 6.0;
    ui.set_cursor_screen_pos([origin[0] + 10.0, y]);
    ui.text_colored(theme::pal().gold, t("ranch.col.build"));
    ui.set_cursor_screen_pos([origin[0] + 10.0 + name_w + GAP, y]);
    ui.text_colored(theme::pal().gold, t("ranch.col.created"));
    ui.set_cursor_screen_pos([origin[0] + 10.0 + name_w + created_w + GAP * 2.0, y]);
    ui.text_colored(theme::pal().gold, t("ranch.col.notes"));
    ui.set_cursor_screen_pos([origin[0] + avail - actions_w, y]);
    ui.text_colored(theme::pal().gold, t("ranch.col.actions"));
    ui.set_cursor_screen_pos([origin[0], origin[1] + HDR_H + 6.0]);

    let mut load_name: Option<String> = None;
    let mut delete_name: Option<String> = None;
    let mut overwrite_name: Option<String> = None;
    let has_opt = current_suggestion(state).is_some();
    let snapshot: Vec<(String, String, String, String, String)> = rows
        .iter()
        .filter_map(|&i| state.main.saved_builds.get(i))
        .map(|b| {
            (
                b.name.clone(),
                format_timestamp(b.timestamp),
                b.game_mode.label().to_string(),
                b.stat_prefix.clone(),
                b.character_name.clone(),
            )
        })
        .collect();

    for (name, created, mode, prefix, character) in &snapshot {
        paint_row_plate(ui, ROW_H, false);
        let row = ui.cursor_screen_pos();
        let text_y = row[1] + 8.0;
        let btn_y = row[1] + ((ROW_H - btn[1]) * 0.5).round();

        ui.set_cursor_screen_pos([row[0] + 10.0, text_y]);
        ui.text_colored(theme::CURRENT, clip_label(ui, name, name_w - 8.0));
        ui.set_cursor_screen_pos([row[0] + 10.0, text_y + ui.text_line_height() + 2.0]);
        let meta = if state.main.selected_character.is_some() {
            format!("{mode}  ·  {prefix}")
        } else {
            format!("{character}  ·  {mode}  ·  {prefix}")
        };
        ui.text_colored(theme::pal().muted, clip_label(ui, &meta, name_w - 8.0));

        ui.set_cursor_screen_pos([row[0] + 10.0 + name_w + GAP, text_y + 8.0]);
        ui.text_colored(theme::pal().cream, created);

        let notes_x = row[0] + 10.0 + name_w + created_w + GAP * 2.0;
        ui.set_cursor_screen_pos([notes_x, btn_y]);
        ui.set_next_item_width(notes_w - 4.0);
        let stored = state
            .main
            .saved_builds
            .iter()
            .find(|b| b.name == *name)
            .map(|b| b.notes.clone())
            .unwrap_or_default();
        let keep_notes = {
            let draft = state
                .main
                .note_drafts
                .entry(name.clone())
                .or_insert_with(|| stored.clone());
            let enter = ui
                .input_text(format!("##notes_{name}"), draft)
                .enter_returns_true(true)
                .build();
            let editing = ui.is_item_active();
            if ui.is_item_hovered() {
                ui.tooltip_text(t("ranch.notes_hint"));
            }
            enter || (!editing && draft.as_str() != stored.as_str())
        };
        if keep_notes {
            persist_notes(state, name);
        }

        let mut ax = row[0] + avail - actions_w;
        ui.set_cursor_screen_pos([ax, btn_y]);
        if state.main.confirm_delete.as_deref() == Some(name.as_str()) {
            ui.text_colored(theme::WARN, t("save.delete_q"));
            ui.same_line_with_spacing(0.0, GAP);
            if theme::gold_button_sized(ui, format!("{}##yes_{name}", t("btn.yes")), btn) {
                delete_name = Some(name.clone());
                state.main.confirm_delete = None;
            }
            ui.same_line_with_spacing(0.0, GAP);
            if ui.button_with_size(format!("{}##no_{name}", t("btn.no")), btn) {
                state.main.confirm_delete = None;
            }
        } else if state.main.confirm_overwrite.as_deref() == Some(name.as_str()) {
            ui.text_colored(theme::WARN, t("ranch.replace_q"));
            ui.same_line_with_spacing(0.0, GAP);
            if theme::gold_button_sized(ui, format!("{}##oy_{name}", t("btn.yes")), btn) {
                overwrite_name = Some(name.clone());
                state.main.confirm_overwrite = None;
            }
            ui.same_line_with_spacing(0.0, GAP);
            if ui.button_with_size(format!("{}##on_{name}", t("btn.no")), btn) {
                state.main.confirm_overwrite = None;
            }
        } else {
            if theme::gold_button_sized(ui, format!("{}##load_{name}", t("btn.load")), btn) {
                load_name = Some(name.clone());
            }
            ax += btn[0] + GAP;
            ui.set_cursor_screen_pos([ax, btn_y]);
            if has_opt {
                if theme::gold_button_sized(ui, format!("{}##save_{name}", t("btn.save")), btn) {
                    state.main.confirm_overwrite = Some(name.clone());
                    state.main.confirm_delete = None;
                }
            } else {
                muted_gold(
                    ui,
                    format!("{}##save_{name}", t("btn.save")),
                    btn,
                    &t("ranch.need_opt"),
                );
            }
            ax += btn[0] + GAP;
            ui.set_cursor_screen_pos([ax, btn_y]);
            if toss_button(ui, format!("{}##del_{name}", t("btn.delete")), btn) {
                persist_notes(state, name);
                state.main.confirm_delete = Some(name.clone());
                state.main.confirm_overwrite = None;
            }
        }

        ui.set_cursor_screen_pos([row[0], row[1] + ROW_H + 6.0]);
    }

    if let Some(name) = load_name {
        load_named(state, &name);
    }
    if let Some(name) = overwrite_name {
        overwrite_named(state, &name);
    }
    if let Some(name) = delete_name {
        delete_named(state, &name);
    }
}

pub(in crate::ui::main_view) fn render_saveload_tab(ui: &Ui, state: &mut AddonState) {
    if !state.main.saved_builds_loaded {
        let (builds, skipped) = load_saved_builds(&state.addon_dir);
        for b in &builds {
            state
                .main
                .note_drafts
                .entry(b.name.clone())
                .or_insert_with(|| b.notes.clone());
        }
        state.main.saved_builds = builds;
        state.main.saved_builds_skipped = skipped;
        state.main.saved_builds_loaded = true;
    }

    let char_name = selected_character_name(state);
    let rows = ranch_indices(&state.main.saved_builds, char_name.as_deref());
    let shown = rows.len();
    let total = state.main.saved_builds.len();

    render_ranch_hero(ui, char_name.as_deref(), shown, total);
    if !state.main.saved_builds_skipped.is_empty() {
        render_skipped_saves_warning(ui, &state.main.saved_builds_skipped);
    }
    ui.dummy([0.0, 10.0]);
    render_corral_bar(ui, state);
    ui.dummy([0.0, 14.0]);

    if rows.is_empty() {
        render_empty_paddock(ui, char_name.as_deref());
        return;
    }

    render_ranch_table(ui, state, &rows);
}

/// Load saved builds plus the basenames of any corrupt `.json` files skipped
/// alongside them, in the single directory scan `BuildStorage::list_with_skipped`
/// performs. Standalone (no `AddonState`) so the skip-surfacing behavior is
/// unit-testable without a live overlay — see
/// `tests::skipped_corrupt_saves_are_listed`.
fn load_saved_builds(
    addon_dir: &std::path::Path,
) -> (Vec<gw2_core::types::SavedBuild>, Vec<String>) {
    gw2_core::storage::BuildStorage::new(addon_dir).list_with_skipped()
}

/// Warn the player that one or more saves could not be read, instead of
/// letting them silently vanish behind the storage layer's `eprintln!` (C29).
fn render_skipped_saves_warning(ui: &Ui, skipped: &[String]) {
    ui.text_colored(
        theme::ERR,
        format!(
            "{} corrupt save file(s) skipped: {}",
            skipped.len(),
            skipped.join(", "),
        ),
    );
}

/// The per-slot prefix record to persist for a suggestion: its own slot map
/// when present, else the legacy expansion of the profile-level fields.
fn suggestion_slot_prefixes(
    suggestion: &crate::ui::comparison::BuildSuggestion,
) -> gw2_core::types::GearSlots {
    suggestion.slot_prefixes.clone().unwrap_or_else(|| {
        // `from_legacy` falls back to the profile-level name for every empty
        // group, so default groups expand to a uniform profile fill.
        gw2_core::types::GearSlots::from_legacy(
            &suggestion.stat_prefix,
            &gw2_core::types::GearPrefixGroups::default(),
        )
    })
}

fn suggestion_to_saved(
    name: &str,
    character_name: &str,
    profession: &str,
    game_mode: &gw2_core::types::GameMode,
    balance_manifest_version: Option<&str>,
    suggestion: &crate::ui::comparison::BuildSuggestion,
) -> gw2_core::types::SavedBuild {
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    gw2_core::types::SavedBuild {
        name: name.trim().to_string(),
        timestamp,
        character_name: character_name.to_string(),
        game_mode: game_mode.clone(),
        profession: profession.to_string(),
        engine_version: crate::VERSION.to_string(),
        balance_manifest_version: balance_manifest_version.map(|s| s.to_string()),
        label: suggestion.label.clone(),
        // Downgrade path keeps only the profile-level name; the slot map is
        // the authoritative per-slot record from here on.
        stat_prefix: suggestion.stat_prefix.clone(),
        gear_prefixes: gw2_core::types::GearPrefixGroups::default(),
        slot_prefixes: Some(suggestion_slot_prefixes(suggestion)),
        specializations: suggestion.specializations.clone(),
        weapons: suggestion.weapons.clone(),
        skills: suggestion.skills.clone(),
        rune: suggestion.rune.clone(),
        sigils: suggestion.sigils.clone(),
        relic: suggestion.relic.clone(),
        explanation: suggestion.explanation.clone(),
        synergy_explanation: suggestion.synergy_explanation.clone(),
        changes_made: suggestion.changes_made.clone(),
        estimated_stats: suggestion.estimated_stats.clone(),
        notes: String::new(),
    }
}

/// The save's profession — "Warrior" for pre-P3-16 saves that stored none.
fn saved_profession(saved: &gw2_core::types::SavedBuild) -> &str {
    if saved.profession.is_empty() {
        nexus::log::log(
            nexus::log::LogLevel::Warning,
            "GW2BuildOpt",
            "Loaded build with empty profession — falling back to Warrior",
        );
        "Warrior"
    } else {
        &saved.profession
    }
}

/// Convert a SavedBuild back to a BuildSuggestion for display.
/// Recomputes combat metrics from estimated stats if available, with no
/// damage modifiers; the load worker re-measures with the engine.
fn saved_to_suggestion(
    saved: &gw2_core::types::SavedBuild,
    game_db: Option<&gw2_optimizer::gamedb::GameDb>,
) -> crate::ui::comparison::BuildSuggestion {
    let profession = saved_profession(saved);

    // No modifiers here: with game data the worker re-measures the save with
    // the engine's stat sheet (`optimization::simulate_suggestion_rotation`).
    let ctx = gw2_optimizer::balance::BalanceContext::new(saved.game_mode.clone());
    let mods = gw2_optimizer::combat::DamageModifiers::default();

    // Recompute combat metrics from saved stats (lossy i32→f64 but good enough for display)
    let (combat_solo, combat_party, combat_squad) = saved
        .estimated_stats
        .as_ref()
        .map(|est| {
            let stats = gw2_optimizer::stats::StatBlock {
                power: est.power as f64,
                precision: est.precision as f64,
                toughness: est.toughness as f64,
                vitality: est.vitality as f64,
                condition_damage: est.condition_damage as f64,
                expertise: est.expertise as f64,
                concentration: est.concentration as f64,
                ferocity: est.ferocity as f64,
                healing_power: est.healing_power as f64,
            };
            let derived = gw2_optimizer::stats::compute_derived(&stats, profession);
            stats::compute_3tier_combat(&stats, &derived, &mods, profession, &ctx)
        })
        .unwrap_or((None, None, None));

    let mut suggestion = crate::ui::comparison::BuildSuggestion {
        // Ours, not published anywhere.
        source_url: String::new(),
        label: if saved.label.is_empty() {
            saved.name.clone()
        } else {
            saved.label.clone()
        },
        build_summary: String::new(),
        stat_prefix: saved.stat_prefix.clone(),
        slot_prefixes: Some(saved.slot_prefixes.clone().unwrap_or_else(|| {
            gw2_core::types::GearSlots::from_legacy(&saved.stat_prefix, &saved.gear_prefixes)
        })),
        specializations: saved.specializations.clone(),
        weapons: saved.weapons.clone(),
        skills: saved.skills.clone(),
        rune: saved.rune.clone(),
        sigils: saved.sigils.clone(),
        relic: saved.relic.clone(),
        chat_code: None,
        explanation: saved.explanation.clone(),
        synergy_explanation: saved.synergy_explanation.clone(),
        changes_made: saved.changes_made.clone(),
        estimated_stats: saved.estimated_stats.clone(),
        combat_solo,
        combat_party,
        combat_squad,
        rotation: None,
        viability: None,
        benchmark_delta: None,
        // Not measured against the community here, so nothing to say about
        // whether the corpus is on disk.
        benchmarks_synced: false,
        data_quality: gw2_optimizer::data::DataQuality::Verified,
        quality_reasons: vec![],
        coverage_note: None,
    };
    if let Some(db) = game_db {
        suggestion.chat_code = optimization::suggestion_to_chat_code(&suggestion, db);
    }
    suggestion
}

/// Format a Unix timestamp as a readable date+time string (YYYY-MM-DD HH:MM).
fn format_timestamp(timestamp: u64) -> String {
    let secs_per_day: u64 = 86400;
    let day_secs = timestamp % secs_per_day;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let days = timestamp / secs_per_day;
    // Days since epoch to Y/M/D
    let mut y = 1970u64;
    let mut remaining_days = days;
    loop {
        let days_in_year =
            if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let days_in_months: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 11;
    for (i, &dim) in days_in_months.iter().enumerate() {
        if remaining_days < dim {
            m = i;
            break;
        }
        remaining_days -= dim;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m + 1,
        remaining_days + 1,
        hours,
        minutes
    )
}

#[cfg(test)]
mod tests {
    fn build_saved_for_modifier_reconstruction() -> gw2_core::types::SavedBuild {
        gw2_core::types::SavedBuild {
            name: "test-save".into(),
            timestamp: 0,
            character_name: "Test Character".into(),
            game_mode: gw2_core::types::GameMode::PvE,
            profession: "Warrior".into(),
            engine_version: "test".into(),
            balance_manifest_version: None,
            label: "Test Build".into(),
            stat_prefix: "Viper's".into(),
            gear_prefixes: gw2_core::types::GearPrefixGroups::default(),
            slot_prefixes: None,
            specializations: vec![("Test Spec".into(), vec!["Test Condition Trait".into()])],
            weapons: vec![],
            skills: vec![],
            rune: "Superior Rune of Test".into(),
            sigils: vec!["Superior Sigil of Bursting".into()],
            relic: "Relic of the Nightmare".into(),
            explanation: String::new(),
            synergy_explanation: String::new(),
            changes_made: vec![],
            notes: String::new(),
            estimated_stats: Some(gw2_core::types::StatBlock {
                power: 1800,
                precision: 1800,
                toughness: 1200,
                vitality: 1200,
                condition_damage: 1800,
                expertise: 0,
                concentration: 0,
                ferocity: 0,
                healing_power: 0,
                crit_chance: 0.0,
                crit_damage: 0.0,
                health: 0,
                armor: 0,
            }),
        }
    }

    #[test]
    fn empty_gear_groups_inherit_stat_prefix_on_load() {
        let saved = build_saved_for_modifier_reconstruction();
        assert!(saved.gear_prefixes.armor.is_empty());
        let suggestion = super::saved_to_suggestion(&saved, None);
        let slots = suggestion
            .slot_prefixes
            .expect("loaded builds always carry a slot map");
        let prefix = slots
            .get(gw2_core::types::GearSlot::Helm)
            .expect("legacy expansion fills armor");
        assert_eq!(prefix.name, "Viper's");
        assert_eq!(
            slots
                .get(gw2_core::types::GearSlot::WeaponSet1Main)
                .unwrap()
                .name,
            "Viper's"
        );
    }

    #[test]
    fn slot_prefixes_round_trip_through_saved_build() {
        use gw2_core::types::{GearSlot, PrefixRef};

        // A suggestion carrying a mixed per-slot map (as the optimizer emits).
        let mut slots = gw2_core::types::GearSlots::default();
        slots.set(
            GearSlot::Helm,
            PrefixRef {
                itemstat_id: 1,
                name: "Berserker's".into(),
            },
        );
        slots.set(
            GearSlot::Coat,
            PrefixRef {
                itemstat_id: 2,
                name: "Cavalier's".into(),
            },
        );
        slots.set(
            GearSlot::WeaponSet1Main,
            PrefixRef {
                itemstat_id: 3,
                name: "Sinister".into(),
            },
        );
        let suggestion = crate::ui::comparison::BuildSuggestion {
            label: "Round Trip".into(),
            stat_prefix: "Berserker's".into(),
            slot_prefixes: Some(slots.clone()),
            ..Default::default()
        };

        let saved = super::suggestion_to_saved(
            "rt",
            "char",
            "Warrior",
            &Default::default(),
            None,
            &suggestion,
        );
        // The save carries the authoritative map and stops duplicating groups.
        assert_eq!(saved.slot_prefixes.as_ref(), Some(&slots));
        assert!(saved.gear_prefixes.armor.is_empty());

        // JSON round-trip keeps the sparse map byte-identical.
        let json = serde_json::to_string(&saved).unwrap();
        let reloaded: gw2_core::types::SavedBuild = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded.slot_prefixes.as_ref(), Some(&slots));

        let back = super::saved_to_suggestion(&reloaded, None);
        assert_eq!(back.slot_prefixes.as_ref(), Some(&slots));
        assert_eq!(
            back.slot_prefixes
                .unwrap()
                .get(GearSlot::Coat)
                .unwrap()
                .name,
            "Cavalier's"
        );
    }

    #[test]
    fn ranch_indices_filters_to_selected_character() {
        let mut a = build_saved_for_modifier_reconstruction();
        a.name = "a".into();
        a.character_name = "Darth".into();
        let mut b = a.clone();
        b.name = "b".into();
        b.character_name = "Other".into();
        let builds = vec![a, b];
        assert_eq!(super::ranch_indices(&builds, Some("Darth")), vec![0]);
        assert_eq!(super::ranch_indices(&builds, None), vec![0, 1]);
    }

    #[test]
    fn skipped_corrupt_saves_are_listed() {
        // C29: a corrupt save file must not just vanish behind
        // `BuildStorage`'s `eprintln!` — its basename must come back from the
        // exact load path `render_saveload_tab` uses, so the Save/Load tab can
        // warn the player instead of the save silently disappearing.
        let dir = std::env::temp_dir().join(format!(
            "gw2_saveload_skipped_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let saves_dir = dir.join("saves");
        std::fs::create_dir_all(&saves_dir).unwrap();

        let good = build_saved_for_modifier_reconstruction();
        let storage = gw2_core::storage::BuildStorage::new(&dir);
        storage.save_new(&good).unwrap();
        std::fs::write(saves_dir.join("corrupt.json"), "{ not json }").unwrap();

        let (builds, skipped) = super::load_saved_builds(&dir);
        assert_eq!(builds.len(), 1, "the one good build should survive");
        assert_eq!(
            skipped,
            vec!["corrupt.json".to_string()],
            "the corrupt save's basename must be surfaced, not just eprintln!-ed",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ranch_load_worker_body_matches_saved_to_suggestion_without_db() {
        let saved = build_saved_for_modifier_reconstruction();
        let weights = gw2_optimizer::scoring::OptimizationWeights::default();
        let scenario = gw2_optimizer::scenario::ScenarioSpec::from_balance_context(
            &gw2_optimizer::balance::BalanceContext::new(saved.game_mode.clone()),
        );
        let via_worker =
            super::suggestion_from_saved_build(&saved, None, &saved.game_mode, &weights, &scenario);
        let via_convert = super::saved_to_suggestion(&saved, None);
        assert_eq!(via_worker.label, via_convert.label);
        assert_eq!(via_worker.combat_solo, via_convert.combat_solo);
        assert_eq!(via_worker.combat_party, via_convert.combat_party);
        assert_eq!(via_worker.combat_squad, via_convert.combat_squad);
        assert_eq!(via_worker.chat_code, via_convert.chat_code);
        assert!(
            via_worker.rotation.is_none(),
            "without GameDb the worker must not invent a rotation"
        );
    }

    #[test]
    fn ranch_load_click_handler_spawns_worker_instead_of_inline_cpu() {
        let src = include_str!("saveload.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("production source");

        fn fn_body<'a>(production: &'a str, name: &'a str) -> &'a str {
            let needle = format!("fn {name}(");
            let start = production
                .find(&needle)
                .unwrap_or_else(|| panic!("{name} missing from production source"));
            let after = &production[start..];
            let nxt = after[1..].find("\nfn ").unwrap_or(after.len() - 1);
            &after[..nxt + 1]
        }

        let click = fn_body(production, "load_named");
        assert!(
            click.contains("spawn_worker"),
            "load_named must call spawn_worker on the Load click/draw pass"
        );
        for forbidden in [
            "simulate_suggestion_rotation",
            "suggestion_to_chat_code",
            "compute_3tier_combat",
            "saved_to_suggestion",
        ] {
            assert!(
                !click.contains(forbidden),
                "load_named must not run {forbidden} synchronously"
            );
        }

        let worker = fn_body(production, "suggestion_from_saved_build");
        assert!(
            worker.contains("saved_to_suggestion"),
            "worker body must call saved_to_suggestion"
        );
        assert!(
            worker.contains("simulate_suggestion_rotation"),
            "worker body must call simulate_suggestion_rotation"
        );

        let convert = fn_body(production, "saved_to_suggestion");
        assert!(
            convert.contains("compute_3tier_combat"),
            "3-tier combat stays inside saved_to_suggestion (worker-only path)"
        );
        assert!(
            convert.contains("suggestion_to_chat_code"),
            "chat-code stays inside saved_to_suggestion (worker-only path)"
        );

        let apply = fn_body(production, "apply_loaded_suggestion");
        assert!(apply.contains("suggestions"));
        assert!(
            !apply.contains("simulate_suggestion_rotation"),
            "completion must not run rotation sim"
        );
        assert!(
            !apply.contains("suggestion_to_chat_code"),
            "completion must not run chat-code"
        );
        assert!(
            !apply.contains("compute_3tier_combat"),
            "completion must not run 3-tier combat"
        );
    }

    #[test]
    fn ranch_load_click_handler_does_not_persist_notes_on_click_frame() {
        let src = include_str!("saveload.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("production source");

        fn fn_body<'a>(production: &'a str, name: &'a str) -> &'a str {
            let needle = format!("fn {name}(");
            let start = production
                .find(&needle)
                .unwrap_or_else(|| panic!("{name} missing from production source"));
            let after = &production[start..];
            let nxt = after[1..].find("\nfn ").unwrap_or(after.len() - 1);
            &after[..nxt + 1]
        }

        let click = fn_body(production, "load_named");
        assert!(
            click.contains("spawn_worker"),
            "A23-2: load_named must still spawn ranch-load"
        );
        assert!(
            click.contains("ranch-load"),
            "A23-2: spawn_worker(\"ranch-load\") must remain"
        );
        assert!(
            !click.contains("persist_notes"),
            "load_named must not call persist_notes on the Load click/draw frame"
        );
        assert!(
            !click.contains("save_overwrite"),
            "load_named must not save_overwrite on the Load click/draw frame"
        );
        assert!(
            click.contains("save_note_now"),
            "if ranch-notes also fails to spawn, notes write on this frame"
        );
        assert!(
            click.contains("notes_ok"),
            "the ranch-notes spawn result must be checked"
        );
        for forbidden in [
            "simulate_suggestion_rotation",
            "suggestion_to_chat_code",
            "compute_3tier_combat",
            "saved_to_suggestion",
        ] {
            assert!(
                !click.contains(forbidden),
                "A23-2 preserved: load_named must not run {forbidden} synchronously"
            );
        }
        assert!(
            click.contains("write_note_snapshot"),
            "dirty notes must still be handed to the ranch-load worker"
        );
        let write_at = click
            .find("write_note_snapshot")
            .expect("write_note_snapshot in load_named");
        let cpu_at = click
            .find("suggestion_from_saved_build")
            .expect("suggestion_from_saved_build in load_named");
        assert!(
            write_at < cpu_at,
            "notes overwrite must run in the worker before rotation/combat CPU"
        );

        let writer = fn_body(production, "write_note_snapshot");
        assert!(
            writer.contains("save_overwrite"),
            "notes disk write lives in write_note_snapshot, off the click frame"
        );
        assert!(
            !writer.contains("simulate_suggestion_rotation"),
            "notes writer must not run rotation sim"
        );

        let persist = fn_body(production, "persist_notes");
        assert!(
            persist.contains("save_overwrite"),
            "persist_notes stays for non-Load paths (notes field / delete confirm)"
        );
    }
}
