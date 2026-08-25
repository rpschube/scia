-- scene-template.lua — a minimal, runnable starting point for your own scene.
--
-- Copy this file into your scenes directory, rename it, change the `id`, and
-- start editing. It draws two things: one vertical bar that grows with
-- loudness, and a dot in the middle that flares on every onset. That is the
-- whole scene — deliberately smaller than the shipped `ripple` and `swarm`
-- scenes, so the shape of a scene is easy to see before you add to it.
--
-- Where this file goes (the same root as config.toml):
--   Unix:    ~/.config/scia/scenes/       (or $XDG_CONFIG_HOME/scia/scenes/)
--   Windows: %APPDATA%\scia\scenes\
-- Then run it with:  scia --scene <id>     (the id you set in the manifest below)
-- Or run it straight from a path and hot-reload on every save:
--                    scia --demo --scene-file path/to/scene-template.lua
--
-- The full guide (every field, the sandbox rules, the in-app author mode) lives
-- in docs/authoring.md. The scripting API reference is docs/scenes.md.

-- ===========================================================================
-- The scene contract
-- ===========================================================================
-- A scene file returns ONE table (the "manifest") that describes the scene and
-- carries its lifecycle functions. The host gives every scene its own Luau VM,
-- so the `local`s below are private per-instance state — there are no globals
-- and nothing is shared between scenes.
--
--   init(ctx)           optional. Called once before the first update.
--                       ctx = { aspect, params, bars }.
--   update(features, dt) required. Fold the newest audio features into state.
--                       `dt` is seconds since the previous update.
--   render(canvas)      required. Draw the current state (the host clears first).
--   state() / restore(t) optional. Carry continuity across a hot reload so a
--                       save mid-run does not visibly reset the animation.

-- ---- per-instance state ----------------------------------------------------
local params -- the live tuning table (ctx.params), refreshed by the host
local level = 0.0 -- smoothed loudness, 0..1
local pulse = 0.0 -- onset flare envelope, 0..1
local was_onset = false -- rising-edge detector so one onset fires once

-- ---- lifecycle -------------------------------------------------------------
local function init(ctx)
  -- Keep a handle on the tuning table the host updates in place each frame.
  -- Read params through it (params.<key>); each value is already clamped to the
  -- manifest range below.
  params = ctx.params
end

local function update(features, dt)
  -- Follow loudness with a light low-pass so the bar does not flicker.
  local target = math.clamp(features.loud * params.sensitivity, 0.0, 1.0)
  level = level + (target - level) * math.min(1.0, dt * 6.0)

  -- Snap the pulse to 1 on the rising edge of an onset, then decay smoothly, so
  -- the flare is a curve rather than a single-frame spike.
  local onset = features.onset
  if onset and not was_onset then
    pulse = 1.0
  else
    pulse = pulse * math.exp(-dt / 0.35)
  end
  was_onset = onset
end

local function render(canvas)
  -- Everything is in NORMALIZED coordinates: x and y are fractions in 0..1, the
  -- origin is top-left, y grows downward. `slot` is a 0..7 palette index and
  -- `intensity` is 0..1; the host clamps every argument, so a value slightly out
  -- of range is safe.

  -- A single bar, centred, growing up from the bottom as loudness rises.
  local h = 0.1 + 0.8 * level
  canvas:bar(0.45, 1.0 - h, 0.10, h, 3, math.clamp(0.3 + level, 0.0, 1.0))

  -- A dot in the middle that pops on every onset.
  canvas:point(0.5, 0.5, 0.02 + 0.06 * pulse, 7, math.clamp(0.2 + pulse, 0.0, 1.0))
end

-- Continuity across a hot reload: hand back a flat bag of named numbers, and
-- read only the keys you know when restoring.
local function state()
  return { level = level, pulse = pulse }
end

local function restore(saved)
  level = saved.level or 0.0
  pulse = saved.pulse or 0.0
end

-- ---- the manifest ----------------------------------------------------------
return {
  -- Required. A stable machine id, ^[a-z0-9][a-z0-9-]*$ (lowercase, digits,
  -- dashes; must start alphanumeric). This is the name you pass to `--scene`.
  -- Rename it when you copy this file so it does not clash with the template.
  id = "scene-template",
  -- Required. A one-word mood, shown in the scene browser.
  mood = "template",
  -- Required. A one-line summary, shown in the scene browser.
  summary = "A starter scene: one loudness bar and an onset pulse — copy it and make it yours.",
  -- Optional. The tuning manifest: the params a preset (or the in-app tuning
  -- strip) may set, each with a default, a range and a one-line doc. Omit the
  -- whole table for a scene with nothing to tune.
  params = {
    { key = "sensitivity", default = 1.0, min = 0.0, max = 4.0, doc = "how strongly loudness drives the bar height" },
  },
  init = init,
  update = update,
  render = render,
  state = state,
  restore = restore,
}
