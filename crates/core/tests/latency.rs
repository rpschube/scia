//! Synthetic end-to-end latency regression (probe P7). Runs the real pipeline
//! through the synthetic click backend — no audio device — and asserts the
//! measured audio→feature latency stays well inside budget. It uses the library
//! types (not the `latency_probe` example), so it exercises exactly what the
//! probe reports.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::capture::{DrainTimeline, RAW_CORR_ACCEPT, drain_into_timeline, rect_xcorr_peak};
use scia_core::{
    CaptureBackend, CaptureTarget, ClickDetector, Detection, Emission, EmitLog, Engine,
    EngineConfig, LatencyStats, Matcher, Pacing, Percentiles, Signal, StreamFormat,
    SyntheticBackend, sample_ring,
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

/// Raw-ring cross-correlation mode (P7 follow-up), end to end through the
/// library types — the CI-testable path for the probe's `--raw-ring --synthetic`
/// mode. It opens the synthetic click backend directly into a probe-local sample
/// ring (no DSP thread), drains it off-thread, and locates each emitted click's
/// leading edge in the captured stream by cross-correlation, with no hop
/// quantization. Because there is no capture hardware and no hop grid, the
/// measured emit → raw-arrival is just the drain/push cadence — a few
/// milliseconds — which proves the correlation + timeline machinery works.
#[test]
fn synthetic_raw_ring_correlation_is_within_budget() {
    let clicks: u32 = 5;
    let spacing_ms: u32 = 200;
    let amp = 0.8f32;
    let click_ms = 1.0f32;

    let epoch = Instant::now();
    let (sink, mut consumer) = sample_ring(epoch);
    let emit_log = Arc::new(EmitLog::new());
    let mut backend = SyntheticBackend {
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
    let stream = backend
        .open(CaptureTarget::SystemMix, sink)
        .expect("synthetic backend opens");
    let format = stream.format();
    consumer.stats().set_channels(format.channels);
    let channels = format.channels.max(1) as usize;
    let sample_rate = format.sample_rate.max(1);

    // Pre-allocate the analysis buffers from the click plan.
    let observe_ms = u64::from(clicks) * u64::from(spacing_ms) + 1000;
    let cap_frames = (((observe_ms + 500) * u64::from(sample_rate)) / 1000) as usize;
    let mut mono: Vec<f32> = Vec::with_capacity(cap_frames);
    let mut scratch: Vec<f32> = Vec::new();
    let mut timeline = DrainTimeline::new(sample_rate);
    timeline.reserve(observe_ms as usize + 16);

    // Drain every 1 ms across the observation window, then one final drain. Each
    // drain is anchored to the capture-delivery clock (see `drain_into_timeline`),
    // so the reconstructed sample times measure ring entry — independent of how
    // coarsely the platform coalesces this poll.
    let mut max_backlog: u64 = 0;
    let deadline = Instant::now() + Duration::from_millis(observe_ms);
    while Instant::now() < deadline {
        max_backlog = max_backlog.max(drain_into_timeline(
            &mut consumer,
            &mut scratch,
            &mut mono,
            &mut timeline,
            channels,
        ));
        sleep(Duration::from_millis(1));
    }
    max_backlog = max_backlog.max(drain_into_timeline(
        &mut consumer,
        &mut scratch,
        &mut mono,
        &mut timeline,
        channels,
    ));

    drop(stream);

    // The unbounded `drain_all` empties the ring each poll, so the shipped path
    // keeps up: the steady-state backlog stays within a couple of chunks (the
    // frames a push can add between the anchor read and the drain), never the
    // hundreds of frames a persistent lag would show. This is the empirical
    // counterpart to the unit-level under-drain proof.
    assert!(
        max_backlog <= 4 * 256,
        "steady-state ring backlog {max_backlog} frames — the drain is not keeping up"
    );

    let mut emissions: Vec<Emission> = Vec::new();
    emit_log.drain(&mut emissions);
    emissions.sort_by_key(|e| e.emit_ns);

    let click_frames = ((click_ms * sample_rate as f32 / 1000.0).ceil() as usize).max(1);
    let half_spacing_ns = u64::from(spacing_ms) / 2 * 1_000_000;

    let mut arrivals_ms: Vec<f32> = Vec::new();
    for e in &emissions {
        // Search a full spacing wide, centered on the emission: the click's true
        // arrival is after emit_ns for real transport, but on the near-zero-
        // transport synthetic path the pre-push emit stamp can precede the
        // sample's (continuous-capture) modeled time by a few ms, so the window
        // reaches back half a spacing. A neighbour click is a full spacing away
        // and cannot intrude.
        let lo = timeline.frame_at_or_after(e.emit_ns.saturating_sub(half_spacing_ns)) as usize;
        let hi = timeline.frame_at_or_after(e.emit_ns + half_spacing_ns) as usize;
        if let Some((offset, peak)) = rect_xcorr_peak(&mono, click_frames, lo, hi) {
            if peak >= RAW_CORR_ACCEPT {
                let arrival = timeline.sample_time_ns(offset as u64).unwrap_or(e.emit_ns);
                arrivals_ms.push((arrival as i64 - e.emit_ns as i64) as f32 / 1.0e6);
            }
        }
    }

    let matched = arrivals_ms.len() as u32;
    let emitted = emissions.len() as u32;
    let pct = Percentiles::nearest_rank(arrivals_ms.clone());
    println!(
        "raw-ring: clicks {emitted} · matched {matched} · \
         emit→raw-arrival median {:.2} ms (min {:.2}, p95 {:.2}, max {:.2})",
        pct.median, pct.min, pct.p95, pct.max
    );

    assert!(
        emitted >= clicks,
        "expected at least {clicks} emitted clicks, got {emitted}"
    );
    // The impulse's correlation is unambiguous against synthetic silence, so
    // every emitted click should correlate; require the same ≥80 % the probe's
    // exit code enforces.
    assert!(
        u64::from(matched) * 100 >= u64::from(emitted) * 80,
        "only {matched}/{emitted} clicks correlated (< 80%)"
    );
    // No hardware and no hop grid: with the drain anchored to the capture-
    // delivery clock (`last_push_ns`, not the poll's read), raw-arrival collapses
    // to a click's sub-chunk offset — roughly −(chunk − k) frame-periods, i.e. a
    // few ms and slightly negative because the synthetic backend stamps emit_ns at
    // the chunk push while the leading-edge sample precedes it within the chunk
    // (see the probe doc). Crucially this no longer carries the probe's poll-lag,
    // so the bound holds regardless of how coarsely the platform coalesces the
    // 1 ms drain sleep. A loose two-sided bound proves the machinery without being
    // flaky on shared runners.
    assert!(
        pct.median.abs() < 10.0,
        "median emit→raw-arrival {:.2} ms should be within ±10 ms of zero",
        pct.median
    );
}

/// Under-drain regression: a drain that runs a constant distance behind the
/// writer (a steady ring backlog) must still place a click at its true capture-
/// delivery time. This is the synthetic reproduction of the field's constant
/// late bias — build a mono stream with a known click, reconstruct its times
/// through a DELIBERATELY lagging drain, and assert the correlation-found arrival
/// lands on delivery once the anchor is corrected for the backlog. Without the
/// occupancy correction the same setup reports the click late by the backlog
/// span, which the final assertion pins down so the test would have caught it.
#[test]
fn synthetic_under_drain_backlog_stays_anchored_to_delivery() {
    let rate = 48_000u32;
    let npf = 1.0e9 / f64::from(rate);
    let fpp = 480u64; // writer packet: 480 frames / 10 ms
    let period_ns = (fpp as f64 * npf) as u64;
    let click_frames = 48usize; // ~1 ms click template

    // The writer delivers `packets` packets. A click burst sits at a known global
    // frame; its true delivery time is the push time of the packet carrying it.
    let packets = 40u64;
    let click_frame = 12_000u64; // inside packet 25 (12000 / 480)
    let click_packet = click_frame / fpp;
    let true_delivery_ns = (click_packet + 1) * period_ns;

    // Build the mono stream: silence with a rectangular click burst.
    let total_frames = (packets * fpp) as usize;
    let mut mono = vec![0.0f32; total_frames];
    for s in &mut mono[click_frame as usize..click_frame as usize + click_frames] {
        *s = 0.8;
    }

    // Reconstruct times through a lagging drain: the drain pops one packet per
    // poll, but the writer stays LAG packets ahead, so LAG*fpp frames are always
    // owed. Corrected and uncorrected timelines run in parallel.
    const LAG: u64 = 3;
    let mut corrected = DrainTimeline::new(rate);
    let mut uncorrected = DrainTimeline::new(rate);
    for k in 0..packets {
        let last_push_ns = (k + 1) * period_ns; // caught-up delivery clock
        // With the writer LAG ahead, the drain reads a last_push_ns that is LAG
        // packets newer and leaves LAG*fpp frames in the ring.
        let lag_push_ns = last_push_ns + LAG * period_ns;
        corrected.record_drain_with_backlog(lag_push_ns, fpp, LAG * fpp);
        uncorrected.record_drain(lag_push_ns, fpp);
    }

    // Search a wide window around the true delivery and correlate the click.
    let lo = corrected.frame_at_or_after(true_delivery_ns.saturating_sub(50_000_000)) as usize;
    let hi = corrected.frame_at_or_after(true_delivery_ns + 50_000_000) as usize;
    let (offset, peak) = rect_xcorr_peak(&mono, click_frames, lo, hi).expect("a peak");
    assert!(
        peak >= RAW_CORR_ACCEPT,
        "click should correlate (ncc {peak})"
    );
    assert!(peak <= 1.0, "ncc {peak} must not exceed 1.0");

    let arrival = corrected
        .sample_time_ns(offset as u64)
        .expect("arrival time");
    let err_ms = (arrival as i64 - true_delivery_ns as i64) as f64 / 1.0e6;
    // One frame period of quantization plus the click's own sub-packet offset.
    assert!(
        err_ms.abs() < 11.0,
        "corrected arrival {arrival} ns is {err_ms:.2} ms off the true delivery \
         {true_delivery_ns} ns — the backlog anchor did not pin it"
    );

    // The uncorrected anchor reports the same click late by ~the backlog span,
    // proving the correction is what removes the constant bias.
    let arrival_bad = uncorrected
        .sample_time_ns(offset as u64)
        .expect("uncorrected time");
    let late_ms = (arrival_bad as i64 - arrival as i64) as f64 / 1.0e6;
    let backlog_span_ms = (LAG * fpp) as f64 * npf / 1.0e6;
    assert!(
        (late_ms - backlog_span_ms).abs() < 1.0,
        "uncorrected arrival should be late by the backlog span {backlog_span_ms:.2} ms, \
         was {late_ms:.2} ms"
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
