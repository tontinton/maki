local ToolView = require("maki.tool_view")

local MIN_CARD_LINES = 10
local CARD_WIDTH = 80
local INDENT = "    "
local ANSWER_PREFIX = "    ✓ "
local ANSWER_INDENT = string.rep(" ", utf8.len(ANSWER_PREFIX))
local DESC_INDENT = "        "
local NO_ANSWER = "(no answer)"
local DISMISSED = "Dismissed by user"
local EXPAND_HINT = " (+)"
local COLLAPSE_HINT = " (−)"

local QuestionHelpers = {}

-- The questions sit in the tool input right above this result, and inputs
-- outlive outputs through compaction, so only the picked labels are sent.
function QuestionHelpers.format_answers(questions, answers)
  local lines = {}
  for i, q in ipairs(questions) do
    local ans = answers[i] or {}
    local label = (q.header and q.header ~= "") and q.header or ("Q" .. i)
    lines[#lines + 1] = label .. ": " .. (#ans > 0 and table.concat(ans, ", ") or NO_ANSWER)
  end
  return table.concat(lines, "\n")
end

-- A fixed wrap width, not the terminal's: the chat panel re-wraps long lines and
-- maps a click back to the line it came from, so the row -> option map survives
-- a resize. The cap gets a floor, because a card that hides the very picks it
-- exists to show helps nobody.
function QuestionHelpers.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return {
    width = CARD_WIDTH,
    max_lines = math.max((tol and tol.other) or 0, MIN_CARD_LINES),
    keep = "head",
  }
end

local function markdown_lines(text, width)
  local ok, lines = pcall(maki.ui.markdown, text, width)
  if not ok or type(lines) ~= "table" or #lines == 0 then
    return { { { text, "" } } }
  end
  return lines
end

local function prefixed(prefix, style, spans)
  local line = { { prefix, style } }
  table.move(spans, 1, #spans, 2, line)
  return line
end

local function option_desc(options, label)
  for _, opt in ipairs(options) do
    if opt.label == label and opt.description and opt.description ~= "" then
      return opt.description
    end
  end
end

-- Only the picked answers get a row: every row here is permanent scrollback,
-- and the options passed over are spent information. A nil {answers} means the
-- form was dismissed. Returns the lines, plus a row -> option key map covering
-- the rows that toggle a description.
local function card_lines(questions, answers, width, expanded)
  local lines, keys = {}, {}
  local function emit(line)
    lines[#lines + 1] = line
    return #lines
  end

  if not answers then
    emit({ { DISMISSED, "dim" } })
    emit({})
  end

  for i, q in ipairs(questions) do
    for j, md_line in ipairs(markdown_lines(q.question, width)) do
      emit(prefixed(j == 1 and ("Q" .. i .. ". ") or INDENT, "bold", md_line))
    end

    local ans = answers and answers[i] or {}
    if #ans == 0 then
      emit({ { INDENT .. NO_ANSWER, "dim" } })
    end
    for _, text in ipairs(ans) do
      local desc = option_desc(q.options, text)
      local key = i .. "\0" .. text
      for j, piece in ipairs(maki.split(text, "\r?\n")) do
        local line = { { j == 1 and ANSWER_PREFIX or ANSWER_INDENT, "success" }, { piece, "success" } }
        if desc and j == 1 then
          line[#line + 1] = { expanded[key] and COLLAPSE_HINT or EXPAND_HINT, "dim" }
          keys[emit(line)] = key
        else
          emit(line)
        end
      end
      if desc and expanded[key] then
        for _, desc_line in ipairs(markdown_lines(desc, width - #DESC_INDENT)) do
          emit(prefixed(DESC_INDENT, "", desc_line))
        end
      end
    end

    if i < #questions then
      emit({})
    end
  end

  return lines, keys
end

function QuestionHelpers.render_card(questions, answers, opts)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, opts)
  local expanded = {}
  local keys

  local function render()
    local lines
    lines, keys = card_lines(questions, answers, opts.width, expanded)
    view:clear()
    for _, line in ipairs(lines) do
      view:append(line)
    end
    view:finish()
  end

  render()

  -- Clicks outside the card's own rows fall through to the shared expand
  -- toggle, so this tool collapses like every other one.
  buf:on("click", function(ev)
    local key = ev.row <= view:visible_count() and keys[ev.row]
    if key then
      expanded[key] = not expanded[key]
      render()
    else
      view:toggle()
    end
  end)

  return buf
end

return QuestionHelpers
