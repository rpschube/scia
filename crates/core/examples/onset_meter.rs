//! No-hardware smoke test for onsets and band split.
//!
//! Part 1 runs a synthetic 120 bpm click train in real time for 3 s and prints
//! one line per detected onset with its timestamp and the inter-onset interval
//! (which should hover around 500 ms). Part 2 runs a 60 Hz, a 1 kHz and a 5 kHz
//! sine in turn and prints the three band ratios for each, showing the tone
//! landing in the bass, mid and treble band respectively.
//!
//! Run with: `just _cargo run -p scia-core --example onset_meter`

use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{
    Engine, EngineConfig, HopProcessor, Pacing, Signal, StreamFormat, SyntheticBackend, sample_ring,
};

const FORMAT: StreamFormat = StreamFormat {
    sample_rate: 48_000,
    channels: 2,
};

fn main() {
    println!("== onsets: 120 bpm clicks, 3 s (expect ~500 ms inter-onset) ==");
    let backend = SyntheticBackend {
        format: FORMAT,
        signal: Signal::Clicks {
            bpm: 120.0,
            amp: 0.8,
        },
        pacing: Pacing::Realtime,
    };
    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    let dt = 256.0 / f64::from(FORMAT.sample_rate); // seconds per hop
    let mut last_gen = 0u64;
    let mut last_onset_s: Option<f64> = None;
    // Poll well faster than the ~5.3 ms hop period so no onset hop is missed.
    for _ in 0..3_000 {
        sleep(Duration::from_millis(1));
        let snap = *reader.latest();
        if snap.generation == last_gen {
            continue;
        }
        last_gen = snap.generation;
        if snap.onset {
            let t = snap.generation as f64 * dt;
            match last_onset_s {
                Some(prev) => println!("onset @ {:7.3} s   ioi {:6.1} ms", t, (t - prev) * 1000.0),
                None => println!("onset @ {t:7.3} s   ioi     -- ms  (first)"),
            }
            last_onset_s = Some(t);
        }
    }
    engine.stop();

    // Bands: drive the processor directly for 0.5 s of each tone so the result
    // is deterministic. `levels` are the raw linear band energies (they show
    // plainly which band the tone lands in); `bands` are those energies
    // normalized against each band's running average (a swell reads > 1).
    println!("\n== bands: pure tones ==");
    println!(
        "{:>9}  {:>28}  {:>22}",
        "tone", "levels [bass, mid, treble]", "ratios"
    );
    let sr = FORMAT.sample_rate;
    let hop = 256usize;
    for (hz, label) in [(60.0f32, "bass"), (1_000.0, "mid"), (5_000.0, "treble")] {
        let mut p = HopProcessor::new(hop, 2, sr);
        let (mut sink, mut consumer) = sample_ring(Instant::now());
        let mut buf = vec![0.0f32; hop * 2];
        let mut frame = 0u64;
        let mut last = None;
        for _ in 0..((0.5 * f64::from(sr) / hop as f64) as usize) {
            for f in 0..hop {
                let t = (frame + f as u64) as f64 / f64::from(sr);
                let s = (0.5 * (2.0 * std::f64::consts::PI * f64::from(hz) * t).sin()) as f32;
                buf[f * 2] = s;
                buf[f * 2 + 1] = s;
            }
            sink.push(&buf);
            last = p.try_process(&mut consumer, FORMAT, 0, 0).or(last);
            frame += hop as u64;
        }
        let levels = p.band_levels();
        let bands = last.map(|s| s.bands).unwrap_or_default();
        println!(
            "{hz:>6.0} Hz  [{:>8.4}, {:>8.4}, {:>8.4}]  [{:.2}, {:.2}, {:.2}]  ({label})",
            levels[0], levels[1], levels[2], bands[0], bands[1], bands[2],
        );
    }
}
