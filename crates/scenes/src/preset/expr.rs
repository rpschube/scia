//! Expression `[map]` values (US-CFG-3): a mapping whose value is a string is a
//! small algebraic expression over the storyboard feature vocabulary. It is
//! compiled once at preset load with `fasteval`'s safe compiled-slab API and
//! evaluated per frame against the newest [`FeatureSnapshot`]; evaluation is
//! allocation-free after construction.
//!
//! The compiled program lives behind an [`Arc`] so cloning a mapping or a
//! [`MappingSet`](super::MappingSet) never rebuilds it and never allocates a new
//! slab.

use std::fmt;
use std::sync::Arc;

use fasteval::{Compiler, Evaler, Slab};

use scia_core::FeatureSnapshot;

/// The onset-envelope time constant, in seconds. Rather than a single-frame
/// spike, the `onset` variable is an exponential decay that reaches `1/e`
/// (~37 %) after this long, so an expression sees a usable envelope. ~250 ms.
pub(super) const ONSET_TAU: f32 = 0.25;

/// The storyboard variable vocabulary a `[map]` expression may reference. Any
/// other name is rejected at load. Kept in one place so the docs, the error
/// messages and the namespace lookup agree.
pub(super) const EXPR_VARS: &[&str] = &[
    "bass",
    "mid",
    "treb",
    "loud",
    "peak",
    "onset",
    "flux",
    "beat",
    "beat_conf",
    "width",
];

/// The variable values an expression reads, rebuilt each frame from the newest
/// [`FeatureSnapshot`] plus the maintained onset envelope.
///
/// Every value is clamped to `0.0..=1.0` except `width`, which is the raw
/// `stereo_correlation` field (published as `0.0` until the stereo card lands).
/// It is a plain `Copy` record, built on the stack per frame — no allocation.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ExprEnv {
    bass: f64,
    mid: f64,
    treb: f64,
    loud: f64,
    peak: f64,
    onset: f64,
    flux: f64,
    beat: f64,
    beat_conf: f64,
    width: f64,
}

impl ExprEnv {
    /// Build the namespace from a snapshot and the current onset envelope value.
    pub(super) fn from_snapshot(f: &FeatureSnapshot, onset_env: f32) -> Self {
        let c = |x: f32| f64::from(x.clamp(0.0, 1.0));
        Self {
            bass: c(f.bands[0]),
            mid: c(f.bands[1]),
            treb: c(f.bands[2]),
            // Engine-normalized loudness (`0..=1`, level-independent), not raw
            // rms — the same signal the builtin scenes read as `f.loudness`.
            loud: c(f.loudness),
            peak: c(f.peak),
            onset: f64::from(onset_env.clamp(0.0, 1.0)),
            flux: c(f.flux),
            beat: c(f.beat_phase),
            beat_conf: c(f.beat_confidence),
            // Raw field value on purpose (see module docs): it is `0.0` in
            // schema 1 and comes alive with the stereo card.
            width: f64::from(f.stereo_correlation),
        }
    }
}

impl fasteval::EvalNamespace for ExprEnv {
    /// Look up a variable. `args` is always empty for a bare variable, so this
    /// never touches `keybuf` and never allocates. An unknown name returns
    /// `None`, which `fasteval` turns into an eval error — surfaced at load by
    /// [`CompiledExpr::compile`]'s probe.
    #[inline]
    fn lookup(&mut self, name: &str, args: Vec<f64>, _keybuf: &mut String) -> Option<f64> {
        if !args.is_empty() {
            return None;
        }
        Some(match name {
            "bass" => self.bass,
            "mid" => self.mid,
            "treb" => self.treb,
            "loud" => self.loud,
            "peak" => self.peak,
            "onset" => self.onset,
            "flux" => self.flux,
            "beat" => self.beat,
            "beat_conf" => self.beat_conf,
            "width" => self.width,
            _ => return None,
        })
    }
}

/// Why compiling a `[map]` expression failed.
pub(super) enum ExprCompileError {
    /// The expression is not valid syntax.
    Syntax(String),
    /// The expression references a name outside the vocabulary.
    UnknownVar(String),
}

/// A compiled `[map]` expression: the `fasteval` parse/compile slab, the root
/// instruction, and the original source text (kept for `Debug`/`PartialEq`).
///
/// Shared behind an [`Arc`] by [`ExprMapping`](super::ExprMapping).
pub(super) struct CompiledExpr {
    slab: Slab,
    instr: fasteval::Instruction,
    source: String,
}

impl CompiledExpr {
    /// Parse and compile `source` against the expression vocabulary.
    ///
    /// Syntax errors surface from the parser; an unknown-variable reference —
    /// which `fasteval` only rejects at eval time — is caught here by probing
    /// one evaluation against a zero namespace. A valid expression that merely
    /// divides by zero evaluates to a non-finite number, not an error, so it
    /// compiles (and is sanitized to `0.0` per frame; see [`eval`]).
    ///
    /// [`eval`]: CompiledExpr::eval
    pub(super) fn compile(source: &str) -> Result<Arc<Self>, ExprCompileError> {
        let parser = fasteval::Parser::new();
        let mut slab = Slab::new();
        let instr = parser
            .parse(source, &mut slab.ps)
            .map_err(|e| ExprCompileError::Syntax(e.to_string()))?
            .from(&slab.ps)
            .compile(&slab.ps, &mut slab.cs);
        let compiled = Self {
            slab,
            instr,
            source: source.to_string(),
        };
        // Probe once against a zero namespace so an unknown variable fails now,
        // at load, rather than silently every frame at runtime.
        let mut env = ExprEnv::default();
        match compiled.instr.eval(&compiled.slab, &mut env) {
            Ok(_) => Ok(Arc::new(compiled)),
            Err(fasteval::Error::Undefined(name)) => Err(ExprCompileError::UnknownVar(name)),
            Err(e) => Err(ExprCompileError::Syntax(e.to_string())),
        }
    }

    /// Evaluate against `env`, returning the result as an `f32`. A non-finite
    /// result (e.g. a division by zero) is sanitized to `0.0` so it can never
    /// poison a scene parameter. Allocation-free.
    #[inline]
    pub(super) fn eval(&self, env: &mut ExprEnv) -> f32 {
        match self.instr.eval(&self.slab, env) {
            Ok(v) if v.is_finite() => v as f32,
            _ => 0.0,
        }
    }

    /// The original expression source.
    pub(super) fn source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for CompiledExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledExpr")
            .field("source", &self.source)
            .finish()
    }
}

impl PartialEq for CompiledExpr {
    /// Two compiled expressions are equal when their source text is equal.
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}
