//! Preferences that follow the account (plans/08 D5): last-writer-wins per
//! preference, booleans only.
//!
//! Only the whitelisted keys in [`SYNCED_PREFS`] are ever synced; anything
//! else in a document is dropped on normalize, on both sides. Values are
//! booleans so the Rust and JavaScript merges cannot disagree on how a value
//! compares or serializes (08a §7.13): two writes with the same stamp are
//! broken by `false < true`, identically in `server/src/sync.js`.

use serde::{Deserialize, Serialize};

/// The preferences that follow the account, by their wire names. Must match
/// `SYNCED_PREFS` in `server/src/sync.js`.
pub const SYNCED_PREFS: [&str; 4] = [
    "translateModifiers",
    "captureShortcuts",
    "clipboardSync",
    "pasteDroppedFiles",
];

/// Whether `key` is a preference that follows the account.
pub fn is_synced_pref(key: &str) -> bool {
    SYNCED_PREFS.contains(&key)
}

/// One synced preference: its value and when it was last changed (ms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncPref {
    pub value: bool,
    pub at: u64,
}

impl SyncPref {
    /// Fold `other` in: the later change wins; the same stamp goes to `true`.
    pub fn merge(&mut self, other: &SyncPref) {
        if (other.at, other.value) > (self.at, self.value) {
            *self = *other;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(value: bool, at: u64) -> SyncPref {
        SyncPref { value, at }
    }

    #[test]
    fn the_later_change_wins_and_ties_go_to_true() {
        let mut a = p(true, 10);
        a.merge(&p(false, 20));
        assert_eq!(a, p(false, 20));
        a.merge(&p(true, 15));
        assert_eq!(a, p(false, 20));
        a.merge(&p(true, 20));
        assert_eq!(a, p(true, 20));
        let mut b = p(true, 20);
        b.merge(&p(false, 20));
        assert_eq!(b, p(true, 20));
    }

    #[test]
    fn only_whitelisted_keys_sync() {
        assert!(is_synced_pref("clipboardSync"));
        assert!(!is_synced_pref("serverUrl"));
        assert!(!is_synced_pref("useOpenH264"));
    }
}
