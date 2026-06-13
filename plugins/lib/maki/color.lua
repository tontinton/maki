local M = {}

function M.lerp(from, to, t)
  local fr, fg, fb = from:match("#(%x%x)(%x%x)(%x%x)")
  local tr, tg, tb = to:match("#(%x%x)(%x%x)(%x%x)")
  if not fr or not tr then
    return from
  end
  fr, fg, fb = tonumber(fr, 16), tonumber(fg, 16), tonumber(fb, 16)
  tr, tg, tb = tonumber(tr, 16), tonumber(tg, 16), tonumber(tb, 16)
  local r = math.floor(fr + (tr - fr) * t + 0.5)
  local g = math.floor(fg + (tg - fg) * t + 0.5)
  local b = math.floor(fb + (tb - fb) * t + 0.5)
  return string.format("#%02x%02x%02x", r, g, b)
end

function M.dim(color, factor)
  local bg = maki.ui.theme_color("background") or "#000000"
  return M.lerp(color, bg, factor)
end

return M
