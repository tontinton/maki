local TextInput = require("maki.text_input")

local ListPicker = {}
ListPicker.__index = ListPicker

local DETAIL_RIGHT_PAD = 2
local NO_MATCHES_LABEL = "  (no matches)"

local function filter_items(items, query)
  if query == "" then
    local indices = {}
    for i = 1, #items do
      indices[i] = i
    end
    return items, indices
  end
  local q = query:lower()
  local filtered, indices = {}, {}
  for i, item in ipairs(items) do
    local label = type(item) == "string" and item or item.label
    if label:lower():find(q, 1, true) then
      filtered[#filtered + 1] = item
      indices[#indices + 1] = i
    end
  end
  return filtered, indices
end

local function find_match_pos(label, query)
  if query == "" then
    return nil
  end
  local ll = label:lower()
  local ql = query:lower()
  local start = ll:find(ql, 1, true)
  if not start then
    return nil
  end
  return start, start + #ql - 1
end

local function render_lines(items, selected, width, query)
  width = width or 80
  query = query or ""
  local lines = {}
  for i, item in ipairs(items) do
    local label = type(item) == "string" and item or item.label
    local detail = type(item) == "table" and item.detail or nil
    local is_sel = (i == selected)
    local style = is_sel and "selected" or "item"
    local detail_style = is_sel and "selected" or "dim"
    local match_style = is_sel and "match_selected" or "match"

    local spans = {}
    local ms, me = find_match_pos(label, query)
    if ms then
      local before = label:sub(1, ms - 1)
      local match = label:sub(ms, me)
      local after = label:sub(me + 1)
      spans[#spans + 1] = { "  " .. before, style }
      spans[#spans + 1] = { match, match_style }
      spans[#spans + 1] = { after, style }
    else
      spans[#spans + 1] = { "  " .. label, style }
    end

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
  local width
  local input = TextInput.new()
  local filtered, original_indices = filter_items(items, "")

  local cursor = opts.cursor or 1
  if cursor > #filtered then
    cursor = #filtered
  end
  if cursor < 1 then
    cursor = 1
  end

  local function build_lines()
    local content
    if #filtered == 0 then
      content = { { { NO_MATCHES_LABEL, "dim" } } }
    else
      content = render_lines(filtered, cursor, width, input:value())
    end
    local r = input:render("\xe2\x9d\xaf ")
    for _, ln in ipairs(r.lines) do
      content[#content + 1] = ln
    end
    return content
  end

  local buf = maki.ui.buf()

  local border_chrome = 2
  local content_h = #items + 1
  local total_h = content_h + border_chrome

  local win = maki.ui.open_win(buf, {
    title = opts.title,
    footer = opts.footer,
    height = total_h,
    reserved_bottom = 1,
  })

  width = win.width
  buf:set_lines(build_lines())

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
      buf:set_lines(build_lines())
    elseif ev.type == "key" then
      if ev.key == "up" then
        if cursor > 1 then
          cursor = cursor - 1
          win:set_cursor(cursor)
          buf:set_lines(build_lines())
        end
        confirming = nil
      elseif ev.key == "down" then
        if cursor < #filtered then
          cursor = cursor + 1
          win:set_cursor(cursor)
          buf:set_lines(build_lines())
        end
        confirming = nil
      elseif ev.key == "esc" or ev.key == "ctrl+c" then
        win:close()
        return { type = "close" }
      elseif ev.key == "ctrl+d" then
        if #filtered > 0 then
          if confirming == cursor then
            win:close()
            return { type = "delete", index = original_indices[cursor] }
          else
            confirming = cursor
            maki.ui.flash("Press Ctrl+D again to delete")
          end
        end
      elseif submit_keys[ev.key] then
        if #filtered > 0 then
          win:close()
          return { type = "choice", index = original_indices[cursor] }
        end
      else
        local result = input:handle_key(ev.key)
        if result == TextInput.Result.CHANGED then
          filtered, original_indices = filter_items(items, input:value())
          if cursor > #filtered then
            cursor = #filtered
            if cursor < 1 then
              cursor = 1
            end
            win:set_cursor(cursor)
          end
          buf:set_lines(build_lines())
          confirming = nil
        elseif result == TextInput.Result.MOVED then
          buf:set_lines(build_lines())
          confirming = nil
        end
      end
    end
  end
end

ListPicker._render_lines = render_lines
ListPicker._filter_items = filter_items
ListPicker._find_match_pos = find_match_pos

return ListPicker
