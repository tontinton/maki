local function line_nr_fmt(count)
  local w = math.max(1, math.floor(math.log(count + 1, 10)) + 1)
  return "%" .. w .. "d "
end

local function truncate_bytes(line, max_bytes)
  if #line <= max_bytes then
    return line
  end
  local i = max_bytes
  while i > 0 and line:byte(i) >= 0x80 and line:byte(i) < 0xC0 do
    i = i - 1
  end
  if i > 0 and line:byte(i) >= 0xC0 then
    i = i - 1
  end
  return line:sub(1, i) .. "..."
end

local function split_lines(content)
  local lines = {}
  local pos = 1
  while pos <= #content do
    local nl = content:find("\n", pos, true)
    if nl then
      local line = content:sub(pos, nl - 1)
      lines[#lines + 1] = line:find("\r$") and line:sub(1, -2) or line
      pos = nl + 1
    else
      local line = content:sub(pos)
      lines[#lines + 1] = line:find("\r$") and line:sub(1, -2) or line
      pos = #content + 1
    end
  end
  return lines
end

local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq
local mktmpdir = function()
  return th.mktmpdir("read_spec")
end
local rmtree = th.rmtree

-- line_nr_fmt: table-driven across all boundaries + alignment

case("line_nr_fmt_boundaries_and_alignment", function()
  local vectors = {
    { 0, "%1d " },
    { 1, "%1d " },
    { 8, "%1d " },
    { 9, "%2d " },
    { 10, "%2d " },
    { 98, "%2d " },
    { 99, "%3d " },
    { 100, "%3d " },
    { 999, "%4d " },
    { 1000, "%4d " },
  }
  for _, v in ipairs(vectors) do
    eq(line_nr_fmt(v[1]), v[2], "count=" .. v[1])
  end
  local fmt = line_nr_fmt(100)
  eq(string.format(fmt, 1), "  1 ")
  eq(string.format(fmt, 100), "100 ")
end)

-- truncate_bytes: ASCII + all UTF-8 widths

case("truncate_ascii", function()
  eq(truncate_bytes("", 10), "")
  eq(truncate_bytes("hello", 10), "hello")
  eq(truncate_bytes("hello", 5), "hello")
  eq(truncate_bytes("hello world", 5), "hello...")
  eq(truncate_bytes("ab", 1), "a...")
end)

case("truncate_utf8_boundary_safety", function()
  -- 2-byte: é = \xC3\xA9
  eq(truncate_bytes("caf\xC3\xA9", 10), "caf\xC3\xA9")
  eq(truncate_bytes("caf\xC3\xA9!", 5), "caf...")
  eq(truncate_bytes("caf\xC3\xA9", 4), "caf...")

  -- 3-byte: € = \xE2\x82\xAC — cut at each byte within the sequence
  eq(truncate_bytes("ab\xE2\x82\xACd", 5), "ab...")
  eq(truncate_bytes("ab\xE2\x82\xAC", 4), "ab...")
  eq(truncate_bytes("ab\xE2\x82\xAC", 3), "ab...")

  -- 4-byte: 🎉 = \xF0\x9F\x8E\x89 — cutting anywhere inside removes entire char
  local emoji = "\xF0\x9F\x8E\x89"
  eq(truncate_bytes(emoji, 4), emoji)
  eq(truncate_bytes(emoji, 3), "...")
  eq(truncate_bytes(emoji, 1), "...")

  -- all multibyte: cutting within sequences
  local s = "\xC3\xA9\xC3\xA9\xC3\xA9"
  eq(truncate_bytes(s, 4), "\xC3\xA9...")
  eq(truncate_bytes(s, 2), "...")
end)

-- split_lines: table-driven

case("split_lines", function()
  local vectors = {
    { "", 0, {} },
    { "hello", 1, { "hello" } },
    { "a\nb", 2, { "a", "b" } },
    { "a\nb\n", 2, { "a", "b" } },
    { "\n\n\n", 3, { "", "", "" } },
    { "a\r\nb\r\n", 2, { "a", "b" } },
  }
  for _, v in ipairs(vectors) do
    local lines = split_lines(v[1])
    eq(#lines, v[2], "count for " .. ("%q"):format(v[1]))
    for i, expected in ipairs(v[3]) do
      eq(lines[i], expected, "line " .. i .. " for " .. ("%q"):format(v[1]))
    end
  end
end)

th.report()
