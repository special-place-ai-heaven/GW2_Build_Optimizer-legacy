//! Interactive 6-axis radar chart for optimization weights and build comparison.
//! Uses ImGui DrawList for custom rendering with dual-polygon overlay support.

use gw2_core::i18n::t;
use gw2_optimizer::scoring::{
    OptimizationWeights, CONDI_DPS_NORM, EFFECTIVE_HEALTH_NORM, HEALING_NORM, STRIKE_DPS_NORM,
};
use nexus::imgui::Ui;

const AXIS_I18N: [&str; 6] = [
    "axis.power",
    "axis.condition",
    "axis.boon",
    "axis.heal",
    "axis.sustain",
    "axis.control",
];

/// Axis colors (RGBA) for each of the 6 axes.
const AXIS_COLORS: [[f32; 4]; 6] = [
    [1.0, 0.3, 0.3, 1.0], // Power -- red
    [0.4, 1.0, 0.4, 1.0], // Condition -- green
    [0.2, 0.6, 1.0, 1.0], // Boon Support -- blue
    [0.3, 0.9, 1.0, 1.0], // Heal -- cyan
    [0.8, 0.4, 1.0, 1.0], // Sustain -- purple
    [1.0, 0.8, 0.2, 1.0], // Control -- yellow
];

/// Color for the user's weight polygon (semi-transparent cyan).
const WEIGHTS_FILL: [f32; 4] = [0.2, 0.7, 1.0, 0.2];
const WEIGHTS_OUTLINE: [f32; 4] = [0.3, 0.8, 1.0, 0.9];

/// Color for the current build overlay (semi-transparent amber).
const CURRENT_FILL: [f32; 4] = [1.0, 0.7, 0.2, 0.15];
const CURRENT_OUTLINE: [f32; 4] = [1.0, 0.7, 0.2, 0.7];

/// Color for the optimized build overlay (semi-transparent green).
const OPTIMIZED_FILL: [f32; 4] = [0.2, 1.0, 0.4, 0.15];
const OPTIMIZED_OUTLINE: [f32; 4] = [0.2, 1.0, 0.4, 0.7];

const NUM_AXES: usize = 6;

use crate::ui::color_u32;

/// Calculate the position of a point on axis `i` at `value` (0.0-1.0).
/// Axis 0 = top (12 o'clock), going clockwise.
fn axis_point(center: [f32; 2], radius: f32, axis_index: usize, value: f32) -> [f32; 2] {
    let angle = std::f32::consts::PI * 2.0 * (axis_index as f32) / NUM_AXES as f32
        - std::f32::consts::FRAC_PI_2;
    [
        center[0] + radius * value * angle.cos(),
        center[1] + radius * value * angle.sin(),
    ]
}

/// Render the interactive radar chart for setting optimization weights.
/// Returns true if weights were modified by user interaction.
pub fn render_radar_chart(
    ui: &Ui,
    weights: &mut OptimizationWeights,
    dragging: &mut Option<usize>,
    current_build_perf: Option<&[f64; 6]>,
    optimized_perf: Option<&[f64; 6]>,
) -> bool {
    let mut modified = false;
    let avail_w = ui.content_region_avail()[0].max(156.0);
    let avail_h = ui.content_region_avail()[1];
    let size = avail_w.min((avail_h - 48.0).max(176.0)).min(avail_w);
    let indent = ((avail_w - size) * 0.5).max(0.0);
    let origin = ui.cursor_screen_pos();
    let cursor_pos = [origin[0] + indent, origin[1]];
    ui.set_cursor_screen_pos(cursor_pos);
    let radius = (size * 0.5 - 58.0).max(size * 0.30);
    let center = [cursor_pos[0] + size / 2.0, cursor_pos[1] + size / 2.0];

    // Reserve space in layout, then restore full-width cursor under the chart.
    ui.invisible_button("##radar_area", [size, size]);
    ui.set_cursor_screen_pos([origin[0], origin[1] + size]);

    let draw_list = crate::ui::window_draw_list(ui);

    // Background circle
    draw_list
        .add_circle(center, radius + 2.0, color_u32([0.15, 0.15, 0.15, 0.8]))
        .filled(true)
        .build();

    // Concentric hexagons (grid lines at 25%, 50%, 75%, 100%)
    for level in &[0.25_f32, 0.5, 0.75, 1.0] {
        let grid_color = color_u32([0.3, 0.3, 0.3, 0.4]);
        for i in 0..NUM_AXES {
            let p1 = axis_point(center, radius, i, *level);
            let p2 = axis_point(center, radius, (i + 1) % NUM_AXES, *level);
            draw_list.add_line(p1, p2, grid_color).build();
        }
    }

    // Axis lines from center
    for i in 0..NUM_AXES {
        let p = axis_point(center, radius, i, 1.0);
        draw_list
            .add_line(center, p, color_u32([0.4, 0.4, 0.4, 0.5]))
            .build();
    }

    // Current build performance overlay (amber)
    if let Some(perf) = current_build_perf {
        draw_filled_polygon(
            &draw_list,
            center,
            radius,
            perf,
            CURRENT_FILL,
            CURRENT_OUTLINE,
        );
    }

    // Optimized build performance overlay (green)
    if let Some(perf) = optimized_perf {
        draw_filled_polygon(
            &draw_list,
            center,
            radius,
            perf,
            OPTIMIZED_FILL,
            OPTIMIZED_OUTLINE,
        );
    }

    // Weights polygon (interactive, cyan)
    let w = weights.as_array();
    draw_filled_polygon(
        &draw_list,
        center,
        radius,
        &w,
        WEIGHTS_FILL,
        WEIGHTS_OUTLINE,
    );

    let mouse_pos = ui.io().mouse_pos;
    let mouse_down = ui.io().mouse_down[0];

    if mouse_down {
        if dragging.is_none() {
            for (i, &wi) in w.iter().enumerate() {
                let handle_pos = axis_point(center, radius, i, wi as f32);
                let dx = mouse_pos[0] - handle_pos[0];
                let dy = mouse_pos[1] - handle_pos[1];
                if dx * dx + dy * dy < 144.0 {
                    // 12px radius
                    *dragging = Some(i);
                    break;
                }
            }
        }

        if let Some(axis) = *dragging {
            // Project mouse position onto axis
            let dx = mouse_pos[0] - center[0];
            let dy = mouse_pos[1] - center[1];
            let angle = std::f32::consts::PI * 2.0 * (axis as f32) / NUM_AXES as f32
                - std::f32::consts::FRAC_PI_2;
            let axis_dx = angle.cos();
            let axis_dy = angle.sin();
            let proj = (dx * axis_dx + dy * axis_dy) / radius;
            let new_val = proj.clamp(0.0, 1.0) as f64;
            weights.set_constrained(axis, new_val);
            modified = true;
        }
    } else {
        *dragging = None;
    }

    for i in 0..NUM_AXES {
        let handle_pos = axis_point(center, radius, i, w[i] as f32);
        let is_dragged = *dragging == Some(i);
        let handle_radius = if is_dragged { 7.0 } else { 5.0 };
        draw_list
            .add_circle(handle_pos, handle_radius, color_u32(AXIS_COLORS[i]))
            .filled(true)
            .build();
        draw_list
            .add_circle(handle_pos, handle_radius, color_u32([1.0, 1.0, 1.0, 0.8]))
            .build();
    }

    // Axis labels + percentage
    for i in 0..NUM_AXES {
        let label_pos = axis_point(center, radius + 16.0, i, 1.0);
        let label = format!("{} {:.0}%", t(AXIS_I18N[i]), w[i] * 100.0);
        let text_size = ui.calc_text_size(&label);
        draw_list.add_text(
            [
                label_pos[0] - text_size[0] / 2.0,
                label_pos[1] - text_size[1] / 2.0,
            ],
            color_u32(AXIS_COLORS[i]),
            &label,
        );
    }

    // Budget indicator at bottom of chart
    let budget_used = weights.total();
    let budget_pct = (budget_used / gw2_optimizer::scoring::WEIGHT_BUDGET).min(1.0);
    let bar_width = size * 0.8;
    let bar_height = 4.0;
    let bar_x = cursor_pos[0] + (size - bar_width) / 2.0;
    let bar_y = cursor_pos[1] + size + 2.0;

    // Background
    draw_list
        .add_rect(
            [bar_x, bar_y],
            [bar_x + bar_width, bar_y + bar_height],
            color_u32([0.2, 0.2, 0.2, 0.6]),
        )
        .filled(true)
        .build();

    let bar_color = if budget_pct > 0.95 {
        [1.0, 0.4, 0.2, 0.9] // near max -- orange
    } else {
        [0.3, 0.8, 1.0, 0.7] // normal -- cyan
    };
    draw_list
        .add_rect(
            [bar_x, bar_y],
            [bar_x + bar_width * budget_pct as f32, bar_y + bar_height],
            color_u32(bar_color),
        )
        .filled(true)
        .build();

    ui.dummy([0.0, 14.0]);

    modified
}

fn draw_filled_polygon(
    draw_list: &nexus::imgui::DrawListMut,
    center: [f32; 2],
    radius: f32,
    values: &[f64; 6],
    fill_color: [f32; 4],
    outline_color: [f32; 4],
) {
    let fill = color_u32(fill_color);
    let outline = color_u32(outline_color);

    // Filled triangles (fan from center)
    for i in 0..NUM_AXES {
        let p1 = axis_point(center, radius, i, values[i] as f32);
        let p2 = axis_point(
            center,
            radius,
            (i + 1) % NUM_AXES,
            values[(i + 1) % NUM_AXES] as f32,
        );
        draw_list
            .add_triangle(center, p1, p2, fill)
            .filled(true)
            .build();
    }

    // Outline
    for i in 0..NUM_AXES {
        let p1 = axis_point(center, radius, i, values[i] as f32);
        let p2 = axis_point(
            center,
            radius,
            (i + 1) % NUM_AXES,
            values[(i + 1) % NUM_AXES] as f32,
        );
        draw_list.add_line(p1, p2, outline).thickness(2.0).build();
    }
}

pub fn render_legend(ui: &Ui, show_current: bool, show_optimized: bool) {
    if show_current {
        ui.text_colored(CURRENT_OUTLINE, t("legend.current"));
    }
    if show_optimized {
        ui.text_colored(OPTIMIZED_OUTLINE, t("legend.optimized"));
    }
    ui.text_colored(WEIGHTS_OUTLINE, t("legend.weights"));
}

/// Compute 6-axis performance values from CombatMetrics (i32 UI type).
pub fn compute_axes_from_metrics(m: &gw2_core::types::CombatMetrics) -> [f64; 6] {
    let power = (m.strike_dps_index as f64 / STRIKE_DPS_NORM).min(1.0);
    let condition = ((m.condition_dps_index as f64 / CONDI_DPS_NORM)
        + (m.condi_duration_pct / 100.0).min(1.0) * 0.15)
        .min(1.0);
    let boon_support = (m.boon_duration_pct / 100.0).min(1.0);
    let heal = (m.healing_index as f64 / HEALING_NORM).min(1.0);
    let sustain = ((m.effective_health as f64 / EFFECTIVE_HEALTH_NORM)
        + m.damage_reduction_pct / 100.0)
        .min(1.0);
    let control = (m.condi_duration_pct / 100.0 * 0.6 + m.boon_duration_pct / 100.0 * 0.4).min(1.0);

    [power, condition, boon_support, heal, sustain, control]
}
