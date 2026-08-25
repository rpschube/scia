//! The scene catalog: the union of the built-in scenes and the discovered Luau
//! scenes, presented as one listing the browser and `--scene` path consume.
//!
//! The built-in listing order is **untouched** — [`catalog_scenes`] returns the
//! built-ins first, in their locked order (`spectra` … `bloom`), then the Luau
//! scenes appended after them. The catalog is built once, on first use, by
//! scanning the shipped scenes and the drop-in directory; it is process-global
//! and immutable thereafter (a live edit to a drop-in reaches a running scene
//! through the [`super::LuauWatcher`] hot-reload path, not by rebuilding the
//! catalog).

use std::sync::OnceLock;

use crate::preset::{Preset, PresetError, builtin_preset};
use crate::registry::{SceneInfo, builtin_scenes, create_builtin};
use crate::scene::Scene;

use super::discover::{DiscoveredScene, discover};
use super::watch::LuauSource;
use super::{LuauError, LuauLimits, LuauScene, default_palette};

/// A catalog entry for one Luau scene: its listing info, the source to build
/// (and rebuild) a live VM from, and the drop-in file it came from (if any, for
/// the hot-reload watch).
struct LuauEntry {
    info: &'static SceneInfo,
    source: std::sync::Arc<str>,
    path: Option<std::path::PathBuf>,
}

/// The immutable process-wide catalog.
struct Catalog {
    /// Built-ins (locked order) followed by the Luau scenes, leaked `'static`.
    all: &'static [SceneInfo],
    /// The Luau scenes alone, for construction by id.
    luau: Vec<LuauEntry>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// Build the catalog once: built-ins first, then the discovered Luau scenes.
fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let discovered: Vec<DiscoveredScene> = discover();

        let mut all: Vec<SceneInfo> = builtin_scenes().to_vec();
        all.extend(discovered.iter().map(|d| *d.info));
        let all: &'static [SceneInfo] = Box::leak(all.into_boxed_slice());

        let luau = discovered
            .into_iter()
            .map(|d| LuauEntry {
                info: d.info,
                source: d.source,
                path: d.path,
            })
            .collect();

        Catalog { all, luau }
    })
}

/// Every scene the app can list: built-ins in their locked order, then the Luau
/// scenes appended after them.
#[must_use]
pub fn catalog_scenes() -> &'static [SceneInfo] {
    catalog().all
}

/// The catalog entry for a scene id (built-in or Luau), or `None`.
#[must_use]
pub fn catalog_scene_info(id: &str) -> Option<&'static SceneInfo> {
    catalog().all.iter().find(|i| i.id == id)
}

/// The ids of the discovered Luau scenes, in listing order.
#[must_use]
pub fn luau_scene_ids() -> Vec<&'static str> {
    catalog().luau.iter().map(|e| e.info.id).collect()
}

/// Construct a live scene by id from the catalog: a built-in through the
/// built-in registry, or a Luau scene compiled from its source. `None` if the id
/// is unknown or (for a Luau id) it fails to compile.
#[must_use]
pub fn create_scene(id: &str) -> Option<Box<dyn Scene>> {
    if let Some(scene) = create_builtin(id) {
        return Some(scene);
    }
    create_luau(id)
}

/// Construct a Luau scene by id, or `None` if it is not a Luau scene (or fails
/// to compile). Kept separate from [`create_scene`] so preset instantiation can
/// try the built-in registry first and fall back here without recursing.
#[must_use]
pub(crate) fn create_luau(id: &str) -> Option<Box<dyn Scene>> {
    let entry = catalog().luau.iter().find(|e| e.info.id == id)?;
    match LuauScene::compile(&entry.source, entry.info, LuauLimits::default()) {
        Ok(scene) => Some(Box::new(scene)),
        Err(_) => None,
    }
}

/// Whether `id` names a discovered Luau scene.
#[must_use]
pub fn is_luau_scene(id: &str) -> bool {
    catalog().luau.iter().any(|e| e.info.id == id)
}

/// The drop-in file a Luau scene was read from, for a hot-reload watch. `None`
/// for a built-in, a shipped (bundled) scene, or an unknown id.
#[must_use]
pub fn luau_scene_path(id: &str) -> Option<std::path::PathBuf> {
    catalog()
        .luau
        .iter()
        .find(|e| e.info.id == id)
        .and_then(|e| e.path.clone())
}

/// Recompile a live scene from re-validated hot-reload [`LuauSource`], for the
/// `.lua` live-reload path.
///
/// The source has already been read and validated off the render thread (by
/// [`super::LuauWatcher`]); this rebuilds the non-`Send` live VM on the render
/// thread and returns it as a [`Scene`], ready for the presenter to swap in with
/// its crossfade. The catalog's leaked `SceneInfo` for the source's id is reused
/// (a hot reload keeps the same id and reuses the info — see
/// [`super::LuauManifest::leak_info`]), so a reload allocates no new `'static`
/// listing; a source whose id is not in the catalog (an unusual id-changing edit)
/// falls back to leaking a fresh info from its own manifest.
///
/// # Errors
/// [`LuauError`] if the source fails to recompile (rare, since the watcher
/// already validated it) — the caller then keeps the running scene.
pub fn rebuild_luau_scene(source: &LuauSource) -> Result<Box<dyn Scene>, LuauError> {
    let limits = LuauLimits::default();
    let id = source.manifest.id.as_str();
    let scene = match catalog_scene_info(id) {
        Some(info) => LuauScene::compile(&source.source, info, limits)?,
        None => LuauScene::from_source(&source.source, id, limits)?,
    };
    Ok(Box::new(scene))
}

/// A preset for scene `id`, for the `--scene`/browser path: the built-in TOML
/// preset when one exists, otherwise a synthesized bare preset that runs the
/// Luau scene with its manifest defaults and the default palette.
///
/// Returns `None` only when `id` is neither a built-in preset nor a Luau scene.
#[must_use]
pub fn scene_preset(id: &str) -> Option<Result<Preset, PresetError>> {
    if let Some(preset) = builtin_preset(id) {
        return Some(preset);
    }
    let entry = catalog().luau.iter().find(|e| e.info.id == id)?;
    Some(Ok(Preset::for_scene(entry.info, default_palette())))
}
