//! The built-in `verso` scene: the track title as the analyzer. Registry and
//! manifest, letter rebuild on `apply_text`, one text primitive per non-space
//! letter, the fallback word, letters riding their bands, the dotted trail, and
//! continuity across a hot reload. Also covers the `apply_text` trait hook: its
//! default no-op on a scene that does not render text.

use scia_core::{FeatureSnapshot, SPECTRUM_BINS};
use scia_scenes::{Canvas, Primitive, Scene, SceneCtx, create_builtin, scene_info};

fn verso() -> Box<dyn Scene> {
    create_builtin("verso").expect("verso exists")
}

/// A snapshot whose whole spectrum sits at `level`, so every letter's band reads
/// the same strong (or weak) value.
fn snap(level: f32) -> FeatureSnapshot {
    let mut f = FeatureSnapshot {
        spectrum_len: SPECTRUM_BINS as u16,
        ..FeatureSnapshot::default()
    };
    for b in &mut f.spectrum {
        *b = level;
    }
    f
}

/// Render one frame's primitives.
fn render_prims(scene: &mut dyn Scene) -> Vec<Primitive> {
    let mut c = Canvas::new(16.0 / 9.0);
    scene.render(&mut c);
    c.primitives().to_vec()
}

/// The text runs of one frame, as `(x, y, string)`.
fn text_runs(scene: &mut dyn Scene) -> Vec<(f32, f32, String)> {
    let mut c = Canvas::new(16.0 / 9.0);
    scene.render(&mut c);
    c.primitives()
        .iter()
        .filter_map(|p| {
            if let Primitive::Text { x, y, .. } = p {
                c.text_of(p).map(|s| (*x, *y, s.to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn count_points(prims: &[Primitive]) -> usize {
    prims
        .iter()
        .filter(|p| matches!(p, Primitive::Point { .. }))
        .count()
}

#[test]
fn verso_is_registered_with_its_manifest() {
    let info = scene_info("verso").expect("verso is listed");
    assert_eq!(info.mood, "literal", "mood matches");
    assert!(!info.summary.is_empty(), "summary is set");

    let scene = verso();
    assert_eq!(scene.id(), "verso");
    assert_eq!(scene.mood(), info.mood, "scene mood matches the registry");

    let keys: Vec<&str> = info.params.iter().map(|s| s.key).collect();
    for key in ["baseline", "lift", "trail", "fall"] {
        assert!(keys.contains(&key), "manifest exposes `{key}`");
    }
    for spec in info.params {
        assert!(spec.min <= spec.max, "`{}` has min <= max", spec.key);
        assert!(
            (spec.min..=spec.max).contains(&spec.default),
            "`{}` default is within range",
            spec.key
        );
    }
}

#[test]
fn verso_falls_back_to_scia() {
    let mut s = verso();
    s.init(&SceneCtx::default());
    // Fresh: the fallback word, one text primitive per letter.
    let runs = text_runs(s.as_mut());
    let word: String = runs.iter().map(|(_, _, c)| c.as_str()).collect();
    assert_eq!(word, "scia", "the fallback word is `scia`");

    // An empty track line falls back too.
    s.apply_text("track", "   ");
    let runs = text_runs(s.as_mut());
    let word: String = runs.iter().map(|(_, _, c)| c.as_str()).collect();
    assert_eq!(word, "scia", "a blank track line falls back to `scia`");
}

#[test]
fn verso_reassigns_letters_on_apply_text() {
    let mut s = verso();
    s.init(&SceneCtx::default());
    assert_eq!(
        text_runs(s.as_mut()).len(),
        4,
        "fallback `scia` is 4 letters"
    );

    // A new track line rebuilds the letters: one text primitive per NON-space
    // letter, in order, and spaces draw nothing.
    s.apply_text("track", "hi there");
    let runs = text_runs(s.as_mut());
    let word: String = runs.iter().map(|(_, _, c)| c.as_str()).collect();
    assert_eq!(word, "hithere", "spaces are skipped, letters kept in order");
    assert_eq!(
        runs.len(),
        "hithere".chars().count(),
        "one text primitive per non-space letter"
    );

    // Each letter sits at a distinct x, laid out left to right.
    for pair in runs.windows(2) {
        assert!(
            pair[1].0 > pair[0].0,
            "letters are laid out left to right: {} then {}",
            pair[0].0,
            pair[1].0
        );
    }

    // An unknown text key is ignored — the letters do not change.
    s.apply_text("subtitle", "ignored");
    let word2: String = text_runs(s.as_mut())
        .iter()
        .map(|(_, _, c)| c.as_str())
        .collect();
    assert_eq!(word2, "hithere", "an unknown text key is ignored");
}

#[test]
fn verso_letters_ride_their_band() {
    // A letter floats up (smaller y) as its band swells. Drive two scenes, one on
    // a loud spectrum and one on silence, then compare the same letter's height.
    let dt = 0.05;

    let mut loud = verso();
    loud.init(&SceneCtx::default());
    for _ in 0..40 {
        loud.update(&snap(0.9), dt);
    }
    let loud_runs = text_runs(loud.as_mut());

    let mut quiet = verso();
    quiet.init(&SceneCtx::default());
    for _ in 0..40 {
        quiet.update(&snap(0.0), dt);
    }
    let quiet_runs = text_runs(quiet.as_mut());

    assert_eq!(loud_runs.len(), quiet_runs.len());
    // Every letter on the loud spectrum rides higher (smaller y) than on silence.
    for (l, q) in loud_runs.iter().zip(quiet_runs.iter()) {
        assert!(
            l.1 < q.1,
            "a swelling band lifts the letter `{}`: loud y {} < quiet y {}",
            l.2,
            l.1,
            q.1
        );
    }
}

#[test]
fn verso_sheds_a_dotted_trail() {
    // With signal the letters shed falling trail dots; in silence, none.
    let dt = 0.05;

    let mut s = verso();
    s.init(&SceneCtx::default());
    for _ in 0..40 {
        s.update(&snap(0.9), dt);
    }
    let dots = count_points(&render_prims(s.as_mut()));
    assert!(dots > 0, "a live band sheds trail dots, got {dots}");

    // Let the trail fall away over a long silence: no signal, no new dots, and
    // the old ones expire.
    for _ in 0..200 {
        s.update(&snap(0.0), dt);
    }
    let dots = count_points(&render_prims(s.as_mut()));
    assert_eq!(dots, 0, "the trail clears in silence, got {dots}");
}

#[test]
fn verso_trail_disabled_sheds_nothing() {
    // `trail = 0` disables the trail entirely.
    let mut s = verso();
    let mut ctx = SceneCtx::default();
    ctx.params.set("trail", 0.0);
    s.init(&ctx);
    for _ in 0..40 {
        s.update(&snap(0.9), 0.05);
    }
    assert_eq!(
        count_points(&render_prims(s.as_mut())),
        0,
        "trail = 0 sheds no dots"
    );
}

#[test]
fn verso_state_round_trip() {
    let dt = 0.05;
    let s1 = snap(0.8); // build up letter values and a trail
    let s2 = snap(0.3); // the frame both scenes then advance

    let mut a = verso();
    a.init(&SceneCtx::default());
    for _ in 0..30 {
        a.update(&s1, dt);
    }
    let state = a.state();
    assert!(
        state.get("tn").is_some(),
        "state carries the live trail-mark count"
    );
    a.update(&s2, dt);
    let runs_a = text_runs(a.as_mut());

    // Restored: a fresh scene (same fallback letters) restores the state and
    // advances the same frame — the letter positions must match.
    let mut b = verso();
    b.init(&SceneCtx::default());
    b.restore(state);
    b.update(&s2, dt);
    let runs_b = text_runs(b.as_mut());

    assert_eq!(runs_a.len(), runs_b.len());
    for (ra, rb) in runs_a.iter().zip(runs_b.iter()) {
        assert!(
            (ra.1 - rb.1).abs() < 1e-6,
            "restored letter `{}` height matches: {} vs {}",
            ra.2,
            ra.1,
            rb.1
        );
    }

    // Control: without the restore the smoothed values start cold, so the letters
    // sit lower (nearer the baseline) after one frame.
    let mut c = verso();
    c.init(&SceneCtx::default());
    c.update(&s2, dt);
    let runs_c = text_runs(c.as_mut());
    assert!(
        runs_a[0].1 < runs_c[0].1 - 1e-4,
        "a scene that skipped restore should not match the warmed height"
    );
}

#[test]
fn apply_text_default_is_a_noop() {
    // The `apply_text` trait hook defaults to a no-op: a scene that does not
    // render text (aurora) is unaffected by it — same render before and after.
    let mut s = create_builtin("aurora").expect("aurora exists");
    s.init(&SceneCtx::default());
    s.update(&FeatureSnapshot::default(), 0.05);

    let mut before = Canvas::new(1.0);
    s.render(&mut before);
    let a = before.field_data().to_vec();

    s.apply_text("track", "does not matter");

    let mut after = Canvas::new(1.0);
    s.render(&mut after);
    let b = after.field_data().to_vec();

    assert_eq!(a, b, "apply_text is a no-op on a scene that ignores it");
}
