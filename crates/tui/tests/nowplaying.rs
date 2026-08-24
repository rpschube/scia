//! Headless tests for the now-playing panel: drive [`scia_tui::draw`] into a
//! ratatui `TestBackend` with a populated [`NowPlayingState`] and assert on the
//! resulting cells — the fields, swatches, progress bar, and the small-pane
//! fallback.

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

use scia_core::FeatureSnapshot;
use scia_meta::{ArtPalette, NowPlaying, PlaybackStatus, PositionInfo, PreviewImage};
use scia_tui::{TrackArt, UiState, draw};

/// Render one frame at `w`×`h` and return the buffer.
fn render(w: u16, h: u16, ui: &UiState) -> Buffer {
    let snap = FeatureSnapshot::default();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, &snap, ui)).expect("draw");
    terminal.backend().buffer().clone()
}

/// The whole buffer flattened to one string, for substring assertions.
fn all_text(buf: &Buffer) -> String {
    let area = *buf.area();
    let mut s = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            s.push_str(buf.cell((x, y)).expect("cell").symbol());
        }
        s.push('\n');
    }
    s
}

/// Whether any cell in the buffer has the given foreground colour.
fn has_fg(buf: &Buffer, color: Color) -> bool {
    let area = *buf.area();
    for y in 0..area.height {
        for x in 0..area.width {
            if buf.cell((x, y)).expect("cell").fg == color {
                return true;
            }
        }
    }
    false
}

/// Eight visibly distinct slots so a swatch colour is unmistakable.
fn distinct_palette() -> ArtPalette {
    let slots = [
        [200, 20, 20],
        [20, 200, 20],
        [20, 20, 200],
        [200, 200, 20],
        [200, 20, 200],
        [20, 200, 200],
        [120, 60, 30],
        [240, 240, 240],
    ];
    ArtPalette {
        dominant: slots[0],
        accents: vec![slots[1], slots[2]],
        light: [255, 120, 120],
        dark: [80, 0, 0],
        slots,
    }
}

fn preview() -> PreviewImage {
    // A tiny 4×4 gradient; the panel nearest-samples it up to the art cells.
    let mut pixels = Vec::new();
    for y in 0..4u8 {
        for x in 0..4u8 {
            pixels.push([x * 40, y * 40, 128]);
        }
    }
    PreviewImage {
        width: 4,
        height: 4,
        pixels,
    }
}

fn playing_track() -> NowPlaying {
    NowPlaying::new(
        Some("Song".to_string()),
        Some("Band".to_string()),
        Some("Record".to_string()),
        PlaybackStatus::Playing,
        Some(PositionInfo {
            position: Duration::from_secs(30),
            length: Some(Duration::from_secs(200)),
            reported_at: Instant::now(),
        }),
        None,
    )
}

/// A UiState with the panel open and a fully populated now-playing state.
fn populated_ui() -> UiState {
    let track = playing_track();
    let key = track.track_key.clone();
    let mut ui = UiState {
        show_now_playing: true,
        ..UiState::default()
    };
    ui.now_playing.current = Some(track);
    ui.now_playing.art = Some(TrackArt {
        track_key: key,
        preview: preview(),
        palette: distinct_palette(),
    });
    ui
}

#[test]
fn panel_shows_track_fields() {
    let ui = populated_ui();
    let buf = render(70, 30, &ui);
    let text = all_text(&buf);
    assert!(text.contains("now playing"), "panel title present:\n{text}");
    assert!(text.contains("Song"), "title line present:\n{text}");
    assert!(text.contains("Band"), "artist line present:\n{text}");
    assert!(text.contains("Record"), "album line present:\n{text}");
}

#[test]
fn panel_shows_progress_bar() {
    let ui = populated_ui();
    let buf = render(70, 30, &ui);
    let text = all_text(&buf);
    // 30 s of a 3:20 track: both timestamps rendered.
    assert!(text.contains("0:30"), "elapsed timestamp present:\n{text}");
    assert!(text.contains("3:20"), "length timestamp present:\n{text}");
    // The bar draws filled and empty cells.
    assert!(text.contains('█'), "filled bar cells present");
    assert!(text.contains('░'), "empty bar cells present");
}

#[test]
fn panel_shows_palette_swatches() {
    let ui = populated_ui();
    let buf = render(70, 30, &ui);
    // Several of the distinct slot colours appear as swatch foregrounds.
    let pal = distinct_palette();
    for slot in [pal.slots[0], pal.slots[3], pal.slots[7]] {
        assert!(
            has_fg(&buf, Color::Rgb(slot[0], slot[1], slot[2])),
            "swatch colour {slot:?} must appear"
        );
    }
}

#[test]
fn panel_degrades_to_one_line_on_a_small_pane() {
    // A body narrower than the panel minimum falls back to the summary line.
    let ui = populated_ui();
    let buf = render(20, 14, &ui);
    let text = all_text(&buf);
    assert!(
        text.contains("Song") && text.contains("Band"),
        "the fallback summary names the track:\n{text}"
    );
    // The full panel's title chrome is not drawn in the fallback.
    assert!(
        !text.contains("now playing"),
        "the small pane uses the one-line summary, not the full panel:\n{text}"
    );
}

#[test]
fn panel_says_nothing_playing_when_empty() {
    let ui = UiState {
        show_now_playing: true,
        ..UiState::default()
    };
    let buf = render(50, 20, &ui);
    assert!(
        all_text(&buf).contains("nothing playing"),
        "an empty state reads as a quiet nothing-playing"
    );
}

#[test]
fn panel_marks_an_applied_palette() {
    let mut ui = populated_ui();
    ui.palette_applied = true;
    let buf = render(70, 30, &ui);
    assert!(
        all_text(&buf).contains("palette applied"),
        "the title notes when the art palette is applied"
    );
}

#[test]
fn hidden_panel_draws_nothing_extra() {
    // With the toggle off the panel is inert: no title, no swatches.
    let mut ui = populated_ui();
    ui.show_now_playing = false;
    let buf = render(70, 30, &ui);
    assert!(!all_text(&buf).contains("now playing"));
}
