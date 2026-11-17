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
    /// Remote ID → a friendly name the user gave it ("Office PC"). Local only,
    /// never sent anywhere. Kept apart from `recent_ids`, so clearing the
    /// recents does not forget names, and a named ID stays listed.
    pub remote_names: BTreeMap<String, String>,
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

const MAX_RECENTS: usize = 10;
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
        let clean = id.trim();
        if clean.is_empty() || clean.len() > 64 {
            return;
        }
        self.recent_ids.retain(|x| x != clean);
        self.recent_ids.insert(0, clean.to_string());
        self.recent_ids.truncate(MAX_RECENTS);
    }

    /// Name (or rename) a remote ID. An empty or whitespace-only `name`
    /// removes the name. Returns false, changing nothing, when `id` is not
    /// usable or the list of names is full.
    pub fn set_remote_name(&mut self, id: &str, name: &str) -> bool {
        let Some(key) = remote_name_key(id) else {
            return false;
        };
        match clean_remote_name(name) {
            None => {
                self.remote_names.remove(&key);
            }
            Some(name) => {
                if !self.remote_names.contains_key(&key)
                    && self.remote_names.len() >= MAX_REMOTE_NAMES
                {
                    return false;
                }
                self.remote_names.insert(key, name);
            }
        }
        true
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
fn remote_name_key(id: &str) -> Option<String> {
    let id = id.trim();
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    Some(id.to_lowercase())
}

/// A name as stored: control characters dropped, runs of whitespace folded to
/// one space, trimmed, capped at `MAX_REMOTE_NAME_CHARS`. `None` if empty.
fn clean_remote_name(name: &str) -> Option<String> {
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
