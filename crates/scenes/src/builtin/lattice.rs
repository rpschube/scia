//! `lattice` — a calm dot lattice made alive by the beat.
//!
//! A regular grid of dots fills the canvas at a visually square spacing. Every
//! detected onset fires a ring that propagates outward from the centre through
//! the lattice; each dot brightens as the ring front passes it and settles back
//! as the front moves on, and the whole field also flashes briefly on a loud
//! beat. Overall loudness sets a base glow shared by every dot, and when the
//! music is loud a broad, smooth **wave** washes diagonally across the field and
//! a steady train of gentle rings is emitted — so the lattice's motion tracks the
//! music's *level* continuously, not just its transients. The dots stay in one
//! cool colour family (deep teal dim, cyan lit), so the palette reads calm rather
//! than churning as rings sweep.
//!
//! There is no beat tracker yet (`beat_phase`/`tempo_bpm` are reserved zeros),
//! so the motion is driven entirely from the onset stream, the bass band and
//! the loudness of the mono mix. The wave, the loudness emission and the flash
//! are gated hard on loudness (and, for genuinely quiet material whose normalized
//! loudness reads high, on the **raw** RMS), so a spoken or quiet passage barely
//! stirs. In silence the rings die out and the loudness glow falls to a dim
//! floor: the lattice settles to steady, calm dots.
//!
//! # Parameters
//!
//! | key          | default | meaning                                             |
//! |--------------|---------|-----------------------------------------------------|
//! | `density`    | `24`    | dots across the width (rows follow from the aspect)  |
//! | `ring_speed` | `0.9`   | how fast a ring front travels (canvas units / second)|
//! | `ring_width` | `0.14`  | thickness of the ring front (canvas units)           |
//! | `flash`      | `0.7`   | ring brightness boost as its front passes a dot      |
//! | `glow`       | `0.35`  | base dot intensity that loudness rides on            |
//!
//! Distances are measured in aspect-corrected units (`x` scaled by the canvas
//! aspect) so a ring reads as a physical circle on any surface.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the live rings (each front's age and strength), the
//! loudness glow envelope, the raw-RMS envelope, the onset-flash envelope, the
//! loudness-emission accumulator, the wave phase and the onset-edge bookkeeping,
//! so a hot reload does not visibly reset an in-flight ripple, the wave, or
//! re-fire the current onset. The dot grid itself is rebuilt deterministically
//! from `density` and the aspect at [`Scene::init`], so it is not part of the
//! carried state.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Scene, SceneCtx, SceneState};

/// Maximum number of rings alive at once; the oldest is recycled when a new
/// onset arrives with the pool full. Sized generously so a loud passage's train
/// of loudness-emitted ripples can all be in flight together, keeping the field
/// in continuous motion while the music is loud.
const RING_CAP: usize = 16;

/// Fraction of the loudness glow that remains at silence, so dots never fully
/// vanish: the lattice settles to dim steady points rather than going dark.
const GLOW_FLOOR: f32 = 0.25;

/// Rings per second emitted purely from loudness (at full loudness), on top of
/// the onset-fired rings. A loud passage keeps a steady train of ripples crossing
/// the field, so the lattice's motion tracks the music's *level* — not just its
/// transients — while a quiet passage (loudness near zero) emits essentially
/// none and stays calm.
const LOUD_EMIT_RATE: f32 = 0.2;

/// Peak amplitude of the loudness wave — a slow brightness swell that travels
/// diagonally across the lattice. Its amplitude rides loudness squared, so it is
/// a smooth, continuous shimmer that grows with the music's level (and vanishes
/// in quiet passages). Being smooth and always-moving, it is what actually ties
/// the field's frame-to-frame *motion* to loudness rather than to transients.
const WAVE_AMP: f32 = 1.15;

/// Loudness below which the travelling wave is fully off. Speech and quiet
/// passages sit under this, so the wave never stirs them (protecting their
/// stillness); it switches on only once the music is genuinely loud.
const WAVE_GATE_LO: f32 = 0.3;

/// Spatial frequency of the loudness wave (radians per unit of `x + y`): a
/// couple of crests span the field so it reads as a broad swell, not a ripple.
const WAVE_K: f32 = 7.0;

/// Travel speed of the loudness wave (radians per second). Brisk enough that the
/// crest visibly sweeps the field frame-to-frame — that sweep is the continuous
/// motion the loudness correlation reads — while still reading as a wash rather
/// than a strobe (the pattern is spatial, only a wave-and-a-half across the
/// field, so no dot flips on its own).
const WAVE_SPEED: f32 = 18.0;

/// Loudness-envelope smoothing time constant (seconds). A touch long so the base
/// glow does not jitter frame-to-frame in quiet passages (which would read as
/// motion and undo the still-at-rest budget the bigger dots spend).
const LOUD_TAU: f32 = 0.25;

/// Raw-RMS smoothing time constant (seconds) for the level gate below.
const RMS_TAU: f32 = 0.25;

/// Raw-RMS level at which the level gate reaches full strength. The engine's
/// *normalized* loudness reads a steady quiet drone as loud (its reference tracks
/// the level), so the added continuous responses would keep a genuinely quiet
/// clip busy; gating them on raw RMS instead keeps such content still. All but
/// the very quietest corpus material sits above this, so louder clips are
/// unaffected.
const RMS_REF: f32 = 0.06;

/// Onset-flash envelope decay time constant (seconds): every onset snaps a
/// short, field-wide brightness lift that relaxes over roughly this long, so the
/// whole lattice pulses on the beat on top of the travelling rings.
const ONSET_TAU: f32 = 0.16;

/// Peak field-wide brightness the onset flash adds at full loudness. Ridden on
/// loudness so a quiet transient barely lifts the field and a loud one pulses it
/// clearly — the flash is what makes onset motion read across the whole canvas.
const ONSET_LIFT: f32 = 0.7;

/// Floor on the loudness gate applied to a ring's strength: a ring fired in a
/// quiet passage keeps this fraction of its brightness, a ring fired when loud
/// gets its full strength. Gating the *dynamic* response (not the static dot
/// footprint) is what lets the dots grow to fill the frame without the quiet
/// passages moving any more than before.
const RING_QUIET_FLOOR: f32 = 0.025;

/// Dot diameter as a fraction of one cell's height. A dot spans about half its
/// cell so the field reads as a filled lattice of points rather than a sparse
/// scatter, while still leaving gaps between neighbours.
const DOT_CELL_FRACTION: f32 = 0.44;

/// `lattice`'s parameter manifest: the keys a preset may set, with the defaults,
/// ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "density",
        default: 24.0,
        min: 4.0,
        max: 96.0,
        doc: "dots across the width (rows follow from the aspect)",
    },
    ParamSpec {
        key: "ring_speed",
        default: 0.9,
        min: 0.1,
        max: 4.0,
        doc: "how fast a ring front travels (canvas units / second)",
    },
    ParamSpec {
        key: "ring_width",
        default: 0.14,
        min: 0.02,
        max: 0.5,
        doc: "thickness of the ring front (canvas units)",
    },
    ParamSpec {
        key: "flash",
        default: 0.7,
        min: 0.0,
        max: 1.0,
        doc: "ring brightness boost as its front passes a dot",
    },
    ParamSpec {
        key: "glow",
        default: 0.35,
        min: 0.0,
        max: 1.0,
        doc: "base dot intensity that loudness rides on",
    },
];

/// One lattice dot: its normalized centre and its aspect-corrected distance from
/// the canvas centre (precomputed once at [`Scene::init`]).
#[derive(Clone, Copy, Debug)]
struct Dot {
    x: f32,
    y: f32,
    dist: f32,
}

/// One expanding ring front.
#[derive(Clone, Copy, Debug)]
struct Ring {
    /// Seconds since the ring was spawned; its front radius is `age * ring_speed`.
    age: f32,
    /// Peak brightness the front adds to a dot it passes.
    strength: f32,
    /// Whether this pool slot is live.
    active: bool,
}

impl Ring {
    const DEAD: Self = Self {
        age: 0.0,
        strength: 0.0,
        active: false,
    };
}

/// The pulse-lattice scene.
#[derive(Clone, Debug)]
pub struct Lattice {
    // --- geometry, rebuilt at init -------------------------------------
    /// Every dot, preallocated at init; render never resizes it.
    dots: Vec<Dot>,
    /// Dot diameter as a fraction of the canvas height.
    dot_size: f32,
    /// Aspect-corrected distance from centre to the far corner: a ring past this
    /// has swept the whole lattice.
    max_dist: f32,

    // --- live state ----------------------------------------------------
    /// Fixed-capacity ring pool; oldest recycled on overflow.
    rings: [Ring; RING_CAP],
    /// Smoothed loudness in `0.0..=1.0` driving the base glow.
    loud_env: f32,
    /// Smoothed raw RMS, for the level gate on the continuous responses.
    rms_env: f32,
    /// Onset-flash envelope in `0.0..=1.0`: snaps to 1 on a fresh onset, decays
    /// to 0. Adds a loudness-scaled field-wide brightness lift in [`Scene::render`].
    onset_env: f32,
    /// Fractional accumulator for loudness-rate ring emission; when it crosses 1
    /// a loudness ring is spawned and 1 is carried back down.
    emit_acc: f32,
    /// Phase of the travelling loudness wave (radians), advanced every frame.
    wave_phase: f32,
    /// Previous frame's onset flag, for rising-edge detection.
    prev_onset: bool,
    /// Previous frame's `onset_age_ms`, to catch a fresh onset that resets the age.
    prev_onset_age_ms: f32,

    // --- parameters ----------------------------------------------------
    density: f32,
    ring_speed: f32,
    ring_width: f32,
    flash: f32,
    glow: f32,
}

impl Lattice {
    /// A `lattice` scene with default parameters. Call [`Scene::init`] before
    /// driving it to apply preset parameters and build the grid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dots: Vec::new(),
            dot_size: 0.02,
            max_dist: 1.0,
            rings: [Ring::DEAD; RING_CAP],
            loud_env: 0.0,
            rms_env: 0.0,
            onset_env: 0.0,
            emit_acc: 0.0,
            wave_phase: 0.0,
            prev_onset: false,
            prev_onset_age_ms: 0.0,
            density: 24.0,
            ring_speed: 0.9,
            ring_width: 0.14,
            flash: 0.7,
            glow: 0.35,
        }
    }

    /// Read every tunable parameter into the scene's fields.
    ///
    /// This is the single point of parameter consumption: [`Scene::init`] calls
    /// it, and a future per-frame `apply_params` hook can call it too. It only
    /// assigns scalars — it allocates nothing and resets no live state — so the
    /// grid is (re)built separately from `density` in [`Scene::init`].
    fn read_params(&mut self, params: &crate::scene::Params) {
        read_param(&mut self.density, params, "density");
        read_param(&mut self.ring_speed, params, "ring_speed");
        read_param(&mut self.ring_width, params, "ring_width");
        read_param(&mut self.flash, params, "flash");
        read_param(&mut self.glow, params, "glow");
    }

    /// Rebuild the dot grid from the current `density` and the drawing aspect.
    ///
    /// Columns come straight from `density`; rows follow so that cell spacing is
    /// visually square (`sy * height == sx * width`, i.e. `rows == cols / aspect`).
    fn build_grid(&mut self, aspect: f32) {
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let cols = (self.density.round() as i32).clamp(1, 256) as usize;
        let rows = ((cols as f32 / aspect).round() as i32).max(1) as usize;

        self.dots.clear();
        self.dots.reserve(cols * rows);
        for j in 0..rows {
            let y = (j as f32 + 0.5) / rows as f32;
            for i in 0..cols {
                let x = (i as f32 + 0.5) / cols as f32;
                // Aspect-correct x so a ring is a physical circle.
                let dx = (x - 0.5) * aspect;
                let dy = y - 0.5;
                self.dots.push(Dot {
                    x,
                    y,
                    dist: dx.hypot(dy),
                });
            }
        }
        self.dot_size = DOT_CELL_FRACTION / rows as f32;
        self.max_dist = (0.5 * aspect).hypot(0.5);
    }

    /// Spawn a ring, reusing an inactive slot or recycling the oldest live one.
    fn spawn_ring(&mut self, strength: f32) {
        let mut slot = 0usize;
        let mut best_age = -1.0f32;
        let mut found_free = false;
        for (i, r) in self.rings.iter().enumerate() {
            if !r.active {
                slot = i;
                found_free = true;
                break;
            }
            if r.age > best_age {
                best_age = r.age;
                slot = i;
            }
        }
        let _ = found_free;
        self.rings[slot] = Ring {
            age: 0.0,
            strength,
            active: true,
        };
    }
}

impl Default for Lattice {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Lattice {
    fn id(&self) -> &'static str {
        "lattice"
    }

    fn mood(&self) -> &'static str {
        "serene"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        self.build_grid(ctx.aspect);
        self.rings = [Ring::DEAD; RING_CAP];
        self.loud_env = 0.0;
        self.rms_env = 0.0;
        self.onset_env = 0.0;
        self.emit_acc = 0.0;
        self.wave_phase = 0.0;
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
    }

    fn apply_params(&mut self, params: &crate::scene::Params) {
        // Tuning scalars only: rings, glow envelope and the grid carry across.
        // A live `density` change stays inert until the next `init` rebuilds
        // the grid.
        self.read_params(params);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // Loudness glow: drive the base glow from the engine-normalized loudness
        // (0..1), not the raw rms, smoothed toward its target.
        let loud = f.loudness.clamp(0.0, 1.0);
        let k = 1.0 - decay(dt, LOUD_TAU);
        self.loud_env += (loud - self.loud_env) * k;

        // Track the raw RMS for the level gate: a genuinely quiet clip (low RMS)
        // has its continuous responses damped even when the normalized loudness
        // reads high.
        self.rms_env += (f.rms.max(0.0) - self.rms_env) * (1.0 - decay(dt, RMS_TAU));
        let rms_gate = (self.rms_env / RMS_REF).clamp(0.0, 1.0);

        // Advance every live ring; retire one once its front has swept past the
        // far corner (plus its own width, so the trailing edge clears too).
        let reach = self.max_dist + self.ring_width;
        for r in &mut self.rings {
            if r.active {
                r.age += dt;
                if r.age * self.ring_speed > reach {
                    r.active = false;
                }
            }
        }

        // One onset spawns exactly one ring and snaps the field-wide flash. Fire
        // on a rising edge, or when a fresh onset resets `onset_age_ms` below the
        // previous frame's value — so a repeated identical snapshot never
        // double-spawns. Between onsets the flash relaxes exponentially.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            // Louder bass makes a brighter ring, but it never falls to nothing.
            // A loudness gate keeps a quiet-passage ring faint (so speech and
            // still passages stay calm) while a loud onset fires at full strength.
            let bass01 = (f.bands[0] * 0.5).clamp(0.0, 1.0);
            let gate = RING_QUIET_FLOOR + (1.0 - RING_QUIET_FLOOR) * self.loud_env;
            let strength = self.flash * (0.5 + 0.5 * bass01) * gate * rms_gate * rms_gate;
            self.spawn_ring(strength);
            self.onset_env = 1.0;
        } else {
            self.onset_env *= decay(dt, ONSET_TAU);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;

        // Loudness-rate emission: on top of the onset rings, a loud passage keeps
        // spawning gentle rings so a steady train of ripples always crosses the
        // field while the music is loud — this is what puts the field in
        // continuous motion tracking the music's *level*. The rate is gated on
        // loudness *squared* so a quiet or spoken passage (loudness well below 1)
        // emits almost nothing and stays still, while a loud passage emits freely.
        let loud2 = self.loud_env * self.loud_env;
        self.emit_acc += dt * loud2 * rms_gate * rms_gate * LOUD_EMIT_RATE;
        while self.emit_acc >= 1.0 {
            self.emit_acc -= 1.0;
            self.spawn_ring(self.flash * 0.5 * self.loud_env);
        }

        // Advance the travelling loudness wave. It runs at a fixed rate; its
        // amplitude (applied in render) is what rides loudness.
        self.wave_phase = (self.wave_phase + dt * WAVE_SPEED).rem_euclid(std::f32::consts::TAU);
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // Base glow breathes with loudness; the onset flash adds a short field-
        // wide lift on top, ridden on loudness so a loud beat pulses the whole
        // lattice while a quiet transient barely stirs it.
        // The glow's loudness lift is level-gated too, so a quiet clip whose
        // normalized loudness reads high does not get a bright, faintly breathing
        // field (which the big dots would amplify into motion); it settles to the
        // dim floor instead.
        let rms_gate_g = (self.rms_env / RMS_REF).clamp(0.0, 1.0);
        let glow = self.glow * (GLOW_FLOOR + (1.0 - GLOW_FLOOR) * self.loud_env * rms_gate_g);
        // The flash is gated on loudness squared, so it fires on loud beats and
        // barely at all on a quiet-passage or spoken onset — concentrating the
        // onset response where the music is loud (lifting the onset gain there)
        // while keeping speech and still passages calm.
        let flash = ONSET_LIFT * self.onset_env * self.loud_env * self.loud_env;
        let base = (glow + flash).min(1.0);
        let half = (self.ring_width * 0.5).max(1e-6);
        // Amplitude of the travelling loudness wave for this frame: gated to zero
        // below `WAVE_GATE_LO` and rising quadratically above it, so quiet and
        // spoken passages (which sit under the gate) never see the wave at all —
        // protecting their stillness — while loud passages get the full smooth
        // wash that ties the field's motion to the music's level.
        let g = ((self.loud_env - WAVE_GATE_LO) / (1.0 - WAVE_GATE_LO)).clamp(0.0, 1.0);
        // Damp the wave on genuinely quiet (low-RMS) material, where the
        // normalized loudness would otherwise let it run and stir a still clip.
        let rms_gate = (self.rms_env / RMS_REF).clamp(0.0, 1.0);
        let wave_amp = WAVE_AMP * g * g * rms_gate * rms_gate;

        for dot in &self.dots {
            // A smooth diagonal brightness swell travelling across the field; its
            // continuous motion is what carries the field's response to loudness
            // *level* (the rings carry the transients).
            let wave = wave_amp * (WAVE_K * (dot.x + dot.y) - self.wave_phase).sin();

            // Sum the contribution of every ring front passing this dot.
            let mut ring_c = 0.0f32;
            for r in &self.rings {
                if !r.active {
                    continue;
                }
                let front = r.age * self.ring_speed;
                let delta = (dot.dist - front).abs();
                if delta < half {
                    let falloff = 1.0 - delta / half; // triangular front profile
                    let fade = (1.0 - front / self.max_dist).clamp(0.0, 1.0);
                    ring_c += r.strength * falloff * fade;
                }
            }

            let intensity = (base + wave + ring_c).clamp(0.0, 1.0);
            let slot = slot_for(base, ring_c);
            // A size pulse tracks intensity so a lit dot reads as bigger, filling
            // more of the frame as the field brightens on the beat.
            let size = self.dot_size * (0.8 + 0.5 * intensity);
            canvas.point(dot.x, dot.y, size, Style::new(slot, intensity));
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("loud_env", self.loud_env);
        s.set("rms_env", self.rms_env);
        s.set("onset_env", self.onset_env);
        s.set("emit_acc", self.emit_acc);
        s.set("wave_phase", self.wave_phase);
        s.set("prev_onset", if self.prev_onset { 1.0 } else { 0.0 });
        s.set("prev_onset_age_ms", self.prev_onset_age_ms);
        for (i, r) in self.rings.iter().enumerate() {
            // An inactive slot is encoded as a negative age.
            let age = if r.active { r.age } else { -1.0 };
            s.set(&format!("ring{i}_age"), age);
            s.set(&format!("ring{i}_str"), r.strength);
        }
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
        if let Some(v) = s.get("emit_acc") {
            self.emit_acc = v;
        }
        if let Some(v) = s.get("wave_phase") {
            self.wave_phase = v;
        }
        if let Some(v) = s.get("prev_onset") {
            self.prev_onset = v >= 0.5;
        }
        if let Some(v) = s.get("prev_onset_age_ms") {
            self.prev_onset_age_ms = v;
        }
        for (i, r) in self.rings.iter_mut().enumerate() {
            let age = s.get(&format!("ring{i}_age"));
            let strength = s.get(&format!("ring{i}_str"));
            match (age, strength) {
                (Some(age), Some(strength)) if age >= 0.0 => {
                    *r = Ring {
                        age,
                        strength,
                        active: true,
                    };
                }
                _ => *r = Ring::DEAD,
            }
        }
    }
}

/// Refresh one tuning scalar from `params` in place. When `key` is present, the
/// value is stored clamped to that parameter's manifest `[min, max]`; when
/// absent, the slot keeps its current value. The clamp matters because a mapping
/// writes `offset + scale * env`, which can leave the range validated at preset
/// load. Allocation-free: a linear scan of the bag and the static manifest.
#[inline]
fn read_param(slot: &mut f32, params: &crate::scene::Params, key: &str) {
    if let Some(v) = params.get(key) {
        let spec = PARAMS
            .iter()
            .find(|s| s.key == key)
            .expect("key is a lattice parameter");
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

/// Pick a palette slot. The lattice stays in one cool family — dim dots are a
/// deep teal, lit dots (ring front passing, or a bright base glow) step up to
/// cyan — so the mean drawn colour barely travels as rings sweep the field and
/// the palette reads as calm rather than churning between warm and cool.
#[inline]
fn slot_for(base: f32, ring: f32) -> crate::Slot {
    if ring > 0.05 || base >= 0.15 { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::FeatureSnapshot;

    /// A snapshot carrying an onset flag, an `onset_age_ms`, a loudness and a bass
    /// band. The `loudness` argument is the engine-normalized level the scene
    /// drives from (mirrored into `rms` so the snapshot stays plausible).
    fn snap(onset: bool, onset_age_ms: f32, loudness: f32, bass: f32) -> FeatureSnapshot {
        let mut f = FeatureSnapshot {
            onset,
            onset_age_ms,
            rms: loudness,
            loudness,
            ..FeatureSnapshot::default()
        };
        f.bands = [bass, 1.0, 1.0];
        f
    }

    fn quiet() -> FeatureSnapshot {
        snap(false, 60_000.0, 0.0, 0.0)
    }

    fn render_points(scene: &mut Lattice) -> Vec<Primitive> {
        let mut c = Canvas::new(1.0);
        scene.render(&mut c);
        c.primitives().to_vec()
    }

    fn active_rings(scene: &Lattice) -> usize {
        scene.rings.iter().filter(|r| r.active).count()
    }

    fn inited(aspect: f32) -> Lattice {
        let mut s = Lattice::new();
        let ctx = SceneCtx {
            aspect,
            ..SceneCtx::default()
        };
        s.init(&ctx);
        s
    }

    #[test]
    fn onset_edge_spawns_exactly_one_ring() {
        let mut s = inited(1.0);
        assert_eq!(active_rings(&s), 0, "no rings before any onset");

        // A quiet frame leaves the pool empty.
        s.update(&quiet(), 0.05);
        assert_eq!(active_rings(&s), 0);

        // The rising edge of an onset spawns one ring.
        s.update(&snap(true, 0.0, 0.3, 1.0), 0.05);
        assert_eq!(active_rings(&s), 1, "one onset edge → one ring");

        // A repeated identical onset snapshot must not spawn another ring.
        s.update(&snap(true, 0.0, 0.3, 1.0), 0.05);
        assert_eq!(active_rings(&s), 1, "a held onset does not re-fire");

        // A fresh onset (age resets below the previous frame) spawns a second.
        s.update(&snap(false, 40.0, 0.3, 1.0), 0.05);
        s.update(&snap(true, 0.0, 0.3, 1.0), 0.05);
        assert_eq!(active_rings(&s), 2, "a new onset fires again");
    }

    #[test]
    fn render_changes_visibly_on_onset() {
        let mut control = inited(1.0);
        control.update(&quiet(), 0.05);
        let before = render_points(&mut control);

        let mut s = inited(1.0);
        s.update(&snap(true, 0.0, 0.3, 1.0), 0.05);
        // Let the front travel into the lattice so a band of dots is lit.
        s.update(&snap(false, 40.0, 0.3, 1.0), 0.05);
        let after = render_points(&mut s);

        assert_eq!(before.len(), after.len(), "same dot budget either way");
        assert_ne!(before, after, "the onset ring visibly changes the render");
    }

    #[test]
    fn dot_count_matches_density_and_stays_in_bounds() {
        // Square aspect: rows == cols == density.
        let mut s = inited(1.0);
        let cols = 24usize;
        let rows = 24usize;
        let prims = render_points(&mut s);
        assert_eq!(prims.len(), cols * rows, "one point per grid cell");
        for p in &prims {
            match p {
                Primitive::Point { x, y, size, .. } => {
                    assert!((0.0..=1.0).contains(x), "x in bounds: {x}");
                    assert!((0.0..=1.0).contains(y), "y in bounds: {y}");
                    assert!(*size >= 0.0 && *size <= 1.0, "size in bounds: {size}");
                }
                other => panic!("expected Point, got {other:?}"),
            }
        }

        // Wide aspect: rows follow so spacing stays square (rows == round(cols/aspect)).
        let mut wide = inited(2.0);
        let wide_prims = render_points(&mut wide);
        assert_eq!(
            wide_prims.len(),
            24 * 12,
            "rows scale down with a wide aspect"
        );
    }

    #[test]
    fn silence_settles_to_a_stable_steady_state() {
        let mut s = inited(1.0);
        // An onset, then a long stretch of silence: rings die and glow decays.
        s.update(&snap(true, 0.0, 0.4, 1.0), 0.05);
        for _ in 0..200 {
            s.update(&quiet(), 0.05);
        }
        assert_eq!(active_rings(&s), 0, "rings have all expired in silence");

        let a = render_points(&mut s);
        s.update(&quiet(), 0.05);
        let b = render_points(&mut s);

        // Two consecutive quiet frames render essentially identically.
        assert_eq!(a.len(), b.len());
        for (pa, pb) in a.iter().zip(&b) {
            match (pa, pb) {
                (
                    Primitive::Point {
                        x: xa,
                        y: ya,
                        size: sa,
                        style: st_a,
                    },
                    Primitive::Point {
                        x: xb,
                        y: yb,
                        size: sb,
                        style: st_b,
                    },
                ) => {
                    assert!((xa - xb).abs() < 1e-6 && (ya - yb).abs() < 1e-6);
                    assert!((sa - sb).abs() < 1e-4, "dot size stable in silence");
                    assert!(
                        (st_a.intensity - st_b.intensity).abs() < 1e-4,
                        "dot intensity stable in silence"
                    );
                }
                _ => panic!("expected points"),
            }
        }
    }

    #[test]
    fn state_restore_carries_continuity() {
        let s1 = snap(true, 0.0, 0.4, 1.2); // onset: spawns a ring, lifts glow
        let s2 = snap(false, 40.0, 0.4, 1.2); // next frame: ring travels, no onset

        // Reference: drive one frame, snapshot, then advance one more.
        let mut a = inited(1.0);
        a.update(&s1, 0.05);
        let state = a.state();
        a.update(&s2, 0.05);
        let prims_a = render_points(&mut a);

        // Restored: fresh scene, restore the snapshot, advance the same frame.
        let mut b = inited(1.0);
        b.restore(state);
        b.update(&s2, 0.05);
        let prims_b = render_points(&mut b);

        assert_eq!(
            prims_a, prims_b,
            "restore reproduces the next render exactly"
        );

        // Control: without the restore the ring and glow are lost, so it differs.
        let mut c = inited(1.0);
        c.update(&s2, 0.05);
        let prims_c = render_points(&mut c);
        assert_ne!(
            prims_a, prims_c,
            "a scene that skipped restore should not match"
        );
    }
}
