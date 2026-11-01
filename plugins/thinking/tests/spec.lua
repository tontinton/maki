local Picker = require("thinking_picker")
local Window = require("thinking_window")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local MODEL_ID = "claude-opus-5"
local WIDE = 52
local NARROW = 30
local MEDIUM = "medium"
local ADAPTIVE = "adaptive"
local HIGH = "high"
local RAW_BUDGET = "8192"
local TOKENS_LABEL = " tokens"
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
local NATIVE_LADDER = {
  { name = "off" },
  { name = "adaptive" },
  { name = "low" },
  { name = "medium" },
  { name = "xhigh" },
}
local TOGGLE_LADDER = { { name = "off" }, { name = "adaptive" } }

-- What a model without an adaptive mode offers: the host's ladder minus that
-- row.
local FIXED_LADDER = {}
for _, option in ipairs(LADDER) do
  if option.name ~= ADAPTIVE then
    FIXED_LADDER[#FIXED_LADDER + 1] = option
  end
end

local function model_with(thinking, options, shown)
  return {
    id = MODEL_ID,
    thinking = thinking,
    thinking_shown = shown or thinking,
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
  eq(press(picker_on(MEDIUM), { "<CR>" }), MEDIUM)
end)

-- The session still holds `adaptive`, but the row is gone, so the level it is
-- sent as takes the cursor instead of the custom row.
case("a_hidden_adaptive_opens_on_the_level_it_is_sent_as", function()
  local state = Picker.new(model_with(ADAPTIVE, FIXED_LADDER, HIGH))
  eq(state.rows[state.cursor].option.name, HIGH)
end)

-- Enter straight away keeps `adaptive`, so a session never stores the level
-- without the user picking it. Once the cursor has moved, every row is a
-- choice, the opened one included.
case("confirming_the_opened_row_keeps_a_hidden_adaptive", function()
  local function hidden()
    return Picker.new(model_with(ADAPTIVE, FIXED_LADDER, HIGH))
  end
  eq(press(hidden(), { "<CR>" }), ADAPTIVE)
  eq(press(hidden(), { "<Up>", "<CR>" }), MEDIUM)
  eq(press(hidden(), { "<Up>", "<Down>", "<CR>" }), HIGH)
  local clicked = hidden()
  Picker.click(clicked, clicked.cursor)
  eq(Picker.value(clicked), ADAPTIVE, "a click on the opened row is not a move")
end)

case("native_levels_skip_unsupported_choices_and_do_not_claim_token_budgets", function()
  local state = picker_on(MEDIUM, NATIVE_LADDER)
  eq(press(state, { "<Down>", "<CR>" }), "xhigh")
  eq(press(state, { "<Up>", "<Up>", "<CR>" }), "low")
  local lines = Picker.render(state, WIDE)
  local names = {}
  for i, row in ipairs(state.rows) do
    if row.option then
      names[#names + 1] = row.option.name
      eq(line_text(lines[i]):find(TOKENS_LABEL, 1, true), nil)
    end
  end
  eq(table.concat(names, ","), "off,adaptive,low,medium,xhigh")
  eq(#level_lines(state, lines), 0)
end)

case("toggle_only_models_offer_modes_without_token_counts", function()
  local state = picker_on(ADAPTIVE, TOGGLE_LADDER)
  eq(state.rows[state.cursor].option.name, ADAPTIVE)
  eq(press(state, { "<Up>", "<CR>" }), "off")
  eq(press(state, { "<Down>", "<CR>" }), ADAPTIVE)
  local lines = Picker.render(state, WIDE)
  for i, row in ipairs(state.rows) do
    if row.option then
      eq(line_text(lines[i]):find(TOKENS_LABEL, 1, true), nil)
    end
  end
  eq(press(state, { "8", "1", "9", "2", "<CR>" }), RAW_BUDGET)
end)

case("snapped_effort_opens_the_effective_row_without_rewriting_intent", function()
  local state = Picker.new(model_with(HIGH, NATIVE_LADDER, MEDIUM))
  eq(state.rows[state.cursor].option.name, MEDIUM)
  eq(press(state, { "<CR>" }), HIGH)
  eq(state.custom, "")
  eq(press(state, { "<Up>", "<Down>", "<CR>" }), MEDIUM)
end)

case("toggle_row_keeps_the_delegation_ceiling_until_the_user_changes_it", function()
  local state = Picker.new(model_with(HIGH, TOGGLE_LADDER, ADAPTIVE))
  eq(state.rows[state.cursor].option.name, ADAPTIVE)
  eq(press(state, { "<CR>" }), HIGH)
  eq(state.custom, "")
  eq(press(state, { "<Up>", "<Down>", "<CR>" }), ADAPTIVE)
end)

case("native_budget_opens_a_named_row_without_a_fake_token_count", function()
  local state = Picker.new(model_with(RAW_BUDGET, NATIVE_LADDER, "xhigh"))
  eq(state.rows[state.cursor].option.name, "xhigh")
  eq(state.custom, "")
  eq(press(state, { "<CR>" }), RAW_BUDGET)
  local lines = Picker.render(state, WIDE)
  for _, line in ipairs(lines) do
    eq(line_text(line):find(TOKENS_LABEL, 1, true), nil)
  end
  eq(press(state, { "<Up>", "<Down>", "<CR>" }), "xhigh")
end)

-- The one fallback: a value no row names is a raw token budget.
case("a_raw_budget_opens_the_custom_row_pre_filled", function()
  local state = picker_on(RAW_BUDGET)
  eq(state.cursor, state.custom_row)
  th.has(line_text(Picker.render(state, WIDE)[state.custom_row]), RAW_BUDGET .. " tokens")
  eq(press(state, { "<CR>" }), RAW_BUDGET)
end)

case("keys_pick_a_value_or_dismiss_the_picker", function()
  eq(press(picker_on(MEDIUM), { "<Down>", "<Down>", "<CR>" }), "xhigh")
  eq(press(picker_on(MEDIUM), { "<Up>", "<CR>" }), "low")
  eq(press(picker_on(MEDIUM), { "<Esc>" }), "cancel")
  eq(press(picker_on(MEDIUM), { "8", "1", "9", "2", "<CR>" }), "8192")
  eq(press(picker_on(MEDIUM), { "4", "0", "9", "6", "<BS>", "<CR>" }), "409")
  eq(press(picker_on(MEDIUM), { "4", "<Esc>", "<CR>" }), "4", "esc leaves the editor, not the picker")
end)

case("the_cursor_walks_over_blank_rows_and_stops_at_the_ends", function()
  local walked = {}
  local state = picker_on("off")
  eq(press(state, { "<Up>" }), "off", "nothing above the first row")
  for _ = 1, #LADDER + 1 do
    Picker.handle_key(state, "<Down>")
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
-- A level the model does not take stays pickable, since it is a ceiling a
-- subagent on another model may reach, but it reads as the row it sends.
case("a_level_the_model_does_not_take_names_the_row_it_sends", function()
  local options = {
    { name = "off", sends = "off" },
    { name = "medium", tokens = 13107, sends = MEDIUM },
    { name = "high", tokens = 19660, sends = MEDIUM },
  }
  local state = picker_on(MEDIUM, options)
  local lines = Picker.render(state, WIDE)
  local high
  for i, row in ipairs(state.rows) do
    if row.option and row.option.name == HIGH then
      high = lines[i]
    end
  end
  eq(high[2][2], "dim")
  eq(line_text(high):match("sends medium") ~= nil, true, line_text(high))
  eq(line_text(high):find(TOKENS_LABEL, 1, true), nil, "an alias claims no budget of its own")
  eq(press(picker_on(MEDIUM, options), { "<Down>", "<CR>" }), HIGH)
end)

case("an_option_the_plugin_never_heard_of_renders_as_a_normal_row", function()
  local extended = {}
  for i, option in ipairs(LADDER) do
    extended[i] = option
  end
  extended[#extended + 1] = { name = "ultra", tokens = 65536 }
  local state = picker_on("max", extended)

  eq(press(state, { "<Down>", "<CR>" }), "ultra")
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
      return { thinking = args.thinking, thinking_shown = opts.shown or args.thinking }
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
  local result = run({ key("<Down>"), key("<Down>"), key("<CR>") })
  eq(result.sets, "xhigh")
  eq(result.flashes, "Thinking: xhigh")
  eq(result.closed, true)
end)

-- Nothing reaches the host until Enter, so there is never a value to undo.
case("a_dismissed_picker_sends_nothing", function()
  for name, events in pairs({
    esc = { key("<Down>"), key("<Esc>") },
    ["external close"] = { key("<Down>"), CLOSE },
    ["dead channel"] = { key("<Down>") },
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
  eq(run({ key("<CR>") }).sets, MEDIUM, "the window reopens after a crash")
end)

case("a_refused_value_flashes_what_the_host_answered", function()
  eq(run({ key("<CR>") }, { reject = true }).flashes, REJECTED)
  eq(run({}, { direct = "nonsense", reject = true }).flashes, REJECTED)
end)

case("the_direct_form_skips_the_picker", function()
  local result = run({}, { direct = "high" })
  eq(result.sets, "high")
  eq(result.flashes, "Thinking: high")
  eq(result.closed, false, "nothing was opened")
end)

case("the_flash_names_the_level_a_hidden_adaptive_is_sent_as", function()
  local result = run({}, { direct = ADAPTIVE, shown = HIGH })
  eq(result.sets, ADAPTIVE)
  eq(result.flashes, "Thinking: " .. HIGH)
end)

case("a_model_without_thinking_gets_the_flash_instead_of_a_picker", function()
  local result = run({}, { options = {} })
  eq(result.flashes, UNSUPPORTED)
  eq(result.sets, "")
end)

th.report()
