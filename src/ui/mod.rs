pub mod row;
pub mod switcher_view;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossbeam_channel::Receiver;
use gpui::{
    App, Bounds, Context, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
    prelude::*, px, size,
};
use parking_lot::RwLock;

use crate::favicon::FaviconService;
use crate::hotkey::HotkeyEvent;
use crate::macos;
use crate::store::TabStore;
use crate::ui::switcher_view::{PanelWindowAction, SwitcherView};
use crate::ws_server::{StoreEvent, WsBridgeHandle};

pub const PANEL_WIDTH: f64 = 864.0;
pub const HEADER_HEIGHT: f64 = 48.0;
pub const GRID_COLUMNS: usize = 3;
pub const MAX_VISIBLE_GRID_ROWS: usize = 2;
pub const MAX_VISIBLE_ROWS: usize = GRID_COLUMNS * MAX_VISIBLE_GRID_ROWS;
pub const CARD_WIDTH: f64 = 266.0;
pub const CARD_SHOT_HEIGHT: f64 = 156.0;
pub const CARD_FOOTER_HEIGHT: f64 = 48.0;
pub const ROW_HEIGHT: f64 = CARD_SHOT_HEIGHT + CARD_FOOTER_HEIGHT;
pub const GRID_GAP: f64 = 12.0;
pub const GRID_PADDING_Y: f64 = 24.0;
pub const COMPACT_ROW_HEIGHT: f64 = 42.0;
pub const MAX_VISIBLE_COMPACT_ROWS: usize = 10;
pub const COMPACT_PADDING_Y: f64 = 16.0;
pub const EMPTY_STATE_HEIGHT: f64 = 88.0;
pub const MAX_PANEL_HEIGHT: f64 = HEADER_HEIGHT
    + (MAX_VISIBLE_GRID_ROWS as f64) * ROW_HEIGHT
    + GRID_GAP
    + GRID_PADDING_Y;

pub fn compute_panel_height(match_count: usize) -> f64 {
    compute_panel_height_for_mode(match_count, false)
}

pub fn compute_panel_height_for_mode(match_count: usize, compact_mode: bool) -> f64 {
    if match_count == 0 {
        return HEADER_HEIGHT + EMPTY_STATE_HEIGHT;
    }
    if compact_mode {
        let visible_rows = match_count.min(MAX_VISIBLE_COMPACT_ROWS);
        return HEADER_HEIGHT + (visible_rows as f64) * COMPACT_ROW_HEIGHT + COMPACT_PADDING_Y;
    }
    let grid_rows = match_count.div_ceil(GRID_COLUMNS).min(MAX_VISIBLE_GRID_ROWS);
    let gaps = if grid_rows > 1 {
        ((grid_rows - 1) as f64) * GRID_GAP
    } else {
        0.0
    };
    HEADER_HEIGHT + (grid_rows as f64) * ROW_HEIGHT + gaps + GRID_PADDING_Y
}

pub enum UiWakeEvent {
    Hotkey(HotkeyEvent),
    StoreUpdated(Vec<(Option<String>, String)>),
    FaviconRepaint,
}

pub fn open_prewarmed_switcher_window(
    cx: &mut App,
    store: Arc<RwLock<TabStore>>,
    ws_handle: WsBridgeHandle,
    switcher_visible_flag: Arc<AtomicBool>,
    hotkey_rx: Receiver<HotkeyEvent>,
    store_event_rx: Receiver<StoreEvent>,
) {
    let (repaint_tx, repaint_rx) = crossbeam_channel::unbounded::<()>();
    let favicon_service = FaviconService::spawn(repaint_tx);

    // Bridge lock-free crossbeam channels into an async mpsc stream that wakes GPUI's main-thread
    // Grand Central Dispatch queue directly with zero timer polling.
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel::<UiWakeEvent>();

    let wake_tx_hotkey = wake_tx.clone();
    std::thread::Builder::new()
        .name("hotkey-bridge".into())
        .spawn(move || {
            while let Ok(evt) = hotkey_rx.recv() {
                if wake_tx_hotkey.send(UiWakeEvent::Hotkey(evt)).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn hotkey-bridge thread");

    let wake_tx_store = wake_tx.clone();
    std::thread::Builder::new()
        .name("store-event-bridge".into())
        .spawn(move || {
            while let Ok(StoreEvent::Updated { fav_icon_targets }) = store_event_rx.recv() {
                if wake_tx_store
                    .send(UiWakeEvent::StoreUpdated(fav_icon_targets))
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("failed to spawn store-event-bridge thread");

    let wake_tx_favicon = wake_tx;
    std::thread::Builder::new()
        .name("favicon-event-bridge".into())
        .spawn(move || {
            while repaint_rx.recv().is_ok() {
                if wake_tx_favicon.send(UiWakeEvent::FaviconRepaint).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn favicon-event-bridge thread");

    // Pre-allocate the GPUI window at MAX_PANEL_HEIGHT (584px) so the Metal viewport is always
    // large enough for all 12 visible rows without ever needing a synchronous drawable resize.
    let bounds = Bounds::centered(
        None,
        size(px(PANEL_WIDTH as f32), px(MAX_PANEL_HEIGHT as f32)),
        cx,
    );

    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            // Frosted glass: GPUI inserts an always-active NSVisualEffectView behind the content.
            // `macos::configure_floating_panel` pins it to the card and rounds its corners.
            window_background: WindowBackgroundAppearance::Blurred,
            ..Default::default()
        },
        |_window, cx| {
            cx.new(|cx: &mut Context<SwitcherView>| {
                let view = SwitcherView::new(
                    store,
                    ws_handle,
                    favicon_service,
                    switcher_visible_flag,
                );

                cx.spawn(async move |this, cx| {
                    while let Some(wake_evt) = wake_rx.recv().await {
                        let action_res = this.update(cx, |view, cx| match wake_evt {
                            UiWakeEvent::Hotkey(hotkey_evt) => {
                                view.dispatch_hotkey(hotkey_evt, cx)
                            }
                            UiWakeEvent::StoreUpdated(fav_targets) => {
                                if !fav_targets.is_empty() {
                                    view.favicon_service.prefetch_batch(&fav_targets);
                                }
                                view.on_store_or_favicon_updated(cx);
                                PanelWindowAction::None
                            }
                            UiWakeEvent::FaviconRepaint => {
                                view.on_store_or_favicon_updated(cx);
                                PanelWindowAction::None
                            }
                        });

                        // Execute AppKit window frame/order operations outside `this.update` so
                        // GPUI's `App` borrow is never held if AppKit fires a synchronous delegate callback.
                        match action_res {
                            Ok(PanelWindowAction::Show) => {
                                macos::show_floating_panel(PANEL_WIDTH, MAX_PANEL_HEIGHT);
                            }
                            Ok(PanelWindowAction::Hide) => {
                                macos::hide_floating_panel();
                            }
                            Ok(PanelWindowAction::None) => {}
                            Err(_) => break,
                        }
                    }
                })
                .detach();

                view
            })
        },
    )
    .expect("failed to open pre-warmed GPUI floating switcher panel");

    macos::configure_floating_panel();
    macos::hide_floating_panel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_panel_height_scales_up_to_max_panel_height() {
        assert_eq!(compute_panel_height(0), 136.0);
        // 1..=3 tabs -> 1 row of 3 cards (48 + 204 + 24 = 276)
        assert_eq!(compute_panel_height(1), 276.0);
        assert_eq!(compute_panel_height(3), 276.0);
        // 4+ tabs -> 2 rows of 3 cards (48 + 2*204 + 12 + 24 = 492)
        assert_eq!(compute_panel_height(4), MAX_PANEL_HEIGHT);
        assert_eq!(compute_panel_height(6), MAX_PANEL_HEIGHT);
        assert_eq!(compute_panel_height(25), MAX_PANEL_HEIGHT);
        assert_eq!(MAX_PANEL_HEIGHT, 492.0);

        // Compact search mode scales by 42px per row up to 10 rows (48 + 10*42 + 16 = 484)
        assert_eq!(compute_panel_height_for_mode(3, true), 48.0 + 3.0 * 42.0 + 16.0);
        assert_eq!(compute_panel_height_for_mode(15, true), 48.0 + 10.0 * 42.0 + 16.0);
    }
}
