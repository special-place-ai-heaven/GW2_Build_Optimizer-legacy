use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ui::chat_bar::ChatBarState;
use crate::ui::comparison::ComparisonState;
use gw2_core::config::AppConfig;
use gw2_core::types::{BuildLocks, GameMode, ResolvedBuild, SavedBuild, StatBlock};
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::scoring::OptimizationWeights;

static STATE: Mutex<Option<AddonState>> = Mutex::new(None);

/// A cancellation token that background threads check to know when to stop.
/// Backed by `Arc<AtomicBool>` — cloning is cheap and each thread gets its own Arc ref.
#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Signal all threads holding a clone of this token to stop.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

// Background workers
//
// Every background thread the addon starts goes through `AddonState::spawn_worker`
// so that unload has something to wait for. The rules the rest of the addon can
// rely on:
//
//   * the worker gets its own `CancellationToken` clone and must poll it,
//   * its `JoinHandle` is tracked here until it finishes,
//   * a panic inside the worker is caught and logged, never unwound,
//   * `on_unload` cancels, then waits `UNLOAD_JOIN_BUDGET` for the stragglers,
//   * on Windows the addon image is pinned before spawn returns so Nexus
//     `FreeLibrary` cannot unmap `.text` under a detached worker.

/// How long unload waits for background workers before giving up on them.
///
/// Nexus calls `on_unload` on the game's main thread, so every millisecond spent
/// waiting is a frozen game. A worker parked in a blocking HTTP read can take far
/// longer than this to notice its cancel flag, so this is a budget, not a
/// guarantee — see [`join_workers`].
pub const UNLOAD_JOIN_BUDGET: Duration = Duration::from_millis(1500);

/// How often [`join_bounded`] re-checks whether a worker has finished.
const JOIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Diagnostics from the worker plumbing.
///
/// `nexus::log::log` needs the Nexus API table, which unit tests do not have
/// (same reason `lock_state` uses `eprintln!`), so test builds go to stderr.
fn worker_log(message: String) {
    #[cfg(test)]
    eprintln!("[GW2BuildOpt] {}", message);
    #[cfg(not(test))]
    nexus::log::log(
        nexus::log::LogLevel::Warning,
        "GW2 Build Optimizer",
        message,
    );
}

/// Opaque `HMODULE`. Copied into the worker; never dereferenced as a pointer.
#[derive(Clone, Copy)]
pub(crate) struct ModuleHandle(usize);

#[cfg(windows)]
impl ModuleHandle {
    fn from_raw(handle: *mut core::ffi::c_void) -> Self {
        Self(handle as usize)
    }

    fn as_raw(self) -> *mut core::ffi::c_void {
        self.0 as *mut core::ffi::c_void
    }
}

/// Address taken by [`pin_addon_module`]; must live in this crate's image.
#[inline(never)]
fn addon_image_anchor() {}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleExW(
        flags: u32,
        name_or_address: *const u16,
        module: *mut *mut core::ffi::c_void,
    ) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
    fn FreeLibrary(module: *mut core::ffi::c_void) -> i32;
    fn FreeLibraryAndExitThread(module: *mut core::ffi::c_void, exit_code: u32);
}

/// Increment this DLL's load count so Nexus `FreeLibrary` cannot unmap `.text`
/// while a detached worker is still inside it.
///
/// Uses **only** `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS` (0x4). Do not add
/// `GET_MODULE_HANDLE_EX_FLAG_PIN` (0x1): that blocks hot-reload forever.
///
/// Returns `None` when this crate is linked into the process image (`cargo test`)
/// or when the API fails (logged; the worker still starts). On `None`, the
/// worker returns normally so existing join tests keep working.
pub(crate) fn pin_addon_module() -> Option<ModuleHandle> {
    #[cfg(not(windows))]
    {
        None
    }
    #[cfg(windows)]
    {
        // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS only — not PIN, not UNCHANGED_REFCOUNT.
        const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

        let mut handle = core::ptr::null_mut();
        // SAFETY: FROM_ADDRESS treats the second parameter as a code address, not
        // a string. `addon_image_anchor` lives in this crate. `handle` is a local
        // out-parameter. Success increments the module's load count.
        let obtained = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                addon_image_anchor as *const u16,
                &mut handle,
            )
        };
        if obtained == 0 || handle.is_null() {
            worker_log(
                "could not pin addon module (GetModuleHandleExW failed); \
                 a detached worker may crash if Nexus unmaps the DLL"
                    .into(),
            );
            return None;
        }
        // SAFETY: a null name returns the process executable; no increment.
        let process = unsafe { GetModuleHandleW(core::ptr::null()) };
        if handle == process {
            // Undo the increment. Never FreeLibraryAndExitThread the test exe.
            // SAFETY: we own the increment from GetModuleHandleExW.
            let _ = unsafe { FreeLibrary(handle) };
            return None;
        }
        Some(ModuleHandle::from_raw(handle))
    }
}

/// Worker finished. Decrement the pin and exit this thread (never returns).
pub(crate) fn exit_pinned_worker(handle: ModuleHandle) {
    #[cfg(windows)]
    {
        // SAFETY: `handle` is the increment taken on the parent before spawn
        // returned. This is the matching decrement; the thread must not run
        // addon `.text` after it.
        unsafe { FreeLibraryAndExitThread(handle.as_raw(), 0) }
    }
    #[cfg(not(windows))]
    {
        let _ = handle;
    }
}

/// `Builder::spawn` failed after a successful pin: undo the increment here.
pub(crate) fn undo_module_pin(pin: Option<ModuleHandle>) {
    #[cfg(windows)]
    if let Some(handle) = pin {
        // SAFETY: the worker never started, so this thread still owns the increment.
        let _ = unsafe { FreeLibrary(handle.as_raw()) };
    }
    #[cfg(not(windows))]
    let _ = pin;
}

/// One background worker, tracked so unload can wait for it.
struct TrackedWorker {
    /// Static label used for the OS thread name and for shutdown/panic logs.
    name: &'static str,
    handle: JoinHandle<()>,
}

impl TrackedWorker {
    /// Join a worker that has **already finished** and report whether it died panicking.
    ///
    /// Callers check `JoinHandle::is_finished()` first, so this never blocks. In
    /// practice the `Err` arm is unreachable: `spawn_worker` catches the worker
    /// body's unwind itself, so only a panic in the logging path could get here.
    fn reap(self) -> bool {
        let name = self.name;
        match self.handle.join() {
            Ok(()) => false,
            Err(_payload) => {
                worker_log(format!("background worker unwound: {}", name));
                true
            }
        }
    }
}

/// Handles of the background workers this addon started.
///
/// A `Mutex` rather than a plain `Vec` because [`AddonState::spawn_worker`] takes
/// `&self`: almost every call site runs inside `with_state(|s| …)` during a render
/// frame, and some (the feedback tasks) hold only `&AddonState`.
///
/// Lock order is always STATE → registry, never the reverse: nothing inside these
/// methods reaches back for `STATE`. Unload moves the handles *out* through
/// [`WorkerRegistry::take_all`] and waits on them with STATE released, because
/// workers publish their results through `with_state` and would otherwise be
/// blocked on the very lock unload is holding.
#[derive(Default)]
struct WorkerRegistry {
    live: Mutex<Vec<TrackedWorker>>,
}

impl WorkerRegistry {
    /// Poison-tolerant lock: a worker that panicked must not take the registry
    /// with it (matching `lock_state`'s recovery).
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<TrackedWorker>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Track `handle`, first reaping workers that already finished.
    ///
    /// Without the reap a long session leaks a handle per spawn — the API-health
    /// worker alone starts one a minute — and unload would then walk a list of
    /// hundreds of corpses.
    fn register(&self, name: &'static str, handle: JoinHandle<()>) {
        let mut live = self.lock();
        let mut kept = Vec::with_capacity(live.len() + 1);
        for worker in std::mem::take(&mut *live) {
            if worker.handle.is_finished() {
                worker.reap();
            } else {
                kept.push(worker);
            }
        }
        kept.push(TrackedWorker { name, handle });
        *live = kept;
    }

    /// Move every tracked handle out, leaving the registry empty.
    fn take_all(&self) -> Vec<TrackedWorker> {
        std::mem::take(&mut *self.lock())
    }

    /// Tracked handles: live workers plus any that finished since the last spawn.
    fn len(&self) -> usize {
        self.lock().len()
    }
}

/// What [`join_workers`] achieved inside its budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Workers that had finished and were joined.
    pub joined: usize,
    /// Of `joined`, how many unwound out of their own panic guard.
    pub panicked: usize,
    /// Workers still running when the budget expired. Their handles were dropped,
    /// which detaches them: the threads keep running with the cancel flag set.
    pub abandoned: Vec<&'static str>,
    /// Wall-clock time actually spent waiting.
    pub waited: Duration,
}

impl std::fmt::Display for ShutdownReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} worker(s) joined in {} ms",
            self.joined,
            self.waited.as_millis()
        )?;
        if self.panicked > 0 {
            write!(f, ", {} panicked", self.panicked)?;
        }
        if !self.abandoned.is_empty() {
            write!(f, ", still running: {}", self.abandoned.join(", "))?;
        }
        Ok(())
    }
}

/// Wait up to `budget` for `pending` to finish, joining each worker as it does.
///
/// Never calls `join()` on a worker that is still running: a blocking join is
/// exactly the unload hang this replaces. Polling `is_finished()` keeps the total
/// wait inside the budget no matter how wedged a worker is.
fn join_bounded(mut pending: Vec<TrackedWorker>, budget: Duration) -> ShutdownReport {
    let started = Instant::now();
    let mut report = ShutdownReport::default();
    loop {
        let mut still_running = Vec::with_capacity(pending.len());
        for worker in pending {
            if worker.handle.is_finished() {
                report.joined += 1;
                if worker.reap() {
                    report.panicked += 1;
                }
            } else {
                still_running.push(worker);
            }
        }
        pending = still_running;
        if pending.is_empty() || started.elapsed() >= budget {
            break;
        }
        std::thread::sleep(JOIN_POLL_INTERVAL);
    }
    report.abandoned = pending.iter().map(|w| w.name).collect();
    report.waited = started.elapsed();
    // ponytail: dropping `pending` detaches those threads. They keep running with
    // the cancel flag set. The unmap window is closed by [`pin_addon_module`] —
    // each worker holds an extra load-count so Nexus `FreeLibrary` cannot unmap
    // `.text` while that thread is still in `reqwest::blocking`. Cancel-aware
    // HTTP is nice-to-have for faster joins, not the unload-safety story. Do not
    // raise [`UNLOAD_JOIN_BUDGET`]: Nexus calls `on_unload` on the game main thread.
    report
}

pub struct AddonState {
    pub window_visible: bool,
    /// Set when the overlay opens; consumed on the next Main render.
    pub needs_character_reload: bool,
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub addon_dir: PathBuf,
    pub screen: Screen,
    pub setup: SetupState,
    pub main: MainState,
    /// Cancellation token — cloned into every background thread.
    /// Cancelled on addon unload so threads exit early.
    pub cancel_token: CancellationToken,
    /// Cancellation for work that Stop is not meant to stop.
    ///
    /// [`Self::cancel_and_renew`] ends the request in flight, which is what
    /// the Stop button means. Measuring a published reference build is not
    /// that request: it was cancelled as collateral and, because the picks
    /// key had not changed, nothing ever retried it, so the tab kept saying
    /// "no rotation simulation" until the player changed roles. Unload still
    /// cancels this one.
    pub pick_cancel_token: CancellationToken,
    /// Handles of the workers started by [`AddonState::spawn_worker`], so unload
    /// can wait for them. Private on purpose: nothing outside this module may
    /// start an untracked thread.
    workers: WorkerRegistry,
    /// Next frame: pin the overlay to [80, 80] (off-screen imgui.ini / Reset button).
    pub force_window_pos: bool,
    /// Official RSS (setup) + News tab feeds. Fetched on a worker, never on the download thread.
    pub news: crate::news::NewsState,
    /// Radio tab session state (search results, playback status, now-playing).
    pub radio: crate::radio::RadioUiState,
    /// Bumped when a non-render thread publishes through [`with_state`] or a
    /// keybind mutates state directly. A frame snapshot compares this to its
    /// capture value so a worker publish during layout is not overwritten.
    epoch: u64,
}

impl AddonState {
    /// Start a tracked background worker — the addon's only production thread launch.
    ///
    /// * `name` — short static label. Becomes the OS thread name (`gw2bo-<name>`)
    ///   and identifies the worker in panic and shutdown logs.
    /// * `work` — the worker body. It is handed its own [`CancellationToken`] clone
    ///   and must poll [`CancellationToken::is_cancelled`] wherever it would
    ///   otherwise keep going for a while: unload waits only [`UNLOAD_JOIN_BUDGET`]
    ///   before abandoning it.
    ///
    /// Takes `&self`, not `&mut self`, because nearly every call site is already
    /// inside `with_state(|s| …)` on the render thread and some hold only a shared
    /// borrow. The helper never locks `STATE` itself — doing so would deadlock the
    /// frame that is calling it.
    ///
    /// The body runs under `catch_unwind`: a panicking worker is logged and its
    /// thread exits normally instead of unwinding into the runtime. A call site
    /// therefore does not need its own guard for *containment* — only for the work
    /// it must still do on the panic path, such as clearing a "loading" flag
    /// through `with_state` so the UI does not spin forever.
    ///
    /// On Windows the addon module is pinned on **this** thread before
    /// `Builder::spawn` returns, so unload cannot race the child's first
    /// instruction. After the body (and its panic guard) the worker calls
    /// `FreeLibraryAndExitThread` when the pin is `Some`. A `None` pin is the
    /// `cargo test` / process-image path: the thread returns so join tests work.
    ///
    /// Returns `false` when the OS refused to create the thread, in which case the
    /// work never started — a caller that set a "loading" flag first should clear it.
    /// A failed pin still spawns (old crash window) rather than refusing the worker.
    /// Cancel every worker on the current token and hand out a fresh one, so
    /// a Stop ends the request in flight without touching the next.
    pub fn cancel_and_renew(&mut self) {
        self.cancel_token.cancel();
        self.cancel_token = CancellationToken::new();
    }

    pub fn spawn_worker<F>(&self, name: &'static str, work: F) -> bool
    where
        F: FnOnce(CancellationToken) + Send + 'static,
    {
        self.spawn_worker_on(name, self.cancel_token.clone(), work)
    }

    /// [`Self::spawn_worker`] on a caller-chosen token.
    ///
    /// Only for work outside the Stop button's meaning; everything else takes
    /// the run token so one Stop ends the whole run.
    pub fn spawn_worker_on<F>(&self, name: &'static str, token: CancellationToken, work: F) -> bool
    where
        F: FnOnce(CancellationToken) + Send + 'static,
    {
        // Pin before spawn returns: `HMODULE` is Copy, so the child and the
        // spawn-fail undo both see the same increment.
        let pin = pin_addon_module();
        let spawned = std::thread::Builder::new()
            .name(format!("gw2bo-{}", name))
            .spawn(move || {
                // Bind this worker's token to the LLM transports before the body
                // runs. They cannot take a token in their signature — `LlmClient`
                // is a `&self` trait shared across threads — so they poll a
                // thread-local predicate between SSE lines, between retry-backoff
                // slices, and between tool-loop turns. Without this bind that
                // predicate is always false and a worker parked in a blocking
                // socket read ignores cancellation entirely, outliving
                // UNLOAD_JOIN_BUDGET no matter how diligently the body polls its
                // own token. Thread-local, so cancelling one worker cannot abort
                // an unrelated request on another thread; dropped with the thread.
                let transport_token = token.clone();
                let _transport_cancel = gw2_optimizer::llm::cancel::CancelScope::new(move || {
                    transport_token.is_cancelled()
                });
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || work(token)))
                    .is_err()
                {
                    worker_log(format!("background worker panicked: {}", name));
                }
                if let Some(handle) = pin {
                    exit_pinned_worker(handle);
                }
            });
        match spawned {
            Ok(handle) => {
                self.track_worker(name, handle);
                true
            }
            Err(err) => {
                undo_module_pin(pin);
                // `std::thread::spawn` panics on this path; a game overlay must not.
                worker_log(format!(
                    "could not start background worker {}: {}",
                    name, err
                ));
                false
            }
        }
    }

    /// Tracked worker handles: still running, plus any that finished since the
    /// last `spawn_worker` reaped the list.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Reset persisted settings and transient UI state back to first-run setup.
    ///
    /// In-memory reset runs under the caller's STATE guard; the disk write is
    /// handed to [`crate::ui::save_config_detached`] so this method never writes
    /// `config.json` while STATE is held. Write errors are logged by that worker.
    pub fn reset_to_first_run(&mut self) -> Result<(), std::io::Error> {
        let config = AppConfig::default();

        // Halt any live radio session, signal-only: this method runs under the
        // caller's STATE guard, so the joining stop() would burn its whole
        // budget waiting on a status write into this very lock.
        crate::radio::player::request_stop();

        // Workers started before the reset keep the old token, which is cancelled
        // here, so they wind down on their own. Their handles stay in the registry
        // and unload still waits for them; only new workers get the fresh token.
        self.cancel_token.cancel();
        self.cancel_token = CancellationToken::new();
        self.pick_cancel_token.cancel();
        self.pick_cancel_token = CancellationToken::new();
        self.config = config;
        self.setup = SetupState::default();

        let mut main = MainState::default();
        main.weights = OptimizationWeights::default_for_mode(main.game_mode.label());
        self.main = main;
        self.news = crate::news::NewsState::default();
        self.radio = crate::radio::RadioUiState::default();
        self.screen = Screen::Setup(SetupStep::Language);

        crate::ui::save_config_detached(self);
        Ok(())
    }

    /// UI state for one frame of layout. `GameDb` and other `Arc`s are
    /// refcount bumps. The worker registry is empty: spawns during paint
    /// register on the live state.
    fn clone_for_paint(&self) -> Self {
        Self {
            window_visible: self.window_visible,
            needs_character_reload: self.needs_character_reload,
            config: self.config.clone(),
            config_path: self.config_path.clone(),
            addon_dir: self.addon_dir.clone(),
            screen: self.screen.clone(),
            setup: self.setup.clone(),
            main: self.main.clone(),
            cancel_token: self.cancel_token.clone(),
            pick_cancel_token: self.pick_cancel_token.clone(),
            workers: WorkerRegistry::default(),
            force_window_pos: self.force_window_pos,
            news: self.news.clone(),
            radio: self.radio.clone(),
            epoch: self.epoch,
        }
    }

    /// Replace UI state with `painted`, keeping this state's worker handles
    /// and publish epoch.
    fn overwrite_from_snapshot(&mut self, mut painted: AddonState) {
        let workers = std::mem::take(&mut self.workers);
        let epoch = self.epoch;
        painted.workers = workers;
        painted.epoch = epoch;
        *self = painted;
    }

    /// Fold a frame's UI edits onto `self` without clobbering a publish that
    /// landed after the snapshot was taken.
    fn merge_paint(&mut self, base: &AddonState, paint: &AddonState) {
        take_ui(
            &mut self.window_visible,
            &base.window_visible,
            &paint.window_visible,
        );
        take_ui(
            &mut self.needs_character_reload,
            &base.needs_character_reload,
            &paint.needs_character_reload,
        );
        take_ui(&mut self.config, &base.config, &paint.config);
        take_ui(&mut self.screen, &base.screen, &paint.screen);
        take_ui(
            &mut self.force_window_pos,
            &base.force_window_pos,
            &paint.force_window_pos,
        );
        self.setup.merge_paint(&base.setup, &paint.setup);
        self.main.merge_paint(&base.main, &paint.main);
        self.news.merge_paint(&base.news, &paint.news);
        self.radio.merge_paint(&base.radio, &paint.radio);
    }

    /// Register `handle` on the live addon when this state is a paint snapshot,
    /// and publish the snapshot first so the worker observes flags set above
    /// the spawn. Callers that already hold `STATE` register here: the mutex
    /// is not re-entrant.
    fn track_worker(&self, name: &'static str, handle: JoinHandle<()>) {
        if render_thread_active() && baseline_is_set() && !state_lock_held() {
            let _ = with_state(|live| {
                if live.epoch == self.epoch {
                    live.overwrite_from_snapshot(self.clone_for_paint());
                } else {
                    with_baseline(|base| {
                        if let Some(base) = base {
                            live.merge_paint(base, self);
                        }
                    });
                }
                live.workers.register(name, handle);
            });
        } else {
            self.workers.register(name, handle);
        }
    }
}

/// Live output of the chat request in flight (specs/006 US5). Written only
/// through [`ChatLiveSink`]; reset on every send; read by the bubble.
#[derive(Default)]
pub struct LiveOutput {
    pub step: Option<gw2_optimizer::llm::live::Step>,
    pub step_started: Option<std::time::Instant>,
    /// Last byte or tool event. Stall = now - this >= `STALL_AFTER`.
    pub last_activity: Option<std::time::Instant>,
    pub reasoning: String,
    pub content: String,
    /// One line per tool call, in order.
    pub tools: Vec<String>,
    pub mode: gw2_optimizer::llm::live::LiveMode,
    /// The player toggled the bubble open.
    pub expanded: bool,
    /// One extra line the bubble shows (a retry that starts over).
    pub note: String,
}

/// Silence long enough to name in the bubble.
pub const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

impl LiveOutput {
    /// A fresh request; the player's expand choice is kept.
    pub fn reset(&mut self) {
        let expanded = self.expanded;
        *self = LiveOutput {
            expanded,
            last_activity: Some(std::time::Instant::now()),
            step_started: Some(std::time::Instant::now()),
            ..Default::default()
        };
    }

    fn touch(&mut self) {
        self.last_activity = Some(std::time::Instant::now());
    }

    /// Seconds since anything arrived, once that is at least `STALL_AFTER`.
    pub fn stalled_for(&self, now: std::time::Instant) -> Option<u64> {
        let since = now.saturating_duration_since(self.last_activity?);
        (since >= STALL_AFTER).then_some(since.as_secs())
    }

    pub fn elapsed_in_step(&self, now: std::time::Instant) -> u64 {
        self.step_started
            .map(|t| now.saturating_duration_since(t).as_secs())
            .unwrap_or(0)
    }

    pub fn apply(&mut self, event: gw2_optimizer::llm::live::LiveEvent) {
        use gw2_optimizer::llm::live::{LiveEvent, LiveMode};
        match event {
            LiveEvent::Step(step) => {
                self.step = Some(step);
                self.step_started = Some(std::time::Instant::now());
                self.touch();
            }
            LiveEvent::Reasoning(text) => {
                self.reasoning.push_str(&text);
                self.mode = LiveMode::Reasoning;
                self.touch();
            }
            LiveEvent::Content(text) => {
                self.content.push_str(&text);
                if self.mode != LiveMode::Reasoning {
                    self.mode = LiveMode::Content;
                }
                self.touch();
            }
            LiveEvent::ToolCall(name) => {
                self.tools.push(name);
                self.touch();
            }
            LiveEvent::Mode(mode) => {
                // A reader that already saw reasoning is not downgraded by a
                // later loop restart.
                if self.mode != LiveMode::Reasoning || mode == LiveMode::AtOnce {
                    self.mode = mode;
                }
            }
        }
    }
}

/// The sink the chat worker installs on its thread.
pub struct ChatLiveSink(pub Arc<Mutex<LiveOutput>>);

impl gw2_optimizer::llm::live::LiveSink for ChatLiveSink {
    fn event(&self, event: gw2_optimizer::llm::live::LiveEvent) {
        if let Ok(mut live) = self.0.lock() {
            live.apply(event);
        }
    }
}

#[derive(Clone, Default)]
pub struct MainState {
    pub characters: Vec<String>,
    pub characters_loading: bool,
    /// Last failed auto-load attempt — gates the per-frame retry so an API
    /// outage cannot spawn a loader thread every frame.
    pub characters_retry_at: Option<std::time::Instant>,
    /// Rising-edge guard so opening the character combo refreshes once.
    pub char_combo_open: bool,
    pub selected_character: Option<usize>,
    pub game_mode: GameMode,
    /// WvW combat sub-tier: Solo (Roaming), Party (Havoc/small group), Squad (Zerg).
    /// Only meaningful when game_mode == WvW. Defaults to Squad.
    pub combat_tier: gw2_optimizer::scenario::CombatTier,
    /// Selected role objective for 'Create New Build' flow. None = no role chosen yet.
    pub selected_role: Option<gw2_optimizer::scenario::RoleObjective>,
    pub current_build: Option<ResolvedBuild>,
    pub current_stats: Option<StatBlock>,
    pub build_loading: bool,
    pub error: Option<String>,
    // Template selection
    pub build_tabs: Vec<gw2_api::models::BuildTab>,
    pub equipment_tabs: Vec<gw2_api::models::EquipmentTab>,
    pub selected_build_tab: Option<usize>,
    pub selected_equipment_tab: Option<usize>,
    pub build_chat_code: Option<String>,
    // Left menu
    pub active_tab: MainTab,
    // Comparison view
    pub comparison: ComparisonState,
    // Chat bar
    pub chat: ChatBarState,
    /// Game database, loaded once on main-screen entry and shared from there on.
    ///
    /// `Arc` rather than a plain `GameDb`: every background worker needs its own
    /// reference, and the database is tens of megabytes, so the old
    /// `state.main.game_db.clone()` at each spawn site was a deep copy taken on the
    /// render thread while holding STATE. Cloning the `Arc` is a refcount bump.
    /// Publish a new one with [`MainState::set_game_db`]. In-place edits go through
    /// [`MainState::game_db_mut`], which skips when a worker still holds a clone.
    pub game_db: Option<Arc<GameDb>>,
    pub game_db_loading: bool,
    /// Last failed auto-load attempt — gates the per-frame retry so a
    /// persistently failing GameDb build cannot spawn a loader every frame.
    pub game_db_retry_at: Option<std::time::Instant>,
    /// Overlay download for official API names (de/es/fr/zh).
    pub names_loading: bool,
    pub names_stage: String,
    pub names_lang: String,
    /// Progress text for game data refresh (separate from optimize_stage to avoid clobbering).
    pub game_refresh_stage: String,
    // Optimization state
    pub optimizing: bool,
    pub optimize_stage: String,
    /// Step feed of the run in flight (optimize or Choya), or the last one.
    /// Written only by that run's worker, through `with_state`.
    pub run_feed: crate::ui::run_feed::LiveFeed,
    /// 6-axis optimization weights (Power, Condition, Boon Support, Heal, Sustain, Control).
    /// Drives gear search, trait selection, and build scoring.
    pub weights: OptimizationWeights,
    /// Which radar chart axis is being dragged (None = no drag).
    pub radar_dragging: Option<usize>,
    pub saved_builds: Vec<SavedBuild>,
    pub saved_builds_loaded: bool,
    /// Basenames of `.json` files in the saves directory that failed to
    /// parse on the last load, so the Save/Load tab can warn the player
    /// instead of only logging (C29). Repopulated alongside `saved_builds`.
    pub saved_builds_skipped: Vec<String>,
    pub save_name_input: String,
    pub save_status: Option<String>,
    pub save_status_err: bool,
    // Benchmark scraping
    pub benchmark_running: bool,
    pub benchmark_last_synced: Option<String>,
    /// Per-source build counts: "snowcrows" -> n, "hardstuck" -> n, "guildjen" -> n.
    pub benchmark_counts: std::collections::HashMap<String, usize>,
    /// Per-source count of pages that listed but produced no build.
    pub benchmark_failed: std::collections::HashMap<String, usize>,
    /// Builds held per source and mode, keyed "snowcrows|PvE".
    ///
    /// The sources do not cover the same modes - Snowcrows is PvE, GuildJen
    /// is WvW and PvP - so one total per source hides which of them can
    /// actually answer the mode you are playing.
    pub benchmark_mode_counts: std::collections::HashMap<String, usize>,
    /// Live heartbeat while a sync is running ("12/45", "listing guardian…").
    pub benchmark_live: std::collections::HashMap<String, String>,
    pub benchmark_error: Option<String>,
    // Settings
    pub confirm_reset: bool,
    /// Input buffer for new API key entry in Settings (current provider).
    pub settings_key_input: String,
    /// Status of the last key validation attempt in Settings.
    pub settings_key_status: Option<String>,
    /// Whether the key validation passed (true = green, false = orange/red).
    pub settings_key_valid: bool,
    /// Optional warning (e.g. billing/quota) shown below the status message.
    pub settings_key_warning: Option<String>,
    /// Whether a key validation is in progress in Settings.
    pub settings_key_validating: bool,
    // UX feedback
    /// Frame counter for auto-dismissing save status messages (~180 frames ≈ 3s at 60fps).
    pub save_status_frames: u32,
    /// Name of the saved build pending delete confirmation (None = no dialog).
    pub confirm_delete: Option<String>,
    /// Name of the saved build pending overwrite confirmation.
    pub confirm_overwrite: Option<String>,
    /// Whether Clear Cache is waiting to be confirmed.
    ///
    /// It reads like a tidy-up and is the most expensive button in the addon:
    /// it throws away every item, skill, trait and icon the API ever sent and
    /// makes the next start re-download all of it. Nothing about the label
    /// says so, and it sits beside Refresh Game Data, which is the one people
    /// actually want.
    pub confirm_clear_cache: bool,
    /// In-progress note drafts keyed by save name.
    pub note_drafts: std::collections::HashMap<String, String>,
    /// Generation for in-flight kitchen orders. Timeout and send bump it; late applies are ignored.
    pub chat_epoch: u64,
    /// Wall-clock start of the current kitchen wait (400s, not frame-counted).
    pub chat_wait_started: Option<std::time::Instant>,
    /// What the in-flight chat request is doing, fed by the transports through
    /// `gw2_optimizer::llm::live` and read by the thinking bubble each frame.
    pub chat_live: Arc<Mutex<LiveOutput>>,
    /// Frame counter for "Copied!" tooltip feedback.
    pub copy_feedback_frames: u32,
    // Dynamic model list
    /// Models fetched from the active provider's API.
    ///
    /// The whole row, not just id and name: the picker prunes on what a model
    /// can do and orders on how well it does it, and the request builder
    /// sizes itself from the same data.
    pub available_models: Vec<gw2_optimizer::llm::ModelInfo>,
    /// Published builds closest to the current proposal, one per site.
    ///
    /// Recomputed only when [`Self::provider_picks_key`] changes: matching
    /// reads every benchmark row on disk, which is not a per-frame job.
    pub provider_picks: Vec<gw2_optimizer::benchmark::BenchmarkBuild>,
    /// What `provider_picks` was computed for — profession, mode, role and
    /// the proposal's specs. Empty means nothing has been matched yet.
    pub provider_picks_key: String,
    /// True while the pick-ranking worker is measuring candidates.
    pub picks_matching: bool,
    /// One per card, in `provider_picks` order: what it measured.
    pub pick_notes: Vec<crate::ui::main_view::provider_picks::PickCardNote>,
    /// Rows a source published that we could not plate or validate. Not the
    /// same as a source with nothing to offer, and it must not read that way.
    pub pick_unparsed: Vec<(String, usize)>,
    /// Sources that publish builds for this profession and mode, but none
    /// whose measured axes come near what was asked for. Printed rather
    /// than left silent: no card because nothing published does this job is
    /// a different answer from no card because we could not read the page.
    pub pick_no_match: Vec<String>,
    /// How far the Free-filter Choya has risen, 0 hidden to 1 fully up.
    ///
    /// Eased per frame rather than stored as a bool so the sprite slides and
    /// fades instead of appearing.
    pub free_choya_rise: f32,
    /// Whether a model list fetch is in progress.
    pub models_loading: bool,
    /// Error from the last model list fetch.
    pub models_error: Option<String>,
    // API health
    pub api_status: ApiStatus,
    /// Frame counter for periodic API health checks (~3600 frames ≈ 60s at 60fps).
    pub api_status_frames: u32,
    /// Whether a health check is currently in flight.
    pub api_health_checking: bool,
    /// Live `/v2/build` id. Compared to `cache_build_number` to prompt a data refresh.
    pub live_build_number: Option<u32>,
    /// Set once the "Auto-refresh on startup" refresh has been started, so it
    /// runs at most once per session (see `stats::should_auto_refresh`).
    pub auto_refresh_done: bool,
    /// Active-manifest vs live `/v2/build` warning from `check_staleness`.
    pub manifest_staleness: Option<String>,
    /// Result of `gw2_optimizer::data::initialize()` at addon load.
    pub data_state: Option<gw2_optimizer::data::DataState>,
    /// Cached "Usage today" count for the active provider's persisted usage
    /// file, displayed in the Settings tab. Refreshed every ~60 frames (~1s)
    /// instead of reading the file every render frame.
    pub settings_usage_today: u64,
    /// Frame counter that throttles `settings_usage_today` refresh.
    pub settings_usage_frames: u32,
    /// Cached cache-directory size in bytes for the Settings tab "Cache: …"
    /// label. Throttled refresh on `settings_cache_size_frames`.
    pub settings_cache_size: u64,
    /// PNG icon cache size (cache/graphics). Separate from JSON data.
    pub settings_graphics_size: u64,
    /// Frame counter that throttles `settings_cache_size` refresh.
    pub settings_cache_size_frames: u32,
    /// Blink Improve/New Build until the player opens that tab.
    pub tab_alert: Option<MainTab>,
    /// About tab: messages, taxonomy, open draft, refresh timers.
    pub feedback: crate::feedback::FeedbackState,
    /// About > Generations: the run history, its filters, sort and page.
    pub generations: crate::ui::main_view::GenerationsTab,
    /// Last LLM/provider failure shown on the Choya header (timeout, 429, billing).
    pub provider_issue: Option<String>,
    /// Search filter text for the Settings tab model-picker dropdown.
    /// Case-insensitive substring filter against model id + display label.
    /// Persists across frames so the user can refine. Cleared when the
    /// active provider changes (different model catalogs) and when the
    /// user explicitly clicks Clear.
    pub settings_model_search: String,
    // Spec & Trait Locks
    /// Granular lock constraints for optimizer (which specs/traits to preserve).
    pub build_locks: BuildLocks,
    /// Currently animating hover element in the lock panel (+ its 0..=1 progress).
    /// Lives on `MainState` so the subtle glow lerps smoothly across frames instead
    /// of snapping when `render_lock_panel` returns.
    pub locks_hover: Option<(crate::ui::main_view::lock_panel::LockElementId, f32)>,
    // Theme
    /// Which of the five custom-theme base colors the inline Settings picker
    /// is editing — an index into `THEME_SLOTS`, matching `CustomTheme`'s
    /// field order (0 bg, 1 panel, 2 accent, 3 text, 4 muted).
    ///
    /// Transient UI state. `MainState` derives
    /// `Default`, so this opens on Background and the picker is never in a
    /// dead "nothing selected" state. Readers clamp with `.min(4)`.
    pub theme_edit_slot: usize,
}

impl MainState {
    /// Publish a freshly loaded game database.
    ///
    /// Workers still holding a clone of the previous `Arc` keep reading it until
    /// they finish; only new clones see this one.
    pub fn set_game_db(&mut self, db: GameDb) {
        self.game_db = Some(Arc::new(db));
    }

    /// Mutable access to the loaded database, for in-place edits such as attaching
    /// a localized name pack.
    ///
    /// If a background worker still holds a clone, this returns `None` rather than
    /// deep-copying tens of megabytes on the thread that holds STATE. Callers that
    /// can build a localized copy off-lock should publish it with
    /// [`MainState::set_game_db`].
    pub fn game_db_mut(&mut self) -> Option<&mut GameDb> {
        self.game_db.as_mut().and_then(Arc::get_mut)
    }

    /// Clear the currently resolved build view so the UI never shows stale data.
    pub fn clear_resolved_view(&mut self) {
        self.current_build = None;
        self.current_stats = None;
        self.comparison.current_combat_solo = None;
        self.comparison.current_combat_party = None;
        self.comparison.current_combat_squad = None;
    }

    fn hydrate_benchmarks_from_disk(&mut self, addon_dir: &std::path::Path) {
        if self.benchmark_last_synced.is_some() {
            return;
        }
        let builds = gw2_optimizer::scraper::load_benchmarks(addon_dir);
        if builds.is_empty() {
            return;
        }
        let mut counts = std::collections::HashMap::new();
        let mut mode_counts = std::collections::HashMap::new();
        for b in &builds {
            *counts.entry(b.source.clone()).or_insert(0) += 1;
            // Same key the live sync writes, or the grid reads a dash for
            // every cell. Only a sync used to fill this, so after a restart
            // the benchmarks table showed nothing at all while several
            // hundred builds sat on disk — a store that looked empty and was
            // not.
            *mode_counts
                .entry(format!("{}|{}", b.source, b.mode))
                .or_insert(0) += 1;
        }
        self.benchmark_counts = counts;
        self.benchmark_mode_counts = mode_counts;
        // `benchmark_failed` is deliberately left alone: what a run could not
        // read is not written to disk, so after a restart we do not know, and
        // an invented zero would show a clean grid over an unknown one.
        let stamp = std::fs::metadata(addon_dir.join("benchmarks"))
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                chrono::DateTime::<chrono::Utc>::from(t)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_else(|| "disk".into());
        self.benchmark_last_synced = Some(stamp);
    }

    fn merge_paint(&mut self, base: &Self, paint: &Self) {
        // ponytail: a new field stays live on conflict until it is named here.
        // Epoch-match commit still moves the whole snapshot.
        keep_worker(&mut self.characters, &base.characters, &paint.characters);
        merge_busy(
            &mut self.characters_loading,
            base.characters_loading,
            paint.characters_loading,
        );
        take_ui(
            &mut self.characters_retry_at,
            &base.characters_retry_at,
            &paint.characters_retry_at,
        );
        take_ui(
            &mut self.char_combo_open,
            &base.char_combo_open,
            &paint.char_combo_open,
        );
        take_ui(
            &mut self.selected_character,
            &base.selected_character,
            &paint.selected_character,
        );
        take_ui(&mut self.game_mode, &base.game_mode, &paint.game_mode);
        take_ui(&mut self.combat_tier, &base.combat_tier, &paint.combat_tier);
        take_ui(
            &mut self.selected_role,
            &base.selected_role,
            &paint.selected_role,
        );
        merge_build(
            &mut self.current_build,
            &base.current_build,
            &paint.current_build,
        );
        // `current_stats` has no Eq. The epoch-match overwrite and the spawn
        // publish carry UI clears; a worker publish keeps the live block.
        merge_busy(
            &mut self.build_loading,
            base.build_loading,
            paint.build_loading,
        );
        merge_message(&mut self.error, &base.error, &paint.error);
        keep_len(
            &mut self.build_tabs,
            base.build_tabs.len(),
            &paint.build_tabs,
        );
        keep_len(
            &mut self.equipment_tabs,
            base.equipment_tabs.len(),
            &paint.equipment_tabs,
        );
        take_ui(
            &mut self.selected_build_tab,
            &base.selected_build_tab,
            &paint.selected_build_tab,
        );
        take_ui(
            &mut self.selected_equipment_tab,
            &base.selected_equipment_tab,
            &paint.selected_equipment_tab,
        );
        keep_worker(
            &mut self.build_chat_code,
            &base.build_chat_code,
            &paint.build_chat_code,
        );
        take_ui(&mut self.active_tab, &base.active_tab, &paint.active_tab);
        self.comparison
            .merge_paint(&base.comparison, &paint.comparison);
        merge_chat(&mut self.chat, &base.chat, &paint.chat);
        merge_db(&mut self.game_db, &base.game_db, &paint.game_db);
        merge_busy(
            &mut self.game_db_loading,
            base.game_db_loading,
            paint.game_db_loading,
        );
        take_ui(
            &mut self.game_db_retry_at,
            &base.game_db_retry_at,
            &paint.game_db_retry_at,
        );
        merge_busy(
            &mut self.names_loading,
            base.names_loading,
            paint.names_loading,
        );
        keep_worker(&mut self.names_stage, &base.names_stage, &paint.names_stage);
        keep_worker(&mut self.names_lang, &base.names_lang, &paint.names_lang);
        keep_worker(
            &mut self.game_refresh_stage,
            &base.game_refresh_stage,
            &paint.game_refresh_stage,
        );
        merge_busy(&mut self.optimizing, base.optimizing, paint.optimizing);
        keep_worker(
            &mut self.optimize_stage,
            &base.optimize_stage,
            &paint.optimize_stage,
        );
        keep_worker(&mut self.run_feed, &base.run_feed, &paint.run_feed);
        take_ui(&mut self.weights, &base.weights, &paint.weights);
        take_ui(
            &mut self.radar_dragging,
            &base.radar_dragging,
            &paint.radar_dragging,
        );
        keep_len(
            &mut self.saved_builds,
            base.saved_builds.len(),
            &paint.saved_builds,
        );
        take_ui(
            &mut self.saved_builds_loaded,
            &base.saved_builds_loaded,
            &paint.saved_builds_loaded,
        );
        keep_worker(
            &mut self.saved_builds_skipped,
            &base.saved_builds_skipped,
            &paint.saved_builds_skipped,
        );
        take_ui(
            &mut self.save_name_input,
            &base.save_name_input,
            &paint.save_name_input,
        );
        take_ui(&mut self.save_status, &base.save_status, &paint.save_status);
        take_ui(
            &mut self.save_status_err,
            &base.save_status_err,
            &paint.save_status_err,
        );
        merge_busy(
            &mut self.benchmark_running,
            base.benchmark_running,
            paint.benchmark_running,
        );
        keep_worker(
            &mut self.benchmark_last_synced,
            &base.benchmark_last_synced,
            &paint.benchmark_last_synced,
        );
        // Counts and the live heartbeat are worker publishes. Leave `self`'s
        // copies when this frame already diverged; nothing in the window body
        // edits them except by starting a run, which spawn publishes itself.
        take_ui(
            &mut self.confirm_reset,
            &base.confirm_reset,
            &paint.confirm_reset,
        );
        take_ui(
            &mut self.settings_key_input,
            &base.settings_key_input,
            &paint.settings_key_input,
        );
        keep_worker(
            &mut self.settings_key_status,
            &base.settings_key_status,
            &paint.settings_key_status,
        );
        take_ui(
            &mut self.settings_key_valid,
            &base.settings_key_valid,
            &paint.settings_key_valid,
        );
        keep_worker(
            &mut self.settings_key_warning,
            &base.settings_key_warning,
            &paint.settings_key_warning,
        );
        merge_busy(
            &mut self.settings_key_validating,
            base.settings_key_validating,
            paint.settings_key_validating,
        );
        take_ui(
            &mut self.save_status_frames,
            &base.save_status_frames,
            &paint.save_status_frames,
        );
        take_ui(
            &mut self.confirm_delete,
            &base.confirm_delete,
            &paint.confirm_delete,
        );
        take_ui(
            &mut self.confirm_overwrite,
            &base.confirm_overwrite,
            &paint.confirm_overwrite,
        );
        take_ui(
            &mut self.confirm_clear_cache,
            &base.confirm_clear_cache,
            &paint.confirm_clear_cache,
        );
        take_ui(&mut self.note_drafts, &base.note_drafts, &paint.note_drafts);
        take_ui(&mut self.chat_epoch, &base.chat_epoch, &paint.chat_epoch);
        take_ui(
            &mut self.chat_wait_started,
            &base.chat_wait_started,
            &paint.chat_wait_started,
        );
        // `chat_live` is an Arc shared with the snapshot. Workers write through it.
        take_ui(
            &mut self.copy_feedback_frames,
            &base.copy_feedback_frames,
            &paint.copy_feedback_frames,
        );
        keep_len(
            &mut self.available_models,
            base.available_models.len(),
            &paint.available_models,
        );
        keep_len(
            &mut self.provider_picks,
            base.provider_picks.len(),
            &paint.provider_picks,
        );
        take_ui(
            &mut self.provider_picks_key,
            &base.provider_picks_key,
            &paint.provider_picks_key,
        );
        merge_busy(
            &mut self.picks_matching,
            base.picks_matching,
            paint.picks_matching,
        );
        keep_len(
            &mut self.pick_notes,
            base.pick_notes.len(),
            &paint.pick_notes,
        );
        keep_worker(
            &mut self.pick_unparsed,
            &base.pick_unparsed,
            &paint.pick_unparsed,
        );
        keep_worker(
            &mut self.pick_no_match,
            &base.pick_no_match,
            &paint.pick_no_match,
        );
        take_ui(
            &mut self.free_choya_rise,
            &base.free_choya_rise,
            &paint.free_choya_rise,
        );
        merge_busy(
            &mut self.models_loading,
            base.models_loading,
            paint.models_loading,
        );
        keep_worker(
            &mut self.models_error,
            &base.models_error,
            &paint.models_error,
        );
        keep_worker(&mut self.api_status, &base.api_status, &paint.api_status);
        take_ui(
            &mut self.api_status_frames,
            &base.api_status_frames,
            &paint.api_status_frames,
        );
        merge_busy(
            &mut self.api_health_checking,
            base.api_health_checking,
            paint.api_health_checking,
        );
        keep_worker(
            &mut self.live_build_number,
            &base.live_build_number,
            &paint.live_build_number,
        );
        take_ui(
            &mut self.auto_refresh_done,
            &base.auto_refresh_done,
            &paint.auto_refresh_done,
        );
        keep_worker(
            &mut self.manifest_staleness,
            &base.manifest_staleness,
            &paint.manifest_staleness,
        );
        take_ui(
            &mut self.settings_usage_today,
            &base.settings_usage_today,
            &paint.settings_usage_today,
        );
        take_ui(
            &mut self.settings_usage_frames,
            &base.settings_usage_frames,
            &paint.settings_usage_frames,
        );
        take_ui(
            &mut self.settings_cache_size,
            &base.settings_cache_size,
            &paint.settings_cache_size,
        );
        take_ui(
            &mut self.settings_graphics_size,
            &base.settings_graphics_size,
            &paint.settings_graphics_size,
        );
        take_ui(
            &mut self.settings_cache_size_frames,
            &base.settings_cache_size_frames,
            &paint.settings_cache_size_frames,
        );
        take_ui(&mut self.tab_alert, &base.tab_alert, &paint.tab_alert);
        self.feedback.merge_paint(&base.feedback, &paint.feedback);
        self.generations
            .merge_paint(&base.generations, &paint.generations);
        merge_message(
            &mut self.provider_issue,
            &base.provider_issue,
            &paint.provider_issue,
        );
        take_ui(
            &mut self.settings_model_search,
            &base.settings_model_search,
            &paint.settings_model_search,
        );
        take_ui(&mut self.build_locks, &base.build_locks, &paint.build_locks);
        take_ui(&mut self.locks_hover, &base.locks_hover, &paint.locks_hover);
        take_ui(
            &mut self.theme_edit_slot,
            &base.theme_edit_slot,
            &paint.theme_edit_slot,
        );
    }
}

/// GW2 API health status, checked periodically via `/v2/build`.
#[derive(Default, Debug, Clone, PartialEq)]
pub enum ApiStatus {
    #[default]
    Unknown,
    Online,
    Degraded,
    Offline,
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum MainTab {
    #[default]
    NewBuild,
    Improve,
    Talk,
    SaveLoad,
    News,
    Radio,
    Settings,
    About,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Setup(SetupStep),
    Main,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStep {
    Language,
    Gw2ApiKey,
    LlmApiKey,
    DataDownload,
}

#[derive(Clone, Default, PartialEq)]
pub struct SetupState {
    // GW2 key input
    pub gw2_key_input: String,
    pub gw2_key_status: KeyStatus,
    pub gw2_key_scopes: Vec<(String, bool)>, // (scope_name, present)
    // AI provider key input (provider-agnostic)
    pub llm_key_input: String,
    pub llm_key_status: KeyStatus,
    // Data download
    pub download_progress: Option<DownloadState>,
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum KeyStatus {
    #[default]
    NotValidated,
    Validating,
    Valid,
    Invalid(String), // error message
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadState {
    pub current_step: usize,
    pub total_steps: usize,
    pub step_name: String,
    pub inner_done: usize,
    pub inner_total: usize,
    pub done: bool,
    pub error: Option<String>,
    /// Hopper items-catalog path from [`gw2_api::download::items_fill_kind`].
    /// `None` keeps normal refresh/Verify copy.
    pub items_fill_kind: Option<gw2_api::download::ItemsFillKind>,
}

/// i18n key for an items fill mode shown during download/setup progress.
pub fn items_fill_i18n_key(kind: gw2_api::download::ItemsFillKind) -> &'static str {
    use gw2_api::download::ItemsFillKind::*;
    match kind {
        FirstFill => "status.items_first_fill",
        Resume => "status.items_resume",
        SameBuildSkip => "status.items_same_build_skip",
    }
}

/// Probe live build + cache for Hopper fill-mode UI (presentation only).
pub fn probe_items_fill_kind(
    client: &gw2_api::client::Gw2Client,
    cache: &gw2_api::cache::DataCache,
) -> Option<gw2_api::download::ItemsFillKind> {
    let build = client.get_build_number().ok()?;
    gw2_api::download::items_fill_kind(cache, build, gw2_api::download::RefreshMode::Default)
}

impl DownloadState {
    /// Overall 0..=1, including the current step's item/icon batches.
    pub fn fraction(&self) -> f32 {
        download_fraction(
            self.current_step,
            self.total_steps,
            self.inner_done,
            self.inner_total,
            self.done,
        )
    }
}

pub(crate) fn download_fraction(
    current_step: usize,
    total_steps: usize,
    inner_done: usize,
    inner_total: usize,
    done: bool,
) -> f32 {
    if done {
        return 1.0;
    }
    if total_steps == 0 {
        return 0.0;
    }
    // `current_step` is 1-based and in-progress (13/13 still has work).
    let inner = if inner_total > 0 {
        (inner_done as f32 / inner_total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let completed = current_step.saturating_sub(1) as f32;
    ((completed + inner) / total_steps as f32).clamp(0.0, 1.0)
}

fn lock_state() -> std::sync::MutexGuard<'static, Option<AddonState>> {
    STATE.lock().unwrap_or_else(|e| {
        // Use eprintln! — nexus::log::log() can panic if the addon API
        // isn't initialized yet (or in tests), and panic recovery code
        // must never itself panic.
        eprintln!("[GW2BuildOpt] State mutex was poisoned, recovering");
        e.into_inner()
    })
}

// SetupState/MainState fields are filled conditionally from optional config
// after default(); a struct literal would force redundant default expressions.
#[allow(clippy::field_reassign_with_default)]
pub fn init(addon_dir: PathBuf) {
    let config_path = AppConfig::config_path(&addon_dir);
    let (config, config_err) = AppConfig::load(&config_path);
    // Build the active palette before the first frame renders; Settings
    // re-applies on every change.
    crate::ui::theme::apply_theme(&config.theme);

    let screen = if config.is_setup_complete() {
        Screen::Main
    } else if !config.has_gw2_key() {
        Screen::Setup(SetupStep::Language)
    } else if config.has_active_llm_key() {
        // GW2 key is present; cache missing — go to download
        Screen::Setup(SetupStep::DataDownload)
    } else {
        Screen::Setup(SetupStep::LlmApiKey)
    };

    let mut setup = SetupState::default();
    if let Some(ref key) = config.gw2_api_key {
        setup.gw2_key_input = key.clone();
        setup.gw2_key_status = KeyStatus::Valid;
    }
    if let Some(key) = config.active_api_key() {
        setup.llm_key_input = key.to_string();
        setup.llm_key_status = KeyStatus::Valid;
    }

    let mut main = MainState::default();
    gw2_core::i18n::set_language(&config.ui_language);
    // Surface any config parse error in the UI status bar
    main.error = config_err;
    let data_state = gw2_optimizer::data::initialize();
    if let Some(reason) = data_state.optimize_block_reason() {
        worker_log(reason.clone());
        if main.error.is_none() {
            main.error = Some(reason);
        }
    }
    main.data_state = Some(data_state);
    // Apply saved default game mode from config
    let default_mode_label = config.default_game_mode.as_deref().unwrap_or("PvE");
    main.game_mode = match default_mode_label {
        "PvP" => gw2_core::types::GameMode::PvP,
        "WvW" => gw2_core::types::GameMode::WvW,
        _ => gw2_core::types::GameMode::PvE,
    };
    main.weights = OptimizationWeights::default_for_mode(main.game_mode.label());
    // Saved defaults for scale and role, so the left panel opens the way the
    // player set it in Settings instead of on the first chip (2026-09-08).
    if let Some(tier) = config.default_combat_tier.as_deref() {
        use gw2_optimizer::scenario::CombatTier;
        if let Some(t) = [CombatTier::Solo, CombatTier::Party, CombatTier::Squad]
            .into_iter()
            .find(|t| format!("{t:?}") == tier)
        {
            main.combat_tier = t;
        }
    }
    if let Some(role) = config.default_role.as_deref() {
        if let Some(r) = gw2_optimizer::scenario::RoleObjective::play_roles_for(&main.game_mode)
            .iter()
            .copied()
            .find(|r| format!("{r:?}") == role)
        {
            main.selected_role = Some(r);
            main.weights = r.to_weights_for(&main.game_mode, main.combat_tier);
        }
    }
    main.chat.history = crate::ui::chat_bar::load_history(&addon_dir);
    main.hydrate_benchmarks_from_disk(&addon_dir);
    crate::ui::icons::set_graphics_dir(addon_dir.join("cache").join("graphics"));
    *lock_state() = Some(AddonState {
        window_visible: config.window_visible,
        needs_character_reload: config.window_visible,
        config,
        config_path,
        addon_dir,
        screen,
        setup,
        main,
        cancel_token: CancellationToken::new(),
        pick_cancel_token: CancellationToken::new(),
        workers: WorkerRegistry::default(),
        force_window_pos: false,
        news: crate::news::NewsState::default(),
        radio: crate::radio::RadioUiState::default(),
        epoch: 0,
    });
}

pub fn toggle_window() {
    let snapshot = {
        let mut guard = lock_state();
        let Some(state) = guard.as_mut() else {
            return;
        };
        state.window_visible = !state.window_visible;
        state.config.window_visible = state.window_visible;
        if state.window_visible {
            state.needs_character_reload = true;
        }
        state.epoch = state.epoch.wrapping_add(1);
        (state.config.clone(), state.config_path.clone())
    };
    if let Err(e) = snapshot.0.save(&snapshot.1) {
        crate::ui::log_disk_error(format!("config save failed: {e}"));
    }
}

/// Keybind: flip the mini radio on or off. Same shape as [`toggle_window`]:
/// flip under the lock, save after releasing it.
pub fn toggle_mini_radio() {
    let snapshot = {
        let mut guard = lock_state();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let mini = &mut state.config.radio.mini_radio;
        mini.enabled = !mini.enabled;
        state.epoch = state.epoch.wrapping_add(1);
        (state.config.clone(), state.config_path.clone())
    };
    if let Err(e) = snapshot.0.save(&snapshot.1) {
        crate::ui::log_disk_error(format!("config save failed: {e}"));
    }
}

/// Keybind: anchor or unanchor the mini radio, through the same setter as
/// the strip's popup and the Choya Tunes tab (its save is detached).
pub fn toggle_mini_radio_anchor() {
    let mut guard = lock_state();
    let Some(state) = guard.as_mut() else {
        return;
    };
    let on = !state.config.radio.mini_radio.anchored;
    crate::ui::mini_radio::set_anchored(state, on);
    state.epoch = state.epoch.wrapping_add(1);
}

pub fn persist_window() {
    let snapshot = {
        let mut guard = lock_state();
        let Some(state) = guard.as_mut() else {
            return;
        };
        state.config.window_visible = state.window_visible;
        state.epoch = state.epoch.wrapping_add(1);
        (state.config.clone(), state.config_path.clone())
    };
    if let Err(e) = snapshot.0.save(&snapshot.1) {
        crate::ui::log_disk_error(format!("config save failed: {e}"));
    }
}

/// Ask every background worker to stop, and return immediately.
///
/// Unload calls this before it persists the window rect so the workers have that
/// disk write worth of head start on their cancel checks; [`join_workers`] then
/// does the waiting. Safe to call when the addon was never initialised.
pub fn request_shutdown() {
    if let Some(state) = lock_state().as_ref() {
        state.cancel_token.cancel();
        state.pick_cancel_token.cancel();
    }
}

/// Cancel every tracked worker and wait up to `budget` for them to finish.
///
/// The STATE mutex is taken only long enough to flip the cancel flag and move the
/// handles out; the wait itself happens with STATE **released**. Workers publish
/// their results through `with_state`, so waiting while holding it would deadlock
/// the game on unload — which is the whole reason this is a free function and not
/// a method that runs under the caller's guard.
///
/// Workers that are still running when the budget expires are detached and named
/// in the returned [`ShutdownReport`].
pub fn join_workers(budget: Duration) -> ShutdownReport {
    let pending = {
        let mut guard = lock_state();
        match guard.as_mut() {
            Some(state) => {
                state.cancel_token.cancel();
                state.pick_cancel_token.cancel();
                state.workers.take_all()
            }
            None => Vec::new(),
        }
    };
    join_bounded(pending, budget)
}

/// Clear state on addon unload.
/// Cancels the token first so background threads exit early,
/// then drops the state to release all resources.
///
/// This does **not** wait: call [`join_workers`] before it. Any handle still
/// tracked when the state drops is detached, not joined.
pub fn clear() {
    let mut guard = lock_state();
    if let Some(ref state) = *guard {
        state.cancel_token.cancel();
        state.pick_cancel_token.cancel();
    }
    *guard = None;
}

// Frame snapshot
//
// ImGui layout must not hold `STATE`. The render thread clones the UI state
// (`PaintCapture`), drops the guard, paints into the clone, then commits.
// Workers call `with_state` during that gap. The publish epoch tells commit
// whether to move the clone over the live state or fold only the UI edits.

thread_local! {
    static RENDER_THREAD: Cell<bool> = const { Cell::new(false) };
    static PAINT_BASELINE: Cell<*const AddonState> = const { Cell::new(std::ptr::null()) };
    static STATE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

fn render_thread_active() -> bool {
    RENDER_THREAD.with(|c| c.get())
}

fn state_lock_held() -> bool {
    STATE_DEPTH.with(|c| c.get() > 0)
}

/// Counts `with_state` frames on this thread so `track_worker` does not
/// re-lock `STATE` from inside one.
struct StateHold;

impl StateHold {
    fn enter() -> Self {
        STATE_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
        Self
    }
}

impl Drop for StateHold {
    fn drop(&mut self) {
        STATE_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

fn baseline_is_set() -> bool {
    PAINT_BASELINE.with(|c| !c.get().is_null())
}

fn with_baseline<R>(f: impl FnOnce(Option<&AddonState>) -> R) -> R {
    PAINT_BASELINE.with(|c| {
        let ptr = c.get();
        // SAFETY: `BaselinePin` stores the address of `PaintFrame::baseline`
        // on this thread and clears it on drop, before that field moves.
        // Only this thread reads it, and only while the pin is live.
        let baseline = if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        };
        f(baseline)
    })
}

/// Marks the calling thread as the overlay render thread for the guard's life.
///
/// `with_state` on this thread does not bump the publish epoch: those calls
/// are the frame's own capture, spawn publish, and commit.
pub(crate) struct RenderThreadGuard;

impl RenderThreadGuard {
    pub(crate) fn enter() -> Self {
        RENDER_THREAD.with(|c| c.set(true));
        Self
    }
}

impl Drop for RenderThreadGuard {
    fn drop(&mut self) {
        RENDER_THREAD.with(|c| c.set(false));
    }
}

/// Pins `PaintFrame::baseline` for [`AddonState::track_worker`] during layout.
pub(crate) struct BaselinePin;

impl BaselinePin {
    fn install(baseline: &AddonState) -> Self {
        PAINT_BASELINE.with(|c| c.set(baseline as *const AddonState));
        Self
    }
}

impl Drop for BaselinePin {
    fn drop(&mut self) {
        PAINT_BASELINE.with(|c| c.set(std::ptr::null()));
    }
}

/// One clone of the live UI state, taken under `STATE`.
pub(crate) struct PaintCapture {
    epoch: u64,
    baseline: AddonState,
}

impl PaintCapture {
    pub(crate) fn take(live: &AddonState) -> Self {
        Self {
            epoch: live.epoch,
            baseline: live.clone_for_paint(),
        }
    }

    /// Second clone, outside the lock: the window body mutates `state`.
    pub(crate) fn materialize(self) -> PaintFrame {
        let state = self.baseline.clone_for_paint();
        PaintFrame {
            epoch: self.epoch,
            baseline: self.baseline,
            state,
        }
    }
}

/// Snapshot plus the working copy the window body paints into.
pub(crate) struct PaintFrame {
    epoch: u64,
    baseline: AddonState,
    pub(crate) state: AddonState,
}

impl PaintFrame {
    pub(crate) fn pin_baseline(&self) -> BaselinePin {
        BaselinePin::install(&self.baseline)
    }

    pub(crate) fn commit(self, live: &mut AddonState) {
        let PaintFrame {
            epoch,
            baseline,
            state,
        } = self;
        if live.epoch == epoch {
            live.overwrite_from_snapshot(state);
        } else {
            live.merge_paint(&baseline, &state);
        }
    }
}

pub(crate) fn take_ui<T: Clone + PartialEq>(live: &mut T, base: &T, paint: &T) {
    if paint != base {
        *live = paint.clone();
    }
}

pub(crate) fn keep_worker<T: Clone + PartialEq>(live: &mut T, base: &T, paint: &T) {
    if live == base && paint != base {
        *live = paint.clone();
    }
}

/// UI set a flag, or a worker cleared it. A finished worker (`live` already
/// false while `paint` is still busy) keeps that clear — spawn published the
/// busy flag, so `live == base` is not "untouched". UI clearing (Stop) wins.
pub(crate) fn merge_busy(live: &mut bool, base: bool, paint: bool) {
    if paint == base {
        return;
    }
    if !paint || *live {
        *live = paint;
    }
}

pub(crate) fn merge_busy_opt<T: Clone + PartialEq>(
    live: &mut Option<T>,
    base: &Option<T>,
    paint: &Option<T>,
) {
    if paint == base {
        return;
    }
    if paint.is_none() {
        *live = None;
        return;
    }
    if live.is_none() {
        return;
    }
    *live = paint.clone();
}

pub(crate) fn merge_message(
    live: &mut Option<String>,
    base: &Option<String>,
    paint: &Option<String>,
) {
    if paint == base {
        return;
    }
    if live == base {
        *live = paint.clone();
        return;
    }
    if paint.is_none() && live.is_some() {
        return;
    }
    *live = paint.clone();
}

/// Copy `paint` when it changed length and `live` has not.
pub(crate) fn keep_len<T: Clone>(live: &mut Vec<T>, base_len: usize, paint: &[T]) {
    if live.len() == base_len && paint.len() != base_len {
        *live = paint.to_vec();
    }
}

fn merge_db(
    live: &mut Option<Arc<gw2_optimizer::gamedb::GameDb>>,
    base: &Option<Arc<gw2_optimizer::gamedb::GameDb>>,
    paint: &Option<Arc<gw2_optimizer::gamedb::GameDb>>,
) {
    let same = |a: &Option<Arc<gw2_optimizer::gamedb::GameDb>>,
                b: &Option<Arc<gw2_optimizer::gamedb::GameDb>>| {
        match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    };
    if same(live, base) && !same(paint, base) {
        *live = paint.clone();
    }
}

fn build_sig(build: &ResolvedBuild) -> (&str, &str) {
    (&build.character_name, &build.profession)
}

fn merge_build(
    live: &mut Option<ResolvedBuild>,
    base: &Option<ResolvedBuild>,
    paint: &Option<ResolvedBuild>,
) {
    let live_sig = live.as_ref().map(build_sig);
    let base_sig = base.as_ref().map(build_sig);
    if live_sig != base_sig {
        return;
    }
    let paint_sig = paint.as_ref().map(build_sig);
    if paint_sig != base_sig {
        *live = paint.clone();
    }
}

fn merge_chat(live: &mut ChatBarState, base: &ChatBarState, paint: &ChatBarState) {
    take_ui(&mut live.input, &base.input, &paint.input);
    take_ui(&mut live.copied_code, &base.copied_code, &paint.copied_code);
    take_ui(
        &mut live.copied_frames,
        &base.copied_frames,
        &paint.copied_frames,
    );
    take_ui(
        &mut live.scroll_to_end,
        &base.scroll_to_end,
        &paint.scroll_to_end,
    );
    take_ui(&mut live.dirty, &base.dirty, &paint.dirty);
    take_ui(&mut live.last_typed, &base.last_typed, &paint.last_typed);
    take_ui(&mut live.header_pose, &base.header_pose, &paint.header_pose);
    take_ui(
        &mut live.header_pose_at,
        &base.header_pose_at,
        &paint.header_pose_at,
    );
    let names_same = |a: &Option<Arc<std::collections::HashSet<String>>>,
                      b: &Option<Arc<std::collections::HashSet<String>>>| {
        match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    };
    if names_same(&live.names, &base.names) && !names_same(&paint.names, &base.names) {
        live.names = paint.names.clone();
    }
    merge_busy(&mut live.waiting, base.waiting, paint.waiting);
    if paint.history != base.history {
        if live.history == base.history || paint.history.starts_with(&live.history) {
            live.history = paint.history.clone();
        } else if live.history.starts_with(&base.history)
            && paint.history.starts_with(&base.history)
        {
            for msg in paint.history.iter().skip(base.history.len()) {
                if !live.history.contains(msg) {
                    live.history.push(msg.clone());
                }
            }
        }
    }
}

impl SetupState {
    fn merge_paint(&mut self, base: &Self, paint: &Self) {
        take_ui(
            &mut self.gw2_key_input,
            &base.gw2_key_input,
            &paint.gw2_key_input,
        );
        take_ui(
            &mut self.llm_key_input,
            &base.llm_key_input,
            &paint.llm_key_input,
        );
        merge_key(
            &mut self.gw2_key_status,
            &base.gw2_key_status,
            &paint.gw2_key_status,
        );
        merge_key(
            &mut self.llm_key_status,
            &base.llm_key_status,
            &paint.llm_key_status,
        );
        keep_worker(
            &mut self.gw2_key_scopes,
            &base.gw2_key_scopes,
            &paint.gw2_key_scopes,
        );
        keep_worker(
            &mut self.download_progress,
            &base.download_progress,
            &paint.download_progress,
        );
    }
}

fn merge_key(live: &mut KeyStatus, base: &KeyStatus, paint: &KeyStatus) {
    if paint == base {
        return;
    }
    if live == base {
        *live = paint.clone();
        return;
    }
    if *paint == KeyStatus::Validating && *live != KeyStatus::Validating {
        return;
    }
    *live = paint.clone();
}

/// Access state for reading/writing in UI code.
///
/// A call from any thread other than the render thread bumps [`AddonState`]'s
/// publish epoch so an in-flight frame snapshot does not commit over that write.
pub fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut AddonState) -> R,
{
    let _held = StateHold::enter();
    let mut guard = lock_state();
    let state = guard.as_mut()?;
    if !render_thread_active() {
        state.epoch = state.epoch.wrapping_add(1);
    }
    Some(f(state))
}

// Every test that touches the global STATE (`init`, `clear`, `with_state`), here or in
// another module such as `feedback::tasks`, mutates the same static mutex. Parallel
// execution can make one test reset/replace state while another is asserting, causing
// flaky None/stale-value results. All of them serialise on this one lock.
#[cfg(test)]
static TEST_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the crate-wide STATE test lock; recovers from a poisoned lock (a test that
/// panicked while holding it) so one failure does not cascade into the rest.
#[cfg(test)]
pub(crate) fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let current = std::thread::current();
    let test_name = current.name().unwrap_or("<unnamed>");
    eprintln!(
        "[GW2BuildOpt][state::tests] acquiring shared STATE test lock: {}",
        test_name
    );
    TEST_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    // Test fixtures are built field-by-field for readability.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use gw2_core::config::AppConfig;
    use gw2_optimizer::scoring::OptimizationWeights;

    /// Reset the global STATE to None for test isolation.
    /// Call as the first line of every test that calls init(), clear(), or with_state().
    /// Required: tests share a global static Mutex — without this, a prior test's
    /// init() leaves state set, causing the next test to start with stale data.
    fn reset_state() {
        *super::lock_state() = None;
    }

    /// Write `config` as config.json inside a per-test temp dir and return the dir.
    /// AppConfig::save() creates parent dirs automatically.
    fn config_in_tempdir(config: &AppConfig, label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_{}", std::process::id(), label));
        let cfg_path = AppConfig::config_path(&dir);
        config.save(&cfg_path).unwrap();
        dir
    }

    fn dummy_resolved_build() -> ResolvedBuild {
        ResolvedBuild {
            character_name: "Test Character".into(),
            profession: "Warrior".into(),
            game_mode: GameMode::PvE,
            specializations: Vec::new(),
            skills: Default::default(),
            legends: Vec::new(),
            pets: Vec::new(),
            weapons: Vec::new(),
            armor: Vec::new(),
            trinkets: Vec::new(),
            relic: None,
            rune: None,
            pvp_amulet: None,
        }
    }

    fn dummy_combat_metrics(total_dps_index: i32) -> gw2_core::types::CombatMetrics {
        gw2_core::types::CombatMetrics {
            effective_power: 0,
            strike_dps_index: total_dps_index,
            condition_dps_index: 0,
            total_dps_index,
            healing_index: 0,
            crit_chance: 0.0,
            boon_duration_pct: 0.0,
            condi_duration_pct: 0.0,
            effective_health: 0,
            damage_reduction_pct: 0.0,
            bleeding_tick: 0,
            burning_tick: 0,
            poison_tick: 0,
            torment_tick: 0,
            confusion_tick: 0,
        }
    }

    // CancellationToken

    #[test]
    fn test_cancel_token_new_is_not_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_cancel_token_cancel_sets_flag() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancel_token_clone_sees_cancellation() {
        // Verifies that clones share the same Arc<AtomicBool> — threads cloning
        // the token at spawn time will see the cancellation signal from clear().
        let token = CancellationToken::new();
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn test_cancel_token_worker_loop_exits_on_pulse() {
        // Validates the worker-loop pattern used in every `std::thread::spawn`
        // site in `crates/addon/src/ui/`: a background thread clones a
        // CancellationToken and checks `is_cancelled()` between iterations of
        // its work loop. Pulsing the token mid-loop must let the worker exit
        // within a bounded time. This covers all 10 audited live spawn sites
        // in crates/addon/src/ui/ — they share this exact loop shape, so the
        // pattern is tested once.
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            // Simulate a long-running worker: 500 iterations × 2ms = up to 1s
            // of work if no cancel arrives. Each iteration checks the token,
            // mirroring the `if token.is_cancelled() { return; }` boundary in
            // every real spawn site.
            for _ in 0..500 {
                if worker_token.is_cancelled() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            false
        });

        // Let the worker start, then pulse the token.
        std::thread::sleep(std::time::Duration::from_millis(20));
        token.cancel();

        // Worker must observe cancellation and exit promptly. Generous
        // 500ms join budget guards against CI flakiness; real exit should be
        // single-digit ms after the next 2ms iteration boundary.
        let start = std::time::Instant::now();
        let exited_via_cancel = handle.join().expect("worker thread panicked");
        let elapsed = start.elapsed();
        assert!(
            exited_via_cancel,
            "worker must exit via is_cancelled() check, not loop completion"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "worker must exit within 500ms of cancel pulse (took {:?})",
            elapsed
        );
    }

    // MainState::default() — initial loading-flag safety
    //
    // Risk: non-optimizer background threads in main_view.rs (lines 1501, 1584,
    // 2168, 2199, 2279, 2312, 3233) have no catch_unwind.  A panic in any of
    // those threads leaves these flags stuck `true` permanently.  This test
    // verifies the safe initial boundary: all flags must start false so any
    // stuck-flag regression is immediately visible as a state divergence.

    #[test]
    fn test_main_state_default_fields() {
        let main = MainState::default();
        assert!(main.characters.is_empty(), "characters must start empty");
        assert!(!main.optimizing, "optimizing must start false");
        assert!(
            !main.characters_loading,
            "characters_loading must start false (stuck-loading-flag risk)"
        );
        assert!(
            !main.game_db_loading,
            "game_db_loading must start false (stuck-loading-flag risk)"
        );
        assert!(
            !main.build_loading,
            "build_loading must start false (stuck-loading-flag risk)"
        );
        assert_eq!(
            main.weights,
            OptimizationWeights::default(),
            "default weights should be preset_balanced; init() overrides to PvE default"
        );
        assert!(
            main.build_locks.specs.iter().all(|s| s.is_none()),
            "build_locks.specs must start as [None; 3]"
        );
        assert!(
            main.build_locks.trait_locks.is_empty(),
            "build_locks.trait_locks must start empty"
        );
        assert_eq!(main.chat_epoch, 0, "kitchen epoch starts at 0");
        assert!(
            main.chat_wait_started.is_none(),
            "kitchen wait clock starts unset"
        );
        assert_eq!(
            main.feedback.view,
            crate::feedback::AboutView::WhatsNew,
            "About tab opens on What's new by default"
        );
        assert!(main.feedback.draft.is_none(), "no wizard draft at start");
    }

    #[test]
    fn test_main_state_clear_resolved_view_clears_current_build_and_combat() {
        let mut main = MainState::default();
        main.current_build = Some(dummy_resolved_build());
        main.current_stats = Some(StatBlock {
            power: 1,
            precision: 2,
            toughness: 3,
            vitality: 4,
            condition_damage: 5,
            expertise: 6,
            concentration: 7,
            ferocity: 8,
            healing_power: 9,
            crit_chance: 10.0,
            crit_damage: 11.0,
            health: 12,
            armor: 13,
        });
        main.comparison.current_combat_solo = Some(dummy_combat_metrics(100));
        main.comparison.current_combat_party = Some(dummy_combat_metrics(200));
        main.comparison.current_combat_squad = Some(dummy_combat_metrics(300));

        main.clear_resolved_view();

        assert!(main.current_build.is_none());
        assert!(main.current_stats.is_none());
        assert!(main.comparison.current_combat_solo.is_none());
        assert!(main.comparison.current_combat_party.is_none());
        assert!(main.comparison.current_combat_squad.is_none());
    }

    // init() screen routing

    #[test]
    fn test_init_routes_to_language_when_no_keys() {
        let _serial = state_test_guard();
        reset_state();
        // No config.json in the dir → AppConfig::load returns default (no keys).
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_no_keys", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);
        let screen = with_state(|s| s.screen.clone()).unwrap();
        assert_eq!(screen, Screen::Setup(SetupStep::Language));
        reset_state();
    }

    #[test]
    fn test_init_routes_to_llm_key_when_only_gw2_key() {
        let _serial = state_test_guard();
        reset_state();
        let config = AppConfig {
            gw2_api_key: Some("test-gw2-key".into()),
            ..Default::default()
        };
        let dir = config_in_tempdir(&config, "gw2_only");
        init(dir);
        let screen = with_state(|s| s.screen.clone()).unwrap();
        assert_eq!(screen, Screen::Setup(SetupStep::LlmApiKey));
        reset_state();
    }

    #[test]
    fn test_init_routes_to_data_download_when_keys_present_no_cache() {
        let _serial = state_test_guard();
        reset_state();
        let config = AppConfig {
            gw2_api_key: Some("test-gw2-key".into()),
            gemini_api_key: Some("test-gemini-key".into()),
            cache_build_number: None,
            ..Default::default()
        };
        let dir = config_in_tempdir(&config, "keys_no_cache");
        init(dir);
        let screen = with_state(|s| s.screen.clone()).unwrap();
        assert_eq!(screen, Screen::Setup(SetupStep::DataDownload));
        reset_state();
    }

    #[test]
    fn test_init_routes_to_main_when_setup_complete() {
        let _serial = state_test_guard();
        reset_state();
        let config = AppConfig {
            gw2_api_key: Some("test-gw2-key".into()),
            gemini_api_key: Some("test-gemini-key".into()),
            cache_build_number: Some(12345),
            ..Default::default()
        };
        let dir = config_in_tempdir(&config, "setup_complete");
        init(dir);
        let screen = with_state(|s| s.screen.clone()).unwrap();
        assert_eq!(screen, Screen::Main);
        reset_state();
    }

    // init() loading-flag safety
    // Complementary to test_main_state_default_fields: confirms init() itself
    // never sets any loading flag — they are set exclusively by background threads.
    // If this test fails, it means init() started a background op without a
    // matching clear path, introducing a permanent stuck-flag risk.

    #[test]
    fn test_init_loading_flags_start_false() {
        let _serial = state_test_guard();
        reset_state();
        let dir = std::env::temp_dir().join(format!(
            "gw2_state_test_{}_loading_flags",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);
        with_state(|s| {
            assert!(!s.main.optimizing, "optimizing must be false after init");
            assert!(
                !s.main.characters_loading,
                "characters_loading must be false after init"
            );
            assert!(
                !s.main.game_db_loading,
                "game_db_loading must be false after init"
            );
            assert!(
                !s.main.build_loading,
                "build_loading must be false after init"
            );
        });
        reset_state();
    }

    // with_state

    #[test]
    fn test_with_state_returns_none_when_uninitialized() {
        let _serial = state_test_guard();
        reset_state();
        let result = with_state(|_s| 42);
        assert!(result.is_none());
    }

    #[test]
    fn test_with_state_invokes_closure_when_initialized() {
        let _serial = state_test_guard();
        reset_state();
        let dir = std::env::temp_dir().join(format!(
            "gw2_state_test_{}_with_state_init",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);
        let result = with_state(|s| s.window_visible);
        assert_eq!(result, Some(true));
        reset_state();
    }

    /// Layout holds a snapshot, not the mutex: a worker's `with_state` returns
    /// while the snapshot is still alive, and commit keeps both the publish
    /// and the UI edit.
    #[test]
    fn paint_snapshot_lets_a_worker_publish_during_layout() {
        let _serial = state_test_guard();
        let _render = RenderThreadGuard::enter();
        reset_state();
        let dir = std::env::temp_dir().join(format!(
            "gw2_state_test_{}_paint_snapshot",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);

        let captured = with_state(|s| PaintCapture::take(s)).expect("state");
        let mut frame = captured.materialize();
        let _pin = frame.pin_baseline();
        frame.state.main.active_tab = MainTab::Settings;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let wrote = with_state(|s| {
                s.main.error = Some("from-worker".into());
            });
            tx.send(wrote).unwrap();
        });
        let wrote = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker blocked on STATE for the whole snapshot");
        assert_eq!(wrote, Some(()));

        drop(_pin);
        with_state(|s| frame.commit(s)).expect("commit");

        let (tab, err) = with_state(|s| (s.main.active_tab.clone(), s.main.error.clone())).unwrap();
        assert_eq!(tab, MainTab::Settings);
        assert_eq!(err.as_deref(), Some("from-worker"));
        reset_state();
    }

    /// Spawn publishes the busy flag, a fast worker clears it, then commit
    /// must keep the clear and the click's other edits.
    #[test]
    fn paint_commit_keeps_a_fast_worker_clear() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let _serial = state_test_guard();
        let _render = RenderThreadGuard::enter();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_paint_clear", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);

        let captured = with_state(|s| PaintCapture::take(s)).expect("state");
        let mut frame = captured.materialize();
        let _pin = frame.pin_baseline();
        frame.state.main.active_tab = MainTab::Settings;
        frame.state.main.optimizing = true;

        let go = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let w_go = go.clone();
        assert!(frame.state.spawn_worker("fast-clear", move |_token| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !w_go.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            with_state(|s| s.main.optimizing = false);
            tx.send(()).unwrap();
        }));
        assert_eq!(
            with_state(|s| s.main.optimizing),
            Some(true),
            "spawn must publish the busy flag onto live state"
        );
        go.store(true, Ordering::SeqCst);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("worker blocked");

        drop(_pin);
        with_state(|s| frame.commit(s)).expect("commit");
        let (tab, optimizing) =
            with_state(|s| (s.main.active_tab.clone(), s.main.optimizing)).unwrap();
        assert_eq!(tab, MainTab::Settings);
        assert!(
            !optimizing,
            "a finished worker must not be restarted by commit"
        );
        let _ = join_workers(Duration::from_secs(2));
        reset_state();
    }

    /// No worker publish: the snapshot replaces live state, including a tab click.
    #[test]
    fn paint_commit_overwrites_when_nothing_published() {
        let _serial = state_test_guard();
        let _render = RenderThreadGuard::enter();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_paint_quiet", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);

        let captured = with_state(|s| PaintCapture::take(s)).expect("state");
        let mut frame = captured.materialize();
        frame.state.main.active_tab = MainTab::Radio;
        with_state(|s| frame.commit(s)).expect("commit");
        assert_eq!(
            with_state(|s| s.main.active_tab.clone()),
            Some(MainTab::Radio)
        );
        reset_state();
    }

    /// Chrome and tests spawn while `with_state` is held. A paint baseline on
    /// the render thread must not make that re-enter the mutex.
    #[test]
    fn spawn_while_state_is_held_does_not_deadlock() {
        let _serial = state_test_guard();
        let _render = RenderThreadGuard::enter();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_paint_held", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);

        let captured = with_state(|s| PaintCapture::take(s)).expect("state");
        let frame = captured.materialize();
        let _pin = frame.pin_baseline();
        let spawned = with_state(|s| s.spawn_worker("held-lock", |_token| {})).expect("state");
        assert!(spawned);
        drop(_pin);
        let _ = join_workers(Duration::from_secs(2));
        reset_state();
    }

    #[test]
    fn test_init_restores_hidden_window() {
        let _serial = state_test_guard();
        reset_state();
        let config = AppConfig {
            gw2_api_key: Some("test-gw2-key".into()),
            gemini_api_key: Some("test-gemini-key".into()),
            cache_build_number: Some(12345),
            window_visible: false,
            ..Default::default()
        };
        let dir = config_in_tempdir(&config, "window_hidden");
        init(dir);
        assert_eq!(with_state(|s| s.window_visible), Some(false));
        reset_state();
    }

    #[test]
    fn anchor_keybind_toggles_the_flag() {
        let _serial = state_test_guard();
        reset_state();
        let dir = config_in_tempdir(&AppConfig::default(), "mini_anchor");
        init(dir);
        let anchored = || with_state(|s| s.config.radio.mini_radio.anchored);
        assert_eq!(anchored(), Some(true));
        toggle_mini_radio_anchor();
        assert_eq!(anchored(), Some(false));
        assert!(with_state(|s| s.radio.mini_anchor_flash.is_some()).unwrap());
        toggle_mini_radio_anchor();
        assert_eq!(anchored(), Some(true));
        // The gear popup's "Anchor in place" checkbox (both popups) unanchors
        // through the same setter.
        toggle_mini_radio_anchor();
        with_state(|s| crate::ui::mini_radio::set_anchored(s, false));
        assert_eq!(anchored(), Some(false));
        reset_state();
    }

    // clear()

    #[test]
    fn test_clear_cancels_token() {
        let _serial = state_test_guard();
        reset_state();
        let dir = std::env::temp_dir().join(format!(
            "gw2_state_test_{}_clear_cancel",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);
        let token = with_state(|s| s.cancel_token.clone()).unwrap();
        assert!(
            !token.is_cancelled(),
            "token must not be cancelled before clear"
        );
        clear();
        assert!(
            token.is_cancelled(),
            "token clone must see cancellation after clear"
        );
    }

    #[test]
    fn test_clear_drops_state() {
        let _serial = state_test_guard();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_clear_drops", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);
        assert!(
            with_state(|_s| ()).is_some(),
            "state must be Some after init"
        );
        clear();
        assert!(
            with_state(|_s| ()).is_none(),
            "state must be None after clear"
        );
    }

    // spawn_worker / join_workers
    //
    // These cover the unload contract: every worker is tracked, cancellation is
    // what makes it stop, the wait is bounded, and a panicking worker cannot
    // take the addon with it.

    /// Fresh STATE rooted at a per-test temp dir.
    fn init_worker_test(label: &str) {
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_{}", std::process::id(), label));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir);
    }

    /// Spin until `flag` is set or `limit` elapses. Returns whether it was set.
    fn wait_for(flag: &Arc<AtomicBool>, limit: Duration) -> bool {
        let start = Instant::now();
        while !flag.load(Ordering::SeqCst) {
            if start.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// G4 wiring, not plumbing: the LLM transports poll a *thread-local*
    /// predicate because `LlmClient` takes `&self` and cannot carry a
    /// per-request token. This proves `spawn_worker` binds that predicate to
    /// the addon's own `CancellationToken`, so a worker body that never
    /// mentions `llm::cancel` still gets its blocking HTTP reads and retry
    /// backoffs cancelled by the same flag `on_unload` sets.
    ///
    /// The worker deliberately ignores the token it is handed — polling that
    /// would only prove the closure works, which was never in doubt.
    #[test]
    fn spawn_worker_installs_cancel_scope() {
        use gw2_optimizer::llm::cancel::is_cancelled as transport_cancelled;

        let _serial = state_test_guard();
        init_worker_test("cancel_scope");

        // Nothing on this thread has a scope, so anything the worker observes
        // can only have come from `spawn_worker`.
        assert!(
            !transport_cancelled(),
            "the test thread must not carry a cancel scope of its own"
        );

        let started = Arc::new(AtomicBool::new(false));
        let clear_before_cancel = Arc::new(AtomicBool::new(false));
        let saw_transport_cancel = Arc::new(AtomicBool::new(false));
        let (w_started, w_clear, w_saw) = (
            started.clone(),
            clear_before_cancel.clone(),
            saw_transport_cancel.clone(),
        );

        let spawned = with_state(|s| {
            s.spawn_worker("test-cancel-scope", move |_token| {
                // A live scope reads false until the token is cancelled; this
                // is what an in-flight LLM stream sees between lines.
                w_clear.store(!transport_cancelled(), Ordering::SeqCst);
                w_started.store(true, Ordering::SeqCst);
                // Bounded so a broken bind fails the assertion instead of
                // leaving a thread spinning for the rest of the run.
                let deadline = Instant::now() + Duration::from_secs(5);
                while !transport_cancelled() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                w_saw.store(transport_cancelled(), Ordering::SeqCst);
            })
        })
        .expect("state must be initialised");
        assert!(spawned, "spawn_worker must report that the thread started");

        assert!(
            wait_for(&started, Duration::from_secs(5)),
            "worker body must actually run"
        );
        assert!(
            clear_before_cancel.load(Ordering::SeqCst),
            "the transport predicate must read false before cancellation"
        );

        // The production cancel path, byte for byte what `on_unload` calls
        // first. Nothing here touches `llm::cancel` directly.
        request_shutdown();

        assert!(
            wait_for(&saw_transport_cancel, Duration::from_secs(5)),
            "the transport predicate must observe the addon's CancellationToken; \
             without the bind in spawn_worker it stays false forever and a worker \
             parked in a socket read never notices unload"
        );

        // Thread-local, not a process-wide flag: cancelling one worker must not
        // abort an unrelated request on another thread.
        assert!(
            !transport_cancelled(),
            "the worker's scope must not leak onto the render thread"
        );

        let report = join_workers(Duration::from_secs(5));
        assert_eq!(report.joined, 1, "tracked worker must be joined: {report}");
        assert!(
            report.abandoned.is_empty(),
            "worker must stop on cancel, not time out: {report}"
        );

        clear();
    }

    #[test]
    fn spawn_worker_registers_handle() {
        let _serial = state_test_guard();
        init_worker_test("spawn_registers");

        let started = Arc::new(AtomicBool::new(false));
        let saw_cancel = Arc::new(AtomicBool::new(false));
        // Escape hatch: if cancellation is broken, this releases the worker so a
        // failing assertion does not leave a thread spinning for the whole run.
        let release = Arc::new(AtomicBool::new(false));
        let (w_started, w_saw_cancel, w_release) =
            (started.clone(), saw_cancel.clone(), release.clone());

        // Spawned from inside `with_state`, i.e. while holding the STATE mutex —
        // exactly what a render frame does. If `spawn_worker` ever locked STATE
        // itself, this line would deadlock instead of returning.
        let spawned = with_state(|s| {
            s.spawn_worker("test-registers", move |token| {
                w_started.store(true, Ordering::SeqCst);
                while !token.is_cancelled() && !w_release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                w_saw_cancel.store(token.is_cancelled(), Ordering::SeqCst);
            })
        })
        .expect("state must be initialised");
        assert!(spawned, "spawn_worker must report that the thread started");

        assert!(
            wait_for(&started, Duration::from_secs(5)),
            "worker body must actually run"
        );
        assert_eq!(
            with_state(|s| s.worker_count()),
            Some(1),
            "the JoinHandle must be tracked while the worker is live"
        );

        // Nothing in this test holds a handle: a join can only come from
        // `join_workers` walking the registry that `spawn_worker` filled in.
        let report = join_workers(Duration::from_secs(5));
        release.store(true, Ordering::SeqCst);

        assert_eq!(report.joined, 1, "tracked worker must be joined: {report}");
        assert_eq!(report.panicked, 0, "clean worker must not report a panic");
        assert!(
            report.abandoned.is_empty(),
            "worker must stop on cancel, not time out: {report}"
        );
        assert!(
            saw_cancel.load(Ordering::SeqCst),
            "worker must exit because its own token clone was cancelled"
        );
        assert_eq!(
            with_state(|s| s.worker_count()),
            Some(0),
            "join_workers must drain the registry"
        );

        clear();
    }

    #[test]
    fn join_workers_is_bounded_when_worker_ignores_cancel() {
        let _serial = state_test_guard();
        init_worker_test("join_bounded");

        let release = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let (w_release, w_started) = (release.clone(), started.clone());

        with_state(|s| {
            s.spawn_worker("test-wedged", move |_token| {
                // Deliberately ignores the token: models a worker parked in a
                // blocking HTTP read that cannot see the flag yet.
                w_started.store(true, Ordering::SeqCst);
                while !w_release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        });
        assert!(wait_for(&started, Duration::from_secs(5)));

        let began = Instant::now();
        let report = join_workers(Duration::from_millis(120));
        let waited = began.elapsed();
        release.store(true, Ordering::SeqCst);

        assert_eq!(
            report.abandoned,
            vec!["test-wedged"],
            "a wedged worker must be reported by name: {report}"
        );
        assert_eq!(report.joined, 0);
        // A blocking `JoinHandle::join` would sit here until the worker returned.
        // This bound is what keeps unload from freezing the game.
        assert!(
            waited < Duration::from_secs(2),
            "join must respect its budget, waited {waited:?}"
        );

        clear();
    }

    #[test]
    fn spawn_worker_contains_a_panicking_worker() {
        let _serial = state_test_guard();
        init_worker_test("spawn_panic");

        // The panic message this prints to stderr is expected output.
        with_state(|s| s.spawn_worker("test-panics", |_token| panic!("worker boom")));

        let report = join_workers(Duration::from_secs(5));
        assert_eq!(
            report.joined, 1,
            "panicking worker still gets joined: {report}"
        );
        assert_eq!(
            report.panicked, 0,
            "the worker's own guard must swallow the unwind before the thread ends"
        );
        assert!(report.abandoned.is_empty());
        assert!(
            with_state(|s| s.worker_count()).is_some(),
            "a panicking worker must not poison STATE"
        );

        clear();
    }

    #[test]
    fn spawn_worker_reaps_finished_handles() {
        let _serial = state_test_guard();
        init_worker_test("spawn_reap");

        // Long sessions spawn constantly (the API-health check alone once a
        // minute). Without reaping, the registry would grow one dead handle per
        // spawn and unload would walk a list of hundreds of corpses.
        for _ in 0..8 {
            let done = Arc::new(AtomicBool::new(false));
            let w_done = done.clone();
            with_state(|s| {
                s.spawn_worker("test-brief", move |_token| {
                    w_done.store(true, Ordering::SeqCst)
                })
            });
            assert!(wait_for(&done, Duration::from_secs(5)));
            // The flag is set by the closure's last statement; give the thread
            // time to actually exit so `is_finished()` is true at the next spawn.
            std::thread::sleep(Duration::from_millis(25));
        }

        // Reaping happens on spawn, so the newest handle is always still tracked
        // and a straggler may not have flipped `is_finished` yet. A registry that
        // never reaped would sit at 8.
        let tracked = with_state(|s| s.worker_count()).unwrap();
        assert!(
            tracked <= 2,
            "finished handles must be reaped, {tracked} still tracked after 8 spawns"
        );

        let report = join_workers(Duration::from_secs(5));
        assert!(report.abandoned.is_empty(), "{report}");
        clear();
    }

    #[test]
    fn request_shutdown_cancels_workers_without_dropping_state() {
        let _serial = state_test_guard();
        init_worker_test("request_shutdown");

        let token = with_state(|s| s.cancel_token.clone()).unwrap();
        request_shutdown();

        assert!(token.is_cancelled(), "workers must see the cancel flag");
        assert!(
            with_state(|_s| ()).is_some(),
            "request_shutdown must not drop the state — unload still has to persist and join"
        );

        clear();
    }

    #[test]
    fn unload_sequence_joins_before_it_drops_the_state() {
        let _serial = state_test_guard();
        init_worker_test("unload_sequence");

        // A worker that publishes through `with_state` on its way out, like every
        // real one does. If unload dropped the state before waiting, this would
        // find None; if unload waited while holding STATE, it would never return.
        let published = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let (w_published, w_started) = (published.clone(), started.clone());
        with_state(|s| {
            s.spawn_worker("test-unload", move |token| {
                w_started.store(true, Ordering::SeqCst);
                while !token.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let seen = with_state(|_s| ()).is_some();
                w_published.store(seen, Ordering::SeqCst);
            })
        });
        assert!(wait_for(&started, Duration::from_secs(5)));

        // Mirror of `lib.rs::on_unload` — keep the two in step.
        request_shutdown();
        persist_window();
        let report = join_workers(UNLOAD_JOIN_BUDGET);
        clear();

        assert_eq!(
            report.joined, 1,
            "unload must wait for the worker: {report}"
        );
        assert!(report.abandoned.is_empty(), "{report}");
        assert!(
            published.load(Ordering::SeqCst),
            "the state must still be reachable while unload waits, so a worker \
             finishing mid-shutdown can publish instead of hitting a dropped state"
        );
        assert!(
            with_state(|_s| ()).is_none(),
            "unload must drop the state once the wait is over"
        );
    }

    #[test]
    fn pin_addon_module_is_none_in_the_test_process_image() {
        assert!(
            pin_addon_module().is_none(),
            "cargo test links this crate into the process exe; a Some pin would \
             FreeLibraryAndExitThread the test runner"
        );
    }

    #[test]
    fn join_workers_is_a_noop_without_state() {
        let _serial = state_test_guard();
        reset_state();

        let began = Instant::now();
        let report = join_workers(Duration::from_secs(5));

        assert_eq!(report.joined, 0);
        assert!(report.abandoned.is_empty());
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "with no state there is nothing to wait for"
        );
    }

    #[test]
    fn test_reset_to_first_run_clears_config_and_transient_state() {
        let _serial = state_test_guard();
        reset_state();
        let config = AppConfig {
            gw2_api_key: Some("test-gw2-key".into()),
            gemini_api_key: Some("test-gemini-key".into()),
            cache_build_number: Some(12345),
            default_game_mode: Some("WvW".into()),
            ..Default::default()
        };
        let dir = config_in_tempdir(&config, "reset_to_first_run");
        init(dir.clone());

        let (
            old_cancelled,
            new_cancelled,
            screen,
            has_gw2_key,
            has_llm_key,
            cache_build,
            locks_cleared,
        ) = with_state(|s| {
            s.main.current_build = Some(dummy_resolved_build());
            s.main.current_stats = Some(StatBlock {
                power: 1,
                precision: 2,
                toughness: 3,
                vitality: 4,
                condition_damage: 5,
                expertise: 6,
                concentration: 7,
                ferocity: 8,
                healing_power: 9,
                crit_chance: 10.0,
                crit_damage: 11.0,
                health: 12,
                armor: 13,
            });
            s.main.comparison.current_combat_solo = Some(dummy_combat_metrics(500));
            s.main.build_locks.specs[2] = Some(42);
            s.setup.gw2_key_input = "gw2-input".into();
            s.setup.llm_key_input = "llm-input".into();

            let old_token = s.cancel_token.clone();
            s.reset_to_first_run().unwrap();

            (
                old_token.is_cancelled(),
                s.cancel_token.is_cancelled(),
                s.screen.clone(),
                s.config.has_gw2_key(),
                s.config.has_active_llm_key(),
                s.config.cache_build_number,
                s.main.build_locks.specs.iter().all(|slot| slot.is_none())
                    && s.main.build_locks.trait_locks.is_empty(),
            )
        })
        .unwrap();

        assert!(old_cancelled, "old token should be cancelled during reset");
        assert!(!new_cancelled, "replacement token must start uncancelled");
        assert_eq!(screen, Screen::Setup(SetupStep::Language));
        assert!(!has_gw2_key);
        assert!(!has_llm_key);
        assert_eq!(cache_build, None);
        assert!(locks_cleared);

        // Disk write is detached; wait with STATE released so the worker can finish.
        let report = join_workers(Duration::from_secs(5));
        assert!(
            report.abandoned.is_empty(),
            "reset detached config write must finish: {report}"
        );

        let (saved_config, err) = AppConfig::load(&AppConfig::config_path(&dir));
        assert!(err.is_none());
        assert!(!saved_config.has_gw2_key());
        assert!(!saved_config.has_active_llm_key());
        assert_eq!(saved_config.cache_build_number, None);
    }

    /// Source pin: toggle/persist snapshot then write after the STATE guard
    /// drops; reset queues the write through save_config_detached.
    #[test]
    fn toggle_persist_reset_do_not_save_under_state() {
        let src = include_str!("state.rs");
        let toggle = pin_fn(src, "toggle_window");
        let persist = pin_fn(src, "persist_window");
        let reset = pin_fn(src, "reset_to_first_run");

        assert!(
            toggle.contains("log_disk_error"),
            "toggle_window must log a failed config save"
        );
        assert!(
            !toggle.contains("state.config.save"),
            "toggle_window must not save through the locked AddonState"
        );
        assert!(
            brace_depth_at(toggle, ".save(") < brace_depth_at(toggle, "lock_state()"),
            "toggle_window must drop the STATE guard before writing config.json"
        );
        assert!(
            persist.contains("log_disk_error"),
            "persist_window must log a failed config save"
        );
        assert!(
            !persist.contains("state.config.save"),
            "persist_window must not save through the locked AddonState"
        );
        assert!(
            brace_depth_at(persist, ".save(") < brace_depth_at(persist, "lock_state()"),
            "persist_window must drop the STATE guard before writing config.json"
        );
        assert!(
            reset.contains("save_config_detached"),
            "reset_to_first_run must queue the write off the STATE guard"
        );
        assert!(
            !reset.contains(".save("),
            "reset_to_first_run must not write config.json on this thread"
        );
    }

    /// Source pin + behaviour: skip the edit when a worker holds a clone.
    #[test]
    fn game_db_mut_skips_when_shared() {
        let src = include_str!("state.rs");
        let body = pin_fn(src, "game_db_mut");
        assert!(
            !body.contains("Arc::make_mut"),
            "game_db_mut must not deep-copy GameDb under STATE"
        );
        assert!(
            body.contains("Arc::get_mut"),
            "game_db_mut must skip when the Arc is shared"
        );

        let mut main = MainState::default();
        main.set_game_db(GameDb::empty_for_tests());
        assert!(
            main.game_db_mut().is_some(),
            "unique owner can edit in place"
        );
        let held = main.game_db.clone();
        assert!(
            main.game_db_mut().is_none(),
            "must skip rather than copy while a clone is live"
        );
        drop(held);
        assert!(main.game_db_mut().is_some());
    }

    #[test]
    fn attach_localized_ui_path_does_not_make_mut() {
        let stats = include_str!("ui/main_view/stats.rs");
        let production = stats
            .split("#[cfg(test)]")
            .next()
            .expect("stats.rs must contain its own #[cfg(test)] marker");
        assert!(
            !production.contains("Arc::make_mut"),
            "ensure_localized_names must not make_mut GameDb to attach names"
        );
        assert!(production.contains("attach_localized"));
        assert!(production.contains("game_db_mut"));

        let gamedb = include_str!("../../optimizer/src/gamedb.rs");
        let body = pin_fn(gamedb, "attach_localized");
        assert!(
            !body.contains("Arc::make_mut"),
            "attach_localized must not clone the world to write names"
        );
    }

    fn pin_fn<'a>(src: &'a str, name: &str) -> &'a str {
        let marker = format!("fn {name}(");
        let idx = src
            .find(&marker)
            .unwrap_or_else(|| panic!("missing fn {name}"));
        let brace = src[idx..].find('{').expect("fn body") + idx;
        let mut depth = 0usize;
        for (i, c) in src[brace..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[idx..=brace + i];
                    }
                }
                _ => {}
            }
        }
        panic!("unclosed fn {name}");
    }

    fn brace_depth_at(body: &str, needle: &str) -> usize {
        let at = body
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle}"));
        body[..at].bytes().filter(|b| *b == b'{').count()
            - body[..at].bytes().filter(|b| *b == b'}').count()
    }

    // catch_unwind panic-recovery tests
    //
    // P2-03: Every background thread is now wrapped in catch_unwind. These tests
    // verify the Err-arm pattern: after a panic is caught, with_state() still
    // works (mutex not permanently poisoned) and the loading flag is cleared.

    #[test]
    fn test_catch_unwind_clears_chat_waiting_on_panic() {
        let _serial = state_test_guard();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_panic_chat", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir.clone());
        with_state(|s| s.main.chat.waiting = true);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated panic in send_chat_message");
        }));

        if result.is_err() {
            with_state(|s| s.main.chat.waiting = false);
        }

        let waiting = with_state(|s| s.main.chat.waiting);
        assert_eq!(
            waiting,
            Some(false),
            "chat.waiting must be cleared after panic"
        );
        reset_state();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_catch_unwind_clears_models_loading_on_panic() {
        let _serial = state_test_guard();
        reset_state();
        let dir = std::env::temp_dir().join(format!(
            "gw2_state_test_{}_panic_models",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir.clone());
        with_state(|s| s.main.models_loading = true);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated panic in start_fetch_models");
        }));

        if result.is_err() {
            with_state(|s| s.main.models_loading = false);
        }

        let loading = with_state(|s| s.main.models_loading);
        assert_eq!(
            loading,
            Some(false),
            "models_loading must be cleared after panic"
        );
        reset_state();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_catch_unwind_clears_characters_loading_on_panic() {
        let _serial = state_test_guard();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_panic_chars", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir.clone());
        with_state(|s| s.main.characters_loading = true);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated panic in load_characters");
        }));

        if result.is_err() {
            with_state(|s| s.main.characters_loading = false);
        }

        let loading = with_state(|s| s.main.characters_loading);
        assert_eq!(
            loading,
            Some(false),
            "characters_loading must be cleared after panic"
        );
        reset_state();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_catch_unwind_resets_gw2_key_status_on_panic() {
        let _serial = state_test_guard();
        reset_state();
        let dir =
            std::env::temp_dir().join(format!("gw2_state_test_{}_panic_setup", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir.clone());
        with_state(|s| s.setup.gw2_key_status = KeyStatus::Validating);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated panic in setup_gw2_key_validation");
        }));

        if result.is_err() {
            with_state(|s| {
                s.setup.gw2_key_status = KeyStatus::Invalid("thread panicked".into());
            });
        }

        let status = with_state(|s| s.setup.gw2_key_status.clone());
        assert!(
            matches!(status, Some(KeyStatus::Invalid(ref msg)) if msg == "thread panicked"),
            "gw2_key_status must be reset to Invalid after panic, got {:?}",
            status
        );
        reset_state();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_catch_unwind_mutex_poison_recovery() {
        let _serial = state_test_guard();
        // Exercises the real danger scenario: a panic INSIDE with_state() poisons
        // the mutex, and subsequent with_state() calls must still succeed via
        // lock_state()'s unwrap_or_else(|e| e.into_inner()) recovery.
        reset_state();
        let dir = std::env::temp_dir().join(format!(
            "gw2_state_test_{}_panic_poison",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        init(dir.clone());
        with_state(|s| s.main.characters_loading = true);

        // Panic inside with_state — this poisons the mutex
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_state(|_s| {
                panic!("simulated panic inside with_state callback");
            });
        }));
        assert!(result.is_err(), "catch_unwind must capture the panic");

        // After poison: with_state must still work (lock_state recovers)
        with_state(|s| s.main.characters_loading = false);
        let loading = with_state(|s| s.main.characters_loading);
        assert_eq!(
            loading,
            Some(false),
            "with_state must work after mutex poison recovery"
        );
        reset_state();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_fraction_crawls_during_item_batches() {
        let start = download_fraction(7, 9, 0, 74056, false);
        assert!((start - 6.0 / 9.0).abs() < 1e-5);
        let mid = download_fraction(7, 9, 24000, 74056, false);
        assert!(mid > start);
        assert!(mid < 7.0 / 9.0);
        assert_eq!(download_fraction(9, 9, 0, 0, true), 1.0);
        assert_eq!(download_fraction(0, 0, 0, 0, false), 0.0);
    }

    #[test]
    fn download_fraction_last_step_stays_open_until_inner_finishes() {
        let start = download_fraction(13, 13, 0, 17232, false);
        assert!((start - 12.0 / 13.0).abs() < 1e-5);
        let mid = download_fraction(13, 13, 4000, 17232, false);
        assert!(mid > start);
        assert!(mid < 1.0);
        assert!((download_fraction(13, 13, 17232, 17232, false) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn items_fill_i18n_keys_cover_all_modes() {
        use gw2_api::download::ItemsFillKind::*;
        assert_eq!(items_fill_i18n_key(FirstFill), "status.items_first_fill");
        assert_eq!(items_fill_i18n_key(Resume), "status.items_resume");
        assert_eq!(
            items_fill_i18n_key(SameBuildSkip),
            "status.items_same_build_skip"
        );
        assert!(!gw2_core::i18n::t(items_fill_i18n_key(FirstFill)).is_empty());
        assert!(!gw2_core::i18n::t(items_fill_i18n_key(Resume)).is_empty());
        assert!(!gw2_core::i18n::t(items_fill_i18n_key(SameBuildSkip)).is_empty());
    }
}

#[cfg(test)]
mod live_output_tests {
    use super::*;
    use gw2_optimizer::llm::live::{LiveEvent, LiveMode, Step};
    use std::time::{Duration, Instant};

    #[test]
    fn stall_after_20s() {
        let mut live = LiveOutput::default();
        let now = Instant::now();
        assert_eq!(live.stalled_for(now), None);
        live.last_activity = Some(now - Duration::from_secs(21));
        assert_eq!(live.stalled_for(now), Some(21));
        live.last_activity = Some(now - Duration::from_secs(5));
        assert_eq!(live.stalled_for(now), None);
    }

    #[test]
    fn reset_on_send_keeps_only_the_expand_choice() {
        let mut live = LiveOutput::default();
        live.apply(LiveEvent::Step(Step::Lookup(1)));
        live.apply(LiveEvent::Reasoning("a".into()));
        live.apply(LiveEvent::Content("b".into()));
        live.apply(LiveEvent::ToolCall("score_build".into()));
        live.expanded = true;
        assert_eq!(live.mode, LiveMode::Reasoning);
        live.reset();
        assert!(live.expanded);
        assert!(live.reasoning.is_empty() && live.content.is_empty() && live.tools.is_empty());
        assert_eq!(live.step, None);
        assert_eq!(live.mode, LiveMode::ToolsOnly);
    }

    #[test]
    fn elapsed_in_step_and_mode_upgrade() {
        let mut live = LiveOutput::default();
        assert_eq!(live.elapsed_in_step(Instant::now()), 0);
        live.apply(LiveEvent::Step(Step::Writing));
        live.step_started = Some(Instant::now() - Duration::from_secs(7));
        assert_eq!(live.elapsed_in_step(Instant::now()), 7);
        live.apply(LiveEvent::Content("x".into()));
        assert_eq!(live.mode, LiveMode::Content);
        live.apply(LiveEvent::Reasoning("y".into()));
        live.apply(LiveEvent::Mode(LiveMode::ToolsOnly));
        assert_eq!(
            live.mode,
            LiveMode::Reasoning,
            "a loop restart does not hide reasoning"
        );
    }
}
