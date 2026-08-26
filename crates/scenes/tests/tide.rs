//! The built-in `tide` scene: field validity, the loudness-driven brightness,
//! the front swell's response to the low band, drift continuity and continuity
//! across a hot reload.

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, Primitive, Scene, SceneCtx, create_builtin, scene_info};

/// The field's fixed internal resolution (mirrors the scene's own constants).
const COLS: usize = 96;
const ROWS: usize = 54;

fn tide() -> Box<dyn Scene> {
    create_builtin("tide").expect("tide exists")
}

/// A snapshot with a given loudness and low band; nothing else is set. The
/// first argument is the engine-normalized `loudness` the scene drives from
/// (mirrored into `rms` so the snapshot stays internally plausible).
fn snap(loudness: f32, bass: f32) -> FeatureSnapshot {
    let mut f = FeatureSnapshot {
        rms: loudness,
        loudness,
        ..FeatureSnapshot::default()
    };
    f.bands = [bass, 1.0, 1.0];
    f
}

/// Render one frame and return the sole field's `(cols, rows, values)`.
fn render_field(scene: &mut dyn Scene) -> (u16, u16, Vec<f32>) {
    let mut c = Canvas::new(16.0 / 9.0);
    scene.render(&mut c);
    let prims = c.primitives();
    assert_eq!(prims.len(), 1, "tide draws exactly one field");
    match prims[0] {
        Primitive::Field { cols, rows, .. } => {
            let data = c.field_of(&prims[0]).expect("field values").to_vec();
            (cols, rows, data)
        }
        ref other => panic!("expected a Field, got {other:?}"),
    }
}

/// Drive `scene` for `frames` steps at `dt` with a fixed snapshot, then render.
fn settle_and_render(
    scene: &mut dyn Scene,
    rms: f32,
    bass: f32,
    frames: usize,
    dt: f32,
) -> Vec<f32> {
    let f = snap(rms, bass);
    for _ in 0..frames {
        scene.update(&f, dt);
    }
    render_field(scene).2
}

/// The brightness-weighted mean row of the field. A ridge that moves up (toward
/// row 0) lowers this; brightening a lower ridge raises it.
fn weighted_mean_row(field: &[f32]) -> f32 {
    let mut num = 0.0f32;
    let mut den = 0.0f32;
    for r in 0..ROWS {
        let sum: f32 = field[r * COLS..(r + 1) * COLS].iter().sum();
        num += sum * r as f32;
        den += sum;
    }
    if den > 0.0 { num / den } else { 0.0 }
}

#[test]
fn tide_is_registered_with_its_manifest() {
    let info = scene_info("tide").expect("tide is listed");
    assert_eq!(info.mood, "fluid", "mood matches");
    assert!(!info.summary.is_empty(), "summary is set");

    let scene = tide();
    assert_eq!(scene.id(), "tide");
    assert_eq!(scene.mood(), info.mood, "scene mood matches the registry");

    // The manifest carries every documented tuning key with sane bounds.
    let keys: Vec<&str> = info.params.iter().map(|s| s.key).collect();
    for key in ["drift", "swell", "response", "level", "contrast"] {
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
fn tide_field_is_valid() {
    let mut s = tide();
    s.init(&SceneCtx::default());
    s.update(&snap(0.4, 1.0), 0.05);
    let (cols, rows, data) = render_field(s.as_mut());

    assert_eq!(cols as usize, COLS);
    assert_eq!(rows as usize, ROWS);
    assert_eq!(data.len(), COLS * ROWS, "field length is cols * rows");
    for (i, &v) in data.iter().enumerate() {
        assert!(
            (0.0..=1.0).contains(&v),
            "cell {i} value {v} is outside 0..=1"
        );
    }
    // The swells give the field real darks and brights, which the coarse tier
    // needs — it must not be uniform.
    let max = data.iter().copied().fold(0.0f32, f32::max);
    let min = data.iter().copied().fold(1.0f32, f32::min);
    assert!(
        max - min > 0.3,
        "field should span a wide intensity range, got {min}..{max}"
    );
}

#[test]
fn tide_front_swell_responds_to_the_low_band() {
    // Two scenes fed the SAME loudness and frame schedule, differing only in the
    // low band. The front swell must respond: the fields differ, and a strong
    // bass lifts the front (brightest, lowest) ridge upward, lowering the
    // brightness-weighted mean row.
    let frames = 200;
    let dt = 0.05;

    let mut weak = tide();
    weak.init(&SceneCtx::default());
    let weak_field = settle_and_render(weak.as_mut(), 0.5, 0.0, frames, dt);

    let mut strong = tide();
    strong.init(&SceneCtx::default());
    let strong_field = settle_and_render(strong.as_mut(), 0.5, 3.0, frames, dt);

    assert_ne!(
        weak_field, strong_field,
        "the front swell must respond to the low band"
    );

    let weak_row = weighted_mean_row(&weak_field);
    let strong_row = weighted_mean_row(&strong_field);
    assert!(
        strong_row < weak_row - 0.5,
        "a strong low band should lift the front swell upward: strong mean row \
         {strong_row} should sit above weak {weak_row}"
    );
}

#[test]
fn tide_brightness_breathes_with_loudness() {
    // Same low band and schedule for both, so only the loudness envelope — and
    // thus the overall brightness — differs.
    let frames = 240;
    let dt = 0.05;

    let mut quiet = tide();
    quiet.init(&SceneCtx::default());
    let quiet_field = settle_and_render(quiet.as_mut(), 0.0, 1.0, frames, dt);

    let mut loud = tide();
    loud.init(&SceneCtx::default());
    let loud_field = settle_and_render(loud.as_mut(), 0.85, 1.0, frames, dt);

    let quiet_sum: f32 = quiet_field.iter().sum();
    let loud_sum: f32 = loud_field.iter().sum();
    assert!(
        loud_sum > quiet_sum * 1.2,
        "loud field total {loud_sum} should clearly exceed quiet {quiet_sum}"
    );
}

#[test]
fn tide_eases_down_in_silence() {
    // A loud passage lights the field; sustained silence must ease it back down
    // (the aurora-style loudness handling), while the swells keep drifting.
    let dt = 0.05;
    let mut s = tide();
    s.init(&SceneCtx::default());

    let loud_field = settle_and_render(s.as_mut(), 0.8, 1.0, 200, dt);
    let quiet_field = settle_and_render(s.as_mut(), 0.0, 0.0, 400, dt);

    let loud_sum: f32 = loud_field.iter().sum();
    let quiet_sum: f32 = quiet_field.iter().sum();
    assert!(
        quiet_sum < loud_sum,
        "silence should ease the field down: quiet {quiet_sum} < loud {loud_sum}"
    );
    assert!(quiet_sum > 0.0, "the swells still drift, dimly, in silence");
}

#[test]
fn tide_drifts_smoothly() {
    // Consecutive frames differ — the swells drift — but every cell moves only a
    // little, so there is no jitter or tearing.
    let mut s = tide();
    s.init(&SceneCtx::default());
    let f = snap(0.3, 1.0);

    s.update(&f, 0.03);
    let frame_a = render_field(s.as_mut()).2;
    s.update(&f, 0.03);
    let frame_b = render_field(s.as_mut()).2;

    let mut max_delta = 0.0f32;
    for (a, b) in frame_a.iter().zip(frame_b.iter()) {
        max_delta = max_delta.max((a - b).abs());
    }
    assert!(max_delta > 0.0, "the field must drift between frames");
    assert!(
        max_delta < 0.2,
        "per-cell change {max_delta} should be small and smooth"
    );
}

#[test]
fn tide_state_round_trip() {
    let dt = 0.05;
    let warm = snap(0.6, 2.5); // build up phases, loudness and the front lift
    let next = snap(0.2, 1.0); // the frame both scenes then advance

    // Reference: advance several frames, snapshot, then advance one more.
    let mut a = tide();
    a.init(&SceneCtx::default());
    for _ in 0..30 {
        a.update(&warm, dt);
    }
    let state = a.state();
    assert!(
        state.get("bass").is_some(),
        "state carries the front-lift envelope"
    );
    a.update(&next, dt);
    let field_a = render_field(a.as_mut()).2;

    // Restored: a fresh scene that restores the snapshot and advances the same
    // frame must reproduce the render exactly.
    let mut b = tide();
    b.init(&SceneCtx::default());
    b.restore(state);
    b.update(&next, dt);
    let field_b = render_field(b.as_mut()).2;
    assert_eq!(
        field_a, field_b,
        "restore reproduces the next render exactly"
    );

    // Control: without the restore, the phases and envelopes are lost, so the
    // field differs.
    let mut c = tide();
    c.init(&SceneCtx::default());
    c.update(&next, dt);
    let field_c = render_field(c.as_mut()).2;
    assert_ne!(
        field_a, field_c,
        "a scene that skipped restore should not match"
    );
}
