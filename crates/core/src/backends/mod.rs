//! Concrete capture backends.
//!
//! [`convert`] holds the cpal-free sample-format conversion and channel
//! downmix (unit-testable on any host); [`cpal`] holds the real hardware
//! backend and is compiled only with the `capture-cpal` feature (on by
//! default).

pub mod convert;

#[cfg(feature = "capture-cpal")]
pub mod cpal;

/// The Windows-only companion render stream that pulls the endpoint engine
/// period down to its minimum (perf mode). Compiled under the `perf-mode`
/// feature on every platform — a real WASAPI implementation on Windows, a
/// compile-everywhere stub elsewhere.
#[cfg(feature = "perf-mode")]
pub mod wasapi_perf;

/// The Windows-only event-driven route-change notifier (an
/// `IMMNotificationClient` that flips the engine's reopen-request flag).
/// Compiled on every platform — a real implementation on the Windows
/// `route-notify` build, a compile-everywhere stub elsewhere — so the engine
/// can reference it unconditionally and fall back to polling when it is a stub.
pub mod wasapi_route;
