//! Drop-in `.toml` presets: a preset file dropped into `<config_dir>/presets`
//! is discovered and reachable by `--scene <name>` — the preset half of the
//! same self-contained-file contract Luau scenes already have. A file that fails
//! to validate is skipped, and a name that collides with a built-in is ignored
//! (built-ins win).
//!
//! Each test runs in its own process under nextest, so pointing the config-home
//! variable at a private temp dir before the first catalog access is isolated to
//! that test — the process-global discovery cache is built once from the env the
//! test set.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use scia_scenes::{discovered_preset_names, presets_dir, scene_preset};

/// A private temp directory, removed when dropped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("scia-preset-disc-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Point the config dir at `dir`, so `presets_dir()` resolves under it and no
/// real drop-ins on this machine leak into the test. The variable is the one
/// the loader actually reads on this platform: `APPDATA` on Windows,
/// `XDG_CONFIG_HOME` elsewhere.
fn set_config_home(dir: &std::path::Path) {
    // SAFETY: set before any discovery access in this (single-test) process; no
    // other thread reads the env concurrently here.
    unsafe {
        #[cfg(windows)]
        std::env::set_var("APPDATA", dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", dir);
    }
}

#[test]
fn a_dropin_preset_is_discovered_and_loads_by_name() {
    let tmp = TempDir::new();
    set_config_home(&tmp.path);

    let dir = presets_dir().expect("a presets dir resolves under the temp config home");
    assert!(
        dir.starts_with(&tmp.path),
        "presets dir is under the temp config home: {dir:?}"
    );
    fs::create_dir_all(&dir).expect("create presets dir");
    fs::write(
        dir.join("my-scene.toml"),
        "[preset]\nname = \"my-scene\"\nscene = \"spectra\"\n",
    )
    .expect("write drop-in");

    assert!(
        discovered_preset_names().contains(&"my-scene"),
        "the drop-in lists: {:?}",
        discovered_preset_names()
    );
    let preset = scene_preset("my-scene")
        .expect("the drop-in resolves by name")
        .expect("the drop-in is a valid preset");
    assert_eq!(preset.name, "my-scene");
    assert_eq!(preset.scene, "spectra", "it drives the scene it names");
}

#[test]
fn a_broken_dropin_preset_is_skipped_not_fatal() {
    let tmp = TempDir::new();
    set_config_home(&tmp.path);

    let dir = presets_dir().expect("presets dir");
    fs::create_dir_all(&dir).expect("create presets dir");
    // Not valid TOML — discovery must skip it, and a valid neighbour still lists.
    fs::write(dir.join("broken.toml"), "this is not [ valid toml =").expect("write");
    fs::write(
        dir.join("good.toml"),
        "[preset]\nname = \"good\"\nscene = \"spectra\"\n",
    )
    .expect("write");

    let names = discovered_preset_names();
    assert!(
        names.contains(&"good"),
        "a valid drop-in beside a broken one still lists: {names:?}"
    );
    assert!(
        !names.contains(&"broken"),
        "the broken drop-in is skipped, not fatal"
    );
}

#[test]
fn a_dropin_named_like_a_builtin_is_ignored_builtins_win() {
    let tmp = TempDir::new();
    set_config_home(&tmp.path);

    let dir = presets_dir().expect("presets dir");
    fs::create_dir_all(&dir).expect("create presets dir");
    // A drop-in claiming the built-in name `spectra`, with a tell-tale param
    // value (the built-in ships gap = 0.15).
    fs::write(
        dir.join("spectra.toml"),
        "[preset]\nname = \"spectra\"\nscene = \"spectra\"\n[params]\ngap = 0.9\n",
    )
    .expect("write");

    assert!(
        !discovered_preset_names().contains(&"spectra"),
        "the colliding drop-in is dropped, not listed"
    );
    // `--scene spectra` still resolves to the BUILT-IN, not the drop-in.
    let preset = scene_preset("spectra")
        .expect("spectra resolves")
        .expect("spectra is valid");
    assert_eq!(
        preset.params.get("gap"),
        Some(0.15),
        "the built-in wins the name; the drop-in's gap = 0.9 is ignored"
    );
    assert_ne!(preset.params.get("gap"), Some(0.9));
}
