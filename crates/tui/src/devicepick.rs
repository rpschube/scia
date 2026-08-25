//! The device picker overlay: a modal list of the capture-relevant audio
//! endpoints for the current platform path, with a live switch, a config pin,
//! and an explicit "follow the system default" entry at the top (storyboard
//! 1l).
//!
//! The model ([`DevicePicker`]) is pure and TTY-free: it holds the enumeration
//! state (loading / ready rows / error), the selection, the currently active
//! selector (to mark the live device), and the PipeWire preference (to pick the
//! platform capture direction). Enumeration itself blocks — device probing can
//! stall — so the render loop runs it on a spawned thread and feeds the result
//! back through [`set_devices`](DevicePicker::set_devices); the model never
//! enumerates on the UI thread.
//!
//! The platform-direction filter is extracted pure ([`capture_filter`]) so every
//! branch is unit-tested from fixtures: Windows and the PipeWire-host path treat
//! **output** endpoints as loopback capture targets, while plain ALSA and macOS
//! capture **inputs** — the same rule [`resolve_device`] applies in the cpal
//! backend, and the same set `--list-devices` shows as capture targets.
//!
//! Pinning writes the selected device into the config file's `[defaults] device`
//! key, comment-preserving via `toml_edit`, reusing the tuning strip's atomic
//! write helper; pinning the follow-system entry removes the key.
//!
//! [`resolve_device`]: scia_core

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use toml_edit::{DocumentMut, Item, Table, Value, value as toml_value};

use scia_core::{DeviceInfo, DeviceKind, DeviceSelector};

use crate::palette;
use crate::tuning::atomic_write;

/// The label of the always-present top entry that follows the system default.
pub const FOLLOW_SYSTEM_LABEL: &str = "Default (follow system)";

/// The config table and key a pin writes under (`[defaults] device`), matching
/// the other CLI-mirroring defaults.
const CONFIG_TABLE: &str = "defaults";
const CONFIG_KEY: &str = "device";

/// The platform whose capture-direction rule applies. Split out (rather than
/// read from `cfg!` inline) so [`capture_filter`] is a pure function every
/// branch of which is testable on any host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Windows (WASAPI loopback on the default output endpoint).
    Windows,
    /// macOS (Core Audio input).
    MacOs,
    /// Linux (ALSA input, or the PipeWire sink monitor when preferred).
    Linux,
    /// Any other target (default input).
    Other,
}

/// Which enumerated devices serve as capture targets on a platform path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureFilter {
    /// The device kind that is a capture target.
    pub kind: DeviceKind,
    /// Restrict to this cpal host (its lowercased name, e.g. `"pipewire"`) when
    /// the path binds one host specifically; `None` accepts any host with the
    /// right kind.
    pub host: Option<String>,
}

impl CaptureFilter {
    /// Whether `d` is a capture target under this filter.
    #[must_use]
    pub fn accepts(&self, d: &DeviceInfo) -> bool {
        d.kind == self.kind
            && self
                .host
                .as_deref()
                .is_none_or(|h| h.eq_ignore_ascii_case(&d.host))
    }
}

/// The capture-target filter for a platform path — pure, so all branches are
/// tested from fixtures.
///
/// Windows and the PipeWire-host path capture **output** endpoints (opened as
/// loopback / monitor inputs); plain ALSA, macOS and the fallback capture
/// **inputs**. `has_pipewire_host` is whether the enumerated devices include a
/// PipeWire host — inferred at runtime so this need not know the compiled cpal
/// feature set.
#[must_use]
pub fn capture_filter(
    platform: Platform,
    prefer_pipewire: bool,
    has_pipewire_host: bool,
) -> CaptureFilter {
    match platform {
        Platform::Windows => CaptureFilter {
            kind: DeviceKind::Output,
            host: None,
        },
        Platform::Linux if prefer_pipewire && has_pipewire_host => CaptureFilter {
            kind: DeviceKind::Output,
            host: Some("pipewire".to_owned()),
        },
        Platform::Linux | Platform::MacOs | Platform::Other => CaptureFilter {
            kind: DeviceKind::Input,
            host: None,
        },
    }
}

/// The platform this build runs on.
#[must_use]
fn current_platform() -> Platform {
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
    #[cfg(target_os = "macos")]
    {
        Platform::MacOs
    }
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Platform::Other
    }
}

/// The capture-target filter for the current platform, given the PipeWire
/// preference and the enumerated devices (from which PipeWire-host availability
/// is inferred).
#[must_use]
pub fn platform_filter(devices: &[DeviceInfo], prefer_pipewire: bool) -> CaptureFilter {
    let has_pipewire = devices
        .iter()
        .any(|d| d.host.eq_ignore_ascii_case("pipewire"));
    capture_filter(current_platform(), prefer_pipewire, has_pipewire)
}

/// Whether two selectors name the same device.
#[must_use]
fn selectors_match(a: &DeviceSelector, b: &DeviceSelector) -> bool {
    match (a, b) {
        (DeviceSelector::Default, DeviceSelector::Default) => true,
        (DeviceSelector::Named(x), DeviceSelector::Named(y)) => x == y,
        _ => false,
    }
}

/// One row in the picker: the follow-system entry or an enumerated endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    /// The display name (the follow-system label, or the cpal device name).
    pub name: String,
    /// The selector this row switches to.
    pub selector: DeviceSelector,
    /// Whether this endpoint is the host default for the capture direction.
    pub is_default: bool,
    /// Whether this is the currently active device.
    pub active: bool,
}

/// Build the picker rows from enumerated devices: the follow-system entry first,
/// then every capture target `filter` accepts in enumeration order. `active`
/// marks the row for the currently bound device.
#[must_use]
pub fn build_rows(
    devices: &[DeviceInfo],
    filter: &CaptureFilter,
    active: &DeviceSelector,
) -> Vec<DeviceRow> {
    let mut rows = Vec::with_capacity(devices.len() + 1);
    rows.push(DeviceRow {
        name: FOLLOW_SYSTEM_LABEL.to_owned(),
        selector: DeviceSelector::Default,
        is_default: false,
        active: matches!(active, DeviceSelector::Default),
    });
    for d in devices.iter().filter(|d| filter.accepts(d)) {
        let selector = DeviceSelector::Named(d.name.clone());
        let is_default = match filter.kind {
            DeviceKind::Output => d.is_default_output,
            DeviceKind::Input => d.is_default_input,
        };
        rows.push(DeviceRow {
            active: selectors_match(active, &selector),
            name: d.name.clone(),
            selector,
            is_default,
        });
    }
    rows
}

/// The enumeration state shown in the overlay.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum EnumState {
    /// Enumeration is running on the worker thread.
    #[default]
    Loading,
    /// Enumeration finished: the capture-target rows.
    Ready(Vec<DeviceRow>),
    /// Enumeration failed with this message.
    Error(String),
}

/// The device-picker model: open state, enumeration state, selection, the active
/// selector (for the marker), and the PipeWire preference (for the filter).
#[derive(Clone, Debug)]
pub struct DevicePicker {
    open: bool,
    state: EnumState,
    selected: usize,
    active: DeviceSelector,
    prefer_pipewire: bool,
}

impl Default for DevicePicker {
    fn default() -> Self {
        Self {
            open: false,
            state: EnumState::Loading,
            selected: 0,
            active: DeviceSelector::Default,
            prefer_pipewire: true,
        }
    }
}

impl DevicePicker {
    /// A picker seeded with the currently active selector and the PipeWire
    /// preference the capture backend runs with.
    #[must_use]
    pub fn new(active: DeviceSelector, prefer_pipewire: bool) -> Self {
        Self {
            active,
            prefer_pipewire,
            ..Self::default()
        }
    }

    /// Whether the overlay is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The enumeration state.
    #[must_use]
    pub fn state(&self) -> &EnumState {
        &self.state
    }

    /// The selected row index.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Open the overlay into the enumerating state, clearing any prior result so
    /// a re-open always re-enumerates.
    pub fn open_enumerating(&mut self) {
        self.open = true;
        self.state = EnumState::Loading;
        self.selected = 0;
    }

    /// Close the overlay.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Fold an enumeration result into the model, building the capture-target
    /// rows (or an error row). The selection snaps to the active device's row
    /// when it is present, else to the top (follow-system).
    pub fn set_devices(&mut self, result: Result<Vec<DeviceInfo>, String>) {
        match result {
            Ok(devices) => {
                let filter = platform_filter(&devices, self.prefer_pipewire);
                let rows = build_rows(&devices, &filter, &self.active);
                self.selected = rows.iter().position(|r| r.active).unwrap_or(0);
                self.state = EnumState::Ready(rows);
            }
            Err(msg) => {
                self.selected = 0;
                self.state = EnumState::Error(msg);
            }
        }
    }

    /// The rows, when enumeration has produced them.
    #[must_use]
    pub fn rows(&self) -> &[DeviceRow] {
        match &self.state {
            EnumState::Ready(rows) => rows,
            _ => &[],
        }
    }

    /// Move the selection to the next row, wrapping. Inert until rows exist.
    pub fn select_next(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    /// Move the selection to the previous row, wrapping. Inert until rows exist.
    pub fn select_prev(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
        }
    }

    /// The selected row, when one exists.
    #[must_use]
    pub fn selected_row(&self) -> Option<&DeviceRow> {
        self.rows().get(self.selected)
    }

    /// Record the newly active selector (after a switch) so a later re-open marks
    /// it.
    pub fn set_active(&mut self, selector: DeviceSelector) {
        self.active = selector;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The chrome rows the panel needs beyond one row per device: a title row and a
/// bottom hint row.
const PANEL_CHROME_ROWS: u16 = 2;
/// The narrowest body that still hosts the full panel; below it (or when the
/// body is too short) the overlay degrades to a single summary line.
const PANEL_MIN_WIDTH: u16 = 28;

/// Paint the device picker over the live canvas, top-left, like the scene
/// browser. Draws nothing when the picker is closed or the body is degenerate.
/// Degrades to a single line on a small pane; long names truncate with `…`.
pub fn draw_devices(buf: &mut Buffer, body: Rect, picker: &DevicePicker) {
    if !picker.open || body.width == 0 || body.height == 0 {
        return;
    }
    let lines = picker_lines(picker);
    let needed = lines.len() as u16 + PANEL_CHROME_ROWS;
    if body.height < needed || body.width < PANEL_MIN_WIDTH {
        render_line(buf, body, &lines);
    } else {
        render_panel(buf, body, &lines, picker.selected);
    }
}

/// The content rows (excluding title/hint) as `(text, active)` pairs: the
/// enumeration placeholder, the error, or one line per device row.
fn picker_lines(picker: &DevicePicker) -> Vec<(String, bool)> {
    match picker.state() {
        EnumState::Loading => vec![("enumerating…".to_owned(), false)],
        EnumState::Error(msg) => vec![(format!("error: {msg}"), false)],
        EnumState::Ready(rows) => rows
            .iter()
            .map(|r| {
                let mark = if r.active { '●' } else { '·' };
                let flag = if r.is_default { " (default)" } else { "" };
                (format!("{mark} {}{flag}", r.name), r.active)
            })
            .collect(),
    }
}

/// The small-pane fallback: one line naming the selected device (or the state).
fn render_line(buf: &mut Buffer, body: Rect, lines: &[(String, bool)]) {
    let style = Style::new()
        .fg(palette::OVERLAY_FG)
        .bg(palette::OVERLAY_BG)
        .add_modifier(Modifier::BOLD);
    buf.set_stringn(body.x, body.y, " devices ", body.width as usize, style);
    if body.height >= 2 {
        // Show the first content line (the placeholder, error, or first device).
        if let Some((text, _)) = lines.first() {
            buf.set_stringn(
                body.x,
                body.y + 1,
                truncate(text, body.width as usize),
                body.width as usize,
                Style::new().fg(palette::OVERLAY_FG).bg(palette::OVERLAY_BG),
            );
        }
    }
}

/// The full list panel, framed and filled top-left of the body: a title, one row
/// per content line (highlighting the selection), and a key hint.
fn render_panel(buf: &mut Buffer, body: Rect, lines: &[(String, bool)], selected: usize) {
    let title = "capture device";
    let hint = "↑↓ select · ⏎ switch · p pin · esc close";
    let content_w = lines
        .iter()
        .map(|(t, _)| t.chars().count())
        .chain([title.chars().count(), hint.chars().count()])
        .max()
        .unwrap_or(0);
    // left/right pad + content.
    let want = (content_w + 2) as u16;
    let width = want.clamp(PANEL_MIN_WIDTH, body.width);
    let height = (lines.len() as u16 + PANEL_CHROME_ROWS).min(body.height);
    let panel = Rect::new(body.x, body.y, width, height);

    let fill = Style::new().bg(palette::OVERLAY_BG).fg(palette::OVERLAY_FG);
    for dy in 0..panel.height {
        for dx in 0..panel.width {
            if let Some(cell) = buf.cell_mut((panel.x + dx, panel.y + dy)) {
                cell.set_char(' ').set_style(fill);
            }
        }
    }

    let inner_x = panel.x + 1;
    let inner_w = panel.width.saturating_sub(2) as usize;
    buf.set_stringn(
        inner_x,
        panel.y,
        title,
        inner_w,
        fill.add_modifier(Modifier::BOLD),
    );

    for (i, (text, _active)) in lines.iter().enumerate() {
        let y = panel.y + 1 + i as u16;
        // Keep the bottom row for the hint.
        if y >= panel.y + panel.height - 1 {
            break;
        }
        let cursor = if i == selected { '▸' } else { ' ' };
        let line = format!("{cursor}{}", truncate(text, inner_w.saturating_sub(1)));
        let mut style = fill;
        if i == selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        buf.set_stringn(inner_x, y, &line, inner_w, style);
    }

    let hint_y = panel.y + panel.height - 1;
    buf.set_stringn(
        inner_x,
        hint_y,
        hint,
        inner_w,
        fill.add_modifier(Modifier::DIM),
    );
}

/// Truncate `s` to `max` characters, appending `…` when it was cut (so the
/// ellipsis fits within `max`). A `max` of zero yields an empty string.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 1 {
        return "…".to_owned();
    }
    let kept: String = s.chars().take(max - 1).collect();
    format!("{kept}…")
}

// ---------------------------------------------------------------------------
// Config pin (comment-preserving)
// ---------------------------------------------------------------------------

/// Apply a device pin to config `src`, preserving every comment and unrelated
/// key: set `[defaults] device` for a named device, or remove the key for the
/// follow-system default. Returns the rendered document.
///
/// # Errors
/// Returns the `toml_edit` parse error when `src` is not valid TOML.
pub fn apply_device_pin(
    src: &str,
    selector: &DeviceSelector,
) -> Result<String, toml_edit::TomlError> {
    let mut doc: DocumentMut = src.parse()?;
    let item = doc
        .entry(CONFIG_TABLE)
        .or_insert_with(|| Item::Table(Table::new()));
    if item.as_table_mut().is_none() {
        *item = Item::Table(Table::new());
    }
    let table = item.as_table_mut().expect("defaults is a table");
    match selector {
        DeviceSelector::Named(name) => {
            // Mutate an existing value in place so its formatting decor (the
            // whitespace and any inline comment around the value) survives; a new
            // key is appended with default formatting.
            if let Some(item) = table.get_mut(CONFIG_KEY) {
                let decor = item.as_value().map(|v| v.decor().clone());
                let mut value = Value::from(name.as_str());
                if let Some(decor) = decor {
                    *value.decor_mut() = decor;
                }
                *item = Item::Value(value);
            } else {
                table.insert(CONFIG_KEY, toml_value(name.as_str()));
            }
        }
        DeviceSelector::Default => {
            table.remove(CONFIG_KEY);
        }
    }
    // Drop a `[defaults]` table left empty by removing the only key it held, so
    // pinning the follow-system default on an otherwise-bare file leaves no
    // stray empty table.
    if table.is_empty() {
        doc.as_table_mut().remove(CONFIG_TABLE);
    }
    Ok(doc.to_string())
}

/// Pin `selector` into `config.toml` under `dir`, comment-preserving and atomic
/// (temp file + rename). A missing file is created; the `[defaults] device` key
/// is set for a named device or removed for the follow-system default. Returns
/// the file written.
///
/// # Errors
/// An I/O error if the directory or file cannot be created / read / written, or
/// an `InvalidData` error if an existing file is not valid TOML.
pub fn pin_device(dir: &Path, selector: &DeviceSelector) -> io::Result<PathBuf> {
    let target = dir.join("config.toml");
    let src = match fs::read_to_string(&target) {
        Ok(existing) => existing,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let edited = apply_device_pin(&src, selector)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::create_dir_all(dir)?;
    atomic_write(&target, &edited)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, kind: DeviceKind, host: &str, def_in: bool, def_out: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            is_default_input: def_in,
            is_default_output: def_out,
            kind,
            host: host.to_owned(),
        }
    }

    // -- capture_filter (every platform branch) ----------------------------

    #[test]
    fn windows_and_pipewire_capture_outputs_alsa_and_macos_inputs() {
        assert_eq!(
            capture_filter(Platform::Windows, true, false).kind,
            DeviceKind::Output
        );
        assert_eq!(
            capture_filter(Platform::MacOs, true, true).kind,
            DeviceKind::Input
        );
        // Linux with PipeWire preferred and present: output endpoints, pinned to
        // the pipewire host.
        let pw = capture_filter(Platform::Linux, true, true);
        assert_eq!(pw.kind, DeviceKind::Output);
        assert_eq!(pw.host.as_deref(), Some("pipewire"));
        // Linux without a PipeWire host (or with it disabled): plain ALSA inputs.
        assert_eq!(
            capture_filter(Platform::Linux, true, false),
            CaptureFilter {
                kind: DeviceKind::Input,
                host: None
            }
        );
        assert_eq!(
            capture_filter(Platform::Linux, false, true),
            CaptureFilter {
                kind: DeviceKind::Input,
                host: None
            }
        );
        assert_eq!(
            capture_filter(Platform::Other, true, true).kind,
            DeviceKind::Input
        );
    }

    #[test]
    fn filter_accepts_matches_kind_and_host_case_insensitively() {
        let filter = CaptureFilter {
            kind: DeviceKind::Output,
            host: Some("pipewire".to_owned()),
        };
        assert!(filter.accepts(&dev("sink", DeviceKind::Output, "PipeWire", false, true)));
        // Wrong kind, or wrong host, is rejected.
        assert!(!filter.accepts(&dev("mic", DeviceKind::Input, "pipewire", false, false)));
        assert!(!filter.accepts(&dev("hw", DeviceKind::Output, "alsa", false, false)));
    }

    // -- build_rows --------------------------------------------------------

    fn fixture() -> Vec<DeviceInfo> {
        vec![
            dev("mic", DeviceKind::Input, "alsa", true, false),
            dev("hdmi", DeviceKind::Output, "alsa", false, false),
            dev("speakers", DeviceKind::Output, "alsa", false, true),
        ]
    }

    #[test]
    fn build_rows_prepends_follow_system_and_filters_by_direction() {
        let filter = CaptureFilter {
            kind: DeviceKind::Output,
            host: None,
        };
        let rows = build_rows(&fixture(), &filter, &DeviceSelector::Default);
        // Follow-system first, then the two outputs (the input is filtered out).
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, FOLLOW_SYSTEM_LABEL);
        assert_eq!(rows[0].selector, DeviceSelector::Default);
        assert_eq!(rows[1].name, "hdmi");
        assert_eq!(rows[2].name, "speakers");
        assert!(rows[2].is_default, "speakers is the default output");
        assert!(!rows[1].is_default);
    }

    #[test]
    fn build_rows_marks_the_active_device() {
        let filter = CaptureFilter {
            kind: DeviceKind::Output,
            host: None,
        };
        // Default active: only the follow-system row is marked.
        let rows = build_rows(&fixture(), &filter, &DeviceSelector::Default);
        assert!(rows[0].active);
        assert!(!rows[1].active && !rows[2].active);
        // A named active device marks its row and not the follow-system row.
        let rows = build_rows(
            &fixture(),
            &filter,
            &DeviceSelector::Named("speakers".to_owned()),
        );
        assert!(!rows[0].active);
        assert!(rows[2].active);
    }

    // -- model states / selection -----------------------------------------

    #[test]
    fn open_enumerating_shows_the_loading_placeholder() {
        let mut p = DevicePicker::new(DeviceSelector::Default, true);
        assert!(!p.is_open());
        p.open_enumerating();
        assert!(p.is_open());
        assert_eq!(p.state(), &EnumState::Loading);
        assert!(p.rows().is_empty(), "no rows while enumerating");
        assert!(p.selected_row().is_none());
    }

    #[test]
    fn set_devices_error_records_an_error_state() {
        let mut p = DevicePicker::new(DeviceSelector::Default, true);
        p.open_enumerating();
        p.set_devices(Err("no host".to_owned()));
        assert_eq!(p.state(), &EnumState::Error("no host".to_owned()));
        assert!(p.rows().is_empty());
    }

    #[test]
    fn set_devices_selects_the_active_row() {
        // On the Linux/pipewire path the fixture below yields output rows; here we
        // drive the model on whatever the host platform's filter picks, using a
        // fixture that has a capture target for both directions and hosts.
        let devices = vec![
            dev("mon", DeviceKind::Output, "pipewire", false, true),
            dev("mic", DeviceKind::Input, "alsa", true, false),
        ];
        // Seed the active device to whichever target the current platform filters
        // in, so the assertion holds on every host.
        let filter = platform_filter(&devices, true);
        let want = build_rows(&devices, &filter, &DeviceSelector::Default);
        // Pick the first non-follow-system row's selector as the active device.
        if let Some(target) = want.get(1).map(|r| r.selector.clone()) {
            let mut p = DevicePicker::new(target.clone(), true);
            p.open_enumerating();
            p.set_devices(Ok(devices));
            let row = p.selected_row().expect("a selected row");
            assert_eq!(row.selector, target, "selection snaps to the active device");
            assert!(row.active);
        }
    }

    #[test]
    fn selection_cycles_and_wraps() {
        // The fixture's rows depend on the host's capture direction, so assert
        // cycling generically over however many rows the platform produced.
        let mut p = DevicePicker::new(DeviceSelector::Default, true);
        p.open_enumerating();
        p.set_devices(Ok(fixture()));
        let n = p.rows().len();
        assert!(
            n >= 2,
            "the fixture yields the follow-system row plus a target"
        );
        let start = p.selected();
        p.select_next();
        assert_eq!(p.selected(), (start + 1) % n);
        p.select_prev();
        assert_eq!(p.selected(), start);
        p.select_prev();
        assert_eq!(
            p.selected(),
            (start + n - 1) % n,
            "wraps backward past the top"
        );
    }

    // -- config pin --------------------------------------------------------

    const CONFIG_FIXTURE: &str = "\
# scia config
[defaults]
# the scene to open with
scene = \"aurora\"
overlay = true

[keys]
quit = \"ctrl+x\"
";

    #[test]
    fn pin_named_sets_only_the_device_key_byte_for_byte() {
        let out = apply_device_pin(
            CONFIG_FIXTURE,
            &DeviceSelector::Named("speakers".to_owned()),
        )
        .expect("valid toml");
        let expected = "\
# scia config
[defaults]
# the scene to open with
scene = \"aurora\"
overlay = true
device = \"speakers\"

[keys]
quit = \"ctrl+x\"
";
        assert_eq!(out, expected);
    }

    #[test]
    fn pin_named_updates_an_existing_device_key_in_place() {
        let src = "\
[defaults]
device = \"old\"  # pinned earlier
scene = \"aurora\"
";
        let out = apply_device_pin(src, &DeviceSelector::Named("new".to_owned())).expect("toml");
        assert!(out.contains("device = \"new\""));
        assert!(
            out.contains("# pinned earlier"),
            "the inline comment survives"
        );
        assert!(out.contains("scene = \"aurora\""));
    }

    #[test]
    fn pin_default_removes_the_device_key() {
        let src = "\
# header
[defaults]
scene = \"aurora\"
device = \"speakers\"
";
        let out = apply_device_pin(src, &DeviceSelector::Default).expect("toml");
        assert!(!out.contains("device"), "the key is removed: {out}");
        assert!(out.contains("scene = \"aurora\""), "other keys stay");
        assert!(out.contains("# header"));
    }

    #[test]
    fn pin_default_on_bare_file_leaves_no_empty_table() {
        let out = apply_device_pin("", &DeviceSelector::Default).expect("toml");
        assert!(
            !out.contains("[defaults]"),
            "no stray empty table is created: {out:?}"
        );
    }

    #[test]
    fn pin_named_on_bare_file_creates_the_table_and_key() {
        let out = apply_device_pin("", &DeviceSelector::Named("hw".to_owned())).expect("toml");
        assert!(out.contains("[defaults]"));
        assert!(out.contains("device = \"hw\""));
        let _: DocumentMut = out.parse().expect("re-parses");
    }

    #[test]
    fn pin_device_writes_atomically_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("scia-devpin-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("config.toml"), CONFIG_FIXTURE).expect("seed");

        let path =
            pin_device(&dir, &DeviceSelector::Named("speakers".to_owned())).expect("pin write");
        assert_eq!(path, dir.join("config.toml"));
        let read = fs::read_to_string(&path).expect("read back");
        assert!(read.contains("device = \"speakers\""));
        assert!(read.contains("# scia config"), "comments intact");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp remains: {leftovers:?}");

        fs::remove_dir_all(&dir).ok();
    }

    // -- rendering ---------------------------------------------------------

    #[test]
    fn truncate_appends_ellipsis_when_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("abc", 0), "");
    }
}
