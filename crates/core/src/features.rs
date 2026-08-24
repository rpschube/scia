//! The versioned feature contract: [`FeatureSnapshot`], a plain-old-data
//! record the DSP thread publishes once per hop and every frontend reads. It
//! is `#[repr(C)]` and `Copy` so it can be triple-buffered cheaply and, in a
//! later card, serialised to an `--output` stream unchanged.
//!
//! Many fields are reserved: they are documented as "reserved, 0 in schema 1"
//! and stay zero until the DSP card that computes them lands. Bumping
//! [`FEATURE_SCHEMA_VERSION`] is the signal that the layout changed.

/// Schema version of [`FeatureSnapshot`]. Bumped whenever the layout or field
/// meaning changes so consumers (including a future serialized stream) can
/// detect a mismatch.
pub const FEATURE_SCHEMA_VERSION: u32 = 1;

/// Number of spectrum bins reserved in every snapshot. The FFT card fills up
/// to this many and sets [`FeatureSnapshot::spectrum_len`].
pub const SPECTRUM_BINS: usize = 256;

/// A single hop's worth of analysis, published on the feature bus.
///
/// All timestamps are monotonic nanoseconds since the engine epoch. Fields
/// marked "reserved" are always `0` in schema 1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FeatureSnapshot {
    /// Always equals [`FEATURE_SCHEMA_VERSION`]; identifies the layout.
    pub schema_version: u32,
    /// Monotonically increasing hop counter (+1 per published hop, real or
    /// synthesized). Never resets for the life of the engine.
    pub generation: u64,
    /// When this hop was processed: ns since the engine epoch.
    pub timestamp_ns: u64,
    /// Stream sample rate in Hz.
    pub sample_rate: u32,
    /// Stream channel count (1 or 2).
    pub channels: u16,
    /// `true` when this hop was synthesized because capture delivered no
    /// samples (silence fill during starvation).
    pub starved: bool,
    /// Cumulative frames dropped to ring overflow, as of this hop.
    pub dropped_frames: u64,
    /// Root-mean-square level of the hop over the mono mix. Range `0.0..=1.0`
    /// for in-range audio.
    pub rms: f32,
    /// Peak absolute sample over the hop across all channels. Range
    /// `0.0..=1.0` for in-range audio.
    pub peak: f32,
    /// Momentary loudness (LUFS). Reserved, 0 in schema 1.
    pub lufs_momentary: f32,
    /// Magnitude spectrum bins; only the first [`spectrum_len`] are valid.
    /// Reserved (all 0) in schema 1.
    ///
    /// [`spectrum_len`]: FeatureSnapshot::spectrum_len
    pub spectrum: [f32; SPECTRUM_BINS],
    /// Number of valid entries in [`spectrum`]. `0` until the FFT card lands.
    ///
    /// [`spectrum`]: FeatureSnapshot::spectrum
    pub spectrum_len: u16,
    /// Bass / mid / treble band energies. Reserved (all 0) in schema 1.
    pub bands: [f32; 3],
    /// Spectral flux for this hop. Reserved, 0 in schema 1.
    pub flux: f32,
    /// Onset detected on this hop. Reserved, `false` in schema 1.
    pub onset: bool,
    /// Milliseconds since the last onset. Reserved, 0 in schema 1.
    pub onset_age_ms: f32,
    /// Beat phase in `0.0..1.0`. Reserved, 0 in schema 1.
    pub beat_phase: f32,
    /// Beat-tracker confidence in `0.0..=1.0`. Reserved, 0 in schema 1.
    pub beat_confidence: f32,
    /// Estimated tempo in BPM. Reserved, 0 in schema 1.
    pub tempo_bpm: f32,
    /// Inter-channel correlation in `-1.0..=1.0`. Reserved, 0 in schema 1.
    pub stereo_correlation: f32,
    /// Mid/side energy ratio. Reserved, 0 in schema 1.
    pub mid_side_ratio: f32,
    /// 12-bin chroma vector. Reserved (all 0) in schema 1.
    pub chroma: [f32; 12],
}

impl Default for FeatureSnapshot {
    fn default() -> Self {
        Self {
            schema_version: FEATURE_SCHEMA_VERSION,
            generation: 0,
            timestamp_ns: 0,
            sample_rate: 0,
            channels: 0,
            starved: false,
            dropped_frames: 0,
            rms: 0.0,
            peak: 0.0,
            lufs_momentary: 0.0,
            spectrum: [0.0; SPECTRUM_BINS],
            spectrum_len: 0,
            bands: [0.0; 3],
            flux: 0.0,
            onset: false,
            onset_age_ms: 0.0,
            beat_phase: 0.0,
            beat_confidence: 0.0,
            tempo_bpm: 0.0,
            stereo_correlation: 0.0,
            mid_side_ratio: 0.0,
            chroma: [0.0; 12],
        }
    }
}

// The snapshot is copied on every hop and will be serialized later; keep it
// comfortably small and catch any accidental bloat at compile time.
const _: () = assert!(std::mem::size_of::<FeatureSnapshot>() <= 2048);
