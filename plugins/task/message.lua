-- task_message reaches a subagent the task tool moved to the background. The
-- task tool tracks its moved calls here, since only it holds their sessions.

local DESCRIPTION =
  'Message a running background subagent by id, e.g. a new fact, a changed scope or "wrap up now". It reads it after its current step.'
local ID_DESCRIPTION = "Task id from its receipt"
local MESSAGE_DESCRIPTION = "What the subagent should know or do"
local NOT_RUNNING = "no running background subagent with id "
local QUEUED = "Message queued for %s."

local moved = {}
local Message = {}

function Message.track(id, chat, name)
  local task = { chat = chat, name = name, pending = {} }
  moved[id] = task
  return task
end

-- A subagent still waiting for a slot has no session yet, so its messages
-- wait here and ride with its first prompt.
function Message.attach(task, sess)
  task.sess = sess
  for _, message in ipairs(task.pending) do
    sess:send(message)
  end
  task.pending = nil
end

function Message.untrack(id)
  moved[id] = nil
end

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
    local task = moved[input.id]
    if not task or task.chat ~= ctx:session_id() then
      return { llm_output = NOT_RUNNING .. input.id, is_error = true }
    end
    if not task.sess then
      table.insert(task.pending, input.message)
      return QUEUED:format(task.name)
    end
    -- A stopped task stays tracked here until its handler unwinds, and
    -- `send` already refuses it then. Answering the same as after the
    -- untrack means the reply does not depend on that timing.
    if not task.sess:send(input.message) then
      return { llm_output = NOT_RUNNING .. input.id, is_error = true }
    end
    return QUEUED:format(task.name)
  end,
})

return Message
