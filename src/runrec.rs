//! The `--log-run` per-run recorder: maps the live feature bus onto the frozen
//! run-record schema ([`scia_telemetry::record`]) and writes it as JSON Lines.
//!
//! One [`RunRecorder`] owns the output file for a session. It is driven either
//! directly (the headless loop calls [`observe`](RunRecorder::observe) as it
//! samples the bus) or as a [`scia_tui::RunObserver`] (the TUI loop calls it once
//! per frame plus on device switches and hot reloads). Either way it emits:
//!
//! * one `run_start` with the resolved scene, preset and scalar params,
//! * one `hop` per recorded hop — throttled (see [`Throttle`]) and de-duplicated
//!   by the snapshot generation, with `canvas: null` (this mode records the
//!   audio-feature plane; the harness fills canvas stats when it renders),
//! * `event` records for scene/preset swaps (detected from the active scene id
//!   changing), device switches and hot reloads,
//! * one `run_end` on [`finish`](RunRecorder::finish).
//!
//! Hop timestamps are the snapshot's own monotonic engine clock, so `t_ms` is
//! monotonic across the run.

use std::io::{self, BufWriter};
use std::path::Path;

use scia_core::FeatureSnapshot;
use scia_telemetry::record::{Event, Hop, RecordWriter, RunEnd, RunStart, SCHEMA};

/// How often live hops are recorded. Replaying a clip records every hop; a live
/// session records every fourth (the bus advances faster than a record per hop
/// is worth on a live run — see `docs/logging.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Throttle {
    /// Record every hop (clip replay via `--input`).
    EveryHop,
    /// Record every fourth hop (live capture / demo).
    EveryFourth,
}

impl Throttle {
    fn stride(self) -> u64 {
        match self {
            Throttle::EveryHop => 1,
            Throttle::EveryFourth => 4,
        }
    }
}

/// Writes a single session's run record. See the module docs.
pub struct RunRecorder {
    writer: RecordWriter<BufWriter<std::fs::File>>,
    stride: u64,
    last_gen: u64,
    have_gen: bool,
    hops: u64,
    /// Reused across hops so the per-hop path allocates nothing beyond the
    /// writer's own serialization buffer once `bands` has settled.
    hop: Hop,
    /// The active scene id last recorded, to detect swaps.
    scene: Option<String>,
    /// The most recent hop time, stamped on events that carry no snapshot.
    last_t_ms: f64,
    /// Set once `run_end` has been written, so [`Drop`] does not write it twice.
    done: bool,
}

impl RunRecorder {
    /// Open `path` for the run record and write the `run_start` line.
    ///
    /// # Errors
    /// Returns any error creating or writing the file.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        path: &Path,
        throttle: Throttle,
        scene: &str,
        preset: Option<String>,
        params: std::collections::BTreeMap<String, f64>,
        source: &str,
        hop_ms: f32,
    ) -> io::Result<Self> {
        let file = std::fs::File::create(path)?;
        let mut writer = RecordWriter::new(BufWriter::new(file));
        writer.run_start(RunStart {
            schema: SCHEMA,
            scene: scene.to_string(),
            preset,
            params,
            source: source.to_string(),
            hop_ms,
        })?;
        Ok(Self {
            writer,
            stride: throttle.stride(),
            last_gen: 0,
            have_gen: false,
            hops: 0,
            hop: Hop {
                t_ms: 0.0,
                rms: 0.0,
                bands: Vec::with_capacity(3),
                onset: 0.0,
                beat_conf: None,
                bpm: None,
                canvas: None,
            },
            scene: Some(scene.to_string()),
            last_t_ms: 0.0,
            done: false,
        })
    }

    /// The current time in ms from a snapshot's monotonic engine clock.
    fn snap_t_ms(snap: &FeatureSnapshot) -> f64 {
        snap.timestamp_ns as f64 / 1.0e6
    }

    /// Observe one frame: record a hop (throttled/de-duplicated) and emit a
    /// scene-swap event when the active scene id changed.
    ///
    /// Errors are swallowed after being noted once — a run record is a
    /// best-effort side channel and must never abort the session.
    pub fn observe(&mut self, snap: &FeatureSnapshot, scene_id: Option<&str>) {
        self.last_t_ms = Self::snap_t_ms(snap);

        if scene_id != self.scene.as_deref() {
            let from = self.scene.take();
            self.scene = scene_id.map(str::to_owned);
            self.emit_event(
                "scene_swap",
                serde_json::json!({ "from": from, "to": scene_id }),
            );
        }

        let hop_gen = snap.generation;
        if self.have_gen && hop_gen == self.last_gen {
            return; // same hop observed twice this frame; record once
        }
        self.have_gen = true;
        self.last_gen = hop_gen;
        if self.stride > 1 && hop_gen % self.stride != 0 {
            return;
        }
        self.write_hop(snap);
    }

    /// Build and write one hop record from a snapshot.
    fn write_hop(&mut self, snap: &FeatureSnapshot) {
        self.hop.t_ms = Self::snap_t_ms(snap);
        self.hop.rms = snap.rms;
        self.hop.bands.clear();
        self.hop.bands.extend_from_slice(&snap.bands);
        // The continuous onset strength (normalized spectral flux); the discrete
        // onset flag is not what the scene-quality harness wants here.
        self.hop.onset = snap.flux;
        let locked = snap.tempo_bpm > 0.0;
        self.hop.beat_conf = locked.then_some(snap.beat_confidence);
        self.hop.bpm = locked.then_some(snap.tempo_bpm);
        self.hop.canvas = None;
        if self.writer.hop(&self.hop).is_ok() {
            self.hops += 1;
        }
    }

    /// Emit an event record stamped at the most recent hop time.
    fn emit_event(&mut self, kind: &str, detail: serde_json::Value) {
        let _ = self.writer.event(Event {
            t_ms: self.last_t_ms,
            kind: kind.to_string(),
            detail,
        });
    }

    /// Record a capture device switch.
    pub fn note_device_switch(&mut self, label: &str) {
        self.emit_event("device_switch", serde_json::json!({ "device": label }));
    }

    /// Record a live preset / Luau scene hot reload.
    pub fn note_reload(&mut self, scene_id: Option<&str>, elapsed_ms: f32) {
        self.emit_event(
            "hot_reload",
            serde_json::json!({ "scene": scene_id, "elapsed_ms": elapsed_ms }),
        );
    }

    /// Write the `run_end` line and flush. Consumes the recorder.
    ///
    /// Dropping the recorder without calling this still writes `run_end` and
    /// flushes (best-effort) — the boxed TUI-observer path relies on that — so a
    /// run record is always terminated.
    ///
    /// # Errors
    /// Returns any error writing or flushing the file.
    pub fn finish(mut self) -> io::Result<()> {
        self.write_run_end()
    }

    /// Write `run_end` (once) and flush.
    fn write_run_end(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        self.writer.run_end(RunEnd {
            t_ms: self.last_t_ms,
            hops: self.hops,
        })?;
        self.writer.flush()
    }
}

impl Drop for RunRecorder {
    fn drop(&mut self) {
        // Terminate the record even on the boxed observer path, where `finish`
        // is never called explicitly.
        let _ = self.write_run_end();
    }
}

impl scia_tui::RunObserver for RunRecorder {
    fn frame(&mut self, snapshot: &FeatureSnapshot, scene_id: Option<&str>) {
        self.observe(snapshot, scene_id);
    }
    fn device_switch(&mut self, label: &str) {
        self.note_device_switch(label);
    }
    fn reload(&mut self, scene_id: Option<&str>, elapsed_ms: f32) {
        self.note_reload(scene_id, elapsed_ms);
    }
}
