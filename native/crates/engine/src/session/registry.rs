//! The sessions of this computer (plans/08 P2, 08a §2.2, §5.2): at most one
//! host session, any number of viewer sessions — never both at once (D1) —
//! and the per-session state that used to be global (the pending request,
//! the login being tried, the negotiated codec, the host's OS, the surface).
//!
//! Everything here is guarded by one lock (`Engine::reg`), so admission,
//! connecting and accepting are each one critical section: an incoming
//! request and an outgoing connect can never both get through (I3).
//!
//! **Lock order:** `ws` → `reg`. Nothing else is taken while `reg` is held,
//! no event is sent and no thread is joined under it.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use platform_api::{Codec, VideoSurface};
use protocol::Permission;

use super::Session;
use crate::events::SessionId;
use crate::modifiers::OsFamily;

/// How long an unanswered connect-request keeps this computer busy for
/// incoming ones: the viewer's own 30 s answer timeout (contract §3.3) plus
/// room for the answer in flight. Past it the request is treated as abandoned
/// for the busy check, even if the UI never cancelled it, so a lost answer can
/// never leave this computer refusing everyone.
const REQUEST_BUSY_FOR: Duration = Duration::from_secs(35);

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

/// Where a viewer session is before it is live.
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
            busy_until: Some(Instant::now() + REQUEST_BUSY_FOR),
            pending_login: None,
            auto_attempted: false,
            permission: Permission::View,
            codec: Codec::H264,
            host_os: OsFamily::Unknown,
            surface,
            workspace: None,
            ended: Arc::new(AtomicBool::new(false)),
            session: None,
        }
    }

    /// The request went out again (a password answer): the timeout restarts.
    pub(crate) fn resent(&mut self) {
        self.phase = ViewerPhase::Requesting;
        self.busy_until = Some(Instant::now() + REQUEST_BUSY_FOR);
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
    pub(crate) ended: Arc<AtomicBool>,
    pub(crate) session: Option<Session>,
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
}

impl Registry {
    /// A new id, never handed out before in this run (08a §6.2 step 2).
    pub(crate) fn reserve(&mut self) -> SessionId {
        self.next += 1;
        SessionId(self.next)
    }

    /// The viewer session with `peer`, if any (at most one, I1).
    pub(crate) fn viewer_by_peer(&self, peer: &str) -> Option<SessionId> {
        self.viewers
            .iter()
            .find(|(_, v)| v.peer == peer)
            .map(|(id, _)| *id)
    }

    /// Whether an incoming connect-request from `from` must be answered "busy"
    /// (D1): hosting, admitting someone else, or any viewer session.
    pub(crate) fn busy_for(&self, from: &str) -> bool {
        self.host.is_some()
            || self.admitting.as_deref().is_some_and(|p| p != from)
            || self.viewers.values().any(ViewerSlot::counts_as_busy)
    }

    /// Whether this computer is hosting or letting a viewer in (D1).
    pub(crate) fn hosting(&self) -> bool {
        self.host.is_some() || self.admitting.is_some()
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
        r.admitting = None;
        let id = r.reserve();
        r.viewers
            .insert(id, ViewerSlot::requesting("h".into(), None));
        assert!(r.busy_for("a"));
        assert_eq!(r.viewer_by_peer("h"), Some(id));
    }
}
