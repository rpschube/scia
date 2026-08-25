//! The DSP stage: a fixed 256-frame hop grid that drains the sample ring,
//! computes per-hop features, and publishes them on the feature bus. When
//! capture stalls the grid keeps advancing with synthesized silence so the
//! render side always has a fresh, real-time snapshot.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bands::{BandConfig, BandSplitter};
use crate::beat::{BeatDebug, BeatTracker};
use crate::bus::FeatureWriter;
use crate::capture::{SampleConsumer, SinkStats, StreamFormat};
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
            if new_format != thread.format {
                // A renegotiated format: rebuild the FFT/analyzer/bands/onset
                // for the new rate, keeping the generation monotonic.
                processor.reformat(new_format.channels, new_format.sample_rate);
                channels = new_format.channels.max(1) as usize;
                needed = thread.config.hop_frames * channels;
                hop_period = Duration::from_secs_f64(
                    thread.config.hop_frames as f64 / f64::from(new_format.sample_rate.max(1)),
                );
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
            while thread.consumer.buffered_samples() >= needed {
                let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
                let timestamp = thread.stats.now_ns();
                let Some(mut snapshot) = processor.process_idle(
                    &mut thread.consumer,
                    thread.format,
                    timestamp,
                    dropped,
                    idle_resume_rms,
                ) else {
                    break;
                };
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
        if thread.consumer.buffered_samples() >= needed {
            let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
            let timestamp = thread.stats.now_ns();
            if let Some(mut snapshot) =
                processor.try_process(&mut thread.consumer, thread.format, timestamp, dropped)
            {
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
