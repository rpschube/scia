//! Headless rendering tests: drive [`scia_tui::draw`] into a ratatui
//! `TestBackend` and assert on the resulting cell buffer. These run with no
//! TTY.

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
