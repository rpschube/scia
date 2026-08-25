//! Structured logging setup for the `scia` binary.
//!
//! Logging is **off by default** and costs nothing until a level is resolved:
//! with no subscriber installed, `tracing`'s static max-level short-circuits
//! every callsite before it builds a single field, so the DSP and render threads
//! never pay for logging that is off (the disabled-path no-alloc test in
//! `scia-core` guards this).
//!
//! ## Level resolution
//! The first of these that is set wins, else logging stays off:
//! 1. the `--log <level>` flag,
//! 2. the `SCIA_LOG` environment variable,
//! 3. the config `[log] level` (with `[log] file` toggling the file sink).
//!
//! ## Sinks
//! When a level resolves, events go to a bounded size-rotating JSON-lines file
//! under the scia config dir (`<config_dir>/logs/scia.log`, a few rolled
//! generations kept), and — only when the TUI is **not** driving the terminal
//! (headless / feature-stream / output modes) — also to stderr. The TUI paths
//! never write log lines to stderr, so a log line can never corrupt the screen.
//!
//! ## Privacy
//! Log *messages* carry no usernames, hostnames or filesystem paths; a path may
//! appear only as the location of the user's own log file, never inside a
//! message. Track titles may appear in local logs (documented in
//! `docs/logging.md`); log files never leave the machine.

use std::sync::{Arc, Mutex};

use clap::ValueEnum;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use scia_telemetry::RotatingFile;

/// Log file size bound before rotation, and how many rolled generations to keep.
const LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
const LOG_KEEP: usize = 3;

/// The verbosity levels selectable with `--log` / `SCIA_LOG` / `[log] level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Lifecycle info (device open/switch, scene swaps, connect/disconnect).
    Info,
    /// Detailed diagnostics (per-stage metadata, reload results).
    Debug,
    /// Everything, including the noisiest per-transition traces.
    Trace,
}

impl LogLevel {
    /// The `tracing` [`LevelFilter`] this level selects.
    fn filter(self) -> LevelFilter {
        match self {
            LogLevel::Error => LevelFilter::ERROR,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Trace => LevelFilter::TRACE,
        }
    }

    /// Parse a case-insensitive level name (`error`..`trace`); `None` on an
    /// unrecognised value so the caller can warn and fall through.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" => Some(LogLevel::Debug),
            "trace" => Some(LogLevel::Trace),
            _ => None,
        }
    }
}

/// The config-file `[log]` layer: an optional level and the file-sink toggle.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogConfig {
    /// The `[log] level` value, if a valid one was set.
    pub level: Option<LogLevel>,
    /// The `[log] file` toggle; `None` leaves the file sink on (the default).
    pub file: Option<bool>,
}

/// Resolve the effective level: `--log` > `SCIA_LOG` > `[log] level` > off.
///
/// An unrecognised `SCIA_LOG` value pushes a warning onto `warnings` and is
/// ignored (falling through to the config, then off).
#[must_use]
pub fn resolve_level(
    cli: Option<LogLevel>,
    cfg: &LogConfig,
    warnings: &mut Vec<String>,
) -> Option<LogLevel> {
    if let Some(level) = cli {
        return Some(level);
    }
    if let Some(raw) = std::env::var_os("SCIA_LOG") {
        let raw = raw.to_string_lossy();
        // An empty SCIA_LOG is "unset", not an error.
        if !raw.trim().is_empty() {
            match LogLevel::parse(&raw) {
                Some(level) => return Some(level),
                None => warnings.push(format!(
                    "ignoring SCIA_LOG=\"{raw}\": expected one of error|warn|info|debug|trace"
                )),
            }
        }
    }
    cfg.level
}

/// A `Write`/`MakeWriter` handle over a shared [`RotatingFile`], so the fmt
/// layer can lock-and-write each JSON log line to the rotating sink.
#[derive(Clone)]
struct SharedFile(Arc<Mutex<RotatingFile>>);

impl std::io::Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedFile {
    type Writer = SharedFile;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install the tracing subscriber for the session.
///
/// `level` is the resolved effective level (call [`resolve_level`] first);
/// `None` installs nothing at all — logging stays fully off and free. `tui`
/// tells the setup whether the terminal UI is active, which gates the stderr
/// sink off so a log line can never corrupt the screen. `file_enabled` is the
/// `[log] file` toggle. `config_dir` is where the rotating log file lives; when
/// it is `None` (no known config dir) the file sink is skipped.
///
/// Returns a short human-readable description of where logs are going (for a
/// one-line startup note), or `None` when logging is off or nothing could be
/// wired.
pub fn init(
    level: Option<LogLevel>,
    tui: bool,
    file_enabled: bool,
    config_dir: Option<&std::path::Path>,
) -> Option<String> {
    let level = level?;
    let filter = level.filter();

    // The file sink: a rotating JSON-lines file under <config_dir>/logs.
    let file_layer = if file_enabled {
        config_dir.and_then(|dir| {
            let logs = dir.join("logs");
            match RotatingFile::open(&logs, "scia.log", LOG_MAX_BYTES, LOG_KEEP) {
                Ok(f) => {
                    let shared = SharedFile(Arc::new(Mutex::new(f)));
                    Some((
                        tracing_subscriber::fmt::layer()
                            .json()
                            .with_ansi(false)
                            .with_writer(shared),
                        logs.join("scia.log"),
                    ))
                }
                Err(err) => {
                    eprintln!("logging: cannot open log file in {}: {err}", logs.display());
                    None
                }
            }
        })
    } else {
        None
    };

    // The stderr sink: only when the TUI is not driving the terminal.
    let stderr_layer = (!tui).then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::io::stderr)
    });

    if file_layer.is_none() && stderr_layer.is_none() {
        // Nothing to write to (TUI active and no config dir): stay silent rather
        // than risk corrupting the screen.
        return None;
    }

    let (file_layer, file_path) = file_layer.unzip();

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    let mut sinks = Vec::new();
    if let Some(path) = &file_path {
        sinks.push(format!("file {}", path.display()));
    }
    if !tui {
        sinks.push("stderr".to_string());
    }
    Some(format!("logging at {level:?} → {}", sinks.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_beats_env_and_config() {
        let cfg = LogConfig {
            level: Some(LogLevel::Warn),
            file: None,
        };
        let mut w = Vec::new();
        // A CLI level wins over everything.
        assert_eq!(
            resolve_level(Some(LogLevel::Trace), &cfg, &mut w),
            Some(LogLevel::Trace)
        );
        assert!(w.is_empty());
    }

    #[test]
    fn config_is_the_fallback_when_no_flag_or_env() {
        // SAFETY: single-threaded unit test; no other thread reads the env here.
        unsafe { std::env::remove_var("SCIA_LOG") };
        let cfg = LogConfig {
            level: Some(LogLevel::Info),
            file: None,
        };
        let mut w = Vec::new();
        assert_eq!(resolve_level(None, &cfg, &mut w), Some(LogLevel::Info));
    }

    #[test]
    fn absent_everywhere_is_off() {
        unsafe { std::env::remove_var("SCIA_LOG") };
        let cfg = LogConfig::default();
        let mut w = Vec::new();
        assert_eq!(resolve_level(None, &cfg, &mut w), None);
    }

    #[test]
    fn level_names_parse_case_insensitively() {
        assert_eq!(LogLevel::parse("INFO"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse(" debug "), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("nonsense"), None);
    }
}
