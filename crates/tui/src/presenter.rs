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
/// The cross-fade duration when a preset is hot-swapped, in seconds (300 ms).
const FADE_SECS: f32 = 0.3;

/// A cross-fade in progress: how far into [`FADE_SECS`] it has advanced.
#[derive(Clone, Copy, Debug)]
struct Fade {
    /// Seconds elapsed since the swap began.
    elapsed: f32,
}

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
    /// The outgoing layers during a cross-fade; empty when no fade is active.
    outgoing: Vec<LayerInstance>,
    /// One parameter bag per outgoing layer.
    outgoing_params: Vec<Params>,
    /// The outgoing palette during a cross-fade.
    outgoing_palette: Palette,
    /// Second frame buffer, holding the outgoing layers' pixels while fading.
    /// Allocated on the first swap and reused thereafter.
    fb_out: FrameBuffer,
    /// Set once the first swap has allocated [`fb_out`](Self::fb_out), so
    /// [`resize`](Self::resize) keeps it in step with the primary buffer.
    fb_out_ready: bool,
    /// The active cross-fade, if any.
    fade: Option<Fade>,
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
            outgoing: Vec::new(),
            outgoing_params: Vec::new(),
            outgoing_palette: preset.palette(),
            fb_out: FrameBuffer::new(),
            fb_out_ready: false,
            fade: None,
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
        // Keep the outgoing buffer in step once a swap has allocated it, so a
        // fade that spans a resize mixes matching grids.
        if self.fb_out_ready {
            self.fb_out.resize(cols, rows, self.tier);
        }
        let (sx, sy) = self.tier.subcells();
        let pw = f32::from(cols) * f32::from(sx);
        let ph = f32::from(rows) * f32::from(sy);
        let aspect = if ph > 0.0 { pw / ph } else { 1.0 };
        self.canvas.set_aspect(aspect);
    }

    /// Whether a preset cross-fade is currently in progress.
    #[must_use]
    pub fn is_fading(&self) -> bool {
        self.fade.is_some()
    }

    /// Swap in a new preset, carrying scene continuity and starting a 300 ms
    /// cross-fade from the old layers to the new.
    ///
    /// The incoming layers are instantiated at the current drawing aspect. For
    /// each incoming layer whose scene id matches the outgoing layer in the same
    /// position, the outgoing scene's [`state`](scia_scenes::Scene::state) is
    /// carried into the incoming scene via
    /// [`restore`](scia_scenes::Scene::restore), so animation does not visibly
    /// reset. The current layers move to the outgoing slot and both sets render
    /// each frame until the fade completes, after which the outgoing layers are
    /// dropped.
    pub fn swap_preset(&mut self, preset: &Preset) {
        let aspect = self.canvas.aspect();
        let mut incoming = preset.instantiate(aspect);

        // Carry scene continuity pairwise by position when the scene ids match.
        for (i, layer) in incoming.iter_mut().enumerate() {
            if let Some(old) = self.layers.get(i) {
                if old.scene.id() == layer.scene.id() {
                    layer.scene.restore(old.scene.state());
                }
            }
        }

        // One seeded parameter bag per incoming layer.
        let mut incoming_params = Vec::with_capacity(incoming.len());
        for layer in &incoming {
            let mut p = Params::new();
            layer.mappings.seed(&mut p);
            incoming_params.push(p);
        }

        // Move the current layers to the outgoing slot; the incoming ones become
        // current.
        self.outgoing = std::mem::replace(&mut self.layers, incoming);
        self.outgoing_params = std::mem::replace(&mut self.params, incoming_params);
        self.outgoing_palette = self.palette;
        self.palette = preset.palette();

        // Allocate (or reuse) the outgoing buffer at the current geometry.
        self.fb_out.resize(self.cols, self.rows, self.tier);
        self.fb_out_ready = true;
        self.fade = Some(Fade { elapsed: 0.0 });
    }

    /// Advance and rasterize one frame from the newest features.
    ///
    /// Per layer: apply the feature mappings into the layer's params, update the
    /// scene, render it onto the shared canvas, and rasterize the canvas into
    /// the frame buffer with the layer's blend and intensity. The layers paint
    /// in order into one buffer; the encoded [`CellGrid`] is produced last.
    pub fn frame(&mut self, snap: &FeatureSnapshot, dt: f32) {
        // The incoming (or, without a fade, the only) layers rasterize into the
        // primary buffer.
        Self::rasterize_layers(
            &mut self.layers,
            &mut self.params,
            &self.palette,
            &mut self.canvas,
            &mut self.fb,
            snap,
            dt,
        );

        if let Some(fade) = self.fade.as_mut() {
            fade.elapsed += dt.max(0.0);
            let elapsed = fade.elapsed;
            let t = (elapsed / FADE_SECS).clamp(0.0, 1.0);
            // The outgoing layers rasterize into the second buffer, then mix
            // under the incoming ones: `out * (1 - t) + in * t`.
            Self::rasterize_layers(
                &mut self.outgoing,
                &mut self.outgoing_params,
                &self.outgoing_palette,
                &mut self.canvas,
                &mut self.fb_out,
                snap,
                dt,
            );
            self.fb.mix_from(&self.fb_out, t);
            if elapsed >= FADE_SECS {
                // Fade complete: drop the outgoing layers, keeping capacity.
                self.fade = None;
                self.outgoing.clear();
                self.outgoing_params.clear();
            }
        }

        self.fb.encode(&mut self.grid);
    }

    /// Advance, render and rasterize a layer stack into `fb`. Per layer: fold the
    /// feature mappings into `params`, update the scene, render it onto the
    /// shared `canvas`, and rasterize with the layer's blend and intensity.
    /// Free-standing (not `&mut self`) so a fade can drive the incoming and
    /// outgoing stacks with disjoint borrows of the presenter's fields.
    #[allow(clippy::too_many_arguments)]
    fn rasterize_layers(
        layers: &mut [LayerInstance],
        params: &mut [Params],
        palette: &Palette,
        canvas: &mut Canvas,
        fb: &mut FrameBuffer,
        snap: &FeatureSnapshot,
        dt: f32,
    ) {
        fb.clear();
        let aspect = canvas.aspect();
        for (layer, p) in layers.iter_mut().zip(params.iter_mut()) {
            layer.mappings.apply(snap, dt, p);
            layer.scene.update(snap, dt);
            canvas.clear();
            canvas.set_aspect(aspect);
            layer.scene.render(canvas);
            fb.rasterize(canvas, palette, layer.blend, layer.intensity);
        }
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
