//! A workspace: everything that belongs to the person rather than the computer
//! (plans/08 D4, 08a §3). One per account signed in on this computer, plus
//! `"local"` for use without an account.
//!
//! It holds the recents, the names given to remote IDs, the open tabs, the
//! preferences that follow the account and the saved host passwords. All of it
//! except the saved passwords and the active tab is merged with the account's
//! copy by [`Workspace::sync_doc`] / [`Workspace::apply_sync_doc`].
//!
//! Every method that changes something takes the stamp to record (`now`) from
//! the caller's [`Stamper`](crate::sync::Stamper), so stamps stay monotonic and
//! skew-corrected; nothing here reads the clock.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{clean_remote_name, remote_name_key};
use crate::sync::order::{between, spread};
use crate::sync::tabs::MAX_ORDER_KEY_CHARS;
use crate::sync::{Pos, Size, SyncDoc, SyncEntry, SyncPref, SyncTab, WinState, SYNCED_PREFS};

/// Most recents kept.
pub const MAX_RECENTS: usize = 10;
/// Remote IDs whose name was removed, kept so the removal reaches the account.
const MAX_NAME_TOMBSTONES: usize = 200;
/// Upper bound on named remote IDs, so a hand-edited file cannot grow unbounded.
pub const MAX_REMOTE_NAMES: usize = 200;

/// When this computer last changed a remote ID (see [`Workspace::id_stamps`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IdStamp {
    /// Name set or removed (ms since the epoch); 0 = never on this computer.
    pub name_at: u64,
    /// Last connected to (ms since the epoch); 0 = never on this computer.
    pub used_at: u64,
}

/// One remote's tab (08a §3). Open ⇔ `opened_at > closed_at`. Same fields
/// and merge rules as [`SyncTab`].
pub type TabRecord = SyncTab;

/// The preferences that follow the account (D5). Booleans only (08a §7.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    /// Viewer: translate modifier keys for the host's OS (a Mac's Cmd becomes
    /// Ctrl on a PC host and the reverse). Default on, as in Microsoft's
    /// Windows App; off sends every key literally.
    pub translate_modifiers: bool,
    /// Viewer: capture OS-reserved shortcuts (Alt+Tab, Win) locally and send
    /// them to the host.
    pub capture_shortcuts: bool,
    /// Mirror clipboard text between the two computers during a session. The
    /// host side also requires the live Control permission. One setting for
    /// both roles (D5, the owner's decision).
    pub clipboard_sync: bool,
    /// Host: paste a received file into the window it was dropped on; off
    /// leaves it on the clipboard.
    pub paste_dropped_files: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            translate_modifiers: true,
            capture_shortcuts: false,
            clipboard_sync: true,
            paste_dropped_files: true,
        }
    }
}

impl Prefs {
    /// The value of a synced preference by its wire name.
    pub fn get(&self, key: &str) -> Option<bool> {
        Some(match key {
            "translateModifiers" => self.translate_modifiers,
            "captureShortcuts" => self.capture_shortcuts,
            "clipboardSync" => self.clipboard_sync,
            "pasteDroppedFiles" => self.paste_dropped_files,
            _ => return None,
        })
    }

    /// Set a synced preference by its wire name; false for an unknown key.
    fn set(&mut self, key: &str, value: bool) -> bool {
        let slot = match key {
            "translateModifiers" => &mut self.translate_modifiers,
            "captureShortcuts" => &mut self.capture_shortcuts,
            "clipboardSync" => &mut self.clipboard_sync,
            "pasteDroppedFiles" => &mut self.paste_dropped_files,
            _ => return false,
        };
        *slot = value;
        true
    }
}

/// Everything that belongs to one person on this computer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Workspace {
    /// Remote IDs connected to, most recent first. Capped at [`MAX_RECENTS`].
    pub recent_ids: Vec<String>,
    /// Remote ID → the name the user gave it ("Office PC"). Kept apart from
    /// the recents, so clearing them does not forget names. A tab's name is
    /// its remote's name (08a §3).
    pub remote_names: BTreeMap<String, String>,
    /// Remote ID → when its name and its last use were last changed here. Only
    /// used to merge with the account's copy, so a newer change wins whichever
    /// computer made it.
    pub id_stamps: BTreeMap<String, IdStamp>,
    /// When the recents were last cleared (ms); 0 = never.
    pub recents_cleared_at: u64,
    /// Remote ID → its tab, open or closed (plans/08 P4/P5).
    pub tabs: BTreeMap<String, TabRecord>,
    /// The tab shown last on this computer. Never synced.
    pub active_tab: Option<String>,
    /// The preferences that follow the account.
    pub prefs: Prefs,
    /// Synced preference (wire name) → when it was last changed here; absent
    /// or 0 = never, so the account's value wins and this one is not sent.
    pub pref_stamps: BTreeMap<String, u64>,
    /// Host UUID → that host's password, sealed by the OS secret store (an
    /// opaque handle; the engine owns the crypto). Only written after the
    /// password was accepted. Never synced.
    pub saved_logins: BTreeMap<String, String>,
    /// Fields a newer version added, kept so this build does not drop them.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Workspace {
    /// Whether there is anything the user would miss: names, open tabs or
    /// recents. Decides the "bring this computer's tabs and names" prompt.
    pub fn has_user_data(&self) -> bool {
        !self.remote_names.is_empty() || !self.recent_ids.is_empty() || self.open_tab_count() > 0
    }

    /// Open tabs.
    pub fn open_tab_count(&self) -> usize {
        self.tabs.values().filter(|t| t.is_open()).count()
    }

    /// Prepend `id` to the recents, de-duplicated and capped, stamped `now`.
    pub fn add_recent(&mut self, id: &str, now: u64) {
        let clean = id.trim();
        if clean.is_empty() || clean.len() > 64 {
            return;
        }
        self.recent_ids.retain(|x| x != clean);
        self.recent_ids.insert(0, clean.to_string());
        self.recent_ids.truncate(MAX_RECENTS);
        if let Some(key) = remote_name_key(clean) {
            let stamp = self.id_stamps.entry(key).or_default();
            stamp.used_at = stamp.used_at.max(now);
        }
        self.prune_stamps();
    }

    /// Empty the recents (named IDs stay). Synced: clears them on every
    /// computer signed in to the same account.
    pub fn clear_recents(&mut self, now: u64) {
        self.recent_ids.clear();
        self.recents_cleared_at = self.recents_cleared_at.max(now);
        self.prune_stamps();
    }

    /// Name (or rename) a remote ID. An empty or whitespace-only `name`
    /// removes the name. Returns false, changing nothing, when `id` is not
    /// usable or the list of names is full.
    pub fn set_remote_name(&mut self, id: &str, name: &str, now: u64) -> bool {
        let Some(key) = remote_name_key(id) else {
            return false;
        };
        let name = clean_remote_name(name);
        if self.remote_names.get(&key) == name.as_ref() {
            return true; // unchanged: keep its stamp, so an older edit elsewhere does not win
        }
        match name {
            None => {
                self.remote_names.remove(&key);
            }
            Some(name) => {
                if !self.remote_names.contains_key(&key)
                    && self.remote_names.len() >= MAX_REMOTE_NAMES
                {
                    return false;
                }
                self.remote_names.insert(key.clone(), name);
            }
        }
        let stamp = self.id_stamps.entry(key).or_default();
        stamp.name_at = stamp.name_at.max(now);
        self.prune_stamps();
        true
    }

    // ---- tabs (plans/08 P4, 08a §3) ------------------------------------------

    /// The open tabs' remote IDs in strip order: by order key, ties broken by
    /// the remote ID, so every computer shows the same order (08a §7.14).
    pub fn open_tabs(&self) -> Vec<String> {
        let mut open: Vec<(&str, &str)> = self
            .tabs
            .iter()
            .filter(|(_, t)| t.is_open())
            .map(|(id, t)| (t.order.as_str(), id.as_str()))
            .collect();
        open.sort_unstable();
        open.into_iter().map(|(_, id)| id.to_string()).collect()
    }

    /// Open a tab for remote `id` at the end of the strip, or leave an open
    /// one where it is. Stamped `now`. Returns the tab's key (the lower-cased
    /// ID), or `None` when `id` is not a remote ID.
    pub fn open_tab(&mut self, id: &str, now: u64) -> Option<String> {
        let key = tab_key(id)?;
        if self.tabs.get(&key).is_some_and(TabRecord::is_open) {
            return Some(key);
        }
        let order = self.order_at_end(now);
        let tab = self.tabs.entry(key.clone()).or_default();
        // Reopened: in the strip, its old window forgotten (09 §7.9), so a
        // tab closed in a window does not come back in one.
        if tab.closed_at > 0 {
            if tab.detached {
                tab.detached = false;
                tab.detached_at = now.max(tab.detached_at);
            }
            if tab.size.is_some() {
                tab.size = None;
                tab.size_at = now.max(tab.size_at);
            }
            if tab.pos.is_some() {
                tab.pos = None;
                tab.pos_at = now.max(tab.pos_at);
            }
            if tab.state != WinState::Normal {
                tab.state = WinState::Normal;
                tab.state_at = now.max(tab.state_at);
            }
        }
        // Opened after its last close, wherever that close was stamped.
        tab.opened_at = now.max(tab.closed_at + 1);
        tab.order = order;
        tab.order_at = tab.order_at.max(now);
        Some(key)
    }

    /// Close `id`'s tab (it stays as a closed record, so the close syncs).
    /// When it was the active one, its right-hand neighbour (else the
    /// left-hand one, else none) becomes active. `false` if it was not open.
    pub fn close_tab(&mut self, id: &str, now: u64) -> bool {
        let Some(key) = tab_key(id) else {
            return false;
        };
        if !self.tabs.get(&key).is_some_and(TabRecord::is_open) {
            return false;
        }
        // The active tab is always a docked one: its neighbours in the strip.
        let docked = self.docked_tabs();
        if let Some(tab) = self.tabs.get_mut(&key) {
            tab.closed_at = now.max(tab.opened_at + 1);
        }
        if self.active_tab.as_deref() == Some(key.as_str()) {
            let pos = docked.iter().position(|k| *k == key);
            self.active_tab = pos.and_then(|pos| {
                docked
                    .get(pos + 1)
                    .or_else(|| pos.checked_sub(1).and_then(|p| docked.get(p)))
                    .cloned()
            });
        }
        true
    }

    /// Move `id`'s tab just before `before`'s (`None`: to the end). `false`
    /// when either is not an open tab.
    pub fn move_tab(&mut self, id: &str, before: Option<&str>, now: u64) -> bool {
        let Some(key) = tab_key(id) else {
            return false;
        };
        let mut open = self.open_tabs();
        let Some(from) = open.iter().position(|k| *k == key) else {
            return false;
        };
        open.remove(from);
        let at = match before.map(tab_key) {
            None => open.len(),
            Some(Some(b)) => match open.iter().position(|k| *k == b) {
                Some(i) => i,
                None => return false,
            },
            Some(None) => return false,
        };
        let order_of = |ws: &Self, k: Option<&String>| k.map(|k| ws.tabs[k].order.clone());
        let lo = order_of(self, at.checked_sub(1).and_then(|i| open.get(i)));
        let hi = order_of(self, open.get(at));
        let key_between = between(lo.as_deref(), hi.as_deref());
        open.insert(at, key.clone());
        match key_between {
            Some(order) => {
                if let Some(tab) = self.tabs.get_mut(&key) {
                    tab.order = order;
                    tab.order_at = tab.order_at.max(now);
                }
            }
            // No room between the neighbours (or equal keys from two
            // computers): every open tab gets a fresh key, in the new order.
            None => self.respread(&open, now),
        }
        true
    }

    /// A key after every open tab's, re-keying the strip first if the last
    /// key leaves no room.
    fn order_at_end(&mut self, now: u64) -> String {
        let open = self.open_tabs();
        let last = open.last().map(|k| self.tabs[k].order.clone());
        if let Some(order) = between(last.as_deref(), None) {
            return order;
        }
        self.respread(&open, now);
        let last = open.last().map(|k| self.tabs[k].order.clone());
        // A fresh spread always leaves room at the end.
        between(last.as_deref(), None).unwrap_or_else(|| "z".repeat(MAX_ORDER_KEY_CHARS))
    }

    /// Give the tabs `in_order` fresh, evenly spread keys in that order.
    fn respread(&mut self, in_order: &[String], now: u64) {
        for (key, order) in in_order.iter().zip(spread(in_order.len())) {
            if let Some(tab) = self.tabs.get_mut(key) {
                tab.order = order;
                tab.order_at = tab.order_at.max(now);
            }
        }
    }
    // ---- detached windows (plans/09, 09a §3.1, §5.1, §5.2) --------------------

    /// The open tabs in the strip (not in a window of their own), in strip
    /// order.
    pub fn docked_tabs(&self) -> Vec<String> {
        self.open_tabs()
            .into_iter()
            .filter(|k| !self.tabs[k].detached)
            .collect()
    }

    /// The open tabs in a window of their own, in strip order.
    pub fn detached_tabs(&self) -> Vec<String> {
        self.open_tabs()
            .into_iter()
            .filter(|k| self.tabs[k].detached)
            .collect()
    }

    /// Put `id`'s open tab in a window of its own. When it was the active
    /// tab, the docked tab before it (else after it, else none) becomes
    /// active. `false` when it is not an open, docked tab.
    pub fn detach_tab(&mut self, id: &str, now: u64) -> bool {
        let Some(key) = tab_key(id) else {
            return false;
        };
        let docked = self.docked_tabs();
        let Some(pos) = docked.iter().position(|k| *k == key) else {
            return false;
        };
        if let Some(tab) = self.tabs.get_mut(&key) {
            tab.detached = true;
            tab.detached_at = now.max(tab.detached_at);
        }
        if self.active_tab.as_deref() == Some(key.as_str()) {
            self.active_tab = pos
                .checked_sub(1)
                .and_then(|p| docked.get(p))
                .or_else(|| docked.get(pos + 1))
                .cloned();
        }
        true
    }

    /// Put `id`'s detached tab back in the strip, at its end, as the active
    /// tab. `false` when it is not an open, detached tab.
    pub fn dock_tab(&mut self, id: &str, now: u64) -> bool {
        let Some(key) = tab_key(id) else {
            return false;
        };
        if !self
            .tabs
            .get(&key)
            .is_some_and(|t| t.is_open() && t.detached)
        {
            return false;
        }
        if let Some(tab) = self.tabs.get_mut(&key) {
            tab.detached = false;
            tab.detached_at = now.max(tab.detached_at);
        }
        self.move_tab(&key, None, now);
        self.active_tab = Some(key);
        true
    }

    /// Record `id`'s window state. Stamped only when it changes. `false`
    /// when it is not an open tab.
    pub fn set_window_state(&mut self, id: &str, state: WinState, now: u64) -> bool {
        let Some(tab) = tab_key(id).and_then(|k| self.tabs.get_mut(&k)) else {
            return false;
        };
        if !tab.is_open() {
            return false;
        }
        if tab.state != state {
            tab.state = state;
            tab.state_at = now.max(tab.state_at);
        }
        true
    }

    /// Record `id`'s window size and, when this computer knows it, its
    /// position (`None`: unknown here, e.g. Wayland — a position saved
    /// elsewhere is kept, 09a J9). Each stamped only when it changes. `false`
    /// when it is not an open tab.
    pub fn set_placement(&mut self, id: &str, size: Size, pos: Option<Pos>, now: u64) -> bool {
        let Some(tab) = tab_key(id).and_then(|k| self.tabs.get_mut(&k)) else {
            return false;
        };
        if !tab.is_open() {
            return false;
        }
        if tab.size != Some(size) {
            tab.size = Some(size);
            tab.size_at = now.max(tab.size_at);
        }
        if let Some(pos) = pos.filter(|p| tab.pos != Some(*p)) {
            tab.pos = Some(pos);
            tab.pos_at = now.max(tab.pos_at);
        }
        true
    }

    /// The name given to `id`, if any.
    pub fn remote_name(&self, id: &str) -> Option<&str> {
        let key = remote_name_key(id)?;
        self.remote_names.get(&key).map(String::as_str)
    }

    /// Set a synced preference. Stamped **only when the value changes**
    /// (08a §6.12 step 7): Settings → Save sends every field, and stamping an
    /// unchanged one would let this computer's stale value beat a newer change
    /// made elsewhere. Returns whether anything changed.
    pub fn set_pref(&mut self, key: &str, value: bool, now: u64) -> bool {
        if self.prefs.get(key) == Some(value) || !self.prefs.set(key, value) {
            return false;
        }
        let at = self.pref_stamps.entry(key.to_string()).or_default();
        *at = (*at).max(now);
        true
    }

    /// The latest stamp in this workspace, so a [`Stamper`] can continue after it.
    ///
    /// [`Stamper`]: crate::sync::Stamper
    pub fn latest_stamp(&self) -> u64 {
        let ids = self.id_stamps.values().map(|s| s.name_at.max(s.used_at));
        let tabs = self.tabs.values().map(SyncTab::last_touched);
        let prefs = self.pref_stamps.values().copied();
        ids.chain(tabs)
            .chain(prefs)
            .chain([self.recents_cleared_at])
            .max()
            .unwrap_or(0)
    }

    /// This workspace as a sync document. Tabs and preferences are included
    /// only when the server advertised them (`with_tabs` / `with_prefs`), and
    /// only preferences changed on some computer (stamped) are sent, so a
    /// default nobody chose never overrides a choice. IDs listed before this
    /// computer kept stamps get tiny times, so any real change made elsewhere
    /// wins over them.
    pub fn sync_doc(&self, with_tabs: bool, with_prefs: bool) -> SyncDoc {
        let mut doc = SyncDoc {
            recents_cleared_at: self.recents_cleared_at,
            ..SyncDoc::default()
        };
        let n = self.recent_ids.len() as u64;
        for (i, id) in self.recent_ids.iter().enumerate() {
            let Some(key) = remote_name_key(id) else {
                continue;
            };
            let stamp = self.id_stamps.get(&key).copied().unwrap_or_default();
            let used_at = if stamp.used_at > self.recents_cleared_at {
                stamp.used_at
            } else {
                // Unstamped (an older config): keep it listed, in its order.
                self.recents_cleared_at + (n - i as u64)
            };
            doc.entries.entry(key).or_default().used_at = used_at;
        }
        for (id, name) in &self.remote_names {
            let stamp = self.id_stamps.get(id).copied().unwrap_or_default();
            let e = doc.entries.entry(id.clone()).or_default();
            e.name.clone_from(name);
            e.name_at = stamp.name_at.max(1);
        }
        // Removed names (empty name, real time), so the removal wins over the
        // old name on other computers. Also covers a removed name on a recent.
        for (id, stamp) in &self.id_stamps {
            if stamp.name_at > 0 && !self.remote_names.contains_key(id) {
                let e: &mut SyncEntry = doc.entries.entry(id.clone()).or_default();
                e.name_at = e.name_at.max(stamp.name_at);
            }
        }
        if with_tabs {
            doc.tabs = self.tabs.clone();
        }
        if with_prefs {
            for key in SYNCED_PREFS {
                let at = self.pref_stamps.get(key).copied().unwrap_or(0);
                if let (true, Some(value)) = (at > 0, self.prefs.get(key)) {
                    doc.prefs.insert(key.to_string(), SyncPref { value, at });
                }
            }
        }
        doc
    }

    /// Merge `remote` (the account's document) into this workspace and return
    /// the merged document, which is what the account should hold. `now` is
    /// the server-corrected time used to clean `remote` (see
    /// [`SyncDoc::normalize`]). Tabs and preferences in `remote` are applied
    /// whenever present; local ones were only sent if the server stores them.
    pub fn apply_sync_doc(
        &mut self,
        remote: &SyncDoc,
        now: u64,
        with_tabs: bool,
        with_prefs: bool,
    ) -> SyncDoc {
        let mut remote = remote.clone();
        remote.normalize(now);
        let mut merged = self.sync_doc(with_tabs, with_prefs);
        merged.merge(&remote);

        self.recents_cleared_at = merged.recents_cleared_at;
        self.recent_ids = merged
            .recents()
            .into_iter()
            .take(MAX_RECENTS)
            .map(String::from)
            .collect();

        // Newest names first, so the cap drops the oldest.
        let mut named: Vec<(&String, &SyncEntry)> = merged
            .entries
            .iter()
            .filter(|(_, e)| !e.name.is_empty())
            .collect();
        named.sort_by(|a, b| b.1.name_at.cmp(&a.1.name_at).then(a.0.cmp(b.0)));
        self.remote_names = named
            .into_iter()
            .take(MAX_REMOTE_NAMES)
            .map(|(id, e)| (id.clone(), e.name.clone()))
            .collect();

        self.id_stamps = merged
            .entries
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    IdStamp {
                        name_at: e.name_at,
                        used_at: e.used_at,
                    },
                )
            })
            .collect();
        self.prune_stamps();

        for (id, theirs) in &merged.tabs {
            match self.tabs.get_mut(id) {
                Some(mine) => mine.merge(theirs),
                None => {
                    self.tabs.insert(id.clone(), theirs.clone());
                }
            }
        }
        self.forget_closed_tabs(now);

        for (key, theirs) in &merged.prefs {
            let mine = SyncPref {
                value: self.prefs.get(key).unwrap_or_default(),
                at: self.pref_stamps.get(key).copied().unwrap_or(0),
            };
            let mut winner = mine;
            winner.merge(theirs);
            if winner != mine && self.prefs.set(key, winner.value) {
                self.pref_stamps.insert(key.clone(), winner.at);
            }
        }
        merged
    }

    /// Fold `local` (this computer's workspace without an account) into this
    /// one, as "Bring this computer's tabs and names into the account" does
    /// (08a §6.10 step 3): names, recents and tabs union by the sync rules,
    /// this workspace's preferences win (local ones are not merged), and a
    /// host's saved password here wins over the local one, which is adopted
    /// only where there is none. Returns the sealed passwords that were not
    /// adopted, so their secret-store items can be forgotten.
    ///
    /// Only what the local workspace *has* joins: its names, its visible
    /// recents and its tabs. Its history does not — not when its recents were
    /// cleared (that would hide the account's own older recents on every
    /// computer) and not the names it removed (a local removal must not delete
    /// the account's name for that computer). P1 review M2.
    pub fn absorb_local(&mut self, local: Workspace, now: u64) -> Vec<String> {
        let mut doc = local.sync_doc(true, false);
        let cleared = doc.recents_cleared_at;
        doc.recents_cleared_at = 0;
        doc.entries.retain(|_, e| {
            if e.used_at <= cleared {
                e.used_at = 0; // not a local recent any more
            }
            if e.name.is_empty() {
                e.name_at = 0; // a removal, not a name
            }
            e.used_at > 0 || e.name_at > 0
        });
        let mut rest = Vec::new();
        self.apply_sync_doc(&doc, now, true, false);
        for (host, sealed) in local.saved_logins {
            match self.saved_logins.entry(host) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(sealed);
                }
                std::collections::btree_map::Entry::Occupied(_) => rest.push(sealed),
            }
        }
        rest
    }

    /// Repair an untrusted (hand-edited, older) value in place: drop unusable
    /// IDs, clean names, cap the lists, clamp every stamp to `max_stamp`, and
    /// drop tabs with unusable keys.
    pub fn normalize(&mut self, max_stamp: u64) {
        self.recent_ids.retain(|x| x.len() <= 64);
        self.recent_ids.truncate(MAX_RECENTS);
        let names = std::mem::take(&mut self.remote_names);
        for (id, name) in names {
            if self.remote_names.len() >= MAX_REMOTE_NAMES {
                break;
            }
            if let (Some(key), Some(name)) = (remote_name_key(&id), clean_remote_name(&name)) {
                self.remote_names.insert(key, name);
            }
        }
        let stamps = std::mem::take(&mut self.id_stamps);
        for (id, mut stamp) in stamps {
            if let Some(key) = remote_name_key(&id) {
                stamp.name_at = stamp.name_at.min(max_stamp);
                stamp.used_at = stamp.used_at.min(max_stamp);
                self.id_stamps.insert(key, stamp);
            }
        }
        self.recents_cleared_at = self.recents_cleared_at.min(max_stamp);
        self.prune_stamps();

        let tabs = std::mem::take(&mut self.tabs);
        for (id, mut tab) in tabs {
            let id = id.trim().to_lowercase();
            if !crate::is_valid_uuid_v4(&id) || !crate::sync::valid_order_key(&tab.order) {
                continue;
            }
            tab.clamp_stamps(max_stamp);
            self.tabs.insert(id, tab);
        }
        // `max_stamp` is now plus the slack: the same 90-day rule as the sync.
        self.forget_closed_tabs(max_stamp.saturating_sub(crate::sync::FUTURE_SLACK_MS));
        if self
            .active_tab
            .as_ref()
            .is_some_and(|a| !self.tabs.get(a).is_some_and(TabRecord::is_open))
        {
            self.active_tab = None;
        }

        self.pref_stamps
            .retain(|key, _| crate::sync::is_synced_pref(key));
        for at in self.pref_stamps.values_mut() {
            *at = (*at).min(max_stamp);
        }
    }

    /// Forget tabs closed more than [`CLOSED_TAB_TTL_MS`] before `now`, as
    /// the sync does on both sides (08a §7.15), so closed tabs cannot pile up
    /// locally and be sent forever.
    ///
    /// [`CLOSED_TAB_TTL_MS`]: crate::sync::CLOSED_TAB_TTL_MS
    pub fn forget_closed_tabs(&mut self, now: u64) {
        let before = now.saturating_sub(crate::sync::CLOSED_TAB_TTL_MS);
        self.tabs
            .retain(|_, t| t.is_open() || t.closed_at >= before);
    }

    /// Keep stamps only for IDs still listed (recent or named) and for the
    /// newest removed names, so the map cannot grow without bound.
    fn prune_stamps(&mut self) {
        let mut tombstones: Vec<(String, u64)> = Vec::new();
        let recents = &self.recent_ids;
        let names = &self.remote_names;
        self.id_stamps.retain(|id, stamp| {
            if names.contains_key(id) || recents.iter().any(|r| r.eq_ignore_ascii_case(id)) {
                return true;
            }
            if stamp.name_at > 0 {
                tombstones.push((id.clone(), stamp.name_at));
                return true;
            }
            false
        });
        if tombstones.len() > MAX_NAME_TOMBSTONES {
            tombstones.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (id, _) in tombstones.into_iter().skip(MAX_NAME_TOMBSTONES) {
                self.id_stamps.remove(&id);
            }
        }
    }
}

/// A tab's key: the remote ID, lower-cased; `None` for anything else.
fn tab_key(id: &str) -> Option<String> {
    let key = id.trim().to_lowercase();
    crate::is_valid_uuid_v4(&key).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "11111111-1111-4111-8111-111111111111";
    const T2: &str = "22222222-2222-4222-8222-222222222222";
    const T3: &str = "33333333-3333-4333-8333-333333333333";

    #[test]
    fn tabs_open_at_the_end_once_and_only_for_remote_ids() {
        let mut w = Workspace::default();
        assert_eq!(w.open_tab(T2, 10).as_deref(), Some(T2));
        assert_eq!(w.open_tab(T1, 11).as_deref(), Some(T1));
        // Already open: stays where it is.
        assert_eq!(w.open_tab(&T2.to_uppercase(), 12).as_deref(), Some(T2));
        assert_eq!(w.open_tabs(), [T2, T1]);
        assert_eq!(w.open_tab("not-an-id", 13), None);
        assert_eq!(w.open_tab_count(), 2);
    }

    #[test]
    fn detaching_and_docking_keep_the_strip_and_the_active_tab_right() {
        let mut w = Workspace::default();
        for (i, t) in [T1, T2, T3].iter().enumerate() {
            w.open_tab(t, 10 + i as u64);
        }
        w.active_tab = Some(T2.into());
        assert!(w.detach_tab(T2, 20));
        assert_eq!(w.active_tab.as_deref(), Some(T1), "the tab before it");
        assert_eq!(w.docked_tabs(), [T1, T3]);
        assert_eq!(w.detached_tabs(), [T2]);
        assert!(!w.detach_tab(T2, 21), "already detached");
        w.active_tab = Some(T1.into());
        assert!(w.detach_tab(T1, 22));
        assert_eq!(
            w.active_tab.as_deref(),
            Some(T3),
            "none before: the one after"
        );
        // Closing a detached tab leaves the active tab alone.
        assert!(w.close_tab(T1, 23));
        assert_eq!(w.active_tab.as_deref(), Some(T3));
        // Dock: at the end of the strip, active.
        assert!(w.dock_tab(T2, 24));
        assert_eq!(w.docked_tabs(), [T3, T2]);
        assert_eq!(w.active_tab.as_deref(), Some(T2));
        assert!(!w.dock_tab(T2, 25), "already docked");
    }

    #[test]
    fn placement_is_stamped_on_change_and_a_reopen_forgets_the_window() {
        let mut w = Workspace::default();
        w.open_tab(T1, 10);
        w.detach_tab(T1, 11);
        let size = Size { w: 5000, h: 5000 };
        let pos = Pos {
            monitor: 0,
            x: 10,
            y: 20,
        };
        assert!(w.set_placement(T1, size, Some(pos), 12));
        assert!(w.set_window_state(T1, WinState::Maximized, 12));
        // Same values later: stamps stay; no position known: it stays.
        w.set_placement(T1, size, None, 30);
        w.set_window_state(T1, WinState::Maximized, 30);
        let t = &w.tabs[T1];
        assert_eq!((t.size_at, t.pos_at, t.state_at), (12, 12, 12));
        assert_eq!(t.pos, Some(pos));
        w.close_tab(T1, 40);
        assert!(!w.set_placement(T1, size, None, 41), "closed");
        w.open_tab(T1, 50);
        let t = &w.tabs[T1];
        assert!(!t.detached && t.size.is_none() && t.pos.is_none());
        assert_eq!(t.state, WinState::Normal);
        assert_eq!(
            (t.detached_at, t.size_at, t.pos_at, t.state_at),
            (50, 50, 50, 50)
        );
    }

    #[test]
    fn closing_the_active_tab_activates_a_neighbour_and_reopening_wins() {
        let mut w = Workspace::default();
        for (i, t) in [T1, T2, T3].iter().enumerate() {
            w.open_tab(t, 10 + i as u64);
        }
        w.active_tab = Some(T2.into());
        assert!(w.close_tab(T2, 20));
        assert_eq!(w.active_tab.as_deref(), Some(T3));
        assert!(w.close_tab(T3, 21));
        assert_eq!(w.active_tab.as_deref(), Some(T1));
        assert!(!w.close_tab(T3, 22), "not open");
        assert!(w.close_tab(T1, 23));
        assert_eq!(w.active_tab, None);
        // A reopen is stamped after the close even with an older clock.
        w.open_tab(T1, 5);
        assert!(w.tabs[T1].is_open());
        assert_eq!(w.open_tabs(), [T1]);
    }

    #[test]
    fn tabs_move_between_neighbours_and_respread_when_out_of_room() {
        let mut w = Workspace::default();
        for (i, t) in [T1, T2, T3].iter().enumerate() {
            w.open_tab(t, 10 + i as u64);
        }
        assert!(w.move_tab(T3, Some(T1), 20));
        assert_eq!(w.open_tabs(), [T3, T1, T2]);
        assert!(w.move_tab(T3, None, 21));
        assert_eq!(w.open_tabs(), [T1, T2, T3]);
        assert!(!w.move_tab(T1, Some("not-an-id"), 22));
        // Two computers gave two tabs the same key: a move between them
        // re-keys the strip, keeping the requested order.
        w.tabs.get_mut(T1).unwrap().order = "m".into();
        w.tabs.get_mut(T2).unwrap().order = "m".into();
        assert!(w.move_tab(T3, Some(T2), 23));
        assert_eq!(w.open_tabs(), [T1, T3, T2]);
        let keys: Vec<_> = w
            .open_tabs()
            .iter()
            .map(|k| w.tabs[k].order.clone())
            .collect();
        assert!(keys.windows(2).all(|p| p[0] < p[1]), "{keys:?}");
    }

    /// P5 acceptance, as data: tabs opened on one computer appear on another,
    /// a rename follows, and a close stays closed even after the other
    /// computer comes back a week later with its old copy.
    #[test]
    fn tabs_follow_the_account_and_a_close_stays_closed() {
        const DAY: u64 = 24 * 3600 * 1000;
        let t0 = 1_800_000_000_000;
        let mut laptop = Workspace::default();
        for (i, t) in [T1, T2, T3].iter().enumerate() {
            laptop.open_tab(t, t0 + i as u64);
        }
        laptop.set_remote_name(T1, "Office", t0 + 5);
        let mut desktop = Workspace::default();
        desktop.apply_sync_doc(&laptop.sync_doc(true, true), t0 + 10, true, true);
        assert_eq!(desktop.open_tabs(), laptop.open_tabs());
        assert_eq!(desktop.remote_name(T1), Some("Office"));

        // Renamed on the desktop: the laptop shows it after its next sync.
        desktop.set_remote_name(T1, "Office PC", t0 + 20);
        laptop.apply_sync_doc(&desktop.sync_doc(true, true), t0 + 30, true, true);
        assert_eq!(laptop.remote_name(T1), Some("Office PC"));

        // Closed on the laptop while the desktop is away for a week; the
        // desktop's stale copy (T2 still open) does not bring it back.
        let stale = desktop.sync_doc(true, true);
        laptop.close_tab(T2, t0 + 40);
        laptop.apply_sync_doc(&stale, t0 + 7 * DAY, true, true);
        assert_eq!(laptop.open_tabs(), [T1, T3]);
        desktop.apply_sync_doc(&laptop.sync_doc(true, true), t0 + 7 * DAY, true, true);
        assert_eq!(desktop.open_tabs(), [T1, T3]);
    }
    use crate::sync::{now_ms, FEATURE_PREFS};

    const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const Y: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

    /// One sync round trip, as the app does it: the server merges the
    /// computer's document into its own, the computer merges the result back.
    fn sync(server: &mut SyncDoc, device: &mut Workspace) {
        let now = now_ms();
        server.merge(&device.sync_doc(true, true));
        let merged = device.apply_sync_doc(server, now, true, true);
        assert_eq!(&merged, server, "computer and server disagree after a sync");
    }

    #[test]
    fn recents_dedupe_and_cap() {
        let mut ws = Workspace::default();
        for i in 0..15 {
            ws.add_recent(&format!("id-{i}"), i + 1);
        }
        ws.add_recent("id-14", 99); // dup moves to front, no growth
        assert_eq!(ws.recent_ids.len(), MAX_RECENTS);
        assert_eq!(ws.recent_ids[0], "id-14");
    }

    #[test]
    fn remote_names_set_rename_and_remove() {
        let mut ws = Workspace::default();
        assert!(ws.set_remote_name(X, "  Office \t PC\n ", 1));
        assert_eq!(ws.remote_name(X), Some("Office PC"));
        // Case and surrounding space in the ID do not matter.
        assert_eq!(
            ws.remote_name(&format!(" {} ", X.to_uppercase())),
            Some("Office PC")
        );
        assert!(ws.set_remote_name(X, "Home", 2));
        assert_eq!(ws.remote_name(X), Some("Home"));
        assert_eq!(ws.remote_names.len(), 1);
        // An empty name removes it.
        assert!(ws.set_remote_name(X, "   ", 3));
        assert_eq!(ws.remote_name(X), None);
        assert!(ws.remote_names.is_empty());
        // Unusable IDs are refused.
        assert!(!ws.set_remote_name("", "x", 4));
        assert!(!ws.set_remote_name(&"a".repeat(65), "x", 4));
    }

    #[test]
    fn remote_names_are_capped_in_length_and_count() {
        let mut ws = Workspace::default();
        assert!(ws.set_remote_name("id", &"é".repeat(100), 1));
        assert_eq!(
            ws.remote_name("id").unwrap().chars().count(),
            super::super::MAX_REMOTE_NAME_CHARS
        );
        let mut ws = Workspace::default();
        for i in 0..MAX_REMOTE_NAMES {
            assert!(ws.set_remote_name(&format!("id-{i}"), "n", 1));
        }
        assert!(!ws.set_remote_name("one-too-many", "n", 1));
        // Renaming and removing still work when full.
        assert!(ws.set_remote_name("id-0", "renamed", 2));
        assert!(ws.set_remote_name("id-1", "", 2));
        assert!(ws.set_remote_name("one-more", "n", 2));
    }

    #[test]
    fn an_older_workspace_syncs_without_losing_order_or_names() {
        let mut ws: Workspace = serde_json::from_str(&format!(
            r#"{{"recentIds":["{Y}","{X}"],"remoteNames":{{"{X}":"Office"}}}}"#
        ))
        .unwrap();
        ws.normalize(u64::MAX);
        let doc = ws.sync_doc(true, true);
        assert_eq!(doc.recents(), vec![Y, X]);
        assert_eq!(doc.entries[X].name, "Office");
        assert_eq!(doc.entries[X].name_at, 1);
        // Applying its own document changes nothing visible.
        let before = (ws.recent_ids.clone(), ws.remote_names.clone());
        ws.apply_sync_doc(&doc, now_ms(), true, true);
        assert_eq!((ws.recent_ids.clone(), ws.remote_names.clone()), before);
    }

    #[test]
    fn two_computers_converge_on_names_recents_and_removals() {
        let mut server = SyncDoc::default();
        let mut a = Workspace::default();
        let mut b = Workspace::default();

        a.add_recent(X, 100);
        a.set_remote_name(X, "Office", 110);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(b.recent_ids, vec![X]);
        assert_eq!(b.remote_name(X), Some("Office"));

        // A later rename on B beats the older name on A.
        b.set_remote_name(X, "Work PC", 200);
        a.add_recent(Y, 210);
        sync(&mut server, &mut b);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(a.remote_name(X), Some("Work PC"));
        assert_eq!(a.recent_ids, vec![Y, X]);
        assert_eq!(b.recent_ids, a.recent_ids);

        // Removing the name on A removes it on B, and B's stale copy of it
        // does not bring it back.
        a.set_remote_name(X, "", 300);
        sync(&mut server, &mut b); // B syncs before hearing of the removal
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(a.remote_name(X), None);
        assert_eq!(b.remote_name(X), None);
        assert!(b.sync_doc(true, true).entries[X].name_at >= 300);
    }

    #[test]
    fn clearing_recents_reaches_other_computers_but_keeps_names() {
        let mut server = SyncDoc::default();
        let mut a = Workspace::default();
        let mut b = Workspace::default();
        a.add_recent(X, 100);
        a.add_recent(Y, 110);
        a.set_remote_name(X, "Office", 120);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(b.recent_ids, vec![Y, X]);

        b.clear_recents(130);
        sync(&mut server, &mut b);
        sync(&mut server, &mut a);
        assert!(a.recent_ids.is_empty());
        assert_eq!(a.remote_name(X), Some("Office"));

        // A connection after the clear is a recent again, everywhere.
        a.add_recent(Y, 140);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(b.recent_ids, vec![Y]);
    }

    #[test]
    fn renaming_to_the_same_name_keeps_the_older_stamp() {
        let mut ws = Workspace::default();
        ws.set_remote_name(X, "Office", 10);
        ws.set_remote_name(X, " Office ", 99);
        assert_eq!(ws.id_stamps[X].name_at, 10);
    }

    #[test]
    fn stamps_are_pruned_to_listed_ids_and_removed_names() {
        let mut ws = Workspace::default();
        for i in 0..(MAX_RECENTS as u64 + 5) {
            ws.add_recent(&format!("id-{i}"), i + 1);
        }
        assert_eq!(ws.id_stamps.len(), MAX_RECENTS);
        ws.set_remote_name("gone", "Old", 1);
        ws.set_remote_name("gone", "", 2);
        assert_eq!(ws.id_stamps["gone"].name_at, 2);
        ws.clear_recents(3);
        assert_eq!(ws.id_stamps.len(), 1);
    }

    #[test]
    fn normalize_cleans_hand_edited_names_and_clamps_stamps() {
        let mut ws: Workspace = serde_json::from_str(
            r#"{"remoteNames":{"ABC":"  Desk  top ","empty":"   ","x":"ok"},
                "idStamps":{"ABC":{"nameAt":99999}},"recentsClearedAt":99999,
                "prefStamps":{"clipboardSync":99999,"bogus":1}}"#,
        )
        .unwrap();
        ws.normalize(500);
        assert_eq!(ws.remote_names.len(), 2);
        assert_eq!(ws.remote_name("abc"), Some("Desk top"));
        assert_eq!(ws.remote_name("x"), Some("ok"));
        assert_eq!(ws.id_stamps["abc"].name_at, 500);
        assert_eq!(ws.recents_cleared_at, 500);
        assert_eq!(ws.pref_stamps.len(), 1);
        assert_eq!(ws.pref_stamps["clipboardSync"], 500);
    }

    #[test]
    fn prefs_are_stamped_only_when_they_change() {
        let mut ws = Workspace::default();
        assert!(!ws.set_pref("clipboardSync", true, 10)); // already true
        assert!(!ws.pref_stamps.contains_key("clipboardSync"));
        assert!(ws.set_pref("clipboardSync", false, 20));
        assert_eq!(ws.pref_stamps["clipboardSync"], 20);
        assert!(!ws.set_pref("clipboardSync", false, 30));
        assert_eq!(ws.pref_stamps["clipboardSync"], 20);
        assert!(!ws.set_pref("serverUrl", true, 40));
    }

    #[test]
    fn a_changed_pref_reaches_the_other_computer_but_defaults_never_override() {
        let mut server = SyncDoc::default();
        let mut a = Workspace::default();
        let mut b = Workspace::default();
        // Nobody chose anything: nothing is sent, nothing changes.
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert!(server.prefs.is_empty());

        a.set_pref("translateModifiers", false, 100);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert!(!b.prefs.translate_modifiers);

        // The later change wins, whichever computer made it.
        b.set_pref("translateModifiers", true, 200);
        a.set_pref("captureShortcuts", true, 150);
        sync(&mut server, &mut b);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert!(a.prefs.translate_modifiers && b.prefs.translate_modifiers);
        assert!(a.prefs.capture_shortcuts && b.prefs.capture_shortcuts);
    }

    #[test]
    fn prefs_are_not_sent_to_a_server_that_does_not_store_them() {
        let mut ws = Workspace::default();
        ws.set_pref("clipboardSync", false, 5);
        assert!(ws.sync_doc(true, false).prefs.is_empty());
        assert_eq!(ws.sync_doc(true, true).prefs.len(), 1);
        // An old server's reply has no prefs: the local value stays.
        let old_reply = SyncDoc::default();
        ws.apply_sync_doc(&old_reply, now_ms(), false, false);
        assert!(!ws.prefs.clipboard_sync);
        let _ = FEATURE_PREFS;
    }

    #[test]
    fn absorbing_the_local_workspace_keeps_the_accounts_prefs_and_passwords() {
        let mut account = Workspace::default();
        account.set_pref("clipboardSync", false, 50);
        account.set_remote_name(X, "Account name", 60);
        account
            .saved_logins
            .insert(X.into(), "sealed-account".into());

        let mut local = Workspace::default();
        local.set_pref("clipboardSync", true, 1); // already true: unstamped
        local.set_pref("captureShortcuts", true, 70);
        local.set_remote_name(X, "Local name", 40);
        local.set_remote_name(Y, "Laptop", 41);
        local.add_recent(Y, 42);
        local.saved_logins.insert(X.into(), "sealed-local-x".into());
        local.saved_logins.insert(Y.into(), "sealed-local-y".into());

        let rest = account.absorb_local(local, now_ms());
        assert!(!account.prefs.clipboard_sync);
        assert!(
            !account.prefs.capture_shortcuts,
            "local prefs are not merged"
        );
        assert_eq!(account.remote_name(X), Some("Account name")); // newer
        assert_eq!(account.remote_name(Y), Some("Laptop"));
        assert_eq!(account.recent_ids, vec![Y]);
        assert_eq!(account.saved_logins[X], "sealed-account");
        assert_eq!(account.saved_logins[Y], "sealed-local-y");
        assert_eq!(rest, vec!["sealed-local-x".to_string()]);
    }

    /// "Add" brings what the local workspace has, not its history: a local
    /// clear of the recents or a locally removed name must not hide or delete
    /// the account's own (P1 review M2).
    #[test]
    fn absorbing_does_not_carry_local_clears_or_removals() {
        let mut account = Workspace::default();
        account.add_recent(Y, 100);
        account.set_remote_name(Y, "Account name", 110);

        let mut local = Workspace::default();
        local.add_recent(Y, 120);
        local.clear_recents(200); // hides Y locally, at 200
        local.add_recent(X, 300); // a recent after the clear
        local.set_remote_name(Y, "Old local name", 50);
        local.set_remote_name(Y, "", 250); // removed locally, later than the account's

        account.absorb_local(local, now_ms());
        assert_eq!(account.recents_cleared_at, 0);
        assert_eq!(account.recent_ids, vec![X, Y]);
        assert_eq!(account.remote_name(Y), Some("Account name"));
    }

    #[test]
    fn closed_tabs_are_forgotten_after_ninety_days() {
        let now = 1_800_000_000_000;
        let mut ws = Workspace::default();
        let tab = |opened, closed| SyncTab {
            order: "a".into(),
            order_at: 1,
            opened_at: opened,
            closed_at: closed,
            ..SyncTab::default()
        };
        ws.tabs
            .insert(X.into(), tab(1, now - crate::sync::CLOSED_TAB_TTL_MS - 1));
        ws.tabs.insert(Y.into(), tab(1, now - 1000));
        ws.forget_closed_tabs(now);
        assert!(!ws.tabs.contains_key(X));
        assert!(ws.tabs.contains_key(Y));
    }
}
