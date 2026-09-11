-- State and rendering for the /thinking picker: a radio list of the ladder
-- `maki.model.get()` reported, where each effort level draws a gauge of what it
-- costs next to the priciest one. The rows, their order and their budgets all
-- come from Rust, so a level added there shows up here untouched.
--
-- Nothing in this file calls `maki.*`. It takes a model table in and answers
-- with lines and an action, which is what lets the spec drive it with plain
-- tables. `thinking_window.lua` owns the window and the round-trips.
--
-- Module names are shared across bundled plugins, hence the prefix: a plain
-- `picker` would collide with the one the task plugin ships.

local M = {}

local PICKED = "● "
local UNPICKED = "○ "
local INDENT = "  "
local NAME_GAP = 3
local VALUE_GAP = 2
local RIGHT_PAD = 2
-- Under this many cells a gauge cannot show a proportion honestly, so it goes.
local MIN_TRACK = 8
local CUSTOM_NAME = "custom…"
local CUSTOM_HINT = "type a budget"
local TOKENS_LABEL = " tokens"
local CARET = "▏"
local MAX_DIGITS = 7
local FILL = "━"
local RAIL = "┄"
local THOUSAND = 1000
local DIGIT = "^%d$"
-- Rows without a budget fill the wide column with this. A mode we have never
-- heard of gets nothing rather than invented prose.
local MODE_NOTES = { off = "no reasoning", adaptive = "model decides" }

local STYLE_NAME = "item"
local STYLE_NAME_PICKED = "bold"
local STYLE_DIM = "dim"
local STYLE_PICKED = "accent"
local STYLE_GAUGE = "thinking"

local function width_of(s)
  return utf8.len(s) or #s
end

local function pad_right(s, width)
  return s .. string.rep(" ", math.max(width - width_of(s), 0))
end

local function pad_left(s, width)
  return string.rep(" ", math.max(width - width_of(s), 0)) .. s
end

-- Notes are prose, so a window too narrow for one cuts it rather than letting
-- the row run past the border.
local function clip(s, width)
  if width_of(s) <= width then
    return s
  end
  return s:sub(1, math.max(width, 0))
end

local function token_label(tokens)
  if tokens < THOUSAND then
    return tostring(tokens)
  end
  return string.format("%.1fk", tokens / THOUSAND)
end

-- A row is an option, the custom one, or a blank spacer. Options that carry a
-- budget are the effort scale and the rest are modes, so the blank line keeps
-- dividing the two however the ladder changes.
local function build_rows(options)
  local modes, levels = {}, {}
  for _, option in ipairs(options) do
    local group = option.tokens and levels or modes
    group[#group + 1] = { option = option }
  end
  local rows = { {} }
  for _, group in ipairs({ modes, levels }) do
    for _, row in ipairs(group) do
      rows[#rows + 1] = row
    end
    rows[#rows + 1] = {}
  end
  rows[#rows + 1] = { custom = true }
  rows[#rows + 1] = {}
  return rows
end

local function selectable(row)
  return row.option ~= nil or row.custom == true
end

local function row_name(row)
  if row.option then
    return row.option.name
  end
  return row.custom and CUSTOM_NAME or ""
end

-- {model} is a `maki.model.get()` table. The cursor opens on the row named by
-- the value the session runs. A value no row names is a raw token budget, so
-- the custom row takes it, pre-filled.
function M.new(model)
  local rows = build_rows(model.thinking_options)
  local state = { rows = rows, custom = "", editing = false, max_tokens = 0 }
  for i, row in ipairs(rows) do
    if row.custom then
      state.custom_row = i
    elseif row.option then
      state.max_tokens = math.max(state.max_tokens, row.option.tokens or 0)
      if row.option.name == model.thinking then
        state.cursor = i
      end
    end
  end
  if not state.cursor then
    state.cursor = state.custom_row
    state.custom = model.thinking
  end
  return state
end

function M.value(state)
  local row = state.rows[state.cursor]
  return row.option and row.option.name or state.custom
end

-- Skips the blank spacers, and stops at either end instead of wrapping.
local function move(state, delta)
  local i = state.cursor + delta
  while state.rows[i] do
    if selectable(state.rows[i]) then
      state.cursor = i
      return
    end
    i = i + delta
  end
end

local function editor_key(state, key)
  if key:match(DIGIT) then
    if #state.custom < MAX_DIGITS then
      state.custom = state.custom .. key
    end
  elseif key == "backspace" then
    state.custom = state.custom:sub(1, -2)
  elseif key == "esc" then
    state.editing = false
  elseif key == "enter" and state.custom ~= "" then
    state.editing = false
    return "commit"
  end
end

-- Answers "commit", "cancel", or nothing for a key that only changed what is
-- on screen.
function M.handle_key(state, key)
  if state.editing then
    return editor_key(state, key)
  end
  if key:match(DIGIT) then
    state.cursor, state.custom, state.editing = state.custom_row, key, true
  elseif key == "up" or key == "k" then
    move(state, -1)
  elseif key == "down" or key == "j" then
    move(state, 1)
  elseif key == "enter" then
    -- An empty custom row has nothing to send yet, so Enter starts typing
    -- instead of committing.
    if state.cursor == state.custom_row and state.custom == "" then
      state.editing = true
    else
      return "commit"
    end
  elseif key == "esc" or key == "ctrl+c" then
    return "cancel"
  end
end

function M.click(state, row)
  if state.rows[row] and selectable(state.rows[row]) then
    state.cursor, state.editing = row, false
  end
end

-- What a budget-less row says in the wide column where the gauges are: a mode
-- describes itself, the custom row shows what is typed.
local function note_spans(state, row)
  if row.option then
    return { { MODE_NOTES[row.option.name] or "", STYLE_DIM } }
  end
  if state.editing then
    return { { state.custom, STYLE_NAME_PICKED }, { CARET, STYLE_PICKED }, { TOKENS_LABEL, STYLE_DIM } }
  end
  if state.custom == "" then
    return { { CUSTOM_HINT, STYLE_DIM } }
  end
  return { { state.custom .. TOKENS_LABEL, STYLE_DIM } }
end

-- Columns are budgeted in this order: names, then the token counts on the
-- right, then whatever is left over becomes the gauge track. A window too
-- narrow to hold an honest gauge drops the track and keeps the counts aligned.
local function layout(state, width)
  local name, value = 0, 0
  for _, row in ipairs(state.rows) do
    name = math.max(name, width_of(row_name(row)))
    if row.option and row.option.tokens then
      value = math.max(value, width_of(token_label(row.option.tokens)))
    end
  end
  name = name + NAME_GAP
  local room = width - width_of(INDENT) - width_of(PICKED) - name - RIGHT_PAD
  local track = room - value - VALUE_GAP
  return {
    name = name,
    value = value + VALUE_GAP,
    room = room,
    track = track >= MIN_TRACK and track or 0,
  }
end

local function row_spans(state, row, picked, cols)
  local spans = {
    { INDENT .. (picked and PICKED or UNPICKED), picked and STYLE_PICKED or STYLE_DIM },
    { pad_right(row_name(row), cols.name), picked and STYLE_NAME_PICKED or STYLE_NAME },
  }
  local tokens = row.option and row.option.tokens
  if not tokens then
    local left = cols.room
    for _, span in ipairs(note_spans(state, row)) do
      local text = clip(span[1], left)
      left = left - width_of(text)
      spans[#spans + 1] = { text, span[2] }
    end
    return spans
  end
  if cols.track > 0 then
    -- Proportional to the largest budget in the list rather than to a nominal
    -- percentage: when a small output window clamps several levels onto one
    -- budget, their gauges come out equal, as they should.
    local filled = math.max(math.floor(tokens / state.max_tokens * cols.track + 0.5), 1)
    spans[#spans + 1] = { string.rep(FILL, filled), picked and STYLE_PICKED or STYLE_GAUGE }
    spans[#spans + 1] = { string.rep(RAIL, cols.track - filled), STYLE_DIM }
  end
  spans[#spans + 1] = { pad_left(token_label(tokens), cols.value), picked and STYLE_NAME_PICKED or STYLE_DIM }
  return spans
end

function M.render(state, width)
  local cols = layout(state, width)
  local lines = {}
  for i, row in ipairs(state.rows) do
    lines[i] = selectable(row) and row_spans(state, row, i == state.cursor, cols) or { { "" } }
  end
  return lines
end

return M
