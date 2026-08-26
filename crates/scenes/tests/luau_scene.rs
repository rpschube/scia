//! A Luau scene produces a deterministic display list from a fixed feature
//! snapshot (golden), and the two shipped scenes construct and render cleanly.

use scia_core::{FEATURE_SCHEMA_VERSION, FeatureSnapshot};
use scia_scenes::{
    Canvas, LuauLimits, LuauScene, Primitive, Scene, SceneCtx, catalog_scenes, create_scene,
    shipped_scenes,
};

/// A minimal scene that draws one bar per valid spectrum band: x steps across
/// the canvas, height is the band level. Deterministic in the snapshot.
const BARS_SCENE: &str = r#"
local vals = {}
local count = 0
return {
  id = "golden",
  mood = "test",
  summary = "one bar per spectrum band",
  update = function(features, dt)
    count = features.bar_count
    for i = 1, count do
      vals[i] = features:bar(i)
    end
  end,
  render = function(canvas)
    for i = 1, count do
      local x = (i - 1) / count
      local h = vals[i]
      canvas:bar(x, 1.0 - h, 1.0 / count, h, (i - 1) % 8, 1.0)
    end
  end,
}
"#;

fn fixed_snapshot() -> FeatureSnapshot {
    let mut s = FeatureSnapshot {
        schema_version: FEATURE_SCHEMA_VERSION,
        sample_rate: 48_000,
        channels: 2,
        spectrum_len: 3,
        ..FeatureSnapshot::default()
    };
    s.spectrum[0] = 0.25;
    s.spectrum[1] = 0.5;
    s.spectrum[2] = 0.75;
    s
}

#[test]
fn golden_display_list_from_a_fixed_snapshot() {
    let mut scene =
        LuauScene::from_source(BARS_SCENE, "golden", LuauLimits::default()).expect("compiles");
    scene.init(&SceneCtx::default());

    let snap = fixed_snapshot();
    scene.update(&snap, 1.0 / 60.0);
    let mut canvas = Canvas::new(1.0);
    scene.render(&mut canvas);

    let prims = canvas.primitives();
    assert_eq!(prims.len(), 3, "one bar per valid band");

    let expected = [
        (0.0f32, 0.25f32, 0u8),
        (1.0 / 3.0, 0.5, 1),
        (2.0 / 3.0, 0.75, 2),
    ];
    for (prim, (ex_x, ex_h, ex_slot)) in prims.iter().zip(expected) {
        let Primitive::Bar { x, y, w, h, style } = prim else {
            panic!("every primitive is a bar, got {prim:?}");
        };
        assert!((x - ex_x).abs() < 1e-4, "x: {x} != {ex_x}");
        assert!((h - ex_h).abs() < 1e-4, "h: {h} != {ex_h}");
        assert!((w - 1.0 / 3.0).abs() < 1e-4, "w: {w}");
        assert!((y - (1.0 - ex_h)).abs() < 1e-4, "y: {y}");
        assert_eq!(style.slot, ex_slot, "palette slot walks the bands");
    }
}

#[test]
fn golden_render_is_repeatable() {
    // The same snapshot twice yields byte-identical display lists.
    let mut scene =
        LuauScene::from_source(BARS_SCENE, "golden", LuauLimits::default()).expect("compiles");
    scene.init(&SceneCtx::default());
    let snap = fixed_snapshot();

    let mut a = Canvas::new(1.0);
    scene.update(&snap, 1.0 / 60.0);
    scene.render(&mut a);

    let mut b = Canvas::new(1.0);
    scene.update(&snap, 1.0 / 60.0);
    scene.render(&mut b);

    assert_eq!(a.primitives(), b.primitives(), "render is deterministic");
}

/// Exposes `features.loud` and `features.rms` as two bar heights so a test can
/// read exactly which snapshot field each name resolves to.
const LOUD_VS_RMS_SCENE: &str = r#"
local loud = 0
local rms = 0
return {
  id = "loudprobe",
  mood = "test",
  summary = "loud and rms as bar heights",
  update = function(features, dt)
    loud = features.loud
    rms = features.rms
  end,
  render = function(canvas)
    canvas:bar(0.0, 1.0 - loud, 0.5, loud, 0, 1.0)
    canvas:bar(0.5, 1.0 - rms, 0.5, rms, 1, 1.0)
  end,
}
"#;

#[test]
fn luau_loud_reads_normalized_loudness_not_raw_rms() {
    // A Luau scene's `features.loud` must resolve to the engine-normalized
    // `loudness`, while `features.rms` stays the raw signal. Drive a snapshot
    // where the two differ so the test discriminates.
    let mut scene = LuauScene::from_source(LOUD_VS_RMS_SCENE, "loudprobe", LuauLimits::default())
        .expect("compiles");
    scene.init(&SceneCtx::default());

    let snap = FeatureSnapshot {
        rms: 0.08,
        loudness: 0.7,
        ..FeatureSnapshot::default()
    };
    scene.update(&snap, 1.0 / 60.0);
    let mut canvas = Canvas::new(1.0);
    scene.render(&mut canvas);

    let prims = canvas.primitives();
    assert_eq!(prims.len(), 2, "one bar for loud, one for rms");
    let heights: Vec<f32> = prims
        .iter()
        .map(|p| {
            let Primitive::Bar { h, .. } = p else {
                panic!("every primitive is a bar, got {p:?}");
            };
            *h
        })
        .collect();
    assert!(
        (heights[0] - 0.7).abs() < 1e-4,
        "`loud` reflects loudness (0.7), got {}",
        heights[0]
    );
    assert!(
        (heights[1] - 0.08).abs() < 1e-4,
        "`rms` stays the raw signal (0.08), got {}",
        heights[1]
    );
}

#[test]
fn shipped_scenes_construct_and_render() {
    // A representative "loud, mid-onset" snapshot.
    let mut snap = FeatureSnapshot {
        schema_version: FEATURE_SCHEMA_VERSION,
        sample_rate: 48_000,
        channels: 2,
        spectrum_len: 32,
        rms: 0.6,
        peak: 0.8,
        onset: true,
        ..FeatureSnapshot::default()
    };
    for (i, bin) in snap.spectrum.iter_mut().take(32).enumerate() {
        *bin = 0.5 + 0.5 * (i as f32 * 0.2).sin();
    }
    snap.bands = [1.4, 0.9, 0.6];

    for (name, source) in shipped_scenes() {
        let mut scene = LuauScene::from_source(source, name, LuauLimits::default())
            .unwrap_or_else(|e| panic!("shipped scene `{name}` compiles: {e}"));
        scene.init(&SceneCtx::default());
        // Drive several frames; a shipped scene must never fault.
        let mut canvas = Canvas::new(1.6);
        for _ in 0..30 {
            scene.update(&snap, 1.0 / 60.0);
            scene.render(&mut canvas);
        }
        assert!(
            !scene.is_errored(),
            "shipped scene `{name}` faulted: {:?}",
            scene.last_error()
        );
        assert!(
            !canvas.primitives().is_empty(),
            "shipped scene `{name}` drew something"
        );
    }
}

#[test]
fn shipped_scene_state_round_trips() {
    // `ripple` carries continuity across a hot reload via state()/restore().
    let (_, ripple_src) = shipped_scenes()
        .iter()
        .find(|(n, _)| *n == "ripple")
        .expect("ripple is shipped");
    let mut a =
        LuauScene::from_source(ripple_src, "ripple", LuauLimits::default()).expect("compiles");
    a.init(&SceneCtx::default());
    let snap = FeatureSnapshot {
        rms: 0.7,
        onset: true,
        ..FeatureSnapshot::default()
    };
    for _ in 0..10 {
        a.update(&snap, 1.0 / 60.0);
    }
    let carried = a.state();
    assert!(
        carried.get("t").is_some(),
        "ripple carries its clock across a reload"
    );

    // A fresh instance restores the carried state without faulting.
    let mut b =
        LuauScene::from_source(ripple_src, "ripple", LuauLimits::default()).expect("compiles");
    b.init(&SceneCtx::default());
    b.restore(carried);
    assert!(
        !b.is_errored(),
        "restore does not fault: {:?}",
        b.last_error()
    );
}

#[test]
fn create_scene_builds_a_shipped_luau_scene() {
    // The catalog can construct a shipped Luau scene by id, alongside built-ins.
    let ids: Vec<&str> = catalog_scenes().iter().map(|i| i.id).collect();
    assert!(ids.contains(&"ripple"), "ripple is catalogued: {ids:?}");
    let scene = create_scene("ripple").expect("ripple constructs via the catalog");
    assert_eq!(scene.id(), "ripple");
}
