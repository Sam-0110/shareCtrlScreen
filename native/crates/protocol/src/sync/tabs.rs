//! Open tabs that follow the account (plans/08 P5; the schema is fixed in P1
//! so the server stores and merges it from the start).
//!
//! One record per remote ID (a tab's identity is the remote it shows). A tab is
//! *open* while `openedAt > closedAt`; opening and closing only ever raise
//! their own stamp, so a close and a later reopen on different computers
//! resolve to whichever happened last. The order in the strip is a fractional
//! index key (`[0-9a-z]`, at most 24 characters) with its own stamp, so moving
//! a tab on one computer does not fight with opening it on another.
//! `server/src/sync.js` applies the same rules.

use serde::{Deserialize, Serialize};

/// Longest fractional index key accepted (08a §7.14).
pub const MAX_ORDER_KEY_CHARS: usize = 24;
/// Closed tabs are forgotten this long after they were closed (08a §7.15).
pub const CLOSED_TAB_TTL_MS: u64 = 90 * 24 * 3600 * 1000;
/// Most tab records in one document, far above any real workspace. The
/// server refuses a document over it; this side drops the excess when it
/// normalizes an untrusted one.
pub const MAX_SYNC_TABS: usize = 1000;

/// One remote's tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncTab {
    /// Fractional index key: tabs are shown in ascending key order.
    pub order: String,
    /// When `order` was last set (ms).
    pub order_at: u64,
    /// When the tab was last opened (ms).
    pub opened_at: u64,
    /// When the tab was last closed (ms); 0 = never.
    pub closed_at: u64,
}

impl SyncTab {
    /// Open ⇔ opened after it was last closed.
    pub fn is_open(&self) -> bool {
        self.opened_at > self.closed_at
    }

    /// Fold `other` in: the later order (ties by the key itself, so both sides
    /// pick the same one), and the later open and close.
    pub fn merge(&mut self, other: &SyncTab) {
        if (other.order_at, &other.order) > (self.order_at, &self.order) {
            self.order.clone_from(&other.order);
            self.order_at = other.order_at;
        }
        self.opened_at = self.opened_at.max(other.opened_at);
        self.closed_at = self.closed_at.max(other.closed_at);
    }

    /// The latest of its stamps.
    pub fn last_touched(&self) -> u64 {
        self.order_at.max(self.opened_at).max(self.closed_at)
    }
}

/// Whether `key` is a usable fractional index key.
pub fn valid_order_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_ORDER_KEY_CHARS
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(order: &str, order_at: u64, opened_at: u64, closed_at: u64) -> SyncTab {
        SyncTab {
            order: order.into(),
            order_at,
            opened_at,
            closed_at,
        }
    }

    #[test]
    fn the_last_open_or_close_wins() {
        let mut a = t("m", 1, 10, 0);
        a.merge(&t("m", 1, 0, 20)); // closed later elsewhere
        assert!(!a.is_open());
        a.merge(&t("m", 1, 30, 0)); // reopened later still
        assert!(a.is_open());
    }

    #[test]
    fn order_is_last_writer_wins_with_a_stable_tie() {
        let mut a = t("b", 5, 1, 0);
        a.merge(&t("a", 9, 1, 0));
        assert_eq!(a.order, "a");
        let mut x = t("c", 9, 1, 0);
        x.merge(&t("d", 9, 1, 0));
        let mut y = t("d", 9, 1, 0);
        y.merge(&t("c", 9, 1, 0));
        assert_eq!(x, y);
    }

    #[test]
    fn order_keys_are_short_lower_case_alphanumerics() {
        assert!(valid_order_key("a0"));
        assert!(valid_order_key(&"z".repeat(MAX_ORDER_KEY_CHARS)));
        assert!(!valid_order_key(""));
        assert!(!valid_order_key("A"));
        assert!(!valid_order_key("a-b"));
        assert!(!valid_order_key(&"z".repeat(MAX_ORDER_KEY_CHARS + 1)));
    }
}
