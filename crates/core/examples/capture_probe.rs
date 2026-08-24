//! Headless capture probe — the binary shipped to real machines to verify the
//! cpal backend end to end.
//!
//! ```text
//! capture_probe [--device NAME] [--seconds N] [--list] [--perf-mode]
//! ```
//!
//! With `--list` it prints every device on every cpal host and exits. Otherwise
//! it starts the engine on [`CpalBackend`], prints the negotiated
//! [`StreamFormat`], then once per second prints a one-line status of the
//! capture-callback cadence and the live features. At the end it prints a
//! summary — total pushes, mean/max push size, worst callback gap and the
//! fraction of hops the DSP grid had to synthesize as silence. That last figure
//! is the P6 silence-starvation measurement: on a healthy stream it is ~0.
//!
//! With `--perf-mode` (Windows only) it opens a companion silent render stream
//! on the default render endpoint at that endpoint's minimum engine period,
//! prints the endpoint periods, and keeps the stream alive for the run. This is
//! the P1 measurement: because a loopback capture inherits the endpoint period,
//! the per-second cadence line should then show pushes of ~128 frames every
//! ~2.7 ms instead of ~480 frames every ~10 ms. Off Windows the switch reports
//! that perf mode is unavailable and the probe runs unchanged.
//!
//! Exit codes: `0` on success; `3` (with a message) when no capture device is
//! available or the backend cannot open one.
//!
//! Run with: `just _cargo run -p scia-core --example capture_probe -- --seconds 3`

use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use scia_core::{
    CaptureError, CpalBackend, DeviceKind, DeviceSelector, Engine, EngineConfig, EngineError,
    EngineStats, PerfModeState, StreamHealth, list_devices,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut device: Option<String> = None;
    let mut seconds: u64 = 10;
    let mut do_list = false;
    let mut perf_mode = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => do_list = true,
            "--perf-mode" => perf_mode = true,
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
                    Some(n) => seconds = n,
                    None => {
                        eprintln!("--seconds needs a non-negative integer");
                        return ExitCode::from(2);
                    }
                }
            }
            "-h" | "--help" => {
                println!(
                    "usage: capture_probe [--device NAME] [--seconds N=10] [--list] [--perf-mode]"
                );
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
    probe(device, seconds, perf_mode)
}

/// Print the device table (`--list`).
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

/// Run the capture probe for `seconds` seconds. When `perf_mode` is set, a
/// companion perf-mode render stream is opened before the engine starts and
/// held for the whole run (Windows only; a no-op with a note elsewhere).
fn probe(device: Option<String>, seconds: u64, perf_mode: bool) -> ExitCode {
    let selector = device
        .clone()
        .map_or(DeviceSelector::Default, DeviceSelector::Named);
    let backend = CpalBackend {
        device: selector,
        prefer_pipewire: true,
    };

    // The engine capability-detects and opens the perf-mode companion stream
    // itself when `perf_mode` is set; it holds the stream for the engine's life
    // and re-evaluates it on a device switch.
    let config = EngineConfig {
        perf_mode,
        ..EngineConfig::default()
    };
    let (engine, mut reader) = match Engine::start(Box::new(backend), config) {
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

    let format = engine.format();
    println!(
        "negotiated stream: {} Hz, {} channel(s)",
        format.sample_rate, format.channels
    );
    if perf_mode {
        report_perf_mode(&engine);
    }
    if let StreamHealth::Errored(msg) = engine.health() {
        eprintln!("warning: stream reported an error at open: {msg}");
    }
    println!(
        "probing for {seconds}s (t / hops / synth / pushes / avg frames per push / \
         max gap / dropped / rms / peak / loudest bar)"
    );

    for t in 1..=seconds {
        sleep(Duration::from_secs(1));
        let stats = engine.stats();
        let snap = *reader.latest();
        let (peak_bar, peak_val) = loudest_bar(&snap.spectrum[..snap.spectrum_len as usize]);
        println!(
            "t={t:>3}s  hops={:>6}  synth={:>5}  pushes={:>6}  avg_frames/push={:>7.1}  \
             max_gap={:>6.1}ms  dropped={:>6}  rms={:>6.4}  peak={:>6.4}  \
             spectrum_peak_bar={peak_bar}({peak_val:.3})",
            stats.hops_processed,
            stats.hops_synthesized,
            stats.pushes,
            mean_push(&stats),
            stats.max_gap_ms,
            stats.dropped_frames,
            snap.rms,
            snap.peak,
        );
        if let StreamHealth::Errored(msg) = engine.health() {
            eprintln!("stream error during capture: {msg}");
            break;
        }
    }

    let stats = engine.stats();
    engine.stop();

    let total_hops = stats.hops_processed + stats.hops_synthesized;
    let synth_frac = if total_hops == 0 {
        0.0
    } else {
        stats.hops_synthesized as f64 / total_hops as f64
    };
    println!("\n== summary ==");
    println!("total pushes:        {}", stats.pushes);
    println!("mean push frames:    {:.1}", mean_push(&stats));
    println!("max push frames:     {}", stats.max_push_frames);
    println!("last push frames:    {}", stats.last_push_frames);
    println!("total frames pushed: {}", stats.pushed_frames);
    println!("dropped frames:      {}", stats.dropped_frames);
    println!("buffer xruns:        {}", stats.xruns);
    println!("max callback gap:    {:.1} ms", stats.max_gap_ms);
    println!(
        "hops processed/synth: {}/{}  (synthesized fraction {:.4})",
        stats.hops_processed, stats.hops_synthesized, synth_frac
    );

    if stats.pushes == 0 {
        eprintln!("\nno callbacks fired in {seconds}s — the device opened but delivered no audio");
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

/// Print the engine's perf-mode state: `perf-mode on` with the chosen engine
/// period when a companion stream is holding the endpoint fast,
/// `perf-mode unavailable` with the reason otherwise (a driver-locked endpoint,
/// no endpoint, or a non-Windows build).
fn report_perf_mode(engine: &Engine) {
    match engine.perf_mode_state() {
        PerfModeState::Active {
            period_frames,
            sample_rate,
        } => {
            let ms = f64::from(period_frames) * 1000.0 / f64::from(sample_rate.max(1));
            println!("perf-mode on: {period_frames}-frame engine period ({ms:.3} ms)");
        }
        PerfModeState::Unavailable { reason } => {
            println!("perf-mode unavailable: {reason}");
        }
        PerfModeState::Off => println!("perf-mode off"),
    }
}

/// Mean frames per push so far (`pushed_frames / pushes`).
fn mean_push(stats: &EngineStats) -> f64 {
    if stats.pushes == 0 {
        0.0
    } else {
        stats.pushed_frames as f64 / stats.pushes as f64
    }
}

/// Index and value of the loudest display-spectrum bar (0-length → `(0, 0.0)`).
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
