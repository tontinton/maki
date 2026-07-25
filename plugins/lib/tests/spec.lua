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

case("tool_view_max_line_bytes_truncates_string_line", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail", max_line_bytes = 10 })
  view:append(string.rep("a", 20))
  eq(#buf.lines[1], 13)
  assert(buf.lines[1]:find("…"), "truncated line should end with ellipsis")
  eq(buf.lines[1]:sub(1, 10), string.rep("a", 10))
end)

case("tool_view_max_line_bytes_truncates_span_line", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail", max_line_bytes = 12 })
  view:append({ { "hello", "dim" }, " ", { "worldoverflow", "error" } })
  eq(buf.lines[1][1][1], "hello")
  eq(buf.lines[1][1][2], "dim")
  eq(buf.lines[1][3][2], "error")
  assert(buf.lines[1][3][1]:find("…"), "span should be truncated with ellipsis")
end)

case("tool_view_max_line_bytes_utf8_safe", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail", max_line_bytes = 10 })
  view:append("éééééééééé")
  eq(buf.lines[1], string.rep("é", 5) .. "…")
end)

case("tool_view_max_line_bytes_default_off", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "tail" })
  local long = string.rep("a", 100)
  view:append(long)
  eq(buf.lines[1], long)
end)

local TextInput = require("maki.text_input")

case("text_input_insert_and_value", function()
  local input = TextInput.new()
  input:handle_key("h")
  input:handle_key("i")
  eq(input:value(), "hi")
  eq(input.col, 2)
end)

case("text_input_backspace_at_start_noop", function()
  local input = TextInput.new()
  input:handle_key("backspace")
  eq(input:value(), "")
  eq(input.col, 0)
end)

case("text_input_backspace_deletes", function()
  local input = TextInput.new()
  input:handle_key("a")
  input:handle_key("b")
  input:handle_key("c")
  input:handle_key("backspace")
  eq(input:value(), "ab")
  eq(input.col, 2)
end)

case("text_input_cursor_movement", function()
  local input = TextInput.new()
  input:handle_key("a")
  input:handle_key("b")
  input:handle_key("c")
  input:handle_key("left")
  eq(input.col, 2)
  input:handle_key("left")
  eq(input.col, 1)
  input:handle_key("left")
  eq(input.col, 0)
  input:handle_key("left")
  eq(input.col, 0)
  input:handle_key("right")
  eq(input.col, 1)
  input:handle_key("end")
  eq(input.col, 3)
  input:handle_key("home")
  eq(input.col, 0)
end)

case("text_input_delete_word", function()
  local input = TextInput.new()
  for c in ("hello world"):gmatch(".") do
    input:handle_key(c)
  end
  eq(input:value(), "hello world")
  input:handle_key("ctrl+w")
  eq(input:value(), "hello ", "ctrl+w eats the last word in one press")
  input:handle_key("ctrl+w")
  eq(input:value(), "", "second ctrl+w eats remaining trailing space and word")
end)

local R = TextInput.Result

case("text_input_unknown_key_returns_ignored", function()
  local input = TextInput.new()
  eq(input:handle_key("ctrl+z"), R.IGNORED)
  eq(input:handle_key("f1"), R.IGNORED)
end)

case("text_input_multibyte_key_inserts_single_codepoint", function()
  local input = TextInput.new()
  eq(input:handle_key("你"), R.CHANGED, "single CJK codepoint key is inserted, not ignored by byte length")
  eq(input:value(), "你")
  input:handle_key("好")
  eq(input:value(), "你好")
  input:handle_key("left")
  eq(input:char_before_cursor(), "你")
end)

case("text_input_render_format", function()
  local input = TextInput.new()
  input:handle_key("a")
  input:handle_key("b")
  input:handle_key("left")
  local r = input:render("> ")
  eq(#r.lines, 1)
  eq(r.cursor_row, 1)
  local spans = r.lines[1]
  eq(#spans, 4)
  eq(spans[1][1], "> ")
  eq(spans[1][2], "dim")
  eq(spans[2][1], "a")
  eq(spans[2][2], "")
  eq(spans[3][1], "b")
  eq(spans[3][2], "cursor")
  eq(spans[4][1], "")
  eq(spans[4][2], "")
end)

case("text_input_is_empty", function()
  local input = TextInput.new()
  eq(input:is_empty(), true)
  input:handle_key("x")
  eq(input:is_empty(), false)
end)

case("text_input_utf8_insert_navigate_delete_render", function()
  local input = TextInput.new()
  input:insert_text("héllo — wörld")
  eq(input:value(), "héllo — wörld", "paste preserves multibyte text")
  eq(input:char_before_cursor(), "d", "char_before_cursor over single-byte")

  input = TextInput.new()
  input:insert_text("aé")
  eq(input.col, 3, "cursor at end of 'aé' (1 + 2 bytes)")
  input:handle_key("left")
  eq(input.col, 1, "left jumps over whole codepoint")
  eq(input:char_before_cursor(), "a")
  input:handle_key("right")
  eq(input.col, 3, "right jumps over whole codepoint")
  input:handle_key("backspace")
  eq(input:value(), "a", "backspace removes whole codepoint")

  input = TextInput.new()
  input:insert_text("aé")
  input:handle_key("left")
  local spans = input:render("> ").lines[1]
  eq(spans[2][1], "a", "text before cursor")
  eq(spans[3][1], "é", "cursor span is whole codepoint")
  eq(spans[3][2], "cursor")
end)

case("text_input_insert_text_table_driven", function()
  local cases = {
    { "foo\nbar\nbaz", 3, "foo\nbar\nbaz", 3, 3 },
    { "a\n", 2, "a\n", 2, 0 },
    { "\nx", 2, "\nx", 2, 1 },
    { "\n\n\n", 4, "\n\n\n", 4, 0 },
    { "é\nö\n世界", 3, "é\nö\n世界", 3, #"世界" },
  }
  for i, c in ipairs(cases) do
    local input = TextInput.new()
    input:insert_text(c[1])
    eq(input:line_count(), c[2], "case " .. i .. ": line_count")
    eq(input:value(), c[3], "case " .. i .. ": value")
    eq(input.line, c[4], "case " .. i .. ": line")
    eq(input.col, c[5], "case " .. i .. ": col")
  end
end)

case("text_input_newline_table_driven", function()
  local cases = {
    { { "left", "left" }, "hel\nlo" },
    { { "home" }, "\nhello" },
    { { "end" }, "hello\n" },
  }
  for i, c in ipairs(cases) do
    local input = TextInput.new()
    input:insert_text("hello")
    for _, k in ipairs(c[1]) do
      input:handle_key(k)
    end
    input:handle_key("newline")
    eq(input:value(), c[2], "case " .. i)
    eq(input:line_count(), 2, "case " .. i .. ": two lines")
    eq(input.line, 2, "case " .. i .. ": cursor on new line")
    eq(input.col, 0, "case " .. i .. ": cursor at col 0")
  end
end)

case("text_input_up_down_navigation_clamps_and_no_ops", function()
  local input = TextInput.new()
  input:insert_text("abc\nlonger_line")
  eq(input.line, 2)
  eq(input.col, 11, "cursor at end of longer line")
  input:handle_key("up")
  eq(input.line, 1)
  eq(input.col, 3, "col clamps to short line length")
  input:handle_key("up")
  eq(input.line, 1, "up at line 1 is a no-op")
  input:handle_key("down")
  input:handle_key("down")
  eq(input.line, 2, "down at last line is a no-op")
end)

case("text_input_cursor_wraps_across_line_boundaries", function()
  local input = TextInput.new()
  input:insert_text("abc\nxy")
  input:handle_key("home")
  input:handle_key("left")
  eq(input.line, 1)
  eq(input.col, 3, "left at col 0 lands at end of previous line")
  input:handle_key("right")
  eq(input.line, 2)
  eq(input.col, 0, "right at end of non-last line goes to start of next")
end)

case("text_input_backspace_joins_lines_table_driven", function()
  local cases = {
    { "foo\nbar", { "home" }, "foobar", 1, 3 },
    { "\nabc", { "home" }, "abc", 1, 0 },
    { "a\nb", { "end", "backspace" }, "a", 1, 1 },
  }
  for i, c in ipairs(cases) do
    local input = TextInput.new()
    input:insert_text(c[1])
    for _, k in ipairs(c[2]) do
      input:handle_key(k)
    end
    input:handle_key("backspace")
    eq(input:value(), c[3], "case " .. i .. ": value")
    eq(input.line, c[4], "case " .. i .. ": line")
    eq(input.col, c[5], "case " .. i .. ": col")
    eq(input:line_count(), 1, "case " .. i .. ": joined to one line")
  end
end)

case("text_input_empty_input_movement_is_noop_and_char_before_cursor_is_nil", function()
  local input = TextInput.new()
  for _, k in ipairs({ "left", "right", "up", "down", "backspace" }) do
    input:handle_key(k)
  end
  eq(input:value(), "")
  eq(input:line_count(), 1)
  eq(input.line, 1)
  eq(input.col, 0)
  eq(input:char_before_cursor(), nil, "no char before cursor at start of empty input")

  input = TextInput.new()
  input:insert_text("abc\ndef")
  input:handle_key("home")
  eq(input:char_before_cursor(), nil, "no char before cursor at col 0 on non-first line")
end)

case("text_input_ctrl_w_consumes_trailing_spaces_then_word", function()
  local input = TextInput.new()
  input:insert_text("hello world  ")
  input:handle_key("ctrl+w")
  eq(input:value(), "hello ", "single ctrl+w eats trailing spaces AND the word")
  input:handle_key("ctrl+w")
  eq(input:value(), "", "second ctrl+w eats what is left")
end)

case("text_input_render_multiline_padding_and_cursor", function()
  local input = TextInput.new()
  input:insert_text("line1\nline2")
  local prefix = "> "
  local r = input:render(prefix, #prefix)
  eq(#r.lines, 2, "one render row per logical line")
  eq(r.cursor_row, 2, "cursor on second logical line")
  eq(r.lines[1][1][1], prefix, "first row uses the prefix")
  eq(r.lines[2][1][1], string.rep(" ", #prefix), "continuation rows use blank padding")
  eq(r.lines[1][2][1], "line1", "non-cursor row renders text in one span")
  local saw_cursor
  for _, span in ipairs(r.lines[2]) do
    if span[2] == "cursor" then
      saw_cursor = true
    end
  end
  assert(saw_cursor, "cursor span must appear on the row holding the cursor")
end)

local function span_text(row)
  local parts = {}
  for _, span in ipairs(row) do
    parts[#parts + 1] = span[1]
  end
  return table.concat(parts)
end

local function find_cursor_char(row)
  for _, span in ipairs(row) do
    if span[2] == "cursor" then
      return span[1]
    end
  end
end

case("text_input_render_wraps_long_line", function()
  local input = TextInput.new()
  input:insert_text("abcdefghij")
  local r = input:render("> ", 2, 8)
  eq(#r.lines, 2, "10 chars at usable=6 produces 2 visual rows")
  eq(r.cursor_row, 2, "cursor on last visual row")
  eq(find_cursor_char(r.lines[2]), " ", "cursor at end is a space")
end)

case("text_input_render_wrap_cursor_mid_line", function()
  local input = TextInput.new()
  input:insert_text("abcdefghij")
  for _ = 1, 5 do
    input:handle_key("left")
  end
  local r = input:render("> ", 2, 8)
  eq(#r.lines, 2, "still 2 visual rows")
  eq(r.cursor_row, 1, "cursor in first chunk")
  eq(find_cursor_char(r.lines[1]), "f", "cursor on 'f'")
end)

case("text_input_render_wrap_multiline", function()
  local input = TextInput.new()
  input:insert_text("abcdefghij\n1234567890")
  local r = input:render("> ", 2, 8)
  eq(#r.lines, 4, "each logical line wraps into 2 visual rows")
  eq(r.cursor_row, 4, "cursor on last visual row of second logical line")
end)

case("text_input_render_degenerate_width", function()
  local input = TextInput.new()
  input:insert_text("abc")
  local r = input:render("", 0, 1)
  eq(#r.lines, 3, "usable=1 means one char per visual row")
  eq(r.cursor_row, 3, "cursor on last row")
end)

case("text_input_render_empty_input_with_width", function()
  local input = TextInput.new()
  local r = input:render("> ", 2, 10)
  eq(#r.lines, 1, "empty input still produces one row")
  eq(r.cursor_row, 1, "cursor on that single row")
  eq(find_cursor_char(r.lines[1]), " ", "cursor is a space on empty input")
end)

case("text_input_render_wrap_utf8_multibyte_at_boundary", function()
  local input = TextInput.new()
  input:insert_text("aaéé")
  local r = input:render("", 0, 3)
  eq(#r.lines, 2, "4 codepoints at usable=3 wraps into 2 rows")
  eq(span_text(r.lines[1]), "aaé", "first chunk has 3 codepoints")
  local second = span_text(r.lines[2])
  assert(second:find("é"), "second chunk contains the remaining é")
  eq(r.cursor_row, 2, "cursor on second row")
end)

case("text_input_render_wrap_cursor_at_exact_chunk_boundary", function()
  local input = TextInput.new()
  input:insert_text("abcdef")
  for _ = 1, 3 do
    input:handle_key("left")
  end
  local r = input:render("", 0, 3)
  eq(#r.lines, 2, "6 chars at usable=3 -> 2 rows")
  eq(r.cursor_row, 2, "cursor col=3 lands in second chunk")
  eq(find_cursor_char(r.lines[2]), "d", "cursor char is 'd'")
end)

case("text_input_render_exact_fit_no_extra_row", function()
  local input = TextInput.new()
  input:insert_text("abcdef")
  local r = input:render(">>", 2, 8)
  eq(#r.lines, 1, "6 chars exactly fills usable=6, no extra row")
  eq(r.cursor_row, 1)
end)

case("text_input_render_empty_lines_in_multiline_with_width", function()
  local input = TextInput.new()
  input:insert_text("ab\n\ncd")
  local r = input:render("> ", 2, 10)
  eq(#r.lines, 3, "three logical lines produce three visual rows")
  eq(r.cursor_row, 3, "cursor on last line")
end)

case("text_input_render_cursor_at_start_with_wrapping", function()
  local input = TextInput.new()
  input:insert_text("abcdef")
  input:handle_key("home")
  local r = input:render("", 0, 3)
  eq(r.cursor_row, 1, "cursor at col=0 is in the first chunk")
  eq(find_cursor_char(r.lines[1]), "a", "cursor on first char 'a'")
end)

case("text_input_render_prefix_width_override", function()
  local input = TextInput.new()
  input:insert_text("abcdefgh")
  local r = input:render("X", 4, 8)
  eq(#r.lines, 2, "usable = 8-4 = 4, 8 chars wraps into 2 rows")
  local first = span_text(r.lines[1])
  assert(first:sub(1, 1) == "X", "first row starts with actual prefix 'X'")
  local second = span_text(r.lines[2])
  assert(second:sub(1, 4) == "    ", "continuation uses prefix_width=4 spaces of padding")
end)

case("text_input_invariants_hold_under_random_sequence", function()
  TextInput._debug = true
  local input = TextInput.new()
  local keys = {
    "a",
    "b",
    "c",
    "x",
    "é",
    "space",
    "newline",
    "left",
    "right",
    "up",
    "down",
    "home",
    "end",
    "backspace",
    "delete",
    "ctrl+w",
    "ctrl+left",
    "ctrl+right",
    "ctrl+a",
    "ctrl+k",
    "alt+d",
    "alt+b",
    "alt+f",
  }
  math.randomseed(0xC0FFEE)
  for _ = 1, 2000 do
    local k = keys[math.random(#keys)]
    if k == "é" then
      input:insert_text("é")
    else
      input:handle_key(k)
    end
  end
  TextInput._debug = false
end)

-- Inline parity cases. Each case sets up an initial value/cursor, applies a
-- sequence of keys, and asserts final value+cursor. Add a case here whenever
-- you change handle_key semantics. Lives in Lua now that there is no second
-- implementation to cross-check against; if a Rust TextBuffer comes back,
-- promote this back to a shared golden file.
local TRACE_CASES = {
  {
    name = "plain_insert",
    initial = "",
    cur = { 1, 0 },
    keys = { "h", "i" },
    final_value = "hi",
    final_cur = { 1, 2 },
  },
  {
    name = "backspace_deletes_char",
    initial = "abc",
    cur = { 1, 3 },
    keys = { "backspace" },
    final_value = "ab",
    final_cur = { 1, 2 },
  },
  {
    name = "delete_at_end_joins_lines",
    initial = "ab\ncd",
    cur = { 1, 2 },
    keys = { "delete" },
    final_value = "abcd",
    final_cur = { 1, 2 },
  },
  {
    name = "backspace_at_line_start_joins",
    initial = "ab\ncd",
    cur = { 2, 0 },
    keys = { "backspace" },
    final_value = "abcd",
    final_cur = { 1, 2 },
  },
  {
    name = "left_then_right_round_trips",
    initial = "abc",
    cur = { 1, 2 },
    keys = { "left", "right" },
    final_value = "abc",
    final_cur = { 1, 2 },
  },
  {
    name = "right_wraps_to_next_line",
    initial = "ab\ncd",
    cur = { 1, 2 },
    keys = { "right" },
    final_value = "ab\ncd",
    final_cur = { 2, 0 },
  },
  {
    name = "left_wraps_to_prev_line",
    initial = "ab\ncd",
    cur = { 2, 0 },
    keys = { "left" },
    final_value = "ab\ncd",
    final_cur = { 1, 2 },
  },
  {
    name = "home_jumps_to_col_zero",
    initial = "hello",
    cur = { 1, 5 },
    keys = { "home" },
    final_value = "hello",
    final_cur = { 1, 0 },
  },
  {
    name = "end_jumps_to_line_length",
    initial = "hello",
    cur = { 1, 0 },
    keys = { "end" },
    final_value = "hello",
    final_cur = { 1, 5 },
  },
  {
    name = "up_clamps_to_short_line",
    initial = "abc\nlonger_line",
    cur = { 2, 11 },
    keys = { "up" },
    final_value = "abc\nlonger_line",
    final_cur = { 1, 3 },
  },
  {
    name = "down_moves_to_next_line",
    initial = "ab\ncd",
    cur = { 1, 0 },
    keys = { "down" },
    final_value = "ab\ncd",
    final_cur = { 2, 0 },
  },
  {
    name = "ctrl_left_jumps_word",
    initial = "hello world",
    cur = { 1, 11 },
    keys = { "ctrl+left" },
    final_value = "hello world",
    final_cur = { 1, 6 },
  },
  {
    name = "ctrl_left_twice_lands_at_zero",
    initial = "hello world",
    cur = { 1, 11 },
    keys = { "ctrl+left", "ctrl+left" },
    final_value = "hello world",
    final_cur = { 1, 0 },
  },
  {
    name = "ctrl_right_jumps_word",
    initial = "hello world",
    cur = { 1, 0 },
    keys = { "ctrl+right" },
    final_value = "hello world",
    final_cur = { 1, 5 },
  },
  {
    name = "ctrl_right_eats_leading_spaces_then_word",
    initial = "hello  ",
    cur = { 1, 0 },
    keys = { "ctrl+right" },
    final_value = "hello  ",
    final_cur = { 1, 5 },
  },
  {
    name = "ctrl_left_eats_leading_spaces_then_word",
    initial = "  hello",
    cur = { 1, 7 },
    keys = { "ctrl+left" },
    final_value = "  hello",
    final_cur = { 1, 2 },
  },
  {
    name = "ctrl_w_eats_trailing_spaces_and_word",
    initial = "hello world  ",
    cur = { 1, 13 },
    keys = { "ctrl+w" },
    final_value = "hello ",
    final_cur = { 1, 6 },
  },
  {
    name = "ctrl_w_twice_clears_input",
    initial = "hello world",
    cur = { 1, 11 },
    keys = { "ctrl+w", "ctrl+w" },
    final_value = "",
    final_cur = { 1, 0 },
  },
  {
    name = "ctrl_w_at_line_start_joins",
    initial = "ab\ncd",
    cur = { 2, 0 },
    keys = { "ctrl+w" },
    final_value = "abcd",
    final_cur = { 1, 2 },
  },
  {
    name = "ctrl_delete_eats_word_after",
    initial = "hello world",
    cur = { 1, 0 },
    keys = { "ctrl+delete" },
    final_value = " world",
    final_cur = { 1, 0 },
  },
  {
    name = "alt_d_eats_word_after_space",
    initial = "hello world",
    cur = { 1, 6 },
    keys = { "alt+d" },
    final_value = "hello ",
    final_cur = { 1, 6 },
  },
  {
    name = "ctrl_delete_at_line_end_joins",
    initial = "ab\ncd",
    cur = { 1, 2 },
    keys = { "ctrl+delete" },
    final_value = "abcd",
    final_cur = { 1, 2 },
  },
  {
    name = "ctrl_k_truncates_line",
    initial = "hello world",
    cur = { 1, 5 },
    keys = { "ctrl+k" },
    final_value = "hello",
    final_cur = { 1, 5 },
  },
  {
    name = "ctrl_k_at_line_end_joins",
    initial = "ab\ncd",
    cur = { 1, 2 },
    keys = { "ctrl+k" },
    final_value = "ab\ncd",
    final_cur = { 1, 2 },
  },
  {
    name = "ctrl_a_moves_home",
    initial = "hello",
    cur = { 1, 5 },
    keys = { "ctrl+a" },
    final_value = "hello",
    final_cur = { 1, 0 },
  },
  {
    name = "alt_b_aliases_ctrl_left",
    initial = "hello world",
    cur = { 1, 11 },
    keys = { "alt+b" },
    final_value = "hello world",
    final_cur = { 1, 6 },
  },
  {
    name = "alt_f_aliases_ctrl_right",
    initial = "hello world",
    cur = { 1, 0 },
    keys = { "alt+f" },
    final_value = "hello world",
    final_cur = { 1, 5 },
  },
  {
    name = "newline_splits_line",
    initial = "abcd",
    cur = { 1, 2 },
    keys = { "newline" },
    final_value = "ab\ncd",
    final_cur = { 2, 0 },
  },
  {
    name = "space_inserts_a_space",
    initial = "abcd",
    cur = { 1, 2 },
    keys = { "space" },
    final_value = "ab cd",
    final_cur = { 1, 3 },
  },
  {
    name = "utf8_left_over_multibyte",
    initial = "aé",
    cur = { 1, 3 },
    keys = { "left" },
    final_value = "aé",
    final_cur = { 1, 1 },
  },
  {
    name = "utf8_backspace_removes_codepoint",
    initial = "aé",
    cur = { 1, 3 },
    keys = { "backspace" },
    final_value = "a",
    final_cur = { 1, 1 },
  },
  {
    name = "utf8_ctrl_w_eats_multibyte_word",
    initial = "hello wörld",
    cur = { 1, 12 },
    keys = { "ctrl+w" },
    final_value = "hello ",
    final_cur = { 1, 6 },
  },
  {
    name = "tab_is_whitespace_for_ctrl_w",
    initial = "hello\tworld",
    cur = { 1, 11 },
    keys = { "ctrl+w" },
    final_value = "hello\t",
    final_cur = { 1, 6 },
  },
  {
    name = "ignored_backspace_at_buffer_start",
    initial = "",
    cur = { 1, 0 },
    keys = { "backspace" },
    final_value = "",
    final_cur = { 1, 0 },
    results = { R.IGNORED },
  },
  {
    name = "ignored_left_at_buffer_start",
    initial = "abc",
    cur = { 1, 0 },
    keys = { "left" },
    final_value = "abc",
    final_cur = { 1, 0 },
    results = { R.IGNORED },
  },
  {
    name = "ignored_right_at_buffer_end",
    initial = "abc",
    cur = { 1, 3 },
    keys = { "right" },
    final_value = "abc",
    final_cur = { 1, 3 },
    results = { R.IGNORED },
  },
  {
    name = "ignored_up_on_first_line",
    initial = "abc",
    cur = { 1, 1 },
    keys = { "up" },
    final_value = "abc",
    final_cur = { 1, 1 },
    results = { R.IGNORED },
  },
  {
    name = "ignored_down_on_last_line",
    initial = "abc",
    cur = { 1, 1 },
    keys = { "down" },
    final_value = "abc",
    final_cur = { 1, 1 },
    results = { R.IGNORED },
  },
  {
    name = "ignored_ctrl_w_at_buffer_start",
    initial = "abc",
    cur = { 1, 0 },
    keys = { "ctrl+w" },
    final_value = "abc",
    final_cur = { 1, 0 },
    results = { R.IGNORED },
  },
  {
    name = "ignored_delete_at_buffer_end",
    initial = "abc",
    cur = { 1, 3 },
    keys = { "delete" },
    final_value = "abc",
    final_cur = { 1, 3 },
    results = { R.IGNORED },
  },
}

case("text_input_trace_cases", function()
  for _, c in ipairs(TRACE_CASES) do
    local input = TextInput.new()
    input:insert_text(c.initial)
    input.line, input.col = c.cur[1], c.cur[2]
    local got = {}
    for _, k in ipairs(c.keys) do
      got[#got + 1] = input:handle_key(k)
    end
    eq(input:value(), c.final_value, c.name .. ": value")
    eq(input.line, c.final_cur[1], c.name .. ": cursor line")
    eq(input.col, c.final_cur[2], c.name .. ": cursor col")
    if c.results then
      for i, want in ipairs(c.results) do
        eq(got[i], want, c.name .. ": key " .. i .. " result")
      end
    end
  end
end)

local ListPicker = require("maki.list_picker")

case("set_highlight_number_width_scales", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 200 })
  local lines = {}
  for i = 1, 100 do
    lines[i] = "x"
  end
  local content = table.concat(lines, "\n")
  local ok = view:set_highlight(content, "txt")
  eq(ok, true)
  eq(view.ring_count, 100)
  local first_nr = buf.lines[1][1][1]
  local last_nr = buf.lines[100][1][1]
  eq(first_nr, "  1 ", "3-digit width for 100 lines, right-aligned")
  eq(last_nr, "100 ", "line 100 should fill the width")
  eq(buf.lines[1][1][2], "line_nr")
end)

case("set_highlight_empty_content_returns_false", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3 })
  eq(view:set_highlight("", "txt"), false)
  eq(view:set_highlight("\n", "txt"), false)
  eq(buf.lines, nil, "nothing flushed for empty content")
end)

case("set_highlight_toggle_keeps_lines_and_collapses_back", function()
  local buf = mock_buf()
  local view = ToolView.new(buf, { max_lines = 3, keep = "head" })
  eq(view:set_highlight("a\nb\nc\nd\ne", "txt"), true)

  view:toggle()
  eq(view.expanded, true)
  eq(#buf.lines, 5, "expanded renders every line")
  eq(buf.lines[1][2][1], "a")
  eq(buf.lines[5][2][1], "e")

  view:toggle()
  eq(view.expanded, false)
  eq(buf.lines[3][2][1], "c", "collapsed shows the head window")
  eq(buf.lines[4][1][1], "... (2 lines) (click to expand)")
end)

local render_lines = ListPicker._render_lines

case("render_lines_string_items_basic", function()
  local lines = render_lines({ "alpha", "beta" }, 1, 40)
  eq(#lines, 2)
  eq(lines[1][1][1], "  alpha")
  eq(lines[1][1][2], "selected")
  eq(lines[2][1][2], "item")
end)

case("render_lines_table_items_with_detail", function()
  local items = {
    { label = "foo", detail = "(3 bytes)" },
    { label = "bar", detail = "(10 bytes)" },
  }
  local lines = render_lines(items, 2, 60)
  eq(lines[1][1][2], "item", "unselected label style")
  eq(lines[1][3][2], "dim", "unselected detail style")
  eq(lines[2][1][2], "selected", "selected label style")
  eq(lines[2][3][2], "selected", "selected detail uses selected")
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
  eq(lines[1][1][2], "item")
  eq(lines[2][1][2], "item")
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

case("render_lines_match_highlight_selected", function()
  local lines = render_lines({ "alpha", "beta" }, 1, 40, "lph")
  eq(lines[1][1][1], "  a")
  eq(lines[1][1][2], "selected")
  eq(lines[1][2][1], "lph")
  eq(lines[1][2][2], "match_selected")
  eq(lines[1][3][1], "a")
  eq(lines[1][3][2], "selected")
end)

case("render_lines_match_highlight_not_selected", function()
  local lines = render_lines({ "beta", "alpha" }, 2, 40, "et")
  eq(lines[1][1][1], "  b")
  eq(lines[1][1][2], "item")
  eq(lines[1][2][1], "et")
  eq(lines[1][2][2], "match")
  eq(lines[1][3][1], "a")
  eq(lines[1][3][2], "item")
end)

case("render_lines_detail_right_pad_always_present", function()
  local items = { { label = "x", detail = "d" } }
  local lines = render_lines(items, 1, 50)
  local right_pad = lines[1][4][1]
  eq(#right_pad, 2, "DETAIL_RIGHT_PAD = 2")
end)

local filter_items = ListPicker._filter_items

case("filter_items_empty_query_returns_all", function()
  local items = { "alpha", "beta", "gamma" }
  local filtered, indices = filter_items(items, "")
  eq(#filtered, 3)
  eq(indices[1], 1)
  eq(indices[2], 2)
  eq(indices[3], 3)
end)

case("filter_items_case_insensitive", function()
  local items = { "Alpha", "BETA", "gamma" }
  local filtered, indices = filter_items(items, "al")
  eq(#filtered, 1)
  eq(filtered[1], "Alpha")
  eq(indices[1], 1)
end)

case("filter_items_no_matches", function()
  local items = { "apple", "banana" }
  local filtered, indices = filter_items(items, "xyz")
  eq(#filtered, 0)
  eq(#indices, 0)
end)

case("filter_items_table_items_uses_label", function()
  local items = {
    { label = "Foo", detail = "d1" },
    { label = "Bar", detail = "d2" },
    { label = "Foobar", detail = "d3" },
  }
  local filtered, indices = filter_items(items, "foo")
  eq(#filtered, 2)
  eq(filtered[1].label, "Foo")
  eq(filtered[2].label, "Foobar")
  eq(indices[1], 1)
  eq(indices[2], 3)
end)

case("filter_items_every_word_must_match", function()
  local items = { "review gh pr 441", "review gh pr 461", "new session" }
  local filtered = filter_items(items, "441 review")
  eq(#filtered, 1)
  eq(filtered[1], "review gh pr 441")
end)

case("highlight_spans_overlapping_words_merge", function()
  local spans = ListPicker.highlight_spans("alphabet", { "alpha", "phab" }, "item", "match")
  eq(#spans, 2)
  eq(spans[1][1], "alphab", "alpha(1-5) + phab(3-6) merge into one span")
  eq(spans[1][2], "match")
  eq(spans[2][1], "et")
  eq(spans[2][2], "item")
end)

case("highlight_spans_multi_word", function()
  local spans = ListPicker.highlight_spans("review pr 441", { "pr", "441" }, "item", "match")
  eq(#spans, 4)
  eq(spans[1][1], "review ")
  eq(spans[1][2], "item")
  eq(spans[2][1], "pr")
  eq(spans[2][2], "match")
  eq(spans[3][1], " ")
  eq(spans[3][2], "item")
  eq(spans[4][1], "441")
  eq(spans[4][2], "match")
end)

case("render_lines_match_at_start_keeps_indent", function()
  local lines = render_lines({ "alpha" }, 1, 40, "al")
  eq(lines[1][1][1], "  ")
  eq(lines[1][1][2], "selected")
  eq(lines[1][2][1], "al")
  eq(lines[1][2][2], "match_selected")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
