//! The engine wires a capture backend to the DSP thread and hands the caller a
//! [`FeatureReader`]. It owns the capture backend and its live stream, the DSP
//! thread, and the shared statistics; stopping (or dropping) it tears the
//! pipeline down.
//!
//! Capture is reopenable at runtime. [`Engine::reopen`] tears down the current
//! stream and builds a fresh one — on the same or a renegotiated format — and
//! swaps the new sample ring under the running DSP thread without stopping it,
//! so a mid-song device switch resumes visualization on the new device within a
//! second. A low-priority `scia-route` watcher thread drives reopens
//! automatically from stream-health faults and from a change in the backend's
//! current default route, and honours an out-of-band [`Engine::request_reopen`]
//! (the seam a platform device-change notification plugs into).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::bus::{FeatureReader, feature_bus};
use crate::capture::{
    CaptureBackend, CaptureError, CaptureStream, CaptureTarget, SinkStats, StreamFormat,
    StreamHealth, sample_ring, sample_ring_with_stats,
};
use crate::dsp::{DspConfig, DspCounters, DspThread, RingSwap};
use crate::features::Activity;

/// Configuration for [`Engine::start`].
#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    /// What to capture.
    pub target: CaptureTarget,
    /// DSP thread tuning.
    pub dsp: DspConfig,
    /// Run the `scia-route` watcher thread that reopens capture automatically on
    /// a stream fault or a default-route change. Default `true`.
    pub route_watch: bool,
    /// How often the route watcher polls. Default 250 ms.
    pub route_poll: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::SystemMix,
            dsp: DspConfig::default(),
            route_watch: true,
            route_poll: Duration::from_millis(250),
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
    /// Transient capture buffer under/overruns (counted, not fatal).
    pub xruns: u64,
    /// Successful runtime capture reopens (device switches, fault recoveries).
    pub reopens: u64,
    /// Reopen attempts that failed to open a stream (kept the previous one).
    pub reopen_failures: u64,
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

/// The backend and its current live stream, plus the identity of the device it
/// is bound to. Behind a mutex on [`Shared`]; touched only on the engine's cold
/// paths (start, reopen, the watcher's poll, teardown), never the hop path.
struct Route {
    backend: Box<dyn CaptureBackend>,
    stream: Option<Box<dyn CaptureStream>>,
    format: StreamFormat,
    route_id: Option<String>,
    target: CaptureTarget,
}

/// State shared between the engine handle and the `scia-route` watcher thread.
struct Shared {
    route: Mutex<Route>,
    /// Cumulative capture statistics, reused across every reopen so counters and
    /// the monotonic epoch survive a stream rebuild.
    stats: Arc<SinkStats>,
    /// The ring/format hand-off the DSP thread adopts on its next wake.
    swap: Arc<RingSwap>,
    /// An out-of-band reopen request the watcher honours on its next tick.
    request: AtomicBool,
    reopens: AtomicU64,
    reopen_failures: AtomicU64,
}

impl Shared {
    /// Tear down the current stream and build a fresh one, swapping the new ring
    /// under the DSP thread without stopping it. See [`Engine::reopen`].
    fn reopen(&self) -> Result<StreamFormat, CaptureError> {
        let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        // A brand-new ring that keeps feeding the same cumulative stats.
        let (sink, consumer) = sample_ring_with_stats(Arc::clone(&self.stats));
        let target = route.target;
        match route.backend.open(target, sink) {
            Ok(new_stream) => {
                let format = new_stream.format();
                self.stats.set_channels(format.channels);
                // Publish the new ring to the DSP thread *before* dropping the
                // old stream, so there is never a window with no live ring.
                self.swap.publish(consumer, format);
                let old = route.stream.replace(new_stream);
                route.format = format;
                route.route_id = route.backend.route_id();
                self.reopens.fetch_add(1, Ordering::Relaxed);
                // Release the route lock before joining the old stream's thread.
                drop(route);
                drop(old);
                Ok(format)
            }
            Err(err) => {
                // Keep whatever stream exists (it may be errored); the DSP thread
                // keeps synthesizing silence until a later attempt succeeds.
                self.reopen_failures.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Health of the current stream.
    fn health(&self) -> StreamHealth {
        let route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        route
            .stream
            .as_ref()
            .map_or(StreamHealth::Ok, |s| s.health())
    }

    /// Whether the backend's current default route differs from the one the live
    /// stream was opened against. `false` when the backend cannot tell (its
    /// `route_id` is `None`) — the watcher then relies on health alone.
    fn route_changed(&self) -> bool {
        let route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        match route.backend.route_id() {
            Some(current) => route.route_id.as_deref() != Some(current.as_str()),
            None => false,
        }
    }
}

/// A running pipeline: capture → ring → DSP → feature bus.
pub struct Engine {
    shared: Arc<Shared>,
    /// The ring epoch every snapshot timestamp is measured from.
    epoch: Instant,
    stop: Arc<AtomicBool>,
    dsp_join: Option<JoinHandle<()>>,
    watch_join: Option<JoinHandle<()>>,
    counters: Arc<DspCounters>,
}

impl Engine {
    /// Open `backend`, start the DSP thread (and, unless disabled, the route
    /// watcher), and return the engine together with a [`FeatureReader`] on the
    /// freshest snapshot.
    ///
    /// # Errors
    /// Returns [`EngineError::Capture`] if the backend cannot open, or
    /// [`EngineError::Spawn`] if a thread cannot be created.
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
        let route_id = backend.route_id();

        let (writer, reader) = feature_bus();
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(DspCounters::default());
        let swap = Arc::new(RingSwap::new());

        let dsp = DspThread {
            consumer,
            format,
            writer,
            config: config.dsp,
            stop: Arc::clone(&stop),
            stats: Arc::clone(&stats),
            counters: Arc::clone(&counters),
            swap: Arc::clone(&swap),
        };

        let dsp_join = thread::Builder::new()
            .name("scia-dsp".into())
            .spawn(move || crate::dsp::run(dsp))
            .map_err(|e| EngineError::Spawn(e.to_string()))?;

        let shared = Arc::new(Shared {
            route: Mutex::new(Route {
                backend,
                stream: Some(stream),
                format,
                route_id,
                target: config.target,
            }),
            stats,
            swap,
            request: AtomicBool::new(false),
            reopens: AtomicU64::new(0),
            reopen_failures: AtomicU64::new(0),
        });

        let watch_join = if config.route_watch {
            let shared = Arc::clone(&shared);
            let stop_watch = Arc::clone(&stop);
            let poll = config.route_poll;
            match thread::Builder::new()
                .name("scia-route".into())
                .spawn(move || run_watcher(&shared, &stop_watch, poll))
            {
                Ok(join) => Some(join),
                Err(e) => {
                    // Undo the DSP thread we already spawned.
                    stop.store(true, Ordering::Release);
                    let _ = dsp_join.join();
                    return Err(EngineError::Spawn(e.to_string()));
                }
            }
        } else {
            None
        };

        Ok((
            Engine {
                shared,
                epoch,
                stop,
                dsp_join: Some(dsp_join),
                watch_join,
                counters,
            },
            reader,
        ))
    }

    /// The negotiated stream format of the current stream.
    #[must_use]
    pub fn format(&self) -> StreamFormat {
        let route = self.shared.route.lock().unwrap_or_else(|e| e.into_inner());
        route.format
    }

    /// The ring epoch: the monotonic [`Instant`] every snapshot's
    /// `timestamp_ns` (and [`Engine::now_ns`]) is measured from. A probe that
    /// timestamps events outside the pipeline stamps them against this same
    /// origin so both ends share one clock.
    #[must_use]
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Monotonic nanoseconds since the ring epoch — the exact clock the DSP
    /// thread stamps `FeatureSnapshot::timestamp_ns` with.
    #[must_use]
    pub fn now_ns(&self) -> u64 {
        self.shared.stats.now_ns()
    }

    /// Current pipeline counters.
    #[must_use]
    pub fn stats(&self) -> EngineStats {
        let xruns = {
            let route = self.shared.route.lock().unwrap_or_else(|e| e.into_inner());
            route.stream.as_ref().map_or(0, |s| s.xruns())
        };
        EngineStats {
            dropped_frames: self.shared.stats.dropped_frames.load(Ordering::Relaxed),
            pushed_frames: self.shared.stats.pushed_frames.load(Ordering::Relaxed),
            hops_processed: self.counters.hops_processed.load(Ordering::Relaxed),
            hops_synthesized: self.counters.hops_synthesized.load(Ordering::Relaxed),
            agc_gain: f32::from_bits(self.counters.agc_gain_bits.load(Ordering::Relaxed)),
            pushes: self.shared.stats.pushes.load(Ordering::Relaxed),
            last_push_frames: self.shared.stats.last_push_frames.load(Ordering::Relaxed),
            max_push_frames: self.shared.stats.max_push_frames.load(Ordering::Relaxed),
            max_gap_ms: self.shared.stats.max_gap_ns.load(Ordering::Relaxed) as f32 / 1.0e6,
            activity: match self.counters.activity.load(Ordering::Relaxed) {
                1 => Activity::Quiet,
                2 => Activity::Idle,
                _ => Activity::Active,
            },
            dsp_wakes: self.counters.dsp_wakes.load(Ordering::Relaxed),
            xruns,
            reopens: self.shared.reopens.load(Ordering::Relaxed),
            reopen_failures: self.shared.reopen_failures.load(Ordering::Relaxed),
        }
    }

    /// The capture stream's health: [`StreamHealth::Errored`] once the current
    /// stream's error callback has fired, otherwise [`StreamHealth::Ok`].
    #[must_use]
    pub fn health(&self) -> StreamHealth {
        self.shared.health()
    }

    /// Tear down the current capture stream and open a fresh one, then swap its
    /// ring under the running DSP thread — no restart, and the DSP thread keeps
    /// synthesizing silence through the brief gap. The new stream may negotiate a
    /// different format (a 44.1 ↔ 48 kHz device switch); the DSP thread rebuilds
    /// its analysis for the new rate on the swap. Cumulative statistics carry
    /// across. On success returns the new format and bumps `reopens`; on failure
    /// the previous stream is left in place (it may be errored), `reopen_failures`
    /// is bumped, and the error is returned. Callable from any thread.
    ///
    /// # Errors
    /// Returns the backend's [`CaptureError`] if the new stream cannot open.
    pub fn reopen(&self) -> Result<StreamFormat, CaptureError> {
        self.shared.reopen()
    }

    /// Ask the route watcher to reopen on its next tick. This is the seam a
    /// platform default-device-change notification (e.g. a Windows
    /// `IMMNotificationClient`) calls: it need only flip this flag, and the same
    /// watcher path that handles faults and polled route changes does the rest.
    /// A no-op if the watcher is disabled.
    pub fn request_reopen(&self) {
        self.shared.request.store(true, Ordering::Release);
    }

    /// Stop the watcher, the DSP thread, and capture, and join everything.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Join the watcher first so no reopen can race the teardown, then the
        // DSP thread, then drop the stream.
        if let Some(join) = self.watch_join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.dsp_join.take() {
            let _ = join.join();
        }
        // Dropping the stream stops the capture thread and joins it.
        let mut route = self.shared.route.lock().unwrap_or_else(|e| e.into_inner());
        route.stream.take();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// First backoff after a failed reopen.
const BACKOFF_START: Duration = Duration::from_millis(100);
/// Upper bound the reopen backoff doubles toward.
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// The `scia-route` watcher loop. A plain sleep loop (no priority crate): every
/// `poll` it decides whether to reopen — an out-of-band request, then a stream
/// fault, then a default-route change, in that priority — and calls
/// [`Shared::reopen`]. After a failed reopen it stays committed and keeps
/// retrying with an exponential backoff (100 ms doubling to a 2 s cap, reset on
/// success), so an unplugged device with nothing to fall back on keeps the
/// engine alive and retrying forever while the DSP thread synthesizes silence
/// and `health` reports the last error. It checks the stop flag inside every
/// sleep, so teardown never waits out a backoff.
fn run_watcher(shared: &Shared, stop: &AtomicBool, poll: Duration) {
    let mut backoff: Option<Duration> = None;
    loop {
        let wait = backoff.unwrap_or(poll);
        if sleep_with_stop(stop, wait) {
            break;
        }
        // Reopen this tick when: already committed to retrying after a failure;
        // an out-of-band request is pending; the stream faulted; or the default
        // route moved — evaluated in that priority (short-circuit `||` keeps the
        // request flag consumed before health and route are examined). A pending
        // request is left set while a backoff retry is in flight and honoured
        // once it clears.
        let trigger = backoff.is_some()
            || shared.request.swap(false, Ordering::AcqRel)
            || matches!(shared.health(), StreamHealth::Errored(_))
            || shared.route_changed();
        if !trigger {
            continue;
        }
        match shared.reopen() {
            Ok(_) => backoff = None,
            Err(_) => {
                backoff = Some(backoff.map_or(BACKOFF_START, |b| (b * 2).min(BACKOFF_CAP)));
            }
        }
    }
}

/// Sleep up to `dur`, returning `true` as soon as `stop` is set (checked in
/// short steps so even a long backoff yields to teardown promptly).
fn sleep_with_stop(stop: &AtomicBool, dur: Duration) -> bool {
    const STEP: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + dur;
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(STEP.min(deadline - now));
    }
}
