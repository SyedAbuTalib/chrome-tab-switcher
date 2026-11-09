# Zero-Latency macOS Chrome Tab Switcher (`chrome-tab-switcher`) — Implementation Plan

## 1. Product Overview & Core Goals

A standalone, ultra-low-latency macOS utility written in **Rust + GPUI** that provides an AltTab-style switcher **exclusively for Google Chrome tabs** across all open Chrome windows and profiles.

### Key Requirements
- **Zero-latency summon (< 2ms to screen)**: All Chrome tab state is pushed proactively over a local WebSocket (`127.0.0.1`) from a tiny Manifest V3 Chrome extension into an in-memory Rust store. Summoning the switcher **never** polls Chrome, spawns a subprocess, or blocks on IPC.
- **UI Stack**: **Rust + GPUI** (Zed's GPU-accelerated UI framework) hosted in a pre-allocated, borderless, non-activating floating macOS window (`WindowKind::PopUp` / `NSPanel`) that stays resident in VRAM and orders front/out without recreating GPU surfaces.
- **Global Shortcut (`Cmd + \``)**: Works globally from any application on macOS (coexisting cleanly alongside AltTab on `Option + Tab`).
  - Pressing `Cmd + \`` suppresses macOS's built-in "Move focus to next window" symbolic hotkey (or intercepts it at the `CGEventTap` level) and summons the Chrome tab switcher.
- **Hybrid Interaction Model ("Cycle or Latch-to-Search")**:
  1. **Hold & Cycle Mode**: Hold `Cmd` and tap `` ` `` (or `Shift + \`` to go backward) to cycle through open Chrome tabs in Most-Recently-Used (MRU) order. Releasing `Cmd` immediately switches to the highlighted tab and brings that Chrome window + tab to the front.
  2. **Latch-on-Type Search Mode**: While the switcher is open (even while holding `Cmd`), typing any character routes into the fuzzy search bar (`nucleo` matcher) and **latches** the switcher open. Once latched, releasing `Cmd` does **not** dismiss the window—the user can type comfortably with both hands, navigate matches with `Up`/`Down` (or `Ctrl+N`/`Ctrl+P`), press `Enter` to activate the selected tab, or press `Esc` to cancel.
- **Visual Layout**: Compact vertical list (~10–15 visible rows) displaying:
  - Cached tab Favicon (with fallback globe icon)
  - Tab Title (with matched fuzzy-search characters highlighted)
  - Shortened Domain / URL path (also searchable & highlighted)
  - Subtle badges for **Audio Playing** (`🔊`), **Pinned** (`📌`), and **Profile / Window identifier** (`Profile 1 · Win 2`) when multiple Chrome windows or profiles are connected.
- **Scope Discipline (v1)**: Strictly switching tabs only (no tab-closing, muting, or clipboard sub-actions in v1).

---

## 2. System Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│                     Google Chrome (1..N Profiles)                        │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │ MV3 Background Service Worker (`extension/`)                       │  │
│  │ - Listens to chrome.tabs.{onCreated,onUpdated,onRemoved,onActivated}│ │
│  │ - Listens to chrome.windows.{onFocusChanged,onRemoved}             │  │
│  │ - Connects to ws://127.0.0.1:17384 (persistent via WebSocket ping) │  │
│  │ - Sends Snapshot on connect + Delta events (< 1ms)                 │  │
│  │ - Executes ActivateTab -> chrome.tabs.update + windows.update      │  │
│  └───────────────────────────────┬────────────────────────────────────┘  │
└──────────────────────────────────┼───────────────────────────────────────┘
                                   │ WebSocket (JSON over loopback TCP_NODELAY)
                                   ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                Rust + GPUI Daemon (`chrome-tab-switcher`)                │
│                                                                          │
│  ┌──────────────────────┐   ┌────────────────────────┐                   │
│  │ CGEventTap Thread    │   │ Tokio WS Server Thread │                   │
│  │ (CFRunLoop, High QoS)│   │ (127.0.0.1:17384)      │                   │
│  │ - Intercepts Cmd+`   │   │ - Tracks 1..N profile  │                   │
│  │ - Detects Cmd release│   │   connections          │                   │
│  │ - Absorbs Esc / keys │   │ - Updates TabStore     │                   │
│  └──────────┬───────────┘   └───────────┬────────────┘                   │
│             │ lock-free channel         │ Arc<RwLock<TabStore>> + notify │
│             ▼                           ▼                                │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │ Main Thread (GPUI App + Pre-warmed Floating Panel)                 │  │
│  │ - SwitcherState machine: Hidden -> Cycling -> LatchedSearch        │  │
│  │ - Nucleo fuzzy matcher over "Title + Domain/URL + Profile"         │  │
│  │ - Favicon LRU cache (decoded GPUI ImageHandles)                    │  │
│  │ - On Commit: sends WS `activate_tab` + NSRunningApplication        │  │
│  │   activate to raise Chrome immediately                             │  │
│  └────────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Repository Structure

```text
chrome-tab-switcher/
├── plan.md
├── Cargo.toml
├── extension/
│   ├── manifest.json          # Manifest V3 (permissions: tabs, storage)
│   └── background.js          # Service worker: WS client, tab events, activation handler
└── src/
    ├── main.rs                # App entry, LSUIElement (accessory policy), GPUI initialization
    ├── protocol.rs            # Shared serde JSON wire types (Extension <-> Rust)
    ├── store.rs               # In-memory TabStore, MRU ordering, multi-profile merge, Nucleo index
    ├── ws_server.rs           # Tokio WebSocket server on 127.0.0.1:17384 with TCP_NODELAY
    ├── hotkey.rs              # macOS CGEventTap on dedicated CFRunLoop thread + symbolic hotkey suppression
    ├── macos.rs               # NSPanel non-activating configuration, Chrome PID activation, Accessibility check
    ├── favicon.rs             # Async favicon fetcher & in-memory LRU cache (url -> gpui::ImageSource)
    └── ui/
        ├── mod.rs             # GPUI Window setup, sizing, centering on active NSScreen
        ├── switcher_view.rs   # State machine (Hidden / Cycling / LatchedSearch), keyboard handlers
        └── row.rs             # Compact row rendering, match character highlighting, badges
```

---

## 4. Wire Protocol (`src/protocol.rs` & `extension/background.js`)

All messages use JSON over WebSocket on `ws://127.0.0.1:17384` with `TCP_NODELAY` enabled.

### 4.1 Extension -> Rust Daemon (`IncomingMessage`)

Each Chrome profile instance generates a persistent UUID (`profile_id`) stored in `chrome.storage.local` (along with a short human-readable label or window number) so multiple Chrome profiles connecting to the same localhost port are cleanly distinguished.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionToDaemon {
    /// Sent immediately upon WebSocket connection and after wake.
    Snapshot {
        profile_id: String,
        profile_name: String,
        tabs: Vec<WireTab>,
    },
    /// Sent on chrome.tabs.onCreated or chrome.tabs.onUpdated
    TabUpserted {
        profile_id: String,
        tab: WireTab,
    },
    /// Sent on chrome.tabs.onRemoved
    TabRemoved {
        profile_id: String,
        tab_id: i64,
    },
    /// Sent on chrome.tabs.onActivated or chrome.windows.onFocusChanged
    TabActivated {
        profile_id: String,
        window_id: i64,
        tab_id: i64,
        timestamp_ms: f64,
    },
    /// Sent on chrome.windows.onRemoved
    WindowRemoved {
        profile_id: String,
        window_id: i64,
    },
    /// Keepalive ping every 20s to keep MV3 Service Worker alive permanently
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireTab {
    pub tab_id: i64,
    pub window_id: i64,
    pub index: i32,
    pub title: String,
    pub url: String,
    pub fav_icon_url: Option<String>,
    pub active: bool,
    pub pinned: bool,
    pub audible: bool,
    pub muted: bool,
    /// Chrome's `tab.lastAccessed` (epoch ms, f64) for exact MRU sorting across restarts
    pub last_accessed: Option<f64>,
}
```

### 4.2 Rust Daemon -> Extension (`OutgoingMessage`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToExtension {
    ActivateTab {
        window_id: i64,
        tab_id: i64,
    },
    Pong,
}
```

---

## 5. Chrome Extension Design (`extension/`)

### 5.1 `manifest.json`
- `"manifest_version": 3`
- `"permissions": ["tabs", "storage"]`
- `"background": { "service_worker": "background.js", "type": "module" }`
- No content scripts needed (zero overhead on web pages; works on `chrome://`, Web Store, PDF viewer, and normal pages alike).

### 5.2 Keeping the MV3 Service Worker Alive
Chrome normally suspends idle MV3 service workers after 30 seconds, which would introduce reconnection latency.
- **Solution**: In Chrome 116+, an active WebSocket connection that exchanges messages at least once every 20 seconds resets the service worker idle timer indefinitely.
- `background.js` runs a `setInterval(() => ws.send(JSON.stringify({ type: "ping" })), 20_000)` while connected, and the Rust server replies with `{"type": "pong"}`.
- If the Rust daemon restarts, `background.js` reconnects with capped exponential backoff (`100ms -> 250ms -> 500ms -> 1000ms`) and immediately sends a fresh `Snapshot`.

### 5.3 Tab Activation Sequence in `background.js`
When `ActivateTab { window_id, tab_id }` arrives:
```javascript
await Promise.all([
  chrome.tabs.update(msg.tab_id, { active: true }),
  chrome.windows.update(msg.window_id, { focused: true, drawAttention: false })
]);
```
Simultaneously, the Rust process calls `NSRunningApplication.activateWithOptions(NSApplicationActivateIgnoringOtherApps)` on the Google Chrome process so macOS switches Space/focus to Chrome in parallel without waiting for the WebSocket round-trip.

---

## 6. Low-Latency Input & State Machine (`src/hotkey.rs` & `src/ui/switcher_view.rs`)

### 6.1 Suppressing macOS Native `Cmd + \`` & Intercepting Globally
By default on macOS, `Cmd + \`` is bound to Symbolic HotKey `27` ("Move focus to next window of active application").
1. **Event Tap**: Install a `CGEventTap` at `kCGSessionEventTap` (`kCGHeadInsertEventTap`) listening for:
   - `CGEventType::KeyDown`
   - `CGEventType::KeyUp`
   - `CGEventType::FlagsChanged`
2. **Handling `Cmd + \`` (Keycode `50` = Grave/Backtick with `kCGEventFlagMaskCommand`)**:
   - When `KeyDown` for keycode `50` arrives with `Command` held (and without `Control`/`Option`):
     - Return `None` (absorbs the event so neither macOS window cycling nor the frontmost app sees `Cmd + \``).
     - Check `Shift` flag (`kCGEventFlagMaskShift`) to determine direction (`Forward` vs `Backward`).
     - Send `HotkeyEvent::CycleNext` (or `CyclePrev`) over a lock-free channel (`crossbeam_channel` / `flume`) to the GPUI main thread.
3. **Handling `Cmd` Release (`FlagsChanged`)**:
   - Track whether `kCGEventFlagMaskCommand` transitions from pressed -> released.
   - When released while the switcher is in `Mode::Cycling`, send `HotkeyEvent::ModifierReleased`.
4. **Tap Timeout Recovery**:
   - If `CGEventType::TapDisabledByTimeout` or `TapDisabledByUserInput` fires, immediately call `CGEventTapEnable(tap, true)` inside the callback so the tap never silently dies.

### 6.2 Interaction State Machine

```
                  ┌──────────────────────────────────────┐
                  │               Hidden                 │
                  └──────────────────┬───────────────────┘
                                     │
                           Press Cmd + `
                                     │
                                     ▼
                  ┌──────────────────────────────────────┐
         ┌───────►│             Mode::Cycling            │◄──────┐
         │        │  (Selected index = 1: previous tab)  │       │
         │        └──┬───────────────┬────────────────┬──┘       │
         │           │               │                │          │
   Press ` / Shift+` │         Release Cmd      Type printable   │
   (while holding    │               │          char / Backspace │
        Cmd)         │               │                │          │
         │           │               ▼                ▼          │
         └───────────┘    ┌──────────────────┐  ┌─────────────────────────┐
                          │ Commit & Activate│  │   Mode::LatchedSearch   │
                          │   Selected Tab   │  │ (Releasing Cmd is no-op;│
                          └──────────────────┘  │  Up/Down/Tab navigate)  │
                                    ▲           └─────┬──────────────┬────┘
                                    │                 │              │
                                    └─── Press Enter ─┘        Press Esc
                                                                     │
                                                                     ▼
                                                           ┌──────────────────┐
                                                           │ Cancel & Hide UI │
                                                           └──────────────────┘
```

#### Edge Cases Handled by the State Machine
- **First highlight index on summon**:
  - If **Chrome is currently the frontmost app** on macOS, index `0` is the tab you are already looking at, so `Cmd + \`` highlights **index `1`** (the previously active tab) for instant toggle between two tabs.
  - If **another app (e.g., VS Code or Terminal) is frontmost**, you usually want either the tab you were just looking at in Chrome (index `0`) or the second one (tapping `` ` `` again moves to `1`). Configurable via a single boolean `start_at_first_tab_when_chrome_inactive = true`.
- **Typing while `Cmd` is still held**:
  - In `Mode::Cycling`, if the user presses any alphanumeric key (e.g., holds `Cmd`, presses `` ` `` to summon, and either releases `Cmd` quickly as they start typing or types a letter while transitioning), any printable character transitions to `Mode::LatchedSearch`, appends that character to the search query, and re-ranks via `nucleo`.
  - To make typing seamless when transitioning from `Cmd + \``, if `Mode::Cycling` is active and the user presses a letter key even while `Cmd` hasn't fully released yet (excluding `Cmd+Q`, `Cmd+W`), treat that letter key as the first search character and latch into `Mode::LatchedSearch`!
- **`Esc` during `Mode::Cycling` (while `Cmd` is held)**:
  - Cancels the session and returns to `Hidden`; the subsequent `Cmd` release is ignored.

---

## 7. Zero-Latency Window Management & Focus Tricks on macOS (`src/macos.rs`)

Standard AppKit windows cause a ~30–100ms frame delay on first show or steal focus/menu-bar state from the active app causing visual flicker. To make summoning instantaneous:

1. **Accessory Activation Policy**:
   - Call `NSApp.setActivationPolicy(NSApplicationActivationPolicyAccessory)` at startup so the daemon has no Dock icon and does not overwrite the active app's menu bar.
2. **Pre-warmed Non-Activating Panel**:
   - Create the GPUI window once at startup and keep it alive for the lifetime of the process.
   - Configure the underlying `NSWindow` / `NSPanel`:
     - `styleMask`: include `NSWindowStyleMaskNonActivatingPanel` (`1 << 7`) so showing the window doesn't deactivate the current app until needed.
     - `level`: `NSPopUpMenuWindowLevel` (or `NSFloatingWindowLevel` + 1) so it floats above full-screen and normal windows.
     - `collectionBehavior`: `NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary | NSWindowCollectionBehaviorIgnoresCycle`.
     - `isOpaque = false`, `backgroundColor = clear`, `hasShadow = true`.
3. **Instant Show / Hide without Destroying GPU Context**:
   - **Never close or deallocate the GPUI window on dismiss.**
   - To **hide**: set `window.setAlphaValue(0.0)` and `window.orderOut(nil)`.
   - To **show**: update the GPUI model synchronously, position on the screen containing the mouse cursor (`NSScreen.screens` + `NSEvent.mouseLocation`), set `window.setAlphaValue(1.0)`, and call `window.orderFrontRegardless()`.
   - When switching to `Mode::LatchedSearch` (or while open), capture keyboard events either directly via the active `CGEventTap` (forwarding keystrokes while the switcher is visible so the underlying app never receives them) or by making the panel key. Forwarding via the `CGEventTap` while `SwitcherState != Hidden` guarantees **zero** macOS text-input-context IPC stalls!

---

## 8. Fuzzy Search & Ranking (`src/store.rs`)

Use the `nucleo-matcher` crate (the same SIMD-accelerated matcher used by Zed and Helix):

### 8.1 MRU Baseline Ordering (When Query is Empty)
- Every tab has an `effective_last_accessed: f64` initialized from Chrome's `tab.lastAccessed` and updated locally whenever `TabActivated` arrives.
- Tabs are sorted by `effective_last_accessed` descending.
- Tabs that have never been accessed (`lastAccessed == 0`) sort after accessed tabs by `(window_id, index)`.

### 8.2 Fuzzy Search Scoring (When Query is Non-Empty)
- Match the query against both `tab.title` and `tab.display_url` (stripped of `https://` and `www.`).
- Compute:
  - `title_score` (weighted `1.5x`) + `title_match_indices: Vec<u32>`
  - `url_score` (weighted `1.0x`) + `url_match_indices: Vec<u32>`
  - Small recency tie-breaker bonus (`+0..15` points based on MRU rank) so recently used tabs win ties.
- With < 500 open tabs, `nucleo-matcher` completes a full scan in **< 50 microseconds** (`0.05ms`), so filtering happens synchronously on every keystroke with zero debounce.

---

## 9. UI Design Specification (`src/ui/`)

### 9.1 Dimensions & Theme
- **Width**: `680px`, centered horizontally on the active screen, positioned at `22%` from the top of the screen (Spotlight/Raycast eye-level).
- **Max Visible Rows**: `12` rows (`44px` height per row -> max list height `528px`).
- **Header Search Bar**: `48px` height.
  - In `Mode::Cycling`: shows subtle placeholder `"Type to search across N Chrome tabs… (Release ⌘ to switch)"`.
  - In `Mode::LatchedSearch`: shows active query text + blinking cursor + match count `"M / N tabs"` on the right, and hint `"↵ Switch · Esc Cancel"`.
- **Colors**: Dark translucent glass aesthetic (`#18181BE6` background, `1px` `#FFFFFF1A` border, `12px` corner radius, `#27272A` selected row background with `#3B82F6` left accent bar).

### 9.2 Row Layout (`44px` height)
```
┌────────────────────────────────────────────────────────────────────────────┐
│ ▌ [🌐]  Fix race condition in WebSocket handler · PR #421   🔊  📌  W1     │
│ ▌       github.com/org/repo/pull/421                                       │
└────────────────────────────────────────────────────────────────────────────┘
```
- **Left**: `16x16` Favicon (loaded from in-memory cache; falls back to a clean vector globe icon for `chrome://` or uncached URLs).
- **Center**:
  - Top line: **Tab Title** (`14px`, medium weight, single-line truncated with ellipsis, matched characters rendered in bold `#60A5FA`).
  - Bottom line: **Clean URL** (`11.5px`, muted gray `#A1A1AA`, e.g., `github.com/org/repo/pull/421`, matched characters highlighted).
- **Right Badges**:
  - `🔊` if `tab.audible`
  - `📌` if `tab.pinned`
  - Window/Profile pill (e.g., `W1`, `W2`, or `Work · W1` when multiple windows/profiles exist).

---

## 10. Step-by-Step Implementation Phases

### Phase 1: Chrome Extension & Wire Protocol
1. Create `extension/manifest.json` and `extension/background.js`.
2. Implement initial tab enumeration (`chrome.tabs.query({})`), event listeners (`onCreated`, `onUpdated`, `onRemoved`, `onActivated`, `windows.onFocusChanged`, `windows.onRemoved`), and WebSocket client with auto-reconnect and 20s keepalive ping.
3. Implement `activate_tab` handler in `background.js` using `chrome.tabs.update` + `chrome.windows.update`.

### Phase 2: Rust Daemon Core (`store`, `ws_server`, `protocol`)
1. Initialize Cargo project with dependencies:
   - `gpui` (from Zed repo or crates.io)
   - `tokio` + `tokio-tungstenite` + `futures-util`
   - `serde` + `serde_json`
   - `nucleo-matcher`
   - `core-foundation`, `core-graphics`, `objc2`, `objc2-app-kit`, `objc2-foundation`
   - `crossbeam-channel`
   - `reqwest` (for async favicon caching)
2. Build `TabStore` with unit tests verifying:
   - Snapshot ingestion & multi-profile isolation
   - Delta updates (`TabUpserted`, `TabRemoved`, `TabActivated`)
   - MRU ordering and `nucleo` fuzzy search with character match indices.
3. Spawn the Tokio WebSocket server on `127.0.0.1:17384` and verify live tab events arriving from Chrome in < 1ms.

### Phase 3: Global `CGEventTap` Input Engine (`hotkey.rs`)
1. Check `AXIsProcessTrustedWithOptions` at startup (prompting macOS Accessibility permission if not yet granted).
2. Spawn dedicated high-priority `CFRunLoop` thread with a `kCGSessionEventTap`.
3. Intercept `Cmd + \`` and `Cmd + Shift + \`` (keycode `50`) so macOS never triggers default window cycling.
4. When the switcher is visible (`AtomicBool` shared with the tap callback):
   - Absorb all keyboard events in the tap (`Up`, `Down`, `Enter`, `Esc`, `Backspace`, printable characters) and forward them directly to GPUI over the channel.
   - **Why this matters for zero latency**: Because the `CGEventTap` captures typing directly while the switcher overlay is visible, the overlay panel **never even needs to steal first-responder / `NSTextInputContext` from the active app**, completely eliminating the notorious macOS AppKit XPC main-thread stalls!

### Phase 4: GPUI Floating Overlay & Favicon Cache (`ui/`, `macos.rs`, `favicon.rs`)
1. Configure GPUI app in Accessory mode (`LSUIElement`) and create the pre-warmed floating panel.
2. Implement `SwitcherView` with `Mode::Hidden`, `Mode::Cycling`, and `Mode::LatchedSearch`.
3. Render the compact vertical list with highlighted match ranges, URL subtitle, and status badges.
4. Add background favicon loader that fetches `fav_icon_url` once per domain, decodes PNG/ICO into an in-memory cache, and triggers a soft GPUI repaint.
5. Wire commit action: send `DaemonToExtension::ActivateTab` over WebSocket + activate `com.google.Chrome` via `NSRunningApplication`.

---

## 11. Verification & Performance Checklist

- [ ] **Latency check**: Summoning with `Cmd + \`` renders the panel on the very next display refresh (< 2ms from key-down to GPUI frame).
- [ ] **Quick tap test**: Fast tap-and-release of `Cmd + \`` (< 60ms hold) reliably switches to the previous Chrome tab even if released before the user visually inspects the list.
- [ ] **Latch-on-type test**: Holding `Cmd + \``, pressing `g`, `h`, and releasing `Cmd` keeps the switcher open in `Mode::LatchedSearch` with `"gh"` in the search box.
- [ ] **Cross-app activation**: Pressing `Cmd + \`` while in Terminal or VS Code brings Google Chrome to the foreground, focuses the exact Chrome window (even on another monitor), and activates the target tab.
- [ ] **Coexistence with AltTab**: Confirm `Option + Tab` still triggers AltTab normally while `Cmd + \`` triggers `chrome-tab-switcher` with zero conflict.
