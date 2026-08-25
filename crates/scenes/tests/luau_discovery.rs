//! Discovery and catalogue: the shipped Luau scenes list alongside the
//! built-ins (built-ins first, in their locked order), a `.lua` dropped in the
//! scenes directory is discovered, and the built-in listing is never disturbed.
//!
//! Each test runs in its own process under nextest, so pointing
//! `XDG_CONFIG_HOME` at a private temp dir before the first catalogue access is
//! isolated to that test — the process-global catalogue is built once from the
//! env this test set.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use scia_scenes::{builtin_scenes, catalog_scenes, create_scene, scene_preset, scenes_dir};

/// A private temp directory, removed when dropped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("scia-luau-disc-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Point the config dir at `dir`, so `scenes_dir()` resolves under it and no
/// real drop-ins on this machine leak into the test. The variable is the one
/// `config_dir()` actually reads on this platform: `APPDATA` on Windows,
/// `XDG_CONFIG_HOME` elsewhere.
fn set_config_home(dir: &std::path::Path) {
    // SAFETY: set before any catalogue access in this (single-test) process; no
    // other thread reads the env concurrently here.
    unsafe {
        #[cfg(windows)]
        std::env::set_var("APPDATA", dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", dir);
    }
}

#[test]
fn shipped_scenes_list_after_the_builtins() {
    let tmp = TempDir::new(); // empty config dir → only the shipped scenes
    set_config_home(&tmp.path);

    let all = catalog_scenes();
    let builtins = builtin_scenes();

    // The built-in listing is intact and comes first, in its locked order.
    assert!(all.len() >= builtins.len());
    for (a, b) in all.iter().zip(builtins.iter()) {
        assert_eq!(a.id, b.id, "built-ins keep their order at the front");
    }

    let ids: Vec<&str> = all.iter().map(|i| i.id).collect();
    assert!(ids.contains(&"ripple"), "ripple is catalogued: {ids:?}");
    assert!(ids.contains(&"swarm"), "swarm is catalogued: {ids:?}");

    // The shipped scenes come after every built-in.
    let ripple_pos = ids.iter().position(|i| *i == "ripple").unwrap();
    assert!(
        ripple_pos >= builtins.len(),
        "a Luau scene is appended after the built-ins"
    );

    // Each shipped scene carries a mood, a summary, and a param manifest.
    let ripple = all.iter().find(|i| i.id == "ripple").unwrap();
    assert!(!ripple.mood.is_empty() && !ripple.summary.is_empty());
    assert!(!ripple.params.is_empty(), "ripple declares tunable params");
    // Distinct ids and moods between the two shipped scenes.
    let swarm = all.iter().find(|i| i.id == "swarm").unwrap();
    assert_ne!(
        ripple.mood, swarm.mood,
        "the two scenes have distinct moods"
    );
}

#[test]
fn builtin_order_is_untouched_by_the_catalog() {
    let tmp = TempDir::new();
    set_config_home(&tmp.path);
    // The dedicated built-in registry still ends spectra..bloom; the catalogue
    // does not reorder or shadow it.
    let ids: Vec<&str> = builtin_scenes().iter().map(|i| i.id).collect();
    assert_eq!(ids.first(), Some(&"spectra"));
    assert_eq!(ids.last(), Some(&"bloom"));
}

#[test]
fn a_dropin_scene_is_discovered_and_constructs() {
    let tmp = TempDir::new();
    set_config_home(&tmp.path);

    // Write a valid drop-in under <config>/scia/scenes/.
    let dir = scenes_dir().expect("a scenes dir resolves under the temp config home");
    assert!(
        dir.starts_with(&tmp.path),
        "scenes dir is under the temp config home: {dir:?}"
    );
    fs::create_dir_all(&dir).expect("create scenes dir");
    let scene = r#"
    return {
      id = "dropin-demo",
      mood = "novel",
      summary = "a drop-in scene discovered from disk",
      update = function(f, dt) end,
      render = function(c) c:point(0.5, 0.5, 0.1, 3, 1.0) end,
    }
    "#;
    fs::write(dir.join("dropin-demo.lua"), scene).expect("write drop-in");

    let ids: Vec<&str> = catalog_scenes().iter().map(|i| i.id).collect();
    assert!(
        ids.contains(&"dropin-demo"),
        "the drop-in appears alongside the built-ins: {ids:?}"
    );

    // It constructs by id and yields a synthesized preset for the --scene path.
    let scene = create_scene("dropin-demo").expect("the drop-in constructs");
    assert_eq!(scene.id(), "dropin-demo");
    assert!(
        scene_preset("dropin-demo").is_some_and(|r| r.is_ok()),
        "the drop-in has a --scene preset"
    );
}

#[test]
fn a_broken_dropin_is_skipped_not_fatal() {
    let tmp = TempDir::new();
    set_config_home(&tmp.path);
    let dir = scenes_dir().expect("scenes dir");
    fs::create_dir_all(&dir).expect("create scenes dir");
    // Not valid Lua — discovery must skip it, and the shipped scenes still list.
    fs::write(dir.join("broken.lua"), "this is not lua ]][[").expect("write");

    let ids: Vec<&str> = catalog_scenes().iter().map(|i| i.id).collect();
    assert!(
        ids.contains(&"ripple"),
        "a broken drop-in does not break discovery"
    );
    assert!(!ids.contains(&"broken"), "the broken drop-in is not listed");
}
