//! Snowcrows — ids in GW2 Armory Embed attributes.
//!
//! The build ships twice in the server response: as an HTML-escaped `[&…]`
//! template, and as Armory embeds whose ids are keyed by the thing they
//! describe.
//!
//! ```html
//! <div data-armory-embed="items" data-armory-ids="85296"
//!      data-armory-85296-stat="1379" data-armory-85296-upgrades="24762"></div>
//! <p class="mb-1">Grieving<span class="…">Helm</span></p>
//! ```
//!
//! Two things make this site hostile to a naive reader.
//!
//! The embeds are **empty divs** — JS fills them in — so
//! [`crate::article::prune_to_article`] deletes every one of them: 146 to 0
//! on the reaper page, and with them every `-traits`, `-stat` and
//! `-upgrades`. Nothing here may run on pruned markup.
//!
//! And only about nineteen of the 103–146 embeds on a page belong to the
//! build. The rest are inline chips in the rotation prose — 107 of them on
//! the reaper page — telling the same story about a skill the build does not
//! run. They are told apart by shape: a build embed is a `<div>` and carries
//! no `data-armory-size`; an inline chip is a `<span>` and always carries
//! one.

use super::{build_code_in, ids_in, GearRow, ProviderBuild, SpecLine};

/// Slot labels whose row names an item rather than a stat prefix.
///
/// Most rows read `<p>Grieving<span>Helm</span></p>` — prefix then slot. The
/// relic and the consumables reuse the same shape for a *name*: the relic
/// row reads `<p>Relic of the Fractal<span>Relic</span></p>`. Recording that
/// as a stat prefix would put "Relic of the Fractal" into the build's gear
/// prefix count.
const NAMED_NOT_STATTED: [&str; 5] = ["relic", "food", "utility", "infusion", "jade bot core"];

/// The build's own name — `<h1>Condition Reaper</h1>` — which is where the
/// job is stated, in the same words the other two sites use.
pub fn build_name(html: &str) -> Option<String> {
    let document = ::html::Html::parse_document(html);
    let h1 = ::html::Selector::parse("h1").expect("valid selector");
    let name = document
        .select(&h1)
        .next()?
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!name.is_empty()).then_some(name)
}

/// Where the build is played, from the section of the URL it was listed
/// under.
///
/// Snowcrows organises by content rather than by game mode — every section
/// is PvE — so the section is the scale: a raid is ten players, a fractal
/// five, open world one.
pub fn scale_from_url(url: &str) -> &'static str {
    for (segment, scale) in [
        ("/builds/raids/", "Raid"),
        ("/builds/fractals/", "Fractal"),
        ("/builds/strikes/", "Strike"),
        ("/builds/open-world/", "Open World"),
    ] {
        if url.contains(segment) {
            return scale;
        }
    }
    ""
}

/// The published benchmark and its log: `(DPS, dps.report URL)`.
///
/// The number is the "Last Benchmark Max" stat card —
/// `<div stat="Last Benchmark Max">…<div class="text-lg"><i …></i>41879</div>` —
/// the best run, which is the one the page's "DPS Report" tab links:
/// `<a class="tab" href="https://dps.report/so5x-…_golem">`. Support pages
/// carry neither, and both come back `None`.
pub fn benchmark(html: &str) -> (Option<f64>, Option<String>) {
    let document = ::html::Html::parse_document(html);
    let card =
        ::html::Selector::parse(r#"[stat="Last Benchmark Max"] .text-lg"#).expect("valid selector");
    let log = ::html::Selector::parse(r#"a[href^="https://dps.report/"]"#).expect("valid selector");
    let dps = document.select(&card).next().and_then(|el| {
        let digits: String = el
            .text()
            .flat_map(str::chars)
            .filter(char::is_ascii_digit)
            .collect();
        digits.parse::<f64>().ok()
    });
    let url = document
        .select(&log)
        .next()
        .and_then(|a| a.value().attr("href"))
        .map(str::to_string);
    (dps, url)
}

/// Read one Snowcrows build page from the raw response.
pub fn parse(html: &str) -> ProviderBuild {
    let document = ::html::Html::parse_document(html);
    let mut build = ProviderBuild {
        build_code: build_code_in(html),
        specs: specs(&document),
        skill_ids: skill_bar(&document),
        ..Default::default()
    };
    build.gear = gear_rows(&document);
    for row in &build.gear {
        if row.slot.eq_ignore_ascii_case("relic") {
            build.relic_id = row.item_id;
        } else if row.is_armour() && build.rune_id.is_none() {
            build.rune_id = row.upgrade_ids.first().copied();
        } else if row.is_weapon() {
            build.sigil_ids.extend(row.upgrade_ids.iter().copied());
        }
    }
    build
}

/// Whether an element is part of the build rather than an inline prose chip.
///
/// The rule is the presence of `data-armory-size`, not the tag: measured
/// across five pages, every build-carrying embed is a `<div>` without it and
/// every rotation chip is a `<span>` with it.
fn is_build_embed(element: &::html::ElementRef<'_>) -> bool {
    element.value().attr("data-armory-size").is_none()
}

/// The three specialization lines with their chosen majors.
///
/// `data-armory-<specId>-traits` gives the trait ids outright, so no
/// `major_traits` table is needed. Only the first three are kept: a page can
/// carry a fourth beside prose reading "the Illusions Trait Line can be
/// taken instead", and that is a suggestion, not the build.
fn specs(document: &::html::Html) -> Vec<SpecLine> {
    let selector = ::html::Selector::parse(r#"[data-armory-embed="specializations"]"#)
        .expect("valid selector");
    document
        .select(&selector)
        .filter(is_build_embed)
        .filter_map(|line| {
            let id: u32 = line.value().attr("data-armory-ids")?.trim().parse().ok()?;
            Some(SpecLine {
                id,
                trait_ids: line
                    .value()
                    .attr(&format!("data-armory-{id}-traits"))
                    .map(ids_in)
                    .unwrap_or_default(),
            })
        })
        .take(3)
        .collect()
}

/// Heal, three utilities, elite — as real skill ids.
///
/// Better than the template's five palette ids, which would still need
/// `GameDb::palette_to_skill`.
fn skill_bar(document: &::html::Html) -> Vec<u32> {
    let selector =
        ::html::Selector::parse(r#"[data-armory-embed="skills"]"#).expect("valid selector");
    document
        .select(&selector)
        .filter(is_build_embed)
        .find_map(|bar| bar.value().attr("data-armory-ids").map(ids_in))
        .unwrap_or_default()
}

/// Every gear row, in page order.
fn gear_rows(document: &::html::Html) -> Vec<GearRow> {
    let row = ::html::Selector::parse("tr").expect("valid selector");
    let cell = ::html::Selector::parse("td").expect("valid selector");
    let item = ::html::Selector::parse(r#"[data-armory-embed="items"]"#).expect("valid selector");
    let label = ::html::Selector::parse("p").expect("valid selector");
    let slot_of = ::html::Selector::parse("span").expect("valid selector");

    let mut rows = Vec::new();
    for tr in document.select(&row) {
        let cells: Vec<_> = tr.select(&cell).collect();
        let Some(equipped) = cells
            .first()
            .and_then(|c| c.select(&item).find(|e| is_build_embed(e)))
        else {
            continue;
        };
        let ids = equipped
            .value()
            .attr("data-armory-ids")
            .map(ids_in)
            .unwrap_or_default();
        // A gear row equips one thing; a multi-id embed is something else.
        let [item_id] = ids[..] else { continue };

        let Some(caption) = cells.get(1).and_then(|c| c.select(&label).next()) else {
            continue;
        };
        // `<p>Grieving<span>Helm</span></p>` — the slot is the nested span,
        // the prefix is what precedes it.
        let slot = caption
            .select(&slot_of)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let whole = caption.text().collect::<String>();
        let prefix = whole
            .trim()
            .strip_suffix(slot.trim())
            .unwrap_or("")
            .trim()
            .to_string();
        // Consumables and the relic reuse the prefix cell for a name, and put
        // something that is not a slot in the span — the infusion row reads
        // `<p>Infusion<span>x18</span></p>`. Whichever half names the kind,
        // that is the slot, and the row states no stat.
        let named = |text: &str| {
            NAMED_NOT_STATTED
                .iter()
                .find(|named| text.eq_ignore_ascii_case(named))
                .is_some()
                .then(|| text.to_string())
        };
        let (slot, stat) = match named(&slot).or_else(|| named(&prefix)) {
            Some(kind) => (kind, String::new()),
            // A weapon row is labelled `Main Hand` or `Off Hand`, with the
            // weapon's TYPE in the prefix cell — "Grieving Spear". Stat
            // prefixes are one word, so the first word is the stat and the
            // rest names the weapon.
            //
            // The weapon becomes the slot, as it already is on the other two
            // sites. It is not decoration: a weapon determines skills 1-5, so
            // it selects five of the build's skills and every trigger they
            // carry. Keeping only "Main Hand" left 180 Snowcrows builds whose
            // published rotation could not be read at all — `Shortbow 5` is
            // unresolvable without knowing there is a shortbow.
            None => {
                let mut words = prefix.split_whitespace();
                let stat = words.next().unwrap_or("").to_string();
                let weapon = words.collect::<Vec<_>>().join(" ");
                let slot = if weapon.is_empty() { slot } else { weapon };
                (slot, stat)
            }
        };

        // `data-armory-<id>-upgrades` with no value at all makes html5ever
        // swallow the following attribute, so the value can come back as
        // `data-armory-79837-upgrade-count='{"79837":`. Non-numeric parts
        // are dropped rather than guessed at, which turns that into nothing.
        let upgrade_ids = equipped
            .value()
            .attr(&format!("data-armory-{item_id}-upgrades"))
            .map(ids_in)
            .unwrap_or_default();

        rows.push(GearRow {
            slot,
            stat,
            item_id: Some(item_id),
            upgrade_ids,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shapes verbatim from snowcrows.com's condition-reaper page as the
    /// server sent it on 2026-09-06 — including the attribute order shuffle
    /// between the first two rows, which is why nothing here reads by
    /// position.
    const PAGE: &str = r#"
      <table><tbody>
        <tr><td><div data-armory-embed="items" data-armory-ids="85296"
                     data-armory-85296-stat="1379" data-armory-85296-upgrades="24762"
                     data-armory-85296-upgrade-count='{"85296": 1}'></div></td>
            <td><p class="mb-1">Grieving<span class="text-neutral-400">Helm</span></p>
                <div data-armory-embed="items" data-armory-ids="24762"
                     data-armory-size="20" data-armory-inline-text="wiki"></div></td></tr>
        <tr><td><div data-armory-embed="items" data-armory-85241-stat="1379"
                     data-armory-ids="85241" data-armory-85241-upgrades="24762"></div></td>
            <td><p class="mb-1">Grieving<span class="text-neutral-400">Shoulders</span></p></td></tr>
        <tr><td><div data-armory-embed="items" data-armory-ids="85254"
                     data-armory-85254-upgrades="44944,24560"></div></td>
            <td><p class="mb-1">Grieving<span class="text-neutral-400">Spear</span></p></td></tr>
        <tr><td><div data-armory-embed="items" data-armory-ids="79837"
                     data-armory-79837-upgrades= data-armory-79837-upgrade-count='{"79837": 1}'></div></td>
            <td><p class="mb-1">Viper's<span class="text-neutral-400">Ring</span></p></td></tr>
        <tr><td><div data-armory-embed="items" data-armory-ids="100153"></div></td>
            <td><p class="mb-1">Relic of the Fractal<span class="text-neutral-400">Relic</span></p></td></tr>
      </tbody></table>
      <div data-armory-embed="skills" data-armory-ids="30488,10607,10544,30670,10549"></div>
      <div data-armory-embed="specializations" data-armory-ids="39" data-armory-39-traits="815,816,801"></div>
      <div data-armory-embed="specializations" data-armory-ids="50" data-armory-50-traits="888,894,893"></div>
      <div data-armory-embed="specializations" data-armory-ids="34" data-armory-34-traits="2020,1969,1919"></div>
      <p>In the rotation, cast
        <span data-armory-embed="skills" data-armory-ids="99999" data-armory-size="20"
              data-armory-inline-text="wiki"></span> here.</p>
      <div data-armory-embed="specializations" data-armory-ids="24"
           data-armory-size="20" data-armory-24-traits="1,2,3"></div>"#;

    #[test]
    fn the_rune_and_sigils_come_from_the_rows_that_carry_them() {
        let build = parse(PAGE);
        assert_eq!(build.rune_id, Some(24762), "attached to the armour rows");
        assert_eq!(
            build.sigil_ids,
            vec![44944, 24560],
            "a two-hander carries two, and only the weapon row's count"
        );
        assert_eq!(build.relic_id, Some(100153));
    }

    /// The one attribute shape that can parse to garbage: an `-upgrades`
    /// written with no value swallows the attribute after it under HTML5
    /// unquoted-value rules. Verified against this project's own parser.
    #[test]
    fn an_upgrades_attribute_with_no_value_yields_no_upgrade() {
        let build = parse(PAGE);
        let ring = build
            .gear
            .iter()
            .find(|r| r.slot == "Ring")
            .expect("the trinket row");
        assert!(
            ring.upgrade_ids.is_empty(),
            "a swallowed attribute is not an upgrade, got {:?}",
            ring.upgrade_ids
        );
    }

    /// A single build-wide prefix is wrong for most Snowcrows builds — the
    /// reaper runs Grieving armour with Viper's on three of five trinkets.
    #[test]
    fn each_row_keeps_its_own_prefix_and_the_relic_row_names_no_stat() {
        let build = parse(PAGE);
        let stat_of = |slot: &str| {
            build
                .gear
                .iter()
                .find(|r| r.slot == slot)
                .map(|r| r.stat.clone())
                .unwrap_or_default()
        };
        assert_eq!(stat_of("Helm"), "Grieving");
        assert_eq!(stat_of("Ring"), "Viper's", "a genuinely mixed set");
        // The weapon is the slot, as on the other two sites. Losing it left
        // a build whose published rotation could not be read: a numbered
        // skill means nothing without knowing what is in hand.
        assert_eq!(
            stat_of("Spear"),
            "Grieving",
            "the weapon type names the row, not 'Main Hand'"
        );
        assert!(
            build.gear.iter().any(|r| r.slot == "Spear"),
            "the weapon type survives: {:?}",
            build.gear.iter().map(|r| &r.slot).collect::<Vec<_>>()
        );
        assert_eq!(
            stat_of("Relic"),
            "",
            "the relic row names an item, not a prefix"
        );
        assert_eq!(build.dominant_stat(), Some("Grieving".into()));
    }

    /// Only about nineteen of a page's 103-146 embeds are the build; the
    /// rest are rotation chips saying the same thing about skills and specs
    /// the build does not run.
    #[test]
    fn inline_rotation_chips_are_not_the_build() {
        let build = parse(PAGE);
        assert_eq!(
            build.skill_ids,
            vec![30488, 10607, 10544, 30670, 10549],
            "the bar div, not the chip in the prose"
        );
        assert_eq!(
            build.specs.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![39, 50, 34],
            "the fourth specialization is a suggestion beside prose"
        );
        assert_eq!(build.specs[0].trait_ids, vec![815, 816, 801]);
    }
}
