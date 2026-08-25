//! The **frozen** run-record schema v1: the JSON-Lines wire form a scene replay
//! (this harness) and the live app both emit, one object per line.
//!
//! A sibling branch implements the identical schema in a `crates/telemetry`
//! writer; these serde structs are the local mirror the harness serialises with
//! until the orchestrator swaps the shared crate in at merge. The names and
//! field types here are the contract — do not rename or retype them.
//!
//! # Records
//!
//! Every line is one [`Record`], tagged by its `rec` field:
//!
//! * `run_start` — [`RunStart`]: the run's scene, preset, params, source and hop
//!   cadence.
//! * `hop` — [`Hop`]: one hop of the feature stream, plus the [`Canvas`] stats
//!   this harness derives from the scene's display list. The live app writes
//!   `canvas: null`; this harness fills it.
//! * `event` — [`Event`]: a timestamped, named side-event with a free-form
//!   `detail` object (an onset, a scene notice, …).
//! * `run_end` — [`RunEnd`]: the closing marker with the final hop count.
//!
//! # Tolerance
//!
//! Readers must tolerate unknown fields — `#[serde(deny_unknown_fields)]` is
//! forbidden here — so a newer producer can add fields without breaking an older
//! reader.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The schema version stamped on every [`RunStart`].
pub const RECORD_SCHEMA: u32 = 1;

/// One line of a run-record stream, tagged by `rec`.
///
/// Serialises with the `rec` discriminator first, then the variant's fields
/// flattened alongside it (serde internally-tagged representation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rec")]
pub enum Record {
    /// The opening record of a run.
    #[serde(rename = "run_start")]
    RunStart(RunStart),
    /// One hop of features plus derived canvas stats.
    #[serde(rename = "hop")]
    Hop(Hop),
    /// A timestamped named side-event.
    #[serde(rename = "event")]
    Event(Event),
    /// The closing record of a run.
    #[serde(rename = "run_end")]
    RunEnd(RunEnd),
}

/// `{"rec":"run_start", ...}` — opens a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunStart {
    /// Schema version; always [`RECORD_SCHEMA`] on emit.
    pub schema: u32,
    /// The scene id being driven.
    pub scene: String,
    /// The preset name or path, or `None` for a bare scene.
    pub preset: Option<String>,
    /// The scene parameters in effect, keyed by name.
    pub params: BTreeMap<String, f64>,
    /// Where the features came from: a clip id, `live`, or `synthetic`.
    pub source: String,
    /// The hop cadence in milliseconds.
    pub hop_ms: f32,
}

/// `{"rec":"hop", ...}` — one hop of the feature stream and its canvas stats.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hop {
    /// Milliseconds since the run start.
    pub t_ms: f64,
    /// Hop RMS level (`0.0..=1.0` for in-range audio).
    pub rms: f32,
    /// Bass / mid / treble band levels.
    pub bands: Vec<f32>,
    /// Onset envelope value for this hop (the normalised spectral flux).
    pub onset: f32,
    /// Beat-tracker confidence, or `None` when not available.
    pub beat_conf: Option<f32>,
    /// Estimated tempo (BPM), or `None` when unlocked/unavailable.
    pub bpm: Option<f32>,
    /// Display-list stats this harness derives, or `None` (the live app writes
    /// `null`; this harness fills it).
    pub canvas: Option<Canvas>,
}

/// Per-hop stats derived from a scene's [`scia_scenes::Canvas`] display list.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Canvas {
    /// Number of primitives drawn this frame.
    pub prims: u32,
    /// Fraction of the canvas area touched by primitives (`0.0..=1.0`).
    pub coverage: f32,
    /// Frame-to-frame motion energy (mean absolute per-cell brightness change).
    pub motion: f32,
    /// Mean drawn brightness over the canvas (`0.0..=1.0`).
    pub brightness: f32,
    /// Colourfulness of the intensity-weighted mean drawn colour (`0.0..=1.0`).
    pub chroma: f32,
}

/// `{"rec":"event", ...}` — a timestamped named side-event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Milliseconds since the run start.
    pub t_ms: f64,
    /// A `snake_case` event kind.
    pub kind: String,
    /// Free-form structured detail.
    pub detail: serde_json::Value,
}

/// `{"rec":"run_end", ...}` — closes a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEnd {
    /// Milliseconds since the run start.
    pub t_ms: f64,
    /// Total hops emitted.
    pub hops: u64,
}

/// Serialise one record to a single JSON line (no trailing newline).
///
/// # Errors
/// Propagates a [`serde_json`] serialisation error (does not occur for a
/// well-formed record).
pub fn to_line(rec: &Record) -> Result<String, serde_json::Error> {
    serde_json::to_string(rec)
}

/// Parse one JSON line into a [`Record`].
///
/// # Errors
/// Propagates a [`serde_json`] parse error.
pub fn from_line(line: &str) -> Result<Record, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_json_lines() {
        let mut params = BTreeMap::new();
        params.insert("release".to_string(), 0.5);
        params.insert("punch".to_string(), 1.25);
        let records = vec![
            Record::RunStart(RunStart {
                schema: RECORD_SCHEMA,
                scene: "spectra".to_string(),
                preset: Some("presets/spectra.toml".to_string()),
                params,
                source: "synth-music".to_string(),
                hop_ms: 5.333_333,
            }),
            Record::Hop(Hop {
                t_ms: 5.3,
                rms: 0.42,
                bands: vec![1.0, 0.5, 0.25],
                onset: 0.7,
                beat_conf: Some(0.9),
                bpm: Some(112.0),
                canvas: Some(Canvas {
                    prims: 64,
                    coverage: 0.31,
                    motion: 0.02,
                    brightness: 0.4,
                    chroma: 0.6,
                }),
            }),
            Record::Event(Event {
                t_ms: 5.3,
                kind: "onset".to_string(),
                detail: serde_json::json!({"flux": 0.7}),
            }),
            Record::RunEnd(RunEnd {
                t_ms: 10.6,
                hops: 2,
            }),
        ];
        for rec in &records {
            let line = to_line(rec).expect("encode");
            assert!(!line.contains('\n'), "a record is one line");
            let back = from_line(&line).expect("decode");
            assert_eq!(&back, rec);
        }
    }

    #[test]
    fn run_start_line_is_tagged_and_ordered() {
        let rec = Record::RunStart(RunStart {
            schema: RECORD_SCHEMA,
            scene: "spectra".to_string(),
            preset: None,
            params: BTreeMap::new(),
            source: "synthetic".to_string(),
            hop_ms: 5.0,
        });
        let line = to_line(&rec).unwrap();
        assert!(
            line.starts_with(r#"{"rec":"run_start","schema":1,"#),
            "got {line}"
        );
        assert!(line.contains(r#""preset":null"#));
    }

    #[test]
    fn canvas_null_line_decodes() {
        // The live app writes canvas:null; a reader must accept it.
        let line = r#"{"rec":"hop","t_ms":1.0,"rms":0.1,"bands":[0.0,0.0,0.0],"onset":0.0,"beat_conf":null,"bpm":null,"canvas":null}"#;
        let rec = from_line(line).expect("decode");
        match rec {
            Record::Hop(h) => assert!(h.canvas.is_none()),
            other => panic!("expected hop, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // A newer producer adds a field an older reader does not know.
        let line = r#"{"rec":"run_end","t_ms":9.0,"hops":3,"extra":"ignored"}"#;
        let rec = from_line(line).expect("decode");
        assert_eq!(rec, Record::RunEnd(RunEnd { t_ms: 9.0, hops: 3 }));
    }
}
