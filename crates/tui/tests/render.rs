//! Headless rendering tests: drive [`scia_tui::draw`] into a ratatui
//! `TestBackend` and assert on the resulting cell buffer. These run with no
//! TTY.

use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use scia_core::{Activity, EngineStats, FeatureSnapshot};
use scia_tui::{ChromeMode, ChromeState, SceneNav, UiState, draw};

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
fn header_shows_fullscreen_paused_marker() {
    // When rendering is paused for a foreground fullscreen app, the header leads
    // the activity indicator with a `fullscreen paused` marker (US-PERF-3).
    let snap = snapshot_with(&[0.5; 8]);
    let ui = UiState {
        fullscreen_paused: true,
        ..ui_with_activity(Activity::Idle)
    };
    let buf = render(70, 10, &snap, &ui);
    let header = row(&buf, 0, 70);
    assert!(
        header.contains("fullscreen paused"),
        "header missing the fullscreen-paused marker: {header:?}"
    );
}

#[test]
fn header_has_no_fullscreen_marker_when_not_paused() {
    let snap = snapshot_with(&[0.5; 8]);
    let ui = ui_with_activity(Activity::Active);
    let buf = render(70, 10, &snap, &ui);
    let header = row(&buf, 0, 70);
    assert!(
        !header.contains("fullscreen"),
        "header should not mention fullscreen when not paused: {header:?}"
    );
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
    // Locked form: `beat 128bpm · conf 0.66` — the tempo and confidence are
    // labelled distinctly so the confidence can't be misread as the BPM.
    assert!(
        panel.contains("128bpm"),
        "overlay missing locked tempo bpm: {panel:?}"
    );
    assert!(
        panel.contains("conf 0.66"),
        "overlay missing beat confidence: {panel:?}"
    );
    assert!(
        panel.contains("tier octants"),
        "overlay missing tier label: {panel:?}"
    );
    assert!(
        panel.contains("schema v2"),
        "overlay missing schema: {panel:?}"
    );
    // The spectrum strip drew at least one eighth-block glyph.
    assert!(
        panel.contains('█') || panel.contains('▇') || panel.contains('▁'),
        "overlay missing spectrum strip: {panel:?}"
    );
}

#[test]
fn overlay_beat_unlocked_reads_dash_not_zero_bpm() {
    // Unlocked: `tempo_bpm == 0.0`. The beat segment must read `beat — · conf X`,
    // never `beat 0 bpm 0.43`, so the confidence is not mistaken for the BPM.
    let mut snap = overlay_snapshot();
    snap.tempo_bpm = 0.0;
    snap.beat_confidence = 0.43;
    let ui = UiState {
        overlay: true,
        ..UiState::default()
    };
    let buf = render(120, 40, &snap, &ui);
    let panel = rows(&buf, 35, 40, 120);
    assert!(
        panel.contains("beat —"),
        "unlocked beat should read an em dash, not a bpm: {panel:?}"
    );
    assert!(
        panel.contains("conf 0.43"),
        "unlocked beat should still show confidence: {panel:?}"
    );
    assert!(
        !panel.contains("beat 0bpm") && !panel.contains("0 bpm"),
        "unlocked beat must not present a zero bpm: {panel:?}"
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
fn browser_panel_falls_back_gracefully_on_a_small_pane() {
    // Open the browser committed to spectra (index 0) and highlight lattice
    // (index 1), then draw it over the direct-bars body headlessly.
    let mut nav = SceneNav::new(0);
    nav.toggle_browser();
    nav.highlight_next();
    let snap = snapshot_with(&[0.3, 0.6, 0.9, 0.4]);
    let ui = UiState {
        scene_nav: nav,
        ..UiState::default()
    };

    // Roomy pane: the full list renders — every scene name and its mood.
    let big = render(120, 40, &snap, &ui);
    let panel = rows(&big, 1, 40, 120);
    assert!(
        panel.contains("scenes"),
        "full panel has a title: {panel:?}"
    );
    assert!(
        panel.contains("spectra") && panel.contains("starfall"),
        "full panel lists every scene: {panel:?}"
    );
    assert!(
        panel.contains("kinetic"),
        "full panel shows moods: {panel:?}"
    );

    // Small pane: a 4-row body cannot host the six-row list, so it degrades to a
    // single summary line naming the highlighted scene — no panic, no full list.
    let small = render(120, 5, &snap, &ui);
    let body = rows(&small, 1, 5, 120);
    assert!(
        body.contains("lattice"),
        "fallback names the highlighted scene: {body:?}"
    );
    assert!(
        !body.contains("kinetic"),
        "small pane must not draw the full list: {body:?}"
    );
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
fn chrome_instrument_rail_draws_over_the_scene_body() {
    // The instrument rail lands on the body's bottom row with its VU meters and
    // fps, over the direct-bars body.
    let snap = overlay_snapshot();
    let ui = UiState {
        source: "48000 Hz 2 ch".to_string(),
        fps_measured: 60.0,
        chrome: ChromeState::new(ChromeMode::Instrument),
        ..UiState::default()
    };
    let buf = render(120, 20, &snap, &ui);
    // Body rows are y = 1..=19; the rail is on the bottom body row (y = 19).
    let rail = row(&buf, 19, 120);
    assert!(rail.contains("vu"), "instrument rail missing VU: {rail:?}");
    assert!(
        rail.contains("fps"),
        "instrument rail missing fps: {rail:?}"
    );
}

#[test]
fn chrome_yields_to_the_debug_overlay_when_both_are_visible() {
    // With the instrument rail AND the debug overlay both on, the debug overlay
    // is a separate surface layered above the chrome: its claimed rows carry the
    // overlay's fields, never the chrome rail. The rail (its "vu" marker) must
    // not survive anywhere the overlay claimed.
    let snap = overlay_snapshot();
    let ui = UiState {
        source: "48000 Hz 2 ch".to_string(),
        overlay: true,
        tier: Some("octants"),
        fps_measured: 60.0,
        chrome: ChromeState::new(ChromeMode::Instrument),
        ..UiState::default()
    };
    // 120x40: header + 39-row body; the overlay panel covers body rows 35..=39.
    let buf = render(120, 40, &snap, &ui);
    let panel = rows(&buf, 35, 40, 120);
    assert!(
        panel.contains("bass") && panel.contains("schema v2"),
        "the debug overlay owns its claimed rows: {panel:?}"
    );
    // The instrument rail's VU marker must not appear anywhere — the overlay
    // covers the bottom row it would have drawn on, and it claims no other row.
    let whole: String = (1..40).map(|y| row(&buf, y, 120)).collect();
    assert!(
        !whole.contains("vu "),
        "chrome must not draw over the debug overlay's rows"
    );
}

#[test]
fn chrome_utilitarian_coexists_with_the_debug_line() {
    // The debug line (the `d` toggle) reserves the frame's bottom row through the
    // layout; the utilitarian chrome row sits on the body's own bottom row just
    // above it. Both are legible, neither overwrites the other.
    let mut snap = overlay_snapshot();
    snap.tempo_bpm = 120.0;
    let ui = UiState {
        source: "48000 Hz 2 ch".to_string(),
        debug: true,
        tier: Some("octants"),
        fps_measured: 60.0,
        chrome: ChromeState::new(ChromeMode::Utilitarian),
        ..UiState::default()
    };
    // 120x12: header (y=0), body y=1..=10, debug line y=11.
    let buf = render(120, 12, &snap, &ui);
    let debug_line = row(&buf, 11, 120);
    assert!(
        debug_line.contains("p99"),
        "the debug line keeps its reserved bottom row: {debug_line:?}"
    );
    let status = row(&buf, 10, 120);
    assert!(
        status.contains("octants") && status.contains("120bpm"),
        "the utilitarian row draws on the body bottom, above the debug line: {status:?}"
    );
}

#[test]
fn chrome_invisible_default_leaves_the_bars_body_untouched() {
    // The default (invisible) chrome with nothing playing must not perturb the
    // direct-bars body: a default UiState renders exactly as an explicit
    // invisible one with no track.
    let snap = overlay_snapshot();
    let base = render(120, 20, &snap, &UiState::default());
    let explicit = render(
        120,
        20,
        &snap,
        &UiState {
            chrome: ChromeState::new(ChromeMode::Invisible),
            ..UiState::default()
        },
    );
    assert_eq!(
        base, explicit,
        "invisible chrome with no track draws nothing"
    );
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

#[test]
fn help_overlay_lists_active_bindings_and_degrades() {
    // A roomy body shows the full panel with the default keys and action labels.
    let snap = snapshot_with(&[0.3; 16]);
    let ui = UiState {
        help: true,
        ..UiState::default()
    };
    let buf = render(60, 24, &snap, &ui);
    let text: String = (0..24)
        .flat_map(|y| (0..60u16).map(move |x| (x, y)))
        .map(|(x, y)| sym(&buf, x, y))
        .collect();
    assert!(text.contains("keys"), "help panel missing title: {text:?}");
    assert!(
        text.contains("pause"),
        "help panel missing pause row: {text:?}"
    );
    assert!(
        text.contains("quit"),
        "help panel missing quit row: {text:?}"
    );

    // A tiny body must not panic and falls back to the single summary line.
    let small = render(12, 4, &snap, &ui);
    let top = row(&small, 1, 12);
    assert!(
        top.contains("keys"),
        "small-pane help should show the fallback line: {top:?}"
    );
}

#[test]
fn help_overlay_reflects_a_rebind() {
    use scia_tui::{InputAction, Keymap, parse_chord};

    let snap = snapshot_with(&[0.3; 16]);
    let mut keymap = Keymap::default();
    keymap.rebind(InputAction::Quit, Some(parse_chord("x").unwrap()));
    let ui = UiState {
        help: true,
        keymap,
        ..UiState::default()
    };
    let buf = render(60, 24, &snap, &ui);
    // The quit row should now show `x`, sitting just before the "quit" label.
    let mut found = false;
    for y in 0..24 {
        let line = row(&buf, y, 60);
        if line.contains("quit") && line.contains('x') {
            found = true;
            break;
        }
    }
    assert!(found, "help overlay should show the rebound quit key");
}
