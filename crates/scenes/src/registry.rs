//! The built-in scene registry: enumerate the scenes compiled into this crate
//! and construct one by id.

use crate::builtin::{
    Aurora, Bloom, EmberDrift, Lattice, Phosphor, Sonar, Spectra, Starfall, Tide, Verso, aurora,
    bloom, ember_drift, lattice, phosphor, sonar, spectra, starfall, tide, verso,
};
use crate::scene::{ParamSpec, Scene};

/// A catalog entry describing a built-in scene, for a browser to list.
///
/// `params` is the scene's parameter manifest: the keys a preset may set, with
/// their defaults and ranges. Presets are typed against it.
///
/// `SceneInfo` is only `PartialEq` (not `Eq`) because [`ParamSpec`] carries
/// `f32` bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneInfo {
    /// The scene's stable identifier, passed to [`create_builtin`].
    pub id: &'static str,
    /// A one-word mood.
    pub mood: &'static str,
    /// A one-line human summary.
    pub summary: &'static str,
    /// The scene's parameter manifest.
    pub params: &'static [ParamSpec],
}

/// Every built-in scene, in listing order.
static BUILTINS: &[SceneInfo] = &[
    SceneInfo {
        id: "spectra",
        mood: "kinetic",
        summary: "The canonical analyzer: log-spaced bars whose low end punches on the onset envelope.",
        params: spectra::PARAMS,
    },
    SceneInfo {
        id: "lattice",
        mood: "serene",
        summary: "A calm dot lattice; onsets fire rings that ripple outward and loudness sets the base glow.",
        params: lattice::PARAMS,
    },
    SceneInfo {
        id: "aurora",
        mood: "serene",
        summary: "A slow interference field: drifting wavefronts under a bright band that breathes with loudness.",
        params: aurora::PARAMS,
    },
    SceneInfo {
        id: "starfall",
        mood: "cosmic",
        summary: "A starfield streaming outward from the centre; loudness rides its speed and onsets warp the outer stars into streaks.",
        params: starfall::PARAMS,
    },
    SceneInfo {
        id: "tide",
        mood: "fluid",
        summary: "Four stacked horizontal swells drifting at their own tempos; the front swell lifts with the low band and the field breathes with loudness.",
        params: tide::PARAMS,
    },
    SceneInfo {
        id: "verso",
        mood: "literal",
        summary: "The track title is the analyzer: each letter rides its own spectrum band, floating and shedding a dotted falling trail.",
        params: verso::PARAMS,
    },
    SceneInfo {
        id: "phosphor",
        mood: "retro",
        summary: "A Lissajous trace burned onto a decaying phosphor screen; the figure precesses and onsets bloom its amplitude.",
        params: phosphor::PARAMS,
    },
    SceneInfo {
        id: "sonar",
        mood: "vigilant",
        summary: "A sweep arm circling at the track tempo; onsets flare contacts that fade with the phosphor.",
        params: sonar::PARAMS,
    },
    SceneInfo {
        id: "ember-drift",
        mood: "organic",
        summary: "Sparse embers rise and cool from a near-black field; at silence it settles into a single breathing ember.",
        params: ember_drift::PARAMS,
    },
    SceneInfo {
        id: "bloom",
        mood: "maximal",
        summary: "A six-fold kaleidoscope mandala breathing with the mids; onsets flash a bright core.",
        params: bloom::PARAMS,
    },
];

/// The catalog of built-in scenes.
#[must_use]
pub fn builtin_scenes() -> &'static [SceneInfo] {
    BUILTINS
}

/// The catalog entry for a scene id, or `None` if the id is unknown.
#[must_use]
pub fn scene_info(id: &str) -> Option<&'static SceneInfo> {
    BUILTINS.iter().find(|info| info.id == id)
}

/// Construct a built-in scene by id, or `None` if the id is unknown.
#[must_use]
pub fn create_builtin(id: &str) -> Option<Box<dyn Scene>> {
    match id {
        "spectra" => Some(Box::new(Spectra::new())),
        "lattice" => Some(Box::new(Lattice::new())),
        "aurora" => Some(Box::new(Aurora::new())),
        "starfall" => Some(Box::new(Starfall::new())),
        "tide" => Some(Box::new(Tide::new())),
        "verso" => Some(Box::new(Verso::new())),
        "phosphor" => Some(Box::new(Phosphor::new())),
        "sonar" => Some(Box::new(Sonar::new())),
        "ember-drift" => Some(Box::new(EmberDrift::new())),
        "bloom" => Some(Box::new(Bloom::new())),
        _ => None,
    }
}
