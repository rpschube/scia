# P1 — Windows loopback cadence and the perf-mode companion stream

Probe, not production. Goal: measure the WASAPI shared-mode loopback packet
cadence on a real Windows machine, and whether a companion silent render
stream opened at the endpoint's minimum engine period
(`IAudioClient3::InitializeSharedAudioStream`) makes the loopback capture
deliver packets faster. Tooling: `capture_probe` (`crates/core/examples`),
built by the `probe-build` workflow and run headless on the machine.

## Setup

- Windows 11 (build 26200), default render endpoint: an onboard Realtek
  HD Audio device, shared mode, 48 kHz, 2 channels, f32 mix format.
- Music playing on the machine for the whole run.
- Probe run twice: baseline, then with `--perf-mode`.

## Results

| Run | Packets | Frames / packet | Worst gap | Dropped | Xruns | Synthesized hops |
|---|---:|---:|---:|---:|---:|---:|
| baseline, 8 s | 800 | 480.0 (10.0 ms) | 11.4 ms | 0 | 1 | 0 |
| `--perf-mode`, 8 s | 800 | 480.0 (10.0 ms) | 14.2 ms | 0 | 1 | 0 |

Engine periods reported by `GetSharedModeEnginePeriod` on this endpoint:

```
default = 480 (10.000 ms)   fundamental = 480 (10.000 ms)
min     = 480 (10.000 ms)   max         = 480 (10.000 ms)   chosen = 480
```

## Reading

- The loopback baseline is the classic 10 ms engine period: one 480-frame
  packet every 10 ms, no drops, one transient buffer xrun in 8 s (counted,
  not fatal).
- **This endpoint's driver exposes no shared-mode period other than 10 ms.**
  The companion stream opened successfully, but the only period it could ask
  for was the default, so nothing changed for the loopback capture. The
  mechanism is implemented and behaves correctly; whether it helps is decided
  by the endpoint's driver, not by the application.
- The design assumption that a small engine period is generally available on
  Windows is therefore **hardware-dependent**: it holds for endpoints whose
  driver advertises a minimum period below the default (many USB-audio and
  newer inbox drivers do), and does nothing on endpoints like this one.

## Consequences

1. Perf mode stays opt-in and becomes capability-detected: when
   `min == default` the engine should report "not available on this
   endpoint" instead of opening a pointless companion stream.
2. The default latency story does not depend on it: capture age is bounded
   by the 10 ms period, well inside the 33 ms audio-to-visual target.
3. Re-run this probe on an endpoint that advertises a sub-10 ms minimum
   period before promising numbers for perf mode in user-facing docs.
