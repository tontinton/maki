local DESCRIPTION =
  [[Activate a tool that is not currently available. Use this when you need a tool that is not in the current mode's tool set. The tool will become available in the next turn.]]

maki.api.register_tool({
  name = "activate_tool",
  modes = { "default", "research", "build", "compact" },
  description = DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      tool_name = {
        type = "string",
        description = "Name of the tool to activate",
        required = true,
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    buf:line({ { "activate: " .. (input.tool_name or "unknown"), "tool" } })
    return buf
  end,

  handler = function(input, ctx)
    local tool_name = input.tool_name
    if not tool_name then
      return { llm_output = "error: tool_name is required", is_error = true }
    end
    local llm_output = string.format("activated tool: %s (available next turn)", tool_name)
    return { llm_output = llm_output }
  end,
})
