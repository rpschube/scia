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

use crate::backends::wasapi_route::RouteNotifier;
use crate::beat::BeatDebug;
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
    /// Opt in to Windows perf mode: on start (and on every capture reopen)
    /// capability-detect the default render endpoint and, when a faster engine
    /// period exists, hold a companion silent render stream that pulls the
    /// endpoint — and the loopback capture on it — down to that period. Default
    /// `false`. Evaluated only when the `perf-mode` feature is compiled in and
    /// the platform is Windows; anywhere else the engine reports
    /// [`PerfModeState::Unavailable`] and captures unchanged. See
    /// [`Engine::perf_mode_state`].
    pub perf_mode: bool,
    /// Register the platform event-driven route-change notifier (a Windows
    /// `IMMNotificationClient`) that flips the reopen-request flag the instant
    /// the default endpoint moves, so a switch is caught on the watcher's next
    /// tick rather than a poll cycle later. Default `true`; a no-op wherever the
    /// notifier is unsupported (every non-Windows build) — polling still covers
    /// the function.
    pub route_notify: bool,
    /// How long capture may stay down — the stream errored and the watcher still
    /// failing to reopen — before [`Engine::engine_health`] reports
    /// [`EngineHealth::Failed`] instead of [`EngineHealth::Reconnecting`]. The
    /// grace window the UI loops render a calm "reconnecting" state through
    /// instead of exiting on the first fault. Default
    /// [`DEFAULT_RECONNECT_DEADLINE`] (10 s); a test shortens it to avoid a real
    /// ten-second wait.
    pub reconnect_deadline: Duration,
}

/// Default [`EngineConfig::reconnect_deadline`]: how long capture may stay down
/// before the engine gives up and reports [`EngineHealth::Failed`]. Ten seconds
/// is long enough to ride out a PipeWire default-sink switch (whose reopen may
/// take a couple of enumeration cycles to settle) yet short enough that a truly
/// dead device surfaces a clear error rather than hanging in "reconnecting"
/// forever.
pub const DEFAULT_RECONNECT_DEADLINE: Duration = Duration::from_secs(10);

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::SystemMix,
            dsp: DspConfig::default(),
            route_watch: true,
            route_poll: Duration::from_millis(250),
            perf_mode: false,
            route_notify: true,
            reconnect_deadline: DEFAULT_RECONNECT_DEADLINE,
        }
    }
}

/// A consumer-facing view of capture health that models the grace window: while
/// the stream is down and the `scia-route` watcher is still trying to reopen it,
/// the engine reports [`EngineHealth::Reconnecting`] rather than a fatal error,
/// so a UI loop can show a calm degraded notice and keep rendering. Only after
/// reopen has failed continuously for [`EngineConfig::reconnect_deadline`] does
/// it become [`EngineHealth::Failed`].
///
/// This is distinct from [`StreamHealth`], which is the raw per-stream fault
/// flag; `EngineHealth` folds in the watcher's reopen progress and the deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineHealth {
    /// Capture is live (or a brief blip recovered before any reopen failed).
    Ok,
    /// Capture is down and the watcher is retrying. Rendered as a degraded
    /// notice; the loop keeps running.
    Reconnecting {
        /// Milliseconds since the reconnect episode began (the first failed
        /// reopen).
        since_ms: u32,
        /// Failed reopen attempts so far in this episode.
        attempts: u64,
    },
    /// Reopen has failed continuously past the deadline. The loops exit with
    /// `error`.
    Failed {
        /// The most recent reopen failure's message.
        error: String,
    },
}

/// The runtime state of Windows perf mode, read with [`Engine::perf_mode_state`].
#[derive(Clone, Debug, PartialEq)]
pub enum PerfModeState {
    /// Perf mode was not requested (`EngineConfig::perf_mode` was `false`).
    Off,
    /// A companion render stream is holding the endpoint at a fast engine
    /// period; the loopback capture inherits it.
    Active {
        /// The engine period the companion stream runs at, in frames.
        period_frames: u32,
        /// The endpoint mix-format sample rate the period is measured against.
        sample_rate: u32,
    },
    /// Perf mode was requested but could not be engaged. `reason` is a one-line
    /// explanation: not Windows, no render endpoint, the endpoint is locked to
    /// its default period, or the companion stream failed to open.
    Unavailable {
        /// Why perf mode is not active.
        reason: String,
    },
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
    /// The companion perf-mode render stream, when perf mode is active. It lives
    /// and dies with the capture stream: re-created on each reopen (on the new
    /// endpoint) and dropped at shutdown. `None` when perf mode is off,
    /// unavailable, or not yet evaluated. Held purely as an RAII guard — its
    /// value is never read back, only dropped — so `dead_code` is allowed.
    #[cfg(feature = "perf-mode")]
    #[allow(dead_code)]
    perf: Option<crate::backends::wasapi_perf::PerfModeStream>,
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
    /// A runtime device switch requested through [`Engine::set_device`], applied
    /// to the backend at the start of the next reopen (before `open`). `None`
    /// when no switch is pending. Only the cpal backend acts on it; a backend
    /// with no device concept ignores the applied selector.
    #[cfg(feature = "capture-cpal")]
    pending_device: Mutex<Option<crate::backends::cpal::DeviceSelector>>,
    reopens: AtomicU64,
    reopen_failures: AtomicU64,
    /// How long capture may stay down before [`Shared::engine_health`] reports
    /// [`EngineHealth::Failed`]. Copied from [`EngineConfig::reconnect_deadline`].
    reconnect_deadline: Duration,
    /// Monotonic ns (engine epoch) when the current reconnect episode began — the
    /// first failed reopen after the stream went down. `0` when capture is
    /// healthy; cleared the instant a reopen succeeds. Drives the grace-window
    /// timing in [`Shared::engine_health`].
    errored_since_ns: AtomicU64,
    /// Failed reopen attempts in the current episode; reset to `0` on recovery.
    episode_attempts: AtomicU64,
    /// The most recent reopen failure's error text, for diagnostics and the
    /// [`EngineHealth::Failed`] payload. `None` until a reopen has failed.
    last_reopen_error: Mutex<Option<String>>,
    /// Whether perf mode was requested (`EngineConfig::perf_mode`). When set,
    /// start and every successful reopen re-evaluate perf mode on the current
    /// default render endpoint. Only read when the `perf-mode` feature is
    /// compiled in; a feature-off build stores it but never consults it.
    #[cfg_attr(not(feature = "perf-mode"), allow(dead_code))]
    perf_mode: bool,
    /// The latest perf-mode state, refreshed on start and every reopen.
    perf_state: Mutex<PerfModeState>,
}

impl Shared {
    /// Tear down the current stream and build a fresh one, swapping the new ring
    /// under the DSP thread without stopping it. See [`Engine::reopen`].
    fn reopen(&self) -> Result<StreamFormat, CaptureError> {
        let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        // Apply a pending runtime device switch to the backend before opening, so
        // this reopen binds the newly chosen device.
        #[cfg(feature = "capture-cpal")]
        if let Some(selector) = self
            .pending_device
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            route.backend.set_device(selector);
        }
        // A brand-new ring that keeps feeding the same cumulative stats.
        let (sink, consumer) = sample_ring_with_stats(Arc::clone(&self.stats));
        let target = route.target;

        // Is the current capture *down* — the stream errored, or already dropped
        // by a previous down-path reopen that then failed to open a replacement?
        // A down stream carries no audio to preserve, so we drop it *before*
        // opening its replacement.
        //
        // This ordering is the CAP-2 Linux fix. On the cpal PipeWire host a
        // `pactl set-default-sink` fires the stream error callback ("default
        // device changed"); the reopen then re-enumerates and opens a fresh
        // stream on the new default. Opening that replacement while the errored
        // stream is still connected leaves two capture streams contending on the
        // PipeWire graph across the switch, and the new stream does not settle —
        // so capture never recovers and the app used to exit. Dropping the dead
        // stream first (stopping its capture thread and clearing it off the
        // graph) lets the fresh device enumeration settle and the replacement
        // connect cleanly. The DSP thread synthesizes silence through the gap.
        //
        // A *healthy* reopen (a seamless route swap — e.g. the Windows
        // default-endpoint notification flips the reopen request without erroring
        // the stream) keeps the original open-before-drop ordering below, so there
        // is never a gap in the live ring. This is why Windows behavior is
        // unchanged: its device switch never errors the stream, so it never takes
        // the down path.
        let down = match route.stream.as_ref() {
            Some(stream) => matches!(stream.health(), StreamHealth::Errored(_)),
            None => true,
        };
        if down {
            // Stop and join the dead capture thread before touching the backend.
            drop(route.stream.take());
        }

        match route.backend.open(target, sink) {
            Ok(new_stream) => {
                let format = new_stream.format();
                self.stats.set_channels(format.channels);
                // Publish the new ring to the DSP thread. On the healthy path the
                // old stream is still live here, so this happens before it is
                // dropped and there is never a window with no live ring; on the
                // down path the old stream is already gone and the DSP thread has
                // been synthesizing silence.
                self.swap.publish(consumer, format);
                let old = route.stream.replace(new_stream);
                route.format = format;
                route.route_id = route.backend.route_id();
                self.reopens.fetch_add(1, Ordering::Relaxed);
                // Recovery: end any reconnect episode.
                self.errored_since_ns.store(0, Ordering::Release);
                self.episode_attempts.store(0, Ordering::Relaxed);
                // Re-evaluate perf mode on the (possibly new) endpoint: a device
                // switch drops the old companion stream and re-detects on the new
                // one. Still under the route lock, before the old stream is joined.
                #[cfg(feature = "perf-mode")]
                if self.perf_mode {
                    self.evaluate_perf(&mut route);
                }
                // Release the route lock before joining the old stream's thread.
                drop(route);
                drop(old);
                Ok(format)
            }
            Err(err) => {
                // The replacement did not open. On the down path the old stream is
                // already gone (health reports Errored from the None arm below);
                // on the healthy path the old stream is kept and still delivering.
                // The DSP thread keeps synthesizing silence until a later attempt
                // succeeds.
                self.reopen_failures.fetch_add(1, Ordering::Relaxed);
                *self
                    .last_reopen_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
                // Only a *down* capture is a reconnect episode: a failed seamless
                // swap is not degraded (the old healthy stream plays on), so it
                // must not start the grace-window clock.
                if down {
                    let now = self.stats.now_ns().max(1);
                    let _ = self.errored_since_ns.compare_exchange(
                        0,
                        now,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    self.episode_attempts.fetch_add(1, Ordering::Relaxed);
                }
                Err(err)
            }
        }
    }

    /// Health of the current stream. A dropped stream (the down-path reopen
    /// cleared it and has not yet re-opened) reports [`StreamHealth::Errored`]
    /// with the last reopen error, since capture is genuinely down in that
    /// window.
    fn health(&self) -> StreamHealth {
        let route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        match route.stream.as_ref() {
            Some(stream) => stream.health(),
            None => {
                let msg = self
                    .last_reopen_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| "capture stream unavailable".to_string());
                StreamHealth::Errored(msg)
            }
        }
    }

    /// The consumer-facing [`EngineHealth`], derived from the reconnect-episode
    /// bookkeeping the watcher maintains through [`Shared::reopen`]. Reads only
    /// atomics and the last-error mutex — never the route lock — so a UI loop can
    /// poll it every frame without blocking on an in-flight reopen.
    fn engine_health(&self) -> EngineHealth {
        let since = self.errored_since_ns.load(Ordering::Acquire);
        if since == 0 {
            return EngineHealth::Ok;
        }
        let now = self.stats.now_ns();
        let elapsed = now.saturating_sub(since);
        let attempts = self.episode_attempts.load(Ordering::Relaxed);
        if elapsed >= self.reconnect_deadline.as_nanos() as u64 {
            let error = self
                .last_reopen_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "capture stream error".to_string());
            EngineHealth::Failed { error }
        } else {
            EngineHealth::Reconnecting {
                since_ms: (elapsed / 1_000_000) as u32,
                attempts,
            }
        }
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

    /// (Re-)evaluate perf mode for the default render endpoint and store the
    /// resulting [`PerfModeState`]. Drops any existing companion stream first
    /// (the endpoint may have changed), then, on Windows with a faster period
    /// available, opens a fresh companion stream and holds it in `route`. Called
    /// under the route lock from `start` and every successful `reopen`.
    ///
    /// The perf-mode evaluation queries the OS default render endpoint, which is
    /// independent of the capture backend — so it neither reads from nor
    /// disturbs a non-cpal (e.g. synthetic) backend.
    #[cfg(feature = "perf-mode")]
    fn evaluate_perf(&self, route: &mut Route) {
        // Drop any prior companion stream before re-detecting; on a device
        // switch the previous endpoint's stream must not linger.
        route.perf = None;

        #[cfg(windows)]
        let state = {
            use crate::backends::cpal::DeviceSelector;
            use crate::backends::wasapi_perf::{
                PerfModeAvailability, PerfModeConfig, PerfModeStream, availability,
            };

            let config = PerfModeConfig {
                device: DeviceSelector::Default,
                require_fast: true,
            };
            match availability(&config) {
                PerfModeAvailability::Available { .. } => match PerfModeStream::open(&config) {
                    Ok(stream) => {
                        let info = stream.info();
                        route.perf = Some(stream);
                        PerfModeState::Active {
                            period_frames: info.chosen_period_frames,
                            sample_rate: info.sample_rate,
                        }
                    }
                    Err(e) => PerfModeState::Unavailable {
                        reason: format!("companion stream failed to open: {e}"),
                    },
                },
                PerfModeAvailability::DriverLocked { info } => {
                    let ms = f64::from(info.default_period_frames) * 1000.0
                        / f64::from(info.sample_rate.max(1));
                    PerfModeState::Unavailable {
                        reason: format!(
                            "endpoint is locked to its {} ({ms:.3} ms) engine period",
                            info.default_period_frames
                        ),
                    }
                }
                PerfModeAvailability::Unsupported(reason) => PerfModeState::Unavailable { reason },
            }
        };
        #[cfg(not(windows))]
        let state = PerfModeState::Unavailable {
            reason: "perf mode is Windows-only".to_string(),
        };

        *self.perf_state.lock().unwrap_or_else(|e| e.into_inner()) = state;
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
    /// The event-driven route-change notifier, when one registered. `None` when
    /// disabled by config or unsupported on the platform; the route watcher's
    /// poll then covers route changes on its own. Dropped first at shutdown so
    /// no callback can flip the reopen flag after the watcher is joined.
    route_notifier: Option<RouteNotifier>,
    counters: Arc<DspCounters>,
    /// Diagnostic side channel: the in-thread beat tracker's latest
    /// [`BeatDebug`], refreshed by the DSP thread once per induction pass. Read
    /// with [`Engine::beat_debug`]; never on the hop path.
    beat_debug: Arc<Mutex<BeatDebug>>,
    /// The fullscreen-pause control shared with the DSP thread (US-PERF-3).
    /// Handed out with [`Engine::pause_flag`] so a
    /// [`FullscreenWatch`](crate::fullscreen::FullscreenWatch) can drive it.
    pause: Arc<AtomicBool>,
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
        let beat_debug = Arc::new(Mutex::new(BeatDebug::default()));
        // The fullscreen-pause control (US-PERF-3): clear by default, so an
        // engine no one drives behaves exactly as before. The DSP thread reads it
        // to force its idle downshift; a caller flips it via a
        // [`FullscreenWatch`](crate::fullscreen::FullscreenWatch) on the same
        // `Arc` returned by [`Engine::pause_flag`].
        let pause = Arc::new(AtomicBool::new(false));

        let dsp = DspThread {
            consumer,
            format,
            writer,
            config: config.dsp,
            stop: Arc::clone(&stop),
            stats: Arc::clone(&stats),
            counters: Arc::clone(&counters),
            swap: Arc::clone(&swap),
            beat_debug: Arc::clone(&beat_debug),
            pause: Arc::clone(&pause),
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
                #[cfg(feature = "perf-mode")]
                perf: None,
            }),
            stats,
            swap,
            request: AtomicBool::new(false),
            #[cfg(feature = "capture-cpal")]
            pending_device: Mutex::new(None),
            reopens: AtomicU64::new(0),
            reopen_failures: AtomicU64::new(0),
            reconnect_deadline: config.reconnect_deadline,
            errored_since_ns: AtomicU64::new(0),
            episode_attempts: AtomicU64::new(0),
            last_reopen_error: Mutex::new(None),
            perf_mode: config.perf_mode,
            perf_state: Mutex::new(PerfModeState::Off),
        });

        // Evaluate perf mode once before the watcher can race a reopen. Only the
        // `perf-mode` feature on Windows can actually engage it; every other
        // build reports it unavailable when it was requested.
        if config.perf_mode {
            #[cfg(feature = "perf-mode")]
            {
                let mut route = shared.route.lock().unwrap_or_else(|e| e.into_inner());
                shared.evaluate_perf(&mut route);
            }
            #[cfg(not(feature = "perf-mode"))]
            {
                *shared.perf_state.lock().unwrap_or_else(|e| e.into_inner()) =
                    PerfModeState::Unavailable {
                        reason: "perf mode requires the perf-mode feature".to_string(),
                    };
            }
        }

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

        // Register the event-driven route notifier. Its only job is to flip the
        // same out-of-band reopen-request flag a poll would, the instant the OS
        // reports a default-endpoint change — the watcher does the actual reopen
        // on its next tick. Registration failure (or an unsupported platform) is
        // non-fatal: `None` is kept and the 250 ms poll still covers the switch.
        let route_notifier = if config.route_notify {
            let shared = Arc::clone(&shared);
            RouteNotifier::start(Box::new(move || {
                shared.request.store(true, Ordering::Release);
            }))
            .ok()
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
                route_notifier,
                counters,
                beat_debug,
                pause,
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

    /// The fullscreen-pause control (US-PERF-3): a clone of the flag the DSP
    /// thread reads. While it is `true` the DSP thread forces its idle downshift
    /// regardless of the audio level, so a foreground fullscreen game cannot keep
    /// the grid at full rate; clearing it resumes full-rate processing within one
    /// idle poll interval. A
    /// [`FullscreenWatch`](crate::fullscreen::FullscreenWatch) drives this flag;
    /// hand it the same `Arc` (and the render loop reads it too, to stop drawing).
    #[must_use]
    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.pause)
    }

    /// The current [`PerfModeState`]: [`PerfModeState::Off`] when perf mode was
    /// not requested, [`PerfModeState::Active`] when a companion stream is
    /// holding the endpoint at a fast period, or [`PerfModeState::Unavailable`]
    /// with a reason. Refreshed on start and on every successful reopen.
    #[must_use]
    pub fn perf_mode_state(&self) -> PerfModeState {
        self.shared
            .perf_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
    /// stream's error callback has fired (or the stream was dropped mid-reconnect),
    /// otherwise [`StreamHealth::Ok`]. This is the raw per-stream flag; UI loops
    /// should prefer [`Engine::engine_health`], which folds in the reconnect grace
    /// window.
    #[must_use]
    pub fn health(&self) -> StreamHealth {
        self.shared.health()
    }

    /// The consumer-facing capture health with the reconnect grace window applied
    /// (see [`EngineHealth`]): [`EngineHealth::Ok`] while capture is live,
    /// [`EngineHealth::Reconnecting`] while the stream is down and the watcher is
    /// still retrying, and [`EngineHealth::Failed`] only once reopen has failed
    /// continuously past [`EngineConfig::reconnect_deadline`]. Reads only atomics,
    /// so it never blocks on an in-flight reopen.
    #[must_use]
    pub fn engine_health(&self) -> EngineHealth {
        self.shared.engine_health()
    }

    /// The most recent reopen failure's error text, or `None` if no reopen has
    /// ever failed. Diagnostic side channel for the headless status line and the
    /// [`EngineHealth::Failed`] payload.
    #[must_use]
    pub fn last_reopen_error(&self) -> Option<String> {
        self.shared
            .last_reopen_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A read-only snapshot of the in-thread beat tracker's internal state (see
    /// [`BeatDebug`]), refreshed by the DSP thread once per induction pass
    /// (≈ every 1.2 s). Returns `None` until the first induction pass has run.
    ///
    /// Diagnostic-only: this is the *real* tracker the pipeline runs, exposed for
    /// calibration probes so they need not maintain a separate mirror tracker. It
    /// is never read back by the pipeline and has no effect on tracking or on the
    /// published [`FeatureSnapshot`](crate::FeatureSnapshot). The DSP thread only
    /// ever `try_lock`s this cell, so reading it can never stall the hop grid.
    #[must_use]
    pub fn beat_debug(&self) -> Option<BeatDebug> {
        let dbg = *self.beat_debug.lock().unwrap_or_else(|e| e.into_inner());
        if dbg.inductions == 0 { None } else { Some(dbg) }
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

    /// Select a different capture device at runtime. The selector is recorded
    /// and applied to the backend at the start of the next reopen, so the caller
    /// pairs this with [`request_reopen`](Engine::request_reopen) to make the
    /// switch happen: the route watcher then tears down the current stream and
    /// opens the newly chosen device, swapping its ring under the running DSP
    /// thread the same way a fault recovery or route change does. Recording a
    /// selector never blocks capture, and a switch with the watcher disabled is
    /// inert (nothing drives the reopen). Callable from any thread.
    #[cfg(feature = "capture-cpal")]
    pub fn set_device(&self, selector: crate::backends::cpal::DeviceSelector) {
        *self
            .shared
            .pending_device
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(selector);
    }

    /// Ask the route watcher to reopen on its next tick. This is the seam a
    /// platform default-device-change notification (e.g. a Windows
    /// `IMMNotificationClient`) calls: it need only flip this flag, and the same
    /// watcher path that handles faults and polled route changes does the rest.
    /// A no-op if the watcher is disabled.
    pub fn request_reopen(&self) {
        self.shared.request.store(true, Ordering::Release);
    }

    /// Whether an event-driven route-change notifier is registered and live.
    /// `true` only on a platform/build that supports it (Windows `route-notify`)
    /// with `route_notify` enabled and registration having succeeded; `false`
    /// everywhere else, where the route watcher's poll covers route changes.
    #[must_use]
    pub fn route_notify_active(&self) -> bool {
        self.route_notifier.is_some()
    }

    /// Stop the watcher, the DSP thread, and capture, and join everything.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Drop the route notifier first: joining its thread unregisters the OS
        // callback, so nothing can flip the reopen-request flag after this.
        self.route_notifier.take();
        // Join the watcher next so no reopen can race the teardown, then the
        // DSP thread, then drop the stream.
        if let Some(join) = self.watch_join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.dsp_join.take() {
            let _ = join.join();
        }
        // Dropping the stream stops the capture thread and joins it; dropping the
        // companion perf stream stops and joins its render thread too.
        let mut route = self.shared.route.lock().unwrap_or_else(|e| e.into_inner());
        route.stream.take();
        #[cfg(feature = "perf-mode")]
        {
            route.perf = None;
        }
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
