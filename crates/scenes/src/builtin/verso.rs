//! `verso` — the minimal typographic scene: the track title *is* the analyzer.
//!
//! The now-playing line is laid out as text on a baseline, and each letter rides
//! its own slice of the spectrum: the letter floats up as its band swells and
//! brightens with it, then settles. As a letter moves it sheds a sparse, dotted
//! trail that falls away beneath it and fades — the visual echo of the band's
//! recent motion. Everything else is empty space. The result is deliberately
//! quiet and literal: you read the title, and the title dances.
//!
//! # Tier-proof by construction
//!
//! Each letter is a single [`crate::canvas::Primitive::Text`] run, which the
//! presenter paints as a real terminal glyph on top of the mosaic — so the
//! letters are pin-sharp at every tier, coarse or fine, with no shading to lose.
//! The trails are ordinary [`crate::canvas::Primitive::Point`]s and are kept
//! sparse, so they read as scattered dots rather than mush even at the half-block
//! tier.
//!
//! # Text and bands
//!
//! The host pushes the current track line through [`Scene::apply_text`] whenever
//! it changes (never per frame). On each change the letter list is rebuilt: the
//! characters are laid out across the width (spaces leave gaps but draw nothing),
//! and every **non-space** letter is assigned an evenly distributed position in
//! the spectrum. When the track line is empty or absent the scene falls back to
//! the word [`FALLBACK`]. Because a letter's band is stored as a fraction of the
//! spectrum, it resolves against whatever [`spectrum_len`] the analyzer publishes
//! at runtime.
//!
//! [`spectrum_len`]: scia_core::FeatureSnapshot::spectrum_len
//!
//! # Per frame
//!
//! Each letter reads its band, eases a smoothed value toward it, and from that
//! value takes a vertical offset above the baseline and a brightness. Every so
//! often (a fixed cadence, not every frame) each live letter drops a trail mark
//! at its current position into a **pre-allocated ring**; the marks fall and fade
//! on their own. Rendering pushes one text primitive per letter and one point per
//! live trail mark onto the canvas, which retains its capacity — so a warmed
//! scene does no per-frame allocation. The only allocation is the letter-list
//! rebuild inside [`Scene::apply_text`], which is off the frame path.
//!
//! # Parameters
//!
//! | key        | default | range        | meaning                                                     |
//! |------------|---------|--------------|-------------------------------------------------------------|
//! | `baseline` | `0.58`  | `0.2..=0.85` | resting vertical position of the letters (fraction of height)|
//! | `lift`     | `0.34`  | `0.0..=0.6`  | how far a letter rises above the baseline at full band       |
//! | `trail`    | `1.0`   | `0.0..=2.0`  | trail lifetime multiplier (`0` disables the trail)           |
//! | `fall`     | `0.25`  | `0.0..=1.0`  | how fast a trail mark falls (canvas heights / second)        |
//!
//! All four are live tuning scalars: the host re-applies them every frame through
//! [`Scene::apply_params`], each clamped to its manifest range on read. The
//! letter list, the smoothed per-letter values and the trail are animation state
//! and are never touched by a re-apply.
//!
//! # Continuity
//!
//! [`Scene::state`] carries the smoothed per-letter values, the trail-spawn phase
//! and the live trail marks, so a hot reload resumes the letters' motion and the
//! falling dots rather than snapping them cold. The text itself cannot ride
//! [`SceneState`] (it holds only scalars), so after a reload the scene shows the
//! fallback word until the host re-applies the track line on its next change.

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};

/// The word shown when there is no track line.
const FALLBACK: &str = "scia";

/// Horizontal margin (fraction of width) left clear on each side of the line.
const MARGIN: f32 = 0.06;

/// Per-letter value follower time constant (seconds): quick enough to dance,
/// smooth enough not to strobe.
const VAL_TAU: f32 = 0.09;

/// Seconds between trail-mark drops. A drop emits one mark per live letter, so
/// the cadence — not a per-frame spray — keeps the trail dotted.
const SPAWN_INTERVAL: f32 = 0.11;

/// A letter must exceed this smoothed value to shed a trail mark, so quiet bands
/// leave the space beneath them clear.
const SPAWN_THRESHOLD: f32 = 0.08;

/// Base trail-mark lifetime in seconds, before the `trail` multiplier.
const TRAIL_LIFE: f32 = 0.9;

/// Trail-ring capacity. Sized to hold every letter's marks over a full lifetime
/// with headroom; a burst that would exceed it simply overwrites the oldest.
const MAX_MARKS: usize = 1024;

/// Trail marks carried across a hot reload (the most recent, capped so the cold
/// restore path stays bounded).
const MAX_CARRY: usize = 128;

/// Dot diameter of a trail mark (fraction of canvas height).
const DOT_SIZE: f32 = 0.012;

/// Brightest a trail mark starts at (it fades from here to nothing).
const TRAIL_MAX_INT: f32 = 0.55;

/// Dimmest a letter ever is, so the title stays readable even on a dead band.
const LETTER_FLOOR: f32 = 0.35;

/// Palette slot for the letters (a bright neutral in the default palette).
const TEXT_SLOT: crate::Slot = 7;
/// Palette slot for the trail dots (a dim neutral in the default palette).
const TRAIL_SLOT: crate::Slot = 6;

/// `verso`'s parameter manifest: the keys a preset may set, with the defaults,
/// ranges and docs from the module table above.
pub static PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "baseline",
        default: 0.58,
        min: 0.2,
        max: 0.85,
        doc: "resting vertical position of the letters (fraction of height)",
    },
    ParamSpec {
        key: "lift",
        default: 0.34,
        min: 0.0,
        max: 0.6,
        doc: "how far a letter rises above the baseline at full band",
    },
    ParamSpec {
        key: "trail",
        default: 1.0,
        min: 0.0,
        max: 2.0,
        doc: "trail lifetime multiplier (0 disables the trail)",
    },
    ParamSpec {
        key: "fall",
        default: 0.25,
        min: 0.0,
        max: 1.0,
        doc: "how fast a trail mark falls (canvas heights / second)",
    },
];

/// One rendered letter: its glyph, its baseline x position and the spectrum
/// fraction it rides.
#[derive(Clone, Copy, Debug)]
struct Letter {
    /// The glyph to draw.
    ch: char,
    /// Baseline x position (fraction of width), fixed at rebuild.
    x: f32,
    /// Position in the spectrum this letter reads, as a fraction in `0.0..1.0`;
    /// resolved to a bin against the live `spectrum_len` each frame.
    band_frac: f32,
}

/// One trail mark in the ring. `life` counts down from `1.0`; `life <= 0` means
/// the slot is free.
#[derive(Clone, Copy, Debug, Default)]
struct Mark {
    /// X position (fraction of width).
    x: f32,
    /// Y position (fraction of height), grows as it falls.
    y: f32,
    /// Remaining life in `0.0..=1.0`.
    life: f32,
}

/// The minimal typographic scene.
#[derive(Clone, Debug)]
pub struct Verso {
    /// The current text (the fallback until the host applies a track line).
    text: String,
    /// The laid-out non-space letters.
    letters: Vec<Letter>,
    /// Smoothed per-letter band value, parallel to `letters`.
    vals: Vec<f32>,
    /// Pre-allocated trail ring; slots with `life <= 0` are free.
    trail: Vec<Mark>,
    /// Round-robin write cursor into `trail`.
    cursor: usize,
    /// Time accumulated toward the next trail-mark drop.
    spawn_accum: f32,

    // --- parameters ----------------------------------------------------
    baseline: f32,
    lift: f32,
    trail_mult: f32,
    fall: f32,
}

impl Verso {
    /// A `verso` scene with default parameters showing the fallback word. Call
    /// [`Scene::init`] before driving it.
    #[must_use]
    pub fn new() -> Self {
        let mut s = Self {
            text: String::new(),
            letters: Vec::new(),
            vals: Vec::new(),
            trail: vec![Mark::default(); MAX_MARKS],
            cursor: 0,
            spawn_accum: 0.0,
            baseline: 0.58,
            lift: 0.34,
            trail_mult: 1.0,
            fall: 0.25,
        };
        s.rebuild(FALLBACK);
        s
    }

    /// Consume the preset parameters. Kept as the single point of parameter
    /// consumption so [`Scene::apply_params`] can reuse it verbatim.
    fn read_params(&mut self, params: &Params) {
        read_param(&mut self.baseline, params, "baseline");
        read_param(&mut self.lift, params, "lift");
        read_param(&mut self.trail_mult, params, "trail");
        read_param(&mut self.fall, params, "fall");
    }

    /// Rebuild the letter list from `text`, laying the glyphs across the width
    /// and assigning every non-space letter an evenly distributed spectrum band.
    /// Falls back to [`FALLBACK`] when `text` has no visible characters. This
    /// allocates, but it runs only on a text change, never per frame.
    fn rebuild(&mut self, text: &str) {
        let chosen = if text.chars().any(|c| !c.is_whitespace()) {
            text
        } else {
            FALLBACK
        };
        self.text = chosen.to_string();
        self.letters.clear();

        let total = self.text.chars().count().max(1);
        let non_space = self.text.chars().filter(|c| !c.is_whitespace()).count();
        let span = 1.0 - 2.0 * MARGIN;
        let mut j = 0usize;
        for (i, ch) in self.text.chars().enumerate() {
            if ch.is_whitespace() {
                continue;
            }
            let x = MARGIN + span * (i as f32 + 0.5) / total as f32;
            let band_frac = if non_space > 0 {
                (j as f32 + 0.5) / non_space as f32
            } else {
                0.0
            };
            self.letters.push(Letter { ch, x, band_frac });
            j += 1;
        }

        self.vals.clear();
        self.vals.resize(self.letters.len(), 0.0);
    }

    /// Write a fresh mark into the ring, overwriting the oldest slot.
    #[inline]
    fn emit(&mut self, x: f32, y: f32) {
        let n = self.trail.len();
        if n == 0 {
            return;
        }
        self.trail[self.cursor] = Mark { x, y, life: 1.0 };
        self.cursor = (self.cursor + 1) % n;
    }
}

impl Default for Verso {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene for Verso {
    fn id(&self) -> &'static str {
        "verso"
    }

    fn mood(&self) -> &'static str {
        "literal"
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.read_params(&ctx.params);
        for m in &mut self.trail {
            *m = Mark::default();
        }
        self.cursor = 0;
        self.spawn_accum = 0.0;
        for v in &mut self.vals {
            *v = 0.0;
        }
    }

    fn apply_params(&mut self, params: &Params) {
        // Tuning scalars only: the letters, their smoothed values and the trail
        // carry across, so a live mapping never resets the animation.
        self.read_params(params);
    }

    fn apply_text(&mut self, key: &str, value: &str) {
        // Only the track line drives the letters today; ignore other keys.
        if key != "track" {
            return;
        }
        self.rebuild(value);
    }

    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Resolve the live spectrum length once; fall back to the full array when
        // the analyzer has not published a length yet (e.g. a default snapshot).
        let len = if f.spectrum_len > 0 {
            (f.spectrum_len as usize).min(f.spectrum.len())
        } else {
            f.spectrum.len()
        };

        // Ease every letter's smoothed value toward its band.
        let coeff = follow_coeff(dt, VAL_TAU);
        for (letter, val) in self.letters.iter().zip(self.vals.iter_mut()) {
            let bin = ((letter.band_frac * len as f32) as usize).min(len.saturating_sub(1));
            let target = f.spectrum.get(bin).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            *val += (target - *val) * coeff;
        }

        // Age the trail: every live mark falls and fades; a mark that leaves the
        // canvas or runs out of life frees its slot.
        let life_tau = (TRAIL_LIFE * self.trail_mult).max(1e-3);
        for m in &mut self.trail {
            if m.life > 0.0 {
                m.y += self.fall * dt;
                m.life -= dt / life_tau;
                if m.y > 1.0 {
                    m.life = 0.0;
                }
            }
        }

        // Drop a fresh dotted mark under every live letter on the spawn cadence.
        // Guarded so `trail = 0` (or no letters) sheds nothing.
        self.spawn_accum += dt;
        if self.trail_mult > 0.0 && self.spawn_accum >= SPAWN_INTERVAL {
            self.spawn_accum -= SPAWN_INTERVAL;
            // Snapshot the (x, y) drops first so the borrow of `self.vals`/
            // `self.letters` does not overlap the `emit` mutation of `self.trail`.
            for i in 0..self.letters.len() {
                let val = self.vals[i];
                if val > SPAWN_THRESHOLD {
                    let letter = self.letters[i];
                    let y = letter_y(self.baseline, self.lift, val);
                    self.emit(letter.x, y);
                }
            }
        }
        // Never let the accumulator run away if dt is huge or the cadence is off.
        if self.spawn_accum > SPAWN_INTERVAL {
            self.spawn_accum = self.spawn_accum.rem_euclid(SPAWN_INTERVAL);
        }
    }

    fn render(&mut self, canvas: &mut Canvas) {
        // Trails first (beneath), then the letters. Terminal text always paints
        // over the mosaic, so the letters read on top regardless.
        for m in &self.trail {
            if m.life > 0.0 {
                let intensity = TRAIL_MAX_INT * m.life;
                canvas.point(m.x, m.y, DOT_SIZE, Style::new(TRAIL_SLOT, intensity));
            }
        }

        let mut buf = [0u8; 4];
        for (letter, val) in self.letters.iter().zip(self.vals.iter()) {
            let y = letter_y(self.baseline, self.lift, *val);
            let intensity = (LETTER_FLOOR + (1.0 - LETTER_FLOOR) * *val).clamp(0.0, 1.0);
            let s = letter.ch.encode_utf8(&mut buf);
            canvas.text(letter.x, y, s, Style::new(TEXT_SLOT, intensity));
        }
    }

    fn state(&self) -> SceneState {
        let mut s = SceneState::new();
        s.set("spawn", self.spawn_accum);
        for (i, v) in self.vals.iter().enumerate() {
            s.set(&format!("v{i}"), *v);
        }
        // Carry the live trail marks (most-recent first), capped so this cold
        // path stays bounded. The count leads so `restore` knows how many follow.
        let live: Vec<&Mark> = self.trail.iter().filter(|m| m.life > 0.0).collect();
        let carried = live.len().min(MAX_CARRY);
        s.set("tn", carried as f32);
        for (k, m) in live.iter().rev().take(carried).enumerate() {
            s.set(&format!("tx{k}"), m.x);
            s.set(&format!("ty{k}"), m.y);
            s.set(&format!("tl{k}"), m.life);
        }
        s
    }

    fn restore(&mut self, s: SceneState) {
        if let Some(v) = s.get("spawn") {
            self.spawn_accum = v;
        }
        for (i, v) in self.vals.iter_mut().enumerate() {
            if let Some(x) = s.get(&format!("v{i}")) {
                *v = x;
            }
        }
        // Repopulate the trail ring from the carried marks.
        for m in &mut self.trail {
            *m = Mark::default();
        }
        self.cursor = 0;
        if let Some(n) = s.get("tn") {
            let n = (n.max(0.0) as usize).min(MAX_CARRY).min(self.trail.len());
            for k in 0..n {
                let (Some(x), Some(y), Some(life)) = (
                    s.get(&format!("tx{k}")),
                    s.get(&format!("ty{k}")),
                    s.get(&format!("tl{k}")),
                ) else {
                    break;
                };
                self.emit(x, y);
                // `emit` set life to 1.0; restore the carried life.
                let slot = (self.cursor + self.trail.len() - 1) % self.trail.len();
                self.trail[slot].life = life;
            }
        }
    }
}

/// A letter's vertical position: it rises above the baseline as its value grows.
/// Canvas `y` grows downward, so a larger value subtracts more.
#[inline]
fn letter_y(baseline: f32, lift: f32, val: f32) -> f32 {
    (baseline - lift * val.clamp(0.0, 1.0)).clamp(0.0, 1.0)
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

/// Refresh one tuning scalar from `params` in place. When `key` is present, the
/// value is stored clamped to that parameter's manifest `[min, max]`; when
/// absent, the slot keeps its current value. Allocation-free: a linear scan of
/// the bag and the static manifest.
#[inline]
fn read_param(slot: &mut f32, params: &Params, key: &str) {
    if let Some(v) = params.get(key) {
        let spec = PARAMS
            .iter()
            .find(|s| s.key == key)
            .expect("key is a verso parameter");
        *slot = v.clamp(spec.min, spec.max);
    }
}
