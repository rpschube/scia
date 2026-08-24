# P4 — Windows Terminal sixel frame rate

Probe, not production. Goal: measure whether WT's experimental sixel support
can carry an animation tier, and at what frame rate, before promising
anything (US-TUI-3).

## Setup

- Windows Terminal 1.24.11911.0 (sixel since 1.22), WSL shell, probe script
  emitting alternating 16-colour striped sixel frames at the home position,
  drain-confirmed with a DSR round-trip; run plain and inside
  synchronized-output (mode 2026) brackets.

## Results

| Frame size | Frames | Plain | Sync 2026 |
|---|---:|---:|---:|
| 800×400 px | 90 | 0.63 s → **142.8 fps** (20.2 MB/s) | 0.63 s → 143.7 fps |
| 1400×700 px | 60 | 0.88 s → **68.3 fps** (17.3 MB/s) | 0.91 s → 66.3 fps |

Capability replies: DA1 `?61;4;…c` (attr 4 = sixel present), DECRQM 2026 →
`2$y` (recognized), cell size 20×10 px. No tearing or partial frames
reported at either size.

## Reading

- WT drains ~17–20 MB/s of sixel and sustains **~68 fps at a 1400×700
  canvas** — far above the "10–30 fps bonus mode" planning assumption. The
  sixel presenter is a serious tier on WT ≥ 1.24, not a curiosity.
- Synchronized output adds no measurable cost; keep frames bracketed.
- Caveats: 16 colours and flat stripes compress the parser's work; scenes
  with per-frame quantization and busier imagery will be slower — the
  presenter card should re-measure with real scene output before locking
  default tiers.
