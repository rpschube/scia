# P7 — end-to-end audio→feature latency

Probe, not production. Goal: measure how long a sound takes to travel from the
system output to the feature snapshot that carries it — the audio→feature
latency that US-PERF-1's ≤33 ms audio-to-visual budget is measured against.
It is **photodiode-free**: both ends are stamped on the same monotonic clock
(the ring epoch that `FeatureSnapshot.timestamp_ns` uses), so no external
instrument is needed. Tooling: `latency_probe` (`crates/core/examples`), built
by the `probe-build` workflow and run headless on the machine; the same library
logic runs as a CI regression test through the synthetic backend
(`crates/core/tests/latency.rs`).

## Method

The probe emits a train of known clicks and watches them come back:

- A **click player** writes rectangular bursts into the system output — one
  every `--spacing-ms`, `--click-ms` long, at amplitude `--amp`, after a 1 s
  pre-roll that lets the engine leave `Idle` and the loopback settle. When the
  first frame of a click lands in the output buffer it records an **emission**
  (`emit_ns`) into a wait-free log; on a real stream it also records the output
  callback→playback delay the host predicts.
- The engine captures the system mix through the normal OS loopback and
  publishes one `FeatureSnapshot` per 256-frame hop, each stamped with
  `timestamp_ns` (the **publish** time).
- An **observer** polls the feature bus every 1 ms and edge-triggers on `peak`
  crossing `--threshold`, recording an **observe** time for each detected click.
- Emissions and detections are paired in order (a detection takes the latest
  unmatched emission within half a click spacing); leftover emissions are
  *missed*, leftover detections are *spurious*.

In **synthetic** mode the click player is replaced by `SyntheticBackend`'s click
generator, which records the same emissions with `emit_ns` sampled from the
capture clock immediately before each chunk is pushed. Everything downstream is
identical, so the synthetic run measures the pipeline's own contribution with no
audio hardware.

### What is *not* measured

The probe measures audio→**feature**, not audio→**pixel**. Screen presentation
is outside it: a renderer reads the freshest snapshot and draws it, and the
render loop adds up to one frame interval on top — about **16.7 ms at 60 fps**
(one refresh), more at lower frame rates. So the audio-to-visual latency
US-PERF-1 bounds is roughly `emit→observe` (this probe) **plus** up to one frame
interval of render/present latency. Reason about the ≤33 ms budget with both
terms in mind; this probe pins down the first one.

## How to run

Built by the `probe-build` workflow (artifact `latency_probe`), or locally
through `just`:

```
# On a real machine — plays clicks and captures them back via loopback:
latency_probe --clicks 25
latency_probe --clicks 25 --perf-mode          # Windows fast-period companion
latency_probe --output-device "NAME"           # pick the output endpoint

# No audio hardware — the library measurement the CI test also runs:
latency_probe --synthetic --clicks 10
```

Useful switches: `--spacing-ms` (gap between clicks), `--amp`, `--click-ms`,
`--threshold` (peak level a click must clear), `--device` (capture device),
`--list` (device table). The probe exits `0` when ≥80 % of the emitted clicks
were matched, `4` otherwise; `2` on a usage error or an output device that
offers no f32 format; `3` when no capture or output device is available.

Note that with a free-running synthetic click train the observer window is a
whole number of click spacings, so a short mid-gap flush tail is added after it
before the log is drained; this keeps the last click's hop observed and the
missed/spurious counts honest.

## The three intervals and their quantization

Each interval is reported as nearest-rank percentiles (min / median / p95 / max)
in milliseconds:

- **emit → publish** — from the click's emission to the `timestamp_ns` of the
  hop that carries it. This is capture transport plus the hop grid: a click can
  wait up to one 256-frame hop (**5.33 ms at 48 kHz**) to be gathered into a
  hop, plus however long the capture path buffered it.
- **publish → observe** — from the hop's publish time to the observer reading
  it. Bounded by the observer's **1 ms** poll interval (plus scheduling
  jitter).
- **emit → observe** — the end-to-end audio→feature latency, the sum of the two
  above. Expected on the order of a hop plus a poll (~6–10 ms) with the
  synthetic backend; a real loopback adds the endpoint's capture latency.
- **output delay (cb→play)** — live only: the host's predicted output
  callback→playback delay plus the click's offset inside that callback's buffer
  (the burst starts partway through the buffer the callback fills), recorded per
  click. Subtract its median from emit → observe to estimate the latency from
  the moment the click actually enters the mix. `0` in synthetic mode.

Quantization sources to keep in mind when reading the numbers: the 256-frame hop
grid (5.33 ms at 48 kHz), the 1 ms DSP poll while waiting for a partial hop, the
1 ms observer poll, and the click length itself (`--click-ms`, default 1 ms).

## Results

Pending — filled in from the real-machine run.

| Run | Clicks | Matched | Missed | Spurious | emit→publish median | publish→observe median | emit→observe median | output delay median |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| | | | | | | | | |
