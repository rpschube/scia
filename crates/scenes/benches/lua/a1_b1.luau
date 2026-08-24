-- P5 scene: feature-access A1 (features is a fresh Lua table each tick),
-- canvas B1 (per-primitive userdata method calls).
-- Returns a constructor so update/render are closures over local state; this
-- runs identically with or without Lua:sandbox(true) because nothing is a
-- script-defined global.
return function(ctx)
  local N = 200
  local barh = {}
  local px, py, vx, vy = {}, {}, {}, {}
  for i = 1, 64 do barh[i] = 0.0 end
  for i = 1, N do
    px[i] = (i % 64) * 2.0
    py[i] = i * 0.5
    vx[i] = 0.05
    vy[i] = -0.05
  end

  local function update(features, dt)
    local rms = features.rms
    local peak = features.peak
    local onset = features.onset
    local kick = onset and 0.02 or 0.0
    local spec = features.spectrum
    for i = 1, 64 do
      barh[i] = spec[i]
    end
    for i = 1, N do
      local b = barh[(i % 64) + 1]
      vx[i] = vx[i] + b * 0.01 + kick
      vy[i] = vy[i] + (rms - 0.5) * dt
      px[i] = px[i] + vx[i] * dt
      py[i] = py[i] + vy[i] * dt + peak * 0.001
    end
  end

  local function render(canvas)
    for i = 1, 64 do
      canvas:bar(i * 2.0, 0.0, 2.0, barh[i] * 100.0, i)
    end
    for i = 1, N do
      canvas:point(px[i], py[i], i % 64, 1.0)
    end
  end

  return update, render
end
