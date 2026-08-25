//! Headless end-to-end latency probe (P7) — the "photodiode-free" measuring
//! stick for the audio→feature path.
//!
//! ```text
//! latency_probe [--synthetic] [--raw-ring] [--dual-tap] [--forensic] [--device NAME]
//!               [--output-device NAME] [--clicks N] [--spacing-ms N] [--amp F]
//!               [--click-ms F] [--threshold F] [--perf-mode] [--list]
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
//! - **Dual-tap** (`--dual-tap`, optionally with `--synthetic`): runs the FULL
//!   engine (hops publish as normal) with a tee on the capture sink, and measures
//!   BOTH `emit → raw-arrival` (cross-correlation on the teed raw samples, exact
//!   per-push mapping) and `emit → publish` (hop detection) from the same clicks
//!   in one process on one clock. It prints their per-click delta and the subset
//!   invariant verdict `raw-arrival ≤ publish ≤ raw-arrival + one hop` — the
//!   definitive discriminator for the cross-mode constant the two separate modes
//!   could never settle.
//! - **Forensic** (`--forensic`, implies `--dual-tap`): the round-6 diagnostic. It
//!   runs the full dual-tap instrument and, for each matched click, dumps a raw
//!   forensic trace to stderr (`F.coord` / `F.energy` / `F.push` / `F.hops` /
//!   `F.output`) that puts both detectors on one absolute frame coordinate, profiles
//!   the energy around the click, and surfaces the per-push driver-vs-wall
//!   timestamps — so a ~30 ms discrepancy is visible as content location or stream
//!   structure rather than modelled. It measures nothing new; it makes the raw data
//!   visible. See `docs/probes/p7-end-to-end-latency.md`.
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

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use scia_core::capture::{
    DrainTimeline, ForensicPush, RAW_CORR_ACCEPT, RING_FRAMES, bucket_peaks, drain_into_timeline,
    peak_abs_in_range, rect_xcorr_peak, tee_drain_forensic, tee_drain_into_timeline,
};
use scia_core::{
    CaptureBackend, CaptureError, CaptureTarget, ClickDetector, CpalBackend, Detection, DeviceKind,
    DeviceSelector, DualTapSample, DualTapStats, Emission, EmitLog, Engine, EngineConfig,
    EngineError, FeatureReader, LatencyStats, Matcher, Pacing, Percentiles, PerfModeState, Sample,
    Signal, StreamHealth, SyntheticBackend, list_devices, sample_ring,
};

/// Tolerance the dual-tap invariant is scored within (ms) — a few frame-periods,
/// covering sub-sample correlation placement and ms rounding on the two clocks.
const DUAL_TAP_EPS_MS: f32 = 0.5;

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
    let mut dual_tap = false;
    let mut forensic = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--synthetic" => synthetic = true,
            "--perf-mode" => perf_mode = true,
            "--list" => do_list = true,
            "--raw-ring" => raw_ring = true,
            "--dual-tap" => dual_tap = true,
            "--forensic" => forensic = true,
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
                    "usage: latency_probe [--synthetic] [--raw-ring] [--dual-tap] [--forensic]\n\
                     \x20                    [--device NAME] [--output-device NAME] [--clicks N=25]\n\
                     \x20                    [--spacing-ms N=400] [--amp F=0.8] [--click-ms F=1.0]\n\
                     \x20                    [--threshold F=0.3] [--perf-mode] [--list]\n\
                     \x20  --forensic implies --dual-tap and dumps a per-click forensic trace \
                     (see docs/probes/p7-end-to-end-latency.md)."
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
    // --forensic is an extension of dual-tap: it runs the full dual-tap instrument
    // and additionally dumps a per-click forensic trace to stderr.
    if forensic {
        dual_tap = true;
    }
    if dual_tap && raw_ring {
        return usage_err("--dual-tap/--forensic and --raw-ring are mutually exclusive");
    }
    if dual_tap {
        run_dual_tap(&params, device, output_device, synthetic, forensic)
    } else if raw_ring {
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
    // Track the steady-state ring backlog (frames the writer had delivered that a
    // drain did not pop). A drain that keeps up leaves ~0; a persistent nonzero
    // backlog is the direct signature of a lagging drain, and the anchor is now
    // corrected for it (see `drain_into_timeline`). Surface both the worst and the
    // last observed backlog so a hardware run can see whether the drain keeps up.
    let mut max_backlog: u64 = 0;
    let deadline = Instant::now() + Duration::from_millis(observe_ms);
    while Instant::now() < deadline {
        max_backlog = max_backlog.max(drain_into_timeline(
            &mut consumer,
            &mut scratch,
            &mut mono,
            &mut timeline,
            channels,
        ));
        sleep(Duration::from_millis(1));
    }
    // The final drain catches the tail; its backlog is the last snapshot reported.
    let last_backlog = drain_into_timeline(
        &mut consumer,
        &mut scratch,
        &mut mono,
        &mut timeline,
        channels,
    );
    max_backlog = max_backlog.max(last_backlog);

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
    let backlog_ms = |frames: u64| frames as f64 * 1000.0 / f64::from(sample_rate);
    println!(
        "clicks {emitted} · matched {matched} · missed {} · ring backlog max {max_backlog} fr \
         ({:.2} ms) · last {last_backlog} fr ({:.2} ms)",
        emitted.saturating_sub(matched),
        backlog_ms(max_backlog),
        backlog_ms(last_backlog),
    );
    if max_backlog > 0 {
        eprintln!(
            "note: a nonzero steady-state ring backlog means the drain ran behind the writer; \
             the reconstructed times are anchored on capture delivery corrected for it \
             (max {max_backlog} frames ≈ {:.2} ms).",
            backlog_ms(max_backlog),
        );
    }
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
         samples entering scia's ring, anchored on the capture-delivery clock (last_push_ns minus \
         ring occupancy) — found by cross-correlation with no hop quantization. emit → publish is \
         not measured in this mode; the hop grid adds up to one {HOP_FRAMES}-frame hop \
         ({hop_ms:.2} ms) of gather on top (stated, not measured). A normal run's emit → publish is \
         anchored on the SAME capture-delivery clock (the hop's newest frame), so on one run \
         raw-arrival ≤ publish ≤ raw-arrival + one hop holds by construction; compare the two to \
         split the interval into capture transport vs hop gather."
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
// Dual-tap mode (P7 final instrument)
// ---------------------------------------------------------------------------

/// Dual-tap mode: run the full engine with a tee on the capture sink and measure
/// BOTH `emit → raw-arrival` (cross-correlation on the teed raw samples, exact
/// per-push mapping) and `emit → publish` (hop detection off the feature stream)
/// from the SAME clicks in ONE process on ONE clock. This is the discriminator
/// the doc's third reconciliation round is owed: the two figures could never come
/// from one process before (the DSP consumes the ring, so the modes were mutually
/// exclusive), so a rock-constant ~27 ms gap between separate runs could not be
/// told apart from an instrumentation bug. Here the invariant
/// `raw-arrival ≤ publish ≤ raw-arrival + one hop` is checked WITHIN one run: if
/// it holds, the per-open capture difference is real; if it breaks, a model still
/// lies and the defect is localizable.
///
/// `synthetic` swaps the loopback capture + output player for the
/// [`SyntheticBackend`] click generator as the engine's capture backend — the
/// CI-testable path the `crates/core/tests/latency.rs` regression mirrors.
fn run_dual_tap(
    p: &Params,
    capture_device: Option<String>,
    output_device: Option<String>,
    synthetic: bool,
    forensic: bool,
) -> ExitCode {
    let emit_log = Arc::new(EmitLog::new());

    // The engine's capture backend: synthetic click train, or the real loopback.
    let backend: Box<dyn CaptureBackend> = if synthetic {
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

    let (engine, mut reader, mut tee) =
        match Engine::start_with_dual_tap(backend, EngineConfig::default()) {
            Ok(triple) => triple,
            Err(EngineError::Capture(CaptureError::NoDevice)) => {
                eprintln!("no capture device available for dual-tap mode");
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

    let format = engine.format();
    let channels = format.channels.max(1) as usize;
    let sample_rate = format.sample_rate.max(1);

    // Live mode plays the click train on the output device, sharing the engine's
    // epoch so emissions and both measured ends are on one clock.
    let output_err = Arc::new(AtomicBool::new(false));
    let (player, output_label) = if synthetic {
        (None, "synthetic".to_string())
    } else {
        match open_click_player(
            p,
            output_device.as_deref(),
            engine.epoch(),
            &emit_log,
            &output_err,
        ) {
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

    // Pre-allocate the raw-arrival analysis buffers. Live mode's click player
    // waits a 1 s pre-roll before the first click.
    let preroll_ms: u64 = if synthetic { 0 } else { 1000 };
    let observe_ms = preroll_ms + u64::from(p.clicks) * u64::from(p.spacing_ms) + 2000;
    let flush_ms = (u64::from(p.spacing_ms) / 2).max(120);
    let cap_frames = (((observe_ms + flush_ms + 500) * u64::from(sample_rate)) / 1000) as usize;
    let mut mono: Vec<f32> = Vec::with_capacity(cap_frames);
    let mut scratch: Vec<f32> = Vec::with_capacity(RING_FRAMES * 2);
    let mut timeline = DrainTimeline::new(sample_rate);
    timeline.reserve(observe_ms as usize + 16);

    // One 1 ms poll loop does BOTH jobs on one thread: feed the observer (for the
    // publish side) and drain the tee (for the raw-arrival side), so the two
    // measurements track the same clicks in lockstep.
    let half_spacing_ns = u64::from(p.spacing_ms) / 2 * 1_000_000;
    let mut detector = ClickDetector::new(p.threshold, half_spacing_ns);
    let mut detections: Vec<Detection> = Vec::new();
    // Forensic-only collection (empty and untouched unless --forensic):
    //  - `forensic_pushes`: every teed push with its wall/driver stamps (item 3);
    //  - `hop_history`: one (generation, peak, rms, timestamp_ns) per published hop,
    //    so the 10 hops before a detection can be dumped (item 4).
    let mut forensic_pushes: Vec<ForensicPush> = Vec::new();
    let mut hop_history: Vec<(u64, f32, f32, u64)> = Vec::new();
    if forensic {
        forensic_pushes.reserve(4096);
        hop_history.reserve((total_hops_estimate(p)) as usize);
    }
    let total_ms = observe_ms + flush_ms;
    let deadline = Instant::now() + Duration::from_millis(total_ms);
    let mut last_gen: Option<u64> = None;
    while Instant::now() < deadline {
        let snapshot = *reader.latest();
        if last_gen != Some(snapshot.generation) {
            last_gen = Some(snapshot.generation);
            if forensic {
                hop_history.push((
                    snapshot.generation,
                    snapshot.peak,
                    snapshot.rms,
                    snapshot.timestamp_ns,
                ));
            }
            if let Some(d) = detector.observe(&snapshot, engine.now_ns()) {
                detections.push(d);
            }
        }
        if forensic {
            tee_drain_forensic(
                &mut tee,
                &mut scratch,
                &mut mono,
                &mut timeline,
                channels,
                &mut forensic_pushes,
            );
        } else {
            tee_drain_into_timeline(&mut tee, &mut scratch, &mut mono, &mut timeline, channels);
        }
        sleep(Duration::from_millis(1));
    }
    // One final drain catches the tail before capture is torn down.
    if forensic {
        tee_drain_forensic(
            &mut tee,
            &mut scratch,
            &mut mono,
            &mut timeline,
            channels,
            &mut forensic_pushes,
        );
    } else {
        tee_drain_into_timeline(&mut tee, &mut scratch, &mut mono, &mut timeline, channels);
    }

    let engine_stats = engine.stats();
    let dropped_pushes = tee.dropped_pushes();

    drop(player);
    if output_err.load(Ordering::Acquire) {
        eprintln!("note: the output click player reported a stream error during the run");
    }
    engine.stop();

    // ---- Correlate both ends against the same emissions ----
    let mut emissions: Vec<Emission> = Vec::new();
    emit_log.drain(&mut emissions);
    emissions.sort_by_key(|e| e.emit_ns);

    // Publish side: pair emissions to detections in order.
    let matched = Matcher::new(half_spacing_ns).match_events(&emissions, &detections);
    let publish_stats = LatencyStats::from_matched(&matched);

    // Raw-arrival side: cross-correlate each emitted click against the teed stream.
    let click_frames = ((p.click_ms * sample_rate as f32 / 1000.0).ceil() as usize).max(1);
    // Per click, the raw-arrival hit: (ms, correlation leading-edge frame, ncc).
    // The extra frame/ncc are only read by the forensic dump; the normal join reads
    // `.0`.
    let mut raw_by_index: HashMap<u32, (f32, usize, f32)> = HashMap::new();
    let mut raw_matched = 0u32;
    for e in &emissions {
        let lo = timeline.frame_at_or_after(e.emit_ns.saturating_sub(half_spacing_ns)) as usize;
        let hi = timeline.frame_at_or_after(e.emit_ns + half_spacing_ns) as usize;
        if let Some((offset, peak)) = rect_xcorr_peak(&mono, click_frames, lo, hi) {
            if peak >= RAW_CORR_ACCEPT {
                let arrival = timeline.sample_time_ns(offset as u64).unwrap_or(e.emit_ns);
                let ms = (arrival as i64 - e.emit_ns as i64) as f32 / 1.0e6;
                raw_by_index.insert(e.index, (ms, offset, peak));
                raw_matched += 1;
            }
        }
    }

    // Join: a click measured BOTH ways becomes one dual-tap sample.
    let mut samples: Vec<DualTapSample> = Vec::new();
    for s in &matched.samples {
        if let Some(&(raw_ms, _, _)) = raw_by_index.get(&s.index) {
            let publish_ms = (s.publish_ns.saturating_sub(s.emit_ns)) as f32 / 1.0e6;
            samples.push(DualTapSample {
                index: s.index,
                emit_to_raw_arrival_ms: raw_ms,
                emit_to_publish_ms: publish_ms,
            });
        }
    }
    samples.sort_by_key(|s| s.index);

    let hop_ms = (f64::from(HOP_FRAMES) * 1000.0 / f64::from(sample_rate)) as f32;
    let stats = DualTapStats::from_samples(&samples, hop_ms, DUAL_TAP_EPS_MS);

    // ---- Report ----
    let emitted = emissions.len() as u32;
    println!(
        "latency probe: dual-tap {} · capture {} Hz {} ch · output {output_label} · \
         hop {HOP_FRAMES} ({hop_ms:.2} ms)",
        if synthetic { "synthetic" } else { "live" },
        sample_rate,
        format.channels,
    );
    let backlog_ms = |frames: u64| frames as f64 * 1000.0 / f64::from(sample_rate);
    println!(
        "clicks {emitted} · publish-matched {} · raw-matched {raw_matched} · both {} · \
         tee dropped-pushes {dropped_pushes} ({:.2} ms)",
        publish_stats.count,
        stats.count,
        backlog_ms(dropped_pushes * u64::from(format.channels)),
    );
    if dropped_pushes > 0 {
        eprintln!(
            "note: the tee dropped {dropped_pushes} whole packet(s) — the raw-arrival drain did \
             not keep up; treat the raw-arrival numbers as suspect."
        );
    }
    print!("{stats}");
    println!();
    for s in &samples {
        let mark = if s.subset_holds(DUAL_TAP_EPS_MS) && s.within_one_hop(hop_ms, DUAL_TAP_EPS_MS) {
            "ok"
        } else if !s.subset_holds(DUAL_TAP_EPS_MS) {
            "SUBSET-BREAK"
        } else {
            "over-one-hop"
        };
        println!(
            "  click {:>3}: raw-arrival {:>8.2}  publish {:>8.2}  Δ {:>7.2} ms  [{mark}]",
            s.index,
            s.emit_to_raw_arrival_ms,
            s.emit_to_publish_ms,
            s.hop_gather_ms(),
        );
    }
    println!(
        "engine: pushes {} · dropped {} · xruns {} · hops {}/{} (processed/synthesized)",
        engine_stats.pushes,
        engine_stats.dropped_frames,
        engine_stats.xruns,
        engine_stats.hops_processed,
        engine_stats.hops_synthesized,
    );
    println!(
        "note: both ends are from ONE running engine on ONE capture-delivery clock. \
         emit → raw-arrival is the click's samples entering scia's ring (exact per-push mapping off \
         the tee); emit → publish is the hop that carries it (delivery-anchored). The subset \
         invariant raw-arrival ≤ publish ≤ raw-arrival + one hop ({hop_ms:.2} ms) therefore holds by \
         construction — a SUBSET-BREAK (raw-arrival above publish inside one run) is impossible for \
         two honest clocks and localizes a model defect; an over-one-hop click is a detection landing \
         a hop late, not a clock bug. Subtract output delay (cb→play, live) to reason about a real \
         player's audio already in the mix."
    );

    if forensic {
        dump_forensic(
            p,
            sample_rate,
            &mono,
            &timeline,
            &matched.samples,
            &raw_by_index,
            &detections,
            &emissions,
            &forensic_pushes,
            &hop_history,
        );
    }

    // Exit contract mirrors the other modes: success when ≥ 80 % of emitted clicks
    // were measured both ways.
    if emitted == 0 {
        eprintln!("no clicks were emitted — nothing to measure");
        return ExitCode::from(4);
    }
    if u64::from(stats.count) * 100 >= u64::from(emitted) * 80 {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "only {}/{emitted} clicks measured both ways (< 80%)",
            stats.count
        );
        ExitCode::from(4)
    }
}

// ---------------------------------------------------------------------------
// Forensic dump (P7 round-6 diagnostic, `--forensic`)
// ---------------------------------------------------------------------------

/// Rough upper bound on published hops in a run, for pre-reserving the forensic hop
/// history: the observation span in ms over the ~5.33 ms hop period, plus slack.
fn total_hops_estimate(p: &Params) -> u64 {
    let ms = 1000 + u64::from(p.clicks) * u64::from(p.spacing_ms) + 2000;
    ms / 5 + 64
}

/// Emit the per-click forensic trace to stderr (machine-readable single lines). This
/// does NOT model anything — it dumps raw positions and content so the discrepancy is
/// visible directly. Every frame index is absolute (cumulative teed frames since the
/// stream started), so the publish side and the raw side share one coordinate.
///
/// Five line kinds per matched click (see the P7 doc's forensic legend):
///   F.coord  — the SHARED COORDINATE: the detecting hop's absolute frame range and
///              its loudest sample vs the raw correlation's leading edge, with the
///              frame delta between them (≈0 = same event; ≈1440 = ~30 ms apart).
///   F.energy — a 70×1 ms peak profile over [raw_edge-50 ms, +20 ms], hex levels
///              scaled to the window max, so a second energy event before the click
///              is visible as a bump left of bucket 50.
///   F.push   — the teed pushes covering the click: wall delivery vs driver capture
///              timestamp (column dropped when the backend has no driver instant).
///   F.hops   — the detector threshold and the peak of the 10 hops before the one
///              that fired, so a premature/noise trigger shows.
///   F.output — the click's emission bookkeeping and a double-write check.
#[allow(clippy::too_many_arguments)]
fn dump_forensic(
    p: &Params,
    sample_rate: u32,
    mono: &[f32],
    timeline: &DrainTimeline,
    matched: &[Sample],
    raw_by_index: &HashMap<u32, (f32, usize, f32)>,
    detections: &[Detection],
    emissions: &[Emission],
    pushes: &[ForensicPush],
    hop_history: &[(u64, f32, f32, u64)],
) {
    let fpb = (sample_rate as usize / 1000).max(1); // frames per 1 ms bucket
    let ns_per_frame = 1.0e9 / f64::from(sample_rate.max(1));

    // Recover the detecting hop's generation from a matched sample via its publish
    // time (the hop's timestamp_ns).
    let det_by_publish: HashMap<u64, Detection> =
        detections.iter().map(|d| (d.publish_ns, *d)).collect();
    // Emissions per index — more than one flags a double-write on the output side.
    let mut emit_count: HashMap<u32, u32> = HashMap::new();
    for e in emissions {
        *emit_count.entry(e.index).or_insert(0) += 1;
    }

    // Does the backend expose a usable driver capture timestamp? 0 everywhere on the
    // synthetic path (and on any host cpal reports no capture instant for).
    let driver_available = pushes.iter().any(|pf| pf.driver_capture_ns != 0);

    eprintln!("-- FORENSIC DUMP ----------------------------------------------");
    eprintln!(
        "F.legend  coordinate=absolute-teed-frame rate={sample_rate}Hz frames_per_ms={fpb} \
         hop_frames={HOP_FRAMES} clicks={} threshold={:.3} — a ~1440-frame Δframes (~30 ms) \
         between raw_edge and the hop peak means the two sides see DIFFERENT events / a shared \
         mapping constant; a Δframes under one hop means the same event.",
        p.clicks, p.threshold,
    );
    if !driver_available {
        eprintln!(
            "F.push-driver: driver capture timestamp NOT available on this backend \
             (synthetic, or cpal reported no capture instant) — driver_capture_ns column omitted."
        );
    }

    let mut ms_sorted: Vec<&Sample> = matched.iter().collect();
    ms_sorted.sort_by_key(|s| s.index);

    for s in ms_sorted {
        let idx = s.index;

        // ---- 1. SHARED COORDINATE ----
        // Publish side: invert the hop's publish time to its newest absolute frame,
        // hop = [newest-(HOP-1), newest]; find the loudest sample in it.
        let g_newest = timeline
            .frame_at_or_after(s.publish_ns)
            .min(mono.len().saturating_sub(1) as u64);
        let first = g_newest.saturating_sub(u64::from(HOP_FRAMES - 1));
        let (peak_idx, peak_val) = peak_abs_in_range(mono, first as usize, g_newest as usize);
        let in_hop = peak_idx as i64 - first as i64;
        // Detecting hop's reported peak (both sides should see the same content).
        let snap_peak = det_by_publish
            .get(&s.publish_ns)
            .and_then(|d| hop_history.iter().find(|h| h.0 == d.generation))
            .map_or(f32::NAN, |h| h.1);

        if let Some(&(_ms, edge, ncc)) = raw_by_index.get(&idx) {
            let delta = edge as i64 - peak_idx as i64;
            let delta_ms = delta as f64 * ns_per_frame / 1.0e6;
            eprintln!(
                "F.coord   click={idx:>3} hop=[{first},{g_newest}] hop_newest={g_newest} \
                 peak_abs={peak_idx} in_hop={in_hop} peak_val={peak_val:.4} snap_peak={snap_peak:.4} \
                 raw_edge={edge} ncc={ncc:.3} Dframes={delta} (D={delta_ms:+.2}ms)"
            );

            // ---- 2. ENERGY PROFILE: 70 one-ms buckets over [edge-50ms, edge+20ms] ----
            let start = edge as i64 - 50 * fpb as i64;
            let levels = bucket_peaks(mono, start, fpb, 70);
            let scale = levels.iter().copied().fold(0.0f32, f32::max).max(1e-9);
            let hexline: String = levels
                .iter()
                .map(|&v| {
                    let lvl = ((v / scale) * 15.0).round().clamp(0.0, 15.0) as u32;
                    char::from_digit(lvl, 16).unwrap_or('0')
                })
                .collect();
            eprintln!(
                "F.energy  click={idx:>3} span=[-50..+20]ms corr@bucket50 peak_scale={scale:.4} \
                 lvl={hexline}"
            );

            // ---- 3. PER-PUSH timestamps covering the click's window ----
            let lo_frame = start.max(0) as u64;
            let hi_frame = (edge as i64 + 20 * fpb as i64).max(0) as u64;
            for pf in pushes {
                let push_first = pf.cumulative_frames.saturating_sub(u64::from(pf.frames));
                let push_last = pf.cumulative_frames.saturating_sub(1);
                if push_last < lo_frame || push_first > hi_frame {
                    continue;
                }
                if driver_available {
                    let skew = pf.delivery_ns as i64 - pf.driver_capture_ns as i64;
                    eprintln!(
                        "F.push    click={idx:>3} cum={} frames={} wall_delivery_ns={} \
                         driver_capture_ns={} wall_minus_driver_ns={skew}",
                        pf.cumulative_frames, pf.frames, pf.delivery_ns, pf.driver_capture_ns,
                    );
                } else {
                    eprintln!(
                        "F.push    click={idx:>3} cum={} frames={} wall_delivery_ns={}",
                        pf.cumulative_frames, pf.frames, pf.delivery_ns,
                    );
                }
            }
        } else {
            eprintln!(
                "F.coord   click={idx:>3} hop=[{first},{g_newest}] hop_newest={g_newest} \
                 peak_abs={peak_idx} in_hop={in_hop} peak_val={peak_val:.4} snap_peak={snap_peak:.4} \
                 raw_edge=NONE (no raw correlation) — publish side only"
            );
        }

        // ---- 4. HOP DETECTION context: threshold + peaks of the 10 hops before ----
        if let Some(d) = det_by_publish.get(&s.publish_ns) {
            if let Some(pos) = hop_history.iter().position(|h| h.0 == d.generation) {
                let lo = pos.saturating_sub(10);
                let before: Vec<String> = hop_history[lo..pos]
                    .iter()
                    .map(|h| format!("{:.3}", h.1))
                    .collect();
                eprintln!(
                    "F.hops    click={idx:>3} threshold={:.3} detect_gen={} detect_peak={:.3} \
                     peaks_before=[{}]",
                    p.threshold,
                    d.generation,
                    hop_history[pos].1,
                    before.join(" "),
                );
            }
        }

        // ---- 5. OUTPUT side: emission bookkeeping + double-write check ----
        let dup = emit_count.get(&idx).copied().unwrap_or(0);
        eprintln!(
            "F.output  click={idx:>3} emit_ns={} output_delay_ns={} emissions_for_index={dup}{}",
            s.emit_ns,
            s.output_delay_ns,
            if dup > 1 { "  <== DOUBLE-WRITE" } else { "" },
        );
    }
    eprintln!("-- END FORENSIC DUMP ------------------------------------------");
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
    println!(
        "note: emit → publish is anchored on the capture-delivery clock — the hop's newest frame \
         is stamped with when it entered scia's ring (last_push_ns minus ring occupancy), not the \
         DSP's processing time. This is the same clock --raw-ring anchors emit → raw-arrival on, so \
         raw-arrival ≤ publish ≤ raw-arrival + one hop ({HOP_FRAMES} frames, {hop_ms:.2} ms) on one \
         run. output delay (cb→play) is the probe player's own render buffering, not scia's path."
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
