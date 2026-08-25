//! Headless render tests for the WSL guidance overlay (storyboard 1n): drive
//! [`scia_tui::draw_wsl`] into a bare ratatui [`Buffer`] and assert on the
//! painted cells. No TTY, no audio hardware — the model is pure.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use scia_tui::{BRIDGE_COMMAND, WslScreen, draw_wsl};

/// Render the overlay over a `w`×`h` body and return the buffer.
fn render(w: u16, h: u16, screen: &WslScreen) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    draw_wsl(&mut buf, area, screen);
    buf
}

/// Concatenate a whole row into a string.
fn row(buf: &Buffer, y: u16, width: u16) -> String {
    (0..width)
        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
        .collect()
}

/// The whole buffer flattened to one string.
fn all(buf: &Buffer, w: u16, h: u16) -> String {
    (0..h)
        .map(|y| row(buf, y, w))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn closed_screen_paints_nothing() {
    let screen = WslScreen::new(false);
    let buf = render(60, 16, &screen);
    let painted = all(&buf, 60, 16);
    assert!(
        painted.chars().all(|c| c == ' ' || c == '\n'),
        "a closed screen paints nothing: {painted:?}"
    );
}

#[test]
fn open_screen_shows_title_steps_command_and_keys() {
    let screen = WslScreen::new(true);
    let buf = render(72, 16, &screen);
    let text = all(&buf, 72, 16);
    assert!(text.contains("WSL"), "the title shows: {text:?}");
    assert!(
        text.contains("not visible"),
        "the honest framing shows: {text:?}"
    );
    assert!(text.contains("1."), "a numbered step shows: {text:?}");
    assert!(text.contains("2."), "a numbered step shows: {text:?}");
    assert!(
        text.contains("scia-bridge --listen"),
        "the copy-able command shows: {text:?}"
    );
    // The key hint names all three action keys and the docs file.
    assert!(text.contains("[c]"), "the copy key shows: {text:?}");
    assert!(text.contains("[s]"), "the demo key shows: {text:?}");
    assert!(text.contains("[?]"), "the docs key shows: {text:?}");
    assert!(
        text.contains("docs/wsl.md"),
        "the docs pointer shows: {text:?}"
    );
    // The command constant is exactly what the copy key would place.
    assert_eq!(screen.command(), BRIDGE_COMMAND);
}

#[test]
fn copied_state_is_acknowledged_in_the_hint() {
    let mut screen = WslScreen::new(true);
    screen.mark_copied();
    let buf = render(72, 16, &screen);
    let text = all(&buf, 72, 16);
    assert!(
        text.contains("copied"),
        "the copy is acknowledged: {text:?}"
    );
}

#[test]
fn small_pane_degrades_to_a_single_line_without_panicking() {
    let screen = WslScreen::new(true);
    // A pane far too small for the full panel falls back to the summary line and
    // never overflows or panics.
    let buf = render(14, 3, &screen);
    let text = all(&buf, 14, 3);
    assert!(text.contains("WSL"), "the fallback line shows: {text:?}");
    for y in 0..3 {
        assert_eq!(row(&buf, y, 14).chars().count(), 14, "no row overflows");
    }
}

#[test]
fn tiny_sizes_never_panic() {
    // Exercise a range of degenerate and tight sizes; the only assertion is that
    // rendering returns.
    let screen = WslScreen::new(true);
    for w in 0..10u16 {
        for h in 0..10u16 {
            let _ = render(w, h, &screen);
        }
    }
}
