//! Round-trip and buffer-reuse coverage for the run-record writer.

use std::collections::BTreeMap;

use scia_telemetry::record::{Event, Hop, Record, RecordWriter, RunEnd, RunStart, SCHEMA};

/// Write one of every record kind, then parse each JSONL line back into the
/// typed [`Record`] structs and assert an exact round-trip.
#[test]
fn every_record_kind_round_trips_through_jsonl() {
    let mut params = BTreeMap::new();
    params.insert("intensity".to_string(), 0.75);
    params.insert("zoom".to_string(), 2.0);

    let records = vec![
        Record::RunStart(RunStart {
            schema: SCHEMA,
            scene: "aurora".to_string(),
            preset: Some("presets/warm.toml".to_string()),
            params,
            source: "clip-042".to_string(),
            hop_ms: 5.333_333,
        }),
        Record::Hop(Hop {
            t_ms: 0.0,
            rms: 0.1,
            bands: vec![1.0, 0.5, 0.2],
            onset: 0.0,
            beat_conf: None,
            bpm: None,
            canvas: None,
        }),
        Record::Hop(Hop {
            t_ms: 5.333_333,
            rms: 0.42,
            bands: vec![1.2, 0.9, 0.3],
            onset: 0.6,
            beat_conf: Some(0.77),
            bpm: Some(128.0),
            canvas: None,
        }),
        Record::Event(Event {
            t_ms: 10.0,
            kind: "scene_swap".to_string(),
            detail: serde_json::json!({ "from": "aurora", "to": "spectra" }),
        }),
        Record::RunEnd(RunEnd {
            t_ms: 128.5,
            hops: 2,
        }),
    ];

    let mut writer = RecordWriter::new(Vec::<u8>::new());
    for r in &records {
        writer.write(r).expect("write record");
    }
    writer.flush().expect("flush");
    let bytes = writer.into_inner();
    let text = String::from_utf8(bytes).expect("utf8");

    let parsed: Vec<Record> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Record::from_line(l).expect("parse line"))
        .collect();

    assert_eq!(
        parsed, records,
        "records survive a JSONL round-trip exactly"
    );
}

/// Unknown fields are ignored, not rejected — a forward-compatible reader.
#[test]
fn unknown_fields_are_tolerated() {
    let line = r#"{"rec":"hop","t_ms":1.0,"rms":0.5,"bands":[1.0,1.0,1.0],"onset":0.1,"beat_conf":null,"bpm":null,"canvas":null,"future_field":123}"#;
    let rec = Record::from_line(line).expect("parse with an unknown field");
    match rec {
        Record::Hop(h) => assert_eq!(h.rms, 0.5),
        other => panic!("expected a hop, got {other:?}"),
    }
}

/// The reusable serialization buffer must not grow across many records of the
/// same shape once it has reached that shape.
#[test]
fn buffer_does_not_grow_across_many_same_shape_records() {
    let mut writer = RecordWriter::new(Vec::<u8>::new());
    let mut hop = Hop {
        t_ms: 0.0,
        rms: 0.3,
        bands: vec![1.0, 0.5, 0.25],
        onset: 0.2,
        beat_conf: Some(0.5),
        bpm: Some(120.0),
        canvas: None,
    };

    // Warm up: after a few writes the buffer has reached the hop-line size.
    for i in 0..8 {
        hop.t_ms = i as f64;
        writer.hop(&hop).expect("write");
    }
    let settled = writer.buffer_capacity();

    for i in 8..10_000 {
        hop.t_ms = i as f64;
        writer.hop(&hop).expect("write");
    }
    assert_eq!(
        writer.buffer_capacity(),
        settled,
        "the serialization buffer must not grow after settling to the record shape"
    );
}
