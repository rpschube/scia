//! The canvas must not allocate once warmed up. A counting global allocator
//! wraps the system allocator; filling and clearing a warmed canvas must not
//! move the counter. Same shape as `crates/core/tests/no_alloc.rs`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use scia_scenes::{Canvas, Style};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: a test-only allocator that only counts allocations and forwards every
// call unchanged to the system allocator. The library crate itself is
// `#![forbid(unsafe_code)]`; integration tests are separate crates.
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

/// Push a fixed budget of 500 mixed primitives with identical backing data
/// every call, so a warmed canvas reuses its capacity.
fn fill(c: &mut Canvas) {
    const TEXTS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
    let field = [0.1f32, 0.2, 0.3, 0.4];
    for i in 0..100 {
        let t = (i as f32) / 100.0;
        let style = Style::new((i % 8) as u8, t);
        c.bar(t, 0.1, 0.05, 0.5, style);
        c.line(0.0, t, 1.0, t, 0.02, style);
        c.point(t, 0.5, 0.03, style);
        c.field(2, 2, &field, style);
        c.text(t, 0.9, TEXTS[i % TEXTS.len()], style);
    }
}

#[test]
fn canvas_retains_capacity() {
    let mut canvas = Canvas::new(1.6);

    // Warm up: grow every backing store to its steady-state capacity.
    for _ in 0..5 {
        canvas.clear();
        fill(&mut canvas);
    }
    assert_eq!(canvas.primitives().len(), 500);

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..100 {
        canvas.clear();
        fill(&mut canvas);
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after,
        before,
        "canvas allocated {} time(s) across 100 clear/refill cycles",
        after - before
    );
}
