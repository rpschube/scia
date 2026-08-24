//! Pure rendering: [`draw`] paints one frame from a [`FeatureSnapshot`] and a
//! [`UiState`] into a ratatui [`Frame`]. It is deliberately side-effect free
//! (no terminal, no timing) so it can be exercised with ratatui's
//! `TestBackend`. The render loop in [`crate::run`] owns everything stateful.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use scia_core::{Activity, EngineStats, FeatureSnapshot};

use crate::palette;

/// Version string shown in the header, resolved from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The eighth-block ladder for a fractional top cell: index `n` is `(n+1)/8`
/// of a cell tall. A full cell is drawn with [`FULL_BLOCK`] instead.
const EIGHTHS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
/// A completely filled cell.
const FULL_BLOCK: char = '█';

/// Everything the renderer needs beyond the current [`FeatureSnapshot`]: the
/// demo label, whether the debug line is on, and the measured timing the debug
/// line reports.
#[derive(Clone, Debug, Default)]
pub struct UiState {
    /// Header label, highlighted so demo mode can never be read as live
    /// capture. `None` for live capture.
    pub label: Option<String>,
    /// Live-capture source description shown in the centre when [`label`] is
    /// `None`, e.g. `"48000 Hz 2 ch"`. Ignored while a demo label is set.
    ///
    /// [`label`]: UiState::label
    pub source: String,
    /// Whether the debug line is currently shown.
    pub debug: bool,
    /// Measured frame rate for the debug line.
    pub fps_measured: f32,
    /// Median frame render time (ms) for the debug line.
    pub p50_frame_ms: f32,
    /// 99th-percentile frame render time (ms) for the debug line.
    pub p99_frame_ms: f32,
    /// Latest engine counters for the debug line.
    pub stats: EngineStats,
    /// The active mosaic tier label (e.g. `"octants"`), shown in the debug line
    /// when a scene presenter is driving the body. `None` for the direct-bars
    /// renderer, which leaves the debug line unchanged.
    pub tier: Option<&'static str>,
}

/// Paint one frame: header row, spectrum body, and (when enabled) the debug
/// line. Safe for any terminal size, including degenerate zero-area frames.
pub fn draw(frame: &mut Frame, snap: &FeatureSnapshot, ui: &UiState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = frame.buffer_mut();

    // Header is always the top row.
    let header = Rect::new(area.x, area.y, area.width, 1);
    render_header(buf, header, snap, ui);

    // The debug line takes the bottom row, but only if a body row survives.
    let debug_rows: u16 = u16::from(ui.debug && area.height >= 3);
    let body_height = area.height.saturating_sub(1 + debug_rows);
    if body_height > 0 {
        let body = Rect::new(area.x, area.y + 1, area.width, body_height);
        render_body(buf, body, snap);
    }
    if debug_rows == 1 {
        let debug = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        render_debug(buf, debug, snap, ui);
    }
}

/// Header: `scia <version>` at the left, a centred label — the highlighted demo
/// label, or `live · <format>` for live capture — and the activity state with
/// the generation at the right.
fn render_header(buf: &mut Buffer, rect: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    let y = rect.y;
    let max = rect.width as usize;

    // Draw the centred text first so the edge texts win any collision on a
    // narrow terminal; the state indicator must always remain legible.
    if let Some(label) = &ui.label {
        let lw = label.chars().count() as u16;
        let lx = rect.x + rect.width.saturating_sub(lw) / 2;
        let hl = Style::new()
            .fg(palette::LABEL_FG)
            .bg(palette::LABEL_BG)
            .add_modifier(Modifier::BOLD);
        buf.set_stringn(lx, y, label, max, hl);
    } else {
        // Live capture: name the source in the centre so a live session reads as
        // live and shows its negotiated format.
        let centre = if ui.source.is_empty() {
            "live".to_string()
        } else {
            format!("live · {}", ui.source)
        };
        let cw = centre.chars().count() as u16;
        let cx = rect.x + rect.width.saturating_sub(cw) / 2;
        buf.set_stringn(
            cx,
            y,
            &centre,
            max,
            Style::new().fg(palette::LIVE).add_modifier(Modifier::BOLD),
        );
    }

    let left = format!("scia {VERSION}");
    buf.set_stringn(
        rect.x,
        y,
        &left,
        max,
        Style::new().add_modifier(Modifier::BOLD),
    );

    // Right indicator reflects the engine's activity state, not the raw
    // starved bit, so `quiet` and `idle` are distinguishable at a glance.
    let activity = ui.stats.activity;
    let right = format!("{}  gen {}", activity_label(activity), snap.generation);
    let rw = right.chars().count() as u16;
    let rx = rect.x + rect.width.saturating_sub(rw);
    buf.set_stringn(
        rx,
        y,
        &right,
        max,
        Style::new().fg(activity_color(activity)),
    );
}

/// The short indicator word for an [`Activity`].
fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Active => "active",
        Activity::Quiet => "quiet",
        Activity::Idle => "idle",
    }
}

/// The header colour for an [`Activity`]: green while active, amber once quiet,
/// orange once idle.
fn activity_color(activity: Activity) -> ratatui::style::Color {
    match activity {
        Activity::Active => palette::LIVE,
        Activity::Quiet => palette::QUIET,
        Activity::Idle => palette::STARVED,
    }
}

/// Body: one vertical bar per output column, spread across the full width.
fn render_body(buf: &mut Buffer, rect: Rect, snap: &FeatureSnapshot) {
    let bars = &snap.spectrum[..snap.spectrum_len as usize];
    if bars.is_empty() {
        return;
    }
    let h = rect.height;
    for cx in 0..rect.width {
        let value = column_value(bars, rect.width, cx).clamp(0.0, 1.0);
        let (full, rem) = fill_cells(value, h);
        for row in 0..h {
            // `row` counts from the bottom of the bar upward.
            let x = rect.x + cx;
            let cell_y = rect.y + h - 1 - row;
            let glyph = if row < full {
                Some(FULL_BLOCK)
            } else if row == full && rem > 0 {
                Some(EIGHTHS[rem - 1])
            } else {
                None
            };
            let Some(cell) = buf.cell_mut((x, cell_y)) else {
                continue;
            };
            match glyph {
                Some(ch) => {
                    let frac = (row as f32 + 1.0) / h as f32;
                    cell.set_char(ch)
                        .set_style(Style::new().fg(palette::bar_color(frac)));
                }
                None => {
                    cell.set_char(' ');
                }
            }
        }
    }
}

/// Debug line: measured fps, frame percentiles, and the engine counters.
fn render_debug(buf: &mut Buffer, rect: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    let s = &ui.stats;
    let mut line = format!(
        "fps {:.1}  frame p50 {:.2}ms p99 {:.2}ms  gen {}  hops {}/{}  dropped {}  agc {:.2}  \
         act {}  push {}  gap {:.1}ms",
        ui.fps_measured,
        ui.p50_frame_ms,
        ui.p99_frame_ms,
        snap.generation,
        s.hops_processed,
        s.hops_synthesized,
        s.dropped_frames,
        s.agc_gain,
        activity_label(s.activity),
        s.pushes,
        s.max_gap_ms,
    );
    // A scene presenter surfaces its active tier so the ladder rung is visible
    // at a glance; the direct-bars renderer leaves `tier` unset.
    if let Some(tier) = ui.tier {
        line.push_str(" · tier ");
        line.push_str(tier);
    }
    buf.set_stringn(
        rect.x,
        rect.y,
        &line,
        rect.width as usize,
        Style::new().fg(palette::DEBUG),
    );
}

/// Map a `0.0..=1.0` bar `value` onto a body of `height` cells, returning the
/// number of completely filled cells and the eighth-index (`0..=7`) of the
/// fractional top cell. `0` means no partial cell.
///
/// The height is quantised to eighth-cells and rounded to nearest, so a value
/// of `1.0` fills every cell exactly and a value that lands on `k/8` of a cell
/// renders that partial glyph.
fn fill_cells(value: f32, height: u16) -> (u16, usize) {
    let total_eighths = (value.clamp(0.0, 1.0) * height as f32 * 8.0).round() as u32;
    let full = (total_eighths / 8) as u16;
    let rem = (total_eighths % 8) as usize;
    (full, rem)
}

/// The value to draw in output column `x` of `width`, given `bars` source bars.
///
/// When the width exceeds the bar count each bar widens across several columns;
/// when it is smaller, adjacent bars are averaged. Either way every column in
/// `0..width` is assigned a value, so the whole width is used.
fn column_value(bars: &[f32], width: u16, x: u16) -> f32 {
    let n = bars.len();
    if n == 0 || width == 0 {
        return 0.0;
    }
    let width = width as usize;
    let x = x as usize;
    let lo = x * n / width;
    let hi = (((x + 1) * n / width).max(lo + 1)).min(n);
    let slice = &bars[lo..hi];
    slice.iter().sum::<f32>() / slice.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_cells_rounds_to_eighths() {
        // Empty, full, half, and a single eighth over a 9-cell body.
        assert_eq!(fill_cells(0.0, 9), (0, 0));
        assert_eq!(fill_cells(1.0, 9), (9, 0));
        assert_eq!(fill_cells(0.5, 9), (4, 4)); // 36 eighths -> 4 full + 4/8
        assert_eq!(fill_cells(0.125, 9), (1, 1)); // 9 eighths -> 1 full + 1/8
    }

    #[test]
    fn column_value_direct_when_widths_match() {
        let bars = [0.0, 0.5, 1.0, 0.125];
        for (i, &v) in bars.iter().enumerate() {
            assert_eq!(column_value(&bars, 4, i as u16), v);
        }
    }

    #[test]
    fn column_value_averages_when_narrower() {
        // 4 bars into 2 columns: each column averages a pair.
        let bars = [0.0, 1.0, 0.0, 0.5];
        assert_eq!(column_value(&bars, 2, 0), 0.5);
        assert_eq!(column_value(&bars, 2, 1), 0.25);
    }

    #[test]
    fn column_value_widens_when_wider() {
        // 2 bars into 6 columns: every column resolves to one of the two bars.
        let bars = [0.2, 0.8];
        for x in 0..6u16 {
            let v = column_value(&bars, 6, x);
            assert!(v == 0.2 || v == 0.8, "column {x} = {v}");
        }
    }
}
