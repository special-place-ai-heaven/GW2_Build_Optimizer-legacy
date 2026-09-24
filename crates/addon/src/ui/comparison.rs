//! Side-by-side build comparison view.
//! Shows current build vs optimized build with full stat tables, bonuses,
//! effects/resistances, and LLM explanation.

use nexus::imgui::{TreeNodeFlags, Ui};

use gw2_core::types::{CombatMetrics, GearSlots, ResolvedBuild, RotationBreakdown, StatBlock};
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::ViabilityReport;

use gw2_core::i18n::{t, tf};

/// A build suggestion from the optimizer + LLM.
#[derive(Debug, Clone, Default)]
pub struct BuildSuggestion {
    pub label: String,
    pub build_summary: String,
    pub stat_prefix: String,
    /// Effective per-slot prefixes. This is authoritative when the optimizer
    /// proposes a mixed allocation; `None` falls back to `stat_prefix` per piece.
    pub slot_prefixes: Option<GearSlots>,
    pub specializations: Vec<(String, Vec<String>)>, // (spec_name, [trait1, trait2, trait3])
    pub weapons: Vec<String>,
    pub skills: Vec<String>,
    pub rune: String,
    pub sigils: Vec<String>,
    pub relic: String,
    /// Generated GW2 build-template chat code for this suggestion, when all required IDs are known.
    pub chat_code: Option<String>,
    pub explanation: String,
    /// Synergy-focused explanation from the new pipeline (preferred over `explanation`).
    pub synergy_explanation: String,
    pub changes_made: Vec<String>,
    pub estimated_stats: Option<StatBlock>,
    /// Combat metrics under Solo profile (gear+traits only).
    pub combat_solo: Option<CombatMetrics>,
    /// Combat metrics under Party profile (Might x15, Fury).
    pub combat_party: Option<CombatMetrics>,
    /// Combat metrics under Full Squad profile (Might x25, Fury, Vulnerability x25).
    pub combat_squad: Option<CombatMetrics>,
    /// Rotation simulation breakdown (if simulation was run).
    pub rotation: Option<RotationBreakdown>,
    /// Viability gate report from the referee (None for legacy/LLM paths that skip the referee).
    /// Populated for S07 Trust UI rendering.
    pub viability: Option<ViabilityReport>,
    /// Benchmark delta vs closest community reference build.
    /// None when no benchmark data has been scraped yet.
    pub benchmark_delta: Option<gw2_optimizer::benchmark::BenchmarkDelta>,
    /// Whether any community builds were on disk when this was evaluated.
    ///
    /// `benchmark_delta: None` has two very different causes and the overlay
    /// named the wrong one: never synced, or synced but nothing published for
    /// this profession and role could be scored in this scenario.
    pub benchmarks_synced: bool,
    /// Data quality assessment from the optimizer pipeline.
    pub data_quality: gw2_optimizer::data::DataQuality,
    /// Human-readable reasons for quality degradation (empty when Verified).
    pub quality_reasons: Vec<String>,
    /// The referee's coverage detail — the source names after
    /// "Not simulated: " — drawn beside the quality marker through the
    /// `quality.coverage_line` locale key. `None` when every equipped source
    /// with a record was executed (which is not a claim that every mechanic
    /// is modeled). One line, one source of truth (specs/004, FR-010).
    pub coverage_note: Option<String>,
    /// Where this build was published, when it came from a community site
    /// rather than from Choya. Empty for anything we cooked ourselves.
    ///
    /// It travels on the suggestion rather than being looked up again at
    /// render time, because by then the build has moved tabs and nothing on
    /// screen remembers which row it came from.
    pub source_url: String,
    /// The run that produced this build: time, model, tokens, cost, steps.
    /// Shared by every tab of that run; `None` for loaded and published builds.
    pub generation: Option<std::sync::Arc<gw2_core::generations::GenerationRecord>>,
}

/// Which result view is showing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResultPane {
    #[default]
    Build,
    Stats,
}

impl ResultPane {
    const ALL: [(ResultPane, &'static str); 2] = [
        (ResultPane::Build, "pane.build"),
        (ResultPane::Stats, "pane.stats"),
    ];
}

/// Compact section tabs as gold pills. Wraps instead of clipping on a narrow overlay.
pub fn render_result_pane_tabs(ui: &Ui, pane: &mut ResultPane) {
    let avail = ui.content_region_avail()[0];
    let mut row_x = 0.0;
    for (i, (tab, key)) in ResultPane::ALL.iter().enumerate() {
        let label = t(key);
        let pill_w = ui.calc_text_size(&label)[0] + 20.0;
        if i > 0 {
            if row_x + pill_w + 6.0 > avail {
                row_x = 0.0;
            } else {
                ui.same_line_with_spacing(0.0, 6.0);
            }
        }
        let selected = *pane == *tab;
        if crate::ui::theme::pill(ui, &label, selected, &format!("##pane_{i}")) {
            *pane = *tab;
        }
        row_x += pill_w + 6.0;
    }
}

/// Hover a skill/trait/upgrade name to show live GameDb description + facts.
pub fn inspect_if_hovered(ui: &Ui, name: &str, db: Option<&GameDb>) {
    if !ui.is_item_hovered() {
        return;
    }
    let Some(db) = db else {
        return;
    };
    let Some(tip) = inspect_text(name, db) else {
        return;
    };
    crate::ui::theme::wide_tooltip(ui, |ui| {
        let mut lines = tip.lines();
        if let Some(title) = lines.next() {
            ui.text_colored(crate::ui::theme::pal().gold, title);
        }
        for line in lines {
            ui.text(line);
        }
    });
}

pub fn loc_name<'a>(db: Option<&'a GameDb>, english: &'a str) -> &'a str {
    db.map(|d| d.loc_name(english)).unwrap_or(english)
}

pub(crate) fn compact_stance_name(name: &str) -> String {
    name.trim_start_matches("Legendary ")
        .trim_end_matches(" Stance")
        .to_string()
}

pub(crate) fn compact_pet_name(name: &str) -> String {
    name.trim_start_matches("Juvenile ").to_string()
}

fn fact_line(fact: &gw2_api::models::facts::Fact) -> Option<String> {
    use gw2_api::models::facts::Fact;
    match fact {
        Fact::AttributeAdjust {
            text,
            target: Some(t),
            value: Some(v),
            ..
        } => {
            if gw2_optimizer::stats::is_permanent_stat_adjust(text.as_deref()) {
                Some(format!("{t}: {v:+}"))
            } else {
                text.as_ref().map(|label| format!("{label}: {v}"))
            }
        }
        Fact::Buff {
            status: Some(s),
            duration,
            apply_count,
            ..
        } => {
            let dur = duration.map(|d| format!(" {d}s")).unwrap_or_default();
            let stacks = apply_count
                .filter(|&c| c > 1)
                .map(|c| format!(" x{c}"))
                .unwrap_or_default();
            Some(format!("Applies {s}{dur}{stacks}"))
        }
        Fact::PrefixedBuff {
            status: Some(s),
            duration,
            apply_count,
            prefix,
            ..
        } => {
            let dur = duration.map(|d| format!(" {d}s")).unwrap_or_default();
            let stacks = apply_count
                .filter(|&c| c > 1)
                .map(|c| format!(" x{c}"))
                .unwrap_or_default();
            let pfx = prefix
                .as_ref()
                .and_then(|p| p.status.as_ref())
                .map(|ps| format!(" (on {ps})"))
                .unwrap_or_default();
            Some(format!("Applies {s}{dur}{stacks}{pfx}"))
        }
        Fact::Damage {
            hit_count,
            dmg_multiplier,
            ..
        } => Some(format!(
            "Damage: {}\u{00d7} (coeff {:.2})",
            hit_count.unwrap_or(1),
            dmg_multiplier.unwrap_or(1.0)
        )),
        Fact::Heal { hit_count, .. } | Fact::HealingAdjust { hit_count, .. } => {
            Some(format!("Healing: {}\u{00d7}", hit_count.unwrap_or(1)))
        }
        Fact::Percent {
            text: Some(t),
            percent: Some(p),
            ..
        } => Some(format!("{t}: {p}%")),
        Fact::Recharge { value: Some(v), .. } => Some(format!("Recharge: {v}s")),
        Fact::Range { value: Some(v), .. } => Some(format!("Range: {v}")),
        Fact::Radius {
            distance: Some(d), ..
        } => Some(format!("Radius: {d}")),
        Fact::BuffConversion {
            source: Some(s),
            target: Some(t),
            percent: Some(p),
            ..
        } => Some(format!("Convert {p}% {s} \u{2192} {t}")),
        Fact::StunBreak {
            value: Some(true), ..
        } => Some("Stun break".to_string()),
        Fact::Unblockable {
            value: Some(true), ..
        } => Some("Unblockable".to_string()),
        Fact::ComboField {
            field_type: Some(ft),
            ..
        } => Some(format!("Combo field: {ft}")),
        Fact::ComboFinisher {
            finisher_type: Some(ft),
            percent,
            ..
        } => {
            let pct = percent.map(|p| format!(" ({p}%)")).unwrap_or_default();
            Some(format!("Combo finisher: {ft}{pct}"))
        }
        Fact::Number {
            text: Some(t),
            value: Some(v),
            ..
        } => Some(format!("{t}: {v}")),
        Fact::Duration {
            text: Some(t),
            duration: Some(d),
            ..
        } => Some(format!("{t}: {d}s")),
        _ => None,
    }
}

fn format_inspect_entry(
    name: &str,
    description: Option<&str>,
    facts: &[gw2_api::models::facts::Fact],
    traited_n: usize,
) -> String {
    let mut lines = vec![name.to_string()];
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        lines.push(d.to_string());
    }
    let mut fact_lines: Vec<String> = facts.iter().filter_map(fact_line).collect();
    const MAX_FACTS: usize = 10;
    let extra = fact_lines.len().saturating_sub(MAX_FACTS);
    fact_lines.truncate(MAX_FACTS);
    lines.extend(fact_lines);
    if extra > 0 {
        lines.push(format!("(+{extra} more)"));
    }
    if traited_n > 0 {
        lines.push("Some numbers change with traits.".to_string());
    }
    lines.join("\n")
}

fn format_inspect_item(item: &gw2_api::models::Item) -> String {
    let mut lines = vec![item.name.clone()];
    if let Some(d) = item.description.as_deref().filter(|d| !d.is_empty()) {
        lines.push(d.to_string());
    }
    if let Some(details) = &item.details {
        for bonus in &details.bonuses {
            if !bonus.is_empty() {
                lines.push(bonus.clone());
            }
        }
    }
    lines.join("\n")
}

fn find_upgrade_item<'a>(db: &'a GameDb, name: &str) -> Option<&'a gw2_api::models::Item> {
    for id in db.runes.iter().chain(&db.sigils).chain(&db.relics) {
        let Some(item) = db.items.get(id) else {
            continue;
        };
        if item.name.eq_ignore_ascii_case(name) {
            return Some(item);
        }
        let stripped = item
            .name
            .strip_prefix("Superior ")
            .or_else(|| item.name.strip_prefix("Minor "))
            .unwrap_or(&item.name);
        if stripped.eq_ignore_ascii_case(name) {
            return Some(item);
        }
    }
    None
}

fn format_inspect_pet(db: &GameDb, pet: &gw2_api::models::Pet) -> String {
    let title = db.loc_pet(pet.id, &pet.name);
    let mut lines = vec![title.to_string()];
    if let Some(d) = pet.description.as_deref().filter(|d| !d.is_empty()) {
        lines.push(d.to_string());
    }
    let skills: Vec<&str> = pet
        .skills
        .iter()
        .filter_map(|s| db.skills.get(&s.id).map(|sk| db.loc_skill(s.id, &sk.name)))
        .collect();
    if !skills.is_empty() {
        lines.push(format!("Skills: {}", skills.join(" / ")));
    }
    lines.join("\n")
}

fn inspect_one(name: &str, db: &GameDb) -> Option<String> {
    let lookup = name.strip_suffix(" [E]").unwrap_or(name);
    if let Some(pet) = db.pet_by_name(lookup) {
        return Some(format_inspect_pet(db, pet));
    }
    let mut candidates = vec![lookup.to_string()];
    for prefix in ["Superior ", "Minor "] {
        if let Some(rest) = lookup.strip_prefix(prefix) {
            candidates.push(rest.to_string());
        }
    }
    candidates.push(format!("Legendary {lookup} Stance"));

    for c in &candidates {
        if let Some(skill) = db.skills.values().find(|s| {
            s.name.eq_ignore_ascii_case(c) || compact_stance_name(&s.name).eq_ignore_ascii_case(c)
        }) {
            return Some(format_inspect_entry(
                &skill.name,
                skill.description.as_deref(),
                &skill.facts,
                skill.traited_facts.len(),
            ));
        }
    }
    for c in &candidates {
        if let Some(tr) = db.traits.values().find(|t| t.name.eq_ignore_ascii_case(c)) {
            return Some(format_inspect_entry(
                &tr.name,
                tr.description.as_deref(),
                &tr.facts,
                tr.traited_facts.len(),
            ));
        }
    }
    for c in &candidates {
        if let Some(item) = find_upgrade_item(db, c) {
            return Some(format_inspect_item(item));
        }
    }
    None
}

pub(crate) fn inspect_text(name: &str, db: &GameDb) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name == "\u{2014}" || name == "-" || name == "(none)" || name == "(empty)"
    {
        return None;
    }
    if name.contains(" / ") {
        let tips: Vec<String> = name
            .split(" / ")
            .filter_map(|part| inspect_one(part.trim(), db))
            .collect();
        if !tips.is_empty() {
            return Some(tips.join("\n\n"));
        }
    }
    inspect_one(name, db)
}

/// State for the comparison view.
#[derive(Default)]
pub struct ComparisonState {
    pub suggestions: Vec<BuildSuggestion>,
    pub selected_suggestion: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Combat metrics for the current build under each profile.
    pub current_combat_solo: Option<CombatMetrics>,
    pub current_combat_party: Option<CombatMetrics>,
    pub current_combat_squad: Option<CombatMetrics>,
    /// Which result section is visible (one at a time — density / no clip).
    pub result_pane: ResultPane,
    /// Build tab shows Optimized when a suggestion exists. Default true.
    pub show_optimized: bool,
    /// Elite spec the last optimisation run was locked to, from the lock
    /// snapshot taken at its start. The Improve pill reads this, not the live
    /// locks, which `auto_populate_locks` refills after every run.
    pub run_locked_spec: Option<String>,
}

/// A link to the site a published build came from, named after that site.
///
/// Only for builds this addon did not cook. It sits with the build rather
/// than on the card that opened it, because this is where someone is when
/// they decide they want to read the author's own write-up — which is the
/// one thing worth going to a website for, since we show the build itself.
pub(crate) fn render_source_link(ui: &Ui, suggestion: &BuildSuggestion) {
    if suggestion.source_url.is_empty() {
        return;
    }
    ui.same_line_with_spacing(0.0, 12.0);
    let site = site_name(&suggestion.source_url);
    // The site's own mark, so the button says where it goes before it is
    // clicked. Sized to the text beside it, so it follows the font scale
    // instead of being a pixel count that drifts out of line.
    let at = ui.cursor_screen_pos();
    let mark = ui.text_line_height();
    let clicked = ui.small_button(format!("  {}", tf("fmt.site_link", &[("site", &site)])));
    let h = ui.item_rect_size()[1];
    let mid = [at[0] + 5.0 + mark * 0.5, at[1] + h * 0.5];
    let dl = ui.get_window_draw_list();
    match crate::ui::theme::site_tex(&site) {
        Some(tid) => {
            let r = mark * 0.5;
            dl.add_image(tid, [mid[0] - r, mid[1] - r], [mid[0] + r, mid[1] + r])
                .build();
        }
        // No mark bundled: a coloured pip still tells the buttons apart.
        None => {
            dl.add_circle(mid, 3.0, site_colour(&site))
                .filled(true)
                .build();
        }
    }
    if clicked {
        let _ = crate::feedback::shell::open_url(&suggestion.source_url);
    }
}

/// Each community site's own brand colour, for telling their links and
/// tabs apart: GuildJen's pink mark, Hardstuck's red, Snowcrows' sky blue.
/// The tab tint adapts these to the active theme (`theme::tab_tint`), so
/// they stay legible on a dark or a light background.
///
/// Anything unrecognised gets the theme's gold, so a fourth site added later
/// looks deliberate rather than broken.
fn site_colour(site: &str) -> [f32; 4] {
    match site.to_lowercase().as_str() {
        "guildjen" => [0.95, 0.42, 0.72, 1.0],
        "hardstuck" => [0.90, 0.30, 0.28, 1.0],
        "snowcrows" => [0.35, 0.82, 0.86, 1.0],
        _ => crate::ui::theme::pal().gold,
    }
}

/// The site's own name, out of its URL: `https://guildjen.com/x` -> `GuildJen`.
///
/// Read from the URL rather than stored, so a build that names its source in
/// one place cannot disagree with itself in another.
fn site_name(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.");
    let word = host.split('.').next().unwrap_or(host);
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Whose build a comparison tab holds. Derived, never stored: a suggestion
/// with a `source_url` was published somewhere; one without is ours.
#[derive(Debug, Clone, PartialEq)]
pub enum TabKind {
    Current,
    Optimized,
    Published(String),
}

pub fn tab_kind(suggestion: &BuildSuggestion) -> TabKind {
    if suggestion.source_url.is_empty() {
        TabKind::Optimized
    } else {
        TabKind::Published(site_name(&suggestion.source_url))
    }
}

pub fn tab_kind_colour(kind: &TabKind) -> [f32; 4] {
    match kind {
        TabKind::Current => crate::ui::theme::CURRENT,
        TabKind::Optimized => crate::ui::theme::OPTIMIZED,
        TabKind::Published(site) => site_colour(site),
    }
}

/// The tab's text: a published build carries its site's name first.
pub fn tab_label(suggestion: &BuildSuggestion, i: usize) -> String {
    let base = if suggestion.label.is_empty() {
        tf("fmt.build_n", &[("n", &(i + 1).to_string())])
    } else if suggestion.label.starts_with("Score:") {
        tf(
            "fmt.option_n",
            &[
                ("n", &(i + 1).to_string()),
                ("prefix", &suggestion.stat_prefix),
            ],
        )
    } else {
        suggestion.label.clone()
    };
    match tab_kind(suggestion) {
        TabKind::Published(site) => format!("{site} \u{00b7} {base}"),
        _ => base,
    }
}

/// Which build the top Chat strip is copying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSource {
    Character,
    Optimized,
}

impl ChatSource {
    pub fn label(self) -> String {
        t(self.i18n_key())
    }

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Character => "label.character",
            Self::Optimized => "label.optimized",
        }
    }

    pub fn color(self) -> [f32; 4] {
        match self {
            Self::Character => crate::ui::theme::CURRENT,
            Self::Optimized => crate::ui::theme::OPTIMIZED,
        }
    }
}

impl ComparisonState {
    /// Add a build as a tab beside the others, replacing one with the same
    /// label. Loading or opening a build never clears the strip (specs/006
    /// FR-004).
    pub fn push_or_replace(&mut self, suggestion: BuildSuggestion) {
        match self
            .suggestions
            .iter()
            .position(|s| s.label == suggestion.label)
        {
            Some(at) => {
                self.suggestions[at] = suggestion;
                self.selected_suggestion = at;
            }
            None => {
                self.suggestions.push(suggestion);
                self.selected_suggestion = self.suggestions.len() - 1;
            }
        }
        self.show_optimized = true;
    }

    /// Chat code for the Current / Optimized focus. One strip at the top follows this.
    pub fn chat_focus(&self, current_code: Option<&str>) -> (ChatSource, Option<String>) {
        if self.show_optimized && !self.suggestions.is_empty() {
            let idx = self.selected_suggestion.min(self.suggestions.len() - 1);
            (
                ChatSource::Optimized,
                self.suggestions[idx].chat_code.clone(),
            )
        } else {
            (ChatSource::Character, current_code.map(str::to_string))
        }
    }
}

/// One tab per build, tinted by whose it is, so opening a published build
/// never hides the optimized one behind a twin (specs/006 US2). The Current
/// tab is the equipped build; it exists whenever there is one. Shared by
/// the New Build and Improve panes, which used to draw their own strips.
/// `currency` is `state.config.cost_currency`: this runs under the STATE lock.
pub fn render_tab_strip(
    ui: &Ui,
    comparison: &mut ComparisonState,
    has_current: bool,
    currency: gw2_core::config::CostCurrency,
) {
    let tab_count = comparison.suggestions.len();
    // The run behind the selected tab, else the newest run on the strip.
    let record = comparison
        .suggestions
        .get(comparison.selected_suggestion)
        .filter(|_| comparison.show_optimized)
        .and_then(|s| s.generation.clone())
        .or_else(|| {
            comparison
                .suggestions
                .iter()
                .rev()
                .find_map(|s| s.generation.clone())
        });
    let x0 = ui.cursor_pos()[0];
    let avail = ui.content_region_avail()[0];
    if !(has_current || tab_count > 1) {
        if let Some(record) = record {
            crate::ui::run_feed::render_generation_pill(ui, &record, currency, x0, avail, 0.0);
            crate::ui::run_feed::render_log(ui, &record.steps, "run_log");
            ui.separator();
        }
        return;
    }
    {
        let mut row_x = 0.0;
        let mut tabs: Vec<(String, bool, [f32; 4], Option<usize>)> = Vec::new();
        if has_current {
            tabs.push((
                t("cmp.tab_current"),
                !comparison.show_optimized,
                crate::ui::theme::CURRENT,
                None,
            ));
        }
        for (i, suggestion) in comparison.suggestions.iter().enumerate() {
            tabs.push((
                tab_label(suggestion, i),
                comparison.show_optimized && comparison.selected_suggestion == i,
                tab_kind_colour(&tab_kind(suggestion)),
                Some(i),
            ));
        }
        for (n, (label, selected, colour, target)) in tabs.iter().enumerate() {
            let pill_w = ui.calc_text_size(label)[0] + 20.0;
            if n > 0 {
                if row_x + pill_w + 6.0 > avail {
                    row_x = 0.0;
                } else {
                    ui.same_line_with_spacing(0.0, 6.0);
                }
            }
            if crate::ui::theme::tinted_pill(ui, label, *selected, &format!("##tab_{n}"), *colour) {
                match target {
                    Some(i) => {
                        comparison.selected_suggestion = *i;
                        comparison.show_optimized = true;
                    }
                    None => comparison.show_optimized = false,
                }
            }
            row_x += pill_w + 6.0;
        }
        if let Some(record) = record {
            crate::ui::run_feed::render_generation_pill(ui, &record, currency, x0, avail, row_x);
            crate::ui::run_feed::render_log(ui, &record.steps, "run_log");
        }
        ui.separator();
    }
}

/// Render the comparison view: current build on left, suggestion on right.
/// Mutates suggestion tab + result pane. `db` is used for hover inspect.
pub fn render_comparison(
    ui: &Ui,
    current_build: Option<&ResolvedBuild>,
    current_stats: Option<&StatBlock>,
    comparison: &mut ComparisonState,
    db: Option<&GameDb>,
    currency: gw2_core::config::CostCurrency,
) {
    // No character selected: the plate is shown on its own. The "current"
    // side is an empty build of the plate's profession, so every diff column
    // reads as "new", and the current/optimized toggle has nothing to
    // toggle to.
    let placeholder;
    let (current_build, has_current) = match current_build {
        Some(b) => (b, true),
        None => {
            let profession = comparison
                .suggestions
                .get(comparison.selected_suggestion)
                .zip(db)
                .and_then(|(s, db)| {
                    gw2_optimizer::validation::infer_profession_from_spec_names(
                        db,
                        s.specializations.iter().map(|(n, _)| n.as_str()),
                    )
                })
                .unwrap_or_default();
            placeholder = ResolvedBuild {
                profession,
                ..Default::default()
            };
            comparison.show_optimized = true;
            (&placeholder, false)
        }
    };
    if comparison.loading {
        ui.text(t("cmp.optimizing"));
        ui.text(t("cmp.ai"));
        return;
    }

    if let Some(ref err) = comparison.error {
        ui.text_colored(crate::ui::theme::ERR, tf("setup.error", &[("msg", err)]));
        return;
    }

    if comparison.suggestions.is_empty() {
        ui.text(t("cmp.none"));
        return;
    }

    let tab_count = comparison.suggestions.len();
    render_tab_strip(ui, comparison, has_current, currency);

    let idx = comparison.selected_suggestion.min(tab_count - 1);
    comparison.selected_suggestion = idx;

    render_data_quality_badge(ui, &comparison.suggestions[idx]);
    render_source_link(ui, &comparison.suggestions[idx]);
    render_result_pane_tabs(ui, &mut comparison.result_pane);
    if comparison.result_pane == ResultPane::Build && has_current {
        ui.same_line_with_spacing(0.0, 16.0);
        crate::ui::gear_sheet::render_view_toggle(ui, &mut comparison.show_optimized);
    }
    ui.spacing();

    let pane = comparison.result_pane;
    let suggestion = comparison.suggestions[idx].clone();
    match pane {
        ResultPane::Build => {
            let viewing = comparison.show_optimized;
            if viewing {
                crate::ui::main_view::build_display::render_suggestion_skills(
                    ui,
                    &suggestion,
                    db,
                    Some(current_build),
                );
            } else {
                crate::ui::main_view::build_display::render_build_skills(ui, current_build, db);
            }
            ui.spacing();
            if viewing {
                crate::ui::main_view::lock_panel::render_optimized_specs_panel(
                    ui,
                    db,
                    &suggestion.specializations,
                    &t("section.optimized_specs"),
                    Some(&spec_pairs_from_build(current_build)),
                );
            } else {
                let current_specs = spec_pairs_from_build(current_build);
                crate::ui::main_view::lock_panel::render_optimized_specs_panel(
                    ui,
                    db,
                    &current_specs,
                    &t("section.specs"),
                    None,
                );
            }
            let gain = crate::ui::gear_sheet::combat_gain(
                comparison.current_combat_solo.as_ref(),
                suggestion.combat_solo.as_ref(),
            );
            crate::ui::gear_sheet::render_current_sheet(
                ui,
                current_build,
                Some(&suggestion),
                db,
                viewing,
                gain,
                None,
            );
            let explanation_text = if !suggestion.synergy_explanation.is_empty() {
                &suggestion.synergy_explanation
            } else {
                &suggestion.explanation
            };
            if !explanation_text.is_empty() {
                ui.spacing();
                ui.text_colored(crate::ui::theme::pal().muted, t("note.how_to_play"));
                ui.spacing();
                ui.text_wrapped(explanation_text);
            }
        }
        ResultPane::Stats => {
            render_stats_pane(ui, current_stats, comparison, &suggestion, db);
        }
    }
}

/// Rotation sim stores buff uptime as 0.0–1.0; some saves used 0–100.
fn display_uptime_pct(v: f64) -> f64 {
    if v <= 1.0 {
        v * 100.0
    } else {
        v
    }
}

pub(crate) fn render_stats_pane(
    ui: &Ui,
    current_stats: Option<&StatBlock>,
    comparison: &ComparisonState,
    suggestion: &BuildSuggestion,
    db: Option<&GameDb>,
) {
    ui.text_colored(crate::ui::theme::pal().gold, t("section.attributes"));
    ui.text_colored(crate::ui::theme::pal().muted, t("note.attributes"));
    ui.text_colored(crate::ui::theme::pal().muted, t("tier.solo"));
    ui.spacing();
    render_primary_stats(ui, current_stats, suggestion.estimated_stats.as_ref());
    ui.spacing();
    ui.text_colored(crate::ui::theme::pal().gold, t("section.combat"));
    ui.text_colored(
        crate::ui::theme::pal().muted,
        format_combat_live(suggestion.combat_solo.as_ref()),
    );
    ui.spacing();
    render_defenses(ui, comparison, current_stats, suggestion);

    ui.spacing();
    ui.text_colored(crate::ui::theme::pal().gold, t("section.boons"));
    ui.text_colored(crate::ui::theme::pal().muted, t("note.boons"));
    match suggestion.rotation.as_ref() {
        Some(rotation) => {
            if rotation.has_stability || rotation.stunbreak_count > 0 {
                ui.spacing();
                ui.text_colored(
                    crate::ui::theme::pal().cream,
                    tf(
                        "fmt.stability",
                        &[
                            (
                                "yn",
                                &if rotation.has_stability {
                                    t("label.yes")
                                } else {
                                    t("label.no")
                                },
                            ),
                            ("pct", &format!("{:.0}", rotation.stability_uptime * 100.0)),
                            ("n", &rotation.stunbreak_count.to_string()),
                        ],
                    ),
                );
            }
            if rotation.buff_uptime.is_empty() {
                ui.spacing();
                ui.text_colored(crate::ui::theme::pal().muted, t("note.no_boons"));
            } else {
                ui.spacing();
                ui.text_colored(crate::ui::theme::pal().gold, t("label.uptime"));
                for (name, frac) in rotation.buff_uptime.iter().take(8) {
                    ui.text(format!("  {name}: {:.0}%", display_uptime_pct(*frac)));
                }
            }
        }
        None => {
            ui.spacing();
            ui.text_colored(crate::ui::theme::pal().muted, t("note.no_rotation"));
        }
    }

    ui.spacing();
    ui.text_colored(crate::ui::theme::pal().gold, t("section.conditions"));
    ui.text_colored(crate::ui::theme::pal().muted, t("note.conditions_full"));
    if let Some(rotation) = suggestion.rotation.as_ref() {
        ui.spacing();
        ui.text_colored(
            crate::ui::theme::pal().cream,
            tf(
                "fmt.cleanse",
                &[
                    ("n", &rotation.cleanse_count.to_string()),
                    ("rate", &format!("{:.1}", rotation.cleanse_rate_per_20s)),
                ],
            ),
        );
        if !rotation.condition_uptime.is_empty() {
            ui.spacing();
            ui.text_colored(crate::ui::theme::pal().gold, t("label.stacks"));
            for (name, stacks) in rotation.condition_uptime.iter().take(8) {
                ui.text(format!("  {name}: {stacks:.1}"));
            }
        }
    }

    if let Some(ref rotation) = suggestion.rotation {
        ui.spacing();
        ui.text_colored(crate::ui::theme::pal().gold, t("section.rotation"));
        render_rotation_breakdown(ui, rotation, db);
    }
    if let Some(ref viability) = suggestion.viability {
        render_viability_report(ui, viability);
    }
    render_benchmark_delta(ui, suggestion);
    if !suggestion.changes_made.is_empty() {
        ui.spacing();
        ui.text_colored(crate::ui::theme::pal().gold, t("section.changes"));
        for change in &suggestion.changes_made {
            ui.bullet_text(change);
        }
    }
}

pub(crate) fn spec_pairs_from_build(build: &ResolvedBuild) -> Vec<(String, Vec<String>)> {
    build
        .specializations
        .iter()
        .map(|s| {
            let name = if s.elite {
                format!("{} [E]", s.name)
            } else {
                s.name.clone()
            };
            let traits = s
                .traits_selected
                .iter()
                .filter(|t| t.selected)
                .map(|t| t.name.clone())
                .collect();
            (name, traits)
        })
        .collect()
}

/// Sticky copy strip for a GW2 build-template chat code.
/// Click the line to copy onto the Windows clipboard (GW2 paste reads that).
/// Rim and label follow [ChatSource]: blue = loaded character, green = optimized.
/// One line — lives on the tab row so the left panel keeps that height.
pub fn render_chat_code_copy(
    ui: &Ui,
    source: ChatSource,
    chat_code: Option<&str>,
    id_suffix: &str,
    copied_frames: &mut u32,
) {
    if *copied_frames > 0 {
        *copied_frames = copied_frames.saturating_sub(1);
    }

    let remain = ui.content_region_avail()[0];
    if remain < 96.0 {
        ui.dummy([0.0, 0.0]);
        return;
    }

    let accent = source.color();
    let h = ui.frame_height().max(ui.text_line_height() + 6.0);
    let w = (remain - 4.0).max(80.0);
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(format!("##chat_copy_{}", id_suffix), [w, h]);
    let hovered = ui.is_item_hovered();
    if clicked {
        if let Some(code) = chat_code {
            if crate::clipboard::copy_text(code) {
                *copied_frames = 120;
            }
        }
    }
    if hovered {
        ui.tooltip_text(if chat_code.is_some() {
            t("tip.copy_chat")
        } else {
            t("tip.no_chat")
        });
    }

    let fill = if *copied_frames > 0 {
        match source {
            ChatSource::Character => [0.10, 0.16, 0.26, 0.95],
            ChatSource::Optimized => [0.10, 0.22, 0.12, 0.95],
        }
    } else if hovered {
        match source {
            ChatSource::Character => [0.16, 0.20, 0.28, 0.95],
            ChatSource::Optimized => [0.14, 0.22, 0.14, 0.95],
        }
    } else {
        crate::ui::theme::pal().plate
    };
    let rim = if chat_code.is_some() {
        accent
    } else {
        crate::ui::theme::pal().gold_dim
    };
    let text_col = if chat_code.is_some() {
        crate::ui::theme::pal().cream
    } else {
        crate::ui::theme::pal().muted
    };

    let src = source.label();
    let prefix = if *copied_frames > 0 {
        tf("fmt.chat_copied", &[("source", &src)])
    } else {
        tf("fmt.chat_source", &[("source", &src)])
    };
    let fallback = match source {
        ChatSource::Character => t("chat.load_character"),
        ChatSource::Optimized => t("chat.no_result_code"),
    };
    let code_part = chat_code.unwrap_or(fallback.as_str());

    let pad = 8.0;
    let inner_w = (w - pad * 2.0).max(20.0);
    let prefix_w = ui.calc_text_size(&prefix)[0];
    let gap = 8.0;
    let code_w = (inner_w - prefix_w - gap).max(12.0);
    let shown_code = if *copied_frames > 0 {
        String::new()
    } else {
        truncate_ui_text(ui, code_part, code_w)
    };

    {
        let dl = ui.get_window_draw_list();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], fill)
            .filled(true)
            .rounding(crate::ui::theme::ICON_ROUNDING)
            .build();
        dl.add_rect([p[0], p[1]], [p[0] + w, p[1] + h], rim)
            .rounding(crate::ui::theme::ICON_ROUNDING)
            .build();
        let ty = p[1] + ((h - ui.text_line_height()) * 0.5).round();
        dl.add_text([p[0] + pad, ty], crate::ui::color_u32(accent), &prefix);
        if !shown_code.is_empty() {
            dl.add_text(
                [p[0] + pad + prefix_w + gap, ty],
                crate::ui::color_u32(text_col),
                &shown_code,
            );
        }
    }
}

fn truncate_ui_text(ui: &Ui, text: &str, width: f32) -> String {
    if ui.calc_text_size(text)[0] <= width {
        return text.to_string();
    }
    let ellipsis = "...";
    let budget = (width - ui.calc_text_size(ellipsis)[0]).max(0.0);
    let mut s = String::new();
    for ch in text.chars() {
        let mut next = s.clone();
        next.push(ch);
        if ui.calc_text_size(&next)[0] > budget {
            s.push_str(ellipsis);
            return s;
        }
        s = next;
    }
    s
}

/// Render all 9 primary attributes in a comparison table.
fn render_primary_stats(ui: &Ui, current: Option<&StatBlock>, suggested: Option<&StatBlock>) {
    let cur = current.cloned().unwrap_or_default();
    let sug = suggested.cloned().unwrap_or_default();

    let names = [
        t("stat.power"),
        t("stat.precision"),
        t("stat.ferocity"),
        t("stat.condi_dmg_full"),
        t("stat.expertise"),
        t("stat.concentration"),
        t("stat.toughness"),
        t("stat.vitality"),
        t("stat.heal_power"),
    ];
    let stats = [
        (names[0].as_str(), cur.power, sug.power),
        (names[1].as_str(), cur.precision, sug.precision),
        (names[2].as_str(), cur.ferocity, sug.ferocity),
        (
            names[3].as_str(),
            cur.condition_damage,
            sug.condition_damage,
        ),
        (names[4].as_str(), cur.expertise, sug.expertise),
        (names[5].as_str(), cur.concentration, sug.concentration),
        (names[6].as_str(), cur.toughness, sug.toughness),
        (names[7].as_str(), cur.vitality, sug.vitality),
        (names[8].as_str(), cur.healing_power, sug.healing_power),
    ];

    render_stat_table(ui, "##primary_stats", &stats);
}

/// Render defenses: Health and Armor (static stats that don't change with buff profile).
/// Effective HP and Damage Reduction are shown per-tier in Combat Performance.
fn render_defenses(
    ui: &Ui,
    _comparison: &ComparisonState,
    current_stats: Option<&StatBlock>,
    suggestion: &BuildSuggestion,
) {
    let sug_stats = suggestion.estimated_stats.clone().unwrap_or_default();
    let cur = current_stats.cloned().unwrap_or_default();

    let health = t("stat.health");
    let armor = t("stat.armor");
    let stats = [
        (health.as_str(), cur.health, sug_stats.health),
        (armor.as_str(), cur.armor, sug_stats.armor),
    ];

    ui.columns(4, "##defense_cols", true);
    ui.text_colored(crate::ui::theme::pal().gold, t("table.defense"));
    ui.next_column();
    ui.text_colored(crate::ui::theme::CURRENT, t("label.current"));
    ui.next_column();
    ui.text_colored(crate::ui::theme::OPTIMIZED, t("label.optimized"));
    ui.next_column();
    ui.text(t("table.diff"));
    ui.next_column();
    ui.separator();

    for (name, cur_val, sug_val) in &stats {
        render_int_row(ui, name, *cur_val, *sug_val);
    }
    ui.columns(1, "##end_defense", false);
}

fn render_int_row(ui: &Ui, name: &str, cur: i32, sug: i32) {
    ui.text(name);
    ui.next_column();
    ui.text(format!("{}", cur));
    ui.next_column();
    ui.text(format!("{}", sug));
    ui.next_column();
    let diff = sug - cur;
    let color = diff_color(diff as f64);
    let sign = if diff > 0 { "+" } else { "" };
    ui.text_colored(color, format!("{}{}", sign, diff));
    ui.next_column();
}

fn render_stat_table(ui: &Ui, id: &str, stats: &[(&str, i32, i32)]) {
    ui.columns(4, id, true);

    ui.text_colored(crate::ui::theme::pal().gold, t("table.attribute"));
    ui.next_column();
    ui.text_colored(crate::ui::theme::CURRENT, t("label.current"));
    ui.next_column();
    ui.text_colored(crate::ui::theme::OPTIMIZED, t("label.optimized"));
    ui.next_column();
    ui.text(t("table.diff"));
    ui.next_column();
    ui.separator();

    for (name, cur, sug) in stats {
        render_int_row(ui, name, *cur, *sug);
    }

    ui.columns(1, format!("{}_end", id), false);
}

/// Color for a diff value: green=positive, red=negative, gray=zero.
fn diff_color(diff: f64) -> [f32; 4] {
    if diff > 0.5 {
        [0.0, 1.0, 0.0, 1.0]
    } else if diff < -0.5 {
        [1.0, 0.0, 0.0, 1.0]
    } else {
        [0.7, 0.7, 0.7, 1.0]
    }
}

/// Render rotation simulation breakdown: simulated DPS, condition uptimes, skill usage.
fn render_rotation_breakdown(ui: &Ui, rotation: &RotationBreakdown, db: Option<&GameDb>) {
    ui.text_colored(crate::ui::theme::pal().muted, t("note.rotation_sim"));
    ui.text(tf(
        "fmt.sim_dps",
        &[
            ("dps", &rotation.simulated_dps.to_string()),
            ("strike", &rotation.strike_dps.to_string()),
            ("condi", &rotation.condition_dps.to_string()),
        ],
    ));
    ui.spacing();

    if !rotation.skill_usage.is_empty() {
        ui.text(t("label.skill_usage"));
        for (name, casts, dps) in &rotation.skill_usage {
            if *casts > 0 {
                ui.text(format!("  {} x{} ({} DPS)", name, casts, dps));
                inspect_if_hovered(ui, name, db);
            }
        }
    }
}

// Trust UI helpers

fn render_data_quality_badge(ui: &Ui, suggestion: &BuildSuggestion) {
    use gw2_optimizer::data::DataQuality;
    let (label, col, tooltip_header) = match suggestion.data_quality {
        DataQuality::Verified => (
            format!("* {}", t("settings.verified")),
            [0.3, 0.9, 0.3, 1.0],
            t("quality.verified_tip"),
        ),
        DataQuality::Provisional => (
            format!("* {}", t("settings.provisional")),
            [0.95, 0.75, 0.15, 1.0],
            t("quality.provisional_tip"),
        ),
        DataQuality::Blocked => (
            format!("* {}", t("settings.blocked")),
            [1.0, 0.3, 0.2, 1.0],
            t("quality.blocked_tip"),
        ),
    };

    ui.text_colored(col, &label);
    if ui.is_item_hovered() {
        crate::ui::theme::wide_tooltip(ui, |ui| {
            ui.text(&tooltip_header);
            if !suggestion.quality_reasons.is_empty() {
                ui.spacing();
                ui.text(t("label.reasons"));
                for reason in &suggestion.quality_reasons {
                    ui.bullet_text(reason);
                }
            }
        });
    }
    if let Some(note) = suggestion.coverage_note.as_deref() {
        ui.same_line();
        ui.text_colored(
            crate::ui::theme::pal().muted,
            tf("quality.coverage_line", &[("detail", note)]),
        );
    }
}

/// The amber the viability report already uses for a gate that did not pass.
const OTHER_ROLE_WARNING: [f32; 4] = [1.0, 0.7, 0.2, 1.0];

/// Same amber, for a reference card that did not pass its gates either.
pub(crate) const DEMOTED_PICK: [f32; 4] = OTHER_ROLE_WARNING;

/// The published role out of a reference tab's summary.
///
/// `adopt_pick_tab` writes the site's own role string first and appends the
/// weapons after a middot, so the role is everything before it.
fn published_role(build_summary: &str) -> &str {
    build_summary
        .split(" \u{00b7} ")
        .next()
        .unwrap_or(build_summary)
        .trim()
}

fn render_benchmark_delta(ui: &Ui, suggestion: &BuildSuggestion) {
    // A published reference IS the yardstick. Scoring it against itself
    // printed "no benchmark data" on a tab that is benchmark data.
    if !suggestion.source_url.is_empty() {
        if ui.collapsing_header(t("bench.header"), TreeNodeFlags::DEFAULT_OPEN) {
            ui.text(format!(
                "  {}",
                tf(
                    "bench.is_reference",
                    &[
                        ("site", &title_case(&site_name(&suggestion.source_url))),
                        ("role", published_role(&suggestion.build_summary)),
                    ],
                )
            ));
        }
        return;
    }
    match &suggestion.benchmark_delta {
        None => {
            // Never synced and "synced, but nothing here could be scored"
            // are different problems; only the first is fixed in Settings.
            if ui.collapsing_header(t("bench.header"), TreeNodeFlags::empty()) {
                if suggestion.benchmarks_synced {
                    ui.text_colored(
                        crate::ui::theme::pal().muted,
                        format!("  {}", t("bench.no_scorable")),
                    );
                } else {
                    ui.text_colored(
                        crate::ui::theme::pal().muted,
                        format!("  {}", t("bench.none")),
                    );
                    ui.text_colored(
                        crate::ui::theme::pal().muted,
                        format!("  {}", t("bench.sync_hint")),
                    );
                }
            }
        }
        Some(delta) => {
            let pct = delta.pct_of_ref;
            let (col, status_key): ([f32; 4], &str) = if pct >= 95.0 {
                ([0.3, 0.9, 0.3, 1.0], "bench.on_par")
            } else if pct >= 80.0 {
                ([0.9, 0.8, 0.2, 1.0], "bench.close")
            } else if pct >= 65.0 {
                ([0.9, 0.55, 0.1, 1.0], "bench.below")
            } else {
                ([1.0, 0.3, 0.2, 1.0], "bench.far_below")
            };
            // A reference for a different job scores honestly under the
            // player's weights but is not a like-for-like comparison, so the
            // header drops the on-par/far-below word rather than claiming
            // this build beat a meta build it was never measured against.
            let header = if delta.role_matched {
                tf(
                    "fmt.vs_meta",
                    &[
                        ("src", &title_case(&delta.source)),
                        ("pct", &format!("{:.0}", pct)),
                        ("status", &t(status_key)),
                    ],
                )
            } else {
                tf(
                    "fmt.vs_meta_other_role",
                    &[
                        ("src", &title_case(&delta.source)),
                        ("role", &delta.role),
                        ("pct", &format!("{:.0}", pct)),
                    ],
                )
            };

            if ui.collapsing_header(&header, TreeNodeFlags::DEFAULT_OPEN) {
                if delta.role_matched {
                    ui.text_colored(
                        col,
                        format!(
                            "  {}",
                            tf("fmt.pct_ref", &[("pct", &format!("{:.0}", pct))])
                        ),
                    );
                } else {
                    ui.text_colored(
                        OTHER_ROLE_WARNING,
                        format!("  {}", t("bench.other_role_title")),
                    );
                    ui.text(format!(
                        "  {}",
                        tf(
                            "bench.other_role",
                            &[("role_hint", &delta.requested_role), ("role", &delta.role),],
                        )
                    ));
                }
                // Fine print in the matched case; part of the warning when
                // the reference does a different job.
                ui.text_colored(
                    if delta.role_matched {
                        crate::ui::theme::pal().muted
                    } else {
                        crate::ui::theme::pal().cream
                    },
                    format!(
                        "  {}",
                        tf(
                            // The role is the whole point of the number, so
                            // name it when we have one.
                            if delta.requested_role.trim().is_empty() {
                                "bench.basis"
                            } else {
                                "bench.basis_role"
                            },
                            &[
                                ("role_hint", &delta.requested_role),
                                ("ref", &format!("{:.2}", delta.ref_score)),
                                ("ours", &format!("{:.2}", delta.our_score)),
                            ],
                        )
                    ),
                );
                // Both scores are measured output whether or not the gates
                // passed, so say which side was refused instead of dropping
                // the comparison. A published page is written for its own
                // scale, so a failed reference gate is normal.
                if !delta.ref_viable || !delta.our_viable {
                    let word = |ok: bool| {
                        if ok {
                            t("bench.gate_passed")
                        } else {
                            t("bench.gate_failed")
                        }
                    };
                    ui.text_colored(
                        crate::ui::theme::pal().muted,
                        format!(
                            "  {}",
                            tf(
                                "bench.gate_caveat",
                                &[
                                    ("ref", &word(delta.ref_viable)),
                                    ("ours", &word(delta.our_viable)),
                                ],
                            )
                        ),
                    );
                }
                ui.spacing();

                // Score bar
                let bar_col = if delta.role_matched {
                    col
                } else {
                    OTHER_ROLE_WARNING
                };
                let bar_width = ui.content_region_avail()[0] - 16.0;
                let filled = (bar_width * (pct / 100.0).min(1.0) as f32).max(0.0);
                let pos = ui.cursor_screen_pos();
                let draw = ui.get_window_draw_list();
                // Background
                draw.add_rect(
                    [pos[0] + 8.0, pos[1] + 2.0],
                    [pos[0] + bar_width + 8.0, pos[1] + 14.0],
                    [0.2, 0.2, 0.2, 0.8],
                )
                .filled(true)
                .build();
                // Fill
                if filled > 0.0 {
                    draw.add_rect(
                        [pos[0] + 8.0, pos[1] + 2.0],
                        [pos[0] + 8.0 + filled, pos[1] + 14.0],
                        bar_col,
                    )
                    .filled(true)
                    .build();
                }
                ui.dummy([0.0, 18.0]);

                ui.spacing();
                ui.text(format!(
                    "  {}",
                    tf(
                        "fmt.reference",
                        &[
                            ("prof", &delta.profession),
                            ("role", &delta.role),
                            ("gear", &delta.ref_gear_prefix),
                            ("src", &delta.source),
                        ],
                    )
                ));
                if !delta.ref_url.is_empty() {
                    ui.text_colored([0.4, 0.6, 0.9, 1.0], format!("  {}", delta.ref_url));
                }
            }
        }
    }
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

/// Live combat dump, or the not-computed note when metrics were never produced.
fn format_combat_live(metrics: Option<&CombatMetrics>) -> String {
    match metrics {
        None => t("note.not_computed"),
        Some(m) => format!(
            "{}/{}/{}",
            m.total_dps_index, m.healing_index, m.effective_health
        ),
    }
}

pub(crate) fn viability_gate_label(gate: &gw2_optimizer::ViabilityGate) -> &'static str {
    use gw2_optimizer::ViabilityGate::*;
    match gate {
        StunbreakCount => "Stunbreaks",
        StabilityAccess => "Stability",
        CleanseRate => "Cleanse",
        ControlCoverage => "Control coverage",
        EffectiveHealth => "Effective health",
        MobilityOut => "Disengage",
        HarasserStrip => "Boon strip",
        BoonUptime => "Boon uptime",
        EncounterOutcome => "Encounter",
        SecureCompletion => "Secure",
        ProtectedExecution => "Protected execution",
        SustainRecovery => "Sustain",
        ResourceLegality => "Resources",
    }
}

/// Render the viability gate breakdown: pass/fail per gate with notes.
fn render_viability_report(ui: &Ui, report: &ViabilityReport) {
    let header_col = if report.is_viable {
        [0.3, 0.9, 0.3, 1.0]
    } else {
        [1.0, 0.35, 0.2, 1.0]
    };
    let status = if report.is_viable {
        t("viable.yes")
    } else {
        t("viable.no")
    };

    if ui.collapsing_header(
        tf("fmt.viability", &[("status", &status)]),
        if report.is_viable {
            TreeNodeFlags::empty()
        } else {
            TreeNodeFlags::DEFAULT_OPEN
        },
    ) {
        ui.text_colored(
            header_col,
            format!("  {}", tf("fmt.viable_status", &[("status", &status)])),
        );
        ui.spacing();
        for gate in &report.gates {
            // Overlay fonts have no colour emoji; ✅/❌ become "?".
            //
            // A skipped gate reports `passed` so that every "list the
            // failures" caller stays correct, and it was being drawn as a
            // green OK - a claim we never checked, shown as a pass. It is
            // its own muted row.
            let (icon, col): (&str, [f32; 4]) = if gate.skipped {
                ("--", crate::ui::theme::pal().muted)
            } else if gate.passed {
                ("OK", [0.4, 0.9, 0.4, 1.0])
            } else {
                ("NO", [1.0, 0.3, 0.2, 1.0])
            };
            let gate_name = viability_gate_label(&gate.gate);
            ui.text_colored(col, format!("  {} {} -- {}", icon, gate_name, gate.note));
        }
        if !report.is_viable {
            ui.spacing();
            ui.text_colored([1.0, 0.7, 0.2, 1.0], format!("  {}", t("note.nonviable")));
        }
    }
}

/// Legacy synthetic tradeoff report retained for internal diagnostics only.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_suggestion_default() {
        let s = BuildSuggestion::default();
        assert!(s.label.is_empty());
        assert!(s.specializations.is_empty());
        assert!(s.weapons.is_empty());
    }

    #[test]
    fn build_suggestion_can_carry_copyable_chat_code() {
        let s = BuildSuggestion {
            chat_code: Some("[&DQIEAAA=]".to_string()),
            ..Default::default()
        };

        assert_eq!(s.chat_code.as_deref(), Some("[&DQIEAAA=]"));
    }

    #[test]
    fn chat_code_follows_current_vs_optimized() {
        let mut c = ComparisonState::default();
        let (src, code) = c.chat_focus(Some("[&CUR]"));
        assert_eq!(src, ChatSource::Character);
        assert_eq!(code.as_deref(), Some("[&CUR]"));

        c.show_optimized = true;
        c.suggestions.push(BuildSuggestion {
            chat_code: Some("[&OPT]".into()),
            ..Default::default()
        });
        let (src, code) = c.chat_focus(Some("[&CUR]"));
        assert_eq!(src, ChatSource::Optimized);
        assert_eq!(code.as_deref(), Some("[&OPT]"));

        c.suggestions[0].chat_code = None;
        let (src, code) = c.chat_focus(Some("[&CUR]"));
        assert_eq!(src, ChatSource::Optimized);
        assert_eq!(code, None);

        c.show_optimized = false;
        let (src, code) = c.chat_focus(Some("[&CUR]"));
        assert_eq!(src, ChatSource::Character);
        assert_eq!(code.as_deref(), Some("[&CUR]"));
    }

    #[test]
    fn test_diff_color() {
        let green = diff_color(100.0);
        assert_eq!(green, [0.0, 1.0, 0.0, 1.0]);
        let red = diff_color(-50.0);
        assert_eq!(red, [1.0, 0.0, 0.0, 1.0]);
        let gray = diff_color(0.0);
        assert_eq!(gray, [0.7, 0.7, 0.7, 1.0]);
    }

    fn inspect_db_with_skill() -> GameDb {
        let mut db = gw2_optimizer::gamedb::GameDb::empty_for_tests();
        let skill: gw2_api::models::Skill = serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "Legendary Assassin Stance",
            "description": "Swap to this legend.",
            "facts": [
                {"type": "Recharge", "value": 10.0},
                {"type": "Range", "value": 900}
            ]
        }))
        .expect("skill fixture");
        db.skills.insert(1, skill);
        let item: gw2_api::models::Item = serde_json::from_value(serde_json::json!({
            "id": 2,
            "name": "Superior Rune of the Scholar",
            "type": "UpgradeComponent",
            "rarity": "Exotic",
            "level": 60,
            "details": {
                "bonuses": ["+25 Power", "+5% Strike Damage"]
            }
        }))
        .expect("item fixture");
        db.items.insert(2, item);
        db.runes.push(2);
        db
    }

    #[test]
    fn inspect_text_includes_facts_and_compact_stance() {
        let db = inspect_db_with_skill();
        let tip = inspect_text("Assassin", &db).expect("stance lookup");
        assert!(tip.contains("Legendary Assassin Stance"), "{tip}");
        assert!(tip.contains("Recharge: 10"), "{tip}");
        assert!(tip.contains("Range: 900"), "{tip}");
    }

    #[test]
    fn inspect_text_finds_rune_bonuses() {
        let db = inspect_db_with_skill();
        let tip = inspect_text("Rune of the Scholar", &db).expect("rune lookup");
        assert!(tip.contains("Superior Rune of the Scholar"), "{tip}");
        assert!(tip.contains("+5% Strike Damage"), "{tip}");
    }

    #[test]
    fn compact_pet_name_strips_juvenile() {
        assert_eq!(compact_pet_name("Juvenile Smokescale"), "Smokescale");
        assert_eq!(compact_pet_name("#66"), "#66");
    }

    #[test]
    fn inspect_text_finds_pet_by_compact_or_hash_id() {
        let mut db = inspect_db_with_skill();
        db.pets.insert(
            66,
            gw2_api::models::Pet {
                id: 66,
                name: "Juvenile Smokescale".into(),
                description: Some("Breathes smoke.".into()),
                icon: None,
                skills: vec![gw2_api::models::PetSkill { id: 1 }],
            },
        );
        let tip = inspect_text("Smokescale", &db).expect("compact name");
        assert!(tip.contains("Juvenile Smokescale"), "{tip}");
        assert!(tip.contains("Breathes smoke."), "{tip}");
        assert!(tip.contains("Legendary Assassin Stance"), "{tip}");
        let by_id = inspect_text("#66", &db).expect("hash id");
        assert!(by_id.contains("Juvenile Smokescale"), "{by_id}");
    }

    #[test]
    fn fact_line_preserves_tooltip_effect_label() {
        let fact = gw2_api::models::facts::Fact::AttributeAdjust {
            text: Some("Life Siphon Damage".into()),
            icon: None,
            value: Some(3517),
            target: Some("Power".into()),
        };

        assert_eq!(
            fact_line(&fact).as_deref(),
            Some("Life Siphon Damage: 3517")
        );
    }

    #[test]
    fn missing_combat_is_not_computed_not_zeros() {
        let s = BuildSuggestion::default();
        assert!(s.combat_solo.is_none());
        let shown = format_combat_live(s.combat_solo.as_ref());
        assert_eq!(shown, t("note.not_computed"));
        assert_ne!(
            shown,
            format_combat_live(Some(&CombatMetrics::default())),
            "None must not format as a live 0/0/0 dump"
        );
        assert!(
            !shown.chars().any(|c| c.is_ascii_digit()),
            "not-computed note must not look like live stats: {shown}"
        );
    }

    #[test]
    fn viability_gate_label_is_not_debug() {
        use gw2_optimizer::ViabilityGate::*;
        for gate in [
            StunbreakCount,
            StabilityAccess,
            CleanseRate,
            ControlCoverage,
            EffectiveHealth,
            MobilityOut,
            HarasserStrip,
            EncounterOutcome,
            SecureCompletion,
            ProtectedExecution,
            SustainRecovery,
            ResourceLegality,
        ] {
            let shown = viability_gate_label(&gate);
            assert!(
                !shown.contains("ViabilityGate"),
                "gate label must not dump the enum type: {shown}"
            );
            assert_ne!(
                shown,
                format!("{gate:?}"),
                "gate label must not be Rust Debug"
            );
        }

        let src = include_str!("comparison.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("comparison.rs must contain its own #[cfg(test)] marker");
        assert!(
            !production.contains("format!(\"{:?}\", gate.gate)"),
            "render_viability_report must not Debug-dump gate names"
        );
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    fn published(label: &str, url: &str) -> BuildSuggestion {
        BuildSuggestion {
            label: label.into(),
            source_url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tab_kind_from_suggestion() {
        assert_eq!(tab_kind(&published("Reaper", "")), TabKind::Optimized);
        assert_eq!(
            tab_kind(&published("Reaper", "https://guildjen.com/reaper/")),
            TabKind::Published("Guildjen".into())
        );
        assert_ne!(
            tab_kind_colour(&TabKind::Current),
            tab_kind_colour(&TabKind::Optimized)
        );
        assert_eq!(
            tab_kind_colour(&TabKind::Published("Guildjen".into())),
            site_colour("guildjen")
        );
    }

    #[test]
    fn published_label_is_site_and_build() {
        let s = published("Reaper", "https://www.hardstuck.gg/gw2/builds/x");
        assert_eq!(tab_label(&s, 0), "Hardstuck \u{00b7} Reaper");
        assert_eq!(tab_label(&published("Reaper", ""), 0), "Reaper");
        assert_eq!(
            tab_label(&published("", ""), 2),
            tf("fmt.build_n", &[("n", "3")])
        );
    }

    #[test]
    fn loaded_suggestion_pushes_not_replaces() {
        let mut c = ComparisonState {
            suggestions: vec![published("A", ""), published("B", "")],
            ..Default::default()
        };
        c.push_or_replace(published("C", ""));
        assert_eq!(c.suggestions.len(), 3);
        assert_eq!(c.selected_suggestion, 2);
        c.push_or_replace(published("B", "https://guildjen.com/b"));
        assert_eq!(c.suggestions.len(), 3);
        assert_eq!(c.selected_suggestion, 1);
        assert!(!c.suggestions[1].source_url.is_empty());
        assert!(c.show_optimized);
    }
}
