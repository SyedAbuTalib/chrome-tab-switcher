use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, point, prelude::*, px, rgb, rgba,
};
use parking_lot::RwLock;

use crate::favicon::FaviconService;
use crate::hotkey::HotkeyEvent;
use crate::macos;
use crate::store::{ScoredTab, TabEntry, TabStore, clean_display_url};
use crate::ui::row::{compute_scrollbar_metrics, render_compact_tab_row, render_tab_row};
use crate::ui::{
    COMPACT_PADDING_Y, COMPACT_ROW_HEIGHT, EMPTY_STATE_HEIGHT, GRID_COLUMNS, GRID_GAP,
    GRID_PADDING_Y, HEADER_HEIGHT, MAX_PANEL_HEIGHT, PANEL_WIDTH, ROW_HEIGHT,
    compute_panel_height_for_mode,
};
use crate::ws_server::WsBridgeHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Hidden,
    Cycling,
    LatchedSearch,
}

#[derive(Debug, Clone)]
pub enum SwitcherOutcome {
    NoOp,
    ShowOrUpdate,
    CommitSelected(TabEntry),
    /// Enter on a search that matched no open tab: open the query in a new Chrome tab instead.
    OpenInNewTab(OpenTarget),
    CancelAndHide,
}

/// What a no-match query should turn into when the user presses Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// Navigate a new tab to this fully-qualified URL.
    Url(String),
    /// Search this text with Chrome's default search engine.
    WebSearch(String),
}

/// Decide whether a no-match query should be navigated to or searched for, roughly the way the
/// omnibox does: a leading `?` forces a search, anything with whitespace is a search, explicit
/// schemes (`https://`, `chrome://`, `about:`) are URLs, and bare `host[:port][/path]` forms
/// (`github.com/foo`, `docs.rs`, `localhost:3000`, `192.168.0.1`) get a scheme prepended.
pub fn resolve_open_target(query: &str) -> Option<OpenTarget> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(forced) = trimmed.strip_prefix('?') {
        let forced = forced.trim();
        return (!forced.is_empty()).then(|| OpenTarget::WebSearch(forced.to_string()));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Some(OpenTarget::WebSearch(trimmed.to_string()));
    }

    let lower = trimmed.to_ascii_lowercase();
    if has_url_scheme(&lower) || lower.starts_with("about:") {
        return Some(OpenTarget::Url(trimmed.to_string()));
    }

    let authority = trimmed.split(['/', '?', '#']).next().unwrap_or_default();
    if looks_like_authority(authority) {
        let host = authority.split(':').next().unwrap_or_default();
        let scheme = if host.eq_ignore_ascii_case("localhost") || is_ipv4_literal(host) {
            "http://"
        } else {
            "https://"
        };
        return Some(OpenTarget::Url(format!("{scheme}{trimmed}")));
    }

    Some(OpenTarget::WebSearch(trimmed.to_string()))
}

/// `scheme://rest` with an RFC 3986 scheme (`https`, `chrome`, `file`, `chrome-extension`, …).
fn has_url_scheme(lower: &str) -> bool {
    match lower.split_once("://") {
        Some((scheme, rest)) => {
            !rest.is_empty()
                && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

fn is_ipv4_literal(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets
            .iter()
            .all(|octet| !octet.is_empty() && octet.parse::<u8>().is_ok())
}

/// `host[:port]` where host is `localhost`, an IPv4 literal, or a dotted domain whose last
/// label is alphabetic and at least two characters (so `v1.2` and `foo.b` stay searches).
fn looks_like_authority(authority: &str) -> bool {
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if let Some(port) = port
        && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") || is_ipv4_literal(host) {
        return true;
    }

    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let is_dns_label = |label: &str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if !labels.iter().all(|label| is_dns_label(label)) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// Upper bound on pasted query length so a stray multi-KB clipboard can't stall the matcher.
pub const MAX_PASTE_CHARS: usize = 256;

/// Delete the trailing word of a cursor-less query, mirroring macOS `deleteWordBackward:`:
/// trailing whitespace/punctuation is removed first, then the alphanumeric run before it.
///
/// `"hello world"` → `"hello "`, `"github.com/foo"` → `"github.com/"`, `"!youtube"` → `"!"`.
pub fn delete_word_backward(query: &mut String) {
    while query.ends_with(|ch: char| !ch.is_alphanumeric()) {
        query.pop();
    }
    while query.ends_with(char::is_alphanumeric) {
        query.pop();
    }
}

/// Normalize clipboard text into a fuzzy-search query: collapse all whitespace runs (including
/// newlines) to single spaces, strip the scheme / `www.` / trailing slash from pasted URLs so
/// they match the way tab URLs are indexed, and cap the length at [`MAX_PASTE_CHARS`].
pub fn normalize_pasted_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = if collapsed.starts_with("http://") || collapsed.starts_with("https://") {
        clean_display_url(&collapsed)
    } else {
        collapsed
    };
    normalized.chars().take(MAX_PASTE_CHARS).collect()
}

pub struct SwitcherController {
    pub mode: Mode,
    pub query: String,
    pub selected_index: usize,
    pub matches: Vec<ScoredTab>,
    pub total_tabs: usize,
    pub start_at_first_tab_when_chrome_inactive: bool,
    store: Arc<RwLock<TabStore>>,
}

impl SwitcherController {
    pub fn new(store: Arc<RwLock<TabStore>>) -> Self {
        Self {
            mode: Mode::Hidden,
            query: String::new(),
            selected_index: 0,
            matches: Vec::new(),
            total_tabs: 0,
            start_at_first_tab_when_chrome_inactive: true,
            store,
        }
    }

    /// Returns `true` when the switcher should render the high-density Compact Search List
    /// instead of the 3-column visual card grid.
    pub fn is_compact_search(&self) -> bool {
        self.mode == Mode::LatchedSearch && !self.query.is_empty()
    }

    pub fn refresh_matches(&mut self) {
        let mut guard = self.store.write();
        self.total_tabs = guard.len();
        self.matches = guard.query(&self.query);
        if self.matches.is_empty() {
            self.selected_index = 0;
        } else if self.selected_index >= self.matches.len() {
            self.selected_index = self.matches.len() - 1;
        }
    }

    /// Profile that should receive a new tab opened from a no-match search.
    pub fn most_recent_profile_id(&self) -> Option<String> {
        self.store.read().most_recent_profile_id()
    }

    /// Apply an edit to the query, latch into Search mode (so releasing `⌘` keeps the window
    /// open), and reset the selection to the top match.
    fn edit_query(&mut self, edit: impl FnOnce(&mut String)) -> SwitcherOutcome {
        self.mode = Mode::LatchedSearch;
        edit(&mut self.query);
        self.selected_index = 0;
        self.refresh_matches();
        SwitcherOutcome::ShowOrUpdate
    }

    /// Insert pasted clipboard text (`⌘V`) into the query. Resolved by the view layer, which
    /// owns the pasteboard read; this keeps the state machine pure and unit-testable.
    pub fn insert_text(&mut self, text: &str) -> SwitcherOutcome {
        if self.mode == Mode::Hidden {
            return SwitcherOutcome::NoOp;
        }
        let normalized = normalize_pasted_text(text);
        if normalized.is_empty() {
            return SwitcherOutcome::NoOp;
        }
        self.edit_query(|query| query.push_str(&normalized))
    }

    pub fn handle_hotkey(
        &mut self,
        event: HotkeyEvent,
        chrome_is_frontmost: bool,
    ) -> SwitcherOutcome {
        match self.mode {
            Mode::Hidden => match event {
                HotkeyEvent::CycleNext => {
                    self.mode = Mode::Cycling;
                    self.query.clear();
                    self.refresh_matches();
                    self.selected_index = self.initial_index_on_summon(true, chrome_is_frontmost);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::CyclePrev => {
                    self.mode = Mode::Cycling;
                    self.query.clear();
                    self.refresh_matches();
                    self.selected_index = self.initial_index_on_summon(false, chrome_is_frontmost);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::OpenLatchedSearch => {
                    self.mode = Mode::LatchedSearch;
                    self.query.clear();
                    self.refresh_matches();
                    self.selected_index = 0;
                    SwitcherOutcome::ShowOrUpdate
                }
                _ => SwitcherOutcome::NoOp,
            },
            Mode::Cycling => match event {
                HotkeyEvent::CycleNext | HotkeyEvent::NavigateRight => {
                    self.step_selection_by(1, true);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::CyclePrev | HotkeyEvent::NavigateLeft => {
                    self.step_selection_by(1, false);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::NavigateDown => {
                    self.step_selection_by(GRID_COLUMNS, true);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::NavigateUp => {
                    self.step_selection_by(GRID_COLUMNS, false);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::ModifierReleased | HotkeyEvent::Confirm => {
                    let chosen = self.matches.get(self.selected_index).map(|m| m.entry.clone());
                    self.reset_to_hidden();
                    if let Some(tab) = chosen {
                        SwitcherOutcome::CommitSelected(tab)
                    } else {
                        SwitcherOutcome::CancelAndHide
                    }
                }
                HotkeyEvent::OpenLatchedSearch | HotkeyEvent::CharInput(' ') => {
                    // Thumb-tapping Space (or pressing Option+`) latches into Search mode
                    // with an empty query so releasing Cmd does not dismiss the window.
                    self.mode = Mode::LatchedSearch;
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::CharInput(ch) => {
                    self.mode = Mode::LatchedSearch;
                    self.query.push(ch);
                    self.selected_index = 0;
                    self.refresh_matches();
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::Backspace => self.edit_query(|query| {
                    query.pop();
                }),
                HotkeyEvent::BackspaceWord => self.edit_query(delete_word_backward),
                HotkeyEvent::BackspaceLine => self.edit_query(String::clear),
                // Clipboard is read by `SwitcherView::dispatch_hotkey`, which calls `insert_text`.
                HotkeyEvent::Paste => SwitcherOutcome::NoOp,
                HotkeyEvent::Cancel => {
                    self.reset_to_hidden();
                    SwitcherOutcome::CancelAndHide
                }
            },
            Mode::LatchedSearch => match event {
                HotkeyEvent::ModifierReleased | HotkeyEvent::OpenLatchedSearch => {
                    SwitcherOutcome::NoOp
                }
                HotkeyEvent::CycleNext | HotkeyEvent::NavigateRight => {
                    self.step_selection_by(1, true);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::CyclePrev | HotkeyEvent::NavigateLeft => {
                    self.step_selection_by(1, false);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::NavigateDown => {
                    let step = if self.is_compact_search() {
                        1
                    } else {
                        GRID_COLUMNS
                    };
                    self.step_selection_by(step, true);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::NavigateUp => {
                    let step = if self.is_compact_search() {
                        1
                    } else {
                        GRID_COLUMNS
                    };
                    self.step_selection_by(step, false);
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::CharInput(ch) => {
                    if ch == ' ' && self.query.is_empty() {
                        return SwitcherOutcome::ShowOrUpdate;
                    }
                    self.query.push(ch);
                    self.selected_index = 0;
                    self.refresh_matches();
                    SwitcherOutcome::ShowOrUpdate
                }
                HotkeyEvent::Backspace => self.edit_query(|query| {
                    query.pop();
                }),
                HotkeyEvent::BackspaceWord => self.edit_query(delete_word_backward),
                HotkeyEvent::BackspaceLine => self.edit_query(String::clear),
                // Clipboard is read by `SwitcherView::dispatch_hotkey`, which calls `insert_text`.
                HotkeyEvent::Paste => SwitcherOutcome::NoOp,
                HotkeyEvent::Confirm => {
                    let chosen = self.matches.get(self.selected_index).map(|m| m.entry.clone());
                    // No open tab matched: fall through to opening the query in a new tab.
                    let fallback = chosen
                        .is_none()
                        .then(|| resolve_open_target(&self.query))
                        .flatten();
                    self.reset_to_hidden();
                    match (chosen, fallback) {
                        (Some(tab), _) => SwitcherOutcome::CommitSelected(tab),
                        (None, Some(target)) => SwitcherOutcome::OpenInNewTab(target),
                        (None, None) => SwitcherOutcome::CancelAndHide,
                    }
                }
                HotkeyEvent::Cancel => {
                    self.reset_to_hidden();
                    SwitcherOutcome::CancelAndHide
                }
            },
        }
    }

    fn initial_index_on_summon(&self, forward: bool, chrome_is_frontmost: bool) -> usize {
        let len = self.matches.len();
        if len <= 1 {
            return 0;
        }
        if !forward {
            return len - 1;
        }
        if chrome_is_frontmost || !self.start_at_first_tab_when_chrome_inactive {
            1
        } else {
            0
        }
    }

    fn step_selection_by(&mut self, step: usize, forward: bool) {
        let len = self.matches.len();
        if len == 0 {
            self.selected_index = 0;
            return;
        }
        let step_mod = step.max(1) % len;
        let effective_step = if step_mod == 0 { 1 } else { step_mod };
        if forward {
            self.selected_index = (self.selected_index + effective_step) % len;
        } else {
            self.selected_index = (self.selected_index + len - effective_step) % len;
        }
    }

    pub fn reset_to_hidden(&mut self) {
        self.mode = Mode::Hidden;
        self.query.clear();
        self.selected_index = 0;
    }
}

pub struct SwitcherView {
    pub controller: SwitcherController,
    pub ws_handle: WsBridgeHandle,
    pub favicon_service: FaviconService,
    pub switcher_visible_flag: Arc<AtomicBool>,
    pub scroll_handle: ScrollHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelWindowAction {
    None,
    Show,
    Hide,
}

impl SwitcherView {
    pub fn new(
        store: Arc<RwLock<TabStore>>,
        ws_handle: WsBridgeHandle,
        favicon_service: FaviconService,
        switcher_visible_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            controller: SwitcherController::new(store),
            ws_handle,
            favicon_service,
            switcher_visible_flag,
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn sync_scroll_to_selection(&self, reset_top: bool) {
        if reset_top {
            self.scroll_handle.set_offset(point(px(0.0), px(0.0)));
        }

        let match_count = self.controller.matches.len();
        if match_count == 0 {
            return;
        }

        let compact = self.controller.is_compact_search();
        let card_height = compute_panel_height_for_mode(match_count, compact) as f32;
        let viewport_h = (card_height - HEADER_HEIGHT as f32).max(1.0);

        let (item_top, item_bottom, max_scroll) = if compact {
            let idx = self.controller.selected_index;
            let pad = (COMPACT_PADDING_Y * 0.5) as f32;
            let row_h = COMPACT_ROW_HEIGHT as f32;
            let top = pad + (idx as f32) * row_h;
            let bottom = top + row_h + pad;
            let total_h = (match_count as f32) * row_h + COMPACT_PADDING_Y as f32;
            (top, bottom, (total_h - viewport_h).max(0.0))
        } else {
            let row_idx = self.controller.selected_index / GRID_COLUMNS;
            let total_rows = match_count.div_ceil(GRID_COLUMNS);
            let pad = (GRID_PADDING_Y * 0.5) as f32;
            let row_h = ROW_HEIGHT as f32;
            let gap = GRID_GAP as f32;
            let top = pad + (row_idx as f32) * (row_h + gap);
            let bottom = top + row_h + pad;
            let total_h = (total_rows as f32) * row_h
                + (total_rows.saturating_sub(1) as f32) * gap
                + GRID_PADDING_Y as f32;
            (top, bottom, (total_h - viewport_h).max(0.0))
        };

        let current_scrolled = (-f32::from(self.scroll_handle.offset().y)).clamp(0.0, max_scroll);
        let new_scrolled = if item_top < current_scrolled {
            (item_top - 12.0).max(0.0)
        } else if item_bottom > current_scrolled + viewport_h {
            (item_bottom - viewport_h).min(max_scroll)
        } else {
            current_scrolled
        };

        self.scroll_handle
            .set_offset(point(px(0.0), px(-new_scrolled)));
    }

    pub fn activate_tab_at_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(scored) = self.controller.matches.get(index) {
            let tab = scored.entry.clone();
            self.controller.reset_to_hidden();
            self.switcher_visible_flag.store(false, Ordering::Release);
            self.ws_handle
                .activate_tab(&tab.key.profile_id, tab.window_id, tab.key.tab_id);
            macos::activate_chrome_application();
            macos::hide_floating_panel();
            cx.notify();
        }
    }

    pub fn dispatch_hotkey(
        &mut self,
        event: HotkeyEvent,
        cx: &mut Context<Self>,
    ) -> PanelWindowAction {
        let was_hidden = self.controller.mode == Mode::Hidden;
        let chrome_frontmost = if was_hidden {
            macos::is_chrome_frontmost()
        } else {
            false
        };

        let outcome = match event {
            // The event tap only signals paste intent; read the pasteboard here on the GPUI
            // thread so the tap callback never blocks on an AppKit/XPC round-trip.
            HotkeyEvent::Paste if !was_hidden => match macos::read_general_pasteboard_string() {
                Some(text) => self.controller.insert_text(&text),
                None => SwitcherOutcome::NoOp,
            },
            event => self.controller.handle_hotkey(event, chrome_frontmost),
        };
        match outcome {
            SwitcherOutcome::NoOp => PanelWindowAction::None,
            SwitcherOutcome::ShowOrUpdate => {
                self.switcher_visible_flag.store(true, Ordering::Release);
                self.sync_scroll_to_selection(was_hidden);
                cx.notify();
                if was_hidden {
                    PanelWindowAction::Show
                } else {
                    PanelWindowAction::None
                }
            }
            SwitcherOutcome::CommitSelected(tab) => {
                self.switcher_visible_flag.store(false, Ordering::Release);
                self.ws_handle
                    .activate_tab(&tab.key.profile_id, tab.window_id, tab.key.tab_id);
                macos::activate_chrome_application();
                cx.notify();
                PanelWindowAction::Hide
            }
            SwitcherOutcome::OpenInNewTab(target) => {
                self.switcher_visible_flag.store(false, Ordering::Release);
                let profile_id = self.controller.most_recent_profile_id();
                let delivered = match &target {
                    OpenTarget::Url(url) => self.ws_handle.open_url(profile_id.as_deref(), url),
                    OpenTarget::WebSearch(text) => {
                        self.ws_handle.search_web(profile_id.as_deref(), text)
                    }
                };
                if !delivered {
                    log::warn!(
                        "No Chrome extension connection to open {target:?} — is extension/ loaded?"
                    );
                }
                macos::activate_chrome_application();
                cx.notify();
                PanelWindowAction::Hide
            }
            SwitcherOutcome::CancelAndHide => {
                self.switcher_visible_flag.store(false, Ordering::Release);
                cx.notify();
                PanelWindowAction::Hide
            }
        }
    }

    pub fn on_store_or_favicon_updated(&mut self, cx: &mut Context<Self>) {
        if self.controller.mode != Mode::Hidden {
            self.controller.refresh_matches();
            cx.notify();
        }
    }
}

impl Render for SwitcherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.controller.mode;
        let compact_mode = self.controller.is_compact_search();
        let total_tabs = self.controller.total_tabs;
        let match_count = self.controller.matches.len();
        let selected_index = self.controller.selected_index;

        let card_height = compute_panel_height_for_mode(match_count, compact_mode);
        macos::set_active_card_height(card_height);
        let viewport_height = (card_height - HEADER_HEIGHT).max(0.0) as f32;

        let header = div()
            .w_full()
            .h(px(HEADER_HEIGHT as f32))
            .flex_none()
            .px_4()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(rgb(0xFFFFFF))
            .border_b_1()
            .border_color(rgb(0xE2E8F0))
            .child(
                div()
                    .flex_1()
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
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .bg(rgb(0xEFF6FF))
                            .text_size(px(13.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0x2563EB))
                            .child("⌘"),
                    )
                    .child(if mode == Mode::LatchedSearch {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .text_size(px(14.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(0x0F172A))
                            .child(self.controller.query.clone())
                            .child(
                                div()
                                    .text_size(px(14.5))
                                    .text_color(rgb(0x2563EB))
                                    .child("▏"),
                            )
                            .when(self.controller.query.is_empty(), |this| {
                                this.child(
                                    div()
                                        .ml_2()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::NORMAL)
                                        .text_color(rgb(0x94A3B8))
                                        .child("Type to filter in compact list, or scroll cards…"),
                                )
                            })
                    } else {
                        div()
                            .text_size(px(13.5))
                            .text_color(rgb(0x64748B))
                            .child(format!(
                                "Press Space (or ⌥`) to search across {total_tabs} Chrome tabs… (Release ⌘ to switch)"
                            ))
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded(px(6.0))
                            .bg(rgb(0xF1F5F9))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(0x64748B))
                            .child(format!("{match_count} / {total_tabs} tabs")),
                    )
                    .when(mode == Mode::LatchedSearch, |this| {
                        this.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(0x64748B))
                                .child(if match_count == 0 && total_tabs > 0 {
                                    "· ↵ Open in new tab · Esc Cancel"
                                } else {
                                    "· ↵ Switch · Esc Cancel"
                                }),
                        )
                    }),
            );

        let body_element = if match_count == 0 {
            div()
                .w_full()
                .h(px(EMPTY_STATE_HEIGHT as f32))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.5))
                .text_color(rgb(0x64748B))
                .child(if total_tabs == 0 {
                    "No Chrome tabs connected yet — load extension/ in chrome://extensions"
                        .to_string()
                } else {
                    match resolve_open_target(&self.controller.query) {
                        Some(OpenTarget::Url(url)) => {
                            format!("No matching tabs · ↵ Open {url} in a new tab")
                        }
                        Some(OpenTarget::WebSearch(text)) => {
                            format!("No matching tabs · ↵ Search the web for \"{text}\"")
                        }
                        None => {
                            format!("No matching Chrome tabs for \"{}\"", self.controller.query)
                        }
                    }
                })
                .into_any_element()
        } else if compact_mode {
            let content_height =
                (match_count as f32) * (COMPACT_ROW_HEIGHT as f32) + (COMPACT_PADDING_Y as f32);
            let scroll_offset_y = f32::from(self.scroll_handle.offset().y);
            let track_height = (viewport_height - 16.0).max(24.0);
            let thumb_metrics = compute_scrollbar_metrics(
                viewport_height,
                content_height,
                scroll_offset_y,
                track_height,
            );

            let compact_rows: Vec<_> = (0..match_count)
                .map(|idx| {
                    let item = &self.controller.matches[idx];
                    let favicon = self
                        .favicon_service
                        .get_or_fetch(item.entry.fav_icon_url.as_deref(), &item.entry.url);
                    div()
                        .id(("compact-row", idx))
                        .w_full()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, _, cx| {
                                view.activate_tab_at_index(idx, cx);
                            }),
                        )
                        .child(render_compact_tab_row(item, idx == selected_index, favicon))
                })
                .collect();

            div()
                .relative()
                .w_full()
                .flex_1()
                .flex()
                .flex_row()
                .overflow_hidden()
                .child(
                    div()
                        .id("tabs-scroll-container")
                        .flex_1()
                        .h_full()
                        .px_3()
                        .py_2()
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        .children(compact_rows),
                )
                .when_some(thumb_metrics, |this, (thumb_top, thumb_h)| {
                    this.child(
                        div()
                            .w(px(12.0))
                            .h_full()
                            .py_2()
                            .pr_1p5()
                            .flex_none()
                            .child(
                                div()
                                    .relative()
                                    .w(px(6.0))
                                    .h(px(track_height))
                                    .rounded_full()
                                    .bg(rgba(0xCBD5E166))
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(thumb_top))
                                            .left(px(0.0))
                                            .w(px(6.0))
                                            .h(px(thumb_h))
                                            .rounded_full()
                                            .bg(rgb(0x94A3B8)),
                                    ),
                            ),
                    )
                })
                .into_any_element()
        } else {
            let total_grid_rows = match_count.div_ceil(GRID_COLUMNS);
            let content_height = (total_grid_rows as f32) * (ROW_HEIGHT as f32)
                + (total_grid_rows.saturating_sub(1) as f32) * (GRID_GAP as f32)
                + (GRID_PADDING_Y as f32);
            let scroll_offset_y = f32::from(self.scroll_handle.offset().y);
            let track_height = (viewport_height - 20.0).max(24.0);
            let thumb_metrics = compute_scrollbar_metrics(
                viewport_height,
                content_height,
                scroll_offset_y,
                track_height,
            );

            let all_indices: Vec<usize> = (0..match_count).collect();
            let grid_rows: Vec<_> = all_indices
                .chunks(GRID_COLUMNS)
                .enumerate()
                .map(|(row_ix, row_indices)| {
                    let cards: Vec<_> = row_indices
                        .iter()
                        .copied()
                        .map(|idx| {
                            let item = &self.controller.matches[idx];
                            let favicon = self
                                .favicon_service
                                .get_or_fetch(item.entry.fav_icon_url.as_deref(), &item.entry.url);
                            let preview = self.favicon_service.get_or_fetch_preview(
                                item.entry.preview_url.as_deref(),
                                &item.entry.url,
                            );
                            div()
                                .id(("grid-card", idx))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.activate_tab_at_index(idx, cx);
                                    }),
                                )
                                .child(render_tab_row(
                                    item,
                                    idx == selected_index,
                                    favicon,
                                    preview,
                                ))
                        })
                        .collect();

                    div()
                        .id(("grid-row", row_ix))
                        .w_full()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(GRID_GAP as f32))
                        .children(cards)
                })
                .collect();

            div()
                .relative()
                .w_full()
                .flex_1()
                .flex()
                .flex_row()
                .overflow_hidden()
                .child(
                    div()
                        .id("tabs-scroll-container")
                        .flex_1()
                        .h_full()
                        .p(px(12.0))
                        .flex()
                        .flex_col()
                        .gap(px(GRID_GAP as f32))
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        .children(grid_rows),
                )
                .when_some(thumb_metrics, |this, (thumb_top, thumb_h)| {
                    this.child(
                        div()
                            .w(px(12.0))
                            .h_full()
                            .py(px(10.0))
                            .pr_1p5()
                            .flex_none()
                            .child(
                                div()
                                    .relative()
                                    .w(px(6.0))
                                    .h(px(track_height))
                                    .rounded_full()
                                    .bg(rgba(0xCBD5E166))
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(thumb_top))
                                            .left(px(0.0))
                                            .w(px(6.0))
                                            .h(px(thumb_h))
                                            .rounded_full()
                                            .bg(rgb(0x94A3B8)),
                                    ),
                            ),
                    )
                })
                .into_any_element()
        };

        let card = div()
            .w(px(PANEL_WIDTH as f32))
            .h(px(card_height as f32))
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgba(0xF8FAFCF5))
            .border_1()
            .border_color(rgb(0xE2E8F0))
            .rounded(px(16.0))
            .overflow_hidden()
            .shadow_lg()
            .child(header)
            .child(body_element);

        div()
            .w(px(PANEL_WIDTH as f32))
            .h(px(MAX_PANEL_HEIGHT as f32))
            .flex()
            .flex_col()
            .items_start()
            .justify_start()
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ExtensionToDaemon, WireTab};

    fn seeded_store() -> Arc<RwLock<TabStore>> {
        let mut store = TabStore::new();
        store.apply(ExtensionToDaemon::Snapshot {
            profile_id: "p1".into(),
            profile_name: "Default".into(),
            tabs: vec![
                WireTab {
                    tab_id: 1,
                    window_id: 10,
                    index: 0,
                    title: "Current Active Tab".into(),
                    url: "https://active.example.com".into(),
                    fav_icon_url: None,
                    preview_url: None,
                    active: true,
                    pinned: false,
                    audible: false,
                    muted: false,
                    last_accessed: Some(3000.0),
                },
                WireTab {
                    tab_id: 2,
                    window_id: 10,
                    index: 1,
                    title: "Previous GitHub PR".into(),
                    url: "https://github.com/org/repo/pull/421".into(),
                    fav_icon_url: None,
                    preview_url: None,
                    active: false,
                    pinned: true,
                    audible: false,
                    muted: false,
                    last_accessed: Some(2000.0),
                },
                WireTab {
                    tab_id: 3,
                    window_id: 10,
                    index: 2,
                    title: "Rust Documentation".into(),
                    url: "https://doc.rust-lang.org".into(),
                    fav_icon_url: None,
                    preview_url: None,
                    active: false,
                    pinned: false,
                    audible: true,
                    muted: false,
                    last_accessed: Some(1000.0),
                },
            ],
        });
        Arc::new(RwLock::new(store))
    }

    #[test]
    fn quick_tap_cycle_starts_at_index_1_when_chrome_active_and_commits_on_cmd_release() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store);

        // Summon while Chrome is frontmost -> highlights index 1 (tab_id = 2)
        let out = ctrl.handle_hotkey(HotkeyEvent::CycleNext, true);
        assert!(matches!(out, SwitcherOutcome::ShowOrUpdate));
        assert_eq!(ctrl.mode, Mode::Cycling);
        assert!(!ctrl.is_compact_search());
        assert_eq!(ctrl.selected_index, 1);
        assert_eq!(ctrl.matches[ctrl.selected_index].entry.key.tab_id, 2);

        // Release Cmd -> immediately commits tab_id = 2 and hides
        let release_out = ctrl.handle_hotkey(HotkeyEvent::ModifierReleased, true);
        match release_out {
            SwitcherOutcome::CommitSelected(tab) => assert_eq!(tab.key.tab_id, 2),
            other => panic!("expected CommitSelected, got {:?}", other),
        }
        assert_eq!(ctrl.mode, Mode::Hidden);
    }

    #[test]
    fn latch_on_type_enables_compact_search_mode_and_commits_on_enter() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store);

        // Summon from non-Chrome app -> starts at index 0 in card grid mode
        ctrl.handle_hotkey(HotkeyEvent::CycleNext, false);
        assert_eq!(ctrl.mode, Mode::Cycling);
        assert!(!ctrl.is_compact_search());
        assert_eq!(ctrl.selected_index, 0);

        // Type 'r', 'u' while in Cycling -> latches into LatchedSearch AND activates compact list mode
        ctrl.handle_hotkey(HotkeyEvent::CharInput('r'), false);
        ctrl.handle_hotkey(HotkeyEvent::CharInput('u'), false);
        assert_eq!(ctrl.mode, Mode::LatchedSearch);
        assert!(ctrl.is_compact_search());
        assert_eq!(ctrl.query, "ru");
        assert_eq!(ctrl.matches[0].entry.key.tab_id, 3);

        // Release Cmd while in LatchedSearch -> must NOT dismiss or commit!
        let release_out = ctrl.handle_hotkey(HotkeyEvent::ModifierReleased, false);
        assert!(matches!(release_out, SwitcherOutcome::NoOp));
        assert_eq!(ctrl.mode, Mode::LatchedSearch);

        // Press Enter -> commits top fuzzy match (tab_id = 3)
        let confirm_out = ctrl.handle_hotkey(HotkeyEvent::Confirm, false);
        match confirm_out {
            SwitcherOutcome::CommitSelected(tab) => assert_eq!(tab.key.tab_id, 3),
            other => panic!("expected CommitSelected, got {:?}", other),
        }
        assert_eq!(ctrl.mode, Mode::Hidden);
    }

    #[test]
    fn esc_cancels_cycling_and_subsequent_cmd_release_is_ignored() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store);

        ctrl.handle_hotkey(HotkeyEvent::CycleNext, true);
        assert_eq!(ctrl.mode, Mode::Cycling);

        let esc_out = ctrl.handle_hotkey(HotkeyEvent::Cancel, true);
        assert!(matches!(esc_out, SwitcherOutcome::CancelAndHide));
        assert_eq!(ctrl.mode, Mode::Hidden);

        let release_out = ctrl.handle_hotkey(HotkeyEvent::ModifierReleased, true);
        assert!(matches!(release_out, SwitcherOutcome::NoOp));
    }

    #[test]
    fn space_in_cycling_or_option_backtick_latches_search_with_empty_query() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store.clone());

        // 1. Summon with Cmd+`, then tap Space while holding Cmd -> latches with empty query (still card grid)
        ctrl.handle_hotkey(HotkeyEvent::CycleNext, true);
        assert_eq!(ctrl.mode, Mode::Cycling);
        assert_eq!(ctrl.selected_index, 1);

        let space_out = ctrl.handle_hotkey(HotkeyEvent::CharInput(' '), true);
        assert!(matches!(space_out, SwitcherOutcome::ShowOrUpdate));
        assert_eq!(ctrl.mode, Mode::LatchedSearch);
        assert_eq!(ctrl.query, "");
        assert!(!ctrl.is_compact_search());
        assert_eq!(ctrl.selected_index, 1);

        // Releasing Cmd after Space latch must NOT close or commit
        let release_out = ctrl.handle_hotkey(HotkeyEvent::ModifierReleased, true);
        assert!(matches!(release_out, SwitcherOutcome::NoOp));
        assert_eq!(ctrl.mode, Mode::LatchedSearch);

        // Typing characters switches into compact search mode
        ctrl.handle_hotkey(HotkeyEvent::CharInput('g'), false);
        assert!(ctrl.is_compact_search());
        ctrl.handle_hotkey(HotkeyEvent::CharInput(' '), false);
        ctrl.handle_hotkey(HotkeyEvent::CharInput('p'), false);
        assert_eq!(ctrl.query, "g p");

        // 2. Direct Option+` from Hidden opens directly in LatchedSearch
        let mut ctrl2 = SwitcherController::new(store);
        let opt_out = ctrl2.handle_hotkey(HotkeyEvent::OpenLatchedSearch, true);
        assert!(matches!(opt_out, SwitcherOutcome::ShowOrUpdate));
        assert_eq!(ctrl2.mode, Mode::LatchedSearch);
        assert_eq!(ctrl2.query, "");
        assert_eq!(ctrl2.selected_index, 0);
    }

    #[test]
    fn delete_word_backward_mirrors_macos_delete_word_backward_semantics() {
        let cases = [
            ("hello world", "hello "),
            ("hello world   ", "hello "),
            ("github.com/foo", "github.com/"),
            ("github.com/", "github."),
            ("!youtube", "!"),
            ("rust docs", "rust "),
            ("foo", ""),
            ("   ", ""),
            ("", ""),
        ];
        for (input, expected) in cases {
            let mut query = input.to_string();
            delete_word_backward(&mut query);
            assert_eq!(query, expected, "delete_word_backward({input:?})");
        }
    }

    #[test]
    fn option_backspace_deletes_word_and_cmd_backspace_clears_query() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store);

        ctrl.handle_hotkey(HotkeyEvent::OpenLatchedSearch, false);
        for ch in "rust docs".chars() {
            ctrl.handle_hotkey(HotkeyEvent::CharInput(ch), false);
        }
        assert_eq!(ctrl.query, "rust docs");
        assert!(ctrl.is_compact_search());
        assert_eq!(ctrl.matches[0].entry.key.tab_id, 3);

        // ⌥⌫ removes only the trailing word (leaving the separator, like macOS)
        let word_out = ctrl.handle_hotkey(HotkeyEvent::BackspaceWord, false);
        assert!(matches!(word_out, SwitcherOutcome::ShowOrUpdate));
        assert_eq!(ctrl.query, "rust ");
        assert_eq!(ctrl.mode, Mode::LatchedSearch);
        assert_eq!(ctrl.selected_index, 0);
        assert_eq!(ctrl.matches[0].entry.key.tab_id, 3);

        // A second ⌥⌫ empties the query and drops back to the full MRU card grid
        ctrl.handle_hotkey(HotkeyEvent::BackspaceWord, false);
        assert_eq!(ctrl.query, "");
        assert!(!ctrl.is_compact_search());
        assert_eq!(ctrl.matches.len(), 3);

        // ⌘⌫ clears a multi-word query in one keystroke without leaving Search mode
        for ch in "git pr".chars() {
            ctrl.handle_hotkey(HotkeyEvent::CharInput(ch), false);
        }
        assert_eq!(ctrl.matches[0].entry.key.tab_id, 2);
        let line_out = ctrl.handle_hotkey(HotkeyEvent::BackspaceLine, false);
        assert!(matches!(line_out, SwitcherOutcome::ShowOrUpdate));
        assert_eq!(ctrl.query, "");
        assert_eq!(ctrl.mode, Mode::LatchedSearch);
        assert_eq!(ctrl.matches.len(), 3);

        // From Cycling (⌘ still held from the summon), ⌘⌫ latches into Search like plain ⌫ does
        let mut cycling = SwitcherController::new(seeded_store());
        cycling.handle_hotkey(HotkeyEvent::CycleNext, true);
        assert_eq!(cycling.mode, Mode::Cycling);
        cycling.handle_hotkey(HotkeyEvent::BackspaceLine, true);
        assert_eq!(cycling.mode, Mode::LatchedSearch);
        assert_eq!(cycling.query, "");
        let release_out = cycling.handle_hotkey(HotkeyEvent::ModifierReleased, true);
        assert!(matches!(release_out, SwitcherOutcome::NoOp));
    }

    #[test]
    fn normalize_pasted_text_collapses_whitespace_strips_url_scheme_and_caps_length() {
        assert_eq!(
            normalize_pasted_text("  Fix\tthe\r\n\nbug  "),
            "Fix the bug"
        );
        assert_eq!(
            normalize_pasted_text("https://www.github.com/org/repo/pull/421/\n"),
            "github.com/org/repo/pull/421"
        );
        assert_eq!(normalize_pasted_text("http://example.com"), "example.com");
        // Non-URL text that merely contains a scheme is left alone
        assert_eq!(normalize_pasted_text("see https://x.y"), "see https://x.y");
        assert_eq!(normalize_pasted_text("\n \t "), "");
        let huge = "a".repeat(MAX_PASTE_CHARS * 4);
        assert_eq!(
            normalize_pasted_text(&huge).chars().count(),
            MAX_PASTE_CHARS
        );
    }

    #[test]
    fn paste_inserts_normalized_clipboard_text_and_latches_compact_search() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store.clone());

        // Pasting while Hidden is ignored (⌘V belongs to the frontmost app then anyway)
        assert!(matches!(ctrl.insert_text("github"), SwitcherOutcome::NoOp));
        assert_eq!(ctrl.mode, Mode::Hidden);
        assert_eq!(ctrl.query, "");

        // Summon with ⌘` then ⌘V a full URL copied from the address bar: the scheme is stripped so
        // it fuzzy-matches the indexed display URL, and the switcher latches into compact search.
        ctrl.handle_hotkey(HotkeyEvent::CycleNext, true);
        assert_eq!(ctrl.mode, Mode::Cycling);
        let paste_out = ctrl.insert_text("https://github.com/org/repo/pull/421\n");
        assert!(matches!(paste_out, SwitcherOutcome::ShowOrUpdate));
        assert_eq!(ctrl.mode, Mode::LatchedSearch);
        assert!(ctrl.is_compact_search());
        assert_eq!(ctrl.query, "github.com/org/repo/pull/421");
        assert_eq!(ctrl.selected_index, 0);
        assert_eq!(ctrl.matches[0].entry.key.tab_id, 2);

        // Releasing ⌘ after a paste must not dismiss or commit
        assert!(matches!(
            ctrl.handle_hotkey(HotkeyEvent::ModifierReleased, true),
            SwitcherOutcome::NoOp
        ));
        assert_eq!(ctrl.mode, Mode::LatchedSearch);

        // Whitespace-only clipboard is a no-op and leaves the query untouched
        assert!(matches!(ctrl.insert_text("  \n"), SwitcherOutcome::NoOp));
        assert_eq!(ctrl.query, "github.com/org/repo/pull/421");

        // Paste appends to an existing query rather than replacing it
        let mut ctrl2 = SwitcherController::new(store);
        ctrl2.handle_hotkey(HotkeyEvent::OpenLatchedSearch, false);
        ctrl2.handle_hotkey(HotkeyEvent::CharInput('r'), false);
        ctrl2.insert_text("ust");
        assert_eq!(ctrl2.query, "rust");
        assert_eq!(ctrl2.matches[0].entry.key.tab_id, 3);

        // The raw `Paste` event is resolved by the view layer; the pure controller treats it as a no-op
        assert!(matches!(
            ctrl2.handle_hotkey(HotkeyEvent::Paste, false),
            SwitcherOutcome::NoOp
        ));
        assert_eq!(ctrl2.query, "rust");
    }

    #[test]
    fn resolve_open_target_classifies_urls_hosts_and_searches_like_the_omnibox() {
        use OpenTarget::{Url, WebSearch};
        let url = |s: &str| Some(Url(s.to_string()));
        let search = |s: &str| Some(WebSearch(s.to_string()));

        let cases = [
            // Explicit schemes pass through untouched
            ("https://github.com/x", url("https://github.com/x")),
            ("HTTP://Example.com", url("HTTP://Example.com")),
            ("chrome://extensions", url("chrome://extensions")),
            ("about:blank", url("about:blank")),
            // Bare hosts / paths get https://, localhost and IPs get http://
            ("github.com/org/repo", url("https://github.com/org/repo")),
            ("docs.rs", url("https://docs.rs")),
            ("news.ycombinator.com?x=1", url("https://news.ycombinator.com?x=1")),
            ("localhost:3000/app", url("http://localhost:3000/app")),
            ("192.168.1.10:8080", url("http://192.168.1.10:8080")),
            ("  github.com  ", url("https://github.com")),
            // Everything else is a web search
            ("rust gpui docs", search("rust gpui docs")),
            ("gpui", search("gpui")),
            ("v1.2", search("v1.2")),
            ("foo.b", search("foo.b")),
            ("-bad-.com", search("-bad-.com")),
            ("github.com:abc", search("github.com:abc")),
            ("300.1.1.1", search("300.1.1.1")),
            ("://nope", search("://nope")),
            // Leading `?` forces a search even for URL-looking text
            ("?docs.rs", search("docs.rs")),
            // Nothing to open
            ("", None),
            ("   ", None),
            ("?", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                resolve_open_target(input),
                expected,
                "resolve_open_target({input:?})"
            );
        }
    }

    #[test]
    fn enter_with_no_matching_tab_opens_query_in_new_tab() {
        let store = seeded_store();
        let mut ctrl = SwitcherController::new(store.clone());

        // A query that matches nothing -> Enter opens a web search and hides the switcher
        ctrl.handle_hotkey(HotkeyEvent::OpenLatchedSearch, false);
        for ch in "zebra quokka".chars() {
            ctrl.handle_hotkey(HotkeyEvent::CharInput(ch), false);
        }
        assert!(ctrl.matches.is_empty());
        let out = ctrl.handle_hotkey(HotkeyEvent::Confirm, false);
        match out {
            SwitcherOutcome::OpenInNewTab(OpenTarget::WebSearch(text)) => {
                assert_eq!(text, "zebra quokka")
            }
            other => panic!("expected OpenInNewTab(WebSearch), got {other:?}"),
        }
        assert_eq!(ctrl.mode, Mode::Hidden);
        assert_eq!(ctrl.query, "");

        // A URL-shaped query that matches nothing -> navigates
        ctrl.handle_hotkey(HotkeyEvent::OpenLatchedSearch, false);
        ctrl.insert_text("crates.io/crates/nucleo");
        assert!(ctrl.matches.is_empty());
        match ctrl.handle_hotkey(HotkeyEvent::Confirm, false) {
            SwitcherOutcome::OpenInNewTab(OpenTarget::Url(url)) => {
                assert_eq!(url, "https://crates.io/crates/nucleo")
            }
            other => panic!("expected OpenInNewTab(Url), got {other:?}"),
        }

        // When something matches, Enter still switches to it (never opens a new tab)
        ctrl.handle_hotkey(HotkeyEvent::OpenLatchedSearch, false);
        ctrl.insert_text("github.com/org/repo/pull/421");
        assert!(!ctrl.matches.is_empty());
        match ctrl.handle_hotkey(HotkeyEvent::Confirm, false) {
            SwitcherOutcome::CommitSelected(tab) => assert_eq!(tab.key.tab_id, 2),
            other => panic!("expected CommitSelected, got {other:?}"),
        }

        // Empty query + Enter in Search mode stays a plain cancel (nothing to open)
        let mut empty_store = SwitcherController::new(Arc::new(RwLock::new(TabStore::new())));
        empty_store.handle_hotkey(HotkeyEvent::OpenLatchedSearch, false);
        assert!(matches!(
            empty_store.handle_hotkey(HotkeyEvent::Confirm, false),
            SwitcherOutcome::CancelAndHide
        ));

        // Cycling mode never opens new tabs: ⌘ release with zero tabs just hides
        let mut cycling = SwitcherController::new(Arc::new(RwLock::new(TabStore::new())));
        cycling.handle_hotkey(HotkeyEvent::CycleNext, true);
        assert!(matches!(
            cycling.handle_hotkey(HotkeyEvent::ModifierReleased, true),
            SwitcherOutcome::CancelAndHide
        ));

        // The new tab goes to the profile owning the MRU tab
        assert_eq!(ctrl.most_recent_profile_id().as_deref(), Some("p1"));
    }
}
