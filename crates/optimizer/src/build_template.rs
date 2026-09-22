//! Guild Wars 2 build template chat links (`[&DQ...]`).
//!
//! Byte layout is the wiki Chat_link_format ruleset. Encode and decode are
//! inverses of the same `to_bytes` / `from_bytes` math — little-endian `u16`
//! palettes after an 8-byte header, then a 16-byte profession tail, then an
//! optional SotO weapon / skill-override trailer.
//!
//! Skill slots store **palette** ids, not `/v2/skills` ids. Palette 3875 is
//! Mesmer Signet of the Ether; the skill id is 21750. Resolve with
//! `GameDb::palette_to_skill`.
//!
//! | Offset | Contents                                          |
//! |--------|---------------------------------------------------|
//! | 0      | `0x0D`, the build-template link type              |
//! | 1      | Profession code (1 Guardian … 9 Revenant)         |
//! | 2..8   | Three pairs of (specialization id, packed traits) |
//! | 8..28  | 10× `u16` LE: land/water heal, util×3, elite      |
//! | 28..44 | Profession-specific: ranger pets, revenant legends|
//! | 44..   | SotO: `u8` weapon count, `u16` types, `u8` overrides, `u32` skill ids |
//!
//! Each packed-trait byte holds three 2-bit fields, adept first: `0` means no
//! trait chosen in that tier, `1..=3` selects one of the tier's three traits.

use base64::Engine;

/// The link type byte that marks a build template.
const BUILD_TEMPLATE_KIND: u8 = 0x0D;
/// Header, three specialization pairs, and the ten skill slots.
const MIN_LEN: usize = 28;
const TAIL_LEN: usize = 16;
const CORE_LEN: usize = 44;

/// One specialization line of a build template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TemplateSpec {
    /// Specialization id, matching `GameDb::specializations`. `0` = empty slot.
    pub id: u32,
    /// Chosen trait per tier, adept first. `0` = none, `1..=3` = which of the
    /// three traits in that tier.
    pub choices: [u8; 3],
}

/// A decoded `[&DQ...]` build template.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildTemplate {
    /// Profession code as the API's `Profession::code`.
    pub profession: u32,
    /// The three specialization slots, in order. Slot 3 may be elite.
    pub specs: [TemplateSpec; 3],
    /// Terrestrial skill palette ids: heal, three utilities, elite. `0` =
    /// empty. These are *palette* ids — `GameDb::palette_to_skill` maps them
    /// to skill ids.
    pub skills: [u32; 5],
    /// Aquatic skill palette ids, same slot order as [`Self::skills`].
    pub aquatic: [u32; 5],
    /// Ranger pets / Revenant legends + inactive-legend palettes. Zeros
    /// for every other profession.
    pub profession_bytes: [u8; TAIL_LEN],
    /// SotO terrestrial weapon type ids (axe=5 … spear=265). Empty = no trailer.
    pub weapons: Vec<u16>,
    /// Weaponmaster skill-override ids (`/v2/skills`), after the weapon list.
    pub skill_overrides: Vec<u32>,
}

impl BuildTemplate {
    /// Trait ids for one specialization, resolved against its major traits.
    ///
    /// `major_traits` is the specialization's nine majors in tier order, as
    /// the API gives them. A tier with no choice, or a specialization whose
    /// major list is not nine long, contributes nothing rather than guessing.
    pub fn trait_ids(spec: &TemplateSpec, major_traits: &[u32]) -> Vec<u32> {
        if major_traits.len() != 9 {
            return Vec::new();
        }
        spec.choices
            .iter()
            .enumerate()
            .filter_map(|(tier, &choice)| {
                let pick = usize::from(choice.checked_sub(1)?);
                (pick < 3).then(|| major_traits[tier * 3 + pick])
            })
            .collect()
    }

    /// Wiki rule: the client refuses a template when the same *non-zero*
    /// palette sits in two slots of one realm.
    pub fn same_realm_duplicate(&self) -> bool {
        realm_duplicate(&self.skills) || realm_duplicate(&self.aquatic)
    }
}

fn realm_duplicate(pals: &[u32; 5]) -> bool {
    let mut seen = [0u32; 5];
    let mut n = 0;
    for &p in pals {
        if p == 0 {
            continue;
        }
        if seen[..n].contains(&p) {
            return true;
        }
        seen[n] = p;
        n += 1;
    }
    false
}

fn packed_traits(spec: &TemplateSpec) -> u8 {
    (spec.choices[0] & 0b11) | ((spec.choices[1] & 0b11) << 2) | ((spec.choices[2] & 0b11) << 4)
}

/// Serialize the template to the exact byte string the game pastes.
pub fn to_bytes(t: &BuildTemplate) -> Vec<u8> {
    let mut buf = Vec::with_capacity(CORE_LEN);
    buf.push(BUILD_TEMPLATE_KIND);
    buf.push(t.profession.min(255) as u8);
    for spec in &t.specs {
        buf.push(spec.id.min(255) as u8);
        buf.push(packed_traits(spec));
    }
    for i in 0..5 {
        buf.extend_from_slice(&(t.skills[i] as u16).to_le_bytes());
        buf.extend_from_slice(&(t.aquatic[i] as u16).to_le_bytes());
    }
    buf.extend_from_slice(&t.profession_bytes);
    if !t.weapons.is_empty() || !t.skill_overrides.is_empty() {
        let n = t.weapons.len().min(8);
        buf.push(n as u8);
        for &id in t.weapons.iter().take(n) {
            buf.extend_from_slice(&id.to_le_bytes());
        }
        let m = t.skill_overrides.len().min(255);
        buf.push(m as u8);
        for &id in t.skill_overrides.iter().take(m) {
            buf.extend_from_slice(&id.to_le_bytes());
        }
    }
    buf
}

/// Parse the wiki layout. `None` if this is not a build template.
pub fn from_bytes(bytes: &[u8]) -> Option<BuildTemplate> {
    if bytes.first() != Some(&BUILD_TEMPLATE_KIND) || bytes.len() < MIN_LEN {
        return None;
    }

    let spec = |i: usize| {
        let packed = bytes[3 + i * 2];
        TemplateSpec {
            id: u32::from(bytes[2 + i * 2]),
            choices: [packed & 0b11, (packed >> 2) & 0b11, (packed >> 4) & 0b11],
        }
    };
    let u16_at = |at: usize| u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]));

    let mut profession_bytes = [0u8; TAIL_LEN];
    if bytes.len() >= CORE_LEN {
        profession_bytes.copy_from_slice(&bytes[28..CORE_LEN]);
    } else if bytes.len() > 28 {
        profession_bytes[..bytes.len() - 28].copy_from_slice(&bytes[28..]);
    }

    let (weapons, skill_overrides) = if bytes.len() > CORE_LEN {
        parse_soto(&bytes[CORE_LEN..])?
    } else {
        (Vec::new(), Vec::new())
    };

    Some(BuildTemplate {
        profession: u32::from(bytes[1]),
        specs: [spec(0), spec(1), spec(2)],
        skills: [u16_at(8), u16_at(12), u16_at(16), u16_at(20), u16_at(24)],
        aquatic: [u16_at(10), u16_at(14), u16_at(18), u16_at(22), u16_at(26)],
        profession_bytes,
        weapons,
        skill_overrides,
    })
}

fn parse_soto(rest: &[u8]) -> Option<(Vec<u16>, Vec<u32>)> {
    if rest.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }
    let n = usize::from(rest[0]);
    let weapons_end = 1 + n * 2;
    if rest.len() < weapons_end + 1 {
        return None;
    }
    let mut weapons = Vec::with_capacity(n);
    for i in 0..n {
        let at = 1 + i * 2;
        weapons.push(u16::from_le_bytes([rest[at], rest[at + 1]]));
    }
    let after = &rest[weapons_end..];
    let m = usize::from(after[0]);
    if after.len() < 1 + m * 4 {
        return None;
    }
    let mut skill_overrides = Vec::with_capacity(m);
    for i in 0..m {
        let at = 1 + i * 4;
        skill_overrides.push(u32::from_le_bytes([
            after[at],
            after[at + 1],
            after[at + 2],
            after[at + 3],
        ]));
    }
    Some((weapons, skill_overrides))
}

fn payload(code: &str) -> Option<Vec<u8>> {
    let s = code.trim();
    let inner = s
        .strip_prefix("[&")
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(s);
    base64::engine::general_purpose::STANDARD
        .decode(inner.trim())
        .ok()
}

/// Decode a chat link, or `None` if it is not a build template.
///
/// Accepts `[&BASE64]` or bare Base64. Item, skill and trait links share the
/// `[&...]` syntax; accepting one produces a confidently wrong build.
pub fn decode(code: &str) -> Option<BuildTemplate> {
    from_bytes(&payload(code)?)
}

/// Encode a template as a pasteable `[&...]` chat link.
pub fn encode(t: &BuildTemplate) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(to_bytes(t));
    format!("[&{encoded}]")
}

/// The first real build template in `text`, ignoring other chat links.
///
/// A build page carries item and skill links too, and the build's own code is
/// rarely the first one on the page.
pub fn find_in_text(text: &str) -> Option<(String, BuildTemplate)> {
    let mut pos = 0;
    while let Some(rel) = text[pos..].find("[&") {
        let start = pos + rel;
        let Some(end_rel) = text[start..].find(']') else {
            break;
        };
        let end = start + end_rel + 1;
        let candidate = &text[start..end];
        if let Some(template) = decode(candidate) {
            return Some((candidate.to_string(), template));
        }
        pos = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real Necromancer template, and the item link that was being taken
    /// instead of it. Both were pulled from the live benchmark store on
    /// 2026-09-06, where the item link had been recorded as the build code of
    /// 407 of 739 builds.
    const NECRO: &str = "[&DQgnNjI1PCp+FgAAgAAAAHUBAABvAQAAkgAAAAAAAAAAAAAAAAAAAAAAAAA=]";
    const ITEM_LINK: &str = "[&BPcAAAA=]";
    /// Verbatim from guildjen.com/support-troubadour-cloud-build/ on
    /// 2026-09-06, where the page prints it under a "Chat Code:" heading.
    const TROUBADOUR: &str =
        "[&DQcXFi02STqJHQ8BhQFmAYQdfwFrHWQBbR2aAQAAAAAAAAAAAAAAAAAAAAADVQBaADEAAA==]";
    /// Forum `struct.unpack` example: Mesmer, land palette 3875 / water 366.
    const PYTHON_MESMER: &str = "[&DQcAAAAAAAAjD24BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=]";
    const GUILDJEN_HERALD: &str = "[&DQkDOQ85NCvcEdwRKxIrEtQR1BEGEgYSyhHKEQIBAgEGEtQRKxIGEtQRKxI=]";
    const METABATTLE_HERALD: &str =
        "[&DQkOFQkbNC/cEdwRBhIGEisSKxLUEdQRyhHKEQECAgEGEisS1BEGEisS1BEDCQE1AFcAAA==]";
    const USER_HERALD: &str =
        "[&DQkDNQ8aNDvcEQAABhIAACsSAADUEQAAyhEAAAMBAwEGEisS1BEGEisS1BEDVgBaADUAAA==]";

    #[test]
    fn decodes_profession_specs_and_traits() {
        let t = decode(NECRO).expect("a build template");
        assert_eq!(t.profession, 8, "8 is Necromancer");
        assert_eq!(
            t.specs.map(|s| s.id),
            [39, 50, 60],
            "Death Magic, Blood Magic, Reaper"
        );
        // 0x36 = 0b00110110: adept 2, master 1, grandmaster 3.
        assert_eq!(t.specs[0].choices, [2, 1, 3]);
        assert!(t.skills.iter().any(|&s| s != 0), "a skill bar was decoded");
    }

    /// A second real page, a different profession, and a code long enough to
    /// carry the profession-specific tail. Whatever else a build page does
    /// or does not print, this one field carries the build.
    #[test]
    fn decodes_a_live_guildjen_page_code() {
        let t = decode(TROUBADOUR).expect("a build template");
        assert_eq!(t.profession, 7, "7 is Mesmer - the page is a Troubadour");
        assert!(
            t.specs.iter().all(|s| s.id != 0),
            "three specializations: {:?}",
            t.specs.map(|s| s.id)
        );
        assert!(
            t.specs.iter().all(|s| s.choices.iter().all(|&c| c <= 3)),
            "every trait choice is none or one of three: {:?}",
            t.specs.map(|s| s.choices)
        );
        assert!(
            t.skills.iter().filter(|&&s| s != 0).count() >= 4,
            "a real bar, not an empty one: {:?}",
            t.skills
        );
        assert_eq!(t.weapons, vec![85, 90, 49], "rifle, sword, focus");
    }

    #[test]
    fn python_struct_unpack_is_palettes_not_skill_ids() {
        // Same math as:
        //   struct.unpack('BBBBBBBB', s[0:8])
        //   struct.unpack('<HHHHHHHHHH', s[8:28])
        let t = decode(PYTHON_MESMER).expect("forum example");
        assert_eq!(t.profession, 7);
        assert_eq!(t.specs, [TemplateSpec::default(); 3]);
        assert_eq!(t.skills[0], 3875, "land heal is a palette id");
        assert_eq!(t.aquatic[0], 366, "water heal is a palette id");
        assert_ne!(
            t.skills[0], 21750,
            "21750 is the /v2/skills id for Signet of the Ether"
        );
        assert!(
            t.skills[1..].iter().all(|&p| p == 0) && t.aquatic[1..].iter().all(|&p| p == 0),
            "empty utility / elite slots stay 0"
        );
        assert_eq!(t.profession_bytes, [0; 16]);
        assert!(t.weapons.is_empty());
    }

    #[test]
    fn encode_is_the_inverse_of_decode() {
        for code in [
            NECRO,
            TROUBADOUR,
            PYTHON_MESMER,
            GUILDJEN_HERALD,
            METABATTLE_HERALD,
            USER_HERALD,
        ] {
            let t = decode(code).unwrap_or_else(|| panic!("decode {code}"));
            let bytes = payload(code).expect("base64");
            assert_eq!(to_bytes(&t), bytes, "to_bytes must match source {code}");
            assert_eq!(decode(&encode(&t)).as_ref(), Some(&t), "round-trip {code}");
        }
    }

    #[test]
    fn same_realm_duplicate_ignores_empty_slots() {
        let mut t = decode(PYTHON_MESMER).expect("forum example");
        assert!(!t.same_realm_duplicate(), "zeros are empty, not a skill");
        t.skills[1] = 3875;
        t.skills[2] = 3875;
        assert!(t.same_realm_duplicate());
    }

    #[test]
    fn rejects_every_other_kind_of_chat_link() {
        // The whole bug in one assertion: this is an item, not a build.
        assert_eq!(decode(ITEM_LINK), None);
        assert_eq!(decode("[&BkgAAAA=]"), None, "skill link");
        assert_eq!(decode("not a link"), None);
        assert_eq!(decode("[&notbase64!]"), None);
        // Right kind, truncated: a short body cannot carry a skill bar.
        assert_eq!(decode("[&DQgnNjI1PCp+]"), None);
    }

    #[test]
    fn finds_the_build_code_past_other_links() {
        let page = format!(
            "<p>Runs {ITEM_LINK} and {}</p><code>{NECRO}</code>",
            "[&BkgAAAA=]"
        );
        let (code, template) = find_in_text(&page).expect("the build, not the item");
        assert_eq!(code, NECRO);
        assert_eq!(template.profession, 8);
        assert_eq!(find_in_text("no links here at all"), None);
    }

    #[test]
    fn trait_ids_resolve_against_the_specialization() {
        let majors: Vec<u32> = (1..=9).collect();
        let spec = TemplateSpec {
            id: 39,
            choices: [2, 1, 3],
        };
        // Second of tier one, first of tier two, third of tier three:
        // majors[0*3+1], majors[1*3+0], majors[2*3+2].
        assert_eq!(BuildTemplate::trait_ids(&spec, &majors), vec![2, 4, 9]);

        // An empty tier contributes nothing.
        let partial = TemplateSpec {
            id: 39,
            choices: [1, 0, 2],
        };
        assert_eq!(BuildTemplate::trait_ids(&partial, &majors), vec![1, 8]);

        // A specialization we cannot resolve is skipped, not guessed at.
        assert!(BuildTemplate::trait_ids(&spec, &[1, 2, 3]).is_empty());
    }
}
