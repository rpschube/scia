# Installing scia

scia ships as a single, self-contained binary — under 10 MiB, with no runtime
dependencies beyond the operating system and its system audio libraries. Pick
the method for your platform below. Until the first release is tagged there are
no published artifacts yet; the commands are the ones that will work once a
release exists.

Replace `X.Y.Z` with the release version throughout.

## Windows

**winget** (once the manifest is accepted):

```powershell
winget install rpschube.scia
```

**PowerShell installer** (direct from the release):

```powershell
irm https://github.com/rpschube/scia/releases/download/vX.Y.Z/scia-installer.ps1 | iex
```

**Manual:** download `scia-x86_64-pc-windows-msvc.zip` from the
[release](https://github.com/rpschube/scia/releases), verify it against the
adjacent `.sha256`, unzip, and put `scia.exe` on your `PATH`.

## macOS (Apple Silicon)

**Homebrew** (once the formula is published to the tap):

```sh
brew install rpschube/tap/scia
```

**Shell installer:**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/rpschube/scia/releases/download/vX.Y.Z/scia-installer.sh | sh
```

**Manual:** download `scia-aarch64-apple-darwin.tar.xz`, verify the `.sha256`,
extract, and move `scia` onto your `PATH`.

## Linux

**Arch (AUR)** — build-from-source, captures the system mix via PipeWire:

```sh
# with an AUR helper
paru -S scia      # or: yay -S scia
```

**Shell installer** (glibc build):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/rpschube/scia/releases/download/vX.Y.Z/scia-installer.sh | sh
```

**Manual:** download `scia-x86_64-unknown-linux-gnu.tar.xz` (glibc) or
`scia-x86_64-unknown-linux-musl.tar.xz` (static musl), verify the `.sha256`,
extract, and move `scia` onto your `PATH`.

### Linux audio: what the prebuilt binaries can and cannot do

scia captures audio on Linux through ALSA/PipeWire, and the prebuilt release
binaries link the ALSA client library (`libasound.so.2`) dynamically. Two
consequences are worth stating plainly rather than shipping a surprise:

- **The generic tarball binaries capture the ALSA default *input* (microphone
  level), not the system output mix.** System-mix capture needs the PipeWire
  host, which is a build-time feature (`capture-pipewire`) not enabled in the
  prebuilt release binaries. For true system-audio visualization on Linux,
  install the **AUR package** (built from source with `capture-pipewire`, so it
  captures a PipeWire sink monitor) or build from source with
  `--features capture-pipewire`.

- **The musl build is static libc, but not audio-standalone.** Because ALSA is
  linked dynamically, the `x86_64-unknown-linux-musl` binary still needs
  `libasound.so.2` present at runtime to capture anything. On a host without the
  ALSA/PipeWire runtime libraries, the musl binary still runs in the modes that
  need no local capture — `--demo` (the built-in synthetic feed) and
  `--input <addr>` (rendering a remote feature stream served by another scia).
  Local capture (`scia` with no flags, or `--output`) requires the audio
  libraries. Use the glibc build on a normal desktop; reach for the musl build
  only where a static libc matters and you accept the capture caveat.

To visualize without any audio device, try:

```sh
scia --demo
```
