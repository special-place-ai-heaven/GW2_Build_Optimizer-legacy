//! Generation history: one record per New Build, Improve or Choya run.
//!
//! Records are appended to `{addon_dir}/generations.jsonl`, one JSON object
//! per line, oldest first. The file is append-only: a record is never
//! rewritten, so a crash can at worst leave one half-written line at the end,
//! which every reader skips.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::LlmProvider;
use crate::types::{GameMode, SavedBuild};

/// File name of the generation log inside the addon directory.
pub const GENERATIONS_FILE: &str = "generations.jsonl";
/// The one rotated backup of the log.
pub const GENERATIONS_BACKUP_FILE: &str = "generations.1.jsonl";
/// Past this size the log is rotated to [`GENERATIONS_BACKUP_FILE`] before
/// the next append, so a history read never parses more than twice this.
pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

/// Which button started the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationKind {
    NewBuild,
    Improve,
    Choya,
}

/// Which pipeline produced the served build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationTier {
    /// `engine::optimize_v2`, the beam search.
    BeamV2,
    /// `engine::optimize_deterministic_cancellable`, the synergy pipeline.
    Deterministic,
    /// `engine::optimize_cancellable`, the legacy gear + spec search.
    Legacy,
    /// A build Choya plated in chat.
    Choya,
}

/// How the run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GenerationStatus {
    Ok,
    Cancelled,
    Failed { message: String },
}

/// Tokens spent by every LLM request of one run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
    /// Chat requests that returned a body, whether or not it carried usage.
    pub requests: u32,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt = self.prompt.saturating_add(other.prompt);
        self.completion = self.completion.saturating_add(other.completion);
        self.total = self.total.saturating_add(other.total);
        self.requests = self.requests.saturating_add(other.requests);
    }
}

/// The model a run talked to. `None` on a record means no LLM request was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmUsed {
    pub provider: LlmProvider,
    pub model: String,
}

/// Where a cost estimate came from: a row of `data/llm_pricing.json`, or the
/// provider's own reported cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingSource {
    pub row_id: String,
    pub as_of: String,
}

/// Wall time one optimizer tier ran for, in run order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierTiming {
    pub tier: GenerationTier,
    pub duration_ms: u64,
    /// Whether this tier produced the served build.
    pub served: bool,
}

/// What a history row shows without opening the build.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationCard {
    pub title: String,
    pub profession: String,
    pub specs: Vec<String>,
    pub prefix_summary: String,
    pub simulated_dps: Option<i32>,
    /// The meter line, e.g. "195 % · Guildjen". Empty when nothing was compared.
    pub meter_text: String,
    pub chat_code: Option<String>,
}

/// What part of a run a step belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunPhase {
    Run,
    Data,
    /// Tier 1, the beam search.
    Search,
    /// Tier 2, the deterministic synergy pipeline.
    Deterministic,
    /// Tier 3, the legacy search.
    Legacy,
    Llm,
    Validation,
    Simulation,
    Reference,
    Record,
    Choya,
}

/// Where a step stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StepState {
    Running,
    Done { took_ms: u64 },
    Failed { message: String },
    Skipped,
}

/// One line of the live run feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStep {
    pub id: u32,
    /// Milliseconds since the run started.
    pub at_ms: u64,
    pub phase: RunPhase,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub state: StepState,
    /// A wait step's announced length, so a view can count it down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// Hover text as builds before `explain_keys` stored it, frozen in the
    /// run's language. Only read, as the fallback for old records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain: Option<String>,
    /// What the step's numbers count, in player words, as catalog keys: the
    /// feed resolves them on hover, in the language shown now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explain_keys: Vec<ExplainPart>,
}

impl RunStep {
    pub fn has_explain(&self) -> bool {
        !self.explain_keys.is_empty() || self.explain.is_some()
    }

    /// The hover explanation in the current language, paragraphs a blank
    /// line apart; an old record's stored text when it has no keys.
    pub fn explain_text(&self) -> Option<String> {
        if self.explain_keys.is_empty() {
            return self.explain.clone();
        }
        Some(
            self.explain_keys
                .iter()
                .map(ExplainPart::text)
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }
}

/// One paragraph of a step's hover explanation: a catalog key and the small
/// values it names (counts, a model id), never the translated text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainPart {
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<(String, String)>,
}

impl ExplainPart {
    pub fn new(key: &str, args: &[(&str, &str)]) -> Self {
        Self {
            key: key.to_string(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    pub fn text(&self) -> String {
        let args: Vec<(&str, &str)> = self
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        crate::i18n::tf(&self.key, &args)
    }
}

/// Most steps a feed keeps: the first half and the last half, with one
/// elision marker between them.
pub const MAX_RUN_STEPS: usize = 500;
/// Id of the elision marker; its `detail` is the number of steps dropped.
pub const ELIDED_STEP_ID: u32 = u32::MAX;

/// The steps of one run, in order, bounded by [`MAX_RUN_STEPS`].
///
/// Steps are addressed by id, not index: eliding shifts indices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFeed {
    pub steps: Vec<RunStep>,
    next_id: u32,
    elided: usize,
}

impl RunFeed {
    /// Append a step; returns its id.
    pub fn push(
        &mut self,
        at_ms: u64,
        phase: RunPhase,
        label: impl Into<String>,
        detail: Option<String>,
        state: StepState,
    ) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.steps.push(RunStep {
            id,
            at_ms,
            phase,
            label: label.into(),
            detail,
            state,
            wait_ms: None,
            explain: None,
            explain_keys: Vec::new(),
        });
        self.enforce_cap();
        id
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut RunStep> {
        self.steps.iter_mut().rev().find(|s| s.id == id)
    }

    /// Mark a step done, timing it from its own start.
    pub fn finish(&mut self, id: u32, at_ms: u64) {
        if let Some(step) = self.get_mut(id) {
            step.state = StepState::Done {
                took_ms: at_ms.saturating_sub(step.at_ms),
            };
        }
    }

    pub fn fail(&mut self, id: u32, message: impl Into<String>) {
        if let Some(step) = self.get_mut(id) {
            step.state = StepState::Failed {
                message: message.into(),
            };
        }
    }

    /// The newest step still running, if any.
    pub fn running(&self) -> Option<&RunStep> {
        self.steps
            .iter()
            .rev()
            .find(|s| s.state == StepState::Running && s.id != ELIDED_STEP_ID)
    }

    /// Close every step still running (the run ended under them): done when
    /// the run succeeded, skipped when it was cancelled or failed, so an
    /// interrupted step never reads as finished.
    pub fn settle(&mut self, at_ms: u64, ok: bool) {
        for step in &mut self.steps {
            if step.state == StepState::Running {
                step.state = if ok {
                    StepState::Done {
                        took_ms: at_ms.saturating_sub(step.at_ms),
                    }
                } else {
                    StepState::Skipped
                };
            }
        }
    }

    /// Keep the first half and the newest half; drop the oldest finished step
    /// after the marker. A step still running is not dropped while a finished
    /// one can be.
    fn enforce_cap(&mut self) {
        if self.steps.len() <= MAX_RUN_STEPS {
            return;
        }
        let head = MAX_RUN_STEPS / 2;
        if self.steps[head].id != ELIDED_STEP_ID {
            self.steps.insert(
                head,
                RunStep {
                    id: ELIDED_STEP_ID,
                    at_ms: self.steps[head].at_ms,
                    phase: RunPhase::Run,
                    label: "...".into(),
                    detail: None,
                    state: StepState::Skipped,
                    wait_ms: None,
                    explain: None,
                    explain_keys: Vec::new(),
                },
            );
        }
        while self.steps.len() > MAX_RUN_STEPS + 1 {
            let tail = head + 1;
            let drop_at = self.steps[tail..]
                .iter()
                .position(|s| s.state != StepState::Running)
                .map_or(tail, |i| tail + i);
            self.steps.remove(drop_at);
            self.elided += 1;
        }
        self.steps[head].detail = Some(self.elided.to_string());
    }
}

/// Most steps a record keeps on disk: the first half, one marker, the last half.
pub const MAX_STORED_STEPS: usize = 200;

/// `steps` cut to [`MAX_STORED_STEPS`] for the log: the first and last
/// hundred, with one elision marker between them counting every step dropped
/// (a marker the live feed already had included).
pub fn steps_for_record(steps: &[RunStep]) -> Vec<RunStep> {
    if steps.len() <= MAX_STORED_STEPS {
        return steps.to_vec();
    }
    let head = MAX_STORED_STEPS / 2;
    let tail = steps.len() - (MAX_STORED_STEPS - head);
    let dropped: usize = steps[head..tail]
        .iter()
        .map(|s| {
            if s.id == ELIDED_STEP_ID {
                s.detail
                    .as_deref()
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(0)
            } else {
                1
            }
        })
        .sum();
    let mut out = steps[..head].to_vec();
    out.push(RunStep {
        id: ELIDED_STEP_ID,
        at_ms: steps[head].at_ms,
        phase: RunPhase::Run,
        label: "...".into(),
        detail: Some(dropped.to_string()),
        state: StepState::Skipped,
        wait_ms: None,
        explain: None,
        explain_keys: Vec::new(),
    });
    out.extend_from_slice(&steps[tail..]);
    out
}

/// One run, as the history tab and the results header read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// Wall time spent inside LLM requests, retries and rate-limit waits included.
    pub llm_wait_ms: u64,
    /// `duration_ms - llm_wait_ms`: everything else the run did.
    pub compute_ms: u64,
    #[serde(default)]
    pub tier_timings: Vec<TierTiming>,
    pub kind: GenerationKind,
    pub character_name: String,
    pub profession: String,
    pub elite_spec: Option<String>,
    pub mode: GameMode,
    /// The scale as the optimizer's `CombatTier` variant name ("Solo",
    /// "Party", "Squad"): the left panel's tier, whatever the mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_id: Option<String>,
    /// The role as the optimizer's `RoleObjective` variant name
    /// ("PowerDps"); `None` when no role was picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// Localized scale label. Only records written before `scale_id` carry
    /// it; readers show it when there is no id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scale: String,
    /// Localized role label, as [`Self::scale`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// `OptimizationWeights` as serialized by the optimizer crate.
    pub weights: serde_json::Value,
    pub llm: Option<LlmUsed>,
    pub tokens: TokenUsage,
    pub cost_estimate_usd: Option<f64>,
    pub pricing_source: Option<PricingSource>,
    /// The tier that produced the served build; `None` when nothing was served.
    pub tier: Option<GenerationTier>,
    pub status: GenerationStatus,
    /// The served build, in the shape Saves persists, so the history can
    /// reopen it exactly like a save. `None` when nothing was served.
    pub build: Option<SavedBuild>,
    pub card: Option<GenerationCard>,
    /// The run's step feed, bounded by [`MAX_STORED_STEPS`].
    #[serde(default)]
    pub steps: Vec<RunStep>,
}

impl GenerationRecord {
    /// A fresh unique record id.
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// The append-only generation log.
pub struct GenerationLog {
    path: PathBuf,
}

/// One append at a time, so the trailing-newline check and the write that
/// follows it cannot interleave with another append in this process.
static APPEND_LOCK: Mutex<()> = Mutex::new(());

impl GenerationLog {
    pub fn new(addon_dir: &Path) -> Self {
        Self {
            path: addon_dir.join(GENERATIONS_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn backup_path(&self) -> PathBuf {
        self.path.with_file_name(GENERATIONS_BACKUP_FILE)
    }

    /// Append one record as one line and flush it to the device. Blocking
    /// file I/O: call it from a worker thread.
    ///
    /// A crash mid-append leaves a line with no newline; the next append
    /// starts on a fresh line so it is not glued onto that fragment. A log
    /// past [`MAX_LOG_BYTES`] first becomes the backup, replacing the old one.
    pub fn append(&self, record: &GenerationRecord) -> std::io::Result<()> {
        self.append_capped(record, MAX_LOG_BYTES)
    }

    fn append_capped(&self, record: &GenerationRecord, max_bytes: u64) -> std::io::Result<()> {
        let _guard = APPEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > max_bytes) {
            std::fs::rename(&self.path, self.backup_path())?;
        }
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        let len = file.metadata()?.len();
        if len > 0 {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.insert(0, '\n');
            }
        }
        file.write_all(line.as_bytes())?;
        file.sync_all()
    }

    /// Every readable record, the backup's then the current log's, oldest
    /// first. Unreadable lines (a torn tail, a record from a future schema
    /// this build cannot parse) are skipped. Blocking file I/O: call it from a
    /// worker thread.
    pub fn load_all(&self) -> Vec<GenerationRecord> {
        let mut out = Vec::new();
        for path in [self.backup_path(), self.path.clone()] {
            for_each_line(&path, |line| {
                if let Ok(record) = serde_json::from_slice(line) {
                    out.push(record);
                }
            });
        }
        out
    }
}

fn for_each_line(path: &Path, mut f: impl FnMut(&[u8])) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => f(&buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gw2bo_generations_{label}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn record(n: u64) -> GenerationRecord {
        let started_at = DateTime::from_timestamp(1_790_000_000 + n as i64, 0).expect("time");
        GenerationRecord {
            id: GenerationRecord::new_id(),
            started_at,
            finished_at: started_at + chrono::Duration::milliseconds(432_000),
            duration_ms: 432_000,
            llm_wait_ms: 340_000,
            compute_ms: 92_000,
            tier_timings: vec![TierTiming {
                tier: GenerationTier::BeamV2,
                duration_ms: 432_000,
                served: true,
            }],
            kind: GenerationKind::NewBuild,
            character_name: format!("Char {n}"),
            profession: "Guardian".into(),
            elite_spec: Some("Dragonhunter".into()),
            mode: GameMode::WvW,
            scale_id: Some("Solo".into()),
            role_id: Some("PowerDps".into()),
            scale: String::new(),
            role: String::new(),
            weights: serde_json::json!({ "power": 0.99 }),
            llm: Some(LlmUsed {
                provider: LlmProvider::Gemini,
                model: "gemini-2.5-flash".into(),
            }),
            tokens: TokenUsage {
                prompt: 40_000,
                completion: 1_200,
                total: 41_200,
                requests: 6,
            },
            cost_estimate_usd: Some(0.015),
            pricing_source: Some(PricingSource {
                row_id: "gemini-2.5-flash".into(),
                as_of: "2026-09-24".into(),
            }),
            tier: Some(GenerationTier::BeamV2),
            status: GenerationStatus::Ok,
            build: None,
            card: Some(GenerationCard {
                title: "Power DPS".into(),
                ..Default::default()
            }),
            steps: Vec::new(),
        }
    }

    #[test]
    fn round_trip_and_newest_first_pages() {
        let dir = temp_dir("round_trip");
        let log = GenerationLog::new(&dir);
        assert!(log.load_all().is_empty(), "no file yet reads as empty");
        for n in 0..5 {
            log.append(&record(n)).expect("append");
        }
        let all = log.load_all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].character_name, "Char 0", "oldest first");
        assert_eq!(all[4].tokens.total, 41_200);
        assert_eq!(all[4].status, GenerationStatus::Ok);
        assert_eq!(
            all[4].llm.as_ref().map(|l| l.model.as_str()),
            Some("gemini-2.5-flash")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past the size cap the log becomes the one backup before the next
    /// append; the history still reads both, oldest first, and a second
    /// rotation replaces the backup.
    #[test]
    fn a_full_log_rotates_to_one_backup_and_both_are_read() {
        let dir = temp_dir("rotate");
        let log = GenerationLog::new(&dir);
        log.append_capped(&record(0), 1).expect("append");
        log.append_capped(&record(1), 1)
            .expect("rotates, then appends");
        let backup = dir.join(GENERATIONS_BACKUP_FILE);
        assert!(backup.exists(), "the full log became the backup");
        let names: Vec<_> = log
            .load_all()
            .into_iter()
            .map(|r| r.character_name)
            .collect();
        assert_eq!(names, ["Char 0", "Char 1"], "backup first, then current");

        log.append_capped(&record(2), 1).expect("rotates again");
        let names: Vec<_> = log
            .load_all()
            .into_iter()
            .map(|r| r.character_name)
            .collect();
        assert_eq!(names, ["Char 1", "Char 2"], "one backup, the oldest gone");
        // Under the cap nothing moves.
        log.append(&record(3)).expect("append");
        assert_eq!(log.load_all().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record keeps the first and last hundred steps and one marker that
    /// counts everything dropped, the live feed's own elisions included.
    #[test]
    fn a_record_stores_at_most_two_hundred_steps() {
        let mut feed = RunFeed::default();
        for n in 0..700u64 {
            feed.push(
                n,
                RunPhase::Llm,
                format!("step {n}"),
                None,
                StepState::Done { took_ms: 0 },
            );
        }
        let stored = steps_for_record(&feed.steps);
        assert_eq!(stored.len(), MAX_STORED_STEPS + 1);
        assert_eq!(stored[0].label, "step 0");
        assert_eq!(stored[99].label, "step 99");
        assert_eq!(stored[100].id, ELIDED_STEP_ID);
        assert_eq!(stored[100].detail.as_deref(), Some("500"), "700 - 200");
        assert_eq!(stored.last().map(|s| s.label.as_str()), Some("step 699"));
        let short = &feed.steps[..10];
        assert_eq!(steps_for_record(short), short, "a short run is kept whole");
    }

    #[test]
    fn a_torn_tail_is_skipped_and_the_next_append_survives() {
        let dir = temp_dir("torn");
        let log = GenerationLog::new(&dir);
        log.append(&record(1)).expect("append");
        // A crash mid-append: half a record, no newline.
        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .expect("open");
        file.write_all(br#"{"id":"torn","started_at":"2026-"#)
            .expect("tear");
        drop(file);

        assert_eq!(log.load_all().len(), 1, "the torn line is skipped");

        log.append(&record(2)).expect("append after tear");
        let all = log.load_all();
        assert_eq!(
            all.len(),
            2,
            "the new record is not glued onto the fragment"
        );
        assert_eq!(all[1].character_name, "Char 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_feed_keeps_the_first_and_last_halves_with_one_marker() {
        let mut feed = RunFeed::default();
        // A long step that stays running across the elision.
        let tier = feed.push(
            0,
            RunPhase::Search,
            "tier",
            None,
            StepState::Done { took_ms: 0 },
        );
        let mut long_running = None;
        for n in 1..=700u64 {
            let id = feed.push(
                n,
                RunPhase::Llm,
                format!("step {n}"),
                None,
                StepState::Running,
            );
            if n == 300 {
                long_running = Some(id);
            } else {
                feed.finish(id, n + 1);
            }
        }
        let head = MAX_RUN_STEPS / 2;
        assert_eq!(
            feed.steps.len(),
            MAX_RUN_STEPS + 1,
            "500 steps plus the marker"
        );
        assert_eq!(feed.steps[0].id, tier, "the first step is kept");
        assert_eq!(feed.steps[head - 1].label, "step 249");
        assert_eq!(feed.steps[head].id, ELIDED_STEP_ID);
        assert_eq!(
            feed.steps.last().map(|s| s.label.as_str()),
            Some("step 700")
        );
        // 701 pushed, 500 kept.
        assert_eq!(feed.steps[head].detail.as_deref(), Some("201"));
        let still = long_running.expect("pushed");
        assert!(
            feed.steps.iter().any(|s| s.id == still),
            "a running step survives elision while finished ones can go"
        );
        feed.settle(800, false);
        assert!(feed.running().is_none());
        assert_eq!(
            feed.steps.iter().find(|s| s.id == still).map(|s| &s.state),
            Some(&StepState::Skipped),
            "a step a stopped run left open does not read as done"
        );
    }

    #[test]
    fn a_step_explanation_is_stored_as_keys_and_resolved_when_read() {
        let dir = temp_dir("explain");
        let log = GenerationLog::new(&dir);
        let mut feed = RunFeed::default();
        let id = feed.push(
            0,
            RunPhase::Llm,
            "Request 1",
            None,
            StepState::Done { took_ms: 0 },
        );
        feed.get_mut(id).expect("step").explain_keys = vec![
            ExplainPart::new(
                "explain.llm_request",
                &[("n", "1"), ("provider", "Gemini"), ("model", "gemini-x")],
            ),
            ExplainPart::new("explain.llm_tokens", &[]),
        ];
        feed.push(1, RunPhase::Run, "Done", None, StepState::Skipped);
        let mut r = record(4);
        r.steps = feed.steps.clone();
        log.append(&r).expect("append");

        let raw = std::fs::read_to_string(log.path()).expect("log");
        assert!(raw.contains("\"explain.llm_request\""), "the key is stored");
        assert!(
            !raw.contains("\"explain\":"),
            "no translated text is stored"
        );
        let loaded = log.load_all();
        assert_eq!(loaded[0].steps, feed.steps, "keys survive the log");
        let step = &loaded[0].steps[0];
        assert!(step.has_explain());
        let text = step.explain_text().expect("resolved");
        assert!(text.contains("gemini-x"), "args filled: {text}");
        assert!(
            !text.contains("{model}") && !text.contains("explain."),
            "{text}"
        );
        assert!(text.contains("\n\n"), "paragraphs apart: {text}");
        assert!(!loaded[0].steps[1].has_explain());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Steps written before the keys existed show the text they stored, and
    /// steps older still load with no explanation at all.
    #[test]
    fn old_steps_fall_back_to_their_stored_text() {
        let with_text = r#"{"id":0,"at_ms":0,"phase":"Data","label":"Game data ready","state":{"state":"done","took_ms":0},"explain":"what is counted"}"#;
        let step: RunStep = serde_json::from_str(with_text).expect("text step loads");
        assert!(step.explain_keys.is_empty());
        assert!(step.has_explain());
        assert_eq!(step.explain_text().as_deref(), Some("what is counted"));

        let bare = r#"{"id":0,"at_ms":0,"phase":"Data","label":"Game data ready","detail":"4761 skills, 999 traits","state":{"state":"done","took_ms":0}}"#;
        let step: RunStep = serde_json::from_str(bare).expect("old step loads");
        assert!(!step.has_explain());
        assert_eq!(step.explain_text(), None);
        let json = serde_json::to_value(&step).expect("json");
        assert!(
            json.get("explain").is_none() && json.get("explain_keys").is_none(),
            "absent stays absent"
        );
    }

    #[test]
    fn failed_status_serializes_with_its_message() {
        let mut r = record(3);
        r.status = GenerationStatus::Failed {
            message: "boom".into(),
        };
        let json = serde_json::to_value(&r).expect("json");
        assert_eq!(json["status"]["state"], "failed");
        assert_eq!(json["status"]["message"], "boom");
        assert!(json["started_at"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z')));
    }

    /// New records store ids and no display text; records written before the
    /// ids existed still read, with their text and no ids.
    #[test]
    fn scale_and_role_are_ids_and_old_text_records_still_read() {
        let json = serde_json::to_value(record(4)).expect("json");
        assert_eq!(json["scale_id"], "Solo");
        assert_eq!(json["role_id"], "PowerDps");
        assert!(json.get("scale").is_none() && json.get("role").is_none());

        let mut old = json;
        let obj = old.as_object_mut().expect("object");
        obj.remove("scale_id");
        obj.remove("role_id");
        obj.insert("scale".into(), "Roam".into());
        obj.insert("role".into(), "Damage".into());
        let dir = temp_dir("old_schema");
        let log = GenerationLog::new(&dir);
        std::fs::write(log.path(), format!("{old}\n")).expect("write");
        let read = log.load_all();
        assert_eq!(read.len(), 1, "an old record is not skipped");
        assert_eq!(read[0].scale_id, None);
        assert_eq!(read[0].role_id, None);
        assert_eq!(read[0].scale, "Roam");
        assert_eq!(read[0].role, "Damage");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
