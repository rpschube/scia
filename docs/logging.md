# Logging and run records

scia has two independent, opt-in diagnostic channels:

1. **Structured logging** — leveled `tracing` events for troubleshooting the
   engine, scenes, metadata and the feature stream. Off by default.
2. **Run records** (`--log-run`) — a machine-readable, per-run transcript of the
   audio-feature plane, the data plane for the scene-quality harness.

Both are silent unless you ask for them, and neither ever leaves the machine.

## Structured logging

### Turning it on

Logging is **off by default**. Enable it, lowest-precedence source last:

| Source | Example | Notes |
| --- | --- | --- |
| `--log <level>` flag | `scia --log debug` | Highest precedence. |
| `SCIA_LOG` env var | `SCIA_LOG=info scia` | Used when `--log` is absent. |
| `[log] level` config | see below | Used when neither above is set. |
| _(nothing)_ | | Logging stays off — zero cost. |

Levels are `error`, `warn`, `info`, `debug`, `trace` (increasing verbosity).
`info` covers lifecycle: device open/switch/reopen, scene and preset swaps,
now-playing session changes, presenter tier at startup, feature-stream
connect/disconnect. `debug` adds per-stage detail (activity transitions, artwork
campaign stages, hot-reload results). `trace` adds the noisiest traces.

Config file (`<config dir>/config.toml`):

```toml
[log]
level = "info"   # error | warn | info | debug | trace
file  = true     # write the rotating log file (default true)
```

An unrecognised `SCIA_LOG` value or `[log] level` is warned about once at startup
and ignored (logging falls through to the next source, then off).

### Where logs go (sinks)

When a level is active, events go to:

- **A rotating JSON-lines file** at `<config dir>/logs/scia.log`. It is bounded
  by size (a few MiB) with a small fixed number of rolled generations
  (`scia.log.1`, `scia.log.2`, …); the oldest is dropped. Set `[log] file =
  false` to disable the file sink.
  - Config dir: `$XDG_CONFIG_HOME/scia` (else `~/.config/scia`) on Unix,
    `%APPDATA%\scia` on Windows.
- **stderr**, but **only when the TUI is not driving the terminal** — i.e. in
  `--headless`, `--output` and other non-TUI modes. While the full-screen TUI is
  active (a bare run, `--demo`, `--input`) the stderr sink is switched off so a
  log line can never corrupt the screen; the file sink still records everything.

### Cost when off

With no level resolved, no subscriber is installed at all, so every `tracing`
callsite short-circuits on a static level check before building a single field.
The DSP and render threads pay nothing for logging that is off — the disabled
path is allocation-free (guarded by `crates/core/tests/no_alloc_logging.rs`, and
the existing hot-path no-alloc budget tests stay green with logging off).

## Run records (`--log-run <path>`)

`--log-run <path>` writes a machine-readable transcript of the session as JSON
Lines to `<path>` — independent of `--log`. It is the data plane the
scene-quality harness reads. One object per line:

```jsonc
{"rec":"run_start","schema":1,"scene":"spectra","preset":null,"params":{},"source":"synthetic","hop_ms":5.333}
{"rec":"hop","t_ms":5.33,"rms":0.21,"bands":[1.1,0.8,0.3],"onset":0.4,"beat_conf":0.7,"bpm":128.0,"canvas":null}
{"rec":"event","t_ms":900.0,"kind":"scene_swap","detail":{"from":"spectra","to":"aurora"}}
{"rec":"run_end","t_ms":1000.0,"hops":47}
```

- **`run_start`** — the resolved scene id, preset (name/path or `null`), resolved
  scalar params, the input source (`synthetic`, `live`, `replay`) and the nominal
  hop period.
- **`hop`** — one per recorded hop. `t_ms` is the snapshot's monotonic engine
  clock, so it is non-decreasing across a run. `onset` is the continuous onset
  strength (normalized spectral flux), not the discrete onset flag. `beat_conf`
  and `bpm` are `null` until the beat tracker locks. `canvas` is always `null`
  in this mode (it records the audio-feature plane; the harness fills canvas
  stats when it renders).
- **`event`** — scene/preset swaps (detected from the active scene id changing),
  device switches, and hot reloads.
- **`run_end`** — the end time and total hop count.

**Throttle.** Live and demo runs record **every fourth** hop; replaying a clip
via `--input` records **every** hop (the replay is paced well below the live hop
rate). Hops are de-duplicated by generation.

Readers must tolerate unknown fields — the schema may gain fields without a
version bump for additive changes. The struct definitions live in the
`scia-telemetry` crate (`record` module) and are mirrored by the harness.

Example:

```console
$ scia --demo --headless --seconds 5 --log-run run.jsonl
```

## Privacy

scia is careful with what it writes, and nothing here leaves the machine:

- Log **messages** contain no usernames, hostnames, LAN addresses or filesystem
  paths. A path appears only as the *location* of your own log file, never inside
  a message. Feature-stream and device logs record the connect/switch **edge**,
  not the address or device path.
- **Track titles** may appear in local logs and run records (they help correlate
  a visual with what was playing). This is the one piece of content that is
  logged; it stays in the local file.
- Log files and run records are written only to the paths you choose (the config
  dir, or the `--log-run` path). scia never transmits them anywhere.
