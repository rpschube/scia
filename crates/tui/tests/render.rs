//! Headless rendering tests: drive [`scia_tui::draw`] into a ratatui
//! `TestBackend` and assert on the resulting cell buffer. These run with no
//! TTY.

use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use scia_core::{Activity, EngineStats, FeatureSnapshot};
use scia_tui::{UiState, draw};

/// Render one frame at `w`×`h` and return the resulting buffer.
fn render(w: u16, h: u16, snap: &FeatureSnapshot, ui: &UiState) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, snap, ui)).expect("draw");
    terminal.backend().buffer().clone()
}

/// The symbol at a cell as a `String`.
fn sym(buf: &Buffer, x: u16, y: u16) -> String {
    buf.cell((x, y))
        .expect("cell in bounds")
        .symbol()
        .to_string()
}

/// Concatenate a whole row into a string.
fn row(buf: &Buffer, y: u16, width: u16) -> String {
    (0..width).map(|x| sym(buf, x, y)).collect()
}

fn snapshot_with(values: &[f32]) -> FeatureSnapshot {
    let mut snap = FeatureSnapshot::default();
    for (i, &v) in values.iter().enumerate() {
        snap.spectrum[i] = v;
    }
    snap.spectrum_len = values.len() as u16;
    snap
}

#[test]
fn bars_map_height_to_glyphs() {
    // 4 bars into a 4-wide terminal: header row + 9-row body.
    let snap = snapshot_with(&[0.0, 0.5, 1.0, 0.125]);
    let ui = UiState::default();
    let buf = render(4, 10, &snap, &ui);

    // Body rows are y = 1..=9, bottom row is y = 9.
    // Column 0 (value 0.0): entirely empty.
    for y in 1..=9 {
        assert_eq!(sym(&buf, 0, y), " ", "col 0 row {y} should be empty");
    }

    // Column 2 (value 1.0): every body cell is a full block.
    for y in 1..=9 {
        assert_eq!(sym(&buf, 2, y), "█", "col 2 row {y} should be full");
    }

    // Column 1 (value 0.5 over 9 cells): four full blocks from the bottom
    // (y = 6..=9) topped by a half block '▄' at y = 5, empty above.
    for y in 6..=9 {
        assert_eq!(sym(&buf, 1, y), "█", "col 1 row {y} should be full");
    }
    assert_eq!(sym(&buf, 1, 5), "▄", "col 1 top glyph is the half block");
    for y in 1..=4 {
        assert_eq!(sym(&buf, 1, y), " ", "col 1 row {y} should be empty");
    }

    // Column 3 (value 0.125): exactly one one-eighth block '▁'.
    let ones = (1..=9u16).filter(|&y| sym(&buf, 3, y) == "▁").count();
    assert_eq!(ones, 1, "col 3 should have exactly one '▁'");
    assert_eq!(sym(&buf, 3, 9), "█", "col 3 bottom cell is a full block");
    assert_eq!(sym(&buf, 3, 8), "▁", "col 3 second cell is one eighth");
}

/// A `UiState` carrying just an activity state on its stats.
fn ui_with_activity(activity: Activity) -> UiState {
    UiState {
        stats: EngineStats {
            activity,
            ..EngineStats::default()
        },
        ..UiState::default()
    }
}

#[test]
fn header_shows_label_and_activity() {
    let snap = snapshot_with(&[0.5; 8]);
    let ui = UiState {
        label: Some("DEMO — synthetic feed".to_string()),
        stats: EngineStats {
            activity: Activity::Idle,
            ..EngineStats::default()
        },
        ..UiState::default()
    };
    let buf = render(60, 10, &snap, &ui);
    let header = row(&buf, 0, 60);
    assert!(header.contains("DEMO"), "header missing label: {header:?}");
    assert!(
        header.contains("synthetic feed"),
        "header missing full label: {header:?}"
    );
    assert!(
        header.contains("idle"),
        "header missing activity indicator: {header:?}"
    );
    assert!(header.contains("scia"), "header missing name: {header:?}");
}

#[test]
fn header_shows_live_and_format_without_label() {
    let snap = snapshot_with(&[0.5; 8]);
    let ui = UiState {
        source: "48000 Hz 2 ch".to_string(),
        ..ui_with_activity(Activity::Active)
    };
    let buf = render(60, 10, &snap, &ui);
    let header = row(&buf, 0, 60);
    assert!(
        header.contains("live"),
        "header should show live: {header:?}"
    );
    assert!(
        header.contains("48000 Hz 2 ch"),
        "header should show the negotiated format: {header:?}"
    );
    // No demo highlight text leaks into a live header.
    assert!(
        !header.contains("DEMO"),
        "live header must not show a demo label: {header:?}"
    );
}

#[test]
fn header_indicator_tracks_activity() {
    let snap = snapshot_with(&[0.5; 8]);
    for (activity, word) in [
        (Activity::Active, "active"),
        (Activity::Quiet, "quiet"),
        (Activity::Idle, "idle"),
    ] {
        let ui = ui_with_activity(activity);
        let buf = render(60, 10, &snap, &ui);
        let header = row(&buf, 0, 60);
        assert!(
            header.contains(word),
            "header for {activity:?} should show {word:?}: {header:?}"
        );
    }
}

#[test]
fn debug_line_toggles() {
    let snap = snapshot_with(&[0.3; 16]);

    // Debug on: the last row carries the debug fields.
    let ui_on = UiState {
        debug: true,
        ..UiState::default()
    };
    let buf = render(120, 10, &snap, &ui_on);
    let last = row(&buf, 9, 120);
    assert!(last.contains("fps"), "debug row missing fps: {last:?}");
    assert!(last.contains("p99"), "debug row missing p99: {last:?}");
    assert!(last.contains("act"), "debug row missing activity: {last:?}");
    assert!(
        last.contains("push"),
        "debug row missing push count: {last:?}"
    );
    assert!(last.contains("gap"), "debug row missing gap: {last:?}");

    // Debug off: no debug row.
    let ui_off = UiState::default();
    let buf = render(60, 10, &snap, &ui_off);
    let last = row(&buf, 9, 60);
    assert!(
        !last.contains("fps"),
        "debug row should be absent: {last:?}"
    );
    assert!(
        !last.contains("p99"),
        "debug row should be absent: {last:?}"
    );
}

#[test]
fn width_mismatch_is_handled() {
    // 64 bars into a narrow and a wide body: neither panics, and both fill the
    // full body width (some cell in each column is painted).
    let mut values = [0.0f32; 64];
    for (i, v) in values.iter_mut().enumerate() {
        *v = (i as f32 / 63.0).clamp(0.05, 1.0);
    }
    let snap = snapshot_with(&values);
    let ui = UiState::default();

    for width in [20u16, 200u16] {
        let buf = render(width, 12, &snap, &ui);
        // Body rows are y = 1..=11. Every column should have at least one
        // painted (non-space) body cell, since all bar values are > 0.
        for x in 0..width {
            let painted = (1..=11u16).any(|y| sym(&buf, x, y) != " ");
            assert!(painted, "width {width}: column {x} was blank");
        }
    }
}

/// A snapshot with known, distinctive signal values for the overlay tests: a
/// full spectrum ramp plus set band/flux/onset/beat/stereo fields.
fn overlay_snapshot() -> FeatureSnapshot {
    let mut snap = FeatureSnapshot::default();
    for i in 0..64 {
        snap.spectrum[i] = (i as f32 / 63.0).clamp(0.02, 1.0);
    }
    snap.spectrum_len = 64;
    snap.rms = 0.42;
    snap.peak = 0.87;
    snap.bands = [1.25, 0.90, 0.30];
    snap.flux = 0.55;
    snap.onset_age_ms = 20.0; // within the 150 ms lamp window -> lit
    snap.tempo_bpm = 128.0;
    snap.beat_confidence = 0.66;
    snap.mid_side_ratio = 0.40;
    snap
}

/// Join a range of rows `[y0, y1)` into one string, for substring assertions
/// across the multi-row overlay panel.
fn rows(buf: &Buffer, y0: u16, y1: u16, width: u16) -> String {
    (y0..y1).map(|y| row(buf, y, width)).collect()
}

#[test]
fn overlay_panel_shows_every_signal() {
    let snap = overlay_snapshot();
    let ui = UiState {
        overlay: true,
        tier: Some("octants"),
        source: "48000 Hz 2 ch".to_string(),
        ..UiState::default()
    };
    // 120x40: header + 39-row body; the panel covers body rows 35..=39.
    let buf = render(120, 40, &snap, &ui);
    let panel = rows(&buf, 35, 40, 120);

    assert!(panel.contains("fps"), "overlay missing fps: {panel:?}");
    assert!(
        panel.contains("dropped"),
        "overlay missing dropped: {panel:?}"
    );
    assert!(panel.contains("xruns"), "overlay missing xruns: {panel:?}");
    assert!(
        panel.contains("age"),
        "overlay missing feature age: {panel:?}"
    );
    assert!(
        panel.contains("push"),
        "overlay missing push cadence: {panel:?}"
    );
    assert!(
        panel.contains("bass"),
        "overlay missing band values: {panel:?}"
    );
    assert!(
        panel.contains("width"),
        "overlay missing stereo width: {panel:?}"
    );
    assert!(panel.contains("flux"), "overlay missing flux: {panel:?}");
    assert!(panel.contains("onset"), "overlay missing onset: {panel:?}");
    assert!(panel.contains('●'), "onset lamp should be lit: {panel:?}");
    assert!(panel.contains("beat"), "overlay missing beat: {panel:?}");
    assert!(
        panel.contains("128"),
        "overlay missing tempo bpm: {panel:?}"
    );
    assert!(
        panel.contains("tier octants"),
        "overlay missing tier label: {panel:?}"
    );
    assert!(
        panel.contains("schema v1"),
        "overlay missing schema: {panel:?}"
    );
    // The spectrum strip drew at least one eighth-block glyph.
    assert!(
        panel.contains('█') || panel.contains('▇') || panel.contains('▁'),
        "overlay missing spectrum strip: {panel:?}"
    );
}

#[test]
fn overlay_lamp_dims_without_recent_onset() {
    let mut snap = overlay_snapshot();
    snap.onset_age_ms = 5_000.0; // well past the 150 ms window
    let ui = UiState {
        overlay: true,
        ..UiState::default()
    };
    let buf = render(120, 40, &snap, &ui);
    let panel = rows(&buf, 35, 40, 120);
    assert!(panel.contains('○'), "onset lamp should be dim: {panel:?}");
    assert!(
        !panel.contains('●'),
        "onset lamp should not be lit: {panel:?}"
    );
}

#[test]
fn overlay_toggles() {
    let snap = overlay_snapshot();

    // Overlay on: the panel labels appear over the bottom of the body.
    let ui_on = UiState {
        overlay: true,
        ..UiState::default()
    };
    let buf = render(120, 40, &snap, &ui_on);
    assert!(
        rows(&buf, 35, 40, 120).contains("fps"),
        "overlay should be present when on"
    );

    // Overlay off: no panel text anywhere in the body.
    let ui_off = UiState::default();
    let buf = render(120, 40, &snap, &ui_off);
    assert!(
        !rows(&buf, 1, 40, 120).contains("fps"),
        "overlay should be absent when off"
    );
}

#[test]
fn overlay_off_is_byte_identical_to_no_overlay() {
    // A default UiState and an explicit overlay-off state must render the same
    // buffer: the overlay never perturbs the direct-bars body or header.
    let snap = overlay_snapshot();
    let base = render(120, 40, &snap, &UiState::default());
    let off = render(
        120,
        40,
        &snap,
        &UiState {
            overlay: false,
            ..UiState::default()
        },
    );
    assert_eq!(base, off, "overlay off must not change the rendered frame");
}

#[test]
fn overlay_falls_back_to_debug_line_on_a_small_pane() {
    // A body shorter than 10 rows has no room for the 5-row panel; the overlay
    // request degrades to the single debug line rather than panicking.
    let snap = overlay_snapshot();
    let ui = UiState {
        overlay: true,
        ..UiState::default()
    };
    // 120x8: header + 7-row body (< 10), so the fallback path is taken.
    let buf = render(120, 8, &snap, &ui);
    let last = row(&buf, 7, 120);
    assert!(
        last.contains("fps"),
        "fallback debug line missing fps: {last:?}"
    );
    assert!(
        last.contains("p99"),
        "fallback debug line missing p99: {last:?}"
    );
    // Only one row of overlay text, not a five-row panel: the row above the
    // fallback line carries no panel field.
    assert!(
        !row(&buf, 6, 120).contains("bass"),
        "small pane must not draw the full panel"
    );
}

#[test]
fn overlay_cost_under_frame_budget() {
    // The acceptance criterion is that *the overlay itself* costs < 5 % of the
    // frame budget. At 60 fps the budget is 1000/60 = 16.667 ms, so 5 % is
    // 0.833 ms; a per-frame overlay cost under 1.0 ms is under 6 % of it.
    //
    // The overlay's own cost is the delta between drawing the same frame with
    // the overlay on vs off — the body and header are identical in both, so the
    // difference is exactly what the overlay adds. We measure the two draws
    // interleaved over 300 frames so any load spike hits both equally, then
    // assert the two generous CI bounds and print both means and the delta:
    //   * turning the overlay on adds <= 0.8 ms over the overlay-off mean, and
    //   * the overlay's own per-frame cost is < 1.0 ms (< 6 % of the budget).
    let snap = overlay_snapshot();
    let (w, h) = (120u16, 40u16);
    let ui_off = UiState::default();
    let ui_on = UiState {
        overlay: true,
        ..UiState::default()
    };
    let (mean_off, mean_on) = interleaved_draw_ms(w, h, &snap, &ui_off, &ui_on, 300);
    let overlay_cost = mean_on - mean_off;
    println!(
        "overlay cost @ {w}x{h}: off {mean_off:.4} ms, on {mean_on:.4} ms, \
         overlay {overlay_cost:.4} ms (budget 16.667 ms, 5% = 0.833 ms)"
    );

    assert!(
        overlay_cost <= 0.8,
        "overlay added {overlay_cost:.4} ms (> 0.8 ms) over the {mean_off:.4} ms baseline"
    );
    assert!(
        overlay_cost < 1.0,
        "overlay per-frame cost {overlay_cost:.4} ms should be < 1.0 ms (< 6% of the budget)"
    );
}

/// Mean wall-clock time (ms) of one `draw` with `off` vs `on`, measured
/// interleaved over `n` frames after a warm-up. Two reused `TestBackend`
/// terminals isolate the render cost; interleaving keeps a load spike from
/// biasing one configuration over the other. Returns `(mean_off, mean_on)`.
fn interleaved_draw_ms(
    w: u16,
    h: u16,
    snap: &FeatureSnapshot,
    off: &UiState,
    on: &UiState,
    n: u32,
) -> (f64, f64) {
    let mut term_off = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    let mut term_on = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    for _ in 0..30 {
        term_off.draw(|frame| draw(frame, snap, off)).expect("draw");
        term_on.draw(|frame| draw(frame, snap, on)).expect("draw");
    }
    let mut sum_off = 0.0f64;
    let mut sum_on = 0.0f64;
    for _ in 0..n {
        let t0 = Instant::now();
        term_off.draw(|frame| draw(frame, snap, off)).expect("draw");
        sum_off += t0.elapsed().as_secs_f64();
        let t1 = Instant::now();
        term_on.draw(|frame| draw(frame, snap, on)).expect("draw");
        sum_on += t1.elapsed().as_secs_f64();
    }
    let to_ms = |s: f64| s * 1000.0 / f64::from(n);
    (to_ms(sum_off), to_ms(sum_on))
}

#[test]
fn debug_line_shows_active_tier() {
    // When a scene presenter drives the body it sets the tier label; the debug
    // line surfaces it. The direct-bars path leaves `tier` unset (covered by
    // `debug_line_toggles`, whose debug row carries no tier).
    let snap = snapshot_with(&[0.3; 16]);
    let ui = UiState {
        debug: true,
        tier: Some("octants"),
        ..UiState::default()
    };
    let buf = render(120, 10, &snap, &ui);
    let last = row(&buf, 9, 120);
    assert!(
        last.contains("tier octants"),
        "debug row missing tier: {last:?}"
    );
}
