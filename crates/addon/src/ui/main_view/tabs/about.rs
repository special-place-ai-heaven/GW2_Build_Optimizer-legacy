//! About tab - in-game changelog, Message developer wizard, message list.

pub(in crate::ui::main_view) mod generations;
pub(super) mod glyphs;
mod wizard;

use std::time::Duration;

use nexus::imgui::{ChildWindow, StyleColor, StyleVar, Ui};

use crate::feedback::{AboutView, Draft, FeedbackState};
use crate::state::AddonState;
use crate::ui::theme;
use gw2_core::feedback::changelog::{self, ChangelogEntry};
use gw2_core::feedback::message::{now_unix, FailReason, LocalMessage, MessageStatus};
use gw2_core::feedback::report::strip_wizard_markup;
use gw2_core::feedback::taxonomy::FeedbackTaxonomy;
use gw2_core::i18n::{t, tf};

/// Rows that went to the server (everything that is not `Local`).
fn sent_count(messages: &[LocalMessage]) -> usize {
    messages.iter().filter(|m| !m.is_local()).count()
}

fn answered_count(messages: &[LocalMessage]) -> usize {
    messages
        .iter()
        .filter(|m| m.status == MessageStatus::Answered)
        .count()
}

/// Every release bundled into the DLL, newest first.
fn changelog_entries() -> Vec<ChangelogEntry> {
    changelog::parse(changelog::EMBEDDED)
}

fn render_about_hero(ui: &Ui, state: &AddonState) {
    const MASCOT: f32 = 96.0;
    const PAD_L: f32 = 20.0;
    const PAD_T: f32 = 38.0;
    const PAD_R: f32 = 24.0;
    const PAD_B: f32 = 8.0;
    let box_w = PAD_L + MASCOT + PAD_R;
    let box_h = PAD_T + MASCOT + PAD_B;
    let top = ui.cursor_screen_pos();
    ui.invisible_button("##about_mascot", [box_w, box_h]);
    let below = ui.cursor_screen_pos();
    let center = [top[0] + PAD_L + MASCOT * 0.5, top[1] + PAD_T + MASCOT * 0.5];
    theme::draw_choya_hero(ui, center, MASCOT);

    let text_x = top[0] + box_w + 8.0;
    let ty0 = top[1] + PAD_T + 10.0;
    let lh = ui.text_line_height();
    ui.set_cursor_screen_pos([text_x, ty0]);
    ui.text_colored(theme::pal().gold, t("about.title"));

    ui.set_cursor_screen_pos([text_x, ty0 + lh + 6.0]);
    let sub = format!(
        "{}  ·  {}  ·  {}",
        t("info.product"),
        tf("fmt.version", &[("ver", crate::VERSION)]),
        tf(
            "fmt.ai",
            &[("provider", state.config.active_provider.label())]
        ),
    );
    ui.text_colored(theme::pal().cream, sub);

    ui.set_cursor_screen_pos([text_x, ty0 + lh * 2.0 + 12.0]);
    ui.text_colored(theme::pal().muted, t("about.tagline"));

    let messages = &state.main.feedback.messages;
    ui.set_cursor_screen_pos([text_x, ty0 + lh * 3.0 + 16.0]);
    ui.text_colored(
        theme::pal().muted,
        tf(
            "about.counts",
            &[
                ("sent", &sent_count(messages).to_string()),
                ("answered", &answered_count(messages).to_string()),
            ],
        ),
    );

    if let Some(reason) = state
        .main
        .data_state
        .as_ref()
        .and_then(|s| s.optimize_block_reason())
    {
        ui.set_cursor_screen_pos([text_x, ty0 + lh * 4.0 + 20.0]);
        ui.text_colored(theme::ERR, reason);
    } else if let Some(stale) = state.main.manifest_staleness.as_deref() {
        let api_current = matches!(
            (state.config.cache_build_number, state.main.live_build_number),
            (Some(cached), Some(live)) if cached == live
        );
        if !api_current {
            ui.set_cursor_screen_pos([text_x, ty0 + lh * 4.0 + 20.0]);
            ui.text_colored(theme::WARN, stale);
        }
    }

    let after = ui.cursor_screen_pos();
    ui.set_cursor_screen_pos([top[0], below[1].max(after[1] + 8.0)]);
}

fn render_action_row(ui: &Ui, state: &mut AddonState) {
    ui.dummy([0.0, 10.0]);
    let msg_label = t("about.btn.message");
    let coffee_label = t("about.btn.coffee");
    let btn_w = theme::gold_button_width(ui, &msg_label)
        .max(theme::gold_button_width(ui, &coffee_label))
        .max(160.0);
    if theme::gold_button_sized(ui, format!("{msg_label}##about_msg"), [btn_w, 0.0])
        && state.main.feedback.draft.is_none()
    {
        state.main.feedback.open_draft();
    }
    ui.same_line_with_spacing(0.0, 10.0);
    if theme::gold_button_sized(ui, format!("{coffee_label}##about_coffee"), [btn_w, 0.0]) {
        let url = state
            .main
            .feedback
            .taxonomy
            .categories
            .iter()
            .find(|c| c.kind == "link")
            .and_then(|c| c.url.clone());
        if let Some(url) = url {
            wizard::coffee(state, &url);
        }
    }
}

fn render_view_toggle(ui: &Ui, state: &mut AddonState) {
    ui.dummy([0.0, 12.0]);
    const VIEWS: [AboutView; 3] = [
        AboutView::Messages,
        AboutView::WhatsNew,
        AboutView::Generations,
    ];
    let labels = [
        t("about.view.messages"),
        t("about.view.whats_new"),
        t("about.view.generations"),
    ];
    let refs = [labels[0].as_str(), labels[1].as_str(), labels[2].as_str()];
    let selected = VIEWS
        .iter()
        .position(|v| *v == state.main.feedback.view)
        .unwrap_or(0);
    // `segment_row` fills the available width; a toggle across the whole
    // content pane looks like a title bar, so box it like the Saves row.
    let row_w = theme::segment_row_min_width(ui, &refs) * 1.1;
    let row_h = ui.text_line_height() + 8.0;
    let mut picked = None;
    ChildWindow::new("##about_view_wrap")
        .size([row_w, row_h])
        .build(ui, || {
            picked = theme::segment_row(ui, &refs, selected, "##about_view");
        });
    if let Some(i) = picked {
        let feedback = &mut state.main.feedback;
        feedback.view = VIEWS[i];
        feedback.view_chosen = true;
    }
}

fn changelog_body(ui: &Ui, text: &str) {
    theme::prose(ui, text);
}

fn render_whats_new(ui: &Ui, state: &AddonState) {
    let scale = state.config.font_scale;
    let entries = changelog_entries();
    let scroll_h = (ui.content_region_avail()[1] - 4.0).max(64.0);
    ChildWindow::new("##about_changelog")
        .size([0.0, scroll_h])
        .build(ui, || {
            // Nested children start at scale 1.0; match the overlay slider.
            ui.set_window_font_scale(scale);
            if entries.is_empty() {
                ui.text_colored(theme::pal().muted, t("about.no_changelog"));
                return;
            }
            let _sp = ui.push_style_var(StyleVar::ItemSpacing([10.0 * scale, 8.0 * scale]));
            ui.dummy([0.0, 4.0]);
            for e in &entries {
                let heading = if e.date.is_empty() {
                    e.version.clone()
                } else {
                    format!("{}  ·  {}", e.version, e.date)
                };
                ui.set_window_font_scale(scale * 1.2);
                ui.text_colored(theme::pal().gold, heading);
                ui.set_window_font_scale(scale);
                ui.dummy([0.0, 6.0]);
                changelog_body(ui, &e.body);
                ui.dummy([0.0, 22.0]);
            }
        });
}

// Messages table (pure helpers)

/// `YYYY-MM-DD HH:MM` (UTC) from unix seconds - the Saves tab's `format_timestamp`
/// algorithm, copied because that helper is private to `saveload.rs`.
fn format_sent(timestamp: u64) -> String {
    let secs_per_day: u64 = 86400;
    let day_secs = timestamp % secs_per_day;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let days = timestamp / secs_per_day;
    let mut y = 1970u64;
    let mut remaining_days = days;
    loop {
        let days_in_year =
            if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let days_in_months: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 11;
    for (i, &dim) in days_in_months.iter().enumerate() {
        if remaining_days < dim {
            m = i;
            break;
        }
        remaining_days -= dim;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m + 1,
        remaining_days + 1,
        hours,
        minutes
    )
}

/// Whether a failed row offers Resend right now, and how the button reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResendState {
    /// No Resend button: not failed, edit-only (`TooLarge`/`Rejected`), or `TooOld`.
    Hidden,
    Enabled,
    /// Rate-limited: drawn dimmed with `n` whole minutes left in the tooltip.
    Countdown(u64),
}

/// The buttons one row offers (design §6a state table).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowActions {
    view: bool,
    resend: ResendState,
    edit: bool,
    discard: bool,
}

fn row_actions(m: &LocalMessage, now: u64) -> RowActions {
    let none = RowActions {
        view: false,
        resend: ResendState::Hidden,
        edit: false,
        discard: false,
    };
    match m.status {
        MessageStatus::Local | MessageStatus::Sending => none,
        MessageStatus::Received
        | MessageStatus::Read
        | MessageStatus::Answered
        | MessageStatus::Closed => RowActions { view: true, ..none },
        MessageStatus::Unknown => RowActions {
            view: true,
            discard: true,
            ..none
        },
        MessageStatus::Failed => {
            let (view, resend, edit) = match &m.last_error {
                Some(FailReason::TooOld) => (false, ResendState::Hidden, false),
                Some(FailReason::TooLarge) | Some(FailReason::Rejected { .. }) => {
                    (true, ResendState::Hidden, true)
                }
                Some(FailReason::RateLimited { .. }) if !m.resend_allowed(now) => (
                    true,
                    ResendState::Countdown(crate::feedback::minutes_left(m, now)),
                    false,
                ),
                _ => (true, ResendState::Enabled, false),
            };
            RowActions {
                view,
                resend,
                edit,
                discard: true,
            }
        }
    }
}

/// Status column text and color (design §6a). `Sending` reports `MUTED`; the
/// renderer blends that toward `CREAM` with the frame pulse.
fn status_view(m: &LocalMessage, now: u64) -> (String, [f32; 4]) {
    match m.status {
        MessageStatus::Sending => (t("msg.status.sending"), theme::pal().muted),
        MessageStatus::Received => (
            tf(
                "msg.status.received",
                &[("id", m.short_id.as_deref().unwrap_or("?"))],
            ),
            theme::pal().muted,
        ),
        MessageStatus::Read => (t("msg.status.read"), theme::pal().muted),
        MessageStatus::Answered => (t("msg.status.answered"), theme::pal().gold),
        MessageStatus::Closed => (t("msg.status.closed"), theme::pal().muted),
        MessageStatus::Failed => {
            let reason = match &m.last_error {
                Some(FailReason::RateLimited { .. }) => rate_fail_text(m, now),
                Some(r) => wizard::fail_text(r),
                None => t("msg.fail.interrupted"),
            };
            (tf("msg.status.failed", &[("reason", &reason)]), theme::WARN)
        }
        MessageStatus::Local => (t("msg.status.local"), theme::pal().muted),
        MessageStatus::Unknown => (t("msg.status.unknown"), theme::pal().muted),
    }
}

/// `Report a bug › Optimize › Wrong result`: the category label followed by the
/// `choice.<id>` label of every path entry; an unknown category shows its raw id.
fn row_path_text(taxonomy: &FeedbackTaxonomy, m: &LocalMessage) -> String {
    let mut out = taxonomy
        .category(&m.category)
        .map_or_else(|| m.category.clone(), |c| t(&c.label));
    for id in &m.path {
        out.push_str(" › ");
        out.push_str(&t(&format!("choice.{id}")));
    }
    out
}

fn blend(a: [f32; 4], b: [f32; 4], k: f32) -> [f32; 4] {
    let k = k.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * k,
        a[1] + (b[1] - a[1]) * k,
        a[2] + (b[2] - a[2]) * k,
        a[3] + (b[3] - a[3]) * k,
    ]
}

/// Everything one row draws, snapshotted before the loop so no `state` borrow
/// is held while ImGui runs (deferred actions apply after the loop).
struct RowView {
    report_id: String,
    title: String,
    path: String,
    icon: String,
    color: [f32; 4],
    sent: String,
    status: String,
    status_color: [f32; 4],
    sending: bool,
    actions: RowActions,
    expanded: bool,
    body: String,
    context: String,
    /// Only for Answered/Closed rows.
    reply: Option<String>,
    closing_note: Option<String>,
}

fn row_view(feedback: &FeedbackState, m: &LocalMessage, now: u64) -> RowView {
    let (icon, color) = feedback.taxonomy.category(&m.category).map_or_else(
        || ("dot".to_string(), theme::pal().muted),
        |c| (c.icon.clone(), glyphs::category_color(&c.color)),
    );
    let (status, status_color) = status_view(m, now);
    let answered = matches!(m.status, MessageStatus::Answered | MessageStatus::Closed);
    RowView {
        report_id: m.report_id.clone(),
        title: strip_wizard_markup(&m.title),
        path: row_path_text(&feedback.taxonomy, m),
        icon,
        color,
        sent: format_sent(m.sent_at),
        status,
        status_color,
        sending: m.status == MessageStatus::Sending,
        actions: row_actions(m, now),
        expanded: feedback.expanded.as_deref() == Some(m.report_id.as_str()),
        body: m.body.clone(),
        context: m.context_summary.clone(),
        reply: if answered { m.reply.clone() } else { None },
        closing_note: m.closing_note.clone(),
    }
}

/// Width the action cell needs for this row's buttons (no trailing gap).
fn actions_width(a: &RowActions, btn_w: f32, edit_w: f32, gap: f32) -> f32 {
    let mut widths = Vec::with_capacity(3);
    if a.view {
        widths.push(btn_w);
    }
    if a.resend != ResendState::Hidden {
        widths.push(btn_w);
    }
    if a.edit {
        widths.push(edit_w);
    }
    if a.discard {
        widths.push(btn_w);
    }
    let n = widths.len() as f32;
    widths.iter().sum::<f32>() + gap * (n - 1.0).max(0.0)
}

// Messages table (ImGui)

/// Copied from `saveload.rs` (private there): the header/row plate behind a table row.
fn paint_row_plate(ui: &Ui, height: f32, header: bool) {
    let p = ui.cursor_screen_pos();
    let w = ui.content_region_avail()[0];
    let fill = if header {
        theme::with_alpha(theme::pal().header_plate, 0.7)
    } else {
        theme::with_alpha(theme::pal().plate, 0.42)
    };
    ui.get_window_draw_list()
        .add_rect(p, [p[0] + w, p[1] + height], fill)
        .filled(true)
        .rounding(5.0)
        .build();
}

/// Copied from `saveload.rs` (private there): trim with `...` to fit `max_w`.
fn clip_label(ui: &Ui, text: &str, max_w: f32) -> String {
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

/// `toss_button`'s shape in WARN amber: the Resend button on failed rows.
fn warn_button(ui: &Ui, label: impl AsRef<str>, size: [f32; 2]) -> bool {
    let _bg = ui.push_style_color(StyleColor::Button, [0.62, 0.42, 0.10, 0.95]);
    let _h = ui.push_style_color(StyleColor::ButtonHovered, [0.78, 0.55, 0.16, 1.0]);
    let _a = ui.push_style_color(StyleColor::ButtonActive, [0.50, 0.34, 0.08, 1.0]);
    let _t = ui.push_style_color(StyleColor::Text, [0.10, 0.08, 0.04, 1.0]);
    ui.button_with_size(label.as_ref(), size)
}

/// [`warn_button`] drawn inert at 40 % with a tooltip saying why (`muted_gold` pattern).
fn dimmed_warn(ui: &Ui, label: impl AsRef<str>, size: [f32; 2], tip: &str) {
    let style = ui.push_style_var(StyleVar::Alpha(0.4));
    warn_button(ui, label, size);
    style.pop();
    if ui.is_item_hovered() {
        ui.tooltip_text(tip);
    }
}

/// Screen-space right edge → ImGui window-local wrap X (`PushTextWrapPos`).
pub(super) fn wrap_pos_local(right_x: f32, window_x: f32, scroll_x: f32) -> f32 {
    right_x - window_x + scroll_x
}

/// Rate-limit copy with remaining minutes, not the original `retry_after`.
pub(super) fn rate_fail_text(m: &LocalMessage, now: u64) -> String {
    tf(
        "msg.fail.rate",
        &[("n", &crate::feedback::minutes_left(m, now).to_string())],
    )
}

/// `text_colored` pinned to an absolute right edge (the plate's inner edge).
/// `PushTextWrapPos` is window-local, so the screen-space edge is converted first.
fn wrapped_to(ui: &Ui, color: [f32; 4], text: &str, right_x: f32) {
    let local = wrap_pos_local(right_x, ui.window_pos()[0], ui.scroll_x());
    let wrap = ui.push_text_wrap_pos_with_pos(local);
    ui.text_colored(color, text);
    wrap.pop(ui);
}

/// Lay out the expanded row's content from (`x`, `y`) and return its height.
/// With `draw == false` nothing is emitted - that pass sizes the plate, which
/// has to be painted before the text it sits under.
fn expanded_block(ui: &Ui, row: &RowView, x: f32, y: f32, wrap_w: f32, draw: bool) -> f32 {
    const GAP: f32 = 8.0;
    let lh = ui.text_line_height();
    let wrapped = |cy: &mut f32, color: [f32; 4], text: &str| {
        if draw {
            ui.set_cursor_screen_pos([x, *cy]);
            wrapped_to(ui, color, text, x + wrap_w);
        }
        *cy += ui.calc_text_size_with_opts(text, false, wrap_w)[1];
    };
    let line = |cy: &mut f32, color: [f32; 4], text: &str| {
        if draw {
            ui.set_cursor_screen_pos([x, *cy]);
            ui.text_colored(color, text);
        }
        *cy += lh + 2.0;
    };

    let mut cy = y;
    if draw {
        ui.set_cursor_screen_pos([x, cy]);
        wizard::paint_mail(ui, &row.body, wrap_w);
    }
    cy += wizard::mail_height(ui, &row.body, wrap_w).max(lh);
    if !row.context.is_empty() {
        cy += GAP;
        let attached = format!("{}  ·  {}", t("about.attached"), row.context);
        wrapped(&mut cy, theme::pal().muted, &attached);
    }
    if let Some(reply) = &row.reply {
        cy += GAP;
        line(&mut cy, theme::pal().gold, &t("about.reply"));
        wrapped(&mut cy, theme::pal().cream, reply);
    }
    if let Some(note) = &row.closing_note {
        cy += GAP;
        line(&mut cy, theme::pal().muted, &t("about.closing_note"));
        wrapped(&mut cy, theme::pal().cream, note);
    }
    cy - y
}

/// Drop a row for good (Failed and Unknown rows offer this).
fn discard(state: &mut AddonState, report_id: &str) {
    let feedback = &mut state.main.feedback;
    feedback.messages.retain(|m| m.report_id != report_id);
    feedback.expanded = None;
    feedback.dirty = true;
}

/// "Edit and resend" (design §6a): discard the failed row and open a fresh draft
/// (new `report_id`) prefilled with its category, path, and body on the text step.
fn edit_and_resend(state: &mut AddonState, report_id: &str) {
    let feedback = &mut state.main.feedback;
    let Some(index) = feedback
        .messages
        .iter()
        .position(|m| m.report_id == report_id && m.status == MessageStatus::Failed)
    else {
        return;
    };
    let row = feedback.messages.remove(index);
    feedback.expanded = None;
    feedback.dirty = true;
    feedback.draft = Some(Draft::from_failed(feedback.taxonomy.clone(), &row));
}

/// The line under the table (design §6a): "Updated just now" in MUTED while the last
/// refresh succeeded under a minute ago; "Status as of … · Choya unreachable" in WARN
/// after a failed one, aged from the last success (or "-" when there never was one);
/// nothing otherwise. `age` is the time since the last successful refresh.
fn refresh_line(ok: Option<bool>, age: Option<Duration>) -> Option<(String, [f32; 4])> {
    match ok {
        Some(true) if age.is_some_and(|a| a < Duration::from_secs(60)) => {
            Some((t("about.updated_now"), theme::pal().muted))
        }
        Some(false) => {
            let age = match age {
                None => "-".to_string(),
                Some(a) => {
                    let mins = a.as_secs() / 60;
                    if mins < 60 {
                        tf("about.age_min", &[("n", &mins.to_string())])
                    } else {
                        tf("about.age_hour", &[("n", &(mins / 60).to_string())])
                    }
                }
            };
            Some((tf("about.stale", &[("age", &age)]), theme::WARN))
        }
        _ => None,
    }
}

/// The Messages view: `render_ranch_table`'s row-plate layout over `feedback.messages`
/// (newest first, as stored). Rows are snapshotted into [`RowView`]s first; button
/// presses are deferred to after the loop so `state` is only borrowed mutably there.
fn render_messages(ui: &Ui, state: &mut AddonState) {
    const GAP: f32 = 8.0;
    const ROW_H: f32 = 56.0;
    const HDR_H: f32 = 28.0;
    const PAD: f32 = 10.0;
    const GLYPH: f32 = 16.0;

    if state.main.feedback.messages.is_empty() {
        ui.dummy([0.0, 8.0]);
        ui.text_colored(theme::pal().muted, t("about.empty"));
        ui.text_colored(theme::pal().muted, t("about.empty_hint"));
        return;
    }

    let now = now_unix();
    let rows: Vec<RowView> = {
        let feedback = &state.main.feedback;
        feedback
            .messages
            .iter()
            .map(|m| row_view(feedback, m, now))
            .collect()
    };

    let view_label = t("about.btn.view");
    let hide_label = t("about.btn.hide");
    let resend_label = t("about.btn.resend");
    let discard_label = t("about.btn.discard");
    let edit_label = t("about.btn.edit_resend");
    let btn_w = [&view_label, &hide_label, &resend_label, &discard_label]
        .iter()
        .map(|s| ui.calc_text_size(s)[0])
        .fold(72.0_f32, f32::max)
        + 24.0;
    let btn = [btn_w, theme::control_height(ui)];
    let edit_w = (ui.calc_text_size(&edit_label)[0] + 24.0).max(btn_w);
    let actions_w = rows
        .iter()
        .map(|r| actions_width(&r.actions, btn_w, edit_w, GAP))
        .fold(btn_w, f32::max)
        + 12.0;
    // Status column fits its widest text (header included) so a short id is never clipped;
    // the cap keeps a long failure line from crowding the message column.
    let status_text_w = rows
        .iter()
        .map(|r| ui.calc_text_size(&r.status)[0])
        .fold(ui.calc_text_size(t("about.col.status"))[0], f32::max);
    // ~3s breathe at 60fps for Sending rows (same formula as the tab pills).
    let pulse = theme::anim_pulse();

    let mut toggle: Option<(String, bool)> = None;
    let mut resend_id: Option<String> = None;
    let mut discard_id: Option<String> = None;
    let mut edit_id: Option<String> = None;

    let refresh = {
        let feedback = &state.main.feedback;
        refresh_line(
            feedback.last_refresh_ok,
            feedback.last_refresh_at.map(|t| t.elapsed()),
        )
    };

    let scroll_h = (ui.content_region_avail()[1] - 4.0).max(64.0);
    ChildWindow::new("##about_messages")
        .size([0.0, scroll_h])
        .build(ui, || {
            let avail = ui.content_region_avail()[0];
            let lh = ui.text_line_height();
            let sent_w = ui.calc_text_size("0000-00-00 00:00")[0] + 12.0;
            let status_w = (status_text_w + 16.0).clamp(110.0, avail * 0.35);
            let message_w = (avail - PAD - sent_w - status_w - actions_w - GAP * 2.0).max(140.0);
            let sent_dx = PAD + message_w + GAP;
            let status_dx = sent_dx + sent_w + GAP;

            paint_row_plate(ui, HDR_H, true);
            let origin = ui.cursor_screen_pos();
            let y = origin[1] + 6.0;
            ui.set_cursor_screen_pos([origin[0] + PAD, y]);
            ui.text_colored(theme::pal().gold, t("about.col.message"));
            ui.set_cursor_screen_pos([origin[0] + sent_dx, y]);
            ui.text_colored(theme::pal().gold, t("about.col.sent"));
            ui.set_cursor_screen_pos([origin[0] + status_dx, y]);
            ui.text_colored(theme::pal().gold, t("about.col.status"));
            ui.set_cursor_screen_pos([origin[0] + avail - actions_w, y]);
            ui.text_colored(theme::pal().gold, t("about.col.actions"));
            ui.set_cursor_screen_pos([origin[0], origin[1] + HDR_H + 6.0]);

            for row in &rows {
                paint_row_plate(ui, ROW_H, false);
                let p = ui.cursor_screen_pos();
                let text_y = p[1] + 8.0;
                let btn_y = p[1] + ((ROW_H - btn[1]) * 0.5).round();
                let id = row.report_id.as_str();

                // Message: glyph, title, category path beneath.
                {
                    let dl = ui.get_window_draw_list();
                    glyphs::draw_glyph(
                        ui,
                        &dl,
                        &row.icon,
                        [p[0] + PAD + GLYPH * 0.5, text_y + lh * 0.5],
                        GLYPH,
                        row.color,
                    );
                }
                let title_x = p[0] + PAD + GLYPH + 8.0;
                let title_w = message_w - GLYPH - 16.0;
                ui.set_cursor_screen_pos([title_x, text_y]);
                ui.text_colored(theme::pal().cream, clip_label(ui, &row.title, title_w));
                ui.set_cursor_screen_pos([title_x, text_y + lh + 2.0]);
                ui.text_colored(theme::pal().muted, clip_label(ui, &row.path, title_w));

                // Sent.
                ui.set_cursor_screen_pos([p[0] + sent_dx, text_y + 8.0]);
                ui.text_colored(theme::pal().cream, &row.sent);

                // Status: clipped to the column, full text on hover.
                let color = if row.sending {
                    blend(theme::pal().muted, theme::pal().cream, pulse)
                } else {
                    row.status_color
                };
                let shown = clip_label(ui, &row.status, status_w - 8.0);
                ui.set_cursor_screen_pos([p[0] + status_dx, text_y + 8.0]);
                ui.text_colored(color, &shown);
                if shown != row.status && ui.is_item_hovered() {
                    theme::wide_tooltip(ui, |ui| ui.text_colored(color, &row.status));
                }

                // Actions, left-aligned in the actions column.
                let a = &row.actions;
                let mut ax = p[0] + avail - actions_w;
                if a.view {
                    let label = if row.expanded {
                        &hide_label
                    } else {
                        &view_label
                    };
                    ui.set_cursor_screen_pos([ax, btn_y]);
                    if theme::gold_button_sized(ui, format!("{label}##view_{id}"), btn) {
                        toggle = Some((row.report_id.clone(), !row.expanded));
                    }
                    ax += btn[0] + GAP;
                }
                match a.resend {
                    ResendState::Hidden => {}
                    ResendState::Enabled => {
                        ui.set_cursor_screen_pos([ax, btn_y]);
                        if warn_button(ui, format!("{resend_label}##resend_{id}"), btn) {
                            resend_id = Some(row.report_id.clone());
                        }
                        ax += btn[0] + GAP;
                    }
                    ResendState::Countdown(mins) => {
                        ui.set_cursor_screen_pos([ax, btn_y]);
                        dimmed_warn(
                            ui,
                            format!("{resend_label}##resend_{id}"),
                            btn,
                            &tf("msg.fail.rate", &[("n", &mins.to_string())]),
                        );
                        ax += btn[0] + GAP;
                    }
                }
                if a.edit {
                    ui.set_cursor_screen_pos([ax, btn_y]);
                    if theme::gold_button_sized(
                        ui,
                        format!("{edit_label}##edit_{id}"),
                        [edit_w, btn[1]],
                    ) {
                        edit_id = Some(row.report_id.clone());
                    }
                    ax += edit_w + GAP;
                }
                if a.discard {
                    ui.set_cursor_screen_pos([ax, btn_y]);
                    if ui.button_with_size(format!("{discard_label}##discard_{id}"), btn) {
                        discard_id = Some(row.report_id.clone());
                    }
                }

                ui.set_cursor_screen_pos([p[0], p[1] + ROW_H + 6.0]);

                // Expanded: a second plate under the row with the body, the
                // attached context, and the reply / closing note when present.
                if row.expanded {
                    let wrap_w = avail - PAD * 2.0;
                    let q = ui.cursor_screen_pos();
                    let inner_h = expanded_block(ui, row, q[0] + PAD, q[1] + PAD, wrap_w, false);
                    let plate_h = inner_h + PAD * 2.0;
                    paint_row_plate(ui, plate_h, false);
                    expanded_block(ui, row, q[0] + PAD, q[1] + PAD, wrap_w, true);
                    ui.set_cursor_screen_pos([q[0], q[1] + plate_h + 6.0]);
                }
            }

            if let Some((text, color)) = &refresh {
                ui.dummy([0.0, 4.0]);
                ui.text_colored(*color, text);
            }
        });

    if let Some((id, expand)) = toggle {
        state.main.feedback.expanded = expand.then_some(id);
    }
    if let Some(id) = resend_id {
        crate::feedback::tasks::resend(state, &id);
    }
    if let Some(id) = discard_id {
        discard(state, &id);
    }
    if let Some(id) = edit_id {
        edit_and_resend(state, &id);
    }
}

pub(in crate::ui::main_view) fn render_about_tab(ui: &Ui, state: &mut AddonState) {
    crate::feedback::tasks::ensure_loaded(state);
    crate::feedback::tasks::refresh_on_open(state);

    render_about_hero(ui, state);
    render_action_row(ui, state);
    if state.main.feedback.draft.is_some() {
        wizard::render_wizard(ui, state);
    }
    render_view_toggle(ui, state);

    ui.dummy([0.0, 10.0]);
    match state.main.feedback.view {
        AboutView::WhatsNew => render_whats_new(ui, state),
        AboutView::Messages => render_messages(ui, state),
        AboutView::Generations => generations::render_generations(ui, state),
    }
    // `messages.json` is flushed by `tasks::flush_dirty` from the frame loop, on any tab.
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_core::feedback::message::{FailReason, LocalMessage, MessageStatus};

    /// `set_language` is process-global and `state::init` calls it too, so the
    /// tests that depend on `en` serialise on the shared STATE test lock.
    fn with_en<R>(f: impl FnOnce() -> R) -> R {
        let _g = crate::state::state_test_guard();
        gw2_core::i18n::set_language("en");
        f()
    }

    fn failed(reason: Option<FailReason>, failed_at: Option<u64>) -> LocalMessage {
        LocalMessage {
            last_error: reason,
            failed_at,
            ..msg(MessageStatus::Failed)
        }
    }

    fn msg(status: MessageStatus) -> LocalMessage {
        LocalMessage {
            report_id: "r".into(),
            short_id: None,
            sent_at: 0,
            category: "bug".into(),
            path: vec![],
            title: "t".into(),
            body: "b".into(),
            status,
            reply: None,
            replied_at: None,
            closing_note: None,
            last_error: None,
            failed_at: None,
            failed_payload: None,
            context_summary: String::new(),
        }
    }

    #[test]
    fn sent_count_excludes_local_rows() {
        let rows = [
            msg(MessageStatus::Local),
            msg(MessageStatus::Received),
            msg(MessageStatus::Answered),
            msg(MessageStatus::Failed),
        ];
        assert_eq!(sent_count(&[]), 0);
        assert_eq!(sent_count(&rows), 3);
    }

    #[test]
    fn answered_count_counts_only_answered() {
        let rows = [
            msg(MessageStatus::Answered),
            msg(MessageStatus::Closed),
            msg(MessageStatus::Answered),
            msg(MessageStatus::Local),
        ];
        assert_eq!(answered_count(&[]), 0);
        assert_eq!(answered_count(&rows), 2);
    }

    #[test]
    fn changelog_entries_are_the_bundled_changelog() {
        let entries = changelog_entries();
        let all = gw2_core::feedback::changelog::parse(gw2_core::feedback::changelog::EMBEDDED);
        assert_eq!(entries, all);
        assert!(
            entries.len() >= 5,
            "What's new should have enough history to scroll"
        );
        assert!(entries
            .iter()
            .all(|e| !e.version.is_empty() && !e.body.is_empty()));
    }

    #[test]
    fn refresh_line_reports_fresh_stale_and_never() {
        with_en(|| {
            let secs = |n| Some(Duration::from_secs(n));
            assert_eq!(
                refresh_line(Some(true), secs(10)),
                Some(("Updated just now".to_string(), theme::pal().muted))
            );
            // A success older than a minute says nothing.
            assert_eq!(refresh_line(Some(true), secs(61)), None);
            assert_eq!(
                refresh_line(Some(false), secs(120)),
                Some((
                    "Status as of 2 min ago  ·  Choya unreachable".to_string(),
                    theme::WARN
                ))
            );
            assert_eq!(
                refresh_line(Some(false), secs(3 * 3600 + 5)),
                Some((
                    "Status as of 3 h ago  ·  Choya unreachable".to_string(),
                    theme::WARN
                ))
            );
            assert_eq!(
                refresh_line(Some(false), None),
                Some((
                    "Status as of -  ·  Choya unreachable".to_string(),
                    theme::WARN
                ))
            );
            assert_eq!(refresh_line(None, None), None);
        });
    }

    #[test]
    fn format_sent_matches_saveload_style() {
        // 2026-08-24 18:41:00 UTC
        assert_eq!(format_sent(1_787_596_860), "2026-08-24 18:41");
        assert_eq!(format_sent(0), "1970-01-01 00:00");
        // 2024-02-29 23:59:59 UTC - leap day survives the month walk.
        assert_eq!(format_sent(1_709_251_199), "2024-02-29 23:59");
    }

    #[test]
    fn status_text_and_color_per_status() {
        with_en(|| {
            assert_eq!(
                status_view(&msg(MessageStatus::Sending), 0),
                ("Sending…".to_string(), theme::pal().muted)
            );
            let received = LocalMessage {
                short_id: Some("a3f9".into()),
                ..msg(MessageStatus::Received)
            };
            assert_eq!(
                status_view(&received, 0),
                ("Received  ·  #a3f9".to_string(), theme::pal().muted)
            );
            assert_eq!(
                status_view(&msg(MessageStatus::Read), 0),
                ("Read".to_string(), theme::pal().muted)
            );
            assert_eq!(
                status_view(&msg(MessageStatus::Answered), 0),
                ("Answered".to_string(), theme::pal().gold)
            );
            assert_eq!(
                status_view(&msg(MessageStatus::Closed), 0),
                ("Closed".to_string(), theme::pal().muted)
            );
            assert_eq!(
                status_view(&failed(Some(FailReason::Network), Some(0)), 0),
                (
                    "Not sent - Couldn't reach Choya. Check your connection.".to_string(),
                    theme::WARN
                )
            );
            assert_eq!(
                status_view(&msg(MessageStatus::Local), 0),
                ("Local".to_string(), theme::pal().muted)
            );
            assert_eq!(
                status_view(&msg(MessageStatus::Unknown), 0),
                ("No longer on server".to_string(), theme::pal().muted)
            );
        });
    }

    #[test]
    fn rate_limit_status_uses_minutes_left() {
        with_en(|| {
            let m = failed(
                Some(FailReason::RateLimited {
                    retry_after_secs: 90,
                }),
                Some(1_000),
            );
            assert_eq!(
                status_view(&m, 1_000).0,
                "Not sent - Slow down - try again in 2 min."
            );
            assert_eq!(
                status_view(&m, 1_030).0,
                "Not sent - Slow down - try again in 1 min."
            );
        });
    }

    #[test]
    fn compact_row_title_strips_wizard_markup() {
        let feedback = FeedbackState {
            taxonomy: FeedbackTaxonomy::embedded(),
            ..FeedbackState::default()
        };
        let m = LocalMessage {
            title: "%NL0P|The optimizer picked a trident on land.".into(),
            ..msg(MessageStatus::Received)
        };
        let row = row_view(&feedback, &m, 0);
        assert!(!row.title.contains("%NL0P|"), "{}", row.title);
        assert_eq!(row.title, "The optimizer picked a trident on land.");
    }

    #[test]
    fn wrap_pos_local_converts_screen_x_to_window_local() {
        assert_eq!(wrap_pos_local(200.0, 50.0, 0.0), 150.0);
        assert_eq!(wrap_pos_local(200.0, 50.0, 10.0), 160.0);
    }

    #[test]
    fn actions_for_row() {
        let now = 1_787_596_860;
        let none = RowActions {
            view: false,
            resend: ResendState::Hidden,
            edit: false,
            discard: false,
        };
        assert_eq!(row_actions(&msg(MessageStatus::Local), now), none);
        assert_eq!(row_actions(&msg(MessageStatus::Sending), now), none);
        assert_eq!(
            row_actions(&msg(MessageStatus::Received), now),
            RowActions {
                view: true,
                ..none.clone()
            }
        );
        assert_eq!(
            row_actions(&failed(Some(FailReason::Network), Some(now)), now),
            RowActions {
                view: true,
                resend: ResendState::Enabled,
                edit: false,
                discard: true,
            }
        );
        assert_eq!(
            row_actions(&failed(None, Some(now)), now).resend,
            ResendState::Enabled
        );
        assert_eq!(
            row_actions(
                &failed(
                    Some(FailReason::RateLimited {
                        retry_after_secs: 90
                    }),
                    Some(now)
                ),
                now
            ),
            RowActions {
                view: true,
                resend: ResendState::Countdown(2),
                edit: false,
                discard: true,
            }
        );
        assert_eq!(
            row_actions(
                &failed(
                    Some(FailReason::RateLimited {
                        retry_after_secs: 90
                    }),
                    Some(now)
                ),
                now + 90
            )
            .resend,
            ResendState::Enabled
        );
        assert_eq!(
            row_actions(&failed(Some(FailReason::TooLarge), Some(now)), now),
            RowActions {
                view: true,
                resend: ResendState::Hidden,
                edit: true,
                discard: true,
            }
        );
        assert!(
            row_actions(
                &failed(
                    Some(FailReason::Rejected {
                        reason: "bad schema".into()
                    }),
                    Some(now)
                ),
                now
            )
            .edit
        );
        assert_eq!(
            row_actions(&failed(Some(FailReason::TooOld), Some(now)), now),
            RowActions {
                view: false,
                resend: ResendState::Hidden,
                edit: false,
                discard: true,
            }
        );
        assert_eq!(
            row_actions(&msg(MessageStatus::Unknown), now),
            RowActions {
                view: true,
                resend: ResendState::Hidden,
                edit: false,
                discard: true,
            }
        );
    }
}
