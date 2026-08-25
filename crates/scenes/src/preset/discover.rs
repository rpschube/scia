//! Discovering `.toml` preset drop-ins: any `<config_dir>/presets/*.toml` file,
//! the same `presets` root the tuning strip exports edited presets into and the
//! same `config_dir` the Luau scene loader and `config.toml` share.
//!
//! A drop-in preset is one self-contained TOML file. It is discovered at first
//! catalog use, parsed once, and reachable by `--scene <name>` exactly like a
//! built-in preset — where `<name>` is the preset's own `[preset].name`. This
//! is the preset half of the same contract Luau scenes already have (see
//! [`super::super::luau::discover`]): a file that fails to parse is skipped (it
//! never shadows a working preset), and a drop-in whose name collides with a
//! built-in scene, a built-in preset, or a discovered Luau scene is dropped, so
//! a built-in listing is never disturbed — **built-ins win**.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use super::{Preset, builtin_presets, load_preset};

/// The subdirectory of the config dir that holds `.toml` preset drop-ins — the
/// same directory the tuning strip exports edited presets into.
pub const DEFAULT_PRESETS_SUBDIR: &str = "presets";

/// A discovered drop-in preset: its selection name (its `[preset].name`) and the
/// parsed, validated preset.
struct DiscoveredPreset {
    name: String,
    preset: Preset,
}

/// The `scia` config directory for this platform, or `None` when no base
/// directory is known. Mirrors the binary's `config::config_dir` (and the Luau
/// scene loader's copy) so scenes, presets and `config.toml` share one root —
/// replicated here, not imported, because the scenes crate must not depend on
/// the binary.
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

/// The drop-in presets directory (`<config_dir>/presets`), or `None` when no
/// base directory is known.
#[must_use]
pub fn presets_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join(DEFAULT_PRESETS_SUBDIR))
}

/// The immutable, process-wide set of discovered drop-in presets, built once on
/// first use (like the Luau catalog) from the config dir the process started in.
static DISCOVERED: OnceLock<Vec<DiscoveredPreset>> = OnceLock::new();

fn discovered() -> &'static [DiscoveredPreset] {
    DISCOVERED.get_or_init(discover).as_slice()
}

/// The names a drop-in preset must not shadow: every scene the catalog lists
/// (built-in scenes and discovered Luau scenes) plus every built-in preset.
fn reserved_names() -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    // The whole scene catalog: built-in scenes first, then Luau scenes. This
    // also transitively initializes the Luau catalog, but there is no cycle —
    // the Luau catalog never consults preset discovery.
    for info in crate::luau::catalog_scenes() {
        seen.insert(info.id.to_string());
    }
    for (name, _) in builtin_presets() {
        seen.insert((*name).to_string());
    }
    seen
}

/// Discover every valid `*.toml` drop-in preset, in file-name order. A file that
/// fails to parse is skipped, and a name already claimed by a built-in or a Luau
/// scene is dropped (built-ins win).
fn discover() -> Vec<DiscoveredPreset> {
    let mut seen = reserved_names();
    let mut out = Vec::new();

    let Some(dir) = presets_dir() else {
        return out;
    };
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("toml"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();

    for path in files {
        // A file that fails to read or validate is skipped, never fatal — a
        // broken drop-in must never shadow a working one or take startup down.
        let Ok(preset) = load_preset(&path) else {
            continue;
        };
        if seen.contains(&preset.name) {
            continue;
        }
        seen.insert(preset.name.clone());
        out.push(DiscoveredPreset {
            name: preset.name.clone(),
            preset,
        });
    }
    out
}

/// A drop-in preset by name, cloned from the cache, or `None` if there is no
/// such drop-in. The lookup key is the preset's own `[preset].name`.
#[must_use]
pub(crate) fn discovered_preset(name: &str) -> Option<Preset> {
    discovered()
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.preset.clone())
}

/// The names of the discovered drop-in presets, in listing (file-name) order.
/// The strings live in the process-wide cache, so they are handed out as
/// `&'static str` for a listing (`--list-scenes`).
#[must_use]
pub fn discovered_preset_names() -> Vec<&'static str> {
    discovered().iter().map(|d| d.name.as_str()).collect()
}
