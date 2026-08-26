//! The frozen **run-record schema (v1)** and the buffered JSONL [`RecordWriter`].
//!
//! A run record is the machine-readable transcript of one `scia` session,
//! written as JSON Lines (one object per line) by the `--log-run` mode and
//! consumed by the scene-quality harness. The schema is **frozen**: a sibling
//! tool mirrors these structs, so field names and types must not drift. Bump the
//! [`SCHEMA`] constant only alongside a coordinated change on both sides.
//!
//! Every line is one of four record kinds, distinguished by a `"rec"` tag:
//!
//! ```text
//! {"rec":"run_start","schema":1,"scene":"…","preset":"…"|null,"params":{…},"source":"…","hop_ms":…}
//! {"rec":"hop","t_ms":…,"rms":…,"loudness":…?,"bands":[…],"onset":…,"beat_conf":…|null,"bpm":…|null,"canvas":{…}|null}
//! {"rec":"event","t_ms":…,"kind":"…","detail":{…}}
//! {"rec":"run_end","t_ms":…,"hops":…}
//! ```
//!
//! Readers must tolerate unknown fields — none of these structs use
//! `deny_unknown_fields`, so a future field is ignored by an older reader rather
//! than failing the parse.

use std::collections::BTreeMap;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

/// The run-record schema version stamped into every [`RunStart`]. Frozen at 1.
pub const SCHEMA: u32 = 1;

/// The first record of a run: the resolved scene, preset and scalar parameters,
/// the input source, and the nominal hop period.
///
/// `params` is the map of resolved scalar preset parameters (name → number).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStart {
    /// Schema version; always [`SCHEMA`].
    pub schema: u32,
    /// Resolved scene id (e.g. `"spectra"`).
    pub scene: String,
    /// Preset name or file path, or `None` when no preset applied.
    pub preset: Option<String>,
    /// Resolved scalar preset parameters, name → value.
    pub params: BTreeMap<String, f64>,
    /// Where the audio came from: a clip id, `"live"`, or `"synthetic"`.
    pub source: String,
    /// Nominal hop period in milliseconds.
    pub hop_ms: f32,
}

/// Per-scene canvas statistics for a hop. `None` in `--log-run` mode (which
/// records the audio-feature plane only); the harness fills it when it renders.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CanvasStats {
    /// Primitive count drawn this frame.
    pub prims: u32,
    /// Fraction of the canvas covered, `0.0..=1.0`.
    pub coverage: f32,
    /// Frame-to-frame motion metric.
    pub motion: f32,
    /// Mean brightness, `0.0..=1.0`.
    pub brightness: f32,
    /// Chroma / colourfulness metric.
    pub chroma: f32,
}

/// One hop's worth of analysis features (and, when rendered, canvas stats).
///
/// `bands` is the per-band level vector (three bands in schema 1). `beat_conf`
/// and `bpm` are `None` until the beat tracker locks. `canvas` is `None` unless
/// a renderer supplied it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hop {
    /// Milliseconds since the run epoch; monotonic across a run.
    pub t_ms: f64,
    /// Hop RMS level.
    pub rms: f32,
    /// Engine-normalized loudness in `0.0..=1.0` (rms against a slow
    /// auto-reference), or `None` when the source did not supply it. Optional and
    /// defaulted so an older record without the field still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<f32>,
    /// Per-band levels (three in schema 1).
    pub bands: Vec<f32>,
    /// Continuous onset strength for the hop (normalized spectral flux).
    pub onset: f32,
    /// Beat-tracker confidence once locked, else `None`.
    pub beat_conf: Option<f32>,
    /// Estimated tempo in BPM once locked, else `None`.
    pub bpm: Option<f32>,
    /// Canvas statistics when a renderer supplied them, else `None`.
    pub canvas: Option<CanvasStats>,
}

/// A discrete event during the run — a scene or preset swap, a device switch —
/// with a free-form JSON `detail` object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Milliseconds since the run epoch.
    pub t_ms: f64,
    /// Event kind, `snake_case` (e.g. `"scene_swap"`, `"device_switch"`).
    pub kind: String,
    /// Event-specific fields.
    pub detail: serde_json::Value,
}

/// The final record of a run: when it ended and how many hops it carried.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunEnd {
    /// Milliseconds since the run epoch.
    pub t_ms: f64,
    /// Total hop records written this run.
    pub hops: u64,
}

/// One line of a run record. Serializes to a single JSON object carrying the
/// discriminating `"rec"` tag; the payload struct's fields are flattened
/// alongside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rec", rename_all = "snake_case")]
pub enum Record {
    /// A [`RunStart`] line.
    RunStart(RunStart),
    /// A [`Hop`] line.
    Hop(Hop),
    /// An [`Event`] line.
    Event(Event),
    /// A [`RunEnd`] line.
    RunEnd(RunEnd),
}

impl Record {
    /// Parse one JSONL line into a [`Record`], dispatching on the `"rec"` tag.
    ///
    /// # Errors
    /// Returns the underlying `serde_json` error when the line is not a valid
    /// record object.
    pub fn from_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

/// Serialize one [`Record`] to a single JSON line (no trailing newline).
///
/// The string counterpart to [`RecordWriter::write`] for callers that collect a
/// run's records in memory and emit them as one JSONL block rather than
/// streaming: the bytes are identical to a [`RecordWriter`] line without its
/// trailing newline. Round-trips with [`Record::from_line`].
///
/// # Errors
/// Returns the underlying `serde_json` error (does not occur for a well-formed
/// record).
pub fn to_line(record: &Record) -> serde_json::Result<String> {
    serde_json::to_string(record)
}

/// A buffered JSON Lines writer for [`Record`]s.
///
/// Each record is serialized into a single reused byte buffer (cleared, not
/// reallocated, between records — so writing many records of the same shape does
/// not grow the buffer once it has reached that shape), a newline is appended,
/// and the bytes are handed to the wrapped [`Write`]. The wrapped writer should
/// be a [`std::io::BufWriter`] (or the run-record file directly). Call
/// [`flush`](RecordWriter::flush) to push buffered bytes to the OS; the writer
/// also flushes on drop is **not** guaranteed, so flush explicitly at end of run.
pub struct RecordWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> RecordWriter<W> {
    /// Wrap `inner`, allocating a small reusable serialization buffer.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            // Enough for a hop line without an immediate regrow; it settles to
            // the largest record shape's size and is reused thereafter.
            buf: Vec::with_capacity(512),
        }
    }

    /// Serialize and write one record followed by a newline.
    ///
    /// # Errors
    /// Returns any serialization or I/O error.
    pub fn write(&mut self, record: &Record) -> io::Result<()> {
        self.buf.clear();
        serde_json::to_writer(&mut self.buf, record)?;
        self.buf.push(b'\n');
        self.inner.write_all(&self.buf)
    }

    /// Convenience: write a [`Record::RunStart`].
    ///
    /// # Errors
    /// See [`write`](RecordWriter::write).
    pub fn run_start(&mut self, r: RunStart) -> io::Result<()> {
        self.write(&Record::RunStart(r))
    }

    /// Convenience: write a [`Record::Hop`].
    ///
    /// # Errors
    /// See [`write`](RecordWriter::write).
    pub fn hop(&mut self, r: &Hop) -> io::Result<()> {
        // Borrow the hop to avoid moving (and re-allocating) a reused buffer.
        self.buf.clear();
        serde_json::to_writer(
            &mut self.buf,
            &HopLine {
                rec: HopTag::Hop,
                hop: r,
            },
        )?;
        self.buf.push(b'\n');
        self.inner.write_all(&self.buf)
    }

    /// Convenience: write a [`Record::Event`].
    ///
    /// # Errors
    /// See [`write`](RecordWriter::write).
    pub fn event(&mut self, r: Event) -> io::Result<()> {
        self.write(&Record::Event(r))
    }

    /// Convenience: write a [`Record::RunEnd`].
    ///
    /// # Errors
    /// See [`write`](RecordWriter::write).
    pub fn run_end(&mut self, r: RunEnd) -> io::Result<()> {
        self.write(&Record::RunEnd(r))
    }

    /// Flush buffered bytes to the wrapped writer's destination.
    ///
    /// # Errors
    /// Returns any I/O error from the wrapped writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    /// Current capacity of the reusable serialization buffer (for tests).
    #[must_use]
    pub fn buffer_capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Consume the writer and return the wrapped [`Write`].
    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// Serializes a borrowed [`Hop`] with the `{"rec":"hop", …}` tag without moving
/// it, so the hot recording path can keep and reuse one `Hop` (with its `bands`
/// `Vec`) across every hop instead of allocating a fresh one per record.
#[derive(Serialize)]
struct HopLine<'a> {
    rec: HopTag,
    #[serde(flatten)]
    hop: &'a Hop,
}

/// A one-variant tag that serializes as the string `"hop"`.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum HopTag {
    Hop,
}

// The reusable-`Hop` path above hard-codes the tag string; keep it in lockstep
// with the `Record::Hop` variant name at compile time via a round-trip check in
// the unit tests below.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hop() -> Hop {
        Hop {
            t_ms: 12.5,
            rms: 0.25,
            loudness: Some(0.7),
            bands: vec![1.0, 0.5, 0.25],
            onset: 0.3,
            beat_conf: Some(0.8),
            bpm: Some(120.0),
            canvas: None,
        }
    }

    #[test]
    fn hop_convenience_matches_the_tagged_enum() {
        // The borrowed-Hop fast path must produce byte-identical output to the
        // owned `Record::Hop` variant, so the hard-coded tag cannot drift.
        let hop = sample_hop();
        let mut a = RecordWriter::new(Vec::new());
        a.hop(&hop).unwrap();
        let mut b = RecordWriter::new(Vec::new());
        b.write(&Record::Hop(hop)).unwrap();
        assert_eq!(a.into_inner(), b.into_inner());
    }

    #[test]
    fn run_start_line_has_expected_shape() {
        let mut params = BTreeMap::new();
        params.insert("gain".to_string(), 1.5);
        let rs = RunStart {
            schema: SCHEMA,
            scene: "spectra".to_string(),
            preset: None,
            params,
            source: "synthetic".to_string(),
            hop_ms: 5.333,
        };
        let mut w = RecordWriter::new(Vec::new());
        w.run_start(rs).unwrap();
        let line = String::from_utf8(w.into_inner()).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["rec"], "run_start");
        assert_eq!(v["schema"], 1);
        assert_eq!(v["scene"], "spectra");
        assert!(v["preset"].is_null());
        assert_eq!(v["params"]["gain"], 1.5);
    }

    #[test]
    fn beat_fields_serialize_as_null_when_absent() {
        let hop = Hop {
            beat_conf: None,
            bpm: None,
            ..sample_hop()
        };
        let mut w = RecordWriter::new(Vec::new());
        w.hop(&hop).unwrap();
        let line = String::from_utf8(w.into_inner()).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert!(v["beat_conf"].is_null());
        assert!(v["bpm"].is_null());
        assert!(v["canvas"].is_null());
    }

    #[test]
    fn loudness_is_omitted_when_absent_and_present_when_set() {
        // Absent: the optional field is skipped entirely, so an older reader (and
        // an older record) is unaffected.
        let hop = Hop {
            loudness: None,
            ..sample_hop()
        };
        let mut w = RecordWriter::new(Vec::new());
        w.hop(&hop).unwrap();
        let line = String::from_utf8(w.into_inner()).unwrap();
        assert!(
            !line.contains("loudness"),
            "absent loudness must not appear in the line: {line}"
        );
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert!(v["loudness"].is_null());

        // Present: it serializes and round-trips through the tagged enum.
        let hop = Hop {
            loudness: Some(0.72),
            ..sample_hop()
        };
        let mut w = RecordWriter::new(Vec::new());
        w.hop(&hop).unwrap();
        let line = String::from_utf8(w.into_inner()).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert!((v["loudness"].as_f64().unwrap() - 0.72).abs() < 1e-6);
        match Record::from_line(line.trim()).unwrap() {
            Record::Hop(h) => assert_eq!(h.loudness, Some(0.72)),
            other => panic!("expected a hop, got {other:?}"),
        }
    }
}
