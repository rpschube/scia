-- ripple — a shipped Luau scene, and living documentation of the scalar-feature
-- half of the scene API.
--
-- Concentric rings breathe outward from the centre: loudness sets their glow,
-- every detected onset flares an envelope that lifts the whole figure, and the
-- rings drift in and out on their own slow clock. It is deliberately driven by
-- SCALAR features (loudness, onset, bass) plus tunable PARAMS — the spectrum
-- table is left to `swarm`, the other shipped scene.
--
-- ============================================================================
-- The scene contract
-- ============================================================================
-- A scene file returns ONE manifest table describing the scene and carrying its
-- lifecycle functions. The host owns its own Luau VM per scene instance, so the
-- `local`s below are private per-instance state — no globals, nothing shared.
--
--   init(ctx)          optional. ctx = { aspect, params, bars }.
--                      Called once, before the first update.
--   update(features,dt) required. Fold the newest features into local state.
--                      `dt` is seconds since the previous update.
--   render(canvas)     required. Draw the current state (host clears first).
--   state()  / restore(t)  optional. Carry continuity across a hot reload.
--
-- ============================================================================
-- The feature API (the `features` argument to update)
-- ============================================================================
-- Scalar fields, each already normalized for display:
--   features.rms / features.loud   loudness, ~0..1
--   features.peak                  hop peak sample, 0..1
--   features.flux                  spectral flux, 0..1
--   features.onset                 boolean: a transient this hop
--   features.onset_age             ms since the last onset
--   features.bass/.mid/.treb       band levels (1.0 == the band's own average)
--   features.beat_phase            0..1 position within the beat
--   features.beat                  beat-tracker confidence, 0..1
--   features.tempo                 BPM (0 while unlocked)
--   features.width                 stereo width, 0..1
--   features.bar_count             how many spectrum bars are valid
--   features:bar(i)                a 1-based spectrum read (see `swarm`)
--
-- ============================================================================
-- The canvas API (the `canvas` argument to render)
-- ============================================================================
-- Everything is in NORMALIZED coordinates: x and y are fractions in 0..1, the
-- origin is top-left, y grows downward. A scene never learns cell or pixel size,
-- so the same file drives a terminal and (later) a GPU presenter unchanged.
-- `slot` is a 0..7 palette index; `intensity` is 0..1. The host clamps every
-- argument, so an out-of-range value is safe, never a fault.
--   canvas:aspect()                      width/height, to keep a circle round
--   canvas:bar(x, y, w, h, slot, i)      filled rectangle
--   canvas:line(x0,y0,x1,y1,width,slot,i)  segment (width is a fraction of H)
--   canvas:point(x, y, size, slot, i)    particle (size is a fraction of H)
--   canvas:text(x, y, str, slot, i)      a text run
--   canvas:field(cols, rows, values, slot, i)  a coarse intensity grid

-- ---- per-instance state -----------------------------------------------------
local RING_POINTS = 64 -- points drawn around each ring
local params -- the live tuning table (ctx.params), refreshed by the host

local t = 0.0 -- seconds of scene time
local env = 0.0 -- onset envelope, 0..1
local loud = 0.0 -- smoothed loudness
local was_onset = false -- rising-edge detector so one onset fires once

-- ---- lifecycle --------------------------------------------------------------
local function init(ctx)
  -- Keep a handle on the tuning table the host updates in place each frame.
  params = ctx.params
end

local function update(features, dt)
  t = t + dt

  -- Params come in through `params.<key>`, already clamped to the manifest
  -- range. Read them fresh every frame so live tuning takes effect at once.
  local decay = params.decay
  local gain = params.gain

  -- Envelope: snap to 1 on the rising edge of an onset, then decay smoothly, so
  -- the flare is a curve rather than a single-frame spike.
  local onset = features.onset
  if onset and not was_onset then
    env = 1.0
  elseif decay > 0.0 then
    env = env * math.exp(-dt / decay)
  end
  was_onset = onset

  -- Follow loudness with a light low-pass so the glow does not flicker.
  local target = math.clamp(features.loud * gain, 0.0, 1.0)
  loud = loud + (target - loud) * math.min(1.0, dt * 8.0)
end

local function render(canvas)
  -- The aspect trick: to draw a physical circle in normalized space, scale the
  -- x offset by 1/aspect (a circle of radius R as a fraction of height H maps to
  -- x = 0.5 + (R/aspect)*cos, y = 0.5 + R*sin).
  local aspect = canvas:aspect()
  local rings = math.floor(params.rings + 0.5)
  local glow = 0.2 + 0.8 * loud -- base brightness floor so silence still shows

  for r = 1, rings do
    -- Each ring sits at its own radius and breathes on a slow, offset clock; the
    -- onset envelope pushes them all outward together.
    local base = (r / rings) * 0.42
    local breathe = 0.06 * math.sin(t * 0.8 + r * 1.3)
    local radius = base + breathe + env * 0.05
    -- Outer rings are dimmer; the envelope brightens every ring on a hit.
    local ring_i = math.clamp(glow * (1.0 - (r - 1) / rings) + env * 0.5, 0.0, 1.0)
    -- Palette slot walks with the ring so the figure is not one flat colour.
    local slot = (r - 1) % 8

    for p = 0, RING_POINTS - 1 do
      local theta = (p / RING_POINTS) * 2.0 * math.pi
      local x = 0.5 + (radius / aspect) * math.cos(theta)
      local y = 0.5 + radius * math.sin(theta)
      canvas:point(x, y, 0.012 + 0.02 * env, slot, ring_i)
    end
  end

  -- A single bright core that pops on every onset — bass gives it a little size.
  canvas:point(0.5, 0.5, 0.03 + 0.05 * env, 7, math.clamp(0.4 + env, 0.0, 1.0))
end

-- Continuity across a hot reload: carry the slow clock and the envelope so a
-- save mid-run does not visibly reset the animation. The host serializes a flat
-- bag of named numbers; a restoring scene reads only the keys it knows.
local function state()
  return { t = t, env = env, loud = loud }
end

local function restore(saved)
  t = saved.t or 0.0
  env = saved.env or 0.0
  loud = saved.loud or 0.0
end

-- ---- the manifest -----------------------------------------------------------
return {
  id = "ripple",
  mood = "serene",
  summary = "Concentric rings breathing outward from the centre; loudness sets their glow and every onset flares the whole figure.",
  params = {
    { key = "gain", default = 1.0, min = 0.0, max = 3.0, doc = "loudness sensitivity of the glow" },
    { key = "decay", default = 0.6, min = 0.05, max = 3.0, doc = "onset-flare decay time constant (seconds)" },
    { key = "rings", default = 4.0, min = 1.0, max = 8.0, doc = "how many concentric rings to draw" },
  },
  init = init,
  update = update,
  render = render,
  state = state,
  restore = restore,
}
