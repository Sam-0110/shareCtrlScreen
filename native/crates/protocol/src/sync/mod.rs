//! Account sync: the document every computer signed in to one account merges.
//!
//! The synced value is a last-writer-wins document:
//!   * each remote ID carries its name and when it was named (`nameAt`, ms
//!     since the Unix epoch; an empty name with a non-zero `nameAt` records
//!     that the name was removed), and when it was last connected to
//!     (`usedAt`);
//!   * "Clear" on the recents is one account-wide time, `recentsClearedAt`:
//!     an ID is a recent only when it was used after it;
//!   * v2: the account's open tabs ([`tabs`]) and the preferences that follow
//!     the account ([`prefs`]).
//!
//! Merging takes the newer value of each field, so it is commutative,
//! associative and idempotent: computers can merge in any order, any number of
//! times, and still agree. The server (`server/src/sync.js`) applies the same
//! rules, checked by the shared cases in `testdata/sync-vectors.json`, so a sync
//! is one round trip: send the local document, get the merged one back, merge
//! it in locally.
//!
//! Every stamp from outside (the network, the disk) is clamped to no later than
//! a few minutes into the future ([`clock`]), on both sides.

pub mod clock;
pub mod order;
pub mod placement;
pub mod prefs;
pub mod tabs;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::{clean_remote_name, remote_name_key};
pub use clock::{Stamper, FUTURE_SLACK_MS};
pub use prefs::{is_synced_pref, SyncPref, SYNCED_PREFS};
pub use tabs::{
    valid_order_key, Pos, Size, SyncTab, WinState, CLOSED_TAB_TTL_MS, FRACTION_ONE, MAX_MONITOR,
    MAX_SYNC_TABS,
};

/// The `v` of a [`SyncDoc`]: 2 adds `tabs` and `prefs`. Version 1 readers
/// ignore both; version 1 documents simply have none.
pub const SYNC_DOC_VERSION: u32 = 2;
/// Most IDs kept in one document; the server enforces the same cap.
pub const MAX_SYNC_ENTRIES: usize = 500;

/// What a server can store, from the `features` of its sync reply. A computer
/// sends tabs and preferences only to a server that advertised them, so an old
/// server never receives (and drops) them.
pub const FEATURE_PREFS: &str = "prefs";
pub const FEATURE_TABS: &str = "tabs";
/// Detached windows' placement in tabs (plans/09): a server that lists it
/// validates and merges those fields; a computer sends them only to such a
/// server ([`SyncDoc::strip_placement`]). A server without it would drop
/// them anyway; each computer keeps its own.
pub const FEATURE_PLACEMENT: &str = "placement";

/// One remote ID in a [`SyncDoc`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncEntry {
    /// The friendly name, or empty for none.
    pub name: String,
    /// When `name` was set or removed (ms since the epoch); 0 = never named.
    pub name_at: u64,
    /// When this ID was last connected to (ms since the epoch); 0 = never.
    pub used_at: u64,
}

impl SyncEntry {
    /// Fold `other` into `self`: the newer name (ties broken by the name
    /// itself, so both sides pick the same one) and the later use.
    pub fn merge(&mut self, other: &SyncEntry) {
        if (other.name_at, &other.name) > (self.name_at, &self.name) {
            self.name.clone_from(&other.name);
            self.name_at = other.name_at;
        }
        self.used_at = self.used_at.max(other.used_at);
    }

    fn last_touched(&self) -> u64 {
        self.name_at.max(self.used_at)
    }
}

/// The document an account stores and every signed-in computer merges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncDoc {
    pub v: u32,
    /// Remote ID (lower-case) → its entry.
    pub entries: BTreeMap<String, SyncEntry>,
    /// Recents used at or before this time are hidden (ms since the epoch).
    pub recents_cleared_at: u64,
    /// Remote ID (lower-case UUID) → its tab.
    pub tabs: BTreeMap<String, SyncTab>,
    /// Synced preference (a [`SYNCED_PREFS`] key) → its value.
    pub prefs: BTreeMap<String, SyncPref>,
}

impl Default for SyncDoc {
    fn default() -> Self {
        SyncDoc {
            v: SYNC_DOC_VERSION,
            entries: BTreeMap::new(),
            recents_cleared_at: 0,
            tabs: BTreeMap::new(),
            prefs: BTreeMap::new(),
        }
    }
}

impl SyncDoc {
    /// Fold `other` into `self` (see the module docs for the rules).
    /// Leave every tab's window out (plans/09 W4): a computer sends
    /// placement only to a server that lists [`FEATURE_PLACEMENT`].
    pub fn strip_placement(&mut self) {
        for tab in self.tabs.values_mut() {
            *tab = tab.without_placement();
        }
    }

    pub fn merge(&mut self, other: &SyncDoc) {
        for (id, theirs) in &other.entries {
            self.entries.entry(id.clone()).or_default().merge(theirs);
        }
        self.recents_cleared_at = self.recents_cleared_at.max(other.recents_cleared_at);
        for (id, theirs) in &other.tabs {
            match self.tabs.get_mut(id) {
                Some(mine) => mine.merge(theirs),
                None => {
                    self.tabs.insert(id.clone(), theirs.clone());
                }
            }
        }
        for (key, theirs) in &other.prefs {
            self.prefs.entry(key.clone()).or_default().merge(theirs);
        }
        self.prune();
    }

    /// Repair a document from an untrusted source (the network, the disk) in
    /// place, `now` being the current server-corrected time: lower-case and
    /// validate the IDs, clean the names the way local names are cleaned,
    /// merge IDs that differ only in case, clamp every stamp to at most
    /// `now + FUTURE_SLACK_MS`, drop tabs with unusable keys and tabs closed
    /// more than [`CLOSED_TAB_TTL_MS`] ago, drop preferences that do not
    /// sync, and apply the caps.
    pub fn normalize(&mut self, now: u64) {
        let max = now.saturating_add(FUTURE_SLACK_MS);
        self.v = SYNC_DOC_VERSION;

        let entries = std::mem::take(&mut self.entries);
        for (id, mut entry) in entries {
            // Remote IDs are UUIDs; the server keeps nothing else either.
            let Some(key) = remote_name_key(&id).filter(|k| crate::is_valid_uuid_v4(k)) else {
                continue;
            };
            entry.name = clean_remote_name(&entry.name).unwrap_or_default();
            entry.name_at = entry.name_at.min(max);
            entry.used_at = entry.used_at.min(max);
            self.entries.entry(key).or_default().merge(&entry);
        }
        self.recents_cleared_at = self.recents_cleared_at.min(max);

        let tabs = std::mem::take(&mut self.tabs);
        for (id, mut tab) in tabs {
            let id = id.trim().to_lowercase();
            if !crate::is_valid_uuid_v4(&id) || !valid_order_key(&tab.order) {
                continue;
            }
            tab.clamp_stamps(max);
            match self.tabs.get_mut(&id) {
                Some(mine) => mine.merge(&tab),
                None => {
                    self.tabs.insert(id, tab);
                }
            }
        }
        let forget_before = now.saturating_sub(CLOSED_TAB_TTL_MS);
        self.tabs
            .retain(|_, t| t.is_open() || t.closed_at >= forget_before);

        self.prefs.retain(|key, _| is_synced_pref(key));
        for pref in self.prefs.values_mut() {
            pref.at = pref.at.min(max);
        }

        self.prune();
    }

    /// IDs that count as recents, most recently used first.
    pub fn recents(&self) -> Vec<&str> {
        let mut used: Vec<(&str, u64)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.used_at > self.recents_cleared_at)
            .map(|(id, e)| (id.as_str(), e.used_at))
            .collect();
        used.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        used.into_iter().map(|(id, _)| id).collect()
    }

    /// Over a cap: keep named IDs first, then the most recently touched; keep
    /// open tabs first, then the most recently touched.
    fn prune(&mut self) {
        if self.entries.len() > MAX_SYNC_ENTRIES {
            let mut order: Vec<(String, bool, u64)> = self
                .entries
                .iter()
                .map(|(id, e)| (id.clone(), !e.name.is_empty(), e.last_touched()))
                .collect();
            order.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
            for (id, _, _) in order.into_iter().skip(MAX_SYNC_ENTRIES) {
                self.entries.remove(&id);
            }
        }
        if self.tabs.len() > MAX_SYNC_TABS {
            let mut order: Vec<(String, bool, u64)> = self
                .tabs
                .iter()
                .map(|(id, t)| (id.clone(), t.is_open(), t.last_touched()))
                .collect();
            order.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
            for (id, _, _) in order.into_iter().skip(MAX_SYNC_TABS) {
                self.tabs.remove(&id);
            }
        }
    }
}

/// Milliseconds since the Unix epoch, 0 if the clock is before it.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    fn entry(name: &str, name_at: u64, used_at: u64) -> SyncEntry {
        SyncEntry {
            name: name.into(),
            name_at,
            used_at,
        }
    }

    fn doc(entries: &[(&str, SyncEntry)], cleared: u64) -> SyncDoc {
        SyncDoc {
            entries: entries
                .iter()
                .map(|(k, e)| (k.to_string(), e.clone()))
                .collect(),
            recents_cleared_at: cleared,
            ..SyncDoc::default()
        }
    }

    #[test]
    fn newer_name_wins_and_uses_take_the_later_time() {
        let mut a = doc(&[("x", entry("Office", 10, 50))], 0);
        let b = doc(&[("x", entry("Work", 20, 30))], 0);
        a.merge(&b);
        assert_eq!(a.entries["x"], entry("Work", 20, 50));
    }

    #[test]
    fn a_newer_removal_beats_an_older_name() {
        let mut a = doc(&[("x", entry("Office", 10, 0))], 0);
        a.merge(&doc(&[("x", entry("", 20, 0))], 0));
        assert_eq!(a.entries["x"].name, "");
        // And the old name sent again later does not bring it back.
        a.merge(&doc(&[("x", entry("Office", 10, 0))], 0));
        assert_eq!(a.entries["x"].name, "");
    }

    #[test]
    fn merge_is_order_independent_and_idempotent() {
        let mut a = doc(&[("x", entry("A", 5, 1)), ("y", entry("", 0, 9))], 3);
        a.prefs.insert(
            "clipboardSync".into(),
            SyncPref {
                value: false,
                at: 4,
            },
        );
        a.tabs.insert(
            X.into(),
            SyncTab {
                order: "m".into(),
                order_at: 2,
                opened_at: 3,
                closed_at: 0,
                ..SyncTab::default()
            },
        );
        let mut b = doc(&[("x", entry("B", 5, 7)), ("z", entry("Z", 2, 0))], 8);
        b.prefs
            .insert("clipboardSync".into(), SyncPref { value: true, at: 4 });
        b.tabs.insert(
            X.into(),
            SyncTab {
                order: "n".into(),
                order_at: 1,
                opened_at: 0,
                closed_at: 5,
                ..SyncTab::default()
            },
        );
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ab, ba);
        // Same-time names: the tie goes to the same one on both sides.
        assert_eq!(ab.entries["x"].name, "B");
        assert!(ab.prefs["clipboardSync"].value);
        assert!(!ab.tabs[X].is_open());
        let mut again = ab.clone();
        again.merge(&a);
        again.merge(&b);
        assert_eq!(again, ab);
        assert_eq!(ab.recents_cleared_at, 8);
    }

    #[test]
    fn recents_are_ordered_and_respect_the_clear_time() {
        let d = doc(
            &[
                ("old", entry("", 0, 5)),
                ("new", entry("", 0, 30)),
                ("mid", entry("", 0, 20)),
                ("never", entry("N", 1, 0)),
            ],
            10,
        );
        assert_eq!(d.recents(), vec!["new", "mid"]);
    }

    #[test]
    fn normalize_cleans_untrusted_input() {
        let mut d: SyncDoc = serde_json::from_str(
            r#"{"v":7,"entries":{
                "0F8FAD5B-D9CB-469F-A165-70867728950E":{"name":"  Desk \n top ","nameAt":4,"usedAt":2},
                "0f8fad5b-d9cb-469f-a165-70867728950e":{"name":"older","nameAt":3,"usedAt":9},
                "not-a-uuid":{"name":"dropped","nameAt":5},
                "":{"name":"x","nameAt":1}
            },"recentsClearedAt":1,"extra":true,
            "prefs":{"clipboardSync":{"value":false,"at":3},"serverUrl":{"value":true,"at":9}},
            "tabs":{"0F8FAD5B-D9CB-469F-A165-70867728950E":{"order":"a","orderAt":1,"openedAt":2},
                    "not-a-uuid":{"order":"a","openedAt":2},
                    "7c9e6679-7425-40de-944b-e07fc1f90ae7":{"order":"A!","openedAt":2}}}"#,
        )
        .unwrap();
        d.normalize(NOW);
        assert_eq!(d.v, SYNC_DOC_VERSION);
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries[X], entry("Desk top", 4, 9));
        assert_eq!(d.prefs.len(), 1);
        assert!(!d.prefs["clipboardSync"].value);
        assert_eq!(d.tabs.len(), 1);
        assert!(d.tabs[X].is_open());
    }

    #[test]
    fn normalize_clamps_future_stamps_and_forgets_old_closed_tabs() {
        let far = NOW + 365 * 24 * 3600 * 1000;
        let mut d = doc(&[(X, entry("A", far, far))], far);
        d.prefs.insert(
            "captureShortcuts".into(),
            SyncPref {
                value: true,
                at: far,
            },
        );
        d.tabs.insert(
            X.into(),
            SyncTab {
                order: "a".into(),
                order_at: far,
                opened_at: 1,
                closed_at: far,
                ..SyncTab::default()
            },
        );
        let old_closed = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        d.tabs.insert(
            old_closed.into(),
            SyncTab {
                order: "b".into(),
                order_at: 1,
                opened_at: 1,
                closed_at: NOW - CLOSED_TAB_TTL_MS - 1,
                ..SyncTab::default()
            },
        );
        d.normalize(NOW);
        let max = NOW + FUTURE_SLACK_MS;
        assert_eq!(d.entries[X], entry("A", max, max));
        assert_eq!(d.recents_cleared_at, max);
        assert_eq!(d.prefs["captureShortcuts"].at, max);
        assert_eq!(d.tabs[X].closed_at, max);
        assert!(!d.tabs.contains_key(old_closed));
    }

    #[test]
    fn the_cap_keeps_named_ids_then_the_newest() {
        let mut d = SyncDoc::default();
        for i in 0..MAX_SYNC_ENTRIES as u64 + 10 {
            d.entries.insert(format!("u{i:04}"), entry("", 0, i + 1));
        }
        d.entries.insert("named".into(), entry("Keep", 1, 0));
        d.prune();
        assert_eq!(d.entries.len(), MAX_SYNC_ENTRIES);
        assert!(d.entries.contains_key("named"));
        assert!(!d.entries.contains_key("u0000"));
        assert!(d
            .entries
            .contains_key(&format!("u{:04}", MAX_SYNC_ENTRIES + 9)));
    }

    /// The merge cases shared with the server (`server/src/sync.js`, run by
    /// `server/test/sync-vectors.mjs`): both sides must produce exactly the
    /// same document.
    #[test]
    fn the_shared_merge_cases_hold() {
        #[derive(Deserialize)]
        struct Vectors {
            now: u64,
            cases: Vec<Case>,
        }
        #[derive(Deserialize)]
        struct Case {
            name: String,
            a: serde_json::Value,
            b: serde_json::Value,
            merged: serde_json::Value,
        }
        let v: Vectors =
            serde_json::from_str(include_str!("../../testdata/sync-vectors.json")).unwrap();
        assert!(!v.cases.is_empty());
        for c in v.cases {
            let mut a: SyncDoc = serde_json::from_value(c.a).unwrap();
            let mut b: SyncDoc = serde_json::from_value(c.b).unwrap();
            a.normalize(v.now);
            b.normalize(v.now);
            let mut ab = a.clone();
            ab.merge(&b);
            let mut ba = b.clone();
            ba.merge(&a);
            assert_eq!(
                serde_json::to_value(&ab).unwrap(),
                c.merged,
                "{}: a then b",
                c.name
            );
            assert_eq!(ab, ba, "{}: the order of merging matters", c.name);
        }
    }

    /// plans/09 W4: what a server without placement receives — the tab,
    /// none of its window — and that merging its answer back loses nothing
    /// here.
    #[test]
    fn placement_is_left_out_for_a_server_that_does_not_store_it() {
        let mut mine = SyncDoc::default();
        let mut tab = SyncTab {
            order: "m".into(),
            order_at: 1,
            opened_at: 2,
            detached: true,
            detached_at: 3,
            size: Some(Size { w: 5000, h: 5000 }),
            size_at: 3,
            ..SyncTab::default()
        };
        mine.tabs.insert(X.into(), tab.clone());
        let mut sent = mine.clone();
        sent.strip_placement();
        assert_eq!(
            serde_json::to_value(&sent.tabs[X]).unwrap(),
            serde_json::json!({"order":"m","orderAt":1,"openedAt":2,"closedAt":0})
        );
        // The old server answers with what it stored; merged here, the
        // window survives.
        mine.merge(&sent);
        tab.merge(&sent.tabs[X]);
        assert_eq!(mine.tabs[X], tab);
        assert!(mine.tabs[X].is_detached());
    }

    #[test]
    fn a_version_1_document_still_reads() {
        let d: SyncDoc =
            serde_json::from_str(r#"{"v":1,"entries":{},"recentsClearedAt":0}"#).unwrap();
        assert!(d.tabs.is_empty() && d.prefs.is_empty());
    }
}
