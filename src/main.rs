use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use gpui::{App, Application};
use parking_lot::RwLock;

use chrome_tab_switcher::hotkey::{
    HotkeyEvent, ensure_accessibility_permission, spawn_hotkey_tap_thread,
};
use chrome_tab_switcher::macos;
use chrome_tab_switcher::store::TabStore;
use chrome_tab_switcher::ui::open_prewarmed_switcher_window;
use chrome_tab_switcher::ws_server::{DEFAULT_WS_ADDR, StoreEvent, spawn_ws_server_thread};

/// Watch our own executable on disk (`std::env::current_exe()`) and automatically respawn
/// when `cargo build` writes a newer binary, so the user never has to restart manually.
fn spawn_binary_hot_reload_watcher() {
    let Ok(exe_path) = std::env::current_exe() else {
        return;
    };
    let Ok(initial_meta) = std::fs::metadata(&exe_path) else {
        return;
    };
    let initial_ino = initial_meta.ino();
    let initial_mtime = initial_meta.mtime_nsec();
    let initial_size = initial_meta.len();

    std::thread::Builder::new()
        .name("binary-hot-reload".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let Ok(meta) = std::fs::metadata(&exe_path) else {
                    continue;
                };
                let changed = meta.ino() != initial_ino
                    || meta.mtime_nsec() != initial_mtime
                    || meta.len() != initial_size;

                if changed && meta.len() > 0 {
                    // Wait briefly to ensure Cargo has finished linking/renaming the binary
                    std::thread::sleep(Duration::from_millis(200));
                    log::info!(
                        "Detected updated binary at {} — hot-reloading chrome-tab-switcher...",
                        exe_path.display()
                    );
                    let args: Vec<String> = std::env::args().skip(1).collect();
                    match Command::new(&exe_path).args(&args).spawn() {
                        Ok(_) => std::process::exit(0),
                        Err(err) => {
                            log::error!("Failed to spawn updated binary: {err}");
                        }
                    }
                }
            }
        })
        .expect("failed to spawn binary-hot-reload thread");
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 0. Watch executable on disk for automatic hot-reload whenever `cargo build` updates it.
    spawn_binary_hot_reload_watcher();

    // 1. Check macOS Accessibility permission (prompts user on first run if needed).
    let trusted = ensure_accessibility_permission(true);
    if !trusted {
        log::warn!(
            "Accessibility permission not yet granted. Grant permission in System Settings -> Privacy & Security -> Accessibility."
        );
    }

    // 2. Initialize shared in-memory TabStore & channels.
    let store = Arc::new(RwLock::new(TabStore::new()));
    let switcher_visible = Arc::new(AtomicBool::new(false));
    let (hotkey_tx, hotkey_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    let (store_event_tx, store_event_rx) = crossbeam_channel::unbounded::<StoreEvent>();

    // 3. Start local WebSocket server on 127.0.0.1:17384 with TCP_NODELAY.
    let ws_addr =
        std::env::var("CHROME_TAB_SWITCHER_WS_ADDR").unwrap_or_else(|_| DEFAULT_WS_ADDR.into());
    let ws_handle = spawn_ws_server_thread(ws_addr.clone(), store.clone(), Some(store_event_tx));
    log::info!("chrome-tab-switcher WebSocket server listening on ws://{ws_addr}");

    // 4. Spawn dedicated high-priority CFRunLoop thread for global CGEventTap (Cmd + `).
    let hotkey_tx_menu = hotkey_tx.clone();
    spawn_hotkey_tap_thread(hotkey_tx, switcher_visible.clone());

    // 5. Launch GPUI in Accessory mode with a pre-warmed, non-activating floating panel and menu bar icon.
    Application::new().run(move |cx: &mut App| {
        macos::set_accessory_activation_policy();
        macos::install_status_bar_item(hotkey_tx_menu.clone());
        open_prewarmed_switcher_window(
            cx,
            store.clone(),
            ws_handle.clone(),
            switcher_visible.clone(),
            hotkey_rx.clone(),
            store_event_rx.clone(),
        );
    });
}
