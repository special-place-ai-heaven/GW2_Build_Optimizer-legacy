//! Item data from /v2/items.
//! Covers armor, weapons, trinkets, upgrade components (runes, sigils),
//! back items, and relics — all equipment relevant to build optimization.
//!
//! `ItemDetails` is a flat struct with all optional fields since the API
//! details object varies by item type but has no discriminant tag.
//! Use `Item::item_type` to determine which fields are populated.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: u32,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub rarity: String,
    pub level: u32,
    pub vendor_value: Option<u32>,
    pub chat_link: Option<String>,
    pub default_skin: Option<u32>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub game_types: Vec<String>,
    #[serde(default)]
    pub restrictions: Vec<String>,
    pub details: Option<ItemDetails>,
}

/// Flat details struct covering all item types.
/// Fields are populated based on `Item::item_type`:
/// - Armor: armor_type, weight_class, defense
/// - Weapon: weapon_type, damage_type, min_power, max_power
/// - Trinket: trinket_type
/// - UpgradeComponent: upgrade_type, suffix, bonuses
/// - Consumable (Food / Utility): description
/// - Common: infusion_slots, attribute_adjustment, infix_upgrade, stat_choices
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemDetails {
    #[serde(rename = "type")]
    pub detail_type: Option<String>,

    pub weight_class: Option<String>,
    pub defense: Option<u32>,

    pub damage_type: Option<String>,
    pub min_power: Option<u32>,
    pub max_power: Option<u32>,

    pub suffix: Option<String>,
    #[serde(default)]
    pub bonuses: Vec<String>,
    #[serde(default)]
    pub infusion_upgrade_flags: Vec<String>,

    #[serde(default)]
    pub infusion_slots: Vec<InfusionSlot>,
    pub attribute_adjustment: Option<f64>,
    pub infix_upgrade: Option<InfixUpgrade>,
    pub suffix_item_id: Option<u32>,
    pub secondary_suffix_item_id: Option<String>,
    #[serde(default)]
    pub stat_choices: Vec<u32>,
    /// Consumable (Food / Utility) tooltip lines, e.g. "+100 Power".
    pub description: Option<String>,
}

/// Stat bonuses built into an item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfixUpgrade {
    pub id: Option<u32>,
    #[serde(default)]
    pub attributes: Vec<InfixAttribute>,
    pub buff: Option<InfixBuff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfixAttribute {
    pub attribute: String,
    pub modifier: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfixBuff {
    pub skill_id: Option<u32>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfusionSlot {
    #[serde(default)]
    pub flags: Vec<String>,
    pub item_id: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_item_base() {
        let json = r#"{
            "id": 12345,
            "name": "Berserker's Greatsword",
            "type": "Weapon",
            "rarity": "Ascended",
            "level": 80,
            "vendor_value": 100,
            "flags": ["SoulbindOnAcquire"],
            "game_types": ["PvE", "WvW"],
            "restrictions": []
        }"#;
        let item: Item = serde_json::from_str(json).unwrap();
        assert_eq!(item.id, 12345);
        assert_eq!(item.item_type, "Weapon");
        assert_eq!(item.rarity, "Ascended");
    }

    #[test]
    fn test_deserialize_armor_details() {
        let json = r#"{
            "id": 80248,
            "name": "Perfected Envoy Helmet",
            "type": "Armor",
            "rarity": "Legendary",
            "level": 80,
            "details": {
                "type": "Helm",
                "weight_class": "Heavy",
                "defense": 127,
                "infusion_slots": [{"flags": ["Infusion"]}],
                "attribute_adjustment": 47.0,
                "stat_choices": [584, 656]
            }
        }"#;
        let item: Item = serde_json::from_str(json).unwrap();
        let details = item.details.unwrap();
        assert_eq!(details.detail_type.as_deref(), Some("Helm"));
        assert_eq!(details.weight_class.as_deref(), Some("Heavy"));
        assert_eq!(details.defense, Some(127));
    }

    #[test]
    fn test_deserialize_back_item() {
        let json = r#"{
            "id": 77474,
            "name": "Ad Infinitum",
            "type": "Back",
            "rarity": "Legendary",
            "level": 80,
            "details": {
                "infusion_slots": [{"flags": ["Infusion"]}, {"flags": ["Infusion"]}],
                "attribute_adjustment": 63.0,
                "stat_choices": [584, 656, 1163]
            }
        }"#;
        let item: Item = serde_json::from_str(json).unwrap();
        assert_eq!(item.item_type, "Back");
        let details = item.details.unwrap();
        assert_eq!(details.infusion_slots.len(), 2);
        assert!(details.attribute_adjustment.is_some());
    }
}
