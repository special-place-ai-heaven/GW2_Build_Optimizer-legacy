//! GW2-style visual specialization & trait lock panel.
//! Renders hexagons (specs) + 3×3 circle grids (traits) in the content area.
//! Click to lock/unlock — locked items are preserved by the optimizer.

use gw2_core::i18n::{slavic_plural_form, t, tf, SlavicPluralForm};
use gw2_core::types::{BuildLocks, GearSlot};
use gw2_optimizer::gamedb::GameDb;
use nexus::imgui::Ui;

// Colors

const SELECTED_COLOR: [f32; 4] = [0.5, 0.8, 1.0, 1.0]; // Cyan — selected but unlocked
const DIM_COLOR: [f32; 4] = [0.35, 0.35, 0.35, 0.8]; // Gray — unselected
const AVAILABLE_COLOR: [f32; 4] = [0.5, 0.5, 0.5, 0.6]; // Dim — available but not selected
const ELITE_COLOR: [f32; 4] = [1.0, 0.6, 0.2, 1.0]; // Orange for elite marker

/// Accent for locked specs/traits/tooltips — follows the active theme.
fn locked_color() -> [f32; 4] {
    crate::ui::theme::pal().gold
}

use crate::ui::color_u32;

// Geometry helpers

fn draw_hexagon(
    draw_list: &nexus::imgui::DrawListMut,
    center: [f32; 2],
    radius: f32,
    color: u32,
    filled: bool,
    thickness: f32,
) {
    // Stack-allocated fixed-size array — hexagons always have exactly 6
    // vertices, so a heap Vec per render frame was pure overhead.
    let mut points = [[0.0f32; 2]; 6];
    for (i, pt) in points.iter_mut().enumerate() {
        let angle = std::f32::consts::PI / 3.0 * i as f32 - std::f32::consts::FRAC_PI_6;
        *pt = [
            center[0] + radius * angle.cos(),
            center[1] + radius * angle.sin(),
        ];
    }
    if filled {
        for i in 0..6 {
            let next = (i + 1) % 6;
            draw_list
                .add_triangle(center, points[i], points[next], color)
                .filled(true)
                .build();
        }
    }
    // Outline
    for i in 0..6 {
        let next = (i + 1) % 6;
        draw_list
            .add_line(points[i], points[next], color)
            .thickness(thickness)
            .build();
    }
}

fn is_in_circle(mouse: [f32; 2], center: [f32; 2], radius: f32) -> bool {
    let dx = mouse[0] - center[0];
    let dy = mouse[1] - center[1];
    dx * dx + dy * dy <= radius * radius
}

fn is_in_hexagon(mouse: [f32; 2], center: [f32; 2], radius: f32) -> bool {
    // Approximate with circle (close enough for click detection)
    is_in_circle(mouse, center, radius)
}

fn spec_hex_width(ui: &Ui, hex_radius: f32, s: f32) -> f32 {
    let name_w = ui.calc_text_size("Dragonhunter")[0] + 8.0;
    (hex_radius * 2.0 + 16.0 * s).max(name_w)
}

fn bezier4(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> [f32; 2] {
    let u = 1.0 - t;
    let uu = u * u;
    let tt = t * t;
    let a = uu * u;
    let b = 3.0 * uu * t;
    let c = 3.0 * u * tt;
    let d = tt * t;
    [
        a * p0[0] + b * p1[0] + c * p2[0] + d * p3[0],
        a * p0[1] + b * p1[1] + c * p2[1] + d * p3[1],
    ]
}

/// Faint path from selected trait text-end to the next selected icon.
fn draw_ghost_link(draw_list: &nexus::imgui::DrawListMut, from: [f32; 2], to: [f32; 2]) {
    let dx = to[0] - from[0];
    if dx < 8.0 {
        return;
    }
    let c1 = [from[0] + dx * 0.42, from[1]];
    let c2 = [to[0] - dx * 0.42, to[1]];
    let color = color_u32(crate::ui::theme::with_alpha(
        crate::ui::theme::pal().gold,
        0.18,
    ));
    let n = 20;
    let mut prev = from;
    for i in 1..=n {
        let p = bezier4(from, c1, c2, to, i as f32 / n as f32);
        if (i - 1) % 3 != 2 {
            draw_list.add_line(prev, p, color).thickness(1.0).build();
        }
        prev = p;
    }
}

// Hover animation

/// Identifies a single interactive element inside the lock panel for hover tracking.
///
/// One shared state slot (`Option<(LockElementId, f32)>`) drives animation for both
/// the hex (slot) and the 3×3 trait grids. Only the currently-hovered element lerps in;
/// everything else renders flat. When hover moves, the new element starts at `t=0`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LockElementId {
    /// Spec hexagon at `slot` (0, 1, 2).
    Hex(u8),
    /// Trait circle at `slot` (0..3), `col` (0..3 = Adept/Master/Grandmaster), `row` (0..3).
    Trait { slot: u8, col: u8, row: u8 },
}

/// Step size applied to hover `t` per frame when the element stays hovered.
/// Picked to reach ~1.0 in ~8 frames (≈130ms at 60fps) — "subtle" per the polish brief.
const HOVER_LERP_IN: f32 = 0.12;

/// Step size applied to hover `t` per frame when nothing is hovered.
/// Slightly faster out than in so the glow doesn't linger when the mouse leaves.
const HOVER_LERP_OUT: f32 = 0.15;

/// Advance the single-element hover animation. Pure — unit-tested below.
///
/// * If `hovered` matches the stored id, lerp `t` up toward 1.0.
/// * If `hovered` is a different element, snap state to `(new, 0.0)` so the new
///   element animates in from scratch (out-animation is intentionally dropped).
/// * If nothing is hovered, decay `t` toward 0.0 and drop the state when it reaches 0.
fn tick_hover(state: &mut Option<(LockElementId, f32)>, hovered: Option<LockElementId>) {
    match (hovered, state.as_mut()) {
        (Some(id), Some((stored_id, t))) if *stored_id == id => {
            *t = (*t + HOVER_LERP_IN).min(1.0);
        }
        (Some(id), _) => {
            *state = Some((id, 0.0));
        }
        (None, Some((_, t))) => {
            *t = (*t - HOVER_LERP_OUT).max(0.0);
            if *t <= 0.0 {
                *state = None;
            }
        }
        (None, None) => {}
    }
}

fn hover_t_for(state: &Option<(LockElementId, f32)>, id: LockElementId) -> f32 {
    match state {
        Some((stored_id, t)) if *stored_id == id => *t,
        _ => 0.0,
    }
}

/// Blend `color` toward white by `t*amount`, preserving alpha.
fn brighten(color: [f32; 4], t: f32, amount: f32) -> [f32; 4] {
    let k = (t * amount).clamp(0.0, 1.0);
    [
        color[0] + (1.0 - color[0]) * k,
        color[1] + (1.0 - color[1]) * k,
        color[2] + (1.0 - color[2]) * k,
        color[3],
    ]
}

/// Which `fmt.lock_*` catalog key the lock-count indicator selects for `n`
/// locked items. Delegates to [`slavic_plural_form`], which applies the
/// active locale's CLDR rule via `current()` — callers stay language-blind.
fn lock_count_key(n: u64) -> &'static str {
    match slavic_plural_form(n) {
        SlavicPluralForm::One => "fmt.lock_one",
        SlavicPluralForm::Few => "fmt.lock_few",
        SlavicPluralForm::Many => "fmt.lock_many",
    }
}

/// Catalog key when the lock grid cannot paint — missing data or no character.
fn lock_empty_state_key(db: Option<&GameDb>, profession_name: &str) -> Option<&'static str> {
    match db {
        None => Some("lock.need_data"),
        Some(_) if profession_name.is_empty() => Some("lock.need_character"),
        Some(db) if !db.professions.contains_key(profession_name) => Some("lock.need_data"),
        Some(_) => None,
    }
}

/// Lock only the three supported specialization slots, even for malformed API/cache input.
fn lock_current_specs(locks: &mut BuildLocks, db: &GameDb, current_specs: &[(u32, Vec<u32>)]) {
    for (slot, (spec_id, trait_ids)) in current_specs.iter().take(locks.specs.len()).enumerate() {
        locks.specs[slot] = Some(*spec_id);
        if let Some(spec) = db.specializations.get(spec_id) {
            if spec.major_traits.len() == 9 {
                let mut cols = [None; 3];
                for &tid in trait_ids {
                    for (col, selected) in cols.iter_mut().enumerate() {
                        if spec.major_traits[col * 3..col * 3 + 3].contains(&tid) {
                            *selected = Some(tid);
                        }
                    }
                }
                locks.trait_locks.insert(*spec_id, cols);
            }
        }
    }
}

/// Render the spec & trait lock panel on the Improve tab.
/// Returns true if any lock state was modified.
///
/// `hover_state` holds the single currently-animating element (if any). It persists
#[allow(clippy::too_many_arguments)]
pub fn render_lock_panel(
    ui: &Ui,
    locks: &mut BuildLocks,
    expanded: &mut bool,
    db: Option<&GameDb>,
    profession_name: &str,
    current_specs: &[(u32, Vec<u32>)], // (spec_id, selected_trait_ids) from current build
    current_build: &gw2_core::types::ResolvedBuild,
    hover_state: &mut Option<(LockElementId, f32)>,
) -> bool {
    let mut modified = false;
    // Set when the mouse lies over an interactive element this frame. Used to
    // advance the single-element hover animation at the end of the function.
    let mut hovered_now: Option<LockElementId> = None;
    let spacing = 4.0_f32;

    // Collapsible header
    ui.dummy([0.0, spacing]);
    {
        let pos = ui.cursor_screen_pos();
        let width = ui.content_region_avail()[0];
        let draw_list = ui.get_window_draw_list();
        draw_list
            .add_rect(
                [pos[0], pos[1]],
                [pos[0] + width, pos[1] + 18.0],
                crate::ui::theme::pal().header,
            )
            .filled(true)
            .build();
        let title = t("section.locks");
        let header_color = crate::ui::theme::pal().gold;
        draw_list.add_text([pos[0] + 6.0, pos[1] + 2.0], header_color, &title);
        // Collapse indicator
        let indicator = if *expanded { "v" } else { ">" };
        let iw = ui.calc_text_size(indicator)[0];
        draw_list.add_text(
            [pos[0] + width - iw - 6.0, pos[1] + 2.0],
            header_color,
            indicator,
        );
    }
    // Invisible button for click detection on the header
    if ui.invisible_button("##locks_header", [ui.content_region_avail()[0], 18.0]) {
        *expanded = !*expanded;
    }
    ui.dummy([0.0, spacing * 0.5]);

    if !*expanded {
        return modified;
    }

    if let Some(key) = lock_empty_state_key(db, profession_name) {
        ui.text_colored(DIM_COLOR, format!("  {}", t(key)));
        return modified;
    }
    let db = db.expect("lock_empty_state_key is None only when db and profession exist");

    // (Spec-picker scaffolding was here. Removed because the two Vecs were
    // computed every render frame and never read. Re-introduce alongside the
    // picker when it's actually wired up.)

    let mouse_pos = ui.io().mouse_pos;
    let mouse_clicked = ui.is_mouse_clicked(nexus::imgui::MouseButton::Left);
    let right_clicked = ui.is_mouse_clicked(nexus::imgui::MouseButton::Right);

    // Render 3 spec rows — sized for content area (wide), scale-aware
    let font_size = ui.current_font_size();
    let s = (font_size / 13.0).max(0.5); // derive scale from font size (13px baseline)
    let avail_width = ui.content_region_avail()[0];
    let hex_radius = (22.0 * s).round();
    let hex_area_width = spec_hex_width(ui, hex_radius, s);
    let trait_area_width = avail_width - hex_area_width - 8.0 * s;
    let col_spacing = trait_area_width / 3.0;
    let row_height = (28.0 * s).round();
    // Fill the row; ~2px gap so neighbors can almost touch.
    let circle_radius = (row_height * 0.5 - 1.0).max(8.0);

    for slot in 0..3_usize {
        let row_start = ui.cursor_screen_pos();
        let total_row_height = hex_radius * 2.0 + 22.0 * s; // hex + name below
                                                            // Reserve enough height for the grid (max of hex height or 3 trait rows)
        let grid_height = row_height * 3.0 + 8.0 * s;
        let section_height = total_row_height.max(grid_height);

        // Hexagon (spec identity)
        let hex_center = [
            row_start[0] + hex_area_width / 2.0,
            row_start[1] + section_height / 2.0,
        ];

        let spec_id = locks.specs[slot];
        let spec_locked = spec_id.is_some();
        let spec_name = spec_id
            .and_then(|id| {
                db.specializations
                    .get(&id)
                    .map(|s| db.loc_spec(id, &s.name))
            })
            .or_else(|| {
                current_specs.get(slot).and_then(|(id, _)| {
                    db.specializations
                        .get(id)
                        .map(|s| db.loc_spec(*id, &s.name))
                })
            })
            .unwrap_or("(empty)");
        let is_elite = spec_id
            .and_then(|id| db.specializations.get(&id))
            .is_some_and(|s| s.elite);

        let hex_id = LockElementId::Hex(slot as u8);
        let hex_t = hover_t_for(hover_state, hex_id);
        {
            let draw_list = ui.get_window_draw_list();

            // Draw hexagon — radius, outline thickness, and brightness lerp in on hover.
            let hex_color_base = if spec_locked {
                locked_color()
            } else {
                DIM_COLOR
            };
            let hex_color = brighten(hex_color_base, hex_t, 0.3);
            let hex_radius_anim = hex_radius + 2.0 * hex_t;
            if spec_locked {
                draw_hexagon(
                    &draw_list,
                    hex_center,
                    hex_radius_anim,
                    color_u32(crate::ui::theme::with_alpha(
                        crate::ui::theme::pal().button,
                        0.6,
                    )),
                    true,
                    2.0,
                );
            }
            draw_hexagon(
                &draw_list,
                hex_center,
                hex_radius_anim,
                color_u32(hex_color),
                false,
                2.0 + 1.5 * hex_t,
            );

            // Subtle glow ring on hover — faint outer hex that fades in with t.
            if hex_t > 0.0 {
                let glow = [
                    hex_color_base[0],
                    hex_color_base[1],
                    hex_color_base[2],
                    0.35 * hex_t,
                ];
                draw_hexagon(
                    &draw_list,
                    hex_center,
                    hex_radius_anim + 4.0,
                    color_u32(glow),
                    false,
                    1.5,
                );
            }

            if spec_locked {
                draw_list
                    .add_circle(
                        hex_center,
                        hex_radius_anim + 3.0,
                        color_u32(crate::ui::theme::with_alpha(
                            crate::ui::theme::pal().gold,
                            0.8,
                        )),
                    )
                    .thickness(2.0)
                    .build();
            }

            let spec_icon = spec_id
                .or_else(|| current_specs.get(slot).map(|(id, _)| *id))
                .and_then(|id| crate::ui::icons::spec_url(db, id));
            if spec_icon.is_some() {
                let r = hex_radius_anim * 0.72;
                crate::ui::icons::paint_on(
                    &draw_list,
                    spec_icon,
                    [hex_center[0] - r, hex_center[1] - r],
                    [hex_center[0] + r, hex_center[1] + r],
                    [1.0, 1.0, 1.0, 1.0],
                );
            } else if is_elite {
                let e_pos = [hex_center[0] - 3.0, hex_center[1] - 5.0];
                draw_list.add_text(e_pos, color_u32(ELITE_COLOR), "E");
            } else if !spec_locked {
                // Slot number
                let num_text = format!("{}", slot + 1);
                let tw = ui.calc_text_size(&num_text)[0];
                draw_list.add_text(
                    [hex_center[0] - tw / 2.0, hex_center[1] - 5.0],
                    color_u32(DIM_COLOR),
                    &num_text,
                );
            }

            // Spec name below hexagon (full name — content area has room)
            let nw = ui.calc_text_size(spec_name)[0];
            draw_list.add_text(
                [hex_center[0] - nw / 2.0, hex_center[1] + hex_radius + 3.0],
                color_u32(crate::ui::theme::pal().cream),
                spec_name,
            );
        } // DrawListMut dropped

        // Hexagon click detection — toggle lock. Hit box follows the animated
        // radius so the cursor never falls out while the glow is still visible.
        if is_in_hexagon(mouse_pos, hex_center, hex_radius + 4.0 + 2.0 * hex_t) {
            hovered_now = Some(hex_id);
            // Tooltip
            crate::ui::theme::wide_tooltip(ui, |ui| {
                if spec_locked {
                    ui.text(format!("{} ({})", spec_name, t("lock.locked")));
                    ui.text_colored(DIM_COLOR, t("lock.click_unlock"));
                } else {
                    ui.text(spec_name);
                    ui.text_colored(DIM_COLOR, t("lock.click_lock"));
                }
            });

            if mouse_clicked {
                if spec_locked {
                    // Unlock: clear spec lock and associated trait locks
                    if let Some(old_id) = locks.specs[slot] {
                        locks.trait_locks.remove(&old_id);
                    }
                    locks.specs[slot] = None;
                } else {
                    // Lock: use current build's spec for this slot
                    if let Some((cur_id, _)) = current_specs.get(slot) {
                        locks.specs[slot] = Some(*cur_id);
                    }
                }
                modified = true;
            }
        }

        // Trait grid (3 columns × 3 rows)
        // Only show traits when a spec is set (locked or from current build)
        let active_spec_id = spec_id.or_else(|| current_specs.get(slot).map(|(id, _)| *id));
        let selected_traits: Vec<u32> = current_specs
            .get(slot)
            .map(|(_, traits)| traits.clone())
            .unwrap_or_default();

        if let Some(sid) = active_spec_id {
            if let Some(spec) = db.specializations.get(&sid) {
                if spec.major_traits.len() == 9 {
                    let grid_x = row_start[0] + hex_area_width + 4.0;
                    let grid_y = row_start[1] + 2.0;
                    let mut selected_link: [Option<([f32; 2], [f32; 2])>; 3] = [None; 3];

                    for col in 0..3_usize {
                        for row in 0..3_usize {
                            let trait_idx = col * 3 + row;
                            let trait_id = spec.major_traits[trait_idx];
                            let trait_info = db.traits.get(&trait_id);
                            let trait_name = trait_info
                                .map(|t| db.loc_trait(trait_id, &t.name))
                                .unwrap_or("?");

                            let cx = grid_x + col as f32 * col_spacing + circle_radius + 2.0;
                            let cy = grid_y + row as f32 * row_height + row_height / 2.0;

                            let is_selected = selected_traits.contains(&trait_id);
                            if is_selected {
                                let text_end =
                                    cx + circle_radius + 8.0 + ui.calc_text_size(trait_name)[0];
                                selected_link[col] =
                                    Some(([text_end + 4.0, cy], [cx - circle_radius - 1.0, cy]));
                            }
                            let is_locked = locks
                                .locked_trait(sid, col)
                                .is_some_and(|id| id == trait_id);

                            let trait_element = LockElementId::Trait {
                                slot: slot as u8,
                                col: col as u8,
                                row: row as u8,
                            };
                            let trait_t = hover_t_for(hover_state, trait_element);
                            let circle_radius_anim = circle_radius + 0.5 * trait_t;

                            {
                                let draw_list = ui.get_window_draw_list();

                                // Circle
                                let (fill_color, outline_color) = if is_locked {
                                    (locked_color(), locked_color())
                                } else if is_selected {
                                    (SELECTED_COLOR, SELECTED_COLOR)
                                } else {
                                    (AVAILABLE_COLOR, DIM_COLOR)
                                };

                                if is_selected || is_locked {
                                    draw_list
                                        .add_circle(
                                            [cx, cy],
                                            circle_radius_anim,
                                            color_u32(brighten(fill_color, trait_t, 0.3)),
                                        )
                                        .filled(true)
                                        .build();
                                }
                                draw_list
                                    .add_circle(
                                        [cx, cy],
                                        circle_radius_anim,
                                        color_u32(brighten(outline_color, trait_t, 0.3)),
                                    )
                                    .thickness(1.0 + 1.0 * trait_t)
                                    .build();

                                if let Some(url) = crate::ui::icons::trait_url(db, trait_id) {
                                    let r = circle_radius_anim;
                                    crate::ui::icons::paint_on(
                                        &draw_list,
                                        Some(url),
                                        [cx - r, cy - r],
                                        [cx + r, cy + r],
                                        [1.0, 1.0, 1.0, 1.0],
                                    );
                                }

                                // Subtle glow ring on hover.
                                if trait_t > 0.0 {
                                    let glow = [
                                        outline_color[0],
                                        outline_color[1],
                                        outline_color[2],
                                        0.35 * trait_t,
                                    ];
                                    draw_list
                                        .add_circle(
                                            [cx, cy],
                                            circle_radius_anim + 3.0,
                                            color_u32(glow),
                                        )
                                        .thickness(1.0)
                                        .build();
                                }

                                if is_locked {
                                    draw_list
                                        .add_circle(
                                            [cx, cy],
                                            circle_radius_anim + 3.0,
                                            color_u32(crate::ui::theme::with_alpha(
                                                crate::ui::theme::pal().gold,
                                                0.8,
                                            )),
                                        )
                                        .thickness(1.5)
                                        .build();
                                }

                                // Trait name to the right of circle
                                let text_color_base = if is_locked {
                                    locked_color()
                                } else if is_selected {
                                    SELECTED_COLOR
                                } else {
                                    DIM_COLOR
                                };
                                draw_list.add_text(
                                    [cx + circle_radius + 8.0, cy - 6.0],
                                    color_u32(brighten(text_color_base, trait_t, 0.25)),
                                    trait_name,
                                );
                            } // DrawListMut dropped

                            // Click/hover detection — hit box follows animated radius.
                            if is_in_circle(
                                mouse_pos,
                                [cx, cy],
                                circle_radius + 4.0 + 0.5 * trait_t,
                            ) {
                                hovered_now = Some(trait_element);
                                // Tooltip with full trait info
                                crate::ui::theme::wide_tooltip(ui, |ui| {
                                    if let Some(tip) =
                                        crate::ui::comparison::inspect_text(trait_name, db)
                                    {
                                        ui.text(tip);
                                    } else {
                                        ui.text(trait_name);
                                    }
                                    if is_locked {
                                        ui.text_colored(locked_color(), t("lock.locked"));
                                        ui.text_colored(DIM_COLOR, t("lock.click_unlock"));
                                    } else {
                                        ui.text_colored(DIM_COLOR, t("lock.click_lock"));
                                    }
                                });

                                if mouse_clicked {
                                    if is_locked {
                                        if let Some(cols) = locks.trait_locks.get_mut(&sid) {
                                            cols[col] = None;
                                        }
                                    } else {
                                        let entry =
                                            locks.trait_locks.entry(sid).or_insert([None; 3]);
                                        entry[col] = Some(trait_id);
                                        if locks.specs[slot].is_none() {
                                            locks.specs[slot] = Some(sid);
                                        }
                                    }
                                    modified = true;
                                }

                                if right_clicked && is_locked {
                                    // Right-click to unlock
                                    if let Some(cols) = locks.trait_locks.get_mut(&sid) {
                                        cols[col] = None;
                                    }
                                    modified = true;
                                }
                            }
                        }
                    }
                    {
                        let draw_list = ui.get_window_draw_list();
                        for col in 0..2 {
                            if let (Some((from, _)), Some((_, to))) =
                                (selected_link[col], selected_link[col + 1])
                            {
                                draw_ghost_link(&draw_list, from, to);
                            }
                        }
                    }
                }
            }
        }

        // Reserve layout space
        ui.dummy([avail_width, section_height]);
        ui.dummy([0.0, 4.0]); // Gap between spec rows

        // Separator between rows
        {
            let sep_pos = ui.cursor_screen_pos();
            let draw_list = ui.get_window_draw_list();
            draw_list
                .add_line(
                    [sep_pos[0], sep_pos[1] - 2.0],
                    [sep_pos[0] + avail_width, sep_pos[1] - 2.0],
                    color_u32(crate::ui::theme::with_alpha(
                        crate::ui::theme::pal().chip_idle_rim,
                        0.3,
                    )),
                )
                .build();
        }
    }

    // Lock All / Unlock All buttons
    ui.dummy([0.0, 4.0]);
    let btn_width = (avail_width - 6.0) / 2.0;
    if crate::ui::theme::gold_button_sized(ui, t("btn.lock_all"), [btn_width, 0.0]) {
        lock_current_specs(locks, db, current_specs);
        let gear_names = resolved_gear_names(current_build);
        for slot in GearSlot::ALL {
            if let Some(name) = gear_names[slot as usize].as_deref() {
                if let Some(id) = db.itemstat_by_name(name).map(|is| is.id) {
                    locks.gear_locks.insert(slot, id);
                }
            }
        }
        modified = true;
    }
    ui.same_line();
    if crate::ui::theme::gold_button_sized(ui, t("btn.unlock_all"), [btn_width, 0.0]) {
        locks.specs = [None; 3];
        locks.trait_locks.clear();
        locks.gear_locks.clear();
        modified = true;
    }

    let lock_count = locks.specs.iter().filter(|s| s.is_some()).count()
        + locks
            .trait_locks
            .values()
            .flat_map(|c| c.iter())
            .filter(|t| t.is_some())
            .count()
        + locks.gear_locks.len();
    if lock_count > 0 {
        let lock_msg = tf(
            lock_count_key(lock_count as u64),
            &[("n", &lock_count.to_string())],
        );
        ui.text_colored(locked_color(), format!("  {lock_msg}"));
    }

    // Advance the hover animation for next frame.
    tick_hover(hover_state, hovered_now);

    modified
}

/// The equipped prefix name for each of the sixteen canonical gear slots, in
/// `GearSlot::ALL` order. Resolved builds carry one prefix per weapon set, so
/// both hands of a set share its name; empty slots stay `None`.
fn resolved_gear_names(build: &gw2_core::types::ResolvedBuild) -> [Option<String>; 16] {
    use crate::ui::gear_sheet::piece_gear_slot;

    let mut names: [Option<String>; 16] = Default::default();
    let mut put = |piece_slot: &str, piece: &gw2_core::types::ResolvedGearPiece| {
        if let Some(slot) = piece_gear_slot(piece_slot) {
            names[slot as usize] = Some(piece.stat_prefix.clone()).filter(|n| !n.is_empty());
        }
    };
    for (slot, piece) in build.armor.iter().map(|p| (p.slot.as_str(), p)) {
        put(slot, piece);
    }
    for p in &build.trinkets {
        put(&p.slot, p);
    }
    for (i, set) in build.weapons.iter().enumerate() {
        if set.main_hand.is_some() {
            let slot = if i == 0 {
                GearSlot::WeaponSet1Main
            } else {
                GearSlot::WeaponSet2Main
            };
            names[slot as usize] = (!set.stat_prefix.is_empty()).then(|| set.stat_prefix.clone());
        }
        if set.off_hand.is_some() {
            let slot = if i == 0 {
                GearSlot::WeaponSet1Off
            } else {
                GearSlot::WeaponSet2Off
            };
            names[slot as usize] = (!set.stat_prefix.is_empty()).then(|| set.stat_prefix.clone());
        }
    }
    names
}

/// Suggestion trait names are English API names. Localized display must not
/// be the match key (de/es/fr/zh pack attached).
fn optimized_trait_selected(selected: &[String], english_name: &str) -> bool {
    selected.iter().any(|n| n == english_name)
}

/// Render the optimized build's specs & traits in the same visual style as the lock panel.
/// Read-only — no click interactions. Matches the lock panel layout for side-by-side comparison.
/// `worn` is the player's equipped specs in the same shape. When present,
/// every spec and trait that differs from it gets a green halo, so a build
/// they are about to equip shows at a glance what Choya moved and what it
/// left alone. `None` while rendering the equipped build itself - there is
/// nothing to mark against.
pub fn render_optimized_specs_panel(
    ui: &Ui,
    db: Option<&GameDb>,
    suggestion_specs: &[(String, Vec<String>)], // (spec_name, [trait1, trait2, trait3])
    title: &str,
    worn: Option<&[(String, Vec<String>)]>,
) {
    let spacing = 4.0_f32;

    // Header — same gold tick + vertically centered title as SKILLS / CHARACTER.
    ui.dummy([0.0, spacing]);
    {
        let pos = ui.cursor_screen_pos();
        let width = ui.content_region_avail()[0];
        let bar_h = 22.0;
        let th = ui.calc_text_size(title)[1];
        let ty = pos[1] + ((bar_h - th) * 0.5).round();
        let draw_list = ui.get_window_draw_list();
        draw_list
            .add_rect(
                [pos[0], pos[1]],
                [pos[0] + width, pos[1] + bar_h],
                [0.10, 0.19, 0.12, 0.9],
            )
            .filled(true)
            .build();
        crate::ui::theme::paint_header_accent(&draw_list, pos[0], pos[1], bar_h);
        draw_list.add_text(
            [crate::ui::theme::header_title_x(pos[0]), ty],
            color_u32([0.3, 1.0, 0.5, 1.0]),
            title,
        );
    }
    ui.dummy([ui.content_region_avail()[0], 22.0]);
    ui.dummy([0.0, spacing * 0.5]);

    if suggestion_specs.is_empty() {
        ui.text_colored(DIM_COLOR, format!("  {}", t("lock.no_result")));
        return;
    }

    // Match sizing from lock panel
    let font_size = ui.current_font_size();
    let s = (font_size / 13.0).max(0.5);
    let avail_width = ui.content_region_avail()[0];
    let hex_radius = (22.0 * s).round();
    let hex_area_width = spec_hex_width(ui, hex_radius, s);
    let trait_area_width = avail_width - hex_area_width - 8.0 * s;
    let col_spacing = trait_area_width / 3.0;
    let row_height = (28.0 * s).round();
    let circle_radius = (row_height * 0.5 - 1.0).max(8.0);

    let spec_by_name: std::collections::HashMap<&str, &gw2_api::models::Specialization> = db
        .map(|db| {
            db.specializations
                .values()
                .map(|sp| (sp.name.as_str(), sp))
                .collect()
        })
        .unwrap_or_default();

    for (slot, (spec_name, trait_names)) in suggestion_specs.iter().enumerate() {
        let row_start = ui.cursor_screen_pos();
        let total_row_height = hex_radius * 2.0 + 22.0 * s;
        let grid_height = row_height * 3.0 + 8.0 * s;
        let section_height = total_row_height.max(grid_height);

        // Strip " [E]" suffix for DB lookup (suggestion names include elite marker)
        let lookup_name = spec_name.strip_suffix(" [E]").unwrap_or(spec_name.as_str());
        let spec_info = spec_by_name.get(lookup_name);
        let is_elite = spec_info.is_some_and(|s| s.elite) || spec_name.ends_with(" [E]");

        // Traits the player already runs in THIS spec. A spec they are not
        // running at all has none, so every trait in it reads as changed -
        // which is the truth.
        let worn_traits: Option<&[String]> = worn.map(|specs| {
            specs
                .iter()
                .find(|(n, _)| {
                    n.strip_suffix(" [E]")
                        .unwrap_or(n.as_str())
                        .eq_ignore_ascii_case(lookup_name)
                })
                .map_or(&[][..], |(_, traits)| traits.as_slice())
        });
        let spec_changed = worn_traits.is_some_and(|w| w.is_empty());

        // Hexagon (spec identity)
        let hex_center = [
            row_start[0] + hex_area_width / 2.0,
            row_start[1] + section_height / 2.0,
        ];

        let optimized_color: [f32; 4] = [0.3, 1.0, 0.5, 1.0]; // Green for optimized

        {
            let draw_list = ui.get_window_draw_list();
            // Filled hexagon
            draw_hexagon(
                &draw_list,
                hex_center,
                hex_radius,
                color_u32([0.05, 0.2, 0.1, 0.6]),
                true,
                2.0,
            );
            draw_hexagon(
                &draw_list,
                hex_center,
                hex_radius,
                color_u32(optimized_color),
                false,
                2.0,
            );
            if spec_changed {
                crate::ui::theme::paint_changed_circle(&draw_list, hex_center, hex_radius);
            }

            if let Some(url) = db
                .and_then(|d| crate::ui::icons::spec_url_by_name(d, lookup_name))
                .or_else(|| spec_info.and_then(|s| s.icon.as_deref()))
            {
                let r = hex_radius * 0.72;
                crate::ui::icons::paint_on(
                    &draw_list,
                    Some(url),
                    [hex_center[0] - r, hex_center[1] - r],
                    [hex_center[0] + r, hex_center[1] + r],
                    [1.0, 1.0, 1.0, 1.0],
                );
            } else if is_elite {
                let e_pos = [hex_center[0] - 3.0, hex_center[1] - 5.0];
                draw_list.add_text(e_pos, color_u32(ELITE_COLOR), "E");
            } else {
                let num_text = format!("{}", slot + 1);
                let tw = ui.calc_text_size(&num_text)[0];
                draw_list.add_text(
                    [hex_center[0] - tw / 2.0, hex_center[1] - 5.0],
                    color_u32(DIM_COLOR),
                    &num_text,
                );
            }

            // Spec name below hexagon (strip " [E]" — hexagon already has elite marker)
            let display_name = spec_info
                .and_then(|s| db.map(|d| d.loc_spec(s.id, lookup_name)))
                .unwrap_or(lookup_name);
            let nw = ui.calc_text_size(display_name)[0];
            draw_list.add_text(
                [hex_center[0] - nw / 2.0, hex_center[1] + hex_radius + 3.0],
                color_u32(crate::ui::theme::pal().cream),
                display_name,
            );
        }

        // Trait display (3 columns, 3 rows each)
        if let Some(spec) = spec_info {
            if spec.major_traits.len() == 9 {
                let grid_x = row_start[0] + hex_area_width + 4.0;
                let grid_y = row_start[1] + 2.0;
                let mut selected_link: [Option<([f32; 2], [f32; 2])>; 3] = [None; 3];

                // `col` drives the hex-grid geometry as well as the link slot.
                #[allow(clippy::needless_range_loop)]
                for col in 0..3_usize {
                    for row in 0..3_usize {
                        let trait_idx = col * 3 + row;
                        let trait_id = spec.major_traits[trait_idx];
                        let trait_info = db.and_then(|d| d.traits.get(&trait_id));
                        let trait_name = trait_info
                            .map(|t| {
                                db.map(|d| d.loc_trait(trait_id, &t.name))
                                    .unwrap_or(t.name.as_str())
                            })
                            .unwrap_or("?");

                        let cx = grid_x + col as f32 * col_spacing + circle_radius + 2.0;
                        let cy = grid_y + row as f32 * row_height + row_height / 2.0;

                        let is_selected = trait_info
                            .map(|t| optimized_trait_selected(trait_names, &t.name))
                            .unwrap_or(false);
                        if is_selected {
                            let text_end =
                                cx + circle_radius + 8.0 + ui.calc_text_size(trait_name)[0];
                            selected_link[col] =
                                Some(([text_end + 4.0, cy], [cx - circle_radius - 1.0, cy]));
                        }

                        {
                            let draw_list = ui.get_window_draw_list();
                            let (fill_color, outline_color) = if is_selected {
                                (optimized_color, optimized_color)
                            } else {
                                (AVAILABLE_COLOR, DIM_COLOR)
                            };

                            if is_selected {
                                draw_list
                                    .add_circle([cx, cy], circle_radius, color_u32(fill_color))
                                    .filled(true)
                                    .build();
                            }
                            draw_list
                                .add_circle([cx, cy], circle_radius, color_u32(outline_color))
                                .build();
                            if is_selected
                                && worn_traits.is_some_and(|w| {
                                    trait_info
                                        .is_some_and(|ti| !optimized_trait_selected(w, &ti.name))
                                })
                            {
                                crate::ui::theme::paint_changed_circle(
                                    &draw_list,
                                    [cx, cy],
                                    circle_radius,
                                );
                            }

                            if let Some(url) = trait_info.and_then(|t| t.icon.as_deref()) {
                                let r = circle_radius;
                                crate::ui::icons::paint_on(
                                    &draw_list,
                                    Some(url),
                                    [cx - r, cy - r],
                                    [cx + r, cy + r],
                                    [1.0, 1.0, 1.0, 1.0],
                                );
                            }

                            // Trait name
                            let text_color = if is_selected {
                                optimized_color
                            } else {
                                DIM_COLOR
                            };
                            draw_list.add_text(
                                [cx + circle_radius + 8.0, cy - 6.0],
                                color_u32(text_color),
                                trait_name,
                            );
                        }

                        // Tooltip on hover
                        let mouse_pos = ui.io().mouse_pos;
                        if is_in_circle(mouse_pos, [cx, cy], circle_radius + 4.0) {
                            crate::ui::theme::wide_tooltip(ui, |ui| {
                                if let Some(tip) = db.and_then(|d| {
                                    crate::ui::comparison::inspect_text(trait_name, d)
                                }) {
                                    ui.text(tip);
                                } else {
                                    ui.text(trait_name);
                                }
                                if is_selected {
                                    ui.text_colored(optimized_color, t("section.optimized_specs"));
                                }
                            });
                        }
                    }
                }
                {
                    let draw_list = ui.get_window_draw_list();
                    for col in 0..2 {
                        if let (Some((from, _)), Some((_, to))) =
                            (selected_link[col], selected_link[col + 1])
                        {
                            draw_ghost_link(&draw_list, from, to);
                        }
                    }
                }
            }
        } else {
            // No DB data — just show trait names as text
            let grid_x = row_start[0] + hex_area_width + 4.0;
            let grid_y = row_start[1] + 2.0;
            let draw_list = ui.get_window_draw_list();
            for (i, tn) in trait_names.iter().enumerate() {
                draw_list.add_text(
                    [grid_x, grid_y + i as f32 * row_height],
                    color_u32(optimized_color),
                    tn,
                );
            }
        }

        ui.dummy([avail_width, section_height]);
        ui.dummy([0.0, 4.0]);

        // Separator
        {
            let sep_pos = ui.cursor_screen_pos();
            let draw_list = ui.get_window_draw_list();
            draw_list
                .add_line(
                    [sep_pos[0], sep_pos[1] - 2.0],
                    [sep_pos[0] + avail_width, sep_pos[1] - 2.0],
                    color_u32([0.1, 0.25, 0.1, 0.3]),
                )
                .build();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_all_bounds_specializations_and_ignores_unknown_traits() {
        let mut db = GameDb::empty_for_tests();
        db.specializations.insert(
            1,
            serde_json::from_value(serde_json::json!({
                "id": 1, "name": "Test", "profession": "Guardian", "elite": false,
                "icon": "", "background": "", "minor_traits": [],
                "major_traits": [10, 11, 12, 20, 21, 22, 30, 31, 32]
            }))
            .unwrap(),
        );
        let mut locks = BuildLocks::default();
        lock_current_specs(
            &mut locks,
            &db,
            &[
                (1, vec![11, 22, 30, 999]),
                (2, vec![]),
                (3, vec![]),
                (4, vec![]),
            ],
        );
        assert_eq!(locks.specs, [Some(1), Some(2), Some(3)]);
        assert_eq!(
            locks.trait_locks.get(&1),
            Some(&[Some(11), Some(22), Some(30)])
        );
        assert!(!locks.trait_locks.contains_key(&4));
        db.specializations
            .get_mut(&1)
            .unwrap()
            .major_traits
            .truncate(8);
        let mut malformed = BuildLocks::default();
        lock_current_specs(&mut malformed, &db, &[(1, vec![11, 22, 30])]);
        assert_eq!(malformed.specs, [Some(1), None, None]);
        assert!(malformed.trait_locks.is_empty());
    }

    #[test]
    fn tick_hover_starts_state_at_zero_when_first_hovered() {
        let mut state: Option<(LockElementId, f32)> = None;
        tick_hover(&mut state, Some(LockElementId::Hex(0)));
        assert_eq!(state, Some((LockElementId::Hex(0), 0.0)));
    }

    #[test]
    fn tick_hover_lerps_up_while_same_element_hovered() {
        let mut state: Option<(LockElementId, f32)> = Some((LockElementId::Hex(1), 0.0));
        tick_hover(&mut state, Some(LockElementId::Hex(1)));
        let (_, t) = state.expect("state remains set while hovered");
        assert!((t - HOVER_LERP_IN).abs() < f32::EPSILON);
    }

    #[test]
    fn tick_hover_clamps_at_one_after_enough_frames() {
        let mut state: Option<(LockElementId, f32)> = Some((LockElementId::Hex(0), 0.95));
        tick_hover(&mut state, Some(LockElementId::Hex(0)));
        let (_, t) = state.expect("hovered state is preserved");
        assert!((t - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tick_hover_snaps_to_new_element_at_zero() {
        let mut state: Option<(LockElementId, f32)> = Some((LockElementId::Hex(0), 0.9));
        let new_id = LockElementId::Trait {
            slot: 1,
            col: 0,
            row: 2,
        };
        tick_hover(&mut state, Some(new_id));
        assert_eq!(state, Some((new_id, 0.0)));
    }

    #[test]
    fn tick_hover_decays_when_nothing_hovered_and_clears_at_zero() {
        let mut state: Option<(LockElementId, f32)> = Some((LockElementId::Hex(2), 0.2));
        tick_hover(&mut state, None);
        let (_, t) = state.expect("state persists while t > 0");
        assert!((t - (0.2 - HOVER_LERP_OUT)).abs() < f32::EPSILON);

        // One more frame drops us to zero and clears the state.
        tick_hover(&mut state, None);
        assert!(state.is_none(), "state should clear once t reaches zero");
    }

    #[test]
    fn hover_t_for_returns_zero_unless_id_matches() {
        let state: Option<(LockElementId, f32)> = Some((LockElementId::Hex(0), 0.7));
        assert!((hover_t_for(&state, LockElementId::Hex(0)) - 0.7).abs() < f32::EPSILON);
        assert_eq!(hover_t_for(&state, LockElementId::Hex(1)), 0.0);
        assert_eq!(
            hover_t_for(
                &None,
                LockElementId::Trait {
                    slot: 0,
                    col: 0,
                    row: 0
                }
            ),
            0.0
        );
    }

    #[test]
    fn brighten_preserves_alpha_and_moves_toward_white() {
        let c = brighten([0.0, 0.0, 0.0, 0.5], 1.0, 0.5);
        assert!((c[0] - 0.5).abs() < f32::EPSILON);
        assert!((c[3] - 0.5).abs() < f32::EPSILON); // alpha untouched

        let c2 = brighten([1.0, 1.0, 1.0, 1.0], 1.0, 1.0);
        assert_eq!(c2, [1.0, 1.0, 1.0, 1.0]); // white stays white
    }

    #[test]
    fn ghost_bezier_starts_and_ends_on_anchors() {
        let a = [10.0, 20.0];
        let b = [110.0, 80.0];
        let c1 = [50.0, 20.0];
        let c2 = [70.0, 80.0];
        let p0 = bezier4(a, c1, c2, b, 0.0);
        let p1 = bezier4(a, c1, c2, b, 1.0);
        assert!((p0[0] - a[0]).abs() < 0.01 && (p0[1] - a[1]).abs() < 0.01);
        assert!((p1[0] - b[0]).abs() < 0.01 && (p1[1] - b[1]).abs() < 0.01);
    }

    #[test]
    fn resolved_gear_names_map_pieces_to_their_slots() {
        use gw2_core::types::{ResolvedBuild, ResolvedGearPiece, ResolvedWeaponSet, WeaponInfo};

        let piece = |slot: &str, prefix: &str| ResolvedGearPiece {
            slot: slot.to_string(),
            name: format!("{slot} item"),
            stat_prefix: prefix.to_string(),
            ..Default::default()
        };
        let weapon = |name: &str| WeaponInfo {
            name: name.to_string(),
            ..Default::default()
        };
        let build = ResolvedBuild {
            armor: vec![piece("Helm", "Berserker's"), piece("Boots", "Cavalier's")],
            trinkets: vec![piece("Amulet", "Sinister")],
            // Set 1 two-handed (main only), Set 2 dual-wield.
            weapons: vec![
                ResolvedWeaponSet {
                    label: "Set 1".into(),
                    stat_prefix: "Assassin's".into(),
                    main_hand: Some(weapon("Greatsword")),
                    off_hand: None,
                    ..Default::default()
                },
                ResolvedWeaponSet {
                    label: "Set 2".into(),
                    stat_prefix: "Viper's".into(),
                    main_hand: Some(weapon("Sword")),
                    off_hand: Some(weapon("Dagger")),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let names = resolved_gear_names(&build);
        assert_eq!(
            names[GearSlot::Helm as usize].as_deref(),
            Some("Berserker's")
        );
        assert_eq!(
            names[GearSlot::Boots as usize].as_deref(),
            Some("Cavalier's")
        );
        assert_eq!(
            names[GearSlot::Shoulders as usize],
            None,
            "unequipped slots stay empty"
        );
        assert_eq!(
            names[GearSlot::Amulet as usize].as_deref(),
            Some("Sinister")
        );
        assert_eq!(
            names[GearSlot::WeaponSet1Main as usize].as_deref(),
            Some("Assassin's")
        );
        assert_eq!(names[GearSlot::WeaponSet1Off as usize], None);
        assert_eq!(
            names[GearSlot::WeaponSet2Main as usize].as_deref(),
            Some("Viper's")
        );
        assert_eq!(
            names[GearSlot::WeaponSet2Off as usize].as_deref(),
            Some("Viper's")
        );
    }

    #[test]
    fn lock_string_has_no_space_runs() {
        // GLM F31: the stale-trait-lock explanation shown to the player used to
        // be a multi-line string literal joined without re-wrapping, leaving
        // literal 35-space runs in the rendered text. Any run of 2+ consecutive
        // spaces means that bug (or one like it) is back.
        let text = gw2_optimizer::engine::STALE_TRAIT_LOCK_EXPLANATION;
        assert!(
            !text.contains("  "),
            "stale-trait-lock explanation has a run of consecutive spaces: {text:?}",
        );
    }

    #[test]
    fn lock_count_uses_slavic_plural_form() {
        // Selection follows the active locale (set here so a parallel addon
        // test cannot leave us on ru/pl). Per-language CLDR tables live in
        // gw2-core i18n; this pins the lock-panel key picker + render site.
        gw2_core::i18n::set_language("en");
        assert_eq!(lock_count_key(1), "fmt.lock_one");
        for n in [0, 2, 21, 22, 101] {
            assert_eq!(lock_count_key(n), "fmt.lock_many", "en n={n}");
        }

        gw2_core::i18n::set_language("pl");
        assert_eq!(lock_count_key(1), "fmt.lock_one");
        assert_eq!(lock_count_key(21), "fmt.lock_many");
        assert_eq!(lock_count_key(2), "fmt.lock_few");

        gw2_core::i18n::set_language("ru");
        assert_eq!(lock_count_key(21), "fmt.lock_one");
        assert_eq!(lock_count_key(2), "fmt.lock_few");

        gw2_core::i18n::set_language("fr");
        assert_eq!(lock_count_key(0), "fmt.lock_one");
        assert_eq!(lock_count_key(2), "fmt.lock_many");

        gw2_core::i18n::set_language("en");

        // DECISION-29 (C31/G4): `lock_count_key` alone is not sufficient --
        // it can stay green while `render_lock_panel`'s call site quietly
        // reverts to a hand-rolled `if lock_count == 1 { ... }` branch with a
        // hardcoded "1". Pin the actual render site by reading this file's
        // own source (the two live side by side, so this is a same-file
        // read, not a cross-file `include_str!` like the kitchen-timeout
        // gate).
        //
        // `include_str!` reads the WHOLE file, including this very test. If
        // the search ran over the whole thing, the two `.contains(..)`
        // argument literals below would trivially match themselves -- the
        // pin would pass even with the render site reverted, no matter what
        // it actually does. Cut the file at its own `#[cfg(test)]` marker so
        // only the real production code (everything above `mod tests`) is
        // searched.
        let src = include_str!("lock_panel.rs");
        let production_src = src
            .split("#[cfg(test)]")
            .next()
            .expect("lock_panel.rs must contain its own #[cfg(test)] marker");
        assert!(
            production_src.contains("lock_count_key(lock_count as u64)"),
            "render_lock_panel's lock-count indicator must call \
                 lock_count_key(lock_count as u64) -- if this reverts to a \
                 hand-rolled `if lock_count == 1` branch, lock_count_key stays \
                 green while Polish/Russian players see the wrong plural form \
                 again",
        );
        assert!(
            production_src.contains("&lock_count.to_string()"),
            "render_lock_panel's lock-count indicator must interpolate the \
                 live lock_count via &lock_count.to_string(), not a hardcoded \
                 \"1\" -- a hardcoded literal renders the wrong number the \
                 moment 21, 31, or 101 select the One form under the CLDR rule",
        );
    }

    #[test]
    fn optimized_spec_panel_matches_english_not_localized() {
        let selected = vec!["Lingering Curse".to_string()];
        assert!(optimized_trait_selected(&selected, "Lingering Curse"));
        assert!(
            !optimized_trait_selected(&selected, "Fluch der Verweilenden"),
            "de display name must not be the selection key"
        );
    }

    #[test]
    fn lock_empty_state_uses_need_keys() {
        assert_eq!(
            lock_empty_state_key(None, "Guardian"),
            Some("lock.need_data")
        );
        assert_eq!(lock_empty_state_key(None, ""), Some("lock.need_data"));
        let db = GameDb::empty_for_tests();
        assert_eq!(
            lock_empty_state_key(Some(&db), ""),
            Some("lock.need_character")
        );
        assert_eq!(
            lock_empty_state_key(Some(&db), "Guardian"),
            Some("lock.need_data"),
            "profession missing from db must not paint a blank grid"
        );

        let src = include_str!("lock_panel.rs");
        let production_src = src
            .split("#[cfg(test)]")
            .next()
            .expect("lock_panel.rs must contain its own #[cfg(test)] marker");
        assert!(
            production_src.contains("lock_empty_state_key(db, profession_name)"),
            "render_lock_panel must show lock.need_data / lock.need_character, not a silent return"
        );
    }
}
