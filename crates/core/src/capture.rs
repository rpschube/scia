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

    /// Record the negotiated channel count. The engine calls this once a backend
    /// reports its format; a probe assembling a capture ring outside the engine
    /// (the P7 raw-ring tap) calls it too, so [`SampleSink::push`]'s frame
    /// accounting matches a non-stereo stream instead of falling back to the
    /// ring's stereo design width.
    pub fn set_channels(&self, channels: u16) {
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

    /// Drain every interleaved sample currently buffered into `out`, replacing
    /// its contents, and return how many were written. Used by the P7 raw-ring
    /// probe, which polls the ring off-thread instead of running the DSP hop
    /// grid; it clears `out` first, so pre-sizing `out` to the ring capacity
    /// keeps the drain allocation-free.
    pub fn drain_all(&mut self, out: &mut Vec<f32>) -> usize {
        out.clear();
        let n = self.consumer.slots();
        if n == 0 {
            return 0;
        }
        match self.consumer.read_chunk(n) {
            Ok(chunk) => {
                let (first, second) = chunk.as_slices();
                out.extend_from_slice(first);
                out.extend_from_slice(second);
                let written = first.len() + second.len();
                chunk.commit_all();
                written
            }
            Err(_) => 0,
        }
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
    sample_ring_with_stats(Arc::new(SinkStats::new(epoch)))
}

/// Create a fresh sample ring that reuses an existing [`SinkStats`] block, so
/// its epoch and cumulative counters carry across a reopen. When the engine
/// tears down and rebuilds a capture stream it hands the new sink the same
/// stats the old one fed, so `pushed_frames`, `dropped_frames`, the callback
/// cadence and the monotonic epoch all continue uninterrupted. Only the
/// producer/consumer halves are new (a brand-new lock-free ring); the shared
/// statistics survive. Engine-internal: a reopen is a cold-path operation.
#[must_use]
pub(crate) fn sample_ring_with_stats(stats: Arc<SinkStats>) -> (SampleSink, SampleConsumer) {
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
    /// Transient buffer under/overruns the backend reported so far (never fatal).
    fn xruns(&self) -> u64 {
        0
    }

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

    /// A stable identity of the device the next [`open`](CaptureBackend::open)
    /// would bind to *right now* for the last-opened target (the device name is
    /// a fine identity). The engine's route watcher polls this to notice that
    /// the OS default output route has moved to a different device and trigger a
    /// reopen. `None` means the backend cannot tell which device it would pick
    /// (the synthetic source, an enumeration failure); the watcher then relies
    /// only on stream-health errors, never on a route-id change.
    ///
    /// Called from a low-priority thread every few hundred milliseconds: it may
    /// enumerate devices, but must never block for long.
    fn route_id(&self) -> Option<String> {
        None
    }

    /// Change the device the next [`open`](CaptureBackend::open) binds to. The
    /// engine's runtime device switch ([`Engine::set_device`](crate::Engine::set_device))
    /// records the new selector here before the route watcher performs the
    /// reopen, so the switch lands on the watcher's next tick. The default is a
    /// no-op for backends with no device concept (the synthetic source); the
    /// cpal backend stores the selector so its next resolution binds it.
    #[cfg(feature = "capture-cpal")]
    fn set_device(&mut self, _selector: crate::backends::cpal::DeviceSelector) {}
}

// ---------------------------------------------------------------------------
// Raw-ring analysis (P7 raw-ring latency probe)
// ---------------------------------------------------------------------------
//
// These pure helpers let the latency probe measure capture transport with no
// hop quantization: it drains the ring off-thread (see
// [`SampleConsumer::drain_all`]), records each drain's clock reading in a
// [`DrainTimeline`] so any sample's capture time is reconstructable on the ring
// epoch, and finds a known click's leading edge in the drained stream with
// [`rect_xcorr_peak`]. They carry no audio dependency and are exercised directly
// by unit tests and the synthetic raw-ring regression.

/// Reconstructs the capture-time of every sample drained from a
/// [`SampleConsumer`] outside the engine (the P7 raw-ring probe's tap).
///
/// The probe drains the ring on a fixed poll: each poll pops whatever whole
/// stream of interleaved samples is buffered and hands this timeline an anchor
/// time for the drain's newest frame (on the ring epoch — the same clock
/// [`SinkStats::now_ns`] and `FeatureSnapshot::timestamp_ns` share) together
/// with the number of *frames* popped (interleaved samples ÷ channels).
///
/// The anchor is the **capture-delivery clock**, not the poll's own read time.
/// The frames a poll pops entered the ring when a capture callback delivered
/// them; the newest of them was delivered by the most recent push, whose time
/// [`SinkStats::last_push_ns`] records. Anchoring there measures "when the
/// samples entered scia's ring" — exactly what `emit → raw-arrival` is defined
/// to measure. Anchoring on the poll's `now_ns()` instead would fold the probe's
/// own drain-poll latency (the gap between a callback delivering a packet and the
/// next poll reading it — larger under a coalesced OS timer) into every
/// reconstructed time, which is a probe artifact, not capture transport, and
/// pushes raw-arrival past the engine's `emit → publish` even though ring entry
/// strictly precedes any hop that carries the sample. [`drain_into_timeline`]
/// supplies `last_push_ns` as the anchor for this reason.
///
/// Bookkeeping, stated exactly so the probe's cross-correlation lands on the
/// right clock: the newest popped frame was captured about one frame-period
/// before its delivery (`anchor_ns`) and the oldest about `frames` frame-periods
/// before it. We therefore place the oldest frame of a drain at
/// `base = anchor_ns − frames × ns_per_frame` and step forward one frame-period
/// per frame, so for global frame index `g` (drains are contiguous, so global
/// indices accumulate across polls)
/// `sample_time_ns(g) = base + (g − start_frame) × ns_per_frame`. The per-frame
/// quantum is `1e9 / sample_rate` ns — ~20.8 µs at 48 kHz, far below the
/// millisecond effects being measured — and the only real error is up to one
/// push interval of jitter on `anchor_ns`.
pub struct DrainTimeline {
    ns_per_frame: f64,
    total_frames: u64,
    segments: Vec<DrainSegment>,
}

/// One recorded drain: a contiguous run of frames and the capture-time of its
/// oldest frame.
#[derive(Clone, Copy, Debug)]
struct DrainSegment {
    start_frame: u64,
    frames: u64,
    base_ns: u64,
}

impl DrainTimeline {
    /// A timeline for a stream captured at `sample_rate` Hz. A zero rate
    /// degenerates to a one-nanosecond frame period so the arithmetic never
    /// divides by zero.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let ns_per_frame = if sample_rate == 0 {
            1.0
        } else {
            1.0e9 / f64::from(sample_rate)
        };
        Self {
            ns_per_frame,
            total_frames: 0,
            segments: Vec::new(),
        }
    }

    /// Reserve room for `polls` drain records up front (off the hot path), so a
    /// long run does not reallocate the segment list mid-drain.
    pub fn reserve(&mut self, polls: usize) {
        self.segments.reserve(polls);
    }

    /// Total frames recorded so far — the global index the next drained frame
    /// will get.
    #[must_use]
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Record one drain of `frames` frames anchored at `anchor_ns` (ns on the
    /// ring epoch) — the capture-delivery time of the drain's newest frame (one
    /// frame-period after that frame), i.e. [`SinkStats::last_push_ns`] at the
    /// drain, not the poll's own read time. A zero-frame drain is ignored.
    pub fn record_drain(&mut self, anchor_ns: u64, frames: u64) {
        if frames == 0 {
            return;
        }
        let span_ns = (frames as f64 * self.ns_per_frame).round() as u64;
        let base_ns = anchor_ns.saturating_sub(span_ns);
        self.segments.push(DrainSegment {
            start_frame: self.total_frames,
            frames,
            base_ns,
        });
        self.total_frames += frames;
    }

    /// Capture-time (ns on the ring epoch) of global frame `frame`, or `None`
    /// when it was never recorded.
    #[must_use]
    pub fn sample_time_ns(&self, frame: u64) -> Option<u64> {
        // Segments are contiguous and sorted by start_frame, so the first whose
        // end is past `frame` is the one that contains it.
        let idx = self
            .segments
            .partition_point(|s| s.start_frame + s.frames <= frame);
        let seg = self.segments.get(idx)?;
        if frame < seg.start_frame {
            return None;
        }
        let offset = frame - seg.start_frame;
        Some(seg.base_ns + (offset as f64 * self.ns_per_frame).round() as u64)
    }

    /// The first global frame whose capture-time is at or after `t_ns`, clamped
    /// to `0..=total_frames`. Turns a time-based search window into a frame
    /// offset range for [`rect_xcorr_peak`].
    #[must_use]
    pub fn frame_at_or_after(&self, t_ns: u64) -> u64 {
        for seg in &self.segments {
            let seg_end_ns = seg.base_ns + (seg.frames as f64 * self.ns_per_frame).round() as u64;
            if t_ns < seg_end_ns {
                if t_ns <= seg.base_ns {
                    return seg.start_frame;
                }
                let into = ((t_ns - seg.base_ns) as f64 / self.ns_per_frame).ceil() as u64;
                return seg.start_frame + into.min(seg.frames);
            }
        }
        self.total_frames
    }
}

/// Drain everything currently buffered from `consumer`, down-mix it to mono,
/// append it to `mono`, and record the drain in `timeline`. This is the raw-ring
/// probe's per-poll step, shared by the `latency_probe` example and the synthetic
/// regression so both reconstruct sample times through one code path.
///
/// The drain is anchored to the **capture-delivery clock**
/// ([`SinkStats::last_push_ns`] — the moment the most recent callback delivered
/// the newest buffered frame into the ring), read just before the drain so it
/// names a frame already buffered. That is precisely "the moment the samples
/// enter scia's ring", the quantity `emit → raw-arrival` measures; anchoring on
/// the poll's own `now_ns()` instead would fold the probe's drain-poll latency
/// into every reconstructed time (worse under a coalesced OS timer) and push
/// raw-arrival past the engine's `emit → publish`, even though ring entry
/// strictly precedes any hop that carries the sample. A push landing in the tiny
/// window between the anchor read and [`SampleConsumer::drain_all`] only shifts
/// the newest few frames sub-millisecond and self-corrects on the next poll.
///
/// `scratch` is reused as the interleaved drain buffer (cleared each call);
/// pre-sizing it and `mono` to the ring capacity keeps the drain allocation-free.
pub fn drain_into_timeline(
    consumer: &mut SampleConsumer,
    scratch: &mut Vec<f32>,
    mono: &mut Vec<f32>,
    timeline: &mut DrainTimeline,
    channels: usize,
) {
    let anchor_ns = consumer.stats().last_push_ns.load(Ordering::Acquire);
    let n = consumer.drain_all(scratch);
    if n == 0 {
        return;
    }
    let channels = channels.max(1);
    let frames = n / channels;
    for f in 0..frames {
        let base = f * channels;
        let mut acc = 0.0f32;
        for c in 0..channels {
            acc += scratch[base + c];
        }
        mono.push(acc / channels as f32);
    }
    timeline.record_drain(anchor_ns, frames as u64);
}

/// Acceptance floor a click's cross-correlation peak must clear for the raw-ring
/// probe to treat it as found. Kept low on purpose: a synthetic click is a
/// single-frame impulse, whose normalized correlation against a rectangular
/// template of `L` frames plateaus at `1/√L` (≈0.14 for a 1 ms / 48 kHz
/// template), while a real full-width click and a matching-width burst score
/// near 1.0 and silence / a never-arrived click score ≈ 0.
pub const RAW_CORR_ACCEPT: f32 = 0.1;

/// Peak normalized cross-correlation of a rectangular (all-ones) template of
/// `template_len` samples against `signal`, scanned over correlation offsets
/// `search_start..search_end`. Offset `o` correlates the template with
/// `signal[o .. o + template_len]`; the return is `(offset of the greatest NCC,
/// that NCC)`.
///
/// This is the matched filter the P7 raw-ring probe uses to place a click's
/// leading edge in the captured stream. An emitted click is a rectangular burst
/// of known width, so the template is all-ones of that width and the NCC peaks
/// where the burst begins. Using NCC rather than a raw dot product makes the
/// score amplitude-independent — 1.0 for a perfectly matching positive burst,
/// near 0 for silence or zero-mean noise — so one acceptance floor
/// ([`RAW_CORR_ACCEPT`]) separates "click found" from "click never arrived".
///
/// Ties are resolved to the *latest* offset. For a rectangular-matched burst
/// that is its unique strict peak, so the choice never bites there; for a pulse
/// narrower than the template (a synthetic single-frame impulse), the tie runs
/// over the plateau of offsets whose window still contains the pulse, and the
/// latest of them is the offset whose template *start* aligns with the pulse —
/// i.e. the leading-edge frame in both cases.
///
/// Returns `None` when `template_len` is zero, longer than `signal`, or the
/// clamped search range is empty.
#[must_use]
pub fn rect_xcorr_peak(
    signal: &[f32],
    template_len: usize,
    search_start: usize,
    search_end: usize,
) -> Option<(usize, f32)> {
    if template_len == 0 || template_len > signal.len() {
        return None;
    }
    // Highest offset whose window still fits, plus one (an exclusive bound).
    let offset_bound = signal.len() - template_len + 1;
    let start = search_start.min(offset_bound);
    let end = search_end.min(offset_bound);
    if start >= end {
        return None;
    }

    // Template energy is L (all ones), so ‖template‖ = √L; NCC(o) =
    // Σ window / (√(Σ window²) · √L). Maintain the window's running sum and
    // sum-of-squares so each offset costs O(1).
    let norm = (template_len as f64).sqrt();
    let mut sum = 0.0f64;
    let mut sq = 0.0f64;
    for &v in &signal[start..start + template_len] {
        let v = f64::from(v);
        sum += v;
        sq += v * v;
    }

    let mut best: Option<(usize, f32)> = None;
    for o in start..end {
        let ncc = if sq > 0.0 {
            (sum / (sq.sqrt() * norm)) as f32
        } else {
            0.0
        };
        // `>=` keeps the latest max (see the tie-break note above).
        match best {
            Some((_, b)) if ncc < b => {}
            _ => best = Some((o, ncc)),
        }
        // Slide the window one sample right for the next offset.
        if o + 1 < end {
            let leaving = f64::from(signal[o]);
            let entering = f64::from(signal[o + template_len]);
            sum += entering - leaving;
            sq += entering * entering - leaving * leaving;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-noise in roughly `-1.0..=1.0` from a splitmix64
    /// finalizer, so the correlation tests are stable across runs.
    fn noise(index: u64) -> f32 {
        let mut z = index.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        ((z >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    }

    #[test]
    fn xcorr_finds_a_rectangular_burst_within_one_sample() {
        // A 64-sample burst at amp 0.8 starting at frame 1500, low noise
        // everywhere.
        let n = 4096;
        let l = 64;
        let p = 1500;
        let mut sig = vec![0.0f32; n];
        for (i, s) in sig.iter_mut().enumerate() {
            *s = 0.02 * noise(i as u64);
        }
        for s in &mut sig[p..p + l] {
            *s += 0.8;
        }
        let (offset, peak) = rect_xcorr_peak(&sig, l, 0, n - l + 1).expect("a peak");
        assert!(
            (offset as i64 - p as i64).abs() <= 1,
            "peak offset {offset} is not within 1 of the burst start {p}"
        );
        assert!(peak > 0.9, "matching-burst NCC {peak} should be near 1.0");
    }

    #[test]
    fn xcorr_rejects_a_no_click_window() {
        // Pure noise, no burst: with a wide template and a modest window the
        // peak NCC stays far below both a matching burst (~1.0) and the probe's
        // acceptance floor for a real full-width click.
        let n = 2560;
        let l = 2048;
        let sig: Vec<f32> = (0..n).map(|i| 0.05 * noise(i as u64 + 777)).collect();
        let (_, peak) = rect_xcorr_peak(&sig, l, 0, n - l + 1).expect("a peak");
        assert!(
            peak < 0.3,
            "no-click NCC {peak} should stay well below a matching burst"
        );
    }

    #[test]
    fn xcorr_edge_cases_return_none() {
        let sig = vec![0.0f32; 16];
        // Zero-length template.
        assert!(rect_xcorr_peak(&sig, 0, 0, 4).is_none());
        // Template longer than the signal.
        assert!(rect_xcorr_peak(&sig, 32, 0, 1).is_none());
        // Empty search range.
        assert!(rect_xcorr_peak(&sig, 4, 5, 5).is_none());
        // Search range clamped past the last valid offset yields the last offset.
        let mut s = vec![0.0f32; 16];
        s[12] = 1.0; // impulse; template 4 => plateau [9..=12], latest = 12
        let (offset, _) = rect_xcorr_peak(&s, 4, 0, 999).expect("a peak");
        assert_eq!(
            offset, 12,
            "tie should resolve to the impulse's leading edge"
        );
    }

    #[test]
    fn drain_timeline_reconstructs_per_sample_times() {
        // Rate 1000 Hz => exactly 1_000_000 ns per frame, so the arithmetic is
        // integer-clean and the reconstruction is exact.
        let mut tl = DrainTimeline::new(1000);
        // Drain 1: read at 10 ms, 5 frames => oldest at 10ms-5ms = 5ms.
        tl.record_drain(10_000_000, 5);
        // Drain 2: read at 15 ms, 3 frames => oldest at 15ms-3ms = 12ms.
        tl.record_drain(15_000_000, 3);
        assert_eq!(tl.total_frames(), 8);

        // Segment 1 frames 0..=4 at 5,6,7,8,9 ms.
        assert_eq!(tl.sample_time_ns(0), Some(5_000_000));
        assert_eq!(tl.sample_time_ns(4), Some(9_000_000));
        // Segment 2 frames 5..=7 at 12,13,14 ms.
        assert_eq!(tl.sample_time_ns(5), Some(12_000_000));
        assert_eq!(tl.sample_time_ns(7), Some(14_000_000));
        // Past the end: no time.
        assert_eq!(tl.sample_time_ns(8), None);

        // frame_at_or_after maps a time back to the first frame at/after it.
        assert_eq!(tl.frame_at_or_after(0), 0);
        assert_eq!(tl.frame_at_or_after(5_000_000), 0);
        assert_eq!(tl.frame_at_or_after(9_000_001), 5); // just past seg-1's last frame -> seg 2
        assert_eq!(tl.frame_at_or_after(13_000_000), 6);
        assert_eq!(tl.frame_at_or_after(99_000_000), 8); // past the end
    }

    #[test]
    fn drain_timeline_zero_frame_drain_is_ignored() {
        let mut tl = DrainTimeline::new(48_000);
        tl.record_drain(1_000_000, 0);
        assert_eq!(tl.total_frames(), 0);
        assert_eq!(tl.sample_time_ns(0), None);
    }

    #[test]
    fn drain_into_timeline_anchors_on_capture_delivery_not_poll() {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let epoch = Instant::now();
        let (mut sink, mut consumer) = sample_ring(epoch);
        sink.stats().set_channels(1);

        // A capture callback delivers 480 mono frames (10 ms at 48 kHz) into the
        // ring after the stream has been running a while; `last_push_ns` records
        // that delivery time. (The delivery is necessarily at least one buffer
        // duration into the stream — a 10 ms block cannot arrive before 10 ms.)
        sleep(Duration::from_millis(12));
        sink.push(&vec![0.5f32; 480]);
        let push_ns = consumer.stats().last_push_ns.load(Ordering::Acquire);

        // The probe's drain poll fires much later than the delivery — the coarse,
        // coalesced-timer regime the field run hit. Anchoring on this poll's read
        // (`now_ns()`) would inflate every reconstructed sample time by the gap.
        sleep(Duration::from_millis(20));
        let poll_ns = consumer.stats().now_ns();

        let mut scratch: Vec<f32> = Vec::new();
        let mut mono: Vec<f32> = Vec::new();
        let mut timeline = DrainTimeline::new(48_000);
        drain_into_timeline(&mut consumer, &mut scratch, &mut mono, &mut timeline, 1);

        assert_eq!(mono.len(), 480, "all delivered frames drained");
        let newest = timeline
            .sample_time_ns(timeline.total_frames() - 1)
            .expect("newest frame has a reconstructed time");
        // The newest reconstructed time tracks the capture-delivery clock
        // (within a frame or two), not the ~20 ms-late poll read.
        assert!(
            newest <= push_ns + 2_000_000,
            "newest reconstructed time {newest} should track the delivery clock \
             {push_ns}, not the late poll"
        );
        assert!(
            poll_ns >= newest + 15_000_000,
            "the late poll read {poll_ns} must not be folded into the newest \
             reconstructed time {newest}"
        );
    }

    #[test]
    fn drain_all_pops_everything_buffered() {
        use std::time::Instant;
        let (mut sink, mut consumer) = sample_ring(Instant::now());
        sink.stats().set_channels(2);
        sink.push(&[0.1, 0.2, 0.3, 0.4]);
        let mut out = Vec::with_capacity(RING_FRAMES * 2);
        let n = consumer.drain_all(&mut out);
        assert_eq!(n, 4);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
        // A second drain with nothing buffered yields nothing.
        assert_eq!(consumer.drain_all(&mut out), 0);
        assert!(out.is_empty());
    }
}
