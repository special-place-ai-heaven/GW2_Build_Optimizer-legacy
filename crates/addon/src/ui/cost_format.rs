//! Cost display in USD or EUR. Records always store USD; this only converts
//! what gets shown, so the Generations tab and the generation pill can share
//! one rule for "cost n/a" / "< $0.01" / "≈ $0.02".

use gw2_core::config::CostCurrency;
use gw2_core::i18n::t;
use gw2_optimizer::llm::pricing::FxRate;

/// "cost n/a", "< $0.01" / "< €0.01" below half a cent, else "≈ $0.02" / "≈ €0.02".
pub fn format_cost(usd: Option<f64>, currency: CostCurrency, fx: &FxRate) -> String {
    let Some(usd) = usd else {
        return t("gen.cost_na");
    };
    let symbol = match currency {
        CostCurrency::Usd => "$",
        CostCurrency::Eur => "\u{20ac}",
    };
    let value = match currency {
        CostCurrency::Usd => usd,
        CostCurrency::Eur => usd * fx.eur_per_usd,
    };
    if value <= 0.0 {
        format!("{symbol}0")
    } else if value < 0.005 {
        format!("< {symbol}0.01")
    } else {
        format!("\u{2248} {symbol}{value:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx() -> FxRate {
        FxRate {
            eur_per_usd: 0.88,
            as_of: "2026-09-24".into(),
            source_url: "https://example.invalid".into(),
        }
    }

    #[test]
    fn none_is_cost_na() {
        assert_eq!(
            format_cost(None, CostCurrency::Usd, &fx()),
            t("gen.cost_na")
        );
    }

    #[test]
    fn usd_formats_with_dollar_sign() {
        assert_eq!(
            format_cost(Some(0.02), CostCurrency::Usd, &fx()),
            "\u{2248} $0.02"
        );
    }

    #[test]
    fn eur_converts_and_formats_with_euro_sign() {
        // 0.02 usd * 0.88 = 0.0176 -> rounds to 0.02
        assert_eq!(
            format_cost(Some(0.02), CostCurrency::Eur, &fx()),
            "\u{2248} \u{20ac}0.02"
        );
    }

    #[test]
    fn small_values_show_the_less_than_form() {
        assert_eq!(
            format_cost(Some(0.003), CostCurrency::Usd, &fx()),
            "< $0.01"
        );
        assert_eq!(
            format_cost(Some(0.003), CostCurrency::Eur, &fx()),
            "< \u{20ac}0.01"
        );
    }

    #[test]
    fn zero_is_a_flat_zero() {
        assert_eq!(format_cost(Some(0.0), CostCurrency::Usd, &fx()), "$0");
    }
}
