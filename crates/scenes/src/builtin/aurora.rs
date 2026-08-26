//! `aurora` — a slow interference field, the calm end of the scene set and the
//! coarse half-block tier at its best.
//!
//! Two to three sinusoidal wavefronts drift across the field with different
//! directions, wavelengths and speeds; where they meet they interfere, so broad
//! bright ridges wander and re-form over seconds. A soft horizontal band brightens
//! the middle of the field like an aurora against a dark sky, and the whole
//! picture *breathes* with the music: loudness widens the band and brightens the
//! field, so a louder passage reads as a brighter, wider aurora and its steady
//! drift moves more of the canvas per frame. A transient — an energy rise,
//! measured as spectral flux — is
//! acknowledged with a quick swell of the band width and a burst of drift speed
//! that then settles; it is deliberately kept out of the mean brightness, so the
//! acknowledgment reads as motion, not a flicker. Every audio driver is folded
//! through an envelope (loudness on the order of a second; the transient on a fast
//! attack and a ~third-of-a-second release), so the field swells and settles but
//! never jitters or strobes.
//!
//! # Loudness normalization
//!
//! The audio drivers read the engine-normalized `loudness` — the mono rms divided
//! by a slow auto-reference, computed once on the DSP thread and already
//! level-independent (`0..=1`, sustained program ~0.6..=0.85) — and the normalized
//! spectral `flux` for the transient swell. The scene folds those through its own
//! envelopes; it no longer keeps a private loudness ceiling, since the engine now
//! performs that normalization for every consumer. The consequence stands: on any
//! material — quiet-mastered or brick-walled — sustained loud stretches push the
//! driver up and quiet stretches fall toward `0.0`, so two recordings at very
//! different absolute levels settle to the same response. The loudness reference is
//! a slow calibration; the transient envelope adds motion only on real onsets, so
//! the calm-at-rest invariant holds: in silence the field eases down to a dim,
//! slowly drifting floor.
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
//! | key           | default | range        | meaning                                           |
//! |---------------|---------|--------------|---------------------------------------------------|
//! | `drift`       | `1.0`   | `0.0..=4.0`  | base field speed: scales how fast the wavefronts move |
//! | `scale`       | `1.0`   | `0.2..=4.0`  | spatial frequency: how many wave cycles span the field |
//! | `band`        | `0.10`  | `0.02..=0.5` | base band half-width (fraction of height) at silence |
//! | `response`    | `0.30`  | `0.0..=0.6`  | loudness-to-width gain: extra band half-width at full (normalized) loudness |
//! | `sensitivity` | `1.0`   | `0.0..=2.0`  | audio-response depth: scales the loudness/transient drift quickening and the onset swell (`0` = plain drift) |
//! | `contrast`    | `2.2`   | `1.0..=4.0`  | contrast shaping: higher pushes darks darker and brights brighter |
//!
//! At the defaults a settled loud passage (normalized loudness `~1.0`) drives the
//! band's gaussian sigma to `band + response ≈ 0.40`, roughly four times the quiet
//! floor of `0.10`, and brightens the field from the silent floor to full — so the
//! lit band grows from a narrow mid-field swathe to nearly the whole height while
//! the whole picture brightens, a change that reads plainly on real music. A
//! transient briefly widens the band and quickens the drift on top of that.
//! `sensitivity` scales the onset width swell and its drift burst.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the three wave phases, the loudness envelope and the
//! transient envelope, so a hot reload resumes the drift, the current band width,
//! brightness and any in-flight onset swell rather than snapping back to the start.

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
/// Loudness-follower time constant while brightening/widening (seconds). Tight
/// enough that the brightness breathing tracks the music's level (so canvas
/// motion follows loudness), slow enough to stay a swell, not a jitter.
const ATTACK_TAU: f32 = 0.9;
/// Loudness-follower time constant while settling back (seconds).
const RELEASE_TAU: f32 = 2.2;
/// Onset-follower time constant while rising on transient flux (seconds): a fast
/// but not instantaneous attack, so the acknowledgment reads as a quick swell
/// rather than a single-frame strobe.
const ONSET_ATTACK_TAU: f32 = 0.03;
/// Onset-follower time constant while the transient decays (seconds): long
/// enough that each transient reads as one smooth lift-and-settle.
const ONSET_RELEASE_TAU: f32 = 0.14;
/// Short flux-smoothing time constant (seconds). Flux is smoothed over a few hops
/// before the novelty is measured, so a single-hop flux spike — the kind steady
/// ambient texture throws off constantly — is attenuated, while a real onset (flux
/// sustained high across several hops) survives. This is what separates a musical
/// transient from a quiet clip's spectral noise.
const FLUX_SMOOTH_TAU: f32 = 0.025;
/// Flux-baseline follower time constant (seconds). The transient driver is the
/// smoothed flux *above this slow average*, so a steadily-textured signal — deep
/// ambient with constant spectral churn, whose flux is high but flat — reads as
/// "no onset" and the field stays still. Only flux that rises above its own recent
/// average counts as an energy rise.
const FLUX_BASE_TAU: f32 = 1.2;
/// Deadband on the transient novelty: the smoothed flux must clear its baseline by
/// at least this much to count as an onset. Together with the smoothing it rejects
/// the flux wobble a steadily-textured signal carries around its average, so the
/// calm/quiet clips neither drift-jitter nor flicker; a real onset clears it.
const NOVELTY_FLOOR: f32 = 0.13;
/// Fast loudness-follower time constants (seconds) feeding the contrast sharpening
/// below. This one is deliberately *tight* — it tracks the music's level closely,
/// where the slow brightness follower lags. It can be fast without flickering
/// because it only drives the (mean-preserving) contrast, not the brightness.
const CONTRAST_ATTACK_TAU: f32 = 0.115;
/// Fast loudness-follower release time constant (seconds) for the contrast drive.
const CONTRAST_RELEASE_TAU: f32 = 0.27;
/// How much a loudness *swing* sharpens the interference contrast (added to the
/// base `contrast`). It is driven by the fast follower's departure from the slow
/// one — the recent change in loudness — so it is near zero on steadily-loud
/// material (the calm/quiet clips, whose level is high but flat) and only bites
/// when the music's loudness is actually moving. The contrast curve is symmetric
/// about the midpoint, so sharpening steepens the wave ridges — and thus the
/// motion the steady drift produces — while barely moving the mean brightness:
/// a loudness→motion coupling that tracks the dynamics tightly yet adds no flicker
/// and does not inflate the still clips, which the brightness lever cannot do.
/// How much a transient sharpens the contrast for a moment (mean-preserving, so
/// it adds motion without flicker, and it acts on the current frame so the motion
/// lands in time with the onset — helping the onset↔motion correlation).
const CONTRAST_ONSET_GAIN: f32 = 0.0;
const CONTRAST_LOUD_GAIN: f32 = 1.3;
/// Brightness of the field at silence, as a fraction of full. The field breathes
/// up from here with loudness rather than sitting at a fixed intensity, so the
/// whole picture — not just the band width — swells with the music: a louder
/// passage is brighter, a quiet passage dims and calms. Driven by the slow
/// follower, so the mean intensity never chases loudness noise (that would
/// flicker); the tight loudness↔motion coupling lives in the contrast instead.
const BRIGHT_FLOOR: f32 = 0.35;
/// How much the loudness envelope speeds the drift at full loudness. Kept at zero:
/// aurora's mean brightness ripples as the interference pattern drifts, so a
/// faster drift would flicker the field and inflate the calm/quiet clips (whose
/// normalized loudness sits high). Loudness shapes intensity and width, not speed;
/// the drift stays a steady, calm base.
const DRIFT_LOUD_GAIN: f32 = 0.0;
/// How much a transient (novelty above the flux baseline) briefly speeds the
/// drift. Small: an onset gets a short burst of motion, but the deadband keeps it
/// off steady material and the burst is sparse, so the flicker stays low.
const DRIFT_ONSET_GAIN: f32 = 5.0;
/// Extra band half-width (fraction of height) a full transient briefly adds — the
/// main onset acknowledgment: the lit band visibly swells on an energy rise, which
/// also moves canvas intensity (the onset→motion lever).
const ONSET_WIDTH_GAIN: f32 = 0.16;
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
        key: "sensitivity",
        default: 1.0,
        min: 0.0,
        max: 2.0,
        doc: "audio-response depth: scales the loudness/transient drift quickening and the onset swell (0 = drift only)",
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
    /// Slow loudness envelope in `0.0..=1.0`; breathes the band width and the field
    /// brightness.
    loud_env: f32,
    /// Fast loudness envelope in `0.0..=1.0`; drives the (mean-preserving) contrast
    /// sharpening, so motion tracks the level tightly without flicker.
    loud_fast: f32,
    /// Fast transient envelope in `0.0..=1.0`; briefly quickens the drift and
    /// widens the band on an energy rise. Driven by flux *above* its baseline.
    onset_env: f32,
    /// Short-smoothed spectral flux; the novelty is measured on this, so single-hop
    /// flux spikes (steady-texture noise) are attenuated before they can register.
    flux_smooth: f32,
    /// Slow baseline of the smoothed flux; the transient driver is flux above it,
    /// so steady spectral texture does not register as a running onset.
    flux_base: f32,
    /// Field speed multiplier.
    drift: f32,
    /// Spatial-frequency multiplier.
    scale: f32,
    /// Base band half-width (fraction of height) at silence.
    band: f32,
    /// Loudness-to-width gain.
    response: f32,
    /// Audio-response depth: scales the drift quickening and the onset swell.
    sensitivity: f32,
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
            loud_fast: 0.0,
            onset_env: 0.0,
            flux_smooth: 0.0,
            flux_base: 0.0,
            drift: 1.0,
            scale: 1.0,
            band: 0.10,
            response: 0.30,
            sensitivity: 1.0,
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
        read_param(&mut self.sensitivity, params, "sensitivity");
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
        self.loud_fast = 0.0;
        self.onset_env = 0.0;
        self.flux_smooth = 0.0;
        self.flux_base = 0.0;
        self.buf.clear();
        self.buf.resize(COLS * ROWS, 0.0);
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: wave phases, the loudness envelope and the field
        // buffer carry across, so a live mapping never resets the drift.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // The loudness envelope (already level-independent, `0..=1`) is folded
        // through a slow follower. It breathes the band width, the field
        // brightness and — below — the drift speed, so a louder passage makes the
        // whole picture swell and quicken. The scene keeps no private loudness
        // ceiling; the engine normalizes for every consumer.
        let target = f.loudness.clamp(0.0, 1.0);
        let tau = if target > self.loud_env {
            ATTACK_TAU
        } else {
            RELEASE_TAU
        };
        self.loud_env += (target - self.loud_env) * follow_coeff(dt, tau);

        // A second, tight loudness follower drives the contrast sharpening. It can
        // track the level closely because it never touches the mean brightness.
        let fast_tau = if target > self.loud_fast {
            CONTRAST_ATTACK_TAU
        } else {
            CONTRAST_RELEASE_TAU
        };
        self.loud_fast += (target - self.loud_fast) * follow_coeff(dt, fast_tau);

        // The transient envelope acknowledges an energy rise with a quick swell of
        // speed and band width, then settles. Its driver is the normalized
        // spectral flux *above a slow baseline* — a continuous novelty measure, not
        // the discrete onset flag. Subtracting the baseline is what keeps the calm
        // scene calm on steady material: deep ambient has high but flat flux, so
        // its novelty (and thus this envelope) stays near zero, while a real onset
        // spikes above the average. The attack is fast enough to read as a response
        // but folded, never a single-frame jump, so the field swells not strobes.
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

        // Drift never stops — this is the calm scene, so the field keeps moving
        // in silence — but a transient briefly quickens it. The onset burst is
        // gated by loudness (`onset_env * loud_env`): a transient in a loud passage
        // quickens the field hard, one in a quiet passage barely at all. That makes
        // the acknowledgment musically proportional AND keeps its motion correlated
        // with loudness rather than fighting it. Scaled by `sensitivity`; at `0`
        // the drift is the plain calm base.
        let onset_drive = self.onset_env * self.loud_env;
        let speed_gain = 1.0
            + self.sensitivity * (DRIFT_LOUD_GAIN * self.loud_env + DRIFT_ONSET_GAIN * onset_drive);
        for (k, p) in self.phase.iter_mut().enumerate() {
            *p = (*p + dt * self.drift * speed_gain * WAVES[k].speed).rem_euclid(TWO_PI);
        }
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
        let onset_drive = self.onset_env * self.loud_env;
        // Loudness sharpens the interference contrast, and a transient sharpens it
        // further for a moment. Because the contrast curve is symmetric around the
        // midpoint, sharpening it barely moves the field's mean brightness (so it
        // adds no flicker), but it steepens the wave ridges, so the steady drift
        // moves more canvas intensity per frame — a clean loudness/onset→motion
        // coupling that does not fight the calm identity. The transient term acts
        // on the current frame (no burst to propagate), so its motion lands in time
        // with the onset.
        let contrast = self.contrast
            + CONTRAST_LOUD_GAIN * self.loud_fast
            + self.sensitivity * CONTRAST_ONSET_GAIN * onset_drive;
        // Band half-width grows with the loudness envelope and swells briefly on a
        // transient (the onset acknowledgment). The onset swell is gated by loudness
        // (`onset_env * loud_env`) so it is proportional to how loud the passage is,
        // and scaled by `sensitivity`.
        let sigma = (self.band
            + self.response * self.loud_env
            + self.sensitivity * ONSET_WIDTH_GAIN * onset_drive)
            .max(MIN_SIGMA);
        let inv_two_sigma2 = 1.0 / (2.0 * sigma * sigma);
        // Overall field brightness breathes up from a silent floor with loudness.
        // Loudness now shapes intensity as well as band width, so a louder passage
        // reads as a brighter, wider, faster field — a deeper response than the
        // width alone. The transient is deliberately NOT routed into brightness:
        // it drives motion through drift and a small width swell, so the onset
        // acknowledgment never flickers the mean intensity.
        let brightness = (BRIGHT_FLOOR + (1.0 - BRIGHT_FLOOR) * self.loud_env).clamp(0.0, 1.0);
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
                // Interference sum in [-N, N] -> [0, 1], then contrast-shaped,
                // gated by the band so the field keeps real darks and brights, and
                // finally scaled by the breathing brightness.
                let n = 0.5 + 0.5 * (s / NWAVES as f32);
                *cell = shape_contrast(n, contrast) * band_gain * brightness;
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
        if let Some(loud) = s.get("loud") {
            self.loud_env = loud;
        }
        if let Some(lf) = s.get("loud_fast") {
            self.loud_fast = lf;
        }
        if let Some(onset) = s.get("onset") {
            self.onset_env = onset;
        }
        if let Some(fs) = s.get("flux_smooth") {
            self.flux_smooth = fs;
        }
        if let Some(fb) = s.get("flux_base") {
            self.flux_base = fb;
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
