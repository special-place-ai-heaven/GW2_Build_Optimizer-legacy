//! About > Generations: every New Build, Improve and Choya run, one
//! hand-painted row each. Filters sit on top, the pager is pinned under the
//! table, and the rows per page follow the window height. Clicking a row's
//! build card reopens that build on its character, in the scenario it was
//! made for, through the measuring path the Saves tab uses.
//!
//! The `render_*` and `draw_*` functions run on the render thread, which
//! holds STATE for the whole frame: they never call `with_state` (the
//! mutex is not re-entrant). Only the workers below do.

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Local, Utc};
use nexus::imgui::{ChildWindow, ComboBox, Selectable, StyleVar, Ui};

use gw2_core::config::CostCurrency;
use gw2_core::generations::{
    GenerationKind, GenerationLog, GenerationRecord, GenerationStatus, RunFeed, RunPhase, StepState,
};
use gw2_core::i18n::{t, tf};
use gw2_core::types::GameMode;
use gw2_optimizer::scenario::RoleObjective;
use gw2_optimizer::scoring::OptimizationWeights;

use crate::state::{with_state, AddonState, MainTab};
use crate::ui::comparison::BuildSuggestion;
use crate::ui::main_view::generation::{from_variant_id, record_scenario_line};
use crate::ui::main_view::tabs::saveload::{
    clip_label, paint_row_plate, suggestion_from_saved_build,
};
use crate::ui::{icons, run_feed, theme};

/// Space between rows, and under the header.
const GAP: f32 = 6.0;

/// Where the history read stands.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum LoadState {
    #[default]
    Idle,
    Loading,
    Ready,
    Error(String),
}

/// The date-range filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum When {
    #[default]
    All,
    Today,
    Week,
    Month,
}

impl When {
    const ALL: [When; 4] = [When::All, When::Today, When::Week, When::Month];

    fn label(self) -> String {
        t(match self {
            When::All => "gen.when.all",
            When::Today => "gen.when.today",
            When::Week => "gen.when.week",
            When::Month => "gen.when.month",
        })
    }

    /// Whether a run started at `at` falls in the range, seen from `now`.
    fn keeps(self, at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        match self {
            When::All => true,
            When::Today => {
                at.with_timezone(&Local).date_naive() == now.with_timezone(&Local).date_naive()
            }
            When::Week => now - at <= chrono::Duration::days(7),
            When::Month => now - at <= chrono::Duration::days(30),
        }
    }
}

/// What the table is ordered by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Date,
    Kind,
    Character,
    Duration,
    Tokens,
    Cost,
    Dps,
}

impl SortKey {
    /// A new key starts A to Z for names, largest first for numbers.
    fn default_asc(self) -> bool {
        matches!(self, SortKey::Kind | SortKey::Character)
    }
}

/// The filter row. `Default` keeps everything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filters {
    pub kind: Option<GenerationKind>,
    pub character: Option<String>,
    pub mode: Option<GameMode>,
    /// An [`llm_key`]; `Some("")` keeps the runs that made no LLM request.
    pub llm: Option<String>,
    pub when: When,
    /// Case-insensitive, over title, character, spec and model.
    pub text: String,
}

/// A record being reopened, with its own steps so the pane shows what it
/// is doing while the build is measured.
pub struct Opening {
    pub id: String,
    pub started: Instant,
    pub feed: RunFeed,
    measure_step: Option<u32>,
}

impl Opening {
    fn ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn note(&mut self, label: String, state: StepState) -> u32 {
        let at = self.ms();
        self.feed.push(at, RunPhase::Run, label, None, state)
    }
}

/// The Generations tab's state, on `MainState`.
#[derive(Default)]
pub struct GenerationsTab {
    pub load: LoadState,
    /// A run appended a record; the next frame the tab is shown re-reads.
    pub stale: bool,
    /// Bumped per read, so an older read cannot land over a newer one.
    load_seq: u64,
    /// Newest first. Replace through [`GenerationsTab::set_records`], which
    /// drops the caches built from them.
    pub records: Vec<Arc<GenerationRecord>>,
    /// 0-based; clamped every frame, since a resize changes the page count.
    pub page: usize,
    pub filters: Filters,
    pub sort: SortKey,
    /// `false` (the default) is newest first.
    pub sort_asc: bool,
    /// Id of the record whose run log is shown under the pager.
    pub expanded: Option<String>,
    pub opening: Option<Opening>,
    /// The filter row's character and LLM choices, built once per read.
    options: Option<Arc<FilterOptions>>,
    /// The visible rows, rebuilt only when the key changes.
    rows: Option<(RowsKey, Arc<Vec<usize>>)>,
}

/// What the visible rows depend on besides the records. The minute makes
/// "Today" and the other date presets roll over without a new read.
type RowsKey = (Filters, SortKey, bool, i64);

/// Sorted, distinct values of the records, for the filter combos.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct FilterOptions {
    names: Vec<String>,
    /// [`llm_key`]s; `""` is the runs that made no LLM request.
    llms: Vec<String>,
}

impl FilterOptions {
    fn of(records: &[Arc<GenerationRecord>]) -> Self {
        let mut names: Vec<String> = records
            .iter()
            .map(|r| r.character_name.clone())
            .filter(|c| !c.is_empty())
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        let mut llms: Vec<String> = records.iter().map(|r| llm_key(r)).collect();
        llms.sort();
        llms.dedup();
        Self { names, llms }
    }
}

impl GenerationsTab {
    pub fn set_records(&mut self, records: Vec<Arc<GenerationRecord>>) {
        self.records = records;
        self.options = None;
        self.rows = None;
    }

    fn options(&mut self) -> Arc<FilterOptions> {
        let records = &self.records;
        Arc::clone(
            self.options
                .get_or_insert_with(|| Arc::new(FilterOptions::of(records))),
        )
    }

    /// [`visible_rows`], cached on (filters, sort, minute) until the records change.
    fn rows(&mut self, now: DateTime<Utc>) -> Arc<Vec<usize>> {
        let key = (
            self.filters.clone(),
            self.sort,
            self.sort_asc,
            now.timestamp() / 60,
        );
        match &self.rows {
            Some((k, rows)) if *k == key => Arc::clone(rows),
            _ => {
                let rows = Arc::new(visible_rows(
                    &self.records,
                    &self.filters,
                    self.sort,
                    self.sort_asc,
                    now,
                ));
                self.rows = Some((key, Arc::clone(&rows)));
                rows
            }
        }
    }
}

// Pure helpers (tested below)

/// Rows that fit: each takes `row_h + gap`; the header and the pager are
/// reserved first. Never fewer than three: a pane too short for them
/// scrolls rather than showing a table of one.
pub(crate) fn rows_per_page(
    avail_h: f32,
    header_h: f32,
    pager_h: f32,
    row_h: f32,
    gap: f32,
) -> usize {
    let room = avail_h - header_h - pager_h;
    (room / (row_h + gap)).floor().max(3.0) as usize
}

pub(crate) fn page_count(rows: usize, per_page: usize) -> usize {
    rows.div_ceil(per_page.max(1)).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerItem {
    Page(usize),
    Gap,
}

/// Page buttons: every page up to seven; past that the first, the last and
/// the current page with its neighbours, or the first / last five near an
/// end, so the row keeps its width as the page moves.
pub(crate) fn pager_items(page: usize, pages: usize) -> Vec<PagerItem> {
    if pages <= 7 {
        return (0..pages).map(PagerItem::Page).collect();
    }
    let last = pages - 1;
    let page = page.min(last);
    let (lo, hi) = if page <= 3 {
        (0, 4)
    } else if page >= last - 3 {
        (last - 4, last)
    } else {
        (page - 1, page + 1)
    };
    let mut shown = vec![0];
    shown.extend(lo..=hi);
    shown.push(last);
    shown.dedup();
    let mut out = Vec::new();
    let mut prev: Option<usize> = None;
    for p in shown {
        if prev.is_some_and(|q| p > q + 1) {
            out.push(PagerItem::Gap);
        }
        out.push(PagerItem::Page(p));
        prev = Some(p);
    }
    out
}

/// "Gemini · gemini-2.5-flash"; empty for a run that made no LLM request.
fn llm_key(r: &GenerationRecord) -> String {
    r.llm
        .as_ref()
        .map(|l| format!("{} \u{00b7} {}", l.provider.short_label(), l.model))
        .unwrap_or_default()
}

/// Tokens are only known when an LLM answered with usage.
fn known_tokens(r: &GenerationRecord) -> Option<u64> {
    r.llm.as_ref().and(Some(r.tokens.total)).filter(|n| *n > 0)
}

fn dps(r: &GenerationRecord) -> Option<i32> {
    r.card.as_ref().and_then(|c| c.simulated_dps)
}

fn title(r: &GenerationRecord) -> &str {
    r.card
        .as_ref()
        .map(|c| c.title.as_str())
        .or(r.build.as_ref().map(|b| b.label.as_str()))
        .unwrap_or("")
}

fn keeps(r: &GenerationRecord, f: &Filters, now: DateTime<Utc>) -> bool {
    if f.kind.is_some_and(|k| k != r.kind)
        || f.character.as_ref().is_some_and(|c| *c != r.character_name)
        || f.mode.as_ref().is_some_and(|m| *m != r.mode)
        || f.llm.as_ref().is_some_and(|l| *l != llm_key(r))
        || !f.when.keeps(r.started_at, now)
    {
        return false;
    }
    let needle = f.text.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    [
        title(r),
        &r.character_name,
        r.elite_spec.as_deref().unwrap_or(""),
        &r.profession,
        &llm_key(r),
    ]
    .iter()
    .any(|hay| hay.to_lowercase().contains(&needle))
}

/// Unknown values last, whichever way the column is sorted.
fn known_last<T: PartialOrd>(a: Option<T>, b: Option<T>, asc: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => {
            let o = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
            if asc {
                o
            } else {
                o.reverse()
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn kind_rank(k: GenerationKind) -> u8 {
    match k {
        GenerationKind::NewBuild => 0,
        GenerationKind::Improve => 1,
        GenerationKind::Choya => 2,
    }
}

fn compare(a: &GenerationRecord, b: &GenerationRecord, key: SortKey, asc: bool) -> Ordering {
    let dir = |o: Ordering| if asc { o } else { o.reverse() };
    match key {
        SortKey::Date => dir(a.started_at.cmp(&b.started_at)),
        SortKey::Kind => dir(kind_rank(a.kind).cmp(&kind_rank(b.kind))),
        SortKey::Character => dir(a
            .character_name
            .to_lowercase()
            .cmp(&b.character_name.to_lowercase())),
        SortKey::Duration => dir(a.duration_ms.cmp(&b.duration_ms)),
        SortKey::Tokens => known_last(known_tokens(a), known_tokens(b), asc),
        SortKey::Cost => known_last(a.cost_estimate_usd, b.cost_estimate_usd, asc),
        SortKey::Dps => known_last(dps(a), dps(b), asc),
    }
}

/// Indices into `records` that pass the filters, in table order. The sort is
/// stable: equal keys keep the records' own (newest-first) order.
pub(crate) fn visible_rows(
    records: &[Arc<GenerationRecord>],
    f: &Filters,
    key: SortKey,
    asc: bool,
    now: DateTime<Utc>,
) -> Vec<usize> {
    let mut rows: Vec<usize> = (0..records.len())
        .filter(|&i| keeps(&records[i], f, now))
        .collect();
    rows.sort_by(|&a, &b| compare(&records[a], &records[b], key, asc));
    rows
}

fn kind_label(k: GenerationKind) -> String {
    t(match k {
        GenerationKind::NewBuild => "gen.kind_new",
        GenerationKind::Improve => "gen.kind_improve",
        GenerationKind::Choya => "gen.kind_choya",
    })
}

fn kind_colour(k: GenerationKind) -> [f32; 4] {
    match k {
        GenerationKind::NewBuild => theme::OPTIMIZED,
        GenerationKind::Improve => theme::CURRENT,
        GenerationKind::Choya => theme::pal().gold,
    }
}

fn local_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

// Loading

fn ensure_loaded(state: &mut AddonState) {
    let tab = &state.main.generations;
    let due = match tab.load {
        LoadState::Idle => true,
        LoadState::Loading => false,
        LoadState::Ready | LoadState::Error(_) => tab.stale,
    };
    if due {
        start_load(state);
    }
}

/// Read the whole log on a worker. The token is the one Stop does not
/// cancel (a history read is not the run in flight); unload still does.
fn start_load(state: &mut AddonState) {
    let tab = &mut state.main.generations;
    tab.load_seq += 1;
    let seq = tab.load_seq;
    tab.load = LoadState::Loading;
    tab.stale = false;
    let log = GenerationLog::new(&state.addon_dir);
    let spawned = state.spawn_worker_on(
        "gen-history",
        state.pick_cancel_token.clone(),
        move |token| {
            let mut records = log.load_all();
            records.reverse();
            let records: Vec<Arc<GenerationRecord>> = records.into_iter().map(Arc::new).collect();
            with_state(|s| {
                let tab = &mut s.main.generations;
                if tab.load_seq != seq {
                    return;
                }
                if token.is_cancelled() {
                    // Read again next time the tab is shown.
                    tab.load = LoadState::Idle;
                    return;
                }
                tab.set_records(records);
                tab.load = LoadState::Ready;
            });
        },
    );
    if !spawned {
        state.main.generations.load = LoadState::Error(t("gen.thread_failed"));
    }
}

// Opening a record in the main view

/// What the worker needs to measure a record's build, captured on the
/// render thread after the character and scenario are restored.
pub(in crate::ui::main_view) struct OpenJob {
    record: Arc<GenerationRecord>,
    saved: gw2_core::types::SavedBuild,
    db: Option<Arc<gw2_optimizer::gamedb::GameDb>>,
    mode: GameMode,
    weights: OptimizationWeights,
    scenario: gw2_optimizer::scenario::ScenarioSpec,
    /// The record's character, when it is on this account.
    character: Option<String>,
    /// The record's character when it is not on this account.
    missing_character: Option<String>,
}

impl OpenJob {
    /// The Saves path: validate, re-plate and measure the stored build.
    pub(in crate::ui::main_view) fn measure(&self) -> BuildSuggestion {
        suggestion_from_saved_build(
            &self.saved,
            self.db.as_deref(),
            &self.mode,
            &self.weights,
            &self.scenario,
        )
    }
}

/// Restore the record's scenario and character, and start its step feed.
/// `None` when the run served no build.
pub(in crate::ui::main_view) fn prepare_open(
    state: &mut AddonState,
    record: Arc<GenerationRecord>,
) -> Option<OpenJob> {
    let saved = record.build.clone()?;
    let mut opening = Opening {
        id: record.id.clone(),
        started: Instant::now(),
        feed: RunFeed::default(),
        measure_step: None,
    };

    // The scenario first, so the character switch below resolves the
    // current build in it, and the plate is scored as it was made.
    let mode = record.mode.clone();
    let tier = record
        .scale_id
        .as_deref()
        .and_then(from_variant_id)
        .unwrap_or(state.main.combat_tier);
    let role: Option<RoleObjective> = record.role_id.as_deref().and_then(from_variant_id);
    let weights = serde_json::from_value::<OptimizationWeights>(record.weights.clone())
        .unwrap_or_else(|_| match role {
            Some(r) => r.to_weights_for(&mode, tier),
            None => OptimizationWeights::default_for_mode(mode.label()),
        });
    let mode_changed =
        crate::ui::main_view::restore_scenario(state, mode.clone(), tier, role, weights.clone());
    opening.note(
        tf(
            "gen.open.scenario",
            &[("scenario", &record_scenario_line(&record))],
        ),
        StepState::Done { took_ms: 0 },
    );

    let name = record.character_name.clone();
    let mut character = None;
    let mut missing_character = None;
    match state.main.characters.iter().position(|c| *c == name) {
        Some(i) => {
            if state.main.selected_character != Some(i) {
                crate::ui::main_view::select_character(state, i, name.clone());
                opening.note(
                    tf("gen.open.character", &[("name", &name)]),
                    StepState::Done { took_ms: 0 },
                );
            } else if mode_changed {
                crate::ui::main_view::resolution::resolve_selected_build(state);
            }
            character = Some(name);
        }
        None => {
            if !name.is_empty() {
                let id = opening.note(
                    tf("gen.open.character", &[("name", &name)]),
                    StepState::Running,
                );
                opening
                    .feed
                    .fail(id, tf("gen.char_missing", &[("name", &name)]));
                missing_character = Some(name);
            }
            if mode_changed {
                crate::ui::main_view::resolution::resolve_selected_build(state);
            }
        }
    }

    let scenario = crate::ui::main_view::optimize_flow::scenario_for_run(
        &gw2_optimizer::balance::BalanceContext::new(mode.clone()),
        crate::ui::main_view::optimize_flow::combat_tier_for(&mode, tier),
        role,
        &weights,
    );
    opening.measure_step = Some(opening.note(t("gen.open.measure"), StepState::Running));
    state.main.generations.opening = Some(opening);
    Some(OpenJob {
        record,
        saved,
        db: state.main.game_db.clone(),
        mode,
        weights,
        scenario,
        character,
        missing_character,
    })
}

/// Plate the measured build with the original run's record attached, and
/// land on the tab the run was made from.
pub(in crate::ui::main_view) fn finish_open(
    state: &mut AddonState,
    job: OpenJob,
    mut suggestion: BuildSuggestion,
) {
    let main = &mut state.main;
    // A newer click took over.
    if main.generations.opening.as_ref().map(|o| o.id.as_str()) != Some(job.record.id.as_str()) {
        return;
    }
    main.generations.opening = None;
    // The player switched character while it measured.
    let selected = main
        .selected_character
        .and_then(|i| main.characters.get(i))
        .cloned();
    if job.character.is_some() && selected != job.character {
        return;
    }
    suggestion.generation = Some(Arc::clone(&job.record));
    if let Some(name) = &job.missing_character {
        suggestion
            .quality_reasons
            .push(tf("gen.char_missing", &[("name", name)]));
    }
    let tab = match job.record.kind {
        GenerationKind::NewBuild => MainTab::NewBuild,
        GenerationKind::Improve if job.missing_character.is_none() => MainTab::Improve,
        GenerationKind::Improve => MainTab::NewBuild,
        GenerationKind::Choya => {
            crate::ui::main_view::optimization::result_alert_tab(main.current_build.is_some())
        }
    };
    // Beside the tabs already there, as Saves does (specs/006 FR-004).
    main.comparison.push_or_replace(suggestion);
    main.comparison.error = None;
    let improve = tab == MainTab::Improve;
    main.active_tab = tab;
    main.tab_alert = None;
    if improve {
        if let Some(build) = main.current_build.clone() {
            crate::ui::main_view::resolution::auto_populate_locks(&build, &mut main.build_locks);
        }
    }
}

/// Ends the open card when its worker leaves without plating: a cancel
/// return or a panic in `measure()` fails the measuring step, so the card
/// stops spinning and Dismiss shows. After `finish_open` it finds nothing
/// open under its id and does nothing. Worker-thread only.
pub(in crate::ui::main_view) struct OpenGuard(pub(in crate::ui::main_view) String);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        let why = if std::thread::panicking() {
            t("run.failed")
        } else {
            t("run.cancelled")
        };
        with_state(|s| {
            let Some(o) = s.main.generations.opening.as_mut() else {
                return;
            };
            if o.id != self.0 {
                return;
            }
            if let Some(id) = o.measure_step {
                if o.feed.running().is_some_and(|r| r.id == id) {
                    o.feed.fail(id, why);
                }
            }
        });
    }
}

fn open_generation(state: &mut AddonState, record: Arc<GenerationRecord>) {
    let Some(job) = prepare_open(state, record) else {
        return;
    };
    let spawned =
        state.spawn_worker_on("gen-open", state.pick_cancel_token.clone(), move |token| {
            let _guard = OpenGuard(job.record.id.clone());
            if token.is_cancelled() {
                return;
            }
            let suggestion = job.measure();
            if token.is_cancelled() {
                return;
            }
            with_state(|s| finish_open(s, job, suggestion));
        });
    if !spawned {
        // The OS refused the thread; the CPU work never runs inline.
        if let Some(o) = state.main.generations.opening.as_mut() {
            if let Some(id) = o.measure_step {
                o.feed.fail(id, t("gen.thread_failed"));
            }
        }
    }
}

// Drawing

fn put(ui: &Ui, pos: [f32; 2], colour: [f32; 4], text: &str) {
    ui.get_window_draw_list()
        .add_text(pos, crate::ui::color_u32(colour), text);
}

/// A small filled triangle: up, down or right.
fn arrow(ui: &Ui, centre: [f32; 2], size: f32, dir: char, colour: [f32; 4]) {
    let [x, y] = centre;
    let s = size;
    let (a, b, c) = match dir {
        'u' => ([x, y - s], [x - s, y + s * 0.7], [x + s, y + s * 0.7]),
        'd' => ([x - s, y - s * 0.7], [x + s, y - s * 0.7], [x, y + s]),
        _ => ([x - s * 0.7, y - s], [x - s * 0.7, y + s], [x + s, y]),
    };
    ui.get_window_draw_list()
        .add_triangle(a, b, c, crate::ui::color_u32(colour))
        .filled(true)
        .build();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Expand,
    Date,
    Kind,
    Character,
    Scenario,
    Llm,
    Duration,
    Tokens,
    Cost,
    Build,
}

impl Col {
    fn header_key(self) -> Option<&'static str> {
        Some(match self {
            Col::Expand => return None,
            Col::Date => "gen.col.date",
            Col::Kind => "gen.col.type",
            Col::Character => "gen.col.character",
            Col::Scenario => "gen.col.scenario",
            Col::Llm => "gen.col.llm",
            Col::Duration => "gen.col.duration",
            Col::Tokens => "gen.col.tokens",
            Col::Cost => "gen.col.cost",
            Col::Build => "gen.col.build",
        })
    }

    fn sort_key(self) -> Option<SortKey> {
        Some(match self {
            Col::Date => SortKey::Date,
            Col::Kind => SortKey::Kind,
            Col::Character => SortKey::Character,
            Col::Duration => SortKey::Duration,
            Col::Tokens => SortKey::Tokens,
            Col::Cost => SortKey::Cost,
            Col::Build => SortKey::Dps,
            Col::Expand | Col::Scenario | Col::Llm => return None,
        })
    }
}

/// Column x offsets and widths, shared by the header and every row.
struct Columns(Vec<(Col, f32, f32)>);

impl Columns {
    fn new(ui: &Ui, width: f32) -> Self {
        const PAD: f32 = 12.0;
        const CARD_MIN: f32 = 200.0;
        let fit = |s: &str| ui.calc_text_size(s)[0];
        // Room for the header and its sort arrow.
        let head = |col: Col| col.header_key().map_or(0.0, |k| fit(&t(k)) + 16.0);
        let kinds = [
            GenerationKind::NewBuild,
            GenerationKind::Improve,
            GenerationKind::Choya,
        ]
        .map(|k| fit(&kind_label(k)) + 16.0)
        .into_iter()
        .fold(0.0_f32, f32::max);
        let mut cols = vec![
            (Col::Expand, ui.text_line_height() + 8.0),
            (
                Col::Date,
                fit("0000-00-00 00:00").max(head(Col::Date)) + PAD,
            ),
            (Col::Kind, kinds.max(head(Col::Kind)) + PAD),
            (Col::Character, (width * 0.14).clamp(120.0, 200.0)),
            (Col::Scenario, (width * 0.12).clamp(110.0, 180.0)),
            (Col::Llm, (width * 0.12).clamp(110.0, 180.0)),
            (
                Col::Duration,
                fit("00 min 00 s").max(head(Col::Duration)) + PAD,
            ),
            (Col::Tokens, fit("000.0k").max(head(Col::Tokens)) + PAD),
            (
                Col::Cost,
                fit("\u{2248} $00.00")
                    .max(fit("\u{2248} \u{20ac}00.00"))
                    .max(head(Col::Cost))
                    + PAD,
            ),
        ];
        // A narrow pane gives up the scenario, then the LLM column; the
        // card's tooltip still names both.
        for drop in [Col::Scenario, Col::Llm] {
            let used: f32 = cols.iter().map(|c| c.1).sum::<f32>() + 16.0;
            if width - used < CARD_MIN {
                cols.retain(|c| c.0 != drop);
            }
        }
        let used: f32 = cols.iter().map(|c| c.1).sum::<f32>() + 16.0;
        cols.push((Col::Build, (width - used).max(CARD_MIN)));
        let mut x = 6.0;
        Self(
            cols.into_iter()
                .map(|(c, w)| {
                    let at = (c, x, w);
                    x += w;
                    at
                })
                .collect(),
        )
    }
}

/// What the rows clicked, acted on after the draw.
#[derive(Default)]
struct Clicks {
    sort: Option<SortKey>,
    page: Option<usize>,
    expand: Option<String>,
    open: Option<Arc<GenerationRecord>>,
}

/// Read-only inputs every row needs.
struct RowCtx {
    db: Option<Arc<gw2_optimizer::gamedb::GameDb>>,
    optimizing: bool,
    /// The record whose build is being measured right now.
    opening: Option<String>,
    expanded: Option<String>,
    currency: CostCurrency,
}

fn draw_header(ui: &Ui, cols: &Columns, h: f32, sort: SortKey, asc: bool, clicks: &mut Clicks) {
    paint_row_plate(ui, h, true);
    let o = ui.cursor_screen_pos();
    let p = theme::pal();
    let lh = ui.text_line_height();
    let ty = o[1] + ((h - lh) * 0.5).round();
    for &(col, x, w) in &cols.0 {
        let Some(key) = col.header_key() else {
            continue;
        };
        let label = clip_label(ui, &t(key), (w - 20.0).max(8.0));
        let tx = o[0] + x;
        let mut colour = p.gold;
        if let Some(sk) = col.sort_key() {
            ui.set_cursor_screen_pos([tx - 4.0, o[1]]);
            if ui.invisible_button(format!("##gen_sort_{key}"), [(w - 4.0).max(8.0), h]) {
                clicks.sort = Some(sk);
            }
            if ui.is_item_hovered() {
                colour = p.cream;
            }
        }
        put(ui, [tx, ty], colour, &label);
        if col.sort_key() == Some(sort) {
            let tw = ui.calc_text_size(&label)[0];
            let dir = if asc { 'u' } else { 'd' };
            arrow(ui, [tx + tw + 9.0, ty + lh * 0.5], lh * 0.22, dir, p.gold);
        }
    }
    ui.set_cursor_screen_pos([o[0], o[1] + h + GAP]);
}

fn draw_row(
    ui: &Ui,
    cols: &Columns,
    rec: &Arc<GenerationRecord>,
    row_h: f32,
    ctx: &RowCtx,
    clicks: &mut Clicks,
) {
    paint_row_plate(ui, row_h, false);
    let o = ui.cursor_screen_pos();
    let p = theme::pal();
    let lh = ui.text_line_height();
    let y1 = o[1] + ((row_h - lh * 2.0 - 3.0) * 0.5).round();
    let y2 = y1 + lh + 3.0;
    let ymid = o[1] + ((row_h - lh) * 0.5).round();
    let mut stats_span = [f32::MAX, 0.0_f32];
    for &(col, x, w) in &cols.0 {
        let x0 = o[0] + x;
        let inner = (w - 10.0).max(8.0);
        match col {
            Col::Expand => {
                ui.set_cursor_screen_pos([x0, o[1]]);
                if ui.invisible_button(format!("##gen_exp_{}", rec.id), [w - 2.0, row_h]) {
                    clicks.expand = Some(rec.id.clone());
                }
                let hovered = ui.is_item_hovered();
                if hovered {
                    ui.tooltip_text(t("gen.expand"));
                }
                let open = ctx.expanded.as_deref() == Some(rec.id.as_str());
                let colour = if hovered || open { p.gold } else { p.gold_dim };
                let dir = if open { 'd' } else { 'r' };
                arrow(
                    ui,
                    [x0 + w * 0.5 - 2.0, o[1] + row_h * 0.5],
                    lh * 0.26,
                    dir,
                    colour,
                );
            }
            Col::Date => {
                put(ui, [x0, y1], p.cream, &local_time(rec.started_at));
                let (text, colour) = match &rec.status {
                    GenerationStatus::Ok => (
                        rec.tier.map(run_feed::tier_label).unwrap_or_default(),
                        p.muted,
                    ),
                    GenerationStatus::Cancelled => (t("run.cancelled"), theme::WARN),
                    GenerationStatus::Failed { .. } => (t("run.failed"), theme::ERR),
                };
                put(ui, [x0, y2], colour, &clip_label(ui, &text, inner));
            }
            Col::Kind => {
                let label = kind_label(rec.kind);
                let colour = kind_colour(rec.kind);
                let pw = ui.calc_text_size(&label)[0] + 16.0;
                let ph = lh + 4.0;
                let min = [x0, ymid - 2.0];
                let max = [x0 + pw, ymid - 2.0 + ph];
                {
                    let dl = ui.get_window_draw_list();
                    dl.add_rect(min, max, theme::with_alpha(colour, 0.20))
                        .filled(true)
                        .rounding(ph * 0.45)
                        .build();
                    dl.add_rect(min, max, theme::with_alpha(colour, 0.85))
                        .rounding(ph * 0.45)
                        .build();
                }
                put(ui, [x0 + 8.0, ymid], colour, &label);
            }
            Col::Character => {
                let size = (lh * 1.6).min(row_h - 10.0);
                let url = ctx.db.as_deref().and_then(|db| {
                    rec.elite_spec
                        .as_deref()
                        .and_then(|s| icons::spec_url_by_name(db, s))
                        .or_else(|| icons::profession_icon_url(db, &rec.profession))
                });
                let letter = rec
                    .profession
                    .chars()
                    .chain(rec.character_name.chars())
                    .next()
                    .unwrap_or('?');
                icons::paint_avatar(ui, url, [x0, o[1] + (row_h - size) * 0.5], size, letter);
                let tx = x0 + size + 6.0;
                let tw = (inner - size - 6.0).max(8.0);
                let name = if rec.character_name.is_empty() {
                    t("gen.no_character")
                } else {
                    rec.character_name.clone()
                };
                put(ui, [tx, y1], p.cream, &clip_label(ui, &name, tw));
                let spec = rec.elite_spec.as_deref().unwrap_or(&rec.profession);
                put(ui, [tx, y2], p.muted, &clip_label(ui, spec, tw));
            }
            Col::Scenario => {
                let line = record_scenario_line(rec);
                let (mode, rest) = line.split_once(" \u{00b7} ").unwrap_or((&line, ""));
                put(ui, [x0, y1], p.cream, mode);
                put(ui, [x0, y2], p.muted, &clip_label(ui, rest, inner));
            }
            Col::Llm => {
                let (provider, model, full) = match &rec.llm {
                    Some(l) => (
                        l.provider.short_label().to_string(),
                        run_feed::model_label(l),
                        llm_key(rec),
                    ),
                    None => (t("gen.deterministic"), String::new(), String::new()),
                };
                put(ui, [x0, y1], p.cream, &clip_label(ui, &provider, inner));
                let shown = clip_label(ui, &model, inner);
                put(ui, [x0, y2], p.muted, &shown);
                if (shown != model || !full.is_empty())
                    && ui.is_window_hovered()
                    && ui.is_mouse_hovering_rect([x0, o[1]], [x0 + w, o[1] + row_h])
                {
                    ui.tooltip_text(&full);
                }
            }
            Col::Duration | Col::Tokens | Col::Cost => {
                let (text, colour) = match col {
                    Col::Duration => (run_feed::format_duration(rec.duration_ms), p.cream),
                    Col::Tokens => match known_tokens(rec) {
                        Some(n) => (run_feed::format_tokens(n), p.cream),
                        None => ("\u{2013}".to_string(), p.muted),
                    },
                    _ => match rec.cost_estimate_usd {
                        Some(_) => (run_feed::cost_text(rec, ctx.currency), p.cream),
                        None => (t("gen.na"), p.muted),
                    },
                };
                put(ui, [x0, ymid], colour, &clip_label(ui, &text, inner));
                stats_span = [stats_span[0].min(x0), stats_span[1].max(x0 + w)];
            }
            Col::Build => draw_card(
                ui,
                rec,
                [x0, o[1] + 4.0],
                [w - 12.0, row_h - 8.0],
                ctx,
                clicks,
            ),
        }
    }
    if stats_span[1] > stats_span[0]
        && ui.is_window_hovered()
        && ui.is_mouse_hovering_rect([stats_span[0], o[1]], [stats_span[1], o[1] + row_h])
    {
        theme::wide_tooltip(ui, |ui| {
            for line in run_feed::tooltip_lines(rec, ctx.currency) {
                ui.text(line);
            }
        });
    }
    ui.set_cursor_screen_pos([o[0], o[1] + row_h + GAP]);
}

/// The build card: the provider-picks card shape, the whole card a button.
fn draw_card(
    ui: &Ui,
    rec: &Arc<GenerationRecord>,
    min: [f32; 2],
    size: [f32; 2],
    ctx: &RowCtx,
    clicks: &mut Clicks,
) {
    let p = theme::pal();
    let lh = ui.text_line_height();
    let openable = rec.build.is_some() && !ctx.optimizing;
    ui.set_cursor_screen_pos(min);
    let clicked = ui.invisible_button(format!("##gen_card_{}", rec.id), size);
    let hovered = ui.is_item_hovered();
    let max = [min[0] + size[0], min[1] + size[1]];
    {
        let dl = ui.get_window_draw_list();
        let fill = if hovered && openable {
            p.gold_hover
        } else {
            p.chip_idle_fill
        };
        dl.add_rect(min, max, fill)
            .filled(true)
            .rounding(4.0)
            .build();
        dl.add_rect(min, max, p.chip_idle_rim).rounding(4.0).build();
    }
    let x = min[0] + 6.0;
    let w = (size[0] - 12.0).max(8.0);
    let y1 = min[1] + ((size[1] - lh * 2.0 - 2.0) * 0.5).max(1.0);
    let y2 = y1 + lh + 2.0;
    if ctx.opening.as_deref() == Some(rec.id.as_str()) {
        let spin = ['|', '/', '-', '\\'][theme::anim_cell(133, 4)];
        put(
            ui,
            [x, y1],
            p.gold,
            &format!("{spin}  {}", t("gen.open.measure")),
        );
        return;
    }
    match (&rec.card, &rec.build) {
        (Some(card), Some(_)) => {
            let dps = card
                .simulated_dps
                .map(|d| format!("  \u{00b7}  {}", tf("gen.dps", &[("n", &d.to_string())])))
                .unwrap_or_default();
            let head = clip_label(ui, &card.title, (w - ui.calc_text_size(&dps)[0]).max(8.0));
            put(ui, [x, y1], p.gold, &head);
            let hw = ui.calc_text_size(&head)[0];
            put(ui, [x + hw, y1], p.cream, &dps);
            let line = [
                card.specs.join(" / "),
                // Records keep English; show the prefix in the overlay
                // language. A multi-prefix summary finds no itemstat and
                // stays as recorded.
                crate::ui::comparison::loc_prefix(ctx.db.as_deref(), &card.prefix_summary)
                    .to_string(),
                card.meter_text.clone(),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("  \u{00b7}  ");
            put(ui, [x, y2], p.muted, &clip_label(ui, &line, w));
        }
        _ => {
            let status = match &rec.status {
                GenerationStatus::Ok => t("run.done"),
                GenerationStatus::Cancelled => t("run.cancelled"),
                GenerationStatus::Failed { message } => format!("{}: {message}", t("run.failed")),
            };
            let text = tf("gen.no_build", &[("status", &status)]);
            put(ui, [x, y1], p.muted, &clip_label(ui, &text, w));
        }
    }
    if hovered {
        let hint = if rec.build.is_none() {
            String::new()
        } else if ctx.optimizing {
            t("gen.open_busy")
        } else {
            tf(
                "gen.open_hint",
                &[
                    ("character", &rec.character_name),
                    ("scenario", &record_scenario_line(rec)),
                ],
            )
        };
        if !hint.is_empty() {
            theme::wide_tooltip(ui, |ui| {
                ui.text(&hint);
                if let Some(code) = rec.card.as_ref().and_then(|c| c.chat_code.as_deref()) {
                    ui.text_colored(p.muted, code);
                }
            });
        }
    }
    if clicked && openable {
        clicks.open = Some(Arc::clone(rec));
    }
}

/// A muted caption and a combo, wrapped onto a new line when this one is
/// full. Returns the picked option.
fn filter_combo(
    ui: &Ui,
    avail: f32,
    row_x: &mut f32,
    caption: &str,
    id: &str,
    options: &[String],
    selected: usize,
) -> Option<usize> {
    let preview = options.get(selected).cloned().unwrap_or_default();
    theme::font_scale(ui, 0.85);
    let cap_w = ui.calc_text_size(caption)[0];
    theme::font_scale_reset(ui);
    // The widest option, so the row does not shift as the pick changes.
    let combo_w = options
        .iter()
        .map(|o| theme::combo_width_for(ui, o))
        .fold(110.0_f32, f32::max)
        .min(260.0);
    theme::wrap_chip(ui, avail, row_x, cap_w + 6.0 + combo_w, 14.0);
    ui.align_text_to_frame_padding();
    theme::font_scale(ui, 0.85);
    ui.text_colored(theme::pal().muted, caption);
    theme::font_scale_reset(ui);
    ui.same_line_with_spacing(0.0, 6.0);
    ui.set_next_item_width(combo_w);
    let mut picked = None;
    if let Some(_c) = ComboBox::new(id).preview_value(&preview).begin(ui) {
        for (i, label) in options.iter().enumerate() {
            if Selectable::new(format!("{label}{id}_{i}"))
                .selected(i == selected)
                .build(ui)
                && i != selected
            {
                picked = Some(i);
            }
        }
    }
    picked
}

fn render_filters(ui: &Ui, state: &mut AddonState) {
    let tab = &mut state.main.generations;
    let before = tab.filters.clone();
    let any = t("gen.filter.any");
    let avail = ui.content_region_avail()[0];
    let mut row_x = 0.0;

    const KINDS: [GenerationKind; 3] = [
        GenerationKind::NewBuild,
        GenerationKind::Improve,
        GenerationKind::Choya,
    ];
    let options: Vec<String> = std::iter::once(any.clone())
        .chain(KINDS.iter().map(|k| kind_label(*k)))
        .collect();
    let at = tab
        .filters
        .kind
        .and_then(|k| KINDS.iter().position(|x| *x == k))
        .map_or(0, |i| i + 1);
    if let Some(i) = filter_combo(
        ui,
        avail,
        &mut row_x,
        &t("gen.filter.type"),
        "##gen_f_kind",
        &options,
        at,
    ) {
        tab.filters.kind = i.checked_sub(1).map(|i| KINDS[i]);
    }

    let opts = tab.options();
    let names = &opts.names;
    let options: Vec<String> = std::iter::once(any.clone())
        .chain(names.iter().cloned())
        .collect();
    let at = tab
        .filters
        .character
        .as_ref()
        .and_then(|c| names.iter().position(|n| n == c))
        .map_or(0, |i| i + 1);
    if let Some(i) = filter_combo(
        ui,
        avail,
        &mut row_x,
        &t("gen.filter.character"),
        "##gen_f_char",
        &options,
        at,
    ) {
        tab.filters.character = i.checked_sub(1).map(|i| names[i].clone());
    }

    let options: Vec<String> = std::iter::once(any.clone())
        .chain(GameMode::ALL.iter().map(|m| m.label().to_string()))
        .collect();
    let at = tab
        .filters
        .mode
        .as_ref()
        .and_then(|m| GameMode::ALL.iter().position(|x| x == m))
        .map_or(0, |i| i + 1);
    if let Some(i) = filter_combo(
        ui,
        avail,
        &mut row_x,
        &t("gen.filter.mode"),
        "##gen_f_mode",
        &options,
        at,
    ) {
        tab.filters.mode = i.checked_sub(1).map(|i| GameMode::ALL[i].clone());
    }

    let llms = &opts.llms;
    let options: Vec<String> = std::iter::once(any.clone())
        .chain(llms.iter().map(|k| {
            if k.is_empty() {
                t("gen.deterministic")
            } else {
                k.clone()
            }
        }))
        .collect();
    let at = tab
        .filters
        .llm
        .as_ref()
        .and_then(|l| llms.iter().position(|k| k == l))
        .map_or(0, |i| i + 1);
    if let Some(i) = filter_combo(
        ui,
        avail,
        &mut row_x,
        &t("gen.filter.llm"),
        "##gen_f_llm",
        &options,
        at,
    ) {
        tab.filters.llm = i.checked_sub(1).map(|i| llms[i].clone());
    }

    let options: Vec<String> = When::ALL.iter().map(|w| w.label()).collect();
    let at = When::ALL
        .iter()
        .position(|w| *w == tab.filters.when)
        .unwrap_or(0);
    if let Some(i) = filter_combo(
        ui,
        avail,
        &mut row_x,
        &t("gen.filter.when"),
        "##gen_f_when",
        &options,
        at,
    ) {
        tab.filters.when = When::ALL[i];
    }

    let search_w = 220.0;
    theme::wrap_chip(ui, avail, &mut row_x, search_w, 14.0);
    ui.set_next_item_width(search_w);
    ui.input_text("##gen_q", &mut tab.filters.text)
        .hint(t("gen.search"))
        .build();

    if tab.filters != Filters::default() {
        let clear = t("gen.filter.clear");
        theme::wrap_chip(
            ui,
            avail,
            &mut row_x,
            theme::gold_button_width(ui, &clear),
            14.0,
        );
        if ui.button(&clear) {
            tab.filters = Filters::default();
        }
    }
    let refresh = t("gen.refresh");
    theme::wrap_chip(
        ui,
        avail,
        &mut row_x,
        theme::gold_button_width(ui, &refresh),
        14.0,
    );
    let refresh_clicked = theme::gold_button(ui, &refresh);
    if !tab.records.is_empty() {
        match &tab.load {
            LoadState::Loading => {
                ui.same_line_with_spacing(0.0, 10.0);
                ui.text_colored(theme::pal().muted, t("gen.refreshing"));
            }
            LoadState::Error(e) => {
                ui.same_line_with_spacing(0.0, 10.0);
                ui.text_colored(theme::WARN, tf("gen.load_failed", &[("err", e)]));
            }
            LoadState::Idle | LoadState::Ready => {}
        }
    }
    if tab.filters != before {
        tab.page = 0;
    }
    if refresh_clicked && tab.load != LoadState::Loading {
        start_load(state);
    }
}

/// The steps of a record being reopened, live.
fn render_opening(ui: &Ui, state: &mut AddonState) {
    let Some(o) = &state.main.generations.opening else {
        return;
    };
    let lh = ui.text_line_height_with_spacing();
    run_feed::render_steps(ui, &o.feed.steps, Some(o.ms()), "##gen_opening", lh * 5.0);
    // A failed open (the thread was refused) stays until dismissed.
    if o.feed.running().is_none() && ui.small_button(t("gen.open.dismiss")) {
        state.main.generations.opening = None;
    }
    ui.dummy([0.0, 4.0]);
}

/// Loading, failed or empty: a choya and one line, never a blank pane.
fn render_empty(ui: &Ui, load: &LoadState) {
    const MASCOT: f32 = 72.0;
    let top = ui.cursor_screen_pos();
    let centre = [top[0] + 12.0 + MASCOT * 0.5, top[1] + 6.0 + MASCOT * 0.5];
    let (text, colour) = match load {
        LoadState::Idle | LoadState::Loading => {
            theme::draw_choya_thinking(ui, centre, MASCOT);
            (t("gen.loading"), theme::pal().muted)
        }
        LoadState::Error(e) => {
            theme::draw_choya_sleep(ui, centre, MASCOT);
            (tf("gen.load_failed", &[("err", e)]), theme::WARN)
        }
        LoadState::Ready => {
            theme::draw_choya_sleep(ui, centre, MASCOT);
            (t("gen.empty"), theme::pal().muted)
        }
    };
    ui.set_cursor_screen_pos([top[0] + MASCOT + 32.0, top[1] + MASCOT * 0.5 - 2.0]);
    theme::wrapped(ui, colour, &text);
    ui.set_cursor_screen_pos([top[0], top[1] + MASCOT + 16.0]);
    ui.dummy([0.0, 0.0]);
}

fn draw_pager(
    ui: &Ui,
    page: usize,
    pages: usize,
    shown: (usize, usize, usize),
    clicks: &mut Clicks,
) {
    let p = theme::pal();
    let lh = ui.text_line_height();
    ui.dummy([0.0, 2.0]);
    let mut first = true;
    let mut step =
        |ui: &Ui, label: &str, id: &str, target: usize, enabled: bool, selected: bool| {
            if !first {
                ui.same_line_with_spacing(0.0, 4.0);
            }
            first = false;
            let _dim = (!enabled).then(|| ui.push_style_var(StyleVar::Alpha(0.35)));
            if theme::pill(ui, label, selected, id) && enabled && !selected {
                clicks.page = Some(target);
            }
        };
    let last = pages - 1;
    step(ui, "\u{00ab}", "##gen_pg_first", 0, page > 0, false);
    step(
        ui,
        "\u{2039}",
        "##gen_pg_prev",
        page.saturating_sub(1),
        page > 0,
        false,
    );
    for (i, item) in pager_items(page, pages).into_iter().enumerate() {
        match item {
            PagerItem::Page(n) => step(
                ui,
                &(n + 1).to_string(),
                &format!("##gen_pg_{n}"),
                n,
                true,
                n == page,
            ),
            PagerItem::Gap => step(
                ui,
                "\u{2026}",
                &format!("##gen_pg_gap{i}"),
                page,
                false,
                false,
            ),
        }
    }
    step(
        ui,
        "\u{203a}",
        "##gen_pg_next",
        (page + 1).min(last),
        page < last,
        false,
    );
    step(ui, "\u{00bb}", "##gen_pg_last", last, page < last, false);
    let (from, to, n) = shown;
    ui.same_line_with_spacing(0.0, 14.0);
    let c = ui.cursor_screen_pos();
    put(
        ui,
        [c[0], c[1] + 3.0],
        p.muted,
        &tf(
            "gen.range",
            &[
                ("from", &from.to_string()),
                ("to", &to.to_string()),
                ("n", &n.to_string()),
            ],
        ),
    );
    ui.dummy([1.0, lh + 6.0]);
}

/// Height kept free for an expanded run log under the pager.
fn timeline_room(lh: f32, avail_h: f32) -> (f32, f32) {
    let steps_h = (avail_h * 0.3).clamp(lh * 4.0, lh * 14.0);
    (steps_h, lh + 12.0 + steps_h)
}

pub(in crate::ui::main_view) fn render_generations(ui: &Ui, state: &mut AddonState) {
    theme::font_scale_reset(ui);
    ensure_loaded(state);
    render_filters(ui, state);
    ui.dummy([0.0, 6.0]);
    render_opening(ui, state);

    if state.main.generations.records.is_empty() {
        render_empty(ui, &state.main.generations.load);
        return;
    }
    let rows = state.main.generations.rows(Utc::now());
    let tab = &state.main.generations;
    if rows.is_empty() {
        ui.text_colored(theme::pal().muted, t("gen.no_match"));
        return;
    }

    let lh = ui.text_line_height();
    let row_h = (lh * 2.0 + 14.0).round();
    let header_h = (lh + 12.0).round();
    let pager_h = lh + 6.0 + 16.0;
    let avail_h = ui.content_region_avail()[1] - 4.0;
    let expanded = tab
        .expanded
        .as_ref()
        .and_then(|id| tab.records.iter().find(|r| &r.id == id))
        .cloned();
    let (steps_h, timeline_h) = timeline_room(lh, avail_h);
    let timeline_h = if expanded.is_some() { timeline_h } else { 0.0 };
    let per_page = rows_per_page(avail_h - timeline_h, header_h + GAP, pager_h, row_h, GAP);
    let pages = page_count(rows.len(), per_page);
    let page = tab.page.min(pages - 1);
    let start = page * per_page;
    let end = (start + per_page).min(rows.len());
    let page_rows: Vec<Arc<GenerationRecord>> = rows[start..end]
        .iter()
        .map(|&i| Arc::clone(&tab.records[i]))
        .collect();
    let (sort, asc) = (tab.sort, tab.sort_asc);
    let ctx = RowCtx {
        db: state.main.game_db.clone(),
        optimizing: state.main.optimizing,
        opening: tab
            .opening
            .as_ref()
            .filter(|o| o.feed.running().is_some())
            .map(|o| o.id.clone()),
        expanded: tab.expanded.clone(),
        currency: state.config.cost_currency,
    };
    let table_h = header_h + GAP + per_page as f32 * (row_h + GAP);

    let mut clicks = Clicks::default();
    // Fixed height, no scrolling: the pager right under it is always in view.
    ChildWindow::new("##gen_table")
        .size([0.0, table_h])
        .scroll_bar(false)
        .scrollable(false)
        .build(ui, || {
            // Nested children start at scale 1.0; match the player's.
            theme::font_scale(ui, 1.0);
            let cols = Columns::new(ui, ui.content_region_avail()[0]);
            draw_header(ui, &cols, header_h, sort, asc, &mut clicks);
            for rec in &page_rows {
                draw_row(ui, &cols, rec, row_h, &ctx, &mut clicks);
            }
        });
    draw_pager(ui, page, pages, (start + 1, end, rows.len()), &mut clicks);

    if let Some(rec) = &expanded {
        ui.dummy([0.0, 4.0]);
        let head = format!(
            "{}  \u{00b7}  {}  \u{00b7}  {}",
            local_time(rec.started_at),
            title(rec),
            run_feed::pill_text(rec, ctx.currency)
        );
        let w = ui.content_region_avail()[0];
        ui.text_colored(theme::pal().gold, clip_label(ui, &head, w));
        if rec.steps.is_empty() {
            ui.text_colored(theme::pal().muted, t("gen.no_steps"));
        } else {
            run_feed::render_steps(ui, &rec.steps, None, "##gen_timeline", steps_h);
        }
    }

    let tab = &mut state.main.generations;
    tab.page = page;
    if let Some(key) = clicks.sort {
        if tab.sort == key {
            tab.sort_asc = !tab.sort_asc;
        } else {
            tab.sort = key;
            tab.sort_asc = key.default_asc();
        }
        tab.page = 0;
    }
    if let Some(n) = clicks.page {
        tab.page = n.min(pages - 1);
    }
    if let Some(id) = clicks.expand {
        tab.expanded = (tab.expanded.as_deref() != Some(id.as_str())).then_some(id);
    }
    if let Some(rec) = clicks.open {
        open_generation(state, rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_core::config::LlmProvider;
    use gw2_core::generations::{GenerationCard, LlmUsed, TokenUsage};

    fn rec(
        n: i64,
        kind: GenerationKind,
        character: &str,
        cost: Option<f64>,
        dps: Option<i32>,
    ) -> Arc<GenerationRecord> {
        let started_at = DateTime::from_timestamp(1_790_000_000 + n * 60, 0).expect("time");
        Arc::new(GenerationRecord {
            id: format!("r{n}"),
            started_at,
            finished_at: started_at,
            duration_ms: 1_000 * n as u64,
            llm_wait_ms: 0,
            compute_ms: 0,
            tier_timings: Vec::new(),
            kind,
            character_name: character.into(),
            profession: "Guardian".into(),
            elite_spec: None,
            mode: GameMode::WvW,
            scale_id: None,
            role_id: None,
            scale: String::new(),
            role: String::new(),
            weights: serde_json::Value::Null,
            llm: cost.map(|_| LlmUsed {
                provider: LlmProvider::Gemini,
                model: "gemini-2.5-flash".into(),
            }),
            tokens: TokenUsage {
                total: if cost.is_some() { 1_000 * n as u64 } else { 0 },
                ..Default::default()
            },
            cost_estimate_usd: cost,
            pricing_source: None,
            tier: None,
            status: GenerationStatus::Ok,
            build: None,
            card: Some(GenerationCard {
                title: format!("Build {n}"),
                simulated_dps: dps,
                ..Default::default()
            }),
            steps: Vec::new(),
        })
    }

    /// Newest first, as the tab holds them.
    fn sample() -> Vec<Arc<GenerationRecord>> {
        vec![
            rec(5, GenerationKind::Choya, "bravo", None, None),
            rec(4, GenerationKind::Improve, "Alpha", Some(0.02), Some(9_000)),
            rec(3, GenerationKind::NewBuild, "alpha", Some(0.02), None),
            rec(
                2,
                GenerationKind::NewBuild,
                "Charlie",
                Some(0.50),
                Some(12_000),
            ),
            rec(1, GenerationKind::Improve, "bravo", None, Some(3_000)),
        ]
    }

    fn ids(records: &[Arc<GenerationRecord>], rows: &[usize]) -> Vec<String> {
        rows.iter().map(|&i| records[i].id.clone()).collect()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000 + 10 * 60, 0).expect("time")
    }

    #[test]
    fn rows_per_page_fills_the_height_and_keeps_three() {
        // 600 - 36 - 34 = 530 of room; rows of 40 + 6.
        assert_eq!(rows_per_page(600.0, 36.0, 34.0, 40.0, 6.0), 11);
        let table = 36.0 + 11.0 * 46.0 + 34.0;
        assert!(table <= 600.0, "the pager still fits: {table}");
        assert_eq!(
            rows_per_page(80.0, 36.0, 34.0, 40.0, 6.0),
            3,
            "floor of three"
        );
        assert_eq!(rows_per_page(-50.0, 36.0, 34.0, 40.0, 6.0), 3);
        assert_eq!(page_count(0, 11), 1, "an empty table is one page");
        assert_eq!(page_count(22, 11), 2);
        assert_eq!(page_count(23, 11), 3);
    }

    #[test]
    fn pager_shows_ends_neighbours_and_ellipses() {
        use PagerItem::{Gap, Page};
        assert_eq!(pager_items(0, 1), [Page(0)]);
        assert_eq!(pager_items(3, 7), (0..7).map(Page).collect::<Vec<_>>());
        assert_eq!(
            pager_items(0, 10),
            [Page(0), Page(1), Page(2), Page(3), Page(4), Gap, Page(9)]
        );
        assert_eq!(
            pager_items(5, 10),
            [Page(0), Gap, Page(4), Page(5), Page(6), Gap, Page(9)]
        );
        assert_eq!(
            pager_items(9, 10),
            [Page(0), Gap, Page(5), Page(6), Page(7), Page(8), Page(9)]
        );
        assert_eq!(
            pager_items(4, 9),
            [Page(0), Gap, Page(3), Page(4), Page(5), Gap, Page(8)]
        );
        assert_eq!(
            pager_items(3, 9),
            [Page(0), Page(1), Page(2), Page(3), Page(4), Gap, Page(8)]
        );
        assert_eq!(pager_items(99, 10).last(), Some(&Page(9)), "clamped");
    }

    #[test]
    fn filters_keep_only_matching_runs() {
        let records = sample();
        let all = |f: &Filters| {
            ids(
                &records,
                &visible_rows(&records, f, SortKey::Date, false, now()),
            )
        };
        assert_eq!(all(&Filters::default()), ["r5", "r4", "r3", "r2", "r1"]);
        let f = Filters {
            kind: Some(GenerationKind::Improve),
            ..Default::default()
        };
        assert_eq!(all(&f), ["r4", "r1"]);
        let f = Filters {
            character: Some("bravo".into()),
            ..Default::default()
        };
        assert_eq!(all(&f), ["r5", "r1"]);
        let f = Filters {
            llm: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(all(&f), ["r5", "r1"], "runs with no LLM request");
        let f = Filters {
            llm: Some("Gemini \u{00b7} gemini-2.5-flash".into()),
            text: "CHARLIE".into(),
            ..Default::default()
        };
        assert_eq!(all(&f), ["r2"], "free text is case-insensitive");
        let f = Filters {
            text: "build 3".into(),
            ..Default::default()
        };
        assert_eq!(all(&f), ["r3"], "title matches");
        let f = Filters {
            mode: Some(GameMode::PvE),
            ..Default::default()
        };
        assert!(all(&f).is_empty());
    }

    #[test]
    fn date_presets_cut_by_age() {
        let now = now();
        let at = |days: i64| now - chrono::Duration::days(days);
        assert!(When::Today.keeps(now, now));
        assert!(!When::Today.keeps(at(2), now));
        assert!(When::Week.keeps(at(6), now));
        assert!(!When::Week.keeps(at(8), now));
        assert!(When::Month.keeps(at(29), now));
        assert!(!When::Month.keeps(at(31), now));
        assert!(When::All.keeps(at(400), now));
    }

    #[test]
    fn sorting_is_stable_and_puts_unknowns_last_both_ways() {
        let records = sample();
        let order = |key, asc| {
            ids(
                &records,
                &visible_rows(&records, &Filters::default(), key, asc, now()),
            )
        };
        // Cost: r4 and r3 tie at 0.02 and keep their newest-first order.
        assert_eq!(order(SortKey::Cost, false), ["r2", "r4", "r3", "r5", "r1"]);
        assert_eq!(order(SortKey::Cost, true), ["r4", "r3", "r2", "r5", "r1"]);
        assert_eq!(order(SortKey::Dps, false), ["r2", "r4", "r1", "r5", "r3"]);
        assert_eq!(order(SortKey::Dps, true), ["r1", "r4", "r2", "r5", "r3"]);
        // Tokens are unknown without an LLM.
        assert_eq!(
            order(SortKey::Tokens, false),
            ["r4", "r3", "r2", "r5", "r1"]
        );
        // Names ignore case; ties keep order.
        assert_eq!(
            order(SortKey::Character, true),
            ["r4", "r3", "r5", "r1", "r2"]
        );
        assert_eq!(order(SortKey::Kind, true), ["r3", "r2", "r4", "r1", "r5"]);
        assert_eq!(order(SortKey::Date, true), ["r1", "r2", "r3", "r4", "r5"]);
        assert_eq!(
            order(SortKey::Duration, false),
            ["r5", "r4", "r3", "r2", "r1"]
        );
    }

    /// Every key this tab reads exists in all twelve catalogs, so no
    /// language falls back to English silently.
    #[test]
    fn every_generations_key_is_in_every_catalog() {
        const KEYS: &[&str] = &[
            "about.view.generations",
            "gen.col.date",
            "gen.col.type",
            "gen.col.character",
            "gen.col.scenario",
            "gen.col.llm",
            "gen.col.duration",
            "gen.col.tokens",
            "gen.col.cost",
            "gen.col.build",
            "gen.filter.type",
            "gen.filter.character",
            "gen.filter.mode",
            "gen.filter.llm",
            "gen.filter.when",
            "gen.filter.any",
            "gen.filter.clear",
            "gen.when.all",
            "gen.when.today",
            "gen.when.week",
            "gen.when.month",
            "gen.search",
            "gen.refresh",
            "gen.refreshing",
            "gen.loading",
            "gen.load_failed",
            "gen.thread_failed",
            "gen.empty",
            "gen.no_match",
            "gen.range",
            "gen.dps",
            "gen.na",
            "gen.no_build",
            "gen.no_character",
            "gen.open_hint",
            "gen.open_busy",
            "gen.expand",
            "gen.char_missing",
            "gen.open.scenario",
            "gen.open.character",
            "gen.open.measure",
            "gen.open.dismiss",
            "gen.no_steps",
        ];
        let catalogs = [
            ("en", include_str!("../../../../../../../locales/en.json")),
            ("fr", include_str!("../../../../../../../locales/fr.json")),
            ("de", include_str!("../../../../../../../locales/de.json")),
            ("es", include_str!("../../../../../../../locales/es.json")),
            ("it", include_str!("../../../../../../../locales/it.json")),
            ("pt", include_str!("../../../../../../../locales/pt.json")),
            ("nl", include_str!("../../../../../../../locales/nl.json")),
            ("pl", include_str!("../../../../../../../locales/pl.json")),
            ("ru", include_str!("../../../../../../../locales/ru.json")),
            ("zh", include_str!("../../../../../../../locales/zh.json")),
            ("ja", include_str!("../../../../../../../locales/ja.json")),
            ("ko", include_str!("../../../../../../../locales/ko.json")),
        ];
        for (lang, text) in catalogs {
            let map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(text).expect("catalog parses");
            for key in KEYS {
                assert!(
                    map.get(*key)
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty()),
                    "{lang} lacks {key}"
                );
            }
        }
    }

    /// The hand-built Daredevil as a plate the validator accepts: two core
    /// lines beside the elite, one trait per tier in each.
    fn three_spec_thief() -> (
        gw2_optimizer::gamedb::GameDb,
        gw2_optimizer::validation::ValidatedBuild,
    ) {
        use gw2_optimizer::validation::ValidatedSpec;
        let (mut db, mut v) = crate::ui::main_view::optimization::tests::hand_built_thief();
        for (tier, id) in [1u32, 4, 7].into_iter().enumerate() {
            if let Some(t) = db.traits.get_mut(&id) {
                t.tier = tier as u32 + 1;
            }
        }
        let elite = db.specializations[&7].clone();
        let base = db.traits[&1].clone();
        let mut specs = Vec::new();
        for (spec_id, name) in [(20u32, "Deadly Arts"), (21, "Critical Strikes")] {
            let mut spec = elite.clone();
            spec.id = spec_id;
            spec.name = name.into();
            spec.elite = false;
            let ids: Vec<u32> = (1..=3).map(|n| spec_id * 10 + n).collect();
            spec.major_traits = ids.clone();
            db.specializations.insert(spec_id, spec);
            let mut names = Vec::new();
            for (n, id) in ids.iter().enumerate() {
                let mut tr = base.clone();
                tr.id = *id;
                tr.name = format!("{name} {}", n + 1);
                tr.specialization = spec_id;
                tr.tier = n as u32 + 1;
                names.push(tr.name.clone());
                db.traits.insert(*id, tr);
            }
            db.traits_by_spec.insert(spec_id, ids.clone());
            specs.push(ValidatedSpec {
                spec_id,
                name: name.into(),
                elite: false,
                trait_ids: ids.clone(),
                trait_names: names,
                all_trait_ids: ids,
            });
        }
        if let Some(p) = db.professions.get_mut("Thief") {
            p.specializations = vec![20, 21, 7];
        }
        let mut daredevil = v.specializations.remove(0);
        daredevil.trait_names = [1u32, 4, 7]
            .iter()
            .map(|id| db.traits[id].name.clone())
            .collect();
        specs.push(daredevil);
        v.specializations = specs;
        (db, v)
    }

    /// Opening a record on the hand-built db: the scenario and the
    /// character come back, the build is measured and plated with the
    /// original run's record attached, and the tab it was made on opens. A
    /// record whose character is gone keeps the current one and says so.
    #[test]
    fn opening_a_record_restores_character_and_scenario_and_plates_the_build() {
        use crate::ui::main_view::optimization::tests::run_meta;
        use gw2_core::generations::GenerationTier;
        use gw2_optimizer::scenario::{CombatTier, ScenarioSpec};

        let _serial = crate::state::state_test_guard();
        let dir = std::env::temp_dir().join(format!("gw2bo_gen_open_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        crate::state::clear();
        crate::state::init(dir.clone());
        gw2_core::i18n::set_language("en");

        // A real record: a deterministic run of the hand-built Daredevil,
        // made on "Tester" in WvW, Roam, Damage.
        let (db, v) = three_spec_thief();
        let meta = run_meta(&dir);
        let run_weights = meta.weights.clone();
        let ctx = gw2_optimizer::balance::BalanceContext::new(GameMode::WvW);
        let scenario = ScenarioSpec::for_request(&ctx, CombatTier::Solo, None, &run_weights);
        let tracker = crate::ui::main_view::generation::RunTracker::start(meta);
        let result = gw2_optimizer::engine::synergy_result_from_validated(
            v,
            &db,
            "Thief",
            &ctx,
            Some(&scenario),
        );
        let served = crate::ui::main_view::optimization::synergy_result_to_suggestion(
            &result,
            &db,
            "Thief",
            &scenario,
            None,
            None,
            None,
            &run_weights,
            &ctx,
        );
        let record = tracker.finish(
            GenerationStatus::Ok,
            Some(crate::ui::main_view::generation::Served {
                suggestion: &served,
                tier: GenerationTier::Deterministic,
                db: Some(&db),
            }),
        );
        assert_eq!(record.scale_id.as_deref(), Some("Solo"));
        assert_eq!(record.role_id.as_deref(), Some("PowerDps"));
        assert_eq!(
            record_scenario_line(&record),
            "WvW \u{00b7} Roam \u{00b7} Damage",
            "labels derive from ids"
        );

        with_state(|s| {
            assert!(
                s.main.generations.stale,
                "the append marks the history stale"
            );
            s.main.characters = vec!["Other".into(), "Tester".into()];
            s.main.selected_character = Some(0);
            s.main.game_mode = GameMode::PvE;
            s.main.combat_tier = CombatTier::Squad;
            s.main.selected_role = None;
            s.main.game_db = Some(Arc::new(db));
        });
        let job = with_state(|s| prepare_open(s, Arc::clone(&record)))
            .flatten()
            .expect("a served build opens");
        with_state(|s| {
            let main = &s.main;
            assert_eq!(main.selected_character, Some(1), "character restored");
            assert_eq!(main.game_mode, GameMode::WvW, "mode restored");
            assert_eq!(main.combat_tier, CombatTier::Solo, "scale restored");
            assert_eq!(main.selected_role, Some(RoleObjective::PowerDps));
            assert_eq!(main.weights, run_weights, "the run's own weights");
            let opening = main.generations.opening.as_ref().expect("steps shown");
            assert!(opening.feed.running().is_some(), "measuring is a live step");
        });
        let suggestion = job.measure();
        with_state(|s| finish_open(s, job, suggestion));
        with_state(|s| {
            let main = &s.main;
            assert!(main.generations.opening.is_none());
            assert_eq!(main.comparison.suggestions.len(), 1, "plated");
            let plated = &main.comparison.suggestions[0];
            assert!(main.comparison.show_optimized);
            assert_eq!(
                plated.generation.as_ref().map(|g| g.id.as_str()),
                Some(record.id.as_str()),
                "the run log rides on the tab"
            );
            assert!(
                plated
                    .rotation
                    .as_ref()
                    .is_some_and(|r| r.simulated_dps > 0),
                "measured on the hand-built db: {:?} {:?} {:?}",
                plated.quality_reasons,
                plated.skills,
                plated.weapons
            );
            assert_eq!(main.active_tab, MainTab::NewBuild, "a New Build run");
        });

        // The same run, made on a character this account no longer has.
        let mut gone = (*record).clone();
        gone.id = "gone".into();
        gone.character_name = "Deleted Char".into();
        gone.kind = GenerationKind::Improve;
        let job = with_state(|s| prepare_open(s, Arc::new(gone)))
            .flatten()
            .expect("opens anyway");
        with_state(|s| {
            assert_eq!(s.main.selected_character, Some(1), "current one kept");
        });
        let suggestion = job.measure();
        with_state(|s| {
            finish_open(s, job, suggestion);
            let plated = s.main.comparison.suggestions.last().expect("plated");
            assert!(
                plated
                    .quality_reasons
                    .iter()
                    .any(|r| r == "Character not found: Deleted Char"),
                "{:?}",
                plated.quality_reasons
            );
            assert_eq!(
                s.main.active_tab,
                MainTab::NewBuild,
                "not Improve on someone else's character"
            );
        });

        crate::state::clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A measuring worker that leaves without plating (a cancel return, a
    /// panic in `measure()`) fails the measuring step, so the card stops
    /// spinning and Dismiss shows; one that finished leaves nothing to close.
    #[test]
    fn an_open_that_ends_without_plating_is_closed() {
        let _serial = crate::state::state_test_guard();
        let dir = std::env::temp_dir().join(format!("gw2bo_gen_guard_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        crate::state::clear();
        crate::state::init(dir.clone());
        gw2_core::i18n::set_language("en");
        let start = |id: &str| {
            with_state(|s| {
                let mut o = Opening {
                    id: id.into(),
                    started: Instant::now(),
                    feed: RunFeed::default(),
                    measure_step: None,
                };
                o.measure_step = Some(o.note(t("gen.open.measure"), StepState::Running));
                s.main.generations.opening = Some(o);
            });
        };
        let measure_state = || {
            with_state(|s| {
                let o = s.main.generations.opening.as_ref().expect("open");
                assert!(o.feed.running().is_none(), "Dismiss shows");
                o.feed.steps[0].state.clone()
            })
            .expect("state")
        };

        start("a");
        let caught = std::panic::catch_unwind(|| {
            let _guard = OpenGuard("a".into());
            panic!("measure() blew up");
        });
        assert!(caught.is_err());
        assert_eq!(
            measure_state(),
            StepState::Failed {
                message: t("run.failed")
            }
        );

        start("b");
        drop(OpenGuard("b".into()));
        assert_eq!(
            measure_state(),
            StepState::Failed {
                message: t("run.cancelled")
            },
            "a cancel return"
        );

        // Another record's worker does not touch this one's card.
        start("c");
        drop(OpenGuard("b".into()));
        with_state(|s| {
            let o = s.main.generations.opening.as_ref().expect("open");
            assert!(o.feed.running().is_some(), "still measuring");
            s.main.generations.opening = None;
        });
        drop(OpenGuard("c".into()));
        with_state(|s| assert!(s.main.generations.opening.is_none()));

        crate::state::clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row set and the option lists are built once and rebuilt when the
    /// filters or the records change, not every frame.
    #[test]
    fn rows_and_options_are_cached_until_something_changes() {
        let mut tab = GenerationsTab::default();
        tab.set_records(sample());
        let now = now();
        let first = tab.rows(now);
        assert!(
            Arc::ptr_eq(&first, &tab.rows(now)),
            "same frame inputs, no rebuild"
        );
        let opts = tab.options();
        assert!(Arc::ptr_eq(&opts, &tab.options()));
        assert_eq!(opts.names, ["Alpha", "alpha", "bravo", "Charlie"]);

        tab.filters.character = Some("bravo".into());
        let bravo = tab.rows(now);
        assert_eq!(ids(&tab.records, &bravo), ["r5", "r1"]);

        tab.set_records(sample()[..2].to_vec());
        let rows = tab.rows(now);
        assert_eq!(ids(&tab.records, &rows), ["r5"], "a new read rebuilds");
        assert!(!Arc::ptr_eq(&opts, &tab.options()));
    }

    /// Records written before the ids existed show the text they stored.
    #[test]
    fn old_records_show_their_stored_labels() {
        let _serial = crate::state::state_test_guard();
        gw2_core::i18n::set_language("en");
        let mut old = (*rec(1, GenerationKind::NewBuild, "a", None, None)).clone();
        old.scale = "Roam".into();
        old.role = "Damage".into();
        assert_eq!(
            record_scenario_line(&old),
            "WvW \u{00b7} Roam \u{00b7} Damage"
        );
        old.scale_id = Some("Squad".into());
        old.role_id = Some("Buffer".into());
        assert_eq!(
            record_scenario_line(&old),
            "WvW \u{00b7} Cloud/Zerg \u{00b7} Support",
            "ids win over stored text"
        );
    }
}
