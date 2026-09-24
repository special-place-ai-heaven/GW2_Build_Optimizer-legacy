use nexus::imgui::{ChildWindow, ComboBox, Selectable, TreeNodeFlags, Ui};

use crate::state::{AddonState, MainTab};
use crate::ui::theme;
use gw2_core::i18n::{t, tf};
use gw2_core::types::GameMode;
use gw2_optimizer::scenario::{CombatTier, RoleObjective};
use gw2_optimizer::scoring::OptimizationWeights;

pub(crate) mod build_display;
mod character;
mod chat_flow;
mod generation;
pub mod lock_panel;
mod optimization;
mod optimize_flow;
pub(crate) mod provider_picks;
mod resolution;
mod stats;
mod tabs;

pub(crate) use tabs::about::generations::GenerationsTab;

/// `kitchen.json` (chat history). Saved from the frame loop whenever the chat
/// is dirty, which can be many frames in a row while a reply streams in.
static KITCHEN_WRITES: crate::ui::SerialWriter = crate::ui::SerialWriter::new("kitchen-save");

pub fn render_main(ui: &Ui, state: &mut AddonState) {
    // Apply global UI scale (text + element sizing)
    let scale = state.config.font_scale;
    ui.set_window_font_scale(scale);
    // Publish it so scoped changes can restore to it — see theme::font_scale.
    theme::set_ui_scale(scale);
    // Scale element sizes proportionally
    let pad = theme::control_pad(ui);
    let _s1 = ui.push_style_var(nexus::imgui::StyleVar::FramePadding(pad));
    let _s2 = ui.push_style_var(nexus::imgui::StyleVar::ItemSpacing([
        8.0 * scale,
        4.0 * scale,
    ]));
    let _s3 = ui.push_style_var(nexus::imgui::StyleVar::ItemInnerSpacing([
        4.0 * scale,
        4.0 * scale,
    ]));

    // Trigger character load on first render
    if state.needs_character_reload {
        state.needs_character_reload = false;
        character::reload_from_api(state);
    } else if state.main.characters.is_empty() && !state.main.characters_loading {
        // Retry at most every 30s: a persistent failure (API outage, bad
        // key) must not spawn a loader thread every frame.
        let cooled = state
            .main
            .characters_retry_at
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(30));
        if cooled {
            state.main.characters_retry_at = Some(std::time::Instant::now());
            character::load_characters(state);
        }
    }

    // Load GameDb once on first entry (S11-T06)
    if state.main.game_db.is_none() && !state.main.game_db_loading {
        let cooled = state
            .main
            .game_db_retry_at
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60));
        if cooled {
            state.main.game_db_retry_at = Some(std::time::Instant::now());
            stats::load_game_db(state);
        }
    }
    if state.main.game_db.is_some() {
        stats::ensure_localized_names(state);
    }

    // Periodic API health check (~every 60s at 60fps)
    state.main.api_status_frames += 1;
    if (state.main.api_status_frames >= 3600
        || state.main.api_status == crate::state::ApiStatus::Unknown)
        && !state.main.api_health_checking
    {
        stats::check_api_health(state);
        state.main.api_status_frames = 0;
    }
    crate::feedback::tasks::maybe_poll(state);

    // Auto-dismiss save status after ~180 frames (~3s at 60fps)
    if state.main.save_status.is_some() {
        state.main.save_status_frames += 1;
        if state.main.save_status_frames > 180 {
            state.main.save_status = None;
            state.main.save_status_frames = 0;
        }
    }

    // Chat timeout. Wall clock, not FPS. Include the live model so a stall isn't a mystery.
    //
    // A backstop for a worker that died, not a budget: the worker owns the
    // deadlines (150 s of tool rounds, a 150 s closing request, two short
    // repairs) and ends every run in a build, the optimizer's own if need be.
    // At 120 s this fired while the closing request was still legitimately
    // running (openrouter/free, 2026-09-07), discarded the worker's result
    // and showed "timed out" with no build at all.
    const KITCHEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(400);
    if state.main.chat.waiting {
        let started = state
            .main
            .chat_wait_started
            .get_or_insert_with(std::time::Instant::now);
        if started.elapsed() >= KITCHEN_TIMEOUT {
            state.main.chat_epoch = state.main.chat_epoch.wrapping_add(1);
            state.main.chat.waiting = false;
            state.main.chat_wait_started = None;
            state.main.optimize_stage.clear();
            let msg = optimization::format_provider_issue(
                "timeout",
                state.config.active_provider.short_label(),
                state.config.active_model_id(),
            );
            state.main.provider_issue = Some(msg.clone());
            crate::ui::chat_bar::add_ai_response(&mut state.main.chat, msg);
        }
    } else {
        state.main.chat_wait_started = None;
    }

    // Top status bar: API health + loading + errors
    render_top_status_bar(ui, state);

    // Horizontal tab bar (main navigation)
    render_top_tabs(ui, state);

    // Two-column layout: left dynamic panel + center content
    let pad = state.config.panel_padding;
    let content_indent = state.config.content_indent;
    let avail = ui.content_region_avail();
    let left_panel_width = {
        // The scale row is the widest thing in the rail, and its words
        // change with the mode — "Open World" is longer than "Roam" — so the
        // rail is sized for whichever mode is showing, not for WvW always.
        let labels: Vec<String> = [CombatTier::Solo, CombatTier::Party, CombatTier::Squad]
            .iter()
            .map(|tier| t(scale_i18n_key(&state.main.game_mode, *tier)))
            .collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let scale_row = theme::segment_row_min_width(ui, &labels);
        let min_left = (scale_row + pad * 2.0 + 18.0).max(360.0);
        let want = (state.config.left_panel_width * scale).max(min_left);
        let cap = (avail[0] * 0.58).max(min_left);
        want.min(cap)
    };
    // Leave a small footer margin so the bottom row (Save/Clear buttons, etc.)
    // is not tight against the game window edge or clipped on some resolutions.
    let child_height = (avail[1] - 6.0).max(0.0);

    ChildWindow::new("##left_panel_global")
        .size([left_panel_width, child_height])
        .build(ui, || {
            ui.dummy([0.0, 2.0]);
            ui.indent_by(pad);
            render_left_panel(ui, state);
            ui.unindent_by(pad);
        });

    ui.same_line();

    ChildWindow::new("##center_content")
        .size([0.0, child_height])
        .build(ui, || {
            // Match left rail: dummy(2) + section header dummy(section_spacing).
            ui.dummy([0.0, 2.0 + state.config.section_spacing]);
            ui.indent_by(content_indent);
            render_main_content(ui, state);
            ui.unindent_by(content_indent);
        });

    if state.main.chat.dirty {
        // Snapshot under the lock, serialize + write on a worker: must never
        // stall the frame (the render callback is the only ImGui draw pass, so
        // a slow frame reads as the game hanging). One saver at a time —
        // `save_history` stages through a single `kitchen.json.tmp`, and two
        // overlapping savers would race over that one path; this also stops a
        // chat that stays dirty from spawning a thread on every frame.
        let snapshot = state.main.chat.history.clone();
        let addon_dir = state.addon_dir.clone();
        state.main.chat.dirty = false;
        KITCHEN_WRITES.submit(state, move || {
            crate::ui::chat_bar::save_history(&addon_dir, &snapshot);
        });
    }
}

/// One always-on chat strip. Follows Current vs Optimized focus.
fn render_focus_chat(ui: &Ui, state: &mut AddonState) {
    let current = state.main.build_chat_code.clone();
    let (source, code) = state.main.comparison.chat_focus(current.as_deref());
    crate::ui::comparison::render_chat_code_copy(
        ui,
        source,
        code.as_deref(),
        "top",
        &mut state.main.copy_feedback_frames,
    );
}

/// Top status bar: API health indicator + loading banner + error bar.
fn render_top_status_bar(ui: &Ui, state: &mut AddonState) {
    // Draw subtle background for the status row, sized to the text it
    // holds so the label sits centred in it at every font and UI scale
    // (a fixed 18 px strip left the text hanging below it, 2026-09-08).
    let line_h = ui.text_line_height();
    let row_h = line_h + 4.0;
    {
        let start = ui.cursor_screen_pos();
        let width = ui.content_region_avail()[0];
        let draw_list = ui.get_window_draw_list();
        draw_list
            .add_rect(
                [start[0] - 1.0, start[1]],
                [start[0] + width + 1.0, start[1] + row_h],
                theme::with_alpha(theme::pal().frame_bg_hovered, 0.95),
            )
            .filled(true)
            .build();
    }

    // API health indicator — live `/v2/build` sits on the chip so a refresh
    // that caught up is not mistaken for a failed update.
    let label = stats::api_status_label(&state.main.api_status, state.main.live_build_number);
    let color = match state.main.api_status {
        crate::state::ApiStatus::Unknown => crate::ui::theme::pal().muted,
        crate::state::ApiStatus::Online => crate::ui::theme::OPTIMIZED,
        crate::state::ApiStatus::Degraded => crate::ui::theme::pal().gold,
        crate::state::ApiStatus::Offline => crate::ui::theme::ERR,
    };
    {
        let p = ui.cursor_screen_pos();
        ui.get_window_draw_list()
            .add_circle([p[0] + 8.0, p[1] + row_h * 0.5], 4.0, color)
            .filled(true)
            .build();
        ui.set_cursor_screen_pos([p[0], p[1] + 2.0]);
    }
    ui.dummy([16.0, line_h]);
    ui.same_line();
    ui.text_colored(color, label);
    if ui.is_item_hovered() {
        ui.tooltip_text(match state.main.api_status {
            crate::state::ApiStatus::Unknown => t("tip.api_checking"),
            crate::state::ApiStatus::Online => t("tip.api_ready"),
            crate::state::ApiStatus::Degraded => t("tip.api_slow"),
            crate::state::ApiStatus::Offline => t("tip.api_offline"),
        });
    }

    if let (Some(cached), Some(live)) = (
        state.config.cache_build_number,
        state.main.live_build_number,
    ) {
        if cached != live && !state.main.game_db_loading {
            ui.same_line();
            ui.text_colored(
                theme::WARN,
                tf(
                    "fmt.stale_cache",
                    &[("cached", &cached.to_string()), ("live", &live.to_string())],
                ),
            );
            if ui.is_item_hovered() {
                ui.tooltip_text(t("tip.stale_cache"));
            }
            ui.same_line();
            if theme::gold_button(ui, format!("{}##stale_data", t("btn.refresh"))) {
                stats::start_game_data_refresh(state);
            }
        }
    }
    stats::render_manifest_staleness(ui, state);

    if !state.main.game_db_loading {
        if let Some(lang) = gw2_core::i18n::api_lang(&state.config.ui_language) {
            let build = state
                .main
                .live_build_number
                .or(state.config.cache_build_number);
            match stats::cached_pack_status(&state.addon_dir, lang, build) {
                gw2_api::localize::PackStatus::Missing | gw2_api::localize::PackStatus::Stale => {
                    ui.same_line();
                    ui.text_colored(theme::WARN, t("status.names_english"));
                    if ui.is_item_hovered() {
                        ui.tooltip_text(t("tip.names_english"));
                    }
                }
                _ => {}
            }
        }
    }

    // Loading banner (GameDb)
    if state.main.game_db_loading {
        ui.same_line();
        let stage = &state.main.game_refresh_stage;
        if !stage.is_empty() {
            ui.text_colored(theme::WARN, format!("| {}", stage));
        } else {
            ui.text_colored(theme::WARN, format!("| {}", t("status.loading_data")));
        }
    }
    if state.main.names_loading {
        ui.same_line();
        ui.text_colored(theme::WARN, format!("| {}", state.main.names_stage));
    }

    // Optimization progress
    if state.main.optimizing {
        ui.same_line();
        ui.text_colored(theme::WARN, format!("| {}", state.main.optimize_stage));
    }

    // Error bar (dismissible)
    if let Some(ref err) = state.main.error {
        ui.text_colored(theme::ERR, format!("  [!] {}", err));
        ui.same_line();
        if ui.small_button(format!("{}##err", t("btn.dismiss"))) {
            state.main.error = None;
        }
    }

    // Accent line below status bar
    {
        let pos = ui.cursor_screen_pos();
        let width = ui.content_region_avail()[0];
        let draw_list = ui.get_window_draw_list();
        draw_list
            .add_line(
                [pos[0] - 1.0, pos[1]],
                [pos[0] + width + 1.0, pos[1]],
                theme::with_alpha(theme::pal().gold_button_active, 0.6),
            )
            .thickness(1.0)
            .build();
    }
    ui.dummy([0.0, 2.0]);
}

/// The banner card's drawn height. The background, the border and the
/// trailing `dummy` all measure from this.
const CARD_HEIGHT: f32 = 62.0;

/// `theme::pill`'s own padding, so a pill can be placed by hand at the same
/// size the widget will draw itself.
const PILL_PAD_X: f32 = 10.0;
const PILL_PAD_Y: f32 = 3.0;

/// Prominent animated progress banner for optimization.
///
/// Returns true when the player clicked Stop on this frame. `can_stop` is
/// false once a stop is already under way, because a second click cancels a
/// token nothing is holding.
pub(super) fn render_optimization_progress(
    ui: &Ui,
    stage: &str,
    frame_count: i32,
    can_stop: bool,
) -> bool {
    let frame_count = frame_count as u32;
    ui.spacing();
    let start = ui.cursor_screen_pos();
    let width = ui.content_region_avail()[0];

    {
        let draw_list = ui.get_window_draw_list();

        // Dark card background
        draw_list
            .add_rect(
                [start[0], start[1]],
                [start[0] + width, start[1] + CARD_HEIGHT],
                theme::with_alpha(theme::pal().title_bg, 0.95),
            )
            .filled(true)
            .rounding(6.0)
            .build();

        // Animated accent stripe at top (sweeping cyan glow)
        let cycle = (frame_count % 180) as f32 / 180.0;
        let glow_x = start[0] + cycle * width;
        let glow_half = width * 0.15;
        draw_list
            .add_rect(
                [(glow_x - glow_half).max(start[0]), start[1]],
                [(glow_x + glow_half).min(start[0] + width), start[1] + 3.0],
                crate::ui::theme::pal().gold,
            )
            .filled(true)
            .build();
        // Dimmer full-width stripe underneath
        draw_list
            .add_rect(
                [start[0], start[1]],
                [start[0] + width, start[1] + 3.0],
                theme::with_alpha(theme::pal().gold_dim, 0.35),
            )
            .filled(true)
            .build();

        // Spinner dots (3 pulsing dots)
        let dot_y = start[1] + 18.0;
        for i in 0..3u32 {
            let phase = ((frame_count + i * 20) % 60) as f32 / 60.0;
            let alpha = 0.3 + 0.7 * (phase * std::f32::consts::PI * 2.0).sin().abs();
            let dot_x = start[0] + 16.0 + i as f32 * 12.0;
            draw_list
                .add_circle(
                    [dot_x, dot_y],
                    3.5,
                    theme::with_alpha(theme::pal().gold, alpha),
                )
                .filled(true)
                .build();
        }

        // "OPTIMIZING..." title
        let finding = t("status.finding_build");
        draw_list.add_text(
            [start[0] + 56.0, start[1] + 10.0],
            crate::ui::theme::pal().gold,
            &finding,
        );

        // Stage detail text
        let starting = t("status.starting_opt");
        let detail = if stage.is_empty() {
            starting.as_str()
        } else {
            stage
        };
        draw_list.add_text(
            [start[0] + 16.0, start[1] + 34.0],
            theme::pal().muted,
            detail,
        );

        // Animated progress bar at bottom
        let bar_y = start[1] + 56.0;
        // Background
        draw_list
            .add_rect(
                [start[0] + 8.0, bar_y],
                [start[0] + width - 8.0, bar_y + 4.0],
                theme::with_alpha(theme::pal().frame_bg, 0.6),
            )
            .filled(true)
            .rounding(2.0)
            .build();
        // Sweeping fill
        let bar_inner = width - 16.0;
        let sweep_width = bar_inner * 0.35;
        let sweep_cycle = ((frame_count % 150) as f32) / 150.0;
        let sweep_x = start[0] + 8.0 + sweep_cycle * (bar_inner + sweep_width) - sweep_width;
        draw_list
            .add_rect(
                [sweep_x.max(start[0] + 8.0), bar_y],
                [
                    (sweep_x + sweep_width).min(start[0] + width - 8.0),
                    bar_y + 4.0,
                ],
                crate::ui::theme::pal().gold_fill,
            )
            .filled(true)
            .rounding(2.0)
            .build();

        // Border
        draw_list
            .add_rect(
                [start[0], start[1]],
                [start[0] + width, start[1] + CARD_HEIGHT],
                crate::ui::theme::pal().gold_dim,
            )
            .rounding(6.0)
            .build();
    }

    // Choya's Stop, not a second one: same pill, same label, so stopping a run
    // looks and reads the same wherever it was started. The card is drawn
    // straight onto the draw list and moves no cursor, so the pill is placed
    // by hand and the cursor is put back before the layout below is measured.
    let mut stopped = false;
    if can_stop {
        let stop = t("chat.stop");
        let sz = ui.calc_text_size(&stop);
        // `theme::pill` pads 10px either side and 3px top and bottom, and
        // floors the width at 36. Its real height follows the font, so it
        // is measured rather than assumed.
        let w = (sz[0] + PILL_PAD_X * 2.0).max(36.0);
        let h = sz[1] + PILL_PAD_Y * 2.0;
        // Centred in the whole card, not on the title row: between the title
        // at +10 and the progress bar at +56 the pill sat visibly high.
        ui.set_cursor_screen_pos([
            start[0] + width - 12.0 - w,
            start[1] + (CARD_HEIGHT - h) * 0.5,
        ]);
        stopped = theme::pill(ui, &stop, false, "##optimize_stop");
        ui.set_cursor_screen_pos(start);
    }

    ui.dummy([0.0, 66.0]);
    ui.spacing();
    stopped
}

/// Horizontal tab bar for main navigation (styled buttons with active indicator).
fn render_top_tabs(ui: &Ui, state: &mut AddonState) {
    let new_build = t("tab.new_build");
    let improve = t("tab.improve");
    let choya = t("tab.choya");
    let modes = [
        (
            MainTab::NewBuild,
            new_build.as_str(),
            "##main_tab_new_build",
        ),
        (MainTab::Improve, improve.as_str(), "##main_tab_improve"),
        (MainTab::Talk, choya.as_str(), "##main_tab_choya"),
    ];
    for (i, (tab, label, id)) in modes.iter().enumerate() {
        if i > 0 {
            ui.same_line_with_spacing(0.0, 8.0);
        }
        let is_active = state.main.active_tab == *tab;
        let pulse = if state.main.tab_alert.as_ref() == Some(tab) && !is_active {
            // ~3s breathe at 60fps (abs(sin) period π / 0.0175).
            0.18 + 0.55 * (ui.frame_count() as f32 * 0.0175).sin().abs()
        } else {
            0.0
        };
        if crate::ui::theme::pill_pulse(ui, label, is_active, id, pulse) {
            state.main.active_tab = tab.clone();
            if state.main.tab_alert.as_ref() == Some(tab) {
                state.main.tab_alert = None;
            }
            match tab {
                MainTab::NewBuild => {
                    state.main.build_locks = gw2_core::types::BuildLocks::default();
                }
                MainTab::Improve => {
                    if let Some(ref build) = state.main.current_build {
                        let build_clone = build.clone();
                        resolution::auto_populate_locks(&build_clone, &mut state.main.build_locks);
                    }
                }
                _ => {}
            }
        }
    }

    ui.same_line_with_spacing(0.0, 28.0);
    let saves = t("tab.saves");
    let news = t("tab.news");
    let radio = t("tab.radio");
    let settings = t("tab.settings");
    let about = t("tab.about");
    let show_news = state.config.news.any_enabled();
    if !show_news && state.main.active_tab == MainTab::News {
        state.main.active_tab = MainTab::Settings;
    }
    let mut utility: Vec<(MainTab, &str, &str)> =
        vec![(MainTab::SaveLoad, saves.as_str(), "##main_tab_saves")];
    if show_news {
        utility.push((MainTab::News, news.as_str(), "##main_tab_news"));
    }
    utility.push((MainTab::Radio, radio.as_str(), "##main_tab_radio"));
    utility.push((MainTab::Settings, settings.as_str(), "##main_tab_settings"));
    utility.push((MainTab::About, about.as_str(), "##main_tab_about"));
    for (tab, label, id) in utility {
        let is_active = state.main.active_tab == tab;
        let pulse = if state.main.tab_alert.as_ref() == Some(&tab) && !is_active {
            0.18 + 0.55 * (ui.frame_count() as f32 * 0.0175).sin().abs()
        } else {
            0.0
        };
        if crate::ui::theme::pill_pulse(ui, label, is_active, id, pulse) {
            if state.main.tab_alert.as_ref() == Some(&tab) {
                state.main.tab_alert = None;
            }
            state.main.active_tab = tab;
        }
        ui.same_line_with_spacing(0.0, 8.0);
    }

    render_focus_chat(ui, state);
}

/// Dynamic left panel: content varies by active tab.
fn render_left_panel(ui: &Ui, state: &mut AddonState) {
    // Character section (always visible except Settings)
    if !matches!(
        state.main.active_tab,
        MainTab::Settings | MainTab::About | MainTab::News | MainTab::Radio
    ) {
        render_left_character_section(ui, state);
    }

    match state.main.active_tab {
        MainTab::NewBuild | MainTab::Improve | MainTab::Talk => {
            render_left_build_controls(ui, state);
        }
        MainTab::SaveLoad => {}
        MainTab::Settings | MainTab::About | MainTab::News | MainTab::Radio => {
            // Settings info
            render_left_section_header(ui, &t("section.info"), state.config.section_spacing);
            ui.text_colored(theme::pal().muted, format!("  {}", t("info.product")));
            ui.text_colored(
                theme::pal().muted,
                format!("  {}", tf("fmt.version", &[("ver", crate::VERSION)])),
            );
            ui.spacing();
            let provider_label = state.config.active_provider.label();
            ui.text_colored(
                theme::pal().muted,
                format!("  {}", tf("fmt.ai", &[("provider", provider_label)])),
            );
        }
    }
}

/// Render a compact section header with accent line in the left panel.
pub(super) fn render_left_section_header(ui: &Ui, title: &str, spacing: f32) {
    ui.dummy([0.0, spacing]);
    let pos = ui.cursor_screen_pos();
    let width = ui.content_region_avail()[0];
    let bar_h = theme::control_height(ui).max(ui.frame_height());
    {
        let draw_list = ui.get_window_draw_list();
        draw_list
            .add_rect(
                [pos[0], pos[1]],
                [pos[0] + width, pos[1] + bar_h],
                theme::with_alpha(theme::pal().header_plate, 0.95),
            )
            .filled(true)
            .rounding(4.0)
            .build();
        crate::ui::theme::paint_header_accent(&draw_list, pos[0], pos[1], bar_h);
    }
    // `Text` follows WindowFontScale + FramePadding; DrawList add_text does not.
    ui.set_cursor_screen_pos([crate::ui::theme::header_title_x(pos[0]), pos[1]]);
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().gold, title);
    ui.set_cursor_screen_pos([pos[0], pos[1] + bar_h]);
    ui.dummy([0.0, spacing * 0.5]);
}

/// How many people are in the fight.
///
/// The same three tiers mean different things per mode, and the words are
/// not interchangeable: WvW fights at Roam / Havoc / Cloud, PvE at Open
/// World / Group / Squad. Showing WvW's words in PvE — which is what every
/// mode used to get — offered a solo open-world player a choice between
/// "Havoc" and "Cloud/Zerg".
///
/// PvP has no scale to pick: conquest is always five a side.
fn render_scale_row(ui: &Ui, state: &mut AddonState) {
    render_left_section_header(ui, &t("section.scale"), state.config.section_spacing);
    let tiers = [CombatTier::Solo, CombatTier::Party, CombatTier::Squad];
    let labels: Vec<String> = tiers
        .iter()
        .map(|tier| t(scale_i18n_key(&state.main.game_mode, *tier)))
        .collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let selected = tiers
        .iter()
        .position(|t| *t == state.main.combat_tier)
        .unwrap_or(2);
    if let Some(i) = theme::segment_row(ui, &labels, selected, "##scale") {
        state.main.combat_tier = tiers[i];
        if let Some(role) = state.main.selected_role {
            state.main.weights = role.to_weights_for(&state.main.game_mode, state.main.combat_tier);
        }
        state.main.comparison.suggestions.clear();
        state.main.comparison.run_locked_spec = None;
        state.main.comparison.error = None;
    }
}

fn role_pip(role: RoleObjective) -> [f32; 4] {
    match role {
        RoleObjective::WvWRoamer => [0.95, 0.48, 0.18, 1.0],
        RoleObjective::PowerDps => theme::PIP_DAMAGE,
        RoleObjective::CondiDps => [0.52, 0.82, 0.28, 1.0],
        RoleObjective::Hybrid => [0.95, 0.78, 0.35, 1.0],
        RoleObjective::Sustain => [0.72, 0.52, 0.88, 1.0],
        RoleObjective::Staller => [0.62, 0.48, 0.32, 1.0],
        RoleObjective::Healer => theme::PIP_HEAL,
        RoleObjective::Buffer => [0.32, 0.78, 0.82, 1.0],
        RoleObjective::Disabler => theme::PIP_CTRL,
        RoleObjective::Tank => theme::PIP_FRONT,
        RoleObjective::WvWZergDps | RoleObjective::PvPBurst => theme::PIP_DAMAGE,
        RoleObjective::WvWZergSupport => theme::PIP_HEAL,
        RoleObjective::WvWDisruptor | RoleObjective::PvPDisruptor => theme::PIP_CTRL,
        RoleObjective::PvPSustain => [0.32, 0.78, 0.82, 1.0],
    }
}

fn named_tab(n: u32, name: Option<&str>) -> String {
    let n = n.to_string();
    let name = match name {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => t("label.unnamed"),
    };
    tf("fmt.tab", &[("n", &n), ("name", &name)])
}

fn apply_role(state: &mut AddonState, role: RoleObjective) {
    state.main.selected_role = Some(role);
    state.main.weights = role.to_weights_for(&state.main.game_mode, state.main.combat_tier);
    state.main.comparison.suggestions.clear();
    state.main.comparison.run_locked_spec = None;
    state.main.comparison.selected_suggestion = 0;
    state.main.comparison.error = None;
}

/// Scale labels are per mode: the same three tiers are Roam / Havoc /
/// Cloud in WvW and Open World / Group / Squad in PvE, and PvP has no
/// scale of its own.
pub(crate) fn scale_i18n_key(game_mode: &GameMode, tier: CombatTier) -> &'static str {
    match (game_mode, tier) {
        (GameMode::WvW, CombatTier::Solo) => "scale.roam",
        (GameMode::WvW, CombatTier::Party) => "scale.havoc",
        (GameMode::WvW, CombatTier::Squad) => "scale.cloud",
        (_, CombatTier::Solo) => "scale.open_world",
        (_, CombatTier::Party) => "scale.group",
        (_, CombatTier::Squad) => "scale.squad",
    }
}

/// The front-line role is a Commander in WvW and a Tank in PvE and PvP —
/// same job, and the only chip whose name changes with the mode. A raid
/// tank holds a boss's attention; a WvW commander holds a line.
fn tank_is_commander(game_mode: &GameMode) -> bool {
    *game_mode == GameMode::WvW
}

pub(crate) fn role_i18n_key(game_mode: &GameMode, role: RoleObjective) -> &'static str {
    match role {
        RoleObjective::WvWRoamer => "role.roamer",
        RoleObjective::PowerDps => "role.damage",
        RoleObjective::CondiDps => "role.condi",
        RoleObjective::Healer => "role.heal",
        RoleObjective::Sustain => "role.bruiser",
        RoleObjective::Staller => "role.troll",
        RoleObjective::Buffer => "role.support",
        RoleObjective::Disabler => "role.disable",
        RoleObjective::Tank if tank_is_commander(game_mode) => "role.commander",
        RoleObjective::Tank => "role.tank",
        _ => "label.pick_role",
    }
}

fn role_hint_key(game_mode: &GameMode, role: RoleObjective) -> &'static str {
    match role {
        RoleObjective::WvWRoamer => "role.hint.roamer",
        RoleObjective::PowerDps => "role.hint.damage",
        RoleObjective::CondiDps => "role.hint.condi",
        RoleObjective::Healer => "role.hint.heal",
        RoleObjective::Sustain => "role.hint.bruiser",
        RoleObjective::Staller => "role.hint.troll",
        RoleObjective::Buffer => "role.hint.support",
        RoleObjective::Disabler => "role.hint.disable",
        RoleObjective::Tank if tank_is_commander(game_mode) => "role.hint.commander",
        RoleObjective::Tank => "role.hint.tank",
        _ => "label.pick_role",
    }
}

fn render_role_chips(ui: &Ui, state: &mut AddonState) {
    render_left_section_header(ui, &t("section.role"), state.config.section_spacing);

    let current = state.main.selected_role;
    let avail = ui.content_region_avail()[0];
    let mut row_x = 0.0_f32;
    let mut picked: Option<RoleObjective> = None;
    for &role in RoleObjective::play_roles_for(&state.main.game_mode) {
        let label = t(role_i18n_key(&state.main.game_mode, role));
        let id = format!("##play_{:?}", role);
        let [cw, _] = theme::select_chip_size(ui, &label, true);
        theme::wrap_chip(ui, avail, &mut row_x, cw, 4.0);
        let selected = current == Some(role);
        if theme::select_chip(ui, &label, selected, &id, Some(role_pip(role))) {
            picked = Some(role);
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(t(role_hint_key(&state.main.game_mode, role)));
        }
    }
    if let Some(role) = picked {
        apply_role(state, role);
    }
}

/// Switch the left panel to character `idx` (`name`): the combo's pick, and
/// the Generations tab opening a record made on another character.
pub(in crate::ui::main_view) fn select_character(state: &mut AddonState, idx: usize, name: String) {
    state.main.selected_character = Some(idx);
    state.main.current_build = None;
    state.main.current_stats = None;
    state.main.build_tabs.clear();
    state.main.equipment_tabs.clear();
    state.main.selected_build_tab = None;
    state.main.selected_equipment_tab = None;
    state.main.build_chat_code = None;
    // Clear prior character's suggestions, combat metrics, and locks — the new
    // character may be a different profession with different specs entirely.
    state.main.comparison.suggestions.clear();
    state.main.comparison.run_locked_spec = None;
    state.main.comparison.selected_suggestion = 0;
    state.main.comparison.error = None;
    state.main.comparison.show_optimized = false;
    state.main.comparison.current_combat_solo = None;
    state.main.comparison.current_combat_party = None;
    state.main.comparison.current_combat_squad = None;
    state.main.build_locks = gw2_core::types::BuildLocks::default();
    character::load_character_tabs(state, name);
}

/// Put the left panel's mode, scale, role and weights back to a stored
/// scenario, with what the mode, scale and role controls do on a change:
/// suggestions scored in the old scenario go, and a new mode drops the
/// locks. The caller re-resolves the current build (a character switch does
/// it anyway). Returns whether the mode changed.
pub(in crate::ui::main_view) fn restore_scenario(
    state: &mut AddonState,
    mode: GameMode,
    tier: CombatTier,
    role: Option<RoleObjective>,
    weights: OptimizationWeights,
) -> bool {
    let main = &mut state.main;
    let mode_changed = main.game_mode != mode;
    if !mode_changed
        && main.combat_tier == tier
        && main.selected_role == role
        && main.weights == weights
    {
        return false;
    }
    main.game_mode = mode;
    main.combat_tier = tier;
    main.selected_role = role;
    main.weights = weights;
    main.comparison.suggestions.clear();
    main.comparison.run_locked_spec = None;
    main.comparison.selected_suggestion = 0;
    main.comparison.error = None;
    if mode_changed {
        main.build_locks = gw2_core::types::BuildLocks::default();
    }
    mode_changed
}

/// Character picker + build/equip template dropdowns.
fn render_left_character_section(ui: &Ui, state: &mut AddonState) {
    render_left_section_header(ui, &t("section.character"), state.config.section_spacing);
    ui.spacing();

    // Character dropdown
    ui.set_next_item_width(-1.0);
    let preview = state
        .main
        .selected_character
        .and_then(|i| state.main.characters.get(i))
        .cloned()
        .unwrap_or_else(|| {
            if state.main.characters_loading {
                t("status.loading")
            } else if state.main.characters.is_empty() {
                t("status.no_characters")
            } else {
                t("status.select")
            }
        });

    let mut new_selection: Option<(usize, String)> = None;
    let combo_open = ComboBox::new("##char_select")
        .preview_value(&preview)
        .begin(ui);
    if combo_open.is_some() && !state.main.char_combo_open {
        state.main.char_combo_open = true;
        character::reload_from_api(state);
    }
    if combo_open.is_none() {
        state.main.char_combo_open = false;
    }
    let chars_snapshot = state.main.characters.clone();
    if let Some(_combo) = combo_open {
        for (i, name) in chars_snapshot.iter().enumerate() {
            let selected = state.main.selected_character == Some(i);
            if Selectable::new(name).selected(selected).build(ui)
                && state.main.selected_character != Some(i)
            {
                new_selection = Some((i, name.clone()));
            }
        }
    }

    if let Some((idx, name)) = new_selection {
        select_character(state, idx, name);
    }

    if !state.main.build_tabs.is_empty() {
        ui.spacing();
        ui.text_colored(theme::pal().muted, t("label.build"));
        ui.set_next_item_width(-1.0);
        let bt_preview = state
            .main
            .selected_build_tab
            .and_then(|i| state.main.build_tabs.get(i))
            .map(|tab| named_tab(tab.tab, tab.build.name.as_deref()))
            .unwrap_or_else(|| t("status.select"));

        let bt_labels: Vec<(usize, String)> = state
            .main
            .build_tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| (i, named_tab(tab.tab, tab.build.name.as_deref())))
            .collect();

        let mut bt_changed: Option<usize> = None;
        if !bt_labels.is_empty() {
            if let Some(_combo) = ComboBox::new("##build_tab_select")
                .preview_value(&bt_preview)
                .begin(ui)
            {
                for (i, label) in &bt_labels {
                    let selected = state.main.selected_build_tab == Some(*i);
                    if Selectable::new(label).selected(selected).build(ui)
                        && state.main.selected_build_tab != Some(*i)
                    {
                        bt_changed = Some(*i);
                    }
                }
            }
        }

        if let Some(idx) = bt_changed {
            state.main.selected_build_tab = Some(idx);
            state.main.comparison.suggestions.clear();
            state.main.comparison.run_locked_spec = None;
            state.main.comparison.selected_suggestion = 0;
            state.main.comparison.error = None;
            state.main.comparison.show_optimized = false;
            character::update_build_chat_code(state);
            resolution::resolve_selected_build(state);
        }
    }

    // Equipment Template dropdown
    if !state.main.equipment_tabs.is_empty() {
        ui.text_colored(theme::pal().muted, t("label.equipment"));
        ui.set_next_item_width(-1.0);
        let et_preview = state
            .main
            .selected_equipment_tab
            .and_then(|i| state.main.equipment_tabs.get(i))
            .map(|tab| named_tab(tab.tab, tab.name.as_deref()))
            .unwrap_or_else(|| t("status.select"));

        let et_labels: Vec<(usize, String)> = state
            .main
            .equipment_tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| (i, named_tab(tab.tab, tab.name.as_deref())))
            .collect();

        let mut et_changed: Option<usize> = None;
        if !et_labels.is_empty() {
            if let Some(_combo) = ComboBox::new("##equip_tab_select")
                .preview_value(&et_preview)
                .begin(ui)
            {
                for (i, label) in &et_labels {
                    let selected = state.main.selected_equipment_tab == Some(*i);
                    if Selectable::new(label).selected(selected).build(ui)
                        && state.main.selected_equipment_tab != Some(*i)
                    {
                        et_changed = Some(*i);
                    }
                }
            }
        }

        if let Some(idx) = et_changed {
            state.main.selected_equipment_tab = Some(idx);
            state.main.comparison.suggestions.clear();
            state.main.comparison.run_locked_spec = None;
            state.main.comparison.selected_suggestion = 0;
            state.main.comparison.error = None;
            state.main.comparison.show_optimized = false;
            resolution::resolve_selected_build(state);
        }
    }

    if state.main.build_loading {
        ui.text_colored(theme::WARN, t("status.resolving"));
    }
}

/// Build controls: mode, scale, shared roles, optional weight radar, actions.
fn render_left_build_controls(ui: &Ui, state: &mut AddonState) {
    render_left_section_header(ui, &t("section.mode"), state.config.section_spacing);
    let mode_idx = GameMode::ALL
        .iter()
        .position(|m| *m == state.main.game_mode)
        .unwrap_or(0);
    if let Some(i) = theme::segment_row(ui, &["PvE", "PvP", "WvW"], mode_idx, "##mode") {
        let mode = GameMode::ALL[i].clone();
        state.main.game_mode = mode.clone();
        // The modes do not share a role list, so a chip chosen in one may
        // not exist in the next. Keeping it would leave the focus line
        // naming a role with no chip lit anywhere in the row.
        if let Some(role) = state.main.selected_role {
            if !RoleObjective::play_roles_for(&mode).contains(&role) {
                state.main.selected_role = None;
            }
        }
        state.main.weights = if let Some(role) = state.main.selected_role {
            role.to_weights_for(&mode, state.main.combat_tier)
        } else {
            OptimizationWeights::default_for_mode(mode.label())
        };
        state.main.comparison.suggestions.clear();
        state.main.comparison.run_locked_spec = None;
        state.main.comparison.selected_suggestion = 0;
        state.main.comparison.error = None;
        state.main.build_locks = gw2_core::types::BuildLocks::default();
        resolution::resolve_selected_build(state);
    }

    // PvP is always five a side, so it gets no scale row.
    if state.main.game_mode != GameMode::PvP {
        render_scale_row(ui, state);
    }

    render_role_chips(ui, state);

    ui.spacing();
    let role_bit = state
        .main
        .selected_role
        .map(|r| t(role_i18n_key(&state.main.game_mode, r)))
        .unwrap_or_else(|| t("label.pick_role"));
    let focus = if state.main.game_mode == GameMode::PvP {
        format!("{} · {}", state.main.game_mode.label(), role_bit)
    } else {
        format!(
            "{} · {} · {}",
            state.main.game_mode.label(),
            t(scale_i18n_key(
                &state.main.game_mode,
                state.main.combat_tier
            )),
            role_bit
        )
    };
    theme::wrapped(ui, theme::CURRENT, &focus);

    if ui.collapsing_header(t("weights.fine_tune"), TreeNodeFlags::DEFAULT_OPEN) {
        let current_axes = state
            .main
            .comparison
            .current_combat_solo
            .as_ref()
            .map(crate::ui::radar_chart::compute_axes_from_metrics);
        let optimized_axes = if !state.main.comparison.suggestions.is_empty() {
            let idx = state
                .main
                .comparison
                .selected_suggestion
                .min(state.main.comparison.suggestions.len() - 1);
            state.main.comparison.suggestions[idx]
                .combat_solo
                .as_ref()
                .map(crate::ui::radar_chart::compute_axes_from_metrics)
        } else {
            None
        };

        let show_current = matches!(state.main.active_tab, MainTab::Improve | MainTab::Talk);
        let _chart_modified = crate::ui::radar_chart::render_radar_chart(
            ui,
            &mut state.main.weights,
            &mut state.main.radar_dragging,
            if show_current {
                current_axes.as_ref()
            } else {
                None
            },
            optimized_axes.as_ref(),
        );
        if current_axes.is_some() || optimized_axes.is_some() {
            crate::ui::radar_chart::render_legend(
                ui,
                show_current && current_axes.is_some(),
                optimized_axes.is_some(),
            );
        }
    }

    render_left_section_header(ui, &t("section.actions"), state.config.section_spacing);

    let is_improve = state.main.active_tab == MainTab::Improve;
    let btn_label_owned;
    let btn_label: &str = if is_improve {
        btn_label_owned = t("btn.improve_build");
        &btn_label_owned
    } else if let Some(role) = state.main.selected_role {
        let role_l = t(role_i18n_key(&state.main.game_mode, role));
        btn_label_owned = tf("btn.optimize_role", &[("role", &role_l)]);
        &btn_label_owned
    } else {
        btn_label_owned = t("btn.optimize_build");
        &btn_label_owned
    };
    let disabled = state.main.optimizing
        || state.main.chat.waiting
        || state.main.game_db.is_none()
        || (is_improve && state.main.current_build.is_none());

    if disabled {
        let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        // Height 0 means `control_height`, which follows the font scale. A
        // hardcoded 28px was the only control in the panel that did not, so
        // at any scale above the default the primary action was the
        // shortest thing on screen.
        theme::gold_button_sized(ui, btn_label, [-1.0, 0.0]);
        style.pop();
        if ui.is_item_hovered() {
            ui.tooltip_text(if state.main.optimizing {
                t("status.opt_in_progress")
            } else if state.main.chat.waiting {
                t("status.thinking")
            } else if state.main.game_db.is_none() {
                t("status.wait_data")
            } else {
                t("status.select_character")
            });
        }
    } else if theme::gold_button_sized(ui, btn_label, [-1.0, 0.0]) {
        if is_improve {
            let profession_name = state
                .main
                .current_build
                .as_ref()
                .map(|b| b.profession.clone());
            if let Some(ref prof_name) = profession_name {
                optimize_flow::start_optimization_with_profession(state, prof_name);
            }
        } else {
            optimize_flow::start_optimization(state);
        }
    }
}

fn render_main_content(ui: &Ui, state: &mut AddonState) {
    match state.main.active_tab {
        MainTab::NewBuild => {
            tabs::new_build::render_new_build_tab(ui, state);
        }
        MainTab::Improve => {
            tabs::improve::render_improve_tab(ui, state);
        }
        MainTab::Talk => {
            tabs::kitchen::render_talk_tab(ui, state);
        }
        MainTab::SaveLoad => {
            tabs::saveload::render_saveload_tab(ui, state);
        }
        MainTab::Settings => {
            tabs::settings::render_settings_tab(ui, state);
        }
        MainTab::News => {
            tabs::news::render_news_tab(ui, state);
        }
        MainTab::About => {
            tabs::about::render_about_tab(ui, state);
        }
        MainTab::Radio => {
            tabs::radio::render_radio_tab(ui, state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_core::i18n;

    /// Every chip a mode offers has to have a label and a hint of its own.
    ///
    /// `role_i18n_key` and `role_hint_key` both fall back to
    /// `label.pick_role` for a role they do not know, so a role added to a
    /// mode's list without a mapping renders as a chip reading "Pick a role"
    /// — no compile error, no panic, just a nonsense menu. That is exactly
    /// what would have happened when PvE gained the Condi and Heal chips.
    #[test]
    fn every_offered_role_has_its_own_label_and_hint() {
        for mode in GameMode::ALL {
            for &role in RoleObjective::play_roles_for(&mode) {
                let label = role_i18n_key(&mode, role);
                let hint = role_hint_key(&mode, role);
                assert_ne!(
                    label, "label.pick_role",
                    "{mode:?} offers {role:?} with no label key"
                );
                assert_ne!(
                    hint, "label.pick_role",
                    "{mode:?} offers {role:?} with no hint key"
                );
                // And the key has to resolve — a mapping to a key no locale
                // defines renders the key itself on screen.
                assert_ne!(i18n::t(label), label, "{label} is not in the locales");
                assert_ne!(i18n::t(hint), hint, "{hint} is not in the locales");
            }
        }
    }

    /// The scale words are per mode, and PvE must not be shown WvW's.
    #[test]
    fn each_mode_scales_in_its_own_words() {
        for mode in GameMode::ALL {
            for tier in [CombatTier::Solo, CombatTier::Party, CombatTier::Squad] {
                let key = scale_i18n_key(&mode, tier);
                assert_ne!(i18n::t(key), key, "{key} is not in the locales");
            }
        }
        assert_eq!(
            scale_i18n_key(&GameMode::WvW, CombatTier::Solo),
            "scale.roam"
        );
        assert_eq!(
            scale_i18n_key(&GameMode::PvE, CombatTier::Solo),
            "scale.open_world",
            "a solo open-world player is not choosing between Havoc and Cloud"
        );
        assert_eq!(
            scale_i18n_key(&GameMode::PvE, CombatTier::Squad),
            "scale.squad"
        );
    }
}
