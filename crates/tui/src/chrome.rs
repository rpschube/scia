//! The four chrome personalities: a runtime-switchable, configurable layer of
//! now-playing / status chrome painted onto the scene canvas after the scene and
//! before the debug and help overlays.
//!
//! A [`ChromeMode`] selects one of four personalities. [`ChromeState`] carries
//! the per-mode animation state — the invisible-mode fade timer, the playful
//! wave phase, and the mode-switch toast — all advanced by a frame `dt`, never a
//! wall clock, so a frame is a pure function of the accumulated state.
//!
//! - **Invisible** (default): a single dim now-playing line that fades out after
//!   ~4 s without user input or a track change and returns on any keypress or
//!   track change. Nothing else is drawn; the scene owns the screen.
//! - **Instrument**: a persistent one-row bottom rail — VU meters, a tempo lamp
//!   that breathes with the beat, compact band meters, the scene id and the fps.
//! - **Playful**: the now-playing text rides the beat, each glyph lifted at most
//!   one cell by the onset envelope and loudness. Deterministic (phase + snapshot
//!   driven; no RNG).
//! - **Utilitarian**: one always-visible dense status row that never fades.
//!
//! Rendering models the existing overlay drawing in [`crate::render`]: it writes
//! straight into the ratatui [`Buffer`] and clips to the body rect, so every mode
//! honours the same small-pane collapse discipline the meter bridge and help
//! overlay use.

use std::f32::consts::TAU;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use scia_core::FeatureSnapshot;

use crate::palette;
use crate::render::{Toast, UiState};

/// Idle seconds the invisible-mode line stays at full brightness before it steps
/// down.
const FADE_HOLD_SECS: f32 = 3.0;
/// Idle seconds after which the invisible-mode line is gone entirely. Between
/// [`FADE_HOLD_SECS`] and this it is drawn one dim step down — a dimming
/// step-down then absent over ~4 s.
const FADE_END_SECS: f32 = 4.0;

/// Radians per second the playful wave phase advances. A gentle sway, not a
/// strobe.
const PLAYFUL_PHASE_RATE: f32 = 6.0;
/// Horizontal wavelength of the playful lift, in radians per glyph.
const PLAYFUL_GLYPH_SPACING: f32 = 0.6;
/// The wave-times-drive value above which a glyph lifts one cell. Keeps the lift
/// to a musical crest rather than a constant shimmer.
const PLAYFUL_LIFT_THRESHOLD: f32 = 0.5;
/// The onset envelope decays to zero over this many milliseconds; it drives the
/// playful lift together with loudness.
const PLAYFUL_ENV_MS: f32 = 300.0;

/// Beat-confidence at or above which the instrument tempo lamp breathes with the
/// tracked beat rather than flashing on raw onsets. This consumer-side gate is
/// the contract with the beat tracker, which always publishes `beat_confidence`
/// honestly; the lamp trusts the phase only once the lock is solid.
const BEAT_CONFIDENCE_GATE: f32 = 0.5;

/// Milliseconds an onset flash lingers on the tempo lamp while the beat is not
/// confidently locked (matches the overlay's onset lamp window).
const ONSET_FLASH_MS: f32 = 150.0;

/// The narrowest body width that still hosts the full instrument rail; below it
/// the rail collapses to a compact VU-plus-lamp form, the way the meter bridge
/// falls back on a small pane.
const INSTRUMENT_MIN_WIDTH: u16 = 32;

/// Cells in one VU meter bar.
const VU_CELLS: usize = 5;

/// The four chrome personalities.
///
/// [`Invisible`](ChromeMode::Invisible) is the default. The order is the cycle
/// order the runtime `chrome` action steps through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChromeMode {
    /// A single dim now-playing line that fades after ~4 s of no input.
    #[default]
    Invisible,
    /// A persistent one-row instrument rail.
    Instrument,
    /// The now-playing text rides the beat.
    Playful,
    /// A dense, always-visible status row.
    Utilitarian,
}

impl ChromeMode {
    /// Every mode, in cycle order.
    pub const ALL: [ChromeMode; 4] = [
        ChromeMode::Invisible,
        ChromeMode::Instrument,
        ChromeMode::Playful,
        ChromeMode::Utilitarian,
    ];

    /// The config / flag name for this mode.
    #[must_use]
    pub fn config_str(self) -> &'static str {
        match self {
            ChromeMode::Invisible => "invisible",
            ChromeMode::Instrument => "instrument",
            ChromeMode::Playful => "playful",
            ChromeMode::Utilitarian => "utilitarian",
        }
    }

    /// A short human label for the mode-switch toast.
    #[must_use]
    pub fn label(self) -> &'static str {
        self.config_str()
    }

    /// The next mode in the cycle, wrapping.
    #[must_use]
    pub fn next(self) -> ChromeMode {
        match self {
            ChromeMode::Invisible => ChromeMode::Instrument,
            ChromeMode::Instrument => ChromeMode::Playful,
            ChromeMode::Playful => ChromeMode::Utilitarian,
            ChromeMode::Utilitarian => ChromeMode::Invisible,
        }
    }

    /// Parse a config / flag value (case-insensitive). Unknown names yield `None`
    /// so the caller can warn and fall back to the default.
    #[must_use]
    pub fn parse(name: &str) -> Option<ChromeMode> {
        let name = name.trim().to_ascii_lowercase();
        ChromeMode::ALL.into_iter().find(|m| m.config_str() == name)
    }
}

/// The three brightness steps of the invisible-mode now-playing line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fade {
    /// Full brightness (recent input).
    Full,
    /// One dim step down, just before it vanishes.
    Dim,
    /// Gone.
    Hidden,
}

impl Fade {
    /// The fade step for an idle time in seconds.
    #[must_use]
    pub fn from_idle(idle: f32) -> Fade {
        if idle < FADE_HOLD_SECS {
            Fade::Full
        } else if idle < FADE_END_SECS {
            Fade::Dim
        } else {
            Fade::Hidden
        }
    }
}

/// Per-mode chrome state, advanced by the frame `dt`.
///
/// It holds the active [`ChromeMode`], the invisible-mode idle timer, the playful
/// wave phase, and the transient mode-switch toast. It is pure state: the input
/// handler resets the idle timer on a keypress, the loop ticks it once per frame,
/// and [`render`] reads it.
#[derive(Clone, Debug)]
pub struct ChromeState {
    /// The active personality.
    pub mode: ChromeMode,
    /// Seconds since the last keypress or track change (invisible-mode fade).
    idle: f32,
    /// The playful wave phase, in radians, wrapping at `TAU`.
    phase: f32,
    /// The mode-switch toast, if showing.
    toast: Option<Toast>,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self::new(ChromeMode::Invisible)
    }
}

impl ChromeState {
    /// A fresh state in `mode`, full brightness, no toast.
    #[must_use]
    pub fn new(mode: ChromeMode) -> Self {
        Self {
            mode,
            idle: 0.0,
            phase: 0.0,
            toast: None,
        }
    }

    /// The active mode.
    #[must_use]
    pub fn mode(&self) -> ChromeMode {
        self.mode
    }

    /// Advance to the next personality, reset the fade, and raise a naming toast.
    pub fn cycle(&mut self) {
        self.mode = self.mode.next();
        self.idle = 0.0;
        self.toast = Some(Toast::new(format!("chrome · {}", self.mode.label())));
    }

    /// Reset the invisible-mode fade because the user pressed a key.
    pub fn on_input(&mut self) {
        self.idle = 0.0;
    }

    /// Reset the invisible-mode fade because the now-playing track changed.
    pub fn on_track_change(&mut self) {
        self.idle = 0.0;
    }

    /// Advance the fade timer, the playful phase, and the toast by `dt` seconds.
    /// Negative `dt` is clamped to zero, so the state only ever moves forward.
    pub fn tick(&mut self, dt: f32) {
        let dt = dt.max(0.0);
        self.idle += dt;
        self.phase = (self.phase + dt * PLAYFUL_PHASE_RATE).rem_euclid(TAU);
        if let Some(toast) = self.toast.as_mut() {
            if !toast.tick(dt) {
                self.toast = None;
            }
        }
    }

    /// The invisible-mode fade step for the current idle time.
    #[must_use]
    pub fn fade(&self) -> Fade {
        Fade::from_idle(self.idle)
    }

    /// The mode-switch toast text, if one is showing.
    #[must_use]
    pub fn toast_text(&self) -> Option<&str> {
        self.toast.as_ref().map(Toast::text)
    }
}

/// Paint the active chrome personality into `body`, then the mode-switch toast on
/// top. Draw this after the scene and before the debug / help overlays, which are
/// separate surfaces layered above the chrome.
///
/// Draws nothing on a degenerate body.
pub(crate) fn render(buf: &mut Buffer, body: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    match ui.chrome.mode {
        ChromeMode::Invisible => render_invisible(buf, body, ui),
        ChromeMode::Instrument => render_instrument(buf, body, snap, ui),
        ChromeMode::Playful => render_playful(buf, body, snap, ui),
        ChromeMode::Utilitarian => render_utilitarian(buf, body, snap, ui),
    }
    if let Some(text) = ui.chrome.toast_text() {
        render_toast(buf, body, text);
    }
}

/// The now-playing text: the wired track line when present, otherwise the source
/// label the header shows today (a demo label, or `live · <format>`). `None` when
/// there is genuinely nothing to name — no track, no label, no source — so the
/// chrome shows nothing rather than a bare placeholder.
///
/// This is the single track-line seam: [`UiState::track_line`] returns `None`
/// until the metadata branch wires the real now-playing value in, at which point
/// it flows through here for every mode with no further change.
fn now_playing(ui: &UiState) -> Option<String> {
    if let Some(track) = ui.track_line() {
        if !track.is_empty() {
            return Some(track.to_string());
        }
    }
    if let Some(label) = ui.label.as_deref() {
        if !label.is_empty() {
            return Some(label.to_string());
        }
    }
    if !ui.source.is_empty() {
        return Some(format!("live · {}", ui.source));
    }
    None
}

/// The scene id for the status rails: the committed scene when a scene presenter
/// is driving the body, otherwise a dash (the direct-bars path has no scene).
fn scene_id(ui: &UiState) -> &'static str {
    if ui.scene_mode {
        ui.scene_nav.current_id().unwrap_or("—")
    } else {
        "—"
    }
}

/// Invisible mode: one dim now-playing line on the body's bottom row, stepping
/// down and out on the fade timer.
fn render_invisible(buf: &mut Buffer, body: Rect, ui: &UiState) {
    let fg = match ui.chrome.fade() {
        Fade::Full => palette::CHROME_FG,
        Fade::Dim => palette::CHROME_DIM,
        Fade::Hidden => return,
    };
    let Some(text) = now_playing(ui) else {
        return;
    };
    let y = body.y + body.height - 1;
    buf.set_stringn(
        body.x,
        y,
        &text,
        body.width as usize,
        Style::new().fg(fg).add_modifier(Modifier::DIM),
    );
}

/// Utilitarian mode: one dense, always-visible status row on the body's bottom
/// row. Never fades. Clips to the body width on a small pane.
fn render_utilitarian(buf: &mut Buffer, body: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    let source = if !ui.source.is_empty() {
        ui.source.as_str()
    } else {
        ui.label.as_deref().unwrap_or("—")
    };
    let tier = ui.tier.unwrap_or("bars");
    let beat = if snap.tempo_bpm == 0.0 {
        "beat —".to_string()
    } else {
        format!("beat {:.0}bpm", snap.tempo_bpm)
    };
    let line = format!(
        "{source} · {} · {tier} · {:.0}fps · rms {:.2} · {beat}",
        scene_id(ui),
        ui.fps_measured,
        snap.rms,
    );
    let y = body.y + body.height - 1;
    buf.set_stringn(
        body.x,
        y,
        &line,
        body.width as usize,
        Style::new().fg(palette::OVERLAY_FG),
    );
}

/// Instrument mode: a persistent one-row rail on the body's bottom row. Never
/// fades. Collapses to a compact VU-plus-lamp form below
/// [`INSTRUMENT_MIN_WIDTH`].
fn render_instrument(buf: &mut Buffer, body: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    let y = body.y + body.height - 1;
    let end_x = body.x + body.width;
    let dim = Style::new().fg(palette::DEBUG);
    let (lamp_glyph, lamp_style) = tempo_lamp(snap);

    // VU L/R. Stereo has not landed, so both channels show the mono rms level
    // (peak brightens the meter); this duplicates until a real L/R split exists.
    let vu = vu_bar(snap.rms);
    let vu_style = Style::new().fg(vu_color(snap.peak));

    let mut x = body.x;
    x = put(buf, x, y, end_x, "vu ", dim);
    x = put(buf, x, y, end_x, "L", dim);
    x = put(buf, x, y, end_x, &vu, vu_style);
    x = put(buf, x, y, end_x, " R", dim);
    x = put(buf, x, y, end_x, &vu, vu_style);
    x = put(buf, x, y, end_x, " · ", dim);
    x = put(buf, x, y, end_x, &lamp_glyph.to_string(), lamp_style);

    // The compact form stops after the beat lamp; the full rail adds bands, the
    // scene id and the fps.
    if body.width < INSTRUMENT_MIN_WIDTH {
        return;
    }

    x = put(buf, x, y, end_x, " · ", dim);
    let bands = format!(
        "b{} m{} t{}",
        band_glyph(snap.bands[0]),
        band_glyph(snap.bands[1]),
        band_glyph(snap.bands[2]),
    );
    x = put(buf, x, y, end_x, &bands, dim);
    x = put(buf, x, y, end_x, " · ", dim);
    x = put(
        buf,
        x,
        y,
        end_x,
        scene_id(ui),
        Style::new().fg(palette::OVERLAY_FG),
    );
    x = put(buf, x, y, end_x, " · ", dim);
    let fps = format!("{:.0}fps", ui.fps_measured);
    let _ = put(buf, x, y, end_x, &fps, dim);
}

/// Playful mode: the now-playing text on the body's bottom row, each glyph lifted
/// at most one cell by the onset-plus-loudness drive and the wave phase.
/// Deterministic: the offset is a pure function of the phase, the glyph index and
/// the drive — no RNG — and never exceeds one cell.
fn render_playful(buf: &mut Buffer, body: Rect, snap: &FeatureSnapshot, ui: &UiState) {
    let Some(text) = now_playing(ui) else {
        return;
    };
    let base_y = body.y + body.height - 1;
    // A glyph can only lift when there is a row above the base to lift into.
    let can_lift = body.height >= 2;
    let drive = playful_drive(snap);
    let phase = ui.chrome.phase;
    let bright = drive > PLAYFUL_LIFT_THRESHOLD;
    let style = if bright {
        Style::new()
            .fg(palette::CHROME_FG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
            .fg(palette::CHROME_DIM)
            .add_modifier(Modifier::DIM)
    };
    for (i, ch) in text.chars().enumerate() {
        let x = body.x + i as u16;
        if x >= body.x + body.width {
            break;
        }
        let off = if can_lift {
            glyph_offset(phase, i, drive)
        } else {
            0
        };
        let y = base_y - off;
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch).set_style(style);
        }
    }
}

/// The transient mode-switch toast: a single bold line in the body's top-left
/// corner, matching the scene-cycle toast.
fn render_toast(buf: &mut Buffer, body: Rect, text: &str) {
    let style = Style::new()
        .fg(palette::LABEL_FG)
        .bg(palette::LABEL_BG)
        .add_modifier(Modifier::BOLD);
    let line = format!(" {text} ");
    buf.set_stringn(body.x, body.y, &line, body.width as usize, style);
}

/// Write `text` starting at `x` on row `y`, one cell per character, stopping at
/// `end_x`. Returns the x just past the last cell written. Assumes single-width
/// glyphs, which every character the rails draw is.
fn put(buf: &mut Buffer, x: u16, y: u16, end_x: u16, text: &str, style: Style) -> u16 {
    let mut cx = x;
    for ch in text.chars() {
        if cx >= end_x {
            break;
        }
        if let Some(cell) = buf.cell_mut((cx, y)) {
            cell.set_char(ch).set_style(style);
        }
        cx += 1;
    }
    cx
}

/// The onset-plus-loudness drive for the playful lift, in `0.0..=1.0`. The onset
/// envelope decays over [`PLAYFUL_ENV_MS`]; loudness adds a floor so a steady
/// loud passage still sways.
fn playful_drive(snap: &FeatureSnapshot) -> f32 {
    let env = (1.0 - snap.onset_age_ms / PLAYFUL_ENV_MS).clamp(0.0, 1.0);
    (env * 0.7 + snap.rms.clamp(0.0, 1.0) * 0.6).clamp(0.0, 1.0)
}

/// The vertical lift for glyph `index` at wave `phase` under `drive`: `1` when
/// the driven wave crests past the threshold, else `0`. Never exceeds one cell,
/// and is a pure function of its inputs (deterministic; no RNG).
#[must_use]
pub(crate) fn glyph_offset(phase: f32, index: usize, drive: f32) -> u16 {
    let wave = (phase + index as f32 * PLAYFUL_GLYPH_SPACING).sin();
    u16::from(wave * drive.clamp(0.0, 1.0) > PLAYFUL_LIFT_THRESHOLD)
}

/// A [`VU_CELLS`]-cell VU bar for a `0.0..=1.0` level, using the eighth-block
/// ramp: filled cells are full blocks, the leading cell shows the fractional
/// eighth, the rest are low bars.
fn vu_bar(level: f32) -> String {
    const RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let eighths = (level.clamp(0.0, 1.0) * (VU_CELLS as f32) * 8.0).round() as u32;
    let mut s = String::with_capacity(VU_CELLS);
    for i in 0..VU_CELLS as u32 {
        let cell_eighths = eighths.saturating_sub(i * 8).min(8);
        s.push(if cell_eighths == 0 {
            '▁'
        } else {
            RAMP[(cell_eighths - 1) as usize]
        });
    }
    s
}

/// One block glyph for a band level, which is normalised so `1.0` is that band's
/// own recent average. Mapped through half-scale so an average level sits mid-ramp
/// and a swell fills.
fn band_glyph(band: f32) -> char {
    const RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let v = (band / 2.0).clamp(0.0, 1.0);
    let idx = ((v * 8.0).ceil() as usize).clamp(1, 8) - 1;
    RAMP[idx]
}

/// The tempo lamp glyph and style. When the beat is confidently locked the lamp
/// breathes with `beat_phase` (brightest on the beat); otherwise it flashes on a
/// recent onset, and is dim when neither holds.
fn tempo_lamp(snap: &FeatureSnapshot) -> (char, Style) {
    let confident = snap.tempo_bpm > 0.0 && snap.beat_confidence >= BEAT_CONFIDENCE_GATE;
    if confident {
        // `beat_phase` wraps 0..1 across the beat; brightness peaks near the beat
        // (phase 0) and dips at mid-beat.
        let breath = (1.0 - 2.0 * (snap.beat_phase - 0.5).abs()).clamp(0.0, 1.0);
        ('●', Style::new().fg(lamp_color(breath)))
    } else if snap.onset_age_ms <= ONSET_FLASH_MS {
        ('●', Style::new().fg(palette::QUIET))
    } else {
        (
            '○',
            Style::new().fg(palette::DEBUG).add_modifier(Modifier::DIM),
        )
    }
}

/// The tempo-lamp colour for a breath intensity in `0.0..=1.0`: a dim base at
/// mid-beat brightening toward the live green on the beat.
fn lamp_color(t: f32) -> Color {
    lerp_rgb((70, 70, 80), (120, 200, 120), t.clamp(0.0, 1.0))
}

/// The VU-bar colour, keyed on the peak level: teal when quiet, warming to amber
/// and red as the peak approaches clipping — the body gradient, reused.
fn vu_color(peak: f32) -> Color {
    palette::bar_color(peak.clamp(0.0, 1.0))
}

/// Linear interpolation between two RGB stops at `t` in `0.0..=1.0`.
fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_playing() -> FeatureSnapshot {
        FeatureSnapshot::default()
    }

    /// A `body` rect and an empty buffer to draw a rail into.
    fn body_buf(w: u16, h: u16) -> (Rect, Buffer) {
        let rect = Rect::new(0, 0, w, h);
        (rect, Buffer::empty(rect))
    }

    /// Concatenate a whole row into a string.
    fn row(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    // ---- ChromeMode -------------------------------------------------------

    #[test]
    fn mode_parses_and_cycles() {
        assert_eq!(ChromeMode::parse("invisible"), Some(ChromeMode::Invisible));
        assert_eq!(
            ChromeMode::parse("  INSTRUMENT "),
            Some(ChromeMode::Instrument)
        );
        assert_eq!(ChromeMode::parse("playful"), Some(ChromeMode::Playful));
        assert_eq!(
            ChromeMode::parse("utilitarian"),
            Some(ChromeMode::Utilitarian)
        );
        assert_eq!(ChromeMode::parse("nope"), None);
        // Default is invisible.
        assert_eq!(ChromeMode::default(), ChromeMode::Invisible);
        // The cycle wraps through all four in order.
        let mut m = ChromeMode::Invisible;
        for expected in [
            ChromeMode::Instrument,
            ChromeMode::Playful,
            ChromeMode::Utilitarian,
            ChromeMode::Invisible,
        ] {
            m = m.next();
            assert_eq!(m, expected);
        }
    }

    #[test]
    fn cycle_raises_a_naming_toast_and_resets_the_fade() {
        let mut cs = ChromeState::new(ChromeMode::Invisible);
        // Fade it out, then cycle: the toast names the new mode and the fade is
        // reset to full.
        cs.tick(10.0);
        assert_eq!(cs.fade(), Fade::Hidden);
        cs.cycle();
        assert_eq!(cs.mode(), ChromeMode::Instrument);
        let toast = cs.toast_text().expect("cycle raises a toast");
        assert!(
            toast.contains("instrument"),
            "toast names the mode: {toast:?}"
        );
        assert_eq!(cs.fade(), Fade::Full, "cycling resets the fade");
    }

    // ---- Invisible fade ---------------------------------------------------

    #[test]
    fn invisible_fades_after_four_seconds_and_input_returns_it() {
        let mut cs = ChromeState::default();
        assert_eq!(cs.fade(), Fade::Full);
        // Full through the hold window.
        cs.tick(2.9);
        assert_eq!(cs.fade(), Fade::Full);
        // A dim step between hold and end.
        cs.tick(0.6); // 3.5 s
        assert_eq!(cs.fade(), Fade::Dim);
        // Gone past ~4 s.
        cs.tick(1.0); // 4.5 s
        assert_eq!(cs.fade(), Fade::Hidden);
        // Input returns it to full.
        cs.on_input();
        assert_eq!(cs.fade(), Fade::Full);
        // A track change also returns it.
        cs.tick(10.0);
        assert_eq!(cs.fade(), Fade::Hidden);
        cs.on_track_change();
        assert_eq!(cs.fade(), Fade::Full);
    }

    #[test]
    fn invisible_line_draws_then_vanishes_on_the_fade() {
        let ui = UiState {
            source: "48000 Hz 2 ch".to_string(),
            chrome: ChromeState::new(ChromeMode::Invisible),
            ..UiState::default()
        };
        let (body, mut buf) = body_buf(40, 3);
        render(&mut buf, body, &snap_playing(), &ui);
        assert!(
            row(&buf, 2, 40).contains("live · 48000"),
            "fresh invisible line names the source"
        );

        // Faded out: the bottom row is blank.
        let mut faded = ui.clone();
        faded.chrome.tick(5.0);
        let (body, mut buf) = body_buf(40, 3);
        render(&mut buf, body, &snap_playing(), &faded);
        assert_eq!(
            row(&buf, 2, 40).trim(),
            "",
            "the invisible line vanishes once fully faded"
        );
    }

    #[test]
    fn invisible_line_is_absent_with_nothing_playing() {
        // No track, no label, no source: nothing to name, so nothing is drawn.
        let ui = UiState::default();
        let (body, mut buf) = body_buf(40, 3);
        render(&mut buf, body, &snap_playing(), &ui);
        for y in 0..3 {
            assert_eq!(row(&buf, y, 40).trim(), "", "row {y} must be blank");
        }
    }

    // ---- Instrument -------------------------------------------------------

    #[test]
    fn instrument_rail_renders_its_elements() {
        let snap = FeatureSnapshot {
            rms: 0.5,
            peak: 0.7,
            bands: [1.4, 0.9, 0.4],
            tempo_bpm: 128.0,
            beat_confidence: 0.8,
            beat_phase: 0.0,
            ..FeatureSnapshot::default()
        };
        let ui = UiState {
            scene_mode: true,
            scene_nav: crate::render::SceneNav::new(0), // spectra
            fps_measured: 60.0,
            chrome: ChromeState::new(ChromeMode::Instrument),
            ..UiState::default()
        };
        let (body, mut buf) = body_buf(80, 6);
        render(&mut buf, body, &snap, &ui);
        let rail = row(&buf, 5, 80);
        assert!(rail.contains("vu"), "rail has VU meters: {rail:?}");
        assert!(rail.contains("spectra"), "rail names the scene: {rail:?}");
        assert!(rail.contains("fps"), "rail shows fps: {rail:?}");
        assert!(
            rail.contains('●'),
            "confident beat lights the lamp: {rail:?}"
        );
        // A block glyph from the VU / band meters is present.
        assert!(
            rail.contains('█') || rail.contains('▇') || rail.contains('▅'),
            "rail draws meter blocks: {rail:?}"
        );
    }

    #[test]
    fn instrument_collapses_on_a_narrow_pane() {
        let snap = FeatureSnapshot {
            rms: 0.5,
            ..FeatureSnapshot::default()
        };
        let ui = UiState {
            scene_mode: true,
            fps_measured: 60.0,
            chrome: ChromeState::new(ChromeMode::Instrument),
            ..UiState::default()
        };
        // Narrower than INSTRUMENT_MIN_WIDTH: the compact form keeps the VU and
        // lamp but drops the bands / scene / fps, and never overflows the width.
        let (body, mut buf) = body_buf(20, 4);
        render(&mut buf, body, &snap, &ui);
        let rail = row(&buf, 3, 20);
        assert!(rail.contains("vu"), "compact rail keeps VU: {rail:?}");
        assert!(!rail.contains("fps"), "compact rail drops fps: {rail:?}");
    }

    #[test]
    fn instrument_lamp_flashes_on_onset_without_a_lock() {
        // No tempo lock, but a recent onset: the lamp flashes lit.
        let snap = FeatureSnapshot {
            tempo_bpm: 0.0,
            onset_age_ms: 20.0,
            ..FeatureSnapshot::default()
        };
        let ui = UiState {
            chrome: ChromeState::new(ChromeMode::Instrument),
            ..UiState::default()
        };
        let (body, mut buf) = body_buf(60, 4);
        render(&mut buf, body, &snap, &ui);
        assert!(
            row(&buf, 3, 60).contains('●'),
            "recent onset lights the lamp"
        );

        // No lock and no recent onset: the lamp is dim/open.
        let quiet = FeatureSnapshot {
            onset_age_ms: 5_000.0,
            ..snap
        };
        let (body, mut buf) = body_buf(60, 4);
        render(&mut buf, body, &quiet, &ui);
        assert!(
            row(&buf, 3, 60).contains('○'),
            "stale onset leaves the lamp open"
        );
    }

    // ---- Utilitarian ------------------------------------------------------

    #[test]
    fn utilitarian_row_never_fades() {
        let snap = FeatureSnapshot {
            rms: 0.33,
            tempo_bpm: 120.0,
            ..FeatureSnapshot::default()
        };
        let mut ui = UiState {
            source: "48000 Hz 2 ch".to_string(),
            tier: Some("octants"),
            fps_measured: 60.0,
            chrome: ChromeState::new(ChromeMode::Utilitarian),
            ..UiState::default()
        };
        // Idle well past the invisible fade window: utilitarian ignores it.
        ui.chrome.tick(30.0);
        let (body, mut buf) = body_buf(90, 5);
        render(&mut buf, body, &snap, &ui);
        let status = row(&buf, 4, 90);
        assert!(
            status.contains("48000 Hz 2 ch"),
            "row shows the source: {status:?}"
        );
        assert!(status.contains("octants"), "row shows the tier: {status:?}");
        assert!(status.contains("fps"), "row shows fps: {status:?}");
        assert!(status.contains("rms"), "row shows rms: {status:?}");
        assert!(status.contains("120bpm"), "row shows the beat: {status:?}");
    }

    // ---- Playful ----------------------------------------------------------

    #[test]
    fn playful_offset_stays_within_one_cell_and_is_deterministic() {
        // Over a grid of phases and drives, the offset is only ever 0 or 1, and
        // the same inputs always give the same output (no RNG).
        for step in 0..64 {
            let phase = step as f32 * (TAU / 64.0);
            for d in 0..=10 {
                let drive = d as f32 / 10.0;
                for i in 0..40 {
                    let a = glyph_offset(phase, i, drive);
                    let b = glyph_offset(phase, i, drive);
                    assert!(a <= 1, "offset never exceeds one cell");
                    assert_eq!(a, b, "offset is deterministic");
                }
            }
        }
    }

    #[test]
    fn playful_is_deterministic_across_a_fixed_snapshot_sequence() {
        // Two identical runs — same snapshot sequence, same dt — must produce the
        // same displacement pattern for the same text.
        let text_len = 24;
        let snaps: Vec<FeatureSnapshot> = (0..30)
            .map(|k| FeatureSnapshot {
                rms: 0.4,
                onset_age_ms: (k as f32 * 13.0) % 400.0,
                ..FeatureSnapshot::default()
            })
            .collect();

        let run = || {
            let mut cs = ChromeState::new(ChromeMode::Playful);
            let mut trace: Vec<Vec<u16>> = Vec::new();
            for s in &snaps {
                cs.tick(1.0 / 60.0);
                let drive = playful_drive(s);
                let frame: Vec<u16> = (0..text_len)
                    .map(|i| glyph_offset(cs.phase, i, drive))
                    .collect();
                trace.push(frame);
            }
            trace
        };

        assert_eq!(run(), run(), "playful displacement is reproducible");
    }

    #[test]
    fn playful_lifts_at_most_one_row_when_driven() {
        // A hard onset at high loudness drives lifts; every drawn glyph lands on
        // the base row or exactly one row above, never further.
        let snap = FeatureSnapshot {
            rms: 1.0,
            onset_age_ms: 0.0,
            ..FeatureSnapshot::default()
        };
        let mut ui = UiState {
            source: "now playing something".to_string(),
            chrome: ChromeState::new(ChromeMode::Playful),
            ..UiState::default()
        };
        ui.chrome.tick(0.25); // advance the wave off zero
        let (body, mut buf) = body_buf(40, 4);
        render(&mut buf, body, &snap, &ui);
        // Text only ever appears on the bottom row (y=3) or one above (y=2).
        for y in 0..2 {
            assert_eq!(
                row(&buf, y, 40).trim(),
                "",
                "no glyph lifts more than one row"
            );
        }
        let painted = row(&buf, 2, 40).trim() != "" || row(&buf, 3, 40).trim() != "";
        assert!(painted, "the playful line draws something");
    }
}
