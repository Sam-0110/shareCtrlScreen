//! When the service (re)launches the host: the decision half of the session
//! loop, kept free of Win32 calls so it is unit-tested on every OS.
//!
//! The service keeps one host running in the active console session (§8b). It
//! launches one when a session becomes active, and launches a new one when the
//! host it started dies: a crash, or being killed. It does NOT relaunch a host
//! that exited cleanly (exit code 0). Code 0 is what every deliberate exit
//! returns:
//!
//! * the user picked Quit (the tray menu),
//! * the updater handed over to the installer (`tauri-plugin-updater` exits the
//!   process with 0 on Windows; the installer then restarts the service, which
//!   launches the new host — relaunching the OLD exe here would race it),
//! * a duplicate launch stepped aside for a host already running in the session
//!   (`platform_windows::instance`).
//!
//! Closing the window does not exit the host at all: it hides to the tray.
//! A cleanly-exited host stays down until the session changes (sign-out and
//! back in, a user switch), the service restarts, or the user starts the app.
//!
//! A host that keeps dying is relaunched with a doubling delay, so a host that
//! crashes at startup cannot spin the CPU. The delay resets once a host has
//! stayed up for [`STABLE_AFTER`].

use std::time::{Duration, Instant};

/// The first relaunch delay after a failure; doubles with each failure in a row.
pub const FIRST_RETRY: Duration = Duration::from_secs(2);
/// The longest delay between relaunch attempts.
pub const MAX_RETRY: Duration = Duration::from_secs(300);
/// A host that ran this long before dying counts as a fresh failure, not one
/// more in a row.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    /// No host yet in this session: launch one.
    Wanted,
    /// The host this service started is running.
    Running { since: Instant },
    /// The host exited cleanly: leave it down until the session changes.
    Stopped,
    /// The host died, or launching it failed: try again at `at`.
    Retry { at: Instant },
}

/// How a host the service watched ended, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exit code 0: a deliberate exit. The host stays down.
    Clean,
    /// Any other code: the host is relaunched after `retry_in`.
    Abnormal { code: u32, retry_in: Duration },
}

/// The relaunch policy for the active console session.
#[derive(Debug)]
pub struct Supervisor {
    session: Option<u32>,
    host: Host,
    /// Failures in a row (host deaths and failed launches) since the last
    /// stable run; sets the next delay.
    failures: u32,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            session: None,
            host: Host::Wanted,
            failures: 0,
        }
    }

    /// Called every poll with the active console session (`None`: nobody is
    /// signed in at the console). Returns the session to launch a host into
    /// now, if any. A session change starts over: the new session wants a
    /// host, and the old session's host is no longer this service's concern.
    pub fn poll(&mut self, active: Option<u32>, now: Instant) -> Option<u32> {
        if active != self.session {
            self.session = active;
            self.host = Host::Wanted;
            self.failures = 0;
        }
        let session = self.session?;
        match self.host {
            Host::Wanted => Some(session),
            Host::Retry { at } if now >= at => Some(session),
            Host::Running { .. } | Host::Stopped | Host::Retry { .. } => None,
        }
    }

    /// Whether the service is watching a running host (and so should check it
    /// for exit).
    pub fn watching(&self) -> bool {
        matches!(self.host, Host::Running { .. })
    }

    /// The launch [`poll`](Self::poll) asked for succeeded.
    pub fn launched(&mut self, now: Instant) {
        self.host = Host::Running { since: now };
    }

    /// The launch [`poll`](Self::poll) asked for failed. Returns the delay
    /// before the next attempt.
    pub fn launch_failed(&mut self, now: Instant) -> Duration {
        self.schedule_retry(now)
    }

    /// The watched host exited with `code`.
    pub fn exited(&mut self, code: u32, now: Instant) -> Exit {
        if code == 0 {
            self.host = Host::Stopped;
            self.failures = 0;
            return Exit::Clean;
        }
        if let Host::Running { since } = self.host {
            if now.saturating_duration_since(since) >= STABLE_AFTER {
                self.failures = 0;
            }
        }
        Exit::Abnormal {
            code,
            retry_in: self.schedule_retry(now),
        }
    }

    fn schedule_retry(&mut self, now: Instant) -> Duration {
        let delay = retry_delay(self.failures);
        self.failures = self.failures.saturating_add(1);
        self.host = Host::Retry { at: now + delay };
        delay
    }
}

/// [`FIRST_RETRY`] doubled once per earlier failure in a row, capped at
/// [`MAX_RETRY`].
fn retry_delay(earlier_failures: u32) -> Duration {
    FIRST_RETRY
        .checked_mul(1u32.checked_shl(earlier_failures).unwrap_or(u32::MAX))
        .map_or(MAX_RETRY, |d| d.min(MAX_RETRY))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: Option<u32> = Some(1);
    const S2: Option<u32> = Some(2);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A supervisor whose host is running in session 1, started at `t0`.
    fn running(t0: Instant) -> Supervisor {
        let mut s = Supervisor::new();
        assert_eq!(s.poll(S1, t0), S1);
        s.launched(t0);
        s
    }

    #[test]
    fn nobody_signed_in_launches_nothing() {
        let mut s = Supervisor::new();
        assert_eq!(s.poll(None, Instant::now()), None);
    }

    #[test]
    fn a_new_session_gets_one_host() {
        let t0 = Instant::now();
        let mut s = running(t0);
        assert!(s.watching());
        // A running host is never launched again.
        assert_eq!(s.poll(S1, t0 + secs(2)), None);
        assert_eq!(s.poll(S1, t0 + secs(3600)), None);
    }

    #[test]
    fn a_clean_exit_stays_down_until_the_session_changes() {
        let t0 = Instant::now();
        let mut s = running(t0);
        assert_eq!(s.exited(0, t0 + secs(10)), Exit::Clean);
        assert!(!s.watching());
        assert_eq!(s.poll(S1, t0 + secs(12)), None);
        assert_eq!(s.poll(S1, t0 + secs(100_000)), None);
        // Signing out and back in brings it back.
        assert_eq!(s.poll(None, t0 + secs(100_001)), None);
        assert_eq!(s.poll(S1, t0 + secs(100_002)), S1);
    }

    #[test]
    fn a_crash_is_relaunched_after_the_first_delay() {
        let t0 = Instant::now();
        let mut s = running(t0);
        let died = t0 + secs(600);
        assert_eq!(
            s.exited(0xC000_0409, died),
            Exit::Abnormal {
                code: 0xC000_0409,
                retry_in: FIRST_RETRY
            }
        );
        assert_eq!(s.poll(S1, died + FIRST_RETRY - secs(1)), None);
        assert_eq!(s.poll(S1, died + FIRST_RETRY), S1);
    }

    #[test]
    fn repeated_quick_crashes_back_off_up_to_the_cap() {
        let mut now = Instant::now();
        let mut s = running(now);
        let mut delays = Vec::new();
        for _ in 0..12 {
            now += secs(1); // dies one second after each start
            let Exit::Abnormal { retry_in, .. } = s.exited(1, now) else {
                panic!("a non-zero exit is abnormal");
            };
            delays.push(retry_in.as_secs());
            now += retry_in;
            assert_eq!(s.poll(S1, now), S1);
            s.launched(now);
        }
        assert_eq!(
            delays,
            [2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300, 300],
            "doubling from FIRST_RETRY, capped at MAX_RETRY"
        );
    }

    #[test]
    fn a_host_that_ran_a_while_resets_the_backoff() {
        let mut now = Instant::now();
        let mut s = running(now);
        for _ in 0..5 {
            now += secs(1);
            let Exit::Abnormal { retry_in, .. } = s.exited(1, now) else {
                panic!("abnormal");
            };
            now += retry_in;
            s.poll(S1, now);
            s.launched(now);
        }
        now += STABLE_AFTER;
        assert_eq!(
            s.exited(1, now),
            Exit::Abnormal {
                code: 1,
                retry_in: FIRST_RETRY
            }
        );
    }

    #[test]
    fn failed_launches_back_off_too() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        assert_eq!(s.poll(S1, t0), S1);
        assert_eq!(s.launch_failed(t0), secs(2));
        assert_eq!(s.poll(S1, t0 + secs(1)), None);
        assert_eq!(s.poll(S1, t0 + secs(2)), S1);
        assert_eq!(s.launch_failed(t0 + secs(2)), secs(4));
    }

    #[test]
    fn a_session_change_launches_at_once_and_forgets_the_backoff() {
        let mut now = Instant::now();
        let mut s = running(now);
        for _ in 0..4 {
            now += secs(1);
            s.exited(1, now);
        }
        // Mid-backoff, the user switches: the new session gets a host now.
        assert_eq!(s.poll(S2, now), S2);
        s.launched(now);
        assert_eq!(
            s.exited(1, now + secs(1)),
            Exit::Abnormal {
                code: 1,
                retry_in: FIRST_RETRY
            }
        );
    }

    #[test]
    fn the_delay_never_overflows() {
        assert_eq!(retry_delay(0), FIRST_RETRY);
        assert_eq!(retry_delay(31), MAX_RETRY);
        assert_eq!(retry_delay(32), MAX_RETRY);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY);
    }
}
