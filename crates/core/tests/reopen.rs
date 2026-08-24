//! Runtime capture-reopen integration tests (US-CAP-2, part A). A test-local
//! [`FaultyBackend`] drives the engine through faults, device switches and open
//! failures with no audio hardware; every wait is deadline-polled rather than a
//! fixed sleep, so the suite stays robust on shared CI runners and finishes
//! well under ten seconds.

mod support {
    pub mod faulty;
}

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{Engine, EngineConfig, FeatureSnapshot, StreamHealth};
use support::faulty::{FaultyBackend, FaultyControl};

/// Poll `f` every 5 ms until it returns `true` or `timeout` elapses. Returns
/// whether it became true in time.
fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(5));
    }
}

/// Peak display-bar value of a snapshot.
fn peak_bar(snap: &FeatureSnapshot) -> f32 {
    snap.spectrum[..snap.spectrum_len as usize]
        .iter()
        .copied()
        .fold(0.0f32, f32::max)
}

/// Index of the loudest display bar of a snapshot.
fn argmax_bar(snap: &FeatureSnapshot) -> usize {
    let mut best = 0usize;
    let mut best_val = f32::MIN;
    for (i, &v) in snap.spectrum[..snap.spectrum_len as usize]
        .iter()
        .enumerate()
    {
        if v > best_val {
            best_val = v;
            best = i;
        }
    }
    best
}

/// A 50 ms route poll makes the watcher react well within the one-second budget
/// each test asserts against.
fn fast_watch() -> EngineConfig {
    EngineConfig {
        route_poll: Duration::from_millis(50),
        ..EngineConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Test 1: a stream fault triggers a reopen and resumes within a second.
// ---------------------------------------------------------------------------

#[test]
fn fault_triggers_reopen_within_a_second() {
    let control = FaultyControl::new(48_000, 2, "device-a");
    let (engine, mut reader) = Engine::start(
        Box::new(FaultyBackend::new(Arc::clone(&control))),
        fast_watch(),
    )
    .expect("engine start");

    // The initial stream produces active (non-starved) snapshots.
    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.generation > 0
        }),
        "never saw an active snapshot from the initial stream"
    );

    let pushed_before = engine.stats().pushed_frames;
    assert!(pushed_before > 0, "no frames pushed before the fault");
    let gen_before = reader.latest().generation;

    // Device lost: producer stops, health flips to Errored.
    control.trip_fault();

    // The watcher reopens within a second: opens climbs to 2, reopens to 1.
    assert!(
        poll_until(Duration::from_secs(1), || {
            engine.stats().reopens >= 1 && control.opens() == 2
        }),
        "reopen did not happen within 1 s (opens={}, reopens={})",
        control.opens(),
        engine.stats().reopens
    );

    // The new stream drives advancing, non-starved snapshots again.
    assert!(
        poll_until(Duration::from_secs(1), || {
            let s = *reader.latest();
            !s.starved && s.generation > gen_before
        }),
        "no advancing active snapshot from the new stream"
    );

    let stats = engine.stats();
    assert_eq!(stats.reopens, 1, "expected exactly one reopen");
    assert!(
        stats.pushed_frames >= pushed_before,
        "cumulative pushed_frames reset across the reopen: {} < {}",
        stats.pushed_frames,
        pushed_before
    );

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 2: a route change reopens and renegotiates 44.1 -> 48 kHz transparently.
// ---------------------------------------------------------------------------

#[test]
fn route_change_triggers_reopen_and_reformat() {
    let control = FaultyControl::new(44_100, 2, "device-a");
    let (engine, mut reader) = Engine::start(
        Box::new(FaultyBackend::new(Arc::clone(&control))),
        fast_watch(),
    )
    .expect("engine start");

    // Settle at 44.1 kHz with a lively bar, then record the peak bar index.
    assert!(
        poll_until(Duration::from_secs(3), || {
            let s = *reader.latest();
            !s.starved && s.sample_rate == 44_100 && peak_bar(&s) > 0.5
        }),
        "never settled at 44.1 kHz with a lively bar"
    );
    let pre_bar = argmax_bar(reader.latest());

    // The OS default route moves to another device that runs at 48 kHz.
    control.set_next_format(48_000, 2);
    control.set_route_id("device-b");

    // Within a second, non-starved snapshots report the new sample rate.
    assert!(
        poll_until(Duration::from_secs(1), || {
            let s = *reader.latest();
            !s.starved && s.sample_rate == 48_000
        }),
        "sample rate did not switch to 48 kHz within 1 s"
    );

    // Once the 48 kHz stream re-animates, the tone lands in the same bar (±1):
    // the log frequency mapping is rate-independent.
    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.sample_rate == 48_000 && peak_bar(&s) > 0.5
        }),
        "the 48 kHz stream never re-animated a lively bar"
    );
    let post_bar = argmax_bar(reader.latest());
    assert!(
        (pre_bar as i32 - post_bar as i32).abs() <= 1,
        "peak bar moved across the sample-rate switch: {pre_bar} -> {post_bar}"
    );
    assert!(engine.stats().reopens >= 1, "no reopen recorded");

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 3: open failures keep the engine alive, then it recovers.
// ---------------------------------------------------------------------------

#[test]
fn open_failure_keeps_engine_alive() {
    let control = FaultyControl::new(48_000, 2, "device-a");
    let (engine, mut reader) = Engine::start(
        Box::new(FaultyBackend::new(Arc::clone(&control))),
        fast_watch(),
    )
    .expect("engine start");

    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.generation > 0
        }),
        "never saw an active snapshot from the initial stream"
    );

    // The device is gone and the next three reopen attempts fail.
    control.fail_next_opens(3);
    control.trip_fault();

    // At least one reopen failure is recorded, and the engine stays alive:
    // the hop grid keeps advancing on synthesized silence and health is Errored.
    assert!(
        poll_until(Duration::from_secs(2), || engine.stats().reopen_failures
            >= 1),
        "no reopen failure recorded"
    );
    let g1 = reader.latest().generation;
    assert!(
        poll_until(Duration::from_secs(1), || reader.latest().generation > g1),
        "the hop grid froze while reopen kept failing"
    );
    assert!(
        matches!(engine.health(), StreamHealth::Errored(_)),
        "health should be Errored while the device is gone"
    );

    // After the failures clear, the next reopen succeeds and playback resumes.
    assert!(
        poll_until(Duration::from_secs(8), || {
            let s = *reader.latest();
            engine.stats().reopens >= 1 && !s.starved
        }),
        "engine never recovered after the failures cleared"
    );
    assert!(
        control.opens() >= 4,
        "expected >= 4 opens (1 initial + 3 failed + a success), got {}",
        control.opens()
    );

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 4: stop() is clean while the reopen retry loop is spinning.
// ---------------------------------------------------------------------------

#[test]
fn stop_is_clean_during_retry_loop() {
    let control = FaultyControl::new(48_000, 2, "device-a");
    let (engine, mut reader) = Engine::start(
        Box::new(FaultyBackend::new(Arc::clone(&control))),
        fast_watch(),
    )
    .expect("engine start");

    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.generation > 0
        }),
        "never saw an active snapshot from the initial stream"
    );

    // Device gone for good: every reopen will fail, so the watcher is stuck in
    // its backoff retry loop.
    control.fail_next_opens(1_000_000);
    control.trip_fault();
    assert!(
        poll_until(Duration::from_secs(2), || engine.stats().reopen_failures
            >= 1),
        "watcher never entered the retry loop"
    );

    // Stopping mid-retry must not hang.
    let started = Instant::now();
    engine.stop();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stop() hung during the retry loop: {:?}",
        started.elapsed()
    );
}
