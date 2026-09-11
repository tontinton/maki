local Picker = require("thinking_picker")
local Window = require("thinking_window")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local MODEL_ID = "claude-opus-5"
local WIDE = 52
local NARROW = 30
local MEDIUM = "medium"
local RAW_BUDGET = "8192"
local REJECTED = "Usage: /thinking [off|adaptive|minimal|low|medium|high|xhigh|max|<budget>]"
local UNSUPPORTED = "Thinking requires a model that supports it"
local CUSTOM_ROW_NAME = "custom…"
local BOOM = "the event loop blew up"
-- Shaped like the host's ladder: two modes, then six levels at 10% to 100% of
-- a 32k ceiling.
local LADDER = {
  { name = "off" },
  { name = "adaptive" },
  { name = "minimal", tokens = 3276 },
  { name = "low", tokens = 6553 },
  { name = "medium", tokens = 13107 },
  { name = "high", tokens = 19660 },
  { name = "xhigh", tokens = 26214 },
  { name = "max", tokens = 32768 },
}
-- A model whose output window is so small that every level clamps onto the
-- floor, so no gauge may claim one level buys more than another.
local CLAMPED_LADDER = {
  { name = "off" },
  { name = "adaptive" },
  { name = "minimal", tokens = 1024 },
  { name = "low", tokens = 1024 },
  { name = "max", tokens = 1024 },
}

local function model_with(thinking, options)
  return {
    id = MODEL_ID,
    thinking = thinking,
    thinking_options = options or LADDER,
  }
end

local function picker_on(thinking, options)
  return Picker.new(model_with(thinking, options))
end

local function dispw(s)
  return utf8.len(s) or #s
end

local function line_text(line)
  local parts = {}
  for _, span in ipairs(line) do
    parts[#parts + 1] = span[1]
  end
  return table.concat(parts)
end

-- The filled half of a gauge is its own span, the third one on a level row.
local function gauge_cells(state, lines, name)
  for i, row in ipairs(state.rows) do
    if row.option and row.option.name == name then
      return dispw(lines[i][3][1])
    end
  end
  error("no row named " .. name)
end

local function level_lines(state, lines)
  local out = {}
  for i, row in ipairs(state.rows) do
    if row.option and row.option.tokens then
      out[#out + 1] = lines[i]
    end
  end
  return out
end

-- Feeds keys and reports what came out: the value a commit would send, or the
-- action that ended the picker. That pair is the whole contract with the
-- window.
local function press(state, keys)
  for _, key in ipairs(keys) do
    local action = Picker.handle_key(state, key)
    if action then
      return action == "commit" and Picker.value(state) or action
    end
  end
  return Picker.value(state)
end

case("the_picker_opens_on_the_row_the_session_runs", function()
  eq(press(picker_on(MEDIUM), { "enter" }), MEDIUM)
end)

-- The one fallback: a value no row names is a raw token budget.
case("a_raw_budget_opens_the_custom_row_pre_filled", function()
  local state = picker_on(RAW_BUDGET)
  eq(state.cursor, state.custom_row)
  th.has(line_text(Picker.render(state, WIDE)[state.custom_row]), RAW_BUDGET .. " tokens")
  eq(press(state, { "enter" }), RAW_BUDGET)
end)

case("keys_pick_a_value_or_dismiss_the_picker", function()
  eq(press(picker_on(MEDIUM), { "down", "down", "enter" }), "xhigh")
  eq(press(picker_on(MEDIUM), { "up", "enter" }), "low")
  eq(press(picker_on(MEDIUM), { "esc" }), "cancel")
  eq(press(picker_on(MEDIUM), { "8", "1", "9", "2", "enter" }), "8192")
  eq(press(picker_on(MEDIUM), { "4", "0", "9", "6", "backspace", "enter" }), "409")
  eq(press(picker_on(MEDIUM), { "4", "esc", "enter" }), "4", "esc leaves the editor, not the picker")
end)

case("the_cursor_walks_over_blank_rows_and_stops_at_the_ends", function()
  local walked = {}
  local state = picker_on("off")
  eq(press(state, { "up" }), "off", "nothing above the first row")
  for _ = 1, #LADDER + 1 do
    Picker.handle_key(state, "down")
    walked[#walked + 1] = Picker.value(state)
  end
  -- The last step falls off the end of the list and stays on the custom row,
  -- which reports what is typed there: nothing yet.
  eq(table.concat(walked, ","), "adaptive,minimal,low,medium,high,xhigh,max,,")
end)

case("a_click_selects_a_row_and_ignores_the_rest", function()
  local state = picker_on(MEDIUM)
  local blank_row, off_row = 1, 2
  Picker.click(state, blank_row)
  eq(Picker.value(state), MEDIUM, "a blank row is not selectable")
  Picker.click(state, #state.rows + 1)
  eq(Picker.value(state), MEDIUM, "a row past the end is not a row")
  Picker.click(state, off_row)
  eq(Picker.value(state), "off")
end)

case("gauges_are_proportional_to_what_a_level_actually_costs", function()
  local state = picker_on(MEDIUM)
  local lines = Picker.render(state, WIDE)
  local previous = 0
  for _, option in ipairs(LADDER) do
    if option.tokens then
      local cells = gauge_cells(state, lines, option.name)
      eq(cells > previous, true, option.name .. " must draw a longer gauge than the level below")
      previous = cells
    end
  end

  local clamped = picker_on(MEDIUM, CLAMPED_LADDER)
  local clamped_lines = Picker.render(clamped, WIDE)
  eq(
    gauge_cells(clamped, clamped_lines, "minimal"),
    gauge_cells(clamped, clamped_lines, "max"),
    "levels that resolve to the same budget must draw the same gauge"
  )
end)

case("token_counts_stay_aligned_and_the_gauge_is_what_a_narrow_window_drops", function()
  local state = picker_on(MEDIUM)
  for _, width in ipairs({ WIDE, NARROW }) do
    local lines = Picker.render(state, width)
    for _, line in ipairs(lines) do
      eq(dispw(line_text(line)) <= width, true, "no row may run past a window " .. width .. " wide")
    end
    local rows = level_lines(state, lines)
    for _, line in ipairs(rows) do
      eq(dispw(line_text(line)), dispw(line_text(rows[1])), "every level ends in the same column at width " .. width)
    end
  end
  eq(#level_lines(state, Picker.render(state, WIDE))[1], 5, "a wide level carries a filled span and a rail")
  eq(#level_lines(state, Picker.render(state, NARROW))[1], 3, "a narrow one drops both")
end)

-- Proof that the ladder lives in Rust: a level this plugin has never heard of
-- is navigable and drawn like any other.
case("an_option_the_plugin_never_heard_of_renders_as_a_normal_row", function()
  local extended = {}
  for i, option in ipairs(LADDER) do
    extended[i] = option
  end
  extended[#extended + 1] = { name = "ultra", tokens = 65536 }
  local state = picker_on("max", extended)

  eq(press(state, { "down", "enter" }), "ultra")
  local lines = Picker.render(state, WIDE)
  th.has(line_text(lines[state.cursor]), "ultra")
  th.has(line_text(lines[state.cursor]), "65.5k")
  eq(gauge_cells(state, lines, "ultra") > gauge_cells(state, lines, "max"), true)
end)

case("the_custom_row_is_named_for_what_it_does", function()
  local state = picker_on(MEDIUM)
  th.has(line_text(Picker.render(state, WIDE)[state.custom_row]), CUSTOM_ROW_NAME)
end)

-- The stubbed half: the window against a scripted event list. Each case is one
-- way a picker can end, and asserts on what the host was asked to set.
local function key(name)
  return { type = "key", key = name }
end

local CLOSE = { type = "close" }

local function run(events, opts)
  opts = opts or {}
  local saved = { model = maki.model, ui = maki.ui }
  local sets, flashes, closed = {}, {}, false
  local index = 0

  local win = {
    recv = function()
      index = index + 1
      local ev = events[index]
      if ev == "raise" then
        error(BOOM)
      end
      return ev
    end,
    set_cursor = function() end,
    close = function()
      closed = true
    end,
  }

  maki.model = {
    get = function()
      return model_with(opts.thinking or MEDIUM, opts.options)
    end,
    set = function(args)
      sets[#sets + 1] = args.thinking
      if opts.reject then
        return nil, REJECTED
      end
      return { thinking = args.thinking }
    end,
  }
  maki.ui = {
    buf = function()
      return {
        set_lines = function() end,
        on = function() end,
      }
    end,
    open_win = function()
      return win
    end,
    flash = function(msg)
      flashes[#flashes + 1] = msg
    end,
    display_width = dispw,
    truncate_text = function(text)
      return { head = text }
    end,
    terminal_size = function()
      return { cols = 120, rows = 40 }
    end,
  }

  local ok, err = pcall(opts.direct and Window.set or Window.open, opts.direct)
  maki.model, maki.ui = saved.model, saved.ui
  return {
    sets = table.concat(sets, ","),
    flashes = table.concat(flashes, ","),
    closed = closed,
    err = not ok and tostring(err) or nil,
  }
end

case("enter_sends_the_picked_value_once_and_says_so", function()
  local result = run({ key("down"), key("down"), key("enter") })
  eq(result.sets, "xhigh")
  eq(result.flashes, "Thinking: xhigh")
  eq(result.closed, true)
end)

-- Nothing reaches the host until Enter, so there is never a value to undo.
case("a_dismissed_picker_sends_nothing", function()
  for name, events in pairs({
    esc = { key("down"), key("esc") },
    ["external close"] = { key("down"), CLOSE },
    ["dead channel"] = { key("down") },
  }) do
    local result = run(events)
    eq(result.sets, "", name)
    eq(result.closed, true, name)
  end
end)

-- A crash inside the loop still closes the window, and it is loud: a picker
-- that silently swallowed the error would leave the guard set and /thinking
-- dead for the rest of the run.
case("a_crash_closes_the_window_and_is_reported", function()
  local result = run({ "raise" })
  eq(result.sets, "")
  eq(result.closed, true)
  th.has(result.err or "", BOOM)
  eq(run({ key("enter") }).sets, MEDIUM, "the window reopens after a crash")
end)

case("a_refused_value_flashes_what_the_host_answered", function()
  eq(run({ key("enter") }, { reject = true }).flashes, REJECTED)
  eq(run({}, { direct = "nonsense", reject = true }).flashes, REJECTED)
end)

case("the_direct_form_skips_the_picker", function()
  local result = run({}, { direct = "high" })
  eq(result.sets, "high")
  eq(result.flashes, "Thinking: high")
  eq(result.closed, false, "nothing was opened")
end)

case("a_model_without_thinking_gets_the_flash_instead_of_a_picker", function()
  local result = run({}, { options = {} })
  eq(result.flashes, UNSUPPORTED)
  eq(result.sets, "")
end)

th.report()
