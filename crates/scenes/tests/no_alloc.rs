//! The canvas must not allocate once warmed up, and neither must a mapping set
//! once its target keys are seeded. A counting global allocator wraps the
//! system allocator; the measured paths must not move the counter. Same shape
//! as `crates/core/tests/no_alloc.rs`.

mod support {
    pub mod alloc_watch;
}

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, Curve, Feature, Mapping, MappingSet, Params, Style};
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

#[test]
fn mapping_apply_does_not_allocate() {
    // A mixed set of mappings covering every curve and both envelope directions.
    let mappings = [
        Mapping {
            target: "punch".to_string(),
            feature: Feature::Onset,
            curve: Curve::Linear,
            attack_ms: 0.0,
            decay_ms: 250.0,
            scale: 0.9,
            offset: 0.0,
        },
        Mapping {
            target: "release".to_string(),
            feature: Feature::Bass,
            curve: Curve::Pow { exponent: 2.0 },
            attack_ms: 10.0,
            decay_ms: 120.0,
            scale: 0.3,
            offset: 0.05,
        },
        Mapping {
            target: "gap".to_string(),
            feature: Feature::Loud,
            curve: Curve::Log,
            attack_ms: 5.0,
            decay_ms: 40.0,
            scale: 0.2,
            offset: 0.0,
        },
        Mapping {
            target: "punch_decay".to_string(),
            feature: Feature::Peak,
            curve: Curve::Step { threshold: 0.4 },
            attack_ms: 0.0,
            decay_ms: 0.0,
            scale: 1.0,
            offset: 0.0,
        },
    ];
    let mut set = MappingSet::new(&mappings);
    let mut params = Params::new();
    set.seed(&mut params);

    let mut onset = FeatureSnapshot {
        onset: true,
        rms: 0.5,
        peak: 0.7,
        ..FeatureSnapshot::default()
    };
    onset.bands = [0.6, 0.4, 0.2];

    // Warm up: run both branches so any lazy state is realized.
    for i in 0..8 {
        onset.onset = i % 2 == 0;
        set.apply(&onset, 0.016, &mut params);
    }

    let ((), stray_count, strays) = watch(|| {
        for i in 0..1000 {
            onset.onset = i % 2 == 0;
            set.apply(&onset, 0.016, &mut params);
        }
    });

    assert!(
        stray_count == 0,
        "MappingSet::apply allocated {} time(s) across 1000 calls:\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}
