# `quality/` — the scene-quality iteration corpus

Objective inputs and records for the scene-quality loop, driven by the
`scia-harness` binary (`crates/harness`).

## Layout

- `corpus/manifest.toml` — the golden-clip catalogue, and the **only** committed
  record of a clip. One `[[clip]]` table per clip: `id`, `genre`, `path`
  (relative to `corpus/`), `duration_s`, `sha256` (the feature-clip file hash),
  `notes`, and a `generated` flag.
  - `generated = false` — a clip whose file is not regenerable in-repo (a real
    recorded fixture, or a clip rendered offline from downloaded source audio);
    `corpus verify` hashes the file on disk. A missing file is reported as a
    per-clip `FAIL` ("re-render from source, see manifest provenance"), not a
    hard error — the other clips still verify.
  - `generated = true` — a deterministic synthetic clip; the file is regenerated
    on demand and `corpus verify` regenerates it and compares the hash.
    Generated-clip hashes are stable for regeneration on the same toolchain and
    platform.
  - **Provenance** (optional, for clips rendered from downloaded source audio):
    `title`, `artist`, `license` (e.g. `CC BY 4.0`, `Public Domain`),
    `source_url`, `audio_sha256` (hash of the exact downloaded source file,
    pre-transcode), `segment_start_s` / `segment_len_s` (the slice cut from the
    source), `gain_db`, and `render_cmd` (a one-liner of the exact reproduction
    pipeline). These fields are omitted entirely when unset, so a pure-synth
    entry round-trips byte-for-byte.
- `corpus/clips/` — clip files (feature-stream NDJSON). **Nothing here is
  committed**: every clip file is git-ignored (see `clips/.gitignore`). Clips are
  local artifacts, reproducible from the manifest — synthetic ones via
  `corpus synth`, rendered ones from their source audio + provenance.
- `corpus/sources/` — downloaded source audio a rendered clip is cut from. Also
  fully git-ignored (see `sources/.gitignore`); re-fetched from each entry's
  `source_url` and identified by `audio_sha256`.
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

## How a clip is born (rendered from source audio)

Rendered clips come from freely-licensed source audio, offline. Nothing binary
is committed — the manifest entry is the record, and the pipeline is
reproducible from it:

1. **Download** the source audio from its page/file `source_url` into
   `corpus/sources/` (git-ignored). Hash the exact downloaded file →
   `audio_sha256`, and record `title`, `artist`, `license`.
2. **Transcode** to a 48 kHz WAV (the feature pipeline's expected rate), e.g.
   `ffmpeg -i sources/<file> -ar 48000 -ac 2 sources/<file>.48k.wav`.
3. **Segment**: pick the slice to use — `segment_start_s` / `segment_len_s`
   (e.g. `ffmpeg -ss <start> -t <len> ...`).
4. **Render** the feature clip offline into `corpus/clips/` with
   `scia --from-file <wav> --gain-db <gain_db> ...` (also git-ignored). Capture
   the exact command shape used into `render_cmd`.
5. **Register**: add a `[[clip]]` entry with `generated = false`, the
   feature-clip file `sha256`, and the full provenance block above. Confirm with
   `scia-harness corpus verify`.

Recording a real captured genre clip instead: capture a feature stream to a file
(see the main app's `--output`), drop it under `corpus/clips/`, and add a
`[[clip]]` entry with `generated = false` and the file's `sha256` (provenance
fields left empty).
