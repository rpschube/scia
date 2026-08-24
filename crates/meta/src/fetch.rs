//! The OS-neutral artwork-fetch scheduler: a pure, time-injected state machine
//! that decides *when* a backend should attempt to fetch a track's artwork.
//!
//! Players are unreliable about art: they often publish the art URL a beat
//! after the track event, and some swap the URL mid-track. So the scheduler
//! debounces a request (wait a short settling window before the first attempt,
//! and restart that window if the request is superseded) and, on a failed
//! fetch, retries a couple of times with exponential backoff before giving up —
//! a track with no reachable art is a normal outcome, not an error.
//!
//! It holds no clock and does no I/O: the caller passes the current [`Instant`]
//! in and performs the actual fetch off the event thread. That keeps the policy
//! unit-testable with synthetic time and keeps a slow network fetch from ever
//! blocking the thread that receives player events.

use std::time::{Duration, Instant};

use crate::ArtworkRef;

/// Default settling window before the first fetch attempt.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(250);
/// Default number of fetch attempts before a track's art is abandoned.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default backoff base; attempt *n* waits `base * 2^(n-1)`.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// The single artwork request the scheduler is tracking. Only the most recent
/// request per backend matters: when the winning track changes, the old track's
/// art is irrelevant, so a new request supersedes the pending one.
#[derive(Clone, Debug)]
struct Pending {
    track_key: String,
    art: ArtworkRef,
    /// The earliest instant the next attempt may run.
    due_at: Instant,
    /// How many attempts have already failed.
    attempts: u32,
}

/// Debounce + retry/backoff policy for artwork fetches. See the module docs.
#[derive(Clone, Debug)]
pub struct FetchScheduler {
    debounce: Duration,
    max_attempts: u32,
    backoff_base: Duration,
    pending: Option<Pending>,
}

impl Default for FetchScheduler {
    fn default() -> Self {
        Self::new(DEFAULT_DEBOUNCE, DEFAULT_MAX_ATTEMPTS, DEFAULT_BACKOFF_BASE)
    }
}

impl FetchScheduler {
    /// Build a scheduler with explicit timings.
    pub fn new(debounce: Duration, max_attempts: u32, backoff_base: Duration) -> Self {
        Self {
            debounce,
            max_attempts: max_attempts.max(1),
            backoff_base,
            pending: None,
        }
    }

    /// Register (or supersede) the artwork request for the current track. The
    /// first attempt is scheduled one debounce window from `now`; any pending
    /// request for an earlier track is dropped.
    pub fn request(&mut self, now: Instant, track_key: String, art: ArtworkRef) {
        self.pending = Some(Pending {
            track_key,
            art,
            due_at: now + self.debounce,
            attempts: 0,
        });
    }

    /// If a request is pending and its scheduled time has arrived, return the
    /// job to fetch. The request stays pending until the caller reports the
    /// outcome via [`FetchScheduler::on_success`] or
    /// [`FetchScheduler::on_failure`], so a single-threaded caller that fetches
    /// synchronously will not be handed the same job twice.
    pub fn due(&mut self, now: Instant) -> Option<(String, ArtworkRef)> {
        match &self.pending {
            Some(p) if now >= p.due_at => Some((p.track_key.clone(), p.art.clone())),
            _ => None,
        }
    }

    /// Report a successful fetch for `track_key`; clears the pending request if
    /// it still refers to that track (a newer request supersedes it untouched).
    pub fn on_success(&mut self, track_key: &str) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.track_key == track_key)
        {
            self.pending = None;
        }
    }

    /// Report a failed fetch for `track_key`. Schedules the next attempt with
    /// exponential backoff, or abandons the request once the attempt budget is
    /// spent. A stale key (a newer request has already superseded it) is
    /// ignored.
    pub fn on_failure(&mut self, now: Instant, track_key: &str) {
        if let Some(p) = self.pending.as_mut() {
            if p.track_key != track_key {
                return;
            }
            p.attempts += 1;
            if p.attempts >= self.max_attempts {
                self.pending = None;
            } else {
                let shift = (p.attempts - 1).min(16);
                p.due_at = now + self.backoff_base * (1u32 << shift);
            }
        }
    }

    /// The instant the pending request is next due, if any — a caller uses it
    /// to size a wait so it wakes exactly when the next attempt is ready.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.due_at)
    }

    /// Whether any request is currently pending.
    pub fn is_idle(&self) -> bool {
        self.pending.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art() -> ArtworkRef {
        ArtworkRef::Url("https://example/art".into())
    }

    #[test]
    fn debounces_before_first_attempt() {
        let mut s = FetchScheduler::new(Duration::from_millis(250), 3, Duration::from_millis(100));
        let t0 = Instant::now();
        s.request(t0, "k".into(), art());
        assert!(s.due(t0).is_none(), "not due before the debounce window");
        assert!(s.due(t0 + Duration::from_millis(249)).is_none());
        assert!(s.due(t0 + Duration::from_millis(250)).is_some());
    }

    #[test]
    fn a_new_request_supersedes_and_restarts_debounce() {
        let mut s = FetchScheduler::new(Duration::from_millis(250), 3, Duration::from_millis(100));
        let t0 = Instant::now();
        s.request(t0, "old".into(), art());
        // A newer track arrives 100 ms later; its window runs to t0+350.
        s.request(t0 + Duration::from_millis(100), "new".into(), art());
        // The old track's window (t0+250) has passed but it was dropped, and
        // the new one is not due until its own window elapses.
        assert!(s.due(t0 + Duration::from_millis(300)).is_none());
        let due = s.due(t0 + Duration::from_millis(350)).unwrap();
        assert_eq!(due.0, "new");
    }

    #[test]
    fn retries_with_backoff_then_gives_up() {
        let mut s = FetchScheduler::new(Duration::ZERO, 3, Duration::from_millis(100));
        let t0 = Instant::now();
        s.request(t0, "k".into(), art());
        assert!(s.due(t0).is_some());

        // First failure → next attempt after 100 ms.
        s.on_failure(t0, "k");
        assert!(s.due(t0 + Duration::from_millis(99)).is_none());
        assert!(s.due(t0 + Duration::from_millis(100)).is_some());

        // Second failure → next attempt after 200 ms.
        let t1 = t0 + Duration::from_millis(100);
        s.on_failure(t1, "k");
        assert!(s.due(t1 + Duration::from_millis(199)).is_none());
        assert!(s.due(t1 + Duration::from_millis(200)).is_some());

        // Third failure exhausts the budget → abandoned.
        s.on_failure(t1 + Duration::from_millis(200), "k");
        assert!(s.is_idle());
        assert!(s.due(t1 + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn success_clears_pending() {
        let mut s = FetchScheduler::new(Duration::ZERO, 3, Duration::from_millis(100));
        let t0 = Instant::now();
        s.request(t0, "k".into(), art());
        assert!(s.due(t0).is_some());
        s.on_success("k");
        assert!(s.is_idle());
    }

    #[test]
    fn stale_outcome_is_ignored() {
        let mut s = FetchScheduler::new(Duration::ZERO, 3, Duration::from_millis(100));
        let t0 = Instant::now();
        s.request(t0, "current".into(), art());
        // An outcome for a superseded track must not disturb the current one.
        s.on_success("old");
        s.on_failure(t0, "old");
        assert!(!s.is_idle());
        assert_eq!(s.due(t0).unwrap().0, "current");
    }

    #[test]
    fn next_deadline_tracks_pending() {
        let mut s = FetchScheduler::new(Duration::from_millis(250), 3, Duration::from_millis(100));
        assert!(s.next_deadline().is_none());
        let t0 = Instant::now();
        s.request(t0, "k".into(), art());
        assert_eq!(s.next_deadline(), Some(t0 + Duration::from_millis(250)));
    }
}
