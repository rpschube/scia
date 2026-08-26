//! `tide` — four stacked horizontal swells drifting at their own tempos, the
//! calm end of the set built to survive the coarsest tier.
//!
//! Four broad, wavy ridges are stacked up the field like ocean swells seen edge
//! on: the front swell sits low, the others recede upward, each dimmer than the
//! one in front so the stack reads with depth. Every swell drifts sideways at
//! its own incommensurate tempo, so the picture rearranges itself over seconds
//! without ever visibly repeating. The whole field *breathes* with loudness —
//! louder passages brighten it and drift it faster, quiet passages ease it down —
//! and the **front** swell alone lifts and swells with the low band (`bands[0]`):
//! when the bass surges above its recent average the front ridge rises and grows,
//! then settles back. A transient — an energy rise, measured as spectral flux —
//! is acknowledged across the whole surface: every swell heaves higher and the
//! drift quickens for a moment, then settles. The transient is kept out of the
//! mean brightness, so the acknowledgment reads as motion, not a flicker. The slow
//! drivers (level, drift and the front lift) are folded through slow followers and
//! the transient through a fast attack / ~third-of-a-second release, so the surface
//! swells and settles but never jitters or strobes.
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
//! The audio drivers read the engine-normalized `loudness` — the mono rms divided
//! by a slow auto-reference, computed once on the DSP thread and already
//! level-independent (`0..=1`, sustained program ~0.6..=0.85) — the normalized low
//! `bands[0]` for the front lift, and the normalized spectral `flux` for the
//! transient acknowledgment. The scene folds those through its own envelopes; it
//! no longer keeps a private loudness ceiling, since the engine now performs that
//! normalization for every consumer. In `Quiet`/`Idle` the input loudness falls
//! and the level envelope eases the field down gracefully while the swells keep
//! drifting so the surface never freezes; the transient envelope adds motion only
//! on real onsets, so the calm-at-rest invariant holds.
//!
//! # Parameters
//!
//! | key           | default | range        | meaning                                                        |
//! |---------------|---------|--------------|----------------------------------------------------------------|
//! | `drift`       | `1.0`   | `0.0..=4.0`  | base swell speed: scales how fast every swell drifts sideways   |
//! | `swell`       | `0.12`  | `0.02..=0.4` | base swell amplitude (fraction of height) the ridges wave by    |
//! | `response`    | `0.6`   | `0.0..=1.5`  | how strongly the front swell lifts and grows with the low band  |
//! | `level`       | `0.6`   | `0.2..=1.0`  | overall brightness ceiling the loudness envelope breathes under |
//! | `sensitivity` | `1.0`   | `0.0..=2.0`  | audio-response depth: scales the loudness/transient drift quickening, the onset brightness pop and the onset swell heave (`0` = plain drift) |
//! | `contrast`    | `2.0`   | `1.0..=4.0`  | contrast shaping: higher pushes darks darker and brights brighter |
//!
//! `drift`, `swell`, `response`, `level`, `sensitivity` and `contrast` are live
//! tuning scalars: the host re-applies them every frame through
//! [`Scene::apply_params`], each clamped to its manifest range on read.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the four swell phases, the loudness envelope, the
//! front-lift (low-band) envelope and the transient envelope, so a hot reload
//! resumes the drift, the current brightness, the front swell's lift and any
//! in-flight onset swell rather than snapping back to the start.

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

/// Level-envelope time constant while brightening (seconds). Slow, so the mean
/// brightness never chases loudness noise (that would flicker); the tight
/// loudness↔motion coupling lives in the contrast follower below instead.
const ATTACK_TAU: f32 = 0.5;
/// Level-envelope time constant while easing back down (seconds).
const RELEASE_TAU: f32 = 1.3;
/// Fast loudness-follower time constants (seconds) feeding the contrast sharpening.
/// Tight — it tracks the music's level closely, and can be fast without flickering
/// because it only drives the mean-preserving contrast, not the brightness.
const CONTRAST_ATTACK_TAU: f32 = 0.07;
/// Fast loudness-follower release time constant (seconds) for the contrast drive.
const CONTRAST_RELEASE_TAU: f32 = 0.18;
/// How much loudness sharpens the swell contrast (added to the base `contrast`).
/// The curve is symmetric about the midpoint, so sharpening steepens the ridges —
/// and thus the drift-driven motion — while barely moving the mean brightness: a
/// loudness→motion coupling that tracks the level tightly yet adds no flicker.
const CONTRAST_LOUD_GAIN: f32 = 3.0;
/// Front-lift (low-band) follower time constant (seconds): a calm swell of the
/// front ridge with the bass, never a per-frame twitch.
const BASS_TAU: f32 = 0.3;
/// Onset-follower time constant while rising on transient flux (seconds): a fast
/// but not instantaneous attack so an energy rise reads as a swell, not a strobe.
const ONSET_ATTACK_TAU: f32 = 0.08;
/// Onset-follower time constant while the transient decays (seconds): each
/// transient lifts and settles as one smooth swell.
const ONSET_RELEASE_TAU: f32 = 0.28;
/// Flux-baseline follower time constant (seconds). The transient driver is flux
/// *above this slow average*, so a steadily-textured signal — deep ambient with
/// constant spectral churn, whose flux is high but flat — reads as "no onset" and
/// the surface stays still. Only flux that rises above its own recent average
/// counts as an energy rise.
const FLUX_BASE_TAU: f32 = 1.2;
/// Short flux-smoothing time constant (seconds). Flux is smoothed over a few hops
/// before the novelty is measured, so a single-hop flux spike — the kind steady
/// ambient texture throws off constantly — is attenuated, while a real onset (flux
/// sustained high across several hops) survives. This is what separates a musical
/// transient from a quiet clip's spectral noise and keeps the still clips still.
const FLUX_SMOOTH_TAU: f32 = 0.008;
/// Deadband on the transient novelty: the smoothed flux must clear its baseline by
/// at least this much to count as an onset. Together with the smoothing it rejects
/// the flux wobble a steadily-textured signal carries around its average, so the
/// calm/quiet clips neither drift-jitter nor heave; a real onset clears it easily.
const NOVELTY_FLOOR: f32 = 0.045;

/// Fraction of the brightness ceiling that remains at silence, so the swells
/// stay dimly legible in `Quiet`/`Idle` instead of vanishing.
const BREATH_FLOOR: f32 = 0.5;

/// Maximum upward shift of the front swell's baseline (fraction of height) at
/// full response and full low-band drive.
const FRONT_LIFT_UNIT: f32 = 0.14;
/// Extra amplitude the front swell gains at full low-band drive, as a fraction
/// of its base amplitude.
const FRONT_AMP_BOOST: f32 = 0.75;

/// How much the loudness envelope speeds the drift at full loudness, as a
/// multiplier added to the unit base. Kept small: normalized loudness sits high
/// even on steadily-quiet material, so a large loudness→speed term would inflate
/// the calm/quiet clips' motion — the loudness response lives in the brightness
/// breathing and the front lift, and the transient carries the rest.
const DRIFT_LOUD_GAIN: f32 = 0.0;
/// How much a transient (novelty above the flux baseline) speeds the drift: the
/// surface quickens for a moment on an energy rise (a main onset→motion lever).
/// Routed through drift, not brightness, so it adds motion without flicker; the
/// deadband keeps it from firing on steady material, so it is safe to make large.
const DRIFT_ONSET_GAIN: f32 = 6.0;
/// Extra swell amplitude every ridge gains at a full transient, as a fraction of
/// its base amplitude: the surface visibly heaves on an energy rise. Safe to make
/// generous because the transient novelty is ~0 on steady material, so the calm
/// clips do not heave.
const ONSET_AMP_GAIN: f32 = 0.85;

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
        key: "sensitivity",
        default: 1.0,
        min: 0.0,
        max: 2.0,
        doc: "audio-response depth: scales the loudness/transient drift quickening, the onset brightness pop and the onset swell heave (0 = plain drift)",
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
    /// Slow loudness envelope in `0.0..=1.0`; breathes the brightness and speeds
    /// the drift.
    loud_env: f32,
    /// Low-band envelope in `0.0..=1.0`; lifts and swells the front ridge.
    bass_env: f32,
    /// Fast loudness envelope in `0.0..=1.0`; drives the (mean-preserving) contrast
    /// sharpening, so motion tracks the level tightly without flicker.
    loud_fast: f32,
    /// Fast transient envelope in `0.0..=1.0`; briefly quickens the drift and
    /// heaves every swell on an energy rise. Driven by flux *above* its baseline.
    onset_env: f32,
    /// Short-smoothed spectral flux; the novelty is measured on this, so single-hop
    /// flux spikes (steady-texture noise) are attenuated before they can register.
    flux_smooth: f32,
    /// Slow baseline of the smoothed flux; the transient driver is flux above it,
    /// so steady spectral texture does not register as a running onset.
    flux_base: f32,
    /// Swell speed multiplier.
    drift: f32,
    /// Base swell amplitude (fraction of height).
    swell: f32,
    /// Front-swell response to the low band.
    response: f32,
    /// Overall brightness ceiling.
    level: f32,
    /// Audio-response depth: scales the drift quickening, the onset brightness pop
    /// and the onset swell heave.
    sensitivity: f32,
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
            loud_fast: 0.0,
            bass_env: 0.0,
            onset_env: 0.0,
            flux_smooth: 0.0,
            flux_base: 0.0,
            drift: 1.0,
            swell: 0.12,
            response: 0.6,
            level: 0.6,
            sensitivity: 1.0,
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
        read_param(&mut self.sensitivity, params, "sensitivity");
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
        self.loud_fast = 0.0;
        self.bass_env = 0.0;
        self.onset_env = 0.0;
        self.flux_smooth = 0.0;
        self.flux_base = 0.0;
        self.buf.clear();
        self.buf.resize(COLS * ROWS, 0.0);
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the swell phases and all three envelopes carry
        // across, so a live mapping never resets the drift or the breathing.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // Overall level breathes with the engine-normalized loudness (already
        // level-independent, `0..=1`), folded through a slow envelope. In
        // `Quiet`/`Idle` the input loudness falls and this eases the field down.
        let loud_target = f.loudness.clamp(0.0, 1.0);
        let loud_tau = if loud_target > self.loud_env {
            ATTACK_TAU
        } else {
            RELEASE_TAU
        };
        self.loud_env += (loud_target - self.loud_env) * follow_coeff(dt, loud_tau);

        // A second, tight loudness follower drives the contrast sharpening below. It
        // can track the level closely (where the slow brightness follower lags)
        // because it only steepens the mean-preserving contrast curve, never the
        // brightness — so it tightens the loudness↔motion correlation without
        // flickering the intensity.
        let fast_tau = if loud_target > self.loud_fast {
            CONTRAST_ATTACK_TAU
        } else {
            CONTRAST_RELEASE_TAU
        };
        self.loud_fast += (loud_target - self.loud_fast) * follow_coeff(dt, fast_tau);

        // The front swell alone lifts with the low band. `bands[0]` is already
        // normalized against its own recent average (1.0 = average), so map it
        // around that: the average reads ~0.5 and a strong bass surge climbs
        // toward 1.0. Fold it through its own slow follower — a calm swell, not a
        // twitch.
        let bass_target = (f.bands[0].clamp(0.0, 4.0) * 0.5).clamp(0.0, 1.0);
        self.bass_env += (bass_target - self.bass_env) * follow_coeff(dt, BASS_TAU);

        // The transient envelope acknowledges an energy rise with a heave of every
        // swell and a burst of drift speed, then settles. Its driver is the
        // normalized spectral flux *above a slow baseline* — a continuous novelty
        // measure, not the discrete onset flag. Subtracting the baseline keeps the
        // surface calm on steady material: deep ambient has high but flat flux, so
        // its novelty (and this envelope) stays near zero, while a real onset spikes
        // above the average. Folded, never a single-frame jump, so it swells not
        // strobes.
        let flux = f.flux.clamp(0.0, 1.0);
        self.flux_smooth += (flux - self.flux_smooth) * follow_coeff(dt, FLUX_SMOOTH_TAU);
        self.flux_base += (self.flux_smooth - self.flux_base) * follow_coeff(dt, FLUX_BASE_TAU);
        let novelty = (self.flux_smooth - self.flux_base - NOVELTY_FLOOR).max(0.0);
        let onset_tau = if novelty > self.onset_env {
            ONSET_ATTACK_TAU
        } else {
            ONSET_RELEASE_TAU
        };
        self.onset_env += (novelty - self.onset_env) * follow_coeff(dt, onset_tau);

        // Every swell keeps drifting even in silence, but a transient makes the
        // whole surface flow faster for a moment — the onset→motion acknowledgment.
        // Scaled by `sensitivity`; at `0` the drift is the plain calm base.
        let speed_gain = 1.0
            + self.sensitivity
                * (DRIFT_LOUD_GAIN * self.loud_env + DRIFT_ONSET_GAIN * self.onset_env);
        for (k, p) in self.phase.iter_mut().enumerate() {
            *p = (*p + dt * self.drift * speed_gain * SPEEDS[k]).rem_euclid(TWO_PI);
        }
    }

    fn render(&mut self, canvas: &mut Canvas) {
        let cx = (COLS as f32 - 1.0).max(1.0);
        let cy = (ROWS as f32 - 1.0).max(1.0);
        // Loudness sharpens the contrast (mean-preserving, so it steepens the swell
        // ridges — and the motion the drift produces — without moving the mean
        // brightness or flickering), driven by the tight follower so canvas motion
        // tracks the level.
        let contrast = self.contrast + CONTRAST_LOUD_GAIN * self.loud_fast;

        // Brightness breathes between a calm floor and the `level` ceiling with
        // loudness. The transient is deliberately kept out of the mean brightness —
        // it drives motion through the drift burst and the swell heave below — so
        // the onset acknowledgment never flickers the intensity.
        let brightness =
            (self.level * (BREATH_FLOOR + (1.0 - BREATH_FLOOR) * self.loud_env)).clamp(0.0, 1.0);

        // A transient heaves every swell: each ridge's amplitude grows for a
        // moment, so the whole surface visibly swells on an energy rise.
        let onset_amp_gain = 1.0 + self.sensitivity * ONSET_AMP_GAIN * self.onset_env;

        // Front-swell drive: an upward baseline shift and an amplitude boost,
        // both scaled by `response` and the low-band envelope.
        let front_drive = (self.response * self.bass_env).clamp(0.0, 1.5);
        let front_shift = (FRONT_LIFT_UNIT * front_drive).min(0.2);
        let front_amp_gain = 1.0 + FRONT_AMP_BOOST * self.bass_env;

        // Precompute each swell's effective baseline and amplitude.
        let mut baseline = BASELINES;
        let mut amp = [0.0f32; NSWELLS];
        for k in 0..NSWELLS {
            amp[k] = self.swell * AMP_SCALE[k] * onset_amp_gain;
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
        s.set("loud_fast", self.loud_fast);
        s.set("bass", self.bass_env);
        s.set("onset", self.onset_env);
        s.set("flux_smooth", self.flux_smooth);
        s.set("flux_base", self.flux_base);
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
        if let Some(v) = s.get("loud_fast") {
            self.loud_fast = v;
        }
        if let Some(v) = s.get("bass") {
            self.bass_env = v;
        }
        if let Some(v) = s.get("onset") {
            self.onset_env = v;
        }
        if let Some(v) = s.get("flux_smooth") {
            self.flux_smooth = v;
        }
        if let Some(v) = s.get("flux_base") {
            self.flux_base = v;
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
