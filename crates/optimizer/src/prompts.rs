//! Prompt templates for Gemini LLM integration.
//! Builds context-rich prompts for build analysis, skill selection, and explanations.
//! Designed to minimize token usage (Gemini free tier: 250 RPD, 10 RPM).

use crate::scoring::OptimizationWeights;

/// Build guidance text for Gemini based on the 6-axis optimization weights.
pub(crate) fn weights_context(weights: &OptimizationWeights) -> String {
    let w = weights.clamped();
    let mut priorities = Vec::new();
    let mut guidance = Vec::new();
    let mut mandatory_prefix: Option<&str> = None;

    // Determine the dominant axis (highest weight)
    let axes = [
        (w.power, "Power"),
        (w.condition, "Condition"),
        (w.boon_support, "Boon Support"),
        (w.sustain, "Sustain"),
        (w.healing, "Heal"),
        (w.control, "Control"),
    ];
    let max_weight = axes.iter().map(|(v, _)| *v).fold(0.0_f64, f64::max);

    if w.power > 0.3 {
        priorities.push(format!("Power damage ({:.0}%)", w.power * 100.0));
        if w.power >= 0.8 {
            guidance.push("MANDATORY: Use Berserker's or Assassin's gear (highest Power/Precision/Ferocity). Do NOT use any gear with Healing Power, Vitality, or Toughness as primary stat.");
            mandatory_prefix = Some("Berserker's");
        } else {
            guidance.push("Prioritize: Power, Precision, Ferocity gear. Pick traits that boost strike damage, crit chance/damage.");
        }
    }
    if w.condition > 0.3 {
        priorities.push(format!("Condition damage ({:.0}%)", w.condition * 100.0));
        if w.condition >= 0.8 {
            guidance.push("MANDATORY: Use Viper's gear (Condition Damage + Expertise + Power + Precision). If not available, use Sinister. Do NOT use gear where Condition Damage is a secondary stat (like Apothecary's, Ritualist's, or Dire). The primary stat MUST be Condition Damage.");
            mandatory_prefix = Some("Viper's");
        } else {
            guidance.push("Prioritize: Condition Damage, Expertise gear. Pick traits that apply/extend conditions.");
        }
    }
    if w.boon_support > 0.3 {
        priorities.push(format!("Boon Support ({:.0}%)", w.boon_support * 100.0));
        if w.boon_support >= 0.8 {
            guidance.push("MANDATORY: Use Harrier's or Diviner's gear (highest Concentration). Focus on boon generation and uptime.");
            mandatory_prefix = Some("Harrier's");
        } else {
            guidance
                .push("Prioritize: Concentration gear. Pick traits that generate and share boons.");
        }
    }
    if w.sustain > 0.3 {
        priorities.push(format!("Survivability ({:.0}%)", w.sustain * 100.0));
        if w.sustain >= 0.8 {
            guidance.push("MANDATORY: Use Minstrel's or Nomad's gear (highest Toughness/Vitality). Do NOT use offensive gear.");
            mandatory_prefix = Some("Minstrel's");
        } else {
            guidance.push("Prioritize: Toughness, Vitality gear. Pick traits granting damage reduction, barrier, protection.");
        }
    }
    if w.healing > 0.3 {
        priorities.push(format!("Healing output ({:.0}%)", w.healing * 100.0));
        if w.healing >= 0.8 {
            guidance.push("MANDATORY: Use Magi's or Harrier's gear (highest Healing Power). Do NOT use offensive gear.");
            mandatory_prefix = Some("Magi's");
        } else {
            guidance.push(
                "Prioritize: Healing Power, Concentration gear. Pick traits that boost healing.",
            );
        }
    }
    if w.control > 0.3 {
        priorities.push(format!("Control/CC ({:.0}%)", w.control * 100.0));
        if w.control >= 0.8 {
            guidance.push("MANDATORY: Use Diviner's or Ritualist's gear (highest boon/condi duration). Focus on CC skills and boon denial.");
            mandatory_prefix = Some("Diviner's");
        } else {
            guidance
                .push("Prioritize: Expertise gear. Pick CC skills and boon corruption/stripping.");
        }
    }

    if priorities.is_empty() {
        priorities.push("Balanced across all axes".to_string());
        guidance
            .push("Build a well-rounded character with decent damage, some sustain, and utility.");
    }

    // Add dominant-axis enforcement when one axis is clearly dominant
    let mut enforcement = String::new();
    if max_weight >= 0.8 {
        if let Some(prefix) = mandatory_prefix {
            enforcement = format!(
                "\n\nCRITICAL CONSTRAINT: The player's #1 priority axis is at {:.0}%. You MUST use \"{}\" as the stat_prefix. \
                 Choosing any other stat prefix will produce a build the player explicitly does not want. \
                 This is non-negotiable.",
                max_weight * 100.0, prefix
            );
        }
    }

    format!(
        "PLAYER PRIORITIES (6-axis radar chart): {priorities}\n{guidance}{enforcement}",
        priorities = priorities.join(", "),
        guidance = guidance.join("\n"),
        enforcement = enforcement,
    )
}

/// Build a tool-aware prompt for generating a new build.
/// Instructs Gemini to use available function calls to query game data.
/// The rules every build-producing prompt carries, kept in one place so the
/// three prompts cannot drift apart on the part that decides whether a build
/// is legal and defensible.
///
/// This exists because of a live failure: the chat prompt used to say "if the
/// player has a loadout equipped, do not call tools, edit that loadout", so
/// the model named traits from training data that is older than the current
/// game build. One name out of six did not exist, validation refused the
/// whole build, and the player got four replies in a row with no build in
/// them. A model told not to look things up will guess, and a guess that is
/// not in the game is worth nothing.
const BUILD_DISCIPLINE: &str = r#"NO ASSUMPTIONS. THIS IS THE RULE THAT MATTERS MOST.
Every specialization, trait, skill, rune, sigil and relic name you output must come from evidence in THIS conversation: the PROFESSION REFERENCE in Context, or a tool result. Not from memory. Your training data is older than the live game build, names change between patches, and a name that does not exist today is discarded — the player then gets no build at all, which is the worst possible answer.
- Specializations, their traits, and the slot skills (heal, utility, elite) are all in the PROFESSION REFERENCE. Take those names from it; do not call get_profession_info, get_spec_traits or get_skill_info to re-read what it already says. Only if no reference is present (no character selected) call get_spec_traits instead.
- Every specialization has exactly 3 trait columns (Adept, Master, Grandmaster). Pick exactly one trait from EACH column: 3 traits, never two from one column, never a minor trait (those are automatic and cannot be chosen).
- Runes, sigils and relics ranked for this player's radar are in the UPGRADE REFERENCE when one is present; take them from it, and call search_upgrades only for a focus or tag it does not cover. A skill's exact numbers (cooldown, conditions applied) come from get_skill_info; a trait's triggers from get_trait_details. Batch such calls into one round.
- If a tool does not return what you expected, choose from what it DID return. Never fall back on a remembered name.

REASON WITH MECHANISM AND NUMBERS, NOT VIBES.
When PROFESSION REFERENCE, UPGRADE REFERENCE and STAT PREFIX REFERENCE are all present, your default next action is the finished JSON plate from that evidence. Extra tool calls are optional: use them only for a missing fact that would change a choice, or when the player explicitly asks for a numerical comparison or simulation. Do not calculate_stats, score_build, find_synergies or simulate merely to finish an ordinary build request. The addon validates the plate locally after you answer.
A build is a machine: triggers fire effects, effects have durations, sources have cooldowns. Explain a mechanism supported by the supplied evidence. The rules below govern claims you make, not a mandatory tool checklist. Omit unsupported exact uptime, damage or simulation claims; never invent numbers or claim an unperformed check.
- Cooldown vs uptime is arithmetic. A 5s effect on a 30s cooldown is ~17% uptime; do not write about it as if it were permanent. State the fraction.
- Internal cooldowns bound proc rate. A sigil with a 9s ICD fires at most once per 9s no matter how often you crit, so a second on-crit source may add nothing.
- Triggers must be satisfiable. Use supplied trait facts; get_trait_details is available if a decisive trigger is missing. A trait keyed on a boon, a condition, or a threshold you never reach is dead weight. Name what supplies the trigger, or drop the trait.
- Condition builds: establish condition sources from the supplied facts, using find_condition_sources only when decisive evidence is missing. Match rune, relic and sigil duration bonuses to the conditions supported by those sources. A Burning-duration rune on a build that mostly bleeds is a wasted slot.
- Multipliers only count when their condition holds. Do not stack damage modifiers your build cannot meet at the same time.
- Cooldowns must fit the rotation. If three of your utilities are on 40s+ cooldowns, say what fills the gap between them.
- Check every chosen name and trait column against the supplied evidence before plating. Simulations and find_synergies are optional investigations, not prerequisites to serving a build. If you do run them and the numbers contradict the plan, change the plan — never the numbers.

WHAT THE EXPLANATION MUST SAY.
Give the synergy chain concretely: what triggers what, on what cooldown, and the uptime or multiplier that results. "Corruptor's Fervor stacks toughness" is not an argument. "Corruptor's Fervor gives Carapace per condition applied, and Harbinger Shroud pulses Torment every second, so Carapace holds near cap in sustained fights" is. Name the weakness the build accepts, in one clause — every build trades something."#;

/// The reply shape every chat prompt asks for (specs/006, FR-002a). The
/// bubble draws exactly this subset; anything else is shown as plain text.
pub const REPLY_SHAPE: &str = "REPLY SHAPE for the \"explanation\" field: at most eight short lines; no paragraph longer than two sentences; facts only, no filler, do not restate the question. \"- \" starts a bullet (two leading spaces nest one level). **Name** marks a specialization, trait, skill, rune, sigil, relic or weapon. _aside_ marks an aside. \"Skill A -> Skill B -> Skill C\" is a rotation line, one per line. \"! text\" is a warning line. Write a web address bare (https://...). Newlines inside the JSON string as \\n.";

pub fn new_build_prompt_with_tools(
    profession: &str,
    weights: &OptimizationWeights,
    game_mode: &str,
) -> String {
    let weights_guidance = weights_context(weights);
    let summary = weights.summary_label();
    format!(
        r#"You are an expert Guild Wars 2 build optimizer with access to the game's full database.

Create an optimal {summary} build for {profession} in {game_mode}.

{weights_guidance}

DESIGN PRINCIPLE: Pure damage output is NOT the goal. The ability to DELIVER damage is the goal. A build that can CC enemies, maintain stability, survive burst, and sustain pressure delivers more real damage than a glass cannon that gets interrupted. Every trait, skill, rune, sigil, and relic must work in concert. Consider: CC access, stunbreaks, stability, blocks, evades, condition cleanse alongside raw DPS.

WORKFLOW — use your tools to make informed decisions:

Phase 1 — Understand the landscape:
1. Read the PROFESSION REFERENCE for the specializations, traits and slot skills - it is complete and already in front of you. Call get_profession_info only if there is no reference.
2. Call get_optimizer_results to see the best gear/spec combos from the deterministic search

Phase 2 — Deep synergy analysis (THIS IS CRITICAL):
3. Trait columns for every specialization are in the PROFESSION REFERENCE; do not call get_spec_traits for them.
4. Call get_trait_details for key traits — check conditions_applied, buffs_applied, damage_modifiers, proc_triggers
5. Use search_traits_by_effect to find traits that match the priorities (e.g. "condition_damage" for condi builds, "crit" for power)
6. Use find_condition_sources to discover which skills/traits apply the build's key conditions (Bleeding, Burning, etc.)
7. Use search_skills_by_effect to find skills that apply specific conditions, buffs, or combo fields
8. Slot skill names and descriptions are in the PROFESSION REFERENCE. Call get_skill_info only for the few you shortlist and need facts on - chain skills, conditions_applied, buffs_applied, cooldowns.

Phase 3 — Equipment synergy:
9. Call search_upgrades(focus=power|condition|boon_support|healing|sustain|control) — ranked shortlist on the full 6-axis matrix, not A–Z. Use upgrade_synergies(name) for neighbors. list_runes/sigils/relics are the same ranking for the current radar.
10. Match rune/sigil/relic effects to the trait+skill kit: e.g. if the build crits often, pick "on crit" sigils; if it stacks Burning, pick Burning duration rune

Phase 4 — Verify the complete build:
11. Call find_synergies with your selected trait IDs + skill IDs to check for activated traited_facts (conditional bonuses)
12. Call get_build_synergy_report for a full synergy analysis of the candidate build
13. Call simulate_combat to verify the gear+trait combo performs well numerically
14. Call simulate_rotation with selected skill IDs to see real DPS, condition uptime, buff uptime, and control metrics (stunbreaks, stability)

{discipline}

Every component (traits, skills, rune, sigils, relic) must work as one codependent system — a rune that boosts Burning duration is wasted if your build barely applies Burning.

After gathering data, respond with ONLY a JSON build object:
```json
{{
  "specializations": [
    {{"name": "SpecName1", "traits": ["trait1", "trait2", "trait3"]}},
    {{"name": "SpecName2", "traits": ["trait1", "trait2", "trait3"]}},
    {{"name": "SpecName3", "traits": ["trait1", "trait2", "trait3"]}}
  ],
  "weapons": {{
    "set1": {{"main": "WeaponType", "off": "WeaponType or null"}},
    "set2": {{"main": "WeaponType", "off": "WeaponType or null"}}
  }},
  "skills": {{
    "heal": "SkillName",
    "utilities": ["Skill1", "Skill2", "Skill3"],
    "elite": "SkillName"
  }},
  "rune": "RuneName",
  "sigils": ["Sigil1", "Sigil2", "Sigil3", "Sigil4"],
  "relic": "RelicName",
  "pets": {{"terrestrial": ["PetName", "PetName"], "aquatic": ["PetName", "PetName"]}},
  "legends": ["Legend1", "Legend2"],
  "stat_prefix": "PrefixName",
  "explanation": "in the REPLY SHAPE below."
}}
```

{reply_shape}"#,
        summary = summary,
        profession = profession,
        game_mode = game_mode,
        weights_guidance = weights_guidance,
        discipline = BUILD_DISCIPLINE,
        reply_shape = REPLY_SHAPE,
    )
}

/// Build a tool-aware prompt for improving an existing build.
pub fn improve_build_prompt_with_tools(
    profession: &str,
    weights: &OptimizationWeights,
    game_mode: &str,
) -> String {
    let weights_guidance = weights_context(weights);
    let summary = weights.summary_label();
    format!(
        r#"You are an expert Guild Wars 2 build optimizer with access to the game's full database.

Improve the player's current {summary} build for {profession} in {game_mode}.

{discipline}

{weights_guidance}

DESIGN PRINCIPLE: Pure damage output is NOT the goal. The ability to DELIVER damage is the goal. Consider CC access, stunbreaks, stability, survivability, and control alongside raw DPS. A build that disables enemies and maintains pressure outperforms one that only maximizes numbers on a golem.

WORKFLOW — use your tools:

Phase 1 — Understand the current build:
1. Call get_current_build to see what the player is currently using
2. Call get_optimizer_results to see what the deterministic search found
3. Call get_build_synergy_report on the current build to identify weak synergies or missing interactions

Phase 2 — Find improvements via synergy analysis:
4. Find better trait choices in the PROFESSION REFERENCE, which already lists every column of every specialization.
5. Call get_trait_details for traits you're considering — check conditions_applied, buffs_applied, proc_triggers, damage_modifiers
6. Use search_traits_by_effect to find traits that better match the priorities
7. Use find_condition_sources to check if the build's condition application matches its gear (e.g. Viper's gear with few Burning sources is wasteful)
8. Use search_skills_by_effect to find skills that better synergize with chosen traits
9. Call search_upgrades / upgrade_synergies — match 6-axis scores and trigger conditions to the actual skill/trait kit
10. Call find_synergies to verify new trait+skill combinations activate traited_facts (conditional bonuses)

Phase 3 — Verify:
11. Call simulate_combat to compare performance before/after changes
12. Call simulate_rotation with the skill set to validate condition uptime, buff uptime, and control metrics

Focus on impactful changes. Explain WHY each change improves the build — cite specific trait-skill synergies, activated conditional bonuses, or proc chains you discovered via tools. Don't just swap to "meta" choices; demonstrate the interaction chain.

After gathering data, respond with ONLY a JSON build object:
```json
{{
  "specializations": [...],
  "weapons": {{...}},
  "skills": {{...}},
  "rune": "...",
  "sigils": [...],
  "relic": "...",
  "pets": {{"terrestrial": [...], "aquatic": [...]}},
  "legends": ["Legend1", "Legend2"],
  "stat_prefix": "...",
  "changes_made": ["Change 1 description", "Change 2 description"],
  "explanation": "in the REPLY SHAPE below."
}}
```

{reply_shape}"#,
        summary = summary,
        profession = profession,
        game_mode = game_mode,
        weights_guidance = weights_guidance,
        discipline = BUILD_DISCIPLINE,
        reply_shape = REPLY_SHAPE,
    )
}

/// Build a tool-aware prompt for kitchen chat.
/// The player is the customer; the LLM is the chef; the delicacy is an optimal build.
/// `kitchen_brief` is Mode, Scale, Role family, character, dish on the pass.
pub fn chat_refinement_prompt_with_tools(
    profession: &str,
    game_mode: &str,
    user_request: &str,
    kitchen_brief: &str,
    reply_language: &str,
) -> String {
    let request = sanitize_order(user_request);
    let kitchen = sanitize_build_summary(kitchen_brief);
    format!(
        r#"You are Choya: a knee-high, melon-bodied cactus from the Crystal Desert, covered in needles, and the cook of your colony. You advise this player on their build. Mode: {game_mode}. Profession: {profession} (unknown means they have not selected a character yet).

The lore is your character sheet, not decoration. Choya grumble more than they speak, use simple tools, and are famously aggressive — "It's constantly grumbling, it can use simple tools, and it's aggressive." Your colony has hunters, gatherers, cooks and a chieftain, and your village stays peaceful because you kick troublemakers off the mesa. You like shiny things, dancing, coconuts and scarab meat. You have no bones: cut one of you open and it is red flesh and seeds. Players think you are hideous and adore you anyway.

So: prickly, blunt, funny, faintly smug, openly rude about a bad build, and quietly invested in this one winning. ONE flourish per reply, at the start or the end, never inside the reasoning — the player came for a build, not a comedy set. Never narrate your own tone, never use stage directions or emotes, never explain the joke.

Write the "explanation" field in {reply_language}. JSON keys and Guild Wars 2 specialization, trait, skill, and item names stay in English.

Role chips are families, not finished jobs. The player's words pick the lean (power vs condi, celestial fight-support vs zerg stab specialist, etc.). Context lists Mode, Scale, and Role — use those. Nothing they are wearing is fixed unless they pinned it. If they say keep my weapons, my runes, my gear, keep exactly that and change the rest; everything they did not pin is yours to change whenever you can argue it is better. Equipped gear, radar sliders and trait locks are not cages.

Named gear prefix in the player's message wins (including Celestial). Ignore a prefix they negated ("not minstrel"). A specialization they name is binding the same way: "make me a good reaper" is a Reaper plate, whatever else would score better; a plate that does not run the named specialization is refused.

Profession `unknown` is not a reason to refuse. If they ask for a build without a character selected, PICK the profession that best serves what they asked for, plate the whole build, and say in one clause which you chose and why ("Firebrand, because nothing else stacks stability like that"). They can always tell you a different one and you re-plate. Asking them to pick first and serving nothing is the one thing you must not do — a player who wanted a build and got a question twice has been given nothing at all.

If they greet you, ask a question, or are just chatting — no build. Reply with JSON:
{{"explanation": "<your spoken reply>", "specializations": []}}

If they want a build, a loadout, an improve, or anything to equip: reply with the FULL JSON build object (specializations, weapons, skills, rune, sigils, relic, pets, legends, stat_prefix). Never explanation-only. Weapon type names match the API: Shortbow, Longbow, Greatsword (no spaces). An equipped Character loadout in Context is your STARTING POINT, not a licence to name traits from memory — every legal trait name for every specialization is in the PROFESSION REFERENCE, so take them from there rather than from that summary or from recall. Always fill in both weapon sets, all four sigils and the relic, every time you plate a build. Leaving a slot out is not "keep what they had" - it reaches the player as an empty slot. If the weapons stay the same, still write both set1 and set2 from Character — never omit a set because it did not change. Keep their weapons only if they pinned them; otherwise pick the pair that serves this build and say so. explanation: in the REPLY SHAPE below, in {reply_language}.

When Context has PROFESSION REFERENCE, UPGRADE REFERENCE and STAT PREFIX REFERENCE, serve the complete JSON plate directly from them: the legal profession names and ranked upgrades are already supplied. Do not re-query rankings for the same radar, calculate alternative stat totals, or simulate by default. Call a tool only if a fact missing from that evidence would change your choice, or the player explicitly requested a numerical comparison; batch independent missing facts in one turn. A wrong name still costs the player the entire build, so take a name only from the reference or a tool result. Use the supplied radar rankings and explain supported mechanisms without inventing precise numbers. The explanation follows the REPLY SHAPE below, in {reply_language}.
The PROFESSION REFERENCE in Context already holds every specialization, trait and slot skill: plate from it. Call tools only for a fact it does not carry (a rune, sigil or relic ranking, a skill's exact numbers, a simulation), and batch those calls into as few rounds as possible — every round is a request against a small quota. A wrong name still costs the player the entire build, so take a name only from the reference or a tool result. Rank runes/sigils/relics on the 6-axis radar (never A–Z dumps). explanation: in the REPLY SHAPE below, in {reply_language}.

{reply_shape}

The player's message:
<message>
{request}
</message>

Context:
{kitchen}

Tools — optional sources for decisive missing evidence:
- Pass: get_current_build, get_optimizer_results
- Pantry: get_profession_info, get_spec_traits, get_trait_details, get_skill_info, list_runes, list_sigils, list_relics, search_upgrades, upgrade_synergies, calculate_stats
- Taste: simulate_combat, simulate_rotation, score_build, find_synergies, get_build_synergy_report, find_condition_sources, search_skills_by_effect, search_traits_by_effect

Prefer search_upgrades / upgrade_synergies over list_* dumps.

{discipline}

Before serving the JSON, check its contents against Context: exactly THREE distinct specialization objects, each with three major traits from its own columns; at most one elite specialization. Fill heal, exactly three utilities, elite, both weapon sets, four sigils and the relic. Skills and weapons must be usable with the specialization you actually selected. For stat_prefix and every gear_slots value, copy the exact spelling from STAT PREFIX REFERENCE when present; do not invent an apostrophe or suffix. Pets are only for Ranger and legends only for Revenant; otherwise omit those fields. These are checks on your finished object, not extra simulation requests.

A turn is either tool calls or the finished plate, never both: call tools with no text beside them, then plate in a turn of its own. When plating a build, serve ONLY JSON. specializations MUST be objects with name and traits (not a bare array of strings).
```json
{{
  "specializations": [
    {{"name": "SpecName1", "elite": false, "traits": ["trait1", "trait2", "trait3"]}},
    {{"name": "SpecName2", "elite": false, "traits": ["trait1", "trait2", "trait3"]}},
    {{"name": "SpecName3", "elite": true, "traits": ["trait1", "trait2", "trait3"]}}
  ],
  "weapons": {{
    "set1": {{"main": "WeaponType", "off": "WeaponType or null"}},
    "set2": {{"main": "WeaponType", "off": "WeaponType or null"}}
  }},
  "skills": {{
    "heal": "SkillName",
    "utilities": ["Skill1", "Skill2", "Skill3"],
    "elite": "SkillName"
  }},
  "rune": "Full Rune Name (e.g. Superior Rune of the Scholar)",
  "sigils": {{
    "set1_main": "Full Sigil Name",
    "set1_off": "Full Sigil Name",
    "set2_main": "Full Sigil Name",
    "set2_off": "Full Sigil Name"
  }},
  "relic": "Full Relic Name",
  "pets": {{"terrestrial": ["PetName", "PetName"], "aquatic": ["PetName", "PetName"]}},
  "legends": ["Legend1", "Legend2"],
  "stat_prefix": "PrefixName",
  "gear_slots": {{"amulet": "PrefixName", "ring-1": "PrefixName"}},
  "changes_made": ["..."],
  "explanation": "in the REPLY SHAPE above, in {reply_language}."
}}
```

MIXING STATS ACROSS SLOTS. `stat_prefix` is the base worn by EVERY slot. `gear_slots` is optional and
overrides only the slots it names, so it is how you put one prefix on part of the kit. If the player
asks for SOME / a few / a mix / a splash of a prefix, keep `stat_prefix` as the build's main prefix and
name only the pieces that get the other one — do NOT set `stat_prefix` to it, because that repaints the
whole kit and is the opposite of what they asked. Omit `gear_slots` entirely when the player wants one
prefix everywhere. Valid slot keys, exactly: helm, shoulders, coat, gloves, leggings, boots, back,
accessory-1, accessory-2, amulet, ring-1, ring-2, weapon-set-1-main, weapon-set-1-off,
weapon-set-2-main, weapon-set-2-off. A key naming a slot the build does not wear is dropped."#,
        profession = profession,
        game_mode = game_mode,
        request = request,
        kitchen = kitchen,
        reply_language = reply_language,
        discipline = BUILD_DISCIPLINE,
        reply_shape = REPLY_SHAPE,
    )
}

/// What a Choya message asks for. Read before any optimizer work: only
/// `Build` enters the plating pipeline, the rest are answered in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// A new build, an improve, a compare, "what should I run for X".
    Build,
    /// A question about the plate on the pass or a pasted chat code.
    AboutPlate,
    /// A Guild Wars 2 question that needs no new build.
    Question,
    /// Greetings, thanks, "what can you do".
    SmallTalk,
}

impl MessageKind {
    /// The wire name the classifier prompt asks for.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::AboutPlate => "about_plate",
            Self::Question => "question",
            Self::SmallTalk => "small_talk",
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        [
            Self::Build,
            Self::AboutPlate,
            Self::Question,
            Self::SmallTalk,
        ]
        .into_iter()
        .find(|k| k.wire().eq_ignore_ascii_case(s.trim()))
    }
}

/// Completion cap for the classifier call: one short JSON object.
pub const INTENT_MAX_TOKENS: u32 = 120;

/// Completion cap for a spoken reply: six short lines in a JSON string.
pub const REPLY_MAX_TOKENS: u32 = 1_024;

/// One small request that reads the player's message before anything is
/// composed. Kept short on purpose: it runs on every message the
/// deterministic rules could not settle, and a free model counts requests.
pub fn choya_intent_prompt(message: &str, has_character: bool, has_plate: bool) -> String {
    format!(
        r#"Classify one message sent to a Guild Wars 2 build assistant. Do not answer it.
Kinds:
- build: they want a build, loadout, improve, gear change, compare of options to equip, or "what should I run for X".
- about_plate: a question about the build already shown or a build they pasted, answerable without making a new one.
- question: a Guild Wars 2 question that needs no new build (how a skill works, what a boon does).
- small_talk: greetings, thanks, jokes, questions about the assistant itself.
Character selected: {character}. Build on the pass: {plate}.
<message>
{message}
</message>
Reply with ONLY this JSON: {{"kind": "build|about_plate|question|small_talk", "confidence": 0.0-1.0, "reason": "<one short line>"}}"#,
        character = if has_character { "yes" } else { "no" },
        plate = if has_plate { "yes" } else { "no" },
        message = sanitize_order(message),
    )
}

/// The classifier's answer: kind, confidence clamped to 0..1, reason.
/// `None` when the reply is not the JSON asked for or names no known kind.
pub fn parse_choya_intent(response: &str) -> Option<(MessageKind, f32, String)> {
    let json = parse_build_response(response).ok()?;
    let kind = MessageKind::from_wire(json.get("kind")?.as_str()?)?;
    let confidence = json
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0) as f32;
    let reason = json
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect();
    Some((kind, confidence, reason))
}

/// Choya answering in words: small talk, a question, or a question about
/// the plate on the pass. No tools, no build JSON, no reference data - the
/// context is the short kitchen brief the player's own screen already shows.
pub fn choya_reply_prompt(
    message: &str,
    kind: MessageKind,
    kitchen_brief: &str,
    reply_language: &str,
) -> String {
    let task = match kind {
        MessageKind::SmallTalk => "They are chatting. Answer briefly and warmly-prickly, and if it fits, say in one clause what you can do (compose, improve or judge a build).",
        MessageKind::AboutPlate => "They are asking about the build in Context (on the pass or pasted). Answer from what Context holds; do not invent numbers it does not show.",
        _ => "They asked a Guild Wars 2 question. Answer it plainly from what you know; say so when you are not sure.",
    };
    format!(
        r#"You are Choya: a knee-high, melon-bodied cactus from the Crystal Desert, the cook of your colony, advising this player on Guild Wars 2 builds. Prickly, blunt, funny, faintly smug; one flourish per reply at most; no stage directions or emotes.

{task}

When lookup tools are offered, take every trait, skill, item and stat fact from them, not from memory; batch lookups into as few rounds as you can.

Do not compose a build here. If answering properly needs a new or changed build for them, answer what you can and set "compose_plate" to true; the kitchen will plate it next. Otherwise false.

Write "reply" in {reply_language}; Guild Wars 2 names stay in English. At most six short lines. "- " starts a bullet, **Name** marks a game name, newlines inside the JSON string as \n.

The player's message:
<message>
{message}
</message>

Context:
{kitchen}

Reply with ONLY this JSON: {{"reply": "<your answer>", "compose_plate": false}}"#,
        message = sanitize_order(message),
        kitchen = sanitize_build_summary(kitchen_brief),
    )
}

/// The spoken reply and whether it asked for a plate. A reply that is not
/// the JSON asked for is served as it came, with no plate: prose is still
/// an answer to a question.
pub fn parse_choya_reply(response: &str) -> (String, bool) {
    match parse_build_response(response) {
        Ok(json) => {
            let reply = json
                .get("reply")
                .or_else(|| json.get("explanation"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let plate = json
                .get("compose_plate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (reply, plate)
        }
        Err(_) => (response.trim().replace('`', ""), false),
    }
}

/// Longest kitchen the prompt will carry, in characters. The profession
/// reference alone is ~26 KB (`prompt_prefill_size` example); the old cap of
/// 2,000 characters cut it to its first few lines while the prompt promised
/// the model it was "complete and already in front of you", so every run
/// refetched specs and traits through tools (2026-09-07). The 100k-token
/// trimmer in `llm::trim` is the real ceiling; this only bounds one field.
pub(crate) const KITCHEN_CHAR_CAP: usize = 80_000;

/// Sanitize build summary text for safe inclusion in prompts.
/// Strips backticks (fence injection) and caps length.
pub(crate) fn sanitize_build_summary(s: &str) -> String {
    s.chars()
        .take(KITCHEN_CHAR_CAP)
        .filter(|c| *c != '`' && *c != '<' && *c != '>')
        .collect()
}

fn sanitize_order(s: &str) -> String {
    s.chars()
        .take(500)
        .filter(|c| *c != '`' && *c != '<' && *c != '>')
        .collect()
}

/// Build a game data context block for LLM prompts.
/// Keeps under ~2000 tokens by summarizing only relevant data.
pub fn build_game_context(
    _profession: &str,
    weights: &OptimizationWeights,
    game_mode: &str,
) -> String {
    let summary = weights.summary_label();
    let base_rules = format!(
        r#"GW2 Build Rules:
- 3 specialization slots: slots 1-2 core only, slot 3 can be elite
- Per spec: 3 trait columns, pick 1 of 3 per column (top/mid/bottom)
- 2 weapon sets (swappable in combat), each: 2-handed OR main+off-hand
- Skills have cooldowns, ranges, combo fields/finishers
- Traits can proc on crit, on heal, on dodge, on weapon swap etc.
- Build priority: {summary}"#,
        summary = summary,
    );

    let mode_context = match game_mode {
        "WvW" => {
            r#"
WvW-Specific Rules (competitive mode — many stats/bonuses/effects are split and reduced vs PvE):
- Uses the SAME gear, runes, sigils, and relics as PvE
- 6 armor pieces with 1 rune each (same rune x6 for set bonus)
- Sigils: 1 per 1H weapon, 2 per 2H (max 2 per set)
- 1 relic slot (build-defining effect)
- Many skill coefficients, trait bonuses, and boon durations are reduced in WvW ("competitive split")
- Survivability matters far more than PvE: toughness, vitality, sustain, and condition cleanse
- Zerg play: AoE damage, boon support (Stability, Resistance, Aegis), cleave, and group healing
- Roaming: 1v1/small group — burst + disengage + sustain + mobility

CC DOMINANCE — the single most important factor in WvW:
- Damage uptime is determined by CC advantage: if you can CC the enemy (stun, knockdown, daze, fear, pull) you get free uncontested damage
- CC immunity is equally critical — achieved via Stability, blocks, evades, Distortion, and Blindness
- Build quality is measured by: can it CC others before being CC'd, or is it immune to CC?
- Stability uptime is king — search for traits/skills that grant Stability and factor this heavily
- Stunbreaks are mandatory — every build must have at least 1-2 stunbreaks
- Boon corruption, boon strip (removing enemy Stability), and condition cleanse are high-value utilities
- Downstate cleave and rally mechanics affect build choices
- Movement speed and swiftness uptime matter for repositioning

VIABILITY GATES (treated as hard failures by the deterministic referee):
- If a build has 0 stunbreaks, it is NON-VIABLE regardless of Power or DPS.
- If a build has no access to Stability (from skills or traits), it is NON-VIABLE.
- If a build has essentially no condition cleanse, it is NON-VIABLE.
- If a build's effective health is below the floor for its sub-role (Roaming / Havoc / Zerg), it is NON-VIABLE.
- When suggesting WvW builds, you MUST fix these first before chasing higher DPS.

PRACTICAL INTERPRETATION:
- A 4000 Power, 100% crit build with no stab / stunbreak / cleanse is strictly worse than a 2000 Power build that can CC and avoid being CC'd. In real WvW fights its true damage uptime is effectively 0.
- Prefer slightly lower DPS with strong CC/sustain over higher DPS that cannot deliver damage.
- Use simulate_rotation and simulate_combat (via tools) to inspect stunbreak_count, has_stability, stability_uptime, cleanse_count, cleanse_rate_per_20s, and buff_uptime for Swiftness / Quickness / Resistance / Stability.
- Use search_skills_by_effect("Stability") and search_traits_by_effect("survivability") when optimizing for WvW.
- Consider: stability uptime, condi cleanse access, CC chain potential, CC immunity sources, escape tools, group synergy"#
        }
        "PvP" => {
            r#"
PvP-Specific Rules (competitive mode — many stats/bonuses/effects are split and reduced vs PvE):
- Stats come from an amulet (replaces all gear stats), NOT from individual gear pieces
- Rune and sigil systems still apply but are standardized PvP versions
- Many skill coefficients, trait bonuses, and boon durations are reduced in PvP ("competitive split")
- 1v1 dueling ability, +1 rotation (arriving to help in fights), and node defense all matter
- Burst windows, sustain between fights, and disengage/reset ability are crucial
- Stunbreaks, condition cleanse, and stability access are essential
- Relic still applies; choose for the game mode's fast-paced fights
- Consider: stomping/rezzing, decapping, mobility between nodes"#
        }
        _ => {
            r#"
PvE-Specific Rules:
- 6 armor pieces with 1 rune each (same rune x6 for set bonus)
- Sigils: 1 per 1H weapon, 2 per 2H (max 2 per set)
- 1 relic slot (build-defining effect)
- Consider: boon strip → vulnerability → damage rotation → buff uptime
- DPS uptime and benchmark rotations matter
- Group composition provides boons (Might, Fury, Quickness, Alacrity)"#
        }
    };

    format!("{}{}", base_rules, mode_context)
}

/// Parse a JSON build suggestion from Gemini's response text.
/// Extracts the JSON block from markdown fences if present.
pub fn parse_build_response(response: &str) -> Result<serde_json::Value, String> {
    // Try to find JSON in markdown code fences
    let json_str = if let Some(start) = response.find("```json") {
        let content = &response[start + 7..];
        if let Some(end) = content.find("```") {
            content[..end].trim()
        } else {
            content.trim()
        }
    } else if let Some(start) = response.find("```") {
        let content = &response[start + 3..];
        if let Some(end) = content.find("```") {
            content[..end].trim()
        } else {
            content.trim()
        }
    } else if response.trim_start().starts_with('{') {
        // Raw JSON without fences
        response.trim()
    } else {
        return Err("No JSON found in response".into());
    };

    serde_json::from_str(json_str).map_err(|e| format!("JSON parse error: {}", e))
}

/// Parsed build suggestion from Gemini response.
#[derive(Debug, Clone, Default)]
pub struct GeminiBuildResponse {
    pub specializations: Vec<(String, Vec<String>)>, // (spec_name, [trait1, trait2, trait3])
    pub weapons: Vec<String>,
    pub skills: Vec<String>,
    pub rune: String,
    pub sigils: Vec<String>,
    pub relic: String,
    pub stat_prefix: String,
    pub explanation: String,
    pub changes_made: Vec<String>,
    // New fields for synergy-driven optimization
    /// Per-slot sigils map (set1_main, set1_off, set2_main, set2_off).
    pub sigils_map: Option<std::collections::HashMap<String, String>>,
    /// Detailed synergy explanation (replaces generic explanation in new format).
    pub synergy_explanation: Option<String>,
    /// Structured changes with slot/from/to/reason (new format).
    pub changes_structured: Option<Vec<serde_json::Value>>,
    /// Per-slot gear map: kebab-case slot name (`helm`, `ring-1`,
    /// `weapon-set-1-main`, …) → stat-prefix **name**. Optional — plates
    /// without it keep today's profile-prefix behavior. Names are resolved
    /// against GameDb by `validation::validate_gemini_build`.
    pub gear_slots: Option<std::collections::HashMap<String, String>>,
    /// Ranger pet names from the plate: terrestrial[2] then aquatic[2].
    /// Missing or empty slots are None. Unresolved until validate.
    pub pets: Option<[Option<String>; 4]>,
}

/// Parse a Gemini response into a typed build suggestion.
/// Extracts JSON from markdown fences and maps fields.
pub fn parse_gemini_build(response: &str) -> Result<GeminiBuildResponse, String> {
    let json = parse_build_response(response)?;

    let mut result = GeminiBuildResponse::default();

    if let Some(v) = json.get("explanation").and_then(|v| v.as_str()) {
        result.explanation = v.to_string();
    }

    if let Some(specs) = json.get("specializations").and_then(|v| v.as_array()) {
        result.specializations = specs
            .iter()
            .filter_map(|s| {
                let name = s.get("name")?.as_str()?.to_string();
                let traits: Vec<String> = s
                    .get("traits")?
                    .as_array()?
                    .iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect();
                Some((name, traits))
            })
            .collect();
    }

    if let Some(weapons) = json.get("weapons") {
        for set_key in &["set1", "set2"] {
            if let Some(set) = weapons.get(*set_key) {
                let main = set.get("main").and_then(|v| v.as_str()).unwrap_or("");
                let off = set.get("off").and_then(|v| v.as_str());
                let label = if *set_key == "set1" { "Set 1" } else { "Set 2" };
                if let Some(off) = off.filter(|o| *o != "null" && !o.is_empty()) {
                    result
                        .weapons
                        .push(format!("{}: {} / {}", label, main, off));
                } else if !main.is_empty() {
                    result.weapons.push(format!("{}: {}", label, main));
                }
            }
        }
    }

    if let Some(skills) = json.get("skills") {
        if let Some(heal) = skills.get("heal").and_then(|v| v.as_str()) {
            result.skills.push(format!("Heal: {}", heal));
        }
        if let Some(utils) = skills.get("utilities").and_then(|v| v.as_array()) {
            let names: Vec<&str> = utils.iter().filter_map(|v| v.as_str()).collect();
            if !names.is_empty() {
                result.skills.push(format!("Utils: {}", names.join(", ")));
            }
        }
        if let Some(elite) = skills.get("elite").and_then(|v| v.as_str()) {
            result.skills.push(format!("Elite: {}", elite));
        }
    }

    if let Some(v) = json.get("rune").and_then(|v| v.as_str()) {
        result.rune = v.to_string();
    }
    // Handle sigils as either flat array or per-slot object
    if let Some(obj) = json.get("sigils").and_then(|v| v.as_object()) {
        // New format: {"set1_main": "...", "set1_off": "...", ...}
        let mut map = std::collections::HashMap::new();
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k.clone(), s.to_string());
            }
        }
        // Also flatten into sigils vec for backward compat. Walk the keys in
        // canonical positional order so downstream code that indexes the flat
        // list as [set1_main, set1_off, set2_main, set2_off] stays correct;
        // previously this was `map.values().cloned().collect()` which has
        // unspecified HashMap order.
        for key in &["set1_main", "set1_off", "set2_main", "set2_off"] {
            if let Some(name) = map.get(*key) {
                result.sigils.push(name.clone());
            }
        }
        result.sigils_map = Some(map);
    } else if let Some(arr) = json.get("sigils").and_then(|v| v.as_array()) {
        // Old format: ["Sigil1", "Sigil2", ...]
        result.sigils = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }

    if let Some(v) = json.get("relic").and_then(|v| v.as_str()) {
        result.relic = v.to_string();
    }
    if let Some(v) = json.get("stat_prefix").and_then(|v| v.as_str()) {
        result.stat_prefix = v.to_string();
    }
    // Old format: flat string array
    if let Some(arr) = json.get("changes_made").and_then(|v| v.as_array()) {
        result.changes_made = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    // New format: synergy_explanation field
    if let Some(v) = json.get("synergy_explanation").and_then(|v| v.as_str()) {
        result.synergy_explanation = Some(v.to_string());
    }
    // New format: structured changes with slot/from/to/reason
    if let Some(arr) = json.get("changes").and_then(|v| v.as_array()) {
        result.changes_structured = Some(arr.clone());
    }
    // Per-slot gear map: kebab slot name → prefix name. Optional; plates
    // without it keep the profile-prefix behavior (validation fills slots).
    if let Some(obj) = json.get("gear_slots").and_then(|v| v.as_object()) {
        let mut map = std::collections::HashMap::new();
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k.clone(), s.to_string());
            }
        }
        if !map.is_empty() {
            result.gear_slots = Some(map);
        }
    }

    result.pets = parse_pets_field(json.get("pets"));
    // Validator already tokenizes GeminiBuildResponse.skills for Legend*
    // ids (legend_ids_from_plate). Fold the plate legends field onto
    // skills, active first, so fill_revenant_legends takes the explicit-win
    // path without a new response field.
    let legends = parse_legends_field(json.get("legends"));
    if !legends.is_empty() {
        let mut prefixed = legends;
        prefixed.extend(std::mem::take(&mut result.skills));
        result.skills = prefixed;
    }

    Ok(result)
}

fn pet_slot_name(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        serde_json::Value::Number(n) => n.as_u64().map(|id| format!("#{id}")),
        serde_json::Value::Null => None,
        _ => None,
    }
}

fn two_pet_slots(arr: Option<&Vec<serde_json::Value>>) -> [Option<String>; 2] {
    let empty: Vec<serde_json::Value> = Vec::new();
    let items = arr.unwrap_or(&empty);
    [
        items.first().and_then(pet_slot_name),
        items.get(1).and_then(pet_slot_name),
    ]
}

/// Plate pets: `{terrestrial, aquatic}` arrays, or a flat 4-name list.
fn parse_pets_field(value: Option<&serde_json::Value>) -> Option<[Option<String>; 4]> {
    let value = value?;
    if let Some(obj) = value.as_object() {
        let land = two_pet_slots(obj.get("terrestrial").and_then(|v| v.as_array()));
        let water = two_pet_slots(obj.get("aquatic").and_then(|v| v.as_array()));
        return Some([
            land[0].clone(),
            land[1].clone(),
            water[0].clone(),
            water[1].clone(),
        ]);
    }
    if let Some(arr) = value.as_array() {
        return Some([
            arr.first().and_then(pet_slot_name),
            arr.get(1).and_then(pet_slot_name),
            arr.get(2).and_then(pet_slot_name),
            arr.get(3).and_then(pet_slot_name),
        ]);
    }
    None
}

/// Plate legends: `["Legend2", "Legend1"]` (active first). Also a lone string.
/// Empty / null entries are skipped. Returned ids are folded onto `skills`.
fn parse_legends_field(value: Option<&serde_json::Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    let mut push = |s: &str| {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
            return;
        }
        if !ids.iter().any(|id| id == trimmed) {
            ids.push(trimmed.to_string());
        }
    };
    if let Some(arr) = value.as_array() {
        for item in arr {
            if let Some(s) = item.as_str() {
                push(s);
            }
        }
    } else if let Some(s) = value.as_str() {
        push(s);
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choya_intent_reads_the_asked_json_and_nothing_else() {
        assert_eq!(
            parse_choya_intent(
                "```json\n{\"kind\": \"small_talk\", \"confidence\": 1.7, \"reason\": \"a greeting\"}\n```"
            ),
            Some((MessageKind::SmallTalk, 1.0, "a greeting".to_string())),
            "fenced, confidence clamped"
        );
        assert_eq!(
            parse_choya_intent("{\"kind\": \"BUILD\"}").map(|(k, c, _)| (k, c)),
            Some((MessageKind::Build, 0.5))
        );
        assert_eq!(parse_choya_intent("{\"kind\": \"recipe\"}"), None);
        assert_eq!(parse_choya_intent("It is small talk."), None);
        let prompt = choya_intent_prompt("how are you `doing`", false, true);
        assert!(prompt.contains("how are you doing") && prompt.contains("Build on the pass: yes"));
        assert!(
            prompt.len() < 1_500,
            "a few hundred tokens: {}",
            prompt.len()
        );
    }

    #[test]
    fn choya_reply_is_served_even_as_prose() {
        assert_eq!(
            parse_choya_reply("{\"reply\": \"Grumble.\", \"compose_plate\": true}"),
            ("Grumble.".to_string(), true)
        );
        assert_eq!(
            parse_choya_reply("Just prose, `no` JSON."),
            ("Just prose, no JSON.".to_string(), false)
        );
        let prompt = choya_reply_prompt("hi", MessageKind::SmallTalk, "Mode: PvE", "Deutsch");
        assert!(prompt.contains("in Deutsch") && prompt.contains("Mode: PvE"));
        assert!(
            !prompt.contains("PROFESSION REFERENCE"),
            "no plating kitchen"
        );
    }

    #[test]
    fn chat_prompt_offers_per_slot_mixing() {
        // Choya could not express "some Plaguedoctor" because the schema it is
        // shown documented only `stat_prefix`. `gear_slots` existed in the
        // response type and in `validate_gear_slot_map`, but nothing ever told
        // the model it could send one, so every mix request repainted the kit.
        let prompt = chat_refinement_prompt_with_tools(
            "Ranger",
            "WvW",
            "add some plaguedoctor in there",
            "",
            "English",
        );
        assert!(
            prompt.contains("\"gear_slots\""),
            "the documented schema must offer gear_slots, or the model cannot mix"
        );
        assert!(
            prompt.contains("stat_prefix") && prompt.contains("overrides only the slots it names"),
            "the prompt must say gear_slots overrides on top of the base stat_prefix"
        );
        // RED TMP-P9-A15-1: parse accepts pets, but the examples the model is
        // shown omit the key, so Ranger plates never send one.
        let schema = prompt
            .split_once("```json")
            .expect("the prompt must show a JSON example")
            .1
            .split_once("```")
            .expect("the JSON example must close")
            .0;
        assert!(
            schema.contains("\"pets\""),
            "the documented schema must include pets, or Ranger plates drop them"
        );
        assert!(
            prompt.contains("FULL JSON build object")
                && prompt
                    .split_once("FULL JSON build object")
                    .expect("FULL JSON key list")
                    .1
                    .split_once(')')
                    .expect("key list closes")
                    .0
                    .contains("pets"),
            "the FULL JSON key list must mention pets"
        );
        // Slot keys are matched against GearSlot::kebab_name(); a documented key
        // the validator would reject is worse than no documentation, because
        // the model would emit it and the slot would be silently dropped.
        //
        // Search ONLY the key list, not the whole prompt. The JSON example above
        // it already contains "amulet" and "ring-1", so a whole-prompt
        // `contains` check passes even when the list itself is wrong — measured:
        // corrupting the list to "ring1" left a whole-prompt check green.
        let list = prompt
            .split_once("Valid slot keys, exactly:")
            .expect("the prompt must introduce the slot keys with a stable phrase")
            .1
            .split_once('.')
            .expect("the slot key list must end in a period")
            .0;
        for key in [
            "helm",
            "shoulders",
            "coat",
            "gloves",
            "leggings",
            "boots",
            "back",
            "accessory-1",
            "accessory-2",
            "amulet",
            "ring-1",
            "ring-2",
            "weapon-set-1-main",
            "weapon-set-1-off",
            "weapon-set-2-main",
            "weapon-set-2-off",
        ] {
            assert!(
                list.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .any(|token| token == key),
                "slot key {key:?} is accepted by validate_gear_slot_map but the \
                 documented list does not offer it verbatim: {list:?}"
            );
            assert!(
                gw2_core::types::GearSlot::ALL
                    .iter()
                    .any(|s| s.kebab_name() == key),
                "documented slot key {key:?} is not a real GearSlot kebab name"
            );
        }
    }

    #[test]
    fn test_parse_build_response_with_fences() {
        let response = r#"Here's the build:
```json
{"stat_prefix": "Berserker's", "explanation": "Test"}
```
"#;
        let result = parse_build_response(response).unwrap();
        assert_eq!(result["stat_prefix"], "Berserker's");
    }

    #[test]
    fn test_parse_build_response_raw_json() {
        let response = r#"{"stat_prefix": "Viper's", "explanation": "Condi"}"#;
        let result = parse_build_response(response).unwrap();
        assert_eq!(result["stat_prefix"], "Viper's");
    }

    #[test]
    fn test_parse_build_response_no_json() {
        let response = "I think you should use Berserker's gear.";
        let result = parse_build_response(response);
        assert!(result.is_err());
    }

    #[test]
    fn parse_gemini_build_accepts_per_slot_gear_map() {
        let response = r#"{
            "stat_prefix": "Berserker's",
            "gear_slots": {
                "helm": "Berserker's",
                "coat": "Cavalier's",
                "ring-1": "Sinister",
                "weapon-set-1-main": "Assassin's"
            }
        }"#;
        let parsed = parse_gemini_build(response).unwrap();
        let map = parsed.gear_slots.expect("gear_slots must parse");
        assert_eq!(map.get("helm").map(String::as_str), Some("Berserker's"));
        assert_eq!(map.get("coat").map(String::as_str), Some("Cavalier's"));
        assert_eq!(map.get("ring-1").map(String::as_str), Some("Sinister"));
        assert_eq!(
            map.get("weapon-set-1-main").map(String::as_str),
            Some("Assassin's")
        );
    }

    #[test]
    fn parse_gemini_build_accepts_plate_without_gear_map() {
        let response = r#"{"stat_prefix": "Viper's", "explanation": "Condi"}"#;
        let parsed = parse_gemini_build(response).unwrap();
        assert!(parsed.gear_slots.is_none());
        assert!(parsed.pets.is_none());
    }

    /// RED: plate pets were dropped. Object form is terrestrial[2] then aquatic[2].
    #[test]
    fn parse_gemini_build_parses_ranger_pets() {
        let response = r#"{
            "pets": {
                "terrestrial": ["Jungle Stalker", "Brown Bear"],
                "aquatic": ["Shark", null]
            }
        }"#;
        let parsed = parse_gemini_build(response).unwrap();
        let pets = parsed.pets.expect("pets must parse");
        assert_eq!(pets[0].as_deref(), Some("Jungle Stalker"));
        assert_eq!(pets[1].as_deref(), Some("Brown Bear"));
        assert_eq!(pets[2].as_deref(), Some("Shark"));
        assert_eq!(pets[3], None);
    }

    /// RED TMP-P9-A15-5-2: plate legends were dropped. Validator already
    /// honors Legend* tokens on GeminiBuildResponse.skills (see
    /// legend_ids_from_plate). Parse must surface a legends field there so
    /// fill_revenant_legends takes the explicit-win path.
    #[test]
    fn parse_gemini_build_parses_revenant_legends() {
        let response = r#"{
            "skills": {"heal": "Enchanting Lullaby"},
            "legends": ["Legend2", "Legend1"]
        }"#;
        let parsed = parse_gemini_build(response).unwrap();
        assert!(
            parsed.skills.iter().any(|line| line
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| t == "Legend2")),
            "Legend2 must land on skills so legend_ids_from_plate can see it: {:?}",
            parsed.skills
        );
        assert!(
            parsed.skills.iter().any(|line| line
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| t == "Legend1")),
            "Legend1 must land on skills: {:?}",
            parsed.skills
        );
        let skills_joined = parsed.skills.join("\n");
        let l2 = skills_joined.find("Legend2").expect("Legend2");
        let l1 = skills_joined.find("Legend1").expect("Legend1");
        assert!(
            l2 < l1,
            "active legend first, same order as the plate: {:?}",
            parsed.skills
        );
    }

    /// RED TMP-P9-A15-5-2: parse is useless if the examples the model is
    /// shown omit legends, so Revenant plates never send one.
    #[test]
    fn prompt_examples_document_revenant_legends() {
        let new_build = new_build_prompt_with_tools(
            "Revenant",
            &OptimizationWeights::preset_power_dps(),
            "PvE",
        );
        let improve = improve_build_prompt_with_tools(
            "Revenant",
            &OptimizationWeights::preset_power_dps(),
            "PvE",
        );
        let chat =
            chat_refinement_prompt_with_tools("Revenant", "PvE", "power revan", "", "English");
        for (name, prompt) in [
            ("new_build", new_build.as_str()),
            ("improve", improve.as_str()),
            ("chat", chat.as_str()),
        ] {
            let schema = prompt
                .split_once("```json")
                .unwrap_or_else(|| panic!("{name} must show a JSON example"))
                .1
                .split_once("```")
                .unwrap_or_else(|| panic!("{name} JSON example must close"))
                .0;
            assert!(
                schema.contains("\"legends\""),
                "{name} documented schema must include legends, or Revenant plates drop them"
            );
        }
        assert!(
            chat.contains("FULL JSON build object")
                && chat
                    .split_once("FULL JSON build object")
                    .expect("FULL JSON key list")
                    .1
                    .split_once(')')
                    .expect("key list closes")
                    .0
                    .contains("legends"),
            "the FULL JSON key list must mention legends"
        );
    }

    #[test]
    fn parse_gemini_build_ignores_non_string_gear_values() {
        // A nested object / number for a slot is unusable; the parser keeps the
        // string entries and drops the rest instead of failing the whole plate.
        let response =
            r#"{"stat_prefix": "Berserker's", "gear_slots": {"helm": 42, "boots": "Berserker's"}}"#;
        let parsed = parse_gemini_build(response).unwrap();
        let map = parsed.gear_slots.expect("string entries must survive");
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("boots").map(String::as_str), Some("Berserker's"));
    }
    #[test]
    fn test_game_context_mentions_priorities() {
        let ctx = build_game_context("Warrior", &OptimizationWeights::preset_power_dps(), "PvE");
        assert!(ctx.contains("Power"));
        assert!(ctx.contains("PvE"));

        let wvw_ctx =
            build_game_context("Warrior", &OptimizationWeights::preset_power_dps(), "WvW");
        assert!(wvw_ctx.contains("WvW"));
        assert!(wvw_ctx.contains("competitive split"));
    }

    #[test]
    fn test_parse_gemini_build_full() {
        let response = r#"```json
{
  "specializations": [
    {"name": "Arms", "traits": ["Furious", "Dual Wielding", "Burst Mastery"]},
    {"name": "Discipline", "traits": ["Warrior's Sprint", "Inspiring Battle Standard", "Axe Mastery"]}
  ],
  "weapons": {
    "set1": {"main": "Axe", "off": "Axe"},
    "set2": {"main": "Greatsword", "off": null}
  },
  "skills": {
    "heal": "Mending",
    "utilities": ["Signet of Fury", "Banner of Strength", "Bull's Charge"],
    "elite": "Signet of Rage"
  },
  "rune": "Superior Rune of the Scholar",
  "sigils": ["Superior Sigil of Force", "Superior Sigil of Air"],
  "relic": "Relic of the Thief",
  "stat_prefix": "Berserker's",
  "explanation": "Power build with high crit synergy.",
  "changes_made": ["Switched to Axe/Axe for burst"]
}
```"#;
        let build = parse_gemini_build(response).unwrap();
        assert_eq!(build.specializations.len(), 2);
        assert_eq!(build.specializations[0].0, "Arms");
        assert_eq!(build.weapons.len(), 2);
        assert!(build.weapons[1].contains("Greatsword"));
        assert_eq!(build.skills.len(), 3);
        assert_eq!(build.rune, "Superior Rune of the Scholar");
        assert_eq!(build.sigils.len(), 2);
        assert_eq!(build.relic, "Relic of the Thief");
        assert_eq!(build.stat_prefix, "Berserker's");
        assert!(!build.explanation.is_empty());
        assert_eq!(build.changes_made.len(), 1);
    }

    #[test]
    fn test_weights_context_power() {
        let ctx = weights_context(&OptimizationWeights::preset_power_dps());
        assert!(ctx.contains("Power damage"));
        assert!(
            ctx.contains("MANDATORY"),
            "Power at 1.0 should trigger mandatory constraint"
        );
        assert!(
            ctx.contains("Berserker"),
            "Power at 1.0 should mandate Berserker's gear"
        );
    }

    #[test]
    fn test_weights_context_balanced() {
        let ctx = weights_context(&OptimizationWeights::preset_balanced());
        assert!(ctx.contains("Power damage"));
        assert!(ctx.contains("Survivability"));
    }

    // Inline snapshot tests for prompt builders.
    //
    // Surface drift in prompt wording in PR diffs. The expected strings below
    // ARE the snapshots: when intentionally changing a prompt, update the
    // corresponding `expected` literal in the same PR.
    //
    // Fixtures use empty/None lock constraints to avoid the nondeterministic
    // HashMap iteration in `BuildLocks::trait_locks` (a separate follow-up
    // addresses that flakiness).

    #[test]
    fn snapshot_new_build_prompt_with_tools() {
        let prompt =
            new_build_prompt_with_tools("Warrior", &OptimizationWeights::preset_power_dps(), "PvE");
        let expected = r#"You are an expert Guild Wars 2 build optimizer with access to the game's full database.

Create an optimal Power build for Warrior in PvE.

PLAYER PRIORITIES (6-axis radar chart): Power damage (100%)
MANDATORY: Use Berserker's or Assassin's gear (highest Power/Precision/Ferocity). Do NOT use any gear with Healing Power, Vitality, or Toughness as primary stat.

CRITICAL CONSTRAINT: The player's #1 priority axis is at 100%. You MUST use "Berserker's" as the stat_prefix. Choosing any other stat prefix will produce a build the player explicitly does not want. This is non-negotiable.

DESIGN PRINCIPLE: Pure damage output is NOT the goal. The ability to DELIVER damage is the goal. A build that can CC enemies, maintain stability, survive burst, and sustain pressure delivers more real damage than a glass cannon that gets interrupted. Every trait, skill, rune, sigil, and relic must work in concert. Consider: CC access, stunbreaks, stability, blocks, evades, condition cleanse alongside raw DPS.

WORKFLOW — use your tools to make informed decisions:

Phase 1 — Understand the landscape:
1. Read the PROFESSION REFERENCE for the specializations, traits and slot skills - it is complete and already in front of you. Call get_profession_info only if there is no reference.
2. Call get_optimizer_results to see the best gear/spec combos from the deterministic search

Phase 2 — Deep synergy analysis (THIS IS CRITICAL):
3. Trait columns for every specialization are in the PROFESSION REFERENCE; do not call get_spec_traits for them.
4. Call get_trait_details for key traits — check conditions_applied, buffs_applied, damage_modifiers, proc_triggers
5. Use search_traits_by_effect to find traits that match the priorities (e.g. "condition_damage" for condi builds, "crit" for power)
6. Use find_condition_sources to discover which skills/traits apply the build's key conditions (Bleeding, Burning, etc.)
7. Use search_skills_by_effect to find skills that apply specific conditions, buffs, or combo fields
8. Slot skill names and descriptions are in the PROFESSION REFERENCE. Call get_skill_info only for the few you shortlist and need facts on - chain skills, conditions_applied, buffs_applied, cooldowns.

Phase 3 — Equipment synergy:
9. Call search_upgrades(focus=power|condition|boon_support|healing|sustain|control) — ranked shortlist on the full 6-axis matrix, not A–Z. Use upgrade_synergies(name) for neighbors. list_runes/sigils/relics are the same ranking for the current radar.
10. Match rune/sigil/relic effects to the trait+skill kit: e.g. if the build crits often, pick "on crit" sigils; if it stacks Burning, pick Burning duration rune

Phase 4 — Verify the complete build:
11. Call find_synergies with your selected trait IDs + skill IDs to check for activated traited_facts (conditional bonuses)
12. Call get_build_synergy_report for a full synergy analysis of the candidate build
13. Call simulate_combat to verify the gear+trait combo performs well numerically
14. Call simulate_rotation with selected skill IDs to see real DPS, condition uptime, buff uptime, and control metrics (stunbreaks, stability)

NO ASSUMPTIONS. THIS IS THE RULE THAT MATTERS MOST.
Every specialization, trait, skill, rune, sigil and relic name you output must come from evidence in THIS conversation: the PROFESSION REFERENCE in Context, or a tool result. Not from memory. Your training data is older than the live game build, names change between patches, and a name that does not exist today is discarded — the player then gets no build at all, which is the worst possible answer.
- Specializations, their traits, and the slot skills (heal, utility, elite) are all in the PROFESSION REFERENCE. Take those names from it; do not call get_profession_info, get_spec_traits or get_skill_info to re-read what it already says. Only if no reference is present (no character selected) call get_spec_traits instead.
- Every specialization has exactly 3 trait columns (Adept, Master, Grandmaster). Pick exactly one trait from EACH column: 3 traits, never two from one column, never a minor trait (those are automatic and cannot be chosen).
- Runes, sigils and relics ranked for this player's radar are in the UPGRADE REFERENCE when one is present; take them from it, and call search_upgrades only for a focus or tag it does not cover. A skill's exact numbers (cooldown, conditions applied) come from get_skill_info; a trait's triggers from get_trait_details. Batch such calls into one round.
- If a tool does not return what you expected, choose from what it DID return. Never fall back on a remembered name.

REASON WITH MECHANISM AND NUMBERS, NOT VIBES.
When PROFESSION REFERENCE, UPGRADE REFERENCE and STAT PREFIX REFERENCE are all present, your default next action is the finished JSON plate from that evidence. Extra tool calls are optional: use them only for a missing fact that would change a choice, or when the player explicitly asks for a numerical comparison or simulation. Do not calculate_stats, score_build, find_synergies or simulate merely to finish an ordinary build request. The addon validates the plate locally after you answer.
A build is a machine: triggers fire effects, effects have durations, sources have cooldowns. Explain a mechanism supported by the supplied evidence. The rules below govern claims you make, not a mandatory tool checklist. Omit unsupported exact uptime, damage or simulation claims; never invent numbers or claim an unperformed check.
- Cooldown vs uptime is arithmetic. A 5s effect on a 30s cooldown is ~17% uptime; do not write about it as if it were permanent. State the fraction.
- Internal cooldowns bound proc rate. A sigil with a 9s ICD fires at most once per 9s no matter how often you crit, so a second on-crit source may add nothing.
- Triggers must be satisfiable. Use supplied trait facts; get_trait_details is available if a decisive trigger is missing. A trait keyed on a boon, a condition, or a threshold you never reach is dead weight. Name what supplies the trigger, or drop the trait.
- Condition builds: establish condition sources from the supplied facts, using find_condition_sources only when decisive evidence is missing. Match rune, relic and sigil duration bonuses to the conditions supported by those sources. A Burning-duration rune on a build that mostly bleeds is a wasted slot.
- Multipliers only count when their condition holds. Do not stack damage modifiers your build cannot meet at the same time.
- Cooldowns must fit the rotation. If three of your utilities are on 40s+ cooldowns, say what fills the gap between them.
- Check every chosen name and trait column against the supplied evidence before plating. Simulations and find_synergies are optional investigations, not prerequisites to serving a build. If you do run them and the numbers contradict the plan, change the plan — never the numbers.

WHAT THE EXPLANATION MUST SAY.
Give the synergy chain concretely: what triggers what, on what cooldown, and the uptime or multiplier that results. "Corruptor's Fervor stacks toughness" is not an argument. "Corruptor's Fervor gives Carapace per condition applied, and Harbinger Shroud pulses Torment every second, so Carapace holds near cap in sustained fights" is. Name the weakness the build accepts, in one clause — every build trades something.

Every component (traits, skills, rune, sigils, relic) must work as one codependent system — a rune that boosts Burning duration is wasted if your build barely applies Burning.

After gathering data, respond with ONLY a JSON build object:
```json
{
  "specializations": [
    {"name": "SpecName1", "traits": ["trait1", "trait2", "trait3"]},
    {"name": "SpecName2", "traits": ["trait1", "trait2", "trait3"]},
    {"name": "SpecName3", "traits": ["trait1", "trait2", "trait3"]}
  ],
  "weapons": {
    "set1": {"main": "WeaponType", "off": "WeaponType or null"},
    "set2": {"main": "WeaponType", "off": "WeaponType or null"}
  },
  "skills": {
    "heal": "SkillName",
    "utilities": ["Skill1", "Skill2", "Skill3"],
    "elite": "SkillName"
  },
  "rune": "RuneName",
  "sigils": ["Sigil1", "Sigil2", "Sigil3", "Sigil4"],
  "relic": "RelicName",
  "pets": {"terrestrial": ["PetName", "PetName"], "aquatic": ["PetName", "PetName"]},
  "legends": ["Legend1", "Legend2"],
  "stat_prefix": "PrefixName",
  "explanation": "in the REPLY SHAPE below."
}
```

"#
            .to_string()
            + REPLY_SHAPE;
        assert_eq!(prompt, expected, "new_build_prompt_with_tools drift");
    }

    #[test]
    fn snapshot_improve_build_prompt_with_tools() {
        let prompt = improve_build_prompt_with_tools(
            "Guardian",
            &OptimizationWeights::preset_power_dps(),
            "PvE",
        );
        let expected = r#"You are an expert Guild Wars 2 build optimizer with access to the game's full database.

Improve the player's current Power build for Guardian in PvE.

NO ASSUMPTIONS. THIS IS THE RULE THAT MATTERS MOST.
Every specialization, trait, skill, rune, sigil and relic name you output must come from evidence in THIS conversation: the PROFESSION REFERENCE in Context, or a tool result. Not from memory. Your training data is older than the live game build, names change between patches, and a name that does not exist today is discarded — the player then gets no build at all, which is the worst possible answer.
- Specializations, their traits, and the slot skills (heal, utility, elite) are all in the PROFESSION REFERENCE. Take those names from it; do not call get_profession_info, get_spec_traits or get_skill_info to re-read what it already says. Only if no reference is present (no character selected) call get_spec_traits instead.
- Every specialization has exactly 3 trait columns (Adept, Master, Grandmaster). Pick exactly one trait from EACH column: 3 traits, never two from one column, never a minor trait (those are automatic and cannot be chosen).
- Runes, sigils and relics ranked for this player's radar are in the UPGRADE REFERENCE when one is present; take them from it, and call search_upgrades only for a focus or tag it does not cover. A skill's exact numbers (cooldown, conditions applied) come from get_skill_info; a trait's triggers from get_trait_details. Batch such calls into one round.
- If a tool does not return what you expected, choose from what it DID return. Never fall back on a remembered name.

REASON WITH MECHANISM AND NUMBERS, NOT VIBES.
When PROFESSION REFERENCE, UPGRADE REFERENCE and STAT PREFIX REFERENCE are all present, your default next action is the finished JSON plate from that evidence. Extra tool calls are optional: use them only for a missing fact that would change a choice, or when the player explicitly asks for a numerical comparison or simulation. Do not calculate_stats, score_build, find_synergies or simulate merely to finish an ordinary build request. The addon validates the plate locally after you answer.
A build is a machine: triggers fire effects, effects have durations, sources have cooldowns. Explain a mechanism supported by the supplied evidence. The rules below govern claims you make, not a mandatory tool checklist. Omit unsupported exact uptime, damage or simulation claims; never invent numbers or claim an unperformed check.
- Cooldown vs uptime is arithmetic. A 5s effect on a 30s cooldown is ~17% uptime; do not write about it as if it were permanent. State the fraction.
- Internal cooldowns bound proc rate. A sigil with a 9s ICD fires at most once per 9s no matter how often you crit, so a second on-crit source may add nothing.
- Triggers must be satisfiable. Use supplied trait facts; get_trait_details is available if a decisive trigger is missing. A trait keyed on a boon, a condition, or a threshold you never reach is dead weight. Name what supplies the trigger, or drop the trait.
- Condition builds: establish condition sources from the supplied facts, using find_condition_sources only when decisive evidence is missing. Match rune, relic and sigil duration bonuses to the conditions supported by those sources. A Burning-duration rune on a build that mostly bleeds is a wasted slot.
- Multipliers only count when their condition holds. Do not stack damage modifiers your build cannot meet at the same time.
- Cooldowns must fit the rotation. If three of your utilities are on 40s+ cooldowns, say what fills the gap between them.
- Check every chosen name and trait column against the supplied evidence before plating. Simulations and find_synergies are optional investigations, not prerequisites to serving a build. If you do run them and the numbers contradict the plan, change the plan — never the numbers.

WHAT THE EXPLANATION MUST SAY.
Give the synergy chain concretely: what triggers what, on what cooldown, and the uptime or multiplier that results. "Corruptor's Fervor stacks toughness" is not an argument. "Corruptor's Fervor gives Carapace per condition applied, and Harbinger Shroud pulses Torment every second, so Carapace holds near cap in sustained fights" is. Name the weakness the build accepts, in one clause — every build trades something.

PLAYER PRIORITIES (6-axis radar chart): Power damage (100%)
MANDATORY: Use Berserker's or Assassin's gear (highest Power/Precision/Ferocity). Do NOT use any gear with Healing Power, Vitality, or Toughness as primary stat.

CRITICAL CONSTRAINT: The player's #1 priority axis is at 100%. You MUST use "Berserker's" as the stat_prefix. Choosing any other stat prefix will produce a build the player explicitly does not want. This is non-negotiable.

DESIGN PRINCIPLE: Pure damage output is NOT the goal. The ability to DELIVER damage is the goal. Consider CC access, stunbreaks, stability, survivability, and control alongside raw DPS. A build that disables enemies and maintains pressure outperforms one that only maximizes numbers on a golem.

WORKFLOW — use your tools:

Phase 1 — Understand the current build:
1. Call get_current_build to see what the player is currently using
2. Call get_optimizer_results to see what the deterministic search found
3. Call get_build_synergy_report on the current build to identify weak synergies or missing interactions

Phase 2 — Find improvements via synergy analysis:
4. Find better trait choices in the PROFESSION REFERENCE, which already lists every column of every specialization.
5. Call get_trait_details for traits you're considering — check conditions_applied, buffs_applied, proc_triggers, damage_modifiers
6. Use search_traits_by_effect to find traits that better match the priorities
7. Use find_condition_sources to check if the build's condition application matches its gear (e.g. Viper's gear with few Burning sources is wasteful)
8. Use search_skills_by_effect to find skills that better synergize with chosen traits
9. Call search_upgrades / upgrade_synergies — match 6-axis scores and trigger conditions to the actual skill/trait kit
10. Call find_synergies to verify new trait+skill combinations activate traited_facts (conditional bonuses)

Phase 3 — Verify:
11. Call simulate_combat to compare performance before/after changes
12. Call simulate_rotation with the skill set to validate condition uptime, buff uptime, and control metrics

Focus on impactful changes. Explain WHY each change improves the build — cite specific trait-skill synergies, activated conditional bonuses, or proc chains you discovered via tools. Don't just swap to "meta" choices; demonstrate the interaction chain.

After gathering data, respond with ONLY a JSON build object:
```json
{
  "specializations": [...],
  "weapons": {...},
  "skills": {...},
  "rune": "...",
  "sigils": [...],
  "relic": "...",
  "pets": {"terrestrial": [...], "aquatic": [...]},
  "legends": ["Legend1", "Legend2"],
  "stat_prefix": "...",
  "changes_made": ["Change 1 description", "Change 2 description"],
  "explanation": "in the REPLY SHAPE below."
}
```

"#
            .to_string()
            + REPLY_SHAPE;
        assert_eq!(prompt, expected, "improve_build_prompt_with_tools drift");
    }

    #[test]
    fn snapshot_chat_refinement_prompt_with_tools() {
        let kitchen =
            "Mode: PvE\nLocks: none\nCharacter: Profession: Warrior\nOn the pass: (empty)";
        let prompt = chat_refinement_prompt_with_tools(
            "Warrior",
            "PvE",
            "make this build more bursty please",
            kitchen,
            "English",
        );
        assert!(
            prompt.contains("Write the \"explanation\" field in English"),
            "reply language instruction missing: {prompt}"
        );
        // Was: "If Context already lists an equipped Character loadout, do not
        // call tools; edit that loadout." That shortcut is why Choya named
        // traits from memory and had whole builds refused. The loadout is now
        // a starting point, and the tool lookup is mandatory either way.
        assert!(
            prompt.contains("An equipped Character loadout in Context is your STARTING POINT"),
            "equipped-loadout handling missing: {prompt}"
        );
        assert!(
            !prompt.contains("do not call tools"),
            "the equipped-loadout path must never forbid tool use: {prompt}"
        );
        // The player pins what they want kept; silence about a slot is a hole
        // they see, not consent to keep it. A plate that named specs, traits,
        // skills and rune but no weapons reached the Optimized tab with an
        // empty WEAPONS column (measured in-game 2026-09-05, 1.11.29).
        assert!(
            prompt.contains("Nothing they are wearing is fixed unless they pinned it"),
            "the pin rule must survive: {prompt}"
        );
        assert!(
            prompt.contains("Always fill in both weapon sets, all four sigils and the relic"),
            "every plate must be told to fill the weapon slots: {prompt}"
        );
        assert!(
            prompt.contains("never omit a set because it did not change"),
            "unchanged weapons must still be written: {prompt}"
        );
        assert!(
            prompt.starts_with("You are Choya: a knee-high, melon-bodied cactus"),
            "persona drift: {prompt}"
        );
        // The persona is lore-bound, not free-associated: these are the wiki's
        // own traits (aggression, grumbling, the mesa) and the discipline that
        // keeps character from eating the build advice.
        for anchor in [
            "aggressive",
            "grumble",
            "kick troublemakers off the mesa",
            "ONE flourish per reply",
        ] {
            assert!(
                prompt.contains(anchor),
                "persona lost its anchor {anchor:?}: {prompt}"
            );
        }
        assert!(
            prompt.contains("<message>\nmake this build more bursty please\n</message>"),
            "message sandbox drift"
        );
        assert!(
            prompt.contains("Context:\nMode: PvE"),
            "context brief missing"
        );
        assert!(
            prompt.contains("Role chips are families"),
            "family-role instruction missing: {prompt}"
        );
        assert!(
            !prompt.contains("PLAYER PRIORITIES"),
            "radar must not cage Choya: {prompt}"
        );
        assert!(
            !prompt.contains("Honor Locks"),
            "locks must not cage Choya: {prompt}"
        );
        assert!(
            prompt.contains("\"stat_prefix\""),
            "JSON tasting schema missing"
        );
        assert!(
            prompt.contains("\"name\": \"SpecName1\""),
            "spec objects missing name"
        );
        assert!(
            prompt.contains("\"traits\":"),
            "spec objects missing traits"
        );
        assert!(
            prompt.contains("Never explanation-only"),
            "build requests must ask for a full kit: {prompt}"
        );
        assert!(
            prompt.contains("Shortbow, Longbow, Greatsword"),
            "API weapon names missing: {prompt}"
        );
        for tool in [
            "get_profession_info",
            "get_spec_traits",
            "get_trait_details",
            "get_skill_info",
            "get_current_build",
            "get_optimizer_results",
            "list_runes",
            "list_sigils",
            "list_relics",
            "search_upgrades",
            "upgrade_synergies",
            "calculate_stats",
            "simulate_combat",
            "simulate_rotation",
            "score_build",
            "find_synergies",
            "get_build_synergy_report",
            "find_condition_sources",
            "search_skills_by_effect",
            "search_traits_by_effect",
        ] {
            assert!(prompt.contains(tool), "chef prompt missing tool {tool}");
        }
    }

    #[test]
    fn chef_prompt_keeps_pasted_names_in_kitchen_when_order_is_capped() {
        let order = "x".repeat(500);
        let kitchen = "Mode: PvE\nPasted: Rune of the Scholar (item)";
        let prompt =
            chat_refinement_prompt_with_tools("Warrior", "PvE", &order, kitchen, "English");
        assert!(
            prompt.contains("Pasted: Rune of the Scholar (item)"),
            "pasted names must live in the kitchen brief, not the 500-char order"
        );
        assert!(
            prompt.contains(&"x".repeat(500)),
            "full capped order should still be present"
        );
    }

    /// The live failure this rule exists for: the chat prompt told the model
    /// NOT to call tools when a loadout was equipped, so it named traits from
    /// training data older than the game build. One name of six did not
    /// exist, validation refused the whole build, and the player got four
    /// replies in a row with no build in them.
    #[test]
    fn build_prompts_forbid_naming_from_memory() {
        let w = OptimizationWeights::preset_power_dps();
        for (name, prompt) in [
            ("new", new_build_prompt_with_tools("Necromancer", &w, "WvW")),
            (
                "improve",
                improve_build_prompt_with_tools("Necromancer", &w, "WvW"),
            ),
            (
                "chat",
                chat_refinement_prompt_with_tools(
                    "Necromancer",
                    "WvW",
                    "give me an unkillable harbinger",
                    "Character: Harbinger, Tab 4",
                    "English",
                ),
            ),
        ] {
            assert!(
                prompt.contains("NO ASSUMPTIONS"),
                "{name}: must forbid naming from memory"
            );
            assert!(
                prompt.contains("get_spec_traits"),
                "{name}: must say where legal trait names come from"
            );
            assert!(
                prompt.contains("one trait from EACH column"),
                "{name}: must state the 3-columns-pick-one rule that validation enforces"
            );
            assert!(
                prompt.contains("Internal cooldowns"),
                "{name}: must demand mechanism/timing reasoning, not vibes"
            );
        }
    }

    /// A player with a build equipped is the COMMON case, and it was the one
    /// that forbade tool calls outright.
    #[test]
    fn equipped_loadout_does_not_forbid_tool_use() {
        let chat = chat_refinement_prompt_with_tools(
            "Necromancer",
            "WvW",
            "improve this",
            "Character: Harbinger, Tab 4: HARBI",
            "English",
        );
        assert!(
            !chat.contains("do not call tools"),
            "an equipped loadout must never again mean 'guess from memory'"
        );
        assert!(
            !chat.contains("at most two tool rounds"),
            "verifying three specs' trait columns does not fit in two rounds"
        );
    }

    #[test]
    fn chef_prompt_asks_for_selected_reply_language() {
        let prompt =
            chat_refinement_prompt_with_tools("Warrior", "PvE", "salut", "Mode: PvE", "French");
        assert!(
            prompt.contains("Write the \"explanation\" field in French"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("Write the \"explanation\" field in English"),
            "must not also demand English"
        );
    }

    #[test]
    fn reply_shape_paragraph_present_in_all_three_prompts() {
        let w = OptimizationWeights::preset_power_dps();
        let prompts = [
            new_build_prompt_with_tools("Warrior", &w, "PvE"),
            improve_build_prompt_with_tools("Warrior", &w, "PvE"),
            chat_refinement_prompt_with_tools("Warrior", "PvE", "hi", "Mode: PvE", "English"),
        ];
        for p in &prompts {
            assert!(p.contains("REPLY SHAPE for the"), "{p}");
            assert!(
                !p.contains("2-4 sentences") && !p.contains("2-3 sentences"),
                "{p}"
            );
        }
    }
}

#[cfg(test)]
mod kitchen_cap_tests {
    /// The profession reference is ~26 KB. It has to reach the model whole.
    #[test]
    fn a_full_profession_reference_survives_the_prompt() {
        let reference = "PROFESSION REFERENCE\n".to_string() + &"trait line\n".repeat(3_000);
        assert!(reference.len() > 30_000);
        let prompt = super::chat_refinement_prompt_with_tools(
            "Necromancer",
            "WvW",
            "power",
            &reference,
            "en",
        );
        assert!(
            prompt.matches("trait line").count() == 3_000,
            "the kitchen was cut before it reached the prompt"
        );
    }
}
