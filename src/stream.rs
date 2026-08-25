//! Feature-stream I/O for the `scia` binary (US-UX-2).
//!
//! The output-serving half — the headless `--output` loop that paces the
//! engine's live feature bus onto stdout or a `--listen` socket — lives in
//! [`scia_core::stream::run_output`], shared with the `scia-bridge` companion so
//! the listener / fan-out / rate pacing is written once. It is re-exported here
//! ([`run_output`], [`DEFAULT_STREAM_RATE`]) so the binary's call sites resolve
//! through this module unchanged.
//!
//! This module owns the inverse, UI-facing path, in two shapes that share the
//! same producer-thread seam and the same decode ([`FrameStreamReader`]):
//!
//! * [`start_input`] — a producer thread connects to a remote `--output
//!   --listen` socket, decodes frames and publishes them onto a local feature
//!   bus exactly where the synthetic backend's generator would — so the whole
//!   TUI (scenes, chrome, overlays) renders from the remote stream, none the
//!   wiser. A dropped connection is ridden out with a bounded backoff while the
//!   TUI shows its normal reconnecting/quiet state; it never freezes or exits on
//!   a blip.
//! * [`start_input_file`] — the same seam fed from a recorded clip file on disk
//!   (a `scia --output binary > clip.bin` capture) rather than a socket. Frames
//!   are published paced by the clip's own inter-frame timestamps, so it renders
//!   like live audio rather than as fast as the file reads. At end of file it
//!   publishes the quiet keepalive and shows the same idle state as a
//!   disconnected socket; with `loop_replay` it seamlessly restarts for extended
//!   A/B listening.

use std::fs::File;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::PathBuf;
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
                        // No address in the message (privacy: no LAN addresses in
                        // log messages) — just the connect/disconnect edges.
                        tracing::info!(target: "scia::stream", "feature stream connected");
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
        tracing::info!(target: "scia::stream", "feature stream disconnected; reconnecting");
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

// ---------------------------------------------------------------------------
// Input: render the TUI from a recorded clip file on disk.
// ---------------------------------------------------------------------------

/// The largest real-time gap the file replay will pace between two consecutive
/// clip frames from their own timestamps. A recorded idle stretch is written at
/// the keepalive cadence (a frame every 500 ms), which paces fine; a larger
/// source gap — a long pause during the original capture, or a clock artefact —
/// is clamped so a single boundary cannot stall the replay for its whole span.
const MAX_REPLAY_GAP: Duration = Duration::from_secs(1);

/// How often the post-EOF idle loop republishes the quiet keepalive, so the
/// scene stays decayed to idle rather than freezing on the last clip frame,
/// until the session stops.
const EOF_KEEPALIVE_PERIOD: Duration = Duration::from_millis(500);

/// The file-replay producer thread: open the clip, decode it and publish each
/// frame onto the local bus paced by the clip's own inter-frame timestamps
/// (real-time playback, not as fast as the file reads). A schema this build does
/// not speak, a bad magic, or a file that cannot be opened is fatal (it will
/// never become readable) — it flips the shared state to `Failed`, which the TUI
/// surfaces before exiting cleanly. A clean end of file settles the scene toward
/// idle with the quiet keepalive and shows the same "down" state as a
/// disconnected socket; with `loop_replay` the clip is instead reopened and
/// replayed from the top for seamless extended listening.
fn file_producer(
    path: PathBuf,
    loop_replay: bool,
    mut writer: scia_core::FeatureWriter,
    state: Arc<InputState>,
    stop: Arc<AtomicBool>,
) {
    let epoch = state.epoch;
    // Each pass replays the whole clip; with `loop_replay` we reopen and start
    // over at a clean EOF for uninterrupted A/B listening.
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                // No path in the message (privacy: no filesystem paths in log
                // messages) — the CLI already named the file on the way in.
                state.set_fatal(format!("cannot open clip file: {err}"));
                return;
            }
        };
        let mut reader = match FrameStreamReader::new(BufReader::new(file)) {
            Ok(reader) => reader,
            Err(StreamError::UnsupportedSchema { found, expected }) => {
                state.set_fatal(format!(
                    "clip schema {found} is not supported (this build speaks {expected})"
                ));
                return;
            }
            Err(StreamError::BadMagic) => {
                state.set_fatal("file is not a scia feature stream (bad magic)".to_string());
                return;
            }
            Err(err) => {
                state.set_fatal(format!("cannot read clip header: {err}"));
                return;
            }
        };

        state.set_connected();
        // Pace by the clip's own inter-frame timestamps so it plays at the
        // cadence it was recorded (real time). The first frame of each pass is
        // published immediately; each later frame waits the source gap.
        let mut prev_src_ts: Option<u64> = None;
        let mut next = Instant::now();
        let mut clean_eof = false;
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match reader.next_frame() {
                Ok(Some(frame)) => {
                    let mut snap = frame.to_snapshot();
                    let src_ts = snap.timestamp_ns;
                    if let Some(prev) = prev_src_ts {
                        let gap =
                            Duration::from_nanos(src_ts.saturating_sub(prev)).min(MAX_REPLAY_GAP);
                        next += gap;
                        let now = Instant::now();
                        if next > now {
                            if sleep_with_stop(&stop, next - now) {
                                return;
                            }
                        } else {
                            // Fell behind the schedule (a long clamp boundary or a
                            // slow reader): resync rather than burst to catch up.
                            next = now;
                        }
                    }
                    prev_src_ts = Some(src_ts);
                    // Restamp to the local receive clock so the overlay's
                    // frame-age reads correctly (the source epoch is meaningless
                    // here); the generation counter is preserved.
                    snap.timestamp_ns = epoch.elapsed().as_nanos() as u64;
                    state.note_frame(snap.activity);
                    writer.publish(snap);
                }
                Ok(None) => {
                    clean_eof = true;
                    break;
                }
                Err(StreamError::UnsupportedSchema { found, expected }) => {
                    state.set_fatal(format!(
                        "clip schema {found} is not supported (this build speaks {expected})"
                    ));
                    return;
                }
                Err(err) => {
                    // A truncated or corrupt frame ends this pass early; treat it
                    // like EOF (settle to idle) rather than a fatal protocol
                    // error, and do not loop on it — a re-read would recur.
                    eprintln!("input: clip read error: {err}");
                    break;
                }
            }
        }

        // Seamlessly restart only on a clean end of file, so a mid-clip
        // corruption cannot spin the replay.
        if loop_replay && clean_eof && !stop.load(Ordering::Acquire) {
            continue;
        }
        break;
    }

    // End of the clip (no loop, or a corrupt tail): settle the scene toward idle
    // with the quiet keepalive and surface the same "down" state the socket path
    // shows on a disconnect, republishing the keepalive so the scene stays idle
    // rather than freezing on the last received frame — until the session stops.
    tracing::info!(target: "scia::stream", "clip replay reached end of file");
    state.set_down();
    while !stop.load(Ordering::Acquire) {
        writer.publish(idle_keepalive(epoch));
        if sleep_with_stop(&stop, EOF_KEEPALIVE_PERIOD) {
            return;
        }
    }
}

/// Start a file-replay `--input` session: create the local feature bus and spawn
/// the producer that decodes `path` and publishes its frames onto the bus, paced
/// by the clip's own cadence. `loop_replay` restarts the clip at each clean end
/// of file. The returned [`FeatureReader`] is handed to [`scia_tui::run`]; the
/// [`InputHandle`] provides its polling closures, exactly as [`start_input`].
#[must_use]
pub fn start_input_file(path: PathBuf, loop_replay: bool) -> (FeatureReader, InputHandle) {
    let epoch = Instant::now();
    let (writer, reader) = feature_bus();
    let state = Arc::new(InputState::new(epoch));
    let stop = Arc::new(AtomicBool::new(false));

    let producer_state = Arc::clone(&state);
    let producer_stop = Arc::clone(&stop);
    thread::Builder::new()
        .name("scia-stream-file".into())
        .spawn(move || file_producer(path, loop_replay, writer, producer_state, producer_stop))
        .ok();

    (reader, InputHandle { state, stop })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scia_core::stream::{FeatureFrame, write_binary_frame, write_binary_header};
    use std::io::Write;

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

    /// A clip frame with a given identity: `generation`, source `timestamp_ns`,
    /// an `rms` tied to the generation so an observer can verify content, and an
    /// activity. Everything else is default-ish but valid.
    fn clip_frame(
        generation: u64,
        timestamp_ns: u64,
        rms: f32,
        activity: Activity,
    ) -> FeatureFrame {
        FeatureFrame {
            schema: scia_core::STREAM_SCHEMA_VERSION,
            generation,
            timestamp_ns,
            sample_rate: 48_000,
            channels: 2,
            starved: false,
            activity,
            quiet_ms: 0.0,
            dropped_frames: 0,
            rms,
            peak: rms,
            lufs_momentary: 0.0,
            spectrum: vec![0.1, 0.2, 0.3, 0.4],
            bands: [1.0, 0.5, 0.25],
            flux: 0.1,
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

    /// Write a binary clip (the `--output binary` wire form) to a fresh temp file
    /// and return its path.
    fn write_clip(tag: &str, frames: &[FeatureFrame]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "scia-clip-{tag}-{}-{}.bin",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let mut file = File::create(&path).expect("create clip");
        let mut buf = Vec::new();
        write_binary_header(&mut buf).expect("header");
        for frame in frames {
            write_binary_frame(&mut buf, frame).expect("frame");
        }
        file.write_all(&buf).expect("write clip");
        file.flush().expect("flush clip");
        path
    }

    /// Poll `reader.latest()` until `pred` returns `Some`, or `timeout` elapses.
    fn poll_until<T>(
        reader: &mut FeatureReader,
        timeout: Duration,
        mut pred: impl FnMut(&FeatureSnapshot) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = pred(reader.latest()) {
                return Some(v);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A recorded clip replays onto the bus: frames arrive with their recorded
    /// generations and content, paced by the clip's own cadence (not instantly),
    /// and a clean EOF settles to the quiet keepalive (idle activity + the
    /// disconnected/reconnecting health).
    #[test]
    fn file_replay_streams_frames_then_settles_to_keepalive_at_eof() {
        // Six frames 10 ms apart; rms == generation as a tenth, so an observed
        // snapshot's rms identifies which frame it is.
        let gap_ns: u64 = 10_000_000;
        let frames: Vec<FeatureFrame> = (0..6u64)
            .map(|g| clip_frame(g, g * gap_ns, g as f32 * 0.1, Activity::Active))
            .collect();
        let path = write_clip("eof", &frames);

        let start = Instant::now();
        let (mut reader, handle) = start_input_file(path.clone(), false);

        // Watch the bus until the EOF keepalive arrives (an `idle` snapshot),
        // recording the distinct active clip frames seen along the way. The
        // active frames are the clip's content; the keepalive is `idle` (and the
        // last clip generation may be overwritten by it before a poll catches it,
        // so the run's end is detected by the keepalive, not by the last gen).
        let mut seen: Vec<(u64, f32)> = Vec::new();
        let reached_eof = poll_until(&mut reader, Duration::from_secs(2), |snap| {
            match snap.activity {
                Activity::Active => {
                    if seen.last().map(|&(g, _)| g) != Some(snap.generation) {
                        seen.push((snap.generation, snap.rms));
                    }
                    None
                }
                Activity::Idle => Some(()),
                Activity::Quiet => None,
            }
        });
        let elapsed = start.elapsed();
        assert!(
            reached_eof.is_some(),
            "never reached the EOF keepalive; saw {seen:?}"
        );

        // Content: every observed clip frame carried its matching rms, and most
        // of the clip flowed through in order.
        for &(g, rms) in &seen {
            assert!(
                (rms - g as f32 * 0.1).abs() < 1e-4,
                "generation {g} arrived with wrong rms {rms}"
            );
        }
        assert!(seen.len() >= 4, "expected most of the clip, saw {seen:?}");
        let max_gen = seen.iter().map(|&(g, _)| g).max().unwrap_or(0);
        assert!(max_gen >= 4, "clip did not play through, saw {seen:?}");
        // Cadence: reaching EOF took a real fraction of the recorded span (six
        // frames 10 ms apart ≈ 50 ms) — it was paced, not dumped as fast as read.
        assert!(
            elapsed >= Duration::from_millis(30),
            "replay was not paced (reached EOF in {elapsed:?})"
        );

        // EOF: the health shows the disconnected/reconnecting state the TUI
        // already renders (the quiet keepalive that decayed the scene to idle was
        // the `idle` snapshot that ended the poll above).
        let mut health = handle.health_fn();
        assert!(
            matches!(health(), EngineHealth::Reconnecting { .. }),
            "EOF should surface the disconnected/reconnecting state"
        );

        handle.stop();
        std::fs::remove_file(&path).ok();
    }

    /// With `loop_replay`, a clip seamlessly restarts at EOF: after the last
    /// generation the producer reopens and republishes from the first, so the
    /// observed generation wraps back down at least once.
    #[test]
    fn file_replay_loops_and_wraps_at_least_once() {
        let gap_ns: u64 = 10_000_000;
        let frames: Vec<FeatureFrame> = (0..3u64)
            .map(|g| clip_frame(g, g * gap_ns, g as f32 * 0.1, Activity::Active))
            .collect();
        let path = write_clip("loop", &frames);

        let (mut reader, handle) = start_input_file(path.clone(), true);

        // Record generations; a wrap is a generation strictly below the running
        // max we have already seen.
        let mut max_seen: Option<u64> = None;
        let mut last: Option<u64> = None;
        let wrapped = poll_until(&mut reader, Duration::from_secs(3), |snap| {
            let g = snap.generation;
            if last != Some(g) {
                last = Some(g);
                let wrap = matches!(max_seen, Some(m) if g < m);
                max_seen = Some(max_seen.map_or(g, |m| m.max(g)));
                if wrap {
                    return Some(());
                }
            }
            None
        });
        assert!(
            wrapped.is_some(),
            "loop replay never wrapped (max seen {max_seen:?})"
        );

        handle.stop();
        std::fs::remove_file(&path).ok();
    }

    /// A file whose header is not a scia stream (a binary-looking first byte but
    /// the wrong magic) is a fatal replay error the TUI surfaces as `Failed`,
    /// never a silent hang.
    #[test]
    fn file_replay_rejects_a_non_clip_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("scia-notclip-{}.bin", std::process::id()));
        // First byte `S` routes to the binary reader, but the magic is wrong, so
        // the header is rejected up front (BadMagic → fatal).
        std::fs::write(&path, b"SCIXbogusheaderbytes").expect("write junk");

        let (_reader, handle) = start_input_file(path.clone(), false);
        let mut health = handle.health_fn();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if matches!(health(), EngineHealth::Failed { .. }) {
                break;
            }
            assert!(Instant::now() < deadline, "junk file never became Failed");
            thread::sleep(Duration::from_millis(2));
        }

        handle.stop();
        std::fs::remove_file(&path).ok();
    }
}
