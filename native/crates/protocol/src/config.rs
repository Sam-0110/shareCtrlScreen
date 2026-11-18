//! Client configuration schema + challenge-response (contract §5, Plan 04 §9).
//!
//! Ported from `app/main/config.js`: same schema, same defaults, same
//! normalization (fill missing keys), and the same SHA-256 challenge-response so
//! the plaintext password never crosses the wire. Persisted as JSON in the user
//! data dir (atomic tmp-file-then-rename write).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::sync::{now_ms, SyncDoc, SyncEntry};

/// Host access mode (contract §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Mode {
    #[default]
    Approve,
    Password,
}

/// Granted permission — the string `"view"` or `"control"`, never anything else
/// (contract terminology table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Permission {
    #[default]
    View,
    Control,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::View => "view",
            Permission::Control => "control",
        }
    }
}

/// One ICE server entry, passed verbatim to the transport (contract §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Persisted configuration (contract §5 + host-local extensions from config.js).
///
/// `camelCase` on the wire so the JSON file matches contract §5 byte-for-byte
/// (`serverUrl`, `passwordHash`, `passwordPermission`, `iceServers`), and
/// `default` so an older/partial file still loads instead of being discarded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Generated once, never changes — the machine's address.
    pub uuid: String,
    pub server_url: String,
    pub mode: Mode,
    /// SHA-256 hex of the password, or `None` if unset.
    pub password_hash: Option<String>,
    pub password_permission: Permission,
    pub ice_servers: Vec<IceServer>,
    // --- host-local extensions (not part of contract §5) ---
    pub share_audio: bool,
    pub share_display_id: Option<String>,
    /// Remote IDs connected to, most-recent-first (autocomplete). Capped.
    pub recent_ids: Vec<String>,
    /// Remote ID → a friendly name the user gave it ("Office PC"). Kept apart
    /// from `recent_ids`, so clearing the recents does not forget names, and a
    /// named ID stays listed. Sent nowhere unless the user signs in to an
    /// account, which then syncs it (see `sync.rs`).
    pub remote_names: BTreeMap<String, String>,
    /// Remote ID → when its name was last set or removed and when it was last
    /// connected to, on this device. Only used to merge with the account's
    /// copy (`sync.rs`), so a newer change wins whichever device made it.
    pub id_stamps: BTreeMap<String, IdStamp>,
    /// When the recents were last cleared (ms since the epoch); 0 = never.
    pub recents_cleared_at: u64,
    /// The account this device syncs its remote IDs with, if signed in.
    pub account: Option<Account>,
    /// Viewer-side: capture OS-reserved shortcuts (Alt+Tab/Win) locally.
    pub capture_shortcuts: bool,
    /// Mirror clipboard TEXT between the two machines during a session. Host
    /// side additionally requires the live control permission.
    pub clipboard_sync: bool,
    /// Host-side: paste a received file into the window it was dropped on. Off
    /// leaves it on the clipboard so the user chooses where it lands.
    pub paste_dropped_files: bool,
    /// Launch the app when the user logs in (per-user Run key). See
    /// `engine::autostart` — this is logon, not boot.
    pub start_with_windows: bool,
    /// Allow Cisco's OpenH264 binary as the software H.264 codec when this
    /// machine has no hardware one (Settings → Advanced). Cisco's licence
    /// requires the user to be able to turn it off and on again; default on.
    #[serde(rename = "useOpenH264")]
    pub use_openh264: bool,
    /// Viewer-side: translate modifier keys for the host's OS (a Mac's Cmd
    /// becomes Ctrl on a Windows/Linux host and Cmd+Tab becomes Alt+Tab; a
    /// PC's Ctrl becomes Cmd on a Mac host). Settings → Advanced, default on,
    /// as in Microsoft's Windows App; off sends every key literally.
    pub translate_modifiers: bool,
    /// Viewer-side: when a host asks for a password we already have working
    /// credentials for, submit them without prompting.
    pub auto_login: bool,
    /// Host UUID → that host's password, encrypted at rest with DPAPI (scoped to
    /// this Windows account, so the config file alone is not enough to read it).
    /// Only written after a password has actually been accepted, so a wrong
    /// guess is never persisted. Opaque here — the engine owns the crypto.
    pub saved_logins: BTreeMap<String, String>,
}

/// When this device last changed a remote ID (see [`Config::id_stamps`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IdStamp {
    /// Name set or removed (ms since the epoch); 0 = never on this device.
    pub name_at: u64,
    /// Last connected to (ms since the epoch); 0 = never on this device.
    pub used_at: u64,
}

/// A signed-in account (Settings → Account).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Account {
    /// The server's HTTPS base URL the account lives on, e.g.
    /// `https://sharectrl-signal.netameta.workers.dev`.
    pub server: String,
    pub username: String,
    /// Bearer token from sign-in. It can read and replace this account's ID
    /// list and nothing else: connecting to a machine still needs that
    /// machine's approval or password. The server keeps only its hash.
    pub token: String,
    /// Last successful sync (ms since the epoch); 0 = never.
    pub last_sync: u64,
}

const MAX_RECENTS: usize = 10;
/// Remote IDs whose name was removed, kept so the removal reaches the account.
const MAX_NAME_TOMBSTONES: usize = 200;
/// Upper bound on named remote IDs, so a hand-edited file cannot grow unbounded.
const MAX_REMOTE_NAMES: usize = 200;
/// Longest name kept, in characters.
pub const MAX_REMOTE_NAME_CHARS: usize = 40;

impl Default for Config {
    fn default() -> Self {
        Config {
            uuid: uuid::Uuid::new_v4().to_string(),
            server_url: "wss://sharectrl-signal.netameta.workers.dev/ws".to_string(),
            mode: Mode::Approve,
            password_hash: None,
            password_permission: Permission::View,
            ice_servers: vec![IceServer {
                urls: "stun:stun.l.google.com:19302".to_string(),
                username: None,
                credential: None,
            }],
            share_audio: true,
            share_display_id: None,
            recent_ids: Vec::new(),
            remote_names: BTreeMap::new(),
            id_stamps: BTreeMap::new(),
            recents_cleared_at: 0,
            account: None,
            capture_shortcuts: false,
            clipboard_sync: true,
            paste_dropped_files: true,
            start_with_windows: false,
            use_openh264: true,
            translate_modifiers: true,
            auto_login: false,
            saved_logins: BTreeMap::new(),
        }
    }
}

impl Config {
    /// SHA-256 hex of a plaintext (contract §5 / config.js `hash`).
    pub fn hash(plain: &str) -> String {
        let mut h = Sha256::new();
        h.update(plain.as_bytes());
        hex::encode(h.finalize())
    }

    /// Direct password check against the stored hash (constant-time compare).
    pub fn verify_password(&self, plain: &str) -> bool {
        match &self.password_hash {
            Some(stored) => ct_eq_hex(&Self::hash(plain), stored),
            None => false,
        }
    }

    /// Challenge-response verification (contract §3.2, config.js `verifyProof`).
    ///
    /// The viewer sends `proof = SHA256( SHA256(plaintext) + ":" + nonce )`. We
    /// hold only `SHA256(plaintext)` as `password_hash`, so we recompute the same
    /// proof and compare in constant time. Plaintext never transits the relay.
    pub fn verify_proof(&self, nonce: &str, proof: &str) -> bool {
        let stored = match &self.password_hash {
            Some(s) => s,
            None => return false,
        };
        if proof.is_empty() || proof.len() > 128 {
            return false;
        }
        let mut h = Sha256::new();
        h.update(stored.as_bytes());
        h.update(b":");
        h.update(nonce.as_bytes());
        let expected = hex::encode(h.finalize());
        ct_eq_hex(&expected, proof)
    }

    /// Set (or clear) the password, storing only its hash.
    pub fn set_password(&mut self, plain: Option<&str>) {
        self.password_hash = plain.map(Self::hash);
    }

    /// Prepend `id`, de-duplicate, cap the recents list (config.js `addRecent`).
    pub fn add_recent(&mut self, id: &str) {
        self.add_recent_at(id, now_ms());
    }

    fn add_recent_at(&mut self, id: &str, now: u64) {
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

    /// Empty the recents list (named IDs stay). Synced: clears it on every
    /// device signed in to the same account.
    pub fn clear_recents(&mut self) {
        self.recent_ids.clear();
        self.recents_cleared_at = self.recents_cleared_at.max(now_ms());
        self.prune_stamps();
    }

    /// Name (or rename) a remote ID. An empty or whitespace-only `name`
    /// removes the name. Returns false, changing nothing, when `id` is not
    /// usable or the list of names is full.
    pub fn set_remote_name(&mut self, id: &str, name: &str) -> bool {
        self.set_remote_name_at(id, name, now_ms())
    }

    fn set_remote_name_at(&mut self, id: &str, name: &str, now: u64) -> bool {
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

    /// This device's remote IDs as a sync document (`sync.rs`). IDs that were
    /// listed before this device kept stamps get tiny times, so any real
    /// change made elsewhere wins over them.
    pub fn sync_doc(&self) -> SyncDoc {
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
        // old name on other devices. Also covers a removed name on a recent.
        for (id, stamp) in &self.id_stamps {
            if stamp.name_at > 0 && !self.remote_names.contains_key(id) {
                let e: &mut SyncEntry = doc.entries.entry(id.clone()).or_default();
                e.name_at = e.name_at.max(stamp.name_at);
            }
        }
        doc
    }

    /// Merge `remote` (the account's document) into this device's remote IDs
    /// and return the merged document, which is what the account should hold.
    pub fn apply_sync_doc(&mut self, remote: &SyncDoc) -> SyncDoc {
        let mut remote = remote.clone();
        remote.normalize();
        let mut merged = self.sync_doc();
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
        merged
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

    /// The name given to `id`, if any.
    pub fn remote_name(&self, id: &str) -> Option<&str> {
        let key = remote_name_key(id)?;
        self.remote_names.get(&key).map(String::as_str)
    }

    /// Repair an untrusted/partial deserialized value in place: lower-case the
    /// UUID, clamp enums, cap recents (config.js `normalize`).
    pub fn normalize(&mut self) {
        self.uuid = self.uuid.to_lowercase();
        // Fail-safe: password mode with no hash behaves like approve (§5).
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
        for (id, stamp) in stamps {
            if let Some(key) = remote_name_key(&id) {
                self.id_stamps.insert(key, stamp);
            }
        }
        self.prune_stamps();
        // An account missing its server or token cannot sync: drop it.
        if self
            .account
            .as_ref()
            .is_some_and(|a| a.server.is_empty() || a.token.is_empty())
        {
            self.account = None;
        }
    }

    /// Effective mode honouring the §5 fail-safe: `password` with no hash ⇒
    /// treat incoming requests as `approve`.
    pub fn effective_mode(&self) -> Mode {
        if self.mode == Mode::Password && self.password_hash.is_none() {
            Mode::Approve
        } else {
            self.mode
        }
    }

    /// Load from `path`, falling back to defaults on missing/corrupt file. A
    /// corrupt file is backed up to `*.bad` before starting fresh (config.js).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<Config>(&raw) {
                Ok(mut cfg) => {
                    cfg.normalize();
                    cfg
                }
                Err(_) => {
                    let _ = std::fs::rename(path, path.with_extension("bad"));
                    let cfg = Config::default();
                    let _ = cfg.persist(path);
                    cfg
                }
            },
            Err(_) => {
                let cfg = Config::default();
                let _ = cfg.persist(path);
                cfg
            }
        }
    }

    /// Atomic write: tmp file then rename over the original (config.js `persist`).
    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp: PathBuf = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// The map key for a remote ID: trimmed and lower-cased (the UI accepts only
/// lower-case UUIDs, but a hand-edited file may not). `None` if unusable.
pub(crate) fn remote_name_key(id: &str) -> Option<String> {
    let id = id.trim();
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    Some(id.to_lowercase())
}

/// A name as stored: control characters dropped, runs of whitespace folded to
/// one space, trimmed, capped at `MAX_REMOTE_NAME_CHARS`. `None` if empty.
pub(crate) fn clean_remote_name(name: &str) -> Option<String> {
    let folded = name
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let capped: String = folded.chars().take(MAX_REMOTE_NAME_CHARS).collect();
    let capped = capped.trim_end().to_string();
    (!capped.is_empty()).then_some(capped)
}

/// Constant-time comparison of two equal-purpose hex strings. Returns false if
/// either is not valid hex or lengths differ (mirrors `timingSafeEqual` usage).
fn ct_eq_hex(a: &str, b: &str) -> bool {
    let (Ok(ab), Ok(bb)) = (hex_decode(a), hex_decode(b)) else {
        return false;
    };
    if ab.len() != bb.len() || ab.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    hex::decode(s).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_response_roundtrip() {
        let mut cfg = Config::default();
        cfg.set_password(Some("test123"));
        // Viewer computes proof = SHA256( SHA256(pw) + ":" + nonce ).
        let nonce = "0011223344556677";
        let pw_hash = Config::hash("test123");
        let mut h = Sha256::new();
        h.update(pw_hash.as_bytes());
        h.update(b":");
        h.update(nonce.as_bytes());
        let proof = hex::encode(h.finalize());

        assert!(cfg.verify_proof(nonce, &proof));
        assert!(!cfg.verify_proof(nonce, &"00".repeat(32)));
        assert!(!cfg.verify_proof("wrong-nonce", &proof));
    }

    #[test]
    fn password_mode_without_hash_is_fail_safe_approve() {
        let mut cfg = Config {
            mode: Mode::Password,
            ..Default::default()
        };
        assert_eq!(cfg.effective_mode(), Mode::Approve);
        cfg.set_password(Some("x"));
        assert_eq!(cfg.effective_mode(), Mode::Password);
    }

    #[test]
    fn recents_dedupe_and_cap() {
        let mut cfg = Config::default();
        for i in 0..15 {
            cfg.add_recent(&format!("id-{i}"));
        }
        cfg.add_recent("id-14"); // dup moves to front, no growth
        assert_eq!(cfg.recent_ids.len(), MAX_RECENTS);
        assert_eq!(cfg.recent_ids[0], "id-14");
    }

    #[test]
    fn remote_names_set_rename_and_remove() {
        let mut cfg = Config::default();
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        assert!(cfg.set_remote_name(id, "  Office \t PC\n "));
        assert_eq!(cfg.remote_name(id), Some("Office PC"));
        // Case and surrounding space in the ID do not matter.
        assert_eq!(
            cfg.remote_name(&format!(" {} ", id.to_uppercase())),
            Some("Office PC")
        );
        assert!(cfg.set_remote_name(id, "Home"));
        assert_eq!(cfg.remote_name(id), Some("Home"));
        assert_eq!(cfg.remote_names.len(), 1);
        // An empty name removes it.
        assert!(cfg.set_remote_name(id, "   "));
        assert_eq!(cfg.remote_name(id), None);
        assert!(cfg.remote_names.is_empty());
        // Unusable IDs are refused.
        assert!(!cfg.set_remote_name("", "x"));
        assert!(!cfg.set_remote_name(&"a".repeat(65), "x"));
    }

    #[test]
    fn remote_names_are_capped_in_length_and_count() {
        let mut cfg = Config::default();
        assert!(cfg.set_remote_name("id", &"é".repeat(100)));
        assert_eq!(
            cfg.remote_name("id").unwrap().chars().count(),
            MAX_REMOTE_NAME_CHARS
        );
        let mut cfg = Config::default();
        for i in 0..MAX_REMOTE_NAMES {
            assert!(cfg.set_remote_name(&format!("id-{i}"), "n"));
        }
        assert!(!cfg.set_remote_name("one-too-many", "n"));
        // Renaming and removing still work when full.
        assert!(cfg.set_remote_name("id-0", "renamed"));
        assert!(cfg.set_remote_name("id-1", ""));
        assert!(cfg.set_remote_name("one-more", "n"));
    }

    #[test]
    fn remote_names_survive_clearing_recents_and_older_files() {
        let old: Config = serde_json::from_str(r#"{"uuid":"u"}"#).unwrap();
        assert!(old.remote_names.is_empty());
        let mut cfg = Config::default();
        cfg.add_recent("abc");
        cfg.set_remote_name("abc", "Laptop");
        cfg.recent_ids.clear();
        assert_eq!(cfg.remote_name("abc"), Some("Laptop"));
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""remoteNames":{"abc":"Laptop"}"#), "{json}");
    }

    /// One sync round trip, as the app does it: the server merges the
    /// device's document into its own, the device merges the result back.
    fn sync(server: &mut SyncDoc, device: &mut Config) {
        server.merge(&device.sync_doc());
        let merged = device.apply_sync_doc(server);
        assert_eq!(&merged, server, "device and server disagree after a sync");
    }

    const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const Y: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

    #[test]
    fn an_older_config_syncs_without_losing_order_or_names() {
        let mut cfg: Config = serde_json::from_str(&format!(
            r#"{{"recentIds":["{Y}","{X}"],"remoteNames":{{"{X}":"Office"}}}}"#
        ))
        .unwrap();
        cfg.normalize();
        assert!(cfg.account.is_none());
        let doc = cfg.sync_doc();
        assert_eq!(doc.recents(), vec![Y, X]);
        assert_eq!(doc.entries[X].name, "Office");
        assert_eq!(doc.entries[X].name_at, 1);
        // Applying its own document changes nothing visible.
        let before = (cfg.recent_ids.clone(), cfg.remote_names.clone());
        cfg.apply_sync_doc(&doc);
        assert_eq!((cfg.recent_ids.clone(), cfg.remote_names.clone()), before);
    }

    #[test]
    fn two_devices_converge_on_names_recents_and_removals() {
        let mut server = SyncDoc::default();
        let mut a = Config::default();
        let mut b = Config::default();

        a.add_recent_at(X, 100);
        a.set_remote_name_at(X, "Office", 110);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(b.recent_ids, vec![X]);
        assert_eq!(b.remote_name(X), Some("Office"));

        // A later rename on B beats the older name on A.
        b.set_remote_name_at(X, "Work PC", 200);
        a.add_recent_at(Y, 210);
        sync(&mut server, &mut b);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(a.remote_name(X), Some("Work PC"));
        assert_eq!(a.recent_ids, vec![Y, X]);
        assert_eq!(b.recent_ids, a.recent_ids);

        // Removing the name on A removes it on B, and B's stale copy of it
        // does not bring it back.
        a.set_remote_name_at(X, "", 300);
        sync(&mut server, &mut b); // B syncs before hearing of the removal
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(a.remote_name(X), None);
        assert_eq!(b.remote_name(X), None);
        assert!(b.sync_doc().entries[X].name_at >= 300);
    }

    #[test]
    fn clearing_recents_reaches_other_devices_but_keeps_names() {
        let mut server = SyncDoc::default();
        let mut a = Config::default();
        let mut b = Config::default();
        a.add_recent_at(X, 100);
        a.add_recent_at(Y, 110);
        a.set_remote_name_at(X, "Office", 120);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(b.recent_ids, vec![Y, X]);

        b.clear_recents();
        sync(&mut server, &mut b);
        sync(&mut server, &mut a);
        assert!(a.recent_ids.is_empty());
        assert_eq!(a.remote_name(X), Some("Office"));

        // A connection after the clear is a recent again, everywhere.
        a.add_recent_at(Y, now_ms() + 1);
        sync(&mut server, &mut a);
        sync(&mut server, &mut b);
        assert_eq!(b.recent_ids, vec![Y]);
    }

    #[test]
    fn renaming_to_the_same_name_keeps_the_older_stamp() {
        let mut cfg = Config::default();
        cfg.set_remote_name_at(X, "Office", 10);
        cfg.set_remote_name_at(X, " Office ", 99);
        assert_eq!(cfg.id_stamps[X].name_at, 10);
    }

    #[test]
    fn stamps_are_pruned_to_listed_ids_and_removed_names() {
        let mut cfg = Config::default();
        for i in 0..(MAX_RECENTS as u64 + 5) {
            cfg.add_recent_at(&format!("id-{i}"), i + 1);
        }
        assert_eq!(cfg.id_stamps.len(), MAX_RECENTS);
        cfg.set_remote_name_at("gone", "Old", 1);
        cfg.set_remote_name_at("gone", "", 2);
        assert_eq!(cfg.id_stamps["gone"].name_at, 2);
        cfg.clear_recents();
        assert_eq!(cfg.id_stamps.len(), 1);
    }

    #[test]
    fn an_account_without_a_token_is_dropped_on_load() {
        let mut cfg: Config =
            serde_json::from_str(r#"{"account":{"server":"https://s","username":"sam"}}"#).unwrap();
        cfg.normalize();
        assert!(cfg.account.is_none());
        let mut cfg: Config = serde_json::from_str(
            r#"{"account":{"server":"https://s","username":"sam","token":"t"}}"#,
        )
        .unwrap();
        cfg.normalize();
        assert_eq!(
            cfg.account.as_ref().map(|a| a.username.as_str()),
            Some("sam")
        );
    }

    #[test]
    fn normalize_cleans_hand_edited_remote_names() {
        let mut cfg: Config = serde_json::from_str(
            r#"{"remoteNames":{"ABC":"  Desk  top ","empty":"   ","x":"ok"}}"#,
        )
        .unwrap();
        cfg.normalize();
        assert_eq!(cfg.remote_names.len(), 2);
        assert_eq!(cfg.remote_name("abc"), Some("Desk top"));
        assert_eq!(cfg.remote_name("x"), Some("ok"));
    }

    #[test]
    fn openh264_is_on_by_default_including_for_older_files() {
        assert!(Config::default().use_openh264);
        // A config written before the setting existed keeps the default.
        let old: Config = serde_json::from_str(r#"{"uuid":"u"}"#).unwrap();
        assert!(old.use_openh264);
        let off: Config = serde_json::from_str(r#"{"useOpenH264":false}"#).unwrap();
        assert!(!off.use_openh264);
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains(r#""useOpenH264":false"#), "{json}");
    }

    #[test]
    fn modifier_translation_is_on_by_default_including_for_older_files() {
        assert!(Config::default().translate_modifiers);
        let old: Config = serde_json::from_str(r#"{"uuid":"u"}"#).unwrap();
        assert!(old.translate_modifiers);
        let off: Config = serde_json::from_str(r#"{"translateModifiers":false}"#).unwrap();
        assert!(!off.translate_modifiers);
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains(r#""translateModifiers":false"#), "{json}");
    }
}
