//! The scene presenter: drive a [`scia_scenes::Preset`]'s layers each frame and
//! rasterize them onto the mosaic ladder, then paint the result into a
//! `ratatui` buffer.
//!
//! [`ScenePresenter`] owns the render-side state a preset needs: the live layer
//! stack, one [`Params`] bag per layer for its feature mappings, a reusable
//! [`Canvas`], and the [`FrameBuffer`]/[`CellGrid`] that turn the canvas into
//! terminal cells. It is the only place in the crate that bridges the UI-free
//! [`crate::mosaic`] rasterizer to `ratatui`.

use std::fmt;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use scia_scenes::{
    Canvas, LayerInstance, Palette, Params, Preset, Rgb, builtin_preset, builtin_presets,
};

use crate::mosaic::{CellGrid, FrameBuffer, Tier};
use scia_core::FeatureSnapshot;

/// Renders a preset's scene layers into terminal cells on a mosaic [`Tier`].
///
/// Build one with [`ScenePresenter::from_preset`] (or the name-resolving
/// [`build_scene_presenter`]), call [`resize`] whenever the target area or tier
/// changes, then [`frame`] once per frame and [`draw`] to paint. The tier is a
/// parameter here; [`crate::default_tier`] picks it from a capability probe.
///
/// [`resize`]: ScenePresenter::resize
/// [`frame`]: ScenePresenter::frame
/// [`draw`]: ScenePresenter::draw
pub struct ScenePresenter {
    tier: Tier,
    fb: FrameBuffer,
    grid: CellGrid,
    canvas: Canvas,
    layers: Vec<LayerInstance>,
    /// One parameter bag per layer, driven by that layer's feature mappings.
    params: Vec<Params>,
    palette: Palette,
    cols: u16,
    rows: u16,
}

impl ScenePresenter {
    /// Build a presenter for `preset` at `tier`. The layers are instantiated
    /// once; [`resize`](Self::resize) later feeds the drawing aspect through the
    /// canvas without re-initializing them.
    #[must_use]
    pub fn from_preset(preset: &Preset, tier: Tier) -> Self {
        let layers = preset.instantiate(1.0);
        let mut params = Vec::with_capacity(layers.len());
        for layer in &layers {
            let mut p = Params::new();
            layer.mappings.seed(&mut p);
            params.push(p);
        }
        Self {
            tier,
            fb: FrameBuffer::new(),
            grid: CellGrid::new(),
            canvas: Canvas::new(1.0),
            layers,
            params,
            palette: preset.palette(),
            cols: 0,
            rows: 0,
        }
    }

    /// The active tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Switch tiers; the next [`resize`](Self::resize) reshapes the pixel grid.
    pub fn set_tier(&mut self, tier: Tier) {
        self.tier = tier;
    }

    /// Resize the pixel grid to a `cols × rows` cell area at the current tier
    /// and update the canvas aspect from the subcell geometry. Call this on a
    /// terminal resize or after [`set_tier`](Self::set_tier).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.fb.resize(cols, rows, self.tier);
        self.grid.resize(cols, rows);
        let (sx, sy) = self.tier.subcells();
        let pw = f32::from(cols) * f32::from(sx);
        let ph = f32::from(rows) * f32::from(sy);
        let aspect = if ph > 0.0 { pw / ph } else { 1.0 };
        self.canvas.set_aspect(aspect);
    }

    /// Advance and rasterize one frame from the newest features.
    ///
    /// Per layer: apply the feature mappings into the layer's params, update the
    /// scene, render it onto the shared canvas, and rasterize the canvas into
    /// the frame buffer with the layer's blend and intensity. The layers paint
    /// in order into one buffer; the encoded [`CellGrid`] is produced last.
    pub fn frame(&mut self, snap: &FeatureSnapshot, dt: f32) {
        self.fb.clear();
        let aspect = self.canvas.aspect();
        for (layer, params) in self.layers.iter_mut().zip(self.params.iter_mut()) {
            layer.mappings.apply(snap, dt, params);
            layer.scene.update(snap, dt);
            self.canvas.clear();
            self.canvas.set_aspect(aspect);
            layer.scene.render(&mut self.canvas);
            self.fb
                .rasterize(&self.canvas, &self.palette, layer.blend, layer.intensity);
        }
        self.fb.encode(&mut self.grid);
    }

    /// Paint the encoded cells into `buf` over `area`, then the collected text
    /// runs on top as real terminal text.
    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        for cy in 0..self.grid.rows().min(area.height) {
            for cx in 0..self.grid.cols().min(area.width) {
                let Some(cell) = self.grid.cell(cx, cy) else {
                    continue;
                };
                let Some(dst) = buf.cell_mut((area.x + cx, area.y + cy)) else {
                    continue;
                };
                dst.set_char(cell.ch)
                    .set_style(Style::new().fg(to_color(cell.fg)).bg(to_color(cell.bg)));
            }
        }

        for run in self.fb.text_runs() {
            if run.cell_x >= area.width || run.cell_y >= area.height {
                continue;
            }
            let text = self.fb.run_text(run);
            if text.is_empty() {
                continue;
            }
            let Rgb(r, g, b) = self.palette.color(run.slot);
            let s = run.intensity.clamp(0.0, 1.0);
            let fg = to_color((
                (f32::from(r) * s).round() as u8,
                (f32::from(g) * s).round() as u8,
                (f32::from(b) * s).round() as u8,
            ));
            let max = area.width.saturating_sub(run.cell_x) as usize;
            buf.set_stringn(
                area.x + run.cell_x,
                area.y + run.cell_y,
                text,
                max,
                Style::new().fg(fg),
            );
        }
    }
}

/// Convert an RGB triple to a `ratatui` truecolor.
#[inline]
fn to_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// A failure to build a [`ScenePresenter`] from a `--scene` name: the preset was
/// unknown, or it existed but failed validation. The [`Display`] is a
/// user-facing message; for an unknown name it lists the available presets.
#[derive(Clone, Debug)]
pub struct SceneError {
    message: String,
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SceneError {}

/// Build a scene presenter for a built-in preset `name` at `tier`.
///
/// This is the seam the CLI (and the tests) drive: an unknown name yields a
/// [`SceneError`] naming the available presets, and a present-but-invalid preset
/// yields a [`SceneError`] carrying the validator's message. Neither panics.
///
/// # Errors
/// [`SceneError`] when `name` is not a built-in preset, or when it is one but
/// fails to parse/validate.
pub fn build_scene_presenter(name: &str, tier: Tier) -> Result<ScenePresenter, SceneError> {
    match builtin_preset(name) {
        Some(Ok(preset)) => Ok(ScenePresenter::from_preset(&preset, tier)),
        Some(Err(err)) => Err(SceneError {
            message: format!("invalid scene preset '{name}': {err}"),
        }),
        None => {
            let names: Vec<&str> = builtin_presets().iter().map(|(n, _)| *n).collect();
            Err(SceneError {
                message: format!(
                    "unknown scene preset '{name}'; available presets: {}",
                    names.join(", ")
                ),
            })
        }
    }
}
