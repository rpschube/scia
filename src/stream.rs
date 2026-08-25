//! Feature-stream I/O for the `scia` binary (US-UX-2).
//!
//! The output-serving half — the headless `--output` loop that paces the
//! engine's live feature bus onto stdout or a `--listen` socket — lives in
//! [`scia_core::stream::run_output`], shared with the `scia-bridge` companion so
//! the listener / fan-out / rate pacing is written once. It is re-exported here
//! ([`run_output`], [`DEFAULT_STREAM_RATE`]) so the binary's call sites resolve
//! through this module unchanged.
//!
//! This module owns the inverse, UI-facing path:
//!
//! * [`run_input`] — a producer thread connects to a remote `--output --listen`
//!   socket, decodes frames and publishes them onto a local feature bus exactly
//!   where the synthetic backend's generator would — so the whole TUI (scenes,
//!   chrome, overlays) renders from the remote stream, none the wiser. A dropped
//!   connection is ridden out with a bounded backoff while the TUI shows its
//!   normal reconnecting/quiet state; it never freezes or exits on a blip.

use std::io::BufReader;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use scia_core::engine::EngineHealth;
use scia_core::stream::{FrameStreamReader, StreamError};
use scia_core::{Activity, EngineStats, FeatureReader, FeatureSnapshot, feature_bus};

// The output-serving loop and its defaults now live in scia_core::stream (shared
// with scia-bridge). Re-export so `stream::run_output` / `stream::DEFAULT_STREAM_RATE`
// keep resolving here.
pub use scia_core::stream::{DEFAULT_STREAM_RATE, run_output};

/// First reconnect backoff on `--input`, and the cap it doubles toward. Mirrors
/// the engine's own reopen backoff.
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Input: render the TUI from a remote feature stream.
// ---------------------------------------------------------------------------

/// Connection state of the `--input` producer, mapped to [`EngineHealth`] for
/// the TUI. All fields are atomics/locks so the render thread reads them each
/// frame without ever blocking the producer.
struct InputState {
    /// 0 connecting/reconnecting, 1 connected, 2 fatal.
    phase: AtomicU8,
    /// Local ns (since `epoch`) when the current down episode began; `0` while
    /// connected.
    down_since_ns: AtomicU64,
    /// Failed connect attempts in the current down episode.
    attempts: AtomicU64,
    /// Frames published so far (surfaced as `pushes` on the debug line).
    frames: AtomicU64,
    /// Latest activity, for the debug line.
    activity: AtomicU8,
    /// The fatal error (an unrecognised schema), when `phase == 2`.
    fatal: Mutex<Option<String>>,
    epoch: Instant,
}

const PHASE_DOWN: u8 = 0;
const PHASE_UP: u8 = 1;
const PHASE_FATAL: u8 = 2;

impl InputState {
    fn new(epoch: Instant) -> Self {
        Self {
            phase: AtomicU8::new(PHASE_DOWN),
            down_since_ns: AtomicU64::new(1), // down from the start, until first connect
            attempts: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            activity: AtomicU8::new(Activity::Active as u8),
            fatal: Mutex::new(None),
            epoch,
        }
    }

    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn set_connected(&self) {
        self.phase.store(PHASE_UP, Ordering::Release);
        self.down_since_ns.store(0, Ordering::Release);
        self.attempts.store(0, Ordering::Release);
    }

    fn set_down(&self) {
        self.phase.store(PHASE_DOWN, Ordering::Release);
        let now = self.now_ns().max(1);
        let _ = self
            .down_since_ns
            .compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    fn set_fatal(&self, msg: String) {
        *self.fatal.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
        self.phase.store(PHASE_FATAL, Ordering::Release);
    }

    fn note_frame(&self, activity: Activity) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.activity.store(activity as u8, Ordering::Relaxed);
    }

    /// The consumer-facing health for the TUI's `health` closure.
    fn health(&self) -> EngineHealth {
        match self.phase.load(Ordering::Acquire) {
            PHASE_UP => EngineHealth::Ok,
            PHASE_FATAL => EngineHealth::Failed {
                error: self
                    .fatal
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| "remote stream error".to_string()),
            },
            _ => {
                let since = self.down_since_ns.load(Ordering::Acquire);
                let elapsed = self.now_ns().saturating_sub(since);
                EngineHealth::Reconnecting {
                    since_ms: (elapsed / 1_000_000) as u32,
                    attempts: self.attempts.load(Ordering::Relaxed),
                }
            }
        }
    }

    fn stats(&self) -> EngineStats {
        EngineStats {
            pushes: self.frames.load(Ordering::Relaxed),
            activity: match self.activity.load(Ordering::Relaxed) {
                1 => Activity::Quiet,
                2 => Activity::Idle,
                _ => Activity::Active,
            },
            ..EngineStats::default()
        }
    }
}

/// The producer thread: connect, decode, publish; reconnect with bounded
/// backoff on any drop. An unrecognised schema is fatal (it will never become
/// readable) — it flips the shared state to `Failed`, which the TUI surfaces
/// before exiting cleanly. Every other drop is transient and retried while the
/// TUI shows its reconnecting state.
fn input_producer(
    addr: String,
    mut writer: scia_core::FeatureWriter,
    state: Arc<InputState>,
    stop: Arc<AtomicBool>,
) {
    let epoch = state.epoch;
    let mut backoff = BACKOFF_START;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                match FrameStreamReader::new(BufReader::new(stream)) {
                    Ok(mut reader) => {
                        state.set_connected();
                        backoff = BACKOFF_START;
                        loop {
                            if stop.load(Ordering::Acquire) {
                                return;
                            }
                            match reader.next_frame() {
                                Ok(Some(frame)) => {
                                    let mut snap = frame.to_snapshot();
                                    // Restamp to the local receive clock so the
                                    // overlay's frame-age reads correctly across
                                    // machines (the source epoch is meaningless
                                    // here). The generation counter is preserved.
                                    snap.timestamp_ns = epoch.elapsed().as_nanos() as u64;
                                    state.note_frame(snap.activity);
                                    writer.publish(snap);
                                }
                                Ok(None) => break, // clean EOF: reconnect
                                Err(StreamError::UnsupportedSchema { found, expected }) => {
                                    state.set_fatal(format!(
                                        "remote stream schema {found} is not supported (this build speaks {expected})"
                                    ));
                                    return;
                                }
                                Err(err) => {
                                    eprintln!("input: stream read error: {err}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(StreamError::UnsupportedSchema { found, expected }) => {
                        state.set_fatal(format!(
                            "remote stream schema {found} is not supported (this build speaks {expected})"
                        ));
                        return;
                    }
                    Err(StreamError::BadMagic) => {
                        state.set_fatal(
                            "remote peer is not a scia feature stream (bad magic)".to_string(),
                        );
                        return;
                    }
                    Err(err) => {
                        eprintln!("input: handshake error: {err}");
                    }
                }
            }
            Err(err) => {
                eprintln!("input: cannot connect to {addr}: {err}");
            }
        }

        // The connection is down. Mark the episode, settle the scene toward its
        // idle state with a quiet keepalive, and back off before retrying.
        state.set_down();
        writer.publish(idle_keepalive(epoch));
        if sleep_with_stop(&stop, backoff) {
            return;
        }
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
}

/// A quiet keepalive snapshot published while the input is disconnected, so the
/// TUI's scenes decay to their idle state rather than freezing on the last
/// received frame.
fn idle_keepalive(epoch: Instant) -> FeatureSnapshot {
    FeatureSnapshot {
        activity: Activity::Idle,
        timestamp_ns: epoch.elapsed().as_nanos() as u64,
        ..FeatureSnapshot::default()
    }
}

/// Sleep up to `dur`, returning `true` as soon as `stop` is set.
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

/// The caller's handle on a running `--input` session: the closures the TUI's
/// [`scia_tui::run`] polls each frame, and the stop signal. The bus reader is
/// returned separately by [`start_input`] so it can be moved into `run`.
pub struct InputHandle {
    state: Arc<InputState>,
    stop: Arc<AtomicBool>,
}

impl InputHandle {
    /// The `health` closure for [`scia_tui::run`]: `Ok` while connected,
    /// `Reconnecting` while a drop is being ridden out, `Failed` on a fatal
    /// protocol error (an unrecognised schema).
    pub fn health_fn(&self) -> impl FnMut() -> EngineHealth {
        let state = Arc::clone(&self.state);
        move || state.health()
    }

    /// The `stats` closure for [`scia_tui::run`] (frames received, activity).
    pub fn stats_fn(&self) -> impl FnMut() -> EngineStats {
        let state = Arc::clone(&self.state);
        move || state.stats()
    }

    /// The `clock` closure for [`scia_tui::run`] — local ns since the session
    /// epoch, the same clock the producer restamps frames against.
    pub fn clock_fn(&self) -> impl FnMut() -> u64 {
        let state = Arc::clone(&self.state);
        move || state.now_ns()
    }

    /// Signal the producer thread to stop (best-effort; it is detached).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Start an `--input` session: create the local feature bus and spawn the
/// producer that connects to `addr`, decodes the remote stream and publishes
/// snapshots onto it. The returned [`FeatureReader`] is handed to
/// [`scia_tui::run`]; the [`InputHandle`] provides its polling closures.
#[must_use]
pub fn start_input(addr: String) -> (FeatureReader, InputHandle) {
    let epoch = Instant::now();
    let (writer, reader) = feature_bus();
    let state = Arc::new(InputState::new(epoch));
    let stop = Arc::new(AtomicBool::new(false));

    let producer_state = Arc::clone(&state);
    let producer_stop = Arc::clone(&stop);
    thread::Builder::new()
        .name("scia-stream-input".into())
        .spawn(move || input_producer(addr, writer, producer_state, producer_stop))
        .ok();

    (reader, InputHandle { state, stop })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_state_maps_phases_to_health() {
        let state = InputState::new(Instant::now());
        // Down from the start.
        assert!(matches!(state.health(), EngineHealth::Reconnecting { .. }));
        state.set_connected();
        assert_eq!(state.health(), EngineHealth::Ok);
        state.set_fatal("bad schema".to_string());
        assert!(matches!(state.health(), EngineHealth::Failed { .. }));
    }
}
