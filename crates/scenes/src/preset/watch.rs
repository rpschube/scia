//! A filesystem watcher for a preset file: notice a save, debounce the burst an
//! editor makes, re-read and re-validate the file off the render thread, and
//! hand the result back as a [`ReloadEvent`].
//!
//! Editors rarely write a file in place; many replace it by writing a temporary
//! sibling and renaming it over the target. Watching the file itself would miss
//! that (the inode we watched is gone), so [`PresetWatcher::start`] watches the
//! file's **parent directory** and filters events down to the target file name.
//! A burst of raw events (truncate, write, rename, attribute change) is
//! coalesced by a short debounce so one save produces one [`ReloadEvent`].
//!
//! The work runs on a dedicated thread named `scia-watch`. It never panics: a
//! read or validation failure travels as a [`ReloadEvent`] carrying the
//! [`PresetError`], and a watch-backend error after start does the same rather
//! than tearing anything down. The watcher stops when the [`PresetWatcher`] is
//! dropped: dropping it releases the OS watch, the event channel disconnects,
//! and the thread joins.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher};

use super::{Preset, PresetError, PresetErrorKind, load_preset};

/// How long to coalesce a burst of raw filesystem events before re-reading. An
/// editor's save is several events (truncate, write, rename, chmod) within a
/// few milliseconds; one debounce window turns them into a single reload.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// The outcome of one preset reload: either the freshly validated [`Preset`] or
/// the [`PresetError`] that reading/validating it produced, plus how long the
/// read-and-validate took.
#[derive(Debug)]
pub struct ReloadEvent {
    /// The re-validated preset, or the error that reading/validating it raised.
    pub result: Result<Preset, PresetError>,
    /// Wall-clock milliseconds spent reading and validating the file.
    pub elapsed_ms: f32,
}

/// A live watch on a preset file. Hold it for as long as reloads are wanted;
/// dropping it stops the watch and joins the worker thread.
///
/// Build one with [`PresetWatcher::start`], which also returns the
/// [`Receiver`] the reloads arrive on.
pub struct PresetWatcher {
    /// The OS watcher. Dropped first (in [`Drop`]) so the raw-event channel
    /// disconnects and the worker thread observes the shutdown.
    watcher: Option<notify::RecommendedWatcher>,
    /// The worker thread handle, joined on drop.
    handle: Option<JoinHandle<()>>,
}

impl PresetWatcher {
    /// Start watching `path`, returning the watcher and the channel its reloads
    /// arrive on.
    ///
    /// The watch is placed on `path`'s parent directory (its own directory when
    /// it has no parent component) so an editor's rename-replace is caught, and
    /// raw events are filtered to `path`'s file name. The first reload arrives
    /// only after the file next changes; the current contents are not replayed.
    ///
    /// # Errors
    ///
    /// Returns a [`PresetError`] if the watch backend cannot be created or the
    /// directory cannot be watched. Failures that happen *after* a successful
    /// start travel as a [`ReloadEvent`] with an error, never as a panic.
    pub fn start(path: &Path) -> Result<(Self, Receiver<ReloadEvent>), PresetError> {
        let path = path.to_path_buf();
        // The directory to watch, and the file name to filter on.
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let file_name = path.file_name().map(std::ffi::OsString::from);

        // Raw events from the OS watcher flow through this channel to the worker.
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            // The receiver hangs up when the worker exits; ignore the send error.
            let _ = raw_tx.send(res);
        })
        .map_err(|e| watch_error(&path, &e))?;
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|e| watch_error(&path, &e))?;

        // Validated reloads flow to the caller over this channel.
        let (evt_tx, evt_rx) = mpsc::channel::<ReloadEvent>();
        let worker_path = path.clone();
        let handle = std::thread::Builder::new()
            .name("scia-watch".to_string())
            .spawn(move || run_worker(&raw_rx, &evt_tx, &worker_path, file_name.as_deref()))
            .map_err(|e| PresetError {
                file: Some(path.clone()),
                line: None,
                col: None,
                kind: PresetErrorKind::Io(e.to_string()),
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

impl Drop for PresetWatcher {
    fn drop(&mut self) {
        // Release the OS watch first so the raw-event channel disconnects; the
        // worker then falls out of its receive loop and the join returns.
        self.watcher.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The worker loop: block for a raw event, coalesce the burst it belongs to,
/// then (if the target file was touched, or a watch error arrived) re-read and
/// validate and forward one [`ReloadEvent`]. Returns when the raw channel
/// disconnects (the watcher was dropped) or the caller hung up.
fn run_worker(
    raw_rx: &Receiver<notify::Result<Event>>,
    evt_tx: &Sender<ReloadEvent>,
    path: &Path,
    file_name: Option<&std::ffi::OsStr>,
) {
    loop {
        // Block until something happens or the watcher is dropped.
        let first = match raw_rx.recv() {
            Ok(res) => res,
            Err(_) => return, // watcher dropped: shut down
        };
        let mut touched = touches_target(&first, file_name);
        let mut watch_err = first.err();

        // Coalesce the rest of the burst within the debounce window.
        loop {
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(res) => {
                    touched |= touches_target(&res, file_name);
                    if watch_err.is_none() {
                        watch_err = res.err();
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    // Watcher dropped mid-burst: process what we have, then exit
                    // after the send below.
                    break;
                }
            }
        }

        if touched {
            // Re-read and re-validate, timing just that work.
            let start = Instant::now();
            let result = load_preset(path);
            let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;
            if evt_tx.send(ReloadEvent { result, elapsed_ms }).is_err() {
                return; // caller hung up
            }
        } else if let Some(err) = watch_err {
            // A backend error unrelated to the file: surface it, don't crash.
            let event = ReloadEvent {
                result: Err(watch_error(path, &err)),
                elapsed_ms: 0.0,
            };
            if evt_tx.send(event).is_err() {
                return;
            }
        }
        // A disconnect during the burst is caught by the blocking `recv` at the
        // top of the next iteration, which returns and shuts the worker down.
    }
}

/// Whether a raw event references the target file by name. A backend error (no
/// paths) is not a match; it is handled separately.
fn touches_target(res: &notify::Result<Event>, file_name: Option<&std::ffi::OsStr>) -> bool {
    let Ok(event) = res else {
        return false;
    };
    let Some(name) = file_name else {
        // No file-name filter: any event in the directory counts.
        return true;
    };
    event.paths.iter().any(|p| p.file_name() == Some(name))
}

/// Wrap a [`notify::Error`] as a [`PresetError`] naming the target file.
fn watch_error(path: &Path, err: &notify::Error) -> PresetError {
    PresetError {
        file: Some(path.to_path_buf()),
        line: None,
        col: None,
        kind: PresetErrorKind::Io(err.to_string()),
    }
}
