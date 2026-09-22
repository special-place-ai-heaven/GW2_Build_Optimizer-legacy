//! Per-hand weapon legality from the wiki usability table.
//!
//! `/v2/professions` stores one `specialization` per weapon *type*. The wiki
//! table is per hand (Guardian Sword OH is Willbender; Ranger Dagger OH is
//! core). This module is the shared source of truth for that table.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

use super::{try_load, DataLoadError};

const WEAPON_HANDS_JSON: &str = include_str!("../../../../data/weapon_hands.json");
const SOURCE_URL: &str = "https://wiki.guildwars2.com/wiki/Weapon#Weapon_usability_by_professions";

const PROFESSIONS: &[&str] = &[
    "Guardian",
    "Revenant",
    "Warrior",
    "Engineer",
    "Ranger",
    "Thief",
    "Elementalist",
    "Mesmer",
    "Necromancer",
];

const ONE_HAND: &[&str] = &[
    "axe", "dagger", "mace", "pistol", "sword", "scepter", "focus", "shield", "torch", "warhorn",
];
const TWO_HAND: &[&str] = &[
    "greatsword",
    "hammer",
    "longbow",
    "rifle",
    "shortbow",
    "staff",
    "spear",
];

static TABLE: OnceLock<WeaponHandTable> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    Main,
    Off,
    TwoHand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeaponAccess {
    None,
    Core,
    Elite(String),
    ExpandedSoto,
    SpearJw,
}

#[derive(Debug, thiserror::Error)]
pub enum WeaponHandsError {
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    ValidationError(String),
}

#[derive(Debug, Deserialize)]
struct WeaponHandsFile {
    source: String,
    table: HashMap<String, HashMap<String, HandsRaw>>,
}

#[derive(Debug, Deserialize)]
struct HandsRaw {
    #[serde(default)]
    main: Option<String>,
    #[serde(default)]
    off: Option<String>,
    #[serde(default)]
    twohand: Option<String>,
}

#[derive(Debug, Clone)]
struct Hands {
    main: WeaponAccess,
    off: WeaponAccess,
    twohand: WeaponAccess,
}

struct WeaponHandTable {
    /// profession lowercase → weapon_type_key → hands
    rows: HashMap<String, HashMap<String, Hands>>,
}

fn table() -> &'static WeaponHandTable {
    TABLE.get_or_init(|| {
        load_weapon_hands(WEAPON_HANDS_JSON).expect("embedded weapon_hands.json is invalid")
    })
}

/// Health-check loader: does not store in `OnceLock`.
pub fn try_load_weapon_hands() -> Result<(), Vec<DataLoadError>> {
    try_load!(
        "weapon_hands",
        load_weapon_hands(WEAPON_HANDS_JSON).map(|_| ()),
        WeaponHandsError
    )
}

fn load_weapon_hands(json: &str) -> Result<WeaponHandTable, WeaponHandsError> {
    let file: WeaponHandsFile = serde_json::from_str(json)?;
    if file.source != SOURCE_URL {
        return Err(WeaponHandsError::ValidationError(format!(
            "source must be {SOURCE_URL}"
        )));
    }
    let mut rows = HashMap::new();
    for prof in PROFESSIONS {
        let Some(weapons) = file.table.get(*prof) else {
            return Err(WeaponHandsError::ValidationError(format!(
                "missing profession {prof}"
            )));
        };
        let mut hands_map = HashMap::new();
        for key in ONE_HAND {
            let raw = weapons.get(*key).ok_or_else(|| {
                WeaponHandsError::ValidationError(format!("{prof} missing one-hand {key}"))
            })?;
            if raw.twohand.is_some() || raw.main.is_none() || raw.off.is_none() {
                return Err(WeaponHandsError::ValidationError(format!(
                    "{prof} {key} must have main+off only"
                )));
            }
            hands_map.insert(
                (*key).to_string(),
                Hands {
                    main: parse_access(raw.main.as_deref().unwrap_or("none"))?,
                    off: parse_access(raw.off.as_deref().unwrap_or("none"))?,
                    twohand: WeaponAccess::None,
                },
            );
        }
        for key in TWO_HAND {
            let raw = weapons.get(*key).ok_or_else(|| {
                WeaponHandsError::ValidationError(format!("{prof} missing two-hand {key}"))
            })?;
            if raw.twohand.is_none() || raw.main.is_some() || raw.off.is_some() {
                return Err(WeaponHandsError::ValidationError(format!(
                    "{prof} {key} must have twohand only"
                )));
            }
            hands_map.insert(
                (*key).to_string(),
                Hands {
                    main: WeaponAccess::None,
                    off: WeaponAccess::None,
                    twohand: parse_access(raw.twohand.as_deref().unwrap_or("none"))?,
                },
            );
        }
        rows.insert(prof.to_ascii_lowercase(), hands_map);
    }
    Ok(WeaponHandTable { rows })
}

fn parse_access(raw: &str) -> Result<WeaponAccess, WeaponHandsError> {
    match raw.trim() {
        "" => Err(WeaponHandsError::ValidationError(
            "empty weapon access".into(),
        )),
        "none" => Ok(WeaponAccess::None),
        "core" => Ok(WeaponAccess::Core),
        "soto" => Ok(WeaponAccess::ExpandedSoto),
        "jw" => Ok(WeaponAccess::SpearJw),
        name => Ok(WeaponAccess::Elite(name.to_string())),
    }
}

fn lookup<'a>(table: &'a WeaponHandTable, profession: &str, weapon: &str) -> Option<&'a Hands> {
    let prof = profession.to_ascii_lowercase();
    let key = gw2_core::i18n::weapon_type_key(weapon);
    table.rows.get(&prof)?.get(&key)
}

/// True when `name` is a real GW2 weapon type, in any language the i18n key
/// table knows. The one list a record's `Gate::Weapon` is checked against.
pub fn is_weapon_type(name: &str) -> bool {
    let key = gw2_core::i18n::weapon_type_key(name);
    ONE_HAND.contains(&key.as_str()) || TWO_HAND.contains(&key.as_str())
}

/// True when the wiki table has a row for this profession + weapon type.
pub(crate) fn known_weapon(profession: &str, weapon: &str) -> bool {
    lookup(table(), profession, weapon).is_some()
}

pub fn access(profession: &str, weapon: &str, hand: Hand) -> WeaponAccess {
    let Some(hands) = lookup(table(), profession, weapon) else {
        return WeaponAccess::None;
    };
    match hand {
        Hand::Main => hands.main.clone(),
        Hand::Off => hands.off.clone(),
        Hand::TwoHand => hands.twohand.clone(),
    }
}

/// Can this profession hold this weapon in this hand on land?
///
/// Weaponmaster Training (SotO) unlocked every elite specialization's weapon
/// for every build of that profession, so `Elite(name)` records which spec
/// *brought* the weapon, not a spec the build must equip — GuildJen publishes
/// Catalyst Hammer on a Weaver and Tempest Warhorn on a Catalyst. Only
/// `None`, a hand the profession never trains, is illegal.
pub fn is_legal(profession: &str, weapon: &str, hand: Hand) -> bool {
    !matches!(access(profession, weapon, hand), WeaponAccess::None)
}

pub fn choya_label(access: &WeaponAccess) -> String {
    match access {
        WeaponAccess::None | WeaponAccess::Core => String::new(),
        WeaponAccess::Elite(name) => format!(" (requires {name} or Weaponmaster Training)"),
        WeaponAccess::ExpandedSoto => " (requires SotO Expanded Weapon Proficiency)".to_string(),
        WeaponAccess::SpearJw => " (requires Janthir Wilds Lowland Spear Training)".to_string(),
    }
}

impl WeaponAccess {
    /// Tool JSON: `"core"` | elite name | `"expanded_soto"` | `"spear_jw"`.
    pub fn json_token(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Core => Some("core"),
            Self::Elite(name) => Some(name.as_str()),
            Self::ExpandedSoto => Some("expanded_soto"),
            Self::SpearJw => Some("spear_jw"),
        }
    }
}

const LAND_WEAPON_NAMES: &[&str] = &[
    "Axe",
    "Dagger",
    "Focus",
    "Greatsword",
    "Hammer",
    "Longbow",
    "Mace",
    "Pistol",
    "Rifle",
    "Scepter",
    "Shield",
    "Shortbow",
    "Spear",
    "Staff",
    "Sword",
    "Torch",
    "Warhorn",
];

/// Sorted land weapons with at least one legal hand for `profession`.
pub fn land_weapons(profession: &str) -> Vec<&'static str> {
    LAND_WEAPON_NAMES
        .iter()
        .copied()
        .filter(|weapon| {
            [Hand::Main, Hand::Off, Hand::TwoHand]
                .into_iter()
                .any(|hand| !matches!(access(profession, weapon, hand), WeaponAccess::None))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guardian_sword_main_core_off_willbender() {
        assert_eq!(access("Guardian", "Sword", Hand::Main), WeaponAccess::Core);
        assert_eq!(
            access("Guardian", "Sword", Hand::Off),
            WeaponAccess::Elite("Willbender".into())
        );
    }

    #[test]
    fn revenant_sword_both_core() {
        assert_eq!(access("Revenant", "Sword", Hand::Main), WeaponAccess::Core);
        assert_eq!(access("Revenant", "Sword", Hand::Off), WeaponAccess::Core);
    }

    #[test]
    fn ranger_dagger_main_soulbeast_off_core() {
        assert_eq!(
            access("Ranger", "Dagger", Hand::Main),
            WeaponAccess::Elite("Soulbeast".into())
        );
        assert_eq!(access("Ranger", "Dagger", Hand::Off), WeaponAccess::Core);
    }

    #[test]
    fn ranger_sword_off_none() {
        assert_eq!(access("Ranger", "Sword", Hand::Off), WeaponAccess::None);
    }

    #[test]
    fn revenant_shield_off_herald() {
        assert_eq!(
            access("Revenant", "Shield", Hand::Off),
            WeaponAccess::Elite("Herald".into())
        );
    }

    #[test]
    fn weapon_type_key_aliases_match() {
        assert_eq!(
            access("Guardian", "Harpoon", Hand::TwoHand),
            WeaponAccess::SpearJw
        );
        assert_eq!(
            access("Ranger", "Short Bow", Hand::TwoHand),
            WeaponAccess::Core
        );
    }
}
