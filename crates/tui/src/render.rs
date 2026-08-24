//! Pure rendering: [`draw`] paints one frame from a [`FeatureSnapshot`] and a
//! [`UiState`] into a ratatui [`Frame`]. It is deliberately side-effect free
//! (no terminal, no timing) so it can be exercised with ratatui's
//! `TestBackend`. The render loop in [`crate::run`] owns everything stateful.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use scia_core::{Activity, EngineStats, FeatureSnapshot};
use scia_scenes::{SceneInfo, builtin_scenes};

use crate::chrome::ChromeState;
use crate::keymap::{InputAction, Keymap};
use crate::nowplaying::{self, NowPlayingState};
use crate::palette;
use crate::tuning::TuningStrip;

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
    /// Whether the scene browser and live cycling are active. Set by the render
    /// loop when a built-in scene presenter is driving the body; the direct-bars
    /// renderer and the disk-preset (`--scene-file`) path leave it `false`, so
    /// the browser keys stay inert there. Modelled on
    /// [`overlay`](UiState::overlay): a plain flag the input handler reads.
    pub scene_mode: bool,
    /// The scene-browser overlay and live scene-cycle state. Drawn over the live
    /// canvas like the meter bridge when open, and consulted each frame by the
    /// loop to retarget the presenter's crossfade.
    pub scene_nav: SceneNav,
    /// The active key bindings, so the help overlay lists the current keys and
    /// the input handler resolves rebound actions. Defaults to the built-in set.
    pub keymap: Keymap,
    /// Whether the scene is frozen. When set, the header shows a paused marker;
    /// the render loop feeds the presenter a frozen snapshot and `dt = 0`.
    pub paused: bool,
    /// Whether the in-app key help overlay is shown (toggled with `?`).
    pub help: bool,
    /// Whether the now-playing panel is shown (toggled with the now-playing key).
    pub show_now_playing: bool,
    /// Whether the current track's art palette is applied to the live scene. Set
    /// by the render loop when the palette key takes effect; drives the panel's
    /// "palette applied" marker and the loop's toggle-back.
    pub palette_applied: bool,
    /// A one-shot request from the palette key, consumed by the render loop:
    /// apply the art palette (or revert) on the next tick.
    pub palette_pending: bool,
    /// The now-playing metadata the loop keeps current from the backend event
    /// stream. Empty (nothing playing) by default.
    pub now_playing: NowPlayingState,
    /// The now-playing track line, when the metadata backend has one. `None`
    /// until the now-playing metadata seam is wired in; [`track_line`] is the one
    /// accessor the chrome reads, so wiring the value here lights every chrome
    /// mode at once.
    ///
    /// [`track_line`]: UiState::track_line
    pub track: Option<String>,
    /// The chrome-personality state: the active mode plus its fade / wave / toast
    /// timers, advanced by the frame `dt`.
    pub chrome: ChromeState,
    /// The quick tuning strip model: the parameters on show, the selection, and
    /// the keys adjusted this session. Drawn over the body bottom when open.
    pub tuning: TuningStrip,
    /// A one-shot request from the tuning key to open the strip, consumed by the
    /// render loop (which owns the presenter it seeds the strip from).
    pub tuning_open_pending: bool,
    /// A one-shot request from the strip's write key, consumed by the render loop
    /// to write the adjusted values back to the preset file.
    pub tuning_write_pending: bool,
}

impl UiState {
    /// The now-playing track line, or `None` when the metadata backend has not
    /// supplied one. The single track-line seam: it returns `None` today, and the
    /// chrome falls back to the source label the header shows. When the metadata
    /// branch sets [`track`](UiState::track), the real value flows through here
    /// with no other change.
    #[must_use]
    pub fn track_line(&self) -> Option<&str> {
        self.track.as_deref()
    }
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
        // The chrome personality paints over the scene body, before the debug
        // and help overlays, which are separate surfaces layered above it.
        crate::chrome::render(buf, body, snap, ui);
        if ui.overlay {
            render_overlay(buf, body, snap, ui);
        }
        // The browser panel and cycle toast paint over the body, last, like the
        // meter bridge. Inert unless the browser is open or a toast is up.
        draw_scene_nav(buf, body, &ui.scene_nav);
        // The now-playing panel paints over the body like the meter bridge.
        if ui.show_now_playing {
            nowplaying::draw_now_playing(buf, body, &ui.now_playing, ui.palette_applied);
        }
        // The help overlay is the topmost body layer; inert unless toggled on.
        draw_help(buf, body, ui);
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

// ---------------------------------------------------------------------------
// Scene browser + live cycling
// ---------------------------------------------------------------------------

/// Seconds a scene-cycle toast stays up before it disappears. The storyboard
/// calls for roughly two seconds.
const TOAST_SECS: f32 = 2.0;

/// A transient corner toast: a single line shown for a bounded time, its life
/// counted down on the frame clock (no wall clock). One small reusable
/// primitive — the design reserves it for hot-reload confirmations too, but the
/// scene-cycle confirmation is its only wiring today.
#[derive(Clone, Debug)]
pub(crate) struct Toast {
    text: String,
    remaining: f32,
}

impl Toast {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            remaining: TOAST_SECS,
        }
    }

    /// Advance the toast by `dt` seconds; returns whether it is still alive.
    pub(crate) fn tick(&mut self, dt: f32) -> bool {
        self.remaining -= dt.max(0.0);
        self.remaining > 0.0
    }

    /// The toast's text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// The scene-browser overlay and live scene-cycle state machine.
///
/// It holds the registry listing (static), the committed scene index, the
/// browser's open/highlight/origin state, a pending switch the render loop
/// applies to the presenter, and the cycle toast. It is pure state: the input
/// handler drives it and the loop reads [`take_pending`](Self::take_pending)
/// once per frame to retarget the presenter's crossfade — no blending lives
/// here, so every transition reuses the one switch path.
///
/// The browser marks the committed scene and moves a separate highlight cursor;
/// each highlight move previews live (a crossfade to the highlighted scene)
/// without committing, so `Enter` keeps the preview and `Esc` crossfades back to
/// the scene that was committed when the browser opened.
#[derive(Clone, Debug)]
pub struct SceneNav {
    /// The built-in registry, in listing order (static).
    scenes: &'static [SceneInfo],
    /// The committed (marked) scene index.
    current: usize,
    /// Whether the browser overlay is open.
    open: bool,
    /// The highlight cursor while the browser is open.
    highlight: usize,
    /// The committed index when the browser opened, restored on `Esc`.
    origin: usize,
    /// A scene index the loop must crossfade the presenter to, if any.
    pending: Option<usize>,
    /// The live cycle toast, if showing.
    toast: Option<Toast>,
}

impl Default for SceneNav {
    fn default() -> Self {
        Self::new(0)
    }
}

impl SceneNav {
    /// Build the navigator committed to `initial` (clamped into the registry).
    #[must_use]
    pub fn new(initial: usize) -> Self {
        let scenes = builtin_scenes();
        let current = if scenes.is_empty() {
            0
        } else {
            initial.min(scenes.len() - 1)
        };
        Self {
            scenes,
            current,
            open: false,
            highlight: current,
            origin: current,
            pending: None,
            toast: None,
        }
    }

    /// Whether the browser overlay is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The committed (marked) scene index.
    #[must_use]
    pub fn current(&self) -> usize {
        self.current
    }

    /// The committed scene's id, if the registry is non-empty.
    #[must_use]
    pub fn current_id(&self) -> Option<&'static str> {
        self.scenes.get(self.current).map(|s| s.id)
    }

    /// The highlight cursor index.
    #[must_use]
    pub fn highlight(&self) -> usize {
        self.highlight
    }

    /// The current toast text, if one is showing.
    #[must_use]
    pub fn toast_text(&self) -> Option<&str> {
        self.toast.as_ref().map(|t| t.text.as_str())
    }

    /// Toggle the browser. Opening snaps the highlight to the committed scene
    /// and remembers it for restore; a second `Tab` closes it keeping the
    /// highlighted scene, the same as `Enter`.
    pub fn toggle_browser(&mut self) {
        if self.scenes.is_empty() {
            return;
        }
        if self.open {
            self.accept();
        } else {
            self.open = true;
            self.origin = self.current;
            self.highlight = self.current;
        }
    }

    /// Commit the highlighted scene and close the browser. The highlighted scene
    /// is already live from the last preview, so no new switch is requested.
    pub fn accept(&mut self) {
        if !self.open {
            return;
        }
        self.current = self.highlight;
        self.open = false;
    }

    /// Close the browser and crossfade back to the scene committed when it
    /// opened. The committed scene never moved while browsing, so only the live
    /// preview needs restoring.
    pub fn cancel(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        if self.highlight != self.origin {
            self.pending = Some(self.origin);
        }
        self.highlight = self.origin;
    }

    /// Move the highlight one scene toward the end of the list, previewing it.
    pub fn highlight_next(&mut self) {
        self.move_highlight(1);
    }

    /// Move the highlight one scene toward the start of the list, previewing it.
    pub fn highlight_prev(&mut self) {
        self.move_highlight(-1);
    }

    /// Move the highlight by `delta`, clamped to the list. A move that lands on a
    /// new scene previews it live (a crossfade) without committing.
    fn move_highlight(&mut self, delta: isize) {
        if !self.open || self.scenes.is_empty() {
            return;
        }
        let last = self.scenes.len() as isize - 1;
        let next = (self.highlight as isize + delta).clamp(0, last) as usize;
        if next != self.highlight {
            self.highlight = next;
            self.pending = Some(next);
        }
    }

    /// Cycle the committed scene one step forward, wrapping, and raise a toast.
    pub fn cycle_next(&mut self) {
        self.cycle(1);
    }

    /// Cycle the committed scene one step backward, wrapping, and raise a toast.
    pub fn cycle_prev(&mut self) {
        self.cycle(-1);
    }

    /// Cycle the committed scene by `delta` in registry order (wrapping), request
    /// the crossfade, and raise the naming toast. Inert while the browser is
    /// open — cycling is the outside-the-browser gesture.
    fn cycle(&mut self, delta: isize) {
        if self.open || self.scenes.is_empty() {
            return;
        }
        let n = self.scenes.len() as isize;
        let next = (self.current as isize + delta).rem_euclid(n) as usize;
        self.current = next;
        self.highlight = next;
        self.pending = Some(next);
        self.toast = Some(Toast::new(self.toast_line(next)));
    }

    /// Age the toast on the frame clock, dropping it once its timer runs out.
    pub fn tick(&mut self, dt: f32) {
        if let Some(toast) = self.toast.as_mut() {
            if !toast.tick(dt) {
                self.toast = None;
            }
        }
    }

    /// Take the pending scene id the loop should crossfade the presenter to, if
    /// any. Only the latest target survives a burst of moves, so a rapid
    /// sequence retargets one fade rather than queuing several.
    #[must_use]
    pub fn take_pending(&mut self) -> Option<&'static str> {
        let idx = self.pending.take()?;
        self.scenes.get(idx).map(|s| s.id)
    }

    /// The toast line for a committed scene: its name plus position dots, e.g.
    /// `lattice   · ● · ·`.
    fn toast_line(&self, idx: usize) -> String {
        let name = self.scenes.get(idx).map_or("", |s| s.id);
        let mut dots = String::with_capacity(self.scenes.len().saturating_mul(2));
        for i in 0..self.scenes.len() {
            if i > 0 {
                dots.push(' ');
            }
            dots.push(if i == idx { '●' } else { '·' });
        }
        format!("{name}   {dots}")
    }
}

/// The chrome rows the browser panel needs beyond one row per scene: a title
/// row and a bottom hint row.
const BROWSER_CHROME_ROWS: u16 = 2;
/// The narrowest body that still hosts the full panel; below it (or when the
/// body is too short) the browser degrades to a single summary line.
const BROWSER_MIN_WIDTH: u16 = 24;

/// Paint the scene browser and cycle toast over the live canvas, like the meter
/// bridge. Draw this after the presenter so it overlays the scene. Draws nothing
/// when the browser is closed and no toast is showing.
pub fn draw_scene_nav(buf: &mut Buffer, body: Rect, nav: &SceneNav) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    if let Some(text) = nav.toast_text() {
        render_toast(buf, body, text);
    }
    if nav.open {
        render_browser(buf, body, nav);
    }
}

/// The cycle toast: a single bold line in the body's top-left corner.
fn render_toast(buf: &mut Buffer, body: Rect, text: &str) {
    let style = Style::new()
        .fg(palette::LABEL_FG)
        .bg(palette::LABEL_BG)
        .add_modifier(Modifier::BOLD);
    let line = format!(" {text} ");
    buf.set_stringn(body.x, body.y, &line, body.width as usize, style);
}

/// The browser overlay: the full list panel when the body has room, otherwise a
/// single summary line — degrading the way the meter bridge falls back to its
/// debug line on a small pane.
fn render_browser(buf: &mut Buffer, body: Rect, nav: &SceneNav) {
    let needed_rows = nav.scenes.len() as u16 + BROWSER_CHROME_ROWS;
    if body.height < needed_rows || body.width < BROWSER_MIN_WIDTH {
        render_browser_line(buf, body, nav);
        return;
    }
    render_browser_panel(buf, body, nav);
}

/// The small-pane fallback: one line naming the highlighted scene and its
/// position, on the body's top row.
fn render_browser_line(buf: &mut Buffer, body: Rect, nav: &SceneNav) {
    let name = nav.scenes.get(nav.highlight).map_or("", |s| s.id);
    let line = format!(
        " browse {} [{}/{}] ",
        name,
        nav.highlight + 1,
        nav.scenes.len()
    );
    let style = Style::new()
        .fg(palette::OVERLAY_FG)
        .bg(palette::OVERLAY_BG)
        .add_modifier(Modifier::BOLD);
    buf.set_stringn(body.x, body.y, &line, body.width as usize, style);
}

/// The full list panel, framed and filled, top-left of the body: a title, one
/// row per scene (marker for the committed scene, cursor for the highlight,
/// name and mood), and a key hint.
fn render_browser_panel(buf: &mut Buffer, body: Rect, nav: &SceneNav) {
    let rows = nav.scenes.len() as u16 + BROWSER_CHROME_ROWS;
    let name_w = nav.scenes.iter().map(|s| s.id.len()).max().unwrap_or(0);
    let mood_w = nav.scenes.iter().map(|s| s.mood.len()).max().unwrap_or(0);
    // markers + gaps + the two separators, then clamp to the body.
    let want = (name_w + mood_w + 8) as u16;
    let width = want.clamp(BROWSER_MIN_WIDTH, body.width);
    let height = rows.min(body.height);
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
        "scenes",
        inner_w,
        fill.add_modifier(Modifier::BOLD),
    );

    for (i, info) in nav.scenes.iter().enumerate() {
        let y = panel.y + 1 + i as u16;
        // The bottom row is reserved for the hint; stop if a scene would land on
        // or past it (a body clamped shorter than the list can happen mid-resize).
        if y >= panel.y + panel.height - 1 {
            break;
        }
        let cursor = if i == nav.highlight { '▸' } else { ' ' };
        let mark = if i == nav.current { '●' } else { '·' };
        let text = format!("{cursor}{mark} {} · {}", info.id, info.mood);
        let mut style = fill;
        if i == nav.highlight {
            style = style.add_modifier(Modifier::REVERSED);
        }
        buf.set_stringn(inner_x, y, &text, inner_w, style);
    }

    let hint_y = panel.y + panel.height - 1;
    buf.set_stringn(
        inner_x,
        hint_y,
        "↑↓ move · enter keep · esc back",
        inner_w,
        fill.add_modifier(Modifier::DIM),
    );
}

// ---------------------------------------------------------------------------
// In-app key help overlay
// ---------------------------------------------------------------------------

/// The chrome rows the help panel needs beyond its binding rows: a title row and
/// a bottom hint row.
const HELP_CHROME_ROWS: u16 = 2;
/// The narrowest body that still hosts the full help panel; below it (or when
/// the body is too short) the overlay degrades to a single line, the way the
/// meter bridge falls back on a small pane.
const HELP_MIN_WIDTH: u16 = 30;

/// The active-key rows the help overlay lists: each rebindable action with its
/// current binding, plus the structural keys. Built from `keymap` so a rebind is
/// reflected immediately.
fn help_rows(keymap: &Keymap) -> Vec<(String, &'static str)> {
    let mut rows = Vec::new();
    for action in InputAction::ALL {
        let key = match keymap.get(action) {
            Some(chord) => chord.display(),
            None => "—".to_string(),
        };
        rows.push((key, action.label()));
    }
    // Structural keys the loop owns directly (not rebindable).
    rows.push(("esc".to_string(), "back / quit"));
    rows.push(("↑↓ jk".to_string(), "browse move"));
    rows.push(("enter".to_string(), "browse keep"));
    rows.push(("d".to_string(), "debug line"));
    rows.push(("ctrl+c".to_string(), "force quit"));
    rows.push(("?".to_string(), "toggle help"));
    rows
}

/// Paint the in-app key help over the body, topmost, when `ui.help` is set.
/// Draws the full panel when the body has room and a single summary line when it
/// does not, mirroring the scene browser's small-pane fallback. Draws nothing
/// when the overlay is off or the body is degenerate.
pub fn draw_help(buf: &mut Buffer, body: Rect, ui: &UiState) {
    if !ui.help || body.width == 0 || body.height == 0 {
        return;
    }
    let rows = help_rows(&ui.keymap);
    let needed = rows.len() as u16 + HELP_CHROME_ROWS;
    if body.height < needed || body.width < HELP_MIN_WIDTH {
        render_help_line(buf, body);
    } else {
        render_help_panel(buf, body, &rows);
    }
}

/// The small-pane fallback: one line on the body's top row.
fn render_help_line(buf: &mut Buffer, body: Rect) {
    let style = Style::new()
        .fg(palette::OVERLAY_FG)
        .bg(palette::OVERLAY_BG)
        .add_modifier(Modifier::BOLD);
    buf.set_stringn(
        body.x,
        body.y,
        " keys — ? closes ",
        body.width as usize,
        style,
    );
}

/// The full help panel: a title, one row per active binding, and a hint. Framed
/// and filled top-left like the scene browser.
fn render_help_panel(buf: &mut Buffer, body: Rect, rows: &[(String, &'static str)]) {
    let key_w = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let label_w = rows
        .iter()
        .map(|(_, l)| l.chars().count())
        .max()
        .unwrap_or(0);
    // key column + gap + label, plus a left and right pad.
    let want = (key_w + label_w + 4) as u16;
    let width = want.clamp(HELP_MIN_WIDTH, body.width);
    let height = (rows.len() as u16 + HELP_CHROME_ROWS).min(body.height);
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
        "keys",
        inner_w,
        fill.add_modifier(Modifier::BOLD),
    );

    for (i, (key, label)) in rows.iter().enumerate() {
        let y = panel.y + 1 + i as u16;
        // Keep the bottom row for the hint.
        if y >= panel.y + panel.height - 1 {
            break;
        }
        let text = format!("{key:<key_w$}  {label}");
        buf.set_stringn(inner_x, y, &text, inner_w, fill);
    }

    let hint_y = panel.y + panel.height - 1;
    buf.set_stringn(
        inner_x,
        hint_y,
        "? closes",
        inner_w,
        fill.add_modifier(Modifier::DIM),
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
    // starved bit, so `quiet` and `idle` are distinguishable at a glance. A
    // `paused` marker leads it while the scene is frozen.
    let activity = ui.stats.activity;
    let paused = if ui.paused { "paused · " } else { "" };
    let right = format!(
        "{}{}  gen {}",
        paused,
        activity_label(activity),
        snap.generation
    );
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
    // The beat segment reads unambiguously: `tempo_bpm == 0.0` means the tracker
    // is unlocked, shown as an em dash, so the confidence can never be mistaken
    // for the BPM. Locked shows the tempo and confidence side by side.
    let beat = if snap.tempo_bpm == 0.0 {
        format!("beat — · conf {:.2}", snap.beat_confidence)
    } else {
        format!(
            "beat {:.0}bpm · conf {:.2}",
            snap.tempo_bpm, snap.beat_confidence
        )
    };
    let l3 = format!(
        "rms {:.2} peak {:.2} · bass/mid/treb {:.2}/{:.2}/{:.2} · width {:.2} · flux {:.2} · \
         onset {} · {}",
        snap.rms,
        snap.peak,
        snap.bands[0],
        snap.bands[1],
        snap.bands[2],
        snap.mid_side_ratio,
        snap.flux,
        lamp,
        beat,
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
