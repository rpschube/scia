//! Determinism: two `run` invocations on the same clip + scene + params produce
//! identical metrics and identical run records.

use scia_harness::metrics::MetricParams;
use scia_harness::replay::{RunRequest, run};
use scia_harness::synth::{SynthSpec, synth_spec};
use scia_telemetry::record::to_line;

fn short_clip() -> SynthSpec {
    SynthSpec {
        duration_s: 3.0,
        ..*synth_spec("synth-music").unwrap()
    }
}

fn run_once(scene: &str) -> (String, String) {
    let spec = short_clip();
    let frames = spec.frames();
    let req = RunRequest {
        scene,
        preset: None,
        preset_label: None,
        sets: &[("release".to_string(), 0.4)],
        frames: &frames,
        source: "synth-music",
        hop_ms: spec.hop_ms(),
        metric_params: MetricParams::default(),
    };
    let out = run(&req);
    let metrics_json = serde_json::to_string_pretty(&out.metrics).unwrap();
    let records: String = out
        .records
        .iter()
        .map(|r| to_line(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    (metrics_json, records)
}

#[test]
fn two_runs_produce_identical_metrics_and_records() {
    let (m1, r1) = run_once("spectra");
    let (m2, r2) = run_once("spectra");
    assert_eq!(m1, m2, "metrics.json differed between runs");
    assert_eq!(r1, r2, "run records differed between runs");
}

#[test]
fn a_field_scene_is_also_deterministic() {
    let (m1, _) = run_once("aurora");
    let (m2, _) = run_once("aurora");
    assert_eq!(m1, m2);
}
