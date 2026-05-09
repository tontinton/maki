local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")

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

case("tool_view_no_truncate_single_line", function()
  for _, mode in ipairs({ "tail", "head" }) do
    local buf = mock_buf()
    local view = ToolView.new(buf, { max_lines = 3, keep = mode })
    for i = 1, 4 do
      view:append("line" .. i)
    end
    if mode == "head" then
      view:finish()
    end
    eq(#buf.lines, 4, mode .. ": should inline the single skipped line")
    eq(buf.lines[1], "line1", mode)
    eq(buf.lines[4], "line4", mode)
  end
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

local ListPicker = require("maki.list_picker")
local highlight_to_view = require("maki.highlight")

case("highlight_to_view_number_width_scales", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 200 })
  local lines = {}
  for i = 1, 100 do
    lines[i] = "x"
  end
  local content = table.concat(lines, "\n")
  local ok = highlight_to_view(view, content, "txt")
  eq(ok, true)
  eq(view.ring_count, 100)
  local first_nr = buf.lines[1][1][1]
  local last_nr = buf.lines[100][1][1]
  eq(first_nr, "  1 ", "3-digit width for 100 lines, right-aligned")
  eq(last_nr, "100 ", "line 100 should fill the width")
  eq(buf.lines[1][1][2], "line_nr")
end)

local render_lines = ListPicker._render_lines

case("render_lines_string_items_basic", function()
  local lines = render_lines({ "alpha", "beta" }, 1, 40)
  eq(#lines, 2)
  eq(lines[1][1][1], "  alpha")
  eq(lines[1][1][2], "cmd_selected")
  eq(lines[2][1][2], "cmd_name")
end)

case("render_lines_table_items_with_detail", function()
  local items = {
    { label = "foo", detail = "(3 bytes)" },
    { label = "bar", detail = "(10 bytes)" },
  }
  local lines = render_lines(items, 2, 60)
  eq(lines[1][1][2], "cmd_name", "unselected label style")
  eq(lines[1][3][2], "cmd_desc", "unselected detail style")
  eq(lines[2][1][2], "cmd_selected", "selected label style")
  eq(lines[2][3][2], "cmd_selected", "selected detail uses cmd_selected")
end)

case("render_lines_detail_padding_never_zero", function()
  local label = string.rep("x", 50)
  local detail = string.rep("y", 50)
  local items = { { label = label, detail = detail } }
  local lines = render_lines(items, 1, 20)
  local pad_span = lines[1][2][1]
  assert(#pad_span >= 1, "padding must be at least 1 space even when overflowing")
end)

case("render_lines_no_detail_fills_trailing", function()
  local lines = render_lines({ "ab" }, 1, 10)
  eq(#lines[1], 2, "label + trailing pad")
  local trail = lines[1][2][1]
  eq(#trail, 10 - 2 - 2, "trail = width - indent(2) - label_len(2)")
end)

case("render_lines_selected_index_out_of_range", function()
  local lines = render_lines({ "a", "b" }, 99, 40)
  eq(lines[1][1][2], "cmd_name")
  eq(lines[2][1][2], "cmd_name")
end)

case("render_lines_empty_items", function()
  local lines = render_lines({}, 1, 40)
  eq(#lines, 0)
end)

case("render_lines_default_width_used", function()
  local items = { "test" }
  local lines_default = render_lines(items, 1)
  local lines_explicit = render_lines(items, 1, 80)
  eq(#lines_default[1], #lines_explicit[1], "default width should be 80")
  eq(lines_default[1][2][1], lines_explicit[1][2][1])
end)

case("render_lines_mixed_string_and_table", function()
  local items = { "plain", { label = "rich", detail = "info" } }
  local lines = render_lines(items, 1, 40)
  eq(lines[1][1][1], "  plain")
  eq(#lines[1], 2, "string item: label + trailing")
  eq(lines[2][1][1], "  rich")
  eq(#lines[2], 4, "table item with detail: label + pad + detail + right_pad")
end)

case("render_lines_trailing_omitted_when_label_fills_width", function()
  local label = string.rep("z", 10)
  local lines = render_lines({ label }, 1, 12)
  eq(#lines[1], 1, "no trailing span when width - indent - label <= 0")
end)

case("render_lines_detail_right_pad_always_present", function()
  local items = { { label = "x", detail = "d" } }
  local lines = render_lines(items, 1, 50)
  local right_pad = lines[1][4][1]
  eq(#right_pad, 2, "DETAIL_RIGHT_PAD = 2")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
