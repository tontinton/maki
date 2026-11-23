--!strict
local M = {}

function M.lerp(from: string, to: string, t: number): string?
  local fr, fg, fb = from:match("^#(%x%x)(%x%x)(%x%x)$")
  local tr, tg, tb = to:match("^#(%x%x)(%x%x)(%x%x)$")
  if not fr or not tr then
    return nil
  end
  -- The pattern matched, so every capture is two hex digits.
  local function channel(from_hex: string?, to_hex: string?): number
    local a, b = tonumber(from_hex, 16) :: number, tonumber(to_hex, 16) :: number
    return math.floor(a + (b - a) * t + 0.5)
  end
  return string.format("#%02x%02x%02x", channel(fr, tr), channel(fg, tg), channel(fb, tb))
end

function M.dim(color: string, factor: number): string?
  local background = maki.ui.theme_style("background")
  local bg = background and background.bg
  return if bg then M.lerp(color, bg, factor) else nil
end

return M
