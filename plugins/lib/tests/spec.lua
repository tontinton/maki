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
  eq(buf.lines[1][1][1], "2 lines hidden")
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
  eq(buf.lines[4][1][1], "2 lines hidden")
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
  eq(buf.lines[1][1][1], "7 lines hidden")
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
  eq(buf.lines[3][1][1], "3 lines hidden")
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

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
