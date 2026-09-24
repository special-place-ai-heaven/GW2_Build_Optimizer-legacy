//! Message developer wizard - the step runner over [`Draft`] shown inside the
//! About tab below the action row: pick tiles, chip steps, text steps, the
//! summary card, and the sent/thanks plates. The label helpers at the top are
//! pure and unit-tested; everything ImGui sits below them.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use nexus::imgui::{
    DrawListMut, InputTextCallbackHandler, InputTextMultilineCallback, StyleVar, TextCallbackData,
    Ui,
};

use super::glyphs::{category_color, draw_glyph};
use crate::feedback::{Draft, FeedbackState, TextError, WizardStep};
use crate::state::AddonState;
use crate::ui::{color_u32, theme};
use gw2_core::feedback::message::{now_unix, FailReason, LocalMessage, MessageStatus};
use gw2_core::feedback::report::{snapshot_bytes, MAX_REQUEST_BYTES, MAX_SNAPSHOT_BYTES};
use gw2_core::feedback::taxonomy::FeedbackTaxonomy;
use gw2_core::i18n::{t, tf};

/// Plate padding on every side.
const PAD: f32 = 12.0;
/// Choya avatar size in the plate's left column.
const AVATAR: f32 = 48.0;
/// Gap between tiles, chips, and footer buttons.
const GAP: f32 = 8.0;
const ROUNDING: f32 = 6.0;
/// `Reach me` input cap (chars).
const CONTACT_MAX: usize = 200;

/// Plate height measured last frame, stored as `f32` bits. The plate has to be
/// painted before its content and the draw list cannot be channel-split here
/// because `select_chip` / `draw_choya_avatar` re-borrow it, so the height lags
/// one frame - invisible except on the very first frame after opening.
static PLATE_H: AtomicU32 = AtomicU32::new(0);
/// Byte range of the mail textarea selection (last InputText ALWAYS callback).
static SEL_LO: AtomicU32 = AtomicU32::new(0);

static SEL_HI: AtomicU32 = AtomicU32::new(0);
static PENDING_BOLD: AtomicBool = AtomicBool::new(false);
static EDITOR_STEP: AtomicU32 = AtomicU32::new(0);

// pure label helpers

/// `Report a bug › Optimize › Wrong result`: the category label followed by
/// the `choice.<id>` label of every entry in `path`.
fn path_label(taxonomy: &FeedbackTaxonomy, category: &str, path: &[String]) -> String {
    let mut out = taxonomy
        .category(category)
        .map_or_else(|| category.to_string(), |c| t(&c.label));
    for id in path {
        out.push_str(" › ");
        out.push_str(&t(&format!("choice.{id}")));
    }
    out
}

/// [`path_label`] for the draft's picked category and current `path()`; empty before a pick.
fn path_text(draft: &Draft) -> String {
    match draft.category.as_deref() {
        Some(cat) => path_label(&draft.taxonomy, cat, &draft.path()),
        None => String::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Cream,
    Gold,
    Muted,
    Warn,
    Alert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum List {
    None,
    Bullet,
    Number,
}

#[derive(Clone, Debug, PartialEq)]
struct RichLine {
    text: String,
    bold: bool,
    align: Align,
    color: Ink,
    list: List,
}

impl Default for RichLine {
    fn default() -> Self {
        Self {
            text: String::new(),
            bold: false,
            align: Align::Left,
            color: Ink::Cream,
            list: List::None,
        }
    }
}

impl RichLine {
    fn is_normal(&self) -> bool {
        !self.bold
            && self.align == Align::Left
            && self.color == Ink::Cream
            && self.list == List::None
    }
}

fn ink_rgba(c: Ink) -> [f32; 4] {
    match c {
        Ink::Cream => theme::pal().cream,
        Ink::Gold => theme::pal().gold,
        Ink::Muted => theme::pal().muted,
        Ink::Warn => theme::WARN,
        Ink::Alert => theme::ERR,
    }
}

fn encode_line(l: &RichLine) -> String {
    let mut s = String::from("%");
    s.push(if l.bold { 'B' } else { 'N' });
    s.push(match l.align {
        Align::Left => 'L',
        Align::Center => 'C',
        Align::Right => 'R',
    });
    s.push(match l.color {
        Ink::Cream => '0',
        Ink::Gold => '1',
        Ink::Muted => '2',
        Ink::Warn => '3',
        Ink::Alert => '4',
    });
    s.push(match l.list {
        List::None => 'P',
        List::Bullet => 'U',
        List::Number => 'O',
    });
    s.push('|');
    s.push_str(&l.text);
    s
}

fn decode_line(raw: &str) -> RichLine {
    if let Some(rest) = raw.strip_prefix('%') {
        let b = rest.as_bytes();
        if b.len() >= 5 && b[4] == b'|' {
            let bold = match b[0] {
                b'B' => Some(true),
                b'N' => Some(false),
                _ => None,
            };
            let align = match b[1] {
                b'L' => Some(Align::Left),
                b'C' => Some(Align::Center),
                b'R' => Some(Align::Right),
                _ => None,
            };
            let color = match b[2] {
                b'0' => Some(Ink::Cream),
                b'1' => Some(Ink::Gold),
                b'2' => Some(Ink::Muted),
                b'3' => Some(Ink::Warn),
                b'4' => Some(Ink::Alert),
                _ => None,
            };
            let list = match b[3] {
                b'P' => Some(List::None),
                b'U' => Some(List::Bullet),
                b'O' => Some(List::Number),
                _ => None,
            };
            if let (Some(bold), Some(align), Some(color), Some(list)) = (bold, align, color, list) {
                return RichLine {
                    text: rest[5..].to_string(),
                    bold,
                    align,
                    color,
                    list,
                };
            }
        }
    }
    fallback_line(raw)
}

fn fallback_line(raw: &str) -> RichLine {
    let mut line = RichLine::default();
    if let Some(t) = raw
        .strip_prefix("### ")
        .or_else(|| raw.strip_prefix("## "))
        .or_else(|| raw.strip_prefix("# "))
    {
        line.bold = true;
        line.color = Ink::Gold;
        line.text = t.into();
        return line;
    }
    if let Some(t) = raw.strip_prefix("- ").or_else(|| raw.strip_prefix("* ")) {
        line.list = List::Bullet;
        line.text = t.into();
        return line;
    }
    let bytes = raw.as_bytes();
    let mut n = 0usize;
    while n < bytes.len() && bytes[n].is_ascii_digit() {
        n += 1;
    }
    if n > 0 && bytes.get(n) == Some(&b'.') && bytes.get(n + 1) == Some(&b' ') {
        line.list = List::Number;
        line.text = raw[n + 2..].into();
        return line;
    }
    line.text = raw.into();
    line
}

fn decode_lines(s: &str) -> Vec<RichLine> {
    if s.is_empty() {
        return vec![RichLine::default()];
    }
    let mut lines: Vec<RichLine> = s.lines().map(decode_line).collect();
    if lines.is_empty() {
        lines.push(RichLine::default());
    }
    lines
}

fn encode_lines(lines: &[RichLine]) -> String {
    if lines.iter().all(|l| l.text.is_empty() && l.is_normal()) {
        return String::new();
    }
    lines.iter().map(encode_line).collect::<Vec<_>>().join("\n")
}

fn plain_from_lines(lines: &[RichLine]) -> String {
    lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_index_at(s: &str, byte: usize) -> usize {
    let mut b = byte.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    s[..b].bytes().filter(|&c| c == b'\n').count()
}

fn selection_lines(plain: &str, lo: usize, hi: usize) -> (usize, usize) {
    let lo = lo.min(plain.len());
    let hi = hi.min(plain.len());
    let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    let first = line_index_at(plain, a);
    let last = if a == b {
        first
    } else {
        line_index_at(plain, b.saturating_sub(1))
    };
    (first, last)
}

fn resync_lines(old: &[RichLine], plain: &str) -> Vec<RichLine> {
    let plain = plain.replace('\r', "");
    let parts: Vec<&str> = plain.split('\n').collect();
    if parts.is_empty() {
        return vec![RichLine::default()];
    }
    let n = parts.len();
    let mut out = Vec::with_capacity(n);
    if n == old.len() {
        for (l, t) in old.iter().zip(&parts) {
            let mut line = l.clone();
            line.text = (*t).to_string();
            out.push(line);
        }
        return out;
    }
    if n == old.len() + 1 && !old.is_empty() {
        if let Some(i) = (0..old.len()).find(|&i| {
            old[..i]
                .iter()
                .map(|l| l.text.as_str())
                .eq(parts[..i].iter().copied())
                && old[i + 1..]
                    .iter()
                    .map(|l| l.text.as_str())
                    .eq(parts[i + 2..].iter().copied())
        }) {
            for (j, t) in parts.iter().enumerate() {
                let src = if j <= i { j } else { j - 1 };
                let mut line = old[src].clone();
                line.text = (*t).to_string();
                out.push(line);
            }
            return out;
        }
    }
    if old.len() == n + 1 && !parts.is_empty() {
        if let Some(i) = (0..n).find(|&i| {
            old[..i]
                .iter()
                .map(|l| l.text.as_str())
                .eq(parts[..i].iter().copied())
                && old[i + 2..]
                    .iter()
                    .map(|l| l.text.as_str())
                    .eq(parts[i + 1..].iter().copied())
        }) {
            for (j, t) in parts.iter().enumerate() {
                let src = if j <= i { j } else { j + 1 };
                let mut line = old[src.min(old.len() - 1)].clone();
                line.text = (*t).to_string();
                out.push(line);
            }
            return out;
        }
    }
    for (i, t) in parts.iter().enumerate() {
        let mut line = old
            .get(i)
            .cloned()
            .unwrap_or_else(|| out.last().cloned().unwrap_or_default());
        line.text = (*t).to_string();
        out.push(line);
    }
    out
}

#[derive(Clone, Copy)]
enum FmtCmd {
    Normal,
    Bold,
    Bullet,
    Number,
    Align(Align),
    Color(Ink),
}

fn apply_fmt(lines: &mut [RichLine], lo: usize, hi: usize, cmd: FmtCmd) {
    if lines.is_empty() {
        return;
    }
    let last = lines.len() - 1;
    let lo = lo.min(last);
    let hi = hi.min(last).max(lo);
    let all_bold = lines[lo..=hi].iter().all(|l| l.bold);
    let all_bullet = lines[lo..=hi].iter().all(|l| l.list == List::Bullet);
    let all_number = lines[lo..=hi].iter().all(|l| l.list == List::Number);
    for line in &mut lines[lo..=hi] {
        match cmd {
            FmtCmd::Normal => {
                line.bold = false;
                line.align = Align::Left;
                line.color = Ink::Cream;
                line.list = List::None;
            }
            FmtCmd::Bold => line.bold = !all_bold,
            FmtCmd::Bullet => {
                line.list = if all_bullet { List::None } else { List::Bullet };
            }
            FmtCmd::Number => {
                line.list = if all_number { List::None } else { List::Number };
            }
            FmtCmd::Align(a) => line.align = a,
            FmtCmd::Color(c) => line.color = c,
        }
    }
}

fn step_fingerprint(id: &str) -> u32 {
    let mut h = 2166136261u32;
    for b in id.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(16777619);
    }
    h
}

struct MailSel;

impl InputTextCallbackHandler for MailSel {
    fn on_always(&mut self, data: TextCallbackData) {
        let sel = data.selection();
        let (a, b) = if sel.start <= sel.end {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };
        SEL_LO.store(a as u32, Ordering::Relaxed);
        SEL_HI.store(b as u32, Ordering::Relaxed);
    }

    fn char_filter(&mut self, c: u16) -> Option<u16> {
        if (c == u16::from(b'b') || c == u16::from(b'B'))
            && SEL_LO.load(Ordering::Relaxed) != SEL_HI.load(Ordering::Relaxed)
        {
            PENDING_BOLD.store(true, Ordering::Relaxed);
            return None;
        }
        Some(c)
    }
}

fn wrap_chunks(ui: &Ui, text: &str, max_w: f32) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for piece in text.split_inclusive(' ') {
        let mut trial = cur.clone();
        trial.push_str(piece);
        if ui.calc_text_size(&trial)[0] <= max_w || cur.is_empty() {
            cur = trial;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = piece.to_string();
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

fn layout_mail(ui: &Ui, encoded: &str, wrap_w: f32, origin: Option<[f32; 2]>) -> f32 {
    if encoded.is_empty() {
        return 0.0;
    }
    let lh = ui.text_line_height() + 2.0;
    let wrap_w = wrap_w.max(8.0);
    let mut y = 0.0_f32;
    let mut n = 0u32;
    let mut glyphs: Vec<([f32; 2], [f32; 4], bool, String)> = Vec::new();
    for line in decode_lines(encoded) {
        if line.text.trim().is_empty() {
            n = 0;
            y += 8.0;
            continue;
        }
        if line.list == List::Number {
            n += 1;
        } else {
            n = 0;
        }
        let prefix = match line.list {
            List::Bullet => "• ".to_string(),
            List::Number => format!("{n}. "),
            List::None => String::new(),
        };
        let shown = format!("{}{}", prefix, line.text);
        let col = ink_rgba(line.color);
        for chunk in wrap_chunks(ui, &shown, wrap_w) {
            if let Some(origin) = origin {
                let tw = ui.calc_text_size(&chunk)[0];
                let x = match line.align {
                    Align::Left => origin[0],
                    Align::Center => origin[0] + (wrap_w - tw).max(0.0) * 0.5,
                    Align::Right => origin[0] + (wrap_w - tw).max(0.0),
                };
                glyphs.push(([x, origin[1] + y], col, line.bold, chunk));
            }
            y += lh;
        }
        y += 4.0;
    }
    if origin.is_some() {
        let dl = crate::ui::window_draw_list(ui);
        for (p, col, bold, chunk) in &glyphs {
            dl.add_text(*p, color_u32(*col), chunk);
            if *bold {
                dl.add_text([p[0] + 1.0, p[1]], color_u32(*col), chunk);
            }
        }
    }
    y
}

/// Formatted mailbag body (bold is a 1px double-draw - overlay fonts have no bold face).
pub(super) fn paint_mail(ui: &Ui, encoded: &str, wrap_w: f32) {
    let origin = ui.cursor_screen_pos();
    let h = layout_mail(ui, encoded, wrap_w, Some(origin));
    ui.dummy([wrap_w.max(1.0), h.max(1.0)]);
}

pub(super) fn mail_height(ui: &Ui, encoded: &str, wrap_w: f32) -> f32 {
    layout_mail(ui, encoded, wrap_w, None)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FmtIcon {
    Normal,
    Bold,
    Bullet,
    Number,
    Left,
    Center,
    Right,
}

fn fmt_icon_button(ui: &Ui, id: &str, side: f32, on: bool, icon: FmtIcon) -> bool {
    let p = ui.cursor_screen_pos();
    let hit = ui.invisible_button(id, [side, side]);
    let hovered = ui.is_item_hovered();
    let fill = if on {
        theme::pal().gold_fill
    } else if hovered {
        theme::pal().gold_hover
    } else {
        theme::with_alpha(theme::pal().chip_idle_fill, 0.72)
    };
    let rim = if on {
        theme::pal().gold
    } else if hovered {
        theme::pal().gold_dim
    } else {
        theme::pal().chip_idle_rim
    };
    let ink = if on {
        theme::pal().gold_button_text
    } else {
        theme::pal().cream
    };
    let dl = crate::ui::window_draw_list(ui);
    dl.add_rect(p, [p[0] + side, p[1] + side], fill)
        .filled(true)
        .rounding(theme::ICON_ROUNDING)
        .build();
    dl.add_rect(p, [p[0] + side, p[1] + side], rim)
        .rounding(theme::ICON_ROUNDING)
        .build();
    if icon == FmtIcon::Bold {
        let ts = ui.calc_text_size("B");
        dl.add_text(
            [p[0] + (side - ts[0]) * 0.5, p[1] + (side - ts[1]) * 0.5],
            color_u32(ink),
            "B",
        );
    } else {
        draw_fmt_icon(
            &dl,
            icon,
            [p[0] + side * 0.5, p[1] + side * 0.5],
            side * 0.62,
            ink,
        );
    }
    hit
}

fn draw_fmt_icon(dl: &DrawListMut, icon: FmtIcon, c: [f32; 2], size: f32, color: [f32; 4]) {
    let thick = (size / 9.0).max(1.2);
    let x0 = c[0] - size * 0.38;
    let x1 = c[0] + size * 0.38;
    match icon {
        FmtIcon::Normal => {
            for i in 0..3 {
                let y = c[1] - size * 0.22 + i as f32 * size * 0.22;
                let inset = if i == 2 { size * 0.16 } else { 0.0 };
                dl.add_line([x0, y], [x1 - inset, y], color)
                    .thickness(thick)
                    .build();
            }
        }
        FmtIcon::Bold => {
            let left = c[0] - size * 0.22;
            let r = size * 0.20;
            dl.add_line(
                [left, c[1] - size * 0.32],
                [left, c[1] + size * 0.32],
                color,
            )
            .thickness(thick * 1.8)
            .build();
            dl.add_circle([left + r, c[1] - size * 0.14], r * 0.95, color)
                .filled(true)
                .build();
            dl.add_circle([left + r, c[1] + size * 0.14], r, color)
                .filled(true)
                .build();
        }
        FmtIcon::Bullet => {
            for i in 0..3 {
                let y = c[1] - size * 0.24 + i as f32 * size * 0.24;
                dl.add_circle([x0 + size * 0.06, y], size * 0.07, color)
                    .filled(true)
                    .build();
                dl.add_line([x0 + size * 0.20, y], [x1, y], color)
                    .thickness(thick)
                    .build();
            }
        }
        FmtIcon::Number => {
            for i in 0..3 {
                let y = c[1] - size * 0.24 + i as f32 * size * 0.24;
                dl.add_line([x0, y], [x0 + size * 0.14, y], color)
                    .thickness(thick)
                    .build();
                dl.add_line([x0 + size * 0.22, y], [x1, y], color)
                    .thickness(thick)
                    .build();
            }
        }
        FmtIcon::Left => {
            for i in 0..3 {
                let y = c[1] - size * 0.22 + i as f32 * size * 0.22;
                let inset = if i == 1 { size * 0.18 } else { 0.0 };
                dl.add_line([x0, y], [x1 - inset, y], color)
                    .thickness(thick)
                    .build();
            }
        }
        FmtIcon::Center => {
            for i in 0..3 {
                let y = c[1] - size * 0.22 + i as f32 * size * 0.22;
                let inset = if i == 1 { size * 0.12 } else { 0.0 };
                dl.add_line([x0 + inset, y], [x1 - inset, y], color)
                    .thickness(thick)
                    .build();
            }
        }
        FmtIcon::Right => {
            for i in 0..3 {
                let y = c[1] - size * 0.22 + i as f32 * size * 0.22;
                let inset = if i == 1 { size * 0.18 } else { 0.0 };
                dl.add_line([x0 + inset, y], [x1, y], color)
                    .thickness(thick)
                    .build();
            }
        }
    }
}

fn color_swatch(ui: &Ui, id: &str, side: f32, on: bool, color: [f32; 4]) -> bool {
    let p = ui.cursor_screen_pos();
    let hit = ui.invisible_button(id, [side, side]);
    let dl = crate::ui::window_draw_list(ui);
    let pad = 3.0;
    dl.add_rect(
        [p[0] + pad, p[1] + pad],
        [p[0] + side - pad, p[1] + side - pad],
        color,
    )
    .filled(true)
    .rounding(2.0)
    .build();
    let rim = if on {
        theme::pal().gold
    } else {
        theme::with_alpha(theme::pal().chip_idle_rim, 0.7)
    };
    dl.add_rect(p, [p[0] + side, p[1] + side], rim)
        .rounding(theme::ICON_ROUNDING)
        .thickness(if on { 2.0 } else { 1.0 })
        .build();
    hit
}

fn format_toolbar(
    ui: &Ui,
    lines: &[RichLine],
    lo: usize,
    hi: usize,
    inner_w: f32,
) -> Option<FmtCmd> {
    ui.text_colored(theme::pal().muted, t("about.fmt.hint"));
    let side = theme::control_height(ui).max(26.0);
    let mut row_x = 0.0_f32;
    let gap = 4.0;
    let last = lines.len().saturating_sub(1);
    let lo = lo.min(last);
    let hi = hi.min(last).max(lo);
    let slice = if lines.is_empty() {
        return None;
    } else {
        &lines[lo..=hi]
    };
    let all_normal = slice.iter().all(|l| l.is_normal());
    let all_bold = slice.iter().all(|l| l.bold);
    let all_bullet = slice.iter().all(|l| l.list == List::Bullet);
    let all_number = slice.iter().all(|l| l.list == List::Number);
    let align = slice.first().map(|l| l.align).unwrap_or(Align::Left);
    let align_same = slice.iter().all(|l| l.align == align);
    let color = slice.first().map(|l| l.color).unwrap_or(Ink::Cream);
    let color_same = slice.iter().all(|l| l.color == color);
    let mut cmd = None;
    let icons: [(FmtIcon, &str, bool, FmtCmd); 7] = [
        (
            FmtIcon::Normal,
            "about.fmt.normal",
            all_normal,
            FmtCmd::Normal,
        ),
        (FmtIcon::Bold, "about.fmt.bold", all_bold, FmtCmd::Bold),
        (
            FmtIcon::Bullet,
            "about.fmt.bullet",
            all_bullet,
            FmtCmd::Bullet,
        ),
        (
            FmtIcon::Number,
            "about.fmt.number",
            all_number,
            FmtCmd::Number,
        ),
        (
            FmtIcon::Left,
            "about.fmt.left",
            align_same && align == Align::Left,
            FmtCmd::Align(Align::Left),
        ),
        (
            FmtIcon::Center,
            "about.fmt.center",
            align_same && align == Align::Center,
            FmtCmd::Align(Align::Center),
        ),
        (
            FmtIcon::Right,
            "about.fmt.right",
            align_same && align == Align::Right,
            FmtCmd::Align(Align::Right),
        ),
    ];
    for (i, (icon, key, on, next)) in icons.iter().enumerate() {
        theme::wrap_chip(ui, inner_w, &mut row_x, side, gap);
        if fmt_icon_button(ui, &format!("##wz_fmt_{i}"), side, *on, *icon) {
            cmd = Some(*next);
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(t(key));
        }
    }
    let inks = [Ink::Cream, Ink::Gold, Ink::Muted, Ink::Warn, Ink::Alert];
    for (i, ink) in inks.iter().enumerate() {
        theme::wrap_chip(ui, inner_w, &mut row_x, side, gap);
        if color_swatch(
            ui,
            &format!("##wz_ink_{i}"),
            side,
            color_same && color == *ink,
            ink_rgba(*ink),
        ) {
            cmd = Some(FmtCmd::Color(*ink));
        }
    }
    ui.dummy([0.0, 4.0]);
    cmd
}

/// The step's translated prompt, or the raw id when the frozen taxonomy does not define it.
fn step_prompt(taxonomy: &FeedbackTaxonomy, step_id: &str) -> String {
    taxonomy
        .step(step_id)
        .map_or_else(|| step_id.to_string(), |s| t(&s.prompt))
}

/// `about.missing` with the prompts of every required step that still lacks a value.
fn missing_text(draft: &Draft) -> String {
    let steps: Vec<String> = draft
        .missing_steps()
        .iter()
        .map(|id| step_prompt(&draft.taxonomy, id))
        .collect();
    tf("about.missing", &[("steps", &steps.join(", "))])
}

/// `about.too_big` with the measured request size and the `MAX_REQUEST_BYTES` cap.
fn too_big_text(bytes: usize) -> String {
    tf(
        "about.too_big",
        &[
            ("bytes", &bytes.to_string()),
            ("max", &MAX_REQUEST_BYTES.to_string()),
        ],
    )
}

/// Locale key for Choya's optional aside on a step.
fn quip_key(cat: &str, step: &str) -> String {
    format!("quip.{cat}.{step}")
}

/// Choya's aside for a step, only when the catalog actually has one.
fn quip_for(cat: &str, step: &str) -> Option<String> {
    let key = quip_key(cat, step);
    let text = t(&key);
    (text != key).then_some(text)
}

fn text_error_text(e: &TextError) -> String {
    match e {
        TextError::TooShort(min) => tf("about.too_short", &[("min", &min.to_string())]),
        TextError::TooLong(max) => tf("about.too_long", &[("max", &max.to_string())]),
    }
}

/// Why `Next` is dimmed on a step, or `None` when the step may be left. A text over
/// its limit blocks even on an optional step (mirrors `Draft::missing_steps`).
fn next_block_text(draft: &Draft, step_id: &str) -> Option<String> {
    if let Some(e) = draft.text_error(step_id) {
        return Some(text_error_text(&e));
    }
    if !draft.is_required(step_id) || draft.has_value(step_id) {
        return None;
    }
    Some(tf(
        "about.missing",
        &[("steps", &step_prompt(&draft.taxonomy, step_id))],
    ))
}

/// Player-facing text for a send failure (`msg.fail.*`). `Rejected` shows the
/// server's reason verbatim; `RateLimited` rounds the wait up to whole minutes.
pub(super) fn fail_text(r: &FailReason) -> String {
    match r {
        FailReason::Network => t("msg.fail.network"),
        FailReason::Server => t("msg.fail.server"),
        FailReason::Timeout => t("msg.fail.timeout"),
        FailReason::RateLimited { retry_after_secs } => tf(
            "msg.fail.rate",
            &[("n", &retry_after_secs.div_ceil(60).to_string())],
        ),
        FailReason::TooLarge => t("msg.fail.too_large"),
        FailReason::Rejected { reason } => reason.clone(),
        FailReason::TooOld => t("msg.fail.too_old"),
        FailReason::Interrupted => t("msg.fail.interrupted"),
    }
}

/// Rate-limit copy uses remaining minutes from the matching row; other reasons
/// stay on [`fail_text`].
fn fail_copy(r: &FailReason, draft: &Draft, feedback: &FeedbackState, now: u64) -> String {
    if matches!(r, FailReason::RateLimited { .. }) {
        if let Some(m) = feedback
            .messages
            .iter()
            .find(|m| m.report_id == draft.report_id)
        {
            return super::rate_fail_text(m, now);
        }
    }
    fail_text(r)
}

/// What the player did this frame; applied after every widget has rendered so
/// no `&Draft` is alive while the state is mutated.
enum Action {
    None,
    Pick(String),
    Choice(String, String),
    Text(String, String),
    Next,
    Back,
    Cancel,
    Coffee(String),
    Send,
    ToggleBuild(bool),
    ToggleAccount(bool),
    Contact(String),
    Done,
    SameAsLast,
}

fn fade(c: [f32; 4], alpha: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * alpha]
}

/// `text_wrapped` pinned to an absolute right edge (the plate's inner edge).
/// `PushTextWrapPos` is window-local, so the screen-space edge is converted first.
fn wrapped_to(ui: &Ui, color: [f32; 4], text: &str, right_x: f32) {
    let local = super::wrap_pos_local(right_x, ui.window_pos()[0], ui.scroll_x());
    let wrap = ui.push_text_wrap_pos_with_pos(local);
    ui.text_colored(color, text);
    wrap.pop(ui);
}

/// Gold button drawn inert at 40 % with a tooltip saying why.
fn dimmed_gold(ui: &Ui, label: impl AsRef<str>, tip: &str) {
    let style = ui.push_style_var(StyleVar::Alpha(0.4));
    theme::gold_button_sized(ui, label, [0.0, 0.0]);
    style.pop();
    if ui.is_item_hovered() {
        ui.tooltip_text(tip);
    }
}

fn done_button(ui: &Ui) -> Action {
    ui.dummy([0.0, 6.0]);
    if theme::gold_button_sized(ui, format!("{}##wz_done", t("about.btn.done")), [0.0, 0.0]) {
        Action::Done
    } else {
        Action::None
    }
}

/// Render the open draft inside a plate under the action row. No-op without a draft.
pub(super) fn render_wizard(ui: &Ui, state: &mut AddonState) {
    let feedback = &state.main.feedback;
    let Some(draft) = feedback.draft.as_ref() else {
        return;
    };
    ui.dummy([0.0, 10.0]);
    let origin = ui.cursor_screen_pos();
    let width = ui.content_region_avail()[0];
    let min_h = AVATAR + PAD * 2.0;
    let plate_h = f32::from_bits(PLATE_H.load(Ordering::Relaxed)).max(min_h);
    {
        let dl = crate::ui::window_draw_list(ui);
        let br = [origin[0] + width, origin[1] + plate_h];
        dl.add_rect(origin, br, theme::pal().plate)
            .filled(true)
            .rounding(ROUNDING)
            .build();
        dl.add_rect(origin, br, theme::pal().gold_dim)
            .rounding(ROUNDING)
            .build();
    }
    theme::draw_choya_avatar(
        ui,
        [
            origin[0] + PAD + AVATAR * 0.5,
            origin[1] + PAD + AVATAR * 0.5,
        ],
        AVATAR,
    );

    let indent = PAD + AVATAR + 10.0;
    let inner_w = (width - indent - PAD).max(80.0);
    let right_x = origin[0] + width - PAD;
    ui.indent_by(indent);
    ui.set_cursor_screen_pos([origin[0] + indent, origin[1] + PAD + 4.0]);

    let action = match draft.step.clone() {
        WizardStep::Pick => render_pick(ui, draft, feedback, inner_w),
        WizardStep::Step(i) => render_step(ui, draft, i, inner_w),
        WizardStep::Summary => {
            // Measured every summary frame so the Send gate tracks the boxes and contact.
            let bytes = crate::feedback::tasks::draft_request_bytes(state);
            render_summary(ui, draft, feedback, bytes, inner_w, right_x)
        }
        WizardStep::Sending => {
            ui.text_colored(theme::pal().muted, t("msg.status.sending"));
            Action::None
        }
        WizardStep::Sent { short_id } => {
            ui.text_colored(
                theme::OPTIMIZED,
                tf("about.sent_plate", &[("id", &short_id)]),
            );
            done_button(ui)
        }
        WizardStep::Thanks => {
            ui.text_colored(theme::pal().gold, t("about.thanks"));
            done_button(ui)
        }
    };

    ui.unindent_by(indent);
    let measured = (ui.cursor_screen_pos()[1] + PAD - origin[1]).max(min_h);
    PLATE_H.store(measured.to_bits(), Ordering::Relaxed);
    ui.set_cursor_screen_pos([origin[0], origin[1] + measured.max(plate_h)]);

    apply(state, action);
}

fn render_pick(ui: &Ui, draft: &Draft, feedback: &FeedbackState, inner_w: f32) -> Action {
    ui.text_colored(theme::pal().cream, t("step.pick"));
    ui.dummy([0.0, 4.0]);

    let cats = &draft.taxonomy.categories;
    let labels: Vec<String> = cats.iter().map(|c| t(&c.label)).collect();
    let widest = labels
        .iter()
        .map(|l| ui.calc_text_size(l)[0])
        .fold(0.0, f32::max);
    let tile_w = (widest + 44.0).max(150.0);
    let tile_h = theme::control_height(ui) + 12.0;

    let mut action = Action::None;
    let mut row_x = 0.0;
    for (cat, label) in cats.iter().zip(&labels) {
        theme::wrap_chip(ui, inner_w, &mut row_x, tile_w, GAP);
        let kind = cat.kind.as_str();
        let live = matches!(kind, "report" | "link");
        // Inert tiles are dimmed by `fade` on every draw-list colour below; the
        // invisible button has no style colours of its own.
        let alpha = if live { 1.0 } else { 0.4 };
        let p = ui.cursor_screen_pos();
        let clicked = ui.invisible_button(format!("##wz_tile_{}", cat.id), [tile_w, tile_h]);
        let hovered = ui.is_item_hovered();
        {
            let dl = crate::ui::window_draw_list(ui);
            let br = [p[0] + tile_w, p[1] + tile_h];
            let fill = if hovered && live {
                theme::pal().gold_hover
            } else {
                theme::with_alpha(theme::pal().plate, 0.9)
            };
            dl.add_rect(p, br, fade(fill, alpha))
                .filled(true)
                .rounding(ROUNDING)
                .build();
            dl.add_rect(p, br, fade(theme::pal().gold_dim, alpha))
                .rounding(ROUNDING)
                .build();
            draw_glyph(
                ui,
                &dl,
                &cat.icon,
                [p[0] + 17.0, p[1] + tile_h * 0.5],
                18.0,
                fade(category_color(&cat.color), alpha),
            );
            let th = ui.calc_text_size(label)[1];
            dl.add_text(
                [p[0] + 36.0, p[1] + ((tile_h - th) * 0.5).round()],
                color_u32(fade(theme::pal().cream, alpha)),
                label,
            );
        }
        match kind {
            "report" => {
                if clicked {
                    action = Action::Pick(cat.id.clone());
                }
            }
            "link" => {
                if clicked {
                    if let Some(url) = &cat.url {
                        action = Action::Coffee(url.clone());
                    }
                }
            }
            _ => {
                if hovered {
                    ui.tooltip_text(t("about.needs_newer"));
                }
            }
        }
    }

    ui.dummy([0.0, 4.0]);
    if let Some(last) = feedback
        .last_path
        .as_ref()
        .filter(|l| draft.taxonomy.category(&l.category).is_some())
    {
        let label = tf(
            "about.same_as_last",
            &[(
                "path",
                &path_label(&draft.taxonomy, &last.category, &last.path),
            )],
        );
        if theme::select_chip(ui, &label, false, "##wz_same_as_last", None) {
            action = Action::SameAsLast;
        }
        ui.same_line_with_spacing(0.0, GAP);
    }
    if ui.button(format!("{}##wz_cancel", t("about.btn.cancel"))) {
        action = Action::Cancel;
    }
    action
}

fn render_step(ui: &Ui, draft: &Draft, i: usize, inner_w: f32) -> Action {
    let step_id = draft.step_id(i).unwrap_or_default().to_string();
    let step = draft.taxonomy.step(&step_id).cloned();
    let cat_id = draft.category.as_deref().unwrap_or_default();

    ui.text_colored(theme::pal().cream, step_prompt(&draft.taxonomy, &step_id));
    if let Some(quip) = quip_for(cat_id, &step_id) {
        ui.text_colored(theme::pal().muted, quip);
    }
    ui.dummy([0.0, 4.0]);

    let mut action = Action::None;
    if let Some(step) = &step {
        if !step.choices.is_empty() {
            let selected = draft.choices.get(&step_id).map(String::as_str);
            let mut row_x = 0.0;
            for c in &step.choices {
                let label = t(&format!("choice.{c}"));
                let w = theme::select_chip_size(ui, &label, false)[0];
                theme::wrap_chip(ui, inner_w, &mut row_x, w, GAP);
                if theme::select_chip(
                    ui,
                    &label,
                    selected == Some(c.as_str()),
                    &format!("##wz_{step_id}_{c}"),
                    None,
                ) {
                    action = Action::Choice(step_id.clone(), c.clone());
                }
            }
        }
        if let Some(rule) = &step.text {
            let stored = draft.texts.get(&step_id).cloned().unwrap_or_default();
            let mut lines = decode_lines(&stored);
            let fp = step_fingerprint(&step_id);
            if EDITOR_STEP.swap(fp, Ordering::Relaxed) != fp {
                SEL_LO.store(0, Ordering::Relaxed);
                SEL_HI.store(0, Ordering::Relaxed);
                PENDING_BOLD.store(false, Ordering::Relaxed);
            }
            let plain_now = plain_from_lines(&lines);
            let (lo, hi) = selection_lines(
                &plain_now,
                SEL_LO.load(Ordering::Relaxed) as usize,
                SEL_HI.load(Ordering::Relaxed) as usize,
            );
            if let Some(cmd) = format_toolbar(ui, &lines, lo, hi, inner_w) {
                apply_fmt(&mut lines, lo, hi, cmd);
            }
            let mut plain = plain_from_lines(&lines);
            let editor_h = ui.text_line_height() * 8.0 + 10.0;
            ui.input_text_multiline(
                format!("##wz_body_{step_id}"),
                &mut plain,
                [inner_w, editor_h],
            )
            .callback(
                InputTextMultilineCallback::ALWAYS | InputTextMultilineCallback::CHAR_FILTER,
                MailSel,
            )
            .build();
            lines = resync_lines(&lines, &plain);
            if PENDING_BOLD.swap(false, Ordering::Relaxed) {
                let (lo, hi) = selection_lines(
                    &plain,
                    SEL_LO.load(Ordering::Relaxed) as usize,
                    SEL_HI.load(Ordering::Relaxed) as usize,
                );
                apply_fmt(&mut lines, lo, hi, FmtCmd::Bold);
            }
            let buf = encode_lines(&lines);
            if buf != stored {
                action = Action::Text(step_id.clone(), buf.clone());
            }
            ui.text_colored(
                theme::pal().muted,
                format!("{}/{}", buf.chars().count(), rule.max),
            );
            if let Some(e) = draft.text_error(&step_id) {
                ui.text_colored(theme::WARN, text_error_text(&e));
            }
            if !buf.trim().is_empty() {
                ui.dummy([0.0, 6.0]);
                ui.text_colored(theme::pal().muted, t("about.fmt.preview"));
                paint_mail(ui, &buf, inner_w);
            }
        }
    }

    ui.dummy([0.0, 6.0]);
    if ui.button(format!("{}##wz_back", t("about.btn.back"))) {
        action = Action::Back;
    }
    ui.same_line_with_spacing(0.0, GAP);
    ui.align_text_to_frame_padding();
    let n = draft.current_index().unwrap_or(i + 1);
    ui.text_colored(
        theme::pal().muted,
        tf(
            "about.step_n",
            &[
                ("n", &n.to_string()),
                ("m", &draft.total_steps().to_string()),
            ],
        ),
    );
    ui.same_line_with_spacing(0.0, GAP);
    let next_label = format!("{}##wz_next", t("about.btn.next"));
    match next_block_text(draft, &step_id) {
        Some(tip) => dimmed_gold(ui, next_label, &tip),
        None => {
            if theme::gold_button_sized(ui, next_label, [0.0, 0.0]) {
                action = Action::Next;
            }
        }
    }
    action
}

fn render_summary(
    ui: &Ui,
    draft: &Draft,
    feedback: &FeedbackState,
    request_bytes: Option<usize>,
    inner_w: f32,
    right_x: f32,
) -> Action {
    let mut action = Action::None;
    let cat = draft.category().cloned();

    let p = ui.cursor_screen_pos();
    if let Some(cat) = &cat {
        let dl = crate::ui::window_draw_list(ui);
        draw_glyph(
            ui,
            &dl,
            &cat.icon,
            [p[0] + 8.0, p[1] + ui.text_line_height() * 0.5],
            16.0,
            category_color(&cat.color),
        );
    }
    ui.set_cursor_screen_pos([p[0] + 22.0, p[1]]);
    ui.text_colored(theme::pal().cream, path_text(draft));
    let encoded = draft.encoded_body();
    if !encoded.is_empty() {
        let w = (right_x - ui.cursor_screen_pos()[0]).max(40.0);
        paint_mail(ui, &encoded, w);
    }

    ui.dummy([0.0, 4.0]);
    ui.text_colored(theme::pal().muted, t("about.attached"));
    ui.same_line_with_spacing(0.0, GAP);
    ui.text_colored(
        theme::pal().cream,
        format!("v{}  ·  {}", crate::VERSION, gw2_core::i18n::current()),
    );

    if cat.as_ref().is_some_and(|c| c.attach_build) {
        let label = format!("{}##wz_build", t("about.include_build"));
        let fits = feedback
            .snapshot
            .as_ref()
            .is_some_and(|s| snapshot_bytes(s) <= MAX_SNAPSHOT_BYTES);
        if fits {
            let mut v = draft.include_build;
            if ui.checkbox(label, &mut v) {
                action = Action::ToggleBuild(v);
            }
        } else {
            let style = ui.push_style_var(StyleVar::Alpha(0.4));
            let mut off = false;
            ui.checkbox(label, &mut off);
            style.pop();
            if ui.is_item_hovered() {
                ui.tooltip_text(t("about.include_build_big"));
            }
        }
    }

    if feedback.account != Some(Err(())) {
        let mut v = draft.include_account;
        if ui.checkbox(
            format!("{}##wz_account", t("about.include_account")),
            &mut v,
        ) {
            action = Action::ToggleAccount(v);
        }
        if feedback.account_looking_up {
            ui.same_line_with_spacing(0.0, GAP);
            ui.text_colored(theme::pal().muted, t("about.account_lookup"));
        } else if let Some(Ok(name)) = &feedback.account {
            ui.same_line_with_spacing(0.0, GAP);
            ui.text_colored(theme::pal().cream, name);
        }
    }

    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("about.reach_me"));
    ui.same_line_with_spacing(0.0, GAP);
    let mut contact = draft.contact.clone();
    ui.set_next_item_width((inner_w * 0.6).max(120.0));
    ui.input_text("##wz_contact", &mut contact)
        .hint(&t("about.reach_hint"))
        .build();
    if contact.chars().count() > CONTACT_MAX {
        contact = contact.chars().take(CONTACT_MAX).collect();
    }
    if contact != draft.contact {
        action = Action::Contact(contact);
    }

    if let Some(err) = &draft.error {
        ui.dummy([0.0, 4.0]);
        wrapped_to(
            ui,
            theme::ERR,
            &fail_copy(err, draft, feedback, now_unix()),
            right_x,
        );
    }

    ui.dummy([0.0, 6.0]);
    if ui.button(format!("{}##wz_back", t("about.btn.back"))) {
        action = Action::Back;
    }
    ui.same_line_with_spacing(0.0, GAP);
    if ui.button(format!("{}##wz_cancel", t("about.btn.cancel"))) {
        action = Action::Cancel;
    }
    ui.same_line_with_spacing(0.0, GAP);
    let send_label = format!("{}##wz_send", t("about.btn.send"));
    // The box says the account name goes along; never ship `account: null` under it.
    let account_pending = draft.include_account && feedback.account_looking_up;
    if !draft.can_send() {
        dimmed_gold(ui, send_label, &missing_text(draft));
    } else if let Some(bytes) = request_bytes.filter(|b| *b > MAX_REQUEST_BYTES) {
        dimmed_gold(ui, send_label, &too_big_text(bytes));
    } else if account_pending {
        dimmed_gold(ui, send_label, &t("about.account_lookup"));
    } else if theme::gold_button_sized(ui, send_label, [0.0, 0.0]) {
        action = Action::Send;
    }
    action
}

// apply phase

/// Run `f` on the open draft; `R::default()` when none is open.
fn with_draft<R: Default>(state: &mut AddonState, f: impl FnOnce(&mut Draft) -> R) -> R {
    state
        .main
        .feedback
        .draft
        .as_mut()
        .map_or_else(R::default, f)
}

fn apply(state: &mut AddonState, action: Action) {
    match action {
        Action::None => {}
        Action::Pick(id) => {
            let summary = with_draft(state, |d| {
                d.pick(&id);
                d.step == WizardStep::Summary
            });
            if summary {
                on_enter_summary(state);
            }
        }
        Action::Choice(step_id, choice) => {
            let summary = with_draft(state, |d| {
                d.set_choice(&step_id, &choice);
                d.next();
                d.step == WizardStep::Summary
            });
            if summary {
                on_enter_summary(state);
            }
        }
        Action::Text(step_id, text) => with_draft(state, |d| d.set_text(&step_id, text)),
        Action::Next => {
            let summary = with_draft(state, |d| {
                d.next();
                d.step == WizardStep::Summary
            });
            if summary {
                on_enter_summary(state);
            }
        }
        Action::Back => with_draft(state, |d| d.back()),
        Action::Cancel | Action::Done => state.main.feedback.close_draft(),
        Action::SameAsLast => {
            let feedback = &mut state.main.feedback;
            let Some(last) = feedback.last_path.clone() else {
                return;
            };
            let Some(taxonomy) = feedback.draft.as_ref().map(|d| d.taxonomy.clone()) else {
                return;
            };
            let draft = Draft::from_last_path(taxonomy, &last);
            let summary = draft.step == WizardStep::Summary;
            feedback.draft = Some(draft);
            if summary {
                on_enter_summary(state);
            }
        }
        Action::Coffee(url) => coffee(state, &url),
        Action::Send => on_send(state),
        Action::ToggleBuild(v) => with_draft(state, |d| d.include_build = v),
        Action::ToggleAccount(v) => {
            with_draft(state, |d| d.include_account = v);
            if v {
                on_account_tick(state);
            }
        }
        Action::Contact(contact) => with_draft(state, |d| d.contact = contact),
    }
}

// hooks: state-side handlers behind the `apply` arms

/// Send the draft: row, lock, background post (`feedback::tasks::send_draft`).
pub(super) fn on_send(state: &mut AddonState) {
    crate::feedback::tasks::send_draft(state);
}

/// The wizard just landed on the summary step: rebuild the attachable snapshot
/// from the selected suggestion. Split borrow: both are fields of `state.main`.
pub(super) fn on_enter_summary(state: &mut AddonState) {
    let (feedback, comparison) = (&mut state.main.feedback, &state.main.comparison);
    feedback.refresh_snapshot(comparison);
}

/// The player ticked "include my GW2 account name".
pub(super) fn on_account_tick(state: &mut AddonState) {
    crate::feedback::tasks::lookup_account(state);
}

/// Open the Ko-fi page and remember it as a local `about.coffee_row` message.
/// No server call, no dialog on failure (a Warning log only). A draft still on
/// the pick step closes; anything further along is left alone.
pub(super) fn coffee(state: &mut AddonState, url: &str) {
    if !crate::feedback::shell::open_url(url) {
        nexus::log::log(
            nexus::log::LogLevel::Warning,
            "GW2BuildOpt",
            format!("could not open {url}"),
        );
    }
    let row = LocalMessage {
        report_id: uuid::Uuid::new_v4().to_string(),
        short_id: None,
        sent_at: now_unix(),
        category: "coffee".to_string(),
        path: vec![],
        title: t("about.coffee_row"),
        body: String::new(),
        status: MessageStatus::Local,
        reply: None,
        replied_at: None,
        closing_note: None,
        last_error: None,
        failed_at: None,
        failed_payload: None,
        context_summary: String::new(),
    };
    let feedback = &mut state.main.feedback;
    feedback.messages.insert(0, row);
    feedback.dirty = true;
    if feedback
        .draft
        .as_ref()
        .is_some_and(|d| d.step == WizardStep::Pick)
    {
        feedback.close_draft();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_language` is process-global and `state::init` calls it too, so the
    /// tests that depend on `en` serialise on the shared STATE test lock.
    fn with_en<R>(f: impl FnOnce() -> R) -> R {
        let _g = crate::state::state_test_guard();
        gw2_core::i18n::set_language("en");
        f()
    }

    fn bug_draft() -> Draft {
        let mut d = Draft::new(FeedbackTaxonomy::embedded());
        d.pick("bug");
        d
    }

    #[test]
    fn path_text_joins_category_and_choice_labels() {
        with_en(|| {
            let mut d = bug_draft();
            assert_eq!(path_text(&d), "Report a bug");
            d.set_choice("area_screen", "optimize");
            d.set_choice("severity", "wrong");
            assert_eq!(path_text(&d), "Report a bug › Optimize › Wrong result");

            let none = Draft::new(FeedbackTaxonomy::embedded());
            assert_eq!(path_text(&none), "");
        });
    }

    #[test]
    fn mail_roundtrip_keeps_bold_color_align_and_list() {
        let line = RichLine {
            text: "Hello".into(),
            bold: true,
            align: Align::Center,
            color: Ink::Gold,
            list: List::Bullet,
        };
        let encoded = encode_lines(std::slice::from_ref(&line));
        assert_eq!(encoded, "%BC1U|Hello");
        assert_eq!(decode_lines(&encoded), vec![line]);
    }

    #[test]
    fn mail_reads_plain_text_and_old_markdown() {
        assert_eq!(decode_line("plain").text, "plain");
        assert!(!decode_line("plain").bold);
        assert_eq!(decode_line("- a").list, List::Bullet);
        assert_eq!(decode_line("2. b").list, List::Number);
        let title = decode_line("# Title");
        assert!(title.bold);
        assert_eq!(title.color, Ink::Gold);
        assert_eq!(title.text, "Title");
    }

    #[test]
    fn encode_keeps_blank_middle_line() {
        let lines = vec![
            RichLine {
                text: "a".into(),
                ..RichLine::default()
            },
            RichLine::default(),
            RichLine {
                text: "b".into(),
                ..RichLine::default()
            },
        ];
        let encoded = encode_lines(&lines);
        assert_eq!(encoded, "%NL0P|a\n%NL0P|\n%NL0P|b");
        assert_eq!(decode_lines(&encoded), lines);
    }

    #[test]
    fn resync_split_keeps_style_on_both_halves() {
        let old = vec![RichLine {
            text: "hello".into(),
            bold: true,
            color: Ink::Gold,
            ..RichLine::default()
        }];
        let out = resync_lines(&old, "hel\nlo");
        assert_eq!(out.len(), 2);
        assert!(out[0].bold && out[1].bold);
        assert_eq!(out[0].color, Ink::Gold);
        assert_eq!(out[1].color, Ink::Gold);
        assert_eq!(out[0].text, "hel");
        assert_eq!(out[1].text, "lo");
    }

    #[test]
    fn resync_merge_keeps_first_style() {
        let old = vec![
            RichLine {
                text: "hel".into(),
                bold: true,
                ..RichLine::default()
            },
            RichLine {
                text: "lo".into(),
                color: Ink::Alert,
                ..RichLine::default()
            },
        ];
        let out = resync_lines(&old, "hello");
        assert_eq!(out.len(), 1);
        assert!(out[0].bold);
        assert_eq!(out[0].color, Ink::Cream);
        assert_eq!(out[0].text, "hello");
    }

    #[test]
    fn selection_lines_covers_byte_span() {
        let s = "ab\ncd\nef";
        assert_eq!(selection_lines(s, 0, 0), (0, 0));
        assert_eq!(selection_lines(s, 0, 2), (0, 0));
        assert_eq!(selection_lines(s, 1, 5), (0, 1));
        assert_eq!(selection_lines(s, 3, 8), (1, 2));
    }

    #[test]
    fn apply_fmt_toggles_bold_on_range() {
        let mut lines = vec![
            RichLine {
                text: "a".into(),
                ..RichLine::default()
            },
            RichLine {
                text: "b".into(),
                ..RichLine::default()
            },
            RichLine {
                text: "c".into(),
                ..RichLine::default()
            },
        ];
        apply_fmt(&mut lines, 1, 2, FmtCmd::Bold);
        assert!(!lines[0].bold);
        assert!(lines[1].bold && lines[2].bold);
        apply_fmt(&mut lines, 1, 2, FmtCmd::Bold);
        assert!(!lines[1].bold && !lines[2].bold);
        apply_fmt(&mut lines, 0, 0, FmtCmd::Color(Ink::Gold));
        assert_eq!(lines[0].color, Ink::Gold);
    }

    #[test]
    fn path_label_falls_back_to_raw_ids() {
        with_en(|| {
            let tax = FeedbackTaxonomy::embedded();
            assert_eq!(
                path_label(&tax, "vote", &["rocket".to_string()]),
                "vote › choice.rocket"
            );
        });
    }

    #[test]
    fn missing_text_lists_step_prompts_in_order() {
        with_en(|| {
            let mut d = bug_draft();
            assert_eq!(
                missing_text(&d),
                "Missing: Where did it happen?, How bad is it?, Tell Choya what happened"
            );
            d.set_choice("area_screen", "optimize");
            d.set_choice("severity", "wrong");
            d.set_text("describe", "Optimize picks Trident on land.".into());
            assert_eq!(missing_text(&d), "Missing: ");
        });
    }

    #[test]
    fn too_big_text_carries_bytes_and_cap() {
        with_en(|| {
            let text = too_big_text(17_234);
            assert!(text.contains("17234"), "{text}");
            assert!(text.contains(&MAX_REQUEST_BYTES.to_string()), "{text}");
            assert!(!text.contains('{'), "{text}");
        });
    }

    #[test]
    fn quip_key_shape() {
        assert_eq!(quip_key("bug", "area_screen"), "quip.bug.area_screen");
    }

    #[test]
    fn quip_for_is_none_without_a_catalog_entry() {
        with_en(|| {
            // The English catalog ships no quips yet: `t` echoes the key back.
            let key = quip_key("bug", "area_screen");
            assert_eq!(t(&key), key);
            assert_eq!(quip_for("bug", "area_screen"), None);
            // Sanity: the same `t(key) != key` rule does resolve a key the catalog has.
            assert_ne!(t("step.pick"), "step.pick");
        });
    }

    #[test]
    fn next_block_text_only_when_required_and_empty() {
        with_en(|| {
            let mut d = bug_draft();
            assert_eq!(
                next_block_text(&d, "severity").as_deref(),
                Some("Missing: How bad is it?")
            );
            assert_eq!(
                next_block_text(&d, "describe").as_deref(),
                Some("At least 10 characters")
            );
            d.set_choice("severity", "wrong");
            assert_eq!(next_block_text(&d, "severity"), None);
            d.set_text("describe", "x".repeat(4001));
            assert_eq!(
                next_block_text(&d, "describe").as_deref(),
                Some("At most 4000 characters")
            );
            let mut praise = Draft::new(FeedbackTaxonomy::embedded());
            praise.pick("praise");
            assert_eq!(next_block_text(&praise, "note_optional"), None);
        });
    }

    #[test]
    fn next_blocked_when_optional_note_too_long() {
        with_en(|| {
            let mut praise = Draft::new(FeedbackTaxonomy::embedded());
            praise.pick("praise");
            praise.set_text("note_optional", "x".repeat(1001));
            assert_eq!(
                next_block_text(&praise, "note_optional").as_deref(),
                Some("At most 1000 characters")
            );
            praise.set_text("note_optional", "x".repeat(1000));
            assert_eq!(next_block_text(&praise, "note_optional"), None);
        });
    }

    #[test]
    fn fail_text_maps_reasons() {
        with_en(|| {
            assert_eq!(
                fail_text(&FailReason::Network),
                "Couldn't reach Choya. Check your connection."
            );
            assert_eq!(
                fail_text(&FailReason::RateLimited {
                    retry_after_secs: 61
                }),
                "Slow down - try again in 2 min."
            );
            assert_eq!(
                fail_text(&FailReason::RateLimited {
                    retry_after_secs: 120
                }),
                "Slow down - try again in 2 min."
            );
            assert_eq!(
                fail_text(&FailReason::Rejected {
                    reason: "bad schema".into()
                }),
                "bad schema"
            );
            assert_eq!(fail_text(&FailReason::Interrupted), "Interrupted");
        });
    }

    #[test]
    fn fail_text_maps_all_seven_reasons() {
        with_en(|| {
            let cases = [
                (
                    FailReason::Network,
                    "Couldn't reach Choya. Check your connection.",
                ),
                (
                    FailReason::Server,
                    "Choya's mailbox is down. Your message is saved - try again later.",
                ),
                (FailReason::Timeout, "Took too long. Saved - try again."),
                (
                    FailReason::RateLimited {
                        retry_after_secs: 90,
                    },
                    "Slow down - try again in 2 min.",
                ),
                (FailReason::TooLarge, "Message too long (limit 4000)."),
                (
                    FailReason::Rejected {
                        reason: "bad schema".into(),
                    },
                    "bad schema",
                ),
                (FailReason::TooOld, "Update the addon to send messages."),
                (FailReason::Interrupted, "Interrupted"),
            ];
            for (reason, expected) in cases {
                let text = fail_text(&reason);
                assert_eq!(text, expected, "{reason:?}");
                assert!(!text.contains("msg.fail."), "{reason:?} leaked its key");
            }
        });
    }

    #[test]
    fn wizard_rate_copy_uses_minutes_left() {
        with_en(|| {
            let mut draft = Draft::new(FeedbackTaxonomy::embedded());
            draft.pick("bug");
            let reason = FailReason::RateLimited {
                retry_after_secs: 90,
            };
            draft.error = Some(reason.clone());
            let row = LocalMessage {
                report_id: draft.report_id.clone(),
                short_id: None,
                sent_at: 0,
                category: "bug".into(),
                path: vec![],
                title: "t".into(),
                body: "b".into(),
                status: MessageStatus::Failed,
                reply: None,
                replied_at: None,
                closing_note: None,
                last_error: Some(reason.clone()),
                failed_at: Some(1_000),
                failed_payload: None,
                context_summary: String::new(),
            };
            let feedback = FeedbackState {
                messages: vec![row],
                ..FeedbackState::default()
            };
            assert_eq!(
                fail_copy(&reason, &draft, &feedback, 1_000),
                "Slow down - try again in 2 min."
            );
            assert_eq!(
                fail_copy(&reason, &draft, &feedback, 1_030),
                "Slow down - try again in 1 min."
            );
        });
    }
}
