//! The cross-platform **session-selection policy** for US-META-1.
//!
//! When several media sessions are live at once, exactly one must win the
//! now-playing slot. The policy is deliberately platform-neutral and pure so it
//! can be unit-tested off-Windows: a backend reduces each of its native
//! sessions to a [`SessionSnapshot`] and calls [`select_winner`], which never
//! touches any platform API.
//!
//! The rule, in priority order:
//!
//! 1. **Playing beats everything.** A session whose status
//!    [`is_playing`](crate::model::PlaybackStatus::is_playing) outranks any
//!    number of paused or stopped sessions.
//! 2. **Most recent activity wins.** Among sessions of equal playing-rank, the
//!    one with the larger `last_activity` marker wins. The marker is a
//!    monotonic counter the backend bumps whenever a session fires a
//!    metadata/playback event, so "most recent activity" means "last session we
//!    heard from".
//! 3. **Ties are deterministic.** If playing-rank and activity are identical,
//!    the lexicographically smallest `app_id` wins, so the same set of sessions
//!    always resolves to the same winner regardless of enumeration order.
//!
//! An empty session set yields `None` — the caller emits
//! [`MetaEvent::Cleared`](crate::model::MetaEvent::Cleared). A non-empty set
//! always has a winner: when nothing is playing, the most-recently-active
//! session (typically a freshly paused track) still supplies metadata, which is
//! the desired behaviour — a paused Spotify track should still theme the
//! scenes. Genuine absence is only ever the empty set.

use crate::model::PlaybackStatus;

/// The minimal, platform-neutral view of one media session the policy needs.
///
/// A backend builds one per native session each time it re-evaluates the
/// winner. `last_activity` is a monotonic marker (higher = more recent) the
/// backend assigns from its own counter when a session last signalled activity;
/// its absolute value is meaningless, only the ordering matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The originating application's identity (Windows `AppUserModelId` / MPRIS
    /// bus name). Used as the deterministic tie-breaker.
    pub app_id: String,
    /// The session's playback status.
    pub status: PlaybackStatus,
    /// Monotonic recency marker; the largest value among equal-rank sessions
    /// wins. Higher means more recently active.
    pub last_activity: u64,
}

impl SessionSnapshot {
    /// Convenience constructor.
    #[must_use]
    pub fn new(app_id: impl Into<String>, status: PlaybackStatus, last_activity: u64) -> Self {
        Self {
            app_id: app_id.into(),
            status,
            last_activity,
        }
    }
}

/// Rank two snapshots by the US-META-1 policy: playing first, then most recent
/// activity, then lexicographically smallest `app_id`.
///
/// Returns [`Ordering::Greater`] when `a` should win over `b`, so the winner is
/// the maximum under this ordering.
///
/// [`Ordering::Greater`]: std::cmp::Ordering::Greater
fn better(a: &SessionSnapshot, b: &SessionSnapshot) -> std::cmp::Ordering {
    // Playing rank: playing outranks everything else.
    let by_playing = a.status.is_playing().cmp(&b.status.is_playing());
    if by_playing != std::cmp::Ordering::Equal {
        return by_playing;
    }
    // More recent activity wins.
    let by_activity = a.last_activity.cmp(&b.last_activity);
    if by_activity != std::cmp::Ordering::Equal {
        return by_activity;
    }
    // Deterministic tie-break: smaller app_id wins, so reverse the string order
    // (smaller string => "greater" under this max-picking ordering).
    b.app_id.cmp(&a.app_id)
}

/// Select the index of the winning session per the US-META-1 policy, or `None`
/// when the set is empty.
///
/// On ties every criterion is total, so the result is independent of the order
/// in which `sessions` are supplied. Among equally-ranked, equally-recent
/// sessions the lexicographically smallest `app_id` wins; if two snapshots are
/// fully identical (same `app_id`, status and activity) the earlier index wins,
/// which is still deterministic for a given input.
#[must_use]
pub fn select_winner(sessions: &[SessionSnapshot]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, s) in sessions.iter().enumerate() {
        match best {
            None => best = Some(i),
            Some(b) => {
                if better(s, &sessions[b]) == std::cmp::Ordering::Greater {
                    best = Some(i);
                }
            }
        }
    }
    best
}

/// Select and clone the winning snapshot, or `None` when the set is empty.
#[must_use]
pub fn select_winner_snapshot(sessions: &[SessionSnapshot]) -> Option<SessionSnapshot> {
    select_winner(sessions).map(|i| sessions[i].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PlaybackStatus::{Paused, Playing, Stopped};

    fn s(app: &str, status: PlaybackStatus, act: u64) -> SessionSnapshot {
        SessionSnapshot::new(app, status, act)
    }

    #[test]
    fn empty_set_has_no_winner() {
        assert_eq!(select_winner(&[]), None);
    }

    #[test]
    fn single_session_always_wins() {
        let v = [s("only", Paused, 3)];
        assert_eq!(select_winner(&v), Some(0));
    }

    #[test]
    fn playing_beats_more_recent_paused() {
        // The paused session is more recently active, but playing wins.
        let v = [s("spotify", Playing, 1), s("chrome", Paused, 99)];
        assert_eq!(select_winner(&v), Some(0));
    }

    #[test]
    fn among_playing_most_recent_activity_wins() {
        let v = [s("spotify", Playing, 5), s("chrome", Playing, 8)];
        assert_eq!(select_winner(&v), Some(1));
    }

    #[test]
    fn playing_tie_breaks_on_lexicographic_app_id() {
        // Same rank, same activity: smallest app_id wins deterministically,
        // regardless of input order.
        let a = s("aaa", Playing, 7);
        let z = s("zzz", Playing, 7);
        assert_eq!(select_winner(&[z.clone(), a.clone()]), Some(1));
        assert_eq!(select_winner(&[a, z]), Some(0));
    }

    #[test]
    fn with_nothing_playing_most_recent_still_wins() {
        // No session is playing, but a non-empty set still resolves to the
        // most-recently-active one (a freshly paused track keeps theming).
        let v = [s("spotify", Paused, 4), s("chrome", Stopped, 10)];
        assert_eq!(select_winner(&v), Some(1));
    }

    #[test]
    fn selection_is_order_independent() {
        let base = [
            s("chrome", Playing, 8),
            s("spotify", Paused, 99),
            s("firefox", Playing, 8),
        ];
        // Winner is a playing session with activity 8; tie-break -> "chrome".
        let mut reversed = base.to_vec();
        reversed.reverse();
        let w1 = select_winner_snapshot(&base).unwrap();
        let w2 = select_winner_snapshot(&reversed).unwrap();
        assert_eq!(w1.app_id, "chrome");
        assert_eq!(w2.app_id, "chrome");
    }
}
