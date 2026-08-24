//! Engine-level test for the diagnostic beat-debug side channel
//! ([`Engine::beat_debug`]). A click train is driven through the *real* engine
//! (capture ring → DSP thread → feature bus); once the pipeline locks, the side
//! channel must expose the in-thread tracker's induction stats, and those stats
//! must agree with the published lock. No audio stack is present.

use std::thread::sleep;
use std::time::{Duration, Instant};

use scia_core::{Engine, EngineConfig, Pacing, Signal, StreamFormat, SyntheticBackend};

const STEREO_48K: StreamFormat = StreamFormat {
    sample_rate: 48_000,
    channels: 2,
};

/// After a click train locks, [`Engine::beat_debug`] returns the real tracker's
/// stats: at least one induction pass has run, and its winning candidate tempo
/// matches the tempo the engine publishes.
#[test]
fn beat_debug_tracks_the_published_lock() {
    let backend = SyntheticBackend {
        format: STEREO_48K,
        signal: Signal::Clicks {
            bpm: 150.0,
            amp: 0.8,
        },
        // Deliver ~12 s of clicks as fast as the ring accepts, so the lock is
        // reached quickly; we read the side channel while the clicks still play.
        pacing: Pacing::Unpaced {
            total_frames: 48_000 * 12,
        },
        emit_log: None,
    };

    let (engine, mut reader) =
        Engine::start(Box::new(backend), EngineConfig::default()).expect("engine start");

    // Poll until the pipeline publishes a lock and the side channel has seen at
    // least one induction pass, or time out.
    let start = Instant::now();
    let mut published_bpm = 0.0f32;
    let mut debug = None;
    while start.elapsed() < Duration::from_secs(12) {
        let snap = *reader.latest();
        let dbg = engine.beat_debug();
        if snap.tempo_bpm > 0.0 && dbg.is_some_and(|d| d.inductions > 0 && d.locked) {
            published_bpm = snap.tempo_bpm;
            debug = dbg;
            break;
        }
        sleep(Duration::from_millis(5));
    }

    let dbg = debug.expect("engine.beat_debug() never reported a locked induction pass");

    println!(
        "beat_debug: inductions {}, candidate {:.2} bpm, debug.tempo {:.2}, published {:.2}, \
         locked {}, coasting {}",
        dbg.inductions, dbg.candidate_bpm, dbg.tempo_bpm, published_bpm, dbg.locked, dbg.coasting
    );

    assert!(dbg.inductions > 0, "no induction pass was mirrored");
    assert!(published_bpm > 0.0, "engine never published a tempo");
    assert!(dbg.locked, "side channel should report the firm lock");
    // The winning candidate the induction pass found matches the tempo the
    // engine publishes (both track the same ~150 bpm click grid).
    assert!(
        (dbg.candidate_bpm - published_bpm).abs() <= 6.0,
        "candidate {:.2} bpm disagrees with published {:.2} bpm",
        dbg.candidate_bpm,
        published_bpm
    );
    // And the side channel's own live published-tempo mirror agrees with the bus.
    assert!(
        (dbg.tempo_bpm - published_bpm).abs() <= 6.0,
        "debug tempo {:.2} disagrees with published {:.2}",
        dbg.tempo_bpm,
        published_bpm
    );
    // Sanity: it locked near the driven 150 bpm (a metrical multiple would be a
    // different, documented outcome, but this clean grid locks the fundamental).
    assert!(
        (published_bpm - 150.0).abs() <= 8.0,
        "published tempo {published_bpm:.2} not near the driven 150 bpm"
    );

    engine.stop();
}
