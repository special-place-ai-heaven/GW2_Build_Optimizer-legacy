use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::ops::{Add, Div, Mul, Sub};

/// Overall data quality assessment for an optimizer output.
/// Degrades from Verified when any input data is Unknown or Provisional.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum DataQuality {
    /// All data inputs are factual and verified.
    #[default]
    Verified,
    /// Some data inputs are estimated or provisional; results are usable but less certain.
    Provisional,
    /// Critical data is missing or unknown; results should not be trusted.
    Blocked,
}

impl DataQuality {
    /// Merge two quality levels, keeping the worse of the two.
    pub fn merge(&self, other: &DataQuality) -> DataQuality {
        match (self, other) {
            (DataQuality::Blocked, _) | (_, DataQuality::Blocked) => DataQuality::Blocked,
            (DataQuality::Provisional, _) | (_, DataQuality::Provisional) => {
                DataQuality::Provisional
            }
            _ => DataQuality::Verified,
        }
    }
}

impl fmt::Display for DataQuality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataQuality::Verified => write!(f, "Verified"),
            DataQuality::Provisional => write!(f, "Provisional"),
            DataQuality::Blocked => write!(f, "Blocked"),
        }
    }
}

/// Explains why data quality was degraded for a specific field/entity.
#[derive(Debug, Clone)]
pub struct DataQualityReason {
    /// The field that caused degradation (e.g., "coefficient").
    pub field: String,
    /// The entity it belongs to (e.g., "Burning", "Warrior").
    pub entity: String,
    /// Game modes affected (e.g., ["PvP", "WvW"]).
    pub modes: Vec<String>,
    /// Human-readable explanation.
    pub explanation: String,
}

impl fmt::Display for DataQualityReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} [{}]: {}",
            self.entity,
            self.field,
            self.modes.join(", "),
            self.explanation,
        )
    }
}

/// Field of the coverage reason: the WvW timeline's unmodeled sources.
pub const COVERAGE_FIELD: &str = "wvw_timeline.effects";
/// PvE/PvP: coverage inventory (`active_normalized_effects`) was not run.
/// Distinct from [`COVERAGE_FIELD`] so a skipped inventory is not read as
/// a WvW coverage line (CONN-01-03).
pub const INVENTORY_FIELD: &str = "coverage.inventory";
/// Prepare-time records the flow sim could not host.
pub const UNHOSTED_FIELD: &str = "coverage.unhosted";
/// Description-fallback Barrier{1000} / Healing{1} (W151).
pub const HEURISTIC_FIELD: &str = "coverage.heuristic";
/// English prefix of the coverage explanation; the addon renders the same
/// line through the `quality.coverage_line` locale key with the detail.
pub const COVERAGE_PREFIX: &str = "Not simulated: ";
/// Stable inventory-skip detail; `{mode}` is the scenario's own label.
pub const INVENTORY_SKIP_PREFIX: &str = "coverage inventory not run for ";
const COVERAGE_NAMED: usize = 3;

/// Why a source sits on the coverage line (specs/007-trait-triggers, US4).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReasonClass {
    /// No record and no consumed fact.
    NoRecord,
    /// Classified: the trait changes nothing the simulator measures.
    PassiveNoEffect,
    /// Classified: needs a mechanic the simulator has no state for.
    NeedsMechanic(String),
    /// A record exists but its number is unresolved.
    UnresolvedValue,
    /// A record exists but its trigger has no runtime firing site.
    NoFiringSite,
}

impl ReasonClass {
    /// The suffix rendered after the name: `Gravedigger (no record)`.
    pub fn suffix(&self) -> String {
        match self {
            ReasonClass::NoRecord => "no record".into(),
            ReasonClass::PassiveNoEffect => "passive, no simulated effect".into(),
            ReasonClass::NeedsMechanic(m) => format!("needs: {m}"),
            ReasonClass::UnresolvedValue => "unresolved value".into(),
            ReasonClass::NoFiringSite => "no firing site".into(),
        }
    }
}

/// One skipped source on the coverage line.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageEntry {
    pub name: String,
    pub class: ReasonClass,
    pub detail: Option<String>,
}

impl CoverageEntry {
    /// `"{name} ({suffix})"`, the string `unmodeled_sources` carries. A
    /// runtime note keeps its own wording in `detail` (`on-crit`, `partial
    /// combo`), so the Sprint 1 and 2 lines read as they always did.
    pub fn rendered(&self) -> String {
        let why = self.detail.clone().unwrap_or_else(|| self.class.suffix());
        format!("{} ({})", self.name, why)
    }

    /// Back from a timeline note `"{name} ({why})"`: the two classes the
    /// loader names verbatim, everything else a trigger with no runtime site.
    pub fn from_runtime_note(note: &str) -> CoverageEntry {
        let (name, why) = note
            .strip_suffix(')')
            .and_then(|s| s.rsplit_once(" ("))
            .unwrap_or((note, "no firing site"));
        let class = match why {
            "no record" => ReasonClass::NoRecord,
            "unresolved value" => ReasonClass::UnresolvedValue,
            _ => ReasonClass::NoFiringSite,
        };
        let detail = (why != class.suffix()).then(|| why.to_string());
        CoverageEntry {
            name: name.to_string(),
            class,
            detail,
        }
    }
}

/// `a, b, c and N others` for up to three named sources; `None` when nothing
/// is unmodeled. The names are game item names and are not translated.
pub fn coverage_detail(unmodeled: &[String]) -> Option<String> {
    if unmodeled.is_empty() {
        return None;
    }
    let named = unmodeled
        .iter()
        .take(COVERAGE_NAMED)
        .cloned()
        .collect::<Vec<_>>();
    let rest = unmodeled.len().saturating_sub(COVERAGE_NAMED);
    Some(match rest {
        0 => named.join(", "),
        1 => format!("{} and 1 other", named.join(", ")),
        n => format!("{} and {n} others", named.join(", ")),
    })
}

/// The one coverage reason both the referee and the Optimize packaging
/// attach: `Not simulated: …` on `wvw_timeline.effects`. `None` when every
/// equipped source with a record was executed — which is not a claim that
/// every mechanic is modeled.
pub fn coverage_reason(
    profession: &str,
    mode: &gw2_core::types::GameMode,
    unmodeled: &[String],
) -> Option<DataQualityReason> {
    let detail = coverage_detail(unmodeled)?;
    Some(reason_on(
        COVERAGE_FIELD,
        profession,
        mode,
        format!("{COVERAGE_PREFIX}{detail}"),
    ))
}

/// PvE/PvP honesty: inventory skipped and/or unhosted flow records, plus
/// heuristic Barrier/Healing stamps. `wvw_unmodeled = Some` means the
/// timeline ran inventory — emit the existing WvW line, never the skip.
/// Bind skip to `inventory_skipped` (mode records existed) so a PvE kit
/// with nothing to inventory stays Verified.
pub fn mode_honesty_reasons(
    profession: &str,
    mode: &gw2_core::types::GameMode,
    wvw_unmodeled: Option<&[String]>,
    unhosted: &[String],
    inventory_skipped: bool,
    heuristic: &[String],
) -> Vec<DataQualityReason> {
    let mut out = Vec::new();
    match wvw_unmodeled {
        Some(names) => out.extend(coverage_reason(profession, mode, names)),
        None => {
            if inventory_skipped {
                out.push(reason_on(
                    INVENTORY_FIELD,
                    profession,
                    mode,
                    format!("{COVERAGE_PREFIX}{INVENTORY_SKIP_PREFIX}{}", mode.label()),
                ));
            }
            if let Some(detail) = coverage_detail(unhosted) {
                out.push(reason_on(
                    UNHOSTED_FIELD,
                    profession,
                    mode,
                    format!("{COVERAGE_PREFIX}{detail}"),
                ));
            }
        }
    }
    if let Some(detail) = coverage_detail(heuristic) {
        out.push(reason_on(
            HEURISTIC_FIELD,
            profession,
            mode,
            format!("{COVERAGE_PREFIX}{detail}"),
        ));
    }
    out
}

fn reason_on(
    field: &str,
    profession: &str,
    mode: &gw2_core::types::GameMode,
    explanation: String,
) -> DataQualityReason {
    DataQualityReason {
        field: field.into(),
        entity: profession.into(),
        modes: vec![mode.label().to_string()],
        explanation,
    }
}

/// `UnresolvedValue` + detail, the coverage-line form for a heuristic coeff.
pub fn heuristic_entry(name: &str, kind: &str) -> CoverageEntry {
    CoverageEntry {
        name: name.into(),
        class: ReasonClass::UnresolvedValue,
        detail: Some(format!("heuristic {kind}")),
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    fn names(n: usize) -> Vec<String> {
        [
            "Superior Sigil of Fire (on-crit)",
            "Reaper Shroud 4 (dark field)",
            "Relic of the Thief (no record)",
            "Superior Rune of the Scholar (no record)",
            "Soul Spiral (partial combo)",
        ]
        .iter()
        .take(n)
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn reason_class_suffixes_match_the_contract() {
        let cases = [
            (ReasonClass::NoRecord, "no record"),
            (ReasonClass::PassiveNoEffect, "passive, no simulated effect"),
            (
                ReasonClass::NeedsMechanic("minions".into()),
                "needs: minions",
            ),
            (ReasonClass::UnresolvedValue, "unresolved value"),
            (ReasonClass::NoFiringSite, "no firing site"),
        ];
        for (class, suffix) in cases {
            assert_eq!(class.suffix(), suffix);
        }
        let entry = CoverageEntry {
            name: "Flesh of the Master".into(),
            class: ReasonClass::NeedsMechanic("minions".into()),
            detail: None,
        };
        assert_eq!(entry.rendered(), "Flesh of the Master (needs: minions)");

        let note = CoverageEntry::from_runtime_note("Superior Sigil of Fire (on-crit)");
        assert_eq!(note.class, ReasonClass::NoFiringSite);
        assert_eq!(note.rendered(), "Superior Sigil of Fire (on-crit)");
        let note = CoverageEntry::from_runtime_note("Scholar (unresolved value)");
        assert_eq!(note.class, ReasonClass::UnresolvedValue);
        assert_eq!(note.detail, None);
    }

    #[test]
    fn coverage_line_renders_zero_one_three_and_five_names() {
        assert_eq!(coverage_detail(&names(0)), None);
        assert_eq!(
            coverage_detail(&names(1)).as_deref(),
            Some("Superior Sigil of Fire (on-crit)")
        );
        assert_eq!(
            coverage_detail(&names(3)).as_deref(),
            Some("Superior Sigil of Fire (on-crit), Reaper Shroud 4 (dark field), Relic of the Thief (no record)")
        );
        assert_eq!(
            coverage_detail(&names(4)).as_deref(),
            Some("Superior Sigil of Fire (on-crit), Reaper Shroud 4 (dark field), Relic of the Thief (no record) and 1 other")
        );
        assert_eq!(
            coverage_detail(&names(5)).as_deref(),
            Some("Superior Sigil of Fire (on-crit), Reaper Shroud 4 (dark field), Relic of the Thief (no record) and 2 others")
        );
        let reason = coverage_reason("Necromancer", &gw2_core::types::GameMode::WvW, &names(1))
            .expect("one name is a reason");
        assert_eq!(reason.field, COVERAGE_FIELD);
        assert_eq!(reason.modes, vec!["WvW".to_string()]);
        assert_eq!(
            reason.explanation,
            "Not simulated: Superior Sigil of Fire (on-crit)"
        );
        assert!(coverage_reason("Necromancer", &gw2_core::types::GameMode::WvW, &[]).is_none());
    }

    #[test]
    fn mode_honesty_binds_skip_to_due_inventory_not_every_pve_run() {
        let pve = gw2_core::types::GameMode::PvE;
        assert!(
            mode_honesty_reasons("Necromancer", &pve, None, &[], false, &[]).is_empty(),
            "empty unhosted and inventory not due must not blanket-Provisional PvE"
        );
        let skipped = mode_honesty_reasons("Necromancer", &pve, None, &[], true, &[]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].field, INVENTORY_FIELD);
        assert_eq!(skipped[0].modes, vec!["PvE".to_string()]);
        assert_eq!(
            skipped[0].explanation,
            "Not simulated: coverage inventory not run for PvE"
        );
        let unhosted = mode_honesty_reasons(
            "Necromancer",
            &pve,
            None,
            &["Path of Corruption OnHit (flow sim: gated)".into()],
            false,
            &[],
        );
        assert_eq!(unhosted[0].field, UNHOSTED_FIELD);
        let wvw = mode_honesty_reasons(
            "Necromancer",
            &gw2_core::types::GameMode::WvW,
            Some(&names(1)),
            &[],
            true,
            &[],
        );
        assert_eq!(wvw.len(), 1);
        assert_eq!(wvw[0].field, COVERAGE_FIELD);
        assert!(
            !wvw[0].explanation.contains(INVENTORY_SKIP_PREFIX),
            "WvW inventory ran: skip must not impersonate the coverage line"
        );
        let wvw_unhosted = mode_honesty_reasons(
            "Necromancer",
            &gw2_core::types::GameMode::WvW,
            Some(&[]),
            &["Path of Corruption OnHit (flow sim: gated)".into()],
            true,
            &[],
        );
        assert!(
            wvw_unhosted.is_empty(),
            "unhosted already rides the WvW resource line: {wvw_unhosted:?}"
        );
    }

    #[test]
    fn heuristic_entry_uses_unresolved_value_plus_detail() {
        let entry = heuristic_entry("Troll Unguent", "Barrier");
        assert_eq!(entry.class, ReasonClass::UnresolvedValue);
        assert_eq!(entry.rendered(), "Troll Unguent (heuristic Barrier)");
        let reasons = mode_honesty_reasons(
            "Ranger",
            &gw2_core::types::GameMode::PvE,
            None,
            &[],
            false,
            &[entry.rendered()],
        );
        assert_eq!(reasons[0].field, HEURISTIC_FIELD);
        assert_eq!(
            reasons[0].explanation,
            "Not simulated: Troll Unguent (heuristic Barrier)"
        );
    }
}

/// A value that is either resolved (known) or explicitly unknown.
/// Unknown values propagate through arithmetic: any operation involving
/// Unknown produces Unknown.
#[derive(Debug, Clone, PartialEq)]
pub enum FactualValue<T> {
    /// A known, resolved value.
    Resolved(T),
    /// The value is explicitly unknown (e.g., no data for this mode).
    Unknown,
}

impl<T> FactualValue<T> {
    pub fn is_resolved(&self) -> bool {
        matches!(self, FactualValue::Resolved(_))
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, FactualValue::Unknown)
    }

    /// Unwrap the resolved value, or return a default.
    pub fn unwrap_or(self, default: T) -> T {
        match self {
            FactualValue::Resolved(v) => v,
            FactualValue::Unknown => default,
        }
    }

    /// Map the inner value if Resolved, otherwise propagate Unknown.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> FactualValue<U> {
        match self {
            FactualValue::Resolved(v) => FactualValue::Resolved(f(v)),
            FactualValue::Unknown => FactualValue::Unknown,
        }
    }

    /// Map the inner value, but if the mapping itself would produce an unknown,
    /// return Unknown. Useful for chaining computations.
    pub fn map_or_unknown<U, F: FnOnce(T) -> FactualValue<U>>(self, f: F) -> FactualValue<U> {
        match self {
            FactualValue::Resolved(v) => f(v),
            FactualValue::Unknown => FactualValue::Unknown,
        }
    }
}

impl<T: fmt::Display> fmt::Display for FactualValue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FactualValue::Resolved(v) => write!(f, "{}", v),
            FactualValue::Unknown => write!(f, "Unknown"),
        }
    }
}

impl<T: Serialize> Serialize for FactualValue<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            FactualValue::Resolved(v) => v.serialize(serializer),
            FactualValue::Unknown => serializer.serialize_none(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for FactualValue<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let opt = Option::<T>::deserialize(deserializer)?;
        Ok(match opt {
            Some(v) => FactualValue::Resolved(v),
            None => FactualValue::Unknown,
        })
    }
}

// Arithmetic for FactualValue<f64>

impl Mul<f64> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn mul(self, rhs: f64) -> Self::Output {
        match self {
            FactualValue::Resolved(v) => FactualValue::Resolved(v * rhs),
            FactualValue::Unknown => FactualValue::Unknown,
        }
    }
}

impl Add<f64> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn add(self, rhs: f64) -> Self::Output {
        match self {
            FactualValue::Resolved(v) => FactualValue::Resolved(v + rhs),
            FactualValue::Unknown => FactualValue::Unknown,
        }
    }
}

impl Sub<f64> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn sub(self, rhs: f64) -> Self::Output {
        match self {
            FactualValue::Resolved(v) => FactualValue::Resolved(v - rhs),
            FactualValue::Unknown => FactualValue::Unknown,
        }
    }
}

impl Div<f64> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn div(self, rhs: f64) -> Self::Output {
        match self {
            FactualValue::Resolved(v) => FactualValue::Resolved(v / rhs),
            FactualValue::Unknown => FactualValue::Unknown,
        }
    }
}

impl Add<FactualValue<f64>> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn add(self, rhs: FactualValue<f64>) -> Self::Output {
        match (self, rhs) {
            (FactualValue::Resolved(a), FactualValue::Resolved(b)) => FactualValue::Resolved(a + b),
            _ => FactualValue::Unknown,
        }
    }
}

impl Sub<FactualValue<f64>> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn sub(self, rhs: FactualValue<f64>) -> Self::Output {
        match (self, rhs) {
            (FactualValue::Resolved(a), FactualValue::Resolved(b)) => FactualValue::Resolved(a - b),
            _ => FactualValue::Unknown,
        }
    }
}

impl Mul<FactualValue<f64>> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn mul(self, rhs: FactualValue<f64>) -> Self::Output {
        match (self, rhs) {
            (FactualValue::Resolved(a), FactualValue::Resolved(b)) => FactualValue::Resolved(a * b),
            _ => FactualValue::Unknown,
        }
    }
}

impl Div<FactualValue<f64>> for FactualValue<f64> {
    type Output = FactualValue<f64>;

    fn div(self, rhs: FactualValue<f64>) -> Self::Output {
        match (self, rhs) {
            (FactualValue::Resolved(a), FactualValue::Resolved(b)) => FactualValue::Resolved(a / b),
            _ => FactualValue::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // DataQuality tests

    #[test]
    fn test_data_quality_display() {
        assert_eq!(DataQuality::Verified.to_string(), "Verified");
        assert_eq!(DataQuality::Provisional.to_string(), "Provisional");
        assert_eq!(DataQuality::Blocked.to_string(), "Blocked");
    }

    #[test]
    fn test_data_quality_merge_verified_stays_verified() {
        assert_eq!(
            DataQuality::Verified.merge(&DataQuality::Verified),
            DataQuality::Verified,
        );
    }

    #[test]
    fn test_data_quality_merge_provisional_degrades() {
        assert_eq!(
            DataQuality::Verified.merge(&DataQuality::Provisional),
            DataQuality::Provisional,
        );
        assert_eq!(
            DataQuality::Provisional.merge(&DataQuality::Verified),
            DataQuality::Provisional,
        );
    }

    #[test]
    fn test_data_quality_merge_blocked_wins() {
        assert_eq!(
            DataQuality::Verified.merge(&DataQuality::Blocked),
            DataQuality::Blocked,
        );
        assert_eq!(
            DataQuality::Provisional.merge(&DataQuality::Blocked),
            DataQuality::Blocked,
        );
        assert_eq!(
            DataQuality::Blocked.merge(&DataQuality::Verified),
            DataQuality::Blocked,
        );
    }

    // DataQualityReason tests

    #[test]
    fn test_data_quality_reason_display() {
        let reason = DataQualityReason {
            field: "coefficient".into(),
            entity: "Burning".into(),
            modes: vec!["PvP".into(), "WvW".into()],
            explanation: "No split-balance data available".into(),
        };
        assert_eq!(
            reason.to_string(),
            "Burning.coefficient [PvP, WvW]: No split-balance data available",
        );
    }

    // FactualValue basic tests

    #[test]
    fn test_factual_value_is_resolved() {
        assert!(FactualValue::Resolved(42.0).is_resolved());
        assert!(!FactualValue::<f64>::Unknown.is_resolved());
    }

    #[test]
    fn test_factual_value_is_unknown() {
        assert!(FactualValue::<f64>::Unknown.is_unknown());
        assert!(!FactualValue::Resolved(42.0).is_unknown());
    }

    #[test]
    fn test_factual_value_unwrap_or() {
        assert_eq!(FactualValue::Resolved(10.0).unwrap_or(0.0), 10.0);
        assert_eq!(FactualValue::<f64>::Unknown.unwrap_or(0.0), 0.0);
    }

    #[test]
    fn test_factual_value_map() {
        let v = FactualValue::Resolved(5.0).map(|x| x * 2.0);
        assert_eq!(v, FactualValue::Resolved(10.0));

        let u: FactualValue<f64> = FactualValue::<f64>::Unknown.map(|x| x * 2.0);
        assert_eq!(u, FactualValue::Unknown);
    }

    #[test]
    fn test_factual_value_map_or_unknown() {
        let v = FactualValue::Resolved(5.0).map_or_unknown(|x| FactualValue::Resolved(x + 1.0));
        assert_eq!(v, FactualValue::Resolved(6.0));

        let u = FactualValue::<f64>::Unknown.map_or_unknown(|x| FactualValue::Resolved(x + 1.0));
        assert_eq!(u, FactualValue::Unknown);

        // Inner function returns Unknown
        let w = FactualValue::Resolved(5.0).map_or_unknown(|_| FactualValue::<f64>::Unknown);
        assert_eq!(w, FactualValue::Unknown);
    }

    #[test]
    fn test_factual_value_display() {
        assert_eq!(FactualValue::Resolved(12.34).to_string(), "12.34");
        assert_eq!(FactualValue::<f64>::Unknown.to_string(), "Unknown");
    }

    // FactualValue<f64> arithmetic with scalar

    #[test]
    fn test_resolved_mul_scalar() {
        assert_eq!(
            FactualValue::Resolved(10.0) * 5.0,
            FactualValue::Resolved(50.0)
        );
    }

    #[test]
    fn test_unknown_mul_scalar() {
        assert_eq!(FactualValue::<f64>::Unknown * 5.0, FactualValue::Unknown);
    }

    #[test]
    fn test_resolved_add_scalar() {
        assert_eq!(
            FactualValue::Resolved(10.0) + 3.0,
            FactualValue::Resolved(13.0)
        );
    }

    #[test]
    fn test_unknown_add_scalar() {
        assert_eq!(FactualValue::<f64>::Unknown + 3.0, FactualValue::Unknown);
    }

    #[test]
    fn test_resolved_sub_scalar() {
        assert_eq!(
            FactualValue::Resolved(10.0) - 3.0,
            FactualValue::Resolved(7.0)
        );
    }

    #[test]
    fn test_unknown_sub_scalar() {
        assert_eq!(FactualValue::<f64>::Unknown - 3.0, FactualValue::Unknown);
    }

    #[test]
    fn test_resolved_div_scalar() {
        assert_eq!(
            FactualValue::Resolved(10.0) / 2.0,
            FactualValue::Resolved(5.0)
        );
    }

    #[test]
    fn test_unknown_div_scalar() {
        assert_eq!(FactualValue::<f64>::Unknown / 2.0, FactualValue::Unknown);
    }

    // FactualValue<f64> arithmetic with FactualValue<f64>

    #[test]
    fn test_resolved_add_resolved() {
        assert_eq!(
            FactualValue::Resolved(10.0) + FactualValue::Resolved(5.0),
            FactualValue::Resolved(15.0),
        );
    }

    #[test]
    fn test_resolved_add_unknown() {
        assert_eq!(
            FactualValue::Resolved(10.0) + FactualValue::<f64>::Unknown,
            FactualValue::Unknown,
        );
    }

    #[test]
    fn test_unknown_add_resolved() {
        assert_eq!(
            FactualValue::<f64>::Unknown + FactualValue::Resolved(5.0),
            FactualValue::Unknown,
        );
    }

    #[test]
    fn test_unknown_add_unknown() {
        assert_eq!(
            FactualValue::<f64>::Unknown + FactualValue::<f64>::Unknown,
            FactualValue::Unknown,
        );
    }

    #[test]
    fn test_resolved_sub_resolved() {
        assert_eq!(
            FactualValue::Resolved(10.0) - FactualValue::Resolved(3.0),
            FactualValue::Resolved(7.0),
        );
    }

    #[test]
    fn test_resolved_sub_unknown() {
        assert_eq!(
            FactualValue::Resolved(10.0) - FactualValue::<f64>::Unknown,
            FactualValue::Unknown,
        );
    }

    #[test]
    fn test_resolved_mul_resolved() {
        assert_eq!(
            FactualValue::Resolved(3.0) * FactualValue::Resolved(4.0),
            FactualValue::Resolved(12.0),
        );
    }

    #[test]
    fn test_resolved_mul_unknown() {
        assert_eq!(
            FactualValue::Resolved(3.0) * FactualValue::<f64>::Unknown,
            FactualValue::Unknown,
        );
    }

    #[test]
    fn test_resolved_div_resolved() {
        assert_eq!(
            FactualValue::Resolved(10.0) / FactualValue::Resolved(2.0),
            FactualValue::Resolved(5.0),
        );
    }

    #[test]
    fn test_resolved_div_unknown() {
        assert_eq!(
            FactualValue::Resolved(10.0) / FactualValue::<f64>::Unknown,
            FactualValue::Unknown,
        );
    }

    // DataQuality defaults to Verified for baseline

    #[test]
    fn test_data_quality_baseline_is_verified() {
        // In the baseline (no overrides), quality should be Verified.
        let quality = DataQuality::Verified;
        assert_eq!(quality, DataQuality::Verified);
    }

    // FactualValue serde tests

    #[test]
    fn test_factual_value_serde_resolved() {
        let v: FactualValue<f64> = FactualValue::Resolved(0.5);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "0.5");
        let deserialized: FactualValue<f64> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, FactualValue::Resolved(0.5));
    }

    #[test]
    fn test_factual_value_serde_unknown() {
        let v: FactualValue<f64> = FactualValue::Unknown;
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "null");
        let deserialized: FactualValue<f64> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, FactualValue::Unknown);
    }
}
