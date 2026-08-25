# macOS

`scia` captures the macOS system mix — whatever any app is playing — through a
**Core Audio process tap**. The cpal backend opens the default *output* endpoint
as an *input* stream; cpal's Core Audio host sees the endpoint has no input
channels and transparently builds a process tap over it plus a private aggregate
device (`AudioHardwareCreateProcessTap` / `CATapDescription`), so the input
stream carries the whole system output mix. This is the same output-as-input
shape scia uses for the Windows WASAPI loopback and the Linux PipeWire sink
monitor — one capture path across all three desktops.

No extra software or virtual device is required on a supported system: this is
zero-config capture of any app's audio, best-effort on macOS.

## Requirements and the permission prompt

- **macOS 14.4 or newer.** The process-tap APIs the capture relies on are only
  available from macOS 14.4. On older releases the tap cannot be created and
  capture fails cleanly with an error — use the loopback-device fallback below.
- **The "System Audio Recording" permission.** The first time scia opens the tap,
  macOS shows a one-time TCC prompt asking you to allow scia to record system
  audio. scia prints a short note on stderr before the tap opens, so the dialog
  is expected rather than a surprise over a black screen. **Click Allow** to
  visualize system audio.

## If you see no audio (permission denied or unanswered)

macOS provides **no API to query** whether the "System Audio Recording"
permission was granted. A denied tap still *opens* — it simply never delivers any
audio. scia detects this by a bounded **zero-delivery timeout**: when a freshly
opened tap has delivered nothing for a few seconds, scia stops showing a silent
black screen and surfaces an actionable notice, in-app and (headless) on the
status line. The loop keeps running on synthesized silence, so granting the
permission later recovers automatically with no restart.

To grant or restore the permission:

1. Open **System Settings**.
2. Go to **Privacy & Security > Screen & System Audio Recording**.
3. Enable **scia** in the list.
4. Return to scia; capture resumes once the tap starts delivering (reopen capture
   if needed).

## Fallback: a loopback device (older macOS, or a denied tap)

On macOS older than 14.4 — or if you prefer not to use the process tap — install
a user-space loopback audio device and select it by name. A common free option is
[BlackHole](https://github.com/ExistentialAudio/BlackHole) (GPL-3): scia does not
bundle or link it; you install it yourself and route your system output through
it (typically via an Aggregate/Multi-Output device in **Audio MIDI Setup**), then
point scia at it:

```text
scia --device "BlackHole 2ch"
```

List the device names scia sees with `scia --list-devices`.

## Notes for maintainers

The output-as-input loopback trigger is behaviour of the pinned `cpal =0.18.2`
Core Audio host (it builds the tap + aggregate device when an output-only
endpoint is opened for input), so the cpal version is pinned exactly, matching the
Linux PipeWire pin in [probes/p2-pipewire-pin.md](probes/p2-pipewire-pin.md).
Runtime capture on macOS is verified only by the CI compile-and-unit-test on the
macOS runner; the runner has no audio device or granted permission, so the tap's
real-audio delivery is not exercised there. The device-selection direction
(output endpoint, output config) is compile-checked on that runner, and the
zero-delivery → actionable-message mapping is unit-tested on every target.
