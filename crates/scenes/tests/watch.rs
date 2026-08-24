//! Integration tests for the preset file watcher: a valid save produces a good
//! [`ReloadEvent`] promptly, a broken save produces a positioned error, an
//! editor's rename-replace is still caught, and dropping the watcher stops its
//! thread without hanging.
//!
//! Each test uses a private temp directory (created under the system temp dir,
//! removed on drop) so the tests never touch the repository and can run
//! concurrently. Timings are deadline-polled and generously bounded so the
//! suite stays robust on a shared runner; the whole file stays well under 10 s.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use scia_scenes::{PresetWatcher, ReloadEvent};

/// A valid single-layer spectra preset with the given `punch` value.
fn preset_src(punch: f32) -> String {
    format!("[preset]\nname = \"spectra\"\nscene = \"spectra\"\n[params]\npunch = {punch}\n")
}

/// A private temp directory, removed when dropped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("scia-watch-{}-{n}", std::process::id()));
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

/// Poll `rx` until an event arrives or `within` elapses.
fn recv_within(rx: &Receiver<ReloadEvent>, within: Duration) -> Option<ReloadEvent> {
    rx.recv_timeout(within).ok()
}

/// Write `contents` to `path` atomically-ish: a plain overwrite. Used for the
/// in-place edit cases.
fn write_file(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write preset file");
}

#[test]
fn valid_edit_reloads_within_500ms() {
    let dir = TempDir::new();
    let path = dir.join("live.toml");
    write_file(&path, &preset_src(0.35));

    let (_watcher, rx) = PresetWatcher::start(&path).expect("start watcher");

    // Change a parameter; the reload must arrive within 500 ms.
    let sent = Instant::now();
    write_file(&path, &preset_src(1.25));

    let event = recv_within(&rx, Duration::from_millis(500)).expect("reload event within 500 ms");
    let latency = sent.elapsed();
    let preset = event.result.expect("valid reload carries a preset");
    // The re-validated preset carries the new value.
    let punch = preset.params.get("punch").expect("punch is set");
    assert!(
        (punch - 1.25).abs() < 1e-6,
        "reload reflects the edited value, got {punch}"
    );
    assert!(
        latency < Duration::from_millis(500),
        "reload latency {latency:?} exceeded 500 ms"
    );
    assert!(
        event.elapsed_ms >= 0.0 && event.elapsed_ms < 500.0,
        "read+validate time {}ms is implausible",
        event.elapsed_ms
    );
    // Report the end-to-end latency for the record.
    eprintln!(
        "valid_edit: end-to-end {:.1} ms, read+validate {:.3} ms",
        latency.as_secs_f32() * 1000.0,
        event.elapsed_ms
    );
}

#[test]
fn broken_edit_reports_positioned_error() {
    let dir = TempDir::new();
    let path = dir.join("live.toml");
    write_file(&path, &preset_src(0.35));

    let (_watcher, rx) = PresetWatcher::start(&path).expect("start watcher");

    // An out-of-range value: a validation error that carries file:line:col.
    write_file(
        &path,
        "[preset]\nname = \"spectra\"\nscene = \"spectra\"\n[params]\npunch = 99.0\n",
    );

    let event = recv_within(&rx, Duration::from_millis(1000)).expect("error event arrives");
    let err = event.result.expect_err("broken edit is an error");
    let msg = err.to_string();
    // The message begins with the file path and carries a line:col position.
    assert!(msg.contains("live.toml"), "error names the file: {msg}");
    assert!(
        err.line.is_some() && err.col.is_some(),
        "error carries a line and column: {msg}"
    );
    // And the Display renders `file:line:col:` form.
    let head = format!(
        "{}:{}:{}:",
        path.display(),
        err.line.unwrap(),
        err.col.unwrap()
    );
    assert!(msg.starts_with(&head), "message positions the error: {msg}");
}

#[test]
fn rename_replace_is_detected() {
    let dir = TempDir::new();
    let path = dir.join("live.toml");
    write_file(&path, &preset_src(0.35));

    let (_watcher, rx) = PresetWatcher::start(&path).expect("start watcher");

    // Editors commonly write a temp sibling and rename it over the target.
    let tmp = dir.join("live.toml.tmp");
    write_file(&tmp, &preset_src(1.75));
    fs::rename(&tmp, &path).expect("rename temp over target");

    let event = recv_within(&rx, Duration::from_millis(1000)).expect("rename-replace is detected");
    assert!(
        event.result.is_ok(),
        "rename-replace yields the new valid preset"
    );
}

#[test]
fn dropping_the_watcher_stops_the_thread() {
    let dir = TempDir::new();
    let path = dir.join("live.toml");
    write_file(&path, &preset_src(0.35));

    let (watcher, rx) = PresetWatcher::start(&path).expect("start watcher");

    // Drop the watcher on a helper thread so a hung join fails the test with a
    // deadline instead of blocking it forever.
    let dropper = std::thread::spawn(move || drop(watcher));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !dropper.is_finished() {
        assert!(
            Instant::now() < deadline,
            "watcher drop did not return within 2 s (thread hung)"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    dropper.join().expect("dropper thread joins");

    // With the worker gone, the event channel is disconnected.
    match rx.recv_timeout(Duration::from_millis(500)) {
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("event channel still open after the watcher was dropped")
        }
        Ok(_) => panic!("unexpected event after the watcher was dropped"),
    }
}
