use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossbeam_channel::Sender;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{DaemonToExtension, ExtensionToDaemon};
use crate::store::TabStore;

pub const DEFAULT_WS_ADDR: &str = "127.0.0.1:17384";

#[derive(Debug, Clone)]
pub enum StoreEvent {
    Updated {
        fav_icon_targets: Vec<(Option<String>, String)>,
    },
}

type OutboundTx = mpsc::UnboundedSender<DaemonToExtension>;

#[derive(Default)]
struct ConnectionRegistry {
    by_profile: HashMap<String, (u64, OutboundTx)>,
    by_conn_id: HashMap<u64, OutboundTx>,
}

#[derive(Clone)]
pub struct WsBridgeHandle {
    registry: Arc<RwLock<ConnectionRegistry>>,
    store: Arc<RwLock<TabStore>>,
    event_tx: Option<Sender<StoreEvent>>,
}

impl WsBridgeHandle {
    pub fn activate_tab(&self, profile_id: &str, window_id: i64, tab_id: i64) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);

        // Optimistically update local MRU state immediately so rapid Cmd+` cycles stay accurate.
        {
            let mut store = self.store.write();
            store.apply(ExtensionToDaemon::TabActivated {
                profile_id: profile_id.to_string(),
                window_id,
                tab_id,
                timestamp_ms: now_ms,
            });
        }
        if let Some(tx) = &self.event_tx {
            let _ = tx.try_send(StoreEvent::Updated {
                fav_icon_targets: Vec::new(),
            });
        }

        let cmd = DaemonToExtension::ActivateTab { window_id, tab_id };
        let reg = self.registry.read();

        if let Some((_, tx)) = reg.by_profile.get(profile_id) {
            if tx.send(cmd.clone()).is_ok() {
                return true;
            }
        }

        let mut sent_any = false;
        for tx in reg.by_conn_id.values() {
            if tx.send(cmd.clone()).is_ok() {
                sent_any = true;
            }
        }
        sent_any
    }

    /// Open `url` in a new foreground tab in `profile_id`'s Chrome (or any connected profile).
    pub fn open_url(&self, profile_id: Option<&str>, url: &str) -> bool {
        self.send_to_one_connection(
            profile_id,
            DaemonToExtension::OpenUrl {
                url: url.to_string(),
            },
        )
    }

    /// Search `text` with Chrome's default search engine in a new tab of `profile_id`'s Chrome.
    pub fn search_web(&self, profile_id: Option<&str>, text: &str) -> bool {
        self.send_to_one_connection(
            profile_id,
            DaemonToExtension::SearchWeb {
                text: text.to_string(),
            },
        )
    }

    /// Ask every connected profile to re-capture its focused window's visible tab. Sent on
    /// summon so thumbnails show pages as the user left them. Broadcast is correct here: each
    /// profile's front tab is a different page, and a capture is cheap and idempotent.
    pub fn request_preview_capture(&self) -> bool {
        let reg = self.registry.read();
        let mut sent_any = false;
        for tx in reg.by_conn_id.values() {
            if tx.send(DaemonToExtension::CapturePreview).is_ok() {
                sent_any = true;
            }
        }
        sent_any
    }

    /// Deliver `cmd` to exactly one extension connection — the given profile's if it is
    /// connected, otherwise any live one. Unlike `activate_tab`, this must never broadcast:
    /// opening a tab in every connected profile at once would be the wrong result.
    fn send_to_one_connection(&self, profile_id: Option<&str>, cmd: DaemonToExtension) -> bool {
        let reg = self.registry.read();
        if let Some((_, tx)) = profile_id.and_then(|pid| reg.by_profile.get(pid))
            && tx.send(cmd.clone()).is_ok()
        {
            return true;
        }
        reg.by_conn_id
            .values()
            .any(|tx| tx.send(cmd.clone()).is_ok())
    }
}

pub fn spawn_ws_server_thread(
    bind_addr: String,
    store: Arc<RwLock<TabStore>>,
    event_tx: Option<Sender<StoreEvent>>,
) -> WsBridgeHandle {
    let registry = Arc::new(RwLock::new(ConnectionRegistry::default()));
    let handle = WsBridgeHandle {
        registry: registry.clone(),
        store: store.clone(),
        event_tx: event_tx.clone(),
    };

    std::thread::Builder::new()
        .name("ws-server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for ws_server");

            rt.block_on(async move {
                let mut bound_listener = None;
                for attempt in 0..60 {
                    match TcpListener::bind(&bind_addr).await {
                        Ok(l) => {
                            bound_listener = Some(l);
                            break;
                        }
                        Err(err) if attempt < 59 => {
                            log::debug!(
                                "Waiting for {bind_addr} to become available (attempt {}): {err}",
                                attempt + 1
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(err) => {
                            log::error!("Failed to bind WebSocket server on {bind_addr}: {err}");
                        }
                    }
                }
                if let Some(listener) = bound_listener {
                    let _ = run_accept_loop(listener, registry, store, event_tx).await;
                }
            });
        })
        .expect("failed to spawn ws-server thread");

    handle
}

pub async fn start_ws_server(
    bind_addr: &str,
    store: Arc<RwLock<TabStore>>,
    event_tx: Option<Sender<StoreEvent>>,
) -> Result<(SocketAddr, WsBridgeHandle)> {
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let registry = Arc::new(RwLock::new(ConnectionRegistry::default()));
    let handle = WsBridgeHandle {
        registry: registry.clone(),
        store: store.clone(),
        event_tx: event_tx.clone(),
    };

    tokio::spawn(async move {
        let _ = run_accept_loop(listener, registry, store, event_tx).await;
    });

    Ok((local_addr, handle))
}

async fn run_accept_loop(
    listener: TcpListener,
    registry: Arc<RwLock<ConnectionRegistry>>,
    store: Arc<RwLock<TabStore>>,
    event_tx: Option<Sender<StoreEvent>>,
) -> Result<()> {
    static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

    loop {
        let (stream, _peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
        let registry_clone = registry.clone();
        let store_clone = store.clone();
        let event_tx_clone = event_tx.clone();

        tokio::spawn(async move {
            handle_connection(
                conn_id,
                stream,
                registry_clone,
                store_clone,
                event_tx_clone,
            )
            .await;
        });
    }
}

async fn handle_connection(
    conn_id: u64,
    stream: TcpStream,
    registry: Arc<RwLock<ConnectionRegistry>>,
    store: Arc<RwLock<TabStore>>,
    event_tx: Option<Sender<StoreEvent>>,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(err) => {
            log::debug!("WebSocket handshake error on conn {conn_id}: {err}");
            return;
        }
    };

    let (mut ws_sink, mut ws_source) = ws_stream.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<DaemonToExtension>();

    registry.write().by_conn_id.insert(conn_id, outbound_tx.clone());

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if ws_sink.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    while let Some(Ok(message)) = ws_source.next().await {
        let text = match message {
            Message::Text(t) => t,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
            _ => continue,
        };

        let parsed: ExtensionToDaemon = match serde_json::from_str(text.as_str()) {
            Ok(m) => m,
            Err(err) => {
                log::debug!("Invalid message from extension on conn {conn_id}: {err}");
                continue;
            }
        };

        if matches!(parsed, ExtensionToDaemon::Ping) {
            let _ = outbound_tx.send(DaemonToExtension::Pong);
            continue;
        }

        if let ExtensionToDaemon::PreviewCaptureFailed {
            tab_id,
            window_id,
            url,
            attempt,
            permanent,
            will_retry,
            error,
            ..
        } = &parsed
        {
            let target = if url.is_empty() {
                format!("tab {tab_id} (window {window_id})")
            } else {
                format!("{url} (tab {tab_id}, window {window_id})")
            };
            if *permanent {
                log::warn!(
                    "No thumbnail possible for {target}: {error} — Chrome blocks captureVisibleTab \
                     here (enterprise ExtensionSettings policy or a chrome:// page); the card will \
                     keep its favicon placeholder."
                );
            } else if *will_retry {
                log::debug!(
                    "captureVisibleTab failed for {target} (attempt {attempt}, retrying): {error}"
                );
            } else {
                log::warn!(
                    "captureVisibleTab gave up on {target} after {attempt} attempts: {error}"
                );
            }
            continue;
        }

        if let Some(profile_id) = extract_profile_id(&parsed) {
            registry
                .write()
                .by_profile
                .insert(profile_id.to_string(), (conn_id, outbound_tx.clone()));
        }

        let fav_icon_targets = extract_favicon_targets(&parsed);
        {
            let mut guard = store.write();
            guard.apply(parsed);
        }

        if let Some(tx) = &event_tx {
            let _ = tx.send(StoreEvent::Updated { fav_icon_targets });
        }
    }

    writer_task.abort();
    let mut reg = registry.write();
    reg.by_conn_id.remove(&conn_id);
    reg.by_profile.retain(|_, (cid, _)| *cid != conn_id);
}

fn extract_profile_id(msg: &ExtensionToDaemon) -> Option<&str> {
    match msg {
        ExtensionToDaemon::Snapshot { profile_id, .. }
        | ExtensionToDaemon::TabUpserted { profile_id, .. }
        | ExtensionToDaemon::TabRemoved { profile_id, .. }
        | ExtensionToDaemon::TabActivated { profile_id, .. }
        | ExtensionToDaemon::TabPreviewUpdated { profile_id, .. }
        | ExtensionToDaemon::PreviewCaptureFailed { profile_id, .. }
        | ExtensionToDaemon::WindowRemoved { profile_id, .. } => Some(profile_id.as_str()),
        ExtensionToDaemon::Ping => None,
    }
}

fn extract_favicon_targets(msg: &ExtensionToDaemon) -> Vec<(Option<String>, String)> {
    match msg {
        ExtensionToDaemon::Snapshot { tabs, .. } => tabs
            .iter()
            .map(|t| (t.fav_icon_url.clone(), t.url.clone()))
            .collect(),
        ExtensionToDaemon::TabUpserted { tab, .. } => {
            vec![(tab.fav_icon_url.clone(), tab.url.clone())]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WireTab;

    #[tokio::test]
    async fn ws_server_ingests_snapshot_replies_pong_and_dispatches_activate_tab() {
        let store = Arc::new(RwLock::new(TabStore::new()));
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let (addr, handle) = start_ws_server("127.0.0.1:0", store.clone(), Some(event_tx))
            .await
            .expect("server should bind");

        let url = format!("ws://{addr}");
        let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("client should connect");

        // 1. Send Snapshot
        let snapshot = ExtensionToDaemon::Snapshot {
            profile_id: "prof-test".into(),
            profile_name: "TestProfile".into(),
            tabs: vec![
                WireTab {
                    tab_id: 101,
                    window_id: 10,
                    index: 0,
                    title: "Tab One".into(),
                    url: "https://one.example.com".into(),
                    fav_icon_url: Some("https://one.example.com/favicon.ico".into()),
                    preview_url: None,
                    active: true,
                    pinned: false,
                    audible: false,
                    muted: false,
                    last_accessed: Some(1000.0),
                },
                WireTab {
                    tab_id: 102,
                    window_id: 10,
                    index: 1,
                    title: "Tab Two".into(),
                    url: "https://two.example.com".into(),
                    fav_icon_url: None,
                    preview_url: None,
                    active: false,
                    pinned: false,
                    audible: false,
                    muted: false,
                    last_accessed: Some(900.0),
                },
            ],
        };
        ws_stream
            .send(Message::Text(
                serde_json::to_string(&snapshot).unwrap().into(),
            ))
            .await
            .unwrap();

        // Wait for StoreEvent::Updated
        let evt = tokio::task::spawn_blocking(move || {
            event_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("should receive store update event")
        })
        .await
        .unwrap();
        match evt {
            StoreEvent::Updated { fav_icon_targets } => {
                assert_eq!(fav_icon_targets.len(), 2);
                assert_eq!(
                    fav_icon_targets[0].0,
                    Some("https://one.example.com/favicon.ico".to_string())
                );
            }
        }
        assert_eq!(store.read().len(), 2);

        // 2. Send Ping -> Expect Pong
        ws_stream
            .send(Message::Text(
                serde_json::to_string(&ExtensionToDaemon::Ping)
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();

        let reply = ws_stream.next().await.unwrap().unwrap();
        let parsed_reply: DaemonToExtension =
            serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(parsed_reply, DaemonToExtension::Pong);

        // 3. Dispatch activate_tab from handle -> Expect ActivateTab over WS and optimistic MRU update
        assert!(handle.activate_tab("prof-test", 10, 102));

        let activate_msg = ws_stream.next().await.unwrap().unwrap();
        let parsed_activate: DaemonToExtension =
            serde_json::from_str(activate_msg.to_text().unwrap()).unwrap();
        assert_eq!(
            parsed_activate,
            DaemonToExtension::ActivateTab {
                window_id: 10,
                tab_id: 102
            }
        );

        // Verify tab 102 is now rank 0 in store
        let ranked = store.write().query("");
        assert_eq!(ranked[0].entry.key.tab_id, 102);

        // 4. No-match fallbacks: open_url targets the profile's connection; search_web with an
        //    unknown profile still reaches the (single) live connection instead of being dropped.
        assert!(handle.open_url(Some("prof-test"), "https://github.com/org/repo"));
        let open_msg = ws_stream.next().await.unwrap().unwrap();
        let parsed_open: DaemonToExtension =
            serde_json::from_str(open_msg.to_text().unwrap()).unwrap();
        assert_eq!(
            parsed_open,
            DaemonToExtension::OpenUrl {
                url: "https://github.com/org/repo".into()
            }
        );

        assert!(handle.search_web(None, "rust gpui docs"));
        let search_msg = ws_stream.next().await.unwrap().unwrap();
        let parsed_search: DaemonToExtension =
            serde_json::from_str(search_msg.to_text().unwrap()).unwrap();
        assert_eq!(
            parsed_search,
            DaemonToExtension::SearchWeb {
                text: "rust gpui docs".into()
            }
        );

        // 5. Summon -> every connection is asked to re-capture its visible tab
        assert!(handle.request_preview_capture());
        let capture_msg = ws_stream.next().await.unwrap().unwrap();
        let parsed_capture: DaemonToExtension =
            serde_json::from_str(capture_msg.to_text().unwrap()).unwrap();
        assert_eq!(parsed_capture, DaemonToExtension::CapturePreview);
    }
}
