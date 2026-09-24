//! The handshake: find out what a model can do before asking it for a build,
//! keep what was learned, shape every run from it, refine after each run.
//!
//! Every chatbot people use has this solved so well nobody thinks about it.
//! This addon instead learned each model's quirks one failed run at a time,
//! in the player's game: a reasoning model that thinks for minutes on the
//! closing request, a free model that answers the plate in prose, one that
//! narrates its plan and calls nothing. Each got a rule after the fact.
//! [`probe`] asks the model to do a tiny version of the real job up front —
//! call one tool, then answer in strict JSON — and measures what came back.
//! [`ModelProfile`] is what it learned; [`Store`] keeps one per model id on
//! disk next to the usage counters; [`ModelProfile::record_run`] folds each
//! real run back in, so the second run on a model is shaped by the first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{LlmClient, LlmError, ToolDefinition};

/// How well the model drives our tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolSupport {
    /// Called the probe tool with the arguments asked for.
    Native,
    /// Called it, but not as asked (wrong arguments, or twice).
    Sloppy,
    /// Never called it, or the provider reported it cannot.
    None,
}

/// How well the model obeys "reply with exactly this JSON".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonDiscipline {
    /// The reply parsed as the object asked for.
    Strict,
    /// The object was in there, wrapped in prose or a code fence.
    Wrapped,
    /// No usable object.
    Prose,
}

/// What one model can do, as measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub model: String,
    pub tools: ToolSupport,
    pub json: JsonDiscipline,
    /// Text arrived alongside the tool call: the model talks while it works.
    pub narrates: bool,
    /// Wall clock of the probe exchange — one tool round plus a short reply.
    pub probe_ms: u32,
    /// Real runs folded in since the probe.
    pub runs: u32,
    /// Of those runs, how many needed the prose→plate repair request.
    pub repairs: u32,
    /// Of those runs, how many ended in an error the fallback build covered.
    pub failures: u32,
    /// Mean seconds one tool round took across real runs.
    pub round_secs: f32,
    /// Unix seconds of the probe, so a stale profile is redone.
    pub probed_at: u64,
}

/// A profile older than this is probed again: models get updated, routes
/// change, and a free tier's speed on Tuesday says little about Friday.
const STALE_AFTER_SECS: u64 = 7 * 24 * 3600;

/// Lookup rounds when requests are scarce. See [`ModelProfile::max_turns`].
pub const THRIFTY_TURNS: usize = 2;

/// Turn budgets the profile hands the run.
impl ModelProfile {
    /// What to assume of a model the probe could not reach: the full job,
    /// with `probed_at: 0` so the next run probes again.
    pub fn assumed(model: &str) -> ModelProfile {
        ModelProfile {
            model: model.to_string(),
            tools: ToolSupport::Native,
            json: JsonDiscipline::Wrapped,
            narrates: false,
            probe_ms: 0,
            runs: 0,
            repairs: 0,
            failures: 0,
            round_secs: 10.0,
            probed_at: 0,
        }
    }

    /// How many tool rounds to allow. A model that cannot drive tools gets
    /// none; a slow one gets fewer, so the whole run still ends in a plate
    /// inside the tool-phase budget.
    ///
    /// `thrifty` is the client's word that requests are scarce (a free
    /// OpenRouter model, a Gemini key on a five-a-minute tier): two lookup
    /// rounds, then the plate, so a run is about three requests.
    pub fn max_turns(&self, thrifty: bool) -> usize {
        let unhurried = match self.tools {
            ToolSupport::None => 0,
            _ if self.round_secs > 40.0 || self.probe_ms > 40_000 => 4,
            _ if self.round_secs > 20.0 || self.probe_ms > 20_000 => 6,
            _ => 8,
        };
        if thrifty {
            unhurried.min(THRIFTY_TURNS)
        } else {
            unhurried
        }
    }

    /// Whether the run should expect to repair prose into a plate.
    pub fn expect_repair(&self) -> bool {
        self.json != JsonDiscipline::Strict || (self.runs > 0 && self.repairs * 2 >= self.runs)
    }

    /// One sentence for the log and the player.
    pub fn summary(&self) -> String {
        format!(
            "{}: tools {}, JSON {}, {}probe {:.1}s, {} run(s), {} repair(s), {} failure(s), {:.1}s/round",
            self.model,
            match self.tools {
                ToolSupport::Native => "native",
                ToolSupport::Sloppy => "sloppy",
                ToolSupport::None => "none",
            },
            match self.json {
                JsonDiscipline::Strict => "strict",
                JsonDiscipline::Wrapped => "wrapped",
                JsonDiscipline::Prose => "prose",
            },
            if self.narrates { "narrates, " } else { "" },
            self.probe_ms as f32 / 1000.0,
            self.runs,
            self.repairs,
            self.failures,
            self.round_secs
        )
    }

    /// Fold a real run back in.
    pub fn record_run(&mut self, round_secs: &[f32], repaired: bool, failed: bool) {
        let n = self.runs as f32;
        if !round_secs.is_empty() {
            let mean = round_secs.iter().sum::<f32>() / round_secs.len() as f32;
            self.round_secs = if self.runs == 0 {
                mean
            } else {
                (self.round_secs * n + mean) / (n + 1.0)
            };
        }
        self.runs += 1;
        if repaired {
            self.repairs += 1;
        }
        if failed {
            self.failures += 1;
        }
    }

    pub fn is_stale(&self, now_secs: u64) -> bool {
        now_secs.saturating_sub(self.probed_at) > STALE_AFTER_SECS
    }
}

const PROBE_TOOL: &str = "ping";
const PROBE_ECHO: &str = "handshake";

fn probe_tool() -> ToolDefinition {
    ToolDefinition {
        name: PROBE_TOOL.to_string(),
        description: "Connectivity check. Echoes the value back.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "echo": { "type": "string" } },
            "required": ["echo"]
        }),
    }
}

const PROBE_PROMPT: &str = "This is a connectivity check. Call the `ping` tool \
     once with {\"echo\": \"handshake\"}. When its result arrives, reply with \
     exactly this JSON object and nothing else: {\"ok\": true, \"echo\": \"handshake\"}";

/// One tiny exchange that exercises everything the real job needs.
///
/// `Err` when the provider never answered at all (a guardrail 404, an
/// upstream 429, a timeout) and no tool was called: that measures the route
/// today, not the model, and the caller must not keep it.
pub fn probe(client: &dyn LlmClient, model: &str, now_secs: u64) -> Result<ModelProfile, LlmError> {
    let started = Instant::now();
    let mut calls: Vec<Value> = Vec::new();
    let tools = [probe_tool()];
    let outcome = client.generate_with_tools_progress(
        PROBE_PROMPT,
        &tools,
        &mut |name, args| {
            calls.push(json!({ "name": name, "args": args }));
            json!({ "echo": args.get("echo").cloned().unwrap_or(Value::Null) })
        },
        2,
        &mut |_, _, names| {
            // Progress fires per tool round; text beside a call is not
            // visible here, so narration is judged from the reply below.
            let _ = names;
        },
    );
    let probe_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
    let outcome = match outcome {
        Err(e) if calls.is_empty() => return Err(e),
        other => other,
    };

    let tools = match calls.as_slice() {
        [] => ToolSupport::None,
        [one] if one["name"] == PROBE_TOOL && one["args"]["echo"] == PROBE_ECHO => {
            ToolSupport::Native
        }
        _ => ToolSupport::Sloppy,
    };
    let (json, narrates) = match &outcome {
        Ok(text) => classify_json_reply(text),
        Err(_) => (JsonDiscipline::Prose, false),
    };
    Ok(ModelProfile {
        model: model.to_string(),
        tools,
        json,
        narrates,
        probe_ms,
        runs: 0,
        repairs: 0,
        failures: 0,
        round_secs: probe_ms as f32 / 1000.0,
        probed_at: now_secs,
    })
}

/// Strict when the whole reply is the object; wrapped when the object is in
/// there with words around it; prose otherwise. The words count as narration.
fn classify_json_reply(text: &str) -> (JsonDiscipline, bool) {
    let trimmed = text.trim();
    let is_expected = |v: &Value| v.get("ok") == Some(&Value::Bool(true));
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if is_expected(&v) {
            return (JsonDiscipline::Strict, false);
        }
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if start < end {
            if let Ok(v) = serde_json::from_str::<Value>(&trimmed[start..=end]) {
                if is_expected(&v) {
                    return (JsonDiscipline::Wrapped, true);
                }
            }
        }
    }
    (JsonDiscipline::Prose, !trimmed.is_empty())
}

/// One profile per model id, on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub profiles: HashMap<String, ModelProfile>,
    #[serde(skip)]
    path: Option<PathBuf>,
    /// Why the last `ensure` could not probe, for the caller's log.
    #[serde(skip)]
    pub last_probe_error: Option<String>,
}

impl Store {
    pub fn load(addon_dir: &Path) -> Store {
        let path = addon_dir.join("model_profiles.json");
        let mut store: Store = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        store.path = Some(path);
        store
    }

    pub fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    pub fn get(&self, model: &str) -> Option<&ModelProfile> {
        self.profiles.get(model)
    }

    /// The profile for `model`, probing when there is none or it is stale.
    /// Returns whether a probe ran, so the caller can say so.
    pub fn ensure(
        &mut self,
        client: &dyn LlmClient,
        model: &str,
        now_secs: u64,
    ) -> (ModelProfile, bool) {
        if let Some(p) = self.profiles.get(model) {
            if !p.is_stale(now_secs) {
                return (p.clone(), false);
            }
        }
        match probe(client, model, now_secs) {
            Ok(p) => {
                self.last_probe_error = None;
                self.profiles.insert(model.to_string(), p.clone());
                self.save();
                (p, true)
            }
            // Kept out of the store: whatever is on file, stale or nothing,
            // beats a week of a measurement that never happened. In-game
            // 2026-09-07 a guardrail 404 and an upstream 429 were both saved
            // as "no tools, prose". The run goes ahead on the assumed
            // profile, so a model behind a passing 429 still gets its turn.
            Err(e) => {
                self.last_probe_error = Some(e.to_string());
                let p = self
                    .profiles
                    .get(model)
                    .cloned()
                    .unwrap_or_else(|| ModelProfile::assumed(model));
                (p, true)
            }
        }
    }

    pub fn record_run(&mut self, model: &str, round_secs: &[f32], repaired: bool, failed: bool) {
        if let Some(p) = self.profiles.get_mut(model) {
            p.record_run(round_secs, repaired, failed);
            self.save();
        }
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(tools: ToolSupport, json: JsonDiscipline) -> ModelProfile {
        ModelProfile {
            model: "test".into(),
            tools,
            json,
            narrates: false,
            probe_ms: 3_000,
            runs: 0,
            repairs: 0,
            failures: 0,
            round_secs: 3.0,
            probed_at: 0,
        }
    }

    #[test]
    fn json_discipline_reads_the_three_kinds_of_reply() {
        assert_eq!(
            classify_json_reply("{\"ok\": true, \"echo\": \"handshake\"}"),
            (JsonDiscipline::Strict, false)
        );
        assert_eq!(
            classify_json_reply("Sure! Here you go:\n```json\n{\"ok\": true}\n```"),
            (JsonDiscipline::Wrapped, true)
        );
        assert_eq!(
            classify_json_reply("I'll start by calling the ping tool."),
            (JsonDiscipline::Prose, true)
        );
    }

    #[test]
    fn a_slow_or_toolless_model_gets_fewer_rounds() {
        assert_eq!(
            profile(ToolSupport::Native, JsonDiscipline::Strict).max_turns(false),
            8
        );
        assert_eq!(
            profile(ToolSupport::Native, JsonDiscipline::Strict).max_turns(true),
            THRIFTY_TURNS,
            "scarce requests: two lookups then the plate"
        );
        assert_eq!(
            profile(ToolSupport::None, JsonDiscipline::Strict).max_turns(true),
            0,
            "thrifty never grants rounds a toolless model cannot use"
        );
        let mut slow = profile(ToolSupport::Native, JsonDiscipline::Strict);
        slow.round_secs = 45.0;
        assert_eq!(slow.max_turns(false), 4);
    }

    #[test]
    fn runs_refine_the_profile() {
        let mut p = profile(ToolSupport::Native, JsonDiscipline::Strict);
        p.record_run(&[10.0, 20.0], true, false);
        p.record_run(&[30.0], false, false);
        assert_eq!(p.runs, 2);
        assert_eq!(p.repairs, 1);
        assert!((p.round_secs - 22.5).abs() < 1e-3);
        assert!(p.expect_repair(), "one repair in two runs is a pattern");
    }

    #[test]
    fn a_week_old_profile_is_stale() {
        let p = profile(ToolSupport::Native, JsonDiscipline::Strict);
        assert!(!p.is_stale(STALE_AFTER_SECS - 1));
        assert!(p.is_stale(STALE_AFTER_SECS + 1));
    }

    #[test]
    fn the_store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("gw2bo_profiles_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = Store::load(&dir);
        store.profiles.insert(
            "m".into(),
            profile(ToolSupport::Sloppy, JsonDiscipline::Wrapped),
        );
        store.save();
        let again = Store::load(&dir);
        assert_eq!(again.get("m").unwrap().tools, ToolSupport::Sloppy);
        std::fs::remove_dir_all(&dir).ok();
    }
}
