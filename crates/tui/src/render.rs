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
    /// Whether the full debug/performance overlay panel is currently shown. The
    /// one-line [`debug`](UiState::debug) field is independent; both can be on.
    pub overlay: bool,
    /// Age of the newest feature — capture→now in milliseconds, computed by the
    /// render loop each frame as `clock() - snap.timestamp_ns`. The rendered
    /// frame adds up to one more frame interval on top. `0.0` on a default
    /// state or under clock skew.
    pub feature_age_ms: f32,
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
    /// A transient status notice — a live-reload confirmation (`reloaded 38ms`)
    /// or a preset error's first line — shown dim and right-aligned on the
    /// bottom row, even when the debug line is off. `None` when there is nothing
    /// to report.
    pub notice: Option<String>,
}

/// Compute the frame layout: the header row, the optional body area, and the
/// optional debug row. The debug row takes the bottom line only when it is
/// enabled and a body row still survives. Returns `None` for a degenerate
/// zero-area frame.
fn layout(area: Rect, debug: bool) -> Option<(Rect, Option<Rect>, Option<Rect>)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let debug_rows: u16 = u16::from(debug && area.height >= 3);
    let body_height = area.height.saturating_sub(1 + debug_rows);
    let body = (body_height > 0).then(|| Rect::new(area.x, area.y + 1, area.width, body_height));
    let debug_rect =
        (debug_rows == 1).then(|| Rect::new(area.x, area.y + area.height - 1, area.width, 1));
    Some((header, body, debug_rect))
}

/// Paint one frame: header row, spectrum body, and (when enabled) the debug
/// line. Safe for any terminal size, including degenerate zero-area frames.
pub fn draw(frame: &mut Frame, snap: &FeatureSnapshot, ui: &UiState) {
    let area = frame.area();
    let Some((header, body, debug)) = layout(area, ui.debug) else {
        return;
    };
    let buf = frame.buffer_mut();
    render_header(buf, header, snap, ui);
    if let Some(body) = body {
        render_body(buf, body, snap);
        if ui.overlay {
            render_overlay(buf, body, snap, ui);
        }
    }
    if let Some(debug) = debug {
        render_debug(buf, debug, snap, ui);
    }
    draw_notice(buf, area, ui);
}

/// Paint the transient reload notice, if any: dim, right-aligned, on the bottom
/// row of `area`, truncated to the terminal width. It overlays whatever is on
/// that row (the debug line when shown, otherwise the last body row), so the
/// caller draws it last.
pub fn draw_notice(buf: &mut Buffer, area: Rect, ui: &UiState) {
    let Some(notice) = &ui.notice else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let text: String = notice.chars().take(width).collect();
    let w = text.chars().count() as u16;
    let x = area.x + area.width - w;
    let y = area.y + area.height - 1;
    buf.set_stringn(
        x,
        y,
        &text,
        width,
        Style::new().fg(palette::DEBUG).add_modifier(Modifier::DIM),
    );
}

/// Paint only the header and debug-line chrome and return the body area, so a
/// scene presenter can rasterize into the body itself. Returns `None` for a
/// degenerate frame, or `Some(None)` when the chrome leaves no body row.
pub fn draw_chrome(frame: &mut Frame, snap: &FeatureSnapshot, ui: &UiState) -> Option<Rect> {
    let area = frame.area();
    let (header, body, debug) = layout(area, ui.debug)?;
    let buf = frame.buffer_mut();
    render_header(buf, header, snap, ui);
    if let Some(debug) = debug {
        render_debug(buf, debug, snap, ui);
    }
    body
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

/// Number of rows the overlay panel occupies at the bottom of the body.
const OVERLAY_ROWS: u16 = 5;
/// Minimum body height that hosts the full panel; below it the overlay falls
/// back to the single debug line. Twice the panel height so the panel never
/// blankets the whole spectrum.
const OVERLAY_MIN_BODY: u16 = 2 * OVERLAY_ROWS;
/// An onset lamp stays lit this many milliseconds after the last onset.
const ONSET_LAMP_MS: f32 = 150.0;
/// The lit onset lamp.
const LAMP_ON: char = '●';
/// The dim onset lamp.
const LAMP_OFF: char = '○';
/// The one-row spectrum-strip ramp, lowest to highest (eight levels).
const STRIP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The feature age in milliseconds: `now_ns - timestamp_ns`, saturating to `0.0`
/// under clock skew (a timestamp ahead of the clock). This is capture→now for
/// the newest feature; the rendered frame adds up to one more frame interval.
#[must_use]
pub(crate) fn feature_age_ms(now_ns: u64, timestamp_ns: u64) -> f32 {
    now_ns.saturating_sub(timestamp_ns) as f32 / 1.0e6
}

/// Map a `0.0..=1.0` value to one of the eight [`STRIP`] block characters, the
/// same eighth-block ramp the body renderer draws with.
fn strip_char(v: f32) -> char {
    let level = (v.clamp(0.0, 1.0) * 8.0).ceil() as usize;
    STRIP[level.clamp(1, 8) - 1]
}

/// The debug/performance overlay: a boxed panel painted over the bottom
/// [`OVERLAY_ROWS`] rows of the body, showing every extracted signal live. When
/// the body is shorter than [`OVERLAY_MIN_BODY`] there is no room for the panel,
/// so it falls back to the single debug line on the body's bottom row.
pub(crate) fn render_overlay(buf: &mut Buffer, body: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    if body.height < OVERLAY_MIN_BODY {
        let line = Rect::new(body.x, body.y + body.height - 1, body.width, 1);
        render_debug(buf, line, snap, ui);
        return;
    }
    let panel = Rect::new(
        body.x,
        body.y + body.height - OVERLAY_ROWS,
        body.width,
        OVERLAY_ROWS,
    );
    render_overlay_panel(buf, panel, snap, ui);
}

/// Paint the five-row overlay panel: clear its area, frame it with side rails,
/// then draw the five content lines. Values are right-truncated to the panel's
/// interior width; the only per-draw allocations are the `format!`s, matching
/// the debug line.
fn render_overlay_panel(buf: &mut Buffer, panel: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    let fill = Style::new().bg(palette::OVERLAY_BG).fg(palette::OVERLAY_FG);
    let rail = Style::new().bg(palette::OVERLAY_BG).fg(palette::DEBUG);

    // Clear the panel to its background so the spectrum beneath does not bleed
    // through, then draw the left/right rails that frame it.
    for dy in 0..panel.height {
        let y = panel.y + dy;
        for dx in 0..panel.width {
            if let Some(cell) = buf.cell_mut((panel.x + dx, y)) {
                cell.set_char(' ').set_style(fill);
            }
        }
        if let Some(cell) = buf.cell_mut((panel.x, y)) {
            cell.set_char('│').set_style(rail);
        }
        if let Some(cell) = buf.cell_mut((panel.x + panel.width - 1, y)) {
            cell.set_char('│').set_style(rail);
        }
    }

    let inner_x = panel.x + 1;
    let inner_w = panel.width.saturating_sub(2) as usize;
    let s = &ui.stats;

    let l1 = format!(
        "fps {:.1} · frame p50/p99 {:.2}/{:.2}ms · dropped {} · xruns {} · reopens {}",
        ui.fps_measured, ui.p50_frame_ms, ui.p99_frame_ms, s.dropped_frames, s.xruns, s.reopens,
    );
    buf.set_stringn(inner_x, panel.y, &l1, inner_w, fill);

    let l2 = format!(
        "capture: {} pushes · push {}f (max {}f) · gap max {:.1}ms · synth {} · age {:.1} ms",
        s.pushes,
        s.last_push_frames,
        s.max_push_frames,
        s.max_gap_ms,
        s.hops_synthesized,
        ui.feature_age_ms,
    );
    buf.set_stringn(inner_x, panel.y + 1, &l2, inner_w, fill);

    let lamp = if snap.onset_age_ms <= ONSET_LAMP_MS {
        LAMP_ON
    } else {
        LAMP_OFF
    };
    let l3 = format!(
        "rms {:.2} peak {:.2} · bass/mid/treb {:.2}/{:.2}/{:.2} · width {:.2} · flux {:.2} · \
         onset {} · beat {:.0} bpm {:.2}",
        snap.rms,
        snap.peak,
        snap.bands[0],
        snap.bands[1],
        snap.bands[2],
        snap.mid_side_ratio,
        snap.flux,
        lamp,
        snap.tempo_bpm,
        snap.beat_confidence,
    );
    buf.set_stringn(inner_x, panel.y + 2, &l3, inner_w, fill);

    render_overlay_strip(buf, inner_x, panel.y + 3, inner_w, snap, fill);

    let tier = ui.tier.unwrap_or("bars");
    let src = ui.label.as_deref().filter(|l| !l.is_empty()).unwrap_or({
        if ui.source.is_empty() {
            "—"
        } else {
            ui.source.as_str()
        }
    });
    let l5 = format!(
        "tier {} · {} · schema v{} · activity {}",
        tier,
        src,
        snap.schema_version,
        activity_label(s.activity),
    );
    buf.set_stringn(inner_x, panel.y + 4, &l5, inner_w, fill);
}

/// One-row spectrum strip: the first `min(max_w, spectrum_len)` bars mapped to
/// the eighth-block ramp, each coloured on the body gradient.
fn render_overlay_strip(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    max_w: usize,
    snap: &FeatureSnapshot,
    fill: Style,
) {
    let bars = &snap.spectrum[..snap.spectrum_len as usize];
    let n = bars.len().min(max_w);
    for (i, &raw) in bars.iter().take(n).enumerate() {
        let v = raw.clamp(0.0, 1.0);
        if let Some(cell) = buf.cell_mut((x0 + i as u16, y)) {
            cell.set_char(strip_char(v))
                .set_style(fill.fg(palette::bar_color(v)));
        }
    }
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
    fn feature_age_is_capture_to_now() {
        // 3 ms elapsed since the feature was captured.
        assert_eq!(feature_age_ms(3_000_000, 0), 3.0);
        // 1.5 ms.
        assert_eq!(feature_age_ms(2_500_000, 1_000_000), 1.5);
        // Zero age when the timestamp equals the clock.
        assert_eq!(feature_age_ms(42, 42), 0.0);
    }

    #[test]
    fn feature_age_clamps_clock_skew() {
        // A timestamp ahead of the clock (skew) saturates to zero, never negative.
        assert_eq!(feature_age_ms(1_000_000, 5_000_000), 0.0);
        assert_eq!(feature_age_ms(0, u64::MAX), 0.0);
    }

    #[test]
    fn strip_char_spans_the_ramp() {
        assert_eq!(strip_char(0.0), '▁');
        assert_eq!(strip_char(1.0), '█');
        assert_eq!(strip_char(0.5), '▄');
        // Out-of-range values clamp rather than panic.
        assert_eq!(strip_char(-1.0), '▁');
        assert_eq!(strip_char(2.0), '█');
    }

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
