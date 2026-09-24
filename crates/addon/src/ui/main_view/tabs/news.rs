//! News tab — Tyria Dispatch (compact, card, detail).

use nexus::imgui::{DrawListMut, Ui};

use crate::news;
use crate::state::AddonState;
use crate::ui::{news_feed, theme};
use gw2_core::config::{NewsKind, NewsLayout};
use gw2_core::i18n::{slavic_plural_form, t, tf, SlavicPluralForm};

pub(in crate::ui::main_view) fn render_news_tab(ui: &Ui, state: &mut AddonState) {
    let sources = state.config.news.enabled_sources();
    news::kick(state, &sources);

    masthead(ui, state);
    ui.dummy([0.0, 6.0]);
    tools(ui, state);
    ui.dummy([0.0, 8.0]);

    let show_images = state.config.news.show_images;
    let layout = state.config.news.layout;
    let scale = state.config.font_scale;
    let loading = state.news.loading;
    let filter = state.news.filter;
    let search = state.news.search.clone();
    let items: Vec<_> = state
        .news
        .collected(&sources)
        .into_iter()
        .filter(|i| news::matches(i, filter, &search))
        .collect();
    if show_images {
        let urls: Vec<String> = items.iter().filter_map(|i| i.image_url.clone()).collect();
        news::kick_art(state, &urls);
    }

    let empty = if state.news.collected(&sources).is_empty() {
        t("news.empty")
    } else {
        t("news.no_match")
    };
    let auto_select = layout != NewsLayout::Magazine;
    news_feed::scroll_area(ui, "##news_scroll", scale, || {
        news_feed::render_workspace(
            ui,
            news_feed::Workspace {
                items: &items,
                selected: &mut state.news.expanded,
                loading,
                empty: &empty,
                layout,
                show_images,
                auto_select,
                id: "desk",
                still_zoom: &mut state.news.still_zoom,
            },
        );
    });
}

/// Same CLDR one/few/many split as lock chrome (`lock_count_key`).
fn news_count_key(n: u64) -> &'static str {
    match slavic_plural_form(n) {
        SlavicPluralForm::One => "fmt.news_one",
        SlavicPluralForm::Few => "fmt.news_few",
        SlavicPluralForm::Many => "fmt.news_many",
    }
}

fn masthead(ui: &Ui, state: &AddonState) {
    let title = t("news.desk.title");
    let n = state
        .news
        .collected(&state.config.news.enabled_sources())
        .len();
    let count = tf(news_count_key(n as u64), &[("n", n.to_string().as_str())]);
    let start = ui.cursor_screen_pos();
    let width = ui.content_region_avail()[0];
    let h = 26.0;
    {
        let dl = ui.get_window_draw_list();
        dl.add_rect(
            [start[0] - 1.0, start[1]],
            [start[0] + width + 1.0, start[1] + h],
            theme::pal().header_plate,
        )
        .filled(true)
        .rounding(5.0)
        .build();
        theme::paint_header_accent(&dl, start[0], start[1], h);
        let th = ui.calc_text_size(&title)[1];
        let ty = start[1] + ((h - th) * 0.5).round();
        dl.add_text(
            [theme::header_title_x(start[0]), ty],
            crate::ui::color_u32(theme::pal().gold),
            &title,
        );
        let extra = if state.news.loading {
            format!("  ·  {}", t("news.loading"))
        } else {
            String::new()
        };
        let right = format!("{count}{extra}");
        let rw = ui.calc_text_size(&right)[0];
        dl.add_text(
            [start[0] + width - rw - 8.0, ty],
            crate::ui::color_u32(theme::pal().muted),
            &right,
        );
    }
    ui.dummy([0.0, h + 2.0]);
}

fn tools(ui: &Ui, state: &mut AddonState) {
    kind_filters(ui, state);
    ui.dummy([0.0, 4.0]);
    layout_and_find(ui, state);
}

fn hover_hint(ui: &Ui, key: &str) {
    if ui.is_item_hovered() {
        theme::wide_tooltip(ui, |ui| ui.text(t(key)));
    }
}

fn kind_filters(ui: &Ui, state: &mut AddonState) {
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("news.filter.show"));
    hover_hint(ui, "news.filter.show.hint");
    ui.same_line_with_spacing(0.0, 8.0);

    // Order: All -> Articles -> Notes -> Videos -> Guides. All keeps filter=None.
    let kinds: [(Option<NewsKind>, &str, &str); 5] = [
        (None, "news.kind.all", "news.kind.all.hint"),
        (
            Some(NewsKind::Articles),
            "news.kind.articles",
            "news.kind.articles.hint",
        ),
        (
            Some(NewsKind::Notes),
            "news.kind.notes",
            "news.kind.notes.hint",
        ),
        (
            Some(NewsKind::Video),
            "news.kind.video",
            "news.kind.video.hint",
        ),
        (
            Some(NewsKind::Guides),
            "news.kind.guides",
            "news.kind.guides.hint",
        ),
    ];
    let h = theme::control_height(ui);
    let avail = ui.content_region_avail()[0];
    let mut row_x = 0.0_f32;
    let mut pick = None;
    for (i, (kind, label_key, hint_key)) in kinds.iter().enumerate() {
        let label = t(label_key);
        let w = kind_filter_tab_width(ui, &label, h);
        theme::wrap_chip(ui, avail, &mut row_x, w, 4.0);
        let on = state.news.filter == *kind;
        if kind_filter_tab(ui, &format!("##news_kind_{i}"), &label, w, h, on, *kind) && !on {
            pick = Some(*kind);
        }
        hover_hint(ui, hint_key);
    }
    if let Some(kind) = pick {
        state.news.filter = kind;
    }
}

fn kind_filter_tab_width(ui: &Ui, label: &str, h: f32) -> f32 {
    let pad_x = 8.0;
    let glyph = (h * 0.55).max(12.0);
    let gap = 6.0;
    let tw = ui.calc_text_size(label)[0];
    (pad_x + glyph + gap + tw + pad_x).max(h)
}

/// Icon + visible label tab. Always shows both; narrow rows wrap via wrap_chip.
fn kind_filter_tab(
    ui: &Ui,
    id: &str,
    label: &str,
    w: f32,
    h: f32,
    on: bool,
    kind: Option<NewsKind>,
) -> bool {
    let p = ui.cursor_screen_pos();
    let hit = ui.invisible_button(id, [w, h]);
    let hovered = ui.is_item_hovered();
    let fill = if on {
        theme::pal().gold_fill
    } else if hovered {
        theme::pal().gold_hover
    } else {
        theme::pal().chip_idle_fill
    };
    let rim = if on {
        theme::pal().gold
    } else if hovered {
        theme::pal().gold_dim
    } else {
        theme::pal().chip_idle_rim
    };
    let ink = if on {
        theme::pal().gold_button_text
    } else {
        theme::pal().cream
    };
    let pad_x = 8.0;
    let glyph = (h * 0.55).max(12.0);
    let gap = 6.0;
    let dl = ui.get_window_draw_list();
    dl.add_rect(p, [p[0] + w, p[1] + h], fill)
        .filled(true)
        .rounding(h * 0.45)
        .build();
    dl.add_rect(p, [p[0] + w, p[1] + h], rim)
        .rounding(h * 0.45)
        .build();
    let gx = p[0] + pad_x + glyph * 0.5;
    let gy = p[1] + h * 0.5;
    draw_kind_glyph(&dl, kind, [gx, gy], glyph, ink);
    let tw = ui.calc_text_size(label);
    let tx = p[0] + pad_x + glyph + gap;
    let ty = p[1] + (h - tw[1]) * 0.5;
    dl.add_text([tx, ty], crate::ui::color_u32(ink), label);
    hit
}

fn draw_kind_glyph(
    dl: &DrawListMut,
    kind: Option<NewsKind>,
    c: [f32; 2],
    size: f32,
    color: [f32; 4],
) {
    let thick = (size / 10.0).max(1.2);
    match kind {
        None => {
            let tile = size * 0.28;
            let gap = size * 0.10;
            let step = tile + gap;
            for dx in [-0.5f32, 0.5] {
                for dy in [-0.5f32, 0.5] {
                    let x = c[0] + dx * step;
                    let y = c[1] + dy * step;
                    dl.add_rect(
                        [x - tile * 0.5, y - tile * 0.5],
                        [x + tile * 0.5, y + tile * 0.5],
                        color,
                    )
                    .filled(true)
                    .rounding(1.5)
                    .build();
                }
            }
        }
        Some(NewsKind::Articles) => {
            let w = size * 0.42;
            let h = size * 0.54;
            let x0 = c[0] - w * 0.5;
            let y0 = c[1] - h * 0.5;
            dl.add_rect([x0, y0], [x0 + w, y0 + h], color)
                .thickness(thick)
                .rounding(1.5)
                .build();
            let fold = size * 0.14;
            dl.add_line([x0 + w - fold, y0], [x0 + w - fold, y0 + fold], color)
                .thickness(thick)
                .build();
            dl.add_line([x0 + w - fold, y0 + fold], [x0 + w, y0 + fold], color)
                .thickness(thick)
                .build();
            for i in 0..3 {
                let y = y0 + h * 0.40 + i as f32 * size * 0.11;
                dl.add_line([x0 + size * 0.08, y], [x0 + w - size * 0.08, y], color)
                    .thickness(thick)
                    .build();
            }
        }
        Some(NewsKind::Notes) => {
            let y0 = c[1] - size * 0.22;
            for i in 0..3 {
                let y = y0 + i as f32 * size * 0.20;
                let inset = if i == 2 { size * 0.12 } else { 0.0 };
                dl.add_line(
                    [c[0] - size * 0.32, y],
                    [c[0] + size * 0.32 - inset, y],
                    color,
                )
                .thickness(thick * 1.15)
                .build();
            }
        }
        Some(NewsKind::Video) => {
            let w = size * 0.56;
            let h = size * 0.40;
            dl.add_rect(
                [c[0] - w * 0.5, c[1] - h * 0.5],
                [c[0] + w * 0.5, c[1] + h * 0.5],
                color,
            )
            .thickness(thick)
            .rounding(size * 0.08)
            .build();
            let s = size * 0.14;
            dl.add_triangle(
                [c[0] - s * 0.35, c[1] - s],
                [c[0] - s * 0.35, c[1] + s],
                [c[0] + s * 0.85, c[1]],
                color,
            )
            .filled(true)
            .build();
        }
        Some(NewsKind::Guides) => {
            let h = size * 0.44;
            let w = size * 0.30;
            let y0 = c[1] - h * 0.45;
            let y1 = c[1] + h * 0.50;
            dl.add_line([c[0], y0], [c[0], y1], color)
                .thickness(thick)
                .build();
            dl.add_line([c[0], y0], [c[0] - w, y0 + size * 0.06], color)
                .thickness(thick)
                .build();
            dl.add_line([c[0] - w, y0 + size * 0.06], [c[0] - w, y1], color)
                .thickness(thick)
                .build();
            dl.add_line([c[0] - w, y1], [c[0], y1 - size * 0.04], color)
                .thickness(thick)
                .build();
            dl.add_line([c[0], y0], [c[0] + w, y0 + size * 0.06], color)
                .thickness(thick)
                .build();
            dl.add_line([c[0] + w, y0 + size * 0.06], [c[0] + w, y1], color)
                .thickness(thick)
                .build();
            dl.add_line([c[0] + w, y1], [c[0], y1 - size * 0.04], color)
                .thickness(thick)
                .build();
        }
    }
}

fn layout_and_find(ui: &Ui, state: &mut AddonState) {
    ui.align_text_to_frame_padding();
    ui.text_colored(theme::pal().muted, t("news.layout.label"));
    hover_hint(ui, "news.layout.hint");

    let layouts = [
        (
            NewsLayout::Desk,
            "news.layout.desk",
            "news.layout.desk.hint",
        ),
        (
            NewsLayout::Magazine,
            "news.layout.magazine",
            "news.layout.magazine.hint",
        ),
        (
            NewsLayout::Reader,
            "news.layout.reader",
            "news.layout.reader.hint",
        ),
    ];
    let mut next_layout = None;
    for (i, (layout, label_key, hint_key)) in layouts.iter().enumerate() {
        ui.same_line_with_spacing(0.0, if i == 0 { 8.0 } else { 10.0 });
        let on = state.config.news.layout == *layout;
        let label = format!("{}##news_lay_{i}", t(label_key));
        if ui.radio_button_bool(&label, on) && !on {
            next_layout = Some(*layout);
        }
        hover_hint(ui, hint_key);
    }
    if let Some(layout) = next_layout {
        state.config.news.layout = layout;
        if layout == NewsLayout::Magazine {
            state.news.expanded = None;
        }
        crate::ui::save_config_detached(state);
    }

    let search_w = (ui.content_region_avail()[0] * 0.42).clamp(140.0, 260.0);
    let stills = t("news.images");
    let refresh = t("btn.refresh");
    let extra = search_w
        + 16.0
        + ui.calc_text_size(&stills)[0]
        + 28.0
        + theme::gold_button_width(ui, &refresh);
    if ui.content_region_avail()[0] > extra {
        ui.same_line_with_spacing(0.0, 16.0);
    } else {
        ui.dummy([0.0, 4.0]);
        ui.align_text_to_frame_padding();
    }
    ui.set_next_item_width(search_w);
    ui.input_text("##news_q", &mut state.news.search)
        .hint(&t("news.search"))
        .build();
    ui.same_line_with_spacing(0.0, 10.0);
    let mut images = state.config.news.show_images;
    if ui.checkbox(format!("{}##news_stills", stills), &mut images) {
        state.config.news.show_images = images;
        crate::ui::save_config_detached(state);
    }
    ui.same_line_with_spacing(0.0, 10.0);
    if theme::gold_button_sized(ui, refresh, [0.0, 0.0]) {
        let sources = state.config.news.enabled_sources();
        state.news.invalidate(&sources);
        news::kick(state, &sources);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_count_key_uses_slavic_plural_form() {
        gw2_core::i18n::set_language("en");
        assert_eq!(news_count_key(1), "fmt.news_one");
        for n in [0, 2, 21, 22, 101] {
            assert_eq!(news_count_key(n), "fmt.news_many", "en n={n}");
        }

        gw2_core::i18n::set_language("pl");
        assert_eq!(news_count_key(1), "fmt.news_one");
        assert_eq!(news_count_key(21), "fmt.news_many");
        assert_eq!(news_count_key(2), "fmt.news_few");

        gw2_core::i18n::set_language("ru");
        assert_eq!(news_count_key(21), "fmt.news_one");
        assert_eq!(news_count_key(2), "fmt.news_few");

        gw2_core::i18n::set_language("fr");
        assert_eq!(news_count_key(0), "fmt.news_one");
        assert_eq!(news_count_key(2), "fmt.news_many");

        gw2_core::i18n::set_language("en");
    }
}
