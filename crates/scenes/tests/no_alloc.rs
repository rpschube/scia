//! The canvas must not allocate once warmed up, and neither must a mapping set
//! once its target keys are seeded. A counting global allocator wraps the
//! system allocator; the measured paths must not move the counter. Same shape
//! as `crates/core/tests/no_alloc.rs`.

mod support {
    pub mod alloc_watch;
}

use scia_core::FeatureSnapshot;
use scia_scenes::{
    Canvas, Curve, Feature, Mapping, MappingSet, Params, SceneCtx, Style, create_builtin,
    parse_preset,
};
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

/// A `[map]` **expression** entry must also be allocation-free per frame once
/// its target key is seeded: the program is compiled at load and the per-frame
/// namespace is a stack-local `Copy` record. Driven through the real preset
/// instantiate path so it exercises exactly what the host runs.
#[test]
fn expression_mapping_apply_does_not_allocate() {
    let preset = parse_preset(
        "[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\npunch = \"onset * 0.9 + loud * 0.1\"\n",
        None,
    )
    .expect("preset validates");
    let mut mappings = preset
        .instantiate(1.0)
        .into_iter()
        .next()
        .expect("one layer")
        .mappings;
    let mut params = Params::new();
    mappings.seed(&mut params);

    let mut f = FeatureSnapshot {
        rms: 0.5,
        onset: false,
        ..FeatureSnapshot::default()
    };

    // Warm up: run both onset branches so any lazy state is realized.
    for i in 0..8 {
        f.onset = i % 2 == 0;
        mappings.apply(&f, 0.016, &mut params);
    }

    let ((), stray_count, strays) = watch(|| {
        for i in 0..1000 {
            f.onset = i % 2 == 0;
            mappings.apply(&f, 0.016, &mut params);
        }
    });

    assert!(
        stray_count == 0,
        "expression MappingSet::apply allocated {} time(s) across 1000 calls:\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

/// The `lattice` scene must not allocate per frame once warmed: its dot grid
/// and ring pool are fixed at init, and the canvas retains capacity.
#[test]
fn lattice_update_render_does_not_allocate() {
    let mut scene = create_builtin("lattice").expect("lattice exists");
    scene.init(&SceneCtx::default());
    let mut canvas = Canvas::new(1.0);

    // A driving snapshot with signal, bass and a toggled onset.
    let mut f = FeatureSnapshot {
        rms: 0.5,
        onset: false,
        onset_age_ms: 100.0,
        ..FeatureSnapshot::default()
    };
    f.bands = [1.2, 1.0, 0.8];

    // Warm up: realize every ring slot and grow the canvas to steady capacity.
    for i in 0..16 {
        f.onset = i % 2 == 0;
        f.onset_age_ms = if f.onset { 0.0 } else { 50.0 };
        scene.update(&f, 0.016);
        canvas.clear();
        scene.render(&mut canvas);
    }

    let ((), stray_count, strays) = watch(|| {
        for i in 0..500 {
            f.onset = i % 7 == 0;
            f.onset_age_ms = if f.onset { 0.0 } else { 50.0 };
            scene.update(&f, 0.016);
            canvas.clear();
            scene.render(&mut canvas);
        }
    });

    assert!(
        stray_count == 0,
        "lattice update/render allocated {} time(s) across 500 frames:\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}

/// The `starfall` scene must not allocate per frame once warmed: its star pool
/// is fixed at init, respawns reuse the slot in place, and the canvas retains
/// capacity.
#[test]
fn starfall_update_render_does_not_allocate() {
    let mut scene = create_builtin("starfall").expect("starfall exists");
    scene.init(&SceneCtx::default());
    let mut canvas = Canvas::new(16.0 / 9.0);

    // A driving snapshot with loudness and a toggled onset, so both the point and
    // streak render paths run and stars keep respawning.
    let mut f = FeatureSnapshot {
        rms: 0.6,
        onset: false,
        onset_age_ms: 100.0,
        ..FeatureSnapshot::default()
    };

    // Warm up: grow the canvas to steady capacity and cycle stars through respawn.
    for i in 0..16 {
        f.onset = i % 2 == 0;
        f.onset_age_ms = if f.onset { 0.0 } else { 50.0 };
        scene.update(&f, 0.05);
        canvas.clear();
        scene.render(&mut canvas);
    }

    let ((), stray_count, strays) = watch(|| {
        for i in 0..500 {
            f.onset = i % 7 == 0;
            f.onset_age_ms = if f.onset { 0.0 } else { 50.0 };
            scene.update(&f, 0.05);
            canvas.clear();
            scene.render(&mut canvas);
        }
    });

    assert!(
        stray_count == 0,
        "starfall update/render allocated {} time(s) across 500 frames:\n{}",
        stray_count,
        strays.join("\n---\n")
    );
}
