//! Choya DJ sprite + radio garnish (ON AIR badge) from the
//! `choya_radio.png` atlas (1536x1024 RGBA, embedded).
//!
//! Self-contained: own embedded-texture helper (mirroring `theme::embedded_tex`),
//! own rect tables, draw-list blits only — nothing here is interactive. Sprite
//! blits in [`draw_dj_choya`] are clipped to the player-bar rect. The quip
//! bubble is deliberately on the foreground draw list above everything; a
//! `right_limit` clamp keeps it off the hearts column.

use std::sync::atomic::{AtomicU32, Ordering};

use nexus::imgui::{DrawListMut, TextureId, Ui};

use crate::radio::RadioStatus;
use crate::state::AddonState;
use crate::ui::theme;

const SHEET_W: f32 = 1536.0;
const SHEET_H: f32 = 1024.0;

// Pixel rects on `choya_radio.png`: x, y, w, h. Every rect below was verified
// frame-by-frame against a rendered montage of the atlas (2026-08-31). The
// auto-measured DANCE[0], ONAIR, and ZZZ rects were wrong and are
// corrected here — see the comments on each.
const RADIO_DJ_IDLE: [[f32; 4]; 5] = [
    [5.0, 10.0, 212.0, 200.0],
    [221.0, 0.0, 209.0, 210.0],
    [650.0, 13.0, 214.0, 197.0],
    [1131.0, 10.0, 197.0, 201.0],
    [438.0, 10.0, 206.0, 200.0],
];
const RADIO_DJ_MIX: [[f32; 4]; 5] = [
    [10.0, 224.0, 182.0, 190.0],
    [204.0, 230.0, 199.0, 186.0],
    [623.0, 226.0, 194.0, 184.0],
    [829.0, 222.0, 175.0, 187.0],
    [418.0, 223.0, 187.0, 189.0],
];
/// Frame 0 was auto-measured as 182x348 — two stacked sprites merged into one
/// box. Re-measured to the upper dancer's alpha bounds.
const RADIO_DANCE: [[f32; 4]; 6] = [
    [12.0, 428.0, 181.0, 175.0],
    [194.0, 431.0, 180.0, 179.0],
    [528.0, 439.0, 164.0, 165.0],
    [706.0, 422.0, 163.0, 176.0],
    [874.0, 414.0, 132.0, 144.0],
    [1196.0, 230.0, 161.0, 176.0],
];
const RADIO_SLEEP: [[f32; 4]; 4] = [
    [804.0, 853.0, 154.0, 130.0],
    [969.0, 871.0, 132.0, 108.0],
    [1113.0, 861.0, 123.0, 121.0],
    [1247.0, 854.0, 164.0, 131.0],
];
const RADIO_MUTED: [f32; 4] = [694.0, 614.0, 126.0, 158.0];
const RADIO_LOVE: [f32; 4] = [357.0, 612.0, 163.0, 172.0];
/// Red ON AIR speech bubble. The auto-measurer merged it into the warm-EQ
/// block; the rect it proposed ([1314,683,63,59]) is actually the zZZ glyphs.
const RADIO_ONAIR: [f32; 4] = [1386.0, 668.0, 134.0, 82.0];
/// Blue zZZ glyphs. The proposed rect ([1286,690,30,30]) was a small heart.
const RADIO_ZZZ: [f32; 4] = [1313.0, 682.0, 66.0, 60.0];
const RADIO_HEART: [f32; 4] = [1227.0, 680.0, 52.0, 51.0];
const RADIO_NOTE_GOLD: [f32; 4] = [854.0, 683.0, 35.0, 60.0];

/// The DJ stays this far from the bar's right edge so the station list's
/// heart column above is never covered by the pop-out.
const HEART_CLEARANCE: f32 = 56.0;

// Frame pacing at the overlay's ~60 fps render rate (same assumption as the
// `frame_count` animations in `theme.rs`).
const SLEEP_STEP: u32 = 30; // ~2 fps
const DANCE_STEP: u32 = 8; // ~8 fps (connecting); playing runs far slower
const DJ_STEP: u32 = 10;
const MIX_STEP: u32 = 13; // ~4.5 fps — deck work reads as moves, not mashing
/// A mix burst starts every ~12 s and runs the 5 MIX frames once.
const MIX_PERIOD: u32 = 720;
const MIX_LEN: u32 = MIX_STEP * RADIO_DJ_MIX.len() as u32;
/// Heart flash duration after a favorite is added (~1 s).
const LOVE_FRAMES: u32 = 60;

/// Multiplier on every sprite blit's alpha: the mini radio's content-opacity
/// slider. Set for the length of one [`with_alpha`] call
/// on the render thread and put back to 1.0 after.
static ALPHA_SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32

/// The animation clock every radio sprite runs on: wall-clock time counted in
/// 60 fps frames. All the step constants above were tuned against a 60 fps
/// `frame_count`; this keeps their real-world speed on any refresh rate.
/// Same source as `theme::anim_cell` / `theme::anim_phase`.
pub fn tick() -> u32 {
    (theme::elapsed_ms() * 60 / 1000) as u32
}

/// Run `f` with every sprite blit's alpha multiplied by `alpha`.
pub fn with_alpha<R>(alpha: f32, f: impl FnOnce() -> R) -> R {
    /// Puts the scale back even if `f` unwinds (the frame's catch_unwind
    /// would otherwise leave every later blit faded).
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALPHA_SCALE.store(1.0f32.to_bits(), Ordering::Relaxed);
        }
    }
    ALPHA_SCALE.store(alpha.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    let _reset = Reset;
    f()
}

/// How the DJ is drawn: the tab's player bar, or the mini radio strip.
#[derive(Clone, Copy)]
pub struct DjLook {
    /// Gap kept between the sprite and the bar's right edge.
    pub right_inset: f32,
    /// Speech-bubble quips on or off.
    pub quips: bool,
    /// Sprite size over the bar-derived size (the mini radio's Choya size).
    pub scale: f32,
    /// Draw past the window's clip (the mini radio: she stands out of a
    /// strip that is its whole window). The tab clips to its bar.
    pub unclipped: bool,
}

impl DjLook {
    /// The tab: clear of the station list's heart column, quips on.
    pub const TAB: DjLook = DjLook {
        right_inset: HEART_CLEARANCE,
        quips: true,
        scale: 1.0,
        unclipped: false,
    };
}

/// Where the DJ stands on a bar: `(centre, size)`. Twice the bar height
/// (48..=170 px) times `look.scale`, feet 4 px above the bar's bottom edge,
/// so she pops OUT over its top. The unscaled sprite's centre sits
/// `right_inset` from the right end; a larger scale grows around it, up
/// and past the right end, while the bar keeps its own size.
pub fn dj_placement(look: DjLook, bar_min: [f32; 2], bar_max: [f32; 2]) -> ([f32; 2], f32) {
    let base = dj_base_size(bar_max[1] - bar_min[1]);
    let size = base * look.scale;
    (
        [
            bar_max[0] - look.right_inset - base * 0.5,
            bar_max[1] - 4.0 - size * 0.5,
        ],
        size,
    )
}

/// The unscaled sprite size for a bar `bar_h` tall: also the width the
/// bar's text keeps clear of at the right end.
pub fn dj_base_size(bar_h: f32) -> f32 {
    ((bar_h - 6.0) * 2.0).clamp(48.0, 170.0)
}

/// [`tick`] of the last favorite-add, 0 = never (`flash_love` stores at
/// least 1, so 0 stays a safe sentinel).
static LOVE_FLASH: AtomicU32 = AtomicU32::new(0);

/// Beat phase, fixed-point x16, advanced once per rendered frame while
/// playing — faster the harder the bass hits, so the DJ's frame stepping and
/// bob follow the music instead of the wall clock.
static BEAT_PHASE: AtomicU32 = AtomicU32::new(0);
/// Smoothed bass (f32 bits) for animation pacing. Raw per-frame bass jitters
/// wildly; driving size/bob/speed with it directly reads as a blur (shipped
/// briefly in 1.10.0-dev, reported in-game).
static BASS_SMOOTH: AtomicU32 = AtomicU32::new(0);
/// Hysteresis flag for the dance routine (enter on a strong beat, exit only
/// when it clearly fades) so the sprite set never flickers at the threshold.
static DANCING: AtomicU32 = AtomicU32::new(0);
/// 1 while the previous rendered frame was Playing — the 0->1 edge schedules
/// the tune-in greeting quip.
static WAS_PLAYING: AtomicU32 = AtomicU32::new(0);

const DANCE_ON: f32 = 0.32;
const DANCE_OFF: f32 = 0.18;

// Quips: short lines in a speech bubble near the DJ, fired at a random
// interval between 30 s and 2 min (unpredictability is a sign of
// intelligence), tiered by how hard the music is hitting and flavored by the
// station's genre tags. ASCII only — the game font atlas draws '?' for fancy
// punctuation (a test below enforces that forever).
const QUIP_SHOW: u32 = 720; // ~12 s visible once fired (doubled: "too fast to read")
const QUIP_GAP_MIN: u32 = 1_800; // 30 s between quips...
const QUIP_GAP_MAX: u32 = 7_200; // ...up to 2 min, hash-picked per quip
const QUIPS_CHILL: &[&str] = &[
    "Did the DJ fall asleep or did you?",
    "cozy detected. deploying blanket",
    "funeral for a skritt named greg",
    "This slaps. Gently. Like a pillow.",
    "my spines are relaxing. all 5000",
    "even the jade bot powered down",
    "Desert nights are louder than this.",
    "i vibrate gently. like a fridge",
    "zzz... huh? still this song?",
    "Okay fine, this is snuggle certified.",
];
const QUIPS_GROOVE: &[&str] = &[
    "Toe tap unlocked. Don't get cocky.",
    "this beat waters my roots a bit",
    "not bad for a bunch of humans",
    "This grooves. Who chose it? Not you.",
    "cautious wiggle. do not perceive me",
    "the beat is buffing me. slightly.",
    "Warmer. You almost have taste now.",
    "bobbing commenced. send snacks",
    "i rolled once. do not tell anyone",
    "Fine. Adding this to MY playlist.",
];
const QUIPS_BANGER: &[&str] = &[
    "OKAY WHO ARMED THE CACTUS.",
    "someone hold my juice i am GOING",
    "this drop killed zhaitan actually",
    "I take back every roast. TURN IT UP.",
    "the ground fears my tiny stomps",
    "raptor is doing donuts. same",
    "Ten out of ten. I never say that.",
    "AAAAA MY SPINES ARE VIBRATING",
    "MAP CHAT NEEDS TO HEAR THIS",
    "Bass so fat I grew a new needle.",
];

// Genre flavor tables, picked half the time when the station's tags match
// (the other half keeps the energy-tier variety). AI quips override both.
const QUIPS_METAL: &[&str] = &[
    "A BANGER!",
    "Whoa. Heavy!",
    "mosh pit of one. me.",
    "headbang protocol engaged",
    "my spines ARE the mosh pit",
    "skritt metal. approved.",
];
const QUIPS_ELECTRONIC: &[&str] = &[
    "unz unz unz unz",
    "the drop is coming. brace.",
    "bpm go brrrr",
    "rave cave activated",
    "jade bot is strobing with me",
    "wub wub little cactus",
];
const QUIPS_SOFT: &[&str] = &[
    "gentle. like rain on needles",
    "acoustic headpats",
    "so soft. so warm.",
    "this song is a hug",
    "quaggan lullaby certified",
    "shhh. feelings happening",
];

/// Frame at which the next quip fires; 0 = not yet scheduled.
static QUIP_NEXT: AtomicU32 = AtomicU32::new(0);
/// Frame the current quip fired at; 0 = none yet.
static QUIP_FIRED: AtomicU32 = AtomicU32::new(0);

/// Tier latched when a quip fires, packed `(fired << 2) | tier`, so the text
/// never swaps mid-display when the bass crosses a threshold. (The packing
/// loses the top 2 bits of `fired` — irrelevant below ~200 days of uptime.)
static QUIP_TIER: AtomicU32 = AtomicU32::new(0);

fn lcg(x: u32) -> u32 {
    x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223)
}

/// Hash-picked gap to the next quip: anywhere in 30 s ..= 2 min.
fn quip_gap(seed: u32) -> u32 {
    QUIP_GAP_MIN + lcg(seed) % (QUIP_GAP_MAX - QUIP_GAP_MIN)
}

/// Fade envelope over the visible window; always in 0..=1.
fn quip_alpha(vis: u32) -> f32 {
    // ~400 ms fade-in: the bubble comes into existence, it does not pop.
    let fade_in = (vis as f32 / 24.0).min(1.0);
    // ~2 s fade-out — the old 0.5 s read as vanishing mid-sentence.
    let fade_out = (QUIP_SHOW.saturating_sub(vis) as f32 / 120.0).min(1.0);
    fade_in.min(fade_out).clamp(0.0, 1.0)
}

/// Genre flavor for the station's (lowercased) tags, or None to use the
/// energy tier tables.
fn genre_table(tags: &str) -> Option<&'static [&'static str]> {
    if tags.contains("metal") || tags.contains("punk") || tags.contains("hard rock") {
        Some(QUIPS_METAL)
    } else if [
        "techno", "house", "electro", "dance", "trance", "edm", "drum",
    ]
    .iter()
    .any(|g| tags.contains(g))
    {
        Some(QUIPS_ELECTRONIC)
    } else if [
        "jazz",
        "classical",
        "ambient",
        "chill",
        "lounge",
        "acoustic",
        "piano",
    ]
    .iter()
    .any(|g| tags.contains(g))
    {
        Some(QUIPS_SOFT)
    } else {
        None
    }
}

/// The quip to show this frame (text, fade alpha, slot hash, visible-frame
/// counter), or None between quips. Quips fire at a random 30 s - 2 min
/// interval; the first lands 10-40 s after the bar first renders. A fetched
/// AI quip (`crate::radio::quips::pick`) overrides the canned line.
fn quip_for(t: u32, bass: f32, tags: &str) -> Option<(String, f32, u32, u32)> {
    let next = QUIP_NEXT.load(Ordering::Relaxed);
    if next == 0 {
        // First quip lands 5-15 s in (the tune-in greeting usually schedules
        // sooner and overrides this).
        QUIP_NEXT.store(t + 300 + lcg(t) % 600, Ordering::Relaxed);
        return None;
    }
    if t >= next {
        // Stamp the fire at NOW, not at the scheduled frame: `t` is the
        // GLOBAL overlay frame counter and keeps running while the Radio tab
        // is hidden, so a due moment that passed off-tab would otherwise
        // land with its whole visibility window already expired — the
        // "0 quips ever" bug. This way a due quip greets the next render.
        QUIP_FIRED.store(t.max(1), Ordering::Relaxed);
        QUIP_NEXT.store(t + quip_gap(next), Ordering::Relaxed);
    }
    let fired = QUIP_FIRED.load(Ordering::Relaxed);
    if fired == 0 {
        return None;
    }
    let vis = t.wrapping_sub(fired);
    if vis >= QUIP_SHOW {
        return None;
    }
    let h = lcg(fired);
    let packed = QUIP_TIER.load(Ordering::Relaxed);
    let tier = if packed >> 2 == fired & 0x3FFF_FFFF {
        packed & 3
    } else {
        let tier = if bass > DANCE_ON {
            2
        } else if bass > DANCE_OFF {
            1
        } else {
            0
        };
        QUIP_TIER.store((fired << 2) | tier, Ordering::Relaxed);
        tier
    };
    let table = match genre_table(tags) {
        // Half the time the genre flavor speaks; half stays on the mood tier.
        Some(flavor) if (h >> 10).is_multiple_of(2) => flavor,
        _ => match tier {
            2 => QUIPS_BANGER,
            1 => QUIPS_GROOVE,
            _ => QUIPS_CHILL,
        },
    };
    let quip = crate::radio::quips::pick(h / 7)
        .unwrap_or_else(|| table[(h / 7) as usize % table.len()].to_string());
    Some((quip, quip_alpha(vis), h, vis))
}

/// Tiny dancing choya used as the now-playing marquee's separator between
/// title repeats: a slow dance-frame cycle, alpha-faded by the caller's
/// marquee zone ramp.
pub fn marquee_choya(dl: &DrawListMut, center: [f32; 2], size: f32, t: u32, alpha: f32) {
    let Some(tid) = radio_sheet() else {
        return;
    };
    let i = (t / 20) as usize % RADIO_DANCE.len();
    blit(dl, tid, center, size, RADIO_DANCE[i], alpha);
}

/// Note a favorite was just added; the DJ hugs a heart for ~1 s.
pub fn flash_love(now_frames: u32) {
    LOVE_FLASH.store(now_frames.max(1), Ordering::Relaxed);
}

fn love_flash_active(flash: u32, now: u32) -> bool {
    flash != 0 && now.wrapping_sub(flash) < LOVE_FRAMES
}

/// Horizontal space reserved left of the DJ for the ON AIR badge.
const BADGE_ZONE: f32 = 48.0;

/// Draw the DJ choya inside the player bar, state-driven: sized to the bar
/// height, anchored at its right end, hard-clipped to the bar rect. Returns
/// the width reserved at the bar's right (sprite + badge zone) so the bar's
/// text lines can stay clear of it; 0.0 when the sheet is unavailable.
pub fn draw_dj_choya(
    ui: &Ui,
    dl: &DrawListMut,
    state: &AddonState,
    look: DjLook,
    bass: f32,
    bar_min: [f32; 2],
    bar_max: [f32; 2],
) -> f32 {
    let Some(tid) = radio_sheet() else {
        return 0.0;
    };
    let t = tick();
    // Popping OUT of the bar (`dj_placement`): the top half rises over the
    // stations area (the bar draws after the list, so the DJ reads as
    // standing in front of it). Inset from the right edge so the hearts
    // column stays clear and clickable. The clip rises 1.25x the sprite
    // height so a two-line speech bubble above the head never gets
    // flat-topped.
    let (center, size) = dj_placement(look, bar_min, bar_max);
    let draw = || draw_dj_states(ui, dl, tid, state, center, size, t, bass, look.quips);
    if look.unclipped {
        dl.with_clip_rect([0.0, 0.0], ui.io().display_size, draw);
    } else {
        dl.with_clip_rect_intersect([bar_min[0], bar_min[1] - size * 1.25], bar_max, draw);
    }
    look.right_inset + dj_base_size(bar_max[1] - bar_min[1]) + BADGE_ZONE
}

#[allow(clippy::too_many_arguments)]
fn draw_dj_states(
    ui: &Ui,
    dl: &DrawListMut,
    tid: TextureId,
    state: &AddonState,
    center: [f32; 2],
    size: f32,
    t: u32,
    bass: f32,
    quips: bool,
) {
    let playing = state.radio.status == RadioStatus::Playing;
    let muted = playing && state.config.radio.volume_percent == 0;

    if love_flash_active(LOVE_FLASH.load(Ordering::Relaxed), t) {
        blit(dl, tid, center, size, RADIO_LOVE, 1.0);
        // A little heart drifts up and fades over the flash.
        let age = t.wrapping_sub(LOVE_FLASH.load(Ordering::Relaxed)) as f32;
        let rise = age * 0.6;
        let fade = (1.0 - age / LOVE_FRAMES as f32).clamp(0.0, 1.0);
        blit(
            dl,
            tid,
            [center[0] + size * 0.34, center[1] - size * 0.42 - rise],
            18.0,
            RADIO_HEART,
            fade,
        );
        if playing {
            draw_playing_garnish(dl, tid, center, size, t);
        }
        return;
    }

    match state.radio.status {
        RadioStatus::Connecting | RadioStatus::Buffering | RadioStatus::Stalled => {
            WAS_PLAYING.store(0, Ordering::Relaxed);
            let i = (t / DANCE_STEP) as usize % RADIO_DANCE.len();
            blit(dl, tid, center, size, RADIO_DANCE[i], 1.0);
        }
        RadioStatus::Playing if muted => {
            blit(dl, tid, center, size, RADIO_MUTED, 1.0);
            draw_playing_garnish(dl, tid, center, size, t);
        }
        RadioStatus::Playing => {
            // Fresh tune-in (or unmute / recovery): the DJ greets the new
            // station with a quip a few seconds in — quips must never feel
            // theoretical.
            if WAS_PLAYING.swap(1, Ordering::Relaxed) == 0 {
                QUIP_NEXT.store(t + 180 + lcg(t) % 300, Ordering::Relaxed);
            }

            // Smooth the bass first: the raw value jumps every frame, and
            // everything below keys off the smoothed one so the DJ sways
            // instead of vibrating. (Talk/news pins raw bass near 1.0 —
            // speech is low-frequency heavy — so caps matter more than
            // smoothing here.)
            let prev = f32::from_bits(BASS_SMOOTH.load(Ordering::Relaxed));
            let sb = (prev + (bass - prev) * 0.06).clamp(0.0, 1.0);
            BASS_SMOOTH.store(sb.to_bits(), Ordering::Relaxed);

            // Beat phase: baseline matches the wall clock, full bass adds at
            // most 50%.
            let step = 16 + (sb * 8.0) as u32;
            let phase = BEAT_PHASE
                .fetch_add(step, Ordering::Relaxed)
                .wrapping_add(step)
                / 16;

            // Slow bob plus a gentle size breath; bottom-anchored so the
            // feet stay planted on the bar's edge.
            let bob = (phase as f32 * 0.02).sin() * (2.0 + sb * 3.0);
            let sz = size * (1.0 + sb * 0.04 * (phase as f32 * 0.013).sin().abs());
            let c = [center[0], center[1] + bob - (sz - size) * 0.5];

            // Dance-routine hysteresis: kick in on a strong beat, drop out
            // only once it clearly fades, so the sprite set never flickers.
            let dancing = if DANCING.load(Ordering::Relaxed) != 0 {
                sb > DANCE_OFF
            } else {
                sb > DANCE_ON
            };
            DANCING.store(dancing as u32, Ordering::Relaxed);

            // Pose rates around 2-3 fps: a groove you can follow, per the
            // in-game "way too fast / banging on the board" reports (twice).
            let cycle = t % MIX_PERIOD;
            if cycle < MIX_LEN {
                let i = (cycle / MIX_STEP) as usize % RADIO_DJ_MIX.len();
                blit(dl, tid, c, sz, RADIO_DJ_MIX[i], 1.0);
            } else if dancing {
                let i = (phase / 28) as usize % RADIO_DANCE.len();
                blit(dl, tid, c, sz, RADIO_DANCE[i], 1.0);
            } else {
                let i = (phase / (DJ_STEP * 3)) as usize % RADIO_DJ_IDLE.len();
                blit(dl, tid, c, sz, RADIO_DJ_IDLE[i], 1.0);
            }

            // A quip in a speech bubble now and then, riding the same bob.
            let tags = state
                .radio
                .current
                .as_ref()
                .map(|s| s.tags.to_lowercase())
                .unwrap_or_default();
            if let Some((quip, a, h, vis)) = quip_for(t, bass, &tags).filter(|_| quips) {
                draw_quip_bubble(ui, tid, &quip, a, h, vis, center, c, size, sz, t, bass);
            }
            // A gold note drifts up past the deck now and then.
            let orbit = (t % 240) as f32 / 240.0;
            let note_a = (1.0 - (orbit * 2.0 - 1.0).abs()).clamp(0.0, 0.8);
            blit(
                dl,
                tid,
                [
                    center[0] - size * 0.52,
                    center[1] - size * (0.1 + orbit * 0.45),
                ],
                16.0,
                RADIO_NOTE_GOLD,
                note_a,
            );
            draw_playing_garnish(dl, tid, center, size, t);
        }
        // Idle, Stopped, DeviceLost and Error all read as "off the decks".
        _ => {
            WAS_PLAYING.store(0, Ordering::Relaxed);
            let i = (t / SLEEP_STEP) as usize % RADIO_SLEEP.len();
            blit(dl, tid, center, size, RADIO_SLEEP[i], 1.0);
            let drift = (t as f32 * 0.03).sin();
            blit(
                dl,
                tid,
                [
                    center[0] + size * 0.42,
                    center[1] - size * 0.26 + drift * 3.0,
                ],
                26.0,
                RADIO_ZZZ,
                0.55 + 0.25 * drift,
            );
        }
    }
}

/// Speech bubble for the quip: warm dark rounded plate + soft border + a
/// small tail pointing back at the choya. All the unpredictability derives
/// from the slot hash `h` with zero stored state: a hash-picked anchor
/// among five above-the-head slots (center, left/right leans, higher,
/// lower), a per-slot pixel nudge, a live bass bob applied identically to
/// plate, tail and text, a ~10 frame ease-out scale-in pop, a mood emote
/// drip of tiny atlas sprites while the bubble shows, and a deterministic
/// 1-in-64 ON-AIR overload (badge pulse, spotlight rays, text shiver).
#[allow(clippy::too_many_arguments)]
fn draw_quip_bubble(
    ui: &Ui,
    tid: TextureId,
    quip: &str,
    alpha: f32,
    h: u32,
    vis: u32,
    center: [f32; 2],
    c: [f32; 2],
    size: f32,
    sz: f32,
    t: u32,
    bass: f32,
) {
    // Foreground draw list: the bubble renders on top of EVERYTHING — the
    // now-playing ticker and status text draw after the choya in the window
    // list and were painting straight over the quip.
    let dl = ui.get_foreground_draw_list();
    let dl = &dl;
    // Plate, border and text take the mini radio's content opacity; sprite
    // blits (`sprite_alpha`) already get it inside `blit`.
    let sprite_alpha = alpha;
    let alpha = alpha * f32::from_bits(ALPHA_SCALE.load(Ordering::Relaxed));
    // Anchor roulette — ALWAYS above the head (a low "muffled" slot used to
    // land inside the now-playing ticker): centered, left/right leans, and
    // higher/lower altitude variants.
    let anchor = (h >> 8) % 5;

    // Real measurement at the bubble's own font scale, greedy word-wrap into
    // short lines — the text always fits INSIDE the plate (the old blind
    // px/char estimate shipped an overflowing bubble).
    theme::font_scale(ui, 1.15);
    const MAX_LINE_W: f32 = 200.0;
    const LINE_GAP: f32 = 2.0;
    let mut lines: Vec<String> = vec![String::new()];
    for word in quip.split_whitespace() {
        let cur = lines.last_mut().expect("starts non-empty");
        let cand = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if cur.is_empty() || ui.calc_text_size(&cand)[0] <= MAX_LINE_W {
            *cur = cand;
        } else {
            lines.push(word.to_string());
        }
    }
    let line_h = ui.text_line_height();
    let text_w = lines
        .iter()
        .map(|l| ui.calc_text_size(l)[0])
        .fold(0.0_f32, f32::max);
    let text_h = line_h * lines.len() as f32 + LINE_GAP * (lines.len() - 1) as f32;
    let bw = text_w + 20.0;
    let bh = text_h + 14.0;

    // Per-slot pixel nudge plus a slow cartoon float: a lazy ~7 s up-down
    // drift, with a small faster breath riding on the bass.
    let y_off = ((h >> 16) % 5) as f32 - 2.0
        + (t as f32 * 0.015 + (h % 628) as f32 * 0.01).sin() * 6.0
        + (t as f32 * 0.05).sin() * bass * 1.5;

    // Full-size bubble center + the point near the choya it pops out from.
    // Every slot: horizontal lean x altitude, all strictly above the head
    // (the lowest sits just on top of it — its bottom clears the sprite's
    // upper edge for any realistic size).
    let (lean, altitude) = match anchor {
        1 => (-size * 0.45, 0.62),
        2 => (size * 0.25, 0.62),
        3 => (0.0, 0.82),          // higher
        4 => (-size * 0.25, 0.50), // lower, still above the head
        _ => (0.0, 0.62),
    };
    let above_y = c[1] - sz * altitude - 10.0 - bh * 0.5 + y_off;
    // No room above (the mini radio parked at the top of the screen): hang
    // the bubble under the sprite instead, tail pointing up.
    let below = quip_goes_below(above_y, bh);
    let (mut bx, by, anchor_pt) = if below {
        (
            center[0] + lean,
            center[1] + size * 0.5 + 14.0 + bh * 0.5 + y_off.abs(),
            [c[0], c[1] + sz * 0.42],
        )
    } else {
        (center[0] + lean, above_y, [c[0], c[1] - sz * 0.42])
    };
    // Never cover the hearts column: clamp to the sprite's right edge.
    let right_limit = center[0] + size * 0.5;
    if bx + bw * 0.5 > right_limit {
        bx = right_limit - bw * 0.5;
    }
    // And never leave the screen sideways.
    bx = clamp_to_screen(bx, bw, ui.io().display_size[0]);

    // Scale-in pop: first ~10 visible frames grow 70% -> 100% around the
    // anchor point, eased out.
    let k = (vis as f32 / 10.0).min(1.0);
    let pop = 0.7 + 0.3 * (1.0 - (1.0 - k) * (1.0 - k));
    let s = |p: [f32; 2]| {
        [
            anchor_pt[0] + (p[0] - anchor_pt[0]) * pop,
            anchor_pt[1] + (p[1] - anchor_pt[1]) * pop,
        ]
    };
    let bmin = s([bx - bw * 0.5, by - bh * 0.5]);
    let bmax = s([bx + bw * 0.5, by + bh * 0.5]);
    let bc = [(bmin[0] + bmax[0]) * 0.5, (bmin[1] + bmax[1]) * 0.5];

    // Rare ON-AIR overload: pure-hash 1-in-64 jackpot, self-terminating
    // after ~2 s of the visible window. Same track slot always jackpots.
    let jackpot = lcg(h).is_multiple_of(64) && vis < 120;
    let fill = theme::with_alpha(theme::pal().plate, 0.92 * alpha);
    if jackpot {
        // Three thin spotlight rays rotating behind the bubble.
        for r in 0..3 {
            let ang = t as f32 * 0.05 + r as f32 * 2.094;
            let (sn, cs) = ang.sin_cos();
            let p2 = [bc[0] + cs * 52.0 - sn * 5.0, bc[1] + sn * 52.0 + cs * 5.0];
            let p3 = [bc[0] + cs * 52.0 + sn * 5.0, bc[1] + sn * 52.0 - cs * 5.0];
            dl.add_triangle(
                bc,
                p2,
                p3,
                theme::with_alpha(theme::pal().gold, 0.12 * alpha),
            )
            .filled(true)
            .build();
        }
    }

    dl.add_rect(bmin, bmax, fill)
        .filled(true)
        .rounding(5.0)
        .build();
    dl.add_rect(
        bmin,
        bmax,
        theme::with_alpha(theme::pal().gold_fill, 0.9 * alpha),
    )
    .rounding(5.0)
    .thickness(1.0)
    .build();
    // Tail after the border so its base covers the border segment cleanly;
    // from the edge that faces the head (bottom, or top when flipped).
    let base_x = c[0].clamp(bmin[0] + 8.0, bmax[0] - 8.0);
    let (edge, tip) = if below {
        (bmin[1] + 1.0, bmin[1] - 7.0)
    } else {
        (bmax[1] - 1.0, bmax[1] + 7.0)
    };
    dl.add_triangle(
        [base_x - 5.0, edge],
        [base_x + 5.0, edge],
        [base_x + 1.0, tip],
        fill,
    )
    .filled(true)
    .build();

    // Lines centered horizontally inside the plate, through the pop scale.
    for (li, line) in lines.iter().enumerate() {
        let lw = ui.calc_text_size(line)[0];
        let ly = by - text_h * 0.5 + li as f32 * (line_h + LINE_GAP);
        let lp = s([bx - lw * 0.5, ly]);
        dl.add_text(
            [lp[0] + 1.0, lp[1] + 1.0],
            [0.0, 0.0, 0.0, 0.75 * alpha],
            line,
        );
        dl.add_text(lp, theme::with_alpha(theme::pal().gold, alpha), line);
        if jackpot && bass > 0.6 {
            // The choya seized the mic: 1 px shiver double-draw.
            let dx = (lcg(t.wrapping_add(li as u32)) % 3) as f32 - 1.0;
            dl.add_text(
                [lp[0] + dx, lp[1]],
                theme::with_alpha(theme::pal().gold, 0.6 * alpha),
                line,
            );
        }
    }
    theme::font_scale_reset(ui);
    if jackpot {
        let pulse = 1.0 + 0.15 * (t as f32 * 0.3).sin() * bass;
        blit(
            dl,
            tid,
            [bmax[0] - 4.0, bmin[1] - 4.0],
            34.0 * pulse,
            RADIO_ONAIR,
            sprite_alpha,
        );
    }

    // Mood emote drip: fake particles recomputed per frame from the frame
    // counter alone — nothing allocated, updated, or freed. Sprite follows
    // the latched mood tier; count and rise speed breathe with the bass.
    let (sprite, ssize) = match QUIP_TIER.load(Ordering::Relaxed) & 3 {
        2 => (RADIO_HEART, 13.0),
        1 => (RADIO_NOTE_GOLD, 12.0),
        _ => (RADIO_ZZZ, 14.0),
    };
    let n = 1 + (bass * 3.0) as usize;
    for i in 0..n {
        let dk = ((t as usize + i * 37) % 90) as f32 / 90.0;
        let x = c[0] + (dk * std::f32::consts::TAU + i as f32).sin() * 6.0;
        let y = c[1] - sz * 0.30 - dk * (18.0 + bass * 14.0);
        blit(dl, tid, [x, y], ssize, sprite, (1.0 - dk) * sprite_alpha);
    }
}

/// Whether the quip bubble must hang below the sprite: its above-the-head
/// spot (`above_y` is the bubble centre) would poke past the screen top.
fn quip_goes_below(above_y: f32, bubble_h: f32) -> bool {
    above_y - bubble_h * 0.5 < 0.0
}

/// Bubble centre x kept so the whole `w`-wide bubble stays on a screen
/// `screen_w` wide (unknown width, 0, leaves it alone).
fn clamp_to_screen(x: f32, w: f32, screen_w: f32) -> f32 {
    if screen_w <= w {
        return x;
    }
    x.clamp(w * 0.5 + 2.0, screen_w - w * 0.5 - 2.0)
}

/// ON AIR badge to the DJ's left, inside the bar. The sprite EQ strip is
/// gone — the bar's real equalizer replaced it.
fn draw_playing_garnish(
    dl: &DrawListMut,
    tid: TextureId,
    choya_center: [f32; 2],
    size: f32,
    t: u32,
) {
    let flash = 0.85 + 0.15 * (t as f32 * 0.08).sin();
    blit(
        dl,
        tid,
        [
            choya_center[0] - size * 0.5 - 26.0,
            choya_center[1] + size * 0.18,
        ],
        40.0,
        RADIO_ONAIR,
        flash,
    );
}

fn radio_sheet() -> Option<TextureId> {
    embedded_tex(
        "GW2BO_CHOYA_RADIO",
        include_bytes!("../../assets/choya_radio.png"),
    )
}

/// Private mirror of `theme::embedded_tex` — same Nexus texture cache, own key.
fn embedded_tex(key: &'static str, bytes: &'static [u8]) -> Option<TextureId> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nexus::texture::get_texture_or_create_from_memory(key, bytes)
    }))
    .ok()
    .flatten()
    .map(|t| t.id())
}

/// Half-pixel inset like `theme::sheet_uv`, so bilinear sampling never bleeds
/// a neighboring sprite in.
fn sheet_uv(frame: [f32; 4]) -> ([f32; 2], [f32; 2]) {
    let [x, y, w, h] = frame;
    let inset = 0.5;
    let u0 = ((x + inset) / SHEET_W).clamp(0.0, 1.0);
    let v0 = ((y + inset) / SHEET_H).clamp(0.0, 1.0);
    let u1 = ((x + w - inset) / SHEET_W).clamp(0.0, 1.0);
    let v1 = ((y + h - inset) / SHEET_H).clamp(0.0, 1.0);
    (
        [u0, v0],
        [u1.max(u0 + f32::EPSILON), v1.max(v0 + f32::EPSILON)],
    )
}

/// Aspect-preserving blit centered on `center`; `size` bounds the longer side.
fn blit(
    dl: &DrawListMut,
    tid: TextureId,
    center: [f32; 2],
    size: f32,
    frame: [f32; 4],
    alpha: f32,
) {
    let [_, _, w, h] = frame;
    let aspect = (w / h).max(0.01);
    let (dw, dh) = if aspect > 1.0 {
        (size, size / aspect)
    } else {
        (size * aspect, size)
    };
    let pmin = [center[0] - dw * 0.5, center[1] - dh * 0.5];
    let pmax = [center[0] + dw * 0.5, center[1] + dh * 0.5];
    let (uv0, uv1) = sheet_uv(frame);
    dl.add_image(tid, pmin, pmax)
        .uv_min(uv0)
        .uv_max(uv1)
        .col([
            1.0,
            1.0,
            1.0,
            alpha.clamp(0.0, 1.0) * f32::from_bits(ALPHA_SCALE.load(Ordering::Relaxed)),
        ])
        .build();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_sheet_uvs_stay_on_atlas() {
        let singles = [
            RADIO_MUTED,
            RADIO_LOVE,
            RADIO_ONAIR,
            RADIO_ZZZ,
            RADIO_HEART,
            RADIO_NOTE_GOLD,
        ];
        for frame in RADIO_DJ_IDLE
            .into_iter()
            .chain(RADIO_DJ_MIX)
            .chain(RADIO_DANCE)
            .chain(RADIO_SLEEP)
            .chain(singles)
        {
            let (a, b) = sheet_uv(frame);
            assert!(a[0] >= 0.0 && a[1] >= 0.0, "{frame:?} {a:?}");
            assert!(b[0] <= 1.0 && b[1] <= 1.0, "{frame:?} {b:?}");
            assert!(b[0] > a[0] && b[1] > a[1], "{frame:?}");
        }
    }

    #[test]
    fn radio_rects_stay_inside_the_sheet_in_pixels() {
        let singles = [
            RADIO_MUTED,
            RADIO_LOVE,
            RADIO_ONAIR,
            RADIO_ZZZ,
            RADIO_HEART,
            RADIO_NOTE_GOLD,
        ];
        for [x, y, w, h] in RADIO_DJ_IDLE
            .into_iter()
            .chain(RADIO_DJ_MIX)
            .chain(RADIO_DANCE)
            .chain(RADIO_SLEEP)
            .chain(singles)
        {
            assert!(x >= 0.0 && y >= 0.0);
            assert!(x + w <= SHEET_W, "x+w {} past sheet", x + w);
            assert!(y + h <= SHEET_H, "y+h {} past sheet", y + h);
            assert!(w > 0.0 && h > 0.0);
        }
    }

    #[test]
    fn with_alpha_resets_after_a_panic() {
        let r = std::panic::catch_unwind(|| with_alpha(0.25, || panic!("boom")));
        assert!(r.is_err());
        assert_eq!(f32::from_bits(ALPHA_SCALE.load(Ordering::Relaxed)), 1.0);
    }

    #[test]
    fn quip_flips_below_when_the_strip_is_at_the_top() {
        // Strip parked at y 10: the bubble centre above the head is negative.
        assert!(quip_goes_below(-30.0, 40.0));
        // Just clipping the top edge still flips.
        assert!(quip_goes_below(15.0, 40.0));
        // Bottom-of-screen strip: plenty of room above.
        assert!(!quip_goes_below(900.0, 40.0));
    }

    #[test]
    fn quip_bubble_stays_inside_the_screen() {
        assert_eq!(clamp_to_screen(10.0, 100.0, 1920.0), 52.0);
        assert_eq!(clamp_to_screen(1900.0, 100.0, 1920.0), 1868.0);
        assert_eq!(clamp_to_screen(500.0, 100.0, 1920.0), 500.0);
        assert_eq!(clamp_to_screen(-5.0, 100.0, 0.0), -5.0);
    }

    #[test]
    fn love_flash_lasts_about_a_second() {
        assert!(!love_flash_active(0, 100)); // never flashed
        assert!(love_flash_active(100, 100));
        assert!(love_flash_active(100, 100 + LOVE_FRAMES - 1));
        assert!(!love_flash_active(100, 100 + LOVE_FRAMES));
    }

    #[test]
    fn mix_burst_fits_inside_its_period() {
        const { assert!(MIX_LEN < MIX_PERIOD) };
        // Every MIX frame is reachable inside a burst.
        assert_eq!((MIX_LEN - 1) / MIX_STEP, RADIO_DJ_MIX.len() as u32 - 1);
    }

    #[test]
    fn quips_fire_repeatedly_in_a_continuous_render_run() {
        // Drive quip_for exactly like the render loop does: one call per
        // frame with a monotonically increasing global frame counter. Over
        // ~11 minutes of frames the choya must speak several times, and
        // each appearance must last the full QUIP_SHOW window.
        QUIP_NEXT.store(0, Ordering::Relaxed);
        QUIP_FIRED.store(0, Ordering::Relaxed);
        let mut shown_frames = 0u32;
        let mut appearances = 0u32;
        let mut prev_visible = false;
        // A "warm" game: the overlay has already rendered for a while.
        for t in 987_654..(987_654 + 40_000u32) {
            let visible = quip_for(t, 0.5, "news talk").is_some();
            if visible && !prev_visible {
                appearances += 1;
            }
            if visible {
                shown_frames += 1;
            }
            prev_visible = visible;
        }
        assert!(appearances >= 4, "only {appearances} quips in ~11 min");
        assert!(
            shown_frames >= appearances * (QUIP_SHOW - 1),
            "windows cut short: {shown_frames} frames over {appearances} quips"
        );
        QUIP_NEXT.store(0, Ordering::Relaxed);
        QUIP_FIRED.store(0, Ordering::Relaxed);
    }

    #[test]
    fn quip_gap_stays_between_30s_and_2min() {
        for seed in 0..10_000u32 {
            let gap = quip_gap(seed);
            assert!(
                (QUIP_GAP_MIN..QUIP_GAP_MAX).contains(&gap),
                "seed {seed}: gap {gap}"
            );
        }
    }

    #[test]
    fn quip_alpha_covers_the_window_and_stays_in_unit_range() {
        for vis in 0..QUIP_SHOW {
            let a = quip_alpha(vis);
            assert!((0.0..=1.0).contains(&a), "alpha {a} at vis {vis}");
        }
        assert!(quip_alpha(0) < 0.2); // fades in
                                      // Slowly: a quarter at 100 ms, half at 200 ms, full at 400 ms.
        assert!((quip_alpha(6) - 0.25).abs() < 1e-6);
        assert!((quip_alpha(12) - 0.5).abs() < 1e-6);
        assert_eq!(quip_alpha(24), 1.0);
        assert_eq!(quip_alpha(QUIP_SHOW / 2), 1.0); // fully visible mid-window
        assert!(quip_alpha(QUIP_SHOW - 1) < 0.2); // fades out
        assert_eq!(quip_alpha(QUIP_SHOW + 500), 0.0); // clamped past the end
    }

    #[test]
    fn genre_table_matches_station_tags() {
        assert_eq!(genre_table("metal,rock"), Some(QUIPS_METAL));
        assert_eq!(
            genre_table("deep house,electronica"),
            Some(QUIPS_ELECTRONIC)
        );
        assert_eq!(genre_table("smooth jazz"), Some(QUIPS_SOFT));
        assert_eq!(genre_table("news,talk"), None);
        assert_eq!(genre_table(""), None);
    }

    #[test]
    fn canned_quips_are_ascii_and_fit_the_bubble() {
        // The game font atlas draws '?' tofu for em/en dashes, bullets and
        // emoji — every canned line must be plain ASCII, and short enough
        // for the bubble width estimate, forever.
        for table in [
            QUIPS_CHILL,
            QUIPS_GROOVE,
            QUIPS_BANGER,
            QUIPS_METAL,
            QUIPS_ELECTRONIC,
            QUIPS_SOFT,
        ] {
            assert!(!table.is_empty());
            for quip in table {
                assert!(quip.is_ascii(), "tofu risk: {quip:?}");
                assert!(quip.chars().count() <= 38, "too wide: {quip:?}");
                assert!(!quip.is_empty());
            }
        }
    }
}
