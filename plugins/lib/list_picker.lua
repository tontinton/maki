local ListPicker = {}
ListPicker.__index = ListPicker

local DEFAULT_WIDTH = 80
local DETAIL_RIGHT_PAD = 2

local function render_lines(items, selected, width)
  width = width or DEFAULT_WIDTH
  local lines = {}
  for i, item in ipairs(items) do
    local label = type(item) == "string" and item or item.label
    local detail = type(item) == "table" and item.detail or nil
    local is_sel = (i == selected)
    local style = is_sel and "cmd_selected" or "cmd_name"
    local detail_style = is_sel and "cmd_selected" or "cmd_desc"

    local spans = {}
    spans[#spans + 1] = { "  " .. label, style }

    if detail then
      local pad = width - 2 - #label - #detail - DETAIL_RIGHT_PAD
      if pad < 1 then
        pad = 1
      end
      spans[#spans + 1] = { string.rep(" ", pad), style }
      spans[#spans + 1] = { detail, detail_style }
      spans[#spans + 1] = { string.rep(" ", DETAIL_RIGHT_PAD), style }
    else
      local trail = width - 2 - #label
      if trail > 0 then
        spans[#spans + 1] = { string.rep(" ", trail), style }
      end
    end

    lines[#lines + 1] = spans
  end
  return lines
end

function ListPicker.open(items, opts)
  opts = opts or {}
  local submit_keys = { enter = true }
  if opts.submit_keys then
    for _, k in ipairs(opts.submit_keys) do
      submit_keys[k] = true
    end
  end
  local width = DEFAULT_WIDTH
  local cursor = opts.cursor or 1
  if cursor > #items then
    cursor = #items
  end
  if cursor < 1 then
    cursor = 1
  end
  local buf = maki.ui.buf()
  buf:set_lines(render_lines(items, cursor, width))

  local win = maki.ui.open_win(buf, {
    title = opts.title,
    footer = opts.footer,
  })

  if cursor > 1 then
    win:set_cursor(cursor)
  end
  local confirming = nil

  while true do
    local ev = win:recv()
    if not ev or ev.type == "close" then
      return { type = "close" }
    end

    if ev.type == "resize" then
      width = ev.width
      buf:set_lines(render_lines(items, cursor, width))
    elseif ev.type == "key" then
      local new_cursor = ev.cursor or cursor
      if new_cursor ~= cursor then
        cursor = new_cursor
        buf:set_lines(render_lines(items, cursor, width))
      end

      if submit_keys[ev.key] then
        win:close()
        return { type = "choice", index = cursor }
      elseif ev.key == "ctrl+d" then
        if confirming == cursor then
          win:close()
          return { type = "delete", index = cursor }
        else
          confirming = cursor
          maki.ui.flash("Press Ctrl+D again to delete")
        end
      else
        confirming = nil
      end
    end
  end
end

ListPicker._render_lines = render_lines

return ListPicker
