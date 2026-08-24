# scia

A real-time music visualizer for the terminal.

scia captures whatever your system is playing — a music app, a game, a browser
tab — with no configuration, and renders generative scenes at 60 fps in modern
terminals (Windows Terminal, ghostty, and others). One engine core is designed
to grow a GPU window and a desktop-wallpaper mode later.

**Status: pre-alpha, under construction.** There is nothing to install yet.

## Goals

- Zero-config capture of any system audio on Windows and Linux (macOS best-effort)
- Sub-frame reactivity: ≤ 33 ms audio-to-visual by default, an opt-in low-latency mode
- Tear-free 60 fps rendering, near-zero CPU when nothing is playing
- A real scene engine: TOML presets, expressions, sandboxed scripts
- Now-playing metadata and album-art palettes from any player
- A single small binary, permissively licensed

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
