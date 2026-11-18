//! The sessions of this computer (plans/08 P2, 08a §2.2, §5.2): at most one
//! host session, any number of viewer sessions — never both at once (D1) —
//! the per-session state that used to be global (the pending request, the
//! login being tried, the negotiated codec, the host's OS, the surface), and
//! the sessions that ended but whose threads are still being joined.
//!
//! Everything here is guarded by one lock (`Engine::reg`), so admission,
//! connecting and accepting are each one critical section: an incoming
//! request and an outgoing connect can never both get through (I3).
//!
//! **A session counts until it is released.** Ending one moves it to
//! `reaping` until its threads are joined; it still makes this computer busy
//! (D1, 08a §6.15), still holds its peer (I1) and its surface, so nothing new
//! starts on top of threads that are still winding down.
//!
//! **Lock order:** `ws` → `reg`. Nothing else is taken while `reg` is held
//! except a session's own inner locks; no event is sent and no thread is
//! joined under it.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use platform_api::{Codec, VideoSurface};
use protocol::Permission;

use super::Session;
use crate::events::{EndReason, SessionId};
use crate::modifiers::OsFamily;

/// How long an unanswered connect-request keeps this computer busy for
/// incoming ones, and how long the engine waits for the host's answer before
/// ending the session itself: the viewer's own 30 s answer timeout (contract
/// §3.3) plus room for the answer in flight.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

/// A password the viewer just submitted, held until the host accepts it: only
/// then is it worth saving, and a rejected guess is never persisted.
pub(crate) struct PendingLogin {
    pub(crate) plaintext: String,
    /// "Remember" was ticked (the default, D7).
    pub(crate) remember: bool,
    /// The workspace that was active when it was typed, where it is saved;
    /// `None` if none was reachable.
    pub(crate) workspace: Option<String>,
}

/// Where a viewer session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewerPhase {
    /// The connect-request is out.
    Requesting,
    /// The user is typing the host's password.
    PasswordRequired,
    /// Accepted; waiting for (or handling) the host's offer.
    Negotiating,
    /// The session's threads run.
    Live,
}

/// One viewer session.
pub(crate) struct ViewerSlot {
    pub(crate) peer: String,
    pub(crate) phase: ViewerPhase,
    /// While requesting: until when this counts as "connecting" for the
    /// one-session rule (D1). `None` while the password prompt is up: that
    /// wait has no limit, and the user ends it with Cancel.
    pub(crate) busy_until: Option<Instant>,
    /// Bumped by every request sent, so a timer started for an earlier one
    /// does nothing.
    pub(crate) attempt: u64,
    pub(crate) pending_login: Option<PendingLogin>,
    /// A saved password was just submitted automatically; a second challenge
    /// means it no longer works.
    pub(crate) auto_attempted: bool,
    /// The permission the host granted.
    pub(crate) permission: Permission,
    /// The codec the host chose (from its connect-response).
    pub(crate) codec: Codec,
    /// The host's OS, for modifier translation (D8).
    pub(crate) host_os: OsFamily,
    /// Where its video is presented.
    pub(crate) surface: Option<Arc<dyn VideoSurface>>,
    /// The workspace the connection was started in and its epoch, so the
    /// recent lands there and nowhere else (I13).
    pub(crate) workspace: Option<(String, u64)>,
    /// Its session is being built (threads, sinks) by `start_viewer`. An end
    /// asked for meanwhile is recorded in `end_requested` and carried out by
    /// the starter once everything it builds exists, so nothing it builds can
    /// outlive the end.
    pub(crate) starting: bool,
    pub(crate) end_requested: Option<EndReason>,
    /// Ended by whichever side notices first (a death on the transport
    /// thread, a user disconnect): set once.
    pub(crate) ended: Arc<AtomicBool>,
    pub(crate) session: Option<Session>,
}

impl ViewerSlot {
    pub(crate) fn requesting(peer: String, surface: Option<Arc<dyn VideoSurface>>) -> Self {
        ViewerSlot {
            peer,
            phase: ViewerPhase::Requesting,
            busy_until: Some(Instant::now() + REQUEST_TIMEOUT),
            attempt: 1,
            pending_login: None,
            auto_attempted: false,
            permission: Permission::View,
            codec: Codec::H264,
            host_os: OsFamily::Unknown,
            surface,
            workspace: None,
            starting: false,
            end_requested: None,
            ended: Arc::new(AtomicBool::new(false)),
            session: None,
        }
    }

    /// The request went out again (a password answer): the timeout restarts.
    pub(crate) fn resent(&mut self) {
        self.phase = ViewerPhase::Requesting;
        self.busy_until = Some(Instant::now() + REQUEST_TIMEOUT);
        self.attempt += 1;
    }

    /// Whether it makes this computer busy for incoming requests (D1): any
    /// viewer session does, except a request whose answer is overdue.
    pub(crate) fn counts_as_busy(&self) -> bool {
        match self.phase {
            ViewerPhase::Requesting => self.busy_until.is_none_or(|t| Instant::now() < t),
            _ => true,
        }
    }
}

/// The host session.
pub(crate) struct HostSlot {
    pub(crate) id: SessionId,
    pub(crate) peer: String,
    pub(crate) permission: Permission,
    /// Being built by `begin_host` (see [`ViewerSlot::starting`]).
    pub(crate) starting: bool,
    pub(crate) end_requested: Option<EndReason>,
    pub(crate) ended: Arc<AtomicBool>,
    pub(crate) session: Option<Session>,
}

/// A session that ended and whose threads are being joined.
pub(crate) struct Reaping {
    pub(crate) peer: String,
    pub(crate) host: bool,
    pub(crate) surface: Option<Arc<dyn VideoSurface>>,
}

/// All sessions, under `Engine::reg`.
#[derive(Default)]
pub(crate) struct Registry {
    next: u64,
    pub(crate) host: Option<HostSlot>,
    /// Host: the one peer currently being admitted — its approval prompt is
    /// open or its password proof is being checked. While it is set, every
    /// other connect-request is answered "busy" and no outgoing session may
    /// start (D1).
    pub(crate) admitting: Option<String>,
    pub(crate) viewers: BTreeMap<SessionId, ViewerSlot>,
    /// The viewer session on screen (plans/08 P3): the only one whose video is
    /// shown and which exchanges the clipboard and holds the shortcut hook.
    pub(crate) active: Option<SessionId>,
    /// Ended sessions not released yet.
    pub(crate) reaping: BTreeMap<SessionId, Reaping>,
}

impl Registry {
    /// A new id, never handed out before in this run (08a §6.2 step 2).
    pub(crate) fn reserve(&mut self) -> SessionId {
        self.next += 1;
        SessionId(self.next)
    }

    /// The live (not ended) viewer session with `peer`, if any. Messages from
    /// that host are routed to it.
    pub(crate) fn viewer_by_peer(&self, peer: &str) -> Option<SessionId> {
        self.viewers
            .iter()
            .find(|(_, v)| v.peer == peer)
            .map(|(id, _)| *id)
    }

    /// A viewer session with `peer` that has not been released, live or
    /// ending (one per remote, I1).
    pub(crate) fn peer_in_use(&self, peer: &str) -> Option<SessionId> {
        self.viewer_by_peer(peer).or_else(|| {
            self.reaping
                .iter()
                .find(|(_, r)| !r.host && r.peer == peer)
                .map(|(id, _)| *id)
        })
    }

    /// A session that has not been released and presents on `surface`.
    /// `true` with it when that session is already ending.
    pub(crate) fn surface_in_use(
        &self,
        surface: &Arc<dyn VideoSurface>,
    ) -> Option<(SessionId, bool)> {
        let same =
            |s: &Option<Arc<dyn VideoSurface>>| s.as_ref().is_some_and(|s| Arc::ptr_eq(s, surface));
        self.viewers
            .iter()
            .find(|(_, v)| same(&v.surface))
            .map(|(id, _)| (*id, false))
            .or_else(|| {
                self.reaping
                    .iter()
                    .find(|(_, r)| same(&r.surface))
                    .map(|(id, _)| (*id, true))
            })
    }

    /// Whether any viewer session would keep this computer from hosting (D1):
    /// a live one (unless its request is overdue), or one still ending.
    pub(crate) fn viewing(&self) -> bool {
        self.viewers.values().any(ViewerSlot::counts_as_busy)
            || self.reaping.values().any(|r| !r.host)
    }

    /// Whether an incoming connect-request from `from` must be answered "busy"
    /// (D1): hosting (or a host session still ending), admitting someone
    /// else, or viewing.
    pub(crate) fn busy_for(&self, from: &str) -> bool {
        self.hosting_excluding(Some(from)) || self.viewing()
    }

    /// Whether this computer is hosting, letting a viewer in, or still ending
    /// a host session (D1): no viewer session may start.
    pub(crate) fn hosting(&self) -> bool {
        self.hosting_excluding(None)
    }

    fn hosting_excluding(&self, admitting_peer: Option<&str>) -> bool {
        self.host.is_some()
            || self.reaping.values().any(|r| r.host)
            || self
                .admitting
                .as_deref()
                .is_some_and(|p| admitting_peer != Some(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_never_reused() {
        let mut r = Registry::default();
        let a = r.reserve();
        let b = r.reserve();
        assert_ne!(a, b);
        assert!(b > a);
    }

    #[test]
    fn an_overdue_request_stops_counting_as_busy_but_a_prompt_does_not() {
        let mut v = ViewerSlot::requesting("h".into(), None);
        assert!(v.counts_as_busy());
        v.busy_until = Instant::now().checked_sub(Duration::from_secs(1));
        assert!(!v.counts_as_busy());
        v.phase = ViewerPhase::PasswordRequired;
        v.busy_until = None;
        assert!(v.counts_as_busy());
        v.phase = ViewerPhase::Live;
        assert!(v.counts_as_busy());
    }

    #[test]
    fn busy_follows_d1() {
        let mut r = Registry::default();
        assert!(!r.busy_for("a"));
        r.admitting = Some("a".into());
        assert!(!r.busy_for("a"), "the peer being admitted may retry");
        assert!(r.busy_for("b"));
        assert!(r.hosting());
        r.admitting = None;
        let id = r.reserve();
        r.viewers
            .insert(id, ViewerSlot::requesting("h".into(), None));
        assert!(r.busy_for("a"));
        assert_eq!(r.viewer_by_peer("h"), Some(id));
    }

    #[test]
    fn an_ending_session_still_counts_until_it_is_released() {
        let mut r = Registry::default();
        let id = r.reserve();
        r.reaping.insert(
            id,
            Reaping {
                peer: "h".into(),
                host: false,
                surface: None,
            },
        );
        assert!(r.busy_for("a"), "still viewing (D1)");
        assert_eq!(r.peer_in_use("h"), Some(id), "still holds its peer (I1)");
        assert_eq!(r.viewer_by_peer("h"), None, "but nothing is routed to it");
        r.reaping.clear();
        let host = r.reserve();
        r.reaping.insert(
            host,
            Reaping {
                peer: "v".into(),
                host: true,
                surface: None,
            },
        );
        assert!(r.hosting(), "a host session still ending blocks viewing");
    }
}
