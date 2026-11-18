//! The one-time move from the 2.1.x `config.json` (no `configVersion`) to the
//! account-workspace layout (plans/08a §6.13). Pure: no I/O, no secret store,
//! no clock — [`Config::load`](super::Config::load) does the file work around
//! it and the engine moves the token into the secret store afterwards.
//!
//! What moves where:
//! * device settings stay on the device;
//! * the recents, names, their stamps and the saved host passwords go to the
//!   workspace of the account the file was signed in to (if it has a token),
//!   else to `"local"`;
//! * saved host passwords are **forgotten**, not moved, when automatic sign-in
//!   was off: retiring that toggle must not start using passwords the user had
//!   told the app not to use;
//! * the four preferences that now follow the account keep their values; a
//!   value that differs from the default is stamped with the migration time,
//!   so it is a choice the account learns, while an untouched default is not
//!   sent and never overrides a choice made on another computer;
//! * the account's plaintext token is handed back to the caller and never
//!   written into the new file.

use serde::Deserialize;
use std::collections::BTreeMap;

use super::workspace::{IdStamp, Prefs, Workspace};
use super::{account_key, AccountRef, Config, IceServer, Mode, Permission, LOCAL_WORKSPACE};
use crate::sync::SYNCED_PREFS;

/// The 2.1.x file, exactly as that version wrote it.
#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ConfigV1 {
    uuid: String,
    server_url: String,
    mode: Mode,
    password_hash: Option<String>,
    password_permission: Permission,
    ice_servers: Vec<IceServer>,
    share_audio: bool,
    share_display_id: Option<String>,
    recent_ids: Vec<String>,
    remote_names: BTreeMap<String, String>,
    id_stamps: BTreeMap<String, IdStamp>,
    recents_cleared_at: u64,
    account: Option<AccountV1>,
    capture_shortcuts: bool,
    clipboard_sync: bool,
    paste_dropped_files: bool,
    start_with_windows: bool,
    #[serde(rename = "useOpenH264")]
    use_openh264: bool,
    translate_modifiers: bool,
    auto_login: bool,
    saved_logins: BTreeMap<String, String>,
}

impl Default for ConfigV1 {
    /// 2.1.x's defaults, so a partial old file reads as that version read it.
    /// An absent `uuid` stays empty: the caller generates one only then.
    fn default() -> Self {
        let d = Config::default();
        let p = Prefs::default();
        ConfigV1 {
            uuid: String::new(),
            server_url: d.server_url,
            mode: d.mode,
            password_hash: None,
            password_permission: d.password_permission,
            ice_servers: d.ice_servers,
            share_audio: d.share_audio,
            share_display_id: None,
            recent_ids: Vec::new(),
            remote_names: BTreeMap::new(),
            id_stamps: BTreeMap::new(),
            recents_cleared_at: 0,
            account: None,
            capture_shortcuts: p.capture_shortcuts,
            clipboard_sync: p.clipboard_sync,
            paste_dropped_files: p.paste_dropped_files,
            start_with_windows: d.start_with_windows,
            use_openh264: d.use_openh264,
            translate_modifiers: p.translate_modifiers,
            auto_login: false,
            saved_logins: BTreeMap::new(),
        }
    }
}

/// 2.1.x's signed-in account, token included.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AccountV1 {
    server: String,
    username: String,
    token: String,
    last_sync: u64,
}

/// The result of [`v1_to_v2`].
#[derive(Debug)]
pub(crate) struct Migrated {
    pub config: Config,
    /// The account's token, to be sealed into the secret store (08a §6.13
    /// step 3). Never written to the new file.
    pub legacy_token: Option<(String, String)>,
    /// Sealed host passwords that were not carried over (automatic sign-in
    /// was off), for the caller to delete from the secret store.
    pub forget: Vec<String>,
}

/// Build the v2 config from a 2.1.x one. `now` stamps the preferences that
/// were changed from their defaults.
pub(crate) fn v1_to_v2(v1: ConfigV1, now: u64) -> Migrated {
    let prefs = Prefs {
        translate_modifiers: v1.translate_modifiers,
        capture_shortcuts: v1.capture_shortcuts,
        clipboard_sync: v1.clipboard_sync,
        paste_dropped_files: v1.paste_dropped_files,
    };
    let defaults = Prefs::default();
    let pref_stamps = SYNCED_PREFS
        .iter()
        .filter(|k| prefs.get(k) != defaults.get(k))
        .map(|k| (k.to_string(), now))
        .collect();

    let (saved_logins, forget) = if v1.auto_login {
        (v1.saved_logins, Vec::new())
    } else {
        (BTreeMap::new(), v1.saved_logins.into_values().collect())
    };

    let workspace = Workspace {
        recent_ids: v1.recent_ids,
        remote_names: v1.remote_names,
        id_stamps: v1.id_stamps,
        recents_cleared_at: v1.recents_cleared_at,
        prefs,
        pref_stamps,
        saved_logins,
        ..Workspace::default()
    };

    let mut config = Config {
        uuid: v1.uuid,
        server_url: v1.server_url,
        mode: v1.mode,
        password_hash: v1.password_hash,
        password_permission: v1.password_permission,
        ice_servers: v1.ice_servers,
        share_audio: v1.share_audio,
        share_display_id: v1.share_display_id,
        start_with_windows: v1.start_with_windows,
        use_openh264: v1.use_openh264,
        ..Config::default()
    };
    config
        .workspaces
        .insert(LOCAL_WORKSPACE.into(), Workspace::default());

    let account = v1
        .account
        .filter(|a| !a.server.is_empty() && !a.username.is_empty() && !a.token.is_empty());
    let legacy_token = match account {
        Some(a) => {
            let key = account_key(&a.server, &a.username);
            config.accounts.push(AccountRef {
                server: a.server,
                username: a.username,
                // The file kept them signed in: keep doing so, if the token
                // can be sealed (otherwise the engine turns this off).
                keep_signed_in: true,
                token_ref: None,
                last_sync: a.last_sync,
            });
            config.active_account = Some(key.clone());
            config.workspaces.insert(key.clone(), workspace);
            Some((key, a.token))
        }
        None => {
            config.workspaces.insert(LOCAL_WORKSPACE.into(), workspace);
            None
        }
    };

    Migrated {
        config,
        legacy_token,
        forget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    fn v1(json: &str) -> ConfigV1 {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_signed_in_file_moves_into_the_accounts_workspace() {
        let m = v1_to_v2(
            v1(&format!(
                r#"{{"uuid":"u-1","recentIds":["{X}"],"remoteNames":{{"{X}":"Office"}},
                    "account":{{"server":"https://S.example/","username":"sam","token":"t0k","lastSync":7}},
                    "autoLogin":true,"savedLogins":{{"{X}":"sealed"}}}}"#
            )),
            1000,
        );
        let key = "https://s.example|sam";
        assert_eq!(m.config.uuid, "u-1");
        assert_eq!(m.config.active_account.as_deref(), Some(key));
        assert_eq!(m.legacy_token, Some((key.to_string(), "t0k".to_string())));
        let ws = &m.config.workspaces[key];
        assert_eq!(ws.recent_ids, vec![X]);
        assert_eq!(ws.remote_name(X), Some("Office"));
        assert_eq!(ws.saved_logins[X], "sealed");
        assert!(m.config.workspaces[LOCAL_WORKSPACE].remote_names.is_empty());
        let acct = &m.config.accounts[0];
        assert!(acct.keep_signed_in && acct.token_ref.is_none() && acct.last_sync == 7);
        assert!(m.forget.is_empty());
        // The token is nowhere in the new file.
        let json = serde_json::to_string(&m.config).unwrap();
        assert!(!json.contains("t0k"), "{json}");
    }

    #[test]
    fn without_a_usable_account_everything_goes_to_local() {
        let m = v1_to_v2(
            v1(&format!(
                r#"{{"uuid":"u-2","remoteNames":{{"{X}":"Office"}},
                    "account":{{"server":"https://s","username":"sam"}}}}"#
            )),
            1000,
        );
        assert!(m.config.accounts.is_empty());
        assert!(m.config.active_account.is_none());
        assert!(m.legacy_token.is_none());
        assert_eq!(
            m.config.workspaces[LOCAL_WORKSPACE].remote_name(X),
            Some("Office")
        );
    }

    #[test]
    fn saved_passwords_are_forgotten_when_automatic_sign_in_was_off() {
        let m = v1_to_v2(
            v1(&format!(
                r#"{{"uuid":"u","autoLogin":false,"savedLogins":{{"{X}":"sealed"}}}}"#
            )),
            1000,
        );
        assert!(m.config.workspaces[LOCAL_WORKSPACE].saved_logins.is_empty());
        assert_eq!(m.forget, vec!["sealed".to_string()]);
    }

    #[test]
    fn changed_preferences_keep_their_values_and_are_stamped() {
        let m = v1_to_v2(
            v1(
                r#"{"uuid":"u","clipboardSync":false,"translateModifiers":true,"captureShortcuts":true}"#,
            ),
            1000,
        );
        let ws = &m.config.workspaces[LOCAL_WORKSPACE];
        assert!(!ws.prefs.clipboard_sync);
        assert!(ws.prefs.capture_shortcuts);
        assert!(ws.prefs.translate_modifiers);
        assert_eq!(ws.pref_stamps.get("clipboardSync"), Some(&1000));
        assert_eq!(ws.pref_stamps.get("captureShortcuts"), Some(&1000));
        assert_eq!(ws.pref_stamps.get("translateModifiers"), None);
    }

    #[test]
    fn device_settings_stay_on_the_device() {
        let m = v1_to_v2(
            v1(
                r#"{"uuid":"u","serverUrl":"ws://127.0.0.1:8787/ws","mode":"password",
                   "passwordHash":"ab","shareAudio":false,"useOpenH264":false,"startWithWindows":true}"#,
            ),
            1000,
        );
        let c = &m.config;
        assert_eq!(c.server_url, "ws://127.0.0.1:8787/ws");
        assert_eq!(c.mode, Mode::Password);
        assert_eq!(c.password_hash.as_deref(), Some("ab"));
        assert!(!c.share_audio && !c.use_openh264 && c.start_with_windows);
    }

    #[test]
    fn an_absent_uuid_stays_absent_for_the_caller_to_fill() {
        let m = v1_to_v2(v1("{}"), 1000);
        assert!(m.config.uuid.is_empty());
    }
}
