//! The quick tuning strip: a bottom overlay that live-adjusts the first few
//! parameters of the running scene's first layer, and writes the adjusted values
//! back to the preset TOML with every comment and formatting detail intact.
//!
//! The model ([`TuningStrip`]) is pure and TTY-free: it holds the parameters on
//! show (up to [`MAX_PARAMS`], sliced from the first layer's scene manifest), the
//! selected index, and the set of keys the user adjusted this session. The render
//! loop seeds it from the [`ScenePresenter`](crate::ScenePresenter) each time it
//! opens, pushes the working values into the layer-0 params bag each frame while
//! open, and — on the write key — hands the dirty edits to one of the write-back
//! functions here.
//!
//! Write-back is comment-preserving: [`apply_params_edit`] parses the existing
//! source with `toml_edit`, sets each adjusted `[params]` key in place, and
//! renders the document back out, so the file the strip writes is byte-for-byte
//! the file a user hand-edits outside the changed values. Both file targets —
//! an existing `--scene-file` ([`write_back_file`]) and a builtin export
//! ([`write_back_export`]) — write atomically (temp file + rename in the same
//! directory), so the target is never observed empty.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use toml_edit::{DocumentMut, Item, Table, Value, value as toml_value};

use crate::palette;

/// The most parameters the strip shows: the first few a tinkerer reaches for
/// (storyboard 1g).
pub const MAX_PARAMS: usize = 4;

/// The number of cells in a slider's fill track (between the `◂` / `▸` caps).
const SLIDER_CELLS: usize = 6;

/// The `←` / `→` adjust step, as a fraction of a parameter's `[min, max]` range.
const STEP_DIVISOR: f32 = 24.0;

/// One parameter shown on the strip: its manifest bounds, current working value,
/// and whether a `[map]` entry drives it.
///
/// A `mapped` parameter is annotated with a `~` and its live adjustment is
/// overwritten by its mapping each frame (the write still lands as the base an
/// unmapped read would see); an unmapped parameter changes the running scene on
/// the same frame.
#[derive(Clone, Debug, PartialEq)]
pub struct TuningParam {
    /// The manifest key (stable, `'static`).
    pub key: &'static str,
    /// Inclusive lower bound.
    pub min: f32,
    /// Inclusive upper bound.
    pub max: f32,
    /// The current working value (seeded from the layer-0 bag, then adjusted).
    pub value: f32,
    /// Whether a `[map]` entry drives this key.
    pub mapped: bool,
}

/// The tuning-strip model: the parameters on show, the selection, whether it is
/// open, and the keys adjusted this session.
///
/// Pure state, driven by the input handler and read by the render loop. The
/// `dirty` set persists across close/reopen so a write covers the whole session,
/// not just the current open panel.
#[derive(Clone, Debug, Default)]
pub struct TuningStrip {
    /// The parameters currently on show (rebuilt each time the strip opens).
    params: Vec<TuningParam>,
    /// The selected parameter index.
    selected: usize,
    /// Whether the strip is open.
    open: bool,
    /// Keys adjusted this session, each with the value last set. Persisted across
    /// close/reopen so write-back covers every key the user touched.
    dirty: Vec<(&'static str, f32)>,
}

impl TuningStrip {
    /// Whether the strip is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The parameters currently on show.
    #[must_use]
    pub fn params(&self) -> &[TuningParam] {
        &self.params
    }

    /// The selected parameter index.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Open the strip on `params` (the first layer's manifest values). The list
    /// is sliced to the first [`MAX_PARAMS`]; any key already adjusted this
    /// session is re-shown at the user's set value (so a reopened mapped key
    /// reads the intended value, not the frame the mapping last left in the bag).
    /// Opening on an empty list is a no-op — there is nothing to tune.
    pub fn open(&mut self, mut params: Vec<TuningParam>) {
        params.truncate(MAX_PARAMS);
        for p in &mut params {
            if let Some((_, v)) = self.dirty.iter().find(|(k, _)| *k == p.key) {
                p.value = *v;
            }
        }
        self.params = params;
        self.selected = 0;
        self.open = !self.params.is_empty();
    }

    /// Close the strip. The dirty set is kept, so a later reopen or write still
    /// carries this session's edits.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Move the selection to the next parameter, wrapping.
    pub fn select_next(&mut self) {
        if !self.params.is_empty() {
            self.selected = (self.selected + 1) % self.params.len();
        }
    }

    /// Adjust the selected parameter by one step in `dir` (`-1` / `+1`), clamped
    /// to its `[min, max]`, marking the key dirty. Returns the `(key, value)`
    /// written, or `None` when there is no selected parameter.
    pub fn adjust_selected(&mut self, dir: i32) -> Option<(&'static str, f32)> {
        let p = self.params.get_mut(self.selected)?;
        let range = p.max - p.min;
        let step = if range > 0.0 {
            range / STEP_DIVISOR
        } else {
            0.0
        };
        let value = (p.value + dir as f32 * step).clamp(p.min, p.max);
        p.value = value;
        let key = p.key;
        Self::mark_dirty(&mut self.dirty, key, value);
        Some((key, value))
    }

    /// The keys adjusted this session, each with the value last set. This is what
    /// write-back applies to the preset's `[params]` table.
    #[must_use]
    pub fn dirty_edits(&self) -> &[(&'static str, f32)] {
        &self.dirty
    }

    /// Record `key` → `value` in `dirty`, overwriting any earlier value.
    fn mark_dirty(dirty: &mut Vec<(&'static str, f32)>, key: &'static str, value: f32) {
        if let Some(entry) = dirty.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = value;
        } else {
            dirty.push((key, value));
        }
    }
}

/// The count of filled slider cells for `value` across `[min, max]`, in
/// `0..=SLIDER_CELLS`: `0` at (or below) `min`, [`SLIDER_CELLS`] at (or above)
/// `max`. A degenerate range yields `0`.
#[must_use]
fn slider_fill(value: f32, min: f32, max: f32) -> usize {
    if max <= min {
        return 0;
    }
    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
    (t * SLIDER_CELLS as f32).round() as usize
}

/// The slider glyph string for a value, e.g. `◂▰▰▰▰▱▱▸`.
fn slider_str(value: f32, min: f32, max: f32) -> String {
    let filled = slider_fill(value, min, max);
    let mut s = String::with_capacity(SLIDER_CELLS + 2);
    s.push('◂');
    for _ in 0..filled {
        s.push('▰');
    }
    for _ in filled..SLIDER_CELLS {
        s.push('▱');
    }
    s.push('▸');
    s
}

/// The full segment for a parameter: `~name ◂▰▰▱▱▱▸ 0.42` (the `~` present only
/// for a mapped key).
fn segment_full(p: &TuningParam) -> String {
    let mark = if p.mapped { "~" } else { "" };
    format!(
        "{mark}{} {} {:.2}",
        p.key,
        slider_str(p.value, p.min, p.max),
        p.value
    )
}

/// The degraded segment for a narrow pane: `~name 0.42`, no slider.
fn segment_degraded(p: &TuningParam) -> String {
    let mark = if p.mapped { "~" } else { "" };
    format!("{mark}{} {:.2}", p.key, p.value)
}

/// The separator drawn between parameter segments.
const SEP: &str = " · ";

/// Paint the tuning strip over the bottom one or two rows of the body, like the
/// other overlays. Draws nothing when the strip is closed, empty, or the body is
/// degenerate. On a pane too narrow for the sliders it degrades to `name value`
/// pairs; the second row (when there is height for it) shows the key hint.
pub fn draw_tuning(buf: &mut Buffer, body: Rect, strip: &TuningStrip) {
    if !strip.open || body.width == 0 || body.height == 0 || strip.params.is_empty() {
        return;
    }
    let rows: u16 = if body.height >= 2 { 2 } else { 1 };
    let y0 = body.y + body.height - rows;
    let width = body.width as usize;
    let fill = Style::new().bg(palette::OVERLAY_BG).fg(palette::OVERLAY_FG);

    // Clear the strip rows so the scene beneath does not bleed through.
    for dy in 0..rows {
        for dx in 0..body.width {
            if let Some(cell) = buf.cell_mut((body.x + dx, y0 + dy)) {
                cell.set_char(' ').set_style(fill);
            }
        }
    }

    // Choose full (with slider) or degraded (name + value) segments by whether
    // the full line fits the width, the way the other overlays fall back.
    let full: Vec<String> = strip.params.iter().map(segment_full).collect();
    let full_len: usize = full.iter().map(|s| s.chars().count()).sum::<usize>()
        + SEP.chars().count() * strip.params.len().saturating_sub(1);
    let segs: Vec<String> = if full_len <= width {
        full
    } else {
        strip.params.iter().map(segment_degraded).collect()
    };

    // Lay the segments out left to right, highlighting the selected one.
    let end = body.x + body.width;
    let mut x = body.x;
    for (i, seg) in segs.iter().enumerate() {
        if x >= end {
            break;
        }
        if i > 0 {
            let (nx, _) = buf.set_stringn(x, y0, SEP, (end - x) as usize, fill.fg(palette::DEBUG));
            x = nx;
            if x >= end {
                break;
            }
        }
        let style = if i == strip.selected {
            fill.add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            fill
        };
        let (nx, _) = buf.set_stringn(x, y0, seg, (end - x) as usize, style);
        x = nx;
    }

    if rows == 2 {
        buf.set_stringn(
            body.x,
            y0 + 1,
            " [tab] param · [←/→] adjust · [w] write · [esc] done",
            width,
            fill.add_modifier(Modifier::DIM),
        );
    }
}

// ---------------------------------------------------------------------------
// Comment-preserving write-back
// ---------------------------------------------------------------------------

/// Apply the adjusted `[params]` edits to preset `src`, preserving every comment,
/// key order and unknown key outside the changed values, and return the rendered
/// document. Creates the `[params]` table when it is absent; sets each key in
/// place (keeping the existing value's own formatting decor) when it is present.
///
/// # Errors
/// Returns the `toml_edit` parse error when `src` is not valid TOML.
pub fn apply_params_edit(src: &str, edits: &[(&str, f32)]) -> Result<String, toml_edit::TomlError> {
    let mut doc: DocumentMut = src.parse()?;
    let item = doc
        .entry("params")
        .or_insert_with(|| Item::Table(Table::new()));
    // A `[params]` that is somehow not a table is replaced with a fresh one; a
    // preset that validates never hits this, but write-back must not panic.
    if item.as_table_mut().is_none() {
        *item = Item::Table(Table::new());
    }
    let table = item.as_table_mut().expect("params is a table");
    for (key, v) in edits {
        set_param_value(table, key, *v);
    }
    Ok(doc.to_string())
}

/// Set `key` = `v` in `table`. When the key already exists, the value is mutated
/// in place so its own formatting decor (the whitespace / inline comment around
/// the value) *and* the key's leading comment survive — only the value token
/// changes. A new key is appended with default formatting.
fn set_param_value(table: &mut Table, key: &str, v: f32) {
    let clean = clean_f64(v);
    if let Some(item) = table.get_mut(key) {
        let decor = item.as_value().map(|val| val.decor().clone());
        let mut value = Value::from(clean);
        if let Some(decor) = decor {
            *value.decor_mut() = decor;
        }
        *item = Item::Value(value);
    } else {
        table.insert(key, toml_value(clean));
    }
}

/// The `f64` to write for an adjusted `f32`, via the `f32`'s shortest decimal
/// string, so `0.9f32` renders as `0.9` and not the `0.8999999…` its widening
/// to `f64` would produce.
fn clean_f64(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or_else(|_| f64::from(v))
}

/// Write the adjusted edits back to an existing preset file (the `--scene-file`
/// path): read it, apply the edits comment-preserving, and write it atomically
/// (temp file + rename in the same directory).
///
/// # Errors
/// Returns an I/O error if the file cannot be read or written, or an
/// `InvalidData` error if it is not valid TOML.
pub fn write_back_file(path: &Path, edits: &[(&str, f32)]) -> io::Result<()> {
    let src = fs::read_to_string(path)?;
    let edited = apply_params_edit(&src, edits).map_err(to_io)?;
    atomic_write(path, &edited)
}

/// Export a builtin preset to `<base_dir>/presets/<name>.toml`, applying the
/// adjusted edits. The source is the exported file itself when it already exists
/// (so repeated writes edit the same file), otherwise the embedded builtin source
/// (comments intact). Creates the `presets` directory and writes atomically.
/// Returns the file written.
///
/// # Errors
/// Returns an I/O error if the directory or file cannot be created / written, or
/// an `InvalidData` error if the existing file is not valid TOML.
pub fn write_back_export(
    base_dir: &Path,
    name: &str,
    builtin_src: &str,
    edits: &[(&str, f32)],
) -> io::Result<PathBuf> {
    let presets_dir = base_dir.join("presets");
    fs::create_dir_all(&presets_dir)?;
    let target = presets_dir.join(format!("{name}.toml"));
    let src = match fs::read_to_string(&target) {
        Ok(existing) => existing,
        Err(err) if err.kind() == io::ErrorKind::NotFound => builtin_src.to_string(),
        Err(err) => return Err(err),
    };
    let edited = apply_params_edit(&src, edits).map_err(to_io)?;
    atomic_write(&target, &edited)?;
    Ok(target)
}

/// Convert a `toml_edit` parse error into an `InvalidData` I/O error.
fn to_io(err: toml_edit::TomlError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

/// Write `contents` to `path` atomically: write a sibling temp file in the same
/// directory, flush and sync it, then rename it over `path`. A reader of `path`
/// therefore never sees a partial or empty file.
fn atomic_write(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = match dir {
        Some(dir) => dir.join(&tmp_name),
        None => PathBuf::from(&tmp_name),
    };

    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    // Rename over the target; clean up the temp file on failure so a botched
    // write leaves nothing behind.
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(key: &'static str, min: f32, max: f32, value: f32, mapped: bool) -> TuningParam {
        TuningParam {
            key,
            min,
            max,
            value,
            mapped,
        }
    }

    #[test]
    fn open_slices_to_at_most_four_params() {
        let mut strip = TuningStrip::default();
        strip.open(vec![
            param("a", 0.0, 1.0, 0.5, false),
            param("b", 0.0, 1.0, 0.5, false),
            param("c", 0.0, 1.0, 0.5, false),
            param("d", 0.0, 1.0, 0.5, false),
            param("e", 0.0, 1.0, 0.5, false),
        ]);
        assert!(strip.is_open());
        assert_eq!(strip.params().len(), MAX_PARAMS);
        assert_eq!(strip.params()[0].key, "a");
        assert_eq!(strip.params()[3].key, "d");
    }

    #[test]
    fn open_on_empty_stays_closed() {
        let mut strip = TuningStrip::default();
        strip.open(vec![]);
        assert!(
            !strip.is_open(),
            "nothing to tune means the strip stays shut"
        );
    }

    #[test]
    fn selection_cycles_and_wraps() {
        let mut strip = TuningStrip::default();
        strip.open(vec![
            param("a", 0.0, 1.0, 0.5, false),
            param("b", 0.0, 1.0, 0.5, false),
            param("c", 0.0, 1.0, 0.5, false),
        ]);
        assert_eq!(strip.selected(), 0);
        strip.select_next();
        assert_eq!(strip.selected(), 1);
        strip.select_next();
        assert_eq!(strip.selected(), 2);
        strip.select_next();
        assert_eq!(strip.selected(), 0, "selection wraps");
    }

    #[test]
    fn slider_fill_spans_the_track_at_the_bounds() {
        // Empty at the minimum, full at the maximum, mid near the middle.
        assert_eq!(slider_fill(0.0, 0.0, 1.0), 0);
        assert_eq!(slider_fill(1.0, 0.0, 1.0), SLIDER_CELLS);
        assert_eq!(slider_fill(0.5, 0.0, 1.0), SLIDER_CELLS / 2);
        // Out-of-range values clamp rather than overflow the track.
        assert_eq!(slider_fill(-5.0, 0.0, 1.0), 0);
        assert_eq!(slider_fill(5.0, 0.0, 1.0), SLIDER_CELLS);
        // A degenerate range is empty, not a divide-by-zero.
        assert_eq!(slider_fill(1.0, 1.0, 1.0), 0);
        // A non-zero minimum maps correctly.
        assert_eq!(slider_fill(0.01, 0.01, 2.0), 0);
        assert_eq!(slider_fill(2.0, 0.01, 2.0), SLIDER_CELLS);
    }

    #[test]
    fn adjust_steps_by_a_twenty_fourth_and_clamps_at_the_bounds() {
        let mut strip = TuningStrip::default();
        strip.open(vec![param("gap", 0.0, 1.2, 0.6, false)]);
        // One step is range / 24 = 0.05.
        let (key, value) = strip.adjust_selected(1).expect("a selected param");
        assert_eq!(key, "gap");
        assert!((value - 0.65).abs() < 1e-6, "one up-step is +0.05: {value}");
        // Drive it hard against the ceiling: it clamps at max, never past.
        for _ in 0..100 {
            strip.adjust_selected(1);
        }
        assert!(
            (strip.params()[0].value - 1.2).abs() < 1e-6,
            "clamps at max"
        );
        // And against the floor.
        for _ in 0..100 {
            strip.adjust_selected(-1);
        }
        assert!(
            (strip.params()[0].value - 0.0).abs() < 1e-6,
            "clamps at min"
        );
    }

    #[test]
    fn only_adjusted_keys_are_dirty_and_persist_across_reopen() {
        let mut strip = TuningStrip::default();
        strip.open(vec![
            param("a", 0.0, 1.0, 0.5, false),
            param("b", 0.0, 1.0, 0.5, false),
        ]);
        assert!(
            strip.dirty_edits().is_empty(),
            "nothing dirty before an edit"
        );
        strip.select_next(); // select b
        let (_, bv) = strip.adjust_selected(1).unwrap();
        assert_eq!(strip.dirty_edits(), &[("b", bv)]);

        // Reopen from fresh manifest values: b is re-shown at the adjusted value,
        // and it is still the only dirty key.
        strip.close();
        strip.open(vec![
            param("a", 0.0, 1.0, 0.5, false),
            param("b", 0.0, 1.0, 0.5, false),
        ]);
        assert!(
            (strip.params()[1].value - bv).abs() < 1e-6,
            "reopen shows the set value"
        );
        assert_eq!(strip.dirty_edits(), &[("b", bv)]);
    }

    #[test]
    fn mapped_annotation_marks_only_mapped_params() {
        let mapped = param("punch", 0.0, 2.0, 0.35, true);
        let plain = param("gap", 0.0, 0.9, 0.15, false);
        assert!(segment_full(&mapped).starts_with('~'), "mapped gets a ~");
        assert!(!segment_full(&plain).starts_with('~'), "unmapped does not");
        assert!(segment_degraded(&mapped).starts_with('~'));
        assert!(!segment_degraded(&plain).starts_with('~'));
    }

    // -- write-back --------------------------------------------------------

    const FIXTURE: &str = "\
# a preset with comments
[preset]
name = \"demo\"
scene = \"spectra\"

# tuning parameters
[params]
# release: extra release time constant
release = 0.15
gap = 0.15
punch = 0.35  # driven on the onset, but still hand-tunable

[palette]
source = \"static\"
";

    #[test]
    fn edit_preserves_comments_order_and_unknown_keys_byte_for_byte() {
        let out =
            apply_params_edit(FIXTURE, &[("release", 0.5), ("gap", 0.9)]).expect("valid toml");
        let expected = "\
# a preset with comments
[preset]
name = \"demo\"
scene = \"spectra\"

# tuning parameters
[params]
# release: extra release time constant
release = 0.5
gap = 0.9
punch = 0.35  # driven on the onset, but still hand-tunable

[palette]
source = \"static\"
";
        assert_eq!(out, expected);
    }

    #[test]
    fn edit_creates_the_params_table_when_absent() {
        let src = "\
[preset]
name = \"demo\"
scene = \"spectra\"
";
        let out = apply_params_edit(src, &[("gap", 0.5)]).expect("valid toml");
        // The original lines survive and a [params] table with the key appears.
        assert!(out.contains("[preset]"));
        assert!(out.contains("scene = \"spectra\""));
        assert!(out.contains("[params]"), "the table is created: {out}");
        assert!(out.contains("gap = 0.5"), "the key is written: {out}");
        // The result is still valid TOML round-tripping through the parser.
        let _: DocumentMut = out.parse().expect("edited output re-parses");
    }

    #[test]
    fn write_back_file_is_atomic_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("scia-tuning-file-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("preset.toml");
        fs::write(&path, FIXTURE).expect("seed fixture");

        write_back_file(&path, &[("release", 0.5), ("gap", 0.9)]).expect("write-back");

        let read = fs::read_to_string(&path).expect("read back");
        assert!(read.contains("release = 0.5"));
        assert!(read.contains("gap = 0.9"));
        assert!(read.contains("# a preset with comments"), "comments intact");
        assert!(!read.is_empty(), "the file is never observed empty");
        // No temp file is left behind in the directory.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("list dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file remains: {leftovers:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_writes_under_the_config_relative_presets_dir() {
        // Inject a temp base dir; the real ~/.config is never touched.
        let base = std::env::temp_dir().join(format!("scia-tuning-export-{}", std::process::id()));
        fs::remove_dir_all(&base).ok();

        let written =
            write_back_export(&base, "spectra", FIXTURE, &[("gap", 0.9)]).expect("export write");
        assert_eq!(written, base.join("presets").join("spectra.toml"));
        let read = fs::read_to_string(&written).expect("read export");
        assert!(read.contains("gap = 0.9"));
        assert!(
            read.contains("# a preset with comments"),
            "builtin comments carried"
        );

        // A second export edits the same file (source is the existing export, not
        // the builtin), so an earlier edit survives alongside the new one.
        let again = write_back_export(&base, "spectra", "SHOULD-NOT-BE-USED", &[("release", 0.5)])
            .expect("second export");
        assert_eq!(again, written);
        let read2 = fs::read_to_string(&again).expect("read export again");
        assert!(read2.contains("release = 0.5"), "new edit applied");
        assert!(read2.contains("gap = 0.9"), "the earlier edit is preserved");

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn draw_renders_a_selected_slider_row() {
        let mut strip = TuningStrip::default();
        strip.open(vec![
            param("gap", 0.0, 1.0, 1.0, false),
            param("punch", 0.0, 2.0, 0.35, true),
        ]);
        let area = Rect::new(0, 0, 60, 6);
        let mut buf = Buffer::empty(area);
        draw_tuning(&mut buf, area, &strip);

        // Collect the two bottom rows into strings.
        let row: String = (0..area.width)
            .map(|x| buf.cell((x, 4)).unwrap().symbol().to_string())
            .collect();
        let hint: String = (0..area.width)
            .map(|x| buf.cell((x, 5)).unwrap().symbol().to_string())
            .collect();

        assert!(row.contains("gap"), "the first param name shows: {row:?}");
        assert!(row.contains('▰'), "a filled slider cell shows: {row:?}");
        assert!(row.contains('▸'), "the slider cap shows: {row:?}");
        assert!(
            row.contains("~punch"),
            "the mapped param is annotated: {row:?}"
        );
        assert!(hint.contains("[tab]"), "the hint row shows: {hint:?}");
    }
}
