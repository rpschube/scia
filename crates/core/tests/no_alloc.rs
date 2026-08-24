//! The per-hop processing path must not allocate after construction. A
//! counting global allocator wraps the system allocator and records every
//! allocation; the hot loop must not move the counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

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
    let mut processor = HopProcessor::new(hop, 2);
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
