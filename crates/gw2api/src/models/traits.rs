//! Trait data from /v2/traits.
//! Traits define passive and triggered effects within specialization lines.
//! The `facts` and `traited_facts` fields are critical for understanding
//! synergies, proc conditions, and stat modifications.

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use super::facts::{
    deserialize_facts, deserialize_traited_facts, Fact, FactDropFrame, TraitedFact,
};

#[derive(Debug, Clone, Serialize)]
pub struct Trait {
    pub id: u32,
    pub name: String,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub specialization: u32,
    pub tier: u32,
    pub order: u32,
    pub slot: String, // "Major" or "Minor"
    pub facts: Vec<Fact>,
    pub traited_facts: Vec<TraitedFact>,
    pub skills: Vec<TraitSkill>,
    /// See [`crate::models::Skill::fact_parse_drops`].
    #[serde(skip_serializing_if = "super::facts::is_zero_u32")]
    pub fact_parse_drops: u32,
}

impl<'de> Deserialize<'de> for Trait {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let frame = FactDropFrame::enter();
        let raw = TraitDe::deserialize(deserializer)?;
        Ok(raw.finish(frame.live_drops()))
    }
}

#[derive(Deserialize)]
struct TraitDe {
    id: u32,
    name: String,
    icon: Option<String>,
    description: Option<String>,
    specialization: u32,
    tier: u32,
    order: u32,
    slot: String,
    #[serde(default, deserialize_with = "deserialize_facts")]
    facts: Vec<Fact>,
    #[serde(default, deserialize_with = "deserialize_traited_facts")]
    traited_facts: Vec<TraitedFact>,
    #[serde(default)]
    skills: Vec<TraitSkill>,
    #[serde(default)]
    fact_parse_drops: u32,
}

impl TraitDe {
    fn finish(self, live: u32) -> Trait {
        // ponytail: same stamp rule as Skill. Nested trait skills keep their own counts.
        Trait {
            id: self.id,
            name: self.name,
            icon: self.icon,
            description: self.description,
            specialization: self.specialization,
            tier: self.tier,
            order: self.order,
            slot: self.slot,
            facts: self.facts,
            traited_facts: self.traited_facts,
            skills: self.skills,
            fact_parse_drops: live.max(self.fact_parse_drops),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TraitSkill {
    pub id: u32,
    pub name: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub facts: Vec<Fact>,
    pub traited_facts: Vec<TraitedFact>,
    #[serde(skip_serializing_if = "super::facts::is_zero_u32")]
    pub fact_parse_drops: u32,
}

impl<'de> Deserialize<'de> for TraitSkill {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let frame = FactDropFrame::enter();
        let raw = TraitSkillDe::deserialize(deserializer)?;
        Ok(raw.finish(frame.live_drops()))
    }
}

#[derive(Deserialize)]
struct TraitSkillDe {
    id: u32,
    name: Option<String>,
    description: Option<String>,
    icon: Option<String>,
    #[serde(default, deserialize_with = "deserialize_facts")]
    facts: Vec<Fact>,
    #[serde(default, deserialize_with = "deserialize_traited_facts")]
    traited_facts: Vec<TraitedFact>,
    #[serde(default)]
    fact_parse_drops: u32,
}

impl TraitSkillDe {
    fn finish(self, live: u32) -> TraitSkill {
        TraitSkill {
            id: self.id,
            name: self.name,
            description: self.description,
            icon: self.icon,
            facts: self.facts,
            traited_facts: self.traited_facts,
            fact_parse_drops: live.max(self.fact_parse_drops),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_trait_raging_storm() {
        let json = r#"{
            "id": 214,
            "tier": 2,
            "order": 1,
            "name": "Raging Storm",
            "description": "Critically striking a foe grants fury.",
            "slot": "Major",
            "facts": [
                {"text": "Recharge", "type": "Recharge", "icon": "https://example.com/i.png", "value": 8},
                {"type": "AttributeAdjust", "icon": "https://example.com/i.png", "value": 180, "target": "CritDamage"},
                {"text": "Apply Buff/Condition", "type": "Buff", "icon": "https://example.com/i.png", "duration": 4, "status": "Fury", "description": "Crit chance increased.", "apply_count": 1},
                {"text": "Radius", "type": "Distance", "icon": "https://example.com/i.png", "distance": 360},
                {"text": "Number of Targets", "type": "Number", "icon": "https://example.com/i.png", "value": 5}
            ],
            "specialization": 41,
            "icon": "https://example.com/icon.png"
        }"#;
        let t: Trait = serde_json::from_str(json).unwrap();
        assert_eq!(t.id, 214);
        assert_eq!(t.name, "Raging Storm");
        assert_eq!(t.slot, "Major");
        assert_eq!(t.tier, 2);
        assert_eq!(t.facts.len(), 5);
        assert_eq!(t.fact_parse_drops, 0);
    }

    #[test]
    fn nested_trait_skill_drops_stay_on_their_own_id() {
        let t: Trait = serde_json::from_str(
            r#"{
                "id": 880015,
                "name": "Probe Trait",
                "specialization": 1,
                "tier": 1,
                "order": 0,
                "slot": "Major",
                "facts": [
                    {"type": "Recharge", "value": 1},
                    {"text": "no type"}
                ],
                "skills": [{
                    "id": 880016,
                    "facts": [
                        {"type": "Range", "value": 100},
                        {"text": "no type"}
                    ]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(t.facts.len(), 1);
        assert_eq!(t.fact_parse_drops, 1);
        assert_eq!(t.skills[0].facts.len(), 1);
        assert_eq!(t.skills[0].fact_parse_drops, 1);
        let loaded: Trait = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(loaded.fact_parse_drops, 1);
        assert_eq!(loaded.skills[0].fact_parse_drops, 1);
    }
}
