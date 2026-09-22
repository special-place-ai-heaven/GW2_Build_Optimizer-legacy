//! Pre-computed context builder for Gemini synergy-driven optimization.
//! Profession traits/skills plus a 6-axis ranked upgrade graph slice
//! (not an A–Z dump of every rune/sigil/relic).

use gw2_api::models::facts::Fact;
use gw2_api::models::{Skill, Specialization, Trait as GW2Trait};

use crate::balance::BalanceContext;
use crate::data::weapon_hands::{access, choya_label, land_weapons, Hand, WeaponAccess};
use crate::gamedb::GameDb;
use crate::scoring::OptimizationWeights;
use crate::upgrade_graph::UpgradeGraph;

/// Configuration for context generation.
pub struct ContextConfig<'a> {
    pub db: &'a GameDb,
    pub profession_name: &'a str,
    pub weights: &'a OptimizationWeights,
    pub game_mode: &'a str,
    pub gear_prefixes: Vec<&'a str>,
    pub current_build_summary: Option<&'a str>,
    /// Deterministically selected gear prefix (authoritative, Gemini cannot override).
    pub determined_prefix: Option<&'a str>,
}

/// Build the complete pre-computed context document for Gemini.
/// Returns a structured text string containing all profession-relevant game data.
pub fn build_gemini_context(config: &ContextConfig) -> String {
    let mut sections = vec![
        section_profession_info(config),
        section_specializations_and_traits(config),
        section_profession_skills(config),
        section_upgrade_graph(config),
        section_gear_prefixes(config),
        crate::context_combo::section_combo_reference(config.profession_name, None),
    ];

    if let Some(summary) = config.current_build_summary {
        sections.push(section_current_build(summary));
    }

    sections.join("\n\n")
}

/// Profession info: land weapons with per-hand wiki gates (not the API dump).
fn section_profession_info(config: &ContextConfig) -> String {
    let mut out = format!("=== PROFESSION: {} ===\n", config.profession_name);

    if config.db.profession(config.profession_name).is_none() {
        return out;
    }

    out.push_str("Land weapons (wiki hands; elite gate is that spec OR Weaponmaster Training):\n");
    for weapon in land_weapons(config.profession_name) {
        let mut parts = Vec::new();
        for (hand, name) in [
            (Hand::Main, "main-hand"),
            (Hand::Off, "off-hand"),
            (Hand::TwoHand, "two-handed"),
        ] {
            let gate = access(config.profession_name, weapon, hand);
            if !matches!(gate, WeaponAccess::None) {
                parts.push(format!("{name}{}", choya_label(&gate)));
            }
        }
        if !parts.is_empty() {
            out.push_str(&format!("  {weapon}: {}\n", parts.join("; ")));
        }
    }

    out
}

/// All specializations for the profession with full trait details.
/// This is the LARGEST and most critical section for synergy reasoning.
fn section_specializations_and_traits(config: &ContextConfig) -> String {
    let mut out = String::new();

    let Some(prof) = config.db.profession(config.profession_name) else {
        return out;
    };

    // Collect and sort specs: core first, then elite
    let mut specs: Vec<&Specialization> = prof
        .specializations
        .iter()
        .filter_map(|id| config.db.spec(*id))
        .collect();
    specs.sort_by_key(|s| (s.elite, s.name.clone()));

    for spec in specs {
        let kind = if spec.elite { "Elite" } else { "Core" };
        out.push_str(&format!(
            "=== SPECIALIZATION: {} ({}) ===\n",
            spec.name, kind
        ));

        // Collect all traits for this spec
        let all_traits = config.db.spec_traits(spec.id);

        // Minor traits (always active)
        let minor_traits: Vec<&GW2Trait> = spec
            .minor_traits
            .iter()
            .filter_map(|id| config.db.traits.get(id))
            .collect();

        if !minor_traits.is_empty() {
            out.push_str("Minor Traits (always active):\n");
            for t in &minor_traits {
                format_trait_entry(&mut out, t, &config.db.traits, 1);
            }
        }

        // Major traits grouped by tier (Adept=1, Master=2, Grandmaster=3)
        let major_traits: Vec<&GW2Trait> = all_traits
            .iter()
            .filter(|t| t.slot == "Major")
            .copied()
            .collect();

        for (tier_num, tier_name) in &[(1, "Adept"), (2, "Master"), (3, "Grandmaster")] {
            let tier_traits: Vec<&&GW2Trait> = major_traits
                .iter()
                .filter(|t| t.tier == *tier_num)
                .collect();

            if !tier_traits.is_empty() {
                out.push_str(&format!("{} (pick 1):\n", tier_name));
                let mut sorted = tier_traits;
                sorted.sort_by_key(|t| t.order);
                for t in sorted {
                    format_trait_entry(&mut out, t, &config.db.traits, 2);
                }
            }
        }

        out.push('\n');
    }

    out
}

/// Format a single trait with its facts and traited_facts.
fn format_trait_entry(
    out: &mut String,
    t: &GW2Trait,
    all_traits: &std::collections::HashMap<u32, GW2Trait>,
    indent: usize,
) {
    let prefix = "  ".repeat(indent);
    let desc = t.description.as_deref().unwrap_or("");
    out.push_str(&format!("{}{}: {}\n", prefix, t.name, desc));

    // Base facts
    for fact in &t.facts {
        if let Some(text) = format_fact_text(fact) {
            out.push_str(&format!("{}  {}\n", prefix, text));
        }
    }

    // Traited facts (conditional bonuses when another trait is equipped)
    for tf in &t.traited_facts {
        let req_name = all_traits
            .get(&tf.requires_trait)
            .map(|r| r.name.as_str())
            .unwrap_or("?");
        if let Some(text) = format_fact_text(&tf.fact) {
            let override_note = tf
                .overrides
                .map(|i| format!(" (overrides base fact #{})", i))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}  TRAITED [if {} equipped]: {}{}\n",
                prefix, req_name, text, override_note
            ));
        }
    }

    // Trait skills (skills triggered by the trait)
    for ts in &t.skills {
        let ts_name = ts.name.as_deref().unwrap_or("(unnamed)");
        let ts_desc = ts.description.as_deref().unwrap_or("");
        out.push_str(&format!(
            "{}  Triggers skill: {} — {}\n",
            prefix, ts_name, ts_desc
        ));
        for fact in &ts.facts {
            if let Some(text) = format_fact_text(fact) {
                out.push_str(&format!("{}    {}\n", prefix, text));
            }
        }
    }
}

/// All profession skills grouped by slot type and weapon.
fn section_profession_skills(config: &ContextConfig) -> String {
    let mut out = format!("=== SKILLS ({}) ===\n", config.profession_name);

    let skills = config.db.profession_skills(config.profession_name);

    // Group by slot type
    let slot_groups = [
        ("Heal", vec!["Heal"]),
        ("Utility", vec!["Utility"]),
        ("Elite", vec!["Elite"]),
    ];

    for (label, slot_values) in &slot_groups {
        let mut group: Vec<&Skill> = skills
            .iter()
            .filter(|s| {
                s.slot
                    .as_deref()
                    .is_some_and(|slot| slot_values.contains(&slot))
            })
            .filter(|s| s.prev_chain.is_none()) // skip chain follow-ups
            .copied()
            .collect();
        group.sort_by(|a, b| a.name.cmp(&b.name));

        if !group.is_empty() {
            out.push_str(&format!("\n[{}]\n", label));
            for skill in &group {
                format_skill_entry(&mut out, skill);
            }
        }
    }

    // Weapon skills grouped by weapon type
    let mut weapon_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| {
            s.slot
                .as_deref()
                .is_some_and(|slot| slot.starts_with("Weapon_"))
        })
        .filter(|s| s.prev_chain.is_none())
        .copied()
        .collect();
    weapon_skills.sort_by(|a, b| {
        let wa = a.weapon_type.as_deref().unwrap_or("");
        let wb = b.weapon_type.as_deref().unwrap_or("");
        wa.cmp(wb).then_with(|| {
            a.slot
                .as_deref()
                .unwrap_or("")
                .cmp(b.slot.as_deref().unwrap_or(""))
        })
    });

    let mut current_weapon = String::new();
    for skill in &weapon_skills {
        let wt = skill.weapon_type.as_deref().unwrap_or("Unknown");
        if wt != current_weapon {
            current_weapon = wt.to_string();

            let gate = config
                .db
                .profession(config.profession_name)
                .and_then(|p| p.weapons.get(wt))
                .and_then(|w| w.specialization)
                .and_then(|id| config.db.spec(id))
                .map(|s| format!(" (requires {})", s.name))
                .unwrap_or_default();

            out.push_str(&format!("\n[Weapon: {}{}]\n", wt, gate));
        }
        format_skill_entry(&mut out, skill);
    }

    out
}

/// Format a single skill entry with its key facts.
fn format_skill_entry(out: &mut String, skill: &Skill) {
    let slot = skill.slot.as_deref().unwrap_or("");
    let cd = skill
        .facts
        .iter()
        .find_map(|f| match f {
            Fact::Recharge { value: Some(v), .. } => Some(format!(", cd:{}s", v)),
            _ => None,
        })
        .unwrap_or_default();

    let cats = if skill.categories.is_empty() {
        String::new()
    } else {
        format!(" [{}]", skill.categories.join(", "))
    };

    let attun = skill
        .attunement
        .as_ref()
        .map(|a| format!(" ({})", a))
        .unwrap_or_default();

    let init = skill
        .initiative
        .map(|i| format!(" (initiative:{})", i))
        .unwrap_or_default();

    let desc = skill.description.as_deref().unwrap_or("");
    // Truncate weapon skill descriptions to first sentence for token economy
    let desc_short = if slot.starts_with("Weapon_") {
        desc.split(". ").next().unwrap_or(desc)
    } else {
        desc
    };

    out.push_str(&format!(
        "  {} ({}{}{}{}): {}{}\n",
        skill.name, slot, cd, attun, init, desc_short, cats
    ));

    // Key facts (skip Distance, Radius, Range for token economy)
    for fact in &skill.facts {
        if let Some(text) = format_fact_text(fact) {
            out.push_str(&format!("    {}\n", text));
        }
    }

    // Note chain/flip
    if skill.next_chain.is_some() {
        out.push_str("    → chains into next skill\n");
    }
    if skill.flip_skill.is_some() {
        out.push_str("    → flips to alternate skill\n");
    }

    // Stun break flag
    let has_stunbreak = crate::rotation::builder::skill_breaks_stun(skill);
    if has_stunbreak {
        out.push_str("    ★ STUN BREAK\n");
    }
}

/// Ranked upgrade slice on all 6 radar axes. Full catalog via search_upgrades.
fn section_upgrade_graph(config: &ContextConfig) -> String {
    let ctx = match config.game_mode {
        "PvP" => BalanceContext::pvp(),
        "WvW" => BalanceContext::wvw(),
        _ => BalanceContext::pve(),
    };
    UpgradeGraph::from_db(config.db, &ctx).format_catalog_slice(config.weights)
}

/// Selected gear prefixes with their stat distributions.
fn section_gear_prefixes(config: &ContextConfig) -> String {
    let mut out = String::from("=== GEAR PREFIX ===\n");

    // Show the determined prefix prominently
    if let Some(determined) = config.determined_prefix {
        out.push_str(&format!(
            "SELECTED PREFIX: {} (determined by player's radar chart weights — this is NON-NEGOTIABLE)\n\n",
            determined
        ));
    }

    out.push_str("Available options for reference:\n");
    for prefix_name in &config.gear_prefixes {
        let marker = if config.determined_prefix == Some(prefix_name) {
            " <<<SELECTED"
        } else {
            ""
        };
        if let Some(itemstat) = config.db.itemstat_by_name(prefix_name) {
            let stats: Vec<String> = itemstat
                .attributes
                .iter()
                .map(|attr| {
                    format!(
                        "{} (mult:{:.4}, val:{})",
                        attr.attribute, attr.multiplier, attr.value
                    )
                })
                .collect();
            out.push_str(&format!(
                "{}: {}{}\n",
                itemstat.name,
                stats.join(", "),
                marker
            ));
        } else {
            out.push_str(&format!(
                "{}: (not found in game data){}\n",
                prefix_name, marker
            ));
        }
    }

    out
}

/// Current build section for the Improve flow.
fn section_current_build(summary: &str) -> String {
    // Sanitize: strip backticks and cap length (reuse logic from prompts.rs)
    let sanitized: String = summary
        .chars()
        .take(2000)
        .filter(|c| *c != '`' && *c != '<' && *c != '>')
        .collect();

    format!("=== CURRENT BUILD (Your Equipped Build) ===\n{}", sanitized)
}

/// Format a single Fact as compact human-readable text for the context document.
/// Returns None for facts that don't contribute to synergy reasoning
/// (Distance, Radius, Range, Time, NoData, Unknown).
fn format_fact_text(fact: &Fact) -> Option<String> {
    match fact {
        Fact::AttributeAdjust {
            text,
            target: Some(t),
            value: Some(v),
            ..
        } => {
            if crate::stats::is_permanent_stat_adjust(text.as_deref()) {
                Some(format!("- {}: {:+}", t, v))
            } else {
                text.as_ref().map(|label| format!("- {}: {}", label, v))
            }
        }

        Fact::Buff {
            status: Some(s),
            duration,
            apply_count,
            ..
        } => {
            let dur = duration.map(|d| format!(" {}s", d)).unwrap_or_default();
            let stacks = apply_count
                .filter(|&c| c > 1)
                .map(|c| format!(" x{}", c))
                .unwrap_or_default();
            Some(format!("- Applies {}{}{}", s, dur, stacks))
        }

        Fact::PrefixedBuff {
            status: Some(s),
            duration,
            apply_count,
            prefix,
            ..
        } => {
            let dur = duration.map(|d| format!(" {}s", d)).unwrap_or_default();
            let stacks = apply_count
                .filter(|&c| c > 1)
                .map(|c| format!(" x{}", c))
                .unwrap_or_default();
            let pfx = prefix
                .as_ref()
                .and_then(|p| p.status.as_ref())
                .map(|ps| format!(" (on {})", ps))
                .unwrap_or_default();
            Some(format!("- Applies {}{}{}{}", s, dur, stacks, pfx))
        }

        Fact::Damage {
            hit_count,
            dmg_multiplier,
            ..
        } => {
            let h = hit_count.unwrap_or(1);
            let m = dmg_multiplier.unwrap_or(1.0);
            Some(format!("- Damage: {}x (coeff {:.2})", h, m))
        }

        Fact::Heal { hit_count, .. } | Fact::HealingAdjust { hit_count, .. } => {
            let h = hit_count.unwrap_or(1);
            Some(format!("- Healing: {}x", h))
        }

        Fact::Percent {
            text: Some(t),
            percent: Some(p),
            ..
        } => Some(format!("- {}: {}%", t, p)),

        Fact::Recharge { value: Some(v), .. } => Some(format!("- Recharge: {}s", v)),

        Fact::BuffConversion {
            source: Some(s),
            target: Some(t),
            percent: Some(p),
            ..
        } => Some(format!("- Convert {}% {} → {}", p, s, t)),

        Fact::StunBreak {
            value: Some(true), ..
        } => Some("- Stun Break".to_string()),

        Fact::Unblockable {
            value: Some(true), ..
        } => Some("- Unblockable".to_string()),

        Fact::ComboField {
            field_type: Some(ft),
            ..
        } => Some(format!("- Combo Field: {}", ft)),

        Fact::ComboFinisher {
            finisher_type: Some(ft),
            percent,
            ..
        } => {
            let pct = percent.map(|p| format!(" ({}%)", p)).unwrap_or_default();
            Some(format!("- Combo Finisher: {}{}", ft, pct))
        }

        Fact::Number {
            text: Some(t),
            value: Some(v),
            ..
        } => Some(format!("- {}: {}", t, v)),

        Fact::Duration {
            text: Some(t),
            duration: Some(d),
            ..
        } => Some(format!("- {}: {}s", t, d)),

        // Skip: Distance, Radius, Range, Time, NoData, Unknown, and unmatched variants
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_fact_text_attribute_adjust() {
        let fact = Fact::AttributeAdjust {
            text: None,
            icon: None,
            value: Some(180),
            target: Some("Power".into()),
        };
        assert_eq!(format_fact_text(&fact), Some("- Power: +180".into()));
    }

    #[test]
    fn test_format_fact_text_tooltip_amount_preserves_label() {
        let fact = Fact::AttributeAdjust {
            text: Some("Life Siphon Damage".into()),
            icon: None,
            value: Some(3517),
            target: Some("Power".into()),
        };
        assert_eq!(
            format_fact_text(&fact),
            Some("- Life Siphon Damage: 3517".into())
        );
    }

    #[test]
    fn test_format_fact_text_buff() {
        let fact = Fact::Buff {
            text: None,
            icon: None,
            duration: Some(4),
            status: Some("Fury".into()),
            description: None,
            apply_count: Some(1),
        };
        assert_eq!(format_fact_text(&fact), Some("- Applies Fury 4s".into()));
    }

    #[test]
    fn test_format_fact_text_buff_stacks() {
        let fact = Fact::Buff {
            text: None,
            icon: None,
            duration: Some(10),
            status: Some("Might".into()),
            description: None,
            apply_count: Some(3),
        };
        assert_eq!(
            format_fact_text(&fact),
            Some("- Applies Might 10s x3".into())
        );
    }

    #[test]
    fn test_format_fact_text_damage() {
        let fact = Fact::Damage {
            text: None,
            icon: None,
            hit_count: Some(8),
            dmg_multiplier: Some(0.5),
        };
        assert_eq!(
            format_fact_text(&fact),
            Some("- Damage: 8x (coeff 0.50)".into())
        );
    }

    #[test]
    fn test_format_fact_text_buff_conversion() {
        let fact = Fact::BuffConversion {
            text: None,
            icon: None,
            source: Some("Toughness".into()),
            target: Some("Power".into()),
            percent: Some(7.0),
        };
        assert_eq!(
            format_fact_text(&fact),
            Some("- Convert 7% Toughness → Power".into())
        );
    }

    #[test]
    fn test_format_fact_text_stun_break() {
        let fact = Fact::StunBreak {
            text: None,
            icon: None,
            value: Some(true),
        };
        assert_eq!(format_fact_text(&fact), Some("- Stun Break".into()));
    }

    #[test]
    fn test_format_fact_text_combo_field() {
        let fact = Fact::ComboField {
            text: None,
            icon: None,
            field_type: Some("Fire".into()),
        };
        assert_eq!(format_fact_text(&fact), Some("- Combo Field: Fire".into()));
    }

    #[test]
    fn test_format_fact_text_skips_distance() {
        let fact = Fact::Distance {
            text: Some("Range".into()),
            icon: None,
            distance: Some(900),
        };
        assert_eq!(format_fact_text(&fact), None);
    }

    #[test]
    fn test_format_fact_text_skips_radius() {
        let fact = Fact::Radius {
            text: Some("Radius".into()),
            icon: None,
            distance: Some(240),
        };
        assert_eq!(format_fact_text(&fact), None);
    }

    #[test]
    fn test_format_fact_text_recharge() {
        let fact = Fact::Recharge {
            text: None,
            icon: None,
            value: Some(8.0),
        };
        assert_eq!(format_fact_text(&fact), Some("- Recharge: 8s".into()));
    }

    #[test]
    fn test_format_fact_text_percent() {
        let fact = Fact::Percent {
            text: Some("Chance on Critical Hit".into()),
            icon: None,
            percent: Some(33.0),
        };
        assert_eq!(
            format_fact_text(&fact),
            Some("- Chance on Critical Hit: 33%".into())
        );
    }

    fn stub_profession(name: &str) -> gw2_api::models::Profession {
        use std::collections::HashMap;
        // API lie: Sword looks like free dual-wield with no spec.
        let mut weapons = HashMap::new();
        weapons.insert(
            "Sword".into(),
            gw2_api::models::WeaponInfo {
                specialization: None,
                flags: vec!["Mainhand".into(), "Offhand".into()],
                skills: vec![],
            },
        );
        gw2_api::models::Profession {
            id: name.into(),
            name: name.into(),
            code: None,
            specializations: vec![],
            weapons,
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        }
    }

    fn profession_section(name: &str) -> String {
        let mut db = GameDb::empty_for_tests();
        db.professions.insert(name.into(), stub_profession(name));
        let weights = OptimizationWeights::default();
        section_profession_info(&ContextConfig {
            db: &db,
            profession_name: name,
            weights: &weights,
            game_mode: "wvw",
            gear_prefixes: vec![],
            current_build_summary: None,
            determined_prefix: None,
        })
    }

    fn weapon_line<'a>(section: &'a str, weapon: &str) -> &'a str {
        let needle = format!("{weapon}:");
        section
            .lines()
            .find(|line| line.contains(&needle))
            .unwrap_or_else(|| panic!("missing {weapon} line in:\n{section}"))
    }

    #[test]
    fn section_guardian_sword_off_needs_willbender() {
        let text = profession_section("Guardian");
        let sword = weapon_line(&text, "Sword");
        assert!(
            sword.contains("off-hand (requires Willbender"),
            "Willbender must gate off-hand: {sword}"
        );
        assert!(
            sword.contains("main-hand;"),
            "main-hand stays ungated: {sword}"
        );
        assert!(
            !sword.contains("Sword ["),
            "API flag dump must be gone: {text}"
        );
    }

    #[test]
    fn section_revenant_sword_both_hands_ungated() {
        let text = profession_section("Revenant");
        let sword = weapon_line(&text, "Sword");
        assert!(sword.contains("main-hand"), "{sword}");
        assert!(sword.contains("off-hand"), "{sword}");
        assert!(
            !sword.contains("requires"),
            "Herald dual swords is core: {sword}"
        );
    }

    #[test]
    fn section_ranger_dagger_soulbeast_main_only() {
        let text = profession_section("Ranger");
        let dagger = weapon_line(&text, "Dagger");
        let (main, off) = dagger
            .split_once("; ")
            .unwrap_or_else(|| panic!("expected two hands: {dagger}"));
        assert!(main.contains("Soulbeast"), "Soulbeast gates main: {dagger}");
        assert!(off.contains("off-hand"), "{dagger}");
        assert!(!off.contains("Soulbeast"), "off-hand is core: {dagger}");
        assert!(!off.contains("requires"), "off-hand is core: {dagger}");
    }

    #[test]
    fn section_guardian_pistol_mentions_expanded() {
        let text = profession_section("Guardian");
        let pistol = weapon_line(&text, "Pistol");
        assert!(pistol.contains("Expanded Weapon Proficiency"), "{pistol}");
    }
}
