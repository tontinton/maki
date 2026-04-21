local truncate = require("truncate")

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

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
