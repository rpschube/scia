//! The DSP stage: a fixed 256-frame hop grid that drains the sample ring,
//! computes per-hop features, and publishes them on the feature bus. When
//! capture stalls the grid keeps advancing with synthesized silence so the
//! render side always has a fresh, real-time snapshot.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::bus::FeatureWriter;
use crate::capture::{SampleConsumer, SinkStats, StreamFormat};
use crate::features::{FEATURE_SCHEMA_VERSION, FeatureSnapshot};
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
    /// How long to sleep between ring checks while starved. Longer than
    /// `poll_interval`; the idle-downshift card extends this hook further.
    pub starved_poll_interval: Duration,
    /// Display-spectrum tuning (bars, FFT sizes, AGC, smoothing).
    pub spectrum: SpectrumConfig,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            hop_frames: 256,
            poll_interval: Duration::from_millis(1),
            gap_timeout: Duration::from_millis(100),
            starved_poll_interval: Duration::from_millis(50),
            spectrum: SpectrumConfig::default(),
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
}

impl HopProcessor {
    /// Allocate scratch for `hop_frames` frames of `channels`-wide audio,
    /// using the default display-spectrum configuration.
    #[must_use]
    pub fn new(hop_frames: usize, channels: u16, sample_rate: u32) -> Self {
        Self::with_spectrum_config(hop_frames, channels, sample_rate, SpectrumConfig::default())
    }

    /// Like [`HopProcessor::new`] but with an explicit display-spectrum
    /// configuration. Allocates every buffer (including the FFT plans) once.
    #[must_use]
    pub fn with_spectrum_config(
        hop_frames: usize,
        channels: u16,
        sample_rate: u32,
        spectrum: SpectrumConfig,
    ) -> Self {
        let channels = channels.max(1) as usize;
        let analyzer = SpectrumAnalyzer::new(spectrum, sample_rate);
        let bars = analyzer.bars();
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
        }
    }

    /// The current display-spectrum AGC gain.
    #[must_use]
    pub fn spectrum_gain(&self) -> f32 {
        self.analyzer.gain()
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

        self.analyzer
            .process_hop(&self.mono, self.dt_seconds, &mut self.spectrum_out);

        self.generation += 1;
        Some(self.snapshot(format, timestamp_ns, dropped_frames, false, rms, peak))
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
        snapshot
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
}

/// Safety cap on silent hops emitted per starved wake, so a long starved sleep
/// (e.g. after the idle-downshift card lengthens `starved_poll_interval`)
/// cannot produce an unbounded catch-up burst.
const MAX_SILENT_BURST: usize = 512;

/// The DSP loop. Runs on the named `scia-dsp` thread until the stop flag is
/// set. Steps 1–2 (real hop / silence fill) are allocation-free.
pub(crate) fn run(mut thread: DspThread) {
    // TODO(priority): raise the scheduling priority of the scia-dsp thread once
    // the priority-tuning card lands so capture jitter cannot starve DSP.
    let channels = thread.format.channels.max(1) as usize;
    let needed = thread.config.hop_frames * channels;
    let gap_ns = thread.config.gap_timeout.as_nanos() as u64;
    let hop_period = Duration::from_secs_f64(
        thread.config.hop_frames as f64 / f64::from(thread.format.sample_rate.max(1)),
    );

    let mut processor = HopProcessor::with_spectrum_config(
        thread.config.hop_frames,
        thread.format.channels,
        thread.format.sample_rate,
        thread.config.spectrum,
    );
    let mut silent_deadline: Option<Instant> = None;

    loop {
        if thread.stop.load(Ordering::Acquire) {
            break;
        }

        // Step 1: a full hop is available — drain exactly one and publish.
        if thread.consumer.buffered_samples() >= needed {
            let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
            let timestamp = thread.stats.now_ns();
            if let Some(snapshot) =
                processor.try_process(&mut thread.consumer, thread.format, timestamp, dropped)
            {
                thread.writer.publish(snapshot);
                thread
                    .counters
                    .hops_processed
                    .fetch_add(1, Ordering::Relaxed);
                thread
                    .counters
                    .agc_gain_bits
                    .store(processor.spectrum_gain().to_bits(), Ordering::Relaxed);
            }
            silent_deadline = None;
            continue;
        }

        // Not enough for a hop: decide between starvation fill and waiting.
        let pushed = thread.stats.pushed_frames.load(Ordering::Relaxed);
        let now_ns = thread.stats.now_ns();
        let last_push = thread.stats.last_push_ns.load(Ordering::Acquire);
        let starving = if pushed == 0 {
            now_ns >= gap_ns
        } else {
            now_ns.saturating_sub(last_push) > gap_ns
        };

        if starving {
            // Step 2: keep the hop grid alive at real-time pace. Emit whatever
            // silent hops are due since the last wake (a bounded catch-up
            // burst), then poll at the slower starved cadence.
            let now = Instant::now();
            let mut deadline = silent_deadline.unwrap_or(now);
            let mut burst = 0;
            while now >= deadline && burst < MAX_SILENT_BURST {
                let dropped = thread.stats.dropped_frames.load(Ordering::Relaxed);
                let timestamp = thread.stats.now_ns();
                let snapshot = processor.synthesize_silence(thread.format, timestamp, dropped);
                thread.writer.publish(snapshot);
                thread
                    .counters
                    .hops_synthesized
                    .fetch_add(1, Ordering::Relaxed);
                thread
                    .counters
                    .agc_gain_bits
                    .store(processor.spectrum_gain().to_bits(), Ordering::Relaxed);
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
