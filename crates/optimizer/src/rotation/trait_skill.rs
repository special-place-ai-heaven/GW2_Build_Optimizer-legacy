//! NeedsMechanic Engine E2: trait-skill cast scheduler.
//!
//! Trait-skill is NOT a bus event. When a trait record with `cast_skill_id`
//! fires on an existing TriggerRule (OnElite / OnSkillUse / …), call
//! [`resolve_trait_skill`] and apply the returned [`SkillEffect`]s on the
//! existing apply path. Lesser resolve does not emit OnElite / OnDisableFoe
//! unless those events actually land via CrowdControl / elite cast.

use std::collections::HashMap;

use crate::data::normalized_effects::{OperationType, StatusOperation, TargetSide};
use crate::data::quality::FactualValue;
use crate::rotation::{CoverKind, RotationSkill, SkillEffect};

/// Look up SkillEffects for a trait-cast lesser skill id.
/// Shared by wvw_timeline and simulator. Returns None when the catalog has
/// no entry — callers must not invent hardcoded trait→skill maps.
pub fn resolve_trait_skill(
    skill_id: u32,
    catalog: &HashMap<u32, Vec<SkillEffect>>,
) -> Option<&[SkillEffect]> {
    catalog.get(&skill_id).map(Vec::as_slice)
}

/// Seed a cast catalog from bar / injected RotationSkills.
pub fn catalog_from_skills(skills: &[RotationSkill]) -> HashMap<u32, Vec<SkillEffect>> {
    let mut map = HashMap::new();
    for skill in skills {
        map.insert(skill.skill_id, skill.effects.clone());
    }
    map
}

/// Convert a status-operation payload (lesser skill outcome on the trait
/// record) into SkillEffects for the cast catalog.
///
/// Stacks and duration are scored only from [`FactualValue::Resolved`].
/// Unknown amount abstains the stack field (no invented 1). Missing or
/// Unknown `base_duration_ms` abstains the duration field (no invented
/// 1000 ms). An effect that needs an unresolved field is omitted, the same
/// way a three-value split applies nothing.
pub fn skill_effects_from_status_operation(op: &StatusOperation) -> Vec<SkillEffect> {
    let stacks = match &op.amount_value {
        FactualValue::Resolved(v) => Some((*v).max(1.0).round() as u32),
        FactualValue::Unknown => None,
    };
    let duration_ms = match &op.base_duration_ms {
        Some(FactualValue::Resolved(ms)) => Some(*ms),
        _ => None,
    };
    match (&op.operation_type, &op.target_side) {
        (OperationType::AppliesBoon, TargetSide::Self_ | TargetSide::Ally) => {
            if let Some((kind, strippable)) = cover_kind_for_status(&op.status_kind) {
                let mut out = Vec::new();
                if let Some(duration_ms) = duration_ms {
                    out.push(SkillEffect::Cover {
                        kind,
                        duration_ms,
                        strippable,
                    });
                }
                // Keep the named buff for causal / uptime reads (Arcane Shield,
                // Enduring Pain) alongside cover when both matter. Cover-only
                // boons stay cover: they have no separate stack count.
                if !op.status_kind.eq_ignore_ascii_case("Protection")
                    && !op.status_kind.eq_ignore_ascii_case("Aegis")
                    && !op.status_kind.eq_ignore_ascii_case("Stability")
                    && !op.status_kind.eq_ignore_ascii_case("Resistance")
                {
                    if let (Some(stacks), Some(duration_ms)) = (stacks, duration_ms) {
                        out.push(SkillEffect::ApplyBuff {
                            buff: op.status_kind.clone(),
                            stacks,
                            duration_ms,
                        });
                    }
                }
                out
            } else if let (Some(stacks), Some(duration_ms)) = (stacks, duration_ms) {
                vec![SkillEffect::ApplyBuff {
                    buff: op.status_kind.clone(),
                    stacks,
                    duration_ms,
                }]
            } else {
                Vec::new()
            }
        }
        (OperationType::AppliesCondition, TargetSide::Enemy) => match (stacks, duration_ms) {
            (Some(stacks), Some(duration_ms)) => vec![SkillEffect::ApplyCondition {
                condition: op.status_kind.clone(),
                stacks,
                duration_ms,
            }],
            _ => Vec::new(),
        },
        (OperationType::RemovesCondition, TargetSide::Self_ | TargetSide::Ally) => stacks
            .map(|conditions_removed| vec![SkillEffect::RemovesCondition { conditions_removed }])
            .unwrap_or_default(),
        (OperationType::ConvertsConditionToBoon, TargetSide::Self_ | TargetSide::Ally) => {
            vec![SkillEffect::ConvertConditions]
        }
        (OperationType::RemovesBoon | OperationType::CorruptsBoon, TargetSide::Enemy) => {
            vec![SkillEffect::CorruptBoons]
        }
        _ => Vec::new(),
    }
}

fn cover_kind_for_status(status: &str) -> Option<(CoverKind, bool)> {
    if status.eq_ignore_ascii_case("Distortion")
        || status.eq_ignore_ascii_case("Invulnerability")
        || status.eq_ignore_ascii_case("Determined")
        || status.eq_ignore_ascii_case("Enduring Pain")
    {
        Some((CoverKind::Invulnerability, false))
    } else if status.eq_ignore_ascii_case("Arcane Shield") {
        Some((CoverKind::Block, false))
    } else if status.eq_ignore_ascii_case("Aegis") {
        Some((CoverKind::Aegis, true))
    } else if status.eq_ignore_ascii_case("Stability") {
        Some((CoverKind::Stability, true))
    } else if status.eq_ignore_ascii_case("Resistance") {
        Some((CoverKind::Resistance, true))
    } else if status.eq_ignore_ascii_case("Protection") {
        Some((CoverKind::Protection, true))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::normalized_effects::{AmountMode, TargetScope};

    fn op_arcane() -> StatusOperation {
        StatusOperation {
            operation_type: OperationType::AppliesBoon,
            target_side: TargetSide::Self_,
            status_kind: "Arcane Shield".into(),
            amount_mode: AmountMode::Stacks,
            amount_value: FactualValue::Resolved(3.0),
            base_duration_ms: Some(FactualValue::Resolved(5_000)),
            target_scope: TargetScope::Self_,
            target_count: None,
            internal_cooldown_ms: None,
            source_duration_multiplier: None,
        }
    }

    #[test]
    fn resolve_trait_skill_returns_catalog_entry() {
        let effects = skill_effects_from_status_operation(&op_arcane());
        let mut catalog = HashMap::new();
        catalog.insert(25_579, effects.clone());
        let got = resolve_trait_skill(25_579, &catalog).expect("catalog hit");
        assert_eq!(got, effects.as_slice());
        assert!(resolve_trait_skill(1, &catalog).is_none());
    }

    fn status_op(
        operation_type: OperationType,
        target_side: TargetSide,
        status_kind: &str,
        amount_value: FactualValue<f64>,
        base_duration_ms: Option<FactualValue<u32>>,
    ) -> StatusOperation {
        StatusOperation {
            operation_type,
            target_side,
            status_kind: status_kind.into(),
            amount_mode: AmountMode::Stacks,
            amount_value,
            base_duration_ms,
            target_scope: TargetScope::Self_,
            target_count: None,
            internal_cooldown_ms: None,
            source_duration_multiplier: None,
        }
    }

    fn scored_stacks(effects: &[SkillEffect]) -> Vec<u32> {
        effects
            .iter()
            .filter_map(|e| match e {
                SkillEffect::ApplyCondition { stacks, .. }
                | SkillEffect::ApplyBuff { stacks, .. } => Some(*stacks),
                SkillEffect::RemovesCondition { conditions_removed } => Some(*conditions_removed),
                _ => None,
            })
            .collect()
    }

    fn scored_durations(effects: &[SkillEffect]) -> Vec<u32> {
        effects
            .iter()
            .filter_map(|e| match e {
                SkillEffect::ApplyCondition { duration_ms, .. }
                | SkillEffect::ApplyBuff { duration_ms, .. }
                | SkillEffect::Cover { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect()
    }

    /// amount_value=Unknown must not become stacks=1 (or conditions_removed=1).
    #[test]
    fn unknown_amount_does_not_invent_stacks() {
        for (op_type, side, status) in [
            (
                OperationType::AppliesCondition,
                TargetSide::Enemy,
                "Bleeding",
            ),
            (OperationType::AppliesBoon, TargetSide::Self_, "Might"),
            (OperationType::RemovesCondition, TargetSide::Self_, "Any"),
        ] {
            let op = status_op(
                op_type,
                side,
                status,
                FactualValue::Unknown,
                Some(FactualValue::Resolved(4_000)),
            );
            let effects = skill_effects_from_status_operation(&op);
            assert!(
                scored_stacks(&effects).is_empty(),
                "{status}: unknown amount invented stacks in {effects:?}"
            );
        }
    }

    /// A resolved cover duration still lands; the unknown stack field abstains.
    #[test]
    fn unknown_stacks_keep_resolved_cover_duration() {
        let op = status_op(
            OperationType::AppliesBoon,
            TargetSide::Self_,
            "Arcane Shield",
            FactualValue::Unknown,
            Some(FactualValue::Resolved(5_000)),
        );
        let effects = skill_effects_from_status_operation(&op);
        assert!(scored_stacks(&effects).is_empty(), "{effects:?}");
        assert_eq!(scored_durations(&effects), vec![5_000]);
        assert!(effects.iter().any(|e| matches!(
            e,
            SkillEffect::Cover {
                kind: CoverKind::Block,
                duration_ms: 5_000,
                ..
            }
        )));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SkillEffect::ApplyBuff { .. })));
    }

    /// Missing or Unknown base_duration_ms must not become 1000 ms.
    #[test]
    fn unknown_or_missing_duration_does_not_invent_1000ms() {
        for duration in [None, Some(FactualValue::Unknown)] {
            let op = status_op(
                OperationType::AppliesBoon,
                TargetSide::Self_,
                "Might",
                FactualValue::Resolved(3.0),
                duration.clone(),
            );
            let effects = skill_effects_from_status_operation(&op);
            assert!(
                effects.is_empty(),
                "duration {duration:?} invented a timed effect: {effects:?}"
            );
            assert!(!scored_durations(&effects).contains(&1_000));
        }

        let cover = status_op(
            OperationType::AppliesBoon,
            TargetSide::Self_,
            "Protection",
            FactualValue::Resolved(1.0),
            Some(FactualValue::Unknown),
        );
        let effects = skill_effects_from_status_operation(&cover);
        assert!(
            scored_durations(&effects).is_empty(),
            "unknown cover duration invented {effects:?}"
        );
    }

    /// Resolved amount keeps max(1).round(); a real 1000 ms duration stays.
    /// Cleanse count does not need a duration.
    #[test]
    fn resolved_amount_and_duration_keep_existing_rules() {
        let bleeding = status_op(
            OperationType::AppliesCondition,
            TargetSide::Enemy,
            "Bleeding",
            FactualValue::Resolved(2.4),
            Some(FactualValue::Resolved(1_000)),
        );
        let effects = skill_effects_from_status_operation(&bleeding);
        assert_eq!(scored_stacks(&effects), vec![2]);
        assert_eq!(scored_durations(&effects), vec![1_000]);

        let low = status_op(
            OperationType::AppliesCondition,
            TargetSide::Enemy,
            "Bleeding",
            FactualValue::Resolved(0.2),
            Some(FactualValue::Resolved(3_500)),
        );
        assert_eq!(
            scored_stacks(&skill_effects_from_status_operation(&low)),
            vec![1]
        );

        let cleanse = status_op(
            OperationType::RemovesCondition,
            TargetSide::Self_,
            "Any",
            FactualValue::Resolved(2.9),
            None,
        );
        assert_eq!(
            skill_effects_from_status_operation(&cleanse),
            vec![SkillEffect::RemovesCondition {
                conditions_removed: 3
            }]
        );
    }

    #[test]
    fn arcane_shield_maps_to_block_cover_and_buff() {
        let effects = skill_effects_from_status_operation(&op_arcane());
        assert!(
            effects.iter().any(|e| matches!(
                e,
                SkillEffect::Cover {
                    kind: CoverKind::Block,
                    ..
                }
            )),
            "{effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                SkillEffect::ApplyBuff { buff, .. } if buff == "Arcane Shield"
            )),
            "{effects:?}"
        );
    }
}
