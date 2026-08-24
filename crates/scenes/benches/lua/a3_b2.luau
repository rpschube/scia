-- P5 scene: feature-access A3, canvas B2 (render fills ctx.dl, a flat display
-- list of numbers with fixed stride 6, and the host reads it back once per
-- tick). dl.n holds the count of populated slots.
return function(ctx)
  local bars = ctx.bars
  local dl = ctx.dl
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
    for i = 1, 64 do
      barh[i] = bars[i]
    end
    for i = 1, N do
      local b = barh[(i % 64) + 1]
      vx[i] = vx[i] + b * 0.01 + kick
      vy[i] = vy[i] + (rms - 0.5) * dt
      px[i] = px[i] + vx[i] * dt
      py[i] = py[i] + vy[i] * dt + peak * 0.001
    end
  end

  local function render()
    local n = 0
    for i = 1, 64 do
      dl[n + 1] = 0.0
      dl[n + 2] = i * 2.0
      dl[n + 3] = 0.0
      dl[n + 4] = 2.0
      dl[n + 5] = barh[i] * 100.0
      dl[n + 6] = i
      n = n + 6
    end
    for i = 1, N do
      dl[n + 1] = 1.0
      dl[n + 2] = px[i]
      dl[n + 3] = py[i]
      dl[n + 4] = i % 64
      dl[n + 5] = 1.0
      dl[n + 6] = 0.0
      n = n + 6
    end
    dl.n = n
  end

  return update, render
end
