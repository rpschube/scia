//! Per-frame scripting overhead fits the frame budget: a representative scripted
//! scene (the shipped 200-particle `swarm`, with the full sandbox + interrupt +
//! memory cap) sustains 60 fps.
//!
//! Modeled on the TUI `overlay_cost_under_frame_budget` test: measure the mean
//! wall-clock cost of one scripted tick (`update` + `render`) over many frames
//! after a warm-up, and assert it against a generous debug-profile bound. At
//! 60 fps the frame budget is 1000/60 = 16.667 ms; a scripted tick well under a
//! few ms leaves the whole rest of the frame for capture, layout and present.

use std::time::{Duration, Instant};

use scia_core::{FEATURE_SCHEMA_VERSION, FeatureSnapshot};
use scia_scenes::{Canvas, LuauLimits, LuauScene, Scene, SceneCtx, shipped_scenes};

/// A snapshot that varies per frame so the tick never degenerates to constant
/// folding — the same shape the P5 bench feeds.
fn refresh(s: &mut FeatureSnapshot, frame: u64) {
    let t = frame as f32 * 0.01;
    s.rms = 0.5 + 0.4 * t.sin();
    s.peak = 0.6 + 0.3 * (t * 1.3).sin();
    s.onset = frame % 16 == 0;
    for (i, bin) in s.spectrum.iter_mut().take(64).enumerate() {
        *bin = 0.5 + 0.5 * (t + i as f32 * 0.1).sin();
    }
}

#[test]
fn scripted_tick_under_frame_budget() {
    let (_, swarm) = shipped_scenes()
        .iter()
        .find(|(n, _)| *n == "swarm")
        .expect("swarm is shipped");

    // A large per-tick deadline so the safety interrupt never trips during the
    // measurement (we are measuring the normal path, not a runaway); the memory
    // cap and sandbox stay at their production defaults.
    let limits = LuauLimits {
        tick_budget: Duration::from_secs(5),
        ..LuauLimits::default()
    };
    let mut scene = LuauScene::from_source(swarm, "swarm", limits).expect("swarm compiles");
    scene.init(&SceneCtx::new(
        1.6,
        scia_scenes::Palette::default_dark(),
        Default::default(),
    ));

    let mut snap = FeatureSnapshot {
        schema_version: FEATURE_SCHEMA_VERSION,
        sample_rate: 48_000,
        channels: 2,
        spectrum_len: 64,
        ..FeatureSnapshot::default()
    };
    let mut canvas = Canvas::new(1.6);

    // Warm-up: let the particle arrays settle and the canvas reach steady
    // capacity.
    for f in 0..120u64 {
        refresh(&mut snap, f);
        scene.update(&snap, 1.0 / 60.0);
        scene.render(&mut canvas);
    }
    assert!(
        !scene.is_errored(),
        "warm-up faulted: {:?}",
        scene.last_error()
    );

    const N: u64 = 600;
    let start = Instant::now();
    for f in 0..N {
        refresh(&mut snap, 120 + f);
        scene.update(&snap, 1.0 / 60.0);
        scene.render(&mut canvas);
    }
    let mean_ms = start.elapsed().as_secs_f64() * 1000.0 / N as f64;

    println!(
        "scripted swarm tick: {mean_ms:.4} ms mean over {N} frames \
         (200 particles, sandbox+interrupt+memcap; frame budget 16.667 ms)"
    );
    assert!(
        !scene.is_errored(),
        "measurement faulted: {:?}",
        scene.last_error()
    );
    assert!(
        !canvas.primitives().is_empty(),
        "the scripted scene drew something"
    );
    // Generous debug-profile bound: a real (release) tick is tens of
    // microseconds; even an unoptimized build under CI load must stay far under
    // a single 60 fps frame.
    assert!(
        mean_ms < 8.0,
        "scripted tick {mean_ms:.4} ms should be well under the 16.667 ms frame budget"
    );
}
