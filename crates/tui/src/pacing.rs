//! Frame pacing policy: how long the loop should wait between frames, and when
//! to drop to the idle rate. Pure functions so the policy is unit-testable
//! without a terminal or a clock.

use std::time::Duration;

/// Frame rate the loop falls back to while the feed has been starved past
/// [`STARVE_THROTTLE`]. The DSP keeps publishing decaying bars, so a low rate
/// still looks smooth.
pub const IDLE_FPS: u32 = 10;

/// How long the feed must be continuously starved before the loop downshifts to
/// [`IDLE_FPS`].
pub const STARVE_THROTTLE: Duration = Duration::from_secs(2);

/// The frame interval for a target `fps`. Falls back to a 60 fps interval if
/// `fps` is zero so the loop can never divide by zero.
pub fn active_interval(fps: u32) -> Duration {
    let fps = if fps == 0 { 60 } else { fps };
    Duration::from_secs_f64(1.0 / f64::from(fps))
}

/// The interval to wait before the next frame: the active interval normally,
/// or the idle interval once the feed has been starved longer than
/// [`STARVE_THROTTLE`].
pub fn target_interval(fps: u32, starved_for: Duration) -> Duration {
    if starved_for > STARVE_THROTTLE {
        active_interval(IDLE_FPS)
    } else {
        active_interval(fps)
    }
}

/// The poll cadence while paused for a foreground fullscreen app (US-PERF-3).
/// The window is covered, so the loop stops drawing and only polls input at this
/// slow rate. Reuses [`IDLE_FPS`] so a state change (the app exiting, or a
/// keystroke) is picked up within one ~100 ms tick — "resume within one poll
/// interval".
pub fn pause_interval() -> Duration {
    active_interval(IDLE_FPS)
}

/// Whether to draw this frame under the fullscreen-pause policy, given the
/// current and previous frame's paused state.
///
/// While paused the terminal is left untouched — the covered window content does
/// not matter — *except* the single transition frame entering the pause, which
/// is drawn so the status line surfaces the paused-for-fullscreen state. Resume
/// (not paused) always draws.
pub fn should_draw(fs_paused: bool, was_paused: bool) -> bool {
    !fs_paused || !was_paused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_interval_is_the_frame_period() {
        let ms = active_interval(60).as_secs_f64() * 1000.0;
        assert!((ms - 16.666).abs() < 0.01, "60 fps interval was {ms} ms");
    }

    #[test]
    fn active_interval_survives_zero_fps() {
        assert_eq!(active_interval(0), active_interval(60));
    }

    #[test]
    fn pause_interval_is_the_idle_rate() {
        // The pause poll runs at the idle rate: slow enough for near-zero CPU,
        // fast enough that resume/input latency stays ~one 100 ms tick.
        assert_eq!(pause_interval(), active_interval(IDLE_FPS));
        assert_eq!(pause_interval(), Duration::from_millis(100));
    }

    #[test]
    fn should_draw_policy() {
        // Not paused: always draw (this covers the disabled case too — a disabled
        // feature never sets fs_paused, so every frame draws).
        assert!(should_draw(false, false));
        assert!(should_draw(false, true), "resume draws");
        // Entering pause: draw the one transition frame that shows the status.
        assert!(should_draw(true, false), "entry frame draws");
        // Held paused: skip drawing, leaving the terminal untouched.
        assert!(!should_draw(true, true), "held pause stops draws");
    }

    #[test]
    fn idle_interval_after_prolonged_starvation() {
        // Below the threshold: full rate.
        assert_eq!(
            target_interval(60, Duration::from_secs(1)),
            active_interval(60),
        );
        // Past the threshold: 10 fps -> exactly 100 ms.
        assert_eq!(
            target_interval(60, Duration::from_secs(3)),
            Duration::from_millis(100),
        );
    }
}
