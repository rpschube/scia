//! Runtime device-switch tests (US-CAP-7). A test-local [`FaultyBackend`] stands
//! in for real hardware: [`Engine::set_device`] records a selector, the paired
//! [`Engine::request_reopen`] drives the route watcher to reopen, and the
//! backend receives the new selector on that reopen. Every wait is
//! deadline-polled, so the suite stays robust on shared CI runners.

mod support {
    pub mod faulty;
}

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{DeviceSelector, Engine, EngineConfig};
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

/// A fast route poll so the watcher reacts well within the assertion budgets.
fn fast_watch() -> EngineConfig {
    EngineConfig {
        route_poll: Duration::from_millis(50),
        ..EngineConfig::default()
    }
}

#[test]
fn set_device_then_request_reopen_switches_the_backend_device() {
    let control = FaultyControl::new(48_000, 2, "device-a");
    let (engine, mut reader) = Engine::start(
        Box::new(FaultyBackend::new(Arc::clone(&control))),
        fast_watch(),
    )
    .expect("engine start");

    // Settle on the initial stream.
    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.generation > 0
        }),
        "never saw an active snapshot from the initial stream"
    );
    assert_eq!(
        control.last_device(),
        None,
        "no device set before the switch"
    );
    let opens_before = control.opens();

    // Request a runtime switch to a named device, then drive the reopen.
    engine.set_device(DeviceSelector::Named("device-b".to_owned()));
    // A recorded selector alone does not reopen: the switch is applied on the
    // next reopen the watcher performs, which `request_reopen` triggers.
    engine.request_reopen();

    // The watcher reopens and the backend receives the new selector.
    assert!(
        poll_until(Duration::from_secs(1), || {
            control.opens() > opens_before
                && control.last_device() == Some(DeviceSelector::Named("device-b".to_owned()))
        }),
        "the device switch never reached the backend (opens {}→{}, last_device {:?})",
        opens_before,
        control.opens(),
        control.last_device()
    );
    assert!(
        engine.stats().reopens >= 1,
        "the switch did not record a reopen"
    );

    // The new stream keeps the pipeline advancing.
    let gen0 = reader.latest().generation;
    assert!(
        poll_until(Duration::from_secs(1), || {
            let s = *reader.latest();
            !s.starved && s.generation > gen0
        }),
        "no advancing snapshot after the device switch"
    );

    engine.stop();
}

#[test]
fn set_device_without_reopen_request_does_not_switch() {
    let control = FaultyControl::new(48_000, 2, "device-a");
    let (engine, mut reader) = Engine::start(
        Box::new(FaultyBackend::new(Arc::clone(&control))),
        // Disable the watcher so nothing drives a reopen on its own.
        EngineConfig {
            route_watch: false,
            route_notify: false,
            ..EngineConfig::default()
        },
    )
    .expect("engine start");

    assert!(
        poll_until(Duration::from_secs(2), || {
            let s = *reader.latest();
            !s.starved && s.generation > 0
        }),
        "never saw an active snapshot"
    );

    // Record a selector but never request a reopen: with no watcher, the switch
    // stays pending and the backend is never told.
    engine.set_device(DeviceSelector::Named("device-b".to_owned()));
    sleep(Duration::from_millis(200));
    assert_eq!(
        control.last_device(),
        None,
        "the selector must not reach the backend until a reopen runs"
    );

    engine.stop();
}
