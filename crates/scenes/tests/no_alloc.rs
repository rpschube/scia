//! The canvas must not allocate once warmed up. A counting global allocator
//! wraps the system allocator; filling and clearing a warmed canvas must not
//! move the counter. Same shape as `crates/core/tests/no_alloc.rs`.

mod support {
    pub mod alloc_watch;
}

use scia_scenes::{Canvas, Style};
use support::alloc_watch::{CountingAllocator, watch};

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

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..100 {
            canvas.clear();
            fill(&mut canvas);
        }
    });

    assert!(
        stray_count == 0,
        "canvas allocated {} time(s) across 100 clear/refill cycles:\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}
