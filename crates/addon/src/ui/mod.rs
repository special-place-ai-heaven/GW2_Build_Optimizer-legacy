pub(crate) mod fonts;

pub mod chat_bar;
pub mod chat_markup;
pub mod comparison;
pub(crate) mod cost_format;
mod gear_diff;
mod gear_sheet;
pub(crate) mod icons;
pub mod main_view;
pub(crate) mod mini_radio;
pub(crate) mod news_feed;
pub mod radar_chart;
pub(crate) mod run_feed;
mod setup;
pub(crate) mod theme;

use std::sync::Mutex;

use nexus::imgui::{
    Condition, MouseButton, MouseCursor, Ui, Window, WindowFlags, WindowHoveredFlags,
};

/// Convert RGBA `[f32;4]` (each channel 0.0–1.0) to ImGui's packed `u32` color
/// (ABGR byte order). Shared by `radar_chart` and `lock_panel` to avoid
/// duplicate definitions drifting out of sync.
pub(crate) fn color_u32(c: [f32; 4]) -> u32 {
    let r = (c[0] * 255.0).clamp(0.0, 255.0) as u32;
    let g = (c[1] * 255.0).clamp(0.0, 255.0) as u32;
    let b = (c[2] * 255.0).clamp(0.0, 255.0) as u32;
    let a = (c[3] * 255.0).clamp(0.0, 255.0) as u32;
    (a << 24) | (b << 16) | (g << 8) | r
}

thread_local! {
    static WINDOW_DRAW_LISTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The current window's draw list, counted. imgui-rs allows one live
/// `DrawListMut` per list kind in the whole process and panics on a second
/// ("already loaded"); this names the nesting first in a debug build. Hold
/// it in the narrowest block that draws and drop it before calling anything
/// that may draw; a helper that must draw in the caller's scope takes
/// `&DrawListMut` instead of acquiring. Pinned for the radio UI by
/// `mini_radio::draw_list_scan`.
pub(crate) fn window_draw_list<'ui>(ui: &'ui Ui<'ui>) -> WindowDrawList<'ui> {
    debug_assert_eq!(
        WINDOW_DRAW_LISTS.with(|d| d.get()),
        0,
        "window draw list acquired while another is live"
    );
    let dl = ui.get_window_draw_list();
    WINDOW_DRAW_LISTS.with(|d| d.set(d.get() + 1));
    WindowDrawList(dl)
}

/// A [`window_draw_list`] handle; derefs to the `DrawListMut`.
pub(crate) struct WindowDrawList<'ui>(nexus::imgui::DrawListMut<'ui>);

impl<'ui> std::ops::Deref for WindowDrawList<'ui> {
    type Target = nexus::imgui::DrawListMut<'ui>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for WindowDrawList<'_> {
    fn drop(&mut self) {
        WINDOW_DRAW_LISTS.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

use crate::state::{self, AddonState, Screen};

// Off-lock disk writes

/// Diagnostics from the write plumbing.
///
/// `nexus::log::log` needs the Nexus API table, which unit tests do not have
/// (same reason `state::worker_log` exists), so test builds go to stderr.
pub(crate) fn log_disk_error(message: String) {
    #[cfg(test)]
    eprintln!("[GW2BuildOpt] {}", message);
    #[cfg(not(test))]
    nexus::log::log(nexus::log::LogLevel::Warning, "GW2BuildOpt", message);
}

/// An already-serialized write, owning everything it needs.
type WriteJob = Box<dyn FnOnce() + Send + 'static>;

/// One file that the overlay rewrites whole, saved by a background worker
/// instead of on the render thread.
///
/// Two reasons this exists. The render callback is the game's only draw pass
/// and it runs with `STATE` held, so an `fs::write` there stalls the frame
/// *and* every background worker queued to publish a result behind that lock.
/// And every saver in this addon publishes through one fixed `<name>.tmp` plus
/// a rename, so two savers of the same file at the same moment would fight
/// over that single staging path.
///
/// So: at most one worker per file, always writing the newest content. A save
/// requested while one is running replaces the queued content instead of
/// racing it — these are whole-file writes, so last-one-wins is the correct
/// merge, and the caller can clear its dirty flag the moment it submits.
pub(crate) struct SerialWriter {
    /// Worker name (also the label in write-failure logs).
    label: &'static str,
    queue: Mutex<WriteQueue>,
}

#[derive(Default)]
struct WriteQueue {
    /// Newest content not yet written. Replaced, never appended to.
    pending: Option<WriteJob>,
    /// True while a worker is draining `pending`. Read and written only under
    /// the queue mutex, so the drain loop and [`SerialWriter::submit`] can
    /// never disagree about whether someone will pick a queued job up.
    draining: bool,
}

impl SerialWriter {
    pub(crate) const fn new(label: &'static str) -> Self {
        Self {
            label,
            queue: Mutex::new(WriteQueue {
                pending: None,
                draining: false,
            }),
        }
    }

    /// Poison-tolerant lock: a job that panicked runs outside this guard, but a
    /// poisoned mutex must not silently stop saving for the rest of the session.
    fn lock(&self) -> std::sync::MutexGuard<'_, WriteQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue `job` and make sure exactly one worker is draining this file.
    ///
    /// Never blocks on disk and never touches `STATE`, so it is safe to call
    /// from inside `with_state` during a frame — which is where every call site
    /// is. Lock order stays STATE → queue → worker registry.
    pub(crate) fn submit(&'static self, state: &AddonState, job: impl FnOnce() + Send + 'static) {
        let start_worker = {
            let mut queue = self.lock();
            queue.pending = Some(Box::new(job));
            let idle = !queue.draining;
            queue.draining = true;
            idle
        };
        if !start_worker {
            return;
        }
        if !state.spawn_worker(self.label, move |_token| self.drain()) {
            // The OS refused a thread. Losing the player's settings is worse
            // than one stalled frame, so write on this thread instead.
            self.drain();
        }
    }

    /// Write queued content until nothing is left, then hand the file back.
    ///
    /// Deliberately ignores the cancellation token: these are the player's own
    /// settings, chat history and message log, and each job is one small file
    /// write. Unload cancels and then waits, so dropping the write to save a
    /// few milliseconds would just lose data the player can see.
    fn drain(&self) {
        loop {
            let job = {
                let mut queue = self.lock();
                match queue.pending.take() {
                    Some(job) => job,
                    None => {
                        // Cleared under the same lock `submit` takes, so a job
                        // queued from here on starts a fresh worker rather than
                        // waiting on one that is already on its way out.
                        queue.draining = false;
                        return;
                    }
                }
            };
            // Outside the lock: the write is the slow part. The guard is for the
            // flag, not for containment — a panicking job must not leave
            // `draining` stuck true and swallow every later save in silence.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                log_disk_error(format!("disk write panicked: {}", self.label));
            }
        }
    }
}

/// `config.json`. Written from several places — window move, close, snap, a
/// finished data refresh — none of which should queue behind another's disk
/// write, and only the newest of which is worth writing.
static CONFIG_WRITES: SerialWriter = SerialWriter::new("config-save");

/// Persist the current config without holding `STATE`.
///
/// Callers are on the render thread inside `with_state`; this snapshots the
/// config and hands the write to a tracked worker, so the frame never waits on
/// disk and no background worker waits on the frame.
///
/// Settings and News persist through this path. Setup and the keybind handler
/// still save synchronously. `AppConfig::save` stages through a private
/// per-save file rather than one shared `config.tmp`, so two detached writes
/// can overlap: the rename is the only step they share, and it is atomic.
pub(crate) fn save_config_detached(state: &AddonState) {
    let config = state.config.clone();
    let path = state.config_path.clone();
    CONFIG_WRITES.submit(state, move || {
        if let Err(e) = config.save(&path) {
            log_disk_error(format!("config save failed: {e}"));
        }
    });
}

/// True when the overlay has little usable area on the game framebuffer
/// (imgui.ini parked it past the right edge, or it was stretched wider than
/// the display).
pub(crate) fn window_needs_snap(pos: [f32; 2], size: [f32; 2], display: [f32; 2]) -> bool {
    let x0 = pos[0].max(0.0);
    let y0 = pos[1].max(0.0);
    let x1 = (pos[0] + size[0]).min(display[0]);
    let y1 = (pos[1] + size[1]).min(display[1]);
    let vis_w = (x1 - x0).max(0.0);
    let vis_h = (y1 - y0).max(0.0);
    if vis_w < 120.0 || vis_h < 80.0 {
        return true;
    }
    let vis_area = vis_w * vis_h;
    let total = size[0].max(1.0) * size[1].max(1.0);
    vis_area < total * 0.75
}

/// First-run / reset apply when size is missing or the player forced a snap.
/// Pre-1.7.22 800×600 is a normal persisted size, not a migrate trigger.
fn window_needs_default_size(unset: bool, force_snap: bool) -> bool {
    unset || force_snap
}

pub fn render(ui: &Ui) {
    // Before the visibility check and outside `with_state`: a copy that lost the
    // race for the clipboard must still land if the player closed the overlay
    // right after clicking, and the retry must never run under the state lock.
    crate::clipboard::pump();
    // Also before the visibility check: combat ducking must keep working with
    // the overlay closed. Render-thread-only; locks STATE itself, so it must
    // stay outside `with_state`.
    crate::radio::player::duck_tick();
    // One STATE lock decides both windows: a closed overlay with a hidden
    // strip costs this lock and nothing else.
    let display = ui.io().display_size;
    let Some((main_open, mini)) = state::with_state(|s| {
        let main_open = s.window_visible;
        (main_open, mini_radio_frame(s, main_open, display))
    }) else {
        return;
    };
    // Outside the main window's early return: the mini radio is the one thing
    // drawn while the overlay is closed.
    if let Some(mini) = mini {
        render_mini_radio(ui, mini);
    }
    if !main_open {
        return;
    }

    let display = ui.io().display_size;
    let (snap, pos, size, opacity, ui_font, ui_lang) = state::with_state(|s| {
        let snap = s.force_window_pos;
        s.force_window_pos = false;
        let unset = s.config.window_w.is_none() || s.config.window_h.is_none();
        let apply = window_needs_default_size(unset, snap);
        if apply {
            let size = gw2_core::config::initial_window_size(display);
            let pos = if snap {
                gw2_core::config::DEFAULT_WINDOW_POS
            } else {
                s.config.window_rect().0
            };
            s.config.set_window_rect(pos, size);
            save_config_detached(s);
        }
        let (pos, size) = s.config.window_rect();
        (
            apply,
            pos,
            size,
            s.config.window_opacity,
            s.config.ui_font.clone(),
            s.config.ui_language.clone(),
        )
    })
    .unwrap_or((
        false,
        gw2_core::config::DEFAULT_WINDOW_POS,
        gw2_core::config::DEFAULT_WINDOW_SIZE,
        1.0,
        String::from("auto"),
        String::from("auto"),
    ));

    // Catch panics inside the ImGui frame so a bug in any render path
    // doesn't unwind through Nexus' C-unwind FFI boundary. A panic mid-frame
    // also leaves the ImGui stack unbalanced (open `begin` without `end`),
    // which the next frame would inherit and corrupt — `catch_unwind` here
    // protects only this addon's draw calls, but that's enough to keep the
    // rest of the game's ImGui state intact.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _theme = theme::push(ui, opacity);
        fonts::init(&ui_font, &ui_lang);
        fonts::init_italic(&ui_font, &ui_lang);
        fonts::init_ticker();
        let _font = fonts::push(&ui_font, &ui_lang);
        let mut opened = true;
        let cond = if snap {
            Condition::Always
        } else {
            Condition::Appearing
        };
        Window::new("GW2 Build Optimizer")
            .opened(&mut opened)
            .flags(WindowFlags::NO_SAVED_SETTINGS)
            .size_constraints(gw2_core::config::MIN_WINDOW_SIZE, [99999.0, 99999.0])
            .collapsed(false, cond)
            .position(pos, cond)
            .size(size, cond)
            .build(ui, || {
                state::with_state(|s| {
                    if window_needs_snap(ui.window_pos(), ui.window_size(), ui.io().display_size) {
                        s.force_window_pos = true;
                    }
                    if !ui.is_window_collapsed() && !ui.is_mouse_down(MouseButton::Left) {
                        let p = ui.window_pos();
                        let sz = ui.window_size();
                        let (old_p, old_sz) = s.config.window_rect();
                        if (p[0] - old_p[0]).abs() > 0.5
                            || (p[1] - old_p[1]).abs() > 0.5
                            || (sz[0] - old_sz[0]).abs() > 0.5
                            || (sz[1] - old_sz[1]).abs() > 0.5
                        {
                            s.config.set_window_rect(p, sz);
                            save_config_detached(s);
                        }
                    }
                    gw2_core::i18n::set_language(&s.config.ui_language);
                    match &s.screen {
                        Screen::Setup(step) => {
                            setup::render_setup(ui, s, step.clone());
                        }
                        Screen::Main => {
                            main_view::render_main(ui, s);
                        }
                    }
                    // Last write wins: buttons/selectables/pills set Hand while hovered.
                    // Nexus maps that to GW2's gloved click cursor. Pin arrow after all widgets,
                    // but only while the mouse is over this overlay so the world cursor stays intact.
                    if ui.is_window_hovered_with_flags(WindowHoveredFlags::ROOT_AND_CHILD_WINDOWS)
                        || ui.is_any_item_hovered()
                    {
                        ui.set_mouse_cursor(Some(MouseCursor::Arrow));
                    }
                });
            });
        if !opened {
            state::with_state(|s| {
                s.window_visible = false;
                s.config.window_visible = false;
                save_config_detached(s);
            });
        }
    }));
    if outcome.is_err() {
        nexus::log::log(
            nexus::log::LogLevel::Warning,
            "GW2BuildOpt",
            "Render panicked — skipping this frame. See debugger or logs for details.",
        );
    }
}

/// The mini radio window. Same shape as the main window below: this function
/// takes STATE, and the window body gets `&mut AddonState` handed in, so
/// nothing in `mini_radio` ever locks it.
///
/// Visibility is decided under the lock every frame (toggle on, overlay
/// closed, not unloading). The strip holds no state of its own beyond
/// `AddonState`, so it keeps working across the overlay opening and closing
/// and across a game-data refresh.
/// Per-frame inputs of the mini radio window, decided under the one lock in
/// [`render`].
struct MiniFrame {
    pos: [f32; 2],
    size: [f32; 2],
    snap: bool,
    ui_font: String,
    ui_lang: String,
    /// Show/hide transition alpha, 0..1.
    fade: f32,
    /// Fading out: still drawn, takes no clicks.
    leaving: bool,
    /// Pinned in place with the mouse passing through.
    anchored: bool,
}

/// `None` when the strip is hidden this frame. Showing and hiding fade
/// (`mini_radio::Fade`); unloading hides at once.
fn mini_radio_frame(s: &mut AddonState, main_open: bool, display: [f32; 2]) -> Option<MiniFrame> {
    let unloading = s.cancel_token.is_cancelled();
    let want = mini_radio::visible(s.config.radio.mini_radio.enabled, main_open, unloading);
    let now = theme::elapsed_ms();
    s.radio.mini_fade = if unloading {
        mini_radio::Fade::default()
    } else {
        s.radio.mini_fade.toward(want, now)
    };
    let fade = s.radio.mini_fade;
    // Not before the display is known: clamping against 1x1 put the strip at
    // (0, 0). `mini_display` stays stale, so the first real display snaps
    // the strip to its saved rect.
    if !fade.drawn(now) || !mini_radio::display_valid(display) {
        s.radio.mini_press = None;
        return None;
    }
    let reset = std::mem::take(&mut s.radio.mini_snap);
    let snap = mini_radio::needs_replace(reset, s.radio.mini_display, display);
    s.radio.mini_display = display;
    if snap {
        // A programmatic move is not the player's drag: never saved.
        s.radio.mini_live_w = None;
        s.radio.mini_press = None;
    }
    let (pos, size) = mini_radio::placement(&s.config.radio.mini_radio, display);
    // The live width wins only while a corner drag is in flight.
    let w = s.radio.mini_live_w.unwrap_or(size[0]);
    Some(MiniFrame {
        pos,
        size: [w, mini_radio::height_for(w)],
        snap,
        ui_font: s.config.ui_font.clone(),
        ui_lang: s.config.ui_language.clone(),
        fade: fade.alpha(now),
        leaving: !fade.showing,
        anchored: s.config.radio.mini_radio.anchored,
    })
}

fn render_mini_radio(ui: &Ui, frame: MiniFrame) {
    let MiniFrame {
        pos,
        size,
        snap,
        ui_font,
        ui_lang,
        fade,
        leaving,
        anchored,
    } = frame;

    // Same unwind guard as the main window: nothing may cross the FFI edge.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _theme = theme::push(ui, 1.0);
        let _pad = ui.push_style_var(nexus::imgui::StyleVar::WindowPadding([6.0, 4.0]));
        let _border = ui.push_style_var(nexus::imgui::StyleVar::WindowBorderSize(0.0));
        fonts::init(&ui_font, &ui_lang);
        fonts::init_ticker();
        let _font = fonts::push(&ui_font, &ui_lang);
        let pos_cond = if snap {
            Condition::Always
        } else {
            Condition::Appearing
        };
        let (mut flags, _) = mini_radio::window_flags(anchored);
        if leaving {
            flags |= WindowFlags::NO_INPUTS;
        }
        Window::new("##gw2bo_mini_radio")
            .flags(flags)
            .bg_alpha(0.0)
            .size_constraints(
                [mini_radio::MIN_W, mini_radio::height_for(mini_radio::MIN_W)],
                [mini_radio::MAX_W, mini_radio::height_for(mini_radio::MAX_W)],
            )
            .position(pos, pos_cond)
            // Height follows width every frame; only the width is free.
            .size(size, Condition::Always)
            .build(ui, || {
                state::with_state(|s| {
                    gw2_core::i18n::set_language(&s.config.ui_language);
                    mini_radio::render_window(ui, s, fade, leaving);
                    if ui.is_window_hovered_with_flags(WindowHoveredFlags::ROOT_AND_CHILD_WINDOWS)
                        || ui.is_any_item_hovered()
                    {
                        ui.set_mouse_cursor(Some(MouseCursor::Arrow));
                    }
                });
            });
    }));
    if outcome.is_err() {
        nexus::log::log(
            nexus::log::LogLevel::Warning,
            "GW2BuildOpt",
            "Mini radio render panicked - skipping this frame.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{window_needs_default_size, window_needs_snap};

    /// Game start: the first frames report a 0x0 or 1x1 display, then
    /// the real one. The strip must not show until then, must then sit at
    /// the saved rect, and nothing may be written back (the old path placed
    /// it at (0, 0) at minimum width and saved that).
    #[test]
    fn mini_radio_startup_uses_the_saved_rect_and_writes_nothing() {
        use super::mini_radio;
        use gw2_core::config::AppConfig;

        let _serial = crate::state::state_test_guard();
        let dir = std::env::temp_dir().join(format!("gw2_ui_mini_start_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = AppConfig::config_path(&dir);
        let (pos, size) = ([900.0, 1100.0], [700.0, mini_radio::height_for(700.0)]);
        let mut cfg = AppConfig::default();
        cfg.radio.mini_radio.enabled = true;
        cfg.radio.mini_radio.pos = Some(pos);
        cfg.radio.mini_radio.size = Some(size);
        cfg.save(&path).unwrap();
        let on_disk = std::fs::read(&path).unwrap();

        crate::state::clear();
        crate::state::init(dir.clone());
        let (frames, prefs, press) = crate::state::with_state(|s| {
            let frames = [[0.0, 0.0], [1.0, 1.0], [2560.0, 1440.0], [2560.0, 1440.0]]
                .map(|d| super::mini_radio_frame(s, false, d).map(|f| (f.pos, f.size, f.snap)));
            // The window now sits at the placed rect, mouse up: what the
            // drag tracker would save.
            let (press, save) =
                mini_radio::drag_step(s.radio.mini_press, false, false, (pos, size));
            assert_eq!(save, None);
            (frames, s.config.radio.mini_radio.clone(), press)
        })
        .expect("state must be initialised");
        crate::state::clear();

        assert_eq!(frames[0], None, "no strip against a 0x0 display");
        assert_eq!(frames[1], None, "nor against 1x1, which clamped to (0, 0)");
        assert_eq!(
            frames[2],
            Some((pos, size, true)),
            "first real display: saved rect"
        );
        assert_eq!(frames[3], Some((pos, size, false)));
        assert_eq!(press, None);
        assert_eq!((prefs.pos, prefs.size), (Some(pos), Some(size)));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            on_disk,
            "config.json untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window rect, "overlay closed", and the build number a finished data
    /// refresh writes are all saved from inside `with_state`, on the render
    /// thread. This proves the write actually leaves that thread: a tracked
    /// worker is started, and `config.json` lands on disk **while the STATE
    /// mutex is still held**. An `AppConfig::save` under the lock would give
    /// `worker_count() == 0`; a worker that published through `with_state`
    /// would block on the very lock this test is holding and never write.
    #[test]
    fn config_save_not_under_lock() {
        use gw2_core::config::AppConfig;
        use std::time::{Duration, Instant};

        let _serial = crate::state::state_test_guard();
        let dir = std::env::temp_dir().join(format!("gw2_ui_config_save_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = AppConfig::config_path(&dir);

        crate::state::clear();
        crate::state::init(dir.clone());
        assert!(
            !path.exists(),
            "the test must start with no config.json, so its appearance is the proof"
        );

        let (workers, landed) = crate::state::with_state(|s| {
            // Not the default, so a file left by another test cannot pass this.
            s.config.window_opacity = 0.42;
            super::save_config_detached(s);
            // Everything below still runs inside the closure: STATE is held.
            let workers = s.worker_count();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut landed = false;
            while Instant::now() < deadline {
                if path.exists() {
                    landed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            (workers, landed)
        })
        .expect("state must be initialised");

        assert!(
            workers >= 1,
            "the save must run on a tracked worker, not inline on the render thread"
        );
        assert!(
            landed,
            "config.json must be written while the render thread still holds STATE"
        );

        let saved = std::fs::read_to_string(&path).expect("config.json is readable");
        assert!(
            saved.contains("0.42"),
            "the submitted snapshot is what landed, got: {saved}"
        );

        crate::state::clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parked_at_right_edge_of_1080p_needs_snap() {
        assert!(window_needs_snap(
            [1880.0, 293.0],
            [800.0, 600.0],
            [1920.0, 1080.0]
        ));
    }

    #[test]
    fn stretched_wider_than_remaining_1080p_needs_snap() {
        assert!(window_needs_snap(
            [1159.0, 181.0],
            [1840.0, 867.0],
            [1920.0, 1080.0]
        ));
    }

    #[test]
    fn default_corner_on_1080p_stays() {
        assert!(!window_needs_snap(
            [80.0, 80.0],
            [800.0, 600.0],
            [1920.0, 1080.0]
        ));
    }

    #[test]
    fn window_init_is_only_missing_size_or_forced_snap() {
        assert!(window_needs_default_size(true, false));
        assert!(window_needs_default_size(false, true));
        assert!(!window_needs_default_size(false, false));
    }

    #[test]
    fn ultrawide_keeps_rightish_window() {
        assert!(!window_needs_snap(
            [1880.0, 293.0],
            [800.0, 600.0],
            [3440.0, 1440.0]
        ));
    }

    #[test]
    fn settings_and_news_saves_are_detached() {
        let settings = include_str!("main_view/tabs/settings.rs");
        let news = include_str!("main_view/tabs/news.rs");
        assert!(
            !settings.contains("let _ = state.config.save(&state.config_path)"),
            "settings.rs still swallows a sync config save"
        );
        assert!(
            !news.contains("let _ = state.config.save(&state.config_path)"),
            "news.rs still swallows a sync config save"
        );
        assert!(
            settings.contains("crate::ui::save_config_detached(state)"),
            "settings.rs must persist via save_config_detached"
        );
        assert!(
            news.contains("crate::ui::save_config_detached(state)"),
            "news.rs must persist via save_config_detached"
        );
    }
}
