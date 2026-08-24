//! The [`Scene`] trait — the TUI↔GPU portability seam — and its supporting
//! context and state types.
//!
//! A scene is a small stateful object driven once per frame: [`Scene::update`]
//! folds the newest [`scia_core::FeatureSnapshot`] into the scene's internal
//! state, then [`Scene::render`] emits a [`crate::Canvas`] display list from
//! that state. Because the canvas is abstract and normalized, the same scene
//! drives a terminal presenter and a GPU presenter unchanged.
//!
//! [`Scene::state`] and [`Scene::restore`] carry a scene's continuity across a
//! hot reload: the host snapshots the state, swaps the scene object (for example
//! after re-reading a preset) and restores it, so animation does not visibly
//! reset. Scenes decide what is worth carrying; anything that re-settles within
//! a frame can be left out.

use crate::canvas::Canvas;
use crate::palette::Palette;

/// One tunable parameter a scene exposes, with its default and valid range.
///
/// A scene's parameters form a `&'static [ParamSpec]` manifest on its
/// [`crate::SceneInfo`]. Presets are typed against this manifest: a `[params]`
/// key must be one of these `key`s, its value must be a number, and that number
/// must lie within `[min, max]`. `doc` is the one-line description surfaced in
/// the preset reference and the built-in preset template.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    /// The parameter name, e.g. `"release"`.
    pub key: &'static str,
    /// The value used when a preset does not set the key.
    pub default: f32,
    /// Inclusive lower bound.
    pub min: f32,
    /// Inclusive upper bound.
    pub max: f32,
    /// A one-line human description.
    pub doc: &'static str,
}

/// The context handed to a scene at [`Scene::init`]: the drawing aspect ratio,
/// the host palette and the scene's typed parameters.
#[derive(Clone, Debug)]
pub struct SceneCtx {
    /// Aspect ratio (width / height in physical units) of the drawing surface.
    pub aspect: f32,
    /// The eight-slot palette the host fills (static now, album-art later).
    pub palette: Palette,
    /// Preset parameters (from TOML later).
    pub params: Params,
}

impl SceneCtx {
    /// Build a context from its parts.
    #[must_use]
    pub fn new(aspect: f32, palette: Palette, params: Params) -> Self {
        Self {
            aspect,
            palette,
            params,
        }
    }
}

impl Default for SceneCtx {
    fn default() -> Self {
        Self {
            aspect: 1.0,
            palette: Palette::default_dark(),
            params: Params::new(),
        }
    }
}

/// A small typed parameter bag for scene presets.
///
/// Parameters are `f32` scalars keyed by short names. Reads
/// ([`Params::get`], [`Params::get_or`]) never allocate; a new key allocates
/// once at [`Params::set`] time, which happens when a preset is loaded, not per
/// frame. Backed by a `Vec<(Box<str>, f32)>`: keys are few, so linear scan is
/// faster than a map and carries no hashing state.
#[derive(Clone, Debug, Default)]
pub struct Params {
    entries: Vec<(Box<str>, f32)>,
}

impl Params {
    /// An empty parameter bag.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Set `key` to `v`, overwriting any existing value.
    pub fn set(&mut self, key: &str, v: f32) {
        for entry in &mut self.entries {
            if &*entry.0 == key {
                entry.1 = v;
                return;
            }
        }
        self.entries.push((Box::from(key), v));
    }

    /// The value for `key`, or `None` if unset.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<f32> {
        self.entries
            .iter()
            .find(|entry| &*entry.0 == key)
            .map(|entry| entry.1)
    }

    /// The value for `key`, or `default` if unset.
    #[must_use]
    pub fn get_or(&self, key: &str, default: f32) -> f32 {
        self.get(key).unwrap_or(default)
    }
}

/// A serializable snapshot of a scene's continuity, carried across a hot
/// reload by [`Scene::state`] / [`Scene::restore`].
///
/// The `values` bag is deliberately a plain list of `(name, scalar)` pairs so it
/// serializes trivially and stays forward-compatible: a restoring scene reads
/// only the keys it recognizes and ignores the rest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneState {
    /// Named scalar continuity values.
    pub values: Vec<(String, f32)>,
}

impl SceneState {
    /// An empty state bag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a named value.
    pub fn set(&mut self, key: &str, v: f32) {
        self.values.push((key.to_string(), v));
    }

    /// The first value stored under `key`, or `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<f32> {
        self.values.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }
}

/// A visualizer: it turns a stream of feature snapshots into a stream of
/// canvas display lists.
///
/// The lifecycle is `init` once, then `update` / `render` each frame. `update`
/// receives the newest features and the elapsed time `dt` in seconds; `render`
/// draws the current state onto a cleared canvas. `state` / `restore` are
/// optional and default to carrying nothing.
pub trait Scene: Send {
    /// A stable machine identifier, e.g. `"spectra"`.
    fn id(&self) -> &'static str;

    /// A one-word mood shown in the scene browser.
    fn mood(&self) -> &'static str;

    /// Prepare the scene against the drawing context and parameters.
    fn init(&mut self, ctx: &SceneCtx);

    /// Fold the newest features into internal state. `dt` is seconds elapsed
    /// since the previous `update`.
    fn update(&mut self, f: &scia_core::FeatureSnapshot, dt: f32);

    /// Emit the current frame onto `canvas`. The host clears the canvas before
    /// calling; the scene only pushes primitives.
    fn render(&mut self, canvas: &mut Canvas);

    /// Snapshot continuity for a hot reload. Defaults to carrying nothing.
    fn state(&self) -> SceneState {
        SceneState::default()
    }

    /// Restore continuity captured by [`Scene::state`]. Defaults to a no-op.
    fn restore(&mut self, _s: SceneState) {}
}
