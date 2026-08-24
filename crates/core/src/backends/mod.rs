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
