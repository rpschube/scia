//! A filesystem watcher for a Luau scene file — the `.lua` twin of the preset
//! watcher ([`crate::PresetWatcher`]), with the same last-good-version contract.
//!
//! It watches the file's **parent directory** (so an editor's rename-replace
//! save is caught), coalesces a save's burst of raw events with one debounce
//! window, then re-reads and **re-validates** the file off the render thread and
//! hands the result back as a [`LuauReloadEvent`]. A failed read, compile or
//! manifest check travels as the event's error; the render thread keeps the old
//! scene running and surfaces the message — the same guarantee US-CFG-2 gives a
//! preset.
//!
//! Validation happens on the watch thread by compiling the manifest in a
//! throwaway sandboxed VM. That VM (and the `LuauScene` the render thread will
//! build) are not `Send`, so the watch thread never ships a live scene: it ships
//! a [`LuauSource`] — the validated source text plus its parsed manifest, both
//! plain `Send` data — and the render thread rebuilds the live VM locally.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher};

use super::{LuauError, LuauErrorKind, LuauManifest, compile_manifest};

/// How long to coalesce a save's burst of raw events before re-reading — an
/// editor's save is several events within a few milliseconds.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// A validated Luau scene source: the text plus its parsed manifest. Plain
/// `Send` data, so it crosses from the watch thread to the render thread, which
/// rebuilds the (non-`Send`) live VM from it.
#[derive(Clone, Debug, PartialEq)]
pub struct LuauSource {
    /// The validated source text.
    pub source: String,
    /// Its parsed manifest (id/mood/summary/params).
    pub manifest: LuauManifest,
}

/// The outcome of one Luau scene reload: the re-validated source or the error
/// that reading/validating it raised, plus how long the work took.
#[derive(Debug)]
pub struct LuauReloadEvent {
    /// The re-validated source, or the error reading/validating it raised.
    pub result: Result<LuauSource, LuauError>,
    /// Wall-clock milliseconds spent reading and validating the file.
    pub elapsed_ms: f32,
}

/// A live watch on a Luau scene file. Hold it for as long as reloads are wanted;
/// dropping it stops the watch and joins the worker thread.
pub struct LuauWatcher {
    watcher: Option<notify::RecommendedWatcher>,
    handle: Option<JoinHandle<()>>,
}

impl LuauWatcher {
    /// Start watching `path`, returning the watcher and the channel its reloads
    /// arrive on. The first reload arrives only after the file next changes; the
    /// current contents are not replayed.
    ///
    /// # Errors
    /// Returns a [`LuauError`] if the watch backend cannot be created or the
    /// directory cannot be watched. Failures after a successful start travel as
    /// a [`LuauReloadEvent`] error, never a panic.
    pub fn start(path: &Path) -> Result<(Self, Receiver<LuauReloadEvent>), LuauError> {
        let path = path.to_path_buf();
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let file_name = path.file_name().map(std::ffi::OsString::from);

        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        })
        .map_err(|e| watch_error(&path, &e))?;
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|e| watch_error(&path, &e))?;

        let (evt_tx, evt_rx) = mpsc::channel::<LuauReloadEvent>();
        let worker_path = path.clone();
        let handle = std::thread::Builder::new()
            .name("scia-luau-watch".to_string())
            .spawn(move || run_worker(&raw_rx, &evt_tx, &worker_path, file_name.as_deref()))
            .map_err(|e| LuauError {
                file: Some(path.clone()),
                kind: LuauErrorKind::Io(e.to_string()),
            })?;

        Ok((
            Self {
                watcher: Some(watcher),
                handle: Some(handle),
            },
            evt_rx,
        ))
    }
}

impl Drop for LuauWatcher {
    fn drop(&mut self) {
        self.watcher.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read, then validate by compiling the manifest, timing just that work.
fn reload(path: &Path) -> LuauReloadEvent {
    let start = Instant::now();
    let result = load_and_validate(path);
    let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;
    LuauReloadEvent { result, elapsed_ms }
}

/// Read `path` and validate its manifest, returning a [`LuauSource`] on success.
fn load_and_validate(path: &Path) -> Result<LuauSource, LuauError> {
    let source = std::fs::read_to_string(path).map_err(|e| LuauError {
        file: Some(path.to_path_buf()),
        kind: LuauErrorKind::Io(e.to_string()),
    })?;
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("scene");
    let manifest = compile_manifest(&source, name).map_err(|e| e.with_file(path))?;
    Ok(LuauSource { source, manifest })
}

/// The worker loop: block for a raw event, coalesce the burst, then (when the
/// target file was touched, or a watch error arrived) re-read/re-validate and
/// forward one [`LuauReloadEvent`].
fn run_worker(
    raw_rx: &Receiver<notify::Result<Event>>,
    evt_tx: &Sender<LuauReloadEvent>,
    path: &Path,
    file_name: Option<&std::ffi::OsStr>,
) {
    loop {
        let first = match raw_rx.recv() {
            Ok(res) => res,
            Err(_) => return,
        };
        let mut touched = touches_target(&first, file_name);
        let mut watch_err = first.err();

        loop {
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(res) => {
                    touched |= touches_target(&res, file_name);
                    if watch_err.is_none() {
                        watch_err = res.err();
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if touched {
            if evt_tx.send(reload(path)).is_err() {
                return;
            }
        } else if let Some(err) = watch_err {
            let event = LuauReloadEvent {
                result: Err(watch_error(path, &err)),
                elapsed_ms: 0.0,
            };
            if evt_tx.send(event).is_err() {
                return;
            }
        }
    }
}

/// Whether a raw event references the target file by name.
fn touches_target(res: &notify::Result<Event>, file_name: Option<&std::ffi::OsStr>) -> bool {
    let Ok(event) = res else {
        return false;
    };
    let Some(name) = file_name else {
        return true;
    };
    event.paths.iter().any(|p| p.file_name() == Some(name))
}

/// Wrap a [`notify::Error`] as a [`LuauError`] naming the target file.
fn watch_error(path: &Path, err: &notify::Error) -> LuauError {
    LuauError {
        file: Some(path.to_path_buf()),
        kind: LuauErrorKind::Io(err.to_string()),
    }
}
