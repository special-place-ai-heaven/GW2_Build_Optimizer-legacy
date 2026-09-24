//! Talk tab — Choya chat. Not a footer on Improve / New Build.

use nexus::imgui::Ui;

use crate::state::{AddonState, MainTab};
use crate::ui::chat_bar::ChatAction;
use crate::ui::theme;
use gw2_core::i18n::t;

const MASCOT: f32 = 132.0;

/// The quick-prompt chips. Each one is a build request, whatever its words
/// (`chat_flow::classify`).
pub(in crate::ui::main_view) const STARTERS: &[(&str, &str)] = &[
    (
        "starter.power",
        "Optimize a power DPS build for my current mode and role.",
    ),
    (
        "starter.condi",
        "Best condition DPS build for raids and strikes.",
    ),
    (
        "starter.sustain",
        "How should I trade survivability vs damage on this character?",
    ),
    ("starter.wvw", "Build me a WvW roaming loadout."),
    (
        "starter.improve",
        "Improve my current equipped build. Keep the playstyle, raise the weak axes.",
    ),
];

pub(in crate::ui::main_view) fn render_talk_tab(ui: &Ui, state: &mut AddonState) {
    render_choya_identity(ui, state);

    ui.spacing();
    // No character is the one part of this line worth interrupting for: it
    // changes what Choya can know about you, and in muted grey among three
    // other facts it reads as scenery. Choya will still plate a build and
    // pick a profession itself — this says why the build is a guess rather
    // than yours.
    if state.main.selected_character.is_none() {
        ui.text_colored(theme::WARN, t("talk.pick_character"));
    }
    theme::wrapped(ui, theme::pal().muted, &talk_context(state));
    ui.spacing();
    render_starters(ui, state);
    ui.spacing();

    let user_icon = state.main.current_build.as_ref().and_then(|b| {
        state.main.game_db.as_deref().and_then(|db| {
            crate::ui::icons::profession_icon_url(db, &b.profession).map(str::to_string)
        })
    });
    let user_letter = state
        .main
        .selected_character
        .and_then(|i| state.main.characters.get(i))
        .and_then(|n| n.chars().next())
        .unwrap_or('?');

    let live = state.main.chat.waiting.then(|| live_view(state));
    if state.main.chat.names.is_none() {
        if let Some(db) = state.main.game_db.as_deref() {
            state.main.chat.names =
                Some(std::sync::Arc::new(crate::ui::chat_markup::name_index(db)));
        }
    }
    // Matched before the chat draws, because the cards sit inside it. The
    // match itself only runs when the proposal changed — see
    // `provider_picks::refresh_provider_picks`.
    crate::ui::main_view::provider_picks::refresh_provider_picks(state);
    let picks: Vec<crate::ui::chat_bar::PickCard> = state
        .main
        .provider_picks
        .iter()
        .map(|build| crate::ui::chat_bar::PickCard {
            title: if build.spec_name.is_empty() {
                build.profession.clone()
            } else {
                build.spec_name.clone()
            },
            source: build.source.clone(),
            detail: if build.gear_prefix.is_empty() {
                build.role.clone()
            } else {
                format!(
                    "{} \u{00b7} {}",
                    build.role,
                    crate::ui::comparison::loc_prefix(
                        state.main.game_db.as_deref(),
                        &build.gear_prefix
                    )
                )
            },
        })
        .collect();

    match crate::ui::chat_bar::render_chat_bar(
        ui,
        &mut state.main.chat,
        live.as_ref(),
        user_icon.as_deref(),
        user_letter,
        &picks,
    ) {
        Some(ChatAction::Send(msg)) => {
            crate::ui::main_view::chat_flow::send_chat_message(state, msg)
        }
        Some(ChatAction::OpenBuild) => open_optimized_tab(state),
        Some(ChatAction::OpenPick(n)) => {
            crate::ui::main_view::provider_picks::adopt_provider_pick(state, n)
        }
        Some(ChatAction::ToggleLive) => {
            if let Ok(mut live) = state.main.chat_live.lock() {
                live.expanded = !live.expanded;
            }
        }
        Some(ChatAction::Stop) => crate::ui::main_view::chat_flow::stop_chat(state),
        Some(ChatAction::Retry(i)) => crate::ui::main_view::chat_flow::retry_chat(state, i),
        None => {}
    }
}

/// The step a request is in, in the player's language.
pub(crate) fn step_label(step: Option<gw2_optimizer::llm::live::Step>) -> String {
    use gw2_optimizer::llm::live::Step;
    match step {
        None => t("choya.thinking"),
        Some(Step::Handshake) => t("chat.step_handshake"),
        Some(Step::Reference) => t("chat.step_reference"),
        Some(Step::Lookup(n)) => gw2_core::i18n::tf("chat.step_lookup", &[("n", &n.to_string())]),
        Some(Step::Scoring) => t("chat.step_scoring"),
        Some(Step::Writing) => t("chat.step_writing"),
        Some(Step::Fallback) => t("chat.step_fallback"),
    }
}

/// Snapshot the live output for this frame's bubble. The lock is held only
/// for the copy; the bubble draws from the snapshot.
fn live_view(state: &AddonState) -> crate::ui::chat_bar::LiveView {
    use gw2_optimizer::llm::live::LiveMode;
    const LINES: usize = 12;
    let now = std::time::Instant::now();
    let elapsed = state
        .main
        .chat_wait_started
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let Ok(live) = state.main.chat_live.lock() else {
        return crate::ui::chat_bar::LiveView {
            line: t("choya.thinking"),
            ..Default::default()
        };
    };
    let line = format!(
        "{} \u{00b7} {:02}:{:02}",
        step_label(live.step),
        elapsed / 60,
        elapsed % 60
    );
    let stall = live
        .stalled_for(now)
        .map(|secs| gw2_core::i18n::tf("chat.stall", &[("secs", &secs.to_string())]));
    let tail = |text: &str| -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        lines
            .iter()
            .skip(lines.len().saturating_sub(LINES))
            .map(|l| l.to_string())
            .collect()
    };
    let (caption, body) = match live.mode {
        LiveMode::AtOnce => (t("chat.live_at_once"), Vec::new()),
        LiveMode::Reasoning if !live.reasoning.is_empty() => {
            (t("chat.live_reasoning"), tail(&live.reasoning))
        }
        _ if !live.content.is_empty() => (t("chat.live_content"), tail(&live.content)),
        _ => (t("chat.live_tools"), tail(&live.tools.join("\n"))),
    };
    crate::ui::chat_bar::LiveView {
        line,
        stall,
        note: live.note.clone(),
        expanded: live.expanded,
        caption,
        body,
        // The run's own steps: every request, wait and check as it happens.
        steps: if state.main.run_feed.live {
            crate::ui::run_feed::bubble_lines(
                &state.main.run_feed,
                if live.expanded { 40 } else { 6 },
            )
        } else {
            Vec::new()
        },
    }
}

fn open_optimized_tab(state: &mut AddonState) {
    state.main.active_tab = if state.main.current_build.is_some() {
        MainTab::Improve
    } else {
        MainTab::NewBuild
    };
    state.main.tab_alert = None;
    state.main.comparison.show_optimized = true;
    if state.main.active_tab == MainTab::Improve {
        if let Some(ref build) = state.main.current_build {
            let build_clone = build.clone();
            super::super::resolution::auto_populate_locks(
                &build_clone,
                &mut state.main.build_locks,
            );
        }
    }
}

fn render_choya_identity(ui: &Ui, state: &mut AddonState) {
    // Hat, maracas, and orbiting notes draw outside the body quad.
    const PAD_L: f32 = 28.0;
    const PAD_T: f32 = 44.0;
    const PAD_R: f32 = 28.0;
    const PAD_B: f32 = 14.0;
    let box_w = PAD_L + MASCOT + PAD_R;
    let box_h = PAD_T + MASCOT + PAD_B;
    let top = ui.cursor_screen_pos();
    ui.invisible_button("##choya_mascot", [box_w, box_h]);
    let below = ui.cursor_screen_pos();
    let center = [top[0] + PAD_L + MASCOT * 0.5, top[1] + PAD_T + MASCOT * 0.5];
    theme::tick_header_pose(
        &mut state.main.chat.header_pose,
        &mut state.main.chat.header_pose_at,
        ui.frame_count() as u32,
        std::time::Instant::now(),
    );
    theme::draw_choya_header(
        ui,
        center,
        MASCOT,
        state.main.chat.waiting,
        state.main.chat.header_pose,
    );

    let text_x = top[0] + box_w + 10.0;
    let ty0 = top[1] + PAD_T + 8.0;
    ui.set_cursor_screen_pos([text_x, ty0]);
    ui.text_colored(theme::pal().gold, t("tab.choya"));
    if !state.main.chat.history.is_empty() && !state.main.chat.waiting {
        ui.same_line_with_spacing(0.0, 12.0);
        if ui.small_button(format!("{}##talk", t("btn.clear"))) {
            state.main.chat.history.clear();
            state.main.chat.copied_code = None;
            state.main.chat.copied_frames = 0;
            state.main.chat.dirty = true;
        }
    }

    ui.set_cursor_screen_pos([text_x, ty0 + ui.text_line_height() + 4.0]);
    ui.text_colored(theme::pal().muted, t("choya.assistant"));
    ui.same_line_with_spacing(0.0, 10.0);
    let online = state.config.has_active_llm_key();
    let pip = if online {
        theme::OPTIMIZED
    } else {
        theme::pal().muted
    };
    let status = if online {
        t("status.online")
    } else {
        t("status.set_api_key")
    };
    let p = ui.cursor_screen_pos();
    let th = ui.calc_text_size(&status)[1];
    crate::ui::window_draw_list(ui)
        .add_circle([p[0] + 5.0, p[1] + th * 0.5], 4.0, pip)
        .filled(true)
        .build();
    ui.dummy([12.0, th]);
    ui.same_line();
    ui.text_colored(pip, &status);

    ui.set_cursor_screen_pos([text_x, ty0 + ui.text_line_height() * 2.0 + 12.0]);
    if let Some(issue) = state.main.provider_issue.clone() {
        theme::wrapped(ui, theme::ERR, &issue);
        ui.set_cursor_screen_pos([text_x, ui.cursor_screen_pos()[1] + 4.0]);
    }
    super::settings::render_talk_model_row(ui, state);
    if let Some(err) = state.main.models_error.clone() {
        ui.set_cursor_screen_pos([text_x, ui.cursor_screen_pos()[1]]);
        theme::wrapped(ui, theme::WARN, &err);
    }

    let after_id = ui.cursor_screen_pos();
    ui.set_cursor_screen_pos([top[0], below[1].max(after_id[1])]);
}

fn render_starters(ui: &Ui, state: &mut AddonState) {
    let avail = ui.content_region_avail()[0];
    let mut row_x = 0.0;
    let mut send: Option<String> = None;
    for (i, (label_key, prompt)) in STARTERS.iter().enumerate() {
        let label = t(label_key);
        let pill_w = ui.calc_text_size(&label)[0] + 20.0;
        if i > 0 {
            if row_x + pill_w + 4.0 > avail {
                row_x = 0.0;
            } else {
                ui.same_line_with_spacing(0.0, 4.0);
            }
        }
        let id = format!("##choya_ask{i}");
        if theme::pill(ui, &label, false, &id) {
            send = Some((*prompt).to_string());
        }
        row_x += pill_w + 4.0;
    }
    if let Some(prompt) = send {
        if let Some(msg) = crate::ui::chat_bar::queue_user_message(&mut state.main.chat, &prompt) {
            crate::ui::main_view::chat_flow::send_chat_message(state, msg);
        }
    }
}

fn talk_context(state: &AddonState) -> String {
    let who = state
        .main
        .selected_character
        .and_then(|i| state.main.characters.get(i))
        .cloned()
        .unwrap_or_else(|| t("talk.no_character"));
    let prof = state
        .main
        .current_build
        .as_ref()
        .map(|b| b.profession.clone())
        .unwrap_or_else(|| t("talk.any_profession"));
    let role = state
        .main
        .selected_role
        .map(|role| super::super::role_i18n_key(&state.main.game_mode, role))
        .map(t)
        .unwrap_or_else(|| t("talk.no_role"));
    format!(
        "{} \u{00b7} {} \u{00b7} {} \u{00b7} {}",
        who,
        prof,
        state.main.game_mode.label(),
        role
    )
}

#[cfg(test)]
mod tests {
    /// Pull the number out of a `from_secs(N)` call on the given line.
    fn parse_from_secs(line: &str) -> Option<u64> {
        let after = line.split("from_secs(").nth(1)?;
        after.split(')').next()?.trim().parse().ok()
    }

    /// Pull the first run of ASCII digits out of a line.
    fn parse_leading_number(line: &str) -> Option<u64> {
        let digits: String = line
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    }

    #[test]
    fn parse_from_secs_extracts_the_value() {
        assert_eq!(
            parse_from_secs("    const KITCHEN_TIMEOUT: Duration = Duration::from_secs(120);"),
            Some(120),
        );
    }

    #[test]
    fn parse_leading_number_extracts_first_digit_run() {
        assert_eq!(
            parse_leading_number(
                "    /// Wall-clock start of the current kitchen wait (90s, not frame-counted)."
            ),
            Some(90),
        );
    }

    #[test]
    fn kitchen_timeout_comment_matches_constant() {
        // GLM F30: `state.rs`'s `chat_wait_started` doc comment must state
        // the same wait duration as the real `KITCHEN_TIMEOUT` constant in
        // `ui/main_view/mod.rs`'s `render_main`. Both numbers are parsed
        // straight out of the live source files (not duplicated here) so
        // this test cannot pass by accident and fails again the moment
        // either one drifts from the other.
        let mod_src = include_str!("../mod.rs");
        let timeout_line = mod_src
            .lines()
            .find(|l| l.contains("const KITCHEN_TIMEOUT"))
            .expect("KITCHEN_TIMEOUT constant must exist in ui/main_view/mod.rs");
        let constant_secs = parse_from_secs(timeout_line)
            .expect("could not parse KITCHEN_TIMEOUT's from_secs(..) value");

        let state_src = include_str!("../../../state.rs");
        let comment_line = state_src
            .lines()
            .find(|l| l.contains("Wall-clock start of the current kitchen wait"))
            .expect("chat_wait_started doc comment must exist in state.rs");
        let commented_secs = parse_leading_number(comment_line)
            .expect("doc comment must state the wait duration in seconds, e.g. `(120s, ...)`");

        assert_eq!(
            commented_secs, constant_secs,
            "state.rs's chat_wait_started doc comment says {commented_secs}s but \
             KITCHEN_TIMEOUT in ui/main_view/mod.rs is {constant_secs}s -- fix the \
             comment so it states the real timeout",
        );
    }
}
