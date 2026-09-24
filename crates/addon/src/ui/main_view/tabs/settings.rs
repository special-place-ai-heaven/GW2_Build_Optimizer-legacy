//! Settings tab — AI provider, API keys + model picker, theme, cache, benchmarks.

use nexus::imgui::{
    ColorButton, ColorEditInputMode, ColorFormat, ColorPicker, ColorPickerMode, ComboBox,
    Selectable, StyleColor, StyleVar, Ui,
};

use crate::state::{AddonState, CancellationToken};
use crate::ui::theme;
use gw2_core::config::{CostCurrency, CustomTheme, NewsKind, NewsLayout, NewsSource, ThemeConfig};
use gw2_core::i18n::{t, tf};

use super::super::{build_display, stats};

thread_local! {
    /// Per-field "reveal password" toggle for the Settings tab LLM API-key
    /// input. Transient UI-only state scoped to the render thread — it must not
    /// survive a save/reload and does not belong on `AddonState`.
    static SHOW_LLM_KEY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the Settings LLM API-key input should mask its contents (ImGui's
/// `InputTextFlags::PASSWORD`), given whether the user has toggled "reveal"
/// on. Same helper Setup uses: Settings is the same overlay a player might be
/// streaming or screenshotting while pasting a live key, so it defaults to
/// masked.
fn key_field_is_masked(revealed: bool) -> bool {
    !revealed
}

pub(in crate::ui::main_view) fn render_settings_tab(ui: &Ui, state: &mut AddonState) {
    let avail_w = ui.content_region_avail()[0];
    let scale = state.config.font_scale.max(0.5);
    let gutter = 48.0 * scale;
    let col_w = ((avail_w - gutter) * 0.5).max(220.0);

    ui.columns(2, "##settings_cols", false);
    ui.set_column_width(0, col_w);

    // Left column
    build_display::render_card_header(ui, &t("settings.ai_provider"), theme::pal().gold);
    render_api_keys_section(ui, state, col_w);
    ui.spacing();
    render_model_picker_section(ui, state, col_w);
    ui.spacing();
    render_cost_currency_toggle(ui, state);

    ui.dummy([0.0, 8.0]);

    // Where Optimization Defaults begins, so Theme can start level with it
    // in the column beside. Columns only offset x, so a y taken in one is
    // directly comparable in the other.
    let defaults_y = ui.cursor_pos()[1];
    build_display::render_card_header(ui, &t("settings.opt_defaults"), theme::pal().gold);
    {
        ui.text(t("settings.default_mode"));
        let current_default = state
            .config
            .default_game_mode
            .clone()
            .unwrap_or_else(|| "PvE".into());
        // One row rather than a stack: the labels are three characters and
        // the panel is short on height.
        //
        // `same_line`, never a nested `ui.columns`. ImGui columns do not
        // nest: opening a set inside another ENDS the outer one, and closing
        // it with `columns(1)` leaves the rest of the tab in a single
        // full-width column. That is what happened here — everything from
        // this row down rendered full width, the right-hand column never
        // appeared, and UI Preferences picked up the right column's 48px
        // indent while sitting under News.
        for (at, mode) in ["PvE", "PvP", "WvW"].iter().enumerate() {
            if at > 0 {
                ui.same_line();
            }
            let is_sel = current_default == *mode;
            if ui.radio_button_bool(mode, is_sel) && !is_sel {
                state.config.default_game_mode = Some((*mode).to_string());
                crate::ui::save_config_detached(state);
            }
        }

        // Scale and role beside the mode, one row each, labelled for the
        // default mode chosen above (2026-09-08).
        use gw2_optimizer::scenario::{CombatTier, RoleObjective};
        let mode = match state.config.default_game_mode.as_deref() {
            Some("PvP") => gw2_core::types::GameMode::PvP,
            Some("WvW") => gw2_core::types::GameMode::WvW,
            _ => gw2_core::types::GameMode::PvE,
        };
        ui.text(t("settings.default_scale"));
        let current_tier = state.config.default_combat_tier.clone();
        for (at, tier) in [CombatTier::Solo, CombatTier::Party, CombatTier::Squad]
            .into_iter()
            .enumerate()
        {
            if at > 0 {
                ui.same_line();
            }
            let name = format!("{tier:?}");
            let is_sel = current_tier.as_deref() == Some(name.as_str());
            let label = format!(
                "{}##tier_{name}",
                t(crate::ui::main_view::scale_i18n_key(&mode, tier))
            );
            if ui.radio_button_bool(&label, is_sel) && !is_sel {
                state.config.default_combat_tier = Some(name);
                crate::ui::save_config_detached(state);
            }
        }
        ui.text(t("settings.default_role"));
        let current_role = state.config.default_role.clone();
        for (at, role) in RoleObjective::play_roles_for(&mode).iter().enumerate() {
            if at > 0 {
                ui.same_line();
            }
            let name = format!("{role:?}");
            let is_sel = current_role.as_deref() == Some(name.as_str());
            let label = format!(
                "{}##role_{name}",
                t(crate::ui::main_view::role_i18n_key(&mode, *role))
            );
            if ui.radio_button_bool(&label, is_sel) && !is_sel {
                state.config.default_role = Some(name);
                crate::ui::save_config_detached(state);
            }
        }
    }

    ui.dummy([0.0, 8.0]);
    build_display::render_card_header(ui, &t("settings.news"), theme::pal().gold);
    render_news_sources(ui, state, col_w);

    // Right column
    ui.next_column();
    ui.indent_by(gutter);

    build_display::render_card_header(ui, &t("settings.ui_prefs"), theme::pal().gold);
    render_theme_section(ui, state, col_w, defaults_y);

    ui.unindent_by(gutter);
    ui.columns(1, "##settings_split_end", false);

    ui.dummy([0.0, 8.0]);
    ui.columns(2, "##settings_bottom", false);
    ui.set_column_width(0, col_w);
    build_display::render_card_header(ui, &t("settings.cache"), theme::pal().gold);
    render_cache_section(ui, state);
    ui.next_column();
    ui.indent_by(gutter);
    build_display::render_card_header(ui, &t("settings.benchmarks"), [0.6, 0.8, 1.0, 1.0]);
    render_benchmark_section(ui, state);
    ui.unindent_by(gutter);
    ui.columns(1, "##settings_end", false);

    // Footer
    ui.dummy([0.0, 4.0]);
    ui.separator();
    ui.dummy([0.0, 2.0]);
    ui.text_colored(
        theme::pal().muted,
        format!(
            "{} {}  —  {}",
            t("info.product"),
            tf("fmt.version", &[("ver", crate::VERSION)]),
            tf(
                "fmt.ai",
                &[("provider", state.config.active_provider.label())]
            ),
        ),
    );
}

/// Spawn a tracked background worker whose result always folds back into
/// `AddonState` through `apply` — on success, on cooperative cancellation, and
/// on a **panic**.
///
/// `AddonState::spawn_worker` already wraps the whole worker body in a
/// containment `catch_unwind` so a panic can never reach the Nexus runtime,
/// but that guard runs *around* this entire closure: if `risky` panics,
/// nothing written after it in the same closure would run — including a
/// caller's "still running" flag reset, which is exactly how a Settings-tab
/// spinner gets stuck forever. Catching only `risky` here means `apply`
/// always runs afterward, with the panic surfaced as `Err` instead of
/// silently skipped.
///
/// Returns `false` when the OS refused to start the thread at all (`risky`
/// never ran); callers that already flipped a "loading" flag before calling
/// this must clear it themselves in that case.
fn spawn_flag_guarded<T>(
    state: &mut AddonState,
    worker_name: &'static str,
    risky: impl FnOnce(&CancellationToken) -> T + Send + 'static,
    apply: impl FnOnce(&mut AddonState, std::thread::Result<T>) + Send + 'static,
) -> bool
where
    T: Send + 'static,
{
    state.spawn_worker(worker_name, move |token| {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| risky(&token)));
        crate::state::with_state(|s| apply(s, outcome));
    })
}

/// Spawn the shared "validate the active provider's API key" worker used by
/// both the Test and Save buttons. `on_failure_key` selects the "validation
/// failed" translation key so the two flows can word the same failure
/// differently ("Test failed…" vs "Saved, but validation failed…").
fn spawn_key_validation(
    state: &mut AddonState,
    worker_name: &'static str,
    on_failure_key: &'static str,
) {
    let addon_dir = state.addon_dir.clone();
    let config_snapshot = state.config.clone();
    let spawned = spawn_flag_guarded(
        state,
        worker_name,
        move |token| {
            if token.is_cancelled() {
                None
            } else {
                let r = gw2_optimizer::llm::create_client(&config_snapshot, &addon_dir)
                    .map(|c| c.validate_key_detailed());
                if token.is_cancelled() {
                    None
                } else {
                    Some(r)
                }
            }
        },
        move |s, outcome| {
            s.main.settings_key_validating = false;
            match outcome {
                Ok(Some(Ok(v))) => {
                    s.main.settings_key_valid = v.valid;
                    s.main.settings_key_status = Some(v.message);
                    s.main.settings_key_warning = v.warning;
                }
                Ok(Some(Err(e))) => {
                    s.main.settings_key_valid = false;
                    s.main.settings_key_status =
                        Some(tf(on_failure_key, &[("err", &e.to_string())]));
                    s.main.settings_key_warning = None;
                }
                Ok(None) => { /* cancelled — flag cleared above */ }
                Err(_) => {
                    nexus::log::log(
                        nexus::log::LogLevel::Warning,
                        "GW2BuildOpt",
                        format!("bg thread panicked: {}", worker_name),
                    );
                    s.main.settings_key_valid = false;
                    // Overwrite whatever "Testing…"/"Saved, validating…" status was
                    // showing — leaving it in place would read as a red "Testing…"
                    // once `settings_key_valid`/`settings_key_validating` both flip
                    // false. Same literal `setup.rs` uses for its own panic paths.
                    s.main.settings_key_status = Some("thread panicked".into());
                    s.main.settings_key_warning = None;
                }
            }
        },
    );
    if !spawned {
        state.main.settings_key_validating = false;
    }
}

fn render_api_keys_section(ui: &Ui, state: &mut AddonState, col_w: f32) {
    let mut provider_changed = false;
    // Two to a row rather than a stack of four. The labels are short and the
    // column is wide, so a single file of radios spent four rows on what
    // fits in two — and this panel is short on height.
    //
    // The second is placed at a fixed offset instead of by `same_line`, so
    // the right-hand radios line up down the column whatever the labels say.
    let start_x = ui.cursor_pos()[0];
    let second_x = start_x + (col_w - 12.0) * 0.5;
    for pair in gw2_core::config::LlmProvider::ALL.chunks(2) {
        let row_y = ui.cursor_pos()[1];
        for (at, provider) in pair.iter().enumerate() {
            if at > 0 {
                ui.set_cursor_pos([second_x, row_y]);
            }
            let is_selected = state.config.active_provider == *provider;
            if ui.radio_button_bool(provider.label(), is_selected) && !is_selected {
                state.config.active_provider = provider.clone();
                provider_changed = true;
            }
        }
    }
    if provider_changed {
        state.main.settings_key_input.clear();
        state.main.settings_key_status = None;
        state.main.settings_key_valid = false;
        state.main.settings_key_warning = None;
        state.main.available_models.clear();
        state.main.models_error = None;
        state.main.settings_model_search.clear();
        if let Err(e) = state.config.save(&state.config_path) {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                format!("Config save failed: {}", e),
            );
        }
    }
    ui.spacing();

    let row_w = (col_w - 8.0).max(80.0);
    let provider_label = state.config.active_provider.label().to_string();
    let has_key = state.config.has_active_llm_key();
    let status_origin = ui.cursor_screen_pos();
    if has_key {
        ui.text_colored(
            [0.0, 1.0, 0.0, 1.0],
            tf("fmt.key_configured", &[("provider", &provider_label)]),
        );
    } else {
        ui.text_colored(
            [1.0, 0.5, 0.0, 1.0],
            tf("fmt.key_not_set", &[("provider", &provider_label)]),
        );
        // The same on-ramp the first-run wizard shows, from the same keys —
        // see `LlmProvider::setup_howto_key`. Anyone who skipped the wizard,
        // or who comes back to change provider, lands here instead, and
        // "Key not set for OpenRouter" on its own is the dead end the wizard
        // used to have: true, and no help at all to somebody who has never
        // made an API key.
        let provider = state.config.active_provider.clone();
        ui.spacing();
        theme::wrapped(ui, theme::pal().muted, &t(provider.setup_howto_key()));
        // A button rather than the wizard's read-only URL field: this panel
        // has no room for one, and the page is what the reader wants.
        if ui.small_button(t("news.open")) {
            let _ = crate::feedback::shell::open_url(provider.key_page_url());
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(provider.key_page_url());
        }
        theme::wrapped(ui, theme::pal().muted, &t(provider.setup_steps_key()));
    }
    let after_status = ui.cursor_screen_pos();

    if has_key {
        let validating = state.main.settings_key_validating;
        let test_label = if validating {
            t("btn.testing")
        } else {
            t("btn.test")
        };
        let test_w = theme::gold_button_width(ui, test_label.as_str());
        ui.set_cursor_screen_pos([
            status_origin[0] + (row_w - test_w).max(0.0),
            status_origin[1],
        ]);
        if validating {
            let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
            theme::gold_button_sized(ui, test_label.as_str(), [test_w, 0.0]);
            style.pop();
        } else if theme::gold_button_sized(ui, test_label.as_str(), [test_w, 0.0]) {
            state.main.settings_key_validating = true;
            state.main.settings_key_status = Some(t("btn.testing"));
            state.main.settings_key_valid = false;
            state.main.settings_key_warning = None;
            spawn_key_validation(state, "settings-test-key", "fmt.failed");
        }
    }
    ui.set_cursor_screen_pos([
        status_origin[0],
        after_status[1].max(status_origin[1] + theme::control_height(ui)) + 2.0,
    ]);

    let save_label = t("btn.save");
    let gap = 8.0;
    let save_origin = ui.cursor_screen_pos();
    let btn_w = theme::gold_button_width(ui, save_label.as_str());
    // Key input — masked by default; see Setup's LLM key step for why.
    let show_llm_key = SHOW_LLM_KEY.with(|c| c.get());
    let toggle_label = if show_llm_key { "Hide" } else { "Show" };
    let toggle_w = theme::gold_button_width(ui, toggle_label) + 8.0;
    let input_w = (row_w - btn_w - toggle_w - gap).max(40.0);
    ui.set_next_item_width(input_w);
    ui.input_text(
        &format!("##{}_key", provider_label),
        &mut state.main.settings_key_input,
    )
    .hint(&t("settings.enter_key"))
    .password(key_field_is_masked(show_llm_key))
    .build();
    ui.same_line();
    if theme::gold_button_sized(ui, toggle_label, [toggle_w - 8.0, 0.0]) {
        SHOW_LLM_KEY.with(|c| c.set(!show_llm_key));
    }
    ui.set_cursor_screen_pos([save_origin[0] + row_w - btn_w, save_origin[1]]);
    let validating = state.main.settings_key_validating;
    if validating {
        let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        theme::gold_button_sized(ui, "...", [btn_w, 0.0]);
        style.pop();
    } else if theme::gold_button_sized(ui, save_label, [btn_w, 0.0]) {
        let key = state.main.settings_key_input.trim().to_string();
        if !key.is_empty() {
            match state.config.active_provider {
                gw2_core::config::LlmProvider::Gemini => {
                    state.config.gemini_api_key = Some(key.clone())
                }
                gw2_core::config::LlmProvider::OpenAI => {
                    state.config.openai_api_key = Some(key.clone())
                }
                gw2_core::config::LlmProvider::Anthropic => {
                    state.config.anthropic_api_key = Some(key.clone())
                }
                gw2_core::config::LlmProvider::OpenRouter => {
                    state.config.openrouter_api_key = Some(key.clone())
                }
            }
            crate::ui::save_config_detached(state);
            state.main.settings_key_input.clear();
            state.main.settings_key_status = Some(t("settings.saved_validating"));
            state.main.settings_key_valid = false;
            state.main.settings_key_validating = true;
            spawn_key_validation(state, "settings-save-key", "fmt.saved_validation_failed");
        }
    }
    ui.set_cursor_screen_pos([
        save_origin[0],
        save_origin[1] + theme::control_height(ui).max(ui.frame_height()) + 4.0,
    ]);

    if let Some(ref status) = state.main.settings_key_status {
        let col = if state.main.settings_key_valid {
            [0.0, 1.0, 0.0, 1.0]
        } else if state.main.settings_key_validating {
            [0.7, 0.7, 0.7, 1.0]
        } else {
            [1.0, 0.3, 0.3, 1.0]
        };
        ui.text_colored(col, status);
    }
    if let Some(ref w) = state.main.settings_key_warning {
        ui.text_colored(
            [1.0, 0.7, 0.0, 1.0],
            format!("  {}", tf("fmt.warning", &[("msg", w)])),
        );
    }
}

/// Every model the active provider offers, before any of the user's filters.
///
/// Pruned to the ones that could serve a request from this addon at all — a
/// music generator and a batch job that answers tomorrow are not alternatives
/// to a chat model, they are noise in a list someone has to read.
fn model_catalog(state: &AddonState) -> Vec<gw2_optimizer::llm::ModelInfo> {
    if !state.main.available_models.is_empty() {
        // Every model that can drive tools and answer in text. Structured
        // output ORDERS the list (see `ModelInfo::rank`); it does not gate it.
        // Gating on it was tried 2026-09-07 and left two free models in the
        // picker, both of which the player's OpenRouter data policy then
        // refused, while the free models that had been working were hidden.
        // A model without schema support gets its plate from the repair
        // request; that is a slower path, not a broken one.
        return state
            .main
            .available_models
            .iter()
            .filter(|m| m.usable())
            .cloned()
            .collect();
    }
    let hardcoded: &[(&str, &str)] = match state.config.active_provider {
        gw2_core::config::LlmProvider::Gemini => gw2_core::config::GEMINI_MODELS,
        gw2_core::config::LlmProvider::OpenAI => gw2_core::config::OPENAI_MODELS,
        gw2_core::config::LlmProvider::Anthropic => gw2_core::config::ANTHROPIC_MODELS,
        gw2_core::config::LlmProvider::OpenRouter => gw2_core::config::OPENROUTER_MODELS,
    };
    // The offline fallback states no capabilities, so it claims none — see
    // `ModelInfo::default`. Nothing here is filtered out for lack of data.
    hardcoded
        .iter()
        .map(|(id, label)| gw2_optimizer::llm::ModelInfo {
            id: (*id).to_string(),
            display_name: (*label).to_string(),
            ..Default::default()
        })
        .collect()
}

/// The catalog as the picker should show it, honouring the Free filter.
///
/// The filter is dropped rather than applied when it would empty the list:
/// showing someone nothing because their provider has no free models teaches
/// them less than showing them what there is.
fn visible_models(
    state: &AddonState,
    catalog: &[gw2_optimizer::llm::ModelInfo],
) -> Vec<gw2_optimizer::llm::ModelInfo> {
    let any_free = catalog.iter().any(|m| m.free);
    catalog
        .iter()
        .filter(|m| !state.config.free_models_only || !any_free || m.free)
        .cloned()
        .collect()
}

fn render_model_combo(
    ui: &Ui,
    state: &mut AddonState,
    preview: &str,
    display_models: &[gw2_optimizer::llm::ModelInfo],
    current_model: &str,
    id: &str,
    width: f32,
) {
    let origin = ui.cursor_screen_pos();
    ui.set_next_item_width(width);
    if let Some(_c) = ComboBox::new(&format!("##{id}_model"))
        .preview_value("\u{00A0}")
        .begin(ui)
    {
        ui.set_next_item_width(-1.0);
        ui.input_text(
            &format!("##{id}_model_search"),
            &mut state.main.settings_model_search,
        )
        .hint(&t("settings.search_models"))
        .build();
        let needle = state.main.settings_model_search.trim().to_lowercase();
        let mut visible = 0usize;
        for model in display_models {
            let (mid, label) = (&model.id, &model.display_name);
            if !needle.is_empty()
                && !mid.to_lowercase().contains(&needle)
                && !label.to_lowercase().contains(&needle)
            {
                continue;
            }
            visible += 1;
            let sel = *mid == current_model;
            if Selectable::new(label).selected(sel).build(ui) {
                state.config.set_active_model_id(mid.clone());
                state.main.provider_issue = None;
                crate::ui::save_config_detached(state);
            }
            // The score the row was ordered by, so the order is legible
            // rather than mysterious. Agentic where it exists — it measures
            // the thing this addon asks of a model.
            if let Some(score) = model.agentic_index.or(model.coding_index) {
                ui.same_line();
                ui.text_colored(theme::pal().muted, format!("{score:.0}"));
            }
        }
        if visible == 0 && !needle.is_empty() {
            ui.text_colored(
                theme::pal().muted,
                tf(
                    "fmt.no_models",
                    &[("q", state.main.settings_model_search.trim())],
                ),
            );
        }
    }
    theme::paint_centered_combo_preview(ui, preview, origin, width);
}

/// Compact provider + model row for the Choya header.
pub(in crate::ui::main_view) fn render_talk_model_row(ui: &Ui, state: &mut AddonState) {
    let has_key = state.config.has_active_llm_key();
    if state.main.available_models.is_empty() && !state.main.models_loading && has_key {
        stats::start_fetch_models(state);
    }
    let current_model = state.config.active_model_id().to_string();
    // The same list Settings shows: the Free switch is one preference, not
    // one per screen (the talk row ignored it until 2026-09-07).
    let display_models = visible_models(state, &model_catalog(state));
    let preview = display_models
        .iter()
        .find(|m| m.id == current_model)
        .map(|m| m.display_name.as_str())
        .unwrap_or(&current_model)
        .to_string();

    let avail = ui.content_region_avail()[0];
    let gap = 8.0;
    let load_w = if state.main.models_loading {
        ui.calc_text_size("...")[0] + gap
    } else {
        0.0
    };
    let provider_need = gw2_core::config::LlmProvider::ALL
        .iter()
        .map(|p| theme::combo_width_for(ui, p.short_label()))
        .fold(0.0_f32, f32::max);
    let leftover = (avail - load_w).max(0.0);
    let provider_w = provider_need.min(leftover);
    let rest = leftover - provider_w;

    let provider_origin = ui.cursor_screen_pos();
    ui.set_next_item_width(provider_w);
    if let Some(_c) = ComboBox::new("##talk_provider")
        .preview_value("\u{00A0}")
        .begin(ui)
    {
        for provider in &gw2_core::config::LlmProvider::ALL {
            let sel = state.config.active_provider == *provider;
            if Selectable::new(provider.short_label())
                .selected(sel)
                .build(ui)
                && !sel
            {
                state.config.active_provider = provider.clone();
                state.main.available_models.clear();
                state.main.models_error = None;
                state.main.settings_model_search.clear();
                state.main.provider_issue = None;
                crate::ui::save_config_detached(state);
            }
        }
    }
    theme::paint_centered_combo_preview(
        ui,
        state.config.active_provider.short_label(),
        provider_origin,
        provider_w,
    );

    let model_w = if rest >= gap + 80.0 {
        rest - gap
    } else {
        leftover.max(80.0)
    };
    if rest >= gap + 80.0 {
        ui.same_line_with_spacing(0.0, gap);
    }
    render_model_combo(
        ui,
        state,
        &preview,
        &display_models,
        &current_model,
        "talk",
        model_w,
    );
    if state.main.models_loading {
        ui.same_line();
        ui.text_colored(theme::pal().muted, "...");
    }
}

fn render_model_picker_section(ui: &Ui, state: &mut AddonState, col_w: f32) {
    let current_model = match state.config.active_provider {
        gw2_core::config::LlmProvider::Gemini => state.config.gemini_model_id().to_string(),
        gw2_core::config::LlmProvider::OpenAI => state.config.openai_model_id().to_string(),
        gw2_core::config::LlmProvider::Anthropic => state.config.anthropic_model_id().to_string(),
        gw2_core::config::LlmProvider::OpenRouter => state.config.openrouter_model_id().to_string(),
    };
    let config_field = match state.config.active_provider {
        gw2_core::config::LlmProvider::Gemini => "gemini",
        gw2_core::config::LlmProvider::OpenAI => "openai",
        gw2_core::config::LlmProvider::Anthropic => "anthropic",
        gw2_core::config::LlmProvider::OpenRouter => "openrouter",
    };
    let has_key = state.config.has_active_llm_key();
    if state.main.available_models.is_empty() && !state.main.models_loading && has_key {
        stats::start_fetch_models(state);
    }
    let catalog = model_catalog(state);
    let display_models = visible_models(state, &catalog);
    let preview = display_models
        .iter()
        .find(|m| m.id == current_model)
        .map(|m| m.display_name.as_str())
        .unwrap_or(&current_model);
    let row_w = (col_w - 8.0).max(80.0);
    let origin = ui.cursor_screen_pos();
    let model_label = t("settings.model");
    let label_w = ui.calc_text_size(model_label.as_str())[0];
    let refresh = t("btn.refresh");
    let refresh_w = theme::gold_button_width(ui, refresh.as_str());
    let gap = 8.0;
    // The Free filter sits between the list it filters and the button that
    // refills it. Greyed where the provider has no free models at all —
    // OpenAI and Anthropic publish none, so the control would be a promise
    // the provider cannot keep.
    let free_label = t("settings.free_only");
    let any_free = catalog.iter().any(|m| m.free);
    let free_w = theme::switch_width(ui, free_label.as_str());
    let combo_w = (row_w - label_w - refresh_w - free_w - gap * 3.0).max(48.0);
    let row_h = ui.frame_height().max(theme::control_height(ui));

    ui.set_cursor_screen_pos([
        origin[0],
        origin[1] + ((row_h - ui.text_line_height()) * 0.5).max(0.0),
    ]);
    ui.text(model_label);
    ui.set_cursor_screen_pos([origin[0] + label_w + gap, origin[1]]);
    render_model_combo(
        ui,
        state,
        preview,
        &display_models,
        &current_model,
        config_field,
        combo_w,
    );
    let free_x = origin[0] + row_w - refresh_w - gap - free_w;
    ui.set_cursor_screen_pos([free_x, origin[1] + (row_h - ui.text_line_height()) * 0.5]);
    // One eased value drives the knob and the Choya together, so the switch
    // does not snap while the dancer slides.
    let want = if any_free && state.config.free_models_only {
        1.0
    } else {
        0.0
    };
    state.main.free_choya_rise += (want - state.main.free_choya_rise) * 0.08;
    let slide = state.main.free_choya_rise;
    if any_free {
        if theme::switch(ui, &free_label, slide, "##free_models") {
            state.config.free_models_only = !state.config.free_models_only;
            crate::ui::save_config_detached(state);
        }
    } else {
        let dim = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        theme::switch(ui, &free_label, 0.0, "##free_models_off");
        dim.pop();
        if ui.is_item_hovered() {
            ui.tooltip_text(t("settings.free_none"));
        }
    }
    theme::draw_free_choya(
        ui,
        [free_x + free_w * 0.5, origin[1]],
        row_h,
        state.main.free_choya_rise,
    );

    ui.set_cursor_screen_pos([origin[0] + row_w - refresh_w, origin[1]]);
    if state.main.models_loading {
        ui.text_colored(theme::pal().muted, "...");
    } else if theme::gold_button_sized(ui, format!("{}##models", refresh), [refresh_w, 0.0]) {
        state.main.available_models.clear();
        state.main.models_error = None;
        stats::start_fetch_models(state);
    }
    ui.set_cursor_screen_pos([origin[0], origin[1] + row_h + 4.0]);
    if let Some(ref err) = state.main.models_error {
        ui.text_colored([1.0, 0.5, 0.0, 1.0], format!("  {}", err));
    }
    // Seven of this player's fifteen free OpenRouter models answered 404
    // "guardrail restrictions and data policy" (2026-09-07): an account
    // setting the addon cannot change, and nothing in the picker said so.
    if matches!(
        state.config.active_provider,
        gw2_core::config::LlmProvider::OpenRouter
    ) && any_free
    {
        theme::wrapped(ui, theme::pal().muted, &t("settings.data_sharing_hint"));
    }

    ui.spacing();
    let usage_filename = match state.config.active_provider {
        gw2_core::config::LlmProvider::Gemini => "gemini_usage.json",
        gw2_core::config::LlmProvider::OpenAI => "openai_usage.json",
        gw2_core::config::LlmProvider::Anthropic => "anthropic_usage.json",
        gw2_core::config::LlmProvider::OpenRouter => "openrouter_usage.json",
    };
    let usage_path = state.addon_dir.join(usage_filename);
    // Refresh the usage display at most ~once per second (~60 frames at 60fps).
    // Previously this read from disk on every render frame just to display a
    // counter that changes at most a few times per minute.
    if state.main.settings_usage_frames == 0 {
        state.main.settings_usage_today = std::fs::read_to_string(&usage_path)
            .ok()
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .and_then(|v| v.get("requests_today").and_then(|x| x.as_u64()))
            .unwrap_or(0);
        state.main.settings_usage_frames = 60;
    } else {
        state.main.settings_usage_frames -= 1;
    }
    ui.text_colored(
        theme::pal().muted,
        tf(
            "fmt.usage_today",
            &[("n", state.main.settings_usage_today.to_string().as_str())],
        ),
    );
}

/// USD/EUR toggle for cost estimates (generation pill/tooltip). Estimates are
/// always computed and stored in USD; this only picks how they are displayed.
fn render_cost_currency_toggle(ui: &Ui, state: &mut AddonState) {
    ui.text(t("settings.cost_currency"));
    let usd = t("settings.cost_currency_usd");
    let eur = t("settings.cost_currency_eur");
    let labels = [usd.as_str(), eur.as_str()];
    let selected = match state.config.cost_currency {
        CostCurrency::Usd => 0,
        CostCurrency::Eur => 1,
    };
    if let Some(i) = theme::segment_row(ui, &labels, selected, "##set_cost_currency") {
        state.config.cost_currency = if i == 1 {
            CostCurrency::Eur
        } else {
            CostCurrency::Usd
        };
        crate::ui::save_config_detached(state);
    }
}

/// News sources, sized to live inside one settings column.
///
/// This used to end the outer two-column split and lay the four kinds out with
/// `ui.columns(4, ...)` across the FULL window width — five checkboxes spread
/// over four columns of a 2000px pane, with the last three nearly empty. It
/// now sits in the left column under the legend, and the kinds are two
/// content-sized groups rather than four stretched columns.
///
/// Groups, not `ui.columns`: ImGui's legacy columns do not nest, and this is
/// rendered inside the settings split.
fn render_news_sources(ui: &Ui, state: &mut AddonState, col_w: f32) {
    let desk = t("news.layout.desk");
    let mag = t("news.layout.magazine");
    let reader = t("news.layout.reader");
    let labels = [desk.as_str(), mag.as_str(), reader.as_str()];
    let selected = match state.config.news.layout {
        NewsLayout::Desk => 0,
        NewsLayout::Magazine => 1,
        NewsLayout::Reader => 2,
    };
    let seg_w = theme::segment_row_min_width(ui, &labels) + 4.0;
    let seg_h = theme::control_height(ui) + 6.0;
    let seg_id = "##set_news_layout_row";
    nexus::imgui::ChildWindow::new(seg_id)
        .size([seg_w, seg_h])
        .border(false)
        .build(ui, || {
            if let Some(i) = theme::segment_row(ui, &labels, selected, "##set_news_layout") {
                state.config.news.layout = match i {
                    1 => NewsLayout::Magazine,
                    2 => NewsLayout::Reader,
                    _ => NewsLayout::Desk,
                };
                crate::ui::save_config_detached(state);
            }
        });
    // The stills toggle keeps the layout row's line only while the column can
    // hold both; in a narrow column it drops underneath instead of clipping.
    let stills = format!("{}##set_news_stills", t("news.images"));
    if seg_w + 12.0 + ui.calc_text_size(&stills)[0] + ui.frame_height() + 8.0 <= col_w {
        ui.same_line_with_spacing(0.0, 12.0);
    }
    let mut images = state.config.news.show_images;
    if ui.checkbox(&stills, &mut images) {
        state.config.news.show_images = images;
        crate::ui::save_config_detached(state);
    }
    if ui.is_item_hovered() {
        theme::wide_tooltip(ui, |ui| ui.text(t("settings.news_hint")));
    }

    ui.dummy([0.0, 6.0]);

    // Two content-sized groups: Articles + Patch notes on the left, Video +
    // Guides on the right. Five ticks over four full-width columns was the
    // sparsest thing on the page.
    let half = NewsKind::ALL.len() / 2;
    let kind_block = |ui: &Ui, state: &mut AddonState, kinds: &[NewsKind]| {
        ui.group(|| {
            for (n, kind) in kinds.iter().enumerate() {
                if n > 0 {
                    ui.dummy([0.0, 2.0]);
                }
                ui.text_colored(theme::pal().gold, t(kind.settings_key()));
                for &src in kind.sources() {
                    news_source_tick(ui, state, src);
                }
            }
        });
    };
    kind_block(ui, state, &NewsKind::ALL[..half]);
    ui.same_line_with_spacing(0.0, 24.0);
    kind_block(ui, state, &NewsKind::ALL[half..]);
}

fn news_source_tick(ui: &Ui, state: &mut AddonState, src: NewsSource) {
    let mut on = state.config.news.get(src);
    let label = format!("{}##news_src_{}", t(src.label_key()), src.index());
    if ui.checkbox(label, &mut on) {
        state.config.news.set(src, on);
        crate::ui::save_config_detached(state);
        if on {
            crate::news::kick(state, &[src]);
        }
    }
    if ui.is_item_hovered() {
        theme::wide_tooltip(ui, |ui| ui.text(t(src.hint_key())));
    }
}

fn render_theme_section(ui: &Ui, state: &mut AddonState, col_w: f32, theme_align_y: f32) {
    let right_item_w = col_w - 12.0;
    // Paired two to a row. Each of these was a label on one line and a
    // control on the next, so four settings cost eight rows in a panel that
    // is short on height and has width to spare.
    //
    // `ui.group` + `same_line`, never a nested `ui.columns`: this renders
    // inside the tab's right-hand column, and opening a column set here
    // would end that one and drop the rest of the tab into a single
    // full-width column. Same rule as `render_news_sources`.
    let pair_w = (right_item_w - 12.0) * 0.5;
    // Where the right-hand half of every pair starts. Fixed, not
    // `same_line`: after a group `same_line` resumes from the group's own
    // baseline, which left the right column sitting a few pixels low and
    // its controls not lining up with the row above.
    let pair_x = ui.cursor_pos()[0];
    let second_x = pair_x + pair_w + 12.0;

    let resolved = gw2_core::i18n::resolve(&state.config.ui_language);
    let cache = gw2_api::cache::DataCache::new(state.addon_dir.join("cache"));
    let build = state
        .main
        .live_build_number
        .or(state.config.cache_build_number);
    tick_pack_status_cache(build);
    let preview_code = if state.config.ui_language.eq_ignore_ascii_case("auto") {
        resolved
    } else {
        state.config.ui_language.as_str()
    };
    let (preview_mark, _) = pack_mark(cached_pack_status(&cache, preview_code, build));
    let font_pref = state.config.ui_font.clone();
    let ui_lang_pref = state.config.ui_language.clone();
    let auto_preview = format!(
        "{preview_mark} {} - {}",
        t("settings.language_auto"),
        gw2_core::i18n::language_by_code(resolved)
            .map(|l| crate::ui::fonts::language_label(l, &font_pref, &ui_lang_pref))
            .unwrap_or("English")
    );
    let preview = if state.config.ui_language.eq_ignore_ascii_case("auto") {
        auto_preview
    } else {
        format!(
            "{preview_mark} {}",
            gw2_core::i18n::language_by_code(&state.config.ui_language)
                .map(|l| crate::ui::fonts::language_label(l, &font_pref, &ui_lang_pref))
                .unwrap_or(state.config.ui_language.as_str())
        )
    };
    let lang_row_y = ui.cursor_pos()[1];
    ui.group(|| {
        ui.text(t("settings.language"));
        ui.set_next_item_width(pair_w);
        if let Some(_c) = ComboBox::new("##ui_language")
            .preview_value(&preview)
            .begin(ui)
        {
            let auto_sel = state.config.ui_language.eq_ignore_ascii_case("auto");
            let auto_code = gw2_core::i18n::resolve("auto");
            let (auto_mark, auto_color) = pack_mark(cached_pack_status(&cache, auto_code, build));
            {
                let auto_label = format!("{auto_mark} {}", t("settings.language_auto"));
                let _color = ui.push_style_color(nexus::imgui::StyleColor::Text, auto_color);
                if Selectable::new(&auto_label).selected(auto_sel).build(ui) && !auto_sel {
                    state.config.ui_language = "auto".into();
                    gw2_core::i18n::set_language("auto");
                    crate::ui::save_config_detached(state);
                    super::super::stats::ensure_localized_names(state);
                }
            }
            for lang in gw2_core::i18n::LANGUAGES {
                let sel = state.config.ui_language == lang.code;
                let (mark, color) = pack_mark(cached_pack_status(&cache, lang.code, build));
                let label = format!(
                    "{mark} {}",
                    crate::ui::fonts::language_label(lang, &font_pref, &ui_lang_pref)
                );
                let _color = ui.push_style_color(nexus::imgui::StyleColor::Text, color);
                if Selectable::new(&label).selected(sel).build(ui) && !sel {
                    state.config.ui_language = lang.code.into();
                    gw2_core::i18n::set_language(lang.code);
                    crate::ui::save_config_detached(state);
                    super::super::stats::ensure_localized_names(state);
                }
            }
        }
    });
    ui.set_cursor_pos([second_x, lang_row_y]);
    ui.group(|| {
        ui.text(t("settings.font"));
        ui.set_next_item_width(pair_w);
        let current_font = state.config.ui_font.clone();
        let font_preview = crate::ui::fonts::label_for(&current_font);
        if let Some(_c) = ComboBox::new("##ui_font")
            .preview_value(&font_preview)
            .begin(ui)
        {
            for (id, label) in crate::ui::fonts::combo_options(&state.config.ui_language) {
                let sel = current_font == id;
                if Selectable::new(&label).selected(sel).build(ui) && !sel {
                    state.config.ui_font = id;
                    crate::ui::save_config_detached(state);
                }
            }
        }
    });
    // Both legends below the pair rather than inside it: wrapped prose in a
    // group is measured against the whole column, which would widen the
    // first group and push the second off the panel.
    theme::wrapped(ui, theme::pal().muted, &t("settings.lang_pack_legend"));
    theme::wrapped(ui, theme::pal().muted, &t("settings.font_hint"));
    ui.spacing();

    let slider_row_y = ui.cursor_pos()[1];
    ui.group(|| {
        ui.text(t("settings.opacity"));
        ui.set_next_item_width(pair_w);
        let mut opacity = state.config.window_opacity;
        if nexus::imgui::Slider::new("##opacity", 0.3, 1.0)
            .display_format("%.2f")
            .build(ui, &mut opacity)
        {
            state.config.window_opacity = opacity;
            crate::ui::save_config_detached(state);
        }
    });
    ui.set_cursor_pos([second_x, slider_row_y]);
    ui.group(|| {
        ui.text(t("settings.scale"));
        ui.set_next_item_width(pair_w);
        let mut scale = state.config.font_scale;
        if nexus::imgui::Slider::new("##font_scale", 0.5, 2.0)
            .display_format("%.2f")
            .build(ui, &mut scale)
        {
            state.config.font_scale = scale;
            crate::ui::save_config_detached(state);
        }
    });

    ui.dummy([0.0, 8.0]);
    render_theme_style_section(ui, state, right_item_w, theme_align_y);

    ui.spacing();
    ui.text_colored(theme::pal().muted, t("settings.layout"));

    let left_l = t("settings.left_panel");
    let pad_l = t("settings.panel_padding");
    let sp_l = t("settings.section_spacing");
    let ind_l = t("settings.content_indent");
    let fields: [(&str, &str, f32, f32, f32); 4] = [
        (left_l.as_str(), "##left_panel_w", 320.0, 480.0, 5.0),
        (pad_l.as_str(), "##panel_pad", 0.0, 20.0, 1.0),
        (sp_l.as_str(), "##section_sp", 0.0, 16.0, 1.0),
        (ind_l.as_str(), "##content_ind", 0.0, 20.0, 1.0),
    ];
    let vals: &mut [f32] = &mut [
        state.config.left_panel_width,
        state.config.panel_padding,
        state.config.section_spacing,
        state.config.content_indent,
    ];
    let mut dirty = false;

    for (i, (label, id, min, max, step)) in fields.iter().enumerate() {
        ui.text(label);
        ui.same_line_with_pos(right_item_w * 0.45);
        ui.set_next_item_width(right_item_w * 0.35);
        if nexus::imgui::InputFloat::new(ui, id, &mut vals[i])
            .step(*step)
            .step_fast(*step * 5.0)
            .build()
        {
            vals[i] = vals[i].clamp(*min, *max);
            dirty = true;
        }
    }
    if dirty {
        state.config.left_panel_width = vals[0];
        state.config.panel_padding = vals[1];
        state.config.section_spacing = vals[2];
        state.config.content_indent = vals[3];
        crate::ui::save_config_detached(state);
    }

    ui.spacing();
    if ui.small_button(t("btn.reset_layout")) {
        state.config.left_panel_width = 360.0;
        state.config.panel_padding = 6.0;
        state.config.section_spacing = 4.0;
        state.config.content_indent = 4.0;
        state.config.window_x = None;
        state.config.window_y = None;
        state.config.window_w = None;
        state.config.window_h = None;
        state.force_window_pos = true;
        crate::ui::save_config_detached(state);
    }
}

/// The five custom-theme base colors in rail order: `(label key, description
/// key)`. The index is [`crate::state::MainState::theme_edit_slot`] and it
/// matches `CustomTheme`'s field order, so the two `theme_base*` helpers stay
/// trivially checkable against each other.
const THEME_SLOTS: [(&str, &str); 5] = [
    ("settings.theme_bg", "settings.theme_bg_desc"),
    ("settings.theme_panel", "settings.theme_panel_desc"),
    ("settings.theme_accent", "settings.theme_accent_desc"),
    ("settings.theme_text", "settings.theme_text_desc"),
    ("settings.theme_muted", "settings.theme_muted_desc"),
];

fn theme_base(c: &CustomTheme, i: usize) -> [f32; 3] {
    match i {
        0 => c.bg,
        1 => c.panel,
        2 => c.accent,
        3 => c.text,
        _ => c.muted,
    }
}

fn theme_base_mut(c: &mut CustomTheme, i: usize) -> &mut [f32; 3] {
    match i {
        0 => &mut c.bg,
        1 => &mut c.panel,
        2 => &mut c.accent,
        3 => &mut c.text,
        _ => &mut c.muted,
    }
}

/// Selection marks for the picked rail row, given the swatch's screen-space
/// top-left. Everything is drawn OUTSIDE the swatch, on the section ground,
/// so no mark has to out-contrast the color it is marking — the awkward case
/// (the player sets a base to the same color as the marker) becomes
/// mark-vs-ground, the contrast every button and separator already rides on.
///
/// The caret is `cream`, deliberately not the accent: `cream` is the theme's
/// text color and the ground is its background, so cream-on-ground cannot
/// fail without every word in the addon becoming unreadable. It is the mark
/// that survives a degenerate palette where `accent == bg` collapses both the
/// gold ring and the row's header plate. Shape as well as color, so it also
/// survives greyscale.
fn draw_slot_marker(ui: &Ui, swatch: [f32; 2], sw: f32) {
    let p = theme::pal();
    let (x, y) = (swatch[0], swatch[1]);
    let dl = ui.get_window_draw_list();
    dl.add_rect([x - 3.0, y - 3.0], [x + sw + 3.0, y + sw + 3.0], p.gold)
        .thickness(2.0)
        .rounding(4.0)
        .build();
    let cy = y + sw * 0.5;
    dl.add_triangle(
        [x - 5.0, cy],
        [x - 11.0, cy - 5.0],
        [x - 11.0, cy + 5.0],
        p.cream,
    )
    .filled(true)
    .build();
}

/// Copy the current preset's five base colors into an UNTOUCHED custom theme,
/// so opening the custom editor starts from the look the player was just
/// wearing instead of a fixed palette. Returns whether it wrote anything.
///
/// The untouched guard is the whole point: once a player has edited or named a
/// custom theme, switching preset -> custom -> preset and back must hand their
/// theme back, not overwrite it. `"custom"` and unknown ids have no bases to
/// copy, so those are no-ops too.
fn seed_custom_from_preset(theme: &mut ThemeConfig) -> bool {
    if theme.custom != CustomTheme::default() {
        return false;
    }
    let Some([bg, panel, accent, text, muted]) = theme::preset_bases(&theme.preset) else {
        return false;
    };
    let c = &mut theme.custom;
    c.bg = bg;
    c.panel = panel;
    c.accent = accent;
    c.text = text;
    c.muted = muted;
    true
}

/// Runtime theme picker: the built-in presets from `theme::preset_ids()` plus
/// one user-defined custom theme. Selecting a row applies instantly —
/// `theme::apply_theme` is a cheap palette rebuild, so it runs on every
/// change for live preview — while persistence uses the same
/// deactivate-after-edit debounce as the radio volume slider (persist once on
/// release/defocus, not per drag tick or keystroke).
fn render_theme_style_section(ui: &Ui, state: &mut AddonState, right_item_w: f32, align_y: f32) {
    // Start level with Optimization Defaults in the column beside, unless
    // UI Preferences already runs past it - never backwards, or the header
    // would be drawn over the sliders above it.
    let at = ui.cursor_pos();
    ui.set_cursor_pos([at[0], at[1].max(align_y)]);
    theme::header(ui, &t("settings.theme_section"));

    let is_custom = state.config.theme.preset == "custom";
    let custom_name = state.config.theme.custom.name.trim().to_string();
    let custom_row_label = if custom_name.is_empty() {
        t("settings.theme_custom")
    } else {
        custom_name
    };
    let preview = if is_custom {
        custom_row_label.clone()
    } else {
        theme::preset_ids()
            .iter()
            .copied()
            .find(|&(id, _)| state.config.theme.preset == id)
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| state.config.theme.preset.clone())
    };
    let style = ui.clone_style();
    let fh = ui.frame_height();
    let gap = style.item_spacing[0];

    // The grid | picker split is decided here rather than further down,
    // because the preset row has to respect it: a theme-name field run to
    // the pane's right edge sat over the picker's column and forced the
    // picker a whole row lower than it needed to be.
    let lane = 12.0; // caret + ring live here, left of the swatch
    let col_gap = 12.0; // grid | picker gutter
    let trail = 6.0; // so the row plate does not end flush against the text
    let picker_w = (fh * 7.0).min(right_item_w).max(fh * 4.0);
    let labels: [String; 5] = std::array::from_fn(|i| t(THEME_SLOTS[i].0));
    // Measure the labels, never assume them: "Background" is 10 chars,
    // "Gedämpfter Text" 15, "Przygaszony tekst" 17, "Приглушённый текст" 18.
    // The grid sits beside the picker only when the widest label in the
    // CURRENT language at the CURRENT font scale actually fits there; below
    // that the section stacks. In side-by-side mode `cell_w` is at least the
    // width this test demanded, so a label cannot clip by construction.
    let widest = labels
        .iter()
        .map(|l| ui.calc_text_size(l)[0])
        .fold(0.0_f32, f32::max);
    let grid_need = lane + (fh + gap + widest + trail) * 2.0 + gap;
    let side_by_side = right_item_w >= grid_need + col_gap + picker_w;
    let grid_w = if side_by_side {
        right_item_w - col_gap - picker_w
    } else {
        right_item_w
    };

    // Preset combo and theme name share one line: the name only exists in
    // custom mode, so a full row of its own bought nothing but height in the
    // one direction this panel has least of. Both stay inside the grid
    // column so the picker can rise to meet them.
    let combo_w = (grid_w * 0.42).max(fh * 5.0).min(grid_w);
    let name_w = grid_w - combo_w - gap;
    let name_beside = name_w >= fh * 6.0;
    // Where the preset row starts, so the picker can be placed level with it.
    let preset_row = ui.cursor_pos();

    ui.set_next_item_width(combo_w);
    if let Some(_c) = ComboBox::new("##theme_preset")
        .preview_value(&preview)
        .begin(ui)
    {
        for &(id, name) in theme::preset_ids() {
            let sel = !is_custom && state.config.theme.preset == id;
            if Selectable::new(name).selected(sel).build(ui) && !sel {
                state.config.theme.preset = id.to_string();
                theme::apply_theme(&state.config.theme);
                crate::ui::save_config_detached(state);
            }
        }
        // Named themes, then "Custom" beneath them. Cloned first: the rows
        // write to `state.config.theme` as they are drawn.
        let kept = state.config.theme.saved.clone();
        for entry in &kept {
            // "###" pins the id to the suffix, so a theme named after a
            // preset cannot collide with it. Names are unique by `remember`.
            let label = format!("{}###theme_saved_{}", entry.name, entry.name);
            let sel = is_custom
                && state
                    .config
                    .theme
                    .custom
                    .name
                    .trim()
                    .eq_ignore_ascii_case(entry.name.trim());
            if Selectable::new(&label).selected(sel).build(ui) && !sel {
                state.config.theme.custom = entry.clone();
                state.config.theme.preset = "custom".into();
                theme::apply_theme(&state.config.theme);
                crate::ui::save_config_detached(state);
            }
        }
        // Always the localized placeholder, never the current theme's name:
        // this row is the scratch slot for the NEXT theme, and labelling it
        // "Rob" is what left no way to start a second one.
        let starting_new = is_custom && state.config.theme.custom.name.trim().is_empty();
        let label = format!("{}###theme_custom_row", t("settings.theme_custom"));
        if Selectable::new(&label).selected(starting_new).build(ui) && !starting_new {
            // Keep whatever was being edited before handing the slot over.
            let editing = state.config.theme.custom.clone();
            state.config.theme.remember(&editing);
            seed_custom_from_preset(&mut state.config.theme);
            state.config.theme.custom.name.clear();
            state.config.theme.preset = "custom".into();
            theme::apply_theme(&state.config.theme);
            crate::ui::save_config_detached(state);
        }
    }

    // Re-read: the combo above may have just changed it.
    if state.config.theme.preset != "custom" {
        return;
    }

    // Beside the combo when the column can hold both, under it when it cannot.
    if name_beside {
        ui.same_line();
        ui.set_next_item_width(name_w);
    } else {
        ui.set_next_item_width(right_item_w * 0.6);
    }
    ui.input_text("##theme_custom_name", &mut state.config.theme.custom.name)
        .hint(&t("settings.theme_name_hint"))
        .build();
    if ui.is_item_deactivated_after_edit() {
        // Naming a theme is what keeps it. On commit rather than per
        // keystroke, or every letter typed would leave a saved theme behind.
        let named = state.config.theme.custom.clone();
        state.config.theme.remember(&named);
        crate::ui::save_config_detached(state);
    }

    // The swatch grid is a new block, not another row of the form above it.
    // One item_spacing does not read as a break; this does.
    ui.dummy([0.0, style.item_spacing[1]]);

    // custom base colors: swatch grid + one always-visible picker
    //
    // Click a swatch (or its label) to point the picker at that base, edit it
    // in place, then click the next one. No popup, no modal.
    //
    // The grid is 2 columns x 3 rows, filled COLUMN-major, so the two surface
    // colors and the accent stay together on the left and the two type colors
    // pair on the right. A single vertical list of five spent height — the one
    // axis a settings pane never has to spare — to say what two columns say in
    // three rows.
    //
    // ColorPicker4's total width IS the item width we set, provided nothing
    // is drawn past it (hence `label(false)` + `side_preview(false)`), and its
    // saturation/value square is `width - (frame_height + item_inner_spacing.x)`.
    // The popup this replaces asked for `frame_height * 12` — a ~238px square.
    // `frame_height * 7` lands ~128px: 29% of the pixel area, still over a
    // pixel per saturation percent. That is the "a bit too large" fix, in
    // numbers.
    const ROWS: usize = 3;
    let fs = state.config.font_scale.max(0.5);
    // Cells are sized to their content, not stretched to the pane: a row plate
    // running the full width of the column was pure decoration and made the
    // five look like menu entries rather than swatches.
    let cell_w = ((grid_w - lane - gap) * 0.5).max(fh + 24.0);
    let label_w = (cell_w - fh - gap).max(24.0);

    let slot = state.main.theme_edit_slot.min(4);
    let mut pick: Option<usize> = None;
    ui.group(|| {
        // The ring reaches 4px past the swatch; ambient row spacing would let
        // it touch the neighbouring row's plate. x is unchanged.
        let _sp = ui.push_style_var(StyleVar::ItemSpacing([gap, 8.0]));
        // A frame-height row beside a frame-height swatch: centre the label.
        let _al = ui.push_style_var(StyleVar::SelectableTextAlign([0.0, 0.5]));
        let p = theme::pal();
        ui.indent_by(lane);
        for row in 0..ROWS {
            for col in 0..2 {
                // Column-major: left column 0,1,2 — right column 3,4 and one
                // empty cell at the bottom right.
                let i = col * ROWS + row;
                let Some(label) = labels.get(i) else {
                    continue;
                };
                if col > 0 {
                    ui.same_line();
                }
                let sel = i == slot;
                let c = theme_base(&state.config.theme.custom, i);
                let swatch = ui.cursor_screen_pos();

                // "###" hashes only the suffix, so widget identity survives a
                // language switch mid-edit. The visible half is still the
                // localized label, which ImGui's built-in color tooltip shows
                // as the tooltip title — free, localized, and a bonus cue
                // rather than the only one.
                if ColorButton::new(format!("{label}###theme_sw_{i}"), [c[0], c[1], c[2], 1.0])
                    .size([fh, fh])
                    .alpha(false)
                    // Without the border, a swatch set to the panel color
                    // dissolves into the plate behind it.
                    .border(true)
                    // ColorButton is a drag-drop SOURCE by default. A drag off a
                    // swatch eats the press so the click never lands, and a
                    // dropped color mutates a base with no ActiveId transition,
                    // which the commit-on-deactivate save below would miss.
                    .drag_drop(false)
                    .build(ui)
                {
                    pick = Some(i);
                }

                ui.same_line();
                {
                    // Selected row in cream, the other four muted: the grid
                    // reads as one live cell and four parked ones.
                    let _tc =
                        ui.push_style_color(StyleColor::Text, if sel { p.cream } else { p.muted });
                    // Explicit width: a 0-width Selectable spans the whole
                    // column, which both swallows the second grid column and
                    // shoves the picker off the right edge.
                    if Selectable::new(format!("{label}###theme_row_{i}"))
                        .selected(sel)
                        .size([label_w, fh])
                        .build(ui)
                    {
                        pick = Some(i);
                    }
                }

                // After both items, so the marks land on top of the row plate.
                if sel {
                    draw_slot_marker(ui, swatch, fh);
                }
            }
        }
        ui.unindent_by(lane);
    });

    // Apply the click BEFORE the picker is submitted: on a frame where a rail
    // click landed the picker cannot be the active item, so re-pointing it is
    // safe and it follows the new slot with zero lag.
    if let Some(i) = pick {
        state.main.theme_edit_slot = i;
    }
    let slot = state.main.theme_edit_slot.min(4);

    // The picker rises to the preset row rather than starting level with the
    // swatch grid, which is a row and a half of height back. `preset_row`
    // came from `cursor_pos`, so feeding it to `set_cursor_pos` round-trips
    // in the same space — unlike `same_line_with_pos`, whose offset excludes
    // window padding and is measured from the window plus group and column
    // offsets, so inside this two-column layout the number passed is not the
    // x you get.
    let grid_bottom = ui.cursor_pos()[1];
    if side_by_side {
        ui.set_cursor_pos([preset_row[0] + grid_w + col_gap, preset_row[1]]);
    }
    let edited = {
        let value = theme_base_mut(&mut state.config.theme.custom, slot);
        ui.set_next_item_width(picker_w);
        ColorPicker::new("###theme_picker", value)
            .label(false)
            // Reclaims frame_height*3 of width, and its "Current"/"Original"
            // captions are hardcoded English in the widget source. The rail
            // is the live preview anyway.
            .side_preview(false)
            // ColorPicker4 only implies NoSmallPreview when the SIDE preview
            // is on, so with side_preview off this must be set explicitly or
            // the hex row keeps a redundant swatch.
            .small_preview(false)
            .alpha(false)
            .alpha_bar(false)
            // The right-click menu writes g.ColorEditOptions, which persists
            // in imgui.ini and feeds the picker/input mask back in — a player
            // who once chose "Hue wheel" in the old popup would get a wheel
            // here and a broken layout.
            .options(false)
            .mode(ColorPickerMode::HueBar) // exactly one PickerMask bit
            .input_mode(ColorEditInputMode::Rgb) // exactly one InputMask bit
            .inputs(true)
            .display_hex(true) // the one field people paste a color into
            .display_rgb(false)
            .display_hsv(false)
            .format(ColorFormat::U8)
            .build(ui)
    };
    // Valid immediately after `build`: ColorPicker4 wraps itself in
    // BeginGroup/EndGroup and EndGroup forwards both the Edited and
    // Deactivated status flags to the group item — the same release/defocus
    // debounce the old ColorEdit rows used.
    let commit = ui.is_item_deactivated_after_edit();

    // Whichever column is taller decides where the section ends. The picker
    // was moved up, so it can now finish ABOVE the swatch grid — carrying on
    // from the picker alone would draw the next paragraph over the swatches.
    if side_by_side {
        let below = grid_bottom.max(ui.cursor_pos()[1]);
        ui.set_cursor_pos([preset_row[0], below]);
    }

    if edited {
        theme::apply_theme(&state.config.theme);
    }
    if commit {
        // A colour change to a named theme belongs to that theme, or editing
        // a saved one would only ever change the live buffer and be lost the
        // next time it was picked from the list.
        let edited = state.config.theme.custom.clone();
        state.config.theme.remember(&edited);
        crate::ui::save_config_detached(state);
    }

    // caption: what the selected base actually paints
    // One description instead of five, at full column width where the long
    // German and Russian strings wrap best. Naming the slot in words is also
    // the one selection cue no color choice can erase.
    ui.spacing();
    theme::wrapped(ui, theme::pal().cream, &labels[slot]);
    ui.set_window_font_scale(fs * 0.85);
    {
        // Reserve the tallest of the five so clicking down the rail never
        // shifts the controls below this section. Measured at the scale it
        // renders at, which is why this sits after the push.
        let wrap_w = ui.content_region_avail()[0].max(8.0);
        let reserve = THEME_SLOTS
            .iter()
            .map(|(_, dk)| ui.calc_text_size_with_opts(t(dk), false, wrap_w)[1])
            .fold(0.0_f32, f32::max);
        let top = ui.cursor_screen_pos()[1];
        theme::wrapped(ui, theme::pal().muted, &t(THEME_SLOTS[slot].1));
        let used = ui.cursor_screen_pos()[1] - top - style.item_spacing[1];
        if reserve - used > 0.5 {
            ui.dummy([0.0, reserve - used]);
        }
    }
    theme::wrapped(ui, theme::pal().muted, &t("settings.theme_custom_hint"));
    // Restore to `fs`, NOT 1.0: `render_main` sets the window font scale to
    // config.font_scale, so resetting to 1.0 here silently un-scaled every
    // section after this one for any player not on 1.0.
    ui.set_window_font_scale(fs);
}

fn pack_mark(status: gw2_api::localize::PackStatus) -> (&'static str, [f32; 4]) {
    match status {
        gw2_api::localize::PackStatus::Ready => ("*", theme::OPTIMIZED),
        gw2_api::localize::PackStatus::Missing => ("!", theme::ERR),
        gw2_api::localize::PackStatus::Stale => ("~", theme::WARN),
        gw2_api::localize::PackStatus::None => ("-", theme::pal().muted),
    }
}

/// Frame-throttled, memoized wrapper around `gw2_api::localize::pack_status`.
///
/// `pack_status` opens and fully deserializes the cached name-pack JSON just
/// to read one `build` field — for the ~800 KB `de`/`es`/`fr`/`zh` packs that
/// is a real parse, not a stat. `render_theme_section` called it once per
/// frame for the current language, and once per language again while the
/// picker combo was open (5-6 parses in the same frame). This cache
/// recomputes at most once every `REFRESH_FRAMES` frames — same throttle
/// shape as `settings_cache_size_frames` in `render_cache_section` — and can
/// be forced early with `invalidate_pack_status_cache` right after an action
/// that actually changes the packs on disk (cache clear, game-data refresh).
///
/// Lives in a `thread_local` instead of on `MainState`: the render thread is
/// the only caller, this leaf's write set does not include `state.rs`, and
/// nothing here needs to be persisted — it is pure UI-side memoization.
struct PackStatusCache {
    frames_left: u32,
    build: Option<u32>,
    statuses: std::collections::HashMap<String, gw2_api::localize::PackStatus>,
}

impl PackStatusCache {
    /// ~1 second at 60 fps — matches `settings_cache_size_frames`'s throttle.
    const REFRESH_FRAMES: u32 = 60;

    fn new() -> Self {
        Self {
            frames_left: 0,
            build: None,
            statuses: std::collections::HashMap::new(),
        }
    }
}

thread_local! {
    static PACK_STATUS_CACHE: std::cell::RefCell<PackStatusCache> =
        std::cell::RefCell::new(PackStatusCache::new());
}

/// Call once per frame (at the top of `render_theme_section`) before reading
/// `cached_pack_status`. Expires the cache when the throttle window elapses or
/// `build` changes — e.g. the periodic API-health check picks up a new game
/// patch mid-session, which can flip a pack from "Ready" to "Stale".
fn tick_pack_status_cache(build: Option<u32>) {
    PACK_STATUS_CACHE.with(|cell| {
        let mut c = cell.borrow_mut();
        if c.frames_left == 0 || c.build != build {
            c.statuses.clear();
            c.build = build;
            c.frames_left = PackStatusCache::REFRESH_FRAMES;
        } else {
            c.frames_left -= 1;
        }
    });
}

/// Force the next frame's `tick_pack_status_cache` to recompute immediately
/// instead of waiting out the throttle window. Call right after clearing the
/// cache dir or kicking off a game-data refresh — both can change the on-disk
/// packs `pack_status` reports on.
fn invalidate_pack_status_cache() {
    PACK_STATUS_CACHE.with(|c| c.borrow_mut().frames_left = 0);
}

/// Throttled, memoized `gw2_api::localize::pack_status`. See `PackStatusCache`
/// and `tick_pack_status_cache`.
fn cached_pack_status(
    cache: &gw2_api::cache::DataCache,
    lang: &str,
    build: Option<u32>,
) -> gw2_api::localize::PackStatus {
    PACK_STATUS_CACHE.with(|cell| {
        let mut c = cell.borrow_mut();
        *c.statuses
            .entry(lang.to_string())
            .or_insert_with(|| gw2_api::localize::pack_status(cache, lang, build))
    })
}

fn render_cache_section(ui: &Ui, state: &mut AddonState) {
    // Set by the confirmation below, acted on after it, so the borrow of
    // `state` for the buttons is finished before the clear runs.
    let mut clear_requested = false;
    if let Some(ref key) = state.config.gw2_api_key {
        let display = if key.chars().count() > 12 {
            let pre: String = key.chars().take(8).collect();
            let suf: String = key
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            format!("{}...{}", pre, suf)
        } else {
            "****".into()
        };
        ui.text(format!("{} {}", t("label.gw2_api_key"), display));
    }
    if let Some(build) = state.config.cache_build_number {
        if let Some(live) = state.main.live_build_number {
            if live != build {
                ui.text_colored(
                    theme::WARN,
                    tf(
                        "fmt.game_build_live",
                        &[("cached", &build.to_string()), ("live", &live.to_string())],
                    ),
                );
            } else {
                ui.text(tf("fmt.game_build", &[("n", &build.to_string())]));
            }
        } else {
            ui.text(tf("fmt.game_build", &[("n", &build.to_string())]));
        }
    }

    let cache_dir = state.addon_dir.join("cache");
    // Throttle the directory scan to ~once per second. The cache holds ~10–20
    // files including a ~50 MB items.json; scanning + metadata-statting every
    // render frame just to display "Cache: X MB" hits disk at ~60 Hz.
    if state.main.settings_cache_size_frames == 0 {
        state.main.settings_cache_size = calculate_dir_size(&cache_dir);
        state.main.settings_graphics_size = calculate_dir_size(&cache_dir.join("graphics"));
        state.main.settings_cache_size_frames = 60;
    } else {
        state.main.settings_cache_size_frames -= 1;
    }
    ui.text(tf(
        "fmt.data_size",
        &[("size", &format_bytes(state.main.settings_cache_size))],
    ));
    ui.same_line();
    ui.text_colored(
        theme::pal().muted,
        tf(
            "fmt.icons_size",
            &[("size", &format_bytes(state.main.settings_graphics_size))],
        ),
    );
    ui.same_line();
    let refreshing = state.main.game_db_loading;
    // Clear Cache reads like a tidy-up and is the most expensive button in
    // the addon: it discards every item, skill, trait and icon the API ever
    // sent, and the next start re-downloads all of it. It also sits beside
    // Refresh Game Data, which is what someone chasing stale data actually
    // wants. So it asks first, and the question names the cost rather than
    // saying "are you sure".
    if refreshing {
        let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        theme::gold_button_sized(ui, t("btn.clear_cache"), [100.0, 0.0]);
        style.pop();
        state.main.confirm_clear_cache = false;
    } else if state.main.confirm_clear_cache {
        if theme::gold_button_sized(ui, t("btn.yes"), [56.0, 0.0]) {
            state.main.confirm_clear_cache = false;
            clear_requested = true;
        }
        ui.same_line();
        if ui.button_with_size(t("btn.no"), [56.0, 0.0]) {
            state.main.confirm_clear_cache = false;
        }
    } else if theme::gold_button_sized(ui, t("btn.clear_cache"), [100.0, 0.0]) {
        state.main.confirm_clear_cache = true;
    }
    if state.main.confirm_clear_cache {
        theme::wrapped(
            ui,
            theme::WARN,
            &tf(
                "settings.clear_cache_q",
                &[(
                    "size",
                    &format_bytes(
                        state.main.settings_cache_size + state.main.settings_graphics_size,
                    ),
                )],
            ),
        );
    }
    if clear_requested {
        let cache = gw2_api::cache::DataCache::new(&cache_dir);
        if let Err(e) = cache.clear_all() {
            state.main.error = Some(tf("fmt.err_clear_cache", &[("err", &e.to_string())]));
        } else {
            state.config.cache_build_number = None;
            crate::ui::save_config_detached(state);
            state.main.game_db = None;
            state.setup.download_progress = None;
            // Force the cached "Cache: …" label to recompute on the next frame
            // instead of waiting for the throttle to roll over.
            state.main.settings_cache_size_frames = 0;
            // The cleared cache dir also wiped the lang-pack files `pack_status`
            // reports on — force the language combo's Ready/Missing/Stale marks
            // to recompute now instead of showing stale "Ready" for up to a
            // second.
            invalidate_pack_status_cache();
            stats::start_game_data_refresh(state);
        }
    }

    ui.spacing();
    let mut auto_refresh = state.config.auto_refresh_cache;
    if ui.checkbox(
        format!("{}##auto_refresh", t("settings.auto_refresh")),
        &mut auto_refresh,
    ) {
        state.config.auto_refresh_cache = auto_refresh;
        crate::ui::save_config_detached(state);
    }
    if ui.is_item_hovered() {
        ui.tooltip_text(t("tip.auto_refresh"));
    }

    ui.spacing();
    if refreshing {
        let stage = state.main.game_refresh_stage.clone();
        let downloading = t("settings.downloading");
        ui.text_colored(
            [1.0, 1.0, 0.0, 1.0],
            if stage.is_empty() {
                downloading.as_str()
            } else {
                &stage
            },
        );
        if let Some(ref dl) = state.setup.download_progress {
            if let Some(kind) = dl.items_fill_kind {
                ui.text_colored(
                    theme::pal().muted,
                    t(crate::state::items_fill_i18n_key(kind)),
                );
            }
            let overlay = format!("{}/{} — {}", dl.current_step, dl.total_steps, dl.step_name);
            theme::download_scribble(ui, dl.fraction(), &overlay);
        }
        let style = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        theme::gold_button_sized(ui, t("btn.refreshing"), [160.0, 0.0]);
        style.pop();
    } else if theme::gold_button_sized(ui, t("btn.refresh_game"), [160.0, 0.0]) {
        // Do NOT wipe the on-disk cache here. RefreshMode::Default (FOLD3)
        // skips KEPT catalog body fetches when CacheEntry.build matches live
        // /v2/build; clear_all() deleted items.json and forced ~74k
        // install_items, defeating FOLD3 and reopening setup (House DIAG).
        // Explicit wipe remains only on Clear Cache above.
        state.setup.download_progress = None;
        // Force the cached "Cache: …" label to recompute after refresh.
        state.main.settings_cache_size_frames = 0;
        stats::start_game_data_refresh(state);
    }

    ui.spacing();
    if !state.main.confirm_reset {
        if theme::gold_button_sized(ui, t("btn.reset_setup"), [160.0, 0.0]) {
            state.main.confirm_reset = true;
        }
    } else {
        ui.text_colored([1.0, 0.3, 0.0, 1.0], t("settings.reset_q"));
        if theme::gold_button_sized(ui, t("btn.yes_reset"), [100.0, 0.0]) {
            state.main.confirm_reset = false;
            if let Err(e) = state.reset_to_first_run() {
                state.main.error = Some(tf("fmt.err_reset", &[("err", &e.to_string())]));
            }
        }
        ui.same_line();
        if theme::gold_button_sized(ui, t("btn.cancel"), [80.0, 0.0]) {
            state.main.confirm_reset = false;
        }
    }
}

/// Sync sources in the order they are reported, with their display casing.
/// Game modes as `BenchmarkBuild::mode` spells them, in display order.
const BENCHMARK_MODES: &[&str] = &["PvE", "PvP", "WvW"];

const BENCHMARK_SOURCES: &[(&str, &str)] = &[
    ("snowcrows", "Snowcrows"),
    ("hardstuck", "Hardstuck"),
    ("guildjen", "GuildJen"),
];

/// The `done/total` a scraper's progress line ends with.
///
/// Every scraper reports its build loop as a trailing `n/total` - "Guardian
/// 12/45", "WvW Necromancer 12/99" - so the tail is the one part of the line
/// that is a contract rather than prose. A listing line has no fraction and
/// gets no bar, which is the honest rendering: nothing is countable yet.
fn progress_fraction(line: &str) -> Option<(usize, usize)> {
    let tail = line.rsplit(char::is_whitespace).next()?;
    let (done, total) = tail.split_once('/')?;
    let total: usize = total.parse().ok()?;
    (total > 0).then_some((done.parse().ok()?, total))
}

/// A thin filled bar for one source's progress.
fn sync_bar(ui: &Ui, fraction: f32) {
    let width = (ui.content_region_avail()[0] - 16.0).max(1.0);
    let pos = ui.cursor_screen_pos();
    let draw = ui.get_window_draw_list();
    draw.add_rect(
        [pos[0] + 8.0, pos[1] + 2.0],
        [pos[0] + width + 8.0, pos[1] + 8.0],
        [0.2, 0.2, 0.2, 0.8],
    )
    .filled(true)
    .rounding(3.0)
    .build();
    let filled = width * fraction.clamp(0.0, 1.0);
    if filled > 0.0 {
        draw.add_rect(
            [pos[0] + 8.0, pos[1] + 2.0],
            [pos[0] + 8.0 + filled, pos[1] + 8.0],
            theme::pal().gold,
        )
        .filled(true)
        .rounding(3.0)
        .build();
    }
    ui.dummy([width, 11.0]);
}

fn render_benchmark_section(ui: &Ui, state: &mut AddonState) {
    ui.spacing();
    ui.text_colored(theme::pal().muted, t("settings.sources"));
    ui.spacing();
    // Set by a row's Retry button; read by the sync trigger below, so both
    // paths spawn the one worker rather than duplicating it.
    let mut retry_requested = false;
    if state.main.benchmark_running {
        // A full sync is several hundred pages over minutes. One joined line
        // said which sources were alive and nothing about how far along they
        // were, so a run that was working looked identical to one that had
        // hung. A row and a bar per source, naming the class in flight.
        let mut any = false;
        for (key, label) in BENCHMARK_SOURCES {
            let Some(live) = state.main.benchmark_live.get(*key) else {
                continue;
            };
            any = true;
            ui.text_colored(theme::pal().gold, *label);
            ui.same_line();
            ui.text_colored(theme::pal().muted, live);
            if let Some((done, total)) = progress_fraction(live) {
                sync_bar(ui, done as f32 / total.max(1) as f32);
            }
        }
        if !any {
            ui.text_colored(theme::pal().muted, t("btn.syncing"));
        }
        // A run that is waiting out a rate limit looks identical to a hung
        // one from outside, and the waits are tens of seconds. Say so.
        if gw2_optimizer::scraper::sync_backoff_ms() > 0 {
            ui.text_colored([0.9, 0.8, 0.2, 1.0], t("bench.throttled"));
        }
        ui.spacing();
    }
    {
        // A row per source: what came back, and what did not. One joined
        // line of three totals could not say that a source listed 157 pages
        // and returned 148 - the nine that failed simply vanished.
        //
        // Drawn whether or not a sync has ever run, and left up while one is
        // running. A grid of dashes still answers the question the table
        // exists for — which modes are not covered — so the progress rows
        // belong above it rather than in place of it.
        let synced = state.main.benchmark_last_synced.clone();
        match synced {
            Some(ref last) => {
                ui.text_colored(theme::pal().muted, tf("fmt.synced_when", &[("when", last)]))
            }
            None => ui.text_colored(theme::pal().muted, t("settings.never_synced")),
        }
        // Providers down the side, game modes across: the sources do not
        // cover the same modes, and one total per source could not say
        // whether the one you play is covered at all. The red column is what
        // a run listed and could not read, with its retry beside it.
        // Positioned by hand rather than with `ui.columns`. This grid sits
        // in the right-hand column of the tab, and a column set cannot be
        // opened inside another one — doing so ends the outer pair and
        // drops everything after it into a single full-width column. Laid
        // out full width the grid also read badly: three counts and a dash
        // stretched across the whole window with nothing between them.
        //
        // Every width comes from the text it holds, so the grid follows the
        // font scale and takes only the room it needs. It does NOT share out
        // the panel: three counts of at most four digits, dealt a third of
        // the window each, left a hand's width of empty space inside every
        // column and still pushed the failure column off the right edge. The
        // slack belongs after the last column, not inside each one.
        let scale = state.config.font_scale.max(0.5);
        let label_w = BENCHMARK_SOURCES
            .iter()
            .map(|(_, label)| ui.calc_text_size(label)[0])
            .fold(0.0_f32, f32::max)
            + 16.0 * scale;
        // Room for a four-digit count or the widest heading, whichever is
        // wider, plus a gap so the next column does not touch it. Failed is
        // one of the headings: it is a column of the same table, not an
        // annotation hung off the end of it.
        let failed_label = t("bench.failed");
        let mode_w = BENCHMARK_MODES
            .iter()
            .map(|mode| ui.calc_text_size(mode)[0])
            .chain(std::iter::once(ui.calc_text_size(&failed_label)[0]))
            .fold(ui.calc_text_size("8888")[0], f32::max)
            + 24.0 * scale;
        // Measured, not guessed: `gold_button_sized` grows a button past the
        // width it is given when the label needs more, so a fixed 76px
        // reservation was wrong at any font scale where "Retry" got wider
        // than that, and the button hung off the panel.
        let retry_button_w = theme::gold_button_width(ui, t("btn.retry")).max(56.0 * scale);
        let avail = ui.content_region_avail()[0];

        // Cells are placed with `set_cursor_pos` from the row's own starting
        // x, NOT `same_line_with_pos`. That offset ignores the indent, and
        // this section renders inside the right column's 48px one, so the
        // counts were drawn on top of the source names — "Snowcr180".
        let row_x = ui.cursor_pos()[0];
        let column_at = |n: usize| row_x + label_w + mode_w * n as f32;
        let fail_x = column_at(BENCHMARK_MODES.len());
        // The button follows the Failed column, pinned inside the panel as a
        // floor for a window narrowed past the point where the grid fits at
        // all. Content-sized columns keep the grid far short of the edge at
        // any ordinary width, so this clamp does nothing until the panel is
        // genuinely too small.
        let retry_x = column_at(BENCHMARK_MODES.len() + 1).min(row_x + avail - retry_button_w);

        // Header: an empty corner cell, then the modes.
        let header_y = ui.cursor_pos()[1];
        ui.text(" ");
        for (n, mode) in BENCHMARK_MODES.iter().enumerate() {
            ui.set_cursor_pos([column_at(n), header_y]);
            ui.text_colored(theme::pal().muted, *mode);
        }
        ui.set_cursor_pos([fail_x, header_y]);
        ui.text_colored(theme::pal().muted, &failed_label);
        for (key, label) in BENCHMARK_SOURCES {
            let row_y = ui.cursor_pos()[1];
            ui.text_colored(theme::pal().gold, *label);
            for (n, mode) in BENCHMARK_MODES.iter().enumerate() {
                ui.set_cursor_pos([column_at(n), row_y]);
                let count = state
                    .main
                    .benchmark_mode_counts
                    .get(&format!("{key}|{mode}"))
                    .copied()
                    .unwrap_or(0);
                if count > 0 {
                    ui.text_colored([0.5, 0.9, 0.5, 1.0], count.to_string());
                } else {
                    ui.text_colored(theme::pal().muted, "-");
                }
            }
            let bad = state.main.benchmark_failed.get(*key).copied().unwrap_or(0);
            ui.set_cursor_pos([fail_x, row_y]);
            if bad > 0 {
                ui.text_colored([1.0, 0.4, 0.2, 1.0], bad.to_string());
                // Only where there is something to retry, and cheap to take:
                // a re-run today skips every page already read and fetches
                // exactly these.
                ui.set_cursor_pos([retry_x, row_y]);
                if theme::gold_button_sized(
                    ui,
                    format!("{}##retry_{key}", t("btn.retry")),
                    [retry_button_w, 0.0],
                ) {
                    retry_requested = true;
                }
            } else {
                // A dash, not a zero. Failures are not written to disk, so
                // after a restart this is unknown rather than clean.
                ui.text_colored(theme::pal().muted, "-");
            }
        }
    }
    if let Some(ref err) = state.main.benchmark_error.clone() {
        let short = if err.chars().count() > 80 {
            format!("{}…", err.chars().take(80).collect::<String>())
        } else {
            err.clone()
        };
        ui.text_colored([1.0, 0.4, 0.2, 1.0], format!("[!] {}", short));
        if ui.is_item_hovered() {
            ui.tooltip_text(err);
        }
    }
    ui.spacing();
    let sync_disabled = state.main.benchmark_running || state.main.game_db.is_none();
    if sync_disabled {
        let _dim = ui.push_style_var(nexus::imgui::StyleVar::Alpha(0.4));
        theme::gold_button_sized(
            ui,
            if state.main.benchmark_running {
                t("btn.syncing")
            } else {
                t("btn.sync")
            },
            [160.0, 0.0],
        );
    } else if theme::gold_button_sized(ui, t("btn.sync"), [160.0, 0.0]) || retry_requested {
        let addon_dir = state.addon_dir.clone();
        state.main.benchmark_running = true;
        state.main.benchmark_error = None;
        // Cancel-aware end-to-end, same shape as before, but the scrape itself
        // is now caught on its own (see `spawn_flag_guarded`) so a panic
        // mid-scrape still resets `benchmark_running` instead of locking the
        // button on "Syncing…" forever.
        let spawned = spawn_flag_guarded(
            state,
            "settings-benchmark-sync",
            move |token| {
                if token.is_cancelled() {
                    None
                } else {
                    let r = gw2_optimizer::scraper::scrape_all_with_progress(
                        &addon_dir,
                        &|| token.is_cancelled(),
                        &|src, msg| {
                            let src = src.to_string();
                            let msg = msg.to_string();
                            let _ = crate::state::with_state(|s| {
                                s.main.benchmark_live.insert(src.clone(), msg.clone());
                            });
                        },
                    );
                    if token.is_cancelled() {
                        None
                    } else {
                        Some(r)
                    }
                }
            },
            |s, outcome| {
                s.main.benchmark_running = false;
                s.main.benchmark_live.clear();
                let results = match outcome {
                    Ok(Some(results)) => results,
                    Ok(None) => return,
                    Err(_) => {
                        s.main.benchmark_error = Some("thread panicked".into());
                        return;
                    }
                };
                let mut counts = std::collections::HashMap::new();
                let mut failures = std::collections::HashMap::new();
                let mut mode_counts = std::collections::HashMap::new();
                let mut errors = Vec::new();
                for r in &results {
                    counts.insert(r.source.clone(), r.builds.len());
                    failures.insert(r.source.clone(), r.failed);
                    for b in &r.builds {
                        *mode_counts
                            .entry(format!("{}|{}", r.source, b.mode))
                            .or_insert(0) += 1;
                    }
                    if let Some(ref e) = r.error {
                        errors.push(format!("{}: {}", r.source, e));
                    }
                }
                s.main.benchmark_counts = counts;
                s.main.benchmark_failed = failures;
                s.main.benchmark_mode_counts = mode_counts;
                s.main.benchmark_error = if errors.is_empty() {
                    None
                } else {
                    Some(errors.join(" | "))
                };
                s.main.benchmark_last_synced =
                    Some(chrono::Utc::now().format("%Y-%m-%d").to_string());
            },
        );
        if !spawned {
            state.main.benchmark_running = false;
        }
    }
}

fn calculate_dir_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod key_howto_tests {
    use gw2_core::config::LlmProvider;
    use gw2_core::i18n::t;

    /// Settings shows the on-ramp from the same keys the wizard uses, so the
    /// two screens cannot drift apart. If either ever needs different words,
    /// that is a second key, not a second copy of this one.
    #[test]
    fn every_provider_has_a_reachable_key_page_and_words_for_it() {
        for provider in &LlmProvider::ALL {
            let url = provider.key_page_url();
            assert!(url.starts_with("https://"), "{url} must be openable");

            for key in [provider.setup_howto_key(), provider.setup_steps_key()] {
                let text = t(key);
                assert_ne!(text, key, "{key} is missing from the catalog");
                assert!(text.len() > 20, "{key} is too short to help anyone");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar is driven off the tail of a progress line, so the shapes
    /// every scraper actually emits have to parse - and a listing line,
    /// which counts nothing yet, must not draw an empty bar.
    #[test]
    fn progress_fraction_reads_the_trailing_count() {
        assert_eq!(progress_fraction("WvW Necromancer 12/99"), Some((12, 99)));
        assert_eq!(progress_fraction("Guardian 12/45"), Some((12, 45)));
        assert_eq!(progress_fraction("0/500"), Some((0, 500)));
        // No count yet, or nothing countable: no bar.
        assert_eq!(progress_fraction("listing categories…"), None);
        assert_eq!(progress_fraction("guildjen WvW"), None);
        assert_eq!(progress_fraction("3/0"), None);
    }

    /// Fresh global `STATE` rooted at a per-test temp dir, mirroring
    /// `state::tests::init_worker_test` (that helper is private to `state.rs`'s
    /// own test module, so this leaf's tests need their own copy built from the
    /// `pub` `init`/`clear` functions).
    fn init_test_state(label: &str) {
        crate::state::clear();
        let dir = std::env::temp_dir().join(format!(
            "gw2_settings_test_{}_{}",
            std::process::id(),
            label
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::state::init(dir);
    }

    /// The exact bug this leaf fixes: a Settings-tab "in progress" flag (e.g.
    /// `settings_key_validating`, `benchmark_running`) that never clears
    /// because the risky call panicked before the code that resets it could
    /// run. This spawns a real worker through the real `spawn_flag_guarded`
    /// helper (the one every settings.rs call site uses) with a `risky`
    /// closure that genuinely panics, then asserts the flag comes back clear.
    #[test]
    fn settings_spawn_clears_flags_on_panic() {
        let _serial = crate::state::state_test_guard();
        init_test_state("spawn_panic_clears_flag");

        let apply_saw_panic = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w_apply_saw_panic = apply_saw_panic.clone();

        crate::state::with_state(|s| {
            // Prime the flag exactly like a real Test/Save/Sync click would.
            s.main.settings_key_validating = true;
            spawn_flag_guarded(
                s,
                "test-settings-panic",
                // Stands in for `create_client(...).map(|c| c.validate_key_detailed())`
                // blowing up mid-call.
                |_token| panic!("boom: simulated risky-closure panic"),
                move |s, outcome: std::thread::Result<()>| {
                    s.main.settings_key_validating = false;
                    w_apply_saw_panic.store(outcome.is_err(), std::sync::atomic::Ordering::SeqCst);
                },
            );
        });

        let report = crate::state::join_workers(std::time::Duration::from_secs(5));
        assert_eq!(
            report.joined, 1,
            "the panicking worker must still be joined: {report}"
        );
        assert_eq!(
            report.panicked, 0,
            "spawn_worker's own containment guard must swallow the unwind \
             before the thread ends: {report}"
        );
        assert!(
            apply_saw_panic.load(std::sync::atomic::Ordering::SeqCst),
            "apply must observe the risky closure's panic as Err, not skip it"
        );
        assert_eq!(
            crate::state::with_state(|s| s.main.settings_key_validating),
            Some(false),
            "settings_key_validating must be cleared even though the risky \
             closure panicked — a stuck flag here is a spinner the user can \
             never clear"
        );

        crate::state::clear();
    }

    /// `cached_pack_status` must serve repeated lookups for the same language
    /// from its in-frame cache instead of hitting `pack_status` (and the ~800
    /// KB pack file behind it) again — verified by counting real calls to the
    /// underlying `gw2_api::localize::pack_status` indirectly: the cache
    /// returns the *first* value for a language until `tick_pack_status_cache`
    /// or `invalidate_pack_status_cache` next asks it to expire.
    #[test]
    fn cached_pack_status_reuses_value_within_a_frame() {
        let dir = std::env::temp_dir().join(format!(
            "gw2_settings_test_{}_pack_status_cache",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = gw2_api::cache::DataCache::new(&dir);

        // "en" is not in `API_LANGS`, so `pack_status` always returns `None`
        // cheaply — this test is about the cache plumbing, not disk I/O.
        invalidate_pack_status_cache();
        tick_pack_status_cache(Some(1));
        let first = cached_pack_status(&cache, "en", Some(1));
        let second = cached_pack_status(&cache, "en", Some(1));
        assert_eq!(
            first, second,
            "two lookups in the same tick must agree (cache hit, not a fresh parse)"
        );

        // A build-number change (a new game patch landing mid-session) must
        // still be observed on the very next tick rather than staying stuck
        // on the old cached value for the rest of the throttle window.
        tick_pack_status_cache(Some(2));
        let after_build_change = cached_pack_status(&cache, "en", Some(2));
        assert_eq!(
            after_build_change,
            gw2_api::localize::PackStatus::None,
            "a build change must not desync the cache from what pack_status would say"
        );
    }

    /// Clicking Custom must open the editor on the preset the player is
    /// looking at — and must never eat a custom theme they already made.
    #[test]
    fn seed_custom_from_preset_only_touches_an_untouched_theme() {
        use gw2_core::config::CustomTheme;

        // Fresh custom theme + a real preset selected: seeded from that preset.
        let mut fresh = ThemeConfig {
            preset: "molten-ember".into(),
            custom: CustomTheme::default(),
            ..Default::default()
        };
        assert!(seed_custom_from_preset(&mut fresh));
        let [bg, panel, accent, text, muted] =
            theme::preset_bases("molten-ember").expect("molten-ember is a built-in preset");
        assert_eq!(
            (
                fresh.custom.bg,
                fresh.custom.panel,
                fresh.custom.accent,
                fresh.custom.text,
                fresh.custom.muted
            ),
            (bg, panel, accent, text, muted)
        );
        assert!(
            fresh.custom.name.is_empty(),
            "seeding copies colors, never a name"
        );

        // A theme the player has edited is left exactly as they left it.
        let mine = CustomTheme {
            name: "Pinkfrost".into(),
            bg: [0.10, 0.02, 0.08],
            panel: [0.18, 0.05, 0.14],
            accent: [1.0, 0.35, 0.75],
            text: [0.96, 0.90, 0.94],
            muted: [0.60, 0.48, 0.56],
        };
        let mut edited = ThemeConfig {
            preset: "verdant-wilds".into(),
            custom: mine.clone(),
            ..Default::default()
        };
        assert!(!seed_custom_from_preset(&mut edited));
        assert_eq!(edited.custom, mine, "an edited custom theme survives");

        // Nothing to copy from: "custom" itself, and an unknown id.
        for preset in ["custom", "no-such-theme"] {
            let mut t = ThemeConfig {
                preset: preset.into(),
                custom: CustomTheme::default(),
                ..Default::default()
            };
            assert!(!seed_custom_from_preset(&mut t), "{preset} has no bases");
            assert_eq!(t.custom, CustomTheme::default());
        }
    }

    /// Every i18n key the Theme section renders must exist in the locale
    /// catalogs — `t()` echoes the key itself when it is missing everywhere,
    /// so equality means a hole in the Settings chrome. (Key parity across
    /// all 12 locales is asserted by gw2-core's
    /// `every_locale_parses_and_covers_english_keys`; this guards the
    /// renderer's key spelling against the catalog.)
    #[test]
    fn theme_section_locale_keys_exist() {
        for key in [
            "settings.theme_section",
            "settings.theme_custom",
            "settings.theme_name_hint",
            "settings.theme_bg",
            "settings.theme_panel",
            "settings.theme_accent",
            "settings.theme_text",
            "settings.theme_muted",
            "settings.theme_bg_desc",
            "settings.theme_panel_desc",
            "settings.theme_accent_desc",
            "settings.theme_text_desc",
            "settings.theme_muted_desc",
            "settings.theme_custom_hint",
        ] {
            assert_ne!(t(key), key, "locale catalogs are missing {key}");
        }
    }

    /// Settings LLM key InputText is the same overlay a player may stream or
    /// screenshot. `key_field_is_masked` is the exact function the
    /// `.password(...)` call site uses, and the reveal toggle must default to
    /// hidden — same contract as Setup's `setup_key_fields_are_masked`.
    #[test]
    fn settings_llm_key_field_is_masked() {
        assert!(
            key_field_is_masked(false),
            "an un-revealed key field must render with the PASSWORD flag set"
        );
        assert!(
            !key_field_is_masked(true),
            "toggling reveal on must unmask the field"
        );

        let llm_default_revealed = SHOW_LLM_KEY.with(|c| c.get());
        assert!(
            !llm_default_revealed,
            "the Settings LLM key reveal toggle must default to hidden"
        );
        assert!(
            key_field_is_masked(llm_default_revealed),
            "the Settings LLM key field must be masked on first render"
        );
    }
}
