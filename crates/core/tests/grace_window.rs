//! CAP-2 grace-window and reopen-ordering tests (US-CAP-2, part B). The engine
//! must ride out a device switch as a reconnecting state instead of failing on
//! the first fault, and — the Linux PipeWire fix — must drop an errored stream
//! before opening its replacement while keeping a *healthy* route swap seamless.
//! Everything runs on the synthetic/faulty backends with no audio hardware, and
//! every wait is deadline-polled rather than a fixed sleep.

mod support {
    pub mod faulty;
}

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::engine::EngineHealth;
use scia_core::{Engine, EngineConfig, Pacing, Signal, SyntheticBackend};
use support::faulty::{FaultyBackend, FaultyControl};

/// Poll `f` every 5 ms until it returns `true` or `timeout` elapses.
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

/// A fast route poll and default (10 s) reconnect deadline.
fn fast_watch() -> EngineConfig {
    EngineConfig {
        route_poll: Duration::from_millis(50),
        ..EngineConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Test 1: a fault becomes Reconnecting (attempts counting), then Ok on reopen.
// ---------------------------------------------------------------------------

#[test]
fn health_reconnects_then_recovers() {
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
    // Healthy capture reports Ok.
    assert_eq!(engine.engine_health(), EngineHealth::Ok);

    // The device is lost and the next three reopens fail before it comes back.
    control.fail_next_opens(3);
    control.trip_fault();

    // While reopen keeps failing the engine reports Reconnecting with a climbing
    // attempt count — it does NOT fail on the first fault.
    assert!(
        poll_until(Duration::from_secs(2), || matches!(
            engine.engine_health(),
            EngineHealth::Reconnecting { attempts, .. } if attempts >= 1
        )),
        "engine never entered the Reconnecting grace state (health={:?})",
        engine.engine_health()
    );
    // The last reopen error is recorded for diagnostics.
    assert!(
        engine.last_reopen_error().is_some(),
        "last_reopen_error not populated during reconnect"
    );

    // Once the failures clear, a reopen succeeds and health returns to Ok.
    assert!(
        poll_until(Duration::from_secs(8), || {
            let s = *reader.latest();
            engine.engine_health() == EngineHealth::Ok && engine.stats().reopens >= 1 && !s.starved
        }),
        "engine never recovered to Ok (health={:?})",
        engine.engine_health()
    );

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 2: persistent failure crosses the (shortened) deadline into Failed.
// ---------------------------------------------------------------------------

#[test]
fn health_fails_after_deadline() {
    let config = EngineConfig {
        route_poll: Duration::from_millis(50),
        // Shortened deadline hook: no real ten-second wait.
        reconnect_deadline: Duration::from_millis(300),
        ..EngineConfig::default()
    };
    let control = FaultyControl::new(48_000, 2, "device-a");
    let (engine, mut reader) =
        Engine::start(Box::new(FaultyBackend::new(Arc::clone(&control))), config)
            .expect("engine start");

    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.generation > 0
        }),
        "never saw an active snapshot from the initial stream"
    );

    // The device is gone for good: every reopen fails.
    control.fail_next_opens(1_000_000);
    control.trip_fault();

    // After the 300 ms deadline of continuous failure, health becomes Failed
    // with the last error text.
    assert!(
        poll_until(Duration::from_secs(3), || matches!(
            engine.engine_health(),
            EngineHealth::Failed { .. }
        )),
        "engine never reached Failed (health={:?})",
        engine.engine_health()
    );
    let EngineHealth::Failed { error } = engine.engine_health() else {
        panic!("expected Failed");
    };
    assert!(!error.is_empty(), "Failed carried an empty error");
    assert_eq!(
        Some(error),
        engine.last_reopen_error(),
        "Failed error should match last_reopen_error"
    );

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 3: an errored reopen drops the dead stream BEFORE opening the new one.
// ---------------------------------------------------------------------------

#[test]
fn errored_reopen_drops_old_before_opening_new() {
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
    assert_eq!(control.max_live(), 1, "one stream live after start");

    // Device lost: the reopen must take the down path and drop the errored stream
    // before opening its replacement — the CAP-2 Linux fix.
    control.trip_fault();
    assert!(
        poll_until(Duration::from_secs(2), || engine.stats().reopens >= 1),
        "reopen never happened"
    );

    // The two streams were never live at once: the dead one went away first.
    assert_eq!(
        control.max_live(),
        1,
        "errored reopen opened the replacement while the dead stream was still live"
    );
    assert_eq!(control.live(), 1, "exactly one live stream after recovery");

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 4: a healthy route swap stays seamless (open before drop).
// ---------------------------------------------------------------------------

#[test]
fn healthy_route_swap_opens_before_dropping() {
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

    // The OS default route moves while the current stream is perfectly healthy —
    // the seamless path (e.g. a Windows default-endpoint notification). No fault
    // is tripped, so the stream never errors.
    control.set_route_id("device-b");
    assert!(
        poll_until(Duration::from_secs(2), || engine.stats().reopens >= 1),
        "route change never triggered a reopen"
    );

    // The replacement was opened before the old stream was dropped, so there was
    // no gap in the live ring — and health never left Ok.
    assert_eq!(
        control.max_live(),
        2,
        "healthy route swap should open the new stream before dropping the old"
    );
    assert_eq!(engine.engine_health(), EngineHealth::Ok);

    engine.stop();
}

// ---------------------------------------------------------------------------
// Test 5: a never-erroring stream never enters the grace state (the Windows
// device-switch shape, which flips the reopen request without a stream error).
// ---------------------------------------------------------------------------

#[test]
fn healthy_stream_stays_ok() {
    let backend = SyntheticBackend {
        signal: Signal::Sine {
            hz: 440.0,
            amp: 0.5,
        },
        pacing: Pacing::Realtime,
        ..SyntheticBackend::default()
    };
    let (engine, mut reader) =
        Engine::start(Box::new(backend), fast_watch()).expect("engine start");

    assert!(
        poll_until(Duration::from_secs(2), || reader.latest().generation > 0),
        "no snapshot from the synthetic feed"
    );
    // Sample health across a few ticks: it must stay Ok the whole time.
    for _ in 0..20 {
        assert_eq!(engine.engine_health(), EngineHealth::Ok);
        sleep(Duration::from_millis(20));
    }
    assert_eq!(engine.stats().reopen_failures, 0);
    assert!(engine.last_reopen_error().is_none());

    engine.stop();
}
