//! Stamps for the synced document: monotonic, corrected for this computer's
//! clock skew against the server, and never trusted beyond a small slack into
//! the future (plans/08a §3, §6.12 step 3, invariant I11).
//!
//! Every synced change is last-writer-wins by its stamp. Two things would make
//! that unfair without this module: a computer whose clock runs fast would win
//! every future edit (and keep a tab it closed reopened forever), and a clock
//! that steps back would let an older edit beat a newer one on the same
//! computer. So a stamp is `max(last + 1, wall + skew)`, the skew comes from
//! the server's time in each sync reply, and every stamp loaded from disk or
//! merged from elsewhere is clamped to `wall + skew + FUTURE_SLACK_MS`. The
//! server clamps to its own time plus the same slack (`server/src/sync.js`).

use super::now_ms;

/// How far into the future a stamp may be before it is clamped (5 minutes):
/// room for network latency and an imperfect skew estimate, too little for a
/// wrong clock to matter.
pub const FUTURE_SLACK_MS: u64 = 5 * 60 * 1000;

/// Hands out stamps for this computer's synced changes.
#[derive(Debug, Clone, Default)]
pub struct Stamper {
    /// Server time minus wall time, from the last sync reply (0 before one).
    skew_ms: i64,
    /// The last stamp handed out or observed; stamps never go below it.
    last: u64,
}

impl Stamper {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh stamp: later than every earlier one, and close to server time.
    pub fn now(&mut self) -> u64 {
        self.now_at(now_ms())
    }

    /// Record the server's clock from a sync reply (`serverTime`, ms).
    pub fn set_skew(&mut self, server_time: u64) {
        self.set_skew_at(server_time, now_ms());
    }

    /// The current time on the server's clock, as far as this computer knows.
    pub fn server_now(&self) -> u64 {
        self.corrected(now_ms())
    }

    /// The latest stamp accepted from disk or another computer right now.
    pub fn max_stamp(&self) -> u64 {
        self.max_stamp_at(now_ms())
    }

    /// `t`, or the latest acceptable stamp if `t` is later than that.
    pub fn clamp(&self, t: u64) -> u64 {
        t.min(self.max_stamp())
    }

    /// Note a stamp that already exists (loaded or merged), so new stamps are
    /// later than it. Clamped first, so a bad stamp cannot push `now` ahead.
    pub fn observe(&mut self, t: u64) {
        self.last = self.last.max(self.clamp(t));
    }

    fn corrected(&self, wall: u64) -> u64 {
        wall.saturating_add_signed(self.skew_ms)
    }

    fn now_at(&mut self, wall: u64) -> u64 {
        let t = self.corrected(wall).max(self.last.saturating_add(1));
        self.last = t;
        t
    }

    fn set_skew_at(&mut self, server_time: u64, wall: u64) {
        if server_time == 0 {
            return; // an old server sends no time: keep what we had
        }
        self.skew_ms =
            (server_time as i128 - wall as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    }

    fn max_stamp_at(&self, wall: u64) -> u64 {
        self.corrected(wall).saturating_add(FUTURE_SLACK_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_never_go_backwards_even_when_the_clock_does() {
        let mut s = Stamper::new();
        let a = s.now_at(1_000);
        let b = s.now_at(500); // the wall clock stepped back
        let c = s.now_at(500);
        assert!(a < b && b < c, "{a} {b} {c}");
    }

    #[test]
    fn skew_moves_stamps_onto_server_time() {
        let mut s = Stamper::new();
        s.set_skew_at(10_000, 4_000); // this computer is 6 s behind
        assert_eq!(s.now_at(4_000), 10_000);
        s.set_skew_at(1_000, 4_000); // now 3 s ahead
                                     // Still monotonic: the earlier stamp is not undercut.
        assert_eq!(s.now_at(4_000), 10_001);
        assert_eq!(s.now_at(20_000), 17_000);
    }

    #[test]
    fn an_old_server_without_a_time_keeps_the_skew() {
        let mut s = Stamper::new();
        s.set_skew_at(10_000, 4_000);
        s.set_skew_at(0, 4_000);
        assert_eq!(s.max_stamp_at(4_000), 10_000 + FUTURE_SLACK_MS);
    }

    #[test]
    fn far_future_stamps_are_clamped_and_cannot_drag_now_along() {
        let mut s = Stamper::new();
        let wall = now_ms();
        let far = wall + 365 * 24 * 3600 * 1000;
        assert!(s.clamp(far) <= s.max_stamp());
        s.observe(far);
        let next = s.now();
        assert!(next <= wall + FUTURE_SLACK_MS + 60_000, "{next}");
        // A stamp within the slack is kept as is.
        assert_eq!(s.clamp(wall + 1_000), wall + 1_000);
    }
}
