//! GW2 API use on this screen is allowlisted I/O: API-key validate and
//! `download_game_and_names`. Plated combat and flow are not measured here.

use nexus::imgui::Ui;

use crate::state::{self, AddonState, DownloadState, KeyStatus, Screen, SetupStep};
use crate::ui::theme;
use gw2_core::config::LlmProvider;
use gw2_core::i18n::{t, tf};

thread_local! {
    /// Per-field "reveal password" toggle for the setup wizard's API-key
    /// inputs (GW2 key step, LLM key step). Transient UI-only state scoped to
    /// the render thread — it must not survive a save/reload and does not
    /// belong on `AddonState`, so it lives here rather than on `SetupState`.
    static SHOW_GW2_KEY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SHOW_LLM_KEY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether an API-key input field should mask its contents (ImGui's
/// `InputTextFlags::PASSWORD`), given whether the user has toggled "reveal"
/// on for that field. The GW2 and LLM key steps are the same screen a player
/// might be streaming or screenshotting while pasting a live key, so both
/// default to masked.
fn key_field_is_masked(revealed: bool) -> bool {
    !revealed
}

const SETUP_NAV_BTN_W: f32 = 120.0;
const SETUP_NAV_GAP: f32 = 8.0;

/// One centered horizontal nav row: Back then primary CTA (`same_line`, gap 8).
/// Buttons are at least `SETUP_NAV_BTN_W` wide and grow to fit the label so
/// "Get Started >>" centers correctly. Never stacks Back under the CTA.
fn setup_nav_row(ui: &Ui, back: Option<&str>, next: Option<&str>) -> (bool, bool) {
    let n = u32::from(back.is_some()) + u32::from(next.is_some());
    if n == 0 {
        return (false, false);
    }
    let back_w = back
        .map(|l| theme::gold_button_width(ui, l).max(SETUP_NAV_BTN_W))
        .unwrap_or(0.0);
    let next_w = next
        .map(|l| theme::gold_button_width(ui, l).max(SETUP_NAV_BTN_W))
        .unwrap_or(0.0);
    let row_w = back_w + next_w + if n == 2 { SETUP_NAV_GAP } else { 0.0 };
    let avail = ui.content_region_avail()[0];
    let pad = ((avail - row_w) / 2.0).max(0.0);
    let at = ui.cursor_pos();
    ui.set_cursor_pos([at[0] + pad, at[1]]);

    let mut back_clicked = false;
    let mut next_clicked = false;
    if let Some(label) = back {
        back_clicked = theme::gold_button_sized(ui, label, [back_w, 0.0]);
    }
    if let Some(label) = next {
        if back.is_some() {
            ui.same_line_with_spacing(0.0, SETUP_NAV_GAP);
        }
        next_clicked = theme::gold_button_sized(ui, label, [next_w, 0.0]);
    }
    (back_clicked, next_clicked)
}

pub fn render_setup(ui: &Ui, state: &mut AddonState, step: SetupStep) {
    theme::header(ui, &t("setup.title"));

    let s_lang = t("setup.step_lang");
    let s_gw2 = t("setup.step_gw2");
    let s_ai = t("setup.step_ai");
    let s_data = t("setup.step_data");
    // Complete is not a wizard step — DataDownload Get Started goes straight to Main.
    let steps = [
        (SetupStep::Language, s_lang.as_str()),
        (SetupStep::Gw2ApiKey, s_gw2.as_str()),
        (SetupStep::LlmApiKey, s_ai.as_str()),
        (SetupStep::DataDownload, s_data.as_str()),
    ];
    let current_idx = match step {
        SetupStep::Language => 0,
        SetupStep::Gw2ApiKey => 1,
        SetupStep::LlmApiKey => 2,
        SetupStep::DataDownload => 3,
    };
    for (i, (target, name)) in steps.iter().enumerate() {
        if i > 0 {
            ui.same_line_with_spacing(0.0, 8.0);
        }
        let selected = i == current_idx;
        let done = i < current_idx;
        if theme::pill(ui, name, selected || done, &format!("##setup_step_{i}")) && done {
            state.screen = Screen::Setup(target.clone());
        }
    }
    ui.spacing();
    ui.spacing();

    match step {
        SetupStep::Language => render_language_step(ui, state),
        SetupStep::Gw2ApiKey => render_gw2_key_step(ui, state),
        SetupStep::LlmApiKey => render_llm_key_step(ui, state),
        SetupStep::DataDownload => render_download_step(ui, state),
    }
}

fn render_language_step(ui: &Ui, state: &mut AddonState) {
    use nexus::imgui::{ComboBox, Selectable};

    theme::header(ui, &t("setup.lang_header"));
    ui.spacing();
    ui.text_wrapped(t("setup.lang_help"));
    ui.spacing();

    ui.text(t("settings.language"));
    ui.set_next_item_width(-1.0);
    let resolved = gw2_core::i18n::resolve(&state.config.ui_language);
    let preview = if state.config.ui_language.eq_ignore_ascii_case("auto") {
        format!(
            "{} — {}",
            t("settings.language_auto"),
            gw2_core::i18n::language_by_code(resolved)
                .map(|l| l.native_name)
                .unwrap_or("English")
        )
    } else {
        gw2_core::i18n::language_by_code(&state.config.ui_language)
            .map(|l| l.native_name.to_string())
            .unwrap_or_else(|| state.config.ui_language.clone())
    };
    if let Some(_c) = ComboBox::new("##setup_ui_language")
        .preview_value(&preview)
        .begin(ui)
    {
        let auto_sel = state.config.ui_language.eq_ignore_ascii_case("auto");
        if Selectable::new(t("settings.language_auto"))
            .selected(auto_sel)
            .build(ui)
            && !auto_sel
        {
            state.config.ui_language = "auto".into();
            gw2_core::i18n::set_language("auto");
            let _ = state.config.save(&state.config_path);
        }
        for lang in gw2_core::i18n::LANGUAGES {
            let sel = state.config.ui_language == lang.code;
            if Selectable::new(lang.native_name).selected(sel).build(ui) && !sel {
                state.config.ui_language = lang.code.into();
                gw2_core::i18n::set_language(lang.code);
                let _ = state.config.save(&state.config_path);
            }
        }
    }

    ui.spacing();
    let (_back, next) = setup_nav_row(ui, None, Some(&t("btn.next")));
    if next {
        state.screen = Screen::Setup(SetupStep::Gw2ApiKey);
    }
}

fn render_gw2_key_step(ui: &Ui, state: &mut AddonState) {
    theme::header(ui, &t("setup.gw2_header"));
    ui.spacing();

    ui.text_wrapped(t("setup.gw2_help"));
    ui.spacing();

    // Copyable URL
    let url = "https://account.arena.net/applications";
    let mut url_buf = String::from(url);
    ui.set_next_item_width(-1.0);
    ui.input_text("##gw2_url", &mut url_buf)
        .read_only(true)
        .build();
    ui.spacing();

    ui.text_wrapped(t("setup.gw2_create"));
    for scope in gw2_api::client::REQUIRED_SCOPES {
        ui.bullet_text(scope);
    }
    ui.spacing();

    // Key input — masked by default. This step (and the LLM key step below)
    // is a screen a player may be streaming or screenshotting while pasting a
    // live API key, so the raw value must be hideable.
    ui.text(t("setup.paste_key"));
    let show_gw2_key = SHOW_GW2_KEY.with(|c| c.get());
    let toggle_label = if show_gw2_key { "Hide" } else { "Show" };
    let toggle_w = theme::gold_button_width(ui, toggle_label) + 8.0;
    ui.set_next_item_width(-toggle_w);
    ui.input_text("##gw2_key", &mut state.setup.gw2_key_input)
        .password(key_field_is_masked(show_gw2_key))
        .build();
    ui.same_line();
    if theme::gold_button_sized(ui, toggle_label, [toggle_w - 8.0, 0.0]) {
        SHOW_GW2_KEY.with(|c| c.set(!show_gw2_key));
    }
    ui.spacing();

    let can_validate = !state.setup.gw2_key_input.is_empty()
        && state.setup.gw2_key_status != KeyStatus::Validating;

    if theme::gold_button_sized(ui, t("btn.validate"), [120.0, 0.0]) && can_validate {
        let key = state.setup.gw2_key_input.clone();
        state.setup.gw2_key_status = KeyStatus::Validating;

        // Run validation in a tracked background worker.
        // Always populate scope table even if required scopes are missing.
        let tx_key = key.clone();
        let spawned = state.spawn_worker("setup-gw2-key-validate", move |token| {
            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Reset the Validating spinner on every exit path (including cancel).
                // Without this, navigating away mid-validation pins the status text on
                // "Validating…" until the user clicks Validate again.
                enum SetupOutcome {
                    Cancelled,
                    Invalid(String),
                    Valid {
                        scopes: Vec<(String, bool)>,
                        missing: Vec<String>,
                    },
                }

                let outcome: SetupOutcome = 'validate: {
                    if token.is_cancelled() {
                        break 'validate SetupOutcome::Cancelled;
                    }
                    let client = match gw2_api::client::Gw2Client::with_key(&tx_key) {
                        Ok(c) => c,
                        Err(e) => break 'validate SetupOutcome::Invalid(e.to_string()),
                    };

                    if token.is_cancelled() {
                        break 'validate SetupOutcome::Cancelled;
                    }
                    let info: gw2_api::client::TokenInfo = match client.get("tokeninfo") {
                        Ok(i) => i,
                        Err(e) => break 'validate SetupOutcome::Invalid(e.to_string()),
                    };

                    if token.is_cancelled() {
                        break 'validate SetupOutcome::Cancelled;
                    }

                    let required = gw2_api::client::REQUIRED_SCOPES;
                    let scopes: Vec<(String, bool)> = required
                        .iter()
                        .map(|scope| {
                            (
                                scope.to_string(),
                                info.permissions.contains(&scope.to_string()),
                            )
                        })
                        .collect();
                    let missing: Vec<String> = required
                        .iter()
                        .filter(|s| !info.permissions.contains(&s.to_string()))
                        .map(|s| s.to_string())
                        .collect();
                    SetupOutcome::Valid { scopes, missing }
                };

                crate::state::with_state(|s| match outcome {
                    SetupOutcome::Cancelled => {
                        // Clear the Validating state but leave the scope table alone.
                        if matches!(s.setup.gw2_key_status, KeyStatus::Validating) {
                            s.setup.gw2_key_status = KeyStatus::NotValidated;
                        }
                    }
                    SetupOutcome::Invalid(e) => {
                        s.setup.gw2_key_status = KeyStatus::Invalid(e);
                    }
                    SetupOutcome::Valid { scopes, missing } => {
                        s.setup.gw2_key_scopes = scopes;
                        if missing.is_empty() {
                            s.setup.gw2_key_status = KeyStatus::Valid;
                            s.config.gw2_api_key = Some(tx_key);
                            if let Err(e) = s.config.save(&s.config_path) {
                                nexus::log::log(
                                    nexus::log::LogLevel::Warning,
                                    "GW2BuildOpt",
                                    format!("Config save failed: {}", e),
                                );
                            }
                        } else {
                            s.setup.gw2_key_status = KeyStatus::Invalid(format!(
                                "Missing required scopes: {}",
                                missing.join(", ")
                            ));
                        }
                    }
                });
            }));
            if panic_result.is_err() {
                nexus::log::log(
                    nexus::log::LogLevel::Warning,
                    "GW2BuildOpt",
                    "bg thread panicked: setup_gw2_key_validation",
                );
                crate::state::with_state(|s| {
                    s.setup.gw2_key_status = KeyStatus::Invalid("thread panicked".into());
                });
            }
        });
        if !spawned {
            state.setup.gw2_key_status = KeyStatus::Invalid("could not start validation".into());
        }
    }

    ui.same_line();

    // Status indicator
    match &state.setup.gw2_key_status {
        KeyStatus::NotValidated => ui.text(t("setup.enter_validate")),
        KeyStatus::Validating => ui.text(t("setup.validating")),
        KeyStatus::Valid => {
            ui.text_colored(theme::OPTIMIZED, t("setup.valid"));
        }
        KeyStatus::Invalid(msg) => {
            ui.text_colored(theme::ERR, tf("setup.error", &[("msg", msg)]));
        }
    }

    if !state.setup.gw2_key_scopes.is_empty() {
        ui.spacing();
        ui.text(t("setup.permissions"));
        for (scope, present) in &state.setup.gw2_key_scopes {
            if *present {
                ui.text_colored(theme::OPTIMIZED, format!("  [v] {}", scope));
            } else {
                ui.text_colored(
                    theme::WARN,
                    format!("  [x] {}", tf("setup.missing", &[("scope", scope)])),
                );
            }
        }
    }

    ui.spacing();
    let next_label = if state.setup.gw2_key_status == KeyStatus::Valid {
        Some(t("btn.next"))
    } else {
        None
    };
    let (back, next) = setup_nav_row(ui, Some(&t("btn.back")), next_label.as_deref());
    if back {
        state.screen = Screen::Setup(SetupStep::Language);
    }
    if next {
        state.screen = Screen::Setup(SetupStep::LlmApiKey);
    }
}

fn render_llm_key_step(ui: &Ui, state: &mut AddonState) {
    theme::header(ui, &t("setup.ai_header"));
    ui.spacing();

    ui.text_wrapped(t("setup.ai_howto"));
    ui.spacing();
    ui.text_wrapped(t("setup.ai_pick"));
    ui.spacing();

    // Provider radio buttons. Gemini and OpenRouter are the free-account path
    // 1.12.0 was built around; the tag is so that is visible before the
    // per-provider steps below.
    ui.text(t("setup.provider"));
    for provider in &LlmProvider::ALL {
        let label = match provider {
            LlmProvider::Gemini | LlmProvider::OpenRouter => {
                format!("{} {}", provider.label(), t("setup.free_tag"))
            }
            _ => provider.label().to_string(),
        };
        if ui.radio_button_bool(&label, state.config.active_provider == *provider)
            && state.config.active_provider != *provider
        {
            state.config.active_provider = provider.clone();
            // Reset validation when switching providers
            state.setup.llm_key_input.clear();
            state.setup.llm_key_status = KeyStatus::NotValidated;
            // Pre-fill if we already have a key for this provider
            if let Some(key) = state.config.active_api_key() {
                state.setup.llm_key_input = key.to_string();
                state.setup.llm_key_status = KeyStatus::Valid;
            }
        }
    }
    ui.spacing();

    // Provider-specific help text and URL
    let provider = &state.config.active_provider;
    let help_key = provider.setup_howto_key();
    let url = provider.key_page_url();
    let next_key = provider.setup_steps_key();
    let help_text = t(help_key);
    let url_instructions = t(next_key);

    ui.text_wrapped(&help_text);
    ui.spacing();

    let mut url_buf = String::from(url);
    ui.set_next_item_width(-1.0);
    ui.input_text("##llm_url", &mut url_buf)
        .read_only(true)
        .build();
    ui.spacing();

    ui.text_wrapped(url_instructions);
    ui.spacing();

    // Key input — masked by default; see the GW2 key step for why.
    let provider_label = state.config.active_provider.label();
    ui.text(tf(
        "setup.paste_provider_key",
        &[("provider", provider_label)],
    ));
    let show_llm_key = SHOW_LLM_KEY.with(|c| c.get());
    let toggle_label = if show_llm_key { "Hide" } else { "Show" };
    let toggle_w = theme::gold_button_width(ui, toggle_label) + 8.0;
    ui.set_next_item_width(-toggle_w);
    ui.input_text("##llm_key", &mut state.setup.llm_key_input)
        .password(key_field_is_masked(show_llm_key))
        .build();
    ui.same_line();
    if theme::gold_button_sized(ui, toggle_label, [toggle_w - 8.0, 0.0]) {
        SHOW_LLM_KEY.with(|c| c.set(!show_llm_key));
    }
    ui.spacing();

    let can_validate = !state.setup.llm_key_input.is_empty()
        && state.setup.llm_key_status != KeyStatus::Validating;

    if theme::gold_button_sized(ui, t("btn.validate"), [120.0, 0.0]) && can_validate {
        let key = state.setup.llm_key_input.clone();
        let provider = state.config.active_provider.clone();
        state.setup.llm_key_status = KeyStatus::Validating;

        let spawned = state.spawn_worker("setup-llm-key-validate", move |token| {
            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Reset the Validating spinner on every exit path. Without this,
                // a cancellation mid-validation pins the status on "Validating…".
                let result = if token.is_cancelled() {
                    None
                } else {
                    let r = (|| -> Result<(), gw2_optimizer::llm::LlmError> {
                        use gw2_optimizer::llm::LlmClient;
                        match provider {
                            LlmProvider::Gemini => {
                                let c = gw2_optimizer::llm::gemini::GeminiLlmClient::new(
                                    &key,
                                    gw2_core::config::DEFAULT_GEMINI_MODEL,
                                )?;
                                c.validate_key()
                            }
                            LlmProvider::OpenAI => {
                                let c = gw2_optimizer::llm::openai::OpenAiClient::new(
                                    &key,
                                    gw2_core::config::DEFAULT_OPENAI_MODEL,
                                )?;
                                c.validate_key()
                            }
                            LlmProvider::Anthropic => {
                                let c = gw2_optimizer::llm::anthropic::AnthropicClient::new(
                                    &key,
                                    gw2_core::config::DEFAULT_ANTHROPIC_MODEL,
                                )?;
                                c.validate_key()
                            }
                            LlmProvider::OpenRouter => {
                                let c = gw2_optimizer::llm::openrouter::OpenRouterClient::new(
                                    &key,
                                    gw2_core::config::DEFAULT_OPENROUTER_MODEL,
                                )?;
                                c.validate_key()
                            }
                        }
                    })();
                    if token.is_cancelled() {
                        None
                    } else {
                        Some(r)
                    }
                };

                crate::state::with_state(|s| match result {
                    Some(Ok(())) => {
                        s.setup.llm_key_status = KeyStatus::Valid;
                        // Store key in the correct provider slot
                        match s.config.active_provider {
                            LlmProvider::Gemini => {
                                s.config.gemini_api_key = Some(key);
                            }
                            LlmProvider::OpenAI => {
                                s.config.openai_api_key = Some(key);
                            }
                            LlmProvider::Anthropic => {
                                s.config.anthropic_api_key = Some(key);
                            }
                            LlmProvider::OpenRouter => {
                                s.config.openrouter_api_key = Some(key);
                            }
                        }
                        if let Err(e) = s.config.save(&s.config_path) {
                            nexus::log::log(
                                nexus::log::LogLevel::Warning,
                                "GW2BuildOpt",
                                format!("Config save failed: {}", e),
                            );
                        }
                    }
                    Some(Err(e)) => {
                        s.setup.llm_key_status = KeyStatus::Invalid(e.to_string());
                    }
                    None => {
                        // Cancelled. Clear the Validating spinner without overwriting
                        // a status the user has since set (e.g. by switching providers).
                        if matches!(s.setup.llm_key_status, KeyStatus::Validating) {
                            s.setup.llm_key_status = KeyStatus::NotValidated;
                        }
                    }
                });
            }));
            if panic_result.is_err() {
                nexus::log::log(
                    nexus::log::LogLevel::Warning,
                    "GW2BuildOpt",
                    "bg thread panicked: setup_llm_key_validation",
                );
                crate::state::with_state(|s| {
                    s.setup.llm_key_status = KeyStatus::Invalid("thread panicked".into());
                });
            }
        });
        if !spawned {
            state.setup.llm_key_status = KeyStatus::Invalid("could not start validation".into());
        }
    }

    ui.same_line();

    match &state.setup.llm_key_status {
        KeyStatus::NotValidated => ui.text(t("setup.enter_validate")),
        KeyStatus::Validating => ui.text(t("setup.validating")),
        KeyStatus::Valid => {
            ui.text_colored(theme::OPTIMIZED, t("setup.valid"));
        }
        KeyStatus::Invalid(msg) => {
            ui.text_colored(theme::ERR, tf("setup.error", &[("msg", msg)]));
        }
    }

    // Navigation — one centered Back | Next row (never stacked).
    ui.spacing();
    let next_label = if state.setup.llm_key_status == KeyStatus::Valid {
        Some(t("btn.next"))
    } else {
        None
    };
    let (back, next) = setup_nav_row(ui, Some(&t("btn.back")), next_label.as_deref());
    if back {
        state.screen = Screen::Setup(SetupStep::Gw2ApiKey);
    }
    if next {
        state.screen = Screen::Setup(SetupStep::DataDownload);
    }
}

fn render_download_step(ui: &Ui, state: &mut AddonState) {
    theme::header(ui, &t("setup.data_header"));
    ui.spacing();

    ui.text_wrapped(t("setup.data_intro"));
    ui.spacing();

    let progress_snapshot = state.setup.download_progress.clone();
    match progress_snapshot {
        None => {
            // Not started yet — show start button
            if theme::gold_button_sized(ui, t("btn.start_download"), [160.0, 0.0]) {
                state.setup.download_progress = Some(DownloadState {
                    current_step: 0,
                    total_steps: 14,
                    step_name: t("status.starting"),
                    inner_done: 0,
                    inner_total: 0,
                    done: false,
                    error: None,
                    items_fill_kind: None,
                });

                let cache_dir = state.addon_dir.join("cache");

                let spawned = state.spawn_worker("setup-data-download", move |token| {
                    let panic_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            // Drive the download in a single labelled block so every exit
                            // path lands at the unified state write below. Previously,
                            // cancel mid-download froze the progress bar with no Retry/Next
                            // affordance.
                            enum DlOutcome {
                                Cancelled,
                                ClientError(String),
                                DownloadError(String),
                                Ok(u32),
                            }

                            let outcome: DlOutcome = 'download: {
                                if token.is_cancelled() {
                                    break 'download DlOutcome::Cancelled;
                                }
                                let client = match gw2_api::client::Gw2Client::without_key() {
                                    Ok(c) => c,
                                    Err(e) => {
                                        break 'download DlOutcome::ClientError(e.to_string())
                                    }
                                };
                                let cache = gw2_api::cache::DataCache::new(&cache_dir);

                                let fill_kind = state::probe_items_fill_kind(&client, &cache);
                                crate::state::with_state(|s| {
                                    if let Some(ref mut dl) = s.setup.download_progress {
                                        dl.items_fill_kind = fill_kind;
                                    }
                                });

                                let token_inner = token.clone();
                                let result = gw2_api::download::download_game_and_names(
                                    &client,
                                    &cache,
                                    || token_inner.is_cancelled(),
                                    gw2_api::download::RefreshMode::Default,
                                    |progress| {
                                        if token_inner.is_cancelled() {
                                            return;
                                        }
                                        crate::state::with_state(|s| {
                                            let name = if let Some(ref detail) = progress.detail {
                                                format!("{} ({})", progress.step_name, detail)
                                            } else {
                                                progress.step_name.clone()
                                            };
                                            let fill = s
                                                .setup
                                                .download_progress
                                                .as_ref()
                                                .and_then(|d| d.items_fill_kind);
                                            s.setup.download_progress = Some(DownloadState {
                                                current_step: progress.current_step,
                                                total_steps: progress.total_steps,
                                                step_name: name,
                                                inner_done: progress.inner_done,
                                                inner_total: progress.inner_total,
                                                done: progress.done,
                                                error: None,
                                                items_fill_kind: fill,
                                            });
                                        });
                                    },
                                );

                                if token.is_cancelled() {
                                    break 'download DlOutcome::Cancelled;
                                }
                                match result {
                                    Ok(b) => DlOutcome::Ok(b),
                                    Err(e) => DlOutcome::DownloadError(e.to_string()),
                                }
                            };

                            crate::state::with_state(|s| match outcome {
                                DlOutcome::Ok(build) => {
                                    s.config.cache_build_number = Some(build);
                                    if let Err(e) = s.config.save(&s.config_path) {
                                        nexus::log::log(
                                            nexus::log::LogLevel::Warning,
                                            "GW2BuildOpt",
                                            format!("Config save failed: {}", e),
                                        );
                                    }
                                    if let Some(ref mut dl) = s.setup.download_progress {
                                        dl.done = true;
                                    }
                                }
                                DlOutcome::ClientError(e) | DlOutcome::DownloadError(e) => {
                                    if let Some(ref mut dl) = s.setup.download_progress {
                                        dl.error = Some(e);
                                    }
                                }
                                DlOutcome::Cancelled => {
                                    // Surface cancellation as an error so the user gets the
                                    // Retry button. Otherwise the progress bar freezes with
                                    // no way forward.
                                    if let Some(ref mut dl) = s.setup.download_progress {
                                        dl.error = Some("Cancelled".into());
                                    }
                                }
                            });
                        }));
                    if panic_result.is_err() {
                        nexus::log::log(
                            nexus::log::LogLevel::Warning,
                            "GW2BuildOpt",
                            "bg thread panicked: setup_data_download",
                        );
                        crate::state::with_state(|s| {
                            s.setup.download_progress = Some(crate::state::DownloadState {
                                current_step: 0,
                                total_steps: 0,
                                step_name: String::new(),
                                inner_done: 0,
                                inner_total: 0,
                                done: true,
                                error: Some("thread panicked".into()),
                                items_fill_kind: None,
                            });
                        });
                    }
                });
                if !spawned {
                    state.setup.download_progress = Some(DownloadState {
                        current_step: 0,
                        total_steps: 0,
                        step_name: String::new(),
                        inner_done: 0,
                        inner_total: 0,
                        done: true,
                        error: Some("could not start download".into()),
                        items_fill_kind: None,
                    });
                }
            }
        }
        Some(dl) => {
            if let Some(kind) = dl.items_fill_kind {
                ui.text_colored(theme::pal().muted, t(state::items_fill_i18n_key(kind)));
            }
            let overlay = format!("{}/{} — {}", dl.current_step, dl.total_steps, dl.step_name);
            theme::download_scribble(ui, dl.fraction(), &overlay);

            if let Some(ref err) = dl.error {
                ui.spacing();
                ui.text_colored(theme::ERR, tf("setup.error", &[("msg", err)]));
                ui.spacing();
                if theme::gold_button_sized(ui, t("btn.retry"), [120.0, 0.0]) {
                    state.setup.download_progress = None;
                }
            } else if dl.done {
                ui.spacing();
                ui.text_colored(theme::OPTIMIZED, t("setup.download_complete"));
            }
        }
    }

    // Nav row (never while downloading). Idle/error: Back only. Done: Back | Get Started
    // one centered row → Main (Complete/Ready screen+pill removed). Start/Retry stay in body.
    let is_downloading = state
        .setup
        .download_progress
        .as_ref()
        .is_some_and(|dl| !dl.done && dl.error.is_none());
    let download_done = state
        .setup
        .download_progress
        .as_ref()
        .is_some_and(|dl| dl.done && dl.error.is_none());

    if !is_downloading {
        ui.spacing();
        let next_label = if download_done {
            Some(t("btn.get_started"))
        } else {
            None
        };
        let (back, next) = setup_nav_row(ui, Some(&t("btn.back")), next_label.as_deref());
        if back {
            state.screen = Screen::Setup(SetupStep::LlmApiKey);
        }
        if next {
            state.screen = Screen::Main;
        }
    }

    ui.dummy([0.0, 10.0]);
    // Match Tyria Dispatch sources (News tab parity), not Official-only.
    let sources = state.config.news.enabled_sources();
    crate::news::kick(state, &sources);
    let items = state.news.collected(&sources);
    if state.config.news.show_images {
        let urls: Vec<String> = items.iter().filter_map(|i| i.image_url.clone()).collect();
        crate::news::kick_art(state, &urls);
    }
    let loading = state.news.loading;
    let empty = t("news.unavailable");
    let scale = state.config.font_scale;
    let show_images = state.config.news.show_images;
    crate::ui::news_feed::scroll_area(ui, "##setup_news", scale, || {
        ui.text_colored(theme::pal().gold, t("setup.news_header"));
        crate::ui::news_feed::render_workspace(
            ui,
            crate::ui::news_feed::Workspace {
                items: &items,
                selected: &mut state.news.expanded,
                loading,
                empty: &empty,
                layout: gw2_core::config::NewsLayout::Desk,
                show_images,
                auto_select: true,
                id: "setup",
                still_zoom: &mut state.news.still_zoom,
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GW2 key and LLM key steps share this screen with a player who may
    /// be streaming or screenshotting the overlay while pasting a live key.
    /// `key_field_is_masked` is the exact function both `.password(...)` call
    /// sites use, and the reveal toggles must default to hidden.
    #[test]
    fn setup_key_fields_are_masked() {
        assert!(
            key_field_is_masked(false),
            "an un-revealed key field must render with the PASSWORD flag set"
        );
        assert!(
            !key_field_is_masked(true),
            "toggling reveal on must unmask the field"
        );

        let gw2_default_revealed = SHOW_GW2_KEY.with(|c| c.get());
        let llm_default_revealed = SHOW_LLM_KEY.with(|c| c.get());
        assert!(
            !gw2_default_revealed,
            "the GW2 key reveal toggle must default to hidden"
        );
        assert!(
            !llm_default_revealed,
            "the LLM key reveal toggle must default to hidden"
        );
        assert!(
            key_field_is_masked(gw2_default_revealed),
            "the GW2 key field must be masked on first render"
        );
        assert!(
            key_field_is_masked(llm_default_revealed),
            "the LLM key field must be masked on first render"
        );
    }

    /// New howto keys live in en.json only; `t()` must not echo the key name.
    /// URLs come from [`LlmProvider::key_page_url`] — one table for both screens.
    #[test]
    fn every_provider_has_english_howto() {
        for provider in &LlmProvider::ALL {
            let help = provider.setup_howto_key();
            let next = provider.setup_steps_key();
            assert_ne!(t(help), help, "{help} is missing from the English catalog");
            assert_ne!(t(next), next, "{next} is missing from the English catalog");
        }
        assert_ne!(t("setup.ai_howto"), "setup.ai_howto");
        assert_ne!(t("setup.ai_pick"), "setup.ai_pick");
        assert_ne!(t("setup.free_tag"), "setup.free_tag");
    }
}
