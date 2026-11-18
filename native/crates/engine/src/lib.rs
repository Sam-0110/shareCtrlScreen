//! Session orchestration (Plan 04 §4 `engine`). Ties the crates together and
//! runs the control plane: it consumes signaling events, drives the connection
//! handshake (contract §3.2, ported from `host.js`/`viewer.js`), enforces the
//! access model (approve popup / password challenge-response, live view↔control
//! switch), and owns the media session (capture→encode→transport on the host,
//! transport→decode→render on the viewer).
//!
//! Everything here is OS-independent: every OS service (capture, codecs, the
//! video surface, input injection, clipboard, secrets) comes from the
//! [`platform_api::Platform`] the app passes to [`Engine::start`].

pub mod handshake;
mod media;
mod modifiers;
mod session;
pub mod transfer;
mod tuning;
pub mod turn;
mod workspace;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use platform_api::{Codec, Platform, VideoSurface};
use protocol::{Config, ControlMsg, Mode, Permission, SignalData, SignalMsg};
use signaling::{Event as SigEvent, SignalingClient};
use std::sync::Arc;
use std::time::Duration;

pub use handshake::{ApprovalDecision, Decider};
pub use workspace::WorkspaceSel;

/// How long auto-login waits for the OS secret store to open a saved password
/// before asking the user instead. Long enough to answer a keyring unlock
/// prompt, short enough that a wedged store does not leave the connect hanging.
pub(crate) const SECRET_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Which side of a session this instance is currently playing. A machine
/// supports one active session at a time (contract §1 v1 simplification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Idle,
    /// Being controlled — we sent the offer and stream our screen.
    Host {
        peer: String,
        permission: Permission,
    },
    /// Controlling — we watch the peer's screen and send input.
    Viewer {
        peer: String,
        permission: Permission,
    },
}

/// Events the engine surfaces to the app UI (WebView2 via Tauri events).
#[derive(Debug, Clone)]
pub enum UiEvent {
    ServerStatus(&'static str),
    /// A viewer wants in (approve mode) — the UI shows the popup and calls back.
    ApprovalRequest {
        from: String,
    },
    /// Viewer side: the host demands a password; the UI opens the prompt and
    /// answers with [`Engine::submit_password`].
    PasswordRequired {
        from: String,
    },
    /// The active session changed.
    RoleChanged(Role),
    /// A transient user-facing message.
    Toast(String),
    /// The peer has gone quiet but the session is not given up on yet.
    /// `secs_left` counts down to the point where it is dropped. Emitted about
    /// once a second so the UI can show a live countdown instead of leaving the
    /// user staring at a frozen picture wondering whether it is still alive.
    LinkTrouble {
        secs_left: u32,
    },
    /// The peer answered again — the session continues normally.
    LinkRestored,
    /// The active workspace changed underneath the UI (a sync merged changes
    /// from another computer, or the account was switched): reload it.
    WorkspaceChanged,
    /// The host's monitors and the one being shared, for the monitor picker.
    /// On the host it comes from its own capture thread (at session start and
    /// after each switch); on the viewer from the host's `displays` message,
    /// and again whenever the viewer's control permission changes.
    /// `can_switch`: whether this side may switch right now (always on the
    /// host; only with control on the viewer).
    Displays {
        list: Vec<protocol::RemoteDisplay>,
        current: String,
        can_switch: bool,
        /// Monitors also streamed to pop-out windows (D14). The viewer's app
        /// opens a window per entry ([`Engine::attach_popout`]) and closes
        /// those no longer listed ([`Engine::close_popout`]).
        popouts: Vec<protocol::PopoutStream>,
    },
}

/// How a session finds a network path to the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IcePolicy {
    /// Host, STUN server-reflexive and TURN relay candidates (normal use).
    #[default]
    Full,
    /// Bind to 127.0.0.1, no STUN/TURN: both peers on this machine. For the
    /// loopback test harness and diagnostics.
    LoopbackOnly,
}

/// The peer a signaling message is from (or, for a relay error, about), which
/// picks the queue it is handled on. `None` for messages that concern no single
/// peer (TURN credentials, unknown kinds): those are handled immediately.
fn signal_peer(msg: &SignalMsg) -> Option<String> {
    match msg {
        SignalMsg::ConnectRequest { from, .. }
        | SignalMsg::ConnectResponse { from, .. }
        | SignalMsg::PasswordRequired { from, .. }
        | SignalMsg::Signal { from, .. }
        | SignalMsg::EndSession { from, .. } => from.clone(),
        SignalMsg::RelayError { to, .. } => to.clone(),
        _ => None,
    }
}

/// How long an unanswered connect-request keeps this computer busy for
/// incoming ones: the viewer's own 30 s answer timeout (contract §3.3) plus
/// room for the answer in flight. Past it the request is treated as abandoned
/// for the busy check, even if the UI never cancelled it, so a lost answer can
/// never leave this computer refusing everyone.
const OUTBOUND_BUSY_FOR: Duration = Duration::from_secs(35);

/// A connect-request this computer sent and is waiting on (viewer side).
struct Outbound {
    host: String,
    /// Until when it counts as "connecting" for the one-session rule (D1).
    /// `None` while the user is answering this host's password prompt: that
    /// wait has no limit, and the UI cancels it with `end_session`.
    busy_until: Option<std::time::Instant>,
}

impl Outbound {
    fn sent_to(host: String) -> Self {
        Outbound {
            host,
            busy_until: Some(std::time::Instant::now() + OUTBOUND_BUSY_FOR),
        }
    }

    fn is_busy(&self) -> bool {
        self.busy_until
            .is_none_or(|t| std::time::Instant::now() < t)
    }
}

/// Why [`Engine::connect_to`] refused to start a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectError {
    /// This computer is hosting, or letting a viewer in (D1).
    Hosting,
    /// The account is being switched (I10).
    Switching,
    /// The selected account is signed out: its computers are not reachable
    /// until its password is entered (I13).
    SignedOut,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConnectError::Hosting => {
                "Someone is connected or connecting to this computer: end that first"
            }
            ConnectError::Switching => "The account is being switched: try again in a moment",
            ConnectError::SignedOut => "Sign in to the account first",
        })
    }
}

impl std::error::Error for ConnectError {}

/// A password the viewer just submitted, held until the host accepts it: only
/// then is it worth saving, and a rejected guess is never persisted.
struct PendingLogin {
    host: String,
    plaintext: String,
    /// "Remember" was ticked (the default, D7).
    remember: bool,
    /// The workspace that was active when it was typed, where it is saved;
    /// `None` if none was reachable.
    workspace: Option<String>,
}

/// Holds [`Engine::admitting`] for one peer and clears it when dropped, so an
/// admission that ends any way (accepted, denied, a wrong password, a panic)
/// never leaves this computer stuck "busy".
struct Admission<'a> {
    engine: &'a Engine,
    peer: String,
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let mut admitting = self.engine.admitting.lock();
        if admitting.as_deref() == Some(self.peer.as_str()) {
            *admitting = None;
        }
    }
}

/// The engine. Owns config, the signaling client, the current role and session.
pub struct Engine {
    config_path: PathBuf,
    config: Arc<Mutex<Config>>,
    /// False when the config must not be written back this run (see
    /// [`protocol::Loaded::writable`]): every save is skipped, so the user's
    /// file is never overwritten from a state that could lose data.
    writable: AtomicBool,
    /// The workspace store's state: which workspace is reachable, the stamper,
    /// the switch epoch, live tokens (workspace.rs). Locked before `config`.
    ws: Mutex<workspace::WsState>,
    /// Woken after each change to the active workspace (account sync).
    ws_changed: Arc<tokio::sync::Notify>,
    signaling: SignalingClient,
    role: Arc<Mutex<Role>>,
    /// Whether the signaling socket is currently registered with the server. The
    /// UI queries this on boot because the one-shot `ServerStatus` event can fire
    /// before the WebView subscribes (startup race — the dot would stay stuck).
    connected: Arc<AtomicBool>,
    decider: Arc<dyn Decider>,
    ui: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    /// Pending host password nonces per requesting peer (approve/challenge).
    pending_nonce: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// Viewer decode capabilities per requesting peer, for codec negotiation (§3).
    pending_caps: Arc<Mutex<std::collections::HashMap<String, Vec<String>>>>,
    /// Requesting peers that advertised pop-out support (D14); consumed at
    /// accept time like `pending_caps`.
    popout_peers: Arc<Mutex<std::collections::HashSet<String>>>,
    /// TURN relay servers minted by the signaling server (Cloudflare TURN),
    /// expanded to one `IceServer` per URL. Fetched on register and cached; the
    /// pipeline reads them when gathering the relayed candidate.
    turn_servers: Arc<Mutex<Vec<protocol::IceServer>>>,
    /// Viewer: the password just submitted, held until the host accepts it.
    /// Only then is it worth saving — a rejected guess must never be persisted.
    /// See [`PendingLogin`].
    pending_login: Arc<Mutex<Option<PendingLogin>>>,
    /// Viewer: the host we last auto-submitted a saved password to. If that host
    /// challenges us again, the saved credential is stale — drop it and fall back
    /// to prompting, instead of looping on a password that no longer works.
    auto_attempted: Arc<Mutex<Option<String>>>,
    /// Viewer: the host we sent a connect-request to and are waiting on. A
    /// connect-response from anyone else is ignored — otherwise any peer that
    /// knows our id could make us start a viewer session with it.
    outbound: Mutex<Option<Outbound>>,
    /// Host: the one peer currently being admitted — its approval prompt is open
    /// or its password proof is being checked. While it is set, every other
    /// connect-request is answered "busy" and no outgoing session may start, so
    /// "one session at a time" (plans/08 D1) still holds now that each peer's
    /// signaling runs on its own task (see [`Engine::dispatch_signaling`]).
    admitting: Mutex<Option<String>>,
    /// Per-peer signaling queues: each peer's messages are handled in arrival
    /// order on that peer's own task, so a slow step for one peer (an approval
    /// prompt, a keyring unlock, ICE gathering) never stalls another peer's.
    peer_queues:
        Mutex<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<SignalMsg>>>,
    /// OS services.
    platform: Arc<dyn Platform>,
    /// The viewer's native video surface, attached by the app once its window
    /// exists (`None` until then, and on platforms without one).
    surface: Mutex<Option<Arc<dyn VideoSurface>>>,
    /// The codec of the current/next session: chosen by the host at accept time,
    /// adopted by the viewer from the connect-response.
    negotiated: Mutex<Codec>,
    /// Viewer: the host's OS from its connect-response (unknown when it did not
    /// say). It arrives before the host's offer, so every viewer session knows
    /// it before its first key event.
    peer_os: Mutex<modifiers::OsFamily>,
    /// The live "Translate modifiers for the remote OS" setting, shared with
    /// the running session's key translator so a change applies to the next
    /// key without a reconnect. Kept equal to `config.translate_modifiers` by
    /// [`Engine::update_config`].
    translate_modifiers: Arc<AtomicBool>,
    /// The live session, if any (one at a time).
    pub(crate) session: Mutex<Option<session::Session>>,
    ice_policy: Mutex<IcePolicy>,
}

impl Engine {
    /// Load config from the platform's config dir, spawn signaling for our UUID,
    /// and return the engine plus the UI and signaling event streams. `decider`
    /// answers approve-mode popups (the app wires it to its modal). Must be called
    /// inside an entered Tokio runtime.
    pub fn start(
        platform: Arc<dyn Platform>,
        decider: Arc<dyn Decider>,
    ) -> (
        Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
        tokio::sync::mpsc::UnboundedReceiver<SigEvent>,
    ) {
        let config_path = platform.paths().config_dir.join("config.json");
        let loaded = Config::load(&config_path);
        for note in &loaded.notes {
            tracing::warn!("{note}");
        }
        let config = loaded.config;
        let (signaling, sig_rx) =
            SignalingClient::spawn(config.server_url.clone(), config.uuid.clone());
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let ws = workspace::WsState::at_launch(&config, loaded.legacy_token);
        // The selected workspace's setting (a signed-out account's stored one).
        let prefs = workspace::prefs_in(&config, &ws.sel);
        let translate_modifiers = Arc::new(AtomicBool::new(prefs.translate_modifiers));
        let engine = Arc::new(Self {
            config_path,
            config: Arc::new(Mutex::new(config)),
            writable: AtomicBool::new(loaded.writable),
            ws: Mutex::new(ws),
            ws_changed: Arc::new(tokio::sync::Notify::new()),
            signaling,
            role: Arc::new(Mutex::new(Role::Idle)),
            connected: Arc::new(AtomicBool::new(false)),
            decider,
            ui: ui_tx,
            pending_nonce: Arc::new(Mutex::new(Default::default())),
            pending_caps: Arc::new(Mutex::new(Default::default())),
            popout_peers: Arc::new(Mutex::new(Default::default())),
            turn_servers: Arc::new(Mutex::new(Vec::new())),
            pending_login: Arc::new(Mutex::new(None)),
            auto_attempted: Arc::new(Mutex::new(None)),
            outbound: Mutex::new(None),
            admitting: Mutex::new(None),
            peer_queues: Mutex::new(Default::default()),
            platform,
            surface: Mutex::new(None),
            negotiated: Mutex::new(Codec::H264),
            peer_os: Mutex::new(modifiers::OsFamily::Unknown),
            translate_modifiers,
            session: Mutex::new(None),
            ice_policy: Mutex::new(IcePolicy::Full),
        });
        // Host passwords the migration did not carry over (automatic sign-in
        // was off): their OS secret-store items go too — but only once the
        // migrated file is on disk; until then the old file still refers to
        // them (P1 review).
        if loaded.persisted {
            engine.forget_sealed(loaded.forget_sealed);
        }
        (engine, ui_rx, sig_rx)
    }

    pub fn config(&self) -> Config {
        self.config.lock().clone()
    }

    pub fn role(&self) -> Role {
        self.role.lock().clone()
    }

    /// Whether signaling is currently registered with the server. Queried by the
    /// UI on boot to back-fill the status dot past the startup event race.
    pub fn server_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether this run may write the config file (see `writable`).
    pub fn is_writable(&self) -> bool {
        self.writable.load(Ordering::Relaxed)
    }

    /// Persist a change to this computer's own settings (Settings → device
    /// fields). The person's data goes through [`Engine::with_workspace`].
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.config.lock();
        f(&mut cfg);
        self.save(&cfg);
    }

    /// Write `cfg` to disk, unless this run must not (see `writable`). A
    /// failed write is logged; the in-memory value stays authoritative.
    pub(crate) fn save(&self, cfg: &Config) {
        if !self.writable.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = cfg.persist(&self.config_path) {
            tracing::warn!(
                "config: could not save {} ({e})",
                self.config_path.display()
            );
        }
    }

    /// Dispatch one inbound signaling event without waiting for it to be handled.
    /// The app's signaling loop calls this.
    ///
    /// Messages from (or about) a peer go to that peer's own queue, handled in
    /// arrival order on its own task; everything else is handled right here.
    /// Ordering only matters within one peer (its connect-response must precede
    /// its offer), and a step that waits — the host's approval prompt, a keyring
    /// unlock for a saved password, ICE gathering — must not hold up any other
    /// peer, nor the registration and TURN bookkeeping below.
    pub async fn dispatch_signaling(self: &Arc<Self>, event: SigEvent) {
        let SigEvent::Message(mut msg) = event else {
            self.handle_signaling(event).await;
            return;
        };
        let Some(peer) = signal_peer(&msg) else {
            self.route(msg).await;
            return;
        };
        // Send under the map lock: the worker checks "empty" and removes its
        // entry under the same lock, so a message can never be queued on a
        // worker that has just decided to exit.
        let mut queues = self.peer_queues.lock();
        if let Some(tx) = queues.get(&peer) {
            match tx.send(msg) {
                Ok(()) => return,
                // The worker is gone (it panicked): start a fresh one below.
                Err(returned) => msg = returned.0,
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(msg);
        queues.insert(peer.clone(), tx);
        drop(queues);
        let engine = Arc::clone(self);
        tokio::spawn(async move { engine.peer_worker(peer, rx).await });
    }

    /// One peer's signaling, in order. Exits after a quiet minute (removing its
    /// queue under the map lock only if nothing arrived meanwhile).
    async fn peer_worker(
        self: Arc<Self>,
        peer: String,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<SignalMsg>,
    ) {
        const IDLE: Duration = Duration::from_secs(60);
        loop {
            match tokio::time::timeout(IDLE, rx.recv()).await {
                Ok(Some(msg)) => self.route(msg).await,
                Ok(None) => return,
                Err(_) => {
                    let mut queues = self.peer_queues.lock();
                    if rx.is_empty() {
                        queues.remove(&peer);
                        return;
                    }
                }
            }
        }
    }

    /// Drive one inbound signaling event to completion. Reached through
    /// [`Engine::dispatch_signaling`], which calls it for the events that
    /// concern no single peer (registration, server loss).
    pub async fn handle_signaling(&self, event: SigEvent) {
        match event {
            SigEvent::Registered => {
                tracing::info!("signaling: registered with server");
                self.connected.store(true, Ordering::Relaxed);
                let _ = self.ui.send(UiEvent::ServerStatus("connected"));
                // Fetch TURN relay credentials now so they're cached before the
                // user starts a session (needed for cross-network / strict NAT).
                let _ = self.signaling.send(SignalMsg::Turn);
            }
            SigEvent::RegisterError(reason) => {
                let _ = self
                    .ui
                    .send(UiEvent::Toast(format!("server rejected: {reason}")));
            }
            SigEvent::Disconnected => {
                self.connected.store(false, Ordering::Relaxed);
                let _ = self.ui.send(UiEvent::ServerStatus("reconnecting"));
            }
            SigEvent::Message(msg) => self.route(msg).await,
        }
    }

    /// Router mirroring signaling.js `route()` → host/viewer handlers.
    async fn route(&self, msg: SignalMsg) {
        match msg {
            SignalMsg::ConnectRequest {
                from: Some(from),
                password,
                caps,
                ..
            } => {
                // Only a first attempt carries caps (a password retry sends
                // none, and keeps what the first said); each first attempt
                // replaces what an earlier, possibly declined one said.
                if let Some(c) = &caps {
                    if c.popout {
                        self.popout_peers.lock().insert(from.clone());
                    } else {
                        self.popout_peers.lock().remove(&from);
                    }
                }
                let decode = caps.map(|c| c.decode).unwrap_or_default();
                self.on_connect_request(from, password, decode).await;
            }
            SignalMsg::ConnectResponse {
                from: Some(from),
                accepted,
                permission,
                reason,
                codec,
                os,
                ..
            } => {
                self.on_connect_response(from, accepted, permission, reason, codec, os)
                    .await;
            }
            SignalMsg::PasswordRequired {
                from: Some(from),
                nonce,
                ..
            } => {
                self.on_password_required(from, nonce).await;
            }
            SignalMsg::Signal {
                from: Some(from),
                data,
                ..
            } => {
                self.on_signal(from, data).await;
            }
            SignalMsg::EndSession {
                from: Some(from), ..
            } => {
                self.on_remote_end(from);
            }
            SignalMsg::RelayError { reason, to } if reason == "peer-offline" => {
                // Only react when the offline peer is the one we are in (or
                // trying to start) a session with — not any id the relay reports.
                let ours = match to.as_deref() {
                    Some(id) => self.is_session_peer(id),
                    None => true, // older servers omit `to`
                };
                if ours {
                    let _ = self.ui.send(UiEvent::Toast("Peer offline".into()));
                    self.end_session_local();
                }
            }
            SignalMsg::TurnCredentials { ice_servers, error } => {
                self.on_turn_credentials(ice_servers, error);
            }
            _ => {} // contract §3: unknown/others ignored
        }
    }

    /// Cache TURN relay servers minted by the signaling server. Each URL becomes
    /// one `IceServer` sharing the username/credential; the pipeline gathers a
    /// relayed candidate from the `turn:` entries.
    fn on_turn_credentials(
        &self,
        ice_servers: Option<protocol::IceServers>,
        error: Option<String>,
    ) {
        if let Some(err) = error {
            tracing::warn!("TURN credentials unavailable: {err} (cross-network relay disabled)");
            return;
        }
        let Some(servers) = ice_servers else { return };
        let expanded: Vec<protocol::IceServer> = servers
            .urls
            .iter()
            .map(|url| protocol::IceServer {
                urls: url.clone(),
                username: servers.username.clone(),
                credential: servers.credential.clone(),
            })
            .collect();
        let turn_count = expanded
            .iter()
            .filter(|s| s.urls.starts_with("turn"))
            .count();
        tracing::info!("TURN credentials received: {turn_count} relay URL(s) cached");
        *self.turn_servers.lock() = expanded;
    }

    /// The cached TURN/STUN relay servers (from the signaling server).
    pub fn turn_servers(&self) -> Vec<protocol::IceServer> {
        self.turn_servers.lock().clone()
    }

    // ---- Host side (contract §3.2 step 2) --------------------------------

    async fn on_connect_request(
        &self,
        from: String,
        password: Option<String>,
        decode_caps: Vec<String>,
    ) {
        let cfg = self.config.lock().clone();
        let mode = cfg.effective_mode();
        // An approval prompt, or a password proof answering a challenge we
        // issued, is an admission in progress. A bare first password request
        // (we only send a challenge) is not — the viewer may never answer it —
        // and neither is a proof with no challenge behind it: it cannot succeed,
        // and letting it claim the admission would let any peer that knows our
        // ID keep this computer "busy" by streaming bad proofs.
        let admits = match mode {
            Mode::Approve => true,
            Mode::Password => password.is_some() && self.pending_nonce.lock().contains_key(&from),
        };

        // Busy: one session at a time, and one admission at a time. Checked and
        // claimed under the same locks, because each peer is handled on its own
        // task: two requests (or a request and our own outgoing connect) must not
        // both get through. Lock order: role → admitting → outbound.
        let busy = {
            let role = self.role.lock();
            let mut admitting = self.admitting.lock();
            let busy = !matches!(*role, Role::Idle)
                || admitting.as_deref().is_some_and(|p| p != from)
                || self.outbound.lock().as_ref().is_some_and(Outbound::is_busy);
            if !busy && admits {
                *admitting = Some(from.clone());
            }
            busy
        };
        if busy {
            let _ = self.signaling.send(SignalMsg::ConnectResponse {
                to: Some(from),
                from: None,
                accepted: false,
                permission: None,
                reason: Some("busy".into()),
                codec: None,
                os: None,
            });
            return;
        }
        // Released however the admission ends (accepted, denied, wrong password).
        // On accept the role is Host before this drops, so there is no gap.
        let admission = admits.then(|| Admission {
            engine: self,
            peer: from.clone(),
        });

        // Remember the viewer's decode caps for codec negotiation at accept time.
        // Only the first attempt carries caps; a password retry sends none, so
        // don't clobber the stored list with an empty one.
        if !decode_caps.is_empty() {
            self.pending_caps.lock().insert(from.clone(), decode_caps);
        }

        match mode {
            Mode::Password => {
                self.handle_password_request(from, password, &cfg, admission)
                    .await
            }
            Mode::Approve => {
                self.handle_approve_request(from).await;
                drop(admission);
            }
        }
    }

    /// `admission` is held while the proof is checked and through `accept`; a
    /// wrong proof releases it before the anti-guessing delay, which slows
    /// that one peer down but must not keep everyone else out.
    async fn handle_password_request(
        &self,
        from: String,
        password: Option<String>,
        cfg: &Config,
        admission: Option<Admission<'_>>,
    ) {
        match password {
            None => {
                // Issue a fresh nonce (contract §3.2).
                let nonce = handshake::random_nonce();
                self.pending_nonce
                    .lock()
                    .insert(from.clone(), nonce.clone());
                let _ = self.signaling.send(SignalMsg::PasswordRequired {
                    to: Some(from),
                    from: None,
                    nonce,
                });
            }
            Some(proof) => {
                let nonce = self.pending_nonce.lock().remove(&from).unwrap_or_default();
                if admission.is_some() && cfg.verify_proof(&nonce, &proof) {
                    self.accept(from, cfg.password_permission).await;
                    drop(admission);
                } else {
                    drop(admission);
                    // §6 security: 2 s artificial delay before rejecting.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let _ = self.signaling.send(SignalMsg::ConnectResponse {
                        to: Some(from),
                        from: None,
                        accepted: false,
                        permission: None,
                        reason: Some("bad-password".into()),
                        codec: None,
                        os: None,
                    });
                }
            }
        }
    }

    async fn handle_approve_request(&self, from: String) {
        let _ = self
            .ui
            .send(UiEvent::ApprovalRequest { from: from.clone() });
        match self.decider.decide(&from).await {
            ApprovalDecision::Deny => {
                let _ = self.signaling.send(SignalMsg::ConnectResponse {
                    to: Some(from),
                    from: None,
                    accepted: false,
                    permission: None,
                    reason: Some("denied".into()),
                    codec: None,
                    os: None,
                });
            }
            ApprovalDecision::Allow(permission) => self.accept(from, permission).await,
        }
    }

    /// Accept a viewer with `permission`, become Host, and begin negotiation as
    /// the offerer (the host owns the media, contract §3.2 step 4).
    async fn accept(&self, peer: String, permission: Permission) {
        // Become Host first, under the role lock, and only from Idle. The
        // admission held by our caller already keeps every other session out
        // (`connect_to` and `on_connect_response` check it under the same
        // locks), so this cannot fail today; it is the last line of the
        // one-session rule (D1) should that ever change.
        {
            let mut role = self.role.lock();
            if !matches!(*role, Role::Idle) {
                drop(role);
                tracing::warn!("not accepting {peer}: a session started meanwhile");
                let _ = self.signaling.send(SignalMsg::ConnectResponse {
                    to: Some(peer),
                    from: None,
                    accepted: false,
                    permission: None,
                    reason: Some("busy".into()),
                    codec: None,
                    os: None,
                });
                return;
            }
            *role = Role::Host {
                peer: peer.clone(),
                permission,
            };
        }
        // Codec negotiation from the viewer's advertised caps (§3) — done before
        // the response so the viewer is told which codec to decode with.
        let decode = self.pending_caps.lock().remove(&peer).unwrap_or_default();
        let viewer: Vec<Codec> = decode
            .iter()
            .filter_map(|s| Codec::from_caps_str(s))
            .collect();
        let chosen = Codec::negotiate(&self.platform.codecs().host_encodable(), &viewer);
        *self.negotiated.lock() = chosen;
        tracing::info!("negotiated codec: {}", chosen.as_caps_str());
        let codec = chosen.as_caps_str().to_string();

        let _ = self.signaling.send(SignalMsg::ConnectResponse {
            to: Some(peer.clone()),
            from: None,
            accepted: true,
            permission: Some(permission.as_str().into()),
            reason: None,
            codec: Some(codec),
            // The viewer translates modifier keys for this OS (D8).
            os: modifiers::OsFamily::from_name(self.platform.info().name)
                .wire_name()
                .map(String::from),
        });
        let _ = self.ui.send(UiEvent::RoleChanged(self.role.lock().clone()));
        self.add_recent(&peer);
        let popouts = self.popout_peers.lock().remove(&peer);
        session::begin_host(self, peer, permission, popouts);
    }

    // ---- Viewer side (contract §3.2) -------------------------------------

    /// UI action: connect out to `host_uuid`. Sends the first connect-request.
    ///
    /// Refused mid-switch, while the selected account is signed out (I10,
    /// I13), and while this computer is hosting or letting a viewer in (D1).
    /// All checked and claimed in one critical section — the workspace state,
    /// then role → admitting → outbound, the same order everywhere — so a
    /// switch cannot slip between the check and the claim: either it sees this
    /// request and ends it, or this request sees the switch and is refused.
    /// The recent is written to the workspace the request started in.
    pub fn connect_to(&self, host_uuid: String) -> Result<(), ConnectError> {
        // Advertise only codecs this viewer can actually hardware-decode. A
        // hardcoded list (e.g. always claiming AV1) makes the host negotiate a
        // codec we cannot decode → the viewer media loop dies → black screen.
        let decode: Vec<String> = self
            .platform
            .codecs()
            .viewer_decodable()
            .iter()
            .map(|c| c.as_caps_str().to_string())
            .collect();
        let started_in = {
            let ws = self.ws.lock();
            if let Some(e) = ws.connect_blocked() {
                return Err(e);
            }
            let role = self.role.lock();
            let admitting = self.admitting.lock();
            if matches!(*role, Role::Host { .. }) || admitting.is_some() {
                return Err(ConnectError::Hosting);
            }
            *self.outbound.lock() = Some(Outbound::sent_to(host_uuid.clone()));
            // Queued (not awaited) inside the section, so a switch's
            // end-session for this request can only follow it, never precede it.
            let _ = self.signaling.send(SignalMsg::ConnectRequest {
                to: Some(host_uuid.clone()),
                from: None,
                password: None,
                caps: Some(protocol::signaling::Caps {
                    decode,
                    popout: true,
                }),
            });
            ws.sel.workspace_key().map(|k| (k.to_string(), ws.epoch))
        };
        if let Some((key, epoch)) = started_in {
            self.add_recent_in(&key, epoch, &host_uuid);
        }
        Ok(())
    }

    /// UI action: clear the recent-ids list. Returns the (now empty) list.
    pub fn clear_recents(&self) -> Vec<String> {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.clear_recents(now);
        });
        Vec::new()
    }

    /// UI action: name, rename or (with an empty `name`) un-name a remote ID.
    /// Returns every name, so the UI can redraw from one source of truth.
    pub fn set_remote_name(
        &self,
        id: &str,
        name: &str,
    ) -> std::collections::BTreeMap<String, String> {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.set_remote_name(id, name, now);
            w.remote_names.clone()
        })
        .unwrap_or_default()
    }

    /// Record a connection to `peer` in the recents of the active workspace
    /// (none while signed out). Persisted, so recents survive a restart.
    fn add_recent(&self, peer: &str) {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.add_recent(peer, now);
        });
    }

    async fn on_password_required(&self, from: String, nonce: String) {
        self.pending_nonce.lock().insert(from.clone(), nonce);

        // A second challenge from a host we just auto-submitted to means the
        // saved password is no longer valid. Forget it and prompt normally,
        // rather than retrying a dead credential forever.
        let stale = self.auto_attempted.lock().as_deref() == Some(from.as_str());
        if stale {
            *self.auto_attempted.lock() = None;
            self.forget_saved_login(&from);
            let _ = self
                .ui
                .send(UiEvent::Toast("Saved password no longer works".into()));
            self.prompt_for_password(from);
            return;
        }

        // Auto-login: replay a password this host has already accepted.
        if let Some(plain) = self.saved_login(&from).await {
            *self.auto_attempted.lock() = Some(from.clone());
            tracing::info!("auto-login: submitting saved password for {from}");
            self.submit_password(from, plain, false);
            return;
        }
        self.prompt_for_password(from);
    }

    /// Ask the user for `host`'s password. While the prompt is up the request
    /// waits on the user, not the network, so it keeps this computer busy for
    /// as long as it takes; Cancel ends it through `end_session`.
    fn prompt_for_password(&self, host: String) {
        if let Some(o) = self.outbound.lock().as_mut() {
            if o.host == host {
                o.busy_until = None;
            }
        }
        let _ = self.ui.send(UiEvent::PasswordRequired { from: host });
    }

    /// The saved password for `host` in the active workspace, if one is saved
    /// and the secret store opens it. Saved passwords are used whenever
    /// present (D7: typed once per host, remembered by default).
    ///
    /// `open` runs off the runtime and is bounded: a locked OS keyring shows an
    /// unlock prompt and waits for the user, and this runs on the host's
    /// signaling task, which other work for that host waits behind. No answer
    /// in time means "no saved password" and the user is asked normally.
    async fn saved_login(&self, host: &str) -> Option<String> {
        let sealed = self
            .with_workspace_read(|w| w.saved_logins.get(host).cloned())
            .flatten()?;
        let store = self.platform.secrets();
        let open = tokio::task::spawn_blocking(move || store.open(&sealed));
        match tokio::time::timeout(SECRET_PROMPT_TIMEOUT, open).await {
            Ok(Ok(plain)) => plain,
            Ok(Err(e)) => {
                tracing::warn!("auto-login: secret store failed while opening: {e}");
                None
            }
            Err(_) => {
                tracing::warn!(
                    "auto-login: the secret store gave no answer in {}s (an OS unlock prompt?) \
                     — asking for the password instead",
                    SECRET_PROMPT_TIMEOUT.as_secs()
                );
                None
            }
        }
    }

    /// Remove `host`'s saved password from the active workspace and let the
    /// OS store drop its copy.
    fn forget_saved_login(&self, host: &str) {
        let removed = self
            .with_workspace(|w, _| w.saved_logins.remove(host))
            .flatten();
        self.forget_sealed(removed.into_iter().collect());
    }

    /// Hand sealed values whose config entries are gone to the store's `forget`,
    /// off the runtime (a keyring may prompt); nothing waits for it.
    pub(crate) fn forget_sealed(&self, sealed: Vec<String>) {
        if sealed.is_empty() {
            return;
        }
        let store = self.platform.secrets();
        tokio::task::spawn_blocking(move || {
            for s in &sealed {
                store.forget(s);
            }
        });
    }

    /// Seal `plaintext` and save it for `host` in `workspace` — the one that
    /// was active when the password was typed, so a password never lands in
    /// another account's workspace (I13). Detached from the caller: the host
    /// has just accepted the password, and `seal` may sit behind an OS unlock
    /// prompt that must not stall signaling. Reported through the log and a
    /// toast.
    fn save_login(&self, workspace: String, host: String, plaintext: String) {
        let store = self.platform.secrets();
        let config = Arc::clone(&self.config);
        let config_path = self.config_path.clone();
        let writable = self.writable.load(Ordering::Relaxed);
        let ui = self.ui.clone();
        tokio::spawn(async move {
            let seal = {
                let store = Arc::clone(&store);
                tokio::task::spawn_blocking(move || store.seal(&plaintext))
            };
            let sealed = match seal.await {
                Ok(Some(sealed)) => sealed,
                Ok(None) => {
                    tracing::warn!("auto-login: could not encrypt password — not saving");
                    let _ = ui.send(UiEvent::Toast(
                        "Could not save the password securely".into(),
                    ));
                    return;
                }
                Err(e) => {
                    tracing::warn!("auto-login: secret store failed while sealing: {e}");
                    let _ = ui.send(UiEvent::Toast(
                        "Could not save the password securely".into(),
                    ));
                    return;
                }
            };
            // Released: the entry this one replaced, or this one if its
            // workspace is gone (its account was removed meanwhile).
            let release = {
                let mut cfg = config.lock();
                let release = match cfg.workspaces.get_mut(&workspace) {
                    None => Some(sealed),
                    Some(w) => {
                        tracing::info!("auto-login: saved password for {host}");
                        w.saved_logins.insert(host, sealed)
                    }
                };
                if writable {
                    if let Err(e) = cfg.persist(&config_path) {
                        tracing::warn!("config: could not save ({e})");
                    }
                }
                release
            };
            if let Some(old) = release {
                tokio::task::spawn_blocking(move || store.forget(&old));
            }
        });
    }

    /// UI action: answer a `password-required` prompt (viewer side). `remember`
    /// asks to save the password — but only once the host has actually
    /// accepted it (see `on_connect_response`).
    pub fn submit_password(&self, host: String, plaintext: String, remember: bool) {
        let nonce = self.pending_nonce.lock().remove(&host).unwrap_or_default();
        // proof = SHA256( SHA256(plaintext) + ":" + nonce ).
        let proof = handshake::compute_proof(&plaintext, &nonce);
        let workspace = self.workspace_sel().workspace_key().map(String::from);
        *self.pending_login.lock() = Some(PendingLogin {
            host: host.clone(),
            plaintext,
            remember,
            workspace,
        });
        // Waiting on the network again: the answer timeout starts over.
        if let Some(o) = self.outbound.lock().as_mut() {
            if o.host == host {
                *o = Outbound::sent_to(host.clone());
            }
        }
        let _ = self.signaling.send(SignalMsg::ConnectRequest {
            to: Some(host),
            from: None,
            password: Some(proof),
            caps: None,
        });
    }

    /// UI action: forget every saved host password of the active workspace
    /// (other accounts' are untouched).
    pub fn clear_saved_logins(&self) {
        let all = self
            .with_workspace(|w, _| std::mem::take(&mut w.saved_logins).into_values().collect())
            .unwrap_or_default();
        self.forget_sealed(all);
    }

    async fn on_connect_response(
        &self,
        from: String,
        accepted: bool,
        permission: Option<String>,
        reason: Option<String>,
        codec: Option<String>,
        os: Option<String>,
    ) {
        let perm = permission
            .as_deref()
            .map(|p| match p {
                "control" => Permission::Control,
                _ => Permission::View,
            })
            .unwrap_or(Permission::View);
        // Only the host we asked may answer, and only once. On acceptance we
        // become its viewer in the same step, under role → admitting →
        // outbound, unless this computer started hosting (or admitting a
        // viewer) while waiting: then a second session must not start (D1).
        // One critical section, so an incoming request cannot slip between the
        // check and the role change.
        let blocked_by_hosting = {
            let mut role = self.role.lock();
            let admitting = self.admitting.lock();
            let mut outbound = self.outbound.lock();
            if outbound.as_ref().map(|o| o.host.as_str()) != Some(from.as_str()) {
                drop((role, admitting, outbound));
                tracing::warn!("ignoring a connect-response from {from}: we did not ask it");
                return;
            }
            *outbound = None;
            let blocked = matches!(*role, Role::Host { .. }) || admitting.is_some();
            if accepted && !blocked {
                *role = Role::Viewer {
                    peer: from.clone(),
                    permission: perm,
                };
            }
            blocked
        };
        if accepted && blocked_by_hosting {
            // The attempt is cancelled: nothing from it is saved or carried over.
            *self.pending_login.lock() = None;
            *self.auto_attempted.lock() = None;
            // Tell that host to drop the session it just opened for us.
            let _ = self.signaling.send(SignalMsg::EndSession {
                to: Some(from),
                from: None,
            });
            let _ = self.ui.send(UiEvent::Toast(
                "Connection cancelled: someone connected to this computer meanwhile".into(),
            ));
            return;
        }
        if !accepted {
            *self.pending_login.lock() = None;
            let auto = self.auto_attempted.lock().take();
            let r = reason.unwrap_or_else(|| "denied".into());
            // A rejected password is reported as a plain refusal, not a second
            // challenge — so this is the ONLY place a stale saved credential can
            // be detected. Without dropping it here, auto-login would keep
            // replaying a password the host no longer accepts and the prompt
            // would never reappear: a permanent lockout from that host.
            if r == "bad-password" && auto.as_deref() == Some(from.as_str()) {
                self.forget_saved_login(&from);
                let _ = self.ui.send(UiEvent::Toast(
                    "Saved password was rejected — forgotten, please connect again".into(),
                ));
                return;
            }
            let _ = self.ui.send(UiEvent::Toast(format!("connection {r}")));
            return;
        }
        // The host accepted us, so any password we just sent is known-good — this
        // is the only point at which it's worth saving. Sealed by the platform's
        // secret store; if that fails we simply don't save (never plaintext).
        *self.auto_attempted.lock() = None;
        // Taken into a local first: the lock must not be held across the save
        // (`submit_password` on the UI thread takes it too).
        let pending = self.pending_login.lock().take();
        if let Some(p) = pending {
            if let (true, true, Some(workspace)) = (p.remember, p.host == from, p.workspace) {
                self.save_login(workspace, p.host, p.plaintext);
            }
        }
        // The host told us which codec it will stream; decode with the same (§3).
        *self.negotiated.lock() = codec
            .as_deref()
            .and_then(Codec::from_caps_str)
            .unwrap_or(Codec::H264);
        // And which OS it runs, for modifier translation; absent = unknown =
        // keys forwarded literally.
        let host_os = os
            .as_deref()
            .map_or(modifiers::OsFamily::Unknown, modifiers::OsFamily::from_name);
        tracing::info!("host OS: {host_os:?}");
        *self.peer_os.lock() = host_os;
        let _ = self.ui.send(UiEvent::RoleChanged(self.role.lock().clone()));
        tracing::info!("viewer session with {from}; awaiting the host's offer");
    }

    /// WebRTC negotiation payloads (offer/answer/ICE) — routed to the transport.
    async fn on_signal(&self, from: String, data: SignalData) {
        session::on_signal(self, &from, data);
    }

    // ---- Session lifecycle -----------------------------------------------

    /// Host action: switch the live permission (view↔control) mid-session.
    pub fn set_permission(&self, permission: Permission) {
        let mut role = self.role.lock();
        if let Role::Host { peer, .. } = &*role {
            let peer = peer.clone();
            *role = Role::Host { peer, permission };
            drop(role);
            // Update the host injection gate, then notify the viewer (§4.2).
            session::set_control(self, permission == Permission::Control);
            session::send_ctl(self, &ControlMsg::Perm { value: permission });
        }
    }

    /// Viewer action: ask the host for a fresh keyframe ("Refresh" button — e.g.
    /// to clear residual smear after packet loss).
    pub fn request_refresh(&self) {
        if matches!(&*self.role.lock(), Role::Viewer { .. }) {
            session::send_ctl(self, &ControlMsg::KeyframeRequest { s: 0 });
        }
    }

    /// Switch the shared monitor mid-session. On the host this takes effect
    /// directly; on the viewer it asks the host, which honours it only while
    /// this viewer has control. The new state comes back as
    /// [`UiEvent::Displays`]. Not saved: the host's "Monitor to share" setting
    /// stays the default for the next session.
    pub fn select_display(&self, id: String) {
        let role = self.role.lock().clone();
        match role {
            Role::Host { .. } => session::request_display(self, id),
            Role::Viewer { .. } => session::send_ctl(self, &ControlMsg::SelectDisplay { id }),
            Role::Idle => {}
        }
    }

    /// Viewer (D14): ask the host to ALSO stream monitor `id`, for a separate
    /// window. Needs control. When the host agrees, a [`UiEvent::Displays`]
    /// lists it under `popouts`; the app then opens the window and calls
    /// [`Engine::attach_popout`].
    pub fn popout_display(&self, id: String) {
        if matches!(&*self.role.lock(), Role::Viewer { .. }) {
            session::send_ctl(self, &ControlMsg::Popout { id });
        }
    }

    /// Viewer (D14): the app opened the window for pop-out stream `slot` and
    /// created `surface` in it (on the UI thread). Starts decoding and
    /// presenting that stream there, and sends its input. Returns the
    /// attachment's generation, which [`Engine::close_popout`] /
    /// [`Engine::detach_popout`] take; `None` when there is no viewer session
    /// or that slot is already attached (see [`Engine::popout_attached`]).
    pub fn attach_popout(&self, slot: u8, surface: Arc<dyn VideoSurface>) -> Option<u64> {
        session::open_viewer_popout(self, slot, surface)
    }

    /// Viewer (D14): whether pop-out stream `slot` has a window attached (an
    /// old window for a reused slot may still be closing).
    pub fn popout_attached(&self, slot: u8) -> bool {
        session::viewer_popout_attached(self, slot)
    }

    /// Viewer (D14): stop presenting pop-out stream `slot` and take its
    /// surface down, then tell the host to stop streaming it. Blocks until the
    /// window's threads are gone, so the app can destroy the window after it.
    /// Call it off the UI thread (a presenter may wait on that thread).
    /// `gen` is what [`Engine::attach_popout`] returned (`None` if the window
    /// never attached: only the host is told).
    pub fn close_popout(&self, slot: u8, gen: Option<u64>) {
        session::close_viewer_popout(self, slot, gen, true);
    }

    /// Viewer (D14): like [`Engine::close_popout`] but without telling the
    /// host — for a window the host itself stopped streaming (it is no longer
    /// listed), or one left over from an ended session.
    pub fn detach_popout(&self, slot: u8, gen: Option<u64>) {
        session::close_viewer_popout(self, slot, gen, false);
    }

    /// End the session locally and tell the peer (best-effort, contract §3.2 s5).
    /// Also cancels a connect-request that has not been answered yet, telling
    /// that host too, so it does not keep a session open for a viewer that left
    /// (08a §6.5 step 1).
    pub fn end_session(&self) {
        let pending = self.outbound.lock().take().map(|o| o.host);
        let peer = match &*self.role.lock() {
            Role::Host { peer, .. } | Role::Viewer { peer, .. } => Some(peer.clone()),
            Role::Idle => pending,
        };
        if let Some(peer) = peer {
            let _ = self.signaling.send(SignalMsg::EndSession {
                to: Some(peer),
                from: None,
            });
        }
        self.end_session_local();
    }

    fn on_remote_end(&self, from: String) {
        let is_peer = matches!(
            &*self.role.lock(),
            Role::Host { peer, .. } | Role::Viewer { peer, .. } if *peer == from
        );
        if is_peer {
            self.end_session_local();
        }
    }

    fn end_session_local(&self) {
        *self.outbound.lock() = None;
        session::teardown(self);
        *self.role.lock() = Role::Idle;
        let _ = self.ui.send(UiEvent::RoleChanged(Role::Idle));
    }

    /// Whether `id` is the peer of the current session or of our pending
    /// connect-request.
    fn is_session_peer(&self, id: &str) -> bool {
        let in_session = matches!(
            &*self.role.lock(),
            Role::Host { peer, .. } | Role::Viewer { peer, .. } if peer == id
        );
        in_session || self.outbound.lock().as_ref().is_some_and(|o| o.host == id)
    }

    /// End the current session because it could not be started or kept going,
    /// telling the user why.
    pub(crate) fn fail_session(&self, why: &str) {
        let _ = self
            .ui
            .send(UiEvent::Toast(format!("Session ended: {why}")));
        self.end_session();
    }

    // ---- platform & surface ------------------------------------------------

    /// The OS services this engine runs on.
    pub fn platform(&self) -> Arc<dyn Platform> {
        self.platform.clone()
    }

    /// Attach the viewer's video surface (the app creates it under its window).
    pub fn attach_surface(&self, surface: Arc<dyn VideoSurface>) {
        *self.surface.lock() = Some(surface);
    }

    pub(crate) fn surface(&self) -> Option<Arc<dyn VideoSurface>> {
        self.surface.lock().clone()
    }

    /// Viewer: hide/show the native video so web modals (settings) are visible
    /// mid-session. The stream keeps running; only presentation is hidden.
    pub fn set_video_visible(&self, visible: bool) {
        if let Some(s) = self.surface() {
            if visible {
                s.show();
                // D12: re-showing the surface does NOT repaint it — the present
                // loop only paints on a new decoded frame (or a cursor move,
                // D11), so a static host screen stays BLACK until the next
                // frame. Signal the present loop to repaint the held frame now.
                session::request_repaint(self);
            } else {
                s.hide();
            }
        }
    }

    pub fn set_ice_policy(&self, policy: IcePolicy) {
        *self.ice_policy.lock() = policy;
    }

    pub(crate) fn ice_policy(&self) -> IcePolicy {
        *self.ice_policy.lock()
    }

    pub(crate) fn negotiated_codec(&self) -> Codec {
        *self.negotiated.lock()
    }

    pub(crate) fn peer_os(&self) -> modifiers::OsFamily {
        *self.peer_os.lock()
    }

    pub(crate) fn translate_modifiers_handle(&self) -> Arc<AtomicBool> {
        self.translate_modifiers.clone()
    }

    // Accessors used by the session module.
    pub(crate) fn signaling(&self) -> &SignalingClient {
        &self.signaling
    }

    /// Clone of the UI event sender, so the transport thread can notify the UI
    /// directly when the connection dies (it doesn't hold `&Engine`).
    pub(crate) fn ui_sender(&self) -> tokio::sync::mpsc::UnboundedSender<UiEvent> {
        self.ui.clone()
    }

    /// Handle to the current role, so the transport thread can reset it to Idle
    /// on an unexpected disconnect.
    pub(crate) fn role_handle(&self) -> Arc<Mutex<Role>> {
        self.role.clone()
    }
}

/// Loopback test hooks (the `test-hooks` feature). They expose the host's live
/// rate state and let a test drive a synthetic bandwidth estimate, so the
/// end-to-end congestion behaviour can be asserted over real str0m without a
/// network shaper. Never compiled into a shipping build.
#[cfg(feature = "test-hooks")]
impl Engine {
    /// The host session's current encoder CBR target (bps), if a host session is
    /// live. This TRACKS the bandwidth estimate (clamped), it is not pinned.
    #[doc(hidden)]
    pub fn test_encoder_target_bps(&self) -> Option<u32> {
        self.session
            .lock()
            .as_ref()
            .and_then(|s| s.encoder_target_bps())
    }

    /// The host session's raw bandwidth estimate (bps).
    #[doc(hidden)]
    pub fn test_raw_link_bps(&self) -> Option<u32> {
        self.session.lock().as_ref().and_then(|s| s.raw_link_bps())
    }

    /// The host session's current encode size `(w, h)`.
    #[doc(hidden)]
    pub fn test_encode_dims(&self) -> Option<(u32, u32)> {
        self.session.lock().as_ref().and_then(|s| s.encode_dims())
    }

    /// Force the host's bandwidth estimate to `bps` (0 releases it back to the
    /// real str0m estimate). No effect on a viewer session.
    #[doc(hidden)]
    pub fn test_inject_bwe(&self, bps: u32) {
        if let Some(s) = self.session.lock().as_ref() {
            s.inject_bwe(bps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn an_unanswered_request_stops_counting_as_busy() {
        let fresh = Outbound::sent_to("host".into());
        assert!(fresh.is_busy());
        let lost = Outbound {
            host: "host".into(),
            busy_until: Instant::now().checked_sub(Duration::from_secs(1)),
        };
        assert!(lost.busy_until.is_some());
        assert!(!lost.is_busy());
        // Waiting on the user's password has no limit.
        let prompting = Outbound {
            host: "host".into(),
            busy_until: None,
        };
        assert!(prompting.is_busy());
    }
}
