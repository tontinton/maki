local Navigation = require("navigation")
local th = require("maki.test_helpers")

local USER = "user"
local ASSISTANT = "assistant"
local FIRST = { role = USER, line = 1 }
local SECOND = { role = ASSISTANT, line = 20 }
local LAST = { role = USER, line = 40 }

for _, test in ipairs({
  { "next_message", 1, true, false, false, SECOND, false },
  { "next_prompt", 1, true, true, false, LAST, false },
  { "previous_message", 40, false, false, false, SECOND, false },
  { "previous_prompt", 40, false, true, false, FIRST, false },
  { "inside_message", 21, false, false, false, SECOND, false },
  { "end_boundary", 40, true, false, false, LAST, true },
  { "start_boundary", 1, false, false, false, FIRST, true },
  { "wrap_forward", 40, true, false, true, FIRST, false },
  { "wrap_backward", 1, false, true, true, LAST, false },
}) do
  th.case(test[1], function()
    local target, boundary = Navigation.target({
      topline = test[2],
      line_count = 60,
      height = 5,
      positions = { FIRST, SECOND, LAST },
    }, test[3], test[4], test[5])
    th.eq(target, test[6])
    th.eq(boundary, test[7])
  end)
end

th.case("empty_transcript", function()
  th.eq(Navigation.target({ topline = 1, line_count = 0, height = 5, positions = {} }, true, false, false), nil)
end)

th.case("clamped_destinations", function()
  local last = { role = USER, line = 23 }
  local target, boundary = Navigation.target({
    topline = SECOND.line,
    line_count = 24,
    height = 5,
    positions = { FIRST, SECOND, last },
  }, true, false, false)
  th.eq(target, last)
  th.eq(boundary, true)
end)

th.report()
