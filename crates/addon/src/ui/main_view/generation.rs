//! One run's live step feed and its generation record.
//!
//! A [`RunTracker`] lives on the run's worker thread. Every step it records
//! is applied to its own feed and, through `with_state`, to the overlay's
//! copy (`MainState::run_feed`) so the player sees each step as it starts and
//! as it ends. It also owns the run's [`UsageScope`], so every LLM request the
//! run makes is counted, and at the end it writes the [`GenerationRecord`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use gw2_core::config::LlmProvider;
use gw2_core::generations::{
    steps_for_record, ExplainPart, GenerationCard, GenerationKind, GenerationLog, GenerationRecord,
    GenerationStatus, GenerationTier, LlmUsed, RunFeed, RunPhase, StepState, TierTiming,
    GENERATIONS_FILE,
};
use gw2_core::i18n::{t, tf};
use gw2_core::types::GameMode;
use gw2_optimizer::llm::usage::{LlmEvent, ObserverScope, UsageScope, WaitReason};
use gw2_optimizer::scenario::{CombatTier, RoleObjective};
use gw2_optimizer::scoring::OptimizationWeights;

use crate::state::AddonState;
use crate::ui::comparison::BuildSuggestion;
use crate::ui::run_feed::{clip, format_tokens, LiveFeed};

static NEXT_RUN: AtomicU64 = AtomicU64::new(1);

/// What the run was asked for, captured on the render thread at the click.
#[derive(Debug, Clone)]
pub(super) struct RunMeta {
    pub kind: GenerationKind,
    pub character_name: String,
    pub profession: String,
    pub mode: GameMode,
    /// The left panel's scale (only WvW shows it).
    pub tier: CombatTier,
    pub role: Option<RoleObjective>,
    pub weights: OptimizationWeights,
    pub provider: LlmProvider,
    pub model: String,
    pub addon_dir: std::path::PathBuf,
}

impl RunMeta {
    pub(super) fn capture(state: &AddonState, kind: GenerationKind, profession: &str) -> Self {
        let character_name = state
            .main
            .current_build
            .as_ref()
            .map(|b| b.character_name.clone())
            .or_else(|| {
                state
                    .main
                    .selected_character
                    .and_then(|i| state.main.characters.get(i).cloned())
            })
            .unwrap_or_default();
        Self {
            kind,
            character_name,
            profession: profession.to_string(),
            mode: state.main.game_mode.clone(),
            tier: state.main.combat_tier,
            role: state.main.selected_role,
            weights: state.main.weights.clone(),
            provider: state.config.active_provider.clone(),
            model: state.config.active_model_id().to_string(),
            addon_dir: state.addon_dir.clone(),
        }
    }

    fn kind_label(&self) -> String {
        t(match self.kind {
            GenerationKind::NewBuild => "gen.kind_new",
            GenerationKind::Improve => "gen.kind_improve",
            GenerationKind::Choya => "gen.kind_choya",
        })
    }

    fn scenario_line(&self) -> String {
        scenario_line(&self.mode, Some(self.tier), self.role, "", "")
    }
}

/// A unit variant's serde name ("Solo", "PowerDps"): the stable id a
/// generation record stores for the scale and the role.
pub(super) fn variant_id<T: serde::Serialize>(value: &T) -> Option<String> {
    serde_json::to_value(value)
        .ok()?
        .as_str()
        .map(str::to_string)
}

/// The variant [`variant_id`] named; `None` for an unknown id.
pub(super) fn from_variant_id<T: serde::de::DeserializeOwned>(id: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(id.to_string())).ok()
}

/// "WvW · Roam · Damage", in the current language. Scale only in WvW, the
/// one mode whose left panel offers it. `scale_text` / `role_text` are the
/// labels an old record stored instead of ids.
pub(super) fn scenario_line(
    mode: &GameMode,
    tier: Option<CombatTier>,
    role: Option<RoleObjective>,
    scale_text: &str,
    role_text: &str,
) -> String {
    let scale = match tier {
        Some(tier) if *mode == GameMode::WvW => t(super::scale_i18n_key(mode, tier)),
        Some(_) => String::new(),
        None => scale_text.to_string(),
    };
    let role = role
        .map(|r| t(super::role_i18n_key(mode, r)))
        .unwrap_or_else(|| role_text.to_string());
    [mode.label().to_string(), scale, role]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{00b7} ")
}

/// A record's scenario line: from its ids, else from the text it stored.
pub(super) fn record_scenario_line(record: &GenerationRecord) -> String {
    scenario_line(
        &record.mode,
        record.scale_id.as_deref().and_then(from_variant_id),
        record.role_id.as_deref().and_then(from_variant_id),
        &record.scale,
        &record.role,
    )
}

/// A step's hover explanation as catalog keys; the feed resolves them when
/// drawn, so a stored run reads in the language shown now.
pub(super) struct Explain(Vec<ExplainPart>);

impl From<&str> for Explain {
    fn from(key: &str) -> Self {
        Self(vec![ExplainPart::new(key, &[])])
    }
}

impl From<ExplainPart> for Explain {
    fn from(part: ExplainPart) -> Self {
        Self(vec![part])
    }
}

impl From<Vec<ExplainPart>> for Explain {
    fn from(parts: Vec<ExplainPart>) -> Self {
        Self(parts)
    }
}

/// The build a run served, and which tier served it.
pub(super) struct Served<'a> {
    pub suggestion: &'a BuildSuggestion,
    pub tier: GenerationTier,
    pub db: Option<&'a gw2_optimizer::gamedb::GameDb>,
}

/// The run's clock, feed and usage. Worker-thread only (`Rc`).
pub(super) struct RunTracker {
    id: u64,
    started: Instant,
    started_at: DateTime<Utc>,
    meta: RunMeta,
    usage: UsageScope,
    feed: RefCell<RunFeed>,
    /// The engine's current progress stage: step id and its grouping key.
    stage: RefCell<Option<(u32, String)>>,
    llm_step: Cell<Option<u32>>,
    wait_step: Cell<Option<u32>>,
    tier_open: Cell<Option<(GenerationTier, Instant, u32)>>,
    tiers: RefCell<Vec<TierTiming>>,
    /// Set once the final step is written; nothing is appended after it.
    closed: Cell<bool>,
}

impl RunTracker {
    /// Start a run on the current (worker) thread and publish an empty feed.
    pub(super) fn start(meta: RunMeta) -> Rc<Self> {
        let id = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        crate::state::with_state(|s| {
            s.main.run_feed = LiveFeed {
                run_id: id,
                started: Some(started),
                feed: RunFeed::default(),
                live: true,
            };
        });
        let tracker = Rc::new(Self {
            id,
            started,
            started_at: Utc::now(),
            meta,
            usage: UsageScope::new(),
            feed: RefCell::new(RunFeed::default()),
            stage: RefCell::new(None),
            llm_step: Cell::new(None),
            wait_step: Cell::new(None),
            tier_open: Cell::new(None),
            tiers: RefCell::new(Vec::new()),
            closed: Cell::new(false),
        });
        let who = if tracker.meta.character_name.is_empty() {
            tracker.meta.profession.clone()
        } else {
            tracker.meta.character_name.clone()
        };
        let first = tracker.note(
            RunPhase::Run,
            tf(
                "run.started",
                &[("kind", &tracker.meta.kind_label()), ("who", &who)],
            ),
            Some(format!(
                "{} \u{00b7} {}",
                tracker.meta.scenario_line(),
                tracker.meta.weights.summary_label()
            )),
        );
        tracker.explain(first, "explain.run_started");
        tracker
    }

    /// Attach the hover explanation to a step.
    pub(super) fn explain(&self, id: u32, explain: impl Into<Explain>) {
        let Explain(parts) = explain.into();
        self.apply(|f| {
            if let Some(step) = f.get_mut(id) {
                step.explain_keys = parts.clone();
            }
        });
    }

    /// Attach the hover explanation to the open stage step.
    pub(super) fn explain_stage(&self, explain: impl Into<Explain>) {
        let open = self.stage.borrow().as_ref().map(|(id, _)| *id);
        if let Some(id) = open {
            self.explain(id, explain);
        }
    }

    pub(super) fn ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Apply one edit to this run's feed and to the overlay's copy of it.
    /// Both start empty and see the same edits, so step ids agree.
    fn apply<R>(&self, op: impl Fn(&mut RunFeed) -> R) -> R {
        let out = op(&mut self.feed.borrow_mut());
        let id = self.id;
        crate::state::with_state(|s| {
            if s.main.run_feed.run_id == id {
                op(&mut s.main.run_feed.feed);
            }
        });
        out
    }

    /// A step that is now running; returns its id.
    pub(super) fn begin(
        &self,
        phase: RunPhase,
        label: impl Into<String>,
        detail: Option<String>,
    ) -> u32 {
        let at = self.ms();
        let label = label.into();
        self.apply(|f| f.push(at, phase, label.clone(), detail.clone(), StepState::Running))
    }

    /// A step that happened at once; returns its id.
    pub(super) fn note(
        &self,
        phase: RunPhase,
        label: impl Into<String>,
        detail: Option<String>,
    ) -> u32 {
        let at = self.ms();
        let label = label.into();
        self.apply(|f| {
            f.push(
                at,
                phase,
                label.clone(),
                detail.clone(),
                StepState::Done { took_ms: 0 },
            )
        })
    }

    pub(super) fn done(&self, id: u32) {
        let at = self.ms();
        self.apply(|f| f.finish(id, at));
    }

    pub(super) fn done_with(&self, id: u32, detail: impl Into<String>) {
        let at = self.ms();
        let detail = detail.into();
        self.apply(|f| {
            f.finish(id, at);
            if let Some(step) = f.get_mut(id) {
                step.detail = Some(detail.clone());
            }
        });
    }

    pub(super) fn fail(&self, id: u32, message: impl Into<String>) {
        let message = clip(&message.into(), 300);
        self.apply(|f| f.fail(id, message.clone()));
    }

    /// One engine progress line. Repeats of the same stage ("gen 3", "gen 4")
    /// update the running step in place; a new stage closes the old one.
    /// `search_v2` receipts are recorded as finished diagnostic lines.
    pub(super) fn stage(&self, phase: RunPhase, text: &str) {
        if text == "Done" {
            self.end_stage();
            return;
        }
        if text.starts_with("search_v2") {
            let (label, detail) = text.split_once(':').unwrap_or((text, ""));
            let id = self.note(phase, label.trim(), Some(clip(detail.trim(), 240)));
            if let Some(explain) = engine_explain(text) {
                self.explain(id, explain);
            }
            return;
        }
        let key = stage_key(text).to_string();
        let open = self.stage.borrow().clone();
        if let Some((id, open_key)) = open {
            if open_key == key {
                let text = text.to_string();
                self.apply(|f| {
                    if let Some(step) = f.get_mut(id) {
                        step.label = text.clone();
                    }
                });
                return;
            }
        }
        self.end_stage();
        let id = self.begin(phase, text, None);
        if let Some(explain) = engine_explain(text) {
            self.explain(id, explain);
        }
        *self.stage.borrow_mut() = Some((id, key));
    }

    pub(super) fn end_stage(&self) {
        if let Some((id, _)) = self.stage.borrow_mut().take() {
            self.done(id);
        }
    }

    pub(super) fn tier_begin(&self, tier: GenerationTier) {
        let (phase, key) = tier_phase(tier);
        let id = self.begin(phase, t(key), None);
        self.explain(id, tier_explain(tier));
        self.tier_open.set(Some((tier, Instant::now(), id)));
    }

    /// Close the open tier. `Err` records the failure and the fallback.
    pub(super) fn tier_end(&self, outcome: Result<(), &str>) {
        self.end_stage();
        let Some((tier, since, id)) = self.tier_open.take() else {
            return;
        };
        self.tiers.borrow_mut().push(TierTiming {
            tier,
            duration_ms: since.elapsed().as_millis() as u64,
            served: outcome.is_ok(),
        });
        match outcome {
            Ok(()) => self.done(id),
            Err(why) => {
                self.fail(id, why);
                if tier != GenerationTier::Legacy && tier != GenerationTier::Choya {
                    let note = self.note(
                        tier_phase(tier).0,
                        tf("run.fallback", &[("tier", &t(tier_phase(tier).1))]),
                        None,
                    );
                    self.explain(note, "explain.fallback");
                }
            }
        }
    }

    /// Route this thread's LLM events into the feed until the guard drops.
    pub(super) fn observe(self: &Rc<Self>) -> ObserverScope {
        let me = Rc::clone(self);
        ObserverScope::new(move |event| me.on_llm(event))
    }

    fn model_name(&self) -> String {
        crate::ui::run_feed::model_label(&LlmUsed {
            provider: self.meta.provider.clone(),
            model: self.meta.model.clone(),
        })
    }

    fn on_llm(&self, event: &LlmEvent) {
        match *event {
            LlmEvent::RequestStarted { n } => {
                let purpose = self.stage.borrow().as_ref().and_then(|(id, _)| {
                    self.feed
                        .borrow()
                        .steps
                        .iter()
                        .rev()
                        .find(|s| s.id == *id)
                        .map(|s| s.label.clone())
                });
                let n = n.to_string();
                let model = self.model_name();
                let provider = self.meta.provider.short_label();
                let mut explain = vec![ExplainPart::new(
                    "explain.llm_request",
                    &[("n", &n), ("provider", provider), ("model", &model)],
                )];
                if let Some(purpose) = purpose.as_deref() {
                    explain.push(ExplainPart::new(
                        "explain.llm_purpose",
                        &[("purpose", &clip(purpose, 120))],
                    ));
                }
                explain.push(ExplainPart::new("explain.llm_tokens", &[]));
                let id = self.begin(
                    RunPhase::Llm,
                    tf("run.llm_request", &[("n", &n), ("model", &model)]),
                    purpose,
                );
                self.explain(id, explain);
                self.llm_step.set(Some(id));
            }
            LlmEvent::RequestFinished {
                tokens, answered, ..
            } => {
                let Some(id) = self.llm_step.take() else {
                    return;
                };
                if answered {
                    let spent = tf(
                        "run.llm_tokens",
                        &[
                            ("prompt", &format_tokens(tokens.prompt)),
                            ("completion", &format_tokens(tokens.completion)),
                        ],
                    );
                    let at = self.ms();
                    self.apply(|f| {
                        f.finish(id, at);
                        if let Some(step) = f.get_mut(id) {
                            step.detail = Some(match step.detail.take() {
                                Some(purpose) => format!("{purpose} \u{00b7} {spent}"),
                                None => spent.clone(),
                            });
                        }
                    });
                } else {
                    self.fail(id, t("run.llm_no_answer"));
                }
            }
            LlmEvent::Waiting { wait, reason } => {
                let secs = wait.as_secs().max(1).to_string();
                let provider = self.meta.provider.short_label().to_string();
                let (label, explain) = match reason {
                    WaitReason::Quota => (
                        tf("run.wait_quota", &[("s", &secs), ("provider", &provider)]),
                        ExplainPart::new("explain.wait_quota", &[("provider", &provider)]),
                    ),
                    WaitReason::Retry => (
                        tf("run.wait_retry", &[("s", &secs), ("provider", &provider)]),
                        ExplainPart::new("explain.wait_retry", &[("provider", &provider)]),
                    ),
                };
                let at = self.ms();
                let wait_ms = wait.as_millis() as u64;
                let id = self.apply(|f| {
                    let id = f.push(at, RunPhase::Llm, label.clone(), None, StepState::Running);
                    if let Some(step) = f.get_mut(id) {
                        step.wait_ms = Some(wait_ms);
                        step.explain_keys = vec![explain.clone()];
                    }
                    id
                });
                self.wait_step.set(Some(id));
            }
            LlmEvent::WaitOver => {
                if let Some(id) = self.wait_step.take() {
                    self.done(id);
                }
            }
        }
    }

    /// End a run that served no build (a Choya reply, a failed, stopped or
    /// panicked Choya run, or a tracker dropped mid-run). A run that made an
    /// LLM request still writes its record, build `None`, so its tokens and
    /// cost reach the history; one that made none only ends its feed.
    pub(super) fn close(&self, status: &GenerationStatus) {
        if self.usage.total().calls > 0 {
            self.finish(status.clone(), None);
        } else {
            self.end_feed(status, false);
            self.mark_finished();
        }
    }

    /// End the run: record and final steps, the record on disk, the feed
    /// handed back to the overlay as finished. Returns the record so the
    /// caller can attach it to the served tabs.
    pub(super) fn finish(
        &self,
        status: GenerationStatus,
        served: Option<Served<'_>>,
    ) -> Arc<GenerationRecord> {
        let already_closed = self.closed.get();
        let record_step = self.end_feed(&status, true);
        let record = Arc::new(self.build_record(status, served));
        // One record per run: a second ending (a panic after the first) only
        // reports, it does not write again.
        if already_closed {
            return record;
        }
        if let Err(e) = GenerationLog::new(&self.meta.addon_dir).append(&record) {
            if let Some(id) = record_step {
                self.fail(id, e.to_string());
            }
            nexus_log(format!("generation record not written: {e}"));
        } else {
            // The history tab re-reads the log the next time it is shown.
            crate::state::with_state(|s| s.main.generations.stale = true);
        }
        self.mark_finished();
        record
    }

    /// Close whatever is open, then the record step (when writing one) and
    /// the final status step. Returns the record step's id.
    fn end_feed(&self, status: &GenerationStatus, with_record: bool) -> Option<u32> {
        if self.closed.replace(true) {
            return None;
        }
        self.end_stage();
        if self.tier_open.get().is_some() {
            let why = match status {
                GenerationStatus::Failed { message } => message.clone(),
                _ => t("run.cancelled"),
            };
            self.tier_end(Err(&why));
        }
        let at = self.ms();
        let record_step = with_record.then(|| {
            self.apply(|f| {
                f.push(
                    at,
                    RunPhase::Record,
                    t("run.record_written"),
                    None,
                    StepState::Done { took_ms: 0 },
                )
            })
        });
        if let Some(id) = record_step {
            // Where the log lives, relative to the game's addons folder: the
            // record is shared in screenshots, the absolute path is not.
            let path = match self.meta.addon_dir.file_name() {
                Some(dir) => format!("{}/{GENERATIONS_FILE}", dir.to_string_lossy()),
                None => GENERATIONS_FILE.to_string(),
            };
            self.explain(id, ExplainPart::new("explain.record", &[("path", &path)]));
        }
        let (label, state) = match status {
            GenerationStatus::Ok => (t("run.done"), StepState::Done { took_ms: at }),
            GenerationStatus::Cancelled => (
                t("run.cancelled"),
                StepState::Failed {
                    message: t("run.cancelled"),
                },
            ),
            GenerationStatus::Failed { message } => (
                t("run.failed"),
                StepState::Failed {
                    message: clip(message, 300),
                },
            ),
        };
        self.apply(|f| {
            f.push(at, RunPhase::Run, label.clone(), None, state.clone());
            f.settle(at, *status == GenerationStatus::Ok);
        });
        record_step
    }

    fn mark_finished(&self) {
        let id = self.id;
        crate::state::with_state(|s| {
            if s.main.run_feed.run_id == id {
                s.main.run_feed.live = false;
            }
        });
    }

    /// A check that did not pass, recorded as a failed step.
    pub(super) fn warn(
        &self,
        phase: RunPhase,
        label: impl Into<String>,
        message: impl Into<String>,
    ) -> u32 {
        let id = self.begin(phase, label, None);
        self.fail(id, message);
        id
    }

    /// The steps so far (tests, and the Choya bubble fallback).
    #[cfg(test)]
    pub(super) fn steps(&self) -> Vec<gw2_core::generations::RunStep> {
        self.feed.borrow().steps.clone()
    }

    fn build_record(
        &self,
        status: GenerationStatus,
        served: Option<Served<'_>>,
    ) -> GenerationRecord {
        let run = self.usage.total();
        let finished_at = Utc::now();
        let duration_ms = self.started.elapsed().as_millis() as u64;
        let llm_wait_ms = (run.wait.as_millis() as u64).min(duration_ms);
        let llm = (run.calls > 0).then(|| LlmUsed {
            provider: self.meta.provider.clone(),
            model: self.meta.model.clone(),
        });
        let today = finished_at.format("%Y-%m-%d").to_string();
        let cost = llm.as_ref().and_then(|l| {
            gw2_optimizer::llm::pricing::estimate(&l.provider, &l.model, &run, &today)
        });
        let (tier, build, card, elite_spec) = match served {
            Some(s) => {
                let sug = s.suggestion;
                let ctx = gw2_optimizer::balance::BalanceContext::new(self.meta.mode.clone());
                let build = super::tabs::saveload::suggestion_to_saved(
                    &sug.label,
                    &self.meta.character_name,
                    &self.meta.profession,
                    &self.meta.mode,
                    Some(&ctx.patch_id),
                    sug,
                );
                // Tabs mark the elite " [E]"; a plate that does not is looked up.
                let elite_spec = sug
                    .specializations
                    .iter()
                    .find_map(|(name, _)| name.strip_suffix(" [E]").map(str::to_string))
                    .or_else(|| {
                        s.db.and_then(|db| {
                            sug.specializations.iter().find_map(|(name, _)| {
                                db.specializations
                                    .values()
                                    .any(|sp| sp.elite && &sp.name == name)
                                    .then(|| name.clone())
                            })
                        })
                    });
                (
                    Some(s.tier),
                    Some(build),
                    Some(card_for(sug, &self.meta.profession)),
                    elite_spec,
                )
            }
            None => (None, None, None, None),
        };
        GenerationRecord {
            id: GenerationRecord::new_id(),
            started_at: self.started_at,
            finished_at,
            duration_ms,
            llm_wait_ms,
            compute_ms: duration_ms - llm_wait_ms,
            tier_timings: self.tiers.borrow().clone(),
            kind: self.meta.kind,
            character_name: self.meta.character_name.clone(),
            profession: self.meta.profession.clone(),
            elite_spec,
            mode: self.meta.mode.clone(),
            scale_id: variant_id(&self.meta.tier),
            role_id: self.meta.role.as_ref().and_then(variant_id),
            scale: String::new(),
            role: String::new(),
            weights: serde_json::to_value(&self.meta.weights).unwrap_or_default(),
            llm,
            tokens: run.tokens,
            cost_estimate_usd: cost.as_ref().and_then(|c| c.usd),
            pricing_source: cost.map(|c| c.source),
            tier,
            status,
            build,
            card,
            steps: steps_for_record(&self.feed.borrow().steps),
        }
    }
}

/// A run that ends without an explicit close (stopped, superseded by a newer
/// chat) still ends its feed, as cancelled.
impl Drop for RunTracker {
    fn drop(&mut self) {
        if !self.closed.get() {
            self.close(&GenerationStatus::Cancelled);
        }
    }
}

fn nexus_log(message: String) {
    // `nexus::log` panics without the addon API (tests); the record is
    // best-effort, the build still lands.
    if cfg!(test) {
        eprintln!("{message}");
    } else {
        nexus::log::log(nexus::log::LogLevel::Warning, "GW2BuildOpt", message);
    }
}

/// Step grouping key: "Permuting kits (gen 3, ...)..." -> "Permuting kits".
fn stage_key(text: &str) -> &str {
    text.split('(')
        .next()
        .unwrap_or(text)
        .trim()
        .trim_end_matches('.')
        .trim()
}

fn tier_phase(tier: GenerationTier) -> (RunPhase, &'static str) {
    match tier {
        GenerationTier::BeamV2 => (RunPhase::Search, "run.tier_beam"),
        GenerationTier::Deterministic => (RunPhase::Deterministic, "run.tier_deterministic"),
        GenerationTier::Legacy => (RunPhase::Legacy, "run.tier_legacy"),
        GenerationTier::Choya => (RunPhase::Choya, "run.tier_choya"),
    }
}

/// The search limits, as the beam search runs them.
fn search_limits() -> [(&'static str, String); 4] {
    let c = gw2_optimizer::search_v2::SearchConfig::default();
    [
        ("beam", c.beam_width.to_string()),
        ("budget", c.eval_budget.to_string()),
        ("limit", c.time_limit_secs.to_string()),
        ("patience", c.patience.to_string()),
    ]
}

fn part_owned(key: &str, args: &[(&'static str, String)]) -> ExplainPart {
    let args: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
    ExplainPart::new(key, &args)
}

fn tier_explain(tier: GenerationTier) -> ExplainPart {
    match tier {
        GenerationTier::BeamV2 => part_owned("explain.tier_beam", &search_limits()),
        GenerationTier::Deterministic => ExplainPart::new("explain.tier_deterministic", &[]),
        GenerationTier::Legacy => ExplainPart::new("explain.tier_legacy", &[]),
        GenerationTier::Choya => ExplainPart::new("explain.tier_choya", &[]),
    }
}

/// What an engine progress line counts. Engine lines are English wire
/// strings, so they are matched by prefix and grouping key.
fn engine_explain(text: &str) -> Option<ExplainPart> {
    let receipt = [
        ("search_v2 meta seeds", "explain.meta_seeds"),
        ("search_v2 seed repair", "explain.seed_repair_receipt"),
        ("search_v2 funnel", "explain.funnel"),
        ("search_v2:", "explain.search_receipt"),
    ];
    if let Some((_, key)) = receipt.iter().find(|(p, _)| text.starts_with(p)) {
        return Some(ExplainPart::new(key, &[]));
    }
    let key = stage_key(text);
    let plain = |k: &str| ExplainPart::new(k, &[]);
    Some(match key {
        "Seeding from synergy pipeline" => plain("explain.seeding"),
        "Repairing seed viability" => plain("explain.seed_repair"),
        "Permuting kits" => part_owned("explain.permuting", &search_limits()),
        "LLM advisor: evaluating mutations" => plain("explain.llm_advisor"),
        "Fine-tuning piece swaps" => plain("explain.fine_tune"),
        _ if key.starts_with("Selecting optimal")
            || key.starts_with("Evaluating specializations and traits") =>
        {
            plain("explain.det_pick")
        }
        _ => return None,
    })
}

/// The measuring step's explanation: the flow window, in seconds.
pub(super) fn measuring_explain() -> ExplainPart {
    let secs = gw2_optimizer::engine::FLOW_WINDOW_MS / 1000;
    ExplainPart::new("explain.measuring", &[("secs", &secs.to_string())])
}

/// What the game-data step counts: one profession, and the whole database.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DataCounts {
    pub profession: String,
    /// The profession's own skills: weapon, utility, heal, elite, mechanic, other.
    pub by_type: [usize; 6],
    pub specs: usize,
    /// Minor + major traits of the profession's specializations: the Hero panel.
    pub traits: usize,
    /// The profession's other trait entries (elite weapon proficiencies).
    pub extra_traits: usize,
    pub db_skills: usize,
    /// Skill entries tagged with at least one profession.
    pub db_prof_skills: usize,
    /// Of those, racial skills shared by several professions.
    pub db_shared_skills: usize,
    pub db_traits: usize,
    pub db_specs: usize,
    pub db_panel_traits: usize,
}

impl DataCounts {
    pub(super) fn of(db: &gw2_optimizer::gamedb::GameDb, profession: &str) -> Self {
        let mut by_type = [0; 6];
        for skill in db.profession_skills(profession) {
            let slot = match skill.skill_type.as_deref() {
                Some("Weapon") => 0,
                Some("Utility") => 1,
                Some("Heal") => 2,
                Some("Elite") => 3,
                Some("Profession") => 4,
                _ => 5,
            };
            by_type[slot] += 1;
        }
        let mut out = Self {
            profession: profession.to_string(),
            by_type,
            db_skills: db.skills.len(),
            db_traits: db.traits.len(),
            db_specs: db.specializations.len(),
            ..Self::default()
        };
        for skill in db.skills.values() {
            if !skill.professions.is_empty() {
                out.db_prof_skills += 1;
            }
            if skill.professions.len() > 1 {
                out.db_shared_skills += 1;
            }
        }
        for spec in db.specializations.values() {
            let panel = spec.minor_traits.len() + spec.major_traits.len();
            out.db_panel_traits += panel;
            if spec.profession == profession {
                out.specs += 1;
                out.traits += panel;
                let held = db.traits_by_spec.get(&spec.id).map_or(0, Vec::len);
                out.extra_traits += held.saturating_sub(panel);
            }
        }
        out
    }

    fn skills(&self) -> usize {
        self.by_type.iter().sum()
    }

    /// "Guardian: 227 skills · 108 traits (db 2311 profession skills / 999 traits)".
    pub(super) fn label(&self) -> String {
        tf(
            "run.data_label",
            &[
                ("profession", &self.profession),
                ("skills", &self.skills().to_string()),
                ("traits", &self.traits.to_string()),
                ("db_skills", &self.db_prof_skills.to_string()),
                ("db_traits", &self.db_traits.to_string()),
            ],
        )
    }

    pub(super) fn explain(&self) -> Vec<ExplainPart> {
        let [weapon, utility, heal, elite, mechanic, other] = self.by_type.map(|n| n.to_string());
        let skills = ExplainPart::new(
            "explain.data_skills",
            &[
                ("profession", &self.profession),
                ("skills", &self.skills().to_string()),
                ("weapon", &weapon),
                ("utility", &utility),
                ("heal", &heal),
                ("elite", &elite),
                ("mechanic", &mechanic),
                ("other", &other),
            ],
        );
        let traits = ExplainPart::new(
            "explain.data_traits",
            &[
                ("profession", &self.profession),
                ("traits", &self.traits.to_string()),
                ("specs", &self.specs.to_string()),
                ("extra", &self.extra_traits.to_string()),
            ],
        );
        let db = ExplainPart::new(
            "explain.data_db",
            &[
                ("all", &self.db_skills.to_string()),
                ("prof", &self.db_prof_skills.to_string()),
                ("shared", &self.db_shared_skills.to_string()),
                (
                    "rest",
                    &self
                        .db_skills
                        .saturating_sub(self.db_prof_skills)
                        .to_string(),
                ),
                ("db_traits", &self.db_traits.to_string()),
                ("db_specs", &self.db_specs.to_string()),
                (
                    "db_extra",
                    &self
                        .db_traits
                        .saturating_sub(self.db_panel_traits)
                        .to_string(),
                ),
            ],
        );
        vec![skills, traits, db]
    }
}

/// What a history row shows for a served build.
pub(super) fn card_for(s: &BuildSuggestion, profession: &str) -> GenerationCard {
    let prefix_summary = s
        .build_summary
        .strip_prefix("Gear: ")
        .unwrap_or(&s.build_summary)
        .to_string();
    GenerationCard {
        title: s.label.clone(),
        profession: profession.to_string(),
        specs: s
            .specializations
            .iter()
            .map(|(n, _)| n.strip_suffix(" [E]").unwrap_or(n).to_string())
            .collect(),
        prefix_summary: if prefix_summary.is_empty() {
            s.stat_prefix.clone()
        } else {
            prefix_summary
        },
        simulated_dps: s.rotation.as_ref().map(|r| r.simulated_dps),
        meter_text: s
            .benchmark_delta
            .as_ref()
            .filter(|d| d.ref_score > 0.0)
            .map(|d| {
                format!(
                    "{:.0} % \u{00b7} {}",
                    d.our_score / d.ref_score * 100.0,
                    d.source
                )
            })
            .unwrap_or_default(),
        chat_code: s.chat_code.clone(),
    }
}

/// Attach the run's record to every tab it served.
pub(super) fn attach(suggestions: &mut [BuildSuggestion], record: &Arc<GenerationRecord>) {
    for s in suggestions {
        s.generation = Some(Arc::clone(record));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_core::generations::TokenUsage;
    use std::time::Duration;

    fn meta() -> RunMeta {
        RunMeta {
            kind: GenerationKind::Choya,
            character_name: String::new(),
            profession: "Guardian".into(),
            mode: GameMode::PvE,
            tier: CombatTier::Party,
            role: None,
            weights: OptimizationWeights::default(),
            provider: LlmProvider::Gemini,
            model: "gemini-2.5-flash".into(),
            addon_dir: std::env::temp_dir(),
        }
    }

    /// The game-data line counts the run's profession, and says what the
    /// database-wide numbers are next to it.
    #[test]
    fn game_data_label_counts_the_profession() {
        use gw2_optimizer::gamedb::GameDb;
        let mut db = GameDb::empty_for_tests();
        let skills = [
            (1, Some("Weapon"), vec!["Guardian"]),
            (2, Some("Weapon"), vec!["Guardian"]),
            (3, Some("Utility"), vec!["Guardian"]),
            (4, Some("Heal"), vec!["Guardian"]),
            (5, Some("Elite"), vec!["Guardian"]),
            (6, Some("Profession"), vec!["Guardian"]),
            (7, Some("Bundle"), vec!["Guardian"]),
            (8, Some("Weapon"), vec!["Warrior"]),
            (9, Some("Utility"), vec!["Guardian", "Warrior"]),
            (10, None, vec![]),
        ];
        for (id, kind, professions) in skills {
            let skill: gw2_api::models::Skill = serde_json::from_value(serde_json::json!({
                "id": id, "name": format!("s{id}"), "type": kind, "professions": professions,
            }))
            .expect("skill");
            if let [only] = skill.professions.as_slice() {
                db.skills_by_profession
                    .entry(only.clone())
                    .or_default()
                    .push(id);
            }
            db.skills.insert(id, skill);
        }
        // Two Guardian specs (the elite one with a weapon trait), one Warrior.
        for (id, profession, extra) in [(1, "Guardian", 0), (2, "Guardian", 1), (3, "Warrior", 0)] {
            let spec: gw2_api::models::Specialization = serde_json::from_value(serde_json::json!({
                "id": id, "name": format!("spec{id}"), "profession": profession, "elite": extra > 0,
                "minor_traits": [1, 2, 3], "major_traits": [4, 5, 6, 7, 8, 9, 10, 11, 12],
            }))
            .expect("spec");
            for n in 0..(12 + extra) {
                let trait_id = id * 100 + n;
                let tr: gw2_api::models::Trait = serde_json::from_value(serde_json::json!({
                    "id": trait_id, "name": "t", "specialization": id, "tier": 1, "order": 0,
                    "slot": "Major",
                }))
                .expect("trait");
                db.traits.insert(trait_id, tr);
                db.traits_by_spec.entry(id).or_default().push(trait_id);
            }
            db.specializations.insert(id, spec);
        }

        let counts = DataCounts::of(&db, "Guardian");
        assert_eq!(counts.by_type, [2, 1, 1, 1, 1, 1]);
        assert_eq!(
            (counts.specs, counts.traits, counts.extra_traits),
            (2, 24, 1)
        );
        assert_eq!(counts.db_prof_skills, 9, "every skill with a profession");
        assert_eq!(counts.db_shared_skills, 1);
        assert_eq!(counts.db_traits, 37);
        gw2_core::i18n::set_language("en");
        assert_eq!(
            counts.label(),
            "Guardian: 7 skills \u{00b7} 24 traits (db 9 profession skills / 37 traits)"
        );
        let explain = counts
            .explain()
            .iter()
            .map(ExplainPart::text)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(
            explain.contains("2 weapon, 1 utility, 1 heal, 1 elite"),
            "{explain}"
        );
        assert!(explain.contains("2 specializations x 12"), "{explain}");
        assert!(explain.contains("10 skill entries"), "{explain}");
        assert!(explain.contains("The other 1 are"), "{explain}");
    }

    /// A Choya run that spent tokens and served no build still writes its
    /// record: a reply, a failure (a panic is closed the same way), and a
    /// run dropped mid-flight (stopped, superseded). A run that made no
    /// request writes nothing.
    #[test]
    fn choya_runs_that_made_requests_are_recorded_without_a_build() {
        use gw2_core::generations::GenerationLog;
        use gw2_optimizer::llm::usage::{simulate_request, ResponseUsage};
        let dir = std::env::temp_dir().join(format!(
            "gw2bo_gen_choya_{}_{}",
            std::process::id(),
            NEXT_RUN.load(Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let choya = || RunMeta {
            addon_dir: dir.clone(),
            ..meta()
        };
        let spend = || {
            simulate_request(Some(ResponseUsage {
                prompt: Some(1_000),
                completion: Some(200),
                total: None,
                cost_usd: None,
            }))
        };

        let quiet = RunTracker::start(choya());
        quiet.close(&GenerationStatus::Ok);
        drop(quiet);
        assert!(
            GenerationLog::new(&dir).load_all().is_empty(),
            "no request, no record"
        );

        let reply = RunTracker::start(choya());
        spend();
        reply.close(&GenerationStatus::Ok);
        drop(reply);

        let failed = RunTracker::start(choya());
        spend();
        let open = failed.begin(RunPhase::Choya, "fallback", None);
        failed.close(&GenerationStatus::Failed {
            message: "panicked".into(),
        });
        failed.close(&GenerationStatus::Failed {
            message: "twice".into(),
        });
        drop(failed);

        let stopped = RunTracker::start(choya());
        spend();
        drop(stopped);

        let all = GenerationLog::new(&dir).load_all();
        let statuses: Vec<_> = all.iter().map(|r| r.status.clone()).collect();
        assert_eq!(
            statuses,
            [
                GenerationStatus::Ok,
                GenerationStatus::Failed {
                    message: "panicked".into()
                },
                GenerationStatus::Cancelled,
            ],
            "one record per run, a second close writes nothing"
        );
        for r in &all {
            assert_eq!(r.kind, GenerationKind::Choya);
            assert!(r.build.is_none() && r.card.is_none() && r.tier.is_none());
            assert_eq!(r.tokens.total, 1_200, "tokens kept");
            assert_eq!(r.tokens.requests, 1);
            assert!(r.llm.is_some());
            assert!(r.cost_estimate_usd.is_some(), "priced from the table");
        }
        let left = all[1].steps.iter().find(|s| s.label == "fallback");
        assert_eq!(
            left.map(|s| s.id),
            Some(open),
            "the open step is in the record"
        );
        assert_eq!(
            left.map(|s| &s.state),
            Some(&StepState::Skipped),
            "an interrupted step does not read as done"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each request and each wait is a step of its own, running until the
    /// transport says it ended.
    #[test]
    fn llm_events_become_steps() {
        let tracker = RunTracker::start(meta());
        tracker.stage(RunPhase::Choya, "Composing the plate (attempt 1)");
        tracker.on_llm(&LlmEvent::RequestStarted { n: 1 });
        tracker.on_llm(&LlmEvent::Waiting {
            wait: Duration::from_secs(42),
            reason: WaitReason::Quota,
        });
        let steps = tracker.steps();
        let wait = steps.last().expect("wait step");
        assert_eq!(wait.state, StepState::Running);
        assert_eq!(wait.wait_ms, Some(42_000), "the view counts it down");
        tracker.on_llm(&LlmEvent::WaitOver);
        tracker.on_llm(&LlmEvent::RequestFinished {
            n: 1,
            tokens: TokenUsage {
                prompt: 1_200,
                completion: 300,
                total: 1_500,
                requests: 1,
            },
            took: Duration::from_secs(3),
            answered: true,
        });
        tracker.on_llm(&LlmEvent::RequestStarted { n: 2 });
        tracker.on_llm(&LlmEvent::RequestFinished {
            n: 2,
            tokens: TokenUsage::default(),
            took: Duration::from_secs(1),
            answered: false,
        });

        let llm: Vec<_> = tracker
            .steps()
            .into_iter()
            .filter(|s| s.phase == RunPhase::Llm)
            .collect();
        assert_eq!(llm.len(), 3, "request 1, its wait, request 2: {llm:?}");
        assert!(matches!(llm[0].state, StepState::Done { .. }));
        let detail = llm[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.starts_with("Composing the plate") && detail.contains("1.2k"),
            "purpose and tokens: {detail}"
        );
        assert!(matches!(llm[1].state, StepState::Done { .. }), "wait over");
        assert!(
            matches!(llm[2].state, StepState::Failed { .. }),
            "no answer"
        );
        tracker.close(&GenerationStatus::Ok);
        let last = tracker.steps().last().cloned().expect("final step");
        assert_eq!(last.phase, RunPhase::Run);
        assert!(tracker
            .steps()
            .iter()
            .all(|s| s.state != StepState::Running));
    }
}
