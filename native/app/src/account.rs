//! Accounts: sign in on several devices and keep their remote IDs (names and
//! recents) in sync through the signaling server's `/api` endpoints
//! (`server/src/index.js`).
//!
//! The merge rules live in `protocol::sync` and `Config::apply_sync_doc`; this
//! module only carries documents over HTTPS and stores the session. A sync is
//! one request: POST this device's document, receive the account's merged
//! document, merge it in locally. Merging (not overwriting) means an edit made
//! while a request is in flight is never lost; it goes up with the next sync.
//!
//! HTTP is `reqwest` with rustls, the same client and TLS stack the updater
//! plugin already ships (no new crates). The web UI cannot call the server
//! itself: its CSP is `default-src 'self'`.

use std::sync::OnceLock;
use std::time::Duration;

use protocol::sync::now_ms;
use protocol::{Account, Config, SyncDoc};
use serde::Deserialize;

use crate::AppState;

/// Bound on every account request, so a dead network fails in the UI instead
/// of hanging.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The server's HTTPS base for the signaling URL in Settings:
/// `wss://host/ws` → `https://host` (`ws://` → `http://` for a local server).
/// `None` for a URL that is not a WebSocket or HTTP one.
pub fn api_base(server_url: &str) -> Option<String> {
    let s = server_url.trim();
    let http = if let Some(rest) = s.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = s.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if s.starts_with("https://") || s.starts_with("http://") {
        s.to_string()
    } else {
        return None;
    };
    let http = http.split(['?', '#']).next().unwrap_or_default();
    let http = http.trim_end_matches('/');
    let http = http.strip_suffix("/ws").unwrap_or(http);
    let host = http.split_once("://").map(|(_, h)| h).unwrap_or_default();
    (!host.is_empty()).then(|| http.to_string())
}

/// What the UI shows for the account (never the token).
pub fn account_json(cfg: &Config) -> serde_json::Value {
    match &cfg.account {
        Some(a) => serde_json::json!({
            "username": a.username,
            "server": a.server,
            "lastSync": a.last_sync,
        }),
        None => serde_json::Value::Null,
    }
}

/// The account plus the lists a sync may have changed, so the UI can redraw.
fn status_json(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "account": account_json(cfg),
        "recentIds": cfg.recent_ids,
        "remoteNames": cfg.remote_names,
    })
}

fn client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // reqwest is built without a bundled crypto provider (the updater
            // plugin's feature set); install the same one it does.
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::ring::default_provider().install_default();
            }
            reqwest::Client::builder()
                .user_agent(concat!("ShareCtrlScreen/", env!("CARGO_PKG_VERSION")))
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|e| tracing::warn!("account: http client: {e}"))
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "Could not start the network client.".to_string())
}

/// The server's error codes, in words for the UI.
fn error_message(code: &str, status: u16) -> String {
    match code {
        "username-taken" => "That username is already taken.".into(),
        "invalid-username" => {
            "Usernames are 3 to 32 characters: letters, digits, dots, dashes or underscores, \
             starting with a letter or digit."
                .into()
        }
        "weak-password" => "Use a password of at least 8 characters.".into(),
        "bad-credentials" => "Wrong username or password.".into(),
        "too-many-attempts" => "Too many attempts. Try again in a few minutes.".into(),
        "unauthorized" => "You were signed out. Sign in again to keep syncing.".into(),
        "too-large" => "The ID list is too large to sync.".into(),
        "" => format!("The server answered with an error ({status})."),
        other => format!("The server refused the request ({other})."),
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct SessionBody {
    username: String,
    token: String,
}

#[derive(Deserialize)]
struct SyncBody {
    doc: SyncDoc,
}

/// Why a request failed: `Unauthorized` means the token is no longer valid.
enum Failure {
    Unauthorized,
    Other(String),
}

impl From<Failure> for String {
    fn from(f: Failure) -> String {
        match f {
            Failure::Unauthorized => error_message("unauthorized", 401),
            Failure::Other(m) => m,
        }
    }
}

/// POST `body` as JSON and parse a JSON answer, mapping the server's error
/// codes to readable messages.
async fn post<T: for<'de> Deserialize<'de>>(
    url: &str,
    token: Option<&str>,
    body: &serde_json::Value,
) -> Result<T, Failure> {
    let mut req = client().map_err(Failure::Other)?.post(url).json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| {
        tracing::warn!("account: {url}: {e}");
        Failure::Other("Could not reach the server. Check your connection.".into())
    })?;
    let status = resp.status();
    if status.is_success() {
        return resp.json::<T>().await.map_err(|e| {
            tracing::warn!("account: {url}: bad answer: {e}");
            Failure::Other("The server sent an answer this version does not understand.".into())
        });
    }
    if status == reqwest::StatusCode::UNAUTHORIZED && token.is_some() {
        return Err(Failure::Unauthorized);
    }
    let code = resp
        .json::<ErrorBody>()
        .await
        .map(|b| b.error)
        .unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND && code.is_empty() {
        return Err(Failure::Other(
            "This server does not support accounts (it may need updating).".into(),
        ));
    }
    Err(Failure::Other(error_message(&code, status.as_u16())))
}

/// Create an account (`create`) or sign in to one, then sync right away.
/// A sign-in that worked but whose first sync did not is still a sign-in: the
/// answer carries `syncError` and the next sync retries.
#[tauri::command]
pub async fn account_sign_in(
    state: tauri::State<'_, AppState>,
    username: String,
    password: String,
    create: bool,
) -> Result<serde_json::Value, String> {
    let base = api_base(&state.engine.config().server_url)
        .ok_or_else(|| "The server URL in Settings is not a valid address.".to_string())?;
    let op = if create { "signup" } else { "login" };
    let session: SessionBody = post(
        &format!("{base}/api/account/{op}"),
        None,
        &serde_json::json!({ "username": username.trim(), "password": password }),
    )
    .await?;
    state.engine.update_config(|c| {
        c.account = Some(Account {
            server: base,
            username: session.username,
            token: session.token,
            last_sync: 0,
        });
    });
    tracing::info!("account: signed in");
    match account_sync(state.clone()).await {
        Ok(v) => Ok(v),
        Err(e) => {
            let mut v = status_json(&state.engine.config());
            v["syncError"] = e.into();
            Ok(v)
        }
    }
}

/// Merge this device's IDs with the account's. Errors are readable messages;
/// an expired token signs this device out (its IDs stay).
#[tauri::command]
pub async fn account_sync(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let (account, doc) = {
        let cfg = state.engine.config();
        let Some(account) = cfg.account.clone() else {
            return Err("Not signed in.".into());
        };
        (account, cfg.sync_doc())
    };
    let result: Result<SyncBody, Failure> = post(
        &format!("{}/api/sync", account.server),
        Some(&account.token),
        &serde_json::json!({ "doc": doc }),
    )
    .await;
    // Only touch the session this request was made for: the user may have
    // signed out (or in as someone else) while it was in flight.
    let same_session = |c: &Config| c.account.as_ref().is_some_and(|a| a.token == account.token);
    match result {
        Ok(body) => {
            state.engine.update_config(|c| {
                if same_session(c) {
                    c.apply_sync_doc(&body.doc);
                    if let Some(a) = c.account.as_mut() {
                        a.last_sync = now_ms();
                    }
                }
            });
            Ok(status_json(&state.engine.config()))
        }
        Err(Failure::Unauthorized) => {
            state.engine.update_config(|c| {
                if same_session(c) {
                    c.account = None;
                }
            });
            tracing::info!("account: token refused, signed out");
            Err(Failure::Unauthorized.into())
        }
        Err(Failure::Other(m)) => Err(m),
    }
}

/// Sign this device out. Its IDs stay on it; the server is asked to revoke
/// the token, best effort (signing out works offline too).
#[tauri::command]
pub async fn account_sign_out(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let mut old = None;
    state.engine.update_config(|c| old = c.account.take());
    if let Some(a) = old {
        let url = format!("{}/api/account/logout", a.server);
        tauri::async_runtime::spawn(async move {
            let r: Result<serde_json::Value, _> =
                post(&url, Some(&a.token), &serde_json::json!({})).await;
            if r.is_err() {
                tracing::info!("account: could not revoke the token on the server");
            }
        });
    }
    Ok(status_json(&state.engine.config()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_follows_the_signaling_url() {
        assert_eq!(
            api_base("wss://sharectrl-signal.netameta.workers.dev/ws").as_deref(),
            Some("https://sharectrl-signal.netameta.workers.dev")
        );
        assert_eq!(
            api_base(" ws://127.0.0.1:8787/ws/ ").as_deref(),
            Some("http://127.0.0.1:8787")
        );
        assert_eq!(
            api_base("wss://example.com/prefix/ws?x=1").as_deref(),
            Some("https://example.com/prefix")
        );
        assert_eq!(
            api_base("https://example.com").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(api_base("example.com/ws"), None);
        assert_eq!(api_base("wss:///ws"), None);
        assert_eq!(api_base(""), None);
    }

    /// Two devices, one account, through a real server:
    ///   cd server && npm run dev
    ///   SHARECTRL_TEST_SERVER=http://127.0.0.1:8787 \
    ///     cargo test -p app --bin sharectrl -- --ignored live_server
    #[tokio::test]
    #[ignore = "needs a running server (SHARECTRL_TEST_SERVER)"]
    async fn two_devices_sync_through_a_live_server() {
        let base = std::env::var("SHARECTRL_TEST_SERVER").expect("SHARECTRL_TEST_SERVER");
        let fail = |f: Failure| -> String { f.into() };
        let username = format!("rt{}", now_ms());
        let creds = serde_json::json!({ "username": username, "password": "correct horse" });
        let a: SessionBody = post(&format!("{base}/api/account/signup"), None, &creds)
            .await
            .map_err(fail)
            .unwrap();
        let b: SessionBody = post(&format!("{base}/api/account/login"), None, &creds)
            .await
            .map_err(fail)
            .unwrap();

        async fn sync(base: &str, token: &str, device: &mut Config) {
            let body: SyncBody = post(
                &format!("{base}/api/sync"),
                Some(token),
                &serde_json::json!({ "doc": device.sync_doc() }),
            )
            .await
            .map_err(String::from)
            .unwrap();
            device.apply_sync_doc(&body.doc);
        }

        const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
        const Y: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let mut dev_a = Config::default();
        let mut dev_b = Config::default();
        dev_a.add_recent(X);
        dev_a.set_remote_name(X, "Office PC");
        std::thread::sleep(Duration::from_millis(5)); // B's use is the later one
        dev_b.add_recent(Y);
        sync(&base, &a.token, &mut dev_a).await;
        sync(&base, &b.token, &mut dev_b).await;
        assert_eq!(dev_b.remote_name(X), Some("Office PC"));
        assert_eq!(dev_b.recent_ids, vec![Y.to_string(), X.to_string()]);

        std::thread::sleep(Duration::from_millis(5));
        dev_b.set_remote_name(X, "");
        sync(&base, &b.token, &mut dev_b).await;
        sync(&base, &a.token, &mut dev_a).await;
        assert_eq!(dev_a.remote_name(X), None);
        assert_eq!(dev_a.recent_ids, dev_b.recent_ids);

        // A revoked token is reported as such.
        let _: serde_json::Value = post(
            &format!("{base}/api/account/logout"),
            Some(&b.token),
            &serde_json::json!({}),
        )
        .await
        .map_err(fail)
        .unwrap();
        let r: Result<SyncBody, _> = post(
            &format!("{base}/api/sync"),
            Some(&b.token),
            &serde_json::json!({}),
        )
        .await;
        assert!(matches!(r, Err(Failure::Unauthorized)));
    }

    #[test]
    fn the_token_never_reaches_the_ui() {
        let cfg = Config {
            account: Some(Account {
                server: "https://s".into(),
                username: "sam".into(),
                token: "secret-token".into(),
                last_sync: 5,
            }),
            ..Config::default()
        };
        let v = status_json(&cfg).to_string();
        assert!(v.contains("\"username\":\"sam\""), "{v}");
        assert!(!v.contains("secret-token"), "{v}");
    }
}
