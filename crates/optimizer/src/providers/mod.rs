//! One parser per build site.
//!
//! The three sites publish the same facts three different ways, and none of
//! them in prose. Measured 2026-09-06 against the live pages, fetched with
//! the scraper's own client:
//!
//! | site      | convention                                              |
//! |-----------|---------------------------------------------------------|
//! | GuildJen  | `<span data-gw2-embed="items" data-gw2-id="…">`, a       |
//! |           | WordPress plugin; names resolved in the browser          |
//! | Snowcrows | `data-armory-embed` plus keyed `data-armory-<id>-traits` |
//! |           | / `-stat` / `-upgrades`; the divs are empty, JS fills    |
//! |           | them                                                     |
//! | Hardstuck | `<gw2object type="…" objid="…" selected_traits="…">`     |
//! |           | custom elements                                          |
//!
//! So there is no shared extractor to write, only a shared *result*. Each
//! parser knows one site's shape and returns [`ProviderBuild`]; nothing else
//! in the workspace needs to know which site a build came from.
//!
//! Two rules the sites taught us the hard way, both measured:
//!
//! 1. **Parse the raw response, never the pruned article.**
//!    [`crate::article::prune_to_article`] deletes the payload on two of the
//!    three sites: Snowcrows' Armory embeds are empty divs (`text_len` 0, so
//!    they score 0.1 against a 0.48 threshold) and 146 of them become 0;
//!    Hardstuck's chat code lives only in an `<input value>`, and `<input>`
//!    is void, so `find_in_text` on the pruned output returns `None` where
//!    the raw output returns the code. Pruning is for prose only.
//!
//! 2. **Resolve nothing here.** These parsers have no [`crate::gamedb::GameDb`]
//!    and should not carry one. They return the numbers the page published;
//!    whoever holds the database turns them into names and item types.

pub mod guildjen;
pub mod hardstuck;
pub mod snowcrows;

/// One equipment row as a site published it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GearRow {
    /// Slot as the page names it: `Helm`, `Rifle`, `Amulet`. Empty when the
    /// page labels the row with nothing, which is how GuildJen renders the
    /// relic, food and utility rows.
    pub slot: String,
    /// Stat prefix as the page names it: `Minstrel`, `Viper`.
    ///
    /// Per row, not per build. Six of eighteen GuildJen pages carry two or
    /// more distinct prefixes, and most Snowcrows builds mix them — one
    /// reaper runs Grieving armour with Viper's on three of five trinkets.
    /// A single build-wide prefix is lossy, so it is recorded where it is
    /// stated.
    pub stat: String,
    /// The equipped item.
    pub item_id: Option<u32>,
    /// Whatever the page attached to this row: rune, sigils, infusions.
    pub upgrade_ids: Vec<u32>,
}

impl GearRow {
    /// Whether this row is one of the six armour pieces, which is what makes
    /// its upgrade the build's rune rather than a sigil.
    ///
    /// Eight names for six pieces: the API calls them Coat and Leggings,
    /// Hardstuck displays Chest and Legs, and a slot the reader recognises
    /// but this list does not would send the rune into the sigils.
    pub fn is_armour(&self) -> bool {
        const ARMOUR: [&str; 8] = [
            "helm",
            "shoulders",
            "coat",
            "chest",
            "gloves",
            "leggings",
            "legs",
            "boots",
        ];
        let slot = self.slot.to_ascii_lowercase();
        ARMOUR.iter().any(|piece| slot == *piece)
    }

    /// Whether this row is a trinket, which carries a stat prefix but never
    /// an upgrade.
    pub fn is_trinket(&self) -> bool {
        const TRINKETS: [&str; 4] = ["amulet", "ring", "accessory", "backpiece"];
        let slot = self.slot.to_ascii_lowercase();
        TRINKETS.iter().any(|piece| slot == *piece)
    }

    /// Whether this row is a weapon, which is what makes its upgrades sigils.
    ///
    /// Decided by elimination rather than by a weapon list: a labelled row
    /// that is neither armour nor trinket is a weapon, and the label is the
    /// weapon's type. That way a weapon the game adds later needs no edit
    /// here — spears became land weapons in 2025 and would have needed one.
    pub fn is_weapon(&self) -> bool {
        !self.slot.is_empty() && !self.is_armour() && !self.is_trinket()
    }
}

/// One specialization line with the three majors the build chose.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpecLine {
    /// Specialization id, matching `GameDb::specializations`.
    pub id: u32,
    /// Chosen major trait ids, adept first. Empty when the page states the
    /// specialization but not its traits.
    pub trait_ids: Vec<u32>,
}

/// A build exactly as one site published it, in GW2 API ids.
///
/// Every field is optional or empty-able on purpose: a PvP page has no
/// armour and no trinkets, a Hardstuck variant can carry no relic at all,
/// and a page that stops publishing something should produce an empty field
/// rather than a confident wrong one.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProviderBuild {
    /// The `[&…]` build template, already validated by
    /// [`crate::build_template::decode`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_code: Option<String>,
    /// Specialization lines in the order the page listed them.
    ///
    /// Not necessarily the build template's slot order — GuildJen's document
    /// order matches the template on only 20 of 27 pages. When the order
    /// matters, take it from the template.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<SpecLine>,
    /// Heal, three utilities, elite — as real skill ids, not palette ids.
    /// Empty when the page publishes only a chat code, whose skill field is
    /// palette ids needing `GameDb::palette_to_skill`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skill_ids: Vec<u32>,
    /// Equipment rows in page order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gear: Vec<GearRow>,
    /// The rune, when the page's structure states which upgrade it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rune_id: Option<u32>,
    /// Sigils, in the order their weapons appear.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sigil_ids: Vec<u32>,
    /// The relic, when the page's structure states it. Left unset where only
    /// an item *type* could tell a relic from food — that is a `GameDb`
    /// question, and these parsers do not have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relic_id: Option<u32>,
    /// PvP amulet, the stat source in that mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amulet_id: Option<u32>,
    /// The page's own prose, pruned to the article body.
    ///
    /// Not decoration. This is where every site writes its rotation —
    /// `Sword 2 > Dagger 5 > Sword 111 > 3` — and the operating instructions
    /// for the role, which say how the build produces its result rather than
    /// just naming it. Kept raw and parsed offline: the notation varies, so
    /// getting the parse right takes several passes, and a re-scrape costs
    /// hundreds of fetches.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prose: String,
}

impl ProviderBuild {
    /// Heal, three utilities and elite, as skill ids — from the markup if
    /// the site published it, otherwise from the chat code.
    ///
    /// Weapons decide skills 1-5 and are resolved from the profession, so
    /// no site needs to publish those. The slot bar is a choice, and most
    /// sites do not mark it up: 328 of 740 synced rows carry `skill_ids`.
    /// Nearly all of them carry a chat code — 739 of 740 — and the code
    /// holds the bar as PALETTE ids, which `GameDb::palette_to_skill` maps
    /// across.
    ///
    /// Positional, with `None` for an empty slot, because that is what a
    /// caller needs to label them: compacting the list first turns a build
    /// with no elite into one whose elite is its last utility.
    pub fn slot_skills(&self, db: &crate::gamedb::GameDb) -> Vec<Option<u32>> {
        // A slot-bar seat only ever holds a Heal, Utility or Elite skill. A
        // published id with any other slot — Snowcrows writes the profession
        // placeholder literally named "Locked" into all three utility seats
        // when its page does not expose them — means the markup did not
        // publish the bar, and the chat code is the better source.
        let markup_is_a_bar = self.skill_ids.iter().all(|id| {
            db.skills
                .get(id)
                .is_some_and(|s| matches!(s.slot.as_deref(), Some("Heal" | "Utility" | "Elite")))
        });
        if !self.skill_ids.is_empty() && markup_is_a_bar {
            return self.skill_ids.iter().map(|id| Some(*id)).collect();
        }
        let Some(template) = self
            .build_code
            .as_deref()
            .and_then(crate::build_template::decode)
        else {
            return Vec::new();
        };
        template
            .skills
            .iter()
            .map(|palette| {
                if *palette == 0 {
                    return None;
                }
                db.palette_to_skill.get(palette).copied()
            })
            .collect()
    }

    /// Whether the page yielded nothing at all.
    ///
    /// Every field the parsers *extract* counts — `prose` deliberately does
    /// not. This decides two things that must agree: whether the record is
    /// written to disk at all, and whether a row cached earlier today may be
    /// reused instead of refetched. A partial check — one that ignored, say,
    /// a page that published only its rune — would drop that rune on save and
    /// then refetch the page forever. Prose is the mirror image: nearly every
    /// page has some, so counting it would mark a page whose ids all failed to
    /// parse as a success and stop us ever refetching it.
    pub fn is_empty(&self) -> bool {
        self.build_code.is_none()
            && self.specs.is_empty()
            && self.skill_ids.is_empty()
            && self.gear.is_empty()
            && self.rune_id.is_none()
            && self.sigil_ids.is_empty()
            && self.relic_id.is_none()
            && self.amulet_id.is_none()
    }

    /// Every stat prefix the rows named, most-used first.
    ///
    /// The benchmark scorer wants one prefix and the page states several, so
    /// the honest reduction is "the one most of the gear uses", with the full
    /// set still on the rows for anything that needs it.
    pub fn dominant_stat(&self) -> Option<String> {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for row in &self.gear {
            if !row.stat.is_empty() {
                *counts.entry(row.stat.as_str()).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by_key(|(stat, n)| (*n, std::cmp::Reverse(*stat)))
            .map(|(stat, _)| stat.to_string())
    }
}

/// Resolve the HTML entity references that appear around chat codes.
///
/// Deliberately not a general entity table: the sites escape `[`, `&` and the
/// quote characters, and an unknown reference is left verbatim rather than
/// guessed at, so nothing can silently change meaning.
///
/// Needed because two of the three sites escape the build template and
/// neither decodes raw — Snowcrows prints `[&amp;DQgn…]` inside an `onclick`,
/// GuildJen prints `&#91;&amp;DQcX…]` inside `<pre class="wp-block-code">`,
/// and GuildJen is not even consistent about the bracket between pages.
pub(crate) fn unescape_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // A reference is `&…;` and short; anything longer is a bare ampersand.
        //
        // Scanned by character, never by byte. `&tail[..12]` panics when a
        // multi-byte character straddles the limit, and these pages are
        // full of them — an em dash in GuildJen's prose ended a sync mid-run
        // and lost every page it had already read. The same trap is
        // documented on `take_chars_window`; this is attacker-influenced
        // markup and every index into it has to be a boundary.
        const LONGEST_REFERENCE: usize = 12;
        let end = tail
            .char_indices()
            .take_while(|(at, _)| *at < LONGEST_REFERENCE)
            .find(|(_, c)| *c == ';')
            .map(|(at, _)| at);
        let Some(end) = end else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let body = &tail[1..end];
        let resolved = match body {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            hex if hex.starts_with("#x") || hex.starts_with("#X") => {
                u32::from_str_radix(&hex[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            dec if dec.starts_with('#') => dec[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match resolved {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The page's build template, looking through entity escaping.
///
/// The decoder is what rejects the noise: GuildJen ships 71 waypoint links
/// per page in a world-boss-timer JSON blob, and every one is a five-byte
/// item link with no `0x0D` header.
pub(crate) fn build_code_in(html: &str) -> Option<String> {
    if let Some((code, _)) = crate::build_template::find_in_text(html) {
        return Some(code);
    }
    crate::build_template::find_in_text(&unescape_entities(html)).map(|(code, _)| code)
}

/// `open-world` becomes `Open World`, `guardian` becomes `Guardian`.
///
/// The sites publish their taxonomy as slugs, and every one of them reaches
/// a player's screen.
pub(crate) fn title_case(slug: &str) -> String {
    slug.split(['-', '_', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The role string stored on a benchmark: where the build is played, then
/// what it does there.
///
/// One vocabulary for all three sites, because matching depends on it.
/// `find_best_benchmark` ranks candidates by word overlap between the
/// player's chosen role and the stored one, so "Roamer" against "WvW
/// Roaming" scores zero and every candidate ties — which is how the
/// reference shown became whichever row happened to sort first. Both halves
/// of the comparison now draw on the same words.
///
/// Either half may be empty: a PvP build has no scale, and a build whose
/// name states no job records only where it is played.
pub fn role_label(scale: &str, job: &str) -> String {
    match (scale.trim(), job.trim()) {
        ("", job) => job.to_string(),
        (scale, "") => scale.to_string(),
        (scale, job) => format!("{scale} {job}"),
    }
}

/// The job a build's own name states.
///
/// All three sites title a build the same way — "Power Dragonhunter", "Heal
/// Alacrity Tempest", "Condition Reaper", "Celestial Willbender" — so the
/// name is a statement of the role rather than a guess about it.
///
/// The alternative, asking whether the page text contains a word, fails on
/// every site for the same reason: a build page discusses the options it
/// rejected. Hardstuck's scan for "condi" answered yes on nearly every page,
/// which is how 142 of its 157 stored rows became "Condi DPS" and only six
/// anything else.
///
/// Boon roles are checked before damage ones: a "Heal Alacrity Tempest"
/// heals, and a "Quickness Dragonhunter" is a boon build that also does
/// damage. `None` when the name names no job, so the caller can say so
/// instead of defaulting to Power DPS.
/// The words are `RoleObjective::label()`'s on purpose. `find_best_benchmark`
/// ranks candidates by word overlap between the player's chosen role and the
/// stored one, so "Heal Support" against the `Healer` chip scores zero and
/// "Condition DPS" against `Condi DPS` scores only on the shared "DPS".
/// Sharing the vocabulary is the whole point of writing it down here.
pub fn role_in_name(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let has = |word: &str| name.contains(word);
    // Most specific first — "Heal Alacrity Druid" is a healer, not a boon
    // DPS, and "Condition Quickness Scrapper" is a boon build that also
    // does damage.
    if has("heal") {
        return Some("Healer");
    }
    if has("support") {
        return Some("Support");
    }
    if has("tank") {
        return Some("Tank");
    }
    if has("alacrity") || has("quickness") || has("boon") {
        return Some("Boon DPS");
    }
    if has("condition") || has("condi") {
        return Some("Condi DPS");
    }
    if has("celestial") || has("hybrid") {
        return Some("Hybrid");
    }
    if has("power") {
        return Some("Power DPS");
    }
    None
}

/// Split a `data-gw2-id="21152,94901"` style attribute into ids.
///
/// One embed can carry a whole skill bar, or both sigils of a PvP weapon
/// set. Anything that is not a number is dropped rather than guessed at.
pub(crate) fn ids_in(value: &str) -> Vec<u32> {
    value
        .split(',')
        .filter_map(|id| id.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slot: &str, stat: &str) -> GearRow {
        GearRow {
            slot: slot.into(),
            stat: stat.into(),
            ..Default::default()
        }
    }

    /// Slot classification decides whether an upgrade is a rune or a sigil,
    /// so it has to hold for every row shape the sites emit.
    #[test]
    fn slots_are_told_apart_by_their_label() {
        for slot in ["Helm", "Shoulders", "Coat", "Gloves", "Leggings", "Boots"] {
            let r = row(slot, "Minstrel");
            assert!(r.is_armour(), "{slot} is armour");
            assert!(!r.is_weapon() && !r.is_trinket(), "{slot}");
        }
        for slot in ["Amulet", "Ring", "Accessory", "Backpiece"] {
            let r = row(slot, "Minstrel");
            assert!(r.is_trinket(), "{slot} is a trinket");
            assert!(!r.is_weapon() && !r.is_armour(), "{slot}");
        }
        // Weapons are whatever is left, so a new weapon type needs no edit.
        for slot in ["Rifle", "Sword", "Focus", "Spear", "Greatsword"] {
            let r = row(slot, "Minstrel");
            assert!(r.is_weapon(), "{slot} is a weapon");
        }
        // An unlabelled row — GuildJen's relic and food — is none of them.
        let blank = row("", "");
        assert!(!blank.is_armour() && !blank.is_trinket() && !blank.is_weapon());
    }

    #[test]
    fn the_dominant_stat_is_what_most_of_the_gear_uses() {
        // Grieving armour with Viper's trinkets: a real mix, measured on the
        // Snowcrows reaper page.
        let build = ProviderBuild {
            gear: ["Helm", "Coat", "Boots"]
                .iter()
                .map(|s| row(s, "Grieving"))
                .chain(["Ring", "Amulet"].iter().map(|s| row(s, "Viper's")))
                .collect(),
            ..Default::default()
        };
        assert_eq!(build.dominant_stat(), Some("Grieving".into()));
        assert_eq!(ProviderBuild::default().dominant_stat(), None);
    }

    /// Snowcrows prints the build template inside a JS string inside an
    /// `onclick`, so the ampersand arrives HTML-escaped and the base64 body
    /// reads `amp;DQgn…`. The only other `[&` on the page is a JSON array in
    /// a Livewire `wire:snapshot` attribute — and that decoy is what all 180
    /// Snowcrows rows in the store recorded as their build code.
    ///
    /// Both shapes verbatim from snowcrows.com/builds/raids/necromancer/
    /// condition-reaper as the server sent it on 2026-09-06.
    #[test]
    fn an_escaped_build_code_is_found_and_the_livewire_decoy_is_not() {
        const PAGE: &str = concat!(
            r#"<div wire:snapshot="[&quot;div&quot;,&quot;9ZtEYc0ZeKP5qDXJEd3X&quot;]">"#,
            r#"<a class="tab mr-2" icon="fa-code" href="javascript://" onclick="#,
            r#"navigator.clipboard.writeText('[&amp;DQgnNzInIibBEgAAgAAAAEABAACm"#,
            r#"EgAAkgAAAAAAAAAAAAAAAAAAAAAAAAA=]');">Build Template</a></div>"#,
        );

        let code = build_code_in(PAGE).expect("the escaped template must be found");
        assert!(
            code.starts_with("[&DQ"),
            "the decoded code must be the unescaped template, got {code}"
        );
        assert!(
            !code.contains("quot;"),
            "the Livewire snapshot array is not a build code, got {code}"
        );
        let decoded = crate::build_template::decode(&code).expect("a real template");
        assert_eq!(
            decoded.profession, 8,
            "8 is Necromancer, as the page title says"
        );

        // A page carrying only the decoy yields nothing rather than the decoy.
        assert_eq!(
            build_code_in(
                r#"<div wire:snapshot="[&quot;div&quot;,&quot;9ZtEYc0ZeKP5qDXJEd3X&quot;]"></div>"#
            ),
            None
        );
    }

    /// GuildJen escapes the bracket as well as the ampersand, and not
    /// consistently between pages — so a literal replace of either form alone
    /// misses the other. Both spellings verbatim from the live site,
    /// 2026-09-06. Measured over 27 pages: 0/27 decode raw, 27/27 unescaped.
    ///
    /// The waypoint link is one of 71 that GuildJen ships on every page in a
    /// world-boss-timer JSON blob. It is the literal source of the code
    /// recorded on 407 of 739 stored builds, and the decoder — not luck —
    /// is what rejects it.
    #[test]
    fn guildjen_escapes_the_bracket_too_and_waypoints_are_not_builds() {
        const WAYPOINT: &str = r#"{"name":"Shadow Behemoth","wp":"[&BPcAAAA=]"}"#;
        const ESCAPED_BRACKET: &str = concat!(
            r#"<pre class="wp-block-code"><code>&#91;&amp;DQcXFi02STqJHQ8BhQFmAYQdfwFr"#,
            r#"HWQBbR2aAQAAAAAAAAAAAAAAAAAAAAADVQBaADEAAA==]</code></pre>"#,
        );
        const BARE_BRACKET: &str = concat!(
            r#"<pre class="wp-block-code"><code>[&amp;DQEQOS4XQSkmDyYPRwFIAdgaRwFM"#,
            r#"AbkBiRKJEgAAAAAAAAAAAAAAAAAAAAA=]</code></pre>"#,
        );

        for (label, page) in [
            ("bracket escaped", format!("{WAYPOINT}{ESCAPED_BRACKET}")),
            ("bracket bare", format!("{WAYPOINT}{BARE_BRACKET}")),
        ] {
            let code = build_code_in(&page).unwrap_or_else(|| panic!("{label}"));
            let decoded = crate::build_template::decode(&code)
                .unwrap_or_else(|| panic!("{label}: {code} must decode"));
            assert!(
                decoded.profession >= 1 && decoded.profession <= 9,
                "{label}"
            );
        }

        // Waypoints alone are not a build, however many there are.
        assert_eq!(build_code_in(&WAYPOINT.repeat(71)), None);
    }

    /// Only the references the sites actually emit are resolved; anything
    /// else is left verbatim so nothing silently changes meaning.
    #[test]
    fn unknown_entities_are_left_alone() {
        assert_eq!(unescape_entities("&#91;&amp;DQ&#x5D;"), "[&DQ]");
        assert_eq!(unescape_entities("a &nbsp; b"), "a &nbsp; b");
        assert_eq!(unescape_entities("R&D and Q&A"), "R&D and Q&A");
        assert_eq!(unescape_entities("&amp"), "&amp", "no semicolon, no change");
    }

    /// A multi-byte character straddling the reference-length limit must not
    /// panic. An em dash in GuildJen's prose landed across byte 12 of an
    /// `&`-tail and killed a sync that had already read 328 pages.
    ///
    /// Every offset here is deliberate: the dash is placed so that a naive
    /// `&tail[..12]` splits it.
    #[test]
    fn a_multibyte_character_at_the_scan_limit_does_not_panic() {
        for pad in 0..16 {
            let text = format!("&{}—more", "a".repeat(pad));
            let out = unescape_entities(&text);
            assert!(out.contains('—'), "pad {pad} lost the dash: {out}");
        }
        // The shape from the live page: an ampersand, then prose.
        assert_eq!(
            unescape_entities("Bleed &amp; burn — poison, torment."),
            "Bleed & burn — poison, torment."
        );
        // Non-Latin scripts hit the same limit differently.
        for text in [
            "&コンディション;",
            "&症状——流血;",
            "R&D — 研究",
            "&#91;&amp;DQ— trailing",
        ] {
            let _ = unescape_entities(text);
        }
    }

    #[test]
    fn a_packed_attribute_yields_every_id_and_no_guesses() {
        assert_eq!(ids_in("21152,94901"), vec![21152, 94901]);
        assert_eq!(ids_in("24839"), vec![24839]);
        assert_eq!(ids_in(" 76695 , 10237 "), vec![76695, 10237]);
        assert!(ids_in("").is_empty());
        assert_eq!(ids_in("24839,,notanid"), vec![24839]);
    }
}
