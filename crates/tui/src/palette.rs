//! The one place colour lives. A later card replaces this with a real palette
//! system; every colour the renderer uses is resolved here so that swap is
//! local.

use ratatui::style::Color;

/// Foreground of the highlighted demo label.
pub const LABEL_FG: Color = Color::Rgb(20, 20, 24);
/// Background of the highlighted demo label (amber, impossible to miss).
pub const LABEL_BG: Color = Color::Rgb(240, 180, 40);
/// The `starved`/`idle` state indicator.
pub const STARVED: Color = Color::Rgb(230, 120, 40);
/// The `live`/`active` state indicator.
pub const LIVE: Color = Color::Rgb(120, 200, 120);
/// The `quiet` activity indicator: signal present but below the quiet
/// threshold.
pub const QUIET: Color = Color::Rgb(210, 200, 120);
/// The debug line.
pub const DEBUG: Color = Color::Rgb(150, 150, 160);
/// Background fill of the debug/performance overlay panel.
pub const OVERLAY_BG: Color = Color::Rgb(16, 16, 22);
/// Foreground of the overlay panel's text.
pub const OVERLAY_FG: Color = Color::Rgb(200, 200, 210);
/// The chrome now-playing line at full brightness.
pub const CHROME_FG: Color = Color::Rgb(150, 150, 160);
/// The chrome now-playing line one dim step down, just before it vanishes.
pub const CHROME_DIM: Color = Color::Rgb(90, 90, 100);

/// The three-stop bar gradient, low to high.
const STOP_LOW: (u8, u8, u8) = (20, 180, 170); // teal
const STOP_MID: (u8, u8, u8) = (240, 180, 40); // amber
const STOP_HIGH: (u8, u8, u8) = (230, 70, 40); // red-orange

/// Colour for a bar cell at height fraction `frac` (`0.0` bottom, `1.0` top),
/// interpolated across the fixed three-stop gradient.
pub fn bar_color(frac: f32) -> Color {
    let frac = frac.clamp(0.0, 1.0);
    let (r, g, b) = if frac <= 0.5 {
        lerp(STOP_LOW, STOP_MID, frac / 0.5)
    } else {
        lerp(STOP_MID, STOP_HIGH, (frac - 0.5) / 0.5)
    };
    Color::Rgb(r, g, b)
}

/// Linear interpolation between two RGB stops at `t` in `0.0..=1.0`.
fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}
