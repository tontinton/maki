local QuestionForm = require("question_form")
local QuestionHelpers = require("question_helpers")

local DESCRIPTION = [[Ask the user questions during execution to gather preferences, clarify instructions, or get decisions. `custom` is enabled by default; don't add catch-all options. Answers are arrays of labels; use `multiSelect` for multi-select. Put the recommended option first.]]

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
    for _, q in ipairs(input.questions) do
      q.options = q.options or {}
      q.header = q.header or ""
      q.multiple = q.multiSelect or false
    end
    local result = QuestionForm.open(input.questions)
    if result.type == "dismiss" then
      return { llm_output = "(question dismissed by user)" }
    end
    return { llm_output = QuestionHelpers.format_answer_list(input.questions, result.answers), format = "markdown" }
  end,
})
