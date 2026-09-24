//! Mini radio: a small strip the player can leave on screen while the
//! overlay is closed. Top row: playback controls, shown only while the mouse
//! is over the strip or Choya. Bottom row: the equalizer with the station and
//! song title scrolling over it. Right end: the dancing Choya standing on the
//! strip and rising out over its top edge, with her quips.
//!
//! Drawn by `ui::render_mini_radio`, which owns the window and the STATE
//! lock; everything here takes `&mut AddonState` from it and never locks
//! STATE itself (pinned by `run_feed::render_paths_never_take_the_state_lock`).
//! Every animation runs on the wall clock (`art::tick`, `theme::elapsed_ms`).

use nexus::imgui::{ColorEdit, DrawListMut, Slider, Ui};

use gw2_core::config::{MiniRadioPrefs, MINI_RADIO_BARS, MINI_RADIO_CHOYA_SCALE, MINI_RADIO_GAP};
use gw2_core::i18n::{t, tf};

use crate::radio::{art, player, RadioStatus, RbStation};
use crate::state::{AddonState, MainTab};
use crate::ui::main_view::radio_tab;
use crate::ui::{color_u32, theme};

/// Narrowest strip that still fits every control.
pub(crate) const MIN_W: f32 = 420.0;
pub(crate) const MAX_W: f32 = 1600.0;
/// Room left under the default spot for the skill bar.
const SKILL_BAR_CLEARANCE: f32 = 150.0;
const GEAR_POPUP: &str = "##mini_radio_gear";
const TAB_GEAR_POPUP: &str = "##mini_radio_tab_gear";

/// Shown when: the toggle is on, the overlay is closed, and the addon is not
/// unloading. Nothing needs to be playing: an idle strip offers the last
/// station's Play button.
pub(crate) fn visible(enabled: bool, main_open: bool, unloading: bool) -> bool {
    enabled && !main_open && !unloading
}

const FADE_IN_MS: f32 = 350.0;
const FADE_OUT_MS: f32 = 200.0;

/// The strip's show/hide transition: the way it is heading and when that
/// started (`theme::elapsed_ms`), `None` once settled. Default: hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fade {
    pub(crate) showing: bool,
    pub(crate) since_ms: Option<u64>,
}

impl Fade {
    /// Alpha at `now`, ease-out both ways: 0 -> 1 over 350 ms while
    /// showing, 1 -> 0 over 200 ms while hiding.
    pub(crate) fn alpha(self, now: u64) -> f32 {
        self.alpha_over(now, FADE_IN_MS, FADE_OUT_MS)
    }

    fn alpha_over(self, now: u64, fade_in: f32, fade_out: f32) -> f32 {
        let dur = if self.showing { fade_in } else { fade_out };
        let k = self
            .since_ms
            .map_or(1.0, |t| (now.saturating_sub(t) as f32 / dur).min(1.0));
        let eased = 1.0 - (1.0 - k) * (1.0 - k);
        if self.showing {
            eased
        } else {
            1.0 - eased
        }
    }

    /// Head toward `want`. A reversal mid-fade starts the new curve at the
    /// current alpha, so the strip never jumps.
    pub(crate) fn toward(self, want: bool, now: u64) -> Fade {
        self.toward_over(want, now, FADE_IN_MS, FADE_OUT_MS)
    }

    fn toward_over(self, want: bool, now: u64, fade_in: f32, fade_out: f32) -> Fade {
        if want == self.showing {
            return self;
        }
        let a = self.alpha_over(now, fade_in, fade_out);
        let (k, dur) = if want {
            (1.0 - (1.0 - a).sqrt(), fade_in)
        } else {
            (1.0 - a.sqrt(), fade_out)
        };
        Fade {
            showing: want,
            since_ms: Some(now.saturating_sub((k * dur) as u64)),
        }
    }

    /// Drawn this frame: shown, or still fading out.
    pub(crate) fn drawn(self, now: u64) -> bool {
        self.showing || self.alpha(now) > 0.0
    }
}

const HOVER_IN_MS: f32 = 150.0;
const HOVER_OUT_MS: f32 = 250.0;
/// The mouse may leave the strip this long before the controls start to
/// fade, so crossing a gap between the box and a button never flickers.
const HOVER_GRACE_MS: u64 = 200;

/// The hover-only controls row: fades in over 150 ms while the mouse is over
/// the strip or Choya, out over 250 ms once it has been away for the grace.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HoverFade {
    fade: Fade,
    last_inside_ms: u64,
}

impl HoverFade {
    pub(crate) fn update(self, inside: bool, now: u64) -> HoverFade {
        let last_inside_ms = if inside { now } else { self.last_inside_ms };
        let want =
            inside || (self.fade.showing && now.saturating_sub(last_inside_ms) < HOVER_GRACE_MS);
        HoverFade {
            fade: self.fade.toward_over(want, now, HOVER_IN_MS, HOVER_OUT_MS),
            last_inside_ms,
        }
    }

    pub(crate) fn alpha(self, now: u64) -> f32 {
        self.fade.alpha_over(now, HOVER_IN_MS, HOVER_OUT_MS)
    }

    fn showing(self) -> bool {
        self.fade.showing
    }
}

/// Gap between the strip's edge and its rows.
const PAD: f32 = 6.0;

/// How Choya is drawn on the strip: the tab's placement with the whole strip
/// as her bar, at the player's size, free of the window's clip.
fn dj_look(prefs: &MiniRadioPrefs) -> art::DjLook {
    art::DjLook {
        right_inset: PAD + 2.0,
        quips: prefs.show_quips,
        scale: prefs.choya_scale.clamp(
            *MINI_RADIO_CHOYA_SCALE.start(),
            *MINI_RADIO_CHOYA_SCALE.end(),
        ),
        unclipped: true,
    }
}

/// Choya's sprite square for a strip at `pos` of `size`: `(min, max)`.
pub(crate) fn choya_rect(look: art::DjLook, pos: [f32; 2], size: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let (c, s) = art::dj_placement(look, pos, [pos[0] + size[0], pos[1] + size[1]]);
    (
        [c[0] - s * 0.5, c[1] - s * 0.5],
        [c[0] + s * 0.5, c[1] + s * 0.5],
    )
}

/// Width the rows keep clear at the strip's right end for her. The frames
/// are square with transparent margins, so 3/4 of the unscaled sprite.
/// A bigger Choya overlaps the rows; the controls draw over her.
fn choya_reserve(look: art::DjLook, strip_h: f32) -> f32 {
    look.right_inset + art::dj_base_size(strip_h) * 0.75
}

fn in_rect(p: [f32; 2], min: [f32; 2], max: [f32; 2]) -> bool {
    p[0] >= min[0] && p[0] < max[0] && p[1] >= min[1] && p[1] < max[1]
}

/// The strip's height for a given width: a fixed aspect, bounded so the
/// control row and the equalizer row both stay usable.
pub(crate) fn height_for(w: f32) -> f32 {
    (w / 5.5).clamp(64.0, 180.0)
}

/// First-run spot: the bottom edge of the screen, centred, above the skill
/// bar. Width follows the screen.
pub(crate) fn default_rect(display: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let w = (display[0] * 0.28).clamp(MIN_W, 760.0);
    let h = height_for(w);
    let x = ((display[0] - w) * 0.5).max(0.0);
    let y = (display[1] - h - SKILL_BAR_CLEARANCE).max(0.0);
    ([x, y], [w, h])
}

/// Keep a saved rect on the current screen: never wider than the display,
/// and moved (not resized) so the whole strip is visible. A display of zero
/// (the first frames before ImGui knows it) leaves the rect alone.
pub(crate) fn clamp_rect(pos: [f32; 2], size: [f32; 2], display: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    if display[0] < 1.0 || display[1] < 1.0 {
        return (pos, size);
    }
    let w = size[0].clamp(
        MIN_W.min(display[0]),
        MAX_W.min(display[0]).max(MIN_W.min(display[0])),
    );
    let h = height_for(w).min(display[1]);
    let x = pos[0].clamp(0.0, (display[0] - w).max(0.0));
    let y = pos[1].clamp(0.0, (display[1] - h).max(0.0));
    ([x, y], [w, h])
}

/// Where the strip goes this frame: the saved rect clamped to the screen, or
/// the default spot when nothing is saved.
pub(crate) fn placement(prefs: &MiniRadioPrefs, display: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let (def_pos, def_size) = default_rect(display);
    clamp_rect(
        prefs.pos.unwrap_or(def_pos),
        prefs.size.unwrap_or(def_size),
        display,
    )
}

/// Previous/Next over a list of station keys, wrapping at both ends. A
/// current station that is not in the list starts at the first entry
/// (Next) or the last (Previous). `None` for an empty list.
pub(crate) fn step_index(keys: &[String], current: Option<&str>, forward: bool) -> Option<usize> {
    let n = keys.len();
    if n == 0 {
        return None;
    }
    Some(
        match current.and_then(|c| keys.iter().position(|k| k == c)) {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        },
    )
}

/// Mute/unmute: `(new volume, level to keep for Unmute)`. Unmute with
/// nothing kept (muted by dragging the slider to 0) comes back at 60%.
pub(crate) fn mute_toggle(volume: u8, kept: u8) -> (u8, u8) {
    match (volume, kept) {
        (0, 0) => (60, 0),
        (0, k) => (k, 0),
        (v, _) => (0, v),
    }
}

/// True when the strip must be put back at its clamped saved spot: a reset
/// was asked for, or the screen size changed since last frame.
pub(crate) fn needs_replace(snap: bool, last_display: [f32; 2], display: [f32; 2]) -> bool {
    snap || last_display != display
}

/// A saved colour, or the theme's when unset, at `alpha`.
pub(crate) fn resolve_color(custom: Option<[f32; 3]>, theme: [f32; 4], alpha: f32) -> [f32; 4] {
    let [r, g, b] = custom.unwrap_or([theme[0], theme[1], theme[2]]);
    [r, g, b, alpha.clamp(0.0, 1.0)]
}

/// The list Previous/Next cycle through: the favourites when "Favourites
/// only" is on, otherwise the Choya Tunes station list in its displayed order.
fn cycle_list(state: &AddonState) -> Vec<RbStation> {
    if state.config.radio.mini_radio.cycle_favourites {
        state
            .config
            .radio
            .favorites
            .iter()
            .map(player::station_from_saved)
            .collect()
    } else {
        state.radio.results.clone()
    }
}

/// Why Previous/Next are disabled, or `None` when there is a list.
fn cycle_blocked(state: &AddonState) -> Option<String> {
    if state.config.radio.mini_radio.cycle_favourites {
        state
            .config
            .radio
            .favorites
            .is_empty()
            .then(|| t("radio.mini.no_favourites"))
    } else if state.radio.results.is_empty() {
        Some(if state.radio.searching {
            t("radio.searching")
        } else {
            t("radio.mini.no_list")
        })
    } else {
        None
    }
}

fn step_station(state: &mut AddonState, forward: bool) {
    let list = cycle_list(state);
    let keys: Vec<String> = list
        .iter()
        .map(|s| radio_tab::fav_key(&s.stationuuid, s.stream_url()))
        .collect();
    let current = radio_tab::current_station_key(state);
    if let Some(i) = step_index(&keys, current.as_deref(), forward) {
        radio_tab::start_play(state, list[i].clone());
    }
}

fn is_live(status: &RadioStatus) -> bool {
    matches!(
        status,
        RadioStatus::Connecting
            | RadioStatus::Buffering
            | RadioStatus::Playing
            | RadioStatus::Paused
            | RadioStatus::Stalled
    )
}

fn resumable(state: &AddonState) -> Option<RbStation> {
    state.radio.current.clone().or_else(|| {
        state
            .config
            .radio
            .last_station
            .as_ref()
            .map(player::station_from_saved)
    })
}

fn set_volume(state: &mut AddonState, v: u8) {
    state.config.radio.volume_percent = v;
    player::set_volume(v);
    crate::ui::save_config_detached(state);
}

#[derive(Clone, Copy)]
enum Icon {
    Prev,
    Play,
    Pause,
    Stop,
    Next,
    Speaker { muted: bool },
    Open,
    Gear,
    Close,
}

/// Square button with a drawn glyph (the game font has no media symbols).
/// A disabled button still shows its tooltip, which says why.
fn icon_button(ui: &Ui, id: &str, icon: Icon, enabled: bool, tip: &str, alpha: f32) -> bool {
    let s = theme::control_height(ui);
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [s, s]);
    let hovered = ui.is_item_hovered();
    let pal = theme::pal();
    let dl = crate::ui::window_draw_list(ui);
    let fill = if hovered && enabled {
        pal.gold_fill
    } else {
        pal.plate
    };
    dl.add_rect(
        p,
        [p[0] + s, p[1] + s],
        theme::with_alpha(fill, 0.85 * alpha),
    )
    .filled(true)
    .rounding(5.0)
    .build();
    let ink = match (enabled, hovered) {
        (false, _) => pal.muted,
        (true, true) => pal.gold_button_text,
        (true, false) => pal.gold,
    };
    draw_icon(&dl, p, s, icon, theme::with_alpha(ink, ink[3] * alpha));
    drop(dl);
    if hovered && !tip.is_empty() {
        theme::wide_tooltip(ui, |ui| ui.text_wrapped(tip));
    }
    clicked && enabled
}

fn draw_icon(dl: &DrawListMut, p: [f32; 2], s: f32, icon: Icon, c: [f32; 4]) {
    let cx = p[0] + s * 0.5;
    let cy = p[1] + s * 0.5;
    let at = |x: f32, y: f32| [cx + x * s, cy + y * s];
    let tri = |a: [f32; 2], b: [f32; 2], d: [f32; 2]| {
        dl.add_triangle(a, b, d, c).filled(true).build();
    };
    let bar = |a: [f32; 2], b: [f32; 2]| {
        dl.add_rect(a, b, c).filled(true).build();
    };
    match icon {
        Icon::Play => tri(at(-0.2, -0.28), at(-0.2, 0.28), at(0.28, 0.0)),
        Icon::Pause => {
            bar(at(-0.22, -0.26), at(-0.07, 0.26));
            bar(at(0.07, -0.26), at(0.22, 0.26));
        }
        Icon::Stop => bar(at(-0.22, -0.22), at(0.22, 0.22)),
        Icon::Next => {
            tri(at(-0.24, -0.26), at(-0.24, 0.26), at(0.14, 0.0));
            bar(at(0.14, -0.26), at(0.26, 0.26));
        }
        Icon::Prev => {
            tri(at(0.24, -0.26), at(0.24, 0.26), at(-0.14, 0.0));
            bar(at(-0.26, -0.26), at(-0.14, 0.26));
        }
        Icon::Speaker { muted } => {
            bar(at(-0.3, -0.1), at(-0.14, 0.1));
            tri(at(-0.16, -0.1), at(0.04, -0.28), at(0.04, 0.28));
            tri(at(-0.16, -0.1), at(0.04, 0.28), at(-0.16, 0.1));
            if muted {
                dl.add_line(at(0.12, -0.12), at(0.32, 0.12), c)
                    .thickness(2.0)
                    .build();
                dl.add_line(at(0.12, 0.12), at(0.32, -0.12), c)
                    .thickness(2.0)
                    .build();
            } else {
                bar(at(0.12, -0.1), at(0.17, 0.1));
                bar(at(0.23, -0.2), at(0.28, 0.2));
            }
        }
        Icon::Open => {
            dl.add_rect(at(-0.28, -0.24), at(0.28, 0.24), c)
                .thickness(1.5)
                .build();
            bar(at(-0.28, -0.24), at(0.28, -0.1));
        }
        Icon::Gear => {
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (sn, cs) = a.sin_cos();
                dl.add_line(at(cs * 0.2, sn * 0.2), at(cs * 0.32, sn * 0.32), c)
                    .thickness(3.0)
                    .build();
            }
            dl.add_circle([cx, cy], s * 0.2, c).thickness(2.0).build();
        }
        Icon::Close => {
            dl.add_line(at(-0.2, -0.2), at(0.2, 0.2), c)
                .thickness(2.0)
                .build();
            dl.add_line(at(-0.2, 0.2), at(0.2, -0.2), c)
                .thickness(2.0)
                .build();
        }
    }
}

/// The strip itself. Called inside the mini window with STATE held. `fade`
/// (0..1, the show/hide transition) scales both configured opacities.
///
/// The window keeps its height: while the controls are hidden the plate
/// shrinks to the equalizer row and the top band is see-through, so the
/// strip never moves under the mouse.
pub(crate) fn render_window(ui: &Ui, state: &mut AddonState, fade: f32) {
    theme::font_scale_reset(ui);
    persist_rect(ui, state);
    radio_tab::ensure_station_list(state);

    let mut prefs = state.config.radio.mini_radio.clone();
    prefs.bg_opacity = prefs.bg_opacity.clamp(0.0, 1.0) * fade;
    prefs.content_opacity = prefs.content_opacity.clamp(0.0, 1.0) * fade;
    let ca = prefs.content_opacity;
    let pal = theme::pal();
    let pos = ui.window_pos();
    let size = ui.window_size();
    let end = [pos[0] + size[0], pos[1] + size[1]];
    let look = dj_look(&prefs);

    // Hover: the strip's rect or Choya's, or a control still being dragged.
    let now = theme::elapsed_ms();
    let mouse = ui.io().mouse_pos;
    let (cmin, cmax) = choya_rect(look, pos, size);
    let inside = in_rect(mouse, pos, end)
        || (prefs.show_choya && in_rect(mouse, cmin, cmax))
        || (ui.is_any_item_active() && state.radio.mini_hover.showing());
    state.radio.mini_hover = state.radio.mini_hover.update(inside, now);
    let ha = state.radio.mini_hover.alpha(now);

    let ctl_h = theme::control_height(ui);
    let left = pos[0] + PAD;
    let right = end[0]
        - PAD
        - if prefs.show_choya {
            choya_reserve(look, size[1])
        } else {
            0.0
        };
    let row1 = pos[1] + PAD;
    let row2_top = row1 + ctl_h + 4.0;

    // The plate list is dropped before the controls row and the popup,
    // which acquire their own (one live window draw list at a time).
    {
        let dl = crate::ui::window_draw_list(ui);
        // Background plate + faint rim, both at the background opacity; the
        // top edge rises over the controls band as they fade in.
        let bg = resolve_color(prefs.bg_color, pal.ink, prefs.bg_opacity);
        let top = [pos[0], pos[1] + (row2_top - PAD - pos[1]) * (1.0 - ha)];
        dl.add_rect(top, end, bg).filled(true).rounding(8.0).build();
        dl.add_rect(
            top,
            end,
            theme::with_alpha(
                pal.gold_dim,
                pal.gold_dim[3] * prefs.bg_opacity.clamp(0.0, 1.0),
            ),
        )
        .rounding(8.0)
        .build();

        let row2_bottom = end[1] - PAD;
        let levels = player::eq_levels();
        let bass = (levels[..6].iter().sum::<f32>() / 6.0).clamp(0.0, 1.0);

        if prefs.show_eq && row2_bottom - row2_top > 4.0 {
            draw_eq(&dl, [left, row2_top], [right, row2_bottom], &levels, &prefs);
        }
        draw_title_row(
            ui,
            &dl,
            state,
            &prefs,
            [left, row2_top],
            [right, row2_bottom],
        );

        if prefs.show_choya {
            if prefs.show_quips {
                crate::radio::quips::tick(state, bass);
            }
            let st: &AddonState = state;
            art::with_alpha(ca, || art::draw_dj_choya(ui, &dl, st, look, bass, pos, end));
        }
    }

    if ha > 0.0 {
        controls_row(ui, state, [left, row1], right, ca * ha);
    }
    ui.popup(GEAR_POPUP, || render_settings_body(ui, state));
}

/// Save the rect once the mouse is released after a move or resize; follow
/// the width live only while the mouse is down (a corner drag), so the
/// clamped saved width is what every other frame uses.
fn persist_rect(ui: &Ui, state: &mut AddonState) {
    let p = ui.window_pos();
    let sz = ui.window_size();
    if ui.is_mouse_down(nexus::imgui::MouseButton::Left) {
        state.radio.mini_live_w = Some(sz[0]);
        return;
    }
    state.radio.mini_live_w = None;
    let mini = &mut state.config.radio.mini_radio;
    let moved = |a: Option<[f32; 2]>, b: [f32; 2]| {
        a.is_none_or(|a| (a[0] - b[0]).abs() > 0.5 || (a[1] - b[1]).abs() > 0.5)
    };
    if moved(mini.pos, p) || moved(mini.size, sz) {
        mini.pos = Some(p);
        mini.size = Some(sz);
        crate::ui::save_config_detached(state);
    }
}

/// Equalizer bars across the bottom row: the 24 analysed bands resampled to
/// the chosen bar count, each bar a vertical gradient from the bar colour at
/// its foot to the peak colour at its top.
fn draw_eq(
    dl: &DrawListMut,
    min: [f32; 2],
    max: [f32; 2],
    levels: &[f32; player::EQ_BANDS],
    prefs: &MiniRadioPrefs,
) {
    if levels.iter().all(|l| *l < 0.004) {
        return;
    }
    let pal = theme::pal();
    let ca = prefs.content_opacity.clamp(0.0, 1.0);
    let foot = resolve_color(prefs.eq_color, pal.gold, 0.55 * ca);
    let peak = resolve_color(prefs.eq_peak_color, pal.cream, 0.85 * ca);
    let count = prefs
        .bar_count
        .clamp(*MINI_RADIO_BARS.start(), *MINI_RADIO_BARS.end()) as usize;
    let gap = f32::from(
        prefs
            .bar_gap
            .clamp(*MINI_RADIO_GAP.start(), *MINI_RADIO_GAP.end()),
    );
    let n = count as f32;
    let bar_w = ((max[0] - min[0]) - gap * (n - 1.0)) / n;
    if bar_w < 1.0 {
        return;
    }
    let reach = (max[1] - min[1]) * prefs.bar_height.clamp(0.2, 1.0);
    for i in 0..count {
        let level = band_at(levels, i, count);
        let h = reach * level;
        if h < 0.5 {
            continue;
        }
        let top = lerp4(foot, peak, level);
        let x = min[0] + i as f32 * (bar_w + gap);
        dl.add_rect_filled_multicolor([x, max[1] - h], [x + bar_w, max[1]], top, top, foot, foot);
    }
}

/// Level of bar `i` of `count`, linearly interpolated over the bands.
fn band_at(levels: &[f32; player::EQ_BANDS], i: usize, count: usize) -> f32 {
    let bands = player::EQ_BANDS;
    let f = ((i as f32 + 0.5) / count as f32 * bands as f32 - 0.5).clamp(0.0, (bands - 1) as f32);
    let lo = f.floor() as usize;
    let hi = (lo + 1).min(bands - 1);
    let k = f - lo as f32;
    (levels[lo] * (1.0 - k) + levels[hi] * k).clamp(0.0, 1.0)
}

fn lerp4(a: [f32; 4], b: [f32; 4], k: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * k)
}

/// Bottom row: the scrolling "station - title" while playing, otherwise the
/// status and the station a Play would resume.
fn draw_title_row(
    ui: &Ui,
    dl: &DrawListMut,
    state: &AddonState,
    prefs: &MiniRadioPrefs,
    min: [f32; 2],
    max: [f32; 2],
) {
    if !prefs.show_title {
        return;
    }
    let ca = prefs.content_opacity.clamp(0.0, 1.0);
    let col = resolve_color(prefs.title_color, theme::pal().cream, ca);
    let row_h = max[1] - min[1];
    if state.radio.status == RadioStatus::Playing {
        let name = state
            .radio
            .current
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let song = state.radio.now_playing.lock().ok().and_then(|g| g.clone());
        // ASCII '-': the game font renders a middle dot or em dash as '?'.
        let text = match song {
            Some(song) if !song.is_empty() && song != name => format!("{name} - {song}"),
            _ => name,
        };
        if !text.is_empty() {
            let scale = (row_h * 0.8 / 42.0).clamp(0.3, 1.0);
            let line_h = 42.0 * scale;
            art::with_alpha(ca, || {
                radio_tab::now_playing_marquee(
                    ui,
                    dl,
                    &text,
                    [min[0], min[1] + ((row_h - line_h) * 0.5).max(0.0)],
                    max[0] - min[0],
                    scale,
                    // The marquee fades glyphs up to 70%; lift the base so the
                    // title reads as crisply as the colour chosen.
                    [col[0], col[1], col[2], (col[3] / 0.7).min(1.0)],
                );
            });
        }
        return;
    }
    let (status, status_col) = radio_tab::status_line(&state.radio.status);
    let name = resumable(state)
        .map(|s| s.name)
        .unwrap_or_else(|| t("radio.mini.no_station"));
    let text = format!("{status} - {name}");
    let shown = radio_tab::clip_text(ui, &text, max[0] - min[0]);
    let y = min[1] + ((row_h - ui.text_line_height()) * 0.5).max(0.0);
    let c = if matches!(
        state.radio.status,
        RadioStatus::Error(_) | RadioStatus::DeviceLost
    ) {
        status_col
    } else {
        col
    };
    dl.add_text(
        [min[0], y],
        color_u32(theme::with_alpha(c, c[3] * ca)),
        &shown,
    );
}

fn controls_row(ui: &Ui, state: &mut AddonState, at: [f32; 2], right: f32, ca: f32) {
    let s = theme::control_height(ui);
    let gap = 4.0;
    ui.set_cursor_screen_pos(at);

    let blocked = cycle_blocked(state);
    let cycle = cycle_tip(state);
    let step_tip = |key: &str| match &blocked {
        Some(why) => why.clone(),
        None => format!("{}\n{}", t(key), cycle),
    };
    if icon_button(
        ui,
        "##mini_prev",
        Icon::Prev,
        blocked.is_none(),
        &step_tip("radio.mini.prev"),
        ca,
    ) {
        step_station(state, false);
    }

    ui.same_line_with_spacing(0.0, gap);
    match state.radio.status.clone() {
        RadioStatus::Playing => {
            if icon_button(ui, "##mini_pause", Icon::Pause, true, &t("radio.pause"), ca) {
                player::pause();
                state.radio.status = RadioStatus::Paused;
            }
        }
        RadioStatus::Paused => {
            if icon_button(ui, "##mini_play", Icon::Play, true, &t("radio.play"), ca) {
                player::resume();
                state.radio.status = RadioStatus::Playing;
            }
        }
        status @ (RadioStatus::Connecting | RadioStatus::Buffering | RadioStatus::Stalled) => {
            let _ = icon_button(
                ui,
                "##mini_play",
                Icon::Play,
                false,
                &radio_tab::status_line(&status).0,
                ca,
            );
        }
        _ => {
            let station = resumable(state);
            let tip = match &station {
                Some(s) => tf("radio.mini.play_tip", &[("station", s.name.as_str())]),
                None => t("radio.mini.no_station"),
            };
            if icon_button(ui, "##mini_play", Icon::Play, station.is_some(), &tip, ca) {
                if let Some(station) = station {
                    radio_tab::start_play(state, station);
                }
            }
        }
    }

    ui.same_line_with_spacing(0.0, gap);
    let live = is_live(&state.radio.status);
    // Signal-only stop: STATE is held here, so the joining stop() never is.
    if icon_button(ui, "##mini_stop", Icon::Stop, live, &t("radio.stop"), ca) {
        player::request_stop();
        state.radio.status = RadioStatus::Stopped;
    }

    ui.same_line_with_spacing(0.0, gap);
    if icon_button(
        ui,
        "##mini_next",
        Icon::Next,
        blocked.is_none(),
        &step_tip("radio.mini.next"),
        ca,
    ) {
        step_station(state, true);
    }

    ui.same_line_with_spacing(0.0, gap * 2.0);
    let volume = state.config.radio.volume_percent;
    let muted = volume == 0;
    let mute_tip = t(if muted {
        "radio.mini.unmute"
    } else {
        "radio.mini.mute"
    });
    if icon_button(
        ui,
        "##mini_mute",
        Icon::Speaker { muted },
        true,
        &mute_tip,
        ca,
    ) {
        let (to, saved) = mute_toggle(volume, state.config.radio.mini_radio.unmute_volume);
        state.config.radio.mini_radio.unmute_volume = saved;
        set_volume(state, to);
    }

    // Right-hand group: open the overlay, settings, hide.
    let right_group = s * 3.0 + gap * 2.0;
    let heart_w = if state.config.radio.mini_radio.cycle_favourites {
        s * 0.8 + gap
    } else {
        0.0
    };
    ui.same_line_with_spacing(0.0, gap);
    let slider_x = ui.cursor_screen_pos()[0];
    let slider_w = (right - right_group - heart_w - gap * 2.0 - slider_x).clamp(40.0, 160.0);
    ui.set_next_item_width(slider_w);
    let mut v = volume;
    let fade = ui.push_style_var(nexus::imgui::StyleVar::Alpha(ca));
    let slid = Slider::new("##mini_vol", 0u8, 100u8)
        .display_format("%d%%")
        .build(ui, &mut v);
    fade.pop();
    if slid {
        state.config.radio.volume_percent = v;
        player::set_volume(v);
    }
    if ui.is_item_deactivated_after_edit() {
        crate::ui::save_config_detached(state);
    }
    if ui.is_item_hovered() {
        let pct = state.config.radio.volume_percent.to_string();
        theme::wide_tooltip(ui, |ui| {
            ui.text_wrapped(tf("radio.mini.volume_tip", &[("pct", pct.as_str())]))
        });
    }

    if state.config.radio.mini_radio.cycle_favourites {
        ui.same_line_with_spacing(0.0, gap);
        let p = ui.cursor_screen_pos();
        let hs = s * 0.8;
        ui.invisible_button("##mini_fav_mark", [hs, s]);
        let heart = [0.92, 0.30, 0.42, ca];
        radio_tab::heart_glyph(
            &crate::ui::window_draw_list(ui),
            [p[0] + hs * 0.5, p[1] + s * 0.5],
            hs * 0.4,
            heart,
        );
        if ui.is_item_hovered() {
            theme::wide_tooltip(ui, |ui| ui.text_wrapped(t("radio.mini.fav_mark_tip")));
        }
    }

    ui.set_cursor_screen_pos([right - right_group, at[1]]);
    if icon_button(
        ui,
        "##mini_open",
        Icon::Open,
        true,
        &t("radio.mini.open_tab"),
        ca,
    ) {
        state.window_visible = true;
        state.config.window_visible = true;
        state.needs_character_reload = true;
        state.main.active_tab = MainTab::Radio;
        crate::ui::save_config_detached(state);
    }
    ui.same_line_with_spacing(0.0, gap);
    if icon_button(
        ui,
        "##mini_gear",
        Icon::Gear,
        true,
        &t("radio.mini.settings"),
        ca,
    ) {
        ui.open_popup(GEAR_POPUP);
    }
    ui.same_line_with_spacing(0.0, gap);
    if icon_button(
        ui,
        "##mini_hide",
        Icon::Close,
        true,
        &t("radio.mini.hide"),
        ca,
    ) {
        state.config.radio.mini_radio.enabled = false;
        crate::ui::save_config_detached(state);
    }
}

fn cycle_tip(state: &AddonState) -> String {
    t(if state.config.radio.mini_radio.cycle_favourites {
        "radio.mini.cycle_tip_favourites"
    } else {
        "radio.mini.cycle_tip_list"
    })
}

/// The Choya Tunes tab's "Mini radio" checkbox and the gear that opens the
/// same settings the strip's own gear shows.
pub(crate) fn render_tab_toggle(ui: &Ui, state: &mut AddonState, label: &str) {
    ui.align_text_to_frame_padding();
    let mut on = state.config.radio.mini_radio.enabled;
    if ui.checkbox(format!("{label}##mini_radio_on"), &mut on) {
        state.config.radio.mini_radio.enabled = on;
        crate::ui::save_config_detached(state);
    }
    if ui.is_item_hovered() {
        theme::wide_tooltip(ui, |ui| ui.text_wrapped(t("radio.mini.toggle_tip")));
    }
    ui.same_line_with_spacing(0.0, 4.0);
    if icon_button(
        ui,
        "##mini_tab_gear",
        Icon::Gear,
        true,
        &t("radio.mini.settings"),
        1.0,
    ) {
        ui.open_popup(TAB_GEAR_POPUP);
    }
    ui.popup(TAB_GEAR_POPUP, || render_settings_body(ui, state));
}

/// Everything the player can set on the strip: cycle list, opacity, which
/// parts show, colours, bar style, resets. Shared by both gear popups.
fn render_settings_body(ui: &Ui, state: &mut AddonState) {
    let mut changed = false;
    theme::header(ui, &t("radio.mini.settings"));

    let m = &mut state.config.radio.mini_radio;
    changed |= checkbox_tip(
        ui,
        "radio.mini.cycle_favourites",
        "radio.mini.cycle_tip_favourites",
        &mut m.cycle_favourites,
    );

    ui.set_next_item_width(200.0);
    changed |= Slider::new(
        format!("{}##mini_bg_a", t("radio.mini.bg_opacity")),
        0.0,
        1.0,
    )
    .display_format("%.2f")
    .build(ui, &mut m.bg_opacity);
    hover_tip(ui, "radio.mini.bg_opacity_tip");
    ui.set_next_item_width(200.0);
    changed |= Slider::new(
        format!("{}##mini_content_a", t("radio.mini.content_opacity")),
        0.2,
        1.0,
    )
    .display_format("%.2f")
    .build(ui, &mut m.content_opacity);
    hover_tip(ui, "radio.mini.content_opacity_tip");

    ui.separator();
    changed |= checkbox_tip(ui, "radio.mini.show_eq", "", &mut m.show_eq);
    ui.same_line();
    changed |= checkbox_tip(ui, "radio.mini.show_title", "", &mut m.show_title);
    ui.same_line();
    changed |= checkbox_tip(ui, "radio.mini.show_choya", "", &mut m.show_choya);
    changed |= checkbox_tip(ui, "radio.mini.show_quips", "", &mut m.show_quips);
    ui.set_next_item_width(200.0);
    changed |= Slider::new(
        format!("{}##mini_choya_scale", t("radio.mini.choya_scale")),
        *MINI_RADIO_CHOYA_SCALE.start(),
        *MINI_RADIO_CHOYA_SCALE.end(),
    )
    .display_format("%.2fx")
    .build(ui, &mut m.choya_scale);
    hover_tip(ui, "radio.mini.choya_scale_tip");

    ui.separator();
    let pal = theme::pal();
    changed |= color_row(ui, "radio.mini.eq_color", "eq", &mut m.eq_color, pal.gold);
    changed |= color_row(
        ui,
        "radio.mini.eq_peak_color",
        "peak",
        &mut m.eq_peak_color,
        pal.cream,
    );
    changed |= color_row(
        ui,
        "radio.mini.title_color",
        "title",
        &mut m.title_color,
        pal.cream,
    );
    changed |= color_row(ui, "radio.mini.bg_color", "bg", &mut m.bg_color, pal.ink);

    ui.separator();
    changed |= stepper(
        ui,
        "radio.mini.bar_count",
        "radio.mini.bar_count_tip",
        &mut m.bar_count,
        MINI_RADIO_BARS,
    );
    changed |= stepper(
        ui,
        "radio.mini.bar_gap",
        "radio.mini.bar_gap_tip",
        &mut m.bar_gap,
        MINI_RADIO_GAP,
    );
    ui.set_next_item_width(200.0);
    changed |= Slider::new(
        format!("{}##mini_bar_h", t("radio.mini.bar_height")),
        0.2,
        1.0,
    )
    .display_format("%.2f")
    .build(ui, &mut m.bar_height);
    hover_tip(ui, "radio.mini.bar_height_tip");

    ui.separator();
    if theme::gold_button(
        ui,
        format!("{}##mini_reset_look", t("radio.mini.reset_appearance")),
    ) {
        m.reset_appearance();
        changed = true;
    }
    ui.same_line();
    if theme::gold_button(
        ui,
        format!("{}##mini_reset_pos", t("radio.mini.reset_position")),
    ) {
        m.pos = None;
        m.size = None;
        state.radio.mini_snap = true;
        state.radio.mini_live_w = None;
        changed = true;
    }
    if changed {
        crate::ui::save_config_detached(state);
    }
}

fn hover_tip(ui: &Ui, key: &str) {
    if ui.is_item_hovered() {
        theme::wide_tooltip(ui, |ui| ui.text_wrapped(t(key)));
    }
}

fn checkbox_tip(ui: &Ui, label_key: &str, tip_key: &str, value: &mut bool) -> bool {
    let changed = ui.checkbox(format!("{}##{label_key}", t(label_key)), value);
    if !tip_key.is_empty() {
        hover_tip(ui, tip_key);
    }
    changed
}

/// Swatch (click for a picker) plus a "use theme colour" checkbox. Unset
/// shows, and edits from, the theme's colour.
fn color_row(
    ui: &Ui,
    label_key: &str,
    id: &str,
    value: &mut Option<[f32; 3]>,
    theme_c: [f32; 4],
) -> bool {
    let mut changed = false;
    let mut rgb = value.unwrap_or([theme_c[0], theme_c[1], theme_c[2]]);
    if ColorEdit::new(format!("{}##mini_col_{id}", t(label_key)), &mut rgb)
        .inputs(false)
        .alpha(false)
        .options(false)
        .build(ui)
    {
        *value = Some(rgb);
        changed = true;
    }
    ui.same_line();
    let mut use_theme = value.is_none();
    if ui.checkbox(
        format!("{}##mini_theme_{id}", t("radio.mini.use_theme")),
        &mut use_theme,
    ) {
        *value = if use_theme { None } else { Some(rgb) };
        changed = true;
    }
    changed
}

fn stepper(
    ui: &Ui,
    label_key: &str,
    tip_key: &str,
    value: &mut u8,
    range: std::ops::RangeInclusive<u8>,
) -> bool {
    let mut v = i32::from(*value);
    ui.set_next_item_width(110.0);
    let edited = ui
        .input_int(format!("{}##{label_key}", t(label_key)), &mut v)
        .step(1)
        .build();
    hover_tip(ui, tip_key);
    let v = v.clamp(i32::from(*range.start()), i32::from(*range.end())) as u8;
    let changed = edited && v != *value;
    *value = v;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn visibility_table() {
        // (enabled, main_open, unloading) -> shown
        let table = [
            (false, false, false, false),
            (false, true, false, false),
            (true, false, false, true),
            (true, true, false, false),
            (true, false, true, false),
            (true, true, true, false),
            (false, false, true, false),
            (false, true, true, false),
        ];
        for (enabled, main_open, unloading, want) in table {
            assert_eq!(
                visible(enabled, main_open, unloading),
                want,
                "enabled={enabled} main_open={main_open} unloading={unloading}"
            );
        }
    }

    #[test]
    fn stepping_over_the_genre_list_wraps() {
        let list = keys(&["a", "b", "c"]);
        assert_eq!(step_index(&list, Some("a"), true), Some(1));
        assert_eq!(step_index(&list, Some("c"), true), Some(0));
        assert_eq!(step_index(&list, Some("a"), false), Some(2));
        assert_eq!(step_index(&list, Some("b"), false), Some(0));
    }

    #[test]
    fn stepping_over_favourites_wraps_and_single_entry_stays() {
        let favs = keys(&["fav1", "fav2"]);
        assert_eq!(step_index(&favs, Some("fav2"), true), Some(0));
        assert_eq!(step_index(&favs, Some("fav1"), false), Some(1));
        let one = keys(&["only"]);
        assert_eq!(step_index(&one, Some("only"), true), Some(0));
        assert_eq!(step_index(&one, Some("only"), false), Some(0));
    }

    #[test]
    fn current_not_in_list_starts_at_the_ends() {
        let list = keys(&["a", "b", "c"]);
        assert_eq!(step_index(&list, Some("zzz"), true), Some(0));
        assert_eq!(step_index(&list, None, true), Some(0));
        assert_eq!(step_index(&list, Some("zzz"), false), Some(2));
    }

    #[test]
    fn empty_list_has_nothing_to_step_to() {
        assert_eq!(step_index(&[], Some("a"), true), None);
        assert_eq!(step_index(&[], None, false), None);
    }

    #[test]
    fn colour_falls_back_to_theme_when_unset() {
        let theme = [0.1, 0.2, 0.3, 0.9];
        assert_eq!(resolve_color(None, theme, 0.5), [0.1, 0.2, 0.3, 0.5]);
        assert_eq!(
            resolve_color(Some([1.0, 0.0, 0.5]), theme, 2.0),
            [1.0, 0.0, 0.5, 1.0]
        );
    }

    #[test]
    fn restored_rect_is_clamped_to_a_smaller_screen() {
        // Saved on 2560x1440 at the bottom right; restored on 1280x720.
        let (pos, size) = clamp_rect([2000.0, 1300.0], [700.0, 127.0], [1280.0, 720.0]);
        assert_eq!(size[0], 700.0);
        assert!(pos[0] + size[0] <= 1280.0 && pos[1] + size[1] <= 720.0);
        assert!(pos[0] >= 0.0 && pos[1] >= 0.0);
        // Wider than the new screen: shrinks to fit.
        let (pos, size) = clamp_rect([-50.0, -20.0], [1500.0, 180.0], [1024.0, 768.0]);
        assert_eq!(size[0], 1024.0);
        assert_eq!(pos, [0.0, 0.0]);
        // Unknown display (first frames): left alone.
        assert_eq!(
            clamp_rect([5.0, 6.0], [500.0, 90.0], [0.0, 0.0]),
            ([5.0, 6.0], [500.0, 90.0])
        );
    }

    #[test]
    fn saved_wide_rect_is_clamped_on_a_smaller_display() {
        let prefs = MiniRadioPrefs {
            pos: Some([1800.0, 1300.0]),
            size: Some([1400.0, 180.0]),
            ..MiniRadioPrefs::default()
        };
        let (pos, size) = placement(&prefs, [1280.0, 720.0]);
        assert_eq!(size[0], 1280.0);
        assert_eq!(pos[0], 0.0);
        assert!(pos[1] + size[1] <= 720.0);
        // The display change is what triggers re-applying it.
        assert!(needs_replace(false, [2560.0, 1440.0], [1280.0, 720.0]));
        assert!(!needs_replace(false, [1280.0, 720.0], [1280.0, 720.0]));
        assert!(needs_replace(true, [1280.0, 720.0], [1280.0, 720.0]));
    }

    #[test]
    fn mute_keeps_the_level_for_unmute() {
        assert_eq!(mute_toggle(45, 0), (0, 45));
        assert_eq!(mute_toggle(0, 45), (45, 0));
        // Muted by the slider, nothing kept: a sane level, not silence.
        assert_eq!(mute_toggle(0, 0), (60, 0));
    }

    #[test]
    fn default_spot_is_bottom_centre_above_the_skill_bar() {
        let (pos, size) = default_rect([1920.0, 1080.0]);
        assert!(((pos[0] + size[0] * 0.5) - 960.0).abs() < 0.5);
        assert!(pos[1] + size[1] <= 1080.0 - SKILL_BAR_CLEARANCE + 0.5);
        assert!(pos[1] > 1080.0 * 0.6, "bottom edge, not the top");
        assert_eq!(size[1], height_for(size[0]));
        let (pos, _) = placement(&MiniRadioPrefs::default(), [1920.0, 1080.0]);
        assert_eq!(pos, default_rect([1920.0, 1080.0]).0);
    }

    #[test]
    fn fade_in_and_out_ease_over_their_lengths() {
        let t0 = 1_000;
        let shown = Fade::default().toward(true, t0);
        assert_eq!(shown.alpha(t0), 0.0);
        let mid = shown.alpha(t0 + 175);
        assert!(
            (mid - 0.75).abs() < 1e-6,
            "ease-out: 3/4 at half time, got {mid}"
        );
        assert_eq!(shown.alpha(t0 + 350), 1.0);
        assert_eq!(shown.alpha(t0 + 5_000), 1.0);

        let t1 = 10_000;
        let hidden = Fade {
            showing: true,
            since_ms: None,
        }
        .toward(false, t1);
        assert_eq!(hidden.alpha(t1), 1.0);
        let mid = hidden.alpha(t1 + 100);
        assert!(
            (mid - 0.25).abs() < 1e-6,
            "ease-out: 1/4 left at half time, got {mid}"
        );
        assert_eq!(hidden.alpha(t1 + 200), 0.0);
        assert!(hidden.drawn(t1 + 199) && !hidden.drawn(t1 + 200));

        // Settled states, and heading the way it already goes is a no-op.
        assert_eq!(Fade::default().alpha(t0), 0.0);
        assert!(!Fade::default().drawn(t0));
        assert_eq!(shown.toward(true, t0 + 50), shown);
    }

    #[test]
    fn reversing_mid_fade_keeps_the_alpha() {
        let shown = Fade::default().toward(true, 0);
        let a = shown.alpha(120);
        let back = shown.toward(false, 120);
        assert!(
            (back.alpha(120) - a).abs() < 0.02,
            "{} vs {a}",
            back.alpha(120)
        );
        let again = back.toward(true, 150);
        assert!((again.alpha(150) - back.alpha(150)).abs() < 0.02);
    }

    #[test]
    fn controls_fade_in_on_hover_and_out_after_the_grace() {
        let t0 = 1_000;
        let h = HoverFade::default().update(true, t0);
        assert_eq!(h.alpha(t0), 0.0);
        assert!((h.alpha(t0 + 75) - 0.75).abs() < 1e-6, "ease-out, mid");
        assert_eq!(h.alpha(t0 + 150), 1.0);

        // Left at t1: the grace keeps it fully shown...
        let t1 = 5_000;
        let h = h.update(true, t1).update(false, t1 + 1);
        let h = h.update(false, t1 + HOVER_GRACE_MS - 1);
        assert_eq!(h.alpha(t1 + HOVER_GRACE_MS - 1), 1.0);
        // ...back inside within it: no flicker at all.
        let back = h.update(true, t1 + 150);
        assert_eq!(back.alpha(t1 + 150), 1.0);
        // Away past the grace: fades out over 250 ms.
        let t2 = t1 + HOVER_GRACE_MS;
        let h = h.update(false, t2);
        assert_eq!(h.alpha(t2), 1.0);
        assert!((h.alpha(t2 + 125) - 0.25).abs() < 1e-6, "ease-out, mid");
        assert_eq!(h.alpha(t2 + 250), 0.0);
    }

    #[test]
    fn choya_stands_on_the_strip_and_rises_over_its_top() {
        let prefs = MiniRadioPrefs::default();
        for w in [MIN_W, 700.0] {
            let pos = [100.0, 500.0];
            let size = [w, height_for(w)];
            let end = [pos[0] + size[0], pos[1] + size[1]];
            let base = art::dj_base_size(size[1]);
            // Same placement as the tab: twice the strip height, capped.
            assert_eq!(base, ((size[1] - 6.0) * 2.0).clamp(48.0, 170.0));
            for scale in [1.0, 2.0] {
                let look = art::DjLook {
                    scale,
                    ..dj_look(&prefs)
                };
                let (min, max) = choya_rect(look, pos, size);
                let s = base * scale;
                assert!((max[0] - min[0] - s).abs() < 1e-3, "w={w} scale={scale}");
                // Feet 4 px above the strip's bottom edge...
                assert!((max[1] - (end[1] - 4.0)).abs() < 1e-3);
                // ...her body out over the top edge.
                assert!(min[1] < pos[1], "w={w} scale={scale}: {min:?}");
                // Anchored at the right end: the unscaled centre sits the
                // inset plus half a base in; scaling grows around it.
                let cx = (min[0] + max[0]) * 0.5;
                assert!((cx - (end[0] - PAD - 2.0 - base * 0.5)).abs() < 1e-3);
                if scale == 1.0 {
                    assert!(max[0] <= end[0]);
                } else {
                    assert!(max[0] > end[0], "a bigger Choya passes the right end");
                }
            }
        }
        // 420 wide: 76 px strip, 141 px Choya; 700 wide: 127 px strip, 170.
        assert!((art::dj_base_size(height_for(MIN_W)) - 140.73).abs() < 0.1);
        assert_eq!(art::dj_base_size(height_for(700.0)), 170.0);
    }

    #[test]
    fn choya_scale_is_clamped() {
        let mut prefs = MiniRadioPrefs {
            choya_scale: 9.0,
            ..MiniRadioPrefs::default()
        };
        assert_eq!(dj_look(&prefs).scale, 2.5);
        prefs.choya_scale = 0.0;
        assert_eq!(dj_look(&prefs).scale, 0.5);
    }

    #[test]
    fn bars_resample_the_bands() {
        let mut levels = [0.0; player::EQ_BANDS];
        levels[..3].fill(1.0); // bar 0 of 8 covers bands 0..3
        assert!(band_at(&levels, 0, 8) > 0.0);
        assert_eq!(band_at(&levels, 7, 8), 0.0);
        let flat = [0.5; player::EQ_BANDS];
        for i in 0..48 {
            assert!((band_at(&flat, i, 48) - 0.5).abs() < 1e-6);
        }
    }
}

/// imgui-rs keeps one live `DrawListMut` per list kind for the whole process
/// and panics on a second ("already loaded": the in-game crash of
/// 2026-09-24, the strip's plate list still live around its icon buttons).
/// This scan pins the rule for the radio UI files, per function body:
///
/// - a window draw list is live from a `let x = ..window_draw_list(..)`
///   binding until its block's closing brace or `drop(x)`, and for the whole
///   body when it arrives as an `x: &DrawListMut` parameter;
/// - while one is live the body may not acquire another, nor call a drawing
///   helper unless the call passes a live list (a helper that receives the
///   list never acquires its own);
/// - drawing helpers: the names in `named_helper`, plus every fn in the same
///   file whose body acquires or calls one (to a fixpoint).
///
/// ponytail: textual, not an AST. Comments and literals are blanked; an alias
/// or a macro would slip past, and cross-file helpers are named by hand.
#[cfg(test)]
mod draw_list_scan {
    use std::collections::HashSet;

    const ACQUIRE: [&str; 2] = ["get_window_draw_list", "window_draw_list"];

    fn named_helper(code: &str, at: usize, t: &str) -> bool {
        t == "icon_button"
            || t == "heart_glyph"
            || t.starts_with("draw_choya_")
            || t.starts_with("render_")
            || t.contains("marquee")
            || t.contains("equalizer")
            || (t == "header" && code[..at].ends_with("theme::"))
    }

    fn is_ident(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    fn wipe(out: &mut [u8]) {
        for c in out.iter_mut().filter(|c| **c != b'\n') {
            *c = b' ';
        }
    }

    /// Comments, string and char literals blanked (newlines kept), test
    /// modules cut off, so braces and names inside them never count.
    fn production(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = b.to_vec();
        let mut i = 0;
        while i < b.len() {
            let rest = &b[i..];
            let end = if rest.starts_with(b"//") {
                rest.iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |n| i + n)
            } else if rest.starts_with(b"/*") {
                src[i..].find("*/").map_or(b.len(), |n| i + n + 2)
            } else if rest[0] == b'r'
                && (i == 0 || !is_ident(b[i - 1]))
                && rest.len() > 1
                && (rest[1] == b'#' || rest[1] == b'"')
            {
                let hashes = rest[1..].iter().take_while(|&&c| c == b'#').count();
                if rest.get(1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                src[i + 2 + hashes..]
                    .find(&close)
                    .map_or(b.len(), |n| i + 2 + hashes + n + close.len())
            } else if rest[0] == b'"' {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                j + 1
            } else if rest[0] == b'\'' && rest.get(1) == Some(&b'\\') {
                src[i + 2..].find('\'').map_or(b.len(), |n| i + 3 + n)
            } else if rest[0] == b'\'' && rest.get(2) == Some(&b'\'') {
                i + 3
            } else {
                i += 1;
                continue;
            };
            let end = end.min(b.len());
            wipe(&mut out[i..end]);
            i = end;
        }
        let code = String::from_utf8(out).expect("ASCII blanks keep UTF-8");
        match code.find("#[cfg(test)]") {
            Some(n) => code[..n].to_string(),
            None => code,
        }
    }

    /// Index of the bracket closing the one at `open`.
    fn matching(code: &str, open: usize) -> usize {
        let b = code.as_bytes();
        let (o, c) = (b[open], if b[open] == b'(' { b')' } else { b'}' });
        let mut depth = 0usize;
        for (i, &x) in b.iter().enumerate().skip(open) {
            if x == o {
                depth += 1;
            } else if x == c {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
        }
        panic!("unbalanced bracket at byte {open}");
    }

    /// Identifier tokens directly followed by `(` (calls), not definitions.
    fn calls(code: &str) -> Vec<(usize, &str)> {
        let b = code.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if is_ident(b[i]) && (i == 0 || !is_ident(b[i - 1])) {
                let end = i + b[i..].iter().take_while(|&&c| is_ident(c)).count();
                if b.get(end) == Some(&b'(') && !code[..i].trim_end().ends_with("fn") {
                    out.push((i, &code[i..end]));
                }
                i = end;
            } else {
                i += 1;
            }
        }
        out
    }

    struct FnSpan {
        name: String,
        params: (usize, usize),
        body: (usize, usize),
    }

    fn fns(code: &str) -> Vec<FnSpan> {
        let b = code.as_bytes();
        let mut out = Vec::new();
        for (at, _) in code.match_indices("fn ") {
            if at > 0 && is_ident(b[at - 1]) {
                continue;
            }
            let name: String = code[at + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let Some(p) = code[at..].find('(').map(|n| at + n) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let p_end = matching(code, p);
            let next = code[p_end..].find(['{', ';']).map(|n| p_end + n);
            let Some(open) = next.filter(|&n| b[n] == b'{') else {
                continue;
            };
            out.push(FnSpan {
                name,
                params: (p, p_end),
                body: (open, matching(code, open)),
            });
        }
        out
    }

    fn has_word(text: &str, word: &str) -> bool {
        let b = text.as_bytes();
        text.match_indices(word).any(|(i, _)| {
            (i == 0 || !is_ident(b[i - 1])) && b.get(i + word.len()).is_none_or(|&c| !is_ident(c))
        })
    }

    fn is_helper(code: &str, at: usize, t: &str, local: &HashSet<String>) -> bool {
        named_helper(code, at, t) || local.contains(t)
    }

    /// Every rule break in one file, as readable lines.
    fn violations(file: &str, src: &str) -> Vec<String> {
        let code = production(src);
        let spans = fns(&code);
        // Fixpoint: a local fn that acquires or calls a helper is a helper.
        let mut local: HashSet<String> = HashSet::new();
        loop {
            let before = local.len();
            for f in &spans {
                let body = &code[f.body.0..=f.body.1];
                let draws = calls(body)
                    .iter()
                    .any(|&(at, t)| ACQUIRE.contains(&t) || is_helper(body, at, t, &local));
                if draws {
                    local.insert(f.name.clone());
                }
            }
            if local.len() == before {
                break;
            }
        }

        let mut out = Vec::new();
        for f in &spans {
            // Parameters `x: &DrawListMut` are live for the whole body.
            let params = &code[f.params.0..=f.params.1];
            let mut live: Vec<(String, usize)> = params
                .split(',')
                .filter(|p| p.contains("DrawListMut"))
                .filter_map(|p| p.split(':').next())
                .map(|n| {
                    (
                        n.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                            .to_string(),
                        0,
                    )
                })
                .collect();
            let events = {
                let mut ev: Vec<(usize, &str)> = calls(&code[..=f.body.1])
                    .into_iter()
                    .filter(|&(at, _)| at > f.body.0)
                    .collect();
                // `let` and braces drive the scope; merge them in by position.
                for (i, c) in code[f.body.0 + 1..f.body.1].char_indices() {
                    let at = f.body.0 + 1 + i;
                    match c {
                        '{' => ev.push((at, "{")),
                        '}' => ev.push((at, "}")),
                        _ if code[at..].starts_with("let ")
                            && !is_ident(code.as_bytes()[at - 1]) =>
                        {
                            ev.push((at, "let"))
                        }
                        _ => {}
                    }
                }
                ev.sort_by_key(|e| e.0);
                ev
            };
            let mut depth = 1usize;
            for (at, t) in events {
                match t {
                    "{" => depth += 1,
                    "}" => {
                        depth -= 1;
                        live.retain(|(_, d)| *d <= depth);
                    }
                    "let" => {
                        // Up to `;`, or `{` for `let x = { .. };` (the block's own
                        // lets are seen on their own).
                        let stmt_end = code[at..].find([';', '{']).map_or(code.len(), |n| at + n);
                        let stmt = &code[at..stmt_end];
                        if ACQUIRE.iter().any(|a| stmt.contains(&format!("{a}("))) {
                            let name: String = stmt[4..]
                                .trim_start_matches("mut ")
                                .chars()
                                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                                .collect();
                            if let Some((held, _)) = live.first() {
                                out.push(format!(
                                    "{file}: fn {}: `{name}` acquired while `{held}` is live",
                                    f.name
                                ));
                            }
                            // The acquisition call itself is checked here.
                            live.push((format!("\u{0}{name}"), depth));
                        }
                    }
                    "drop" => {
                        let arg = code[at + 5..].split(')').next().unwrap_or("").trim();
                        live.retain(|(n, _)| n.trim_start_matches('\u{0}') != arg);
                    }
                    t if ACQUIRE.contains(&t) => {
                        // Skip the one a `let` just recorded (marked pending).
                        if let Some(p) = live.iter_mut().find(|(n, _)| n.starts_with('\u{0}')) {
                            p.0.remove(0);
                        } else if let Some((held, _)) = live.first() {
                            out.push(format!(
                                "{file}: fn {}: draw list acquired while `{held}` is live",
                                f.name
                            ));
                        }
                    }
                    t if !live.is_empty() && is_helper(&code, at, t, &local) => {
                        let open = at + t.len();
                        let args = &code[open..=matching(&code, open)];
                        if !live
                            .iter()
                            .any(|(n, _)| has_word(args, n.trim_start_matches('\u{0}')))
                        {
                            out.push(format!(
                                "{file}: fn {}: `{t}` called while `{}` is live",
                                f.name, live[0].0
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    #[test]
    fn the_scan_flags_nesting_and_accepts_a_passed_list() {
        let bad =
            "fn a(ui: &Ui) {\n let dl = ui.get_window_draw_list();\n icon_button(ui, \"x\");\n}\n";
        assert_eq!(
            violations("bad", bad).len(),
            1,
            "{:?}",
            violations("bad", bad)
        );
        let twice = "fn a(ui: &Ui) {\n let dl = ui.get_window_draw_list();\n f(&ui.get_window_draw_list());\n}\n";
        assert_eq!(violations("twice", twice).len(), 1);
        // Transitive: `b` draws, so calling it under a live list is nesting.
        let transitive = "fn b(ui: &Ui) { icon_button(ui); }\nfn a(ui: &Ui) {\n let dl = ui.get_window_draw_list();\n b(ui);\n}\n";
        assert_eq!(violations("transitive", transitive).len(), 1);
        let good = "fn a(ui: &Ui) {\n {\n let dl = window_draw_list(ui);\n heart_glyph(&dl, 1.0);\n }\n icon_button(ui, \"}\");\n}\n\
                    fn p(dl: &DrawListMut) { now_playing_marquee(ui, dl); }\n\
                    fn d(ui: &Ui) { let dl = window_draw_list(ui); drop(dl); render_x(ui); }\n";
        assert!(
            violations("good", good).is_empty(),
            "{:?}",
            violations("good", good)
        );
    }

    #[test]
    fn no_window_draw_list_is_live_across_a_drawing_call() {
        let mut all = Vec::new();
        for (file, src) in [
            ("mini_radio.rs", include_str!("mini_radio.rs")),
            ("radio/art.rs", include_str!("../radio/art.rs")),
            ("radio/quips.rs", include_str!("../radio/quips.rs")),
            ("tabs/radio.rs", include_str!("main_view/tabs/radio.rs")),
        ] {
            all.extend(violations(file, src));
        }
        assert!(
            all.is_empty(),
            "nested window draw lists:\n{}",
            all.join("\n")
        );
    }
}
