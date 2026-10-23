-- A background task lives in `entries` from the moment its session starts
-- opening. Once open it gets a session and a worker task, and once the worker
-- gets a concurrency slot it also holds a permit. It leaves exactly once:
-- through `finish` with one notice, or without a notice when its chat ends.
--
-- A user stop reaches a running task as an error from its own prompt, so it
-- keeps its partial output. A queued task has no prompt to fail, so the
-- TaskStopped event finishes it instead.

local Runner = require("runner")

local NOT_LIVE_PREFIX = "session not live: "
local CANCELLED = "background task cancelled"
local STOPPED = { llm_output = "sub-agent error: stopped by user", is_error = true }

local entries = {}
local Background = {}

local function release(entry)
  if entry.permit then
    entry.permit:release()
    entry.permit = nil
  end
end

-- The host drops a session from this list when the user stops it.
local function registered(entry)
  local id = entry.session:id()
  for _, session in ipairs(maki.agent.list({ session = entry.chat }) or {}) do
    if session.id == id then
      return true
    end
  end
  return false
end

local function finish(entry, result, wake)
  if not entries[entry] then
    return
  end
  entries[entry] = nil
  local status = result.is_error and "failed" or "finished"
  local _, err = maki.session.notify(
    "Background task " .. entry.name .. " " .. status .. ":\n\n" .. result.llm_output,
    { session = entry.chat, wake = wake }
  )
  if err then
    maki.log.warn("background task notification failed: " .. err)
  end
end

local function work(entry, prepared)
  -- Hooks must not yield, so closing the session is left to whoever cancels us.
  maki.async.on_cancel(function()
    release(entry)
  end)
  entry.permit = Runner.semaphore:acquire()
  local ran, result = pcall(Runner.run, entry.session, prepared)
  if not ran then
    result = { llm_output = tostring(result), is_error = true }
  end
  -- A task the user stopped still reports back, it just does not wake the chat.
  local wake = registered(entry)
  -- Keep the permit until the session is closed, so TaskStopped never takes
  -- this task for a queued one.
  entry.session:close()
  release(entry)
  finish(entry, result, wake)
end

-- Returns nil when there is no live chat to deliver to, and the caller runs
-- the task blocking instead.
function Background.start(prepared, ctx)
  local chat = ctx:session_id()
  if not chat then
    return nil
  end
  local entry = { chat = chat, name = prepared.options.name }
  entries[entry] = true
  -- Once the session is open the task no longer belongs to this tool call.
  maki.async.on_cancel(function()
    if not entry.session then
      entries[entry] = nil
    end
  end)
  -- The blocking fallback opens with a ctx, which ignores these two.
  prepared.options.session, prepared.options.scope = chat, "session"
  local session, err = maki.agent.session(prepared.options)
  if err then
    entries[entry] = nil
    if err == NOT_LIVE_PREFIX .. chat then
      return nil
    end
    return { llm_output = err, is_error = true }
  end
  -- Opening yields, and the tool call or the chat may have ended meanwhile.
  if not entries[entry] then
    session:close()
    return { llm_output = CANCELLED, is_error = true }
  end
  entry.session = session
  entry.task = maki.async.spawn(function()
    work(entry, prepared)
  end)
  return {
    llm_output = "Background task started: " .. entry.name .. ". The result will be delivered when it finishes.",
  }
end

maki.api.create_autocmd("TaskStopped", {
  callback = function(ev)
    for entry in pairs(entries) do
      if entry.session and entry.session:id() == ev.data.id then
        -- Any plugin can fire TaskStopped, so the host's list has the final word.
        if not entry.permit and not registered(entry) then
          finish(entry, STOPPED, false)
          entry.task:cancel()
          entry.session:close()
        end
        return
      end
    end
  end,
})

maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    -- Cancel every worker before the first close yields, or a slot freed by
    -- one could start a sibling that is about to be stopped.
    local closing = {}
    for entry in pairs(entries) do
      if entry.chat == ev.data.session_id then
        entries[entry] = nil
        if entry.session then
          entry.task:cancel()
          closing[#closing + 1] = entry.session
        end
      end
    end
    for _, session in ipairs(closing) do
      session:close()
    end
  end,
})

return Background
