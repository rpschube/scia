# Luau scenes

A scene can be written in [Luau](https://luau.org/) and dropped in next to the
built-ins. A `.lua` scene binds to the same scene interface a built-in Rust scene
does — `init` / `update` / `render` against the feature stream and the abstract,
normalized canvas — so it lists in the scene browser and loads with
`--scene <id>` exactly like a built-in. Scenes are presenter-agnostic: they draw
in normalized coordinates and never learn the cell or pixel size, so one file
drives the terminal and (later) the GPU unchanged.

Two shipped scenes double as living documentation of the API, each exercising a
different half of it:

- **`ripple`** — scalar-feature-driven, with tunable params and continuity
  across a reload. Source:
  [`crates/scenes/src/luau/scenes/ripple.lua`](../crates/scenes/src/luau/scenes/ripple.lua).
- **`swarm`** — spectrum-driven, a 200-particle system reading the in-place
  spectrum table. Source:
  [`crates/scenes/src/luau/scenes/swarm.lua`](../crates/scenes/src/luau/scenes/swarm.lua).

New to authoring? Start from the runnable skeleton
[`templates/scene-template.lua`](../templates/scene-template.lua) (simpler than
either shipped scene) and the narrative guide
[`docs/authoring.md`](authoring.md), which walks the whole ladder — presets,
expressions, and Luau scenes — and the in-app author workflow. This file is the
API reference those point back to.

## Drop-in directory

Put your own `.lua` scenes in the scenes directory under the config dir (the same
root as `config.toml`):

- Unix: `$XDG_CONFIG_HOME/scia/scenes/`, else `~/.config/scia/scenes/`
- Windows: `%APPDATA%\scia\scenes\`

Every `*.lua` file there is discovered at startup and listed after the built-ins.
`scia --list-scenes` prints the resolved directory and tags each Luau scene with
`[luau]`. A file that fails to compile or is not a well-formed manifest is
skipped — it never shadows a working scene — and a drop-in whose `id` collides
with a built-in (or the reserved name `bars`) is ignored, so the built-in listing
is never disturbed.

## The manifest

A scene file returns one table describing the scene and carrying its lifecycle
functions:

```lua
return {
  id = "ripple",              -- stable machine id, ^[a-z0-9][a-z0-9-]*$
  mood = "serene",            -- one word, shown in the scene browser
  summary = "one-line …",     -- one line, shown in the scene browser
  params = {                  -- optional tuning manifest
    { key = "gain", default = 1.0, min = 0.0, max = 4.0, doc = "…" },
  },
  init = function(ctx) end,             -- optional; ctx = { aspect, params, bars }
  update = function(features, dt) end,  -- required
  render = function(canvas) end,        -- required
  state = function() return { k = 1.0 } end,  -- optional continuity out
  restore = function(saved) end,              -- optional continuity in
}
```

Each scene runs in its own VM, so the file's `local`s are private per-instance
state — there are no shared globals.

### Features (`update`)

Scalar fields, each already normalized for display: `loud` (normalized
loudness, `0..1`, level-independent — the everyday loudness field), `peak`,
`flux`, `onset` (boolean), `onset_age` (ms), `bass` / `mid` / `treb`,
`beat_phase`, `beat` (confidence), `tempo` (BPM), `width`, `bar_count`, and
`rms` — the raw signal loudness, the escape hatch for a scene that wants the
un-normalized level rather than `loud`. The
display spectrum crosses as one table the host rewrites in place each tick,
handed to `init` as `ctx.bars` (`bars[1..=bar_count]`); `features:bar(i)` is a
1-based convenience read of the same data.

### Canvas (`render`)

Everything is in normalized coordinates — `x`, `y` are fractions in `0..1`, the
origin top-left, `y` down. `slot` is a `0..7` palette index; `intensity` is
`0..1`. Every argument is clamped by the host, so an out-of-range value is safe.

- `canvas:aspect()` — width / height, to keep a circle round.
- `canvas:bar(x, y, w, h, slot, intensity)`
- `canvas:line(x0, y0, x1, y1, width, slot, intensity)`
- `canvas:point(x, y, size, slot, intensity)`
- `canvas:text(x, y, string, slot, intensity)`
- `canvas:field(cols, rows, values, slot, intensity)`

## Sandbox and limits

Scenes are sandboxed and cannot touch the filesystem, network or OS: the VM
loads only the safe libraries (`math`, `string`, `table`, `bit32`, `utf8`,
`coroutine`) — no `os`, `io` or `package` — and the dynamic-code base functions
(`load`, `loadstring`, `require`, `dofile`, `loadfile`, `getfenv`, `setfenv`,
`newproxy`, `collectgarbage`) are removed before the globals are frozen.

Three limits bound a tick: a per-VM memory cap, an instruction interrupt
enforcing a per-tick deadline, and Luau's built-in call-depth limit. If a script
runs away (an infinite loop, unbounded memory) or errors, the fault is
interrupted and reported — it never takes the app down. The scene freezes on its
last good frame, the same guarantee a preset hot reload gives: a live edit to a
drop-in that fails to compile or errors keeps the previous scene running and
surfaces the message.
