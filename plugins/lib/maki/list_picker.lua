local TextInput = require("maki.text_input")
local Color = require("maki.color")

local ListPicker = {}
ListPicker.__index = ListPicker

local INDENT = 2
local DETAIL_RIGHT_PAD = 2
local MIN_DETAIL_COLS = 6
local ELLIPSIS = "…"
local DEFAULT_DETAIL_STYLE = "dim"
local PEER_BLEND = 0.25
local BORDER_CHROME = 2
local NO_MATCHES_LABEL = "  (no matches)"

-- A caller's spelling of a key as the host emits it, or nil with a warning
-- when maki cannot name it: a key that would never match is worth a line in
-- the log rather than a picker that quietly ignores one of its bindings.
local function canonical(lhs)
  local canon, err = maki.keymap.normalize(lhs)
  if not canon then
    maki.log.warn(("list_picker: dropping key %s: %s"):format(tostring(lhs), err))
  end
  return canon
end

-- {list} of key spellings as a set keyed by canonical notation.
local function key_set(list)
  local set = {}
  for _, lhs in ipairs(list or {}) do
    local canon = canonical(lhs)
    if canon then
      set[canon] = true
    end
  end
  return set
end

local function split_words(query)
  local words = {}
  for w in (query or ""):lower():gmatch("%S+") do
    words[#words + 1] = w
  end
  return words
end

-- Words may come in any order: "441 review" still hits "review gh pr 441".
local function matches(label, words)
  local hay = label:lower()
  for _, w in ipairs(words) do
    if not hay:find(w, 1, true) then
      return false
    end
  end
  return true
end

-- Word hits can overlap ("alpha" and "phab" in "alphabet"), which would nest
-- highlights, so the ranges are merged before styling.
local function match_ranges(label, words)
  local hay = label:lower()
  local ranges = {}
  for _, w in ipairs(words) do
    local s, e = hay:find(w, 1, true)
    if s then
      ranges[#ranges + 1] = { s, e }
    end
  end
  table.sort(ranges, function(a, b)
    return a[1] < b[1]
  end)
  local merged = {}
  for _, r in ipairs(ranges) do
    local last = merged[#merged]
    if last and r[1] <= last[2] + 1 then
      last[2] = math.max(last[2], r[2])
    else
      merged[#merged + 1] = r
    end
  end
  return merged
end

-- {ranges} are ascending, disjoint, 1-based inclusive byte ranges of {label},
-- the shape `maki.text.fuzzy` and the file ranking report matches in.
local function range_spans(label, ranges, base, match_style)
  if #ranges == 0 then
    return { { label, base } }
  end
  local spans, pos = {}, 1
  for _, r in ipairs(ranges) do
    if r[1] > pos then
      spans[#spans + 1] = { label:sub(pos, r[1] - 1), base }
    end
    spans[#spans + 1] = { label:sub(r[1], r[2]), match_style }
    pos = r[2] + 1
  end
  if pos <= #label then
    spans[#spans + 1] = { label:sub(pos), base }
  end
  return spans
end

local function highlight_spans(label, words, base, match_style)
  return range_spans(label, match_ranges(label, words), base, match_style)
end

local function item_label(item)
  return type(item) == "string" and item or item.label
end

local function item_section(item)
  return type(item) == "table" and item.section or nil
end

local function next_section(item, prev)
  local s = item_section(item)
  if s and s ~= prev then
    return s
  end
  return nil
end

local function section_rows(items)
  local n = 0
  local prev = nil
  for _, item in ipairs(items) do
    local s = next_section(item, prev)
    if s then
      n = n + 1
      prev = s
    end
  end
  if n == 0 then
    return 0
  end
  return item_section(items[1]) and 2 * n - 1 or 2 * n
end

local function filter_items(items, query)
  local words = split_words(query)
  if #words == 0 then
    local indices = {}
    for i = 1, #items do
      indices[i] = i
    end
    return items, indices
  end
  local filtered, indices = {}, {}
  for i, item in ipairs(items) do
    local section = item_section(item)
    local hay = section and (item_label(item) .. " " .. section) or item_label(item)
    if matches(hay, words) then
      filtered[#filtered + 1] = item
      indices[#indices + 1] = i
    end
  end
  return filtered, indices
end

local function as_parts(detail)
  if type(detail) ~= "table" then
    return detail ~= nil and { { detail, DEFAULT_DETAIL_STYLE } } or nil
  end
  return detail
end

local function item_detail(item)
  return type(item) == "table" and item.detail or nil
end

local function parts_width(parts)
  local w = 0
  for _, p in ipairs(parts) do
    w = w + maki.ui.display_width(p[1])
  end
  return w
end

local function cut(text, cols)
  if cols <= 0 then
    return ""
  end
  if maki.ui.display_width(text) <= cols then
    return text
  end
  return maki.ui.truncate_text(text, cols - 1).head .. ELLIPSIS
end

-- Cuts inside the part the limit falls in, so the ellipsis wears the style of
-- the text it replaced.
local function clamp_parts(parts, cols)
  local out, used = {}, 0
  for _, p in ipairs(parts) do
    local w = maki.ui.display_width(p[1])
    if used + w <= cols then
      out[#out + 1] = p
      used = used + w
    else
      local head = cut(p[1], cols - used)
      if #head > 0 then
        out[#out + 1] = { head, p[2] }
      end
      break
    end
  end
  return out
end

-- The part marked `elastic` gives up the cells so the parts after it stay
-- whole, which is how a row keeps a trailing column like a file size. With no
-- elastic part, or when even the fixed parts do not fit, the tail goes.
local function shrink_parts(parts, cols)
  local total = parts_width(parts)
  if total <= cols then
    return parts
  end
  for i, p in ipairs(parts) do
    if p.elastic then
      local room = cols - (total - maki.ui.display_width(p[1]))
      if room < 1 then
        break
      end
      local out = {}
      for j, q in ipairs(parts) do
        out[j] = j == i and { cut(q[1], room), q[2] } or q
      end
      return out
    end
  end
  return clamp_parts(parts, cols)
end

-- Holds INDENT + width(label) + pad + width(detail) + DETAIL_RIGHT_PAD == width
-- for every input, even widths too small for either side, since a row wider
-- than the float breaks its frame. The detail gives up cells first, down to
-- MIN_DETAIL_COLS, and only then does {label} shrink.
local function fit_row(label, parts, width)
  local usable = math.max((width or 0) - INDENT - DETAIL_RIGHT_PAD, 0)
  label = label or ""
  parts = parts or {}
  local lw = maki.ui.display_width(label)
  local dw = parts_width(parts)
  local gap = dw > 0 and 1 or 0

  if lw + gap + dw > usable and dw > 0 then
    local cols = math.max(usable - lw - gap, MIN_DETAIL_COLS)
    parts = shrink_parts(parts, math.max(math.min(cols, usable - gap), 0))
    dw = parts_width(parts)
    gap = dw > 0 and 1 or 0
  end
  if lw + gap + dw > usable then
    label = cut(label, math.max(usable - gap - dw, 0))
    lw = maki.ui.display_width(label)
  end
  return label, parts, usable - lw - dw
end

local function append_tail(spans, pad, detail, right_cols, style)
  if #detail == 0 then
    pad, right_cols = pad + right_cols, 0
  end
  if pad > 0 then
    spans[#spans + 1] = { string.rep(" ", pad), style }
  end
  for _, d in ipairs(detail) do
    spans[#spans + 1] = d
  end
  if right_cols > 0 then
    spans[#spans + 1] = { string.rep(" ", right_cols), style }
  end
end

-- {peer} comes from ListPicker._peer: rows sharing the selected row's key are
-- the same thing underneath, so they get the tint.
local function render_lines(items, selected, width, query, peer)
  width = width or 80
  local words = split_words(query)
  -- On a very narrow window the pads themselves give way, so the row still
  -- ends at {width}.
  local indent_cols = math.min(INDENT, width)
  local right_cols = math.min(DETAIL_RIGHT_PAD, width - indent_cols)
  local indent = string.rep(" ", indent_cols)
  local sel_key = peer and items[selected] and peer.key(items[selected]) or nil
  local peer_style = peer and peer.style
  -- Bold rather than a color, so a match stays legible on the tint without the
  -- picker knowing any theme.
  local peer_match = peer_style and { fg = peer_style.fg, bg = peer_style.bg, bold = true }

  local function detail_spans(parts, is_sel, is_peer)
    local spans = {}
    for i, p in ipairs(parts) do
      local style = p[2] or DEFAULT_DETAIL_STYLE
      if is_sel then
        style = "selected"
      elseif is_peer then
        style = peer.detail_style(style)
      end
      spans[i] = { p[1], style }
    end
    return spans
  end

  local lines = {}
  local item_lines = {}
  local prev_section = nil
  for i, item in ipairs(items) do
    local section = next_section(item, prev_section)
    local is_sel = (i == selected)
    local is_peer = not is_sel and sel_key ~= nil and peer.key(item) == sel_key
    local style = is_sel and "selected" or (is_peer and peer_style or "item")
    local match_style = is_sel and "match_selected" or (is_peer and peer_match or "match")

    if section then
      if #lines > 0 then
        lines[#lines + 1] = {}
      end
      local sec, sec_detail, sec_pad = fit_row(section, as_parts(item.section_detail), width)
      local header = { { indent .. sec, "keybind_section" } }
      append_tail(header, sec_pad, detail_spans(sec_detail), right_cols, "keybind_section")
      lines[#lines + 1] = header
      prev_section = section
    end

    item_lines[i] = #lines + 1

    local label, detail, pad = fit_row(item_label(item), as_parts(item_detail(item)), width)
    local spans = highlight_spans(label, words, style, match_style)
    if indent_cols > 0 then
      if spans[1][2] == style then
        spans[1][1] = indent .. spans[1][1]
      else
        table.insert(spans, 1, { indent, style })
      end
    end
    append_tail(spans, pad, detail_spans(detail, is_sel, is_peer), right_cols, style)

    lines[#lines + 1] = spans
  end
  return lines, item_lines
end

-- The palette for rows that share an identity. {style_of} is
-- maki.ui.theme_style, passed in so a test can hand over its own. A peer's
-- detail keeps the color it wears everywhere else, only over the tint, so a
-- size or a tag list never reads like part of the label.
function ListPicker._peer(key_fn, style_of)
  local fg_of = function(name)
    local style = style_of(name)
    return style and style.fg
  end
  local selected = style_of("item_selected")
  local sel = selected and selected.bg
  local background = style_of("background")
  local bg = background and background.bg
  -- A palette color has no numeric value to blend (same constraint as
  -- plugins/grep/init.lua), so peers take the selection color as foreground
  -- instead and still stand out.
  local tint = bg and sel and Color.lerp(bg, sel, PEER_BLEND)
  return {
    key = key_fn,
    style = tint and { fg = fg_of("item"), bg = tint } or (sel and { fg = sel }),
    detail_style = function(role)
      if not tint then
        return role
      end
      local fg = fg_of(role)
      return fg and { fg = fg, bg = tint } or { bg = tint, dim = true }
    end,
  }
end

-- After a swap the cursor follows the row carrying {prev_key}, else it stays
-- where it was.
function ListPicker._select_after_swap(filtered, key_fn, prev_key, prev_cursor)
  if #filtered == 0 then
    return 1
  end
  if key_fn and prev_key ~= nil then
    for i, item in ipairs(filtered) do
      if key_fn(item) == prev_key then
        return i
      end
    end
  end
  return math.max(math.min(prev_cursor or 1, #filtered), 1)
end

-- Draws the filter query and its blank spacer into {lines}, pins that height on
-- {win} and returns it, which is also the first scrollable line. Drawing and
-- pinning belong together: a query that wraps, or one pasted with a newline,
-- makes the header taller than a picker would guess, and a reserved_top guessed
-- elsewhere then mis-scrolls the list.
function ListPicker.render_header(win, lines, input, prefix, inner)
  for _, ln in ipairs(input:render(prefix, utf8.len(prefix) or #prefix, inner).lines) do
    lines[#lines + 1] = ln
  end
  lines[#lines + 1] = {}
  win:set_config({ reserved_top = #lines })
  return #lines
end

local function content_height(items)
  return #items + section_rows(items) + 1
end

-- Open a fuzzy-filter picker in a floating window and block until the user
-- decides.
--
-- {items} is a list of strings or of
-- { label, detail?, section?, section_detail? } tables. A detail is a string, or
-- a list of { text, style } parts when the right of a row needs more than one
-- color. Mark one part `elastic = true` and that is the part a narrow row cuts,
-- leaving the parts after it whole.
--
-- {opts}:
--   title, footer, cursor (initial index)
--   submit_keys: extra submit keys besides <CR>
--   action_keys: keys that close the picker and report themselves, like { "R" }
--     for a refresh binding. Use uppercase keys, lowercase ones keep feeding
--     the filter
--   live_keys: { [key] = function(item|nil) -> items|nil }, keys that swap the
--     list in place, keeping the typed query. Called with the selected item, or
--     nil when nothing matches, and returning nil leaves the list alone.
--     Handlers run inside the render loop, so keep them cheap and hand anything
--     slow to an action_key
--   key: function(item) -> string|nil, a row's identity. Rows sharing the
--     selected row's key are tinted, and the cursor follows its key across a
--     live swap
--
-- Keys you pass go through `maki.keymap.normalize`, so `"<Enter>"` and
-- `"<CR>"` are the same binding. An invalid key is dropped with a warning.
--
-- Returns { type = "choice"|"delete", index, item },
-- { type = "key", key, index?, item? } or { type = "close" }. Prefer {item},
-- since {index} points into an {items} a live swap may have replaced.
function ListPicker.open(items, opts)
  opts = opts or {}
  local submit_keys = key_set(opts.submit_keys)
  submit_keys["<CR>"] = true
  local action_keys = key_set(opts.action_keys)
  local live_keys = {}
  for lhs, handler in pairs(opts.live_keys or {}) do
    local canon = canonical(lhs)
    if canon then
      live_keys[canon] = handler
    end
  end
  local key_fn = opts.key
  -- Resolved once: a theme cannot change while this float holds focus.
  local peer = key_fn and ListPicker._peer(key_fn, maki.ui.theme_style) or nil
  local width
  local input = TextInput.new()
  local filtered, original_indices = filter_items(items, "")

  local cursor = math.max(math.min(opts.cursor or 1, #filtered), 1)
  local item_lines = {}

  local function build_lines()
    local content
    if #filtered == 0 then
      content = { { { NO_MATCHES_LABEL, "dim" } } }
      item_lines = {}
    else
      content, item_lines = render_lines(filtered, cursor, width, input:value(), peer)
    end
    local r = input:render("\xe2\x9d\xaf ")
    for _, ln in ipairs(r.lines) do
      content[#content + 1] = ln
    end
    return content
  end

  local buf = maki.ui.buf()

  local win = maki.ui.open_win(buf, {
    title = opts.title,
    footer = opts.footer,
    height = content_height(items) + BORDER_CHROME,
    reserved_bottom = 1,
  })

  width = win.width
  local height = win.height
  local confirming = nil

  local function move_cursor(to)
    if #filtered > 0 then
      cursor = math.max(math.min(to, #filtered), 1)
    end
    buf:set_lines(build_lines())
    if item_lines[cursor] then
      win:set_cursor(item_lines[cursor])
    end
    confirming = nil
  end

  local function page_size()
    return math.max(height - 2, 1)
  end

  buf:set_lines(build_lines())
  if #filtered > 0 then
    move_cursor(cursor)
  end

  while true do
    local ev = win:recv()
    if not ev or ev.type == "close" then
      return { type = "close" }
    end

    if ev.type == "resize" then
      width = ev.width
      height = ev.height
      move_cursor(cursor)
    elseif ev.type == "key" then
      if ev.key == "<Up>" then
        move_cursor((cursor - 2) % math.max(#filtered, 1) + 1)
      elseif ev.key == "<Down>" then
        move_cursor(cursor % math.max(#filtered, 1) + 1)
      elseif ev.key == "<PageUp>" then
        move_cursor(cursor - page_size())
      elseif ev.key == "<PageDown>" then
        move_cursor(cursor + page_size())
      elseif ev.key == "<Esc>" or ev.key == "<C-c>" then
        win:close()
        return { type = "close" }
      elseif ev.key == "<C-d>" then
        if #filtered > 0 then
          if confirming == cursor then
            win:close()
            return { type = "delete", index = original_indices[cursor], item = filtered[cursor] }
          else
            confirming = cursor
            maki.ui.flash("Press Ctrl+D again to delete")
          end
        end
      elseif submit_keys[ev.key] then
        if #filtered > 0 then
          win:close()
          return { type = "choice", index = original_indices[cursor], item = filtered[cursor] }
        end
      elseif live_keys[ev.key] then
        local selected = filtered[cursor]
        local swapped = live_keys[ev.key](selected)
        if swapped then
          local prev_key = key_fn and selected and key_fn(selected) or nil
          items = swapped
          filtered, original_indices = filter_items(items, input:value())
          -- The host answers with a resize event the loop already re-renders
          -- on, so there is no height bookkeeping here.
          win:set_config({ height = content_height(items) + BORDER_CHROME })
          move_cursor(ListPicker._select_after_swap(filtered, key_fn, prev_key, cursor))
        end
      elseif action_keys[ev.key] then
        win:close()
        return {
          type = "key",
          key = ev.key,
          index = #filtered > 0 and original_indices[cursor] or nil,
          item = filtered[cursor],
        }
      else
        local result = input:handle_key(ev.key)
        if result == TextInput.Result.CHANGED then
          filtered, original_indices = filter_items(items, input:value())
          move_cursor(1)
        elseif result == TextInput.Result.MOVED then
          move_cursor(cursor)
        end
      end
    end
  end
end

ListPicker.split_words = split_words
ListPicker.matches = matches
ListPicker.highlight_spans = highlight_spans
ListPicker.range_spans = range_spans

ListPicker._render_lines = render_lines
ListPicker._filter_items = filter_items
ListPicker._section_rows = section_rows
ListPicker._fit_row = fit_row
ListPicker._key_set = key_set

return ListPicker
