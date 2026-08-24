//! The engine wires a capture backend to the DSP thread and hands the caller a
//! [`FeatureReader`]. It owns the capture stream, the DSP thread, and the
//! shared statistics; stopping (or dropping) it tears the pipeline down.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::bus::{FeatureReader, feature_bus};
use crate::capture::{
    CaptureBackend, CaptureError, CaptureStream, CaptureTarget, SinkStats, StreamFormat,
    StreamHealth, sample_ring,
};
use crate::dsp::{DspConfig, DspCounters, DspThread};
use crate::features::Activity;

/// Configuration for [`Engine::start`].
#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    /// What to capture.
    pub target: CaptureTarget,
    /// DSP thread tuning.
    pub dsp: DspConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::SystemMix,
            dsp: DspConfig::default(),
        }
    }
}

/// A snapshot of pipeline counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EngineStats {
    /// Cumulative frames dropped to ring overflow.
    pub dropped_frames: u64,
    /// Cumulative frames written into the ring by the backend.
    pub pushed_frames: u64,
    /// Hops produced from real captured samples.
    pub hops_processed: u64,
    /// Hops synthesized as silence during starvation.
    pub hops_synthesized: u64,
    /// Latest display-spectrum AGC gain (1.0 with autosens off, or before the
    /// first hop).
    pub agc_gain: f32,
    /// Number of non-empty capture-callback pushes so far.
    pub pushes: u64,
    /// Frames delivered by the most recent push.
    pub last_push_frames: u32,
    /// Largest frame count seen in a single push.
    pub max_push_frames: u32,
    /// Largest interval between two consecutive pushes, in milliseconds.
    pub max_gap_ms: f32,
    /// Latest activity state of the silence state machine.
    pub activity: Activity,
    /// Count of DSP-loop iterations so far. Climbs at the polling rate while
    /// `Active`, and at the (much lower) idle poll rate once `Idle` — the
    /// meter-free way to observe the downshift.
    pub dsp_wakes: u64,
}

/// Why the engine could not start.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The capture backend failed to open.
    #[error(transparent)]
    Capture(#[from] CaptureError),
    /// The DSP thread could not be spawned.
    #[error("failed to spawn DSP thread: {0}")]
    Spawn(String),
}

/// A running pipeline: capture → ring → DSP → feature bus.
pub struct Engine {
    stream: Option<Box<dyn CaptureStream>>,
    format: StreamFormat,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    stats: Arc<SinkStats>,
    counters: Arc<DspCounters>,
}

impl Engine {
    /// Open `backend`, start the DSP thread, and return the engine together
    /// with a [`FeatureReader`] on the freshest snapshot.
    ///
    /// # Errors
    /// Returns [`EngineError::Capture`] if the backend cannot open, or
    /// [`EngineError::Spawn`] if the DSP thread cannot be created.
    pub fn start(
        mut backend: Box<dyn CaptureBackend>,
        config: EngineConfig,
    ) -> Result<(Engine, FeatureReader), EngineError> {
        let epoch = Instant::now();
        let (sink, consumer) = sample_ring(epoch);
        let stats = Arc::clone(sink.stats());

        let stream = backend.open(config.target, sink)?;
        let format = stream.format();
        stats.set_channels(format.channels);

        let (writer, reader) = feature_bus();
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(DspCounters::default());

        let dsp = DspThread {
            consumer,
            format,
            writer,
            config: config.dsp,
            stop: Arc::clone(&stop),
            stats: Arc::clone(&stats),
            counters: Arc::clone(&counters),
        };

        let join = thread::Builder::new()
            .name("scia-dsp".into())
            .spawn(move || crate::dsp::run(dsp))
            .map_err(|e| EngineError::Spawn(e.to_string()))?;

        Ok((
            Engine {
                stream: Some(stream),
                format,
                stop,
                join: Some(join),
                stats,
                counters,
            },
            reader,
        ))
    }

    /// The negotiated stream format.
    #[must_use]
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// Current pipeline counters.
    #[must_use]
    pub fn stats(&self) -> EngineStats {
        EngineStats {
            dropped_frames: self.stats.dropped_frames.load(Ordering::Relaxed),
            pushed_frames: self.stats.pushed_frames.load(Ordering::Relaxed),
            hops_processed: self.counters.hops_processed.load(Ordering::Relaxed),
            hops_synthesized: self.counters.hops_synthesized.load(Ordering::Relaxed),
            agc_gain: f32::from_bits(self.counters.agc_gain_bits.load(Ordering::Relaxed)),
            pushes: self.stats.pushes.load(Ordering::Relaxed),
            last_push_frames: self.stats.last_push_frames.load(Ordering::Relaxed),
            max_push_frames: self.stats.max_push_frames.load(Ordering::Relaxed),
            max_gap_ms: self.stats.max_gap_ns.load(Ordering::Relaxed) as f32 / 1.0e6,
            activity: match self.counters.activity.load(Ordering::Relaxed) {
                1 => Activity::Quiet,
                2 => Activity::Idle,
                _ => Activity::Active,
            },
            dsp_wakes: self.counters.dsp_wakes.load(Ordering::Relaxed),
        }
    }

    /// The capture stream's health: [`StreamHealth::Errored`] once the
    /// backend's error callback has fired, otherwise [`StreamHealth::Ok`].
    #[must_use]
    pub fn health(&self) -> StreamHealth {
        self.stream
            .as_ref()
            .map_or(StreamHealth::Ok, |s| s.health())
    }

    /// Stop the DSP thread, stop capture, and join everything.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // Dropping the stream stops the capture thread and joins it.
        self.stream.take();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}
