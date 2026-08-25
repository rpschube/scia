//! Foreground fullscreen-app detection and the pause control it drives
//! (US-PERF-3, the v1 hardening leg on top of the idle/quiet machinery).
//!
//! A game running fullscreen-exclusive covers the terminal, so there is nothing
//! worth rendering and no reason to keep the DSP grid at its full hop rate. This
//! module owns a platform [`FullscreenDetector`] and a low-cadence
//! [`FullscreenWatch`] that polls it and publishes the answer into a shared
//! "pause" [`AtomicBool`]. Two consumers read that one flag:
//!
//!   * the **render loop** stops drawing frames while it is set (see the TUI
//!     `run` loop), leaving the terminal untouched;
//!   * the **DSP thread** forces itself into its *existing* idle downshift while
//!     it is set (see [`crate::dsp::run`]) — the same slow-poll, FFT-free path
//!     the silence state machine already uses, so no second throttle is
//!     introduced. The engine hands the flag to the DSP thread and exposes it
//!     with [`Engine::pause_flag`](crate::Engine::pause_flag).
//!
//! The check must be microseconds-cheap because it runs on a timer; the polling
//! thread does nothing but call it and sleep.
//!
//! ## Platform scope (v1)
//!
//! * **Windows** — the primary gaming target — asks the shell via
//!   `SHQueryUserNotificationState`. It reports what the shell believes the
//!   foreground app is doing; a fullscreen-exclusive Direct3D app (a game) or a
//!   fullscreen presentation surfaces as a distinct state. The call is a single
//!   cheap shell query with no allocation. See [`detector`].
//! * **Linux / macOS** — a no-op detector that always reports not-fullscreen.
//!   This is a deliberate v1 scope decision: there is no reliable *portable*
//!   Wayland signal for "a fullscreen app is foreground" (each compositor
//!   differs, and there is no cross-desktop API), and X11/quirk-specific probing
//!   is out of scope for this leg. The trait is the seam a later per-desktop
//!   heuristic slots into without touching the engine or the render loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The default detector poll cadence. The check itself is microseconds-cheap, so
/// a slow poll costs effectively nothing; two seconds keeps the resume latency
/// (from a game exiting to full rendering) well under the time it takes a person
/// to alt-tab back and look at the terminal.
pub const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// A platform probe for "is a fullscreen-exclusive app foreground right now?".
///
/// Implementations must be **microseconds-cheap** and non-blocking: the
/// [`FullscreenWatch`] polls this on a timer, and the value gates both the
/// render loop and the DSP downshift. A failed probe should report `false`
/// (not-fullscreen) rather than error, so a platform quirk can never wedge the
/// visualizer into a permanent pause.
pub trait FullscreenDetector: Send {
    /// Whether a fullscreen-exclusive app is currently the foreground window.
    fn is_fullscreen(&self) -> bool;
}

/// The non-Windows detector: always not-fullscreen. See the module docs for the
/// v1 scope decision behind this (no reliable portable Wayland signal yet).
pub struct NoopFullscreenDetector;

impl FullscreenDetector for NoopFullscreenDetector {
    fn is_fullscreen(&self) -> bool {
        false
    }
}

/// The Windows detector, built on the shell's user-notification state.
///
/// `SHQueryUserNotificationState` returns the shell's view of the foreground
/// app. We treat exactly two states as "pause the visualizer":
///
///   * `QUNS_RUNNING_D3D_FULL_SCREEN` — a fullscreen-exclusive Direct3D app is
///     running (the classic fullscreen game);
///   * `QUNS_PRESENTATION_MODE` — the machine is in presentation mode (a
///     fullscreen presentation is up).
///
/// `QUNS_BUSY` is deliberately **not** treated as fullscreen: it also fires for
/// a browser (or any app) in F11 fullscreen, where the terminal can still be
/// visible on another monitor, so pausing on it would be wrong. Every other
/// state (`QUNS_ACCEPTS_NOTIFICATIONS`, `QUNS_QUIET_TIME`, `QUNS_APP`,
/// `QUNS_NOT_PRESENT`) is not-fullscreen. A failed query reports not-fullscreen
/// so a shell hiccup never strands the visualizer paused.
#[cfg(all(windows, feature = "fullscreen"))]
#[allow(unsafe_code)]
mod windows_impl {
    use super::FullscreenDetector;
    use windows::Win32::UI::Shell::{
        QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
    };

    /// See the module-level docs on [`super`] for the QUNS state mapping.
    pub struct WindowsFullscreenDetector;

    impl FullscreenDetector for WindowsFullscreenDetector {
        fn is_fullscreen(&self) -> bool {
            // SAFETY: `SHQueryUserNotificationState` is a plain shell query that
            // writes a single `i32`-backed enum through the pointer the `windows`
            // wrapper manages internally; it takes no borrowed state and has no
            // threading requirements. The crate's blanket `deny(unsafe_code)` is
            // opted out of here exactly as the WASAPI backend modules do.
            match unsafe { SHQueryUserNotificationState() } {
                Ok(state) => {
                    state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE
                }
                // A failed query (no shell, session 0, ...) is treated as
                // not-fullscreen: never wedge the visualizer paused on an error.
                Err(_) => false,
            }
        }
    }
}

/// Construct the detector for this platform: the real Windows shell probe when
/// compiled for Windows with the `fullscreen` feature, otherwise the no-op that
/// always reports not-fullscreen (see the module docs for the v1 scope).
#[must_use]
pub fn detector() -> Box<dyn FullscreenDetector> {
    #[cfg(all(windows, feature = "fullscreen"))]
    {
        Box::new(windows_impl::WindowsFullscreenDetector)
    }
    #[cfg(not(all(windows, feature = "fullscreen")))]
    {
        Box::new(NoopFullscreenDetector)
    }
}

/// A background poller that reflects a [`FullscreenDetector`] into a shared
/// [`AtomicBool`] on a fixed cadence.
///
/// [`spawn`](FullscreenWatch::spawn) publishes the current state once
/// synchronously (so the very first frame already sees reality), then a named
/// thread republishes it every `poll` interval. Dropping the watch flips a stop
/// flag and joins the thread — the sleep is broken into short steps so a drop
/// returns promptly rather than after a full poll period, mirroring the engine's
/// route-watcher lifecycle.
pub struct FullscreenWatch {
    state: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FullscreenWatch {
    /// Start polling `detector` every `poll`, publishing each answer into
    /// `state`. `state` is shared so the same flag the engine's DSP thread reads
    /// (see [`Engine::pause_flag`](crate::Engine::pause_flag)) is the one this
    /// watch drives. The current state is published once before the thread is
    /// spawned, so a caller that reads `state` immediately never sees a stale
    /// default.
    #[must_use]
    pub fn spawn(
        detector: Box<dyn FullscreenDetector>,
        poll: Duration,
        state: Arc<AtomicBool>,
    ) -> Self {
        // Publish once synchronously so the first render frame and the first DSP
        // wake already reflect the true state, not the flag's initial value.
        state.store(detector.is_fullscreen(), Ordering::Release);

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name("scia-fullscreen".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    thread_state.store(detector.is_fullscreen(), Ordering::Release);
                    // A flag-polled sleep so a dropped watch stops within a step
                    // rather than after a full poll period.
                    if sleep_with_stop(&thread_stop, poll) {
                        break;
                    }
                }
            })
            .expect("spawn scia-fullscreen thread");

        Self {
            state,
            stop,
            join: Some(join),
        }
    }

    /// A clone of the shared flag this watch drives — `true` while a fullscreen
    /// app is foreground.
    #[must_use]
    pub fn state(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state)
    }
}

impl Drop for FullscreenWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Sleep `dur`, waking early to re-check `stop` every 50 ms. Returns `true` if
/// `stop` was observed set (so the caller should stop looping), `false` if the
/// full duration elapsed.
fn sleep_with_stop(stop: &AtomicBool, dur: Duration) -> bool {
    const STEP: Duration = Duration::from_millis(50);
    let deadline = Instant::now() + dur;
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(STEP.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scriptable fake detector: reports whatever a shared flag says, and
    /// counts how many times it was polled so a test can prove the thread runs.
    struct FakeDetector {
        state: Arc<AtomicBool>,
        polls: Arc<std::sync::atomic::AtomicU64>,
    }

    impl FullscreenDetector for FakeDetector {
        fn is_fullscreen(&self) -> bool {
            self.polls.fetch_add(1, Ordering::Relaxed);
            self.state.load(Ordering::Acquire)
        }
    }

    #[test]
    fn detector_reports_not_fullscreen_in_this_env() {
        // On Linux/macOS this is the no-op; on the Windows CI runner it is the
        // real `SHQueryUserNotificationState` impl, and CI is never in a
        // fullscreen game — so either way the answer is a clean `false` (never a
        // panic or an error surfacing as a wedged pause). This is the trait test
        // the Windows gate exercises against the real impl.
        assert!(!detector().is_fullscreen());
    }

    #[test]
    fn noop_detector_is_never_fullscreen() {
        assert!(!NoopFullscreenDetector.is_fullscreen());
    }

    #[test]
    fn watch_publishes_initial_state_synchronously() {
        // The flag reflects the detector before `spawn` returns, so the first
        // consumer read is never stale.
        let source = Arc::new(AtomicBool::new(true));
        let polls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let detector = FakeDetector {
            state: Arc::clone(&source),
            polls: Arc::clone(&polls),
        };
        let flag = Arc::new(AtomicBool::new(false));
        let watch = FullscreenWatch::spawn(Box::new(detector), DEFAULT_POLL, Arc::clone(&flag));
        assert!(
            flag.load(Ordering::Acquire),
            "the initial state was not published synchronously by spawn"
        );
        drop(watch);
    }

    #[test]
    fn watch_reflects_detector_changes_and_joins_cleanly() {
        let source = Arc::new(AtomicBool::new(false));
        let polls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let detector = FakeDetector {
            state: Arc::clone(&source),
            polls: Arc::clone(&polls),
        };
        let flag = Arc::new(AtomicBool::new(false));
        // A fast poll so the test does not wait out the 2 s default.
        let watch = FullscreenWatch::spawn(
            Box::new(detector),
            Duration::from_millis(10),
            Arc::clone(&flag),
        );

        assert!(!flag.load(Ordering::Acquire), "should start not-fullscreen");

        // Flip the fake to fullscreen: the shared flag follows within a few polls.
        source.store(true, Ordering::Release);
        assert!(
            wait_until(Duration::from_secs(1), || flag.load(Ordering::Acquire)),
            "flag never rose after the detector reported fullscreen"
        );

        // Flip back: the flag clears again.
        source.store(false, Ordering::Release);
        assert!(
            wait_until(Duration::from_secs(1), || !flag.load(Ordering::Acquire)),
            "flag never cleared after the detector reported not-fullscreen"
        );

        // The thread was polling (proves it actually ran), and drop joins it
        // without hanging — a hung join would time out the whole test run.
        assert!(
            polls.load(Ordering::Relaxed) >= 2,
            "the watch thread never polled the detector"
        );
        drop(watch);
    }

    /// Poll `pred` every 2 ms until it holds or `timeout` elapses.
    fn wait_until(timeout: Duration, pred: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        pred()
    }
}
