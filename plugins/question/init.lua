local QuestionForm = require("question_form")
local QuestionHelpers = require("question_helpers")

local DESCRIPTION = [[Use this tool when you need to ask the user questions during execution. This allows you to:
- Gather user preferences or requirements
- Clarify ambiguous instructions
- Get decisions on implementation choices as you work
- Offer choices to the user about what direction to take

Rules:
- `custom` enabled by default adds "Type your own answer" - don't include catch-all options.
- Answers returned as arrays of labels. Set `multiple: true` for multi-select.
- Put recommended option first with "(Recommended)" suffix.]]

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
            multiple = {
              type = "boolean",
              description = "Whether multiple options can be selected",
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
      return "error: at least one question is required"
    end
    for _, q in ipairs(input.questions) do
      q.options = q.options or {}
      q.header = q.header or ""
      q.multiple = q.multiple or false
    end
    local result = QuestionForm.open(input.questions)
    if result.type == "dismiss" then
      return { llm_output = "(question dismissed by user)" }
    end
    return { llm_output = QuestionHelpers.format_answer_table(input.questions, result.answers), format = "markdown" }
  end,
})
