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

/// Observer poll interval. Committed at 1 ms; temporarily set to 10 ms to
/// reproduce the macOS timer-coalescing half-coverage regime locally (a
/// one-line change that must still pass — see the miss-tolerance comment).
const POLL_SLEEP: Duration = Duration::from_millis(1);

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
    let mut coverage = Coverage::default();
    poll(
        &engine,
        &mut reader,
        &mut detector,
        &mut detections,
        &mut coverage,
        observe,
    );
    poll(
        &engine,
        &mut reader,
        &mut detector,
        &mut detections,
        &mut coverage,
        flush,
    );

    engine.stop();

    let mut emissions: Vec<Emission> = Vec::new();
    emit_log.drain(&mut emissions);

    let matched = Matcher::new(half_spacing_ns).match_events(&emissions, &detections);
    let stats = LatencyStats::from_matched(&matched);

    let coverage = coverage.fraction();

    // Surface the real numbers in CI logs.
    println!(
        "synthetic latency: clicks {} · matched {} · missed {} · spurious {} · coverage {:.2}",
        emissions.len(),
        stats.count,
        stats.missed,
        stats.spurious,
        coverage
    );
    print!("{stats}");

    assert!(
        emissions.len() >= clicks as usize,
        "expected at least {clicks} emitted clicks, got {}",
        emissions.len()
    );
    // A synthetic click clears the detector threshold for only a single hop
    // (~5.3 ms at 256 frames / 48 kHz), so whether a click is observed at all
    // rides on the observer sampling that one hop. When the platform coalesces
    // the observer's short sleeps — macOS timers round up to ~10 ms — the
    // observer samples only about every other hop and sees roughly half the
    // clicks. That is a sampling-granularity artifact, not a pipeline dropping
    // clicks. (Windows shows a milder version; it reproduces on Linux by
    // setting POLL_SLEEP to 10 ms.) So the miss tolerance is calibrated from
    // the observer's own measured coverage — observed hops over the generation
    // span they cover — rather than fixed: allowed = ceil((1 − coverage) ×
    // emitted) + 2. At full coverage this collapses to the original +2 slack,
    // so a pipeline genuinely dropping clicks while the observer samples every
    // hop still fails. The latency bound below is enforced over every click
    // that did match.
    let allowed_missed = ((1.0 - coverage) * emissions.len() as f64).ceil() as u32 + 2;
    assert!(
        matched.missed <= allowed_missed,
        "{} clicks missed — exceeds the {allowed_missed} that {coverage:.2} observation \
         coverage explains",
        matched.missed
    );
    assert_eq!(matched.spurious, 0, "no spurious detection should appear");
    // Guard against a vacuous pass: the observer must have covered a real share
    // of the hops, and enough clicks must have matched to characterize latency.
    assert!(
        coverage > 0.2,
        "observation coverage {coverage:.2} is degenerate — the observer saw almost nothing"
    );
    assert!(
        stats.count >= 6,
        "only {} clicks matched — too few to characterize latency",
        stats.count
    );
    // Generous for shared CI runners; the expected value is ~6–10 ms
    // (a 256-frame chunk + one hop + 1 ms poll).
    assert!(
        stats.emit_to_observe.median < 40.0,
        "median emit→observe {} ms should be < 40 ms",
        stats.emit_to_observe.median
    );
}

/// Tracks how much of the published hop stream the observer actually sampled.
///
/// `coverage` is the number of distinct hop generations the observer saw over
/// the generation span they cover (`last − first + 1`). Full coverage (every
/// hop sampled) is `1.0`; sampling every other hop — the macOS timer-coalescing
/// regime — is about `0.5`.
#[derive(Default)]
struct Coverage {
    observed: u64,
    first_gen: Option<u64>,
    last_gen: Option<u64>,
}

impl Coverage {
    /// Record a newly observed hop generation. Ignores an immediate repeat of
    /// the last generation (e.g. the same hop seen across the two poll calls).
    fn observe(&mut self, generation: u64) {
        if self.last_gen == Some(generation) {
            return;
        }
        self.observed += 1;
        self.first_gen.get_or_insert(generation);
        self.last_gen = Some(generation);
    }

    /// Observed hops over the generation span they cover, clamped to `0.0..=1.0`.
    /// Degenerate (no observation) is `0.0`.
    fn fraction(&self) -> f64 {
        match (self.first_gen, self.last_gen) {
            (Some(first), Some(last)) => {
                let span = last.saturating_sub(first) + 1;
                (self.observed as f64 / span as f64).clamp(0.0, 1.0)
            }
            _ => 0.0,
        }
    }
}

/// Poll `reader` every `POLL_SLEEP` for `duration`, feeding fresh snapshots to
/// the detector and recording observation coverage.
fn poll(
    engine: &Engine,
    reader: &mut scia_core::FeatureReader,
    detector: &mut ClickDetector,
    detections: &mut Vec<Detection>,
    coverage: &mut Coverage,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    let mut last_gen: Option<u64> = None;
    while Instant::now() < deadline {
        let snapshot = *reader.latest();
        if last_gen != Some(snapshot.generation) {
            last_gen = Some(snapshot.generation);
            coverage.observe(snapshot.generation);
            if let Some(d) = detector.observe(&snapshot, engine.now_ns()) {
                detections.push(d);
            }
        }
        sleep(POLL_SLEEP);
    }
}
