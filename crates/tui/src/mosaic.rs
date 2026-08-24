//! The cell-mosaic rasterizer: turn a scenes [`Canvas`] display list into
//! terminal cells on a subpixel ladder.
//!
//! A [`FrameBuffer`] holds a preallocated RGB pixel grid whose resolution is the
//! current cell grid multiplied by the active [`Tier`]'s subcell geometry
//! (octant `2×4` → sextant `2×3` → quadrant `2×2` → half-block `1×2`). Scenes
//! rasterize normalized [`Primitive`]s into that grid; [`FrameBuffer::encode`]
//! then reduces every `sx×sy` pixel block to a single terminal [`Cell`] — a
//! glyph plus a truecolor foreground/background pair — by clustering the block
//! into two colors and mapping the occupancy bitmap to the matching legacy
//! block glyph.
//!
//! This module is UI-free: it depends on `scia-scenes` (itself pure) but never
//! on `ratatui`, so the whole rasterizer is exercised with plain data in unit
//! tests. The `ratatui` conversion lives in [`crate::presenter`].
//!
//! # Glyph tables
//!
//! The four glyph tables are indexed by an `sx*sy`-bit occupancy pattern whose
//! bit for subpixel `(col, row)` is `1 << (row * sx + col)` (row-major, row `0`
//! at the top, bit `0` the top-left subpixel). Every table entry is a single
//! `char`; a pattern that Unicode did not encode as a dedicated block glyph
//! reuses the pre-existing legacy block character that draws the same shape.
//! Code points are taken from the official charts, verified entry-by-entry
//! against `UnicodeData.txt`:
//!
//! - Quadrants: **Unicode "Block Elements", U+2580–U+259F** plus the space.
//! - Sextants: **Unicode 13.0 "Symbols for Legacy Computing", U+1FB00–U+1FB3B**
//!   (the block sextants), with the four patterns Unicode omitted — empty,
//!   left half `U+258C`, right half `U+2590`, full `U+2588` — mapped to those
//!   existing characters.
//! - Octants: **Unicode 16.0 "Symbols for Legacy Computing Supplement",
//!   U+1CD00–U+1CDE5** (230 block octants), with the 26 patterns Unicode did
//!   not re-encode mapped to their existing characters: the space and full
//!   block; the upper/lower/left/right halves (`U+2580 U+2584 U+258C U+2590`);
//!   the ten quadrant glyphs (`U+2596–U+259F`); the upper/lower one-quarter and
//!   three-quarter blocks (`U+1FB82 U+1FB85 U+2582 U+2586`); the four
//!   single-corner "half upper/lower one quarter" cells (`U+1CEA8 U+1CEAB
//!   U+1CEA3 U+1CEA0`); and the two "middle left/right one quarter" cells
//!   (`U+1FBE6 U+1FBE7`).
//!
//! Every table is complete (all patterns mapped), maps the empty pattern to a
//! space and the full pattern to `U+2588`, and is injective; the tests in
//! `tests/mosaic.rs` assert these invariants.

use scia_scenes::{Blend, Canvas, Palette, Primitive, Rgb, Slot, Style};

/// The subpixel ladder. Each rung packs more subpixels per terminal cell, so a
/// coarser rung is safe on terminals whose fonts lack the finer glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tier {
    /// `2×4` subpixels per cell (Unicode 16 block octants).
    Octant,
    /// `2×3` subpixels per cell (block sextants).
    Sextant,
    /// `2×2` subpixels per cell (quadrant block elements).
    Quadrant,
    /// `1×2` subpixels per cell (upper/lower half blocks). The safe default
    /// until runtime capability probing (a later card) can pick a finer rung.
    #[default]
    Half,
}

impl Tier {
    /// The `(sx, sy)` subpixel geometry: columns then rows per terminal cell.
    #[must_use]
    pub fn subcells(self) -> (u16, u16) {
        match self {
            Tier::Octant => (2, 4),
            Tier::Sextant => (2, 3),
            Tier::Quadrant => (2, 2),
            Tier::Half => (1, 2),
        }
    }

    /// The human label shown in the debug line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tier::Octant => "octants",
            Tier::Sextant => "sextants",
            Tier::Quadrant => "quadrants",
            Tier::Half => "half-blocks",
        }
    }

    /// The glyph table for this tier, indexed by the occupancy bitmap.
    fn glyphs(self) -> &'static [char] {
        match self {
            Tier::Octant => &OCTANT_GLYPHS,
            Tier::Sextant => &SEXTANT_GLYPHS,
            Tier::Quadrant => &QUADRANT_GLYPHS,
            Tier::Half => &HALF_GLYPHS,
        }
    }
}

/// One rasterized terminal cell: a glyph and its truecolor pair. Plain data —
/// no `ratatui` types — so the rasterizer stays testable without a terminal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    /// The block glyph for this cell's occupancy pattern.
    pub ch: char,
    /// Foreground colour (the brighter cluster).
    pub fg: (u8, u8, u8),
    /// Background colour (the darker cluster).
    pub bg: (u8, u8, u8),
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: (0, 0, 0),
            bg: (0, 0, 0),
        }
    }
}

/// A grid of encoded [`Cell`]s: the rasterizer's terminal-facing output. Plain
/// data; [`crate::presenter`] copies it into a `ratatui` buffer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellGrid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
}

impl CellGrid {
    /// An empty grid.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resize to `cols × rows`, retaining capacity. Cells are reset to the
    /// default (empty, black).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        let len = cols as usize * rows as usize;
        self.cells.clear();
        self.cells.resize(len, Cell::default());
    }

    /// Grid width in cells.
    #[must_use]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Grid height in cells.
    #[must_use]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// The cell at `(x, y)`, or `None` when out of bounds.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get(y as usize * self.cols as usize + x as usize)
    }
}

/// A text run collected during rasterization: a [`Primitive::Text`] is never
/// drawn into the pixel grid, it is handed back here so the caller can draw it
/// as real terminal text on top of the mosaic.
///
/// The run's string lives in the [`FrameBuffer`]'s text arena; resolve it with
/// [`FrameBuffer::run_text`]. Keeping the text in a shared arena (rather than an
/// owned `String` per run) is what lets rasterization stay allocation-free once
/// warmed up.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    /// Anchor cell column.
    pub cell_x: u16,
    /// Anchor cell row.
    pub cell_y: u16,
    /// The palette slot colouring the text.
    pub slot: Slot,
    /// The text intensity in `0.0..=1.0`.
    pub intensity: f32,
    /// Byte range of the run in the frame buffer's text arena.
    span: core::ops::Range<u32>,
}

/// A preallocated RGB pixel grid plus the text runs collected from the last
/// rasterization pass.
///
/// The grid is sized `cols*sx × rows*sy` for the current cell grid and
/// [`Tier`]; [`FrameBuffer::resize`] reallocates only when those dimensions
/// change. After a warm-up frame, [`FrameBuffer::rasterize`] and
/// [`FrameBuffer::encode`] allocate nothing: the pixel grid, the text-run
/// vector and the text arena all retain their capacity across [`clear`]. The
/// text-run vector's steady-state capacity is whatever the busiest frame needs;
/// a scene that draws the same text budget every frame never regrows it.
///
/// [`clear`]: FrameBuffer::clear
#[derive(Clone, Debug, Default)]
pub struct FrameBuffer {
    cols: u16,
    rows: u16,
    tier: Tier,
    /// Pixel-grid width, `cols * sx`.
    px_w: u16,
    /// Pixel-grid height, `rows * sy`.
    px_h: u16,
    /// Row-major RGB pixels in `0.0..=1.0`, `px_w * px_h` of them.
    pixels: Vec<[f32; 3]>,
    text_runs: Vec<TextRun>,
    text_arena: String,
}

impl FrameBuffer {
    /// An empty frame buffer with no pixels.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resize to a `cols × rows` cell grid at `tier`, (re)allocating the pixel
    /// grid only when the pixel dimensions actually change. Clears to black.
    pub fn resize(&mut self, cols: u16, rows: u16, tier: Tier) {
        let (sx, sy) = tier.subcells();
        let px_w = cols.saturating_mul(sx);
        let px_h = rows.saturating_mul(sy);
        self.cols = cols;
        self.rows = rows;
        self.tier = tier;
        self.px_w = px_w;
        self.px_h = px_h;
        let len = px_w as usize * px_h as usize;
        if self.pixels.len() != len {
            self.pixels.clear();
            self.pixels.resize(len, [0.0; 3]);
        } else {
            self.clear();
        }
    }

    /// The active tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Cell-grid width.
    #[must_use]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Cell-grid height.
    #[must_use]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Reset every pixel to black and drop the collected text runs, retaining
    /// all capacity.
    pub fn clear(&mut self) {
        for p in &mut self.pixels {
            *p = [0.0; 3];
        }
        self.text_runs.clear();
        self.text_arena.clear();
    }

    /// Cross-fade `other` into this buffer: each pixel becomes
    /// `other * (1 - t) + self * t`, with `t` clamped to `0.0..=1.0`. `self` is
    /// the incoming frame and `other` the outgoing one, so `t = 0` shows only
    /// `other` and `t = 1` only `self`.
    ///
    /// A no-op when the two grids differ in pixel size. Allocation-free: it
    /// reads and writes the existing pixel stores in place. Text runs are not
    /// mixed — the incoming buffer's runs are kept as-is.
    pub fn mix_from(&mut self, other: &FrameBuffer, t: f32) {
        if self.px_w != other.px_w || self.px_h != other.px_h {
            return;
        }
        let t = clamp01(t);
        let inv = 1.0 - t;
        for (dst, src) in self.pixels.iter_mut().zip(other.pixels.iter()) {
            dst[0] = src[0] * inv + dst[0] * t;
            dst[1] = src[1] * inv + dst[1] * t;
            dst[2] = src[2] * inv + dst[2] * t;
        }
    }

    /// The text runs collected by the last [`rasterize`](Self::rasterize) pass.
    #[must_use]
    pub fn text_runs(&self) -> &[TextRun] {
        &self.text_runs
    }

    /// Resolve a [`TextRun`] to its string.
    #[must_use]
    pub fn run_text(&self, run: &TextRun) -> &str {
        let start = run.span.start as usize;
        let end = run.span.end as usize;
        self.text_arena.get(start..end).unwrap_or("")
    }

    /// Rasterize one canvas into the pixel grid with the given blend and layer
    /// intensity. Text primitives are collected as [`TextRun`]s instead of
    /// being drawn. Within an `over` layer later primitives replace earlier
    /// ones; an `add`/`max` layer accumulates per pixel (clamped to `1.0`).
    pub fn rasterize(&mut self, canvas: &Canvas, palette: &Palette, blend: Blend, intensity: f32) {
        if self.px_w == 0 || self.px_h == 0 {
            return;
        }
        let layer = clamp01(intensity);
        for prim in canvas.primitives() {
            match *prim {
                Primitive::Bar { x, y, w, h, style } => {
                    let color = self.slot_color(palette, style, layer);
                    let (x0, x1) = span_px(x, w, self.px_w);
                    let (y0, y1) = span_px(y, h, self.px_h);
                    self.fill_rect(x0, y0, x1, y1, color, blend);
                }
                Primitive::Line {
                    x0,
                    y0,
                    x1,
                    y1,
                    width,
                    style,
                } => {
                    let color = self.slot_color(palette, style, layer);
                    self.draw_line(x0, y0, x1, y1, width, color, blend);
                }
                Primitive::Point { x, y, size, style } => {
                    let color = self.slot_color(palette, style, layer);
                    self.draw_point(x, y, size, color, blend);
                }
                Primitive::Field {
                    cols, rows, style, ..
                } => {
                    if let Some(values) = canvas.field_of(prim) {
                        self.fill_field(cols, rows, values, palette, style, layer, blend);
                    }
                }
                Primitive::Text { x, y, style, .. } => {
                    if let Some(text) = canvas.text_of(prim) {
                        self.collect_text(x, y, text, style, layer);
                    }
                }
            }
        }
    }

    /// The linear-scaled RGB colour for a styled primitive: the palette slot
    /// colour times the style intensity times the layer intensity.
    fn slot_color(&self, palette: &Palette, style: Style, layer: f32) -> [f32; 3] {
        let Rgb(r, g, b) = palette.color(style.slot);
        let s = clamp01(style.intensity) * layer;
        [
            f32::from(r) / 255.0 * s,
            f32::from(g) / 255.0 * s,
            f32::from(b) / 255.0 * s,
        ]
    }

    /// Blend `color` into the pixel at `(px, py)`.
    fn put(&mut self, px: u16, py: u16, color: [f32; 3], blend: Blend) {
        if px >= self.px_w || py >= self.px_h {
            return;
        }
        let idx = py as usize * self.px_w as usize + px as usize;
        let dst = &mut self.pixels[idx];
        match blend {
            Blend::Over => *dst = color,
            Blend::Add => {
                *dst = [
                    (dst[0] + color[0]).min(1.0),
                    (dst[1] + color[1]).min(1.0),
                    (dst[2] + color[2]).min(1.0),
                ];
            }
            Blend::Max => {
                *dst = [
                    dst[0].max(color[0]),
                    dst[1].max(color[1]),
                    dst[2].max(color[2]),
                ];
            }
        }
    }

    /// Fill the half-open pixel rectangle `[x0, x1) × [y0, y1)`.
    fn fill_rect(&mut self, x0: u16, y0: u16, x1: u16, y1: u16, color: [f32; 3], blend: Blend) {
        for py in y0..y1 {
            for px in x0..x1 {
                self.put(px, py, color, blend);
            }
        }
    }

    /// A Bresenham line thickened to `width` (a fraction of the pixel-grid
    /// height, at least one pixel) by stamping a square at each step.
    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        color: [f32; 3],
        blend: Blend,
    ) {
        let ax = to_px(x0, self.px_w);
        let ay = to_px(y0, self.px_h);
        let bx = to_px(x1, self.px_w);
        let by = to_px(y1, self.px_h);
        let thick = ((clamp01(width) * self.px_h as f32).round() as i32).max(1);
        let half = (thick - 1) / 2;

        let dx = (bx - ax).abs();
        let dy = -(by - ay).abs();
        let sx = if ax < bx { 1 } else { -1 };
        let sy = if ay < by { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut cx, mut cy) = (ax, ay);
        loop {
            self.stamp(cx, cy, half, color, blend);
            if cx == bx && cy == by {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                cx += sx;
            }
            if e2 <= dx {
                err += dx;
                cy += sy;
            }
        }
    }

    /// A filled square of half-extent `size` centred on the point.
    fn draw_point(&mut self, x: f32, y: f32, size: f32, color: [f32; 3], blend: Blend) {
        let cx = to_px(x, self.px_w);
        let cy = to_px(y, self.px_h);
        let extent = ((clamp01(size) * self.px_h as f32).round() as i32).max(1);
        let half = (extent - 1) / 2;
        self.stamp(cx, cy, half, color, blend);
    }

    /// Stamp a `(2*half+1)` square centred at `(cx, cy)`, clipped to the grid.
    fn stamp(&mut self, cx: i32, cy: i32, half: i32, color: [f32; 3], blend: Blend) {
        for oy in -half..=half {
            for ox in -half..=half {
                let px = cx + ox;
                let py = cy + oy;
                if px >= 0 && py >= 0 && px < self.px_w as i32 && py < self.px_h as i32 {
                    self.put(px as u16, py as u16, color, blend);
                }
            }
        }
    }

    /// Fill a `cols × rows` field: each field cell paints its pixel rect with
    /// the slot colour scaled by the field value.
    #[allow(clippy::too_many_arguments)]
    fn fill_field(
        &mut self,
        cols: u16,
        rows: u16,
        values: &[f32],
        palette: &Palette,
        style: Style,
        layer: f32,
        blend: Blend,
    ) {
        if cols == 0 || rows == 0 {
            return;
        }
        let Rgb(r, g, b) = palette.color(style.slot);
        let base = [
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        ];
        let s = clamp01(style.intensity) * layer;
        for fr in 0..rows {
            let y0 = frac_px(fr, rows, self.px_h);
            let y1 = frac_px(fr + 1, rows, self.px_h);
            for fc in 0..cols {
                let idx = fr as usize * cols as usize + fc as usize;
                let v = clamp01(values.get(idx).copied().unwrap_or(0.0));
                let scale = s * v;
                let color = [base[0] * scale, base[1] * scale, base[2] * scale];
                let x0 = frac_px(fc, cols, self.px_w);
                let x1 = frac_px(fc + 1, cols, self.px_w);
                self.fill_rect(x0, y0, x1, y1, color, blend);
            }
        }
    }

    /// Record a text run at the cell containing its normalized anchor.
    fn collect_text(&mut self, x: f32, y: f32, text: &str, style: Style, layer: f32) {
        let cell_x = ((clamp01(x) * self.cols as f32) as u16).min(self.cols.saturating_sub(1));
        let cell_y = ((clamp01(y) * self.rows as f32) as u16).min(self.rows.saturating_sub(1));
        let start = self.text_arena.len() as u32;
        self.text_arena.push_str(text);
        let end = self.text_arena.len() as u32;
        self.text_runs.push(TextRun {
            cell_x,
            cell_y,
            slot: style.slot,
            intensity: clamp01(style.intensity) * layer,
            span: start..end,
        });
    }

    /// Encode the pixel grid into `out`: one [`Cell`] per `sx×sy` block.
    pub fn encode(&self, out: &mut CellGrid) {
        out.resize(self.cols, self.rows);
        let (sx, sy) = self.tier.subcells();
        let glyphs = self.tier.glyphs();
        for cy in 0..self.rows {
            for cx in 0..self.cols {
                let cell = self.encode_cell(cx, cy, sx, sy, glyphs);
                let idx = cy as usize * self.cols as usize + cx as usize;
                out.cells[idx] = cell;
            }
        }
    }

    /// Encode a single cell: cluster its `sx×sy` pixels into two colours and map
    /// the occupancy bitmap to a glyph.
    fn encode_cell(&self, cx: u16, cy: u16, sx: u16, sy: u16, glyphs: &[char]) -> Cell {
        // Gather the block's pixels and find the darkest and brightest.
        let mut min_l = f32::INFINITY;
        let mut max_l = f32::NEG_INFINITY;
        let mut dark = [0.0; 3];
        let mut bright = [0.0; 3];
        for row in 0..sy {
            for col in 0..sx {
                let p = self.pixel(cx * sx + col, cy * sy + row);
                let l = luma(p);
                if l < min_l {
                    min_l = l;
                    dark = p;
                }
                if l > max_l {
                    max_l = l;
                    bright = p;
                }
            }
        }

        // An empty (all-black) block is a space over black.
        if max_l < EPS {
            return Cell {
                ch: ' ',
                fg: (0, 0, 0),
                bg: (0, 0, 0),
            };
        }
        // A uniform non-black block is a solid full cell in that colour.
        if max_l - min_l < EPS {
            let c = to_u8(bright);
            return Cell {
                ch: glyphs[glyphs.len() - 1],
                fg: c,
                bg: c,
            };
        }

        // Two-means on RGB seeded with the darkest and brightest pixels; a small
        // fixed iteration count keeps it deterministic and allocation-free.
        let (bg_c, fg_c) = self.two_means(cx, cy, sx, sy, dark, bright);

        // Build the occupancy bitmap: a subpixel is foreground when it is at
        // least as close to the brighter centroid as to the darker one.
        let mut bitmap: u32 = 0;
        for row in 0..sy {
            for col in 0..sx {
                let p = self.pixel(cx * sx + col, cy * sy + row);
                if dist2(p, fg_c) <= dist2(p, bg_c) {
                    bitmap |= 1 << (row * sx + col);
                }
            }
        }
        Cell {
            ch: glyphs[bitmap as usize],
            fg: to_u8(fg_c),
            bg: to_u8(bg_c),
        }
    }

    /// Two rounds of 2-means over a cell's pixels, returning `(bg, fg)`
    /// centroids ordered darker-first.
    fn two_means(
        &self,
        cx: u16,
        cy: u16,
        sx: u16,
        sy: u16,
        mut c0: [f32; 3],
        mut c1: [f32; 3],
    ) -> ([f32; 3], [f32; 3]) {
        for _ in 0..3 {
            let mut sum0 = [0.0; 3];
            let mut sum1 = [0.0; 3];
            let mut n0 = 0u32;
            let mut n1 = 0u32;
            for row in 0..sy {
                for col in 0..sx {
                    let p = self.pixel(cx * sx + col, cy * sy + row);
                    if dist2(p, c0) <= dist2(p, c1) {
                        sum0 = [sum0[0] + p[0], sum0[1] + p[1], sum0[2] + p[2]];
                        n0 += 1;
                    } else {
                        sum1 = [sum1[0] + p[0], sum1[1] + p[1], sum1[2] + p[2]];
                        n1 += 1;
                    }
                }
            }
            if n0 > 0 {
                let d = n0 as f32;
                c0 = [sum0[0] / d, sum0[1] / d, sum0[2] / d];
            }
            if n1 > 0 {
                let d = n1 as f32;
                c1 = [sum1[0] / d, sum1[1] / d, sum1[2] / d];
            }
        }
        // Order darker-first so cluster 0 is the background.
        if luma(c0) <= luma(c1) {
            (c0, c1)
        } else {
            (c1, c0)
        }
    }

    /// The pixel at `(px, py)`, or black when out of bounds.
    fn pixel(&self, px: u16, py: u16) -> [f32; 3] {
        if px >= self.px_w || py >= self.px_h {
            return [0.0; 3];
        }
        self.pixels[py as usize * self.px_w as usize + px as usize]
    }
}

/// Clamp to `0.0..=1.0`, mapping `NaN` to `0.0`.
#[inline]
fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// Below this luma a block counts as black, and two lumas this close count as
/// equal.
const EPS: f32 = 1.0 / 512.0;

/// Rec. 601 luma of a linear-scaled RGB triple.
#[inline]
fn luma(c: [f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

/// Squared Euclidean distance between two RGB triples.
#[inline]
fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    d0 * d0 + d1 * d1 + d2 * d2
}

/// Convert a `0.0..=1.0` RGB triple to `u8` components.
#[inline]
fn to_u8(c: [f32; 3]) -> (u8, u8, u8) {
    let q = |v: f32| (clamp01(v) * 255.0).round() as u8;
    (q(c[0]), q(c[1]), q(c[2]))
}

/// Map a normalized coordinate to a pixel index (rounded), clamped to
/// `0..dim`.
#[inline]
fn to_px(v: f32, dim: u16) -> i32 {
    let p = (clamp01(v) * dim as f32).round() as i32;
    p.clamp(0, dim as i32 - 1)
}

/// The half-open pixel span `[start, end)` for a normalized `[a, a+len]`
/// interval over `dim` pixels, rounded to the nearest pixel edge.
#[inline]
fn span_px(a: f32, len: f32, dim: u16) -> (u16, u16) {
    let lo = (clamp01(a) * dim as f32).round() as i32;
    let hi = (clamp01(a + len) * dim as f32).round() as i32;
    let lo = lo.clamp(0, dim as i32) as u16;
    let hi = hi.clamp(0, dim as i32) as u16;
    (lo, hi.max(lo))
}

/// The pixel edge for grid line `i` of `n` over `dim` pixels.
#[inline]
fn frac_px(i: u16, n: u16, dim: u16) -> u16 {
    ((i as u32 * dim as u32) / n as u32) as u16
}

// ---------------------------------------------------------------------------
// Glyph tables — see the module docs for the chart citations. Each is indexed
// by the `sx*sy`-bit occupancy pattern (`bit = 1 << (row * sx + col)`). Every
// pattern maps to a single `char`; patterns Unicode did not encode as a
// dedicated block glyph reuse the pre-existing legacy character of the same
// shape. Generated from `UnicodeData.txt` (Unicode 16.0) and verified by the
// invariants in the unit tests below.
// ---------------------------------------------------------------------------

const HALF_GLYPHS: [char; 4] = ['\u{20}', '\u{2580}', '\u{2584}', '\u{2588}'];

const QUADRANT_GLYPHS: [char; 16] = [
    '\u{20}', '\u{2598}', '\u{259D}', '\u{2580}', '\u{2596}', '\u{258C}', '\u{259E}', '\u{259B}',
    '\u{2597}', '\u{259A}', '\u{2590}', '\u{259C}', '\u{2584}', '\u{2599}', '\u{259F}', '\u{2588}',
];

const SEXTANT_GLYPHS: [char; 64] = [
    '\u{20}',
    '\u{1FB00}',
    '\u{1FB01}',
    '\u{1FB02}',
    '\u{1FB03}',
    '\u{1FB04}',
    '\u{1FB05}',
    '\u{1FB06}',
    '\u{1FB07}',
    '\u{1FB08}',
    '\u{1FB09}',
    '\u{1FB0A}',
    '\u{1FB0B}',
    '\u{1FB0C}',
    '\u{1FB0D}',
    '\u{1FB0E}',
    '\u{1FB0F}',
    '\u{1FB10}',
    '\u{1FB11}',
    '\u{1FB12}',
    '\u{1FB13}',
    '\u{258C}',
    '\u{1FB14}',
    '\u{1FB15}',
    '\u{1FB16}',
    '\u{1FB17}',
    '\u{1FB18}',
    '\u{1FB19}',
    '\u{1FB1A}',
    '\u{1FB1B}',
    '\u{1FB1C}',
    '\u{1FB1D}',
    '\u{1FB1E}',
    '\u{1FB1F}',
    '\u{1FB20}',
    '\u{1FB21}',
    '\u{1FB22}',
    '\u{1FB23}',
    '\u{1FB24}',
    '\u{1FB25}',
    '\u{1FB26}',
    '\u{1FB27}',
    '\u{2590}',
    '\u{1FB28}',
    '\u{1FB29}',
    '\u{1FB2A}',
    '\u{1FB2B}',
    '\u{1FB2C}',
    '\u{1FB2D}',
    '\u{1FB2E}',
    '\u{1FB2F}',
    '\u{1FB30}',
    '\u{1FB31}',
    '\u{1FB32}',
    '\u{1FB33}',
    '\u{1FB34}',
    '\u{1FB35}',
    '\u{1FB36}',
    '\u{1FB37}',
    '\u{1FB38}',
    '\u{1FB39}',
    '\u{1FB3A}',
    '\u{1FB3B}',
    '\u{2588}',
];

const OCTANT_GLYPHS: [char; 256] = [
    '\u{20}',
    '\u{1CEA8}',
    '\u{1CEAB}',
    '\u{1FB82}',
    '\u{1CD00}',
    '\u{2598}',
    '\u{1CD01}',
    '\u{1CD02}',
    '\u{1CD03}',
    '\u{1CD04}',
    '\u{259D}',
    '\u{1CD05}',
    '\u{1CD06}',
    '\u{1CD07}',
    '\u{1CD08}',
    '\u{2580}',
    '\u{1CD09}',
    '\u{1CD0A}',
    '\u{1CD0B}',
    '\u{1CD0C}',
    '\u{1FBE6}',
    '\u{1CD0D}',
    '\u{1CD0E}',
    '\u{1CD0F}',
    '\u{1CD10}',
    '\u{1CD11}',
    '\u{1CD12}',
    '\u{1CD13}',
    '\u{1CD14}',
    '\u{1CD15}',
    '\u{1CD16}',
    '\u{1CD17}',
    '\u{1CD18}',
    '\u{1CD19}',
    '\u{1CD1A}',
    '\u{1CD1B}',
    '\u{1CD1C}',
    '\u{1CD1D}',
    '\u{1CD1E}',
    '\u{1CD1F}',
    '\u{1FBE7}',
    '\u{1CD20}',
    '\u{1CD21}',
    '\u{1CD22}',
    '\u{1CD23}',
    '\u{1CD24}',
    '\u{1CD25}',
    '\u{1CD26}',
    '\u{1CD27}',
    '\u{1CD28}',
    '\u{1CD29}',
    '\u{1CD2A}',
    '\u{1CD2B}',
    '\u{1CD2C}',
    '\u{1CD2D}',
    '\u{1CD2E}',
    '\u{1CD2F}',
    '\u{1CD30}',
    '\u{1CD31}',
    '\u{1CD32}',
    '\u{1CD33}',
    '\u{1CD34}',
    '\u{1CD35}',
    '\u{1FB85}',
    '\u{1CEA3}',
    '\u{1CD36}',
    '\u{1CD37}',
    '\u{1CD38}',
    '\u{1CD39}',
    '\u{1CD3A}',
    '\u{1CD3B}',
    '\u{1CD3C}',
    '\u{1CD3D}',
    '\u{1CD3E}',
    '\u{1CD3F}',
    '\u{1CD40}',
    '\u{1CD41}',
    '\u{1CD42}',
    '\u{1CD43}',
    '\u{1CD44}',
    '\u{2596}',
    '\u{1CD45}',
    '\u{1CD46}',
    '\u{1CD47}',
    '\u{1CD48}',
    '\u{258C}',
    '\u{1CD49}',
    '\u{1CD4A}',
    '\u{1CD4B}',
    '\u{1CD4C}',
    '\u{259E}',
    '\u{1CD4D}',
    '\u{1CD4E}',
    '\u{1CD4F}',
    '\u{1CD50}',
    '\u{259B}',
    '\u{1CD51}',
    '\u{1CD52}',
    '\u{1CD53}',
    '\u{1CD54}',
    '\u{1CD55}',
    '\u{1CD56}',
    '\u{1CD57}',
    '\u{1CD58}',
    '\u{1CD59}',
    '\u{1CD5A}',
    '\u{1CD5B}',
    '\u{1CD5C}',
    '\u{1CD5D}',
    '\u{1CD5E}',
    '\u{1CD5F}',
    '\u{1CD60}',
    '\u{1CD61}',
    '\u{1CD62}',
    '\u{1CD63}',
    '\u{1CD64}',
    '\u{1CD65}',
    '\u{1CD66}',
    '\u{1CD67}',
    '\u{1CD68}',
    '\u{1CD69}',
    '\u{1CD6A}',
    '\u{1CD6B}',
    '\u{1CD6C}',
    '\u{1CD6D}',
    '\u{1CD6E}',
    '\u{1CD6F}',
    '\u{1CD70}',
    '\u{1CEA0}',
    '\u{1CD71}',
    '\u{1CD72}',
    '\u{1CD73}',
    '\u{1CD74}',
    '\u{1CD75}',
    '\u{1CD76}',
    '\u{1CD77}',
    '\u{1CD78}',
    '\u{1CD79}',
    '\u{1CD7A}',
    '\u{1CD7B}',
    '\u{1CD7C}',
    '\u{1CD7D}',
    '\u{1CD7E}',
    '\u{1CD7F}',
    '\u{1CD80}',
    '\u{1CD81}',
    '\u{1CD82}',
    '\u{1CD83}',
    '\u{1CD84}',
    '\u{1CD85}',
    '\u{1CD86}',
    '\u{1CD87}',
    '\u{1CD88}',
    '\u{1CD89}',
    '\u{1CD8A}',
    '\u{1CD8B}',
    '\u{1CD8C}',
    '\u{1CD8D}',
    '\u{1CD8E}',
    '\u{1CD8F}',
    '\u{2597}',
    '\u{1CD90}',
    '\u{1CD91}',
    '\u{1CD92}',
    '\u{1CD93}',
    '\u{259A}',
    '\u{1CD94}',
    '\u{1CD95}',
    '\u{1CD96}',
    '\u{1CD97}',
    '\u{2590}',
    '\u{1CD98}',
    '\u{1CD99}',
    '\u{1CD9A}',
    '\u{1CD9B}',
    '\u{259C}',
    '\u{1CD9C}',
    '\u{1CD9D}',
    '\u{1CD9E}',
    '\u{1CD9F}',
    '\u{1CDA0}',
    '\u{1CDA1}',
    '\u{1CDA2}',
    '\u{1CDA3}',
    '\u{1CDA4}',
    '\u{1CDA5}',
    '\u{1CDA6}',
    '\u{1CDA7}',
    '\u{1CDA8}',
    '\u{1CDA9}',
    '\u{1CDAA}',
    '\u{1CDAB}',
    '\u{2582}',
    '\u{1CDAC}',
    '\u{1CDAD}',
    '\u{1CDAE}',
    '\u{1CDAF}',
    '\u{1CDB0}',
    '\u{1CDB1}',
    '\u{1CDB2}',
    '\u{1CDB3}',
    '\u{1CDB4}',
    '\u{1CDB5}',
    '\u{1CDB6}',
    '\u{1CDB7}',
    '\u{1CDB8}',
    '\u{1CDB9}',
    '\u{1CDBA}',
    '\u{1CDBB}',
    '\u{1CDBC}',
    '\u{1CDBD}',
    '\u{1CDBE}',
    '\u{1CDBF}',
    '\u{1CDC0}',
    '\u{1CDC1}',
    '\u{1CDC2}',
    '\u{1CDC3}',
    '\u{1CDC4}',
    '\u{1CDC5}',
    '\u{1CDC6}',
    '\u{1CDC7}',
    '\u{1CDC8}',
    '\u{1CDC9}',
    '\u{1CDCA}',
    '\u{1CDCB}',
    '\u{1CDCC}',
    '\u{1CDCD}',
    '\u{1CDCE}',
    '\u{1CDCF}',
    '\u{1CDD0}',
    '\u{1CDD1}',
    '\u{1CDD2}',
    '\u{1CDD3}',
    '\u{1CDD4}',
    '\u{1CDD5}',
    '\u{1CDD6}',
    '\u{1CDD7}',
    '\u{1CDD8}',
    '\u{1CDD9}',
    '\u{1CDDA}',
    '\u{2584}',
    '\u{1CDDB}',
    '\u{1CDDC}',
    '\u{1CDDD}',
    '\u{1CDDE}',
    '\u{2599}',
    '\u{1CDDF}',
    '\u{1CDE0}',
    '\u{1CDE1}',
    '\u{1CDE2}',
    '\u{259F}',
    '\u{1CDE3}',
    '\u{2586}',
    '\u{1CDE4}',
    '\u{1CDE5}',
    '\u{2588}',
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every table entry is a real glyph, the empty pattern is a space, the full
    /// pattern is `U+2588`, and the table is injective (each pattern draws a
    /// distinct glyph — stronger than "injective where Unicode is", which the
    /// authoritative tables happen to satisfy in full).
    fn assert_table(glyphs: &[char], expected_len: usize) {
        assert_eq!(glyphs.len(), expected_len);
        assert_eq!(glyphs[0], ' ', "empty pattern must be a space");
        assert_eq!(
            glyphs[expected_len - 1],
            '\u{2588}',
            "full pattern must be U+2588"
        );
        for (i, &c) in glyphs.iter().enumerate() {
            assert!(c != '\0', "entry {i} is the null char");
            assert!(
                !c.is_whitespace() || i == 0,
                "entry {i} is blank but nonzero"
            );
        }
        let distinct: HashSet<char> = glyphs.iter().copied().collect();
        assert_eq!(distinct.len(), glyphs.len(), "table has duplicate glyphs");
    }

    #[test]
    fn half_table_complete() {
        assert_table(&HALF_GLYPHS, 4);
    }

    #[test]
    fn quadrant_table_complete() {
        assert_table(&QUADRANT_GLYPHS, 16);
    }

    #[test]
    fn sextant_table_complete() {
        assert_table(&SEXTANT_GLYPHS, 64);
    }

    #[test]
    fn octant_table_complete() {
        assert_table(&OCTANT_GLYPHS, 256);
    }

    #[test]
    fn tables_match_tier_geometry() {
        for tier in [Tier::Octant, Tier::Sextant, Tier::Quadrant, Tier::Half] {
            let (sx, sy) = tier.subcells();
            let patterns = 1usize << (sx * sy);
            assert_eq!(tier.glyphs().len(), patterns, "{:?}", tier.label());
        }
    }

    #[test]
    fn subcell_geometry_is_the_ladder() {
        assert_eq!(Tier::Octant.subcells(), (2, 4));
        assert_eq!(Tier::Sextant.subcells(), (2, 3));
        assert_eq!(Tier::Quadrant.subcells(), (2, 2));
        assert_eq!(Tier::Half.subcells(), (1, 2));
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(Tier::Octant.label(), "octants");
        assert_eq!(Tier::Sextant.label(), "sextants");
        assert_eq!(Tier::Quadrant.label(), "quadrants");
        assert_eq!(Tier::Half.label(), "half-blocks");
    }

    /// Spot-check a few octant patterns against the Unicode 16.0 assignment:
    /// the first dedicated octant, a reused quadrant, and the reused single
    /// corner cells.
    #[test]
    fn octant_reused_codepoints() {
        // Pattern 0b0000_0100 = subpixel (col0,row1) only = "BLOCK OCTANT-3".
        assert_eq!(OCTANT_GLYPHS[0b0000_0100], '\u{1CD00}');
        // Upper-left quadrant (col0 rows0..1) reuses U+2598.
        assert_eq!(OCTANT_GLYPHS[0b0000_0101], '\u{2598}');
        // Single top-left subpixel reuses LEFT HALF UPPER ONE QUARTER BLOCK.
        assert_eq!(OCTANT_GLYPHS[0b0000_0001], '\u{1CEA8}');
        // Single bottom-right subpixel reuses RIGHT HALF LOWER ONE QUARTER BLOCK.
        assert_eq!(OCTANT_GLYPHS[0b1000_0000], '\u{1CEA0}');
    }
}
