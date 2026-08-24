//! Integration test for the built-in music demo signal ([`Signal::Music`]).
//!
//! It drives the real per-hop DSP seam ([`HopProcessor`]) with the synthetic
//! backend's own fill routine over ~8 s of audio and asserts the extracted
//! features read as music: onsets fire near the beat rate, all three bands see
//! energy, the level stays sane (no clipping catastrophe, no silence), the
//! stereo image has width, and the generator is deterministic. Runs with no
//! audio stack present.

use std::time::Instant;

use scia_core::synthetic::fill_signal;
use scia_core::{HopProcessor, Signal, StreamFormat, sample_ring};

const SR: u32 = 48_000;
const HOP: usize = 256;
const CH: usize = 2;
const BPM: f32 = 112.0;
const FORMAT: StreamFormat = StreamFormat {
    sample_rate: SR,
    channels: 2,
};

/// Run the music signal through the per-hop processor for `seconds` of audio and
/// return every published snapshot's onset flag, rms and ratio-normalized bands.
fn run_music(seconds: f32) -> Vec<(bool, f32, [f32; 3])> {
    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(HOP, 2, SR);
    let signal = Signal::Music { bpm: BPM };
    let total_hops = (seconds * SR as f32 / HOP as f32) as u64;
    let mut buf = vec![0.0f32; HOP * CH];
    let mut frame_index = 0u64;
    let mut out = Vec::with_capacity(total_hops as usize);
    for _ in 0..total_hops {
        fill_signal(&mut buf, HOP, CH, signal, f64::from(SR), frame_index);
        sink.push(&buf);
        let snap = processor
            .try_process(&mut consumer, FORMAT, 0, 0)
            .expect("a full hop should be available");
        out.push((snap.onset, snap.rms, snap.bands));
        frame_index += HOP as u64;
    }
    out
}

/// (a) Onsets fire near the beat rate: at least 0.6× the kick count (the beat
/// rate), and no more than 4× it (hats add extra transients but not without
/// bound). Prints the measured count so the number is visible in CI logs.
#[test]
fn music_onsets_track_the_beat() {
    let seconds = 8.0f32;
    let snaps = run_music(seconds);
    let onsets = snaps.iter().filter(|s| s.0).count();
    let beats = f64::from(BPM) / 60.0 * f64::from(seconds);
    let lower = 0.6 * beats;
    let upper = 4.0 * beats;
    println!(
        "music_onsets_track_the_beat: {onsets} onsets over {seconds:.1}s \
         (beats {beats:.2}, expect {lower:.2}..={upper:.2})"
    );
    assert!(
        onsets as f64 >= lower,
        "only {onsets} onsets in {seconds:.1}s, want at least {lower:.2} (kick rate)"
    );
    assert!(
        onsets as f64 <= upper,
        "{onsets} onsets in {seconds:.1}s, more than {upper:.2} (4× the kick rate)"
    );
}

/// (b) All three bands see energy: the mean ratio of each band over the run
/// clears 0.01.
#[test]
fn music_drives_all_three_bands() {
    let snaps = run_music(8.0);
    let n = snaps.len() as f32;
    let mut sums = [0.0f32; 3];
    for (_, _, bands) in &snaps {
        for i in 0..3 {
            sums[i] += bands[i];
        }
    }
    let means = [sums[0] / n, sums[1] / n, sums[2] / n];
    println!(
        "music_drives_all_three_bands: mean band ratios bass {:.3} mid {:.3} treble {:.3}",
        means[0], means[1], means[2]
    );
    for (i, name) in ["bass", "mid", "treble"].iter().enumerate() {
        assert!(
            means[i] > 0.01,
            "{name} band never saw energy (mean ratio {:.5})",
            means[i]
        );
    }
}

/// (c) The level stays healthy: after the first few hops settle, every hop's rms
/// is within 0.05..0.7 — no clipping catastrophe and no accidental silence.
#[test]
fn music_rms_stays_in_range() {
    let snaps = run_music(8.0);
    // Skip the first few hops while the very first bars ramp up.
    let mut lo = f32::MAX;
    let mut hi = 0.0f32;
    for (_, rms, _) in snaps.iter().skip(8) {
        lo = lo.min(*rms);
        hi = hi.max(*rms);
    }
    println!("music_rms_stays_in_range: rms in {lo:.4}..={hi:.4}");
    assert!(lo >= 0.05, "rms dipped to {lo:.4} (< 0.05, near silence)");
    assert!(
        hi <= 0.7,
        "rms rose to {hi:.4} (> 0.7, clipping catastrophe)"
    );
}

/// (d) The stereo image has width. The reserved `FeatureSnapshot.stereo_correlation`
/// field is not computed in schema 1 (it is always 0), so width is verified at
/// the source. The center bus (kick/bass/pad) is identical on both channels, so
/// the whole-buffer correlation sits near 1.0; the width shows up *at some point*
/// — when a left-panned hat or right-panned sparkle plays, the correlation over
/// that hop drops well below 1.0. The minimum per-hop correlation is asserted
/// below 0.999. A nonzero side-channel (L−R) energy backs the same claim.
#[test]
fn music_has_stereo_width() {
    let signal = Signal::Music { bpm: BPM };
    let frames = SR as usize * 4; // 4 s
    let mut buf = vec![0.0f32; frames * CH];
    fill_signal(&mut buf, frames, CH, signal, f64::from(SR), 0);

    // Minimum per-hop (256-frame) Pearson correlation across the run.
    let mut min_corr = 1.0f64;
    let mut side_sq = 0.0f64;
    let mut mid_sq = 0.0f64;
    let mut f = 0usize;
    while f + HOP <= frames {
        let (mut sl, mut sr, mut sll, mut srr, mut slr) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for i in 0..HOP {
            let l = f64::from(buf[(f + i) * CH]);
            let r = f64::from(buf[(f + i) * CH + 1]);
            sl += l;
            sr += r;
            sll += l * l;
            srr += r * r;
            slr += l * r;
            side_sq += (l - r) * (l - r);
            mid_sq += (l + r) * (l + r);
        }
        let n = HOP as f64;
        let cov = slr - sl * sr / n;
        let var_l = sll - sl * sl / n;
        let var_r = srr - sr * sr / n;
        let denom = (var_l * var_r).sqrt();
        if denom > 1e-12 {
            min_corr = min_corr.min(cov / denom);
        }
        f += HOP;
    }
    let side_ratio = (side_sq / mid_sq).sqrt();
    println!(
        "music_has_stereo_width: min per-hop L/R correlation {min_corr:.4}, \
         side/mid energy ratio {side_ratio:.4}"
    );
    assert!(
        min_corr < 0.999,
        "channels never decorrelated (min per-hop correlation {min_corr:.4}); no width"
    );
    assert!(
        side_ratio > 1e-3,
        "side channel (L−R) is essentially empty (ratio {side_ratio:.5}); no width"
    );
}

/// (e) Deterministic: two runs over the same frame range produce byte-identical
/// samples (also covered as a unit test, asserted here through the public hook
/// over a longer range).
#[test]
fn music_is_deterministic_across_runs() {
    let signal = Signal::Music { bpm: BPM };
    let frames = SR as usize * 2;
    let mut a = vec![0.0f32; frames * CH];
    let mut b = vec![0.0f32; frames * CH];
    fill_signal(&mut a, frames, CH, signal, f64::from(SR), 12_345);
    fill_signal(&mut b, frames, CH, signal, f64::from(SR), 12_345);
    assert_eq!(a, b, "two fills of the same frame range differed");
}
