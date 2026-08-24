//! The built-in `aurora` scene: field validity, the loudness-driven band, the
//! level-independent normalizer, the no-jitter guarantee, drift continuity and
//! continuity across a hot reload.

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, Primitive, Scene, SceneCtx, create_builtin};

/// The field's fixed internal resolution (mirrors the scene's own constants).
const COLS: usize = 96;
const ROWS: usize = 54;

fn aurora() -> Box<dyn Scene> {
    create_builtin("aurora").expect("aurora exists")
}

/// A snapshot with a given loudness and onset flag; nothing else is set.
fn snap(rms: f32, onset: bool) -> FeatureSnapshot {
    FeatureSnapshot {
        rms,
        onset,
        ..FeatureSnapshot::default()
    }
}

/// Render one frame and return the sole field's `(cols, rows, values)`.
fn render_field(scene: &mut dyn Scene) -> (u16, u16, Vec<f32>) {
    let mut c = Canvas::new(16.0 / 9.0);
    scene.render(&mut c);
    let prims = c.primitives();
    assert_eq!(prims.len(), 1, "aurora draws exactly one field");
    match prims[0] {
        Primitive::Field { cols, rows, .. } => {
            let data = c.field_of(&prims[0]).expect("field values").to_vec();
            (cols, rows, data)
        }
        ref other => panic!("expected a Field, got {other:?}"),
    }
}

/// Drive `scene` for `frames` steps at `dt` with a fixed snapshot, then render.
fn settle_and_render(scene: &mut dyn Scene, rms: f32, frames: usize, dt: f32) -> Vec<f32> {
    let f = snap(rms, false);
    for _ in 0..frames {
        scene.update(&f, dt);
    }
    render_field(scene).2
}

/// The band's vertical extent: the number of rows whose mean brightness clears a
/// threshold. Outside the bright band the field sits near the ambient floor, so
/// this counts the rows the band actually lights up.
fn band_extent(field: &[f32]) -> usize {
    const THRESHOLD: f32 = 0.15;
    (0..ROWS)
        .filter(|&r| {
            let sum: f32 = field[r * COLS..(r + 1) * COLS].iter().sum();
            sum / COLS as f32 > THRESHOLD
        })
        .count()
}

#[test]
fn aurora_field_is_valid() {
    let mut s = aurora();
    s.init(&SceneCtx::default());
    s.update(&snap(0.4, false), 0.05);
    let (cols, rows, data) = render_field(s.as_mut());

    assert_eq!(cols as usize, COLS);
    assert_eq!(rows as usize, ROWS);
    assert_eq!(
        data.len(),
        COLS * ROWS,
        "field length is cols * rows = {}",
        COLS * ROWS
    );
    for (i, &v) in data.iter().enumerate() {
        assert!(
            (0.0..=1.0).contains(&v),
            "cell {i} value {v} is outside 0..=1"
        );
    }
    // The field must not be uniform: the band and the wave ridges give it real
    // darks and brights, which the coarse tier needs.
    let max = data.iter().copied().fold(0.0f32, f32::max);
    let min = data.iter().copied().fold(1.0f32, f32::min);
    assert!(
        max - min > 0.3,
        "field should span a wide intensity range, got {min}..{max}"
    );
}

#[test]
fn aurora_loudness_widens_the_band() {
    // Same frame schedule for both, so only the loudness envelope — and thus the
    // band width — differs between them.
    let frames = 240;
    let dt = 0.05;

    let mut quiet = aurora();
    quiet.init(&SceneCtx::default());
    let quiet_field = settle_and_render(quiet.as_mut(), 0.0, frames, dt);

    let mut loud = aurora();
    loud.init(&SceneCtx::default());
    let loud_field = settle_and_render(loud.as_mut(), 0.85, frames, dt);

    let quiet_extent = band_extent(&quiet_field);
    let loud_extent = band_extent(&loud_field);
    assert!(
        loud_extent > quiet_extent,
        "sustained loud input should widen the band: loud rows {loud_extent} \
         should exceed quiet rows {quiet_extent}"
    );

    // The total lit energy grows too — a second, coarser check on the same fact.
    let quiet_sum: f32 = quiet_field.iter().sum();
    let loud_sum: f32 = loud_field.iter().sum();
    assert!(
        loud_sum > quiet_sum,
        "loud field total {loud_sum} should exceed quiet {quiet_sum}"
    );
}

#[test]
fn aurora_response_is_level_independent() {
    // The adaptive loudness ceiling calibrates to each material's own loud
    // passages, so two *sustained* signals at very different absolute levels — a
    // quiet-mastered 0.08 and a loud 0.30 — both drive the normalized loudness to
    // ~1.0 and must settle to essentially the same band width. This is the whole
    // point of the normalizer: response independent of mastering level.
    let frames = 300;
    let dt = 0.05;

    let mut quiet = aurora();
    quiet.init(&SceneCtx::default());
    let quiet_field = settle_and_render(quiet.as_mut(), 0.08, frames, dt);

    let mut loud = aurora();
    loud.init(&SceneCtx::default());
    let loud_field = settle_and_render(loud.as_mut(), 0.30, frames, dt);

    let quiet_extent = band_extent(&quiet_field);
    let loud_extent = band_extent(&loud_field);

    // They converge: settled widths differ by at most a row. (Measured: both
    // settle to 54 rows — the band fills the field at normalized loudness ~1.)
    let delta = (quiet_extent as i32 - loud_extent as i32).abs();
    assert!(
        delta <= 1,
        "level-independent response: quiet-mastered ({quiet_extent}) and loud \
         ({loud_extent}) settled band widths should converge, differ by {delta}"
    );
    // ...and both are genuinely widened, not converging on the narrow floor.
    assert!(
        quiet_extent >= 40 && loud_extent >= 40,
        "both sustained levels should widen the band well past the quiet floor: \
         quiet {quiet_extent}, loud {loud_extent}"
    );
}

#[test]
fn aurora_loud_quiet_band_ratio_is_visible() {
    // Within one normalization context the swing from a quiet floor to a settled
    // loud passage must be large enough to read on real music: assert the settled
    // loud band is at least twice the quiet band's height.
    let frames = 300;
    let dt = 0.05;

    let mut quiet = aurora();
    quiet.init(&SceneCtx::default());
    let quiet_field = settle_and_render(quiet.as_mut(), 0.0, frames, dt);

    let mut loud = aurora();
    loud.init(&SceneCtx::default());
    let loud_field = settle_and_render(loud.as_mut(), 0.30, frames, dt);

    let quiet_extent = band_extent(&quiet_field);
    let loud_extent = band_extent(&loud_field);

    // Measured: quiet floor ~18 rows, settled loud ~54 rows (ratio ~3x).
    assert!(
        quiet_extent > 0,
        "the quiet floor should still light a band, got {quiet_extent}"
    );
    assert!(
        loud_extent >= 2 * quiet_extent,
        "settled loud band ({loud_extent} rows) should be at least twice the \
         quiet band ({quiet_extent} rows) to read as a visible response"
    );
}

#[test]
fn aurora_ignores_onset_at_equal_loudness() {
    // Two scenes fed the SAME loudness; one has its onset flag flipping every
    // frame, the other never. Nothing in aurora reads the onset, so the fields
    // must be bit-for-bit identical after any number of frames.
    let dt = 0.05;
    let rms = 0.5;

    let mut flipping = aurora();
    flipping.init(&SceneCtx::default());
    let mut steady = aurora();
    steady.init(&SceneCtx::default());

    for i in 0..120 {
        flipping.update(&snap(rms, i % 2 == 0), dt);
        steady.update(&snap(rms, false), dt);
    }

    let a = render_field(flipping.as_mut()).2;
    let b = render_field(steady.as_mut()).2;
    assert_eq!(
        a, b,
        "onset flips must not change the field at equal loudness"
    );
}

#[test]
fn aurora_drifts_smoothly() {
    // Consecutive frames differ — the field drifts — but every cell moves by only
    // a small amount, so there is no jitter or tearing.
    let mut s = aurora();
    s.init(&SceneCtx::default());
    let f = snap(0.3, false);

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
fn aurora_state_round_trip() {
    let dt = 0.05;
    let warm = snap(0.6, false); // build up phases and the loudness envelope
    let next = snap(0.2, false); // the frame both scenes then advance

    // Reference: advance several frames, snapshot, then advance one more.
    let mut a = aurora();
    a.init(&SceneCtx::default());
    for _ in 0..30 {
        a.update(&warm, dt);
    }
    let state = a.state();
    // The loudness ceiling is part of the calibration that must survive a hot
    // reload: after sustaining rms 0.6 it has climbed to roughly that level, and
    // the round-trip below only reproduces the next render if `next` (rms 0.2,
    // below the ceiling) reads the same normalized loudness on both scenes.
    let ceil = state
        .get("ceil")
        .expect("state carries the loudness ceiling");
    assert!(
        ceil > 0.4,
        "the ceiling should have climbed toward the sustained level, got {ceil}"
    );
    a.update(&next, dt);
    let field_a = render_field(a.as_mut()).2;

    // Restored: a fresh scene that restores the snapshot and advances the same
    // frame must reproduce the render exactly.
    let mut b = aurora();
    b.init(&SceneCtx::default());
    b.restore(state);
    b.update(&next, dt);
    let field_b = render_field(b.as_mut()).2;
    assert_eq!(
        field_a, field_b,
        "restore reproduces the next render exactly"
    );

    // Control: without the restore, the phases and envelope are lost, so the
    // field differs.
    let mut c = aurora();
    c.init(&SceneCtx::default());
    c.update(&next, dt);
    let field_c = render_field(c.as_mut()).2;
    assert_ne!(
        field_a, field_c,
        "a scene that skipped restore should not match"
    );
}
