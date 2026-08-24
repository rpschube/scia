//! No-hardware smoke test for the display spectrum: run the engine on a
//! synthetic 1 kHz sine for one second and print an ASCII bar row every 100 ms,
//! plus the peak bar index, its value and the current AGC gain.
//!
//! Run with: `just _cargo run -p scia-core --example spectrum_bars`

use std::thread::sleep;
use std::time::Duration;

use scia_core::{Engine, EngineConfig, Pacing, Signal, StreamFormat, SyntheticBackend};

const RAMP: &[u8] = b" .:-=+*#%@";

fn main() {
    let backend = SyntheticBackend {
        format: StreamFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        signal: Signal::Sine {
            hz: 1_000.0,
            amp: 0.5,
        },
        pacing: Pacing::Realtime,

        emit_log: None,
    };

    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    for _ in 0..10 {
        sleep(Duration::from_millis(100));
        let snapshot = *reader.latest();
        let bars = &snapshot.spectrum[..snapshot.spectrum_len as usize];

        // One ASCII row: one glyph per bar, height by its 0..=1 value.
        let mut row = String::with_capacity(bars.len());
        for &v in bars {
            let idx = ((v.clamp(0.0, 1.0) * (RAMP.len() - 1) as f32).round()) as usize;
            row.push(RAMP[idx] as char);
        }

        let (peak_bar, peak_val) = bars.iter().enumerate().fold(
            (0usize, 0.0f32),
            |(bi, bv), (i, &v)| {
                if v > bv { (i, v) } else { (bi, bv) }
            },
        );

        println!(
            "gen={:>5} |{row}| peak=bar{peak_bar} ({peak_val:.3}) gain={:.2}",
            snapshot.generation,
            engine.stats().agc_gain,
        );
    }

    engine.stop();
}
