//! Keeping core running on Windows, where nothing like launchd or systemd does it: the
//! login launcher (`src/bin/nekorisu-start.rs`, started by a scheduled task) runs core and
//! starts it again when it exits with an error, as launchd's `KeepAlive` and systemd's
//! `Restart=on-failure` do. Exiting with 0 means it was asked to stop, so it stays stopped.
//!
//! Shared by core and the launcher (which includes this file by path), so std only.

use std::time::Duration;

/// Core's exit code asking for a fresh start right away: after switching or rolling back
/// versions, the link names a different binary. The launcher and core must agree on it, so it
/// never changes.
pub const RESTART: i32 = 75;

/// A run shorter than this counts toward giving up; a longer one clears the count.
const SHORT_RUN: Duration = Duration::from_secs(60);
/// Short runs in a row after which the launcher stops trying. Larger than the start attempts a
/// freshly installed version gets before it is rolled back, so a rollback always gets its turn.
const GIVE_UP_AFTER: u32 = 10;
/// Waits between attempts double from one second up to this.
const LONGEST_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Asked to stop: don't start it again.
    Stop,
    /// Start it again after this long.
    Again(Duration),
    /// It keeps failing right away: leave it stopped.
    GiveUp,
}

#[derive(Default)]
pub struct Policy {
    short_runs: u32,
}

impl Policy {
    /// What to do once core has exited with `code` (none: no code to report) after running
    /// for `ran`.
    pub fn after(&mut self, code: Option<i32>, ran: Duration) -> Next {
        if code == Some(0) {
            return Next::Stop;
        }
        if ran >= SHORT_RUN {
            self.short_runs = 0;
        }
        self.short_runs += 1;
        if self.short_runs > GIVE_UP_AFTER {
            return Next::GiveUp;
        }
        if code == Some(RESTART) {
            return Next::Again(Duration::ZERO);
        }
        let wait = Duration::from_secs(1 << (self.short_runs - 1).min(6));
        Next::Again(wait.min(LONGEST_WAIT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: Duration = Duration::from_secs(1);

    #[test]
    fn a_clean_exit_stays_stopped() {
        assert_eq!(Policy::default().after(Some(0), QUICK), Next::Stop);
    }

    #[test]
    fn a_restart_request_starts_again_at_once() {
        let mut policy = Policy::default();
        assert_eq!(policy.after(Some(RESTART), QUICK), Next::Again(Duration::ZERO));
        assert_eq!(policy.after(Some(RESTART), QUICK), Next::Again(Duration::ZERO));
    }

    #[test]
    fn failures_wait_longer_each_time_then_give_up() {
        let mut policy = Policy::default();
        let waits: Vec<_> = (0..GIVE_UP_AFTER)
            .map(|_| match policy.after(Some(1), QUICK) {
                Next::Again(wait) => wait.as_secs(),
                other => panic!("expected another try, got {other:?}"),
            })
            .collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(policy.after(Some(1), QUICK), Next::GiveUp);
    }

    #[test]
    fn a_long_run_clears_the_count() {
        let mut policy = Policy::default();
        for _ in 0..GIVE_UP_AFTER {
            policy.after(Some(1), QUICK);
        }
        assert_eq!(policy.after(Some(1), SHORT_RUN), Next::Again(Duration::from_secs(1)));
    }

    #[test]
    fn a_rollback_gets_its_turn_before_giving_up() {
        // A broken new version fails its allowed starts, then the next start rolls back and asks
        // for a restart; that must still be granted.
        let mut policy = Policy::default();
        for _ in 0..3 {
            assert!(matches!(policy.after(Some(1), QUICK), Next::Again(_)));
        }
        assert_eq!(policy.after(Some(RESTART), QUICK), Next::Again(Duration::ZERO));
    }
}
