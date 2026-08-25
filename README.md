# scia

A real-time music visualizer for the terminal.

scia captures whatever your system is playing — a music app, a game, a browser
tab — with no configuration, and renders generative scenes at 60 fps in modern
terminals (Windows Terminal, ghostty, and others). One engine core is designed
to grow a GPU window and a desktop-wallpaper mode later.

**Status: pre-alpha, under construction.** Packaging is release-ready, but no
version has been tagged yet — the install commands below activate with the
first release.

## Install

scia is a single self-contained binary (under 10 MiB, no runtime dependencies
beyond the OS and its system audio libraries). Full per-platform detail,
including the Linux audio notes, is in [docs/installing.md](docs/installing.md).
Replace `X.Y.Z` with the release version.

**Windows**

```powershell
winget install rpschube.scia
```

**macOS (Apple Silicon)**

```sh
brew install rpschube/tap/scia
```

**Arch Linux (AUR)**

```sh
paru -S scia      # or: yay -S scia
```

**Shell / PowerShell installer (any release, direct download)**

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/rpschube/scia/releases/download/vX.Y.Z/scia-installer.sh | sh
```

```powershell
# Windows
irm https://github.com/rpschube/scia/releases/download/vX.Y.Z/scia-installer.ps1 | iex
```

**Manual:** download the archive for your platform from the
[releases page](https://github.com/rpschube/scia/releases), verify it against
the adjacent `.sha256`, extract, and put `scia` on your `PATH`. On Linux, the
prebuilt binaries capture the microphone-level input; for system-mix capture use
the AUR package or build with `--features capture-pipewire` (see
[docs/installing.md](docs/installing.md)).

## Goals

- Zero-config capture of any system audio on Windows and Linux (macOS best-effort)
- Sub-frame reactivity: ≤ 33 ms audio-to-visual by default, an opt-in low-latency mode
- Tear-free 60 fps rendering, near-zero CPU when nothing is playing
- A real scene engine: [TOML presets](docs/presets.md), expressions, sandboxed scripts
- Now-playing metadata and album-art palettes from any player
- A single small binary, permissively licensed

## Platform notes

- **Windows** — captures the system mix through WASAPI loopback, no setup.
- **Linux** — captures the system mix through the PipeWire sink monitor (build
  with the `capture-pipewire` feature); on plain ALSA it falls back to the
  default input. Inside WSL, see [docs/wsl.md](docs/wsl.md).
- **macOS** — captures the system mix through a Core Audio process tap on macOS
  14.4+, after a one-time **System Audio Recording** permission prompt. If you
  see no audio, grant it under System Settings > Privacy & Security > Screen &
  System Audio Recording; on older macOS use a loopback device. Full notes,
  including recovery and the loopback fallback, are in
  [docs/macos.md](docs/macos.md).

## Make your own scenes

A scene or preset is one self-contained file you drop next to the built-ins — no
build step. Retune a built-in with a TOML preset, wire the audio to a knob with a
one-line expression, or write a small sandboxed Luau scene for a wholly new look.
Start from the guide, [docs/authoring.md](docs/authoring.md), and the two ready
templates: [templates/preset-template.toml](templates/preset-template.toml) and
[templates/scene-template.lua](templates/scene-template.lua). The format and
scripting references are [docs/presets.md](docs/presets.md) and
[docs/scenes.md](docs/scenes.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
build tooling, workflow and privacy rules every change follows.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
