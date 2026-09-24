//! Tyria Dispatch — list, magazine cells, and a full-text reader.

use nexus::imgui::{ChildWindow, TextureId, Ui};

use crate::clipboard;
use crate::feedback::shell::open_url;
use crate::news::NewsItem;
use crate::ui::theme;
use gw2_core::config::{NewsLayout, NewsSource};
use gw2_core::i18n::t;

const PAD: f32 = 10.0;
const GAP: f32 = 6.0;
const ROUND: f32 = 6.0;
const STILL_ASPECT: f32 = 16.0 / 9.0;
const INDEX_THUMB_H: f32 = 48.0;

pub struct Workspace<'a> {
    pub items: &'a [NewsItem],
    pub selected: &'a mut Option<String>,
    pub loading: bool,
    pub empty: &'a str,
    pub layout: NewsLayout,
    pub show_images: bool,
    pub auto_select: bool,
    pub id: &'a str,
    pub still_zoom: &'a mut f32,
}

pub fn render_workspace(ui: &Ui, mut view: Workspace<'_>) {
    if view.items.is_empty() {
        ui.dummy([0.0, 6.0]);
        ui.text_colored(
            theme::pal().muted,
            if view.loading {
                t("news.loading")
            } else {
                view.empty.to_string()
            },
        );
        return;
    }

    ensure_selection(view.items, view.selected, view.auto_select);

    match view.layout {
        NewsLayout::Desk => desk(ui, &mut view),
        NewsLayout::Magazine => {
            if view.selected.is_some() {
                reader_pane(ui, &mut view, true);
            } else {
                magazine(ui, &mut view);
            }
        }
        NewsLayout::Reader => reader_pane(ui, &mut view, false),
    }
}

pub fn scroll_area(ui: &Ui, id: &str, scale: f32, body: impl FnOnce()) {
    let h = (ui.content_region_avail()[1] - 4.0).max(80.0);
    ChildWindow::new(id).size([0.0, h]).build(ui, || {
        ui.set_window_font_scale(scale);
        body();
    });
}

fn ensure_selection(items: &[NewsItem], selected: &mut Option<String>, auto: bool) {
    if let Some(url) = selected.as_deref() {
        if items.iter().any(|i| i.url == url) {
            return;
        }
    }
    *selected = if auto {
        items.first().map(|i| i.url.clone())
    } else {
        None
    };
}

fn desk(ui: &Ui, view: &mut Workspace<'_>) {
    let avail = ui.content_region_avail();
    let h = avail[1].max(80.0);
    let gap = 10.0;
    let index_w = (avail[0] * 0.58).clamp(400.0, 720.0);
    let stack = avail[0] < index_w + gap + 280.0;
    if stack {
        let index_h = (h * 0.42).max(140.0);
        let idx_id = format!("##news_idx_{}", view.id);
        let read_id = format!("##news_read_{}", view.id);
        ChildWindow::new(idx_id.as_str())
            .size([0.0, index_h])
            .build(ui, || index_list(ui, view));
        ui.dummy([0.0, 6.0]);
        ChildWindow::new(read_id.as_str())
            .size([0.0, 0.0])
            .build(ui, || reader_body(ui, view, false));
        return;
    }
    let idx_id = format!("##news_idx_{}", view.id);
    let read_id = format!("##news_read_{}", view.id);
    ChildWindow::new(idx_id.as_str())
        .size([index_w, h])
        .build(ui, || index_list(ui, view));
    ui.same_line_with_spacing(0.0, gap);
    ChildWindow::new(read_id.as_str())
        .size([0.0, h])
        .build(ui, || reader_body(ui, view, false));
}

fn magazine(ui: &Ui, view: &mut Workspace<'_>) {
    let avail = ui.content_region_avail()[0];
    let col_w = ((avail - GAP) * 0.5).max(160.0);
    let cols_id = format!("##news_mag_{}", view.id);
    ui.columns(2, &cols_id, false);
    ui.set_column_width(0, col_w);
    for i in (0..view.items.len()).step_by(2) {
        mag_cell(ui, view, i);
        ui.dummy([0.0, GAP]);
    }
    ui.next_column();
    for i in (1..view.items.len()).step_by(2) {
        mag_cell(ui, view, i);
        ui.dummy([0.0, GAP]);
    }
    ui.columns(1, format!("##news_mag_end_{}", view.id), false);
}

fn mag_cell(ui: &Ui, view: &mut Workspace<'_>, index: usize) {
    let item = &view.items[index];
    let w = ui.content_region_avail()[0].max(80.0);
    let inner = (w - PAD * 2.0).max(40.0);
    let lh = ui.text_line_height();
    let title = clip_label(ui, &item.title, inner);
    let kicker = clip_label(ui, &kicker_line(item), inner);
    let img_h = if view.show_images && item.image_url.is_some() {
        fit_box(inner, 120.0, still_aspect(item.image_url.as_deref()))[1]
    } else {
        0.0
    };
    let url = item.url.clone();
    let image = item.image_url.clone();
    let h = PAD + img_h + lh + 2.0 + lh + PAD;
    let p = ui.cursor_screen_pos();
    let id = format!("##news_mag_{}_{index}", view.id);
    let hit = ui.invisible_button(&id, [w, h]);
    let hovered = ui.is_item_hovered();
    plate(ui, p, [w, h], hovered, false);
    let mut cy = p[1] + PAD;
    if img_h > 0.0 {
        paint_still(ui, image.as_deref(), [p[0] + PAD, cy], [inner, img_h]);
        cy += img_h;
    }
    ui.set_cursor_screen_pos([p[0] + PAD, cy]);
    ui.text_colored(theme::pal().gold, kicker);
    ui.set_cursor_screen_pos([p[0] + PAD, cy + lh + 2.0]);
    ui.text_colored(theme::pal().cream, title);
    ui.set_cursor_screen_pos([p[0], p[1] + h]);
    if hit {
        *view.selected = Some(url);
    }
}

fn index_list(ui: &Ui, view: &mut Workspace<'_>) {
    let mut clicked: Option<String> = None;
    for i in 0..view.items.len() {
        let on = view.selected.as_deref() == Some(view.items[i].url.as_str());
        if index_row(ui, view, i, on) {
            clicked = Some(view.items[i].url.clone());
        }
        ui.dummy([0.0, 3.0]);
    }
    if let Some(url) = clicked {
        *view.selected = Some(url);
    }
}

fn index_row(ui: &Ui, view: &Workspace<'_>, index: usize, on: bool) -> bool {
    let item = &view.items[index];
    let w = ui.content_region_avail()[0].max(40.0);
    let lh = ui.text_line_height();
    let thumb_w = INDEX_THUMB_H * STILL_ASPECT;
    let thumb = view.show_images && item.image_url.is_some();
    let h = if thumb {
        INDEX_THUMB_H + 8.0
    } else {
        PAD + lh * 2.0 + 6.0
    };
    let p = ui.cursor_screen_pos();
    let id = format!("##news_idx_{}_{index}", view.id);
    let hit = ui.invisible_button(&id, [w, h]);
    let hovered = ui.is_item_hovered();
    plate(ui, p, [w, h], hovered, on);
    if on {
        let dl = crate::ui::window_draw_list(ui);
        theme::paint_header_accent(&dl, p[0], p[1], h);
    }
    let text_x = if thumb {
        p[0] + 8.0 + thumb_w + 8.0
    } else {
        p[0] + PAD + if on { 4.0 } else { 0.0 }
    };
    if thumb {
        paint_still(
            ui,
            item.image_url.as_deref(),
            [p[0] + 8.0, p[1] + 4.0],
            [thumb_w, INDEX_THUMB_H],
        );
    }
    let inner = (p[0] + w - 8.0 - text_x).max(20.0);
    ui.set_cursor_screen_pos([text_x, p[1] + 6.0]);
    ui.text_colored(
        if on {
            theme::pal().gold
        } else {
            theme::pal().cream
        },
        clip_label(ui, &item.title, inner),
    );
    ui.set_cursor_screen_pos([text_x, p[1] + 6.0 + lh + 2.0]);
    ui.text_colored(
        theme::pal().muted,
        clip_label(ui, &kicker_line(item), inner),
    );
    ui.set_cursor_screen_pos([p[0], p[1] + h]);
    hit
}

fn reader_pane(ui: &Ui, view: &mut Workspace<'_>, back: bool) {
    let h = ui.content_region_avail()[1].max(80.0);
    let read_id = format!("##news_read_{}", view.id);
    ChildWindow::new(read_id.as_str())
        .size([0.0, h])
        .build(ui, || reader_body(ui, view, back));
}

fn reader_body(ui: &Ui, view: &mut Workspace<'_>, back: bool) {
    let Some(item) = view
        .items
        .iter()
        .find(|i| Some(i.url.as_str()) == view.selected.as_deref())
        .cloned()
    else {
        ui.text_colored(theme::pal().muted, t("news.no_match"));
        return;
    };

    let idx = view
        .items
        .iter()
        .position(|i| i.url == item.url)
        .unwrap_or(0);

    if back && ui.button(format!("{}##news_back_{}", t("news.back"), view.id)) {
        *view.selected = None;
        return;
    }

    ui.text_colored(theme::pal().gold, kicker_line(&item));
    ui.dummy([0.0, 4.0]);
    theme::wrapped(ui, theme::pal().cream, &item.title);

    if view.show_images {
        if let Some(url) = item.image_url.as_deref() {
            ui.dummy([0.0, 8.0]);
            let detail = view.layout != NewsLayout::Desk;
            if *view.still_zoom < 1.0 {
                *view.still_zoom = 3.0;
            }
            let zoom = if detail {
                view.still_zoom.clamp(1.0, 5.0)
            } else {
                1.0
            };
            let max_w = ui.content_region_avail()[0].max(8.0);
            let aspect = still_aspect(Some(url));
            let fitted = fit_box(max_w, 240.0 * zoom, aspect);
            let p = ui.cursor_screen_pos();
            ui.dummy([max_w, fitted[1]]);
            paint_still(ui, Some(url), p, [max_w, fitted[1]]);
            if detail {
                ui.dummy([0.0, 4.0]);
                ui.align_text_to_frame_padding();
                ui.text_colored(theme::pal().muted, t("news.zoom"));
                ui.same_line_with_spacing(0.0, 8.0);
                ui.set_next_item_width(140.0);
                let _ = nexus::imgui::Slider::new("##news_zoom", 1.0, 5.0)
                    .display_format("%.1fx")
                    .build(ui, view.still_zoom);
                ui.same_line_with_spacing(0.0, 8.0);
                if ui.small_button(format!("{}##news_zoom_reset", t("news.zoom.reset"))) {
                    *view.still_zoom = 3.0;
                }
            }
        }
    }

    if item.source == NewsSource::Youtube {
        ui.dummy([0.0, 6.0]);
        theme::wrapped(ui, theme::pal().muted, &t("news.video_note"));
    }

    ui.dummy([0.0, 8.0]);
    if !item.body.is_empty() {
        theme::prose(ui, &item.body);
    }

    ui.dummy([0.0, 12.0]);
    let mut pick: Option<String> = None;
    if idx > 0 && ui.button(format!("{}##news_prev_{}", t("news.prev"), view.id)) {
        pick = Some(view.items[idx - 1].url.clone());
    }
    if idx > 0 {
        ui.same_line_with_spacing(0.0, 8.0);
    }
    if idx + 1 < view.items.len() && ui.button(format!("{}##news_next_{}", t("news.next"), view.id))
    {
        pick = Some(view.items[idx + 1].url.clone());
    }
    ui.same_line_with_spacing(0.0, 12.0);
    if theme::gold_button_sized(
        ui,
        format!("{}##news_open_{}", t("news.open"), view.id),
        [0.0, 0.0],
    ) {
        let _ = open_url(&item.url);
    }
    ui.same_line_with_spacing(0.0, 8.0);
    if ui.button(format!("{}##news_copy_{}", t("news.copy"), view.id)) {
        let _ = clipboard::copy_text(&item.url);
    }
    if let Some(url) = pick {
        *view.selected = Some(url);
    }
}

fn kicker_line(item: &NewsItem) -> String {
    let src = t(item.source.label_key());
    if item.published.is_empty() {
        src
    } else {
        format!("{src}  ·  {}", item.published)
    }
}

fn plate(ui: &Ui, p: [f32; 2], size: [f32; 2], hovered: bool, on: bool) {
    let fill = if on {
        theme::with_alpha(theme::pal().header_plate, 0.92)
    } else if hovered {
        theme::pal().gold_hover
    } else {
        theme::with_alpha(theme::pal().plate, 0.72)
    };
    let rim = if on {
        theme::pal().gold
    } else if hovered {
        theme::pal().gold_dim
    } else {
        theme::with_alpha(theme::pal().chip_idle_rim, 0.45)
    };
    let dl = crate::ui::window_draw_list(ui);
    dl.add_rect(p, [p[0] + size[0], p[1] + size[1]], fill)
        .filled(true)
        .rounding(ROUND)
        .build();
    dl.add_rect(p, [p[0] + size[0], p[1] + size[1]], rim)
        .rounding(ROUND)
        .build();
}

fn still_aspect(url: Option<&str>) -> f32 {
    url.map(crate::news_art::aspect).unwrap_or(STILL_ASPECT)
}

fn fit_box(max_w: f32, max_h: f32, aspect: f32) -> [f32; 2] {
    let aspect = if aspect > 0.05 { aspect } else { STILL_ASPECT };
    let mut w = max_w.max(1.0);
    let mut h = w / aspect;
    if h > max_h {
        h = max_h.max(1.0);
        w = h * aspect;
    }
    [w, h]
}

fn paint_still(ui: &Ui, url: Option<&str>, origin: [f32; 2], max_size: [f32; 2]) {
    let aspect = still_aspect(url);
    let fitted = fit_box(max_size[0], max_size[1], aspect);
    let ox = origin[0] + (max_size[0] - fitted[0]) * 0.5;
    let oy = origin[1] + (max_size[1] - fitted[1]) * 0.5;
    let dl = crate::ui::window_draw_list(ui);
    let plate = [origin[0] + max_size[0], origin[1] + max_size[1]];
    dl.add_rect(
        origin,
        plate,
        theme::with_alpha(theme::pal().chip_idle_fill, 0.9),
    )
    .filled(true)
    .rounding(theme::ICON_ROUNDING)
    .build();
    if let Some(tid) = url.and_then(crate::news_art::texture) {
        image_rounded(&dl, tid, [ox, oy], [ox + fitted[0], oy + fitted[1]]);
    }
}

fn image_rounded(
    dl: &nexus::imgui::DrawListMut<'_>,
    tid: TextureId,
    p_min: [f32; 2],
    p_max: [f32; 2],
) {
    dl.add_image_rounded(tid, p_min, p_max, theme::ICON_ROUNDING)
        .col([1.0, 1.0, 1.0, 1.0])
        .build();
}

fn clip_label(ui: &Ui, text: &str, max_w: f32) -> String {
    if ui.calc_text_size(text)[0] <= max_w {
        return text.to_string();
    }
    let mut s = String::new();
    for ch in text.chars() {
        let mut probe = s.clone();
        probe.push(ch);
        probe.push_str("...");
        if ui.calc_text_size(&probe)[0] > max_w {
            break;
        }
        s.push(ch);
    }
    s.push_str("...");
    s
}
