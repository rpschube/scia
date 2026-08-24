//! Once warmed up, driving the `aurora` scene must not allocate: the field
//! buffer is sized at init and overwritten in place, and the canvas field arena
//! retains its capacity. A counting global allocator wraps the system allocator;
//! the measured update+render loop must not move the counter. Same shape as
//! `crates/scenes/tests/no_alloc.rs`.

mod support {
    pub mod alloc_watch;
}

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, SceneCtx, create_builtin};
use support::alloc_watch::{CountingAllocator, watch};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn aurora_warm_frames_do_not_allocate() {
    let mut scene = create_builtin("aurora").expect("aurora exists");
    scene.init(&SceneCtx::default());
    let mut canvas = Canvas::new(16.0 / 9.0);
    let f = FeatureSnapshot {
        rms: 0.5,
        ..FeatureSnapshot::default()
    };

    // Warm up: grow the field buffer and the canvas arena to steady state.
    for _ in 0..5 {
        scene.update(&f, 0.016);
        canvas.clear();
        scene.render(&mut canvas);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..100 {
            scene.update(&f, 0.016);
            canvas.clear();
            scene.render(&mut canvas);
        }
    });

    assert!(
        stray_count == 0,
        "aurora allocated {} time(s) across 100 update/render cycles:\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}
