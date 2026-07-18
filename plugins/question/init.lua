local QuestionForm = require("question_form")
local QuestionHelpers = require("question_helpers")
local ToolView = require("maki.tool_view")

local DESCRIPTION = [[Use this tool when you need to ask the user questions during execution. This allows you to:
- Gather user preferences or requirements
- Clarify ambiguous instructions
- Get decisions on implementation choices as you work
- Offer choices to the user about what direction to take

Rules:
- `custom` enabled by default adds "Type your own answer" - don't include catch-all options.
- Answers returned as arrays of labels. Set `multiSelect: true` for multi-select.
- Put recommended option first with "(Recommended)" suffix.]]

local function card_width()
  local ok, size = pcall(maki.ui.terminal_size)
  if ok and type(size) == "table" and type(size.cols) == "number" then
    return math.max(40, size.cols - 8)
  end
  return 80
end

local function normalize_questions(questions)
  for _, q in ipairs(questions or {}) do
    q.options = q.options or {}
    q.header = q.header or ""
    q.multiple = q.multiSelect or false
  end
end

maki.api.register_tool({
  name = "question",
  description = DESCRIPTION,
  schema = {
    type = "object",
    required = { "questions" },
    properties = {
      questions = {
        type = "array",
        description = "List of questions to ask the user",
        items = {
          type = "object",
          required = { "question" },
          properties = {
            question = { type = "string", description = "The question text" },
            header = { type = "string", description = "Short tab header for the question" },
            options = {
              type = "array",
              description = "List of predefined options",
              items = {
                type = "object",
                required = { "label" },
                properties = {
                  label = { type = "string", description = "Option label" },
                  description = { type = "string", description = "Option description" },
                },
              },
            },
            multiSelect = {
              type = "boolean",
              description = "Whether multiple options can be selected",
              alias = "multiple",
            },
          },
        },
      },
    },
  },
  audiences = { "main" },
  timeout = false,
  header = function(input)
    local n = #input.questions
    return n .. " question" .. (n == 1 and "" or "s")
  end,
  handler = function(input, ctx)
    if #input.questions == 0 then
      return { llm_output = "error: at least one question is required", is_error = true }
    end
    normalize_questions(input.questions)
    local result = QuestionForm.open(input.questions)
    local width = card_width()
    if result.type == "dismiss" then
      return {
        llm_output = "(question dismissed by user)",
        state = { dismissed = true },
        body = QuestionHelpers.render_card(input.questions, {}, { width = width, dismissed = true }),
      }
    end
    return {
      llm_output = QuestionHelpers.format_answer_list(input.questions, result.answers),
      format = "markdown",
      state = { answers = result.answers },
      body = QuestionHelpers.render_card(input.questions, result.answers, { width = width }),
    }
  end,
  restore = function(input, output, _is_error, ctx)
    normalize_questions(input.questions)
    local state = ctx:state()
    local width = card_width()
    if state and state.answers then
      return { body = QuestionHelpers.render_card(input.questions, state.answers, { width = width }) }
    end
    if state and state.dismissed then
      return { body = QuestionHelpers.render_card(input.questions, {}, { width = width, dismissed = true }) }
    end
    return { body = ToolView.restore(output, { max_lines = 80 }) }
  end,
})
