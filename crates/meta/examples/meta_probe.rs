//! Headless SMTC now-playing probe — the diagnostic shipped to a real Windows
//! machine to watch the album-artwork path over a long run and prove where it
//! fails (and that it no longer starves).
//!
//! ```text
//! meta_probe [--seconds N] [--handle-report]
//! ```
//!
//! It runs the real SMTC backend (`scia_meta::smtc`) with a diagnostic trace
//! sink installed and, for `--seconds N` (default 300), writes one timestamped
//! line per event to **stderr**:
//!
//! * session-set changes, per-session `MediaPropertiesChanged` /
//!   `PlaybackInfoChanged` events;
//! * every artwork fetch attempt's step-by-step outcome, and on failure the
//!   exact WinRT stage (`props` / `thumbnail` / `open-read` / `size` /
//!   `create-reader` / `load` / `read-bytes`) with its `HRESULT`;
//! * the `MetaEvent` stream the backend emits (track changes, artwork bytes,
//!   cleared).
//!
//! With `--handle-report` it also logs this process's open handle count via
//! `GetProcessHandleCount` every 30 s, so a handle leak shows up as a monotonic
//! climb correlated with fetches; with the lifecycle fix in place the count
//! stays flat across many tracks.
//!
//! Timestamps are seconds elapsed since the probe started — monotonic, and
//! carrying no wall-clock or machine identity. Track titles are logged as
//! ephemeral context (stderr only, never written to a file).
//!
//! The SMTC backend is Windows-only; on every other platform this probe prints
//! that there is nothing to probe and exits successfully, so it still builds
//! everywhere.
//!
//! Run with: `just _cargo run -p scia-meta --example meta_probe -- --seconds 300 --handle-report`

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    windows_probe::run()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "meta_probe: the SMTC now-playing backend is Windows-only; there is nothing to probe on this platform"
    );
    std::process::ExitCode::SUCCESS
}

#[cfg(windows)]
mod windows_probe {
    use std::process::ExitCode;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use scia_meta::MetaEvent;
    use scia_meta::artwork::RetryPolicy;
    use scia_meta::smtc;

    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    /// How often `--handle-report` samples the process handle count.
    const HANDLE_INTERVAL: Duration = Duration::from_secs(30);

    pub fn run() -> ExitCode {
        let mut seconds: u64 = 300;
        let mut handle_report = false;

        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--handle-report" => handle_report = true,
                "--seconds" => {
                    i += 1;
                    match args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                        Some(n) => seconds = n,
                        None => {
                            eprintln!("--seconds needs a non-negative integer");
                            return ExitCode::from(2);
                        }
                    }
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    eprintln!("usage: meta_probe [--seconds N] [--handle-report]");
                    return ExitCode::from(2);
                }
            }
            i += 1;
        }

        let t0 = Instant::now();

        // The trace sink runs on the backend thread; it captures only a
        // monotonic start instant, so it is `Send` and leaks no identity.
        let tracer: Box<smtc::TraceFn> = Box::new(move |msg: &str| {
            eprintln!("[{:>9.3}s] {msg}", t0.elapsed().as_secs_f64());
        });

        let (tx, rx) = mpsc::channel::<MetaEvent>();
        eprintln!(
            "[{:>9.3}s] meta_probe: starting SMTC backend for {seconds}s (handle-report={handle_report})",
            0.0
        );
        let handle = smtc::start_traced(tx, RetryPolicy::default(), tracer);

        let deadline = t0 + Duration::from_secs(seconds);
        let mut next_handle = t0 + HANDLE_INTERVAL;
        if handle_report {
            log_handles(t0); // a baseline sample at t=0
        }

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if handle_report && now >= next_handle {
                log_handles(t0);
                // Advance to the next slot that is still in the future, so a
                // scheduling hiccup cannot spam catch-up samples.
                while next_handle <= now {
                    next_handle += HANDLE_INTERVAL;
                }
            }

            // Wake for the next handle sample or the deadline, whichever is
            // sooner, but at least often enough to stay responsive.
            let wake = if handle_report {
                next_handle.min(deadline)
            } else {
                deadline
            };
            let wait = wake
                .saturating_duration_since(now)
                .min(Duration::from_millis(500));
            match rx.recv_timeout(wait) {
                Ok(ev) => log_event(t0, &ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        eprintln!(
            "[{:>9.3}s] meta_probe: run complete, stopping backend",
            t0.elapsed().as_secs_f64()
        );
        // Dropping the handle flips the stop flag and joins the backend thread.
        drop(handle);
        ExitCode::SUCCESS
    }

    /// Log one `MetaEvent` from the backend's public stream.
    fn log_event(t0: Instant, ev: &MetaEvent) {
        let el = t0.elapsed().as_secs_f64();
        match ev {
            MetaEvent::TrackChanged(np) => {
                eprintln!(
                    "[{el:>9.3}s] meta: track-changed title={:?} status={:?} app={:?}",
                    np.title, np.status, np.source_app
                );
            }
            MetaEvent::Artwork {
                track_key,
                bytes,
                source_app,
            } => {
                eprintln!(
                    "[{el:>9.3}s] meta: artwork track_key={track_key} bytes={} app={:?}",
                    bytes.len(),
                    source_app
                );
            }
            MetaEvent::Cleared => {
                eprintln!("[{el:>9.3}s] meta: cleared");
            }
        }
    }

    /// Sample and log this process's open handle count.
    fn log_handles(t0: Instant) {
        let el = t0.elapsed().as_secs_f64();
        let mut count: u32 = 0;
        // SAFETY: `GetCurrentProcess` returns the current-process pseudo-handle
        // (no ownership to release); `GetProcessHandleCount` writes the live
        // handle count into `count`, which outlives the call.
        let result = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
        match result {
            Ok(()) => eprintln!("[{el:>9.3}s] handle-report: process_handles={count}"),
            Err(e) => eprintln!(
                "[{el:>9.3}s] handle-report: GetProcessHandleCount failed hresult={} msg={}",
                e.code(),
                e.message()
            ),
        }
    }
}
