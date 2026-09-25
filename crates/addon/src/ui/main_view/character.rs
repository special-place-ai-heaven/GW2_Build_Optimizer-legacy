//! GW2 API use here is allowlisted I/O: the character list, build and
//! equipment tab fetch, and the DataCache copies of those payloads.
//! Not a measure path.

use super::resolution::resolve_selected_build_inner;
use crate::state::AddonState;

fn report_cache_write_error(state: &mut AddonState, message: String) {
    nexus::log::log(nexus::log::LogLevel::Warning, "GW2BuildOpt", &message);
    state.main.error = Some(message);
}

/// Refresh account characters and the selected character's tabs from the API.
pub(super) fn reload_from_api(state: &mut AddonState) {
    if state.config.gw2_api_key.is_none() {
        return;
    }
    if !state.main.characters_loading {
        load_characters(state);
    }
    let Some(name) = state
        .main
        .selected_character
        .and_then(|i| state.main.characters.get(i).cloned())
    else {
        return;
    };
    if !state.main.build_loading {
        load_character_tabs(state, name);
    }
}

/// Phase 1: Load characters from cache (instant) then refresh from API in background.
pub(super) fn load_characters(state: &mut AddonState) {
    if state.main.characters_loading {
        return;
    }
    let Some(ref key) = state.config.gw2_api_key else {
        state.main.error = Some("No GW2 API key configured".into());
        return;
    };

    state.main.error = None;

    // Phase 1: try loading from cache instantly
    let cache_dir = state.addon_dir.join("cache");
    let cache = gw2_api::cache::DataCache::new(&cache_dir);
    if let Ok(Some(cached_chars)) = cache.load_characters() {
        state.main.characters = cached_chars;
        state.main.characters_loading = false;
    } else {
        // No cache — show loading indicator until API responds
        state.main.characters_loading = true;
    }

    // Phase 2: background refresh from API
    let key = key.clone();
    let had_cache = !state.main.characters.is_empty();

    let spawned = state.spawn_worker("load-characters", move |token| {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Always reset characters_loading on every exit path. Early-return cancels
            // previously left the spinner stuck if the user navigated away mid-fetch.
            let result = if token.is_cancelled() {
                None
            } else {
                let r =
                    gw2_api::client::Gw2Client::with_key(&key).and_then(|c| c.fetch_characters());
                if token.is_cancelled() {
                    None
                } else {
                    Some(r)
                }
            };

            // Update the cache *before* taking STATE. That mutex is the render
            // thread's: holding it across a disk write stalls the frame, which
            // in an overlay reads as the game hanging.
            let cache_error = match &result {
                Some(Ok(fresh_chars)) => gw2_api::cache::DataCache::new(&cache_dir)
                    .save_characters(fresh_chars)
                    .err()
                    .map(|e| format!("Loaded characters, but failed to update cache: {}", e)),
                _ => None,
            };

            crate::state::with_state(|s| {
                s.main.characters_loading = false;
                match result {
                    Some(Ok(fresh_chars)) => {
                        if let Some(message) = cache_error {
                            report_cache_write_error(s, message);
                        }

                        // Only update UI if data changed
                        if s.main.characters != fresh_chars {
                            let keep = s
                                .main
                                .selected_character
                                .and_then(|i| s.main.characters.get(i).cloned());
                            s.main.characters = fresh_chars;
                            s.main.selected_character =
                                keep.and_then(|n| s.main.characters.iter().position(|c| *c == n));
                        }
                    }
                    Some(Err(e)) => {
                        // If we had cached data, don't overwrite it with an error
                        if !had_cache {
                            s.main.error = Some(e.to_string());
                        }
                        s.main.api_status = crate::state::ApiStatus::Offline;
                    }
                    None => { /* cancelled — flag reset above */ }
                }
            });
        }));
        if panic_result.is_err() {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                "bg thread panicked: load_characters",
            );
            crate::state::with_state(|s| {
                s.main.characters_loading = false;
            });
        }
    });
    if !spawned {
        // The OS refused the thread (`spawn_worker` logged it). Whatever came
        // from the cache stays on screen; drop the spinner so `render_main`'s
        // 30 s retry can try again instead of waiting on a load that never began.
        state.main.characters_loading = false;
    }
}

/// The tab to show after tabs arrive: the player's pick if it still exists,
/// else the in-game active tab, else nothing.
fn keep_selection(current: Option<usize>, len: usize, active: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(current.filter(|i| *i < len).unwrap_or(active))
    }
}

/// Apply fetched tabs to state: auto-select active tabs, generate chat code, resolve build.
fn apply_character_tabs(
    state: &mut AddonState,
    build_tabs: Vec<gw2_api::models::BuildTab>,
    equipment_tabs: Vec<gw2_api::models::EquipmentTab>,
) {
    // A selection the player already made survives a refresh: the API's
    // background pass used to land seconds after they picked a tab and
    // snap both combos back to the in-game active one (RITUALIST -> HEAL,
    // in-game 2026-09-08). Only a fresh character starts on the active tab.
    let keep = keep_selection;
    let bt_idx = build_tabs.iter().position(|t| t.is_active).unwrap_or(0);
    let et_idx = equipment_tabs.iter().position(|t| t.is_active).unwrap_or(0);
    state.main.build_tabs = build_tabs;
    state.main.equipment_tabs = equipment_tabs;
    state.main.selected_build_tab = keep(
        state.main.selected_build_tab,
        state.main.build_tabs.len(),
        bt_idx,
    );
    state.main.selected_equipment_tab = keep(
        state.main.selected_equipment_tab,
        state.main.equipment_tabs.len(),
        et_idx,
    );
    update_build_chat_code_inner(state);
    resolve_selected_build_inner(state);
}

/// Phase 1: Load character tabs from cache (instant) then refresh from API in background.
pub(super) fn load_character_tabs(state: &mut AddonState, character_name: String) {
    let key = match state.config.gw2_api_key.clone() {
        Some(k) => k,
        None => return,
    };

    state.main.error = None;

    // Phase 1: try loading from cache instantly
    let cache_dir = state.addon_dir.join("cache");
    let cache = gw2_api::cache::DataCache::new(&cache_dir);
    let cached_bt: Option<Vec<gw2_api::models::BuildTab>> = cache
        .load_character(&character_name, "buildtabs")
        .ok()
        .flatten();
    let cached_et: Option<Vec<gw2_api::models::EquipmentTab>> = cache
        .load_character(&character_name, "equiptabs")
        .ok()
        .flatten();

    let had_cache = if let (Some(bt), Some(et)) = (cached_bt, cached_et) {
        // Cache hit — display immediately
        apply_character_tabs(state, bt, et);
        state.main.build_loading = false;
        true
    } else {
        // No cache — show loading indicator
        state.main.build_loading = true;
        false
    };

    // Phase 2: background refresh from API
    let expected_char = character_name.clone();

    let spawned = state.spawn_worker("load-character-tabs", move |token| {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Always reset build_loading on every exit path. Early-return cancels
            // previously left the spinner stuck if the user switched character mid-fetch.
            let result = if token.is_cancelled() {
                None
            } else {
                let r: Result<
                    (
                        Vec<gw2_api::models::BuildTab>,
                        Vec<gw2_api::models::EquipmentTab>,
                    ),
                    String,
                > = (|| {
                    let client =
                        gw2_api::client::Gw2Client::with_key(&key).map_err(|e| e.to_string())?;
                    let build_tabs = client
                        .fetch_build_tabs(&character_name)
                        .map_err(|e| e.to_string())?;
                    let equip_tabs = client
                        .fetch_equipment_tabs(&character_name)
                        .map_err(|e| e.to_string())?;
                    Ok((build_tabs, equip_tabs))
                })();
                if token.is_cancelled() {
                    None
                } else {
                    Some(r)
                }
            };

            // Update the cache *before* taking STATE, for the same reason as
            // `load_characters`: that mutex belongs to the render thread and a
            // worker must not hold it across a disk write. Keyed by character,
            // so this is worth writing even if the player has since switched
            // away and the UI below discards the result.
            let cache_errors: Vec<String> = match &result {
                Some(Ok((fresh_bt, fresh_et))) => {
                    let cache = gw2_api::cache::DataCache::new(&cache_dir);
                    [
                        cache
                            .save_character(&expected_char, "buildtabs", fresh_bt)
                            .err()
                            .map(|e| {
                                format!(
                                    "Loaded build tabs, but failed to update cache for {}: {}",
                                    expected_char, e
                                )
                            }),
                        cache
                            .save_character(&expected_char, "equiptabs", fresh_et)
                            .err()
                            .map(|e| {
                                format!(
                                    "Loaded equipment tabs, but failed to update cache for {}: {}",
                                    expected_char, e
                                )
                            }),
                    ]
                    .into_iter()
                    .flatten()
                    .collect()
                }
                _ => Vec::new(),
            };

            crate::state::with_state(|s| {
                // Always clear the loading flag — covers cancellation and the case where
                // the user switched character (skipping the apply branch below) which
                // previously left the spinner stuck.
                s.main.build_loading = false;
                let Some(result) = result else {
                    return;
                };
                // Only apply if user hasn't switched to a different character
                if s.main
                    .selected_character
                    .and_then(|i| s.main.characters.get(i))
                    .map(|n| n == &expected_char)
                    .unwrap_or(false)
                {
                    match result {
                        Ok((fresh_bt, fresh_et)) => {
                            for message in cache_errors {
                                report_cache_write_error(s, message);
                            }

                            // Compare: only update UI if data actually changed
                            let bt_changed = serde_json::to_string(&s.main.build_tabs).ok()
                                != serde_json::to_string(&fresh_bt).ok();
                            let et_changed = serde_json::to_string(&s.main.equipment_tabs).ok()
                                != serde_json::to_string(&fresh_et).ok();

                            if bt_changed || et_changed {
                                apply_character_tabs(s, fresh_bt, fresh_et);
                            }
                        }
                        Err(e) => {
                            // If we had cached data, don't overwrite with error
                            if !had_cache {
                                s.main.error = Some(e);
                            }
                            s.main.api_status = crate::state::ApiStatus::Offline;
                        }
                    }
                }
            });
        }));
        if panic_result.is_err() {
            nexus::log::log(
                nexus::log::LogLevel::Warning,
                "GW2BuildOpt",
                "bg thread panicked: load_character_tabs",
            );
            crate::state::with_state(|s| {
                s.main.build_loading = false;
            });
        }
    });
    if !spawned {
        // The OS refused the thread (`spawn_worker` logged it). Cached tabs, if
        // there were any, are already applied; drop the spinner so the UI is not
        // stuck waiting on a refresh that never started.
        state.main.build_loading = false;
    }
}

/// Update build chat code from currently selected build tab.
pub(super) fn update_build_chat_code(state: &mut AddonState) {
    update_build_chat_code_inner(state);
}

fn update_build_chat_code_inner(state: &mut AddonState) {
    let weapons = current_weapon_types(state);
    let build_tab = state
        .main
        .selected_build_tab
        .and_then(|i| state.main.build_tabs.get(i));
    let game_db = state.main.game_db.as_ref();

    if let (Some(bt), Some(db)) = (build_tab, game_db) {
        state.main.build_chat_code = generate_build_chat_code(&bt.build, db, &weapons);
    } else {
        state.main.build_chat_code = None;
    }
}

fn current_weapon_types(state: &AddonState) -> Vec<String> {
    let Some(build) = state.main.current_build.as_ref() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for set in &build.weapons {
        if let Some(w) = &set.main_hand {
            names.push(w.weapon_type.clone());
        }
        if let Some(w) = &set.off_hand {
            names.push(w.weapon_type.clone());
        }
    }
    names
}

/// Generate GW2 build template chat code from a Build.
/// Format: 0x0D + profession_code(1) + 3x(spec_id(1) + trait_bits(1)) + 10x skill_palette(2 LE)
/// + 16 bytes profession-specific + SotO weapon list + skill-override count → [&...]
pub(in crate::ui::main_view) fn generate_build_chat_code(
    build: &gw2_api::models::Build,
    db: &gw2_optimizer::gamedb::GameDb,
    weapons: &[String],
) -> Option<String> {
    let profession_name = build.profession.as_deref()?;
    let profession = db.profession(profession_name)?;
    let code = profession.code?;
    if code > 255 {
        return None;
    }
    let profession_code = code as u8;

    let mut specs = [gw2_optimizer::build_template::TemplateSpec::default(); 3];
    for (i, spec) in specs.iter_mut().enumerate() {
        let Some(sel) = build.specializations.get(i) else {
            continue;
        };
        let Some(spec_id) = sel.id else {
            continue;
        };
        if spec_id > 255 {
            return None;
        }
        spec.id = spec_id;
        // Bits: 00CCBBAA where AA = col0, BB = col1, CC = col2
        if let Some(spec_data) = db.spec(spec_id) {
            for (col, trait_id) in sel.traits.iter().enumerate().take(3) {
                let Some(tid) = trait_id else {
                    continue;
                };
                let col_start = col * 3;
                if let Some(pos) = spec_data
                    .major_traits
                    .iter()
                    .skip(col_start)
                    .take(3)
                    .position(|&mt| mt == *tid)
                {
                    spec.choices[col] = (pos as u8 + 1) & 0x03;
                }
            }
        }
    }

    // 5 terrestrial skills as palette IDs (u16 LE): heal, util1, util2, util3, elite
    // Interleaved with 5 aquatic skills
    let terrestrial_skills = build
        .skills
        .as_ref()
        .map(|sk| {
            let mut ids = vec![sk.heal.unwrap_or(0)];
            for u in sk.utilities.iter().take(3) {
                ids.push(u.unwrap_or(0));
            }
            while ids.len() < 4 {
                ids.push(0);
            }
            ids.push(sk.elite.unwrap_or(0));
            ids
        })
        .unwrap_or_else(|| vec![0; 5]);

    let aquatic_skills = build
        .aquatic_skills
        .as_ref()
        .map(|sk| {
            let mut ids = vec![sk.heal.unwrap_or(0)];
            for u in sk.utilities.iter().take(3) {
                ids.push(u.unwrap_or(0));
            }
            while ids.len() < 4 {
                ids.push(0);
            }
            ids.push(sk.elite.unwrap_or(0));
            ids
        })
        .unwrap_or_else(|| vec![0; 5]);

    let mut skills = [0u32; 5];
    let mut aquatic = [0u32; 5];
    for i in 0..5 {
        let t_skill = terrestrial_skills.get(i).copied().unwrap_or(0);
        skills[i] = db.skill_palette_id(t_skill);
        let a_skill = aquatic_skills.get(i).copied().unwrap_or(0);
        // Revenant legend palettes are valid on land and water. Live Herald
        // codes copy the land bar; leaving water empty is legal (zeros are
        // empty slots, not a skill) but does not match what the game copies.
        aquatic[i] = if a_skill == 0 && profession_name.eq_ignore_ascii_case("Revenant") {
            skills[i]
        } else {
            db.skill_palette_id(a_skill)
        };
    }

    let mut profession_bytes = [0u8; 16];
    match profession_name {
        "Ranger" => {
            if let Some(ref pets) = build.pets {
                for (i, pet) in pets.terrestrial.iter().take(2).enumerate() {
                    profession_bytes[i] = pet.unwrap_or(0) as u8;
                }
                for (i, pet) in pets.aquatic.iter().take(2).enumerate() {
                    profession_bytes[2 + i] = pet.unwrap_or(0) as u8;
                }
            }
        }
        "Revenant" => {
            encode_revenant_profession_bytes(&mut profession_bytes, build, db);
        }
        _ => {}
    }

    let template = gw2_optimizer::build_template::BuildTemplate {
        profession: u32::from(profession_code),
        specs,
        skills,
        aquatic,
        profession_bytes,
        weapons: soto_weapon_ids(weapons),
        skill_overrides: vec![],
    };
    if template.same_realm_duplicate() {
        return None;
    }
    Some(gw2_optimizer::build_template::encode(&template))
}

fn encode_revenant_profession_bytes(
    buf: &mut [u8; 16],
    build: &gw2_api::models::Build,
    db: &gw2_optimizer::gamedb::GameDb,
) {
    let spec_ids: Vec<u32> = build.specializations.iter().filter_map(|s| s.id).collect();
    let mut legends: Vec<String> = build
        .legends
        .iter()
        .flatten()
        .filter(|&id| !id.is_empty())
        .cloned()
        .collect();
    if legends.is_empty() {
        legends = infer_revenant_legends(build, db, &spec_ids);
    }
    let mut aquatic: Vec<String> = build
        .aquatic_legends
        .iter()
        .flatten()
        .filter(|&id| !id.is_empty())
        .cloned()
        .collect();
    if aquatic.is_empty() {
        aquatic = legends.clone();
    }

    let legend_byte =
        |id: Option<&String>| -> u8 { id.map(|s| db.legend_template_code(s)).unwrap_or(0) };
    buf[0] = legend_byte(legends.first());
    buf[1] = legend_byte(legends.get(1));
    buf[2] = legend_byte(aquatic.first());
    buf[3] = legend_byte(aquatic.get(1));

    // Inactive legend's 3 terrestrial + 3 aquatic utility palettes (u16 LE).
    let inactive_land = legends.get(1).or(legends.first());
    let inactive_water = aquatic.get(1).or(aquatic.first());
    let mut at = 4;
    for legend_id in [inactive_land, inactive_water] {
        let utils: Vec<u32> = legend_id
            .and_then(|id| db.legends.get(id))
            .map(|l| l.utilities.clone())
            .unwrap_or_default();
        for i in 0..3 {
            let pal = utils
                .get(i)
                .copied()
                .map(|sid| db.skill_palette_id(sid) as u16)
                .unwrap_or(0);
            buf[at..at + 2].copy_from_slice(&pal.to_le_bytes());
            at += 2;
        }
    }
}

fn infer_revenant_legends(
    build: &gw2_api::models::Build,
    db: &gw2_optimizer::gamedb::GameDb,
    spec_ids: &[u32],
) -> Vec<String> {
    let heal = build.skills.as_ref().and_then(|s| s.heal);
    let mut ids: Vec<String> = Vec::new();
    if let Some(heal_id) = heal {
        if let Some((id, _)) = db
            .legends
            .iter()
            .find(|(_, l)| l.heal == heal_id && db.legend_available(&l.id, spec_ids))
        {
            ids.push(id.clone());
        }
    }
    let mut rest: Vec<(u8, String)> = db
        .legends
        .keys()
        .filter(|id| !ids.contains(id) && db.legend_available(id, spec_ids))
        .map(|id| (db.legend_template_code(id), id.clone()))
        .collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, id) in rest {
        if ids.len() >= 2 {
            break;
        }
        ids.push(id);
    }
    ids
}

fn soto_weapon_ids(weapons: &[String]) -> Vec<u16> {
    let mut ids: Vec<u16> = Vec::new();
    for name in weapons {
        let Some(id) = weapon_type_id(name) else {
            continue;
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.truncate(8);
    ids
}

fn weapon_type_id(name: &str) -> Option<u16> {
    Some(match gw2_core::i18n::weapon_type_key(name).as_str() {
        "axe" => 5,
        "longbow" => 35,
        "dagger" => 47,
        "focus" => 49,
        "greatsword" => 50,
        "hammer" => 51,
        "mace" => 53,
        "pistol" => 54,
        "rifle" => 85,
        "scepter" => 86,
        "shield" => 87,
        "staff" => 89,
        "sword" => 90,
        "torch" => 102,
        "warhorn" => 103,
        "shortbow" => 107,
        // Land spear (Janthir). Type 265 in the SotO trailer is how GW2 encodes it.
        // Trident / Speargun stay aquatic-only and must not appear here.
        "spear" => 265,
        "trident" | "speargun" => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use gw2_api::models::{Build, PetSelection, Profession, SkillSelection};
    use gw2_optimizer::gamedb::GameDb;
    use std::collections::HashMap;

    #[test]
    fn a_refresh_keeps_the_players_tab_pick() {
        use super::keep_selection;
        assert_eq!(keep_selection(Some(1), 4, 0), Some(1));
        assert_eq!(keep_selection(None, 4, 0), Some(0));
        assert_eq!(keep_selection(Some(7), 4, 2), Some(2));
        assert_eq!(keep_selection(Some(1), 0, 0), None);
    }

    #[test]
    fn weapon_type_id_accepts_item_api_spellings() {
        assert_eq!(weapon_type_id("Shortbow"), Some(107));
        assert_eq!(weapon_type_id("ShortBow"), Some(107));
        assert_eq!(weapon_type_id("Short Bow"), Some(107));
        assert_eq!(weapon_type_id("LongBow"), Some(35));
        assert_eq!(weapon_type_id("Harpoon"), Some(265));
        assert_eq!(weapon_type_id("Spear"), Some(265));
        assert_eq!(weapon_type_id("HarpoonGun"), None);
        assert_eq!(weapon_type_id("Trident"), None);
    }

    fn revenant_db() -> GameDb {
        let mut db = GameDb::empty_for_tests();
        db.professions.insert(
            "Revenant".into(),
            Profession {
                id: "Revenant".into(),
                name: "Revenant".into(),
                code: Some(9),
                specializations: vec![],
                weapons: HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        db.legends.insert(
            "Legend1".into(),
            gw2_api::models::Legend {
                id: "Legend1".into(),
                code: Some(1),
                swap: 28085,
                heal: 27220,
                elite: 27760,
                utilities: vec![28379, 27014, 26644],
            },
        );
        db.legends.insert(
            "Legend2".into(),
            gw2_api::models::Legend {
                id: "Legend2".into(),
                code: Some(2),
                swap: 28134,
                heal: 26937,
                elite: 28472,
                utilities: vec![27025, 26679, 27322],
            },
        );
        db.legends.insert(
            "Legend8".into(),
            gw2_api::models::Legend {
                id: "Legend8".into(),
                code: Some(8),
                swap: 76610,
                heal: 77043,
                elite: 76968,
                utilities: vec![77243, 77291, 76805],
            },
        );
        // Only the latest legend is in skills_by_palette — older stances share these.
        db.skill_to_palette.insert(77043, 4572);
        db.skill_to_palette.insert(76968, 4554);
        db.skill_to_palette.insert(77243, 4614);
        db.skill_to_palette.insert(77291, 4651);
        db.skill_to_palette.insert(76805, 4564);
        db
    }

    fn decode_template(code: &str) -> Vec<u8> {
        let inner = code
            .strip_prefix("[&")
            .and_then(|s| s.strip_suffix("]"))
            .expect("[&...] wrapper");
        base64::engine::general_purpose::STANDARD
            .decode(inner)
            .expect("base64")
    }

    fn u16_at(buf: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([buf[offset], buf[offset + 1]])
    }

    #[test]
    fn revenant_older_legend_encodes_shared_palettes_and_stance_bytes() {
        let db = revenant_db();
        let build = Build {
            name: None,
            profession: Some("Revenant".into()),
            specializations: vec![],
            skills: Some(SkillSelection {
                heal: Some(27220),
                utilities: vec![Some(28379), Some(27014), Some(26644)],
                elite: Some(27760),
            }),
            aquatic_skills: None,
            legends: vec![Some("Legend1".into()), Some("Legend2".into())],
            aquatic_legends: vec![],
            pets: None,
        };
        let code = generate_build_chat_code(&build, &db, &[]).expect("encode");
        let buf = decode_template(&code);
        assert_eq!(buf[0], 0x0D);
        assert_eq!(buf[1], 9);
        // Land heal / util / elite palettes (even indices in the 10×u16 block).
        assert_eq!(u16_at(&buf, 8), 4572, "Legend1 heal shares Conduit palette");
        assert_eq!(u16_at(&buf, 10), 4572, "Revenant water copies land");
        assert_eq!(u16_at(&buf, 12), 4614);
        assert_eq!(u16_at(&buf, 14), 4614);
        assert_eq!(u16_at(&buf, 16), 4651);
        assert_eq!(u16_at(&buf, 18), 4651);
        assert_eq!(u16_at(&buf, 20), 4564);
        assert_eq!(u16_at(&buf, 22), 4564);
        assert_eq!(u16_at(&buf, 24), 4554);
        assert_eq!(u16_at(&buf, 26), 4554);
        assert_eq!(
            &buf[28..32],
            &[1, 2, 1, 2],
            "legend codes active/inactive × land/water"
        );
        // Inactive legend utility palettes (also shared).
        assert_eq!(u16_at(&buf, 32), 4614);
        assert_eq!(u16_at(&buf, 34), 4651);
        assert_eq!(u16_at(&buf, 36), 4564);
    }

    #[test]
    fn ranger_pets_pad_to_four_bytes() {
        let mut db = GameDb::empty_for_tests();
        db.professions.insert(
            "Ranger".into(),
            Profession {
                id: "Ranger".into(),
                name: "Ranger".into(),
                code: Some(4),
                specializations: vec![],
                weapons: HashMap::new(),
                training: vec![],
                skills_by_palette: vec![],
                icon: None,
                icon_big: None,
            },
        );
        let build = Build {
            name: None,
            profession: Some("Ranger".into()),
            specializations: vec![],
            skills: None,
            aquatic_skills: None,
            legends: vec![],
            aquatic_legends: vec![],
            pets: Some(PetSelection {
                terrestrial: vec![Some(31)],
                aquatic: vec![Some(7), Some(8)],
            }),
        };
        let buf = decode_template(&generate_build_chat_code(&build, &db, &[]).unwrap());
        assert_eq!(&buf[28..32], &[31, 0, 7, 8]);
        assert_eq!(buf.len(), 44);
    }

    #[test]
    fn land_spear_encodes_in_soto_trailer_trident_does_not() {
        let db = revenant_db();
        let build = Build {
            name: None,
            profession: Some("Revenant".into()),
            specializations: vec![],
            skills: None,
            aquatic_skills: None,
            legends: vec![],
            aquatic_legends: vec![],
            pets: None,
        };
        let spear_only =
            decode_template(&generate_build_chat_code(&build, &db, &["Spear".into()]).unwrap());
        assert!(
            spear_only.len() > 44,
            "land spear must use the SotO trailer"
        );
        let rest = &spear_only[44..];
        assert_eq!(rest[0], 1);
        assert_eq!(u16_at(rest, 1), 265);
        assert_eq!(*rest.last().unwrap(), 0);

        let trident_only =
            decode_template(&generate_build_chat_code(&build, &db, &["Trident".into()]).unwrap());
        assert_eq!(trident_only.len(), 44, "trident must stay aquatic-only");

        let buf = decode_template(
            &generate_build_chat_code(
                &build,
                &db,
                &["Staff".into(), "Spear".into(), "Trident".into()],
            )
            .unwrap(),
        );
        assert!(buf.len() > 44);
        let rest = &buf[44..];
        assert_eq!(rest[0], 2, "Staff + land Spear; not Trident");
        assert_eq!(u16_at(rest, 1), 89);
        assert_eq!(u16_at(rest, 3), 265);
        assert_eq!(*rest.last().unwrap(), 0);
    }
}
