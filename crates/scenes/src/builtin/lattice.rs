//! `lattice` — a calm dot lattice made alive by the beat.
//!
//! A regular grid of dots fills the canvas at a visually square spacing. Every
//! detected onset fires a ring that propagates outward from the centre through
//! the lattice; each dot brightens as the ring front passes it and settles back
//! as the front moves on. Overall loudness sets a base glow shared by every dot,
//! so the whole field breathes with the music instead of strobing.
//!
//! There is no beat tracker yet (`beat_phase`/`tempo_bpm` are reserved zeros),
//! so the motion is driven entirely from the onset stream, the bass band and
//! the loudness of the mono mix. In silence the rings die out and the loudness
//! glow falls to a dim floor: the lattice settles to steady, calm dots with no
//! flicker.
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
//! loudness glow envelope and the onset-edge bookkeeping, so a hot reload does
//! not visibly reset an in-flight ripple or re-fire the current onset. The dot
//! grid itself is rebuilt deterministically from `density` and the aspect at
//! [`Scene::init`], so it is not part of the carried state.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Scene, SceneCtx, SceneState};

/// Maximum number of rings alive at once; the oldest is recycled when a new
/// onset arrives with the pool full.
const RING_CAP: usize = 8;

/// Fraction of the loudness glow that remains at silence, so dots never fully
/// vanish: the lattice settles to dim steady points rather than going dark.
const GLOW_FLOOR: f32 = 0.25;

/// Loudness-envelope smoothing time constant (seconds).
const LOUD_TAU: f32 = 0.15;

/// Dot diameter as a fraction of one cell's height. Kept well under `1.0` so
/// dots read as points with clear gaps between them.
const DOT_CELL_FRACTION: f32 = 0.4;

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
        self.density = params.get_or("density", 24.0);
        self.ring_speed = params.get_or("ring_speed", 0.9);
        self.ring_width = params.get_or("ring_width", 0.14);
        self.flash = params.get_or("flash", 0.7);
        self.glow = params.get_or("glow", 0.35);
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
        self.prev_onset = false;
        self.prev_onset_age_ms = 0.0;
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        // Loudness glow: momentary LUFS is reserved (0) in schema 1, so drive
        // the base glow from the mono RMS, smoothed toward its target.
        let loud = f.rms.clamp(0.0, 1.0);
        let k = 1.0 - decay(dt, LOUD_TAU);
        self.loud_env += (loud - self.loud_env) * k;

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

        // One onset spawns exactly one ring. Fire on a rising edge, or when a
        // fresh onset resets `onset_age_ms` below the previous frame's value —
        // so a repeated identical snapshot never double-spawns.
        let new_onset = f.onset && (!self.prev_onset || f.onset_age_ms < self.prev_onset_age_ms);
        if new_onset {
            // Louder bass makes a brighter ring, but it never falls to nothing.
            let bass01 = (f.bands[0] * 0.5).clamp(0.0, 1.0);
            let strength = self.flash * (0.5 + 0.5 * bass01);
            self.spawn_ring(strength);
        }
        self.prev_onset = f.onset;
        self.prev_onset_age_ms = f.onset_age_ms;
    }

    fn render(&mut self, canvas: &mut Canvas) {
        let base = self.glow * (GLOW_FLOOR + (1.0 - GLOW_FLOOR) * self.loud_env);
        let half = (self.ring_width * 0.5).max(1e-6);

        for dot in &self.dots {
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

            let intensity = (base + ring_c).clamp(0.0, 1.0);
            let slot = slot_for(base, ring_c);
            // A gentle size pulse tracks intensity so a lit dot reads as bigger.
            let size = self.dot_size * (0.85 + 0.3 * intensity);
            canvas.point(dot.x, dot.y, size, Style::new(slot, intensity));
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("loud_env", self.loud_env);
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

/// Pick a palette slot: dots the ring is lighting go warm (amber, then coral),
/// otherwise a cool teal→cyan by base glow.
#[inline]
fn slot_for(base: f32, ring: f32) -> crate::Slot {
    if ring > 0.05 {
        if ring < 0.4 { 3 } else { 4 }
    } else if base < 0.15 {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Primitive;
    use scia_core::FeatureSnapshot;

    /// A snapshot carrying an onset flag, an `onset_age_ms`, an rms and a bass band.
    fn snap(onset: bool, onset_age_ms: f32, rms: f32, bass: f32) -> FeatureSnapshot {
        let mut f = FeatureSnapshot {
            onset,
            onset_age_ms,
            rms,
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
