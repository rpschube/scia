//! Engine core for scia: the headless pipeline that turns captured system
//! audio into a stream of feature snapshots.
//!
//! The pipeline is a single in-process chain: a capture backend copies
//! interleaved `f32` samples into a wait-free SPSC ring ([`SampleSink`] →
//! [`SampleConsumer`]); a DSP thread drains the ring on a fixed 256-frame hop
//! grid and computes per-hop features; each hop is published to a
//! triple-buffered [`FeatureSnapshot`] bus ([`FeatureWriter`] →
//! [`FeatureReader`]) that consumers read without ever blocking the DSP thread.
//! Alongside the hop-grid features the DSP thread also computes a **display
//! spectrum** ([`SpectrumAnalyzer`]) — the log-spaced, auto-ranged, smoothed
//! bar graph a renderer draws — and folds it into the same snapshot. When
//! capture stalls the grid keeps advancing with synthesized silence. A
//! [`SyntheticBackend`] drives the whole chain with no audio hardware, and
//! [`Engine`] wires a backend to the DSP thread. This crate carries no
//! user-interface dependency of any kind.

// The crate is unsafe-free everywhere except the audited Windows-only
// `backends::wasapi_perf` module, which must call the raw WASAPI COM API. That
// one module opts in with `#[allow(unsafe_code)]`; `deny` (rather than `forbid`)
// makes that scoped opt-in possible while still rejecting any unannounced
// `unsafe` anywhere else in the crate.
#![deny(unsafe_code)]

pub mod backends;
pub mod bands;
pub mod bus;
pub mod capture;
pub mod dsp;
pub mod engine;
pub mod features;
pub mod onset;
pub mod spectrum;
pub mod synthetic;

#[cfg(feature = "capture-cpal")]
pub use backends::cpal::{CpalBackend, DeviceInfo, DeviceKind, DeviceSelector, list_devices};
#[cfg(feature = "perf-mode")]
pub use backends::wasapi_perf::{PerfModeConfig, PerfModeInfo, PerfModeStream};
pub use bands::{BandConfig, BandSplitter};
pub use bus::{FeatureReader, FeatureWriter, feature_bus};
pub use capture::{
    CaptureBackend, CaptureError, CaptureStream, CaptureTarget, RING_FRAMES, SampleConsumer,
    SampleSink, SinkStats, StreamFormat, StreamHealth, sample_ring,
};
pub use dsp::{DspConfig, HopProcessor};
pub use engine::{Engine, EngineConfig, EngineError, EngineStats};
pub use features::{Activity, FEATURE_SCHEMA_VERSION, FeatureSnapshot, SPECTRUM_BINS};
pub use onset::{OnsetConfig, OnsetDetector};
pub use spectrum::{SpectrumAnalyzer, SpectrumConfig};
pub use synthetic::{Pacing, Signal, SyntheticBackend};

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_crate_name() {
        assert_eq!(NAME, "scia-core");
    }
}
