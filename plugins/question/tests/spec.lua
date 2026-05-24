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

local function selecting_single()
  return QuestionForm._initial_state(single_question())
end

local function editing_custom_single()
  local s = selecting_single()
  press_many(s, { "down", "down", "enter" })
  return s
end

local function confirming_multi()
  local s = QuestionForm._initial_state(multi_questions())
  press_many(s, { "enter", "enter" })
  return s
end

case("dismiss_keys_per_mode", function()
  local cases = {
    { build = selecting_single, key = "esc" },
    { build = selecting_single, key = "ctrl+c" },
    { build = editing_custom_single, key = "ctrl+c" },
    { build = confirming_multi, key = "esc" },
    { build = confirming_multi, key = "ctrl+c" },
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
  local s = editing_custom_single()
  eq(s.mode, MODE.EDITING_CUSTOM)
  press(s, "esc")
  eq(s.mode, MODE.SELECTING)
  eq(s.done, nil, "esc in editing_custom must NOT dismiss the form")
end)

case("editing_custom_empty_or_whitespace_submit_returns_to_selecting", function()
  for _, prefix in ipairs({ {}, { "space", "space" } }) do
    local s = editing_custom_single()
    press_many(s, prefix)
    press(s, "enter")
    eq(s.mode, MODE.SELECTING, "empty/whitespace must not advance")
    eq(s.answers[1], nil, "no answer recorded")
  end
end)

case("editing_custom_submits_trimmed_text_and_finishes_single_question", function()
  local s = selecting_single()
  press_many(s, { "down", "down", "enter", "space", "h", "i", "space", "enter" })
  eq(s.answers[1][1], "hi", "leading/trailing whitespace trimmed")
  eq(s.done.type, "submit")
end)

case("editing_custom_newline_shortcuts_insert_not_submit", function()
  for _, key in ipairs({ "alt+enter", "shift+enter", "ctrl+enter", "ctrl+j" }) do
    local s = selecting_single()
    press_many(s, { "down", "down", "enter", "a", key, "b" })
    eq(s.mode, MODE.EDITING_CUSTOM, key .. ": stays in editing")
    eq(s.custom_input:value(), "a\nb", key .. ": inserted newline")
  end
  local s = selecting_single()
  press_many(s, { "down", "down", "enter", "a", "\\", "enter", "b" })
  eq(s.mode, MODE.EDITING_CUSTOM)
  eq(s.custom_input:value(), "a\nb", "backslash+enter inserts newline, consumes backslash")
end)

case("format_answer_list_renders_questions_answers_pipes_newlines_and_missing", function()
  local questions = {
    { question = "Has | pipe\nand newline" },
    { question = "Q2" },
  }
  local out = QuestionHelpers.format_answer_list(questions, { { "a", "ans|with|pipes" } })
  assert(out:find("**Q1.** Has | pipe\nand newline", 1, true), "Q1 header + verbatim pipes/newlines")
  assert(out:find("**Q2.** Q2", 1, true), "Q2 header present")
  assert(out:find("\n- a\n", 1, true), "answer a on its own bullet")
  assert(out:find("\n- ans|with|pipes", 1, true), "answer pipes preserved verbatim on bullet")
  assert(out:find("- (no answer)", 1, true), "missing answer renders as (no answer)")
end)

case("format_answer_list_indents_multiline_answer_continuation", function()
  local out = QuestionHelpers.format_answer_list({ { question = "Q" } }, { { "a\nb" } })
  assert(out:find("- a\n  b", 1, true), "multi-line answer continuation indented with two spaces")
end)

case("format_answer_list_with_no_questions_returns_empty_string", function()
  eq(QuestionHelpers.format_answer_list({}, {}), "")
end)

case("render_reserves_tab_bar_only_when_confirm_present", function()
  eq(QuestionForm._render(selecting_single(), 80).reserved_top, 0)
  eq(QuestionForm._render(QuestionForm._initial_state(multi_questions()), 80).reserved_top, 2)
end)

local function line_width(line)
  local w = 0
  for _, span in ipairs(line) do
    w = w + (utf8.len(span[1]) or #span[1])
  end
  return w
end

local function assert_all_within(lines, max_width, label)
  for i, line in ipairs(lines) do
    assert(line_width(line) <= max_width, label .. " line " .. i .. " exceeds width " .. max_width)
  end
end

case("render_selecting_wraps_long_question_within_width", function()
  local long = string.rep("foo bar ", 20)
  local s = QuestionForm._initial_state(single_question({ question = long }))
  assert_all_within(QuestionForm._render(s, 40).lines, 40, "selecting")
end)

case("render_confirming_wraps_long_question_and_answer_within_width", function()
  local long_ans = string.rep("answerword ", 15)
  local long_q = string.rep("promptword ", 15)
  local s = QuestionForm._initial_state({
    { question = "Q1", header = "q1", multiple = false, options = { { label = "x" } } },
    { question = long_q, header = "q2", multiple = false, options = { { label = "y" } } },
  })
  s.mode = MODE.CONFIRMING
  s.answers = { { long_ans }, { "y" } }
  assert_all_within(QuestionForm._render(s, 40).lines, 40, "confirming")
end)

case("wrap_spans_preserves_style_across_break", function()
  local lines = QuestionForm._wrap_spans({ { "alpha beta gamma delta", "bold" } }, 11)
  assert(#lines >= 2, "expected wrapping")
  eq(lines[2][1][2], "bold", "style must carry to wrapped continuation")
end)

case("wrap_spans_hard_splits_oversize_word_on_valid_utf8_boundaries", function()
  for _, c in ipairs({ { word = "abcdefghij", width = 4 }, { word = "ééééééé", width = 3 } }) do
    local lines = QuestionForm._wrap_spans({ { c.word, "" } }, c.width)
    local rebuilt = ""
    for _, line in ipairs(lines) do
      assert(line_width(line) <= c.width, c.word .. ": line exceeds width")
      for _, span in ipairs(line) do
        assert(utf8.len(span[1]), c.word .. ": span is not valid utf8")
        rebuilt = rebuilt .. span[1]
      end
    end
    eq(rebuilt, c.word, c.word .. ": reassembled output must equal input")
  end
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

case("question_md_falls_back_to_plain_text_on_invalid_markdown_return", function()
  local original = maki.ui.markdown
  local mocks = {
    {
      name = "error",
      fn = function(_text, _width)
        error("boom")
      end,
    },
    {
      name = "non-table",
      fn = function(_text, _width)
        return "not a table"
      end,
    },
    {
      name = "empty-table",
      fn = function(_text, _width)
        return {}
      end,
    },
  }
  for _, m in ipairs(mocks) do
    maki.ui.markdown = m.fn
    local ok, r = pcall(QuestionForm._render, selecting_single(), 80)
    maki.ui.markdown = original
    assert(ok, m.name .. ": render must not propagate markdown errors")
    local span = find_span_with_text(r.lines, "Pick one")
    assert(span, m.name .. ": fallback must surface the question text")
    eq(span[2], "", m.name .. ": fallback span must be plain")
  end
end)

case("confirming_view_renders_all_question_lines_at_inline_width", function()
  local original = maki.ui.markdown
  maki.ui.markdown = function(_text, _width)
    return { { { "first", "" } }, { { "second", "" } } }
  end
  local s = confirming_multi()
  local r = QuestionForm._render(s, 80)
  maki.ui.markdown = original
  eq(s.mode, MODE.CONFIRMING)
  assert(find_span_with_text(r.lines, "first"), "confirming row must include first markdown line")
  assert(find_span_with_text(r.lines, "second"), "confirming row must also include subsequent markdown lines")
end)

case("question_md_cache_invalidates_on_width_change", function()
  local original = maki.ui.markdown
  local calls = 0
  maki.ui.markdown = function(_text, width)
    calls = calls + 1
    return { { { "w=" .. tostring(width), "" } } }
  end
  local s = selecting_single()
  QuestionForm._render(s, 80)
  local calls_after_80 = calls
  QuestionForm._render(s, 80)
  eq(calls, calls_after_80, "same width must reuse cache")
  QuestionForm._render(s, 60)
  maki.ui.markdown = original
  assert(calls > calls_after_80, "width change must invalidate cache and re-render")
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

case("render_width_zero_does_not_crash", function()
  local ok, r = pcall(QuestionForm._render, selecting_single(), 0)
  assert(ok, "render with width=0 must not crash")
  assert(r and r.lines, "render must still return a lines field")
end)

case("review_tab_label_present_and_styled_differently_between_modes", function()
  local s = QuestionForm._initial_state(multi_questions())
  local review_inactive = find_span_with_text(QuestionForm._render(s, 80).lines, " Review ")
  assert(review_inactive, "Review tab must appear in selecting mode")
  press_many(s, { "enter", "enter" })
  eq(s.mode, MODE.CONFIRMING)
  local review_active = find_span_with_text(QuestionForm._render(s, 80).lines, " Review ")
  assert(review_active, "Review tab must appear in confirming mode")
  assert(review_active[2] ~= review_inactive[2], "Review tab style must change between modes")
end)

case("tab_label_prefers_header_over_q_index_fallback", function()
  local questions = {
    { question = "A?", header = "", multiple = false, options = { { label = "a1" } } },
    { question = "B?", header = "abc", multiple = false, options = { { label = "b1" } } },
  }
  local tab_bar = QuestionForm._render(QuestionForm._initial_state(questions), 80).lines[1]
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
  local tab_bar = QuestionForm._render(s, 80).lines[1]
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
  press(s, "right")
  eq(s.mode, MODE.CONFIRMING, "from last question, right goes to confirming")
  local placeholder = find_span_with_text(QuestionForm._render(s, 80).lines, "(no answer)")
  assert(placeholder, "unanswered question row must contain '(no answer)' span")
end)

case("render_selecting_focus_row_tracks_cursor_down_movement", function()
  local s = QuestionForm._initial_state(single_question({
    options = { { label = "o1" }, { label = "o2" }, { label = "o3" } },
  }))
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
  for _, c in ipairs({
    { label = "foo", expected_label_w = 3 },
    { label = "café", expected_label_w = 4 },
  }) do
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
    eq(leading_space_count(cont), expected_pad, "label=" .. c.label .. ": continuation alignment")
  end
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
