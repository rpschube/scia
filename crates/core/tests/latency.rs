//! Synthetic end-to-end latency regression (probe P7). Runs the real pipeline
//! through the synthetic click backend — no audio device — and asserts the
//! measured audio→feature latency stays well inside budget. It uses the library
//! types (not the `latency_probe` example), so it exercises exactly what the
//! probe reports.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{
    ClickDetector, Detection, Emission, EmitLog, Engine, EngineConfig, LatencyStats, Matcher,
    Pacing, Signal, StreamFormat, SyntheticBackend,
};

#[test]
fn synthetic_end_to_end_latency_is_within_budget() {
    let clicks: u32 = 12;
    let spacing_ms: u32 = 400; // 150 bpm
    let amp = 0.8f32;
    let threshold = 0.3f32;

    let emit_log = Arc::new(EmitLog::new());
    let backend = SyntheticBackend {
        format: StreamFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        signal: Signal::Clicks {
            bpm: 60_000.0 / spacing_ms as f32,
            amp,
        },
        pacing: Pacing::Realtime,
        emit_log: Some(Arc::clone(&emit_log)),
    };

    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    let half_spacing_ns = u64::from(spacing_ms) / 2 * 1_000_000;
    let mut detector = ClickDetector::new(threshold, half_spacing_ns);
    let mut detections: Vec<Detection> = Vec::new();

    // Observe for clicks * spacing + 2 s, then a mid-gap flush tail so the last
    // click's hop is observed before we drain.
    let observe = Duration::from_millis(u64::from(clicks) * u64::from(spacing_ms) + 2_000);
    let flush = Duration::from_millis(u64::from(spacing_ms) / 2);
    poll(
        &engine,
        &mut reader,
        &mut detector,
        &mut detections,
        observe,
    );
    poll(&engine, &mut reader, &mut detector, &mut detections, flush);

    engine.stop();

    let mut emissions: Vec<Emission> = Vec::new();
    emit_log.drain(&mut emissions);

    let matched = Matcher::new(half_spacing_ns).match_events(&emissions, &detections);
    let stats = LatencyStats::from_matched(&matched);

    // Surface the real numbers in CI logs.
    println!(
        "synthetic latency: clicks {} · matched {} · missed {} · spurious {}",
        emissions.len(),
        stats.count,
        stats.missed,
        stats.spurious
    );
    print!("{stats}");

    assert!(
        emissions.len() >= clicks as usize,
        "expected at least {clicks} emitted clicks, got {}",
        emissions.len()
    );
    assert_eq!(matched.missed, 0, "no click should be missed");
    assert_eq!(matched.spurious, 0, "no spurious detection should appear");
    assert!(stats.count > 0, "at least one click must match");
    // Generous for shared CI runners; the expected value is ~6–10 ms
    // (a 256-frame chunk + one hop + 1 ms poll).
    assert!(
        stats.emit_to_observe.median < 40.0,
        "median emit→observe {} ms should be < 40 ms",
        stats.emit_to_observe.median
    );
}

/// Poll `reader` every 1 ms for `duration`, feeding fresh snapshots to the
/// detector.
fn poll(
    engine: &Engine,
    reader: &mut scia_core::FeatureReader,
    detector: &mut ClickDetector,
    detections: &mut Vec<Detection>,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    let mut last_gen: Option<u64> = None;
    while Instant::now() < deadline {
        let snapshot = *reader.latest();
        if last_gen != Some(snapshot.generation) {
            last_gen = Some(snapshot.generation);
            if let Some(d) = detector.observe(&snapshot, engine.now_ns()) {
                detections.push(d);
            }
        }
        sleep(Duration::from_millis(1));
    }
}
