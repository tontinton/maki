-- task_stop lives next to the /tasks picker: both act on tasks the user can
-- see in the task tab, and the host's maki.task.stop does the work.

local DESCRIPTION = "Stop a background task by id."
local ID_DESCRIPTION = "Task id from its receipt"
-- What the model reads when the final reply is not in yet.
local STOP_REQUESTED_REPLY = "Stop requested. Its output arrives later and will not wake you, so do not wait for it."

local function header_buf(name)
  local buf = maki.ui.buf()
  buf:line({ { name } })
  return buf
end

-- The model stops a task by id, but the user knows it by name, so the row
-- shows the name. A handler's header is not saved with the session, so the
-- name also goes in the state for a restore to show. A refused stop keeps
-- the id.
local function reply(stopped)
  return {
    llm_output = stopped.output or STOP_REQUESTED_REPLY,
    header = header_buf(stopped.name),
    state = { name = stopped.name },
  }
end

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
  restore = function(_, _, _, ctx)
    local state = ctx:state()
    return state and state.name and { header = header_buf(state.name) } or {}
  end,
  handler = function(input, ctx)
    local stopped, err = maki.task.stop(ctx, input.id)
    if not stopped then
      return { llm_output = err, is_error = true }
    end
    return reply(stopped)
  end,
})
