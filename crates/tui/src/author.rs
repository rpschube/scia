//! Scene-author mode (storyboard 2f): a split view over the running scene for
//! live source authoring.
//!
//! The layout is three regions: a **source pane** showing the active scene's
//! source file, the **live canvas** still rendering beside it, and the
//! **meter bridge** along the bottom — the very same debug-overlay panel
//! ([`crate::render::render_overlay`]), reused, not reimplemented. Author mode
//! *surfaces* the hot-reload pipeline; it never drives it: a broken source file
//! still leaves the last good scene running (the pipeline's guarantee), and this
//! pane simply shows the file, its watch/reload state, and any error in place.
//!
//! The model ([`AuthorMode`]) is pure and TTY-free, the sibling of the tuning
//! strip ([`crate::tuning`]) and expression overlay ([`crate::mapping_ui`]): it
//! holds the source lines on show, the scroll position, the watch/reload status,
//! and the current reload error (if any). The render loop opens it against a
//! [`SceneSource`] the scenes crate delivers (a `--scene-file` on disk, or a
//! built-in preset compiled into the binary), reads a live [`ReloadEvent`] into
//! it each time one arrives, and draws it once per frame.
//!
//! The pane is a **viewer, not an editor**: the user edits the file in their own
//! editor and the hot-reload watcher feeds changes back here. A path source is
//! re-read live so a reload shows the newly saved bytes — broken or not — with
//! the failing line highlighted and the validator's message alongside, plus a
//! cheap did-you-mean hint ([`did_you_mean`]) when the offending identifier is
//! within edit distance of the known vocabulary.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use scia_core::FeatureSnapshot;
use scia_scenes::{PresetError, PresetErrorKind, ReloadEvent, SceneSource};

use crate::palette;
use crate::render::{self, UiState};

/// The largest edit distance a did-you-mean hint is offered at. Beyond this a
/// suggestion is more likely noise than help, so none is shown.
pub const MAX_EDIT_DISTANCE: usize = 2;

/// How many lines a page scroll moves.
const PAGE: usize = 10;

/// The narrowest body that splits into a source pane plus a visible canvas
/// strip; below it the source pane takes the full width.
const SPLIT_MIN_WIDTH: u16 = 40;

// ---------------------------------------------------------------------------
// did-you-mean
// ---------------------------------------------------------------------------

/// The closest word in `vocab` to `word` within [`MAX_EDIT_DISTANCE`], or `None`
/// when the vocabulary is empty or nothing is that close. Ties resolve to the
/// smallest distance, then the earliest word in `vocab`.
///
/// Pure and cheap: a single Levenshtein pass per candidate, no allocation beyond
/// the two DP rows.
#[must_use]
pub fn did_you_mean<'v>(word: &str, vocab: &[&'v str]) -> Option<&'v str> {
    let mut best: Option<(usize, &'v str)> = None;
    for &cand in vocab {
        let d = levenshtein(word, cand);
        if d <= MAX_EDIT_DISTANCE && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, w)| w)
}

/// The Levenshtein edit distance between `a` and `b`, over Unicode scalar
/// values. Classic two-row dynamic program.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// The offending identifier a preset error names, if any — the seam did-you-mean
/// runs against. An unknown `[table]` key or an unknown feature name carries the
/// bare identifier directly; other error kinds carry no single identifier to
/// suggest an alternative for.
fn offending_identifier(kind: &PresetErrorKind) -> Option<&str> {
    match kind {
        PresetErrorKind::UnknownKey { key, .. } => Some(key),
        PresetErrorKind::UnknownFeature { name } => Some(name),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The watch/reload state shown in the source-pane header.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ReloadStatus {
    /// A built-in preset: read-only, not watched (there is no file to reload).
    Builtin,
    /// A file source is being watched; no reload has arrived yet.
    #[default]
    Watching,
    /// The last reload succeeded, in `ms` milliseconds.
    Reloaded {
        /// Milliseconds the read-and-validate took.
        ms: f32,
    },
    /// The last reload failed to validate (details in [`AuthorMode::error`]).
    Failed,
    /// The source file could not be read (missing or unreadable).
    Unreadable,
}

impl ReloadStatus {
    /// The short header label for this status.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Builtin => "read-only · not watched".to_string(),
            Self::Watching => "watching".to_string(),
            Self::Reloaded { ms } => format!("reloaded {ms:.0}ms"),
            Self::Failed => "reload failed".to_string(),
            Self::Unreadable => "unreadable".to_string(),
        }
    }
}

/// A reload error, resolved for inline display: the failing 1-based line (when
/// known), the validator's message, and a cheap did-you-mean hint (when the
/// offending identifier is within [`MAX_EDIT_DISTANCE`] of the vocabulary).
#[derive(Clone, Debug, PartialEq)]
pub struct SourceError {
    /// The 1-based source line the error is on, if the validator located one.
    pub line: Option<usize>,
    /// The validator's single-line message (the error kind, without the
    /// file:line:col prefix).
    pub message: String,
    /// A did-you-mean hint, e.g. `did you mean 'treble'?`, when cheap.
    pub hint: Option<String>,
}

impl SourceError {
    /// The one-line summary shown on the pane's status row: `line N: <message>`
    /// (the `line N:` prefix only when the line is known), plus ` — <hint>` when
    /// a did-you-mean hint is offered.
    #[must_use]
    pub fn summary(&self) -> String {
        let loc = self.line.map(|l| format!("line {l}: ")).unwrap_or_default();
        let hint = self
            .hint
            .as_ref()
            .map(|h| format!(" — {h}"))
            .unwrap_or_default();
        format!("{loc}{}{hint}", self.message)
    }
}

/// The scene-author model: the source lines on show, the scroll position, the
/// watch/reload status, and the current reload error.
///
/// Pure state driven by the input handler and read by the render loop, the
/// sibling of [`TuningStrip`](crate::tuning::TuningStrip) and
/// [`MappingUi`](crate::mapping_ui::MappingUi).
#[derive(Clone, Debug, Default)]
pub struct AuthorMode {
    /// Whether the mode is open.
    open: bool,
    /// The active source descriptor (path + kind + label), or `None` while
    /// closed.
    source: Option<SceneSource>,
    /// The source split into display lines.
    lines: Vec<String>,
    /// The index of the top visible line.
    scroll: usize,
    /// The watch/reload status.
    status: ReloadStatus,
    /// The current reload error, if the last reload failed.
    error: Option<SourceError>,
    /// The did-you-mean vocabulary: the expression signal names plus the active
    /// scene's parameter keys.
    vocab: Vec<String>,
}

impl AuthorMode {
    /// Whether the mode is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The source lines currently on show.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The index of the top visible line.
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The current watch/reload status.
    #[must_use]
    pub fn status(&self) -> &ReloadStatus {
        &self.status
    }

    /// The current reload error, if the last reload failed.
    #[must_use]
    pub fn error(&self) -> Option<&SourceError> {
        self.error.as_ref()
    }

    /// The header label of the active source, or an empty string while closed.
    #[must_use]
    pub fn label(&self) -> &str {
        self.source.as_ref().map_or("", |s| s.label.as_str())
    }

    /// Open the mode on `source`, loading its lines, with `vocab` (the signal
    /// names plus the scene's parameter keys) for did-you-mean hints. A file
    /// source is read live from its path; a built-in uses its embedded text.
    pub fn open(&mut self, source: SceneSource, vocab: Vec<String>) {
        self.vocab = vocab;
        self.scroll = 0;
        self.error = None;
        self.load(&source);
        self.source = Some(source);
        self.open = true;
    }

    /// Close the mode. The source and vocabulary are dropped; a reopen rebuilds
    /// them from the loop.
    pub fn close(&mut self) {
        self.open = false;
        self.source = None;
        self.lines.clear();
        self.error = None;
        self.scroll = 0;
    }

    /// Load the display lines from `source`, setting the status: a file source
    /// is read live (an unreadable file shows a clear inline line and the
    /// [`Unreadable`](ReloadStatus::Unreadable) status), a built-in uses its
    /// embedded text and reads [`Builtin`](ReloadStatus::Builtin).
    fn load(&mut self, source: &SceneSource) {
        match &source.path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => {
                    self.lines = split_lines(&text);
                    self.status = ReloadStatus::Watching;
                }
                Err(err) => {
                    self.lines = vec![format!("cannot read {}: {err}", source.label)];
                    self.status = ReloadStatus::Unreadable;
                }
            },
            None => {
                self.lines = split_lines(&source.text);
                self.status = ReloadStatus::Builtin;
            }
        }
    }

    /// Fold in a live [`ReloadEvent`]: re-read the (path) source so the pane
    /// shows the bytes just saved — broken or not — then set the status and,
    /// on failure, the resolved inline error (with a did-you-mean hint). A no-op
    /// while closed. The running scene is never touched here: a failed reload is
    /// the pipeline's business, and the last good frame holds; this only shows
    /// what happened.
    pub fn on_reload(&mut self, event: &ReloadEvent) {
        if !self.open {
            return;
        }
        if let Some(source) = self.source.take() {
            self.load(&source);
            self.source = Some(source);
        }
        match &event.result {
            Ok(_) => {
                self.status = ReloadStatus::Reloaded {
                    ms: event.elapsed_ms,
                };
                self.error = None;
            }
            Err(err) => {
                let detail = self.resolve_error(err);
                // Bring the failing line into view (top of the content), so the
                // highlight is never scrolled off.
                if let Some(line) = detail.line {
                    self.scroll = line.saturating_sub(1).min(self.max_scroll());
                }
                self.status = ReloadStatus::Failed;
                self.error = Some(detail);
            }
        }
    }

    /// Resolve a [`PresetError`] into an inline [`SourceError`]: its line, its
    /// message, and a did-you-mean hint when the offending identifier is within
    /// [`MAX_EDIT_DISTANCE`] of the vocabulary.
    fn resolve_error(&self, err: &PresetError) -> SourceError {
        let hint = offending_identifier(&err.kind).and_then(|ident| {
            let vocab: Vec<&str> = self.vocab.iter().map(String::as_str).collect();
            did_you_mean(ident, &vocab).map(|s| format!("did you mean '{s}'?"))
        });
        SourceError {
            line: err.line,
            message: err.kind.to_string(),
            hint,
        }
    }

    /// The largest valid scroll offset (the last line index).
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    /// Scroll down one line.
    pub fn scroll_down(&mut self) {
        self.scroll = (self.scroll + 1).min(self.max_scroll());
    }

    /// Scroll up one line.
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Scroll down one page.
    pub fn page_down(&mut self) {
        self.scroll = (self.scroll + PAGE).min(self.max_scroll());
    }

    /// Scroll up one page.
    pub fn page_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(PAGE);
    }
}

/// Split source text into display lines, keeping an empty file as a single
/// empty line so the pane always has a body to draw.
fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    text.lines().map(str::to_string).collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The source-pane width for a body of `width`: about three-fifths, leaving the
/// remainder for the live canvas. A body narrower than [`SPLIT_MIN_WIDTH`] is
/// too tight to split, so the pane takes it all.
fn pane_width(width: u16) -> u16 {
    if width < SPLIT_MIN_WIDTH {
        width
    } else {
        (u32::from(width) * 3 / 5) as u16
    }
}

/// Paint scene-author mode over the body: the meter bridge across the bottom
/// (the reused debug-overlay panel), and the source pane above it on the left,
/// leaving the live canvas visible on the right. Draws nothing when the mode is
/// closed or the body is degenerate.
pub fn draw_author(
    buf: &mut Buffer,
    body: Rect,
    author: &AuthorMode,
    snap: &FeatureSnapshot,
    ui: &UiState,
) {
    if !author.open || body.width == 0 || body.height == 0 {
        return;
    }
    // The meter bridge along the bottom — the same component the debug overlay
    // paints, reused verbatim. It draws over the bottom rows of the body.
    render::render_overlay(buf, body, snap, ui);
    let bridge_rows = if body.height >= render::OVERLAY_MIN_BODY {
        render::OVERLAY_ROWS
    } else {
        1
    };
    let pane_h = body.height.saturating_sub(bridge_rows);
    if pane_h == 0 {
        return;
    }
    let pane = Rect::new(body.x, body.y, pane_width(body.width), pane_h);
    render_source_pane(buf, pane, author);
}

/// Paint the source pane: a header row (label · kind · status), the scrolled
/// source with a line-number gutter and the failing line highlighted, and a
/// bottom status row carrying the inline error (with its did-you-mean hint) or
/// the scroll hint.
fn render_source_pane(buf: &mut Buffer, pane: Rect, author: &AuthorMode) {
    let fill = Style::new().bg(palette::OVERLAY_BG).fg(palette::OVERLAY_FG);
    let width = pane.width as usize;

    // Clear the pane so the scene beneath does not bleed through.
    for dy in 0..pane.height {
        for dx in 0..pane.width {
            if let Some(cell) = buf.cell_mut((pane.x + dx, pane.y + dy)) {
                cell.set_char(' ').set_style(fill);
            }
        }
    }

    // Header: source label, kind, and watch/reload status.
    let kind = author.source.as_ref().map(|s| s.kind.label()).unwrap_or("");
    let header = format!(
        " {} · {} · {} ",
        author.label(),
        kind,
        author.status.label()
    );
    buf.set_stringn(
        pane.x,
        pane.y,
        &header,
        width,
        fill.add_modifier(Modifier::BOLD),
    );
    if pane.height == 1 {
        return;
    }

    // Reserve the bottom row for the status/error line when there is room.
    let has_status = pane.height >= 3;
    let status_rows = u16::from(has_status);
    let content_h = pane.height.saturating_sub(1 + status_rows);
    let err_line = author.error.as_ref().and_then(|e| e.line);
    let gutter = gutter_width(author.lines.len());

    for row in 0..content_h {
        let idx = author.scroll + row as usize;
        let Some(line) = author.lines.get(idx) else {
            break;
        };
        let y = pane.y + 1 + row;
        let lineno = idx + 1;
        let is_err = err_line == Some(lineno);
        let text = format!("{lineno:>gutter$} │ {line}");
        let style = if is_err {
            fill.fg(palette::ERROR)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            fill
        };
        // Clear the row first so a highlighted (reversed) error line fills its
        // whole width, then lay the text over it.
        if is_err {
            for dx in 0..pane.width {
                if let Some(cell) = buf.cell_mut((pane.x + dx, y)) {
                    cell.set_char(' ').set_style(style);
                }
            }
        }
        buf.set_stringn(pane.x, y, &text, width, style);
    }

    if has_status {
        let y = pane.y + pane.height - 1;
        match &author.error {
            Some(err) => {
                let msg = format!(" {} ", err.summary());
                buf.set_stringn(
                    pane.x,
                    y,
                    &msg,
                    width,
                    fill.fg(palette::ERROR).add_modifier(Modifier::BOLD),
                );
            }
            None => {
                buf.set_stringn(
                    pane.x,
                    y,
                    " ↑↓ scroll · esc close",
                    width,
                    fill.add_modifier(Modifier::DIM),
                );
            }
        }
    }
}

/// The line-number gutter width for a file of `line_count` lines: the digit
/// count of the last line number, at least two.
fn gutter_width(line_count: usize) -> usize {
    let digits = line_count.max(1).to_string().len();
    digits.max(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scia_scenes::{PresetErrorKind, SourceKind};
    use std::time::Instant;

    fn file_source(path: &std::path::Path) -> SceneSource {
        SceneSource::from_file(path)
    }

    fn write_temp(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("scia-author-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write temp");
        path
    }

    fn vocab() -> Vec<String> {
        scia_scenes::expression_vocabulary()
            .iter()
            .map(|s| (*s).to_string())
            .chain(["gap".to_string(), "punch".to_string()])
            .collect()
    }

    fn preset_err(line: Option<usize>, kind: PresetErrorKind) -> PresetError {
        PresetError {
            file: None,
            line,
            col: None,
            kind,
        }
    }

    fn reload_ok(ms: f32) -> ReloadEvent {
        // A trivially valid preset stands in for a successful reload; only the
        // Ok/Err discriminant and elapsed_ms matter to author mode.
        let preset = scia_scenes::builtin_preset("spectra")
            .expect("spectra exists")
            .expect("spectra parses");
        ReloadEvent {
            result: Ok(preset),
            elapsed_ms: ms,
        }
    }

    fn reload_err(err: PresetError, ms: f32) -> ReloadEvent {
        ReloadEvent {
            result: Err(err),
            elapsed_ms: ms,
        }
    }

    // -- did-you-mean -----------------------------------------------------

    #[test]
    fn did_you_mean_hits_within_distance_two() {
        let vocab = ["bass", "mid", "treb", "loud", "onset"];
        // One or two edits away resolves to the intended word.
        assert_eq!(did_you_mean("bas", &vocab), Some("bass")); // one insertion
        assert_eq!(did_you_mean("onsett", &vocab), Some("onset")); // one deletion
        assert_eq!(did_you_mean("treeb", &vocab), Some("treb")); // one deletion
        assert_eq!(did_you_mean("onset", &vocab), Some("onset")); // exact
    }

    #[test]
    fn did_you_mean_misses_beyond_distance_two() {
        let vocab = ["bass", "mid", "treb", "loud", "onset"];
        // Nothing within edit distance 2 of a wholly unrelated word.
        assert_eq!(did_you_mean("saxophone", &vocab), None);
        assert_eq!(did_you_mean("xyz", &vocab), None);
    }

    #[test]
    fn did_you_mean_on_empty_vocab_is_none() {
        assert_eq!(did_you_mean("treb", &[]), None);
    }

    #[test]
    fn did_you_mean_prefers_the_closest_then_earliest() {
        // "mud" is distance 1 from both "mad" and "mid", distance 3 from "bass".
        // A tie at the smallest distance resolves to the earliest word, "mad".
        let vocab = ["bass", "mad", "mid"];
        assert_eq!(did_you_mean("mud", &vocab), Some("mad"));
    }

    // -- model ------------------------------------------------------------

    #[test]
    fn opens_a_builtin_read_only_and_closes() {
        let mut a = AuthorMode::default();
        assert!(!a.is_open());
        let source = SceneSource::builtin("spectra").expect("spectra builtin");
        a.open(source, vocab());
        assert!(a.is_open());
        assert_eq!(a.status(), &ReloadStatus::Builtin);
        assert!(!a.lines().is_empty(), "the embedded source loads");
        assert!(
            a.lines().iter().any(|l| l.contains("spectra")),
            "the builtin source shows: {:?}",
            a.lines()
        );
        a.close();
        assert!(!a.is_open());
        assert!(a.lines().is_empty());
    }

    #[test]
    fn opens_a_file_source_and_watches() {
        let path = write_temp(
            "watch.toml",
            "[preset]\nname = \"demo\"\nscene = \"spectra\"\n",
        );
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        assert!(a.is_open());
        assert_eq!(a.status(), &ReloadStatus::Watching);
        assert_eq!(a.lines().len(), 3, "three source lines: {:?}", a.lines());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_shows_a_clear_state_without_panicking() {
        let path =
            std::env::temp_dir().join(format!("scia-author-missing-{}.toml", std::process::id()));
        std::fs::remove_file(&path).ok();
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        assert!(a.is_open());
        assert_eq!(a.status(), &ReloadStatus::Unreadable);
        assert!(
            a.lines().iter().any(|l| l.contains("cannot read")),
            "a clear inline state shows: {:?}",
            a.lines()
        );
    }

    #[test]
    fn a_successful_reload_records_the_time() {
        let path = write_temp(
            "reload-ok.toml",
            "[preset]\nname = \"demo\"\nscene = \"spectra\"\n",
        );
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        a.on_reload(&reload_ok(38.0));
        assert_eq!(a.status(), &ReloadStatus::Reloaded { ms: 38.0 });
        assert!(a.error().is_none(), "a clean reload clears any error");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_failed_reload_surfaces_the_error_line_and_hint() {
        let path = write_temp(
            "reload-bad.toml",
            "[preset]\nname = \"demo\"\nscene = \"spectra\"\n[params]\npunchh = 0.5\n",
        );
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        let err = preset_err(
            Some(5),
            PresetErrorKind::UnknownKey {
                table: "params".to_string(),
                key: "punchh".to_string(),
                known: vec!["punch".to_string()],
            },
        );
        a.on_reload(&reload_err(err, 12.0));
        assert_eq!(a.status(), &ReloadStatus::Failed);
        let detail = a.error().expect("an error is surfaced");
        assert_eq!(detail.line, Some(5));
        assert!(
            detail.hint.as_deref() == Some("did you mean 'punch'?"),
            "a did-you-mean hint is offered: {:?}",
            detail.hint
        );
        assert!(
            detail.summary().contains("line 5"),
            "the summary locates the line: {}",
            detail.summary()
        );
        // The failing line is scrolled into view.
        assert!(a.scroll() <= 4, "the error line is not scrolled off");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_broken_reload_never_empties_the_pane() {
        // Surfacing a broken reload keeps showing the source (the last good scene
        // keeps running in the pipeline; author mode only reports the error).
        let path = write_temp(
            "hold.toml",
            "[preset]\nname = \"demo\"\nscene = \"spectra\"\n",
        );
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        let before = a.lines().len();
        let err = preset_err(
            Some(2),
            PresetErrorKind::Syntax("expected a value".to_string()),
        );
        a.on_reload(&reload_err(err, 3.0));
        assert!(!a.lines().is_empty(), "the pane still shows the source");
        assert_eq!(
            a.lines().len(),
            before,
            "the source is re-read, not dropped"
        );
        // A syntax error carries no identifier, so no did-you-mean hint.
        assert!(a.error().unwrap().hint.is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn scrolling_clamps_at_both_ends() {
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let path = write_temp("scroll.toml", &body);
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        assert_eq!(a.scroll(), 0);
        a.scroll_up();
        assert_eq!(a.scroll(), 0, "cannot scroll above the top");
        a.page_down();
        assert_eq!(a.scroll(), 10);
        for _ in 0..50 {
            a.scroll_down();
        }
        assert_eq!(a.scroll(), a.lines().len() - 1, "clamps at the last line");
        std::fs::remove_file(&path).ok();
    }

    // -- render -----------------------------------------------------------

    /// Concatenate every glyph the buffer holds into one string.
    fn buffer_text(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn author_ui(author: AuthorMode) -> UiState {
        UiState {
            author,
            fps_measured: 60.0,
            ..UiState::default()
        }
    }

    #[test]
    fn draw_shows_the_source_header_and_bridge() {
        let source = SceneSource::builtin("spectra").expect("spectra builtin");
        let mut a = AuthorMode::default();
        a.open(source, vocab());
        let ui = author_ui(a);
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        draw_author(&mut buf, area, &ui.author, &FeatureSnapshot::default(), &ui);
        let text = buffer_text(&buf);
        assert!(text.contains("spectra"), "the source label shows");
        assert!(text.contains("toml"), "the source kind shows");
        assert!(text.contains("read-only"), "the watch status shows");
        // The meter bridge (debug overlay panel) shows fps and signals.
        assert!(text.contains("fps"), "the meter bridge shows fps");
        assert!(text.contains("bass"), "the meter bridge shows live signals");
    }

    #[test]
    fn draw_highlights_the_error_line_and_message() {
        let path = write_temp(
            "draw-err.toml",
            "[preset]\nname = \"demo\"\nscene = \"spectra\"\n[params]\npunchh = 0.5\n",
        );
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        let err = preset_err(
            Some(5),
            PresetErrorKind::UnknownKey {
                table: "params".to_string(),
                key: "punchh".to_string(),
                known: vec!["punch".to_string()],
            },
        );
        a.on_reload(&reload_err(err, 9.0));
        let ui = author_ui(a);
        // A wide body so the whole inline error line (message plus hint) fits the
        // source pane's status row rather than truncating.
        let area = Rect::new(0, 0, 160, 30);
        let mut buf = Buffer::empty(area);
        draw_author(&mut buf, area, &ui.author, &FeatureSnapshot::default(), &ui);
        let text = buffer_text(&buf);
        assert!(text.contains("punchh"), "the failing source line shows");
        assert!(
            text.contains("did you mean 'punch'?"),
            "the hint shows: {text:?}"
        );
        assert!(text.contains("reload failed"), "the failed status shows");
    }

    #[test]
    fn draw_degrades_on_a_tiny_body_without_panicking() {
        let source = SceneSource::builtin("spectra").expect("spectra builtin");
        let mut a = AuthorMode::default();
        a.open(source, vocab());
        let ui = author_ui(a);
        for (w, h) in [(1u16, 1u16), (3, 2), (20, 4), (40, 6)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            // Must not panic at any degenerate size.
            draw_author(&mut buf, area, &ui.author, &FeatureSnapshot::default(), &ui);
        }
    }

    #[test]
    fn source_kind_is_inferred_from_the_extension() {
        assert_eq!(
            SceneSource::from_file(std::path::Path::new("/x/scene.lua")).kind,
            SourceKind::Lua
        );
        assert_eq!(
            SceneSource::from_file(std::path::Path::new("/x/scene.toml")).kind,
            SourceKind::Toml
        );
        assert_eq!(
            SceneSource::from_file(std::path::Path::new("/x/scene")).kind,
            SourceKind::Toml
        );
    }

    #[test]
    fn draw_cost_under_frame_budget() {
        // Author mode's own per-frame draw cost must stay well under the 60 fps
        // frame budget (16.667 ms). We draw a full-size pane over a large source
        // many times and assert the mean per-frame cost is a small fraction of
        // the budget. Generous bound for CI.
        let body: String = (1..=400)
            .map(|i| format!("line {i} = some source text\n"))
            .collect();
        let path = write_temp("budget.toml", &body);
        let mut a = AuthorMode::default();
        a.open(file_source(&path), vocab());
        let ui = author_ui(a);
        let (w, h) = (120u16, 40u16);
        let area = Rect::new(0, 0, w, h);
        let snap = FeatureSnapshot::default();

        let mut buf = Buffer::empty(area);
        for _ in 0..30 {
            draw_author(&mut buf, area, &ui.author, &snap, &ui);
        }
        let n = 300;
        let t0 = Instant::now();
        for _ in 0..n {
            draw_author(&mut buf, area, &ui.author, &snap, &ui);
        }
        let per_frame_ms = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
        println!("author draw @ {w}x{h}: {per_frame_ms:.4} ms/frame (budget 16.667 ms)");
        // A loaded shared CI runner has measured ~2.9 ms for this draw; the
        // bound guards the frame budget, not a wall-clock ideal, so it uses the
        // scripted-scene budget test's margin (half the 16.667 ms budget).
        assert!(
            per_frame_ms < 8.0,
            "author draw {per_frame_ms:.4} ms/frame should be < 8.0 ms (under the frame budget)"
        );
        std::fs::remove_file(&path).ok();
    }
}
