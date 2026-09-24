use super::stats::compute_3tier_combat;
use crate::state::{AddonState, MainTab};

/// CombatBundle type alias for stat + 3-tier combat metrics.
type CombatBundle = (
    gw2_core::types::StatBlock,
    Option<gw2_core::types::CombatMetrics>,
    Option<gw2_core::types::CombatMetrics>,
    Option<gw2_core::types::CombatMetrics>,
);

/// Phase 2: Resolve build from currently selected tabs. Called on tab change or game mode change.
pub(super) fn resolve_selected_build(state: &mut AddonState) {
    resolve_selected_build_inner(state);
}

pub(super) fn resolve_selected_build_inner(state: &mut AddonState) {
    let build_tab = state
        .main
        .selected_build_tab
        .and_then(|i| state.main.build_tabs.get(i))
        .cloned();
    let equip_tab = state
        .main
        .selected_equipment_tab
        .and_then(|i| state.main.equipment_tabs.get(i))
        .cloned();

    let (Some(bt), Some(et)) = (build_tab, equip_tab) else {
        state.main.build_loading = false;
        return;
    };

    // GameDb required — if not loaded yet, skip; load_game_db() will trigger resolve when ready
    let Some(ref db) = state.main.game_db else {
        return;
    };

    let game_mode = state.main.game_mode.clone();
    let char_name = state
        .main
        .selected_character
        .and_then(|i| state.main.characters.get(i).cloned())
        .unwrap_or_default();

    // Synchronous resolve — all lookups are O(1) HashMap hits on the in-memory GameDb
    state.main.build_loading = false;
    state.main.error = None;

    match resolve_build_from_db(&char_name, &bt.build, &et, db, &game_mode) {
        Ok(build) => {
            // Auto-populate locks from current build in Improve mode
            if state.main.active_tab == MainTab::Improve {
                auto_populate_locks(&build, &mut state.main.build_locks);
            }
            // Damage modifiers from the engine's stat sheet, read off the
            // same validated plate the Improve baseline ranks, so the
            // current build's combat line cannot price a trait the engine
            // prices differently.
            let plate = super::optimize_flow::baseline_plate_from_loadout(&build);
            let validated =
                gw2_optimizer::validation::validate_gemini_build(&plate, db, &build.profession);
            let (_, modifiers) = gw2_optimizer::engine::calculate_validated_stats(
                &validated,
                db,
                &build.profession,
                &gw2_optimizer::balance::BalanceContext::new(game_mode.clone()),
            );
            state.main.current_build = Some(build);
            match calculate_current_stats_from_db(&bt.build, &et, db, &game_mode, &modifiers) {
                Ok((stats, combat_solo, combat_party, combat_squad)) => {
                    state.main.current_stats = Some(stats);
                    state.main.comparison.current_combat_solo = combat_solo;
                    state.main.comparison.current_combat_party = combat_party;
                    state.main.comparison.current_combat_squad = combat_squad;
                }
                Err(_) => {
                    // Clear stats AND combat metrics together — stale combat data from
                    // a previous build would be shown alongside the new current_build.
                    state.main.current_stats = None;
                    state.main.comparison.current_combat_solo = None;
                    state.main.comparison.current_combat_party = None;
                    state.main.comparison.current_combat_squad = None;
                }
            }
        }
        Err(e) => {
            state.main.clear_resolved_view();
            state.main.error = Some(e);
        }
    }
}

/// Auto-populate BuildLocks from the current resolved build.
/// Locks only the elite specialization slot (slot 2) so the optimizer preserves the
/// profession identity. Core specs and all traits remain unlocked by default.
pub(in crate::ui::main_view) fn auto_populate_locks(
    build: &gw2_core::types::ResolvedBuild,
    locks: &mut gw2_core::types::BuildLocks,
) {
    // Start with everything unlocked
    locks.specs = [None; 3];
    locks.trait_locks.clear();

    // Only lock the elite specialization (slot 2) — traits remain unlocked
    if let Some(spec) = build.specializations.get(2) {
        if spec.id != 0 {
            locks.specs[2] = Some(spec.id);
        }
    }
}

/// Resolve the current build using the in-memory GameDb (O(1) lookups, zero disk I/O).
fn resolve_build_from_db(
    character_name: &str,
    build: &gw2_api::models::Build,
    equipment: &gw2_api::models::EquipmentTab,
    db: &gw2_optimizer::gamedb::GameDb,
    game_mode: &gw2_core::types::GameMode,
) -> Result<gw2_core::types::ResolvedBuild, String> {
    use gw2_core::types::*;

    let resolved_specs = resolve_specs_db(build, db);
    let resolved_skills = resolve_skills_db(build, db);
    let (legends, pets) = resolve_profession_extras(build, db);
    let (weapons, armor, trinkets_vec, rune, relic_resolved) = resolve_equipment_db(equipment, db);
    let pvp_amulet = resolve_pvp_amulet_db(game_mode, equipment, db);

    Ok(ResolvedBuild {
        character_name: character_name.to_string(),
        profession: build.profession.clone().unwrap_or_default(),
        game_mode: game_mode.clone(),
        specializations: resolved_specs,
        skills: resolved_skills,
        legends,
        pets,
        weapons,
        armor,
        trinkets: trinkets_vec,
        relic: relic_resolved,
        rune,
        pvp_amulet,
    })
}

/// Calculate current stats using the in-memory GameDb (O(1) lookups, zero disk I/O).
fn calculate_current_stats_from_db(
    build: &gw2_api::models::Build,
    equipment: &gw2_api::models::EquipmentTab,
    db: &gw2_optimizer::gamedb::GameDb,
    game_mode: &gw2_core::types::GameMode,
    modifiers: &gw2_optimizer::combat::DamageModifiers,
) -> Result<CombatBundle, String> {
    let profession = build.profession.clone().unwrap_or_default();

    // PvP mode: stats come from amulet (O(1) lookup in db.pvp_amulets)
    let balance_ctx = gw2_optimizer::balance::BalanceContext::new(game_mode.clone());
    if *game_mode == gw2_core::types::GameMode::PvP {
        if let Some(ref pvp) = equipment.equipment_pvp {
            if let Some(amulet_id) = pvp.amulet {
                if let Some(amulet) = db.pvp_amulets.get(&amulet_id) {
                    let opt_stats = gw2_optimizer::stats::calculate_pvp_stats(&amulet.attributes);
                    let derived = gw2_optimizer::stats::compute_derived(&opt_stats, &profession);
                    let stats = opt_stats_to_stat_block(&opt_stats, &derived);
                    let (solo, party, squad) = compute_3tier_combat(
                        &opt_stats,
                        &derived,
                        modifiers,
                        &profession,
                        &balance_ctx,
                    );
                    return Ok((stats, solo, party, squad));
                }
            }
        }
    }

    // PvE/WvW: collect equipped trait IDs (major + minor) via O(1) lookups
    let mut equipped_trait_ids = Vec::new();
    for spec_sel in &build.specializations {
        for &trait_id in &spec_sel.traits {
            if let Some(tid) = trait_id {
                equipped_trait_ids.push(tid);
            }
        }
        if let Some(spec_id) = spec_sel.id {
            if let Some(spec) = db.specializations.get(&spec_id) {
                equipped_trait_ids.extend(&spec.minor_traits);
            }
        }
    }

    // Find rune/sigil IDs from equipment upgrades (O(1) item lookups)
    let rune_id = equipment
        .equipment
        .iter()
        .flat_map(|p| p.upgrades.iter())
        .find_map(|&uid| {
            db.items.get(&uid).and_then(|item| {
                item.details.as_ref().and_then(|d| {
                    if d.detail_type.as_deref() == Some("Rune") {
                        Some(uid)
                    } else {
                        None
                    }
                })
            })
        });

    let sigil_ids: Vec<u32> = equipment
        .equipment
        .iter()
        .flat_map(|p| p.upgrades.iter())
        .filter_map(|&uid| {
            db.items.get(&uid).and_then(|item| {
                item.details.as_ref().and_then(|d| {
                    if d.detail_type.as_deref() == Some("Sigil") {
                        Some(uid)
                    } else {
                        None
                    }
                })
            })
        })
        .collect();

    // Pass GameDb's pre-indexed HashMaps directly — no copying needed
    let (opt_stats, derived) = gw2_optimizer::stats::calculate_full_stats(
        equipment,
        &equipped_trait_ids,
        rune_id,
        &sigil_ids,
        &profession,
        &db.items,
        &db.itemstats,
        &db.traits,
        game_mode,
    );

    let (combat_solo, combat_party, combat_squad) =
        compute_3tier_combat(&opt_stats, &derived, modifiers, &profession, &balance_ctx);

    Ok((
        opt_stats_to_stat_block(&opt_stats, &derived),
        combat_solo,
        combat_party,
        combat_squad,
    ))
}

/// Convert optimizer StatBlock + DerivedStats to display StatBlock.
fn opt_stats_to_stat_block(
    opt_stats: &gw2_optimizer::stats::StatBlock,
    derived: &gw2_optimizer::stats::DerivedStats,
) -> gw2_core::types::StatBlock {
    gw2_core::types::StatBlock {
        power: opt_stats.power.round() as i32,
        precision: opt_stats.precision.round() as i32,
        toughness: opt_stats.toughness.round() as i32,
        vitality: opt_stats.vitality.round() as i32,
        condition_damage: opt_stats.condition_damage.round() as i32,
        expertise: opt_stats.expertise.round() as i32,
        concentration: opt_stats.concentration.round() as i32,
        ferocity: opt_stats.ferocity.round() as i32,
        healing_power: opt_stats.healing_power.round() as i32,
        crit_chance: derived.crit_chance,
        crit_damage: derived.crit_damage,
        health: derived.health.round() as i32,
        armor: derived.armor.round() as i32,
    }
}

/// Resolve specializations using GameDb O(1) lookups.
fn resolve_specs_db(
    build: &gw2_api::models::Build,
    db: &gw2_optimizer::gamedb::GameDb,
) -> Vec<gw2_core::types::ResolvedSpec> {
    use gw2_core::types::*;
    build
        .specializations
        .iter()
        .filter_map(|sel| {
            let spec_id = sel.id?;
            let spec = db.specializations.get(&spec_id)?;
            let traits_selected: Vec<ResolvedTrait> = sel
                .traits
                .iter()
                .enumerate()
                .filter_map(|(col, trait_id)| {
                    let tid = (*trait_id)?;
                    let t = db.traits.get(&tid)?;
                    Some(ResolvedTrait {
                        id: t.id,
                        name: t.name.clone(),
                        description: t.description.clone().unwrap_or_default(),
                        column: col,
                        selected: true,
                    })
                })
                .collect();
            Some(ResolvedSpec {
                id: spec.id,
                name: spec.name.clone(),
                elite: spec.elite,
                traits_selected,
                traits_available: Vec::new(),
            })
        })
        .collect()
}

/// Resolve skills using GameDb O(1) lookups.
fn resolve_skills_db(
    build: &gw2_api::models::Build,
    db: &gw2_optimizer::gamedb::GameDb,
) -> gw2_core::types::ResolvedSkills {
    use gw2_core::types::*;
    let find_skill = |id: u32| -> Option<SkillInfo> {
        db.skills.get(&id).map(|s| SkillInfo {
            id: s.id,
            name: s.name.clone(),
        })
    };
    if let Some(ref sk) = build.skills {
        ResolvedSkills {
            heal: sk.heal.and_then(&find_skill),
            utilities: sk
                .utilities
                .iter()
                .map(|id| id.and_then(&find_skill))
                .collect(),
            elite: sk.elite.and_then(&find_skill),
        }
    } else {
        ResolvedSkills::default()
    }
}

fn resolve_profession_extras(
    build: &gw2_api::models::Build,
    db: &gw2_optimizer::gamedb::GameDb,
) -> (Vec<String>, Vec<String>) {
    let legends = build
        .legends
        .iter()
        .flatten()
        .filter(|id| !id.is_empty())
        .map(|id| {
            db.legends
                .get(id)
                .and_then(|l| db.skills.get(&l.swap))
                .map(|s| crate::ui::comparison::compact_stance_name(&s.name))
                .unwrap_or_else(|| id.clone())
        })
        .collect();
    let mut pets = Vec::new();
    if let Some(ref p) = build.pets {
        for id in p.terrestrial.iter().flatten() {
            pets.push(db.pet_display_name(*id));
        }
    }
    (legends, pets)
}

/// Resolved equipment bundle: (weapon sets, armor, trinkets, rune, relic).
type ResolvedEquipment = (
    Vec<gw2_core::types::ResolvedWeaponSet>,
    Vec<gw2_core::types::ResolvedGearPiece>,
    Vec<gw2_core::types::ResolvedGearPiece>,
    Option<gw2_core::types::ResolvedUpgrade>,
    Option<gw2_core::types::ResolvedRelic>,
);

/// Resolve equipment using GameDb O(1) lookups.
fn resolve_equipment_db(
    equipment: &gw2_api::models::EquipmentTab,
    db: &gw2_optimizer::gamedb::GameDb,
) -> ResolvedEquipment {
    use gw2_core::types::*;

    let mut armor = Vec::new();
    let mut trinkets_vec = Vec::new();
    let mut rune = None;
    let mut relic_resolved = None;
    let mut ws1 = ResolvedWeaponSet {
        label: "Set 1".into(),
        ..Default::default()
    };
    let mut ws2 = ResolvedWeaponSet {
        label: "Set 2".into(),
        ..Default::default()
    };

    for piece in &equipment.equipment {
        let item = db.items.get(&piece.id);
        let item_name = item
            .map(|i| i.name.clone())
            .unwrap_or_else(|| format!("#{}", piece.id));
        let stat_prefix = piece
            .stats
            .as_ref()
            .and_then(|s| db.itemstats.get(&s.id).map(|is| is.name.clone()))
            .unwrap_or_default();

        let extract_sigils = |piece: &gw2_api::models::EquipmentPiece,
                              ws: &mut ResolvedWeaponSet| {
            for &uid in &piece.upgrades {
                if let Some(u) = db.items.get(&uid) {
                    ws.sigils.push(UpgradeInfo {
                        id: uid,
                        name: u.name.clone(),
                    });
                }
            }
        };

        match piece.slot.as_str() {
            "WeaponA1" => {
                ws1.main_hand = Some(WeaponInfo {
                    name: item_name,
                    weapon_type: item
                        .and_then(|i| i.details.as_ref()?.detail_type.as_deref())
                        .map(gw2_core::i18n::canonical_weapon_type)
                        .unwrap_or_default(),
                    id: piece.id,
                });
                if ws1.stat_prefix.is_empty() {
                    ws1.stat_prefix = stat_prefix;
                }
                extract_sigils(piece, &mut ws1);
            }
            "WeaponA2" => {
                ws1.off_hand = Some(WeaponInfo {
                    name: item_name,
                    weapon_type: item
                        .and_then(|i| i.details.as_ref()?.detail_type.as_deref())
                        .map(gw2_core::i18n::canonical_weapon_type)
                        .unwrap_or_default(),
                    id: piece.id,
                });
                if ws1.stat_prefix.is_empty() {
                    ws1.stat_prefix = stat_prefix;
                }
                extract_sigils(piece, &mut ws1);
            }
            "WeaponB1" => {
                ws2.main_hand = Some(WeaponInfo {
                    name: item_name,
                    weapon_type: item
                        .and_then(|i| i.details.as_ref()?.detail_type.as_deref())
                        .map(gw2_core::i18n::canonical_weapon_type)
                        .unwrap_or_default(),
                    id: piece.id,
                });
                if ws2.stat_prefix.is_empty() {
                    ws2.stat_prefix = stat_prefix;
                }
                extract_sigils(piece, &mut ws2);
            }
            "WeaponB2" => {
                ws2.off_hand = Some(WeaponInfo {
                    name: item_name,
                    weapon_type: item
                        .and_then(|i| i.details.as_ref()?.detail_type.as_deref())
                        .map(gw2_core::i18n::canonical_weapon_type)
                        .unwrap_or_default(),
                    id: piece.id,
                });
                if ws2.stat_prefix.is_empty() {
                    ws2.stat_prefix = stat_prefix;
                }
                extract_sigils(piece, &mut ws2);
            }
            "Helm" | "Shoulders" | "Coat" | "Gloves" | "Leggings" | "Boots" => {
                if rune.is_none() {
                    if let Some(&uid) = piece.upgrades.first() {
                        if let Some(u) = db.items.get(&uid) {
                            rune = Some(ResolvedUpgrade {
                                id: uid,
                                name: u.name.clone(),
                            });
                        }
                    }
                }
                armor.push(ResolvedGearPiece {
                    slot: piece.slot.clone(),
                    name: item_name,
                    stat_prefix,
                    infusions: Vec::new(),
                    id: piece.id,
                });
            }
            "Backpack" | "Accessory1" | "Accessory2" | "Amulet" | "Ring1" | "Ring2" => {
                trinkets_vec.push(ResolvedGearPiece {
                    slot: piece.slot.clone(),
                    name: item_name,
                    stat_prefix,
                    infusions: Vec::new(),
                    id: piece.id,
                });
            }
            "Relic" => {
                relic_resolved = Some(ResolvedRelic {
                    id: piece.id,
                    name: item_name,
                    description: item.and_then(|i| i.description.clone()).unwrap_or_default(),
                });
            }
            _ => {}
        }
    }

    let mut weapons = Vec::new();
    if ws1.main_hand.is_some() || ws1.off_hand.is_some() {
        weapons.push(ws1);
    }
    if ws2.main_hand.is_some() || ws2.off_hand.is_some() {
        weapons.push(ws2);
    }

    (weapons, armor, trinkets_vec, rune, relic_resolved)
}

/// Resolve PvP amulet using GameDb O(1) lookup.
fn resolve_pvp_amulet_db(
    game_mode: &gw2_core::types::GameMode,
    equipment: &gw2_api::models::EquipmentTab,
    db: &gw2_optimizer::gamedb::GameDb,
) -> Option<gw2_core::types::ResolvedPvpAmulet> {
    use gw2_core::types::*;
    if *game_mode != GameMode::PvP {
        return None;
    }
    let pvp_eq = equipment.equipment_pvp.as_ref()?;
    let amulet_id = pvp_eq.amulet?;
    db.pvp_amulets.get(&amulet_id).map(|a| ResolvedPvpAmulet {
        id: a.id,
        name: a.name.clone(),
        stats: a.attributes.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    })
}
