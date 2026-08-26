//! Cross-crate round-trip: a run record produced by the harness serialises and
//! parses back through the shared `scia-telemetry` read-side types, with the
//! per-hop canvas both filled (as the harness renders it) and null (as the live
//! app writes it in `--log-run` mode).

use scia_harness::metrics::MetricParams;
use scia_harness::replay::{RunRequest, run};
use scia_harness::synth::{SynthSpec, synth_spec};
use scia_telemetry::record::{Hop, Record, to_line};

/// A short deterministic clip so the test stays fast.
fn short_clip() -> SynthSpec {
    SynthSpec {
        duration_s: 3.0,
        ..*synth_spec("synth-music").unwrap()
    }
}

#[test]
fn harness_records_parse_through_shared_read_side_with_filled_canvas() {
    let spec = short_clip();
    let frames = spec.frames();
    let req = RunRequest {
        scene: "spectra",
        preset: None,
        preset_label: None,
        sets: &[],
        frames: &frames,
        source: "synth-music",
        hop_ms: spec.hop_ms(),
        metric_params: MetricParams::default(),
    };
    let out = run(&req);

    // Each harness-written line parses back through the shared `Record` read-side
    // and is byte-for-byte the same record.
    let mut hops_with_canvas = 0usize;
    for rec in &out.records {
        let line = to_line(rec).expect("encode");
        assert!(!line.contains('\n'), "a record is one line");
        let back = Record::from_line(&line).expect("decode");
        assert_eq!(
            &back, rec,
            "record did not round-trip through the shared types"
        );
        if let Record::Hop(h) = &back {
            if h.canvas.is_some() {
                hops_with_canvas += 1;
            }
        }
    }
    assert!(
        hops_with_canvas > 0,
        "the harness fills the per-hop canvas, so at least one hop must carry it"
    );
}

#[test]
fn hop_with_null_canvas_round_trips_through_shared_read_side() {
    // The live app writes `canvas: null`; the shared read-side must accept it and
    // preserve the absence across a round-trip.
    let hop = Hop {
        t_ms: 1.0,
        rms: 0.1,
        loudness: None,
        bands: vec![0.0, 0.0, 0.0],
        onset: 0.0,
        beat_conf: None,
        bpm: None,
        canvas: None,
    };
    let line = to_line(&Record::Hop(hop.clone())).expect("encode");
    assert!(line.contains(r#""canvas":null"#), "got {line}");

    let back = Record::from_line(&line).expect("decode");
    match back {
        Record::Hop(h) => {
            assert!(h.canvas.is_none(), "null canvas must decode as None");
            assert_eq!(h, hop, "hop did not round-trip");
        }
        other => panic!("expected hop, got {other:?}"),
    }
}
