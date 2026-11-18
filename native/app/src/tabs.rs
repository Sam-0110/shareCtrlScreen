//! The TabManager (plans/08 §3.4, 08a §4, §5.1, §6.1–§6.7): the one authority
//! for the tab strip. The web UI renders what it is told (a full
//! `tabs-changed` snapshot after every change) and calls the `tab_*`
//! commands; it holds no tab truth.
//!
//! * **Tabs are remotes.** A tab is identified by its remote ID, and its name
//!   is that remote's name. The open tabs, their order and the active one live
//!   in the active workspace (`Workspace::{tabs, active_tab}`), so they follow
//!   the account and survive a restart; what is live — the session, its phase,
//!   its surface — lives here and starts empty on every launch (nothing
//!   auto-connects, 08a §6.8).
//! * **One session per tab, the newest wins** (I4): a session's events apply to
//!   its tab only while it is that tab's current session, so late events from
//!   an ended session can never touch its successor.
//! * **Surfaces** are made for a connection before it is asked for, kept by
//!   session, and destroyed on the UI thread once that session is released —
//!   never while its threads may still draw into it (08a §6.5).
//! * **Locks.** `inner` is taken around the engine's `connect` (which emits the
//!   session's first event) and around every event, so the session id is
//!   recorded before any of its events is applied. Nothing waits on the UI
//!   thread, sends an event or awaits while holding it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use engine::{
    ConnectError, Engine, PwReason, Role, SessionEvent, SessionId, SessionPhase, TabStep,
};
use platform_api::VideoSurface;
use protocol::Permission;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::menu::update_monitor_menu;
use crate::surfaces::SurfaceFactory;

/// Connected tabs past which opening another asks first (D2: no cap, a
/// confirm).
pub(crate) const TAB_WARN_AT: usize = 8;

/// The gap between two starts of "Connect all" (08a §6.7): a burst of
/// simultaneous ICE gathers and host prompts helps nobody.
const CONNECT_ALL_STAGGER: Duration = Duration::from_millis(500);

/// How long a reconnect waits for the host's previous session to finish
/// tearing down (its threads joined; normally well under a second), and how
/// often it looks.
const ENDING_WAIT: Duration = Duration::from_secs(5);
const ENDING_POLL: Duration = Duration::from_millis(100);

/// How one connection attempt went.
enum Attempt {
    /// The host's previous session is still ending: try again shortly.
    Retry,
    Connected,
    /// Refused, failed or called off; the tab shows why.
    Stopped,
}

/// What a tab is doing (08a §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Disconnected,
    Connecting,
    Password(PwReason),
    Live,
}

/// A host's monitors as the native Monitor menu shows them.
#[derive(Clone, PartialEq)]
struct MonitorMenu {
    list: Vec<protocol::RemoteDisplay>,
    current: String,
    can_switch: bool,
    popouts: Vec<protocol::PopoutStream>,
}

/// The live half of one tab.
#[derive(Default)]
struct TabLive {
    /// Its current session; `None` while disconnected.
    session: Option<SessionId>,
    phase: Phase,
    permission: Option<Permission>,
    /// Seconds left of the link-trouble grace period, while it runs.
    trouble: Option<u32>,
    /// Why its last session ended, in a sentence (cleared on reconnect).
    last_end: Option<String>,
    /// The host's monitors, for the session bar (a `tab-displays` payload)
    /// and the native Monitor menu.
    displays: Option<Value>,
    menu: Option<MonitorMenu>,
}

#[derive(Default)]
struct Inner {
    /// By remote ID. Only tabs that were ever live in this run; the strip
    /// itself is the workspace's.
    live: HashMap<String, TabLive>,
    /// Each session's surface, until the session is released.
    surfaces: HashMap<SessionId, Arc<dyn VideoSurface>>,
    /// Remotes whose connection is being set up (its surface being made).
    starting: HashSet<String>,
    /// Of those, the ones the user disconnected or closed meanwhile: the
    /// setup stops before asking the host.
    cancelled: HashSet<String>,
    /// The tab on screen, as this computer's user last chose it (`Some(None)`:
    /// the connect page); `None` until the first choice, when the
    /// workspace's `active_tab` is shown. Kept apart from the workspace's
    /// copy because a sync may close the tab on screen elsewhere while it is
    /// connected here: it stays on screen until it disconnects (08a §6.12).
    shown: Option<Option<String>>,
    /// The workspace `shown` was chosen in: another account's choice does
    /// not carry over (each workspace has its own `active_tab`).
    shown_epoch: u64,
    /// The Monitor menu as last applied (`Some(None)`: emptied); `None`:
    /// unknown, so the next pass applies it.
    menu_applied: Option<Option<MonitorMenu>>,
}

/// The result of a command that may start connections.
pub(crate) enum Started {
    Done,
    /// It would pass [`TAB_WARN_AT`] connected tabs: nothing was done; the UI
    /// asks and repeats the command confirmed.
    NeedsConfirm {
        would_be: usize,
    },
}

impl Started {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Started::Done => json!({ "done": true }),
            Started::NeedsConfirm { would_be } => json!({ "needsConfirm": would_be }),
        }
    }
}

pub(crate) struct TabManager {
    app: AppHandle,
    engine: Arc<Engine>,
    surfaces: SurfaceFactory,
    inner: Mutex<Inner>,
    /// The app is exiting: no connection starts any more (Connect all
    /// included), so the sessions being ended stay ended (I9).
    closing: AtomicBool,
    /// Bumped by [`stop_starting`](Self::stop_starting): a connection being
    /// set up, and Connect all, stop when it changed since they began.
    stops: AtomicU64,
}

impl TabManager {
    pub(crate) fn new(app: AppHandle, engine: Arc<Engine>) -> Arc<Self> {
        Arc::new(TabManager {
            surfaces: SurfaceFactory::new(app.clone(), engine.clone()),
            app,
            engine,
            inner: Mutex::default(),
            closing: AtomicBool::new(false),
            stops: AtomicU64::new(0),
        })
    }

    /// Stop the connections being set up and a running Connect all (the
    /// window went to the tray: nothing may go live behind it). Later
    /// commands connect as usual.
    /// (Only the Windows service host hides to a tray.)
    #[cfg(windows)]
    pub(crate) fn stop_starting(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }

    /// The app is about to exit: start nothing new. Call before ending the
    /// sessions.
    pub(crate) fn begin_exit(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // The data stays consistent across a panic elsewhere (plain maps).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- commands ------------------------------------------------------------

    /// Open the tab for `remote` (or show the open one) and connect it unless
    /// it is connected already (08a §6.1).
    pub(crate) async fn open(
        self: &Arc<Self>,
        remote: &str,
        confirmed: bool,
    ) -> Result<Started, String> {
        let remote = remote.trim().to_lowercase();
        if !protocol::is_valid_uuid_v4(&remote) {
            return Err(ConnectError::InvalidId.to_string());
        }
        if remote == self.engine.config().uuid {
            return Err(ConnectError::SelfId.to_string());
        }
        let opened = self
            .engine
            .with_workspace(|w, s| {
                let now = s.now();
                let key = w.open_tab(&remote, now);
                if key.is_some() {
                    w.active_tab = key.clone();
                }
                key
            })
            .flatten();
        if opened.is_none() {
            return Err(ConnectError::SignedOut.to_string());
        }
        self.lock().shown = Some(opened);
        self.apply_active();
        if self.is_connected(&remote) {
            self.emit_snapshot();
            return Ok(Started::Done);
        }
        self.connect(&remote, confirmed).await
    }

    /// Connect the open tab for `remote` (08a §6.2). A refusal (the host is
    /// offline, this computer is being viewed, ...) is the tab's to show
    /// (`lastEnd`); only a command that cannot apply is an error.
    pub(crate) async fn connect(
        self: &Arc<Self>,
        remote: &str,
        confirmed: bool,
    ) -> Result<Started, String> {
        let remote = remote.trim().to_lowercase();
        if self.closing.load(Ordering::SeqCst) {
            return Err("The app is closing".into());
        }
        if self.engine.workspace().is_none() {
            return Err(ConnectError::SignedOut.to_string());
        }
        if !self.is_open(&remote) {
            return Err("That tab is not open".into());
        }
        // The workspace it is connected in: a switch while the surface is
        // being made stops it (it would start in the next one, unreachable).
        let epoch = self.engine.workspace_epoch();
        let stops = self.stops.load(Ordering::SeqCst);
        {
            let mut inner = self.lock();
            if inner.starting.contains(&remote) || connected(&inner, &remote) {
                return Ok(Started::Done);
            }
            let would_be = connected_count(&inner) + inner.starting.len() + 1;
            if would_be > TAB_WARN_AT && !confirmed {
                return Ok(Started::NeedsConfirm { would_be });
            }
            inner.starting.insert(remote.clone());
            let tab = inner.live.entry(remote.clone()).or_default();
            tab.phase = Phase::Connecting;
            tab.last_end = None;
        }
        self.emit_snapshot();
        let made = self.surfaces.create().await;
        // A session that just ended with this host may still be being torn
        // down (its threads joined): a quick reconnect waits for that.
        let deadline = Instant::now() + ENDING_WAIT;
        let connected = loop {
            match self.try_connect(&remote, &made, (epoch, stops), Instant::now() < deadline) {
                Attempt::Retry => tokio::time::sleep(ENDING_POLL).await,
                Attempt::Connected => break true,
                Attempt::Stopped => break false,
            }
        };
        // An unused surface goes, outside the lock (the UI thread does it).
        if let (false, Ok(surface)) = (connected, made) {
            self.surfaces.destroy(surface);
        }
        self.apply_active();
        Ok(Started::Done)
    }

    /// One attempt of [`connect`](Self::connect), under the lock the
    /// session's first event waits on, so its id is recorded first (I4).
    fn try_connect(
        &self,
        remote: &str,
        made: &Result<Arc<dyn VideoSurface>, String>,
        (epoch, stops): (u64, u64),
        may_retry: bool,
    ) -> Attempt {
        let mut inner = self.lock();
        let stopped = inner.cancelled.remove(remote)
            || self.closing.load(Ordering::SeqCst)
            || self.engine.workspace_epoch() != epoch
            || self.stops.load(Ordering::SeqCst) != stops
            || !self.is_open(remote);
        let refused = if stopped {
            None
        } else {
            match made {
                Err(e) => Some(e.clone()),
                Ok(surface) => match self.engine.connect(remote, surface.clone()) {
                    Ok(id) => {
                        inner.starting.remove(remote);
                        inner.surfaces.insert(id, surface.clone());
                        let tab = inner.live.entry(remote.to_string()).or_default();
                        tab.session = Some(id);
                        tab.phase = Phase::Connecting;
                        tab.trouble = None;
                        tab.displays = None;
                        tab.menu = None;
                        return Attempt::Connected;
                    }
                    Err(ConnectError::Ending(_)) if may_retry => return Attempt::Retry,
                    Err(e) => Some(e.to_string()),
                },
            }
        };
        inner.starting.remove(remote);
        if let Some(tab) = inner.live.get_mut(remote) {
            tab.phase = Phase::Disconnected;
            tab.last_end = refused;
        }
        Attempt::Stopped
    }

    /// End `remote`'s session (or its request); the tab stays.
    pub(crate) fn disconnect(&self, remote: &str) {
        let remote = remote.trim().to_lowercase();
        let session = {
            let mut inner = self.lock();
            if inner.starting.contains(&remote) {
                inner.cancelled.insert(remote.clone());
            }
            inner.live.get(&remote).and_then(|t| t.session)
        };
        if let Some(id) = session {
            self.engine.disconnect(id);
        }
    }

    /// Disconnect and close `remote`'s tab; its neighbour becomes active.
    pub(crate) fn close(&self, remote: &str) {
        self.disconnect(remote);
        let remote = remote.trim().to_lowercase();
        self.engine.with_workspace(|w, s| {
            let now = s.now();
            w.close_tab(&remote, now);
        });
        {
            let mut inner = self.lock();
            if inner.live.get(&remote).is_some_and(|t| t.session.is_none()) {
                inner.live.remove(&remote);
            }
            // The neighbour the workspace chose takes its place on screen.
            if inner.shown.as_ref() == Some(&Some(remote)) {
                inner.shown = None;
            }
        }
        self.apply_active();
    }

    /// Show `remote`'s tab (08a §6.4); `None`: the connect page.
    pub(crate) fn activate(&self, remote: Option<&str>) -> Result<(), String> {
        let remote = remote.map(|r| r.trim().to_lowercase());
        if let Some(r) = &remote {
            let shown = self
                .engine
                .workspace()
                .is_some_and(|w| visible(&w, &self.lock()).contains(r));
            if !shown {
                return Err("That tab is not open".into());
            }
        }
        self.engine.set_active_tab(remote.clone());
        let focus = {
            let mut inner = self.lock();
            inner.shown = Some(remote.clone());
            // Choosing a live tab (again) gives its video the keyboard, as a
            // click in it does: that is how the keys leave the web page.
            remote
                .as_ref()
                .and_then(|r| inner.live.get(r))
                .filter(|t| t.phase == Phase::Live)
                .and_then(|t| t.session)
                .and_then(|id| inner.surfaces.get(&id).cloned())
        };
        self.apply_active();
        if let Some(surface) = focus {
            surface.focus();
        }
        Ok(())
    }

    /// Show the next / previous tab by strip order, wrapping (D3).
    pub(crate) fn step(&self, step: TabStep) {
        let Some(ws) = self.engine.workspace() else {
            return;
        };
        let (open, active) = {
            let inner = self.lock();
            (visible(&ws, &inner), shown_tab(&ws, &inner))
        };
        if open.is_empty() {
            return;
        }
        let at = active
            .as_ref()
            .and_then(|a| open.iter().position(|k| k == a));
        let n = open.len();
        let next = match (at, step) {
            (Some(i), TabStep::Next) => (i + 1) % n,
            (Some(i), TabStep::Prev) => (i + n - 1) % n,
            (None, TabStep::Next) => 0,
            (None, TabStep::Prev) => n - 1,
        };
        let _ = self.activate(Some(&open[next]));
    }

    /// Rename the remote (and so its tab, everywhere it is shown).
    pub(crate) fn rename(&self, remote: &str, name: &str) {
        self.engine.set_remote_name(remote, name);
        self.emit_snapshot();
    }

    /// Move `remote`'s tab before `before`'s (`None`: to the end).
    pub(crate) fn move_tab(&self, remote: &str, before: Option<&str>) -> Result<(), String> {
        let moved = self
            .engine
            .with_workspace(|w, s| {
                let now = s.now();
                w.move_tab(remote, before, now)
            })
            .unwrap_or(false);
        self.emit_snapshot();
        if moved {
            Ok(())
        } else {
            Err("That tab is not open".into())
        }
    }

    /// Connect every disconnected open tab, one every
    /// [`CONNECT_ALL_STAGGER`] (08a §6.7). Stops if the workspace changes
    /// meanwhile (account switch, sign-out).
    pub(crate) fn connect_all(self: &Arc<Self>, confirmed: bool) -> Started {
        let Some(ws) = self.engine.workspace() else {
            return Started::Done;
        };
        let pending: Vec<String> = ws
            .open_tabs()
            .into_iter()
            .filter(|r| !self.is_connected(r))
            .collect();
        let would_be = connected_count(&self.lock()) + pending.len();
        if would_be > TAB_WARN_AT && !confirmed {
            return Started::NeedsConfirm { would_be };
        }
        let epoch = self.engine.workspace_epoch();
        let stops = self.stops.load(Ordering::SeqCst);
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            for (i, remote) in pending.into_iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(CONNECT_ALL_STAGGER).await;
                }
                if me.engine.workspace_epoch() != epoch || me.stops.load(Ordering::SeqCst) != stops
                {
                    return;
                }
                if let Err(e) = me.connect(&remote, true).await {
                    tracing::info!("connect all: {remote}: {e}");
                }
            }
        });
        Started::Done
    }

    /// Answer `remote`'s password prompt.
    pub(crate) fn password(&self, remote: &str, password: String, remember: bool) {
        let session = {
            let mut inner = self.lock();
            let remote = remote.trim().to_lowercase();
            let Some(tab) = inner.live.get_mut(&remote) else {
                return;
            };
            if !matches!(tab.phase, Phase::Password(_)) {
                return;
            }
            tab.phase = Phase::Connecting;
            tab.session
        };
        if let Some(id) = session {
            self.engine.submit_password(id, password, remember);
        }
        self.emit_snapshot();
    }

    /// Forget the saved password for `remote` in this workspace.
    pub(crate) fn forget_password(&self, remote: &str) {
        self.engine.forget_saved_login(remote);
        self.emit_snapshot();
    }

    // ---- engine events (from the bridge) --------------------------------------

    /// A viewer session's event (08a §5.1). `peer` is the remote: its tab.
    pub(crate) fn on_session(&self, id: SessionId, peer: &str, ev: SessionEvent) {
        let mut out: Vec<(&'static str, Value)> = Vec::new();
        let mut forget = false;
        let mut new_active: Option<Option<String>> = None;
        let mut released = None;
        {
            let mut inner = self.lock();
            if let SessionEvent::Released = ev {
                released = inner.surfaces.remove(&id);
            }
            // A tab closed on another computer while connected here stays
            // until it disconnects (08a §6.12 step 5). Read under `inner`, so a
            // sync landing meanwhile cannot leave a stale entry (lock order:
            // the TabManager's, then the engine's).
            let ws = self.engine.workspace();
            let still_open = ws
                .as_ref()
                .is_some_and(|w| w.tabs.get(peer).is_some_and(|t| t.is_open()));
            // If it is on screen and goes, its neighbour takes its place, as
            // for a close here.
            let was_shown = ws
                .as_ref()
                .is_some_and(|w| shown_tab(w, &inner).as_deref() == Some(peer));
            let neighbour = ws
                .as_ref()
                .filter(|_| was_shown)
                .and_then(|w| neighbour_of(&visible(w, &inner), peer));
            let Some(tab) = inner.live.get_mut(peer).filter(|t| t.session == Some(id)) else {
                // An earlier session of this tab (or a closed tab): only its
                // surface (above) still matters.
                drop(inner);
                if let Some(s) = released {
                    self.surfaces.destroy(s);
                }
                return;
            };
            match ev {
                SessionEvent::Phase(SessionPhase::Requesting) => tab.phase = Phase::Connecting,
                SessionEvent::Phase(SessionPhase::PasswordRequired { reason }) => {
                    tab.phase = Phase::Password(reason);
                }
                SessionEvent::Phase(SessionPhase::Negotiating { permission }) => {
                    tab.phase = Phase::Live;
                    tab.permission = Some(permission);
                }
                SessionEvent::Phase(SessionPhase::Ended(reason)) => {
                    tab.phase = Phase::Disconnected;
                    tab.session = None;
                    tab.trouble = None;
                    tab.displays = None;
                    tab.menu = None;
                    tab.permission = None;
                    tab.last_end = reason.message();
                    forget = !still_open;
                }
                SessionEvent::LinkTrouble { secs_left } => tab.trouble = Some(secs_left),
                SessionEvent::LinkRestored => tab.trouble = None,
                SessionEvent::Displays {
                    list,
                    current,
                    can_switch,
                    popouts,
                } => {
                    let mut d =
                        crate::commands::displays_json(&list, &current, can_switch, &popouts);
                    d["remoteId"] = json!(peer);
                    tab.displays = Some(d.clone());
                    tab.menu = Some(MonitorMenu {
                        list,
                        current,
                        can_switch,
                        popouts,
                    });
                    out.push(("tab-displays", d));
                }
                SessionEvent::Toast(message) => {
                    out.push(("tab-toast", json!({ "remoteId": peer, "message": message })));
                }
                SessionEvent::Released => {}
            }
            if forget {
                inner.live.remove(peer);
                if was_shown {
                    inner.shown = Some(neighbour.clone());
                    new_active = Some(neighbour);
                }
            }
        }
        if let Some(active) = new_active {
            self.engine.set_active_tab(active);
        }
        if let Some(s) = released {
            self.surfaces.destroy(s);
        }
        for (name, payload) in out {
            let _ = self.app.emit(name, payload);
        }
        self.apply_active();
    }

    /// The workspace changed (a sync, an account switch): forget live state of
    /// tabs that are no longer open and redraw.
    pub(crate) fn on_workspace_changed(&self) {
        let open: HashSet<String> = self
            .engine
            .workspace()
            .map(|w| w.open_tabs().into_iter().collect())
            .unwrap_or_default();
        let epoch = self.engine.workspace_epoch();
        {
            let mut inner = self.lock();
            inner
                .live
                .retain(|remote, t| open.contains(remote) || t.session.is_some());
            if inner.shown_epoch != epoch {
                inner.shown_epoch = epoch;
                inner.shown = None;
            }
        }
        self.apply_active();
    }

    // ---- helpers ---------------------------------------------------------------

    fn is_open(&self, remote: &str) -> bool {
        self.engine
            .workspace()
            .is_some_and(|w| w.tabs.get(remote).is_some_and(|t| t.is_open()))
    }

    fn is_connected(&self, remote: &str) -> bool {
        connected(&self.lock(), remote)
    }

    /// Put the workspace's active tab on screen (its session, if connected,
    /// else none: the UI shows that tab's card), then redraw.
    fn apply_active(&self) {
        let hosting = matches!(self.engine.role(), Role::Host { .. });
        let menu = {
            let mut inner = self.lock();
            let ws = self.engine.workspace();
            // A choice that left the strip is forgotten, so it cannot come back
            // by itself if that tab is reopened later.
            let stale = matches!(
                (&ws, &inner.shown),
                (Some(w), Some(Some(choice))) if !visible(w, &inner).contains(choice)
            );
            if stale {
                inner.shown = None;
            }
            let (session, menu) = ws
                .as_ref()
                .and_then(|ws| shown_tab(ws, &inner))
                .and_then(|r| inner.live.get(&r))
                .filter(|t| t.phase != Phase::Disconnected)
                .map(|t| (t.session, t.menu.clone()))
                .unwrap_or_default();
            // Read and applied under `inner`, so a click and a session event
            // deciding at the same time cannot cross (the later decision is
            // the one on screen). `set_active` never blocks.
            self.engine.set_active(session);
            // The Monitor menu belongs to the tab on screen — or, hosting, to
            // this computer's own monitors, whose events keep it. Rebuilt only
            // when it changes: it is a round trip to the UI thread.
            if hosting {
                inner.menu_applied = None;
                None
            } else if inner.menu_applied.as_ref() == Some(&menu) {
                None
            } else {
                inner.menu_applied = Some(menu.clone());
                Some(menu)
            }
        };
        if let Some(menu) = menu {
            match &menu {
                Some(m) => {
                    update_monitor_menu(&self.app, &m.list, &m.current, m.can_switch, &m.popouts)
                }
                None => update_monitor_menu(&self.app, &[], "", false, &[]),
            }
        }
        self.emit_snapshot();
    }

    /// The whole strip, as the UI draws it (08a §4.2).
    pub(crate) fn snapshot(&self) -> Value {
        let ws = self.engine.workspace();
        let hosting = matches!(self.engine.role(), Role::Host { .. });
        let Some(ws) = ws else {
            return json!({ "tabs": [], "active": null, "connected": 0, "hosting": hosting,
                           "signedOut": true });
        };
        let inner = self.lock();
        let tabs: Vec<Value> = visible(&ws, &inner)
            .into_iter()
            .map(|remote| {
                let live = inner.live.get(&remote);
                let starting = inner.starting.contains(&remote);
                let phase = live.map(|t| t.phase).unwrap_or_default();
                let (phase, reason) = match phase {
                    Phase::Disconnected if starting => ("connecting", None),
                    Phase::Disconnected => ("disconnected", None),
                    Phase::Connecting => ("connecting", None),
                    Phase::Password(PwReason::First) => ("password", Some("first")),
                    Phase::Password(PwReason::SavedRejected) => {
                        ("password", Some("saved-rejected"))
                    }
                    Phase::Live => ("live", None),
                };
                json!({
                    "remoteId": remote,
                    "name": ws.remote_name(&remote).unwrap_or(""),
                    "phase": phase,
                    "passwordReason": reason,
                    "permission": live.and_then(|t| t.permission).map(|p| p.as_str()),
                    "troubleSecs": live.and_then(|t| t.trouble),
                    "lastEnd": live.and_then(|t| t.last_end.clone()),
                    "autoSignIn": ws.saved_logins.contains_key(&remote),
                    "displays": live.and_then(|t| t.displays.clone()),
                    // Closed on another computer, still connected here: it goes
                    // when it disconnects, and cannot be moved meanwhile.
                    "closedElsewhere": !ws.tabs.get(&remote).is_some_and(|t| t.is_open()),
                })
            })
            .collect();
        json!({
            "tabs": tabs,
            "active": shown_tab(&ws, &inner),
            "connected": connected_count(&inner),
            "hosting": hosting,
            "signedOut": false,
        })
    }

    fn emit_snapshot(&self) {
        let _ = self.app.emit("tabs-changed", self.snapshot());
    }
}

fn connected(inner: &Inner, remote: &str) -> bool {
    inner
        .live
        .get(remote)
        .is_some_and(|t| t.session.is_some() && t.phase != Phase::Disconnected)
}

fn connected_count(inner: &Inner) -> usize {
    inner
        .live
        .values()
        .filter(|t| t.session.is_some() && t.phase != Phase::Disconnected)
        .count()
}

/// The tabs in the strip, in order: the open ones, and those closed on
/// another computer while connected here (they stay until they disconnect,
/// 08a §6.12 step 5).
fn visible(ws: &protocol::Workspace, inner: &Inner) -> Vec<String> {
    let mut tabs: Vec<(&str, &str)> = ws
        .tabs
        .iter()
        .filter(|(remote, t)| t.is_open() || connected(inner, remote))
        .map(|(remote, t)| (t.order.as_str(), remote.as_str()))
        .collect();
    tabs.sort_unstable();
    tabs.into_iter()
        .map(|(_, remote)| remote.to_string())
        .collect()
}

/// The tab on screen: the user's last choice while it is still in the strip,
/// else the workspace's active tab.
fn shown_tab(ws: &protocol::Workspace, inner: &Inner) -> Option<String> {
    let strip = visible(ws, inner);
    match &inner.shown {
        Some(choice) if choice.as_ref().is_none_or(|r| strip.contains(r)) => choice.clone(),
        _ => ws.active_tab.clone().filter(|r| strip.contains(r)),
    }
}

/// The tab next to `remote` in `strip` — the right-hand one, else the
/// left-hand one — as a close picks it (`Workspace::close_tab`).
fn neighbour_of(strip: &[String], remote: &str) -> Option<String> {
    let at = strip.iter().position(|r| r == remote)?;
    strip
        .get(at + 1)
        .or_else(|| at.checked_sub(1).and_then(|i| strip.get(i)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    fn workspace() -> protocol::Workspace {
        let mut w = protocol::Workspace::default();
        for (i, t) in [A, B, C].iter().enumerate() {
            w.open_tab(t, 10 + i as u64);
        }
        w
    }

    fn live(inner: &mut Inner, remote: &str) {
        let tab = inner.live.entry(remote.to_string()).or_default();
        tab.session = Some(SessionId(7));
        tab.phase = Phase::Live;
    }

    /// 08a §6.12 step 5: a tab closed elsewhere stays while connected here.
    #[test]
    fn a_tab_closed_elsewhere_stays_while_connected() {
        let mut w = workspace();
        let mut inner = Inner::default();
        w.close_tab(B, 20);
        assert_eq!(visible(&w, &inner), [A, C], "disconnected: gone at once");
        live(&mut inner, B);
        assert_eq!(
            visible(&w, &inner),
            [A, B, C],
            "connected: kept, in its place"
        );
    }

    #[test]
    fn the_tab_on_screen_is_the_users_choice_while_it_is_in_the_strip() {
        let mut w = workspace();
        let mut inner = Inner::default();
        w.active_tab = Some(A.into());
        assert_eq!(shown_tab(&w, &inner).as_deref(), Some(A), "no choice yet");
        inner.shown = Some(Some(B.into()));
        assert_eq!(shown_tab(&w, &inner).as_deref(), Some(B));
        inner.shown = Some(None);
        assert_eq!(shown_tab(&w, &inner), None, "the connect page");
        // The chosen tab was closed elsewhere and is not connected: the
        // workspace's active tab (if still in the strip) is shown instead.
        inner.shown = Some(Some(B.into()));
        w.close_tab(B, 20);
        assert_eq!(shown_tab(&w, &inner).as_deref(), Some(A));
        w.active_tab = Some(B.into());
        assert_eq!(shown_tab(&w, &inner), None);
    }

    #[test]
    fn a_neighbour_is_picked_as_a_close_picks_it() {
        let strip: Vec<String> = [A, B, C].iter().map(|s| s.to_string()).collect();
        assert_eq!(neighbour_of(&strip, B).as_deref(), Some(C));
        assert_eq!(neighbour_of(&strip, C).as_deref(), Some(B));
        assert_eq!(neighbour_of(&strip[..1], A), None);
        assert_eq!(neighbour_of(&strip, "nope"), None);
    }
}
