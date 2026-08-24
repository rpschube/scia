//! `aurora` — a slow interference field, the calm end of the scene set and the
//! coarse half-block tier at its best.
//!
//! Two to three sinusoidal wavefronts drift across the field with different
//! directions, wavelengths and speeds; where they meet they interfere, so broad
//! bright ridges wander and re-form over seconds. A soft horizontal band brightens
//! the middle of the field like an aurora against a dark sky, and the band
//! *breathes*: loudness — and loudness alone — widens it. Nothing here reacts to
//! an onset or to per-frame spectrum wiggle. The only audio-driven quantity, the
//! band width, is folded through a slow envelope (time constants on the order of
//! a second), so the picture never jitters — it only swells and settles.
//!
//! # Loudness normalization
//!
//! Raw `rms` is a poor width driver: real music sits around `0.1..=0.25`, so a
//! width fed the bare level barely moves and the band reads as static regardless
//! of the gain the response is tuned for. Instead the scene normalizes loudness
//! against an adaptive reference before the envelope. A slow **loudness ceiling**
//! peak-follows `rms` — a fast attack ([`CEILING_ATTACK_TAU`]) so a sustained
//! passage raises it within a second, a very slow release ([`CEILING_RELEASE_TAU`],
//! ~10 s) so it remembers the loudest recent passage, and a floor
//! ([`CEILING_FLOOR`]) so genuine silence can never divide the ratio up toward one.
//! The envelope is then driven by `min(1, rms / ceiling)`. The consequence is that
//! the response is *level-independent*: on any material — quiet-mastered or
//! brick-walled — the ceiling calibrates to that material's own loud passages, so
//! sustained loud stretches push the driver toward `1.0` and quiet stretches fall
//! toward `0.0`, and two recordings at very different absolute levels settle to the
//! same band width. The ceiling is a slow calibration reference, not a reactive
//! signal: it never adds per-frame motion, so the calm-scene invariant holds.
//!
//! # Internal resolution
//!
//! The scene writes one [`crate::canvas::Primitive::Field`] per frame at a fixed
//! `96 × 54` grid. That is exactly 16:9, so the presenter downsamples cleanly to
//! any terminal or GPU surface without anisotropic stretching, and 5184 cells is
//! coarse enough to stay cheap yet fine enough that the wavefronts read as smooth
//! curves rather than stair-steps even before the presenter quantizes them to the
//! four shade characters `░▒▓█`. The value buffer is allocated once at
//! [`Scene::init`] and overwritten in place every frame, so a warmed scene does no
//! per-frame allocation.
//!
//! # Legibility at the coarse tier
//!
//! The coarse presenter maps intensity onto roughly four shade levels, so a field
//! that hovers around mid-gray would turn to mush. Two choices keep real darks and
//! brights: a symmetric contrast curve pushes interference values away from the
//! midpoint toward the extremes, and the region *outside* the bright band is
//! multiplied down to a dim ambient floor. The band therefore reads as a clearly
//! brighter horizontal swathe over a dark field, with the wave ridges legible
//! inside it.
//!
//! # Parameters
//!
//! | key        | default | range        | meaning                                           |
//! |------------|---------|--------------|---------------------------------------------------|
//! | `drift`    | `1.0`   | `0.0..=4.0`  | field speed: scales how fast the wavefronts move   |
//! | `scale`    | `1.0`   | `0.2..=4.0`  | spatial frequency: how many wave cycles span the field |
//! | `band`     | `0.10`  | `0.02..=0.5` | base band half-width (fraction of height) at silence |
//! | `response` | `0.30`  | `0.0..=0.6`  | loudness-to-width gain: extra band half-width at full (normalized) loudness |
//! | `contrast` | `2.2`   | `1.0..=4.0`  | contrast shaping: higher pushes darks darker and brights brighter |
//!
//! At the defaults a settled loud passage (normalized loudness `~1.0`) drives the
//! band's gaussian sigma to `band + response ≈ 0.40`, roughly four times the quiet
//! floor of `0.10`, so the lit band grows from a narrow mid-field swathe to nearly
//! the whole height — a change that reads plainly on real music.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the three wave phases, the loudness envelope and the
//! loudness ceiling, so a hot reload resumes the drift, the current band width and
//! the loudness calibration rather than snapping back to the start.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};

/// Field columns. `96 × 54` is 16:9; see the module docs.
const COLS: usize = 96;
/// Field rows.
const ROWS: usize = 54;
/// Number of interfering wavefronts.
const NWAVES: usize = 3;
/// Dim floor the field is multiplied down to outside the bright band, so the
/// band reads as clearly brighter without the rest going fully black.
const AMBIENT: f32 = 0.12;
/// Lower clamp on the band's gaussian sigma, guarding the reciprocal.
const MIN_SIGMA: f32 = 0.01;
/// Palette slot the field is coloured with (cyan in the default palette).
const SLOT: crate::Slot = 2;
/// `2π`, the period of a full wave cycle.
const TWO_PI: f32 = std::f32::consts::TAU;
/// Loudness-follower time constant while the band is widening (seconds).
const ATTACK_TAU: f32 = 0.6;
/// Loudness-follower time constant while the band is settling back (seconds).
const RELEASE_TAU: f32 = 1.5;
/// Loudness-ceiling attack time constant (seconds): a sustained passage lifts the
/// reference within about a second, while a single loud hop only nudges it, so a
/// stray transient cannot latch the calibration high.
const CEILING_ATTACK_TAU: f32 = 0.3;
/// Loudness-ceiling release time constant (seconds): the reference decays back
/// toward the floor very slowly, so it remembers the loudest recent passage and a
/// quiet stretch reads quiet for several seconds before the scale recalibrates.
const CEILING_RELEASE_TAU: f32 = 10.0;
/// Floor under the loudness ceiling. The normalized driver is `rms / ceiling`, so
/// the floor bounds the divisor: genuine silence and near-silence stay near `0.0`
/// instead of dividing up toward `1.0`, and it sits below any real musical level
/// so quiet-mastered material still calibrates to its own loud passages.
const CEILING_FLOOR: f32 = 0.05;
/// Starting phases, offset so the first frame already has texture.
const INITIAL_PHASES: [f32; NWAVES] = [0.0, 2.0, 4.0];

/// One drifting wavefront: a direction, a relative spatial frequency and a phase
/// speed. Directions are non-parallel and the speeds are incommensurate, so the
/// interference pattern evolves without visibly repeating.
struct Wave {
    /// Propagation direction in degrees.
    angle_deg: f32,
    /// Spatial frequency relative to `scale` (cycles across the field).
    freq: f32,
    /// Phase speed in radians per second, scaled by `drift`.
    speed: f32,
}

/// The three wavefronts. Chosen by hand for slow, non-repeating interference.
const WAVES: [Wave; NWAVES] = [
    Wave {
        angle_deg: 18.0,
        freq: 1.0,
        speed: 0.23,
    },
    Wave {
        angle_deg: 105.0,
        freq: 1.7,
        speed: -0.15,
    },
    Wave {
        angle_deg: 212.0,
        freq: 2.4,
        speed: 0.31,
    },
];

/// `aurora`'s parameter manifest: the keys a preset may set, with the defaults,
/// ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "drift",
        default: 1.0,
        min: 0.0,
        max: 4.0,
        doc: "field speed: scales how fast the wavefronts move",
    },
    ParamSpec {
        key: "scale",
        default: 1.0,
        min: 0.2,
        max: 4.0,
        doc: "spatial frequency: how many wave cycles span the field",
    },
    ParamSpec {
        key: "band",
        default: 0.10,
        min: 0.02,
        max: 0.5,
        doc: "base band half-width (fraction of height) at silence",
    },
    ParamSpec {
        key: "response",
        default: 0.30,
        min: 0.0,
        max: 0.6,
        doc: "loudness-to-width gain: extra band half-width at full (normalized) loudness",
    },
    ParamSpec {
        key: "contrast",
        default: 2.2,
        min: 1.0,
        max: 4.0,
        doc: "contrast shaping: higher pushes darks darker and brights brighter",
    },
];

/// The calm interference-field scene.
#[derive(Clone, Debug)]
pub struct Aurora {
    /// Per-wave phase in radians, wrapped to `0..2π`.
    phase: [f32; NWAVES],
    /// Slow loudness envelope in `0.0..=1.0`; drives the band width only.
    loud_env: f32,
    /// Adaptive loudness ceiling: a slow peak-follower over `rms` that the raw
    /// level is normalized against before the envelope. See the module docs.
    loud_ceiling: f32,
    /// Field speed multiplier.
    drift: f32,
    /// Spatial-frequency multiplier.
    scale: f32,
    /// Base band half-width (fraction of height) at silence.
    band: f32,
    /// Loudness-to-width gain.
    response: f32,
    /// Contrast shaping exponent.
    contrast: f32,
    /// Pre-allocated field buffer, `COLS * ROWS` values, row-major.
    buf: Vec<f32>,
}

impl Aurora {
    /// An `aurora` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters and size the field buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: INITIAL_PHASES,
            loud_env: 0.0,
            loud_ceiling: CEILING_FLOOR,
            drift: 1.0,
            scale: 1.0,
            band: 0.10,
            response: 0.30,
            contrast: 2.2,
            buf: vec![0.0; COLS * ROWS],
        }
    }

    /// Consume the preset parameters. Kept as the single point of parameter
    /// consumption so a per-frame `apply_params` hook can reuse it verbatim.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.drift, params, "drift");
        read_param(&mut self.scale, params, "scale");
        read_param(&mut self.band, params, "band");
        read_param(&mut self.response, params, "response");
        read_param(&mut self.contrast, params, "contrast");
    }
}

impl Default for Aurora {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Aurora {
    fn id(&self) -> &'static str {
        "aurora"
    }

    fn mood(&self) -> &'static str {
        "serene"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.phase = INITIAL_PHASES;
        self.loud_env = 0.0;
        self.loud_ceiling = CEILING_FLOOR;
        self.buf.clear();
        self.buf.resize(COLS * ROWS, 0.0);
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: wave phases, the loudness envelope and the field
        // buffer carry across, so a live mapping never resets the drift.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // Drift continues regardless of audio: this is the calm scene, so the
        // field keeps moving even in silence.
        for (k, p) in self.phase.iter_mut().enumerate() {
            *p = (*p + dt * self.drift * WAVES[k].speed).rem_euclid(TWO_PI);
        }

        // The only audio-driven quantity is the band width, and it is folded
        // through a slow follower — never an onset, never a per-frame spike.
        // `lufs_momentary` is reserved (0 in schema 1), so loudness is read from
        // `rms`; swap to LUFS once that field is computed.
        //
        // First update the adaptive loudness ceiling: a peak-follower over rms
        // with a fast attack and a very slow release, floored so silence can't
        // divide the ratio up. Then normalize the raw level against it, so the
        // driver is level-independent — sustained loud → ~1, quiet → ~0 — on any
        // material. The ceiling is a slow calibration reference; it adds no
        // per-frame motion of its own.
        let rms = f.rms.clamp(0.0, 1.0);
        let (ceil_target, ceil_tau) = if rms > self.loud_ceiling {
            (rms, CEILING_ATTACK_TAU)
        } else {
            (CEILING_FLOOR, CEILING_RELEASE_TAU)
        };
        self.loud_ceiling += (ceil_target - self.loud_ceiling) * follow_coeff(dt, ceil_tau);
        self.loud_ceiling = self.loud_ceiling.max(CEILING_FLOOR);
        let target = (rms / self.loud_ceiling).clamp(0.0, 1.0);
        let tau = if target > self.loud_env {
            ATTACK_TAU
        } else {
            RELEASE_TAU
        };
        self.loud_env += (target - self.loud_env) * follow_coeff(dt, tau);
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // Square up the wave space so a diagonal wavefront looks diagonal rather
        // than stretched by the field's own aspect.
        let aspect = COLS as f32 / ROWS as f32;

        // Per-wave direction times angular spatial frequency, precomputed once.
        let mut wx = [0.0f32; NWAVES];
        let mut wy = [0.0f32; NWAVES];
        for k in 0..NWAVES {
            let a = WAVES[k].angle_deg.to_radians();
            let f = TWO_PI * self.scale * WAVES[k].freq;
            wx[k] = a.cos() * f;
            wy[k] = a.sin() * f;
        }

        let ph = self.phase;
        let contrast = self.contrast;
        // Band half-width grows with the loudness envelope; loudness ONLY widens
        // the band, it never brightens it.
        let sigma = (self.band + self.response * self.loud_env).max(MIN_SIGMA);
        let inv_two_sigma2 = 1.0 / (2.0 * sigma * sigma);
        let cx = (COLS as f32 - 1.0).max(1.0);
        let cy = (ROWS as f32 - 1.0).max(1.0);

        for (r, row) in self.buf.chunks_mut(COLS).enumerate() {
            let y = r as f32 / cy;
            let dy = y - 0.5;
            let band = (-(dy * dy) * inv_two_sigma2).exp();
            let band_gain = AMBIENT + (1.0 - AMBIENT) * band;
            for (c, cell) in row.iter_mut().enumerate() {
                let x = (c as f32 / cx) * aspect;
                let mut s = 0.0;
                for k in 0..NWAVES {
                    s += (ph[k] + wx[k] * x + wy[k] * y).sin();
                }
                // Interference sum in [-N, N] -> [0, 1], then contrast-shaped and
                // gated by the band so the field keeps real darks and brights.
                let n = 0.5 + 0.5 * (s / NWAVES as f32);
                *cell = shape_contrast(n, contrast) * band_gain;
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
        s
    }

    fn restore(&mut self, s: SceneState) {
        for (k, p) in self.phase.iter_mut().enumerate() {
            if let Some(v) = s.get(&format!("phase{k}")) {
                *p = v;
            }
        }
        if let Some(loud) = s.get("loud") {
            self.loud_env = loud;
        }
        if let Some(ceil) = s.get("ceil") {
            self.loud_ceiling = ceil.max(CEILING_FLOOR);
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
            .expect("key is an aurora parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}
