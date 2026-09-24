//! Radio tab — search radio-browser.info, play icecast streams in the
//! background, favorites, now-playing, choya DJ in the corner.
//!
//! Layout, top to bottom: search row, genre chips, FAVORITES (only when any
//! exist), STATIONS results in a child that takes the remaining height, and a
//! fixed-height player bar pinned at the bottom. The bar's height never
//! changes with state (error hints replace the now-playing line) so nothing
//! below the list ever shifts.

use nexus::imgui::{ChildWindow, ComboBox, DrawListMut, Selectable, Slider, StyleColor, Ui};

use crate::radio::{art, directory, logos, player, RadioSort, RadioStatus, RbStation};
use crate::state::{with_state, AddonState};
use crate::ui::theme;
use gw2_core::config::SavedStation;
use gw2_core::i18n::{t, tf, LANGUAGES};

/// Rows per directory search. The API orders by votes; 50 is plenty to scan.
const SEARCH_LIMIT: usize = 50;
const ROW_H: f32 = 68.0;
const AVATAR: f32 = 56.0;
/// Vertical gap between station rows — the list breathes.
const ROW_GAP: f32 = 8.0;
const FAV_ROW_H: f32 = 44.0;
const FAV_AVATAR: f32 = 36.0;
const HEART: f32 = 22.0;
/// Favorites list shows this many rows before it scrolls.
const FAV_VISIBLE_ROWS: usize = 4;

/// Genre chips: i18n key suffix → radio-browser tag. The empty tag is "Top
/// stations" — no tag filter, so the vote ordering alone decides.
const GENRES: [(&str, &str); 16] = [
    ("top", ""),
    ("pop", "pop"),
    ("rock", "rock"),
    ("news", "news"),
    ("classical", "classical"),
    ("dance", "dance"),
    ("jazz", "jazz"),
    ("eighties", "80s"),
    ("oldies", "oldies"),
    ("talk", "talk"),
    ("electronic", "electronic"),
    ("country", "country"),
    ("hiphop", "hip hop"),
    ("metal", "metal"),
    ("chill", "chillout"),
    ("world", "world music"),
];

pub(in crate::ui::main_view) fn render_radio_tab(ui: &Ui, state: &mut AddonState) {
    ensure_station_list(state);
    search_row(ui, state);
    filter_row(ui, state);
    ui.dummy([0.0, 4.0]);
    genre_chips(ui, state);
    ui.dummy([0.0, 6.0]);
    favorites(ui, state);
    stations(ui, state);
    // One pass per frame: start a logo download worker for whatever the
    // visible rows enqueued above (dedupe-guarded, single-flight).
    logos::kick(state);
    player_bar(ui, state);
}

/// First use each session (the tab, or the mini radio's Next/Previous):
/// restore the remembered genre (World for new listeners) so nobody is
/// greeted by an empty station list.
pub(crate) fn ensure_station_list(state: &mut AddonState) {
    if !state.radio.auto_kicked
        && !state.radio.searching
        && state.radio.results.is_empty()
        && state.radio.search_text.trim().is_empty()
    {
        let stored = state.config.radio.last_genre.clone();
        let tag = GENRES
            .iter()
            .map(|(_, t)| *t)
            .find(|t| *t == stored)
            .unwrap_or("world music");
        state.radio.selected_genre = Some(tag);
        kick_search(state, SearchKind::Tag(tag));
    }
}

// Search + genres

fn search_row(ui: &Ui, state: &mut AddonState) {
    let btn = t("radio.search");
    let btn_w = theme::gold_button_width(ui, &btn);
    let mini_label = t("radio.mini.toggle");
    let mini_w = ui.calc_text_size(&mini_label)[0]
        + theme::control_height(ui) * 2.0
        + ui.clone_style().item_inner_spacing[0]
        + 20.0;
    let avail = ui.content_region_avail()[0];
    ui.set_next_item_width((avail - btn_w - mini_w - 8.0).max(120.0));
    let entered = ui
        .input_text("##radio_q", &mut state.radio.search_text)
        .hint(&t("radio.search_hint"))
        .enter_returns_true(true)
        .build();
    ui.same_line_with_spacing(0.0, 8.0);
    let clicked = theme::gold_button_sized(ui, &btn, [0.0, 0.0]);
    ui.same_line_with_spacing(0.0, 12.0);
    crate::ui::mini_radio::render_tab_toggle(ui, state, &mini_label);
    if state.radio.searching {
        ui.same_line_with_spacing(0.0, 10.0);
        ui.align_text_to_frame_padding();
        ui.text_colored(theme::pal().muted, t("radio.searching"));
    }
    if entered || clicked {
        let query = state.radio.search_text.trim().to_string();
        if !query.is_empty() {
            state.radio.selected_genre = None;
            kick_search(state, SearchKind::Name(query));
        }
    }
}

fn genre_chips(ui: &Ui, state: &mut AddonState) {
    theme::font_scale(ui, 0.85);
    ui.text_colored(theme::pal().muted, t("radio.genres"));
    theme::font_scale_reset(ui);
    let avail = ui.content_region_avail()[0];
    let mut row_x = 0.0_f32;
    let mut pick: Option<&'static str> = None;
    for (i, (key, tag)) in GENRES.iter().enumerate() {
        let label = t(&format!("radio.genre.{key}"));
        let [w, _] = theme::select_chip_size(ui, &label, false);
        theme::wrap_chip(ui, avail, &mut row_x, w, 4.0);
        let on = state.radio.selected_genre == Some(*tag);
        if theme::select_chip(ui, &label, on, &format!("##radio_genre_{i}"), None)
            && !state.radio.searching
        {
            pick = Some(*tag);
        }
    }
    if let Some(tag) = pick {
        state.radio.selected_genre = Some(tag);
        state.config.radio.last_genre = tag.to_string();
        crate::ui::save_config_detached(state);
        kick_search(state, SearchKind::Tag(tag));
    }
}

enum SearchKind {
    Name(String),
    Tag(&'static str),
}

/// UI locale code -> radio-browser language NAME (its `language=` search
/// param filters on names, not ISO codes).
const LOCALE_TO_RB: [(&str, &str); 12] = [
    ("en", "english"),
    ("de", "german"),
    ("es", "spanish"),
    ("fr", "french"),
    ("it", "italian"),
    ("nl", "dutch"),
    ("pl", "polish"),
    ("pt", "portuguese"),
    ("ru", "russian"),
    ("ja", "japanese"),
    ("ko", "korean"),
    ("zh", "chinese"),
];

/// Country combo: ISO 3166-1 alpha-2 -> English name. English throughout —
/// internationally recognizable, needs no translation into 12 locales, and
/// safe in every glyph set the overlay ships.
const COUNTRIES: [(&str, &str); 34] = [
    ("US", "United States"),
    ("GB", "United Kingdom"),
    ("IE", "Ireland"),
    ("CA", "Canada"),
    ("AU", "Australia"),
    ("DE", "Germany"),
    ("AT", "Austria"),
    ("CH", "Switzerland"),
    ("FR", "France"),
    ("BE", "Belgium"),
    ("NL", "Netherlands"),
    ("ES", "Spain"),
    ("PT", "Portugal"),
    ("IT", "Italy"),
    ("PL", "Poland"),
    ("CZ", "Czechia"),
    ("SI", "Slovenia"),
    ("HR", "Croatia"),
    ("HU", "Hungary"),
    ("RO", "Romania"),
    ("GR", "Greece"),
    ("SE", "Sweden"),
    ("NO", "Norway"),
    ("DK", "Denmark"),
    ("FI", "Finland"),
    ("RU", "Russia"),
    ("BR", "Brazil"),
    ("MX", "Mexico"),
    ("AR", "Argentina"),
    ("TR", "Turkey"),
    ("JP", "Japan"),
    ("KR", "South Korea"),
    ("CN", "China"),
    ("IN", "India"),
];

/// Language + country filter combos. Persisted in config; a change re-runs
/// whatever search context is active so the list updates immediately.
fn filter_row(ui: &Ui, state: &mut AddonState) {
    let font_pref = state.config.ui_font.clone();
    let ui_lang = state.config.ui_language.clone();
    let mut changed = false;

    ui.align_text_to_frame_padding();
    theme::font_scale(ui, 0.85);
    ui.text_colored(theme::pal().muted, t("radio.language"));
    theme::font_scale_reset(ui);
    ui.same_line_with_spacing(0.0, 6.0);
    let cur_lang = state.config.radio.language_filter.clone();
    let preview = language_filter_label(&cur_lang, &font_pref, &ui_lang);
    ui.set_next_item_width(theme::combo_width_for(ui, &preview).max(150.0));
    if let Some(_c) = ComboBox::new("##radio_lang")
        .preview_value(&preview)
        .begin(ui)
    {
        for value in ["auto", "any"]
            .into_iter()
            .chain(LOCALE_TO_RB.iter().map(|(_, rb)| *rb))
        {
            let label = language_filter_label(value, &font_pref, &ui_lang);
            if Selectable::new(format!("{label}##radio_lang_{value}"))
                .selected(cur_lang == value)
                .build(ui)
                && cur_lang != value
            {
                state.config.radio.language_filter = value.to_string();
                changed = true;
            }
        }
    }

    ui.same_line_with_spacing(0.0, 14.0);
    theme::font_scale(ui, 0.85);
    ui.text_colored(theme::pal().muted, t("radio.country"));
    theme::font_scale_reset(ui);
    ui.same_line_with_spacing(0.0, 6.0);
    let cur_cc = state.config.radio.country_filter.clone();
    let cc_preview = country_filter_label(&cur_cc);
    ui.set_next_item_width(theme::combo_width_for(ui, &cc_preview).max(150.0));
    if let Some(_c) = ComboBox::new("##radio_cc")
        .preview_value(&cc_preview)
        .begin(ui)
    {
        for value in ["any"]
            .into_iter()
            .chain(COUNTRIES.iter().map(|(cc, _)| *cc))
        {
            let label = country_filter_label(value);
            if Selectable::new(format!("{label}##radio_cc_{value}"))
                .selected(cur_cc == value)
                .build(ui)
                && cur_cc != value
            {
                state.config.radio.country_filter = value.to_string();
                changed = true;
            }
        }
    }

    ui.same_line_with_spacing(0.0, 14.0);
    theme::font_scale(ui, 0.85);
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("radio.bitrate"));
    theme::font_scale_reset(ui);
    ui.same_line_with_spacing(0.0, 6.0);
    let cur_cap = state.config.radio.bitrate_max;
    let cap_preview = bitrate_cap_label(ui, cur_cap);
    ui.set_next_item_width(theme::combo_width_for(ui, &cap_preview).max(110.0));
    if let Some(_c) = ComboBox::new("##radio_cap")
        .preview_value(&cap_preview)
        .begin(ui)
    {
        for cap in [0u32, 64, 128, 192] {
            let label = bitrate_cap_label(ui, cap);
            if Selectable::new(format!("{label}##radio_cap_{cap}"))
                .selected(cur_cap == cap)
                .build(ui)
                && cur_cap != cap
            {
                state.config.radio.bitrate_max = cap;
                changed = true;
            }
        }
    }

    ui.same_line_with_spacing(0.0, 14.0);
    theme::font_scale(ui, 0.85);
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("radio.sort"));
    theme::font_scale_reset(ui);
    ui.same_line_with_spacing(0.0, 6.0);
    let cur_sort = state.radio.sort;
    let sort_preview = sort_label(cur_sort);
    ui.set_next_item_width(theme::combo_width_for(ui, &sort_preview).max(110.0));
    if let Some(_c) = ComboBox::new("##radio_sort")
        .preview_value(&sort_preview)
        .begin(ui)
    {
        for (i, opt) in [
            RadioSort::Popular,
            RadioSort::Name,
            RadioSort::Bitrate,
            RadioSort::Country,
        ]
        .into_iter()
        .enumerate()
        {
            if Selectable::new(format!("{}##radio_sort_{i}", sort_label(opt)))
                .selected(cur_sort == opt)
                .build(ui)
                && cur_sort != opt
            {
                state.radio.sort = opt;
                apply_sort(&mut state.radio.results, opt);
            }
        }
    }

    if changed {
        crate::ui::save_config_detached(state);
        rekick_current(state);
    }
}

/// "Any" or "128 kbps" — a cap for poor connections, reusing the existing
/// bitrate format key (ASCII only; the game font lacks a <= glyph).
fn bitrate_cap_label(_ui: &Ui, cap: u32) -> String {
    if cap == 0 {
        t("radio.filter_any")
    } else {
        tf("fmt.radio_bitrate", &[("kbps", &cap.to_string())])
    }
}

fn sort_label(sort: RadioSort) -> String {
    match sort {
        RadioSort::Popular => t("radio.sort.popular"),
        RadioSort::Name => t("radio.sort.name"),
        RadioSort::Bitrate => t("radio.bitrate"),
        RadioSort::Country => t("radio.country"),
    }
}

/// Client-side result ordering; stable, so equal keys keep the API's vote
/// order as the tiebreak.
fn apply_sort(list: &mut [RbStation], sort: RadioSort) {
    match sort {
        RadioSort::Popular => list.sort_by_key(|s| std::cmp::Reverse(s.votes)),
        RadioSort::Name => list.sort_by_key(|s| s.name.to_lowercase()),
        RadioSort::Bitrate => list.sort_by_key(|s| std::cmp::Reverse(s.bitrate)),
        RadioSort::Country => list.sort_by(|a, b| {
            a.countrycode
                .cmp(&b.countrycode)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
}

/// Display label for a language-filter value ("auto"/"any"/rb name). Native
/// names via the same CJK-gated helper the Settings language list uses.
fn language_filter_label(value: &str, font_pref: &str, ui_lang: &str) -> String {
    match value {
        "auto" => t("radio.filter_auto"),
        "any" => t("radio.filter_any"),
        rb => LOCALE_TO_RB
            .iter()
            .find(|(_, name)| *name == rb)
            .and_then(|(code, _)| LANGUAGES.iter().find(|l| l.code == *code))
            .map(|l| crate::ui::fonts::language_label(l, font_pref, ui_lang).to_string())
            .unwrap_or_else(|| rb.to_string()),
    }
}

fn country_filter_label(value: &str) -> String {
    if value == "any" {
        return t("radio.filter_any");
    }
    COUNTRIES
        .iter()
        .find(|(cc, _)| *cc == value)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| value.to_string())
}

/// Resolve the persisted filter settings into directory search params.
fn active_filters(state: &AddonState) -> directory::SearchFilters {
    let language = match state.config.radio.language_filter.as_str() {
        "any" => None,
        "auto" => {
            let ui = gw2_core::i18n::current();
            LOCALE_TO_RB
                .iter()
                .find(|(code, _)| *code == ui)
                .map(|(_, rb)| (*rb).to_string())
        }
        rb => Some(rb.to_string()),
    };
    let countrycode = match state.config.radio.country_filter.as_str() {
        "any" => None,
        cc => Some(cc.to_string()),
    };
    directory::SearchFilters {
        language,
        countrycode,
        max_bitrate: match state.config.radio.bitrate_max {
            0 => None,
            n => Some(n),
        },
    }
}

/// Re-run whatever search context is active (name if typed, else the
/// selected genre chip, else Top stations) — used when a filter changes.
fn rekick_current(state: &mut AddonState) {
    let query = state.radio.search_text.trim().to_string();
    if !query.is_empty() {
        kick_search(state, SearchKind::Name(query));
    } else {
        let tag = state.radio.selected_genre.unwrap_or("");
        state.radio.selected_genre = Some(tag);
        kick_search(state, SearchKind::Tag(tag));
    }
}

/// Start a directory-search worker, publishing back via `with_state` — same
/// shape as `news::kick`. Double-kicks are guarded by `radio.searching`.
fn kick_search(state: &mut AddonState, kind: SearchKind) {
    if state.radio.searching {
        return;
    }
    state.radio.auto_kicked = true;
    state.radio.searching = true;
    state.radio.last_error = None;
    let filters = active_filters(state);
    let spawned = state.spawn_worker("radio-search", move |token| {
        // catch_unwind so a panicking search can never strand the spinner.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &kind {
            SearchKind::Name(q) => directory::search_by_name(q, &filters, SEARCH_LIMIT),
            SearchKind::Tag(tag) => directory::search_by_tag(tag, &filters, SEARCH_LIMIT),
        }))
        .unwrap_or_else(|_| Err("station search failed".into()));
        if token.is_cancelled() {
            return;
        }
        let _ = with_state(|s| {
            s.radio.searching = false;
            match result {
                Ok(mut list) => {
                    apply_sort(&mut list, s.radio.sort);
                    // Pin the live station into the results if the search
                    // didn't include it, so its row (and the controls that
                    // live on it) is always reachable — a fringe favorite is
                    // not guaranteed a spot in a genre's top 50.
                    if matches!(
                        s.radio.status,
                        RadioStatus::Connecting
                            | RadioStatus::Buffering
                            | RadioStatus::Playing
                            | RadioStatus::Paused
                            | RadioStatus::Stalled
                    ) {
                        if let Some(cur) = s.radio.current.clone() {
                            let key = fav_key(&cur.stationuuid, cur.stream_url());
                            if !list
                                .iter()
                                .any(|r| fav_key(&r.stationuuid, r.stream_url()) == key)
                            {
                                list.insert(0, cur);
                            }
                        }
                    }
                    s.radio.results = list;
                }
                Err(err) => s.radio.last_error = Some(err),
            }
        });
    });
    if !spawned {
        state.radio.searching = false;
    }
}

// Favorites + station list

fn favorites(ui: &Ui, state: &mut AddonState) {
    if state.config.radio.favorites.is_empty() {
        return;
    }
    theme::header(ui, &t("radio.favorites"));
    let n = state.config.radio.favorites.len();
    let h = (n.min(FAV_VISIBLE_ROWS) as f32) * (FAV_ROW_H + 4.0) + 4.0;
    let mut play: Option<RbStation> = None;
    let mut remove: Option<String> = None;
    let current_key = current_station_key(state);
    {
        let st: &AddonState = state;
        ChildWindow::new("##radio_favs")
            .size([0.0, h])
            .build(ui, || {
                for (i, f) in st.config.radio.favorites.iter().enumerate() {
                    let key = fav_key(&f.stationuuid, &f.url);
                    let active = current_key.as_deref() == Some(key.as_str());
                    match favorite_row(ui, i, f, active) {
                        RowAction::Play => play = Some(player::station_from_saved(f)),
                        RowAction::Heart => remove = Some(key.clone()),
                        RowAction::None => {}
                    }
                }
            });
    }
    if let Some(key) = remove {
        state
            .config
            .radio
            .favorites
            .retain(|f| fav_key(&f.stationuuid, &f.url) != key);
        crate::ui::save_config_detached(state);
    }
    if let Some(station) = play {
        // Load the station's genre into the stations list below, so the
        // playing row (and its controls) appears there immediately. No genre
        // tag recognized -> search the station's name instead; either way the
        // search-completion pin guarantees the row shows up.
        let tags = station.tags.to_lowercase();
        let genre = GENRES
            .iter()
            .skip(1) // skip "Top stations" (empty tag matches everything)
            .map(|(_, t)| *t)
            .find(|t| tags.contains(*t));
        match genre {
            Some(tag) => {
                state.radio.selected_genre = Some(tag);
                state.config.radio.last_genre = tag.to_string();
                crate::ui::save_config_detached(state);
                kick_search(state, SearchKind::Tag(tag));
            }
            None => {
                state.radio.selected_genre = None;
                state.radio.search_text = station.name.clone();
                kick_search(state, SearchKind::Name(station.name.clone()));
            }
        }
        start_play(state, station);
    }
}

fn stations(ui: &Ui, state: &mut AddonState) {
    theme::header(ui, &t("radio.results"));
    let ctl_on_row = controls_on_active_row(state);
    let bar_h = player_bar_height(ui, !ctl_on_row);
    let h = (ui.content_region_avail()[1] - bar_h - 10.0).max(72.0);
    let mut play: Option<RbStation> = None;
    let mut fav_toggle: Option<RbStation> = None;
    let mut ctl_actions: Vec<CtlAction> = Vec::new();
    let current_key = current_station_key(state);
    {
        let st: &AddonState = state;
        let ctl = ctl_on_row.then(|| RowCtl {
            status: st.radio.status.clone(),
            volume: st.config.radio.volume_percent,
            duck: st.config.radio.duck_in_combat,
            quips: st.config.radio.ai_quips,
        });
        ChildWindow::new("##radio_stations")
            .size([0.0, h])
            .build(ui, || {
                if st.radio.searching {
                    ui.dummy([0.0, 8.0]);
                    theme::wrapped(ui, theme::pal().muted, &t("radio.searching"));
                } else if let Some(err) = &st.radio.last_error {
                    ui.dummy([0.0, 8.0]);
                    theme::wrapped(ui, theme::ERR, err);
                    theme::font_scale(ui, 0.85);
                    theme::wrapped(ui, theme::pal().muted, &t("radio.error.av_hint"));
                    theme::font_scale_reset(ui);
                } else if st.radio.results.is_empty() {
                    ui.dummy([0.0, 8.0]);
                    // "No stations found" only after an actual search came back
                    // empty; the untouched tab just nudges toward the heart.
                    let searched = !st.radio.search_text.trim().is_empty()
                        || st.radio.selected_genre.is_some();
                    if searched {
                        theme::wrapped(ui, theme::pal().muted, &t("radio.no_results"));
                    }
                    if st.config.radio.favorites.is_empty() {
                        theme::wrapped(ui, theme::pal().muted, &t("radio.no_favorites"));
                    }
                } else {
                    for (i, s) in st.radio.results.iter().enumerate() {
                        let key = fav_key(&s.stationuuid, s.stream_url());
                        let active = current_key.as_deref() == Some(key.as_str());
                        let fav = is_favorite(st, s);
                        let row_ctl = if active { ctl.as_ref() } else { None };
                        match station_row(ui, i, s, fav, active, row_ctl, &mut ctl_actions) {
                            RowAction::Play => play = Some(s.clone()),
                            RowAction::Heart => fav_toggle = Some(s.clone()),
                            RowAction::None => {}
                        }
                    }
                }
            });
    }
    // Deferred row-control mutations (the child closure only had &state).
    for action in ctl_actions {
        match action {
            CtlAction::Pause => state.radio.status = RadioStatus::Paused,
            CtlAction::Resume => state.radio.status = RadioStatus::Playing,
            CtlAction::Stop => state.radio.status = RadioStatus::Stopped,
            CtlAction::Volume(v) => state.config.radio.volume_percent = v,
            CtlAction::VolumeCommit => crate::ui::save_config_detached(state),
            CtlAction::ToggleDuck => {
                state.config.radio.duck_in_combat = !state.config.radio.duck_in_combat;
                crate::ui::save_config_detached(state);
            }
            CtlAction::ToggleQuips => {
                state.config.radio.ai_quips = !state.config.radio.ai_quips;
                crate::ui::save_config_detached(state);
            }
        }
    }
    if let Some(station) = fav_toggle {
        toggle_favorite(state, &station, art::tick());
    }
    if let Some(station) = play {
        start_play(state, station);
    }
}

enum RowAction {
    None,
    Play,
    Heart,
}

/// Deferred playback-control actions from the active station's row. The row
/// renders inside the stations child, which only holds `&AddonState` — the
/// mutations run after the child closes. (Direct `player::*` calls happen
/// in-row; they never touch STATE.)
enum CtlAction {
    Pause,
    Resume,
    Stop,
    Volume(u8),
    VolumeCommit,
    ToggleDuck,
    ToggleQuips,
}

/// Snapshot handed to the active row so it can draw the controls cluster.
struct RowCtl {
    status: RadioStatus,
    volume: u8,
    duck: bool,
    quips: bool,
}

/// Whether the playback controls live on the active station's row this frame
/// (they do whenever a session is live AND its row is in the results list);
/// otherwise they fall back to the player bar.
fn controls_on_active_row(state: &AddonState) -> bool {
    if !matches!(
        state.radio.status,
        RadioStatus::Connecting
            | RadioStatus::Buffering
            | RadioStatus::Playing
            | RadioStatus::Paused
            | RadioStatus::Stalled
    ) {
        return false;
    }
    let Some(key) = current_station_key(state) else {
        return false;
    };
    state
        .radio
        .results
        .iter()
        .any(|s| fav_key(&s.stationuuid, s.stream_url()) == key)
}

/// Slider width in the row's controls cluster.
const ROW_VOL_W: f32 = 220.0;

/// Measured footprint of the active row's controls. Computed BEFORE the
/// row's invisible click-catcher is submitted so its hit zone can exclude
/// the whole cluster — in ImGui the first-submitted item under the mouse
/// wins the click, so an overlapping row button would eat every slider drag
/// and button press and re-tune the station instead (shipped bug).
struct CtlLayout {
    btn_w: f32,
    with_checks: bool,
    /// Width reserved at the row's right edge (hit-zone exclusion + text clip).
    reserve: f32,
}

fn row_ctl_layout(ui: &Ui, ctl: &RowCtl, origin: [f32; 2], heart_x: f32) -> CtlLayout {
    let ch = theme::control_height(ui);
    let btn_w = theme::gold_button_width(ui, t("radio.play"))
        .max(theme::gold_button_width(ui, t("radio.stop")))
        .max(theme::gold_button_width(ui, t("radio.pause")));
    match ctl.status {
        RadioStatus::Playing | RadioStatus::Paused => {
            theme::font_scale(ui, 0.85);
            let vol_label_w = ui.calc_text_size(t("radio.volume"))[0] + 6.0;
            let duck_w = ui.calc_text_size(t("radio.duck_in_combat"))[0] + ch + 8.0;
            let quips_w = ui.calc_text_size(t("radio.ai_quips"))[0] + ch + 8.0;
            theme::font_scale_reset(ui);
            let base_w = btn_w + 8.0 + btn_w + 14.0 + vol_label_w + ROW_VOL_W;
            let with_checks =
                heart_x - 12.0 - (base_w + 16.0 + duck_w + quips_w) > origin[0] + AVATAR + 320.0;
            let total = base_w
                + if with_checks {
                    16.0 + duck_w + quips_w
                } else {
                    0.0
                };
            CtlLayout {
                btn_w,
                with_checks,
                reserve: total + 12.0,
            }
        }
        RadioStatus::Connecting | RadioStatus::Buffering | RadioStatus::Stalled => {
            theme::font_scale(ui, 1.3);
            let text_reserve = ui.calc_text_size(format!(
                "{}...",
                tuning_label(&ctl.status)
                    .trim_end_matches('.')
                    .to_uppercase()
            ))[0];
            theme::font_scale_reset(ui);
            CtlLayout {
                btn_w,
                with_checks: false,
                reserve: btn_w + 28.0 + text_reserve,
            }
        }
        _ => CtlLayout {
            btn_w,
            with_checks: false,
            reserve: 0.0,
        },
    }
}

fn tuning_label(status: &RadioStatus) -> String {
    match status {
        RadioStatus::Connecting => t("radio.status.connecting"),
        RadioStatus::Buffering => t("radio.status.buffering"),
        _ => t("radio.status.stalled"),
    }
}

/// Controls cluster on the active row, right-aligned before the heart.
/// Connecting/Buffering/Stalled: big ALL-CAPS status with additive dots
/// (dot, dot, dot, clear) breathing in alpha, plus Stop. Playing/Paused:
/// Pause|Play, Stop, a labeled volume slider, and the two checkboxes when
/// the row is wide enough.
fn row_controls(
    ui: &Ui,
    dl: &DrawListMut,
    ctl: &RowCtl,
    layout: &CtlLayout,
    origin: [f32; 2],
    heart_x: f32,
    out: &mut Vec<CtlAction>,
) {
    let ch = theme::control_height(ui);
    let cy = origin[1] + (ROW_H - ch) * 0.5;
    let play_l = t("radio.play");
    let stop_l = t("radio.stop");
    let pause_l = t("radio.pause");
    let btn_w = layout.btn_w;
    let tf = ui.frame_count() as u32;

    match ctl.status {
        RadioStatus::Playing | RadioStatus::Paused => {
            ui.set_cursor_screen_pos([heart_x - 12.0 - (layout.reserve - 12.0), cy]);
            if ctl.status == RadioStatus::Playing {
                if theme::gold_button_sized(ui, format!("{pause_l}##row_pause"), [btn_w, 0.0]) {
                    player::pause();
                    out.push(CtlAction::Pause);
                }
            } else if theme::gold_button_sized(ui, format!("{play_l}##row_resume"), [btn_w, 0.0]) {
                player::resume();
                out.push(CtlAction::Resume);
            }
            ui.same_line_with_spacing(0.0, 8.0);
            if theme::gold_button_sized(ui, format!("{stop_l}##row_stop"), [btn_w, 0.0]) {
                player::request_stop();
                out.push(CtlAction::Stop);
            }
            // "Volume" label so the slider reads as one, then the slider.
            ui.same_line_with_spacing(0.0, 14.0);
            theme::font_scale(ui, 0.85);
            ui.align_text_to_frame_padding();
            ui.text_colored(theme::pal().muted, t("radio.volume"));
            theme::font_scale_reset(ui);
            ui.same_line_with_spacing(0.0, 6.0);
            ui.set_next_item_width(ROW_VOL_W);
            let mut vol = ctl.volume;
            if Slider::new("##row_vol", 0u8, 100u8).build(ui, &mut vol) {
                player::set_volume(vol);
                out.push(CtlAction::Volume(vol));
            }
            if ui.is_item_deactivated_after_edit() {
                out.push(CtlAction::VolumeCommit);
            }
            if layout.with_checks {
                ui.same_line_with_spacing(0.0, 12.0);
                theme::font_scale(ui, 0.85);
                let duck_l = t("radio.duck_in_combat");
                let quips_l = t("radio.ai_quips");
                let mut duck = ctl.duck;
                if ui.checkbox(format!("{duck_l}##row_duck"), &mut duck) {
                    out.push(CtlAction::ToggleDuck);
                }
                ui.same_line_with_spacing(0.0, 8.0);
                let mut quips = ctl.quips;
                if ui.checkbox(format!("{quips_l}##row_quips"), &mut quips) {
                    out.push(CtlAction::ToggleQuips);
                }
                theme::font_scale_reset(ui);
            }
        }
        RadioStatus::Connecting | RadioStatus::Buffering | RadioStatus::Stalled => {
            let stop_x = heart_x - 12.0 - btn_w;
            ui.set_cursor_screen_pos([stop_x, cy]);
            if theme::gold_button_sized(ui, format!("{stop_l}##row_stop"), [btn_w, 0.0]) {
                player::request_stop();
                out.push(CtlAction::Stop);
            }
            // ALL CAPS, dots appended additively (., .., ..., clear), alpha
            // breathing slowly. The layout reserve includes the full "..."
            // so the base text never shifts as dots grow.
            let base = tuning_label(&ctl.status)
                .trim_end_matches('.')
                .to_uppercase();
            let dots = ".".repeat(((tf / 20) % 4) as usize);
            let breath = 0.5 + 0.35 * (tf as f32 * 0.045).sin();
            theme::font_scale(ui, 1.3);
            let reserve = ui.calc_text_size(format!("{base}..."))[0];
            let ty = origin[1] + (ROW_H - ui.text_line_height()) * 0.5;
            let bx = stop_x - 16.0 - reserve;
            let text = format!("{base}{dots}");
            dl.add_text(
                [bx + 1.0, ty + 1.0],
                crate::ui::color_u32([0.0, 0.0, 0.0, 0.7 * breath]),
                &text,
            );
            dl.add_text(
                [bx, ty],
                crate::ui::color_u32([
                    theme::pal().gold[0],
                    theme::pal().gold[1],
                    theme::pal().gold[2],
                    breath,
                ]),
                &text,
            );
            theme::font_scale_reset(ui);
        }
        _ => {}
    }
}

/// Two-line result row: letter-plate avatar, name, muted country • codec •
/// bitrate, heart on the right. Row click plays; the heart has its own
/// non-overlapping hit zone so a heart click never also tunes in. The active
/// row additionally hosts the playback controls (or the tuning text).
fn station_row(
    ui: &Ui,
    i: usize,
    s: &RbStation,
    fav: bool,
    active: bool,
    ctl: Option<&RowCtl>,
    ctl_out: &mut Vec<CtlAction>,
) -> RowAction {
    let origin = ui.cursor_screen_pos();
    let avail = ui.content_region_avail()[0];
    let heart_x = origin[0] + avail - HEART - 10.0;
    let row_w = (heart_x - 8.0 - origin[0]).max(40.0);

    // Measure the controls cluster FIRST so the row's click-catcher can
    // exclude it entirely: in ImGui the first-submitted item under the mouse
    // wins the click, so an overlapping row button would eat slider drags
    // and button presses and re-tune the station instead.
    let layout = ctl.map(|c| row_ctl_layout(ui, c, origin, heart_x));
    let ctl_w = layout.as_ref().map_or(0.0, |l| l.reserve);
    let hit_w = (row_w - ctl_w).max(40.0);

    let mut action = RowAction::None;
    if ui.invisible_button(format!("##radio_row_{i}"), [hit_w, ROW_H]) {
        action = RowAction::Play;
    }
    let hovered = ui.is_item_hovered();
    let dl = crate::ui::window_draw_list(ui);
    row_plate(&dl, origin, avail, ROW_H, hovered, active);

    let av_y = origin[1] + (ROW_H - AVATAR) * 0.5;
    station_avatar(
        ui,
        &dl,
        [origin[0] + 2.0, av_y],
        AVATAR,
        &s.name,
        &s.favicon,
    );

    // Controls (or tuning text) on the active row, drawn after the plate so
    // they sit on top of it visually; their hit zone is already carved out
    // of the row button above.
    if let (Some(c), Some(l)) = (ctl, layout.as_ref()) {
        row_controls(ui, &dl, c, l, origin, heart_x, ctl_out);
    }

    let tx = origin[0] + 2.0 + AVATAR + 10.0;
    let text_w = (row_w - (AVATAR + 14.0) - ctl_w).max(60.0);
    let name_color = if active {
        theme::pal().gold
    } else {
        theme::pal().cream
    };
    let name = clip_text(ui, &s.name, text_w);
    let lh = ui.text_line_height();
    dl.add_text(
        [tx, origin[1] + ROW_H * 0.5 - lh - 2.0],
        crate::ui::color_u32(name_color),
        &name,
    );

    theme::font_scale(ui, 0.85);
    let meta = meta_line(&s.countrycode, &s.codec, s.bitrate);
    let meta = clip_text(ui, &meta, text_w);
    dl.add_text(
        [tx, origin[1] + ROW_H * 0.5 + 3.0],
        crate::ui::color_u32(theme::pal().muted),
        &meta,
    );
    theme::font_scale_reset(ui);

    if heart_button(
        ui,
        &dl,
        &format!("##radio_fav_{i}"),
        heart_x,
        origin[1],
        ROW_H,
        fav,
    ) {
        action = RowAction::Heart;
    }
    ui.set_cursor_screen_pos([origin[0], origin[1] + ROW_H + ROW_GAP]);
    action
}

/// One-line favorite row; the heart removes.
fn favorite_row(ui: &Ui, i: usize, f: &SavedStation, active: bool) -> RowAction {
    let origin = ui.cursor_screen_pos();
    let avail = ui.content_region_avail()[0];
    let heart_x = origin[0] + avail - HEART - 10.0;
    let row_w = (heart_x - 8.0 - origin[0]).max(40.0);
    let mut action = RowAction::None;
    if ui.invisible_button(format!("##radio_favrow_{i}"), [row_w, FAV_ROW_H]) {
        action = RowAction::Play;
    }
    let hovered = ui.is_item_hovered();
    let dl = crate::ui::window_draw_list(ui);
    row_plate(&dl, origin, avail, FAV_ROW_H, hovered, active);

    let av_y = origin[1] + (FAV_ROW_H - FAV_AVATAR) * 0.5;
    station_avatar(
        ui,
        &dl,
        [origin[0] + 2.0, av_y],
        FAV_AVATAR,
        &f.name,
        &f.favicon,
    );

    let tx = origin[0] + 2.0 + FAV_AVATAR + 8.0;
    let text_w = row_w - (FAV_AVATAR + 12.0);
    let name_color = if active {
        theme::pal().gold
    } else {
        theme::pal().cream
    };
    let name = clip_text(ui, &f.name, text_w);
    let th = ui.calc_text_size(&name)[1];
    dl.add_text(
        [tx, origin[1] + (FAV_ROW_H - th) * 0.5],
        crate::ui::color_u32(name_color),
        &name,
    );

    if heart_button(
        ui,
        &dl,
        &format!("##radio_favdel_{i}"),
        heart_x,
        origin[1],
        FAV_ROW_H,
        true,
    ) {
        action = RowAction::Heart;
    }
    ui.set_cursor_screen_pos([origin[0], origin[1] + FAV_ROW_H + 4.0]);
    action
}

fn row_plate(dl: &DrawListMut, origin: [f32; 2], w: f32, h: f32, hovered: bool, active: bool) {
    if !hovered && !active {
        return;
    }
    let fill = if active {
        theme::with_alpha(theme::pal().gold_hover, 0.55)
    } else {
        theme::pal().gold_hover
    };
    dl.add_rect(
        [origin[0] - 2.0, origin[1]],
        [origin[0] + w, origin[1] + h],
        fill,
    )
    .filled(true)
    .rounding(4.0)
    .build();
}

/// Station avatar: the real logo once `radio::logos` has the favicon cached
/// and decoded, the letter plate while it loads — or permanently, when the
/// favicon is missing, failed, undecodable (.ico/.webp/.svg), or the session
/// texture budget is spent. Only rows inside the child's clip rect ask the
/// pipeline, so a search result never queues 50 downloads at once.
fn station_avatar(ui: &Ui, dl: &DrawListMut, p: [f32; 2], size: f32, name: &str, favicon: &str) {
    let p_max = [p[0] + size, p[1] + size];
    if !favicon.is_empty() && ui.is_rect_visible(p, p_max) {
        if let Some(tid) = logos::texture(favicon) {
            let r = size * 0.5;
            dl.add_image_rounded(tid, p, p_max, r)
                .col([1.0, 1.0, 1.0, 1.0])
                .build();
            dl.add_rect(p, p_max, theme::pal().gold_dim)
                .rounding(r)
                .build();
            return;
        }
    }
    letter_avatar(ui, dl, p, size, name.chars().next().unwrap_or('#'));
}

/// Letter-plate avatar in the `icons::paint_avatar` fallback style — the
/// immediate stand-in while a favicon downloads, and the permanent avatar for
/// stations without a usable one.
fn letter_avatar(ui: &Ui, dl: &DrawListMut, p: [f32; 2], size: f32, letter: char) {
    let p_max = [p[0] + size, p[1] + size];
    let r = size * 0.5;
    dl.add_rect(p, p_max, theme::pal().plate)
        .filled(true)
        .rounding(r)
        .build();
    let s: String = letter.to_uppercase().collect();
    let sz = ui.calc_text_size(&s);
    dl.add_text(
        [p[0] + (size - sz[0]) * 0.5, p[1] + (size - sz[1]) * 0.5],
        crate::ui::color_u32(theme::CURRENT),
        &s,
    );
    dl.add_rect(p, p_max, theme::pal().gold_dim)
        .rounding(r)
        .build();
}

/// Heart toggle with its own hit zone; returns true on click.
fn heart_button(
    ui: &Ui,
    dl: &DrawListMut,
    id: &str,
    x: f32,
    row_y: f32,
    row_h: f32,
    fav: bool,
) -> bool {
    let y = row_y + (row_h - HEART) * 0.5;
    ui.set_cursor_screen_pos([x, y]);
    let clicked = ui.invisible_button(id, [HEART, HEART]);
    let hovered = ui.is_item_hovered();
    let color = match (fav, hovered) {
        (true, true) => [1.0, 0.45, 0.55, 1.0],
        (true, false) => [0.92, 0.30, 0.42, 1.0],
        (false, true) => [0.85, 0.45, 0.55, 0.95],
        (false, false) => theme::with_alpha(theme::pal().muted, 0.85),
    };
    heart_glyph(dl, [x + HEART * 0.5, y + HEART * 0.5], HEART * 0.46, color);
    if hovered {
        ui.tooltip_text(t(if fav {
            "radio.remove_favorite"
        } else {
            "radio.add_favorite"
        }));
    }
    clicked
}

/// Two lobes + a point — reads as a heart down to ~12px without an icon font.
pub(crate) fn heart_glyph(dl: &DrawListMut, c: [f32; 2], r: f32, color: [f32; 4]) {
    let lobe = r * 0.52;
    let ly = c[1] - r * 0.30;
    dl.add_circle([c[0] - lobe * 0.92, ly], lobe, color)
        .filled(true)
        .build();
    dl.add_circle([c[0] + lobe * 0.92, ly], lobe, color)
        .filled(true)
        .build();
    dl.add_triangle(
        [c[0] - r * 0.96, ly + lobe * 0.35],
        [c[0] + r * 0.96, ly + lobe * 0.35],
        [c[0], c[1] + r * 0.95],
        color,
    )
    .filled(true)
    .build();
}

// Player bar

/// Bar height: the status line plus the big now-playing marquee line (1.5x
/// scale), and — only when the controls are NOT on the active station's row —
/// the controls row. The error hint borrows the now-playing line.
fn player_bar_height(ui: &Ui, controls_in_bar: bool) -> f32 {
    let line_h = ui.text_line_height();
    let base = 6.0 + line_h + 3.0 + line_h * 3.05 + 8.0;
    if controls_in_bar {
        base + theme::control_height(ui) + 4.0
    } else {
        base
    }
}

fn player_bar(ui: &Ui, state: &mut AddonState) {
    let ctl_in_bar = !controls_on_active_row(state);
    let bar_h = player_bar_height(ui, ctl_in_bar);
    let origin = ui.cursor_screen_pos();
    let w = ui.content_region_avail()[0];
    let line_h = ui.text_line_height();
    {
        let dl = crate::ui::window_draw_list(ui);
        dl.add_rect(
            [origin[0] - 2.0, origin[1]],
            [origin[0] + w + 2.0, origin[1] + bar_h],
            theme::pal().plate,
        )
        .filled(true)
        .rounding(6.0)
        .build();
        dl.add_rect(
            [origin[0] - 2.0, origin[1]],
            [origin[0] + w + 2.0, origin[1] + bar_h],
            theme::pal().gold_dim,
        )
        .rounding(6.0)
        .build();
        // Equalizer bars live between the plate and everything else, so all
        // text and controls render on top of them. One eq_levels() call per
        // frame — the smoothing lives there — shared with the DJ below.
        let levels = player::eq_levels();
        eq_bars(&dl, origin, w, bar_h, &levels);

        // Bass energy (lowest 6 bands) drives the DJ's dancing.
        let bass = (levels[..6].iter().sum::<f32>() / 6.0).clamp(0.0, 1.0);
        // AI quip fetch driver — the call site is the visibility gate: it
        // only runs while the player bar renders (and only while Playing).
        crate::radio::quips::tick(state, bass);

        // DJ choya at the right end of the bar, clipped to it — it can no
        // longer cover the station list's hearts. Text stays clear of the
        // width it reserves.
        let choya_w = art::draw_dj_choya(
            ui,
            &dl,
            state,
            art::DjLook::TAB,
            bass,
            [origin[0] - 2.0, origin[1]],
            [origin[0] + w + 2.0, origin[1] + bar_h],
        );

        let pad = 10.0;
        let x0 = origin[0] + pad;
        let right = origin[0] + w - pad - choya_w;
        let y1 = origin[1] + 6.0;
        let y2 = y1 + line_h + 3.0;
        let status = state.radio.status.clone();
        let name = state.radio.current.as_ref().map(|c| c.name.clone());

        // Line 1: status (or LIVE badge) + station name.
        let mut x = x0;
        if status == RadioStatus::Playing {
            x += live_badge(ui, &dl, [x, y1]);
            x += 8.0;
        } else {
            let (text, color) = status_line(&status);
            let shown = clip_text(ui, &text, right - x);
            dl.add_text([x, y1], crate::ui::color_u32(color), &shown);
            x += ui.calc_text_size(&shown)[0] + 10.0;
        }
        if let Some(name) = name {
            if !matches!(status, RadioStatus::Error(_)) {
                let color = if status == RadioStatus::Playing {
                    theme::pal().gold
                } else {
                    theme::pal().cream
                };
                let shown = clip_text(ui, &name, right - x);
                dl.add_text([x, y1], crate::ui::color_u32(color), &shown);
            }
        }

        // Line 2: big now-playing marquee, or the antivirus hint on error.
        // A station on a port security software famously blocks (Tor/SOCKS/
        // proxy ports) gets the specific story instead of the generic hint.
        if matches!(status, RadioStatus::Error(_)) {
            theme::font_scale(ui, 0.85);
            let text = state
                .radio
                .current
                .as_ref()
                .and_then(|s| av_blocked_port(s.stream_url()))
                .map(|p| tf("radio.error.port_hint", &[("port", &p.to_string())]))
                .unwrap_or_else(|| t("radio.error.av_hint"));
            let hint = clip_text(ui, &text, right - x0);
            dl.add_text([x0, y2], crate::ui::color_u32(theme::pal().muted), &hint);
            theme::font_scale_reset(ui);
        } else if status == RadioStatus::Playing {
            // No ICY title (plenty of stations send none) -> the station
            // name rides the marquee instead of an empty line.
            let title = state
                .radio
                .now_playing
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .or_else(|| state.radio.current.as_ref().map(|c| c.name.clone()));
            if let Some(title) = title {
                // Indented: the marquee zone starts under where the station
                // name roughly ends, not at the bar's true edge.
                let indent = (w * 0.12).clamp(60.0, 240.0);
                now_playing_marquee(
                    ui,
                    &dl,
                    &title,
                    [x0 + indent, y2],
                    (right - x0 - indent).max(0.0),
                    1.0,
                    theme::pal().cream,
                );
            }
        }
    }

    // Controls row: only when the active station's row is not hosting the
    // controls (no live session, or the station is not in the results list).
    // The marquee line above runs at 3x scale, hence the 3.05 line factor.
    if !ctl_in_bar {
        ui.set_cursor_screen_pos([origin[0], origin[1] + bar_h]);
        return;
    }
    let pad = 10.0;
    let y3 = origin[1] + 6.0 + line_h + 3.0 + line_h * 3.05 + 5.0;
    ui.set_cursor_screen_pos([origin[0] + pad, y3]);
    let play_label = t("radio.play");
    let stop_label = t("radio.stop");
    let pause_label = t("radio.pause");
    let btn_w = theme::gold_button_width(ui, &play_label)
        .max(theme::gold_button_width(ui, &stop_label))
        .max(theme::gold_button_width(ui, &pause_label));
    match state.radio.status {
        RadioStatus::Playing => {
            if theme::gold_button_sized(ui, format!("{pause_label}##radio_pause"), [btn_w, 0.0]) {
                // pause() never locks STATE; the status is written
                // optimistically here (the audio thread deliberately goes
                // quiet while paused), same pattern as Stop below.
                player::pause();
                state.radio.status = RadioStatus::Paused;
            }
            ui.same_line_with_spacing(0.0, 8.0);
            stop_button(ui, state, &stop_label, btn_w);
        }
        RadioStatus::Paused => {
            if theme::gold_button_sized(ui, format!("{play_label}##radio_play"), [btn_w, 0.0]) {
                player::resume();
                state.radio.status = RadioStatus::Playing;
            }
            ui.same_line_with_spacing(0.0, 8.0);
            stop_button(ui, state, &stop_label, btn_w);
        }
        RadioStatus::Connecting | RadioStatus::Buffering | RadioStatus::Stalled => {
            stop_button(ui, state, &stop_label, btn_w);
        }
        _ => {
            let resumable = state.radio.current.clone().or_else(|| {
                state
                    .config
                    .radio
                    .last_station
                    .as_ref()
                    .map(player::station_from_saved)
            });
            match resumable {
                Some(station) => {
                    if theme::gold_button_sized(
                        ui,
                        format!("{play_label}##radio_play"),
                        [btn_w, 0.0],
                    ) {
                        start_play(state, station);
                    }
                }
                None => dim_button(ui, &format!("{play_label}##radio_play"), btn_w),
            }
        }
    }

    ui.same_line_with_spacing(0.0, 16.0);
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("radio.volume"));
    ui.same_line_with_spacing(0.0, 8.0);
    // Reserve the combat-duck and AI-quips checkboxes' footprints (drawn at
    // 0.85 scale) before clamping the slider, so neither falls off the bar.
    let duck_label = t("radio.duck_in_combat");
    let duck_w = ui.calc_text_size(&duck_label)[0] * 0.85 + theme::control_height(ui) + 10.0;
    let quips_label = t("radio.ai_quips");
    let quips_w = ui.calc_text_size(&quips_label)[0] * 0.85 + theme::control_height(ui) + 10.0;
    let slider_w = (ui.content_region_avail()[0] - pad - duck_w - quips_w).clamp(90.0, 220.0);
    ui.set_next_item_width(slider_w);
    let mut volume = state.config.radio.volume_percent;
    if Slider::new("##radio_vol", 0u8, 100u8).build(ui, &mut volume) {
        state.config.radio.volume_percent = volume;
        player::set_volume(volume);
    }
    // Persist once on release, not per drag tick (same debounce idea as the
    // window-rect save in ui/mod.rs).
    if ui.is_item_deactivated_after_edit() {
        crate::ui::save_config_detached(state);
    }

    ui.same_line_with_spacing(0.0, 10.0);
    theme::font_scale(ui, 0.85);
    let mut duck = state.config.radio.duck_in_combat;
    if ui.checkbox(format!("{duck_label}##radio_duck"), &mut duck) {
        state.config.radio.duck_in_combat = duck;
        crate::ui::save_config_detached(state);
    }
    ui.same_line_with_spacing(0.0, 10.0);
    let mut quips = state.config.radio.ai_quips;
    if ui.checkbox(format!("{quips_label}##radio_ai_quips"), &mut quips) {
        state.config.radio.ai_quips = quips;
        crate::ui::save_config_detached(state);
    }
    theme::font_scale_reset(ui);
    ui.set_cursor_screen_pos([origin[0], origin[1] + bar_h]);
}

/// The shared Stop button: signal-only stop + optimistic status write. This
/// click handler runs under the frame's STATE guard, where the joining
/// `player::stop()` could never finish its join — hence `request_stop`.
fn stop_button(ui: &Ui, state: &mut AddonState, label: &str, w: f32) {
    if theme::gold_button_sized(ui, format!("{label}##radio_stop"), [w, 0.0]) {
        player::request_stop();
        state.radio.status = RadioStatus::Stopped;
    }
}

/// Real equalizer in the bar's background: 24 low-alpha gold bars driven by
/// the decoded audio (levels computed once per frame by the caller). Skipped
/// entirely while idle, so a silent bar costs nothing and shows nothing.
fn eq_bars(
    dl: &DrawListMut,
    origin: [f32; 2],
    w: f32,
    bar_h: f32,
    levels: &[f32; player::EQ_BANDS],
) {
    if levels.iter().all(|l| *l < 0.004) {
        return;
    }
    let pad = 8.0;
    let gap = 2.0;
    let n = player::EQ_BANDS as f32;
    let bar_w = ((w - pad * 2.0) - gap * (n - 1.0)) / n;
    if bar_w < 1.0 {
        return;
    }
    let base = origin[1] + bar_h - 2.0;
    let max_h = bar_h - 4.0;
    // Theme gold at low alpha — "slight transparency", text stays readable.
    let fill = [
        theme::pal().gold[0],
        theme::pal().gold[1],
        theme::pal().gold[2],
        0.14,
    ];
    for (i, level) in levels.iter().enumerate() {
        let h = max_h * level.clamp(0.0, 1.0);
        if h < 0.5 {
            continue;
        }
        let x = origin[0] + pad + i as f32 * (bar_w + gap);
        dl.add_rect([x, base - h], [x + bar_w, base], fill)
            .filled(true)
            .rounding(1.0)
            .build();
    }
}

/// Gold LIVE pill; returns the width consumed.
fn live_badge(ui: &Ui, dl: &DrawListMut, pos: [f32; 2]) -> f32 {
    let label = t("radio.live");
    let sz = ui.calc_text_size(&label);
    let pad_x = 7.0;
    let h = sz[1] + 3.0;
    let w = sz[0] + pad_x * 2.0;
    dl.add_rect(pos, [pos[0] + w, pos[1] + h], theme::pal().gold_fill)
        .filled(true)
        .rounding(h * 0.45)
        .build();
    dl.add_text(
        [pos[0] + pad_x, pos[1] + 1.0],
        crate::ui::color_u32(theme::pal().gold_button_text),
        &label,
    );
    w
}

/// Inert muted button for "nothing to resume" — same footprint as the gold
/// Play button so the bar never shifts.
fn dim_button(ui: &Ui, label: &str, w: f32) {
    let bg = theme::with_alpha(theme::pal().header_plate, 0.8);
    let _b = ui.push_style_color(StyleColor::Button, bg);
    let _h = ui.push_style_color(StyleColor::ButtonHovered, bg);
    let _a = ui.push_style_color(StyleColor::ButtonActive, bg);
    let _t = ui.push_style_color(StyleColor::Text, theme::pal().muted);
    let _ = ui.button_with_size(label, [w, theme::control_height(ui)]);
}

/// Big now-playing ticker at 1.5x scale: the title repeats endlessly,
/// separated by a small dancing choya with breathing room on both sides,
/// flowing continuously left to right through three opacity zones — glyphs
/// spawn at 0% at the left edge, ride at 70% through the middle, and melt
/// back to 0% before the DJ choya. The zone starts indented, not at the
/// bar's true edge. Per-glyph alpha keeps the equalizer behind the text
/// untouched.
///
/// `scale` 1.0 is the tab's 42 px line; the mini radio passes its row height
/// over 42. Travel is wall-clock (24 px/s at scale 1), not per frame.
pub(crate) fn now_playing_marquee(
    ui: &Ui,
    dl: &DrawListMut,
    text: &str,
    pos: [f32; 2],
    avail: f32,
    scale: f32,
    col: [f32; 4],
) {
    if avail < 60.0 {
        return;
    }
    const MAX_A: f32 = 0.70;
    let icon = 44.0 * scale;
    let sep_gap = 40.0 * scale; // breathing room each side of the icon
    let t = art::tick();
    // Crisp path: the dedicated 42 px ticker face. Fallback (atlas rebuild,
    // no TTF, CJK title): bitmap-scale the current font instead.
    let big = if crate::ui::fonts::ticker_can_render(text) {
        crate::ui::fonts::push_ticker()
    } else {
        None
    };
    if big.is_none() {
        theme::font_scale(ui, 3.0 * scale);
    } else if scale != 1.0 {
        theme::font_scale(ui, scale);
    }
    let full_w = ui.calc_text_size(text)[0];
    let line_h = ui.text_line_height();
    let fade = (avail * 0.20).clamp(24.0, 180.0);
    let ramp = |mid: f32| -> f32 {
        let a_in = ((mid - pos[0]) / fade).clamp(0.0, 1.0);
        let a_out = ((pos[0] + avail - mid) / fade).clamp(0.0, 1.0);
        a_in.min(a_out)
    };
    let span = full_w + sep_gap * 2.0 + icon;
    // 24 px/s of wall-clock time, drifting rightward forever.
    let travel = ((theme::elapsed_ms() as f64 * 0.024 * f64::from(scale)) % f64::from(span)) as f32;
    // Tile copies from left of the zone until past its right edge.
    let mut base = pos[0] + travel;
    while base > pos[0] - span {
        base -= span;
    }
    while base < pos[0] + avail {
        let mut cx = base;
        for ch in text.chars() {
            let mut buf = [0u8; 4];
            let glyph: &str = ch.encode_utf8(&mut buf);
            let w = ui.calc_text_size(glyph)[0];
            if cx + w >= pos[0] && cx <= pos[0] + avail {
                let a = ramp(cx + w * 0.5) * MAX_A;
                if a > 0.02 {
                    dl.add_text(
                        [cx, pos[1]],
                        crate::ui::color_u32([col[0], col[1], col[2], col[3] * a]),
                        glyph,
                    );
                }
            }
            cx += w;
        }
        // The little dancer between repeats.
        let icon_mid = base + full_w + sep_gap + icon * 0.5;
        if icon_mid + icon * 0.5 >= pos[0] && icon_mid - icon * 0.5 <= pos[0] + avail {
            let a = ramp(icon_mid) * 0.85;
            if a > 0.02 {
                art::marquee_choya(dl, [icon_mid, pos[1] + line_h * 0.5], icon, t, a);
            }
        }
        base += span;
    }
    if big.is_none() || scale != 1.0 {
        theme::font_scale_reset(ui);
    }
}

/// The stream URL's explicit port, when it is one that security software
/// (antivirus web protection, firewalls) commonly blocks outright: SOCKS,
/// Tor, I2P and Privoxy ports. Shoutcast servers on these ports fail at
/// connect time on protected machines — seen in the wild on :9050 twice.
fn av_blocked_port(url: &str) -> Option<u16> {
    let port = reqwest::Url::parse(url).ok()?.port()?;
    matches!(port, 1080 | 4444 | 4445 | 8118 | 9050 | 9051 | 9150).then_some(port)
}

pub(crate) fn status_line(status: &RadioStatus) -> (String, [f32; 4]) {
    match status {
        RadioStatus::Idle => (t("radio.status.idle"), theme::pal().muted),
        RadioStatus::Connecting => (t("radio.status.connecting"), theme::pal().gold),
        RadioStatus::Buffering => (t("radio.status.buffering"), theme::pal().gold),
        RadioStatus::Playing => (t("radio.live"), theme::pal().gold),
        RadioStatus::Paused => (t("radio.status.paused"), theme::pal().muted),
        RadioStatus::Stalled => (t("radio.status.stalled"), theme::WARN),
        RadioStatus::Stopped => (t("radio.status.stopped"), theme::pal().muted),
        RadioStatus::DeviceLost => (t("radio.status.device_lost"), theme::ERR),
        // ASCII '-': the game font renders an em dash as '?'.
        RadioStatus::Error(msg) => (format!("{} - {}", t("radio.status.error"), msg), theme::ERR),
    }
}

// Actions

/// Tune in and reflect it immediately; the playback thread confirms (or
/// corrects) the status a moment later through `with_state`.
pub(crate) fn start_play(state: &mut AddonState, station: RbStation) {
    player::play(&station);
    state.radio.status = RadioStatus::Connecting;
    state.radio.current = Some(station);
}

fn toggle_favorite(state: &mut AddonState, station: &RbStation, now_frames: u32) {
    let key = fav_key(&station.stationuuid, station.stream_url());
    let favorites = &mut state.config.radio.favorites;
    if let Some(i) = favorites
        .iter()
        .position(|f| fav_key(&f.stationuuid, &f.url) == key)
    {
        favorites.remove(i);
    } else {
        favorites.push(player::saved_from_station(station));
        art::flash_love(now_frames);
    }
    crate::ui::save_config_detached(state);
}

fn is_favorite(state: &AddonState, station: &RbStation) -> bool {
    let key = fav_key(&station.stationuuid, station.stream_url());
    state
        .config
        .radio
        .favorites
        .iter()
        .any(|f| fav_key(&f.stationuuid, &f.url) == key)
}

pub(crate) fn current_station_key(state: &AddonState) -> Option<String> {
    state
        .radio
        .current
        .as_ref()
        .map(|c| fav_key(&c.stationuuid, c.stream_url()))
}

/// Favorite identity: stationuuid, falling back to the trimmed stream URL for
/// hand-entered stations without one (matches the `RadioPreferences` doc).
pub(crate) fn fav_key(uuid: &str, url: &str) -> String {
    if uuid.is_empty() {
        url.trim().to_string()
    } else {
        uuid.to_string()
    }
}

/// country • codec • bitrate, skipping whatever a sparse record lacks.
fn meta_line(country: &str, codec: &str, bitrate: u32) -> String {
    let bits = if bitrate > 0 {
        tf(
            "fmt.radio_bitrate",
            &[("kbps", bitrate.to_string().as_str())],
        )
    } else {
        String::new()
    };
    join_meta([country, codec, &bits])
}

fn join_meta(parts: [&str; 3]) -> String {
    parts
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim())
        .collect::<Vec<_>>()
        .join(" - ")
}

/// Width-aware end truncation with an ellipsis — UTF-8 safe (chars, never
/// byte slices), mirroring the private `theme::clip_to_width`.
pub(crate) fn clip_text(ui: &Ui, text: &str, max_w: f32) -> String {
    if max_w <= 4.0 {
        return String::new();
    }
    if ui.calc_text_size(text)[0] <= max_w {
        return text.to_string();
    }
    // Accumulate per-char widths instead of re-measuring the growing prefix:
    // O(n) instead of O(n²) — this runs per row, per frame, in a game overlay.
    // Summed advances ignore kerning, which the bundled fonts do not use.
    let ell_w = ui.calc_text_size("…")[0];
    let mut s = String::new();
    let mut w = 0.0;
    let mut buf = [0u8; 4];
    for ch in text.chars() {
        let cw = ui.calc_text_size(ch.encode_utf8(&mut buf))[0];
        if w + cw + ell_w > max_w {
            break;
        }
        w += cw;
        s.push(ch);
    }
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genre_chips_cover_the_translated_set() {
        assert_eq!(GENRES.len(), 16);
        // "Top stations" is the tagless vote-order search.
        assert_eq!(GENRES[0], ("top", ""));
        // Multi-word radio-browser tags stay exactly as the directory spells them.
        assert!(GENRES.contains(&("hiphop", "hip hop")));
        assert!(GENRES.contains(&("world", "world music")));
        assert!(GENRES.contains(&("chill", "chillout")));
        assert!(GENRES.contains(&("eighties", "80s")));
    }

    #[test]
    fn fav_key_prefers_uuid_and_falls_back_to_trimmed_url() {
        assert_eq!(fav_key("abc", "http://x/"), "abc");
        assert_eq!(fav_key("", "  http://x/  "), "http://x/");
    }

    #[test]
    fn join_meta_skips_empty_parts() {
        assert_eq!(join_meta(["DE", "MP3", "128 kbps"]), "DE - MP3 - 128 kbps");
        assert_eq!(join_meta(["", "MP3", ""]), "MP3");
        assert_eq!(join_meta(["", "", ""]), "");
    }
}
