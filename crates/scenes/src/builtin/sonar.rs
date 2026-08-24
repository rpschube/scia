//! `sonar` — a sweep arm circling at the track tempo, contacts flaring where
//! onsets land.
//!
//! A single arm sweeps around the centre like a radar display. When the beat
//! tracker is confident (`beat_confidence >= 0.5` — the same consumer gate the
//! chrome uses) the arm locks to the beat: it rotates so one full revolution
//! spans a fixed number of beats (a parameter), its angular speed derived from
//! `tempo_bpm` and its phase nudged toward `beat_phase`, forward only so it
//! never snaps backward. Below the gate it falls back to a free rotation rate,
//! and the scene eases between the two regimes rather than jumping.
//!
//! Every detected onset spawns a *contact* from a fixed pool at the arm's
//! current angle. The contact's radius is derived deterministically from the
//! signal — a bass/treble mix places it inner or outer, and a hash of the hop
//! `generation` scatters it — so contacts land believably without any RNG. A
//! contact flares bright, then fades with a phosphor-style decay (a parameter).
//!
//! The arm is drawn as a [`crate::canvas::Primitive::Line`] from the centre with
//! a short fading trail behind it; contacts are [`crate::canvas::Primitive::Point`]s;
//! faint range rings are drawn as sparse points so the display stays legible at
//! the coarse tier.
//!
//! # Quiet / Idle
//!
//! The sweep slows as the signal falls quiet (and nearly stops when the DSP
//! thread reports [`scia_core::Activity::Idle`]); contacts stop spawning and
//! fade out, so an idle display winds down to a dim, slowly turning arm.
//!
//! # Geometry
//!
//! Positions are aspect-corrected exactly as `starfall` does: a contact at
//! aspect-corrected radius `r` and angle `θ` lands at `0.5 + r·cos θ / aspect`
//! horizontally and `0.5 + r·sin θ` vertically, so the sweep reads as a physical
//! circle on any surface. Radii are fractions of the canvas half-height.
//!
//! # Parameters
//!
//! | key            | default | range         | meaning                                                          |
//! |----------------|---------|---------------|------------------------------------------------------------------|
//! | `beats_per_rev`| `4.0`   | `1.0..=16.0`  | beats spanned by one full sweep revolution when locked to the beat |
//! | `rate`         | `0.15`  | `0.02..=1.0`  | free rotation rate (revolutions/second) when the beat is unlocked  |
//! | `decay`        | `1.2`   | `0.1..=5.0`   | contact persistence time constant (seconds)                       |
//! | `trail`        | `0.5`   | `0.0..=1.0`   | sweep trail length (phosphor persistence of the arm)             |
//! | `rings`        | `2.0`   | `0.0..=4.0`   | number of faint range rings (rounded)                            |
//!
//! All parameters are live tuning scalars, re-applied every frame through
//! [`Scene::apply_params`] and clamped to their manifest range on read.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the sweep angle, the regime lock envelope, the
//! beat-phase bookkeeping, the onset-edge bookkeeping and the live contacts
//! (each angle, radius and intensity), so a hot reload resumes the sweep where
//! it was and keeps the contacts on screen.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};
use scia_core::Activity;

/// `2π`, one full turn.
const TWO_PI: f32 = std::f32::consts::TAU;
/// The beat-confidence gate: at or above this the arm locks to the beat. The
/// same value the chrome uses to gate the beat fields — a decided convention.
const BEAT_GATE: f32 = 0.5;
/// Regime-ease time constant (seconds): the lock envelope moves between the free
/// and beat-locked regimes over roughly this long, so the sweep never jumps.
const LOCK_TAU: f32 = 0.4;
/// Forward-only phase-lock gain: the fraction of the phase error toward the
/// beat-implied angle the arm closes per frame while locked.
const PHASE_LOCK_GAIN: f32 = 0.15;
/// Maximum number of contacts alive at once; the oldest is recycled on overflow.
const CONTACT_CAP: usize = 24;
/// A contact below this intensity has faded out and its slot is freed.
const CONTACT_EPS: f32 = 0.02;
/// Rim radius (fraction of half-height) the sweep arm reaches.
const ARM_RADIUS: f32 = 0.95;
/// Number of trailing segments drawn behind the arm at full `trail`.
const TRAIL_MAX: usize = 8;
/// Angular spacing between trail segments (radians).
const TRAIL_STEP: f32 = 0.06;
/// Sparse points drawn around each range ring.
const RING_SEGMENTS: usize = 24;
/// Base contact diameter (fraction of canvas height) at full intensity.
const CONTACT_SIZE: f32 = 0.03;
/// Arm stroke width (fraction of canvas height).
const ARM_WIDTH: f32 = 0.01;
/// Fraction of the free rotation that survives at silence, so an idle display
/// keeps turning slowly instead of freezing.
const IDLE_SPIN: f32 = 0.1;

/// Palette slot for the bright sweep arm (cyan).
const ARM_SLOT: crate::Slot = 2;
/// Palette slot for a flaring contact (coral).
const CONTACT_SLOT: crate::Slot = 4;
/// Palette slot for the faint range rings (mid neutral).
const RING_SLOT: crate::Slot = 6;

/// `sonar`'s parameter manifest: the keys a preset may set, with the defaults,
/// ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "beats_per_rev",
        default: 4.0,
        min: 1.0,
        max: 16.0,
        doc: "beats spanned by one full sweep revolution when locked to the beat",
    },
    ParamSpec {
        key: "rate",
        default: 0.15,
        min: 0.02,
        max: 1.0,
        doc: "free rotation rate (revolutions/second) when the beat is unlocked",
    },
    ParamSpec {
        key: "decay",
        default: 1.2,
        min: 0.1,
        max: 5.0,
        doc: "contact persistence time constant (seconds)",
    },
    ParamSpec {
        key: "trail",
        default: 0.5,
        min: 0.0,
        max: 1.0,
        doc: "sweep trail length (phosphor persistence of the arm)",
    },
    ParamSpec {
        key: "rings",
        default: 2.0,
        min: 0.0,
        max: 4.0,
        doc: "number of faint range rings (rounded)",
    },
];

/// One contact: a point on the display that flares on an onset and fades.
#[derive(Clone, Copy, Debug)]
struct Contact {
    /// Angle from centre (radians).
    angle: f32,
    /// Aspect-corrected radius (fraction of half-height).
    radius: f32,
    /// Current intensity in `0.0..=1.0`; fades toward zero.
    intensity: f32,
}

impl Contact {
    const DEAD: Self = Self {
        angle: 0.0,
        radius: 0.0,
        intensity: 0.0,
    };
}

/// The radar-sweep scene.
#[derive(Clone, Debug)]
pub struct Sonar {
    // --- geometry, captured at init ------------------------------------
    /// Aspect ratio captured at init, used to place the sweep on the canvas.
    aspect: f32,

    // --- live state ----------------------------------------------------
    /// Sweep arm angle in radians, wrapped to `0..2π`. Only ever advances.
    angle: f32,
    /// Regime lock envelope in `0.0..=1.0`: 1 when locked to the beat, 0 free.
    lock_env: f32,
    /// Arm angle captured at the last beat boundary, the base for phase-locking.
    beat_base_angle: f32,
    /// Previous frame's `beat_phase`, to detect the beat wrap (a new beat).
    prev_beat_phase: f32,
    /// Fixed-capacity contact pool; oldest recycled on overflow.
    contacts: [Contact; CONTACT_CAP],
    /// Previous frame's onset flag, for rising-edge detection.
    prev_onset: bool,
    /// Previous frame's `onset_age_ms`, to catch a fresh onset that resets the age.
    prev_onset_age_ms: f32,

    // --- parameters ----------------------------------------------------
    beats_per_rev: f32,
    rate: f32,
    decay: f32,
    trail: f32,
    rings: f32,
}

impl Sonar {
    /// A `sonar` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            aspect: 1.0,
            angle: 0.0,
            lock_env: 0.0,
            beat_base_angle: 0.0,
            prev_beat_phase: 0.0,
            contacts: [Contact::DEAD; CONTACT_CAP],
            prev_onset: false,
            prev_onset_age_ms: 0.0,
            beats_per_rev: 4.0,
            rate: 0.15,
            decay: 1.2,
            trail: 0.5,
            rings: 2.0,
        }
    }

    /// Refresh the tuning scalars from `params`, and only those — the sweep
    /// angle, the lock state and the contacts are left untouched so a live
    /// re-apply does not reset the animation. Shared by [`Scene::init`] and
    /// [`Scene::apply_params`]. Allocation-free.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.beats_per_rev, params, "beats_per_rev");
        read_param(&mut self.rate, params, "rate");
        read_param(&mut self.decay, params, "decay");
        read_param(&mut self.trail, params, "trail");
        read_param(&mut self.rings, params, "rings");
    }

    /// Spawn a contact, reusing an inactive slot or recycling the faintest one.
    fn spawn_contact(&mut self, angle: f32, radius: f32) {
        let mut slot = 0usize;
        let mut weakest = f32::INFINITY;
        for (i, c) in self.contacts.iter().enumerate() {
            if c.intensity < CONTACT_EPS {
                slot = i;
                weakest = -1.0;
                break;
            }
            if c.intensity < weakest {
                weakest = c.intensity;
                slot = i;
            }
        }
        let _ = weakest;
        self.contacts[slot] = Contact {
            angle,
            radius,
            intensity: 1.0,
        };
    }
}

impl Default for Sonar {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Sonar {
    fn id(&self) -> &'static str {
        "sonar"
    }

    fn mood(&self) -> &'static str {
        "vigilant"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.aspect = if ctx.aspect.is_finite() && ctx.aspect > 0.0 {
            ctx.aspect
        } else {
            1.0
        };
        self.angle = 0.0;
        self.lock_env = 0.0;
        self.beat_base_angle = 0.0;
        self.prev_beat_phase = 0.0;
        self.contacts = [Contact::DEAD; CONTACT_CAP];
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the sweep angle, lock envelope and contacts carry
        // across, so a live mapping never resets the sweep.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Ease the regime lock toward the beat gate: locked when the tracker is
        // confident, free otherwise, blended smoothly so the sweep never jumps.
        let lock_target = if f.beat_confidence >= BEAT_GATE {
            1.0
        } else {
            0.0
        };
        self.lock_env += (lock_target - self.lock_env) * (1.0 - decay(dt, LOCK_TAU));

        // Angular velocity: blend the free rate with the beat-derived rate by the
        // lock envelope. The free rate is revolutions/second; the beat rate makes
        // one revolution span `beats_per_rev` beats at the tracked tempo.
        let omega_free = TWO_PI * self.rate;
        let omega_beat = if f.tempo_bpm > 0.0 {
            TWO_PI * (f.tempo_bpm / 60.0) / self.beats_per_rev.max(1.0)
        } else {
            omega_free
        };
        let mut omega = omega_free * (1.0 - self.lock_env) + omega_beat * self.lock_env;

        // The sweep slows as the signal falls quiet and nearly stops when idle,
        // so an idle display winds down instead of spinning at full rate.
        let spin = match f.activity {
            Activity::Active => 1.0,
            Activity::Quiet => 0.5,
            Activity::Idle => IDLE_SPIN,
        };
        omega *= spin;
        omega = omega.max(0.0);

        self.angle = (self.angle + omega * dt).rem_euclid(TWO_PI);

        // Forward-only phase lock: at each beat boundary (a phase wrap) capture
        // the arm angle as the base, then nudge the arm toward the angle the beat
        // phase implies within the revolution — but only forward, never back, so
        // the sweep can speed up to catch the beat but never snaps backward.
        if f.tempo_bpm > 0.0 && self.lock_env > 0.01 {
            if f.beat_phase < self.prev_beat_phase {
                self.beat_base_angle = self.angle;
            }
            let per_beat = TWO_PI / self.beats_per_rev.max(1.0);
            let desired = self.beat_base_angle + f.beat_phase * per_beat;
            // Forward-only correction, scaled by how locked we are.
            let err = desired - self.angle;
            if err > 0.0 {
                self.angle =
                    (self.angle + err * PHASE_LOCK_GAIN * self.lock_env).rem_euclid(TWO_PI);
            }
        }
        self.prev_beat_phase = f.beat_phase;

        // Fade every live contact by the phosphor decay.
        let cd = decay(dt, self.decay);
        for c in &mut self.contacts {
            if c.intensity >= CONTACT_EPS {
                c.intensity *= cd;
            }
        }

        // One onset spawns exactly one contact at the current sweep angle. Fire
        // on a rising edge, or when a fresh onset resets `onset_age_ms` below the
        // previous frame's value, so a held onset never double-spawns.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            let radius = contact_radius(f);
            self.spawn_contact(self.angle, radius);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;
    }

    fn render(&mut self, canvas: &mut Canvas) {
        let aspect = self.aspect;

        // Faint range rings, drawn as sparse points so they stay legible coarse.
        let ring_count = (self.rings.round() as i32).clamp(0, 4) as usize;
        for k in 1..=ring_count {
            let r = ARM_RADIUS * (k as f32 / (ring_count as f32 + 1.0));
            for s in 0..RING_SEGMENTS {
                let a = (s as f32 / RING_SEGMENTS as f32) * TWO_PI;
                let (x, y) = place(0.5, 0.5, r, a, aspect);
                canvas.point(x, y, CONTACT_SIZE * 0.3, Style::new(RING_SLOT, 0.25));
            }
        }

        // Contacts: flaring points fading with the phosphor decay.
        for c in &self.contacts {
            if c.intensity < CONTACT_EPS {
                continue;
            }
            let (x, y) = place(0.5, 0.5, c.radius, c.angle, aspect);
            let size = CONTACT_SIZE * (0.4 + 0.6 * c.intensity);
            canvas.point(x, y, size, Style::new(CONTACT_SLOT, c.intensity));
        }

        // The sweep arm, with a short fading trail behind it for the phosphor
        // feel. The number of lit trail segments follows the `trail` parameter.
        let trail_segments = (self.trail * TRAIL_MAX as f32).round() as usize;
        for i in (1..=trail_segments).rev() {
            let a = self.angle - i as f32 * TRAIL_STEP;
            let (tx, ty) = place(0.5, 0.5, ARM_RADIUS, a, aspect);
            let bright = 0.5 * (1.0 - i as f32 / (TRAIL_MAX as f32 + 1.0));
            canvas.line(0.5, 0.5, tx, ty, ARM_WIDTH, Style::new(ARM_SLOT, bright));
        }
        let (hx, hy) = place(0.5, 0.5, ARM_RADIUS, self.angle, aspect);
        canvas.line(0.5, 0.5, hx, hy, ARM_WIDTH, Style::new(ARM_SLOT, 1.0));
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("angle", self.angle);
        s.set("lock_env", self.lock_env);
        s.set("beat_base_angle", self.beat_base_angle);
        s.set("prev_beat_phase", self.prev_beat_phase);
        s.set("prev_onset", if self.prev_onset { 1.0 } else { 0.0 });
        s.set("prev_onset_age_ms", self.prev_onset_age_ms);
        for (i, c) in self.contacts.iter().enumerate() {
            s.set(&format!("c{i}_a"), c.angle);
            s.set(&format!("c{i}_r"), c.radius);
            s.set(&format!("c{i}_i"), c.intensity);
        }
        s
    }

    fn restore(&mut self, s: SceneState) {
        if let Some(v) = s.get("angle") {
            self.angle = v;
        }
        if let Some(v) = s.get("lock_env") {
            self.lock_env = v;
        }
        if let Some(v) = s.get("beat_base_angle") {
            self.beat_base_angle = v;
        }
        if let Some(v) = s.get("prev_beat_phase") {
            self.prev_beat_phase = v;
        }
        if let Some(v) = s.get("prev_onset") {
            self.prev_onset = v >= 0.5;
        }
        if let Some(v) = s.get("prev_onset_age_ms") {
            self.prev_onset_age_ms = v;
        }
        for (i, c) in self.contacts.iter_mut().enumerate() {
            let a = s.get(&format!("c{i}_a"));
            let r = s.get(&format!("c{i}_r"));
            let intensity = s.get(&format!("c{i}_i"));
            match (a, r, intensity) {
                (Some(a), Some(r), Some(intensity)) => {
                    *c = Contact {
                        angle: a,
                        radius: r,
                        intensity,
                    };
                }
                _ => *c = Contact::DEAD,
            }
        }
    }
}

/// Place a point at aspect-corrected radius `r` and angle `a` around `(cx, cy)`,
/// returning normalized canvas coordinates. Matches `starfall`'s handling so the
/// sweep reads as a physical circle on any surface.
#[inline]
fn place(cx: f32, cy: f32, r: f32, a: f32, aspect: f32) -> (f32, f32) {
    let x = cx + r * a.cos() / aspect;
    let y = cy + r * a.sin();
    (x, y)
}

/// Derive a contact's aspect-corrected radius deterministically from the signal:
/// a bass/treble mix places it inner (bass) or outer (treble), and a hash of the
/// hop `generation` scatters it. No RNG crate, no wall clock.
#[inline]
fn contact_radius(f: &scia_core::FeatureSnapshot) -> f32 {
    let bass = f.bands[0].max(0.0);
    let mid = f.bands[1].max(0.0);
    let treb = f.bands[2].max(0.0);
    let total = bass + mid + treb + 1e-6;
    // 0 (all bass) → inner, 1 (all treble) → outer.
    let pos = (mid * 0.5 + treb) / total;
    let base = 0.2 + 0.6 * pos.clamp(0.0, 1.0);
    // Deterministic ±0.08 jitter from the hop generation.
    let h = hash_u32(f.generation as u32 ^ (f.generation >> 32) as u32);
    let jitter = ((h >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.16;
    (base + jitter).clamp(0.1, 0.9)
}

/// A one-word integer hash (an xorshift-multiply finalizer). Deterministic; no
/// wall clock, no RNG crate.
#[inline]
fn hash_u32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
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
            .expect("key is a sonar parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::FeatureSnapshot;

    fn inited(aspect: f32) -> Sonar {
        let mut s = Sonar::new();
        let ctx = SceneCtx {
            aspect,
            ..SceneCtx::default()
        };
        s.init(&ctx);
        s
    }

    /// A snapshot with an onset flag, an `onset_age_ms` and band levels.
    fn onset_snap(onset: bool, onset_age_ms: f32) -> FeatureSnapshot {
        let mut f = FeatureSnapshot {
            onset,
            onset_age_ms,
            ..FeatureSnapshot::default()
        };
        f.bands = [1.0, 1.0, 1.0];
        f
    }

    /// A snapshot carrying a beat hypothesis at a given confidence and tempo.
    fn beat_snap(confidence: f32, tempo_bpm: f32, beat_phase: f32) -> FeatureSnapshot {
        FeatureSnapshot {
            beat_confidence: confidence,
            tempo_bpm,
            beat_phase,
            ..FeatureSnapshot::default()
        }
    }

    fn active_contacts(scene: &Sonar) -> usize {
        scene
            .contacts
            .iter()
            .filter(|c| c.intensity >= CONTACT_EPS)
            .count()
    }

    #[test]
    fn onset_spawns_a_contact() {
        let mut s = inited(16.0 / 9.0);
        assert_eq!(active_contacts(&s), 0, "no contacts before any onset");

        // A quiet frame leaves the pool empty.
        s.update(&onset_snap(false, 60_000.0), 0.05);
        assert_eq!(active_contacts(&s), 0);

        // The rising edge of an onset spawns one contact.
        s.update(&onset_snap(true, 0.0), 0.05);
        assert_eq!(active_contacts(&s), 1, "one onset edge → one contact");

        // A repeated identical onset snapshot must not spawn another.
        s.update(&onset_snap(true, 0.0), 0.05);
        assert_eq!(active_contacts(&s), 1, "a held onset does not re-fire");

        // A fresh onset (age resets) spawns a second.
        s.update(&onset_snap(false, 40.0), 0.05);
        s.update(&onset_snap(true, 0.0), 0.05);
        assert_eq!(active_contacts(&s), 2, "a new onset fires again");
    }

    #[test]
    fn contacts_decay_out() {
        let mut s = inited(1.0);
        s.update(&onset_snap(true, 0.0), 0.05);
        assert_eq!(active_contacts(&s), 1, "a contact is live after the onset");

        // A long silence fades the contact below the visibility threshold.
        for _ in 0..400 {
            s.update(&onset_snap(false, 60_000.0), 0.05);
        }
        assert_eq!(
            active_contacts(&s),
            0,
            "contacts fade out over a long silence"
        );
    }

    #[test]
    fn beat_gate_switches_rotation_regime() {
        // A slow free rate but a fast beat tempo: below the 0.5 gate the sweep
        // uses the slow free rate; at/above the gate it locks to the fast tempo
        // and advances markedly further over the same frames.
        let dt = 0.05;
        let frames = 200;

        // Free regime: confidence below the gate.
        let mut free = inited(1.0);
        free.rate = 0.05; // very slow free rotation
        let mut free_total = 0.0f32;
        let mut prev = free.angle;
        for i in 0..frames {
            let phase = (i as f32 * 0.1).rem_euclid(1.0);
            free.update(&beat_snap(0.49, 180.0, phase), dt);
            free_total += forward_delta(prev, free.angle);
            prev = free.angle;
        }

        // Locked regime: confidence at the gate, same slow free rate, fast tempo.
        let mut locked = inited(1.0);
        locked.rate = 0.05;
        let mut locked_total = 0.0f32;
        let mut prev = locked.angle;
        for i in 0..frames {
            let phase = (i as f32 * 0.1).rem_euclid(1.0);
            locked.update(&beat_snap(0.5, 180.0, phase), dt);
            locked_total += forward_delta(prev, locked.angle);
            prev = locked.angle;
        }

        assert!(
            locked_total > free_total * 1.5,
            "the 0.5 gate locks to the faster beat tempo: locked {locked_total} \
             should clearly exceed free {free_total}"
        );
    }

    #[test]
    fn sweep_never_snaps_backward() {
        // Under a locked beat with a wrapping phase the arm must only ever
        // advance; the forward-only phase lock must never reverse it.
        let mut s = inited(1.0);
        let dt = 0.03;
        let mut prev = s.angle;
        for i in 0..400 {
            let phase = (i as f32 * 0.13).rem_euclid(1.0);
            s.update(&beat_snap(0.9, 128.0, phase), dt);
            let d = forward_delta(prev, s.angle);
            assert!(
                d >= -1e-4,
                "the sweep must not snap backward: step {d} at frame {i}"
            );
            prev = s.angle;
        }
    }

    #[test]
    fn render_primitives_stay_in_bounds() {
        let mut s = inited(16.0 / 9.0);
        for _ in 0..10 {
            s.update(&onset_snap(true, 0.0), 0.05);
        }
        let mut c = Canvas::new(16.0 / 9.0);
        s.render(&mut c);
        for p in c.primitives() {
            match p {
                Primitive::Point { x, y, size, .. } => {
                    assert!((0.0..=1.0).contains(x) && (0.0..=1.0).contains(y));
                    assert!((0.0..=1.0).contains(size));
                }
                Primitive::Line {
                    x0,
                    y0,
                    x1,
                    y1,
                    width,
                    ..
                } => {
                    for v in [x0, y0, x1, y1, width] {
                        assert!((0.0..=1.0).contains(v), "line coord in bounds: {v}");
                    }
                }
                other => panic!("expected Point or Line, got {other:?}"),
            }
        }
    }

    #[test]
    fn state_restore_carries_continuity() {
        let s1 = beat_snap(0.9, 120.0, 0.3);
        let mut s1 = s1;
        s1.onset = true;
        let s2 = beat_snap(0.9, 120.0, 0.5);

        // Reference: drive a frame (spawns a contact, advances the sweep),
        // snapshot, then advance one more.
        let mut a = inited(1.0);
        a.update(&s1, 0.05);
        let state = a.state();
        a.update(&s2, 0.05);

        // Restored: fresh scene, restore, advance the same frame.
        let mut b = inited(1.0);
        b.restore(state);
        b.update(&s2, 0.05);

        assert!((a.angle - b.angle).abs() < 1e-5, "sweep angle carried");
        assert!(
            (a.lock_env - b.lock_env).abs() < 1e-6,
            "lock envelope carried"
        );
        assert_eq!(
            active_contacts(&a),
            active_contacts(&b),
            "the live contact is carried across the restore"
        );

        // Control: without the restore the sweep and contact are lost.
        let mut c = inited(1.0);
        c.update(&s2, 0.05);
        assert!(
            active_contacts(&c) < active_contacts(&a),
            "a scene that skipped restore should not carry the contact"
        );
    }

    /// The forward angular delta from `prev` to `cur`, unwrapping one `2π` turn
    /// so a wrap counts as a small forward step rather than a huge backward one.
    fn forward_delta(prev: f32, cur: f32) -> f32 {
        let mut d = cur - prev;
        if d < -std::f32::consts::PI {
            d += TWO_PI;
        }
        d
    }
}
