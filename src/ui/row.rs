use std::ops::Range;

use gpui::{
    FontWeight, HighlightStyle, IntoElement, ObjectFit, Styled, StyledImage, StyledText, div, img,
    prelude::*, px, rgb, rgba,
};

use crate::favicon::CachedFavicon;
use crate::store::ScoredTab;
use crate::ui::{
    CARD_FOOTER_HEIGHT, CARD_SHOT_HEIGHT, CARD_WIDTH, COMPACT_ROW_HEIGHT, GRID_COLUMNS, ROW_HEIGHT,
};

pub const DAYLIGHT_ROW_SELECTED_BG: u32 = 0xEFF6FF;
pub const DAYLIGHT_ROW_SELECTED_BORDER: u32 = 0x2563EB;
pub const DAYLIGHT_TITLE_TEXT: u32 = 0x0F172A;
pub const DAYLIGHT_TITLE_SELECTED_TEXT: u32 = 0x1D4ED8;
pub const DAYLIGHT_SUBTITLE_TEXT: u32 = 0x64748B;
pub const DAYLIGHT_SUBTITLE_SELECTED_TEXT: u32 = 0x3B82F6;
pub const DAYLIGHT_HIGHLIGHT_TEXT: u32 = 0x2563EB;

/// Compute continuous scrollbar `(thumb_top_px, thumb_height_px)` given the viewport height,
/// total scrollable content height, current vertical scroll offset (`<= 0.0`), and track height.
/// Returns `None` when `content_height <= viewport_height` (no scrollbar needed).
pub fn compute_scrollbar_metrics(
    viewport_height: f32,
    content_height: f32,
    scroll_offset_y: f32,
    track_height: f32,
) -> Option<(f32, f32)> {
    if content_height <= viewport_height + 1.0 || viewport_height <= 0.0 || track_height <= 0.0 {
        return None;
    }
    let visible_ratio = (viewport_height / content_height).clamp(0.10, 1.0);
    let thumb_height = (track_height * visible_ratio).max(28.0).min(track_height);
    let max_scroll = (content_height - viewport_height).max(1.0);
    let scrolled = (-scroll_offset_y).clamp(0.0, max_scroll);
    let progress = scrolled / max_scroll;
    let thumb_top = progress * (track_height - thumb_height);
    Some((thumb_top, thumb_height))
}

pub fn visible_window_range(
    total_items: usize,
    selected_index: usize,
    max_visible: usize,
) -> Range<usize> {
    if total_items == 0 || max_visible == 0 {
        return 0..0;
    }
    if total_items <= max_visible {
        return 0..total_items;
    }
    let cols = GRID_COLUMNS.max(1);
    let max_rows = (max_visible / cols).max(1);
    let clamped_selected = selected_index.min(total_items - 1);
    let selected_row = clamped_selected / cols;

    let start_row = if selected_row < max_rows {
        0
    } else {
        selected_row + 1 - max_rows
    };
    let start = (start_row * cols).min(total_items.saturating_sub(max_visible));
    let end = (start + max_visible).min(total_items);
    start..end
}

fn render_favicon_glyph(favicon: Option<CachedFavicon>, size_px: f32) -> gpui::AnyElement {
    if let Some(cached_icon) = favicon {
        img(cached_icon.to_image_source())
            .w(px(size_px))
            .h(px(size_px))
            .rounded_xs()
            .with_fallback(move || {
                div()
                    .w(px(size_px))
                    .h(px(size_px))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(size_px * 0.75))
                    .text_color(rgb(DAYLIGHT_SUBTITLE_TEXT))
                    .child("🌐")
                    .into_any_element()
            })
            .into_any_element()
    } else {
        div()
            .w(px(size_px))
            .h(px(size_px))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(size_px * 0.75))
            .text_color(rgb(DAYLIGHT_SUBTITLE_TEXT))
            .child("🌐")
            .into_any_element()
    }
}

/// Render a large visual card (`266×204`) for the 3-column grid view.
pub fn render_tab_row(
    item: &ScoredTab,
    is_selected: bool,
    favicon: Option<CachedFavicon>,
    preview: Option<CachedFavicon>,
) -> impl IntoElement {
    let title_text = if item.entry.title.is_empty() {
        item.entry.display_url.clone()
    } else {
        item.entry.title.clone()
    };

    let domain_host = item
        .entry
        .display_url
        .split('/')
        .next()
        .unwrap_or(&item.entry.display_url)
        .to_string();

    let title_highlight_style = HighlightStyle {
        color: Some(rgb(DAYLIGHT_HIGHLIGHT_TEXT).into()),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let title_highlights: Vec<(Range<usize>, HighlightStyle)> = if item.entry.title.is_empty() {
        Vec::new()
    } else {
        item.title_highlight_ranges
            .iter()
            .cloned()
            .map(|r| (r, title_highlight_style))
            .collect()
    };

    let url_highlight_style = HighlightStyle {
        color: Some(rgb(DAYLIGHT_TITLE_SELECTED_TEXT).into()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let url_highlights: Vec<(Range<usize>, HighlightStyle)> = item
        .url_highlight_ranges
        .iter()
        .cloned()
        .map(|r| (r, url_highlight_style))
        .collect();

    let shot_height = CARD_SHOT_HEIGHT as f32;
    let footer_height = CARD_FOOTER_HEIGHT as f32;

    let screenshot_area = if let Some(cached_preview) = preview {
        div()
            .relative()
            .w_full()
            .h(px(shot_height))
            .flex_none()
            .overflow_hidden()
            .bg(rgb(0x0F172A))
            .border_b_1()
            .border_color(if is_selected {
                rgb(0xBFDBFE)
            } else {
                rgb(0xE2E8F0)
            })
            .child(
                img(cached_preview.to_image_source())
                    .w(px(CARD_WIDTH as f32))
                    .h(px(shot_height))
                    .object_fit(ObjectFit::Cover),
            )
            .when(item.entry.audible, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(8.0))
                        .right(px(8.0))
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(rgba(0x0F172ACC))
                        .text_size(px(10.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(0xFFFFFF))
                        .child("🔊 Audio"),
                )
            })
    } else {
        let hero_fav = render_favicon_glyph(favicon.clone(), 28.0);
        div()
            .w_full()
            .h(px(shot_height))
            .flex_none()
            .overflow_hidden()
            .bg(rgb(0xF8FAFC))
            .border_b_1()
            .border_color(if is_selected {
                rgb(0xBFDBFE)
            } else {
                rgb(0xE2E8F0)
            })
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .h(px(22.0))
                    .flex_none()
                    .px_2p5()
                    .bg(rgb(0xF1F5F9))
                    .border_b_1()
                    .border_color(rgb(0xE2E8F0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(0xFCA5A5)))
                            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(0xFCD34D)))
                            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(0x86EFAC))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(14.0))
                            .px_2()
                            .rounded(px(4.0))
                            .bg(rgb(0xFFFFFF))
                            .flex()
                            .items_center()
                            .overflow_x_hidden()
                            .text_size(px(9.5))
                            .text_color(rgb(0x64748B))
                            .truncate()
                            .child(domain_host.clone()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .px_4()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(48.0))
                            .h(px(48.0))
                            .rounded(px(12.0))
                            .bg(rgb(0xFFFFFF))
                            .border_1()
                            .border_color(rgb(0xE2E8F0))
                            .shadow_sm()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(hero_fav),
                    )
                    .child(
                        div()
                            .w_full()
                            .overflow_x_hidden()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(0x475569))
                            .truncate()
                            .child(title_text.clone()),
                    ),
            )
    };

    let footer_fav = render_favicon_glyph(favicon, 14.0);

    div()
        .w(px(CARD_WIDTH as f32))
        .h(px(ROW_HEIGHT as f32))
        .flex_none()
        .rounded(px(12.0))
        .overflow_hidden()
        .bg(if is_selected {
            rgb(DAYLIGHT_ROW_SELECTED_BG)
        } else {
            rgb(0xFFFFFF)
        })
        .border_2()
        .border_color(if is_selected {
            rgb(DAYLIGHT_ROW_SELECTED_BORDER)
        } else {
            rgb(0xE2E8F0)
        })
        .shadow_sm()
        .flex()
        .flex_col()
        .child(screenshot_area)
        .child(
            div()
                .w_full()
                .h(px(footer_height))
                .flex_none()
                .px_2p5()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .overflow_x_hidden()
                .child(
                    div()
                        .w(px(24.0))
                        .h(px(24.0))
                        .flex_none()
                        .rounded(px(6.0))
                        .bg(rgb(0xFFFFFF))
                        .border_1()
                        .border_color(if is_selected {
                            rgb(0xBFDBFE)
                        } else {
                            rgb(0xE2E8F0)
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(footer_fav),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .justify_center()
                        .overflow_x_hidden()
                        .child(
                            div()
                                .w_full()
                                .overflow_x_hidden()
                                .text_size(px(12.5))
                                .font_weight(if is_selected {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_color(if is_selected {
                                    rgb(DAYLIGHT_TITLE_SELECTED_TEXT)
                                } else {
                                    rgb(DAYLIGHT_TITLE_TEXT)
                                })
                                .truncate()
                                .child(
                                    StyledText::new(title_text).with_highlights(title_highlights),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .overflow_x_hidden()
                                .text_size(px(10.5))
                                .text_color(if is_selected {
                                    rgb(DAYLIGHT_SUBTITLE_SELECTED_TEXT)
                                } else {
                                    rgb(DAYLIGHT_SUBTITLE_TEXT)
                                })
                                .truncate()
                                .child(
                                    StyledText::new(item.entry.display_url.clone())
                                        .with_highlights(url_highlights),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .when(item.entry.pinned, |this| {
                            this.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(DAYLIGHT_SUBTITLE_TEXT))
                                    .child("📌"),
                            )
                        })
                        .when(!item.badge_label.is_empty(), |this| {
                            this.child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(4.0))
                                    .bg(if is_selected {
                                        rgb(0xDBEAFE)
                                    } else {
                                        rgb(0xF1F5F9)
                                    })
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if is_selected {
                                        rgb(0x1E40AF)
                                    } else {
                                        rgb(0x475569)
                                    })
                                    .child(item.badge_label.clone()),
                            )
                        })
                        .when(is_selected, |this| {
                            this.child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(DAYLIGHT_SUBTITLE_SELECTED_TEXT))
                                    .child("↵"),
                            )
                        }),
                ),
        )
}

/// Render a high-density single-line / two-column compact row (`42px` tall) used automatically
/// when the user types a search query (`Auto-Compact on Search`).
pub fn render_compact_tab_row(
    item: &ScoredTab,
    is_selected: bool,
    favicon: Option<CachedFavicon>,
) -> impl IntoElement {
    let title_text = if item.entry.title.is_empty() {
        item.entry.display_url.clone()
    } else {
        item.entry.title.clone()
    };

    let title_highlight_style = HighlightStyle {
        color: Some(rgb(DAYLIGHT_HIGHLIGHT_TEXT).into()),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let title_highlights: Vec<(Range<usize>, HighlightStyle)> = if item.entry.title.is_empty() {
        Vec::new()
    } else {
        item.title_highlight_ranges
            .iter()
            .cloned()
            .map(|r| (r, title_highlight_style))
            .collect()
    };

    let url_highlight_style = HighlightStyle {
        color: Some(rgb(DAYLIGHT_TITLE_SELECTED_TEXT).into()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let url_highlights: Vec<(Range<usize>, HighlightStyle)> = item
        .url_highlight_ranges
        .iter()
        .cloned()
        .map(|r| (r, url_highlight_style))
        .collect();

    let fav = render_favicon_glyph(favicon, 15.0);

    div()
        .w_full()
        .h(px(COMPACT_ROW_HEIGHT as f32))
        .flex_none()
        .flex()
        .items_center()
        .child(
            div()
                .w_full()
                .h(px((COMPACT_ROW_HEIGHT - 4.0) as f32))
                .px_3()
                .rounded(px(9.0))
                .bg(if is_selected {
                    rgb(DAYLIGHT_ROW_SELECTED_BG)
                } else {
                    rgb(0xFFFFFF)
                })
                .border_1()
                .border_color(if is_selected {
                    rgb(DAYLIGHT_ROW_SELECTED_BORDER)
                } else {
                    rgb(0xE2E8F0)
                })
                .flex()
                .flex_row()
                .items_center()
                .gap_2p5()
                .overflow_x_hidden()
                .child(
                    div()
                        .w(px(24.0))
                        .h(px(24.0))
                        .flex_none()
                        .rounded(px(6.0))
                        .bg(rgb(0xF8FAFC))
                        .border_1()
                        .border_color(if is_selected {
                            rgb(0xBFDBFE)
                        } else {
                            rgb(0xE2E8F0)
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(fav),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_x_hidden()
                        .text_size(px(13.0))
                        .font_weight(if is_selected {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::MEDIUM
                        })
                        .text_color(if is_selected {
                            rgb(DAYLIGHT_TITLE_SELECTED_TEXT)
                        } else {
                            rgb(DAYLIGHT_TITLE_TEXT)
                        })
                        .truncate()
                        .child(StyledText::new(title_text).with_highlights(title_highlights)),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(260.0))
                        .px_2()
                        .py_0p5()
                        .rounded(px(5.0))
                        .bg(if is_selected {
                            rgb(0xDBEAFE)
                        } else {
                            rgb(0xF1F5F9)
                        })
                        .overflow_x_hidden()
                        .text_size(px(11.0))
                        .text_color(if is_selected {
                            rgb(0x1E40AF)
                        } else {
                            rgb(DAYLIGHT_SUBTITLE_TEXT)
                        })
                        .truncate()
                        .child(
                            StyledText::new(item.entry.display_url.clone())
                                .with_highlights(url_highlights),
                        ),
                )
                .when(item.entry.audible, |this| {
                    this.child(div().text_size(px(11.5)).child("🔊"))
                })
                .when(item.entry.pinned, |this| {
                    this.child(div().text_size(px(11.5)).child("📌"))
                })
                .when(!item.badge_label.is_empty(), |this| {
                    this.child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded(px(4.0))
                            .bg(rgb(0xE2E8F0))
                            .text_size(px(10.0))
                            .text_color(rgb(0x475569))
                            .child(item.badge_label.clone()),
                    )
                })
                .when(is_selected, |this| {
                    this.child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(DAYLIGHT_SUBTITLE_SELECTED_TEXT))
                            .child("↵"),
                    )
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_range_scrolls_by_grid_row_to_keep_selected_card_in_view() {
        assert_eq!(visible_window_range(5, 2, 6), 0..5);
        assert_eq!(visible_window_range(12, 0, 6), 0..6);
        assert_eq!(visible_window_range(12, 5, 6), 0..6);
        assert_eq!(visible_window_range(12, 6, 6), 3..9);
        assert_eq!(visible_window_range(12, 8, 6), 3..9);
        assert_eq!(visible_window_range(12, 9, 6), 6..12);
    }

    #[test]
    fn compute_scrollbar_metrics_scales_continuously_with_scroll_offset() {
        // Content fits in viewport -> None
        assert_eq!(compute_scrollbar_metrics(400.0, 380.0, 0.0, 380.0), None);

        // Content is 2x viewport (800px in 400px viewport), scrolled to top (0.0)
        let (top0, h0) = compute_scrollbar_metrics(400.0, 800.0, 0.0, 400.0).unwrap();
        assert_eq!(top0, 0.0);
        assert_eq!(h0, 200.0);

        // Scrolled halfway (-200.0px) -> thumb_top is halfway (100.0px)
        let (top_mid, _) = compute_scrollbar_metrics(400.0, 800.0, -200.0, 400.0).unwrap();
        assert!((top_mid - 100.0).abs() < 0.01);

        // Scrolled to bottom (-400.0px) -> thumb_top is at bottom (200.0px)
        let (top_end, _) = compute_scrollbar_metrics(400.0, 800.0, -400.0, 400.0).unwrap();
        assert!((top_end - 200.0).abs() < 0.01);
    }

    #[test]
    fn daylight_palette_uses_light_surface_and_cobalt_selection() {
        assert_eq!(DAYLIGHT_ROW_SELECTED_BG, 0xEFF6FF);
        assert_eq!(DAYLIGHT_ROW_SELECTED_BORDER, 0x2563EB);
        assert_eq!(DAYLIGHT_TITLE_TEXT, 0x0F172A);
        assert_eq!(DAYLIGHT_TITLE_SELECTED_TEXT, 0x1D4ED8);
    }
}
