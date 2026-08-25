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
  hop that carries it. `timestamp_ns` is the **capture-delivery time of the hop's
  newest frame** — when that frame entered scia's ring, taken as the *exact
  delivery time of the push that carried it* (from the primary ring's per-push
  delivery log), *not* the DSP's own wall-clock at the moment it processed the hop
  (see *Publish clock* below). So this is capture
  transport plus up to one 256-frame hop (**5.33 ms at 48 kHz**) of gather: a
  click can wait up to a hop to be gathered, on top of however long the capture
  path buffered it. It is anchored on the **same** capture-delivery clock the
  raw-ring mode's `emit → raw-arrival` uses, so on one run
  `emit → raw-arrival ≤ emit → publish ≤ emit → raw-arrival + one hop` holds by
  construction.
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

### Publish clock

A snapshot's `timestamp_ns` marks *when its audio was captured*, not when the DSP
thread got around to processing it. The DSP pops the **oldest** buffered hop but
runs at its own cadence, so a plain "wall-clock at pull" stamp folds in the DSP's
scheduling jitter and however long the hop sat in the ring — a term that does not
belong in a capture timestamp, and one that is invisible to the raw-ring mode
(which anchors on delivery). To keep the two modes on one clock, the DSP stamps
each real hop with the **capture-delivery time of its newest frame** — the *exact
delivery time of the push that carried that frame*.

The primary capture ring keeps a wait-free **per-push delivery log**: every push
records `(frames, delivery_ns, cumulative_frames)`, the same record the dual-tap
tee logs. The DSP consumes that log in lockstep with the frames it pops, so for a
hop ending at global frame `g` it finds the push whose range covers `g` and stamps
`delivery_ns − (push_newest − g) × ns_per_frame` — exact per push, with no
occupancy inference.

That inference is what the earlier model used — `last_push_ns − frames_left_in_ring
× ns_per_frame` — and it is exact only when the ring's occupancy was delivered at a
*uniform* nominal frame rate. A real backend does not oblige: WASAPI shared-mode
loopback under Windows timer coalescing hands several packets over in a
faster-than-realtime burst, so the occupancy spans pushes whose wall-clock spacing
is shorter than `frames × ns_per_frame`. Spreading the occupancy uniformly back
from `last_push_ns` across those bursts places the hop's newest frame *earlier*
than it truly arrived — the round-5 SUBSET-BREAK (see *Field reconciliation —
fifth round*). The per-push mapping anchors each frame inside the push that
actually carried it, so it is immune to the cadence *between* pushes and tracks the
same capture-delivery instant the raw-ring/tee mapping does. Synthesized-silence
hops (capture starved) carry no delivered frame and keep the wall-clock, which is
correct — they represent "now, nothing captured". The render overlay already treats
`timestamp_ns` as capture time (`feature_age = now − timestamp_ns` is "capture →
now"), so this also makes the displayed feature age honest.

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

> **Publish clock changed since this run.** This `emit → publish` column was
> measured with the *pre-fix* publish clock — the DSP's wall-clock at the moment
> it pulled the hop. That stamp is `now ≥ delivery`: it sits *above* the hop's
> capture-delivery time by the ring residence, so it can only over-state
> `emit → publish`, never under-state it. The clock is now anchored on capture
> delivery (see *Publish clock* and *Field reconciliation — third round*); a fresh
> run's `emit → publish` will read a few ms lower than this table and satisfies the
> subset invariant against `emit → raw-arrival` by construction. Treat the 81.3 ms
> median as an upper-bound intermediate, not the endpoint's delivery-anchored
> number.

Reading:

- Detection is airtight: 25/25 matched, no drops, no synthesized hops, and
  the feature bus adds ~1.4 ms (publish → observe).
- The ~39.9 ms output delay is the probe player's own render buffering
  (cpal's default WASAPI output stream) plus the click's intra-buffer
  offset — it is NOT part of scia's capture path; a real player's audio is
  already in the mix.
- **Playback → publish decomposition.** Estimated playback → publish ≈ 41 ms
  median (81.3 − 39.9) on the pre-fix clock, vs ~15–20 ms expected from the P1
  cadence (10 ms loopback packet + 5.3 ms hop + polls). The raw-ring follow-up
  below decomposes this by measuring capture transport directly; its first two
  field runs mis-measured (a drain-timestamp poll-jitter bug, then a residual
  constant bias), and the third run finally read clean (backlog max 0 frames,
  every click `ncc` 1.000, `emit → raw-arrival` ≈ 108.7 ms with a ±0.15 ms
  spread — see *Field reconciliation — third round*). That clean raw-arrival sat
  *above* the pre-fix `emit → publish`, which is impossible for two honest clocks
  and is what motivated putting both modes on one capture-delivery clock. A
  back-to-back dual run on the delivery-anchored clock is owed before the ≤ 33 ms
  US-PERF-1 criterion is scored.

## Raw-ring mode (`--raw-ring`)

The follow-up the open question asks for. It answers a narrower question than the
main probe — *how much of the emit → publish interval is capture transport,
before scia's hop grid touches the samples* — and answers it without the
256-frame hop quantization that blurs the main probe's `emit → publish` number.

**How it differs.** In raw-ring mode the probe does **not** run the engine or the
DSP hop grid. It opens the same capture backend the engine uses
(`CaptureBackend::open`) directly into a probe-local sample ring, then drains
that ring on a 1 ms poll off-thread, accumulating the exact interleaved samples
(down-mixed to mono) with each drain anchored to the capture-delivery clock — the
time the samples entered the ring, not the time the probe read them out (see
*Timestamp bookkeeping*). For each emitted click
it runs a **normalized matched-filter cross-correlation** of the captured stream
against the known click template (a rectangular burst of `--click-ms` at
`--amp`) over a search window one click spacing wide centered on the click's
`emit_ns` (`emit_ns − spacing/2` … `emit_ns + spacing/2`), and takes the
correlation argmax as the click's **leading edge in the raw stream**. Resolution
is one sample (≈ 0.02 ms at 48 kHz), not one hop. The normalized score is bounded
to `[−1, 1]` by construction (Cauchy–Schwarz): each offset's window sum and window
energy are computed from the same samples, so the ratio cannot exceed 1. A score
`> 1` would therefore be a numerical artifact — it was one, before this round: a
running window energy carried across offsets accumulated the rounding error of a
loud burst, and in a later low-energy window that residual dwarfed the true energy
and let the normalized score blow past 1 and win the peak, planting a spurious
late arrival. The energy is now recomputed exactly per offset (and the score
clamped to 1 as a last-ulp guard), so `ncc > 1` can no longer be reported. (The
window reaches back half a
spacing as well as forward because the synthetic backend stamps `emit_ns` just
before pushing a chunk, so on the near-zero-transport synthetic path a sample's
continuous-capture time can fall a few ms *before* the emission; a neighbour
click is a full spacing away and cannot be picked up, and real transport always
lands to the right of `emit_ns`.)

**Timestamp bookkeeping.** A drain is anchored to *when its samples entered the
ring*, not *when the probe read them out*. The frames a poll drains entered the
ring when a capture callback delivered them; the newest was delivered by the most
recent push, whose time the sink records as `last_push_ns` (on the ring epoch).
The probe reads `last_push_ns` just before the drain and places the drain's oldest
frame at `anchor − frames × ns_per_frame`, stepping one frame-period per
frame — so the newest sits about one frame-period before its delivery and the
oldest about `frames` frame-periods before it. The ring clock is the same epoch
`FeatureSnapshot.timestamp_ns` and the click player's `emit_ns` use, so
`emit → raw-arrival` is a difference on one clock. The residual error is a single
*push* interval of jitter on the anchor (sub-millisecond quanta are negligible).

`anchor` is `last_push_ns` only when the drain has caught up to the writer.
`last_push_ns` is the delivery time of the writer's *newest* frame; if a steady
backlog of undrained frames remains in the ring after a drain, the newest frame
that drain actually ended on is older than the writer's newest by exactly the
backlog, and anchoring on `last_push_ns` shifts every reconstructed time late by
that constant. The probe therefore reads the writer's cumulative `pushed_frames`
alongside `last_push_ns` and corrects the anchor to
`last_push_ns − backlog × ns_per_frame`, where `backlog` is the frames the writer
had delivered that the drain did not pop. With the unbounded drain the ring
empties every poll, so the backlog is ~0 and the correction vanishes; it only
bites if the drain ever falls behind (a bounded chunk, correlation work between
wakes, or a push landing mid-drain). The probe now also **reports the observed
steady-state backlog** (worst and last, in frames and ms) so a hardware run can
read directly whether the drain kept up rather than inferring it.

Anchoring on the *delivery* clock, not the probe's own poll-read clock, is what
keeps raw-arrival a true subset of the publish path. A sample enters the ring
strictly before any hop that carries it can be gathered and published, so
`emit → raw-arrival ≤ emit → publish` must hold. Anchoring instead on the poll's
read time would fold the probe's own drain-poll latency into every reconstructed
time — and that latency balloons whenever the OS coalesces the probe's short poll
sleeps into a coarse timer (the same ~15.6 ms Windows-timer effect the observer
hits, noted above). That is a probe artifact, not capture transport; loading it
into raw-arrival can push it *past* `emit → publish`, breaking the subset
invariant (see *Field reconciliation* below).

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

**Field reconciliation.** An early raw-ring run on the Windows/Realtek endpoint
reported `emit → raw-arrival ≈ 109.6 ms` median while a same-session normal run
reported `emit → publish ≈ 81.3 ms`. That ordering is impossible: a sample enters
the ring before any hop that carries it is published, so raw-arrival is a strict
subset of publish and must be the *smaller* number. The cause was the drain
timestamp model, not the pipeline. The probe anchored each drain to its poll-read
clock (`now_ns()` at the drain) instead of the capture-delivery clock; under the
coalesced Windows timer the poll fired well after the callback that delivered the
newest frame, and that ~28 ms poll-to-delivery gap was folded into every
reconstructed sample time, inflating raw-arrival above publish. The probe now
anchors on `last_push_ns` (the delivery clock, see *Timestamp bookkeeping*), so
raw-arrival measures ring entry and the subset invariant holds by construction.

> **Superseded:** the `109.6 ms` raw-ring figure above is a *pre-correction*
> measurement — an instrumentation artifact, not an endpoint property. Do not
> compare it against the `81.3 ms` normal-run number or use it to score
> US-PERF-1. The `81.3 ms` normal-run table stands; only the raw-ring figure was
> affected. A corrected raw-ring run lands here later via the orchestrator.

**Field reconciliation — second round.** The delivery-clock anchor above fixed
the *jitter*: a follow-up raw-ring run held `emit → raw-arrival` to an extremely
tight spread (~0.4 ms across the clicks), confirming the poll-to-delivery gap no
longer leaks into the reconstruction. But a **constant ~27.5 ms late bias**
remained — the run centred near `108.7 ms` median, still above the same session's
`emit → publish`, still violating the subset invariant. `108.7 ms` is therefore a
second *biased intermediate*, not an endpoint property; do not score US-PERF-1
against it either. That run also showed one outlier click reading a normalized
correlation `> 1` (the energy-drift artifact fixed above), which returned a
spuriously late arrival for that click.

Two changes this round address the constant bias at its most likely source and
make the next run self-diagnosing:

- **Occupancy-corrected anchor.** A constant late offset is exactly the signature
  of a steady ring backlog: if a drain runs a fixed distance behind the writer,
  the frame it ends on is a fixed number of frames older than `last_push_ns`, so
  every reconstructed time is late by that fixed span. The anchor now subtracts
  the measured backlog (see *Timestamp bookkeeping*), which cancels such an offset
  exactly. A deliberately under-draining regression (`crates/core/tests`) that
  builds a constant backlog now asserts the reconstruction stays pinned to
  delivery — and that the *uncorrected* anchor drifts late by exactly the backlog
  span, so the class of bug cannot silently return.
- **Backlog readout.** The probe reports the observed steady-state backlog each
  run. This is the discriminating measurement the next hardware run needs: a
  reading near zero means the unbounded drain kept up and the residual constant
  lives *outside* the drain reconstruction (candidates then narrow to a real
  capture/output-path latency difference between the two modes, or an
  under-measurement on the normal-mode publish side); a nonzero reading localizes
  it to the drain and the correction above will have removed it.

A reading of the reconstruction arithmetic (and an offline model of the
writer/drain cadence) indicates the *current* unbounded per-poll drain empties
the ring each wake and so should carry ~0 backlog — meaning the occupancy term,
while provably correct and the right anchor, may not by itself account for the
full 27.5 ms on this endpoint. That is precisely why the backlog readout was
added rather than assumed: the next corrected raw-ring run should be read with the
backlog line in hand before attributing the residual. The `81.3 ms` normal-run
table still stands; a third raw-ring run lands here later via the orchestrator.

**Field reconciliation — third round.** The third raw-ring run read **clean**:
`emit → raw-arrival` ≈ `108.7 ms` median with a `±0.15 ms` spread, every click
`ncc` exactly `1.000`, and **ring backlog max 0 frames**. The backlog readout is
the discriminating measurement: at zero, the unbounded drain kept up, the
occupancy correction is a no-op, and the reconstruction is not the source of a
residual bias. Raw-arrival is now a trustworthy direct measurement of capture
delivery: from the click's emission (at the output callback) to its samples
entering scia's ring, ≈ 108.7 ms on this endpoint.

That reading forced the real diagnosis. Raw-arrival (≈ 108.7 ms) sat *above* the
same-endpoint normal-run `emit → publish` (≈ 81.3 ms) by ~27.4 ms — impossible for
two honest clocks, since a sample enters the ring **before** any hop that carries
it is published, so raw-arrival must be the *smaller* number. The suspect this
round was the **normal-mode publish clock**, on the hypothesis that it read the
click ~27 ms early. Reading the code refutes an *early* stamp: the pre-fix
`timestamp_ns` was the DSP's `now_ns()` at hop pull, read only *after* the hop's
frames were already buffered, so it is `now ≥ delivery` — it can only sit *above*
the hop's capture-delivery time (by the ring residence), never below it. There is
no `epoch + index/rate` gapless-sample model in the normal path to read early from
a startup gap. So the pre-fix publish clock cannot explain a 27 ms *inversion*; if
anything it inflates `emit → publish`, which would only make the true
delivery-anchored number *smaller* and the inversion *wider*.

The honest resolution is therefore twofold:

- **The two modes were measuring against different reference points, and now do
  not.** Normal mode stamped the DSP's *processing* wall-clock; raw-ring anchored
  on *capture delivery*. Those are not the same instant, and nothing said so. Both
  modes are now anchored on the one capture-delivery clock (the hop's newest frame
  at `last_push_ns` minus ring occupancy — see *Publish clock*), so on a single
  run `emit → raw-arrival ≤ emit → publish ≤ emit → raw-arrival + one hop` holds by
  construction. The probe reports state their reference points explicitly.
- **The ~27.4 ms is a cross-run difference, not a clock bug in either mode.** The
  `81.3 ms` and `108.7 ms` figures come from **separate** probe processes — the
  normal run and the raw-ring run are not simultaneous, and each opens its own
  loopback capture on a shared-mode WASAPI endpoint whose buffering need not match
  between two independent opens. With the delivery-anchored clock, the two modes
  can no longer invert *within one run*; a residual gap between a **back-to-back**
  normal and raw-ring pair on the delivery clock would then be a genuine
  capture-transport property to chase, not an instrumentation artifact. That
  back-to-back pair is the confirmation run now owed.

What this means for the latency story: `emit → raw-arrival` ≈ 108.7 ms is capture
transport from the *output callback*, and it includes the probe player's own
render buffering (`output delay (cb→play)` ≈ 39.9 ms), which is **not** scia's
path — a real player's audio is already in the mix. Subtracting it leaves
**playback → ring ≈ 68.8 ms** of loopback/endpoint capture buffering on this
endpoint, well above the ~10–15 ms a bare packet cadence suggests; that endpoint
buffering, not scia's hop grid, dominates the pre-pixel budget here. scia's own
contribution above ring entry is small and bounded: up to one hop of gather
(≤ 5.33 ms) into `emit → publish`, then ~1 ms of feature-bus poll into
`emit → observe`, then up to one frame interval of render/present (~16.7 ms at
60 fps) that this probe does not measure. The ≤ 33 ms US-PERF-1 budget is about
scia's audio→pixel contribution; the large endpoint-capture term is upstream of
scia and rides in front of every visualiser on this endpoint equally. Scoring
US-PERF-1 still needs the back-to-back delivery-clock run to pin
`emit → publish − emit → raw-arrival` (the hop-gather term) directly.

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

## Dual-tap mode (`--dual-tap`)

The final P7 instrument, and what settles the cross-mode constant the three
reconciliation rounds above could not. Every prior figure came from a **separate
process**: the DSP thread consumes the sample ring, so raw-ring mode (which drains
the ring itself) and normal mode (which lets the DSP drain it) are mutually
exclusive — they can never run at once. That is exactly why a rock-constant
~27–29 ms gap between a raw-ring run's `emit → raw-arrival` (≈ 108.7 ms) and a
separate normal run's `emit → publish` (≈ 79.3 ms) could not be told apart from an
instrumentation artifact: the two numbers were two different WASAPI loopback opens
on a shared-mode endpoint, and nothing forced them onto one capture-delivery
instant.

Dual-tap removes the separateness. It runs the **full engine** — the DSP drains
the primary ring and publishes hops exactly as in normal mode — and adds a **tee**
at the capture sink: on each push, the delivered packet is *also* copied into a
second lightweight ring, together with that push's delivery time (`last_push_ns`)
and running frame count. The DSP never sees the tee; the probe reads it. So one
running engine, one capture stream, one clock, yields **both** measurements from
the **same** clicks:

- `emit → publish` — the hop that carries each click, detected off the feature
  stream, delivery-anchored exactly as shipped.
- `emit → raw-arrival` — each click's samples entering scia's ring, by
  cross-correlation on the teed raw samples.

**Exact per-push mapping.** Because every push logs its own `(delivery_ns,
frames, cumulative_frames)`, the probe maps a matched click's sample index to its
capture-delivery time *exactly* — one timeline segment per push, no occupancy or
backlog inference at all. This also retires round 2's open question about the
`last_push_ns`/commit ordering: the delivery time and the frame count are captured
together at the push, not read back from separate atomics and reconciled later.

**The invariant it checks, in one run.** A sample enters the ring strictly before
any hop that carries it can be gathered and published, and the hop grid adds at
most one 256-frame hop of gather, so

```
emit → raw-arrival  ≤  emit → publish  ≤  emit → raw-arrival + one hop (5.33 ms)
```

must hold **per click and in summary**. Both ends are now on the one
capture-delivery clock in one process, so this holds *by construction*. The probe
prints the verdict explicitly (`subset N/N · within-one-hop N/N`) and marks each
click `ok` / `SUBSET-BREAK` / `over-one-hop`.

**How to run.**

```
# Real machine — full engine + loopback capture + output click player, teed:
latency_probe --dual-tap --clicks 25
latency_probe --dual-tap --output-device "NAME"   # pick the output endpoint

# No audio hardware — the CI-testable path (synthetic click backend):
latency_probe --dual-tap --synthetic --clicks 12
```

The report prints the two intervals and their per-click hop-gather delta (Δ) as
percentiles, the invariant verdict, a per-click table, and the tee's dropped-push
count (`0` when the 1 ms drain kept up; nonzero means the raw-arrival numbers are
suspect).

**Reading the verdict — the two branches of the dichotomy.** The on-hardware
command is `latency_probe --dual-tap --clicks 25` on the endpoint under test. Read
the `subset` count first:

- **subset N/N holds** (every click `raw-arrival ≤ publish`, and the Δ column sits
  in `0 … 5.33 ms`): the two modes' models are both honest, and the ~27 ms that
  separated the *earlier separate runs* was a genuine **per-open capture-transport
  difference** between two independent shared-mode loopback opens — not a clock
  bug. Dual-tap has shown the invariant holding inside one process, so the residual
  is a real property of the endpoint's buffering across opens, and the doc records
  the per-open difference as real. In this branch `Δ` (median) is the true
  hop-gather term to subtract when scoring US-PERF-1: `emit → publish` is
  `raw-arrival` plus that Δ, both from one run.
- **a SUBSET-BREAK appears** (some click reads `raw-arrival` *above* `publish`
  within the one run): impossible for two honest clocks, so one mode's model still
  lies — and now the defect is **localizable**, because both figures came from the
  same push stream on the same clock. Compare the offending click's teed
  raw-arrival reconstruction against the hop's delivery stamp for the same frames;
  the break is in whichever of the two derives the wrong time from the shared
  `last_push_ns`/occupancy data. An `over-one-hop` click (publish more than one hop
  above raw-arrival) with `subset` still intact is the softer case: a detection
  landing a hop late because a partial hop's peak missed the threshold, not a clock
  bug — it does not break the dichotomy.

**Synthetic self-test.** `--dual-tap --synthetic` (and the
`synthetic_dual_tap_invariant_holds_in_one_run` regression in
`crates/core/tests/latency.rs`) drives the whole instrument with no hardware: the
synthetic click backend feeds the engine, the tee reconstructs raw-arrival, the
observer reads publish, and the test asserts `subset N/N`, `within-one-hop N/N`,
and that each Δ lies in `0 … one hop`. On the synthetic path `emit → publish`
collapses to ~0 (the emit stamp is taken at the chunk push, which is also the
delivery instant) and `emit → raw-arrival` is a few milliseconds *negative* (the
continuous-capture sample time precedes the pre-push emit stamp), so Δ is the pure
hop-gather term — the invariant holds with room to spare, proving the tee, the
exact mapping, and the joined measurement end-to-end. The synthetic backend
delivers one 256-frame chunk per real-time sleep, so its ring never holds an
occupancy spanning several non-uniform pushes — which is exactly why the synthetic
path did **not** reproduce the field's break (see below): the defect lived in the
delivery cadence, not the pipeline logic.

**Field reconciliation — fifth round (the break, localized and fixed).** The first
on-hardware dual-tap run (Windows shared-mode, 48 kHz 2 ch, 10 ms period, hop 256;
25 clicks) removed the last ambiguity by measuring both legs from the same clicks
in one process on one clock:

```
25 clicks · publish-matched 25 · raw-matched 25 · both 25 · tee dropped-pushes 0
emit → raw-arrival  median 108.72 ms  (spread 108.55–108.91)
emit → publish      median  79.33 ms  (spread  79.09– 79.44)
per-click Δ = publish − raw-arrival = −29.38 ms constant (−29.60 … −29.30)
subset invariant 0/25   (raw-arrival sits ABOVE publish for the same click)
engine: pushes 1324 (480-frame/10 ms packets) · hops 2480 · dropped 0 · xruns 1
```

A **SUBSET-BREAK**: raw-arrival above publish for the same click in the same run is
impossible for two honest clocks, since a sample enters the ring strictly before
any hop that carries it is published. And now it is *localizable*, because both
figures come from one push stream. The raw-arrival leg maps a click's leading edge
to the **exact** delivery time of the push that carried it (the tee's per-push log,
no inference) — it is trustworthy (the round-3 finding stands). So the **publish
leg lied, reading ~29.4 ms early**.

The mechanism is the publish clock's **occupancy inference**. The pre-fix stamp was
`last_push_ns − ring_occupancy × ns_per_frame`, which assumes the ring's occupancy
was delivered at a uniform nominal frame rate. On this endpoint it is not: WASAPI
shared-mode loopback under timer coalescing hands several 480-frame packets over in
a faster-than-realtime burst, so the frames resident *newer* than the hop being
stamped were delivered in far less wall-time than `frames × ns_per_frame`. Spreading
the occupancy uniformly back from `last_push_ns` across that burst placed the hop's
newest frame ≈ 1410 frames (`−29.38 ms ≈ 1410 / 48 000`) before it truly arrived —
a tight constant because the coalesced cadence is stable. The synthetic backend
(one 256-frame chunk per realtime sleep) never builds such an occupancy, so it never
reproduced the break; a deterministic reproduction of the bursty cadence lives in
`crates/core/src/dsp.rs::tests::bursty_delivery_breaks_uniform_stamp_but_exact_mapping_holds`,
which fails the subset invariant under the old model and holds under the fix.

**The fix.** The DSP no longer infers. The primary capture ring now carries a
wait-free **per-push delivery log** (`(frames, delivery_ns, cumulative_frames)` per
push — the same record the tee logs), and the DSP maps a hop's newest frame to the
delivery time of the push that actually carried it — exact per push, immune to the
cadence between pushes (see *Publish clock*). This is the **honest side made
symmetric**: the raw-arrival leg was already exact per push; the publish leg now is
too. The tee's raw-arrival reconstruction was not touched.

**Corrected interpretation — which absolute numbers stand.**

- `emit → raw-arrival` ≈ **108.7 ms** stands as the endpoint's capture transport
  from the output callback (it was always the exact-mapped leg). Of it,
  ≈ 39.9 ms is the probe player's own render buffering (`output delay (cb→play)`,
  **not** scia's path) and ≈ 68.8 ms is loopback/endpoint capture buffering upstream
  of scia — the dominant pre-pixel term on this endpoint, in front of every
  visualiser equally.
- The pre-fix `emit → publish` figures — the `81.3 ms` first-run table *and* this
  round's `79.33 ms` — are **both retired**: the first was the DSP's processing
  wall-clock, the second the occupancy inference, and neither is the hop's true
  capture-delivery time. Do not score US-PERF-1 against either.
- On a fresh delivery-anchored dual-tap run the subset invariant holds by
  construction: `emit → raw-arrival ≤ emit → publish ≤ emit → raw-arrival + one hop`.
  So the corrected `emit → publish` is `raw-arrival + Δ` with the hop-gather
  **Δ ∈ [0, 5.33 ms]** measured directly per click — expected ≈ 108.7 … 114.0 ms on
  this endpoint. **US-PERF-1 scoring uses that Δ** (median) as scia's own gather
  contribution above ring entry, plus ~1 ms feature-bus poll into `emit → observe`
  and up to one render frame (~16.7 ms at 60 fps) this probe does not measure; the
  large endpoint-capture term is upstream of scia and not scia's audio→pixel budget.
  The confirming on-hardware run: `latency_probe --dual-tap --clicks 25` must now
  read `subset 25/25` with every `Δ ∈ [0, 5.33 ms]`.
