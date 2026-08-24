//! Pure rasterizer tests: drive the mosaic [`FrameBuffer`]/[`CellGrid`] and the
//! [`ScenePresenter`] with plain data and assert on the encoded cells. No TTY,
//! no terminal — the ratatui path is exercised only through a `TestBackend`
//! buffer. Table-completeness invariants live in the `mosaic` module's own unit
//! tests; these cover rasterization, encoding and the presenter.

mod support {
    pub mod alloc_watch;
}

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use scia_core::FeatureSnapshot;
use scia_scenes::{
    Blend, Canvas, Palette, Preset, Style as SceneStyle, builtin_preset, parse_preset,
};
use scia_tui::{CellGrid, FrameBuffer, ScenePresenter, Tier};

use support::alloc_watch::{CountingAllocator, watch};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const ALL_TIERS: [Tier; 4] = [Tier::Octant, Tier::Sextant, Tier::Quadrant, Tier::Half];
const SPACE: char = ' ';
const FULL: char = '\u{2588}';

/// Rasterize a freshly built canvas at `tier` into a `cols × rows` grid.
fn render(
    tier: Tier,
    cols: u16,
    rows: u16,
    build: impl FnOnce(&mut Canvas),
) -> (FrameBuffer, CellGrid) {
    let palette = Palette::default_dark();
    let mut fb = FrameBuffer::new();
    fb.resize(cols, rows, tier);
    let mut canvas = Canvas::new(1.0);
    build(&mut canvas);
    fb.rasterize(&canvas, &palette, Blend::Over, 1.0);
    let mut grid = CellGrid::new();
    fb.encode(&mut grid);
    (fb, grid)
}

/// The palette colour of `slot` as a truecolor triple.
fn slot_rgb(slot: u8) -> (u8, u8, u8) {
    let scia_scenes::Rgb(r, g, b) = Palette::default_dark().color(slot);
    (r, g, b)
}

#[test]
fn full_bar_fills_every_cell_at_each_tier() {
    let slot = 3; // amber
    for tier in ALL_TIERS {
        let (_fb, grid) = render(tier, 4, 3, |c| {
            c.bar(0.0, 0.0, 1.0, 1.0, SceneStyle::new(slot, 1.0));
        });
        for y in 0..grid.rows() {
            for x in 0..grid.cols() {
                let cell = grid.cell(x, y).expect("cell");
                assert_eq!(cell.ch, FULL, "{:?} cell ({x},{y})", tier.label());
                assert_eq!(cell.fg, slot_rgb(slot), "{:?} fg", tier.label());
            }
        }
    }
}

#[test]
fn half_bar_boundary_row_is_a_lower_half_block() {
    // Half tier, one column, four cells tall (eight pixels). A bar over the
    // bottom five pixels fills the bottom two cells and leaves a lower-half
    // block on the boundary cell.
    let slot = 1; // teal
    let color = slot_rgb(slot);
    let (_fb, grid) = render(Tier::Half, 1, 4, |c| {
        c.bar(0.0, 0.375, 1.0, 0.625, SceneStyle::new(slot, 1.0));
    });
    assert_eq!(grid.cell(0, 3).unwrap().ch, FULL, "bottom cell full");
    assert_eq!(grid.cell(0, 2).unwrap().ch, FULL, "next cell full");
    let boundary = grid.cell(0, 1).unwrap();
    assert_eq!(boundary.ch, '\u{2584}', "boundary is a lower half block");
    assert_eq!(boundary.fg, color, "boundary fg is the bar colour");
    assert_eq!(boundary.bg, (0, 0, 0), "boundary bg is black");
    assert_eq!(grid.cell(0, 0).unwrap().ch, SPACE, "top cell empty");
}

#[test]
fn point_lights_exactly_one_octant_subpixel() {
    // A one-pixel point placed on the top-left subpixel of cell (1,1) at octant
    // resolution: only that cell lights, with the single-corner glyph.
    let slot = 2; // cyan
    let (_fb, grid) = render(Tier::Octant, 3, 3, |c| {
        // px grid is 6x12; target pixel (2,4) = cell (1,1), subpixel (col0,row0).
        c.point(0.34, 0.34, 0.01, SceneStyle::new(slot, 1.0));
    });
    for y in 0..grid.rows() {
        for x in 0..grid.cols() {
            let cell = grid.cell(x, y).unwrap();
            if x == 1 && y == 1 {
                assert_eq!(cell.ch, '\u{1CEA8}', "lit cell glyph (bit 0)");
                assert_eq!(cell.fg, slot_rgb(slot), "lit cell fg");
            } else {
                assert_eq!(cell.ch, SPACE, "cell ({x},{y}) should be dark");
            }
        }
    }
}

#[test]
fn later_overlapping_bar_wins() {
    let (_fb, grid) = render(Tier::Quadrant, 2, 2, |c| {
        c.bar(0.0, 0.0, 1.0, 1.0, SceneStyle::new(1, 1.0)); // teal, under
        c.bar(0.0, 0.0, 1.0, 1.0, SceneStyle::new(4, 1.0)); // coral, over
    });
    for y in 0..grid.rows() {
        for x in 0..grid.cols() {
            let cell = grid.cell(x, y).unwrap();
            assert_eq!(cell.ch, FULL);
            assert_eq!(cell.fg, slot_rgb(4), "later slot (coral) wins");
        }
    }
}

#[test]
fn field_lights_the_expected_cells() {
    // A 2x2 field with [0,1,1,0] over a 2x2 quadrant grid: top-right and
    // bottom-left field cells are bright, the diagonal is dark.
    let (_fb, grid) = render(Tier::Quadrant, 2, 2, |c| {
        c.field(2, 2, &[0.0, 1.0, 1.0, 0.0], SceneStyle::new(2, 1.0));
    });
    assert_eq!(grid.cell(1, 0).unwrap().ch, FULL, "top-right bright");
    assert_eq!(grid.cell(0, 1).unwrap().ch, FULL, "bottom-left bright");
    assert_eq!(grid.cell(0, 0).unwrap().ch, SPACE, "top-left dark");
    assert_eq!(grid.cell(1, 1).unwrap().ch, SPACE, "bottom-right dark");
}

#[test]
fn text_is_collected_not_rasterized() {
    let slot = 5;
    let (fb, grid) = render(Tier::Half, 4, 4, |c| {
        c.text(0.5, 0.5, "hi", SceneStyle::new(slot, 0.8));
    });
    // Nothing painted into the pixel grid: every cell is empty.
    for y in 0..grid.rows() {
        for x in 0..grid.cols() {
            assert_eq!(grid.cell(x, y).unwrap().ch, SPACE, "text must not paint");
        }
    }
    let runs = fb.text_runs();
    assert_eq!(runs.len(), 1, "one text run collected");
    let run = &runs[0];
    assert_eq!((run.cell_x, run.cell_y), (2, 2), "anchor cell");
    assert_eq!(run.slot, slot);
    assert!((run.intensity - 0.8).abs() < 1e-6, "intensity carried");
    assert_eq!(fb.run_text(run), "hi");
}

#[test]
fn encoding_is_deterministic() {
    let build = |c: &mut Canvas| {
        c.bar(0.1, 0.2, 0.3, 0.4, SceneStyle::new(3, 0.9));
        c.line(0.0, 0.0, 1.0, 1.0, 0.05, SceneStyle::new(4, 1.0));
        c.point(0.7, 0.6, 0.1, SceneStyle::new(2, 0.7));
    };
    let (_a_fb, a) = render(Tier::Octant, 8, 6, build);
    let (_b_fb, b) = render(Tier::Octant, 8, 6, build);
    assert_eq!(a, b, "same canvas encodes to identical grids");
}

/// A synthetic snapshot whose display spectrum is a `0..len` ramp.
fn ramp_snapshot(len: usize) -> FeatureSnapshot {
    let mut snap = FeatureSnapshot::default();
    for i in 0..len {
        snap.spectrum[i] = i as f32 / (len - 1) as f32;
    }
    snap.spectrum_len = len as u16;
    snap
}

#[test]
fn presenter_renders_spectra_into_the_body() {
    let preset = builtin_preset("spectra")
        .expect("builtin exists")
        .expect("valid");
    let mut presenter = ScenePresenter::from_preset(&preset, Tier::Half);
    assert_eq!(presenter.tier(), Tier::Half);

    let snap = ramp_snapshot(64);
    let body = Rect::new(0, 1, 40, 10);
    presenter.resize(body.width, body.height);
    presenter.frame(&snap, 1.0 / 60.0);

    // A pre-filled header row must survive; the body must gain content.
    let mut buf = Buffer::empty(Rect::new(0, 0, 40, 11));
    buf.set_string(0, 0, "HEADERHEADERHEADER", Style::new());
    presenter.draw(&mut buf, body);

    let header: String = (0..6).map(|x| symbol(&buf, x, 0)).collect();
    assert_eq!(header, "HEADER", "header row untouched");

    let painted = (body.y..body.y + body.height)
        .flat_map(|y| (0..body.width).map(move |x| (x, y)))
        .any(|(x, y)| symbol(&buf, x, y) != " ");
    assert!(painted, "presenter drew something into the body");
}

#[test]
fn set_tier_switches_the_ladder() {
    let preset = builtin_preset("spectra").unwrap().unwrap();
    let mut presenter = ScenePresenter::from_preset(&preset, Tier::Half);
    presenter.set_tier(Tier::Octant);
    assert_eq!(presenter.tier(), Tier::Octant);
    let snap = ramp_snapshot(64);
    presenter.resize(20, 8);
    presenter.frame(&snap, 0.016); // must not panic at a finer tier
}

fn symbol(buf: &Buffer, x: u16, y: u16) -> String {
    buf.cell((x, y)).expect("cell").symbol().to_string()
}

#[test]
fn build_scene_presenter_resolves_a_valid_preset() {
    // The seam the CLI drives: a known preset builds at the requested tier, and
    // the tier label is exactly what the debug line surfaces.
    let presenter =
        scia_tui::build_scene_presenter("spectra", Tier::Octant).expect("spectra builds");
    assert_eq!(presenter.tier(), Tier::Octant);
    assert_eq!(presenter.tier().label(), "octants");
}

#[test]
fn build_scene_presenter_reports_an_unknown_name() {
    let err = scia_tui::build_scene_presenter("nope", Tier::Half)
        .map(|_| ())
        .expect_err("unknown preset is an error, not a panic");
    let msg = err.to_string();
    assert!(msg.contains("nope"), "names the bad preset: {msg}");
    assert!(msg.contains("spectra"), "lists available presets: {msg}");
}

#[test]
fn debug_line_carries_the_active_scene_tier_label() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use scia_tui::{UiState, draw};

    // A scene is active: its presenter's tier label is what the debug line must
    // carry, wiring the probe-selected tier through to the debug overlay.
    let presenter =
        scia_tui::build_scene_presenter("spectra", Tier::Octant).expect("spectra builds");
    let ui = UiState {
        debug: true,
        tier: Some(presenter.tier().label()),
        ..UiState::default()
    };
    let snap = ramp_snapshot(64);

    let width = 140;
    let mut terminal = Terminal::new(TestBackend::new(width, 10)).expect("terminal");
    terminal
        .draw(|frame| draw(frame, &snap, &ui))
        .expect("draw");
    let buf = terminal.backend().buffer().clone();

    let last = (0..width).map(|x| symbol(&buf, x, 9)).collect::<String>();
    assert!(
        last.contains("tier octants"),
        "debug row missing scene tier label: {last:?}"
    );
}

#[test]
fn presenter_frame_does_not_allocate_when_warm() {
    let preset = builtin_preset("spectra").unwrap().unwrap();
    let mut presenter = ScenePresenter::from_preset(&preset, Tier::Octant);
    let snap = ramp_snapshot(64);
    presenter.resize(48, 16);

    // Warm up: grow every backing store to steady state.
    for _ in 0..8 {
        presenter.frame(&snap, 0.016);
    }

    // A swap allocates the second frame buffer (and the incoming layers) once;
    // drive its cross-fade to completion so the measured window is steady state
    // with the outgoing layers already dropped.
    let swapped = builtin_preset("spectra").unwrap().unwrap();
    presenter.swap_preset(&swapped);
    for _ in 0..40 {
        presenter.frame(&snap, 0.016);
    }
    assert!(!presenter.is_fading(), "fade completes during warm-up");

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..100 {
            presenter.frame(&snap, 0.016);
        }
    });
    assert!(
        stray_count == 0,
        "ScenePresenter::frame allocated {stray_count} time(s):\n{}",
        strays.join("\n---\n")
    );
}

/// A single-layer spectra preset with the given `[params]` body.
fn spectra_preset(params: &str) -> Preset {
    let src = format!("[preset]\nname = \"spectra\"\nscene = \"spectra\"\n[params]\n{params}\n");
    parse_preset(&src, None).expect("valid preset")
}

/// A flat display spectrum: `len` bars all at `v`.
fn flat_snapshot(len: usize, v: f32) -> FeatureSnapshot {
    let mut snap = FeatureSnapshot::default();
    for i in 0..len {
        snap.spectrum[i] = v;
    }
    snap.spectrum_len = len as u16;
    snap
}

/// Paint a presenter into a fresh `cols × rows` buffer.
fn paint(p: &ScenePresenter, cols: u16, rows: u16) -> Buffer {
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);
    p.draw(&mut buf, area);
    buf
}

/// Count the non-space (lit) cells in column `x` of `area`.
fn column_fill(buf: &Buffer, area: Rect, x: u16) -> u16 {
    (area.y..area.y + area.height)
        .filter(|&y| symbol(buf, area.x + x, y) != " ")
        .count() as u16
}

/// Column fill for a fresh presenter of `preset` after one frame of `snap`.
fn column_fill_of(preset: &Preset, cols: u16, rows: u16, snap: &FeatureSnapshot, x: u16) -> u16 {
    let mut p = ScenePresenter::from_preset(preset, Tier::Half);
    p.resize(cols, rows);
    p.frame(snap, 0.016);
    column_fill(&paint(&p, cols, rows), Rect::new(0, 0, cols, rows), x)
}

#[test]
fn swap_carries_spectra_envelope() {
    let (cols, rows) = (16u16, 8u16);
    let base = spectra_preset("punch = 2.0");
    let mut p = ScenePresenter::from_preset(&base, Tier::Half);
    p.resize(cols, rows);

    // Charge the onset envelope on the current scene (an onset snaps it to 1.0).
    let mut onset = flat_snapshot(8, 0.4);
    onset.onset = true;
    p.frame(&onset, 0.016);

    // Swap to a modified preset (same scene id) and drive the fade to completion
    // in one 300 ms step with NO onset, so the envelope only decays from the
    // value it carried. This is the first post-swap frame.
    let modified = spectra_preset("punch = 2.0\ngap = 0.2");
    p.swap_preset(&modified);
    p.frame(&flat_snapshot(8, 0.4), 0.30);
    assert!(!p.is_fading(), "a single 300 ms step completes the fade");

    // Output is now purely the incoming scene. A carried envelope lifts the low
    // quarter of bars above the flat spectrum; a reset envelope would leave them
    // level with the rest, so the low bar stands taller than a high one.
    let buf = paint(&p, cols, rows);
    let area = Rect::new(0, 0, cols, rows);
    let low = column_fill(&buf, area, 0);
    let high = column_fill(&buf, area, 15);
    assert!(
        low > high,
        "carried envelope lifts the low bars (low col {low} > high col {high})"
    );
}

#[test]
fn fade_blends_outgoing_and_incoming() {
    let (cols, rows) = (16u16, 8u16);
    // Dense bars (no gap) fill column 0; sparse bars (wide gaps) leave it dark,
    // so column 0 is a cell only the outgoing scene lights.
    let dense = spectra_preset("gap = 0.0");
    let sparse = spectra_preset("gap = 0.5");
    let snap = flat_snapshot(4, 0.6);

    let dense_col0 = column_fill_of(&dense, cols, rows, &snap, 0);
    let sparse_col0 = column_fill_of(&sparse, cols, rows, &snap, 0);
    assert!(
        dense_col0 > 0 && sparse_col0 == 0,
        "column 0 must be outgoing-only (dense {dense_col0}, sparse {sparse_col0})"
    );

    let mut p = ScenePresenter::from_preset(&dense, Tier::Half);
    p.resize(cols, rows);
    p.frame(&snap, 0.016); // warm the outgoing scene
    p.swap_preset(&sparse);

    // Two probes early in the fade (t ≈ 0.1, 0.2): the outgoing-only column
    // stays lit, so the outgoing scene is still contributing.
    let area = Rect::new(0, 0, cols, rows);
    for step in 0..2 {
        p.frame(&snap, 0.03);
        assert!(p.is_fading(), "still fading at probe {step}");
        let buf = paint(&p, cols, rows);
        assert!(
            column_fill(&buf, area, 0) > 0,
            "outgoing scene lights column 0 mid-fade at probe {step}"
        );
    }
}

/// One presenter frame of `preset` under `snap`, flattened to its symbol grid.
fn frame_grid(preset: &Preset, snap: &FeatureSnapshot, cols: u16, rows: u16) -> String {
    let mut p = ScenePresenter::from_preset(preset, Tier::Half);
    p.resize(cols, rows);
    p.frame(snap, 0.016);
    let buf = paint(&p, cols, rows);
    let mut grid = String::new();
    for y in 0..rows {
        for x in 0..cols {
            grid.push_str(&symbol(&buf, x, y));
        }
    }
    grid
}

#[test]
fn mapping_makes_the_presenter_feature_responsive() {
    let (cols, rows) = (16u16, 8u16);
    // The same scene, once with `gap` mapped to loudness and once without.
    let mapped = parse_preset(
        "[preset]\nname = \"m\"\nscene = \"spectra\"\n\
         [map]\ngap = { feature = \"loud\", scale = 0.8 }\n",
        None,
    )
    .expect("valid mapped preset");
    let plain = parse_preset("[preset]\nname = \"p\"\nscene = \"spectra\"\n", None)
        .expect("valid plain preset");

    // Two snapshots with identical spectra but different loudness.
    let quiet = flat_snapshot(8, 0.6);
    let mut loud = flat_snapshot(8, 0.6);
    loud.rms = 0.9;

    // The mapped preset paints differently under the two loudness levels: the
    // live `gap` mapping reaches the render on the same frame it is folded in.
    assert_ne!(
        frame_grid(&mapped, &quiet, cols, rows),
        frame_grid(&mapped, &loud, cols, rows),
        "a `[map]` on gap must let loudness change the painted frame"
    );

    // Spectra reads only the spectrum and onset, so the unmapped preset is
    // invariant to loudness — confirming the difference above is the mapping.
    assert_eq!(
        frame_grid(&plain, &quiet, cols, rows),
        frame_grid(&plain, &loud, cols, rows),
        "with no mapping, loudness must not change the frame"
    );
}

#[test]
fn fade_completes_after_300ms() {
    let mut p = ScenePresenter::from_preset(&spectra_preset("gap = 0.0"), Tier::Half);
    p.resize(16, 8);
    let snap = flat_snapshot(8, 0.5);
    p.frame(&snap, 0.016);

    p.swap_preset(&spectra_preset("gap = 0.3"));
    assert!(p.is_fading(), "fade is active right after the swap");

    // ~290 ms in 10 ms steps: still fading.
    for _ in 0..29 {
        p.frame(&snap, 0.01);
    }
    assert!(p.is_fading(), "still fading before 300 ms");

    // Cross the 300 ms mark: the fade ends.
    for _ in 0..3 {
        p.frame(&snap, 0.01);
    }
    assert!(!p.is_fading(), "fade ends after 300 ms");
}
