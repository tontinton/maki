local function highlight_to_view(view, content, ext)
  local highlighted = maki.ui.highlight(content, ext or "md")
  if not highlighted then
    return false
  end
  local w = math.max(1, math.floor(math.log(#highlighted, 10)) + 1)
  local fmt = "%" .. w .. "d "
  for idx, hl_line in ipairs(highlighted) do
    local spans = { { string.format(fmt, idx), "line_nr" } }
    for _, seg in ipairs(hl_line) do
      spans[#spans + 1] = seg
    end
    view:append(spans)
  end
  return true
end

return highlight_to_view
