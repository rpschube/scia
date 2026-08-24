# CAP-2 on Linux — second staging target bring-up + default-sink switch

Live verification of the Linux capture path on a second physical staging
machine (the first Linux hardware the project has run on — CI's PipeWire job
is a headless virtual session). Environment: x86_64 laptop, Ubuntu-24.04-based
distribution, kernel 6.8, PipeWire 1.0.5 (pipewire-pulse), ghostty 1.2.3,
real ALSA hardware including a built-in microphone. Binaries: debug builds of
`capture_probe` and `scia` at master `8567deb`, delivered over SSH; all runs
below are headless (no terminal UI involved).

Method note: sink-level test fixtures were two `module-null-sink` sinks
(`scia_a`, `scia_b`) plus a continuous 440 Hz sine played by a single pinned
`paplay` stream. An earlier attempt with `speaker-test` was discarded — it
reopens its stream every loop cycle, so a stream moved off the default sink
silently respawns back onto it and contaminates any routing experiment.

## Finding 1 — a Linux binary without `capture-pipewire` captures the microphone

The first binaries shipped to the machine were built with default features.
Symptom: `capture_probe` reported full-scale, fluctuating signal with a
wandering spectrum peak **with zero playback streams running**; muting the
machine's microphone source zeroed it instantly. Cause: without the
`capture-pipewire` feature the backend falls back to the ALSA default *input*
device — microphone-level capture, exactly as documented in
`crates/core/src/backends/cpal.rs`. On a desktop with a real mic this is
silent, plausible-looking garbage: ambient noise normalizes into lively bars.

Fixes that came out of this:

- probe-build now installs the PipeWire headers and builds Linux probe
  binaries with `--features capture-pipewire`, and the root crate forwards
  the feature so the `scia` binary itself can carry it.
- Open follow-up (queued): the running binary should surface a notice when
  `prefer_pipewire` is requested but the PipeWire host is unavailable in the
  build, instead of silently visualizing the microphone.

With the feature enabled, the same sequence behaves correctly: sink-monitor
capture is silent with the mic live and no streams playing; a pinned 440 Hz
tone on the default sink gives a rock-steady rms 0.443 (sine at 0.61 peak,
0.61/√2 ≈ 0.43 ✓), spectrum peak fixed on one bar, 512-frame pushes, 0 drops,
0 synthesized hops, max callback gap ≈ 11–14 ms.

## Finding 2 — default-sink switch: detection works, recovery does not

Discriminating design: tone pinned to the *non-default* sink `scia_b`, default
sink `scia_a`, capture running. Expected on a correct CAP-2 pass: silence
until the default moves, then signal after `pactl set-default-sink scia_b`.

Observed, twice (once with `capture_probe`, once with the full engine via
`scia --headless`):

- Pre-switch behavior is correct: pure silence from the `scia_a` monitor
  (0.0000 rms for 8–9 s) while the tone plays elsewhere and the mic is live.
- At the switch, the stream error callback fires (`default device changed`) —
  **detection works** on the cpal PipeWire host.
- The process then exits ≈1 s later with
  `capture stream error: default device changed`. **No recovery.**

Why this is a real CAP-2 gap and not probe artifact: `run_headless` (and the
TUI frame loop, `crates/tui/src/lib.rs`) exit on the first
`StreamHealth::Errored` poll. The `scia-route` watcher (250 ms tick) does
treat `Errored` as a reopen trigger, so for the process to still be `Errored`
at the next 1 s status poll, reopen must have failed at least twice (backoff
doubles from 500 ms) — or each reopened stream immediately re-errors. Either
way the effective behavior on this hardware is: **any default-device switch
on Linux terminates the app.** On Windows this path never fires — the WASAPI
notification flips the reopen-request flag proactively without the stream
erroring, which is why the original CAP-2 verification (done on Windows
hardware) passed.

Suggested shape of the fix (to be specced as its own item):

1. Instrument first: surface `reopen_failures` in the headless status line
   and find out whether reopen fails persistently on the PipeWire host after
   a default switch, or succeeds while the UI loses the race.
2. Treat `Errored` as a *reopening* state with a grace window (the storyboard
   already designs degraded states) — exit only after reopen has failed
   continuously for N seconds, showing the degraded notice meanwhile.

## Tier expectations (for the record)

Capability probing needs the real terminal and was not run over SSH. From the
family defaults (`crates/tui/src/probe.rs`): ghostty ⇒ octant mosaic tier,
kitty-graphics available for the pixel presenter once it lands. Live visual
confirmation in ghostty on this machine is a user-side step.

## Verdicts

- Second Linux staging machine: **viable** — same staging pattern as the
  Windows target (CI- or dev-machine-built binaries over SSH, no local
  toolchain), with
  the one build-feature caveat above, now fixed in probe-build.
- US-CAP-1 (PipeWire sink capture) on real hardware: **pass**, with the
  correct feature set.
- US-CAP-2 (device switch) on Linux: **fail — reopened as a card.**
  Detection confirmed, recovery absent; fix shape above.
