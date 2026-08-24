//! Silence-pipeline integration tests: the decay → idle-downshift state machine
//! (US-CAP-3, and the near-zero-idle mechanism for US-PERF-3).
//!
//! A scripted capture backend feeds the engine a timeline of tone / delivered
//! silence / starvation segments, real-time paced, so a test can script
//! "1 s of tone → 6 s of zeros → tone again" or "1 s of tone → stop delivering".
//! Both silence forms drive the same `Active → Quiet → Idle` machine, and the
//! tests observe it through `EngineStats`/`FeatureSnapshot` without a CPU meter.
//!
//! Timing tolerances are generous: these tests run real threads under the OS
//! scheduler, so they assert on windows, not exact instants.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

use scia_core::capture::{CaptureBackend, CaptureError, CaptureStream, CaptureTarget, SampleSink};
use scia_core::{
    Activity, DspConfig, Engine, EngineConfig, FeatureReader, FeatureSnapshot, HopProcessor,
    StreamFormat, sample_ring,
};

const SR: u32 = 48_000;
const HOP: usize = 256;
const CH: usize = 2;
const FORMAT: StreamFormat = StreamFormat {
    sample_rate: SR,
    channels: 2,
};

/// One hop of audio, seconds.
fn dt() -> f32 {
    HOP as f32 / SR as f32
}

// ---------------------------------------------------------------------------
// Scripted backend: its producer thread pushes whatever the mpsc channel hands
// it. Delivering nothing (a `Starve` segment) exercises capture starvation;
// delivering zeros exercises delivered silence.
// ---------------------------------------------------------------------------

struct ScriptedBackend {
    format: StreamFormat,
    rx: Option<Receiver<Vec<f32>>>,
}

impl CaptureBackend for ScriptedBackend {
    fn open(
        &mut self,
        _target: CaptureTarget,
        mut sink: SampleSink,
    ) -> Result<Box<dyn CaptureStream>, CaptureError> {
        let rx = self.rx.take().expect("backend opened once");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("scripted-feed".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(chunk) => sink.push(&chunk),
                        // No data right now: leave the DSP thread to starve.
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        Ok(Box::new(ScriptedStream {
            format: self.format,
            stop,
            handle: Some(handle),
        }))
    }
}

struct ScriptedStream {
    format: StreamFormat,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureStream for ScriptedStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
}

impl Drop for ScriptedStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A timeline segment for the feeder.
#[derive(Clone, Copy, Debug)]
enum Seg {
    /// Deliver a `hz`/`amp` sine for `dur`, real-time paced.
    Tone { hz: f32, amp: f32, dur: Duration },
    /// Deliver zeros for `dur`, real-time paced (the Linux "delivered silence").
    Silence { dur: Duration },
    /// Deliver nothing for `dur` (the Windows-loopback "starvation").
    Starve { dur: Duration },
}

fn tone(secs: f32) -> Seg {
    Seg::Tone {
        hz: 1_000.0,
        amp: 0.5,
        dur: Duration::from_secs_f32(secs),
    }
}
fn silence(secs: f32) -> Seg {
    Seg::Silence {
        dur: Duration::from_secs_f32(secs),
    }
}
fn starve(secs: f32) -> Seg {
    Seg::Starve {
        dur: Duration::from_secs_f32(secs),
    }
}

/// Spawn a feeder thread that plays `script` on `tx`, real-time paced. It also
/// records the wall-clock start instant of each segment into the returned
/// `marks` vector, so a test can time events relative to a segment boundary.
fn spawn_feeder(
    tx: Sender<Vec<f32>>,
    format: StreamFormat,
    script: Vec<Seg>,
) -> (JoinHandle<()>, Arc<Mutex<Vec<Instant>>>) {
    let marks = Arc::new(Mutex::new(Vec::new()));
    let marks_thread = Arc::clone(&marks);
    let handle = thread::spawn(move || {
        let channels = format.channels as usize;
        let sr = f64::from(format.sample_rate);
        let hop_period = Duration::from_secs_f64(HOP as f64 / sr);
        let mut buf = vec![0.0f32; HOP * channels];
        let mut frame: u64 = 0;

        for seg in script {
            marks_thread.lock().unwrap().push(Instant::now());
            match seg {
                Seg::Starve { dur } => sleep(dur),
                Seg::Tone { hz, amp, dur } => {
                    let chunks = (dur.as_secs_f64() * sr / HOP as f64).round() as u64;
                    let start = Instant::now();
                    for i in 0..chunks {
                        for f in 0..HOP {
                            let t = (frame + f as u64) as f64 / sr;
                            let s = (f64::from(amp)
                                * (2.0 * std::f64::consts::PI * f64::from(hz) * t).sin())
                                as f32;
                            buf[f * channels] = s;
                            if channels > 1 {
                                buf[f * channels + 1] = s;
                            }
                        }
                        if tx.send(buf.clone()).is_err() {
                            return;
                        }
                        frame += HOP as u64;
                        pace(start, hop_period, i + 1);
                    }
                }
                Seg::Silence { dur } => {
                    let chunks = (dur.as_secs_f64() * sr / HOP as f64).round() as u64;
                    let start = Instant::now();
                    buf.fill(0.0);
                    for i in 0..chunks {
                        if tx.send(buf.clone()).is_err() {
                            return;
                        }
                        frame += HOP as u64;
                        pace(start, hop_period, i + 1);
                    }
                }
            }
        }
    });
    (handle, marks)
}

/// Sleep until `start + period * n` so the feeder tracks real time without
/// cumulative drift.
fn pace(start: Instant, period: Duration, n: u64) {
    let target = start + period * (n as u32);
    let now = Instant::now();
    if target > now {
        sleep(target - now);
    }
}

/// Default config but with a 1 s `idle_after`, so wake-rate / resume / decay
/// tests do not have to wait the full 4 s to reach `Idle`.
fn fast_idle_config() -> EngineConfig {
    EngineConfig {
        dsp: DspConfig {
            idle_after: Duration::from_secs(1),
            ..DspConfig::default()
        },
        ..EngineConfig::default()
    }
}

/// Peak display bar of a snapshot.
fn peak_bar(snap: &FeatureSnapshot) -> f32 {
    snap.spectrum[..snap.spectrum_len as usize]
        .iter()
        .copied()
        .fold(0.0f32, f32::max)
}

/// Poll `reader` every 2 ms until `pred` holds, returning the instant it first
/// did (and the snapshot), or `None` on timeout.
fn wait_until(
    reader: &mut FeatureReader,
    timeout: Duration,
    pred: impl Fn(&FeatureSnapshot) -> bool,
) -> Option<(Instant, FeatureSnapshot)> {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = *reader.latest();
        if pred(&snap) {
            return Some((Instant::now(), snap));
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(2));
    }
}

/// Start an engine on a scripted backend, returning the engine, reader, feeder
/// handle, the segment-marks, and the reference instant (just before feeding).
fn start_scripted(
    config: EngineConfig,
    script: Vec<Seg>,
) -> (
    Engine,
    FeatureReader,
    JoinHandle<()>,
    Arc<Mutex<Vec<Instant>>>,
) {
    let (tx, rx) = channel::<Vec<f32>>();
    let backend = ScriptedBackend {
        format: FORMAT,
        rx: Some(rx),
    };
    let (engine, reader) = Engine::start(Box::new(backend), config).expect("engine start");
    let (feeder, marks) = spawn_feeder(tx, FORMAT, script);
    (engine, reader, feeder, marks)
}

// ---------------------------------------------------------------------------
// Test 1: delivered silence reaches Idle within 5 s.
// ---------------------------------------------------------------------------

#[test]
fn delivered_silence_reaches_idle_within_5s() {
    // Default thresholds: Quiet after 0.5 s, Idle after 4 s.
    let script = vec![tone(1.0), silence(6.0)];
    let (engine, mut reader, feeder, _marks) = start_scripted(EngineConfig::default(), script);
    let silence_start = Instant::now() + Duration::from_secs(1); // tone is 1 s

    // Confirm signal is seen first.
    assert!(
        wait_until(&mut reader, Duration::from_millis(800), |s| s.activity
            == Activity::Active)
        .is_some(),
        "never reached Active while the tone played"
    );

    let (t_quiet, quiet_snap) = wait_until(&mut reader, Duration::from_secs(3), |s| {
        s.activity == Activity::Quiet
    })
    .expect("never reached Quiet");
    let (t_idle, idle_snap) = wait_until(&mut reader, Duration::from_secs(4), |s| {
        s.activity == Activity::Idle
    })
    .expect("never reached Idle");

    let quiet_after = t_quiet
        .saturating_duration_since(silence_start)
        .as_secs_f32();
    let idle_after = t_idle
        .saturating_duration_since(silence_start)
        .as_secs_f32();
    assert!(
        (0.3..=1.1).contains(&quiet_after),
        "Quiet at {quiet_after:.2}s of silence, want ~0.5s"
    );
    assert!(
        (3.5..=5.0).contains(&idle_after),
        "Idle at {idle_after:.2}s of silence, want <5s"
    );
    assert!(
        idle_snap.quiet_ms > quiet_snap.quiet_ms && quiet_snap.quiet_ms > 0.0,
        "quiet_ms did not grow: quiet {} -> idle {}",
        quiet_snap.quiet_ms,
        idle_snap.quiet_ms
    );

    engine.stop();
    let _ = feeder.join();
}

// ---------------------------------------------------------------------------
// Test 2: starvation reaches Idle within 5 s.
// ---------------------------------------------------------------------------

#[test]
fn starvation_reaches_idle_within_5s() {
    let script = vec![tone(1.0), starve(6.0)];
    let (engine, mut reader, feeder, _marks) = start_scripted(EngineConfig::default(), script);
    let silence_start = Instant::now() + Duration::from_secs(1);

    assert!(
        wait_until(&mut reader, Duration::from_millis(800), |s| s.activity
            == Activity::Active)
        .is_some(),
        "never reached Active while the tone played"
    );

    let (t_quiet, quiet_snap) = wait_until(&mut reader, Duration::from_secs(3), |s| {
        s.activity == Activity::Quiet
    })
    .expect("never reached Quiet");
    let (t_idle, idle_snap) = wait_until(&mut reader, Duration::from_secs(4), |s| {
        s.activity == Activity::Idle
    })
    .expect("never reached Idle");

    let quiet_after = t_quiet
        .saturating_duration_since(silence_start)
        .as_secs_f32();
    let idle_after = t_idle
        .saturating_duration_since(silence_start)
        .as_secs_f32();
    assert!(
        (0.3..=1.1).contains(&quiet_after),
        "Quiet at {quiet_after:.2}s of starvation, want ~0.5s"
    );
    assert!(
        (3.5..=5.0).contains(&idle_after),
        "Idle at {idle_after:.2}s of starvation, want <5s"
    );
    assert!(
        idle_snap.quiet_ms > quiet_snap.quiet_ms && quiet_snap.quiet_ms > 0.0,
        "quiet_ms did not grow: quiet {} -> idle {}",
        quiet_snap.quiet_ms,
        idle_snap.quiet_ms
    );

    engine.stop();
    let _ = feeder.join();
}

// ---------------------------------------------------------------------------
// Test 3: the idle wake rate is low (and the active rate is high).
// ---------------------------------------------------------------------------

#[test]
fn idle_wake_rate_is_low() {
    // Shorten idle_after so the test does not have to wait the full 4 s.
    let script = vec![tone(2.0), starve(3.0)];
    let (engine, mut reader, feeder, _marks) = start_scripted(fast_idle_config(), script);

    // --- Active wake rate: measured over 1 s while the tone plays. ---
    assert!(
        wait_until(&mut reader, Duration::from_millis(800), |s| s.activity
            == Activity::Active)
        .is_some(),
        "never reached Active"
    );
    sleep(Duration::from_millis(200)); // settle into steady Active
    let active_start = engine.stats().dsp_wakes;
    sleep(Duration::from_secs(1));
    let active_wakes = engine.stats().dsp_wakes - active_start;

    // --- Idle wake rate: measured over 1 s once Idle. ---
    wait_until(&mut reader, Duration::from_secs(3), |s| {
        s.activity == Activity::Idle
    })
    .expect("never reached Idle");
    sleep(Duration::from_millis(100));
    let idle_start = engine.stats().dsp_wakes;
    sleep(Duration::from_secs(1));
    let idle_wakes = engine.stats().dsp_wakes - idle_start;

    assert!(
        active_wakes >= 150,
        "active wake rate too low: {active_wakes} in 1 s (want >= 150)"
    );
    assert!(
        idle_wakes <= 30,
        "idle wake rate too high: {idle_wakes} in 1 s (want <= 30)"
    );

    engine.stop();
    let _ = feeder.join();
}

// ---------------------------------------------------------------------------
// Test 4: the spectrum decays smoothly into idle (no freeze, no flicker).
// ---------------------------------------------------------------------------

#[test]
fn spectrum_decays_smoothly_into_idle() {
    let script = vec![tone(1.0), silence(3.0)];
    let (engine, mut reader, feeder, _marks) = start_scripted(fast_idle_config(), script);

    // Let the tone settle, confirm a lively bar, then sample from silence start.
    assert!(
        wait_until(&mut reader, Duration::from_millis(800), |s| peak_bar(s)
            > 0.3)
        .is_some(),
        "the tone never produced a lively bar"
    );
    // Silence begins ~1 s after feeding started; wait out the remaining tone.
    let silence_start = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < silence_start {
        sleep(Duration::from_millis(5));
    }

    // Sample the peak bar every 50 ms until Idle + 1 s (idle_after 1 s → ~2 s).
    let mut samples = Vec::new();
    let end = Instant::now() + Duration::from_millis(2100);
    while Instant::now() < end {
        samples.push(peak_bar(reader.latest()));
        sleep(Duration::from_millis(50));
    }

    // Monotone non-increasing (small epsilon for float / AGC ripple), and never
    // a one-step crash to zero from a lively level.
    let mut max_rise = 0.0f32;
    let mut worst_drop_from = 0.0f32;
    for w in samples.windows(2) {
        let (a, b) = (w[0], w[1]);
        max_rise = max_rise.max(b - a);
        if a > 0.2 && b < 0.02 {
            worst_drop_from = worst_drop_from.max(a);
        }
    }
    assert!(
        max_rise <= 0.01,
        "spectrum rose during silence by {max_rise:.4} (should only decay)"
    );
    assert!(
        worst_drop_from == 0.0,
        "spectrum jumped to ~0 in one step from {worst_drop_from:.3} (flicker)"
    );
    let last = *samples.last().unwrap();
    assert!(
        last < 0.02,
        "spectrum did not reach a quiescent floor: ended at {last:.4}"
    );

    engine.stop();
    let _ = feeder.join();
}

// ---------------------------------------------------------------------------
// Test 5: resume reanimates within 100 ms.
// ---------------------------------------------------------------------------

#[test]
fn resume_within_100ms() {
    // Tone, then long-enough silence to reach Idle, then tone again (segment 2).
    let script = vec![tone(0.5), silence(2.5), tone(1.5)];
    let (engine, mut reader, feeder, marks) = start_scripted(fast_idle_config(), script);

    // Confirm we actually reach Idle before the resume.
    wait_until(&mut reader, Duration::from_secs(3), |s| {
        s.activity == Activity::Idle
    })
    .expect("never reached Idle before resume");

    // The feeder records each segment's start; the resume tone is segment index 2.
    let resume_at = loop {
        if let Some(&t) = marks.lock().unwrap().get(2) {
            break t;
        }
        sleep(Duration::from_millis(2));
    };

    let (t_active, _) = wait_until(&mut reader, Duration::from_millis(500), |s| {
        s.activity == Activity::Active
    })
    .expect("never re-activated after resume");
    let (t_bar, _) = wait_until(&mut reader, Duration::from_millis(500), |s| {
        peak_bar(s) > 0.3
    })
    .expect("bar never re-animated after resume");

    let flip_ms = t_active.saturating_duration_since(resume_at).as_secs_f32() * 1000.0;
    let bar_ms = t_bar.saturating_duration_since(resume_at).as_secs_f32() * 1000.0;
    assert!(
        flip_ms <= 100.0,
        "activity flip took {flip_ms:.1} ms (want <= 100)"
    );
    assert!(
        bar_ms <= 250.0,
        "bar re-animation took {bar_ms:.1} ms (want <= 250)"
    );

    engine.stop();
    let _ = feeder.join();
}

// ---------------------------------------------------------------------------
// Test 6: the cheap idle path honours the same release time constants.
// ---------------------------------------------------------------------------

#[test]
fn process_idle_matches_release_curve() {
    let (mut sink_r, mut con_r) = sample_ring(Instant::now());
    let (mut sink_i, mut con_i) = sample_ring(Instant::now());
    let mut normal = HopProcessor::new(HOP, 2, SR);
    let mut idle = HopProcessor::new(HOP, 2, SR);
    let mut buf = vec![0.0f32; HOP * CH];
    let mut frame = 0u64;

    // Settle an identical tone, then clear the FFT history with identical
    // normal-path silence, on BOTH processors. After the clear the analyzer
    // windows are fully zeroed (target 0) — the state in which the real pipeline
    // ever runs `process_idle`. From here the only difference is the path taken.
    let settle = (1.0 / dt()) as usize;
    for _ in 0..settle {
        for f in 0..HOP {
            let t = (frame + f as u64) as f64 / f64::from(SR);
            let s = (0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()) as f32;
            buf[f * CH] = s;
            buf[f * CH + 1] = s;
        }
        sink_r.push(&buf);
        normal.try_process(&mut con_r, FORMAT, 0, 0);
        sink_i.push(&buf);
        idle.try_process(&mut con_i, FORMAT, 0, 0);
        frame += HOP as u64;
    }
    buf.fill(0.0);
    // > fft_bass / HOP hops of silence clears both FFT windows.
    for _ in 0..40 {
        sink_r.push(&buf);
        normal.try_process(&mut con_r, FORMAT, 0, 0);
        sink_i.push(&buf);
        idle.try_process(&mut con_i, FORMAT, 0, 0);
    }

    // Now T of silence: reference via the full path, comparison via process_idle.
    let n = 60usize;
    let mut ref_bar = 0.0;
    let mut idle_bar = 0.0;
    for _ in 0..n {
        sink_r.push(&buf);
        ref_bar = peak_bar(&normal.try_process(&mut con_r, FORMAT, 0, 0).unwrap());
        sink_i.push(&buf);
        idle_bar = peak_bar(&idle.process_idle(&mut con_i, FORMAT, 0, 0, 0.001).unwrap());
    }

    let tol = 0.05 * ref_bar.max(1e-4);
    assert!(
        (ref_bar - idle_bar).abs() <= tol,
        "idle decay {idle_bar:.6} strayed from the normal-path curve {ref_bar:.6} (tol {tol:.6})"
    );
}
