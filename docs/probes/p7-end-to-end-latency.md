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

A synthetic click clears the detector's threshold for only a single hop
(~5.3 ms at 256 frames / 48 kHz), so whether the CI regression test observes a
click at all depends on the observer sampling that one hop. When the platform
coalesces the observer's short poll sleeps into a coarser timer, the observer
samples only a fraction of the hops and sees a matching fraction of the clicks —
a sampling-granularity artifact, not a pipeline dropping clicks. The test
therefore calibrates its missed-click allowance from the observer's measured
sampling coverage (observed hops over the generation span they cover) rather
than a fixed count: at full coverage the allowance is the old small slack, so a
pipeline genuinely dropping clicks while every hop is sampled still fails.

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

First live run — Windows 11 desktop, onboard Realtek endpoint (shared mode,
48 kHz, 2 ch), default mode (perf mode off), 25 clicks at 400 ms spacing:

```
clicks 25 · matched 25 · missed 0 · spurious 0
                           min  median     p95     max   (ms)
emit → publish           81.03   81.31   81.89   81.99
publish → observe         0.75    1.36    2.17    2.19
emit → observe           82.09   82.59   83.34   83.44
output delay (cb→play)   39.65   39.85   39.94   39.94
engine: pushes 1224 · dropped 0 · xruns 1 · hops 2293/0 (processed/synthesized)
```

Reading:

- Detection is airtight: 25/25 matched, no drops, no synthesized hops, and
  the feature bus adds ~1.4 ms (publish → observe).
- The ~39.9 ms output delay is the probe player's own render buffering
  (cpal's default WASAPI output stream) plus the click's intra-buffer
  offset — it is NOT part of scia's capture path; a real player's audio is
  already in the mix.
- **Open question:** estimated playback → publish ≈ 41 ms median
  (81.3 − 39.9), vs ~15–20 ms expected from the P1 cadence (10 ms loopback
  packet + 5.3 ms hop + polls). Either cpal's output playback timestamp
  underestimates the true chain (making the residual smaller than it looks)
  or the loopback path buffers more than its packet cadence suggests. A
  follow-up probe should cross-correlate raw ring samples against the
  emitted click (sub-millisecond, no hop quantization) before the ≤ 33 ms
  US-PERF-1 criterion is scored on this endpoint.

## Raw-ring mode (`--raw-ring`)

The follow-up the open question asks for. It answers a narrower question than the
main probe — *how much of the emit → publish interval is capture transport,
before scia's hop grid touches the samples* — and answers it without the
256-frame hop quantization that blurs the main probe's `emit → publish` number.

**How it differs.** In raw-ring mode the probe does **not** run the engine or the
DSP hop grid. It opens the same capture backend the engine uses
(`CaptureBackend::open`) directly into a probe-local sample ring, then drains
that ring on a 1 ms poll off-thread, accumulating the exact interleaved samples
(down-mixed to mono) with a per-drain capture timestamp. For each emitted click
it runs a **normalized matched-filter cross-correlation** of the captured stream
against the known click template (a rectangular burst of `--click-ms` at
`--amp`) over a search window one click spacing wide centered on the click's
`emit_ns` (`emit_ns − spacing/2` … `emit_ns + spacing/2`), and takes the
correlation argmax as the click's **leading edge in the raw stream**. Resolution
is one sample (≈ 0.02 ms at 48 kHz), not one hop. (The window reaches back half a
spacing as well as forward because the synthetic backend stamps `emit_ns` just
before pushing a chunk, so on the near-zero-transport synthetic path a sample's
continuous-capture time can fall a few ms *before* the emission; a neighbour
click is a full spacing away and cannot be picked up, and real transport always
lands to the right of `emit_ns`.)

**Timestamp bookkeeping.** The frames a poll drains are the most-recently
captured frames still in the ring, so the newest sits about one frame-period
before the poll's clock read and the oldest about `frames` frame-periods before
it. The probe places a drain's oldest frame at `drain_ns − frames ×
ns_per_frame` and steps one frame-period per frame; the ring clock is the same
epoch `FeatureSnapshot.timestamp_ns` and the click player's `emit_ns` use, so
`emit → raw-arrival` is a difference on one clock. The residual error is a single
poll interval of jitter on `drain_ns` (sub-millisecond quanta are negligible).

**How to run.**

```
# Real machine — plays clicks, captures them back, correlates the raw ring:
latency_probe --raw-ring --clicks 25
latency_probe --raw-ring --output-device "NAME"   # pick the output endpoint

# No audio hardware — the CI-testable path (synthetic click backend):
latency_probe --raw-ring --synthetic --clicks 10
```

The report prints, per click and as nearest-rank percentiles, one new interval:

- **emit → raw-arrival** — from the click's emission to the moment its samples
  enter scia's ring, by cross-correlation. This is **capture transport only**;
  it is the part of the main probe's `emit → publish` that happens *before* scia.

The probe keeps the same exit-code contract as the main mode (`0` when ≥ 80 % of
emitted clicks correlate, `4` otherwise; `2`/`3` for usage / no device).

**What it measures, and what it cannot.** Raw-arrival tells how much of the
~41 ms residual is capture transport. It does **not** measure `emit → publish`
(no hop grid runs in this mode); the hop-gather term is *stated, not measured* —
a click waits up to one 256-frame hop (5.33 ms at 48 kHz) to be gathered, so a
normal run's `emit → publish ≈ raw-arrival + up-to-one-hop`. Comparing a
raw-ring run's `emit → raw-arrival` against a normal run's `emit → publish` on
the same endpoint decomposes the residual: whatever raw-arrival accounts for is
capture buffering, and the rest is the hop grid plus any output-timestamp error.
It also does not correct for the output player's own render buffering — subtract
the main probe's `output delay (cb→play)` from raw-arrival, as with `emit →
observe`, to reason about the moment a real player's audio is already in the mix.

**Synthetic self-test.** `--raw-ring --synthetic` (and the
`crates/core/tests/latency.rs` regression that drives the same library logic)
feeds the synthetic click generator into the ring with no hardware. A synthetic
click is a single-frame impulse in otherwise-silent audio, so its correlation
peak is unambiguous and `emit → raw-arrival` collapses to the drain/push cadence
— a few milliseconds, and slightly *negative* because the emit stamp precedes the
push while the sample time is on the continuous-capture model. The test asserts
≥ 80 % correlated and a loose two-sided median bound (±10 ms), proving the
correlation and timeline machinery end-to-end. Real-hardware raw-arrival numbers
land here later via the orchestrator.
