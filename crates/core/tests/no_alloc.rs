//! The per-hop processing path must not allocate after construction. A
//! counting global allocator wraps the system allocator and records every
//! allocation; the hot loop must not move the counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use scia_core::backends::convert::{Downmix, f32_id, i16_to_f32};
use scia_core::spectrum::{SpectrumAnalyzer, SpectrumConfig};
use scia_core::{HopProcessor, StreamFormat, sample_ring};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: this is a test-only allocator that only counts allocations and
// forwards every call unchanged to the system allocator. The library crate
// itself is `#![forbid(unsafe_code)]`; integration tests are separate crates.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

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

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..200 {
        sink.push(&chunk);
        let snapshot = processor.try_process(&mut consumer, format, 0, 0);
        assert!(snapshot.is_some());
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after,
        before,
        "hot path allocated {} time(s)",
        after - before
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

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..200 {
        sink.push(&silence);
        let snapshot = processor.process_idle(&mut consumer, format, 0, 0, resume_rms);
        assert!(snapshot.is_some());
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after,
        before,
        "idle path allocated {} time(s)",
        after - before
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

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..200 {
        fill(&mut chunk, &mut frame);
        sink.push(&chunk);
        let snapshot = processor.try_process(&mut consumer, format, 0, 0);
        assert!(snapshot.is_some());
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after,
        before,
        "full hop path allocated {} time(s)",
        after - before
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

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..200 {
        analyzer.process_hop(&mono, dt, &mut out);
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after,
        before,
        "spectrum analyzer allocated {} time(s)",
        after - before
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

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..200 {
        let n6 = dm6.mix_frames(&input6, f32_id, &mut out);
        assert_eq!(n6, frames * 2);
        let n2 = dm2.mix_frames(&input_i16, i16_to_f32, &mut out);
        assert_eq!(n2, frames * 2);
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after,
        before,
        "converter/downmix allocated {} time(s)",
        after - before
    );
}
