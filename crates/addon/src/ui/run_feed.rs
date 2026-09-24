//! What a run is doing while it runs, and what it cost once it is done.
//!
//! The live step list (fed by the run's worker through `with_state`), the
//! generation pill at the right end of the results tab strip, and the
//! collapsible run log under it.
//!
//! Everything here runs on the render thread, which already holds STATE
//! (`ui/mod.rs` locks it around the whole frame). `with_state` from here
//! would lock a non-re-entrant mutex twice and hang the game, so callers
//! pass what these functions need (the cost currency) as arguments; the
//! `render_paths_never_take_the_state_lock` test holds that line.

use std::time::Instant;

use nexus::imgui::{ChildWindow, TreeNodeFlags, Ui};

use gw2_core::config::CostCurrency;
use gw2_core::generations::{
    GenerationRecord, GenerationTier, LlmUsed, RunFeed, RunStep, StepState, ELIDED_STEP_ID,
};
use gw2_core::i18n::{t, tf};

use crate::ui::{cost_format::format_cost, theme};

/// The feed of the run in flight (or the last one), mirrored from its worker.
#[derive(Debug, Default)]
pub struct LiveFeed {
    /// Which run owns this feed; a worker only writes to its own.
    pub run_id: u64,
    pub started: Option<Instant>,
    pub feed: RunFeed,
    /// True while the run is in flight.
    pub live: bool,
}

/// UTF-8 safe clip to `max` chars, with an ellipsis when cut.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// "7 min 12 s", "42 s", "3.4 s".
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 {
        tf(
            "gen.min_sec",
            &[
                ("m", &(secs / 60).to_string()),
                ("s", &(secs % 60).to_string()),
            ],
        )
    } else if ms >= 10_000 {
        tf("gen.sec", &[("s", &secs.to_string())])
    } else {
        tf("gen.sec", &[("s", &format!("{:.1}", ms as f64 / 1000.0))])
    }
}

/// "41.2k", "1.3M", "812".
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// The model's display name from the pricing table, else its id.
pub fn model_label(llm: &LlmUsed) -> String {
    gw2_optimizer::llm::pricing::lookup(&llm.provider, &llm.model)
        .map(|row| row.label.clone())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| llm.model.clone())
}

/// `currency` is `state.config.cost_currency`, read by the caller.
pub fn cost_text(record: &GenerationRecord, currency: CostCurrency) -> String {
    format_cost(
        record.cost_estimate_usd,
        currency,
        gw2_optimizer::llm::pricing::fx(),
    )
}

pub fn tier_label(tier: GenerationTier) -> String {
    t(match tier {
        GenerationTier::BeamV2 => "gen.tier_beam",
        GenerationTier::Deterministic => "gen.tier_deterministic",
        GenerationTier::Legacy => "gen.tier_legacy",
        GenerationTier::Choya => "gen.tier_choya",
    })
}

/// "7 min 12 s · Gemini 2.5 Flash · 41.2k tokens · ≈ $0.02", or
/// "42 s · deterministic".
pub fn pill_text(record: &GenerationRecord, currency: CostCurrency) -> String {
    let mut parts = vec![format_duration(record.duration_ms)];
    match &record.llm {
        Some(llm) => {
            parts.push(model_label(llm));
            parts.push(tf(
                "gen.tokens",
                &[("n", &format_tokens(record.tokens.total))],
            ));
            parts.push(cost_text(record, currency));
        }
        None => parts.push(t("gen.deterministic")),
    }
    parts.join(" \u{00b7} ")
}

/// The pill's hover breakdown, one line each.
pub fn tooltip_lines(record: &GenerationRecord, currency: CostCurrency) -> Vec<String> {
    let started = record
        .started_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let mut lines = vec![tf("gen.tip_started", &[("time", &started)])];
    if let Some(tier) = record.tier {
        lines.push(tf("gen.tip_tier", &[("tier", &tier_label(tier))]));
    }
    lines.push(tf(
        "gen.tip_time_split",
        &[
            ("wait", &format_duration(record.llm_wait_ms)),
            ("compute", &format_duration(record.compute_ms)),
        ],
    ));
    for timing in &record.tier_timings {
        lines.push(format!(
            "  {}: {}",
            tier_label(timing.tier),
            format_duration(timing.duration_ms)
        ));
    }
    if record.llm.is_some() {
        lines.push(tf(
            "gen.tip_tokens",
            &[
                ("prompt", &format_tokens(record.tokens.prompt)),
                ("completion", &format_tokens(record.tokens.completion)),
            ],
        ));
        lines.push(tf(
            "gen.tip_requests",
            &[("n", &record.tokens.requests.to_string())],
        ));
        lines.push(match &record.pricing_source {
            Some(src) if src.row_id == gw2_optimizer::llm::pricing::REPORTED_COST_ROW => {
                tf("gen.tip_reported", &[("date", &src.as_of)])
            }
            Some(src) if src.row_id == gw2_optimizer::llm::pricing::PARTIAL_COST_ROW => {
                t("gen.tip_cost_partial")
            }
            Some(src) => tf(
                "gen.tip_pricing",
                &[("row", &src.row_id), ("date", &src.as_of)],
            ),
            None => t("gen.cost_na"),
        });
        if currency == CostCurrency::Eur {
            let fx = gw2_optimizer::llm::pricing::fx();
            lines.push(tf(
                "gen.tip_fx_rate",
                &[
                    ("rate", &format!("{:.2}", fx.eur_per_usd)),
                    ("date", &fx.as_of),
                ],
            ));
        }
    }
    lines
}

fn pill_size(ui: &Ui, text: &str) -> [f32; 2] {
    let sz = ui.calc_text_size(text);
    [sz[0] + 20.0, sz[1] + 6.0]
}

/// The generation pill, drawn at the cursor. Hover for the breakdown.
fn draw_pill(ui: &Ui, text: &str, id: &str) {
    let [w, h] = pill_size(ui, text);
    let p = ui.cursor_screen_pos();
    ui.invisible_button(id, [w, h]);
    let th = theme::pal();
    {
        let dl = ui.get_window_draw_list();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], th.chip_idle_fill)
            .filled(true)
            .rounding(h * 0.45)
            .build();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], th.gold_dim)
            .rounding(h * 0.45)
            .build();
        dl.add_text([p[0] + 10.0, p[1] + 3.0], th.gold, text);
    }
}

/// The pill, right-aligned on the current row when it fits after `row_x`
/// (the strip's used width), else right-aligned on a row of its own.
/// `x0` is the strip's local start x, `avail` its width.
pub fn render_generation_pill(
    ui: &Ui,
    record: &GenerationRecord,
    currency: CostCurrency,
    x0: f32,
    avail: f32,
    row_x: f32,
) {
    let text = pill_text(record, currency);
    let [w, _] = pill_size(ui, &text);
    let target = x0 + (avail - w).max(0.0);
    if row_x > 0.0 && row_x + 12.0 + w <= avail {
        ui.same_line();
        let y = ui.cursor_pos()[1];
        ui.set_cursor_pos([target, y]);
    } else {
        let y = ui.cursor_pos()[1];
        ui.set_cursor_pos([target, y]);
    }
    draw_pill(ui, &text, "##generation_pill");
    if ui.is_item_hovered() {
        theme::wide_tooltip(ui, |ui| {
            for line in tooltip_lines(record, currency) {
                ui.text(line);
            }
            // What the numbers above count, so nobody has to guess.
            ui.separator();
            let muted = theme::pal().muted;
            ui.text_colored(muted, t("gen.tip_explain_time"));
            if record.llm.is_some() {
                ui.text_colored(muted, t("gen.tip_explain_tokens"));
                ui.text_colored(muted, t("gen.tip_explain_cost"));
            }
        });
    }
}

/// One step as text and colour. `now_ms` is the run clock for running steps.
fn step_line(step: &RunStep, now_ms: Option<u64>) -> (String, [f32; 4]) {
    let th = theme::pal();
    let at = step.at_ms / 1000;
    let clock = format!("{:>2}:{:02}", at / 60, at % 60);
    if step.id == ELIDED_STEP_ID {
        let n = step.detail.clone().unwrap_or_default();
        return (
            format!("{clock}  {}", tf("gen.elided", &[("n", &n)])),
            th.muted,
        );
    }
    let detail = step
        .detail
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|d| format!(" \u{00b7} {}", clip(d, 160)))
        .unwrap_or_default();
    match &step.state {
        StepState::Running => {
            let spin = ['|', '/', '-', '\\'][theme::anim_cell(133, 4)];
            let extra = match (step.wait_ms, now_ms) {
                (Some(wait), Some(now)) => {
                    let left = (step.at_ms + wait).saturating_sub(now) / 1000;
                    format!(" ({})", tf("gen.left", &[("s", &left.to_string())]))
                }
                (None, Some(now)) => {
                    format!(" ({})", format_duration(now.saturating_sub(step.at_ms)))
                }
                _ => String::new(),
            };
            (
                format!("{clock}  {spin}  {}{detail}{extra}", step.label),
                th.gold,
            )
        }
        StepState::Done { took_ms } => {
            let took = if *took_ms >= 100 {
                format!(" ({})", format_duration(*took_ms))
            } else {
                String::new()
            };
            (
                format!("{clock}  +  {}{detail}{took}", step.label),
                th.cream,
            )
        }
        StepState::Failed { message } => (
            format!(
                "{clock}  !  {}{detail} \u{00b7} {}",
                step.label,
                clip(message, 160)
            ),
            theme::WARN,
        ),
        StepState::Skipped => (format!("{clock}  -  {}{detail}", step.label), th.muted),
    }
}

/// The step list in a scrolling child. Follows the newest line unless the
/// player has scrolled up to read.
pub fn render_steps(ui: &Ui, steps: &[RunStep], now_ms: Option<u64>, id: &str, max_h: f32) {
    let line_h = ui.text_line_height_with_spacing();
    let h = (steps.len() as f32 * line_h + 10.0).clamp(line_h * 2.0, max_h.max(line_h * 2.0));
    ChildWindow::new(id).size([0.0, h]).build(ui, || {
        // Nested children start at scale 1.0; match the player's.
        theme::font_scale(ui, 1.0);
        let following = ui.scroll_y() >= ui.scroll_max_y() - line_h;
        for step in steps {
            let (text, colour) = step_line(step, now_ms);
            ui.text_colored(colour, text);
            // A step whose numbers need saying what they count carries a
            // quiet "?" and explains itself on hover (line or glyph).
            // Resolved on hover, in the language shown now.
            if step.has_explain() {
                let hovered = ui.is_item_hovered();
                ui.same_line();
                ui.text_colored(theme::pal().muted, "?");
                if hovered || ui.is_item_hovered() {
                    if let Some(explain) = step.explain_text() {
                        theme::wide_tooltip(ui, |ui| ui.text(&explain));
                    }
                }
            }
        }
        if following && now_ms.is_some() {
            ui.set_scroll_here_y_with_ratio(1.0);
        }
    });
}

/// The run in flight, under the progress card.
pub fn render_live(ui: &Ui, live: &LiveFeed) {
    if !live.live || live.feed.steps.is_empty() {
        return;
    }
    let now_ms = live
        .started
        .map(|s| Instant::now().saturating_duration_since(s).as_millis() as u64);
    let max_h = (ui.content_region_avail()[1] * 0.5).max(120.0);
    render_steps(ui, &live.feed.steps, now_ms, "##run_feed_live", max_h);
}

/// A finished run's log, collapsed under the header.
pub fn render_log(ui: &Ui, steps: &[RunStep], id: &str) {
    if steps.is_empty() {
        return;
    }
    let label = format!(
        "{}##{id}",
        tf("gen.run_log", &[("n", &steps.len().to_string())])
    );
    if ui.collapsing_header(&label, TreeNodeFlags::empty()) {
        render_steps(ui, steps, None, &format!("##{id}_steps"), 240.0);
    }
}

/// The newest steps as plain lines, for Choya's thinking bubble.
pub fn bubble_lines(live: &LiveFeed, max: usize) -> Vec<String> {
    let now_ms = live
        .started
        .map(|s| Instant::now().saturating_duration_since(s).as_millis() as u64);
    let steps = &live.feed.steps;
    steps
        .iter()
        .skip(steps.len().saturating_sub(max))
        .map(|s| clip(&step_line(s, now_ms).0, 120))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_tokens_read_like_the_header() {
        assert_eq!(format_duration(432_000), "7 min 12 s");
        assert_eq!(format_duration(42_000), "42 s");
        assert_eq!(format_duration(3_400), "3.4 s");
        assert_eq!(format_tokens(41_200), "41.2k");
        assert_eq!(format_tokens(812), "812");
        assert_eq!(format_tokens(1_300_000), "1.3M");
    }

    /// The rule: code the render thread runs never calls `with_state`. The
    /// frame already holds STATE (`ui/mod.rs`), the mutex is not re-entrant,
    /// and a second lock hangs the game. The cost pill once read the currency
    /// that way. This pins the files the generation pill and the Generations
    /// tab draw through: all of run_feed.rs and cost_format.rs, and every
    /// `render_*` / `draw_*` function of comparison.rs and the tab. Workers
    /// (closures handed to `spawn_worker*`, guards they own) stay outside the
    /// pinned functions.
    #[test]
    fn render_paths_never_take_the_state_lock() {
        fn production(src: &str) -> &str {
            src.split("#[cfg(test)]").next().unwrap_or(src)
        }
        fn body(src: &str, at: usize) -> &str {
            let open = src[at..].find('{').expect("fn body") + at;
            let mut depth = 0usize;
            for (i, c) in src[open..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return &src[at..=open + i];
                        }
                    }
                    _ => {}
                }
            }
            panic!("unclosed fn at byte {at}");
        }
        for (file, src) in [
            ("run_feed.rs", include_str!("run_feed.rs")),
            ("cost_format.rs", include_str!("cost_format.rs")),
            // The mini radio window body: `ui::render_mini_radio` holds STATE.
            ("mini_radio.rs", include_str!("mini_radio.rs")),
            // Choya's sprites, quip bubble and quip driver draw from it.
            ("radio/art.rs", include_str!("../radio/art.rs")),
            ("radio/quips.rs", include_str!("../radio/quips.rs")),
        ] {
            assert!(
                !production(src).contains("with_state("),
                "{file} runs on the render thread and must not call with_state"
            );
        }
        for (file, src) in [
            ("comparison.rs", include_str!("comparison.rs")),
            (
                "tabs/about/generations.rs",
                include_str!("main_view/tabs/about/generations.rs"),
            ),
        ] {
            let src = production(src);
            let mut pinned = 0;
            for prefix in ["fn render_", "fn draw_"] {
                for (at, _) in src.match_indices(prefix) {
                    let f = body(src, at);
                    pinned += 1;
                    assert!(
                        !f.contains("with_state("),
                        "{file}: {} runs on the render thread and must not call with_state",
                        f.lines().next().unwrap_or_default()
                    );
                }
            }
            assert!(pinned >= 2, "{file}: no render functions found to pin");
        }
    }

    #[test]
    fn clip_is_char_safe() {
        assert_eq!(clip("Überprüfung läuft", 8), "Überp...");
        assert_eq!(clip("短い", 8), "短い");
    }
}
