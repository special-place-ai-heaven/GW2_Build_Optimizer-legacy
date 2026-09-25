//! Skill data from /v2/skills.
//! Skills define abilities — their damage multipliers, cooldowns, costs,
//! buff/condition applications, and how they change with traits (traited_facts).

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use super::facts::{
    deserialize_facts, deserialize_traited_facts, Fact, FactDropFrame, TraitedFact,
};

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub id: u32,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub chat_link: Option<String>,
    #[serde(rename = "type")]
    pub skill_type: Option<String>,
    pub weapon_type: Option<String>,
    pub professions: Vec<String>,
    pub slot: Option<String>,
    pub facts: Vec<Fact>,
    pub traited_facts: Vec<TraitedFact>,
    pub categories: Vec<String>,
    pub attunement: Option<String>,
    pub cost: Option<u32>,
    pub dual_wield: Option<String>,
    pub flip_skill: Option<u32>,
    pub initiative: Option<u32>,
    pub next_chain: Option<u32>,
    pub prev_chain: Option<u32>,
    pub transform_skills: Vec<u32>,
    pub bundle_skills: Vec<u32>,
    pub toolbelt_skill: Option<u32>,
    pub flags: Vec<String>,
    pub specialization: Option<u32>,
    /// Fact objects that failed to parse. A cache save drops those objects, so
    /// the stamp is what the next load uses. Omitted from JSON when zero.
    #[serde(skip_serializing_if = "super::facts::is_zero_u32")]
    pub fact_parse_drops: u32,
}

impl<'de> Deserialize<'de> for Skill {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let frame = FactDropFrame::enter();
        let raw = SkillDe::deserialize(deserializer)?;
        Ok(raw.finish(frame.live_drops()))
    }
}

#[derive(Deserialize)]
struct SkillDe {
    id: u32,
    name: String,
    description: Option<String>,
    icon: Option<String>,
    chat_link: Option<String>,
    #[serde(rename = "type")]
    skill_type: Option<String>,
    weapon_type: Option<String>,
    #[serde(default)]
    professions: Vec<String>,
    slot: Option<String>,
    #[serde(default, deserialize_with = "deserialize_facts")]
    facts: Vec<Fact>,
    #[serde(default, deserialize_with = "deserialize_traited_facts")]
    traited_facts: Vec<TraitedFact>,
    #[serde(default)]
    categories: Vec<String>,
    attunement: Option<String>,
    cost: Option<u32>,
    dual_wield: Option<String>,
    flip_skill: Option<u32>,
    initiative: Option<u32>,
    next_chain: Option<u32>,
    prev_chain: Option<u32>,
    #[serde(default)]
    transform_skills: Vec<u32>,
    #[serde(default)]
    bundle_skills: Vec<u32>,
    toolbelt_skill: Option<u32>,
    #[serde(default)]
    flags: Vec<String>,
    specialization: Option<u32>,
    #[serde(default)]
    fact_parse_drops: u32,
}

impl SkillDe {
    fn finish(self, live: u32) -> Skill {
        // ponytail: max(live, stamp). Cache JSON no longer contains the bad
        // objects, so the stamp is the count on reload. Upgrade: persist the
        // failed objects if traited-fact override indices must survive a drop.
        let fact_parse_drops = live.max(self.fact_parse_drops);
        Skill {
            id: self.id,
            name: self.name,
            description: self.description,
            icon: self.icon,
            chat_link: self.chat_link,
            skill_type: self.skill_type,
            weapon_type: self.weapon_type,
            professions: self.professions,
            slot: self.slot,
            facts: self.facts,
            traited_facts: self.traited_facts,
            categories: self.categories,
            attunement: self.attunement,
            cost: self.cost,
            dual_wield: self.dual_wield,
            flip_skill: self.flip_skill,
            initiative: self.initiative,
            next_chain: self.next_chain,
            prev_chain: self.prev_chain,
            transform_skills: self.transform_skills,
            bundle_skills: self.bundle_skills,
            toolbelt_skill: self.toolbelt_skill,
            flags: self.flags,
            specialization: self.specialization,
            fact_parse_drops,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_skill() {
        let json = r#"{
            "name": "Arcing Slice",
            "id": 14375,
            "description": "Burst. Deliver a circular attack.",
            "icon": "https://example.com/icon.png",
            "chat_link": "[&Byc4AAA=]",
            "type": "Profession",
            "weapon_type": "None",
            "professions": ["Warrior"],
            "slot": "Profession_1",
            "cost": 30,
            "flip_skill": 14545,
            "categories": ["Burst"],
            "facts": [
                {"text": "Range", "type": "Range", "value": 150},
                {"text": "Recharge", "type": "Recharge", "value": 8}
            ]
        }"#;
        let skill: Skill = serde_json::from_str(json).unwrap();
        assert_eq!(skill.id, 14375);
        assert_eq!(skill.name, "Arcing Slice");
        assert_eq!(skill.cost, Some(30));
        assert_eq!(skill.professions, vec!["Warrior"]);
        assert_eq!(skill.facts.len(), 2);
        assert_eq!(skill.fact_parse_drops, 0);
    }

    #[test]
    fn missing_type_fact_is_kept_as_a_counted_drop() {
        let skill: Skill = serde_json::from_str(
            r#"{
                "id": 880014,
                "name": "Parse Drop Probe",
                "facts": [
                    {"text": "Recharge", "type": "Recharge", "value": 8},
                    {"text": "Some effect", "icon": "i.png", "value": 5}
                ],
                "traited_facts": [
                    {"requires_trait": 1, "type": "Range", "value": 300},
                    {"text": "no type"}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(skill.facts.len(), 1);
        assert!(matches!(skill.facts[0], Fact::Recharge { .. }));
        assert_eq!(skill.traited_facts.len(), 1);
        assert_eq!(skill.fact_parse_drops, 2);
        let saved = serde_json::to_string(&skill).unwrap();
        let loaded: Skill = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded.facts.len(), 1);
        assert_eq!(loaded.fact_parse_drops, 2);
    }
}
