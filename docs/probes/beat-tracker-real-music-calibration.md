# Beat tracker — real-music calibration

Five 90-second live-capture runs of the `beat_probe` example against streamed
music on a Windows desktop (WASAPI loopback), interleaved with three fixes.
Two source classes: a four-on-the-floor house track (~124 BPM) and an ambient
piece (no beat).

## Verdict

The tracker as merged (coasting + tempo-memory warm re-lock) publishes a tempo
for **93.2%** of hops on the house track — candidate 124 BPM on **73 of 73**
induction passes, median confidence 0.437 — with the residual gap being the
honest cold-start lock-in. The ambient run publishes **0.0%** with confidence
never above 0.060. No lock/gate constants needed retuning.

## What the runs found, in order

1. **Run 1 (house, mirror probe):** the in-engine tracker locked 124.1 BPM and
   held ~78% of the run, but dropped to tempo 0 twice during breakdowns. The
   probe's own mirror tracker skipped 32% of hops (a `latest()`-polling gap)
   and under-reported every diagnostic column — mirror data discarded.
2. **Run 2 (ambient, mirror probe):** honest rejection (no lock), but numbers
   biased by the same mirror gap — re-run later on the fixed probe.
3. **Fixes:** lock coasting (hold tempo/phase up to 6 s of weak evidence,
   true silence cuts within ~1.5 s); probe reads the real in-thread tracker
   via `Engine::beat_debug()`; then tempo-memory warm re-lock (30 s window,
   ±3% band, re-lock at the lock-off threshold with hard phase align).
4. **Run 3 (house, fixed probe):** comb correct on 73/73 passes; coast bridged
   the first breakdown; publishing 78.8%. Remaining defect: after coast
   expiry the cold-lock threshold (0.38) was unreachable while the kurtosis
   gate held real-music confidence at 0.07–0.27 — motivating the warm re-lock.
5. **Runs 4/5 (house/ambient, final):** the verdict numbers above.

## Measured real-music statistics (for future tuning)

| signal | ODF kurtosis (min/med/max) | confidence (min/med/max) |
|---|---|---|
| house, four-on-floor | 7.6 / 11.4 / 26.0 | 0.00 / 0.44 / 0.53 |
| ambient | 3.0 / 4.2 / 9.7 | 0.00 / 0.00 / 0.06 |

The kurtosis classes separate cleanly at the medians and overlap only at the
extremes; the smoothstep(6, 12) gate needs no adjustment for these classes.
Soft/legato *rhythmic* material (low-transient ODF) remains uncharacterized —
re-run the probe against it before trusting the gate there.
