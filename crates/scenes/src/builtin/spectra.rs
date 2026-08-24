//! `spectra` — the canonical analyzer, tuned so its low end visibly punches
//! with the kick.
//!
//! Each spectrum bar is drawn as a vertical bar rising from the bottom. The
//! engine already normalizes and smooths the display spectrum; this scene adds
//! only a light extra release so it has its own feel, plus an **onset envelope**
//! that lifts the low bars on every detected onset — the low end swells with the
//! kick and settles between hits.
//!
//! # Parameters
//!
//! | key           | default | meaning                                             |
//! |---------------|---------|-----------------------------------------------------|
//! | `release`     | `0.15`  | extra release time constant (seconds)               |
//! | `punch_decay` | `0.25`  | onset-envelope decay time constant (seconds)        |
//! | `punch`       | `0.35`  | how much the envelope lifts the low bars            |
//! | `gap`         | `0.15`  | gap between bars, as a fraction of a column          |
//!
//! All four are tuning scalars: the host re-applies them every frame through
//! [`Scene::apply_params`] (after feature mappings rewrite the layer's params),
//! so a `[map]` on any of them is honored live. Each is clamped to its manifest
//! range on read, since a mapping's `offset + scale * env` can exceed it.
//!
//! # Continuity
//!
//! [`Scene::state`] carries only the onset envelope. The per-bar heights are
//! **not** carried: on the first frame after a restore they re-settle to the
//! current spectrum (an out-of-range target is an instant attack), so a frame's
//! worth of history is not worth serializing.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};

/// The fraction of bars, from the low end, that ride the onset envelope.
const LOW_FRACTION: f32 = 0.25;

/// `spectra`'s parameter manifest: the keys a preset may set, with the
/// defaults, ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "release",
        default: 0.15,
        min: 0.01,
        max: 2.0,
        doc: "extra release time constant (seconds)",
    },
    ParamSpec {
        key: "punch_decay",
        default: 0.25,
        min: 0.01,
        max: 2.0,
        doc: "onset-envelope decay time constant (seconds)",
    },
    ParamSpec {
        key: "punch",
        default: 0.35,
        min: 0.0,
        max: 2.0,
        doc: "how much the envelope lifts the low bars",
    },
    ParamSpec {
        key: "gap",
        default: 0.15,
        min: 0.0,
        max: 0.9,
        doc: "gap between bars, as a fraction of a column",
    },
];

/// The canonical spectrum analyzer scene.
#[derive(Clone, Debug)]
pub struct Spectra {
    /// Per-bar smoothed heights, one entry per active spectrum bar.
    heights: Vec<f32>,
    /// Onset envelope in `0.0..=1.0`.
    env: f32,
    /// Extra release time constant (seconds).
    release: f32,
    /// Onset-envelope decay time constant (seconds).
    punch_decay: f32,
    /// Low-end lift amount.
    punch: f32,
    /// Gap between bars as a fraction of a column.
    gap: f32,
}

impl Spectra {
    /// A `spectra` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            heights: Vec::new(),
            env: 0.0,
            release: 0.15,
            punch_decay: 0.25,
            punch: 0.35,
            gap: 0.15,
        }
    }

    /// Refresh the tuning scalars from `params`, and only those — the onset
    /// envelope and per-bar heights are left untouched so a mid-run re-apply
    /// (feature mappings, later live tuning) does not reset animation.
    ///
    /// Shared by [`Scene::init`] and [`Scene::apply_params`]. A key absent from
    /// the bag keeps its current value; a present key is clamped to its
    /// [`ParamSpec`] range, since a mapping writes `offset + scale * env`, which
    /// can leave the range the preset validated at load. Allocation-free.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.release, params, "release");
        read_param(&mut self.punch_decay, params, "punch_decay");
        read_param(&mut self.punch, params, "punch");
        read_param(&mut self.gap, params, "gap");
    }
}

impl Default for Spectra {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Spectra {
    fn id(&self) -> &'static str {
        "spectra"
    }

    fn mood(&self) -> &'static str {
        "kinetic"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.env = 0.0;
        self.heights.clear();
    }

    fn apply_params(&mut self, params: &Params) {
        // Only the tuning scalars refresh: the onset envelope and per-bar
        // heights carry across, so a live mapping is honored without resetting
        // the animation mid-run.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        let len = f.spectrum_len as usize;
        // Track the onset envelope: snap to full on an onset, otherwise decay.
        if f.onset {
            self.env = 1.0;
        } else {
            self.env *= decay(dt, self.punch_decay);
        }

        // Match the buffer to the active bar count, then apply a light extra
        // release: an instant attack, a slow release toward the engine's value.
        self.heights.resize(len, 0.0);
        let rel = decay(dt, self.release);
        for (i, h) in self.heights.iter_mut().enumerate() {
            let target = f.spectrum[i];
            if target >= *h {
                *h = target;
            } else {
                *h = target + (*h - target) * rel;
            }
        }
    }

    fn render(&mut self, canvas: &mut Canvas) {
        let len = self.heights.len();
        if len == 0 {
            return;
        }
        let col = 1.0 / len as f32;
        let gap_w = self.gap.clamp(0.0, 1.0) * col;
        let bar_w = (col - gap_w).max(0.0);
        // Ceil of len * 25%: at least the first quarter of bars ride the punch.
        let low_cut = ((len as f32 * LOW_FRACTION).ceil()) as usize;
        let lift = 1.0 + self.punch * self.env;

        for (i, &raw) in self.heights.iter().enumerate() {
            let h = if i < low_cut {
                (raw * lift).clamp(0.0, 1.0)
            } else {
                raw
            };
            let x = i as f32 * col + gap_w * 0.5;
            let y = 1.0 - h; // grow from the bottom (y grows downward)
            let slot = slot_for_height(h);
            canvas.bar(x, y, bar_w, h, Style::new(slot, h));
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("env", self.env);
        s
    }

    fn restore(&mut self, s: SceneState) {
        if let Some(env) = s.get("env") {
            self.env = env;
        }
    }
}

/// Refresh one tuning scalar from `params` in place. When `key` is present, the
/// value is stored clamped to that parameter's manifest `[min, max]`; when
/// absent, the slot keeps its current value. The clamp matters because a mapping
/// writes `offset + scale * env`, which can leave the range validated at preset
/// load. Allocation-free: a linear scan of the bag and the static manifest.
#[inline]
fn read_param(slot: &mut f32, params: &Params, key: &str) {
    if let Some(v) = params.get(key) {
        let spec = PARAMS
            .iter()
            .find(|s| s.key == key)
            .expect("key is a spectra parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}

/// The per-step multiplier of an exponential decay with time constant `tau`
/// over `dt` seconds. `tau <= 0` (or a non-finite `dt`) collapses to an instant
/// decay (multiplier `0`).
#[inline]
fn decay(dt: f32, tau: f32) -> f32 {
    if tau > 0.0 && dt.is_finite() {
        (-dt / tau).exp()
    } else {
        0.0
    }
}

/// Choose a palette slot from a bar height: three bands mapped to slots 2
/// (cyan), 3 (amber) and 4 (coral) of the default palette.
#[inline]
fn slot_for_height(h: f32) -> crate::Slot {
    if h < 1.0 / 3.0 {
        2
    } else if h < 2.0 / 3.0 {
        3
    } else {
        4
    }
}
