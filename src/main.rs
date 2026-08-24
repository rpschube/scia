//! `scia` command-line entry point.
//!
//! Parses the CLI and starts the engine. With `--demo` the built-in synthetic
//! feed drives the pipeline; otherwise `scia` captures real system audio through
//! the cpal backend. The feature bus feeds either the terminal frontend or, with
//! `--headless`, a once-a-second status line on stderr. `--list-devices` prints
//! the device table and exits. Exit codes: `0` success, `1` runtime error, `2`
//! usage / unsupported, `3` no capture device.

use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use scia_core::{
    Activity, CaptureError, CpalBackend, DeviceKind, DeviceSelector, Engine, EngineConfig,
    EngineError, FeatureReader, Pacing, Signal, StreamHealth, SyntheticBackend, list_devices,
};
use scia_tui::{TuiOptions, run};

/// A live, terminal audio spectrum.
#[derive(Parser, Debug)]
#[command(name = "scia", version, about, long_about = None)]
struct Cli {
    /// Use the built-in synthetic feed (no audio capture).
    #[arg(long)]
    demo: bool,

    /// Which synthetic waveform the demo feed generates.
    #[arg(long, value_enum, default_value_t = DemoSignal::Sine)]
    demo_signal: DemoSignal,

    /// Capture device name (exact match from --list-devices). Defaults to the
    /// system mix (Windows loopback / PipeWire sink) or the default input.
    #[arg(long)]
    device: Option<String>,

    /// Prefer the PipeWire host on Linux (the sink monitor = system mix) when
    /// built with the `capture-pipewire` feature. This is the default.
    #[arg(long, overrides_with = "no_pipewire")]
    pipewire: bool,

    /// Use the default host (ALSA on Linux) instead of PipeWire.
    #[arg(long, overrides_with = "pipewire")]
    no_pipewire: bool,

    /// Print the host/device table and exit.
    #[arg(long)]
    list_devices: bool,

    /// No TUI: print one status line per second to stderr until --seconds
    /// elapses (or the process is killed).
    #[arg(long)]
    headless: bool,

    /// With --headless, exit after N seconds. `0` (the default) runs until the
    /// process is killed.
    #[arg(long, default_value_t = 0)]
    seconds: u64,

    /// Target frame rate.
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Exit after N rendered frames (testing).
    #[arg(long)]
    frames: Option<u64>,

    /// Start with the debug line visible.
    #[arg(long)]
    debug: bool,
}

/// The synthetic waveform choices for `--demo`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum DemoSignal {
    /// A 220 Hz sine at amplitude 0.5.
    Sine,
    /// 120 bpm clicks at amplitude 0.8.
    Clicks,
}

impl DemoSignal {
    fn signal(self) -> Signal {
        match self {
            DemoSignal::Sine => Signal::Sine {
                hz: 220.0,
                amp: 0.5,
            },
            DemoSignal::Clicks => Signal::Clicks {
                bpm: 120.0,
                amp: 0.8,
            },
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list_devices {
        return print_device_table();
    }

    if cli.demo {
        run_demo(&cli)
    } else {
        run_live(&cli)
    }
}

/// Print every device on every cpal host and exit 0. Enumeration failure is a
/// runtime error (exit 1).
fn print_device_table() -> ExitCode {
    let devices = match list_devices() {
        Ok(devices) => devices,
        Err(err) => {
            eprintln!("device enumeration failed: {err}");
            return ExitCode::from(1);
        }
    };
    println!("{:<8}  {:<8}  {:<7}  name", "host", "kind", "default");
    for d in &devices {
        let kind = match d.kind {
            DeviceKind::Input => "input",
            DeviceKind::Output => "output",
        };
        let default = if d.is_default_input {
            "in"
        } else if d.is_default_output {
            "out"
        } else {
            ""
        };
        println!("{:<8}  {:<8}  {:<7}  {}", d.host, kind, default, d.name);
    }
    println!("\n{} device(s) across all hosts", devices.len());
    ExitCode::SUCCESS
}

/// Start the engine on the built-in synthetic feed and run the TUI.
fn run_demo(cli: &Cli) -> ExitCode {
    let backend = SyntheticBackend {
        signal: cli.demo_signal.signal(),
        pacing: Pacing::Realtime,
        ..SyntheticBackend::default()
    };

    let (engine, reader) = match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("failed to start engine: {err}");
            return ExitCode::from(1);
        }
    };

    let opts = TuiOptions {
        fps: cli.fps,
        label: Some("DEMO — synthetic feed".to_string()),
        source: String::new(),
        frames: cli.frames,
        debug: cli.debug,
    };

    let outcome = run(reader, || engine.stats(), || engine.health(), opts);
    engine.stop();
    report_tui_outcome(outcome)
}

/// Start live capture on the cpal backend and run the TUI or the headless
/// status loop.
fn run_live(cli: &Cli) -> ExitCode {
    let selector = match &cli.device {
        Some(name) => DeviceSelector::Named(name.clone()),
        None => DeviceSelector::Default,
    };
    // With the mutually-exclusive flags, later wins; the default (neither set)
    // prefers PipeWire.
    let prefer_pipewire = cli.pipewire || !cli.no_pipewire;
    let backend = CpalBackend {
        device: selector,
        prefer_pipewire,
    };

    let (engine, reader) = match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok(pair) => pair,
        Err(EngineError::Capture(CaptureError::NoDevice)) => {
            eprintln!("no capture device available; try --list-devices, or --demo");
            return ExitCode::from(3);
        }
        Err(err) => {
            eprintln!("failed to start capture: {err}");
            return ExitCode::from(1);
        }
    };

    // Print the negotiated format once. The host is a best-effort lookup: known
    // for a named device, omitted otherwise.
    let format = engine.format();
    match capture_host(&cli.device) {
        Some(host) => eprintln!(
            "capture: {} Hz, {} ch via {}",
            format.sample_rate, format.channels, host
        ),
        None => eprintln!("capture: {} Hz, {} ch", format.sample_rate, format.channels),
    }

    // A fault reported at open aborts before the frontend takes the terminal.
    if let StreamHealth::Errored(msg) = engine.health() {
        eprintln!("capture stream error: {msg}");
        engine.stop();
        return ExitCode::from(1);
    }

    if cli.headless {
        return run_headless(engine, reader, cli.seconds);
    }

    let opts = TuiOptions {
        fps: cli.fps,
        label: None,
        source: format!("{} Hz {} ch", format.sample_rate, format.channels),
        frames: cli.frames,
        debug: cli.debug,
    };

    let outcome = run(reader, || engine.stats(), || engine.health(), opts);
    engine.stop();
    report_tui_outcome(outcome)
}

/// Report a completed TUI run: the timing summary on success, the stream error
/// (exit 1) when the loop aborted, or the I/O error (exit 1).
fn report_tui_outcome(outcome: std::io::Result<scia_tui::RunSummary>) -> ExitCode {
    match outcome {
        Ok(summary) => {
            eprintln!(
                "frames={} p50={:.2}ms p99={:.2}ms",
                summary.frames, summary.p50_frame_ms, summary.p99_frame_ms
            );
            if let Some(msg) = summary.error {
                eprintln!("capture stream error: {msg}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("runtime error: {err}");
            ExitCode::from(1)
        }
    }
}

/// Headless status loop: one line per second on stderr with the same numbers
/// the TUI debug line reports. Runs `seconds` seconds, or until killed when
/// `seconds` is `0`. Returns exit 1 if the stream faults.
fn run_headless(engine: Engine, mut reader: FeatureReader, seconds: u64) -> ExitCode {
    let mut t: u64 = 0;
    loop {
        sleep(Duration::from_secs(1));
        t += 1;

        let stats = engine.stats();
        let snap = *reader.latest();
        let (bar, val) = loudest_bar(&snap.spectrum[..snap.spectrum_len as usize]);
        eprintln!(
            "act {}  gen {}  rms {:.4}  peak {:.4}  loudest {}({:.3})  push {}  gap {:.1}ms  \
             dropped {}",
            activity_label(stats.activity),
            snap.generation,
            snap.rms,
            snap.peak,
            bar,
            val,
            stats.pushes,
            stats.max_gap_ms,
            stats.dropped_frames,
        );

        if let StreamHealth::Errored(msg) = engine.health() {
            eprintln!("capture stream error: {msg}");
            engine.stop();
            return ExitCode::from(1);
        }

        if seconds != 0 && t >= seconds {
            break;
        }
    }
    engine.stop();
    ExitCode::SUCCESS
}

/// Best-effort host name for the capture device. Known for a named device (its
/// host in the device table); `None` for the platform default, where the
/// backend chooses the host and no accessor exposes it.
fn capture_host(device: &Option<String>) -> Option<String> {
    let name = device.as_ref()?;
    let devices = list_devices().ok()?;
    devices
        .into_iter()
        .find(|d| &d.name == name)
        .map(|d| d.host)
}

/// The short indicator word for an [`Activity`], matching the TUI header.
fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Active => "active",
        Activity::Quiet => "quiet",
        Activity::Idle => "idle",
    }
}

/// Index and value of the loudest display-spectrum bar (`(0, 0.0)` when empty).
fn loudest_bar(spectrum: &[f32]) -> (usize, f32) {
    let mut best = 0usize;
    let mut best_val = 0.0f32;
    for (i, &v) in spectrum.iter().enumerate() {
        if v > best_val {
            best_val = v;
            best = i;
        }
    }
    (best, best_val)
}
