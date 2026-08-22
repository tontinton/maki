local helpers = require("read_helpers")

local truncate_bytes = helpers.truncate_bytes
local split_lines = helpers.split_lines

local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

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
