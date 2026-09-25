//! Mini radio: a small strip the player can leave on screen while the
//! overlay is closed. Top row: playback controls in their own window, shown
//! only while the mouse is over the strip or Choya. Bottom row: the equalizer with the station and
//! song title scrolling over it. Right end: the dancing Choya standing on the
//! strip and rising out over its top edge, with her quips.
//!
//! Drawn by `ui::render_mini_radio`, which owns the window. The body paints
//! a snapshot of STATE and does not hold the mutex; everything here takes
//! `&mut AddonState` from that snapshot and never locks STATE itself
//! (pinned by `run_feed::render_paths_never_take_the_state_lock`).
//! Every animation runs on the wall clock (`art::tick`, `theme::elapsed_ms`).

use nexus::imgui::{ColorEdit, DrawListMut, Slider, Ui, WindowFlags};

use gw2_core::config::{MiniRadioPrefs, MINI_RADIO_BARS, MINI_RADIO_CHOYA_SCALE, MINI_RADIO_GAP};
use gw2_core::i18n::{t, tf};

use crate::radio::{art, player, RadioStatus, RbStation};
use crate::state::{AddonState, MainTab};
use crate::ui::main_view::radio_tab;
use crate::ui::{color_u32, theme};

/// Narrowest strip that still fits every control.
pub(crate) const MIN_W: f32 = 420.0;
pub(crate) const MAX_W: f32 = 1600.0;
/// Default width as a fraction of the screen width, so the strip stays the
/// same relative size on a Full HD or a 5120-wide screen.
const DEFAULT_WIDTH_FRACTION: f32 = 0.186;
/// Default horizontal centre as a fraction of the screen width.
const DEFAULT_CENTRE_FRACTION: f32 = 0.70;
/// Default bottom margin as a fraction of the screen height.
const DEFAULT_BOTTOM_MARGIN_FRACTION: f32 = 0.0132;
const GEAR_POPUP: &str = "##mini_radio_gear";
const TAB_GEAR_POPUP: &str = "##mini_radio_tab_gear";

/// Shown when: the toggle is on, the overlay is closed, and the addon is not
/// unloading. Nothing needs to be playing: an idle strip offers the last
/// station's Play button.
pub(crate) fn visible(enabled: bool, main_open: bool, unloading: bool) -> bool {
    enabled && !main_open && !unloading
}

/// Window flags: `(strip, controls)`. Two windows in both modes, so they
/// look the same: the strip (plate, equalizer, title, Choya) and, over its
/// top band, the controls row as its own window, created only while the
/// hover fade is above zero.
///
/// Anchored, the strip is pinned (no move, no resize) and `NO_INPUTS`, whose
/// `NoMouseInputs` part makes ImGui 1.80 skip it in `FindHoveredWindow`, so
/// the game keeps the mouse over it. The controls window never moves or
/// resizes and always takes input, so the buttons stay usable. The strip
/// never comes to the front on a click, so the controls stay above it.
pub(crate) fn window_flags(anchored: bool) -> (WindowFlags, WindowFlags) {
    let base = WindowFlags::NO_TITLE_BAR
        | WindowFlags::NO_SCROLLBAR
        | WindowFlags::NO_SCROLL_WITH_MOUSE
        | WindowFlags::NO_COLLAPSE
        | WindowFlags::NO_SAVED_SETTINGS
        | WindowFlags::NO_FOCUS_ON_APPEARING
        | WindowFlags::NO_NAV;
    let mut strip = base | WindowFlags::NO_BRING_TO_FRONT_ON_FOCUS;
    if anchored {
        strip |= WindowFlags::NO_MOVE | WindowFlags::NO_RESIZE | WindowFlags::NO_INPUTS;
    }
    let controls =
        base | WindowFlags::NO_MOVE | WindowFlags::NO_RESIZE | WindowFlags::NO_BACKGROUND;
    (strip, controls)
}

/// A screen rect as `(min, max)`.
pub(crate) type Span = ([f32; 2], [f32; 2]);

/// The mouse is over the strip, Choya's sprite (`None` while she is hidden)
/// or the controls window. Plain rect tests on `io.mouse_pos`, so an
/// anchored strip that takes no mouse input still knows when to show them.
pub(crate) fn hovering(mouse: [f32; 2], strip: Span, choya: Option<Span>, controls: Span) -> bool {
    [Some(strip), choya, Some(controls)]
        .into_iter()
        .flatten()
        .any(|(min, max)| in_rect(mouse, min, max))
}

/// The controls window's rect for a row from `at` to `right`, `h` tall,
/// grown by the window padding (ImGui clips half of it) and kept on screen.
pub(crate) fn controls_span(
    at: [f32; 2],
    right: f32,
    h: f32,
    pad: [f32; 2],
    display: [f32; 2],
) -> Span {
    let size = [right - at[0] + pad[0] * 2.0, h + pad[1] * 2.0];
    let mut min = [at[0] - pad[0], at[1] - pad[1]];
    if display_valid(display) {
        min[0] = min[0].clamp(0.0, (display[0] - size[0]).max(0.0));
        min[1] = min[1].clamp(0.0, (display[1] - size[1]).max(0.0));
    }
    (min, [min[0] + size[0], min[1] + size[1]])
}

/// The one anchor setter: the strip's gear popup, the Choya Tunes tab and
/// the keybind all land here. Starts the padlock flash and saves.
pub(crate) fn set_anchored(state: &mut AddonState, on: bool) {
    state.config.radio.mini_radio.anchored = on;
    state.radio.mini_anchor_flash = Some(theme::elapsed_ms());
    crate::ui::save_config_detached(state);
}

const ANCHOR_FLASH_MS: f32 = 1500.0;
/// The padlock's resting opacity (times content opacity) while anchored.
const ANCHOR_FAINT: f32 = 0.35;

/// Padlock opacity: a flash from full that fades over 1.5 s after a flip,
/// settling at [`ANCHOR_FAINT`] while anchored and at 0 once unanchored.
pub(crate) fn padlock_alpha(anchored: bool, flash_at: Option<u64>, now: u64) -> f32 {
    let flash = flash_at.map_or(0.0, |t| {
        (1.0 - now.saturating_sub(t) as f32 / ANCHOR_FLASH_MS).clamp(0.0, 1.0)
    });
    if anchored {
        flash.max(ANCHOR_FAINT)
    } else {
        flash
    }
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

/// First-run spot: centred at 70% of the screen width, near the bottom edge.
/// Width and margin scale with the screen so the strip is the same relative
/// size on Full HD and on a wide/high-resolution display.
pub(crate) fn default_rect(display: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let w = (display[0] * DEFAULT_WIDTH_FRACTION)
        .round()
        .clamp(MIN_W, MAX_W);
    let h = height_for(w);
    let bottom_margin = (display[1] * DEFAULT_BOTTOM_MARGIN_FRACTION).round();
    let centre_x = display[0] * DEFAULT_CENTRE_FRACTION;
    let x = (centre_x - w * 0.5).clamp(0.0, (display[0] - w).max(0.0));
    let y = (display[1] - bottom_margin - h).max(0.0);
    ([x, y], [w, h])
}

/// False for the first frames' display (0x0, 1x1, NaN): nothing may be
/// placed or clamped against it, or the strip lands at (0, 0).
pub(crate) fn display_valid(display: [f32; 2]) -> bool {
    display[0] > 1.0 && display[1] > 1.0
}

/// Keep a saved rect on the current screen: never wider than the display,
/// and moved (not resized) so the whole strip is visible. An invalid display
/// leaves the rect alone.
pub(crate) fn clamp_rect(pos: [f32; 2], size: [f32; 2], display: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    if !display_valid(display) {
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
pub(crate) fn render_window(ui: &Ui, state: &mut AddonState, fade: f32, leaving: bool) {
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
    let controls = controls_span(
        [left, row1],
        right,
        ctl_h,
        ui.clone_style().window_padding,
        ui.io().display_size,
    );

    // Hover: the strip, Choya or the controls, or a control still being
    // dragged. From the mouse position, not ImGui's hover, so it works while
    // an anchored strip lets the mouse through.
    let now = theme::elapsed_ms();
    let choya = prefs.show_choya.then(|| choya_rect(look, pos, size));
    let inside = hovering(ui.io().mouse_pos, (pos, end), choya, controls)
        || (ui.is_any_item_active() && state.radio.mini_hover.showing());
    state.radio.mini_hover = state.radio.mini_hover.update(inside, now);
    let ha = state.radio.mini_hover.alpha(now);

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

        // Padlock just above the plate's left corner, in the controls band:
        // it fades out under the controls and back once they are gone.
        let lock_a =
            ca * (1.0 - ha) * padlock_alpha(prefs.anchored, state.radio.mini_anchor_flash, now);
        if lock_a > 0.0 {
            let s = 10.0;
            draw_padlock(
                &dl,
                [left, row2_top - PAD - s - 2.0],
                s,
                theme::with_alpha(pal.gold, pal.gold[3] * lock_a),
            );
        }
    }

    if controls_drawn(ha) && controls_window(ui, state, controls, ca * ha, leaving) {
        // Opened here, in the strip's ID scope, so the popup outlives the
        // controls window when the mouse moves off the strip into it.
        ui.open_popup(GEAR_POPUP);
    }
    // The popup is its own window, so it stays usable while anchored: the
    // player can unanchor right here, as in the tab's popup.
    ui.popup(GEAR_POPUP, || render_settings_body(ui, state));
}

/// The controls window exists only while the hover fade is above zero: a
/// faded-out row costs nothing.
pub(crate) fn controls_drawn(hover_alpha: f32) -> bool {
    hover_alpha > 0.0
}

/// The controls row in its own window over the strip's top band, begun
/// inside the strip's (Dear ImGui allows a nested `Begin` of another
/// top-level window), so it shares the strip's STATE lock and unwind guard.
/// It takes the mouse only over its own rect. True when the gear was clicked.
fn controls_window(ui: &Ui, state: &mut AddonState, span: Span, ca: f32, leaving: bool) -> bool {
    use nexus::imgui::{Condition, MouseCursor, Window};
    let (_, mut flags) = window_flags(state.config.radio.mini_radio.anchored);
    if leaving {
        flags |= WindowFlags::NO_INPUTS;
    }
    let pad = ui.clone_style().window_padding;
    let (min, max) = span;
    Window::new("##gw2bo_mini_radio_controls")
        .flags(flags)
        .position(min, Condition::Always)
        .size([max[0] - min[0], max[1] - min[1]], Condition::Always)
        .build(ui, || {
            let gear = controls_row(
                ui,
                state,
                [min[0] + pad[0], min[1] + pad[1]],
                max[0] - pad[0],
                ca,
            );
            if ui.is_window_hovered() {
                ui.set_mouse_cursor(Some(MouseCursor::Arrow));
            }
            gear
        })
        .unwrap_or(false)
}

/// A small drawn padlock (no font glyph): shackle outline over a body.
fn draw_padlock(dl: &DrawListMut, p: [f32; 2], s: f32, c: [f32; 4]) {
    dl.add_rect(
        [p[0] + s * 0.22, p[1]],
        [p[0] + s * 0.78, p[1] + s * 0.7],
        c,
    )
    .rounding(s * 0.28)
    .thickness(1.5)
    .build();
    dl.add_rect([p[0], p[1] + s * 0.42], [p[0] + s, p[1] + s], c)
        .filled(true)
        .rounding(1.5)
        .build();
}

/// A strip rect: `(pos, size)`.
pub(crate) type Rect = ([f32; 2], [f32; 2]);

/// One frame of the drag tracker: `(press to keep, rect to save)`.
///
/// Only the player's own drag is ever saved: a press that started on the
/// strip (`pressed_here`) remembers the rect, and the release saves the rect
/// only if it differs from that. Programmatic placement (first frames,
/// display changes, clamping, reset) never has a press, so never saves.
pub(crate) fn drag_step(
    press: Option<Rect>,
    down: bool,
    pressed_here: bool,
    rect: Rect,
) -> (Option<Rect>, Option<Rect>) {
    if down {
        return (press.or(pressed_here.then_some(rect)), None);
    }
    let moved = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() > 0.5 || (a[1] - b[1]).abs() > 0.5;
    let save = press.filter(|p| moved(p.0, rect.0) || moved(p.1, rect.1));
    (None, save.map(|_| rect))
}

/// Save the rect when the player releases a move or resize of the strip;
/// follow the width live only while the mouse is down (a corner drag), so
/// the clamped saved width is what every other frame uses.
fn persist_rect(ui: &Ui, state: &mut AddonState) {
    use nexus::imgui::{MouseButton, WindowHoveredFlags};
    let rect = (ui.window_pos(), ui.window_size());
    let down = ui.is_mouse_down(MouseButton::Left);
    let pressed_here = ui.is_mouse_clicked(MouseButton::Left)
        && ui.is_window_hovered_with_flags(WindowHoveredFlags::ALLOW_WHEN_BLOCKED_BY_ACTIVE_ITEM);
    state.radio.mini_live_w = down.then_some(rect.1[0]);
    let (press, save) = drag_step(state.radio.mini_press, down, pressed_here, rect);
    state.radio.mini_press = press;
    if let Some((p, sz)) = save {
        let mini = &mut state.config.radio.mini_radio;
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

/// The row itself; true when the gear was clicked (the caller opens the popup).
fn controls_row(ui: &Ui, state: &mut AddonState, at: [f32; 2], right: f32, ca: f32) -> bool {
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
    let gear = icon_button(
        ui,
        "##mini_gear",
        Icon::Gear,
        true,
        &t("radio.mini.settings"),
        ca,
    );
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
    gear
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
    // Never gated on the anchor: this popup is the way back from it.
    ui.popup(TAB_GEAR_POPUP, || render_settings_body(ui, state));
}

/// Popup width at font size 13; rows may widen it further.
const SETTINGS_MIN_W: f32 = 460.0;

/// Everything the player can set on the strip: cycle list, opacity, which
/// parts show, colours, bar style, then the anchor and the resets. The one
/// body both gear popups (the strip's and the Choya Tunes tab's) show.
fn render_settings_body(ui: &Ui, state: &mut AddonState) {
    let mut changed = false;
    let s = (ui.current_font_size() / 13.0).max(0.75);
    let slider_w = 240.0 * s;
    theme::header(ui, &t("radio.mini.settings"));
    ui.dummy([SETTINGS_MIN_W * s, 0.0]);

    let m = &mut state.config.radio.mini_radio;
    changed |= checkbox_tip(
        ui,
        "radio.mini.cycle_favourites",
        "radio.mini.cycle_tip_favourites",
        &mut m.cycle_favourites,
    );

    ui.set_next_item_width(slider_w);
    changed |= Slider::new(
        format!("{}##mini_bg_a", t("radio.mini.bg_opacity")),
        0.0,
        1.0,
    )
    .display_format("%.2f")
    .build(ui, &mut m.bg_opacity);
    hover_tip(ui, "radio.mini.bg_opacity_tip");
    ui.set_next_item_width(slider_w);
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
    ui.set_next_item_width(slider_w);
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
    ui.set_next_item_width(slider_w);
    changed |= Slider::new(
        format!("{}##mini_bar_h", t("radio.mini.bar_height")),
        0.2,
        1.0,
    )
    .display_format("%.2f")
    .build(ui, &mut m.bar_height);
    hover_tip(ui, "radio.mini.bar_height_tip");

    ui.separator();
    let mut on = m.anchored;
    if ui.checkbox(format!("{}##mini_anchor", t("radio.mini.anchor")), &mut on) {
        set_anchored(state, on);
    }
    hover_tip(ui, "radio.mini.anchor_tip");
    let m = &mut state.config.radio.mini_radio;
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
    // Room for three digits plus the -/+ buttons at any font scale.
    let st = ui.clone_style();
    let digits = ui.calc_text_size("000")[0] + st.frame_padding[0] * 2.0 + 8.0;
    ui.set_next_item_width(digits + 2.0 * (ui.frame_height() + st.item_inner_spacing[0]));
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

    #[test]
    fn anchor_pins_the_strip_and_passes_the_mouse_through_but_not_the_controls() {
        let pinned = WindowFlags::NO_MOVE | WindowFlags::NO_RESIZE;
        let (strip, controls) = window_flags(true);
        assert!(strip.contains(pinned | WindowFlags::NO_MOUSE_INPUTS | WindowFlags::NO_NAV));
        let (loose, loose_controls) = window_flags(false);
        assert!(!loose.intersects(pinned | WindowFlags::NO_MOUSE_INPUTS));
        // The controls take the mouse, never move or resize, draw no
        // background of their own, and are the same in both modes.
        assert_eq!(controls, loose_controls);
        assert!(controls.contains(pinned | WindowFlags::NO_BACKGROUND));
        assert!(!controls.intersects(WindowFlags::NO_MOUSE_INPUTS));
        // The strip never rises over its controls on a click.
        for s in [strip, loose] {
            assert!(s.contains(WindowFlags::NO_BRING_TO_FRONT_ON_FOCUS));
            assert!(s.contains(WindowFlags::NO_FOCUS_ON_APPEARING));
        }
    }

    #[test]
    fn hover_is_the_strip_choya_or_the_controls() {
        let strip: Span = ([100.0, 500.0], [800.0, 600.0]);
        let choya: Span = ([700.0, 420.0], [800.0, 520.0]);
        let controls: Span = ([100.0, 490.0], [650.0, 530.0]);
        let h = |p: [f32; 2], c: Option<Span>| hovering(p, strip, c, controls);
        assert!(h([400.0, 550.0], None), "on the plate");
        assert!(h([750.0, 450.0], Some(choya)), "on Choya above the strip");
        assert!(!h([750.0, 450.0], None), "Choya hidden: nothing there");
        assert!(h([300.0, 495.0], None), "controls clamped above the top");
        assert!(!h([50.0, 450.0], Some(choya)));
        assert!(!h([800.0, 600.0], Some(choya)), "max edge is outside");
    }

    #[test]
    fn controls_sit_on_the_band_and_stay_on_screen() {
        let pad = [6.0, 4.0];
        let (min, max) = controls_span([106.0, 506.0], 600.0, 30.0, pad, [2560.0, 1440.0]);
        assert_eq!((min, max), ([100.0, 502.0], [606.0, 540.0]));
        // A strip flush with the top-left corner: the padding stays on screen.
        let (min, max) = controls_span([2.0, 1.0], 500.0, 30.0, pad, [2560.0, 1440.0]);
        assert_eq!(min, [0.0, 0.0]);
        assert_eq!(max, [510.0, 38.0], "size kept, moved not shrunk");
        // Unknown display: left alone.
        let (min, _) = controls_span([2.0, 1.0], 500.0, 30.0, pad, [1.0, 1.0]);
        assert_eq!(min, [-4.0, -3.0]);
    }

    #[test]
    fn controls_window_exists_only_while_the_hover_fade_shows() {
        let t0 = 1_000;
        let h = HoverFade::default();
        assert!(!controls_drawn(h.alpha(t0)), "never hovered: no window");
        let h = h.update(true, t0);
        assert!(!controls_drawn(h.alpha(t0)));
        assert!(controls_drawn(h.alpha(t0 + 1)));
        let t1 = t0 + 1_000;
        let h = h.update(true, t1).update(false, t1 + 1);
        let h = h.update(false, t1 + HOVER_GRACE_MS);
        assert!(controls_drawn(h.alpha(t1 + HOVER_GRACE_MS + 249)));
        assert!(
            !controls_drawn(h.alpha(t1 + HOVER_GRACE_MS + 250)),
            "gone after fade-out"
        );
    }

    #[test]
    fn padlock_flashes_then_rests_faint_while_anchored() {
        assert_eq!(padlock_alpha(false, None, 5_000), 0.0);
        assert_eq!(padlock_alpha(true, None, 5_000), ANCHOR_FAINT);
        assert_eq!(padlock_alpha(true, Some(1_000), 1_000), 1.0);
        let mid = padlock_alpha(true, Some(1_000), 1_600);
        assert!(mid > ANCHOR_FAINT && mid < 1.0);
        assert_eq!(padlock_alpha(true, Some(1_000), 2_500), ANCHOR_FAINT);
        // Unanchoring fades the padlock out to nothing.
        assert!(padlock_alpha(false, Some(1_000), 1_300) > 0.0);
        assert_eq!(padlock_alpha(false, Some(1_000), 2_500), 0.0);
    }

    /// Both gear popups show the one settings body, and the anchor lives
    /// only there (with the resets), so the tab's popup is always the way
    /// back from an anchor. The keybind is the other setter, in state.rs.
    #[test]
    fn both_popups_show_one_body_with_the_anchor() {
        let src = include_str!("mini_radio.rs");
        let src = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            src.matches("|| render_settings_body(ui, state))").count(),
            2
        );
        assert!(src.contains("ui.popup(GEAR_POPUP, || render_settings_body"));
        assert!(src.contains("ui.popup(TAB_GEAR_POPUP, || render_settings_body"));
        let body = &src[src.find("fn render_settings_body").unwrap()..];
        let body = &body[..body.find("\nfn ").unwrap()];
        let anchor = body
            .find("##mini_anchor\"")
            .expect("anchor row in the body");
        assert!(anchor < body.find("##mini_reset_look").unwrap());
        assert_eq!(src.matches("##mini_anchor").count(), 1, "one anchor row");
        assert!(
            !src.contains("close_current_popup"),
            "anchoring keeps settings open"
        );
        assert_eq!(src.matches("set_anchored(state, on)").count(), 1);
        assert_eq!(
            src.matches("anchored = ").count(),
            1,
            "only the setter writes it"
        );
        assert!(include_str!("../state.rs").contains("mini_radio::set_anchored(state, on)"));
    }

    const SAVED: Rect = ([900.0, 1100.0], [700.0, 127.27273]);

    #[test]
    fn a_drag_of_the_strip_saves_on_release() {
        let moved = ([950.0, 1000.0], SAVED.1);
        let (press, save) = drag_step(None, true, true, SAVED);
        assert_eq!((press, save), (Some(SAVED), None));
        let (press, save) = drag_step(press, true, false, moved);
        assert_eq!((press, save), (Some(SAVED), None), "nothing saved mid-drag");
        assert_eq!(drag_step(press, false, false, moved), (None, Some(moved)));
        // A click that did not move it (a button on the strip) saves nothing.
        assert_eq!(drag_step(Some(SAVED), false, false, SAVED), (None, None));
    }

    #[test]
    fn a_clamp_adjustment_is_never_saved() {
        let mut prefs = MiniRadioPrefs {
            pos: Some(SAVED.0),
            size: Some(SAVED.1),
            ..Default::default()
        };
        let before = prefs.clone();
        let clamped = placement(&prefs, [1280.0, 720.0]);
        assert_ne!(clamped, SAVED, "the small screen moves it");
        // Mouse up, or held on the game (a camera drag), never on the strip.
        for down in [false, true, false] {
            let (_, save) = drag_step(None, down, false, clamped);
            if let Some((p, s)) = save {
                prefs.pos = Some(p);
                prefs.size = Some(s);
            }
        }
        assert_eq!(prefs, before);
        // Back on the big screen the untouched saved rect comes back whole.
        assert_eq!(placement(&prefs, [2560.0, 1440.0]).0, SAVED.0);
    }

    #[test]
    fn an_unknown_display_places_nothing() {
        for d in [[0.0, 0.0], [1.0, 1.0], [f32::NAN, 1440.0], [2560.0, 0.0]] {
            assert!(!display_valid(d), "{d:?}");
        }
        assert!(display_valid([2560.0, 1440.0]));
    }

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
    fn default_spot_scales_with_the_screen() {
        // 5120x1440: the owner's own reference resolution.
        let (pos, size) = default_rect([5120.0, 1440.0]);
        assert_eq!(size[0], 952.0);
        assert_eq!(size[1], height_for(952.0));
        assert_eq!(pos[0], 3108.0);
        assert_eq!(pos[1], 1440.0 - 19.0 - size[1]);

        // 2560x1440: half the width, half the reference strip.
        let (pos, size) = default_rect([2560.0, 1440.0]);
        assert_eq!(size[0], 476.0);
        assert_eq!(pos[0], 1554.0);
        assert_eq!(pos[1], 1440.0 - 19.0 - size[1]);

        // 4096x1440: an in-between wide display.
        let (pos, size) = default_rect([4096.0, 1440.0]);
        assert_eq!(size[0], (0.186_f32 * 4096.0).round());
        assert_eq!(pos[0], 0.70 * 4096.0 - size[0] * 0.5);
        assert_eq!(pos[1], 1440.0 - 19.0 - size[1]);

        // 1920x1080: Full HD, clamped up to MIN_W.
        let (pos, size) = default_rect([1920.0, 1080.0]);
        assert_eq!(size[0], MIN_W);
        assert_eq!(pos[0], 1134.0);
        assert_eq!(pos[1], 1080.0 - 14.0 - size[1]);
        assert!(pos[1] > 1080.0 * 0.6, "bottom edge, not the top");

        // 1280-wide display: still clamped to MIN_W, never narrower.
        let (_, size) = default_rect([1280.0, 720.0]);
        assert_eq!(size[0], MIN_W);

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
        // Only the column-0 `#[cfg(test)]\nmod ...` marks the test module —
        // an indented `#[cfg(test)]` on a single statement (e.g.
        // `log_disk_error`'s stderr fallback) is production code and must
        // not truncate the rest of the file.
        match code
            .match_indices("#[cfg(test)]\nmod ")
            .find(|&(n, _)| n == 0 || code.as_bytes()[n - 1] == b'\n')
        {
            Some((n, _)) => code[..n].to_string(),
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

    /// Every source file under `crates/addon/src/ui` (plus the sibling
    /// `radio/` module, which the mini radio window shares draw calls with).
    /// `mod.rs` keeps the one raw acquisition — [`super::window_draw_list`]'s
    /// own body — so it stays in the nesting scan but is exempted below from
    /// the "no raw calls elsewhere" rule.
    const UI_FILES: &[(&str, &str)] = &[
        ("mod.rs", include_str!("mod.rs")),
        ("chat_bar.rs", include_str!("chat_bar.rs")),
        ("chat_markup.rs", include_str!("chat_markup.rs")),
        ("comparison.rs", include_str!("comparison.rs")),
        ("cost_format.rs", include_str!("cost_format.rs")),
        ("fonts.rs", include_str!("fonts.rs")),
        ("gear_diff.rs", include_str!("gear_diff.rs")),
        ("gear_sheet.rs", include_str!("gear_sheet.rs")),
        ("icons.rs", include_str!("icons.rs")),
        ("mini_radio.rs", include_str!("mini_radio.rs")),
        ("news_feed.rs", include_str!("news_feed.rs")),
        ("radar_chart.rs", include_str!("radar_chart.rs")),
        ("run_feed.rs", include_str!("run_feed.rs")),
        ("setup.rs", include_str!("setup.rs")),
        ("theme.rs", include_str!("theme.rs")),
        (
            "main_view/build_display.rs",
            include_str!("main_view/build_display.rs"),
        ),
        (
            "main_view/character.rs",
            include_str!("main_view/character.rs"),
        ),
        (
            "main_view/chat_flow.rs",
            include_str!("main_view/chat_flow.rs"),
        ),
        (
            "main_view/generation.rs",
            include_str!("main_view/generation.rs"),
        ),
        (
            "main_view/lock_panel.rs",
            include_str!("main_view/lock_panel.rs"),
        ),
        ("main_view/mod.rs", include_str!("main_view/mod.rs")),
        (
            "main_view/optimization.rs",
            include_str!("main_view/optimization.rs"),
        ),
        (
            "main_view/optimize_flow.rs",
            include_str!("main_view/optimize_flow.rs"),
        ),
        (
            "main_view/provider_picks.rs",
            include_str!("main_view/provider_picks.rs"),
        ),
        (
            "main_view/resolution.rs",
            include_str!("main_view/resolution.rs"),
        ),
        ("main_view/stats.rs", include_str!("main_view/stats.rs")),
        (
            "main_view/tabs/about.rs",
            include_str!("main_view/tabs/about.rs"),
        ),
        (
            "main_view/tabs/about/generations.rs",
            include_str!("main_view/tabs/about/generations.rs"),
        ),
        (
            "main_view/tabs/about/glyphs.rs",
            include_str!("main_view/tabs/about/glyphs.rs"),
        ),
        (
            "main_view/tabs/about/wizard.rs",
            include_str!("main_view/tabs/about/wizard.rs"),
        ),
        (
            "main_view/tabs/improve.rs",
            include_str!("main_view/tabs/improve.rs"),
        ),
        (
            "main_view/tabs/kitchen.rs",
            include_str!("main_view/tabs/kitchen.rs"),
        ),
        (
            "main_view/tabs/mod.rs",
            include_str!("main_view/tabs/mod.rs"),
        ),
        (
            "main_view/tabs/new_build.rs",
            include_str!("main_view/tabs/new_build.rs"),
        ),
        (
            "main_view/tabs/news.rs",
            include_str!("main_view/tabs/news.rs"),
        ),
        (
            "main_view/tabs/radio.rs",
            include_str!("main_view/tabs/radio.rs"),
        ),
        (
            "main_view/tabs/saveload.rs",
            include_str!("main_view/tabs/saveload.rs"),
        ),
        (
            "main_view/tabs/settings.rs",
            include_str!("main_view/tabs/settings.rs"),
        ),
        ("radio/art.rs", include_str!("../radio/art.rs")),
        (
            "radio/decode_tests.rs",
            include_str!("../radio/decode_tests.rs"),
        ),
        ("radio/directory.rs", include_str!("../radio/directory.rs")),
        ("radio/logos.rs", include_str!("../radio/logos.rs")),
        ("radio/mod.rs", include_str!("../radio/mod.rs")),
        ("radio/player.rs", include_str!("../radio/player.rs")),
        ("radio/quips.rs", include_str!("../radio/quips.rs")),
    ];

    #[test]
    fn no_window_draw_list_is_live_across_a_drawing_call() {
        let mut all = Vec::new();
        for (file, src) in UI_FILES {
            all.extend(violations(file, src));
        }
        // The controls window's body is a scanned fn of its own, and its
        // build closure is where the controls row draws.
        let code = production(include_str!("mini_radio.rs"));
        let body = fns(&code)
            .into_iter()
            .find(|f| f.name == "controls_window")
            .map(|f| code[f.body.0..=f.body.1].to_string())
            .expect("controls_window is scanned");
        assert!(body.contains(".build(ui,") && body.contains("controls_row("));
        assert!(
            all.is_empty(),
            "nested window draw lists:\n{}",
            all.join("\n")
        );
    }

    /// Every call site must go through [`super::window_draw_list`] (or a
    /// module-local wrapper built on it), never the raw imgui-rs method
    /// directly — that raw path is what let the nesting bug back in once
    /// already. `mod.rs` keeps the sole exemption: the wrapper's own body.
    #[test]
    fn no_raw_get_window_draw_list_outside_the_one_wrapper() {
        let mut offenders = Vec::new();
        for (file, src) in UI_FILES {
            if *file == "mod.rs" {
                continue;
            }
            if production(src).contains("get_window_draw_list(") {
                offenders.push(*file);
            }
        }
        assert!(
            offenders.is_empty(),
            "raw ui.get_window_draw_list() outside crate::ui::window_draw_list in: {offenders:?}"
        );
    }
}
