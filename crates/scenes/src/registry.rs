//! The built-in scene registry: enumerate the scenes compiled into this crate
//! and construct one by id.

use crate::builtin::Spectra;
use crate::scene::Scene;

/// A catalog entry describing a built-in scene, for a browser to list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SceneInfo {
    /// The scene's stable identifier, passed to [`create_builtin`].
    pub id: &'static str,
    /// A one-word mood.
    pub mood: &'static str,
    /// A one-line human summary.
    pub summary: &'static str,
}

/// Every built-in scene, in listing order.
static BUILTINS: &[SceneInfo] = &[SceneInfo {
    id: "spectra",
    mood: "kinetic",
    summary: "The canonical analyzer: log-spaced bars whose low end punches on the onset envelope.",
}];

/// The catalog of built-in scenes.
#[must_use]
pub fn builtin_scenes() -> &'static [SceneInfo] {
    BUILTINS
}

/// Construct a built-in scene by id, or `None` if the id is unknown.
#[must_use]
pub fn create_builtin(id: &str) -> Option<Box<dyn Scene>> {
    match id {
        "spectra" => Some(Box::new(Spectra::new())),
        _ => None,
    }
}
