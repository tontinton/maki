local Runner = require("runner")

local CANCELLED = "background task cancelled"
local UNKNOWN_PREFIX = "unknown background task: "
local WAIT_ERROR_PREFIX = "background task result wait "
local MAX_NONCE_WORD = 0x7fffffff
local nonce = string.format(
  "%08x%08x%08x%08x",
  math.random(0, MAX_NONCE_WORD),
  math.random(0, MAX_NONCE_WORD),
  math.random(0, MAX_NONCE_WORD),
  math.random(0, MAX_NONCE_WORD)
)
local next_id = 0
local entries = {}
local Background = {}

local function settle(entry, result, status)
  if entry.result then
    return false
  end
  entry.result, entry.status = result, status
  local waiters = entry.waiters
  entry.waiters = {}
  for _, done in pairs(waiters) do
    done(result)
  end
  return true
end

local function finish(entry, result, status)
  if entries[entry.id] ~= entry or not settle(entry, result, status) then
    return
  end
  entry.task, entry.session = nil, nil
  local _, err = maki.session.notify(
    "Background task " .. entry.id .. " " .. status .. ". Call task_result with this id to read its result.",
    { session = entry.session_id, wake = true }
  )
  if err then
    maki.log.warn("background task notification failed: " .. err)
  end
end

local function release(entry)
  local permit = entry.permit
  entry.permit = nil
  if permit then
    permit:release()
  end
end

local function lookup(input, ctx)
  local entry = entries[input.id]
  if not entry or entry.status == "opening" or entry.session_id ~= ctx:session_id() then
    return nil, { llm_output = UNKNOWN_PREFIX .. input.id, is_error = true }
  end
  return entry
end

function Background.start(prepared, ctx)
  local session_id = ctx:session_id()
  if not session_id then
    return { llm_output = "background task requires a live chat session", is_error = true }
  end
  next_id = next_id + 1
  local entry = {
    id = nonce .. "-" .. next_id,
    session_id = session_id,
    status = "opening",
    waiters = {},
  }
  -- Opening can yield; SessionEnd must own the pending admission too.
  entries[entry.id] = entry
  maki.async.on_cancel(function()
    if entries[entry.id] == entry and entry.status == "opening" then
      entries[entry.id] = nil
    end
  end)
  local opened, session, err = pcall(Runner.open, ctx, prepared, session_id)
  if not opened or err then
    entries[entry.id] = nil
    return { llm_output = opened and err or tostring(session), is_error = true }
  end
  if entries[entry.id] ~= entry then
    session:close()
    return { llm_output = CANCELLED, is_error = true }
  end
  entry.session, entry.status = session, "queued"
  local ok, task = pcall(maki.async.spawn, function()
    maki.async.on_cancel(function()
      release(entry)
    end)
    entry.permit = Runner.semaphore:acquire()
    entry.status = "running"
    local ran, result = pcall(Runner.run, session, prepared)
    local closed, close_err = pcall(session.close, session)
    release(entry)
    if not ran then
      result = { llm_output = tostring(result), is_error = true }
    elseif not closed then
      result = { llm_output = tostring(close_err), is_error = true }
    end
    finish(entry, result, entry.cancel_requested and "cancelled" or (result.is_error and "failed" or "done"))
  end)
  if not ok then
    entries[entry.id] = nil
    session:close()
    return { llm_output = tostring(task), is_error = true }
  end
  entry.task = task
  return { llm_output = maki.json.encode({ id = entry.id, status = entry.status }) }
end

function Background.result(input, ctx)
  local entry, err = lookup(input, ctx)
  if err then
    return err
  end
  if entry.result then
    return entry.result
  end

  local waiter = {}
  maki.async.on_cancel(function(reason)
    waiter.cancelled = reason
    local done = entry.waiters[waiter]
    entry.waiters[waiter] = nil
    if done then
      done({ llm_output = WAIT_ERROR_PREFIX .. reason, is_error = true })
    end
  end)
  return maki.async.await(1, function(done)
    if waiter.cancelled then
      done({ llm_output = WAIT_ERROR_PREFIX .. waiter.cancelled, is_error = true })
    else
      entry.waiters[waiter] = done
    end
  end)
end

function Background.cancel(input, ctx)
  local entry, err = lookup(input, ctx)
  if err then
    return err
  end
  if not entry.result then
    entry.cancel_requested = true
    local session = entry.session
    if entry.status == "queued" then
      entry.task:cancel()
      finish(entry, { llm_output = CANCELLED, is_error = true }, "cancelled")
    end
    session:close()
  end
  return {
    llm_output = maki.json.encode({ id = entry.id, status = entry.result and entry.status or "cancelling" }),
  }
end

maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    local session_id = ev.data and ev.data.session_id
    local ending = {}
    for id, entry in pairs(entries) do
      if entry.session_id == session_id then
        entries[id] = nil
        ending[#ending + 1] = entry
      end
    end
    for _, entry in ipairs(ending) do
      settle(entry, { llm_output = CANCELLED, is_error = true }, "cancelled")
      if entry.task then
        entry.task:cancel()
      end
    end
    for _, entry in ipairs(ending) do
      if entry.session then
        entry.session:close()
      end
    end
  end,
})

return Background
