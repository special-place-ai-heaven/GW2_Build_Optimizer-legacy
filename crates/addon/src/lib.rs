mod cache_bounds;
mod chat_links;
mod clipboard;
mod feedback;
mod news;
mod news_art;
mod radio;
mod state;
pub mod ui;

use nexus::addon::UpdateProvider;
use nexus::gui::{register_render, RenderType};
use nexus::imgui::Ui;
use nexus::keybind::{keybind_handler, register_keybind_with_string};
use nexus::log::{log, LogLevel};
use nexus::paths::get_addon_dir;
use nexus::quick_access::add_quick_access;
use nexus::texture::get_texture_or_create_from_memory;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Crate version from `Cargo.toml`. UI and logs must use this, never a literal.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

nexus::export! {
    name: "GW2 Build Optimizer",
    signature: -0x47573242,
    load: on_load,
    unload: on_unload,
    provider: UpdateProvider::GitHub,
    update_link: "https://github.com/special-place-ai-heaven/GW2_Build_Optimizer",
}

/// Run addon load with an unwind guard.
///
/// Nexus calls `on_load` across the addon ABI, so a panic escaping it takes the
/// game with it. Unlike [`unload_step`], remaining init is skipped: a half-registered
/// addon is worse than a logged abort.
fn load_guard(run: impl FnOnce() + std::panic::UnwindSafe) {
    if std::panic::catch_unwind(run).is_err() {
        log(
            LogLevel::Warning,
            "GW2 Build Optimizer",
            "Load panicked; aborting addon initialization.",
        );
    }
}

/// Heap crash (0xC0000374) hits ~1s after load, while ArcDPS is still hooking
/// D3D11. First PostRender can fire before that, so wait this out first.
pub(crate) const CHROME_SETTLE: Duration = Duration::from_millis(2000);

/// `attach_overlay_host` is one-shot. `BOOTSTRAP_FAILED` latches a panic inside
/// attach so the PostRender bootstrapper stops retrying for the session;
/// recovery requires an addon reload.
static HOST_ATTACHED: AtomicBool = AtomicBool::new(false);
static BOOTSTRAP_FAILED: AtomicBool = AtomicBool::new(false);
static CHROME_AT: OnceLock<Instant> = OnceLock::new();

/// D3D-touching chrome that must wait for ArcDPS to finish hooking: texture
/// uploads and the Nexus quick-access entry. The ImGui `Render` hook for
/// [`ui::render`] is registered in `on_load` because Nexus's render registry
/// is locked for the whole frame and a `Register` call from inside a
/// PostRender callback would mutate the very vector being iterated.
fn attach_overlay_host() {
    if HOST_ATTACHED.load(Ordering::Acquire) {
        return;
    }
    if BOOTSTRAP_FAILED.load(Ordering::Acquire) {
        return;
    }
    // Claim the slot *before* doing D3D work. If the swap loses, another caller
    // already attached. If we panic after this point we still flag the failure
    // so the bootstrapper can retry next frame.
    HOST_ATTACHED.store(true, Ordering::Release);
    let result = std::panic::catch_unwind(|| {
        let _ = get_texture_or_create_from_memory(
            "GW2_BUILD_OPT_ICON_v1",
            include_bytes!("../assets/build_optimizer.png"),
        );
        let _ = get_texture_or_create_from_memory(
            "GW2_BUILD_OPT_ICON_HOVER_v1",
            include_bytes!("../assets/build_optimizer_hover.png"),
        );
        add_quick_access(
            "QA_GW2_BUILD_OPTIMIZER",
            "GW2_BUILD_OPT_ICON_v1",
            "GW2_BUILD_OPT_ICON_HOVER_v1",
            "GW2_BUILD_OPT_TOGGLE",
            "GW2 Build Optimizer",
        )
        .revert_on_unload();
    });
    if result.is_err() {
        HOST_ATTACHED.store(false, Ordering::Release);
        BOOTSTRAP_FAILED.store(true, Ordering::Release);
        log(
            LogLevel::Warning,
            "GW2 Build Optimizer",
            "Overlay chrome attach panicked; will not retry this session.",
        );
    } else {
        log(
            LogLevel::Info,
            "GW2 Build Optimizer",
            "Overlay chrome attached.",
        );
    }
}

/// PostRender callback Nexus invokes once per frame after the ImGui frame ends.
/// Nexus only fires this once it has a stable frame, which is the earliest
/// point we trust ArcDPS has finished hooking D3D11. We still wait
/// [`CHROME_SETTLE`] past `state::init` so a slow machine where Nexus starts
/// framing before ArcDPS settles still has the buffer observed in production.
fn bootstrap_chrome(_ui: &Ui) {
    let Some(at) = CHROME_AT.get() else {
        return;
    };
    if Instant::now() < *at {
        return;
    }
    if HOST_ATTACHED.load(Ordering::Acquire) {
        return;
    }
    attach_overlay_host();
}

fn on_load() {
    load_guard(|| {
        let Some(addon_dir) = get_addon_dir(gw2_core::ADDON_DIR_NAME) else {
            log(
                LogLevel::Warning,
                "GW2 Build Optimizer",
                "Failed to locate the Nexus addon directory. Aborting addon initialization.",
            );
            return;
        };

        let models_dev_dir = addon_dir.clone();
        // A pinned image keeps statics across unload/reload; undo the
        // previous unload's playback latch.
        radio::player::arm();
        state::init(addon_dir);
        let _ = CHROME_AT.set(Instant::now() + CHROME_SETTLE);

        // models.dev is updated most days; refresh our copy of it at launch
        // in the background so the model picker and the handshake read a
        // current catalog. A failed fetch keeps yesterday's file.
        state::with_state(|s| {
            s.spawn_worker("models-dev-refresh", move |token| {
                if !token.is_cancelled() {
                    gw2_optimizer::llm::models_dev::load(&models_dev_dir);
                }
            });
        });

        register_keybind_with_string(
            "GW2_BUILD_OPT_TOGGLE",
            keybind_handler!(|_id, is_release| {
                if !is_release {
                    // Nexus dispatches keybinds from its own input hook across the
                    // addon ABI, exactly like the render callback that `ui::render`
                    // already guards. `toggle_window` writes config to disk, so it
                    // has real failure modes; none of them may unwind into the game.
                    if std::panic::catch_unwind(state::toggle_window).is_err() {
                        log(
                            LogLevel::Warning,
                            "GW2 Build Optimizer",
                            "Toggle keybind panicked; overlay state may be stale.",
                        );
                    }
                }
            }),
            "CTRL+SHIFT+O",
        )
        .revert_on_unload();

        register_keybind_with_string(
            "GW2_BUILD_OPT_RADIO_TOGGLE",
            keybind_handler!(|_id, is_release| {
                if !is_release {
                    // Same ABI-boundary guard as the window toggle above: the
                    // radio toggle touches the playback runtime and config.
                    if std::panic::catch_unwind(radio::player::toggle).is_err() {
                        log(
                            LogLevel::Warning,
                            "GW2 Build Optimizer",
                            "Radio keybind panicked; playback state may be stale.",
                        );
                    }
                }
            }),
            "", // unbound by default; the user assigns one in Nexus
        )
        .revert_on_unload();

        register_keybind_with_string(
            "GW2_BUILD_OPT_MINI_RADIO_TOGGLE",
            keybind_handler!(|_id, is_release| {
                if !is_release {
                    // Same ABI-boundary guard as the toggles above: it writes
                    // config to disk.
                    if std::panic::catch_unwind(state::toggle_mini_radio).is_err() {
                        log(
                            LogLevel::Warning,
                            "GW2 Build Optimizer",
                            "Mini radio keybind panicked; its toggle may be stale.",
                        );
                    }
                }
            }),
            "", // unbound by default; the user assigns one in Nexus
        )
        .revert_on_unload();

        // The `Render` hook for `ui::render` is registered on load. Nexus locks
        // its render registry for the entire frame and iterates it on every
        // PreRender/Render/PostRender pass; a `Register` call from inside a
        // PostRender callback (deferring chrome attach) would push into the
        // very vector being iterated. Texture uploads and the quick-access
        // entry still defer to PostRender via `bootstrap_chrome`.
        register_render(RenderType::Render, nexus::gui::render!(ui::render)).revert_on_unload();

        // Function pointer only — no D3D. Texture uploads and the quick-access
        // entry attach from the first PostRender after [`CHROME_SETTLE`], when
        // ArcDPS has had time to hook D3D11.
        register_render(
            RenderType::PostRender,
            nexus::gui::render!(bootstrap_chrome),
        )
        .revert_on_unload();
        log(
            LogLevel::Info,
            "GW2 Build Optimizer",
            format!("v{} loaded. Press Ctrl+Shift+O to toggle.", crate::VERSION),
        );
    });
}

/// Run one shutdown step with its own unwind guard.
///
/// Nexus calls `on_unload` across the addon ABI, so a panic escaping it takes the
/// game with it — and a step that panics must not skip the steps after it. Leaving
/// workers running with no cancel flag, or the state alive after unload, is a worse
/// outcome than the panic that caused it.
fn unload_step(step: &str, run: impl FnOnce() + std::panic::UnwindSafe) {
    if std::panic::catch_unwind(run).is_err() {
        log(
            LogLevel::Warning,
            "GW2 Build Optimizer",
            format!("Unload step '{}' panicked; continuing shutdown.", step),
        );
    }
}

fn on_unload() {
    // Cancel first: the workers get the window-rect disk write worth of head start
    // on their `is_cancelled()` checks before anything waits on them.
    // Audio first: the playback stack owns OS threads (cpal, tokio) that must
    // be joined before FreeLibrary; radio teardown is bounded and independent
    // of the worker registry.
    unload_step("stop radio", radio::player::shutdown);
    unload_step("cancel workers", state::request_shutdown);
    unload_step("persist window", state::persist_window);

    // Wait for the workers with STATE released, then drop the state — it is that
    // drop which makes `with_state` start returning None to anything still alive,
    // so doing it before the wait would strand results mid-flight.
    let report = std::panic::catch_unwind(|| state::join_workers(state::UNLOAD_JOIN_BUDGET))
        .unwrap_or_default();
    unload_step("release state", state::clear);
    HOST_ATTACHED.store(false, Ordering::SeqCst);
    BOOTSTRAP_FAILED.store(false, Ordering::SeqCst);

    log(
        LogLevel::Info,
        "GW2 Build Optimizer",
        format!("Addon unloaded. {}", report),
    );
    if !report.abandoned.is_empty() {
        // Detached workers keep the addon image pinned (`pin_addon_module`) so
        // Nexus `FreeLibrary` cannot unmap `.text` under them. The leftover is a
        // long-lived mapping, not the old return-into-unmapped-code crash.
        log(
            LogLevel::Warning,
            "GW2 Build Optimizer",
            format!(
                "{} background worker(s) did not stop within {} ms and were detached; the addon image stays pinned until they return.",
                report.abandoned.len(),
                state::UNLOAD_JOIN_BUDGET.as_millis()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// Pin: `state::init` (and the rest of load) must sit behind `catch_unwind`
    /// so a panic cannot cross Nexus `C-unwind`. Freeze SHA had `catch_unwind`
    /// only on the keybind nested inside `on_load`, after `state::init`.
    ///
    /// This is a source pin, not an in-process call of `on_load`: that entry
    /// talks to the Nexus API table (`get_addon_dir`, `log`, register_*), which
    /// unit tests do not have. A panic inside `load_guard`'s log path would also
    /// need that table. Leave a live load-panic on the in-game list.
    #[test]
    fn on_load_wraps_init_in_catch_unwind() {
        let src = include_str!("lib.rs");
        let start = src.find("\nfn on_load()").expect("on_load must exist");
        let rest = &src[start..];
        let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
        let body = &rest[..end];
        let init = body
            .find("state::init")
            .expect("on_load must call state::init");
        let catch = body.find("catch_unwind");
        let guard = body.find("load_guard");
        let wrapped =
            catch.map(|c| c < init).unwrap_or(false) || guard.map(|g| g < init).unwrap_or(false);
        assert!(
            wrapped,
            "state::init must run inside catch_unwind so a load panic cannot unwind into the game"
        );
        assert!(
            !body.contains("fonts::init"),
            "fonts must not register on load"
        );
        assert!(
            !body.contains("attach_overlay_host"),
            "on_load must not attach D3D chrome, even when the overlay starts visible"
        );
        assert!(
            !body.contains("get_texture") && !body.contains("add_quick_access"),
            "texture upload and quick access are D3D; they belong in attach_overlay_host"
        );
        assert!(
            body.contains("PostRender"),
            "on_load registers a PostRender bootstrap so chrome can attach after ArcDPS"
        );
        assert!(
            body.contains("RenderType::Render"),
            "on_load must register the Render hook for ui::render; registering it from PostRender mutates the very vector Nexus is iterating (heap crash)"
        );
    }

    #[test]
    fn chrome_settle_outlasts_the_observed_arcdps_race() {
        assert!(
            super::CHROME_SETTLE >= Duration::from_millis(2000),
            "crash is ~1s after load; settle must wait out that window"
        );
    }

    /// Pin: chrome attach must always run inside `catch_unwind`. A panic from
    /// `register_render` / `add_quick_access` / `get_texture_or_create_from_memory`
    /// would otherwise unwind into Nexus's `extern "C-unwind"` callback and the
    /// game. `bootstrap_chrome` owns that guard for the deferred path; the
    /// function body itself must not skip it.
    #[test]
    fn bootstrap_chrome_guards_attach_in_catch_unwind() {
        let src = include_str!("lib.rs");
        let start = src
            .find("\nfn bootstrap_chrome(")
            .expect("bootstrap_chrome must exist");
        let rest = &src[start..];
        let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("attach_overlay_host"),
            "bootstrap_chrome must call attach_overlay_host, which itself wraps D3D work in catch_unwind"
        );
        assert!(
            body.contains("attach_overlay_host"),
            "bootstrap_chrome must call attach_overlay_host"
        );
    }

    /// Pin: `attach_overlay_host` must short-circuit when already attached and
    /// must release the slot on panic, otherwise a panic inside D3D work would
    /// silently kill the overlay for the rest of the session.
    #[test]
    fn attach_overlay_host_is_idempotent_and_recovers_from_panic() {
        let src = include_str!("lib.rs");
        let start = src
            .find("\nfn attach_overlay_host(")
            .expect("attach_overlay_host must exist");
        let rest = &src[start..];
        let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("HOST_ATTACHED"),
            "attach_overlay_host must consult HOST_ATTACHED"
        );
        assert!(
            body.contains("catch_unwind"),
            "attach_overlay_host must catch_unwind so a panic cannot escape into Nexus"
        );
        assert!(
            body.contains("BOOTSTRAP_FAILED"),
            "attach_overlay_host must latch BOOTSTRAP_FAILED so the bootstrapper can observe a dead attach"
        );
    }
}
