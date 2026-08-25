//! Logging that is *off* must cost the hot path nothing. With no `tracing`
//! subscriber installed (the default), a `tracing` event on a tight loop must
//! not allocate: the callsite's static max-level check short-circuits before any
//! field is built. This guards the structured-logging promise that the DSP and render
//! threads pay nothing for logging that is disabled — the same property the DSP
//! loop's `note_activity` transition trace relies on.
//!
//! Only the watching thread's allocations are counted (libtest keeps its own
//! bookkeeping on the main thread), matching the repo's other no-alloc tests.

mod support {
    pub mod alloc_watch;
}

use support::alloc_watch::{CountingAllocator, watch};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn disabled_logging_does_not_allocate_on_the_hot_path() {
    // No `tracing` subscriber is installed in this test binary, so every event
    // is disabled. The two events live inside one closure so the warmup below
    // hits the *same* callsites as the measured loop — a callsite registers
    // (and may allocate) only on its first touch, which the warmup absorbs.
    let mut counter: u64 = 0;
    let mut body = || {
        counter = counter.wrapping_add(1);
        tracing::debug!(
            target: "scia::dsp",
            from = "Active",
            to = "Quiet",
            generation = counter,
            "activity transition"
        );
        tracing::info!(target: "scia::engine", generation = counter, "hop");
    };

    // Warm up: register both callsites outside the measured window.
    for _ in 0..8 {
        body();
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..5_000 {
            body();
        }
    });

    assert!(
        stray_count == 0,
        "disabled logging allocated {stray_count} time(s) on the hot path:\n{}",
        strays.join("\n---\n")
    );
}
