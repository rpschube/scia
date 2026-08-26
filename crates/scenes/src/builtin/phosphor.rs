//! `phosphor` — a Lissajous trace burned onto a decaying phosphor screen.
//!
//! A generative parametric figure `x = A·sin(a·θ + δ)`, `y = A·sin(b·θ)` is
//! sampled along `θ` each frame and deposited into a persistence field. The
//! field decays exponentially every frame — the phosphor afterglow — so the
//! figure leaves a fading trail as it changes. The phase offset `δ` drifts
//! slowly, so the figure *precesses* and the trail smears the way a real
//! Lissajous scope wanders. The frequency ratio `a:b` eases gently with the
//! band levels, staying close to small integer-ish values so the figure always
//! reads as a coherent curve rather than dissolving into noise. Amplitude opens
//! on an onset and relaxes slowly, riding on a base amplitude that follows
//! loudness, so the figure swells with the music and blooms on transients. The
//! **deposit brightness** rides an energy envelope — a low quiet floor lifted by
//! loudness and punched hard by an onset — so the trace burns bright on a loud
//! beat and stays dim and calm when quiet; and the **precession speed** rides
//! loudness, so the figure wanders fast (its trail smearing over more of the
//! screen, moving more) when loud and nearly holds still when quiet. Driving the
//! response through brightness and drift rather than the figure's size is what
//! ties the beam's frame-to-frame motion to the music's level without leaving a
//! large, static figure sitting still.
//!
//! A bright *beam head* — a few [`crate::canvas::Primitive::Point`]s at a point
//! that races around the figure — is overdrawn on top of the dim decaying
//! field, giving the single-hue CRT feel: a dim persistent trace under a hot
//! moving spot. The head's brightness rides loudness and it flares on an onset.
//! The field is coloured from a dim palette slot and the beam from a brighter
//! one.
//!
//! # Internal resolution
//!
//! The persistence field is a fixed `96 × 54` grid (16:9), allocated once at
//! [`Scene::init`] and decayed and re-deposited in place every frame, so a
//! warmed scene does no per-frame allocation. Positions are aspect-corrected
//! against the field's own 16:9 proportion so the figure is drawn round in the
//! field's native cells (the presenter then downsamples the whole field to the
//! surface, exactly as `aurora` does).
//!
//! # Quiet / Idle
//!
//! Loudness and the onset envelope fall toward zero in quiet passages, so the
//! deposit dims, the precession stalls and the figure collapses toward a small,
//! nearly still resting figure. Because normalized loudness reads a steady quiet
//! drone as loud, a gate on the **raw** RMS damps the deposit and precession on
//! genuinely quiet material too. When the DSP thread reports
//! [`scia_core::Activity::Idle`] the scene stops depositing entirely and the
//! field simply fades to black — the screen goes dark, like an idle scope.
//!
//! # Parameters
//!
//! | key          | default | range        | meaning                                                       |
//! |--------------|---------|--------------|---------------------------------------------------------------|
//! | `freq_a`     | `3.0`   | `1.0..=8.0`  | horizontal base frequency of the Lissajous figure             |
//! | `freq_b`     | `2.0`   | `1.0..=8.0`  | vertical base frequency of the Lissajous figure               |
//! | `decay`      | `0.17`  | `0.05..=3.0` | phosphor persistence time constant (seconds)                  |
//! | `precession` | `0.6`   | `0.0..=4.0`  | precession base speed (loudness rides it): how fast `δ` drifts |
//! | `swell`      | `0.45`  | `0.0..=1.0`  | loudness-to-amplitude gain: how much loudness opens the figure |
//! | `open`       | `0.35`  | `0.0..=1.0`  | onset amplitude opening: extra figure amplitude on a transient |
//!
//! `freq_a`, `freq_b`, `decay`, `precession`, `swell` and `open` are live
//! tuning scalars: the host re-applies them every frame through
//! [`Scene::apply_params`], each clamped to its manifest range on read.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the precession phase, the beam-head phase, the eased
//! frequency ratio, and the loudness, raw-RMS and onset envelopes, so a hot
//! reload resumes the drift and the response rather than snapping back to the
//! start.
//! The persistence field itself is **not** carried: the whole figure is
//! re-deposited every frame, so it re-appears immediately and only the fading
//! afterglow re-accumulates over one decay time — the same judgment `starfall`
//! makes for its star positions.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};
use scia_core::Activity;

/// Field columns. `96 × 54` is 16:9; see the module docs.
const COLS: usize = 96;
/// Field rows.
const ROWS: usize = 54;
/// `2π`, one full turn.
const TWO_PI: f32 = std::f32::consts::TAU;
/// Number of samples taken along `θ` each frame. Dense enough that the deposited
/// figure reads as a continuous curve at the field resolution.
const SAMPLES: usize = 512;
/// Brightness deposited per sample into a field cell (before clamping). Small so
/// the curve builds up smoothly rather than saturating in one frame.
const DEPOSIT: f32 = 0.35;
/// Resting-figure amplitude at silence (fraction of the field half-height), so a
/// quiet-but-active passage still shows a small figure instead of a dot.
const REST_AMP: f32 = 0.12;
/// Loudness-follower time constant (seconds). Kept fairly short so the base
/// amplitude tracks the music's level rather than lagging into a smear, which is
/// what lets the figure's size move *with* loudness.
const LOUD_TAU: f32 = 0.2;
/// Onset-envelope decay time constant (seconds): the figure opens fast on a
/// transient and relaxes over roughly this long. Short enough that each onset
/// reads as a distinct bloom rather than a slow swell.
const ONSET_TAU: f32 = 0.3;
/// Frequency-ratio easing time constant (seconds): the a:b ratio drifts with the
/// bands only very slowly, so the figure stays readable.
const FREQ_TAU: f32 = 2.0;
/// Raw-RMS smoothing time constant (seconds) for the level gate below.
const RMS_TAU: f32 = 0.25;
/// Raw-RMS level at which the level gate reaches full strength. The normalized
/// loudness reads a steady quiet drone as loud, so without this the deposit and
/// precession would keep such a clip moving; gating them on raw RMS keeps
/// genuinely quiet material calm while leaving all louder clips untouched.
const RMS_REF: f32 = 0.06;
/// Maximum band-driven deviation of a base frequency (cycles), so the ratio
/// stays near its integer-ish base and the figure never dissolves.
const FREQ_DEV: f32 = 0.8;
/// Beam-head angular speed along the figure (radians of `θ` per second).
const HEAD_SPEED: f32 = 9.0;
/// Number of trailing points drawn for the bright beam head.
const HEAD_POINTS: usize = 4;
/// Beam-head point diameter (fraction of canvas height).
const HEAD_SIZE: f32 = 0.02;
/// Palette slot the dim persistence field is coloured with (teal).
const FIELD_SLOT: crate::Slot = 1;
/// Palette slot the bright beam head is coloured with (cyan).
const BEAM_SLOT: crate::Slot = 2;

/// `phosphor`'s parameter manifest: the keys a preset may set, with the
/// defaults, ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "freq_a",
        default: 3.0,
        min: 1.0,
        max: 8.0,
        doc: "horizontal base frequency of the Lissajous figure",
    },
    ParamSpec {
        key: "freq_b",
        default: 2.0,
        min: 1.0,
        max: 8.0,
        doc: "vertical base frequency of the Lissajous figure",
    },
    ParamSpec {
        key: "decay",
        default: 0.17,
        min: 0.05,
        max: 3.0,
        doc: "phosphor persistence time constant (seconds)",
    },
    ParamSpec {
        key: "precession",
        default: 0.6,
        min: 0.0,
        max: 4.0,
        doc: "precession speed: how fast the phase offset drifts (rad/s)",
    },
    ParamSpec {
        key: "swell",
        default: 0.45,
        min: 0.0,
        max: 1.0,
        doc: "loudness-to-amplitude gain: how much loudness opens the figure",
    },
    ParamSpec {
        key: "open",
        default: 0.35,
        min: 0.0,
        max: 1.0,
        doc: "onset amplitude opening: extra figure amplitude on a transient",
    },
];

/// The Lissajous phosphor scene.
#[derive(Clone, Debug)]
pub struct Phosphor {
    // --- field, sized at init ------------------------------------------
    /// Pre-allocated persistence field, `COLS * ROWS` values, row-major.
    buf: Vec<f32>,

    // --- live state ----------------------------------------------------
    /// Precession phase `δ` in radians, wrapped to `0..2π`.
    precess: f32,
    /// Beam-head phase along `θ` in radians, wrapped to `0..2π`.
    head: f32,
    /// Eased horizontal frequency (cycles), drifting with the bass band.
    a_cur: f32,
    /// Eased vertical frequency (cycles), drifting with the treble band.
    b_cur: f32,
    /// Smoothed loudness in `0.0..=1.0`, the base of the amplitude.
    loud_env: f32,
    /// Smoothed raw RMS, for the level gate on the continuous responses.
    rms_env: f32,
    /// Onset envelope in `0.0..=1.0`: snaps to 1 on an onset, decays to 0.
    onset_env: f32,
    /// Current figure amplitude (fraction of the field half-height), computed in
    /// [`Scene::update`] and read by [`Scene::render`] and the tests.
    amp: f32,
    /// Whether the scene is depositing (false while `Idle`, so the field fades).
    depositing: bool,
    /// Previous frame's onset flag, for rising-edge detection.
    prev_onset: bool,
    /// Previous frame's `onset_age_ms`, to catch a fresh onset that resets the age.
    prev_onset_age_ms: f32,

    // --- parameters ----------------------------------------------------
    freq_a: f32,
    freq_b: f32,
    decay: f32,
    precession: f32,
    swell: f32,
    open: f32,
}

impl Phosphor {
    /// A `phosphor` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters and size the field buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: vec![0.0; COLS * ROWS],
            precess: 0.0,
            head: 0.0,
            a_cur: 3.0,
            b_cur: 2.0,
            loud_env: 0.0,
            rms_env: 0.0,
            onset_env: 0.0,
            amp: REST_AMP,
            depositing: true,
            prev_onset: false,
            prev_onset_age_ms: 0.0,
            freq_a: 3.0,
            freq_b: 2.0,
            decay: 0.17,
            precession: 0.6,
            swell: 0.45,
            open: 0.35,
        }
    }

    /// Refresh the tuning scalars from `params`, and only those — the field, the
    /// phases and the envelopes are left untouched so a live re-apply does not
    /// reset the animation. Shared by [`Scene::init`] and [`Scene::apply_params`].
    /// Allocation-free.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.freq_a, params, "freq_a");
        read_param(&mut self.freq_b, params, "freq_b");
        read_param(&mut self.decay, params, "decay");
        read_param(&mut self.precession, params, "precession");
        read_param(&mut self.swell, params, "swell");
        read_param(&mut self.open, params, "open");
    }
}

impl Default for Phosphor {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Phosphor {
    fn id(&self) -> &'static str {
        "phosphor"
    }

    fn mood(&self) -> &'static str {
        "retro"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.precess = 0.0;
        self.head = 0.0;
        self.a_cur = self.freq_a;
        self.b_cur = self.freq_b;
        self.loud_env = 0.0;
        self.rms_env = 0.0;
        self.onset_env = 0.0;
        self.amp = REST_AMP;
        self.depositing = true;
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
        self.buf.clear();
        self.buf.resize(COLS * ROWS, 0.0);
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the field, phases, frequency ease and envelopes
        // all carry across, so a live mapping never resets the animation.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Track raw RMS for the level gate: genuinely quiet material (low RMS)
        // damps the continuous responses even when normalized loudness reads high.
        self.rms_env += (f.rms.max(0.0) - self.rms_env) * (1.0 - decay(dt, RMS_TAU));
        let rms_gate = (self.rms_env / RMS_REF).clamp(0.0, 1.0);

        // Precession drifts the phase offset; the beam head races around the
        // figure. Both advance regardless of audio so the trace always lives, but
        // the precession speed rides loudness — the figure wanders faster when the
        // music is loud and nearly holds still when it is quiet — so the field's
        // frame-to-frame motion tracks the music's level, not just its onsets.
        // `self.loud_env` is the previous frame's value here; it is refreshed just
        // below, which is fine for a slowly-drifting phase.
        let precess_rate = self.precession * (0.25 + 3.6 * self.loud_env * rms_gate);
        self.precess = (self.precess + dt * precess_rate).rem_euclid(TWO_PI);
        self.head = (self.head + dt * HEAD_SPEED).rem_euclid(TWO_PI);

        // Ease the frequency ratio with the bands. Bands are normalized to
        // 1.0 = recent average; the deviation from average nudges each base
        // frequency by at most FREQ_DEV, eased slowly so the figure stays read-
        // able. Bass drives the horizontal frequency, treble the vertical.
        let a_target = self.freq_a + ((f.bands[0] - 1.0) * 0.3).clamp(-FREQ_DEV, FREQ_DEV);
        let b_target = self.freq_b + ((f.bands[2] - 1.0) * 0.3).clamp(-FREQ_DEV, FREQ_DEV);
        let ke = 1.0 - decay(dt, FREQ_TAU);
        self.a_cur += (a_target - self.a_cur) * ke;
        self.b_cur += (b_target - self.b_cur) * ke;

        // Loudness follower: ride the base amplitude on the engine-normalized
        // loudness (0..1), not the raw rms, smoothed toward its target.
        let loud = f.loudness.clamp(0.0, 1.0);
        let kl = 1.0 - decay(dt, LOUD_TAU);
        self.loud_env += (loud - self.loud_env) * kl;

        // Onset envelope: snap to full on a fresh onset (fast attack), otherwise
        // decay (slow release). Fire on a rising edge, or when a fresh onset
        // resets `onset_age_ms` below the previous frame's value.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            self.onset_env = 1.0;
        } else {
            self.onset_env *= decay(dt, ONSET_TAU);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;

        // Amplitude: a small resting figure, opened by loudness and blown open by
        // an onset. Clamped so it never exceeds the field half-height.
        self.amp =
            (REST_AMP + self.swell * self.loud_env + self.open * self.onset_env).clamp(0.0, 1.0);

        // Idle: stop depositing so the field fades to black. Any other activity
        // keeps the trace alive (Quiet shows the small resting figure).
        self.depositing = f.activity != Activity::Idle;
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // Decay the whole persistence field one frame — the phosphor afterglow.
        let d = decay_dt_from_frame(self.decay);
        for cell in &mut self.buf {
            *cell *= d;
        }

        // Aspect-correct against the field's own 16:9 so the figure is round in
        // the field's native cells.
        let field_aspect = COLS as f32 / ROWS as f32;
        let ax = 0.5 * self.amp / field_aspect;
        let ay = 0.5 * self.amp;
        let a = self.a_cur;
        let b = self.b_cur;
        let delta = self.precess;

        // Deposit brightness rides an energy envelope — a low quiet floor lifted
        // by loudness and punched hard by an onset — so the trace is dim and calm
        // when the music is quiet and burns bright on a loud beat. Driving the
        // response through brightness (not the figure's size) keeps the beam's
        // frame-to-frame motion tied to the music without the "big stable figure"
        // that an amplitude-driven response leaves sitting still. The strong onset
        // term is what gives a transient a sharp, well-correlated brightness jump.
        let rms_gate = (self.rms_env / RMS_REF).clamp(0.0, 1.0);
        let energy = (0.1 + (0.6 * self.loud_env + 1.1 * self.onset_env) * rms_gate).min(1.8);

        if self.depositing {
            // Sample the Lissajous curve along θ and deposit into the field.
            let dep = DEPOSIT * energy;
            for s in 0..SAMPLES {
                let theta = (s as f32 / SAMPLES as f32) * TWO_PI;
                let x = 0.5 + ax * (a * theta + delta).sin();
                let y = 0.5 + ay * (b * theta).sin();
                deposit(&mut self.buf, x, y, dep);
            }
        }

        canvas.field(
            COLS as u16,
            ROWS as u16,
            &self.buf,
            Style::new(FIELD_SLOT, 1.0),
        );

        // Overdraw the bright beam head: a few points trailing the head phase,
        // fading back along the curve. Skipped while idle (nothing is lit).
        if self.depositing {
            // The head flares on a transient: an onset briefly fattens the hot
            // spot, so a beat reads as a bright bloom racing the figure rather
            // than a steady dot. Its brightness rides loudness so the moving spot
            // dims in quiet passages and blazes when loud.
            let flare = 1.0 + 0.8 * self.onset_env;
            let head_bright = 0.3 + 0.7 * self.loud_env;
            for i in 0..HEAD_POINTS {
                let back = i as f32 * 0.05;
                let theta = self.head - back;
                let x = 0.5 + ax * (a * theta + delta).sin();
                let y = 0.5 + ay * (b * theta).sin();
                let bright = (1.0 - (i as f32 / HEAD_POINTS as f32) * 0.7) * head_bright;
                let size = HEAD_SIZE * (1.0 - back) * flare;
                canvas.point(x, y, size, Style::new(BEAM_SLOT, bright));
            }
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("precess", self.precess);
        s.set("head", self.head);
        s.set("a_cur", self.a_cur);
        s.set("b_cur", self.b_cur);
        s.set("loud_env", self.loud_env);
        s.set("rms_env", self.rms_env);
        s.set("onset_env", self.onset_env);
        s.set("prev_onset", if self.prev_onset { 1.0 } else { 0.0 });
        s.set("prev_onset_age_ms", self.prev_onset_age_ms);
        s
    }

    fn restore(&mut self, s: SceneState) {
        if let Some(v) = s.get("precess") {
            self.precess = v;
        }
        if let Some(v) = s.get("head") {
            self.head = v;
        }
        if let Some(v) = s.get("a_cur") {
            self.a_cur = v;
        }
        if let Some(v) = s.get("b_cur") {
            self.b_cur = v;
        }
        if let Some(v) = s.get("loud_env") {
            self.loud_env = v;
        }
        if let Some(v) = s.get("rms_env") {
            self.rms_env = v;
        }
        if let Some(v) = s.get("onset_env") {
            self.onset_env = v;
        }
        if let Some(v) = s.get("prev_onset") {
            self.prev_onset = v >= 0.5;
        }
        if let Some(v) = s.get("prev_onset_age_ms") {
            self.prev_onset_age_ms = v;
        }
    }
}

/// Deposit `amount` into the field cell nearest `(x, y)` (both `0.0..=1.0`),
/// accumulating and clamping the cell to `1.0` so the buffer never grows past
/// full brightness (which would slow its decay). Out-of-range points are ignored.
#[inline]
fn deposit(buf: &mut [f32], x: f32, y: f32, amount: f32) {
    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
        return;
    }
    let cx = ((x * (COLS as f32 - 1.0)).round() as isize).clamp(0, COLS as isize - 1) as usize;
    let cy = ((y * (ROWS as f32 - 1.0)).round() as isize).clamp(0, ROWS as isize - 1) as usize;
    let idx = cy * COLS + cx;
    buf[idx] = (buf[idx] + amount).min(1.0);
}

/// The per-frame decay multiplier for a phosphor field with time constant
/// `tau` seconds, assumed at a nominal 60 fps. Kept frame-rate-nominal (rather
/// than driven by the render `dt`, which `render` does not receive) so the
/// afterglow length reads consistently; `tau <= 0` collapses to an instant wipe.
#[inline]
fn decay_dt_from_frame(tau: f32) -> f32 {
    const FRAME_DT: f32 = 1.0 / 60.0;
    if tau > 0.0 {
        (-FRAME_DT / tau).exp()
    } else {
        0.0
    }
}

/// The per-step multiplier of an exponential decay with time constant `tau` over
/// `dt` seconds. `tau <= 0` (or a non-finite `dt`) collapses to an instant decay
/// (multiplier `0`).
#[inline]
fn decay(dt: f32, tau: f32) -> f32 {
    if tau > 0.0 && dt.is_finite() {
        (-dt / tau).exp()
    } else {
        0.0
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
            .expect("key is a phosphor parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::{Activity, FeatureSnapshot};

    /// The first argument is the engine-normalized `loudness` the scene drives
    /// from (mirrored into `rms` so the snapshot stays plausible).
    fn snap(loudness: f32, onset: bool, onset_age_ms: f32) -> FeatureSnapshot {
        FeatureSnapshot {
            rms: loudness,
            loudness,
            onset,
            onset_age_ms,
            ..FeatureSnapshot::default()
        }
    }

    fn quiet() -> FeatureSnapshot {
        snap(0.0, false, 60_000.0)
    }

    fn inited() -> Phosphor {
        let mut s = Phosphor::new();
        s.init(&SceneCtx::default());
        s
    }

    /// Render one frame and return the sole field's values.
    fn render_field(scene: &mut Phosphor) -> Vec<f32> {
        let mut c = Canvas::new(16.0 / 9.0);
        scene.render(&mut c);
        let prims = c.primitives();
        let field = prims
            .iter()
            .find(|p| matches!(p, Primitive::Field { .. }))
            .expect("phosphor draws a field");
        c.field_of(field).expect("field values").to_vec()
    }

    #[test]
    fn onset_opens_amplitude() {
        // A steady quiet-active signal settles to a small resting figure; an
        // onset on top of the same loudness must open the amplitude wider.
        let mut steady = inited();
        for _ in 0..30 {
            steady.update(&snap(0.2, false, 5_000.0), 0.05);
        }
        let calm_amp = steady.amp;

        let mut hit = inited();
        for _ in 0..30 {
            hit.update(&snap(0.2, false, 5_000.0), 0.05);
        }
        // Same history, then a fresh onset at equal loudness.
        hit.update(&snap(0.2, true, 0.0), 0.05);
        assert!(
            hit.onset_env > 0.9,
            "a fresh onset snaps the onset envelope up: {}",
            hit.onset_env
        );
        assert!(
            hit.amp > calm_amp + 0.1,
            "the onset opens the amplitude: {} should exceed calm {}",
            hit.amp,
            calm_amp
        );
    }

    #[test]
    fn field_decays_toward_zero_without_input() {
        // Light the field with active frames, then feed Idle (no input): the
        // scene stops depositing and the phosphor fades toward black.
        let mut s = inited();
        for _ in 0..20 {
            s.update(&snap(0.6, true, 0.0), 0.05);
            let _ = render_field(&mut s);
        }
        let lit: f32 = render_field(&mut s).iter().sum();
        assert!(lit > 0.0, "the field is lit after active input: {lit}");

        // Now go idle for a long stretch.
        let mut idle = quiet();
        idle.activity = Activity::Idle;
        for _ in 0..400 {
            s.update(&idle, 0.05);
            let _ = render_field(&mut s);
        }
        let faded: f32 = render_field(&mut s).iter().sum();
        assert!(
            faded < lit * 0.01,
            "the field decays toward zero without input: {faded} vs lit {lit}"
        );
    }

    #[test]
    fn field_values_stay_in_bounds() {
        let mut s = inited();
        for _ in 0..50 {
            s.update(&snap(0.9, true, 0.0), 0.05);
            let field = render_field(&mut s);
            for (i, &v) in field.iter().enumerate() {
                assert!((0.0..=1.0).contains(&v), "cell {i} value {v} out of range");
            }
        }
    }

    #[test]
    fn state_restore_carries_continuity() {
        let warm = snap(0.6, false, 5_000.0);
        let next = snap(0.2, false, 5_100.0);

        // Reference: advance several frames, snapshot, advance one more.
        let mut a = inited();
        for _ in 0..30 {
            a.update(&warm, 0.05);
        }
        let state = a.state();
        a.update(&next, 0.05);

        // Restored: a fresh scene that restores the snapshot and advances the
        // same frame must reproduce the driving scalars exactly.
        let mut b = inited();
        b.restore(state);
        b.update(&next, 0.05);

        assert!((a.precess - b.precess).abs() < 1e-6, "precession carried");
        assert!((a.a_cur - b.a_cur).abs() < 1e-6, "frequency ease carried");
        assert!((a.loud_env - b.loud_env).abs() < 1e-6, "loudness carried");
        assert!((a.amp - b.amp).abs() < 1e-6, "amplitude reproduced");

        // Control: without the restore the drift and envelope are lost.
        let mut c = inited();
        c.update(&next, 0.05);
        assert!(
            (a.loud_env - c.loud_env).abs() > 1e-3,
            "a scene that skipped restore should not match"
        );
    }
}
