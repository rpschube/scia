-- swarm — a shipped Luau scene, and living documentation of the SPECTRUM half
-- of the scene API and of a particle system.
--
-- A cloud of particles streams outward from the centre. Each particle is bound
-- to one spectrum band and rides that band's energy: loud bands fling their
-- particles fast, quiet bands let them drift, and every onset gives the whole
-- swarm an outward kick. Where `ripple` reads scalar features, `swarm` reads the
-- spectrum through `ctx.bars` — the ONE table the host rewrites in place each
-- tick, so a 200-particle scene allocates nothing per frame on either side.
--
-- See `ripple.lua` for the full contract; this file focuses on what it does
-- differently: the spectrum table and a per-particle update/render loop.

-- ---- tunables the host cannot see (fixed structure) -------------------------
local N = 200 -- particle count (also the perf-budget reference scene size)
local TWO_PI = 2.0 * math.pi
local GOLDEN = 2.399963229 -- golden angle, for an even initial spread

-- ---- per-instance state -----------------------------------------------------
local params -- live tuning table (ctx.params)
local bars -- the in-place spectrum table (ctx.bars); bars[i] is band i, 1-based
local bar_count = 1 -- how many spectrum bars are valid this hop

-- Particle arrays (structure-of-arrays keeps the inner loop tight).
local px, py = {}, {} -- position, normalized 0..1
local vx, vy = {}, {} -- velocity, normalized units per second
local ang = {} -- the particle's seed direction, reused on respawn
local band = {} -- which spectrum band (1-based) this particle rides
local was_onset = false

-- Seed one particle near the centre, heading along its stored angle.
local function spawn(i)
  local a = ang[i]
  -- A small random-free jitter from the index keeps them from starting stacked.
  local r = 0.02 + (i % 7) * 0.004
  px[i] = 0.5 + math.cos(a) * r
  py[i] = 0.5 + math.sin(a) * r
  vx[i] = 0.0
  vy[i] = 0.0
end

-- ---- lifecycle --------------------------------------------------------------
local function init(ctx)
  params = ctx.params
  bars = ctx.bars
  for i = 1, N do
    -- Deterministic spread (no math.random, so a golden test is reproducible):
    -- the golden angle gives an even fan, and the band assignment interleaves
    -- particles across the spectrum.
    ang[i] = (i * GOLDEN) % TWO_PI
    band[i] = i
    spawn(i)
  end
end

local function update(features, dt)
  -- How many bars are live this hop; fall back to 1 so the modulo is safe.
  bar_count = math.max(1, features.bar_count)

  local speed = params.speed
  local drag = params.drag
  local kick = params.kick

  -- One outward kick on the rising edge of an onset.
  local onset = features.onset
  local kicked = onset and not was_onset
  was_onset = onset

  local damp = math.max(0.0, 1.0 - drag * dt)

  for i = 1, N do
    -- Direction from the centre; if a particle is essentially at the centre,
    -- use its seed angle so it still has somewhere to go.
    local dx = px[i] - 0.5
    local dy = py[i] - 0.5
    local dist = math.sqrt(dx * dx + dy * dy)
    local ux, uy
    if dist > 1e-4 then
      ux, uy = dx / dist, dy / dist
    else
      ux, uy = math.cos(ang[i]), math.sin(ang[i])
    end

    -- The band energy this particle rides (1-based; wrap into the valid range).
    local b = ((band[i] - 1) % bar_count) + 1
    local energy = bars[b] or 0.0

    -- Accelerate outward with the band energy; add the onset kick.
    local accel = energy * speed
    if kicked then
      accel = accel + kick
    end
    vx[i] = (vx[i] + ux * accel * dt) * damp
    vy[i] = (vy[i] + uy * accel * dt) * damp

    px[i] = px[i] + vx[i] * dt
    py[i] = py[i] + vy[i] * dt

    -- Respawn once a particle leaves the field, so the swarm is self-sustaining.
    if dist > 0.72 or px[i] < 0.0 or px[i] > 1.0 or py[i] < 0.0 or py[i] > 1.0 then
      spawn(i)
    end
  end
end

local function render(canvas)
  local aspect = canvas:aspect()
  for i = 1, N do
    local b = ((band[i] - 1) % bar_count) + 1
    local energy = bars[b] or 0.0
    -- Brighter and larger when its band is hot; the palette slot walks the
    -- spectrum so bass and treble read as different colours.
    local intensity = math.clamp(0.25 + energy, 0.0, 1.0)
    local size = 0.008 + 0.02 * energy
    local slot = (b - 1) % 8
    -- The x coordinate is drawn as-is (positions already live in normalized
    -- space); aspect only matters if you want perfectly round motion — see the
    -- note in ripple.lua. Referencing it keeps the swarm honest across shapes.
    local _ = aspect
    canvas:point(px[i], py[i], size, slot, intensity)
  end
end

-- ---- the manifest -----------------------------------------------------------
return {
  id = "swarm",
  mood = "kinetic",
  summary = "A cloud of particles streaming outward from the centre; each rides one spectrum band, and every onset kicks the whole swarm.",
  params = {
    { key = "speed", default = 1.4, min = 0.0, max = 6.0, doc = "how hard band energy flings a particle" },
    { key = "drag", default = 0.6, min = 0.0, max = 4.0, doc = "velocity damping per second" },
    { key = "kick", default = 1.2, min = 0.0, max = 6.0, doc = "outward impulse on each onset" },
  },
  init = init,
  update = update,
  render = render,
}
