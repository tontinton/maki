local truncate = require("truncate")
local ToolView = require("tool_view")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

-- Mock buf that records set_lines calls
local function mock_buf()
  local b = { lines = nil, call_count = 0 }
  function b:set_lines(lines)
    self.lines = lines
    self.call_count = self.call_count + 1
  end
  return b
end

case("truncate_within_limits_unchanged", function()
  eq(truncate("hello", 100, 1000), "hello")
  eq(truncate("a\nb\nc", 3, 1000), "a\nb\nc")
  eq(truncate("", 100, 1000), "")
end)

case("truncate_exceeds_line_limit", function()
  local result = truncate("aaa\nbbb\nccc\nddd", 2, 1000)
  assert(result:find("aaa", 1, true), "should keep first line")
  assert(result:find("bbb", 1, true), "should keep second line")
  assert(not result:find("ccc", 1, true), "should drop third line")
  assert(result:find("%[truncated %d+ bytes%]"), "should have truncation marker")
end)

case("truncate_exceeds_byte_limit", function()
  local text = string.rep("x", 200)
  local result = truncate(text, 1000, 50)
  assert(#result < #text, "should be shorter")
  assert(result:find("%[truncated"), "should have truncation marker")
end)

case("truncate_byte_limit_mid_line", function()
  local text = "short\n" .. string.rep("x", 100)
  local result = truncate(text, 1000, 20)
  assert(result:find("short"), "should keep first line")
  assert(not result:find(string.rep("x", 100)), "should drop long line")
  assert(result:find("%[truncated"), "should have truncation marker")
end)

case("truncate_trailing_newlines_counted", function()
  local result = truncate("a\n\n\n\n\n", 2, 1000)
  assert(result:find("%[truncated"), "trailing newlines should count as lines")
end)

-- ToolView tests

case("tool_view_tail_keeps_last_n", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  eq(#buf.lines, 4) -- 3 ring lines + 1 notice
  eq(buf.lines[1][1][1], "... (2 lines) (click to expand)")
  eq(buf.lines[2], "line3")
  eq(buf.lines[3], "line4")
  eq(buf.lines[4], "line5")
end)

case("tool_view_head_keeps_first_n", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "head" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  view:finish()
  eq(#buf.lines, 4) -- 3 ring lines + 1 notice
  eq(buf.lines[1], "line1")
  eq(buf.lines[2], "line2")
  eq(buf.lines[3], "line3")
  eq(buf.lines[4][1][1], "... (2 lines) (click to expand)")
end)

case("tool_view_header_appears_first", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 5 })
  view:set_header({ "cmd", { { "---", "dim" } } })
  view:append("output1")
  eq(buf.lines[1], "cmd")
  eq(buf.lines[2][1][1], "---")
  eq(buf.lines[3], "output1")
end)

case("tool_view_ring_wraparound", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  for i = 1, 10 do
    view:append("line" .. i)
  end
  eq(view.skipped, 7)
  eq(buf.lines[1][1][1], "... (7 lines) (click to expand)")
  eq(buf.lines[2], "line8")
  eq(buf.lines[3], "line9")
  eq(buf.lines[4], "line10")
end)

case("tool_view_finish_flushes_head_skipped", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 2, keep = "head" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  local count_before = buf.call_count
  view:finish()
  assert(buf.call_count > count_before, "finish should flush when head has skipped lines")
  eq(buf.lines[3][1][1], "... (3 lines) (click to expand)")
end)

case("tool_view_no_truncation_within_limit", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 10, keep = "tail" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  eq(#buf.lines, 5)
  eq(view.skipped, 0)
end)

case("tool_view_toggle_expands_all_lines", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  for i = 1, 10 do
    view:append("line" .. i)
  end
  eq(#buf.lines, 4) -- 3 visible + hidden notice
  view:toggle()
  eq(#buf.lines, 10) -- 10 data lines
  eq(buf.lines[1], "line1")
  eq(buf.lines[10], "line10")
end)

case("tool_view_toggle_twice_collapses_back", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  for i = 1, 10 do
    view:append("line" .. i)
  end
  view:toggle()
  view:toggle()
  eq(#buf.lines, 4)
  eq(buf.lines[1][1][1], "... (7 lines) (click to expand)")
  eq(buf.lines[2], "line8")
end)

case("tool_view_toggle_head_mode_expands", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 2, keep = "head" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  view:finish()
  eq(buf.lines[3][1][1], "... (3 lines) (click to expand)")
  view:toggle()
  eq(buf.lines[1], "line1")
  eq(buf.lines[5], "line5")
end)

case("tool_view_expand_cap_overflow_shows_omitted", function()
  local buf = mock_buf()
  local cap = 20
  local view = ToolView.new(buf, { max_lines = 2, keep = "tail", max_expand_lines = cap })
  for i = 1, cap + 5 do
    view:append("line" .. i)
  end
  eq(view.all_skipped, 5)
  view:toggle()
  eq(buf.lines[1], "line1")
  eq(buf.lines[cap], "line" .. cap)
  eq(buf.lines[cap + 1][1][1], "5 lines omitted")
end)

case("tool_view_no_collapse_link_when_within_max", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 10, keep = "tail" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  view:toggle()
  for _, line in ipairs(buf.lines) do
    if type(line) == "table" and line[1] and line[1][1] == "click to collapse" then
      error("should not show collapse link when lines <= max")
    end
  end
end)

case("tool_view_clear_resets_data_but_keeps_expanded", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  for i = 1, 10 do
    view:append("line" .. i)
  end
  view:toggle()
  eq(view.expanded, true)
  view:clear()
  eq(#view.all_lines, 0)
  eq(view.all_skipped, 0)
  eq(view.ring_count, 0)
  eq(view.skipped, 0)
end)

case("tool_view_header_preserved_after_toggle", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  view:set_header({ "$ echo hello", { { "---", "dim" } } })
  for i = 1, 10 do
    view:append("line" .. i)
  end
  view:toggle()
  eq(buf.lines[1], "$ echo hello")
  eq(buf.lines[2][1][1], "---")
  eq(buf.lines[3], "line1")
  eq(buf.lines[12], "line10")
end)

case("tool_view_append_after_toggle_still_works", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  for i = 1, 5 do
    view:append("line" .. i)
  end
  view:toggle()
  view:append("line6")
  eq(view.all_lines[6], "line6")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
