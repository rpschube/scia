//! End-to-end: replay a synthetic clip through a bars scene and a field scene,
//! assert the metrics are finite and sane, that the run-record JSONL round-trips
//! through the serde structs, and that `corpus verify` passes for a committed
//! fixture.

use scia_harness::corpus::{ClipEntry, Manifest, verify};
use scia_harness::hash::sha256_hex;
use scia_harness::metrics::MetricParams;
use scia_harness::records::{Record, from_line, to_line};
use scia_harness::replay::{RunRequest, run};
use scia_harness::synth::{SynthSpec, synth_spec};

/// A short deterministic clip so the tests stay fast.
fn short_clip() -> SynthSpec {
    SynthSpec {
        duration_s: 3.0,
        ..*synth_spec("synth-music").unwrap()
    }
}

fn replay_scene(scene: &str) -> scia_harness::replay::RunOutput {
    let spec = short_clip();
    let frames = spec.frames();
    let req = RunRequest {
        scene,
        preset: None,
        preset_label: None,
        sets: &[],
        frames: &frames,
        source: "synth-music",
        hop_ms: spec.hop_ms(),
        metric_params: MetricParams::default(),
    };
    run(&req)
}

#[test]
fn spectra_replay_produces_sane_metrics() {
    let out = replay_scene("spectra");
    let m = out.metrics;
    assert!(m.all_finite(), "metrics not finite: {m:?}");
    assert!(out.hops > 100, "expected many hops, got {}", out.hops);

    // Correlations are in range.
    for r in [m.loudness_motion_r, m.loudness_brightness_r] {
        assert!((-1.0..=1.0).contains(&r), "correlation out of range: {r}");
    }
    // Coverage is a fraction; spectra draws bars, so it touches *something*.
    assert!(
        m.coverage_mean > 0.0 && m.coverage_mean <= 1.0,
        "coverage_mean {}",
        m.coverage_mean
    );
    assert!((0.0..=1.0).contains(&m.coverage_p95));
    assert!((0.0..=1.0).contains(&m.flicker), "flicker {}", m.flicker);
    assert!(
        m.onset_response_latency_ms >= 0.0,
        "latency {}",
        m.onset_response_latency_ms
    );
    assert!(m.palette_churn >= 0.0);
}

#[test]
fn field_scene_replay_produces_sane_metrics() {
    // `aurora` is a field-type scene (it emits Primitive::Field), so it should
    // cover a large fraction of the canvas.
    let out = replay_scene("aurora");
    let m = out.metrics;
    assert!(m.all_finite(), "metrics not finite: {m:?}");
    assert!(
        m.coverage_mean > 0.25,
        "a field scene should fill much of the canvas, got {}",
        m.coverage_mean
    );
}

#[test]
fn run_records_round_trip_through_serde() {
    let out = replay_scene("spectra");

    // First record is run_start; last is run_end; the rest are hops/events.
    assert!(matches!(out.records.first(), Some(Record::RunStart(_))));
    assert!(matches!(out.records.last(), Some(Record::RunEnd(_))));

    for rec in &out.records {
        let line = to_line(rec).expect("encode");
        assert!(!line.contains('\n'));
        let back = from_line(&line).expect("decode");
        assert_eq!(&back, rec, "record did not round-trip");
    }

    // Every hop carries a filled canvas (this harness never writes canvas:null).
    let hops_with_canvas = out
        .records
        .iter()
        .filter(|r| matches!(r, Record::Hop(h) if h.canvas.is_some()))
        .count();
    assert!(hops_with_canvas > 0);
}

#[test]
fn corpus_verify_passes_for_a_committed_fixture() {
    let tmp = std::env::temp_dir().join(format!("scia-harness-e2e-{}", std::process::id()));
    let corpus_root = tmp.join("corpus");
    std::fs::create_dir_all(corpus_root.join("clips")).unwrap();

    // A tiny committed clip fixture.
    let bytes = short_clip().encode_ndjson();
    let sha = sha256_hex(&bytes);
    std::fs::write(corpus_root.join("clips/tiny.ndjson"), &bytes).unwrap();

    let mut manifest = Manifest::default();
    manifest.upsert(ClipEntry {
        id: "tiny".to_string(),
        genre: "synthetic".to_string(),
        path: "clips/tiny.ndjson".to_string(),
        duration_s: 3.0,
        sha256: sha,
        notes: "e2e fixture".to_string(),
        generated: false,
    });
    manifest.save(&corpus_root.join("manifest.toml")).unwrap();

    let results = verify(&corpus_root).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].ok, "verify failed: {}", results[0].detail);

    let _ = std::fs::remove_dir_all(&tmp);
}
