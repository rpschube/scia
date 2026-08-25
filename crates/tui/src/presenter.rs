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
    Blend, Canvas, LayerInstance, MapEntry, Palette, ParamSpec, Params, Preset, Rgb,
    catalog_scene_info, catalog_scenes, scene_preset,
};

use crate::mosaic::{CellGrid, FrameBuffer, TextRun, Tier};
use crate::pixel::{PIXEL_BUDGET, PixelBuffer, image_dims, image_downscale};
use scia_core::FeatureSnapshot;

/// Which presenter drives the scene body: the cell mosaic on a [`Tier`], the
/// kitty graphics pixel image, or the sixel graphics pixel image.
///
/// The kitty and sixel variants carry the terminal's cell size in pixels
/// `(height, width)`, used to size the transmitted image; the mosaic variant
/// carries the subpixel ladder rung. The two pixel presenters share the whole
/// render path ([`PixelBuffer`]); they differ only in how the run loop encodes
/// and writes the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenterMode {
    /// The cell-mosaic rasterizer at the given ladder rung.
    Mosaic(Tier),
    /// The kitty graphics pixel presenter, sized from the reported cell size.
    Kitty {
        /// Terminal cell size in pixels `(height, width)`.
        cell_px: (u16, u16),
    },
    /// The sixel graphics pixel presenter, sized from the reported cell size.
    Sixel {
        /// Terminal cell size in pixels `(height, width)`.
        cell_px: (u16, u16),
    },
}

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

/// A palette cross-fade in progress: the endpoints and how far into [`FADE_SECS`]
/// it has advanced. Unlike a preset [`Fade`], this interpolates the palette
/// itself and keeps the current layers, so a track's colours ease in without a
/// hard snap and without duplicating the scene stack.
#[derive(Clone, Copy, Debug)]
struct PaletteFade {
    /// The palette the fade started from.
    from: Palette,
    /// The palette the fade is easing toward.
    to: Palette,
    /// Seconds elapsed since the palette fade began.
    elapsed: f32,
}

pub struct ScenePresenter {
    mode: PresenterMode,
    fb: FrameBuffer,
    grid: CellGrid,
    /// The pixel image, in kitty mode. Empty (zero-sized) in mosaic mode.
    px: PixelBuffer,
    /// The outgoing pixel image during a cross-fade, in kitty mode.
    px_out: PixelBuffer,
    /// Set once the first swap has allocated [`px_out`](Self::px_out).
    px_out_ready: bool,
    /// The flattened RGB8 image the kitty encoder consumes, refreshed by
    /// [`frame`](Self::frame). Empty in mosaic mode.
    rgb8: Vec<u8>,
    /// The current image size in pixels `(width, height)`, in a pixel mode.
    img_w: u16,
    img_h: u16,
    /// The integer pixel-repeat factor for the sixel emit (`≥ 1`); the sixel
    /// image is rasterized at `(img_w, img_h)` and repeated `×img_k` on emit to
    /// cover the body area. Unused in kitty and mosaic modes.
    img_k: u16,
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
    /// The active palette cross-fade, if any.
    palette_fade: Option<PaletteFade>,
}

impl ScenePresenter {
    /// Build a presenter for `preset` at `tier`. The layers are instantiated
    /// once; [`resize`](Self::resize) later feeds the drawing aspect through the
    /// canvas without re-initializing them.
    #[must_use]
    pub fn from_preset(preset: &Preset, tier: Tier) -> Self {
        Self::with_mode(preset, PresenterMode::Mosaic(tier))
    }

    /// Build a presenter for `preset` in `mode` (cell mosaic or kitty graphics).
    /// The mosaic constructor [`from_preset`](Self::from_preset) is the common
    /// case; this is the general form the CLI drives once a presenter mode has
    /// been selected.
    #[must_use]
    pub fn with_mode(preset: &Preset, mode: PresenterMode) -> Self {
        let layers = preset.instantiate(1.0);
        let mut params = Vec::with_capacity(layers.len());
        for layer in &layers {
            let mut p = Params::new();
            layer.mappings.seed(&mut p);
            params.push(p);
        }
        Self {
            mode,
            fb: FrameBuffer::new(),
            grid: CellGrid::new(),
            px: PixelBuffer::new(),
            px_out: PixelBuffer::new(),
            px_out_ready: false,
            rgb8: Vec::new(),
            img_w: 0,
            img_h: 0,
            img_k: 1,
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
            palette_fade: None,
        }
    }

    /// Push an external text value to every layer's scene.
    ///
    /// Forwards to [`Scene::apply_text`](scia_scenes::Scene::apply_text) on each
    /// current layer (and any outgoing layer still cross-fading), so a scene that
    /// renders host text — for example `verso` — rebuilds when the value changes.
    /// The host calls this only on a change (e.g. the now-playing track line),
    /// never per frame. Scenes that do not render text ignore it (the trait
    /// default is a no-op).
    pub fn set_text(&mut self, key: &str, value: &str) {
        for layer in self.layers.iter_mut().chain(self.outgoing.iter_mut()) {
            layer.scene.apply_text(key, value);
        }
    }

    /// The first layer's scene id, if any layer exists. The tuning strip drives
    /// the first layer, so its manifest, values and mappings are all read from
    /// this scene.
    #[must_use]
    pub fn layer0_scene_id(&self) -> Option<&'static str> {
        self.layers.first().map(|l| l.scene.id())
    }

    /// The first layer's scene parameter manifest, or an empty slice when there
    /// is no layer or the scene is unknown.
    #[must_use]
    pub fn layer0_specs(&self) -> &'static [ParamSpec] {
        self.layer0_scene_id()
            .and_then(catalog_scene_info)
            .map_or(&[], |i| i.params)
    }

    /// The current value of a first-layer parameter: the layer-0 params bag
    /// value, falling back to the manifest default when the bag has no entry
    /// (and `0.0` when the key is not a manifest key at all).
    #[must_use]
    pub fn layer0_value(&self, key: &str) -> f32 {
        self.params
            .first()
            .and_then(|p| p.get(key))
            .or_else(|| {
                self.layer0_specs()
                    .iter()
                    .find(|s| s.key == key)
                    .map(|s| s.default)
            })
            .unwrap_or(0.0)
    }

    /// Whether a first-layer parameter is driven by a `[map]` entry. Derived
    /// from the layer's mapping set: seeding a fresh bag inserts exactly the
    /// mapping target keys, so a key present afterwards is mapped. A mapped key's
    /// live adjustment is overwritten each frame by the mapping, so the strip
    /// annotates it.
    #[must_use]
    pub fn layer0_mapped(&self, key: &str) -> bool {
        let Some(layer) = self.layers.first() else {
            return false;
        };
        let mut probe = Params::new();
        layer.mappings.seed(&mut probe);
        probe.get(key).is_some()
    }

    /// Set a first-layer parameter in the layer-0 params bag, so the value takes
    /// effect on the next frame's [`Scene::apply_params`](scia_scenes::Scene::apply_params).
    /// A `[map]`-driven key is overwritten by its mapping each frame (the write
    /// still lands as the base an unmapped read would see); an unmapped key
    /// changes the running scene on the same frame.
    pub fn set_param(&mut self, key: &str, v: f32) {
        if let Some(p) = self.params.first_mut() {
            p.set(key, v);
        }
    }

    /// The first layer's `[map]` entries as public [`MapEntry`] values, for the
    /// expression-mapping overlay to list. Empty when there is no layer. The
    /// mappings ride the first layer (see [`Preset::instantiate`]).
    #[must_use]
    pub fn layer0_mapping_entries(&self) -> Vec<MapEntry> {
        self.layers
            .first()
            .map(|l| l.mappings.entries_view())
            .unwrap_or_default()
    }

    /// Swap `entry` into the first layer's mapping set in place of the row with
    /// the same target, so an edited expression previews on the next frame.
    /// Returns whether a matching row was found. A no-op with no first layer.
    pub fn replace_layer0_mapping(&mut self, entry: MapEntry) -> bool {
        self.layers
            .first_mut()
            .map(|l| l.mappings.replace(entry))
            .unwrap_or(false)
    }

    /// The active tier. In kitty mode there is no ladder rung, so the default
    /// tier is reported; use [`mode`](Self::mode) to tell the modes apart.
    #[must_use]
    pub fn tier(&self) -> Tier {
        match self.mode {
            PresenterMode::Mosaic(tier) => tier,
            PresenterMode::Kitty { .. } | PresenterMode::Sixel { .. } => Tier::default(),
        }
    }

    /// The presenter mode: cell mosaic on a tier, or kitty graphics.
    #[must_use]
    pub fn mode(&self) -> PresenterMode {
        self.mode
    }

    /// The label shown on the debug line: the ladder rung in mosaic mode, or
    /// `"kitty"` for the graphics presenter.
    #[must_use]
    pub fn mode_label(&self) -> &'static str {
        match self.mode {
            PresenterMode::Mosaic(tier) => tier.label(),
            PresenterMode::Kitty { .. } => "kitty",
            PresenterMode::Sixel { .. } => "sixel",
        }
    }

    /// Switch to a mosaic tier; the next [`resize`](Self::resize) reshapes the
    /// grid. Leaves kitty mode if it was active.
    pub fn set_tier(&mut self, tier: Tier) {
        self.mode = PresenterMode::Mosaic(tier);
    }

    /// The kitty image bytes (row-major RGB8) refreshed by the last
    /// [`frame`](Self::frame). Empty in mosaic mode.
    #[must_use]
    pub fn image_rgb8(&self) -> &[u8] {
        &self.rgb8
    }

    /// The kitty image size in pixels `(width, height)`.
    #[must_use]
    pub fn image_px(&self) -> (u16, u16) {
        (self.img_w, self.img_h)
    }

    /// The kitty on-screen placement in terminal cells `(cols, rows)`.
    #[must_use]
    pub fn image_cells(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// The sixel pixel-repeat factor (`≥ 1`): the budgeted image at
    /// [`image_px`](Self::image_px) is repeated `×k` on emit to cover the body
    /// area. `1` outside sixel mode.
    #[must_use]
    pub fn image_k(&self) -> u16 {
        self.img_k
    }

    /// Resize the pixel grid to a `cols × rows` cell area at the current tier
    /// and update the canvas aspect from the subcell geometry. Call this on a
    /// terminal resize or after [`set_tier`](Self::set_tier).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        match self.mode {
            PresenterMode::Mosaic(tier) => {
                self.fb.resize(cols, rows, tier);
                self.grid.resize(cols, rows);
                // Keep the outgoing buffer in step once a swap has allocated it,
                // so a fade that spans a resize mixes matching grids.
                if self.fb_out_ready {
                    self.fb_out.resize(cols, rows, tier);
                }
                let (sx, sy) = tier.subcells();
                let pw = f32::from(cols) * f32::from(sx);
                let ph = f32::from(rows) * f32::from(sy);
                let aspect = if ph > 0.0 { pw / ph } else { 1.0 };
                self.canvas.set_aspect(aspect);
            }
            PresenterMode::Kitty { cell_px } | PresenterMode::Sixel { cell_px } => {
                let (w, h) = image_dims(cols, rows, cell_px, PIXEL_BUDGET);
                // The sixel emit has no terminal-side scaling, so it repeats the
                // budgeted image by the same integer factor the downscale used;
                // kitty leaves scaling to the terminal and ignores this.
                self.img_k = image_downscale(cols, rows, cell_px, PIXEL_BUDGET).max(1) as u16;
                self.img_w = w;
                self.img_h = h;
                self.px.resize(w, h);
                self.px.set_cells(cols, rows);
                if self.px_out_ready {
                    self.px_out.resize(w, h);
                    self.px_out.set_cells(cols, rows);
                }
                // Pre-grow the RGB8 scratch so a warm frame's flatten allocates
                // nothing.
                let need = w as usize * h as usize * 3;
                if self.rgb8.capacity() < need {
                    let add = need - self.rgb8.len();
                    self.rgb8.reserve(add);
                }
                let aspect = if h > 0 {
                    f32::from(w) / f32::from(h)
                } else {
                    1.0
                };
                self.canvas.set_aspect(aspect);
            }
        }
    }

    /// Whether a preset cross-fade is currently in progress.
    #[must_use]
    pub fn is_fading(&self) -> bool {
        self.fade.is_some()
    }

    /// The palette the presenter is currently rendering with.
    #[must_use]
    pub fn palette(&self) -> Palette {
        self.palette
    }

    /// Whether a palette cross-fade is currently in progress.
    #[must_use]
    pub fn is_palette_fading(&self) -> bool {
        self.palette_fade.is_some()
    }

    /// Cross-fade the host palette to `to` over [`FADE_SECS`], keeping the
    /// current layers and their animation. The colours ease from the presenter's
    /// current palette to `to` frame by frame — no hard colour snap — reusing the
    /// same frame/`dt` drive the preset fade uses. Applying the current track's
    /// palette and reverting to the scene's own palette are both just calls here.
    pub fn fade_palette(&mut self, to: Palette) {
        self.palette_fade = Some(PaletteFade {
            from: self.palette,
            to,
            elapsed: 0.0,
        });
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

        // Allocate (or reuse) the outgoing buffer at the current geometry for the
        // active mode.
        match self.mode {
            PresenterMode::Mosaic(tier) => {
                self.fb_out.resize(self.cols, self.rows, tier);
                self.fb_out_ready = true;
            }
            PresenterMode::Kitty { .. } | PresenterMode::Sixel { .. } => {
                self.px_out.resize(self.img_w, self.img_h);
                self.px_out.set_cells(self.cols, self.rows);
                self.px_out_ready = true;
            }
        }
        self.fade = Some(Fade { elapsed: 0.0 });
        // A preset swap sets its own palette; abandon any in-flight palette fade
        // rather than let it fight the new scene's colours.
        self.palette_fade = None;
    }

    /// Advance and rasterize one frame from the newest features.
    ///
    /// Per layer: apply the feature mappings into the layer's params, re-apply
    /// those params to the scene so a mapped value takes effect this frame,
    /// update the scene, render it onto the shared canvas, and rasterize the
    /// canvas into the frame buffer with the layer's blend and intensity. The
    /// layers paint in order into one buffer; the encoded [`CellGrid`] is
    /// produced last.
    pub fn frame(&mut self, snap: &FeatureSnapshot, dt: f32) {
        // Advance an active palette fade first, so the layers rasterize with the
        // interpolated palette this frame.
        if let Some(mut pf) = self.palette_fade {
            pf.elapsed += dt.max(0.0);
            let t = (pf.elapsed / FADE_SECS).clamp(0.0, 1.0);
            self.palette = lerp_palette(&pf.from, &pf.to, t);
            if pf.elapsed >= FADE_SECS {
                self.palette = pf.to;
                self.palette_fade = None;
            } else {
                self.palette_fade = Some(pf);
            }
        }

        match self.mode {
            PresenterMode::Mosaic(_) => self.frame_mosaic(snap, dt),
            PresenterMode::Kitty { .. } | PresenterMode::Sixel { .. } => self.frame_pixel(snap, dt),
        }
    }

    /// The mosaic frame path: rasterize into the cell frame buffer (mixing an
    /// active cross-fade), then encode to the cell grid.
    fn frame_mosaic(&mut self, snap: &FeatureSnapshot, dt: f32) {
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
                self.fade = None;
                self.outgoing.clear();
                self.outgoing_params.clear();
            }
        }

        self.fb.encode(&mut self.grid);
    }

    /// The pixel frame path (kitty and sixel): rasterize into the pixel image
    /// (mixing an active cross-fade), then flatten it to the RGB8 buffer the
    /// encoder consumes. Both graphics presenters share this — they diverge only
    /// at the run loop's encode-and-write step.
    fn frame_pixel(&mut self, snap: &FeatureSnapshot, dt: f32) {
        Self::rasterize_layers(
            &mut self.layers,
            &mut self.params,
            &self.palette,
            &mut self.canvas,
            &mut self.px,
            snap,
            dt,
        );

        if let Some(fade) = self.fade.as_mut() {
            fade.elapsed += dt.max(0.0);
            let elapsed = fade.elapsed;
            let t = (elapsed / FADE_SECS).clamp(0.0, 1.0);
            Self::rasterize_layers(
                &mut self.outgoing,
                &mut self.outgoing_params,
                &self.outgoing_palette,
                &mut self.canvas,
                &mut self.px_out,
                snap,
                dt,
            );
            self.px.mix_from(&self.px_out, t);
            if elapsed >= FADE_SECS {
                self.fade = None;
                self.outgoing.clear();
                self.outgoing_params.clear();
            }
        }

        self.px.write_rgb8(&mut self.rgb8);
    }

    /// Advance, render and rasterize a layer stack into `buf`. Per layer: fold the
    /// feature mappings into `params`, re-apply `params` to the scene (so a value
    /// a mapping just rewrote is honored on this frame), update the scene, render
    /// it onto the shared `canvas`, and rasterize with the layer's blend and
    /// intensity. Generic over the raster target so the same drive feeds the cell
    /// mosaic and the pixel image; free-standing (not `&mut self`) so a fade can
    /// drive the incoming and outgoing stacks with disjoint borrows.
    #[allow(clippy::too_many_arguments)]
    fn rasterize_layers<R: LayerRaster>(
        layers: &mut [LayerInstance],
        params: &mut [Params],
        palette: &Palette,
        canvas: &mut Canvas,
        buf: &mut R,
        snap: &FeatureSnapshot,
        dt: f32,
    ) {
        buf.clear();
        let aspect = canvas.aspect();
        for (layer, p) in layers.iter_mut().zip(params.iter_mut()) {
            layer.mappings.apply(snap, dt, p);
            layer.scene.apply_params(p);
            layer.scene.update(snap, dt);
            canvas.clear();
            canvas.set_aspect(aspect);
            layer.scene.render(canvas);
            buf.rasterize(canvas, palette, layer.blend, layer.intensity);
        }
    }

    /// Paint the encoded cells into `buf` over `area`, then the collected text
    /// runs on top as real terminal text.
    ///
    /// In mosaic mode the encoded cells are painted first, then the text runs on
    /// top. In the pixel modes the image is *not* painted here — it is written to
    /// the terminal as a graphics-protocol frame by the caller — so only the text
    /// runs are drawn, leaving the body cells clear for the image. In kitty mode
    /// the image sits below the text layer; in sixel mode it paints over the
    /// cells at its rectangle, so the text runs the caller emits afterward land on
    /// top of it.
    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        match self.mode {
            PresenterMode::Mosaic(_) => {
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
                self.draw_text(buf, area, self.fb.text_runs(), |r| self.fb.run_text(r));
            }
            PresenterMode::Kitty { .. } | PresenterMode::Sixel { .. } => {
                self.draw_text(buf, area, self.px.text_runs(), |r| self.px.run_text(r));
            }
        }
    }

    /// Paint the collected text runs into `buf` over `area` as real terminal
    /// text. Shared by both modes; `run_text` resolves a run against the active
    /// buffer's arena.
    fn draw_text<'a>(
        &self,
        buf: &mut Buffer,
        area: Rect,
        runs: &'a [TextRun],
        run_text: impl Fn(&'a TextRun) -> &'a str,
    ) {
        for run in runs {
            if run.cell_x >= area.width || run.cell_y >= area.height {
                continue;
            }
            let text = run_text(run);
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

/// A raster target for [`ScenePresenter::rasterize_layers`]: the cell mosaic
/// [`FrameBuffer`] or the [`PixelBuffer`]. Both clear, rasterize a canvas and
/// cross-fade identically, so the layer-drive is written once against this trait.
trait LayerRaster {
    fn clear(&mut self);
    fn rasterize(&mut self, canvas: &Canvas, palette: &Palette, blend: Blend, intensity: f32);
}

impl LayerRaster for FrameBuffer {
    fn clear(&mut self) {
        FrameBuffer::clear(self);
    }
    fn rasterize(&mut self, canvas: &Canvas, palette: &Palette, blend: Blend, intensity: f32) {
        FrameBuffer::rasterize(self, canvas, palette, blend, intensity);
    }
}

impl LayerRaster for PixelBuffer {
    fn clear(&mut self) {
        PixelBuffer::clear(self);
    }
    fn rasterize(&mut self, canvas: &Canvas, palette: &Palette, blend: Blend, intensity: f32) {
        PixelBuffer::rasterize(self, canvas, palette, blend, intensity);
    }
}

/// Convert an RGB triple to a `ratatui` truecolor.
#[inline]
fn to_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// Interpolate every slot of two palettes at `t` in `0.0..=1.0` (sRGB-linear in
/// byte space — plenty for a UI colour crossfade).
fn lerp_palette(a: &Palette, b: &Palette, t: f32) -> Palette {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    let mut slots = a.slots;
    for (dst, (sa, sb)) in slots.iter_mut().zip(a.slots.iter().zip(b.slots.iter())) {
        *dst = Rgb(lerp(sa.0, sb.0), lerp(sa.1, sb.1), lerp(sa.2, sb.2));
    }
    Palette { slots }
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
    build_scene_presenter_mode(name, PresenterMode::Mosaic(tier))
}

/// Build a scene presenter for a built-in preset `name` in `mode` (cell mosaic or
/// kitty graphics). The general form of [`build_scene_presenter`].
///
/// # Errors
/// [`SceneError`] when `name` is not a built-in preset, or when it is one but
/// fails to parse/validate.
pub fn build_scene_presenter_mode(
    name: &str,
    mode: PresenterMode,
) -> Result<ScenePresenter, SceneError> {
    // The catalog resolves both a built-in preset and a discovered Luau scene
    // (via a synthesized preset), so a `.lua` drop-in is reachable by `--scene`
    // exactly like a built-in.
    match scene_preset(name) {
        Some(Ok(preset)) => Ok(ScenePresenter::with_mode(&preset, mode)),
        Some(Err(err)) => Err(SceneError {
            message: format!("invalid scene preset '{name}': {err}"),
        }),
        None => {
            let names: Vec<&str> = catalog_scenes().iter().map(|i| i.id).collect();
            Err(SceneError {
                message: format!(
                    "unknown scene '{name}'; available scenes: {}",
                    names.join(", ")
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use scia_scenes::builtin_preset;

    /// Rasterize the presenter's current grid into a fresh buffer of `cols × rows`.
    fn snapshot_buffer(p: &ScenePresenter, cols: u16, rows: u16) -> Buffer {
        let area = Rect::new(0, 0, cols, rows);
        let mut buf = Buffer::empty(area);
        p.draw(&mut buf, area);
        buf
    }

    #[test]
    fn a_paused_frame_holds_and_resuming_advances() {
        // aurora animates purely on `dt` (its wave phases drift by `dt`), so a
        // dt=0 frame is a true freeze and a dt>0 frame advances.
        let preset = builtin_preset("aurora")
            .expect("aurora is a built-in preset")
            .expect("aurora parses");
        let mut p = ScenePresenter::from_preset(&preset, Tier::Half);
        let (cols, rows) = (24u16, 12u16);
        p.resize(cols, rows);

        let snap = FeatureSnapshot::default();
        // Advance once so there is non-trivial state to hold.
        p.frame(&snap, 0.1);
        let before = snapshot_buffer(&p, cols, rows);

        // Paused: dt = 0 must not advance the scene, so the frame is identical.
        p.frame(&snap, 0.0);
        let paused = snapshot_buffer(&p, cols, rows);
        assert_eq!(before, paused, "a paused frame (dt=0) must not advance");

        // Resume: a real dt advances the animation, so the frame changes.
        p.frame(&snap, 0.1);
        let resumed = snapshot_buffer(&p, cols, rows);
        assert_ne!(before, resumed, "resuming (dt>0) must advance the scene");
    }

    /// A distinctive flat palette, far from any scene default, so a swap is
    /// unmistakable in both the palette values and the rendered frame.
    fn flat_palette(c: Rgb) -> Palette {
        Palette { slots: [c; 8] }
    }

    #[test]
    fn palette_fade_interpolates_without_snapping() {
        // spectra's geometry is a pure function of the snapshot (no dt drift), so
        // with a fixed snapshot the frame changes only when the palette changes.
        let preset = builtin_preset("spectra")
            .expect("spectra is a built-in preset")
            .expect("spectra parses");
        let mut p = ScenePresenter::from_preset(&preset, Tier::Half);
        let (cols, rows) = (24u16, 12u16);
        p.resize(cols, rows);

        let mut snap = FeatureSnapshot::default();
        for (i, bar) in snap.spectrum.iter_mut().enumerate() {
            *bar = 0.4 + 0.5 * ((i % 5) as f32) / 5.0;
        }
        snap.spectrum_len = snap.spectrum.len() as u16;

        // Warm to a steady geometry so only the palette moves afterwards.
        let base_palette = p.palette();
        for _ in 0..80 {
            p.frame(&snap, 0.05);
        }
        let scene_buf = snapshot_buffer(&p, cols, rows);

        let target = flat_palette(Rgb(255, 0, 0));
        p.fade_palette(target);
        assert!(p.is_palette_fading());

        // Halfway (FADE_SECS = 0.3): the interpolated palette differs from both
        // endpoints — a true crossfade, not a jump.
        p.frame(&snap, 0.05);
        p.frame(&snap, 0.05);
        p.frame(&snap, 0.05);
        let mid_palette = p.palette();
        assert_ne!(mid_palette, base_palette, "mid palette is not the scene's");
        assert_ne!(mid_palette, target, "mid palette is not the target yet");
        let mid_buf = snapshot_buffer(&p, cols, rows);

        // Finish the fade.
        for _ in 0..10 {
            p.frame(&snap, 0.05);
        }
        assert!(!p.is_palette_fading(), "the fade completes");
        assert_eq!(
            p.palette(),
            target,
            "the palette lands exactly on the target"
        );
        let art_buf = snapshot_buffer(&p, cols, rows);

        // The rendered frames confirm no hard snap: the mid frame differs from
        // both endpoints, and the endpoints themselves differ.
        assert_ne!(
            mid_buf, scene_buf,
            "mid frame differs from the scene endpoint"
        );
        assert_ne!(mid_buf, art_buf, "mid frame differs from the art endpoint");
        assert_ne!(scene_buf, art_buf, "the palette visibly changed the frame");
    }

    /// Concatenate every glyph the buffer holds into one string.
    fn buffer_text(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn set_text_forwards_to_a_text_scene() {
        // `verso` renders the host track line as glyphs. Setting the text must
        // reach the scene through the presenter and change what is drawn.
        let preset = builtin_preset("verso")
            .expect("verso is a built-in preset")
            .expect("verso parses");
        let mut p = ScenePresenter::from_preset(&preset, Tier::Half);
        let (cols, rows) = (48u16, 16u16);
        p.resize(cols, rows);

        // A spectrum with signal so the letters render brightly on the baseline.
        let mut snap = FeatureSnapshot {
            spectrum_len: scia_core::SPECTRUM_BINS as u16,
            ..FeatureSnapshot::default()
        };
        for b in &mut snap.spectrum {
            *b = 0.8;
        }

        // Before: the fallback word `scia` is on screen.
        for _ in 0..8 {
            p.frame(&snap, 0.05);
        }
        let before = buffer_text(&snapshot_buffer(&p, cols, rows));
        assert!(
            before.contains('s') && before.contains('c'),
            "fallback drawn"
        );

        // Forward a new track line and let it settle.
        p.set_text("track", "zephyr");
        for _ in 0..8 {
            p.frame(&snap, 0.05);
        }
        let after = buffer_text(&snapshot_buffer(&p, cols, rows));
        for ch in ['z', 'e', 'p', 'h', 'y', 'r'] {
            assert!(
                after.contains(ch),
                "the forwarded track line `zephyr` should draw `{ch}`: {after:?}"
            );
        }
    }

    #[test]
    fn set_param_changes_what_the_scene_sees_next_frame() {
        // spectra's `gap` is an unmapped manifest key, so a set_param write
        // survives the per-frame mapping pass and both reads back from the bag
        // and visibly changes the rendered frame.
        let preset = builtin_preset("spectra")
            .expect("spectra is a built-in preset")
            .expect("spectra parses");
        let mut p = ScenePresenter::from_preset(&preset, Tier::Half);
        let (cols, rows) = (24u16, 12u16);
        p.resize(cols, rows);

        // A varied spectrum with signal so bars (and their gaps) render, matching
        // the setup the palette-fade test uses to get a steady spectra geometry.
        let mut snap = FeatureSnapshot::default();
        for (i, bar) in snap.spectrum.iter_mut().enumerate() {
            *bar = 0.4 + 0.5 * ((i % 5) as f32) / 5.0;
        }
        snap.spectrum_len = snap.spectrum.len() as u16;

        // The manifest default and the bag agree before any write.
        assert!((p.layer0_value("gap") - 0.15).abs() < 1e-6);
        assert!(p.layer0_specs().iter().any(|s| s.key == "gap"));
        assert!(!p.layer0_mapped("gap"), "gap is unmapped in spectra");
        assert!(p.layer0_mapped("punch"), "punch is mapped in spectra");

        // Warm the bars to a steady height so the gap change is visible.
        for _ in 0..80 {
            p.frame(&snap, 0.05);
        }
        let before = snapshot_buffer(&p, cols, rows);

        // Narrow the bars dramatically (a near-max gap); the change reads back
        // from the bag and, once the next frames apply it, thins the bars.
        p.set_param("gap", 0.9);
        assert!((p.layer0_value("gap") - 0.9).abs() < 1e-6, "bag read back");
        for _ in 0..10 {
            p.frame(&snap, 0.05);
        }
        let after = snapshot_buffer(&p, cols, rows);

        assert_ne!(
            before, after,
            "an unmapped param set through set_param changes the rendered frame"
        );
    }

    #[test]
    fn replacing_a_layer0_mapping_takes_effect_on_the_params_bag() {
        use scia_scenes::{ExprMapping, MapEntry};

        // spectra's `punch` is a mapped key. Swapping its mapping for a constant
        // expression makes the presenter write that constant into the params bag
        // on the next frame, observable through `layer0_value`.
        let preset = builtin_preset("spectra")
            .expect("spectra is a built-in preset")
            .expect("spectra parses");
        let mut p = ScenePresenter::from_preset(&preset, Tier::Half);
        p.resize(16, 8);
        assert!(p.layer0_mapped("punch"), "punch is mapped in spectra");

        // The row is listed for the overlay.
        let entries = p.layer0_mapping_entries();
        assert!(
            entries.iter().any(|e| e.target() == "punch"),
            "punch is listed as a mapping row"
        );

        let entry = MapEntry::Expr(ExprMapping::compile("punch", "0.7").expect("compiles"));
        assert!(p.replace_layer0_mapping(entry), "the row is replaced");

        p.frame(&FeatureSnapshot::default(), 0.05);
        assert!(
            (p.layer0_value("punch") - 0.7).abs() < 1e-6,
            "the replaced mapping drives the bag: {}",
            p.layer0_value("punch")
        );
    }

    #[test]
    fn palette_fade_toggles_back_to_the_scene_palette() {
        let preset = builtin_preset("spectra")
            .expect("spectra is a built-in preset")
            .expect("spectra parses");
        let mut p = ScenePresenter::from_preset(&preset, Tier::Half);
        p.resize(16, 8);
        let scene_palette = p.palette();

        // Apply an art palette and let it settle.
        p.fade_palette(flat_palette(Rgb(12, 200, 60)));
        for _ in 0..10 {
            p.frame(&FeatureSnapshot::default(), 0.05);
        }
        assert_ne!(p.palette(), scene_palette);

        // Toggle back: fade to the remembered scene palette.
        p.fade_palette(scene_palette);
        // Mid-fade it is neither endpoint.
        p.frame(&FeatureSnapshot::default(), 0.05);
        assert!(p.is_palette_fading());
        for _ in 0..10 {
            p.frame(&FeatureSnapshot::default(), 0.05);
        }
        assert_eq!(
            p.palette(),
            scene_palette,
            "reverts exactly to the scene palette"
        );
    }
}
