//! In-memory indexed game database.
//! Pre-indexes all cached game data into HashMaps for O(1) lookups.
//! This is the single source of truth for the optimizer — loaded once from
//! the file cache, then queried throughout the optimization pipeline.

use std::collections::HashMap;

use gw2_api::cache::DataCache;
use gw2_api::models::{
    Item, ItemStat, Legend, Pet, Profession, PvpAmulet, Skill, Specialization, Trait as GW2Trait,
};

/// `data/form_variants.json`: palette skill id -> the id cast outside the
/// form. Compile-time data, parsed by `druid_bar_carries_the_out_of_form_glyph`.
fn form_variants() -> &'static HashMap<u32, u32> {
    #[derive(serde::Deserialize)]
    struct Variant {
        skill: u32,
        out_of_form: u32,
    }
    #[derive(serde::Deserialize)]
    struct File {
        variants: Vec<Variant>,
    }
    static VARIANTS: std::sync::OnceLock<HashMap<u32, u32>> = std::sync::OnceLock::new();
    VARIANTS.get_or_init(|| {
        let file: File = serde_json::from_str(include_str!("../../../data/form_variants.json"))
            .expect("embedded form_variants.json is invalid");
        file.variants
            .into_iter()
            .map(|v| (v.skill, v.out_of_form))
            .collect()
    })
}

/// In-memory indexed game database loaded from cache.
#[derive(Clone)]
pub struct GameDb {
    pub items: HashMap<u32, Item>,
    pub itemstats: HashMap<u32, ItemStat>,
    pub skills: HashMap<u32, Skill>,
    pub traits: HashMap<u32, GW2Trait>,
    pub specializations: HashMap<u32, Specialization>,
    pub professions: HashMap<String, Profession>,
    pub legends: HashMap<String, Legend>,
    pub pvp_amulets: HashMap<u32, PvpAmulet>,
    pub pets: HashMap<u32, Pet>,

    // Derived indexes for fast lookups
    pub skills_by_profession: HashMap<String, Vec<u32>>,
    pub traits_by_spec: HashMap<u32, Vec<u32>>,
    pub items_by_type: HashMap<String, Vec<u32>>,
    pub runes: Vec<u32>,
    pub sigils: Vec<u32>,
    pub relics: Vec<u32>,
    // Skill ↔ palette ID mapping (for build template chat codes)
    pub skill_to_palette: HashMap<u32, u32>,
    pub palette_to_skill: HashMap<u32, u32>,

    // Reverse indexes for synergy queries (condition/buff name → IDs that apply it)
    pub traits_by_condition: HashMap<String, Vec<u32>>,
    pub skills_by_condition: HashMap<String, Vec<u32>>,
    pub traits_by_buff: HashMap<String, Vec<u32>>,
    pub skills_by_buff: HashMap<String, Vec<u32>>,

    /// Official API names for the current UI language. Optimizer still uses English `.name`.
    pub localized: Option<std::sync::Arc<gw2_api::localize::LocalizedNames>>,
}

impl GameDb {
    /// Load all game data from the file cache and build indexes.
    pub fn load(cache: &DataCache) -> Result<Self, String> {
        let items_vec: Vec<Item> = cache
            .load("items")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let itemstats_vec: Vec<ItemStat> = cache
            .load("itemstats")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let skills_vec: Vec<Skill> = cache
            .load("skills")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let traits_vec: Vec<GW2Trait> = cache
            .load("traits")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let specs_vec: Vec<Specialization> = cache
            .load("specializations")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let professions_vec: Vec<Profession> = cache
            .load("professions")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let legends_vec: Vec<Legend> = cache
            .load("legends")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let pvp_amulets_vec: Vec<PvpAmulet> = cache
            .load("pvp_amulets")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let pets_vec: Vec<Pet> = cache
            .load("pets")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();

        // Validate critical data is non-empty
        if professions_vec.is_empty() {
            return Err("No professions found in cache — game data may not be downloaded".into());
        }
        if specs_vec.is_empty() {
            return Err(
                "No specializations found in cache — game data may not be downloaded".into(),
            );
        }
        if itemstats_vec.is_empty() {
            return Err("No item stats found in cache — game data may not be downloaded".into());
        }
        if skills_vec.is_empty() {
            return Err("No skills found in cache — game data may not be downloaded".into());
        }
        if traits_vec.is_empty() {
            return Err("No traits found in cache — game data may not be downloaded".into());
        }
        if items_vec.is_empty() {
            return Err("No items found in cache — game data may not be downloaded".into());
        }
        if legends_vec.is_empty() {
            return Err("No legends found in cache — game data may not be downloaded".into());
        }
        if pets_vec.is_empty() {
            return Err("No pets found in cache — game data may not be downloaded".into());
        }
        if pvp_amulets_vec.is_empty() {
            return Err("No PvP amulets found in cache — game data may not be downloaded".into());
        }

        let items: HashMap<u32, Item> = items_vec.into_iter().map(|i| (i.id, i)).collect();
        let itemstats: HashMap<u32, ItemStat> =
            itemstats_vec.into_iter().map(|i| (i.id, i)).collect();
        let skills: HashMap<u32, Skill> = skills_vec.into_iter().map(|s| (s.id, s)).collect();
        let traits: HashMap<u32, GW2Trait> = traits_vec.into_iter().map(|t| (t.id, t)).collect();
        let specializations: HashMap<u32, Specialization> =
            specs_vec.into_iter().map(|s| (s.id, s)).collect();
        let professions: HashMap<String, Profession> = professions_vec
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        let legends: HashMap<String, Legend> =
            legends_vec.into_iter().map(|l| (l.id.clone(), l)).collect();
        let pvp_amulets: HashMap<u32, PvpAmulet> =
            pvp_amulets_vec.into_iter().map(|a| (a.id, a)).collect();
        let pets: HashMap<u32, Pet> = pets_vec.into_iter().map(|p| (p.id, p)).collect();

        let mut skills_by_profession = profession_skill_index(&skills);

        let mut traits_by_spec: HashMap<u32, Vec<u32>> = HashMap::new();
        for t in traits.values() {
            traits_by_spec
                .entry(t.specialization)
                .or_default()
                .push(t.id);
        }

        let mut items_by_type: HashMap<String, Vec<u32>> = HashMap::new();
        let mut runes = Vec::new();
        let mut sigils = Vec::new();
        let mut relics = Vec::new();
        let mut pvp_runes = Vec::new();
        let mut pvp_sigils = Vec::new();

        // Build skill ↔ palette ID maps from professions.
        //
        // Iterate professions in a deterministic order (sorted by name) so
        // that when a skill_id or palette_id is shared across professions
        // (e.g. racial elites, downed-state skills), the last-writer-wins
        // resolution is stable across runs and machines. `HashMap::values()`
        // ordering is unspecified — without this sort the same input cache
        // could yield two different `skill_to_palette` / `palette_to_skill`
        // mappings, then break weapon-skill-by-palette lookups depending on
        // which profession won the insert.
        let mut prof_names: Vec<&String> = professions.keys().collect();
        prof_names.sort_unstable();
        let mut skill_to_palette: HashMap<u32, u32> = HashMap::new();
        let mut palette_to_skill: HashMap<u32, u32> = HashMap::new();
        for prof_name in prof_names {
            if let Some(prof) = professions.get(prof_name) {
                for pair in &prof.skills_by_palette {
                    if pair.len() == 2 {
                        let palette_id = pair[0];
                        let skill_id = pair[1];
                        skill_to_palette.insert(skill_id, palette_id);
                        palette_to_skill.insert(palette_id, skill_id);
                    }
                }
            }
        }

        for item in items.values() {
            items_by_type
                .entry(item.item_type.clone())
                .or_default()
                .push(item.id);

            // Categorize upgrade components
            if item.item_type == "UpgradeComponent" {
                if let Some(ref details) = item.details {
                    match details.detail_type.as_deref() {
                        Some("Rune") => runes.push(item.id),
                        Some("Sigil") => sigils.push(item.id),
                        // PvP rune/sigil variants (see `pvp_rune`/`pvp_sigil`).
                        // Appended after the sort so a PvE item always wins a
                        // fuzzy name lookup; only an exact PvP name reaches one.
                        Some("Default") if pvp_rune(item, details) => pvp_runes.push(item.id),
                        Some("Default") if pvp_sigil(item, details) => pvp_sigils.push(item.id),
                        _ => {}
                    }
                }
            } else if item.item_type == "Relic" {
                relics.push(item.id);
            }
        }

        // Build reverse indexes for synergy queries
        let mut traits_by_condition: HashMap<String, Vec<u32>> = HashMap::new();
        let mut traits_by_buff: HashMap<String, Vec<u32>> = HashMap::new();
        for t in traits.values() {
            for fact in &t.facts {
                if let gw2_api::models::facts::Fact::Buff {
                    status: Some(s), ..
                } = fact
                {
                    if is_condition(s) {
                        traits_by_condition
                            .entry(condition_index_key(s).to_string())
                            .or_default()
                            .push(t.id);
                    } else if is_boon(s) {
                        traits_by_buff.entry(s.clone()).or_default().push(t.id);
                    }
                }
            }
        }

        let mut skills_by_condition: HashMap<String, Vec<u32>> = HashMap::new();
        let mut skills_by_buff: HashMap<String, Vec<u32>> = HashMap::new();
        for skill in skills.values() {
            for fact in &skill.facts {
                if let gw2_api::models::facts::Fact::Buff {
                    status: Some(s), ..
                } = fact
                {
                    if is_condition(s) {
                        skills_by_condition
                            .entry(condition_index_key(s).to_string())
                            .or_default()
                            .push(skill.id);
                    } else if is_boon(s) {
                        skills_by_buff.entry(s.clone()).or_default().push(skill.id);
                    }
                }
            }
        }

        // Deduplicate (a trait/skill may have multiple Buff facts for same condition).
        // Also sort the profession/spec/type indexes so downstream consumers
        // (`profession_skills()`, LLM tool execs)
        // get a stable order across runs — these Vecs were previously populated
        // from `HashMap::values()` iteration which has non-deterministic order.
        for ids in traits_by_condition.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        for ids in traits_by_buff.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        for ids in skills_by_condition.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        for ids in skills_by_buff.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        for ids in skills_by_profession.values_mut() {
            ids.sort_unstable();
        }
        for ids in traits_by_spec.values_mut() {
            ids.sort_unstable();
        }
        for ids in items_by_type.values_mut() {
            ids.sort_unstable();
        }

        // Sort the upgrade-item id vecs so `all_runes()`/`all_sigils()`/
        // `all_relics()` iterate in stable order. Populated from
        // `items.values()` HashMap iteration, which is unspecified — beam
        // search neighbor order (swap_rune, swap_relic, swap_sigil_slots)
        // inherited the nondeterminism without these sorts.
        runes.sort_unstable();
        sigils.sort_unstable();
        relics.sort_unstable();
        pvp_runes.sort_unstable();
        pvp_sigils.sort_unstable();
        runes.extend(pvp_runes);
        sigils.extend(pvp_sigils);

        Ok(GameDb {
            items,
            itemstats,
            skills,
            traits,
            specializations,
            professions,
            legends,
            pvp_amulets,
            pets,
            skills_by_profession,
            traits_by_spec,
            items_by_type,
            runes,
            sigils,
            relics,
            skill_to_palette,
            palette_to_skill,
            traits_by_condition,
            skills_by_condition,
            traits_by_buff,
            skills_by_buff,
            localized: None,
        })
    }

    pub fn profession(&self, name: &str) -> Option<&Profession> {
        self.professions.get(name)
    }

    /// The skill the game casts for bar skill `id` outside a form. A Druid
    /// glyph's palette skill (Glyph of Alignment 31322, the id the build tab
    /// and palette 4821 carry) has no damage or conditions and no API link
    /// to what is cast; `data/form_variants.json` names the variant from the
    /// wiki (31607 out of Celestial Avatar). Ids it does not catalogue, and
    /// variants missing from the cache, come back unchanged.
    pub fn out_of_form_variant(&self, id: u32) -> u32 {
        form_variants()
            .get(&id)
            .copied()
            .filter(|variant| self.skills.contains_key(variant))
            .unwrap_or(id)
    }

    /// Deterministic itemstat lookup by name (case-insensitive). Returns the
    /// exact-name match if any, otherwise the shortest substring match
    /// (lower id wins ties). Exact ties do not lowest-id across distinct
    /// multiplier shapes: wiki three-stat Giver's (Toughness / Healing /
    /// BoonDuration) outranks other Giver's templates, then lower id.
    ///
    /// Centralizes the policy used by validation, Gemini tool execs, and
    /// LLM-context builders — a raw `itemstats.values().find(contains)` is
    /// non-deterministic because `HashMap::values()` iteration order is
    /// unspecified, so the same input could resolve to different itemstats
    /// across runs and machines.
    pub fn itemstat_by_name(&self, needle: &str) -> Option<&ItemStat> {
        if needle.is_empty() {
            return None;
        }
        let needle_lower = needle.to_lowercase();
        let needle_key = gw2_core::i18n::alnum_key(needle);
        let mut exact: Option<&ItemStat> = None;
        let mut fuzzy: Option<(usize, u32, &ItemStat)> = None;
        for is in self.itemstats.values() {
            let lower = is.name.to_lowercase();
            let alnum_hit =
                !needle_key.is_empty() && gw2_core::i18n::alnum_key(&is.name) == needle_key;
            if lower == needle_lower || alnum_hit {
                match exact {
                    Some(prev) if !Self::exact_name_outranks(is, prev) => {}
                    _ => exact = Some(is),
                }
            } else if needle_lower.len() >= 5 && lower.contains(&needle_lower) {
                let key = (is.name.len(), is.id);
                match fuzzy {
                    Some((plen, pid, _)) if (plen, pid) <= key => {}
                    _ => fuzzy = Some((key.0, key.1, is)),
                }
            }
        }
        exact.or(fuzzy.map(|(_, _, is)| is))
    }

    /// Exact-name ties: wiki three-stat Giver's outranks other multiplier
    /// shapes of that English name; otherwise lower id wins.
    fn exact_name_outranks(candidate: &ItemStat, incumbent: &ItemStat) -> bool {
        match (
            crate::itemstat_pool::is_wiki_givers_three_stat(candidate),
            crate::itemstat_pool::is_wiki_givers_three_stat(incumbent),
        ) {
            (true, false) => true,
            (false, true) => false,
            _ => candidate.id < incumbent.id,
        }
    }

    pub fn attach_localized(&mut self, mut names: gw2_api::localize::LocalizedNames) {
        names.by_english.clear();
        let mut add = |en: &str, loc: &str| {
            if !en.is_empty() && !loc.is_empty() {
                names
                    .by_english
                    .insert(en.to_ascii_lowercase(), loc.to_string());
            }
        };
        for (id, loc) in &names.skills {
            if let Some(s) = self.skills.get(id) {
                add(&s.name, loc);
            }
        }
        for (id, loc) in &names.traits {
            if let Some(t) = self.traits.get(id) {
                add(&t.name, loc);
            }
        }
        for (id, loc) in &names.specs {
            if let Some(s) = self.specializations.get(id) {
                add(&s.name, loc);
            }
        }
        for (id, loc) in &names.items {
            if let Some(i) = self.items.get(id) {
                add(&i.name, loc);
            }
        }
        for (id, loc) in &names.itemstats {
            if let Some(s) = self.itemstats.get(id) {
                add(&s.name, loc);
            }
        }
        for (id, loc) in &names.professions {
            add(id, loc);
            if let Some(p) = self.professions.get(id) {
                add(&p.name, loc);
            }
        }
        for (id, loc) in &names.legends {
            add(id, loc);
        }
        for (id, loc) in &names.pvp_amulets {
            if let Some(a) = self.pvp_amulets.get(id) {
                add(&a.name, loc);
            }
        }
        for (id, loc) in &names.pets {
            if let Some(p) = self.pets.get(id) {
                add(&p.name, loc);
                let compact = p.name.trim_start_matches("Juvenile ");
                if compact != p.name {
                    add(compact, loc);
                }
            }
        }
        self.localized = Some(std::sync::Arc::new(names));
    }

    pub fn loc_skill<'a>(&'a self, id: u32, fallback: &'a str) -> &'a str {
        self.localized
            .as_ref()
            .and_then(|l| l.skills.get(&id))
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    pub fn loc_trait<'a>(&'a self, id: u32, fallback: &'a str) -> &'a str {
        self.localized
            .as_ref()
            .and_then(|l| l.traits.get(&id))
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    pub fn loc_spec<'a>(&'a self, id: u32, fallback: &'a str) -> &'a str {
        self.localized
            .as_ref()
            .and_then(|l| l.specs.get(&id))
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    pub fn loc_item<'a>(&'a self, id: u32, fallback: &'a str) -> &'a str {
        self.localized
            .as_ref()
            .and_then(|l| l.items.get(&id))
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    pub fn loc_pet<'a>(&'a self, id: u32, fallback: &'a str) -> &'a str {
        self.localized
            .as_ref()
            .and_then(|l| l.pets.get(&id))
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    /// English display name for a ranger pet id, or `#id` when the catalog
    /// has not been downloaded yet.
    pub fn pet_display_name(&self, id: u32) -> String {
        self.pets
            .get(&id)
            .map(|p| self.loc_pet(id, &p.name).to_string())
            .unwrap_or_else(|| format!("#{id}"))
    }

    pub fn pet_by_name(&self, needle: &str) -> Option<&Pet> {
        let needle = needle.trim();
        if needle.is_empty() {
            return None;
        }
        if let Some(id) = needle.strip_prefix('#').and_then(|s| s.parse::<u32>().ok()) {
            return self.pets.get(&id);
        }
        let compact = needle.trim_start_matches("Juvenile ");
        self.pets.values().find(|p| {
            p.name.eq_ignore_ascii_case(needle)
                || p.name
                    .trim_start_matches("Juvenile ")
                    .eq_ignore_ascii_case(compact)
        })
    }

    pub fn loc_prefix<'a>(&'a self, english: &'a str) -> &'a str {
        self.itemstat_by_name(english)
            .and_then(|s| {
                self.localized
                    .as_ref()
                    .and_then(|l| l.itemstats.get(&s.id))
                    .map(String::as_str)
            })
            .unwrap_or(english)
    }

    pub fn loc_name<'a>(&'a self, english: &'a str) -> &'a str {
        let Some(loc) = &self.localized else {
            return english;
        };
        loc.by_english
            .get(&english.to_ascii_lowercase())
            .map(String::as_str)
            .unwrap_or(english)
    }

    /// Palette ID for a build-template skill slot.
    ///
    /// Revenant legend skills share one palette per slot; `/v2/professions`
    /// `skills_by_palette` only lists the latest legend's skill IDs. A miss on
    /// an older legend skill is mapped through that shared slot palette.
    pub fn skill_palette_id(&self, skill_id: u32) -> u32 {
        if let Some(&p) = self.skill_to_palette.get(&skill_id) {
            return p;
        }
        let Some((heal_p, util_p, elite_p)) = self.revenant_shared_palettes() else {
            return 0;
        };
        for legend in self.legends.values() {
            if skill_id == legend.heal {
                return heal_p;
            }
            if skill_id == legend.elite {
                return elite_p;
            }
            if let Some(i) = legend.utilities.iter().position(|&u| u == skill_id) {
                if i < 3 {
                    return util_p[i];
                }
            }
        }
        0
    }

    /// Template byte for a revenant legend id (`Legend1`…), from `/v2/legends.code`.
    pub fn legend_template_code(&self, legend_id: &str) -> u8 {
        if let Some(c) = self.legends.get(legend_id).and_then(|l| l.code) {
            return c.min(255) as u8;
        }
        legend_id
            .strip_prefix("Legend")
            .and_then(|n| n.parse::<u8>().ok())
            .unwrap_or(0)
    }

    /// True when this legend's swap skill is ungated, or its elite spec is equipped.
    pub fn legend_available(&self, legend_id: &str, spec_ids: &[u32]) -> bool {
        let Some(legend) = self.legends.get(legend_id) else {
            return false;
        };
        match self.skills.get(&legend.swap).and_then(|s| s.specialization) {
            None => true,
            Some(spec_id) => spec_ids.contains(&spec_id),
        }
    }

    fn revenant_shared_palettes(&self) -> Option<(u32, [u32; 3], u32)> {
        for legend in self.legends.values() {
            let Some(&heal_p) = self.skill_to_palette.get(&legend.heal) else {
                continue;
            };
            let Some(&elite_p) = self.skill_to_palette.get(&legend.elite) else {
                continue;
            };
            if legend.utilities.len() < 3 {
                continue;
            }
            let Some(&u0) = self.skill_to_palette.get(&legend.utilities[0]) else {
                continue;
            };
            let Some(&u1) = self.skill_to_palette.get(&legend.utilities[1]) else {
                continue;
            };
            let Some(&u2) = self.skill_to_palette.get(&legend.utilities[2]) else {
                continue;
            };
            return Some((heal_p, [u0, u1, u2], elite_p));
        }
        None
    }

    pub fn profession_skills(&self, profession: &str) -> Vec<&Skill> {
        self.skills_by_profession
            .get(profession)
            .map(|ids| ids.iter().filter_map(|id| self.skills.get(id)).collect())
            .unwrap_or_default()
    }

    /// Every skill a character of `profession` can slot: the profession's
    /// own skills plus racial skills (shared across professions). The search
    /// never picks from this — it does not know the race — but a build the
    /// player or an LLM names must still resolve.
    pub fn skills_usable_by(&self, profession: &str) -> Vec<&Skill> {
        let mut out = self.profession_skills(profession);
        let mut racial: Vec<&Skill> = self
            .skills
            .values()
            .filter(|s| s.professions.len() > 1 && s.professions.iter().any(|p| p == profession))
            .collect();
        racial.sort_by_key(|s| s.id);
        out.extend(racial);
        out
    }

    pub fn spec_traits(&self, spec_id: u32) -> Vec<&GW2Trait> {
        self.traits_by_spec
            .get(&spec_id)
            .map(|ids| ids.iter().filter_map(|id| self.traits.get(id)).collect())
            .unwrap_or_default()
    }

    pub fn all_runes(&self) -> Vec<&Item> {
        self.runes
            .iter()
            .filter_map(|id| self.items.get(id))
            .collect()
    }

    pub fn all_sigils(&self) -> Vec<&Item> {
        self.sigils
            .iter()
            .filter_map(|id| self.items.get(id))
            .collect()
    }

    pub fn all_relics(&self) -> Vec<&Item> {
        self.relics
            .iter()
            .filter_map(|id| self.items.get(id))
            .collect()
    }

    /// Nourishment (food) legal in `mode` via `game_types`.
    pub fn nourishments_for(&self, mode: &gw2_core::types::GameMode) -> Vec<&Item> {
        self.consumables_of(&["Food", "Nourishment"], mode)
    }

    /// Enhancement (utility consumable) legal in `mode` via `game_types`.
    pub fn enhancements_for(&self, mode: &gw2_core::types::GameMode) -> Vec<&Item> {
        self.consumables_of(&["Utility", "Enhancement"], mode)
    }

    /// Standing combat-stat infusions/enrichments legal in `mode`.
    /// Agony-only upgrades are excluded (fixed fill+lock only).
    pub fn stat_infusions_for(&self, mode: &gw2_core::types::GameMode) -> Vec<&Item> {
        let ids = self
            .items_by_type
            .get("UpgradeComponent")
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut out: Vec<&Item> = ids
            .iter()
            .filter_map(|id| self.items.get(id))
            .filter(|item| {
                crate::infusions::is_stat_infusion(item)
                    && crate::infusions::item_legal_for_mode(item, mode)
            })
            .collect();
        out.sort_by_key(|item| item.id);
        out
    }

    /// Sample Ascended (or any) item's infusion_slots flags for a gear slot.
    pub fn sample_infusion_slot_flags(
        &self,
        slot: gw2_core::types::GearSlot,
    ) -> Option<Vec<Vec<String>>> {
        use gw2_core::types::GearSlot;
        let want_types: &[&str] = match slot {
            GearSlot::Helm
            | GearSlot::Shoulders
            | GearSlot::Coat
            | GearSlot::Gloves
            | GearSlot::Leggings
            | GearSlot::Boots => &["Armor"],
            GearSlot::Back => &["Back"],
            GearSlot::Accessory1 | GearSlot::Accessory2 => &["Trinket"],
            GearSlot::Amulet => &["Trinket"],
            GearSlot::Ring1 | GearSlot::Ring2 => &["Trinket"],
            GearSlot::WeaponSet1Main
            | GearSlot::WeaponSet1Off
            | GearSlot::WeaponSet2Main
            | GearSlot::WeaponSet2Off => &["Weapon"],
        };
        let detail_hint: Option<&str> = match slot {
            GearSlot::Amulet => Some("Amulet"),
            GearSlot::Ring1 | GearSlot::Ring2 => Some("Ring"),
            GearSlot::Accessory1 | GearSlot::Accessory2 => Some("Accessory"),
            _ => None,
        };
        for ty in want_types {
            let ids = self
                .items_by_type
                .get(*ty)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            for id in ids {
                let Some(item) = self.items.get(id) else {
                    continue;
                };
                let Some(details) = item.details.as_ref() else {
                    continue;
                };
                if details.infusion_slots.is_empty() {
                    continue;
                }
                if let Some(hint) = detail_hint {
                    let dt = details.detail_type.as_deref().unwrap_or("");
                    if !dt.eq_ignore_ascii_case(hint) {
                        continue;
                    }
                }
                return Some(
                    details
                        .infusion_slots
                        .iter()
                        .map(|s| s.flags.clone())
                        .collect(),
                );
            }
        }
        None
    }

    fn consumables_of(
        &self,
        detail_types: &[&str],
        mode: &gw2_core::types::GameMode,
    ) -> Vec<&Item> {
        let ids = self
            .items_by_type
            .get("Consumable")
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut out: Vec<&Item> = ids
            .iter()
            .filter_map(|id| self.items.get(id))
            .filter(|item| {
                crate::consumables::kind_of(item).is_some()
                    && item
                        .details
                        .as_ref()
                        .and_then(|d| d.detail_type.as_deref())
                        .is_some_and(|t| {
                            detail_types.iter().any(|want| t.eq_ignore_ascii_case(want))
                        })
                    && crate::consumables::item_legal_for_mode(item, mode)
            })
            .collect();
        // items_by_type is already sorted; keep id order for a stable argmax.
        out.sort_by_key(|item| item.id);
        out
    }

    pub fn spec(&self, id: u32) -> Option<&Specialization> {
        self.specializations.get(&id)
    }

    pub fn traits_applying_condition(&self, condition: &str) -> Vec<&GW2Trait> {
        self.traits_by_condition
            .get(condition_index_key(condition))
            .map(|ids| ids.iter().filter_map(|id| self.traits.get(id)).collect())
            .unwrap_or_default()
    }

    pub fn skills_applying_condition(&self, condition: &str) -> Vec<&Skill> {
        self.skills_by_condition
            .get(condition_index_key(condition))
            .map(|ids| ids.iter().filter_map(|id| self.skills.get(id)).collect())
            .unwrap_or_default()
    }

    /// Summary stats for logging.
    pub fn summary(&self) -> String {
        format!(
            "GameDb: {} items, {} itemstats, {} skills, {} traits, {} specs, {} professions, {} runes, {} sigils, {} relics",
            self.items.len(),
            self.itemstats.len(),
            self.skills.len(),
            self.traits.len(),
            self.specializations.len(),
            self.professions.len(),
            self.runes.len(),
            self.sigils.len(),
            self.relics.len(),
        )
    }

    /// Empty indexed db for unit tests (optimizer + addon).
    pub fn empty_for_tests() -> Self {
        use std::collections::HashMap;
        Self {
            items: HashMap::new(),
            itemstats: HashMap::new(),
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
}

use crate::data::boon_condition_formulas::is_condition;

fn condition_index_key(status: &str) -> &str {
    crate::data::boon_condition_formulas::canonical_condition_name(status)
}

/// PvP is on its own item table: the PvP copy of a rune or sigil is a
/// separate item whose `details.type` is `"Default"`, not `"Rune"`/`"Sigil"`,
/// so the tag-driven classification above drops it and no synced PvP
/// reference build can be plated. Measured over the cached `items.json`
/// (17 296 items, 987 UpgradeComponents, 614 of them `"Default"`): exactly
/// 146 `"Default"` components are PvP-only (`game_types == ["Pvp",
/// "PvpLobby"]`) — 78 runes, 58 sigils and 10 stat jewels. Every other
/// `"Default"` is a PvE infusion or jewel. Within the PvP-only set the split
/// is structural, so nothing here matches on a name: a rune carries the six
/// tier `bonuses` strings, a sigil carries an `infix_upgrade.buff` and no
/// bonuses, a jewel carries neither (its infix is bare attributes).
///
/// Names do not collide with the PvE items (the PvP copies are "Rune of X",
/// the PvE ones "Superior Rune of X"), and the optimizer's own search paths
/// filter on `"Superior"`, so these never enter a PvE or WvW candidate set.
fn pvp_rune(item: &Item, details: &gw2_api::models::ItemDetails) -> bool {
    is_pvp_only(item) && !details.bonuses.is_empty()
}

/// Sigil half of [`pvp_rune`]: an `infix_upgrade.buff` with no rune tiers.
fn pvp_sigil(item: &Item, details: &gw2_api::models::ItemDetails) -> bool {
    is_pvp_only(item)
        && details.bonuses.is_empty()
        && details
            .infix_upgrade
            .as_ref()
            .is_some_and(|i| i.buff.is_some())
}

fn is_pvp_only(item: &Item) -> bool {
    !item.game_types.is_empty()
        && item
            .game_types
            .iter()
            .all(|g| g == "Pvp" || g == "PvpLobby")
}

/// GW2 boons.
fn is_boon(status: &str) -> bool {
    matches!(
        status,
        "Might"
            | "Fury"
            | "Quickness"
            | "Alacrity"
            | "Protection"
            | "Resolution"
            | "Regeneration"
            | "Vigor"
            | "Stability"
            | "Swiftness"
            | "Resistance"
            | "Aegis"
    )
}

/// Test-only thin wrapper preserved so the alias-routing regression suite in
/// `data::boon_condition_formulas::tests` keeps exercising the shared
/// `is_condition` helper through the path the gamedb module consumes it by.
/// Skills per profession, by the profession the skill belongs to.
///
/// Racial skills list every profession that can slot them (eight: all but
/// Revenant). The optimizer does not know the character's race, no published
/// build carries one, and the flow simulation cannot value most of them
/// (Healing Seed has no Heal fact), so a skill shared by several professions
/// belongs to none here. Measured 2026-09-05: 10 of 36 calibration seeds had
/// picked Battle Roar, Shrapnel Mine, Reaper of Grenth or Healing Seed.
fn profession_skill_index(skills: &HashMap<u32, Skill>) -> HashMap<String, Vec<u32>> {
    let mut index: HashMap<String, Vec<u32>> = HashMap::new();
    for skill in skills.values() {
        if let [profession] = skill.professions.as_slice() {
            index.entry(profession.clone()).or_default().push(skill.id);
        }
    }
    index
}

#[cfg(test)]
pub(crate) mod tests_alias_helpers {
    pub(crate) fn is_condition(status: &str) -> bool {
        crate::data::boon_condition_formulas::is_condition(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw2_api::models::Legend;

    fn legend(
        id: &str,
        code: u32,
        heal: u32,
        elite: u32,
        utilities: [u32; 3],
        swap: u32,
    ) -> Legend {
        Legend {
            id: id.into(),
            code: Some(code),
            swap,
            heal,
            elite,
            utilities: utilities.to_vec(),
        }
    }

    #[test]
    fn racial_skills_belong_to_no_profession() {
        let skill = |id: u32, professions: &[&str]| gw2_api::models::Skill {
            id,
            name: format!("skill{id}"),
            description: None,
            icon: None,
            chat_link: None,
            skill_type: None,
            weapon_type: None,
            professions: professions.iter().map(|p| p.to_string()).collect(),
            slot: Some("Utility".into()),
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
        };
        let mut skills = HashMap::new();
        skills.insert(1, skill(1, &["Warrior"]));
        skills.insert(
            2,
            skill(
                2,
                &[
                    "Guardian",
                    "Warrior",
                    "Engineer",
                    "Ranger",
                    "Thief",
                    "Elementalist",
                    "Mesmer",
                    "Necromancer",
                ],
            ),
        );
        let index = profession_skill_index(&skills);
        assert_eq!(index.get("Warrior"), Some(&vec![1]));
        assert!(
            index.values().flatten().all(|id| *id != 2),
            "a racial skill must not be indexed under any profession: {index:?}"
        );

        // ...but a named racial skill still resolves for the profession.
        let mut db = GameDb::empty_for_tests();
        db.skills = skills;
        db.skills_by_profession = index;
        let usable: Vec<u32> = db
            .skills_usable_by("Warrior")
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(usable, vec![1, 2]);
        assert!(db.skills_usable_by("Revenant").is_empty());
    }

    #[test]
    fn itemstat_by_name_ignores_apostrophe() {
        let mut db = GameDb::empty_for_tests();
        db.itemstats.insert(
            1,
            gw2_api::models::ItemStat {
                id: 1,
                name: "Knight's".into(),
                attributes: vec![],
            },
        );
        assert_eq!(db.itemstat_by_name("Knights").unwrap().name, "Knight's");
        assert_eq!(db.itemstat_by_name("Knight's").unwrap().name, "Knight's");
    }

    #[test]
    fn itemstat_short_needle_does_not_fuzzy() {
        let mut db = GameDb::empty_for_tests();
        db.itemstats.insert(
            1,
            gw2_api::models::ItemStat {
                id: 1,
                name: "Berserker's".into(),
                attributes: vec![],
            },
        );
        assert!(db.itemstat_by_name("a").is_none());
        assert!(db.itemstat_by_name("sig").is_none());
        assert_eq!(db.itemstat_by_name("Berserker's").map(|s| s.id), Some(1));
    }

    fn hollow_cache() -> (std::path::PathBuf, gw2_api::cache::DataCache) {
        let dir = std::env::temp_dir().join(format!(
            "gw2bo-hollow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let cache = gw2_api::cache::DataCache::new(&dir);
        (dir, cache)
    }

    fn save_json(cache: &gw2_api::cache::DataCache, key: &str, row: serde_json::Value) {
        cache
            .save(key, &serde_json::json!([row]), 1)
            .unwrap_or_else(|e| panic!("save {key}: {e}"));
    }

    /// Professions / specs / itemstats / skills / traits / items / legends /
    /// pets / pvp_amulets. `skip` is omitted (missing file). `empty` is saved
    /// as `[]`.
    fn seed_load_catalogs(cache: &gw2_api::cache::DataCache, skip: &str, empty: &str) {
        let put = |key: &str, row: serde_json::Value| {
            if key == skip {
                return;
            }
            if key == empty {
                cache
                    .save(key, &Vec::<serde_json::Value>::new(), 1)
                    .unwrap_or_else(|e| panic!("save empty {key}: {e}"));
                return;
            }
            save_json(cache, key, row);
        };
        put(
            "professions",
            serde_json::json!({
                "id": "Guardian",
                "name": "Guardian",
                "specializations": [],
                "weapons": {},
                "training": [],
                "skills_by_palette": []
            }),
        );
        put(
            "specializations",
            serde_json::json!({
                "id": 1,
                "name": "Zeal",
                "profession": "Guardian",
                "elite": false,
                "minor_traits": [],
                "major_traits": []
            }),
        );
        put(
            "itemstats",
            serde_json::json!({"id": 161, "name": "Berserker's", "attributes": []}),
        );
        put("skills", serde_json::json!({"id": 1, "name": "Strike"}));
        put(
            "traits",
            serde_json::json!({
                "id": 1,
                "name": "Zealot's Speed",
                "specialization": 1,
                "tier": 1,
                "order": 0,
                "slot": "Major"
            }),
        );
        put(
            "items",
            serde_json::json!({
                "id": 1,
                "name": "Piece",
                "type": "Armor",
                "rarity": "Exotic",
                "level": 80
            }),
        );
        put(
            "legends",
            serde_json::json!({
                "id": "Legend1",
                "swap": 1,
                "heal": 2,
                "elite": 3,
                "utilities": []
            }),
        );
        put(
            "pets",
            serde_json::json!({"id": 1, "name": "Juvenile Bear"}),
        );
        put(
            "pvp_amulets",
            serde_json::json!({"id": 1, "name": "Berserker Amulet", "attributes": {}}),
        );
    }

    #[test]
    fn load_rejects_empty_skills_traits_or_items() {
        let (dir, cache) = hollow_cache();
        seed_load_catalogs(&cache, "skills", "");
        let err = match GameDb::load(&cache) {
            Ok(_) => panic!("hollow skills must fail"),
            Err(e) => e,
        };
        assert!(
            err.contains("skills"),
            "expected skills fail-closed, got {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_empty_or_missing_legends_pets_or_pvp_amulets() {
        for catalog in ["legends", "pets", "pvp_amulets"] {
            let needle = match catalog {
                "pvp_amulets" => "amulet",
                other => other,
            };
            for empty in ["", catalog] {
                let skip = if empty.is_empty() { catalog } else { "" };
                let (dir, cache) = hollow_cache();
                seed_load_catalogs(&cache, skip, empty);
                let err = match GameDb::load(&cache) {
                    Ok(_) => panic!("{catalog} skip={skip:?} empty={empty:?} must fail"),
                    Err(e) => e,
                };
                assert!(
                    err.to_lowercase().contains(needle)
                        && err.contains("game data may not be downloaded"),
                    "{catalog} skip={skip:?} empty={empty:?}: {err}"
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
        let (dir, cache) = hollow_cache();
        seed_load_catalogs(&cache, "", "");
        GameDb::load(&cache).expect("full seed must load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live `/v2/itemstats` ships several Giver's multiplier shapes under one
    /// English name. Lowest-id exact match is Toughness-only 627; wiki L80
    /// Giver's (Attribute combinations, retrieved 2026-08-29) is Toughness /
    /// Healing Power / Concentration (API: Healing, BoonDuration) at
    /// 628/1070/1430. Math must resolve the three-stat template.
    #[test]
    fn itemstat_by_name_givers_prefers_three_stat_not_lowest_id() {
        let mut db = GameDb::empty_for_tests();
        let attr = |attribute: &str, multiplier: f64, value: i32| {
            gw2_api::models::itemstats::StatAttribute {
                attribute: attribute.into(),
                multiplier,
                value,
            }
        };
        let row = |id: u32, attributes: Vec<gw2_api::models::itemstats::StatAttribute>| {
            gw2_api::models::ItemStat {
                id,
                name: "Giver's".into(),
                attributes,
            }
        };
        db.itemstats
            .insert(627, row(627, vec![attr("Toughness", 0.35, 0)]));
        db.itemstats.insert(
            628,
            row(
                628,
                vec![
                    attr("Toughness", 0.35, 0),
                    attr("Healing", 0.25, 0),
                    attr("BoonDuration", 0.25, 0),
                ],
            ),
        );
        db.itemstats.insert(
            629,
            row(
                629,
                vec![attr("Toughness", 0.35, 0), attr("Healing", 0.25, 0)],
            ),
        );
        db.itemstats.insert(
            1070,
            row(
                1070,
                vec![
                    attr("Toughness", 0.35, 0),
                    attr("Healing", 0.25, 0),
                    attr("BoonDuration", 0.25, 0),
                ],
            ),
        );
        db.itemstats.insert(
            1430,
            row(
                1430,
                vec![
                    attr("Toughness", 0.35, 32),
                    attr("Healing", 0.25, 18),
                    attr("BoonDuration", 0.25, 18),
                ],
            ),
        );

        let got = db
            .itemstat_by_name("Giver's")
            .expect("Giver's must resolve");
        assert_ne!(
            got.id, 627,
            "Giver's must not resolve to Toughness-only 627"
        );
        assert_eq!(got.id, 628, "lowest wiki three-stat Giver's id");
        let mut attrs: Vec<&str> = got
            .attributes
            .iter()
            .filter(|a| a.multiplier > 0.0)
            .map(|a| a.attribute.as_str())
            .collect();
        attrs.sort_unstable();
        assert_eq!(
            attrs.as_slice(),
            ["BoonDuration", "Healing", "Toughness"].as_slice()
        );
        assert_eq!(db.itemstat_by_name("Givers").map(|s| s.id), Some(628));
    }

    #[test]
    fn revenant_older_legend_skill_uses_shared_palette() {
        let mut db = GameDb::empty_for_tests();
        db.legends.insert(
            "Legend1".into(),
            legend("Legend1", 1, 27220, 27760, [28379, 27014, 26644], 28085),
        );
        db.legends.insert(
            "Legend8".into(),
            legend("Legend8", 8, 77043, 76968, [77243, 77291, 76805], 76610),
        );
        db.skill_to_palette.insert(77043, 4572);
        db.skill_to_palette.insert(76968, 4554);
        db.skill_to_palette.insert(77243, 4614);
        db.skill_to_palette.insert(77291, 4651);
        db.skill_to_palette.insert(76805, 4564);

        assert_eq!(db.skill_palette_id(77043), 4572);
        assert_eq!(
            db.skill_palette_id(27220),
            4572,
            "Shiro/Dragon heal shares Conduit heal palette"
        );
        assert_eq!(db.skill_palette_id(27760), 4554);
        assert_eq!(db.skill_palette_id(28379), 4614);
        assert_eq!(db.skill_palette_id(26644), 4564);
        assert_eq!(db.skill_palette_id(99999), 0);
    }

    #[test]
    fn legend_template_code_prefers_api_code() {
        let mut db = GameDb::empty_for_tests();
        db.legends
            .insert("Legend5".into(), legend("Legend5", 5, 1, 2, [3, 4, 5], 6));
        assert_eq!(db.legend_template_code("Legend5"), 5);
        assert_eq!(db.legend_template_code("Legend9"), 9);
    }

    #[test]
    fn legend_available_respects_swap_specialization() {
        let mut db = GameDb::empty_for_tests();
        db.legends.insert(
            "Legend1".into(),
            legend("Legend1", 1, 1, 2, [3, 4, 5], 28085),
        );
        db.skills.insert(
            28085,
            Skill {
                id: 28085,
                name: "Legendary Dragon Stance".into(),
                description: None,
                icon: None,
                chat_link: None,
                skill_type: Some("Profession".into()),
                weapon_type: None,
                professions: vec!["Revenant".into()],
                slot: Some("Profession_1".into()),
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
                specialization: Some(52),
            },
        );
        assert!(!db.legend_available("Legend1", &[3, 9]));
        assert!(db.legend_available("Legend1", &[3, 9, 52]));
    }

    #[test]
    fn loc_skill_falls_back_and_uses_overlay() {
        let mut db = GameDb::empty_for_tests();
        let skill: Skill = serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "Signet of Malice"
        }))
        .unwrap();
        db.skills.insert(1, skill);
        assert_eq!(db.loc_skill(1, "Signet of Malice"), "Signet of Malice");
        assert_eq!(db.loc_name("Signet of Malice"), "Signet of Malice");
        let mut names = gw2_api::localize::LocalizedNames {
            lang: "fr".into(),
            ..Default::default()
        };
        names.skills.insert(1, "Sceau de malice".into());
        db.attach_localized(names);
        assert_eq!(db.loc_skill(1, "Signet of Malice"), "Sceau de malice");
        assert_eq!(db.loc_name("Signet of Malice"), "Sceau de malice");
    }

    #[test]
    fn pet_display_name_uses_catalog_or_hash_id() {
        let mut db = GameDb::empty_for_tests();
        assert_eq!(db.pet_display_name(66), "#66");
        db.pets.insert(
            66,
            Pet {
                id: 66,
                name: "Juvenile Smokescale".into(),
                description: None,
                icon: None,
                skills: vec![],
            },
        );
        assert_eq!(db.pet_display_name(66), "Juvenile Smokescale");
        assert_eq!(db.pet_by_name("Smokescale").map(|p| p.id), Some(66));
        assert_eq!(db.pet_by_name("#66").map(|p| p.id), Some(66));
    }

    #[test]
    fn immobilized_aliases_hit_immobile_index() {
        let mut db = GameDb::empty_for_tests();
        let skill: Skill = serde_json::from_value(serde_json::json!({
            "id": 7,
            "name": "Test Immobilize"
        }))
        .unwrap();
        db.skills.insert(7, skill);
        db.skills_by_condition.insert("Immobile".into(), vec![7]);
        for name in ["Immobilized", "Immobilize", "Immobile"] {
            let hits = db.skills_applying_condition(name);
            assert_eq!(hits.len(), 1, "{name}");
            assert_eq!(hits[0].id, 7, "{name}");
        }
    }

    /// The PvP item table's runes and sigils carry `details.type = "Default"`,
    /// so they are told apart structurally. Shapes are copied from the cached
    /// `items.json`: rune 21092, sigil 21121, jewel 21093, PvE sigil 24615.
    #[test]
    fn pvp_default_upgrades_split_into_runes_and_sigils() {
        let item = |v: serde_json::Value| -> Item { serde_json::from_value(v).expect("item") };
        let pvp = serde_json::json!(["Pvp", "PvpLobby"]);

        let rune = item(serde_json::json!({
            "id": 21092, "name": "Rune of Strength", "type": "UpgradeComponent",
            "rarity": "Exotic", "level": 0, "game_types": pvp,
            "details": { "type": "Default", "suffix": "of Strength",
                "bonuses": ["+25 Power", "+4% Might Duration"],
                "infix_upgrade": { "id": 112, "attributes": [], "buff": null } }
        }));
        let sigil = item(serde_json::json!({
            "id": 21121, "name": "Sigil of Agony", "type": "UpgradeComponent",
            "rarity": "Exotic", "level": 0, "game_types": pvp,
            "details": { "type": "Default", "suffix": "of Agony", "bonuses": [],
                "infix_upgrade": { "id": 1250, "attributes": [],
                    "buff": { "skill_id": 38742, "description": "Bleeding +25%" } } }
        }));
        // A stat jewel is PvP-only and "Default" too: no tiers, no buff.
        let jewel = item(serde_json::json!({
            "id": 21093, "name": "Berserker's Jewel", "type": "UpgradeComponent",
            "rarity": "Exotic", "level": 0, "game_types": pvp,
            "details": { "type": "Default", "suffix": "", "bonuses": [],
                "infix_upgrade": { "id": 512,
                    "attributes": [{ "attribute": "Power", "modifier": 125 }], "buff": null } }
        }));
        // The PvE copy keeps its own tag and never reaches these predicates.
        let pve_sigil = item(serde_json::json!({
            "id": 24615, "name": "Superior Sigil of Force", "type": "UpgradeComponent",
            "rarity": "Exotic", "level": 39, "game_types": ["Activity", "Wvw", "Dungeon", "Pve"],
            "details": { "type": "Sigil", "suffix": "of Force", "bonuses": [],
                "infix_upgrade": { "id": 1234, "attributes": [],
                    "buff": { "skill_id": 9447, "description": "+5% damage" } } }
        }));

        let d = |i: &Item| i.details.clone().expect("details");
        assert!(pvp_rune(&rune, &d(&rune)));
        assert!(!pvp_sigil(&rune, &d(&rune)));
        assert!(pvp_sigil(&sigil, &d(&sigil)));
        assert!(!pvp_rune(&sigil, &d(&sigil)));
        assert!(!pvp_rune(&jewel, &d(&jewel)) && !pvp_sigil(&jewel, &d(&jewel)));
        assert_eq!(d(&pve_sigil).detail_type.as_deref(), Some("Sigil"));
        assert!(!is_pvp_only(&pve_sigil));
    }
}
