//! The Luau scene hot-reload watch (US-CFG-4, same contract as US-CFG-2): a
//! save re-validates the file off the render thread and delivers one
//! [`LuauReloadEvent`]; a valid edit yields the new source, a broken edit yields
//! a positioned error and the caller keeps the old scene.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use scia_scenes::{LuauReloadEvent, LuauWatcher};

/// A private temp directory, removed when dropped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("scia-luau-watch-{}-{n}", std::process::id()));
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

fn scene_src(mood: &str) -> String {
    format!(
        r#"
        return {{
          id = "watched",
          mood = "{mood}",
          summary = "a watched scene",
          update = function(f, dt) end,
          render = function(c) c:point(0.5, 0.5, 0.1, 1, 1.0) end,
        }}
        "#
    )
}

fn recv_within(rx: &Receiver<LuauReloadEvent>, within: Duration) -> Option<LuauReloadEvent> {
    rx.recv_timeout(within).ok()
}

#[test]
fn valid_edit_reloads_with_the_new_source() {
    let dir = TempDir::new();
    let path = dir.join("watched.lua");
    fs::write(&path, scene_src("serene")).expect("seed the file");

    let (_watcher, rx) = LuauWatcher::start(&path).expect("watch starts");

    // Edit to a new valid version.
    fs::write(&path, scene_src("kinetic")).expect("edit the file");

    let event = recv_within(&rx, Duration::from_millis(500)).expect("a reload arrives");
    let source = event.result.expect("the valid edit validates");
    assert_eq!(
        source.manifest.mood, "kinetic",
        "the new manifest is parsed"
    );
    assert!(
        source.source.contains("kinetic"),
        "the new source is carried"
    );
}

#[test]
fn broken_edit_reports_an_error_and_keeps_the_old() {
    let dir = TempDir::new();
    let path = dir.join("watched.lua");
    fs::write(&path, scene_src("serene")).expect("seed the file");

    let (_watcher, rx) = LuauWatcher::start(&path).expect("watch starts");

    // Edit to something that is not valid Lua.
    fs::write(&path, "this is not lua ]][[").expect("break the file");

    let event = recv_within(&rx, Duration::from_millis(500)).expect("a reload arrives");
    let err = event.result.expect_err("a broken edit is an error");
    // The error names the file, so the caller can surface it and keep running.
    assert_eq!(
        err.file.as_deref(),
        Some(path.as_path()),
        "the error names the file"
    );
}

#[test]
fn dropping_the_watcher_stops_the_thread() {
    let dir = TempDir::new();
    let path = dir.join("watched.lua");
    fs::write(&path, scene_src("serene")).expect("seed the file");

    let (watcher, rx) = LuauWatcher::start(&path).expect("watch starts");
    drop(watcher);
    // With the watcher dropped, the channel disconnects; no reload can arrive.
    fs::write(&path, scene_src("kinetic")).ok();
    assert!(
        recv_within(&rx, Duration::from_millis(200)).is_none(),
        "no reload after the watcher is dropped"
    );
    let _ = Path::new(&path);
}
