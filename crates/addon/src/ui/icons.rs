//! ArenaNet skill/trait/item icons.
//!
//! Prefer `cache/graphics/*.png` (downloaded with game data). Fall back to the
//! render CDN through Nexus. Corners use the same 5px rounding as buttons.

use std::path::PathBuf;
use std::sync::Mutex;

use gw2_optimizer::gamedb::GameDb;
use nexus::imgui::{DrawListMut, TextureId, Ui};

use super::theme;

static GRAPHICS_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static REQUESTED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
static GEAR_URLS: Mutex<Option<std::collections::HashMap<String, Option<String>>>> =
    Mutex::new(None);

pub fn set_graphics_dir(dir: PathBuf) {
    if let Ok(mut g) = GRAPHICS_DIR.lock() {
        *g = Some(dir);
    }
}

fn graphics_dir() -> Option<PathBuf> {
    GRAPHICS_DIR.lock().ok()?.clone()
}

fn tex_id(endpoint: &str) -> String {
    format!("GW2BO{}", endpoint.replace('/', "_"))
}

fn ensure_texture(url: &str) -> Option<TextureId> {
    let (remote, endpoint) = gw2_api::graphics::parse_render_url(url)?;
    let id = tex_id(endpoint);
    let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nexus::texture::get_texture(&id)
    }))
    .ok()
    .flatten();
    if let Some(tex) = loaded {
        return Some(tex.id());
    }

    if let Some(path) = graphics_dir().and_then(|d| gw2_api::graphics::local_path(&d, endpoint)) {
        if path.exists() {
            let from_file = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                nexus::texture::get_texture_or_create_from_file(&id, &path)
            }))
            .ok()
            .flatten();
            if let Some(tex) = from_file {
                return Some(tex.id());
            }
        }
    }

    let mut req = REQUESTED.lock().ok()?;
    let set = req.get_or_insert_with(std::collections::HashSet::new);
    if set.insert(id.clone()) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            nexus::texture::load_texture_from_url(&id, remote, endpoint, None);
        }));
    }
    None
}

/// Draw a rounded icon (or a gold plate while it loads). Advances the cursor.
pub fn draw(ui: &Ui, url: Option<&str>, size: f32, tint: [f32; 4]) -> bool {
    let p = ui.cursor_screen_pos();
    let id = format!("##ico_{}_{}", p[0] as i32, p[1] as i32);
    let clicked = ui.invisible_button(&id, [size, size]);
    paint_at(ui, url, p, size, tint);
    clicked
}

pub fn paint_at(ui: &Ui, url: Option<&str>, p: [f32; 2], size: f32, tint: [f32; 4]) {
    let dl = crate::ui::window_draw_list(ui);
    paint_on(&dl, url, p, [p[0] + size, p[1] + size], tint);
}

/// Pet portraits sit in a padded 256px canvas; skill icons fill 128px.
pub const PET_ICON_ZOOM: f32 = 1.7;

fn icon_uv(zoom: f32) -> ([f32; 2], [f32; 2]) {
    if zoom <= 1.0 {
        ([0.0, 0.0], [1.0, 1.0])
    } else {
        let inset = (0.5 - 0.5 / zoom).clamp(0.0, 0.45);
        ([inset, inset], [1.0 - inset, 1.0 - inset])
    }
}

pub fn paint_on(
    draw_list: &DrawListMut<'_>,
    url: Option<&str>,
    p_min: [f32; 2],
    p_max: [f32; 2],
    tint: [f32; 4],
) {
    paint_on_zoomed(draw_list, url, p_min, p_max, tint, 1.0);
}

pub fn paint_on_zoomed(
    draw_list: &DrawListMut<'_>,
    url: Option<&str>,
    p_min: [f32; 2],
    p_max: [f32; 2],
    tint: [f32; 4],
    zoom: f32,
) {
    let rounding = theme::ICON_ROUNDING;
    if let Some(tid) = url.and_then(ensure_texture) {
        let (uv0, uv1) = icon_uv(zoom);
        draw_list
            .add_image_rounded(tid, p_min, p_max, rounding)
            .uv_min(uv0)
            .uv_max(uv1)
            .col(tint)
            .build();
    } else {
        draw_list
            .add_rect(p_min, p_max, theme::pal().plate)
            .filled(true)
            .rounding(rounding)
            .build();
        draw_list
            .add_rect(p_min, p_max, theme::pal().gold_dim)
            .rounding(rounding)
            .build();
    }
}

pub fn profession_icon_url<'a>(db: &'a GameDb, profession: &str) -> Option<&'a str> {
    let p = db.profession(profession).or_else(|| {
        db.professions.values().find(|p| {
            p.id.eq_ignore_ascii_case(profession) || p.name.eq_ignore_ascii_case(profession)
        })
    })?;
    p.icon_big
        .as_deref()
        .or(p.icon.as_deref())
        .filter(|s| !s.is_empty())
}

pub fn paint_avatar(ui: &Ui, url: Option<&str>, p: [f32; 2], size: f32, letter: char) {
    let dl = crate::ui::window_draw_list(ui);
    let p_max = [p[0] + size, p[1] + size];
    let r = size * 0.5;
    if let Some(tid) = url.and_then(ensure_texture) {
        dl.add_image_rounded(tid, p, p_max, r)
            .col([1.0, 1.0, 1.0, 1.0])
            .build();
    } else {
        dl.add_rect(p, p_max, theme::pal().plate)
            .filled(true)
            .rounding(r)
            .build();
        let s = letter.to_ascii_uppercase().to_string();
        let sz = ui.calc_text_size(&s);
        dl.add_text(
            [p[0] + (size - sz[0]) * 0.5, p[1] + (size - sz[1]) * 0.5],
            crate::ui::color_u32(theme::CURRENT),
            &s,
        );
    }
    dl.add_rect(p, p_max, theme::pal().gold_dim)
        .rounding(r)
        .build();
}

pub fn item_url(db: &GameDb, id: u32) -> Option<&str> {
    db.items.get(&id).and_then(|i| i.icon.as_deref())
}

/// A representative icon for a gear slot the build names by prefix only (an
/// optimized plate carries no item ids): the lowest-id exotic, else ascended,
/// cached piece of that slot, armour weight and stat prefix. Memoised per
/// slot key; the scan is one pass over the cached items of the type.
pub fn gear_slot_url(db: &GameDb, profession: &str, slot: &str, prefix: &str) -> Option<String> {
    let (item_type, detail, weight) = match slot {
        "Helm" | "Shoulders" | "Coat" | "Gloves" | "Leggings" | "Boots" => (
            "Armor",
            slot,
            gw2_optimizer::data::profession_profiles::profiles().armor_weight(profession),
        ),
        "Backpack" => ("Back", "", None),
        "Accessory1" | "Accessory2" => ("Trinket", "Accessory", None),
        "Ring1" | "Ring2" => ("Trinket", "Ring", None),
        "Amulet" => ("Trinket", "Amulet", None),
        _ => return None,
    };
    let key = format!("{item_type}|{detail}|{}|{prefix}", weight.unwrap_or(""));
    let mut memo = GEAR_URLS.lock().ok()?;
    let memo = memo.get_or_insert_with(std::collections::HashMap::new);
    if let Some(hit) = memo.get(&key) {
        return hit.clone();
    }
    let stat_ids: Vec<u32> = db
        .itemstats
        .values()
        .filter(|s| s.name.eq_ignore_ascii_case(prefix.trim()))
        .map(|s| s.id)
        .collect();
    let mut best: Option<(u8, u32, &str)> = None;
    for id in db.items_by_type.get(item_type).into_iter().flatten() {
        let Some(item) = db.items.get(id) else {
            continue;
        };
        let Some(icon) = item.icon.as_deref() else {
            continue;
        };
        let details = item.details.as_ref();
        if !detail.is_empty() && details.and_then(|d| d.detail_type.as_deref()) != Some(detail) {
            continue;
        }
        if weight.is_some() && details.and_then(|d| d.weight_class.as_deref()) != weight {
            continue;
        }
        // A fixed-stat piece names the prefix in its infix; a selectable
        // piece (every Marauder back item in the cache) lists it in its
        // stat choices. A piece of the slot with neither is the last resort.
        let infix = details
            .and_then(|d| d.infix_upgrade.as_ref())
            .and_then(|i| i.id);
        let carries_prefix = infix.is_some_and(|i| stat_ids.contains(&i))
            || details.is_some_and(|d| d.stat_choices.iter().any(|c| stat_ids.contains(c)));
        let rank = u8::from(!carries_prefix) * 4
            + match item.rarity.as_str() {
                "Exotic" => 0,
                "Ascended" => 1,
                _ => 2,
            };
        if best.is_none_or(|(r, bid, _)| (rank, *id) < (r, bid)) {
            best = Some((rank, *id, icon));
        }
    }
    let url = best.map(|(_, _, u)| u.to_string());
    memo.insert(key, url.clone());
    url
}

pub fn skill_url(db: &GameDb, id: u32) -> Option<&str> {
    db.skills.get(&id).and_then(|s| s.icon.as_deref())
}

pub fn trait_url(db: &GameDb, id: u32) -> Option<&str> {
    db.traits.get(&id).and_then(|t| t.icon.as_deref())
}

pub fn spec_url(db: &GameDb, id: u32) -> Option<&str> {
    db.specializations.get(&id).and_then(|s| s.icon.as_deref())
}

pub fn pet_url<'a>(db: &'a GameDb, name: &str) -> Option<&'a str> {
    db.pet_by_name(name)?
        .icon
        .as_deref()
        .filter(|s| !s.is_empty())
}

pub fn skill_url_by_name<'a>(db: &'a GameDb, name: &str) -> Option<&'a str> {
    lookup_name(
        db.skills
            .values()
            .map(|s| (s.name.as_str(), s.icon.as_deref())),
        name,
    )
}

pub fn spec_url_by_name<'a>(db: &'a GameDb, name: &str) -> Option<&'a str> {
    lookup_name(
        db.specializations
            .values()
            .map(|s| (s.name.as_str(), s.icon.as_deref())),
        name,
    )
}

pub fn upgrade_url<'a>(db: &'a GameDb, name: &str) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    for ids in [&db.runes, &db.sigils, &db.relics] {
        if let Some(url) = lookup_item_ids(db, ids, name) {
            return Some(url);
        }
    }
    None
}

pub fn weapon_type_url<'a>(db: &'a GameDb, profession: &str, weapon_type: &str) -> Option<&'a str> {
    let prof = db.professions.get(profession)?;
    let needle = gw2_core::i18n::weapon_type_key(weapon_type);
    let w = prof
        .weapons
        .iter()
        .find(|(k, _)| gw2_core::i18n::weapon_type_key(k) == needle)
        .map(|(_, info)| info)?;
    let sid = w.skills.first()?.id;
    skill_url(db, sid)
}

fn lookup_item_ids<'a>(db: &'a GameDb, ids: &[u32], name: &str) -> Option<&'a str> {
    lookup_name(
        ids.iter().filter_map(|id| {
            db.items
                .get(id)
                .map(|i| (i.name.as_str(), i.icon.as_deref()))
        }),
        name,
    )
}

fn lookup_name<'a>(
    rows: impl Iterator<Item = (&'a str, Option<&'a str>)>,
    needle: &str,
) -> Option<&'a str> {
    let needle = needle.trim();
    if needle.is_empty() {
        return None;
    }
    let lower = needle.to_lowercase();
    let mut best: Option<(usize, &'a str)> = None;
    for (name, icon) in rows {
        let Some(icon) = icon else { continue };
        if name.eq_ignore_ascii_case(needle) {
            return Some(icon);
        }
        let nlow = name.to_lowercase();
        if nlow.contains(&lower) {
            let key = name.len();
            if best.is_none_or(|(blen, _)| key < blen) {
                best = Some((key, icon));
            }
        }
    }
    best.map(|(_, u)| u)
}

#[cfg(test)]
mod tests {
    use super::{icon_uv, PET_ICON_ZOOM};

    #[test]
    fn icon_uv_unzoomed_is_full_quad() {
        assert_eq!(icon_uv(1.0), ([0.0, 0.0], [1.0, 1.0]));
    }

    #[test]
    fn pet_icon_zoom_crops_the_padded_canvas() {
        let (uv0, uv1) = icon_uv(PET_ICON_ZOOM);
        assert!(uv0[0] > 0.15 && uv0[0] < 0.25, "{uv0:?}");
        assert!((uv0[0] - (1.0 - uv1[0])).abs() < f32::EPSILON);
    }
}
