//! Open tabs that follow the account (plans/08 P5; the schema is fixed in P1
//! so the server stores and merges it from the start).
//!
//! One record per remote ID (a tab's identity is the remote it shows). A tab is
//! *open* while `openedAt > closedAt`; opening and closing only ever raise
//! their own stamp, so a close and a later reopen on different computers
//! resolve to whichever happened last. The order in the strip is a fractional
//! index key (`[0-9a-z]`, at most 24 characters) with its own stamp, so moving
//! a tab on one computer does not fight with opening it on another.
//!
//! **Detached windows** (plans/09, 09a §3.1): whether the tab is in its own
//! window, and that window's state, size and position — each with its own
//! stamp, last writer wins. Size and position are shares of a monitor's work
//! area in ten-thousandths, never pixels, so they mean the same on screens of
//! any size ([`super::placement`]). Position is optional and separately
//! stamped: a computer that cannot know where its windows are (Wayland) only
//! ever writes the others, and never overwrites a position written elsewhere.
//! Every field is left out of the JSON while it has its default, so a
//! document without detached windows reads and writes exactly as before.
//!
//! `server/src/sync.js` applies the same rules.

use serde::{Deserialize, Deserializer, Serialize};

/// Longest fractional index key accepted (08a §7.14).
pub const MAX_ORDER_KEY_CHARS: usize = 24;
/// Closed tabs are forgotten this long after they were closed (08a §7.15).
pub const CLOSED_TAB_TTL_MS: u64 = 90 * 24 * 3600 * 1000;
/// Most tab records in one document, far above any real workspace. The
/// server refuses a document over it; this side drops the excess when it
/// normalizes an untrusted one.
pub const MAX_SYNC_TABS: usize = 1000;
/// The whole of a monitor's work area, in the units of [`Size`] and [`Pos`].
pub const FRACTION_ONE: u16 = 10_000;
/// Highest monitor index kept (`0` = the primary, then left to right, top to
/// bottom). Far above any real desk; a larger one is dropped as invalid.
pub const MAX_MONITOR: u8 = 15;

/// A detached window's state. Deliberately not `Ord`: equal stamps are
/// broken by the wire name ([`as_str`](Self::as_str)), as in JS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WinState {
    #[default]
    Normal,
    Maximized,
    Minimized,
}

impl WinState {
    /// The name on the wire (and the tie-break on equal stamps: the larger
    /// name in code-point order wins, on both sides).
    pub fn as_str(self) -> &'static str {
        match self {
            WinState::Normal => "normal",
            WinState::Maximized => "maximized",
            WinState::Minimized => "minimized",
        }
    }

    /// Lenient: an unknown name reads as `Normal` (09a §3.1).
    pub fn from_name(name: &str) -> Self {
        match name {
            "maximized" => WinState::Maximized,
            "minimized" => WinState::Minimized,
            _ => WinState::Normal,
        }
    }
}

impl Serialize for WinState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WinState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(v.as_str().map(WinState::from_name).unwrap_or_default())
    }
}

/// A detached window's outer size as shares of its monitor's work area, in
/// ten-thousandths (`1..=10000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Size {
    pub w: u16,
    pub h: u16,
}

/// A detached window's outer top-left corner: which monitor (`0` = the
/// primary, then left to right, top to bottom) and where on its work area, in
/// ten-thousandths (`0..=10000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pos {
    pub monitor: u8,
    pub x: u16,
    pub y: u16,
}

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
    /// In its own window rather than in the strip.
    #[serde(skip_serializing_if = "is_false", deserialize_with = "lenient_bool")]
    pub detached: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub detached_at: u64,
    /// Its window's state.
    #[serde(skip_serializing_if = "is_normal")]
    pub state: WinState,
    #[serde(skip_serializing_if = "is_zero")]
    pub state_at: u64,
    /// Its window's size (`None`: not known, a default is used).
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_size"
    )]
    pub size: Option<Size>,
    #[serde(skip_serializing_if = "is_zero")]
    pub size_at: u64,
    /// Its window's position (`None`: not known; placed on the primary).
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_pos"
    )]
    pub pos: Option<Pos>,
    #[serde(skip_serializing_if = "is_zero")]
    pub pos_at: u64,
}

impl SyncTab {
    /// Open ⇔ opened after it was last closed.
    pub fn is_open(&self) -> bool {
        self.opened_at > self.closed_at
    }

    /// Open and in a window of its own. A detach happens while the tab is
    /// open, so it is stamped after that opening; a later reopen (also by an
    /// older computer that knows nothing of windows) makes it stale, and the
    /// tab comes back in the strip (09 §7, answer 9).
    pub fn is_detached(&self) -> bool {
        self.is_open() && self.detached && self.detached_at > self.opened_at
    }

    /// Its window's size, state and position as saved since it was last
    /// opened (older ones belong to a window a reopen forgot, as above).
    pub fn window_size(&self) -> Option<Size> {
        self.size.filter(|_| self.size_at > self.opened_at)
    }

    pub fn window_pos(&self) -> Option<Pos> {
        self.pos.filter(|_| self.pos_at > self.opened_at)
    }

    pub fn window_state(&self) -> WinState {
        if self.state_at > self.opened_at {
            self.state
        } else {
            WinState::Normal
        }
    }

    /// Fold `other` in: for each field the later stamp wins; on equal stamps
    /// the larger value (the key itself; `true` over `false`; the state's
    /// name in code-point order; a known size or position over none, then
    /// field by field), so both sides pick the same one. Opens and closes
    /// take the later of each.
    pub fn merge(&mut self, other: &SyncTab) {
        if (other.order_at, &other.order) > (self.order_at, &self.order) {
            self.order.clone_from(&other.order);
            self.order_at = other.order_at;
        }
        self.opened_at = self.opened_at.max(other.opened_at);
        self.closed_at = self.closed_at.max(other.closed_at);
        if (other.detached_at, other.detached) > (self.detached_at, self.detached) {
            self.detached = other.detached;
            self.detached_at = other.detached_at;
        }
        if (other.state_at, other.state.as_str()) > (self.state_at, self.state.as_str()) {
            self.state = other.state;
            self.state_at = other.state_at;
        }
        if (other.size_at, other.size) > (self.size_at, self.size) {
            self.size = other.size;
            self.size_at = other.size_at;
        }
        if (other.pos_at, other.pos) > (self.pos_at, self.pos) {
            self.pos = other.pos;
            self.pos_at = other.pos_at;
        }
    }

    /// The latest of its stamps.
    pub fn last_touched(&self) -> u64 {
        [
            self.order_at,
            self.opened_at,
            self.closed_at,
            self.detached_at,
            self.state_at,
            self.size_at,
            self.pos_at,
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    /// Clamp every stamp to at most `max` (08a §7.3: a computer whose clock
    /// runs ahead must not win every future edit).
    pub fn clamp_stamps(&mut self, max: u64) {
        for at in [
            &mut self.order_at,
            &mut self.opened_at,
            &mut self.closed_at,
            &mut self.detached_at,
            &mut self.state_at,
            &mut self.size_at,
            &mut self.pos_at,
        ] {
            *at = (*at).min(max);
        }
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn is_normal(v: &WinState) -> bool {
    *v == WinState::Normal
}

/// A share in ten-thousandths, if `v` is one within `lo..=FRACTION_ONE`.
fn fraction(v: Option<&serde_json::Value>, lo: u16) -> Option<u16> {
    let n = whole(v?)?;
    u16::try_from(n)
        .ok()
        .filter(|n| (lo..=FRACTION_ONE).contains(n))
}

/// A non-negative whole number, also when written as one with a fraction
/// (`5000.0`): JavaScript cannot tell them apart, so the server accepts both.
fn whole(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(f))
            .map(|f| f as u64)
    })
}

/// Lenient: anything but `true` reads as `false`.
fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(serde_json::Value::deserialize(d)? == serde_json::Value::Bool(true))
}

/// Lenient (09a §3.1): anything but a valid size reads as none, never as an
/// unreadable document.
fn lenient_size<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Size>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(size_from(&v))
}

fn lenient_pos<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Pos>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(pos_from(&v))
}

fn size_from(v: &serde_json::Value) -> Option<Size> {
    Some(Size {
        w: fraction(v.get("w"), 1)?,
        h: fraction(v.get("h"), 1)?,
    })
}

fn pos_from(v: &serde_json::Value) -> Option<Pos> {
    let monitor = u8::try_from(whole(v.get("monitor")?)?)
        .ok()
        .filter(|m| *m <= MAX_MONITOR)?;
    Some(Pos {
        monitor,
        x: fraction(v.get("x"), 0)?,
        y: fraction(v.get("y"), 0)?,
    })
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
            ..SyncTab::default()
        }
    }

    #[test]
    fn a_reopen_by_an_older_computer_brings_the_tab_back_to_the_strip() {
        let mut tab = t("m", 1, 10, 0);
        tab.detached = true;
        tab.detached_at = 12;
        tab.size = Some(Size { w: 5000, h: 5000 });
        tab.size_at = 13;
        tab.state = WinState::Maximized;
        tab.state_at = 13;
        assert!(tab.is_detached());
        assert_eq!(tab.window_state(), WinState::Maximized);
        // Closed and reopened where windows are unknown: only the stamps move.
        tab.merge(&t("m", 1, 0, 20));
        assert!(!tab.is_detached(), "closed");
        tab.merge(&t("m", 1, 30, 0));
        assert!(tab.is_open() && !tab.is_detached(), "reopened in the strip");
        assert_eq!(tab.window_size(), None, "its old window is forgotten");
        assert_eq!(tab.window_state(), WinState::Normal);
    }

    #[test]
    fn whole_numbers_written_with_a_fraction_still_read() {
        let tab: SyncTab = serde_json::from_str(
            r#"{"order":"m","size":{"w":5000.0,"h":1e3},"pos":{"monitor":1.0,"x":-0.0,"y":2.5}}"#,
        )
        .unwrap();
        assert_eq!(tab.size, Some(Size { w: 5000, h: 1000 }));
        assert_eq!(tab.pos, None, "2.5 is not a share");
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

    #[test]
    fn placement_fields_merge_each_on_its_own_stamp() {
        let mut here = t("m", 1, 1, 0);
        here.size = Some(Size { w: 5000, h: 4000 });
        here.size_at = 10;
        here.pos = Some(Pos {
            monitor: 1,
            x: 100,
            y: 200,
        });
        here.pos_at = 10;
        // Wayland elsewhere: a newer size, no position, detached later.
        let mut there = t("m", 1, 1, 0);
        there.size = Some(Size { w: 6000, h: 6000 });
        there.size_at = 20;
        there.detached = true;
        there.detached_at = 20;
        there.state = WinState::Maximized;
        there.state_at = 5;
        let mut a = here.clone();
        a.merge(&there);
        let mut b = there.clone();
        b.merge(&here);
        assert_eq!(a, b, "the order of merging does not matter");
        assert_eq!(a.size, Some(Size { w: 6000, h: 6000 }));
        assert_eq!(a.pos, here.pos, "a position is never lost to none");
        assert!(a.detached);
        assert_eq!(a.state, WinState::Maximized);
        assert_eq!(a.last_touched(), 20);
    }

    #[test]
    fn equal_stamps_pick_the_same_value_on_both_sides() {
        let mut x = t("m", 1, 1, 0);
        x.state = WinState::Minimized;
        x.state_at = 7;
        x.detached_at = 7;
        let mut y = t("m", 1, 1, 0);
        y.state_at = 7;
        y.detached = true;
        y.detached_at = 7;
        let (mut a, mut b) = (x.clone(), y.clone());
        a.merge(&y);
        b.merge(&x);
        assert_eq!(a, b);
        assert_eq!(a.state, WinState::Normal, "\"normal\" > \"minimized\"");
        assert!(a.detached);
    }

    #[test]
    fn placement_reads_leniently_and_writes_nothing_by_default() {
        let plain = t("m", 1, 2, 0);
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"order":"m","orderAt":1,"openedAt":2,"closedAt":0}"#,
            "a tab without a window serializes as before"
        );
        let odd: SyncTab = serde_json::from_str(
            r#"{"order":"m","openedAt":2,"state":"sideways","stateAt":3,
                "size":{"w":0,"h":5000},"sizeAt":4,
                "pos":{"monitor":99,"x":1,"y":1},"posAt":5,"detached":true,"detachedAt":6}"#,
        )
        .unwrap();
        assert_eq!(odd.state, WinState::Normal);
        assert_eq!(odd.size, None);
        assert_eq!(odd.pos, None);
        assert!(odd.detached);
        let good: SyncTab = serde_json::from_str(
            r#"{"order":"m","size":{"w":10000,"h":1},"pos":{"monitor":2,"x":0,"y":10000}}"#,
        )
        .unwrap();
        assert_eq!(good.size, Some(Size { w: 10000, h: 1 }));
        assert_eq!(
            good.pos,
            Some(Pos {
                monitor: 2,
                x: 0,
                y: 10000
            })
        );
        let back: SyncTab = serde_json::from_str(&serde_json::to_string(&good).unwrap()).unwrap();
        assert_eq!(back, good);
    }
}
