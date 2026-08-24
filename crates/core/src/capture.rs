//! Capture seam: the trait a backend implements, the lock-free sample ring it
//! feeds, and the shared statistics the DSP thread reads back.
//!
//! A backend never touches anything but the [`SampleSink`] it is handed at
//! [`CaptureBackend::open`]. The sink copies interleaved `f32` frames into a
//! wait-free single-producer/single-consumer ring; the DSP thread owns the
//! matching [`SampleConsumer`]. No locks, allocation, or blocking sit on the
//! capture path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Ring capacity in **frames** (~170 ms at 48 kHz). The ring is always sized
/// for two channels regardless of the stream width, so it stores
/// `RING_FRAMES * 2` interleaved `f32` slots.
pub const RING_FRAMES: usize = 32768;

/// Interleaved channel count the ring is dimensioned for. Streams are 1 or 2
/// channels; the ring is sized for the maximum so a stereo stream fits.
const RING_CHANNELS: usize = 2;

/// Total `f32` slots in the sample ring.
const RING_SLOTS: usize = RING_FRAMES * RING_CHANNELS;

/// What to capture. Only the system mix exists today; per-process and
/// per-device targets arrive with the real backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The full system output mix (loopback).
    SystemMix,
}

/// The negotiated shape of a capture stream. Fixed for the lifetime of the
/// stream in this card; renegotiation is a later card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    /// Sample rate in Hz. A stream property (44_100 / 48_000 are routine),
    /// never a compile-time constant.
    pub sample_rate: u32,
    /// Interleaved channel count. `1` (mono) or `2` (stereo); backends downmix
    /// anything wider before pushing.
    pub channels: u16,
}

/// Why a capture backend could not open or run a stream.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No capture device / endpoint was available.
    #[error("no capture device available")]
    NoDevice,
    /// The requested configuration is not supported.
    #[error("unsupported capture configuration: {0}")]
    Unsupported(String),
    /// A backend-specific failure, carrying its own message.
    #[error("capture backend error: {0}")]
    Backend(String),
}

/// Shared, atomics-only statistics written by the capture path and read by the
/// DSP thread and the engine. Cheap to clone (an [`Arc`] wrapper).
///
/// All timestamps are monotonic nanoseconds since the engine epoch — the
/// [`Instant`] the ring was created with.
pub struct SinkStats {
    /// Cumulative frames dropped because the ring was full on push.
    pub dropped_frames: AtomicU64,
    /// Delivery time of the most recent push (ns since the engine epoch);
    /// `0` until the first push. Used for gap / starvation detection.
    pub last_push_ns: AtomicU64,
    /// Cumulative frames successfully written into the ring.
    pub pushed_frames: AtomicU64,
    /// Number of non-empty pushes (capture-callback deliveries) so far. The
    /// probe divides [`pushed_frames`] by this for the mean callback size.
    ///
    /// [`pushed_frames`]: SinkStats::pushed_frames
    pub pushes: AtomicU64,
    /// Frames delivered by the most recent push (whether or not they all fit
    /// in the ring). `0` until the first push.
    pub last_push_frames: AtomicU32,
    /// Largest frame count seen in a single push. `0` until the first push.
    pub max_push_frames: AtomicU32,
    /// Largest interval, in nanoseconds, ever observed between two consecutive
    /// pushes. `0` until at least two pushes have landed. A callback-cadence
    /// metric for the probe; the DSP thread uses [`last_push_ns`] for its own
    /// (independent) starvation check.
    ///
    /// [`last_push_ns`]: SinkStats::last_push_ns
    pub max_gap_ns: AtomicU64,
    /// Interleaved channel count, set by the engine once the backend reports
    /// its format. `0` until then; frame accounting falls back to the ring's
    /// design width (stereo) in that startup window, which is exact for the
    /// stereo synthetic source and self-corrects for real backends.
    channels: AtomicU16,
    /// Monotonic clock origin. Immutable after construction.
    epoch: Instant,
}

impl SinkStats {
    fn new(epoch: Instant) -> Self {
        Self {
            dropped_frames: AtomicU64::new(0),
            last_push_ns: AtomicU64::new(0),
            pushed_frames: AtomicU64::new(0),
            pushes: AtomicU64::new(0),
            last_push_frames: AtomicU32::new(0),
            max_push_frames: AtomicU32::new(0),
            max_gap_ns: AtomicU64::new(0),
            channels: AtomicU16::new(0),
            epoch,
        }
    }

    /// Monotonic nanoseconds elapsed since the engine epoch.
    #[must_use]
    pub fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// Channel count used for frame accounting, falling back to the ring's
    /// design width until the engine records the real format.
    fn accounting_channels(&self) -> usize {
        match self.channels.load(Ordering::Relaxed) {
            0 => RING_CHANNELS,
            c => c as usize,
        }
    }

    /// Record the negotiated channel count (engine-internal).
    pub(crate) fn set_channels(&self, channels: u16) {
        self.channels.store(channels, Ordering::Relaxed);
    }

    /// Test hook: pin the frame-accounting channel count without an engine.
    #[doc(hidden)]
    pub fn set_channels_for_test(&self, channels: u16) {
        self.set_channels(channels);
    }
}

/// The only object a capture callback touches: it copies interleaved samples
/// into the ring, wait-free and allocation-free.
pub struct SampleSink {
    producer: rtrb::Producer<f32>,
    stats: Arc<SinkStats>,
}

impl SampleSink {
    /// Copy interleaved `f32` samples into the ring. Wait-free and
    /// allocation-free. If the ring cannot hold all of them the excess is
    /// dropped and counted as dropped frames. Records the delivery time for
    /// gap detection.
    pub fn push(&mut self, interleaved: &[f32]) {
        // Delivery time first: gap detection must see the attempt even if the
        // ring is full and nothing is written. `swap` hands back the previous
        // delivery time so the callback-cadence gap can be measured without a
        // second clock read.
        let now = self.stats.now_ns();
        let prev_push_ns = self.stats.last_push_ns.swap(now, Ordering::AcqRel);

        if interleaved.is_empty() {
            return;
        }

        let channels = self.stats.accounting_channels();

        // Callback-cadence statistics (probe-facing; independent of the ring's
        // success). Counted per non-empty push, on the delivered frame count.
        let frames_in = (interleaved.len() / channels) as u32;
        self.stats.pushes.fetch_add(1, Ordering::Relaxed);
        self.stats
            .last_push_frames
            .store(frames_in, Ordering::Relaxed);
        self.stats
            .max_push_frames
            .fetch_max(frames_in, Ordering::Relaxed);
        if prev_push_ns != 0 {
            self.stats
                .max_gap_ns
                .fetch_max(now.saturating_sub(prev_push_ns), Ordering::Relaxed);
        }
        let available = self.producer.slots();
        // Never split a frame: writing a partial frame would misalign every
        // channel in the ring for the rest of the stream. Whole frames only;
        // a trailing partial frame in the input is discarded.
        let to_write = (interleaved.len().min(available) / channels) * channels;

        if to_write > 0 {
            // `write_chunk` yields default-initialised slices (f32: Copy +
            // Default), so filling them needs no `unsafe`.
            if let Ok(mut chunk) = self.producer.write_chunk(to_write) {
                let (first, second) = chunk.as_mut_slices();
                let src = &interleaved[..to_write];
                let split = first.len();
                first.copy_from_slice(&src[..split]);
                second.copy_from_slice(&src[split..]);
                chunk.commit_all();
            }
        }

        let pushed = (to_write / channels) as u64;
        let dropped = ((interleaved.len() - to_write) / channels) as u64;
        if pushed > 0 {
            self.stats
                .pushed_frames
                .fetch_add(pushed, Ordering::Relaxed);
        }
        if dropped > 0 {
            self.stats
                .dropped_frames
                .fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// Free `f32` slots currently available for writing. Backends that must not
    /// drop (e.g. a paced synthetic source) can wait on this before pushing.
    #[must_use]
    pub fn free_samples(&self) -> usize {
        self.producer.slots()
    }

    /// Shared statistics for this sink.
    #[must_use]
    pub fn stats(&self) -> &Arc<SinkStats> {
        &self.stats
    }
}

/// The consumer half of the sample ring, owned by the DSP thread.
pub struct SampleConsumer {
    consumer: rtrb::Consumer<f32>,
    stats: Arc<SinkStats>,
}

impl SampleConsumer {
    /// Interleaved `f32` samples currently buffered.
    #[must_use]
    pub fn buffered_samples(&self) -> usize {
        self.consumer.slots()
    }

    /// Buffered frames for a given channel count.
    #[must_use]
    pub fn buffered_frames(&self, channels: u16) -> usize {
        self.consumer.slots() / channels.max(1) as usize
    }

    /// Shared statistics for this ring.
    #[must_use]
    pub fn stats(&self) -> &Arc<SinkStats> {
        &self.stats
    }

    /// Pop exactly `samples` interleaved values into `out`, returning `false`
    /// (and consuming nothing) when fewer than `samples` are buffered.
    /// Allocation-free; `out` must be at least `samples` long.
    pub(crate) fn read_hop(&mut self, samples: usize, out: &mut [f32]) -> bool {
        if self.consumer.slots() < samples {
            return false;
        }
        match self.consumer.read_chunk(samples) {
            Ok(chunk) => {
                let (first, second) = chunk.as_slices();
                out[..first.len()].copy_from_slice(first);
                out[first.len()..first.len() + second.len()].copy_from_slice(second);
                chunk.commit_all();
                true
            }
            Err(_) => false,
        }
    }
}

/// Create the sample ring: a [`SampleSink`] for the backend and a
/// [`SampleConsumer`] for the DSP thread, sharing one [`SinkStats`] with the
/// given monotonic `epoch`. The ring is sized for stereo regardless of the
/// eventual stream width.
#[must_use]
pub fn sample_ring(epoch: Instant) -> (SampleSink, SampleConsumer) {
    let stats = Arc::new(SinkStats::new(epoch));
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(RING_SLOTS);
    (
        SampleSink {
            producer,
            stats: Arc::clone(&stats),
        },
        SampleConsumer { consumer, stats },
    )
}

/// Health of a live capture stream, read back through
/// [`CaptureStream::health`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamHealth {
    /// No stream error has been reported.
    Ok,
    /// The backend's error callback has fired at least once; the payload is the
    /// most recent error message.
    Errored(String),
}

/// A live capture stream. Dropping it stops capture.
pub trait CaptureStream: Send {
    /// The negotiated stream format, fixed for the stream's lifetime.
    fn format(&self) -> StreamFormat;

    /// Whether the stream's error callback has fired. The default returns
    /// [`StreamHealth::Ok`] for backends (e.g. the synthetic source) that
    /// cannot fail asynchronously; real hardware backends override it to
    /// surface the device error that a data callback never sees.
    fn health(&self) -> StreamHealth {
        StreamHealth::Ok
    }
}

/// A capture backend: opens a stream for a target, wiring it to a
/// [`SampleSink`].
pub trait CaptureBackend: Send {
    /// Open a stream on `target`, delivering samples through `sink`.
    ///
    /// # Errors
    /// Returns a [`CaptureError`] if no device is available or the requested
    /// configuration cannot be honoured.
    fn open(
        &mut self,
        target: CaptureTarget,
        sink: SampleSink,
    ) -> Result<Box<dyn CaptureStream>, CaptureError>;
}
