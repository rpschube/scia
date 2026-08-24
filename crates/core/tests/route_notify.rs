//! Event-driven route-notification (`backends::wasapi_route`) integration tests
//! (US-CAP-2, part B). They run on CI runners with no audio endpoint: off
//! Windows the notifier is an `Unsupported` stub; on Windows registration either
//! succeeds (even with no endpoint) or reports a clean backend error, and never
//! panics or hangs. A final test proves the notifier never interferes with a
//! normal engine run.

#![cfg(feature = "route-notify")]

#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{CaptureError, Engine, EngineConfig, RouteNotifier, SyntheticBackend};

/// Off Windows, event-driven route notification is a stub that always reports
/// `Unsupported` — never a panic.
#[cfg(not(windows))]
#[test]
fn route_notify_is_unsupported_off_windows() {
    match RouteNotifier::start(Box::new(|| {})) {
        Err(CaptureError::Unsupported(msg)) => {
            println!("route notify unsupported off Windows, as expected: {msg}");
        }
        Err(e) => panic!("expected Unsupported off Windows, got a different error: {e:?}"),
        Ok(_) => panic!("expected Unsupported off Windows, but start() succeeded"),
    }
}

/// On Windows, `start` must either succeed (registration works even without an
/// endpoint) or report a clean backend condition — never panic, never
/// `Unsupported`. When it succeeds, dropping it must return promptly (the notify
/// thread unregisters and joins), well within three seconds.
#[cfg(windows)]
#[test]
fn route_notify_start_or_skip_and_drop_is_prompt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_cb = Arc::clone(&hits);
    match RouteNotifier::start(Box::new(move || {
        hits_cb.fetch_add(1, Ordering::Relaxed);
    })) {
        Ok(notifier) => {
            println!("route notifier registered");
            // Dropping unregisters the client and joins the notify thread; it
            // must not hang.
            let started = Instant::now();
            drop(notifier);
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(3),
                "dropping the route notifier hung: {elapsed:?}"
            );
            // The counter is only observed to prove the closure type-checks and
            // is callable; no callback is required on a headless runner.
            let _ = hits.load(Ordering::Relaxed);
        }
        Err(CaptureError::Backend(msg)) => {
            println!("skip: route notifier could not register: {msg}");
        }
        Err(CaptureError::Unsupported(msg)) => {
            panic!(
                "route notification must be supported on the Windows route-notify build, got Unsupported: {msg}"
            );
        }
        Err(CaptureError::NoDevice) => {
            // The notifier registers against the enumerator, not a device, so it
            // has no reason to report NoDevice — but tolerate it as a clean skip
            // rather than a panic.
            println!("skip: route notifier reported NoDevice");
        }
    }
}

/// A default-config engine on the synthetic backend runs and stops cleanly with
/// the notifier active (Windows) or absent (elsewhere): the notifier never
/// interferes with the pipeline. `route_notify_active()` reports the platform
/// truth. (The part-A reopen suite runs alongside this and must still pass,
/// proving the flag path is unchanged.)
#[test]
fn engine_runs_and_stops_with_default_notifier() {
    let (engine, mut reader) = Engine::start(
        Box::new(SyntheticBackend::default()),
        EngineConfig::default(),
    )
    .expect("engine start");

    // Off Windows the notifier is unsupported, so it is never active; on the
    // Windows route-notify build it is active when registration succeeded.
    #[cfg(not(windows))]
    assert!(
        !engine.route_notify_active(),
        "route notifier must be inactive off Windows"
    );
    #[cfg(windows)]
    println!("route_notify_active = {}", engine.route_notify_active());

    // The pipeline advances snapshots regardless of the notifier.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut advanced = false;
    while Instant::now() < deadline {
        if reader.latest().generation > 0 {
            advanced = true;
            break;
        }
        sleep(Duration::from_millis(5));
    }
    assert!(advanced, "engine never produced an advancing snapshot");

    // Teardown drops the notifier first, then joins the watcher and DSP thread;
    // it must not hang.
    let started = Instant::now();
    engine.stop();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "engine stop() hung with the notifier wired: {:?}",
        started.elapsed()
    );
}
