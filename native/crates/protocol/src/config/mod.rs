//! Client configuration schema + challenge-response (contract §5, Plan 04 §9),
//! and since config version 2 the accounts and their workspaces (plans/08 P1,
//! 08a §3, §6.13).
//!
//! The file holds three kinds of thing:
//! * **device settings** (the machine's UUID, host access mode and password
//!   hash, server, capture and codec choices): never synced, never per-account;
//! * **accounts** signed in on this computer ([`AccountRef`]), each with its
//!   token only as an OS secret-store handle, never the token itself;
//! * **workspaces** ([`Workspace`]): one per account plus `"local"`, holding
//!   everything that belongs to the person — recents, names, tabs, synced
//!   preferences, saved host passwords.
//!
//! Persisted as JSON in the user data dir with an atomic tmp-file-then-rename
//! write. Loading a 2.1.x file migrates it once ([`migrate`]), keeping a backup;
//! a file that cannot be read is set aside and **the machine's UUID is kept**
//! whenever the old file has one (invariant I12).

mod migrate;
pub mod workspace;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::sync::{now_ms, FUTURE_SLACK_MS};
pub use workspace::{IdStamp, Prefs, TabRecord, Workspace, MAX_RECENTS, MAX_REMOTE_NAMES};

/// The layout this build writes. 1 (or absent) is the 2.1.x layout.
pub const CONFIG_VERSION: u32 = 2;
/// The workspace used without an account.
pub const LOCAL_WORKSPACE: &str = "local";
/// Longest name kept, in characters.
pub const MAX_REMOTE_NAME_CHARS: usize = 40;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// An account signed in (now or before) on this computer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AccountRef {
    /// The server's HTTPS base URL the account lives on, e.g.
    /// `https://sharectrl-signal.netameta.workers.dev`.
    pub server: String,
    pub username: String,
    /// Sign in by itself at launch ("Keep me signed in"). Only meaningful
    /// while `token_ref` is set.
    pub keep_signed_in: bool,
    /// The sign-in token, sealed by the OS secret store: an opaque handle,
    /// never the token. Set only while kept signed in.
    pub token_ref: Option<String>,
    /// Last successful sync (ms since the epoch); 0 = never.
    pub last_sync: u64,
    /// Fields a newer version added, kept so this build does not drop them.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl AccountRef {
    /// This account's key in [`Config::accounts`] and [`Config::workspaces`].
    pub fn key(&self) -> String {
        account_key(&self.server, &self.username)
    }
}

/// `"<server>|<username>"`, the server lower-cased without a trailing `/`.
pub fn account_key(server: &str, username: &str) -> String {
    format!(
        "{}|{}",
        server.trim().trim_end_matches('/').to_lowercase(),
        username.trim().to_lowercase()
    )
}

/// Persisted configuration.
///
/// `camelCase` on the wire so the device fields match contract §5 byte-for-byte
/// (`serverUrl`, `passwordHash`, `passwordPermission`, `iceServers`), and
/// `default` so a partial file still loads instead of being discarded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// The layout version ([`CONFIG_VERSION`] once written by this build).
    pub config_version: u32,
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
    /// Launch the app when the user logs in (per-user Run key). See
    /// `engine::autostart` — this is logon, not boot.
    pub start_with_windows: bool,
    /// Allow Cisco's OpenH264 binary as the software H.264 codec when this
    /// machine has no hardware one (Settings → Advanced). Cisco's licence
    /// requires the user to be able to turn it off and on again; default on.
    #[serde(rename = "useOpenH264")]
    pub use_openh264: bool,
    // --- accounts and workspaces (config version 2) ---
    /// Accounts used on this computer, in the order they were added.
    pub accounts: Vec<AccountRef>,
    /// The account this computer uses ([`AccountRef::key`]); `None` = the
    /// local workspace.
    pub active_account: Option<String>,
    /// [`LOCAL_WORKSPACE`] or an account key → its workspace.
    pub workspaces: BTreeMap<String, Workspace>,
    /// Fields this build does not know (written by a newer version), kept so
    /// rewriting the file does not drop them.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        let mut workspaces = BTreeMap::new();
        workspaces.insert(LOCAL_WORKSPACE.to_string(), Workspace::default());
        Config {
            config_version: CONFIG_VERSION,
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
            start_with_windows: false,
            use_openh264: true,
            accounts: Vec::new(),
            active_account: None,
            workspaces,
            extra: serde_json::Map::new(),
        }
    }
}

/// What [`Config::load`] found, beyond the config itself.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// False when the config must not be written back this run: a migration
    /// whose backup could not be written, or a file that could not be read
    /// for a reason other than not existing. The app then runs on what it has
    /// in memory rather than risk overwriting the user's data.
    pub writable: bool,
    /// `(account key, token)`: a 2.1.x token to move into the secret store
    /// (08a §6.13 step 3). Never written to the file.
    pub legacy_token: Option<(String, String)>,
    /// Sealed secrets the migration did not carry over, for the caller to
    /// delete from the secret store.
    pub forget_sealed: Vec<String>,
    /// Whether this load wrote the file (a migration or a fresh one). The
    /// secrets in `forget_sealed` may be deleted only then: until the new file
    /// is on disk, the old one still refers to them.
    pub persisted: bool,
    /// What happened, for the log (no secrets).
    pub notes: Vec<String>,
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

    /// Effective mode honouring the §5 fail-safe: `password` with no hash ⇒
    /// treat incoming requests as `approve`.
    pub fn effective_mode(&self) -> Mode {
        if self.mode == Mode::Password && self.password_hash.is_none() {
            Mode::Approve
        } else {
            self.mode
        }
    }

    /// The account with `key`, if this computer knows it.
    pub fn account(&self, key: &str) -> Option<&AccountRef> {
        self.accounts.iter().find(|a| a.key() == key)
    }

    /// The account with `key`, mutable.
    pub fn account_mut(&mut self, key: &str) -> Option<&mut AccountRef> {
        self.accounts.iter_mut().find(|a| a.key() == key)
    }

    /// The workspace stored under `key`, created empty if absent.
    pub fn workspace_mut(&mut self, key: &str) -> &mut Workspace {
        self.workspaces.entry(key.to_string()).or_default()
    }

    /// The latest stamp anywhere in the file, so a stamper continues after it.
    pub fn latest_stamp(&self) -> u64 {
        self.workspaces
            .values()
            .map(Workspace::latest_stamp)
            .max()
            .unwrap_or(0)
    }

    /// Repair an untrusted/partial value in place: lower-case the UUID (or
    /// generate one if there is none at all), drop unusable or duplicate
    /// accounts, make sure the local workspace exists, and repair every
    /// workspace with stamps clamped to `now + FUTURE_SLACK_MS`.
    pub fn normalize(&mut self, now: u64) {
        self.uuid = self.uuid.trim().to_lowercase();
        if self.uuid.is_empty() {
            // Only ever reached when no file had a UUID (I12).
            self.uuid = uuid::Uuid::new_v4().to_string();
        }
        let mut seen = std::collections::BTreeSet::new();
        self.accounts.retain(|a| {
            !a.server.trim().is_empty() && !a.username.trim().is_empty() && seen.insert(a.key())
        });
        if let Some(active) = &self.active_account {
            if self.account(active).is_none() {
                self.active_account = None;
            }
        }
        self.workspaces
            .entry(LOCAL_WORKSPACE.to_string())
            .or_default();
        let max = now.saturating_add(FUTURE_SLACK_MS);
        for ws in self.workspaces.values_mut() {
            ws.normalize(max);
        }
    }

    /// Load from `path`. Never loses the machine's UUID (I12) and never
    /// overwrites a file it could not make sense of without keeping a copy:
    /// * missing → defaults, written out;
    /// * unreadable (permissions, a lock) → defaults in memory, **not** written;
    /// * not valid JSON, or not a config → moved to `*.bad`; defaults with the
    ///   old UUID if one can be found in it, written out;
    /// * a 2.1.x file → migrated ([`migrate`]): a copy without the account
    ///   token is kept as `config.v1.json`, then the new layout is written; if
    ///   the copy cannot be written, the migration runs in memory only;
    /// * written by a newer version → read as far as understood; a copy is
    ///   kept as `config.v<N>.json` before this build rewrites it.
    pub fn load(path: &Path) -> Loaded {
        Self::load_at(path, now_ms())
    }

    fn load_at(path: &Path, now: u64) -> Loaded {
        let mut out = Loaded {
            config: Config::default(),
            writable: true,
            legacy_token: None,
            forget_sealed: Vec::new(),
            notes: Vec::new(),
            persisted: false,
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out.config.normalize(now);
                out.persisted = out.persist_or_note(path);
                return out;
            }
            Err(e) => {
                out.writable = false;
                out.notes.push(format!(
                    "config: could not read {} ({e}); running on defaults without saving",
                    path.display()
                ));
                out.config.normalize(now);
                return out;
            }
        };

        let value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                out.notes.push(format!(
                    "config: not valid JSON ({e}); set aside as config.bad"
                ));
                return Self::recover(path, salvage_uuid_text(&raw), now, out);
            }
        };
        let version = value
            .get("configVersion")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);

        if version < u64::from(CONFIG_VERSION) {
            let v1 = match serde_json::from_value::<migrate::ConfigV1>(value.clone()) {
                Ok(v1) => v1,
                Err(e) => {
                    out.notes.push(format!(
                        "config: not a readable 2.1.x config ({e}); set aside as config.bad"
                    ));
                    return Self::recover(path, salvage_uuid_value(&value), now, out);
                }
            };
            let migrated = migrate::v1_to_v2(v1, now);
            out.config = migrated.config;
            out.config.normalize(now);
            out.legacy_token = migrated.legacy_token;
            out.forget_sealed = migrated.forget;
            out.notes
                .push("config: migrated from the 2.1.x layout".to_string());
            // A 2.1.x file next to a copy of this build's layout means the app
            // was downgraded and upgraded again: the older version dropped the
            // accounts and workspaces it did not know. Bring them back, with
            // what the older version recorded meanwhile merged in.
            if let Some(kept) = read_v2_copy(path, &out.config.uuid, now) {
                out.config = restore_after_downgrade(kept, out.config, now);
                out.notes.push(
                    "config: restored the accounts and workspaces kept from before a downgrade"
                        .to_string(),
                );
            }
            match write_backup(path, "v1", &without_token(value)) {
                Ok(()) => {
                    out.persisted = out.persist_or_note(path);
                    if out.persisted {
                        if let Ok(v) = serde_json::to_value(&out.config) {
                            if let Err(e) = write_copy(path, V2_COPY, &v) {
                                out.notes
                                    .push(format!("config: could not write {V2_COPY} ({e})"));
                            }
                        }
                    }
                }
                Err(e) => {
                    out.writable = false;
                    out.notes.push(format!(
                        "config: could not keep a copy of the old file ({e}); \
                         running on the migrated values without saving"
                    ));
                }
            }
            return out;
        }

        if version > u64::from(CONFIG_VERSION) {
            out.notes.push(format!(
                "config: written by a newer version (layout {version}); \
                 keeping a copy as config.v{version}.json"
            ));
            if let Err(e) = write_backup(path, &format!("v{version}"), &value) {
                out.writable = false;
                out.notes.push(format!(
                    "config: could not keep that copy ({e}); running without saving"
                ));
            }
        }
        match serde_json::from_value::<Config>(value.clone()) {
            Ok(mut cfg) => {
                cfg.config_version = CONFIG_VERSION;
                cfg.normalize(now);
                // Kept for a re-upgrade after a downgrade (see above): a 2.1.x
                // build rewrites this file without the fields it does not know.
                if let Err(e) = write_copy(path, V2_COPY, &value) {
                    out.notes
                        .push(format!("config: could not refresh {V2_COPY} ({e})"));
                }
                out.config = cfg;
                out
            }
            Err(e) => {
                out.notes.push(format!(
                    "config: not a readable config ({e}); set aside as config.bad"
                ));
                Self::recover(path, salvage_uuid_value(&value), now, out)
            }
        }
    }

    /// The file is unusable: move it aside to `*.bad` and start from defaults,
    /// keeping its UUID when one was found.
    fn recover(path: &Path, uuid: Option<String>, now: u64, mut out: Loaded) -> Loaded {
        if let Err(e) = std::fs::rename(path, path.with_extension("bad")) {
            // Without the copy, writing would destroy the only one.
            out.writable = false;
            out.notes.push(format!(
                "config: could not set the unreadable file aside ({e}); running without saving"
            ));
        }
        out.config = Config::default();
        if let Some(uuid) = uuid {
            out.config.uuid = uuid;
        }
        out.config.normalize(now);
        if out.writable {
            out.persisted = out.persist_or_note(path);
        }
        out
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

impl Loaded {
    /// Save, noting a failure. Returns whether the file was written.
    fn persist_or_note(&mut self, path: &Path) -> bool {
        match self.config.persist(path) {
            Ok(()) => true,
            Err(e) => {
                self.notes
                    .push(format!("config: could not save {} ({e})", path.display()));
                false
            }
        }
    }
}

/// A copy of this build's layout, refreshed at every launch, for a re-upgrade
/// after a downgrade: 2.1.x reads the file (its UUID survives) but rewrites it
/// without `accounts` and `workspaces`, which it does not know.
const V2_COPY: &str = "config.v2.json";

/// Write `value` next to `path` as `name`, replacing any older copy
/// (tmp file then rename, so a crash never leaves half a copy).
fn write_copy(path: &Path, name: &str, value: &serde_json::Value) -> std::io::Result<()> {
    let copy = path.with_file_name(name);
    let tmp = copy.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, &copy)
}

/// The kept copy of this build's layout, if there is a readable one for this
/// machine (same UUID).
fn read_v2_copy(path: &Path, uuid: &str, now: u64) -> Option<Config> {
    let raw = std::fs::read_to_string(path.with_file_name(V2_COPY)).ok()?;
    let mut kept: Config = serde_json::from_str(&raw).ok()?;
    kept.normalize(now);
    (kept.uuid == uuid).then_some(kept)
}

/// Rebuild after a downgrade and re-upgrade: the kept accounts and
/// workspaces, the device settings as the older version last saved them, and
/// what it recorded meanwhile (recents, names, saved passwords) merged into
/// the workspace it belongs to by the sync rules — nothing is dropped.
fn restore_after_downgrade(mut kept: Config, migrated: Config, now: u64) -> Config {
    kept.server_url = migrated.server_url;
    kept.mode = migrated.mode;
    kept.password_hash = migrated.password_hash;
    kept.password_permission = migrated.password_permission;
    kept.ice_servers = migrated.ice_servers;
    kept.share_audio = migrated.share_audio;
    kept.share_display_id = migrated.share_display_id;
    kept.start_with_windows = migrated.start_with_windows;
    kept.use_openh264 = migrated.use_openh264;
    for account in migrated.accounts {
        if kept.account(&account.key()).is_none() {
            kept.accounts.push(account);
        }
    }
    for (key, meanwhile) in migrated.workspaces {
        // A password the older version saved for a host this workspace already
        // has one for is left alone: its secret may be the very same item.
        let _ = kept.workspace_mut(&key).absorb_local(meanwhile, now);
    }
    if migrated.active_account.is_some() {
        kept.active_account = migrated.active_account;
    }
    kept.config_version = CONFIG_VERSION;
    kept.normalize(now);
    kept
}

/// Write `value` next to `path` as `config.<tag>.json`, once: an existing copy
/// is the older one and is kept.
fn write_backup(path: &Path, tag: &str, value: &serde_json::Value) -> std::io::Result<()> {
    let backup = path.with_file_name(format!("config.{tag}.json"));
    if backup.exists() {
        return Ok(());
    }
    let tmp = backup.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, &backup)
}

/// A 2.1.x file without its account token: the backup keeps the user's data,
/// not a credential (the token moves to the secret store).
fn without_token(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(account) = value
        .get_mut("account")
        .and_then(serde_json::Value::as_object_mut)
    {
        account.remove("token");
    }
    value
}

/// The UUID in a parsed-but-unusable file, if it has a valid one.
fn salvage_uuid_value(value: &serde_json::Value) -> Option<String> {
    let uuid = value.get("uuid")?.as_str()?.trim().to_lowercase();
    crate::is_valid_uuid_v4(&uuid).then_some(uuid)
}

/// The UUID in a file that is not even JSON (truncated, half-written): the
/// first `"uuid"` key followed by a quoted valid UUID.
fn salvage_uuid_text(raw: &str) -> Option<String> {
    let after = &raw[raw.find("\"uuid\"")? + "\"uuid\"".len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let quoted = after.strip_prefix('"')?;
    let uuid = quoted.get(..36)?.to_lowercase();
    crate::is_valid_uuid_v4(&uuid).then_some(uuid)
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
        // U+FEFF too: JavaScript's `\s` folds it (server/src/sync.js), Rust's
        // whitespace does not, and the two sides must clean names alike.
        .split(|c: char| c.is_whitespace() || c.is_control() || c == '\u{feff}')
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

    const NOW: u64 = 1_800_000_000_000;
    const U: &str = "1c9a7b3e-8f21-4d6a-9e0b-2f4c8a1d5e73";
    const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sct-config-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

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
    fn account_keys_ignore_case_and_a_trailing_slash() {
        assert_eq!(
            account_key("https://S.example/", " Sam "),
            "https://s.example|sam"
        );
    }

    #[test]
    fn openh264_is_on_by_default_including_for_partial_files() {
        assert!(Config::default().use_openh264);
        let old: Config = serde_json::from_str(r#"{"uuid":"u"}"#).unwrap();
        assert!(old.use_openh264);
        let off: Config = serde_json::from_str(r#"{"useOpenH264":false}"#).unwrap();
        assert!(!off.use_openh264);
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains(r#""useOpenH264":false"#), "{json}");
    }

    #[test]
    fn unknown_fields_survive_a_rewrite() {
        let cfg: Config =
            serde_json::from_str(r#"{"configVersion":2,"uuid":"u","fromTheFuture":{"a":1}}"#)
                .unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""fromTheFuture":{"a":1}"#), "{json}");
    }

    #[test]
    fn normalize_drops_bad_accounts_and_a_dangling_active_account() {
        let mut cfg: Config = serde_json::from_str(
            r#"{"configVersion":2,"uuid":" ABC ","accounts":[
                {"server":"https://s","username":"sam"},
                {"server":"https://S/","username":"SAM"},
                {"server":"","username":"x"}],
               "activeAccount":"https://s|nobody","workspaces":{}}"#,
        )
        .unwrap();
        cfg.normalize(NOW);
        assert_eq!(cfg.uuid, "abc");
        assert_eq!(cfg.accounts.len(), 1);
        assert!(cfg.active_account.is_none());
        assert!(cfg.workspaces.contains_key(LOCAL_WORKSPACE));
    }

    #[test]
    fn a_missing_file_gets_defaults_written() {
        let d = dir("missing");
        let path = d.join("config.json");
        let l = Config::load_at(&path, NOW);
        assert!(l.writable && path.exists());
        assert!(crate::is_valid_uuid_v4(&l.config.uuid));
    }

    #[test]
    fn a_corrupt_file_is_set_aside_and_its_uuid_kept() {
        let d = dir("corrupt");
        let path = d.join("config.json");
        std::fs::write(&path, format!(r#"{{"uuid": "{U}", "recentIds": ["#)).unwrap();
        let l = Config::load_at(&path, NOW);
        assert_eq!(l.config.uuid, U, "the machine's ID must survive (I12)");
        assert!(d.join("config.bad").exists());
        let again = Config::load_at(&path, NOW);
        assert_eq!(again.config.uuid, U);
    }

    #[test]
    fn a_file_that_is_json_but_not_a_config_keeps_its_uuid() {
        let d = dir("wrong-shape");
        let path = d.join("config.json");
        std::fs::write(&path, format!(r#"{{"uuid":"{U}","recentIds":"oops"}}"#)).unwrap();
        let l = Config::load_at(&path, NOW);
        assert_eq!(l.config.uuid, U);
        assert!(d.join("config.bad").exists());
    }

    /// A 2.1.10 `config.json`, as that version wrote it (values replaced; the
    /// token and sealed passwords are fake). Every name, recent and saved host
    /// password survives, in the account's workspace.
    #[test]
    fn a_2_1_x_file_is_migrated_with_a_backup_and_nothing_lost() {
        let d = dir("migrate");
        let path = d.join("config.json");
        let old = include_str!("../../testdata/config/v1-2.1.10.json");
        std::fs::write(&path, old).unwrap();

        let l = Config::load_at(&path, NOW);
        assert!(l.writable);
        let c = &l.config;
        assert_eq!(c.uuid, "5b1c0d2e-7f3a-4c8b-9d6e-1a2b3c4d5e6f");
        assert_eq!(c.config_version, CONFIG_VERSION);
        assert_eq!(c.mode, Mode::Password);
        assert_eq!(
            c.server_url,
            "wss://sharectrl-signal.netameta.workers.dev/ws"
        );
        let key = "https://sharectrl-signal.netameta.workers.dev|sam";
        assert_eq!(c.active_account.as_deref(), Some(key));
        let ws = &c.workspaces[key];
        assert_eq!(ws.recent_ids.len(), 3);
        assert_eq!(ws.remote_name(X), Some("Office PC"));
        assert_eq!(ws.remote_names.len(), 2);
        assert_eq!(ws.saved_logins.len(), 2);
        assert!(!ws.prefs.clipboard_sync);
        assert_eq!(l.legacy_token.as_ref().map(|(k, _)| k.as_str()), Some(key));

        // The new file has no token; the backup has the data but not the token.
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(!new.contains("sct1.sam."), "{new}");
        let backup = std::fs::read_to_string(d.join("config.v1.json")).unwrap();
        assert!(backup.contains("Office PC"));
        assert!(!backup.contains("sct1.sam."), "{backup}");

        // Loading again does not migrate again, and keeps everything.
        let again = Config::load_at(&path, NOW);
        assert!(again.legacy_token.is_none());
        assert_eq!(again.config.workspaces[key], *ws);
        assert_eq!(again.config.uuid, c.uuid);
    }

    /// An earlier 2.1.x file (no names, no account), the shape of a real
    /// install's: everything goes to the local workspace; the one preference
    /// changed from its default is kept and stamped.
    #[test]
    fn an_early_2_1_x_file_migrates_to_the_local_workspace() {
        let d = dir("migrate-early");
        let path = d.join("config.json");
        std::fs::write(&path, include_str!("../../testdata/config/v1-2.1.8.json")).unwrap();
        let l = Config::load_at(&path, NOW);
        let c = &l.config;
        assert_eq!(c.uuid, "a3e1c6f0-2b4d-4e8f-9a1b-3c5d7e9f0b2d");
        assert!(c.accounts.is_empty() && c.active_account.is_none());
        assert!(l.legacy_token.is_none() && l.forget_sealed.is_empty());
        let ws = &c.workspaces[LOCAL_WORKSPACE];
        assert_eq!(ws.recent_ids, vec![X]);
        assert!(ws.prefs.capture_shortcuts);
        assert_eq!(ws.pref_stamps.get("captureShortcuts"), Some(&NOW));
        assert_eq!(ws.pref_stamps.len(), 1);
        assert!(d.join("config.v1.json").exists());
    }

    #[test]
    fn a_file_from_a_newer_version_is_copied_before_it_is_rewritten() {
        let d = dir("newer");
        let path = d.join("config.json");
        std::fs::write(
            &path,
            format!(r#"{{"configVersion":3,"uuid":"{U}","somethingNew":true}}"#),
        )
        .unwrap();
        let l = Config::load_at(&path, NOW);
        assert_eq!(l.config.uuid, U);
        assert!(d.join("config.v3.json").exists());
        assert_eq!(
            l.config.extra.get("somethingNew"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn loaded_stamps_are_clamped() {
        let d = dir("clamp");
        let path = d.join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"configVersion":2,"uuid":"{U}","workspaces":{{"local":{{
                    "recentIds":["{X}"],"idStamps":{{"{X}":{{"usedAt":99999999999999}}}}}}}}}}"#
            ),
        )
        .unwrap();
        let l = Config::load_at(&path, NOW);
        assert_eq!(
            l.config.workspaces[LOCAL_WORKSPACE].id_stamps[X].used_at,
            NOW + FUTURE_SLACK_MS
        );
    }

    /// A downgrade to 2.1.x rewrites the file without the accounts and
    /// workspaces; upgrading again brings them back from the kept copy, with
    /// what the older version recorded meanwhile merged in (P1 review M4).
    #[test]
    fn accounts_and_workspaces_survive_a_downgrade_and_re_upgrade() {
        const Y: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let d = dir("downgrade");
        let path = d.join("config.json");
        std::fs::write(&path, include_str!("../../testdata/config/v1-2.1.10.json")).unwrap();
        let first = Config::load_at(&path, NOW);
        let key = first.config.active_account.clone().unwrap();
        // This build runs, and later launches keep the copy fresh.
        let mut cfg = first.config;
        cfg.workspace_mut(&key)
            .set_remote_name(Y, "Added in 2.2", NOW);
        cfg.persist(&path).unwrap();
        let _ = Config::load_at(&path, NOW);
        assert!(d.join(V2_COPY).exists());

        // 2.1.x rewrites the file in its own layout: same UUID, one new recent,
        // no accounts or workspaces.
        let x = "886313e1-3b8a-4372-9b90-0c9aee199e5d";
        std::fs::write(
            &path,
            format!(
                r#"{{"uuid":"{}","recentIds":["{x}"],"idStamps":{{"{x}":{{"usedAt":{}}}}}}}"#,
                cfg.uuid,
                NOW + 1
            ),
        )
        .unwrap();

        let back = Config::load_at(&path, NOW + 10);
        let c = &back.config;
        assert_eq!(c.uuid, cfg.uuid);
        assert!(c.account(&key).is_some(), "the account is back");
        let ws = &c.workspaces[&key];
        assert_eq!(ws.remote_name(Y), Some("Added in 2.2"));
        assert_eq!(ws.remote_name(X), Some("Office PC"));
        assert_eq!(ws.saved_logins.len(), 2);
        // What 2.1.x recorded meanwhile is there too (it had no account, so
        // it went to the local workspace).
        assert_eq!(c.workspaces[LOCAL_WORKSPACE].recent_ids, vec![x]);
    }
}
