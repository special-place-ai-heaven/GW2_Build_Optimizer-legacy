use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::resolution::resolve_selected_build_inner;
use crate::state::{AddonState, ApiStatus};
use gw2_core::i18n::t;

/// How long a name pack that could not be loaded is left alone before the next
/// try. [`ensure_localized_names`] runs on every frame, and a pack that is
/// missing, stale or corrupt costs a file probe — or, when it parses only
/// partly, a multi-megabyte JSON parse — on every one of them without this.
const LOCALE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Which name pack was last asked for from disk, and when.
struct LocaleAttempt {
    /// API language plus the cache build number. A language switch or a game
    /// patch is a *different* pack, not a retry, so it must not have to sit out
    /// a cooldown that something else started.
    key: (String, Option<u32>),
    at: Instant,
}

/// Cooldown for [`ensure_localized_names`]. A static rather than `MainState`
/// because it is cache-miss bookkeeping, not addon state: nothing renders it,
/// nothing persists it, and it means the same thing across a state reset.
static LOCALE_ATTEMPT: Mutex<Option<LocaleAttempt>> = Mutex::new(None);

/// Record an attempt to load `key` and report whether it may go to disk now.
///
/// Pure over `slot` and `now`, so the frame loop is testable without a clock,
/// a cache directory, or a thread.
fn locale_attempt_allowed(
    slot: &mut Option<LocaleAttempt>,
    key: (&str, Option<u32>),
    now: Instant,
) -> bool {
    let allowed = match slot {
        Some(last) => {
            last.key.0 != key.0
                || last.key.1 != key.1
                || now.saturating_duration_since(last.at) >= LOCALE_RETRY_INTERVAL
        }
        None => true,
    };
    if allowed {
        *slot = Some(LocaleAttempt {
            key: (key.0.to_string(), key.1),
            at: now,
        });
    }
    allowed
}

/// The last answer [`cached_pack_status`] got from disk.
struct CachedPackStatus {
    key: (String, Option<u32>),
    at: Instant,
    status: gw2_api::localize::PackStatus,
}

static PACK_STATUS: Mutex<Option<CachedPackStatus>> = Mutex::new(None);

/// [`gw2_api::localize::pack_status`] for the status bar, re-read from disk at
/// most every [`LOCALE_RETRY_INTERVAL`] — the same "how long a locale-pack disk
/// answer stays good for" window the loader uses.
///
/// The bare call opens the cache entry and parses its header, and
/// `DataCache::new` runs a `create_dir_all` on top. None of that belongs on the
/// render thread 60 times a second to pick the colour of one label.
pub(super) fn cached_pack_status(
    addon_dir: &std::path::Path,
    lang: &str,
    build: Option<u32>,
) -> gw2_api::localize::PackStatus {
    let key = (lang.to_string(), build);
    let now = Instant::now();
    let mut slot = PACK_STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = slot.as_ref() {
        if cached.key == key && now.saturating_duration_since(cached.at) < LOCALE_RETRY_INTERVAL {
            return cached.status;
        }
    }
    let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
    let status = gw2_api::localize::pack_status(&cache, lang, build);
    *slot = Some(CachedPackStatus {
        key,
        at: now,
        status,
    });
    status
}

/// Attach cached official API names for de/es/fr/zh. Never downloads — packs come from setup/refresh.
///
/// Called on every frame from `render_main`, so neither of the two costs may
/// land there: the read and JSON parse happen on a worker rather than under
/// `STATE`, and a pack that is missing or unreadable is retried every
/// [`LOCALE_RETRY_INTERVAL`] instead of every frame.
pub(super) fn ensure_localized_names(state: &mut AddonState) {
    let Some(lang) = gw2_core::i18n::api_lang(&state.config.ui_language) else {
        // Only reach for the database mutably when there is something to clear.
        // `game_db_mut` is skip-when-shared (`Arc::get_mut`), so an unconditional
        // call here is a no-op while a worker holds a clone — still skip it on
        // the English path so we do not even take the unique-owner write.
        if state
            .main
            .game_db
            .as_ref()
            .is_some_and(|db| db.localized.is_some())
        {
            if let Some(db) = state.main.game_db_mut() {
                db.localized = None;
            }
        }
        state.main.names_loading = false;
        state.main.names_stage.clear();
        state.main.names_lang.clear();
        return;
    };
    if state
        .main
        .game_db
        .as_ref()
        .and_then(|d| d.localized.as_ref())
        .is_some_and(|l| l.lang == lang)
    {
        return;
    }
    if state.main.game_db.is_none() {
        return;
    }
    let build = state.config.cache_build_number;
    {
        let mut slot = LOCALE_ATTEMPT.lock().unwrap_or_else(|e| e.into_inner());
        if !locale_attempt_allowed(&mut slot, (lang, build), Instant::now()) {
            return;
        }
    }

    let cache_dir = state.addon_dir.join("cache");
    let lang = lang.to_string();
    // ponytail: the cooldown, not an in-flight flag, is what keeps this to one
    // worker. A load that outlives `LOCALE_RETRY_INTERVAL` can be joined by a
    // second one; both read the same file and attach the same names, so the
    // cost is a duplicate parse, not a wrong result. Add a flag if packs ever
    // grow big enough for that to be a real second of work.
    state.spawn_worker("locale-pack", move |token| {
        let cache = gw2_api::cache::DataCache::new(&cache_dir);
        let loaded = gw2_api::localize::load(&cache, &lang, build);
        if token.is_cancelled() {
            return;
        }
        crate::state::with_state(|s| {
            // The player can switch language while a pack loads, and a refresh
            // can publish a different database — only attach to the one the UI
            // is asking for right now.
            if let Ok(Some(names)) = loaded {
                if gw2_core::i18n::api_lang(&s.config.ui_language) == Some(lang.as_str()) {
                    // `game_db_mut` is skip-when-shared (`Arc::get_mut`): attach
                    // in place when unique, skip rather than clone GameDb under
                    // STATE if an optimize is mid-flight with its own clone.
                    if let Some(db) = s.main.game_db_mut() {
                        db.attach_localized(names);
                        s.main.names_lang = lang.clone();
                    }
                }
            }
            s.main.names_loading = false;
            s.main.names_stage.clear();
        });
    });
}
/// Fetch available models from the active provider's API in a background thread.
pub(super) fn start_fetch_models(state: &mut AddonState) {
    state.main.models_loading = true;
    state.main.models_error = None;
    let addon_dir = state.addon_dir.clone();
    let config_snapshot = state.config.clone();
    /// models.dev's id for one of our providers.
    fn models_dev_provider(provider: gw2_core::config::LlmProvider) -> &'static str {
        match provider {
            gw2_core::config::LlmProvider::Gemini => "google",
            gw2_core::config::LlmProvider::OpenAI => "openai",
            gw2_core::config::LlmProvider::Anthropic => "anthropic",
            gw2_core::config::LlmProvider::OpenRouter => "openrouter",
        }
    }
    let spawned = state.spawn_worker("fetch-models", move |token| {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Always reset models_loading on every exit path. Without this, an early
            // cancellation (e.g. user closed the Settings tab) leaves the spinner
            // stuck "Loading models…" forever.
            let result = if token.is_cancelled() {
                None
            } else {
                let r = gw2_optimizer::llm::create_client(&config_snapshot, &addon_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.list_models().map_err(|e| e.to_string()))
                    .map(|mut models| {
                        // What models.dev knows on top of what the provider
                        // said: tools where the provider's catalog is silent
                        // (OpenAI, Anthropic, Google), structured output
                        // everywhere. Refreshed daily at launch; a stale or
                        // absent file changes nothing here.
                        gw2_optimizer::llm::models_dev::load(&addon_dir);
                        gw2_optimizer::llm::models_dev::enrich(
                            models_dev_provider(config_snapshot.active_provider),
                            &mut models,
                        );
                        models
                    });
                if token.is_cancelled() {
                    None
                } else {
                    Some(r)
                }
            };
            crate::state::with_state(|s| {
                s.main.models_loading = false;
                match result {
                    Some(Ok(models)) => {
                        s.main.available_models = models;
                        s.main.models_error = None;
                    }
                    Some(Err(e)) => {
                        s.main.models_error = Some(e);
                    }
                    None => { /* cancelled — only the flag reset matters */ }
                }
            });
        }));
        if panic_result.is_err() {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                "bg thread panicked: start_fetch_models",
            );
            crate::state::with_state(|s| {
                s.main.models_loading = false;
            });
        }
    });
    if !spawned {
        // The OS refused the thread (`spawn_worker` logged it). Nothing will
        // fetch, so clear the spinner this function turned on rather than
        // leaving "Loading models…" up forever.
        state.main.models_loading = false;
    }
}

/// Start the one automatic game-data refresh of this session? Only when the
/// user enabled it, setup is complete, the cache is behind the live build,
/// no load or refresh is running (`busy`), and it has not already run.
pub(super) fn should_auto_refresh(
    enabled: bool,
    setup_complete: bool,
    stale: bool,
    busy: bool,
    already_ran: bool,
) -> bool {
    enabled && setup_complete && stale && !busy && !already_ran
}

/// Re-download game data from the GW2 API, then reload GameDb.
pub(super) fn start_game_data_refresh(state: &mut AddonState) {
    state.main.game_db_loading = true;
    let cache_dir = state.addon_dir.join("cache");

    let spawned = state.spawn_worker("game-data-refresh", move |token| {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Drive the refresh in a single labelled block so every exit path falls
            // through to the unified state reset below. Previously the 4 early-return
            // cancel checks left game_db_loading=true and game_refresh_stage non-empty,
            // freezing the main screen on a partial "Refreshing: …" banner.
            enum Outcome {
                Cancelled,
                ClientError(String),
                DownloadError(String),
                DbLoad(Box<Result<gw2_optimizer::gamedb::GameDb, String>>),
            }

            let outcome: Outcome = 'refresh: {
                if token.is_cancelled() {
                    break 'refresh Outcome::Cancelled;
                }
                let client = match gw2_api::client::Gw2Client::without_key() {
                    Ok(c) => c,
                    Err(e) => break 'refresh Outcome::ClientError(e.to_string()),
                };
                let cache = gw2_api::cache::DataCache::new(&cache_dir);

                let fill_kind = crate::state::probe_items_fill_kind(&client, &cache);
                crate::state::with_state(|s| {
                    s.setup.download_progress = Some(crate::state::DownloadState {
                        current_step: 0,
                        total_steps: 14,
                        step_name: String::new(),
                        inner_done: 0,
                        inner_total: 0,
                        done: false,
                        error: None,
                        items_fill_kind: fill_kind,
                    });
                });

                let download_result = gw2_api::download::download_game_and_names(
                    &client,
                    &cache,
                    || token.is_cancelled(),
                    gw2_api::download::RefreshMode::Default,
                    |progress| {
                        if token.is_cancelled() {
                            return;
                        }
                        crate::state::with_state(|s| {
                            let fill = s
                                .setup
                                .download_progress
                                .as_ref()
                                .and_then(|d| d.items_fill_kind);
                            let mode = fill
                                .map(crate::state::items_fill_i18n_key)
                                .map(gw2_core::i18n::t);
                            let detail = if let Some(ref d) = progress.detail {
                                format!("Refreshing: {} ({})", progress.step_name, d)
                            } else {
                                format!("Refreshing: {}", progress.step_name)
                            };
                            s.main.game_refresh_stage = if let Some(ref mode) = mode {
                                format!("{} | {}", mode, detail)
                            } else {
                                detail
                            };
                            s.setup.download_progress = Some(crate::state::DownloadState {
                                current_step: progress.current_step,
                                total_steps: progress.total_steps,
                                step_name: progress.step_name,
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
                    break 'refresh Outcome::Cancelled;
                }

                let build_number = match download_result {
                    Ok(n) => n,
                    Err(e) => break 'refresh Outcome::DownloadError(e.to_string()),
                };

                // Publish the new build number, then hand the write to the
                // config writer: this runs inside `with_state`, and an
                // `AppConfig::save` here would hold STATE — and so the render
                // thread — for the length of a disk write.
                crate::state::with_state(|s| {
                    s.config.cache_build_number = Some(build_number);
                    crate::ui::save_config_detached(s);
                });

                if token.is_cancelled() {
                    break 'refresh Outcome::Cancelled;
                }
                let cache2 = gw2_api::cache::DataCache::new(&cache_dir);
                let db_result =
                    gw2_optimizer::gamedb::GameDb::load(&cache2).map_err(|e| e.to_string());

                if token.is_cancelled() {
                    break 'refresh Outcome::Cancelled;
                }

                Outcome::DbLoad(Box::new(db_result))
            };

            crate::state::with_state(|s| {
                s.main.game_db_loading = false;
                s.main.game_refresh_stage = String::new();
                s.setup.download_progress = None;
                match outcome {
                    Outcome::Cancelled => {}
                    Outcome::ClientError(e) | Outcome::DownloadError(e) => {
                        s.main.error = Some(format!("Refresh failed: {}", e));
                    }
                    Outcome::DbLoad(db_result) => match *db_result {
                        Ok(db) => {
                            nexus::log::log(
                                nexus::log::LogLevel::Info,
                                "GW2 Build Optimizer",
                                "Game data refreshed successfully",
                            );
                            s.main.set_game_db(db);
                            crate::ui::main_view::stats::ensure_localized_names(s);
                            if s.main.selected_build_tab.is_some()
                                && s.main.selected_equipment_tab.is_some()
                            {
                                resolve_selected_build_inner(s);
                            }
                        }
                        Err(e) => {
                            s.main.error = Some(format!("Failed to reload game data: {}", e));
                        }
                    },
                }
            });
        }));
        if panic_result.is_err() {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                "bg thread panicked: start_game_data_refresh",
            );
            crate::state::with_state(|s| {
                s.main.game_db_loading = false;
                s.main.game_refresh_stage = String::new();
                s.setup.download_progress = None;
            });
        }
    });
    if !spawned {
        // The OS refused the thread (`spawn_worker` logged it): clear the
        // banner this function turned on instead of freezing on "Refreshing…".
        state.main.game_db_loading = false;
        state.main.game_refresh_stage = String::new();
    }
}

/// Lightweight API health check: pings GET /v2/build (unauthenticated, returns a single integer).
pub(super) fn check_api_health(state: &mut AddonState) {
    state.main.api_health_checking = true;

    let spawned = state.spawn_worker("api-health", move |token| {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut live_build = None;
            let status: Option<crate::state::ApiStatus> = if token.is_cancelled() {
                None
            } else {
                let start = std::time::Instant::now();
                let result =
                    gw2_api::client::Gw2Client::without_key().and_then(|c| c.get_build_number());
                if token.is_cancelled() {
                    None
                } else {
                    Some(match result {
                        Ok(build) => {
                            live_build = Some(build);
                            if start.elapsed().as_secs() >= 5 {
                                crate::state::ApiStatus::Degraded
                            } else {
                                crate::state::ApiStatus::Online
                            }
                        }
                        Err(_) => crate::state::ApiStatus::Offline,
                    })
                }
            };
            crate::state::with_state(|s| {
                s.main.api_health_checking = false;
                if let Some(b) = live_build {
                    s.main.live_build_number = Some(b);
                    s.main.manifest_staleness =
                        gw2_optimizer::balance::live_build_mismatch(u64::from(b));
                }
                if let Some(st) = status {
                    s.main.api_status = st;
                }
            });
        }));
        if panic_result.is_err() {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                "bg thread panicked: check_api_health",
            );
            crate::state::with_state(|s| {
                s.main.api_status = crate::state::ApiStatus::Offline;
                s.main.api_health_checking = false;
            });
        }
    });
    if !spawned {
        // No thread, no ping: release the "checking" latch so the next frame
        // that is due can try again.
        state.main.api_health_checking = false;
    }
}

/// Status-bar chip: API readiness plus the live `/v2/build` id when we have it.
pub(super) fn api_status_label(status: &ApiStatus, live_build: Option<u32>) -> String {
    let base = match status {
        ApiStatus::Unknown => t("status.checking_api"),
        ApiStatus::Online => t("status.api_ready"),
        ApiStatus::Degraded => t("status.api_slow"),
        ApiStatus::Offline => t("status.api_offline"),
    };
    match (status, live_build) {
        (ApiStatus::Online | ApiStatus::Degraded, Some(n)) => format!("{base} · {n}"),
        _ => base,
    }
}

/// Status-bar chip when catalog load is blocked. Combat-snapshot vs live build
/// stays off this bar — after a refresh it looks like the API update failed.
pub(super) fn render_manifest_staleness(ui: &nexus::imgui::Ui, state: &crate::state::AddonState) {
    if let Some(reason) = state
        .main
        .data_state
        .as_ref()
        .and_then(|s| s.optimize_block_reason())
    {
        ui.same_line();
        ui.text_colored(crate::ui::theme::ERR, "| Data disabled");
        if ui.is_item_hovered() {
            ui.tooltip_text(reason);
        }
    }
}

/// Load GameDb once on main screen entry (S11-T06)
pub(super) fn load_game_db(state: &mut AddonState) {
    state.main.game_db_loading = true;
    let cache_dir = state.addon_dir.join("cache");

    let spawned = state.spawn_worker("load-game-db", move |token| {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Always reset game_db_loading on every exit path. Early-return cancel
            // checks previously skipped the state write, leaving the main screen
            // spinner stuck "Loading game data…".
            let result = if token.is_cancelled() {
                None
            } else {
                let cache = gw2_api::cache::DataCache::new(&cache_dir);
                let r = gw2_optimizer::gamedb::GameDb::load(&cache);
                if token.is_cancelled() {
                    None
                } else {
                    Some(r)
                }
            };

            crate::state::with_state(|s| {
                s.main.game_db_loading = false;
                match result {
                    Some(Ok(db)) => {
                        nexus::log::log(
                            nexus::log::LogLevel::Info,
                            "GW2 Build Optimizer",
                            db.summary(),
                        );
                        s.main.set_game_db(db);
                        crate::ui::main_view::stats::ensure_localized_names(s);
                        // If build tabs were loaded before GameDb, trigger resolve now
                        if s.main.selected_build_tab.is_some()
                            && s.main.selected_equipment_tab.is_some()
                        {
                            resolve_selected_build_inner(s);
                        }
                    }
                    Some(Err(e)) => {
                        s.main.error = Some(format!("Failed to load game data: {}", e));
                    }
                    None => { /* cancelled — flag reset above */ }
                }
            });
        }));
        if panic_result.is_err() {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                "bg thread panicked: load_game_db",
            );
            crate::state::with_state(|s| {
                s.main.game_db_loading = false;
            });
        }
    });
    if !spawned {
        // The OS refused the thread (`spawn_worker` logged it). `game_db_retry_at`
        // already spaces the next attempt out, so just drop the spinner.
        state.main.game_db_loading = false;
    }
}

/// Convert CombatPerformance to the display-friendly CombatMetrics bridge type.
pub(super) fn perf_to_combat_metrics(
    perf: &gw2_optimizer::combat::CombatPerformance,
) -> gw2_core::types::CombatMetrics {
    gw2_core::types::CombatMetrics {
        effective_power: perf.effective_power.round() as i32,
        strike_dps_index: perf.strike_dps_index.round() as i32,
        condition_dps_index: perf.condition_dps_index.round() as i32,
        total_dps_index: perf.total_dps_index.round() as i32,
        healing_index: perf.healing_power_index.round() as i32,
        crit_chance: perf.crit_chance,
        boon_duration_pct: perf.boon_duration_pct,
        condi_duration_pct: perf.condi_duration_pct,
        effective_health: perf.effective_health.round() as i32,
        damage_reduction_pct: perf.damage_reduction_pct,
        bleeding_tick: perf.condition_ticks.bleeding.round() as i32,
        burning_tick: perf.condition_ticks.burning.round() as i32,
        poison_tick: perf.condition_ticks.poison.round() as i32,
        torment_tick: perf.condition_ticks.torment.round() as i32,
        confusion_tick: perf.condition_ticks.confusion.round() as i32,
    }
}

/// Stats pane for one plated build: [`super::optimization::measure_validated`],
/// which wraps [`gw2_optimizer::engine::measure_plated`]. Optimizer suggestions
/// read this so the tab and Stats are not two formulas.
pub(super) fn plated_display(
    validated: &gw2_optimizer::validation::ValidatedBuild,
    db: &gw2_optimizer::gamedb::GameDb,
    profession: &str,
    weights: &gw2_optimizer::scoring::OptimizationWeights,
    ctx: &gw2_optimizer::balance::BalanceContext,
    scenario: &gw2_optimizer::scenario::ScenarioSpec,
) -> crate::ui::comparison::BuildSuggestion {
    let mut suggestion = crate::ui::comparison::BuildSuggestion::default();
    super::optimization::measure_validated(
        &mut suggestion,
        validated,
        db,
        profession,
        weights,
        ctx,
        scenario,
    );
    suggestion
}

/// Closed-form Solo / Party / Squad metrics for a stat sheet that is not a plate.
///
/// The formula is [`gw2_optimizer::engine::combat_tiers`] (the combat half of
/// [`gw2_optimizer::engine::measure_plated`]). A plated build uses
/// [`plated_display`] so flow is included.
///
/// ponytail: equipped-character resolve and the save preview still pass their
/// own sheet in. Upgrade path is to price that sheet with
/// `calculate_validated_stats` when `resolution.rs` is next touched.
pub(super) fn compute_3tier_combat(
    stats: &gw2_optimizer::stats::StatBlock,
    derived: &gw2_optimizer::stats::DerivedStats,
    modifiers: &gw2_optimizer::combat::DamageModifiers,
    profession: &str,
    balance_ctx: &gw2_optimizer::balance::BalanceContext,
) -> (
    Option<gw2_core::types::CombatMetrics>,
    Option<gw2_core::types::CombatMetrics>,
    Option<gw2_core::types::CombatMetrics>,
) {
    let [solo, party, squad] =
        gw2_optimizer::engine::combat_tiers(stats, derived, modifiers, profession, balance_ctx);
    (
        Some(perf_to_combat_metrics(&solo)),
        Some(perf_to_combat_metrics(&party)),
        Some(perf_to_combat_metrics(&squad)),
    )
}

#[cfg(test)]
mod tests {
    use super::{locale_attempt_allowed, LOCALE_RETRY_INTERVAL};
    use std::time::{Duration, Instant};

    #[test]
    fn auto_refresh_starts_only_when_every_condition_holds() {
        use super::should_auto_refresh as go;
        // enabled, setup complete, stale, not busy, not yet run -> start
        assert!(go(true, true, true, false, false));
        assert!(!go(false, true, true, false, false), "setting off");
        assert!(!go(true, false, true, false, false), "setup incomplete");
        assert!(!go(true, true, false, false, false), "cache current");
        assert!(
            !go(true, true, true, true, false),
            "refresh or load running"
        );
        assert!(
            !go(true, true, true, false, true),
            "already ran this session"
        );
    }

    #[test]
    fn api_status_label_puts_live_build_on_ready_and_slow() {
        use crate::state::ApiStatus;
        assert_eq!(
            super::api_status_label(&ApiStatus::Online, Some(207032)),
            format!("{} · 207032", gw2_core::i18n::t("status.api_ready"))
        );
        assert_eq!(
            super::api_status_label(&ApiStatus::Degraded, Some(207032)),
            format!("{} · 207032", gw2_core::i18n::t("status.api_slow"))
        );
        assert_eq!(
            super::api_status_label(&ApiStatus::Online, None),
            gw2_core::i18n::t("status.api_ready")
        );
        assert_eq!(
            super::api_status_label(&ApiStatus::Offline, Some(207032)),
            gw2_core::i18n::t("status.api_offline")
        );
    }

    #[test]
    fn live_build_mismatch_is_what_the_health_check_stores() {
        let verified = gw2_optimizer::data::manifests::latest_manifest().game_build_id;
        assert!(gw2_optimizer::balance::live_build_mismatch(verified).is_none());
        let warn = gw2_optimizer::balance::live_build_mismatch(1).expect("stale");
        assert!(warn.contains("1"));
        assert!(warn.contains(&verified.to_string()));
    }

    /// `ensure_localized_names` runs on every frame. When the pack for the
    /// selected language is missing, stale or corrupt there is nothing to
    /// attach, so without a cooldown the overlay would go back to disk 60
    /// times a second forever — and pay a full JSON parse each time whenever
    /// the file exists but cannot be used.
    #[test]
    fn locale_pack_retry_is_throttled() {
        let mut slot = None;
        let t0 = Instant::now();

        // 300 frames at 60 fps ≈ 4.8 s: a whole cooldown, minus a frame.
        let attempts = (0..300u64)
            .filter(|&frame| {
                locale_attempt_allowed(
                    &mut slot,
                    ("de", Some(7)),
                    t0 + Duration::from_millis(16 * frame),
                )
            })
            .count();
        assert_eq!(
            attempts, 1,
            "a missing pack must reach disk once per cooldown, not once per frame"
        );

        // The pack can appear at any time (a download just finished), so the
        // first frame after the cooldown does try again.
        let after = t0 + LOCALE_RETRY_INTERVAL + Duration::from_millis(1);
        assert!(
            locale_attempt_allowed(&mut slot, ("de", Some(7)), after),
            "the cooldown must expire, not latch the pack off"
        );

        // A different pack is not a retry: switching language, or a game patch
        // bumping the cache build number, must load now instead of sitting out
        // a cooldown something else started.
        assert!(locale_attempt_allowed(&mut slot, ("fr", Some(7)), after));
        assert!(locale_attempt_allowed(&mut slot, ("fr", Some(8)), after));

        // …and that pack then gets its own cooldown.
        assert!(!locale_attempt_allowed(&mut slot, ("fr", Some(8)), after));
    }

    #[test]
    fn ensure_localized_names_does_not_make_mut() {
        let src = include_str!("stats.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("stats.rs must contain its own #[cfg(test)] marker");
        assert!(
            !production.contains("Arc::make_mut"),
            "locale attach must not Arc::make_mut a shared GameDb"
        );
        assert!(
            production.contains("game_db_mut"),
            "locale attach goes through skip-when-shared game_db_mut"
        );
        assert!(
            production.contains("attach_localized"),
            "locale attach still calls attach_localized on a unique db"
        );
    }
}
