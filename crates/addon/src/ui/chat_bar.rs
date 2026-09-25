//! Bubble chat: player on the right, animated Choya on the left.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use nexus::imgui::{ChildWindow, InputTextFlags, StyleColor, StyleVar, Ui};
use serde::{Deserialize, Serialize};

use crate::chat_links::ChatChip;
use crate::ui::chat_markup::{self, Line, SpanStyle};
use crate::ui::{color_u32, fonts, icons, theme};
use gw2_core::i18n::{t, tf};

/// State for the talk-tab transcript.
#[derive(Clone, Default)]
pub struct ChatBarState {
    pub input: String,
    pub history: Vec<ChatMessage>,
    /// Lowercased names the game data knows, for the accent pass in bubbles.
    /// Filled once the game database is loaded (`chat_markup::name_index`).
    pub names: Option<Arc<HashSet<String>>>,
    pub waiting: bool,
    pub copied_code: Option<String>,
    pub copied_frames: u32,
    pub scroll_to_end: bool,
    pub dirty: bool,
    /// Last keystroke in the composer. Bob while this is recent; otherwise sleep.
    pub last_typed: Option<std::time::Instant>,
    /// Header idle pose (0..HEADER_POSE_COUNT). Cycles about once a minute.
    pub header_pose: u8,
    pub header_pose_at: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub from_user: bool,
    pub text: String,
    pub chips: Vec<ChatChip>,
    /// Clickable "Build is ready" card under this reply.
    #[serde(default)]
    pub open_result: bool,
    /// This reply was meant to be a build and is not one.
    ///
    /// Recorded rather than inferred from the absence of a build card: a
    /// greeting has no build card either, and offering somebody else's raid
    /// build in answer to "hello" is not help. Only a reply that tried and
    /// failed earns the community cards.
    #[serde(default)]
    pub build_failed: bool,
    /// Partial output kept after Stop or a failure; shows the Retry button.
    #[serde(default)]
    pub stopped: bool,
    /// The player message to re-send on Retry.
    #[serde(default)]
    pub retry_of: Option<String>,
}

pub enum ChatAction {
    Send(String),
    OpenBuild,
    /// A published build offered beside ours was chosen, by index.
    OpenPick(usize),
    /// The thinking bubble was clicked: expand or collapse it.
    ToggleLive,
    /// Stop the request in flight, keeping what arrived.
    Stop,
    /// Ask the model to continue the reply at this index.
    Retry(usize),
}

/// What the thinking bubble shows this frame, snapshotted by the caller from
/// the live output so the bubble never holds that lock while drawing.
#[derive(Debug, Clone, Default)]
pub struct LiveView {
    /// `{step} · mm:ss`
    pub line: String,
    /// "nothing for N s", when stalled.
    pub stall: Option<String>,
    /// One extra line (a retry that starts over).
    pub note: String,
    pub expanded: bool,
    /// Caption over the body: Thinking / Answer so far / Tools called.
    pub caption: String,
    /// The newest lines of the mode's text.
    pub body: Vec<String>,
    /// The newest lines of the run's step feed.
    pub steps: Vec<String>,
}

/// One community build, as much of it as a card beside the reply can show.
///
/// Display strings rather than the row itself: this module draws a chat and
/// knows nothing about benchmarks, and it should stay that way.
#[derive(Debug, Clone, Default)]
pub struct PickCard {
    /// Elite specialization, or the profession when the site did not name one.
    pub title: String,
    /// The site, lowercased as it is stored — `guildjen`, `hardstuck`.
    pub source: String,
    /// The job, and the stat prefix when there is one.
    pub detail: String,
}

/// Maximum chat history entries retained. Beyond this, the oldest entries are
/// dropped on append so a long-running session can't grow the Vec without bound.
const CHAT_HISTORY_CAP: usize = 100;

const AVATAR: f32 = 42.0;
const AVATAR_GAP: f32 = 10.0;
const COPY: f32 = 16.0;
const COPY_GAP: f32 = 6.0;
const BUBBLE_PAD: f32 = 10.0;
const BUBBLE_ROUND: f32 = 14.0;
const COMPOSER_H: f32 = 76.0;
const COMPOSER_CHOYA: f32 = 56.0;
const SEND_SZ: f32 = 36.0;
const ROW_GAP: f32 = 12.0;

fn trim_history(history: &mut Vec<ChatMessage>) {
    if history.len() > CHAT_HISTORY_CAP {
        let drop = history.len() - CHAT_HISTORY_CAP;
        history.drain(..drop);
    }
}

/// Push a player line and return it for `send_chat_message`.
pub fn queue_user_message(state: &mut ChatBarState, msg: &str) -> Option<String> {
    let msg = msg.trim();
    if msg.is_empty() {
        return None;
    }
    state.history.push(ChatMessage {
        from_user: true,
        text: msg.to_string(),
        ..Default::default()
    });
    trim_history(&mut state.history);
    state.input.clear();
    state.scroll_to_end = true;
    state.dirty = true;
    Some(msg.to_string())
}

/// The reply the published cards sit under: the newest one that plated a
/// build or tried to. Follow-up questions do not move it (specs/006 US3).
pub fn cards_anchor(history: &[ChatMessage]) -> Option<usize> {
    history
        .iter()
        .rposition(|m| !m.from_user && (m.open_result || m.build_failed))
}

/// Last `n` turns for the LLM brief. Oldest first.
pub fn recent_transcript(history: &[ChatMessage], n: usize) -> String {
    let start = history.len().saturating_sub(n);
    history[start..]
        .iter()
        .map(|m| {
            let who = if m.from_user { "Player" } else { "Assistant" };
            let text: String = m.text.chars().take(240).collect();
            format!("{who}: {text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse and wrap a reply to the bubble width. Runs every frame per visible
/// bubble, as the plain word-wrap did before it.
// ponytail: no layout cache; add one keyed by (text hash, width) if a long
// transcript ever shows up in a frame profile.
fn wrap_bubble(ui: &Ui, text: &str, max_w: f32, names: &HashSet<String>) -> (Vec<Line>, f32, f32) {
    let known = |n: &str| names.contains(&n.to_lowercase());
    let blocks = chat_markup::parse(text, &known);
    let scale = theme::ui_scale();
    let measure = |s: &str| ui.calc_text_size(s)[0];
    let lines = chat_markup::wrap_spans(&measure, &blocks, max_w / scale);
    let line_h = ui.calc_text_size("Ag")[1];
    let text_w = lines
        .iter()
        .map(|l| l.width * scale)
        .fold(0.0f32, f32::max)
        .min(max_w);
    let text_h = (lines.len() as f32 * line_h).max(line_h);
    (lines, text_w, text_h)
}

fn bubble_size(
    ui: &Ui,
    text: &str,
    avail: f32,
    from_user: bool,
    names: &HashSet<String>,
) -> (Vec<Line>, f32, f32) {
    let copy_slot = if from_user { COPY + COPY_GAP } else { 0.0 };
    let max_text = ((avail - AVATAR - AVATAR_GAP - copy_slot - 24.0) * 0.78).max(72.0);
    let (lines, text_w, text_h) = wrap_bubble(ui, text, max_text, names);
    let bw = (text_w + BUBBLE_PAD * 2.0).clamp(48.0, max_text + BUBBLE_PAD * 2.0);
    let bh = (text_h + BUBBLE_PAD * 2.0).max(AVATAR * 0.65);
    (lines, bw, bh)
}

fn draw_bubble_rect(ui: &Ui, p: [f32; 2], bw: f32, bh: f32, from_user: bool) {
    let dl = crate::ui::window_draw_list(ui);
    let fill = if from_user {
        [0.16, 0.20, 0.28, 0.96]
    } else {
        theme::pal().plate
    };
    let rim = if from_user {
        [
            theme::CURRENT[0],
            theme::CURRENT[1],
            theme::CURRENT[2],
            0.55,
        ]
    } else {
        theme::pal().gold_dim
    };
    dl.add_rect(p, [p[0] + bw, p[1] + bh], fill)
        .filled(true)
        .rounding(BUBBLE_ROUND)
        .build();
    dl.add_rect(p, [p[0] + bw, p[1] + bh], rim)
        .rounding(BUBBLE_ROUND)
        .build();
}

fn draw_copy_glyph(ui: &Ui, p: [f32; 2], size: f32, copied: bool) {
    let dl = crate::ui::window_draw_list(ui);
    let col = if copied {
        theme::pal().gold
    } else {
        theme::pal().muted
    };
    let back = [p[0] + size * 0.28, p[1]];
    let back_br = [p[0] + size, p[1] + size * 0.78];
    let front = [p[0], p[1] + size * 0.22];
    let front_br = [p[0] + size * 0.72, p[1] + size];
    dl.add_rect(back, back_br, col).rounding(2.0).build();
    dl.add_rect(front, front_br, col).rounding(2.0).build();
}

/// Draw wrapped spans: plain in cream, bold drawn twice a pixel apart, italic
/// in the family's italic face (else muted), names and arrows in the accent,
/// warnings in `WARN`, links underlined and clickable.
fn draw_bubble_text(ui: &Ui, p: [f32; 2], lines: &[Line], msg_i: usize) {
    let th = theme::pal();
    let line_h = ui.calc_text_size("Ag")[1];
    let scale = theme::ui_scale();
    let bold_off = scale.round().max(1.0);
    let has_italic = fonts::has_italic();
    // Link hit boxes are laid after the draw list is released: imgui-rs
    // allows one live draw list per window.
    let mut links: Vec<([f32; 2], f32, String)> = Vec::new();
    {
        let dl = crate::ui::window_draw_list(ui);
        for (li, line) in lines.iter().enumerate() {
            let ty = p[1] + BUBBLE_PAD + li as f32 * line_h;
            for placed in &line.spans {
                let pos = [p[0] + BUBBLE_PAD + placed.x * scale, ty];
                let text = placed.span.text.as_str();
                let col = match &placed.span.style {
                    SpanStyle::Plain | SpanStyle::Bold | SpanStyle::Bullet => th.cream,
                    SpanStyle::Italic if has_italic => th.cream,
                    SpanStyle::Italic => th.muted,
                    SpanStyle::Name | SpanStyle::Arrow | SpanStyle::Link(_) => th.gold,
                    SpanStyle::Warn => theme::WARN,
                };
                let col = color_u32(col);
                match &placed.span.style {
                    SpanStyle::Bold => {
                        dl.add_text(pos, col, text);
                        dl.add_text([pos[0] + bold_off, pos[1]], col, text);
                    }
                    SpanStyle::Italic => {
                        let _italic = fonts::push_italic();
                        dl.add_text(pos, col, text);
                    }
                    // Shapes, not glyphs: `→` and `•` are missing from some
                    // atlases and drew as `?` (in-game 2026-09-08).
                    SpanStyle::Arrow => {
                        let w = ui.calc_text_size(text)[0];
                        let y = ty + line_h * 0.5;
                        let (x0, x1) = (pos[0] + 3.0 * scale, pos[0] + w - 3.0 * scale);
                        let h = (line_h * 0.18).max(2.0);
                        dl.add_line([x0, y], [x1 - h, y], th.gold)
                            .thickness(1.5 * scale)
                            .build();
                        dl.add_triangle(
                            [x1 - h * 1.6, y - h],
                            [x1, y],
                            [x1 - h * 1.6, y + h],
                            th.gold,
                        )
                        .filled(true)
                        .build();
                    }
                    SpanStyle::Bullet => {
                        let w = ui.calc_text_size(text)[0];
                        dl.add_circle(
                            [pos[0] + w * 0.35, ty + line_h * 0.55],
                            (line_h * 0.13).max(2.0),
                            th.cream,
                        )
                        .filled(true)
                        .build();
                    }
                    SpanStyle::Link(url) => {
                        dl.add_text(pos, col, text);
                        let w = ui.calc_text_size(text)[0];
                        let uy = ty + line_h - 1.0;
                        dl.add_line([pos[0], uy], [pos[0] + w, uy], th.gold).build();
                        links.push((pos, w, url.clone()));
                    }
                    _ => dl.add_text(pos, col, text),
                }
            }
        }
    }
    let after = ui.cursor_screen_pos();
    for (k, (pos, w, url)) in links.iter().enumerate() {
        ui.set_cursor_screen_pos(*pos);
        if ui.invisible_button(format!("##lnk{msg_i}_{k}"), [*w, line_h]) {
            let _ = crate::feedback::shell::open_url(url);
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(url);
        }
    }
    ui.set_cursor_screen_pos(after);
}

/// Transcript fills leftover height; composer stays pinned. `user_icon` is the
/// profession portrait when a character is selected.
pub fn render_chat_bar(
    ui: &Ui,
    state: &mut ChatBarState,
    live: Option<&LiveView>,
    user_icon: Option<&str>,
    user_letter: char,
    picks: &[PickCard],
) -> Option<ChatAction> {
    let mut action = None;

    if state.copied_frames > 0 {
        state.copied_frames = state.copied_frames.saturating_sub(1);
        if state.copied_frames == 0 {
            state.copied_code = None;
        }
    }

    let avail_h = ui.content_region_avail()[1];
    let scroll_h = (avail_h - COMPOSER_H - 10.0).max(80.0);
    let _child_bg = ui.push_style_color(
        StyleColor::ChildBg,
        theme::with_alpha(theme::pal().ink, 0.35),
    );
    ChildWindow::new("##talk_scroll")
        .size([0.0, scroll_h])
        .build(ui, || {
            let avail = ui.content_region_avail()[0];
            if state.history.is_empty() && !state.waiting {
                theme::wrapped(ui, theme::pal().muted, &t("chat.placeholder_new"));
                return;
            }
            let names = state.names.clone().unwrap_or_default();
            let anchor = cards_anchor(&state.history);
            let n = state.history.len();
            for i in 0..n {
                let from_user = state.history[i].from_user;
                let text = state.history[i].text.clone();
                let open_result = state.history[i].open_result;
                let build_failed = state.history[i].build_failed;
                let (lines, bw, bh) = bubble_size(ui, &text, avail, from_user, &names);
                let origin = ui.cursor_screen_pos();
                let bubble_h = bh.max(AVATAR);
                ui.dummy([avail, bubble_h]);

                let (av_x, bub_x, copy_x) = if from_user {
                    let av_x = origin[0] + avail - AVATAR;
                    let copy_x = av_x - COPY_GAP - COPY;
                    (av_x, copy_x - AVATAR_GAP - bw, Some(copy_x))
                } else {
                    (origin[0], origin[0] + AVATAR + AVATAR_GAP, None)
                };
                let av_y = origin[1];
                let bub_y = origin[1];

                if from_user {
                    icons::paint_avatar(ui, user_icon, [av_x, av_y], AVATAR, user_letter);
                    if let Some(copy_x) = copy_x {
                        let copy_y = av_y + (AVATAR - COPY) * 0.5;
                        let key = format!("##msg{i}");
                        let mut copied = state.copied_code.as_deref() == Some(key.as_str())
                            && state.copied_frames > 0;
                        ui.set_cursor_screen_pos([copy_x, copy_y]);
                        if ui.invisible_button(format!("##copy_msg{i}"), [COPY, COPY])
                            && crate::clipboard::copy_text(&text)
                        {
                            state.copied_code = Some(key);
                            state.copied_frames = 120;
                            copied = true;
                        }
                        if ui.is_item_hovered() {
                            ui.tooltip_text(if copied {
                                t("chat.copied")
                            } else {
                                t("chat.copy_gw2")
                            });
                        }
                        draw_copy_glyph(ui, [copy_x, copy_y], COPY, copied);
                    }
                } else {
                    theme::draw_choya_avatar(
                        ui,
                        [av_x + AVATAR * 0.5, av_y + AVATAR * 0.5],
                        AVATAR,
                    );
                }
                draw_bubble_rect(ui, [bub_x, bub_y], bw, bh, from_user);
                draw_bubble_text(ui, [bub_x, bub_y], &lines, i);

                ui.set_cursor_screen_pos([bub_x, bub_y + bh + 4.0]);
                if !state.history[i].chips.is_empty() {
                    render_chips(ui, state, i, bw);
                }
                if open_result {
                    let cy = ui.cursor_screen_pos()[1] + 6.0;
                    ui.set_cursor_screen_pos([bub_x, cy]);
                    if render_build_card(ui, i) {
                        action = Some(ChatAction::OpenBuild);
                    }
                    // Beside our own card, not under it: they are the same
                    // kind of thing — a build you can open — and reading them
                    // as a row says so. Only under the newest plate, so an
                    // old conversation does not sprout cards against builds
                    // that have long since been replaced.
                    if anchor == Some(i) && !picks.is_empty() {
                        if let Some(n) = render_pick_cards(ui, picks, i, false) {
                            action = Some(ChatAction::OpenPick(n));
                        }
                    }
                } else if build_failed && anchor == Some(i) && !picks.is_empty() {
                    // The dead end. Choya tried and produced nothing usable,
                    // so the answer is not an apology on its own — it is the
                    // apology and somewhere to go next. These are the builds
                    // other people published for the same job.
                    let cy = ui.cursor_screen_pos()[1] + 6.0;
                    ui.set_cursor_screen_pos([bub_x, cy]);
                    ui.dummy([0.0, 0.0]);
                    if let Some(n) = render_pick_cards(ui, picks, i, true) {
                        action = Some(ChatAction::OpenPick(n));
                    }
                }
                // A stopped or fallback reply can be continued.
                if !from_user && state.history[i].retry_of.is_some() && !state.waiting {
                    let cy = ui.cursor_screen_pos()[1] + 4.0;
                    ui.set_cursor_screen_pos([bub_x, cy]);
                    if theme::pill(ui, &t("chat.retry"), false, &format!("##retry{i}")) {
                        action = Some(ChatAction::Retry(i));
                    }
                }
                let end_y = ui.cursor_screen_pos()[1].max(origin[1] + bubble_h) + ROW_GAP;
                ui.set_cursor_screen_pos([origin[0], end_y]);
            }
            if state.waiting {
                // The thinking bubble (specs/006 US5): step and seconds,
                // the stall line, and on click the model's live output.
                let text = match live {
                    Some(v) => {
                        let mut t = v.line.clone();
                        if let Some(stall) = &v.stall {
                            t.push('\n');
                            t.push_str(stall);
                        }
                        if !v.note.is_empty() {
                            t.push('\n');
                            t.push_str(&v.note);
                        }
                        if !v.steps.is_empty() {
                            t.push('\n');
                            for line in &v.steps {
                                t.push('\n');
                                t.push_str(line);
                            }
                        }
                        if v.expanded {
                            t.push_str("\n\n");
                            t.push_str(&v.caption);
                            for line in &v.body {
                                t.push('\n');
                                t.push_str(line);
                            }
                        }
                        t
                    }
                    None => t("choya.thinking"),
                };
                let (lines, bw, bh) = bubble_size(ui, &text, avail, false, &names);
                let stop = t("chat.stop");
                let pill_h = ui.calc_text_size(&stop)[1] + 6.0;
                let row_h = bh.max(AVATAR) + 6.0 + pill_h + ROW_GAP;
                let origin = ui.cursor_screen_pos();
                if ui.invisible_button("##talk_thinking", [avail, row_h]) {
                    action = Some(ChatAction::ToggleLive);
                }
                let av_x = origin[0];
                theme::draw_choya_thinking_row(
                    ui,
                    [av_x + AVATAR * 0.5, origin[1] + AVATAR * 0.5],
                    AVATAR,
                );
                let bub_x = origin[0] + AVATAR + AVATAR_GAP;
                draw_bubble_rect(ui, [bub_x, origin[1]], bw, bh, false);
                draw_bubble_text(ui, [bub_x, origin[1]], &lines, usize::MAX);
                ui.set_cursor_screen_pos([bub_x, origin[1] + bh + 6.0]);
                if theme::pill(ui, &stop, false, "##chat_stop") {
                    action = Some(ChatAction::Stop);
                }
                ui.set_cursor_screen_pos([origin[0], origin[1] + row_h]);
            }
            if state.scroll_to_end {
                ui.set_scroll_here_y();
                state.scroll_to_end = false;
            }
        });
    drop(_child_bg);

    if let Some(send) = render_composer(ui, state) {
        action = Some(ChatAction::Send(send));
    }
    action
}

fn render_build_card(ui: &Ui, msg_i: usize) -> bool {
    const PAD_X: f32 = 14.0;
    const PAD_Y: f32 = 10.0;
    const GEM_H: f32 = 28.0;
    const GEM_GAP: f32 = 10.0;
    let title = t("chat.build_ready");
    let sub = t("chat.open_optimized");
    let title_sz = ui.calc_text_size(&title);
    let sub_sz = ui.calc_text_size(&sub);
    let text_w = title_sz[0].max(sub_sz[0]);
    let gem_w = GEM_H * (308.0 / 256.0);
    let w = PAD_X + gem_w + GEM_GAP + text_w + PAD_X;
    let text_h = title_sz[1] + 4.0 + sub_sz[1];
    let h = (text_h + PAD_Y * 2.0).max(GEM_H + PAD_Y * 2.0);
    let p = ui.cursor_screen_pos();
    let id = format!("##build_card{msg_i}");
    let clicked = ui.invisible_button(&id, [w, h]);
    let hovered = ui.is_item_hovered();
    let fill = if hovered {
        theme::with_alpha(theme::pal().gold_hover, 0.96)
    } else {
        theme::pal().plate
    };
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect(p, [p[0] + w, p[1] + h], fill)
            .filled(true)
            .rounding(10.0)
            .build();
        dl.add_rect(p, [p[0] + w, p[1] + h], theme::pal().gold)
            .rounding(10.0)
            .build();
        let gem_cx = p[0] + PAD_X + gem_w * 0.5;
        let gem_top = p[1] + (h - GEM_H) * 0.5;
        theme::draw_gem_icon(&dl, [gem_cx, gem_top], GEM_H);
        let tx = p[0] + PAD_X + gem_w + GEM_GAP;
        let ty = p[1] + (h - text_h) * 0.5;
        dl.add_text([tx, ty], color_u32(theme::pal().gold), &title);
        dl.add_text(
            [tx, ty + title_sz[1] + 4.0],
            color_u32(theme::pal().muted),
            &sub,
        );
    }
    if hovered {
        ui.tooltip_text(t("chat.open_optimized"));
    }
    clicked
}

/// "You might also like" and the published builds, in a row to the right of
/// our own card.
///
/// Compact on purpose: a site mark, the specialization, and the job. Enough
/// to tell three apart and decide which to open; the build itself is one
/// click away and this is a chat, not a catalogue.
fn render_pick_cards(ui: &Ui, picks: &[PickCard], msg_i: usize, alone: bool) -> Option<usize> {
    const GAP: f32 = 18.0;
    const PAD: f32 = 9.0;
    let row_top = ui.item_rect_min()[1];
    // Standing alone there is no card to match, so the row is sized from the
    // text it holds instead of from a neighbour that is not there.
    let row_h = if alone {
        ui.text_line_height() * 2.0 + 18.0
    } else {
        ui.item_rect_size()[1]
    };
    let mut x = if alone {
        ui.item_rect_min()[0]
    } else {
        ui.item_rect_max()[0] + GAP
    };

    let label = if alone {
        t("cmp.none_of_mine")
    } else {
        t("cmp.also_like_short")
    };
    let label_sz = ui.calc_text_size(&label);
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_text(
            [x, row_top + (row_h - label_sz[1]) * 0.5],
            color_u32(theme::pal().muted),
            &label,
        );
    }
    x += label_sz[0] + GAP * 0.5;

    let mut chosen = None;
    for (n, pick) in picks.iter().enumerate() {
        let mark = ui.text_line_height();
        let title_sz = ui.calc_text_size(&pick.title);
        let detail_sz = ui.calc_text_size(&pick.detail);
        let text_w = title_sz[0].max(detail_sz[0]);
        let w = PAD + mark + 8.0 + text_w + PAD;
        let h = row_h;

        ui.set_cursor_screen_pos([x, row_top]);
        let clicked = ui.invisible_button(format!("##pick{msg_i}_{n}"), [w, h]);
        let hovered = ui.is_item_hovered();
        let fill = if hovered {
            theme::with_alpha(theme::pal().gold_hover, 0.96)
        } else {
            theme::pal().plate
        };
        {
            let dl = crate::ui::window_draw_list(ui);
            dl.add_rect([x, row_top], [x + w, row_top + h], fill)
                .filled(true)
                .rounding(10.0)
                .build();
            dl.add_rect(
                [x, row_top],
                [x + w, row_top + h],
                theme::pal().chip_idle_rim,
            )
            .rounding(10.0)
            .build();
            let text_h = title_sz[1] + 4.0 + detail_sz[1];
            let ty = row_top + (h - text_h) * 0.5;
            let mid = [x + PAD + mark * 0.5, row_top + h * 0.5];
            match theme::site_tex(&pick.source) {
                Some(tid) => {
                    let r = mark * 0.5;
                    dl.add_image(tid, [mid[0] - r, mid[1] - r], [mid[0] + r, mid[1] + r])
                        .build();
                }
                None => {
                    dl.add_circle(mid, 3.0, theme::pal().gold)
                        .filled(true)
                        .build();
                }
            }
            let tx = x + PAD + mark + 8.0;
            dl.add_text([tx, ty], color_u32(theme::pal().gold), &pick.title);
            dl.add_text(
                [tx, ty + title_sz[1] + 4.0],
                color_u32(theme::pal().muted),
                &pick.detail,
            );
        }
        if hovered {
            ui.tooltip_text(tf("fmt.open_from", &[("site", &pick.source)]));
        }
        if clicked {
            chosen = Some(n);
        }
        x += w + GAP * 0.5;
    }
    chosen
}

fn draw_send_icon(ui: &Ui, c: [f32; 2], on: bool) {
    let col = if on {
        theme::pal().gold
    } else {
        theme::pal().muted
    };
    let dl = crate::ui::window_draw_list(ui);
    let s = 11.0;
    dl.add_triangle(
        [c[0] - s * 0.7, c[1] - s * 0.55],
        [c[0] + s * 0.85, c[1]],
        [c[0] - s * 0.7, c[1] + s * 0.55],
        col,
    )
    .filled(true)
    .build();
}

fn render_composer(ui: &Ui, state: &mut ChatBarState) -> Option<String> {
    let avail = ui.content_region_avail()[0];
    let origin = ui.cursor_screen_pos();
    ui.dummy([avail, COMPOSER_H]);
    let after = ui.cursor_screen_pos();

    let choya_c = [
        origin[0] + COMPOSER_CHOYA * 0.5,
        origin[1] + COMPOSER_H * 0.52,
    ];
    if state.waiting {
        // Faces cycling (blink, wink, gasp): awake, and not the header's walk.
        theme::draw_choya_avatar(ui, choya_c, COMPOSER_CHOYA);
    } else if theme::composer_choya_bobbing(state.last_typed, std::time::Instant::now()) {
        theme::draw_choya_walk(ui, choya_c, COMPOSER_CHOYA);
    } else {
        theme::draw_choya_sleep(ui, choya_c, COMPOSER_CHOYA);
    }

    let bx = origin[0] + COMPOSER_CHOYA + 8.0;
    let by = origin[1];
    let bw = (avail - COMPOSER_CHOYA - 8.0).max(80.0);
    let bh = COMPOSER_H;
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect([bx, by], [bx + bw, by + bh], theme::pal().plate)
            .filled(true)
            .rounding(18.0)
            .build();
        dl.add_rect([bx, by], [bx + bw, by + bh], theme::pal().gold_dim)
            .rounding(18.0)
            .build();
    }

    let input_w = (bw - SEND_SZ - 20.0).max(40.0);
    ui.set_cursor_screen_pos([bx + 12.0, by + 8.0]);
    let _pad = ui.push_style_var(StyleVar::FramePadding([8.0, 8.0]));
    let _bg = ui.push_style_color(StyleColor::FrameBg, [0.0, 0.0, 0.0, 0.0]);
    let _bgh = ui.push_style_color(StyleColor::FrameBgHovered, [0.0, 0.0, 0.0, 0.0]);
    let _bga = ui.push_style_color(StyleColor::FrameBgActive, [0.0, 0.0, 0.0, 0.0]);
    let _brd = ui.push_style_var(StyleVar::FrameBorderSize(0.0));

    let enter_pressed = ui
        .input_text_multiline("##chat_input", &mut state.input, [input_w, bh - 16.0])
        .flags(
            InputTextFlags::CALLBACK_RESIZE
                | InputTextFlags::ENTER_RETURNS_TRUE
                | InputTextFlags::CTRL_ENTER_FOR_NEW_LINE,
        )
        .build();
    if ui.is_item_edited() {
        state.last_typed = Some(std::time::Instant::now());
    }
    drop(_brd);
    drop(_bga);
    drop(_bgh);
    drop(_bg);
    drop(_pad);

    if state.input.is_empty() {
        crate::ui::window_draw_list(ui).add_text(
            [bx + 20.0, by + 16.0],
            color_u32(theme::pal().muted),
            t("chat.placeholder"),
        );
    }

    ui.set_cursor_screen_pos([bx + bw - SEND_SZ - 10.0, by + (bh - SEND_SZ) * 0.5]);
    let send_hit = ui.invisible_button("##chat_send", [SEND_SZ, SEND_SZ]);
    let send_on = !state.input.trim().is_empty();
    let send_p = ui.item_rect_min();
    draw_send_icon(
        ui,
        [send_p[0] + SEND_SZ * 0.5, send_p[1] + SEND_SZ * 0.5],
        send_on,
    );
    if ui.is_item_hovered() {
        ui.tooltip_text(t("chat.send_tip"));
    }

    ui.set_cursor_screen_pos(after);

    let can_send = !state.input.trim().is_empty();
    if (enter_pressed || send_hit) && can_send {
        return queue_user_message(state, &state.input.clone());
    }
    None
}

fn render_chips(ui: &Ui, state: &mut ChatBarState, msg_i: usize, max_w: f32) {
    let n = state.history[msg_i].chips.len();
    if n == 0 {
        return;
    }
    let mut row_x = 0.0;
    for chip_i in 0..n {
        let label = if state.copied_code.as_deref()
            == Some(state.history[msg_i].chips[chip_i].code.as_str())
            && state.copied_frames > 0
        {
            t("chat.copied")
        } else {
            state.history[msg_i].chips[chip_i].label.clone()
        };
        let pill_w = ui.calc_text_size(&label)[0] + 20.0;
        if chip_i > 0 {
            if row_x + pill_w + 4.0 > max_w {
                row_x = 0.0;
            } else {
                ui.same_line_with_spacing(0.0, 4.0);
            }
        }
        let id = format!("##kchip{msg_i}_{chip_i}");
        let selected = state.copied_code.as_deref()
            == Some(state.history[msg_i].chips[chip_i].code.as_str())
            && state.copied_frames > 0;
        if theme::pill(ui, &label, selected, &id) {
            let code = state.history[msg_i].chips[chip_i].code.clone();
            if crate::clipboard::copy_text(&code) {
                state.copied_code = Some(code);
                state.copied_frames = 120;
            }
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(t("chat.copy_gw2"));
        }
        row_x += pill_w + 4.0;
    }
}

/// Add an assistant reply with no serving chips (errors, timeout, talk).
pub fn add_ai_response(state: &mut ChatBarState, text: String) {
    add_plated_response(state, text, Vec::new(), false);
}

/// Fold the punctuation a language model writes into what every atlas can
/// draw. Three font fixes in two days each covered one face and the player
/// read a '?' in the next one (`ui_font: "ja"`, 2026-09-07). A bubble does
/// not need an em dash; it needs to never show a question mark the model
/// did not write. Glyph ranges stay declared for the faces that honour them.
pub fn fold_punctuation(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{2014}' | '\u{2013}' | '\u{2012}' | '\u{2015}' => out.push_str(" - "),
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => out.push('"'),
            '\u{2026}' => out.push_str("..."),
            '\u{2022}' | '\u{25CF}' | '\u{25E6}' => out.push('*'),
            '\u{2192}' | '\u{27A1}' | '\u{2794}' => out.push_str("->"),
            '\u{2190}' => out.push_str("<-"),
            '\u{2265}' => out.push_str(">="),
            '\u{2264}' => out.push_str("<="),
            '\u{2260}' => out.push_str("!="),
            '\u{00D7}' => out.push('x'),
            '\u{00A0}' | '\u{202F}' | '\u{2009}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Add an assistant reply and optional GW2 chat-link chips.
pub fn add_plated_response(
    state: &mut ChatBarState,
    text: String,
    chips: Vec<ChatChip>,
    open_result: bool,
) {
    state.waiting = false;
    // Whole reply, whatever its length (specs/006 FR-001): the bubble wraps
    // and the transcript scrolls. `CHAT_HISTORY_CAP` bounds the count only.
    let text = fold_punctuation(&text);
    state.history.push(ChatMessage {
        from_user: false,
        text,
        chips,
        open_result,
        ..Default::default()
    });
    trim_history(&mut state.history);
    state.scroll_to_end = true;
    state.dirty = true;
}

/// Add a reply that was supposed to be a build and could not be one.
///
/// The distinction matters at draw time: this is the reply that gets the
/// community builds offered under it, because it is the one that left the
/// player with nothing.
pub fn add_failed_build_response(state: &mut ChatBarState, text: String) {
    add_ai_response(state, text);
    if let Some(last) = state.history.last_mut() {
        last.build_failed = true;
    }
}

/// Attach inbound chips to the latest player message.
pub fn attach_order_chips(state: &mut ChatBarState, display: String, chips: Vec<ChatChip>) {
    if let Some(last) = state.history.last_mut() {
        if last.from_user {
            last.text = display;
            last.chips = chips;
            state.dirty = true;
        }
    }
}

pub fn load_history(addon_dir: &Path) -> Vec<ChatMessage> {
    let path = addon_dir.join("kitchen.json");
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(mut hist) = serde_json::from_slice::<Vec<ChatMessage>>(&bytes) else {
        return Vec::new();
    };
    if hist.len() > CHAT_HISTORY_CAP {
        let drop = hist.len() - CHAT_HISTORY_CAP;
        hist.drain(..drop);
    }
    hist
}

pub fn save_history(addon_dir: &Path, history: &[ChatMessage]) {
    let path = addon_dir.join("kitchen.json");
    let json = match serde_json::to_vec(history) {
        Ok(json) => json,
        Err(e) => {
            crate::ui::log_disk_error(format!("chat history serialize failed: {e}"));
            return;
        }
    };
    let tmp = addon_dir.join("kitchen.json.tmp");
    if let Err(e) = std::fs::write(&tmp, json) {
        crate::ui::log_disk_error(format!("chat history write failed: {e}"));
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        crate::ui::log_disk_error(format!("chat history rename failed: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_links::{encode_item, ChatChip, LinkKind};

    #[test]
    fn queue_user_message_while_waiting_still_queues() {
        let mut state = ChatBarState {
            waiting: true,
            ..Default::default()
        };
        assert_eq!(queue_user_message(&mut state, "hi").as_deref(), Some("hi"));
        assert_eq!(state.history.len(), 1);
        assert!(state.history[0].from_user);
        assert!(state.waiting);
        state.waiting = false;
        assert_eq!(
            queue_user_message(&mut state, "  yo  ").as_deref(),
            Some("yo")
        );
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn recent_transcript_keeps_last_n() {
        let mut history = Vec::new();
        for i in 0..5 {
            history.push(ChatMessage {
                from_user: i % 2 == 0,
                text: format!("m{i}"),
                chips: Vec::new(),
                ..Default::default()
            });
        }
        let t = recent_transcript(&history, 3);
        assert!(t.contains("m2"));
        assert!(t.contains("m4"));
        assert!(!t.contains("m0"));
        assert!(t.starts_with("Player: m2") || t.contains("Player: m4"));
    }

    #[test]
    fn cards_anchor_is_newest_open_result_message() {
        let msg = |from_user: bool, open_result: bool, build_failed: bool| ChatMessage {
            from_user,
            open_result,
            build_failed,
            ..Default::default()
        };
        let history = vec![
            msg(true, false, false),
            msg(false, true, false),
            msg(true, false, false),
            msg(false, false, false),
        ];
        assert_eq!(cards_anchor(&history), Some(1));
        let history = vec![msg(false, true, false), msg(false, true, false)];
        assert_eq!(cards_anchor(&history), Some(1));
        let history = vec![msg(false, false, true), msg(false, false, false)];
        assert_eq!(cards_anchor(&history), Some(0));
        assert_eq!(cards_anchor(&[msg(false, false, false)]), None);
        assert_eq!(cards_anchor(&[]), None);
    }

    #[test]
    fn plated_response_keeps_whole_text() {
        let mut state = ChatBarState {
            waiting: true,
            ..Default::default()
        };
        let long: String = "é".repeat(3000);
        add_ai_response(&mut state, long.clone());
        assert!(!state.waiting);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].text, long);
        assert!(!state.history[0].text.ends_with("..."));
        assert!(state.history[0].chips.is_empty());
        assert!(state.scroll_to_end);
    }

    #[test]
    fn add_plated_response_keeps_chips() {
        let mut state = ChatBarState {
            waiting: true,
            ..Default::default()
        };
        add_plated_response(
            &mut state,
            "Scholar rune.".into(),
            vec![ChatChip {
                kind: LinkKind::Item,
                label: "Rune of the Scholar".into(),
                code: encode_item(24836),
            }],
            true,
        );
        assert_eq!(state.history[0].chips.len(), 1);
        assert_eq!(state.history[0].chips[0].code, "[&AgEEYQAA]");
        assert!(state.history[0].open_result);
    }

    #[test]
    fn attach_order_chips_updates_last_customer_line() {
        let mut state = ChatBarState::default();
        state.history.push(ChatMessage {
            from_user: true,
            text: "[&AgEEYQAA]".into(),
            chips: Vec::new(),
            ..Default::default()
        });
        attach_order_chips(
            &mut state,
            "Item #24836".into(),
            vec![ChatChip {
                kind: LinkKind::Item,
                label: "Item #24836".into(),
                code: encode_item(24836),
            }],
        );
        assert_eq!(state.history[0].text, "Item #24836");
        assert_eq!(state.history[0].chips.len(), 1);
    }

    #[test]
    fn kitchen_history_roundtrips_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "gw2_kitchen_hist_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let history = vec![ChatMessage {
            from_user: true,
            text: "plate this".into(),
            chips: vec![ChatChip {
                kind: LinkKind::Item,
                label: "Scholar".into(),
                code: encode_item(24836),
            }],
            ..Default::default()
        }];
        save_history(&dir, &history);
        let loaded = load_history(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text, "plate this");
        assert_eq!(loaded[0].chips[0].code, encode_item(24836));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_history_logs_when_directory_is_missing() {
        let dir = std::env::temp_dir().join(format!(
            "gw2_kitchen_missing_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        save_history(&dir, &[]);
        assert!(!dir.join("kitchen.json").exists());
    }

    #[test]
    fn old_kitchen_json_defaults_open_result_off() {
        let hist: Vec<ChatMessage> =
            serde_json::from_str(r#"[{"from_user":true,"text":"hi","chips":[]}]"#).unwrap();
        assert!(!hist[0].open_result);
        assert!(!hist[0].stopped);
        assert!(hist[0].retry_of.is_none());
        assert_eq!(hist[0].text, "hi");
    }
}
