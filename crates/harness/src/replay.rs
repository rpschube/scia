//! Headless replay: drive a scene (or a preset's layer stack) over a clip's
//! feature frames at the canonical hop cadence, collect the display list each
//! frame, and produce the run record and the per-hop series the metrics consume.
//!
//! The per-frame loop mirrors the host: for each layer, feature→param mappings
//! rewrite the layer's params, [`Scene::apply_params`] refreshes the scene's
//! tuning scalars, [`Scene::update`] folds in the newest features and
//! [`Scene::render`] emits the layer's canvas. The layers' primitives are merged
//! into one display list (each layer's intensity scaling its primitives); blend
//! modes are a presenter concern and are not modelled at the display-list level.

use std::collections::BTreeMap;

use scia_core::FeatureFrame;
use scia_scenes::{
    Canvas as SceneCanvas, LayerInstance, Palette, Params, Preset, Primitive, Style, load_preset,
    scene_info,
};

use crate::canvas_stats::CanvasProbe;
use crate::metrics::{self, MetricParams, Metrics, Series};
use scia_telemetry::record::{Event, Hop, Record, RunEnd, RunStart, SCHEMA};

/// The canonical drawing aspect ratio (width / height) a headless run renders
/// at: a 16:9 display. Coverage is aspect-independent (it is a normalised-area
/// fraction); the aspect only keeps round primitives round.
pub const CANON_ASPECT: f32 = 16.0 / 9.0;

/// Everything a replay produces.
pub struct RunOutput {
    /// The full run-record stream (run_start, hops, events, run_end).
    pub records: Vec<Record>,
    /// The computed whole-run metrics.
    pub metrics: Metrics,
    /// The number of hops replayed.
    pub hops: u64,
}

/// A fully resolved replay request.
pub struct RunRequest<'a> {
    /// The scene id to drive (ignored when `preset` names its own scene, but
    /// used as the record's scene label and the fallback).
    pub scene: &'a str,
    /// An optional preset (already loaded).
    pub preset: Option<Preset>,
    /// A label for the preset in the record (name or path), or `None`.
    pub preset_label: Option<String>,
    /// `--set key=value` overrides applied on top of every layer's params.
    pub sets: &'a [(String, f32)],
    /// The clip's feature frames, in hop order.
    pub frames: &'a [FeatureFrame],
    /// The `source` label for the record.
    pub source: &'a str,
    /// The hop cadence in milliseconds.
    pub hop_ms: f32,
    /// Metric tunables.
    pub metric_params: MetricParams,
}

/// Load a preset from a TOML file, returning it with a display label.
///
/// # Errors
/// The preset validator's message on a failed load.
pub fn load_preset_labeled(path: &str) -> Result<(Preset, String), String> {
    let preset = load_preset(std::path::Path::new(path)).map_err(|e| e.to_string())?;
    Ok((preset, path.to_string()))
}

/// Build the effective params bag for a layer: manifest defaults, then (for the
/// preset's own scene) the preset's merged `[params]`, then the layer overlay,
/// then the `--set` overrides.
fn layer_params(
    scene_id: &str,
    preset: &Preset,
    layer_overlay: &[(String, f32)],
    sets: &[(String, f32)],
) -> Params {
    let mut p = Params::new();
    if let Some(info) = scene_info(scene_id) {
        for spec in info.params {
            p.set(spec.key, spec.default);
        }
        // The preset's merged params only cover its own scene's manifest keys.
        if scene_id == preset.scene {
            for spec in info.params {
                if let Some(v) = preset.params.get(spec.key) {
                    p.set(spec.key, v);
                }
            }
        }
    }
    for (k, v) in layer_overlay {
        p.set(k, *v);
    }
    for (k, v) in sets {
        p.set(k, *v);
    }
    p
}

/// The `[params]` overlay a given layer index carries (empty for the implicit
/// single layer).
fn overlay_for(preset: &Preset, index: usize) -> &[(String, f32)] {
    preset
        .layers
        .get(index)
        .map_or(&[], |l| l.params.as_slice())
}

/// Run a replay end to end.
#[must_use]
pub fn run(req: &RunRequest) -> RunOutput {
    let palette = req
        .preset
        .as_ref()
        .map_or_else(Palette::default_dark, Preset::palette);

    // Build the preset (an explicit one, or a synthesized layerless one) and its
    // live layers.
    let owned_preset;
    let preset: &Preset = match &req.preset {
        Some(p) => p,
        None => {
            let info = scene_info(req.scene).unwrap_or_else(|| {
                // Total fallback so this path cannot panic; it renders (and
                // scores) spectra, NOT the requested id — callers must validate
                // the scene id first (the CLI rejects unknown ids up front).
                scene_info("spectra").expect("spectra is registered")
            });
            owned_preset = Preset::for_scene(info, palette);
            &owned_preset
        }
    };

    let mut layers: Vec<LayerInstance> = preset.instantiate(CANON_ASPECT);

    // Per-layer effective params bags, seeded with any mapping targets.
    let mut bags: Vec<Params> = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate() {
        let scene_id = layer.scene.id();
        let mut bag = layer_params(scene_id, preset, overlay_for(preset, i), req.sets);
        layer.mappings.seed(&mut bag);
        bags.push(bag);
    }

    // The record's scene id and params come from the first layer.
    let record_scene = layers
        .first()
        .map_or(req.scene, |l| l.scene.id())
        .to_string();
    let params_map = record_params(&record_scene, &bags);

    let mut records: Vec<Record> = Vec::with_capacity(req.frames.len() + 2);
    records.push(Record::RunStart(RunStart {
        schema: SCHEMA,
        scene: record_scene,
        preset: req.preset_label.clone(),
        params: params_map,
        source: req.source.to_string(),
        hop_ms: req.hop_ms,
    }));

    let mut probe = CanvasProbe::new(palette, CANON_ASPECT);
    let mut combined = SceneCanvas::new(CANON_ASPECT);
    let mut layer_canvas = SceneCanvas::new(CANON_ASPECT);

    // Per-hop series for the metrics.
    let n = req.frames.len();
    let mut s_onset = Vec::with_capacity(n);
    let mut s_rms = Vec::with_capacity(n);
    let mut s_motion = Vec::with_capacity(n);
    let mut s_bright = Vec::with_capacity(n);
    let mut s_cover = Vec::with_capacity(n);
    let mut s_color: Vec<[f32; 3]> = Vec::with_capacity(n);

    let dt = req.hop_ms / 1000.0;
    let t0 = req.frames.first().map_or(0, |f| f.timestamp_ns);
    let mut last_t_ms = 0.0f64;

    for (i, frame) in req.frames.iter().enumerate() {
        let snap = frame.to_snapshot();
        let t_ms = if frame.timestamp_ns >= t0 {
            (frame.timestamp_ns - t0) as f64 / 1.0e6
        } else {
            i as f64 * f64::from(req.hop_ms)
        };
        last_t_ms = t_ms;

        combined.clear();
        for (li, layer) in layers.iter_mut().enumerate() {
            layer.mappings.apply(&snap, dt, &mut bags[li]);
            layer.scene.apply_params(&bags[li]);
            layer.scene.update(&snap, dt);
            layer_canvas.clear();
            layer.scene.render(&mut layer_canvas);
            merge_layer(&mut combined, &layer_canvas, layer.intensity);
        }

        let (canvas_rec, mean_rgb) = probe.probe(&combined);

        s_onset.push(frame.flux);
        s_rms.push(frame.rms);
        s_motion.push(canvas_rec.motion);
        s_bright.push(canvas_rec.brightness);
        s_cover.push(canvas_rec.coverage);
        s_color.push(mean_rgb);

        records.push(Record::Hop(Hop {
            t_ms,
            rms: frame.rms,
            loudness: Some(frame.loudness),
            bands: frame.bands.to_vec(),
            onset: frame.flux,
            beat_conf: Some(frame.beat_confidence),
            bpm: Some(frame.tempo_bpm),
            canvas: Some(canvas_rec),
        }));

        if frame.onset {
            records.push(Record::Event(Event {
                t_ms,
                kind: "onset".to_string(),
                // Round the flux to six decimals so the embedded number
                // serialises to a short decimal that survives a JSON
                // serialise→parse cycle exactly (a full-precision float would
                // depend on the reader's float formatting).
                detail: serde_json::json!({ "flux": round6(frame.flux) }),
            }));
        }
    }

    let hops = req.frames.len() as u64;
    records.push(Record::RunEnd(RunEnd {
        t_ms: last_t_ms,
        hops,
    }));

    let series = Series {
        onset: &s_onset,
        rms: &s_rms,
        motion: &s_motion,
        brightness: &s_bright,
        coverage: &s_cover,
        color: &s_color,
        hop_ms: req.hop_ms,
    };
    let metrics = metrics::compute(&series, &req.metric_params);

    RunOutput {
        records,
        metrics,
        hops,
    }
}

/// The run-record params map for the record scene, read back from its layer's
/// effective bag over its manifest keys.
fn record_params(scene_id: &str, bags: &[Params]) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    if let (Some(info), Some(bag)) = (scene_info(scene_id), bags.first()) {
        for spec in info.params {
            if let Some(v) = bag.get(spec.key) {
                map.insert(spec.key.to_string(), f64::from(v));
            }
        }
    }
    map
}

/// Copy `src`'s primitives into `dst`, scaling each primitive's intensity by
/// `intensity_scale` (the layer intensity). Field and text bulk data are
/// resolved and re-pushed so `dst`'s arenas stay consistent.
fn merge_layer(dst: &mut SceneCanvas, src: &SceneCanvas, intensity_scale: f32) {
    let scale = intensity_scale.clamp(0.0, 1.0);
    for prim in src.primitives() {
        match *prim {
            Primitive::Bar { x, y, w, h, style } => {
                dst.bar(x, y, w, h, scaled(style, scale));
            }
            Primitive::Line {
                x0,
                y0,
                x1,
                y1,
                width,
                style,
            } => {
                dst.line(x0, y0, x1, y1, width, scaled(style, scale));
            }
            Primitive::Point { x, y, size, style } => {
                dst.point(x, y, size, scaled(style, scale));
            }
            Primitive::Field {
                cols, rows, style, ..
            } => {
                let values = src.field_of(prim).unwrap_or(&[]);
                dst.field(cols, rows, values, scaled(style, scale));
            }
            Primitive::Text { x, y, style, .. } => {
                if let Some(text) = src.text_of(prim) {
                    dst.text(x, y, text, scaled(style, scale));
                }
            }
        }
    }
}

fn scaled(style: Style, scale: f32) -> Style {
    Style::new(style.slot, style.intensity * scale)
}

/// Round to six decimals as an `f64`, for embedding in a JSON detail object
/// where a short decimal round-trips exactly.
fn round6(v: f32) -> f64 {
    (f64::from(v) * 1.0e6).round() / 1.0e6
}
