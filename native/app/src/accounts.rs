//! AccountManager (plans/08 P1, 08a §1, §5.3, §6.8–§6.12): the accounts on this
//! computer, signing in, up and out, switching, and keeping the active
//! account's workspace in sync with the server's `/api` (`server/src/
//! accounts.js`).
//!
//! The engine owns the data and the rules (its workspace store: which
//! workspace is reachable, the merge, the tokens in the OS secret store); this
//! module carries documents over HTTPS, decides when to sync, and tells the UI
//! what state the account is in (`account-state`, `account-bring-in`).
//!
//! Account states (08a §5.3), derived from the engine's selection plus a
//! pending "bring in" question:
//! * `local` — no account; the local workspace;
//! * `signedIn` — an account's workspace, syncing;
//! * `locked` — an account is selected but not signed in: only its sign-in
//!   form is shown, nothing of its workspace (I13);
//! * `resolving` — just signed in from local with local data: the user is
//!   asked whether to bring it in; new connections wait.
//!
//! A sync is one request: POST this workspace's document, receive the
//! account's merged document, merge it in locally. Merging (not overwriting)
//! means an edit made while a request is in flight is never lost; it goes up
//! with the next sync. Syncs run after any workspace change (1.5 s later,
//! coalesced), every 5 minutes, when the window comes back after a minute, on
//! sign-in and switch, and on request.
//!
//! HTTP is `reqwest` with rustls, the same client and TLS stack the updater
//! plugin already ships (no new crates). The web UI cannot call the server
//! itself: its CSP is `default-src 'self'`.
//!
//! ## References
//! - plans/08a-STRUCTURE-AND-FLOWS.md §4 (commands, events), §5.3, §6.8–§6.12
//! - server/src/accounts.js (the API), server/src/sync.js (the merge)

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use engine::{Engine, WorkspaceSel};
use protocol::SyncDoc;
use serde::Deserialize;
use tauri::Emitter;

use crate::AppState;

/// Bound on every account request, so a dead network fails in the UI instead
/// of hanging.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// After a workspace change, wait this long for more before syncing.
const SYNC_DEBOUNCE: Duration = Duration::from_millis(1500);
/// Sync at least this often while signed in.
const SYNC_EVERY: Duration = Duration::from_secs(5 * 60);
/// The window coming back to the front syncs if the last try is this old.
const SYNC_ON_FOCUS_AFTER: Duration = Duration::from_secs(60);

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
        "too-large" => "The account's data is too large to sync.".into(),
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

/// A sync reply. `features` and `serverTime` are absent from servers before
/// 1.2.0: no tabs or preferences are sent to those, and stamps stay on this
/// computer's clock.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBody {
    doc: SyncDoc,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    server_time: u64,
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

/// What a server stores, from its last sync reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Features {
    tabs: bool,
    prefs: bool,
}

impl Features {
    fn from_reply(list: &[String]) -> Self {
        Features {
            tabs: list.iter().any(|f| f == protocol::sync::FEATURE_TABS),
            prefs: list.iter().any(|f| f == protocol::sync::FEATURE_PREFS),
        }
    }
}

/// A sign-in waiting for "bring this computer's tabs and names in?".
#[derive(Clone)]
struct Resolving {
    key: String,
    /// The username, as the question names it.
    account: String,
    tabs: usize,
    names: usize,
}

#[derive(Default)]
struct Inner {
    /// Signed in from local with local data: the account waiting for the
    /// "bring this computer's tabs and names in?" answer. Part of the state
    /// the UI is sent, so a reloaded page asks again instead of leaving new
    /// connections held with no question on screen.
    resolving: Option<Resolving>,
    /// Per account: what its server stores (learned from each reply).
    features: HashMap<String, Features>,
    /// The last sync problem, shown under the account until the next success.
    error: Option<String>,
    /// When a sync was last tried.
    last_attempt: Option<Instant>,
}

/// The accounts on this computer. One per app, in [`AppState`].
pub(crate) struct AccountManager {
    engine: Arc<Engine>,
    handle: OnceLock<tauri::AppHandle>,
    inner: Mutex<Inner>,
    /// One sync at a time.
    sync_gate: tokio::sync::Mutex<()>,
    /// Asks the sync loop to sync now (sign-in, switch, focus, the button).
    kick: tokio::sync::Notify,
}

impl AccountManager {
    pub(crate) fn new(engine: Arc<Engine>) -> Arc<Self> {
        Arc::new(AccountManager {
            engine,
            handle: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
            sync_gate: tokio::sync::Mutex::new(()),
            kick: tokio::sync::Notify::new(),
        })
    }

    /// The inner state. A panic while it was held cannot leave it
    /// inconsistent (every update is a single assignment), so a poisoned lock
    /// is used as is rather than taking the account system down with it.
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Launch (08a §6.8): finish a 2.1.x token migration, sign the selected
    /// account in by itself when it was kept, and start syncing.
    pub(crate) fn start(self: &Arc<Self>, handle: tauri::AppHandle) {
        let _ = self.handle.set(handle);
        let am = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            am.launch().await;
            am.sync_loop().await;
        });
    }

    async fn launch(&self) {
        if let Some((key, token)) = self.engine.take_legacy_token() {
            // The 2.1.x file kept this account signed in: move its token into
            // the secret store (08a §6.13 step 3). Without a keyring it works
            // for this run only, and the user is told.
            // Kept only when this run may write the file: otherwise every launch
            // would seal it again and orphan the previous secret-store item.
            let keep = self.engine.is_writable();
            if let Err(e) = self.engine.store_token(&key, token, keep).await {
                self.toast(&e);
            }
            self.engine
                .switch_workspace(WorkspaceSel::Account(key), false)
                .await;
        } else if let WorkspaceSel::Locked(key) = self.engine.workspace_sel() {
            if self.engine.account_token(&key).await.is_some() {
                self.engine
                    .switch_workspace(WorkspaceSel::Account(key), false)
                    .await;
            }
        }
        self.emit_state();
    }

    /// Sync whenever the workspace changes (coalesced), when kicked, and every
    /// few minutes. Runs for the life of the app.
    async fn sync_loop(&self) {
        let changed = self.engine.workspace_changed();
        let mut every = tokio::time::interval(SYNC_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = changed.notified() => {
                    // Let a burst of edits settle into one sync.
                    tokio::time::sleep(SYNC_DEBOUNCE).await;
                }
                _ = self.kick.notified() => {}
                _ = every.tick() => {}
            }
            if matches!(self.engine.workspace_sel(), WorkspaceSel::Account(_)) {
                let _ = self.sync_once().await;
            }
        }
    }

    /// One sync of the signed-in account (08a §6.12). A server that just
    /// started advertising tabs or preferences gets them in a second round
    /// right away.
    async fn sync_once(&self) -> Result<(), String> {
        let _one = self.sync_gate.lock().await;
        for _ in 0..2 {
            match self.sync_round().await? {
                Round::Done => break,
                Round::AgainWithNewFeatures => continue,
            }
        }
        Ok(())
    }

    async fn sync_round(&self) -> Result<Round, String> {
        let WorkspaceSel::Account(key) = self.engine.workspace_sel() else {
            return Err("Not signed in.".into());
        };
        let epoch = self.engine.workspace_epoch();
        self.inner().last_attempt = Some(Instant::now());
        let Some(token) = self.engine.account_token(&key).await else {
            return Err(self.lose_session(&key).await);
        };
        let Some(server) = self.engine.config().account(&key).map(|a| a.server.clone()) else {
            return Err("Not signed in.".into());
        };
        let sent = self.inner().features.get(&key).copied().unwrap_or_default();
        let Some(doc) = self.engine.sync_doc(&key, sent.tabs, sent.prefs) else {
            return Err("Not signed in.".into());
        };
        let result: Result<SyncBody, Failure> = post(
            &format!("{server}/api/sync"),
            Some(&token),
            &serde_json::json!({ "doc": doc }),
        )
        .await;
        match result {
            Ok(body) => {
                self.engine.set_server_time(body.server_time);
                let now = Features::from_reply(&body.features);
                self.inner().features.insert(key.clone(), now);
                // Applied only if this account is still the one in use and no
                // switch happened while the request was out (08a §6.12 step 5).
                self.engine
                    .apply_sync(&key, epoch, &body.doc, sent.tabs, sent.prefs);
                self.inner().error = None;
                self.emit_state();
                let gained = (now.tabs && !sent.tabs) || (now.prefs && !sent.prefs);
                Ok(if gained {
                    Round::AgainWithNewFeatures
                } else {
                    Round::Done
                })
            }
            Err(Failure::Unauthorized) => Err(self.lose_session(&key).await),
            Err(Failure::Other(m)) => {
                self.inner().error = Some(m.clone());
                self.emit_state();
                Err(m)
            }
        }
    }

    /// The token is gone or refused: the account is signed out here and
    /// locked (08a §5.3), and the user is told.
    async fn lose_session(&self, key: &str) -> String {
        let msg = error_message("unauthorized", 401);
        // A reply for an account no longer in use (signed out or switched away
        // while the request was out) changes nothing and is not reported.
        if self.engine.workspace_sel() != WorkspaceSel::Account(key.to_string()) {
            return msg;
        }
        tracing::info!("account: token refused or missing, signed out");
        self.engine.forget_token(key);
        self.engine
            .switch_workspace(WorkspaceSel::Locked(key.to_string()), true)
            .await;
        self.inner().error = Some(msg.clone());
        self.emit_state();
        self.toast(&msg);
        msg
    }

    /// The window came back to the front: sync if the last try is old.
    pub(crate) fn on_focus(&self) {
        let stale = self
            .inner()
            .last_attempt
            .is_none_or(|t| t.elapsed() > SYNC_ON_FOCUS_AFTER);
        if stale {
            self.kick.notify_one();
        }
    }

    /// The account state for the UI (never a token).
    pub(crate) fn view(&self) -> serde_json::Value {
        let cfg = self.engine.config();
        let sel = self.engine.workspace_sel();
        let inner = self.inner();
        let bring_in = inner
            .resolving
            .as_ref()
            .map(|r| serde_json::json!({ "account": r.account, "tabs": r.tabs, "names": r.names }));
        let (state, active) = match (&inner.resolving, &sel) {
            (Some(r), _) => ("resolving", Some(r.key.clone())),
            (None, WorkspaceSel::Local) => ("local", None),
            (None, WorkspaceSel::Account(k)) => ("signedIn", Some(k.clone())),
            (None, WorkspaceSel::Locked(k)) => ("locked", Some(k.clone())),
        };
        let active = active.and_then(|k| cfg.account(&k).cloned()).map(|a| {
            serde_json::json!({
                "key": a.key(),
                "username": a.username,
                "server": a.server,
                "lastSync": a.last_sync,
                "keepSignedIn": a.keep_signed_in,
            })
        });
        let accounts: Vec<_> = cfg
            .accounts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "key": a.key(),
                    "username": a.username,
                    "server": a.server,
                })
            })
            .collect();
        serde_json::json!({
            "state": state,
            "active": active,
            "accounts": accounts,
            "error": inner.error,
            "bringIn": bring_in,
        })
    }

    fn emit_state(&self) {
        if let Some(h) = self.handle.get() {
            let _ = h.emit("account-state", self.view());
        }
    }

    fn toast(&self, message: &str) {
        if let Some(h) = self.handle.get() {
            let _ = h.emit("toast", serde_json::json!({ "message": message }));
        }
    }

    /// Sign in to an existing account (08a §6.10) or create one (§6.9).
    /// `Ok(Some(warning))`: signed in, but "Keep me signed in" could not be
    /// honoured (no keyring) — said as such, not as a sync failure.
    async fn sign_in(
        &self,
        server: String,
        username: &str,
        password: &str,
        keep: bool,
        create: bool,
    ) -> Result<Option<String>, String> {
        let op = if create { "signup" } else { "login" };
        let session: SessionBody = post(
            &format!("{server}/api/account/{op}"),
            None,
            &serde_json::json!({ "username": username.trim(), "password": password }),
        )
        .await?;
        let key = self.engine.upsert_account(&server, &session.username);
        let warning = self
            .engine
            .store_token(&key, session.token, keep)
            .await
            .err();
        tracing::info!("account: signed in");

        let from = self.engine.workspace_sel();
        if from == WorkspaceSel::Account(key.clone()) {
            // Already this account (a repeated sign-in): nothing to move.
        } else if create && from == WorkspaceSel::Local {
            // Sign-up adopts this computer's local workspace; the sessions
            // stay, it is the same data (§6.9 step 2).
            self.engine.adopt_local_into(&key);
            self.engine
                .switch_workspace(WorkspaceSel::Account(key.clone()), false)
                .await;
        } else if from == WorkspaceSel::Local && !create {
            if let Some((tabs, names)) = self.engine.local_data() {
                // Ask first (§6.10 step 2); connections wait for the answer.
                self.engine.hold_connects(true);
                self.inner().resolving = Some(Resolving {
                    key: key.clone(),
                    account: session.username.clone(),
                    tabs,
                    names,
                });
                // The question rides on the state (`bringIn`), so the UI shows
                // it now and again after a reload.
                self.emit_state();
                return Ok(warning);
            }
            self.engine
                .switch_workspace(WorkspaceSel::Account(key.clone()), true)
                .await;
        } else {
            // From another account (or its locked form): a switch (§6.11).
            self.engine
                .switch_workspace(WorkspaceSel::Account(key.clone()), true)
                .await;
        }
        self.emit_state();
        // The first sync runs now, so the account's data appears at once.
        self.sync_once().await?;
        Ok(warning)
    }

    /// The answer to "bring this computer's tabs and names in?" (§6.10 step 3).
    /// Dismissing the question counts as "separate".
    async fn resolve_local(&self, add: bool) -> Result<(), String> {
        let Some(Resolving { key, .. }) = self.inner().resolving.take() else {
            return Ok(());
        };
        if add {
            self.engine.merge_local_into(&key);
        }
        // "Add" keeps the connected local sessions (now the account's);
        // "Keep separate" ends them: they are not part of the account.
        self.engine
            .switch_workspace(WorkspaceSel::Account(key), !add)
            .await;
        self.engine.hold_connects(false);
        self.emit_state();
        self.sync_once().await
    }

    /// Switch to the local workspace or to an account (§6.11). An account
    /// whose token is not kept opens locked, asking for its password.
    async fn switch(&self, target: Option<String>) -> Result<(), String> {
        self.cancel_resolving().await;
        let to = match target {
            None => WorkspaceSel::Local,
            Some(key) => {
                if self.engine.config().account(&key).is_none() {
                    return Err("That account is not on this computer.".into());
                }
                if self.engine.account_token(&key).await.is_some() {
                    WorkspaceSel::Account(key)
                } else {
                    WorkspaceSel::Locked(key)
                }
            }
        };
        let signed_in = matches!(to, WorkspaceSel::Account(_));
        self.engine.switch_workspace(to, true).await;
        self.emit_state();
        if signed_in {
            let _ = self.sync_once().await;
        }
        Ok(())
    }

    /// Sign out of the selected account (§6.11 step 3): its token is revoked
    /// on the server (best effort) and forgotten here; the computer goes back
    /// to the local workspace. The account stays listed, locked.
    async fn sign_out(&self) {
        // Signing out of the account whose question is still open drops the
        // question and that sign-in only: the local workspace and its
        // sessions stay as they are (P1 review).
        if self.abandon_resolving(None).await {
            return;
        }
        let Some(key) = self.engine.workspace_sel().account_key().map(String::from) else {
            return;
        };
        self.revoke(&key).await;
        self.engine.forget_token(&key);
        self.engine
            .switch_workspace(WorkspaceSel::Local, true)
            .await;
        self.emit_state();
    }

    /// "Remove from this computer": sign out, then delete the account, its
    /// workspace and its saved host passwords.
    async fn remove(&self, key: &str) {
        if !self.abandon_resolving(Some(key)).await {
            self.cancel_resolving().await;
        }
        if self.engine.workspace_sel().account_key() == Some(key) {
            self.engine
                .switch_workspace(WorkspaceSel::Local, true)
                .await;
        }
        self.revoke(key).await;
        self.engine.remove_account(key);
        self.inner().features.remove(key);
        self.emit_state();
    }

    /// Drop an open "bring in" question together with its sign-in (token
    /// revoked and forgotten), leaving the local workspace and its sessions
    /// untouched — for `key`'s question, or any when `None`. Returns whether
    /// there was one to drop.
    async fn abandon_resolving(&self, key: Option<&str>) -> bool {
        let pending = {
            let mut inner = self.inner();
            match &inner.resolving {
                Some(r) if key.is_none_or(|k| k == r.key) => inner.resolving.take(),
                _ => None,
            }
        };
        let Some(r) = pending else {
            return false;
        };
        self.revoke(&r.key).await;
        self.engine.forget_token(&r.key);
        self.engine.hold_connects(false);
        self.emit_state();
        true
    }

    /// A pending "bring in" question is answered "separate" by any other
    /// account action.
    async fn cancel_resolving(&self) {
        if self.inner().resolving.is_some() {
            let _ = self.resolve_local(false).await;
        }
    }

    /// Ask the server to revoke this computer's token, in the background (a
    /// sign-out works offline too).
    async fn revoke(&self, key: &str) {
        let Some(server) = self.engine.config().account(key).map(|a| a.server.clone()) else {
            return;
        };
        let Some(token) = self.engine.account_token(key).await else {
            return;
        };
        let url = format!("{server}/api/account/logout");
        tauri::async_runtime::spawn(async move {
            let r: Result<serde_json::Value, _> =
                post(&url, Some(&token), &serde_json::json!({})).await;
            if r.is_err() {
                tracing::info!("account: could not revoke the token on the server");
            }
        });
    }
}

enum Round {
    Done,
    AgainWithNewFeatures,
}

/// The server an account action talks to: the given account's, or the one in
/// Settings for a new sign-in.
fn server_for(state: &AppState, account: Option<&str>) -> Result<String, String> {
    if let Some(key) = account {
        if let Some(a) = state.engine.config().account(key) {
            return Ok(a.server.clone());
        }
    }
    api_base(&state.engine.config().server_url)
        .ok_or_else(|| "The server URL in Settings is not a valid address.".to_string())
}

/// Sign in to an existing account. `account` (a key) signs in to that known
/// account on its own server — the locked form; otherwise the server in
/// Settings is used. Answers the account state; a sign-in that worked but whose
/// first sync did not is still a sign-in (`syncError`).
#[tauri::command]
pub async fn account_sign_in(
    state: tauri::State<'_, AppState>,
    username: String,
    password: String,
    keep_signed_in: bool,
    account: Option<String>,
) -> Result<serde_json::Value, String> {
    let server = server_for(&state, account.as_deref())?;
    sign_in_or_up(&state, server, &username, &password, keep_signed_in, false).await
}

/// Create an account; this computer's local workspace becomes its own.
#[tauri::command]
pub async fn account_sign_up(
    state: tauri::State<'_, AppState>,
    username: String,
    password: String,
    keep_signed_in: bool,
) -> Result<serde_json::Value, String> {
    let server = server_for(&state, None)?;
    sign_in_or_up(&state, server, &username, &password, keep_signed_in, true).await
}

async fn sign_in_or_up(
    state: &AppState,
    server: String,
    username: &str,
    password: &str,
    keep: bool,
    create: bool,
) -> Result<serde_json::Value, String> {
    let am = &state.accounts;
    match am.sign_in(server, username, password, keep, create).await {
        Ok(warning) => {
            let mut v = am.view();
            if let Some(w) = warning {
                v["warning"] = w.into();
            }
            Ok(v)
        }
        // The account exists and is selected; only a later step failed.
        Err(e) if am.engine.workspace_sel().account_key().is_some() => {
            let mut v = am.view();
            v["syncError"] = e.into();
            Ok(v)
        }
        Err(e) => Err(e),
    }
}

/// Answer "Bring this computer's tabs and names into the account?":
/// `"add"` or `"separate"`.
#[tauri::command]
pub async fn account_resolve_local(
    state: tauri::State<'_, AppState>,
    choice: String,
) -> Result<serde_json::Value, String> {
    let r = state.accounts.resolve_local(choice == "add").await;
    let mut v = state.accounts.view();
    if let Err(e) = r {
        v["syncError"] = e.into();
    }
    Ok(v)
}

/// Switch to an account (its key) or to `"local"`.
#[tauri::command]
pub async fn account_switch(
    state: tauri::State<'_, AppState>,
    account: String,
) -> Result<serde_json::Value, String> {
    let target = (account != protocol::LOCAL_WORKSPACE).then_some(account);
    state.accounts.switch(target).await?;
    Ok(state.accounts.view())
}

/// Sign out of the selected account; its data stays on this computer, locked.
#[tauri::command]
pub async fn account_sign_out(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    state.accounts.sign_out().await;
    Ok(state.accounts.view())
}

/// Remove an account and everything it keeps on this computer.
#[tauri::command]
pub async fn account_remove(
    state: tauri::State<'_, AppState>,
    account: String,
) -> Result<serde_json::Value, String> {
    state.accounts.remove(&account).await;
    Ok(state.accounts.view())
}

/// Sync now (the "Sync now" button). Errors are readable messages.
#[tauri::command]
pub async fn account_sync(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    state.accounts.sync_once().await?;
    Ok(state.accounts.view())
}

/// The window came back to the front.
#[tauri::command]
pub fn account_window_focused(state: tauri::State<'_, AppState>) {
    state.accounts.on_focus();
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

    #[test]
    fn features_come_from_the_reply() {
        assert_eq!(Features::from_reply(&[]), Features::default());
        let f = Features::from_reply(&["prefs".into(), "tabs".into(), "later".into()]);
        assert!(f.prefs && f.tabs);
    }

    #[test]
    fn a_reply_from_an_old_server_still_parses() {
        let b: SyncBody =
            serde_json::from_str(r#"{"doc":{"v":1,"entries":{},"recentsClearedAt":0}}"#).unwrap();
        assert!(b.features.is_empty());
        assert_eq!(b.server_time, 0);
        let b: SyncBody = serde_json::from_str(
            r#"{"doc":{"v":2},"features":["prefs","tabs"],"serverTime":1800000000000}"#,
        )
        .unwrap();
        assert_eq!(
            Features::from_reply(&b.features),
            Features {
                tabs: true,
                prefs: true
            }
        );
        assert_eq!(b.server_time, 1_800_000_000_000);
    }

    /// Two computers, one account, through a real server (`server/`):
    ///   cd server && npm run dev
    ///   SHARECTRL_TEST_SERVER=http://127.0.0.1:8787 \
    ///     cargo test -p app --bin sharectrl -- --ignored live_server
    #[tokio::test]
    #[ignore = "needs a running server (SHARECTRL_TEST_SERVER)"]
    async fn two_computers_sync_names_recents_and_prefs_through_a_live_server() {
        use protocol::sync::now_ms;
        use protocol::Workspace;
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

        async fn sync(base: &str, token: &str, ws: &mut Workspace) {
            let body: SyncBody = post(
                &format!("{base}/api/sync"),
                Some(token),
                &serde_json::json!({ "doc": ws.sync_doc(true, true) }),
            )
            .await
            .map_err(String::from)
            .unwrap();
            assert!(Features::from_reply(&body.features).prefs);
            ws.apply_sync_doc(&body.doc, now_ms(), true, true);
        }

        const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
        const Y: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let mut one = Workspace::default();
        let mut two = Workspace::default();
        one.add_recent(X, now_ms());
        one.set_remote_name(X, "Office PC", now_ms());
        one.set_pref("translateModifiers", false, now_ms());
        std::thread::sleep(Duration::from_millis(5)); // two's use is the later one
        two.add_recent(Y, now_ms());
        sync(&base, &a.token, &mut one).await;
        sync(&base, &b.token, &mut two).await;
        assert_eq!(two.remote_name(X), Some("Office PC"));
        assert_eq!(two.recent_ids, vec![Y.to_string(), X.to_string()]);
        assert!(!two.prefs.translate_modifiers);

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
}
