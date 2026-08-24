//! Integration tests for the causal beat tracker ([`scia_core::beat`]).
//!
//! Each case drives the real per-hop DSP seam ([`HopProcessor`]) — the same path
//! the DSP thread runs — with the synthetic backend's own fill routine, then
//! reads the beat fields straight off the published [`FeatureSnapshot`]s. They
//! assert the tracker locks the demo/click tempo to within a few BPM at high
//! confidence with a beat-rate phase, and that unmusical input (silence, noise,
//! a sustained tone) stays honestly unconfident and unlocked. No audio stack is
//! present.
//!
//! Known metrical ambiguity: a beat tracker can lock a metrical multiple
//! (half- or double-time) of the true tempo — an accepted outcome. The demo and
//! click tempi here are induced with a Rayleigh prior centred on ~120 BPM, and
//! each case checks the actual locked value; the phase-rate checks count beats
//! rather than assume the exact BPM, so they document (not paper over) that a
//! multiple would still read as a regular phase.

use std::time::Instant;

use scia_core::synthetic::fill_signal;
use scia_core::{HopProcessor, Signal, StreamFormat, sample_ring};

const SR: u32 = 48_000;
const HOP: usize = 256;
const CH: usize = 2;
const FORMAT: StreamFormat = StreamFormat {
    sample_rate: SR,
    channels: 2,
};
const HOPS_PER_S: f32 = SR as f32 / HOP as f32; // ~187.5

/// A published hop's beat fields.
#[derive(Clone, Copy)]
struct Beat {
    tempo_bpm: f32,
    phase: f32,
    confidence: f32,
}

/// Drive `signal` through the per-hop processor for `seconds` and return the
/// beat fields of every published hop.
fn run_signal(signal: Signal, seconds: f32) -> Vec<Beat> {
    run_fill(seconds, |frame_index, buf| {
        fill_signal(buf, HOP, CH, signal, f64::from(SR), frame_index);
    })
}

/// Drive an arbitrary per-hop fill through the processor for `seconds`.
fn run_fill(seconds: f32, mut fill: impl FnMut(u64, &mut [f32])) -> Vec<Beat> {
    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(HOP, 2, SR);
    let total_hops = (seconds * HOPS_PER_S) as u64;
    let mut buf = vec![0.0f32; HOP * CH];
    let mut frame_index = 0u64;
    let mut out = Vec::with_capacity(total_hops as usize);
    for _ in 0..total_hops {
        fill(frame_index, &mut buf);
        sink.push(&buf);
        let snap = processor
            .try_process(&mut consumer, FORMAT, 0, 0)
            .expect("a full hop should be available");
        out.push(Beat {
            tempo_bpm: snap.tempo_bpm,
            phase: snap.beat_phase,
            confidence: snap.beat_confidence,
        });
        frame_index += HOP as u64;
    }
    out
}

/// Number of beat-phase wraps (a drop of more than half a period) over `beats`.
fn count_wraps(beats: &[Beat]) -> usize {
    beats
        .windows(2)
        .filter(|w| w[1].phase < w[0].phase - 0.3)
        .count()
}

/// Assert a demo-music run locks near `bpm`: after a lock-in window the tempo is
/// within ±3 BPM, confidence clears a meaningful gate, the beat fields advance
/// monotonically (phase stays in `[0,1)`), and the phase wraps at the beat rate.
fn assert_music_locks(bpm: f32) {
    let seconds = 10.0f32;
    let beats = run_signal(Signal::Music { bpm }, seconds);

    // Look only at the last 4 s, well past the ~3.5 s lock-in.
    let tail_len = (4.0 * HOPS_PER_S) as usize;
    let tail = &beats[beats.len() - tail_len..];

    let n = tail.len() as f32;
    let tempo_mean = tail.iter().map(|b| b.tempo_bpm).sum::<f32>() / n;
    let conf_min = tail.iter().map(|b| b.confidence).fold(f32::MAX, f32::min);
    let wraps = count_wraps(tail);
    let expected_beats = f64::from(bpm) / 60.0 * 4.0;

    println!(
        "beat_tracking music {bpm:.0}: locked tempo {tempo_mean:.2} bpm, \
         min confidence {conf_min:.3}, phase wraps {wraps} over 4 s \
         (expected ~{expected_beats:.1} beats)"
    );

    assert!(
        (tempo_mean - bpm).abs() <= 3.0,
        "tempo {tempo_mean:.2} not within ±3 of {bpm:.0}"
    );
    assert!(
        conf_min > 0.4,
        "confidence dipped to {conf_min:.3} (<0.4) over the locked tail"
    );
    for b in tail {
        assert!(
            (0.0..1.0).contains(&b.phase),
            "phase {} out of [0,1)",
            b.phase
        );
    }
    // Phase wraps at the beat rate (±25 %): the grid advances once per beat.
    let lo = 0.75 * expected_beats;
    let hi = 1.25 * expected_beats;
    assert!(
        (wraps as f64) >= lo && (wraps as f64) <= hi,
        "phase wrapped {wraps} times over 4 s, expected {lo:.1}..={hi:.1} \
         (beat rate for {bpm:.0} bpm)"
    );
}

#[test]
fn music_locks_tempo_100() {
    assert_music_locks(100.0);
}

#[test]
fn music_locks_tempo_140() {
    assert_music_locks(140.0);
}

#[test]
fn click_train_locks_150() {
    let seconds = 10.0f32;
    let beats = run_signal(
        Signal::Clicks {
            bpm: 150.0,
            amp: 0.8,
        },
        seconds,
    );
    let tail_len = (4.0 * HOPS_PER_S) as usize;
    let tail = &beats[beats.len() - tail_len..];

    let n = tail.len() as f32;
    let tempo_mean = tail.iter().map(|b| b.tempo_bpm).sum::<f32>() / n;
    let conf_min = tail.iter().map(|b| b.confidence).fold(f32::MAX, f32::min);
    let wraps = count_wraps(tail);
    let expected_beats = 150.0 / 60.0 * 4.0;

    println!(
        "beat_tracking clicks 150: locked tempo {tempo_mean:.2} bpm, \
         min confidence {conf_min:.3}, phase wraps {wraps} over 4 s \
         (expected ~{expected_beats:.1} beats)"
    );

    assert!(
        (tempo_mean - 150.0).abs() <= 3.0,
        "click tempo {tempo_mean:.2} not within ±3 of 150"
    );
    assert!(
        conf_min > 0.5,
        "click-train confidence dipped to {conf_min:.3} (<0.5)"
    );
    assert!(
        (wraps as f64) >= 0.75 * expected_beats && (wraps as f64) <= 1.25 * expected_beats,
        "click phase wrapped {wraps} times over 4 s, expected ~{expected_beats:.1}"
    );
}

#[test]
fn silence_stays_unconfident() {
    let beats = run_signal(Signal::Silence, 10.0);
    let conf_max = beats.iter().map(|b| b.confidence).fold(0.0f32, f32::max);
    let ever_locked = beats.iter().any(|b| b.tempo_bpm > 0.0);
    println!("beat_tracking silence: max confidence {conf_max:.3}, ever locked {ever_locked}");
    assert!(conf_max < 0.35, "silence reached confidence {conf_max:.3}");
    assert!(!ever_locked, "silence produced a tempo lock");
}

#[test]
fn white_noise_stays_unconfident() {
    // Deterministic white-noise-like input via a splitmix hash of the frame
    // index, so the test carries no RNG state and reproduces exactly.
    let beats = run_fill(10.0, |frame_index, buf| {
        for (i, x) in buf.iter_mut().enumerate() {
            let mut z = (frame_index + i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            *x = ((z >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * 0.3;
        }
    });
    let conf_max = beats.iter().map(|b| b.confidence).fold(0.0f32, f32::max);
    let ever_locked = beats.iter().any(|b| b.tempo_bpm > 0.0);
    println!("beat_tracking white noise: max confidence {conf_max:.3}, ever locked {ever_locked}");
    assert!(
        conf_max < 0.35,
        "white noise reached confidence {conf_max:.3} (above the gate)"
    );
    assert!(!ever_locked, "white noise produced a tempo lock");
}

#[test]
fn sustained_tone_stays_unconfident() {
    // A steady sine has no beats: its ODF is a sustained ripple, not impulses.
    // Even as the onset detector's slow normalization creeps that ripple up to
    // full scale, the kurtosis-based impulsiveness gate keeps confidence low.
    let beats = run_signal(
        Signal::Sine {
            hz: 440.0,
            amp: 0.3,
        },
        12.0,
    );
    let conf_max = beats.iter().map(|b| b.confidence).fold(0.0f32, f32::max);
    let ever_locked = beats.iter().any(|b| b.tempo_bpm > 0.0);
    println!(
        "beat_tracking sustained tone: max confidence {conf_max:.3}, ever locked {ever_locked}"
    );
    assert!(
        conf_max < 0.35,
        "sustained tone reached confidence {conf_max:.3} (above the gate)"
    );
    assert!(!ever_locked, "sustained tone produced a tempo lock");
}
