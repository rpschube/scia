//! `scia` command-line entry point.
//!
//! Parses the CLI, and — with `--demo` — starts the engine on the built-in
//! synthetic feed and hands its feature bus to the terminal frontend. Live
//! capture is not wired yet; without `--demo` the binary reports that and
//! exits with a usage code. Exit codes: `0` success, `1` runtime error,
//! `2` usage / unsupported.

use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use scia_core::{Engine, EngineConfig, Pacing, Signal, SyntheticBackend};
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

    if !cli.demo {
        eprintln!("live capture is not implemented yet; run with --demo");
        return ExitCode::from(2);
    }

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
        frames: cli.frames,
        debug: cli.debug,
    };

    let outcome = run(reader, || engine.stats(), opts);
    engine.stop();

    match outcome {
        Ok(summary) => {
            eprintln!(
                "frames={} p50={:.2}ms p99={:.2}ms",
                summary.frames, summary.p50_frame_ms, summary.p99_frame_ms
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("runtime error: {err}");
            ExitCode::from(1)
        }
    }
}
