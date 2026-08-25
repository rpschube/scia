//! Objective per-hop stats derived from a scene's [`Canvas`] display list.
//!
//! A [`CanvasProbe`] rasterises each frame's primitives onto a fixed
//! normalised-coordinate grid ([`GRID_W`] × [`GRID_H`] cells over the unit
//! square, so a touched-cell count is a true canvas-area fraction) and reads five
//! scalars per frame:
//!
//! * **prims** — the primitive count.
//! * **coverage** — the fraction of grid cells any primitive touches.
//! * **brightness** — the mean accumulated cell intensity (clamped per cell).
//! * **motion** — the mean absolute per-cell brightness change from the previous
//!   frame (`0.0` on the first frame).
//! * **chroma** — the colourfulness (HSV saturation) of the intensity-weighted
//!   mean drawn colour, resolved through the host [`Palette`].
//!
//! The probe also exposes that intensity-weighted mean colour so the palette
//! churn metric can measure how fast it moves between frames.
//!
//! Rasterisation is deliberately coarse and approximate — this is an objective
//! *relative* measure for A/B comparison, not a renderer. Every value is a pure
//! function of the display list, so a replay is deterministic.

use scia_scenes::{Canvas as SceneCanvas, PALETTE_SLOTS, Palette, Primitive, Rgb};

use scia_telemetry::record::CanvasStats as CanvasRec;

/// Grid width (cells across the normalised unit square).
pub const GRID_W: usize = 64;
/// Grid height (cells down the normalised unit square).
pub const GRID_H: usize = 64;

const CELLS: usize = GRID_W * GRID_H;

/// A cell is "touched" once its accumulated intensity clears this.
const TOUCH_EPS: f32 = 1e-4;

/// A stateful probe over a stream of frames from one scene: it holds the
/// previous frame's grid so it can compute frame-to-frame motion.
pub struct CanvasProbe {
    palette: Palette,
    aspect: f32,
    grid: Vec<f32>,
    prev: Vec<f32>,
    have_prev: bool,
}

impl CanvasProbe {
    /// A probe that resolves palette slots through `palette` and treats the
    /// drawing surface as `aspect` wide (width / height) so round primitives
    /// rasterise round.
    #[must_use]
    pub fn new(palette: Palette, aspect: f32) -> Self {
        Self {
            palette,
            aspect: if aspect.is_finite() && aspect > 0.0 {
                aspect
            } else {
                1.0
            },
            grid: vec![0.0; CELLS],
            prev: vec![0.0; CELLS],
            have_prev: false,
        }
    }

    /// Rasterise one frame and return its stats plus the intensity-weighted mean
    /// drawn colour (normalised RGB in `0.0..=1.0`), which the palette-churn
    /// metric consumes.
    pub fn probe(&mut self, canvas: &SceneCanvas) -> (CanvasRec, [f32; 3]) {
        for cell in &mut self.grid {
            *cell = 0.0;
        }

        // Colour accumulation, weighted by (area touched × intensity).
        let mut color_w = 0.0f64;
        let mut color_rgb = [0.0f64; 3];

        for prim in canvas.primitives() {
            let (style, touched) = self.rasterize(prim, canvas);
            if touched == 0 {
                continue;
            }
            let area = touched as f64 / CELLS as f64;
            let weight = area * f64::from(style_intensity(prim));
            if weight > 0.0 {
                let Rgb(r, g, b) = self.palette.color(style);
                color_rgb[0] += weight * f64::from(r) / 255.0;
                color_rgb[1] += weight * f64::from(g) / 255.0;
                color_rgb[2] += weight * f64::from(b) / 255.0;
                color_w += weight;
            }
        }

        // Reduce the grid.
        let mut touched = 0u32;
        let mut bright_sum = 0.0f64;
        let mut motion_sum = 0.0f64;
        for i in 0..CELLS {
            let v = self.grid[i].min(1.0);
            if v > TOUCH_EPS {
                touched += 1;
            }
            bright_sum += f64::from(v);
            if self.have_prev {
                motion_sum += f64::from((v - self.prev[i]).abs());
            }
        }

        let coverage = touched as f32 / CELLS as f32;
        let brightness = (bright_sum / CELLS as f64) as f32;
        let motion = if self.have_prev {
            (motion_sum / CELLS as f64) as f32
        } else {
            0.0
        };

        let mean_rgb = if color_w > 0.0 {
            [
                (color_rgb[0] / color_w) as f32,
                (color_rgb[1] / color_w) as f32,
                (color_rgb[2] / color_w) as f32,
            ]
        } else {
            [0.0, 0.0, 0.0]
        };
        let chroma = saturation(mean_rgb);

        // Roll the grid forward.
        std::mem::swap(&mut self.grid, &mut self.prev);
        self.have_prev = true;

        (
            CanvasRec {
                prims: canvas.primitives().len() as u32,
                coverage,
                motion,
                brightness,
                chroma,
            },
            mean_rgb,
        )
    }

    /// Mark the cells a primitive covers (accumulating its intensity) and return
    /// its palette slot plus the number of cells it touched.
    fn rasterize(&mut self, prim: &Primitive, canvas: &SceneCanvas) -> (u8, usize) {
        match *prim {
            Primitive::Bar { x, y, w, h, style } => {
                (style.slot, self.fill_rect(x, y, w, h, style.intensity))
            }
            Primitive::Point { x, y, size, style } => {
                (style.slot, self.fill_disc(x, y, size, style.intensity))
            }
            Primitive::Line {
                x0,
                y0,
                x1,
                y1,
                width,
                style,
            } => (
                style.slot,
                self.fill_line(x0, y0, x1, y1, width, style.intensity),
            ),
            Primitive::Field {
                cols, rows, style, ..
            } => {
                let values = canvas.field_of(prim).unwrap_or(&[]);
                (
                    style.slot,
                    self.fill_field(cols, rows, values, style.intensity),
                )
            }
            Primitive::Text {
                x, y, len, style, ..
            } => {
                // A text run has no intrinsic size on the abstract canvas; treat
                // it as a small box whose width grows with the byte length.
                let w = (len as f32 * 0.008).min(0.5);
                let h = 0.04;
                (
                    style.slot,
                    self.fill_rect(x - w * 0.5, y - h * 0.5, w, h, style.intensity),
                )
            }
        }
    }

    fn add(&mut self, cx: usize, cy: usize, intensity: f32) {
        let idx = cy * GRID_W + cx;
        self.grid[idx] += intensity;
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, intensity: f32) -> usize {
        let x0 = x.clamp(0.0, 1.0);
        let y0 = y.clamp(0.0, 1.0);
        let x1 = (x + w).clamp(0.0, 1.0);
        let y1 = (y + h).clamp(0.0, 1.0);
        let cx0 = (x0 * GRID_W as f32).floor() as usize;
        let cy0 = (y0 * GRID_H as f32).floor() as usize;
        let cx1 = ((x1 * GRID_W as f32).ceil() as usize).min(GRID_W);
        let cy1 = ((y1 * GRID_H as f32).ceil() as usize).min(GRID_H);
        let mut touched = 0;
        for cy in cy0..cy1 {
            for cx in cx0..cx1 {
                self.add(cx, cy, intensity);
                touched += 1;
            }
        }
        touched
    }

    fn fill_disc(&mut self, x: f32, y: f32, size: f32, intensity: f32) -> usize {
        // `size` is a diameter as a fraction of canvas height. Keep it round on
        // the physical surface: the x-radius shrinks with the aspect ratio.
        let ry = (size * 0.5).max(0.5 / GRID_H as f32);
        let rx = (ry / self.aspect).max(0.5 / GRID_W as f32);
        let cx0 = (((x - rx) * GRID_W as f32).floor() as isize).max(0) as usize;
        let cy0 = (((y - ry) * GRID_H as f32).floor() as isize).max(0) as usize;
        let cx1 = (((x + rx) * GRID_W as f32).ceil() as usize).min(GRID_W);
        let cy1 = (((y + ry) * GRID_H as f32).ceil() as usize).min(GRID_H);
        let mut touched = 0;
        for cy in cy0..cy1 {
            let py = (cy as f32 + 0.5) / GRID_H as f32;
            for cx in cx0..cx1 {
                let px = (cx as f32 + 0.5) / GRID_W as f32;
                let dx = (px - x) / rx;
                let dy = (py - y) / ry;
                if dx * dx + dy * dy <= 1.0 {
                    self.add(cx, cy, intensity);
                    touched += 1;
                }
            }
        }
        touched
    }

    fn fill_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        intensity: f32,
    ) -> usize {
        // Stamp small discs along the segment; the half-width is a fraction of
        // canvas height. A boolean mask avoids double-counting the same cell.
        let hw = (width * 0.5).max(0.5 / GRID_H as f32);
        let len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        let steps = ((len * GRID_W.max(GRID_H) as f32).ceil() as usize).max(1);
        let mut touched = 0;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let x = x0 + (x1 - x0) * t;
            let y = y0 + (y1 - y0) * t;
            touched += self.stamp_disc_masked(x, y, hw, intensity);
        }
        touched
    }

    /// Stamp a disc but only add to cells not already at/above `intensity` this
    /// call — approximated by skipping cells whose accumulated value already
    /// covers this stamp. To keep it simple and allocation-light we just add,
    /// but count each covered cell once via a coarse guard on the pre-add value.
    fn stamp_disc_masked(&mut self, x: f32, y: f32, r: f32, intensity: f32) -> usize {
        let rx = (r / self.aspect).max(0.5 / GRID_W as f32);
        let ry = r.max(0.5 / GRID_H as f32);
        let cx0 = (((x - rx) * GRID_W as f32).floor() as isize).max(0) as usize;
        let cy0 = (((y - ry) * GRID_H as f32).floor() as isize).max(0) as usize;
        let cx1 = (((x + rx) * GRID_W as f32).ceil() as usize).min(GRID_W);
        let cy1 = (((y + ry) * GRID_H as f32).ceil() as usize).min(GRID_H);
        let mut touched = 0;
        for cy in cy0..cy1 {
            let py = (cy as f32 + 0.5) / GRID_H as f32;
            for cx in cx0..cx1 {
                let px = (cx as f32 + 0.5) / GRID_W as f32;
                let dx = (px - x) / rx;
                let dy = (py - y) / ry;
                if dx * dx + dy * dy <= 1.0 {
                    let idx = cy * GRID_W + cx;
                    if self.grid[idx] < TOUCH_EPS {
                        touched += 1;
                    }
                    self.grid[idx] += intensity;
                }
            }
        }
        touched
    }

    fn fill_field(&mut self, cols: u16, rows: u16, values: &[f32], intensity: f32) -> usize {
        if cols == 0 || rows == 0 {
            return 0;
        }
        // A field spans the whole canvas; sample the field value under each grid
        // cell (nearest cell) and add value × intensity.
        let mut touched = 0;
        for cy in 0..GRID_H {
            let fy = ((cy as f32 + 0.5) / GRID_H as f32 * rows as f32) as usize;
            let fy = fy.min(rows as usize - 1);
            for cx in 0..GRID_W {
                let fx = ((cx as f32 + 0.5) / GRID_W as f32 * cols as f32) as usize;
                let fx = fx.min(cols as usize - 1);
                let v = values.get(fy * cols as usize + fx).copied().unwrap_or(0.0);
                let contrib = v * intensity;
                if contrib > TOUCH_EPS {
                    self.add(cx, cy, contrib);
                    touched += 1;
                }
            }
        }
        touched
    }
}

/// The intensity carried by a primitive's style.
fn style_intensity(prim: &Primitive) -> f32 {
    match prim {
        Primitive::Bar { style, .. }
        | Primitive::Line { style, .. }
        | Primitive::Point { style, .. }
        | Primitive::Field { style, .. }
        | Primitive::Text { style, .. } => style.intensity,
    }
}

/// HSV saturation of a normalised RGB colour: `(max - min) / max`, or `0` for
/// black. This is the "how far from grey" measure the palette-vibrancy verdict
/// cares about.
#[must_use]
pub fn saturation(rgb: [f32; 3]) -> f32 {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    if max <= 0.0 { 0.0 } else { (max - min) / max }
}

/// Palette slot count re-exported so callers can size slot histograms.
pub const SLOTS: usize = PALETTE_SLOTS;

#[cfg(test)]
mod tests {
    use super::*;
    use scia_scenes::Style;

    fn probe() -> CanvasProbe {
        CanvasProbe::new(Palette::default_dark(), 1.0)
    }

    #[test]
    fn full_bar_covers_everything() {
        let mut p = probe();
        let mut c = SceneCanvas::new(1.0);
        c.bar(0.0, 0.0, 1.0, 1.0, Style::new(2, 1.0));
        let (rec, _) = p.probe(&c);
        assert_eq!(rec.prims, 1);
        assert!(
            (rec.coverage - 1.0).abs() < 1e-6,
            "coverage {}",
            rec.coverage
        );
        assert!((rec.brightness - 1.0).abs() < 1e-6);
        assert_eq!(rec.motion, 0.0, "first frame has no motion");
    }

    #[test]
    fn half_bar_covers_half() {
        let mut p = probe();
        let mut c = SceneCanvas::new(1.0);
        c.bar(0.0, 0.0, 1.0, 0.5, Style::new(2, 1.0));
        let (rec, _) = p.probe(&c);
        assert!(
            (rec.coverage - 0.5).abs() < 0.02,
            "coverage {}",
            rec.coverage
        );
    }

    #[test]
    fn quarter_bar_covers_quarter() {
        let mut p = probe();
        let mut c = SceneCanvas::new(1.0);
        c.bar(0.0, 0.0, 0.5, 0.5, Style::new(2, 1.0));
        let (rec, _) = p.probe(&c);
        assert!(
            (rec.coverage - 0.25).abs() < 0.02,
            "coverage {}",
            rec.coverage
        );
    }

    #[test]
    fn static_frames_have_zero_motion() {
        let mut p = probe();
        let mut c = SceneCanvas::new(1.0);
        c.bar(0.1, 0.1, 0.3, 0.3, Style::new(3, 0.8));
        let _ = p.probe(&c);
        let (rec, _) = p.probe(&c);
        assert_eq!(rec.motion, 0.0, "identical frame → zero motion");
    }

    #[test]
    fn moving_bar_has_motion() {
        let mut p = probe();
        let mut a = SceneCanvas::new(1.0);
        a.bar(0.0, 0.0, 0.2, 1.0, Style::new(3, 1.0));
        let _ = p.probe(&a);
        let mut b = SceneCanvas::new(1.0);
        b.bar(0.8, 0.0, 0.2, 1.0, Style::new(3, 1.0));
        let (rec, _) = p.probe(&b);
        assert!(rec.motion > 0.0, "a moved bar must register motion");
    }

    #[test]
    fn grey_palette_has_low_chroma_vibrant_has_high() {
        // Slot 6 is a mid neutral (grey); slot 2 is cyan.
        let mut p = probe();
        let mut grey = SceneCanvas::new(1.0);
        grey.bar(0.0, 0.0, 1.0, 1.0, Style::new(6, 1.0));
        let (g, _) = p.probe(&grey);

        let mut p2 = probe();
        let mut vivid = SceneCanvas::new(1.0);
        vivid.bar(0.0, 0.0, 1.0, 1.0, Style::new(2, 1.0));
        let (v, _) = p2.probe(&vivid);

        assert!(g.chroma < 0.2, "grey chroma {}", g.chroma);
        assert!(v.chroma > 0.5, "cyan chroma {}", v.chroma);
    }
}
