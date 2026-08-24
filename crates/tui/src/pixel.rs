//! The shared pixel rasterizer core: turn a scenes [`Canvas`] display list into
//! a flat RGB pixel image.
//!
//! A [`PixelBuffer`] holds a preallocated RGB pixel grid sized in physical
//! pixels (not terminal cells). Scenes rasterize normalized [`Primitive`]s into
//! it with exactly the same style semantics the cell mosaic uses — the palette
//! [`Slot`] resolution, intensity scaling and [`Blend`] behaviour are the shared
//! helpers from [`crate::mosaic`], so a shape drawn here resolves to the same
//! colour it would in a mosaic cell.
//!
//! Unlike the mosaic [`crate::mosaic::FrameBuffer`], there is no subpixel ladder
//! and no glyph clustering: the grid *is* the image. [`PixelBuffer::write_rgb8`]
//! flattens it to row-major RGB8 bytes, which an encoder ([`crate::kitty`], and
//! later a sixel encoder) turns into a terminal graphics protocol frame. The
//! core is deliberately encoder-agnostic.
//!
//! Text primitives are **not** pixel-rendered: like the mosaic path they are
//! collected as [`TextRun`]s so the terminal font draws them as real text,
//! layered above the image.
//!
//! # Allocation
//!
//! [`PixelBuffer::resize`] reallocates only when the pixel dimensions change.
//! After a warm-up frame [`rasterize`](PixelBuffer::rasterize),
//! [`clear`](PixelBuffer::clear) and [`write_rgb8`](PixelBuffer::write_rgb8)
//! allocate nothing: the pixel grid, the text-run vector and the text arena all
//! retain their capacity across `clear`.

use scia_scenes::{Blend, Canvas, Palette, Primitive, Style};

use crate::mosaic::{TextRun, clamp01, frac_px, slot_color, span_px, to_px, to_u8};

/// The default pixel budget: the largest total pixel count a graphics frame is
/// allowed before it is integer-downscaled to fit. Keeps per-frame compression
/// and transmission bounded regardless of terminal size.
pub const PIXEL_BUDGET: u32 = 500_000;

/// The fallback cell size in pixels `(height, width)` when the terminal did not
/// report one. A typical monospace cell is about twice as tall as it is wide.
pub const FALLBACK_CELL_PX: (u16, u16) = (20, 10);

/// The image pixel dimensions `(width, height)` for a `cols × rows` cell area at
/// the given cell size, integer-downscaled to fit `budget`.
///
/// The target is `cols * cell_width × rows * cell_height`. If that exceeds the
/// budget, both dimensions are divided by the smallest integer `k ≥ 1` for which
/// the downscaled image fits. The placement keys (`c=`/`r=`) then let the
/// terminal scale the transmitted image back up to the full cell area, so the
/// downscale only bounds transmission cost, never the on-screen size.
///
/// `cell_px` is `(height, width)`, matching
/// [`crate::CapabilityReport::cell_px`]. Returns `(0, 0)` for an empty area.
#[must_use]
pub fn image_dims(cols: u16, rows: u16, cell_px: (u16, u16), budget: u32) -> (u16, u16) {
    let (cell_h, cell_w) = cell_px;
    let w = u32::from(cols) * u32::from(cell_w);
    let h = u32::from(rows) * u32::from(cell_h);
    if w == 0 || h == 0 || budget == 0 {
        return (0, 0);
    }
    let mut k = 1u32;
    while u64::from(w / k) * u64::from(h / k) > u64::from(budget) {
        k += 1;
    }
    ((w / k).max(1) as u16, (h / k).max(1) as u16)
}

/// A preallocated RGB pixel grid plus the text runs collected from the last
/// rasterization pass. See the [module docs](self) for the coordinate system and
/// the allocation contract.
#[derive(Clone, Debug, Default)]
pub struct PixelBuffer {
    /// Width in pixels.
    px_w: u16,
    /// Height in pixels.
    px_h: u16,
    /// Row-major RGB pixels in `0.0..=1.0`, `px_w * px_h` of them. Kept in float
    /// so blending matches the mosaic path bit-for-bit; flattened to RGB8 only
    /// by [`write_rgb8`](Self::write_rgb8).
    pixels: Vec<[f32; 3]>,
    /// Terminal cell columns of the target area, used only to quantize text
    /// anchors (text is placed at a cell, not a pixel). Set via
    /// [`set_cells`](Self::set_cells).
    cell_cols: u16,
    /// Terminal cell rows of the target area; see [`cell_cols`](Self::cell_cols).
    cell_rows: u16,
    text_runs: Vec<TextRun>,
    text_arena: String,
}

impl PixelBuffer {
    /// An empty buffer with no pixels.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resize to `px_w × px_h`, (re)allocating the pixel grid only when the pixel
    /// dimensions actually change. Clears to black either way.
    pub fn resize(&mut self, px_w: u16, px_h: u16) {
        self.px_w = px_w;
        self.px_h = px_h;
        let len = px_w as usize * px_h as usize;
        if self.pixels.len() != len {
            self.pixels.clear();
            self.pixels.resize(len, [0.0; 3]);
            self.text_runs.clear();
            self.text_arena.clear();
        } else {
            self.clear();
        }
    }

    /// The pixel dimensions `(width, height)`.
    #[must_use]
    pub fn dims(&self) -> (u16, u16) {
        (self.px_w, self.px_h)
    }

    /// Set the terminal cell dimensions of the target area. Only affects where
    /// collected [`TextRun`]s anchor (text is drawn as real cells above the
    /// image); the pixel grid is unchanged.
    pub fn set_cells(&mut self, cols: u16, rows: u16) {
        self.cell_cols = cols;
        self.cell_rows = rows;
    }

    /// Reset every pixel to black and drop the collected text runs, retaining all
    /// capacity.
    pub fn clear(&mut self) {
        for p in &mut self.pixels {
            *p = [0.0; 3];
        }
        self.text_runs.clear();
        self.text_arena.clear();
    }

    /// Cross-fade `other` into this buffer: each pixel becomes
    /// `other * (1 - t) + self * t`, with `t` clamped to `0.0..=1.0`. Mirrors
    /// [`crate::mosaic::FrameBuffer::mix_from`] so a scene cross-fade behaves the
    /// same in the pixel path. A no-op when the grids differ in size.
    pub fn mix_from(&mut self, other: &PixelBuffer, t: f32) {
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
        let span = run.span();
        self.text_arena
            .get(span.start as usize..span.end as usize)
            .unwrap_or("")
    }

    /// Rasterize one canvas into the pixel grid with the given blend and layer
    /// intensity. Text primitives are collected as [`TextRun`]s instead of being
    /// drawn — the same split the mosaic path makes.
    pub fn rasterize(&mut self, canvas: &Canvas, palette: &Palette, blend: Blend, intensity: f32) {
        if self.px_w == 0 || self.px_h == 0 {
            return;
        }
        let layer = clamp01(intensity);
        for prim in canvas.primitives() {
            match *prim {
                Primitive::Bar { x, y, w, h, style } => {
                    let color = slot_color(palette, style, layer);
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
                    let color = slot_color(palette, style, layer);
                    self.draw_line(x0, y0, x1, y1, width, color, blend);
                }
                Primitive::Point { x, y, size, style } => {
                    let color = slot_color(palette, style, layer);
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

    /// Flatten the pixel grid to row-major RGB8 bytes in `out` (cleared first),
    /// `px_w * px_h * 3` of them. Retains `out`'s capacity, so a warm caller that
    /// pre-reserves the byte count allocates nothing.
    pub fn write_rgb8(&self, out: &mut Vec<u8>) {
        out.clear();
        for p in &self.pixels {
            let (r, g, b) = to_u8(*p);
            out.push(r);
            out.push(g);
            out.push(b);
        }
    }

    /// Blend `color` into the pixel at `(px, py)`. Identical blend semantics to
    /// the mosaic path.
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

    /// Fill a `cols × rows` field: each field cell paints its pixel rect with the
    /// slot colour scaled by the field value.
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
        let scia_scenes::Rgb(r, g, b) = palette.color(style.slot);
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

    /// Record a text run at the cell containing its normalized anchor. The cell
    /// grid is `px`-independent, so the anchor is quantized against the caller's
    /// cell dimensions — passed via [`set_cells`](Self::set_cells).
    fn collect_text(&mut self, x: f32, y: f32, text: &str, style: Style, layer: f32) {
        let cols = self.cell_cols.max(1);
        let rows = self.cell_rows.max(1);
        let cell_x = ((clamp01(x) * cols as f32) as u16).min(cols.saturating_sub(1));
        let cell_y = ((clamp01(y) * rows as f32) as u16).min(rows.saturating_sub(1));
        let start = self.text_arena.len() as u32;
        self.text_arena.push_str(text);
        let end = self.text_arena.len() as u32;
        self.text_runs.push(TextRun::new(
            cell_x,
            cell_y,
            style.slot,
            clamp01(style.intensity) * layer,
            start..end,
        ));
    }
}
