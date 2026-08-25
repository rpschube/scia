//! The `.lua` live-reload building blocks (US-CFG-4, same last-good contract as
//! US-CFG-2): re-validated source rebuilds into a live scene, the whole feature
//! vocabulary is served by real bridge getters, and a watched edit feeds a fresh
//! scene through the rebuild seam. The presenter-side crossfade of the swap is
//! covered by the presenter's own unit tests; the run loop wires these together
//! exactly as the preset reload path wires the preset watcher to `swap_preset`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use scia_core::{FeatureSnapshot, SPECTRUM_BINS};
use scia_scenes::{
    Canvas, LuauLimits, LuauReloadEvent, LuauScene, LuauSource, LuauWatcher, Scene, SceneCtx,
    compile_manifest, luau_feature_vocabulary, rebuild_luau_scene,
};

/// A private temp directory, removed when dropped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("scia-luau-reload-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn scene_src(id: &str, mood: &str) -> String {
    format!(
        r#"
        return {{
          id = "{id}",
          mood = "{mood}",
          summary = "a live-reload scene",
          update = function(f, dt) end,
          render = function(c) c:point(0.5, 0.5, 0.1, 1, 1.0) end,
        }}
        "#
    )
}

fn source_of(id: &str, mood: &str) -> LuauSource {
    let src = scene_src(id, mood);
    let manifest = compile_manifest(&src, id).expect("valid manifest");
    LuauSource {
        source: src,
        manifest,
    }
}

fn recv_within(rx: &Receiver<LuauReloadEvent>, within: Duration) -> Option<LuauReloadEvent> {
    rx.recv_timeout(within).ok()
}

#[test]
fn rebuild_builds_a_live_scene_from_revalidated_source() {
    // A novel id (not in the catalog) exercises the fresh-info fallback; the
    // rebuilt scene carries the id and mood the source declares and runs a frame.
    let source = source_of("reload-demo", "kinetic");
    let mut scene = rebuild_luau_scene(&source).expect("rebuilds");
    assert_eq!(scene.id(), "reload-demo");
    assert_eq!(scene.mood(), "kinetic");

    // It is a live scene: init, update and render drive without faulting.
    scene.init(&SceneCtx::default());
    scene.update(&FeatureSnapshot::default(), 0.016);
    let mut canvas = Canvas::new(1.0);
    scene.render(&mut canvas);
}

#[test]
fn every_feature_vocabulary_name_is_a_live_getter() {
    // Prove the did-you-mean vocabulary is not stale: a scene that reads every
    // name off the `features` userdata each tick must not fault. A name that is
    // not a real getter would read `nil` (or raise), faulting the arithmetic
    // below and latching the scene into its error state.
    let mut reads = String::new();
    for name in luau_feature_vocabulary() {
        if *name == "onset" {
            // `onset` is a boolean getter — read it in a boolean context.
            reads.push_str("          if f.onset then acc = acc + 1.0 end\n");
        } else {
            reads.push_str(&format!("          acc = acc + f.{name}\n"));
        }
    }
    let src = format!(
        r#"
        local acc = 0.0
        return {{
          id = "vocab-probe",
          mood = "kinetic",
          summary = "reads the whole feature API",
          update = function(f, dt)
{reads}          end,
          render = function(c) c:point(0.5, 0.5, 0.1, 1, 1.0) end,
        }}
        "#
    );

    let mut scene =
        LuauScene::from_source(&src, "vocab-probe", LuauLimits::default()).expect("compiles");
    scene.init(&SceneCtx::default());
    // A snapshot with a valid spectrum so `bar_count` and the bands read sanely.
    let snap = FeatureSnapshot {
        spectrum_len: SPECTRUM_BINS as u16,
        ..FeatureSnapshot::default()
    };
    scene.update(&snap, 0.016);
    let mut canvas = Canvas::new(1.0);
    scene.render(&mut canvas);
    assert!(
        !scene.is_errored(),
        "reading every vocabulary name is fault-free: {:?}",
        scene.last_error()
    );
}

#[test]
fn a_watched_edit_rebuilds_into_the_new_scene() {
    // The library-level chain the run loop wires: an edit reaches the watcher,
    // which re-validates off-thread and hands back the new source; rebuilding it
    // yields a live scene carrying the edited manifest — the swap material the
    // presenter then crossfades in.
    let dir = TempDir::new();
    let path = dir.join("watched.lua");
    fs::write(&path, scene_src("watched", "serene")).expect("seed the file");

    let (_watcher, rx) = LuauWatcher::start(&path).expect("watch starts");
    fs::write(&path, scene_src("watched", "kinetic")).expect("edit the file");

    let event = recv_within(&rx, Duration::from_millis(500)).expect("a reload arrives");
    let source = event.result.expect("the valid edit validates");

    let scene = rebuild_luau_scene(&source).expect("the new source rebuilds");
    assert_eq!(scene.id(), "watched");
    assert_eq!(
        scene.mood(),
        "kinetic",
        "the rebuilt scene reflects the edited source"
    );
}

#[test]
fn a_broken_edit_never_yields_a_scene() {
    // A broken edit travels as an error on the same channel; the caller keeps the
    // running scene (nothing to rebuild). This mirrors the preset path's contract.
    let dir = TempDir::new();
    let path = dir.join("watched.lua");
    fs::write(&path, scene_src("watched", "serene")).expect("seed the file");

    let (_watcher, rx) = LuauWatcher::start(&path).expect("watch starts");
    fs::write(&path, "this is not lua ]][[").expect("break the file");

    let event = recv_within(&rx, Duration::from_millis(500)).expect("a reload arrives");
    assert!(
        event.result.is_err(),
        "a broken edit is an error, so no scene is rebuilt"
    );
}
