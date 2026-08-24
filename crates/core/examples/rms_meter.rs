//! No-hardware smoke test: run the engine on a synthetic 220 Hz sine for one
//! second and print an RMS/peak line every 100 ms.
//!
//! Run with: `just _cargo run -p scia-core --example rms_meter`

use std::thread::sleep;
use std::time::Duration;

use scia_core::{Engine, EngineConfig, Pacing, Signal, StreamFormat, SyntheticBackend};

fn main() {
    let backend = SyntheticBackend {
        format: StreamFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        signal: Signal::Sine {
            hz: 220.0,
            amp: 0.5,
        },
        pacing: Pacing::Realtime,

        emit_log: None,
    };

    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    for _ in 0..10 {
        sleep(Duration::from_millis(100));
        let snapshot = reader.latest();
        println!(
            "gen={:>5} rms={:.4} peak={:.4} starved={} dropped_frames={}",
            snapshot.generation,
            snapshot.rms,
            snapshot.peak,
            snapshot.starved,
            snapshot.dropped_frames,
        );
    }

    engine.stop();
}
