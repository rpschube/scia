//! Onset-detection and band-split integration tests. Everything is driven
//! through [`HopProcessor`] with deterministically generated samples pushed
//! through the sample ring, so timing is exact and thread-free — no audio
//! hardware and no dependence on thread scheduling.

use scia_core::{HopProcessor, StreamFormat, sample_ring};
use std::time::Instant;

const SR: u32 = 48_000;
const HOP: usize = 256;
const CH: usize = 2;
const FORMAT: StreamFormat = StreamFormat {
    sample_rate: SR,
    channels: 2,
};

/// One hop of audio in seconds.
fn dt() -> f32 {
    HOP as f32 / SR as f32
}

/// A one-sample-per-beat click train: `amp` on frames that are a multiple of the
/// beat period, silence elsewhere.
fn click_sample(frame: u64, bpm: f32, amp: f32) -> f32 {
    let period = (60.0 / bpm * SR as f32).round() as u64;
    if period > 0 && frame % period == 0 {
        amp
    } else {
        0.0
    }
}

/// A continuous sine.
fn sine_sample(frame: u64, hz: f32, amp: f32) -> f32 {
    (f64::from(amp) * (2.0 * std::f64::consts::PI * f64::from(hz) * frame as f64 / f64::from(SR)))
        .sin() as f32
}

/// A per-hop snapshot record reduced to what these tests assert on.
#[derive(Clone, Copy, Debug)]
struct Hop {
    #[allow(dead_code)]
    generation: u64,
    t: f32,
    onset: bool,
    bands: [f32; 3],
    flux: f32,
    age_ms: f32,
}

/// Drive `hops` hops of a mono generator (duplicated to both channels) through a
/// [`HopProcessor`], returning one record per produced hop.
fn drive(processor: &mut HopProcessor, g: impl Fn(u64) -> f32, hops: usize) -> Vec<Hop> {
    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut buf = vec![0.0f32; HOP * CH];
    let mut out = Vec::with_capacity(hops);
    let mut frame = 0u64;
    let d = dt();
    for _ in 0..hops {
        for f in 0..HOP {
            let s = g(frame + f as u64);
            buf[f * CH] = s;
            buf[f * CH + 1] = s;
        }
        sink.push(&buf);
        if let Some(snap) = processor.try_process(&mut consumer, FORMAT, 0, 0) {
            out.push(Hop {
                generation: snap.generation,
                t: snap.generation as f32 * d,
                onset: snap.onset,
                bands: snap.bands,
                flux: snap.flux,
                age_ms: snap.onset_age_ms,
            });
        }
        frame += HOP as u64;
    }
    out
}

/// Onset times (seconds) from a hop record.
fn onset_times(hops: &[Hop]) -> Vec<f32> {
    hops.iter().filter(|h| h.onset).map(|h| h.t).collect()
}

#[test]
fn clicks_produce_onsets_at_tempo() {
    let mut p = HopProcessor::new(HOP, 2, SR);
    // 5 s of audio at 48 kHz = 240 000 frames = 937.5 hops; run 938.
    let hops = drive(&mut p, |f| click_sample(f, 120.0, 0.8), 938);
    let times = onset_times(&hops);

    // Onsets in the last 4 s (t >= 1.0): 120 bpm = one beat every 0.5 s = 8.
    let last4: Vec<f32> = times.iter().copied().filter(|&t| t >= 1.0).collect();
    assert!(
        (7..=9).contains(&last4.len()),
        "expected ~8 onsets in the last 4 s, got {}",
        last4.len()
    );

    // Consecutive gaps in the last-4s window are ~500 ms.
    for w in last4.windows(2) {
        let gap_ms = (w[1] - w[0]) * 1000.0;
        assert!(
            (485.0..=515.0).contains(&gap_ms),
            "onset gap {gap_ms:.1} ms not within 500 ± 15",
        );
    }
}

#[test]
fn steady_sine_has_no_onsets() {
    let mut p = HopProcessor::new(HOP, 2, SR);
    let hops = drive(
        &mut p,
        |f| sine_sample(f, 1_000.0, 0.5),
        (3.3 / dt()) as usize,
    );
    // Ignore the first 300 ms (the tone's own turn-on transient).
    let late: Vec<&Hop> = hops.iter().filter(|h| h.t >= 0.3).collect();
    let onsets = late.iter().filter(|h| h.onset).count();
    assert_eq!(
        onsets, 0,
        "a steady sine fired {onsets} onsets after 300 ms"
    );
    // The normalized flux stays well under the onset threshold once steady.
    let max_flux = late.iter().fold(0.0f32, |m, h| m.max(h.flux));
    assert!(
        max_flux < 0.3,
        "steady-sine flux {max_flux:.4} reached the threshold"
    );
}

#[test]
fn silence_has_no_onsets() {
    let mut p = HopProcessor::new(HOP, 2, SR);
    let hops = drive(&mut p, |_| 0.0, (3.0 / dt()) as usize);
    let onsets = hops.iter().filter(|h| h.onset).count();
    assert_eq!(onsets, 0, "silence fired {onsets} onsets");

    // The onset-age clock only ever grows over a silent run.
    let mut prev = 0.0f32;
    for h in &hops {
        assert!(
            h.age_ms >= prev,
            "age went backwards {prev} -> {}",
            h.age_ms
        );
        assert!(h.age_ms <= 60_000.0);
        prev = h.age_ms;
    }
    assert!(prev > 0.0, "age never advanced");
}

#[test]
fn min_ioi_suppresses_double_triggers() {
    let min_ioi_ms = 20.0f32;
    for &bpm in &[1_500.0f32, 6_000.0] {
        let mut p = HopProcessor::new(HOP, 2, SR);
        let hops = drive(&mut p, |f| click_sample(f, bpm, 0.8), (2.0 / dt()) as usize);
        let times = onset_times(&hops);
        assert!(
            times.len() >= 2,
            "bpm {bpm}: too few onsets {}",
            times.len()
        );
        let mut min_gap = f32::MAX;
        for w in times.windows(2) {
            min_gap = min_gap.min((w[1] - w[0]) * 1000.0);
        }
        assert!(
            min_gap >= min_ioi_ms - 1e-3,
            "bpm {bpm}: onsets {min_gap:.2} ms apart, closer than min IOI {min_ioi_ms}",
        );
    }
}

#[test]
fn bands_place_tones_correctly() {
    // Each pure tone should put its energy in the matching band. This is checked
    // on the instantaneous linear energies (`band_levels`), which — unlike the
    // ratio-normalized snapshot `bands` — directly reflect where the power sits.
    for (hz, want) in [(60.0f32, 0usize), (1_000.0, 1), (5_000.0, 2)] {
        let mut p = HopProcessor::new(HOP, 2, SR);
        let _ = drive(&mut p, |f| sine_sample(f, hz, 0.5), (0.5 / dt()) as usize);
        let levels = p.band_levels();
        let (dom, _) = levels
            .iter()
            .enumerate()
            .fold(
                (0usize, f32::MIN),
                |(bi, bv), (i, &v)| {
                    if v > bv { (i, v) } else { (bi, bv) }
                },
            );
        assert_eq!(
            dom, want,
            "{hz} Hz dominant band {dom} (levels {levels:?}), want {want}"
        );
    }
}

#[test]
fn band_ratio_normalizes_to_one() {
    // Steady 1 kHz for 10 s: the mid band relaxes to ~1.0.
    let mut p = HopProcessor::new(HOP, 2, SR);
    let hops = drive(
        &mut p,
        |f| sine_sample(f, 1_000.0, 0.5),
        (10.0 / dt()) as usize,
    );
    let settled = hops.last().unwrap().bands[1];
    assert!(
        (settled - 1.0).abs() <= 0.1,
        "steady mid ratio {settled:.4} not within 1.0 ± 0.1",
    );

    // A step from −20 dBFS (amp 0.1) to −6 dBFS (amp 0.501) swells the mid band
    // well above 1.5 within 100 ms, then relaxes back toward 1.0.
    let mut p = HopProcessor::new(HOP, 2, SR);
    let pre_hops = (3.0 / dt()) as usize; // settle the average at the quiet level
    let step_frame = pre_hops as u64 * HOP as u64;
    let hz = 1_000.0f32;
    let g = move |f: u64| {
        let amp = if f < step_frame { 0.1 } else { 0.501 };
        sine_sample(f, hz, amp)
    };
    let hops = drive(&mut p, g, pre_hops + (10.0 / dt()) as usize);

    let step_t = pre_hops as f32 * dt();
    // Peak mid ratio in the 100 ms right after the step.
    let peak_after = hops
        .iter()
        .filter(|h| h.t >= step_t && h.t <= step_t + 0.1)
        .fold(0.0f32, |m, h| m.max(h.bands[1]));
    assert!(
        peak_after > 1.5,
        "step did not swell the mid band: peak {peak_after:.3}",
    );
    // Well after the step the ratio has relaxed back toward 1.0.
    let relaxed = hops.last().unwrap().bands[1];
    assert!(
        relaxed < 1.5,
        "mid ratio did not relax after the step: {relaxed:.3}",
    );
}

#[test]
fn silence_does_not_drag_averages() {
    // 1 kHz for 5 s, then 5 s of silence, then 1 kHz again: because the average
    // is frozen during silence, the mid ratio on return is near 1.0, not a huge
    // swell (which is what a decayed-to-zero average would produce).
    let secs_a = 5.0f32;
    let secs_sil = 5.0f32;
    let hops_a = (secs_a / dt()) as u64;
    let hops_sil = (secs_sil / dt()) as u64;
    let a_end = hops_a * HOP as u64;
    let sil_end = (hops_a + hops_sil) * HOP as u64;
    let hz = 1_000.0f32;
    let g = move |f: u64| {
        if f < a_end {
            sine_sample(f, hz, 0.5)
        } else if f < sil_end {
            0.0
        } else {
            sine_sample(f, hz, 0.5)
        }
    };
    let mut p = HopProcessor::new(HOP, 2, SR);
    let total = (hops_a + hops_sil) as usize + (0.3 / dt()) as usize;
    let hops = drive(&mut p, g, total);

    // The tone returns after hops_a + hops_sil hops, i.e. ~10 s in.
    let return_t = (hops_a + hops_sil) as f32 * dt();
    let ratio = hops
        .iter()
        .find(|h| h.t >= return_t + 0.1)
        .map(|h| h.bands[1])
        .expect("a hop ~100 ms after the tone returns");
    assert!(
        (0.7..=1.5).contains(&ratio),
        "mid ratio {ratio:.3} on return not within 0.7..=1.5 (average was dragged)",
    );
}
