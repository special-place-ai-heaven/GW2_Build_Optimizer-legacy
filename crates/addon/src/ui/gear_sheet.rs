//! One-page shopping-list loadout: prefix on every slot, nested upgrades, icons.

use gw2_core::i18n::{loc_weapon_types, t};
use gw2_core::types::{BuildLocks, CombatMetrics, GearSlot, ResolvedBuild};
use gw2_optimizer::gamedb::GameDb;
use nexus::imgui::{MouseButton, Ui};

use super::comparison::BuildSuggestion;
use super::gear_diff::parse_suggestion_weapons;
use super::theme;
use super::{comparison, icons};

/// Same cyan as a selected trait (`Blood Moon`). Locked gear uses this, not gold.
const LOCKED_TEXT: [f32; 4] = [0.5, 0.8, 1.0, 1.0];
/// Same grey as an unselected trait (`Druidic Clarity`).
const UNLOCKED_TEXT: [f32; 4] = [0.35, 0.35, 0.35, 0.8];

const ARMOR_SLOTS: [&str; 6] = ["Helm", "Shoulders", "Coat", "Gloves", "Leggings", "Boots"];
const TRINKET_SLOTS: [&str; 6] = [
    "Backpack",
    "Accessory1",
    "Accessory2",
    "Amulet",
    "Ring1",
    "Ring2",
];

/// Resolved-build piece slot string → canonical gear slot (armor + trinkets).
pub fn piece_gear_slot(slot: &str) -> Option<GearSlot> {
    match slot {
        "Helm" => Some(GearSlot::Helm),
        "Shoulders" => Some(GearSlot::Shoulders),
        "Coat" => Some(GearSlot::Coat),
        "Gloves" => Some(GearSlot::Gloves),
        "Leggings" => Some(GearSlot::Leggings),
        "Boots" => Some(GearSlot::Boots),
        "Backpack" => Some(GearSlot::Back),
        "Accessory1" => Some(GearSlot::Accessory1),
        "Accessory2" => Some(GearSlot::Accessory2),
        "Amulet" => Some(GearSlot::Amulet),
        "Ring1" => Some(GearSlot::Ring1),
        "Ring2" => Some(GearSlot::Ring2),
        _ => None,
    }
}

/// Click target for one visual gear row. Relic has no `GearSlot` and stays None.
struct RowLock<'a> {
    locks: &'a mut BuildLocks,
    db: &'a GameDb,
    slots: &'a [GearSlot],
}

/// True when every listed slot is pinned. An empty list is never locked.
pub(crate) fn gear_row_locked(locks: &BuildLocks, slots: &[GearSlot]) -> bool {
    !slots.is_empty() && slots.iter().all(|s| locks.gear_locks.contains_key(s))
}

fn apply_gear_row_toggle(locks: &mut BuildLocks, slots: &[GearSlot], id: Option<u32>) {
    if slots.is_empty() {
        return;
    }
    if gear_row_locked(locks, slots) {
        for s in slots {
            locks.gear_locks.remove(s);
        }
        return;
    }
    let Some(id) = id else {
        return;
    };
    for s in slots {
        locks.gear_locks.insert(*s, id);
    }
}

pub(crate) fn toggle_gear_row(
    locks: &mut BuildLocks,
    db: &GameDb,
    slots: &[GearSlot],
    prefix: &str,
) {
    apply_gear_row_toggle(locks, slots, db.itemstat_by_name(prefix).map(|s| s.id));
}

fn weapon_lock_slots(set_idx: usize, has_main: bool, has_off: bool) -> Vec<GearSlot> {
    let (main, off) = if set_idx == 0 {
        (GearSlot::WeaponSet1Main, GearSlot::WeaponSet1Off)
    } else {
        (GearSlot::WeaponSet2Main, GearSlot::WeaponSet2Off)
    };
    let mut v = Vec::with_capacity(2);
    if has_main {
        v.push(main);
    }
    if has_off {
        v.push(off);
    }
    v
}

fn piece_lock<'a>(
    locks: Option<&'a mut BuildLocks>,
    db: Option<&'a GameDb>,
    api_slot: &str,
    buf: &'a mut [GearSlot; 1],
) -> Option<RowLock<'a>> {
    buf[0] = piece_gear_slot(api_slot)?;
    Some(RowLock {
        locks: locks?,
        db: db?,
        slots: buf,
    })
}

fn weapon_lock<'a>(
    locks: Option<&'a mut BuildLocks>,
    db: Option<&'a GameDb>,
    slots: &'a [GearSlot],
) -> Option<RowLock<'a>> {
    if slots.is_empty() {
        return None;
    }
    Some(RowLock {
        locks: locks?,
        db: db?,
        slots,
    })
}

/// The suggestion's prefix for one armor/trinket piece: its own slot entry on
/// the map when present, else the profile-level prefix name.
pub fn sug_prefix_for_piece<'a>(sug: &'a BuildSuggestion, piece_slot: &str) -> &'a str {
    sug.slot_prefixes
        .as_ref()
        .zip(piece_gear_slot(piece_slot))
        .and_then(|(map, gear_slot)| map.get(gear_slot))
        .map(|p| p.name.as_str())
        .unwrap_or(&sug.stat_prefix)
}

/// The suggestion's prefix for weapon set `set_idx` (0-based): the set's main
/// hand entry, else the profile-level prefix name.
pub fn sug_prefix_for_weapon(sug: &BuildSuggestion, set_idx: usize) -> &str {
    let slot = match set_idx {
        0 => GearSlot::WeaponSet1Main,
        _ => GearSlot::WeaponSet2Main,
    };
    sug.slot_prefixes
        .as_ref()
        .and_then(|map| map.get(slot))
        .map(|p| p.name.as_str())
        .unwrap_or(&sug.stat_prefix)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainTint {
    None,
    Up,
    Down,
    /// Changed, but the build's combat result did not move. Still marked:
    /// the player is being handed a build to equip, and "same as yours" and
    /// "different, no measured gain" are not the same answer.
    Flat,
}

/// Whether this slot differs from what the player is wearing.
pub fn tint_changed(tint: GainTint) -> bool {
    !matches!(tint, GainTint::None)
}

pub fn slot_label(api: &str) -> String {
    t(match api {
        "Helm" => "slot.helm",
        "Shoulders" => "slot.shoulders",
        "Coat" => "slot.chest",
        "Gloves" => "slot.gloves",
        "Leggings" => "slot.legs",
        "Boots" => "slot.boots",
        "Backpack" => "slot.back",
        "Accessory1" | "Accessory2" => "slot.accessory",
        "Amulet" => "slot.amulet",
        "Ring1" | "Ring2" => "slot.ring",
        _ => return api.to_string(),
    })
}

/// Combat result delta — not Power. DPS first, then healing, then eHP.
pub fn combat_gain(cur: Option<&CombatMetrics>, opt: Option<&CombatMetrics>) -> i32 {
    let (Some(c), Some(o)) = (cur, opt) else {
        return 0;
    };
    let dps = o.total_dps_index - c.total_dps_index;
    if dps != 0 {
        return dps;
    }
    let heal = o.healing_index - c.healing_index;
    if heal != 0 {
        return heal;
    }
    o.effective_health - c.effective_health
}

pub fn slot_tint(changed: bool, viewing_optimized: bool, gain: i32) -> GainTint {
    if !changed {
        return GainTint::None;
    }
    if gain == 0 {
        return GainTint::Flat;
    }
    let opt_is_better = gain > 0;
    if viewing_optimized == opt_is_better {
        GainTint::Up
    } else {
        GainTint::Down
    }
}

fn icon_tint(gain: GainTint) -> [f32; 4] {
    match gain {
        GainTint::None | GainTint::Flat => [1.0, 1.0, 1.0, 1.0],
        GainTint::Up => [0.78, 1.0, 0.78, 1.0],
        GainTint::Down => [1.0, 0.78, 0.78, 1.0],
    }
}

pub fn render_view_toggle(ui: &Ui, show_optimized: &mut bool) {
    if theme::pill(ui, &t("label.current"), !*show_optimized, "##view_current") {
        *show_optimized = false;
    }
    ui.same_line_with_spacing(0.0, 6.0);
    if theme::pill(
        ui,
        &t("label.optimized"),
        *show_optimized,
        "##view_optimized",
    ) {
        *show_optimized = true;
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_current_sheet(
    ui: &Ui,
    build: &ResolvedBuild,
    suggestion: Option<&BuildSuggestion>,
    db: Option<&GameDb>,
    viewing_optimized: bool,
    gain: i32,
    locks: Option<&mut BuildLocks>,
) {
    if viewing_optimized {
        if let Some(sug) = suggestion {
            render_suggestion_sheet(ui, build, sug, db, true, gain, locks);
            return;
        }
    }
    render_resolved_sheet(ui, build, suggestion, db, false, gain, locks);
}

#[allow(clippy::too_many_arguments)]
fn render_resolved_sheet(
    ui: &Ui,
    build: &ResolvedBuild,
    suggestion: Option<&BuildSuggestion>,
    db: Option<&GameDb>,
    viewing_optimized: bool,
    gain: i32,
    mut locks: Option<&mut BuildLocks>,
) {
    let rune_name = build.rune.as_ref().map(|r| r.name.as_str()).unwrap_or("");
    let rune_url = db.and_then(|d| {
        build
            .rune
            .as_ref()
            .and_then(|r| icons::item_url(d, r.id))
            .or_else(|| icons::upgrade_url(d, rune_name))
    });
    // Per-piece suggested prefix: the suggestion's slot map entry for each
    // piece, falling back to the profile-level name. Empty when no suggestion.
    let sug_prefix = |piece_slot: &str| -> String {
        suggestion
            .map(|s| sug_prefix_for_piece(s, piece_slot))
            .unwrap_or("")
            .to_string()
    };
    let sug_rune = suggestion.map(|s| s.rune.as_str()).unwrap_or("");

    ui.columns(3, "##gear_cols", false);
    {
        section(ui, &t("section.armor"));
        for slot in ARMOR_SLOTS {
            let piece = build.armor.iter().find(|p| p.slot == slot);
            let prefix = piece.map(|p| p.stat_prefix.as_str()).unwrap_or("");
            let name = piece.map(|p| p.name.as_str()).unwrap_or("");
            let url = db.and_then(|d| piece.and_then(|p| icons::item_url(d, p.id)));
            let suggested = sug_prefix(slot);
            let shown = if prefix.is_empty() {
                suggested.as_str()
            } else {
                prefix
            };
            let fallback = db
                .filter(|_| url.is_none())
                .and_then(|d| icons::gear_slot_url(d, &build.profession, slot, shown));
            let url = url.or(fallback.as_deref());
            let changed = suggestion.is_some()
                && (prefix != suggested && !suggested.is_empty() || rune_name != sug_rune);
            let other = suggestion.map(|_| format!("{} {}", sug_prefix(slot), slot_label(slot)));
            let mut buf = [GearSlot::Helm];
            row(
                ui,
                db,
                url,
                prefix,
                slot_label(slot),
                name,
                rune_name,
                rune_url,
                other.as_deref(),
                slot_tint(changed, viewing_optimized, gain),
                piece_lock(locks.as_deref_mut(), db, slot, &mut buf),
            );
        }
    }
    ui.next_column();
    {
        section(ui, &t("section.trinkets"));
        for slot in TRINKET_SLOTS {
            let piece = build.trinkets.iter().find(|p| p.slot == slot);
            let prefix = piece.map(|p| p.stat_prefix.as_str()).unwrap_or("");
            let name = piece.map(|p| p.name.as_str()).unwrap_or("");
            let url = db.and_then(|d| piece.and_then(|p| icons::item_url(d, p.id)));
            let suggested = sug_prefix(slot);
            let shown = if prefix.is_empty() {
                suggested.as_str()
            } else {
                prefix
            };
            let fallback = db
                .filter(|_| url.is_none())
                .and_then(|d| icons::gear_slot_url(d, &build.profession, slot, shown));
            let url = url.or(fallback.as_deref());
            let changed = suggestion.is_some() && prefix != suggested && !suggested.is_empty();
            let other = suggestion.map(|_| format!("{} {}", sug_prefix(slot), slot_label(slot)));
            let mut buf = [GearSlot::Helm];
            row(
                ui,
                db,
                url,
                prefix,
                slot_label(slot),
                name,
                "",
                None,
                other.as_deref(),
                slot_tint(changed, viewing_optimized, gain),
                piece_lock(locks.as_deref_mut(), db, slot, &mut buf),
            );
        }
        if let Some(relic) = &build.relic {
            let sug_relic = suggestion.map(|s| s.relic.as_str()).unwrap_or("");
            let url = db.and_then(|d| {
                icons::item_url(d, relic.id).or_else(|| icons::upgrade_url(d, &relic.name))
            });
            let other = suggestion
                .filter(|s| !s.relic.is_empty())
                .map(|s| s.relic.as_str());
            row(
                ui,
                db,
                url,
                "",
                "Relic",
                &relic.name,
                "",
                None,
                other,
                slot_tint(
                    suggestion.is_some() && relic.name != sug_relic,
                    viewing_optimized,
                    gain,
                ),
                None,
            );
        }
    }
    ui.next_column();
    {
        section(ui, &t("section.weapons"));
        let sug_sets = suggestion
            .map(|s| parse_suggestion_weapons(&s.weapons))
            .unwrap_or_default();
        for (i, set) in build.weapons.iter().enumerate() {
            let parts: Vec<&str> = [set.main_hand.as_ref(), set.off_hand.as_ref()]
                .into_iter()
                .flatten()
                .map(|w| {
                    if w.weapon_type.is_empty() {
                        w.name.as_str()
                    } else {
                        w.weapon_type.as_str()
                    }
                })
                .collect();
            let label = parts.join(" / ");
            let sug_line = sug_sets.get(i).map(|(_, v)| v.as_str()).unwrap_or("");
            let url = db.and_then(|d| {
                set.main_hand
                    .as_ref()
                    .and_then(|w| icons::item_url(d, w.id))
                    .or_else(|| {
                        set.main_hand.as_ref().and_then(|w| {
                            icons::weapon_type_url(d, &build.profession, &w.weapon_type)
                        })
                    })
            });
            let sigils: Vec<String> = set.sigils.iter().map(|s| s.name.clone()).collect();
            let wslots = weapon_lock_slots(i, set.main_hand.is_some(), set.off_hand.is_some());
            weapon_row(
                ui,
                db,
                url,
                &set.stat_prefix,
                &set.label,
                &label,
                &sigils,
                if sug_line.is_empty() {
                    None
                } else {
                    Some(sug_line)
                },
                slot_tint(
                    suggestion.is_some() && !sug_line.is_empty() && sug_line != label,
                    viewing_optimized,
                    gain,
                ),
                weapon_lock(locks.as_deref_mut(), db, &wslots),
            );
        }
    }
    ui.columns(1, "##gear_end", false);
}

#[allow(clippy::too_many_arguments)]
fn render_suggestion_sheet(
    ui: &Ui,
    current: &ResolvedBuild,
    sug: &BuildSuggestion,
    db: Option<&GameDb>,
    viewing_optimized: bool,
    gain: i32,
    mut locks: Option<&mut BuildLocks>,
) {
    let rune_url = db.and_then(|d| icons::upgrade_url(d, &sug.rune));
    let rune_changed = !sug.rune.is_empty()
        && current
            .rune
            .as_ref()
            .is_none_or(|r| !r.name.eq_ignore_ascii_case(&sug.rune));
    ui.columns(3, "##gear_cols", false);
    {
        section(ui, &t("section.armor"));
        for slot in ARMOR_SLOTS {
            let cur = current.armor.iter().find(|p| p.slot == slot);
            let cur_prefix = cur.map(|p| p.stat_prefix.as_str()).unwrap_or("");
            let suggested = sug_prefix_for_piece(sug, slot);
            let other = cur.map(|p| {
                format!(
                    "{} {}",
                    if p.stat_prefix.is_empty() {
                        ""
                    } else {
                        p.stat_prefix.as_str()
                    },
                    slot_label(slot)
                )
            });
            let mut buf = [GearSlot::Helm];
            let url = db.and_then(|d| cur.and_then(|p| icons::item_url(d, p.id)));
            let fallback = db
                .filter(|_| url.is_none())
                .and_then(|d| icons::gear_slot_url(d, &current.profession, slot, suggested));
            row(
                ui,
                db,
                url.or(fallback.as_deref()),
                suggested,
                slot_label(slot),
                cur.map(|p| p.name.as_str()).unwrap_or(""),
                &sug.rune,
                rune_url,
                other.as_deref(),
                slot_tint(
                    rune_changed || (cur_prefix != suggested && !suggested.is_empty()),
                    viewing_optimized,
                    gain,
                ),
                piece_lock(locks.as_deref_mut(), db, slot, &mut buf),
            );
        }
    }
    ui.next_column();
    {
        section(ui, &t("section.trinkets"));
        for slot in TRINKET_SLOTS {
            let cur = current.trinkets.iter().find(|p| p.slot == slot);
            let cur_prefix = cur.map(|p| p.stat_prefix.as_str()).unwrap_or("");
            let suggested = sug_prefix_for_piece(sug, slot);
            let other = cur.map(|p| format!("{} {}", p.stat_prefix, slot_label(slot)));
            let mut buf = [GearSlot::Helm];
            let url = db.and_then(|d| cur.and_then(|p| icons::item_url(d, p.id)));
            let fallback = db
                .filter(|_| url.is_none())
                .and_then(|d| icons::gear_slot_url(d, &current.profession, slot, suggested));
            row(
                ui,
                db,
                url.or(fallback.as_deref()),
                suggested,
                slot_label(slot),
                cur.map(|p| p.name.as_str()).unwrap_or(""),
                "",
                None,
                other.as_deref(),
                slot_tint(
                    cur_prefix != suggested && !suggested.is_empty(),
                    viewing_optimized,
                    gain,
                ),
                piece_lock(locks.as_deref_mut(), db, slot, &mut buf),
            );
        }
        if !sug.relic.is_empty() {
            let cur_relic = current
                .relic
                .as_ref()
                .map(|r| r.name.as_str())
                .unwrap_or("");
            row(
                ui,
                db,
                db.and_then(|d| icons::upgrade_url(d, &sug.relic)),
                "",
                "Relic",
                &sug.relic,
                "",
                None,
                if cur_relic.is_empty() {
                    None
                } else {
                    Some(cur_relic)
                },
                slot_tint(cur_relic != sug.relic, viewing_optimized, gain),
                None,
            );
        }
    }
    ui.next_column();
    {
        section(ui, &t("section.weapons"));
        let sets = parse_suggestion_weapons(&sug.weapons);
        let sigil_pairs = sug.sigils.chunks(2);
        for (i, ((label, weapons), sigils)) in sets.iter().zip(sigil_pairs).enumerate() {
            let url = db.and_then(|d| {
                let wt = weapons.split(" / ").next().unwrap_or(weapons).trim();
                icons::weapon_type_url(d, &current.profession, wt)
            });
            let cur = current.weapons.get(i).map(|w| {
                let parts: Vec<&str> = [w.main_hand.as_ref(), w.off_hand.as_ref()]
                    .into_iter()
                    .flatten()
                    .map(|x| {
                        if x.weapon_type.is_empty() {
                            x.name.as_str()
                        } else {
                            x.weapon_type.as_str()
                        }
                    })
                    .collect();
                parts.join(" / ")
            });
            let sigil_names: Vec<String> = sigils.to_vec();
            let has_off = weapons.contains(" / ");
            let wslots = weapon_lock_slots(i, true, has_off);
            weapon_row(
                ui,
                db,
                url,
                sug_prefix_for_weapon(sug, i),
                label,
                weapons,
                &sigil_names,
                cur.as_deref(),
                slot_tint(
                    cur.as_deref() != Some(weapons.as_str()),
                    viewing_optimized,
                    gain,
                ),
                weapon_lock(locks.as_deref_mut(), db, &wslots),
            );
        }
        if sets.is_empty() {
            for w in &sug.weapons {
                ui.text_colored(theme::pal().cream, w);
            }
        }
    }
    ui.columns(1, "##gear_end", false);
}

fn section(ui: &Ui, title: &str) {
    ui.spacing();
    ui.text_colored(theme::pal().gold, title);
}

fn dim_icon(gain: GainTint) -> [f32; 4] {
    let t = icon_tint(gain);
    [t[0] * 0.5, t[1] * 0.5, t[2] * 0.5, 0.7]
}

fn lock_text_colors(interactive: bool, locked: bool) -> ([f32; 4], [f32; 4], [f32; 4]) {
    if locked {
        (LOCKED_TEXT, LOCKED_TEXT, LOCKED_TEXT)
    } else if interactive {
        (UNLOCKED_TEXT, UNLOCKED_TEXT, UNLOCKED_TEXT)
    } else {
        (theme::pal().gold, theme::pal().cream, theme::pal().muted)
    }
}

fn paint_slot_icon(
    ui: &Ui,
    url: Option<&str>,
    p: [f32; 2],
    size: f32,
    tint: [f32; 4],
    locked: bool,
) {
    icons::paint_at(ui, url, p, size, tint);
    if locked {
        ui.get_window_draw_list()
            .add_rect(
                p,
                [p[0] + size, p[1] + size],
                theme::with_alpha(theme::pal().gold, 0.8),
            )
            .thickness(1.5)
            .rounding(theme::ICON_ROUNDING)
            .build();
    }
}

fn lock_row_hit(ui: &Ui, p: [f32; 2], col_w: f32, row_h: f32) -> (bool, bool) {
    let id = format!("##glock_{}_{}", p[0] as i32, p[1] as i32);
    let _ = ui.invisible_button(&id, [col_w, row_h]);
    let hovered = ui.is_item_hovered();
    let clicked = ui.is_item_clicked() || (hovered && ui.is_mouse_clicked(MouseButton::Right));
    ui.set_cursor_screen_pos(p);
    (clicked, hovered)
}

fn append_lock_hint(ui: &Ui, locked: bool, prefix: &str) {
    if locked {
        ui.text_colored(LOCKED_TEXT, t("lock.locked"));
        ui.text_colored(UNLOCKED_TEXT, t("lock.click_unlock"));
    } else if prefix.is_empty() {
        ui.text_colored(UNLOCKED_TEXT, t("gear.no_prefix"));
    } else {
        ui.text_colored(UNLOCKED_TEXT, t("lock.click_lock"));
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    ui: &Ui,
    db: Option<&GameDb>,
    url: Option<&str>,
    prefix: &str,
    slot: impl AsRef<str>,
    name: &str,
    nested: &str,
    nested_url: Option<&str>,
    other: Option<&str>,
    tint: GainTint,
    lock: Option<RowLock<'_>>,
) {
    let slot = slot.as_ref();
    const ICON: f32 = 28.0;
    const GAP: f32 = 14.0;
    let p = ui.cursor_screen_pos();
    let interactive = lock.is_some();
    let locked = lock
        .as_ref()
        .is_some_and(|h| gear_row_locked(h.locks, h.slots));
    let extra = if nested.is_empty() { 0.0 } else { 22.0 };
    let (clicked, hovered) = if interactive {
        lock_row_hit(
            ui,
            p,
            ui.content_region_avail()[0].max(1.0),
            ICON + extra + 2.0,
        )
    } else {
        (false, false)
    };

    let itint = if interactive && !locked {
        dim_icon(tint)
    } else {
        icon_tint(tint)
    };
    if interactive {
        paint_slot_icon(ui, url, p, ICON, itint, locked);
    } else {
        icons::draw(ui, url, ICON, itint);
        if ui.is_item_hovered() {
            let key = if name.is_empty() { slot } else { name };
            if db.and_then(|d| comparison::inspect_text(key, d)).is_none() {
                tooltip(ui, prefix, slot, name, nested, other);
            }
            comparison::inspect_if_hovered(ui, key, db);
        }
        ui.same_line();
    }
    // Green halo means Choya moved this slot; the gold lock ring means the
    // player pinned it. Same shape, so the two read as one language, and an
    // unmarked slot honestly means "same as what you are wearing".
    if tint_changed(tint) {
        theme::paint_changed_rect(
            &ui.get_window_draw_list(),
            p,
            [p[0] + ICON, p[1] + ICON],
            theme::ICON_ROUNDING,
        );
    }
    ui.set_cursor_screen_pos([p[0] + ICON + GAP, p[1]]);
    let (prefix_col, slot_col, name_col) = lock_text_colors(interactive, locked);
    if !prefix.is_empty() {
        ui.text_colored(prefix_col, comparison::loc_prefix(db, prefix));
        ui.same_line();
    }
    ui.text_colored(slot_col, slot);
    if !name.is_empty() && name != slot {
        ui.same_line();
        ui.text_colored(name_col, comparison::loc_name(db, name));
    }
    if !interactive && ui.is_item_hovered() {
        tooltip(ui, prefix, slot, name, nested, other);
    }

    if !nested.is_empty() {
        ui.indent();
        let np = ui.cursor_screen_pos();
        icons::draw(ui, nested_url, 18.0, [1.0, 1.0, 1.0, 1.0]);
        ui.same_line();
        ui.set_cursor_screen_pos([np[0] + 18.0 + 10.0, np[1]]);
        ui.text_colored(theme::pal().muted, comparison::loc_upgrade(db, nested));
        if !interactive {
            comparison::inspect_if_hovered(ui, nested, db);
        }
        ui.unindent();
    }

    if hovered {
        let key = if name.is_empty() { slot } else { name };
        crate::ui::theme::wide_tooltip(ui, |tip| {
            if let Some(text) = db.and_then(|d| comparison::inspect_text(key, d)) {
                let mut lines = text.lines();
                if let Some(title) = lines.next() {
                    tip.text_colored(theme::pal().gold, title);
                }
                for line in lines {
                    tip.text(line);
                }
            } else {
                let shown = format!("{} {} {}", prefix, slot, name);
                tip.text_colored(theme::pal().gold, shown.trim());
                if !nested.is_empty() {
                    tip.text_colored(theme::pal().muted, nested);
                }
                if let Some(o) = other {
                    tip.spacing();
                    tip.text_colored(theme::pal().muted, t("gear.other"));
                    tip.text_colored(theme::pal().cream, o);
                }
            }
            append_lock_hint(tip, locked, prefix);
        });
    }
    if clicked {
        if let Some(h) = lock {
            toggle_gear_row(h.locks, h.db, h.slots, prefix);
        }
    }
    if interactive {
        ui.set_cursor_screen_pos([p[0], p[1] + ICON + extra + 2.0]);
        ui.dummy([0.0, 0.0]);
    }
}

#[allow(clippy::too_many_arguments)]
fn weapon_row(
    ui: &Ui,
    db: Option<&GameDb>,
    url: Option<&str>,
    prefix: &str,
    set_label: &str,
    weapons: &str,
    sigils: &[String],
    other: Option<&str>,
    tint: GainTint,
    lock: Option<RowLock<'_>>,
) {
    const ICON: f32 = 28.0;
    const GAP: f32 = 14.0;
    let p = ui.cursor_screen_pos();
    let interactive = lock.is_some();
    let locked = lock
        .as_ref()
        .is_some_and(|h| gear_row_locked(h.locks, h.slots));
    let extra = sigils.iter().filter(|s| !s.is_empty()).count() as f32 * 20.0;
    let (clicked, hovered) = if interactive {
        lock_row_hit(
            ui,
            p,
            ui.content_region_avail()[0].max(1.0),
            ICON + extra + 2.0,
        )
    } else {
        (false, false)
    };

    let itint = if interactive && !locked {
        dim_icon(tint)
    } else {
        icon_tint(tint)
    };
    if interactive {
        paint_slot_icon(ui, url, p, ICON, itint, locked);
    } else {
        icons::draw(ui, url, ICON, itint);
        ui.same_line();
    }
    // Green halo means Choya moved this slot; the gold lock ring means the
    // player pinned it. Same shape, so the two read as one language, and an
    // unmarked slot honestly means "same as what you are wearing".
    if tint_changed(tint) {
        theme::paint_changed_rect(
            &ui.get_window_draw_list(),
            p,
            [p[0] + ICON, p[1] + ICON],
            theme::ICON_ROUNDING,
        );
    }
    ui.set_cursor_screen_pos([p[0] + ICON + GAP, p[1]]);
    let (prefix_col, slot_col, _) = lock_text_colors(interactive, locked);
    if !prefix.is_empty() {
        ui.text_colored(prefix_col, comparison::loc_prefix(db, prefix));
        ui.same_line();
    }
    ui.text_colored(slot_col, set_label);
    ui.same_line();
    ui.text_colored(slot_col, loc_weapon_types(weapons));
    if !interactive && ui.is_item_hovered() {
        tooltip(ui, prefix, set_label, weapons, &sigils.join(" · "), other);
    }
    for sig in sigils {
        if sig.is_empty() {
            continue;
        }
        ui.indent();
        let sp = ui.cursor_screen_pos();
        let surl = db.and_then(|d| icons::upgrade_url(d, sig));
        icons::draw(ui, surl, 18.0, [1.0, 1.0, 1.0, 1.0]);
        ui.same_line();
        ui.set_cursor_screen_pos([sp[0] + 18.0 + 10.0, sp[1]]);
        ui.text_colored(theme::pal().muted, comparison::loc_upgrade(db, sig));
        if !interactive {
            comparison::inspect_if_hovered(ui, sig, db);
        }
        ui.unindent();
    }
    if hovered {
        crate::ui::theme::wide_tooltip(ui, |tip| {
            let shown = format!("{} {} {}", prefix, set_label, weapons);
            tip.text_colored(theme::pal().gold, shown.trim());
            let nested = sigils.join(" · ");
            if !nested.is_empty() {
                tip.text_colored(theme::pal().muted, nested);
            }
            if let Some(o) = other {
                tip.spacing();
                tip.text_colored(theme::pal().muted, t("gear.other"));
                tip.text_colored(theme::pal().cream, o);
            }
            append_lock_hint(tip, locked, prefix);
        });
    }
    if clicked {
        if let Some(h) = lock {
            toggle_gear_row(h.locks, h.db, h.slots, prefix);
        }
    }
    if interactive {
        ui.set_cursor_screen_pos([p[0], p[1] + ICON + extra + 2.0]);
        ui.dummy([0.0, 0.0]);
    }
}

fn tooltip(ui: &Ui, prefix: &str, slot: &str, name: &str, nested: &str, other: Option<&str>) {
    crate::ui::theme::wide_tooltip(ui, |ui| {
        let shown = format!("{} {} {}", prefix, slot, name);
        ui.text_colored(theme::pal().gold, shown.trim());
        if !nested.is_empty() {
            ui.text_colored(theme::pal().muted, nested);
        }
        if let Some(o) = other {
            ui.spacing();
            ui.text_colored(theme::pal().muted, t("gear.other"));
            ui.text_colored(theme::pal().cream, o);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coat_is_chest() {
        gw2_core::i18n::set_language("en");
        assert_eq!(slot_label("Coat"), "Chest");
        assert_eq!(slot_label("Helm"), "Helm");
    }

    #[test]
    fn tint_follows_combat_not_power() {
        assert_eq!(slot_tint(false, true, 100), GainTint::None);
        // A changed slot with no combat delta still reads as changed: the
        // player is being handed a build to equip, and "identical to yours"
        // is a different answer from "different, no measured gain".
        assert_eq!(slot_tint(true, true, 0), GainTint::Flat);
        assert!(tint_changed(GainTint::Flat));
        assert!(!tint_changed(GainTint::None));
        assert_eq!(slot_tint(true, true, 50), GainTint::Up);
        assert_eq!(slot_tint(true, false, 50), GainTint::Down);
        assert_eq!(slot_tint(true, true, -20), GainTint::Down);
    }

    #[test]
    fn gear_row_locked_requires_every_slot() {
        let mut locks = BuildLocks::default();
        locks.gear_locks.insert(GearSlot::Helm, 1);
        assert!(gear_row_locked(&locks, &[GearSlot::Helm]));
        assert!(!gear_row_locked(&locks, &[GearSlot::Helm, GearSlot::Boots]));
        assert!(!gear_row_locked(&locks, &[]));
    }

    #[test]
    fn toggle_gear_row_pins_and_releases() {
        let mut locks = BuildLocks::default();
        let slots = [GearSlot::Helm, GearSlot::Boots];
        apply_gear_row_toggle(&mut locks, &slots, None);
        assert!(locks.gear_locks.is_empty(), "no prefix → no pin");
        apply_gear_row_toggle(&mut locks, &slots, Some(42));
        assert_eq!(locks.gear_locks.get(&GearSlot::Helm), Some(&42));
        assert_eq!(locks.gear_locks.get(&GearSlot::Boots), Some(&42));
        apply_gear_row_toggle(&mut locks, &slots, Some(42));
        assert!(locks.gear_locks.is_empty(), "second click releases");
    }

    #[test]
    fn weapon_lock_slots_split_set_hands() {
        assert_eq!(
            weapon_lock_slots(0, true, false),
            vec![GearSlot::WeaponSet1Main]
        );
        assert_eq!(
            weapon_lock_slots(0, true, true),
            vec![GearSlot::WeaponSet1Main, GearSlot::WeaponSet1Off]
        );
        assert_eq!(
            weapon_lock_slots(1, true, true),
            vec![GearSlot::WeaponSet2Main, GearSlot::WeaponSet2Off]
        );
    }

    /// Pin: Current-view (`render_resolved_sheet`) paints and locks the
    /// equipped prefix (`set.stat_prefix`). `sug_prefix_for_weapon` is the
    /// Optimized sheet only. Freeze SHA passed `sug_weapon_prefix` into
    /// `weapon_row` whenever a suggestion was present, so Improve Current +
    /// lock wrote the suggestion itemstat id.
    ///
    /// Source pin, not a live imgui render: this crate is a Windows cdylib
    /// (`arcdps-imgui-sys` needs `c++`) and cannot compile on this Linux host.
    #[test]
    fn current_weapon_row_uses_equipped_prefix_not_suggestion() {
        let src = include_str!("gear_sheet.rs");
        let start = src
            .find("\nfn render_resolved_sheet(")
            .expect("render_resolved_sheet must exist");
        let rest = &src[start..];
        let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains("sug_prefix_for_weapon"),
            "Current sheet must not read sug_prefix_for_weapon for lock/display prefix"
        );
        let wr = body
            .find("weapon_row(")
            .expect("render_resolved_sheet must call weapon_row");
        let call = &body[wr..];
        let close = call.find(");").expect("weapon_row call must close");
        let args = &call[..close];
        assert!(
            args.contains("&set.stat_prefix"),
            "weapon_row must receive the equipped set.stat_prefix"
        );
    }
}
