//! The expression-mapping overlay (storyboard 2e): a panel listing the running
//! scene's layer-0 `[map]` rows as `target ← expression`, each with a live
//! sparkline of the source signal it is wired to, and an inline line-editor that
//! previews an edited expression the instant it compiles.
//!
//! The model ([`MappingUi`]) is pure and TTY-free, the sibling of the tuning
//! strip ([`crate::tuning`]): it holds the rows on show (rebuilt each time the
//! overlay opens from the presenter's layer-0 mappings), the selection, the
//! inline-edit state, and the set of rows the user committed this session. The
//! render loop samples every row's source once per frame from the live
//! [`FeatureSnapshot`] while the overlay is open, drains a one-shot [`MapEntry`]
//! the model wants live and swaps it into the presenter's layer-0
//! [`MappingSet`](scia_scenes::MappingSet), and — on the write key — hands the
//! committed rows to the `[map]` write-back helpers in [`crate::tuning`].
//!
//! Editing model — rows edit as **expression** form. A row that is currently
//! table-form is shown as a readable `source [curve …]` description
//! ([`table_display`]); it is converted to an expression only when the user
//! actually edits it, and a table row the user never touches is never rewritten
//! (write-back leaves it byte-for-byte in table form). Opening an edit on a
//! table row seeds the buffer with the row's **source signal** as a bare
//! expression (e.g. `onset`): an expression has no envelope follower, so the
//! curve/scale/offset/attack/decay a table row carries cannot be reproduced
//! exactly, and the user re-expresses the mapping from its signal rather than
//! from a lossy auto-conversion.
//!
//! A broken edit never disturbs the running scene: while the draft fails to
//! compile the last valid mapping keeps running and the parse error shows
//! inline; `⏎` commits a compiling draft (dirty-flagging it), `esc` reverts to
//! the mapping that was live before the edit began.
//!
//! Known vocabulary quirk (preserved, not fixed here): `beat` is
//! `beat_confidence` in a table `[map]` entry but `beat_phase` in an expression.
//! The overlay speaks the expression vocabulary, so a row's `beat` sparkline
//! samples `beat_phase`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use scia_core::FeatureSnapshot;
use scia_scenes::{Curve, ExprMapping, Feature, MapEntry, Mapping};

use crate::palette;

/// The number of samples each row's sparkline ring retains (~1 s at 60 fps).
const SPARK_CAP: usize = 60;

/// The eight block glyphs a sparkline sample maps to, lowest to highest.
const SPARK_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The storyboard signal vocabulary shown on the overlay's hint line.
const VOCAB_HINT: &str = "bass mid treb loud onset beat width";

// ---------------------------------------------------------------------------
// Source signals + sparklines
// ---------------------------------------------------------------------------

/// A feature scalar a mapping row is wired to, sampled once per frame for its
/// sparkline. Covers every [`Feature`] a table row can name; an expression row
/// resolves to its primary vocabulary signal ([`SourceSignal::from_expr`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceSignal {
    /// Bass band level (`bands[0]`).
    Bass,
    /// Mid band level (`bands[1]`).
    Mid,
    /// Treble band level (`bands[2]`).
    Treb,
    /// Loudness (`rms`).
    Loud,
    /// Peak sample of the hop (`peak`).
    Peak,
    /// Onset gate: `1.0` on an onset hop, else `0.0`.
    Onset,
    /// Spectral flux.
    Flux,
    /// Beat phase (expression-form `beat`; see the module quirk note).
    Beat,
    /// Stereo width.
    Width,
}

impl SourceSignal {
    /// The signal's vocabulary name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bass => "bass",
            Self::Mid => "mid",
            Self::Treb => "treb",
            Self::Loud => "loud",
            Self::Peak => "peak",
            Self::Onset => "onset",
            Self::Flux => "flux",
            Self::Beat => "beat",
            Self::Width => "width",
        }
    }

    /// The source signal a table row is wired to (its `[map]` feature).
    #[must_use]
    pub fn from_feature(feature: Feature) -> Self {
        match feature {
            Feature::Bass => Self::Bass,
            Feature::Mid => Self::Mid,
            Feature::Treb => Self::Treb,
            Feature::Loud => Self::Loud,
            Feature::Peak => Self::Peak,
            Feature::Onset => Self::Onset,
            Feature::Flux => Self::Flux,
            Feature::Beat => Self::Beat,
            Feature::Width => Self::Width,
        }
    }

    /// The primary source signal an expression row is wired to: the first
    /// vocabulary name that appears as a whole word in `src`, scanned in a fixed
    /// priority order (event signals first). Falls back to [`SourceSignal::Loud`]
    /// when the expression names none of them (e.g. a bare constant).
    #[must_use]
    pub fn from_expr(src: &str) -> Self {
        const DETECT: [SourceSignal; 9] = [
            SourceSignal::Onset,
            SourceSignal::Beat,
            SourceSignal::Bass,
            SourceSignal::Mid,
            SourceSignal::Treb,
            SourceSignal::Loud,
            SourceSignal::Peak,
            SourceSignal::Flux,
            SourceSignal::Width,
        ];
        DETECT
            .into_iter()
            .find(|s| word_present(src, s.name()))
            .unwrap_or(Self::Loud)
    }

    /// Sample the signal from `snap`, clamped to `0.0..=1.0` (the range a
    /// sparkline plots). Mirrors the runtime feature reads; `beat` samples
    /// `beat_phase`, the expression-form reading.
    #[must_use]
    pub fn sample(self, snap: &FeatureSnapshot) -> f32 {
        let clamp = |x: f32| x.clamp(0.0, 1.0);
        match self {
            Self::Bass => clamp(snap.bands[0]),
            Self::Mid => clamp(snap.bands[1]),
            Self::Treb => clamp(snap.bands[2]),
            Self::Loud => clamp(snap.rms),
            Self::Peak => clamp(snap.peak),
            Self::Onset => {
                if snap.onset {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Flux => clamp(snap.flux),
            Self::Beat => clamp(snap.beat_phase),
            Self::Width => ((1.0 - snap.stereo_correlation) / 2.0).clamp(0.0, 1.0),
        }
    }
}

/// Whether `name` appears in `src` as a whole word — not a substring of a longer
/// identifier — so `beat` does not match inside `beat_conf`.
fn word_present(src: &str, name: &str) -> bool {
    let bytes = src.as_bytes();
    let mut from = 0;
    while let Some(rel) = src[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Whether `b` is an identifier byte (so an adjacent one means `name` was only a
/// substring).
fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// A fixed-capacity ring of the most recent sparkline samples, oldest to newest.
#[derive(Clone, Debug)]
struct Ring {
    buf: Vec<f32>,
    next: usize,
    filled: usize,
}

impl Ring {
    /// A ring pre-allocated for `cap` samples, holding none.
    fn new(cap: usize) -> Self {
        Self {
            buf: vec![0.0; cap.max(1)],
            next: 0,
            filled: 0,
        }
    }

    /// Push the newest sample, overwriting the oldest once full. Allocation-free.
    fn push(&mut self, v: f32) {
        let cap = self.buf.len();
        self.buf[self.next] = v;
        self.next = (self.next + 1) % cap;
        self.filled = (self.filled + 1).min(cap);
    }

    /// The retained samples oldest-to-newest.
    fn samples(&self) -> Vec<f32> {
        let cap = self.buf.len();
        let start = (self.next + cap - self.filled) % cap;
        (0..self.filled)
            .map(|i| self.buf[(start + i) % cap])
            .collect()
    }
}

/// The glyph for one sparkline sample in `0.0..=1.0`.
fn spark_glyph(v: f32) -> char {
    let idx = (v.clamp(0.0, 1.0) * (SPARK_BARS.len() - 1) as f32).round() as usize;
    SPARK_BARS[idx]
}

/// Render `ring`'s most recent samples into a `width`-cell sparkline, newest at
/// the right, left-padded with spaces when there are fewer samples than cells.
fn sparkline(ring: &Ring, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let samples = ring.samples();
    let take = samples.len().min(width);
    let start = samples.len() - take;
    let mut out = String::with_capacity(width);
    for _ in 0..(width - take) {
        out.push(' ');
    }
    for &v in &samples[start..] {
        out.push(spark_glyph(v));
    }
    out
}

// ---------------------------------------------------------------------------
// Table-row display text
// ---------------------------------------------------------------------------

/// The `f32` rendered via its shortest decimal string, so `0.9f32` shows as `0.9`
/// rather than the widened `0.899999…`.
fn clean_num(v: f32) -> String {
    format!("{v}")
}

/// The readable one-line description of a table-form `[map]` row:
/// `<source>[ [curve …]][ ×scale][ +offset][ ~attack/decay ms]`, omitting each
/// clause at its default (linear curve, unit scale, zero offset, zero envelope).
/// This is the display text only; a table row is converted to an actual
/// expression solely when the user edits it (see the module docs).
#[must_use]
pub fn table_display(m: &Mapping) -> String {
    let mut s = SourceSignal::from_feature(m.feature).name().to_string();
    match m.curve {
        Curve::Linear => {}
        Curve::Pow { exponent } => s.push_str(&format!(" [pow {}]", clean_num(exponent))),
        Curve::Log => s.push_str(" [log]"),
        Curve::Step { threshold } => s.push_str(&format!(" [step {}]", clean_num(threshold))),
    }
    if m.scale != 1.0 {
        s.push_str(&format!(" ×{}", clean_num(m.scale)));
    }
    if m.offset != 0.0 {
        if m.offset > 0.0 {
            s.push_str(&format!(" +{}", clean_num(m.offset)));
        } else {
            s.push_str(&format!(" {}", clean_num(m.offset)));
        }
    }
    if m.attack_ms > 0.0 || m.decay_ms > 0.0 {
        s.push_str(&format!(
            " ~{}/{}ms",
            clean_num(m.attack_ms),
            clean_num(m.decay_ms)
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One row of the overlay: a layer-0 mapping, its committed display text, the
/// source signal driving its sparkline, and the sample ring.
#[derive(Clone, Debug)]
struct Row {
    /// The parameter the mapping drives.
    target: String,
    /// The source signal the sparkline plots.
    source: SourceSignal,
    /// The committed display text: the expression source, or the table
    /// description ([`table_display`]).
    text: String,
    /// Whether the committed mapping is expression form.
    is_expr: bool,
    /// The committed live mapping (the pre-edit state `esc` reverts to).
    entry: MapEntry,
    /// The sparkline sample ring.
    spark: Ring,
}

impl Row {
    fn from_entry(entry: MapEntry) -> Self {
        let target = entry.target().to_string();
        let (text, source, is_expr) = match &entry {
            MapEntry::Expr(e) => (
                e.source().to_string(),
                SourceSignal::from_expr(e.source()),
                true,
            ),
            MapEntry::Table(m) => (
                table_display(m),
                SourceSignal::from_feature(m.feature),
                false,
            ),
        };
        Self {
            target,
            source,
            text,
            is_expr,
            entry,
            spark: Ring::new(SPARK_CAP),
        }
    }
}

/// The in-progress inline edit of the selected row.
#[derive(Clone, Debug)]
struct Edit {
    /// The draft expression text.
    buf: String,
    /// The cursor position, as a character index in `0..=buf.chars().count()`.
    cursor: usize,
    /// The current parse error (short form), or `None` while the draft compiles.
    error: Option<String>,
    /// The mapping that was live before the edit began, restored on `esc`.
    original: MapEntry,
}

/// The expression-mapping overlay model: the rows on show, the selection, the
/// inline edit (if any), and the rows committed this session.
///
/// Pure state driven by the input handler and read by the render loop, the
/// sibling of [`TuningStrip`](crate::tuning::TuningStrip). The `dirty` set
/// persists across close/reopen so a write covers the whole session.
#[derive(Clone, Debug, Default)]
pub struct MappingUi {
    /// The mapping rows on show (rebuilt each time the overlay opens).
    rows: Vec<Row>,
    /// The selected row index.
    selected: usize,
    /// Whether the overlay is open.
    open: bool,
    /// The in-progress inline edit, when editing the selected row.
    editing: Option<Edit>,
    /// Targets committed dirty this session, in first-commit order.
    dirty: Vec<String>,
    /// A one-shot mapping the render loop should swap into the live set (a live
    /// preview, a commit, or an `esc` revert). Drained by [`drain_apply`].
    ///
    /// [`drain_apply`]: MappingUi::drain_apply
    apply: Option<MapEntry>,
}

impl MappingUi {
    /// Whether the overlay is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether an inline edit is in progress.
    #[must_use]
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// The selected row index.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The number of rows on show.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The target parameter of row `i`, if it exists.
    #[must_use]
    pub fn row_target(&self, i: usize) -> Option<&str> {
        self.rows.get(i).map(|r| r.target.as_str())
    }

    /// The committed display text of row `i` (expression source or table
    /// description), if it exists.
    #[must_use]
    pub fn row_text(&self, i: usize) -> Option<&str> {
        self.rows.get(i).map(|r| r.text.as_str())
    }

    /// The source-signal name of row `i`, if it exists.
    #[must_use]
    pub fn row_source(&self, i: usize) -> Option<&'static str> {
        self.rows.get(i).map(|r| r.source.name())
    }

    /// Whether row `i` is committed as expression form.
    #[must_use]
    pub fn row_is_expr(&self, i: usize) -> Option<bool> {
        self.rows.get(i).map(|r| r.is_expr)
    }

    /// Whether `target` has been committed dirty this session.
    #[must_use]
    pub fn is_dirty(&self, target: &str) -> bool {
        self.dirty.iter().any(|t| t == target)
    }

    /// The current edit buffer, if an edit is in progress.
    #[must_use]
    pub fn edit_buffer(&self) -> Option<&str> {
        self.editing.as_ref().map(|e| e.buf.as_str())
    }

    /// The current edit cursor (character index), if an edit is in progress.
    #[must_use]
    pub fn edit_cursor(&self) -> Option<usize> {
        self.editing.as_ref().map(|e| e.cursor)
    }

    /// The current inline parse error, if an edit is in progress and its draft
    /// does not compile.
    #[must_use]
    pub fn edit_error(&self) -> Option<&str> {
        self.editing.as_ref().and_then(|e| e.error.as_deref())
    }

    /// Open the overlay on `entries` (the presenter's layer-0 mappings). Rebuilds
    /// the rows with fresh (empty) sparkline rings; the `dirty` set is kept so a
    /// reopened session still writes its edits. Opening on an empty list is a
    /// no-op — there is nothing to map.
    pub fn open(&mut self, entries: Vec<MapEntry>) {
        self.rows = entries.into_iter().map(Row::from_entry).collect();
        self.selected = 0;
        self.editing = None;
        self.apply = None;
        self.open = !self.rows.is_empty();
    }

    /// Rebuild the rows from `entries` after the running preset changed under an
    /// open overlay (a live reload or scene swap), clamping the selection,
    /// cancelling any edit, and clearing the dirty set (the target file changed).
    /// A no-op when the overlay is closed; closes it when the new preset has no
    /// mappings.
    pub fn on_preset_swap(&mut self, entries: Vec<MapEntry>) {
        if !self.open {
            return;
        }
        self.rows = entries.into_iter().map(Row::from_entry).collect();
        self.editing = None;
        self.dirty.clear();
        self.apply = None;
        if self.rows.is_empty() {
            self.open = false;
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
    }

    /// Close the overlay. A half-finished edit reverts to its pre-edit mapping
    /// (so a broken draft never lingers). The dirty set is kept.
    pub fn close(&mut self) {
        if let Some(edit) = self.editing.take() {
            self.apply = Some(edit.original);
        }
        self.open = false;
    }

    /// Sample every row's source signal from `snap` into its sparkline ring. The
    /// render loop calls this once per frame while the overlay is open.
    pub fn sample(&mut self, snap: &FeatureSnapshot) {
        for row in &mut self.rows {
            let v = row.source.sample(snap);
            row.spark.push(v);
        }
    }

    /// Take the one-shot mapping the loop should swap into the live set, if any.
    pub fn drain_apply(&mut self) -> Option<MapEntry> {
        self.apply.take()
    }

    /// Move the selection to the next row, wrapping. Inert while editing.
    pub fn select_next(&mut self) {
        if self.editing.is_some() || self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
    }

    /// Move the selection to the previous row, wrapping. Inert while editing.
    pub fn select_prev(&mut self) {
        if self.editing.is_some() || self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + self.rows.len() - 1) % self.rows.len();
    }

    /// Begin editing the selected row. The buffer seeds from the expression
    /// source for an expression row, or the row's source signal name for a table
    /// row (converting it to an expression). Previews the seeded draft at once.
    pub fn begin_edit(&mut self) {
        if self.editing.is_some() {
            return;
        }
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let buf = if row.is_expr {
            row.text.clone()
        } else {
            row.source.name().to_string()
        };
        let cursor = buf.chars().count();
        self.editing = Some(Edit {
            buf,
            cursor,
            error: None,
            original: row.entry.clone(),
        });
        self.reparse();
    }

    /// Insert `c` at the cursor and re-preview. Control characters are ignored.
    pub fn insert_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        {
            let Some(edit) = self.editing.as_mut() else {
                return;
            };
            let at = byte_index(&edit.buf, edit.cursor);
            edit.buf.insert(at, c);
            edit.cursor += 1;
        }
        self.reparse();
    }

    /// Delete the character before the cursor and re-preview.
    pub fn backspace(&mut self) {
        {
            let Some(edit) = self.editing.as_mut() else {
                return;
            };
            if edit.cursor == 0 {
                return;
            }
            let at = byte_index(&edit.buf, edit.cursor - 1);
            edit.buf.remove(at);
            edit.cursor -= 1;
        }
        self.reparse();
    }

    /// Move the cursor one character left.
    pub fn cursor_left(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            edit.cursor = edit.cursor.saturating_sub(1);
        }
    }

    /// Move the cursor one character right.
    pub fn cursor_right(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            let len = edit.buf.chars().count();
            edit.cursor = (edit.cursor + 1).min(len);
        }
    }

    /// Compile the draft against the selected row's target. A valid draft becomes
    /// the previewed live mapping (dropped into `apply`); an invalid one leaves
    /// the last valid mapping running untouched and records the error inline.
    fn reparse(&mut self) {
        let (buf, target) = match (self.editing.as_ref(), self.rows.get(self.selected)) {
            (Some(edit), Some(row)) => (edit.buf.clone(), row.target.clone()),
            _ => return,
        };
        match ExprMapping::compile(&target, &buf) {
            Ok(mapping) => {
                if let Some(edit) = self.editing.as_mut() {
                    edit.error = None;
                }
                self.apply = Some(MapEntry::Expr(mapping));
            }
            Err(err) => {
                if let Some(edit) = self.editing.as_mut() {
                    edit.error = Some(err.message().to_string());
                }
            }
        }
    }

    /// Commit the current draft: on a compiling draft, keep it applied, update
    /// the row to the new expression, dirty-flag the target, and leave edit mode;
    /// on a non-compiling draft, stay in edit mode with the error shown (an
    /// invalid mapping is never committed).
    pub fn commit_edit(&mut self) {
        let buf = match self.editing.as_ref() {
            Some(edit) => edit.buf.clone(),
            None => return,
        };
        let Some(target) = self.rows.get(self.selected).map(|r| r.target.clone()) else {
            self.editing = None;
            return;
        };
        match ExprMapping::compile(&target, &buf) {
            Ok(mapping) => {
                let entry = MapEntry::Expr(mapping);
                if let Some(row) = self.rows.get_mut(self.selected) {
                    row.entry = entry.clone();
                    row.text = buf.clone();
                    row.is_expr = true;
                    row.source = SourceSignal::from_expr(&buf);
                }
                self.mark_dirty(&target);
                self.apply = Some(entry);
                self.editing = None;
            }
            Err(err) => {
                if let Some(edit) = self.editing.as_mut() {
                    edit.error = Some(err.message().to_string());
                }
            }
        }
    }

    /// Cancel the edit, reverting the live mapping to the pre-edit state.
    pub fn cancel_edit(&mut self) {
        if let Some(edit) = self.editing.take() {
            self.apply = Some(edit.original);
        }
    }

    /// The committed dirty rows as `(target, expression)` pairs, for write-back.
    /// Every committed row is expression form, so its display text is its
    /// expression source.
    #[must_use]
    pub fn dirty_edits(&self) -> Vec<(&str, &str)> {
        self.dirty
            .iter()
            .filter_map(|target| {
                self.rows
                    .iter()
                    .find(|r| &r.target == target)
                    .map(|r| (r.target.as_str(), r.text.as_str()))
            })
            .collect()
    }

    /// Record `target` in the dirty set, keeping first-commit order.
    fn mark_dirty(&mut self, target: &str) {
        if !self.dirty.iter().any(|t| t == target) {
            self.dirty.push(target.to_string());
        }
    }
}

/// The byte offset of character index `idx` in `s` (its end when `idx` is at or
/// past the last character).
fn byte_index(s: &str, idx: usize) -> usize {
    s.char_indices()
        .nth(idx)
        .map_or_else(|| s.len(), |(b, _)| b)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The narrowest body that still shows the sparkline column; below it the rows
/// degrade to `target ← text` alone.
const SPARK_MIN_WIDTH: u16 = 44;

/// The sparkline column width (cells) when the pane is wide enough for it.
const SPARK_WIDTH: usize = 16;

/// Paint the expression-mapping overlay over the bottom of the body, a framed
/// panel like the other overlays: a title row, one row per layer-0 mapping
/// (`*target ← text   ▁▂▃ [source]`, the selected row highlighted, the editing
/// row showing its draft and a cursor), and a bottom hint row (the signal
/// vocabulary, or the inline parse error while editing). Draws nothing when the
/// overlay is closed, empty, or the body is degenerate; on a short or narrow
/// pane it degrades gracefully.
pub fn draw_mapping(buf: &mut Buffer, body: Rect, ui: &MappingUi) {
    if !ui.open || body.width == 0 || body.height == 0 || ui.rows.is_empty() {
        return;
    }

    let width = body.width as usize;
    let fill = Style::new().bg(palette::OVERLAY_BG).fg(palette::OVERLAY_FG);

    // A pane too short for even a title + one row + hint degrades to one line.
    if body.height < 3 {
        let y = body.y + body.height - 1;
        clear_row(buf, body, y, fill);
        buf.set_stringn(
            body.x,
            y,
            " expression map — esc closes ",
            width,
            fill.add_modifier(Modifier::BOLD),
        );
        return;
    }

    // Height: a title, every row (capped to what fits), and a hint.
    let max_rows = (body.height as usize).saturating_sub(2);
    let shown = ui.rows.len().min(max_rows.max(1));
    let panel_h = (shown as u16) + 2;
    let y0 = body.y + body.height - panel_h;

    for dy in 0..panel_h {
        clear_row(buf, body, y0 + dy, fill);
    }

    buf.set_stringn(
        body.x,
        y0,
        "expression map",
        width,
        fill.add_modifier(Modifier::BOLD),
    );

    for (i, row) in ui.rows.iter().take(shown).enumerate() {
        let y = y0 + 1 + i as u16;
        let selected = i == ui.selected;
        let editing = selected && ui.editing.is_some();
        draw_row(buf, body, y, width, fill, row, selected, editing, ui);
    }

    // Hint row: the parse error while editing an invalid draft, the edit keys
    // while editing a valid one, else the signal vocabulary and nav keys.
    let hint_y = y0 + panel_h - 1;
    let hint = match ui.editing.as_ref() {
        Some(edit) => match &edit.error {
            Some(err) => format!("err: {err}"),
            None => "⏎ apply · esc cancel".to_string(),
        },
        None => format!("signals: {VOCAB_HINT} · ↑↓ row · ⏎ edit · [w] write · esc done"),
    };
    buf.set_stringn(
        body.x,
        hint_y,
        &hint,
        width,
        fill.add_modifier(Modifier::DIM),
    );
}

/// Clear one overlay row to the fill style so the scene beneath does not bleed.
fn clear_row(buf: &mut Buffer, body: Rect, y: u16, fill: Style) {
    for dx in 0..body.width {
        if let Some(cell) = buf.cell_mut((body.x + dx, y)) {
            cell.set_char(' ').set_style(fill);
        }
    }
}

/// Paint one mapping row.
#[allow(clippy::too_many_arguments)]
fn draw_row(
    buf: &mut Buffer,
    body: Rect,
    y: u16,
    width: usize,
    fill: Style,
    row: &Row,
    selected: bool,
    editing: bool,
    ui: &MappingUi,
) {
    let style = if selected {
        fill.add_modifier(Modifier::REVERSED)
    } else {
        fill
    };
    let mark = if ui.is_dirty(&row.target) { '*' } else { ' ' };

    // The right-hand sparkline column (`▁▂▃ [source]`), when the pane is wide.
    let show_spark = body.width >= SPARK_MIN_WIDTH;
    let right = if show_spark {
        format!(
            "{} [{}]",
            sparkline(&row.spark, SPARK_WIDTH),
            row.source.name()
        )
    } else {
        String::new()
    };
    let right_w = right.chars().count();

    // The left part: `*target ← text`, with the editing row showing its draft
    // and a cursor caret.
    let body_text = if editing {
        edit_line(ui)
    } else {
        row.text.clone()
    };
    let left = format!("{mark}{} ← {body_text}", row.target);

    // Lay the left part out, then the right column flush right if it fits.
    let avail = width.saturating_sub(right_w + 1);
    buf.set_stringn(body.x, y, &left, avail.max(1), style);
    if show_spark && right_w < width {
        let rx = body.x + (width - right_w) as u16;
        buf.set_stringn(rx, y, &right, right_w, style);
    }
}

/// The editing row's body text: the draft with a thin cursor caret at the cursor
/// position.
fn edit_line(ui: &MappingUi) -> String {
    let Some(edit) = ui.editing.as_ref() else {
        return String::new();
    };
    let at = byte_index(&edit.buf, edit.cursor);
    let mut s = edit.buf.clone();
    s.insert(at, '▏');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(target: &str, src: &str) -> MapEntry {
        MapEntry::Expr(ExprMapping::compile(target, src).expect("compiles"))
    }

    fn table(target: &str, feature: Feature, curve: Curve) -> MapEntry {
        MapEntry::Table(Mapping {
            target: target.to_string(),
            feature,
            curve,
            attack_ms: 0.0,
            decay_ms: 0.0,
            scale: 1.0,
            offset: 0.0,
        })
    }

    // -- source signals + sparklines --------------------------------------

    #[test]
    fn expression_source_binds_to_its_primary_signal() {
        assert_eq!(
            SourceSignal::from_expr("onset * 0.7 + bass * 0.2"),
            SourceSignal::Onset
        );
        assert_eq!(SourceSignal::from_expr("bass * 0.5"), SourceSignal::Bass);
        // A bare constant names no signal: falls back to loud.
        assert_eq!(SourceSignal::from_expr("0.5"), SourceSignal::Loud);
        // `beat` must not be found inside `beat_conf`.
        assert_eq!(SourceSignal::from_expr("beat_conf"), SourceSignal::Loud);
        assert_eq!(SourceSignal::from_expr("beat * 2"), SourceSignal::Beat);
    }

    #[test]
    fn table_source_binds_to_its_feature() {
        let e = table("gap", Feature::Treb, Curve::Linear);
        let row = Row::from_entry(e);
        assert_eq!(row.source, SourceSignal::Treb);
    }

    #[test]
    fn sparkline_renders_a_known_series() {
        let mut ring = Ring::new(SPARK_CAP);
        // Rising ramp 0, 0.5, 1.0 -> lowest, middle, highest glyphs.
        for v in [0.0, 0.5, 1.0] {
            ring.push(v);
        }
        // Width 3 with exactly three samples: no padding. 0.5 rounds to the
        // fifth of eight glyphs.
        assert_eq!(sparkline(&ring, 3), "▁▅█");
        // Wider than the sample count: left-padded with spaces, newest at right.
        assert_eq!(sparkline(&ring, 5), "  ▁▅█");
    }

    #[test]
    fn sample_reads_the_bound_signal() {
        let mut snap = FeatureSnapshot::default();
        snap.bands[0] = 0.8;
        snap.onset = true;
        assert!((SourceSignal::Bass.sample(&snap) - 0.8).abs() < 1e-6);
        assert!((SourceSignal::Onset.sample(&snap) - 1.0).abs() < 1e-6);
        snap.onset = false;
        assert!(SourceSignal::Onset.sample(&snap).abs() < 1e-6);
    }

    #[test]
    fn per_row_source_binding_samples_independently() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("a", "bass"), expr("b", "treb")]);
        let mut snap = FeatureSnapshot::default();
        snap.bands[0] = 1.0; // bass full
        snap.bands[2] = 0.0; // treb empty
        ui.sample(&snap);
        assert_eq!(sparkline(&ui.rows[0].spark, 1), "█", "row a follows bass");
        assert_eq!(sparkline(&ui.rows[1].spark, 1), "▁", "row b follows treb");
    }

    // -- table -> expression display --------------------------------------

    #[test]
    fn table_display_covers_each_curve_form() {
        assert_eq!(
            table_display(&Mapping {
                target: "x".into(),
                feature: Feature::Onset,
                curve: Curve::Linear,
                attack_ms: 0.0,
                decay_ms: 0.0,
                scale: 1.0,
                offset: 0.0,
            }),
            "onset"
        );
        assert_eq!(
            table_display(&Mapping {
                target: "x".into(),
                feature: Feature::Bass,
                curve: Curve::Pow { exponent: 2.0 },
                attack_ms: 0.0,
                decay_ms: 0.0,
                scale: 0.5,
                offset: 0.1,
            }),
            "bass [pow 2] ×0.5 +0.1"
        );
        assert_eq!(
            table_display(&Mapping {
                target: "x".into(),
                feature: Feature::Treb,
                curve: Curve::Log,
                attack_ms: 0.0,
                decay_ms: 0.0,
                scale: 1.0,
                offset: 0.0,
            }),
            "treb [log]"
        );
        assert_eq!(
            table_display(&Mapping {
                target: "x".into(),
                feature: Feature::Loud,
                curve: Curve::Step { threshold: 0.5 },
                attack_ms: 100.0,
                decay_ms: 400.0,
                scale: 1.0,
                offset: 0.0,
            }),
            "loud [step 0.5] ~100/400ms"
        );
    }

    // -- edit model -------------------------------------------------------

    #[test]
    fn valid_draft_previews_live() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("gap", "bass")]);
        assert!(ui.drain_apply().is_none(), "nothing pending after open");
        ui.begin_edit();
        // begin_edit seeds the buffer and previews it immediately.
        let applied = ui.drain_apply().expect("begin previews");
        assert_eq!(applied.target(), "gap");
        // Type more: each edit re-previews the compiling draft.
        for c in " * 0.5".chars() {
            ui.insert_char(c);
        }
        assert!(ui.edit_error().is_none(), "a valid draft has no error");
        let applied = ui.drain_apply().expect("valid draft previews");
        match applied {
            MapEntry::Expr(e) => assert_eq!(e.source(), "bass * 0.5"),
            MapEntry::Table(_) => panic!("preview is an expression"),
        }
    }

    #[test]
    fn invalid_draft_keeps_the_last_valid_mapping_and_shows_an_error() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("gap", "bass")]);
        ui.begin_edit();
        let _ = ui.drain_apply(); // clear the begin preview
        // A trailing space is still valid: it previews the last valid mapping.
        ui.insert_char(' ');
        assert!(ui.drain_apply().is_some(), "the valid draft previews");
        // Now an operator with no right-hand side: a parse error, and no NEW
        // preview — the last valid mapping keeps running untouched.
        ui.insert_char('*');
        assert!(ui.edit_error().is_some(), "invalid draft surfaces an error");
        assert!(
            ui.drain_apply().is_none(),
            "an invalid draft must not swap in a new mapping"
        );
    }

    #[test]
    fn esc_reverts_and_enter_commits_and_dirties() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("gap", "bass")]);

        // esc reverts to the pre-edit mapping.
        ui.begin_edit();
        let _ = ui.drain_apply();
        for c in " * 2".chars() {
            ui.insert_char(c);
        }
        ui.cancel_edit();
        let reverted = ui.drain_apply().expect("esc reverts live");
        match reverted {
            MapEntry::Expr(e) => assert_eq!(e.source(), "bass", "reverts to pre-edit"),
            MapEntry::Table(_) => panic!("unexpected form"),
        }
        assert!(!ui.is_dirty("gap"), "a cancelled edit is not dirty");
        assert!(!ui.is_editing());

        // enter commits: keeps applied, updates the row, dirty-flags it.
        ui.begin_edit();
        let _ = ui.drain_apply();
        for c in " * 3".chars() {
            ui.insert_char(c);
        }
        ui.commit_edit();
        assert!(!ui.is_editing(), "commit leaves edit mode");
        assert!(ui.is_dirty("gap"), "commit dirty-flags the target");
        assert_eq!(ui.row_text(0), Some("bass * 3"));
        assert_eq!(ui.dirty_edits(), vec![("gap", "bass * 3")]);
    }

    #[test]
    fn editing_a_table_row_seeds_from_its_source_signal() {
        let mut ui = MappingUi::default();
        ui.open(vec![table("gap", Feature::Onset, Curve::Linear)]);
        assert_eq!(ui.row_is_expr(0), Some(false), "starts table form");
        ui.begin_edit();
        assert_eq!(
            ui.edit_buffer(),
            Some("onset"),
            "seeds from the source signal"
        );
        ui.commit_edit();
        assert_eq!(ui.row_is_expr(0), Some(true), "now expression form");
        assert_eq!(ui.dirty_edits(), vec![("gap", "onset")]);
    }

    #[test]
    fn untouched_table_row_is_never_dirtied() {
        let mut ui = MappingUi::default();
        ui.open(vec![
            table("a", Feature::Bass, Curve::Linear),
            expr("b", "treb"),
        ]);
        // Edit only b.
        ui.select_next();
        ui.begin_edit();
        ui.insert_char('*');
        ui.backspace();
        ui.commit_edit();
        assert!(!ui.is_dirty("a"), "the untouched table row stays clean");
        assert_eq!(
            ui.dirty_edits(),
            vec![("b", "treb")],
            "only the edited row writes"
        );
    }

    #[test]
    fn selection_wraps_and_is_inert_while_editing() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("a", "bass"), expr("b", "mid")]);
        assert_eq!(ui.selected(), 0);
        ui.select_next();
        assert_eq!(ui.selected(), 1);
        ui.select_next();
        assert_eq!(ui.selected(), 0, "wraps");
        ui.select_prev();
        assert_eq!(ui.selected(), 1, "wraps back");
        ui.begin_edit();
        ui.select_next();
        assert_eq!(ui.selected(), 1, "selection is frozen while editing");
    }

    #[test]
    fn open_on_empty_stays_closed() {
        let mut ui = MappingUi::default();
        ui.open(vec![]);
        assert!(!ui.is_open(), "no mappings means nothing to show");
    }

    #[test]
    fn cursor_moves_and_backspace_edits_at_the_cursor() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("gap", "bass")]);
        ui.begin_edit();
        // Buffer "bass", cursor at end (4). Move left twice -> between "ba" and "ss".
        ui.cursor_left();
        ui.cursor_left();
        ui.insert_char('X');
        assert_eq!(ui.edit_buffer(), Some("baXss"));
        ui.backspace();
        assert_eq!(ui.edit_buffer(), Some("bass"));
    }

    // -- render -----------------------------------------------------------

    #[test]
    fn draw_renders_two_rows_with_targets_and_hint() {
        let mut ui = MappingUi::default();
        ui.open(vec![expr("gap", "bass * 0.5"), expr("punch", "onset")]);
        let mut snap = FeatureSnapshot::default();
        snap.bands[0] = 0.5;
        ui.sample(&snap);

        let area = Rect::new(0, 0, 70, 8);
        let mut buf = Buffer::empty(area);
        draw_mapping(&mut buf, area, &ui);

        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("expression map"), "title shows: {text:?}");
        assert!(text.contains("gap"), "first target shows");
        assert!(text.contains("punch"), "second target shows");
        assert!(text.contains('←'), "the rung arrow shows");
        assert!(text.contains("signals:"), "the vocab hint shows");
        assert!(text.contains("bass"), "the vocabulary shows");
    }
}
