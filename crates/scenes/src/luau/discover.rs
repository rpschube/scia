//! Discovering Luau scenes: the two shipped scenes bundled into the binary, plus
//! any `.lua` drop-ins in the user scenes directory.
//!
//! The shipped scenes are compiled in with `include_str!`, so they work with no
//! files installed and appear in the browser like drop-ins. A drop-in is any
//! `<config_dir>/scenes/*.lua` file (the same `config_dir` the preset loader and
//! `config.toml` use). Discovery reads each candidate's manifest in a throwaway
//! sandboxed VM; a file that fails to compile or is not a well-formed manifest is
//! skipped (it never shadows a working scene), and a drop-in whose id collides
//! with a built-in or a shipped scene is skipped so the built-in listing is
//! never disturbed.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::registry::{SceneInfo, builtin_scenes};

use super::{LuauManifest, compile_manifest};

/// The subdirectory of the config dir that holds drop-in `.lua` scenes.
pub const DEFAULT_SCENES_SUBDIR: &str = "scenes";

/// The reserved scene name the CLI maps to the legacy direct-bars renderer; a
/// drop-in must never claim it.
const RESERVED_ID: &str = "bars";

/// The two shipped Luau scenes, bundled as `(name, source)`. They are living
/// documentation of the feature + canvas API (see each file's header).
static BUNDLED: &[(&str, &str)] = &[
    ("ripple", include_str!("scenes/ripple.lua")),
    ("swarm", include_str!("scenes/swarm.lua")),
];

/// The two shipped Luau scenes as `(name, source)`, bundled into the binary.
/// Exposed so callers (and tests) can reach the source without touching the
/// filesystem.
#[must_use]
pub fn shipped_scenes() -> &'static [(&'static str, &'static str)] {
    BUNDLED
}

/// A discovered Luau scene: its leaked `'static` listing info plus the source
/// needed to instantiate (and reinstantiate, on hot reload) a live VM.
pub(crate) struct DiscoveredScene {
    pub(crate) info: &'static SceneInfo,
    pub(crate) source: Arc<str>,
    /// The drop-in file, or `None` for a bundled scene.
    pub(crate) path: Option<PathBuf>,
}

/// The `scia` config directory for this platform, or `None` when no base
/// directory is known. Mirrors the binary's `config::config_dir` so scenes and
/// presets share one root — replicated here (not imported) because the scenes
/// crate must not depend on the binary.
#[must_use]
fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut path = PathBuf::from(std::env::var_os("APPDATA")?);
        path.push("scia");
        Some(path)
    }
    #[cfg(not(windows))]
    {
        let mut path = match std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
        {
            Some(dir) => dir,
            None => {
                let mut home = PathBuf::from(std::env::var_os("HOME")?);
                home.push(".config");
                home
            }
        };
        path.push("scia");
        Some(path)
    }
}

/// The drop-in scenes directory (`<config_dir>/scenes`), or `None` when no base
/// directory is known.
#[must_use]
pub fn scenes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join(DEFAULT_SCENES_SUBDIR))
}

/// Discover every Luau scene: the shipped scenes first (in listed order), then
/// drop-ins from the scenes directory sorted by file name. Ids that collide with
/// a built-in, the reserved `bars`, or an already-discovered scene are dropped.
pub(crate) fn discover() -> Vec<DiscoveredScene> {
    let mut seen: BTreeSet<String> = builtin_scenes().iter().map(|i| i.id.to_string()).collect();
    seen.insert(RESERVED_ID.to_string());

    let mut out = Vec::new();

    // Shipped scenes: a compile failure here is a build-time bug in our own
    // source, but we still skip rather than panic, so a bad ship can never take
    // the whole app down.
    for (name, source) in BUNDLED {
        if let Ok(manifest) = compile_manifest(source, name) {
            push_scene(&mut out, &mut seen, manifest, source, None);
        }
    }

    // Drop-ins.
    if let Some(dir) = scenes_dir() {
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "lua"))
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort();
        for path in files {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("scene");
            if let Ok(manifest) = compile_manifest(&source, name) {
                push_scene(&mut out, &mut seen, manifest, &source, Some(path));
            }
        }
    }

    out
}

/// Push a discovered scene unless its id is already taken.
fn push_scene(
    out: &mut Vec<DiscoveredScene>,
    seen: &mut BTreeSet<String>,
    manifest: LuauManifest,
    source: &str,
    path: Option<PathBuf>,
) {
    if seen.contains(&manifest.id) {
        return;
    }
    seen.insert(manifest.id.clone());
    out.push(DiscoveredScene {
        info: manifest.leak_info(),
        source: Arc::from(source),
        path,
    });
}
