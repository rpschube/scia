//! Concrete capture backends.
//!
//! [`convert`] holds the cpal-free sample-format conversion and channel
//! downmix (unit-testable on any host); [`cpal`] holds the real hardware
//! backend and is compiled only with the `capture-cpal` feature (on by
//! default).

pub mod convert;

#[cfg(feature = "capture-cpal")]
pub mod cpal;
