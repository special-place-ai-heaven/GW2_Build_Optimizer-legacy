//! Playback core: stream-download + icy-metadata + rodio on a dedicated
//! tokio runtime (1 worker), owned by this module behind a
//! `Mutex<Option<...>>` so `shutdown` can tear it down deterministically.
//!
//! ## Ownership
//!
//! Three statics own everything:
//! - [`RUNTIME`] owns the tokio runtime (created lazily on first play; taken
//!   and shut down by [`shutdown`]).
//! - [`SESSION`] owns the live station session: its stop flag, the shared
//!   sink handle for [`set_volume`], and the audio-owner thread's JoinHandle.
//! - [`VOLUME_PERCENT`] mirrors the config volume so a slider move during
//!   tune-in is never lost.
//!
//! The audio-owner thread (`gw2bo-radio-audio`) owns the rodio
//! `MixerDeviceSink` + `Player` for the lifetime of one station session and is
//! the only thread that touches rodio/cpal — never the render thread, never a
//! tokio worker. It also owns every status write for its session, which keeps
//! the public entry points safe to call from the render thread even while the
//! STATE mutex is held (they never lock STATE on the calling thread).
//!
//! Each connection gets one decode thread (`gw2bo-radio-decode`, owned by the
//! audio-owner thread through [`Decoding`]) that reads the network stream,
//! decodes it, and appends ~[`CHUNK`] PCM buffers to the player, keeping
//! ~[`QUEUE_TARGET`] queued. The cpal output callback therefore only reads
//! memory: a slow network read starves the queue, never the callback.
//! Dropping the [`Decoding`] halts the thread, cancels the download and waits
//! for it (bounded by [`DECODE_JOIN_BUDGET`]).
//!
//! ## Locking discipline
//!
//! [`play`], [`set_volume`], [`pause`] and [`resume`] never block: safe from
//! any thread, including inside `with_state`. [`pause`]/[`resume`] do not
//! write `RadioStatus` at all — the UI caller (or [`toggle`]) writes it
//! optimistically, the same way the Stop button writes `Stopped`; the audio
//! thread deliberately goes quiet while its session is paused. [`stop`]'s
//! bounded join can cost up to its ~1s budget if the caller holds STATE while
//! the audio thread is waiting on it — the join is bounded precisely so that
//! worst case is a hitch, never a deadlock. [`toggle`] reads STATE on the
//! calling thread and must only be called from threads that do not hold it
//! (the keybind handler path). [`duck_tick`] is the one render-thread-only
//! entry point: it locks STATE itself, so it must never be called from inside
//! `with_state`.

use std::io::{Read, Seek};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gw2_core::config::SavedStation;
use icy_metadata::{IcyHeaders, IcyMetadataReader};
use rodio::buffer::SamplesBuffer;
use rodio::source::SeekError;
use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, Player, Sample, SampleRate, Source};
use stream_download::http::{reqwest, HttpStream};
use stream_download::storage::bounded::BoundedStorageProvider;
use stream_download::storage::memory::MemoryStorageProvider;
use stream_download::{Settings, StreamDownload};

use super::{NowPlayingCell, RadioStatus, RbStation};

/// How many bytes to buffer before playback may start. At a typical 128 kbps
/// (16 KiB/s) this is ~8 s of cushion against delivery jitter (raised from
/// 64 KiB after in-game stutter reports on marginal stations). Icecast sends
/// a burst on connect, so on most stations this fills near-instantly; the
/// Buffering status makes the wait visible on the ones that trickle.
const PREFETCH_BYTES: u64 = 128 * 1024;

/// Size of the in-memory ring buffer holding the live stream. Must comfortably
/// exceed [`PREFETCH_BYTES`]; ~30 s of audio at 128 kbps.
const RING_BUFFER_BYTES: usize = 512 * 1024;

/// Decoded PCM per chunk the decode thread hands the player. The output
/// callback only ever reads these in-memory chunks, never the network.
const CHUNK: Duration = Duration::from_millis(100);

/// Decoded audio the decode thread keeps queued ahead of the output callback:
/// the callback's cushion against a slow read, a TLS stall or a frame spike
/// starving the decode thread.
const QUEUE_TARGET: Duration = Duration::from_millis(2500);

/// Decoded audio queued before a (re)connect starts playing, so playback
/// never opens on a near-empty queue.
const QUEUE_PRIME: Duration = Duration::from_secs(1);

/// Cap on the priming wait; past it playback starts with what is queued.
const PRIME_BUDGET: Duration = Duration::from_secs(5);

/// Decode-thread sleep while the queue sits at [`QUEUE_TARGET`] (no spin).
const DECODE_POLL: Duration = Duration::from_millis(20);

/// Queue depth below which the decode thread counts a dip.
const LOW_WATER: Duration = Duration::from_millis(500);

/// At most one low-water log line per this interval.
const LOW_WATER_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Bounded wait for a halted decode thread. A halt cancels the download,
/// which wakes a read blocked on the network, so the thread normally ends in
/// milliseconds; 50ms stop poll + this stays inside [`SHUTDOWN_JOIN_BUDGET`].
const DECODE_JOIN_BUDGET: Duration = Duration::from_millis(300);

/// Cap on establishing the TCP+TLS connection to the station.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on the connect + header-wait phase (`HttpStream::new`). Bounds ONLY the
/// tune-in handshake — the audio body is an infinite stream and must never be
/// subject to a request timeout.
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the watchdog checks for an unexpected stall.
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

/// How finely the watchdog polls the stop flag between sink checks, so
/// [`stop`] interrupts a session promptly.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A reconnect is quiet exactly once per stall: after a reconnect the stream
/// must play this long before another stall earns a new attempt, so a
/// flapping stream errors out instead of hot-looping connect attempts.
const STALL_REARM: Duration = Duration::from_secs(10);

/// Watchdog ticks skipped after a [`resume`] before the empty-sink stall
/// check re-arms (~1 s at [`WATCHDOG_INTERVAL`]): the sink needs a moment to
/// start draining the ring buffer again before "empty" means anything. If the
/// server dropped the stream during the pause, the re-armed check stalls and
/// the existing quiet-reconnect machinery re-tunes on its own.
const RESUME_GRACE_TICKS: u32 = 2;

/// Bounded join budget for [`stop`].
const STOP_JOIN_BUDGET: Duration = Duration::from_secs(1);

/// Bounded join budget inside [`shutdown`]: 600ms join + 700ms runtime
/// shutdown stays well inside `state::UNLOAD_JOIN_BUDGET` (1500ms).
const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_millis(600);

/// Runtime teardown cap inside [`shutdown`].
const RUNTIME_SHUTDOWN_BUDGET: Duration = Duration::from_millis(700);

/// UI cap for stream titles and error messages (`chars`, never bytes).
const MAX_TITLE_CHARS: usize = 200;

/// Visualizer bands rendered behind the player bar.
pub const EQ_BANDS: usize = 24;

/// Mono samples the visualizer tap ring holds. Power of two so the write
/// index is a cheap modulo; ~85 ms at 48 kHz — several UI frames of headroom
/// over [`EQ_WINDOW`].
const TAP_LEN: usize = 4096;

/// Newest mono samples analyzed per rendered frame (~21 ms at 48 kHz).
const EQ_WINDOW: usize = 1024;

/// Log-spaced band centers run from [`EQ_FREQ_MIN`] to [`EQ_FREQ_MAX`] Hz.
const EQ_FREQ_MIN: f32 = 50.0;
const EQ_FREQ_MAX: f32 = 12_000.0;

/// Perceptual range of a bar: a band at full scale is 1.0; this many dB
/// below full scale is 0.
const EQ_RANGE_DB: f32 = 50.0;

/// Per-frame smoothing factors: bars rise fast and fall slow, so a beat
/// snaps up and playback stopping lets the bars sink gracefully.
const EQ_ATTACK: f32 = 0.5;
const EQ_DECAY: f32 = 0.08;

/// Gain multiplier while combat-ducked: ~-10.5 dB under the slider's level —
/// the music steps back without vanishing.
const DUCK_FLOOR: f32 = 0.30;

/// Per-frame exponential approach rate for the duck ramp: settles in ~22
/// frames (~350 ms at 60 fps), inside the 250-400 ms design window. Same
/// per-frame-constant style as [`EQ_ATTACK`]/[`EQ_DECAY`].
const DUCK_STEP: f32 = 0.18;

/// Snap-to-target threshold ending a duck ramp, so a settled duck factor
/// costs zero sink writes per frame.
const DUCK_SNAP: f32 = 0.01;

/// The bounds rodio's decoder needs from the reader stack, as one nameable
/// trait so the two shapes (with/without ICY metadata) can be boxed.
trait StreamReader: Read + Seek + Send + Sync {}
impl<T: Read + Seek + Send + Sync> StreamReader for T {}

/// The dedicated playback runtime. Not `OnceLock`: [`shutdown`] must `take()`
/// it and `shutdown_timeout` it deterministically before the DLL unloads.
static RUNTIME: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);

/// The live station session, if any.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Last requested volume percent; seeded from config on play so a slider move
/// during the connect phase still wins over the config snapshot.
static VOLUME_PERCENT: AtomicU8 = AtomicU8::new(60);

/// Combat-duck gain factor as f32 bits, composed into every sink gain write
/// by [`apply_gain`]. Written only by the render thread ([`duck_tick`]); read
/// from any thread. `0x3F80_0000` is `1.0f32` — not ducked.
static DUCK_FACTOR: AtomicU32 = AtomicU32::new(0x3F80_0000);

/// One station session. Owned by [`SESSION`]; the audio-owner thread holds
/// clones of the stop flag and the sink cell, never the session itself.
/// Monotone session generation: bumped by every [`start_session`] and
/// [`request_stop`]. A session thread may only write status/config while its
/// generation is still current - a detached predecessor's late writes (stale
/// `Playing`, wrong `last_station`) are discarded at the lock, where the
/// check and the write are atomic with respect to the successor's writes.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Sample tap for the player-bar visualizer: installed by the session that
/// owns playback, read by [`eq_levels`] on the render thread. Installs are
/// generation-checked under this lock, so a replaced session can never
/// resurrect its tap over the successor's (same discipline as STATE writes).
static TAP: Mutex<Option<Arc<TapBuffer>>> = Mutex::new(None);

/// Smoothed band levels + staleness cursor. Render-thread only, but a static
/// so bars decay across tab close/reopen instead of snapping to full height.
static EQ_STATE: Mutex<EqState> = Mutex::new(EqState {
    levels: [0.0; EQ_BANDS],
    last_cursor: 0,
});

fn still_current(my_gen: u64) -> bool {
    GENERATION.load(Ordering::Acquire) == my_gen
}

/// Lock-free ring of the newest decoded mono samples: written by the decode
/// thread (via [`SampleTap`]), read by the render thread (via
/// [`eq_levels`]). Single writer; `cursor` counts mono samples ever written
/// and `cursor % TAP_LEN` is the next slot. Samples are stored as f32 bits in
/// `AtomicU32`s — a window torn by a concurrent overwrite is a one-frame
/// visual glitch, never UB and never a NaN on screen (see [`analyze_bands`]).
struct TapBuffer {
    samples: Vec<AtomicU32>,
    cursor: AtomicUsize,
    /// Source sample rate for the samples currently in the ring; refreshed by
    /// the tap when a new span changes it.
    sample_rate: AtomicU32,
}

impl TapBuffer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            samples: (0..TAP_LEN).map(|_| AtomicU32::new(0)).collect(),
            cursor: AtomicUsize::new(0),
            sample_rate: AtomicU32::new(0),
        })
    }

    /// Decode-thread write path: two relaxed stores plus one release store —
    /// wait-free, no locks, no allocation, no panic.
    fn push(&self, sample: f32) {
        let at = self.cursor.load(Ordering::Relaxed);
        self.samples[at % TAP_LEN].store(sample.to_bits(), Ordering::Relaxed);
        // Release pairs with the reader's acquire: whoever sees the new
        // cursor also sees the sample it covers.
        self.cursor.store(at.wrapping_add(1), Ordering::Release);
    }
}

/// Render-side visualizer state: the smoothed levels and the tap cursor last
/// analyzed — an unchanged cursor means no new audio, which decays the bars.
struct EqState {
    levels: [f32; EQ_BANDS],
    last_cursor: usize,
}

/// Pass-through `Source` adapter mirroring rodio's own `Amplify` delegation
/// exactly, additionally folding each interleaved frame down to one mono
/// sample for the visualizer tap. It wraps the decoder BEFORE the sink
/// applies volume gain, so bar heights track the stream itself, not the
/// volume slider — deliberate: the visualizer stays alive at low volume.
/// It runs on the decode thread, so the bars lead the audible audio by the
/// queue depth (~[`QUEUE_TARGET`]).
struct SampleTap<I> {
    input: I,
    tap: Arc<TapBuffer>,
    /// Interleaved position within the current frame + running frame sum.
    frame_pos: u16,
    frame_sum: f32,
    /// Span parameters cached at the last frame boundary.
    chans: u16,
    rate: u32,
}

impl<I: Source> SampleTap<I> {
    fn new(input: I, tap: Arc<TapBuffer>) -> Self {
        let chans = input.channels().get();
        let rate = input.sample_rate().get();
        tap.sample_rate.store(rate, Ordering::Relaxed);
        Self {
            input,
            tap,
            frame_pos: 0,
            frame_sum: 0.0,
            chans,
            rate,
        }
    }
}

impl<I: Source> Iterator for SampleTap<I> {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        let sample = self.input.next()?;
        if self.frame_pos == 0 {
            // Frame boundary: spans are frame-aligned, so channel count and
            // sample rate can only have changed here.
            self.chans = self.input.channels().get();
            let rate = self.input.sample_rate().get();
            if rate != self.rate {
                self.rate = rate;
                self.tap.sample_rate.store(rate, Ordering::Relaxed);
            }
        }
        self.frame_sum += sample;
        self.frame_pos += 1;
        if self.frame_pos >= self.chans {
            // Average the frame's channels — proper mono fold, not raw
            // interleaved pushes.
            self.tap.push(self.frame_sum / f32::from(self.chans));
            self.frame_sum = 0.0;
            self.frame_pos = 0;
        }
        Some(sample)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.input.size_hint()
    }
}

impl<I: Source> Source for SampleTap<I> {
    #[inline]
    fn current_span_len(&self) -> Option<usize> {
        self.input.current_span_len()
    }

    #[inline]
    fn channels(&self) -> ChannelCount {
        self.input.channels()
    }

    #[inline]
    fn sample_rate(&self) -> SampleRate {
        self.input.sample_rate()
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }

    #[inline]
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let out = self.input.try_seek(pos);
        if out.is_ok() {
            // Restart the mono fold on a clean frame boundary — a mid-frame
            // seek would otherwise smear pre-seek samples into the first
            // post-seek frame. Dead path for live radio, kept exact anyway.
            self.frame_pos = 0;
            self.frame_sum = 0.0;
        }
        out
    }
}

struct Session {
    /// Set by [`stop`]/[`play`] to end the session; polled by the audio thread.
    stop: Arc<AtomicBool>,
    /// Set by [`pause`]/[`resume`]; read by the audio thread's watchdog,
    /// which suspends its empty-sink stall check while the flag is up (a
    /// paused sink drains to silence and icecast may drop the idle socket —
    /// both are fine). Never read through STATE.
    paused: Arc<AtomicBool>,
    /// Sink handle published by the audio thread once the device is open, so
    /// [`set_volume`] can reach the live sink without touching the thread.
    sink: Arc<Mutex<Option<Arc<Player>>>>,
    /// JOINed (bounded) by [`stop`]/[`shutdown`], or by the next session's
    /// audio thread when [`play`] replaces this session.
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Start playing `station`, replacing any current stream.
///
/// Never blocks and never locks STATE on the calling thread: the old session
/// is signalled here but joined by the new audio-owner thread, and every
/// status write happens on that thread. Safe to call from inside `with_state`.
pub fn play(station: &RbStation) {
    start_session(station.clone());
}

/// Stop playback and release the audio device. Cheap to call when idle.
///
/// Signals the session flag, then JOINs the audio-owner thread (bounded ~1s);
/// the thread drops the sink/stream and writes `Stopped` as its last act.
/// Keybind/unload path ONLY - never call while STATE is held (the join waits
/// on a status write into that lock); the tab UI uses [`request_stop`].
pub fn stop() {
    stop_session(STOP_JOIN_BUDGET);
}

/// Signal-only stop for callers that may hold STATE (the tab's Stop button,
/// settings reset): raises the stop flag and invalidates the session
/// generation, so the dying thread's late status writes are discarded - the
/// caller owns the status from here. Never joins and never locks STATE; the
/// audio thread reaps itself (~50ms) and its handle is joined by the next
/// [`play`] or by [`shutdown`].
pub fn request_stop() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    if let Some(session) = lock_or_recover(&SESSION).as_ref() {
        session.stop.store(true, Ordering::Release);
    }
}

/// Pause the live sink in place, keeping the session (and its buffered
/// audio) alive for an instant [`resume`]. Never blocks and never locks
/// STATE on the calling thread — safe from the render thread inside
/// `with_state`. Deliberately does NOT write `RadioStatus`: the caller sets
/// `Paused` optimistically, exactly like the Stop button sets `Stopped`
/// after [`request_stop`]. No-op without a live session.
pub fn pause() {
    if let Some(sink) = flag_paused(true) {
        sink.pause();
    }
}

/// Resume a paused sink from its buffer. Same contract as [`pause`]: never
/// blocks, never locks STATE, status written optimistically by the caller.
/// The watchdog re-arms its stall check after a short grace
/// ([`RESUME_GRACE_TICKS`]), so a stream that died during the pause stalls
/// honestly and the quiet-reconnect machinery re-tunes it.
pub fn resume() {
    if let Some(sink) = flag_paused(false) {
        sink.play();
    }
}

/// Flip the session's paused flag and hand back the live sink, if any. The
/// flag is flipped BEFORE the sink is touched so the watchdog can never see
/// a pausing sink with an armed stall check.
fn flag_paused(paused: bool) -> Option<Arc<Player>> {
    if is_shut_down() {
        return None;
    }
    let guard = lock_or_recover(&SESSION);
    let session = guard.as_ref()?;
    session.paused.store(paused, Ordering::Release);
    let sink = lock_or_recover(&session.sink).clone();
    // Both module locks are released before the caller touches the sink.
    drop(guard);
    sink
}

/// What the keybind toggle does for a given status. Pure: the whole toggle
/// state machine in one testable place.
#[derive(Debug, PartialEq, Eq)]
enum ToggleAction {
    Pause,
    Resume,
    Stop,
    Tune,
    Nothing,
}

fn toggle_action(status: &RadioStatus, has_station: bool) -> ToggleAction {
    match status {
        RadioStatus::Playing => ToggleAction::Pause,
        RadioStatus::Paused => ToggleAction::Resume,
        RadioStatus::Connecting | RadioStatus::Buffering | RadioStatus::Stalled => {
            ToggleAction::Stop
        }
        _ if has_station => ToggleAction::Tune,
        _ => ToggleAction::Nothing,
    }
}

/// Keybind toggle: playing -> pause, paused -> resume, tuning/stalled ->
/// stop; otherwise re-tune the current/last station.
///
/// Reads STATE on the calling thread — keybind-handler path only; must not be
/// called while STATE is held (the tab UI calls the entry points directly).
/// Pause/resume run inside the STATE closure ([`pause`]/[`resume`] never lock
/// STATE), which keeps the optimistic status write atomic with the session
/// thread's generation-gated writes; stop/play defer until STATE is released
/// because [`stop`] joins a thread that may be waiting on a STATE write.
pub fn toggle() {
    enum Deferred {
        Stop,
        Play(Box<RbStation>),
    }
    let deferred = crate::state::with_state(|s| {
        let station = s
            .radio
            .current
            .clone()
            .or_else(|| s.config.radio.last_station.as_ref().map(station_from_saved));
        match toggle_action(&s.radio.status, station.is_some()) {
            ToggleAction::Pause => {
                pause();
                s.radio.status = RadioStatus::Paused;
                None
            }
            ToggleAction::Resume => {
                resume();
                s.radio.status = RadioStatus::Playing;
                None
            }
            ToggleAction::Stop => Some(Deferred::Stop),
            ToggleAction::Tune => station.map(|s| Deferred::Play(Box::new(s))),
            ToggleAction::Nothing => None,
        }
    })
    .flatten();
    match deferred {
        Some(Deferred::Stop) => stop(),
        Some(Deferred::Play(station)) => play(&station),
        // Nothing ever played and nothing saved — or the addon is unloading.
        None => {}
    }
}

/// Output gain from 0-100 percent; log taper applied inside.
///
/// Applies to the live sink if any and always returns fast. Never locks STATE.
pub fn set_volume(percent: u8) {
    VOLUME_PERCENT.store(percent, Ordering::Release);
    apply_gain();
}

/// Push the effective gain — slider taper × combat-duck factor — to the live
/// sink, if any. The two compose and never write each other's inputs, so the
/// duck ramp cannot fight the slider (and percent 0 stays muted regardless of
/// the duck factor). Sink gain only: the visualizer tap is pre-gain by
/// design. Never locks STATE.
fn apply_gain() {
    if is_shut_down() {
        return;
    }
    let sink = lock_or_recover(&SESSION)
        .as_ref()
        .and_then(|s| lock_or_recover(&s.sink).clone());
    if let Some(sink) = sink {
        let duck = f32::from_bits(DUCK_FACTOR.load(Ordering::Relaxed));
        sink.set_volume(volume_gain(VOLUME_PERCENT.load(Ordering::Acquire)) * duck);
    }
}

/// Combat-duck driver: call once per rendered frame. Render-thread ONLY, and
/// never from inside `with_state` — unlike every other entry point here it
/// takes the STATE lock itself (for the config flag). Reads the mumble
/// link's `IS_IN_COMBAT` bit and ramps [`DUCK_FACTOR`] toward [`DUCK_FLOOR`]
/// (ducking enabled + in combat) or 1.0, pushing the composed gain to the
/// sink only while the ramp is actually moving; a settled factor costs one
/// STATE read and (when enabled) one shared-memory read per frame.
pub fn duck_tick() {
    let enabled = crate::state::with_state(|s| s.config.radio.duck_in_combat).unwrap_or(false);
    let target = if enabled && mumble_in_combat() {
        DUCK_FLOOR
    } else {
        1.0
    };
    let current = f32::from_bits(DUCK_FACTOR.load(Ordering::Relaxed));
    let next = duck_step(current, target);
    if next != current {
        DUCK_FACTOR.store(next.to_bits(), Ordering::Relaxed);
        apply_gain();
    }
}

/// True when the mumble link reports the player in combat. A missing link
/// (character select, mumble disabled) reads as out of combat; the read is a
/// volatile load from Nexus's shared memory — no locks, no allocation beyond
/// the identifier lookup.
fn mumble_in_combat() -> bool {
    nexus::data_link::get_mumble_link()
        .map(|link| {
            link.read_ui_state()
                .contains(nexus::data_link::mumble::UiState::IS_IN_COMBAT)
        })
        .unwrap_or(false)
}

/// Per-band 0..1 levels for the player-bar visualizer; call once per rendered
/// frame. Reads the newest [`EQ_WINDOW`] tapped mono samples, measures the
/// [`EQ_BANDS`] log-spaced bands (Goertzel — no FFT dependency), and folds the
/// result through attack/decay smoothing. With no tap installed, no new
/// samples since the last call, or too little audio yet, the target is
/// silence and the bars decay gracefully toward zero. Render-thread path
/// only; never touches STATE and never blocks the audio thread (the TAP lock
/// is only ever held briefly at session install, not on the sample path).
pub fn eq_levels() -> [f32; EQ_BANDS] {
    let tap = lock_or_recover(&TAP).clone();
    let mut eq = lock_or_recover(&EQ_STATE);
    let mut targets = [0.0_f32; EQ_BANDS];
    if let Some(tap) = tap {
        let cursor = tap.cursor.load(Ordering::Acquire);
        let rate = tap.sample_rate.load(Ordering::Relaxed);
        if cursor != eq.last_cursor && cursor >= EQ_WINDOW && rate > 0 {
            eq.last_cursor = cursor;
            let mut window = [0.0_f32; EQ_WINDOW];
            let start = cursor.wrapping_sub(EQ_WINDOW);
            for (i, slot) in window.iter_mut().enumerate() {
                let bits = tap.samples[start.wrapping_add(i) % TAP_LEN].load(Ordering::Relaxed);
                *slot = f32::from_bits(bits);
            }
            targets = analyze_bands(&window, rate as f32);
        }
    }
    for (level, target) in eq.levels.iter_mut().zip(targets) {
        *level = smooth_band(*level, target);
    }
    eq.levels
}

/// Unload teardown: stop sink -> join audio-owner thread -> drop reader ->
/// shut the tokio runtime down (bounded). Called from `on_unload` BEFORE
/// worker cancellation; must never block long or panic.
pub fn shutdown() {
    // Latch first: a frame that runs between this and the worker cancel in
    // `on_unload` must not start a session after the teardown below.
    SHUT_DOWN.store(true, Ordering::Release);
    // Invalidate the generation FIRST: if the bounded join below times out
    // while the audio thread sits between its stop check and the tap install,
    // the detached thread would otherwise repopulate TAP after this clear.
    GENERATION.fetch_add(1, Ordering::AcqRel);
    stop_session(SHUTDOWN_JOIN_BUDGET);
    *lock_or_recover(&TAP) = None;
    let runtime = lock_or_recover(&RUNTIME).take();
    if let Some(runtime) = runtime {
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_BUDGET);
    }
}

// Session lifecycle

/// Set by [`shutdown`]; every entry that would start or touch a session
/// becomes a no-op. Cleared by [`arm`] on load: a pinned image keeps its
/// statics across an unload/reload.
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

/// Allow playback again. Called from `on_load`.
pub fn arm() {
    SHUT_DOWN.store(false, Ordering::Release);
}

fn is_shut_down() -> bool {
    SHUT_DOWN.load(Ordering::Acquire)
}

fn start_session(station: RbStation) {
    if is_shut_down() {
        return;
    }
    let my_gen = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let mut guard = lock_or_recover(&SESSION);
    // Signal the old session but let the NEW audio thread join it: the caller
    // may be the render thread holding STATE, and the old thread may be
    // blocked on a status write into that very STATE.
    let old = guard.take();
    if let Some(old) = &old {
        old.stop.store(true, Ordering::Release);
    }
    let old_handle = old.and_then(|mut o| o.handle.take());

    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let sink: Arc<Mutex<Option<Arc<Player>>>> = Arc::new(Mutex::new(None));

    let t_stop = Arc::clone(&stop);
    let t_paused = Arc::clone(&paused);
    let t_sink = Arc::clone(&sink);
    // Pin the module before spawn returns (same contract as spawn_worker):
    // if this thread is ever detached - a stop/shutdown join timing out on a
    // socket read - FreeLibrary must not unmap `.text` under it.
    let pin = crate::state::pin_addon_module();
    let spawned = std::thread::Builder::new()
        .name("gw2bo-radio-audio".to_string())
        .spawn(move || {
            // The audio thread runs across an ABI-adjacent boundary (cpal,
            // symphonia); a panic must die here, not take the process down.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_session(station, my_gen, t_stop, t_paused, t_sink, old_handle);
            }));
            if outcome.is_err() {
                radio_log("radio audio thread panicked; session abandoned".to_string());
                let _ = crate::state::with_state(|s| {
                    if still_current(my_gen) {
                        s.radio.status = RadioStatus::Error("playback thread panicked".to_string());
                    }
                });
            }
            if let Some(handle) = pin {
                crate::state::exit_pinned_worker(handle);
            }
        });
    match spawned {
        Ok(handle) => {
            *guard = Some(Session {
                stop,
                paused,
                sink,
                handle: Some(handle),
            });
        }
        Err(e) => {
            crate::state::undo_module_pin(pin);
            radio_log(format!("failed to spawn radio audio thread: {e}"));
        }
    }
}

fn stop_session(join_budget: Duration) {
    // Take the session out under a short lock, join with the lock released.
    let session = lock_or_recover(&SESSION).take();
    if let Some(mut session) = session {
        session.stop.store(true, Ordering::Release);
        if let Some(handle) = session.handle.take() {
            join_bounded(handle, join_budget);
        }
    }
}

/// Everything a station session does, on the audio-owner thread.
fn run_session(
    station: RbStation,
    my_gen: u64,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    sink_cell: Arc<Mutex<Option<Arc<Player>>>>,
    old_handle: Option<std::thread::JoinHandle<()>>,
) {
    // Serialize sessions: the old thread writes its final status and releases
    // the audio device before this session opens its own.
    if let Some(handle) = old_handle {
        join_bounded(handle, STOP_JOIN_BUDGET);
    }
    if stop.load(Ordering::Acquire) {
        return; // replaced or stopped before we even started
    }

    // Connecting on entry; snapshot the now-playing cell and config volume.
    // The Arc is cloned out of STATE here, BEFORE the stream opens, so the
    // ICY callback never locks addon state.
    let Some(Some((now_playing, volume))) = crate::state::with_state(|s| {
        if !still_current(my_gen) {
            return None; // replaced before we even connected
        }
        s.radio.status = RadioStatus::Connecting;
        s.radio.current = Some(station.clone());
        if let Ok(mut np) = s.radio.now_playing.lock() {
            *np = None;
        }
        Some((
            Arc::clone(&s.radio.now_playing),
            s.config.radio.volume_percent,
        ))
    }) else {
        return; // addon is unloading, or a newer session owns the state
    };
    VOLUME_PERCENT.store(volume, Ordering::Release);

    let handle = match runtime_handle() {
        Ok(h) => h,
        Err(msg) => {
            set_error(my_gen, msg);
            return;
        }
    };

    // Open the output device. `log_on_drop(false)`: rodio would otherwise
    // write a drop notice to stderr, which is a black hole in-game. The
    // device-lost flag lives here, shared only with the error callback.
    let device_lost = Arc::new(AtomicBool::new(false));
    let lost = Arc::clone(&device_lost);
    let opened = DeviceSinkBuilder::from_default_device().and_then(|b| {
        b.with_error_callback(move |_err| {
            // cpal's callback thread: set a flag only, the watchdog translates
            // it to `DeviceLost`. Never lock addon state from here.
            lost.store(true, Ordering::Release);
        })
        .open_sink_or_fallback()
    });
    let mut device = match opened {
        Ok(d) => d,
        Err(e) => {
            set_error(my_gen, short_msg("no audio output device", &e.to_string()));
            return;
        }
    };
    device.log_on_drop(false);
    let player = Arc::new(Player::connect_new(device.mixer()));
    *lock_or_recover(&sink_cell) = Some(Arc::clone(&player));

    // One visualizer tap per session, shared by the initial tune-in and every
    // stall reconnect below. Installed under the TAP lock with a generation
    // check — a replaced session's install is discarded at the lock.
    let tap = TapBuffer::new();
    {
        let mut guard = lock_or_recover(&TAP);
        if still_current(my_gen) {
            *guard = Some(Arc::clone(&tap));
        }
    }

    // Connect, buffer, decode, append. Once headers land the UI flips from
    // Connecting to Buffering (gen-gated like every session status write).
    let on_headers = || {
        let _ = crate::state::with_state(|s| {
            if still_current(my_gen) {
                s.radio.status = RadioStatus::Buffering;
            }
        });
    };
    // Declared after `player`, so every early return drops (halts) the decode
    // thread before the player and device go away.
    let mut decoding = match open_and_append(
        &handle,
        &station,
        &player,
        &now_playing,
        &stop,
        &tap,
        &on_headers,
    ) {
        Ok(decoding) => Some(decoding),
        Err(SessionEnd::Cancelled) => {
            finish_stopped(my_gen, None, &player, &sink_cell);
            return;
        }
        Err(SessionEnd::Failed(msg)) => {
            *lock_or_recover(&sink_cell) = None;
            set_error(my_gen, msg);
            return;
        }
    };
    // Apply the stored volume through the same path the UI slider uses (the
    // published sink cell), then let it sound.
    set_volume(VOLUME_PERCENT.load(Ordering::Acquire));
    player.play();

    // Playing: publish the station and persist it for the keybind toggle.
    // The click ping (directory usage policy) rides the same lock: going
    // through spawn_worker gives it the module pin, registry tracking, and
    // cancel token that a raw fire-and-forget thread lacks.
    let published = crate::state::with_state(|s| {
        if !still_current(my_gen) {
            return false;
        }
        s.radio.status = RadioStatus::Playing;
        s.radio.current = Some(station.clone());
        s.config.radio.last_station = Some(saved_from_station(&station));
        crate::ui::save_config_detached(s);
        if !station.stationuuid.is_empty() {
            let uuid = station.stationuuid.clone();
            let _ = s.spawn_worker("radio-click", move |_token| {
                crate::radio::directory::click(&uuid);
            });
        }
        true
    })
    .unwrap_or(false);
    if !published {
        finish_stopped(my_gen, decoding.take(), &player, &sink_cell);
        return;
    }

    // Watchdog: a stall is the decoder running dry (stream ended, connection
    // dropped, decode error) AND the queued tail having played out. A queue
    // that merely drains while the decoder is alive is a slow network, which
    // the low-water log reports; it is not a stall.
    let mut last_reconnect: Option<Instant> = None;
    let mut resume_grace: u32 = 0;
    loop {
        let ticks = (WATCHDOG_INTERVAL.as_millis() / STOP_POLL_INTERVAL.as_millis()).max(1);
        for _ in 0..ticks {
            std::thread::sleep(STOP_POLL_INTERVAL);
            if stop.load(Ordering::Acquire) {
                finish_stopped(my_gen, decoding.take(), &player, &sink_cell);
                return;
            }
        }
        if device_lost.load(Ordering::Acquire) {
            // cpal does not recover on its own; the user presses play to
            // reopen. Leave the session; the next play() reaps this thread.
            *lock_or_recover(&sink_cell) = None;
            let _ = crate::state::with_state(|s| {
                if still_current(my_gen) {
                    s.radio.status = RadioStatus::DeviceLost;
                }
            });
            return;
        }
        if !stall_check_armed(paused.load(Ordering::Acquire), &mut resume_grace) {
            // Paused (or just resumed): a paused sink legitimately runs dry
            // and icecast may drop the idle socket — neither is a stall. A
            // stream that actually died surfaces right after the grace, and
            // the reconnect below re-tunes it.
            continue;
        }
        let decoder_ended = decoding.as_ref().is_none_or(Decoding::ended);
        if is_stall(decoder_ended, player.empty()) {
            // The decoder ran dry: the stream ended or the connection dropped.
            let live = crate::state::with_state(|s| {
                if !still_current(my_gen) {
                    return false;
                }
                s.radio.status = RadioStatus::Stalled;
                true
            })
            .unwrap_or(false);
            if !live {
                finish_stopped(my_gen, decoding.take(), &player, &sink_cell);
                return;
            }
            if !stall_wants_reconnect(last_reconnect.map(|t| t.elapsed())) {
                *lock_or_recover(&sink_cell) = None;
                set_error(my_gen, "stream keeps stalling".to_string());
                return;
            }
            // Reap the dry decode thread before the reconnect clears the player.
            drop(decoding.take());
            match open_and_append(
                &handle,
                &station,
                &player,
                &now_playing,
                &stop,
                &tap,
                &on_headers,
            ) {
                Ok(fresh) => {
                    decoding = Some(fresh);
                    // A pause can land between the stall check and this line;
                    // resume() owns sink.play() in that case.
                    if !paused.load(Ordering::Acquire) {
                        player.play();
                    }
                    last_reconnect = Some(Instant::now());
                    let live = crate::state::with_state(|s| {
                        if !still_current(my_gen) {
                            return false;
                        }
                        s.radio.status = RadioStatus::Playing;
                        true
                    })
                    .unwrap_or(false);
                    if !live {
                        finish_stopped(my_gen, decoding.take(), &player, &sink_cell);
                        return;
                    }
                }
                Err(SessionEnd::Cancelled) => {
                    finish_stopped(my_gen, None, &player, &sink_cell);
                    return;
                }
                Err(SessionEnd::Failed(msg)) => {
                    *lock_or_recover(&sink_cell) = None;
                    set_error(my_gen, msg);
                    return;
                }
            }
        }
    }
}

/// Why `open_and_append` (or the session) ended early.
enum SessionEnd {
    /// The stop flag was raised; exit quietly.
    Cancelled,
    /// Something broke; the message goes into `RadioStatus::Error`.
    Failed(String),
}

/// Connect to the station, buffer it, strip ICY metadata, and start the
/// decode thread feeding `player`; returns once [`QUEUE_PRIME`] is queued (or
/// the decoder ended, or [`PRIME_BUDGET`] passed). Used for both the initial
/// tune-in and the stall reconnect. `on_headers` fires once the server
/// answered (the buffering phase begins) — the session uses it for the
/// gen-gated Buffering status write.
fn open_and_append(
    handle: &tokio::runtime::Handle,
    station: &RbStation,
    player: &Arc<Player>,
    now_playing: &NowPlayingCell,
    stop: &Arc<AtomicBool>,
    tap: &Arc<TapBuffer>,
    on_headers: &dyn Fn(),
) -> Result<Decoding, SessionEnd> {
    // Some directory rows resolve to a dead `url_resolved` while the raw
    // `url` still answers (playlist unwrap gone stale, CDN hop down). One
    // quiet fallback to the other URL before surfacing the error.
    let primary = station.stream_url();
    let opened = match open_stream(handle, primary, now_playing, stop, on_headers) {
        Ok(opened) => opened,
        Err(SessionEnd::Cancelled) => return Err(SessionEnd::Cancelled),
        Err(SessionEnd::Failed(msg)) => {
            let alt = station.url.trim();
            if alt.is_empty() || alt == primary {
                return Err(SessionEnd::Failed(msg));
            }
            match open_stream(handle, alt, now_playing, stop, on_headers) {
                Ok(opened) => opened,
                Err(SessionEnd::Cancelled) => return Err(SessionEnd::Cancelled),
                // The primary URL's error is the station's real story.
                Err(SessionEnd::Failed(_)) => return Err(SessionEnd::Failed(msg)),
            }
        }
    };

    // A live stream has no filename, so the format probe is primed from the
    // Content-Type; `with_seekable(false)` keeps symphonia from issuing the
    // `Seek` that breaks on an infinite HTTP stream.
    let mut builder = Decoder::builder()
        .with_data(opened.reader)
        .with_seekable(false);
    if let Some(mime) = &opened.content_type {
        builder = builder.with_mime_type(mime);
    }
    let decoder = builder
        .build()
        .map_err(|e| SessionEnd::Failed(short_msg("cannot decode stream", &e.to_string())))?;

    // Only ever called with no live decode thread, so nothing appends while
    // the (normally already empty) queue is cleared.
    player.clear();
    // The tap rides between the decoder and the player: every decoded sample
    // is mirrored (mono-folded) into the visualizer ring on the decode thread.
    let decoding = Decoding::spawn(
        SampleTap::new(decoder, Arc::clone(tap)),
        Arc::clone(player),
        Arc::clone(stop),
        opened.cancel,
    )?;
    let deadline = Instant::now() + PRIME_BUDGET;
    while queued_audio(player.len()) < QUEUE_PRIME && !decoding.ended() {
        if stop.load(Ordering::Acquire) {
            return Err(SessionEnd::Cancelled); // dropping `decoding` halts it
        }
        if Instant::now() >= deadline {
            break; // a trickling stream: play what is queued
        }
        std::thread::sleep(DECODE_POLL);
    }
    Ok(decoding)
}

/// One connection's decode thread. Dropping it halts the thread, cancels the
/// download (waking a read blocked on the network) and waits for the thread
/// to finish, bounded by [`DECODE_JOIN_BUDGET`].
struct Decoding {
    halt: Arc<AtomicBool>,
    /// The decoder returned `None` (stream ended / dropped / undecodable) or
    /// the thread panicked: nothing more will be appended.
    ended: Arc<AtomicBool>,
    /// Set as the thread's last act. Pinned threads leave through
    /// `FreeLibraryAndExitThread`, so `JoinHandle::is_finished` never turns
    /// true for them; this flag is the join.
    done: Arc<AtomicBool>,
    cancel: Box<dyn Fn() + Send + Sync>,
}

impl Decoding {
    fn spawn<I: Source + Send + 'static>(
        source: I,
        player: Arc<Player>,
        stop: Arc<AtomicBool>,
        cancel: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self, SessionEnd> {
        let halt = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (t_halt, t_ended, t_done) = (Arc::clone(&halt), Arc::clone(&ended), Arc::clone(&done));
        // Same pin contract as the audio-owner thread: a decode thread that
        // outlives its join budget must not run on unmapped `.text`.
        let pin = crate::state::pin_addon_module();
        let spawned = std::thread::Builder::new()
            .name("gw2bo-radio-decode".to_string())
            .spawn(move || {
                // Everything captured moves into this closure and drops when
                // it returns: the decoder (cancelling its download) and the
                // player handle must not leak past FreeLibraryAndExitThread.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    decode_loop(source, &player, &stop, &t_halt)
                }));
                let ran_dry = outcome.unwrap_or_else(|_| {
                    radio_log("radio decode thread panicked; stream ends".to_string());
                    true
                });
                if ran_dry {
                    t_ended.store(true, Ordering::Release);
                }
                t_done.store(true, Ordering::Release);
                if let Some(handle) = pin {
                    crate::state::exit_pinned_worker(handle);
                }
            });
        match spawned {
            Ok(_detached) => Ok(Self {
                halt,
                ended,
                done,
                cancel,
            }),
            Err(e) => {
                crate::state::undo_module_pin(pin);
                cancel();
                Err(SessionEnd::Failed(short_msg(
                    "decode thread",
                    &e.to_string(),
                )))
            }
        }
    }

    fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

impl Drop for Decoding {
    fn drop(&mut self) {
        self.halt.store(true, Ordering::Release);
        (self.cancel)();
        let deadline = Instant::now() + DECODE_JOIN_BUDGET;
        while !self.done.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                radio_log("radio decode thread outlived its join budget; detaching".to_string());
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The decode thread body: keep ~[`QUEUE_TARGET`] of decoded chunks queued on
/// `player` until halted, stopped, or the source runs dry. Returns true when
/// the source ran dry. Blocks only inside the source's own reads (woken by a
/// download cancel) or in [`DECODE_POLL`] sleeps — never spins.
fn decode_loop<I: Source>(
    mut source: I,
    player: &Player,
    stop: &AtomicBool,
    halt: &AtomicBool,
) -> bool {
    let mut carry = None;
    let mut low_water = LowWater::new();
    loop {
        if stop.load(Ordering::Acquire) || halt.load(Ordering::Acquire) {
            return false;
        }
        let queued = player.len();
        low_water.observe(queued_audio(queued));
        if let Some(line) = low_water.report(Instant::now()) {
            radio_log(line);
        }
        if !decode_wanted(queued) {
            std::thread::sleep(DECODE_POLL);
            continue;
        }
        match pull_chunk(&mut source, &mut carry) {
            Some(chunk) => player.append(chunk),
            None => return true,
        }
    }
}

/// A sample that opened a new span (new channel count or rate), carried into
/// the next chunk.
type Carry = Option<(Sample, ChannelCount, SampleRate)>;

/// Pull up to one [`CHUNK`] of interleaved samples from `source`, all at one
/// channel count and rate. Spans are frame-aligned, so a format change is
/// only checked at frame boundaries; the sample that reveals it is parked in
/// `carry` and opens the next chunk. `None` once the source is exhausted.
fn pull_chunk<I: Source>(source: &mut I, carry: &mut Carry) -> Option<SamplesBuffer> {
    let (first, channels, rate) = match carry.take() {
        Some(parked) => parked,
        None => {
            let sample = source.next()?;
            (sample, source.channels(), source.sample_rate())
        }
    };
    let chans = usize::from(channels.get());
    let want = chunk_frames(rate) * chans;
    let mut samples = Vec::with_capacity(want);
    samples.push(first);
    while samples.len() < want {
        let Some(sample) = source.next() else { break };
        if samples.len() % chans == 0 {
            let (c, r) = (source.channels(), source.sample_rate());
            if c != channels || r != rate {
                *carry = Some((sample, c, r));
                break;
            }
        }
        samples.push(sample);
    }
    Some(SamplesBuffer::new(channels, rate, samples))
}

/// Frames in one [`CHUNK`] at `rate` (at least one).
fn chunk_frames(rate: SampleRate) -> usize {
    let frames = u128::from(rate.get()) * CHUNK.as_millis() / 1000;
    usize::try_from(frames).unwrap_or(usize::MAX).max(1)
}

/// Audio held by `chunks` queued chunks. Counts the playing head chunk in
/// full, so it overstates the true depth by at most one [`CHUNK`].
fn queued_audio(chunks: usize) -> Duration {
    CHUNK.saturating_mul(u32::try_from(chunks).unwrap_or(u32::MAX))
}

/// Whether the decode thread should decode another chunk now.
fn decode_wanted(queued_chunks: usize) -> bool {
    queued_audio(queued_chunks) < QUEUE_TARGET
}

/// Whether the watchdog sees a stall: the decoder ran dry AND its queued tail
/// has played out. A draining queue with a live decoder is not a stall.
fn is_stall(decoder_ended: bool, queue_empty: bool) -> bool {
    decoder_ended && queue_empty
}

/// Per-connection queue low-water accounting. Arms once the queue first
/// reaches [`LOW_WATER`] (the initial fill is not a dip); from then on a dip
/// is each fall below [`LOW_WATER`], and `min` is the shallowest depth seen.
struct LowWater {
    armed: bool,
    below: bool,
    min: Duration,
    dips: u32,
    reported_dips: u32,
    last_log: Option<Instant>,
}

impl LowWater {
    fn new() -> Self {
        Self {
            armed: false,
            below: false,
            min: Duration::MAX,
            dips: 0,
            reported_dips: 0,
            last_log: None,
        }
    }

    fn observe(&mut self, queued: Duration) {
        if !self.armed {
            if queued < LOW_WATER {
                return;
            }
            self.armed = true;
        }
        self.min = self.min.min(queued);
        let below = queued < LOW_WATER;
        if below && !self.below {
            self.dips += 1;
        }
        self.below = below;
    }

    /// The log line owed, if a new dip happened and the last line is at
    /// least [`LOW_WATER_LOG_INTERVAL`] old.
    fn report(&mut self, now: Instant) -> Option<String> {
        if self.dips == self.reported_dips
            || self
                .last_log
                .is_some_and(|t| now.duration_since(t) < LOW_WATER_LOG_INTERVAL)
        {
            return None;
        }
        self.reported_dips = self.dips;
        self.last_log = Some(now);
        Some(format!(
            "radio: audio queue low-water {:.1} s, {} dips",
            self.min.as_secs_f32(),
            self.dips
        ))
    }
}

/// The connected, buffered, ICY-stripped stream — everything `open_stream`
/// produces before any decoder or audio device is involved, so the network
/// path stays testable without a sink.
struct OpenedStream {
    reader: Box<dyn StreamReader>,
    content_type: Option<String>,
    /// Cancels the download from another thread, waking a blocked read.
    cancel: Box<dyn Fn() + Send + Sync>,
}

/// Connect to the stream URL, buffer it, and wrap ICY metadata handling.
/// `on_headers` fires between the header handshake and the prefetch wait.
fn open_stream(
    handle: &tokio::runtime::Handle,
    url: &str,
    now_playing: &NowPlayingCell,
    stop: &Arc<AtomicBool>,
    on_headers: &dyn Fn(),
) -> Result<OpenedStream, SessionEnd> {
    // Enter the runtime context for this whole construction sequence:
    // `tokio::time::timeout` (and other tokio primitives) bind the timer
    // driver at CONSTRUCTION, and these futures are built on the plain audio
    // thread before `block_on` runs — without this guard that construction
    // panics with "there is no reactor running" (shipped crash in 1.8.0).
    let _reactor = handle.enter();

    let parsed: reqwest::Url = url
        .parse()
        .map_err(|_| SessionEnd::Failed("invalid stream URL".to_string()))?;

    // The station URL is community-submitted directory data: refuse to dial
    // into the local network. Hostnames are resolved once here; DNS rebinding
    // after the check is accepted residual risk (the attacker controls
    // timing, not this addon).
    if stream_host_reserved(&parsed) {
        return Err(SessionEnd::Failed(
            "station points at a private address".to_string(),
        ));
    }

    let mut headers = reqwest::header::HeaderMap::new();
    // Ask the server to interleave ICY metadata into the stream.
    icy_metadata::add_icy_metadata_header(&mut headers);
    let client = reqwest::Client::builder()
        // radio-browser.info asks clients to identify themselves; icecast
        // servers occasionally reject UA-less requests too.
        .user_agent(concat!("GW2BuildOptimizer/", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        // Bound the connect phase only. Deliberately NO blanket `.timeout()`:
        // the audio body is infinite, and a request timeout would kill
        // playback mid-song.
        .connect_timeout(CONNECT_TIMEOUT)
        // Icecast mounts redirect occasionally (http->https, CDN hops); ten
        // hops (reqwest's default) is a tunnel, three is a stream. Each hop
        // is re-screened so a CDN 302 cannot bounce us into the LAN (SEC-RADIO).
        .redirect(stream_redirect_policy())
        .build()
        .map_err(|e| SessionEnd::Failed(short_msg("http client", &e.to_string())))?;

    // `HttpStream::new` connects and waits for response headers; a station
    // that accepts the connection then withholds headers would hang forever,
    // so the header-wait is capped. The select keeps stop() prompt even
    // mid-handshake — dropping the future cancels the connect.
    let stream = block_on_cancellable(
        handle,
        stop,
        tokio::time::timeout(HEADER_TIMEOUT, HttpStream::new(client, parsed)),
    )
    .ok_or(SessionEnd::Cancelled)?
    .map_err(|_| SessionEnd::Failed("timed out waiting for stream headers".to_string()))?
    .map_err(|e| SessionEnd::Failed(short_msg("connect failed", &e.to_string())))?;

    let icy_headers = IcyHeaders::parse_from_headers(stream.headers());
    let content_type = stream
        .content_type()
        .as_ref()
        .map(|ct| format!("{}/{}", ct.r#type, ct.subtype));

    // Headers are in — the wait below is the prefetch filling.
    on_headers();

    let settings = Settings::default().prefetch_bytes(PREFETCH_BYTES);
    let storage = BoundedStorageProvider::new(
        MemoryStorageProvider,
        NonZeroUsize::new(RING_BUFFER_BYTES).expect("ring buffer size is non-zero"),
    );
    let download = block_on_cancellable(
        handle,
        stop,
        StreamDownload::from_stream(stream, storage, settings),
    )
    .ok_or(SessionEnd::Cancelled)?
    .map_err(|e| SessionEnd::Failed(short_msg("buffering failed", &e.to_string())))?;
    let token = download.cancellation_token();
    let cancel: Box<dyn Fn() + Send + Sync> = Box::new(move || token.cancel());

    // Wrap in the ICY reader only when the server actually interleaves
    // metadata; the callback writes into the shared cell, never into STATE.
    let reader: Box<dyn StreamReader> = match icy_headers.metadata_interval() {
        Some(metaint) => {
            let cell = Arc::clone(now_playing);
            Box::new(IcyMetadataReader::new(
                download,
                Some(metaint),
                move |meta| {
                    // Parse errors on one metadata block are transient — keep the
                    // last good title rather than blanking the display.
                    if let Ok(meta) = meta {
                        if let Some(raw) = meta.stream_title() {
                            let title = sanitize_title(raw);
                            if let Ok(mut np) = cell.lock() {
                                *np = if title.is_empty() { None } else { Some(title) };
                            }
                        }
                    }
                },
            ))
        }
        // No `icy-metaint`: a plain audio stream, pass it through untouched.
        None => Box::new(download),
    };

    Ok(OpenedStream {
        reader,
        content_type,
        cancel,
    })
}

/// True when the stream URL's host is (or resolves to) an address inside the
/// local network - loopback, RFC1918, link-local, ULA, unspecified. Literal
/// IPs are checked directly; hostnames get one blocking resolve (the OS
/// caches it for the connect that follows). An unresolvable host returns
/// false: the connect will fail with its own honest error.
fn stream_host_reserved(url: &reqwest::Url) -> bool {
    crate::news_art::url_host_is_reserved(url)
}

/// Same hop screen as radio logos, with a 3-hop cap for Icecast CDN bounces.
fn stream_redirect_policy() -> reqwest::redirect::Policy {
    crate::news_art::screened_redirect_policy(3, super::logos::hop_ok)
}

/// Stop-flag exit path: halt the decode thread (which cancels its download),
/// clear the player, drop the sink handle and write `Stopped`. The decode
/// thread goes first so nothing appends while the player is cleared.
fn finish_stopped(
    my_gen: u64,
    decoding: Option<Decoding>,
    player: &Player,
    sink_cell: &Arc<Mutex<Option<Arc<Player>>>>,
) {
    drop(decoding);
    player.clear();
    *lock_or_recover(sink_cell) = None;
    let _ = crate::state::with_state(|s| {
        if still_current(my_gen) {
            s.radio.status = RadioStatus::Stopped;
        }
    });
}

fn set_error(my_gen: u64, msg: String) {
    radio_log(format!("radio: {msg}"));
    let _ = crate::state::with_state(|s| {
        if still_current(my_gen) {
            s.radio.status = RadioStatus::Error(msg);
        }
    });
}

/// Lazily create the playback runtime and hand out a handle to it.
fn runtime_handle() -> Result<tokio::runtime::Handle, String> {
    let mut guard = lock_or_recover(&RUNTIME);
    if guard.is_none() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("gw2bo-radio-rt")
            .enable_all()
            .build()
            .map_err(|e| short_msg("audio runtime failed", &e.to_string()))?;
        *guard = Some(runtime);
    }
    Ok(guard
        .as_ref()
        .expect("runtime just created")
        .handle()
        .clone())
}

/// `block_on` a future on the playback runtime, racing it against the session
/// stop flag so [`stop`] interrupts even a mid-handshake session promptly.
/// `None` = cancelled (the future is dropped, aborting the work).
fn block_on_cancellable<F: std::future::Future>(
    handle: &tokio::runtime::Handle,
    stop: &Arc<AtomicBool>,
    fut: F,
) -> Option<F::Output> {
    let stop = Arc::clone(stop);
    handle.block_on(async move {
        tokio::select! {
            out = fut => Some(out),
            () = async {
                while !stop.load(Ordering::Acquire) {
                    tokio::time::sleep(STOP_POLL_INTERVAL).await;
                }
            } => None,
        }
    })
}

/// Poll-join a thread with a budget. On timeout the handle is dropped (the
/// thread finishes on its own shortly after) and the abandonment is logged —
/// a bounded hitch, never a deadlock.
fn join_bounded(handle: std::thread::JoinHandle<()>, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            radio_log("radio audio thread outlived its join budget; detaching".to_string());
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join(); // finished: reap; panics were caught inside
}

/// Recover a poisoned module mutex the same way `state::lock_state` does:
/// the data is flags and handles, all valid regardless of where a panic hit.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Diagnostics through nexus, mirroring `state::worker_log` (which is private
/// there): tests have no Nexus API table, so test builds go to stderr.
fn radio_log(message: String) {
    #[cfg(test)]
    eprintln!("[GW2BuildOpt] {}", message);
    #[cfg(not(test))]
    nexus::log::log(
        nexus::log::LogLevel::Warning,
        "GW2 Build Optimizer",
        message,
    );
}

// Volume/gain helpers (unit-tested)

/// The gain for a volume percent: a log taper over a 60 dB range
/// (`1000^(p/100 - 1)`). 0% is silence, 100% is stream level, 80% ~ -12 dB.
fn volume_gain(percent: u8) -> f32 {
    const DB_RATIO: f64 = 1000.0;
    match percent {
        0 => 0.0,
        p if p >= 100 => 1.0,
        p => ((f64::from(p) / 100.0 - 1.0) * DB_RATIO.ln()).exp() as f32,
    }
}

/// Log-spaced band center frequency in Hz: band 0 at [`EQ_FREQ_MIN`], the
/// last band at [`EQ_FREQ_MAX`].
fn band_center(band: usize) -> f32 {
    let t = band as f32 / (EQ_BANDS - 1) as f32;
    EQ_FREQ_MIN * (EQ_FREQ_MAX / EQ_FREQ_MIN).powf(t)
}

/// One analysis window -> perceptual 0..1 levels per band. Non-finite input
/// samples are zeroed and every output is finite and clamped, so a torn ring
/// read can never paint NaN bars.
fn analyze_bands(samples: &[f32], sample_rate: f32) -> [f32; EQ_BANDS] {
    let mut out = [0.0_f32; EQ_BANDS];
    let n = samples.len().min(EQ_WINDOW);
    if n < 2 || sample_rate <= 0.0 {
        return out;
    }
    // Hann window against spectral leakage; NaN/inf samples are dropped here.
    let mut windowed = [0.0_f32; EQ_WINDOW];
    for (i, slot) in windowed[..n].iter_mut().enumerate() {
        let s = samples[i];
        let s = if s.is_finite() { s } else { 0.0 };
        let phase = std::f32::consts::TAU * i as f32 / (n - 1) as f32;
        *slot = s * (0.5 - 0.5 * phase.cos());
    }
    for (band, level) in out.iter_mut().enumerate() {
        let freq = band_center(band);
        // Bands at/above Nyquist do not exist in this stream; leave them 0.
        if freq >= sample_rate * 0.45 {
            continue;
        }
        let magnitude = goertzel_magnitude(&windowed[..n], sample_rate, freq);
        *level = perceptual_level(magnitude, n);
    }
    out
}

/// Magnitude of the DTFT at `freq` over `samples` via the Goertzel recurrence
/// (the closing power formula is valid off bin centers too, which log-spaced
/// bands need). f64 state: the recurrence is marginally stable and f32 error
/// grows with window length.
fn goertzel_magnitude(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
    let omega = std::f64::consts::TAU * f64::from(freq) / f64::from(sample_rate);
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0_f64, 0.0_f64);
    for &x in samples {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power.max(0.0).sqrt() as f32
}

/// Windowed magnitude -> perceptual 0..1 level: a full-scale sine at the band
/// center is 1.0, [`EQ_RANGE_DB`] below full scale is 0. The `4/n` undoes the
/// DTFT's `n/2` sine gain and the Hann window's 0.5 coherent gain.
fn perceptual_level(magnitude: f32, window_len: usize) -> f32 {
    if window_len == 0 {
        return 0.0;
    }
    let amplitude = 4.0 * magnitude / window_len as f32;
    if !amplitude.is_finite() || amplitude <= 0.0 {
        return 0.0;
    }
    ((20.0 * amplitude.log10() + EQ_RANGE_DB) / EQ_RANGE_DB).clamp(0.0, 1.0)
}

/// One smoothing step toward `target`: rise at [`EQ_ATTACK`], fall at
/// [`EQ_DECAY`] per frame. Both inputs are sanitized, so a NaN can neither
/// enter nor persist in the smoothed state.
fn smooth_band(prev: f32, target: f32) -> f32 {
    let prev = if prev.is_finite() {
        prev.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let target = if target.is_finite() {
        target.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let rate = if target > prev { EQ_ATTACK } else { EQ_DECAY };
    prev + (target - prev) * rate
}

/// Whether a stall has earned its ONE quiet reconnect attempt. The first
/// stall always has; after a reconnect the stream must have played for
/// [`STALL_REARM`] before another stall earns a new attempt.
fn stall_wants_reconnect(since_last_reconnect: Option<Duration>) -> bool {
    since_last_reconnect.is_none_or(|d| d >= STALL_REARM)
}

/// Whether the watchdog's empty-sink stall check may run this tick. While
/// paused the check is suspended and the grace counter stays fully armed;
/// after a resume the counter counts [`RESUME_GRACE_TICKS`] ticks down before
/// the check fires again. Pure: the whole pause/resume watchdog policy in one
/// testable place.
fn stall_check_armed(paused: bool, grace: &mut u32) -> bool {
    if paused {
        *grace = RESUME_GRACE_TICKS;
        return false;
    }
    if *grace > 0 {
        *grace -= 1;
        return false;
    }
    true
}

/// One exponential duck-ramp step toward `target` (either [`DUCK_FLOOR`] or
/// 1.0), snapping when within [`DUCK_SNAP`] so a settled ramp goes quiescent.
/// Both inputs sanitized: a NaN can neither enter nor persist in the factor.
fn duck_step(current: f32, target: f32) -> f32 {
    let current = if current.is_finite() {
        current.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let target = if target.is_finite() {
        target.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let next = current + (target - current) * DUCK_STEP;
    if (next - target).abs() <= DUCK_SNAP {
        target
    } else {
        next
    }
}

/// UI-safe stream title: control characters stripped, capped at
/// [`MAX_TITLE_CHARS`] via `chars()` (never byte slicing), trimmed.
fn sanitize_title(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Status/log message with the detail capped UTF-8-safely.
fn short_msg(prefix: &str, detail: &str) -> String {
    let detail: String = detail.chars().take(MAX_TITLE_CHARS).collect();
    format!("{prefix}: {detail}")
}

/// Snapshot a directory row for persistence (`config.radio.last_station` /
/// favorites). `url` is the resolved stream URL so re-tuning never needs a
/// directory round-trip.
pub fn saved_from_station(station: &RbStation) -> SavedStation {
    SavedStation {
        stationuuid: station.stationuuid.clone(),
        name: station.name.clone(),
        url: station.stream_url().to_string(),
        favicon: station.favicon.clone(),
        codec: station.codec.clone(),
        bitrate: station.bitrate,
        countrycode: station.countrycode.clone(),
        tags: station.tags.clone(),
    }
}

/// Rehydrate a saved snapshot into a playable station row. The snapshot's
/// `url` was already resolved at save time, so it fills both URL fields; the
/// health flags are set playable — the save itself is the health evidence.
pub fn station_from_saved(saved: &SavedStation) -> RbStation {
    RbStation {
        stationuuid: saved.stationuuid.clone(),
        name: saved.name.clone(),
        url: saved.url.clone(),
        url_resolved: saved.url.clone(),
        favicon: saved.favicon.clone(),
        // A rehydrated favorite has no directory vote count.
        votes: 0,
        tags: saved.tags.clone(),
        countrycode: saved.countrycode.clone(),
        codec: saved.codec.clone(),
        bitrate: saved.bitrate,
        lastcheckok: 1,
        hls: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_plays_after_shutdown_until_rearmed() {
        shutdown();
        play(&RbStation {
            name: "late".into(),
            url: "http://example.invalid/stream".into(),
            ..RbStation::default()
        });
        assert!(
            lock_or_recover(&SESSION).is_none(),
            "a play after shutdown must not start a session"
        );
        assert!(flag_paused(true).is_none());
        arm();
        assert!(!is_shut_down());
    }

    #[test]
    fn stream_guard_rejects_reserved_ipv6_literals() {
        for host in [
            "[::1]",
            "[::]",
            "[fe80::1]",
            "[fd00::1]",
            "[::ffff:127.0.0.1]",
        ] {
            let url = reqwest::Url::parse(&format!("http://{host}:8000/stream")).unwrap();
            assert!(
                stream_host_reserved(&url),
                "reserved literal admitted: {host}"
            );
        }
        let public = reqwest::Url::parse("https://[2606:4700:4700::1111]/stream").unwrap();
        assert!(!stream_host_reserved(&public));
    }

    #[test]
    fn redirect_to_reserved_is_stopped_before_connect() {
        crate::news_art::assert_policy_stops_reserved_redirects(stream_redirect_policy());
    }

    /// Regression for the v1.8.0 in-game crash: `tokio::time::timeout` grabs
    /// the timer driver at CONSTRUCTION, and `open_stream` builds it on the
    /// plain audio thread before `block_on` ever runs — panicking with
    /// "there is no reactor running". The `.invalid` TLD never resolves
    /// (RFC 2606), so the call must reach the tokio construction site and
    /// then fail the connect honestly — no sockets to real stream hosts.
    #[test]
    fn open_stream_off_runtime_thread_fails_cleanly_instead_of_panicking() {
        let handle = runtime_handle().expect("test runtime");
        let stop = Arc::new(AtomicBool::new(false));
        let np: NowPlayingCell = Arc::new(Mutex::new(None));
        let joined = std::thread::spawn(move || {
            open_stream(
                &handle,
                "http://gw2bo-no-such-host.invalid/stream",
                &np,
                &stop,
                &|| {},
            )
            .err()
        })
        .join();
        let ended = joined.expect("open_stream must not panic off-runtime");
        assert!(
            matches!(ended, Some(SessionEnd::Failed(_))),
            "expected a clean connect failure"
        );
    }

    #[test]
    fn volume_gain_follows_the_log_curve() {
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(150), 1.0);
        let at_80 = volume_gain(80);
        assert!(
            (at_80 - 0.251).abs() < 0.001,
            "80% is about -12 dB: {at_80}"
        );
        let at_50 = volume_gain(50);
        assert!(
            (at_50 - 0.0316).abs() < 0.001,
            "50% is about -30 dB: {at_50}"
        );
        assert!(
            volume_gain(20) < at_50 && at_50 < at_80,
            "taper is monotonic"
        );
    }

    #[test]
    fn keybind_toggle_pauses_playing_resumes_paused_and_stops_tuning() {
        assert_eq!(
            toggle_action(&RadioStatus::Playing, true),
            ToggleAction::Pause
        );
        assert_eq!(
            toggle_action(&RadioStatus::Playing, false),
            ToggleAction::Pause
        );
        assert_eq!(
            toggle_action(&RadioStatus::Paused, true),
            ToggleAction::Resume
        );
        assert_eq!(
            toggle_action(&RadioStatus::Connecting, true),
            ToggleAction::Stop
        );
        assert_eq!(
            toggle_action(&RadioStatus::Stalled, true),
            ToggleAction::Stop
        );
        // Every "off" state re-tunes when there is a station to tune to.
        for status in [
            RadioStatus::Idle,
            RadioStatus::Stopped,
            RadioStatus::DeviceLost,
            RadioStatus::Error("x".to_string()),
        ] {
            assert_eq!(toggle_action(&status, true), ToggleAction::Tune);
            assert_eq!(toggle_action(&status, false), ToggleAction::Nothing);
        }
    }

    #[test]
    fn watchdog_stall_check_suspends_while_paused_and_rearms_after_grace() {
        let mut grace = 0;
        assert!(stall_check_armed(false, &mut grace), "normal play: armed");
        // Paused: suspended, however long the pause lasts.
        for _ in 0..10 {
            assert!(!stall_check_armed(true, &mut grace));
        }
        // Resumed: the grace ticks pass before the check re-arms, so a
        // buffer that has not started draining again cannot read as a stall.
        for tick in 0..RESUME_GRACE_TICKS {
            assert!(!stall_check_armed(false, &mut grace), "grace tick {tick}");
        }
        assert!(stall_check_armed(false, &mut grace), "grace over: armed");
        assert!(stall_check_armed(false, &mut grace), "and stays armed");
    }

    #[test]
    fn duck_ramp_settles_at_both_ends_within_the_design_window() {
        // Down: 1.0 -> DUCK_FLOOR, monotone, settling EXACTLY at the floor
        // in ~22 frames (~350 ms at 60 fps, inside the 250-400 ms window).
        let mut f = 1.0_f32;
        let mut steps = 0;
        while f != DUCK_FLOOR {
            let next = duck_step(f, DUCK_FLOOR);
            assert!(next < f, "duck ramp is monotone down: {next} !< {f}");
            f = next;
            steps += 1;
            assert!(steps < 60, "ramp must settle");
        }
        assert!(
            (15..=30).contains(&steps),
            "settled in {steps} frames (~{} ms at 60 fps)",
            steps * 1000 / 60
        );

        // Up: DUCK_FLOOR -> 1.0, monotone, settling exactly at unity.
        let mut steps = 0;
        while f != 1.0 {
            let next = duck_step(f, 1.0);
            assert!(next > f, "recover ramp is monotone up");
            f = next;
            steps += 1;
            assert!(steps < 60, "ramp must settle");
        }
        assert!((15..=30).contains(&steps));

        // Settled endpoints are fixed points — a quiescent duck costs nothing.
        assert_eq!(duck_step(1.0, 1.0), 1.0);
        assert_eq!(duck_step(DUCK_FLOOR, DUCK_FLOOR), DUCK_FLOOR);

        // NaN can neither enter nor persist.
        assert!(duck_step(f32::NAN, DUCK_FLOOR).is_finite());
        assert!(duck_step(0.5, f32::NAN).is_finite());
    }

    #[test]
    fn stall_policy_grants_the_first_reconnect_and_rearms_after_stable_play() {
        // The very first stall of a session always gets its one attempt.
        assert!(stall_wants_reconnect(None));
        // A stall right after a reconnect means the stream is bad — give up.
        assert!(!stall_wants_reconnect(Some(Duration::from_secs(3))));
        // After a stretch of stable playback the attempt is re-armed.
        assert!(stall_wants_reconnect(Some(STALL_REARM)));
        assert!(stall_wants_reconnect(Some(Duration::from_secs(3600))));
    }

    #[test]
    fn sanitize_title_strips_controls_and_caps_by_chars() {
        assert_eq!(
            sanitize_title("Artist \u{0} - \u{7} Title\r\n"),
            "Artist  -  Title"
        );
        assert_eq!(sanitize_title("   padded   "), "padded");
        assert_eq!(sanitize_title(""), "");
        // Multibyte input truncates on char boundaries, never mid-codepoint.
        let long: String = "ドイツ語".chars().cycle().take(500).collect();
        let cleaned = sanitize_title(&long);
        assert_eq!(cleaned.chars().count(), MAX_TITLE_CHARS);
        assert!(cleaned.starts_with('ド'));
    }

    #[test]
    fn station_round_trips_through_the_saved_snapshot() {
        let station = RbStation {
            votes: 0,
            stationuuid: "uuid-1".to_string(),
            name: "Groove Salad".to_string(),
            url: "https://example.com/playlist.pls".to_string(),
            url_resolved: "https://example.com/stream".to_string(),
            favicon: "https://example.com/icon.png".to_string(),
            tags: "ambient,chill".to_string(),
            countrycode: "US".to_string(),
            codec: "MP3".to_string(),
            bitrate: 128,
            lastcheckok: 1,
            hls: 0,
        };
        let saved = saved_from_station(&station);
        // The snapshot captures the RESOLVED url — the playable one.
        assert_eq!(saved.url, "https://example.com/stream");
        assert_eq!(saved.name, "Groove Salad");
        assert_eq!(saved.bitrate, 128);

        let back = station_from_saved(&saved);
        assert_eq!(back.stream_url(), "https://example.com/stream");
        assert_eq!(back.stationuuid, "uuid-1");
        assert_eq!(back.name, station.name);
        assert_eq!(back.codec, station.codec);
        assert_eq!(back.countrycode, station.countrycode);
        assert_eq!(back.tags, station.tags);
        assert_eq!(back.lastcheckok, 1, "a saved station is presumed playable");
        assert_eq!(back.hls, 0);
    }

    #[test]
    fn a_sine_at_a_band_center_peaks_in_that_band_and_not_its_neighbors() {
        // Band centers are log-spaced across the declared range.
        assert!((band_center(0) - EQ_FREQ_MIN).abs() < 1e-2);
        assert!((band_center(EQ_BANDS - 1) - EQ_FREQ_MAX).abs() < 1.0);
        for b in 1..EQ_BANDS {
            assert!(band_center(b) > band_center(b - 1), "centers are monotone");
        }

        // A high band, where log-spaced neighbors sit far outside the Hann
        // main lobe (low bands are closer together than the FFT resolution).
        let band = 20;
        let rate = 48_000.0_f32;
        let freq = band_center(band);
        let window: Vec<f32> = (0..EQ_WINDOW)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate).sin())
            .collect();
        let levels = analyze_bands(&window, rate);
        assert!(
            levels[band] > 0.9,
            "a full-scale sine at the center is near 1.0: {}",
            levels[band]
        );
        assert!(
            levels[band - 1] < levels[band] - 0.3 && levels[band + 1] < levels[band] - 0.3,
            "neighbors stay well below the peak: {} / {} / {}",
            levels[band - 1],
            levels[band],
            levels[band + 1]
        );
        assert!(
            levels[0] < 0.2 && levels[EQ_BANDS - 1] < 0.2,
            "far bands stay quiet"
        );
    }

    #[test]
    fn silence_analyzes_to_zero_and_smoothing_decays_toward_zero() {
        let silence = [0.0_f32; EQ_WINDOW];
        assert_eq!(analyze_bands(&silence, 48_000.0), [0.0; EQ_BANDS]);

        // A loud bar with a silent target sinks monotonically toward zero.
        let mut level = 1.0_f32;
        for _ in 0..120 {
            let next = smooth_band(level, 0.0);
            assert!(next < level && next >= 0.0);
            level = next;
        }
        assert!(level < 0.01, "decayed after ~2s of frames: {level}");

        // And the attack path rises much faster than the decay path falls.
        assert!(smooth_band(0.0, 1.0) > 1.0 - smooth_band(1.0, 0.0));
    }

    #[test]
    fn nan_and_infinite_input_cannot_produce_nan_levels() {
        let mut window = [0.25_f32; EQ_WINDOW];
        window[10] = f32::NAN;
        window[11] = f32::INFINITY;
        window[12] = f32::NEG_INFINITY;
        for level in analyze_bands(&window, 48_000.0) {
            assert!(level.is_finite() && (0.0..=1.0).contains(&level));
        }
        // The smoother sanitizes both of its inputs too.
        assert!(smooth_band(f32::NAN, f32::NAN).is_finite());
        assert!(smooth_band(f32::NAN, 0.5).is_finite());
        assert!(smooth_band(0.5, f32::INFINITY).is_finite());
        assert_eq!(perceptual_level(f32::NAN, EQ_WINDOW), 0.0);
        assert_eq!(perceptual_level(1.0, 0), 0.0);
    }

    #[test]
    fn tap_ring_keeps_the_newest_samples_and_counts_them() {
        let tap = TapBuffer::new();
        let total = TAP_LEN + 100;
        for i in 0..total {
            tap.push(i as f32);
        }
        let cursor = tap.cursor.load(Ordering::Acquire);
        assert_eq!(cursor, total);
        // The newest EQ_WINDOW samples read back exactly, oldest overwritten.
        let start = cursor - EQ_WINDOW;
        for i in 0..EQ_WINDOW {
            let bits = tap.samples[(start + i) % TAP_LEN].load(Ordering::Relaxed);
            assert_eq!(f32::from_bits(bits), (start + i) as f32);
        }
    }

    #[test]
    fn sample_tap_passes_audio_through_unchanged_and_folds_stereo_to_mono() {
        // Exactly representable values so the mono averages compare with ==.
        let data = vec![0.25_f32, 0.75, -0.5, 0.5, 1.0, 0.0];
        let buffer = rodio::buffer::SamplesBuffer::new(
            ChannelCount::new(2).expect("stereo"),
            SampleRate::new(44_100).expect("rate"),
            data.clone(),
        );
        let tap = TapBuffer::new();
        let out: Vec<f32> = SampleTap::new(buffer, Arc::clone(&tap)).collect();
        assert_eq!(out, data, "the tap is a pure pass-through");
        assert_eq!(tap.cursor.load(Ordering::Acquire), 3, "one push per frame");
        assert_eq!(tap.sample_rate.load(Ordering::Relaxed), 44_100);
        let mono: Vec<f32> = (0..3)
            .map(|i| f32::from_bits(tap.samples[i].load(Ordering::Relaxed)))
            .collect();
        assert_eq!(mono, vec![0.5, 0.0, 0.5], "channels average per frame");
    }

    fn stereo(rate: u32, frames: usize) -> SamplesBuffer {
        SamplesBuffer::new(
            ChannelCount::new(2).expect("stereo"),
            SampleRate::new(rate).expect("rate"),
            (0..frames * 2).map(|i| i as f32).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn chunks_are_one_chunk_long_and_carry_every_sample_in_order() {
        assert_eq!(chunk_frames(SampleRate::new(44_100).unwrap()), 4_410);
        assert_eq!(chunk_frames(SampleRate::new(48_000).unwrap()), 4_800);
        assert_eq!(chunk_frames(SampleRate::new(1).unwrap()), 1, "never zero");

        let mut source = stereo(44_100, 10_000);
        let mut carry = None;
        let mut lens = Vec::new();
        let mut all = Vec::new();
        while let Some(chunk) = pull_chunk(&mut source, &mut carry) {
            assert_eq!(chunk.channels().get(), 2);
            assert_eq!(chunk.sample_rate().get(), 44_100);
            let samples: Vec<f32> = chunk.collect();
            lens.push(samples.len());
            all.extend(samples);
        }
        assert_eq!(
            lens,
            vec![8_820, 8_820, 2_360],
            "full chunks, then the tail"
        );
        let expected: Vec<f32> = (0..20_000).map(|i| i as f32).collect();
        assert_eq!(all, expected, "no sample lost, duplicated or reordered");
        assert!(
            pull_chunk(&mut source, &mut carry).is_none(),
            "stays exhausted"
        );
    }

    #[test]
    fn a_format_change_closes_the_chunk_and_opens_the_next_one() {
        let (input, mut output) = rodio::queue::queue(false);
        input.append(stereo(44_100, 100));
        input.append(stereo(48_000, 100));
        let mut carry = None;
        let first = pull_chunk(&mut output, &mut carry).expect("first span");
        assert_eq!(first.sample_rate().get(), 44_100);
        assert_eq!(first.count(), 200, "the chunk ends at the span boundary");
        let second = pull_chunk(&mut output, &mut carry).expect("second span");
        assert_eq!(second.sample_rate().get(), 48_000);
        let samples: Vec<f32> = second.collect();
        assert_eq!(samples.len(), 200);
        assert_eq!(samples[0], 0.0, "the carried sample opens the new chunk");
    }

    #[test]
    fn decode_pacing_fills_to_the_target_and_then_waits() {
        let target_chunks = (QUEUE_TARGET.as_millis() / CHUNK.as_millis()) as usize;
        assert!(decode_wanted(0));
        assert!(decode_wanted(target_chunks - 1));
        assert!(
            !decode_wanted(target_chunks),
            "at target: sleep, do not decode"
        );
        assert!(!decode_wanted(usize::MAX), "no overflow on absurd counts");
        assert_eq!(queued_audio(3), CHUNK * 3);
        assert!(QUEUE_PRIME < QUEUE_TARGET && LOW_WATER < QUEUE_PRIME);
        // A halted decode thread fits the unload budget with the stop poll.
        assert!(STOP_POLL_INTERVAL + DECODE_JOIN_BUDGET < SHUTDOWN_JOIN_BUDGET);
    }

    #[test]
    fn a_stall_is_a_dry_decoder_with_an_empty_queue_only() {
        assert!(is_stall(true, true));
        assert!(!is_stall(false, true), "a draining queue is a slow network");
        assert!(!is_stall(true, false), "the queued tail still plays out");
        assert!(!is_stall(false, false));
    }

    #[test]
    fn low_water_ignores_the_initial_fill_counts_dips_and_rate_limits_logs() {
        let ms = Duration::from_millis;
        let mut lw = LowWater::new();
        let t0 = Instant::now();
        // Filling from empty is warm-up, not a dip.
        for q in [0, 100, 300] {
            lw.observe(ms(q));
        }
        assert_eq!(lw.dips, 0);
        assert_eq!(lw.report(t0), None);
        lw.observe(ms(2500));
        lw.observe(ms(300));
        lw.observe(ms(200)); // same dip, deeper
        assert_eq!(lw.dips, 1);
        assert_eq!(
            lw.report(t0).as_deref(),
            Some("radio: audio queue low-water 0.2 s, 1 dips")
        );
        assert_eq!(lw.report(t0), None, "nothing new to say");
        lw.observe(ms(2500));
        lw.observe(ms(400));
        assert_eq!(lw.dips, 2);
        assert_eq!(lw.report(t0 + ms(30_000)), None, "at most once a minute");
        assert_eq!(
            lw.report(t0 + LOW_WATER_LOG_INTERVAL).as_deref(),
            Some("radio: audio queue low-water 0.2 s, 2 dips")
        );
    }

    /// The decode thread must stop promptly on halt and on the session stop
    /// flag, and report a dry source as ended — the unload contract.
    #[test]
    fn decode_thread_ends_on_a_dry_source_and_halts_promptly() {
        let (player, _output) = Player::new();
        let player = Arc::new(player);
        // Dry source: ended, and its chunks landed on the player.
        let cancelled = Arc::new(AtomicBool::new(false));
        let c = Arc::clone(&cancelled);
        let decoding = Decoding::spawn(
            stereo(48_000, 4_800),
            Arc::clone(&player),
            Arc::new(AtomicBool::new(false)),
            Box::new(move || c.store(true, Ordering::Release)),
        )
        .ok()
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !decoding.ended() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(decoding.ended());
        assert_eq!(player.len(), 1, "one 100 ms chunk queued");
        drop(decoding);
        assert!(
            cancelled.load(Ordering::Acquire),
            "drop cancels the download"
        );

        // Endless source with nothing draining the queue: the thread parks at
        // the target; the session stop flag ends it well inside the budget.
        let stop = Arc::new(AtomicBool::new(false));
        let endless = rodio::source::SineWave::new(440.0);
        let decoding = Decoding::spawn(
            endless,
            Arc::clone(&player),
            Arc::clone(&stop),
            Box::new(|| {}),
        )
        .ok()
        .expect("spawn");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!decoding.ended());
        stop.store(true, Ordering::Release);
        let started = Instant::now();
        drop(decoding);
        assert!(started.elapsed() < DECODE_JOIN_BUDGET, "halted promptly");
    }

    #[test]
    fn saved_station_with_empty_uuid_still_round_trips() {
        // A hand-entered or legacy save may lack a directory uuid; playback
        // (and the click-ping guard) must tolerate that.
        let saved = SavedStation {
            stationuuid: String::new(),
            name: "Local Icecast".to_string(),
            url: "http://192.168.1.10:8000/live".to_string(),
            ..Default::default()
        };
        let station = station_from_saved(&saved);
        assert_eq!(station.stream_url(), "http://192.168.1.10:8000/live");
        assert!(station.stationuuid.is_empty());
    }
}
