//! `starfall` — a starfield streaming outward from the centre, warping into
//! streaks on the beat.
//!
//! A fixed pool of stars drifts outward from the canvas centre along fixed
//! per-star directions. Overall loudness rides the stream speed, the star size
//! and the brightness together: the louder the track, the faster the field
//! flows, the larger and brighter each star swells; in silence the speed falls to
//! a slow floor and the stars shrink and dim so the field always drifts calmly,
//! never freezes. Every star trails a short **motion-blur tail** along its
//! direction of travel (lengthening with the stream speed), so a star reads as a
//! continuous glide rather than a blinking point — this, together with the
//! loudness-driven size swell, is what keeps the streaming smooth on the coarse
//! display grid. Every detected onset briefly extends the **outer** stars' tails
//! into longer streaks (capped short) and then relaxes them. Brightness rises
//! gently toward the rim, so the field reads with depth.
//!
//! There is no beat tracker yet (`beat_phase`/`tempo_bpm` are reserved zeros),
//! so the motion is driven entirely from the onset stream and the
//! engine-normalized `loudness` (the mono rms divided by a slow auto-reference),
//! which is level-independent so real music actually drives the stream speed. A
//! secondary gate on the **raw** RMS damps the loudness-driven speed, size and
//! brightness on genuinely quiet material (where the normalized loudness would
//! otherwise read high), so a near-silent clip stays calm.
//!
//! # Geometry
//!
//! Radial motion is measured in **aspect-corrected** units (the same handling as
//! `lattice`): a star's offset from centre is `radius · (cos θ, sin θ)` in a
//! space where `x` carries the canvas aspect, then divided back by the aspect
//! when it is placed on the canvas. The stream therefore reads as a physical
//! circle on any surface rather than an ellipse. Each star knows the radius at
//! which its own direction leaves the canvas, so it respawns near the centre
//! exactly at the edge — no pile-up against a clamped border.
//!
//! # Determinism
//!
//! The star field is pseudo-random but never touches an RNG crate or the wall
//! clock: each star seeds a tiny inline LCG from its index and a respawn counter
//! and draws its direction, speed jitter and size jitter from that sequence. The
//! same index always yields the same star, so the whole field rebuilds
//! identically at [`Scene::init`]. When a star leaves the canvas it bumps its
//! counter and draws a fresh sequence, so a respawned star is deterministic yet
//! different from its previous life.
//!
//! # Parameters
//!
//! | key      | default | range       | meaning                                                           |
//! |----------|---------|-------------|-------------------------------------------------------------------|
//! | `stars`  | `192`   | `16..=512`  | number of stars in the pool, preallocated at init                 |
//! | `speed`  | `0.2`   | `0.05..=2.0`| base outward speed (canvas units / second) that loudness rides on |
//! | `streak` | `0.05`  | `0.0..=2.0` | streak-length gain on an onset (outer stars stretch into lines)   |
//! | `size`   | `1.0`   | `0.2..=3.0` | star size multiplier                                              |
//! | `spread` | `0.6`   | `0.0..=1.0` | spawn-direction spread: 1 spaces stars evenly around the circle, 0 scatters them into clusters |
//!
//! `speed`, `streak`, `size` and `spread` are live tuning scalars: the host
//! re-applies them every frame through [`Scene::apply_params`], each clamped to
//! its manifest range on read (a mapping's `offset + scale · env` can exceed it).
//! `stars` is applied at init only — a live change to it stays inert until the
//! next [`Scene::init`] rebuilds the pool, exactly as `lattice`'s `density` does.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the loudness envelope, the raw-RMS envelope, the
//! streak (onset) envelope and the onset-edge bookkeeping, so a hot reload keeps
//! the current flow speed and does not re-fire the streak. The star positions are
//! **not** carried: the
//! pool re-seeds deterministically at [`Scene::init`] and re-settles into a full
//! field within a few frames, so a frame's worth of positions is not worth
//! serializing — the same judgment `spectra` makes for its per-bar heights.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};

/// Fraction of the stream speed that remains at silence, so the field keeps a
/// slow calm drift instead of freezing.
const SPEED_FLOOR: f32 = 0.15;

/// Loudness-follower time constant (seconds).
const LOUD_TAU: f32 = 0.15;

/// Raw-RMS smoothing time constant (seconds) for the level gate below.
const RMS_TAU: f32 = 0.25;

/// Raw-RMS level at which the level gate reaches full strength. Normalized
/// loudness reads a steady quiet clip as loud, which would keep the field
/// streaming and swelling; gating the loudness-driven speed, size and brightness
/// on raw RMS keeps genuinely quiet material calm while leaving louder clips
/// untouched.
const RMS_REF: f32 = 0.06;

/// Streak-envelope decay time constant (seconds): the onset snap decays over
/// roughly this long back to points.
const ONSET_TAU: f32 = 0.25;

/// Base star diameter as a fraction of the canvas height, before the `size`
/// param and the per-star jitter scale it.
const STAR_SIZE: f32 = 0.044;

/// Floor on the loudness gate applied to star *size*: stars are small (and cheap
/// on the quiet-stillness budget) when the music is quiet and swell to their full
/// multi-cell footprint when it is loud. The big loud-passage footprint is what
/// turns the streaming into a smooth glide over the coarse grid — taming the
/// jitter exactly where the music is busiest — without a large static field
/// inflating the still passages.
const SIZE_QUIET: f32 = 0.4;

/// Base motion-blur tail length (fraction of the canvas) every star trails along
/// its direction of travel, before it is scaled by the stream speed. A short
/// tail turns each star's frame-to-frame hop into a continuous glide instead of a
/// blinking point — the tails, not larger points, are what tame the streaming
/// jitter, and because the tail rides the (loudness-driven) stream speed the
/// field's motion tracks the music without any onset spike.
const BASE_TAIL: f32 = 0.045;

/// Brightness of an inner star; the rim reaches `1.0`. The field brightens
/// gently outward so it reads with depth.
const INNER_BRIGHT: f32 = 0.35;

/// Floor on the loudness gate applied to star brightness: the field keeps this
/// fraction of its brightness at silence and brightens toward full as the music
/// swells, so overall brightness rides the music (and quiet passages dim and
/// settle) rather than shimmering at a constant level.
const STAR_QUIET: f32 = 0.25;

/// A star past this fraction of its exit radius is an "outer" star that can
/// streak; inner stars always render as points.
const STREAK_MID: f32 = 0.5;

/// Fraction of its travel over which a freshly spawned star fades up from dark,
/// so a respawn eases in near the centre instead of popping into existence.
const BIRTH_FADE: f32 = 0.08;

/// Fraction of its travel (near the exit radius) over which a star fades back to
/// dark before it recycles, so a rim star dims out instead of vanishing at full
/// brightness — the single biggest source of frame-to-frame brightness jitter.
const EXIT_FADE: f32 = 0.14;

/// The streak envelope must exceed this to draw lines; below it the field has
/// relaxed back to points.
const STREAK_EPS: f32 = 0.03;

/// Maps the dimensionless speed factor onto a normalized streak length.
const STREAK_LEN_SCALE: f32 = 0.35;

/// Upper bound on a streak's normalized length. Kept short: a streak's job here
/// is to give each outer star a brief sustained tail that smooths its motion into
/// a glide (taming flicker), not to fling a long line across the canvas — long
/// streaks make the onset response spike far past the calm band we want.
const MAX_STREAK: f32 = 0.16;

/// Radius (aspect-corrected) a respawned star starts at, scattered up to this so
/// a burst of respawns does not fire a synchronized ring out of the centre.
const SPAWN_RADIUS: f32 = 0.015;

/// The golden-ratio conjugate: an evenly spaced, low-discrepancy anchor for each
/// star's spawn direction (`spread = 1` places every star exactly on its anchor).
const PHI: f32 = 0.618_034;

/// `2π`, one full turn.
const TWO_PI: f32 = std::f32::consts::TAU;

/// Default pool size, used before [`Scene::init`] reads the `stars` param.
const DEFAULT_STARS: usize = 192;

/// `starfall`'s parameter manifest: the keys a preset may set, with the
/// defaults, ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "stars",
        default: 192.0,
        min: 16.0,
        max: 512.0,
        doc: "number of stars in the pool, preallocated at init",
    },
    ParamSpec {
        key: "speed",
        default: 0.2,
        min: 0.05,
        max: 2.0,
        doc: "base outward speed (canvas units / second) that loudness rides on",
    },
    ParamSpec {
        key: "streak",
        default: 0.05,
        min: 0.0,
        max: 2.0,
        doc: "streak-length gain on an onset (outer stars stretch into lines)",
    },
    ParamSpec {
        key: "size",
        default: 1.0,
        min: 0.2,
        max: 3.0,
        doc: "star size multiplier",
    },
    ParamSpec {
        key: "spread",
        default: 0.6,
        min: 0.0,
        max: 1.0,
        doc: "spawn-direction spread: 1 spaces stars evenly around the circle, 0 scatters them into clusters",
    },
];

/// A tiny inline linear-congruential generator, seeded per star from its index
/// and respawn counter. Deterministic and allocation-free; never an RNG crate or
/// the wall clock. The multiplier/increment are the Numerical Recipes LCG.
struct Lcg(u32);

impl Lcg {
    /// Seed from a star index and its respawn counter, mixing both so successive
    /// lives of one star and neighbouring stars all get well-separated streams.
    #[inline]
    fn seeded(index: usize, seq: u32) -> Self {
        let a = hash_u32((index as u32).wrapping_mul(0x9E37_79B1));
        let b = hash_u32(seq.wrapping_add(0x0001_2345));
        Self(hash_u32(a ^ b))
    }

    /// The next raw word.
    #[inline]
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    /// The next value in `0.0..1.0`, from the top 24 bits.
    #[inline]
    fn next01(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// One star: a fixed direction and travel bounds, its current radius, and its
/// per-star jitters. Directions and jitters come from the star's LCG sequence;
/// `seq` counts how many times it has respawned.
#[derive(Clone, Copy, Debug)]
struct Star {
    /// Direction cosine (aspect-corrected space).
    cos: f32,
    /// Direction sine (aspect-corrected space).
    sin: f32,
    /// Aspect-corrected radius at which this direction leaves the canvas.
    r_exit: f32,
    /// Current aspect-corrected radius from centre.
    radius: f32,
    /// Per-star speed multiplier.
    speed_jitter: f32,
    /// Per-star size multiplier.
    size_jitter: f32,
    /// Respawn counter; part of the LCG seed so each life differs.
    seq: u32,
}

/// The starfield scene.
#[derive(Clone, Debug)]
pub struct Starfall {
    // --- field, rebuilt at init ----------------------------------------
    /// The star pool, preallocated at init; render never resizes it.
    stars: Vec<Star>,
    /// Aspect ratio captured at init, used to place radial motion on the canvas.
    aspect: f32,

    // --- live state ----------------------------------------------------
    /// Smoothed loudness in `0.0..=1.0` driving the stream speed.
    loud_env: f32,
    /// Smoothed raw RMS, for the level gate on the loudness-driven responses.
    rms_env: f32,
    /// Streak envelope in `0.0..=1.0`: snaps to 1 on an onset, decays to 0.
    onset_env: f32,
    /// Previous frame's onset flag, for rising-edge detection.
    prev_onset: bool,
    /// Previous frame's `onset_age_ms`, to catch a fresh onset that resets the age.
    prev_onset_age_ms: f32,

    // --- parameters ----------------------------------------------------
    stars_param: f32,
    speed: f32,
    streak: f32,
    size: f32,
    spread: f32,
}

impl Starfall {
    /// A `starfall` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters and build the star pool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stars: Vec::new(),
            aspect: 1.0,
            loud_env: 0.0,
            rms_env: 0.0,
            onset_env: 0.0,
            prev_onset: false,
            prev_onset_age_ms: 0.0,
            stars_param: DEFAULT_STARS as f32,
            speed: 0.2,
            streak: 0.05,
            size: 1.0,
            spread: 0.6,
        }
    }

    /// Refresh the tuning scalars from `params`, and only those — the star pool
    /// and the envelopes are left untouched so a mid-run re-apply (feature
    /// mappings, later live tuning) does not reset the animation.
    ///
    /// Shared by [`Scene::init`] and [`Scene::apply_params`]. `stars` is read
    /// here too, but only [`Scene::init`] acts on it (by rebuilding the pool); a
    /// live change stays inert until then, exactly as `lattice`'s `density` does.
    /// Allocation-free.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.stars_param, params, "stars");
        read_param(&mut self.speed, params, "speed");
        read_param(&mut self.streak, params, "streak");
        read_param(&mut self.size, params, "size");
        read_param(&mut self.spread, params, "spread");
    }

    /// Rebuild the star pool from the current `stars` count and the drawing
    /// aspect. Every star is scattered along its own path so the field is full
    /// on the first frame rather than erupting from the centre.
    fn build_field(&mut self, aspect: f32) {
        self.aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let count = (self.stars_param.round() as i32).clamp(1, 1024) as usize;
        self.stars.clear();
        self.stars.reserve(count);
        for i in 0..count {
            self.stars
                .push(spawn_star(i, 0, self.spread, self.aspect, false));
        }
    }
}

/// Build a star deterministically from its index and respawn counter.
///
/// `near_centre` chooses where it starts: a respawned star begins near the
/// centre (scattered a touch so respawns never synchronize), while an init star
/// is scattered along the whole length of its path so the field starts full.
fn spawn_star(index: usize, seq: u32, spread: f32, aspect: f32, near_centre: bool) -> Star {
    let mut rng = Lcg::seeded(index, seq);
    let u = rng.next01();
    let speed_jitter = 0.6 + 0.8 * rng.next01();
    let size_jitter = 0.6 + 0.8 * rng.next01();
    let r_frac = rng.next01();

    // Blend an evenly spaced anchor with a random offset: `spread = 1` sits on
    // the anchor (even coverage), `spread = 0` adds a full ±0.5 random turn
    // (clustered). `fract` wraps it into one turn.
    let anchor = fract((index as f32 + 1.0) * PHI);
    let angle01 = fract(anchor + (1.0 - spread) * (u - 0.5));
    let angle = angle01 * TWO_PI;
    let cos = angle.cos();
    let sin = angle.sin();

    let r_exit = exit_radius(cos, sin, aspect);
    let radius = if near_centre {
        SPAWN_RADIUS * r_frac
    } else {
        r_frac * r_exit
    };

    Star {
        cos,
        sin,
        r_exit,
        radius,
        speed_jitter,
        size_jitter,
        seq,
    }
}

/// The aspect-corrected radius at which the ray `(cos, sin)` leaves the canvas.
///
/// On the canvas a corrected offset `r · (cos, sin)` lands at
/// `0.5 + r·cos/aspect` horizontally and `0.5 + r·sin` vertically, so the ray
/// exits at the smaller of the two axis crossings. A near-zero component makes
/// its crossing infinite, and the other axis wins.
#[inline]
fn exit_radius(cos: f32, sin: f32, aspect: f32) -> f32 {
    let rx = if cos.abs() > 1e-4 {
        0.5 * aspect / cos.abs()
    } else {
        f32::INFINITY
    };
    let ry = if sin.abs() > 1e-4 {
        0.5 / sin.abs()
    } else {
        f32::INFINITY
    };
    rx.min(ry)
}

impl Default for Starfall {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Starfall {
    fn id(&self) -> &'static str {
        "starfall"
    }

    fn mood(&self) -> &'static str {
        "cosmic"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.build_field(ctx.aspect);
        self.loud_env = 0.0;
        self.rms_env = 0.0;
        self.onset_env = 0.0;
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the star pool and both envelopes carry across, so
        // a live mapping is honored without resetting the flow. A live `stars`
        // change stays inert until the next `init` rebuilds the pool.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // Loudness follower: ride the stream speed on the engine-normalized
        // loudness (0..1), not the raw rms, smoothed toward its target.
        let loud = f.loudness.clamp(0.0, 1.0);
        let k = 1.0 - decay(dt, LOUD_TAU);
        self.loud_env += (loud - self.loud_env) * k;
        // Track raw RMS for the level gate: quiet material damps the loudness-
        // driven speed even when the normalized loudness reads high.
        self.rms_env += (f.rms.max(0.0) - self.rms_env) * (1.0 - decay(dt, RMS_TAU));
        let rms_gate = (self.rms_env / RMS_REF).clamp(0.0, 1.0);

        // Streak envelope: snap to full on a fresh onset, otherwise decay. Fire
        // on a rising edge, or when a fresh onset resets `onset_age_ms` below the
        // previous frame's value, so a held onset never re-fires.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            self.onset_env = 1.0;
        } else {
            self.onset_env *= decay(dt, ONSET_TAU);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;

        // Advance every star outward. The speed factor never falls below the
        // floor, so silence is a slow drift, not a freeze. A star that leaves the
        // canvas respawns near the centre with a fresh deterministic sequence.
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let speed_factor = self.speed * (SPEED_FLOOR + self.loud_env * rms_gate);
        for (i, star) in self.stars.iter_mut().enumerate() {
            star.radius += speed_factor * star.speed_jitter * dt;
            if star.radius > star.r_exit {
                star.seq = star.seq.wrapping_add(1);
                *star = spawn_star(i, star.seq, self.spread, self.aspect, true);
            }
        }
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // A raw-RMS level gate so a genuinely quiet clip stays calm even when its
        // normalized loudness reads high; it damps the loudness term in the speed,
        // brightness and size below.
        let rms_gate = (self.rms_env / RMS_REF).clamp(0.0, 1.0);
        let loud = self.loud_env * rms_gate;
        // The dimensionless speed factor (floor..≈1.15) drives streak length, so
        // streaks lengthen with loudness independently of the `speed` param.
        let speed_factor = SPEED_FLOOR + loud;
        let streaking_env = self.onset_env;
        // Loudness gate on brightness: the whole field dims in quiet passages and
        // swells when loud, tying overall brightness to the music.
        let loud_gate = STAR_QUIET + (1.0 - STAR_QUIET) * loud;
        // Star size also rides loudness: small and calm when quiet, swelling to a
        // smooth multi-cell footprint when loud.
        let size_gate = SIZE_QUIET + (1.0 - SIZE_QUIET) * loud;

        for star in &self.stars {
            let frac = if star.r_exit > 0.0 {
                (star.radius / star.r_exit).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Aspect-corrected offset placed back on the canvas.
            let hx = 0.5 + star.radius * star.cos / self.aspect;
            let hy = 0.5 + star.radius * star.sin;

            // Ease brightness up at birth and down toward the exit so a recycled
            // star never pops from full brightness to gone (or gone to full):
            // that discontinuity is what read as flicker rather than motion.
            let birth = (frac / BIRTH_FADE).clamp(0.0, 1.0);
            let exit = ((1.0 - frac) / EXIT_FADE).clamp(0.0, 1.0);
            let bright = ((INNER_BRIGHT + (1.0 - INNER_BRIGHT) * frac) * birth * exit * loud_gate)
                .clamp(0.0, 1.0);
            let base = STAR_SIZE * self.size * star.size_jitter * size_gate;

            // Every star trails a short motion-blur tail along its direction of
            // travel; the tail rides the stream speed, so it lengthens with
            // loudness. An onset extends the *outer* stars' tails into bright
            // streaks (capped short), the extra length being the onset response.
            let blur = BASE_TAIL * (0.35 + speed_factor) * star.speed_jitter;
            let streaking = frac > STREAK_MID && streaking_env > STREAK_EPS;
            let streak_len = if streaking {
                (self.streak * streaking_env * speed_factor * star.speed_jitter * STREAK_LEN_SCALE)
                    .min(MAX_STREAK)
            } else {
                0.0
            };
            let len = blur + streak_len;
            // Screen-space unit direction of motion (motion grows the radius).
            let dx = star.cos / self.aspect;
            let mag = dx.hypot(star.sin).max(1e-6);
            let ux = dx / mag;
            let uy = star.sin / mag;
            let tx = hx - ux * len;
            let ty = hy - uy * len;
            let width = base * (0.8 + 0.4 * frac);
            canvas.line(
                tx,
                ty,
                hx,
                hy,
                width,
                Style::new(slot_for(frac, streaking), bright),
            );
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("loud_env", self.loud_env);
        s.set("rms_env", self.rms_env);
        s.set("onset_env", self.onset_env);
        s.set("prev_onset", if self.prev_onset { 1.0 } else { 0.0 });
        s.set("prev_onset_age_ms", self.prev_onset_age_ms);
        s
    }

    fn restore(&mut self, s: SceneState) {
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

/// A one-word integer hash (an xorshift-multiply finalizer). Used to seed the
/// per-star LCG deterministically; no wall clock, no RNG crate.
#[inline]
fn hash_u32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// The fractional part of `v`, in `0.0..1.0` for finite non-negative input.
#[inline]
fn fract(v: f32) -> f32 {
    v - v.floor()
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
            .expect("key is a starfall parameter");
        *slot = v.clamp(spec.min, spec.max);
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

/// Pick a palette slot. A streaking star burns warm (coral); otherwise the field
/// ramps cool→warm from the dim inner teal outward, giving the rim more heat.
#[inline]
fn slot_for(frac: f32, streaking: bool) -> crate::Slot {
    if streaking {
        4
    } else if frac > 0.66 {
        3
    } else if frac > 0.33 {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::FeatureSnapshot;

    /// A snapshot carrying an onset flag, an `onset_age_ms` and a loudness. The
    /// last argument is the engine-normalized `loudness` the scene drives from
    /// (mirrored into `rms` so the snapshot stays plausible).
    fn snap(onset: bool, onset_age_ms: f32, loudness: f32) -> FeatureSnapshot {
        FeatureSnapshot {
            onset,
            onset_age_ms,
            rms: loudness,
            loudness,
            ..FeatureSnapshot::default()
        }
    }

    fn quiet() -> FeatureSnapshot {
        snap(false, 60_000.0, 0.0)
    }

    fn inited(aspect: f32) -> Starfall {
        let mut s = Starfall::new();
        let ctx = SceneCtx {
            aspect,
            ..SceneCtx::default()
        };
        s.init(&ctx);
        s
    }

    fn render_prims(scene: &mut Starfall) -> Vec<Primitive> {
        let mut c = Canvas::new(scene.aspect);
        scene.render(&mut c);
        c.primitives().to_vec()
    }

    fn count_lines(prims: &[Primitive]) -> usize {
        prims
            .iter()
            .filter(|p| matches!(p, Primitive::Line { .. }))
            .count()
    }

    /// The longest line (streak/tail) in a render, in screen-space length.
    fn max_line_len(prims: &[Primitive]) -> f32 {
        prims
            .iter()
            .filter_map(|p| match p {
                Primitive::Line { x0, y0, x1, y1, .. } => Some((x1 - x0).hypot(y1 - y0)),
                _ => None,
            })
            .fold(0.0f32, f32::max)
    }

    /// The index of the star closest to the centre; safe to advance a few frames
    /// without it reaching its exit radius.
    fn innermost(scene: &Starfall) -> usize {
        scene
            .stars
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.radius.partial_cmp(&b.radius).unwrap())
            .map(|(i, _)| i)
            .expect("pool is non-empty")
    }

    #[test]
    fn onset_extends_streaks_then_relaxes() {
        let mut s = inited(1.0);
        // Drive the streak mechanism with a meaningful gain (the shipped default
        // keeps it gentle; here we exercise the mechanism itself).
        s.streak = 0.8;

        // Every star trails a short motion-blur tail, so a render is always all
        // lines — one per star — whether calm or hit.
        s.update(&quiet(), 0.05);
        let calm = render_prims(&mut s);
        assert_eq!(
            count_lines(&calm),
            s.stars.len(),
            "every star trails a tail"
        );
        let calm_len = max_line_len(&calm);

        // An onset snaps the streak envelope to full: outer stars' tails extend
        // into visibly longer streaks than the calm motion-blur tails.
        s.update(&snap(true, 0.0, 0.4), 0.05);
        let hot = render_prims(&mut s);
        assert!(
            max_line_len(&hot) > calm_len * 1.5,
            "an onset stretches outer stars into longer streaks: {} vs calm {}",
            max_line_len(&hot),
            calm_len
        );

        // Let the envelope decay over a long silence: the streaks relax back to
        // the short calm tails.
        for _ in 0..80 {
            s.update(&quiet(), 0.05);
        }
        assert!(
            s.onset_env < STREAK_EPS,
            "streak envelope decayed: {}",
            s.onset_env
        );
        assert!(
            max_line_len(&render_prims(&mut s)) <= calm_len * 1.2,
            "streaks relaxed back to the calm tail length"
        );
    }

    #[test]
    fn loudness_raises_outward_speed() {
        // Two identically seeded fields: one driven loud, one quiet. The loud one
        // advances its innermost star further over the same frames.
        let mut loud = inited(1.0);
        let mut calm = inited(1.0);
        let idx = innermost(&loud);
        let r0 = loud.stars[idx].radius;
        assert!(
            (calm.stars[idx].radius - r0).abs() < 1e-6,
            "both fields seed identically"
        );

        for _ in 0..4 {
            loud.update(&snap(false, 60_000.0, 0.9), 0.016);
            calm.update(&quiet(), 0.016);
        }

        let r_loud = loud.stars[idx].radius;
        let r_calm = calm.stars[idx].radius;
        assert!(r_loud > r_calm, "louder flows faster: {r_loud} vs {r_calm}");
        assert!(r_calm > r0, "even silence drifts outward, never freezes");
    }

    #[test]
    fn primitives_stay_in_bounds_and_within_the_pool() {
        let mut s = inited(16.0 / 9.0);
        // Drive an onset so both points and lines are present.
        s.update(&snap(true, 0.0, 0.6), 0.05);
        let prims = render_prims(&mut s);

        assert_eq!(
            prims.len(),
            s.stars.len(),
            "one primitive per star: count is bounded by the pool"
        );
        for p in &prims {
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
    fn silence_is_a_smooth_drift() {
        let mut s = inited(1.0);
        // Settle the loudness envelope to the silence floor.
        for _ in 0..40 {
            s.update(&quiet(), 0.05);
        }

        let idx = innermost(&s);
        let mut prev = s.stars[idx].radius;
        let mut deltas = Vec::new();
        for _ in 0..8 {
            s.update(&quiet(), 0.05);
            let r = s.stars[idx].radius;
            deltas.push(r - prev);
            prev = r;
        }

        // Every step moves the star outward by a small, steady amount: it drifts
        // (never a freeze) and the step is consistent frame to frame (no flicker).
        let min = deltas.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = deltas.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(min > 0.0, "the field keeps drifting in silence");
        assert!(max < 0.02, "the drift step stays small: {max}");
        assert!(
            (max - min) < 1e-4,
            "the drift is steady frame to frame (no flicker): {min}..{max}"
        );
    }

    #[test]
    fn state_restore_carries_the_envelopes() {
        let s1 = snap(true, 0.0, 0.5); // onset: lifts loudness, fires the streak
        let s2 = snap(false, 40.0, 0.5); // next frame: envelopes evolve, no onset

        // Reference: drive one frame, snapshot, then advance one more.
        let mut a = inited(1.0);
        a.update(&s1, 0.05);
        let state = a.state();
        a.update(&s2, 0.05);

        // Restored: a fresh scene re-seeds the same field, restore the envelopes,
        // advance the same frame — the loudness and streak envelopes must match.
        let mut b = inited(1.0);
        b.restore(state);
        b.update(&s2, 0.05);

        assert!((a.loud_env - b.loud_env).abs() < 1e-6, "loudness carried");
        assert!((a.onset_env - b.onset_env).abs() < 1e-6, "streak carried");

        // Control: without the restore both envelopes start cold and differ.
        let mut c = inited(1.0);
        c.update(&s2, 0.05);
        assert!(
            (a.onset_env - c.onset_env).abs() > 1e-3,
            "a scene that skipped restore should not match"
        );
    }
}
