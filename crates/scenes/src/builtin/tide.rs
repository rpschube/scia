//! `tide` — four stacked horizontal swells drifting at their own tempos, the
//! calm end of the set built to survive the coarsest tier.
//!
//! Four broad, wavy ridges are stacked up the field like ocean swells seen edge
//! on: the front swell sits low, the others recede upward, each dimmer than the
//! one in front so the stack reads with depth. Every swell drifts sideways at
//! its own incommensurate tempo, so the picture rearranges itself over seconds
//! without ever visibly repeating. The whole field *breathes* with loudness —
//! louder passages brighten it, quiet passages ease it down — and the **front**
//! swell alone lifts and swells with the low band (`bands[0]`): when the bass
//! surges above its recent average the front ridge rises and grows, then settles
//! back. Nothing here reacts to an onset or to per-frame spectrum wiggle; the two
//! audio-driven quantities (overall level and the front lift) are both folded
//! through slow followers, so the surface swells and settles but never jitters.
//!
//! # Built from the field primitive
//!
//! Like `aurora`, `tide` writes exactly one [`crate::canvas::Primitive::Field`]
//! per frame at a fixed `96 × 54` grid (16:9, so the presenter downsamples
//! cleanly). Drawing the swells as broad shaded ridges — rather than lines or
//! points — is a *designed* requirement: the coarse half-block tier quantizes
//! intensity to roughly four shade levels, and a broad ridge over a dark field
//! stays legible there where a thin stroke would break up. The value buffer is
//! allocated once at [`Scene::init`] and overwritten in place every frame, so a
//! warmed scene does no per-frame allocation.
//!
//! # Loudness normalization
//!
//! Raw `rms` is a poor brightness driver — real music sits around `0.1..=0.25`,
//! so a level fed the bare value barely moves. As in `aurora`, loudness is
//! normalized against an adaptive **ceiling** (a fast-attack, slow-release
//! peak-follower over `rms`, floored so silence cannot divide the ratio up)
//! before it drives the level envelope. The result is level-independent: on any
//! material the ceiling calibrates to that material's own loud passages. In
//! `Quiet`/`Idle` the input `rms` falls, the ceiling releases and the level
//! envelope eases the field down gracefully — the same handling `aurora` relies
//! on — while the swells keep drifting so the surface never freezes.
//!
//! # Parameters
//!
//! | key        | default | range        | meaning                                                        |
//! |------------|---------|--------------|----------------------------------------------------------------|
//! | `drift`    | `1.0`   | `0.0..=4.0`  | swell speed: scales how fast every swell drifts sideways        |
//! | `swell`    | `0.12`  | `0.02..=0.4` | base swell amplitude (fraction of height) the ridges wave by    |
//! | `response` | `0.6`   | `0.0..=1.5`  | how strongly the front swell lifts and grows with the low band  |
//! | `level`    | `0.6`   | `0.2..=1.0`  | overall brightness ceiling the loudness envelope breathes under |
//! | `contrast` | `2.0`   | `1.0..=4.0`  | contrast shaping: higher pushes darks darker and brights brighter |
//!
//! `drift`, `swell`, `response`, `level` and `contrast` are live tuning scalars:
//! the host re-applies them every frame through [`Scene::apply_params`], each
//! clamped to its manifest range on read.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the four swell phases, the loudness envelope, the
//! loudness ceiling and the front-lift (low-band) envelope, so a hot reload
//! resumes the drift, the current brightness and the front swell's lift rather
//! than snapping back to the start.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};

/// Field columns. `96 × 54` is 16:9; see the module docs.
const COLS: usize = 96;
/// Field rows.
const ROWS: usize = 54;
/// Number of stacked swells.
const NSWELLS: usize = 4;
/// Dim floor added to every cell so the dark field is never pure black.
const AMBIENT: f32 = 0.05;
/// Palette slot the field is coloured with (cyan in the default palette).
const SLOT: crate::Slot = 2;
/// `2π`, the period of a full wave cycle.
const TWO_PI: f32 = std::f32::consts::TAU;

/// Level-envelope time constant while brightening (seconds).
const ATTACK_TAU: f32 = 0.5;
/// Level-envelope time constant while easing back down (seconds).
const RELEASE_TAU: f32 = 1.4;
/// Loudness-ceiling attack time constant (seconds); see `aurora`'s docs.
const CEILING_ATTACK_TAU: f32 = 0.3;
/// Loudness-ceiling release time constant (seconds).
const CEILING_RELEASE_TAU: f32 = 10.0;
/// Floor under the loudness ceiling, bounding the normalizing divisor.
const CEILING_FLOOR: f32 = 0.05;
/// Front-lift (low-band) follower time constant (seconds): a calm swell of the
/// front ridge with the bass, never a per-frame twitch.
const BASS_TAU: f32 = 0.35;

/// Fraction of the brightness ceiling that remains at silence, so the swells
/// stay dimly legible in `Quiet`/`Idle` instead of vanishing.
const BREATH_FLOOR: f32 = 0.5;

/// Maximum upward shift of the front swell's baseline (fraction of height) at
/// full response and full low-band drive.
const FRONT_LIFT_UNIT: f32 = 0.12;
/// Extra amplitude the front swell gains at full low-band drive, as a fraction
/// of its base amplitude.
const FRONT_AMP_BOOST: f32 = 0.6;

/// Per-swell baseline (fraction of height, `0` top). Index `0` is the front
/// swell, low in the field; higher indices recede upward.
const BASELINES: [f32; NSWELLS] = [0.80, 0.60, 0.42, 0.26];
/// Per-swell spatial frequency (wave cycles across the width). Incommensurate.
const FREQS: [f32; NSWELLS] = [1.0, 1.4, 0.8, 1.7];
/// Per-swell drift speed (radians / second at `drift = 1`). Different tempos and
/// signs, so the swells never march in lock-step.
const SPEEDS: [f32; NSWELLS] = [0.18, -0.13, 0.10, -0.07];
/// Per-swell brightness: the front swell is brightest and the stack dims with
/// depth, so it reads front-to-back.
const BRIGHTS: [f32; NSWELLS] = [1.0, 0.72, 0.52, 0.38];
/// Per-swell ridge half-width (fraction of height); the front ridge is a touch
/// broader so it dominates.
const SIGMAS: [f32; NSWELLS] = [0.055, 0.05, 0.045, 0.04];
/// Per-swell amplitude scale applied to the `swell` parameter; the front swell
/// waves the most.
const AMP_SCALE: [f32; NSWELLS] = [1.0, 0.85, 0.7, 0.6];
/// Starting phases, offset so the first frame already has texture.
const INITIAL_PHASES: [f32; NSWELLS] = [0.0, 1.7, 3.3, 5.0];

/// `tide`'s parameter manifest: the keys a preset may set, with the defaults,
/// ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "drift",
        default: 1.0,
        min: 0.0,
        max: 4.0,
        doc: "swell speed: scales how fast every swell drifts sideways",
    },
    ParamSpec {
        key: "swell",
        default: 0.12,
        min: 0.02,
        max: 0.4,
        doc: "base swell amplitude (fraction of height) the ridges wave by",
    },
    ParamSpec {
        key: "response",
        default: 0.6,
        min: 0.0,
        max: 1.5,
        doc: "how strongly the front swell lifts and grows with the low band",
    },
    ParamSpec {
        key: "level",
        default: 0.6,
        min: 0.2,
        max: 1.0,
        doc: "overall brightness ceiling the loudness envelope breathes under",
    },
    ParamSpec {
        key: "contrast",
        default: 2.0,
        min: 1.0,
        max: 4.0,
        doc: "contrast shaping: higher pushes darks darker and brights brighter",
    },
];

/// The calm stacked-swell scene.
#[derive(Clone, Debug)]
pub struct Tide {
    /// Per-swell phase in radians, wrapped to `0..2π`.
    phase: [f32; NSWELLS],
    /// Slow loudness envelope in `0.0..=1.0`; breathes the brightness.
    loud_env: f32,
    /// Adaptive loudness ceiling: a slow peak-follower over `rms`.
    loud_ceiling: f32,
    /// Low-band envelope in `0.0..=1.0`; lifts and swells the front ridge.
    bass_env: f32,
    /// Swell speed multiplier.
    drift: f32,
    /// Base swell amplitude (fraction of height).
    swell: f32,
    /// Front-swell response to the low band.
    response: f32,
    /// Overall brightness ceiling.
    level: f32,
    /// Contrast shaping exponent.
    contrast: f32,
    /// Pre-allocated field buffer, `COLS * ROWS` values, row-major.
    buf: Vec<f32>,
}

impl Tide {
    /// A `tide` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters and size the field buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: INITIAL_PHASES,
            loud_env: 0.0,
            loud_ceiling: CEILING_FLOOR,
            bass_env: 0.0,
            drift: 1.0,
            swell: 0.12,
            response: 0.6,
            level: 0.6,
            contrast: 2.0,
            buf: vec![0.0; COLS * ROWS],
        }
    }

    /// Consume the preset parameters. Kept as the single point of parameter
    /// consumption so [`Scene::apply_params`] can reuse it verbatim.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.drift, params, "drift");
        read_param(&mut self.swell, params, "swell");
        read_param(&mut self.response, params, "response");
        read_param(&mut self.level, params, "level");
        read_param(&mut self.contrast, params, "contrast");
    }
}

impl Default for Tide {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Tide {
    fn id(&self) -> &'static str {
        "tide"
    }

    fn mood(&self) -> &'static str {
        "fluid"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.phase = INITIAL_PHASES;
        self.loud_env = 0.0;
        self.loud_ceiling = CEILING_FLOOR;
        self.bass_env = 0.0;
        self.buf.clear();
        self.buf.resize(COLS * ROWS, 0.0);
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the swell phases and all three envelopes carry
        // across, so a live mapping never resets the drift or the breathing.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // Every swell drifts regardless of audio: this is a calm scene, so the
        // surface keeps moving even in silence.
        for (k, p) in self.phase.iter_mut().enumerate() {
            *p = (*p + dt * self.drift * SPEEDS[k]).rem_euclid(TWO_PI);
        }

        // Overall level breathes with loudness. `lufs_momentary` is reserved (0
        // in schema 1), so loudness is read from `rms`; normalize it against an
        // adaptive ceiling (as `aurora` does) so the response is level-
        // independent, then fold the normalized driver through a slow envelope.
        // In `Quiet`/`Idle` the raw rms falls and this eases the field down.
        let rms = f.rms.clamp(0.0, 1.0);
        let (ceil_target, ceil_tau) = if rms > self.loud_ceiling {
            (rms, CEILING_ATTACK_TAU)
        } else {
            (CEILING_FLOOR, CEILING_RELEASE_TAU)
        };
        self.loud_ceiling += (ceil_target - self.loud_ceiling) * follow_coeff(dt, ceil_tau);
        self.loud_ceiling = self.loud_ceiling.max(CEILING_FLOOR);
        let loud_target = (rms / self.loud_ceiling).clamp(0.0, 1.0);
        let loud_tau = if loud_target > self.loud_env {
            ATTACK_TAU
        } else {
            RELEASE_TAU
        };
        self.loud_env += (loud_target - self.loud_env) * follow_coeff(dt, loud_tau);

        // The front swell alone lifts with the low band. `bands[0]` is already
        // normalized against its own recent average (1.0 = average), so map it
        // around that: the average reads ~0.5 and a strong bass surge climbs
        // toward 1.0. Fold it through its own slow follower — a calm swell, not a
        // twitch.
        let bass_target = (f.bands[0].clamp(0.0, 4.0) * 0.5).clamp(0.0, 1.0);
        self.bass_env += (bass_target - self.bass_env) * follow_coeff(dt, BASS_TAU);
    }

    fn render(&mut self, canvas: &mut Canvas) {
        let cx = (COLS as f32 - 1.0).max(1.0);
        let cy = (ROWS as f32 - 1.0).max(1.0);
        let contrast = self.contrast;

        // Brightness breathes between a calm floor and the `level` ceiling.
        let brightness = self.level * (BREATH_FLOOR + (1.0 - BREATH_FLOOR) * self.loud_env);

        // Front-swell drive: an upward baseline shift and an amplitude boost,
        // both scaled by `response` and the low-band envelope.
        let front_drive = (self.response * self.bass_env).clamp(0.0, 1.5);
        let front_shift = (FRONT_LIFT_UNIT * front_drive).min(0.2);
        let front_amp_gain = 1.0 + FRONT_AMP_BOOST * self.bass_env;

        // Precompute each swell's effective baseline and amplitude.
        let mut baseline = BASELINES;
        let mut amp = [0.0f32; NSWELLS];
        for k in 0..NSWELLS {
            amp[k] = self.swell * AMP_SCALE[k];
        }
        baseline[0] -= front_shift;
        amp[0] *= front_amp_gain;

        let ph = self.phase;
        for (r, row) in self.buf.chunks_mut(COLS).enumerate() {
            let y = r as f32 / cy;
            for (c, cell) in row.iter_mut().enumerate() {
                let x = c as f32 / cx;
                let mut v = AMBIENT;
                for k in 0..NSWELLS {
                    let center = baseline[k] + amp[k] * (TWO_PI * FREQS[k] * x + ph[k]).sin();
                    let d = (y - center) / SIGMAS[k];
                    v += BRIGHTS[k] * (-0.5 * d * d).exp();
                }
                *cell = shape_contrast(v.min(1.0), contrast) * brightness;
            }
        }

        canvas.field(COLS as u16, ROWS as u16, &self.buf, Style::new(SLOT, 1.0));
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        for (k, p) in self.phase.iter().enumerate() {
            s.set(&format!("phase{k}"), *p);
        }
        s.set("loud", self.loud_env);
        s.set("ceil", self.loud_ceiling);
        s.set("bass", self.bass_env);
        s
    }

    fn restore(&mut self, s: SceneState) {
        for (k, p) in self.phase.iter_mut().enumerate() {
            if let Some(v) = s.get(&format!("phase{k}")) {
                *p = v;
            }
        }
        if let Some(v) = s.get("loud") {
            self.loud_env = v;
        }
        if let Some(v) = s.get("ceil") {
            self.loud_ceiling = v.max(CEILING_FLOOR);
        }
        if let Some(v) = s.get("bass") {
            self.bass_env = v;
        }
    }
}

/// The step fraction a first-order follower moves toward its target over `dt`
/// seconds with time constant `tau`: `1 - exp(-dt / tau)`. A non-positive `tau`
/// (or non-finite `dt`) snaps straight to the target.
#[inline]
fn follow_coeff(dt: f32, tau: f32) -> f32 {
    if tau > 0.0 && dt.is_finite() {
        1.0 - (-dt / tau).exp()
    } else {
        1.0
    }
}

/// A symmetric contrast curve around `0.5`. `c == 1.0` is the identity; `c > 1.0`
/// pushes values away from the midpoint toward `0.0` and `1.0`, so a coarse
/// four-level quantizer sees real darks and brights instead of mid-gray mush.
/// Output stays within `0.0..=1.0`.
#[inline]
fn shape_contrast(v: f32, c: f32) -> f32 {
    let t = (v - 0.5) * 2.0; // -1..1
    let s = t.signum() * t.abs().powf(1.0 / c);
    0.5 + 0.5 * s
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
            .expect("key is a tide parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}
