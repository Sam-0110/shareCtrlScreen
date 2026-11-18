//! The workspace store (plans/08 P1, 08a §1, §5.3, §6.8–§6.13): which
//! workspace is reachable, every read and write of the person's data through
//! one function, account switching as one operation, the local workspace
//! adopted or merged on sign-in, and account tokens kept only in the OS secret
//! store.
//!
//! **Isolation (invariant I13).** An account's workspace is reachable only
//! while that account is signed in on this computer: [`WorkspaceSel::Locked`]
//! resolves to no workspace at all, so its names, recents, preferences and
//! saved host passwords can be neither read nor used until its password is
//! entered again. The local workspace is always reachable.
//!
//! **One path (I5).** Recents, names, tabs, preferences and saved host
//! passwords are read and written only through [`Engine::with_workspace`],
//! which resolves the active workspace, stamps with the engine's [`Stamper`],
//! persists, and wakes the account sync.
//!
//! **Lock order.** `ws` (this module's state) → `reg` (the sessions) →
//! `config`, never the other way round. No event is sent and no secret store
//! is called while any of them is held.
//!
//! ## References
//! - plans/08a-STRUCTURE-AND-FLOWS.md §3 (types), §5.3 (account states),
//!   §6.9–§6.13 (flows), §8 (I5, I10, I11, I13)
//! - `platform_api::SecretStore` (seal/open/forget may block on an OS unlock
//!   prompt: always off the runtime, always bounded)

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use protocol::config::{AccountRef, Prefs, Workspace, LOCAL_WORKSPACE};
use protocol::sync::{now_ms, Stamper, SyncDoc};
use protocol::Config;

use crate::{Engine, UiEvent, SECRET_PROMPT_TIMEOUT};

/// Which workspace this computer is using.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSel {
    /// No account: the local workspace.
    Local,
    /// Signed in to this account (key): its workspace.
    Account(String),
    /// This account (key) is selected but not signed in: no workspace is
    /// reachable until its password is entered (I13).
    Locked(String),
}

impl WorkspaceSel {
    /// The workspace key it reaches, if any.
    pub fn workspace_key(&self) -> Option<&str> {
        match self {
            WorkspaceSel::Local => Some(LOCAL_WORKSPACE),
            WorkspaceSel::Account(k) => Some(k),
            WorkspaceSel::Locked(_) => None,
        }
    }

    /// The account it is about, if any.
    pub fn account_key(&self) -> Option<&str> {
        match self {
            WorkspaceSel::Local => None,
            WorkspaceSel::Account(k) | WorkspaceSel::Locked(k) => Some(k),
        }
    }
}

/// The store's own state, under `Engine::ws`.
pub(crate) struct WsState {
    pub(crate) sel: WorkspaceSel,
    pub(crate) stamper: Stamper,
    /// Bumped by every workspace switch; work scheduled for one workspace
    /// checks it before acting (08a §6.7 step 2).
    pub(crate) epoch: u64,
    /// Switches in progress (a count: two can overlap, e.g. a user switch and
    /// a sign-out from a refused token); new connections are refused (I10).
    pub(crate) switching: u32,
    /// New connections wait for an answer the account manager is asking for
    /// ("bring this computer's tabs and names in?", 08a §6.10 step 2).
    pub(crate) held: bool,
    /// Live tokens of the accounts signed in this run, by account key. Never
    /// persisted: the file holds only the secret store's handle.
    pub(crate) tokens: HashMap<String, String>,
    /// A 2.1.x token found by the migration, not yet in the secret store.
    pub(crate) legacy_token: Option<(String, String)>,
}

impl WsState {
    /// The state at launch: an account selected in the file starts Locked
    /// until its token is opened (08a §6.8), so nothing of it is reachable
    /// before then.
    pub(crate) fn at_launch(cfg: &Config, legacy_token: Option<(String, String)>) -> Self {
        let mut stamper = Stamper::new();
        stamper.observe(cfg.latest_stamp());
        let sel = match &cfg.active_account {
            None => WorkspaceSel::Local,
            Some(k) => WorkspaceSel::Locked(k.clone()),
        };
        WsState {
            sel,
            stamper,
            epoch: 0,
            switching: 0,
            held: false,
            tokens: HashMap::new(),
            legacy_token,
        }
    }
}

impl Engine {
    /// Which workspace this computer is using.
    pub fn workspace_sel(&self) -> WorkspaceSel {
        self.ws.lock().sel.clone()
    }

    /// The workspace epoch (bumped by every switch).
    pub fn workspace_epoch(&self) -> u64 {
        self.ws.lock().epoch
    }

    /// Woken after every change to the active workspace, so the account sync
    /// can follow it (08a §6.12 step 1).
    pub fn workspace_changed(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.ws_changed)
    }

    /// Read or change the active workspace — the only way to reach the
    /// person's data (I5). `f` gets the workspace and the stamper for any
    /// stamp it records. The file is saved and the sync woken afterwards.
    /// `None` while the selected account is signed out (I13).
    pub fn with_workspace<R>(
        &self,
        f: impl FnOnce(&mut Workspace, &mut Stamper) -> R,
    ) -> Option<R> {
        let r = {
            let mut ws = self.ws.lock();
            let key = ws.sel.workspace_key()?.to_string();
            let mut cfg = self.config.lock();
            let r = f(cfg.workspace_mut(&key), &mut ws.stamper);
            self.translate_modifiers.store(
                cfg.workspaces[&key].prefs.translate_modifiers,
                Ordering::Relaxed,
            );
            self.save(&cfg);
            r
        };
        self.ws_changed.notify_one();
        Some(r)
    }

    /// The tab on screen on this computer. Saved, but the sync is not woken:
    /// it is per computer and never synced, and switching tabs must not
    /// cost a sync round. Nothing while the selected account is signed out.
    pub fn set_active_tab(&self, remote: Option<String>) {
        let ws = self.ws.lock();
        let Some(key) = ws.sel.workspace_key() else {
            return;
        };
        let mut cfg = self.config.lock();
        let w = cfg.workspace_mut(key);
        if w.active_tab == remote {
            return;
        }
        w.active_tab = remote;
        self.save(&cfg);
    }

    /// Record a connection to `peer` in the recents of workspace `key`, if no
    /// switch happened since `epoch` (the connection was started in that
    /// workspace; a later one must not receive it, I13).
    pub(crate) fn add_recent_in(&self, key: &str, epoch: u64, peer: &str) {
        {
            let mut ws = self.ws.lock();
            if ws.epoch != epoch {
                return;
            }
            let now = ws.stamper.now();
            let mut cfg = self.config.lock();
            cfg.workspace_mut(key).add_recent(peer, now);
            self.save(&cfg);
        }
        self.ws_changed.notify_one();
    }

    /// Read the active workspace without changing it (nothing is saved, the
    /// sync is not woken). `None` while signed out (I13).
    pub(crate) fn with_workspace_read<R>(&self, f: impl FnOnce(&Workspace) -> R) -> Option<R> {
        let ws = self.ws.lock();
        let key = ws.sel.workspace_key()?;
        let cfg = self.config.lock();
        Some(match cfg.workspaces.get(key) {
            Some(w) => f(w),
            None => f(&Workspace::default()),
        })
    }

    /// A copy of the active workspace, `None` while signed out (I13).
    pub fn workspace(&self) -> Option<Workspace> {
        let ws = self.ws.lock();
        let key = ws.sel.workspace_key()?;
        let cfg = self.config.lock();
        Some(cfg.workspaces.get(key).cloned().unwrap_or_default())
    }

    /// The preferences in force: the active workspace's, and while the
    /// selected account is signed out, that account's stored ones. Unlike its
    /// names and passwords they are not private, and falling back to the
    /// defaults would silently turn clipboard sharing back on for a host that
    /// switched it off (P1 review M1).
    pub fn prefs(&self) -> Prefs {
        let ws = self.ws.lock();
        let cfg = self.config.lock();
        prefs_in(&cfg, &ws.sel)
    }

    /// Settings → a synced preference. Stamped only when it changes (08a §6.12
    /// step 7). Returns whether it changed.
    pub fn set_pref(&self, key: &str, value: bool) -> bool {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.set_pref(key, value, now)
        })
        .unwrap_or(false)
    }

    /// Record the server's clock from a sync reply, for skew-corrected stamps.
    pub fn set_server_time(&self, server_time: u64) {
        self.ws.lock().stamper.set_skew(server_time);
    }

    /// Hold (or release) new connections while the account manager waits for
    /// the user's answer about bringing local data into an account.
    pub fn hold_connects(&self, on: bool) {
        self.ws.lock().held = on;
    }

    /// Switch to `to` as one operation (08a §6.11, I10): new connections are
    /// refused meanwhile, the viewer session (if any) is ended when
    /// `end_viewer` (always, except when the same data simply changes owner:
    /// sign-up adopting local, "Add" on sign-in, launch), then the selection
    /// changes, the choice is saved and the epoch bumped. Hosting is this
    /// computer's, not the account's, and goes on. Returns the new epoch.
    pub async fn switch_workspace(&self, to: WorkspaceSel, end_viewer: bool) -> u64 {
        self.ws.lock().switching += 1;
        if end_viewer {
            self.end_viewer_sessions(crate::EndReason::AccountSwitch);
        }
        let epoch = {
            let mut ws = self.ws.lock();
            ws.sel = to.clone();
            ws.epoch += 1;
            ws.switching -= 1;
            let mut cfg = self.config.lock();
            cfg.active_account = to.account_key().map(String::from);
            let prefs = prefs_in(&cfg, &to);
            self.translate_modifiers
                .store(prefs.translate_modifiers, Ordering::Relaxed);
            self.save(&cfg);
            ws.epoch
        };
        self.ws_changed.notify_one();
        // The UI shows the other workspace's names, recents and settings now.
        let _ = self.ui.send(UiEvent::WorkspaceChanged);
        epoch
    }

    /// Sign-up (08a §6.9): the local workspace becomes the new account's, with
    /// its saved password handles (the secret store is not touched); local is
    /// left empty. Nothing is disconnected: it is the same data.
    pub fn adopt_local_into(&self, account: &str) {
        let mut cfg = self.config.lock();
        let local = std::mem::take(cfg.workspace_mut(LOCAL_WORKSPACE));
        let target = cfg.workspace_mut(account);
        let replaced = std::mem::replace(target, local);
        // A fresh account has no workspace yet; an old leftover one is replaced,
        // so its saved passwords must go from the OS store too.
        let orphaned: Vec<String> = replaced.saved_logins.into_values().collect();
        self.save(&cfg);
        drop(cfg);
        self.forget_sealed(orphaned);
    }

    /// Sign-in, "Add" (08a §6.10 step 3): this computer's local tabs and names
    /// join the account's workspace by the sync rules; the account's
    /// preferences win; a host's saved password in the account wins over the
    /// local one. Local is left empty.
    pub fn merge_local_into(&self, account: &str) {
        let now = self.ws.lock().stamper.now();
        let mut cfg = self.config.lock();
        let local = std::mem::take(cfg.workspace_mut(LOCAL_WORKSPACE));
        let rest = cfg.workspace_mut(account).absorb_local(local, now);
        self.save(&cfg);
        drop(cfg);
        self.forget_sealed(rest);
    }

    /// Whether the local workspace has anything to bring into an account:
    /// `(open tabs, names)`, or `None` when it has nothing at all.
    pub fn local_data(&self) -> Option<(usize, usize)> {
        let cfg = self.config.lock();
        let local = cfg.workspaces.get(LOCAL_WORKSPACE)?;
        local
            .has_user_data()
            .then(|| (local.open_tab_count(), local.remote_names.len()))
    }

    /// Add an account, or update a known one's server-side facts (username as
    /// the server spells it, last sync). Its token handle and "keep signed in"
    /// are managed by [`Engine::store_token`] / [`Engine::forget_token`].
    pub fn upsert_account(&self, server: &str, username: &str) -> String {
        let mut cfg = self.config.lock();
        let key = protocol::account_key(server, username);
        if cfg.account(&key).is_none() {
            cfg.accounts.push(AccountRef {
                server: server.to_string(),
                username: username.to_string(),
                ..AccountRef::default()
            });
        }
        cfg.workspace_mut(&key);
        self.save(&cfg);
        key
    }

    /// "Remove from this computer" (08a §6.11 step 3): the account, its
    /// workspace and every secret it holds (token, saved host passwords).
    /// The caller has switched away from it first.
    pub fn remove_account(&self, account: &str) {
        let mut sealed = Vec::new();
        {
            let mut ws = self.ws.lock();
            ws.tokens.remove(account);
            let mut cfg = self.config.lock();
            if let Some(i) = cfg.accounts.iter().position(|a| a.key() == account) {
                let a = cfg.accounts.remove(i);
                sealed.extend(a.token_ref);
            }
            if let Some(w) = cfg.workspaces.remove(account) {
                sealed.extend(w.saved_logins.into_values());
            }
            if cfg.active_account.as_deref() == Some(account) {
                cfg.active_account = None;
            }
            self.save(&cfg);
        }
        self.forget_sealed(sealed);
    }

    /// Keep `token` for `account` for this run and, when `keep` ("Keep me
    /// signed in"), sealed in the OS secret store for the next launches. If it
    /// cannot be sealed the account works for this run only, and the error
    /// says so.
    pub async fn store_token(
        &self,
        account: &str,
        token: String,
        keep: bool,
    ) -> Result<(), String> {
        self.ws
            .lock()
            .tokens
            .insert(account.to_string(), token.clone());
        let sealed = if keep {
            match self.seal_bounded(token).await {
                Some(s) => Some(s),
                None => {
                    self.set_token_ref(account, None, false);
                    return Err(
                        "Could not save the sign-in securely on this computer (no keyring?); \
                         you will be asked to sign in again next time."
                            .into(),
                    );
                }
            }
        } else {
            None
        };
        let keep = sealed.is_some();
        self.set_token_ref(account, sealed, keep);
        Ok(())
    }

    /// The live token of `account`: from this run, or opened from the secret
    /// store (bounded: a locked keyring may prompt). `None` when it is not
    /// kept or cannot be opened.
    pub async fn account_token(&self, account: &str) -> Option<String> {
        if let Some(t) = self.ws.lock().tokens.get(account) {
            return Some(t.clone());
        }
        let sealed = self.config.lock().account(account)?.token_ref.clone()?;
        let store = self.platform.secrets();
        let open = tokio::task::spawn_blocking(move || store.open(&sealed));
        let token = match tokio::time::timeout(SECRET_PROMPT_TIMEOUT, open).await {
            Ok(Ok(t)) => t?,
            Ok(Err(e)) => {
                tracing::warn!("account: the secret store failed while opening: {e}");
                return None;
            }
            Err(_) => {
                tracing::warn!("account: the secret store gave no answer in time");
                return None;
            }
        };
        self.ws
            .lock()
            .tokens
            .insert(account.to_string(), token.clone());
        Some(token)
    }

    /// Sign-out (or a token the server refused): drop the token from memory
    /// and the secret store. The account and its workspace stay, locked.
    pub fn forget_token(&self, account: &str) {
        self.ws.lock().tokens.remove(account);
        self.set_token_ref(account, None, false);
    }

    /// The 2.1.x token found by the migration, once (08a §6.13 step 3).
    pub fn take_legacy_token(&self) -> Option<(String, String)> {
        self.ws.lock().legacy_token.take()
    }

    /// The active account's workspace as a sync document; `None` unless
    /// `account` is signed in and selected.
    pub fn sync_doc(&self, account: &str, with_tabs: bool, with_prefs: bool) -> Option<SyncDoc> {
        let ws = self.ws.lock();
        if ws.sel != WorkspaceSel::Account(account.to_string()) {
            return None;
        }
        let cfg = self.config.lock();
        Some(
            cfg.workspaces
                .get(account)
                .cloned()
                .unwrap_or_default()
                .sync_doc(with_tabs, with_prefs),
        )
    }

    /// Merge the account's document into its workspace — only if `account` is
    /// still the one signed in and selected, and no switch happened since the
    /// sync started (`epoch`, 08a §6.12 step 5). Returns whether it was applied.
    pub fn apply_sync(
        &self,
        account: &str,
        epoch: u64,
        doc: &SyncDoc,
        with_tabs: bool,
        with_prefs: bool,
    ) -> bool {
        {
            let mut ws = self.ws.lock();
            if ws.sel != WorkspaceSel::Account(account.to_string()) || ws.epoch != epoch {
                return false;
            }
            let now = ws.stamper.server_now();
            let mut cfg = self.config.lock();
            let w = cfg.workspace_mut(account);
            // The server's clock is known now: stamps this computer made while
            // its clock ran fast come back within the slack before merging, so
            // they cannot beat newer changes from other computers (I11).
            ws.stamper.cap_to_max();
            w.normalize(ws.stamper.max_stamp());
            w.apply_sync_doc(doc, now, with_tabs, with_prefs);
            ws.stamper.observe(w.latest_stamp());
            let tm = w.prefs.translate_modifiers;
            if let Some(a) = cfg.account_mut(account) {
                a.last_sync = now_ms();
            }
            self.translate_modifiers.store(tm, Ordering::Relaxed);
            self.save(&cfg);
        }
        let _ = self.ui.send(UiEvent::WorkspaceChanged);
        true
    }

    fn set_token_ref(&self, account: &str, sealed: Option<String>, keep: bool) {
        let old = {
            let mut cfg = self.config.lock();
            let Some(a) = cfg.account_mut(account) else {
                drop(cfg);
                self.forget_sealed(sealed.into_iter().collect());
                return;
            };
            let old = std::mem::replace(&mut a.token_ref, sealed);
            a.keep_signed_in = keep;
            self.save(&cfg);
            old
        };
        self.forget_sealed(old.into_iter().collect());
    }

    async fn seal_bounded(&self, plain: String) -> Option<String> {
        let store = self.platform.secrets();
        let seal = tokio::task::spawn_blocking(move || store.seal(&plain));
        match tokio::time::timeout(SECRET_PROMPT_TIMEOUT, seal).await {
            Ok(Ok(sealed)) => sealed,
            Ok(Err(e)) => {
                tracing::warn!("account: the secret store failed while sealing: {e}");
                None
            }
            Err(_) => {
                tracing::warn!("account: the secret store gave no answer in time");
                None
            }
        }
    }
}

/// The preferences in force for `sel`: its workspace's, or the locked
/// account's stored ones (see [`Engine::prefs`]).
pub(crate) fn prefs_in(cfg: &Config, sel: &WorkspaceSel) -> Prefs {
    let key = match sel {
        WorkspaceSel::Local => LOCAL_WORKSPACE,
        WorkspaceSel::Account(k) | WorkspaceSel::Locked(k) => k.as_str(),
    };
    cfg.workspaces.get(key).map(|w| w.prefs).unwrap_or_default()
}

impl WsState {
    /// Why a new connection is refused right now, if it is.
    pub(crate) fn connect_blocked(&self) -> Option<crate::ConnectError> {
        if self.switching > 0 || self.held {
            Some(crate::ConnectError::Switching)
        } else if matches!(self.sel, WorkspaceSel::Locked(_)) {
            Some(crate::ConnectError::SignedOut)
        } else {
            None
        }
    }
}
