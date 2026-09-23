//! In-memory knowledge graph over runes, sigils, and relics.
//!
//! Every node is scored on the full 6-axis radar (power, condition, boon_support,
//! healing, sustain, control) from parsed API text — not a 2–3 bucket subset.
//! Edges are inverted-tag neighbors (duration:burning ↔ apply:burning), not O(n²).

use std::collections::{BTreeSet, HashMap};

use gw2_api::models::Item;
use serde_json::{json, Value};

use crate::balance::BalanceContext;
use crate::combat;
use crate::gamedb::GameDb;
use crate::scoring::{OptimizationWeights, AXIS_KEYS};
use crate::synergy::{
    extract_relic_effects, extract_rune_effects, extract_sigil_effects, score_normalized_effect,
    DamageCategory, DurationKind, NormalizedEffect, StatType,
};
use crate::text_util::strip_gw2_markup;

const GENERIC_TAGS: &[&str] = &["strike", "condition"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeKind {
    Rune,
    Sigil,
    Relic,
}

impl UpgradeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rune => "rune",
            Self::Sigil => "sigil",
            Self::Relic => "relic",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "rune" | "runes" => Some(Self::Rune),
            "sigil" | "sigils" => Some(Self::Sigil),
            "relic" | "relics" => Some(Self::Relic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpgradeNode {
    pub id: u32,
    pub name: String,
    pub kind: UpgradeKind,
    pub rely: &'static str,
    pub axes: OptimizationWeights,
    pub tags: Vec<String>,
    pub blurb: String,
}

impl UpgradeNode {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind.as_str(),
            "rely": self.rely,
            "axes": axes_json(&self.axes),
            "tags": self.tags,
            "blurb": self.blurb,
        })
    }

    fn axis(&self, index: usize) -> f64 {
        self.axes.get(index)
    }
}

#[derive(Debug, Clone)]
pub struct UpgradeGraph {
    nodes: Vec<UpgradeNode>,
    by_name: HashMap<String, usize>,
    tag_index: HashMap<String, Vec<usize>>,
}

impl UpgradeGraph {
    pub fn from_db(db: &GameDb, ctx: &BalanceContext) -> Self {
        let mut best: HashMap<String, (u8, UpgradeNode)> = HashMap::new();
        ingest(&mut best, db.all_runes(), UpgradeKind::Rune, ctx);
        ingest(&mut best, db.all_sigils(), UpgradeKind::Sigil, ctx);
        ingest(&mut best, db.all_relics(), UpgradeKind::Relic, ctx);

        let mut nodes: Vec<UpgradeNode> = best.into_values().map(|(_, n)| n).collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));

        let mut by_name = HashMap::new();
        let mut tag_index: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, node) in nodes.iter().enumerate() {
            by_name.insert(node.name.to_lowercase(), i);
            for tag in &node.tags {
                tag_index.entry(tag.clone()).or_default().push(i);
            }
        }
        Self {
            nodes,
            by_name,
            tag_index,
        }
    }

    pub fn nodes(&self) -> &[UpgradeNode] {
        &self.nodes
    }

    pub fn get(&self, name: &str) -> Option<&UpgradeNode> {
        let key = name.to_lowercase();
        if let Some(&i) = self.by_name.get(&key) {
            return Some(&self.nodes[i]);
        }
        if key.len() < 4 {
            return None;
        }
        let mut matches: Vec<(usize, &str, usize)> = self
            .by_name
            .iter()
            .filter(|(n, _)| n.contains(&key))
            .map(|(n, &i)| (n.len(), n.as_str(), i))
            .collect();
        matches.sort_unstable();
        matches.first().map(|(_, _, i)| &self.nodes[*i])
    }

    /// Ranked slice. `focus` is one of the 6 AXIS_KEYS (or aliases). Without
    /// focus, ranks by the player's full 6-axis weight vector.
    pub fn search(
        &self,
        focus: Option<&str>,
        kind: Option<UpgradeKind>,
        tag: Option<&str>,
        weights: Option<&OptimizationWeights>,
        limit: usize,
    ) -> Vec<&UpgradeNode> {
        let axis = focus.and_then(parse_focus);
        let tag_l = tag.map(|t| t.to_lowercase());
        let mut hits: Vec<&UpgradeNode> = self
            .nodes
            .iter()
            .filter(|n| kind.is_none_or(|k| n.kind == k))
            .filter(|n| {
                tag_l.as_deref().is_none_or(|t| {
                    n.tags
                        .iter()
                        .any(|x| x == t || x.ends_with(&format!(":{t}")))
                })
            })
            .filter(|n| n.rely != "unreliable")
            .filter(|n| axis.is_none_or(|i| n.axis(i) > 0.0005))
            .collect();
        hits.sort_by(|a, b| {
            rank_score(b, axis, weights)
                .partial_cmp(&rank_score(a, axis, weights))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        hits.truncate(limit.max(1));
        hits
    }

    pub fn synergies(&self, name: &str, limit: usize) -> Value {
        let Some(node) = self.get(name) else {
            return json!({ "error": format!("Unknown upgrade '{name}'") });
        };
        let mut scored: HashMap<usize, (f64, Vec<String>, &'static str)> = HashMap::new();
        for tag in &node.tags {
            if GENERIC_TAGS.contains(&tag.as_str()) {
                continue;
            }
            let Some(idxs) = self.tag_index.get(tag) else {
                continue;
            };
            for &i in idxs {
                if self.nodes[i].name == node.name {
                    continue;
                }
                let rel = relation(tag);
                let e = scored.entry(i).or_insert((0.0, Vec::new(), rel));
                e.0 += 1.0;
                if !e.1.iter().any(|t| t == tag) {
                    e.1.push(tag.clone());
                }
            }
            // duration:X ↔ apply:X (and grant:X)
            if let Some(status) = tag.strip_prefix("duration:") {
                for prefix in ["apply:", "grant:"] {
                    if let Some(idxs) = self.tag_index.get(&format!("{prefix}{status}")) {
                        for &i in idxs {
                            if self.nodes[i].name == node.name {
                                continue;
                            }
                            let e = scored.entry(i).or_insert((0.0, Vec::new(), "amplifies"));
                            e.0 += 2.0;
                            let shared = format!("{prefix}{status}");
                            if !e.1.iter().any(|t| t == &shared) {
                                e.1.push(shared);
                            }
                        }
                    }
                }
            }
            if let Some(status) = tag
                .strip_prefix("apply:")
                .or_else(|| tag.strip_prefix("grant:"))
            {
                if let Some(idxs) = self.tag_index.get(&format!("duration:{status}")) {
                    for &i in idxs {
                        if self.nodes[i].name == node.name {
                            continue;
                        }
                        let e = scored.entry(i).or_insert((0.0, Vec::new(), "amplifies"));
                        e.0 += 2.0;
                        let shared = format!("duration:{status}");
                        if !e.1.iter().any(|t| t == &shared) {
                            e.1.push(shared);
                        }
                    }
                }
            }
        }
        let mut neighbors: Vec<_> = scored.into_iter().collect();
        neighbors.sort_by(|a, b| {
            b.1 .0
                .partial_cmp(&a.1 .0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        neighbors.truncate(limit.max(1));
        json!({
            "item": node.to_json(),
            "neighbors": neighbors.iter().map(|(i, (_, tags, rel))| {
                let n = &self.nodes[*i];
                json!({
                    "name": n.name,
                    "kind": n.kind.as_str(),
                    "rely": n.rely,
                    "relation": rel,
                    "shared_tags": tags,
                    "axes": axes_json(&n.axes),
                })
            }).collect::<Vec<_>>(),
        })
    }

    /// Compact catalog covering **all 6 axes**, ranked — not an A–Z dump.
    pub fn format_catalog_slice(&self, weights: &OptimizationWeights) -> String {
        let mut out = String::from(
            "=== UPGRADE GRAPH (full 6-axis matrix) ===\n\
             Every rune/sigil/relic is scored on Power, Condition, Boon Support, Heal, Sustain, and Control.\n\
             Slice below is top items per axis (not the full catalog). Navigate with search_upgrades(focus=axis) and upgrade_synergies(name).\n\
             Do not assume a missing name does not exist.\n\
             Sigils: 2 per weapon set, no duplicates within a set; same sigil may appear in both sets.\n\n",
        );
        for (i, key) in AXIS_KEYS.iter().enumerate() {
            let w = weights.get(i);
            let per_kind = if w >= 0.25 { 4 } else { 2 };
            out.push_str(&format!("-- {} (radar {:.2}) --\n", key, w));
            for kind in [UpgradeKind::Rune, UpgradeKind::Sigil, UpgradeKind::Relic] {
                let hits = self.search(Some(key), Some(kind), None, None, per_kind);
                let nonempty: Vec<_> = hits
                    .into_iter()
                    .filter(|n| n.axis(i) > 0.0005 && n.rely != "unreliable")
                    .collect();
                if nonempty.is_empty() {
                    continue;
                }
                out.push_str(&format!("  {}s:\n", kind.as_str()));
                for n in nonempty {
                    out.push_str(&format!(
                        "    {} [{}] {}={:.3} tags={}\n",
                        n.name,
                        n.rely,
                        key,
                        n.axis(i),
                        n.tags.join(",")
                    ));
                }
            }
            out.push('\n');
        }
        out
    }
}

fn ingest(
    best: &mut HashMap<String, (u8, UpgradeNode)>,
    items: Vec<&Item>,
    kind: UpgradeKind,
    ctx: &BalanceContext,
) {
    for item in items {
        let node = classify(item, kind, ctx);
        let key = canonical_key(&node.name);
        let rank = item_rank(&node.name);
        match best.get(&key) {
            Some((r, _)) if *r >= rank => {}
            _ => {
                best.insert(key, (rank, node));
            }
        }
    }
}

fn classify(item: &Item, kind: UpgradeKind, ctx: &BalanceContext) -> UpgradeNode {
    let chunks = item_chunks(item);
    let text = chunks.join(" ");
    let effects = match kind {
        UpgradeKind::Rune => extract_rune_effects(item),
        UpgradeKind::Sigil => extract_sigil_effects(item, ctx),
        UpgradeKind::Relic => extract_relic_effects(item),
    };
    let mut axes = axis_scores(&effects);
    let tags = tags_from(&effects, &chunks);
    if tags.iter().any(|t| t == "cc") && axes.control < 0.02 {
        axes.control = 0.05;
    }
    if tags
        .iter()
        .any(|t| t == "cleanse" || t == "barrier" || t == "endurance" || t == "incoming_reduction")
        && axes.sustain < 0.02
    {
        axes.sustain = 0.04;
    }
    let rely = if text.is_empty() {
        "passive"
    } else {
        combat::upgrade_rely_label(&text)
    };
    if rely == "unreliable" {
        axes = OptimizationWeights {
            power: 0.0,
            condition: 0.0,
            boon_support: 0.0,
            healing: 0.0,
            sustain: 0.0,
            control: 0.0,
        };
    }
    let blurb: String = strip_gw2_markup(&text).chars().take(160).collect();
    UpgradeNode {
        id: item.id,
        name: item.name.clone(),
        kind,
        rely,
        axes,
        tags,
        blurb,
    }
}

fn axis_scores(effects: &[NormalizedEffect]) -> OptimizationWeights {
    let zeros = OptimizationWeights {
        power: 0.0,
        condition: 0.0,
        boon_support: 0.0,
        healing: 0.0,
        sustain: 0.0,
        control: 0.0,
    };
    let mut axes = zeros.clone();
    for i in 0..OptimizationWeights::NUM_AXES {
        let mut unit = zeros.clone();
        unit.set(i, 1.0);
        let s: f64 = effects
            .iter()
            .map(|e| score_normalized_effect(e, &unit))
            .sum();
        axes.set(i, s);
    }
    axes
}

fn tags_from(effects: &[NormalizedEffect], chunks: &[String]) -> Vec<String> {
    let mut tags = BTreeSet::new();
    for e in effects {
        match e {
            NormalizedEffect::StatBonus { stat, .. } => {
                tags.insert(format!("attr:{}", stat_key(stat)));
            }
            NormalizedEffect::DamageModifier { category, .. } => match category {
                DamageCategory::Strike => {
                    tags.insert("strike".into());
                }
                DamageCategory::Condition | DamageCategory::SpecificCondition(_) => {
                    tags.insert("condition".into());
                    if let DamageCategory::SpecificCondition(c) = category {
                        tags.insert(format!("apply:{}", c.to_lowercase()));
                    }
                }
                DamageCategory::Crit => {
                    tags.insert("crit".into());
                }
                DamageCategory::Healing => {
                    tags.insert("heal".into());
                }
            },
            NormalizedEffect::AppliesStatus {
                status,
                is_condition,
                ..
            } => {
                let s = status.to_lowercase();
                if s == "cleanse" {
                    tags.insert("cleanse".into());
                } else if *is_condition {
                    tags.insert(format!("apply:{s}"));
                    tags.insert("condition".into());
                } else {
                    tags.insert(format!("grant:{s}"));
                }
            }
            NormalizedEffect::DurationBonus { kind, .. } => match kind {
                DurationKind::AllCondition => {
                    tags.insert("duration:condition".into());
                }
                DurationKind::AllBoon => {
                    tags.insert("duration:boon".into());
                }
                DurationKind::SpecificCondition(c) => {
                    tags.insert(format!("duration:{}", c.to_lowercase()));
                }
            },
            _ => {}
        }
    }
    for chunk in chunks {
        let t = strip_gw2_markup(chunk).to_lowercase();
        if t.contains("incoming") {
            tags.insert("incoming".into());
            if t.contains('-') || t.contains("reduc") {
                tags.insert("incoming_reduction".into());
            }
            continue;
        }
        add_text_tags(&t, &mut tags);
    }
    tags.into_iter().collect()
}

fn add_text_tags(t: &str, tags: &mut BTreeSet<String>) {
    for (needle, tag) in [
        ("burning", "apply:burning"),
        ("bleeding", "apply:bleeding"),
        ("poison", "apply:poisoned"),
        ("torment", "apply:torment"),
        ("confusion", "apply:confusion"),
        ("might", "grant:might"),
        ("fury", "grant:fury"),
        ("quickness", "grant:quickness"),
        ("alacrity", "grant:alacrity"),
        ("protection", "grant:protection"),
    ] {
        if t.contains(needle)
            && (t.contains("duration")
                || t.contains("inflict")
                || t.contains("apply")
                || t.contains("grant")
                || t.contains("gain "))
        {
            if t.contains("duration") {
                let status = tag.split(':').nth(1).unwrap_or(needle);
                tags.insert(format!("duration:{status}"));
            } else {
                tags.insert(tag.into());
            }
        }
    }
    if t.contains("critical") || t.contains("on crit") {
        tags.insert("crit".into());
    }
    if t.contains("weapon swap") || t.contains("swap weapon") {
        tags.insert("swap".into());
    }
    if t.contains("evade") || t.contains("dodge") {
        tags.insert("evade".into());
    }
    if t.contains("elite") {
        tags.insert("elite".into());
    }
    if combat::upgrade_unreliable(t) {
        tags.insert("kill".into());
    }
    if combat::foe_cc_trigger(t) {
        tags.insert("cc".into());
    }
    if t.contains("barrier") {
        tags.insert("barrier".into());
    }
    if t.contains("endurance") {
        tags.insert("endurance".into());
    }
    if t.contains("heal") {
        tags.insert("heal".into());
    }
}

fn stat_key(stat: &StatType) -> &'static str {
    match stat {
        StatType::Power => "power",
        StatType::Precision => "precision",
        StatType::Toughness => "toughness",
        StatType::Vitality => "vitality",
        StatType::ConditionDamage => "condition_damage",
        StatType::Expertise => "expertise",
        StatType::Concentration => "concentration",
        StatType::Ferocity => "ferocity",
        StatType::HealingPower => "healing_power",
    }
}

fn parse_focus(s: &str) -> Option<usize> {
    match s.to_lowercase().replace([' ', '-'], "_").as_str() {
        "power" | "strike" => Some(0),
        "condition" | "condi" => Some(1),
        "boon_support" | "boon" | "boons" | "boon_spt" => Some(2),
        "healing" | "heal" => Some(3),
        "sustain" | "survivability" | "ehp" => Some(4),
        "control" | "disable" | "cc" => Some(5),
        _ => None,
    }
}

fn rank_score(n: &UpgradeNode, axis: Option<usize>, weights: Option<&OptimizationWeights>) -> f64 {
    if n.rely == "unreliable" {
        return 0.0;
    }
    if let Some(i) = axis {
        return n.axis(i);
    }
    if let Some(w) = weights {
        return (0..OptimizationWeights::NUM_AXES)
            .map(|i| n.axis(i) * w.get(i))
            .sum();
    }
    (0..OptimizationWeights::NUM_AXES)
        .map(|i| n.axis(i))
        .fold(0.0, f64::max)
}

fn relation(tag: &str) -> &'static str {
    if tag.starts_with("duration:") || tag.starts_with("apply:") || tag.starts_with("grant:") {
        "amplifies"
    } else if tag == "crit" {
        "feeds"
    } else {
        "stacks_with"
    }
}

fn axes_json(axes: &OptimizationWeights) -> Value {
    json!({
        "power": round3(axes.power),
        "condition": round3(axes.condition),
        "boon_support": round3(axes.boon_support),
        "healing": round3(axes.healing),
        "sustain": round3(axes.sustain),
        "control": round3(axes.control),
    })
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn item_chunks(item: &Item) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(d) = &item.description {
        if !d.is_empty() {
            v.push(d.clone());
        }
    }
    if let Some(b) = combat::item_buff_description(item) {
        v.push(b.to_string());
    }
    if let Some(details) = &item.details {
        v.extend(details.bonuses.iter().cloned());
    }
    v
}

fn canonical_key(name: &str) -> String {
    name.to_lowercase()
        .replace(" (pvp)", "")
        .replace(" (infused)", "")
        .replace("minor rune of the ", "rune:")
        .replace("minor rune of ", "rune:")
        .replace("major rune of the ", "rune:")
        .replace("major rune of ", "rune:")
        .replace("superior rune of the ", "rune:")
        .replace("superior rune of ", "rune:")
        .replace("minor sigil of the ", "sigil:")
        .replace("minor sigil of ", "sigil:")
        .replace("major sigil of the ", "sigil:")
        .replace("major sigil of ", "sigil:")
        .replace("superior sigil of the ", "sigil:")
        .replace("superior sigil of ", "sigil:")
        // Untiered: the PvP item table's copies, "Rune of X"/"Sigil of X".
        // Last, so the tiered prefixes above have already been rewritten.
        // `item_rank` ranks these below Superior, so the PvE item stays the
        // graph node and the PvP copy only collapses into it.
        .replace("rune of the ", "rune:")
        .replace("rune of ", "rune:")
        .replace("sigil of the ", "sigil:")
        .replace("sigil of ", "sigil:")
}

fn item_rank(name: &str) -> u8 {
    let n = name.to_lowercase();
    if n.contains("(pvp)") {
        0
    } else if n.starts_with("minor ") {
        1
    } else if n.contains("infused") {
        2
    } else if n.starts_with("major ") {
        3
    } else if n.starts_with("superior ") {
        4
    } else {
        3
    }
}

pub fn parse_kind(s: &str) -> Option<UpgradeKind> {
    UpgradeKind::parse(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_api::models::{InfixBuff, InfixUpgrade, ItemDetails};

    fn item(id: u32, name: &str, details: Option<ItemDetails>, desc: Option<&str>) -> Item {
        Item {
            id,
            name: name.into(),
            description: desc.map(str::to_string),
            icon: None,
            item_type: "UpgradeComponent".into(),
            rarity: "Exotic".into(),
            level: 80,
            vendor_value: None,
            chat_link: None,
            default_skin: None,
            flags: vec![],
            game_types: vec![],
            restrictions: vec![],
            details,
        }
    }

    fn rune_bonuses(bonuses: &[&str]) -> ItemDetails {
        ItemDetails {
            description: None,
            detail_type: Some("Rune".into()),
            weight_class: None,
            defense: None,
            damage_type: None,
            min_power: None,
            max_power: None,
            suffix: None,
            bonuses: bonuses.iter().map(|s| s.to_string()).collect(),
            infusion_upgrade_flags: vec![],
            infusion_slots: vec![],
            attribute_adjustment: None,
            infix_upgrade: None,
            suffix_item_id: None,
            secondary_suffix_item_id: None,
            stat_choices: vec![],
        }
    }

    fn sigil_buff(description: &str) -> ItemDetails {
        ItemDetails {
            description: None,
            detail_type: Some("Sigil".into()),
            weight_class: None,
            defense: None,
            damage_type: None,
            min_power: None,
            max_power: None,
            suffix: None,
            bonuses: vec![],
            infusion_upgrade_flags: vec![],
            infusion_slots: vec![],
            attribute_adjustment: None,
            infix_upgrade: Some(InfixUpgrade {
                id: None,
                attributes: vec![],
                buff: Some(InfixBuff {
                    skill_id: None,
                    description: Some(description.into()),
                }),
            }),
            suffix_item_id: None,
            secondary_suffix_item_id: None,
            stat_choices: vec![],
        }
    }

    fn push(db: &mut GameDb, mut item: Item, kind: UpgradeKind) {
        if kind == UpgradeKind::Relic {
            item.item_type = "Relic".into();
        }
        let id = item.id;
        match kind {
            UpgradeKind::Rune => db.runes.push(id),
            UpgradeKind::Sigil => db.sigils.push(id),
            UpgradeKind::Relic => db.relics.push(id),
        }
        db.items.insert(id, item);
    }

    /// Exact live API strings from items.json build 205505.
    fn live_graph() -> UpgradeGraph {
        let mut db = GameDb::empty_for_tests();
        push(
            &mut db,
            item(
                24615,
                "Superior Sigil of Force",
                Some(sigil_buff("+5% Damage")),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                44944,
                "Superior Sigil of Bursting",
                Some(sigil_buff("+5% Condition Damage")),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                24575,
                "Superior Sigil of Bloodlust",
                Some(sigil_buff(
                    "Gain a charge of +10 power each time you kill a foe, five charges if you kill an enemy player. <c=@reminder>(Max 25 stacks; ends on down.)</c>",
                )),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                24618,
                "Superior Sigil of Accuracy",
                Some(sigil_buff("+7% Critical Chance")),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                24607,
                "Superior Sigil of Energy",
                Some(sigil_buff(
                    "Gain 50% of your endurance when you swap to this weapon while in combat. <c=@reminder>(Cooldown: 9 Seconds)</c>",
                )),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                24800,
                "Superior Sigil of Smoldering",
                Some(sigil_buff("Increase Inflicted Burning Duration: 20%")),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                24836,
                "Superior Rune of the Scholar",
                Some(rune_bonuses(&[
                    "+25 Power",
                    "+35 Ferocity",
                    "+50 Power",
                    "+65 Ferocity",
                    "+100 Power",
                    "+125 Ferocity",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                24771,
                "Superior Rune of Melandru",
                Some(rune_bonuses(&[
                    "+25 Toughness",
                    "+35 Vitality",
                    "+50 Toughness",
                    "-10% Incoming Condition Duration",
                    "+100 Toughness",
                    "-10% Incoming Condition Duration",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                24842,
                "Superior Rune of the Monk",
                Some(rune_bonuses(&[
                    "+25 Healing",
                    "+5% Boon Duration",
                    "+50 Healing",
                    "+10% Boon Duration",
                    "+100 Healing",
                    "+125 Healing",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                70600,
                "Superior Rune of Leadership",
                Some(rune_bonuses(&[
                    "+8 to All Stats",
                    "+5% Boon Duration",
                    "+12 to All Stats",
                    "+10% Boon Duration",
                    "+16 to All Stats",
                    "+10% Boon Duration",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                24699,
                "Superior Rune of the Dolyak",
                Some(rune_bonuses(&[
                    "+25 Toughness",
                    "+35 Vitality",
                    "+50 Toughness",
                    "+65 Vitality",
                    "+100 Toughness",
                    "+125 Toughness",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                83338,
                "Superior Rune of the Firebrand",
                Some(rune_bonuses(&[
                    "+25 Condition Damage",
                    "+10% Quickness Duration",
                    "+50 Condition Damage",
                    "+10% Boon Duration",
                    "+100 Condition Damage",
                    "+20% Quickness Duration",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                24765,
                "Superior Rune of Balthazar",
                Some(rune_bonuses(&[
                    "+10% Burning Duration",
                    "+20% Burning Duration",
                    "+20% Burning Duration",
                ])),
                None,
            ),
            UpgradeKind::Rune,
        );
        push(
            &mut db,
            item(
                100262,
                "Relic of Fireworks",
                None,
                Some("Upon dealing strike damage using a weapon skill with a recharge time of 20 seconds or more, deal increased strike damage for a duration. Refreshes duration on stack."),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100916,
                "Relic of the Thief",
                None,
                Some("Upon striking an enemy with a weapon skill that has a recharge or resource cost, gain increased strike damage. <c=@reminder>When this triggers, refresh the duration of all stacks.</c>"),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100031,
                "Relic of the Monk",
                None,
                Some("Increase healing effectiveness to allies after granting a boon to an ally."),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100579,
                "Relic of the Nightmare",
                None,
                Some("After using an elite skill, send forth a nightmare pulse that inflicts fear and poison on nearby enemies."),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100531,
                "Relic of the Water",
                None,
                Some("Cleanse conditions from yourself and nearby allies after using a healing skill."),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100090,
                "Relic of Galdra",
                None,
                Some("When you use an elite skill, launch projectiles that inflict burning at nearby foes."),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100542,
                "Relic of the Citadel",
                None,
                Some("After using an elite skill, call down an artillery strike that inflicts stun on the skill target or nearest enemy."),
            ),
            UpgradeKind::Relic,
        );
        UpgradeGraph::from_db(&db, &BalanceContext::pve())
    }

    fn graph() -> UpgradeGraph {
        live_graph()
    }

    #[test]
    fn every_live_fixture_classifies() {
        let g = live_graph();
        let force = g.get("Superior Sigil of Force").unwrap();
        assert_eq!(force.rely, "passive");
        assert!(force.axes.power > 0.0);
        assert_eq!(force.axes.healing, 0.0);
        assert_eq!(force.axes.sustain, 0.0);

        let bursting = g.get("Bursting").unwrap();
        assert!(bursting.axes.condition > bursting.axes.power);

        let bloodlust = g.get("Bloodlust").unwrap();
        assert_eq!(bloodlust.rely, "unreliable");
        assert_eq!(bloodlust.axes.power, 0.0);

        let accuracy = g.get("Accuracy").unwrap();
        assert_eq!(accuracy.rely, "passive");
        assert!(accuracy.axes.power > 0.0);

        let energy = g.get("Energy").unwrap();
        assert!(energy.axes.sustain > 0.0, "endurance swap is sustain");
        assert!(energy.tags.iter().any(|t| t == "endurance" || t == "swap"));

        let scholar = g.get("Scholar").unwrap();
        assert!(scholar.axes.power > scholar.axes.condition);
        assert!(scholar.tags.iter().any(|t| t == "attr:ferocity"));

        let melandru = g.get("Melandru").unwrap();
        assert!(melandru.axes.sustain > 0.0);
        assert!(melandru.axes.condition < 0.0005);
        assert!(melandru.tags.iter().any(|t| t == "incoming_reduction"));

        let monk = g.get("Superior Rune of the Monk").unwrap();
        assert!(monk.axes.healing > 0.0);
        assert!(monk.axes.boon_support > 0.0);

        let lead = g.get("Leadership").unwrap();
        assert!(lead.axes.power > 0.0, "all-stats must score power");
        assert!(lead.axes.condition > 0.0);
        assert!(lead.axes.boon_support > 0.0);
        assert!(lead.axes.healing > 0.0);
        assert!(lead.axes.sustain > 0.0);
        assert!(lead.axes.control > 0.0);

        let dolyak = g.get("Dolyak").unwrap();
        assert!(dolyak.axes.sustain > dolyak.axes.power);

        let firebrand = g.get("Firebrand").unwrap();
        assert!(firebrand.axes.condition > 0.0);
        assert!(firebrand.axes.boon_support > 0.0);
        assert!(firebrand
            .tags
            .iter()
            .all(|t| t != "duration:burning" && t != "apply:burning"));

        let balthazar = g.get("Balthazar").unwrap();
        assert!(balthazar.tags.iter().any(|t| t == "duration:burning"));

        let fireworks = g.get("Fireworks").unwrap();
        assert_eq!(fireworks.rely, "long_recharge");
        assert!(fireworks.axes.power > 0.0);

        let thief = g.get("Thief").unwrap();
        assert_eq!(thief.rely, "rotation");
        assert!(thief.axes.power > fireworks.axes.power);

        let monk_relic = g.get("Relic of the Monk").unwrap();
        assert!(monk_relic.axes.healing > 0.0);

        let nightmare = g.get("Nightmare").unwrap();
        assert_eq!(nightmare.rely, "elite_cd");
        assert!(nightmare.axes.control > 0.0);

        let water = g.get("Water").unwrap();
        assert!(water.axes.sustain > 0.0);
        assert!(water.tags.iter().any(|t| t == "cleanse"));

        let galdra = g.get("Galdra").unwrap();
        assert_eq!(galdra.rely, "elite_cd");
        assert!(galdra.tags.iter().any(|t| t == "apply:burning"));

        let citadel = g.get("Citadel").unwrap();
        assert_eq!(citadel.rely, "elite_cd");
        assert!(citadel.axes.control > 0.0);
    }

    #[test]
    fn force_is_power_passive() {
        let g = graph();
        let n = g.get("Superior Sigil of Force").unwrap();
        assert_eq!(n.rely, "passive");
        assert!(n.axes.power > n.axes.condition);
        assert!(n.axes.power > 0.0);
        assert_eq!(n.axes.healing, 0.0);
        assert_eq!(n.axes.sustain, 0.0);
    }

    #[test]
    fn bursting_is_condition() {
        let g = graph();
        let n = g.get("Bursting").unwrap();
        assert!(n.axes.condition > n.axes.power);
    }

    #[test]
    fn bloodlust_unreliable_zero_combat() {
        let g = graph();
        let n = g.get("Bloodlust").unwrap();
        assert_eq!(n.rely, "unreliable");
        assert_eq!(n.axes.power, 0.0);
        let power = g.search(Some("power"), Some(UpgradeKind::Sigil), None, None, 8);
        assert!(power.iter().any(|n| n.name.contains("Force")));
        assert!(power.iter().any(|n| n.name.contains("Accuracy")));
        assert!(power.iter().all(|n| !n.name.contains("Bloodlust")));
    }

    #[test]
    fn search_condition_excludes_incoming_melandru() {
        let g = graph();
        let hits = g.search(Some("condition"), Some(UpgradeKind::Rune), None, None, 8);
        assert!(hits.iter().any(|n| n.name.contains("Firebrand")));
        assert!(hits.iter().all(|n| !n.name.contains("Melandru")));
        let sustain = g.search(Some("sustain"), Some(UpgradeKind::Rune), None, None, 8);
        assert!(sustain.iter().any(|n| n.name.contains("Melandru")));
        assert!(sustain.iter().any(|n| n.name.contains("Dolyak")));
    }

    #[test]
    fn full_matrix_has_heal_boon_and_control() {
        let g = graph();
        let heal = g.search(Some("healing"), None, None, None, 8);
        assert!(heal.iter().any(|n| n.name.contains("Monk")));
        let boon = g.search(Some("boon_support"), None, None, None, 8);
        assert!(boon
            .iter()
            .any(|n| n.name.contains("Monk") || n.name.contains("Leadership")));
        let control = g.search(Some("control"), None, None, None, 8);
        assert!(control
            .iter()
            .any(|n| n.name.contains("Citadel") || n.name.contains("Nightmare")));
        let slice = g.format_catalog_slice(&OptimizationWeights::preset_power_dps());
        for key in AXIS_KEYS {
            assert!(slice.contains(key), "slice missing axis {key}");
        }
    }

    #[test]
    fn burning_duration_neighbors_apply_burning() {
        let g = graph();
        let syn = g.synergies("Balthazar", 8);
        let names: Vec<&str> = syn["neighbors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.contains("Galdra") || n.contains("Smoldering")),
            "expected burning neighbor, got {names:?}"
        );
    }

    #[test]
    fn scholar_scores_power_not_condition() {
        let g = graph();
        let n = g.get("Scholar").unwrap();
        assert!(n.axes.power > 0.0);
        assert!(n.axes.power > n.axes.condition);
        assert!(n
            .tags
            .iter()
            .any(|t| t == "attr:power" || t == "attr:ferocity"));
    }

    #[test]
    fn get_prefers_shortest_stable_match() {
        let mut db = GameDb::empty_for_tests();
        push(
            &mut db,
            item(
                1,
                "Superior Sigil of Force",
                Some(sigil_buff("+5% Damage")),
                None,
            ),
            UpgradeKind::Sigil,
        );
        push(
            &mut db,
            item(
                2,
                "Superior Sigil of Forceful Winds",
                Some(sigil_buff("+1% Damage")),
                None,
            ),
            UpgradeKind::Sigil,
        );
        let g = UpgradeGraph::from_db(&db, &BalanceContext::pve());
        assert_eq!(g.get("force").unwrap().name, "Superior Sigil of Force");
    }

    #[test]
    fn search_is_not_first_n_alphabetical() {
        let mut db = GameDb::empty_for_tests();
        for i in 0..40u32 {
            let mut it = item(
                2000 + i,
                &format!("Relic of Aardvark {i:02}"),
                None,
                Some("Gain endurance after using a healing skill."),
            );
            it.item_type = "Relic".into();
            push(&mut db, it, UpgradeKind::Relic);
        }
        push(
            &mut db,
            item(
                100262,
                "Relic of Fireworks",
                None,
                Some("Upon dealing strike damage using a weapon skill with a recharge time of 20 seconds or more, deal increased strike damage for a duration."),
            ),
            UpgradeKind::Relic,
        );
        push(
            &mut db,
            item(
                100916,
                "Relic of the Thief",
                None,
                Some("Upon striking an enemy with a weapon skill that has a recharge or resource cost, gain increased strike damage."),
            ),
            UpgradeKind::Relic,
        );
        let g = UpgradeGraph::from_db(&db, &BalanceContext::pve());
        let hits = g.search(Some("power"), Some(UpgradeKind::Relic), None, None, 12);
        let names: Vec<&str> = hits.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("Fireworks")),
            "Fireworks missing from ranked slice: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("Thief")),
            "Thief missing from ranked slice: {names:?}"
        );
        assert!(names.iter().all(|n| !n.contains("Aardvark")));
    }
}
