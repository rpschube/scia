//! Headless beat-tracker calibration probe — the diagnostic shipped to real
//! machines to measure the confidence pipeline on live streamed music.
//!
//! ```text
//! beat_probe [--device NAME] [--seconds N] [--list]
//! ```
//!
//! With `--list` it prints every device on every cpal host and exits. Otherwise
//! it starts the normal engine on [`CpalBackend`], then watches the published
//! feature stream. Every hop's onset detection function (the spectral flux the
//! snapshot already carries) is fed into a mirror [`BeatTracker`] constructed for
//! the same format — the identical, deterministic per-hop computation the DSP
//! thread runs — so the tracker's internals become observable through the
//! read-only [`BeatTracker::debug_stats`] surface without disturbing the engine
//! or the published snapshot. The published tempo column is read straight off
//! the engine's snapshot; the internal columns come from the mirror fed that
//! same ODF, so the two agree hop for hop unless a hop is missed (reported).
//!
//! Once per second it prints one aligned status line — elapsed, RMS, the
//! short-term ODF level, the ODF-window kurtosis, the raw comb energy at the
//! winning period, that period's candidate tempo, the smoothed confidence, the
//! lock flag and the engine's published tempo. At each induction pass it prints
//! a compact line with the top three comb candidates (`bpm:score`). At the end
//! it prints a summary: min/median/max of kurtosis and confidence over the run,
//! the fraction of hops locked, and the modal candidate tempo.
//!
//! This never retunes anything: it only measures. The tracker's constants, the
//! DSP chain and the published snapshot semantics are untouched.
//!
//! Exit codes: `0` on success; `3` (with a message) when no capture device is
//! available or the backend cannot open one.
//!
//! Run with: `just _cargo run -p scia-core --example beat_probe -- --seconds 30`

use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{
    BeatDebug, BeatTracker, CaptureError, CpalBackend, DeviceKind, DeviceSelector, Engine,
    EngineConfig, EngineError, StreamHealth, list_devices,
};

/// Frames per hop the pipeline runs on — fixed at 256 across the codebase.
const HOP_FRAMES: usize = 256;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut device: Option<String> = None;
    let mut seconds: u64 = 60;
    let mut do_list = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => do_list = true,
            "--device" => {
                i += 1;
                let Some(name) = args.get(i) else {
                    eprintln!("--device needs a NAME");
                    return ExitCode::from(2);
                };
                device = Some(name.clone());
            }
            "--seconds" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    Some(n) if n > 0 => seconds = n,
                    _ => {
                        eprintln!("--seconds needs a positive integer");
                        return ExitCode::from(2);
                    }
                }
            }
            "-h" | "--help" => {
                println!("usage: beat_probe [--device NAME] [--seconds N=60] [--list]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    if do_list {
        return list();
    }
    probe(device, seconds)
}

/// Print the device table (`--list`), mirroring the other probes.
fn list() -> ExitCode {
    match list_devices() {
        Ok(devices) => {
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
        Err(CaptureError::NoDevice) => {
            eprintln!("no devices found on any host");
            ExitCode::from(3)
        }
        Err(e) => {
            eprintln!("device enumeration failed: {e}");
            ExitCode::from(3)
        }
    }
}

/// Run the beat probe for `seconds` seconds against the system mix.
fn probe(device: Option<String>, seconds: u64) -> ExitCode {
    let selector = device
        .clone()
        .map_or(DeviceSelector::Default, DeviceSelector::Named);
    let backend = CpalBackend {
        device: selector,
        prefer_pipewire: true,
    };

    let (engine, mut reader) = match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok(pair) => pair,
        Err(EngineError::Capture(CaptureError::NoDevice)) => {
            eprintln!(
                "no capture device available{}",
                device
                    .map(|d| format!(" for --device {d}"))
                    .unwrap_or_default()
            );
            eprintln!(
                "on plain ALSA the default input is a microphone; the system mix needs \
                 PipeWire or a named loopback/monitor device (see --list)"
            );
            return ExitCode::from(3);
        }
        Err(e) => {
            eprintln!("could not start capture: {e}");
            return ExitCode::from(3);
        }
    };

    let mut format = engine.format();
    println!(
        "negotiated stream: {} Hz, {} channel(s)",
        format.sample_rate, format.channels
    );
    if let StreamHealth::Errored(msg) = engine.health() {
        eprintln!("warning: stream reported an error at open: {msg}");
    }
    println!(
        "probing beat tracker for {seconds}s (per-second: t · rms · odf · kurt · comb · \
         cand · conf · locked · published bpm)"
    );

    // The mirror tracker: fed the published ODF hop for hop, so its induction
    // internals mirror the engine's tracker exactly (deterministic per-hop math).
    let mut tracker = BeatTracker::new(format.sample_rate, HOP_FRAMES);

    // Run accumulators for the summary.
    let mut conf_samples: Vec<f32> = Vec::new();
    let mut kurt_samples: Vec<f32> = Vec::new();
    let mut cand_bpm_bins: Vec<i32> = Vec::new();
    let mut locked_hops: u64 = 0;
    let mut total_hops: u64 = 0;
    let mut skipped_hops: u64 = 0;

    let start = Instant::now();
    let run = Duration::from_secs(seconds);
    let mut last_gen: Option<u64> = None;
    let mut last_induction: u64 = 0;
    let mut next_tick = Duration::from_secs(1);
    let mut stream_errored = false;

    while start.elapsed() < run {
        let snap = *reader.latest();

        // A format renegotiation (a device switch) rebuilds the engine's tracker;
        // rebuild the mirror to match so the geometry and cadence stay aligned.
        if snap.sample_rate != 0 && snap.sample_rate != format.sample_rate {
            format = engine.format();
            tracker = BeatTracker::new(format.sample_rate, HOP_FRAMES);
            last_induction = 0;
            println!("-- stream reformatted to {} Hz --", format.sample_rate);
        }

        if last_gen != Some(snap.generation) {
            if let Some(prev) = last_gen {
                if snap.generation > prev + 1 {
                    skipped_hops += snap.generation - prev - 1;
                }
            }
            last_gen = Some(snap.generation);

            // Feed the mirror tracker the same ODF the engine's tracker saw.
            tracker.process_hop(snap.flux);
            let dbg = tracker.debug_stats();

            conf_samples.push(dbg.confidence);
            total_hops += 1;
            if dbg.locked {
                locked_hops += 1;
            }

            if dbg.inductions != last_induction {
                last_induction = dbg.inductions;
                kurt_samples.push(dbg.kurtosis);
                if dbg.candidate_bpm > 0.0 {
                    cand_bpm_bins.push(dbg.candidate_bpm.round() as i32);
                }
                print_induction(start.elapsed().as_secs_f32(), &dbg);
            }
        }

        if let StreamHealth::Errored(msg) = engine.health() {
            eprintln!("stream error during capture: {msg}");
            stream_errored = true;
            break;
        }

        if start.elapsed() >= next_tick {
            let dbg = tracker.debug_stats();
            print_status(next_tick.as_secs(), snap.rms, snap.tempo_bpm, &dbg);
            next_tick += Duration::from_secs(1);
        }

        sleep(Duration::from_millis(1));
    }

    engine.stop();
    print_summary(
        &conf_samples,
        &kurt_samples,
        &cand_bpm_bins,
        locked_hops,
        total_hops,
        skipped_hops,
    );

    if total_hops == 0 {
        eprintln!(
            "\nno hops were published in {seconds}s — the device opened but delivered no audio"
        );
        return ExitCode::from(3);
    }
    if stream_errored {
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

/// One aligned per-second status line. `pub_bpm` is the engine's published tempo
/// (0 while unlocked); every other internal comes from the mirror tracker.
fn print_status(t: u64, rms: f32, pub_bpm: f32, dbg: &BeatDebug) {
    println!(
        "t={t:>3}s  rms={rms:>7.4}  odf={:>8.5}  kurt={:>6.2}  comb={:>6.3}  \
         cand={:>6.1}bpm  conf={:>5.3}  locked={:>3}  published={:>6.1}bpm",
        dbg.odf_level,
        dbg.kurtosis,
        dbg.comb_energy,
        dbg.candidate_bpm,
        dbg.confidence,
        if dbg.locked { "yes" } else { "no" },
        pub_bpm,
    );
}

/// Compact per-induction line: the top three comb candidates as `bpm:score`.
fn print_induction(t: f32, dbg: &BeatDebug) {
    let mut cands = String::new();
    for (i, c) in dbg.top.iter().enumerate() {
        if c.score <= 0.0 {
            continue;
        }
        if i > 0 && !cands.is_empty() {
            cands.push_str("  ");
        }
        cands.push_str(&format!("{:.0}:{:.3}", c.bpm, c.score));
    }
    if cands.is_empty() {
        cands.push_str("(no candidates)");
    }
    println!("  induct#{:<4} t={t:>5.1}s  top: {cands}", dbg.inductions);
}

/// The closing summary block: distribution of kurtosis and confidence, the
/// fraction of hops locked, and the modal candidate tempo.
fn print_summary(
    conf: &[f32],
    kurt: &[f32],
    cand_bins: &[i32],
    locked_hops: u64,
    total_hops: u64,
    skipped_hops: u64,
) {
    println!("\n== summary ==");
    println!("hops observed:       {total_hops}");
    println!("hops skipped:        {skipped_hops}");
    println!("induction passes:    {}", kurt.len());
    print_dist("kurtosis", kurt);
    print_dist("confidence", conf);
    let frac = if total_hops == 0 {
        0.0
    } else {
        locked_hops as f64 / total_hops as f64
    };
    println!(
        "time locked:         {:.1}% ({locked_hops}/{total_hops} hops)",
        frac * 100.0
    );
    match modal(cand_bins) {
        Some((bpm, count)) => {
            println!(
                "modal candidate:     {bpm} bpm ({count}/{} passes)",
                cand_bins.len()
            )
        }
        None => println!("modal candidate:     n/a (no candidates)"),
    }
}

/// Print `min / median / max` of `values`, or `n/a` when empty.
fn print_dist(label: &str, values: &[f32]) {
    if values.is_empty() {
        println!("{label:<10} min/med/max: n/a");
        return;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let med = sorted[sorted.len() / 2];
    println!("{label:<10} min/med/max: {min:>7.3} / {med:>7.3} / {max:>7.3}");
}

/// The most frequent value in `bins` and its count, or `None` when empty.
fn modal(bins: &[i32]) -> Option<(i32, usize)> {
    let mut best: Option<(i32, usize)> = None;
    for &v in bins {
        let count = bins.iter().filter(|&&x| x == v).count();
        let better = match best {
            Some((_, bc)) => count > bc,
            None => true,
        };
        if better {
            best = Some((v, count));
        }
    }
    best
}
