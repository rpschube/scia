//! The sandbox and fault-isolation guarantees of a Luau scene (US-CFG-4):
//!
//!  * `os`, `io`, `package`, `require`, `loadstring`, `load` are all absent;
//!  * an infinite loop is interrupted within the deadline and reported;
//!  * unbounded table/string growth trips the memory cap;
//!  * an error at load OR at a tick never panics the host and never blanks the
//!    canvas — the last good frame holds and the error is surfaced.

use std::time::{Duration, Instant};

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, LuauLimits, LuauScene, Scene, SceneCtx};

/// A scene whose `update` asserts every dangerous global is `nil`; if any leaked
/// the assert fails and the scene latches into its error state.
const SANDBOX_PROBE: &str = r#"
return {
  id = "probe",
  mood = "test",
  summary = "asserts the sandbox hides dangerous globals",
  update = function(features, dt)
    assert(os == nil, "os leaked")
    assert(io == nil, "io leaked")
    assert(package == nil, "package leaked")
    assert(require == nil, "require leaked")
    assert(loadstring == nil, "loadstring leaked")
    assert(load == nil, "load leaked")
    assert(dofile == nil, "dofile leaked")
    assert(getfenv == nil, "getfenv leaked")
  end,
  render = function(canvas) end,
}
"#;

fn ctx() -> SceneCtx {
    SceneCtx::default()
}

#[test]
fn sandbox_hides_dangerous_globals() {
    let mut scene =
        LuauScene::from_source(SANDBOX_PROBE, "probe", LuauLimits::default()).expect("compiles");
    scene.init(&ctx());
    // init already ran one probe update; a leak would have latched the scene.
    scene.update(&FeatureSnapshot::default(), 1.0 / 60.0);
    assert!(
        !scene.is_errored(),
        "no dangerous global leaked into the sandbox; error: {:?}",
        scene.last_error()
    );
}

#[test]
fn a_failing_assert_latches_the_scene() {
    // The control for the test above: prove the assert mechanism actually bites,
    // so a passing sandbox probe means the globals really are nil.
    const ALWAYS_FAILS: &str = r#"
    return {
      id = "boom", mood = "test", summary = "always errors",
      update = function() assert(false, "intentional") end,
      render = function(c) end,
    }
    "#;
    let mut scene =
        LuauScene::from_source(ALWAYS_FAILS, "boom", LuauLimits::default()).expect("compiles");
    scene.init(&ctx());
    assert!(scene.is_errored(), "a failing assert latches the scene");
    assert!(
        scene
            .last_error()
            .is_some_and(|e| e.contains("intentional")),
        "the message is surfaced: {:?}",
        scene.last_error()
    );
}

#[test]
fn infinite_loop_is_interrupted_and_reported() {
    const RUNAWAY: &str = r#"
    return {
      id = "runaway", mood = "test", summary = "spins forever",
      update = function() while true do end end,
      render = function(c) end,
    }
    "#;
    let limits = LuauLimits {
        tick_budget: Duration::from_millis(40),
        ..LuauLimits::default()
    };
    let scene = LuauScene::from_source(RUNAWAY, "runaway", limits);
    // Compilation succeeds; the loop only runs when a tick drives it.
    let mut scene = scene.expect("compiles");
    let start = Instant::now();
    // init primes with one update, which runs the infinite loop; it must be
    // interrupted rather than hang.
    scene.init(&ctx());
    let elapsed = start.elapsed();
    assert!(scene.is_errored(), "the runaway must latch the scene");
    assert!(
        scene.last_error().is_some_and(|e| e.contains("deadline")),
        "the deadline trip is surfaced: {:?}",
        scene.last_error()
    );
    // It ran at least to its deadline and stopped promptly after — the generous
    // upper bound keeps a loaded machine from flaking.
    assert!(elapsed >= Duration::from_millis(40));
    assert!(
        elapsed < Duration::from_secs(5),
        "interrupt overshoot too large: {elapsed:?}"
    );
}

#[test]
fn unbounded_growth_trips_the_memory_cap() {
    // A generous per-tick deadline so memory — not the clock — is what stops it.
    const GLUTTON: &str = r#"
    return {
      id = "glutton", mood = "test", summary = "grows a table without bound",
      update = function()
        local t = {}
        local n = 0
        while true do
          n = n + 1
          t[n] = string.rep("x", 4096)
        end
      end,
      render = function(c) end,
    }
    "#;
    let limits = LuauLimits {
        memory_bytes: 16 * 1024 * 1024,
        tick_budget: Duration::from_secs(10),
    };
    let mut scene = LuauScene::from_source(GLUTTON, "glutton", limits).expect("compiles");
    scene.init(&ctx());
    assert!(scene.is_errored(), "the memory cap must latch the scene");
    assert!(
        scene
            .last_error()
            .is_some_and(|e| e.to_lowercase().contains("memory")),
        "the memory-cap trip is surfaced: {:?}",
        scene.last_error()
    );
}

#[test]
fn error_at_load_is_reported_not_panicked() {
    // Not valid Lua at all.
    assert!(
        LuauScene::from_source("this is not lua ]][[", "bad", LuauLimits::default()).is_err(),
        "a syntax error is an Err, never a panic"
    );
    // Valid Lua that does not return a manifest table.
    assert!(
        LuauScene::from_source("return 42", "num", LuauLimits::default()).is_err(),
        "a non-table return is a manifest error"
    );
    // A manifest missing the required render function.
    const NO_RENDER: &str = r#"
    return { id = "x", mood = "m", summary = "s", update = function() end }
    "#;
    assert!(
        LuauScene::from_source(NO_RENDER, "x", LuauLimits::default()).is_err(),
        "a missing render is a manifest error"
    );
}

#[test]
fn error_at_tick_holds_the_last_good_frame() {
    // Renders one point every frame; the third update errors. The render after
    // the fault must hold the last good frame (one point), not blank.
    const FAILS_ON_FRAME_3: &str = r#"
    local n = 0
    return {
      id = "late", mood = "test", summary = "errors on the third update",
      update = function(f, dt) n = n + 1; if n >= 3 then error("boom") end end,
      render = function(c) c:point(0.5, 0.5, 0.05, 1, 1.0) end,
    }
    "#;
    let mut scene =
        LuauScene::from_source(FAILS_ON_FRAME_3, "late", LuauLimits::default()).expect("compiles");
    let cx = SceneCtx::default();
    scene.init(&cx); // update #1 (prime), render primes last_good with one point
    let snap = FeatureSnapshot::default();

    // Frame with update #2 — still healthy, one point drawn.
    let mut canvas = Canvas::new(1.0);
    scene.update(&snap, 0.016);
    scene.render(&mut canvas);
    assert!(!scene.is_errored());
    assert_eq!(
        canvas.primitives().len(),
        1,
        "a healthy frame draws one point"
    );

    // Frame with update #3 — the fault fires in update; render must hold the
    // last good frame rather than blank the canvas.
    scene.update(&snap, 0.016);
    assert!(scene.is_errored(), "the tick fault latches the scene");
    assert!(scene.last_error().is_some_and(|e| e.contains("boom")));
    let mut after = Canvas::new(1.0);
    scene.render(&mut after);
    assert_eq!(
        after.primitives().len(),
        1,
        "the last good frame holds; the canvas is not blanked"
    );
}

#[test]
fn error_in_render_holds_the_last_good_frame() {
    // The render itself errors on the third call; the last good frame holds.
    const RENDER_FAILS: &str = r#"
    local n = 0
    return {
      id = "rfail", mood = "test", summary = "render errors on the third call",
      update = function(f, dt) end,
      render = function(c)
        n = n + 1
        if n >= 3 then error("render boom") end
        c:point(0.5, 0.5, 0.05, 2, 1.0)
      end,
    }
    "#;
    let mut scene =
        LuauScene::from_source(RENDER_FAILS, "rfail", LuauLimits::default()).expect("compiles");
    let cx = SceneCtx::default();
    scene.init(&cx); // render #1 (prime) draws one point
    let snap = FeatureSnapshot::default();

    let mut c2 = Canvas::new(1.0);
    scene.update(&snap, 0.016);
    scene.render(&mut c2); // render #2, one point
    assert_eq!(c2.primitives().len(), 1);
    assert!(!scene.is_errored());

    let mut c3 = Canvas::new(1.0);
    scene.update(&snap, 0.016);
    scene.render(&mut c3); // render #3 errors → hold last good
    assert!(scene.is_errored(), "a render fault latches the scene");
    assert_eq!(
        c3.primitives().len(),
        1,
        "the render fault holds the last good frame"
    );
}

#[test]
fn a_hostile_field_grid_is_bounded_and_prompt() {
    // canvas:field() copies cols*rows values on the host side, outside the Lua
    // interrupt and memory cap — the bridge must clamp the dimensions itself.
    // Unclamped, 65535×65535 would copy ~4.3 billion values (a multi-minute
    // stall and gigabytes of host memory); clamped, the tick stays microseconds.
    const HOSTILE_FIELD: &str = r#"
    return {
      id = "hostile-field", mood = "test", summary = "oversized field grid",
      update = function(features, dt) end,
      render = function(canvas)
        canvas:field(65535, 65535, {}, 0, 1.0)
      end,
    }
    "#;
    let mut scene = LuauScene::from_source(HOSTILE_FIELD, "hostile-field", LuauLimits::default())
        .expect("compiles");
    scene.init(&ctx());
    scene.update(&FeatureSnapshot::default(), 1.0 / 60.0);
    let mut canvas = Canvas::new(1.0);
    let started = Instant::now();
    scene.render(&mut canvas);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an oversized field must be clamped, not copied whole"
    );
    assert!(
        !scene.is_errored(),
        "a clamped field draw is not a fault: {:?}",
        scene.last_error()
    );
    // The stored grid is the clamped one: at most 256×256 values.
    assert!(
        canvas.field_data().len() <= 256 * 256,
        "field arena holds {} values, expected the clamped grid",
        canvas.field_data().len()
    );
}
