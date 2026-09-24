//! Themeable overlay tokens. Dark stone + a warm accent — GW2, not a dashboard.
//! The active [`Palette`] is derived from 5 base colors (preset or custom) via
//! [`apply_theme`]; chrome reads it through [`pal`]. Semantic colors (status,
//! profession, pips) stay as consts below — themes never touch meaning.
//! Hold the return value of [`push`] for the whole window frame or styles pop.

use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use nexus::imgui::{DrawListMut, StyleColor, StyleVar, TextureId, Ui};

use super::color_u32;

// Semantic colors (NOT themeable — they carry meaning, not chrome)
pub const CURRENT: [f32; 4] = [0.62, 0.82, 1.0, 1.0];
pub const OPTIMIZED: [f32; 4] = [0.55, 0.92, 0.62, 1.0];
pub const HEAL_RIM: [f32; 4] = [0.42, 0.78, 0.48, 0.95];
pub const ELITE_RIM: [f32; 4] = [0.95, 0.62, 0.22, 0.95];
pub const ERR: [f32; 4] = [1.0, 0.38, 0.28, 1.0];
pub const WARN: [f32; 4] = [1.0, 0.72, 0.28, 1.0];
/// Same as `FrameRounding` in [`push`] — icons match buttons/windows.
pub const ICON_ROUNDING: f32 = 5.0;
/// Gold tick on section headers. Title starts this far after the tick.
pub const HEADER_ACCENT_W: f32 = 3.0;
pub const HEADER_TITLE_GAP: f32 = 10.0;

// Runtime theme palette

/// Every themeable chrome slot, resolved to RGBA. Themes replace RGB only —
/// each slot keeps the alpha the shipped Tyrian Gold theme uses.
///
/// Field names mirror the historical const names (`gold` is "the accent",
/// whatever hue the active theme gives it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Window background (old `INK`).
    pub ink: [f32; 4],
    /// Child pane background (old `CHILD`).
    pub child: [f32; 4],
    /// Primary text (old `CREAM`).
    pub cream: [f32; 4],
    /// Secondary text (old `MUTED`).
    pub muted: [f32; 4],
    /// Accent (old `GOLD`).
    pub gold: [f32; 4],
    /// Dim accent: borders, separators, scrollbar (old `GOLD_DIM`).
    pub gold_dim: [f32; 4],
    /// Filled accent: selected pills, gauges, gold buttons (old `GOLD_FILL`).
    pub gold_fill: [f32; 4],
    /// Accent-tinted dark hover fill (old `GOLD_HOVER`).
    pub gold_hover: [f32; 4],
    /// Card plate (old `PLATE`).
    pub plate: [f32; 4],
    /// Empty slot plate (old `PLATE_EMPTY`).
    pub plate_empty: [f32; 4],
    // Derived slots consumed by `push()` / `gold_button*` / chips.
    pub popup_bg: [f32; 4],
    pub frame_bg: [f32; 4],
    pub frame_bg_hovered: [f32; 4],
    pub frame_bg_active: [f32; 4],
    pub button: [f32; 4],
    pub button_hovered: [f32; 4],
    pub button_active: [f32; 4],
    pub header: [f32; 4],
    pub header_hovered: [f32; 4],
    pub title_bg: [f32; 4],
    pub title_bg_active: [f32; 4],
    pub gold_button_hovered: [f32; 4],
    pub gold_button_active: [f32; 4],
    /// Near-black text on accent fills. Constant across themes — the WCAG
    /// gate on every preset keeps it legible on any accent.
    pub gold_button_text: [f32; 4],
    /// Idle (unselected, unhovered) pill/chip fill.
    pub chip_idle_fill: [f32; 4],
    /// Idle pill/chip rim.
    pub chip_idle_rim: [f32; 4],
    /// Section-header plate behind the gold tick (see [`header`]).
    pub header_plate: [f32; 4],
}

/// The shipped Tyrian Gold palette, byte-for-byte the pre-theme colors so the
/// default theme is pixel-identical to what always shipped. `derive_palette`
/// on the tyrian bases lands close (see tests) but not exact, hence stored.
const TYRIAN: Palette = Palette {
    ink: [0.07, 0.06, 0.045, 0.96],
    child: [0.09, 0.08, 0.055, 0.35],
    cream: [0.93, 0.90, 0.82, 1.0],
    muted: [0.58, 0.54, 0.46, 1.0],
    gold: [1.0, 0.84, 0.38, 1.0],
    gold_dim: [0.55, 0.44, 0.18, 0.85],
    gold_fill: [0.78, 0.62, 0.22, 0.95],
    gold_hover: [0.22, 0.18, 0.08, 0.95],
    plate: [0.12, 0.10, 0.07, 0.94],
    plate_empty: [0.08, 0.07, 0.055, 0.75],
    popup_bg: [0.08, 0.07, 0.05, 0.98],
    frame_bg: [0.12, 0.10, 0.07, 0.95],
    frame_bg_hovered: [0.18, 0.15, 0.09, 1.0],
    frame_bg_active: [0.22, 0.18, 0.08, 1.0],
    button: [0.32, 0.25, 0.08, 0.95],
    button_hovered: [0.46, 0.36, 0.10, 1.0],
    button_active: [0.58, 0.45, 0.12, 1.0],
    header: [0.22, 0.18, 0.08, 0.9],
    header_hovered: [0.30, 0.24, 0.10, 1.0],
    title_bg: [0.10, 0.08, 0.05, 1.0],
    title_bg_active: [0.16, 0.12, 0.06, 1.0],
    gold_button_hovered: [0.90, 0.74, 0.28, 1.0],
    gold_button_active: [0.70, 0.55, 0.16, 1.0],
    gold_button_text: [0.10, 0.08, 0.04, 1.0],
    chip_idle_fill: [0.10, 0.09, 0.06, 0.55],
    chip_idle_rim: [0.32, 0.26, 0.12, 0.55],
    header_plate: [0.15, 0.13, 0.08, 0.9],
};

/// One built-in theme: id (stable, stored in config), English display name,
/// and the 5 base colors the full palette derives from.
struct Preset {
    id: &'static str,
    name: &'static str,
    bg: [f32; 3],
    panel: [f32; 3],
    accent: [f32; 3],
    text: [f32; 3],
    muted: [f32; 3],
}

const PRESETS: &[Preset] = &[
    Preset {
        id: "tyrian-gold",
        name: "Tyrian Gold",
        bg: [0.07, 0.06, 0.045],
        panel: [0.12, 0.10, 0.07],
        accent: [1.0, 0.84, 0.38],
        text: [0.93, 0.90, 0.82],
        muted: [0.58, 0.54, 0.46],
    },
    Preset {
        id: "glacial-ward",
        name: "Glacial Ward",
        bg: [0.045, 0.06, 0.08],
        panel: [0.075, 0.10, 0.13],
        accent: [0.40, 0.76, 1.0],
        text: [0.84, 0.90, 0.95],
        muted: [0.47, 0.55, 0.62],
    },
    Preset {
        id: "verdant-wilds",
        name: "Verdant Wilds",
        bg: [0.045, 0.07, 0.05],
        panel: [0.075, 0.115, 0.082],
        accent: [0.42, 0.85, 0.45],
        text: [0.85, 0.93, 0.86],
        muted: [0.47, 0.57, 0.49],
    },
    Preset {
        id: "molten-ember",
        name: "Molten Ember",
        bg: [0.08, 0.055, 0.045],
        panel: [0.13, 0.09, 0.07],
        accent: [1.0, 0.58, 0.32],
        text: [0.95, 0.89, 0.84],
        muted: [0.62, 0.53, 0.47],
    },
    Preset {
        id: "void-orchid",
        name: "Void Orchid",
        bg: [0.065, 0.05, 0.085],
        panel: [0.105, 0.082, 0.135],
        accent: [0.80, 0.58, 1.0],
        text: [0.92, 0.88, 0.95],
        muted: [0.57, 0.51, 0.63],
    },
];

/// The five base colors of a built-in preset, in `CustomTheme` field order
/// (bg, panel, accent, text, muted). `None` for `"custom"` or an unknown id.
/// Used to seed a fresh custom theme from whatever preset is on screen.
pub fn preset_bases(id: &str) -> Option<[[f32; 3]; 5]> {
    PRESETS
        .iter()
        .find(|p| p.id == id)
        .map(|p| [p.bg, p.panel, p.accent, p.text, p.muted])
}

/// `(id, English display name)` for every built-in preset, Settings-combo order.
pub fn preset_ids() -> &'static [(&'static str, &'static str)] {
    static IDS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    IDS.get_or_init(|| PRESETS.iter().map(|p| (p.id, p.name)).collect())
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// RGB (0..=1) to HSV, h in degrees.
fn rgb_to_hsv([r, g, b]: [f32; 3]) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max <= 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let hp = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r1 + m, g1 + m, b1 + m]
}

fn rgba(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

/// Same RGB, caller-chosen alpha — for chrome that reuses a palette slot at a
/// different opacity than the slot ships with.
pub fn with_alpha(c: [f32; 4], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

/// Derive the full palette from the 5 theme bases.
///
/// Accent family scales the accent's HSV value; bg-family lifts lerp toward
/// the accent in RGB so the hue tint stays subtle. Alphas are fixed per slot
/// (the shipped Tyrian alphas) — themes replace RGB only. Lerp fractions stay
/// at or under 0.28 toward the accent, which keeps `text` readable on every
/// hover/header fill for any preset passing the WCAG gate in the tests.
pub fn derive_palette(
    bg: [f32; 3],
    panel: [f32; 3],
    accent: [f32; 3],
    text: [f32; 3],
    muted: [f32; 3],
) -> Palette {
    let (ah, a_s, av) = rgb_to_hsv(accent);
    let (bh, bs, bv) = rgb_to_hsv(bg);
    let accent_v = |f: f32| hsv_to_rgb(ah, a_s, av * f);
    Palette {
        ink: rgba(bg, 0.96),
        child: rgba(lerp3(bg, [1.0, 1.0, 1.0], 0.035), 0.35),
        cream: rgba(text, 1.0),
        muted: rgba(muted, 1.0),
        gold: rgba(accent, 1.0),
        gold_dim: rgba(hsv_to_rgb(ah, a_s * 0.92, av * 0.55), 0.85),
        gold_fill: rgba(accent_v(0.80), 0.95),
        gold_hover: rgba(lerp3(bg, accent, 0.16), 0.95),
        plate: rgba(panel, 0.94),
        plate_empty: rgba(lerp3(bg, panel, 0.2), 0.75),
        popup_bg: rgba(hsv_to_rgb(bh, bs, bv * 0.92), 0.98),
        frame_bg: rgba(panel, 0.95),
        frame_bg_hovered: rgba(lerp3(panel, accent, 0.12), 1.0),
        frame_bg_active: rgba(lerp3(panel, accent, 0.22), 1.0),
        button: rgba(accent_v(0.30), 0.95),
        button_hovered: rgba(accent_v(0.46), 1.0),
        button_active: rgba(accent_v(0.58), 1.0),
        header: rgba(lerp3(bg, accent, 0.10), 0.9),
        header_hovered: rgba(lerp3(bg, accent, 0.18), 1.0),
        title_bg: rgba(bg, 1.0),
        title_bg_active: rgba(panel, 1.0),
        gold_button_hovered: rgba(accent_v(0.90), 1.0),
        gold_button_active: rgba(accent_v(0.70), 1.0),
        gold_button_text: [0.10, 0.08, 0.04, 1.0],
        chip_idle_fill: rgba(lerp3(bg, accent, 0.035), 0.55),
        chip_idle_rim: rgba(lerp3(bg, accent, 0.28), 0.55),
        header_plate: rgba(lerp3(bg, accent, 0.09), 0.9),
    }
}

/// Active palette. Render is effectively single-threaded; a plain read lock
/// per [`pal`] call is cheap and uncontended.
static ACTIVE: RwLock<Palette> = RwLock::new(TYRIAN);

/// The window font scale `render_main` pushed for this frame, i.e. the
/// player's Settings font-scale slider. Same single-threaded-render argument
/// as [`ACTIVE`].
static UI_SCALE: RwLock<f32> = RwLock::new(1.0);

/// Record the frame's base font scale. Called once by `render_main`, right
/// after it pushes the scale onto the window.
pub fn set_ui_scale(scale: f32) {
    *UI_SCALE.write().unwrap_or_else(|e| e.into_inner()) = scale;
}

/// The player's font scale, for code that needs to size against it.
pub fn ui_scale() -> f32 {
    *UI_SCALE.read().unwrap_or_else(|e| e.into_inner())
}

/// Push a font scale RELATIVE to the player's UI scale, and pair it with
/// [`font_scale_reset`].
///
/// Always use these two instead of `ui.set_window_font_scale` directly.
/// `render_main` sets the window font scale to `config.font_scale`, so a
/// section that pushed `0.85` and restored a literal `1.0` did not restore
/// anything — it silently un-scaled every section rendered after it for any
/// player not on 1.0. Going through here makes the restore impossible to get
/// wrong, because it does not take a number.
///
/// Nested `ChildWindow`s start at 1.0 regardless of the parent, so a child
/// that wants the player's scale must call one of these too.
pub fn font_scale(ui: &Ui, factor: f32) {
    ui.set_window_font_scale(ui_scale() * factor);
}

/// Restore the player's UI scale after [`font_scale`].
pub fn font_scale_reset(ui: &Ui) {
    ui.set_window_font_scale(ui_scale());
}

/// Snapshot of the active palette. Take it once per function, not per color.
pub fn pal() -> Palette {
    *ACTIVE.read().unwrap_or_else(|e| e.into_inner())
}

/// Rebuild the active palette from config. Cheap — call on every change.
/// `"tyrian-gold"` uses the exact shipped colors; other preset ids derive
/// from their bases (the default `"glacial-ward"` among them); `"custom"`
/// derives from `cfg.custom`; anything unknown falls back to Tyrian Gold.
pub fn apply_theme(cfg: &gw2_core::config::ThemeConfig) {
    let palette = match cfg.preset.as_str() {
        "tyrian-gold" => TYRIAN,
        "custom" => {
            // A hand-edited config can hold out-of-range values; clamp so
            // rendering never sees an out-of-gamut palette.
            let clamp3 = |c: [f32; 3]| c.map(|v| v.clamp(0.0, 1.0));
            let c = &cfg.custom;
            derive_palette(
                clamp3(c.bg),
                clamp3(c.panel),
                clamp3(c.accent),
                clamp3(c.text),
                clamp3(c.muted),
            )
        }
        id => PRESETS
            .iter()
            .find(|p| p.id == id)
            .map(|p| derive_palette(p.bg, p.panel, p.accent, p.text, p.muted))
            .unwrap_or(TYRIAN),
    };
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = palette;
}

pub fn header_title_x(left: f32) -> f32 {
    left + HEADER_ACCENT_W + HEADER_TITLE_GAP
}

/// A slot the suggestion moved away from what the player is wearing.
///
/// Deliberately the same shape as the gold lock ring: one visual language,
/// two meanings - gold says you pinned it, green says Choya changed it. It is
/// brighter than [`HEAL_RIM`], which is also green and already sits on the
/// heal slot, so the two never read as the same mark.
pub const CHANGED: [f32; 4] = [0.30, 1.0, 0.50, 1.0];

/// Halo just outside a rectangular slot or icon.
pub fn paint_changed_rect(draw: &DrawListMut, min: [f32; 2], max: [f32; 2], rounding: f32) {
    draw.add_rect(
        [min[0] - 2.0, min[1] - 2.0],
        [max[0] + 2.0, max[1] + 2.0],
        with_alpha(CHANGED, 0.95),
    )
    .thickness(2.0)
    .rounding(rounding + 2.0)
    .build();
}

/// Halo just outside a circular trait node.
pub fn paint_changed_circle(draw: &DrawListMut, center: [f32; 2], radius: f32) {
    draw.add_circle(center, radius + 3.0, with_alpha(CHANGED, 0.95))
        .thickness(2.0)
        .build();
}

pub fn paint_header_accent(draw: &DrawListMut, left: f32, top: f32, height: f32) {
    draw.add_rect(
        [left, top],
        [left + HEADER_ACCENT_W, top + height],
        pal().gold,
    )
    .filled(true)
    .rounding(2.0)
    .build();
}

fn fade(c: [f32; 4], opacity: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * opacity.clamp(0.3, 1.0)]
}

/// Push overlay colors/rounding. Keep the value alive for the window frame.
pub fn push<'ui>(ui: &'ui Ui<'_>, opacity: f32) -> impl Sized + 'ui {
    let a = opacity.clamp(0.3, 1.0);
    let p = pal();
    (
        ui.push_style_var(StyleVar::WindowRounding(8.0)),
        ui.push_style_var(StyleVar::ChildRounding(6.0)),
        ui.push_style_var(StyleVar::FrameRounding(5.0)),
        ui.push_style_var(StyleVar::GrabRounding(4.0)),
        ui.push_style_var(StyleVar::PopupRounding(6.0)),
        ui.push_style_var(StyleVar::WindowBorderSize(1.0)),
        ui.push_style_var(StyleVar::WindowPadding([24.0, 16.0])),
        ui.push_style_color(StyleColor::WindowBg, fade(p.ink, a)),
        ui.push_style_color(StyleColor::ChildBg, fade(p.child, a)),
        ui.push_style_color(StyleColor::PopupBg, fade(p.popup_bg, a)),
        ui.push_style_color(StyleColor::Border, p.gold_dim),
        ui.push_style_color(StyleColor::FrameBg, p.frame_bg),
        ui.push_style_color(StyleColor::FrameBgHovered, p.frame_bg_hovered),
        ui.push_style_color(StyleColor::FrameBgActive, p.frame_bg_active),
        ui.push_style_color(StyleColor::Button, p.button),
        ui.push_style_color(StyleColor::ButtonHovered, p.button_hovered),
        ui.push_style_color(StyleColor::ButtonActive, p.button_active),
        ui.push_style_color(StyleColor::Header, p.header),
        ui.push_style_color(StyleColor::HeaderHovered, p.header_hovered),
        ui.push_style_color(StyleColor::TitleBg, fade(p.title_bg, a)),
        ui.push_style_color(StyleColor::TitleBgActive, fade(p.title_bg_active, a)),
        ui.push_style_color(StyleColor::Separator, p.gold_dim),
        ui.push_style_color(StyleColor::CheckMark, p.gold),
        ui.push_style_color(StyleColor::SliderGrab, p.gold_fill),
        ui.push_style_color(StyleColor::Text, p.cream),
        ui.push_style_color(StyleColor::TextDisabled, p.muted),
        ui.push_style_color(StyleColor::ScrollbarGrab, p.gold_dim),
    )
}

/// Shared frame pad so InputText and gold buttons are the same height.
pub fn control_pad(ui: &Ui) -> [f32; 2] {
    let s = (ui.current_font_size() / 13.0).max(0.75);
    [6.0 * s, 4.0 * s]
}

/// Font size + pad.y*2 — use this for sized buttons next to InputText.
pub fn control_height(ui: &Ui) -> f32 {
    let p = control_pad(ui);
    (ui.current_font_size() + p[1] * 2.0).round()
}

/// Gold plate button (Copy, Send, Test). Dark text on gold so it reads as the action.
pub fn gold_button(ui: &Ui, label: impl AsRef<str>) -> bool {
    let p = pal();
    let _pad = push_gold_button_pad(ui);
    let _bg = ui.push_style_color(StyleColor::Button, p.gold_fill);
    let _h = ui.push_style_color(StyleColor::ButtonHovered, p.gold_button_hovered);
    let _a = ui.push_style_color(StyleColor::ButtonActive, p.gold_button_active);
    let _t = ui.push_style_color(StyleColor::Text, p.gold_button_text);
    ui.button(label.as_ref())
}

pub fn gold_button_sized(ui: &Ui, label: impl AsRef<str>, size: [f32; 2]) -> bool {
    let label = label.as_ref();
    let visible = label.split("##").next().unwrap_or(label);
    let need_w = gold_button_width(ui, visible);
    let w = if size[0] < 0.0 {
        size[0]
    } else if size[0] <= 0.0 {
        need_w
    } else {
        size[0].max(need_w)
    };
    let h = if size[1] <= 0.0 {
        control_height(ui)
    } else {
        size[1]
    };
    let p = pal();
    let _pad = push_gold_button_pad(ui);
    let _bg = ui.push_style_color(StyleColor::Button, p.gold_fill);
    let _h = ui.push_style_color(StyleColor::ButtonHovered, p.gold_button_hovered);
    let _a = ui.push_style_color(StyleColor::ButtonActive, p.gold_button_active);
    let _t = ui.push_style_color(StyleColor::Text, p.gold_button_text);
    ui.button_with_size(label, [w, h])
}

fn gold_button_pad(ui: &Ui) -> (f32, f32) {
    let p = control_pad(ui);
    let extra_x = 4.0 * (ui.current_font_size() / 13.0).max(0.75);
    (p[0] + extra_x, p[1])
}

/// Width a gold button needs so its label is not clipped at the current font.
pub fn gold_button_width(ui: &Ui, label: impl AsRef<str>) -> f32 {
    let label = label.as_ref();
    let visible = label.split("##").next().unwrap_or(label);
    let (pad_x, _) = gold_button_pad(ui);
    ui.calc_text_size(visible)[0] + pad_x * 2.0
}

/// Combo frame width that fits `preview` plus the arrow, at the current font.
pub fn combo_width_for(ui: &Ui, preview: &str) -> f32 {
    let text_w = ui.calc_text_size(preview)[0];
    let arrow = ui.frame_height();
    let pad_x = control_pad(ui)[0];
    (text_w + pad_x * 2.0 + arrow + 2.0).ceil()
}

fn clip_to_width(ui: &Ui, text: &str, max_w: f32) -> String {
    if max_w <= 4.0 {
        return String::new();
    }
    if ui.calc_text_size(text)[0] <= max_w {
        return text.to_string();
    }
    let mut s = String::new();
    for ch in text.chars() {
        let mut probe = s.clone();
        probe.push(ch);
        probe.push_str("...");
        if ui.calc_text_size(&probe)[0] > max_w {
            break;
        }
        s.push(ch);
    }
    s.push_str("...");
    s
}

/// ImGui Combo previews are left-aligned. Draw `preview` centered in the value area.
pub fn paint_centered_combo_preview(ui: &Ui, preview: &str, origin: [f32; 2], width: f32) {
    if preview.is_empty() || width <= 0.0 {
        return;
    }
    let h = ui.frame_height();
    let pad = control_pad(ui)[0];
    let inner_w = (width - h - pad * 2.0).max(4.0);
    let shown = clip_to_width(ui, preview, inner_w);
    let sz = ui.calc_text_size(&shown);
    let tx = origin[0] + pad + (inner_w - sz[0]) * 0.5;
    let ty = origin[1] + (h - sz[1]) * 0.5;
    crate::ui::window_draw_list(ui).add_text([tx, ty], color_u32(pal().cream), &shown);
}

fn push_gold_button_pad<'ui>(ui: &'ui Ui<'_>) -> impl Sized + 'ui {
    let (pad_x, pad_y) = gold_button_pad(ui);
    ui.push_style_var(StyleVar::FramePadding([pad_x, pad_y]))
}

/// Rounded pill tab. `id` must be unique (used as an invisible button id).
pub fn pill(ui: &Ui, label: &str, selected: bool, id: &str) -> bool {
    pill_pulse(ui, label, selected, id, 0.0)
}

/// `pulse` 0..=1 gold blink for a waiting result tab.
pub fn pill_pulse(ui: &Ui, label: &str, selected: bool, id: &str, pulse: f32) -> bool {
    let pad_x = 10.0;
    let pad_y = 3.0;
    let sz = ui.calc_text_size(label);
    let w = (sz[0] + pad_x * 2.0).max(36.0);
    let h = sz[1] + pad_y * 2.0;
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hovered = ui.is_item_hovered();
    let pulse = pulse.clamp(0.0, 1.0);
    let th = pal();
    let mut fill = if selected {
        th.gold_fill
    } else if hovered {
        th.gold_hover
    } else {
        th.chip_idle_fill
    };
    let mut rim = if selected {
        th.gold
    } else if hovered {
        th.gold_dim
    } else {
        th.chip_idle_rim
    };
    if pulse > 0.0 && !selected {
        fill = [
            fill[0] + (th.gold_fill[0] - fill[0]) * pulse,
            fill[1] + (th.gold_fill[1] - fill[1]) * pulse,
            fill[2] + (th.gold_fill[2] - fill[2]) * pulse,
            0.55 + 0.40 * pulse,
        ];
        rim = [th.gold[0], th.gold[1], th.gold[2], 0.45 + 0.55 * pulse];
    }
    let text = if selected {
        th.gold_button_text
    } else if pulse > 0.0 {
        let d = th.gold_button_text;
        [
            th.cream[0] + (d[0] - th.cream[0]) * pulse,
            th.cream[1] + (d[1] - th.cream[1]) * pulse,
            th.cream[2] + (d[2] - th.cream[2]) * pulse,
            1.0,
        ]
    } else {
        th.cream
    };
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], fill)
            .filled(true)
            .rounding(h * 0.45)
            .build();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], rim)
            .rounding(h * 0.45)
            .build();
        dl.add_text([p[0] + pad_x, p[1] + pad_y], color_u32(text), label);
    }
    clicked
}

/// Colours for one comparison tab of a kind: current (blue), optimized
/// (green) or published (the site's colour). Fill is the idle chip fill
/// tinted a third of the way toward the kind; the selected tab is the kind
/// colour itself with the dark button text every accent passes the WCAG gate
/// with.
pub struct TabTint {
    pub fill: [f32; 4],
    pub rim: [f32; 4],
    pub selected_fill: [f32; 4],
    pub text: [f32; 4],
    pub selected_text: [f32; 4],
}

pub fn tab_tint(kind: [f32; 4]) -> TabTint {
    tab_tint_in(&pal(), kind)
}

/// Perceived brightness, 0..1 (Rec. 601 weights on gamma-encoded values;
/// close enough to pick a direction to tint in).
fn brightness(c: [f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

/// A kind colour (a site's brand, or CURRENT / OPTIMIZED) adapted to the
/// theme: on a dark ground a dark brand is lifted toward the text colour,
/// on a light ground a bright brand is pulled toward the dark button text,
/// so the tint reads as that brand without vanishing into the panel.
pub fn adapt_kind(p: &Palette, kind: [f32; 4]) -> [f32; 4] {
    let k = [kind[0], kind[1], kind[2]];
    let ground = brightness([p.ink[0], p.ink[1], p.ink[2]]);
    let kb = brightness(k);
    // Brightness is scaled, not lerped to a neutral: pulling two blues toward
    // the same grey made them one colour (custom-white, Current vs Snowcrows).
    let scaled = |target: f32| {
        let s = target / kb.max(0.01);
        [
            (k[0] * s).min(1.0),
            (k[1] * s).min(1.0),
            (k[2] * s).min(1.0),
        ]
    };
    let out = if ground < 0.5 {
        // Dark theme: want the kind at least ~0.45 bright. A brand too dark
        // to scale up (near black) is lifted toward the text colour instead.
        if kb >= 0.45 {
            k
        } else {
            let up = scaled(0.45);
            let short = ((0.45 - brightness(up)) / 0.45).clamp(0.0, 1.0);
            lerp3(up, [p.cream[0], p.cream[1], p.cream[2]], short)
        }
    } else if kb > 0.55 {
        // Light theme: want the kind at most ~0.55 bright.
        scaled(0.55)
    } else {
        k
    };
    [out[0], out[1], out[2], kind[3]]
}

pub fn tab_tint_in(p: &Palette, kind: [f32; 4]) -> TabTint {
    let kind = adapt_kind(p, kind);
    let k = [kind[0], kind[1], kind[2]];
    let idle = [
        p.chip_idle_fill[0],
        p.chip_idle_fill[1],
        p.chip_idle_fill[2],
    ];
    let f = lerp3(idle, k, 0.35);
    TabTint {
        fill: [f[0], f[1], f[2], 0.40],
        rim: [k[0], k[1], k[2], 0.75],
        selected_fill: [k[0], k[1], k[2], 1.0],
        text: p.cream,
        selected_text: p.gold_button_text,
    }
}

/// [`pill`] in a kind's colours: the comparison strip's tabs.
pub fn tinted_pill(ui: &Ui, label: &str, selected: bool, id: &str, kind: [f32; 4]) -> bool {
    let pad_x = 10.0;
    let pad_y = 3.0;
    let sz = ui.calc_text_size(label);
    let w = (sz[0] + pad_x * 2.0).max(36.0);
    let h = sz[1] + pad_y * 2.0;
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hovered = ui.is_item_hovered();
    let tint = tab_tint(kind);
    let kind = adapt_kind(&pal(), kind);
    let fill = if selected {
        tint.selected_fill
    } else if hovered {
        let f = lerp3(
            [tint.fill[0], tint.fill[1], tint.fill[2]],
            [kind[0], kind[1], kind[2]],
            0.4,
        );
        [f[0], f[1], f[2], 0.9]
    } else {
        tint.fill
    };
    let text = if selected {
        tint.selected_text
    } else {
        tint.text
    };
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], fill)
            .filled(true)
            .rounding(h * 0.45)
            .build();
        // The selected tab is unmistakable: solid brand fill, a bright rim,
        // bold label and a bar underneath. Idle tabs are a faint wash of
        // their colour (in-game 2026-09-08: the two looked alike).
        let rim = if selected { pal().cream } else { tint.rim };
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], rim)
            .rounding(h * 0.45)
            .thickness(if selected { 2.0 } else { 1.0 })
            .build();
        dl.add_text([p[0] + pad_x, p[1] + pad_y], color_u32(text), label);
        if selected {
            dl.add_text([p[0] + pad_x + 1.0, p[1] + pad_y], color_u32(text), label);
            dl.add_rect(
                [p[0] + 4.0, p[1] + h + 2.0],
                [p[0] + w - 4.0, p[1] + h + 5.0],
                tint.selected_fill,
            )
            .filled(true)
            .rounding(1.5)
            .build();
        }
    }
    clicked
}

/// Width of [`switch`] for a given label, so a row can be laid out before it
/// is drawn.
pub fn switch_width(ui: &Ui, label: &str) -> f32 {
    let h = switch_height(ui);
    h * 1.9 + 6.0 + ui.calc_text_size(label)[0]
}

fn switch_height(ui: &Ui) -> f32 {
    (ui.current_font_size() * 0.85).round().max(12.0)
}

/// A sliding on/off switch: track, knob, label to its right.
///
/// ImGui has no such widget — it ships `checkbox` — so this is drawn the same
/// way [`pill`] is, an invisible button under two draw calls.
///
/// `anim` is the eased 0..1 the caller already keeps for this state, not a
/// second copy of it. The knob travels with it and the track colour lerps
/// along the same value, so the switch and whatever else the state drives
/// move together instead of one snapping while the other slides. Pass `on as
/// u8 as f32` to get an unanimated switch.
///
/// Returns true on the frame it is clicked.
pub fn switch(ui: &Ui, label: &str, anim: f32, id: &str) -> bool {
    let th = pal();
    let h = switch_height(ui);
    let track_w = h * 1.9;
    let gap = 6.0;
    let text = ui.calc_text_size(label);
    let p = ui.cursor_screen_pos();
    // The whole thing is the hit target, label included — a knob this size is
    // a small thing to ask someone to hit.
    let clicked = ui.invisible_button(id, [track_w + gap + text[0], h.max(text[1])]);
    let hovered = ui.is_item_hovered();
    let anim = anim.clamp(0.0, 1.0);

    // Vertically centred against the taller of knob and label, so the switch
    // lines up with text of any scale.
    let row_h = h.max(text[1]);
    let ty = p[1] + (row_h - h) * 0.5;
    let off = [
        th.chip_idle_fill[0],
        th.chip_idle_fill[1],
        th.chip_idle_fill[2],
    ];
    let on = [th.gold_fill[0], th.gold_fill[1], th.gold_fill[2]];
    let mix = |a: f32, b: f32| a + (b - a) * anim;
    let fill = [
        mix(off[0], on[0]),
        mix(off[1], on[1]),
        mix(off[2], on[2]),
        if hovered { 1.0 } else { 0.92 },
    ];
    let rim = if hovered { th.gold } else { th.chip_idle_rim };

    let dl = crate::ui::window_draw_list(ui);
    dl.add_rect([p[0], ty], [p[0] + track_w, ty + h], fill)
        .filled(true)
        .rounding(h * 0.5)
        .build();
    dl.add_rect([p[0], ty], [p[0] + track_w, ty + h], rim)
        .rounding(h * 0.5)
        .build();
    // Left at rest, right when on, and every point between while it moves.
    let r = h * 0.5 - 2.0;
    let cx = p[0] + r + 2.0 + (track_w - (r + 2.0) * 2.0) * anim;
    dl.add_circle([cx, ty + h * 0.5], r, th.cream)
        .filled(true)
        .build();
    dl.add_text(
        [p[0] + track_w + gap, p[1] + (row_h - text[1]) * 0.5],
        color_u32(th.cream),
        label,
    );
    clicked
}

pub const PIP_DAMAGE: [f32; 4] = [0.90, 0.32, 0.28, 1.0];
pub const PIP_FRONT: [f32; 4] = [0.80, 0.82, 0.86, 1.0];
pub const PIP_HEAL: [f32; 4] = [0.32, 0.78, 0.48, 1.0];
pub const PIP_CTRL: [f32; 4] = [0.95, 0.70, 0.22, 1.0];

pub fn select_chip_size(ui: &Ui, label: &str, pip: bool) -> [f32; 2] {
    let sz = ui.calc_text_size(label);
    let extra = if pip { 12.0 } else { 0.0 };
    [(sz[0] + 16.0 + extra).max(28.0), sz[1] + 6.0]
}

/// GW2Mists-style filter chip. `pip` is the role-family color on the left.
pub fn select_chip(ui: &Ui, label: &str, selected: bool, id: &str, pip: Option<[f32; 4]>) -> bool {
    let [w, h] = select_chip_size(ui, label, pip.is_some());
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hovered = ui.is_item_hovered();
    let th = pal();
    let fill = if selected {
        th.gold_fill
    } else if hovered {
        th.gold_hover
    } else {
        th.chip_idle_fill
    };
    let rim = if selected {
        th.gold
    } else if hovered {
        th.gold_dim
    } else {
        th.chip_idle_rim
    };
    let text = if selected {
        th.gold_button_text
    } else {
        th.cream
    };
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], fill)
            .filled(true)
            .rounding(h * 0.45)
            .build();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], rim)
            .rounding(h * 0.45)
            .build();
        let mut tx = p[0] + 8.0;
        if let Some(pip) = pip {
            let cy = p[1] + h * 0.5;
            dl.add_circle([tx + 3.5, cy], 3.5, pip).filled(true).build();
            tx += 12.0;
        }
        dl.add_text([tx, p[1] + 3.0], color_u32(text), label);
    }
    clicked
}

/// Place the next chip on this line, or wrap. `avail` is the row width captured
/// before any chips (content_region_avail shrinks after `same_line`).
pub fn wrap_chip(ui: &Ui, avail: f32, row_x: &mut f32, chip_w: f32, gap: f32) {
    if *row_x > 0.5 && *row_x + chip_w + gap > avail {
        *row_x = 0.0;
    } else if *row_x > 0.5 {
        ui.same_line_with_spacing(0.0, gap);
    }
    *row_x += chip_w + gap;
}

/// Width needed so every label in a segment row fits without clipping.
pub fn segment_row_min_width(ui: &Ui, labels: &[&str]) -> f32 {
    if labels.is_empty() {
        return 0.0;
    }
    let widest = labels
        .iter()
        .map(|l| ui.calc_text_size(l)[0])
        .fold(0.0_f32, f32::max);
    let n = labels.len() as f32;
    n * (widest + 16.0) + 3.0 * (n - 1.0)
}

/// Exclusive pills. Each pill is at least as wide as the longest label.
pub fn segment_row(ui: &Ui, labels: &[&str], selected: usize, id_prefix: &str) -> Option<usize> {
    let n = labels.len();
    if n == 0 {
        return None;
    }
    let avail = ui.content_region_avail()[0];
    let gap = 3.0;
    let th = ui.calc_text_size(labels[0])[1];
    let h = th + 8.0;
    let widest = labels
        .iter()
        .map(|l| ui.calc_text_size(l)[0])
        .fold(0.0_f32, f32::max);
    let fit = widest + 16.0;
    let share = ((avail - gap * (n as f32 - 1.0)) / n as f32).max(28.0);
    let w = share.max(fit);
    let pl = pal();
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            ui.same_line_with_spacing(0.0, gap);
        }
        let id = format!("{id_prefix}{i}");
        let p = ui.cursor_screen_pos();
        if ui.invisible_button(&id, [w, h]) {
            clicked = Some(i);
        }
        let hovered = ui.is_item_hovered();
        let on = i == selected;
        let fill = if on {
            pl.gold_fill
        } else if hovered {
            pl.gold_hover
        } else {
            pl.chip_idle_fill
        };
        let rim = if on {
            pl.gold
        } else if hovered {
            pl.gold_dim
        } else {
            pl.chip_idle_rim
        };
        let text = if on { pl.gold_button_text } else { pl.cream };
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], fill)
            .filled(true)
            .rounding(h * 0.45)
            .build();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], rim)
            .rounding(h * 0.45)
            .build();
        let tw = ui.calc_text_size(label)[0];
        dl.add_text(
            [p[0] + (w - tw) * 0.5, p[1] + (h - th) * 0.5],
            color_u32(text),
            label,
        );
    }
    clicked.filter(|&i| i != selected)
}

/// Screen-space right edge → ImGui window-local wrap X (`PushTextWrapPos`).
pub(crate) fn wrap_pos_local(right_x: f32, window_x: f32, scroll_x: f32) -> f32 {
    right_x - window_x + scroll_x
}

/// Wrap to the current content edge. `text_colored` never wraps, so it clips.
/// `PushTextWrapPos` is window-local — passing a screen X leaves wrap past the
/// clip rect and the line is cut at the pane edge (News / What's new / Settings).
pub fn wrapped(ui: &Ui, color: [f32; 4], text: &str) {
    let right = ui.cursor_screen_pos()[0] + ui.content_region_avail()[0].max(8.0);
    let local = wrap_pos_local(right, ui.window_pos()[0], ui.scroll_x());
    let wrap = ui.push_text_wrap_pos_with_pos(local);
    {
        let _c = ui.push_style_color(StyleColor::Text, color);
        ui.text_wrapped(text);
    }
    wrap.pop(ui);
}

/// Headings, nested lists, numbered lists, paragraphs. Overlay fonts have no bold.
pub fn prose(ui: &Ui, text: &str) {
    if text.is_empty() {
        return;
    }
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed_end = line.trim_end();
        if trimmed_end.trim().is_empty() {
            ui.dummy([0.0, 8.0]);
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let depth = indent / 2;
        let t = line.trim_start();
        if let Some((level, rest)) = heading(t) {
            heading_line(ui, level, rest);
            continue;
        }
        if t == "---" {
            ui.dummy([0.0, 6.0]);
            continue;
        }
        if let Some((mark, rest)) = list_mark(t) {
            let pad = 12.0 + depth as f32 * 16.0;
            ui.indent_by(pad);
            wrapped(ui, pal().cream, &format!("{mark} {rest}"));
            ui.unindent_by(pad);
            ui.dummy([0.0, 2.0]);
            continue;
        }
        if plain_section(t) && next_nonempty(&lines, i + 1).is_some_and(|n| list_mark(n).is_some())
        {
            heading_line(ui, 2, t);
            continue;
        }
        wrapped(ui, pal().cream, t);
        ui.dummy([0.0, 6.0]);
    }
}

fn heading_line(ui: &Ui, level: u8, rest: &str) {
    let before = match level {
        1 => 10.0,
        2 => 8.0,
        _ => 6.0,
    };
    ui.dummy([0.0, before]);
    wrapped(ui, pal().gold, rest);
    ui.dummy([0.0, 4.0]);
}

fn next_nonempty<'a>(lines: &'a [&str], from: usize) -> Option<&'a str> {
    lines
        .get(from..)?
        .iter()
        .map(|l| l.trim_start())
        .find(|l| !l.is_empty())
}

fn heading(line: &str) -> Option<(u8, &str)> {
    if let Some(rest) = line.strip_prefix("### ") {
        Some((3, rest))
    } else if let Some(rest) = line.strip_prefix("## ") {
        Some((2, rest))
    } else {
        line.strip_prefix("# ").map(|rest| (1, rest))
    }
}

fn plain_section(line: &str) -> bool {
    let n = line.chars().count();
    if !(2..=56).contains(&n) {
        return false;
    }
    !matches!(line.chars().last(), Some('.' | '!' | '?' | ',' | ';'))
}

fn list_mark(line: &str) -> Option<(String, &str)> {
    for p in ["- ", "* ", "o "] {
        if let Some(rest) = line.strip_prefix(p) {
            return Some(("•".into(), rest));
        }
    }
    if let Some(rest) = line.strip_prefix('•') {
        return Some(("•".into(), rest.strip_prefix(' ').unwrap_or(rest)));
    }
    let mut n = 0usize;
    let bytes = line.as_bytes();
    while n < bytes.len() && bytes[n].is_ascii_digit() {
        n += 1;
    }
    if n > 0 && bytes.get(n) == Some(&b'.') && bytes.get(n + 1) == Some(&b' ') {
        return Some((format!("{}.", &line[..n]), &line[n + 2..]));
    }
    None
}

/// Gold-tick section header — same plate as build cards, used on Setup/Settings too.
pub fn header(ui: &Ui, title: &str) {
    let start = ui.cursor_screen_pos();
    let width = ui.content_region_avail()[0];
    let p = pal();
    {
        let dl = super::window_draw_list(ui);
        dl.add_rect(
            [start[0] - 1.0, start[1]],
            [start[0] + width + 1.0, start[1] + 22.0],
            p.header_plate,
        )
        .filled(true)
        .rounding(5.0)
        .round_bot_left(false)
        .round_bot_right(false)
        .build();
        paint_header_accent(&dl, start[0], start[1], 22.0);
        let th = ui.calc_text_size(title)[1];
        let ty = start[1] + ((22.0 - th) * 0.5).round();
        dl.add_text([header_title_x(start[0]), ty], color_u32(p.gold), title);
    }
    ui.dummy([0.0, 24.0]);
}

/// Tooltips auto-size from content; wrap-width is ~0 until then, so
/// `text_wrapped` becomes a one-glyph column. Pin wrap at 360px.
pub fn wide_tooltip(ui: &Ui, body: impl FnOnce(&Ui)) {
    ui.tooltip(|| {
        let wrap = ui.push_text_wrap_pos_with_pos(360.0);
        body(ui);
        wrap.pop(ui);
    });
}

fn scribble_hash(x: i32, y: i32, salt: u32) -> u32 {
    let mut n = (x as u32).wrapping_mul(374761393) ^ (y as u32).wrapping_mul(668265263) ^ salt;
    n = (n ^ (n >> 13)).wrapping_mul(1274126177);
    n ^ (n >> 16)
}

fn checkpoint_fill(fraction: f32, t: f32) -> f32 {
    ((fraction - t + 0.06) / 0.14).clamp(0.0, 1.0)
}

fn hatch_span(cx: f32, cy: f32, r: f32, y: f32) -> Option<(f32, f32)> {
    let dy = y - cy;
    let t = r * r - dy * dy;
    if t <= 0.0 {
        return None;
    }
    let dx = t.sqrt();
    Some((cx - dx, cx + dx))
}

/// Doodle download bar: gold scribble fill, mystic-coin checkpoints on the
/// track, choya rocking the leading edge.
pub fn download_scribble(ui: &Ui, fraction: f32, caption: &str) {
    let fraction = fraction.clamp(0.0, 1.0);
    const SIDE: f32 = 28.0;
    let full = ui.content_region_avail()[0].max(120.0);
    let avail = (full - SIDE * 2.0).max(120.0);
    let bar_h = 22.0;
    let choya_h = bar_h * 3.0;
    let coin_r: f32 = 15.0;
    let chest_h: f32 = 80.0;
    let chest_w = chest_h * (308.0 / 256.0);
    let line_gap = 8.0;
    let title_h = ui.text_line_height() + 4.0;
    let headroom = choya_h - bar_h + 4.0;
    let coin_h = coin_r * 2.0;
    const CAP_SCALE: f32 = 1.45;
    const CAP_GAP: f32 = 28.0;
    const NOTE: &str = "Speed depends on GW2 servers, not this addon.";
    const NOTE_SCALE: f32 = 0.68;
    ui.set_window_font_scale(CAP_SCALE);
    let cap_sz = ui.calc_text_size(caption);
    ui.set_window_font_scale(NOTE_SCALE);
    let note_sz = ui.calc_text_size(NOTE);
    ui.set_window_font_scale(1.0);
    let total_h = title_h
        + headroom
        + bar_h
        + line_gap
        + 6.0
        + coin_h
        + CAP_GAP
        + cap_sz[1]
        + 3.0
        + note_sz[1]
        + 8.0;
    let origin = ui.cursor_screen_pos();
    let p = [origin[0] + SIDE, origin[1]];
    let _ = ui.invisible_button("##dl_scribble", [full, total_h]);
    let t = elapsed_ms() as f32 / 1000.0;
    let labels = ["Bronze", "Silver", "Gold", "GEM"];
    let end_half = ui.calc_text_size(labels[0])[0].max(ui.calc_text_size(labels[3])[0]) * 0.5 + 6.0;
    let theme = pal();
    {
        let dl = crate::ui::window_draw_list(ui);

        dl.add_text([p[0], p[1]], color_u32(theme.gold), "FETCHING TYRIA...");

        let bar_x = p[0];
        let bar_y = p[1] + title_h + headroom;
        let left_pad = (coin_r + 4.0).max(end_half);
        let track_gap = 14.0;
        let right_reserve = chest_w + track_gap + 10.0;
        let track_x0 = bar_x + left_pad;
        let span = (avail - left_pad - right_reserve).max(40.0);
        let track_x1 = track_x0 + span;
        let fill_x = track_x0 + span * fraction;
        let empty = theme.chip_idle_rim;
        let gold_scribble = with_alpha(theme.gold, 0.95);
        let gold_scribble_dim = with_alpha(theme.gold_fill, 0.85);

        let y0 = bar_y as i32;
        let y1 = (bar_y + bar_h) as i32;
        let x0 = track_x0 as i32;
        let x1 = track_x1 as i32;
        for y in (y0..y1).step_by(2) {
            for x in (x0..x1).step_by(3) {
                let h = scribble_hash(x, y, 0xA11CE);
                let jagged = fill_x + ((h % 11) as f32) - 5.0;
                let filled = (x as f32) < jagged;
                let color = if filled {
                    if h & 1 == 0 {
                        gold_scribble
                    } else {
                        gold_scribble_dim
                    }
                } else {
                    empty
                };
                let len = 4.0 + (h % 4) as f32;
                let slant = if h & 2 == 0 { 2.2 } else { -1.6 };
                dl.add_line(
                    [x as f32, y as f32 + ((h >> 3) % 3) as f32],
                    [x as f32 + len, y as f32 + slant],
                    color,
                )
                .thickness(1.15)
                .build();
            }
        }

        let line_y = bar_y + bar_h + line_gap;
        let mut prev = [track_x0, line_y];
        let mut x = track_x0 + 4.0;
        while x <= track_x1 {
            let wobble = ((x * 0.18 + t * 2.4).sin()) * 1.1;
            let cur = [x, line_y + wobble];
            dl.add_line(prev, cur, theme.gold_dim)
                .thickness(1.2)
                .build();
            prev = cur;
            x += 5.0;
        }

        let coins = [
            (0.0_f32, [0.78, 0.45, 0.18, 1.0]),
            (1.0 / 3.0, [0.78, 0.80, 0.84, 1.0]),
            (2.0 / 3.0, theme.gold),
        ];
        let mark_top = line_y + 6.0;
        let coin_bottom = mark_top + coin_h;
        let label_y = coin_bottom + 2.0;
        for (i, (mark, metal)) in coins.iter().enumerate() {
            let cx = track_x0 + span * mark;
            let fill = checkpoint_fill(fraction, *mark);
            draw_mystic_coin(&dl, [cx, mark_top + coin_r], coin_r, i, fill, *metal);
            let label = labels[i];
            let tw = ui.calc_text_size(label)[0];
            dl.add_text([cx - tw * 0.5, label_y], color_u32(theme.muted), label);
        }

        let chest_cx = track_x1 + track_gap + chest_w * 0.5;
        let chest_top = coin_bottom - chest_h;
        draw_gem_chest(&dl, [chest_cx, chest_top], chest_h);
        let gem_tw = ui.calc_text_size(labels[3])[0];
        dl.add_text(
            [chest_cx - gem_tw * 0.5, label_y],
            color_u32(theme.muted),
            labels[3],
        );

        let t_slow = t * 1.92;
        let sway = t_slow.sin();
        let hop = (t_slow * 0.7).sin().abs() * 6.0;
        let feet_x = (fill_x + sway * 6.0).clamp(track_x0, track_x1);
        draw_choya(
            ui,
            &dl,
            [feet_x, bar_y + bar_h - hop],
            choya_h,
            sway,
            track_x1,
        );

        ui.set_window_font_scale(CAP_SCALE);
        let cap_sz = ui.calc_text_size(caption);
        let cap_x = origin[0] + ((full - cap_sz[0]) * 0.5).max(0.0);
        let cap_y = label_y + CAP_GAP;
        dl.add_text([cap_x, cap_y], color_u32(theme.gold), caption);
        ui.set_window_font_scale(NOTE_SCALE);
        let note_sz = ui.calc_text_size(NOTE);
        let note_x = origin[0] + ((full - note_sz[0]) * 0.5).max(0.0);
        let note_y = cap_y + cap_sz[1] + 3.0;
        let faint = with_alpha(theme.muted, 0.42);
        dl.add_text([note_x, note_y], color_u32(faint), NOTE);
        ui.set_window_font_scale(1.0);
    }
}

fn draw_coin(dl: &DrawListMut, c: [f32; 2], r: f32, metal: [f32; 4], fill: f32) {
    dl.add_circle(c, r, [0.10, 0.08, 0.05, 0.9]).build();
    let y_top = c[1] + r - fill * 2.0 * r;
    let y0 = y_top.max(c[1] - r) as i32;
    let y1 = (c[1] + r) as i32;
    for y in y0..=y1 {
        if let Some((x0, x1)) = hatch_span(c[0], c[1], r - 0.8, y as f32) {
            dl.add_line([x0, y as f32], [x1, y as f32], metal)
                .thickness(1.0)
                .build();
        }
    }
    dl.add_circle(c, r, metal).build();
    dl.add_circle(c, r * 0.55, metal).build();
}

fn embedded_tex(key: &'static str, bytes: &'static [u8]) -> Option<TextureId> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nexus::texture::get_texture_or_create_from_memory(key, bytes)
    }))
    .ok()
    .flatten()
    .map(|t| t.id())
}

fn mystic_coin_tex(metal: usize) -> Option<TextureId> {
    let (key, bytes): (&str, &[u8]) = match metal {
        0 => (
            "GW2BO_MYSTIC_BRONZE2",
            include_bytes!("../../assets/mystic_coin_bronze.png"),
        ),
        1 => (
            "GW2BO_MYSTIC_SILVER2",
            include_bytes!("../../assets/mystic_coin_silver.png"),
        ),
        2 => (
            "GW2BO_MYSTIC_GOLD2",
            include_bytes!("../../assets/mystic_coin_gold.png"),
        ),
        _ => return None,
    };
    embedded_tex(key, bytes)
}

/// A community site's own mark, for the button that links to it.
///
/// Their favicon, shown beside a link to their page: it says where the URL
/// goes before you click it, which is the whole job. A site we have no mark
/// for returns None and the caller draws a coloured pip instead.
pub fn site_tex(site: &str) -> Option<TextureId> {
    let (key, bytes): (&str, &[u8]) = match site.to_lowercase().as_str() {
        "guildjen" => (
            "GW2BO_SITE_GUILDJEN",
            include_bytes!("../../assets/guildjen.png"),
        ),
        "hardstuck" => (
            "GW2BO_SITE_HARDSTUCK",
            include_bytes!("../../assets/hardstuck.png"),
        ),
        "snowcrows" => (
            "GW2BO_SITE_SNOWCROWS",
            include_bytes!("../../assets/snowcrows.png"),
        ),
        _ => return None,
    };
    embedded_tex(key, bytes)
}

/// Ko-fi logomark (creator kit), used only for the coffee tile and header button.
pub fn kofi_tex() -> Option<TextureId> {
    embedded_tex(
        "GW2BO_KOFI_CUP1",
        include_bytes!("../../assets/kofi_cup.png"),
    )
}

const CHOYA_SHEET_W: f32 = 1536.0;
const CHOYA_SHEET_H: f32 = 1024.0;
/// Pixel rect on `choya_animated.png`: x, y, w, h
const CHOYA_HERO: [f32; 4] = [42.0, 18.0, 456.0, 460.0];
const CHOYA_DANCE: [f32; 4] = [523.0, 98.0, 248.0, 277.0];
const CHOYA_THINK: [f32; 4] = [1101.0, 634.0, 117.0, 89.0];
const CHOYA_IDLE: [[f32; 4]; 9] = [
    [446.0, 417.0, 111.0, 119.0],
    [559.0, 415.0, 118.0, 122.0],
    [694.0, 418.0, 99.0, 118.0],
    [798.0, 421.0, 100.0, 115.0],
    [912.0, 421.0, 108.0, 115.0],
    [1030.0, 421.0, 104.0, 116.0],
    [1146.0, 424.0, 89.0, 115.0],
    [1263.0, 425.0, 106.0, 116.0],
    [1387.0, 417.0, 106.0, 123.0],
];

/// `choya_animated_02.png` — same atlas size, isolated props + portraits.
const CHOYA2_HERO: [f32; 4] = [8.0, 7.0, 286.0, 269.0];

/// Peek-behind-rocks on sheet 2 (thinking). Sleep+Zzz is ~871,626 — do not reuse that rect.
const CHOYA2_PEEK: [f32; 4] = [1312.0, 570.0, 216.0, 189.0];
const CHOYA2_PARTY: [f32; 4] = [1198.0, 297.0, 261.0, 259.0];
const CHOYA2_WALK: [[f32; 4]; 6] = [
    [5.0, 569.0, 122.0, 170.0],
    [138.0, 572.0, 125.0, 164.0],
    [270.0, 570.0, 139.0, 170.0],
    [414.0, 570.0, 142.0, 164.0],
    [567.0, 571.0, 147.0, 164.0],
    [707.0, 573.0, 154.0, 166.0],
];
const CHOYA2_FACE: [[f32; 4]; 5] = [
    [4.0, 784.0, 150.0, 190.0],
    [162.0, 792.0, 145.0, 178.0],
    [312.0, 784.0, 148.0, 184.0],
    [469.0, 797.0, 156.0, 176.0],
    [635.0, 790.0, 155.0, 183.0],
];
const CHOYA2_SOMBRERO: [f32; 4] = [1198.0, 799.0, 180.0, 108.0];
const CHOYA2_SHADES: [f32; 4] = [1378.0, 804.0, 138.0, 51.0];
const CHOYA2_MARACA: [f32; 4] = [842.0, 814.0, 68.0, 138.0];
const CHOYA2_MARACA2: [f32; 4] = [801.0, 897.0, 93.0, 110.0];
const CHOYA2_NOTE: [f32; 4] = [1145.0, 874.0, 32.0, 40.0];
const CHOYA2_HEART: [f32; 4] = [964.0, 944.0, 43.0, 44.0];
const CHOYA2_LEI: [f32; 4] = [1396.0, 863.0, 121.0, 65.0];
/// Belly-sleep + Zzz on sheet 1 (idle composer / header).
const CHOYA_SLEEP: [f32; 4] = [1180.0, 700.0, 311.0, 265.0];

/// Isolated belly-sleep + Zzz on sheet 2. Tight crop — the sheet-1 rect packed several icons.
const CHOYA2_SLEEP: [f32; 4] = [871.0, 630.0, 206.0, 128.0];

/// Process-start instant. Wall-clock, monotonic — never the system clock, so
/// animations stay correct across NTP adjustments and DST.
static ANIM_START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the addon started rendering. The single time source for
/// every animation below, so a sprite advances at the same real-world speed
/// on a 60 Hz and a 240 Hz monitor alike (`ui.frame_count()` does not).
pub(crate) fn elapsed_ms() -> u64 {
    ANIM_START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Pure core of [`anim_cell`], split out so tests can drive it with
/// simulated elapsed times instead of real wall-clock time.
fn anim_cell_at(elapsed_ms: u64, cell_ms: u64, cells: usize) -> usize {
    if cells == 0 {
        return 0;
    }
    ((elapsed_ms / cell_ms.max(1)) as usize) % cells
}

/// Cycle through `cells` animation frames, `cell_ms` wall-clock milliseconds
/// each. Replaces `frame_count() / n` so pacing is framerate-independent.
pub fn anim_cell(cell_ms: u64, cells: usize) -> usize {
    anim_cell_at(elapsed_ms(), cell_ms, cells)
}

/// Pure core of [`anim_pulse`]; see [`anim_cell_at`].
fn anim_pulse_at(elapsed_ms: u64) -> f32 {
    (elapsed_ms as f32 / 1000.0 * 1.05).sin().abs()
}

/// `abs(sin(t))` pulse, `t` in wall-clock seconds. Replaces
/// `frame_count() as f32 * 0.0175` (0.0175 rad/frame at 60 fps = 1.05 rad/s).
pub fn anim_pulse() -> f32 {
    anim_pulse_at(elapsed_ms())
}

/// Radians accumulated at `rad_per_sec` since start. Replaces
/// `frame_count() as f32 * k` (`rad_per_sec == k * 60.0`, the 60 fps the old
/// per-frame constants were tuned against).
fn anim_phase_at(elapsed_ms: u64, rad_per_sec: f32) -> f32 {
    elapsed_ms as f32 / 1000.0 * rad_per_sec
}

pub fn anim_phase(rad_per_sec: f32) -> f32 {
    anim_phase_at(elapsed_ms(), rad_per_sec)
}

fn choya_sheet() -> Option<TextureId> {
    embedded_tex(
        "GW2BO_CHOYA_SHEET",
        include_bytes!("../../assets/choya_animated.png"),
    )
}

fn choya_sheet2() -> Option<TextureId> {
    embedded_tex(
        "GW2BO_CHOYA_SHEET2",
        include_bytes!("../../assets/choya_animated_02.png"),
    )
}

fn sheet_uv(frame: [f32; 4]) -> ([f32; 2], [f32; 2]) {
    let [x, y, w, h] = frame;
    let inset = 0.5;
    let u0 = ((x + inset) / CHOYA_SHEET_W).clamp(0.0, 1.0);
    let v0 = ((y + inset) / CHOYA_SHEET_H).clamp(0.0, 1.0);
    let u1 = ((x + w - inset) / CHOYA_SHEET_W).clamp(0.0, 1.0);
    let v1 = ((y + h - inset) / CHOYA_SHEET_H).clamp(0.0, 1.0);
    (
        [u0, v0],
        [u1.max(u0 + f32::EPSILON), v1.max(v0 + f32::EPSILON)],
    )
}

fn blit_choya_frame(dl: &DrawListMut, center: [f32; 2], size: f32, frame: [f32; 4]) {
    let Some(tid) = choya_sheet() else {
        return;
    };
    blit_frame(dl, tid, center, size, frame);
}

fn blit_frame(dl: &DrawListMut, tid: TextureId, center: [f32; 2], size: f32, frame: [f32; 4]) {
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
        .build();
}

fn draw_choya_sprite(dl: &DrawListMut, feet: [f32; 2], height: f32, sway: f32) {
    let (tid, frame) = if let Some(tid) = choya_sheet2() {
        (tid, CHOYA2_PARTY)
    } else if let Some(tid) = choya_sheet() {
        (tid, CHOYA_DANCE)
    } else {
        return;
    };
    let [_, _, fw, fh] = frame;
    let h = height;
    let w = h * (fw / fh);
    let tilt = sway * 0.18;
    let (c, s) = (tilt.cos(), tilt.sin());
    let rot =
        |dx: f32, dy: f32| -> [f32; 2] { [feet[0] + dx * c - dy * s, feet[1] + dx * s + dy * c] };
    let (uv0, uv1) = sheet_uv(frame);
    dl.add_image_quad(
        tid,
        rot(-w * 0.5, -h),
        rot(w * 0.5, -h),
        rot(w * 0.5, 0.0),
        rot(-w * 0.5, 0.0),
    )
    .uv(
        [uv0[0], uv0[1]],
        [uv1[0], uv0[1]],
        [uv1[0], uv1[1]],
        [uv0[0], uv1[1]],
    )
    .build();
}

/// Piñata choya standing on `feet`, slow bob + rock, chatting.
fn draw_choya(ui: &Ui, dl: &DrawListMut, feet: [f32; 2], height: f32, sway: f32, bar_right: f32) {
    draw_choya_sprite(dl, feet, height, sway);
    let [_, _, fw, fh] = CHOYA2_PARTY;
    let w = height * (fw / fh);

    let Some(text) = choya_quip() else {
        return;
    };
    let pad = 6.0;
    let sz = ui.calc_text_size(text);
    let bw = sz[0] + pad * 2.0;
    let bh = sz[1] + pad;
    let gap = 12.0;
    let mut bx = feet[0] + w * 0.5 + gap;
    if bx + bw > bar_right {
        bx = feet[0] - w * 0.5 - gap - bw;
    }
    let by = feet[1] - height * 0.78;
    let p = pal();
    dl.add_rect([bx, by], [bx + bw, by + bh], p.plate)
        .filled(true)
        .rounding(6.0)
        .build();
    dl.add_rect([bx, by], [bx + bw, by + bh], p.gold_dim)
        .rounding(6.0)
        .build();
    dl.add_text([bx + pad, by + pad * 0.4], color_u32(p.cream), text);
}

/// Face portraits from sheet 2. `center` is the avatar slot center.
pub fn draw_choya_avatar(ui: &Ui, center: [f32; 2], size: f32) {
    let Some(tid) = choya_sheet2() else {
        let i = anim_cell(100, CHOYA_IDLE.len());
        blit_choya_frame(
            &crate::ui::window_draw_list(ui),
            center,
            size,
            CHOYA_IDLE[i],
        );
        return;
    };
    let i = anim_cell(800, CHOYA2_FACE.len());
    blit_frame(
        &crate::ui::window_draw_list(ui),
        tid,
        center,
        size,
        CHOYA2_FACE[i],
    );
}

/// Standing pose plus isolated props (sombrero, shades, maracas, notes).
pub fn draw_choya_hero(ui: &Ui, center: [f32; 2], size: f32) {
    let dl = crate::ui::window_draw_list(ui);
    let Some(tid) = choya_sheet2() else {
        blit_choya_frame(&dl, center, size, CHOYA_HERO);
        return;
    };
    blit_frame(&dl, tid, center, size, CHOYA2_HERO);
    blit_frame(
        &dl,
        tid,
        [center[0], center[1] + size * 0.22],
        size * 0.55,
        CHOYA2_LEI,
    );
    blit_frame(
        &dl,
        tid,
        [center[0], center[1] - size * 0.46],
        size * 0.78,
        CHOYA2_SOMBRERO,
    );
    blit_frame(
        &dl,
        tid,
        [center[0], center[1] - size * 0.04],
        size * 0.44,
        CHOYA2_SHADES,
    );
    let t = anim_phase(2.1);
    let bounce = t.sin() * size * 0.05;
    blit_frame(
        &dl,
        tid,
        [center[0] - size * 0.52, center[1] + size * 0.16 + bounce],
        size * 0.32,
        CHOYA2_MARACA,
    );
    blit_frame(
        &dl,
        tid,
        [center[0] + size * 0.54, center[1] + size * 0.20 - bounce],
        size * 0.28,
        CHOYA2_MARACA2,
    );
    let a = t * 0.45;
    blit_frame(
        &dl,
        tid,
        [
            center[0] + a.cos() * size * 0.62,
            center[1] - size * 0.18 + a.sin() * size * 0.22,
        ],
        size * 0.16,
        CHOYA2_NOTE,
    );
    blit_frame(
        &dl,
        tid,
        [
            center[0] - (a + 2.2).cos() * size * 0.55,
            center[1] + size * 0.38 + (a + 2.2).sin() * size * 0.10,
        ],
        size * 0.14,
        CHOYA2_HEART,
    );
}

/// Pacing while the LLM works. The rock-peek used to sit here and it read
/// as asleep — every slot showed a still Choya for the whole request, so
/// "working" and "idle" looked the same. A brisk walk is unmistakably awake.
pub fn draw_choya_thinking(ui: &Ui, center: [f32; 2], size: f32) {
    if choya_sheet2().is_some() {
        draw_choya_walk_paced(ui, center, size, 133);
        return;
    }
    let i = anim_cell(83, CHOYA_IDLE.len());
    blit_choya_frame(
        &crate::ui::window_draw_list(ui),
        center,
        size,
        CHOYA_IDLE[i],
    );
}

/// The chat row's working pose: the nine-frame maraca bob from sheet 1.
/// Header paces, row bobs, composer blinks — three slots, three motions.
pub fn draw_choya_thinking_row(ui: &Ui, center: [f32; 2], size: f32) {
    let i = anim_cell(100, CHOYA_IDLE.len());
    blit_choya_frame(
        &crate::ui::window_draw_list(ui),
        center,
        size,
        CHOYA_IDLE[i],
    );
}

/// Peeking-from-the-rock pose, kept for whoever wants a shy Choya.
#[allow(dead_code)]
pub fn draw_choya_peek(ui: &Ui, center: [f32; 2], size: f32) {
    let Some(tid) = choya_sheet2() else {
        blit_choya_frame(&crate::ui::window_draw_list(ui), center, size, CHOYA_THINK);
        return;
    };
    blit_frame(
        &crate::ui::window_draw_list(ui),
        tid,
        center,
        size,
        CHOYA2_PEEK,
    );
}

/// Six-frame bounce from sheet 2 (composer / small slots).
pub fn draw_choya_walk(ui: &Ui, center: [f32; 2], size: f32) {
    draw_choya_walk_paced(ui, center, size, 233);
}

pub fn draw_choya_walk_paced(ui: &Ui, center: [f32; 2], size: f32, cell_ms: u64) {
    let Some(tid) = choya_sheet2() else {
        draw_choya_avatar(ui, center, size);
        return;
    };
    let i = anim_cell(cell_ms, CHOYA2_WALK.len());
    blit_frame(
        &crate::ui::window_draw_list(ui),
        tid,
        center,
        size,
        CHOYA2_WALK[i],
    );
}

pub fn draw_choya_party(ui: &Ui, center: [f32; 2], size: f32) {
    let Some(tid) = choya_sheet2() else {
        blit_choya_frame(&crate::ui::window_draw_list(ui), center, size, CHOYA_DANCE);
        return;
    };
    blit_frame(
        &crate::ui::window_draw_list(ui),
        tid,
        center,
        size,
        CHOYA2_PARTY,
    );
}

/// Quips the Free Choya says while it is up. One is picked per appearance.
const FREE_QUIPS: [&str; 6] = [
    "We are free, baby!",
    "YEY! Free!",
    "No card, no problem",
    "Zero gold, zero coin",
    "Free as a skritt in a vault",
    "Choya approves this budget",
];

/// A Choya in shades, dancing above the Free filter.
///
/// `rise` is 0 hidden to 1 fully up, eased by the caller. It drives both the
/// slide and the fade together, so the sprite arrives from under the row
/// rather than blinking into place, and leaves the same way. At 0 nothing is
/// drawn at all — an invisible sprite is still a texture bind every frame.
///
/// Drawn on the foreground list: the row it sits above is near the top of a
/// panel, and a window-local list would clip the poor thing off at the knees.
pub fn draw_free_choya(ui: &Ui, anchor: [f32; 2], row_h: f32, rise: f32) {
    let rise = rise.clamp(0.0, 1.0);
    if rise <= 0.01 {
        return;
    }
    let size = (row_h * 1.9).clamp(28.0, 64.0);
    // Slow, because a fast dance beside a settings control reads as a bug.
    // Two rates that do not divide evenly, so the sway and the bob drift
    // against each other and the loop never looks like a loop.
    let sway = anim_phase(2.7).sin() * size * 0.10;
    let bob = anim_phase(1.86).sin() * size * 0.05;
    // Travels its own height on the way in, so it looks like it climbed out
    // from behind the row.
    let hidden = size * 0.9;
    let center = [
        anchor[0] + sway,
        anchor[1] - size * 0.55 + bob + hidden * (1.0 - rise),
    ];
    let dl = ui.get_foreground_draw_list();
    let Some(tid) = choya_sheet2() else {
        // Sheet two is where the shades live. Without it, the first sheet's
        // dance pose still dances — just bare-faced.
        if let Some(tid) = choya_sheet() {
            blit_alpha(&dl, tid, center, size, CHOYA_DANCE, rise);
        }
        return;
    };
    blit_alpha(&dl, tid, center, size, CHOYA2_PARTY, rise);
    // Shades sit on the face, and the sprite is wider than it is tall, so it
    // is placed against the body's width rather than scaled to `size`.
    blit_alpha(
        &dl,
        tid,
        [center[0], center[1] - size * 0.17],
        size * 0.52,
        CHOYA2_SHADES,
        rise,
    );
    // A maraca each side, counter-swaying, and a note that drifts off.
    blit_alpha(
        &dl,
        tid,
        [center[0] - size * 0.42 - sway, center[1] + size * 0.08],
        size * 0.26,
        CHOYA2_MARACA,
        rise,
    );
    blit_alpha(
        &dl,
        tid,
        [center[0] + size * 0.42 - sway, center[1] + size * 0.08],
        size * 0.26,
        CHOYA2_MARACA2,
        rise,
    );
    let note = (elapsed_ms() as f32 / 1000.0 * 1.2).fract();
    blit_alpha(
        &dl,
        tid,
        [
            center[0] + size * (0.30 + note * 0.25),
            center[1] - size * (0.35 + note * 0.55),
        ],
        size * 0.18,
        CHOYA2_NOTE,
        rise * (1.0 - note),
    );
    draw_free_quip(ui, &dl, center, size, rise);
}

/// The speech bubble above the dancing Choya.
///
/// The quip is chosen from the frame count at the moment it rises, so it
/// holds still while the Choya is up and is a different one next time. It
/// fades in behind the sprite's own rise so the words arrive after the
/// dancer, which is the order a reader expects.
fn draw_free_quip(ui: &Ui, dl: &DrawListMut, center: [f32; 2], size: f32, rise: f32) {
    let alpha = ((rise - 0.55) / 0.45).clamp(0.0, 1.0);
    if alpha <= 0.02 {
        return;
    }
    let quip = FREE_QUIPS[(ui.frame_count() as usize / 600) % FREE_QUIPS.len()];
    let text = ui.calc_text_size(quip);
    let pad = [7.0, 4.0];
    let tail = 5.0;
    let w = text[0] + pad[0] * 2.0;
    let h = text[1] + pad[1] * 2.0;
    let min = [center[0] - w * 0.5, center[1] - size * 0.62 - h - tail];
    let max = [min[0] + w, min[1] + h];
    let p = pal();
    let bg = [p.gold_fill[0], p.gold_fill[1], p.gold_fill[2], alpha * 0.95];
    dl.add_rect(min, max, color_u32(bg))
        .filled(true)
        .rounding(5.0)
        .build();
    // The tail, pointing down at whoever is talking.
    dl.add_triangle(
        [center[0] - tail, max[1]],
        [center[0] + tail, max[1]],
        [center[0], max[1] + tail],
        color_u32(bg),
    )
    .filled(true)
    .build();
    let ink = [0.10, 0.08, 0.05, alpha];
    dl.add_text([min[0] + pad[0], min[1] + pad[1]], color_u32(ink), quip);
}

/// [`blit_frame`] with an alpha, for anything that fades.
fn blit_alpha(
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
        .col([1.0, 1.0, 1.0, alpha.clamp(0.0, 1.0)])
        .build();
}

pub const HEADER_POSE_COUNT: u8 = 4;
pub const HEADER_POSE_SECS: u64 = 60;
pub const COMPOSER_BOB_SECS: f32 = 3.0;

/// Pick a different idle pose. `mix` is typically `frame_count`.
pub fn next_header_pose(prev: u8, mix: u32) -> u8 {
    let n = HEADER_POSE_COUNT;
    let step = 1 + (mix % (n as u32 - 1)) as u8;
    prev.wrapping_add(step) % n
}

pub fn tick_header_pose(pose: &mut u8, since: &mut Option<Instant>, mix: u32, now: Instant) {
    match *since {
        None => *since = Some(now),
        Some(t) if now.saturating_duration_since(t).as_secs() >= HEADER_POSE_SECS => {
            *pose = next_header_pose(*pose, mix);
            *since = Some(now);
        }
        _ => {}
    }
}

pub fn composer_choya_bobbing(last_typed: Option<Instant>, now: Instant) -> bool {
    last_typed
        .map(|t| now.saturating_duration_since(t).as_secs_f32() < COMPOSER_BOB_SECS)
        .unwrap_or(false)
}

/// Header mascot: peek while the LLM works, otherwise a minute-cycled idle pose.
pub fn draw_choya_header(ui: &Ui, center: [f32; 2], size: f32, waiting: bool, pose: u8) {
    if waiting {
        draw_choya_thinking(ui, center, size);
        return;
    }
    match pose {
        1 => draw_choya_hero(ui, center, size),
        2 => draw_choya_walk(ui, center, size),
        3 => draw_choya_party(ui, center, size),
        _ => draw_choya_sleep(ui, center, size),
    }
}

pub fn draw_choya_sleep(ui: &Ui, center: [f32; 2], size: f32) {
    if let Some(tid) = choya_sheet2() {
        blit_frame(
            &crate::ui::window_draw_list(ui),
            tid,
            center,
            size,
            CHOYA2_SLEEP,
        );
        return;
    }
    blit_choya_frame(&crate::ui::window_draw_list(ui), center, size, CHOYA_SLEEP);
}

pub fn draw_gem_icon(dl: &DrawListMut, top: [f32; 2], height: f32) {
    draw_gem_chest(dl, top, height);
}

const CHOYA_LINES: &[&str] = &[
    "poke!",
    "not salad.",
    "fiesta!",
    "olé!",
    "don't hug",
    "candy?",
    "spiky.",
    "ow ow ow",
    "shake it!",
    "I'm a plant",
    "needles.",
    "boom?",
    "hit me",
    "loot inside",
    "no touchy",
    "yeet?",
    "pop!",
    "not a pear",
    "hug = ouch",
    "I bite",
    "piñata!",
    "amigo!",
    "ay ay ay",
    "boop?",
    "no candy",
    "violence!",
    "revenge poke",
    "don't sit",
    "I'm loot",
    "wiggle",
    "too cute",
    "coins?",
    "spicy hug",
    "keep off",
    "Elona!",
];

struct ChoyaTalk {
    rng: u64,
    line: usize,
    showing: bool,
    until: Instant,
}

static CHOYA_TALK: Mutex<Option<ChoyaTalk>> = Mutex::new(None);

fn choya_gap_ms(rng: u64) -> u64 {
    1000 + rng % 4001
}

fn choya_rng_step(rng: u64) -> u64 {
    rng.wrapping_mul(6364136223846793005).wrapping_add(1)
}

fn choya_quip() -> Option<&'static str> {
    let now = Instant::now();
    let mut guard = CHOYA_TALK.lock().unwrap_or_else(|e| e.into_inner());
    let talk = guard.get_or_insert_with(|| {
        let rng = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xC0C0_A11CE);
        ChoyaTalk {
            rng,
            line: 0,
            showing: false,
            until: now + Duration::from_millis(choya_gap_ms(rng)),
        }
    });
    if now >= talk.until {
        talk.rng = choya_rng_step(talk.rng);
        if talk.showing {
            talk.showing = false;
            talk.until = now + Duration::from_millis(choya_gap_ms(talk.rng));
        } else {
            talk.showing = true;
            let n = CHOYA_LINES.len();
            let skip = 1 + (talk.rng as usize % (n - 1).max(1));
            talk.line = (talk.line + skip) % n;
            talk.until = now + Duration::from_millis(1600);
        }
    }
    if talk.showing {
        Some(CHOYA_LINES[talk.line])
    } else {
        None
    }
}

fn draw_gem_chest(dl: &DrawListMut, top: [f32; 2], height: f32) {
    let w = height * (308.0 / 256.0);
    let pmin = [top[0] - w * 0.5, top[1]];
    let pmax = [top[0] + w * 0.5, top[1] + height];
    if let Some(tid) = embedded_tex(
        "GW2BO_GEM_CHEST3",
        include_bytes!("../../assets/gem_chest.png"),
    ) {
        dl.add_image(tid, pmin, pmax).build();
    }
}

fn draw_mystic_coin(
    dl: &DrawListMut,
    c: [f32; 2],
    r: f32,
    metal: usize,
    fill: f32,
    fallback: [f32; 4],
) {
    if let Some(tid) = mystic_coin_tex(metal) {
        let a = 0.40 + 0.60 * fill;
        dl.add_image(tid, [c[0] - r, c[1] - r], [c[0] + r, c[1] + r])
            .col([1.0, 1.0, 1.0, a])
            .build();
    } else {
        draw_coin(dl, c, r, fallback, fill);
    }
}

#[cfg(test)]
mod tests {
    use super::choya_gap_ms;
    use std::time::{Duration, Instant};

    // Theme system

    /// WCAG 2.x sRGB channel linearization.
    fn srgb_lin(c: f32) -> f64 {
        let c = c as f64;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn rel_luminance(c: [f32; 3]) -> f64 {
        0.2126 * srgb_lin(c[0]) + 0.7152 * srgb_lin(c[1]) + 0.0722 * srgb_lin(c[2])
    }

    fn contrast(a: [f32; 3], b: [f32; 3]) -> f64 {
        let (la, lb) = (rel_luminance(a), rel_luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Every built-in preset keeps its chrome legible. Thresholds sit slightly
    /// under the design targets (10/4.5/4.5/7) to absorb rounding.
    #[test]
    fn presets_pass_wcag_contrast() {
        let near_black = [0.10, 0.08, 0.04];
        for p in super::PRESETS {
            let tb = contrast(p.text, p.bg);
            assert!(tb >= 7.0, "{}: text/bg contrast {tb:.2} < 7.0", p.id);
            let mb = contrast(p.muted, p.bg);
            assert!(mb >= 4.0, "{}: muted/bg contrast {mb:.2} < 4.0", p.id);
            let ab = contrast(p.accent, p.bg);
            assert!(ab >= 4.0, "{}: accent/bg contrast {ab:.2} < 4.0", p.id);
            let na = contrast(near_black, p.accent);
            assert!(
                na >= 6.0,
                "{}: dark-text/accent contrast {na:.2} < 6.0",
                p.id
            );
        }
    }

    /// The derivation rules reproduce the shipped Tyrian Gold colors closely.
    /// Major tokens land exactly; the derived family stays within 0.12 per
    /// channel (the stored `TYRIAN` palette keeps the default pixel-identical).
    #[test]
    fn derive_palette_tyrian_stays_close_to_shipped() {
        let t = &super::PRESETS[0];
        assert_eq!(t.id, "tyrian-gold");
        let d = super::derive_palette(t.bg, t.panel, t.accent, t.text, t.muted);
        let s = super::TYRIAN;
        let close = |name: &str, a: [f32; 4], b: [f32; 4], tol: f32| {
            for i in 0..3 {
                assert!(
                    (a[i] - b[i]).abs() <= tol,
                    "{name}[{i}]: derived {} vs shipped {} (tol {tol})",
                    a[i],
                    b[i]
                );
            }
            assert!(
                (a[3] - b[3]).abs() <= 1e-6,
                "{name}: alpha {} must equal the shipped alpha {}",
                a[3],
                b[3]
            );
        };
        for (name, a, b) in [
            ("ink", d.ink, s.ink),
            ("cream", d.cream, s.cream),
            ("muted", d.muted, s.muted),
            ("gold", d.gold, s.gold),
            ("plate", d.plate, s.plate),
            ("frame_bg", d.frame_bg, s.frame_bg),
        ] {
            close(name, a, b, 0.005);
        }
        for (name, a, b) in [
            ("child", d.child, s.child),
            ("gold_dim", d.gold_dim, s.gold_dim),
            ("gold_fill", d.gold_fill, s.gold_fill),
            ("gold_hover", d.gold_hover, s.gold_hover),
            ("plate_empty", d.plate_empty, s.plate_empty),
            ("popup_bg", d.popup_bg, s.popup_bg),
            ("frame_bg_hovered", d.frame_bg_hovered, s.frame_bg_hovered),
            ("frame_bg_active", d.frame_bg_active, s.frame_bg_active),
            ("button", d.button, s.button),
            ("button_hovered", d.button_hovered, s.button_hovered),
            ("button_active", d.button_active, s.button_active),
            ("header", d.header, s.header),
            ("header_hovered", d.header_hovered, s.header_hovered),
            ("title_bg", d.title_bg, s.title_bg),
            ("title_bg_active", d.title_bg_active, s.title_bg_active),
            (
                "gold_button_hovered",
                d.gold_button_hovered,
                s.gold_button_hovered,
            ),
            (
                "gold_button_active",
                d.gold_button_active,
                s.gold_button_active,
            ),
            ("gold_button_text", d.gold_button_text, s.gold_button_text),
            ("chip_idle_fill", d.chip_idle_fill, s.chip_idle_fill),
            ("chip_idle_rim", d.chip_idle_rim, s.chip_idle_rim),
            ("header_plate", d.header_plate, s.header_plate),
        ] {
            close(name, a, b, 0.12);
        }
    }

    /// Seeding a fresh custom theme reads its bases from here, so every
    /// preset must hand back exactly the five colors it renders from, and
    /// `"custom"` must NOT resolve (it has no bases of its own to copy).
    #[test]
    fn preset_bases_round_trip_every_preset() {
        for p in super::PRESETS {
            assert_eq!(
                super::preset_bases(p.id),
                Some([p.bg, p.panel, p.accent, p.text, p.muted]),
                "{} must expose its own bases",
                p.id
            );
        }
        assert_eq!(super::preset_bases("custom"), None);
        assert_eq!(super::preset_bases("no-such-theme"), None);
    }

    #[test]
    fn preset_ids_match_preset_data() {
        let ids = super::preset_ids();
        assert_eq!(ids.len(), super::PRESETS.len());
        for (i, p) in super::PRESETS.iter().enumerate() {
            assert_eq!(ids[i], (p.id, p.name));
        }
        assert_eq!(ids[0].0, "tyrian-gold");
    }

    /// One test mutates the shared active palette (no other test reads it) so
    /// the assertions stay serial. Ends on the shipped Tyrian Gold palette.
    #[test]
    fn apply_theme_selects_presets_and_falls_back() {
        use gw2_core::config::{CustomTheme, ThemeConfig};
        let cfg = |preset: &str| ThemeConfig {
            preset: preset.into(),
            custom: CustomTheme::default(),
            ..Default::default()
        };

        super::apply_theme(&cfg("glacial-ward"));
        let g = &super::PRESETS[1];
        assert_eq!(g.id, "glacial-ward");
        assert_eq!(
            super::pal(),
            super::derive_palette(g.bg, g.panel, g.accent, g.text, g.muted)
        );

        super::apply_theme(&cfg("no-such-theme"));
        assert_eq!(super::pal(), super::TYRIAN, "unknown preset falls back");

        let custom = ThemeConfig {
            preset: "custom".into(),
            custom: CustomTheme {
                name: "Test".into(),
                bg: [0.02, 0.03, 0.04],
                panel: [0.05, 0.06, 0.08],
                accent: [0.9, 0.2, 0.4],
                text: [0.9, 0.9, 0.9],
                muted: [0.5, 0.5, 0.5],
            },
            ..Default::default()
        };
        super::apply_theme(&custom);
        assert_eq!(
            super::pal(),
            super::derive_palette(
                [0.02, 0.03, 0.04],
                [0.05, 0.06, 0.08],
                [0.9, 0.2, 0.4],
                [0.9, 0.9, 0.9],
                [0.5, 0.5, 0.5]
            )
        );

        super::apply_theme(&cfg("tyrian-gold"));
        assert_eq!(
            super::pal(),
            super::TYRIAN,
            "tyrian-gold is the shipped palette"
        );
    }

    #[test]
    fn hsv_round_trips_key_colors() {
        for c in [
            [1.0, 0.84, 0.38],
            [0.40, 0.76, 1.0],
            [0.42, 0.85, 0.45],
            [0.80, 0.58, 1.0],
            [0.07, 0.06, 0.045],
        ] {
            let (h, s, v) = super::rgb_to_hsv(c);
            let back = super::hsv_to_rgb(h, s, v);
            for i in 0..3 {
                assert!((back[i] - c[i]).abs() < 1e-5, "{c:?} -> {back:?}");
            }
        }
    }

    #[test]
    fn wrap_pos_local_converts_screen_x_to_window_local() {
        assert_eq!(super::wrap_pos_local(200.0, 50.0, 0.0), 150.0);
        assert_eq!(super::wrap_pos_local(200.0, 50.0, 10.0), 160.0);
    }

    #[test]
    fn choya_gap_is_one_to_five_seconds() {
        for seed in [0u64, 1, 4000, 4001, u64::MAX] {
            let ms = choya_gap_ms(seed);
            assert!((1000..=5000).contains(&ms), "{ms} from {seed}");
        }
    }

    #[test]
    fn choya_sheet_uvs_stay_on_atlas() {
        let extra = [
            super::CHOYA2_HERO,
            super::CHOYA2_PEEK,
            super::CHOYA2_PARTY,
            super::CHOYA2_SOMBRERO,
            super::CHOYA2_SHADES,
            super::CHOYA2_MARACA,
            super::CHOYA2_MARACA2,
            super::CHOYA2_NOTE,
            super::CHOYA2_HEART,
            super::CHOYA2_LEI,
            super::CHOYA_SLEEP,
            super::CHOYA2_SLEEP,
        ];
        for frame in [super::CHOYA_HERO, super::CHOYA_DANCE, super::CHOYA_THINK]
            .into_iter()
            .chain(super::CHOYA_IDLE)
            .chain(extra)
            .chain(super::CHOYA2_WALK)
            .chain(super::CHOYA2_FACE)
        {
            let (a, b) = super::sheet_uv(frame);
            assert!(a[0] >= 0.0 && a[1] >= 0.0, "{frame:?} {a:?}");
            assert!(b[0] <= 1.0 && b[1] <= 1.0, "{frame:?} {b:?}");
            assert!(b[0] > a[0] && b[1] > a[1], "{frame:?}");
        }
    }

    #[test]
    fn sheet_uv_insets_half_pixel() {
        let frame = super::CHOYA2_SLEEP;
        let [x, y, w, h] = frame;
        let (a, b) = super::sheet_uv(frame);
        assert!(a[0] > x / super::CHOYA_SHEET_W);
        assert!(a[1] > y / super::CHOYA_SHEET_H);
        assert!(b[0] < (x + w) / super::CHOYA_SHEET_W);
        assert!(b[1] < (y + h) / super::CHOYA_SHEET_H);
    }

    #[test]
    fn next_header_pose_always_changes() {
        for prev in 0..super::HEADER_POSE_COUNT {
            for mix in 0..16u32 {
                let next = super::next_header_pose(prev, mix);
                assert!(next < super::HEADER_POSE_COUNT);
                assert_ne!(next, prev);
            }
        }
    }

    #[test]
    fn tick_header_pose_advances_after_a_minute() {
        let t0 = Instant::now();
        let mut pose = 0u8;
        let mut since = Some(t0);
        super::tick_header_pose(&mut pose, &mut since, 0, t0);
        assert_eq!(pose, 0);
        super::tick_header_pose(
            &mut pose,
            &mut since,
            0,
            t0 + Duration::from_secs(super::HEADER_POSE_SECS),
        );
        assert_ne!(pose, 0);
        assert_eq!(
            since,
            Some(t0 + Duration::from_secs(super::HEADER_POSE_SECS))
        );
    }

    #[test]
    fn composer_choya_sleeps_three_seconds_after_typing() {
        let t0 = Instant::now();
        assert!(!super::composer_choya_bobbing(None, t0));
        assert!(super::composer_choya_bobbing(Some(t0), t0));
        assert!(super::composer_choya_bobbing(
            Some(t0),
            t0 + Duration::from_millis(2999)
        ));
        assert!(!super::composer_choya_bobbing(
            Some(t0),
            t0 + Duration::from_secs(4)
        ));
    }

    #[test]
    fn prose_classifies_headings_and_lists() {
        assert_eq!(super::heading("## Open World"), Some((2, "Open World")));
        assert_eq!(super::heading("# Patch"), Some((1, "Patch")));
        assert_eq!(super::heading("Hello"), None);
        assert_eq!(
            super::list_mark("- Black Lion"),
            Some(("•".into(), "Black Lion"))
        );
        assert_eq!(super::list_mark("2. Second"), Some(("2.".into(), "Second")));
        assert_eq!(super::list_mark("• Already"), Some(("•".into(), "Already")));
        assert_eq!(super::list_mark("plain"), None);
        assert!(super::plain_section("Open World"));
        assert!(!super::plain_section("See below for details."));
    }
}

#[cfg(test)]
mod anim_timing_tests {
    use super::{anim_cell_at, anim_phase_at, anim_pulse_at, elapsed_ms};

    /// Every animation cell duration in use, paired with its cycle length.
    /// Mirrors the actual call sites: idle fallback, thinking row, header
    /// walk (paced 133ms), composer walk (paced 233ms), and the "opening"
    /// spinner.
    const CASES: [(u64, usize); 5] = [(83, 6), (100, 6), (133, 6), (233, 6), (133, 4)];

    /// Simulate rendering at a fixed frame rate up to `total_ms`, returning
    /// the millisecond timestamp of the frame actually on screen at each
    /// wall-clock instant in `sample_points_ms`.
    fn displayed_ms_at_samples(hz: f64, total_ms: u64, sample_points_ms: &[u64]) -> Vec<u64> {
        let step_ms = 1000.0 / hz;
        let mut frame_ms = Vec::new();
        let mut t = 0.0f64;
        while t <= total_ms as f64 {
            frame_ms.push(t.floor() as u64);
            t += step_ms;
        }
        let mut out = Vec::with_capacity(sample_points_ms.len());
        let mut fi = 0usize;
        for &sample in sample_points_ms {
            while fi + 1 < frame_ms.len() && frame_ms[fi + 1] <= sample {
                fi += 1;
            }
            out.push(frame_ms[fi]);
        }
        out
    }

    /// Shortest distance between two cell indices around a `modulus`-cycle,
    /// e.g. `cyclic_diff(5, 0, 6) == 1` (wraps), not 5.
    fn cyclic_diff(a: usize, b: usize, modulus: usize) -> usize {
        let raw = a.abs_diff(b);
        raw.min(modulus - raw)
    }

    /// The whole point of driving animation off wall-clock time instead of
    /// `frame_count()`: a 60/144/240 Hz monitor must show the same cell at
    /// the same wall-clock instant, and advance at the same long-run rate.
    /// A coarser monitor can lag the finer ones by at most one of *its own*
    /// frames (it simply hasn't rendered yet) — that is normal display
    /// latency, not a speed difference, so the tolerance is one cell step,
    /// not exact equality.
    #[test]
    fn anim_cell_is_frame_rate_independent() {
        let total_ms = 5000u64;
        let sample_points: Vec<u64> = (0..=total_ms).step_by(50).collect();
        let rates = [60.0f64, 144.0, 240.0];

        for &(cell_ms, cells) in &CASES {
            let sequences: Vec<Vec<usize>> = rates
                .iter()
                .map(|&hz| {
                    displayed_ms_at_samples(hz, total_ms, &sample_points)
                        .into_iter()
                        .map(|ms| anim_cell_at(ms, cell_ms, cells))
                        .collect()
                })
                .collect();
            for pair in sequences.windows(2) {
                for (a, b) in pair[0].iter().zip(&pair[1]) {
                    assert!(
                        cyclic_diff(*a, *b, cells) <= 1,
                        "cell_ms={cell_ms}: {a} vs {b} — more than one frame's display lag"
                    );
                }
            }

            // Advance rate: every rate must cover the same distance in cell
            // space over 5s, within one cell (the trailing partial cell may
            // or may not have completed depending on rounding).
            let expected = total_ms as f64 / cell_ms as f64;
            for &hz in &rates {
                let step_ms = 1000.0 / hz;
                let mut frame_ms = Vec::new();
                let mut t = 0.0f64;
                while t <= total_ms as f64 {
                    frame_ms.push(t.floor() as u64);
                    t += step_ms;
                }
                let advances = frame_ms
                    .windows(2)
                    .filter(|w| {
                        anim_cell_at(w[0], cell_ms, cells) != anim_cell_at(w[1], cell_ms, cells)
                    })
                    .count() as f64;
                assert!(
                    (advances - expected).abs() <= 1.0,
                    "cell_ms={cell_ms} @ {hz}Hz: {advances} advances over {total_ms}ms, expected ~{expected}"
                );
            }
        }
    }

    /// Pulse math at one fixed instant — the sin curve itself doesn't need a
    /// framerate sweep, just a check it reads from the same time source.
    #[test]
    fn anim_pulse_matches_expected_curve_at_one_second() {
        let got = anim_pulse_at(1000);
        let want = (1.05f32).sin().abs();
        assert!((got - want).abs() < 1e-6);
    }

    /// `draw_choya_hero`'s maraca bounce (`anim_phase(2.1)`) at one instant —
    /// same reasoning as the pulse check above.
    #[test]
    fn anim_phase_matches_hero_bounce_at_one_second() {
        let got = anim_phase_at(1000, 2.1);
        let want = 2.1f32;
        assert!((got - want).abs() < 1e-6);
    }

    /// The real clock never goes backward between two reads.
    #[test]
    fn elapsed_ms_is_monotonic() {
        let a = elapsed_ms();
        let b = elapsed_ms();
        assert!(b >= a);
    }
}

#[cfg(test)]
mod tab_tint_tests {
    use super::*;

    fn rgb(c: [f32; 4]) -> [f32; 3] {
        [c[0], c[1], c[2]]
    }

    fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    /// The three kinds of tab stay apart from each other and from the panel
    /// on every preset and at the custom-theme extremes (specs/006 US2).
    #[test]
    fn tab_tints_keep_contrast_over_every_preset() {
        let mut palettes: Vec<(String, Palette)> = PRESETS
            .iter()
            .map(|p| {
                (
                    p.id.to_string(),
                    derive_palette(p.bg, p.panel, p.accent, p.text, p.muted),
                )
            })
            .collect();
        palettes.push(("tyrian-shipped".into(), TYRIAN));
        palettes.push((
            "custom-black".into(),
            derive_palette(
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
                [1.0, 0.84, 0.38],
                [1.0, 1.0, 1.0],
                [0.6, 0.6, 0.6],
            ),
        ));
        palettes.push((
            "custom-white".into(),
            derive_palette(
                [1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0],
                [0.8, 0.5, 0.1],
                [0.0, 0.0, 0.0],
                [0.4, 0.4, 0.4],
            ),
        ));
        // Current, optimized, and the three published-site brand colours
        // (GuildJen pink, Hardstuck red, Snowcrows sky blue).
        let kinds = [
            CURRENT,
            OPTIMIZED,
            [0.95, 0.42, 0.72, 1.0],
            [0.90, 0.30, 0.28, 1.0],
            [0.35, 0.82, 0.86, 1.0],
        ];
        for (id, p) in &palettes {
            let fills: Vec<[f32; 3]> = kinds.iter().map(|k| rgb(tab_tint_in(p, *k).fill)).collect();
            for i in 0..fills.len() {
                for j in (i + 1)..fills.len() {
                    let d = dist(fills[i], fills[j]);
                    assert!(d >= 0.06, "{id}: tab fills {i} and {j} too close ({d:.3})");
                }
                for (name, bg) in [("ink", rgb(p.ink)), ("plate", rgb(p.plate))] {
                    let d = dist(fills[i], bg);
                    assert!(d >= 0.10, "{id}: tab fill {i} vs {name} too close ({d:.3})");
                }
                let sel = rgb(tab_tint_in(p, kinds[i]).selected_fill);
                let d = dist(sel, fills[i]);
                assert!(
                    d >= 0.15,
                    "{id}: selected tab {i} indistinct from idle ({d:.3})"
                );
                // The adapted brand stays readable against the ground.
                let adapted = rgb(adapt_kind(p, kinds[i]));
                let db = (brightness(adapted) - brightness(rgb(p.ink))).abs();
                assert!(
                    db >= 0.3,
                    "{id}: kind {i} too close to the ground in brightness ({db:.3})"
                );
            }
        }
    }
}
