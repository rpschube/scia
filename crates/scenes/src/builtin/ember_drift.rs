//! `ember-drift` — sparse embers rising from a near-black field, cooling as they
//! climb, settling into a single breathing ember when the music falls silent.
//!
//! A fixed pool of embers spawns from the lower region at a rate that follows
//! loudness — sparse, tens at most, never a wall of sparks. Each ember rises with
//! a slight deterministic horizontal sway and **cools over its life**: it burns
//! from a hot amber slot through a cooling coral slot to a near-black slot as its
//! intensity ramps down, then dies at the top of its climb or when it has cooled
//! out. A fresh onset briefly lifts the brightness of the youngest embers.
//!
//! # Quiet / Idle — the designed idle handoff
//!
//! This scene implements the storyboard's idle spec itself. When the DSP thread
//! reports [`scia_core::Activity::Quiet`] or [`scia_core::Activity::Idle`],
//! spawning stops; the embers already in flight finish cooling and die, and the
//! scene settles into a single near-black central ember **breathing** at about
//! twelve cycles per minute (a five-second sinusoidal intensity, very dim). The
//! breathing ember eases in as the signal quiets (`idle_env`) and releases
//! promptly the moment signal returns, at which point normal spawning resumes.
//! So after minutes of silence the scene reads as intentional — a single living
//! ember pulsing in the dark rather than a dead black frame.
//!
//! # Determinism
//!
//! An ember is a pure function of its pool slot and a respawn counter `seq`: a
//! tiny inline LCG (the same construction as `starfall`) seeds from the two and
//! draws the ember's spawn position, sway, speed and size. No RNG crate, no wall
//! clock. Because the whole shape derives from `(slot, seq)` and the age, the
//! continuity snapshot need only carry each ember's `seq` and `age`; a restored
//! scene rebuilds every ember's geometry identically.
//!
//! # Geometry
//!
//! An ember of age `a` in `0.0..=1.0` sits at `y = y0·(1 − a)` (it rises from its
//! spawn height toward the top, dying at `a = 1`) and sways horizontally by a
//! small deterministic sine of its age. Embers are drawn with `point` primitives
//! kept at a modest size so they stay legible at the coarse half-block tier.
//!
//! # Parameters
//!
//! | key      | default | range        | meaning                                                        |
//! |----------|---------|--------------|----------------------------------------------------------------|
//! | `embers` | `64`    | `16..=256`   | ember pool size, preallocated at init                          |
//! | `spawn`  | `7.0`   | `0.0..=40.0` | spawn rate (embers/second) at full loudness while active       |
//! | `rise`   | `0.16`  | `0.03..=1.0` | rise/cool rate (inverse lifetime, per second) an ember climbs at |
//! | `drift`  | `0.04`  | `0.0..=0.2`  | horizontal sway amplitude (fraction of canvas width)           |
//! | `size`   | `1.0`   | `0.3..=3.0`  | ember size multiplier                                          |
//!
//! `spawn`, `rise`, `drift` and `size` are live tuning scalars, re-applied every
//! frame through [`Scene::apply_params`] and clamped to their manifest range on
//! read. `embers` is applied at init only — a live change stays inert until the
//! next [`Scene::init`] rebuilds the pool, exactly as `starfall`'s `stars` does.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the loudness, onset and idle envelopes, the breathing
//! phase, the spawn bookkeeping and every ember's `seq`/`age`, so a hot reload
//! keeps the embers in flight and the idle ember mid-breath rather than snapping
//! back to a cold field.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};
use scia_core::Activity;

/// `2π`, one full turn.
const TWO_PI: f32 = std::f32::consts::TAU;
/// Default pool size, used before [`Scene::init`] reads the `embers` param.
const DEFAULT_EMBERS: usize = 64;
/// Loudness-follower time constant (seconds).
const LOUD_TAU: f32 = 0.25;
/// Onset-envelope decay time constant (seconds).
const ONSET_TAU: f32 = 0.3;
/// Idle-envelope ease time constant (seconds): the breathing ember fades in over
/// roughly this long as the signal quiets, and releases just as quickly when it
/// returns, so the handoff both ways reads as intentional.
const IDLE_TAU: f32 = 0.6;
/// The breathing period of the idle ember (seconds): a five-second cycle is the
/// storyboard's ~12 cycles-per-minute resting breath.
const IDLE_PERIOD: f32 = 5.0;
/// Peak intensity of the very dim breathing idle ember.
const IDLE_BASE: f32 = 0.3;
/// Idle-ember diameter (fraction of canvas height) before the `size` param.
const IDLE_SIZE: f32 = 0.03;
/// Base ember diameter (fraction of canvas height) before the `size` param and
/// per-ember jitter. Kept modest so embers stay legible at the coarse tier.
const EMBER_SIZE: f32 = 0.022;
/// How much a fresh onset lifts the brightness of the youngest embers.
const ONSET_LIFT: f32 = 0.6;
/// Embers younger than this age fraction get the onset brightness lift.
const YOUNG_AGE: f32 = 0.25;
/// An ember whose rendered intensity is below this has cooled out for drawing.
const EMBER_EPS: f32 = 0.02;
/// Number of sway cycles over an ember's whole life (a gentle drift, not a wobble).
const SWAY_CYCLES: f32 = 0.6;

/// Palette slot for a hot, freshly risen ember (amber).
const HOT_SLOT: crate::Slot = 3;
/// Palette slot for a cooling ember (coral).
const COOL_SLOT: crate::Slot = 4;
/// Palette slot for a nearly spent ember (near-black neutral).
const SPENT_SLOT: crate::Slot = 5;
/// Palette slot for the breathing idle ember (amber, kept very dim).
const IDLE_SLOT: crate::Slot = 3;

/// `ember-drift`'s parameter manifest: the keys a preset may set, with the
/// defaults, ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "embers",
        default: 64.0,
        min: 16.0,
        max: 256.0,
        doc: "ember pool size, preallocated at init",
    },
    ParamSpec {
        key: "spawn",
        default: 7.0,
        min: 0.0,
        max: 40.0,
        doc: "spawn rate (embers/second) at full loudness while active",
    },
    ParamSpec {
        key: "rise",
        default: 0.16,
        min: 0.03,
        max: 1.0,
        doc: "rise/cool rate (inverse lifetime, per second) an ember climbs at",
    },
    ParamSpec {
        key: "drift",
        default: 0.04,
        min: 0.0,
        max: 0.2,
        doc: "horizontal sway amplitude (fraction of canvas width)",
    },
    ParamSpec {
        key: "size",
        default: 1.0,
        min: 0.3,
        max: 3.0,
        doc: "ember size multiplier",
    },
];

/// A tiny inline linear-congruential generator, seeded per ember from its pool
/// slot and respawn counter. Deterministic and allocation-free; never an RNG
/// crate or the wall clock. The multiplier/increment are the Numerical Recipes
/// LCG, matching `starfall`.
struct Lcg(u32);

impl Lcg {
    /// Seed from a pool slot and its respawn counter, mixing both so successive
    /// lives of one slot and neighbouring slots all get well-separated streams.
    #[inline]
    fn seeded(slot: usize, seq: u32) -> Self {
        let a = hash_u32((slot as u32).wrapping_mul(0x9E37_79B1));
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

/// One ember: a pool slot's current occupant, described entirely by its respawn
/// counter and its age. Everything else — spawn position, sway, speed, size,
/// brightness — is a deterministic function of `(slot, seq)` and `age`, so this
/// is all the continuity a hot reload must carry.
#[derive(Clone, Copy, Debug)]
struct Ember {
    /// Respawn counter; part of the LCG seed so each life of a slot differs.
    seq: u32,
    /// Age in `0.0..=1.0`; `>= 1.0` means the slot is dead and free to reuse.
    age: f32,
}

impl Ember {
    /// A dead ember: an empty pool slot.
    const DEAD: Self = Self { seq: 0, age: 1.0 };

    /// Whether this slot holds a live ember.
    #[inline]
    fn alive(&self) -> bool {
        self.age < 1.0
    }
}

/// The per-ember geometry derived from `(slot, seq)`: the spawn point, the sway
/// and the per-ember jitters. Pure and allocation-free.
struct Shape {
    x0: f32,
    y0: f32,
    sway_dir: f32,
    sway_phase: f32,
    speed_jitter: f32,
    size_jitter: f32,
    bright_jitter: f32,
}

/// Derive an ember's geometry deterministically from its slot and respawn
/// counter. No RNG crate, no wall clock.
fn shape_of(slot: usize, seq: u32) -> Shape {
    let mut rng = Lcg::seeded(slot, seq);
    let x0 = 0.15 + 0.70 * rng.next01();
    let y0 = 0.80 + 0.15 * rng.next01();
    let sway_dir = if rng.next01() < 0.5 { -1.0 } else { 1.0 };
    let sway_phase = rng.next01() * TWO_PI;
    let speed_jitter = 0.7 + 0.6 * rng.next01();
    let size_jitter = 0.7 + 0.6 * rng.next01();
    let bright_jitter = 0.7 + 0.3 * rng.next01();
    Shape {
        x0,
        y0,
        sway_dir,
        sway_phase,
        speed_jitter,
        size_jitter,
        bright_jitter,
    }
}

/// The rising-ember scene.
#[derive(Clone, Debug)]
pub struct EmberDrift {
    // --- pool, sized at init -------------------------------------------
    /// The ember pool, preallocated at init; render and spawn never resize it.
    embers: Vec<Ember>,

    // --- live state ----------------------------------------------------
    /// Smoothed loudness in `0.0..=1.0` driving the spawn rate.
    loud_env: f32,
    /// Onset envelope in `0.0..=1.0`: snaps to 1 on an onset, decays to 0.
    onset_env: f32,
    /// Idle envelope in `0.0..=1.0`: eases to 1 while quiet/idle, 0 while active.
    idle_env: f32,
    /// Breathing phase of the idle ember, radians, wrapped to `0..2π`.
    breath: f32,
    /// Fractional spawn accumulator: whole units spawn an ember.
    spawn_credit: f32,
    /// Global respawn counter, mixed into each spawned ember's LCG seed.
    spawn_seq: u32,
    /// Previous frame's onset flag, for rising-edge detection.
    prev_onset: bool,
    /// Previous frame's `onset_age_ms`, to catch a fresh onset that resets the age.
    prev_onset_age_ms: f32,

    // --- parameters ----------------------------------------------------
    embers_param: f32,
    spawn: f32,
    rise: f32,
    drift: f32,
    size: f32,
}

impl EmberDrift {
    /// An `ember-drift` scene with default parameters. Call [`Scene::init`]
    /// before driving it to apply preset parameters and build the ember pool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            embers: Vec::new(),
            loud_env: 0.0,
            onset_env: 0.0,
            idle_env: 0.0,
            breath: 0.0,
            spawn_credit: 0.0,
            spawn_seq: 0,
            prev_onset: false,
            prev_onset_age_ms: 0.0,
            embers_param: DEFAULT_EMBERS as f32,
            spawn: 7.0,
            rise: 0.16,
            drift: 0.04,
            size: 1.0,
        }
    }

    /// Refresh the tuning scalars from `params`, and only those — the pool and
    /// the envelopes are left untouched so a live re-apply does not reset the
    /// animation. Shared by [`Scene::init`] and [`Scene::apply_params`].
    /// `embers` is read here too, but only [`Scene::init`] acts on it (by
    /// rebuilding the pool); a live change stays inert until then, exactly as
    /// `starfall`'s `stars` does. Allocation-free.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.embers_param, params, "embers");
        read_param(&mut self.spawn, params, "spawn");
        read_param(&mut self.rise, params, "rise");
        read_param(&mut self.drift, params, "drift");
        read_param(&mut self.size, params, "size");
    }

    /// Rebuild the ember pool to the current `embers` count, all slots dead so the
    /// field starts near-black and fills in as loudness spawns embers.
    fn build_pool(&mut self) {
        let count = (self.embers_param.round() as i32).clamp(1, 1024) as usize;
        self.embers.clear();
        self.embers.reserve(count);
        self.embers.resize(count, Ember::DEAD);
    }

    /// Spawn one ember into the first dead slot, if any is free. A full pool (all
    /// embers still alive) simply drops the spawn — the field is meant to be
    /// sparse, so this is the rare case, not the common one.
    fn spawn_one(&mut self) {
        if let Some(slot) = self.embers.iter().position(|e| !e.alive()) {
            self.spawn_seq = self.spawn_seq.wrapping_add(1);
            self.embers[slot] = Ember {
                seq: self.spawn_seq,
                age: 0.0,
            };
        }
    }
}

impl Default for EmberDrift {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for EmberDrift {
    fn id(&self) -> &'static str {
        "ember-drift"
    }

    fn mood(&self) -> &'static str {
        "organic"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.build_pool();
        self.loud_env = 0.0;
        self.onset_env = 0.0;
        self.idle_env = 0.0;
        self.breath = 0.0;
        self.spawn_credit = 0.0;
        self.spawn_seq = 0;
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the pool and every envelope carry across, so a
        // live mapping is honored without resetting the field. A live `embers`
        // change stays inert until the next `init` rebuilds the pool.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Loudness follower drives the spawn rate; smoothed so a spawn burst
        // eases in and out rather than chattering. Reads the engine-normalized
        // loudness (0..1), not the raw rms, so real music actually spawns embers.
        let loud = f.loudness.clamp(0.0, 1.0);
        self.loud_env += (loud - self.loud_env) * (1.0 - decay(dt, LOUD_TAU));

        // Onset envelope: snap to full on a fresh onset, otherwise decay. Fire on
        // a rising edge, or when a fresh onset resets `onset_age_ms` below the
        // previous frame's value, so a held onset never re-fires.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            self.onset_env = 1.0;
        } else {
            self.onset_env *= decay(dt, ONSET_TAU);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;

        // Idle envelope eases toward 1 while the pipeline is quiet or idle and
        // toward 0 while it is active — the designed handoff into and out of the
        // breathing resting state.
        let idle_target = if f.activity == Activity::Active {
            0.0
        } else {
            1.0
        };
        self.idle_env += (idle_target - self.idle_env) * (1.0 - decay(dt, IDLE_TAU));

        // The idle ember breathes on a fixed clock so its pulse is steady
        // regardless of audio.
        self.breath = (self.breath + dt * TWO_PI / IDLE_PERIOD).rem_euclid(TWO_PI);

        // Advance every live ember's age; the rise/cool rate rides the `rise`
        // param and the per-ember speed jitter. An ember that reaches full age
        // dies (its slot frees for a future spawn).
        for (slot, e) in self.embers.iter_mut().enumerate() {
            if e.alive() {
                let sj = shape_of(slot, e.seq).speed_jitter;
                e.age += dt * self.rise * sj;
                if e.age >= 1.0 {
                    *e = Ember::DEAD;
                }
            }
        }

        // Spawn only while active; quiet and idle passages stop spawning so the
        // field cools out into the resting ember. The rate follows loudness.
        if f.activity == Activity::Active {
            let rate = self.spawn * self.loud_env;
            self.spawn_credit += rate * dt;
            // Cap the credit so a long loud dt cannot fire a huge synchronized
            // burst; sparse is the design.
            self.spawn_credit = self.spawn_credit.min(self.embers.len() as f32);
            while self.spawn_credit >= 1.0 {
                self.spawn_credit -= 1.0;
                self.spawn_one();
            }
        } else {
            // Drop any pending credit so spawning stops immediately on quiet.
            self.spawn_credit = 0.0;
        }
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // Live embers: rising, swaying, cooling from hot amber through coral to a
        // near-black spent slot as their intensity ramps down.
        for (slot, e) in self.embers.iter().enumerate() {
            if !e.alive() {
                continue;
            }
            let a = e.age.clamp(0.0, 1.0);
            let s = shape_of(slot, e.seq);

            let y = (s.y0 * (1.0 - a)).clamp(0.0, 1.0);
            let sway = self.drift * s.sway_dir * (a * SWAY_CYCLES * TWO_PI + s.sway_phase).sin();
            let x = (s.x0 + sway).clamp(0.0, 1.0);

            // Cooling: bright at birth, ramping to near-zero at death. The
            // youngest embers get a brief onset brightness lift.
            let mut intensity = s.bright_jitter * (1.0 - a);
            if a < YOUNG_AGE {
                intensity *= 1.0 + ONSET_LIFT * self.onset_env;
            }
            let intensity = intensity.clamp(0.0, 1.0);
            if intensity < EMBER_EPS {
                continue;
            }

            let size = EMBER_SIZE * self.size * s.size_jitter * (0.7 + 0.3 * (1.0 - a));
            canvas.point(x, y, size, Style::new(cool_slot(a), intensity));
        }

        // The breathing idle ember: a single very dim warm point at the centre,
        // pulsing on the five-second resting breath, scaled by the idle envelope
        // so it appears only as the field falls quiet and fades as signal returns.
        if self.idle_env > 0.01 {
            let breath = 0.5 + 0.5 * self.breath.sin();
            let intensity = (IDLE_BASE * breath * self.idle_env).clamp(0.0, 1.0);
            if intensity >= EMBER_EPS {
                canvas.point(
                    0.5,
                    0.5,
                    IDLE_SIZE * self.size,
                    Style::new(IDLE_SLOT, intensity),
                );
            }
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("loud_env", self.loud_env);
        s.set("onset_env", self.onset_env);
        s.set("idle_env", self.idle_env);
        s.set("breath", self.breath);
        s.set("spawn_credit", self.spawn_credit);
        s.set("spawn_seq", self.spawn_seq as f32);
        s.set("prev_onset", if self.prev_onset { 1.0 } else { 0.0 });
        s.set("prev_onset_age_ms", self.prev_onset_age_ms);
        for (i, e) in self.embers.iter().enumerate() {
            s.set(&format!("e{i}_seq"), e.seq as f32);
            s.set(&format!("e{i}_age"), e.age);
        }
        s
    }

    fn restore(&mut self, s: SceneState) {
        if let Some(v) = s.get("loud_env") {
            self.loud_env = v;
        }
        if let Some(v) = s.get("onset_env") {
            self.onset_env = v;
        }
        if let Some(v) = s.get("idle_env") {
            self.idle_env = v;
        }
        if let Some(v) = s.get("breath") {
            self.breath = v;
        }
        if let Some(v) = s.get("spawn_credit") {
            self.spawn_credit = v;
        }
        if let Some(v) = s.get("spawn_seq") {
            self.spawn_seq = v.max(0.0) as u32;
        }
        if let Some(v) = s.get("prev_onset") {
            self.prev_onset = v >= 0.5;
        }
        if let Some(v) = s.get("prev_onset_age_ms") {
            self.prev_onset_age_ms = v;
        }
        for (i, e) in self.embers.iter_mut().enumerate() {
            match (s.get(&format!("e{i}_seq")), s.get(&format!("e{i}_age"))) {
                (Some(seq), Some(age)) => {
                    *e = Ember {
                        seq: seq.max(0.0) as u32,
                        age,
                    };
                }
                _ => *e = Ember::DEAD,
            }
        }
    }
}

/// Pick the cooling palette slot for an ember of age `a`: hot amber while young,
/// cooling coral through the middle, near-black once nearly spent.
#[inline]
fn cool_slot(a: f32) -> crate::Slot {
    if a < 0.30 {
        HOT_SLOT
    } else if a < 0.65 {
        COOL_SLOT
    } else {
        SPENT_SLOT
    }
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
            .expect("key is an ember-drift parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::{Activity, FeatureSnapshot};

    fn inited() -> EmberDrift {
        let mut s = EmberDrift::new();
        s.init(&SceneCtx::default());
        s
    }

    /// An active snapshot at a given loudness, no onset. The argument is the
    /// engine-normalized `loudness` the scene now drives from; `rms` is set to the
    /// same value so the snapshot stays internally plausible.
    fn active(loudness: f32) -> FeatureSnapshot {
        FeatureSnapshot {
            rms: loudness,
            loudness,
            onset: false,
            onset_age_ms: 60_000.0,
            activity: Activity::Active,
            ..FeatureSnapshot::default()
        }
    }

    /// An idle snapshot: silent, quiet for a long time.
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

    fn alive_count(scene: &EmberDrift) -> usize {
        scene.embers.iter().filter(|e| e.alive()).count()
    }

    fn render_points(scene: &mut EmberDrift) -> Vec<Primitive> {
        let mut c = Canvas::new(1.0);
        scene.render(&mut c);
        c.primitives().to_vec()
    }

    #[test]
    fn spawn_rate_rises_with_rms() {
        // Two fields driven for the same short window, one loud, one quiet. The
        // loud field must spawn (and hold alive) markedly more embers. The window
        // is short relative to the ember lifetime so few die during it.
        let mut loud = inited();
        let mut soft = inited();
        for _ in 0..40 {
            loud.update(&active(0.9), 0.03);
            soft.update(&active(0.1), 0.03);
        }
        let loud_n = alive_count(&loud);
        let soft_n = alive_count(&soft);
        assert!(
            loud_n > soft_n,
            "louder audio spawns more embers: loud {loud_n} vs soft {soft_n}"
        );
        assert!(loud_n > 0, "a loud active passage spawns embers");
    }

    #[test]
    fn realistic_rms_but_normalized_loudness_spawns_a_healthy_field() {
        // The regression that motivated the engine-normalized loudness: real
        // music sits around rms 0.08, but its normalized loudness is ~0.7. A scene
        // reading raw rms spawned ~2 embers (canvas coverage ~0.001); reading
        // `loudness` it must fill a healthy field. Drive a few seconds of such a
        // snapshot and assert a healthy rendered primitive count.
        let mut s = inited();
        let f = FeatureSnapshot {
            rms: 0.08,
            loudness: 0.7,
            onset: false,
            onset_age_ms: 60_000.0,
            activity: Activity::Active,
            ..FeatureSnapshot::default()
        };
        for _ in 0..300 {
            // ~9 s at 30 ms/frame, long enough to reach the steady ember count
            s.update(&f, 0.03);
        }
        let prims = render_points(&mut s).len();
        assert!(
            prims > 20,
            "realistic rms (0.08) with normalized loudness (0.7) should spawn a \
             healthy ember field, got {prims} primitives"
        );
    }

    #[test]
    fn quiet_stops_spawning_then_signal_resumes_it() {
        // Warm the field with a loud active passage.
        let mut s = inited();
        for _ in 0..40 {
            s.update(&active(0.8), 0.03);
        }
        assert!(alive_count(&s) > 0, "active spawning fills the field");

        // Go idle for long enough that every in-flight ember cools out and no new
        // ember spawns: the field empties.
        for _ in 0..2000 {
            s.update(&idle(), 0.05);
        }
        assert_eq!(
            alive_count(&s),
            0,
            "quiet stops spawning and the field cools out"
        );

        // Signal returns: spawning resumes promptly.
        for _ in 0..40 {
            s.update(&active(0.8), 0.03);
        }
        assert!(
            alive_count(&s) > 0,
            "spawning resumes promptly when signal returns"
        );
    }

    #[test]
    fn idle_ember_breathes_after_sustained_quiet() {
        // Sustained quiet: the idle envelope rises and the central ember appears.
        let mut s = inited();
        for _ in 0..400 {
            s.update(&idle(), 0.05);
        }
        assert!(
            s.idle_env > 0.9,
            "the idle envelope settles high in silence: {}",
            s.idle_env
        );

        // The lone rendered ember is the central breathing one, and its intensity
        // varies sinusoidally frame to frame — it breathes rather than sitting
        // flat (or dead).
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut saw_centre = false;
        for _ in 0..120 {
            s.update(&idle(), 0.05);
            for p in render_points(&mut s) {
                if let Primitive::Point { x, y, style, .. } = p {
                    assert!(
                        (x - 0.5).abs() < 1e-3 && (y - 0.5).abs() < 1e-3,
                        "the only ember in deep silence is the central one"
                    );
                    saw_centre = true;
                    min = min.min(style.intensity);
                    max = max.max(style.intensity);
                }
            }
        }
        assert!(saw_centre, "a breathing ember is drawn in deep silence");
        assert!(
            max - min > 0.1,
            "the idle ember breathes (intensity varies): {min}..{max}"
        );
        assert!(
            max <= IDLE_BASE + 1e-3,
            "the idle ember stays very dim: {max}"
        );
    }

    #[test]
    fn embers_cool_over_life() {
        // Spawn a single ember, then watch its rendered intensity fall as it ages.
        // Drive quiet so no further embers spawn to confuse the reading.
        let mut s = inited();
        s.spawn_one();
        let slot = s.embers.iter().position(|e| e.alive()).unwrap();

        let mut prev = f32::INFINITY;
        let mut samples = 0;
        for _ in 0..20 {
            // Advance the ember with quiet frames so nothing else spawns, then
            // read this ember's cooling intensity directly from its age.
            s.update(&idle(), 0.1);
            if !s.embers[slot].alive() {
                break;
            }
            let a = s.embers[slot].age;
            let sh = shape_of(slot, s.embers[slot].seq);
            let intensity = (sh.bright_jitter * (1.0 - a)).clamp(0.0, 1.0);
            assert!(
                intensity <= prev + 1e-6,
                "an ember only cools as it ages: {intensity} after {prev}"
            );
            prev = intensity;
            samples += 1;
        }
        assert!(
            samples > 3,
            "the ember was observed cooling over several frames"
        );
    }

    #[test]
    fn render_primitives_stay_in_bounds() {
        let mut s = inited();
        for i in 0..60 {
            let mut f = active(0.7);
            f.onset = i % 5 == 0;
            f.onset_age_ms = if f.onset { 0.0 } else { 50.0 };
            s.update(&f, 0.03);
        }
        let mut c = Canvas::new(16.0 / 9.0);
        s.render(&mut c);
        for p in c.primitives() {
            match p {
                Primitive::Point { x, y, size, .. } => {
                    assert!((0.0..=1.0).contains(x) && (0.0..=1.0).contains(y));
                    assert!((0.0..=1.0).contains(size));
                }
                other => panic!("ember-drift draws only points, got {other:?}"),
            }
        }
    }

    #[test]
    fn state_restore_carries_the_embers_and_envelopes() {
        // Warm a scene into a lively state, snapshot, advance one frame.
        let mut a = inited();
        for i in 0..50 {
            let mut f = active(0.8);
            f.onset = i % 4 == 0;
            f.onset_age_ms = if f.onset { 0.0 } else { 40.0 };
            a.update(&f, 0.03);
        }
        let live = alive_count(&a);
        assert!(live > 0, "the reference scene has embers in flight");
        let next = active(0.3);
        let state = a.state();
        a.update(&next, 0.03);

        // A fresh scene that restores the snapshot and advances the same frame
        // must reproduce the envelopes and keep the same embers alive.
        let mut b = inited();
        b.restore(state);
        b.update(&next, 0.03);

        assert!((a.loud_env - b.loud_env).abs() < 1e-6, "loudness carried");
        assert!((a.onset_env - b.onset_env).abs() < 1e-6, "onset carried");
        assert!((a.breath - b.breath).abs() < 1e-6, "breath phase carried");
        assert_eq!(
            alive_count(&a),
            alive_count(&b),
            "the in-flight embers are carried across the restore"
        );

        // Control: a scene that skipped the restore has a cold, empty field.
        let mut c = inited();
        c.update(&next, 0.03);
        assert!(
            alive_count(&c) < alive_count(&a),
            "a scene that skipped restore should not carry the embers"
        );
    }
}
