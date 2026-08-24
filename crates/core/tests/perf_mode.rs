//! Perf-mode (`backends::wasapi_perf`) integration tests. All must run on CI
//! runners that have no audio endpoint: opening a stream or querying
//! availability either succeeds or reports a device/backend condition, and
//! never panics.

#![cfg(feature = "perf-mode")]

use scia_core::{
    CaptureError, Engine, EngineConfig, PerfModeAvailability, PerfModeConfig, PerfModeState,
    PerfModeStream, SyntheticBackend, perf_mode_availability,
};

/// Off Windows, perf mode is a stub that always reports `Unsupported`.
#[cfg(not(windows))]
#[test]
fn perf_mode_is_unsupported_off_windows() {
    match PerfModeStream::open(&PerfModeConfig::default()) {
        Err(CaptureError::Unsupported(msg)) => {
            println!("perf mode unsupported off Windows, as expected: {msg}");
        }
        Err(e) => panic!("expected Unsupported off Windows, got a different error: {e:?}"),
        Ok(_) => panic!("expected Unsupported off Windows, but open() succeeded"),
    }
}

/// On Windows, `open` must either succeed (and report sane periods) or report a
/// device/backend condition — never panic, never `Unsupported`. The CI Windows
/// job has no audio endpoint, so `NoDevice`/`Backend` is the expected path
/// there; a developer machine with an endpoint exercises the `Ok` path.
#[cfg(windows)]
#[test]
fn perf_mode_open_or_skip() {
    match PerfModeStream::open(&PerfModeConfig::default()) {
        Ok(stream) => {
            let info = stream.info();
            println!("perf mode opened: {info:?}");
            assert!(
                info.chosen_period_frames > 0,
                "chosen period must be positive on a successful open"
            );
            assert!(
                info.sample_rate > 0,
                "sample rate must be positive on a successful open"
            );
            // Dropping stops the companion stream and releases the COM objects.
            drop(stream);
        }
        Err(CaptureError::NoDevice) => {
            println!("skip: no render endpoint available (NoDevice)");
        }
        Err(CaptureError::Backend(msg)) => {
            println!("skip: backend could not open a companion stream: {msg}");
        }
        Err(CaptureError::Unsupported(msg)) => {
            panic!("perf mode must be supported on Windows, got Unsupported: {msg}");
        }
    }
}

/// Off Windows, capability detection always reports `Unsupported`.
#[cfg(not(windows))]
#[test]
fn availability_is_unsupported_off_windows() {
    match perf_mode_availability(&PerfModeConfig::default()) {
        PerfModeAvailability::Unsupported(msg) => {
            println!("availability off Windows, as expected: {msg}");
        }
        other => panic!("expected Unsupported off Windows, got {other:?}"),
    }
}

/// On Windows, capability detection must never panic and must return one of the
/// three verdicts. The CI Windows job has no endpoint, so `Unsupported` is the
/// expected path there; a developer machine exercises `Available`/`DriverLocked`.
#[cfg(windows)]
#[test]
fn availability_never_panics_on_windows() {
    match perf_mode_availability(&PerfModeConfig::default()) {
        PerfModeAvailability::Available { info } => {
            println!("availability: Available {info:?}");
            assert!(
                info.min_period_frames < info.default_period_frames,
                "Available implies a minimum period below the default"
            );
        }
        PerfModeAvailability::DriverLocked { info } => {
            println!("availability: DriverLocked {info:?}");
            assert_eq!(
                info.min_period_frames, info.default_period_frames,
                "DriverLocked implies min == default"
            );
        }
        PerfModeAvailability::Unsupported(msg) => {
            println!("availability: Unsupported (expected on a headless CI runner): {msg}");
        }
    }
}

/// Requesting perf mode must never break the engine, even on a non-cpal
/// backend: the perf evaluation queries the OS default render endpoint, not the
/// backend, so a synthetic-backed engine still starts and simply reports a
/// non-`Off` perf state. On a machine with no fast endpoint (every CI runner,
/// and every non-Windows build) that state is `Unavailable`; a Windows box with
/// a sub-default endpoint may report `Active`. `Off` — the not-requested state —
/// must never appear here.
#[test]
fn engine_reports_perf_state_with_synthetic_backend() {
    let config = EngineConfig {
        perf_mode: true,
        // Keep the watcher out so the state is not re-evaluated under us.
        route_watch: false,
        ..EngineConfig::default()
    };
    let (engine, _reader) = Engine::start(Box::new(SyntheticBackend::default()), config)
        .expect("engine must start even when perf mode is requested");

    let state = engine.perf_mode_state();
    println!("perf state (synthetic backend, perf_mode=true): {state:?}");
    assert_ne!(
        state,
        PerfModeState::Off,
        "a requested perf mode must not report Off"
    );
    assert!(
        matches!(
            state,
            PerfModeState::Active { .. } | PerfModeState::Unavailable { .. }
        ),
        "perf state must be Active or Unavailable when requested"
    );

    engine.stop();
}
