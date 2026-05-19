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
