//! Pixel-rasterizer tests: drive the [`PixelBuffer`] with plain data and assert
//! on the flattened RGB8 image. No TTY, no terminal. Covers primitive placement,
//! the shared Slot/intensity resolution against the mosaic path, the image sizing
//! math, and the warm pixel+encode no-alloc contract.

mod support {
    pub mod alloc_watch;
}

use scia_scenes::{Blend, Canvas, Palette, Rgb, Style};
use scia_tui::{
    FALLBACK_CELL_PX, FrameBuffer, KittyEncoder, PIXEL_BUDGET, PixelBuffer, Tier, image_dims,
};

use support::alloc_watch::{CountingAllocator, watch};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// The palette colour of `slot` as a truecolor triple.
fn slot_rgb(slot: u8) -> (u8, u8, u8) {
    let Rgb(r, g, b) = Palette::default_dark().color(slot);
    (r, g, b)
}

/// Rasterize a freshly built canvas into a `px_w × px_h` pixel image, returning
/// the flattened RGB8 bytes.
fn render_px(px_w: u16, px_h: u16, build: impl FnOnce(&mut Canvas)) -> Vec<u8> {
    let palette = Palette::default_dark();
    let mut px = PixelBuffer::new();
    px.resize(px_w, px_h);
    let mut canvas = Canvas::new(px_w as f32 / px_h as f32);
    build(&mut canvas);
    px.rasterize(&canvas, &palette, Blend::Over, 1.0);
    let mut rgb = Vec::new();
    px.write_rgb8(&mut rgb);
    rgb
}

/// The pixel at `(x, y)` of a row-major RGB8 image `w` pixels wide.
fn px_at(rgb: &[u8], w: u16, x: u16, y: u16) -> (u8, u8, u8) {
    let i = (y as usize * w as usize + x as usize) * 3;
    (rgb[i], rgb[i + 1], rgb[i + 2])
}

#[test]
fn full_bar_fills_every_pixel() {
    let slot = 3;
    let (w, h) = (8u16, 6u16);
    let rgb = render_px(w, h, |c| c.bar(0.0, 0.0, 1.0, 1.0, Style::new(slot, 1.0)));
    for y in 0..h {
        for x in 0..w {
            assert_eq!(px_at(&rgb, w, x, y), slot_rgb(slot), "pixel ({x},{y})");
        }
    }
}

#[test]
fn bar_fills_its_rect_and_leaves_the_rest_black() {
    // A bar over the left half fills columns 0..w/2 and leaves the right dark.
    let slot = 1;
    let (w, h) = (8u16, 4u16);
    let rgb = render_px(w, h, |c| c.bar(0.0, 0.0, 0.5, 1.0, Style::new(slot, 1.0)));
    for y in 0..h {
        assert_eq!(px_at(&rgb, w, 0, y), slot_rgb(slot), "left edge lit");
        assert_eq!(px_at(&rgb, w, 3, y), slot_rgb(slot), "left half lit");
        assert_eq!(px_at(&rgb, w, 4, y), (0, 0, 0), "right half dark");
        assert_eq!(px_at(&rgb, w, 7, y), (0, 0, 0), "right edge dark");
    }
}

#[test]
fn point_lights_its_pixel() {
    // A tiny point at the centre lights exactly one pixel and leaves the corners
    // dark. `0.5 * 9 = 4.5` rounds away from zero to pixel 5.
    let slot = 2;
    let (w, h) = (9u16, 9u16);
    let rgb = render_px(w, h, |c| c.point(0.5, 0.5, 0.01, Style::new(slot, 1.0)));
    assert_eq!(px_at(&rgb, w, 5, 5), slot_rgb(slot), "point pixel lit");
    assert_eq!(px_at(&rgb, w, 0, 0), (0, 0, 0), "top-left dark");
    assert_eq!(px_at(&rgb, w, 8, 8), (0, 0, 0), "bottom-right dark");
}

#[test]
fn line_lights_its_endpoints() {
    // A thin diagonal from corner to corner lights both endpoints.
    let slot = 4;
    let (w, h) = (10u16, 10u16);
    let rgb = render_px(w, h, |c| {
        c.line(0.0, 0.0, 1.0, 1.0, 0.01, Style::new(slot, 1.0))
    });
    assert_eq!(px_at(&rgb, w, 0, 0), slot_rgb(slot), "start endpoint lit");
    assert_eq!(
        px_at(&rgb, w, w - 1, h - 1),
        slot_rgb(slot),
        "end endpoint lit"
    );
}

#[test]
fn field_lights_the_expected_pixels() {
    // A 2x2 field [0,1,1,0]: top-right and bottom-left quadrants bright, the
    // diagonal dark.
    let slot = 2;
    let (w, h) = (4u16, 4u16);
    let rgb = render_px(w, h, |c| {
        c.field(2, 2, &[0.0, 1.0, 1.0, 0.0], Style::new(slot, 1.0));
    });
    assert_eq!(px_at(&rgb, w, 3, 0), slot_rgb(slot), "top-right bright");
    assert_eq!(px_at(&rgb, w, 0, 3), slot_rgb(slot), "bottom-left bright");
    assert_eq!(px_at(&rgb, w, 0, 0), (0, 0, 0), "top-left dark");
    assert_eq!(px_at(&rgb, w, 3, 3), (0, 0, 0), "bottom-right dark");
}

#[test]
fn slot_and_intensity_resolution_matches_the_mosaic() {
    // A full bar at a partial intensity must resolve to exactly the same colour
    // the mosaic path produces for a full cell — the shared slot_color helper.
    let slot = 3;
    let intensity = 0.7;
    let build = |c: &mut Canvas| c.bar(0.0, 0.0, 1.0, 1.0, Style::new(slot, intensity));
    let palette = Palette::default_dark();

    // Mosaic: a uniform full bar encodes to a full cell whose fg is the colour.
    let mut fb = FrameBuffer::new();
    fb.resize(2, 2, Tier::Half);
    let mut canvas = Canvas::new(1.0);
    build(&mut canvas);
    fb.rasterize(&canvas, &palette, Blend::Over, 1.0);
    let mut grid = scia_tui::CellGrid::new();
    fb.encode(&mut grid);
    let mosaic_fg = grid.cell(0, 0).expect("cell").fg;

    // Pixel: the same bar fills the image with that colour.
    let rgb = render_px(4, 4, build);
    let pixel = px_at(&rgb, 4, 1, 1);

    assert_eq!(
        pixel, mosaic_fg,
        "pixel colour must match the mosaic cell fg (shared slot/intensity math)"
    );
}

#[test]
fn text_is_collected_not_rasterized() {
    let palette = Palette::default_dark();
    let mut px = PixelBuffer::new();
    px.resize(8, 8);
    px.set_cells(4, 4);
    let mut canvas = Canvas::new(1.0);
    canvas.text(0.5, 0.5, "hi", Style::new(5, 0.8));
    px.rasterize(&canvas, &palette, Blend::Over, 1.0);

    // Nothing painted into the pixel grid.
    let mut rgb = Vec::new();
    px.write_rgb8(&mut rgb);
    assert!(rgb.iter().all(|&b| b == 0), "text must not paint pixels");

    // The run is collected against the cell grid.
    let runs = px.text_runs();
    assert_eq!(runs.len(), 1, "one text run collected");
    assert_eq!((runs[0].cell_x, runs[0].cell_y), (2, 2), "anchor cell");
    assert_eq!(runs[0].slot, 5);
    assert_eq!(px.run_text(&runs[0]), "hi");
}

#[test]
fn image_dims_budget_and_downscale() {
    // Fallback cell size is height x width = 20 x 10.
    assert_eq!(FALLBACK_CELL_PX, (20, 10));

    // Under budget: no downscale (k = 1). 10 cols * 10 wide = 100, 5 rows * 20
    // tall = 100; 10_000 <= 500_000.
    assert_eq!(
        image_dims(10, 5, FALLBACK_CELL_PX, PIXEL_BUDGET),
        (100, 100)
    );

    // Over budget: 200*10 = 2000 wide, 100*20 = 2000 tall, product 4_000_000.
    // k = 2 gives 1000*1000 = 1_000_000 (still over); k = 3 gives 666*666 =
    // 443_556 (fits), so the smallest k is 3.
    assert_eq!(image_dims(200, 100, (20, 10), PIXEL_BUDGET), (666, 666));

    // A custom cell size is honoured (not the fallback): 4 cols * 12 wide = 48,
    // 3 rows * 6 tall = 18.
    assert_eq!(image_dims(4, 3, (6, 12), PIXEL_BUDGET), (48, 18));

    // An empty area is zero-sized.
    assert_eq!(image_dims(0, 10, FALLBACK_CELL_PX, PIXEL_BUDGET), (0, 0));
    assert_eq!(image_dims(10, 0, FALLBACK_CELL_PX, PIXEL_BUDGET), (0, 0));
}

/// A representative canvas: bars, a line, a point, a field and a text run — the
/// same shape budget every frame.
fn build_scene(canvas: &mut Canvas) {
    for i in 0..16u16 {
        let x = i as f32 / 16.0;
        canvas.bar(x, 0.2, 0.05, 0.6, Style::new(1, 0.9));
    }
    canvas.line(0.0, 0.0, 1.0, 1.0, 0.02, Style::new(4, 1.0));
    canvas.point(0.5, 0.5, 0.1, Style::new(2, 0.7));
    canvas.field(4, 4, &[0.5; 16], Style::new(3, 0.6));
    canvas.text(0.1, 0.1, "hi", Style::new(5, 0.8));
}

#[test]
fn warm_pixel_and_encode_frame_does_not_allocate() {
    let palette = Palette::default_dark();
    let mut px = PixelBuffer::new();
    px.resize(320, 200);
    px.set_cells(32, 20);
    let mut canvas = Canvas::new(1.6);
    build_scene(&mut canvas);

    let mut rgb: Vec<u8> = Vec::new();
    let mut encoder = KittyEncoder::new();
    let mut out: Vec<u8> = Vec::new();

    // Warm up: grow every backing store (pixel arena, text arena, rgb8, zlib,
    // base64, APC output) to steady state. Each frame clears then rasterizes,
    // mirroring the presenter's per-frame sequence.
    for _ in 0..8 {
        px.clear();
        px.rasterize(&canvas, &palette, Blend::Over, 1.0);
        px.write_rgb8(&mut rgb);
        encoder.encode(&rgb, px.dims(), (32, 20), &mut out);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..50 {
            px.clear();
            px.rasterize(&canvas, &palette, Blend::Over, 1.0);
            px.write_rgb8(&mut rgb);
            encoder.encode(&rgb, px.dims(), (32, 20), &mut out);
        }
    });
    assert!(
        stray_count == 0,
        "warm pixel+encode allocated {stray_count} time(s):\n{}",
        strays.join("\n---\n")
    );
}
