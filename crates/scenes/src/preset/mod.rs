//! The TOML preset format: parse, validate and instantiate a scene preset.
//!
//! A preset is a plain TOML file that fully describes a scene instance: the
//! scene type, its typed parameters, an optional layer stack, feature →
//! parameter mappings with response curves and attack/decay envelopes, and a
//! palette source. [`parse_preset`] and [`load_preset`] turn source text into a
//! validated [`Preset`]; [`Preset::instantiate`] builds the live
//! [`LayerInstance`]s (each an initialized [`Scene`] plus its [`MappingSet`]).
//!
//! Validation is strict and every error names the file and line: unknown keys,
//! type mismatches, out-of-range values, unknown scenes and features, malformed
//! palettes and invalid `[map]` expressions all surface as a
//! [`PresetError`] whose [`Display`](std::fmt::Display) begins with
//! `file:line:col:`.
//!
//! A `[map]` value may be either a response **table** or a string
//! **expression** (rung 1): the expression is compiled once at load over the
//! storyboard feature vocabulary and evaluated per frame, allocation-free. See
//! [`expr`] and `docs/presets.md`.
//!
//! Selecting a preset at launch, cycling it at runtime and album-art palettes
//! arrive with later cards; see `docs/presets.md` for what is not yet wired.

// `PresetError` is intentionally rich — a source path, a line/column and a
// structured kind — so every failure names the file and position (criterion 2).
// That makes it larger than clippy's `result_large_err` threshold, but it only
// ever travels on the cold validation-failure path, never per frame.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use toml::Spanned;

use scia_core::FeatureSnapshot;

use crate::palette::{Palette, Rgb};
use crate::registry::{create_builtin, scene_info};
use crate::scene::{ParamSpec, Params, Scene, SceneCtx};

mod discover;
mod expr;
mod watch;

pub(crate) use discover::discovered_preset;
pub use discover::{DEFAULT_PRESETS_SUBDIR, discovered_preset_names, presets_dir};
use expr::{CompiledExpr, EXPR_VARS, ExprCompileError, ExprEnv, ONSET_TAU};
pub use watch::{PresetWatcher, ReloadEvent};

// ---------------------------------------------------------------------------
// Public value types
// ---------------------------------------------------------------------------

/// How a layer composites over the layers beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Blend {
    /// Source-over (the default).
    #[default]
    Over,
    /// Additive.
    Add,
    /// Component-wise maximum.
    Max,
}

/// Where a preset's palette comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaletteSource {
    /// A fixed palette, either the eight `slots` or the host default.
    #[default]
    Static,
    /// Derived from the currently playing album art. Accepted and validated,
    /// but resolves to the host default palette until the album-art card lands.
    AlbumArt,
}

/// A feature scalar a mapping can read from a [`FeatureSnapshot`].
///
/// Each variant clamps to `0.0..=1.0` before the response curve is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// `bands[0]` — bass band level.
    Bass,
    /// `bands[1]` — mid band level.
    Mid,
    /// `bands[2]` — treble band level.
    Treb,
    /// `rms` — loudness.
    Loud,
    /// `peak` — peak sample of the hop.
    Peak,
    /// `1.0` on an onset hop, else `0.0`.
    Onset,
    /// `flux` — spectral flux.
    Flux,
    /// `beat_confidence` — `0.0` until the beat-tracker card lands.
    Beat,
    /// `((1 - stereo_correlation) / 2).clamp(0, 1)` — stereo width.
    Width,
}

/// A response curve applied to a clamped feature value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// `x`.
    Linear,
    /// `x^exponent` (`exponent > 0`).
    Pow {
        /// The exponent.
        exponent: f32,
    },
    /// `ln(1 + 9x) / ln 10`.
    Log,
    /// `1.0` when `x >= threshold`, else `0.0` (`threshold` in `0..=1`).
    Step {
        /// The threshold.
        threshold: f32,
    },
}

/// One layer of a preset's layer stack.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// The scene id this layer draws.
    pub scene: String,
    /// How the layer composites.
    pub blend: Blend,
    /// Layer intensity in `0.0..=1.0`.
    pub intensity: f32,
    /// The explicit `[layer.params]` overlay (over the layer scene's defaults).
    pub params: Vec<(String, f32)>,
}

/// A validated feature → parameter mapping.
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    /// The parameter the mapping drives.
    pub target: String,
    /// The feature it reads.
    pub feature: Feature,
    /// The response curve.
    pub curve: Curve,
    /// Attack time in milliseconds (`0` = instant).
    pub attack_ms: f32,
    /// Decay time in milliseconds (`0` = instant).
    pub decay_ms: f32,
    /// Output scale.
    pub scale: f32,
    /// Output offset.
    pub offset: f32,
}

/// A validated string `[map]` expression: the target parameter plus a compiled
/// program over the storyboard vocabulary, shared cheaply behind an [`Arc`].
#[derive(Clone, Debug, PartialEq)]
pub struct ExprMapping {
    /// The parameter the expression drives.
    pub target: String,
    /// The compiled expression.
    expr: Arc<CompiledExpr>,
}

impl ExprMapping {
    /// The original expression source text.
    #[must_use]
    pub fn source(&self) -> &str {
        self.expr.source()
    }

    /// Compile `source` into an expression mapping driving `target`, over the
    /// storyboard feature vocabulary. This is the runtime entry point the
    /// expression-mapping overlay uses to build a mapping from edited text; it
    /// applies exactly the same compile and vocabulary check
    /// [`parse_preset`] applies to a string `[map]` value at load, so a draft
    /// that compiles here is a draft that would load.
    ///
    /// # Errors
    /// Returns an [`ExprError`] with a short, single-line message when the
    /// expression is malformed or references a name outside the vocabulary.
    pub fn compile(target: impl Into<String>, source: &str) -> Result<Self, ExprError> {
        let expr = CompiledExpr::compile(source).map_err(|e| ExprError {
            message: match e {
                ExprCompileError::Syntax(msg) => format!("invalid expression: {msg}"),
                ExprCompileError::UnknownVar(name) => format!("unknown variable `{name}`"),
            },
        })?;
        Ok(Self {
            target: target.into(),
            expr,
        })
    }
}

/// An error compiling a `[map]` expression string at runtime (from the
/// expression-mapping overlay, via [`ExprMapping::compile`]). Its
/// [`Display`](fmt::Display) is a short, single-line message suitable for inline
/// UI — unlike [`PresetError`] it carries no file position, because a runtime
/// draft has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExprError {
    message: String,
}

impl ExprError {
    /// The short, single-line error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExprError {}

/// One validated `[map]` entry: either a response **table** or a compiled string
/// **expression**. Both forms drive one target parameter of the mapped scene.
#[derive(Clone, Debug, PartialEq)]
pub enum MapEntry {
    /// A feature → parameter response table with a curve and envelope.
    Table(Mapping),
    /// A per-frame expression over the feature vocabulary.
    Expr(ExprMapping),
}

impl MapEntry {
    /// The target parameter this entry drives.
    #[must_use]
    pub fn target(&self) -> &str {
        match self {
            Self::Table(m) => &m.target,
            Self::Expr(e) => &e.target,
        }
    }
}

/// A fully validated preset: a plain value ready to [`instantiate`].
///
/// [`instantiate`]: Preset::instantiate
#[derive(Clone, Debug)]
pub struct Preset {
    /// The preset name (`^[a-z0-9][a-z0-9-]*$`).
    pub name: String,
    /// The preset's scene id (also the sole layer's scene when there are no
    /// explicit `[[layer]]`s).
    pub scene: String,
    /// The optional description.
    pub description: Option<String>,
    /// The mood (defaults to the scene's mood).
    pub mood: String,
    /// Preset parameters, merged over the scene manifest defaults.
    pub params: Params,
    /// The explicit layer stack (empty for a single-layer preset).
    pub layers: Vec<Layer>,
    /// The feature → parameter mappings (table or expression entries).
    pub mappings: Vec<MapEntry>,
    /// Where the palette comes from.
    pub palette_source: PaletteSource,
    /// The `[params]` overlay alone, retained for per-layer merging.
    params_overlay: Vec<(String, f32)>,
    /// The resolved palette (from `slots`, or the host default).
    palette: Palette,
}

/// A live layer produced by [`Preset::instantiate`]: an initialized scene, its
/// blend and intensity, and its mapping state.
pub struct LayerInstance {
    /// The initialized scene.
    pub scene: Box<dyn Scene>,
    /// How it composites.
    pub blend: Blend,
    /// Its intensity in `0.0..=1.0`.
    pub intensity: f32,
    /// The feature → parameter mappings driving this layer.
    pub mappings: MappingSet,
}

impl fmt::Debug for LayerInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerInstance")
            .field("scene", &self.scene.id())
            .field("blend", &self.blend)
            .field("intensity", &self.intensity)
            .field("mappings", &self.mappings)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Mapping runtime
// ---------------------------------------------------------------------------

/// Per-mapping runtime state: either a table response (with its own
/// envelope-follower value) or a compiled expression.
#[derive(Clone, Debug)]
enum MappingState {
    /// A response table: the spec plus the envelope-follower value.
    Table(TableState),
    /// A compiled expression, evaluated per frame against the shared namespace.
    Expr(ExprState),
}

impl MappingState {
    /// The target parameter key this state writes.
    fn target(&self) -> &str {
        match self {
            Self::Table(t) => &t.target,
            Self::Expr(e) => &e.target,
        }
    }
}

/// Runtime state for a table mapping: its spec plus the envelope-follower value.
#[derive(Clone, Debug)]
struct TableState {
    target: Box<str>,
    feature: Feature,
    curve: Curve,
    attack_tau: f32,
    decay_tau: f32,
    scale: f32,
    offset: f32,
    env: f32,
}

impl TableState {
    fn new(m: &Mapping) -> Self {
        Self {
            target: Box::from(m.target.as_str()),
            feature: m.feature,
            curve: m.curve,
            attack_tau: m.attack_ms / 1000.0,
            decay_tau: m.decay_ms / 1000.0,
            scale: m.scale,
            offset: m.offset,
            env: 0.0,
        }
    }
}

/// Runtime state for an expression mapping: its target and compiled program.
#[derive(Clone, Debug)]
struct ExprState {
    target: Box<str>,
    expr: Arc<CompiledExpr>,
}

/// The runtime bundle of a layer's mappings.
///
/// [`MappingSet::apply`] folds the newest features into every mapping and writes
/// the results into a [`Params`] bag. It allocates nothing after construction as
/// long as the target keys are already present in the bag; [`MappingSet::seed`]
/// pre-seeds them. Expression entries are compiled once (at preset load) and
/// evaluated per frame with no per-frame allocation.
#[derive(Clone, Debug, Default)]
pub struct MappingSet {
    entries: Vec<MappingState>,
    /// The maintained onset envelope read by the `onset` expression variable:
    /// `1.0` on an onset hop, an exponential decay (tau [`ONSET_TAU`]) otherwise.
    onset_env: f32,
}

impl MappingSet {
    /// Build a mapping set from validated table mapping specs. Envelope state
    /// starts at zero. (Expression entries are built via [`from_entries`].)
    ///
    /// [`from_entries`]: MappingSet::from_entries
    #[must_use]
    pub fn new(mappings: &[Mapping]) -> Self {
        let entries = mappings
            .iter()
            .map(|m| MappingState::Table(TableState::new(m)))
            .collect();
        Self {
            entries,
            onset_env: 0.0,
        }
    }

    /// Build a mapping set from validated `[map]` entries (table or expression).
    fn from_entries(entries: &[MapEntry]) -> Self {
        let entries = entries
            .iter()
            .map(|e| match e {
                MapEntry::Table(m) => MappingState::Table(TableState::new(m)),
                MapEntry::Expr(x) => MappingState::Expr(ExprState {
                    target: Box::from(x.target.as_str()),
                    expr: Arc::clone(&x.expr),
                }),
            })
            .collect();
        Self {
            entries,
            onset_env: 0.0,
        }
    }

    /// The number of mappings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no mappings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Ensure every target key exists in `params`, so the first [`apply`] does
    /// not have to push (and therefore does not allocate). A key already present
    /// keeps its value.
    ///
    /// [`apply`]: MappingSet::apply
    pub fn seed(&self, params: &mut Params) {
        for st in &self.entries {
            let target = st.target();
            if params.get(target).is_none() {
                params.set(target, 0.0);
            }
        }
    }

    /// The current entries as public [`MapEntry`] values, for UI introspection
    /// (the expression-mapping overlay lists these). A table entry is
    /// reconstructed from its runtime spec (its live envelope-follower value is
    /// not part of the view); an expression entry shares the compiled program
    /// behind the [`Arc`], so this allocates only the row vector and the target
    /// strings, never a new expression slab.
    #[must_use]
    pub fn entries_view(&self) -> Vec<MapEntry> {
        self.entries
            .iter()
            .map(|s| match s {
                MappingState::Table(t) => MapEntry::Table(Mapping {
                    target: t.target.to_string(),
                    feature: t.feature,
                    curve: t.curve,
                    attack_ms: t.attack_tau * 1000.0,
                    decay_ms: t.decay_tau * 1000.0,
                    scale: t.scale,
                    offset: t.offset,
                }),
                MappingState::Expr(e) => MapEntry::Expr(ExprMapping {
                    target: e.target.to_string(),
                    expr: Arc::clone(&e.expr),
                }),
            })
            .collect()
    }

    /// Replace the entry whose target matches `entry`'s target with `entry`,
    /// preserving row order and every other entry's runtime state. The replaced
    /// row's runtime state is rebuilt from `entry` (a table row's envelope
    /// follower therefore resets to zero). Returns whether a matching row was
    /// found and replaced; a target present in no row is left unchanged.
    ///
    /// The expression-mapping overlay uses this to swap one row's mapping into
    /// the live set as the user edits, so a valid draft previews on the next
    /// frame without rebuilding the whole set.
    pub fn replace(&mut self, entry: MapEntry) -> bool {
        let target = entry.target();
        let Some(slot) = self.entries.iter_mut().find(|s| s.target() == target) else {
            return false;
        };
        *slot = match &entry {
            MapEntry::Table(m) => MappingState::Table(TableState::new(m)),
            MapEntry::Expr(x) => MappingState::Expr(ExprState {
                target: Box::from(x.target.as_str()),
                expr: Arc::clone(&x.expr),
            }),
        };
        true
    }

    /// Advance every mapping by `dt` seconds against the newest features and
    /// write each result into `params`.
    ///
    /// For a **table** entry: read the feature, clamp to `0.0..=1.0`, apply the
    /// curve, run a first-order envelope follower toward that target (instant
    /// when the relevant time constant is zero; otherwise
    /// `y += (x - y) * (1 - exp(-dt / tau))`, using the attack constant while
    /// rising and the decay constant while falling), then store
    /// `offset + scale * y`.
    ///
    /// For an **expression** entry: evaluate the compiled program against the
    /// namespace built from `f` and the maintained onset envelope, and store the
    /// result (non-finite results are sanitized to `0.0`). Either way the scene
    /// clamps the stored value to the parameter's manifest range on read.
    ///
    /// Allocation-free once the target keys are present (see [`seed`]).
    ///
    /// [`seed`]: MappingSet::seed
    pub fn apply(&mut self, f: &FeatureSnapshot, dt: f32, params: &mut Params) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Maintain the shared onset envelope: full on an onset hop, otherwise an
        // exponential decay so the `onset` variable is a usable envelope rather
        // than a single-frame spike.
        if f.onset {
            self.onset_env = 1.0;
        } else if ONSET_TAU > 0.0 {
            self.onset_env *= (-dt / ONSET_TAU).exp();
        }
        let onset_env = self.onset_env;

        // Built lazily on the stack the first time an expression entry needs it;
        // pure-table sets never touch it. `ExprEnv` is `Copy` — no allocation.
        let mut env: Option<ExprEnv> = None;

        for st in &mut self.entries {
            match st {
                MappingState::Table(t) => {
                    let x = feature_value(t.feature, f).clamp(0.0, 1.0);
                    let target = curve_apply(t.curve, x);
                    let tau = if target > t.env {
                        t.attack_tau
                    } else {
                        t.decay_tau
                    };
                    if tau <= 0.0 {
                        t.env = target;
                    } else {
                        t.env += (target - t.env) * (1.0 - (-dt / tau).exp());
                    }
                    params.set(&t.target, t.offset + t.scale * t.env);
                }
                MappingState::Expr(e) => {
                    let ns = env.get_or_insert_with(|| ExprEnv::from_snapshot(f, onset_env));
                    params.set(&e.target, e.expr.eval(ns));
                }
            }
        }
    }
}

/// Read a feature scalar from a snapshot.
fn feature_value(feature: Feature, f: &FeatureSnapshot) -> f32 {
    match feature {
        Feature::Bass => f.bands[0],
        Feature::Mid => f.bands[1],
        Feature::Treb => f.bands[2],
        Feature::Loud => f.rms,
        Feature::Peak => f.peak,
        Feature::Onset => {
            if f.onset {
                1.0
            } else {
                0.0
            }
        }
        Feature::Flux => f.flux,
        Feature::Beat => f.beat_confidence,
        Feature::Width => ((1.0 - f.stereo_correlation) / 2.0).clamp(0.0, 1.0),
    }
}

/// Apply a response curve to a value already clamped to `0.0..=1.0`.
fn curve_apply(curve: Curve, x: f32) -> f32 {
    match curve {
        Curve::Linear => x,
        Curve::Pow { exponent } => x.powf(exponent),
        Curve::Log => (1.0 + 9.0 * x).ln() / 10.0_f32.ln(),
        Curve::Step { threshold } => {
            if x >= threshold {
                1.0
            } else {
                0.0
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A preset validation error, locating the offending file and position.
///
/// Its [`Display`](fmt::Display) is `<file or "<preset>">:<line>:<col>:
/// <message>`; the line and column are omitted only when the position is truly
/// unknown (an IO error, or a missing required key).
#[derive(Clone, Debug)]
pub struct PresetError {
    /// The file the preset was read from, if any.
    pub file: Option<PathBuf>,
    /// The 1-based line of the error, if known.
    pub line: Option<usize>,
    /// The 1-based column of the error, if known.
    pub col: Option<usize>,
    /// What went wrong.
    pub kind: PresetErrorKind,
}

/// The specific cause of a [`PresetError`].
#[derive(Clone, Debug, PartialEq)]
pub enum PresetErrorKind {
    /// The file could not be read.
    Io(String),
    /// The source was not valid TOML.
    Syntax(String),
    /// A key that is not allowed in its table.
    UnknownKey {
        /// The table the key appeared in.
        table: String,
        /// The offending key.
        key: String,
        /// The keys that would have been accepted.
        known: Vec<String>,
    },
    /// A value had the wrong type.
    TypeMismatch {
        /// The key whose value was wrong.
        key: String,
        /// The expected type.
        expected: String,
        /// The type actually found.
        found: String,
    },
    /// A numeric value fell outside its allowed range.
    OutOfRange {
        /// The key whose value was out of range.
        key: String,
        /// The offending value.
        value: f64,
        /// The inclusive minimum.
        min: f32,
        /// The inclusive maximum.
        max: f32,
    },
    /// A scene id that is not registered.
    UnknownScene {
        /// The offending id.
        id: String,
        /// The registered scene ids.
        known: Vec<String>,
    },
    /// A feature name that is not part of the feature vocabulary.
    UnknownFeature {
        /// The offending name.
        name: String,
    },
    /// A string `[map]` expression failed to compile: an invalid syntax or a
    /// reference to a name outside the expression vocabulary.
    ExpressionInvalid {
        /// The mapping key.
        key: String,
        /// What was wrong with the expression.
        message: String,
    },
    /// The palette was malformed (wrong slot count or a bad `#rrggbb` entry).
    PaletteShape {
        /// A human description of the problem.
        message: String,
    },
    /// A preset name that does not match `^[a-z0-9][a-z0-9-]*$`.
    InvalidName {
        /// The offending name.
        name: String,
    },
}

impl fmt::Display for PresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let loc = match &self.file {
            Some(p) => p.display().to_string(),
            None => "<preset>".to_string(),
        };
        match (self.line, self.col) {
            (Some(line), Some(col)) => write!(f, "{loc}:{line}:{col}: {}", self.kind),
            _ => write!(f, "{loc}: {}", self.kind),
        }
    }
}

impl std::error::Error for PresetError {}

impl fmt::Display for PresetErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "{msg}"),
            Self::Syntax(msg) => write!(f, "{msg}"),
            Self::UnknownKey { table, key, known } => {
                write!(
                    f,
                    "unknown key `{key}` in [{table}] (known: {})",
                    join_known(known)
                )
            }
            Self::TypeMismatch {
                key,
                expected,
                found,
            } => write!(f, "`{key}`: expected {expected}, found {found}"),
            Self::OutOfRange {
                key,
                value,
                min,
                max,
            } => write!(f, "`{key}` = {value} is out of range [{min}, {max}]"),
            Self::UnknownScene { id, known } => {
                write!(f, "unknown scene `{id}` (known: {})", join_known(known))
            }
            Self::UnknownFeature { name } => write!(f, "unknown feature `{name}`"),
            Self::ExpressionInvalid { key, message } => write!(f, "`{key}`: {message}"),
            Self::PaletteShape { message } => write!(f, "{message}"),
            Self::InvalidName { name } => write!(
                f,
                "invalid preset name `{name}`; must match ^[a-z0-9][a-z0-9-]*$"
            ),
        }
    }
}

fn join_known(known: &[String]) -> String {
    known
        .iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Raw (source-parsed) schema
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDoc {
    preset: RawPreset,
    #[serde(default)]
    params: BTreeMap<String, Spanned<toml::Value>>,
    #[serde(default)]
    layer: Vec<RawLayer>,
    #[serde(default)]
    map: BTreeMap<String, Spanned<toml::Value>>,
    #[serde(default)]
    palette: Option<RawPalette>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPreset {
    name: Spanned<String>,
    scene: Spanned<String>,
    description: Option<String>,
    mood: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayer {
    scene: Spanned<String>,
    blend: Option<Spanned<String>>,
    intensity: Option<Spanned<f64>>,
    #[serde(default)]
    params: BTreeMap<String, Spanned<toml::Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPalette {
    source: Spanned<String>,
    slots: Option<Vec<Spanned<String>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMapEntry {
    feature: String,
    curve: Option<String>,
    exponent: Option<f64>,
    threshold: Option<f64>,
    attack_ms: Option<f64>,
    decay_ms: Option<f64>,
    scale: Option<f64>,
    offset: Option<f64>,
}

/// The keys a `[map]` entry table accepts (for unknown-key errors).
const MAP_ENTRY_KEYS: &[&str] = &[
    "feature",
    "curve",
    "exponent",
    "threshold",
    "attack_ms",
    "decay_ms",
    "scale",
    "offset",
];

// ---------------------------------------------------------------------------
// Parsing / validation
// ---------------------------------------------------------------------------

/// Parse and validate a preset from source text. `file` is used only to name
/// the source in errors.
///
/// # Errors
///
/// Returns a [`PresetError`] for any syntax or validation failure; its
/// [`Display`](fmt::Display) names the file and, where known, the line and
/// column.
pub fn parse_preset(text: &str, file: Option<&Path>) -> Result<Preset, PresetError> {
    let raw: RawDoc = toml::from_str(text).map_err(|e| map_toml_err(&e, text, file))?;
    validate(raw, text, file)
}

/// Read, parse and validate a preset file.
///
/// # Errors
///
/// Returns a [`PresetError`] if the file cannot be read or fails validation.
pub fn load_preset(path: &Path) -> Result<Preset, PresetError> {
    let text = std::fs::read_to_string(path).map_err(|e| PresetError {
        file: Some(path.to_path_buf()),
        line: None,
        col: None,
        kind: PresetErrorKind::Io(e.to_string()),
    })?;
    parse_preset(&text, Some(path))
}

fn validate(raw: RawDoc, src: &str, file: Option<&Path>) -> Result<Preset, PresetError> {
    // Preset name.
    let name = raw.preset.name.get_ref().clone();
    if !valid_name(&name) {
        return Err(err_at(
            file,
            src,
            raw.preset.name.span(),
            PresetErrorKind::InvalidName { name },
        ));
    }

    // Preset scene.
    let scene = raw.preset.scene.get_ref().clone();
    let info = scene_info(&scene).ok_or_else(|| {
        err_at(
            file,
            src,
            raw.preset.scene.span(),
            PresetErrorKind::UnknownScene {
                id: scene.clone(),
                known: known_scenes(),
            },
        )
    })?;

    let mood = raw.preset.mood.unwrap_or_else(|| info.mood.to_string());

    // Top-level [params], typed against the preset scene manifest.
    let params_overlay = validate_params(&raw.params, "params", info.params, src, file)?;
    let params = merge_params(info.params, &[&params_overlay]);

    // Layers.
    let mut layers = Vec::with_capacity(raw.layer.len());
    for raw_layer in &raw.layer {
        let l_scene = raw_layer.scene.get_ref().clone();
        let l_info = scene_info(&l_scene).ok_or_else(|| {
            err_at(
                file,
                src,
                raw_layer.scene.span(),
                PresetErrorKind::UnknownScene {
                    id: l_scene.clone(),
                    known: known_scenes(),
                },
            )
        })?;
        let blend = match &raw_layer.blend {
            None => Blend::Over,
            Some(s) => parse_blend(s.get_ref()).ok_or_else(|| {
                err_at(
                    file,
                    src,
                    s.span(),
                    PresetErrorKind::TypeMismatch {
                        key: "blend".to_string(),
                        expected: "one of \"over\", \"add\", \"max\"".to_string(),
                        found: format!("\"{}\"", s.get_ref()),
                    },
                )
            })?,
        };
        let intensity = match &raw_layer.intensity {
            None => 1.0,
            Some(s) => {
                let v = *s.get_ref();
                range_check("intensity", v, 0.0, 1.0, s.span(), src, file)?;
                v as f32
            }
        };
        let l_overlay =
            validate_params(&raw_layer.params, "layer.params", l_info.params, src, file)?;
        layers.push(Layer {
            scene: l_scene,
            blend,
            intensity,
            params: l_overlay,
        });
    }

    // Mappings target the first layer's scene, or the preset scene when there
    // are no explicit layers.
    let map_manifest = layers
        .first()
        .and_then(|l| scene_info(&l.scene))
        .map_or(info.params, |i| i.params);
    let mappings = validate_mappings(&raw.map, map_manifest, src, file)?;

    // Palette.
    let (palette_source, palette) = validate_palette(raw.palette.as_ref(), src, file)?;

    Ok(Preset {
        name,
        scene,
        description: raw.preset.description,
        mood,
        params,
        layers,
        mappings,
        palette_source,
        params_overlay,
        palette,
    })
}

/// Validate a `[params]` / `[layer.params]` table against a scene manifest,
/// returning the explicit overlay entries.
fn validate_params(
    raw: &BTreeMap<String, Spanned<toml::Value>>,
    table: &str,
    manifest: &[ParamSpec],
    src: &str,
    file: Option<&Path>,
) -> Result<Vec<(String, f32)>, PresetError> {
    let mut out = Vec::with_capacity(raw.len());
    for (key, spanned) in raw {
        let spec = manifest.iter().find(|s| s.key == key).ok_or_else(|| {
            err_at(
                file,
                src,
                spanned.span(),
                PresetErrorKind::UnknownKey {
                    table: table.to_string(),
                    key: key.clone(),
                    known: manifest.iter().map(|s| s.key.to_string()).collect(),
                },
            )
        })?;
        let v = as_number(spanned.get_ref()).ok_or_else(|| {
            err_at(
                file,
                src,
                spanned.span(),
                PresetErrorKind::TypeMismatch {
                    key: key.clone(),
                    expected: "number".to_string(),
                    found: value_type(spanned.get_ref()).to_string(),
                },
            )
        })?;
        range_check(key, v, spec.min, spec.max, spanned.span(), src, file)?;
        out.push((key.clone(), v as f32));
    }
    Ok(out)
}

/// Validate the `[map]` table against the target scene manifest.
fn validate_mappings(
    raw: &BTreeMap<String, Spanned<toml::Value>>,
    manifest: &[ParamSpec],
    src: &str,
    file: Option<&Path>,
) -> Result<Vec<MapEntry>, PresetError> {
    let mut out = Vec::with_capacity(raw.len());
    for (key, spanned) in raw {
        let span = spanned.span();
        // The target must be a manifest key of the mapped scene.
        if !manifest.iter().any(|s| s.key == key) {
            return Err(err_at(
                file,
                src,
                span,
                PresetErrorKind::UnknownKey {
                    table: "map".to_string(),
                    key: key.clone(),
                    known: manifest.iter().map(|s| s.key.to_string()).collect(),
                },
            ));
        }
        match spanned.get_ref() {
            // A string value is an expression: compile it now so a syntax error
            // or an unknown variable fails here, at load, at this entry's span.
            toml::Value::String(source) => {
                let expr = CompiledExpr::compile(source)
                    .map_err(|e| expr_compile_err(&e, key, span.clone(), src, file))?;
                out.push(MapEntry::Expr(ExprMapping {
                    target: key.clone(),
                    expr,
                }));
            }
            toml::Value::Table(_) => {
                let entry: RawMapEntry = spanned
                    .get_ref()
                    .clone()
                    .try_into()
                    .map_err(|e| map_entry_err(&e, key, span.clone(), src, file))?;
                out.push(MapEntry::Table(build_mapping(
                    key, &entry, span, src, file,
                )?));
            }
            other => {
                return Err(err_at(
                    file,
                    src,
                    span,
                    PresetErrorKind::TypeMismatch {
                        key: key.clone(),
                        expected: "mapping table or expression string".to_string(),
                        found: value_type(other).to_string(),
                    },
                ));
            }
        }
    }
    Ok(out)
}

/// Turn an [`ExprCompileError`] into a positioned [`PresetError`] at the map
/// entry's span, matching the message conventions of the other error classes.
fn expr_compile_err(
    err: &ExprCompileError,
    key: &str,
    span: Range<usize>,
    src: &str,
    file: Option<&Path>,
) -> PresetError {
    let message = match err {
        ExprCompileError::Syntax(msg) => format!("invalid expression: {msg}"),
        ExprCompileError::UnknownVar(name) => format!(
            "unknown variable `{name}` in expression (known: {})",
            join_known(
                &EXPR_VARS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect::<Vec<_>>()
            )
        ),
    };
    err_at(
        file,
        src,
        span,
        PresetErrorKind::ExpressionInvalid {
            key: key.to_string(),
            message,
        },
    )
}

fn build_mapping(
    key: &str,
    entry: &RawMapEntry,
    span: Range<usize>,
    src: &str,
    file: Option<&Path>,
) -> Result<Mapping, PresetError> {
    let feature = parse_feature(&entry.feature).ok_or_else(|| {
        err_at(
            file,
            src,
            span.clone(),
            PresetErrorKind::UnknownFeature {
                name: entry.feature.clone(),
            },
        )
    })?;

    let curve_name = entry.curve.as_deref().unwrap_or("linear");
    let curve = match curve_name {
        "linear" => Curve::Linear,
        "log" => Curve::Log,
        "pow" => {
            let exponent = entry.exponent.ok_or_else(|| {
                err_at(
                    file,
                    src,
                    span.clone(),
                    PresetErrorKind::TypeMismatch {
                        key: format!("{key}.exponent"),
                        expected: "a number (required for curve = pow)".to_string(),
                        found: "nothing".to_string(),
                    },
                )
            })?;
            if exponent <= 0.0 || exponent.is_nan() {
                return Err(err_at(
                    file,
                    src,
                    span.clone(),
                    PresetErrorKind::OutOfRange {
                        key: format!("{key}.exponent"),
                        value: exponent,
                        min: f32::MIN_POSITIVE,
                        max: f32::INFINITY,
                    },
                ));
            }
            Curve::Pow {
                exponent: exponent as f32,
            }
        }
        "step" => {
            let threshold = entry.threshold.ok_or_else(|| {
                err_at(
                    file,
                    src,
                    span.clone(),
                    PresetErrorKind::TypeMismatch {
                        key: format!("{key}.threshold"),
                        expected: "a number (required for curve = step)".to_string(),
                        found: "nothing".to_string(),
                    },
                )
            })?;
            range_check(
                &format!("{key}.threshold"),
                threshold,
                0.0,
                1.0,
                span.clone(),
                src,
                file,
            )?;
            Curve::Step {
                threshold: threshold as f32,
            }
        }
        other => {
            return Err(err_at(
                file,
                src,
                span.clone(),
                PresetErrorKind::TypeMismatch {
                    key: format!("{key}.curve"),
                    expected: "one of \"linear\", \"pow\", \"log\", \"step\"".to_string(),
                    found: format!("\"{other}\""),
                },
            ));
        }
    };

    let attack_ms = entry.attack_ms.unwrap_or(0.0);
    let decay_ms = entry.decay_ms.unwrap_or(0.0);
    if attack_ms < 0.0 {
        return Err(err_at(
            file,
            src,
            span.clone(),
            PresetErrorKind::OutOfRange {
                key: format!("{key}.attack_ms"),
                value: attack_ms,
                min: 0.0,
                max: f32::INFINITY,
            },
        ));
    }
    if decay_ms < 0.0 {
        return Err(err_at(
            file,
            src,
            span,
            PresetErrorKind::OutOfRange {
                key: format!("{key}.decay_ms"),
                value: decay_ms,
                min: 0.0,
                max: f32::INFINITY,
            },
        ));
    }

    Ok(Mapping {
        target: key.to_string(),
        feature,
        curve,
        attack_ms: attack_ms as f32,
        decay_ms: decay_ms as f32,
        scale: entry.scale.unwrap_or(1.0) as f32,
        offset: entry.offset.unwrap_or(0.0) as f32,
    })
}

fn validate_palette(
    raw: Option<&RawPalette>,
    src: &str,
    file: Option<&Path>,
) -> Result<(PaletteSource, Palette), PresetError> {
    let Some(raw) = raw else {
        return Ok((PaletteSource::Static, Palette::default_dark()));
    };
    let source = match raw.source.get_ref().as_str() {
        "static" => PaletteSource::Static,
        "album-art" => PaletteSource::AlbumArt,
        other => {
            return Err(err_at(
                file,
                src,
                raw.source.span(),
                PresetErrorKind::PaletteShape {
                    message: format!(
                        "unknown palette source `{other}`; expected `static` or `album-art`"
                    ),
                },
            ));
        }
    };

    let mut palette = Palette::default_dark();
    if let Some(slots) = &raw.slots {
        if slots.len() != crate::PALETTE_SLOTS {
            // Locate the error at the first slot, or the source line if empty.
            let span = slots
                .first()
                .map_or_else(|| raw.source.span(), toml::Spanned::span);
            return Err(err_at(
                file,
                src,
                span,
                PresetErrorKind::PaletteShape {
                    message: format!(
                        "palette must have exactly {} slots, found {}",
                        crate::PALETTE_SLOTS,
                        slots.len()
                    ),
                },
            ));
        }
        let mut rgb = [Rgb(0, 0, 0); crate::PALETTE_SLOTS];
        for (i, slot) in slots.iter().enumerate() {
            rgb[i] = parse_hex(slot.get_ref()).ok_or_else(|| {
                err_at(
                    file,
                    src,
                    slot.span(),
                    PresetErrorKind::PaletteShape {
                        message: format!(
                            "palette slot `{}` is not a \"#rrggbb\" colour",
                            slot.get_ref()
                        ),
                    },
                )
            })?;
        }
        // Slots only theme a `static` palette; an `album-art` preset validates
        // them but resolves to the host default until the album-art card lands.
        if source == PaletteSource::Static {
            palette = Palette { slots: rgb };
        }
    }

    Ok((source, palette))
}

impl Preset {
    /// The resolved palette (from the preset's `slots`, or the host default).
    #[must_use]
    pub fn palette(&self) -> Palette {
        self.palette
    }

    /// A synthesized, layerless preset that runs the scene described by `info`
    /// with its manifest defaults and `palette`. This is how a Luau scene reaches
    /// the `--scene`/browser path, which is built around presets: it carries no
    /// `[map]` mappings and one implicit layer (the scene itself). `info` is
    /// typically a Luau [`SceneInfo`] from the catalog, but the shape works for
    /// any registered scene.
    #[must_use]
    pub fn for_scene(info: &crate::registry::SceneInfo, palette: Palette) -> Self {
        let params = merge_params(info.params, &[]);
        Self {
            name: info.id.to_string(),
            scene: info.id.to_string(),
            description: Some(info.summary.to_string()),
            mood: info.mood.to_string(),
            params,
            layers: Vec::new(),
            mappings: Vec::new(),
            palette_source: PaletteSource::Static,
            params_overlay: Vec::new(),
            palette,
        }
    }

    /// Instantiate the preset into its live layers at the given aspect ratio.
    ///
    /// A layerless preset yields exactly one layer (the preset scene with the
    /// merged `[params]`); a preset with `[[layer]]`s yields one per layer, each
    /// scene created through the registry and initialized with the merged
    /// parameters (manifest default < `[params]` < `[layer.params]`). The
    /// `[map]` mappings ride the first layer.
    #[must_use]
    pub fn instantiate(&self, aspect: f32) -> Vec<LayerInstance> {
        if self.layers.is_empty() {
            let info = scene_info(&self.scene);
            let params = merge_params(info.map_or(&[], |i| i.params), &[&self.params_overlay]);
            return vec![self.make_layer(
                &self.scene,
                Blend::Over,
                1.0,
                params,
                &self.mappings,
                aspect,
            )];
        }

        self.layers
            .iter()
            .enumerate()
            .map(|(i, layer)| {
                let info = scene_info(&layer.scene);
                let params = merge_params(
                    info.map_or(&[], |i| i.params),
                    &[&self.params_overlay, &layer.params],
                );
                let mappings: &[MapEntry] = if i == 0 { &self.mappings } else { &[] };
                self.make_layer(
                    &layer.scene,
                    layer.blend,
                    layer.intensity,
                    params,
                    mappings,
                    aspect,
                )
            })
            .collect()
    }

    fn make_layer(
        &self,
        scene_id: &str,
        blend: Blend,
        intensity: f32,
        params: Params,
        mappings: &[MapEntry],
        aspect: f32,
    ) -> LayerInstance {
        // A layer scene is a built-in or a discovered Luau scene; fall back to
        // the preset's own scene (then to a Luau scene of that id) so a
        // synthesized Luau preset instantiates its scripted scene.
        let mut scene = create_builtin(scene_id)
            .or_else(|| crate::luau::catalog::create_luau(scene_id))
            .or_else(|| create_builtin(&self.scene))
            .or_else(|| crate::luau::catalog::create_luau(&self.scene))
            .expect("preset scene is registered");
        let ctx = SceneCtx::new(aspect, self.palette, params);
        scene.init(&ctx);
        LayerInstance {
            scene,
            blend,
            intensity,
            mappings: MappingSet::from_entries(mappings),
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in presets
// ---------------------------------------------------------------------------

/// The `spectra` built-in preset template.
static SPECTRA_PRESET: &str = include_str!("../../../../presets/spectra.toml");

/// The `lattice` built-in preset template.
static LATTICE_PRESET: &str = include_str!("../../../../presets/lattice.toml");

/// The `aurora` built-in preset template.
static AURORA_PRESET: &str = include_str!("../../../../presets/aurora.toml");

/// The `starfall` built-in preset template.
static STARFALL_PRESET: &str = include_str!("../../../../presets/starfall.toml");

/// The `tide` built-in preset template.
static TIDE_PRESET: &str = include_str!("../../../../presets/tide.toml");

/// The `verso` built-in preset template.
static VERSO_PRESET: &str = include_str!("../../../../presets/verso.toml");

/// The `phosphor` built-in preset template.
static PHOSPHOR_PRESET: &str = include_str!("../../../../presets/phosphor.toml");

/// The `sonar` built-in preset template.
static SONAR_PRESET: &str = include_str!("../../../../presets/sonar.toml");

/// The `ember-drift` built-in preset template.
static EMBER_DRIFT_PRESET: &str = include_str!("../../../../presets/ember-drift.toml");

/// The `bloom` built-in preset template.
static BLOOM_PRESET: &str = include_str!("../../../../presets/bloom.toml");

/// The built-in preset files compiled into the crate, as `(name, source)`.
static BUILTIN_PRESETS: &[(&str, &str)] = &[
    ("spectra", SPECTRA_PRESET),
    ("lattice", LATTICE_PRESET),
    ("aurora", AURORA_PRESET),
    ("starfall", STARFALL_PRESET),
    ("tide", TIDE_PRESET),
    ("verso", VERSO_PRESET),
    ("phosphor", PHOSPHOR_PRESET),
    ("sonar", SONAR_PRESET),
    ("ember-drift", EMBER_DRIFT_PRESET),
    ("bloom", BLOOM_PRESET),
];

/// The built-in preset files compiled into the crate, as `(name, source)`.
#[must_use]
pub fn builtin_presets() -> &'static [(&'static str, &'static str)] {
    BUILTIN_PRESETS
}

/// Parse a built-in preset by name, or `None` if there is no such preset.
#[must_use]
pub fn builtin_preset(name: &str) -> Option<Result<Preset, PresetError>> {
    builtin_presets()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, src)| parse_preset(src, None))
}

/// The expression vocabulary: the feature/signal names a `[map]` expression may
/// reference (`bass`, `mid`, `treb`, `loud`, `onset`, `beat`, `width`, …).
///
/// Exposed so a source-authoring view can offer did-you-mean hints against the
/// same canonical list the expression compiler validates against.
#[must_use]
pub fn expression_vocabulary() -> &'static [&'static str] {
    EXPR_VARS
}

/// The language a scene's source is written in, so a source viewer can label it
/// — and a future syntax-aware view can branch on it.
///
/// Every preset is TOML today; the Luau scripting rung adds [`Lua`](Self::Lua)
/// with no change to a viewer that reads a [`SceneSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A TOML preset (`.toml`).
    Toml,
    /// A Luau script (`.lua`).
    Lua,
}

impl SourceKind {
    /// Infer the kind from a path's extension: a `.lua` extension (any case) is
    /// [`Lua`](Self::Lua); everything else (a `.toml` preset, or no extension)
    /// is [`Toml`](Self::Toml).
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("lua") => Self::Lua,
            _ => Self::Toml,
        }
    }

    /// A short lowercase label for a pane header (`"toml"` / `"lua"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Toml => "toml",
            Self::Lua => "lua",
        }
    }
}

/// A descriptor of where the active scene's source lives, for a source viewer
/// (scene-author mode) to show.
///
/// It carries the source language and a short display label, plus — depending on
/// where the source lives — either an on-disk [`path`](Self::path) the viewer
/// reads live (a `--scene-file`) or the embedded [`text`](Self::text) of a
/// built-in preset compiled into the binary. A viewer is generic over it, so an
/// on-disk TOML preset, an embedded built-in, and a future Luau script are all
/// shown the same way.
#[derive(Clone, Debug)]
pub struct SceneSource {
    /// The on-disk path, when the source is a file (a `--scene-file`); `None`
    /// for a built-in preset compiled into the binary. A viewer reads a path
    /// source live, so a live reload shows the newly saved bytes.
    pub path: Option<PathBuf>,
    /// The source language.
    pub kind: SourceKind,
    /// A short human label for a pane header — the file name, or
    /// `<name> (built-in)` for an embedded preset.
    pub label: String,
    /// The embedded source text for a built-in preset. Empty for a file source,
    /// which a viewer reads live from [`path`](Self::path).
    pub text: String,
}

impl SceneSource {
    /// Describe an on-disk `--scene-file`: infer the kind from the extension and
    /// label it by file name. The text is left empty — a viewer reads a file
    /// source live from its path, so a reload shows the current bytes.
    #[must_use]
    pub fn from_file(path: &Path) -> Self {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path: Some(path.to_path_buf()),
            kind: SourceKind::from_path(path),
            label,
            text: String::new(),
        }
    }

    /// Describe a shipped (embedded) Luau scene by name, carrying its bundled
    /// `.lua` source text. Read-only and unwatched — there is no drop-in file —
    /// labeled like a built-in preset (`<name> (built-in)`), so scene-author mode
    /// shows a shipped scene the same way it shows a compiled-in TOML preset. A
    /// drop-in Luau scene has a file instead and uses [`from_file`](Self::from_file).
    #[must_use]
    pub fn luau_builtin(name: &str, source: &str) -> Self {
        Self {
            path: None,
            kind: SourceKind::Lua,
            label: format!("{name} (built-in)"),
            text: source.to_string(),
        }
    }

    /// Describe a built-in preset by name, carrying its embedded TOML source.
    /// `None` when `name` is not a built-in preset.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        builtin_presets()
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(n, src)| Self {
                path: None,
                kind: SourceKind::Toml,
                label: format!("{n} (built-in)"),
                text: (*src).to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn known_scenes() -> Vec<String> {
    crate::builtin_scenes()
        .iter()
        .map(|s| s.id.to_string())
        .collect()
}

/// Seed a fresh [`Params`] with `manifest` defaults, then apply each overlay in
/// order (later overlays win).
fn merge_params(manifest: &[ParamSpec], overlays: &[&[(String, f32)]]) -> Params {
    let mut p = Params::new();
    for spec in manifest {
        p.set(spec.key, spec.default);
    }
    for overlay in overlays {
        for (k, v) in *overlay {
            p.set(k, *v);
        }
    }
    p
}

fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn parse_blend(s: &str) -> Option<Blend> {
    match s {
        "over" => Some(Blend::Over),
        "add" => Some(Blend::Add),
        "max" => Some(Blend::Max),
        _ => None,
    }
}

fn parse_feature(s: &str) -> Option<Feature> {
    Some(match s {
        "bass" => Feature::Bass,
        "mid" => Feature::Mid,
        "treb" => Feature::Treb,
        "loud" => Feature::Loud,
        "peak" => Feature::Peak,
        "onset" => Feature::Onset,
        "flux" => Feature::Flux,
        "beat" => Feature::Beat,
        "width" => Feature::Width,
        _ => return None,
    })
}

fn as_number(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Integer(i) => Some(*i as f64),
        toml::Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn value_type(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

fn range_check(
    key: &str,
    value: f64,
    min: f32,
    max: f32,
    span: Range<usize>,
    src: &str,
    file: Option<&Path>,
) -> Result<(), PresetError> {
    if value < min as f64 || value > max as f64 {
        return Err(err_at(
            file,
            src,
            span,
            PresetErrorKind::OutOfRange {
                key: key.to_string(),
                value,
                min,
                max,
            },
        ));
    }
    Ok(())
}

/// Build a positioned error from a byte span.
fn err_at(
    file: Option<&Path>,
    src: &str,
    span: Range<usize>,
    kind: PresetErrorKind,
) -> PresetError {
    let (line, col) = line_col(src, span.start);
    PresetError {
        file: file.map(Path::to_path_buf),
        line: Some(line),
        col: Some(col),
        kind,
    }
}

/// 1-based line and column of a byte offset in `src`.
fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let mut line = 1usize;
    let mut col = 1usize;
    for &b in &src.as_bytes()[..offset] {
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Map a top-level `toml::de::Error` to a [`PresetError`], recognizing the
/// unknown-field case so it becomes a structured [`PresetErrorKind::UnknownKey`].
fn map_toml_err(err: &toml::de::Error, src: &str, file: Option<&Path>) -> PresetError {
    let span = err.span();
    let (line, col) = match &span {
        Some(s) => {
            let (l, c) = line_col(src, s.start);
            (Some(l), Some(c))
        }
        None => (None, None),
    };
    let message = err.message();
    let kind = if let Some(key) = unknown_field_key(message) {
        let table = span
            .as_ref()
            .and_then(|s| enclosing_table(src, s.start))
            .unwrap_or_else(|| "preset".to_string());
        PresetErrorKind::UnknownKey {
            table,
            key,
            known: unknown_field_known(message),
        }
    } else {
        PresetErrorKind::Syntax(message.to_string())
    };
    PresetError {
        file: file.map(Path::to_path_buf),
        line,
        col,
        kind,
    }
}

/// Map an error from deserializing a `[map]` entry table, attaching the entry's
/// span (value deserialization carries no inner span of its own).
fn map_entry_err(
    err: &toml::de::Error,
    key: &str,
    span: Range<usize>,
    src: &str,
    file: Option<&Path>,
) -> PresetError {
    let message = err.message();
    let kind = if let Some(field) = unknown_field_key(message) {
        PresetErrorKind::UnknownKey {
            table: format!("map.{key}"),
            key: field,
            known: MAP_ENTRY_KEYS.iter().map(|s| (*s).to_string()).collect(),
        }
    } else {
        PresetErrorKind::Syntax(message.to_string())
    };
    err_at(file, src, span, kind)
}

/// If `message` is serde's "unknown field `X`, ..." error, return `X`.
fn unknown_field_key(message: &str) -> Option<String> {
    let rest = message.strip_prefix("unknown field `")?;
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// Extract the backtick-quoted known keys from an "unknown field" message.
fn unknown_field_known(message: &str) -> Vec<String> {
    let Some(idx) = message.find("expected") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = &message[idx..];
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        if let Some(close) = rest.find('`') {
            out.push(rest[..close].to_string());
            rest = &rest[close + 1..];
        } else {
            break;
        }
    }
    out
}

/// Find the name of the `[table]` or `[[table]]` header enclosing a byte
/// offset, by scanning backwards from the offset's line.
fn enclosing_table(src: &str, offset: usize) -> Option<String> {
    let offset = offset.min(src.len());
    let prefix = &src[..offset];
    for line in prefix.lines().rev() {
        let trimmed = line.trim_start();
        if let Some(name) = trimmed
            .strip_prefix("[[")
            .and_then(|r| r.split_once("]]"))
            .map(|(n, _)| n)
            .or_else(|| {
                trimmed
                    .strip_prefix('[')
                    .and_then(|r| r.split_once(']'))
                    .map(|(n, _)| n)
            })
        {
            return Some(name.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod runtime_api_tests {
    use super::*;

    #[test]
    fn compile_builds_and_rejects_expression_mappings() {
        // A valid expression compiles and drives its target.
        let m = ExprMapping::compile("gap", "bass * 0.5").expect("valid");
        assert_eq!(m.target, "gap");
        assert_eq!(m.source(), "bass * 0.5");
        // An unknown variable is rejected with a short message.
        let err = ExprMapping::compile("gap", "nope").expect_err("unknown var");
        assert!(err.message().contains("unknown variable"), "{err}");
        // A syntax error is rejected too.
        assert!(ExprMapping::compile("gap", "bass *").is_err());
    }

    #[test]
    fn entries_view_round_trips_table_and_expression_rows() {
        let table = Mapping {
            target: "trail".into(),
            feature: Feature::Loud,
            curve: Curve::Linear,
            attack_ms: 100.0,
            decay_ms: 400.0,
            scale: 0.7,
            offset: 0.2,
        };
        let expr = ExprMapping::compile("gap", "onset * 0.5").expect("valid");
        let set = MappingSet::from_entries(&[
            MapEntry::Table(table.clone()),
            MapEntry::Expr(expr.clone()),
        ]);

        let view = set.entries_view();
        assert_eq!(view.len(), 2);
        match &view[0] {
            MapEntry::Table(m) => {
                assert_eq!(m.target, "trail");
                assert_eq!(m.feature, Feature::Loud);
                assert!((m.attack_ms - 100.0).abs() < 1e-3);
                assert!((m.decay_ms - 400.0).abs() < 1e-3);
                assert!((m.scale - 0.7).abs() < 1e-6);
                assert!((m.offset - 0.2).abs() < 1e-6);
            }
            MapEntry::Expr(_) => panic!("row 0 is a table"),
        }
        match &view[1] {
            MapEntry::Expr(e) => assert_eq!(e.source(), "onset * 0.5"),
            MapEntry::Table(_) => panic!("row 1 is an expression"),
        }
    }

    #[test]
    fn replace_swaps_one_row_in_place() {
        let mut set = MappingSet::from_entries(&[
            MapEntry::Table(Mapping {
                target: "gap".into(),
                feature: Feature::Bass,
                curve: Curve::Linear,
                attack_ms: 0.0,
                decay_ms: 0.0,
                scale: 1.0,
                offset: 0.0,
            }),
            MapEntry::Expr(ExprMapping::compile("punch", "onset").expect("valid")),
        ]);

        // Replace the table row with an expression; order and the other row hold.
        let ok = set.replace(MapEntry::Expr(
            ExprMapping::compile("gap", "treb * 2").expect("valid"),
        ));
        assert!(ok, "the matching row is replaced");
        let view = set.entries_view();
        assert_eq!(view[0].target(), "gap");
        match &view[0] {
            MapEntry::Expr(e) => assert_eq!(e.source(), "treb * 2"),
            MapEntry::Table(_) => panic!("gap became an expression"),
        }
        assert_eq!(view[1].target(), "punch", "the other row is untouched");

        // A target present in no row leaves the set unchanged.
        assert!(!set.replace(MapEntry::Expr(
            ExprMapping::compile("absent", "bass").expect("valid")
        )));
        assert_eq!(set.entries_view().len(), 2);
    }
}
