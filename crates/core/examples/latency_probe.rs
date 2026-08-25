//! Headless end-to-end latency probe (P7) — the "photodiode-free" measuring
//! stick for the audio→feature path.
//!
//! ```text
//! latency_probe [--synthetic] [--raw-ring] [--device NAME] [--output-device NAME]
//!               [--clicks N] [--spacing-ms N] [--amp F] [--click-ms F]
//!               [--threshold F] [--perf-mode] [--list]
//! ```
//!
//! It emits a train of known clicks into the system output, captures them back
//! through the normal engine (OS loopback), and reports how long each click
//! took to travel from emission to the feature snapshot that carries it. Both
//! ends are stamped on the same monotonic clock — the ring epoch that
//! [`scia_core::FeatureSnapshot::timestamp_ns`] uses — so no external instrument
//! is needed.
//!
//! - **Live** (default): starts the engine on the loopback [`CpalBackend`], then
//!   opens a cpal OUTPUT stream that plays `--clicks` rectangular bursts, one
//!   every `--spacing-ms`, after a 1 s pre-roll. Each click's first output frame
//!   is stamped into an [`EmitLog`]; the observer detects the captured clicks in
//!   the feature stream.
//! - **Synthetic** (`--synthetic`): drives the engine with a [`SyntheticBackend`]
//!   click train and its emit log — the same measurement with no audio hardware,
//!   which is what the CI regression test exercises.
//! - **Raw-ring** (`--raw-ring`, optionally with `--synthetic`): skips the DSP
//!   hop grid entirely. It opens the capture backend directly into a
//!   probe-local sample ring, drains it off-thread, and cross-correlates each
//!   emitted click against a rectangular template to place its leading edge in
//!   the captured stream with sub-millisecond (no-hop-quantization) resolution —
//!   the P7 follow-up that isolates capture transport from hop-gather latency.
//!
//! Three intervals are reported as nearest-rank percentiles (ms): emit→publish
//! (capture + one hop), publish→observe (poll latency), and the end-to-end
//! emit→observe. In live mode a fourth row reports the output callback→playback
//! delay the host predicts.
//!
//! Exit codes: `0` success (≥80 % of clicks matched); `2` usage / unusable
//! output format; `3` no capture or output device; `4` too few clicks matched.
//!
//! Run with: `just _cargo run -p scia-core --example latency_probe -- --synthetic --clicks 10`

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use scia_core::capture::{
    DrainTimeline, RAW_CORR_ACCEPT, RING_FRAMES, drain_into_timeline, rect_xcorr_peak,
};
use scia_core::{
    CaptureBackend, CaptureError, CaptureTarget, ClickDetector, CpalBackend, Detection, DeviceKind,
    DeviceSelector, Emission, EmitLog, Engine, EngineConfig, EngineError, FeatureReader,
    LatencyStats, Matcher, Pacing, Percentiles, PerfModeState, Signal, StreamHealth,
    SyntheticBackend, list_devices, sample_ring,
};

/// Frames per hop the pipeline runs on — fixed at 256 across the codebase.
const HOP_FRAMES: u32 = 256;

/// Parsed probe parameters.
struct Params {
    clicks: u32,
    spacing_ms: u32,
    amp: f32,
    click_ms: f32,
    threshold: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            clicks: 25,
            spacing_ms: 400,
            amp: 0.8,
            click_ms: 1.0,
            threshold: 0.3,
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut params = Params::default();
    let mut device: Option<String> = None;
    let mut output_device: Option<String> = None;
    let mut synthetic = false;
    let mut perf_mode = false;
    let mut do_list = false;
    let mut raw_ring = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--synthetic" => synthetic = true,
            "--perf-mode" => perf_mode = true,
            "--list" => do_list = true,
            "--raw-ring" => raw_ring = true,
            "--device" => {
                i += 1;
                match args.get(i) {
                    Some(name) => device = Some(name.clone()),
                    None => return usage_err("--device needs a NAME"),
                }
            }
            "--output-device" => {
                i += 1;
                match args.get(i) {
                    Some(name) => output_device = Some(name.clone()),
                    None => return usage_err("--output-device needs a NAME"),
                }
            }
            "--clicks" => match parse_next::<u32>(&args, &mut i) {
                Some(v) => params.clicks = v,
                None => return usage_err("--clicks needs a positive integer"),
            },
            "--spacing-ms" => match parse_next::<u32>(&args, &mut i) {
                Some(v) if v > 0 => params.spacing_ms = v,
                _ => return usage_err("--spacing-ms needs a positive integer"),
            },
            "--amp" => match parse_next::<f32>(&args, &mut i) {
                Some(v) => params.amp = v,
                None => return usage_err("--amp needs a number"),
            },
            "--click-ms" => match parse_next::<f32>(&args, &mut i) {
                Some(v) => params.click_ms = v,
                None => return usage_err("--click-ms needs a number"),
            },
            "--threshold" => match parse_next::<f32>(&args, &mut i) {
                Some(v) => params.threshold = v,
                None => return usage_err("--threshold needs a number"),
            },
            "-h" | "--help" => {
                println!(
                    "usage: latency_probe [--synthetic] [--raw-ring] [--device NAME]\n\
                     \x20                    [--output-device NAME] [--clicks N=25] [--spacing-ms N=400]\n\
                     \x20                    [--amp F=0.8] [--click-ms F=1.0] [--threshold F=0.3]\n\
                     \x20                    [--perf-mode] [--list]"
                );
                return ExitCode::SUCCESS;
            }
            other => return usage_err(&format!("unknown argument: {other}")),
        }
        i += 1;
    }

    if params.clicks == 0 {
        return usage_err("--clicks needs a positive integer");
    }
    if do_list {
        return list();
    }
    if raw_ring {
        run_raw_ring(&params, device, output_device, synthetic)
    } else if synthetic {
        run_synthetic(&params)
    } else {
        run_live(&params, device, output_device, perf_mode)
    }
}

/// Parse the argument following `args[*i]` as `T`, advancing `*i` past it.
fn parse_next<T: std::str::FromStr>(args: &[String], i: &mut usize) -> Option<T> {
    *i += 1;
    args.get(*i).and_then(|s| s.parse::<T>().ok())
}

/// Print a usage message to stderr and return exit code 2.
fn usage_err(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(2)
}

/// Print the device table (`--list`), mirroring `capture_probe`.
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

// ---------------------------------------------------------------------------
// Synthetic mode
// ---------------------------------------------------------------------------

/// Drive the engine with a synthetic click train and its emit log; no hardware.
fn run_synthetic(p: &Params) -> ExitCode {
    let emit_log = Arc::new(EmitLog::new());
    let backend = SyntheticBackend {
        signal: Signal::Clicks {
            bpm: 60_000.0 / p.spacing_ms as f32,
            amp: p.amp,
        },
        pacing: Pacing::Realtime,
        emit_log: Some(Arc::clone(&emit_log)),
        ..SyntheticBackend::default()
    };

    let (engine, mut reader) = match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("could not start synthetic engine: {e}");
            return ExitCode::from(3);
        }
    };

    let code = observe_and_report(
        &engine,
        &mut reader,
        &emit_log,
        p,
        "synthetic",
        "synthetic",
        "off",
    );
    engine.stop();
    code
}

// ---------------------------------------------------------------------------
// Live mode
// ---------------------------------------------------------------------------

/// Start the loopback engine, open the click player on the chosen output
/// device, run the probe, and tear everything down.
fn run_live(
    p: &Params,
    device: Option<String>,
    output_device: Option<String>,
    perf_mode: bool,
) -> ExitCode {
    let selector = device
        .clone()
        .map_or(DeviceSelector::Default, DeviceSelector::Named);
    let backend = CpalBackend {
        device: selector,
        prefer_pipewire: true,
    };
    // The engine capability-detects and opens the perf-mode companion stream
    // itself when `perf_mode` is set, and holds it for the run.
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
    if let StreamHealth::Errored(msg) = engine.health() {
        eprintln!("warning: capture stream reported an error at open: {msg}");
    }

    // The engine has already capability-detected perf mode and (when available)
    // opened the companion stream. Report its state for the summary line.
    let perf_status = match engine.perf_mode_state() {
        PerfModeState::Active {
            period_frames,
            sample_rate,
        } => {
            let ms = f64::from(period_frames) * 1000.0 / f64::from(sample_rate.max(1));
            println!("perf mode: on — {period_frames}-frame engine period ({ms:.3} ms)");
            "on"
        }
        PerfModeState::Unavailable { reason } => {
            eprintln!("perf mode: unavailable — {reason}");
            "unavailable"
        }
        PerfModeState::Off => "off",
    };

    // Open the click player on the chosen output device.
    let emit_log = Arc::new(EmitLog::new());
    let output_err = Arc::new(AtomicBool::new(false));
    let (stream, output_label) = match open_click_player(
        p,
        output_device.as_deref(),
        engine.epoch(),
        &emit_log,
        &output_err,
    ) {
        Ok(pair) => pair,
        Err(PlayerError::NoDevice(msg)) => {
            eprintln!("{msg}");
            return ExitCode::from(3);
        }
        Err(PlayerError::Format(msg)) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
        Err(PlayerError::Backend(msg)) => {
            eprintln!("could not open the output click player: {msg}");
            return ExitCode::from(3);
        }
    };
    if let Err(e) = stream.play() {
        eprintln!("could not start the output click player: {e}");
        return ExitCode::from(3);
    }

    let code = observe_and_report(
        &engine,
        &mut reader,
        &emit_log,
        p,
        "live",
        &output_label,
        perf_status,
    );

    if output_err.load(Ordering::Acquire) {
        eprintln!("note: the output click player reported a stream error during the run");
    }
    drop(stream);
    engine.stop();
    code
}

/// Errors opening the click player.
enum PlayerError {
    NoDevice(String),
    Format(String),
    Backend(String),
}

/// Resolve the output device and build the silent-with-scheduled-clicks output
/// stream. Returns the live stream and a display label for the report.
fn open_click_player(
    p: &Params,
    output_device: Option<&str>,
    epoch: Instant,
    emit_log: &Arc<EmitLog>,
    output_err: &Arc<AtomicBool>,
) -> Result<(cpal::Stream, String), PlayerError> {
    let host = cpal::default_host();
    let device = match output_device {
        None => host
            .default_output_device()
            .ok_or_else(|| PlayerError::NoDevice("no default output device available".into()))?,
        Some(name) => host
            .output_devices()
            .map_err(|e| PlayerError::Backend(e.to_string()))?
            .find(|d| d.to_string() == name)
            .ok_or_else(|| PlayerError::NoDevice(format!("no output device named {name}")))?,
    };
    let label = device.to_string();

    let supported = device
        .default_output_config()
        .map_err(|e| PlayerError::Backend(e.to_string()))?;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(PlayerError::Format(format!(
            "output device {label} offers {:?}, not f32; pick an f32-capable device with \
             --output-device (see --list)",
            supported.sample_format()
        )));
    }

    let config: cpal::StreamConfig = supported.config();
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;

    // Precompute the click schedule in frames.
    let preroll_frames = u64::from(sample_rate); // 1 s
    let spacing_frames = u64::from(p.spacing_ms) * u64::from(sample_rate) / 1000;
    let click_frames = ((p.click_ms * sample_rate as f32 / 1000.0).ceil() as u64).max(1);
    let total_clicks = u64::from(p.clicks);
    let amp = p.amp;

    let log = Arc::clone(emit_log);
    let mut frames_written: u64 = 0;

    // The data callback: silence except scheduled click bursts. It must not
    // allocate, lock, or print — it does arithmetic, writes the buffer, and (on
    // a click's first frame) records one wait-free emission.
    let data_cb = move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
        let cb_ns = epoch.elapsed().as_nanos() as u64;
        let ts = info.timestamp();
        let delay_ns = ts
            .playback
            .checked_duration_since(ts.callback)
            .map_or(0, |d| d.as_nanos() as u64);
        let frames = data.len() / channels.max(1);
        for frame in 0..frames {
            let g = frames_written + frame as u64;
            let mut sample = 0.0f32;
            if g >= preroll_frames && spacing_frames > 0 {
                let rel = g - preroll_frames;
                let click_idx = rel / spacing_frames;
                let phase = rel % spacing_frames;
                if click_idx < total_clicks && phase < click_frames {
                    sample = amp;
                    if phase == 0 {
                        // The burst starts `frame` frames into this buffer: fold
                        // that intra-buffer offset into the output delay so the
                        // delay column is callback → this sample's playback.
                        let offset_ns = frame as u64 * 1_000_000_000 / u64::from(sample_rate);
                        log.push(Emission {
                            index: click_idx as u32,
                            emit_ns: cb_ns,
                            output_delay_ns: delay_ns + offset_ns,
                        });
                    }
                }
            }
            let base = frame * channels;
            for ch in 0..channels {
                data[base + ch] = sample;
            }
        }
        frames_written += frames as u64;
    };

    let err_flag = Arc::clone(output_err);
    let err_cb = move |_e: cpal::Error| {
        err_flag.store(true, Ordering::Release);
    };

    let stream = device
        .build_output_stream(config, data_cb, err_cb, None)
        .map_err(|e| match e.kind() {
            cpal::ErrorKind::DeviceNotAvailable => {
                PlayerError::NoDevice(format!("output device {label} became unavailable"))
            }
            cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
                PlayerError::Format(format!(
                    "output device {label} rejected its default f32 config"
                ))
            }
            _ => PlayerError::Backend(e.to_string()),
        })?;
    Ok((stream, label))
}

// ---------------------------------------------------------------------------
// Raw-ring mode (P7 follow-up)
// ---------------------------------------------------------------------------

/// Raw-ring cross-correlation mode: instead of running the full engine and
/// detecting clicks off the feature stream (which quantizes to the 256-frame
/// hop grid), open the capture backend directly into a probe-local sample ring,
/// drain it off-thread, and locate each emitted click's leading edge in the
/// captured stream by sub-millisecond matched-filter cross-correlation. This
/// isolates capture transport (playback → the click's samples entering scia's
/// ring) from the hop-gather latency, which the doc states rather than measures.
///
/// `synthetic` swaps the output click player + loopback capture for the
/// [`SyntheticBackend`] click generator feeding the same ring — the CI-testable
/// path, and the one `crates/core/tests/latency.rs` exercises through the
/// library types.
fn run_raw_ring(
    p: &Params,
    capture_device: Option<String>,
    output_device: Option<String>,
    synthetic: bool,
) -> ExitCode {
    // One epoch shared by the ring clock (drained-sample times) and the click
    // player's emit timestamps, so both ends are measured on one clock.
    let epoch = Instant::now();
    let (sink, mut consumer) = sample_ring(epoch);
    let emit_log = Arc::new(EmitLog::new());

    // Open the same capture backend the engine would, directly into our ring —
    // no DSP thread — so the probe sees the exact interleaved samples.
    let mut backend: Box<dyn CaptureBackend> = if synthetic {
        Box::new(SyntheticBackend {
            signal: Signal::Clicks {
                bpm: 60_000.0 / p.spacing_ms as f32,
                amp: p.amp,
            },
            pacing: Pacing::Realtime,
            emit_log: Some(Arc::clone(&emit_log)),
            ..SyntheticBackend::default()
        })
    } else {
        let selector = capture_device
            .clone()
            .map_or(DeviceSelector::Default, DeviceSelector::Named);
        Box::new(CpalBackend {
            device: selector,
            prefer_pipewire: true,
        })
    };

    let stream = match backend.open(CaptureTarget::SystemMix, sink) {
        Ok(s) => s,
        Err(CaptureError::NoDevice) => {
            eprintln!("no capture device available for raw-ring mode");
            return ExitCode::from(3);
        }
        Err(e) => {
            eprintln!("could not open capture backend: {e}");
            return ExitCode::from(3);
        }
    };
    let format = stream.format();
    // Match the sink's frame accounting to the real width (a no-op for the
    // stereo synthetic source; correct for a mono loopback).
    consumer.stats().set_channels(format.channels);
    let channels = format.channels.max(1) as usize;
    let sample_rate = format.sample_rate.max(1);

    // Live mode plays the click train on the output device, sharing `epoch`.
    let output_err = Arc::new(AtomicBool::new(false));
    let (player, output_label) = if synthetic {
        (None, "synthetic".to_string())
    } else {
        match open_click_player(p, output_device.as_deref(), epoch, &emit_log, &output_err) {
            Ok((stream, label)) => {
                if let Err(e) = stream.play() {
                    eprintln!("could not start the output click player: {e}");
                    return ExitCode::from(3);
                }
                (Some(stream), label)
            }
            Err(PlayerError::NoDevice(msg)) => {
                eprintln!("{msg}");
                return ExitCode::from(3);
            }
            Err(PlayerError::Format(msg)) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
            Err(PlayerError::Backend(msg)) => {
                eprintln!("could not open the output click player: {msg}");
                return ExitCode::from(3);
            }
        }
    };

    // Pre-allocate every analysis buffer from --clicks / --spacing-ms. Live mode
    // waits a 1 s pre-roll (the click player does) before the first click.
    let preroll_ms: u64 = if synthetic { 0 } else { 1000 };
    let observe_ms = preroll_ms + u64::from(p.clicks) * u64::from(p.spacing_ms) + 1000;
    let cap_frames = (((observe_ms + 500) * u64::from(sample_rate)) / 1000) as usize;
    let mut mono: Vec<f32> = Vec::with_capacity(cap_frames);
    let mut scratch: Vec<f32> = Vec::with_capacity(RING_FRAMES * 2);
    let mut timeline = DrainTimeline::new(sample_rate);
    timeline.reserve(observe_ms as usize + 16); // ~one drain per 1 ms poll

    // Drain the ring every 1 ms across the observation window, accumulating mono
    // samples and their capture-delivery times; one final drain catches the last
    // tail. Each drain is anchored to `last_push_ns` (see `drain_into_timeline`),
    // so the reconstructed times measure ring entry, not the probe's poll read.
    let deadline = Instant::now() + Duration::from_millis(observe_ms);
    while Instant::now() < deadline {
        drain_into_timeline(
            &mut consumer,
            &mut scratch,
            &mut mono,
            &mut timeline,
            channels,
        );
        sleep(Duration::from_millis(1));
    }
    drain_into_timeline(
        &mut consumer,
        &mut scratch,
        &mut mono,
        &mut timeline,
        channels,
    );

    drop(player);
    drop(stream);
    if output_err.load(Ordering::Acquire) {
        eprintln!("note: the output click player reported a stream error during the run");
    }

    // Correlate each emitted click against the raw stream over its own search
    // window: from the emission to half a click spacing later.
    let mut emissions: Vec<Emission> = Vec::with_capacity(p.clicks as usize);
    emit_log.drain(&mut emissions);
    emissions.sort_by_key(|e| e.emit_ns);

    let click_frames = ((p.click_ms * sample_rate as f32 / 1000.0).ceil() as usize).max(1);
    let half_spacing_ns = u64::from(p.spacing_ms) / 2 * 1_000_000;

    let mut arrivals_ms: Vec<f32> = Vec::with_capacity(emissions.len());
    let mut per_click: Vec<(u32, Option<f32>, f32)> = Vec::with_capacity(emissions.len());
    for e in &emissions {
        // Search a full spacing wide, centered on the emission. The click's true
        // arrival is after emit_ns for real capture transport, but on the
        // near-zero-transport synthetic path the emit stamp (taken before the
        // chunk is pushed) can precede the sample's continuous-capture modeled
        // time by a few ms, so the window reaches back half a spacing. A
        // neighbour click is a full spacing away and cannot be picked up.
        let lo = timeline.frame_at_or_after(e.emit_ns.saturating_sub(half_spacing_ns)) as usize;
        let hi = timeline.frame_at_or_after(e.emit_ns + half_spacing_ns) as usize;
        match rect_xcorr_peak(&mono, click_frames, lo, hi) {
            Some((offset, peak)) if peak >= RAW_CORR_ACCEPT => {
                let arrival = timeline.sample_time_ns(offset as u64).unwrap_or(e.emit_ns);
                let ms = (arrival as i64 - e.emit_ns as i64) as f32 / 1.0e6;
                arrivals_ms.push(ms);
                per_click.push((e.index, Some(ms), peak));
            }
            other => {
                let peak = other.map_or(0.0, |(_, pk)| pk);
                per_click.push((e.index, None, peak));
            }
        }
    }

    // ---- Report ----
    let emitted = emissions.len() as u32;
    let matched = arrivals_ms.len() as u32;
    let hop_ms = f64::from(HOP_FRAMES) * 1000.0 / f64::from(sample_rate);
    println!(
        "latency probe: raw-ring {} · capture {} Hz {} ch · output {output_label} · \
         hop {HOP_FRAMES} ({hop_ms:.2} ms)",
        if synthetic { "synthetic" } else { "live" },
        sample_rate,
        format.channels,
    );
    println!(
        "clicks {emitted} · matched {matched} · missed {}",
        emitted.saturating_sub(matched),
    );
    let pct = Percentiles::nearest_rank(arrivals_ms.clone());
    println!(
        "{:<22} {:>7} {:>7} {:>7} {:>7}   (ms)",
        "", "min", "median", "p95", "max"
    );
    println!(
        "{:<22} {:>7.2} {:>7.2} {:>7.2} {:>7.2}",
        "emit → raw-arrival", pct.min, pct.median, pct.p95, pct.max,
    );
    for (idx, ms, peak) in &per_click {
        match ms {
            Some(v) => {
                println!("  click {idx:>3}: emit → raw-arrival {v:>7.2} ms  (ncc {peak:.3})")
            }
            None => println!("  click {idx:>3}: no raw arrival found       (ncc {peak:.3})"),
        }
    }
    println!(
        "note: emit → raw-arrival is capture transport only — from the click's emission to its \
         samples entering scia's ring — found by cross-correlation with no hop quantization. \
         emit → publish is not measured in this mode; the hop grid adds up to one {HOP_FRAMES}-frame \
         hop ({hop_ms:.2} ms) of gather on top (stated, not measured). Compare raw-arrival against \
         a normal run's emit → publish to see how much of that interval is capture transport."
    );

    if emitted == 0 {
        eprintln!("no clicks were emitted — nothing to measure");
        return ExitCode::from(4);
    }
    if u64::from(matched) * 100 >= u64::from(emitted) * 80 {
        ExitCode::SUCCESS
    } else {
        eprintln!("only {matched}/{emitted} clicks correlated (< 80%)");
        ExitCode::from(4)
    }
}

// ---------------------------------------------------------------------------
// Shared observer + report
// ---------------------------------------------------------------------------

/// Run the observer over the feature stream, match emissions to detections,
/// and print the report. Returns the process exit code.
fn observe_and_report(
    engine: &Engine,
    reader: &mut FeatureReader,
    emit_log: &Arc<EmitLog>,
    p: &Params,
    mode: &str,
    output_label: &str,
    perf_status: &str,
) -> ExitCode {
    let half_spacing_ns = u64::from(p.spacing_ms) / 2 * 1_000_000;
    let mut detector = ClickDetector::new(p.threshold, half_spacing_ns);
    let mut detections: Vec<Detection> = Vec::new();

    // Observe for clicks * spacing + 2 s, then a short flush tail so the last
    // click's hop is observed before the log is drained (see the P7 doc's
    // method — with a boundary-aligned window the tail lands mid-gap).
    let observe = Duration::from_millis(u64::from(p.clicks) * u64::from(p.spacing_ms) + 2_000);
    let flush = Duration::from_millis((u64::from(p.spacing_ms) / 2).max(120));
    poll_observe(engine, reader, &mut detector, &mut detections, observe);
    poll_observe(engine, reader, &mut detector, &mut detections, flush);

    let engine_stats = engine.stats();

    let mut emissions: Vec<Emission> = Vec::new();
    emit_log.drain(&mut emissions);

    let matcher = Matcher::new(half_spacing_ns);
    let matched = matcher.match_events(&emissions, &detections);
    let stats = LatencyStats::from_matched(&matched);

    // ---- Report ----
    let format = engine.format();
    let hop_ms = f64::from(HOP_FRAMES) * 1000.0 / f64::from(format.sample_rate.max(1));
    println!(
        "latency probe: {mode} · capture {} Hz {} ch · output {output_label} · \
         perf-mode {perf_status} · hop {HOP_FRAMES} ({hop_ms:.2} ms)",
        format.sample_rate, format.channels
    );
    println!(
        "clicks {} · matched {} · missed {} · spurious {}",
        emissions.len(),
        stats.count,
        stats.missed,
        stats.spurious
    );
    print!("{stats}");
    println!(
        "engine: pushes {} · dropped {} · xruns {} · hops {}/{} (processed/synthesized)",
        engine_stats.pushes,
        engine_stats.dropped_frames,
        engine_stats.xruns,
        engine_stats.hops_processed,
        engine_stats.hops_synthesized,
    );

    // Exit 0 when at least 80 % of emitted clicks were matched.
    let emitted = emissions.len() as u32;
    if emitted == 0 {
        eprintln!("no clicks were emitted — nothing to measure");
        return ExitCode::from(4);
    }
    if u64::from(stats.count) * 100 >= u64::from(emitted) * 80 {
        ExitCode::SUCCESS
    } else {
        eprintln!("only {}/{} clicks matched (< 80%)", stats.count, emitted);
        ExitCode::from(4)
    }
}

/// Poll `reader` every 1 ms for `duration`, feeding each freshly-published
/// snapshot to the detector and collecting detections.
fn poll_observe(
    engine: &Engine,
    reader: &mut FeatureReader,
    detector: &mut ClickDetector,
    detections: &mut Vec<Detection>,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    let mut last_gen: Option<u64> = None;
    while Instant::now() < deadline {
        let snapshot = *reader.latest();
        if last_gen != Some(snapshot.generation) {
            last_gen = Some(snapshot.generation);
            if let Some(d) = detector.observe(&snapshot, engine.now_ns()) {
                detections.push(d);
            }
        }
        sleep(Duration::from_millis(1));
    }
}
