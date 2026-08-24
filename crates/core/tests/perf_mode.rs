//! Perf-mode (`backends::wasapi_perf`) integration tests. Both must run on CI
//! runners that have no audio endpoint: opening either succeeds or reports a
//! device/backend condition, and never panics.

#![cfg(feature = "perf-mode")]

use scia_core::{CaptureError, PerfModeConfig, PerfModeStream};

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
