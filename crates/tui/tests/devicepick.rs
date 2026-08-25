//! Headless render tests for the device picker overlay: drive
//! [`scia_tui::draw_devices`] into a bare ratatui [`Buffer`] and assert on the
//! painted cells. No TTY, no audio hardware — the model is fed fixtures.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use scia_core::{DeviceInfo, DeviceKind, DeviceSelector};
use scia_tui::{DevicePicker, draw_devices};

/// Render a picker over a `w`×`h` body and return the buffer.
fn render(w: u16, h: u16, picker: &DevicePicker) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    draw_devices(&mut buf, area, picker);
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

/// A capture-target device that survives the filter on the Linux dev/CI host and
/// on Windows (output endpoints are the capture targets on both).
fn out(name: &str, host: &str, default: bool) -> DeviceInfo {
    DeviceInfo {
        name: name.to_owned(),
        is_default_input: false,
        is_default_output: default,
        kind: DeviceKind::Output,
        host: host.to_owned(),
    }
}

#[test]
fn closed_picker_paints_nothing() {
    let picker = DevicePicker::new(DeviceSelector::Default, true);
    let buf = render(40, 12, &picker);
    let painted = all(&buf, 40, 12);
    assert!(
        painted.chars().all(|c| c == ' ' || c == '\n'),
        "a closed picker paints nothing: {painted:?}"
    );
}

#[test]
fn loading_state_shows_the_placeholder_and_chrome() {
    let mut picker = DevicePicker::new(DeviceSelector::Default, true);
    picker.open_enumerating();
    let buf = render(40, 12, &picker);
    let text = all(&buf, 40, 12);
    assert!(text.contains("capture device"), "the title shows: {text:?}");
    assert!(
        text.contains("enumerating"),
        "the placeholder shows: {text:?}"
    );
    assert!(text.contains("switch"), "the key hint shows: {text:?}");
}

#[test]
fn ready_state_marks_the_active_follow_system_row() {
    // Default active: the follow-system row is present and marked, whatever the
    // platform filter does to the device rows.
    let mut picker = DevicePicker::new(DeviceSelector::Default, true);
    picker.open_enumerating();
    picker.set_devices(Ok(vec![out("hdmi", "pipewire", true)]));
    let buf = render(48, 12, &picker);
    let text = all(&buf, 48, 12);
    assert!(
        text.contains("Default (follow system)"),
        "the follow-system row shows: {text:?}"
    );
    assert!(text.contains('●'), "the active marker shows: {text:?}");
    // On the dev/CI platforms the output endpoint is a capture target and shows.
    if picker.rows().len() > 1 {
        assert!(text.contains("hdmi"), "the device row shows: {text:?}");
    }
}

#[test]
fn error_state_shows_the_error_row() {
    let mut picker = DevicePicker::new(DeviceSelector::Default, true);
    picker.open_enumerating();
    picker.set_devices(Err("no host available".to_owned()));
    let buf = render(48, 12, &picker);
    let text = all(&buf, 48, 12);
    assert!(text.contains("error"), "the error row shows: {text:?}");
    assert!(text.contains("no host"), "the message shows: {text:?}");
}

#[test]
fn long_name_truncates_with_ellipsis_on_a_narrow_panel() {
    let mut picker = DevicePicker::new(DeviceSelector::Default, true);
    picker.open_enumerating();
    picker.set_devices(Ok(vec![out(
        "A Very Long Capture Device Name That Will Not Fit",
        "pipewire",
        false,
    )]));
    // Only assert truncation where the device row actually materializes.
    if picker.rows().len() > 1 {
        let buf = render(30, 12, &picker);
        let text = all(&buf, 30, 12);
        assert!(
            text.contains('…'),
            "a truncated name shows an ellipsis: {text:?}"
        );
        // The body never overflows: no row is wider than the pane.
        for y in 0..12 {
            assert_eq!(row(&buf, y, 30).chars().count(), 30);
        }
    }
}

#[test]
fn small_pane_degrades_to_a_single_line() {
    let mut picker = DevicePicker::new(DeviceSelector::Default, true);
    picker.open_enumerating();
    picker.set_devices(Ok(vec![out("hdmi", "pipewire", true)]));
    // A pane too narrow for the panel falls back to the summary line.
    let buf = render(12, 6, &picker);
    let text = all(&buf, 12, 6);
    assert!(
        text.contains("devices"),
        "the fallback line shows: {text:?}"
    );
}
