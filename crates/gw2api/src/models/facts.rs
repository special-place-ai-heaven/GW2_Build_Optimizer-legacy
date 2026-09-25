//! Shared fact types used by both traits and skills.
//! Facts describe the mechanical effects (damage, buffs, conditions, etc.)
//! that are critical for the LLM to reason about synergies and rotations.

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

/// The `type` field in the API determines which variant this is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Fact {
    AttributeAdjust {
        text: Option<String>,
        icon: Option<String>,
        value: Option<i32>,
        target: Option<String>,
    },
    Buff {
        text: Option<String>,
        icon: Option<String>,
        duration: Option<u32>,
        status: Option<String>,
        description: Option<String>,
        apply_count: Option<u32>,
    },
    BuffConversion {
        text: Option<String>,
        icon: Option<String>,
        source: Option<String>,
        percent: Option<f64>,
        target: Option<String>,
    },
    ComboField {
        text: Option<String>,
        icon: Option<String>,
        field_type: Option<String>,
    },
    ComboFinisher {
        text: Option<String>,
        icon: Option<String>,
        finisher_type: Option<String>,
        percent: Option<u32>,
    },
    Damage {
        text: Option<String>,
        icon: Option<String>,
        hit_count: Option<u32>,
        dmg_multiplier: Option<f64>,
    },
    Distance {
        text: Option<String>,
        icon: Option<String>,
        distance: Option<u32>,
    },
    Duration {
        text: Option<String>,
        icon: Option<String>,
        duration: Option<u32>,
    },
    Heal {
        text: Option<String>,
        icon: Option<String>,
        hit_count: Option<u32>,
    },
    HealingAdjust {
        text: Option<String>,
        icon: Option<String>,
        hit_count: Option<u32>,
    },
    NoData {
        text: Option<String>,
        icon: Option<String>,
    },
    Number {
        text: Option<String>,
        icon: Option<String>,
        value: Option<i32>,
    },
    Percent {
        text: Option<String>,
        icon: Option<String>,
        percent: Option<f64>,
    },
    PrefixedBuff {
        text: Option<String>,
        icon: Option<String>,
        duration: Option<u32>,
        status: Option<String>,
        description: Option<String>,
        apply_count: Option<u32>,
        prefix: Option<BuffPrefix>,
    },
    Radius {
        text: Option<String>,
        icon: Option<String>,
        distance: Option<u32>,
    },
    Range {
        text: Option<String>,
        icon: Option<String>,
        value: Option<u32>,
    },
    Recharge {
        text: Option<String>,
        icon: Option<String>,
        value: Option<f64>,
    },
    StunBreak {
        text: Option<String>,
        icon: Option<String>,
        value: Option<bool>,
    },
    Time {
        text: Option<String>,
        icon: Option<String>,
        duration: Option<u32>,
    },
    Unblockable {
        text: Option<String>,
        icon: Option<String>,
        value: Option<bool>,
    },
    /// Fallback for unknown fact types the API may add in the future.
    #[serde(other)]
    Unknown,
}

/// Prefix for PrefixedBuff facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuffPrefix {
    pub text: Option<String>,
    pub icon: Option<String>,
    pub status: Option<String>,
    pub description: Option<String>,
}

/// A conditional fact that activates when a specific trait is selected.
/// `requires_trait` is the trait ID that must be equipped.
/// `overrides` is the index in the parent facts array to replace (or append if absent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraitedFact {
    pub requires_trait: u32,
    pub overrides: Option<u32>,
    #[serde(flatten)]
    pub fact: Fact,
}

thread_local! {
    static FACT_DROP_FRAMES: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// Opened by skill/trait deserialize so a fact that fails to parse is counted
/// on that owner instead of discarded with no record.
pub(crate) struct FactDropFrame {
    open: bool,
}

impl FactDropFrame {
    pub(crate) fn enter() -> Self {
        FACT_DROP_FRAMES.with(|frames| frames.borrow_mut().push(0));
        Self { open: true }
    }

    pub(crate) fn live_drops(mut self) -> u32 {
        self.open = false;
        FACT_DROP_FRAMES.with(|frames| frames.borrow_mut().pop().unwrap_or(0))
    }
}

impl Drop for FactDropFrame {
    fn drop(&mut self) {
        if self.open {
            FACT_DROP_FRAMES.with(|frames| {
                frames.borrow_mut().pop();
            });
        }
    }
}

pub(crate) fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn partition<T: serde::de::DeserializeOwned>(values: Vec<serde_json::Value>) -> (Vec<T>, u32) {
    let mut kept = Vec::with_capacity(values.len());
    let mut dropped = 0u32;
    for value in values {
        match serde_json::from_value(value) {
            Ok(item) => kept.push(item),
            Err(_) => dropped = dropped.saturating_add(1),
        }
    }
    (kept, dropped)
}

/// A parse failure with no open [`FactDropFrame`] cannot be tied to a skill or
/// trait id. Fail the value instead of returning a shorter vec that looks complete.
fn note_or_fail<E: serde::de::Error>(dropped: u32) -> Result<(), E> {
    if dropped == 0 {
        return Ok(());
    }
    let attached = FACT_DROP_FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        match frames.last_mut() {
            Some(top) => {
                *top = top.saturating_add(dropped);
                true
            }
            None => false,
        }
    });
    if attached {
        Ok(())
    } else {
        Err(E::custom(format!(
            "{dropped} fact(s) failed to parse and no skill or trait quality surface is attached"
        )))
    }
}

fn deserialize_lenient<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    let (kept, dropped) = partition(values);
    note_or_fail(dropped)?;
    Ok(kept)
}

/// Keeps facts that parse. A failure is counted on the enclosing skill or trait
/// (`fact_parse_drops`). With no owner frame, the value fails closed.
/// The GW2 API occasionally returns fact objects without a `type` discriminator.
pub fn deserialize_facts<'de, D>(deserializer: D) -> Result<Vec<Fact>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_lenient(deserializer)
}

/// Same contract as [`deserialize_facts`] for traited facts.
pub fn deserialize_traited_facts<'de, D>(deserializer: D) -> Result<Vec<TraitedFact>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_lenient(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_attribute_adjust() {
        let json = r#"{
            "type": "AttributeAdjust",
            "icon": "https://example.com/icon.png",
            "value": 180,
            "target": "CritDamage"
        }"#;
        let fact: Fact = serde_json::from_str(json).unwrap();
        match fact {
            Fact::AttributeAdjust { value, target, .. } => {
                assert_eq!(value, Some(180));
                assert_eq!(target.as_deref(), Some("CritDamage"));
            }
            _ => panic!("Expected AttributeAdjust"),
        }
    }

    #[test]
    fn test_deserialize_buff() {
        let json = r#"{
            "text": "Apply Buff/Condition",
            "type": "Buff",
            "icon": "https://example.com/icon.png",
            "duration": 4,
            "status": "Fury",
            "description": "Critical chance increased; stacks duration.",
            "apply_count": 1
        }"#;
        let fact: Fact = serde_json::from_str(json).unwrap();
        match fact {
            Fact::Buff {
                duration,
                status,
                apply_count,
                ..
            } => {
                assert_eq!(duration, Some(4));
                assert_eq!(status.as_deref(), Some("Fury"));
                assert_eq!(apply_count, Some(1));
            }
            _ => panic!("Expected Buff"),
        }
    }

    #[test]
    fn test_deserialize_recharge() {
        let json = r#"{"text": "Recharge", "type": "Recharge", "icon": "https://example.com/icon.png", "value": 8}"#;
        let fact: Fact = serde_json::from_str(json).unwrap();
        match fact {
            Fact::Recharge { value, .. } => assert_eq!(value, Some(8.0)),
            _ => panic!("Expected Recharge"),
        }
    }

    #[test]
    fn typed_facts_deserialize_without_an_owner_frame() {
        #[derive(Deserialize)]
        struct Bare {
            #[serde(deserialize_with = "deserialize_facts")]
            facts: Vec<Fact>,
        }
        let bare: Bare = serde_json::from_str(
            r#"{"facts":[
                {"type":"Recharge","value":8},
                {"type":"Range","value":300},
                {"type":"FutureFact"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(bare.facts.len(), 3);
        assert!(matches!(bare.facts[2], Fact::Unknown));
    }

    /// The old `test_lenient_facts_skips_missing_type` treated a shorter vec as
    /// success. A missing `type` with nowhere to record the drop must fail.
    #[test]
    fn missing_type_is_not_a_silent_skip() {
        #[derive(Deserialize)]
        struct Bare {
            #[serde(default, deserialize_with = "deserialize_facts")]
            #[allow(dead_code)]
            facts: Vec<Fact>,
        }
        let err = serde_json::from_str::<Bare>(
            r#"{"facts":[
                {"text":"Recharge","type":"Recharge","icon":"i.png","value":8},
                {"text":"Some effect","icon":"i.png","value":5},
                {"text":"Range","type":"Range","icon":"i.png","value":300}
            ]}"#,
        )
        .err()
        .expect("missing type must not deserialize");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse"),
            "silent skip must not be success: {msg}"
        );
    }
}
