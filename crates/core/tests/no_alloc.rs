//! The per-hop processing path must not allocate after construction. A
//! counting global allocator wraps the system allocator and records every
//! allocation; the hot loop must not move the counter.

mod support {
    pub mod alloc_watch;
}

use scia_core::backends::convert::{Downmix, f32_id, i16_to_f32};
use scia_core::capture::{DrainTimeline, sample_ring_with_tee, tee_drain_into_timeline};
use scia_core::spectrum::{SpectrumAnalyzer, SpectrumConfig};
use scia_core::{HopProcessor, StreamFormat, sample_ring};
use std::time::Instant;
use support::alloc_watch::{CountingAllocator, watch};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn hot_path_does_not_allocate() {
    let format = StreamFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    let hop = 256usize;
    let channels = 2usize;

    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(hop, 2, 48_000);
    let chunk = vec![0.3f32; hop * channels];

    // Warm up: prime the ring and the processor.
    for _ in 0..5 {
        sink.push(&chunk);
        let _ = processor.try_process(&mut consumer, format, 0, 0);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..200 {
            sink.push(&chunk);
            let snapshot = processor.try_process(&mut consumer, format, 0, 0);
            assert!(snapshot.is_some());
        }
    });

    assert!(
        stray_count == 0,
        "hot path allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

#[test]
fn idle_path_does_not_allocate() {
    // The cheap idle path (rms/peak plus release decay, no FFT) must also be
    // allocation-free. Feed silence so `process_idle` takes its decay branch.
    let format = StreamFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    let hop = 256usize;
    let channels = 2usize;
    // Below the −60 dBFS quiet threshold: the silence hops stay on the cheap path.
    let resume_rms = 0.001f32;

    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(hop, 2, 48_000);
    let tone = vec![0.3f32; hop * channels];
    let silence = vec![0.0f32; hop * channels];

    // Warm up: run a real tone through the full path (allocating FFT scratch is
    // done in `new`, but prime the smoothing state), then a few idle hops.
    for _ in 0..5 {
        sink.push(&tone);
        let _ = processor.try_process(&mut consumer, format, 0, 0);
    }
    for _ in 0..5 {
        sink.push(&silence);
        let _ = processor.process_idle(&mut consumer, format, 0, 0, resume_rms);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..200 {
            sink.push(&silence);
            let snapshot = processor.process_idle(&mut consumer, format, 0, 0, resume_rms);
            assert!(snapshot.is_some());
        }
    });

    assert!(
        stray_count == 0,
        "idle path allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

#[test]
fn full_hop_path_does_not_allocate() {
    // Like `hot_path_does_not_allocate` but with a time-varying click train, so
    // the band splitter's averages move and the onset detector actually fires
    // (exercising every branch of the extended per-hop path), and asserts the
    // whole thing stays allocation-free.
    let format = StreamFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    let hop = 256usize;
    let channels = 2usize;
    let sr = 48_000u64;
    let period = sr / 10; // a click every 100 ms

    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(hop, 2, 48_000);
    let mut chunk = vec![0.0f32; hop * channels];
    let mut frame: u64 = 0;
    let fill = |chunk: &mut [f32], frame: &mut u64| {
        for f in 0..hop {
            let s = if (*frame + f as u64) % period == 0 {
                0.8
            } else {
                0.0
            };
            chunk[f * channels] = s;
            chunk[f * channels + 1] = s;
        }
        *frame += hop as u64;
    };

    for _ in 0..10 {
        fill(&mut chunk, &mut frame);
        sink.push(&chunk);
        let _ = processor.try_process(&mut consumer, format, 0, 0);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..200 {
            fill(&mut chunk, &mut frame);
            sink.push(&chunk);
            let snapshot = processor.try_process(&mut consumer, format, 0, 0);
            assert!(snapshot.is_some());
        }
    });

    assert!(
        stray_count == 0,
        "full hop path allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

#[test]
fn beat_tracker_induction_does_not_allocate() {
    // The beat tracker's periodic autocorrelation/comb induction pass — which
    // fires from inside `try_process` roughly once a second once its ODF ring
    // has filled — must be allocation-free too. Warm past the first induction so
    // the ring is full and locked, then measure a span long enough to cross
    // several more induction boundaries, driving a click train so the ODF has
    // real structure and the tracker actually runs its full induction.
    let format = StreamFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    let hop = 256usize;
    let channels = 2usize;
    let sr = 48_000u64;
    let period = sr / 5; // a click every 200 ms (150 bpm ≈ 320 ms; 200 ms is fine)

    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(hop, 2, 48_000);
    let mut chunk = vec![0.0f32; hop * channels];
    let mut frame: u64 = 0;
    let fill = |chunk: &mut [f32], frame: &mut u64| {
        for f in 0..hop {
            let s = if (*frame + f as u64) % period == 0 {
                0.8
            } else {
                0.0
            };
            chunk[f * channels] = s;
            chunk[f * channels + 1] = s;
        }
        *frame += hop as u64;
    };

    // Warm-up: ~5 s of hops so the ~6 s ring fills and the first (allocating in
    // `new`, but never here) induction passes have all run.
    for _ in 0..950 {
        fill(&mut chunk, &mut frame);
        sink.push(&chunk);
        let _ = processor.try_process(&mut consumer, format, 0, 0);
    }

    // Measure ~4 s: crosses at least three induction boundaries.
    let ((), stray_count, strays) = watch(|| {
        for _ in 0..750 {
            fill(&mut chunk, &mut frame);
            sink.push(&chunk);
            let snapshot = processor.try_process(&mut consumer, format, 0, 0);
            assert!(snapshot.is_some());
        }
    });

    assert!(
        stray_count == 0,
        "beat induction path allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

#[test]
fn tee_push_path_does_not_allocate() {
    // With the P7 dual-tap tee installed, the capture push writes the packet into
    // the tee's second ring and logs one per-push record — that extra work must
    // also stay allocation-free after warm-up (a memcpy plus atomics). Drain the
    // tee each iteration into pre-sized buffers so the tee ring never fills and the
    // real write+log path (not the drop path) is what is measured.
    let hop = 256usize;
    let channels = 2usize;

    let (mut sink, _primary, mut tee) = sample_ring_with_tee(Instant::now());
    sink.stats().set_channels(2);
    let chunk = vec![0.3f32; hop * channels];

    // Pre-size every drain buffer so the reconstruction never grows one.
    let mut scratch: Vec<f32> = Vec::with_capacity(hop * channels * 4);
    let mut mono: Vec<f32> = Vec::with_capacity(hop * 300);
    let mut timeline = DrainTimeline::new(48_000);
    timeline.reserve(300);

    // Warm up: prime the tee ring, the drain buffers, and the segment list.
    for _ in 0..5 {
        sink.push(&chunk);
        tee_drain_into_timeline(&mut tee, &mut scratch, &mut mono, &mut timeline, channels);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..200 {
            sink.push(&chunk);
            tee_drain_into_timeline(&mut tee, &mut scratch, &mut mono, &mut timeline, channels);
        }
    });

    assert!(
        stray_count == 0,
        "tee push+drain path allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

#[test]
fn spectrum_analyzer_hot_path_does_not_allocate() {
    let sr = 48_000u32;
    let hop = 256usize;
    let dt = hop as f32 / sr as f32;
    let mut analyzer = SpectrumAnalyzer::new(SpectrumConfig::default(), sr);
    let mut out = vec![0.0f32; analyzer.bars()];
    let mut mono = vec![0.0f32; hop];
    for (k, m) in mono.iter_mut().enumerate() {
        *m = (k as f32 * 0.01).sin() * 0.5;
    }

    // Warm up the FFT plans and the smoothing state.
    for _ in 0..5 {
        analyzer.process_hop(&mono, dt, &mut out);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..200 {
            analyzer.process_hop(&mono, dt, &mut out);
        }
    });

    assert!(
        stray_count == 0,
        "spectrum analyzer allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

#[test]
fn converter_downmix_does_not_allocate() {
    // The capture callback converts and folds device samples into the ring with
    // a preallocated output buffer; that path must not allocate. Drive the
    // 6-channel fold (the branch with the most work) and, separately, the i16
    // scalar converter through `mix_frames` with a fixed `out` buffer.
    let frames = 512usize;
    let dm6 = Downmix::new(6);
    let input6 = vec![0.25f32; frames * 6];
    let mut out = vec![0.0f32; frames * 2];

    let dm2 = Downmix::new(2);
    let input_i16 = vec![12_345i16; frames * 2];

    // Warm up (nothing to warm here, but mirror the other cases).
    for _ in 0..5 {
        dm6.mix_frames(&input6, f32_id, &mut out);
        dm2.mix_frames(&input_i16, i16_to_f32, &mut out);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..200 {
            let n6 = dm6.mix_frames(&input6, f32_id, &mut out);
            assert_eq!(n6, frames * 2);
            let n2 = dm2.mix_frames(&input_i16, i16_to_f32, &mut out);
            assert_eq!(n2, frames * 2);
        }
    });

    assert!(
        stray_count == 0,
        "converter/downmix allocated {} time(s):\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}
