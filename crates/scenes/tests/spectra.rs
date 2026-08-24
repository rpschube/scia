//! The built-in `spectra` scene: geometry, the onset punch and continuity.

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, Primitive, Scene, SceneCtx, create_builtin};

/// Build a snapshot with the given display spectrum and onset flag.
fn snap(values: &[f32], onset: bool) -> FeatureSnapshot {
    let mut f = FeatureSnapshot {
        spectrum_len: values.len() as u16,
        onset,
        ..FeatureSnapshot::default()
    };
    for (i, v) in values.iter().enumerate() {
        f.spectrum[i] = *v;
    }
    f
}

fn spectra() -> Box<dyn Scene> {
    create_builtin("spectra").expect("spectra exists")
}

/// The `(x, w, h)` of a `Bar` primitive.
fn bar_xwh(p: &Primitive) -> (f32, f32, f32) {
    match p {
        Primitive::Bar { x, w, h, .. } => (*x, *w, *h),
        other => panic!("expected Bar, got {other:?}"),
    }
}

fn render_to_vec(scene: &mut dyn Scene) -> Vec<Primitive> {
    let mut c = Canvas::new(1.0);
    scene.render(&mut c);
    c.primitives().to_vec()
}

#[test]
fn spectra_renders_one_bar_per_bin() {
    let values: Vec<f32> = (0..16).map(|i| 0.1 + i as f32 * 0.04).collect();
    let mut s = spectra();
    s.init(&SceneCtx::default());
    s.update(&snap(&values, false), 1.0);
    let prims = render_to_vec(s.as_mut());

    assert_eq!(prims.len(), 16, "one bar per spectrum bin");

    // Heights match the input (smoothing converges in one frame; no punch).
    for (i, p) in prims.iter().enumerate() {
        let (_, _, h) = bar_xwh(p);
        assert!(
            (h - values[i]).abs() < 1e-6,
            "bar {i} height {h} != value {}",
            values[i]
        );
    }

    // Bars are laid left to right without overlap.
    for i in 0..prims.len() - 1 {
        let (x, w, _) = bar_xwh(&prims[i]);
        let (xn, _, _) = bar_xwh(&prims[i + 1]);
        assert!(w > 0.0, "bar {i} has positive width");
        assert!(
            x + w <= xn + 1e-6,
            "bar {i} right edge {} overlaps bar {} left edge {xn}",
            x + w,
            i + 1
        );
    }

    // And they span essentially the whole width.
    let (x0, _, _) = bar_xwh(&prims[0]);
    let (xl, wl, _) = bar_xwh(&prims[prims.len() - 1]);
    assert!(x0 < 0.05, "first bar starts near the left edge");
    assert!(xl + wl > 0.95, "last bar reaches near the right edge");
}

#[test]
fn spectra_punches_on_onset() {
    let values = [0.5f32; 16];

    let mut with = spectra();
    with.init(&SceneCtx::default());
    with.update(&snap(&values, true), 1.0);
    let a = render_to_vec(with.as_mut());

    let mut without = spectra();
    without.init(&SceneCtx::default());
    without.update(&snap(&values, false), 1.0);
    let b = render_to_vec(without.as_mut());

    // First quarter (indices 0..4 for 16 bars) rides the punch; the rest do not.
    for i in 0..16 {
        let (_, _, ha) = bar_xwh(&a[i]);
        let (_, _, hb) = bar_xwh(&b[i]);
        if i < 4 {
            assert!(
                ha > hb + 1e-4,
                "low bar {i}: onset height {ha} should exceed non-onset {hb}"
            );
        } else {
            assert!(
                (ha - hb).abs() < 1e-6,
                "high bar {i}: onset height {ha} should equal non-onset {hb}"
            );
        }
    }
}

#[test]
fn spectra_state_round_trip() {
    let values = [0.5f32; 16];
    let s1 = snap(&values, true); // onset: drives the envelope to full
    let s2 = snap(&values, false); // next frame: same spectrum, no onset

    // Reference scene: build the envelope, snapshot it, then advance one frame.
    let mut a = spectra();
    a.init(&SceneCtx::default());
    a.update(&s1, 0.05);
    let state = a.state();
    a.update(&s2, 0.05);
    let prims_a = render_to_vec(a.as_mut());

    // Restored scene: fresh, restore the envelope, advance the same frame.
    let mut b = spectra();
    b.init(&SceneCtx::default());
    b.restore(state);
    b.update(&s2, 0.05);
    let prims_b = render_to_vec(b.as_mut());

    assert_eq!(
        prims_a, prims_b,
        "restore reproduces the next render exactly"
    );

    // Control: without the restore the envelope is lost, so the low bars differ.
    let mut c = spectra();
    c.init(&SceneCtx::default());
    c.update(&s2, 0.05);
    let prims_c = render_to_vec(c.as_mut());
    assert_ne!(
        prims_a, prims_c,
        "a scene that skipped restore should not match (envelope was carried)"
    );
}

#[test]
fn spectra_empty_spectrum_renders_nothing() {
    let mut s = spectra();
    s.init(&SceneCtx::default());
    s.update(&snap(&[], false), 1.0);
    let prims = render_to_vec(s.as_mut());
    assert!(prims.is_empty(), "no bars when spectrum_len == 0");
}
