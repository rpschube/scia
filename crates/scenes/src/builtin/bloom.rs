//! `bloom` — a six-fold kaleidoscope mandala breathing with the mids, its core
//! flashing on every onset. The maximal, screensaver-grade scene.
//!
//! One 60° wedge of structure is computed — a small fan of curved arms whose
//! radius and thickness breathe with the mid band — and mirrored six-fold by
//! rotation, so the whole figure is a mandala with true six-fold rotational
//! symmetry. The arms in the wedge each have their own length and bend, so the
//! symmetry is exactly six-fold rather than an accidental higher order. A slow
//! global rotation turns the whole mandala; loudness eases its overall radius,
//! so it opens with the music. Every detected onset flashes a bright core at the
//! centre — a cluster of concentric points with a fast attack and slower decay.
//!
//! # Quiet / Idle
//!
//! As the signal falls quiet the overall radius contracts (loudness drives it) and
//! the global rotation slows (and nearly stops when the DSP thread reports
//! [`scia_core::Activity::Idle`]), so the mandala winds down to a dim, small,
//! slowly turning figure rather than freezing at full spread.
//!
//! # Geometry
//!
//! Positions are aspect-corrected exactly as `sonar` does: a point at
//! aspect-corrected radius `r` and angle `θ` lands at `0.5 + r·cos θ / aspect`
//! horizontally and `0.5 + r·sin θ` vertically, so the mandala reads as a
//! physical circle on any surface. Radii are fractions of the canvas half-height.
//! The six-fold copies are emitted at `rot + k·60°` for `k` in `0..6`, so a
//! rotation of the rendered figure by 60° maps it onto itself.
//!
//! # Determinism
//!
//! The figure is pure per-frame geometry from `point` primitives — no persistence
//! field, no RNG, no wall clock. The arm shapes are fixed constants; only the
//! rotation phase and the smoothed envelopes carry any state.
//!
//! # Parameters
//!
//! | key       | default | range        | meaning                                                     |
//! |-----------|---------|--------------|-------------------------------------------------------------|
//! | `rotate`  | `0.05`  | `0.0..=0.5`  | global rotation speed (revolutions/second) while active     |
//! | `radius`  | `0.6`   | `0.1..=1.0`  | base overall radius (fraction of the canvas half-height)    |
//! | `swell`   | `0.4`   | `0.0..=1.0`  | loudness-to-radius gain: how much loudness opens the mandala |
//! | `breathe` | `0.5`   | `0.0..=1.0`  | mid-band gain on petal radius and thickness (the breath)    |
//! | `flash`   | `0.8`   | `0.0..=1.0`  | onset core-flash gain                                       |
//! | `size`    | `1.0`   | `0.3..=3.0`  | point size multiplier                                       |
//!
//! All parameters are live tuning scalars, re-applied every frame through
//! [`Scene::apply_params`] and clamped to their manifest range on read.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the rotation phase and the loudness, mid and onset
//! envelopes, so a hot reload resumes the turn and the response rather than
//! snapping the mandala back to its start angle.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};
use scia_core::Activity;

/// `2π`, one full turn.
const TWO_PI: f32 = std::f32::consts::TAU;
/// The rotational symmetry order — a six-fold mandala.
const SYMMETRY: usize = 6;
/// The wedge angle one symmetry copy spans (radians).
const WEDGE: f32 = TWO_PI / SYMMETRY as f32;
/// Points sampled along each arm from the centre outward.
const SEGMENTS: usize = 9;
/// Per-arm outer-length fractions (of the overall radius). Distinct per arm so
/// the wedge has internal structure and the symmetry is exactly six-fold.
const ARM_LEN: [f32; 4] = [1.0, 0.62, 0.86, 0.72];
/// Per-arm angular bend across the arm (radians). Distinct per arm, as above.
const ARM_BEND: [f32; 4] = [0.10, -0.20, 0.06, -0.30];
/// Loudness-follower time constant (seconds).
const LOUD_TAU: f32 = 0.3;
/// Mid-band-follower time constant (seconds): the breath eases rather than jumps.
const MID_TAU: f32 = 0.25;
/// Onset (core-flash) decay time constant (seconds): fast attack, slower release.
const ONSET_TAU: f32 = 0.35;
/// Overall radius at silence, as a fraction of the base radius, so a quiet
/// mandala contracts to a small figure instead of vanishing.
const RADIUS_FLOOR: f32 = 0.25;
/// Mid-band gain on petal radius bulge (scaled by the `breathe` param).
const BREATHE_R: f32 = 0.35;
/// Base point diameter (fraction of canvas height) before the `size` param.
const POINT_SIZE: f32 = 0.016;
/// Number of concentric points in the bright central core.
const CORE_POINTS: usize = 4;
/// Base core diameter (fraction of canvas height) before the `size` param.
const CORE_SIZE: f32 = 0.05;
/// The core's steady glow when no onset is flashing it.
const CORE_GLOW: f32 = 0.12;

/// Palette slot for the inner arm points (teal).
const INNER_SLOT: crate::Slot = 1;
/// Palette slot for the mid arm points (cyan).
const MID_SLOT: crate::Slot = 2;
/// Palette slot for the outer arm points (amber).
const OUTER_SLOT: crate::Slot = 3;
/// Palette slot for the bright flashing core (near-white).
const CORE_SLOT: crate::Slot = 7;

/// `bloom`'s parameter manifest: the keys a preset may set, with the defaults,
/// ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "rotate",
        default: 0.05,
        min: 0.0,
        max: 0.5,
        doc: "global rotation speed (revolutions/second) while active",
    },
    ParamSpec {
        key: "radius",
        default: 0.6,
        min: 0.1,
        max: 1.0,
        doc: "base overall radius (fraction of the canvas half-height)",
    },
    ParamSpec {
        key: "swell",
        default: 0.4,
        min: 0.0,
        max: 1.0,
        doc: "loudness-to-radius gain: how much loudness opens the mandala",
    },
    ParamSpec {
        key: "breathe",
        default: 0.5,
        min: 0.0,
        max: 1.0,
        doc: "mid-band gain on petal radius and thickness (the breath)",
    },
    ParamSpec {
        key: "flash",
        default: 0.8,
        min: 0.0,
        max: 1.0,
        doc: "onset core-flash gain",
    },
    ParamSpec {
        key: "size",
        default: 1.0,
        min: 0.3,
        max: 3.0,
        doc: "point size multiplier",
    },
];

/// The kaleidoscope-mandala scene.
#[derive(Clone, Debug)]
pub struct Bloom {
    // --- geometry, captured at init ------------------------------------
    /// Aspect ratio captured at init, used to place the mandala on the canvas.
    aspect: f32,

    // --- live state ----------------------------------------------------
    /// Global rotation phase in radians, wrapped to `0..2π`.
    rot: f32,
    /// Smoothed loudness in `0.0..=1.0`, easing the overall radius.
    loud_env: f32,
    /// Smoothed mid band in `0.0..=1.0`, driving the petal breath.
    mid_env: f32,
    /// Onset (core-flash) envelope in `0.0..=1.0`: snaps to 1 on an onset, decays.
    onset_env: f32,
    /// Previous frame's onset flag, for rising-edge detection.
    prev_onset: bool,
    /// Previous frame's `onset_age_ms`, to catch a fresh onset that resets the age.
    prev_onset_age_ms: f32,

    // --- parameters ----------------------------------------------------
    rotate: f32,
    radius: f32,
    swell: f32,
    breathe: f32,
    flash: f32,
    size: f32,
}

impl Bloom {
    /// A `bloom` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            aspect: 1.0,
            rot: 0.0,
            loud_env: 0.0,
            mid_env: 0.0,
            onset_env: 0.0,
            prev_onset: false,
            prev_onset_age_ms: 0.0,
            rotate: 0.05,
            radius: 0.6,
            swell: 0.4,
            breathe: 0.5,
            flash: 0.8,
            size: 1.0,
        }
    }

    /// Refresh the tuning scalars from `params`, and only those — the rotation
    /// phase and the envelopes are left untouched so a live re-apply does not
    /// reset the animation. Shared by [`Scene::init`] and [`Scene::apply_params`].
    /// Allocation-free.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.rotate, params, "rotate");
        read_param(&mut self.radius, params, "radius");
        read_param(&mut self.swell, params, "swell");
        read_param(&mut self.breathe, params, "breathe");
        read_param(&mut self.flash, params, "flash");
        read_param(&mut self.size, params, "size");
    }

    /// Draw one arm: a curved run of points from the centre outward at base angle
    /// `ang`, its outer length and bend taken from the per-arm constants and its
    /// radius/thickness breathing with the mid envelope.
    #[allow(clippy::too_many_arguments)]
    fn draw_arm(
        &self,
        canvas: &mut Canvas,
        ang: f32,
        arm: usize,
        overall_r: f32,
        petal_bright: f32,
    ) {
        let len = ARM_LEN[arm];
        let bend = ARM_BEND[arm];
        for s in 0..SEGMENTS {
            let tf = (s as f32 + 1.0) / SEGMENTS as f32;
            // Radius grows outward; the mid breath bulges the middle of the arm.
            let bulge =
                1.0 + BREATHE_R * self.breathe * self.mid_env * (tf * std::f32::consts::PI).sin();
            let r = overall_r * len * tf * bulge;
            let a = ang + bend * (tf * std::f32::consts::PI).sin();
            let (x, y) = place(r, a, self.aspect);
            let size = POINT_SIZE
                * self.size
                * (0.5 + 0.5 * tf)
                * (0.7 + 0.6 * self.breathe * self.mid_env);
            let intensity = (petal_bright * (0.45 + 0.55 * tf)).clamp(0.0, 1.0);
            canvas.point(x, y, size, Style::new(arm_slot(tf), intensity));
        }
    }
}

impl Default for Bloom {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Bloom {
    fn id(&self) -> &'static str {
        "bloom"
    }

    fn mood(&self) -> &'static str {
        "maximal"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.aspect = if ctx.aspect.is_finite() && ctx.aspect > 0.0 {
            ctx.aspect
        } else {
            1.0
        };
        self.rot = 0.0;
        self.loud_env = 0.0;
        self.mid_env = 0.0;
        self.onset_env = 0.0;
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the rotation phase and every envelope carry across,
        // so a live mapping never resets the mandala.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Loudness follower eases the overall radius; the mid follower drives the
        // petal breath. Reads the engine-normalized loudness (0..1), not the raw
        // rms. Bands are normalized to 1.0 = recent average, so halve to land the
        // average near the middle of the 0..1 breath range.
        let loud = f.loudness.clamp(0.0, 1.0);
        self.loud_env += (loud - self.loud_env) * (1.0 - decay(dt, LOUD_TAU));
        let mid = (f.bands[1] * 0.5).clamp(0.0, 1.0);
        self.mid_env += (mid - self.mid_env) * (1.0 - decay(dt, MID_TAU));

        // Onset (core-flash) envelope: snap to full on a fresh onset, otherwise
        // decay. Fire on a rising edge, or when a fresh onset resets
        // `onset_age_ms` below the previous frame's value, so a held onset never
        // re-fires.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            self.onset_env = 1.0;
        } else {
            self.onset_env *= decay(dt, ONSET_TAU);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;

        // Global rotation, slowed as the signal quiets so an idle mandala barely
        // turns instead of spinning at full rate.
        let spin = match f.activity {
            Activity::Active => 1.0,
            Activity::Quiet => 0.4,
            Activity::Idle => 0.1,
        };
        self.rot = (self.rot + dt * TWO_PI * self.rotate * spin).rem_euclid(TWO_PI);
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // Overall radius eases with loudness; a quiet mandala contracts to a small
        // figure but never vanishes (the floor keeps it present).
        let overall_r =
            self.radius * (RADIUS_FLOOR + (1.0 - RADIUS_FLOOR) * self.swell * self.loud_env);
        // Petals dim as the signal quiets, so the resting mandala is dim.
        let petal_bright = (0.3 + 0.5 * self.loud_env + 0.2 * self.mid_env).clamp(0.0, 1.0);

        // The six-fold rotational copies: emit every arm at `rot + k·60°`, so the
        // rendered figure is invariant under a 60° rotation.
        for k in 0..SYMMETRY {
            let base = self.rot + k as f32 * WEDGE;
            for arm in 0..ARM_LEN.len() {
                // Space the arms across the wedge so the six copies tile the turn.
                let wedge_frac = (arm as f32 + 0.5) / ARM_LEN.len() as f32;
                let ang = base + wedge_frac * WEDGE;
                self.draw_arm(canvas, ang, arm, overall_r, petal_bright);
            }
        }

        // The bright central core: concentric points at the exact centre (so they
        // stay rotation-invariant), flashing on the onset envelope over a steady
        // glow. Fast attack, slower decay.
        let core = (CORE_GLOW + self.flash * self.onset_env).clamp(0.0, 1.0);
        for i in 0..CORE_POINTS {
            let shrink = 1.0 - i as f32 / CORE_POINTS as f32;
            let size = CORE_SIZE * self.size * shrink;
            let intensity = (core * (0.5 + 0.5 * shrink)).clamp(0.0, 1.0);
            canvas.point(0.5, 0.5, size, Style::new(CORE_SLOT, intensity));
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("rot", self.rot);
        s.set("loud_env", self.loud_env);
        s.set("mid_env", self.mid_env);
        s.set("onset_env", self.onset_env);
        s.set("prev_onset", if self.prev_onset { 1.0 } else { 0.0 });
        s.set("prev_onset_age_ms", self.prev_onset_age_ms);
        s
    }

    fn restore(&mut self, s: SceneState) {
        if let Some(v) = s.get("rot") {
            self.rot = v;
        }
        if let Some(v) = s.get("loud_env") {
            self.loud_env = v;
        }
        if let Some(v) = s.get("mid_env") {
            self.mid_env = v;
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

/// Place a point at aspect-corrected radius `r` and angle `a` around the canvas
/// centre, returning normalized coordinates. Matches `sonar`'s handling so the
/// mandala reads as a physical circle on any surface.
#[inline]
fn place(r: f32, a: f32, aspect: f32) -> (f32, f32) {
    let x = 0.5 + r * a.cos() / aspect;
    let y = 0.5 + r * a.sin();
    (x, y)
}

/// Pick the arm palette slot for a point at outward fraction `tf`: teal inner,
/// cyan through the middle, amber at the rim.
#[inline]
fn arm_slot(tf: f32) -> crate::Slot {
    if tf < 0.4 {
        INNER_SLOT
    } else if tf < 0.75 {
        MID_SLOT
    } else {
        OUTER_SLOT
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
            .expect("key is a bloom parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::{Activity, FeatureSnapshot};

    fn inited(aspect: f32) -> Bloom {
        let mut s = Bloom::new();
        let ctx = SceneCtx {
            aspect,
            ..SceneCtx::default()
        };
        s.init(&ctx);
        s
    }

    /// An active snapshot with loudness, a mid level and an optional onset. The
    /// first argument is the engine-normalized `loudness` the scene drives from.
    fn snap(loudness: f32, mid: f32, onset: bool) -> FeatureSnapshot {
        let mut f = FeatureSnapshot {
            rms: loudness,
            loudness,
            onset,
            onset_age_ms: if onset { 0.0 } else { 60_000.0 },
            activity: Activity::Active,
            ..FeatureSnapshot::default()
        };
        f.bands = [mid, mid, mid];
        f
    }

    /// A deep-silence snapshot.
    fn idle() -> FeatureSnapshot {
        FeatureSnapshot {
            rms: 0.0,
            onset: false,
            onset_age_ms: 60_000.0,
            activity: Activity::Idle,
            quiet_ms: 60_000.0,
            ..FeatureSnapshot::default()
        }
    }

    /// Every non-core point (the arm points), as `(x, y)` pairs, from a rendered
    /// frame at aspect 1.0. Core points sit at the exact centre and are excluded.
    fn arm_points(scene: &mut Bloom) -> Vec<(f32, f32)> {
        let mut c = Canvas::new(1.0);
        scene.render(&mut c);
        c.primitives()
            .iter()
            .filter_map(|p| match p {
                Primitive::Point { x, y, .. } => Some((*x, *y)),
                _ => None,
            })
            .filter(|(x, y)| (x - 0.5).hypot(y - 0.5) > 1e-3)
            .collect()
    }

    /// The maximum intensity among the centred core points of a rendered frame.
    fn core_intensity(scene: &mut Bloom) -> f32 {
        let mut c = Canvas::new(1.0);
        scene.render(&mut c);
        c.primitives()
            .iter()
            .filter_map(|p| match p {
                Primitive::Point { x, y, style, .. }
                    if (x - 0.5).abs() < 1e-6 && (y - 0.5).abs() < 1e-6 =>
                {
                    Some(style.intensity)
                }
                _ => None,
            })
            .fold(0.0, f32::max)
    }

    #[test]
    fn six_fold_symmetry_holds() {
        // Warm the envelopes with a moderate active signal so the arms have real
        // spread (radius stays well within bounds, so nothing is clamped).
        let mut s = inited(1.0);
        for _ in 0..40 {
            s.update(&snap(0.5, 1.0, false), 0.05);
        }
        let pts = arm_points(&mut s);
        assert!(
            pts.len() >= SEGMENTS * SYMMETRY,
            "the mandala drew its arms"
        );

        // Rotating the whole arm-point set by 60° about the centre must map it
        // onto itself: every rotated point has a matching original point.
        let (c60, s60) = (WEDGE.cos(), WEDGE.sin());
        for &(x, y) in &pts {
            let (dx, dy) = (x - 0.5, y - 0.5);
            let rx = 0.5 + dx * c60 - dy * s60;
            let ry = 0.5 + dx * s60 + dy * c60;
            let matched = pts.iter().any(|&(px, py)| (px - rx).hypot(py - ry) < 3e-3);
            assert!(
                matched,
                "a 60° rotation of ({x},{y}) -> ({rx},{ry}) has no matching point"
            );
        }
    }

    #[test]
    fn onset_flashes_the_core() {
        // A steady active passage settles the core to its dim glow.
        let mut s = inited(1.0);
        for _ in 0..20 {
            s.update(&snap(0.4, 1.0, false), 0.05);
        }
        let calm = core_intensity(&mut s);

        // A fresh onset snaps the core envelope up and the core flashes brighter.
        s.update(&snap(0.4, 1.0, true), 0.05);
        let flashed = core_intensity(&mut s);
        assert!(
            s.onset_env > 0.9,
            "a fresh onset snaps the core envelope up: {}",
            s.onset_env
        );
        assert!(
            flashed > calm + 0.2,
            "the onset flashes the core brighter: {flashed} vs calm {calm}"
        );
    }

    #[test]
    fn quiet_contracts_the_mandala() {
        // An active loud passage opens the mandala wide.
        let mut active = inited(1.0);
        for _ in 0..60 {
            active.update(&snap(0.9, 1.2, false), 0.05);
        }
        let active_reach = arm_points(&mut active)
            .iter()
            .map(|(x, y)| (x - 0.5).hypot(y - 0.5))
            .fold(0.0, f32::max);

        // A long silence contracts it to a small figure.
        let mut quiet = inited(1.0);
        for _ in 0..200 {
            quiet.update(&idle(), 0.05);
        }
        let quiet_reach = arm_points(&mut quiet)
            .iter()
            .map(|(x, y)| (x - 0.5).hypot(y - 0.5))
            .fold(0.0, f32::max);

        assert!(
            quiet_reach < active_reach * 0.6,
            "quiet contracts the mandala: quiet reach {quiet_reach} vs active {active_reach}"
        );
        assert!(quiet_reach > 0.0, "the resting mandala is still present");
    }

    #[test]
    fn render_primitives_stay_in_bounds() {
        let mut s = inited(16.0 / 9.0);
        for i in 0..30 {
            s.update(&snap(0.8, 1.5, i % 4 == 0), 0.05);
        }
        let mut c = Canvas::new(16.0 / 9.0);
        s.render(&mut c);
        for p in c.primitives() {
            match p {
                Primitive::Point { x, y, size, .. } => {
                    assert!((0.0..=1.0).contains(x) && (0.0..=1.0).contains(y));
                    assert!((0.0..=1.0).contains(size));
                }
                other => panic!("bloom draws only points, got {other:?}"),
            }
        }
    }

    #[test]
    fn state_restore_carries_rotation_and_envelopes() {
        let warm = snap(0.7, 1.2, false);
        let next = snap(0.3, 0.8, false);

        // Reference: advance several frames, snapshot, advance one more.
        let mut a = inited(1.0);
        for _ in 0..30 {
            a.update(&warm, 0.05);
        }
        let state = a.state();
        a.update(&next, 0.05);

        // Restored: fresh scene, restore, advance the same frame.
        let mut b = inited(1.0);
        b.restore(state);
        b.update(&next, 0.05);

        assert!((a.rot - b.rot).abs() < 1e-6, "rotation phase carried");
        assert!((a.loud_env - b.loud_env).abs() < 1e-6, "loudness carried");
        assert!((a.mid_env - b.mid_env).abs() < 1e-6, "mid breath carried");

        // Control: without the restore the rotation and envelopes are cold.
        let mut c = inited(1.0);
        c.update(&next, 0.05);
        assert!(
            (a.rot - c.rot).abs() > 1e-4,
            "a scene that skipped restore should not match the rotation"
        );
    }
}
