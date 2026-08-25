//! The WSL guidance screen: a modal overlay shown when the Linux binary is
//! asked to capture live audio while running inside WSL (storyboard 1n).
//!
//! A Linux process inside WSL cannot see the Windows system mix — WSLg's
//! PulseAudio carries only WSL-app audio — so a live capture there would react
//! only to WSL sounds and otherwise sit black. Instead of that silent failure
//! this overlay names the situation and lays out the two supported paths with
//! copy-able steps. Capture still proceeds underneath (WSL-app audio is
//! legitimate, just labeled); the overlay is dismissed with `esc`.
//!
//! The model ([`WslScreen`]) is pure and TTY-free — open state plus whether the
//! command was copied this session — and [`draw_wsl`] renders it as a sibling of
//! the device picker: a framed panel top-left, degrading to a single summary
//! line on a small pane. Its keys are wired by the render loop: `[c]` copies the
//! command (an OSC 52 clipboard write via [`osc52_copy`], with the command kept
//! highlighted so a terminal that ignores OSC 52 can still be selected from by
//! hand), `[s]` switches to the demo feed, and `[?]` points at the docs file.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::palette;

/// The command `[c]` copies: the Windows-side bridge invocation that serves the
/// system-audio feature stream to WSL. `0.0.0.0` (not the bridge's own loopback
/// default) so the WSL side can reach it across the virtual network.
///
/// The winget package does not exist yet, so the steps present the manual path
/// (copy `scia-bridge.exe` to the Windows side and run it). The command itself
/// is stable, so a later card can swap the step *text* to a winget install
/// without changing this line or the layout.
pub const BRIDGE_COMMAND: &str = "scia-bridge --listen 0.0.0.0:7526";

/// The documentation file the `[?]` hint points at (repo-relative, neutral).
pub const DOCS_PATH: &str = "docs/wsl.md";

/// One content row of the panel, tagged so the copy-able command renders
/// highlighted (the OSC 52 fallback: it can be selected by hand).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WslLine {
    /// Ordinary explanatory or step text.
    Plain(String),
    /// The copy-able command line, rendered highlighted.
    Command(String),
}

impl WslLine {
    /// The row's text, whichever kind it is.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            WslLine::Plain(s) | WslLine::Command(s) => s,
        }
    }
}

/// The WSL guidance overlay model: whether it is open, and whether the command
/// has been copied this session (so the hint can acknowledge the keypress).
#[derive(Clone, Debug, Default)]
pub struct WslScreen {
    open: bool,
    copied: bool,
}

impl WslScreen {
    /// A screen that starts open when `open` (the binary detected WSL on a live
    /// capture) or closed otherwise.
    #[must_use]
    pub fn new(open: bool) -> Self {
        Self {
            open,
            copied: false,
        }
    }

    /// Whether the overlay is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the command has been copied this session.
    #[must_use]
    pub fn copied(&self) -> bool {
        self.copied
    }

    /// Close the overlay (capture continues underneath).
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Note that the command was copied, so the hint acknowledges it.
    pub fn mark_copied(&mut self) {
        self.copied = true;
    }

    /// The command `[c]` copies.
    #[must_use]
    pub fn command(&self) -> &'static str {
        BRIDGE_COMMAND
    }

    /// The panel's content rows (excluding the title and key hint): the honest
    /// explanation and the numbered steps for the bridge path, with the copy-able
    /// command tagged so it renders highlighted.
    #[must_use]
    pub fn lines(&self) -> Vec<WslLine> {
        vec![
            WslLine::Plain("Windows system audio is not visible from inside WSL.".to_owned()),
            WslLine::Plain(
                "WSL carries only WSL-app audio. Two supported paths reach the Windows mix:"
                    .to_owned(),
            ),
            WslLine::Plain("1. On Windows, get scia-bridge (copy scia-bridge.exe over;".to_owned()),
            WslLine::Plain(
                "   a winget package comes later) and run it to serve audio:".to_owned(),
            ),
            WslLine::Command(format!("     {BRIDGE_COMMAND}")),
            WslLine::Plain("2. In WSL, render that stream:".to_owned()),
            WslLine::Plain("     scia --input <windows-host>:7526".to_owned()),
            WslLine::Plain("Simpler: run scia.exe from this shell to use Windows audio".to_owned()),
            WslLine::Plain(
                "directly (the Windows PATH is on your WSL PATH by default).".to_owned(),
            ),
        ]
    }
}

// ---------------------------------------------------------------------------
// OSC 52 clipboard write
// ---------------------------------------------------------------------------

/// The standard base64 alphabet.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64-encode `src` (standard alphabet, `=` padding) into a fresh `String`.
fn base64(src: &[u8]) -> String {
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    let mut chunks = src.chunks_exact(3);
    for c in &mut chunks {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(B64[(n >> 6 & 63) as usize] as char);
        out.push(B64[(n & 63) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(B64[(n >> 18 & 63) as usize] as char);
            out.push(B64[(n >> 12 & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(B64[(n >> 18 & 63) as usize] as char);
            out.push(B64[(n >> 12 & 63) as usize] as char);
            out.push(B64[(n >> 6 & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Build the OSC 52 escape sequence that copies `text` to the terminal's
/// clipboard: `ESC ] 52 ; c ; <base64> BEL`. A terminal that speaks OSC 52 puts
/// `text` on the clipboard; one that does not ignores the sequence harmlessly
/// (the command is left highlighted for a manual selection either way).
#[must_use]
pub fn osc52_copy(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The chrome rows the panel needs beyond its content: a title row and a bottom
/// hint row.
const PANEL_CHROME_ROWS: u16 = 2;
/// The narrowest body that still hosts the full panel; below it (or when the body
/// is too short) the overlay degrades to a single summary line.
const PANEL_MIN_WIDTH: u16 = 32;

/// The key-hint row, acknowledging a copy once it has happened.
fn hint(screen: &WslScreen) -> String {
    let copy = if screen.copied {
        "[c] copied ✓"
    } else {
        "[c] copy command"
    };
    format!("{copy} · [s] demo mode · [?] {DOCS_PATH} · esc close")
}

/// Paint the WSL guidance overlay over the live canvas, top-left, like the device
/// picker. Draws nothing when the screen is closed or the body is degenerate.
/// Degrades to a single line on a small pane; long lines truncate with `…`, and
/// no row ever overflows the pane, so it never panics at small sizes.
pub fn draw_wsl(buf: &mut Buffer, body: Rect, screen: &WslScreen) {
    if !screen.is_open() || body.width == 0 || body.height == 0 {
        return;
    }
    let lines = screen.lines();
    let needed = lines.len() as u16 + PANEL_CHROME_ROWS;
    if body.height < needed || body.width < PANEL_MIN_WIDTH {
        render_line(buf, body, screen);
    } else {
        render_panel(buf, body, &lines, screen);
    }
}

/// The small-pane fallback: one line naming the state and the docs file.
fn render_line(buf: &mut Buffer, body: Rect, _screen: &WslScreen) {
    let style = Style::new()
        .fg(palette::OVERLAY_FG)
        .bg(palette::OVERLAY_BG)
        .add_modifier(Modifier::BOLD);
    buf.set_stringn(
        body.x,
        body.y,
        " WSL — no Windows audio ",
        body.width as usize,
        style,
    );
    if body.height >= 2 {
        buf.set_stringn(
            body.x,
            body.y + 1,
            truncate(&format!("see {DOCS_PATH}"), body.width as usize),
            body.width as usize,
            Style::new().fg(palette::OVERLAY_FG).bg(palette::OVERLAY_BG),
        );
    }
}

/// The full panel, framed and filled top-left of the body: a title, the content
/// rows (the command highlighted), and the key hint.
fn render_panel(buf: &mut Buffer, body: Rect, lines: &[WslLine], screen: &WslScreen) {
    let title = "WSL — Windows audio not visible";
    let hint = hint(screen);
    let content_w = lines
        .iter()
        .map(|l| l.text().chars().count())
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

    for (i, line) in lines.iter().enumerate() {
        let y = panel.y + 1 + i as u16;
        // Keep the bottom row for the hint.
        if y >= panel.y + panel.height - 1 {
            break;
        }
        let style = match line {
            // The command is highlighted so it can be selected by hand on a
            // terminal that ignores the OSC 52 write.
            WslLine::Command(_) => fill.add_modifier(Modifier::REVERSED),
            WslLine::Plain(_) => fill,
        };
        buf.set_stringn(inner_x, y, truncate(line.text(), inner_w), inner_w, style);
    }

    let hint_y = panel.y + panel.height - 1;
    buf.set_stringn(
        inner_x,
        hint_y,
        truncate(&hint, inner_w),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn osc52_wraps_base64_in_the_escape() {
        let seq = osc52_copy("hi");
        let s = String::from_utf8(seq).unwrap();
        assert_eq!(s, "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn lines_include_numbered_steps_and_the_command() {
        let screen = WslScreen::new(true);
        let lines = screen.lines();
        let all: String = lines
            .iter()
            .map(WslLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("1."), "a numbered step: {all:?}");
        assert!(all.contains("2."), "a numbered step: {all:?}");
        assert!(
            lines
                .iter()
                .any(|l| matches!(l, WslLine::Command(c) if c.contains(BRIDGE_COMMAND))),
            "the command line is present and tagged: {all:?}"
        );
    }

    #[test]
    fn copied_flips_the_hint() {
        let mut screen = WslScreen::new(true);
        assert!(hint(&screen).contains("copy command"));
        screen.mark_copied();
        assert!(screen.copied());
        assert!(hint(&screen).contains("copied"));
    }
}
