//! P5 probe D: the instruction interrupt actually stops a runaway script and
//! surfaces the error back to Rust. This is a correctness probe, not a timing
//! benchmark — it proves the deadline mechanism the scene engine will rely on.

use std::time::{Duration, Instant};

use mlua::{Lua, VmState};

#[test]
fn interrupt_stops_infinite_loop() {
    let lua = Lua::new();
    let budget = Duration::from_millis(50);
    let start = Instant::now();
    let deadline = start + budget;

    lua.set_interrupt(move |_| {
        if Instant::now() >= deadline {
            Err(mlua::Error::runtime("scene tick exceeded its deadline"))
        } else {
            Ok(VmState::Continue)
        }
    });

    let result = lua.load("while true do end").exec();
    let elapsed = start.elapsed();

    let err = result.expect_err("an infinite loop must be interrupted, not run forever");
    assert!(
        err.to_string().contains("deadline"),
        "the interrupt error should surface to Rust; got: {err}"
    );
    // It must run to at least the deadline and stop promptly after it. The
    // target overshoot is ~2 ms; the upper bound is generous so a loaded
    // machine does not flake while still proving the loop cannot run away.
    assert!(
        elapsed >= budget,
        "script stopped before its deadline, after only {elapsed:?}"
    );
    assert!(
        elapsed < budget + Duration::from_millis(250),
        "interrupt overshoot too large: {elapsed:?} (budget {budget:?})"
    );
}
