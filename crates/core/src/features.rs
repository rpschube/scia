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

/// Number of display-spectrum bars reserved in every snapshot. The analyzer
/// fills up to this many and sets [`FeatureSnapshot::spectrum_len`].
pub const SPECTRUM_BINS: usize = 256;

/// Coarse activity state of the pipeline, driven by the silence state machine in
/// the DSP thread. It is `#[repr(u8)]` so it rides in the `#[repr(C)]`
/// [`FeatureSnapshot`] without disturbing the layout.
///
/// The states form a monotone ladder as silence persists (`Active` → `Quiet` →
/// `Idle`) and collapse straight back to `Active` the moment signal returns.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Activity {
    /// Signal is present: the DSP thread processes at full hop rate.
    #[default]
    Active = 0,
    /// The signal has been below the quiet threshold (or the capture has been
    /// starved) long enough to count as quiet, but processing continues at full
    /// hop rate so the spectrum, bands and flux decay smoothly with their
    /// release constants.
    Quiet = 1,
    /// Quiet long enough that the DSP thread has downshifted to a low wake rate,
    /// producing decayed snapshots on the cheap idle path. CPU is near zero here.
    Idle = 2,
}

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
    /// Coarse activity state at this hop. `Active` while signal is present,
    /// `Quiet`/`Idle` as silence persists. New in the still-open schema 1
    /// (unreleased until v0.1); default snapshots read `Active`.
    pub activity: Activity,
    /// Milliseconds since the last non-quiet hop; `0.0` while `Active`. Grows
    /// while `Quiet`/`Idle` and resets the moment signal returns. New in the
    /// still-open schema 1 (unreleased until v0.1).
    pub quiet_ms: f32,
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
    /// Display spectrum: log-spaced bars in `0.0..=1.0` spanning the
    /// configured `low_hz..high_hz`, normalized, auto-ranged and smoothed for
    /// rendering. Only the first [`spectrum_len`] entries are valid.
    ///
    /// [`spectrum_len`]: FeatureSnapshot::spectrum_len
    pub spectrum: [f32; SPECTRUM_BINS],
    /// Number of valid bars in [`spectrum`]. `0` on a default snapshot; the DSP
    /// thread sets it to the analyzer's bar count on every published hop.
    ///
    /// [`spectrum`]: FeatureSnapshot::spectrum
    pub spectrum_len: u16,
    /// Bass / mid / treble band levels, each normalized against that band's own
    /// recent long-term average: `1.0` is the average level, `> 1.0` a swell,
    /// `< 1.0` a dip. Clamped to `0.0..=4.0`.
    pub bands: [f32; 3],
    /// Half-wave-rectified spectral flux for this hop, normalized against a slow
    /// peak tracker. Range `0.0..=1.0`.
    pub flux: f32,
    /// `true` when an onset (transient) was detected on this hop.
    pub onset: bool,
    /// Milliseconds since the last onset, counting up from engine start and
    /// saturating at `60_000.0` (the value that means "no recent onset"). Range
    /// `0.0..=60_000.0`.
    pub onset_age_ms: f32,
    /// Beat phase in `0.0..1.0`: position within the current beat period,
    /// wrapping at each predicted beat. `0.0` while the beat tracker is
    /// unlocked. Filled by the causal beat tracker in schema 1.
    pub beat_phase: f32,
    /// Beat-tracker confidence in `0.0..=1.0`: how dominant the locked tempo
    /// hypothesis is. Always published — low on silence, noise and arrhythmic
    /// input, high on steady music. Consumers gate the beat fields on it.
    /// Filled by the causal beat tracker in schema 1.
    pub beat_confidence: f32,
    /// Estimated tempo in BPM: the locked tempo, `0.0` while unlocked. Filled by
    /// the causal beat tracker in schema 1.
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
            activity: Activity::Active,
            quiet_ms: 0.0,
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
