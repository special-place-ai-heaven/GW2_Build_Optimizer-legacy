//! Model prices as data (`data/llm_pricing.json`) and the per-run cost estimate.
//!
//! Every row cites the official page it was read from. A model with no row
//! has no estimate: a guessed price shown as a number would read as fact.

use std::sync::OnceLock;

use gw2_core::config::LlmProvider;
use gw2_core::generations::{PricingSource, TokenUsage};
use serde::Deserialize;

use super::usage::RunUsage;

const PRICING_JSON: &str = include_str!("../../../../data/llm_pricing.json");

/// Row id recorded when the cost came from the provider's own `usage.cost`.
pub const REPORTED_COST_ROW: &str = "provider:usage.cost";
/// Row id recorded when only some responses reported a cost and no table
/// row prices the rest: the cost is unknown, and the tooltip says why.
pub const PARTIAL_COST_ROW: &str = "provider:usage.cost:partial";

#[derive(Debug, Clone, Deserialize)]
pub struct PricingRow {
    pub id: String,
    pub provider: LlmProvider,
    /// Exact id, `prefix*` or `*suffix`.
    pub model: String,
    /// Display name; empty for wildcard rows.
    #[serde(default)]
    pub label: String,
    pub input_usd_per_million: f64,
    pub output_usd_per_million: f64,
    pub source_url: String,
    pub as_of: String,
    #[serde(default)]
    pub note: String,
}

impl PricingRow {
    pub fn cost_usd(&self, tokens: &TokenUsage) -> f64 {
        (tokens.prompt as f64 * self.input_usd_per_million
            + tokens.completion as f64 * self.output_usd_per_million)
            / 1_000_000.0
    }

    /// How specifically this row names `model`: exact beats any wildcard,
    /// a longer wildcard beats a shorter one. `None` when it does not match.
    fn specificity(&self, model: &str) -> Option<usize> {
        if let Some(prefix) = self.model.strip_suffix('*') {
            model.starts_with(prefix).then_some(prefix.len())
        } else if let Some(suffix) = self.model.strip_prefix('*') {
            model.ends_with(suffix).then_some(suffix.len())
        } else {
            (self.model == model).then_some(usize::MAX)
        }
    }
}

/// ECB reference rate for the Settings currency toggle. `USD` amounts are
/// always what gets stored; this only converts what gets displayed.
#[derive(Debug, Clone, Deserialize)]
pub struct FxRate {
    pub eur_per_usd: f64,
    pub as_of: String,
    pub source_url: String,
}

#[derive(Deserialize)]
struct PricingFile {
    rows: Vec<PricingRow>,
    fx: FxRate,
}

/// The bundled table. Empty if the file does not parse (a test pins that it does).
pub fn table() -> &'static [PricingRow] {
    &bundled().rows
}

/// EUR-per-USD conversion for the Settings currency toggle. Falls back to a
/// stale-but-labeled rate if the bundled JSON ever fails to parse (the
/// `the_bundled_table_parses_and_every_row_cites_a_page` test pins that it does).
pub fn fx() -> &'static FxRate {
    &bundled().fx
}

struct Bundled {
    rows: Vec<PricingRow>,
    fx: FxRate,
}

fn bundled() -> &'static Bundled {
    static BUNDLED: OnceLock<Bundled> = OnceLock::new();
    BUNDLED.get_or_init(|| match serde_json::from_str::<PricingFile>(PRICING_JSON) {
        Ok(f) => Bundled {
            rows: f.rows,
            fx: f.fx,
        },
        Err(_) => Bundled {
            rows: Vec::new(),
            fx: FxRate {
                eur_per_usd: 0.92,
                as_of: String::new(),
                source_url: String::new(),
            },
        },
    })
}

/// The most specific row for `model` under `provider`.
pub fn lookup_in<'a>(
    rows: &'a [PricingRow],
    provider: &LlmProvider,
    model: &str,
) -> Option<&'a PricingRow> {
    rows.iter()
        .filter(|row| &row.provider == provider)
        .filter_map(|row| row.specificity(model).map(|s| (s, row)))
        .max_by_key(|(s, _)| *s)
        .map(|(_, row)| row)
}

pub fn lookup(provider: &LlmProvider, model: &str) -> Option<&'static PricingRow> {
    lookup_in(table(), provider, model)
}

/// A cost estimate and where its price came from. `usd` is `None` only for
/// [`PARTIAL_COST_ROW`]: the source is known, the total is not.
#[derive(Debug, Clone, PartialEq)]
pub struct CostEstimate {
    pub usd: Option<f64>,
    pub source: PricingSource,
}

/// Cost of a run: the provider's own reported cost when every response sent
/// one (OpenRouter), else tokens times the table price, else `None`. When
/// only some responses reported a cost, the rest are priced from the table
/// (row id `provider:usage.cost+<row>`), or the total is unknown
/// ([`PARTIAL_COST_ROW`]): a partial sum is never shown as the whole.
/// `today` dates a reported cost (`YYYY-MM-DD`).
pub fn estimate(
    provider: &LlmProvider,
    model: &str,
    run: &RunUsage,
    today: &str,
) -> Option<CostEstimate> {
    let row = lookup(provider, model);
    match (run.reported_cost_usd, row) {
        (Some(usd), _) if run.uncosted.requests == 0 => Some(CostEstimate {
            usd: Some(usd),
            source: PricingSource {
                row_id: REPORTED_COST_ROW.to_string(),
                as_of: today.to_string(),
            },
        }),
        (Some(usd), Some(row)) => Some(CostEstimate {
            usd: Some(usd + row.cost_usd(&run.uncosted)),
            source: PricingSource {
                row_id: format!("{REPORTED_COST_ROW}+{}", row.id),
                as_of: row.as_of.clone(),
            },
        }),
        (Some(_), None) => Some(CostEstimate {
            usd: None,
            source: PricingSource {
                row_id: PARTIAL_COST_ROW.to_string(),
                as_of: today.to_string(),
            },
        }),
        (None, row) => row.map(|row| CostEstimate {
            usd: Some(row.cost_usd(&run.tokens)),
            source: PricingSource {
                row_id: row.id.clone(),
                as_of: row.as_of.clone(),
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, provider: LlmProvider, model: &str, input: f64, output: f64) -> PricingRow {
        PricingRow {
            id: id.into(),
            provider,
            model: model.into(),
            label: String::new(),
            input_usd_per_million: input,
            output_usd_per_million: output,
            source_url: "https://example.invalid".into(),
            as_of: "2026-09-24".into(),
            note: String::new(),
        }
    }

    #[test]
    fn exact_beats_prefix_beats_nothing() {
        let rows = vec![
            row("flash", LlmProvider::Gemini, "gemini-2.5-flash", 0.30, 2.50),
            row("flash*", LlmProvider::Gemini, "gemini-2.5-flash*", 9.0, 9.0),
            row(
                "haiku",
                LlmProvider::Anthropic,
                "claude-haiku-4-5*",
                1.0,
                5.0,
            ),
            row("free", LlmProvider::OpenRouter, "*:free", 0.0, 0.0),
        ];
        let id = |p: LlmProvider, m: &str| lookup_in(&rows, &p, m).map(|r| r.id.as_str());
        assert_eq!(
            id(LlmProvider::Gemini, "gemini-2.5-flash"),
            Some("flash"),
            "exact"
        );
        assert_eq!(
            id(LlmProvider::Gemini, "gemini-2.5-flash-preview-09"),
            Some("flash*"),
            "prefix"
        );
        assert_eq!(
            id(LlmProvider::Anthropic, "claude-haiku-4-5-20251001"),
            Some("haiku")
        );
        assert_eq!(
            id(LlmProvider::OpenRouter, "z-ai/glm-5.2:free"),
            Some("free")
        );
        assert_eq!(id(LlmProvider::Gemini, "gemini-9-ultra"), None, "unknown");
        assert_eq!(
            id(LlmProvider::OpenAI, "gemini-2.5-flash"),
            None,
            "another provider's row never prices this one"
        );
    }

    #[test]
    fn cost_is_tokens_times_price() {
        let r = row("x", LlmProvider::Gemini, "m", 0.30, 2.50);
        let tokens = TokenUsage {
            prompt: 40_000,
            completion: 1_200,
            total: 41_200,
            requests: 6,
        };
        // 40k * 0.30/1M + 1.2k * 2.50/1M = 0.012 + 0.003
        assert!((r.cost_usd(&tokens) - 0.015).abs() < 1e-12);
    }

    #[test]
    fn estimate_prefers_the_providers_reported_cost_then_the_table() {
        let run = RunUsage {
            tokens: TokenUsage {
                prompt: 1_000_000,
                completion: 0,
                total: 1_000_000,
                requests: 1,
            },
            ..RunUsage::default()
        };
        let est =
            estimate(&LlmProvider::Gemini, "gemini-2.5-flash", &run, "2026-09-24").expect("priced");
        assert!((est.usd.expect("usd") - 0.30).abs() < 1e-9);
        assert_eq!(est.source.row_id, "gemini:gemini-2.5-flash");

        let reported = RunUsage {
            reported_cost_usd: Some(0.0421),
            ..run
        };
        let est = estimate(
            &LlmProvider::OpenRouter,
            "anthropic/claude-sonnet-4.5",
            &reported,
            "2026-09-24",
        )
        .expect("reported");
        assert_eq!(est.usd, Some(0.0421));
        assert_eq!(est.source.row_id, REPORTED_COST_ROW);

        assert_eq!(
            estimate(
                &LlmProvider::OpenRouter,
                "anthropic/claude-sonnet-4.5",
                &run,
                "2026-09-24"
            ),
            None,
            "a paid OpenRouter model without a reported cost is unknown, not guessed"
        );
        let free = estimate(
            &LlmProvider::OpenRouter,
            "z-ai/glm-5.2:free",
            &run,
            "2026-09-24",
        )
        .expect("free is known");
        assert_eq!(free.usd, Some(0.0));
    }

    /// Two requests, one reported cost: the reported sum is not the run's
    /// cost. The other request is priced from the table when a row exists,
    /// else the total is unknown and says why.
    #[test]
    fn a_partly_reported_cost_is_never_shown_as_the_whole() {
        let half = RunUsage {
            tokens: TokenUsage {
                prompt: 2_000_000,
                completion: 0,
                total: 2_000_000,
                requests: 2,
            },
            reported_cost_usd: Some(0.10),
            uncosted: TokenUsage {
                prompt: 1_000_000,
                completion: 0,
                total: 1_000_000,
                requests: 1,
            },
            ..RunUsage::default()
        };
        let unpriced = estimate(
            &LlmProvider::OpenRouter,
            "anthropic/claude-sonnet-4.5",
            &half,
            "2026-09-24",
        )
        .expect("the source is still named");
        assert_eq!(unpriced.usd, None, "not the partial 0.10");
        assert_eq!(unpriced.source.row_id, PARTIAL_COST_ROW);

        let mixed = estimate(
            &LlmProvider::Gemini,
            "gemini-2.5-flash",
            &half,
            "2026-09-24",
        )
        .expect("priced");
        // 0.10 reported + 1M prompt tokens at 0.30 / 1M.
        assert!((mixed.usd.expect("usd") - 0.40).abs() < 1e-9);
        assert_eq!(
            mixed.source.row_id,
            format!("{REPORTED_COST_ROW}+gemini:gemini-2.5-flash")
        );
    }

    #[test]
    fn the_bundled_table_parses_and_every_row_cites_a_page() {
        let rows = table();
        assert!(rows.len() >= 20, "table did not parse: {} rows", rows.len());
        let mut ids = std::collections::HashSet::new();
        for r in rows {
            assert!(ids.insert(r.id.as_str()), "duplicate row id {}", r.id);
            assert!(
                r.source_url.starts_with("https://"),
                "{} has no source",
                r.id
            );
            assert_eq!(r.as_of.len(), 10, "{} as_of is not a date", r.id);
            assert!(r.input_usd_per_million >= 0.0 && r.output_usd_per_million >= 0.0);
        }
        // Every model the Settings tab offers offline is priced or explicitly
        // unknown; the defaults must be priced.
        for (provider, model) in [
            (LlmProvider::Gemini, gw2_core::config::DEFAULT_GEMINI_MODEL),
            (LlmProvider::OpenAI, gw2_core::config::DEFAULT_OPENAI_MODEL),
            (
                LlmProvider::Anthropic,
                gw2_core::config::DEFAULT_ANTHROPIC_MODEL,
            ),
        ] {
            assert!(lookup(&provider, model).is_some(), "{model} unpriced");
        }
    }
}
