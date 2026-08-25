# `quality/` — the scene-quality iteration corpus

Objective inputs and records for the scene-quality loop, driven by the
`scia-harness` binary (`crates/harness`).

## Layout

- `corpus/manifest.toml` — the golden-clip catalogue. One `[[clip]]` table per
  clip: `id`, `genre`, `path` (relative to `corpus/`), `duration_s`, `sha256`,
  `notes`, and a `generated` flag.
  - `generated = false` — a committed fixture; `corpus verify` hashes the file.
  - `generated = true` — a deterministic synthetic clip too large to commit; the
    file is regenerated on demand and `corpus verify` regenerates it and compares
    the hash. Generated-clip hashes are stable for regeneration on the same
    toolchain and platform.
- `corpus/clips/` — clip files (feature-stream NDJSON). Generated clips are
  git-ignored (see `clips/.gitignore`); real recorded genre clips are committed.
- `preference-log.toml` — structured human verdicts steering calibration,
  seeded with the standing maintainer verdicts and appended to by
  `scia-harness verdict`.
- `envelopes/<scene>.toml` — per-scene metric envelopes, materialised by
  `scia-harness freeze` from an approved run (not committed until a scene is
  frozen).

## Common commands

```
scia-harness corpus synth                 # (re)generate the synthetic clip(s)
scia-harness corpus verify                # check every manifest entry's hash
scia-harness run --clip <id|path> --scene <id> [--preset p] [--set k=v] [--out d]
scia-harness ab --clip <id> --scene <s> --preset-a p --preset-b p
scia-harness verdict --scene <s> --clip <c> --winner a|b|neither --why "..."
scia-harness freeze --scene <id> --from <metrics.json> [--margin m]
```

Recording a real genre clip later: capture a feature stream to a file (see the
main app's `--output`), drop it under `corpus/clips/`, and add a `[[clip]]`
entry with `generated = false` and the file's `sha256`.
