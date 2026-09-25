//! GW2 chat-link encode/decode for the kitchen plate.
//!
//! Types (wiki + live API v2 `chat_link`):
//! - `0x02` item (quantity + 32-bit LE id)
//! - `0x06` skill
//! - `0x07` trait
//! - `0x0D` build template (encode lives in character.rs; we decode a label)

use base64::Engine;
use gw2_optimizer::gamedb::GameDb;
use gw2_optimizer::validation::ValidatedBuild;
use serde::{Deserialize, Serialize};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Item,
    Skill,
    Trait,
    Build,
}

impl LinkKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LinkKind::Item => "item",
            LinkKind::Skill => "skill",
            LinkKind::Trait => "trait",
            LinkKind::Build => "build",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChip {
    pub kind: LinkKind,
    pub label: String,
    pub code: String,
}

pub fn encode_item(id: u32) -> String {
    let mut buf = Vec::with_capacity(6);
    buf.push(0x02);
    buf.push(1);
    buf.extend_from_slice(&id.to_le_bytes());
    wrap(&buf)
}

pub fn encode_skill(id: u32) -> String {
    let mut buf = Vec::with_capacity(5);
    buf.push(0x06);
    buf.extend_from_slice(&id.to_le_bytes());
    wrap(&buf)
}

pub fn encode_trait(id: u32) -> String {
    let mut buf = Vec::with_capacity(5);
    buf.push(0x07);
    buf.extend_from_slice(&id.to_le_bytes());
    wrap(&buf)
}

fn wrap(bytes: &[u8]) -> String {
    format!("[&{}]", B64.encode(bytes))
}

fn inner_b64(code: &str) -> Option<&str> {
    let t = code.trim();
    t.strip_prefix("[&")?.strip_suffix(']')
}

pub fn decode(code: &str) -> Option<(LinkKind, Option<u32>)> {
    let bytes = B64.decode(inner_b64(code)?).ok()?;
    if bytes.is_empty() {
        return None;
    }
    match bytes[0] {
        0x02 if bytes.len() >= 5 => {
            // Wiki/API: type, qty, 3-byte id. Byte 5 is flags (skin/upgrade); never fold it into the id.
            let mut id = [0u8; 4];
            let n = (bytes.len() - 2).min(3);
            id[..n].copy_from_slice(&bytes[2..2 + n]);
            Some((LinkKind::Item, Some(u32::from_le_bytes(id))))
        }
        0x06 if bytes.len() >= 5 => Some((
            LinkKind::Skill,
            Some(u32::from_le_bytes(bytes[1..5].try_into().ok()?)),
        )),
        0x07 if bytes.len() >= 5 => Some((
            LinkKind::Trait,
            Some(u32::from_le_bytes(bytes[1..5].try_into().ok()?)),
        )),
        0x0D => Some((LinkKind::Build, None)),
        _ => None,
    }
}

/// Profession + elite (or core specs) from a `0x0D` template. Gear is not in the code.
pub fn decode_build_label(code: &str, db: Option<&GameDb>) -> String {
    let Some(bytes) = inner_b64(code).and_then(|s| B64.decode(s).ok()) else {
        return "Build template".into();
    };
    if bytes.first() != Some(&0x0D) || bytes.len() < 8 {
        return "Build template".into();
    }
    let prof_code = bytes[1] as u32;
    let spec_ids = [bytes[2] as u32, bytes[4] as u32, bytes[6] as u32];
    let Some(db) = db else {
        return format!("Build · profession {prof_code}");
    };
    let prof = db
        .professions
        .values()
        .find(|p| p.code == Some(prof_code))
        .map(|p| p.name.as_str())
        .unwrap_or("Build");
    let mut names = Vec::new();
    let mut elite = None;
    for id in spec_ids {
        if id == 0 {
            continue;
        }
        if let Some(spec) = db.specializations.get(&id) {
            if spec.elite {
                elite = Some(spec.name.as_str());
            }
            names.push(spec.name.as_str());
        }
    }
    if let Some(e) = elite {
        format!("{prof} · {e}")
    } else if names.is_empty() {
        format!("{prof} template")
    } else {
        format!("{prof} · {}", names.join(" / "))
    }
}

/// Byte spans of `[&...=]` codes. ASCII delimiters, UTF-8 safe.
pub fn find_code_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find("[&") {
        let start = pos + rel;
        let rest = &text[start + 2..];
        let Some(end_rel) = rest.find(']') else {
            break;
        };
        let end = start + 2 + end_rel + 1;
        let inner = &text[start + 2..end - 1];
        if !inner.is_empty()
            && inner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=')
        {
            spans.push((start, end));
            pos = end;
        } else {
            pos = start + 2;
        }
    }
    spans
}

pub fn compact_label(name: &str) -> String {
    let t = name.trim();
    t.strip_prefix("Superior ")
        .or_else(|| t.strip_prefix("superior "))
        .unwrap_or(t)
        .to_string()
}

fn item_code(db: &GameDb, id: u32) -> String {
    db.items
        .get(&id)
        .and_then(|i| i.chat_link.clone())
        .filter(|c| c.starts_with("[&"))
        .unwrap_or_else(|| encode_item(id))
}

fn skill_code(db: &GameDb, id: u32) -> String {
    db.skills
        .get(&id)
        .and_then(|s| s.chat_link.clone())
        .filter(|c| c.starts_with("[&"))
        .unwrap_or_else(|| encode_skill(id))
}

fn resolve_chip(code: &str, db: Option<&GameDb>) -> Option<ChatChip> {
    let (kind, id) = decode(code)?;
    let label = match (kind, id, db) {
        (LinkKind::Build, _, db) => decode_build_label(code, db),
        (LinkKind::Item, Some(id), Some(db)) => db
            .items
            .get(&id)
            .map(|i| compact_label(&i.name))
            .unwrap_or_else(|| format!("Item #{id}")),
        (LinkKind::Skill, Some(id), Some(db)) => db
            .skills
            .get(&id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("Skill #{id}")),
        (LinkKind::Trait, Some(id), Some(db)) => db
            .traits
            .get(&id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| format!("Trait #{id}")),
        (LinkKind::Item, Some(id), None) => format!("Item #{id}"),
        (LinkKind::Skill, Some(id), None) => format!("Skill #{id}"),
        (LinkKind::Trait, Some(id), None) => format!("Trait #{id}"),
        _ => "Link".into(),
    };
    Some(ChatChip {
        kind,
        label,
        code: code.to_string(),
    })
}

/// Replace pasted `[&...]` with names, collect chips, and a chef-facing order.
pub fn annotate_order(text: &str, db: Option<&GameDb>) -> (String, Vec<ChatChip>, String) {
    let spans = find_code_spans(text);
    if spans.is_empty() {
        return (text.to_string(), Vec::new(), text.to_string());
    }
    let mut display = String::with_capacity(text.len());
    let mut chips = Vec::new();
    let mut pos = 0;
    for (start, end) in spans {
        display.push_str(&text[pos..start]);
        let code = &text[start..end];
        if let Some(chip) = resolve_chip(code, db) {
            display.push_str(&chip.label);
            if !chips.iter().any(|c: &ChatChip| c.code == chip.code) {
                chips.push(chip);
            }
        } else {
            display.push_str(code);
        }
        pos = end;
    }
    display.push_str(&text[pos..]);

    let mut chef = display.clone();
    chef.push_str("\n\nPasted links:\n");
    for c in &chips {
        chef.push_str(&format!("- {} ({}) {}\n", c.label, c.kind.as_str(), c.code));
    }
    (display, chips, chef)
}

pub fn build_template_chip(code: &str) -> ChatChip {
    ChatChip {
        kind: LinkKind::Build,
        label: "Build template".into(),
        code: code.to_string(),
    }
}

fn push_unique(chips: &mut Vec<ChatChip>, chip: ChatChip) {
    if !chips.iter().any(|c| c.code == chip.code) {
        chips.push(chip);
    }
}

/// Structural gate for a model-supplied chat code before it becomes a chip
/// the user can paste into in-game chat: must decode, must be a `0x0D`
/// build template with a profession byte and plausible length. Prompt
/// injection through poisoned/compromised content could otherwise make the
/// model emit arbitrary link bytes that render as whatever the declared
/// type claims.
fn is_plausible_build_code(code: &str) -> bool {
    if code.len() > 512 {
        return false;
    }
    let Some(bytes) = inner_b64(code).and_then(|s| B64.decode(s).ok()) else {
        return false;
    };
    bytes.first() == Some(&0x0D) && bytes.len() >= 8
}

pub fn chips_from_plate(
    db: &GameDb,
    plated: &ValidatedBuild,
    build_code: Option<&str>,
) -> Vec<ChatChip> {
    let mut chips = Vec::new();
    if let Some(code) = build_code.filter(|c| is_plausible_build_code(c)) {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Build,
                label: decode_build_label(code, Some(db)),
                code: code.to_string(),
            },
        );
    }
    if let Some(rune) = &plated.rune {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Item,
                label: compact_label(&rune.name),
                code: item_code(db, rune.id),
            },
        );
    }
    for sigil in &plated.sigils {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Item,
                label: compact_label(&sigil.name),
                code: item_code(db, sigil.id),
            },
        );
    }
    if let Some(relic) = &plated.relic {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Item,
                label: compact_label(&relic.name),
                code: item_code(db, relic.id),
            },
        );
    }
    if let Some((id, name)) = &plated.skills.heal {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Skill,
                label: name.clone(),
                code: skill_code(db, *id),
            },
        );
    }
    for (id, name) in plated.skills.utilities.iter().flatten() {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Skill,
                label: name.clone(),
                code: skill_code(db, *id),
            },
        );
    }
    if let Some((id, name)) = &plated.skills.elite {
        push_unique(
            &mut chips,
            ChatChip {
                kind: LinkKind::Skill,
                label: name.clone(),
                code: skill_code(db, *id),
            },
        );
    }
    for spec in &plated.specializations {
        for (id, name) in spec.trait_ids.iter().zip(spec.trait_names.iter()) {
            push_unique(
                &mut chips,
                ChatChip {
                    kind: LinkKind::Trait,
                    label: name.clone(),
                    code: encode_trait(*id),
                },
            );
        }
    }
    chips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_item_matches_live_scholar_and_thief_relic() {
        assert_eq!(encode_item(24836), "[&AgEEYQAA]");
        assert_eq!(encode_item(100916), "[&AgE0igEA]");
    }

    #[test]
    fn encode_skill_matches_live_arcing_slice() {
        assert_eq!(encode_skill(14375), "[&Bic4AAA=]");
    }

    #[test]
    fn encode_trait_matches_wiki_sample() {
        assert_eq!(encode_trait(743), "[&B+cCAAA=]");
    }

    #[test]
    fn decode_roundtrips_item_skill_trait_build() {
        assert_eq!(decode("[&AgEEYQAA]"), Some((LinkKind::Item, Some(24836))));
        assert_eq!(decode("[&Bic4AAA=]"), Some((LinkKind::Skill, Some(14375))));
        assert_eq!(decode("[&B+cCAAA=]"), Some((LinkKind::Trait, Some(743))));
        let mut buf = vec![0x0D, 1];
        buf.extend_from_slice(&[0u8; 6]);
        assert_eq!(decode(&wrap(&buf)), Some((LinkKind::Build, None)));
    }

    #[test]
    fn annotate_order_accepts_gw2skills_virtuoso_template() {
        let code = "[&DQUcGzYnBxqFAAAAKRMAABkBAABYAAAAnwEAAAAAAAAAAAAAAAAAAAAAAAA=]";
        assert_eq!(decode(code), Some((LinkKind::Build, None)));
        let (display, chips, chef) = annotate_order(code, None);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].kind, LinkKind::Build);
        assert_eq!(chips[0].code, code);
        assert!(display.contains("profession"));
        assert!(chef.contains("Pasted links:"));
        let spans = find_code_spans(&format!("try {code} please"));
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn decode_item_ignores_flags_byte() {
        // qty=1, id=24836, flags=1 in byte 5 — must not become 16_802_052.
        let bytes = [0x02, 1, 0x04, 0x61, 0x00, 0x01];
        assert_eq!(decode(&wrap(&bytes)), Some((LinkKind::Item, Some(24836))));
    }

    #[test]
    fn decode_build_label_without_db_uses_profession_byte() {
        let mut bytes = vec![0x0D, 5, 7, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&[0u8; 20]);
        let code = wrap(&bytes);
        assert_eq!(decode_build_label(&code, None), "Build · profession 5");
    }

    #[test]
    fn decode_build_label_prefers_elite_spec_name() {
        let mut db = GameDb::empty_for_tests();
        let prof: gw2_api::models::Profession = serde_json::from_value(serde_json::json!({
            "id": "Thief",
            "name": "Thief",
            "code": 5,
            "specializations": [7],
            "weapons": {}
        }))
        .expect("prof");
        db.professions.insert("Thief".into(), prof);
        let spec: gw2_api::models::Specialization = serde_json::from_value(serde_json::json!({
            "id": 7,
            "name": "Daredevil",
            "profession": "Thief",
            "elite": true,
            "minor_traits": [],
            "major_traits": []
        }))
        .expect("spec");
        db.specializations.insert(7, spec);
        let mut bytes = vec![0x0D, 5, 7, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&[0u8; 20]);
        assert_eq!(
            decode_build_label(&wrap(&bytes), Some(&db)),
            "Thief · Daredevil"
        );
    }

    #[test]
    fn annotate_order_substitutes_codes_without_db() {
        let raw = "I want [&AgEEYQAA] and [&Bic4AAA=] please";
        let (display, chips, chef) = annotate_order(raw, None);
        assert_eq!(display, "I want Item #24836 and Skill #14375 please");
        assert_eq!(chips.len(), 2);
        assert_eq!(chips[0].code, "[&AgEEYQAA]");
        assert!(chef.contains("Pasted links:"));
        assert!(chef.contains("item"));
    }

    #[test]
    fn find_code_spans_skips_junk_brackets() {
        let text = "see [not a link] and [&AgEEYQAA] done";
        let spans = find_code_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].0..spans[0].1], "[&AgEEYQAA]");
    }

    #[test]
    fn chips_from_plate_includes_build_rune_skill() {
        let db = GameDb::empty_for_tests();
        let plated = ValidatedBuild {
            rune: Some(gw2_optimizer::validation::ValidatedItem {
                id: 24836,
                name: "Superior Rune of the Scholar".into(),
            }),
            skills: gw2_optimizer::validation::ValidatedSkills {
                heal: Some((123, "Shelter".into())),
                ..Default::default()
            },
            ..Default::default()
        };
        let chips = chips_from_plate(&db, &plated, Some("[&DQkAAAAAAAA=]"));
        assert_eq!(chips[0].kind, LinkKind::Build);
        assert_eq!(chips[1].label, "Rune of the Scholar");
        assert_eq!(chips[1].code, "[&AgEEYQAA]");
        assert_eq!(chips[2].kind, LinkKind::Skill);
        assert_eq!(chips[2].label, "Shelter");
        assert_eq!(chips[2].code, encode_skill(123));
    }

    #[test]
    fn chips_from_plate_includes_traits_and_dedupes() {
        let db = GameDb::empty_for_tests();
        let mut plated = ValidatedBuild::default();
        plated
            .specializations
            .push(gw2_optimizer::validation::ValidatedSpec {
                spec_id: 1,
                name: "Arms".into(),
                elite: false,
                trait_ids: vec![743, 743],
                trait_names: vec!["Signet Mastery".into(), "Signet Mastery".into()],
                all_trait_ids: vec![743, 743],
            });
        let chips = chips_from_plate(&db, &plated, None);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].kind, LinkKind::Trait);
        assert_eq!(chips[0].label, "Signet Mastery");
        assert_eq!(chips[0].code, encode_trait(743));
    }

    #[test]
    fn annotate_order_uses_item_name_from_gamedb() {
        let mut db = GameDb::empty_for_tests();
        let item: gw2_api::models::Item = serde_json::from_value(serde_json::json!({
            "id": 24836,
            "name": "Superior Rune of the Scholar",
            "type": "UpgradeComponent",
            "rarity": "Exotic",
            "level": 60,
            "chat_link": "[&AgEEYQAA]"
        }))
        .expect("item");
        db.items.insert(24836, item);
        let (display, chips, _) = annotate_order("give me [&AgEEYQAA]", Some(&db));
        assert_eq!(display, "give me Rune of the Scholar");
        assert_eq!(chips[0].label, "Rune of the Scholar");
    }
}
