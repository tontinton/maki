local L = require("lines")
local Window = require("todo_window")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq
local has = th.has

local NO_TODOS = "No todos"
local BOOM = "the event loop blew up"
local REFRESH = "refresh"
local RAISE = "raise"
local CLOSE = { type = "close" }
local END_KEY = { type = "key", key = "<End>" }

local function item(content, status)
  return { content = content, status = status or "pending" }
end

local function text(lines, i)
  local parts = {}
  for _, span in ipairs(lines[i]) do
    parts[#parts + 1] = span[1]
  end
  return table.concat(parts)
end

local function texts(lines)
  local out = {}
  for i = 1, #lines do
    out[i] = text(lines, i)
  end
  return table.concat(out, ",")
end

case("markers_follow_the_status_and_an_unknown_one_reads_pending", function()
  local lines = L.build_lines({
    item("a", "completed"),
    item("b", "in_progress"),
    item("c"),
    item("d", "cancelled"),
    item("e", "weird"),
  })
  eq(texts(lines), "[✓] a,[•] b,[ ] c,[x] d,[ ] e")
  eq(lines[1][1][2], "todo_completed")
  eq(lines[2][1][2], "todo_in_progress")
  eq(lines[3][1][2], "todo_pending")
  eq(lines[4][1][2], "todo_cancelled")
  eq(lines[5][1][2], "todo_pending")
end)

case("count_done_counts_only_completed", function()
  local items = {
    item("a", "completed"),
    item("b", "cancelled"),
    item("c", "in_progress"),
    item("d", "completed"),
  }
  eq(L.count_done(items), 2)
  eq(L.count_done({}), 0)
end)

case("a_fitting_panel_shows_everything_without_a_hint", function()
  local lines = L.panel_lines({ item("a", "completed"), item("x", "cancelled"), item("b") }, 3)
  eq(texts(lines), "[✓] a,[x] x,[ ] b")
end)

case("an_overflowing_panel_anchors_on_the_last_completed_item", function()
  local items = {
    item("a", "completed"),
    item("b", "completed"),
    item("c", "in_progress"),
    item("d"),
    item("e"),
  }
  local lines = L.panel_lines(items, 3)
  eq(texts(lines), "[✓] b,[•] c,[ ] d,+2 more · /todo")
  eq(lines[4][1][2], "tool_dim")
end)

case("items_before_the_anchor_are_hidden_but_counted", function()
  local items = {
    item("a"),
    item("b", "completed"),
    item("c"),
    item("d"),
    item("e"),
  }
  local lines = L.panel_lines(items, 3)
  eq(texts(lines), "[✓] b,[ ] c,[ ] d,+2 more · /todo")
end)

case("with_nothing_completed_the_first_live_item_anchors", function()
  local items = {
    item("x", "cancelled"),
    item("a"),
    item("b", "in_progress"),
    item("c"),
    item("d"),
  }
  local lines = L.panel_lines(items, 3)
  eq(texts(lines), "[ ] a,[•] b,[ ] c,+2 more · /todo")
end)

case("a_single_row_prefers_the_first_in_progress_item", function()
  local items = { item("a"), item("b", "in_progress"), item("c", "in_progress"), item("d") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[•] b,+3 more · /todo")
end)

case("a_single_row_without_in_progress_prefers_the_first_pending", function()
  local items = { item("a", "completed"), item("b"), item("c", "completed"), item("d", "cancelled") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[ ] b,+3 more · /todo")
end)

case("a_single_row_on_a_finished_list_shows_the_last_completed_item", function()
  local items = { item("a", "completed"), item("b", "completed"), item("c", "completed") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[✓] c,+2 more · /todo")
end)

case("a_single_row_on_an_all_cancelled_list_shows_the_first_item", function()
  local items = { item("a", "cancelled"), item("b", "cancelled") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[x] a,+1 more · /todo")
end)

case("the_popup_title_counts_done_of_total_and_the_height_clamps", function()
  eq(L.window_title(2, 5), " Todos · 2/5 ")
  eq(L.window_height(3), 5)
  eq(L.window_height(16), 18)
  eq(L.window_height(20), 18)
end)

-- The stubbed half: the popup against a scripted event list, exactly like the
-- thinking spec. A REFRESH event stands in for a `todo_write` call landing
-- while the window is open: it advances the fetch to the next list, then runs
-- Window.refresh() at the spot the host would run it.
local function run(lists, events)
  local saved_ui = maki.ui
  local current = lists[1] or {}
  local list_idx = 1
  local index = 0
  local flashes, line_sets, config_sets, cursor_sets = {}, {}, {}, {}
  local opened, closed = false, false

  local buf = {
    set_lines = function(_, lines)
      line_sets[#line_sets + 1] = lines
    end,
  }

  local win
  win = {
    height = 8,
    recv = function()
      index = index + 1
      local ev = events[index]
      if ev == RAISE then
        error(BOOM)
      elseif ev == REFRESH then
        index = index - 1
        if list_idx < #lists then
          list_idx = list_idx + 1
        end
        current = lists[list_idx] or {}
        Window.refresh()
        return win:recv()
      end
      return ev
    end,
    set_config = function(_, cfg)
      config_sets[#config_sets + 1] = cfg
    end,
    set_cursor = function(_, row)
      cursor_sets[#cursor_sets + 1] = row
    end,
    is_open = function()
      return not closed
    end,
    close = function()
      closed = true
    end,
  }

  maki.ui = {
    buf = function()
      return buf
    end,
    open_win = function(_, opts)
      opened = true
      win.open_opts = opts
      return win
    end,
    flash = function(msg)
      flashes[#flashes + 1] = msg
    end,
  }

  local ok, err = pcall(Window.open, function()
    local items = current
    return items, L.count_done(items), #items
  end)

  maki.ui = saved_ui
  return {
    ok = ok,
    err = err,
    opened = opened,
    closed = closed,
    flashes = table.concat(flashes, ","),
    line_sets = line_sets,
    cursor_sets = cursor_sets,
    config_sets = config_sets,
    open_opts = win.open_opts,
  }
end

case("a_cold_start_flashes_and_opens_nothing", function()
  local r = run({}, {})
  eq(r.opened, false)
  eq(r.closed, false)
  eq(r.flashes, NO_TODOS)
end)

case("an_emptied_list_closes_the_open_popup", function()
  local first = { item("a", "completed"), item("b", "in_progress"), item("c") }
  local r = run({ first, {} }, { REFRESH, CLOSE })
  eq(r.opened, true)
  eq(r.open_opts.title, " Todos · 1/3 ")
  eq(r.open_opts.cursor_line, true)
  eq(r.open_opts.height, 5)
  eq(r.closed, true)
  eq(#r.line_sets, 1, "no repaint is owed by an emptied list")
end)

case("a_changed_list_repaints_and_an_unchanged_one_is_a_noop", function()
  local first = { item("a", "completed"), item("b", "in_progress"), item("c") }
  local second = {
    item("a", "completed"),
    item("b", "completed"),
    item("c", "in_progress"),
    item("d"),
  }
  local r = run({ first, second }, { REFRESH, REFRESH, CLOSE })
  eq(r.opened, true)
  eq(r.open_opts.title, " Todos · 1/3 ")
  eq(#r.line_sets, 2, "one repaint for the change, none for the repeat")
  eq(texts(r.line_sets[2]), "[✓] a,[✓] b,[•] c,[ ] d")
  eq(#r.config_sets, 1)
  eq(r.config_sets[1].title, " Todos · 2/4 ")
  eq(r.config_sets[1].height, 6)
end)

case("a_shrunk_list_clamps_the_cursor_to_the_end", function()
  local first = { item("a"), item("b"), item("c"), item("d"), item("e") }
  local second = { item("a"), item("b"), item("c") }
  local r = run({ first, second }, { END_KEY, REFRESH, CLOSE })
  eq(r.opened, true)
  eq(r.cursor_sets[1], 1, "primed at open")
  eq(r.cursor_sets[2], 5, "<End> reaches the bottom")
  eq(r.cursor_sets[3], 3, "the shrink pulls it back")
  eq(#r.cursor_sets, 3)
end)

case("a_blown_up_loop_closes_the_window_and_allows_reopen", function()
  local one = { item("a"), item("b") }
  local r = run({ one }, { RAISE })
  eq(r.opened, true)
  eq(r.ok, false)
  has(tostring(r.err), BOOM)
  eq(r.closed, true)

  local again = run({ one }, { CLOSE })
  eq(again.ok, true)
  eq(again.opened, true)
  eq(again.closed, true)
end)

th.report()
