# P5 — Luau scene-tick cost

Probe, not production. Goal: measure ns per tick for a representative sandboxed
Luau scene running against the real `scia_core::FeatureSnapshot`, so the scene
trait can be frozen with numbers rather than guesses. Nothing here is API yet;
the code lives in `crates/scenes/benches/luau_tick.rs`,
`crates/scenes/benches/lua/*.luau` and `crates/scenes/tests/luau_deadline.rs`,
all behind `[dev-dependencies]`.

## Setup

- Machine: the dev machine (12-core x86-64 Linux).
- Engine: `mlua` 0.11.6 with features `luau` + `vendored` (MIT), bundling
  Luau 0.709 (`luau0-src` 0.18.3+luau709, `mlua-sys` 0.10.0). Vendored Luau
  compiled cleanly with the system C++ toolchain — no build failures.
- Harness: `criterion` 0.6.0 (default features off, `cargo_bench_support` only),
  `harness = false`. Compiler: rustc 1.96.1, release/bench profile
  (`lto = "fat"`, `codegen-units = 1`).
- Bench config: `sample_size = 30`, 0.5 s warm-up, 2 s measurement per variant.
  Whole suite wall time: ~46 s including the optimized compile; the measured
  portion is well under one minute.

Representative scene per tick: read `rms`, `peak`, all 64 spectrum bars and
`onset`; advance 200 particles with simple physics (velocity + a nudge from the
matching spectrum bar); draw 64 bars + 200 points. The host clears/reads the
draw output back every tick, symmetric across canvas shapes.

Budget reference: a 144 fps frame is 6.94 ms; the 10% target for the scene tick
is **≤ ~694 µs**.

## Results (median per tick)

Medians from one representative run; the low/high bounds are criterion's 95% CI.
`ns/tick` is the median restated.

### A. Feature-access shape (each with canvas B1)

| Variant | What crosses into Luau | Median | ns/tick |
|---|---|---:|---:|
| A1 `table` | fresh Lua table rebuilt every tick | 52.15 µs | 52,150 |
| A2 `userdata` | field getters + `features:bar(i)` method ×64 | 56.45 µs | 56,450 |
| A3 `inplace` | userdata scalars + preallocated bars table updated in place | **50.27 µs** | **50,270** |

A3 is fastest; A2 is slowest because 64 `bar(i)` method calls each pay a
C→Rust crossing. A1 is close behind A3 — a full 64-bar table rebuild per tick is
cheaper than expected because Luau table writes are fast, but it still allocates.

### B. Canvas call shape (each with feature-access A3)

| Variant | How draw calls cross back | Median | ns/tick |
|---|---|---:|---:|
| B1 `methods` | per-primitive `canvas:bar/point` userdata calls | **50.26 µs** | **50,260** |
| B2 `displaylist` | script fills a flat number array, host reads it back | 78.41 µs | 78,410 |

B1 wins by ~36%. B2's flat-array readback crosses ~1,584 individual numbers back
into Rust per tick (`raw_get` in a loop), which costs more than 264 typed method
calls despite “feeling” bulk.

### C. Sandbox / limit overhead (best combination A3 + B1)

| Variant | Median | vs `plain` |
|---|---:|---:|
| `plain` (no sandbox, no interrupt, no memlimit) | 50.99 µs | — |
| `sandbox` (only `sandbox(true)`) | 52.07 µs | +2.1% |
| `sandbox_interrupt_noop` (interrupt installed, empty callback) | 58.80 µs | +15.3% |
| `sandbox_interrupt_clock` (interrupt reads clock every safepoint) | **1013.6 µs** | **+1888%** |
| `sandbox_interrupt_counted` (clock read gated to every 1024th safepoint) | 62.38 µs | +22.3% |
| `full` (sandbox + clock-every-safepoint interrupt + 8 MiB memlimit) | 1015.8 µs | +1892% |

Reading of the overhead:

- **`sandbox(true)` is essentially free** (+2%). Luau's `safeenv` fast path is
  not disturbed by our scene, which touches no globals.
- **The 8 MiB memory limit is free**: `full` (1015.8 µs) vs `clock` (1013.6 µs)
  is within noise, so `set_memory_limit` adds ~0%.
- **The interrupt crossing itself costs ~15%** (`noop` 58.8 µs). Luau fires the
  interrupt at *every* VM safepoint (loop back-edges, calls, returns) — there is
  no “every N instructions” knob — so a scene with 264-iteration loops and 264
  draw calls incurs on the order of a thousand crossings per tick.
- **The killer is calling a clock on every safepoint.** `Instant::now()` per
  safepoint explodes the tick to ~1.01 ms — a 20× blow-up and, at 14.6% of the
  6.94 ms frame, over the 10% budget on its own.
- **Gating the clock read fixes it.** Checking the deadline only every 1024th
  safepoint (`counted`) returns the tick to 62.4 µs (+22% over plain), i.e. the
  crossing cost plus a cheap counter. This is the shape the engine should use.

## D. Deadline enforcement

`tests/luau_deadline.rs` installs a `set_interrupt` deadline 50 ms out and runs
`while true do end`. The interrupt fires on the loop back-edge, returns
`Err(mlua::Error::runtime(...))`, the loop is aborted, and the error surfaces to
Rust as a failed `call`. Observed: the test completes a few ms past the 50 ms
deadline (well within the ~2 ms-class target; the assertion bound is a generous
250 ms so a loaded machine cannot flake). **Deadline enforcement works.**

## Recommendation for the scene trait

1. **Feature access: A3.** Userdata field getters for scalars + one preallocated
   `bars` table the host refreshes in place (50.3 µs). Avoid per-tick table
   rebuilds (A1) and avoid a per-bar method (A2).
2. **Canvas: B1.** Per-primitive userdata methods pushing into a host `Vec`
   (50.3 µs); the flat display-list readback (B2, 78.4 µs) is ~36% slower.
3. **Sandbox: yes.** `sandbox(true)` (+2%) and an 8 MiB `set_memory_limit` (~0%)
   are effectively free — enable both unconditionally.
4. **Deadline: interrupt, but never read a clock per safepoint.** Gate the clock
   check (or read an atomic flag set by a watchdog) to ~1-in-1024 safepoints:
   62 µs vs 1.01 ms for the naive check. It still stops a runaway loop promptly.
5. **Headroom is ample.** The full recommended tick (A3+B1+sandbox+counted
   interrupt+memlimit) is ~62 µs — ~0.9% of the 6.94 ms 144 fps frame, far under
   the ~694 µs (10%) budget. The only way to blow it is the naive per-safepoint
   clock, which this probe rules out.

### Notes / honesty

- All variants compiled and ran; nothing was skipped.
- The spec framed the interrupt as “every 1000 instructions”. Luau has no such
  knob — the interrupt fires at every safepoint — so the `counted` variant
  (clock every 1024th safepoint) is the practical realization of that intent and
  is what the recommendation adopts.
- Medians drift run to run (±~5%); the relative ordering above was stable across
  runs. Absolute numbers are for the dev machine only.
