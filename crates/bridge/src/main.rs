//! `scia-bridge` — the Windows-side capture companion.
//!
//! It starts the capture engine on the default device and serves the machine
//! readable feature stream on a TCP socket, so a `scia` running elsewhere (for
//! example inside WSL, where the Windows system mix is not otherwise visible)
//! can render it with `scia --input <addr>`. It is the packaged equivalent of
//! `scia --output binary --listen <addr>` with bridge-appropriate defaults.
//!
//! The output-serving loop itself — the listener, client fan-out, rate pacing
//! and idle keepalive — lives in [`scia_core::stream::run_output`], shared with
//! the main binary so neither side duplicates it. This file is just the CLI and
//! the engine wiring around it. See `docs/wsl.md` and `docs/feature-stream.md`.
//!
//! Exit codes mirror `scia`: `0` success, `1` runtime error, `3` no capture
//! device.

use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use scia_core::stream::run_output;
use scia_core::{
    CaptureError, CpalBackend, DEFAULT_STREAM_RATE, DeviceSelector, Encoding, Engine, EngineConfig,
    EngineError, FeatureReader, Pacing, Signal, StreamHealth, SyntheticBackend,
};

/// The bridge's default listen address: the standard scia bridge port on the
/// loopback interface. Serve on `0.0.0.0:7526` instead to accept a consumer on
/// the (virtual) network, as a WSL guest reaching a Windows host does.
const DEFAULT_LISTEN: &str = "127.0.0.1:7526";

/// Serve scia's audio feature stream to a networked consumer.
#[derive(Parser, Debug)]
#[command(name = "scia-bridge", version, about, long_about = None)]
struct Cli {
    /// TCP address to serve the feature stream on. Every connected client
    /// receives the stream from the moment it connects. Use `0.0.0.0:7526` to
    /// accept a consumer from the (virtual) network, e.g. a scia inside WSL.
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_LISTEN)]
    listen: String,

    /// Feature frames per second (1..=1000). While the engine is idle the stream
    /// drops to a slower keepalive cadence regardless.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_STREAM_RATE)]
    rate: u32,

    /// Wire encoding: length-prefixed `binary` (the default) or line-delimited
    /// `json`.
    #[arg(long, value_enum, value_name = "FORMAT", default_value_t = BridgeEncoding::Binary)]
    encoding: BridgeEncoding,

    /// Serve the built-in synthetic feed instead of capturing real audio (no
    /// audio hardware needed — for testing the wire path).
    #[arg(long)]
    demo: bool,
}

/// The wire encodings selectable with `--encoding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum BridgeEncoding {
    /// NDJSON: one feature-frame object per line.
    Json,
    /// Length-prefixed little-endian binary with a one-time stream header.
    Binary,
}

impl BridgeEncoding {
    /// The [`Encoding`] this flag value selects.
    fn encoding(self) -> Encoding {
        match self {
            BridgeEncoding::Json => Encoding::Json,
            BridgeEncoding::Binary => Encoding::Binary,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rate = cli.rate.clamp(1, 1000);

    let (engine, reader) = match start_engine(&cli) {
        Ok(pair) => pair,
        Err(code) => return code,
    };

    run_output(
        cli.encoding.encoding(),
        Some(cli.listen.clone()),
        rate,
        None,
        engine,
        reader,
    )
}

/// Start the capture engine the bridge serves from: the synthetic feed under
/// `--demo`, otherwise live capture on the default device. Capture info goes to
/// stderr so it never contaminates the served stream.
fn start_engine(cli: &Cli) -> Result<(Engine, FeatureReader), ExitCode> {
    if cli.demo {
        let backend = SyntheticBackend {
            signal: Signal::Music { bpm: 112.0 },
            pacing: Pacing::Realtime,
            ..SyntheticBackend::default()
        };
        return Engine::start(Box::new(backend), EngineConfig::default()).map_err(|err| {
            eprintln!("failed to start engine: {err}");
            ExitCode::from(1)
        });
    }

    let backend = CpalBackend {
        device: DeviceSelector::Default,
        prefer_pipewire: true,
    };
    match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok((engine, reader)) => {
            let format = engine.format();
            eprintln!("capture: {} Hz, {} ch", format.sample_rate, format.channels);
            if let StreamHealth::Errored(msg) = engine.health() {
                eprintln!("capture stream error: {msg}");
                engine.stop();
                return Err(ExitCode::from(1));
            }
            Ok((engine, reader))
        }
        Err(EngineError::Capture(CaptureError::NoDevice)) => {
            eprintln!("no capture device available; try --demo");
            Err(ExitCode::from(3))
        }
        Err(err) => {
            eprintln!("failed to start capture: {err}");
            Err(ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_bridge_contract() {
        let cli = Cli::try_parse_from(["scia-bridge"]).expect("parses with no args");
        assert_eq!(cli.listen, "127.0.0.1:7526", "default listen address");
        assert_eq!(cli.rate, DEFAULT_STREAM_RATE, "default rate is 60");
        assert_eq!(
            cli.encoding,
            BridgeEncoding::Binary,
            "default encoding is binary"
        );
        assert!(!cli.demo, "demo is off by default");
    }

    #[test]
    fn encoding_flag_parses_both_values() {
        let json = Cli::try_parse_from(["scia-bridge", "--encoding", "json"]).expect("parses");
        assert_eq!(json.encoding, BridgeEncoding::Json);
        assert_eq!(json.encoding.encoding(), Encoding::Json);
        let binary = Cli::try_parse_from(["scia-bridge", "--encoding", "binary"]).expect("parses");
        assert_eq!(binary.encoding.encoding(), Encoding::Binary);
    }

    #[test]
    fn listen_rate_and_demo_flags_parse() {
        let cli = Cli::try_parse_from([
            "scia-bridge",
            "--listen",
            "0.0.0.0:9000",
            "--rate",
            "30",
            "--demo",
        ])
        .expect("parses");
        assert_eq!(cli.listen, "0.0.0.0:9000");
        assert_eq!(cli.rate, 30);
        assert!(cli.demo);
    }

    #[test]
    fn unknown_encoding_is_rejected() {
        assert!(Cli::try_parse_from(["scia-bridge", "--encoding", "protobuf"]).is_err());
    }
}
