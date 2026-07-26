-- Watch a long-running command and let the agent hear about it later.
--
-- The job outlives the tool call that started it (owner = "session"), and
-- each interesting line goes to the session mailbox instead of being sent
-- as its own prompt, so a chatty watcher costs nothing until the agent
-- runs again.

local monitors = {}

-- A watcher that floods would quietly undo the point of the mailbox, so
-- each one gets a budget and then goes quiet with one line saying so.
local MAX_LINES = 200

local SCHEMA = {
  type = "object",
  properties = {
    command = {
      type = "string",
      description = "Shell command that prints one line per event, e.g. `tail -f app.log | grep --line-buffered ERROR`.",
    },
    label = {
      type = "string",
      description = "Short name for this monitor, shown with every line it reports.",
    },
    match = {
      type = "string",
      description = "Lua pattern. When set, only matching lines are reported.",
    },
    wake = {
      type = "boolean",
      description = "Interrupt an idle agent instead of waiting for the next turn. Off by default; use it only for events worth stopping for.",
    },
  },
  required = { "command" },
  additionalProperties = false,
}

local function report(entry, line)
  if entry.match and not line:match(entry.match) then
    return
  end
  entry.seen = entry.seen + 1
  if entry.seen > MAX_LINES then
    if not entry.capped then
      entry.capped = true
      maki.session.notify(
        string.format("[%s] stopped reporting after %d lines", entry.label, MAX_LINES),
        { session = entry.session }
      )
    end
    return
  end
  maki.session.notify(string.format("[%s] %s", entry.label, line), { session = entry.session, wake = entry.wake })
end

-- Labels and commands are whatever the model sent, newlines and all,
-- and both get rendered a line per monitor. Left as they came, one entry
-- could draw rows of its own and invent or hide a monitor in a listing
-- someone is reading to decide what to stop.
local function one_line(s)
  return (tostring(s):gsub("%s+", " "))
end

-- One Lua runtime serves every session in the UI, so this table holds
-- other sessions' monitors too and their ids are small integers anyone
-- could land on. A session may only stop its own: answer for someone
-- else's exactly as for one that was never started, so the reply says
-- nothing about what another session is watching.
local function stop(id, session)
  local entry = monitors[id]
  if not entry or entry.session ~= session then
    return false
  end
  maki.fn.jobstop(id)
  monitors[id] = nil
  return true
end

-- The one description of what is running, so the palette and the model
-- never disagree about it. A {session} of nil lists every session's
-- monitors, which is the human's view of the machine and deliberately
-- not the model's. Ids come back out of a hash table in no order, and
-- sorting the rendered lines would put 10 before 2, so sort the ids.
local function listing(session)
  local ids = {}
  for id, entry in pairs(monitors) do
    if session == nil or entry.session == session then
      ids[#ids + 1] = id
    end
  end
  table.sort(ids)

  local lines = {}
  for _, id in ipairs(ids) do
    local entry = monitors[id]
    local line = string.format("%d  %s  %s", id, entry.label, one_line(entry.command))
    if entry.capped then
      line = line .. string.format("  (quiet since %d lines)", MAX_LINES)
    end
    lines[#lines + 1] = line
  end
  return lines
end

-- Every model-facing entry point has to know which session is asking,
-- both to own what it starts and to be kept out of what it did not.
local function caller_session(ctx)
  local session, err = ctx:session_id()
  if err or not session then
    return nil,
      {
        llm_output = "error: a monitor needs a session, and this tool is not running for one",
        is_error = true,
      }
  end
  return session, nil
end

maki.api.register_tool({
  name = "monitor",
  description = "Watch a command in the background and report what it prints. "
    .. "Returns straight away; lines arrive later, with the next turn. "
    .. "Use it for a dev server, a test watcher, or a deploy, when you want "
    .. "to know what happened without asking again. Stop it with monitor_stop.",
  schema = SCHEMA,
  -- Starting a job needs the `run` permission, which a bundled plugin
  -- already has. That covers the plugin, not the command: without a scope
  -- here the model could run through this tool anything the bash tool
  -- would have had to ask about first. Always prompt rather than reuse a
  -- standing grant, because the job outlives the call it was granted to,
  -- and an "always allow" answered for a command that runs once never
  -- agreed to one that keeps running.
  permission_scopes = function(input)
    local command = input.command
    if not command or command:match("^%s*$") then
      return nil
    end
    return { scopes = { command }, force_prompt = true }
  end,
  handler = function(input, ctx)
    local command = input.command
    if not command or command:match("^%s*$") then
      return { llm_output = "error: command must not be blank", is_error = true }
    end

    local session, no_session = caller_session(ctx)
    if no_session then
      return no_session
    end

    local entry = {
      command = command,
      label = input.label,
      match = input.match,
      wake = input.wake or false,
      session = session,
      seen = 0,
    }

    local id, start_err = maki.fn.jobstart(command, {
      owner = "session",
      on_stdout = function(job_id, line)
        local e = monitors[job_id]
        if e then
          report(e, line)
        end
      end,
      on_exit = function(job_id, code)
        local e = monitors[job_id]
        if e then
          monitors[job_id] = nil
          maki.session.notify(string.format("[%s] exited with %d", e.label, code), { session = e.session })
        end
      end,
    })
    if start_err then
      return { llm_output = "error: " .. tostring(start_err), is_error = true }
    end

    -- Naming an unnamed monitor after its own id keeps the two things the
    -- model has to keep together — what it reads in a report and what it
    -- passes to monitor_stop — from drifting apart, and needs no counter
    -- of its own to carry numbering between sessions.
    if entry.label and entry.label ~= "" then
      entry.label = one_line(entry.label)
    else
      entry.label = "monitor " .. id
    end

    monitors[id] = entry
    return string.format("%s watching `%s` (id %d)", entry.label, command, id)
  end,
})

maki.api.register_tool({
  name = "monitor_stop",
  description = "Stop a monitor started with the monitor tool.",
  schema = {
    type = "object",
    properties = { id = { type = "integer", description = "Monitor id." } },
    required = { "id" },
    additionalProperties = false,
  },
  handler = function(input, ctx)
    local session, no_session = caller_session(ctx)
    if no_session then
      return no_session
    end
    if stop(input.id, session) then
      return "stopped monitor " .. input.id
    end
    return {
      llm_output = "error: no monitor with id " .. tostring(input.id) .. "; monitor_list shows the ones still running",
      is_error = true,
    }
  end,
})

-- A monitor is started once and stopped a turn or an hour later, by
-- which point the id it was given may have been compacted away. Without
-- somewhere to look it up the model can start watchers it has no way to
-- call off; `/monitors` shows the same list, but only to a human.
maki.api.register_tool({
  name = "monitor_list",
  description = "List the monitors running in this session, with their ids. "
    .. "Use it to find the id of a monitor you want to stop.",
  schema = { type = "object", properties = {}, additionalProperties = false },
  handler = function(_, ctx)
    local session, no_session = caller_session(ctx)
    if no_session then
      return no_session
    end
    local lines = listing(session)
    if #lines == 0 then
      return "no monitors running"
    end
    return table.concat(lines, "\n")
  end,
})

-- The model is asked before a monitor starts, but the job outlives that
-- one call, so the person who granted it needs a way to see what is
-- still running and to call it off.
maki.api.register_command({
  name = "/monitors",
  description = "List running monitors",
  handler = function()
    local lines = listing(nil)
    if #lines == 0 then
      maki.ui.flash("no monitors running")
      return
    end
    maki.ui.flash(table.concat(lines, "\n"))
  end,
})

-- Unlike `stop`, this cannot tell whose monitors these are: SessionReset
-- does not yet name the session that ended, so one session resetting
-- takes down every session's watchers. Right with one session open and
-- wrong with several; it wants the event to carry the id.
local function stop_all()
  for id in pairs(monitors) do
    maki.fn.jobstop(id)
  end
  monitors = {}
end

maki.api.create_autocmd({ "SessionReset" }, { callback = stop_all })
