//! Account sync of remote IDs (their names and the recents list) between the
//! devices signed in to one account.
//!
//! The synced value is a last-writer-wins document:
//!   * each remote ID carries its name and when it was named (`nameAt`, ms
//!     since the Unix epoch; an empty name with a non-zero `nameAt` records
//!     that the name was removed), and when it was last connected to
//!     (`usedAt`);
//!   * "Clear" on the recents is one account-wide time, `recentsClearedAt`:
//!     an ID is a recent only when it was used after it.
//!
//! Merging takes the newer name and the later times, so it is commutative,
//! associative and idempotent: devices can merge in any order, any number of
//! times, and still agree. The server (`server/src/index.js`, `mergeDocs`)
//! applies the same rules, so a sync is one round trip: send the local
//! document, get the merged one back, merge it in locally.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::{clean_remote_name, remote_name_key};

/// The `v` of a [`SyncDoc`].
pub const SYNC_DOC_VERSION: u32 = 1;
/// Most IDs kept in one document; the server enforces the same cap.
pub const MAX_SYNC_ENTRIES: usize = 500;

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

/// The document an account stores and every signed-in device merges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncDoc {
    pub v: u32,
    /// Remote ID (lower-case) → its entry.
    pub entries: BTreeMap<String, SyncEntry>,
    /// Recents used at or before this time are hidden (ms since the epoch).
    pub recents_cleared_at: u64,
}

impl Default for SyncDoc {
    fn default() -> Self {
        SyncDoc {
            v: SYNC_DOC_VERSION,
            entries: BTreeMap::new(),
            recents_cleared_at: 0,
        }
    }
}

impl SyncDoc {
    /// Fold `other` into `self` (see the module docs for the rules).
    pub fn merge(&mut self, other: &SyncDoc) {
        for (id, theirs) in &other.entries {
            self.entries.entry(id.clone()).or_default().merge(theirs);
        }
        self.recents_cleared_at = self.recents_cleared_at.max(other.recents_cleared_at);
        self.prune();
    }

    /// Repair a document from an untrusted source (the network) in place:
    /// lower-case and validate the IDs, clean the names the way local names
    /// are cleaned, merge IDs that differ only in case, and cap the count.
    pub fn normalize(&mut self) {
        self.v = SYNC_DOC_VERSION;
        let entries = std::mem::take(&mut self.entries);
        for (id, mut entry) in entries {
            let Some(key) = remote_name_key(&id) else {
                continue;
            };
            entry.name = clean_remote_name(&entry.name).unwrap_or_default();
            self.entries.entry(key).or_default().merge(&entry);
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

    /// Over the cap: keep named IDs first, then the most recently touched.
    fn prune(&mut self) {
        if self.entries.len() <= MAX_SYNC_ENTRIES {
            return;
        }
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

    fn entry(name: &str, name_at: u64, used_at: u64) -> SyncEntry {
        SyncEntry {
            name: name.into(),
            name_at,
            used_at,
        }
    }

    fn doc(entries: &[(&str, SyncEntry)], cleared: u64) -> SyncDoc {
        SyncDoc {
            v: 1,
            entries: entries
                .iter()
                .map(|(k, e)| (k.to_string(), e.clone()))
                .collect(),
            recents_cleared_at: cleared,
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
        let a = doc(&[("x", entry("A", 5, 1)), ("y", entry("", 0, 9))], 3);
        let b = doc(&[("x", entry("B", 5, 7)), ("z", entry("Z", 2, 0))], 8);
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ab, ba);
        // Same-time names: the tie goes to the same one on both sides.
        assert_eq!(ab.entries["x"].name, "B");
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
                "ABC":{"name":"  Desk \n top ","nameAt":4,"usedAt":2},
                "abc":{"name":"older","nameAt":3,"usedAt":9},
                "":{"name":"x","nameAt":1}
            },"recentsClearedAt":1,"extra":true}"#,
        )
        .unwrap();
        d.normalize();
        assert_eq!(d.v, SYNC_DOC_VERSION);
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries["abc"], entry("Desk top", 4, 9));
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
}
