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

local function normalize(questions)
  questions = questions or {}
  for _, q in ipairs(questions) do
    q.options = q.options or {}
    q.header = q.header or ""
    q.multiple = q.multiSelect or false
  end
  return questions
end

-- The form leaves a hole for every skipped question, and a sparse array
-- round-trips through JSON as an object, which restore cannot index.
local function dense(questions, answers)
  local out = {}
  for i = 1, #questions do
    out[i] = answers[i] or {}
  end
  return out
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
    local questions = normalize(input.questions)
    if #questions == 0 then
      return { llm_output = "error: at least one question is required", is_error = true }
    end
    local result = QuestionForm.open(questions)
    local opts = QuestionHelpers.view_opts(ctx)
    if result.type == "dismiss" then
      return {
        llm_output = "(question dismissed by user)",
        state = { dismissed = true },
        body = QuestionHelpers.render_card(questions, nil, opts),
      }
    end
    local answers = dense(questions, result.answers)
    return {
      llm_output = QuestionHelpers.format_answers(questions, answers),
      state = { answers = answers },
      body = QuestionHelpers.render_card(questions, answers, opts),
    }
  end,
  -- No state means a session older than the card: its output is the markdown
  -- list the tool used to return.
  restore = function(input, output, is_error, ctx)
    local opts = QuestionHelpers.view_opts(ctx)
    local state = ctx:state()
    if not state then
      return ToolView.restore_markdown(output, is_error, opts)
    end
    return QuestionHelpers.render_card(normalize(input.questions), state.answers, opts)
  end,
})
