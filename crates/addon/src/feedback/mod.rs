//! Feedback feature (addon side): state, HTTP client, shell helper, background tasks.

pub mod client;
pub mod shell;
pub mod tasks;

use std::collections::HashMap;
use std::time::Instant;

use gw2_core::feedback::message::{FailReason, LastPath, LocalMessage, MessageStatus};
use gw2_core::feedback::report::{
    snapshot_bytes, strip_wizard_markup, BuildSnapshot, MAX_SNAPSHOT_BYTES,
};
use gw2_core::feedback::taxonomy::{Category, FeedbackTaxonomy};

/// Which list the About tab shows under the hero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AboutView {
    #[default]
    WhatsNew,
    Messages,
    /// Every New Build, Improve and Choya run, from `generations.jsonl`.
    Generations,
}

/// Where the Message developer wizard currently is.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum WizardStep {
    #[default]
    Pick,
    Step(usize),
    Summary,
    Sending,
    Sent {
        short_id: String,
    },
    Thanks,
}

/// An open wizard draft. The taxonomy is frozen at open time so a background
/// refresh cannot change the steps under the player.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub report_id: String,
    pub taxonomy: FeedbackTaxonomy,
    pub category: Option<String>,
    /// step id → choice id
    pub choices: HashMap<String, String>,
    /// step id → text
    pub texts: HashMap<String, String>,
    pub contact: String,
    pub include_build: bool,
    pub include_account: bool,
    pub step: WizardStep,
    /// Last send failure shown under Send.
    pub error: Option<FailReason>,
}

/// Why a free-text step does not satisfy its `TextRule`; carries the bound that was violated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    /// Fewer characters than the step's `min`.
    TooShort(usize),
    /// More characters than the step's `max`.
    TooLong(usize),
}

impl Draft {
    /// New draft with a frozen copy of the taxonomy and a fresh report id; step = Pick, no category.
    pub fn new(taxonomy: FeedbackTaxonomy) -> Self {
        Self {
            report_id: uuid::Uuid::new_v4().to_string(),
            taxonomy,
            category: None,
            choices: HashMap::new(),
            texts: HashMap::new(),
            contact: String::new(),
            include_build: false,
            include_account: false,
            step: WizardStep::Pick,
            error: None,
        }
    }

    /// "Same as last time": pick the category, fill the choices from `last.path` in order over
    /// the category's choice steps, then jump to the first text step (or Summary if none).
    /// A choice id the frozen taxonomy no longer offers is skipped, so it shows up in `missing_steps`.
    pub fn from_last_path(taxonomy: FeedbackTaxonomy, last: &LastPath) -> Self {
        let mut draft = Self::new(taxonomy);
        draft.pick(&last.category);
        let choice_steps: Vec<String> = draft
            .step_ids()
            .iter()
            .filter(|id| draft.taxonomy.step(id).is_some_and(|s| s.text.is_none()))
            .cloned()
            .collect();
        for (step_id, choice_id) in choice_steps.iter().zip(&last.path) {
            let offered = draft
                .taxonomy
                .step(step_id)
                .is_some_and(|s| s.choices.iter().any(|c| c == choice_id));
            if offered {
                draft.set_choice(step_id, choice_id);
            }
        }
        if draft.category.is_some() {
            draft.step = match draft.first_text_step() {
                Some(index) => WizardStep::Step(index),
                None => WizardStep::Summary,
            };
        }
        draft
    }

    /// "Edit and resend": a fresh draft (new `report_id`, design §6a) prefilled from a failed
    /// row — its category and choice path via [`Self::from_last_path`], its body typed into the
    /// first text step, opened on that step. An unknown category leaves the draft on Pick.
    pub fn from_failed(taxonomy: FeedbackTaxonomy, m: &LocalMessage) -> Self {
        let last = LastPath {
            category: m.category.clone(),
            path: m.path.clone(),
        };
        let mut draft = Self::from_last_path(taxonomy, &last);
        let text_step = draft
            .first_text_step()
            .and_then(|i| draft.step_id(i))
            .map(str::to_string);
        if let Some(step_id) = text_step {
            draft.set_text(&step_id, m.body.clone());
        }
        draft
    }

    /// Pick a category: sets `category`, resets choices/texts and the last send error, and moves
    /// to `Step(0)` (or `Summary` when the category has no steps). Unknown ids are ignored.
    pub fn pick(&mut self, category_id: &str) {
        if self.taxonomy.category(category_id).is_none() {
            return;
        }
        self.category = Some(category_id.to_string());
        self.choices.clear();
        self.texts.clear();
        self.error = None;
        self.step = if self.step_ids().is_empty() {
            WizardStep::Summary
        } else {
            WizardStep::Step(0)
        };
    }

    /// The picked category, if it exists in the frozen taxonomy.
    pub fn category(&self) -> Option<&Category> {
        self.category
            .as_deref()
            .and_then(|id| self.taxonomy.category(id))
    }

    /// The picked category's step ids in order; empty before a pick or for links.
    pub fn step_ids(&self) -> &[String] {
        self.category().map_or(&[], |c| c.steps.as_slice())
    }

    /// Step id at `index` in the category's step order.
    pub fn step_id(&self, index: usize) -> Option<&str> {
        self.step_ids().get(index).map(String::as_str)
    }

    /// Number of wizard screens: the category's steps plus the summary.
    pub fn total_steps(&self) -> usize {
        self.step_ids().len() + 1
    }

    /// 1-based position for "Step n of m": `Step(i)` → `i + 1`, `Summary` → `total_steps()`, else None.
    pub fn current_index(&self) -> Option<usize> {
        match self.step {
            WizardStep::Step(i) => Some(i + 1),
            WizardStep::Summary => Some(self.total_steps()),
            _ => None,
        }
    }

    /// Choice steps are always required; text steps only when their `min` is above zero.
    /// A step id the frozen taxonomy does not define is never required.
    pub fn is_required(&self, step_id: &str) -> bool {
        match self.taxonomy.step(step_id) {
            Some(step) => match &step.text {
                Some(rule) => rule.min > 0,
                None => true,
            },
            None => false,
        }
    }

    /// True when the step is satisfied: a choice was made, or the text is within its rule and
    /// either non-empty or optional (`min == 0`).
    pub fn has_value(&self, step_id: &str) -> bool {
        match self.taxonomy.step(step_id) {
            Some(step) => match &step.text {
                Some(rule) => {
                    let text = self.texts.get(step_id).map_or("", String::as_str);
                    self.text_error(step_id).is_none() && (!text.is_empty() || rule.min == 0)
                }
                None => self.choices.contains_key(step_id),
            },
            None => false,
        }
    }

    /// Length check for a text step against its `TextRule`, counting chars (not bytes).
    /// Missing text counts as empty. Choice steps and unknown ids never error.
    pub fn text_error(&self, step_id: &str) -> Option<TextError> {
        let rule = self.taxonomy.step(step_id)?.text.as_ref()?;
        let count = self
            .texts
            .get(step_id)
            .map_or(0, |text| text.chars().count());
        if count < rule.min {
            Some(TextError::TooShort(rule.min))
        } else if count > rule.max {
            Some(TextError::TooLong(rule.max))
        } else {
            None
        }
    }

    /// Required step ids that still lack a value, in taxonomy order.
    pub fn missing_steps(&self) -> Vec<String> {
        self.step_ids()
            .iter()
            .filter(|id| {
                (self.is_required(id) && !self.has_value(id)) || self.text_error(id).is_some()
            })
            .cloned()
            .collect()
    }

    /// True when nothing is missing and the category is a `report` kind (links never post).
    pub fn can_send(&self) -> bool {
        self.category().is_some_and(|c| c.kind == "report") && self.missing_steps().is_empty()
    }

    /// Advance one screen: `Step(i)` → `Step(i + 1)`, or `Summary` after the last step.
    /// Every other position stays where it is.
    pub fn next(&mut self) {
        if let WizardStep::Step(i) = self.step {
            self.step = if i + 1 >= self.step_ids().len() {
                WizardStep::Summary
            } else {
                WizardStep::Step(i + 1)
            };
        }
    }

    /// Go back one screen: `Summary` → last step, `Step(i > 0)` → `Step(i - 1)`, `Step(0)` → `Pick`.
    /// The category and everything typed are kept.
    pub fn back(&mut self) {
        self.step = match self.step {
            WizardStep::Summary => match self.step_ids().len().checked_sub(1) {
                Some(last) => WizardStep::Step(last),
                None => WizardStep::Pick,
            },
            WizardStep::Step(0) => WizardStep::Pick,
            WizardStep::Step(i) => WizardStep::Step(i - 1),
            ref other => other.clone(),
        };
    }

    /// Record the choice for a choice step.
    pub fn set_choice(&mut self, step_id: &str, choice_id: &str) {
        self.choices
            .insert(step_id.to_string(), choice_id.to_string());
    }

    /// Record the text for a text step.
    pub fn set_text(&mut self, step_id: &str, text: String) {
        self.texts.insert(step_id.to_string(), text);
    }

    /// Choice ids in step order, for choice steps that have a value.
    pub fn path(&self) -> Vec<String> {
        self.step_ids()
            .iter()
            .filter(|id| self.taxonomy.step(id).is_some_and(|s| s.text.is_none()))
            .filter_map(|id| self.choices.get(id).cloned())
            .collect()
    }

    /// `choice.<id>` locale keys for each entry of `path()`, for the report title.
    pub fn choice_label_keys(&self) -> Vec<String> {
        self.path()
            .into_iter()
            .map(|id| format!("choice.{id}"))
            .collect()
    }

    /// First non-empty text step, as stored (wizard encode prefixes intact).
    pub(crate) fn encoded_body(&self) -> String {
        self.step_ids()
            .iter()
            .filter(|id| self.taxonomy.step(id).is_some_and(|s| s.text.is_some()))
            .filter_map(|id| self.texts.get(id))
            .find(|text| !text.is_empty())
            .cloned()
            .unwrap_or_default()
    }

    /// First non-empty text step as plaintext (wizard `%NL0P|` markup stripped).
    pub fn body(&self) -> String {
        strip_wizard_markup(&self.encoded_body())
    }

    /// Index (in step order) of the first step that carries a text rule.
    pub fn first_text_step(&self) -> Option<usize> {
        self.step_ids()
            .iter()
            .position(|id| self.taxonomy.step(id).is_some_and(|s| s.text.is_some()))
    }
}

/// About tab state, held as `MainState.feedback`.
#[derive(Debug, Clone, Default)]
pub struct FeedbackState {
    pub loaded: bool,
    /// A failed history load refuses writes for this session, even if the file
    /// later becomes readable. Reload the addon after repairing the file.
    pub history_load_error: Option<String>,
    pub messages: Vec<LocalMessage>,
    pub last_path: Option<LastPath>,
    /// Embedded or cached copy once `ensure_loaded` ran; a newer server copy replaces it when no draft is open.
    pub taxonomy: FeedbackTaxonomy,
    pub taxonomy_fetching: bool,
    /// Arrived while a draft was open; applied once the draft closes.
    pub pending_taxonomy: Option<FeedbackTaxonomy>,
    pub draft: Option<Draft>,
    pub view: AboutView,
    pub view_chosen: bool,
    /// report_id of the expanded row.
    pub expanded: Option<String>,
    /// report_id in flight.
    pub sending: Option<String>,
    pub refreshing: bool,
    pub last_refresh_at: Option<Instant>,
    pub last_refresh_ok: Option<bool>,
    pub last_poll: Option<Instant>,
    /// A successful send asks the next frame's `maybe_poll` for a refresh (the send
    /// thread must not spawn from inside `with_state`).
    pub refresh_requested: bool,
    pub account: Option<Result<String, ()>>,
    pub account_looking_up: bool,
    /// Built when the summary step opens.
    pub snapshot: Option<BuildSnapshot>,
    /// Messages need saving.
    pub dirty: bool,
    pub was_open: bool,
}

impl FeedbackState {
    /// Fold one frame of About-tab edits. Messages and taxonomy stay with
    /// `self` when a worker published them; the open draft is the player's.
    pub(crate) fn merge_paint(&mut self, base: &Self, paint: &Self) {
        self.draft = paint.draft.clone();
        self.view = paint.view;
        self.view_chosen = paint.view_chosen;
        self.expanded = paint.expanded.clone();
        self.dirty = paint.dirty;
        self.was_open = paint.was_open;
        self.snapshot = paint.snapshot.clone();
        crate::state::merge_busy(
            &mut self.taxonomy_fetching,
            base.taxonomy_fetching,
            paint.taxonomy_fetching,
        );
        crate::state::merge_busy(&mut self.refreshing, base.refreshing, paint.refreshing);
        crate::state::merge_busy(
            &mut self.account_looking_up,
            base.account_looking_up,
            paint.account_looking_up,
        );
        crate::state::merge_busy(
            &mut self.refresh_requested,
            base.refresh_requested,
            paint.refresh_requested,
        );
        crate::state::merge_busy_opt(&mut self.sending, &base.sending, &paint.sending);
        if self.messages.len() == base.messages.len() && paint.messages.len() != base.messages.len()
        {
            self.messages = paint.messages.clone();
        }
    }

    /// Open a fresh draft over a frozen copy of the current taxonomy.
    pub fn open_draft(&mut self) {
        self.draft = Some(Draft::new(self.taxonomy.clone()));
    }

    /// Drop the draft, then let a taxonomy that arrived meanwhile take over.
    pub fn close_draft(&mut self) {
        self.draft = None;
        self.apply_pending_taxonomy();
    }

    /// A newer taxonomy replaces the current one only when no draft is open; otherwise it waits
    /// in `pending_taxonomy`. Same or older versions are ignored.
    pub fn offer_taxonomy(&mut self, t: FeedbackTaxonomy) {
        if t.taxonomy_version <= self.taxonomy.taxonomy_version {
            return;
        }
        if self.draft.is_none() {
            self.taxonomy = t;
        } else if self
            .pending_taxonomy
            .as_ref()
            .is_none_or(|p| t.taxonomy_version > p.taxonomy_version)
        {
            self.pending_taxonomy = Some(t);
        }
    }

    /// Move `pending_taxonomy` into `taxonomy` if no draft is open and it is still newer.
    pub fn apply_pending_taxonomy(&mut self) {
        if self.draft.is_some() {
            return;
        }
        if let Some(pending) = self.pending_taxonomy.take() {
            if pending.taxonomy_version > self.taxonomy.taxonomy_version {
                self.taxonomy = pending;
            }
        }
    }

    /// Messages that went to the server (everything except `Local` rows).
    pub fn sent_count(&self) -> usize {
        self.messages.iter().filter(|m| !m.is_local()).count()
    }

    /// Messages the developer has answered.
    pub fn answered_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.status == MessageStatus::Answered)
            .count()
    }

    /// `Messages` once anything was sent, else `WhatsNew`.
    pub fn default_view(&self) -> AboutView {
        if self.sent_count() > 0 {
            AboutView::Messages
        } else {
            AboutView::WhatsNew
        }
    }

    /// Rebuild `snapshot` from the currently selected suggestion (`None` when there is none).
    /// Called when the summary step opens.
    pub fn refresh_snapshot(&mut self, comparison: &crate::ui::comparison::ComparisonState) {
        self.snapshot = comparison
            .suggestions
            .get(comparison.selected_suggestion)
            .map(snapshot_from);
    }

    /// True when a snapshot exists and fits the server cap (`MAX_SNAPSHOT_BYTES`).
    pub fn snapshot_attachable(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| snapshot_bytes(s) <= MAX_SNAPSHOT_BYTES)
    }
}

/// Slim allowlist of the last optimize result (design §6): names and the chat code, nothing
/// else. Explanations, rotation, combat profiles, and quality notes never cross this boundary.
pub fn snapshot_from(s: &crate::ui::comparison::BuildSuggestion) -> BuildSnapshot {
    BuildSnapshot {
        stat_prefix: s.stat_prefix.clone(),
        slot_prefixes: s.slot_prefixes.clone(),
        specializations: s.specializations.clone(),
        weapons: s.weapons.clone(),
        sigils: s.sigils.clone(),
        skills: s.skills.clone(),
        rune: s.rune.clone(),
        relic: s.relic.clone(),
        chat_code: s.chat_code.clone(),
    }
}

/// Whole minutes until a rate-limited row may be resent (`ceil` of the remaining seconds);
/// 0 once [`LocalMessage::resend_allowed`] holds or when the failure is not a rate limit.
pub fn minutes_left(m: &LocalMessage, now: u64) -> u64 {
    if m.status != MessageStatus::Failed || m.resend_allowed(now) {
        return 0;
    }
    match m.last_error {
        Some(FailReason::RateLimited { retry_after_secs }) => m
            .failed_at
            .unwrap_or(0)
            .saturating_add(retry_after_secs)
            .saturating_sub(now)
            .div_ceil(60),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taxonomy() -> FeedbackTaxonomy {
        FeedbackTaxonomy::embedded()
    }

    fn message(status: MessageStatus) -> LocalMessage {
        LocalMessage {
            report_id: uuid::Uuid::new_v4().to_string(),
            short_id: None,
            sent_at: 0,
            category: "bug".to_string(),
            path: Vec::new(),
            title: "t".to_string(),
            body: String::new(),
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

    fn bug_draft() -> Draft {
        let mut draft = Draft::new(taxonomy());
        draft.pick("bug");
        draft
    }

    #[test]
    fn new_draft_mints_uuid_v4_and_freezes_taxonomy() {
        let tax = taxonomy();
        let draft = Draft::new(tax.clone());
        let id = uuid::Uuid::parse_str(&draft.report_id).expect("report_id is a uuid");
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(draft.taxonomy, tax);
        assert_eq!(draft.step, WizardStep::Pick);
        assert_eq!(draft.category, None);
    }

    #[test]
    fn missing_steps_lists_required_steps_in_order() {
        let mut draft = bug_draft();
        assert_eq!(
            draft.missing_steps(),
            vec!["area_screen", "severity", "describe"]
        );

        draft.set_choice("area_screen", "optimize");
        assert_eq!(draft.missing_steps(), vec!["severity", "describe"]);

        let mut praise = Draft::new(taxonomy());
        praise.pick("praise");
        praise.set_choice("liked", "choya");
        assert!(praise.missing_steps().is_empty());
    }

    #[test]
    fn can_send_only_when_nothing_missing() {
        let mut draft = bug_draft();
        assert!(!draft.can_send());
        draft.set_choice("area_screen", "optimize");
        draft.set_choice("severity", "wrong");
        assert!(!draft.can_send());
        draft.set_text("describe", "123456789".to_string());
        assert!(!draft.can_send());
        draft.set_text("describe", "1234567890".to_string());
        assert!(draft.can_send());

        let mut coffee = Draft::new(taxonomy());
        coffee.pick("coffee");
        assert!(coffee.missing_steps().is_empty());
        assert!(!coffee.can_send());
    }

    #[test]
    fn optional_text_over_max_blocks_send() {
        let mut praise = Draft::new(taxonomy());
        praise.pick("praise");
        praise.set_choice("liked", "choya");
        assert!(praise.can_send(), "optional note may stay empty");
        praise.set_text("note_optional", "x".repeat(1001));
        assert!(!praise.can_send(), "over the taxonomy max must block Send");
        assert_eq!(praise.missing_steps(), vec!["note_optional".to_string()]);
        praise.set_text("note_optional", "x".repeat(1000));
        assert!(praise.can_send());
    }

    #[test]
    fn text_validation_min_max() {
        let mut draft = bug_draft();
        draft.set_text("describe", "x".repeat(9));
        assert_eq!(draft.text_error("describe"), Some(TextError::TooShort(10)));
        draft.set_text("describe", "x".repeat(4001));
        assert_eq!(draft.text_error("describe"), Some(TextError::TooLong(4000)));
        draft.set_text("describe", "x".repeat(10));
        assert_eq!(draft.text_error("describe"), None);
        draft.set_text("describe", "é".repeat(10));
        assert_eq!(draft.text_error("describe"), None);

        let mut praise = Draft::new(taxonomy());
        praise.pick("praise");
        praise.set_text("note_optional", String::new());
        assert_eq!(praise.text_error("note_optional"), None);
    }

    #[test]
    fn step_count_and_index() {
        let mut draft = bug_draft();
        assert_eq!(draft.total_steps(), 4);
        assert_eq!(draft.step, WizardStep::Step(0));
        assert_eq!(draft.current_index(), Some(1));
        draft.step = WizardStep::Summary;
        assert_eq!(draft.current_index(), Some(4));
        draft.step = WizardStep::Pick;
        assert_eq!(draft.current_index(), None);

        let fresh = Draft::new(taxonomy());
        assert_eq!(fresh.total_steps(), 1);
        assert!(fresh.step_ids().is_empty());
    }

    #[test]
    fn next_and_back_walk_the_steps() {
        let mut draft = Draft::new(taxonomy());
        draft.next();
        assert_eq!(draft.step, WizardStep::Pick);

        draft.pick("bug");
        assert_eq!(draft.step, WizardStep::Step(0));
        draft.next();
        assert_eq!(draft.step, WizardStep::Step(1));
        draft.next();
        assert_eq!(draft.step, WizardStep::Step(2));
        draft.next();
        assert_eq!(draft.step, WizardStep::Summary);
        draft.next();
        assert_eq!(draft.step, WizardStep::Summary);

        draft.back();
        assert_eq!(draft.step, WizardStep::Step(2));
        draft.back();
        assert_eq!(draft.step, WizardStep::Step(1));
        draft.back();
        assert_eq!(draft.step, WizardStep::Step(0));
        draft.back();
        assert_eq!(draft.step, WizardStep::Pick);
        assert_eq!(draft.category.as_deref(), Some("bug"));
        draft.back();
        assert_eq!(draft.step, WizardStep::Pick);
    }

    #[test]
    fn pick_resets_choices_and_texts() {
        let mut draft = bug_draft();
        draft.set_choice("area_screen", "optimize");
        draft.set_text("describe", "something went wrong".to_string());
        draft.pick("wish");
        assert!(draft.choices.is_empty());
        assert!(draft.texts.is_empty());
        assert_eq!(draft.step, WizardStep::Step(0));
        assert_eq!(draft.step_ids(), ["area_feature", "describe"]);
        assert_eq!(draft.first_text_step(), Some(1));

        draft.pick("coffee");
        assert_eq!(draft.step, WizardStep::Summary);
        assert_eq!(draft.first_text_step(), None);
    }

    #[test]
    fn path_and_body_follow_step_order() {
        let mut draft = bug_draft();
        draft.set_choice("severity", "wrong");
        draft.set_choice("area_screen", "optimize");
        assert_eq!(draft.path(), vec!["optimize", "wrong"]);
        assert_eq!(draft.body(), "");
        draft.set_text("describe", "Optimize picks Trident on land".to_string());
        assert_eq!(draft.body(), "Optimize picks Trident on land");

        let mut praise = Draft::new(taxonomy());
        praise.pick("praise");
        assert!(praise.path().is_empty());
        praise.set_text("note_optional", "nice".to_string());
        assert_eq!(praise.body(), "nice");
    }

    #[test]
    fn body_strips_wizard_encode_markup() {
        let mut draft = bug_draft();
        let encoded = "%NL0P|The optimizer picked a trident on land.";
        draft.set_text("describe", encoded.to_string());
        assert_eq!(draft.encoded_body(), encoded);
        assert_eq!(draft.body(), "The optimizer picked a trident on land.");
        assert!(!draft.body().contains("%NL0P|"));
    }

    #[test]
    fn same_as_last_prefills_choices_and_jumps_to_first_text_step() {
        let last = LastPath {
            category: "bug".to_string(),
            path: vec!["optimize".to_string(), "wrong".to_string()],
        };
        let draft = Draft::from_last_path(taxonomy(), &last);
        assert_eq!(draft.category.as_deref(), Some("bug"));
        assert_eq!(
            draft.choices.get("area_screen").map(String::as_str),
            Some("optimize")
        );
        assert_eq!(
            draft.choices.get("severity").map(String::as_str),
            Some("wrong")
        );
        assert_eq!(draft.step, WizardStep::Step(2));
        assert_eq!(draft.path(), vec!["optimize", "wrong"]);
        assert_eq!(
            draft.choice_label_keys(),
            vec!["choice.optimize", "choice.wrong"]
        );
        assert_eq!(draft.missing_steps(), vec!["describe"]);

        let short = LastPath {
            category: "bug".to_string(),
            path: vec!["optimize".to_string()],
        };
        let draft = Draft::from_last_path(taxonomy(), &short);
        assert_eq!(draft.step, WizardStep::Step(2));
        assert_eq!(draft.missing_steps(), vec!["severity", "describe"]);
    }

    #[test]
    fn draft_from_failed_row_mints_new_report_id_and_keeps_text() {
        let row = LocalMessage {
            path: vec!["optimize".to_string(), "wrong".to_string()],
            body: "Optimize picks Trident on land.".to_string(),
            last_error: Some(FailReason::TooLarge),
            failed_at: Some(1_000),
            ..message(MessageStatus::Failed)
        };
        let draft = Draft::from_failed(taxonomy(), &row);
        assert_ne!(draft.report_id, row.report_id);
        assert_eq!(
            uuid::Uuid::parse_str(&draft.report_id).map(|u| u.get_version_num()),
            Ok(4)
        );
        assert_eq!(draft.category.as_deref(), Some("bug"));
        assert_eq!(draft.path(), vec!["optimize", "wrong"]);
        assert_eq!(draft.body(), "Optimize picks Trident on land.");
        assert_eq!(
            draft.texts.get("describe").map(String::as_str),
            Some("Optimize picks Trident on land.")
        );
        // `describe` is the bug category's third step: the draft opens on it.
        assert_eq!(draft.step, WizardStep::Step(2));
        assert!(draft.missing_steps().is_empty());
        assert_eq!(draft.error, None);

        // An unknown category leaves the draft on Pick with nothing filled.
        let stray = LocalMessage {
            category: "vote".to_string(),
            ..row.clone()
        };
        let draft = Draft::from_failed(taxonomy(), &stray);
        assert_eq!(draft.category, None);
        assert_eq!(draft.step, WizardStep::Pick);
        assert!(draft.texts.is_empty());
    }

    #[test]
    fn interrupted_rows_offer_resend() {
        let now = 1_000;
        let row = LocalMessage {
            last_error: Some(FailReason::Interrupted),
            failed_at: Some(now),
            failed_payload: Some("{}".to_string()),
            ..message(MessageStatus::Failed)
        };
        assert!(row.resend_allowed(now));
        assert_eq!(minutes_left(&row, now), 0);
    }

    #[test]
    fn rate_limited_countdown_minutes() {
        let limited = |failed_at: u64| LocalMessage {
            last_error: Some(FailReason::RateLimited {
                retry_after_secs: 90,
            }),
            failed_at: Some(failed_at),
            ..message(MessageStatus::Failed)
        };
        assert_eq!(minutes_left(&limited(1_000), 1_000), 2);
        assert_eq!(minutes_left(&limited(1_000), 1_030), 1);
        assert_eq!(minutes_left(&limited(1_000), 1_089), 1);
        assert_eq!(minutes_left(&limited(1_000), 1_090), 0);
        assert_eq!(minutes_left(&limited(1_000), 5_000), 0);
        // Edit-only failures never count down.
        let too_large = LocalMessage {
            last_error: Some(FailReason::TooLarge),
            failed_at: Some(1_000),
            ..message(MessageStatus::Failed)
        };
        assert_eq!(minutes_left(&too_large, 1_000), 0);
        // A rate limit on a row that is no longer Failed is moot.
        let sent = LocalMessage {
            status: MessageStatus::Received,
            ..limited(1_000)
        };
        assert_eq!(minutes_left(&sent, 1_000), 0);
    }

    #[test]
    fn pending_taxonomy_applies_only_when_no_draft() {
        let v1 = taxonomy();
        let mut v2 = taxonomy();
        v2.taxonomy_version = 2;

        let mut state = FeedbackState {
            taxonomy: v1.clone(),
            ..Default::default()
        };
        state.open_draft();
        assert!(state.draft.is_some());
        state.offer_taxonomy(v2.clone());
        assert_eq!(state.taxonomy.taxonomy_version, 1);
        assert_eq!(state.pending_taxonomy, Some(v2.clone()));

        state.close_draft();
        assert!(state.draft.is_none());
        assert_eq!(state.taxonomy, v2);
        assert_eq!(state.pending_taxonomy, None);

        state.offer_taxonomy(v1);
        assert_eq!(state.taxonomy.taxonomy_version, 2);
        assert_eq!(state.pending_taxonomy, None);

        let mut v3 = taxonomy();
        v3.taxonomy_version = 3;
        state.offer_taxonomy(v3.clone());
        assert_eq!(state.taxonomy, v3);
        assert_eq!(state.pending_taxonomy, None);
    }

    #[test]
    fn older_or_equal_taxonomy_is_ignored() {
        let mut v2 = taxonomy();
        v2.taxonomy_version = 2;
        let mut state = FeedbackState {
            taxonomy: v2.clone(),
            ..Default::default()
        };

        // No draft: neither an equal nor an older version replaces the one in use.
        state.offer_taxonomy(v2.clone());
        state.offer_taxonomy(taxonomy());
        assert_eq!(state.taxonomy, v2);
        assert_eq!(state.pending_taxonomy, None);

        // Draft open: they are not queued either.
        state.open_draft();
        state.offer_taxonomy(v2.clone());
        state.offer_taxonomy(taxonomy());
        assert_eq!(state.pending_taxonomy, None);
        state.close_draft();
        assert_eq!(state.taxonomy, v2);
    }

    #[test]
    fn default_view_and_counts() {
        let mut state = FeedbackState::default();
        assert_eq!(state.default_view(), AboutView::WhatsNew);
        assert_eq!(state.sent_count(), 0);
        assert_eq!(state.answered_count(), 0);

        state.messages.push(message(MessageStatus::Local));
        assert_eq!(state.default_view(), AboutView::WhatsNew);
        assert_eq!(state.sent_count(), 0);

        state.messages.push(message(MessageStatus::Received));
        assert_eq!(state.default_view(), AboutView::Messages);
        assert_eq!(state.sent_count(), 1);
        assert_eq!(state.answered_count(), 0);

        state.messages.push(message(MessageStatus::Answered));
        assert_eq!(state.sent_count(), 2);
        assert_eq!(state.answered_count(), 1);
    }

    // T022 — build snapshot allowlist.

    /// A suggestion whose non-allowlisted fields all carry bait text.
    fn suggestion(stat_prefix: &str) -> crate::ui::comparison::BuildSuggestion {
        crate::ui::comparison::BuildSuggestion {
            label: "NOT-IN-SNAPSHOT label".to_string(),
            build_summary: "NOT-IN-SNAPSHOT summary".to_string(),
            stat_prefix: stat_prefix.to_string(),
            slot_prefixes: {
                let mut slots = gw2_core::types::GearSlots::default();
                slots.set(
                    gw2_core::types::GearSlot::Amulet,
                    gw2_core::types::PrefixRef {
                        itemstat_id: 1,
                        name: "Berserker".into(),
                    },
                );
                slots.set(
                    gw2_core::types::GearSlot::Helm,
                    gw2_core::types::PrefixRef {
                        itemstat_id: 1,
                        name: "Marauder".into(),
                    },
                );
                Some(slots)
            },
            specializations: vec![(
                "Skirmishing".to_string(),
                vec!["Sharpened Edges".to_string()],
            )],
            weapons: vec!["Hammer".to_string()],
            skills: vec!["Troll Unguent".to_string()],
            rune: "Scholar".to_string(),
            sigils: vec!["Force".to_string()],
            relic: "Thief".to_string(),
            chat_code: Some("[&DQQ...]".to_string()),
            explanation: "NOT-IN-SNAPSHOT explanation".to_string(),
            synergy_explanation: "NOT-IN-SNAPSHOT synergy".to_string(),
            changes_made: vec!["NOT-IN-SNAPSHOT change".to_string()],
            estimated_stats: Some(Default::default()),
            combat_solo: Some(Default::default()),
            combat_party: Some(Default::default()),
            combat_squad: Some(Default::default()),
            rotation: Some(Default::default()),
            quality_reasons: vec!["NOT-IN-SNAPSHOT reason".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_from_suggestion_contains_only_allowlist() {
        let snap = snapshot_from(&suggestion("Marauder"));
        let json = serde_json::to_string(&snap).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys: std::collections::BTreeSet<String> =
            v.as_object().unwrap().keys().cloned().collect();
        let want: std::collections::BTreeSet<String> = [
            "stat_prefix",
            "slot_prefixes",
            "specializations",
            "weapons",
            "sigils",
            "skills",
            "rune",
            "relic",
            "chat_code",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(keys, want);
        assert!(!json.contains("NOT-IN-SNAPSHOT"), "{json}");

        assert_eq!(snap.stat_prefix, "Marauder");
        assert_eq!(
            snap.slot_prefixes
                .as_ref()
                .and_then(|s| s.get(gw2_core::types::GearSlot::Amulet))
                .map(|p| p.name.as_str()),
            Some("Berserker")
        );
        assert_eq!(snap.specializations[0].0, "Skirmishing");
        assert_eq!(snap.weapons, vec!["Hammer".to_string()]);
        assert_eq!(snap.sigils, vec!["Force".to_string()]);
        assert_eq!(snap.skills, vec!["Troll Unguent".to_string()]);
        assert_eq!(snap.rune, "Scholar");
        assert_eq!(snap.relic, "Thief");
        assert_eq!(snap.chat_code.as_deref(), Some("[&DQQ...]"));
    }

    #[test]
    fn snapshot_over_6144_bytes_is_not_attachable() {
        let mut state = FeedbackState::default();
        assert!(!state.snapshot_attachable());

        state.snapshot = Some(snapshot_from(&suggestion("Marauder")));
        assert!(state.snapshot_attachable());

        let mut big = suggestion("Marauder");
        big.relic = "x".repeat(7000);
        state.snapshot = Some(snapshot_from(&big));
        assert!(!state.snapshot_attachable());
    }

    #[test]
    fn refresh_snapshot_uses_selected_suggestion() {
        let mut comparison = crate::ui::comparison::ComparisonState::default();
        comparison.suggestions.push(suggestion("Marauder"));
        comparison.suggestions.push(suggestion("Berserker"));
        comparison.selected_suggestion = 1;

        let mut state = FeedbackState::default();
        state.refresh_snapshot(&comparison);
        assert_eq!(
            state.snapshot.as_ref().map(|s| s.stat_prefix.as_str()),
            Some("Berserker")
        );

        comparison.suggestions.clear();
        state.refresh_snapshot(&comparison);
        assert_eq!(state.snapshot, None);
    }
}
