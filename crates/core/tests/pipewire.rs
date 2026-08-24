//! Linux baseline tripwire: pin cpal's PipeWire sink-capture behaviour.
//!
//! The Linux system-mix path depends on undocumented cpal 0.18 behaviour: with
//! the PipeWire host, an `Audio/Sink` is exposed as a duplex (input-capable)
//! device, and opening an *input* stream on it sets
//! `PW_KEY_STREAM_CAPTURE_SINK=true`, which turns the input into a capture of
//! whatever is playing to that sink (the sink monitor = the real system mix).
//! That behaviour lives in cpal's source, not its public contract — see
//! `docs/probes/p2-pipewire-pin.md` for the exact file and lines — so the cpal
//! version is pinned exactly and this test continuously verifies the behaviour
//! against a live PipeWire session.
//!
//! The whole file compiles only on Linux with the `capture-pipewire` feature.
//! It never runs on an ordinary build: it is a no-op skip unless
//! `SCIA_PIPEWIRE_TEST=1` is set, because it needs a headless PipeWire session
//! with a known null sink and a known tone playing into it — the environment the
//! `pipewire` CI job stands up. A developer machine with PipeWire can run it the
//! same way (see the probe doc).
#![cfg(all(target_os = "linux", feature = "capture-pipewire"))]

use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::spectrum::{SpectrumAnalyzer, SpectrumConfig};
use scia_core::{Activity, CpalBackend, DeviceSelector, Engine, EngineConfig, list_devices};

/// The null sink the CI job (or a local run) creates; the device that carries
/// the system mix must be named for it.
const SINK_MATCH: &str = "scia-test-sink";
/// The tone the job plays into the sink.
const TONE_HZ: f32 = 1_000.0;

/// Index and value of the largest entry.
fn argmax(v: &[f32]) -> (usize, f32) {
    let mut bi = 0;
    let mut bv = f32::MIN;
    for (i, &x) in v.iter().enumerate() {
        if x > bv {
            bv = x;
            bi = i;
        }
    }
    (bi, bv)
}

#[test]
fn pipewire_sink_capture_pins_cpal_behaviour() {
    if std::env::var("SCIA_PIPEWIRE_TEST").as_deref() != Ok("1") {
        println!(
            "skip: SCIA_PIPEWIRE_TEST != 1 — this test needs a headless PipeWire \
             session with a `{SINK_MATCH}` null sink and a {TONE_HZ:.0} Hz tone \
             playing into it (see docs/probes/p2-pipewire-pin.md)"
        );
        return;
    }

    // Tripwire 1: the PipeWire host must be present. If cpal ever stops
    // compiling in / detecting the PipeWire host, the whole Linux system-mix
    // baseline is gone and this fails loudly.
    let hosts = cpal::available_hosts();
    assert!(
        hosts.contains(&cpal::HostId::PipeWire),
        "cpal::available_hosts() does not contain the PipeWire host: {hosts:?} — \
         the Linux system-mix baseline depends on it"
    );
    let pipewire_host = cpal::HostId::PipeWire.to_string();

    // Tripwire 2: the null sink must show up as an input-capable device on the
    // PipeWire host (cpal exposes `Audio/Sink` nodes as duplex, so they appear
    // among the input devices). Print the whole table for the log.
    let devices = list_devices().expect("list_devices() failed with the PipeWire host up");
    println!("device table ({} device(s)):", devices.len());
    for d in &devices {
        println!(
            "  host={} kind={:?} default_in={} default_out={} name={}",
            d.host, d.kind, d.is_default_input, d.is_default_output, d.name
        );
    }
    let has_sink = devices
        .iter()
        .any(|d| d.host == pipewire_host && d.name.contains(SINK_MATCH));
    assert!(
        has_sink,
        "no PipeWire (host={pipewire_host:?}) device whose name contains {SINK_MATCH:?} \
         was enumerated — the null sink is missing or cpal no longer exposes sinks \
         as input-capable devices"
    );

    // Open the system-mix capture through the engine: default device on the
    // PipeWire host, preferring PipeWire (the sink monitor).
    let backend = CpalBackend {
        device: DeviceSelector::Default,
        prefer_pipewire: true,
    };
    let (engine, mut reader) = Engine::start(Box::new(backend), EngineConfig::default())
        .expect("Engine::start failed on the PipeWire default device");

    let format = engine.format();
    println!(
        "negotiated format: {} Hz, {} channel(s)",
        format.sample_rate, format.channels
    );
    assert_eq!(
        format.sample_rate, 48_000,
        "expected PipeWire's default graph rate of 48 kHz, got {} Hz",
        format.sample_rate
    );
    assert!(
        (1..=2).contains(&format.channels),
        "expected mono or stereo, got {} channels",
        format.channels
    );

    // Reconstruct the bar → frequency mapping the DSP thread uses (default
    // spectrum config at the negotiated sample rate), so we can check which
    // frequency band the loudest bar covers.
    let analyzer = SpectrumAnalyzer::new(SpectrumConfig::default(), format.sample_rate);
    let bins = analyzer.bar_bins();

    // Poll for up to 5 s: the captured audio must be the played tone — present
    // (not starved, loud, Active) and peaking in the bar whose band contains
    // 1 kHz.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = *reader.latest();
    let mut matched = false;
    let mut loud_idx = 0usize;
    while Instant::now() < deadline {
        let snap = *reader.latest();
        last = snap;
        let len = (snap.spectrum_len as usize).min(bins.len());
        if !snap.starved && snap.activity == Activity::Active && snap.rms >= 0.02 && len > 0 {
            let (loud, _) = argmax(&snap.spectrum[..len]);
            let b = bins[loud];
            if b.f_lo <= TONE_HZ && TONE_HZ <= b.f_hi {
                matched = true;
                loud_idx = loud;
                break;
            }
        }
        sleep(Duration::from_millis(10));
    }

    if !matched {
        let stats = engine.stats();
        let len = (last.spectrum_len as usize).min(bins.len());
        let (loud, val) = if len > 0 {
            argmax(&last.spectrum[..len])
        } else {
            (0, 0.0)
        };
        let band = bins.get(loud).copied();
        engine.stop();
        panic!(
            "did not capture the {TONE_HZ:.0} Hz tone within 5 s.\n  \
             last snapshot: starved={} activity={:?} rms={:.4} peak={:.4} \
             generation={}\n  loudest bar: idx={loud} value={val:.4} band={band:?}\n  \
             stats: pushes={} pushed_frames={} dropped_frames={} xruns={} \
             hops_processed={} hops_synthesized={}",
            last.starved,
            last.activity,
            last.rms,
            last.peak,
            last.generation,
            stats.pushes,
            stats.pushed_frames,
            stats.dropped_frames,
            stats.xruns,
            stats.hops_processed,
            stats.hops_synthesized,
        );
    }

    let band = bins[loud_idx];
    let stats_mid = engine.stats();
    println!(
        "captured tone: rms={:.4} peak={:.4} loudest bar idx={loud_idx} \
         band={:.1}..{:.1} Hz (contains {TONE_HZ:.0} Hz)",
        last.rms, last.peak, band.f_lo, band.f_hi
    );
    println!(
        "stats at match: pushes={} pushed_frames={} dropped_frames={} xruns={} \
         hops_processed={} hops_synthesized={}",
        stats_mid.pushes,
        stats_mid.pushed_frames,
        stats_mid.dropped_frames,
        stats_mid.xruns,
        stats_mid.hops_processed,
        stats_mid.hops_synthesized,
    );

    // Steady delivery: over the last 2 s of the run no hop is synthesized (the
    // capture never starves) and no frame is dropped.
    let syn_before = stats_mid.hops_synthesized;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        let _ = reader.latest();
        sleep(Duration::from_millis(10));
    }
    let stats_end = engine.stats();
    println!(
        "stats after +2 s: pushes={} pushed_frames={} dropped_frames={} xruns={} \
         hops_processed={} hops_synthesized={}",
        stats_end.pushes,
        stats_end.pushed_frames,
        stats_end.dropped_frames,
        stats_end.xruns,
        stats_end.hops_processed,
        stats_end.hops_synthesized,
    );

    let dropped = stats_end.dropped_frames;
    let syn_after = stats_end.hops_synthesized;
    engine.stop();

    assert_eq!(
        dropped, 0,
        "the pipeline dropped {dropped} frames to ring overflow"
    );
    assert_eq!(
        syn_after,
        syn_before,
        "capture starved: {} hop(s) were synthesized as silence during the last 2 s",
        syn_after - syn_before
    );
}
