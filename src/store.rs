use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::protocol::{ExtensionToDaemon, WireTab};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabKey {
    pub profile_id: String,
    pub tab_id: i64,
}

#[derive(Debug, Clone)]
pub struct TabEntry {
    pub key: TabKey,
    pub profile_name: String,
    pub window_id: i64,
    pub index: i32,
    pub title: String,
    pub url: String,
    pub display_url: String,
    pub fav_icon_url: Option<String>,
    pub preview_url: Option<String>,
    pub active: bool,
    pub pinned: bool,
    pub audible: bool,
    pub muted: bool,
    pub effective_last_accessed: f64,
}

#[derive(Debug, Clone)]
pub struct ScoredTab {
    pub entry: TabEntry,
    pub badge_label: String,
    pub score: u32,
    pub title_match_indices: Vec<u32>,
    pub url_match_indices: Vec<u32>,
    pub title_highlight_ranges: Vec<Range<usize>>,
    pub url_highlight_ranges: Vec<Range<usize>>,
}

pub struct TabStore {
    tabs: HashMap<TabKey, TabEntry>,
    profile_names: HashMap<String, String>,
    matcher: Matcher,
}

impl Default for TabStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TabStore {
    pub fn new() -> Self {
        Self {
            tabs: HashMap::new(),
            profile_names: HashMap::new(),
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn apply(&mut self, msg: ExtensionToDaemon) {
        match msg {
            ExtensionToDaemon::Snapshot {
                profile_id,
                profile_name,
                tabs,
            } => {
                self.profile_names
                    .insert(profile_id.clone(), profile_name.clone());

                let mut previous_state: HashMap<i64, (f64, Option<String>)> = HashMap::new();
                self.tabs.retain(|k, v| {
                    if k.profile_id == profile_id {
                        previous_state.insert(
                            k.tab_id,
                            (v.effective_last_accessed, v.preview_url.clone()),
                        );
                        false
                    } else {
                        true
                    }
                });

                for mut wire in tabs {
                    let (prev_ts, prev_preview) = previous_state
                        .remove(&wire.tab_id)
                        .unwrap_or((0.0, None));
                    let wire_ts = wire.last_accessed.unwrap_or(0.0);
                    let effective_last_accessed = prev_ts.max(wire_ts);
                    if wire.preview_url.is_none() {
                        wire.preview_url = prev_preview;
                    }
                    let entry = Self::entry_from_wire(
                        &profile_id,
                        &profile_name,
                        wire,
                        effective_last_accessed,
                    );
                    self.tabs.insert(entry.key.clone(), entry);
                }
            }
            ExtensionToDaemon::TabUpserted {
                profile_id,
                mut tab,
            } => {
                let profile_name = self
                    .profile_names
                    .entry(profile_id.clone())
                    .or_insert_with(|| format!("Profile {}", &profile_id[..profile_id.len().min(4)]))
                    .clone();
                let key = TabKey {
                    profile_id: profile_id.clone(),
                    tab_id: tab.tab_id,
                };
                let (prev_ts, prev_preview) = self
                    .tabs
                    .get(&key)
                    .map(|e| (e.effective_last_accessed, e.preview_url.clone()))
                    .unwrap_or((0.0, None));
                let wire_ts = tab.last_accessed.unwrap_or(0.0);
                let effective_last_accessed = prev_ts.max(wire_ts);
                if tab.preview_url.is_none() {
                    tab.preview_url = prev_preview;
                }
                let entry = Self::entry_from_wire(
                    &profile_id,
                    &profile_name,
                    tab,
                    effective_last_accessed,
                );
                self.tabs.insert(key, entry);
            }
            ExtensionToDaemon::TabRemoved { profile_id, tab_id } => {
                self.tabs.remove(&TabKey { profile_id, tab_id });
            }
            ExtensionToDaemon::TabActivated {
                profile_id,
                window_id,
                tab_id,
                timestamp_ms,
            } => {
                for entry in self.tabs.values_mut() {
                    if entry.key.profile_id == profile_id && entry.window_id == window_id {
                        if entry.key.tab_id == tab_id {
                            entry.active = true;
                            entry.effective_last_accessed =
                                entry.effective_last_accessed.max(timestamp_ms);
                        } else {
                            entry.active = false;
                        }
                    }
                }
            }
            ExtensionToDaemon::TabPreviewUpdated {
                profile_id,
                tab_id,
                preview_url,
            } => {
                if let Some(entry) = self.tabs.get_mut(&TabKey { profile_id, tab_id }) {
                    let cleaned = preview_url.trim().to_string();
                    if !cleaned.is_empty() {
                        entry.preview_url = Some(cleaned);
                    }
                }
            }
            ExtensionToDaemon::WindowRemoved {
                profile_id,
                window_id,
            } => {
                self.tabs
                    .retain(|k, v| !(k.profile_id == profile_id && v.window_id == window_id));
            }
            ExtensionToDaemon::Ping => {}
        }
    }

    fn entry_from_wire(
        profile_id: &str,
        profile_name: &str,
        wire: WireTab,
        effective_last_accessed: f64,
    ) -> TabEntry {
        let display_url = clean_display_url(&wire.url);
        TabEntry {
            key: TabKey {
                profile_id: profile_id.to_string(),
                tab_id: wire.tab_id,
            },
            profile_name: profile_name.to_string(),
            window_id: wire.window_id,
            index: wire.index,
            title: wire.title,
            url: wire.url,
            display_url,
            fav_icon_url: wire.fav_icon_url.filter(|s| !s.is_empty()),
            preview_url: wire.preview_url.filter(|s| !s.is_empty()),
            active: wire.active,
            pinned: wire.pinned,
            audible: wire.audible,
            muted: wire.muted,
            effective_last_accessed,
        }
    }

    fn compute_window_badges(&self) -> HashMap<(String, i64), String> {
        let mut windows_by_profile: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for entry in self.tabs.values() {
            windows_by_profile
                .entry(entry.key.profile_id.clone())
                .or_default()
                .insert(entry.window_id);
        }

        let multi_profile = windows_by_profile.len() > 1;
        let total_windows: usize = windows_by_profile.values().map(|s| s.len()).sum();
        if !multi_profile && total_windows <= 1 {
            return HashMap::new();
        }

        let mut badges = HashMap::new();

        for (profile_id, window_ids) in windows_by_profile {
            let profile_name = self
                .profile_names
                .get(&profile_id)
                .cloned()
                .unwrap_or_else(|| profile_id.clone());
            for (idx, window_id) in window_ids.into_iter().enumerate() {
                let win_num = idx + 1;
                let label = if multi_profile {
                    format!("{profile_name} · W{win_num}")
                } else {
                    format!("W{win_num}")
                };
                badges.insert((profile_id.clone(), window_id), label);
            }
        }

        badges
    }

    fn mru_sorted_entries(&self) -> Vec<TabEntry> {
        let mut entries: Vec<TabEntry> = self.tabs.values().cloned().collect();
        entries.sort_by(compare_mru);
        entries
    }

    pub fn query(&mut self, query_str: &str) -> Vec<ScoredTab> {
        let mru_entries = self.mru_sorted_entries();
        let badges = self.compute_window_badges();
        let trimmed = query_str.trim();

        if trimmed.is_empty() {
            return mru_entries
                .into_iter()
                .map(|entry| {
                    let badge_label = badges
                        .get(&(entry.key.profile_id.clone(), entry.window_id))
                        .cloned()
                        .unwrap_or_default();
                    ScoredTab {
                        entry,
                        badge_label,
                        score: 0,
                        title_match_indices: Vec::new(),
                        url_match_indices: Vec::new(),
                        title_highlight_ranges: Vec::new(),
                        url_highlight_ranges: Vec::new(),
                    }
                })
                .collect();
        }

        let pattern = Pattern::parse(trimmed, CaseMatching::Ignore, Normalization::Smart);
        let total = mru_entries.len().max(1);
        let mut title_buf = Vec::new();
        let mut url_buf = Vec::new();
        let mut scored = Vec::new();

        for (mru_rank, entry) in mru_entries.into_iter().enumerate() {
            let mut title_indices = Vec::new();
            let mut url_indices = Vec::new();

            let title_utf32 = Utf32Str::new(&entry.title, &mut title_buf);
            let title_score = pattern.indices(title_utf32, &mut self.matcher, &mut title_indices);

            let url_utf32 = Utf32Str::new(&entry.display_url, &mut url_buf);
            let url_score = pattern.indices(url_utf32, &mut self.matcher, &mut url_indices);

            if title_score.is_none() && url_score.is_none() {
                continue;
            }

            title_indices.sort_unstable();
            title_indices.dedup();
            url_indices.sort_unstable();
            url_indices.dedup();

            // Weight title matches 1.5x and URL matches 1.0x, plus 0..=15 recency tie-breaker bonus
            let weighted_title = title_score.map(|s| (s * 3) / 2).unwrap_or(0);
            let weighted_url = url_score.unwrap_or(0);
            let recency_bonus = ((total.saturating_sub(mru_rank)) * 15 / total) as u32;
            let total_score = weighted_title.max(weighted_url)
                + (weighted_title.min(weighted_url) / 4)
                + recency_bonus;

            let title_highlight_ranges =
                char_indices_to_byte_ranges(&entry.title, &title_indices);
            let url_highlight_ranges =
                char_indices_to_byte_ranges(&entry.display_url, &url_indices);

            let badge_label = badges
                .get(&(entry.key.profile_id.clone(), entry.window_id))
                .cloned()
                .unwrap_or_default();

            scored.push((
                total_score,
                mru_rank,
                ScoredTab {
                    entry,
                    badge_label,
                    score: total_score,
                    title_match_indices: title_indices,
                    url_match_indices: url_indices,
                    title_highlight_ranges,
                    url_highlight_ranges,
                },
            ));
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, item)| item).collect()
    }
}

fn compare_mru(a: &TabEntry, b: &TabEntry) -> Ordering {
    let a_accessed = a.effective_last_accessed > 0.0;
    let b_accessed = b.effective_last_accessed > 0.0;

    match (a_accessed, b_accessed) {
        (true, true) => b
            .effective_last_accessed
            .partial_cmp(&a.effective_last_accessed)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.key.profile_id.cmp(&b.key.profile_id))
            .then_with(|| a.window_id.cmp(&b.window_id))
            .then_with(|| a.index.cmp(&b.index))
            .then_with(|| a.key.tab_id.cmp(&b.key.tab_id)),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a
            .key
            .profile_id
            .cmp(&b.key.profile_id)
            .then_with(|| a.window_id.cmp(&b.window_id))
            .then_with(|| a.index.cmp(&b.index))
            .then_with(|| a.key.tab_id.cmp(&b.key.tab_id)),
    }
}

pub fn clean_display_url(url: &str) -> String {
    let trimmed = url.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let without_www = without_scheme
        .strip_prefix("www.")
        .unwrap_or(without_scheme);
    let cleaned = without_www.strip_suffix('/').unwrap_or(without_www);
    cleaned.to_string()
}

pub fn char_indices_to_byte_ranges(text: &str, char_indices: &[u32]) -> Vec<Range<usize>> {
    if char_indices.is_empty() || text.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut target_iter = char_indices.iter().copied().peekable();

    for (char_idx, (byte_idx, ch)) in text.char_indices().enumerate() {
        let char_idx_u32 = char_idx as u32;
        while let Some(&target) = target_iter.peek() {
            if target < char_idx_u32 {
                target_iter.next();
            } else {
                break;
            }
        }
        if let Some(&target) = target_iter.peek() {
            if target == char_idx_u32 {
                target_iter.next();
                let start = byte_idx;
                let end = byte_idx + ch.len_utf8();
                if let Some(last) = ranges.last_mut() {
                    if last.end == start {
                        last.end = end;
                        continue;
                    }
                }
                ranges.push(start..end);
            }
        } else {
            break;
        }
    }

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_wire_tab(
        tab_id: i64,
        window_id: i64,
        index: i32,
        title: &str,
        url: &str,
        last_accessed: Option<f64>,
    ) -> WireTab {
        WireTab {
            tab_id,
            window_id,
            index,
            title: title.to_string(),
            url: url.to_string(),
            fav_icon_url: None,
            preview_url: None,
            active: false,
            pinned: false,
            audible: false,
            muted: false,
            last_accessed,
        }
    }

    #[test]
    fn clean_display_url_strips_scheme_www_and_trailing_slash() {
        assert_eq!(
            clean_display_url("https://www.github.com/org/repo/pull/421"),
            "github.com/org/repo/pull/421"
        );
        assert_eq!(clean_display_url("http://example.com/"), "example.com");
        assert_eq!(clean_display_url("chrome://extensions/"), "chrome://extensions");
    }

    #[test]
    fn mru_ordering_sorts_accessed_tabs_descending_and_unaccessed_by_window_index() {
        let mut store = TabStore::new();
        store.apply(ExtensionToDaemon::Snapshot {
            profile_id: "p1".into(),
            profile_name: "Work".into(),
            tabs: vec![
                sample_wire_tab(1, 10, 0, "Older", "https://a.com", Some(1000.0)),
                sample_wire_tab(2, 10, 1, "Newest", "https://b.com", Some(3000.0)),
                sample_wire_tab(3, 10, 2, "Never Accessed B", "https://c.com", None),
                sample_wire_tab(4, 10, 0, "Never Accessed A", "https://d.com", Some(0.0)),
                sample_wire_tab(5, 10, 3, "Middle", "https://e.com", Some(2000.0)),
            ],
        });

        let results = store.query("");
        let ids: Vec<i64> = results.iter().map(|r| r.entry.key.tab_id).collect();
        assert_eq!(ids, vec![2, 5, 1, 4, 3]);
        // Single profile + single window (window 10) -> badge_label should be empty
        assert!(results.iter().all(|r| r.badge_label.is_empty()));
    }

    #[test]
    fn multi_profile_isolation_and_delta_updates() {
        let mut store = TabStore::new();
        store.apply(ExtensionToDaemon::Snapshot {
            profile_id: "p1".into(),
            profile_name: "Personal".into(),
            tabs: vec![sample_wire_tab(
                10,
                1,
                0,
                "Personal Mail",
                "https://mail.google.com",
                Some(100.0),
            )],
        });
        store.apply(ExtensionToDaemon::Snapshot {
            profile_id: "p2".into(),
            profile_name: "Work".into(),
            tabs: vec![sample_wire_tab(
                10,
                1,
                0,
                "Work Docs",
                "https://docs.google.com",
                Some(200.0),
            )],
        });

        assert_eq!(store.len(), 2);

        // Activate tab 10 in p1 at timestamp 500.0 -> should move to rank 0
        store.apply(ExtensionToDaemon::TabActivated {
            profile_id: "p1".into(),
            window_id: 1,
            tab_id: 10,
            timestamp_ms: 500.0,
        });

        let results = store.query("");
        assert_eq!(results[0].entry.key.profile_id, "p1");
        assert_eq!(results[0].entry.title, "Personal Mail");
        assert!(results[0].entry.active);
        assert!(results[0].badge_label.contains("Personal"));

        // Upsert new tab in p2
        store.apply(ExtensionToDaemon::TabUpserted {
            profile_id: "p2".into(),
            tab: sample_wire_tab(11, 2, 0, "PR #421", "https://github.com/pr/421", Some(600.0)),
        });
        assert_eq!(store.len(), 3);

        // Remove tab 10 from p1 only; p2's tab 10 must remain intact
        store.apply(ExtensionToDaemon::TabRemoved {
            profile_id: "p1".into(),
            tab_id: 10,
        });
        assert_eq!(store.len(), 2);
        let remaining_profiles: Vec<String> = store
            .query("")
            .into_iter()
            .map(|r| r.entry.key.profile_id)
            .collect();
        assert_eq!(remaining_profiles, vec!["p2".to_string(), "p2".to_string()]);

        // Remove window 1 in p2
        store.apply(ExtensionToDaemon::WindowRemoved {
            profile_id: "p2".into(),
            window_id: 1,
        });
        assert_eq!(store.len(), 1);
        assert_eq!(store.query("")[0].entry.key.tab_id, 11);
    }

    #[test]
    fn fuzzy_search_ranks_title_higher_and_produces_utf8_byte_highlight_ranges() {
        let mut store = TabStore::new();
        store.apply(ExtensionToDaemon::Snapshot {
            profile_id: "p1".into(),
            profile_name: "Default".into(),
            tabs: vec![
                sample_wire_tab(
                    1,
                    1,
                    0,
                    "Random Blog Post",
                    "https://example.com/websocket-guide",
                    Some(2000.0),
                ),
                sample_wire_tab(
                    2,
                    1,
                    1,
                    "Fix race condition in WebSocket handler · PR #421",
                    "https://github.com/org/repo/pull/421",
                    Some(1000.0),
                ),
            ],
        });

        let results = store.query("websocket");
        assert_eq!(results.len(), 2);
        // Title match (1.5x weight) beats URL-only match
        assert_eq!(results[0].entry.key.tab_id, 2);
        assert!(!results[0].title_match_indices.is_empty());
        assert!(!results[0].title_highlight_ranges.is_empty());

        // Verify byte ranges slice cleanly on UTF-8 boundaries (note '·' is multi-byte!)
        for range in &results[0].title_highlight_ranges {
            let sliced = &results[0].entry.title[range.clone()];
            assert!(!sliced.is_empty());
        }
    }
}
