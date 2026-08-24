# P2 — PipeWire sink-capture pin

Tripwire, not production. Goal: pin the undocumented cpal 0.18 behaviour the
Linux system-mix baseline depends on, and prove it continuously in CI. Tooling:
`crates/core/tests/pipewire.rs`, run by the `pipewire` workflow against a
headless PipeWire session.

## What is being pinned

The Linux system mix is captured by opening the default *output* device on the
PipeWire host as an *input* stream (`CpalBackend { prefer_pipewire: true }`, the
`capture-pipewire` feature). That only captures the system mix because of two
behaviours in cpal 0.18.2 that are documented **only in its source**, not in any
public contract:

- **Sinks are exposed as input-capable.** When cpal's PipeWire host enumerates
  the graph it maps an `Audio/Sink` node to `DeviceDirection::Duplex`, so a sink
  appears among the input devices and can be opened for capture.
  `cpal-0.18.2/src/host/pipewire/device.rs:900-916` (the `role`/`direction`
  match, with the explaining comment at lines 910-916).
- **An input stream on a sink captures the sink monitor.** When an input stream
  is built on a node whose role is `Sink`, the host inserts
  `PW_KEY_STREAM_CAPTURE_SINK = "true"` into the stream properties, which tells
  PipeWire to feed that stream what is *playing to* the sink (its monitor)
  rather than a capture source. `cpal-0.18.2/src/host/pipewire/device.rs:167-169`
  (the `pw_properties` method: `role == Sink && direction == Input` inserts the
  key).

Neither is part of cpal's public API, so a patch release could change or remove
it. The cpal version is therefore pinned exactly — `cpal = "=0.18.2"` in
`crates/core/Cargo.toml` — and this probe fails loudly if the behaviour ever
stops holding.

## Method

The `pipewire` workflow (`.github/workflows/pipewire.yml`) runs on
`ubuntu-latest`:

1. Installs PipeWire, WirePlumber, the PulseAudio-compat layer and the
   libpipewire/libspa dev headers that cpal's `pipewire` feature links.
2. Starts a headless session under a private `XDG_RUNTIME_DIR` and a private
   session bus: `pipewire`, then `wireplumber`, then `pipewire-pulse`, waiting
   until `pw-cli info 0` and `pactl info` both answer.
3. Creates a null sink named `scia-test-sink`
   (`pactl load-module module-null-sink`) and makes it the default sink.
4. Generates a 1 kHz, amplitude 0.5, 48 kHz, stereo, 60 s WAV with python3's
   `wave` module and plays it into the default sink with `pw-play`, confirming a
   sink-input appears.
5. Runs the test with the feature:
   `SCIA_PIPEWIRE_TEST=1 cargo nextest run -p scia-core --features capture-pipewire --test pipewire`.

The test (`pipewire_sink_capture_pins_cpal_behaviour`) asserts, in order:

- `cpal::available_hosts()` contains the PipeWire host (else the whole baseline
  is gone).
- `list_devices()` contains a PipeWire-host device whose name contains
  `scia-test-sink` (the sink is enumerated as input-capable).
- `CpalBackend { prefer_pipewire: true }` opens through the engine at 48 kHz,
  1–2 channels (PipeWire's default graph rate).
- Within 5 s a snapshot arrives that is not starved, is `Active`, has
  `rms >= 0.02`, and whose loudest display-spectrum bar covers 1 kHz (the played
  tone) — proving the captured audio is the sink monitor, not silence or a
  microphone source.
- Over the last 2 s no hop is synthesized (steady, non-starved delivery) and no
  frame is dropped.

Without `SCIA_PIPEWIRE_TEST=1` the test prints a skip line and returns, so an
ordinary developer build with the feature but no session does not fail.

## Running it locally

On a machine that has a PipeWire session and the libpipewire dev headers:

```sh
# Create the null sink and make it default.
pactl load-module module-null-sink \
  sink_name=scia-test-sink \
  sink_properties=device.description=scia-test-sink
pactl set-default-sink scia-test-sink

# Play a 1 kHz tone into it (any 1 kHz source works; e.g. a generated WAV).
pw-play tone-1khz.wav &

# Run the pinned test.
SCIA_PIPEWIRE_TEST=1 cargo nextest run \
  -p scia-core --features capture-pipewire --test pipewire
```

(The CI job generates `tone-1khz.wav` inline with python3's `wave` module — see
the workflow for the exact snippet.)

## Results

First green CI run (`ubuntu-latest`, `pipewire` job). The runner's PipeWire
graph came up at 48 kHz / 1024-frame quantum; the null sink was enumerated on
the PipeWire host and carried the played tone through its monitor.

Device table (13 devices; the relevant rows):

```
host=pipewire kind=Input  default_in=true  default_out=false name=default_input
host=pipewire kind=Input  default_in=false default_out=false name=scia-test-sink
host=pipewire kind=Output default_in=false default_out=true  name=default_output
host=pipewire kind=Output default_in=false default_out=false name=scia-test-sink
```

`scia-test-sink` appears among the **input** devices — the pinned behaviour: an
`Audio/Sink` exposed as input-capable.

| Measurement | Value |
|---|---|
| Negotiated format | 48 000 Hz, 2 channels |
| rms / peak at match | 0.3553 / 0.5000 |
| Loudest bar | idx 36, band 984.7 .. 1069.7 Hz (contains 1 kHz) |
| Stats at match | pushes 2, pushed_frames 1024, dropped 0, xruns 0, hops_processed 4, synthesized 0 |
| Stats after +2 s | pushes 190, pushed_frames 97 280, dropped 0, xruns 0, hops_processed 380, synthesized 0 |

The captured tone peaks exactly in the 1 kHz bar at the amplitude it was played
(peak 0.5), and over the 2 s steady-state window nothing was dropped and no hop
was synthesized — the sink monitor delivers continuously.
