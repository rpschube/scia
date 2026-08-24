//! Album-art palette extraction and caching.
//!
//! This module turns raw encoded artwork bytes (JPEG or PNG) into an
//! [`ArtPalette`]: a dominant colour, a handful of distinct accents, light and
//! dark anchor variants, and an eight-slot layout that mirrors the host
//! palette convention used by scenes. It is a self-contained input source — it
//! depends only on `scia-core` and the `image` decoder and knows nothing about
//! any now-playing backend.
//!
//! # Off the render path
//!
//! [`extract`] is a synchronous, pure function: same bytes in, same palette
//! out, no threads, no I/O beyond decoding the buffer it is handed. Decoding
//! and k-means on a downscaled image are cheap (well under 200 ms for typical
//! artwork), but they are **not** free, and they must never run on a frame
//! render. Callers are responsible for keeping extraction off the render path
//! — the wave-2 TUI calls it from a worker thread and hands scenes the
//! finished palette. This module deliberately does not spawn threads so that
//! the scheduling policy lives with the caller.
//!
//! # Pipeline
//!
//! `decode → crop uniform near-black padding → downscale to ~64×64 → k-means
//! (k=8) in Oklab with deterministic k-means++ seeding → order clusters by
//! population → derive dominant / accents / light / dark / slots`.
//!
//! The Oklab conversion follows Björn Ottosson, "A perceptual color space for
//! image processing" (<https://bottosson.github.io/posts/oklab/>); the matrix
//! coefficients below are taken verbatim from that reference. Working in Oklab
//! makes "distinct hue" and "lighter / darker" mean what the eye expects
//! rather than what raw sRGB arithmetic produces.
//!
//! # Spotify / SMTC quirk absorbed here
//!
//! Windows `SystemMediaTransportControls` thumbnails routinely letterbox or
//! pillarbox the real cover in black or near-black bars. We detect uniform,
//! near-black border bands (whole rows or columns that are near-constant and
//! near-black) and crop them off before sampling — unconditionally, because
//! the crop is a no-op on art that has no such bars. (The Linux `mpris:artUrl`
//! rewrite is a transport concern and lives in the MPRIS backend, not here.)

use std::collections::HashMap;

use image::imageops::FilterType;

/// An extracted album-art palette, exposed as a first-class colour source.
///
/// All colours are 24-bit sRGB `[r, g, b]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtPalette {
    /// The most populous colour in the artwork — the visual "key" colour.
    pub dominant: [u8; 3],
    /// Further colours that are perceptually distinct from the dominant and
    /// from each other, ordered by population. May be empty (e.g. monochrome
    /// art). At most [`MAX_ACCENTS`] entries.
    pub accents: Vec<[u8; 3]>,
    /// A lighter variant of the dominant, suitable as a background or a light
    /// foreground anchor. Guaranteed no darker than [`dominant`].
    pub light: [u8; 3],
    /// A darker variant of the dominant, suitable as a background or a dark
    /// foreground anchor. Guaranteed no lighter than [`dominant`].
    pub dark: [u8; 3],
    /// Eight colour slots laid out to match the host palette convention (see
    /// module docs and [`ArtPalette::slots`] rationale). Index-compatible with
    /// the scenes palette so a scene re-themes without change.
    pub slots: [[u8; 3]; 8],
}

/// Why extraction failed. Only genuinely undecodable input is an error; tiny
/// and monochrome images degrade to a valid palette rather than failing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteError {
    /// The bytes could not be decoded as a supported image format.
    Decode(String),
    /// The image decoded but has zero pixels.
    Empty,
}

impl std::fmt::Display for PaletteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaletteError::Decode(m) => write!(f, "could not decode artwork: {m}"),
            PaletteError::Empty => write!(f, "artwork has zero pixels"),
        }
    }
}

impl std::error::Error for PaletteError {}

/// Number of k-means clusters.
const K: usize = 8;
/// Target longest edge after downscaling, in pixels.
const SAMPLE_EDGE: u32 = 64;
/// A pixel is "near-black" for padding detection if every channel is at or
/// below this value.
const NEAR_BLACK: u8 = 24;
/// A border band must be near-constant to this tolerance (per-channel spread
/// across the band) to count as padding.
const BAND_SPREAD: u8 = 20;
/// Minimum Oklab distance for an accent to count as distinct from the dominant
/// and from other accents.
const ACCENT_MIN_DIST: f32 = 0.12;
/// Maximum number of accents reported.
pub const MAX_ACCENTS: usize = 4;
/// Maximum Lloyd iterations; k-means on a downscaled image converges quickly.
const MAX_ITERS: usize = 24;

/// Extract a palette from raw encoded artwork bytes.
///
/// Pure and synchronous — see the module docs for the off-render-path
/// contract. Returns [`PaletteError::Decode`] only when `bytes` is not a
/// decodable image; tiny and monochrome images yield a valid palette.
///
/// # Errors
///
/// [`PaletteError::Decode`] if the bytes are not a supported/valid image,
/// [`PaletteError::Empty`] if the decoded image has no pixels.
pub fn extract(bytes: &[u8]) -> Result<ArtPalette, PaletteError> {
    let img = image::load_from_memory(bytes).map_err(|e| PaletteError::Decode(e.to_string()))?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return Err(PaletteError::Empty);
    }

    // Crop uniform near-black padding (SMTC letterbox/pillarbox bars).
    let (x0, y0, x1, y1) = padding_crop(&rgb);
    let cropped = image::imageops::crop_imm(&rgb, x0, y0, x1 - x0, y1 - y0).to_image();

    // Downscale so the longest edge is ~SAMPLE_EDGE; k-means samples this.
    let (cw, ch) = cropped.dimensions();
    let small = if cw.max(ch) > SAMPLE_EDGE {
        let scale = f64::from(SAMPLE_EDGE) / f64::from(cw.max(ch));
        let nw = ((f64::from(cw) * scale).round() as u32).max(1);
        let nh = ((f64::from(ch) * scale).round() as u32).max(1);
        // Nearest is intentional: k-means only needs representative samples,
        // and nearest downscaling is markedly cheaper than a filtered one,
        // keeping extraction comfortably inside the render-budget guidance.
        image::imageops::resize(&cropped, nw, nh, FilterType::Nearest)
    } else {
        cropped
    };

    // Collect Oklab samples.
    let samples: Vec<Oklab> = small
        .pixels()
        .map(|p| Oklab::from_srgb(p.0[0], p.0[1], p.0[2]))
        .collect();

    let clusters = kmeans(&samples);
    Ok(build_palette(&clusters))
}

/// A cluster: its mean colour and how many pixels it holds.
struct Cluster {
    mean: Oklab,
    count: usize,
}

/// Deterministic k-means (k-means++ seeding, fixed seed). Returns non-empty
/// clusters ordered by population, most populous first. Always returns at least
/// one cluster for a non-empty sample set.
fn kmeans(samples: &[Oklab]) -> Vec<Cluster> {
    debug_assert!(!samples.is_empty());
    let mut rng = SplitMix64::new(0x5c1a_9e3d_1b2c_4f77);

    // k-means++ seeding.
    let mut centers: Vec<Oklab> = Vec::with_capacity(K);
    centers.push(samples[rng.next_bounded(samples.len() as u64) as usize]);
    while centers.len() < K.min(samples.len()) {
        // D^2 weighting: distance to the nearest chosen center.
        let d2: Vec<f32> = samples
            .iter()
            .map(|s| {
                centers
                    .iter()
                    .map(|c| s.dist2(c))
                    .fold(f32::INFINITY, f32::min)
            })
            .collect();
        let total: f64 = d2.iter().map(|&x| f64::from(x)).sum();
        if total <= f64::EPSILON {
            // All remaining points coincide with a center (e.g. near-mono
            // image). Pick an arbitrary distinct index deterministically.
            let idx = rng.next_bounded(samples.len() as u64) as usize;
            centers.push(samples[idx]);
            continue;
        }
        let mut target = rng.next_f64() * total;
        let mut chosen = samples.len() - 1;
        for (i, &d) in d2.iter().enumerate() {
            target -= f64::from(d);
            if target <= 0.0 {
                chosen = i;
                break;
            }
        }
        centers.push(samples[chosen]);
    }

    // Lloyd iterations.
    let k = centers.len();
    let mut assign = vec![0usize; samples.len()];
    for _ in 0..MAX_ITERS {
        let mut changed = false;
        for (i, s) in samples.iter().enumerate() {
            let mut best = 0;
            let mut best_d = f32::INFINITY;
            for (c, center) in centers.iter().enumerate() {
                let d = s.dist2(center);
                if d < best_d {
                    best_d = d;
                    best = c;
                }
            }
            if assign[i] != best {
                assign[i] = best;
                changed = true;
            }
        }
        // Recompute means.
        let mut sums = vec![(0f32, 0f32, 0f32, 0usize); k];
        for (i, s) in samples.iter().enumerate() {
            let e = &mut sums[assign[i]];
            e.0 += s.l;
            e.1 += s.a;
            e.2 += s.b;
            e.3 += 1;
        }
        for (c, e) in sums.iter().enumerate() {
            if e.3 > 0 {
                let n = e.3 as f32;
                centers[c] = Oklab {
                    l: e.0 / n,
                    a: e.1 / n,
                    b: e.2 / n,
                };
            }
        }
        if !changed {
            break;
        }
    }

    // Final counts, drop empty clusters, order by population.
    let mut counts = vec![0usize; k];
    for &a in &assign {
        counts[a] += 1;
    }
    let mut clusters: Vec<Cluster> = centers
        .into_iter()
        .zip(counts)
        .filter(|(_, c)| *c > 0)
        .map(|(mean, count)| Cluster { mean, count })
        .collect();
    clusters.sort_by_key(|c| std::cmp::Reverse(c.count));
    clusters
}

/// Derive the reported palette from population-ordered clusters.
fn build_palette(clusters: &[Cluster]) -> ArtPalette {
    // `kmeans` never returns an empty vec for a non-empty image, but stay total.
    let dom_lab = clusters.first().map_or(Oklab::GREY, |c| c.mean);
    let dominant = dom_lab.to_srgb();

    // Accents: perceptually distinct from the dominant and from each other.
    let mut accents: Vec<[u8; 3]> = Vec::new();
    let mut accent_labs: Vec<Oklab> = Vec::new();
    for c in clusters.iter().skip(1) {
        if accents.len() >= MAX_ACCENTS {
            break;
        }
        if c.mean.dist2(&dom_lab) < ACCENT_MIN_DIST * ACCENT_MIN_DIST {
            continue;
        }
        if accent_labs
            .iter()
            .any(|a| c.mean.dist2(a) < ACCENT_MIN_DIST * ACCENT_MIN_DIST)
        {
            continue;
        }
        accents.push(c.mean.to_srgb());
        accent_labs.push(c.mean);
    }

    // Light / dark variants: interpolate the dominant's lightness toward white
    // and black in Oklab, keeping chroma. The interpolation guarantees the
    // light variant is no darker and the dark variant no lighter than the
    // dominant (strictly so for any non-extreme dominant).
    let light = Oklab {
        l: dom_lab.l + (1.0 - dom_lab.l) * 0.45,
        ..dom_lab
    }
    .to_srgb();
    let dark = Oklab {
        l: dom_lab.l * 0.55,
        ..dom_lab
    }
    .to_srgb();

    let slots = build_slots(dom_lab, &accent_labs, light, dark);

    ArtPalette {
        dominant,
        accents,
        light,
        dark,
        slots,
    }
}

/// Lay out the eight slots to match the host palette convention.
///
/// The scenes default palette (see `crates/scenes/src/palette.rs`) is:
/// `0` deep/darkest gradient anchor, `1` mid, `2` bright, `3` warm accent,
/// `4` warm accent 2, `5` near-black neutral, `6` mid neutral, `7` near-white
/// neutral. Scenes address these by index (`slot_for` in the built-ins picks
/// `1`/`2` for cool body colour, `3`/`4` for warm highlights), so an art
/// palette must mirror that ordering exactly to re-theme those scenes without
/// touching them. We map:
///
/// - `0` = dark variant of the dominant (bg-ish dark anchor)
/// - `1` = dominant (the cool body colour scenes reach for at slot 1)
/// - `2` = light variant of the dominant (bright body colour, slot 2)
/// - `3` = first accent (warm highlight); falls back to the dominant
/// - `4` = second accent (warning-ish highlight); falls back to first accent,
///   then dominant
/// - `5` = near-black neutral, faintly tinted toward the dominant hue
/// - `6` = mid neutral, faintly tinted
/// - `7` = near-white neutral, faintly tinted
///
/// Neutrals carry a trace of the dominant's chroma (15%) so the whole palette
/// reads as one family rather than dominant-plus-greys.
fn build_slots(dom: Oklab, accent_labs: &[Oklab], light: [u8; 3], dark: [u8; 3]) -> [[u8; 3]; 8] {
    let a0 = accent_labs.first().map_or(dom, |a| *a).to_srgb();
    let a1 = accent_labs
        .get(1)
        .or_else(|| accent_labs.first())
        .map_or(dom, |a| *a)
        .to_srgb();

    // Neutrals share the dominant hue at reduced chroma and fixed lightness.
    let neutral = |l: f32| {
        Oklab {
            l,
            a: dom.a * 0.15,
            b: dom.b * 0.15,
        }
        .to_srgb()
    };

    [
        dark,          // 0 bg-ish dark
        dom.to_srgb(), // 1 mid body
        light,         // 2 bright body
        a0,            // 3 warm accent
        a1,            // 4 warning-ish accent
        neutral(0.12), // 5 near-black neutral
        neutral(0.55), // 6 mid neutral
        neutral(0.93), // 7 near-white neutral
    ]
}

/// Find the crop rectangle after stripping uniform near-black border bands.
/// Returns `(x0, y0, x1, y1)` as a half-open rectangle; falls back to the full
/// image if cropping would leave nothing.
fn padding_crop(img: &image::RgbImage) -> (u32, u32, u32, u32) {
    let (w, h) = img.dimensions();

    let mut top = 0;
    while top < h && row_is_padding(img, top) {
        top += 1;
    }
    if top == h {
        return (0, 0, w, h); // whole image is near-black; keep it as-is
    }
    let mut bottom = h;
    while bottom > top + 1 && row_is_padding(img, bottom - 1) {
        bottom -= 1;
    }
    let mut left = 0;
    while left < w && col_is_padding(img, left, top, bottom) {
        left += 1;
    }
    let mut right = w;
    while right > left + 1 && col_is_padding(img, right - 1, top, bottom) {
        right -= 1;
    }
    if left >= right || top >= bottom {
        return (0, 0, w, h);
    }
    (left, top, right, bottom)
}

/// True if every pixel in row `y` is near-black and the row is near-constant.
fn row_is_padding(img: &image::RgbImage, y: u32) -> bool {
    let (w, _) = img.dimensions();
    band_is_padding((0..w).map(|x| img.get_pixel(x, y).0))
}

/// True if column `x` (within `[y0, y1)`) is near-black and near-constant.
fn col_is_padding(img: &image::RgbImage, x: u32, y0: u32, y1: u32) -> bool {
    band_is_padding((y0..y1).map(|y| img.get_pixel(x, y).0))
}

/// A band (row or column) is padding when every pixel is near-black and the
/// per-channel spread across the band stays within [`BAND_SPREAD`].
fn band_is_padding(px: impl Iterator<Item = [u8; 3]>) -> bool {
    let mut lo = [255u8; 3];
    let mut hi = [0u8; 3];
    for p in px {
        for c in 0..3 {
            if p[c] > NEAR_BLACK {
                return false;
            }
            lo[c] = lo[c].min(p[c]);
            hi[c] = hi[c].max(p[c]);
        }
    }
    (0..3).all(|c| hi[c] - lo[c] <= BAND_SPREAD)
}

// ---------------------------------------------------------------------------
// Oklab colour space (Ottosson, https://bottosson.github.io/posts/oklab/).
// ---------------------------------------------------------------------------

/// A colour in Oklab: `l` lightness, `a`/`b` opponent chroma axes.
#[derive(Clone, Copy, Debug)]
struct Oklab {
    l: f32,
    a: f32,
    b: f32,
}

impl Oklab {
    /// A mid grey, used only as a total-function fallback.
    const GREY: Oklab = Oklab {
        l: 0.6,
        a: 0.0,
        b: 0.0,
    };

    /// Convert an sRGB byte triple to Oklab.
    fn from_srgb(r: u8, g: u8, b: u8) -> Oklab {
        let lr = srgb_to_linear(f32::from(r) / 255.0);
        let lg = srgb_to_linear(f32::from(g) / 255.0);
        let lb = srgb_to_linear(f32::from(b) / 255.0);

        let l = 0.412_221_47 * lr + 0.536_332_55 * lg + 0.051_445_995 * lb;
        let m = 0.211_903_5 * lr + 0.680_699_5 * lg + 0.107_396_96 * lb;
        let s = 0.088_302_46 * lr + 0.281_718_85 * lg + 0.629_978_7 * lb;

        let l_ = l.cbrt();
        let m_ = m.cbrt();
        let s_ = s.cbrt();

        Oklab {
            l: 0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.004_072_047 * s_,
            a: 1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_,
            b: 0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_,
        }
    }

    /// Convert Oklab back to an sRGB byte triple (clamped into gamut).
    fn to_srgb(self) -> [u8; 3] {
        let l_ = self.l + 0.396_337_78 * self.a + 0.215_803_76 * self.b;
        let m_ = self.l - 0.105_561_346 * self.a - 0.063_854_17 * self.b;
        let s_ = self.l - 0.089_484_18 * self.a - 1.291_485_5 * self.b;

        let l = l_ * l_ * l_;
        let m = m_ * m_ * m_;
        let s = s_ * s_ * s_;

        let r = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
        let g = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s;
        let b = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;

        [
            (linear_to_srgb(r) * 255.0).round().clamp(0.0, 255.0) as u8,
            (linear_to_srgb(g) * 255.0).round().clamp(0.0, 255.0) as u8,
            (linear_to_srgb(b) * 255.0).round().clamp(0.0, 255.0) as u8,
        ]
    }

    /// Squared Euclidean distance in Oklab (a fine perceptual metric here).
    fn dist2(&self, other: &Oklab) -> f32 {
        let dl = self.l - other.l;
        let da = self.a - other.a;
        let db = self.b - other.b;
        dl * dl + da * da + db * db
    }
}

/// sRGB transfer function, inverse (companded → linear). Input/output in `0..1`.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB transfer function (linear → companded). Input/output in `0..1`.
fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

// ---------------------------------------------------------------------------
// Deterministic RNG: SplitMix64. Fixed-seeded so the same bytes always produce
// the same palette without pulling in an RNG dependency.
// ---------------------------------------------------------------------------

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform `f64` in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        // Top 53 bits → [0, 1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A uniform integer in `[0, bound)`; `bound` must be non-zero.
    fn next_bounded(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

// ---------------------------------------------------------------------------
// Bounded LRU cache keyed by a caller-supplied string.
// ---------------------------------------------------------------------------

/// Default capacity of a [`PaletteCache`].
pub const DEFAULT_CACHE_CAP: usize = 32;

/// A bounded LRU cache of extracted palettes, keyed by a caller-supplied string
/// (a track or album identifier). A repeated key returns the cached palette
/// without decoding again — that is the "repeated keys never re-extract"
/// acceptance criterion.
///
/// Uses only std collections. Not internally synchronised; wrap it in the
/// caller's own lock if it is shared across threads (again, extraction belongs
/// on a worker, not the render path).
pub struct PaletteCache {
    cap: usize,
    /// key → (palette, tick-of-last-use). A monotonically increasing clock
    /// records recency; the least-recently-used entry is evicted at capacity.
    map: HashMap<String, (ArtPalette, u64)>,
    clock: u64,
}

impl PaletteCache {
    /// A cache with [`DEFAULT_CACHE_CAP`] capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CACHE_CAP)
    }

    /// A cache holding at most `cap` entries (at least one).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            clock: 0,
        }
    }

    /// Number of cached palettes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Return the palette for `key`, extracting it only on a miss.
    ///
    /// On a hit the cached palette is returned and `bytes` is never called — no
    /// re-decode, no re-extraction. On a miss `bytes()` is invoked to obtain the
    /// encoded artwork, [`extract`] runs, and the result is cached (evicting the
    /// least-recently-used entry if at capacity). `bytes` is a closure so the
    /// caller can defer or avoid the cost of materialising the image bytes until
    /// a miss actually needs them.
    ///
    /// # Errors
    ///
    /// Propagates whatever [`extract`] returns on a miss. A failure is not
    /// cached, so a later call may retry.
    pub fn get_or_extract<F>(&mut self, key: &str, bytes: F) -> Result<ArtPalette, PaletteError>
    where
        F: FnOnce() -> Vec<u8>,
    {
        self.clock += 1;
        if let Some(entry) = self.map.get_mut(key) {
            entry.1 = self.clock;
            return Ok(entry.0.clone());
        }
        let palette = extract(&bytes())?;
        if self.map.len() >= self.cap {
            self.evict_lru();
        }
        self.map
            .insert(key.to_owned(), (palette.clone(), self.clock));
        Ok(palette)
    }

    /// Drop the least-recently-used entry.
    fn evict_lru(&mut self) {
        if let Some(k) = self
            .map
            .iter()
            .min_by_key(|(_, (_, t))| *t)
            .map(|(k, _)| k.clone())
        {
            self.map.remove(&k);
        }
    }
}

impl Default for PaletteCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbImage};
    use std::io::Cursor;
    use std::time::Instant;

    /// Encode an `RgbImage` to in-memory PNG bytes (no fixture files on disk).
    fn png_bytes(img: &RgbImage) -> Vec<u8> {
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode png");
        buf
    }

    /// A solid rectangle of one colour.
    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        ImageBuffer::from_pixel(w, h, Rgb(c))
    }

    /// A two-tone image: left `frac` of the width is `a`, the rest is `b`.
    fn two_tone(w: u32, h: u32, frac: f32, a: [u8; 3], b: [u8; 3]) -> RgbImage {
        let split = (w as f32 * frac) as u32;
        ImageBuffer::from_fn(w, h, |x, _| if x < split { Rgb(a) } else { Rgb(b) })
    }

    /// Perceptual lightness (Oklab L) of an sRGB triple, for assertions.
    fn lightness(c: [u8; 3]) -> f32 {
        Oklab::from_srgb(c[0], c[1], c[2]).l
    }

    #[test]
    fn deterministic_same_image_same_palette() {
        let img = two_tone(128, 128, 0.6, [200, 40, 40], [30, 60, 180]);
        let bytes = png_bytes(&img);
        let a = extract(&bytes).expect("extract a");
        let b = extract(&bytes).expect("extract b");
        assert_eq!(a, b, "same bytes must yield an identical palette");
    }

    #[test]
    fn dominant_matches_majority_tone() {
        // 70% red, 30% blue → dominant is red-ish.
        let red = [210, 30, 30];
        let blue = [30, 40, 200];
        let img = two_tone(200, 100, 0.7, red, blue);
        let pal = extract(&png_bytes(&img)).expect("extract");
        assert!(
            pal.dominant[0] > 150 && pal.dominant[1] < 90 && pal.dominant[2] < 90,
            "dominant should be the red majority, got {:?}",
            pal.dominant
        );
        // Blue should appear as an accent.
        assert!(
            pal.accents.iter().any(|a| a[2] > 140 && a[0] < 100),
            "the blue minority should surface as an accent, accents={:?}",
            pal.accents
        );
    }

    #[test]
    fn letterbox_bars_are_cropped_before_sampling() {
        // A colored core surrounded by thick black bars. The bars dominate by
        // area, so without the crop they would win the palette.
        let core = [40, 190, 90];
        let w = 200;
        let h = 200;
        let mut img = solid(w, h, [0, 0, 0]);
        for y in 60..140 {
            for x in 40..160 {
                img.put_pixel(x, y, Rgb(core));
            }
        }
        let padded = extract(&png_bytes(&img)).expect("extract padded");

        // The unpadded core alone.
        let bare = extract(&png_bytes(&solid(120, 80, core))).expect("extract bare");

        // Dominant of the padded image must be the core colour, not black.
        assert!(
            padded.dominant[1] > 120 && padded.dominant[0] < 120,
            "bars should be cropped; dominant={:?}",
            padded.dominant
        );
        let dist = (0..3)
            .map(|i| (i32::from(padded.dominant[i]) - i32::from(bare.dominant[i])).abs())
            .max()
            .unwrap();
        assert!(
            dist <= 8,
            "padded dominant {:?} should match the bare core {:?}",
            padded.dominant,
            bare.dominant
        );
    }

    #[test]
    fn light_is_lighter_and_dark_is_darker() {
        let img = solid(64, 64, [70, 130, 150]);
        let pal = extract(&png_bytes(&img)).expect("extract");
        let dl = lightness(pal.dominant);
        assert!(
            lightness(pal.light) > dl,
            "light {:?} (L={}) must be lighter than dominant {:?} (L={})",
            pal.light,
            lightness(pal.light),
            pal.dominant,
            dl
        );
        assert!(
            lightness(pal.dark) < dl,
            "dark {:?} (L={}) must be darker than dominant {:?} (L={})",
            pal.dark,
            lightness(pal.dark),
            pal.dominant,
            dl
        );
    }

    #[test]
    fn cache_hit_does_not_re_extract() {
        use std::cell::Cell;
        let img = two_tone(96, 96, 0.5, [180, 60, 60], [60, 60, 180]);
        let bytes = png_bytes(&img);

        let calls = Cell::new(0);
        let mut cache = PaletteCache::new();

        let first = cache
            .get_or_extract("track:1", || {
                calls.set(calls.get() + 1);
                bytes.clone()
            })
            .expect("first");
        let second = cache
            .get_or_extract("track:1", || {
                calls.set(calls.get() + 1);
                bytes.clone()
            })
            .expect("second");

        assert_eq!(calls.get(), 1, "the provider must run exactly once");
        assert_eq!(first, second, "cached palette must equal the extracted one");

        // A different key does re-extract.
        let _ = cache
            .get_or_extract("track:2", || {
                calls.set(calls.get() + 1);
                bytes.clone()
            })
            .expect("third");
        assert_eq!(calls.get(), 2, "a new key must invoke the provider");
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let mut cache = PaletteCache::with_capacity(2);
        let mk = |c: [u8; 3]| png_bytes(&solid(32, 32, c));
        let a = mk([200, 30, 30]);
        let b = mk([30, 200, 30]);
        let c = mk([30, 30, 200]);

        cache.get_or_extract("a", || a.clone()).unwrap();
        cache.get_or_extract("b", || b.clone()).unwrap();
        // Touch "a" so "b" becomes least-recently-used.
        cache.get_or_extract("a", || a.clone()).unwrap();
        // Insert "c": should evict "b".
        cache.get_or_extract("c", || c.clone()).unwrap();
        assert_eq!(cache.len(), 2);

        let calls = std::cell::Cell::new(0);
        cache
            .get_or_extract("b", || {
                calls.set(calls.get() + 1);
                b.clone()
            })
            .unwrap();
        assert_eq!(
            calls.get(),
            1,
            "b should have been evicted and re-extracted"
        );
    }

    #[test]
    fn monochrome_image_yields_valid_palette() {
        let img = solid(80, 80, [120, 120, 120]);
        let pal = extract(&png_bytes(&img)).expect("monochrome must not error");
        // Dominant is the grey; accents may be empty; light/dark still ordered.
        assert!(
            pal.accents.is_empty(),
            "a flat image has no distinct accents"
        );
        assert!(lightness(pal.light) >= lightness(pal.dominant));
        assert!(lightness(pal.dark) <= lightness(pal.dominant));
        // All eight slots are populated (no panics, full array).
        assert_eq!(pal.slots.len(), 8);
    }

    #[test]
    fn undecodable_bytes_error() {
        let err = extract(b"not an image at all").unwrap_err();
        assert!(matches!(err, PaletteError::Decode(_)));
    }

    #[test]
    fn extraction_is_under_budget_on_640x640() {
        // A non-trivial synthetic cover: a diagonal blend of several bands so
        // k-means has real work to do.
        let img: RgbImage = ImageBuffer::from_fn(640, 640, |x, y| {
            let band = ((x + y) / 80) % 5;
            let c = match band {
                0 => [200, 40, 40],
                1 => [40, 160, 200],
                2 => [240, 200, 40],
                3 => [80, 60, 160],
                _ => [30, 30, 30],
            };
            Rgb(c)
        });
        let bytes = png_bytes(&img);

        // Warm the decode path once, then measure extraction.
        let _ = extract(&bytes).expect("warm");
        let start = Instant::now();
        let _ = extract(&bytes).expect("measure");
        let elapsed = start.elapsed();
        // The AC's <200 ms budget describes optimized extraction; tests run
        // unoptimized, often on loaded shared CI runners (170 ms was observed
        // there against the release-path ~tens of ms). Keep a tight bound for
        // release builds and an order-of-magnitude smoke bound for debug.
        let budget_ms = if cfg!(debug_assertions) { 600 } else { 150 };
        assert!(
            elapsed.as_millis() < budget_ms,
            "extraction took {elapsed:?}, budget is {budget_ms}ms (AC: <200ms optimized)"
        );
        // Print the measurement so the runner surfaces it.
        println!("extraction_640x640_ms = {}", elapsed.as_secs_f64() * 1000.0);
    }

    #[test]
    fn slots_mirror_the_host_layout() {
        let img = two_tone(120, 120, 0.6, [190, 50, 50], [50, 120, 200]);
        let pal = extract(&png_bytes(&img)).expect("extract");
        // Slot 0 (dark anchor) must be no lighter than slot 1 (mid) which must
        // be no lighter than slot 2 (bright) — the gradient ordering scenes
        // rely on.
        assert!(lightness(pal.slots[0]) <= lightness(pal.slots[1]) + 1e-3);
        assert!(lightness(pal.slots[1]) <= lightness(pal.slots[2]) + 1e-3);
        // Neutrals climb in lightness across 5→6→7.
        assert!(lightness(pal.slots[5]) < lightness(pal.slots[6]));
        assert!(lightness(pal.slots[6]) < lightness(pal.slots[7]));
    }
}
