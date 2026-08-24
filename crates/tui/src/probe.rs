//! Runtime terminal capability probing.
//!
//! Capability detection here is *runtime probing* — control-sequence queries
//! sent to the real tty with bounded reads — not a hardcoded terminal
//! allowlist. An unknown terminal that answers nothing simply falls back to the
//! safe cell-mosaic [`Tier::Half`]. The reply *parsers* are pure and separated
//! from the tty I/O so they unit-test on any host; only [`probe`] touches a
//! terminal.
//!
//! ## Why there is no automatic missing-glyph check
//!
//! Whether a terminal can draw the mosaic ladder's finer glyphs (octants,
//! sextants) is a property of the *font*, not the terminal, and it is invisible
//! to escape-sequence probing: a font that lacks a Symbols-for-Legacy-Computing
//! glyph still advances the cursor one cell for the replacement box, so nothing
//! in the terminal's replies reveals the gap (probe P3,
//! `docs/probes/p3-wt-octant-glyph-coverage.md`). Automatic detection from the
//! terminal alone is therefore impossible. Instead the default rung is chosen
//! per terminal *family* (P3-informed, see [`default_tier`]) with a manual
//! `--presenter` override; the interactive first-run confirmation is a separate
//! card.

#![forbid(unsafe_code)]

use std::env;
use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::mosaic::Tier;

/// The terminal family a report was classified into, derived from environment
/// variables. Drives the P3-informed default tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermFamily {
    /// Windows Terminal (`WT_SESSION` set).
    WindowsTerminal,
    /// Ghostty (`TERM_PROGRAM == "ghostty"` or `GHOSTTY_RESOURCES_DIR` set).
    Ghostty,
    /// kitty (`KITTY_WINDOW_ID` set or `TERM` starts with `xterm-kitty`).
    Kitty,
    /// Anything else — treated conservatively.
    Other,
}

impl fmt::Display for TermFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TermFamily::WindowsTerminal => "windows-terminal",
            TermFamily::Ghostty => "ghostty",
            TermFamily::Kitty => "kitty",
            TermFamily::Other => "other",
        };
        f.write_str(s)
    }
}

/// Parsed synchronized-output (DEC mode 2026) support state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncSupport {
    /// The mode is recognized (DECRPM value 1 = set, or 2 = reset).
    Supported,
    /// The mode is not recognized (value 0), or permanently unsettable (4).
    Unsupported,
}

/// The primary device attributes (DA1) reply, reduced to its attribute list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Da1 {
    /// The attribute numbers, including the leading terminal-class code.
    /// Attribute `4` means sixel graphics are present.
    pub attrs: Vec<u16>,
}

/// The facts a [`probe`] gathered about the current terminal. `Display` is one
/// compact line, suitable for the debug overlay or a stderr note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityReport {
    /// 24-bit color: `COLORTERM` advertises it, or the family always has it.
    pub truecolor: bool,
    /// Sixel graphics (DA1 attribute `4`).
    pub sixel: bool,
    /// Synchronized output, DEC mode 2026 (DECRQM recognized).
    pub sync_2026: bool,
    /// Cell size in pixels `(height, width)`, from `CSI 16 t`, if reported.
    pub cell_px: Option<(u16, u16)>,
    /// The kitty graphics protocol (an APC `_G…` reply preceded DA1).
    pub kitty_graphics: bool,
    /// The classified terminal family.
    pub family: TermFamily,
}

impl fmt::Display for CapabilityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cell = match self.cell_px {
            Some((h, w)) => format!("{w}x{h}px"),
            None => "?".to_string(),
        };
        write!(
            f,
            "caps: {} truecolor={} sixel={} sync2026={} kitty-graphics={} cell={}",
            self.family,
            yes_no(self.truecolor),
            yes_no(self.sixel),
            yes_no(self.sync_2026),
            yes_no(self.kitty_graphics),
            cell,
        )
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

// ---------------------------------------------------------------------------
// Pure reply parsers (tty-free, unit-tested everywhere)
// ---------------------------------------------------------------------------

/// Parse a primary device attributes reply, `CSI ? <attrs> c` (e.g.
/// `\x1b[?61;4;6;…c`). Scans for the first `\x1b[?…c` whose body is a
/// well-formed `;`-separated number list. Returns `None` on anything malformed.
#[must_use]
pub fn parse_da1(reply: &str) -> Option<Da1> {
    let mut rest = reply;
    while let Some(idx) = rest.find("\x1b[?") {
        let after = &rest[idx + 3..];
        if let Some(end) = after.find('c') {
            if let Some(attrs) = parse_number_list(&after[..end]) {
                return Some(Da1 { attrs });
            }
        }
        rest = after;
    }
    None
}

/// Parse a DECRQM reply for mode 2026, `CSI ? 2026 ; N $ y`. Anchors on the
/// `$y` terminator so it is robust inside a buffer that also holds other
/// replies. `N` of 1|2 → [`SyncSupport::Supported`]; 0|4 →
/// [`SyncSupport::Unsupported`]; anything else, or a malformed reply, → `None`.
#[must_use]
pub fn parse_decrqm_2026(reply: &str) -> Option<SyncSupport> {
    let end = reply.find("$y")?;
    let head = &reply[..end];
    let start = head.rfind("\x1b[?")?;
    let mut parts = head[start + 3..].split(';');
    let mode: u16 = parts.next()?.parse().ok()?;
    if mode != 2026 {
        return None;
    }
    let value: u16 = parts.next()?.parse().ok()?;
    match value {
        1 | 2 => Some(SyncSupport::Supported),
        0 | 4 => Some(SyncSupport::Unsupported),
        _ => None,
    }
}

/// Parse a cell-size report, `CSI 6 ; H ; W t` (e.g. `\x1b[6;20;10t`).
/// Returns `(height_px, width_px)`, or `None` when malformed.
#[must_use]
pub fn parse_cell_size(reply: &str) -> Option<(u16, u16)> {
    let start = reply.find("\x1b[6;")?;
    let after = &reply[start + 4..];
    let end = after.find('t')?;
    let mut parts = after[..end].split(';');
    let h: u16 = parts.next()?.parse().ok()?;
    let w: u16 = parts.next()?.parse().ok()?;
    Some((h, w))
}

/// Parse a `;`-separated number list into `u16`s, tolerating empty fields.
/// `None` if any field is non-numeric or the list is empty.
fn parse_number_list(body: &str) -> Option<Vec<u16>> {
    let mut attrs = Vec::new();
    for part in body.split(';') {
        if part.is_empty() {
            continue;
        }
        attrs.push(part.parse::<u16>().ok()?);
    }
    if attrs.is_empty() { None } else { Some(attrs) }
}

// ---------------------------------------------------------------------------
// Pure environment classifiers (tty-free, unit-tested everywhere)
// ---------------------------------------------------------------------------

/// Classify the terminal family from the relevant environment values. Pure so
/// it is tested without mutating the process environment.
#[must_use]
pub fn classify_family(
    wt_session: Option<&str>,
    term_program: Option<&str>,
    ghostty_resources_dir: Option<&str>,
    kitty_window_id: Option<&str>,
    term: Option<&str>,
) -> TermFamily {
    if wt_session.is_some() {
        return TermFamily::WindowsTerminal;
    }
    if term_program == Some("ghostty") || ghostty_resources_dir.is_some() {
        return TermFamily::Ghostty;
    }
    if kitty_window_id.is_some() || term.is_some_and(|t| t.starts_with("xterm-kitty")) {
        return TermFamily::Kitty;
    }
    TermFamily::Other
}

/// Decide truecolor from `COLORTERM` and the family. Pure for testability.
#[must_use]
pub fn truecolor_from(colorterm: Option<&str>, family: TermFamily) -> bool {
    if let Some(ct) = colorterm {
        if ct.contains("truecolor") || ct.contains("24bit") {
            return true;
        }
    }
    matches!(
        family,
        TermFamily::WindowsTerminal | TermFamily::Ghostty | TermFamily::Kitty
    )
}

/// The environment-only report: family + truecolor from env, every probed fact
/// left at its safe default. Returned verbatim when there is no tty.
fn env_report() -> CapabilityReport {
    let family = classify_family(
        env::var("WT_SESSION").ok().as_deref(),
        env::var("TERM_PROGRAM").ok().as_deref(),
        env::var("GHOSTTY_RESOURCES_DIR").ok().as_deref(),
        env::var("KITTY_WINDOW_ID").ok().as_deref(),
        env::var("TERM").ok().as_deref(),
    );
    CapabilityReport {
        truecolor: truecolor_from(env::var("COLORTERM").ok().as_deref(), family),
        sixel: false,
        sync_2026: false,
        cell_px: None,
        kitty_graphics: false,
        family,
    }
}

// ---------------------------------------------------------------------------
// The default-tier ladder (P3-informed)
// ---------------------------------------------------------------------------

/// The mosaic tier to start on for a report — the P3-informed ladder start.
///
/// - Ghostty and kitty rasterize the Symbols-for-Legacy-Computing glyphs with
///   their built-in glyph rasterizers, so octants are safe regardless of the
///   user font → [`Tier::Octant`].
/// - Windows Terminal delegates to the user font, and P3
///   (`docs/probes/p3-wt-octant-glyph-coverage.md`) found octants and sextants
///   commonly absent (tofu) on a real WT + Nerd-Font setup, while quadrants
///   always render → [`Tier::Quadrant`].
/// - Anything else gets the universally-safe half-block rung → [`Tier::Half`].
///
/// The sixel and kitty-graphics *presenters* are a later card (US-TUI-3); the
/// report records those facts now, but the mosaic tier is what is selected.
#[must_use]
pub fn default_tier(report: &CapabilityReport) -> Tier {
    match report.family {
        TermFamily::Ghostty | TermFamily::Kitty => Tier::Octant,
        TermFamily::WindowsTerminal => Tier::Quadrant,
        TermFamily::Other => Tier::Half,
    }
}

// ---------------------------------------------------------------------------
// The live probe
// ---------------------------------------------------------------------------

/// Probe the real terminal for its capabilities.
///
/// Runs on the current tty in raw mode (restored on every path by an RAII
/// guard), *before* the alternate screen. It sends, in order: the kitty
/// graphics query fenced by DA1, a DECRQM query for synchronized output (mode
/// 2026), and a cell-size query; a cursor-position report (DSR) is appended as a
/// drain sentinel that every real terminal answers, so the reader thread always
/// sees a final byte and exits rather than parking on a blocking read. Every
/// read is bounded by `timeout_per_query` (default 150 ms; total under ~600 ms
/// across the queries), interleaved or absent replies are tolerated, and any
/// I/O failure yields the environment-only report.
///
/// When stdout (or stdin) is not a tty, no terminal I/O happens at all and the
/// environment-only report is returned.
#[must_use]
pub fn probe(timeout_per_query: Duration) -> CapabilityReport {
    let mut report = env_report();
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return report;
    }
    if let Some(raw) = probe_tty(timeout_per_query) {
        apply_replies(&mut report, &raw);
    }
    report
}

/// Fold the raw reply bytes into the report. Never fails; unknown bytes are
/// ignored.
fn apply_replies(report: &mut CapabilityReport, raw: &str) {
    // A kitty graphics APC reply (`\x1b_G…`) only comes back from a terminal
    // that speaks the protocol; the DA1 fence guarantees non-speakers answer
    // DA1 without ever emitting one.
    if raw.contains("\x1b_G") {
        report.kitty_graphics = true;
    }
    if let Some(da1) = parse_da1(raw) {
        if da1.attrs.contains(&4) {
            report.sixel = true;
        }
    }
    if parse_decrqm_2026(raw) == Some(SyncSupport::Supported) {
        report.sync_2026 = true;
    }
    if let Some(cell) = parse_cell_size(raw) {
        report.cell_px = Some(cell);
    }
}

/// Restores raw mode on drop, so every early return leaves the terminal clean.
struct RawGuard;

impl RawGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Drive the tty: raw mode on, spawn a bounded reader, write the queries plus a
/// DSR sentinel, collect the replies, restore raw mode. `None` on any I/O
/// failure (the caller then keeps the environment-only report).
fn probe_tty(timeout_per_query: Duration) -> Option<String> {
    let _raw = RawGuard::enter().ok()?;

    let (tx, rx) = mpsc::channel::<u8>();
    // The reader blocks on stdin byte-by-byte and self-terminates the instant it
    // sees the DSR sentinel's `R` (or the channel drops / EOF / an error), so it
    // never parks holding the tty once the sentinel round-trips.
    let reader = thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut byte = [0u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(byte[0]).is_err() || byte[0] == b'R' {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Order matters: the kitty graphics query is fenced by DA1 (a kitty reply
    // arrives before the DA1 reply); then synchronized-output and cell-size;
    // then the DSR sentinel that unblocks the reader.
    {
        let mut out = io::stdout();
        let write = out
            .write_all(b"\x1b_Gi=31,s=1,v=1,a=q;AAAA\x1b\\")
            .and_then(|()| out.write_all(b"\x1b[c"))
            .and_then(|()| out.write_all(b"\x1b[?2026$p"))
            .and_then(|()| out.write_all(b"\x1b[16t"))
            .and_then(|()| out.write_all(b"\x1b[6n"))
            .and_then(|()| out.flush());
        if write.is_err() {
            // The reader is parked on a read that will never be fed; drop the
            // receiver so a later send (if any) fails, and leave it to exit on
            // its own. Restore happens via the guard.
            return None;
        }
    }

    let overall = timeout_per_query
        .checked_mul(4)
        .unwrap_or(Duration::from_millis(600));
    let deadline = Instant::now() + overall;
    let mut buf: Vec<u8> = Vec::new();
    let mut saw_sentinel = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = (deadline - now).min(timeout_per_query);
        match rx.recv_timeout(wait) {
            Ok(b) => {
                buf.push(b);
                if b == b'R' {
                    saw_sentinel = true;
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }

    // If the sentinel round-tripped, the reader has already exited (or is about
    // to on the same byte); join it so nothing is left touching the tty. If it
    // did not — a terminal that answered nothing at all — do not block on a
    // parked read; drop the receiver and let the (harmless, idle) thread go.
    if saw_sentinel {
        let _ = reader.join();
    } else {
        drop(rx);
        drop(reader);
    }

    Some(String::from_utf8_lossy(&buf).into_owned())
}
