use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionToDaemon {
    Snapshot {
        profile_id: String,
        profile_name: String,
        tabs: Vec<WireTab>,
    },
    TabUpserted {
        profile_id: String,
        tab: WireTab,
    },
    TabRemoved {
        profile_id: String,
        tab_id: i64,
    },
    TabActivated {
        profile_id: String,
        window_id: i64,
        tab_id: i64,
        timestamp_ms: f64,
    },
    TabPreviewUpdated {
        profile_id: String,
        tab_id: i64,
        preview_url: String,
    },
    WindowRemoved {
        profile_id: String,
        window_id: i64,
    },
    Ping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTab {
    pub tab_id: i64,
    pub window_id: i64,
    pub index: i32,
    pub title: String,
    pub url: String,
    pub fav_icon_url: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    pub active: bool,
    pub pinned: bool,
    pub audible: bool,
    pub muted: bool,
    pub last_accessed: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToExtension {
    ActivateTab {
        window_id: i64,
        tab_id: i64,
    },
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snapshot_message_with_snake_case_tag() {
        let raw = r#"{
            "type": "snapshot",
            "profile_id": "prof-1",
            "profile_name": "Default",
            "tabs": [
                {
                    "tab_id": 10,
                    "window_id": 1,
                    "index": 0,
                    "title": "GitHub",
                    "url": "https://github.com",
                    "fav_icon_url": "https://github.com/favicon.ico",
                    "active": true,
                    "pinned": false,
                    "audible": false,
                    "muted": false,
                    "last_accessed": 1700000000123.0
                }
            ]
        }"#;

        let parsed: ExtensionToDaemon = serde_json::from_str(raw).expect("valid snapshot JSON");
        match parsed {
            ExtensionToDaemon::Snapshot {
                profile_id,
                profile_name,
                tabs,
            } => {
                assert_eq!(profile_id, "prof-1");
                assert_eq!(profile_name, "Default");
                assert_eq!(tabs.len(), 1);
                assert_eq!(tabs[0].tab_id, 10);
                assert_eq!(tabs[0].title, "GitHub");
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn parses_delta_and_ping_messages() {
        let activated_json = r#"{
            "type": "tab_activated",
            "profile_id": "prof-1",
            "window_id": 2,
            "tab_id": 55,
            "timestamp_ms": 1700000009999.5
        }"#;
        let msg: ExtensionToDaemon = serde_json::from_str(activated_json).unwrap();
        assert_eq!(
            msg,
            ExtensionToDaemon::TabActivated {
                profile_id: "prof-1".into(),
                window_id: 2,
                tab_id: 55,
                timestamp_ms: 1700000009999.5,
            }
        );

        let ping_json = r#"{"type": "ping"}"#;
        let ping: ExtensionToDaemon = serde_json::from_str(ping_json).unwrap();
        assert_eq!(ping, ExtensionToDaemon::Ping);
    }

    #[test]
    fn serializes_daemon_to_extension_messages_with_type_tag() {
        let msg = DaemonToExtension::ActivateTab {
            window_id: 3,
            tab_id: 99,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "activate_tab",
                "window_id": 3,
                "tab_id": 99
            })
        );

        let pong = DaemonToExtension::Pong;
        let pong_json = serde_json::to_value(&pong).unwrap();
        assert_eq!(pong_json, serde_json::json!({ "type": "pong" }));
    }
}
