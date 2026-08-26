# Feature stream

`scia` can run headless and emit its per-hop analysis as a machine-readable
stream instead of drawing the terminal UI, and it can render its UI from such a
stream produced elsewhere. This is the seam for external visualizers, bridges
and the supported split where capture runs on one host and rendering on another.

Each stream frame is one hop of the analysis pipeline: the same
`FeatureSnapshot` contract every built-in renderer consumes, projected onto a
stable wire form. Two encodings carry it — line-delimited JSON and a compact
length-prefixed binary — and both are versioned so a consumer can reject a
frame it does not understand.

## Command line

```text
scia --output json                    # NDJSON frames to stdout, no UI
scia --output binary                  # binary frames to stdout, no UI
scia --output json --listen ADDR      # serve frames to every client on a TCP socket
scia --output json --rate 30          # cap the frame cadence (frames per second)
scia --demo --output json             # drive the stream from the synthetic feed
scia --output binary > clip.bin       # record a clip to a file
scia --from-file take.wav --output binary > clip.bin  # render a file offline into a clip
scia --input ADDR                     # render the full UI from a remote stream
scia --input clip.bin                 # render the full UI by replaying a clip file
scia --input clip.bin --input-loop    # ...looping the clip for extended listening
```

- `--output <json|binary>` runs headless: no scene, chrome or overlay is drawn.
  Frames go to standard output, or to a socket when `--listen` is given. It
  works with `--demo` (the built-in synthetic feed, needing no audio hardware)
  or with live capture.
- `--listen <ADDR>` binds a TCP listener at `ADDR` (e.g. `127.0.0.1:9000`, or
  `0.0.0.0:9000` to accept from the local network). Every connected client
  receives the stream from the moment it connects; a binary client receives the
  one-time header first. Only valid with `--output`.
- `--rate <N>` sets the target frames per second (default 60, clamped to
  `1..=1000`). Only valid with `--output`. See [Idle cadence](#idle-cadence).
- `--input <ADDR|PATH>` renders the full UI — scenes, chrome, overlays — from a
  feature stream instead of capturing local audio. The argument is dispatched by
  shape: a **socket address** (`ip:port` or `host:port`) connects to a remote
  `scia --output --listen`, as before; otherwise an **existing file path**
  replays a recorded clip from disk; anything else is an error naming both
  interpretations. The encoding is auto-detected either way. A dropped socket
  connection is retried automatically with a bounded backoff while the UI shows
  its normal reconnecting/quiet state; it never freezes or exits on a blip.
- `--input-loop` (with a file `--input` only) seamlessly restarts the clip at
  end of file, for extended A/B listening. It is an error alongside a socket
  `--input`.

- `--from-file <PATH>` renders an audio **file** through the exact live DSP
  chain into the feature stream instead of capturing live audio — faster than
  realtime and **bit-for-bit deterministic** (the same file and flags produce
  byte-identical output). Requires `--output`; mutually exclusive with
  `--input`, `--listen` and every live-capture flag. See
  [Offline render](#offline-render-from-a-file).
- `--gain-db <DB>` (with `--from-file` only) applies a linear gain, in decibels,
  to the input samples before the DSP, so corpus prep can loudness-normalize
  externally-measured files without re-encoding. Default `0` (no change).

`--output` and `--input` are mutually exclusive, and each conflicts with the
flags that do not apply to it (`--output` with the UI-only flags; `--input` with
the local-capture flags). `--listen` and `--rate` are only accepted with
`--output`; `--input-loop` only with `--input`; `--gain-db` only with
`--from-file`. `--rate` is rejected with `--from-file` (offline mode emits one
frame per DSP hop, not a subsampled rate).

## Offline render from a file

`scia --from-file <wav> --output <json|binary>` renders an audio file through
the same DSP pipeline the live capture path runs, writing the feature stream to
stdout (or a clip file by redirection). It never starts the realtime engine
threads and is not paced by a clock, so it runs as fast as the CPU allows.

```text
scia --from-file take.wav --output binary > clip.bin   # record a clip from a file
scia --from-file take.wav --output json                # NDJSON frames to stdout
scia --from-file take.wav --gain-db -6 --output binary > quiet.bin  # attenuated
scia --input clip.bin                                  # replay the rendered clip in the UI
```

- **Deterministic.** The same file and the same flags produce byte-identical
  output. Timestamps are the sample clock (the hop index times the hop period),
  never wall time, so nothing about the machine or the moment of the run leaks
  into the bytes. This is what lets the scene-quality golden corpus be
  regenerated from source audio: the manifest stores the source plus hashes, no
  committed binaries, and no loopback jitter.
- **One frame per DSP hop.** Offline emits a frame for **every** analysis hop
  (the native ~187 fps hop cadence at 48 kHz), richer than the live `--output`
  default of 60 fps subsampling. `--rate` therefore has no meaning offline and
  is rejected.
- **Capture-period chunking.** The input is fed to the pipeline on the same
  period boundaries a live capture backend delivers, so an offline feature
  stream is comparable to a live one — the same hop/window cadence, just not
  paced by a clock.
- **Input format.** WAV only: 16- or 24-bit PCM or 32-bit IEEE float, **48000
  Hz**, mono or stereo (a mono file is duplicated to stereo so the chain always
  sees the stereo shape live capture delivers). Anything else is a clear error
  naming the constraint; transcode with an external tool first.

The output is exactly the `--output` wire form below, so a rendered clip is a
valid input for `scia --input` and `scia-harness run` unchanged.

## Recording and replaying a clip

The clip format is exactly the `--output binary` wire form (the framing below)
written to a file — no separate container. Record by redirecting the binary
stream, and replay by pointing `--input` at the file:

```text
scia --output binary > clip.bin          # record (live capture)
scia --demo --output binary > clip.bin   # record the synthetic feed (no audio hardware)
scia --input clip.bin                     # replay it in the full UI
scia --input clip.bin --input-loop        # replay on a loop
```

Replay is paced by the clip's own inter-frame timestamps, so it renders at the
cadence it was recorded — live, not as fast as the file reads — including the
idle keepalive stretches. Each frame's `timestamp_ns` is restamped to the local
receive clock on replay (as for a socket `--input`) so the UI's frame-age reads
correctly; the `generation` counter is preserved. At end of file the scene
settles to idle on the quiet keepalive and the UI shows the same disconnected
state as a dropped socket; with `--input-loop` the clip instead restarts
seamlessly. A file that is not a scia stream (bad magic, or a schema this build
does not speak) is reported as a stream error rather than replayed.

This makes the harness's printed A/B commands (`scia --input <clip> --scene <s>
--preset <p>`) directly runnable, and is the listening path for the scene-quality
calibration sessions.

## Frame schema

Every frame carries the fields below. Names are stable within a schema version.
The current schema version is **1**.

| field                | type       | meaning |
| -------------------- | ---------- | ------- |
| `schema`             | u32        | Schema version of this frame. Always the emitter's version; a consumer rejects any version it does not speak. |
| `generation`         | u64        | Monotonic hop counter; never resets for the life of the source engine. |
| `timestamp_ns`       | u64        | When the hop was processed, in nanoseconds since the source engine epoch. (On `--input` this is restamped to the local receive clock so the UI's frame-age reads correctly across machines.) |
| `sample_rate`        | u32        | Stream sample rate in Hz. |
| `channels`           | u16        | Channel count (1 or 2). |
| `starved`            | bool       | `true` when the hop was synthesized during capture starvation. |
| `activity`           | string     | Coarse activity state: `active`, `quiet`, or `idle`. |
| `quiet_ms`           | f32        | Milliseconds since the last non-quiet hop; `0` while active. |
| `dropped_frames`     | u64        | Cumulative frames dropped to ring overflow, as of this hop. |
| `rms`                | f32        | RMS level of the hop over the mono mix (`0.0..=1.0` for in-range audio). |
| `peak`               | f32        | Peak absolute sample over the hop (`0.0..=1.0` for in-range audio). |
| `lufs_momentary`     | f32        | Momentary loudness (LUFS). Reserved, `0` in schema 1. |
| `spectrum`           | f32 array  | Display spectrum: the valid log-spaced bars in `0.0..=1.0`. Its length is the analyzer's bar count (never more than 256). |
| `bands`              | f32[3]     | Bass / mid / treble levels, each normalized to its own recent average (`1.0` = average, clamped `0.0..=4.0`). |
| `flux`               | f32        | Half-wave-rectified spectral flux, normalized (`0.0..=1.0`). |
| `onset`              | bool       | `true` when an onset (transient) was detected on this hop. |
| `onset_age_ms`       | f32        | Milliseconds since the last onset, saturating at `60000` ("no recent onset"). |
| `beat_phase`         | f32        | Position within the current beat, `0.0..1.0`; `0.0` while unlocked. |
| `beat_confidence`    | f32        | Beat-tracker confidence, `0.0..=1.0`. Gate the beat fields on this. |
| `tempo_bpm`          | f32        | Estimated tempo in BPM; `0.0` while unlocked. |
| `stereo_correlation` | f32        | Inter-channel correlation, `-1.0..=1.0`. Reserved, `0` in schema 1. |
| `mid_side_ratio`     | f32        | Mid/side energy ratio. Reserved, `0` in schema 1. |
| `chroma`             | f32[12]    | 12-bin chroma vector. Reserved (all `0`) in schema 1. |

"Reserved" fields are present and part of the layout but always zero in schema
1; they are filled by later analysis without a schema bump (adding a value to a
field that was documented as reserved is not a breaking change).

## Framing

### JSON (NDJSON)

One JSON object per line, terminated by `\n` (newline-delimited JSON). Field
names are exactly the table above; the `spectrum`, `bands` and `chroma` arrays
are JSON arrays; `activity` is a lowercase string. Blank lines are ignored by
the reader (they may appear as keepalive spacing). Every line stands alone and
carries its own `schema`, so a consumer can validate each line independently.

Example line (spectrum truncated for readability):

```json
{"schema":1,"generation":3,"timestamp_ns":1000000,"sample_rate":48000,"channels":2,"starved":false,"activity":"active","quiet_ms":0.0,"dropped_frames":0,"rms":0.5,"peak":0.8,"lufs_momentary":0.0,"spectrum":[0.0,0.25,0.5,1.0],"bands":[1.0,0.5,0.25],"flux":0.1,"onset":false,"onset_age_ms":0.0,"beat_phase":0.0,"beat_confidence":0.9,"tempo_bpm":120.0,"stereo_correlation":0.0,"mid_side_ratio":0.0,"chroma":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}
```

### Binary

A little-endian, length-prefixed stream:

```text
stream = header frame*
header = magic(4)  schema(u16)  reserved(u16)
frame  = length(u32)  payload(length bytes)
```

- **`magic`** — the four ASCII bytes `SCIA` (`0x53 0x43 0x49 0x41`). They open
  the stream once and let a reader auto-detect the encoding (a JSON stream opens
  with `{`, never `S`).
- **`schema`** — the stream's schema version as a `u16`. Validated once, at the
  header; a reader rejects a version it does not speak before reading any frame.
- **`reserved`** — a `u16`, zero in schema 1.
- **`length`** — the byte length of the following payload, as a little-endian
  `u32`. A reader that hits end-of-stream exactly on a frame boundary stops
  cleanly; a short read mid-frame is an error.

The payload is the frame fields in the table's order, each little-endian:

```text
schema u32 · generation u64 · timestamp_ns u64 · sample_rate u32 ·
channels u16 · starved u8 · activity u8 (0 active, 1 quiet, 2 idle) ·
quiet_ms f32 · dropped_frames u64 · rms f32 · peak f32 · lufs_momentary f32 ·
spectrum_len u16 · spectrum (spectrum_len × f32) · bands (3 × f32) · flux f32 ·
onset u8 · onset_age_ms f32 · beat_phase f32 · beat_confidence f32 ·
tempo_bpm f32 · stereo_correlation f32 · mid_side_ratio f32 · chroma (12 × f32)
```

The binary payload keeps a per-frame `spectrum_len` prefix; the JSON encoding
carries the same information implicitly in the array length. Both encodings
round-trip a frame exactly.

## Idle cadence

Streaming follows the engine's silence discipline. While the engine is active or
quiet, frames are emitted at the `--rate` cadence. Once the engine reaches its
**idle** state (signal has been silent long enough that the DSP thread has
downshifted), the stream drops to a reduced **keepalive cadence** of one frame
every 500 ms (2 Hz), or the configured `--rate` if that is already slower. A
silent input therefore never spins the stream at full rate; a consumer still
receives periodic frames (with the `idle` activity and a growing `quiet_ms`) so
it can tell the stream is alive. The cadence returns to `--rate` the moment
signal resumes.

## Versioning policy

The `schema` field is the compatibility contract:

- A **breaking change** — renaming, removing, reordering or changing the meaning
  or type of a field, or changing the binary layout — **bumps the schema
  version**. The version rides on every JSON line and in the binary header.
- Filling a field previously documented as *reserved* (it was already present
  and zero) is **not** breaking and does not bump the version.
- A reader accepts only its own schema version and rejects any other with a
  clear error rather than mis-parsing. Emitters always stamp their own version.

The stream schema is pinned to the underlying `FeatureSnapshot` contract: the
two versions move together, so a change to the snapshot layout bumps the stream
version in lock-step.

## Consumers

- **`scia --input <ADDR>`** — the built-in renderer, driving the full terminal UI
  from a remote stream (see [Command line](#command-line)).
- **`scia-bridge`** — the Windows-side capture companion. It serves this exact
  stream (the same serving loop as `scia --output --listen`), so a `scia
  --input` elsewhere can render Windows audio. It is the second of the two
  [WSL](wsl.md) paths, where the Windows system mix is not visible to a Linux
  process directly.
