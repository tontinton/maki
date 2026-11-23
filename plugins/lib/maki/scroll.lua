--!strict
-- Relative scrolling on top of maki.fn.winsaveview / winrestview.
-- Positive {delta} scrolls down, negative up. Returns (true, nil) or (nil, err).
local function scroll(delta: number): (boolean?, string?)
  local view, err = maki.fn.winsaveview()
  if not view then
    return nil, err
  end
  return maki.fn.winrestview({ topline = view.topline + delta })
end

return scroll
