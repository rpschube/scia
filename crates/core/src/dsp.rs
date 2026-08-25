//! The DSP stage: a fixed 256-frame hop grid that drains the sample ring,
//! computes per-hop features, and publishes them on the feature bus. When
//! capture stalls the grid keeps advancing with synthesized silence so the
//! render side always has a fresh, real-time snapshot.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bands::{BandConfig, BandSplitter};
use crate::beat::{BeatDebug, BeatTracker};
use crate::bus::FeatureWriter;
use crate::capture::{PushRecord, SampleConsumer, SinkStats, StreamFormat};
use crate::features::{Activity, FEATURE_SCHEMA_VERSION, FeatureSnapshot};
use crate::onset::{OnsetConfig, OnsetDetector};
use crate::spectrum::{SpectrumAnalyzer, SpectrumConfig};

/// Tuning for the DSP thread.
#[derive(Clone, Copy, Debug)]
pub struct DspConfig {
    /// Frames processed per hop. The whole pipeline is built around 256.
    pub hop_frames: usize,
    /// How long to sleep between ring checks while waiting for a partial hop.
    pub poll_interval: Duration,
    /// How long with no delivery before the hop grid switches to synthesizing
    /// silence.
    pub gap_timeout: Duration,
    /// How long to sleep between ring checks while starved but not yet idle
    /// (the `Active`/`Quiet` starvation window). Longer than `poll_interval`.
    pub starved_poll_interval: Duration,
    /// RMS level (dBFS) a hop must reach to count as signal. Below it (or when
    /// starved) a hop is quiet and feeds the silence state machine. −60 dBFS by
    /// default, matching the display-spectrum and band silence gates.
    pub quiet_threshold_dbfs: f32,
    /// How long the signal must stay quiet before the pipeline reports `Quiet`.
    /// Processing continues at full rate through this window so features decay
    /// smoothly.
    pub quiet_after: Duration,
    /// How long the signal must stay quiet before the pipeline downshifts to
    /// `Idle`. Kept under the 5 s "near-zero idle" budget.
    pub idle_after: Duration,
    /// How long the DSP thread sleeps between wakes once `Idle`. Kept short
    /// enough that resume latency stays within ~100 ms.
    pub idle_poll_interval: Duration,
    /// Display-spectrum tuning (bars, FFT sizes, AGC, smoothing).
    pub spectrum: SpectrumConfig,
    /// Crossover band-split tuning (bass/mid crossovers, averaging).
    pub bands: BandConfig,
    /// Onset-detector tuning (threshold, min IOI, flux normalization).
    pub onset: OnsetConfig,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            hop_frames: 256,
            poll_interval: Duration::from_millis(1),
            gap_timeout: Duration::from_millis(100),
            starved_poll_interval: Duration::from_millis(50),
            quiet_threshold_dbfs: -60.0,
            quiet_after: Duration::from_millis(500),
            idle_after: Duration::from_secs(4),
            idle_poll_interval: Duration::from_millis(50),
            spectrum: SpectrumConfig::default(),
            bands: BandConfig::default(),
            onset: OnsetConfig::default(),
        }
    }
}

/// Hop counters the engine reports.
#[derive(Debug, Default)]
pub(crate) struct DspCounters {
    /// Hops produced from real captured samples.
    pub hops_processed: AtomicU64,
    /// Hops synthesized as silence during starvation.
    pub hops_synthesized: AtomicU64,
    /// Latest display-spectrum AGC gain, stored as the bit pattern of an `f32`
    /// (the snapshot schema is frozen, so the gain rides here instead).
    pub agc_gain_bits: AtomicU32,
    /// Count of DSP-loop iterations (every iteration that did work or slept).
    /// While `Active` this climbs at the polling rate; once `Idle` it climbs at
    /// the idle poll rate, which is how the downshift is observed without a CPU
    /// meter.
    pub dsp_wakes: AtomicU64,
    /// Latest activity state, stored as [`Activity`]'s `u8` discriminant.
    pub activity: AtomicU8,
}

/// Owns the preallocated scratch buffers and the hop counter, and turns one
/// hop of interleaved samples into a [`FeatureSnapshot`]. Every method after
/// [`HopProcessor::new`] is allocation-free, which is what makes it the
/// testable seam for the hot path.
pub struct HopProcessor {
    hop_frames: usize,
    channels: usize,
    generation: u64,
    dt_seconds: f32,
    interleaved: Vec<f32>,
    mono: Vec<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
    analyzer: SpectrumAnalyzer,
    spectrum_out: Vec<f32>,
    band_splitter: BandSplitter,
    onset_detector: OnsetDetector,
    beat_tracker: BeatTracker,
    /// Optional diagnostic side channel: when installed (only the engine's DSP
    /// thread does — see [`set_beat_debug_sink`](HopProcessor::set_beat_debug_sink)),
    /// the in-thread beat tracker's [`BeatDebug`] is mirrored here after every
    /// induction pass. `None` on every direct-test/tooling path, which is what
    /// keeps the per-hop no-alloc tests free of any lock.
    beat_debug_sink: Option<Arc<Mutex<BeatDebug>>>,
    /// The induction count last mirrored into `beat_debug_sink`, so the mirror
    /// write happens once per induction pass rather than every hop.
    last_beat_inductions: u64,
    bands_out: [f32; 3],
    flux: f32,
    onset: bool,
    onset_age_ms: f32,
    tempo_bpm: f32,
    beat_phase: f32,
    beat_confidence: f32,
    // The configs the analyzer/bands/onset were built from, kept so a
    // sample-rate/channel change ([`reformat`](HopProcessor::reformat)) can
    // rebuild them exactly as the constructor did.
    spectrum_config: SpectrumConfig,
    bands_config: BandConfig,
    onset_config: OnsetConfig,
}

impl HopProcessor {
    /// Allocate scratch for `hop_frames` frames of `channels`-wide audio,
    /// using the default display-spectrum, band and onset configurations.
    #[must_use]
    pub fn new(hop_frames: usize, channels: u16, sample_rate: u32) -> Self {
        Self::with_configs(
            hop_frames,
            channels,
            sample_rate,
            SpectrumConfig::default(),
            BandConfig::default(),
            OnsetConfig::default(),
        )
    }

    /// Like [`HopProcessor::new`] but with an explicit display-spectrum
    /// configuration (band and onset detectors keep their defaults).
    #[must_use]
    pub fn with_spectrum_config(
        hop_frames: usize,
        channels: u16,
        sample_rate: u32,
        spectrum: SpectrumConfig,
    ) -> Self {
        Self::with_configs(
            hop_frames,
            channels,
            sample_rate,
            spectrum,
            BandConfig::default(),
            OnsetConfig::default(),
        )
    }

    /// Full constructor: spectrum, band-split and onset configs. Allocates every
    /// buffer (including the FFT plans and detector state) once.
    #[must_use]
    pub fn with_configs(
        hop_frames: usize,
        channels: u16,
        sample_rate: u32,
        spectrum: SpectrumConfig,
        bands: BandConfig,
        onset: OnsetConfig,
    ) -> Self {
        let channels = channels.max(1) as usize;
        let analyzer = SpectrumAnalyzer::new(spectrum, sample_rate);
        let bars = analyzer.bars();
        let fft_main = analyzer.config().fft_main;
        let fft_bass = analyzer.config().fft_bass;
        let band_splitter = BandSplitter::new(bands, sample_rate, fft_main, fft_bass);
        let onset_detector = OnsetDetector::new(onset, sample_rate, fft_main);
        let beat_tracker = BeatTracker::new(sample_rate, hop_frames);
        Self {
            hop_frames,
            channels,
            generation: 0,
            dt_seconds: hop_frames as f32 / sample_rate.max(1) as f32,
            interleaved: vec![0.0; hop_frames * channels],
            mono: vec![0.0; hop_frames],
            left: vec![0.0; hop_frames],
            right: vec![0.0; hop_frames],
            analyzer,
            spectrum_out: vec![0.0; bars],
            band_splitter,
            onset_detector,
            beat_tracker,
            beat_debug_sink: None,
            last_beat_inductions: 0,
            bands_out: [0.0; 3],
            flux: 0.0,
            onset: false,
            onset_age_ms: 0.0,
            tempo_bpm: 0.0,
            beat_phase: 0.0,
            beat_confidence: 0.0,
            spectrum_config: spectrum,
            bands_config: bands,
            onset_config: onset,
        }
    }

    /// Rebuild every format-dependent piece for a new stream shape, exactly as
    /// [`with_configs`](HopProcessor::with_configs) would — the FFT plans and
    /// analyzer, the band splitter and the onset detector are recreated for
    /// `channels`/`sample_rate`, and the scratch buffers resized — but the hop
    /// `generation` is kept monotonic so consumers never see the counter jump
    /// back. Used by the DSP thread when a runtime reopen renegotiates the
    /// format (a 44.1 ↔ 48 kHz device switch, say): after this the next
    /// published snapshot carries the new `sample_rate`/`channels` and the
    /// frequency mappings track the new rate. Off the hot path — it allocates,
    /// like the constructor, and runs only on the rare reformat.
    pub fn reformat(&mut self, channels: u16, sample_rate: u32) {
        let channels = channels.max(1) as usize;
        let analyzer = SpectrumAnalyzer::new(self.spectrum_config, sample_rate);
        let bars = analyzer.bars();
        let fft_main = analyzer.config().fft_main;
        let fft_bass = analyzer.config().fft_bass;
        let band_splitter = BandSplitter::new(self.bands_config, sample_rate, fft_main, fft_bass);
        let onset_detector = OnsetDetector::new(self.onset_config, sample_rate, fft_main);
        let beat_tracker = BeatTracker::new(sample_rate, self.hop_frames);

        self.channels = channels;
        self.dt_seconds = self.hop_frames as f32 / sample_rate.max(1) as f32;
        self.interleaved = vec![0.0; self.hop_frames * channels];
        self.mono = vec![0.0; self.hop_frames];
        self.left = vec![0.0; self.hop_frames];
        self.right = vec![0.0; self.hop_frames];
        self.analyzer = analyzer;
        self.spectrum_out = vec![0.0; bars];
        self.band_splitter = band_splitter;
        self.onset_detector = onset_detector;
        self.beat_tracker = beat_tracker;
        // The rebuilt tracker restarts its induction count from zero; keep the
        // mirror cadence in step. The `beat_debug_sink` itself is preserved.
        self.last_beat_inductions = 0;
        self.bands_out = [0.0; 3];
        self.flux = 0.0;
        self.onset = false;
        self.onset_age_ms = 0.0;
        self.tempo_bpm = 0.0;
        self.beat_phase = 0.0;
        self.beat_confidence = 0.0;
        // `generation` deliberately preserved: the grid keeps climbing.
    }

    /// The current display-spectrum AGC gain.
    #[must_use]
    pub fn spectrum_gain(&self) -> f32 {
        self.analyzer.gain()
    }

    /// Instantaneous linear band energies (bass, mid, treble) from the last
    /// processed hop. Unlike the ratio-normalized [`FeatureSnapshot::bands`],
    /// these raw energies directly show which band a signal's power sits in —
    /// handy for an overlay or a test.
    #[must_use]
    pub fn band_levels(&self) -> [f32; 3] {
        self.band_splitter.levels()
    }

    /// Per-band long-term averages (bass, mid, treble) — the reference levels
    /// the ratio normalization divides by.
    #[must_use]
    pub fn band_averages(&self) -> [f32; 3] {
        self.band_splitter.averages()
    }

    /// Current hop generation (last published, or 0 before the first hop).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Pop one hop from `consumer` if a full hop is buffered and turn it into a
    /// non-starved snapshot, incrementing the generation. Returns `None`
    /// (consuming nothing) when the ring holds less than one hop.
    /// Allocation-free after construction.
    pub fn try_process(
        &mut self,
        consumer: &mut SampleConsumer,
        format: StreamFormat,
        timestamp_ns: u64,
        dropped_frames: u64,
    ) -> Option<FeatureSnapshot> {
        let needed = self.hop_frames * self.channels;
        if !consumer.read_hop(needed, &mut self.interleaved) {
            return None;
        }

        let (rms, peak) = self.deinterleave_rms_peak();

        self.analyzer
            .process_hop(&self.mono, self.dt_seconds, &mut self.spectrum_out);
        self.run_bands_and_onset();

        self.generation += 1;
        Some(self.snapshot(format, timestamp_ns, dropped_frames, false, rms, peak))
    }

    /// Deinterleave `self.interleaved` into the mono/left/right scratch buffers
    /// and return the hop's `(rms, peak)`. Cheap: one pass, no FFT. Shared by
    /// the full and idle paths. Allocation-free.
    fn deinterleave_rms_peak(&mut self) -> (f32, f32) {
        let mut sum_sq = 0.0f64;
        let mut peak = 0.0f32;
        for frame in 0..self.hop_frames {
            let base = frame * self.channels;
            let mut acc = 0.0f32;
            for ch in 0..self.channels {
                let sample = self.interleaved[base + ch];
                acc += sample;
                let mag = sample.abs();
                if mag > peak {
                    peak = mag;
                }
            }
            let mono = acc / self.channels as f32;
            self.mono[frame] = mono;
            self.left[frame] = self.interleaved[base];
            self.right[frame] = if self.channels > 1 {
                self.interleaved[base + 1]
            } else {
                self.interleaved[base]
            };
            sum_sq += f64::from(mono) * f64::from(mono);
        }
        let rms = (sum_sq / self.hop_frames as f64).sqrt() as f32;
        (rms, peak)
    }

    /// Feed the freshest spectra into the band splitter and onset detector and
    /// cache their outputs for the next snapshot. Allocation-free.
    fn run_bands_and_onset(&mut self) {
        let mag_main = self.analyzer.mag_main();
        let mag_bass = self.analyzer.mag_bass();
        self.band_splitter
            .process_hop(mag_main, mag_bass, self.dt_seconds, &mut self.bands_out);
        let (flux, onset) = self.onset_detector.process_hop(mag_main, self.dt_seconds);
        self.flux = flux;
        self.onset = onset;
        self.onset_age_ms = self.onset_detector.onset_age_ms();
        self.update_beat();
    }

    /// Install the diagnostic beat-debug side channel. After this, every
    /// induction pass mirrors the in-thread tracker's [`BeatDebug`] into `sink`
    /// (via a non-blocking `try_lock`), so a probe can read the *real* tracker's
    /// internals through [`Engine::beat_debug`](crate::Engine::beat_debug) rather
    /// than a separate mirror tracker. Diagnostic-only: it never feeds back into
    /// tracking. Left uninstalled on every direct-test path, so the per-hop
    /// no-alloc tests never touch a lock.
    pub fn set_beat_debug_sink(&mut self, sink: Arc<Mutex<BeatDebug>>) {
        self.beat_debug_sink = Some(sink);
    }

    /// Feed this hop's onset detection function (the normalized spectral flux)
    /// into the causal beat tracker and cache its tempo/phase/confidence for the
    /// next snapshot. Called exactly once per hop on every publish path.
    /// Allocation-free.
    fn update_beat(&mut self) {
        let est = self.beat_tracker.process_hop(self.flux);
        self.tempo_bpm = est.tempo_bpm;
        self.beat_phase = est.phase;
        self.beat_confidence = est.confidence;
        self.mirror_beat_debug();
    }

    /// Mirror the beat tracker's [`BeatDebug`] into the diagnostic side channel,
    /// but only when a fresh induction pass has landed (≈ every
    /// `INDUCTION_SECONDS`), never on the per-hop path between passes. The read is
    /// an allocation-free `Copy`; the write is a non-blocking `try_lock` that
    /// skips on contention so a reader can never stall the DSP thread. When no
    /// sink is installed (every direct-test path) this returns before touching
    /// anything, keeping the per-hop and induction no-alloc tests lock-free.
    fn mirror_beat_debug(&mut self) {
        let Some(sink) = &self.beat_debug_sink else {
            return;
        };
        let dbg = self.beat_tracker.debug_stats();
        if dbg.inductions == self.last_beat_inductions {
            return;
        }
        self.last_beat_inductions = dbg.inductions;
        if let Ok(mut cell) = sink.try_lock() {
            *cell = dbg;
        }
    }

    /// Emit a silent hop (rms/peak 0, `starved = true`), incrementing the
    /// generation so the hop grid keeps advancing while capture is stalled.
    /// Allocation-free.
    pub fn synthesize_silence(
        &mut self,
        format: StreamFormat,
        timestamp_ns: u64,
        dropped_frames: u64,
    ) -> FeatureSnapshot {
        for value in &mut self.mono {
            *value = 0.0;
        }
        // Still run the analyzer on the silent hop so the bars decay with the
        // release time constant instead of snapping to zero.
        self.analyzer
            .process_hop(&self.mono, self.dt_seconds, &mut self.spectrum_out);
        // Run the band splitter and onset detector on the silent spectra too, so
        // their averages relax and the onset-age clock keeps advancing while
        // capture is stalled.
        self.run_bands_and_onset();
        self.generation += 1;
        self.snapshot(format, timestamp_ns, dropped_frames, true, 0.0, 0.0)
    }

    /// Advance every cached feature by one hop of silence on the **cheap** path:
    /// decay the spectrum bars and onset peak with their release constants, drop
    /// the band levels to zero, and grow the onset-age clock — all without
    /// running either FFT. Produces the same decayed features
    /// [`synthesize_silence`](Self::synthesize_silence) would, for a few
    /// arithmetic operations instead of two FFTs. Allocation-free.
    fn relax_features(&mut self) {
        self.analyzer.relax(self.dt_seconds, &mut self.spectrum_out);
        self.band_splitter.relax(&mut self.bands_out);
        let (flux, onset) = self.onset_detector.relax(self.dt_seconds);
        self.flux = flux;
        self.onset = onset;
        self.onset_age_ms = self.onset_detector.onset_age_ms();
        self.update_beat();
    }

    /// Idle-path counterpart to [`try_process`](Self::try_process): pop one
    /// buffered hop and turn it into a snapshot the cheap way. Its rms/peak are
    /// still measured, but as long as the hop stays below `resume_rms` both FFTs
    /// are skipped and the features decay via [`relax_features`]. The one hop
    /// that crosses `resume_rms` — playback resuming — is run through the full
    /// path so the display reanimates immediately; the caller detects the resume
    /// by the returned snapshot's `rms >= resume_rms`. Returns `None` (consuming
    /// nothing) when the ring holds less than one hop. Allocation-free.
    ///
    /// [`relax_features`]: Self::relax_features
    pub fn process_idle(
        &mut self,
        consumer: &mut SampleConsumer,
        format: StreamFormat,
        timestamp_ns: u64,
        dropped_frames: u64,
        resume_rms: f32,
    ) -> Option<FeatureSnapshot> {
        let needed = self.hop_frames * self.channels;
        if !consumer.read_hop(needed, &mut self.interleaved) {
            return None;
        }

        let (rms, peak) = self.deinterleave_rms_peak();
        self.generation += 1;
        if rms >= resume_rms {
            // Resume: the cheap path cannot reconstruct a live spectrum, so run
            // the full analysis on this one hop.
            self.analyzer
                .process_hop(&self.mono, self.dt_seconds, &mut self.spectrum_out);
            self.run_bands_and_onset();
        } else {
            self.relax_features();
        }
        Some(self.snapshot(format, timestamp_ns, dropped_frames, false, rms, peak))
    }

    /// Idle-path counterpart to
    /// [`synthesize_silence`](Self::synthesize_silence): emit a starved silent
    /// hop the cheap way (decayed features, no FFT), incrementing the generation
    /// so the grid keeps advancing while capture is stalled. Allocation-free.
    pub fn synthesize_idle(
        &mut self,
        format: StreamFormat,
        timestamp_ns: u64,
        dropped_frames: u64,
    ) -> FeatureSnapshot {
        self.relax_features();
        self.generation += 1;
        self.snapshot(format, timestamp_ns, dropped_frames, true, 0.0, 0.0)
    }

    fn snapshot(
        &self,
        format: StreamFormat,
        timestamp_ns: u64,
        dropped_frames: u64,
        starved: bool,
        rms: f32,
        peak: f32,
    ) -> FeatureSnapshot {
        let mut snapshot = FeatureSnapshot {
            schema_version: FEATURE_SCHEMA_VERSION,
            generation: self.generation,
            timestamp_ns,
            sample_rate: format.sample_rate,
            channels: format.channels,
            starved,
            dropped_frames,
            rms,
            peak,
            ..FeatureSnapshot::default()
        };
        let bars = self.analyzer.bars();
        snapshot.spectrum[..bars].copy_from_slice(&self.spectrum_out[..bars]);
        snapshot.spectrum_len = bars as u16;
        snapshot.bands = self.bands_out;
        snapshot.flux = self.flux;
        snapshot.onset = self.onset;
        snapshot.onset_age_ms = self.onset_age_ms;
        snapshot.tempo_bpm = self.tempo_bpm;
        snapshot.beat_phase = self.beat_phase;
        snapshot.beat_confidence = self.beat_confidence;
        snapshot
    }
}

/// A one-slot hand-off from the engine to the running DSP thread, used to swap
/// the sample ring under the thread without stopping it. On a runtime reopen the
/// engine opens a fresh stream, then [`publish`](RingSwap::publish)es the new
/// [`SampleConsumer`] and its (possibly renegotiated) [`StreamFormat`] here; the
/// DSP thread picks it up at the top of its next wake with
/// [`try_take`](RingSwap::try_take).
///
/// The engine side is a cold path and may block briefly on the mutex; the DSP
/// side only ever `try_lock`s, so lock contention just means "retry next wake"
/// and never stalls the hop grid. The DSP thread is not the capture callback, so
/// this lock never sits on the wait-free capture path.
pub(crate) struct RingSwap(Mutex<Option<(SampleConsumer, StreamFormat)>>);

impl RingSwap {
    /// An empty swap slot.
    pub(crate) fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Engine side: hand a fresh consumer and format to the DSP thread. Replaces
    /// any pending swap the thread has not yet taken (only the newest matters).
    pub(crate) fn publish(&self, consumer: SampleConsumer, format: StreamFormat) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some((consumer, format));
    }

    /// DSP side: take a pending swap if one is present and the lock is free.
    /// Never blocks — on contention it returns `None` and the thread retries on
    /// its next wake. Allocation-free.
    fn try_take(&self) -> Option<(SampleConsumer, StreamFormat)> {
        match self.0.try_lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        }
    }
}

/// Everything the DSP thread owns for its run.
pub(crate) struct DspThread {
    pub consumer: SampleConsumer,
    pub format: StreamFormat,
    pub writer: FeatureWriter,
    pub config: DspConfig,
    pub stop: Arc<AtomicBool>,
    pub stats: Arc<SinkStats>,
    pub counters: Arc<DspCounters>,
    /// Runtime ring/format hand-off from the engine (see [`RingSwap`]).
    pub swap: Arc<RingSwap>,
    /// Diagnostic side channel: the in-thread beat tracker's latest
    /// [`BeatDebug`], refreshed once per induction pass. Read by
    /// [`Engine::beat_debug`](crate::Engine::beat_debug); never on the hop path.
    pub beat_debug: Arc<Mutex<BeatDebug>>,
    /// The fullscreen-pause control (US-PERF-3): while set, the loop forces its
    /// *existing* idle downshift regardless of the audio level, so a game's own
    /// sound cannot keep the grid at full rate. Driven by the
    /// [`FullscreenWatch`](crate::fullscreen::FullscreenWatch); exposed to the
    /// engine as [`Engine::pause_flag`](crate::Engine::pause_flag). Defaults to
    /// an always-clear flag, so a paused-unaware caller behaves exactly as before.
    pub pause: Arc<AtomicBool>,
}

/// Safety cap on silent hops emitted per starved wake, so a long starved sleep
/// (e.g. after the idle-downshift card lengthens `starved_poll_interval`)
/// cannot produce an unbounded catch-up burst.
const MAX_SILENT_BURST: usize = 512;

/// Classify the current activity from how long the signal has been quiet.
fn classify(quiet: Duration, quiet_after: Duration, idle_after: Duration) -> Activity {
    if quiet >= idle_after {
        Activity::Idle
    } else if quiet >= quiet_after {
        Activity::Quiet
    } else {
        Activity::Active
    }
}

/// Stamp the silence-state fields on a snapshot the DSP loop is about to publish.
fn stamp(snapshot: &mut FeatureSnapshot, activity: Activity, quiet_ms: f32) {
    snapshot.activity = activity;
    snapshot.quiet_ms = quiet_ms;
}

/// The DSP loop. Runs on the named `scia-dsp` thread until the stop flag is set.
///
/// It carries a three-state silence machine ([`Activity`]). While `Active` or
/// `Quiet` it processes at the full hop rate (real hops, or — when starved —
/// real-time silence fill) so features decay smoothly through their release
/// constants. Once the signal has been quiet for `idle_after` it downshifts to
/// `Idle`: it wakes only every `idle_poll_interval`, drains all buffered hops on
/// the cheap FFT-free path, and synthesizes starved hops the cheap way — a few
/// wakes per second doing trivial arithmetic, so idle CPU is near zero. The
/// first hop whose RMS crosses the quiet threshold snaps it straight back to
/// `Active` and reanimates the display. Every publish path is allocation-free.
pub(crate) fn run(mut thread: DspThread) {
    // TODO(priority): raise the scheduling priority of the scia-dsp thread once
    // the priority-tuning card lands so capture jitter cannot starve DSP.
    // Format-derived; recomputed if a runtime reopen renegotiates the format.
    let mut channels = thread.format.channels.max(1) as usize;
    let mut needed = thread.config.hop_frames * channels;
    let mut hop_period = Duration::from_secs_f64(
        thread.config.hop_frames as f64 / f64::from(thread.format.sample_rate.max(1)),
    );
    // Nanoseconds per captured frame, for the delivery-anchored publish clock
    // (see `hop_delivery_ns`). Recomputed if a runtime reopen renegotiates the rate.
    let mut ns_per_frame = 1.0e9 / f64::from(thread.format.sample_rate.max(1));
    let gap_ns = thread.config.gap_timeout.as_nanos() as u64;
    let quiet_after = thread.config.quiet_after;
    let idle_after = thread.config.idle_after;
    // Linear-RMS form of the quiet threshold; a hop at or above it is signal.
    let resume_rms = 10f32.powf(thread.config.quiet_threshold_dbfs / 20.0);

    let mut processor = HopProcessor::with_configs(
        thread.config.hop_frames,
        thread.format.channels,
        thread.format.sample_rate,
        thread.config.spectrum,
        thread.config.bands,
        thread.config.onset,
    );
    // Wire the diagnostic beat-debug mirror: the induction pass writes the
    // tracker's stats into the shared cell the engine exposes. Off the hop path.
    processor.set_beat_debug_sink(Arc::clone(&thread.beat_debug));
    // Maps each real hop's newest frame to the exact delivery time of the push that
    // carried it (see [`DeliveryMap`]), replacing the uniform `last_push_ns −
    // occupancy` inference that read early under a real backend's bursty delivery.
    let mut delivery = DeliveryMap::new();
    let hop_frames = thread.config.hop_frames as u64;
    let mut silent_deadline: Option<Instant> = None;
    // The instant of the last hop that carried signal. Everything since is quiet;
    // its age drives the state machine and `quiet_ms`. Seeded to "now" so a
    // pipeline that never sees audio idles after `idle_after`.
    let mut last_non_quiet = Instant::now();

    loop {
        if thread.stop.load(Ordering::Acquire) {
            break;
        }
        // Every iteration is a wake, whether it processed hops or only slept.
        thread.counters.dsp_wakes.fetch_add(1, Ordering::Relaxed);

        // Adopt a pending ring swap before anything else this wake. `try_take`
        // never blocks: on contention we retry next wake. The old consumer is
        // dropped here; its samples are gone, but a reopen only ever swaps in a
        // fresh ring when the old stream was being replaced anyway. The DSP
        // thread's silence machine (`last_non_quiet`, `silent_deadline`) is left
        // untouched, so the activity state carries across the swap and the
        // reopen window renders as a short starved quiet, never a freeze.
        if let Some((new_consumer, new_format)) = thread.swap.try_take() {
            thread.consumer = new_consumer;
            // The swapped-in consumer is a fresh ring with a fresh delivery log,
            // both numbered from frame 0; rewind the map to match.
            delivery.reset();
            if new_format != thread.format {
                // A renegotiated format: rebuild the FFT/analyzer/bands/onset
                // for the new rate, keeping the generation monotonic.
                processor.reformat(new_format.channels, new_format.sample_rate);
                channels = new_format.channels.max(1) as usize;
                needed = thread.config.hop_frames * channels;
                hop_period = Duration::from_secs_f64(
                    thread.config.hop_frames as f64 / f64::from(new_format.sample_rate.max(1)),
                );
                ns_per_frame = 1.0e9 / f64::from(new_format.sample_rate.max(1));
            }
            thread.format = new_format;
        }

        // Fullscreen pause (US-PERF-3): while a fullscreen-exclusive app is
        // foreground, force the *existing* idle downshift regardless of the audio
        // level — a game's own sound must not keep the grid at full rate. This
        // reuses the idle branch below rather than adding a second throttle; the
        // pause lifting (not a loud hop) is what resumes.
        let paused = thread.pause.load(Ordering::Acquire);
        let mode = if paused {
            Activity::Idle
        } else {
            classify(last_non_quiet.elapsed(), quiet_after, idle_after)
        };

        if mode == Activity::Idle {
            // ---- Idle downshift: drain everything cheaply, then sleep long. ----
            // While paused, raise the resume threshold out of reach so a loud hop
            // is never mistaken for a resume: `process_idle` then always takes its
            // FFT-free relax path, holding paused CPU near zero even with audio
            // flowing, and the drain never flips back to full rate until the pause
            // is lifted.
            let idle_resume_rms = if paused { f32::INFINITY } else { resume_rms };
            let mut resumed = false;
            loop {
                let buffered = thread.consumer.buffered_samples();
                if buffered < needed {
                    break;
                }
                let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
                // Delivery-anchored publish clock, as in the Active branch: stamp
                // the hop's newest frame with the delivery time of the push that
                // actually carried it (exact per-push mapping, see `DeliveryMap`).
                let now = thread.stats.now_ns();
                delivery.ingest(&thread.consumer);
                let timestamp = delivery.hop_stamp(hop_frames, ns_per_frame, now);
                let Some(mut snapshot) = processor.process_idle(
                    &mut thread.consumer,
                    thread.format,
                    timestamp,
                    dropped,
                    idle_resume_rms,
                ) else {
                    break;
                };
                delivery.advance(hop_frames);
                if snapshot.rms >= idle_resume_rms && !snapshot.starved {
                    // Playback resumed: this hop went through the full path.
                    last_non_quiet = Instant::now();
                    stamp(&mut snapshot, Activity::Active, 0.0);
                    resumed = true;
                } else {
                    let quiet = last_non_quiet.elapsed();
                    // While paused the signal may be loud, so `classify` would
                    // return Active from the recent `last_non_quiet`; force Idle
                    // so the fullscreen pause actually reads as the downshift it
                    // is. Unpaused, the normal classification stands.
                    let activity = if paused {
                        Activity::Idle
                    } else {
                        classify(quiet, quiet_after, idle_after)
                    };
                    stamp(&mut snapshot, activity, quiet.as_secs_f32() * 1000.0);
                }
                thread
                    .counters
                    .hops_processed
                    .fetch_add(1, Ordering::Relaxed);
                thread
                    .counters
                    .agc_gain_bits
                    .store(processor.spectrum_gain().to_bits(), Ordering::Relaxed);
                thread
                    .counters
                    .activity
                    .store(snapshot.activity as u8, Ordering::Relaxed);
                thread.writer.publish(snapshot);
                if resumed {
                    break;
                }
            }
            if resumed {
                // Back to full-rate handling on the next iteration, immediately.
                silent_deadline = None;
                continue;
            }

            // Nothing delivered? Keep the grid alive cheaply if starved.
            if is_starving(&thread.stats, gap_ns) {
                let now = Instant::now();
                let mut deadline = silent_deadline.unwrap_or(now);
                let mut burst = 0;
                while now >= deadline && burst < MAX_SILENT_BURST {
                    let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
                    let timestamp = thread.stats.now_ns();
                    let mut snapshot = processor.synthesize_idle(thread.format, timestamp, dropped);
                    let quiet = last_non_quiet.elapsed();
                    stamp(&mut snapshot, Activity::Idle, quiet.as_secs_f32() * 1000.0);
                    thread
                        .counters
                        .hops_synthesized
                        .fetch_add(1, Ordering::Relaxed);
                    thread
                        .counters
                        .activity
                        .store(Activity::Idle as u8, Ordering::Relaxed);
                    thread.writer.publish(snapshot);
                    deadline += hop_period;
                    burst += 1;
                }
                if deadline <= now {
                    deadline = now + hop_period;
                }
                silent_deadline = Some(deadline);
            } else {
                thread
                    .counters
                    .activity
                    .store(Activity::Idle as u8, Ordering::Relaxed);
            }
            std::thread::sleep(thread.config.idle_poll_interval);
            continue;
        }

        // ---- Active / Quiet: full-rate processing. ----
        // Step 1: a full hop is available — drain exactly one and publish.
        let buffered = thread.consumer.buffered_samples();
        if buffered >= needed {
            let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
            // Delivery-anchored publish clock: stamp the hop with the
            // capture-delivery time of its newest frame — the exact delivery time of
            // the push that carried it (see `DeliveryMap`), not the DSP's own
            // processing wall-clock, and not a `last_push_ns − occupancy` inference
            // that reads early when a real backend delivers in faster-than-realtime
            // bursts. This is the same capture-delivery clock the P7 raw-ring/tee
            // mapping anchors on, so a click's `emit → publish` and its
            // `emit → raw-arrival` are measured against one reference by construction.
            let now = thread.stats.now_ns();
            delivery.ingest(&thread.consumer);
            let timestamp = delivery.hop_stamp(hop_frames, ns_per_frame, now);
            if let Some(mut snapshot) =
                processor.try_process(&mut thread.consumer, thread.format, timestamp, dropped)
            {
                delivery.advance(hop_frames);
                let loud = snapshot.rms >= resume_rms && !snapshot.starved;
                if loud {
                    last_non_quiet = Instant::now();
                }
                let quiet = last_non_quiet.elapsed();
                let (activity, quiet_ms) = if loud {
                    (Activity::Active, 0.0)
                } else {
                    (
                        classify(quiet, quiet_after, idle_after),
                        quiet.as_secs_f32() * 1000.0,
                    )
                };
                stamp(&mut snapshot, activity, quiet_ms);
                thread
                    .counters
                    .hops_processed
                    .fetch_add(1, Ordering::Relaxed);
                thread
                    .counters
                    .agc_gain_bits
                    .store(processor.spectrum_gain().to_bits(), Ordering::Relaxed);
                thread
                    .counters
                    .activity
                    .store(activity as u8, Ordering::Relaxed);
                thread.writer.publish(snapshot);
            }
            silent_deadline = None;
            continue;
        }

        // Not enough for a hop: decide between starvation fill and waiting.
        if is_starving(&thread.stats, gap_ns) {
            // Step 2: keep the hop grid alive at real-time pace. Emit whatever
            // silent hops are due since the last wake (a bounded catch-up
            // burst), then poll at the slower starved cadence. Full FFT path so
            // the spectrum decays smoothly through the Active/Quiet window.
            let now = Instant::now();
            let mut deadline = silent_deadline.unwrap_or(now);
            let mut burst = 0;
            while now >= deadline && burst < MAX_SILENT_BURST {
                let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
                let timestamp = thread.stats.now_ns();
                let mut snapshot = processor.synthesize_silence(thread.format, timestamp, dropped);
                let quiet = last_non_quiet.elapsed();
                stamp(
                    &mut snapshot,
                    classify(quiet, quiet_after, idle_after),
                    quiet.as_secs_f32() * 1000.0,
                );
                thread
                    .counters
                    .hops_synthesized
                    .fetch_add(1, Ordering::Relaxed);
                thread
                    .counters
                    .agc_gain_bits
                    .store(processor.spectrum_gain().to_bits(), Ordering::Relaxed);
                thread
                    .counters
                    .activity
                    .store(snapshot.activity as u8, Ordering::Relaxed);
                thread.writer.publish(snapshot);
                deadline += hop_period;
                burst += 1;
            }
            if deadline <= now {
                // Fell further behind than the burst cap allows; drop the
                // backlog rather than accumulate it.
                deadline = now + hop_period;
            }
            silent_deadline = Some(deadline);
            std::thread::sleep(thread.config.starved_poll_interval);
        } else {
            // Step 3: a partial hop is buffered or the gap is still short —
            // wait a little and retry.
            std::thread::sleep(thread.config.poll_interval);
        }
    }
}

/// Maps a hop's newest frame to the capture-delivery time of the push that
/// **actually** delivered it, from the primary ring's per-push delivery log
/// ([`crate::capture::SampleSink::delivery`]) — the exact mapping the P7 dual-tap
/// tee uses, applied to the production hop stamp.
///
/// A snapshot's `timestamp_ns` marks *when its audio was captured*, not when the
/// DSP thread happened to process it. The earlier model inferred that time as
/// `last_push_ns − ring_occupancy × ns_per_frame` — the writer's newest frame
/// entered at `last_push_ns` and the hop's newest frame is the occupancy-after-pop
/// number of frames older. That inference is exact only when the ring's occupancy
/// was delivered at a *uniform* nominal frame rate. A real backend does not deliver
/// uniformly: WASAPI shared-mode loopback under timer coalescing hands over several
/// packets in a faster-than-realtime burst, so the occupancy spans pushes whose
/// wall-clock spacing is shorter than `frames × ns_per_frame`. Spreading the
/// occupancy uniformly back from `last_push_ns` across those bursts then places the
/// hop's newest frame *earlier* than it truly arrived — a constant that, on the
/// field endpoint, ran ≈ 29.4 ms (≈ 1410 frames) and pushed `emit → publish`
/// *below* the raw-ring `emit → raw-arrival`, breaking the subset invariant a
/// sample-enters-before-its-hop-publishes ordering guarantees.
///
/// This map instead consumes the delivery log in lockstep with the frames the DSP
/// pops: [`advance`](DeliveryMap::advance) after each real hop, [`ingest`] before
/// each stamp, and [`hop_newest_delivery`] locates the push covering the hop's
/// newest frame and returns `delivery_ns − (push_newest − frame) × ns_per_frame` —
/// exact per push, immune to the cadence between pushes, so it tracks the same
/// capture-delivery instant the raw-ring/tee mapping does and the subset invariant
/// holds by construction on any backend.
///
/// [`ingest`]: DeliveryMap::ingest
/// [`hop_newest_delivery`]: DeliveryMap::hop_newest_delivery
pub(crate) struct DeliveryMap {
    /// Logged pushes not yet fully consumed, in push order. Each record's
    /// `cumulative_frames` is the running total *through* that push, so the record
    /// covering global frame `g` is the first whose `cumulative_frames > g`.
    records: VecDeque<PushRecord>,
    /// Reused drain buffer, so [`ingest`](DeliveryMap::ingest) never allocates in
    /// steady state.
    scratch: Vec<PushRecord>,
    /// Frames the DSP has popped from the current ring — the base of the next hop.
    consumed: u64,
}

impl DeliveryMap {
    /// An empty map with room reserved for the pushes a ring can hold undrained, so
    /// the steady-state hot path neither allocates nor reallocates.
    fn new() -> Self {
        // The primary ring holds at most `RING_FRAMES`; even implausibly small
        // packets leave far fewer pending pushes than this. Reserving it keeps the
        // queue and drain buffer allocation-free across a run.
        const PENDING_RESERVE: usize = 1024;
        Self {
            records: VecDeque::with_capacity(PENDING_RESERVE),
            scratch: Vec::with_capacity(PENDING_RESERVE),
            consumed: 0,
        }
    }

    /// Reset to a fresh ring: drop every pending record and rewind the frame
    /// cursor. Called when the DSP adopts a swapped-in consumer (a reopen builds a
    /// new ring and a new delivery log that both start at frame 0).
    fn reset(&mut self) {
        self.records.clear();
        self.scratch.clear();
        self.consumed = 0;
    }

    /// Drain any newly-logged pushes into the pending queue. Cheap: one wait-free
    /// log drain plus a move of the new records; allocation-free once the reserve
    /// is warm.
    fn ingest(&mut self, consumer: &SampleConsumer) {
        self.scratch.clear();
        consumer.drain_delivery(&mut self.scratch);
        self.records.extend(self.scratch.drain(..));
    }

    /// The capture-delivery time of the newest frame of the hop about to be popped
    /// (the hop consumes frames `consumed .. consumed + hop_frames`, so its newest
    /// is `consumed + hop_frames − 1`). Returns `None` when no logged push covers
    /// that frame yet — the caller then falls back to the processing clock. In
    /// normal operation the frame is readable only because its push committed, and
    /// the push logged its record before that commit, so a covering record is
    /// present whenever the hop is.
    fn hop_newest_delivery(&self, hop_frames: u64, ns_per_frame: f64) -> Option<u64> {
        let target = self.consumed + hop_frames - 1;
        for rec in &self.records {
            if rec.cumulative_frames > target {
                // `rec` covers frames `cumulative_frames − frames .. cumulative_frames`;
                // its newest is `cumulative_frames − 1`, delivered at `delivery_ns`,
                // and `target` is `(newest − target)` frame-periods older.
                let newest = rec.cumulative_frames - 1;
                let back = ((newest - target) as f64 * ns_per_frame).round() as u64;
                return Some(rec.delivery_ns.saturating_sub(back));
            }
        }
        None
    }

    /// Account for a popped hop: advance the frame cursor and drop pushes now fully
    /// consumed from the front of the queue.
    fn advance(&mut self, hop_frames: u64) {
        self.consumed += hop_frames;
        while let Some(front) = self.records.front() {
            if front.cumulative_frames <= self.consumed {
                self.records.pop_front();
            } else {
                break;
            }
        }
    }

    /// Stamp for the hop about to be popped: its newest frame's exact delivery time
    /// clamped to `≤ now_ns` (a push can land between reads; a capture stamp must
    /// never read into the future), falling back to `now_ns` when no record covers
    /// the frame yet.
    fn hop_stamp(&self, hop_frames: u64, ns_per_frame: f64, now_ns: u64) -> u64 {
        self.hop_newest_delivery(hop_frames, ns_per_frame)
            .map_or(now_ns, |d| d.min(now_ns))
    }
}

/// Whether capture has gone quiet past the gap timeout: either nothing has ever
/// been pushed and the timeout has elapsed since the epoch, or the last push is
/// older than the timeout.
fn is_starving(stats: &SinkStats, gap_ns: u64) -> bool {
    let pushed = stats.pushed_frames.load(Ordering::Relaxed);
    let now_ns = stats.now_ns();
    let last_push = stats.last_push_ns.load(Ordering::Acquire);
    if pushed == 0 {
        now_ns >= gap_ns
    } else {
        now_ns.saturating_sub(last_push) > gap_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPF: f64 = 1.0e9 / 48_000.0;
    const HOP: u64 = 256;
    const HOP_MS: f64 = HOP as f64 * NPF / 1.0e6;

    fn rec(frames: u32, delivery_ns: u64, cum: u64) -> PushRecord {
        PushRecord {
            frames,
            delivery_ns,
            cumulative_frames: cum,
        }
    }

    /// A [`DeliveryMap`] preloaded with `records` and a frame cursor, bypassing the
    /// ring drain so the mapping arithmetic can be exercised directly.
    fn map_with(records: &[PushRecord], consumed: u64) -> DeliveryMap {
        let mut m = DeliveryMap::new();
        m.records.extend(records.iter().copied());
        m.consumed = consumed;
        m
    }

    /// The pre-fix stamp model: spread the ring occupancy uniformly at the nominal
    /// frame rate back from `last_push_ns`. Kept here to prove the reproduction gate
    /// bites — this model breaks the subset invariant on the bursty schedule below;
    /// the [`DeliveryMap`] mapping that replaced it holds.
    fn uniform_inference(last_push_ns: u64, after_frames: u64, now: u64) -> u64 {
        if last_push_ns == 0 {
            return now;
        }
        let back = (after_frames as f64 * NPF).round() as u64;
        last_push_ns.saturating_sub(back).min(now)
    }

    /// Exact delivery time of global frame `g` from the push log — the reference
    /// the tee's raw-arrival mapping computes, used to place a click's leading edge.
    fn exact_delivery(records: &[PushRecord], g: u64) -> u64 {
        for r in records {
            if r.cumulative_frames > g {
                let newest = r.cumulative_frames - 1;
                return r
                    .delivery_ns
                    .saturating_sub(((newest - g) as f64 * NPF).round() as u64);
            }
        }
        records.last().unwrap().delivery_ns
    }

    /// **Reproduction of the P7 round-5 SUBSET-BREAK.** A real backend (WASAPI
    /// shared-mode loopback under Windows timer coalescing) hands the capture path
    /// several packets in a *faster-than-realtime burst*: the wall-clock spacing
    /// between those pushes is far shorter than `frames × ns_per_frame`. The pre-fix
    /// stamp spread the ring occupancy uniformly at the nominal rate back from
    /// `last_push_ns`, crossing the burst, and so placed a hop's newest frame ~29 ms
    /// (≈ 1410 frames) *before* it truly arrived — pushing `emit → publish` below
    /// the raw-ring `emit → raw-arrival`, which a sample-enters-before-its-hop
    /// ordering makes impossible. The exact per-push [`DeliveryMap`] mapping anchors
    /// the frame within the push that carried it and holds the invariant.
    ///
    /// This asserts the old model breaks (publish well below raw) and the new model
    /// holds (`raw ≤ publish ≤ raw + one hop`) on the same schedule — the failing
    /// state on master's model, the passing state after the fix.
    #[test]
    fn bursty_delivery_breaks_uniform_stamp_but_exact_mapping_holds() {
        // Two realtime 480-frame packets, then a coalesced burst of four packets
        // handed over within 0.3 ms at ~200 ms — 40 ms of audio delivered almost
        // instantaneously, the timer-coalescing cadence the synthetic 256-frame
        // realtime backend never exercises.
        let records = [
            rec(480, 100_000_000, 480),  // realtime
            rec(480, 110_000_000, 960),  // realtime
            rec(480, 200_000_000, 1440), // burst packet 0
            rec(480, 200_100_000, 1920), // burst packet 1
            rec(480, 200_200_000, 2400), // burst packet 2
            rec(480, 200_300_000, 2880), // burst packet 3
        ];
        // A click whose leading edge (frame 1400) and hop-newest frame (1439) both
        // sit in burst packet 0, while packets 1–3 (1440 frames) sit newer in the
        // ring — the compressed span the old model over-counts as 30 ms.
        let leading_edge = 1400u64;
        let hop_newest = 1439u64; // newest frame of a 256-hop ending here
        let now = 200_400_000u64; // DSP processes just after the burst lands

        let raw_ns = exact_delivery(&records, leading_edge);

        // New model: exact per-push mapping for the hop ending at `hop_newest`.
        let consumed = hop_newest + 1 - HOP;
        let map = map_with(&records, consumed);
        let publish_new = map.hop_stamp(HOP, NPF, now);

        // Old model: last_push − occupancy×npf. Occupancy after the pop is every
        // frame newer than the hop's newest that is resident (the whole burst tail),
        // anchored on the burst's last delivery.
        let newest_resident = records.last().unwrap().cumulative_frames - 1; // 2879
        let after_frames = newest_resident - hop_newest; // 1440
        let last_push = records.last().unwrap().delivery_ns;
        let publish_old = uniform_inference(last_push, after_frames, now);

        let raw_ms = raw_ns as f64 / 1.0e6;
        let new_ms = publish_new as f64 / 1.0e6;
        let old_ms = publish_old as f64 / 1.0e6;
        let new_delta = new_ms - raw_ms;
        let old_delta = old_ms - raw_ms;

        // The old model breaks the subset invariant grossly: publish reads ~29 ms
        // BELOW raw-arrival (the field's −29.38 ms; here 1440 frames of burst tail).
        assert!(
            old_delta < -20.0,
            "old uniform stamp should read the hop far below raw-arrival \
             (Δ {old_delta:.2} ms) — the SUBSET-BREAK the field hit"
        );
        let predicted = -(after_frames as f64 * NPF / 1.0e6); // −30 ms nominal span
        assert!(
            (old_delta - predicted).abs() < HOP_MS + 1.0,
            "old break Δ {old_delta:.2} ms should match the over-counted burst span \
             {predicted:.2} ms (≈ {after_frames} frames)"
        );

        // The exact mapping holds the invariant: raw ≤ publish ≤ raw + one hop.
        assert!(
            new_delta >= -0.5,
            "exact stamp must not read below raw-arrival (Δ {new_delta:.2} ms)"
        );
        assert!(
            new_delta <= HOP_MS + 0.5,
            "exact stamp must stay within one hop of raw-arrival (Δ {new_delta:.2} ms, \
             hop {HOP_MS:.2} ms)"
        );
    }

    /// On the honest case the fix does not disturb — a uniform realtime cadence
    /// (the synthetic 256-frame backend, or a well-behaved endpoint) maps to the
    /// same stamp the old occupancy inference produced, so the passing synthetic
    /// dual-tap numbers are unchanged.
    #[test]
    fn uniform_delivery_matches_the_occupancy_inference() {
        // Ten realtime 480-frame packets.
        let mut records = Vec::new();
        let mut cum = 0u64;
        for k in 0..10u64 {
            cum += 480;
            records.push(rec(480, (k + 1) * 10_000_000, cum));
        }
        // A hop ending at frame 2559 (inside packet 5), some occupancy still ahead.
        let hop_newest = 2559u64;
        let consumed = hop_newest + 1 - HOP;
        let map = map_with(&records, consumed);
        let now = 100_000_000u64;
        let exact = map.hop_stamp(HOP, NPF, now);

        let newest_resident = cum - 1;
        let after = newest_resident - hop_newest;
        let old = uniform_inference((10) * 10_000_000, after, now);
        assert!(
            (exact as i64 - old as i64).unsigned_abs() <= NPF.ceil() as u64,
            "uniform cadence: exact stamp {exact} and occupancy inference {old} \
             should agree within a frame period"
        );
    }

    /// `advance` drops fully-consumed pushes and the cursor tracks the frames the
    /// DSP has popped, so a later hop maps against the still-resident pushes.
    #[test]
    fn advance_drops_consumed_records_and_moves_the_cursor() {
        let records = [
            rec(256, 10_000_000, 256),
            rec(256, 20_000_000, 512),
            rec(256, 30_000_000, 768),
        ];
        let mut map = map_with(&records, 0);
        // First hop [0,256): newest 255 → packet 0 delivery.
        assert_eq!(map.hop_stamp(HOP, NPF, 40_000_000), 10_000_000);
        map.advance(HOP);
        // Packet 0 is now fully consumed and dropped.
        assert_eq!(map.records.len(), 2);
        // Second hop [256,512): newest 511 → packet 1 delivery.
        assert_eq!(map.hop_stamp(HOP, NPF, 40_000_000), 20_000_000);
        map.advance(HOP);
        assert_eq!(map.records.len(), 1);
    }

    /// With no record covering the hop's newest frame yet (a race the ordering
    /// makes rare), the stamp falls back to the processing clock rather than
    /// inventing a time; and it never reads past `now`.
    #[test]
    fn missing_record_falls_back_to_now_and_never_exceeds_it() {
        // Empty map: nothing logged.
        let map = map_with(&[], 0);
        assert_eq!(map.hop_stamp(HOP, NPF, 5_000), 5_000);
        // A record whose delivery is ahead of the sampled `now` (a push raced in):
        // clamp to now.
        let records = [rec(256, 60_000_000, 256)];
        let map = map_with(&records, 0);
        assert_eq!(
            map.hop_stamp(HOP, NPF, 50_000_000),
            50_000_000,
            "stamp must never exceed now"
        );
    }

    /// `reset` rewinds the map for a reopened ring (fresh log numbered from 0).
    #[test]
    fn reset_clears_records_and_cursor() {
        let records = [rec(256, 10_000_000, 256), rec(256, 20_000_000, 512)];
        let mut map = map_with(&records, 256);
        map.reset();
        assert!(map.records.is_empty());
        assert_eq!(map.consumed, 0);
        // After reset a fresh hop falls back to now (no records yet).
        assert_eq!(map.hop_stamp(HOP, NPF, 7_777), 7_777);
    }
}
