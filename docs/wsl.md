# WSL

A Linux build of `scia` running inside the Windows Subsystem for Linux **cannot
see the Windows system audio**. WSLg exposes a PulseAudio server to the guest,
but it carries only the audio of WSL applications — not the Windows mix that a
music player, browser or game produces on the Windows side. A live capture
inside WSL therefore reacts only to WSL-app sounds and is otherwise silent.

`scia` detects WSL (from `/proc/version` or `WSL_DISTRO_NAME`) and, on a live
capture there, says so plainly — on the way in and in an in-app guidance screen —
rather than presenting a black window. Capture still proceeds, because WSL-app
audio is legitimate; it is just labeled for what it is.

Two supported paths reach the Windows mix.

## 1. Interop exec (recommended)

Run the Windows build of `scia` from your WSL shell. WSL puts the Windows `PATH`
on the guest `PATH` by default, so a `scia.exe` on the Windows side is directly
callable:

```text
scia.exe
```

The process runs as a native Windows program — it captures the Windows system
mix through WASAPI loopback and renders into the same terminal. This is the
simplest path and needs no second process.

If `scia.exe` is not found, either add its folder to the Windows `PATH`, or call
it by its full path. Windows `PATH` interop is controlled by the
`appendWindowsPath` setting in `/etc/wsl.conf`; it is on by default, and if it
has been turned off the Windows `PATH` will not appear in WSL and interop exec
will not resolve `scia.exe`:

```ini
# /etc/wsl.conf
[interop]
appendWindowsPath = true
```

(Restart the distribution with `wsl --shutdown` after editing `wsl.conf`.)

## 2. Feature-stream split (scia-bridge)

Run a capture process on the Windows side that serves the machine-readable
[feature stream](feature-stream.md), and render it from inside WSL. The
companion `scia-bridge` is exactly this server, with bridge-appropriate defaults.

On Windows, get `scia-bridge` (copy `scia-bridge.exe` across for now; a winget
package comes in a later release) and run it, binding an address the WSL guest
can reach:

```text
scia-bridge --listen 0.0.0.0:7526
```

`scia-bridge` starts capture on the default Windows output (WASAPI loopback) and
serves each analysis hop to every connected client. Its flags:

- `--listen <ADDR>` — the TCP address to serve on. Default `127.0.0.1:7526`; use
  `0.0.0.0:7526` so a consumer on the (virtual) network — such as a WSL guest
  reaching the Windows host — can connect.
- `--encoding <binary|json>` — the wire encoding. Default `binary` (compact,
  length-prefixed); `json` emits one frame object per line.
- `--rate <N>` — target frames per second (default 60). The stream drops to a
  slower keepalive cadence while the audio is idle.
- `--demo` — serve the built-in synthetic feed instead of real capture (needs no
  audio hardware; useful for testing the wire path).

Then, inside WSL, render that stream instead of capturing local audio:

```text
scia --input <windows-host>:7526
```

The encoding is auto-detected. A dropped connection is retried automatically
while the UI shows its normal reconnecting state. See
[feature-stream.md](feature-stream.md) for the frame schema, both framings, and
the versioning policy — `scia-bridge` and `scia --output --listen` produce the
same stream.

## Which to use

The interop exec path is the least setup and the recommended default. Reach for
the bridge when the Windows capture and the rendering must be separate processes
— for instance to render on a machine other than the one producing the audio, or
to feed the stream into another consumer at the same time.
