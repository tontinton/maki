local QuestionForm = require("question_form")
local QuestionHelpers = require("question_helpers")

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

local MODE = QuestionForm.MODE

local function single_question(overrides)
  local q = {
    question = "Pick one",
    header = "",
    multiple = false,
    options = {
      { label = "Yes", description = "the yes" },
      { label = "No" },
    },
  }
  for k, v in pairs(overrides or {}) do
    q[k] = v
  end
  return { q }
end

local function multi_questions()
  return {
    { question = "A?", header = "a", multiple = false, options = { { label = "a1" }, { label = "a2" } } },
    { question = "B?", header = "b", multiple = false, options = { { label = "b1" }, { label = "b2" } } },
  }
end

local function press(state, key)
  return QuestionForm._handle_key(state, key)
end

local function press_many(state, keys)
  for _, k in ipairs(keys) do
    press(state, k)
  end
end

local function type_text(state, text)
  for i = 1, #text do
    press(state, text:sub(i, i))
  end
end

case("dismiss_keys_per_mode", function()
  -- selecting + confirming dismiss on esc/ctrl+c; editing_custom dismisses only on ctrl+c (esc returns to selecting).
  local cases = {
    {
      build = function()
        return QuestionForm._initial_state(single_question())
      end,
      key = "esc",
    },
    {
      build = function()
        return QuestionForm._initial_state(single_question())
      end,
      key = "ctrl+c",
    },
    {
      build = function()
        local s = QuestionForm._initial_state(single_question())
        press_many(s, { "down", "down", "enter" })
        return s
      end,
      key = "ctrl+c",
    },
    {
      build = function()
        local s = QuestionForm._initial_state(multi_questions())
        press_many(s, { "enter", "enter" })
        return s
      end,
      key = "esc",
    },
    {
      build = function()
        local s = QuestionForm._initial_state(multi_questions())
        press_many(s, { "enter", "enter" })
        return s
      end,
      key = "ctrl+c",
    },
  }
  for i, c in ipairs(cases) do
    local s = c.build()
    press(s, c.key)
    eq(s.done and s.done.type, "dismiss", "case " .. i .. " key=" .. c.key)
  end
end)

case("multiple_choice_toggle_then_tab_to_review_and_submit", function()
  local s = QuestionForm._initial_state(single_question({ multiple = true }))
  press(s, "enter")
  eq(s.answers[1][1], "Yes", "first enter toggles on")
  eq(s.mode, MODE.SELECTING, "multi-toggle stays in selecting")
  press(s, "enter")
  eq(s.answers[1] == nil or #s.answers[1] == 0, true, "second enter toggles off")
  press(s, "enter")
  press(s, "tab")
  eq(s.mode, MODE.CONFIRMING)
  press(s, "enter")
  eq(s.done.type, "submit")
  eq(s.done.answers[1][1], "Yes")
end)

case("arrow_keys_navigate_questions_and_clamp_at_ends", function()
  -- Right/left alias Tab/Shift+Tab. Asserting the aliased keys covers both behaviors.
  local s = QuestionForm._initial_state(multi_questions())
  press(s, "left")
  eq(s.tab, 1, "shift+tab at first question is a no-op")
  press(s, "right")
  eq(s.tab, 2)
  press(s, "right")
  eq(s.mode, MODE.CONFIRMING, "past last question goes to review")
  press(s, "left")
  eq(s.mode, MODE.SELECTING, "shift+tab from confirming returns to last question")
  eq(s.tab, #s.questions)
end)

case("enter_advances_through_questions_then_confirming", function()
  local s = QuestionForm._initial_state(multi_questions())
  press(s, "enter")
  eq(s.tab, 2, "after selecting q1, auto-advance to q2")
  eq(s.answers[1][1], "a1")
  press(s, "enter")
  eq(s.mode, MODE.CONFIRMING, "last question lands on review")
  eq(s.answers[2][1], "b1")
end)

case("editing_custom_esc_returns_to_selecting", function()
  local s = QuestionForm._initial_state(single_question())
  press_many(s, { "down", "down", "enter" })
  eq(s.mode, MODE.EDITING_CUSTOM)
  press(s, "esc")
  eq(s.mode, MODE.SELECTING)
  eq(s.done, nil, "esc in editing_custom must NOT dismiss the form")
end)

case("editing_custom_empty_or_whitespace_submit_returns_to_selecting", function()
  for _, prefix in ipairs({ {}, { "space", "space" } }) do
    local s = QuestionForm._initial_state(single_question())
    press_many(s, { "down", "down", "enter" })
    press_many(s, prefix)
    press(s, "enter")
    eq(s.mode, MODE.SELECTING, "empty/whitespace must not advance")
    eq(s.answers[1], nil, "no answer recorded")
  end
end)

case("editing_custom_submits_trimmed_text_and_finishes_single_question", function()
  local s = QuestionForm._initial_state(single_question())
  press_many(s, { "down", "down", "enter", "space", "h", "i", "space", "enter" })
  eq(s.answers[1][1], "hi", "leading/trailing whitespace trimmed")
  eq(s.done.type, "submit")
end)

case("editing_custom_newline_shortcuts_insert_not_submit", function()
  for _, key in ipairs({ "alt+enter", "shift+enter", "ctrl+enter", "ctrl+j" }) do
    local s = QuestionForm._initial_state(single_question())
    press_many(s, { "down", "down", "enter", "a", key, "b" })
    eq(s.mode, MODE.EDITING_CUSTOM, key .. ": stays in editing")
    eq(s.custom_input:value(), "a\nb", key .. ": inserted newline")
  end
  -- Backslash+Enter takes a different path: backslash is consumed and a newline inserted in its place.
  local s = QuestionForm._initial_state(single_question())
  press_many(s, { "down", "down", "enter", "a", "\\", "enter", "b" })
  eq(s.mode, MODE.EDITING_CUSTOM)
  eq(s.custom_input:value(), "a\nb", "backslash+enter inserts newline, consumes backslash")
end)

case("confirming_enter_submits_all_answers", function()
  local s = QuestionForm._initial_state(multi_questions())
  press_many(s, { "enter", "enter", "enter" })
  eq(s.done.type, "submit")
  eq(s.done.answers[1][1], "a1")
  eq(s.done.answers[2][1], "b1")
end)

case("escape_cell_table_driven", function()
  local cases = {
    { "a\\b", "a\\\\b", "backslash doubles" },
    { "a|b", "a\\|b", "pipe escapes" },
    { "a\nb", "a<br>b", "LF -> <br>" },
    { "a\r\nb", "a<br>b", "CRLF -> <br>" },
    { "\\|", "\\\\\\|", "backslash escaped before pipe (no double-escape)" },
    { "hello", "hello", "plain text unchanged" },
  }
  for _, c in ipairs(cases) do
    eq(QuestionHelpers.escape_cell(c[1]), c[2], c[3])
  end
end)

case("format_answer_table_renders_header_separator_rows_and_missing_answers", function()
  local questions = { { question = "Q1" }, { question = "Q2" } }
  local out = QuestionHelpers.format_answer_table(questions, { { "a", "b" } })
  assert(out:find("| Question | Answer |", 1, true), "header present")
  assert(out:find("|----------|--------|", 1, true), "separator present")
  assert(out:find("| Q1 | a, b |", 1, true), "answers joined with commas")
  assert(out:find("| Q2 | %(no answer%) |"), "missing answers render as (no answer)")
end)

case("format_answer_table_escapes_pipes_and_newlines_in_both_columns", function()
  local questions = { { question = "Has | pipe\nand newline" } }
  local out = QuestionHelpers.format_answer_table(questions, { { "ans|with|pipes" } })
  assert(out:find("Has \\| pipe<br>and newline", 1, true), "question escaped")
  assert(out:find("ans\\|with\\|pipes", 1, true), "answer escaped")
end)

case("render_reserves_tab_bar_only_when_confirm_present", function()
  eq(QuestionForm._render(QuestionForm._initial_state(single_question()), 80).reserved_top, 0)
  eq(QuestionForm._render(QuestionForm._initial_state(multi_questions()), 80).reserved_top, 2)
end)

case("render_footer_changes_between_selecting_and_editing", function()
  local s = QuestionForm._initial_state(single_question())
  local footer_selecting = QuestionForm._render(s, 80).footer
  press_many(s, { "down", "down", "enter" })
  local footer_editing = QuestionForm._render(s, 80).footer
  assert(footer_selecting ~= footer_editing, "footer must differ between selecting and editing modes")
end)

case("render_handles_long_question_text_by_wrapping", function()
  local long = string.rep("supercalifragilistic ", 10)
  local s = QuestionForm._initial_state(single_question({ question = long }))
  local r = QuestionForm._render(s, 40)
  assert(#r.lines > 1, "long question must wrap onto multiple lines")
end)

local function find_span_with_text(lines, text)
  for _, line in ipairs(lines) do
    for _, span in ipairs(line) do
      if span[1] == text then
        return span
      end
    end
  end
  return nil
end

case("render_selecting_uses_markdown_styles_for_prompt", function()
  local s = QuestionForm._initial_state(single_question({ question = "Pick a **bold** option" }))
  local r = QuestionForm._render(s, 80)
  local span = find_span_with_text(r.lines, "bold")
  assert(span, "expected to find span containing 'bold'")
  eq(span[2], "bold", "markdown bold delimiters must map to the 'bold' style name")
end)

case("render_selecting_preserves_bold_italic_style_name", function()
  -- The old lossy mapping collapsed `***...***` to "bold" and dropped italic.
  local s = QuestionForm._initial_state(single_question({ question = "Try ***both*** styles" }))
  local r = QuestionForm._render(s, 80)
  local span = find_span_with_text(r.lines, "both")
  assert(span, "expected to find span containing 'both'")
  eq(span[2], "bold_italic", "*** delimiters must map to the 'bold_italic' style name")
end)

case("render_selecting_falls_back_on_markdown_failure", function()
  local original = maki.ui.markdown
  maki.ui.markdown = function()
    error("boom")
  end
  local ok, r = pcall(QuestionForm._render, QuestionForm._initial_state(single_question()), 80)
  maki.ui.markdown = original
  assert(ok, "render must not propagate markdown errors")
  local span = find_span_with_text(r.lines, "Pick one")
  assert(span, "fallback must still surface the question text as a plain span")
  eq(span[2], "", "fallback span must be plain (empty style name)")
end)

case("render_selecting_caches_markdown_across_renders", function()
  local original = maki.ui.markdown
  local calls = 0
  maki.ui.markdown = function(text)
    calls = calls + 1
    return original(text)
  end
  local s = QuestionForm._initial_state(single_question())
  QuestionForm._render(s, 80)
  QuestionForm._render(s, 80)
  maki.ui.markdown = original
  eq(calls, 1, "markdown must be parsed exactly once per question across renders")
end)

local function multi_with_custom()
  return single_question({ multiple = true, options = { { label = "a1" }, { label = "a2" } } })
end

case("multi_custom_appends_keeps_predefined_selections", function()
  local s = QuestionForm._initial_state(multi_with_custom())
  press(s, "enter")
  press_many(s, { "down", "enter" })
  press_many(s, { "down", "down", "enter" })
  eq(s.mode, MODE.EDITING_CUSTOM)
  type_text(s, "foo")
  press(s, "enter")
  eq(s.mode, MODE.SELECTING)
  eq(s.done, nil, "multi custom submit must not finish")
  local ans = s.answers[1]
  eq(#ans, 3)
  eq(ans[1], "a1")
  eq(ans[2], "a2")
  eq(ans[3], "foo")
end)

case("multi_custom_resubmit_replaces_only_custom", function()
  local s = QuestionForm._initial_state(multi_with_custom())
  press_many(s, { "enter", "down", "enter", "down", "down", "enter" })
  type_text(s, "foo")
  press(s, "enter")
  press(s, "enter")
  press_many(s, { "backspace", "backspace", "backspace" })
  type_text(s, "bar")
  press(s, "enter")
  local ans = s.answers[1]
  eq(#ans, 3)
  eq(ans[1], "a1")
  eq(ans[2], "a2")
  eq(ans[3], "bar")
end)

case("multi_custom_reopen_prefills_editor", function()
  local s = QuestionForm._initial_state(multi_with_custom())
  press_many(s, { "down", "down", "enter" })
  type_text(s, "foo")
  press_many(s, { "enter", "enter" })
  eq(s.mode, MODE.EDITING_CUSTOM)
  eq(s.custom_input:value(), "foo")
end)

case("multi_custom_clearing_keeps_predefined", function()
  local s = QuestionForm._initial_state(single_question({ multiple = true }))
  press(s, "enter")
  eq(s.answers[1][1], "Yes", "predefined selected")
  press_many(s, { "down", "down", "enter", "h", "i", "enter" })
  eq(#s.answers[1], 2, "predefined + custom selected")
  press_many(s, { "enter", "backspace", "backspace", "enter" })
  eq(#s.answers[1], 1, "only predefined remains")
  eq(s.answers[1][1], "Yes")
end)

local function with_markdown_mock(fn, mock)
  local original = maki.ui.markdown
  maki.ui.markdown = mock
  local ok, err = pcall(fn)
  maki.ui.markdown = original
  if not ok then
    error(err)
  end
end

local function count_lines_with_text(lines, text)
  local n = 0
  for _, line in ipairs(lines) do
    for _, span in ipairs(line) do
      if span[1] == text then
        n = n + 1
        break
      end
    end
  end
  return n
end

local GUTTER = " "

case("render_selecting_emits_one_line_per_markdown_line_with_gutter", function()
  with_markdown_mock(function()
    local s = QuestionForm._initial_state(single_question({ question = "irrelevant" }))
    local r = QuestionForm._render(s, 80)
    eq(count_lines_with_text(r.lines, "line one"), 1, "first markdown line emitted once")
    eq(count_lines_with_text(r.lines, "line two"), 1, "second markdown line emitted once")
    for _, line in ipairs(r.lines) do
      for _, span in ipairs(line) do
        if span[1] == "line one" or span[1] == "line two" then
          eq(line[1][1], GUTTER, "markdown line must start with gutter span text")
          eq(line[1][2], "", "gutter span style must be empty")
          break
        end
      end
    end
  end, function()
    return { { { "line one", "" } }, { { "line two", "" } } }
  end)
end)

case("inline_md_returns_only_first_markdown_line_in_confirming", function()
  with_markdown_mock(function()
    local s = QuestionForm._initial_state(multi_questions())
    press_many(s, { "enter", "enter" })
    eq(s.mode, MODE.CONFIRMING)
    local r = QuestionForm._render(s, 80)
    assert(find_span_with_text(r.lines, "first"), "confirming row must include first markdown line")
    assert(not find_span_with_text(r.lines, "second"), "confirming row must NOT include subsequent markdown lines")
  end, function()
    return { { { "first", "" } }, { { "second", "" } } }
  end)
end)

case("question_md_caches_per_question_index", function()
  local seen = {}
  local calls = 0
  with_markdown_mock(function()
    local s = QuestionForm._initial_state(multi_questions())
    QuestionForm._render(s, 80)
    press_many(s, { "enter", "enter" })
    QuestionForm._render(s, 80)
    press(s, "enter")
    eq(s.mode, MODE.CONFIRMING)
    QuestionForm._render(s, 80)
    QuestionForm._render(s, 80)
    eq(calls, 2, "markdown must be parsed once per unique question text")
    eq(seen["A?"], 1, "Q1 text parsed exactly once")
    eq(seen["B?"], 1, "Q2 text parsed exactly once")
  end, function(text)
    calls = calls + 1
    seen[text] = (seen[text] or 0) + 1
    return { { { text, "" } } }
  end)
end)

case("question_md_fallback_on_non_table_return", function()
  with_markdown_mock(function()
    local s = QuestionForm._initial_state(single_question())
    local r = QuestionForm._render(s, 80)
    local span = find_span_with_text(r.lines, "Pick one")
    assert(span, "non-table markdown return must fall back to plain question text")
    eq(span[2], "", "fallback span must be plain (empty style)")
  end, function()
    return "not a table"
  end)
end)

case("question_md_fallback_on_empty_table_return", function()
  with_markdown_mock(function()
    local s = QuestionForm._initial_state(single_question())
    local r = QuestionForm._render(s, 80)
    local span = find_span_with_text(r.lines, "Pick one")
    assert(span, "empty-table markdown return must fall back to plain question text")
    eq(span[2], "", "fallback span must be plain (empty style)")
  end, function()
    return {}
  end)
end)

case("render_confirming_preserves_markdown_styles_in_question_row", function()
  with_markdown_mock(function()
    local s = QuestionForm._initial_state(multi_questions())
    press_many(s, { "enter", "enter" })
    eq(s.mode, MODE.CONFIRMING)
    local r = QuestionForm._render(s, 80)
    local span = find_span_with_text(r.lines, "world")
    assert(span, "expected styled span from markdown in confirming row")
    eq(span[2], "bold", "markdown style must be preserved through inline_md")
  end, function()
    return { { { "Hello ", "" }, { "world", "bold" } } }
  end)
end)

case("render_width_zero_does_not_crash", function()
  local s = QuestionForm._initial_state(single_question())
  local ok, r = pcall(QuestionForm._render, s, 0)
  assert(ok, "render with width=0 must not crash")
  assert(r and r.lines, "render must still return a lines field")
end)

case("review_tab_label_present_and_changes_style_between_modes", function()
  local s = QuestionForm._initial_state(multi_questions())
  local r_selecting = QuestionForm._render(s, 80)
  local review_inactive = find_span_with_text(r_selecting.lines, " Review ")
  assert(review_inactive, "Review tab must appear in selecting mode")
  eq(review_inactive[2], "form_inactive", "Review tab inactive while not confirming")
  press_many(s, { "enter", "enter" })
  eq(s.mode, MODE.CONFIRMING)
  local r_confirming = QuestionForm._render(s, 80)
  local review_active = find_span_with_text(r_confirming.lines, " Review ")
  assert(review_active, "Review tab must appear in confirming mode")
  eq(review_active[2], "form_active", "Review tab active while confirming")
end)

case("tab_label_prefers_header_over_q_index_fallback", function()
  local questions = {
    { question = "A?", header = "", multiple = false, options = { { label = "a1" } } },
    { question = "B?", header = "abc", multiple = false, options = { { label = "b1" } } },
  }
  local r = QuestionForm._render(QuestionForm._initial_state(questions), 80)
  local tab_bar = r.lines[1]
  local has_q1, has_abc = false, false
  for _, span in ipairs(tab_bar) do
    if span[1]:find("Q1", 1, true) then
      has_q1 = true
    end
    if span[1]:find("abc", 1, true) then
      has_abc = true
    end
  end
  assert(has_q1, "empty header must fall back to Q<n> label")
  assert(has_abc, "non-empty header must be used as tab label")
end)

case("answered_non_current_tab_shows_check_glyph", function()
  local s = QuestionForm._initial_state(multi_questions())
  press(s, "enter")
  eq(s.tab, 2, "after answering Q1, cursor advances to Q2")
  local r = QuestionForm._render(s, 80)
  local tab_bar = r.lines[1]
  local q1_has_check, q2_has_check = false, false
  for _, span in ipairs(tab_bar) do
    if span[1]:find("a", 1, true) and span[1]:find("✓", 1, true) then
      q1_has_check = true
    end
    if span[1]:find("b", 1, true) and span[1]:find("✓", 1, true) then
      q2_has_check = true
    end
  end
  assert(q1_has_check, "answered non-current tab must show ✓")
  assert(not q2_has_check, "current unanswered tab must NOT show ✓")
end)

case("render_confirming_shows_no_answer_placeholder_for_unanswered_question", function()
  local s = QuestionForm._initial_state(multi_questions())
  press(s, "enter")
  eq(s.tab, 2)
  press(s, "right")
  eq(s.mode, MODE.CONFIRMING, "from last question, right goes to confirming")
  local r = QuestionForm._render(s, 80)
  local placeholder = find_span_with_text(r.lines, "(no answer)")
  assert(placeholder, "unanswered question row must contain '(no answer)' span")
end)

case("escape_cell_empty_string_returns_empty_string", function()
  eq(QuestionHelpers.escape_cell(""), "", "empty input escapes to empty output")
end)

case("format_answer_table_with_no_questions_returns_header_and_separator_only", function()
  local out = QuestionHelpers.format_answer_table({}, {})
  eq(out, "| Question | Answer |\n|----------|--------|", "empty questions yields header+separator with no data rows")
end)

case("render_selecting_focus_row_tracks_cursor_down_movement", function()
  local q = {
    question = "Pick",
    header = "",
    multiple = false,
    options = { { label = "o1" }, { label = "o2" }, { label = "o3" } },
  }
  local s = QuestionForm._initial_state({ q })
  local r1 = QuestionForm._render(s, 80)
  press_many(s, { "down", "down" })
  eq(s.cursor, 3, "two downs land on option 3")
  local r3 = QuestionForm._render(s, 80)
  assert(r3.focus_row > r1.focus_row, "focus_row must advance when cursor moves down")
  assert(r3.focus_row <= #r3.lines, "focus_row must stay within rendered line range")
end)

local DESC_LABEL_INDENT = 4
local DESC_SEP_WIDTH = 3
local DESC_WRAP_WIDTH = 30
local DESC_LONG = "alpha beta gamma delta epsilon zeta eta theta"
local PAD_MISMATCH_MSG = "description continuation line must align under first description char"

local function leading_space_count(line)
  local text = ""
  for _, span in ipairs(line) do
    text = text .. span[1]
  end
  return #(text:match("^( *)") or "")
end

local function continuation_after(lines, marker)
  for i, line in ipairs(lines) do
    for _, span in ipairs(line) do
      if span[1]:find(marker, 1, true) then
        return lines[i + 1]
      end
    end
  end
  return nil
end

case("render_selecting_aligns_description_continuation_under_first_desc_char", function()
  -- display_width = utf8.len matches #s for ASCII, differs for multibyte; both must align.
  local cases = {
    { label = "foo", expected_label_w = 3 },
    { label = "café", expected_label_w = 4 },
  }
  for _, c in ipairs(cases) do
    local q = {
      question = "Pick",
      header = "",
      multiple = false,
      options = { { label = c.label, description = DESC_LONG }, { label = "other" } },
    }
    local r = QuestionForm._render(QuestionForm._initial_state({ q }), DESC_WRAP_WIDTH)
    local cont = continuation_after(r.lines, "alpha")
    assert(cont, "label=" .. c.label .. ": expected a wrapped continuation line")
    local expected_pad = DESC_LABEL_INDENT + c.expected_label_w + DESC_SEP_WIDTH
    eq(leading_space_count(cont), expected_pad, PAD_MISMATCH_MSG .. " (label=" .. c.label .. ")")
  end
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
