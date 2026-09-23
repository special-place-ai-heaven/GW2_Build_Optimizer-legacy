//! Gemini function-calling tools — gives the LLM access to game data and calculations.
//! Each tool maps to a Gemini `functionDeclaration` and an execution handler.
//! Tools return concise JSON (<500 tokens) to stay within Gemini context limits.

use std::collections::HashMap;

use serde_json::{json, Value};

use gw2_api::models::{Fact, ItemStat, Trait as GW2Trait};

use crate::balance::BalanceContext;
use crate::combat::{self, CombatPerformance, DamageModifiers};
use crate::data::weapon_hands::{access, land_weapons, Hand};
use crate::engine::BuildCandidate;
use crate::gamedb::GameDb;
use crate::gemini::{FunctionDeclaration, Tool};
use crate::scoring::{self, OptimizationWeights};
use crate::stats;

/// Named record indexable by the deterministic lookup helper below.
/// Implemented for any indexed game entity (ItemStat, Specialization, GW2Trait) with
/// `id` + `name` fields.
trait NamedRecord {
    fn rec_id(&self) -> u32;
    fn rec_name(&self) -> &str;
}

impl NamedRecord for ItemStat {
    fn rec_id(&self) -> u32 {
        self.id
    }
    fn rec_name(&self) -> &str {
        &self.name
    }
}

impl NamedRecord for gw2_api::models::Specialization {
    fn rec_id(&self) -> u32 {
        self.id
    }
    fn rec_name(&self) -> &str {
        &self.name
    }
}

impl NamedRecord for GW2Trait {
    fn rec_id(&self) -> u32 {
        self.id
    }
    fn rec_name(&self) -> &str {
        &self.name
    }
}

/// Deterministic record lookup by name across any `HashMap<u32, T>` of `NamedRecord`.
///
/// `HashMap::values()` iteration order is unspecified, so a naive `.find(contains)`
/// can pick a different record across runs given identical input. Gemini tool execs
/// are LLM-driven and must be reproducible: the same name string must always
/// resolve to the same record.
///
/// Policy (matches `validation::validate_gear_prefix`):
///   1. Exact case-insensitive name match wins (lower id tiebreak).
///   2. Otherwise: shortest substring match wins (lower id tiebreak).
fn find_named<'a, T, I>(records: I, needle: &str) -> Option<&'a T>
where
    T: NamedRecord + 'a,
    I: IntoIterator<Item = &'a T>,
{
    if needle.is_empty() {
        return None;
    }
    let needle_lower = needle.to_lowercase();
    let mut exact: Option<&T> = None;
    let mut fuzzy: Option<(usize, u32, &T)> = None;
    for r in records {
        let lower = r.rec_name().to_lowercase();
        if lower == needle_lower {
            match exact {
                Some(prev) if prev.rec_id() <= r.rec_id() => {}
                _ => exact = Some(r),
            }
        } else if needle_lower.len() >= 5 && lower.contains(&needle_lower) {
            let key = (r.rec_name().len(), r.rec_id());
            match fuzzy {
                Some((plen, pid, _)) if (plen, pid) <= key => {}
                _ => fuzzy = Some((key.0, key.1, r)),
            }
        }
    }
    exact.or(fuzzy.map(|(_, _, r)| r))
}

fn find_itemstat_by_name<'a>(db: &'a GameDb, needle: &str) -> Option<&'a ItemStat> {
    // Delegate to GameDb's centralized deterministic helper. Kept as a thin
    // wrapper so existing call sites and tests continue to compile unchanged.
    db.itemstat_by_name(needle)
}

/// The rune, sigil and relic rankings for this radar, handed to the model
/// in the prompt the way [`profession_reference`] hands it the profession.
///
/// In-game 2026-09-07 every run opened with two or three `search_upgrades`
/// rounds and a `list_sigils`, ten to forty seconds each on a thinking
/// model, to learn what these twelve-a-kind lists say. Same shape as the
/// tool answers, so the model reads one format whether handed or fetched.
pub fn upgrade_reference(
    db: &GameDb,
    weights: &OptimizationWeights,
    balance_ctx: &BalanceContext,
) -> String {
    let ctx = ToolContext {
        db,
        profession_name: "",
        candidates: &[],
        current_build_summary: None,
        weights: weights.clone(),
        balance_ctx,
        scenario: crate::scenario::ScenarioSpec::from_balance_context(balance_ctx),
    };
    let runes = execute_tool("list_runes", &json!({}), &ctx);
    let sigils = execute_tool("list_sigils", &json!({}), &ctx);
    let relics = execute_tool("list_relics", &json!({}), &ctx);
    // Prefix spelling is evidence too: e.g. inventing a possessive suffix
    // can turn an otherwise legal plate into a rejected gear selection.
    let mut prefixes: Vec<&str> = db
        .itemstats
        .values()
        .map(|stat| stat.name.as_str())
        .filter(|name| !name.trim().is_empty())
        .collect();
    prefixes.sort_unstable();
    prefixes.dedup();
    let prefixes = json!(prefixes);
    format!(
        "\nUPGRADE REFERENCE - the runes, sigils and relics ranked for this \
         player's radar, read from the live game data. These are the answers \
         list_runes, list_sigils and list_relics would give: do NOT call them. \
         Call search_upgrades only for a focus or tag these lists do not \
         cover.\n{runes}\n{sigils}\n{relics}\n\n\
         STAT PREFIX REFERENCE - exact names from the local game database. \
         Copy stat_prefix and gear_slots values exactly from this list; do not \
         invent spelling or possessive suffixes. This is a name list, not a \
         stat calculation or a guarantee of PvP amulet availability:\n{prefixes}\n"
    )
}

/// Runtime context for tool execution — holds references to all game data
/// and optimizer state needed by the tools.
pub struct ToolContext<'a> {
    pub db: &'a GameDb,
    pub profession_name: &'a str,
    pub candidates: &'a [BuildCandidate],
    pub current_build_summary: Option<&'a str>,
    pub weights: OptimizationWeights,
    pub balance_ctx: &'a BalanceContext,
    /// The addon's current scenario (mode, tier, kind). `score_build`'s
    /// full-build mode evaluates against it, never against an argument, so
    /// a PvP amulet cannot be scored as WvW. Owned like `weights`: it is a
    /// few enums and a label, cloned once per request.
    pub scenario: crate::scenario::ScenarioSpec,
}

/// Build the Gemini tool declarations for all available tools.
pub fn tool_declarations() -> Vec<Tool> {
    vec![Tool {
        function_declarations: vec![
            decl_get_profession_info(),
            decl_get_spec_traits(),
            decl_get_trait_details(),
            decl_get_skill_info(),
            decl_list_runes(),
            decl_list_sigils(),
            decl_list_relics(),
            decl_search_upgrades(),
            decl_upgrade_synergies(),
            decl_calculate_stats(),
            decl_simulate_combat(),
            decl_score_build(),
            decl_get_current_build(),
            decl_get_optimizer_results(),
            // Synergy discovery tools
            decl_search_traits_by_effect(),
            decl_find_condition_sources(),
            decl_search_skills_by_effect(),
            decl_find_synergies(),
            decl_get_build_synergy_report(),
            // Rotation simulator
            decl_simulate_rotation(),
        ],
    }]
}

/// Everything about a profession the model would otherwise fetch a round at a
/// time, rendered once for the prompt.
///
/// Measured in-game 2026-09-06, Necromancer, one chat message on a free model:
///
/// | round | calls | took |
/// |---|---|---|
/// | 1 | get_profession_info | 9.1s |
/// | 2-4 | get_spec_traits x8 | 5.1s |
/// | 5, 7 | get_skill_info x10 | 4.7s |
/// | 8 | get_trait_details x6 | 2.2s |
///
/// 21.1 s of a 30.8 s tool phase, and six whole-conversation uploads, spent
/// enumerating data that is already in memory behind O(1) lookups. Rounds 5
/// and 7 are a guessing game: `list_runes`, `list_sigils` and `list_relics`
/// exist, but nothing lists a profession's skills, so the model has to
/// remember a name and call `get_skill_info` to find out whether it was real -
/// which is also how a wrong name reaches a player's build.
///
/// About 10.5k tokens for a Necromancer, against a 100k prompt budget.
pub fn profession_reference(db: &GameDb, profession_name: &str) -> String {
    // `get_profession_info` and `get_spec_traits` read only `db` and
    // `profession_name`; the rest of the context is required by the type and
    // unused here. Going through `execute_tool` rather than re-deriving the
    // data keeps this byte-identical to what a call would have returned, so
    // the model reads one shape whether it was handed the data or fetched it.
    let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::WvW);
    let ctx = ToolContext {
        db,
        profession_name,
        candidates: &[],
        current_build_summary: None,
        weights: OptimizationWeights::default(),
        balance_ctx: &balance_ctx,
        scenario: crate::scenario::ScenarioSpec::from_balance_context(&balance_ctx),
    };

    let info = execute_tool("get_profession_info", &json!({}), &ctx);
    if info.get("error").is_some() {
        return String::new();
    }

    let mut out = format!(
        "\nPROFESSION REFERENCE - {profession_name}, read from the live game \
         data at the moment you were asked. It is complete: every \
         specialization, every trait, every heal/utility/elite skill this \
         profession can take. Do NOT call get_profession_info, get_spec_traits \
         or get_trait_details for anything below - you already have their \
         answers, and a round spent re-reading this is a round the player \
         waits for nothing. Use tools for what is NOT here: gear, runes, \
         sigils, relics, stats, simulation.\n\n{info}\n"
    );

    for name in info["specializations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str())
    {
        let traits = execute_tool("get_spec_traits", &json!({ "spec_name": name }), &ctx);
        out.push('\n');
        out.push_str(&traits.to_string());
        out.push('\n');
    }

    // No tool lists these, so the shape is ours. Name, slot and the game's own
    // description is what picking a skill needs; the facts behind it are a
    // `get_skill_info` call away for the few the model actually shortlists.
    let mut skills: Vec<&gw2_api::models::Skill> = db
        .skills
        .values()
        .filter(|s| {
            s.professions.iter().any(|p| p == profession_name)
                && matches!(s.slot.as_deref(), Some("Heal" | "Utility" | "Elite"))
        })
        .collect();
    // `HashMap::values()` order is unspecified; without this the same request
    // produces a different prompt on different runs.
    skills.sort_by(|a, b| a.slot.cmp(&b.slot).then(a.name.cmp(&b.name)));
    if !skills.is_empty() {
        out.push_str(
            "\nSlot skills. These names are exact and this list is exhaustive - \
             a name not on it does not exist for this profession:\n",
        );
        for s in skills {
            let slot = s.slot.as_deref().unwrap_or("");
            let gate = s
                .specialization
                .and_then(|id| db.spec(id))
                .map(|spec| format!(" [{}]", spec.name))
                .unwrap_or_default();
            let desc = s.description.as_deref().unwrap_or("");
            out.push_str(&format!("{slot}: {}{gate} - {desc}\n", s.name));
        }
    }
    out
}

/// Execute a tool call by name, dispatching to the appropriate handler.
pub fn execute_tool(name: &str, args: &Value, ctx: &ToolContext) -> Value {
    match name {
        "get_profession_info" => exec_get_profession_info(args, ctx),
        "get_spec_traits" => exec_get_spec_traits(args, ctx),
        "get_trait_details" => exec_get_trait_details(args, ctx),
        "get_skill_info" => exec_get_skill_info(args, ctx),
        "list_runes" => exec_list_runes(ctx),
        "list_sigils" => exec_list_sigils(ctx),
        "list_relics" => exec_list_relics(ctx),
        "search_upgrades" => exec_search_upgrades(args, ctx),
        "upgrade_synergies" => exec_upgrade_synergies(args, ctx),
        "calculate_stats" => exec_calculate_stats(args, ctx),
        "simulate_combat" => exec_simulate_combat(args, ctx),
        "score_build" => exec_score_build(args, ctx),
        "get_current_build" => exec_get_current_build(ctx),
        "get_optimizer_results" => exec_get_optimizer_results(ctx),
        "search_traits_by_effect" => exec_search_traits_by_effect(args, ctx),
        "find_condition_sources" => exec_find_condition_sources(args, ctx),
        "search_skills_by_effect" => exec_search_skills_by_effect(args, ctx),
        "find_synergies" => exec_find_synergies(args, ctx),
        "get_build_synergy_report" => exec_get_build_synergy_report(args, ctx),
        "simulate_rotation" => exec_simulate_rotation(args, ctx),
        _ => json!({ "error": format!("Unknown tool: {}", name) }),
    }
}

// Tool Declarations

fn decl_get_profession_info() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_profession_info".into(),
        description: "Get profession details: specializations, available weapons (per hand), and resource type.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "profession": {
                    "type": "string",
                    "description": "Profession name (e.g. 'Warrior', 'Elementalist')"
                }
            },
            "required": ["profession"]
        }),
    }
}

fn decl_get_spec_traits() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_spec_traits".into(),
        description: "Get a specialization's trait layout: 3 minor traits (always active) and 9 major traits (3 columns × 3 choices, pick 1 per column).".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "spec_name": {
                    "type": "string",
                    "description": "Specialization name (e.g. 'Berserker', 'Arms', 'Firebrand')"
                }
            },
            "required": ["spec_name"]
        }),
    }
}

fn decl_get_trait_details() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_trait_details".into(),
        description: "Get detailed info about a trait: description, stat bonuses, damage modifiers, buffs, and conditions.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "trait_name": {
                    "type": "string",
                    "description": "Trait name (e.g. 'Forceful Greatsword', 'Empowered')"
                }
            },
            "required": ["trait_name"]
        }),
    }
}

fn decl_get_skill_info() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_skill_info".into(),
        description: "Get skill details: type, slot, cost, recharge, damage facts, conditions applied, weapon type.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Skill name (e.g. 'Hundred Blades', 'Fireball')"
                }
            },
            "required": ["skill_name"]
        }),
    }
}

fn decl_list_runes() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "list_runes".into(),
        description: "Top Superior runes for the current 6-axis radar (Power, Condition, Boon Support, Heal, Sustain, Control). Not A–Z. Use search_upgrades for another focus.".into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

fn decl_list_sigils() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "list_sigils".into(),
        description:
            "Top Superior sigils for the current 6-axis radar. Not A–Z. Use search_upgrades for another focus.".into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

fn decl_list_relics() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "list_relics".into(),
        description: "Top relics for the current 6-axis radar. Not A–Z. Use search_upgrades for another focus.".into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

fn decl_search_upgrades() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "search_upgrades".into(),
        description: "Search runes/sigils/relics on the full 6-axis matrix. focus is power|condition|boon_support|healing|sustain|control. Optional kind and tag (burning, crit, cc, heal, kill). Returns a short ranked list with rely + all 6 axis scores.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "focus": {
                    "type": "string",
                    "description": "Axis: power, condition, boon_support, healing, sustain, or control"
                },
                "kind": {
                    "type": "string",
                    "description": "rune, sigil, or relic"
                },
                "tag": {
                    "type": "string",
                    "description": "Optional tag such as burning, crit, swap, evade, cc, heal, kill"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 12)"
                }
            }
        }),
    }
}

fn decl_upgrade_synergies() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "upgrade_synergies".into(),
        description: "Neighbors of a rune/sigil/relic in the upgrade graph (shared tags and duration↔apply pairs) with the full 6-axis scores.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Upgrade name (partial match ok)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max neighbors (default 8)"
                }
            },
            "required": ["name"]
        }),
    }
}

fn decl_calculate_stats() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "calculate_stats".into(),
        description: "Calculate the full 9-stat block for a gear prefix on the current profession. Shows what stats you'd get.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "gear_prefix": {
                    "type": "string",
                    "description": "Stat prefix name (e.g. 'Berserker\\'s', 'Viper\\'s', 'Celestial')"
                }
            },
            "required": ["gear_prefix"]
        }),
    }
}

fn decl_simulate_combat() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "simulate_combat".into(),
        description: "Simulate combat performance for a gear prefix with optional trait IDs. Returns DPS indexes, healing, survivability under Solo/Party/Squad buff profiles.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "gear_prefix": {
                    "type": "string",
                    "description": "Stat prefix name (e.g. 'Berserker\\'s')"
                },
                "trait_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Optional list of equipped trait IDs for damage modifier extraction"
                }
            },
            "required": ["gear_prefix"]
        }),
    }
}

fn decl_score_build() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "score_build".into(),
        description: "Score a build against the player's optimization weights. Pass `build` (the whole plate, same JSON shape as your final answer) to get the app's own referee verdict for the current game mode and scenario: viable, gate results, user_intent_score, the six realized axes, data quality and what was not simulated. Pass only `gear_prefix` for the old prefix-only stat score (no traits, upgrades or rotation). Nothing is evaluated when both are missing.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "gear_prefix": {
                    "type": "string",
                    "description": "Prefix-only mode: stat prefix name (e.g. 'Berserker\\'s')"
                },
                "build": {
                    "type": "object",
                    "description": "Full-build mode: the complete plate in the answer's JSON shape",
                    "properties": {
                        "specializations": {
                            "type": "array",
                            "description": "Three objects: {\"name\": \"Spite\", \"traits\": [three trait names]}",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "traits": {"type": "array", "items": {"type": "string"}}
                                }
                            }
                        },
                        "weapons": {
                            "type": "object",
                            "description": "{\"set1\": {\"main\": \"Greatsword\", \"off\": null}, \"set2\": {\"main\": \"Axe\", \"off\": \"Focus\"}}"
                        },
                        "skills": {
                            "type": "object",
                            "description": "{\"heal\": name, \"utilities\": [three names], \"elite\": name}"
                        },
                        "rune": {"type": "string"},
                        "sigils": {
                            "type": "array",
                            "description": "Four sigil names: set1 main, set1 off, set2 main, set2 off",
                            "items": {"type": "string"}
                        },
                        "relic": {"type": "string"},
                        "stat_prefix": {"type": "string"},
                        "gear_slots": {
                            "type": "object",
                            "description": "Optional per-slot prefix override: kebab slot name (helm, ring-1, weapon-set-1-main, ...) to prefix name"
                        },
                        "pets": {"type": "array", "items": {"type": "string"}},
                        "legends": {"type": "array", "items": {"type": "string"}}
                    }
                }
            },
            "required": []
        }),
    }
}

fn decl_get_current_build() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_current_build".into(),
        description: "Get a summary of the player's currently equipped build (if available)."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

fn decl_get_optimizer_results() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_optimizer_results".into(),
        description: "Get the top candidates from the deterministic optimizer. Shows best gear/spec combos found.".into(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

// Synergy Tool Declarations

fn decl_search_traits_by_effect() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "search_traits_by_effect".into(),
        description: "Search for traits by effect type. Finds traits that deal strike damage, apply conditions, grant boons, boost stats, etc. Use to discover synergy candidates.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "effect_type": {
                    "type": "string",
                    "description": "Effect to search for: 'strike_damage', 'condition_damage', 'healing', 'boon_duration', 'condition_duration', 'crit', 'survivability'"
                },
                "profession": {
                    "type": "string",
                    "description": "Optional: filter to traits available to this profession"
                }
            },
            "required": ["effect_type"]
        }),
    }
}

fn decl_find_condition_sources() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "find_condition_sources".into(),
        description: "Find all skills and traits that apply a specific condition. Essential for building condition-focused builds.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "condition": {
                    "type": "string",
                    "description": "Condition name: 'Bleeding', 'Burning', 'Poison', 'Torment', 'Confusion', 'Vulnerability', 'Weakness'"
                },
                "profession": {
                    "type": "string",
                    "description": "Optional: filter to this profession's skills/traits"
                }
            },
            "required": ["condition"]
        }),
    }
}

fn decl_search_skills_by_effect() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "search_skills_by_effect".into(),
        description: "Search for skills that apply a condition, grant a boon, or create a combo field. Use to find skills that synergize with traits and gear.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "effect": {
                    "type": "string",
                    "description": "Condition, boon, or combo field type: 'Bleeding', 'Burning', 'Might', 'Fury', 'Fire', 'Light', etc."
                },
                "profession": {
                    "type": "string",
                    "description": "Optional: filter to this profession"
                },
                "weapon_type": {
                    "type": "string",
                    "description": "Optional: filter to this weapon (e.g. 'Greatsword', 'Scepter')"
                }
            },
            "required": ["effect"]
        }),
    }
}

fn decl_find_synergies() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "find_synergies".into(),
        description: "Analyze interactions between selected traits. Shows activated traited_facts (conditional bonuses that unlock when specific trait combinations are equipped).".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "trait_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Trait IDs to analyze for cross-references"
                }
            },
            "required": ["trait_ids"]
        }),
    }
}

fn decl_get_build_synergy_report() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "get_build_synergy_report".into(),
        description: "Generate a comprehensive synergy report for a complete build. Analyzes trait interactions, condition chains, damage modifier stacking, and duration bonuses.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "trait_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "All equipped trait IDs"
                },
                "gear_prefix": {
                    "type": "string",
                    "description": "Gear stat prefix (e.g. 'Berserker\\'s')"
                },
                "rune_name": {
                    "type": "string",
                    "description": "Optional: equipped rune name"
                }
            },
            "required": ["trait_ids"]
        }),
    }
}

fn decl_simulate_rotation() -> FunctionDeclaration {
    FunctionDeclaration {
        name: "simulate_rotation".into(),
        description: "Simulate a skill rotation to estimate real DPS, condition uptime, buff uptime, and control metrics. Validates whether a build's skills actually work together over time. Estimates a skill list on an open dummy; not a full-build verdict; use score_build with a build for that.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "skill_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Skill IDs to include in the rotation (weapon skills + heal + utilities + elite)"
                },
                "gear_prefix": {
                    "type": "string",
                    "description": "Gear stat prefix (e.g. 'Berserker\\'s') for stat calculation"
                },
                "duration_seconds": {
                    "type": "integer",
                    "description": "Optional simulation duration (default 30 seconds)"
                },
                "trait_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Optional list of equipped trait IDs for vs-target modifier extraction"
                }
            },
            "required": ["skill_ids"]
        }),
    }
}

// Tool Execution Handlers

fn exec_get_profession_info(args: &Value, ctx: &ToolContext) -> Value {
    let prof_name = args["profession"].as_str().unwrap_or(ctx.profession_name);

    let Some(prof) = ctx.db.profession(prof_name) else {
        return json!({ "error": format!("Profession '{}' not found", prof_name) });
    };

    let specs: Vec<Value> = prof
        .specializations
        .iter()
        .filter_map(|&id| {
            ctx.db.spec(id).map(|s| {
                json!({
                    "id": s.id,
                    "name": &s.name,
                    "elite": s.elite
                })
            })
        })
        .collect();

    let weapons: Vec<Value> = land_weapons(prof_name)
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "main": access(prof_name, name, Hand::Main).json_token(),
                "off": access(prof_name, name, Hand::Off).json_token(),
                "two_hand": access(prof_name, name, Hand::TwoHand).json_token()
            })
        })
        .collect();

    json!({
        "profession": &prof.name,
        "specializations": specs,
        "weapons": weapons,
        "note": "Elite names require that spec or Weaponmaster Training; expanded_soto / spear_jw are account unlocks."
    })
}

fn exec_get_spec_traits(args: &Value, ctx: &ToolContext) -> Value {
    let spec_name = args["spec_name"].as_str().unwrap_or("");

    let spec = find_named(ctx.db.specializations.values(), spec_name);

    let Some(spec) = spec else {
        return json!({ "error": format!("Specialization '{}' not found", spec_name) });
    };

    // Minor traits (always active)
    let minors: Vec<Value> = spec
        .minor_traits
        .iter()
        .filter_map(|&id| {
            ctx.db.traits.get(&id).map(|t| {
                json!({
                    "id": t.id,
                    "name": &t.name,
                    "tier": t.tier,
                    "description": t.description.as_deref().unwrap_or("")
                })
            })
        })
        .collect();

    // Major traits organized by column (tier)
    let mut columns: HashMap<u32, Vec<Value>> = HashMap::new();
    for &id in &spec.major_traits {
        if let Some(t) = ctx.db.traits.get(&id) {
            columns.entry(t.tier).or_default().push(json!({
                "id": t.id,
                "name": &t.name,
                "order": t.order,
                "description": t.description.as_deref().unwrap_or(""),
                "key_effects": summarize_trait_facts(t)
            }));
        }
    }

    // Sort choices within each column by order
    for choices in columns.values_mut() {
        choices.sort_by_key(|v| v["order"].as_u64().unwrap_or(0));
    }

    json!({
        "specialization": &spec.name,
        "elite": spec.elite,
        "profession": &spec.profession,
        "minor_traits": minors,
        "columns": {
            "adept": columns.get(&1).unwrap_or(&vec![]),
            "master": columns.get(&2).unwrap_or(&vec![]),
            "grandmaster": columns.get(&3).unwrap_or(&vec![])
        }
    })
}

fn exec_get_trait_details(args: &Value, ctx: &ToolContext) -> Value {
    let trait_name = args["trait_name"].as_str().unwrap_or("");

    let t = find_named(ctx.db.traits.values(), trait_name);

    let Some(t) = t else {
        return json!({ "error": format!("Trait '{}' not found", trait_name) });
    };

    let spec_name = ctx
        .db
        .spec(t.specialization)
        .map(|s| s.name.as_str())
        .unwrap_or("Unknown");

    let facts: Vec<Value> = t.facts.iter().map(format_fact).collect();
    let traited: Vec<Value> = t
        .traited_facts
        .iter()
        .map(|tf| {
            let mut v = format_fact(&tf.fact);
            if let Some(req) = ctx.db.traits.get(&tf.requires_trait) {
                v["requires_trait"] = json!(&req.name);
            }
            if let Some(idx) = tf.overrides {
                v["overrides_fact_index"] = json!(idx);
            }
            v
        })
        .collect();

    // Synergy summaries
    let conditions_applied = extract_conditions(&t.facts);
    let buffs_applied = extract_buffs(&t.facts);
    let stat_bonuses = extract_stat_bonuses(&t.facts);
    let damage_modifiers = extract_damage_modifiers_from_facts(&t.facts);
    let proc_triggers = detect_proc_triggers(&t.facts, t.description.as_deref());

    json!({
        "id": t.id,
        "name": &t.name,
        "specialization": spec_name,
        "tier": t.tier,
        "slot": &t.slot,
        "description": t.description.as_deref().unwrap_or(""),
        "facts": facts,
        "traited_facts": traited,
        "conditions_applied": conditions_applied,
        "buffs_applied": buffs_applied,
        "stat_bonuses": stat_bonuses,
        "damage_modifiers": damage_modifiers,
        "proc_triggers": proc_triggers
    })
}

fn exec_get_skill_info(args: &Value, ctx: &ToolContext) -> Value {
    let skill_name = args["skill_name"].as_str().unwrap_or("").trim();
    if skill_name.is_empty() {
        return json!({ "error": "Skill name is empty" });
    }

    // Find matching skills, preferring profession match.
    // Same name-matching gate as the shared `find_named` helper: an exact
    // (case-insensitive) name match always resolves; a substring ("fuzzy")
    // match requires a needle of at least 5 chars so a 2-char needle like
    // "zi" can't resolve to an arbitrary skill.
    let needle_lower = skill_name.to_lowercase();
    let allow_fuzzy = needle_lower.len() >= 5;
    let mut matches: Vec<_> = ctx
        .db
        .skills
        .values()
        .filter(|s| {
            let name_lower = s.name.to_lowercase();
            name_lower == needle_lower || (allow_fuzzy && name_lower.contains(&needle_lower))
        })
        .collect();

    // Sort: profession-specific first, then by name length (shorter = more exact),
    // then by id so ties are broken deterministically across runs.
    // `HashMap::values()` order is unspecified — without an id tiebreak the
    // same skill_name argument could resolve to different skills on different
    // machines or runs, making LLM tool results non-reproducible.
    matches.sort_by(|a, b| {
        let a_prof = a.professions.contains(&ctx.profession_name.to_string());
        let b_prof = b.professions.contains(&ctx.profession_name.to_string());
        b_prof
            .cmp(&a_prof)
            .then(a.name.len().cmp(&b.name.len()))
            .then(a.id.cmp(&b.id))
    });

    let Some(skill) = matches.first() else {
        if !allow_fuzzy {
            return json!({ "error": format!(
                "Skill name '{}' too short for fuzzy match (min 5 chars); use the exact skill name",
                skill_name
            ) });
        }
        return json!({ "error": format!("Skill '{}' not found", skill_name) });
    };

    let facts: Vec<Value> = skill.facts.iter().map(format_fact).collect();
    let conditions_applied = extract_conditions(&skill.facts);
    let buffs_applied = extract_buffs(&skill.facts);

    json!({
        "id": skill.id,
        "name": &skill.name,
        "description": skill.description.as_deref().unwrap_or(""),
        "type": skill.skill_type.as_deref().unwrap_or(""),
        "slot": skill.slot.as_deref().unwrap_or(""),
        "weapon_type": skill.weapon_type.as_deref().unwrap_or(""),
        "cost": skill.cost,
        "initiative": skill.initiative,
        "professions": &skill.professions,
        "categories": &skill.categories,
        "next_chain": skill.next_chain,
        "prev_chain": skill.prev_chain,
        "flip_skill": skill.flip_skill,
        "conditions_applied": conditions_applied,
        "buffs_applied": buffs_applied,
        "facts": facts
    })
}

fn exec_list_runes(ctx: &ToolContext) -> Value {
    exec_list_kind(ctx, crate::upgrade_graph::UpgradeKind::Rune)
}

fn exec_list_sigils(ctx: &ToolContext) -> Value {
    exec_list_kind(ctx, crate::upgrade_graph::UpgradeKind::Sigil)
}

fn exec_list_relics(ctx: &ToolContext) -> Value {
    exec_list_kind(ctx, crate::upgrade_graph::UpgradeKind::Relic)
}

fn graph(ctx: &ToolContext) -> crate::upgrade_graph::UpgradeGraph {
    crate::upgrade_graph::UpgradeGraph::from_db(ctx.db, ctx.balance_ctx)
}

fn exec_list_kind(ctx: &ToolContext, kind: crate::upgrade_graph::UpgradeKind) -> Value {
    let g = graph(ctx);
    let hits: Vec<Value> = g
        .search(None, Some(kind), None, Some(&ctx.weights), 12)
        .iter()
        .map(|n| n.to_json())
        .collect();
    match kind {
        crate::upgrade_graph::UpgradeKind::Rune => json!({ "runes": hits }),
        crate::upgrade_graph::UpgradeKind::Sigil => json!({ "sigils": hits }),
        crate::upgrade_graph::UpgradeKind::Relic => json!({ "relics": hits }),
    }
}

fn exec_search_upgrades(args: &Value, ctx: &ToolContext) -> Value {
    let g = graph(ctx);
    let focus = args["focus"].as_str();
    let kind = args["kind"]
        .as_str()
        .and_then(crate::upgrade_graph::parse_kind);
    let tag = args["tag"].as_str();
    let limit = args["limit"].as_u64().unwrap_or(12) as usize;
    let hits = g.search(focus, kind, tag, Some(&ctx.weights), limit.min(24));
    json!({
        "focus": focus,
        "kind": kind.map(|k| k.as_str()),
        "axes": ["power", "condition", "boon_support", "healing", "sustain", "control"],
        "upgrades": hits.iter().map(|n| n.to_json()).collect::<Vec<_>>()
    })
}

fn exec_upgrade_synergies(args: &Value, ctx: &ToolContext) -> Value {
    let Some(name) = args["name"].as_str() else {
        return json!({ "error": "name is required" });
    };
    let limit = args["limit"].as_u64().unwrap_or(8) as usize;
    graph(ctx).synergies(name, limit.min(16))
}

fn exec_calculate_stats(args: &Value, ctx: &ToolContext) -> Value {
    let gear_prefix = args["gear_prefix"].as_str().unwrap_or("");

    // The chat's own balance context, so a PvP question is answered with the
    // amulet the build would actually wear rather than a land kit's budgets.
    let Some((prefix_name, full_stats, derived)) =
        estimate_prefix_stats_in(ctx.db, gear_prefix, ctx.profession_name, ctx.balance_ctx)
    else {
        return json!({
            "error": format!(
                "No stat sheet for '{}' in {}: the prefix is unknown, or it has no PvP amulet.",
                gear_prefix,
                ctx.balance_ctx.game_mode.label()
            )
        });
    };

    json!({
        "prefix": prefix_name,
        "stats": {
            "power": full_stats.power.round() as i32,
            "precision": full_stats.precision.round() as i32,
            "toughness": full_stats.toughness.round() as i32,
            "vitality": full_stats.vitality.round() as i32,
            "condition_damage": full_stats.condition_damage.round() as i32,
            "expertise": full_stats.expertise.round() as i32,
            "concentration": full_stats.concentration.round() as i32,
            "ferocity": full_stats.ferocity.round() as i32,
            "healing_power": full_stats.healing_power.round() as i32
        },
        "derived": {
            "crit_chance": format!("{:.1}%", derived.crit_chance),
            "crit_damage": format!("{:.1}%", derived.crit_damage),
            "effective_power": derived.effective_power.round() as i32,
            "health": derived.health.round() as i32,
            "armor": derived.armor.round() as i32
        }
    })
}

fn exec_simulate_combat(args: &Value, ctx: &ToolContext) -> Value {
    let gear_prefix = args["gear_prefix"].as_str().unwrap_or("");

    let itemstat = find_itemstat_by_name(ctx.db, gear_prefix);

    let Some(itemstat) = itemstat else {
        return json!({ "error": format!("Stat prefix '{}' not found", gear_prefix) });
    };

    let Some(gear_stats) = calculate_full_set_stats(ctx.db, itemstat, ctx.balance_ctx) else {
        return json!({
            "error": format!(
                "No stat sheet for '{}' in {}: the prefix has no budget the game mode can price.",
                gear_prefix,
                ctx.balance_ctx.game_mode.label()
            )
        });
    };
    let mut full_stats = stats::base_stats();
    full_stats += &gear_stats;
    let derived = stats::compute_derived(&full_stats, ctx.profession_name);

    // Extract damage modifiers from traits if provided
    let trait_ids: Vec<u32> = args
        .get("trait_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();

    let modifiers = if trait_ids.is_empty() {
        DamageModifiers::default()
    } else {
        combat::extract_damage_modifiers(
            &trait_ids,
            None,
            &[],
            None,
            &ctx.db.traits,
            &ctx.db.items,
            ctx.balance_ctx,
        )
    };

    // Simulate under all 3 buff profiles using profession-specific rotation profile data
    let profiles = combat::buff_profiles_for_profession(ctx.profession_name, ctx.balance_ctx);
    let cw = combat::condition_weights_for_profession(ctx.profession_name, ctx.balance_ctx);
    let results: Vec<Value> = profiles
        .iter()
        .map(|bp| {
            let perf = combat::calculate_combat_performance(
                &full_stats,
                &derived,
                &modifiers,
                bp,
                &cw,
                ctx.profession_name,
                ctx.balance_ctx,
            );
            format_combat_performance(&perf, &bp.label)
        })
        .collect();

    json!({
        "prefix": &itemstat.name,
        "combat_profiles": results
    })
}

/// Full-build mode of `score_build`: the same path the app runs at plate
/// acceptance — `validate_gemini_build`, then the referee — so the verdict
/// Choya reads is the one the player would get. Scenario and mode come from
/// the context, never from the argument. An omitted `build` is refused by
/// the caller; nothing here ever substitutes the equipped loadout.
fn score_full_build(build: &Value, ctx: &ToolContext) -> Value {
    let plate = match crate::prompts::parse_gemini_build(&build.to_string()) {
        Ok(plate) => plate,
        Err(e) => {
            return json!({ "mode": "full_build", "errors": [format!("unreadable build: {e}")] })
        }
    };
    let validated = crate::validation::validate_gemini_build(&plate, ctx.db, ctx.profession_name);
    if !validated.errors.is_empty() {
        return json!({
            "mode": "full_build",
            "errors": validated.errors.iter().map(|e| e.detail.clone()).collect::<Vec<_>>(),
            "warnings": validated.warnings,
        });
    }
    let report = crate::referee::evaluate_validated_build_with(
        &validated,
        ctx.db,
        ctx.profession_name,
        &ctx.weights,
        ctx.balance_ctx,
        &ctx.scenario,
        &[],
    );
    let coverage_note = report
        .quality_reasons
        .iter()
        .find(|r| r.field == crate::data::quality::COVERAGE_FIELD)
        .map(|r| {
            r.explanation
                .strip_prefix(crate::data::quality::COVERAGE_PREFIX)
                .unwrap_or(&r.explanation)
                .to_string()
        });
    json!({
        "mode": "full_build",
        "scenario": format!("{} {:?}/{:?}", ctx.scenario.game_mode.label(), ctx.scenario.combat_tier, ctx.scenario.combat_kind),
        "viable": report.viability.is_viable,
        "gates": report.viability.gates.iter().map(|g| json!({
            "gate": format!("{:?}", g.gate),
            // A skipped gate judged nothing: `passed` is meaningless there.
            "outcome": if g.skipped { "skipped" } else if g.passed { "passed" } else { "failed" },
            "passed": g.passed,
            "skipped": g.skipped,
            "note": g.note,
        })).collect::<Vec<_>>(),
        "user_intent_score": report.user_intent_score,
        "realized": {
            "power": report.realized.power,
            "condition": report.realized.condition,
            "boon_support": report.realized.boon_support,
            "healing": report.realized.healing,
            "sustain": report.realized.sustain,
            "control": report.realized.control,
        },
        "quality": format!("{:?}", report.quality),
        "quality_reasons": report.quality_reasons.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
        "coverage_note": coverage_note,
        "warnings": validated.warnings,
        "errors": Vec::<String>::new(),
    })
}

fn exec_score_build(args: &Value, ctx: &ToolContext) -> Value {
    if let Some(build) = args.get("build").filter(|b| b.is_object()) {
        return score_full_build(build, ctx);
    }
    let Some(gear_prefix) = args["gear_prefix"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    else {
        return json!({
            "error": "no build supplied: pass `build` (the whole plate) for the referee verdict, or `gear_prefix` for a prefix-only stat score"
        });
    };

    let itemstat = find_itemstat_by_name(ctx.db, gear_prefix);

    let Some(itemstat) = itemstat else {
        return json!({ "error": format!("Stat prefix '{}' not found", gear_prefix) });
    };

    let Some(gear_stats) = calculate_full_set_stats(ctx.db, itemstat, ctx.balance_ctx) else {
        return json!({
            "error": format!(
                "No stat sheet for '{}' in {}: the prefix has no budget the game mode can price.",
                gear_prefix,
                ctx.balance_ctx.game_mode.label()
            )
        });
    };
    let mut full_stats = stats::base_stats();
    full_stats += &gear_stats;
    let derived = stats::compute_derived(&full_stats, ctx.profession_name);

    let mods = DamageModifiers::default();
    let solo = &combat::buff_profiles_for_profession(ctx.profession_name, ctx.balance_ctx)[0];
    let perf = combat::calculate_combat_performance(
        &full_stats,
        &derived,
        &mods,
        solo,
        &combat::condition_weights_for_profession(ctx.profession_name, ctx.balance_ctx),
        ctx.profession_name,
        ctx.balance_ctx,
    );

    let score = scoring::score_with_weights(&perf, &ctx.weights);

    json!({
        "mode": "prefix_only",
        "scope": "prefix only; traits, upgrades and rotation not evaluated",
        "prefix": &itemstat.name,
        "weights_summary": ctx.weights.summary_label(),
        "score": format!("{:.4}", score),
        "combat_summary": {
            "effective_power": perf.effective_power.round() as i32,
            "strike_dps_index": perf.strike_dps_index.round() as i32,
            "condition_dps_index": perf.condition_dps_index.round() as i32,
            "total_dps_index": perf.total_dps_index.round() as i32,
            "healing_power_index": perf.healing_power_index.round() as i32,
            "boon_duration_pct": format!("{:.1}%", perf.boon_duration_pct),
            "effective_health": perf.effective_health.round() as i32
        }
    })
}

fn exec_get_current_build(ctx: &ToolContext) -> Value {
    match ctx.current_build_summary {
        // Run the exact sanitizer `chat_refinement_prompt_with_tools` applies to
        // this same string before embedding it in the prompt. Without this, a
        // tool call mid-conversation hands the model a richer/rawer kitchen than
        // the one the prompt already committed to — two different worlds for the
        // same "current build".
        Some(summary) => json!({
            "has_build": true,
            "summary": crate::prompts::sanitize_build_summary(summary)
        }),
        None => json!({
            "has_build": false,
            "message": "No current build equipped or build could not be resolved"
        }),
    }
}

fn exec_get_optimizer_results(ctx: &ToolContext) -> Value {
    if ctx.candidates.is_empty() {
        return json!({ "candidates": [], "message": "No optimizer results available" });
    }

    let results: Vec<Value> = ctx
        .candidates
        .iter()
        .take(5)
        .map(|c| {
            let spec_names: Vec<String> = c
                .core_specs
                .iter()
                .chain(c.elite_spec.iter())
                .filter_map(|&id| ctx.db.spec(id).map(|s| s.name.clone()))
                .collect();

            let trait_names: Vec<String> = c
                .equipped_traits
                .iter()
                .filter_map(|&id| ctx.db.traits.get(&id).map(|t| t.name.clone()))
                .collect();

            json!({
                "gear_prefix": &c.gear.stat_prefix_name,
                "score": format!("{:.4}", c.score),
                "specializations": spec_names,
                "equipped_traits": trait_names,
                "stats": {
                    "power": c.stats.power.round() as i32,
                    "precision": c.stats.precision.round() as i32,
                    "toughness": c.stats.toughness.round() as i32,
                    "vitality": c.stats.vitality.round() as i32,
                    "condition_damage": c.stats.condition_damage.round() as i32,
                    "ferocity": c.stats.ferocity.round() as i32,
                    "expertise": c.stats.expertise.round() as i32,
                    "concentration": c.stats.concentration.round() as i32,
                    "healing_power": c.stats.healing_power.round() as i32
                },
                "combat": {
                    "effective_power": c.combat.effective_power.round() as i32,
                    "crit_chance": format!("{:.1}%", c.combat.crit_chance),
                    "strike_dps_index": c.combat.strike_dps_index.round() as i32,
                    "condition_dps_index": c.combat.condition_dps_index.round() as i32,
                    "total_dps_index": c.combat.total_dps_index.round() as i32,
                    "healing_power_index": c.combat.healing_power_index.round() as i32,
                    "boon_duration": format!("{:.1}%", c.combat.boon_duration_pct),
                    "condi_duration": format!("{:.1}%", c.combat.condi_duration_pct),
                    "effective_health": c.combat.effective_health.round() as i32
                }
            })
        })
        .collect();

    json!({ "candidates": results })
}

// Synergy Tool Execution Handlers

fn exec_search_traits_by_effect(args: &Value, ctx: &ToolContext) -> Value {
    let effect_type = args["effect_type"].as_str().unwrap_or("");
    let profession_filter = args.get("profession").and_then(|v| v.as_str());

    // Iterate by id so the "first 20" surfaced to the LLM are stable across
    // runs. `db.traits.values()` HashMap iteration order is unspecified.
    let mut trait_ids: Vec<u32> = ctx.db.traits.keys().copied().collect();
    trait_ids.sort_unstable();

    let mut results: Vec<Value> = Vec::new();

    for &tid in &trait_ids {
        let Some(t) = ctx.db.traits.get(&tid) else {
            continue;
        };
        // Filter by profession if specified
        if let Some(prof) = profession_filter {
            let spec = ctx.db.spec(t.specialization);
            if let Some(spec) = spec {
                if spec.profession != prof {
                    continue;
                }
            }
        }

        let matches = match effect_type {
            "strike_damage" => t.facts.iter().any(|f| matches!(f,
                Fact::Percent { text: Some(text), .. } if text.to_lowercase().contains("damage")
            )),
            "condition_damage" => t.facts.iter().any(|f| matches!(f,
                Fact::Buff { status: Some(s), .. } if ["Bleeding", "Burning", "Poisoned", "Torment", "Confusion"].contains(
                    &crate::data::boon_condition_formulas::canonical_condition_name(s)
                )
            )),
            "healing" => t.facts.iter().any(|f| matches!(f,
                Fact::AttributeAdjust { target: Some(t), .. } if t.contains("Healing")
            ) || matches!(f, Fact::HealingAdjust { .. })),
            "boon_duration" => t.facts.iter().any(|f| matches!(f,
                Fact::Percent { text: Some(text), .. } if text.to_lowercase().contains("boon duration")
            ) || matches!(f, Fact::AttributeAdjust { target: Some(t), .. } if t == "Concentration")),
            "condition_duration" => t.facts.iter().any(|f| matches!(f,
                Fact::Percent { text: Some(text), .. } if text.to_lowercase().contains("duration") && !text.to_lowercase().contains("boon")
            ) || matches!(f, Fact::AttributeAdjust { target: Some(t), .. } if t == "Expertise" || t == "ConditionDuration")),
            "crit" => {
                let desc = t.description.as_deref().unwrap_or("").to_lowercase();
                desc.contains("crit") || t.facts.iter().any(|f| matches!(f,
                    Fact::AttributeAdjust { target: Some(t), .. } if t == "Precision" || t == "CritDamage" || t == "Ferocity"
                ))
            }
            "survivability" => t.facts.iter().any(|f| matches!(f,
                Fact::AttributeAdjust { target: Some(t), .. } if t == "Toughness" || t == "Vitality"
            ) || matches!(f, Fact::Buff { status: Some(s), .. } if s == "Protection" || s == "Regeneration" || s == "Resistance")),
            _ => false,
        };

        if matches {
            let spec_name = ctx
                .db
                .spec(t.specialization)
                .map(|s| s.name.as_str())
                .unwrap_or("Unknown");
            let effects = summarize_trait_facts(t);
            results.push(json!({
                "id": t.id,
                "name": &t.name,
                "spec": spec_name,
                "tier": t.tier,
                "key_effects": effects
            }));
            if results.len() >= 20 {
                break;
            }
        }
    }

    json!({ "effect_type": effect_type, "count": results.len(), "traits": results })
}

fn exec_find_condition_sources(args: &Value, ctx: &ToolContext) -> Value {
    let condition = args["condition"].as_str().unwrap_or("");
    let profession_filter = args
        .get("profession")
        .and_then(|v| v.as_str())
        .unwrap_or(ctx.profession_name);

    // Find traits that apply this condition
    let trait_sources: Vec<Value> = ctx
        .db
        .traits_applying_condition(condition)
        .iter()
        .filter(|t| {
            ctx.db
                .spec(t.specialization)
                .map(|s| s.profession == profession_filter)
                .unwrap_or(false)
        })
        .take(15)
        .map(|t| {
            let spec_name = ctx
                .db
                .spec(t.specialization)
                .map(|s| s.name.as_str())
                .unwrap_or("Unknown");
            // Find the specific Buff fact for this condition
            let detail = t.facts.iter().find_map(|f| {
                if let Fact::Buff {
                    status: Some(s),
                    duration,
                    apply_count,
                    ..
                } = f
                {
                    if s == condition {
                        return Some(json!({
                            "stacks": apply_count.unwrap_or(1),
                            "duration_s": duration.unwrap_or(0)
                        }));
                    }
                }
                None
            });
            json!({
                "id": t.id,
                "name": &t.name,
                "spec": spec_name,
                "tier": t.tier,
                "application": detail
            })
        })
        .collect();

    // Find skills that apply this condition
    let skill_sources: Vec<Value> = ctx
        .db
        .skills_applying_condition(condition)
        .iter()
        .filter(|s| s.professions.contains(&profession_filter.to_string()))
        .take(20)
        .map(|skill| {
            let detail = skill.facts.iter().find_map(|f| {
                if let Fact::Buff {
                    status: Some(s),
                    duration,
                    apply_count,
                    ..
                } = f
                {
                    if s == condition {
                        return Some(json!({
                            "stacks": apply_count.unwrap_or(1),
                            "duration_s": duration.unwrap_or(0)
                        }));
                    }
                }
                None
            });
            json!({
                "id": skill.id,
                "name": &skill.name,
                "slot": skill.slot.as_deref().unwrap_or(""),
                "weapon_type": skill.weapon_type.as_deref().unwrap_or(""),
                "application": detail
            })
        })
        .collect();

    json!({
        "condition": condition,
        "profession": profession_filter,
        "trait_sources": trait_sources,
        "skill_sources": skill_sources
    })
}

fn exec_search_skills_by_effect(args: &Value, ctx: &ToolContext) -> Value {
    let effect = args["effect"].as_str().unwrap_or("");
    let profession_filter = args
        .get("profession")
        .and_then(|v| v.as_str())
        .unwrap_or(ctx.profession_name);
    let weapon_filter = args.get("weapon_type").and_then(|v| v.as_str());

    // Hoist the needle lowercase once outside the per-skill/per-fact loop.
    // Also walk the pre-built `skills_by_profession` index so iteration order
    // is stable across runs — the previous `db.skills.values()` scan picked
    // nondeterministic first-20 to surface to the LLM.
    let needle = effect.to_lowercase();
    let prof_skill_ids = ctx.db.skills_by_profession.get(profession_filter);

    let mut results: Vec<Value> = Vec::new();

    if let Some(ids) = prof_skill_ids {
        for &id in ids {
            let Some(skill) = ctx.db.skills.get(&id) else {
                continue;
            };
            if let Some(wt) = weapon_filter {
                let want = gw2_core::i18n::weapon_type_key(wt);
                let got = skill
                    .weapon_type
                    .as_deref()
                    .map(gw2_core::i18n::weapon_type_key)
                    .unwrap_or_default();
                if got != want {
                    continue;
                }
            }

            let matches = skill.facts.iter().any(|f| match f {
                Fact::Buff {
                    status: Some(s), ..
                }
                | Fact::PrefixedBuff {
                    status: Some(s), ..
                } => s.to_lowercase().contains(&needle),
                Fact::ComboField {
                    field_type: Some(ft),
                    ..
                } => ft.to_lowercase().contains(&needle),
                Fact::ComboFinisher {
                    finisher_type: Some(ft),
                    ..
                } => ft.to_lowercase().contains(&needle),
                _ => false,
            });

            if matches {
                let conditions = extract_conditions(&skill.facts);
                let buffs = extract_buffs(&skill.facts);
                results.push(json!({
                    "id": skill.id,
                    "name": &skill.name,
                    "slot": skill.slot.as_deref().unwrap_or(""),
                    "weapon_type": skill.weapon_type.as_deref().unwrap_or(""),
                    "conditions_applied": conditions,
                    "buffs_applied": buffs
                }));
                if results.len() >= 20 {
                    break;
                }
            }
        }
    }

    json!({ "effect": effect, "count": results.len(), "skills": results })
}

fn exec_find_synergies(args: &Value, ctx: &ToolContext) -> Value {
    let trait_ids: Vec<u32> = args
        .get("trait_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();

    let trait_id_set: std::collections::HashSet<u32> = trait_ids.iter().copied().collect();
    let mut activated_synergies: Vec<Value> = Vec::new();

    for &tid in &trait_ids {
        let Some(t) = ctx.db.traits.get(&tid) else {
            continue;
        };

        for tf in &t.traited_facts {
            if trait_id_set.contains(&tf.requires_trait) {
                let req_name = ctx
                    .db
                    .traits
                    .get(&tf.requires_trait)
                    .map(|r| r.name.as_str())
                    .unwrap_or("Unknown");
                let fact_json = format_fact(&tf.fact);
                activated_synergies.push(json!({
                    "trait": &t.name,
                    "trait_id": t.id,
                    "requires": req_name,
                    "requires_id": tf.requires_trait,
                    "activated_effect": fact_json,
                    "overrides_base_fact": tf.overrides
                }));
            }
        }
    }

    // Also check what conditions the selected traits apply
    let mut conditions_from_traits: Vec<Value> = Vec::new();
    for &tid in &trait_ids {
        let Some(t) = ctx.db.traits.get(&tid) else {
            continue;
        };
        let condis = extract_conditions(&t.facts);
        if !condis.is_empty() {
            conditions_from_traits.push(json!({
                "trait": &t.name,
                "conditions": condis
            }));
        }
    }

    json!({
        "trait_count": trait_ids.len(),
        "activated_synergies": activated_synergies,
        "conditions_from_traits": conditions_from_traits
    })
}

fn exec_get_build_synergy_report(args: &Value, ctx: &ToolContext) -> Value {
    let trait_ids: Vec<u32> = args
        .get("trait_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();
    let gear_prefix = args
        .get("gear_prefix")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let rune_name = args.get("rune_name").and_then(|v| v.as_str());

    // 1. Trait synergies (activated traited_facts)
    let synergies_result = exec_find_synergies(args, ctx);

    // 2. Damage modifiers from traits
    let modifiers = combat::extract_damage_modifiers(
        &trait_ids,
        None,
        &[],
        None,
        &ctx.db.traits,
        &ctx.db.items,
        ctx.balance_ctx,
    );

    // 3. All conditions the build can apply
    let mut all_conditions: HashMap<String, Vec<String>> = HashMap::new();
    for &tid in &trait_ids {
        if let Some(t) = ctx.db.traits.get(&tid) {
            for f in &t.facts {
                if let Fact::Buff {
                    status: Some(s), ..
                } = f
                {
                    let conditions = ["Bleeding", "Burning", "Poisoned", "Torment", "Confusion"];
                    let canonical =
                        crate::data::boon_condition_formulas::canonical_condition_name(s);
                    if conditions.contains(&canonical) {
                        all_conditions
                            .entry(canonical.to_string())
                            .or_default()
                            .push(t.name.clone());
                    }
                }
            }
        }
    }

    // 4. Rune synergy (if specified)
    let rune_info = rune_name.and_then(|name| {
        ctx.db
            .all_runes()
            .iter()
            .find(|r| r.name.to_lowercase().contains(&name.to_lowercase()))
            .map(|item| {
                let bonuses = item
                    .details
                    .as_ref()
                    .map(|d| &d.bonuses)
                    .cloned()
                    .unwrap_or_default();
                let parsed: Vec<Value> = bonuses
                    .iter()
                    .map(|b| parse_rune_bonus_structured(b))
                    .collect();
                json!({ "name": &item.name, "bonuses": parsed })
            })
    });

    json!({
        "gear_prefix": gear_prefix,
        "trait_synergies": synergies_result["activated_synergies"],
        "damage_modifiers": {
            "total_strike_mult": format!("{:.3}", modifiers.total_strike_mult()),
            "total_condi_mult": format!("{:.3}", modifiers.total_condi_mult()),
            "total_healing_mult": format!("{:.3}", modifiers.total_healing_mult()),
            "crit_damage_bonus": format!("{:.1}%", modifiers.total_crit_damage_bonus()),
            "condi_duration_bonus": format!("{:.1}%", modifiers.total_condi_duration_bonus()),
            "boon_duration_bonus": format!("{:.1}%", modifiers.total_boon_duration_bonus())
        },
        "condition_sources": all_conditions,
        "rune": rune_info
    })
}

/// Upper bound on `simulate_rotation`'s `duration_seconds` tool argument.
///
/// `rotation::simulator::SimState::run` advances one scheduled action at a
/// time for the full requested duration — there is no early-exit budget like
/// the search beam's deadline/eval cap. Ten minutes of simulated combat is far
/// beyond any real rotation-planning need; it exists to keep a malformed or
/// adversarial tool call from turning into a multi-hour compute loop on the
/// chat/optimize worker thread (the same thread `AddonState::spawn_worker`'s
/// `UNLOAD_JOIN_BUDGET` is trying to bound).
const MAX_ROTATION_DURATION_SECONDS: u32 = 600;

/// Clamp a `duration_seconds` tool argument into `1..=MAX_ROTATION_DURATION_SECONDS`.
///
/// Clamps the `u64` *before* narrowing to `u32` — a bare `as u32` on an
/// oversized LLM-supplied value (e.g. `u64::MAX`) wraps instead of erroring,
/// and `duration_s * 1000` on the wrapped result can itself overflow `u32`.
/// Missing/unparseable input keeps the documented default of 30 seconds.
fn clamp_rotation_duration(raw: Option<u64>) -> u32 {
    raw.map(|n| n.clamp(1, MAX_ROTATION_DURATION_SECONDS as u64) as u32)
        .unwrap_or(30)
}

/// Sort a flat skill list into (set 1, set 2, non-weapon) by the weapon each
/// skill belongs to. Two-handers and main-hand skills (slots 1-3) open a set;
/// off-hand skills (slots 4-5) join the first set without an off-hand.
fn split_weapon_sets(
    skill_ids: &[u32],
    db: &crate::gamedb::GameDb,
) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    const TWO_HANDED: [&str; 9] = [
        "Greatsword",
        "Hammer",
        "Longbow",
        "Rifle",
        "Shortbow",
        "Staff",
        "Spear",
        "Speargun",
        "Trident",
    ];
    let mut sets: [(Option<String>, Option<String>, Vec<u32>); 2] =
        [(None, None, Vec::new()), (None, None, Vec::new())];
    let mut other = Vec::new();
    for &id in skill_ids {
        let Some(skill) = db.skills.get(&id) else {
            continue;
        };
        let (Some(weapon), Some(slot)) = (skill.weapon_type.as_deref(), skill.slot.as_deref())
        else {
            other.push(id);
            continue;
        };
        if !slot.starts_with("Weapon_") {
            other.push(id);
            continue;
        }
        let two_handed = TWO_HANDED.iter().any(|w| w.eq_ignore_ascii_case(weapon));
        let main_hand = two_handed || matches!(slot, "Weapon_1" | "Weapon_2" | "Weapon_3");
        // A weapon already placed keeps its set.
        let placed = sets.iter().position(|(main, off, _)| {
            main.as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case(weapon))
                || off
                    .as_deref()
                    .is_some_and(|o| o.eq_ignore_ascii_case(weapon))
        });
        let target = placed.or_else(|| {
            if main_hand {
                sets.iter().position(|(main, _, _)| main.is_none())
            } else {
                sets.iter().position(|(_, off, _)| off.is_none())
            }
        });
        let Some(target) = target else {
            other.push(id);
            continue;
        };
        if placed.is_none() {
            if main_hand {
                sets[target].0 = Some(weapon.to_string());
                if two_handed {
                    sets[target].1 = Some(weapon.to_string());
                }
            } else {
                sets[target].1 = Some(weapon.to_string());
            }
        }
        sets[target].2.push(id);
    }
    let [(_, _, set1), (_, _, set2)] = sets;
    (set1, set2, other)
}

fn exec_simulate_rotation(args: &Value, ctx: &ToolContext) -> Value {
    use crate::rotation;

    let mut skill_ids: Vec<u32> = args
        .get("skill_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();
    // A build has at most ~10 bar skills plus swaps; cap generously to bound
    // CPU on the LLM thread against a runaway model-supplied list.
    skill_ids.truncate(64);

    if skill_ids.is_empty() {
        return json!({ "error": "No skill IDs provided" });
    }

    let duration_s = clamp_rotation_duration(args.get("duration_seconds").and_then(|v| v.as_u64()));

    let gear_prefix = args
        .get("gear_prefix")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let Some(gear_stats) = find_itemstat_by_name(ctx.db, gear_prefix)
        .and_then(|istat| calculate_full_set_stats(ctx.db, istat, ctx.balance_ctx))
    else {
        return json!({
            "error": format!(
                "No stat sheet for '{}' in {}: the prefix is unknown, or it has no budget the game mode can price.",
                gear_prefix,
                ctx.balance_ctx.game_mode.label()
            )
        });
    };
    let mut full = stats::base_stats();
    full += &gear_stats;
    let mut trait_ids: Vec<u32> = args
        .get("trait_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();
    // A build has at most 9 traits; cap generously to bound CPU on the LLM
    // thread against a runaway model-supplied list.
    trait_ids.truncate(36);
    let params = rotation_sim_params(
        &full,
        ctx.profession_name,
        ctx.balance_ctx,
        ctx.db,
        &trait_ids,
    );

    // The model hands over a flat id list. Weapon skills belong to a set —
    // two weapons cannot both be in hand — so group them the way the engine
    // does: first weapon seen is set 1, the next main-hand weapon is set 2,
    // off-hands fill whichever set still has room. Everything else is set 0.
    let (set1_ids, set2_ids, other_ids) = split_weapon_sets(&skill_ids, ctx.db);
    let mut rotation_skills =
        rotation::builder::build_rotation_skills_for_context(&other_ids, ctx.db, ctx.balance_ctx);
    let mut set1 =
        rotation::builder::build_rotation_skills_for_context(&set1_ids, ctx.db, ctx.balance_ctx);
    rotation::builder::tag_weapon_set(&mut set1, 1);
    let mut set2 =
        rotation::builder::build_rotation_skills_for_context(&set2_ids, ctx.db, ctx.balance_ctx);
    rotation::builder::tag_weapon_set(&mut set2, 2);
    rotation_skills.extend(rotation::builder::merge_weapon_sets(set1, set2));

    if rotation_skills.is_empty() {
        return json!({ "error": "No valid skills found for the provided IDs" });
    }

    let result = rotation::simulator::simulate_with(
        &rotation_skills,
        duration_s * 1000,
        &params,
        rotation::combat_model::EnemyDummy::open(),
    );

    // Format condition uptimes
    let mut condition_uptimes: Vec<Value> = result
        .condition_uptime
        .iter()
        .map(|(name, avg_stacks)| {
            json!({
                "condition": name,
                "avg_stacks": format!("{:.1}", avg_stacks)
            })
        })
        .collect();
    condition_uptimes.sort_by(|a, b| a["condition"].as_str().cmp(&b["condition"].as_str()));

    // Format buff uptimes
    let mut buff_uptimes: Vec<Value> = result
        .buff_uptime
        .iter()
        .map(|(name, fraction)| {
            json!({
                "buff": name,
                "uptime_pct": format!("{:.0}%", fraction * 100.0)
            })
        })
        .collect();
    buff_uptimes.sort_by(|a, b| a["buff"].as_str().cmp(&b["buff"].as_str()));

    // Format skill usage
    let skill_usage: Vec<Value> = result
        .skill_usage
        .iter()
        .map(|su| {
            json!({
                "skill": &su.name,
                "casts": su.cast_count,
                "dps": format!("{:.0}", su.dps_contribution)
            })
        })
        .collect();

    json!({
        "duration_s": duration_s,
        "dps": {
            "strike": format!("{:.0}", result.strike_dps),
            "condition": format!("{:.0}", result.condition_dps),
            "total": format!("{:.0}", result.total_dps)
        },
        "condition_uptime": condition_uptimes,
        "buff_uptime": buff_uptimes,
        "skill_usage": skill_usage,
        "control_metrics": {
            "stunbreak_count": result.stunbreak_count,
            "has_stability": result.has_stability,
            "stability_uptime_pct": format!("{:.0}%", result.stability_uptime * 100.0)
        },
        "skills_simulated": rotation_skills.len()
    })
}

/// A full kit of one prefix, priced the same way the optimizer prices one.
///
/// Walking the raw `EQUIPMENT_SLOTS` table counted **all sixteen** entries,
/// including the inactive weapon set, and billed the active set's main hand a
/// two-hand budget *and* its off-hand a one-hand budget. A ThreeStat major came
/// out at 1883 instead of 1507, so Choya's plated build showed ~2883 Power for
/// the same prefix Optimize showed ~2507 on, and the LLM reasoned over the
/// inflated block through `calculate_stats` / `simulate_combat` / `score_build`.
/// [`crate::engine::apply_optimized_gear_stats`] is the one model now — same
/// armour, same trinkets, one land weapon set — so the two panels agree.
fn calculate_full_set_stats(
    db: &GameDb,
    itemstat: &ItemStat,
    ctx: &BalanceContext,
) -> Option<stats::StatBlock> {
    let mut gear_stats = stats::StatBlock::default();
    match crate::engine::apply_optimized_gear_stats(&mut gear_stats, db, Some(itemstat.id), ctx) {
        // Unpriceable prefix (no PvP amulet, or a flat-value legacy row): a
        // zeroed stat sheet is not an estimate, so say there is none.
        Some(_) => None,
        None => Some(gear_stats),
    }
}

/// Prefix-only combat inputs for the LLM rotation tool. Trait mods stay at
/// default — the tool sees a named prefix, not a validated trait line.
/// Vs-target / per-stack percents still come from `extract_damage_modifiers`
/// (same list `prepare_validated_rotation` clones into SimParams).
/// Weapon strength stays 1100 until W068 publishes `REFERENCE_WEAPON_STRENGTH`.
fn rotation_sim_params(
    full: &stats::StatBlock,
    profession: &str,
    ctx: &BalanceContext,
    db: &GameDb,
    trait_ids: &[u32],
) -> crate::rotation::simulator::SimParams {
    let derived = stats::compute_derived(full, profession);
    let mods = DamageModifiers::default();
    let mode = ctx.game_mode.clone();
    crate::rotation::simulator::SimParams {
        power: full.power,
        condition_damage: full.condition_damage,
        weapon_strength: 1100.0,
        precision: full.precision,
        ferocity: full.ferocity,
        crit_chance_bonus: 0.0,
        fury_crit_chance_bonus: crate::data::boon_condition_formulas::boons()
            .fury_crit_bonus(mode.clone())
            * 100.0,
        strike_mult: 1.0,
        condition_mult: 1.0,
        condition_duration_mult: combat::outgoing_condition_duration_mult(
            full.expertise,
            &mods,
            ctx,
        ),
        boon_duration_mult: combat::outgoing_boon_duration_mult(full.concentration, &mods, ctx),
        healing_power: full.healing_power,
        healing_mult: 1.0,
        max_health: derived.health,
        armor: derived.armor,
        mode,
        intent: None,
        deferred_target: combat::extract_damage_modifiers(
            trait_ids,
            None,
            &[],
            None,
            &db.traits,
            &db.items,
            ctx,
        )
        .deferred_target,
        weaver: trait_ids.iter().any(|id| {
            db.traits
                .get(id)
                .is_some_and(|t| t.specialization == crate::rotation::attunement::WEAVER_SPEC_ID)
        }),
        form: None,
        triggered: Vec::new(),
        strike_add: 0.0,
        condition_add: 0.0,
        folded: Default::default(),
    }
}

/// Gear + base + derived stats for a named prefix, as a land PvE kit.
///
/// Used by the `calculate_stats` tool and by Choya plating so the Stats tab is
/// not a column of zeros. Prefer [`estimate_prefix_stats_in`] wherever the game
/// mode is known — in PvP a prefix is worth its amulet or nothing at all, and
/// pricing it as land gear is the exact inflation that made amulet-less
/// prefixes look best.
pub fn estimate_prefix_stats(
    db: &GameDb,
    prefix: &str,
    profession: &str,
) -> Option<(String, stats::StatBlock, stats::DerivedStats)> {
    estimate_prefix_stats_in(db, prefix, profession, &BalanceContext::pve())
}

/// [`estimate_prefix_stats`] for a specific game mode.
///
/// `None` when the prefix does not resolve, and also when the mode cannot price
/// it — a PvP prefix with no amulet has no stat sheet, and returning zeros
/// would read as "this build is terrible" rather than "ask for a different
/// prefix".
pub fn estimate_prefix_stats_in(
    db: &GameDb,
    prefix: &str,
    profession: &str,
    ctx: &BalanceContext,
) -> Option<(String, stats::StatBlock, stats::DerivedStats)> {
    let itemstat = db.itemstat_by_name(prefix)?;
    let gear = calculate_full_set_stats(db, itemstat, ctx)?;
    let mut full = stats::base_stats();
    full += &gear;
    let derived = stats::compute_derived(&full, profession);
    Some((itemstat.name.clone(), full, derived))
}

/// Summarize a trait's key mechanical effects (for trait column overview).
fn summarize_trait_facts(t: &GW2Trait) -> Vec<String> {
    let mut effects = Vec::new();
    for fact in &t.facts {
        match fact {
            Fact::AttributeAdjust { text, value, .. } => {
                if let (Some(text), Some(val)) = (text, value) {
                    effects.push(format!("{}: {}", text, val));
                }
            }
            Fact::Buff {
                status, duration, ..
            }
            | Fact::PrefixedBuff {
                status, duration, ..
            } => {
                if let Some(status) = status {
                    let dur = duration.map(|d| format!(" ({}s)", d)).unwrap_or_default();
                    effects.push(format!("Applies {}{}", status, dur));
                }
            }
            Fact::Damage {
                hit_count,
                dmg_multiplier,
                ..
            } => {
                let hits = hit_count.unwrap_or(1);
                let mult = dmg_multiplier.unwrap_or(1.0);
                effects.push(format!("Damage: {}x {:.2}", hits, mult));
            }
            Fact::Percent { text, percent, .. } => {
                if let (Some(text), Some(pct)) = (text, percent) {
                    effects.push(format!("{}: {}%", text, pct));
                }
            }
            _ => {}
        }
        if effects.len() >= 5 {
            break;
        }
    }
    effects
}

/// Extract conditions applied by a set of facts.
///
/// API `Fact::Buff.status` may emit either verb-form (Poison) or canonical
/// (Poisoned) names. Inputs are normalized via `canonical_condition_name`
/// before filtering, and the canonical form is shipped in JSON so the LLM
/// sees one consistent name per condition. `Immobilized` stays in the
/// list — the resolver only knows `Immobilize→Immobile`.
fn extract_conditions(facts: &[Fact]) -> Vec<Value> {
    use crate::data::boon_condition_formulas::canonical_condition_name;
    let conditions = [
        "Bleeding",
        "Burning",
        "Poisoned",
        "Torment",
        "Confusion",
        "Vulnerability",
        "Weakness",
        "Blinded",
        "Chilled",
        "Crippled",
        "Fear",
        "Immobile",
        "Immobilized",
        "Slow",
        "Taunt",
    ];
    facts
        .iter()
        .filter_map(|f| match f {
            Fact::Buff {
                status: Some(s),
                duration,
                apply_count,
                ..
            }
            | Fact::PrefixedBuff {
                status: Some(s),
                duration,
                apply_count,
                ..
            } => {
                let canonical = canonical_condition_name(s);
                if conditions.contains(&canonical) {
                    Some(json!({
                        "condition": canonical,
                        "stacks": apply_count.unwrap_or(1),
                        "duration_s": duration.unwrap_or(0)
                    }))
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect()
}

/// Extract boons/buffs applied by a set of facts.
fn extract_buffs(facts: &[Fact]) -> Vec<Value> {
    let boons = [
        "Might",
        "Fury",
        "Quickness",
        "Alacrity",
        "Protection",
        "Resolution",
        "Regeneration",
        "Vigor",
        "Stability",
        "Swiftness",
        "Resistance",
        "Aegis",
    ];
    facts
        .iter()
        .filter_map(|f| match f {
            Fact::Buff {
                status: Some(s),
                duration,
                apply_count,
                ..
            }
            | Fact::PrefixedBuff {
                status: Some(s),
                duration,
                apply_count,
                ..
            } if boons.contains(&s.as_str()) => Some(json!({
                "buff": s,
                "stacks": apply_count.unwrap_or(1),
                "duration_s": duration.unwrap_or(0)
            })),
            _ => None,
        })
        .collect()
}

/// Extract stat bonuses from AttributeAdjust facts.
fn extract_stat_bonuses(facts: &[Fact]) -> Vec<Value> {
    facts
        .iter()
        .filter_map(|f| {
            if let Fact::AttributeAdjust {
                text,
                target,
                value: Some(val),
                ..
            } = f
            {
                if !crate::stats::is_permanent_stat_adjust(text.as_deref()) {
                    return None;
                }
                Some(json!({
                    "stat": target.as_deref().unwrap_or("unknown"),
                    "value": val
                }))
            } else {
                None
            }
        })
        .collect()
}

/// Extract damage modifier percentages from Percent facts.
fn extract_damage_modifiers_from_facts(facts: &[Fact]) -> Vec<Value> {
    facts
        .iter()
        .filter_map(|f| {
            if let Fact::Percent {
                text,
                percent: Some(pct),
                ..
            } = f
            {
                Some(json!({
                    "description": text.as_deref().unwrap_or(""),
                    "percent": pct
                }))
            } else {
                None
            }
        })
        .collect()
}

/// Detect proc triggers by scanning fact descriptions.
fn detect_proc_triggers(facts: &[Fact], description: Option<&str>) -> Vec<String> {
    let mut triggers = Vec::new();
    let trigger_patterns = [
        ("on crit", "on_critical_hit"),
        ("critical hit", "on_critical_hit"),
        ("on hit", "on_hit"),
        ("when you apply", "on_condition_apply"),
        ("when applying", "on_condition_apply"),
        ("on dodge", "on_dodge"),
        ("when you dodge", "on_dodge"),
        ("when struck", "when_struck"),
        ("when you are struck", "when_struck"),
        ("when health", "health_threshold"),
        ("above 90%", "health_above_90"),
        ("below 50%", "health_below_50"),
        ("on weapon swap", "on_weapon_swap"),
        ("on kill", "on_kill"),
        ("when you use a heal", "on_heal_skill"),
        ("on interval", "periodic"),
        ("recharge of 20", "long_recharge"),
        ("20 seconds or more", "long_recharge"),
        ("stunned", "on_cc"),
        ("disabled foe", "on_cc"),
        ("hitting a disabled", "on_cc"),
    ];

    let mut check_text = |text: &str| {
        let lower = text.to_lowercase();
        for &(pattern, trigger) in &trigger_patterns {
            if lower.contains(pattern) && !triggers.contains(&trigger.to_string()) {
                triggers.push(trigger.to_string());
            }
        }
    };

    for fact in facts {
        match fact {
            Fact::Buff { text: Some(t), .. }
            | Fact::Percent { text: Some(t), .. }
            | Fact::AttributeAdjust { text: Some(t), .. }
            | Fact::Damage { text: Some(t), .. } => check_text(t),
            _ => {}
        }
    }

    if let Some(desc) = description {
        check_text(desc);
    }

    triggers
}

/// Parse a rune bonus string into structured JSON.
fn parse_rune_bonus_structured(bonus: &str) -> Value {
    let lower = bonus.to_lowercase();

    // Stat bonus: "+125 Power", "+180 Precision", etc.
    if let Some(captures) = extract_stat_bonus(bonus) {
        return captures;
    }

    // Condition duration: "+10% Burning Duration"
    // Search terms stay in verb form (GW2 tooltip text uses "Poison Duration",
    // not "Poisoned Duration"); the emitted `condition` key is canonicalized
    // so LLM / downstream consumers see a single spelling per condition.
    for condi in &["Bleeding", "Burning", "Poison", "Torment", "Confusion"] {
        if lower.contains(&condi.to_lowercase()) && lower.contains("duration") {
            if let Some(pct) = extract_number(bonus) {
                let canonical =
                    crate::data::boon_condition_formulas::canonical_condition_name(condi);
                return json!({
                    "type": "condition_duration",
                    "condition": canonical,
                    "value_pct": pct,
                    "raw": bonus
                });
            }
        }
    }

    // Boon duration
    if lower.contains("boon duration") {
        if let Some(pct) = extract_number(bonus) {
            return json!({ "type": "boon_duration", "value_pct": pct, "raw": bonus });
        }
    }

    // Condition damage bonus
    if lower.contains("condition damage") {
        if let Some(val) = extract_number(bonus) {
            return json!({ "type": "stat", "stat": "ConditionDamage", "value": val, "raw": bonus });
        }
    }

    // Damage modifier: "+5% damage", "+7% Condition Damage"
    if lower.contains("% damage") || lower.contains("% all damage") {
        if let Some(pct) = extract_number(bonus) {
            return json!({ "type": "damage_modifier", "value_pct": pct, "raw": bonus });
        }
    }

    // Fallback: return raw text
    json!({ "type": "other", "raw": bonus })
}

/// Extract a stat bonus like "+125 Power" from text.
fn extract_stat_bonus(text: &str) -> Option<Value> {
    let stats = [
        "Power",
        "Precision",
        "Toughness",
        "Vitality",
        "Ferocity",
        "Condition Damage",
        "Expertise",
        "Concentration",
        "Healing Power",
    ];
    for stat in &stats {
        if text.contains(stat) {
            if let Some(val) = extract_number(text) {
                return Some(json!({
                    "type": "stat",
                    "stat": stat,
                    "value": val,
                    "raw": text
                }));
            }
        }
    }
    None
}

/// Extract the first number from text.
fn extract_number(text: &str) -> Option<f64> {
    crate::text_util::first_number(&crate::text_util::strip_gw2_markup(text))
}

/// Format a single Fact into a JSON value for tool responses.
fn format_fact(fact: &Fact) -> Value {
    match fact {
        Fact::AttributeAdjust {
            text,
            target,
            value,
            ..
        } => json!({
            "type": "AttributeAdjust",
            "text": text,
            "target": target,
            "value": value
        }),
        Fact::Buff {
            text,
            status,
            duration,
            apply_count,
            ..
        } => json!({
            "type": "Buff",
            "text": text,
            "status": status,
            "duration": duration,
            "apply_count": apply_count
        }),
        Fact::BuffConversion {
            text,
            source,
            target,
            percent,
            ..
        } => json!({
            "type": "BuffConversion",
            "text": text,
            "source": source,
            "target": target,
            "percent": percent
        }),
        Fact::Damage {
            text,
            hit_count,
            dmg_multiplier,
            ..
        } => json!({
            "type": "Damage",
            "text": text,
            "hit_count": hit_count,
            "dmg_multiplier": dmg_multiplier
        }),
        Fact::Percent { text, percent, .. } => json!({
            "type": "Percent",
            "text": text,
            "percent": percent
        }),
        Fact::Recharge { text, value, .. } => json!({
            "type": "Recharge",
            "text": text,
            "value": value
        }),
        Fact::Number { text, value, .. } => json!({
            "type": "Number",
            "text": text,
            "value": value
        }),
        Fact::Duration { text, duration, .. } => json!({
            "type": "Duration",
            "text": text,
            "duration": duration
        }),
        Fact::ComboField {
            text, field_type, ..
        } => json!({
            "type": "ComboField",
            "text": text,
            "field_type": field_type
        }),
        Fact::ComboFinisher {
            text,
            finisher_type,
            percent,
            ..
        } => json!({
            "type": "ComboFinisher",
            "text": text,
            "finisher_type": finisher_type,
            "percent": percent
        }),
        Fact::Distance { text, distance, .. } => json!({
            "type": "Distance",
            "text": text,
            "distance": distance
        }),
        Fact::Radius { text, distance, .. } => json!({
            "type": "Radius",
            "text": text,
            "distance": distance
        }),
        Fact::Range { text, value, .. } => json!({
            "type": "Range",
            "text": text,
            "value": value
        }),
        Fact::Time { text, duration, .. } => json!({
            "type": "Time",
            "text": text,
            "duration": duration
        }),
        Fact::NoData { text, .. } => json!({
            "type": "NoData",
            "text": text
        }),
        Fact::PrefixedBuff {
            text,
            status,
            duration,
            apply_count,
            ..
        } => json!({
            "type": "PrefixedBuff",
            "text": text,
            "status": status,
            "duration": duration,
            "apply_count": apply_count
        }),
        Fact::StunBreak { text, value, .. } => json!({
            "type": "StunBreak",
            "text": text,
            "value": value
        }),
        Fact::HealingAdjust {
            text, hit_count, ..
        } => json!({
            "type": "HealingAdjust",
            "text": text,
            "hit_count": hit_count
        }),
        _ => json!({ "type": "Other" }),
    }
}

/// Format CombatPerformance into a concise JSON value.
fn format_combat_performance(perf: &CombatPerformance, label: &str) -> Value {
    json!({
        "profile": label,
        "effective_power": perf.effective_power.round() as i32,
        "strike_dps_index": perf.strike_dps_index.round() as i32,
        "condition_dps_index": perf.condition_dps_index.round() as i32,
        "total_dps_index": perf.total_dps_index.round() as i32,
        "healing_power_index": perf.healing_power_index.round() as i32,
        "crit_chance": format!("{:.1}%", perf.crit_chance),
        "boon_duration": format!("{:.1}%", perf.boon_duration_pct),
        "condi_duration": format!("{:.1}%", perf.condi_duration_pct),
        "effective_health": perf.effective_health.round() as i32,
        "damage_reduction": format!("{:.1}%", perf.damage_reduction_pct),
        "condition_ticks": {
            "bleeding": perf.condition_ticks.bleeding.round() as i32,
            "burning": perf.condition_ticks.burning.round() as i32,
            "poison": perf.condition_ticks.poison.round() as i32,
            "torment": perf.condition_ticks.torment.round() as i32,
            "confusion": perf.condition_ticks.confusion.round() as i32
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_api::models::StatAttribute;

    #[test]
    fn test_tool_declarations_count() {
        let tools = tool_declarations();
        assert_eq!(tools.len(), 1); // Single tool block
        assert_eq!(tools[0].function_declarations.len(), 20);
    }

    #[test]
    fn test_tool_declarations_have_names() {
        let tools = tool_declarations();
        let names: Vec<&str> = tools[0]
            .function_declarations
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert!(names.contains(&"get_profession_info"));
        assert!(names.contains(&"get_spec_traits"));
        assert!(names.contains(&"simulate_combat"));
        assert!(names.contains(&"list_runes"));
        assert!(names.contains(&"search_upgrades"));
        assert!(names.contains(&"upgrade_synergies"));
        assert!(names.contains(&"get_optimizer_results"));
        // Synergy tools
        assert!(names.contains(&"search_traits_by_effect"));
        assert!(names.contains(&"find_condition_sources"));
        assert!(names.contains(&"search_skills_by_effect"));
        assert!(names.contains(&"find_synergies"));
        assert!(names.contains(&"get_build_synergy_report"));
        assert!(names.contains(&"simulate_rotation"));
    }

    #[test]
    fn test_format_combat_performance() {
        let perf = CombatPerformance {
            effective_power: 12345.6,
            strike_dps_index: 5000.0,
            condition_dps_index: 3000.0,
            total_dps_index: 8000.0,
            healing_power_index: 500.0,
            boon_duration_pct: 45.5,
            condi_duration_pct: 30.0,
            effective_health: 20000.0,
            damage_reduction_pct: 35.5,
            ..Default::default()
        };
        let result = format_combat_performance(&perf, "Solo");
        assert_eq!(result["profile"], "Solo");
        assert_eq!(result["effective_power"], 12346);
        assert_eq!(result["total_dps_index"], 8000);
    }

    // find_itemstat_by_name() determinism

    fn db_with_itemstats(stats: Vec<(u32, &str)>) -> GameDb {
        let mut itemstats = HashMap::new();
        for (id, name) in stats {
            itemstats.insert(
                id,
                ItemStat {
                    id,
                    name: name.into(),
                    attributes: vec![],
                },
            );
        }
        GameDb {
            items: HashMap::new(),
            itemstats,
            skills: HashMap::new(),
            traits: HashMap::new(),
            specializations: HashMap::new(),
            professions: HashMap::new(),
            legends: HashMap::new(),
            pvp_amulets: HashMap::new(),
            pets: HashMap::new(),
            skills_by_profession: HashMap::new(),
            traits_by_spec: HashMap::new(),
            items_by_type: HashMap::new(),
            runes: vec![],
            sigils: vec![],
            relics: vec![],
            skill_to_palette: HashMap::new(),
            palette_to_skill: HashMap::new(),
            traits_by_condition: HashMap::new(),
            skills_by_condition: HashMap::new(),
            traits_by_buff: HashMap::new(),
            skills_by_buff: HashMap::new(),
            localized: None,
        }
    }

    #[test]
    fn test_find_itemstat_exact_match_wins_over_substring() {
        let db = db_with_itemstats(vec![
            (10, "Marauder's Berserker Combo"),
            (11, "Berserker's"),
        ]);
        let is = find_itemstat_by_name(&db, "Berserker's").expect("match");
        assert_eq!(is.id, 11);
    }

    #[test]
    fn test_find_itemstat_fuzzy_prefers_shortest_name() {
        // Three substring matches. Run repeatedly — HashMap iteration would
        // otherwise leak through. Shortest name (Viper's = 7) must always win.
        let db = db_with_itemstats(vec![
            (20, "Trailblazer's Viper Combo"),
            (21, "Viper's"),
            (22, "Carrion-Viper Hybrid"),
        ]);
        for _ in 0..20 {
            let is = find_itemstat_by_name(&db, "Viper").expect("match");
            assert_eq!(is.id, 21, "Viper's must win every run");
        }
    }

    #[test]
    fn test_find_itemstat_empty_needle_returns_none() {
        let db = db_with_itemstats(vec![(30, "Berserker's")]);
        assert!(find_itemstat_by_name(&db, "").is_none());
    }

    #[test]
    fn get_skill_info_empty_needle_is_empty() {
        let mut db = GameDb::empty_for_tests();
        db.skills.insert(
            1,
            gw2_api::models::Skill {
                id: 1,
                name: "Healing Spring".into(),
                description: None,
                icon: None,
                chat_link: None,
                skill_type: None,
                weapon_type: None,
                professions: vec!["Ranger".into()],
                slot: Some("Heal".into()),
                facts: vec![],
                traited_facts: vec![],
                categories: vec![],
                attunement: None,
                cost: None,
                dual_wield: None,
                flip_skill: None,
                initiative: None,
                next_chain: None,
                prev_chain: None,
                transform_skills: vec![],
                bundle_skills: vec![],
                toolbelt_skill: None,
                flags: vec![],
                specialization: None,
            },
        );
        let bal = BalanceContext::pve();
        let ctx = ToolContext {
            db: &db,
            profession_name: "Ranger",
            candidates: &[],
            current_build_summary: None,
            weights: OptimizationWeights::default(),
            balance_ctx: &bal,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(&bal),
        };
        let v = execute_tool("get_skill_info", &json!({ "skill_name": "" }), &ctx);
        assert!(
            v.get("id").is_none(),
            "empty needle must not dump a skill: {v}"
        );
        assert!(v.get("error").is_some(), "empty needle must error, got {v}");
        let spec = execute_tool("get_spec_traits", &json!({ "spec_name": "" }), &ctx);
        assert!(
            spec.get("error").is_some(),
            "empty spec needle must error: {spec}"
        );
        let tr = execute_tool("get_trait_details", &json!({ "trait_name": "" }), &ctx);
        assert!(
            tr.get("error").is_some(),
            "empty trait needle must error: {tr}"
        );
    }

    // exec_get_skill_info() fuzzy-match gate (mirrors find_named's ≥5 rule)

    fn make_skill(id: u32, name: &str) -> gw2_api::models::Skill {
        gw2_api::models::Skill {
            id,
            name: name.into(),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: None,
            professions: vec![],
            slot: None,
            facts: vec![],
            traited_facts: vec![],
            categories: vec![],
            attunement: None,
            cost: None,
            dual_wield: None,
            flip_skill: None,
            initiative: None,
            next_chain: None,
            prev_chain: None,
            transform_skills: vec![],
            bundle_skills: vec![],
            toolbelt_skill: None,
            flags: vec![],
            specialization: None,
        }
    }

    fn skill_ctx<'a>(db: &'a GameDb, bal: &'a BalanceContext) -> ToolContext<'a> {
        ToolContext {
            db,
            profession_name: "Guardian",
            candidates: &[],
            current_build_summary: None,
            weights: OptimizationWeights::default(),
            balance_ctx: bal,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(bal),
        }
    }

    #[test]
    fn get_skill_info_short_needle_never_fuzzy_matches() {
        let mut db = GameDb::empty_for_tests();
        db.skills.insert(1, make_skill(1, "Blazing Frenzy")); // contains "zi"
        let bal = BalanceContext::pve();
        let ctx = skill_ctx(&db, &bal);

        for needle in ["a", "zi"] {
            let v = execute_tool("get_skill_info", &json!({ "skill_name": needle }), &ctx);
            assert!(v.get("id").is_none(), "'{needle}' must not resolve: {v}");
            let err = v["error"].as_str().expect("short needle must error");
            assert!(
                err.contains("too short for fuzzy match"),
                "'{needle}' must report the min-length gate, got: {err}"
            );
        }

        // Empty name stays rejected with its own message.
        let v = execute_tool("get_skill_info", &json!({ "skill_name": "" }), &ctx);
        assert!(v.get("id").is_none(), "empty needle must not resolve: {v}");
        assert!(v.get("error").is_some(), "empty needle must error: {v}");
    }

    #[test]
    fn get_skill_info_exact_short_name_still_resolves() {
        let mut db = GameDb::empty_for_tests();
        db.skills.insert(7, make_skill(7, "Zap"));
        db.skills.insert(9, make_skill(9, "Zephyr Strike"));
        let bal = BalanceContext::pve();
        let ctx = skill_ctx(&db, &bal);

        // Case-insensitive exact match is always allowed, even under 5 chars.
        let v = execute_tool("get_skill_info", &json!({ "skill_name": "zAp" }), &ctx);
        assert_eq!(
            v["id"].as_u64(),
            Some(7),
            "exact short name must resolve: {v}"
        );
        assert_eq!(v["name"].as_str(), Some("Zap"));
    }

    #[test]
    fn get_skill_info_long_needle_still_fuzzy_matches() {
        let mut db = GameDb::empty_for_tests();
        db.skills.insert(1, make_skill(1, "Blazing Frenzy"));
        let bal = BalanceContext::pve();
        let ctx = skill_ctx(&db, &bal);

        let v = execute_tool("get_skill_info", &json!({ "skill_name": "blazi" }), &ctx);
        assert_eq!(
            v["id"].as_u64(),
            Some(1),
            "a >=5-char substring needle must still fuzzy-match: {v}"
        );
    }

    #[test]
    fn test_find_itemstat_id_tiebreak_when_lengths_equal() {
        let db = db_with_itemstats(vec![(50, "Foobar Alpha"), (40, "Foobar Betas")]);
        // Both length 12, both contain "Foobar" (>=5). Lower id (40) must win.
        let is = find_itemstat_by_name(&db, "Foobar").expect("match");
        assert_eq!(is.id, 40);
    }

    // find_named() generic lookup: Specialization + Trait

    fn make_spec(id: u32, name: &str) -> gw2_api::models::Specialization {
        gw2_api::models::Specialization {
            id,
            name: name.into(),
            profession: "Test".into(),
            elite: false,
            minor_traits: vec![],
            major_traits: vec![],
            weapon_trait: None,
            icon: None,
            background: None,
            profession_icon: None,
            profession_icon_big: None,
        }
    }

    fn make_trait(id: u32, name: &str) -> GW2Trait {
        GW2Trait {
            id,
            name: name.into(),
            icon: None,
            description: None,
            specialization: 0,
            tier: 1,
            order: 0,
            slot: "Major".into(),
            facts: vec![],
            traited_facts: vec![],
            skills: vec![],
        }
    }

    #[test]
    fn test_find_named_spec_exact_beats_substring() {
        let specs = vec![
            make_spec(1, "Berserker"),
            make_spec(2, "Berserker Variant Spec"),
        ];
        let s = find_named(&specs, "Berserker").expect("match");
        assert_eq!(s.id, 1);
    }

    #[test]
    fn test_find_named_spec_fuzzy_shortest_wins_deterministically() {
        // Two substring matches across many repeats — must be stable.
        let specs = vec![
            make_spec(10, "Soulbeast Special Edition"),
            make_spec(11, "Soulbeast"),
            make_spec(12, "Soulbeast Long Name Variant"),
        ];
        for _ in 0..20 {
            let s = find_named(&specs, "Soulbeast").expect("match");
            assert_eq!(s.id, 11, "shortest name must win every iteration");
        }
    }

    #[test]
    fn test_find_named_trait_fuzzy_id_tiebreak() {
        // Equal-length names containing needle → lower id wins.
        // Needle must be ≥5 (A15-4); "Big" is too short to fuzzy.
        let traits = vec![
            make_trait(200, "Big Trait Alpha"),
            make_trait(100, "Big Trait Bravo"),
        ];
        let t = find_named(&traits, "Trait").expect("match");
        assert_eq!(t.id, 100);
    }

    #[test]
    fn test_find_named_empty_needle_returns_none() {
        let traits = vec![make_trait(300, "Anything")];
        assert!(find_named(&traits, "").is_none());
    }

    #[test]
    fn extract_stat_bonuses_keeps_only_unconditional_panel_stats() {
        let facts = vec![
            Fact::AttributeAdjust {
                text: Some("Life Siphon Damage".into()),
                icon: None,
                value: Some(3517),
                target: Some("Power".into()),
            },
            Fact::AttributeAdjust {
                text: Some("Additional Power".into()),
                icon: None,
                value: Some(80),
                target: Some("Power".into()),
            },
        ];

        let bonuses = extract_stat_bonuses(&facts);
        assert!(bonuses.is_empty());
    }

    // C23: tools see the same sanitized kitchen as the prompt

    #[test]
    fn get_current_build_is_sanitized() {
        // A payload with prompt-fence and injection-ish characters, plus a
        // unique marker so a substring match against the real prompt output
        // cannot be a template coincidence.
        let raw_summary =
            "UNIQUE_MARKER_7f3c <script>alert(1)</script> ```inject``` Mode: PvE".to_string();

        // Independent proof: build the SAME prompt `send_chat_message` builds,
        // from the SAME raw string, through `chat_refinement_prompt_with_tools`'s
        // own sanitizer. The tool's answer must be byte-for-byte the text that
        // ended up in that prompt — not a second, differently-sanitized (or
        // unsanitized) copy handed to the model mid-conversation.
        let prompt = crate::prompts::chat_refinement_prompt_with_tools(
            "Guardian",
            "PvE",
            "give me a build",
            &raw_summary,
            "en",
        );

        let db = db_with_itemstats(vec![]);
        let empty_candidates: Vec<BuildCandidate> = vec![];
        let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let ctx = ToolContext {
            db: &db,
            profession_name: "Guardian",
            candidates: &empty_candidates,
            current_build_summary: Some(raw_summary.as_str()),
            weights: OptimizationWeights::default(),
            balance_ctx: &balance_ctx,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(&balance_ctx),
        };

        let result = exec_get_current_build(&ctx);
        let summary = result["summary"]
            .as_str()
            .expect("summary field must be a string");

        assert!(
            !summary.contains('`') && !summary.contains('<') && !summary.contains('>'),
            "tool must strip the same fence/injection characters the prompt sanitizer \
             strips: {summary:?}"
        );
        assert!(
            prompt.contains(summary),
            "tool's sanitized summary must be the exact text the prompt embedded, not a \
             richer/rawer view; summary={summary:?} was not found verbatim in the prompt"
        );
    }

    // C23: simulate_rotation's duration_seconds cannot run forever

    #[test]
    fn duration_seconds_is_clamped() {
        // Below the ceiling: passes through unchanged.
        assert_eq!(clamp_rotation_duration(Some(45)), 45);
        // At/above the ceiling: pinned to MAX_ROTATION_DURATION_SECONDS, not
        // wrapped by a bare u64->u32 cast (u64::MAX as u32 == u32::MAX, which the
        // old unclamped code fed straight into `duration_s * 1000` — u32
        // overflow).
        assert_eq!(
            clamp_rotation_duration(Some(u64::MAX)),
            MAX_ROTATION_DURATION_SECONDS
        );
        assert_eq!(
            clamp_rotation_duration(Some(1_000_000)),
            MAX_ROTATION_DURATION_SECONDS
        );
        // Zero floors to 1 rather than silently reaching the simulator's
        // internal "0 means default" convention — the tool's reported duration
        // must match what it actually asked the simulator to run.
        assert_eq!(clamp_rotation_duration(Some(0)), 1);
        // Missing argument keeps the documented default.
        assert_eq!(clamp_rotation_duration(None), 30);

        // Independent proof through the real tool entry point: an absurd
        // duration_seconds must come back clamped in the tool's own JSON answer,
        // and the call must return promptly — a hang here would mean the clamp
        // never reached `rotation::simulator::simulate_with`.
        let mut db = db_with_itemstats(vec![(1, "Berserker's")]);
        db.itemstats.insert(
            1,
            ItemStat {
                id: 1,
                name: "Berserker's".into(),
                attributes: vec![
                    StatAttribute {
                        attribute: "Power".into(),
                        multiplier: 0.35,
                        value: 0,
                    },
                    StatAttribute {
                        attribute: "Precision".into(),
                        multiplier: 0.25,
                        value: 0,
                    },
                    StatAttribute {
                        attribute: "Ferocity".into(),
                        multiplier: 0.25,
                        value: 0,
                    },
                ],
            },
        );
        db.skills.insert(
            999,
            gw2_api::models::Skill {
                id: 999,
                name: "Test Strike".into(),
                description: None,
                icon: None,
                chat_link: None,
                skill_type: None,
                weapon_type: None,
                professions: vec![],
                slot: None,
                facts: vec![],
                traited_facts: vec![],
                categories: vec![],
                attunement: None,
                cost: None,
                dual_wield: None,
                flip_skill: None,
                initiative: None,
                next_chain: None,
                prev_chain: None,
                transform_skills: vec![],
                bundle_skills: vec![],
                toolbelt_skill: None,
                flags: vec![],
                specialization: None,
            },
        );
        let empty_candidates: Vec<BuildCandidate> = vec![];
        let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let ctx = ToolContext {
            db: &db,
            profession_name: "Guardian",
            candidates: &empty_candidates,
            current_build_summary: None,
            weights: OptimizationWeights::default(),
            balance_ctx: &balance_ctx,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(&balance_ctx),
        };
        let args = json!({
            "skill_ids": [999],
            "gear_prefix": "Berserker's",
            "duration_seconds": u64::MAX
        });
        let result = exec_simulate_rotation(&args, &ctx);
        let reported = result["duration_s"]
            .as_u64()
            .expect("a valid skill list must produce a duration_s in the response");
        assert_eq!(
            reported, MAX_ROTATION_DURATION_SECONDS as u64,
            "tool must clamp an absurd duration_seconds, not run (or report) it unbounded"
        );
    }

    #[test]
    fn simulate_rotation_errors_when_prefix_cannot_be_priced() {
        let mut db = db_with_itemstats(vec![]);
        db.skills.insert(
            999,
            gw2_api::models::Skill {
                id: 999,
                name: "Test Strike".into(),
                description: None,
                icon: None,
                chat_link: None,
                skill_type: None,
                weapon_type: None,
                professions: vec![],
                slot: None,
                facts: vec![],
                traited_facts: vec![],
                categories: vec![],
                attunement: None,
                cost: None,
                dual_wield: None,
                flip_skill: None,
                initiative: None,
                next_chain: None,
                prev_chain: None,
                transform_skills: vec![],
                bundle_skills: vec![],
                toolbelt_skill: None,
                flags: vec![],
                specialization: None,
            },
        );
        let empty_candidates: Vec<BuildCandidate> = vec![];
        let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let ctx = ToolContext {
            db: &db,
            profession_name: "Guardian",
            candidates: &empty_candidates,
            current_build_summary: None,
            weights: OptimizationWeights::default(),
            balance_ctx: &balance_ctx,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(&balance_ctx),
        };
        let result = exec_simulate_rotation(
            &json!({ "skill_ids": [999], "gear_prefix": "NotARealPrefix" }),
            &ctx,
        );
        assert!(
            result
                .get("error")
                .and_then(|v| v.as_str())
                .is_some_and(|e| { e.contains("NotARealPrefix") && e.contains("No stat sheet") }),
            "unresolved prefix must be a tool error, not invented DPS: {result}"
        );
        assert!(result.get("dps").is_none());
    }

    #[test]
    fn simulate_rotation_uses_resolved_crit_not_basic_defaults() {
        let mut db = db_with_itemstats(vec![(1, "Berserker's")]);
        db.itemstats.insert(
            1,
            ItemStat {
                id: 1,
                name: "Berserker's".into(),
                attributes: vec![
                    StatAttribute {
                        attribute: "Power".into(),
                        multiplier: 0.35,
                        value: 0,
                    },
                    StatAttribute {
                        attribute: "Precision".into(),
                        multiplier: 0.25,
                        value: 0,
                    },
                    StatAttribute {
                        attribute: "Ferocity".into(),
                        multiplier: 0.25,
                        value: 0,
                    },
                ],
            },
        );
        let mut skill = make_skill(999, "Test Strike");
        skill.facts = vec![Fact::Damage {
            text: None,
            icon: None,
            hit_count: Some(1),
            dmg_multiplier: Some(1.0),
        }];
        db.skills.insert(999, skill);
        let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let ctx = ToolContext {
            db: &db,
            profession_name: "Guardian",
            candidates: &[],
            current_build_summary: None,
            weights: OptimizationWeights::default(),
            balance_ctx: &balance_ctx,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(&balance_ctx),
        };
        let result = exec_simulate_rotation(
            &json!({
                "skill_ids": [999],
                "gear_prefix": "Berserker's",
                "duration_seconds": 10
            }),
            &ctx,
        );
        let tool_strike: f64 = result["dps"]["strike"]
            .as_str()
            .expect("priced strike skill must report strike DPS")
            .parse()
            .expect("strike DPS is a formatted number");

        let skills =
            crate::rotation::builder::build_rotation_skills_for_context(&[999], &db, &balance_ctx);
        let gear = calculate_full_set_stats(
            &db,
            find_itemstat_by_name(&db, "Berserker's").expect("seeded prefix"),
            &balance_ctx,
        )
        .expect("Berserker's is priceable");
        let mut full = stats::base_stats();
        full += &gear;
        let basic = crate::rotation::simulator::simulate(
            &skills,
            10_000,
            full.power,
            full.condition_damage,
            1100.0,
        );
        let with = crate::rotation::simulator::simulate_with(
            &skills,
            10_000,
            &rotation_sim_params(&full, "Guardian", &balance_ctx, &db, &[]),
            crate::rotation::combat_model::EnemyDummy::open(),
        );
        assert_ne!(
            basic.strike_dps.round(),
            with.strike_dps.round(),
            "Berserker precision must move strike off SimParams::basic (precision=0)"
        );
        assert_eq!(
            tool_strike,
            with.strike_dps.round(),
            "tool must call simulate_with on resolved stats, not simulate()/basic: {result}"
        );
    }

    #[test]
    fn rotation_sim_params_threads_extracted_deferred_target() {
        let mut db = db_with_itemstats(vec![]);
        db.traits.insert(
            9001,
            GW2Trait {
                id: 9001,
                name: "Kent Vs Target".into(),
                icon: None,
                description: None,
                specialization: 0,
                tier: 0,
                order: 0,
                slot: "Minor".into(),
                facts: vec![Fact::Percent {
                    text: Some("Strike Damage vs. Vulnerability".into()),
                    icon: None,
                    percent: Some(10.0),
                }],
                traited_facts: vec![],
                skills: vec![],
            },
        );
        let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let full = stats::base_stats();
        let params = rotation_sim_params(&full, "Guardian", &balance_ctx, &db, &[9001]);
        let extracted = combat::extract_damage_modifiers(
            &[9001],
            None,
            &[],
            None,
            &db.traits,
            &db.items,
            &balance_ctx,
        );
        assert!(
            !params.deferred_target.is_empty(),
            "vs-target fact must reach SimParams.deferred_target"
        );
        assert_eq!(
            params.deferred_target.len(),
            extracted.deferred_target.len(),
            "rotation preview must clone the extracted deferred_target list"
        );
        assert_eq!(
            params.deferred_target[0].percent,
            extracted.deferred_target[0].percent
        );
        assert!(
            !params.weaver,
            "Guardian fixture trait is spec 0, not Weaver"
        );
    }

    #[test]
    fn rotation_sim_params_sets_weaver_from_trait_spec() {
        let mut db = db_with_itemstats(vec![]);
        db.traits.insert(
            2177,
            GW2Trait {
                id: 2177,
                name: "Weaver's Prowess".into(),
                icon: None,
                description: None,
                specialization: crate::rotation::attunement::WEAVER_SPEC_ID,
                tier: 1,
                order: 0,
                slot: "Major".into(),
                facts: vec![],
                traited_facts: vec![],
                skills: vec![],
            },
        );
        let balance_ctx = BalanceContext::new(gw2_core::types::GameMode::PvE);
        let full = stats::base_stats();
        let weaver = rotation_sim_params(&full, "Elementalist", &balance_ctx, &db, &[2177]);
        assert!(weaver.weaver);
        db.traits.insert(
            2178,
            GW2Trait {
                id: 2178,
                name: "Willbender trait".into(),
                icon: None,
                description: None,
                specialization: 65,
                tier: 1,
                order: 0,
                slot: "Major".into(),
                facts: vec![],
                traited_facts: vec![],
                skills: vec![],
            },
        );
        let willbender = rotation_sim_params(&full, "Guardian", &balance_ctx, &db, &[2178]);
        assert!(!willbender.weaver, "spec 65 is Willbender, not Weaver");
        let core = rotation_sim_params(&full, "Elementalist", &balance_ctx, &db, &[]);
        assert!(!core.weaver);
    }

    /// The reference replaces tool rounds, so it has to carry what those
    /// rounds returned - and step aside cleanly when there is nothing to
    /// carry, because with no character selected the tools are the only
    /// source left.
    #[test]
    fn the_profession_reference_carries_the_rounds_it_replaces() {
        let mut db = db_with_itemstats(vec![]);
        assert_eq!(
            profession_reference(&db, "unknown"),
            "",
            "no profession means no reference; the prompt must fall back to the tools"
        );

        db.professions.insert(
            "Necromancer".into(),
            gw2_api::models::Profession {
                id: "Necromancer".into(),
                name: "Necromancer".into(),
                code: None,
                specializations: vec![],
                weapons: HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        for (id, name, slot) in [
            (2, "Well of Blood", "Heal"),
            (1, "Trail of Anguish", "Utility"),
            (3, "Chilled to the Bone!", "Elite"),
        ] {
            let mut skill = make_skill(id, name);
            skill.professions = vec!["Necromancer".into()];
            skill.slot = Some(slot.into());
            db.skills.insert(id, skill);
        }

        let reference = profession_reference(&db, "Necromancer");
        for name in ["Well of Blood", "Trail of Anguish", "Chilled to the Bone!"] {
            assert!(
                reference.contains(name),
                "the skill palette is what stops the model guessing names: {reference}"
            );
        }
        assert!(
            reference.contains("Do NOT call get_profession_info"),
            "without this the model re-fetches what it was just handed: {reference}"
        );
        assert_eq!(
            reference,
            profession_reference(&db, "Necromancer"),
            "HashMap order must not leak into the prompt"
        );
    }
    // score_build full-build mode (specs/004-simulator-trust, FR-018)

    fn reaper_ctx<'a>(
        db: &'a GameDb,
        bal: &'a BalanceContext,
        scenario: &crate::scenario::ScenarioSpec,
        summary: Option<&'a str>,
    ) -> ToolContext<'a> {
        ToolContext {
            db,
            profession_name: "Necromancer",
            candidates: &[],
            current_build_summary: summary,
            weights: OptimizationWeights::default(),
            balance_ctx: bal,
            scenario: scenario.clone(),
        }
    }

    fn reaper_plate() -> Value {
        json!({
            "specializations": [
                {"name": "Spite", "traits": ["Spite Adept Left", "Spite Master Left", "Spite Grandmaster Left"]},
                {"name": "Soul Reaping", "traits": ["Soul Reaping Adept Left", "Soul Reaping Master Left", "Soul Reaping Grandmaster Left"]},
                {"name": "Reaper", "traits": ["Reaper Adept Left", "Reaper Master Left", "Reaper Grandmaster Left"]}
            ],
            "weapons": {"set1": {"main": "Greatsword", "off": null}, "set2": {"main": "Axe", "off": "Focus"}},
            "skills": {
                "heal": "Signet of Vampirism",
                "utilities": ["Well of Suffering", "Well of Darkness", "\"You Are All Weaklings!\""],
                "elite": "\"Chilled to the Bone!\""
            },
            "rune": "Superior Rune of the Scholar",
            "sigils": ["Superior Sigil of Fire", "Superior Sigil of Force"],
            "relic": "Relic of the Thief",
            "stat_prefix": "Marauder",
            "explanation": "fixture plate"
        })
    }

    #[test]
    fn score_build_full_mode_matches_referee() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let (bal, scenario) = fx::scenario();
        let ctx = reaper_ctx(&db, &bal, &scenario, None);
        let plate = reaper_plate();
        let v = execute_tool("score_build", &json!({ "build": plate.clone() }), &ctx);
        assert_eq!(v["mode"], "full_build", "{v}");
        assert!(
            v["errors"].as_array().is_some_and(|e| e.is_empty()),
            "the fixture plate validates: {v}"
        );

        let parsed = crate::prompts::parse_gemini_build(&plate.to_string()).expect("plate parses");
        let validated = crate::validation::validate_gemini_build(&parsed, &db, "Necromancer");
        assert!(validated.errors.is_empty(), "{:?}", validated.errors);
        let report = crate::referee::evaluate_validated_build_with(
            &validated,
            &db,
            "Necromancer",
            &ctx.weights,
            &bal,
            &scenario,
            &[],
        );
        let eq = |name: &str, tool: f64, referee: f64| {
            assert!(
                (tool - referee).abs() <= 1e-9,
                "{name}: tool {tool} vs referee {referee}"
            );
        };
        eq(
            "user_intent_score",
            v["user_intent_score"].as_f64().unwrap(),
            report.user_intent_score,
        );
        eq(
            "power",
            v["realized"]["power"].as_f64().unwrap(),
            report.realized.power,
        );
        eq(
            "condition",
            v["realized"]["condition"].as_f64().unwrap(),
            report.realized.condition,
        );
        eq(
            "boon_support",
            v["realized"]["boon_support"].as_f64().unwrap(),
            report.realized.boon_support,
        );
        eq(
            "healing",
            v["realized"]["healing"].as_f64().unwrap(),
            report.realized.healing,
        );
        eq(
            "sustain",
            v["realized"]["sustain"].as_f64().unwrap(),
            report.realized.sustain,
        );
        eq(
            "control",
            v["realized"]["control"].as_f64().unwrap(),
            report.realized.control,
        );
        assert_eq!(v["viable"].as_bool(), Some(report.viability.is_viable));
        assert_eq!(
            v["quality"].as_str(),
            Some(format!("{:?}", report.quality).as_str())
        );
        assert_eq!(
            v["gates"].as_array().map(|g| g.len()),
            Some(report.viability.gates.len())
        );
        // The on-crit sigil executes (Sprint 2) and is no longer in Choya's
        // coverage note; the sources without a record still are.
        assert!(
            v["coverage_note"]
                .as_str()
                .is_some_and(|s| !s.contains("Sigil of Fire") && s.contains("(no record)")),
            "coverage_note: {}",
            v["coverage_note"]
        );
    }

    #[test]
    fn score_build_without_build_or_prefix_errors() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let (bal, scenario) = fx::scenario();
        let ctx = reaper_ctx(&db, &bal, &scenario, None);
        let v = execute_tool("score_build", &json!({}), &ctx);
        assert!(
            v["error"]
                .as_str()
                .is_some_and(|e| e.contains("no build supplied")),
            "{v}"
        );
        assert!(v.get("viable").is_none() && v.get("score").is_none());
        let empty = execute_tool("score_build", &json!({ "gear_prefix": "  " }), &ctx);
        assert!(
            empty["error"]
                .as_str()
                .is_some_and(|e| e.contains("no build supplied")),
            "{empty}"
        );
    }

    #[test]
    fn score_build_never_substitutes_equipped_build() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let (bal, scenario) = fx::scenario();
        let kitchen = "EQUIPPED: Reaper greatsword, Marauder, Scholar";
        let ctx = reaper_ctx(&db, &bal, &scenario, Some(kitchen));
        let v = execute_tool("score_build", &json!({}), &ctx);
        assert!(
            v.get("error").is_some(),
            "an omitted build is an error, never the equipped loadout: {v}"
        );
        assert!(v.get("viable").is_none() && v.get("user_intent_score").is_none());
    }

    #[test]
    fn score_build_prefix_only_mode_is_labelled() {
        use crate::rotation::reaper_fixture as fx;
        let db = fx::db();
        let (bal, scenario) = fx::scenario();
        let ctx = reaper_ctx(&db, &bal, &scenario, None);
        let v = execute_tool("score_build", &json!({ "gear_prefix": "Marauder" }), &ctx);
        assert_eq!(v["mode"], "prefix_only", "{v}");
        assert!(v["scope"]
            .as_str()
            .is_some_and(|s| s.contains("prefix only")));
    }

    fn stub_profession(name: &str) -> gw2_api::models::Profession {
        gw2_api::models::Profession {
            id: name.into(),
            name: name.into(),
            code: None,
            specializations: vec![],
            weapons: HashMap::new(),
            training: vec![],
            skills_by_palette: vec![],
            icon: None,
            icon_big: None,
        }
    }

    fn profession_info_for(name: &str) -> Value {
        let mut db = GameDb::empty_for_tests();
        db.professions.insert(name.into(), stub_profession(name));
        let bal = BalanceContext::pve();
        let ctx = ToolContext {
            db: &db,
            profession_name: name,
            candidates: &[],
            current_build_summary: None,
            weights: OptimizationWeights::default(),
            balance_ctx: &bal,
            scenario: crate::scenario::ScenarioSpec::from_balance_context(&bal),
        };
        execute_tool("get_profession_info", &json!({ "profession": name }), &ctx)
    }

    #[test]
    fn get_profession_info_guardian_sword_per_hand() {
        let v = profession_info_for("Guardian");
        let sword = v["weapons"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["name"] == "Sword")
            .expect("Sword row");
        assert_eq!(sword["main"], "core", "{sword}");
        assert_eq!(sword["off"], "Willbender", "{sword}");
        assert!(sword["two_hand"].is_null(), "{sword}");
        assert!(sword.get("flags").is_none(), "{sword}");
        assert!(sword.get("requires_elite").is_none(), "{sword}");
        assert!(
            v["note"]
                .as_str()
                .is_some_and(|n| n.contains("Weaponmaster Training")),
            "{v}"
        );
    }
}
