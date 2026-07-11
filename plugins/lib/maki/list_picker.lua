local TextInput = require("maki.text_input")

local ListPicker = {}
ListPicker.__index = ListPicker

local DETAIL_RIGHT_PAD = 2
local NO_MATCHES_LABEL = "  (no matches)"

local function is_header(item)
  return type(item) == "table" and item.header == true
end

local function label_of(item)
  return type(item) == "string" and item or item.label
end

local function search_text(item)
  local label = label_of(item)
  if type(item) == "table" and item.match_text and #item.match_text > 0 then
    return label .. " " .. item.match_text
  end
  return label
end

local function fuzzy_match(item, query)
  if query == "" then
    return nil
  end
  local label = label_of(item)
  local hay = search_text(item):lower()
  local q = query:lower()
  local label_len = #label
  local hpos = 1
  local label_indices = {}
  for qi = 1, #q do
    local needle = q:byte(qi)
    local found = false
    while hpos <= #hay do
      local ch = hay:byte(hpos)
      hpos = hpos + 1
      if ch == needle then
        if hpos - 1 <= label_len then
          label_indices[#label_indices + 1] = hpos - 1
        end
        found = true
        break
      end
    end
    if not found then
      return nil
    end
  end
  return label_indices
end

-- Group-aware filter. Header rows are kept iff at least one following child
-- (up to the next header) matches; headers with no matching children collapse.
local function filter_items(items, query)
  if query == "" then
    local indices = {}
    for i = 1, #items do
      indices[i] = i
    end
    return items, indices
  end
  local filtered, indices = {}, {}
  local i = 1
  while i <= #items do
    if is_header(items[i]) then
      local header_idx = i
      local matching = {}
      i = i + 1
      while i <= #items and not is_header(items[i]) do
        if fuzzy_match(items[i], query) then
          matching[#matching + 1] = i
        end
        i = i + 1
      end
      if #matching > 0 then
        filtered[#filtered + 1] = items[header_idx]
        indices[#indices + 1] = header_idx
        for _, ci in ipairs(matching) do
          filtered[#filtered + 1] = items[ci]
          indices[#indices + 1] = ci
        end
      end
    else
      if fuzzy_match(items[i], query) then
        filtered[#filtered + 1] = items[i]
        indices[#indices + 1] = i
      end
      i = i + 1
    end
  end
  return filtered, indices
end

local function label_spans(item, query, style, match_style)
  local label = label_of(item)
  local indices = fuzzy_match(item, query)
  if not indices or #indices == 0 then
    return { { label, style } }
  end
  local match_set = {}
  for _, idx in ipairs(indices) do
    match_set[idx] = true
  end
  local spans = {}
  local run_start = 1
  local in_match = false
  for pos = 1, #label do
    local is_m = match_set[pos] == true
    if is_m ~= in_match then
      if pos > run_start then
        spans[#spans + 1] = { label:sub(run_start, pos - 1), in_match and match_style or style }
      end
      run_start = pos
      in_match = is_m
    end
  end
  if #label >= run_start then
    spans[#spans + 1] = { label:sub(run_start, #label), in_match and match_style or style }
  end
  return spans
end

local function render_lines(items, selected, width, query)
  width = width or 80
  query = query or ""
  local lines = {}
  for i, item in ipairs(items) do
    local label = label_of(item)
    local detail = type(item) == "table" and item.detail or nil
    local is_sel = (i == selected)
    local is_hdr = is_header(item)

    local style, detail_style, match_style
    if is_hdr then
      style = "dim"
      detail_style = "dim"
    elseif is_sel then
      style = "selected"
      detail_style = "selected"
      match_style = "match_selected"
    else
      style = "item"
      detail_style = "dim"
      match_style = "match"
    end

    local spans = {}
    if is_hdr then
      spans[#spans + 1] = { label, style }
    else
      local lspans = label_spans(item, query, style, match_style)
      if #lspans > 0 then
        lspans[1][1] = "  " .. lspans[1][1]
      end
      for _, s in ipairs(lspans) do
        spans[#spans + 1] = s
      end
    end

    local label_w = #label
    if detail ~= nil then
      local detail_w = #detail
      local pad = width - 2 - label_w - detail_w - DETAIL_RIGHT_PAD
      if pad < 1 then
        pad = 1
      end
      spans[#spans + 1] = { string.rep(" ", pad), style }
      spans[#spans + 1] = { detail, detail_style }
      spans[#spans + 1] = { string.rep(" ", DETAIL_RIGHT_PAD), style }
    else
      local trail = width - 2 - label_w
      if trail > 0 then
        spans[#spans + 1] = { string.rep(" ", trail), style }
      end
    end

    lines[#lines + 1] = spans
  end
  return lines
end

local function next_selectable(items, from, delta)
  local i = from + delta
  while i >= 1 and i <= #items do
    if not is_header(items[i]) then
      return i
    end
    i = i + delta
  end
  return from
end

local function clamp_selectable(items, target)
  if #items == 0 then
    return 1
  end
  if target < 1 then
    target = 1
  end
  if target > #items then
    target = #items
  end
  if not is_header(items[target]) then
    return target
  end
  local fwd = next_selectable(items, target, 1)
  if fwd ~= target then
    return fwd
  end
  local bwd = next_selectable(items, target, -1)
  return bwd
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

  local cursor = clamp_selectable(filtered, opts.cursor or 1)

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
        local nxt = next_selectable(filtered, cursor, -1)
        if nxt ~= cursor then
          cursor = nxt
          win:set_cursor(cursor)
          buf:set_lines(build_lines())
        end
        confirming = nil
      elseif ev.key == "down" then
        local nxt = next_selectable(filtered, cursor, 1)
        if nxt ~= cursor then
          cursor = nxt
          win:set_cursor(cursor)
          buf:set_lines(build_lines())
        end
        confirming = nil
      elseif ev.key == "esc" or ev.key == "ctrl+c" then
        win:close()
        return { type = "close" }
      elseif ev.key == "ctrl+d" then
        if #filtered > 0 and not is_header(filtered[cursor]) then
          if confirming == cursor then
            win:close()
            return { type = "delete", index = original_indices[cursor] }
          else
            confirming = cursor
            maki.ui.flash("Press Ctrl+D again to delete")
          end
        end
      elseif submit_keys[ev.key] then
        if #filtered > 0 and not is_header(filtered[cursor]) then
          win:close()
          return { type = "choice", index = original_indices[cursor] }
        end
      else
        local result = input:handle_key(ev.key)
        if result == TextInput.Result.CHANGED then
          filtered, original_indices = filter_items(items, input:value())
          cursor = clamp_selectable(filtered, cursor)
          win:set_cursor(cursor)
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
ListPicker._fuzzy_match = fuzzy_match
ListPicker._is_header = is_header

return ListPicker
