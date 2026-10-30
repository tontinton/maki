local L = require("lines")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local MORE_STYLE = "tool_dim"

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

case("a_list_within_the_cap_has_no_hint", function()
  local lines = L.panel_lines({ item("a", "completed"), item("x", "cancelled"), item("b") }, 5)
  eq(texts(lines), "[✓] a,[x] x,[ ] b")
end)

case("the_panel_starts_one_row_before_the_live_item", function()
  local items = {
    item("a", "completed"),
    item("b", "completed"),
    item("c", "in_progress"),
    item("d"),
    item("e"),
    item("f"),
    item("g"),
  }
  local lines = L.panel_lines(items, 5)
  eq(texts(lines), "[✓] b,[•] c,[ ] d,[ ] e,[ ] f,+2 more")
  eq(lines[6][1][2], MORE_STYLE)
end)

case("a_clamp_keeps_the_window_on_the_list_near_its_start", function()
  local items = {
    item("a"),
    item("b", "completed"),
    item("c"),
    item("d"),
    item("e"),
    item("f"),
    item("g"),
  }
  local lines = L.panel_lines(items, 5)
  eq(texts(lines), "[ ] a,[✓] b,[ ] c,[ ] d,[ ] e,+2 more")
end)

case("with_nothing_completed_the_first_live_item_anchors", function()
  local items = {
    item("x", "cancelled"),
    item("a"),
    item("b", "in_progress"),
    item("c"),
    item("d"),
    item("e"),
    item("f"),
  }
  local lines = L.panel_lines(items, 5)
  eq(texts(lines), "[ ] a,[•] b,[ ] c,[ ] d,[ ] e,+2 more")
end)

case("a_hole_never_hides_the_live_item", function()
  local lines = L.panel_lines({
    item("a", "completed"),
    item("b", "in_progress"),
    item("c", "completed"),
    item("d"),
    item("e"),
    item("f"),
    item("g"),
  }, 5)
  eq(texts(lines), "[✓] a,[•] b,[✓] c,[ ] d,[ ] e,+2 more")
end)

case("a_live_item_last_in_a_mostly_done_list_shows", function()
  local lines = L.panel_lines({
    item("a"),
    item("b"),
    item("c"),
    item("d", "in_progress"),
    item("e"),
    item("f"),
    item("g"),
  }, 5)
  eq(texts(lines), "[ ] c,[•] d,[ ] e,[ ] f,[ ] g,+2 more")
end)

case("a_single_row_prefers_the_first_in_progress_item", function()
  local items = { item("a"), item("b", "in_progress"), item("c", "in_progress"), item("d") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[•] b,+3 more")
end)

case("a_single_row_without_in_progress_prefers_the_first_pending", function()
  local items = { item("a", "completed"), item("b"), item("c", "completed"), item("d", "cancelled") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[ ] b,+3 more")
end)

case("a_finished_list_shows_its_tail", function()
  local items = { item("a", "completed"), item("b", "completed"), item("c", "completed") }
  local lines = L.panel_lines(items, 2)
  eq(texts(lines), "[✓] b,[✓] c,+1 more")
end)

case("an_all_cancelled_list_shows_its_tail", function()
  local items = { item("a", "cancelled"), item("b", "cancelled"), item("c", "cancelled") }
  local lines = L.panel_lines(items, 1)
  eq(texts(lines), "[x] c,+2 more")
end)

case("the_cap_counts_cancelled_rows_it_shows", function()
  local items = {
    item("a", "cancelled"),
    item("b", "in_progress"),
    item("c", "cancelled"),
    item("d", "pending"),
  }
  local lines = L.panel_lines(items, 2)
  eq(texts(lines), "[x] a,[•] b,+2 more")
end)

th.report()
