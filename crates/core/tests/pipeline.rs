//! Pipeline integration tests. All run with no audio stack present.

use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{
    Engine, EngineConfig, FEATURE_SCHEMA_VERSION, FeatureSnapshot, HopProcessor, Pacing,
    RING_FRAMES, Signal, StreamFormat, SyntheticBackend, sample_ring,
};

const STEREO_48K: StreamFormat = StreamFormat {
    sample_rate: 48_000,
    channels: 2,
};

/// Push three hops of a constant pattern through a sink and drain them with the
/// per-hop processor. Generation increments per hop; rms/peak are exact.
#[test]
fn sink_push_and_hop_drain() {
    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let hop = 256usize;
    let channels = 2usize;
    let mut processor = HopProcessor::new(hop, 2, 48_000);

    for expected_gen in 1..=3 {
        let pattern = vec![0.25f32; hop * channels];
        sink.push(&pattern);
        let snapshot = processor
            .try_process(&mut consumer, STEREO_48K, 0, 0)
            .expect("a full hop should be available");
        assert_eq!(snapshot.generation, expected_gen);
        assert!(!snapshot.starved);
        // Constant 0.25 on both channels: mono mix 0.25, rms 0.25, peak 0.25.
        assert!((snapshot.rms - 0.25).abs() < 1e-6, "rms = {}", snapshot.rms);
        assert!(
            (snapshot.peak - 0.25).abs() < 1e-6,
            "peak = {}",
            snapshot.peak
        );
    }
}

/// A 1 kHz sine at amplitude 0.5 through the real engine has the theoretical
/// rms (0.5/√2) and peak (0.5).
#[test]
fn sine_rms_matches_theory() {
    let backend = SyntheticBackend {
        format: STEREO_48K,
        signal: Signal::Sine {
            hz: 1_000.0,
            amp: 0.5,
        },
        pacing: Pacing::Unpaced {
            total_frames: 48_000,
        },
    };
    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    let start = Instant::now();
    let mut last_real: Option<FeatureSnapshot> = None;
    while reader.generation() < 100 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for hops (gen = {})",
            reader.generation()
        );
        let snapshot = *reader.latest();
        if !snapshot.starved && snapshot.rms > 0.0 {
            last_real = Some(snapshot);
        }
        sleep(Duration::from_millis(2));
    }
    // One more read after crossing the threshold in case the loop exited on a
    // starved snapshot.
    let snapshot = *reader.latest();
    if !snapshot.starved && snapshot.rms > 0.0 {
        last_real = Some(snapshot);
    }

    let snapshot = last_real.expect("a non-starved snapshot");
    let expected_rms = 0.5 / 2.0f32.sqrt();
    assert!(
        (snapshot.rms - expected_rms).abs() <= expected_rms * 0.02,
        "rms {} not within 2% of {}",
        snapshot.rms,
        expected_rms
    );
    assert!(
        (snapshot.peak - 0.5).abs() <= 0.5 * 0.01,
        "peak {} not within 1% of 0.5",
        snapshot.peak
    );

    engine.stop();
}

/// Once the synthetic source stops delivering, the hop grid keeps advancing
/// with synthesized silence.
#[test]
fn starvation_synthesizes_silence() {
    let backend = SyntheticBackend {
        format: STEREO_48K,
        signal: Signal::Sine {
            hz: 440.0,
            amp: 0.5,
        },
        pacing: Pacing::Unpaced { total_frames: 2048 },
    };
    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    // Wait past the gap timeout (100 ms) plus a few hops.
    sleep(Duration::from_millis(300));
    let first = *reader.latest();
    assert!(first.starved, "expected a starved snapshot");
    assert_eq!(first.rms, 0.0, "starved rms must be 0");

    sleep(Duration::from_millis(120));
    let second = *reader.latest();
    assert!(second.starved, "still starved");
    assert!(
        second.generation > first.generation,
        "generation should keep climbing while starved: {} -> {}",
        first.generation,
        second.generation
    );

    engine.stop();
}

/// Overflowing the ring in a single push drops the excess, counts it, and
/// returns promptly without blocking.
#[test]
fn overflow_counts_drops_without_blocking() {
    let (mut sink, consumer) = sample_ring(Instant::now());
    let capacity_samples = RING_FRAMES * 2;
    let payload = vec![0.1f32; capacity_samples * 4];

    let started = Instant::now();
    sink.push(&payload);
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(50),
        "push blocked for {elapsed:?}"
    );
    assert!(
        sink.stats().dropped_frames.load(Ordering::Relaxed) > 0,
        "overflow must count dropped frames"
    );
    assert_eq!(
        consumer.buffered_samples(),
        capacity_samples,
        "ring should hold exactly its capacity"
    );
}

/// The snapshot layout stays small and self-describing.
#[test]
fn snapshot_layout() {
    assert!(std::mem::size_of::<FeatureSnapshot>() <= 2048);
    let default = FeatureSnapshot::default();
    assert_eq!(default.schema_version, FEATURE_SCHEMA_VERSION);
    assert_eq!(default.spectrum_len, 0);
}

/// The engine starts and stops cleanly and quickly.
#[test]
fn engine_stops_cleanly() {
    let backend = SyntheticBackend {
        format: STEREO_48K,
        signal: Signal::Silence,
        pacing: Pacing::Realtime,
    };
    let started = Instant::now();
    let (engine, _reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");
    sleep(Duration::from_millis(50));
    engine.stop();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "start+stop took too long"
    );
}

#[test]
fn push_never_splits_frames() {
    use std::sync::atomic::Ordering::Relaxed;
    // Stereo ring: pushes that do not fit whole must never write a partial
    // frame, or every later hop would read L/R swapped.
    let (mut sink, consumer) = scia_core::sample_ring(std::time::Instant::now());
    sink.stats().set_channels_for_test(2);
    let capacity = sink.free_samples();
    sink.push(&vec![0.25f32; capacity - 4]);
    assert_eq!(sink.free_samples(), 4);

    // 3 samples = one whole frame + a trailing partial frame: the partial is discarded.
    sink.push(&[1.0, 2.0, 3.0]);
    assert_eq!(sink.free_samples(), 2, "only the whole frame was written");
    assert_eq!(
        sink.stats().dropped_frames.load(Relaxed),
        0,
        "a partial frame is not a dropped frame"
    );

    // 4 samples into 2 free: one frame written, one whole frame dropped.
    sink.push(&[5.0, 6.0, 7.0, 8.0]);
    assert_eq!(sink.free_samples(), 0);
    assert_eq!(sink.stats().dropped_frames.load(Relaxed), 1);
    assert_eq!(
        sink.stats().pushed_frames.load(Relaxed) as usize,
        capacity / 2
    );
    assert_eq!(consumer.buffered_samples(), capacity);
}
