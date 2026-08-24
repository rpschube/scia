//! Headless end-to-end latency probe (P7) — the "photodiode-free" measuring
//! stick for the audio→feature path.
//!
//! ```text
//! latency_probe [--synthetic] [--device NAME] [--output-device NAME]
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

use scia_core::{
    CaptureError, ClickDetector, CpalBackend, Detection, DeviceKind, DeviceSelector, Emission,
    EmitLog, Engine, EngineConfig, EngineError, FeatureReader, LatencyStats, Matcher, Pacing,
    Signal, StreamHealth, SyntheticBackend, list_devices,
};
#[cfg(feature = "perf-mode")]
use scia_core::{PerfModeConfig, PerfModeStream};

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

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--synthetic" => synthetic = true,
            "--perf-mode" => perf_mode = true,
            "--list" => do_list = true,
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
                    "usage: latency_probe [--synthetic] [--device NAME] [--output-device NAME]\n\
                     \x20                    [--clicks N=25] [--spacing-ms N=400] [--amp F=0.8]\n\
                     \x20                    [--click-ms F=1.0] [--threshold F=0.3] [--perf-mode] [--list]"
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
    if synthetic {
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
    // Perf-mode companion stream first (Windows only), so the endpoint is at its
    // fast period before the loopback capture starts. Held for the whole run.
    #[cfg(feature = "perf-mode")]
    let (_perf_guard, perf_status) = open_perf_mode(perf_mode);
    #[cfg(not(feature = "perf-mode"))]
    let perf_status = {
        if perf_mode {
            eprintln!("perf mode: this build was compiled without the perf-mode feature");
            "unavailable"
        } else {
            "off"
        }
    };

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
    if let StreamHealth::Errored(msg) = engine.health() {
        eprintln!("warning: capture stream reported an error at open: {msg}");
    }

    // Open the click player on the chosen output device.
    let emit_log = Arc::new(EmitLog::new());
    let output_err = Arc::new(AtomicBool::new(false));
    let (stream, output_label) =
        match open_click_player(p, output_device.as_deref(), &engine, &emit_log, &output_err) {
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
    engine: &Engine,
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

    let epoch = engine.epoch();
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
                        log.push(Emission {
                            index: click_idx as u32,
                            emit_ns: cb_ns,
                            output_delay_ns: delay_ns,
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

/// Open the perf-mode companion render stream and report its periods (Windows
/// only). Returns the live stream (held for the run) and the report status.
#[cfg(feature = "perf-mode")]
fn open_perf_mode(perf_mode: bool) -> (Option<PerfModeStream>, &'static str) {
    if !perf_mode {
        return (None, "off");
    }
    match PerfModeStream::open(&PerfModeConfig::default()) {
        Ok(stream) => {
            let info = stream.info();
            let hz = f64::from(info.sample_rate.max(1));
            let ms = |frames: u32| f64::from(frames) * 1000.0 / hz;
            println!(
                "perf mode: endpoint {} Hz, {} ch — chosen engine period {} ({:.3} ms)",
                info.sample_rate,
                info.channels,
                info.chosen_period_frames,
                ms(info.chosen_period_frames),
            );
            (Some(stream), "on")
        }
        Err(e) => {
            eprintln!("perf mode unavailable: {e}");
            (None, "unavailable")
        }
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
