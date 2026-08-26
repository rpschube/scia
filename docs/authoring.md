# Authoring scenes

This guide is for a musician or tinkerer who wants to make their own scenes for
scia and has never read the source. You do not need to build the project or
write Rust: a scene can be a single text file you drop next to the built-ins.

There is a ladder of three rungs, from "edit a few numbers" to "write a little
program". You can stop at any rung.

| Rung | What you write | File | Good for |
| ---- | -------------- | ---- | -------- |
| 0 | A **preset**: a TOML file that picks a scene and sets its knobs | `.toml` | Retuning a built-in, restyling colours, stacking layers |
| 1 | **Expressions** inside a preset's `[map]` table | `.toml` | Wiring the audio to a knob with a one-line formula |
| 2 | A **Luau scene**: a small sandboxed script that draws | `.lua` | A brand-new look the built-ins do not cover |

Two ready-to-edit skeletons ship with the project — copy one and start changing
it:

- [`templates/preset-template.toml`](../templates/preset-template.toml) — rung 0,
  every section commented.
- [`templates/scene-template.lua`](../templates/scene-template.lua) — rung 2, a
  runnable scene smaller than either shipped example.

## Where your files go

Both kinds live under scia's config directory — the same folder that holds
`config.toml`:

| | Scenes (`.lua`) | Presets (`.toml`) |
| --- | --- | --- |
| Unix | `$XDG_CONFIG_HOME/scia/scenes/`, else `~/.config/scia/scenes/` | `…/scia/presets/` |
| Windows | `%APPDATA%\scia\scenes\` | `%APPDATA%\scia\presets\` |

Every file in those folders is discovered at startup. `scia --list-scenes`
prints the resolved directories, lists each discovered scene and preset, and
tags Luau scenes with `[luau]` and drop-in presets with `[drop-in]`. Once a
file is in place, run it by name:

```sh
scia --scene <name>        # a built-in, a dropped-in preset, or a .lua scene
```

You can also run a file straight from any path and edit it live (see
[The in-app workflow](#the-in-app-workflow)):

```sh
scia --demo --scene-file path/to/your.toml
```

---

## Rung 0 — presets (TOML)

A preset is a plain TOML file that fully describes one scene instance: which
scene to draw, its typed parameters, an optional stack of layers, feature →
parameter mappings, and a palette. No code runs; every value is validated when
the file loads, with errors that name the file, line and column.

The five tables, in brief:

- **`[preset]`** (required) — `name` (the id you pass to `--scene`, matching
  `^[a-z0-9][a-z0-9-]*$`), `scene` (a registered scene id), and optional
  `description` / `mood`.
- **`[params]`** — typed knobs for the scene. Each key must exist in that
  scene's manifest; each value is a number inside the parameter's range. Omitted
  keys keep their default. Run `scia --list-scenes` to see the scenes; the
  per-scene manifests are tabulated in [`docs/presets.md`](presets.md).
- **`[[layer]]`** — an optional layer stack. With no layers the preset is one
  layer (its `scene` + `[params]`). Add layers to composite scenes with a
  `blend` (`over` / `add` / `max`) and `intensity`. Parameter precedence is
  `manifest default < [params] < [layer.params]`.
- **`[map]`** — feature → parameter mappings, applied live every frame. Covered
  in [Rung 1](#rung-1--expressions).
- **`[palette]`** — `source = "static"` with an optional `slots` array of
  exactly eight `"#rrggbb"` colours, or `source = "album-art"` (accepted and
  validated, but resolves to the host default palette until the album-art
  feature lands).

The annotated [`templates/preset-template.toml`](../templates/preset-template.toml)
shows every table and key. The full reference — types, ranges, envelope maths,
the palette rules and the exact error format — is
[`docs/presets.md`](presets.md).

---

## Rung 1 — expressions

Each `[map]` entry drives one parameter of the mapped scene (the preset's scene,
or the first layer's scene when there are layers). An entry is written in **one
of two forms**:

**Table form** — a feature read through a response curve and an attack/decay
envelope:

```toml
[map]
punch = { feature = "onset", curve = "linear", attack_ms = 0, decay_ms = 250, scale = 0.9, offset = 0.0 }
```

**Expression form** — a one-line algebraic formula, compiled once at load and
evaluated every frame:

```toml
[map]
punch = "onset * 0.7 + bass * 0.2"
gap   = "0.1 + loud * 0.4"
drift = "bass ^ 2"
```

### The expression vocabulary

An expression may reference exactly these names (the canonical list from
[`crates/scenes/src/preset/expr.rs`](../crates/scenes/src/preset/expr.rs)); any
other name is rejected at load, at that entry's `file:line:col`:

| name | source | notes |
| ---- | ------ | ----- |
| `bass` | `bands[0]` | clamped to `0..1` |
| `mid` | `bands[1]` | clamped to `0..1` |
| `treb` | `bands[2]` | clamped to `0..1` |
| `loud` | `loudness` | normalized loudness, `0..1`, level-independent |
| `peak` | `peak` | clamped to `0..1` |
| `onset` | onset **envelope** | `1.0` on an onset hop, then an exponential decay (τ ≈ 250 ms), so you get a usable curve, not a one-frame spike |
| `flux` | `flux` | clamped to `0..1` |
| `beat` | `beat_phase` | position within the beat, `0..1` |
| `beat_conf` | `beat_confidence` | beat-tracker confidence, `0..1` |
| `width` | `stereo_correlation` (raw) | `0.0` until the stereo feature lands |

Operators are `+ - * / % ^` with parentheses, alongside `fasteval`'s built-in
math functions (`sin`, `cos`, `abs`, `min`, `max`, `log`, …). A non-finite
result (for example a division by zero) is sanitized to `0.0`. Whatever the
expression produces, the scene clamps it back into the parameter's `[min, max]`
when it reads it, so an expression that overshoots is capped, not rejected.

### Curves (table form)

Applied to the clamped feature value `x`:

| curve | formula | parameter |
| ----- | ------- | --------- |
| `linear` | `x` | — |
| `pow` | `x^exponent` | `exponent` required, `> 0` |
| `log` | `ln(1 + 9x) / ln 10` | — |
| `step` | `x >= threshold ? 1 : 0` | `threshold` required, `0..=1` |

After the curve, a table entry runs a first-order envelope follower (attack
while rising, decay while falling) and stores `offset + scale * y`. The full
maths is in [`docs/presets.md`](presets.md).

### A vocabulary quirk to know

The two forms do **not** read three of the names the same way, by design:

| name | table `[map]` | expression |
| ---- | ------------- | ---------- |
| `onset` | `1.0` / `0.0` spike | a decaying envelope |
| `beat` | `beat_confidence` | `beat_phase` |
| `width` | `(1 - correlation)/2` | raw `correlation` |

The live mapping overlay (`m`, below) speaks the **expression** vocabulary, so a
row's `beat` sparkline samples `beat_phase`.

---

## Rung 2 — Luau scenes

When no combination of preset knobs gives the look you want, write a scene. A
scene is a small [Luau](https://luau.org/) script that returns one **manifest
table** carrying its lifecycle functions. It binds to the very same interface a
built-in Rust scene does, so it lists in the browser and loads with `--scene`
exactly like a built-in.

Start from [`templates/scene-template.lua`](../templates/scene-template.lua) — a
single loudness bar plus an onset pulse, deliberately smaller than the two
shipped scenes. When you want more, read the two shipped scenes, each of which
exercises a different half of the API:

- [`ripple.lua`](../crates/scenes/src/luau/scenes/ripple.lua) — scalar features
  (loudness, onset) plus tunable params and reload continuity.
- [`swarm.lua`](../crates/scenes/src/luau/scenes/swarm.lua) — the spectrum table
  and a 200-particle system.

### The API in one paragraph

The manifest returns `id` / `mood` / `summary`, an optional `params` array (the
knobs a preset may set), and the functions `update(features, dt)` and
`render(canvas)` (both required), plus optional `init(ctx)` and `state()` /
`restore(saved)` for continuity across a hot reload. In `update` you read
already-normalized feature fields (`features.loud`, `.bass`/`.mid`/`.treb`,
`.onset`, `.beat_phase`, `features:bar(i)`, …) and fold them into your own
locals. In `render` you draw in **normalized coordinates** — `x`, `y` are
fractions in `0..1`, origin top-left — with `canvas:bar`, `:line`, `:point`,
`:text` and `:field`; `slot` is a `0..7` palette index and `intensity` is
`0..1`. A scene never learns the cell or pixel size, so one file drives the
terminal (and, later, a GPU window) unchanged. The **complete reference** —
every feature field, every canvas call, the manifest shape — is
[`docs/scenes.md`](scenes.md); this guide does not repeat it.

### The sandbox, in plain language

A scene cannot touch the outside world. Its VM loads only the safe standard
libraries (`math`, `string`, `table`, `bit32`, `utf8`, `coroutine`) — there is
**no `os`, `io` or `package`** — and the functions that could load new code or
escape the environment (`load`, `loadstring`, `require`, `dofile`, `loadfile`,
`getfenv`, `setfenv`, `newproxy`, `collectgarbage`) are removed before the
globals are frozen. So a scene cannot read files, reach the network, run other
programs, or import libraries. It also cannot keep global state: each scene runs
in its own VM, so your `local`s are private per-instance.

Three limits bound every tick, so a runaway script can never take the app down:

- a **per-VM memory cap** (an allocation that would cross it fails the tick),
- a **per-tick deadline** (an infinite loop is interrupted), and
- Luau's built-in **call-depth limit** (deep recursion errors rather than
  crashing).

If a tick faults — a limit trip or any script error — the scene freezes on its
**last good frame** and the message is surfaced; the canvas is never blanked.

---

## The in-app workflow

Three keys open live authoring surfaces over the running scene. Each is
rebindable in `config.toml` under `[keys]` (`tuning`, `mapping`, `author`); the
defaults are below.

### `t` — the tuning strip

A bottom strip that live-adjusts the first few parameters of the running scene.
`←` / `→` change the selected value and the scene reacts on the same frame. A
parameter driven by a `[map]` entry is marked `~`: your adjustment sets the base
value, but the mapping overwrites it each frame.

The write key saves the adjusted values **back to the preset TOML** with every
comment and formatting detail intact — it edits only the values you changed. It
writes to your `--scene-file` if you launched with one; otherwise it exports the
built-in you are tuning to `<config_dir>/presets/<name>.toml`, so your tuned
copy becomes a file you own.

### `m` — the expression-mapping overlay

A panel listing the running scene's `[map]` rows as `target ← expression`, each
with a **live sparkline** of the signal it is wired to. Select a row and edit its
expression inline: the moment a draft compiles it previews on the next frame; a
draft that does not compile leaves the last valid mapping running and shows the
parse error inline. `⏎` commits a compiling draft, `esc` reverts. The write key
saves committed rows back to the `[map]` table, comment-preserving; a table-form
row you never touch is left byte-for-byte as it was.

### `a` — scene-author mode

A split view: the active scene's **source file** on the left, the live canvas on
the right, and the meter bridge along the bottom. It is a viewer, not an editor —
you edit the file in your own editor and the change flows back here.

### Hot reload and write-back

Whether you edit in your own editor or via `t` / `m`, a save is picked up
without restarting or interrupting audio. A `--scene-file` preset re-reads and
re-validates off the render thread and a good edit **cross-fades in** (the old
scene fades out over ~300 ms while the new one fades in), well under half a
second. Scene continuity carries across the swap: a scene that snapshots state
(via `state()` / `restore()`) resumes rather than resetting. A `.lua` drop-in
reloads the same way. A broken edit never blanks the screen — the last good
scene keeps running and the error surfaces (see below).

---

## Troubleshooting

**A preset fails to load or reload.** Every preset error begins with
`<file>:<line>:<col>: ` and a specific message — an unknown key (with the keys
that were expected), a type mismatch, an out-of-range value, an unknown scene or
feature, a bad expression, or a malformed palette. At startup a bad `--scene` /
`--scene-file` exits with that message. During a live edit, author mode (`a`)
shows the failing line highlighted with the message on the status row, plus a
**did-you-mean** hint when the offending name is close to a known one (for
example `did you mean 'punch'?`). The one-line-per-class catalogue of messages
is in [`docs/presets.md`](presets.md#error-format).

**A Luau scene errors.** A compile error (bad syntax, or the returned value is
not a well-formed manifest) keeps the previous scene running and surfaces the
message. A runtime fault — a deadline trip, a memory-cap overrun, or any script
error inside `update` / `render` — freezes the scene on its last good frame and
reports the message; it never takes the app down.

**A dropped-in file does not appear in `--list-scenes`.** Two causes: (1) the
file failed to parse or is not a well-formed manifest, so discovery skipped it —
run it directly with `--scene-file <path>` to see the exact error; or (2) its id
collides with a built-in or another discovered file, so it was dropped (see
[Sharing](#sharing-scenes-and-presets) for the collision rule). Rename it and it
will list.

**A broken edit "does nothing".** That is the guarantee working: a save that
fails to validate leaves the last good scene running and shows the error rather
than blanking the canvas. Fix the file and the next save reloads cleanly.

---

## Sharing scenes and presets

A scene or preset is meant to be **one self-contained file** you can hand to
someone else. They drop it into their `scenes/` (for `.lua`) or `presets/` (for
`.toml`) directory and run it with `--scene <name>` — nothing else to install.

**What a file may reference.** Only what is built into scia: for a preset, a
registered scene id and the feature/expression vocabulary above; for a Luau
scene, the safe standard libraries and the feature/canvas API. **A file may not
reference external assets** — no images, fonts, sounds, extra data files, other
scripts, network resources, or absolute paths. In v1 a shared file is exactly
those bytes and nothing more. (Album-art palettes are the one host-provided
input, and they come from the current track, not from the file.)

**Id collision rules — built-ins always win.** The id a file is selected by is
its `[preset].name` (for a preset) or its manifest `id` (for a Luau scene). A
drop-in whose id collides with a built-in scene, a built-in preset, the reserved
name `bars`, or another already-discovered file is **ignored**, so the built-in
listing is never disturbed. This is enforced in two places:

- Luau scene discovery drops any colliding id — see
  [`crates/scenes/src/luau/discover.rs`](../crates/scenes/src/luau/discover.rs)
  (`discover` / `push_scene`).
- Preset discovery drops any name already owned by a scene or a built-in preset
  — see
  [`crates/scenes/src/preset/discover.rs`](../crates/scenes/src/preset/discover.rs)
  (`discover` / `reserved_names`) — and name resolution checks the built-in
  preset first — see
  [`crates/scenes/src/luau/catalog.rs`](../crates/scenes/src/luau/catalog.rs)
  (`scene_preset`).

To share an edited copy of a built-in, give it a **new name** (the tuning-strip
export writes to `<config_dir>/presets/<name>.toml`; rename it if the name
matches a built-in) or run it directly with `--scene-file <path>`.

**Versioning.** Both file kinds are pinned to the `FeatureSnapshot` contract —
the analysis fields a scene or expression reads. That contract is **schema 1**
today, and its stability rules are the stream's: renaming, removing, reordering
or changing the meaning of a field is a breaking change that bumps the schema
version; filling a field previously documented as *reserved* (already present
and zero, e.g. `stereo_correlation`) is **not** breaking. See
[`docs/feature-stream.md`](feature-stream.md#versioning-policy). In practice: a
file that reads only documented fields keeps working across non-breaking
releases, and a field currently reserved (so `0.0`) may simply come alive later
with no change to your file.

**Not in v1: `.milk` import.** Importing MilkDrop `.milk` presets is explicitly
out of scope for v1. scia presets and scenes are their own formats; there is no
converter, and dropping a `.milk` file into either directory does nothing.
