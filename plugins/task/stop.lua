-- task_stop lives next to the /tasks picker: both act on tasks the user can
-- see in the task tab, and the host's maki.task.stop does the work.

local DESCRIPTION = "Stop a background task (a moved command or subagent) by id."
local ID_DESCRIPTION = "Id of the background task, as named in its receipt"
-- The host answers with this exact text when the final reply is not in yet.
local STOP_REQUESTED = "stop requested"
local STOP_REQUESTED_REPLY =
  "Stop requested; the task has not ended yet. Its output comes later and will not wake you, so do not wait for it."

maki.api.register_tool({
  name = "task_stop",
  description = DESCRIPTION,
  kind = "execute",
  audiences = { "main" },
  needs_background = true,
  schema = {
    type = "object",
    required = { "id" },
    properties = {
      id = { type = "string", description = ID_DESCRIPTION },
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
    local output, err = maki.task.stop(ctx, input.id)
    if not output then
      return { llm_output = err, is_error = true }
    end
    if output == STOP_REQUESTED then
      return STOP_REQUESTED_REPLY
    end
    return output
  end,
})
