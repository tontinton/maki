-- task_message reaches a subagent the task tool moved to the background.

local DESCRIPTION =
  [[Message a running background subagent by id, e.g. a new fact, a changed scope or "wrap up now". It reads it after its current step.
Set interrupt only when it must act now: it cuts the current step, running tools included.]]
local ID_DESCRIPTION = "Task id from its receipt"
local MESSAGE_DESCRIPTION = "What the subagent should know or do"
local INTERRUPT_DESCRIPTION = "Cut the current step short"
local QUEUED = "Message queued for %s."
local INTERRUPTED = "Interrupted %s with the message."

maki.api.register_tool({
  name = "task_message",
  description = DESCRIPTION,
  audiences = { "main" },
  needs_background = true,
  schema = {
    type = "object",
    required = { "id", "message" },
    additionalProperties = false,
    properties = {
      id = { type = "string", description = ID_DESCRIPTION },
      message = { type = "string", description = MESSAGE_DESCRIPTION },
      interrupt = { type = "boolean", description = INTERRUPT_DESCRIPTION },
    },
  },
  header = function(input)
    return input.id
  end,
  -- A restore hook that returns nothing would also drop the restored header.
  restore = function()
    return {}
  end,
  handler = function(input, ctx)
    local interrupt = input.interrupt == true
    local sent, err = maki.task.send(ctx, input.id, input.message, { interrupt = interrupt })
    if not sent then
      return { llm_output = err, is_error = true }
    end
    return (interrupt and INTERRUPTED or QUEUED):format(sent.name)
  end,
})
