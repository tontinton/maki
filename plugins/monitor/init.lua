local MAX_WAIT_MS = 600000

local description = [[Spawn a long-running command you own (a test suite, build, or server).
The command keeps running after this tool returns. You will be notified in this
session when it exits, including on success. A new turn starts when the session
is idle unless you set `wake = false`.

Do not `sleep` or poll. Keep working; the exit observation includes log paths
and a short tail. Use `monitor_wait` only when you have no independent work
left. Use `monitor_peek` to look now without waiting. Use `read` on the log
paths for a post-mortem. Do not `cat` them through bash.

Logs are always written to a per-monitor directory (stdout.log, stderr.log,
meta.json). `monitor_list` shows this session's live and recently-exited
monitors; `monitor_stop` kills one (safe after exit).

The monitor survives plugin reloads.]]

local function parse_session(input)
  if input.session and input.session ~= "" then
    return input.session
  end
  local id, err = maki.session.current()
  if not id then
    return nil, err or "no interactive session"
  end
  return id
end

local function monitor_dir(session, id)
  local root = maki.env.logs_dir()
  if not root then
    return nil
  end
  return maki.fs.joinpath(root, session, "monitor-" .. id)
end

local function stdout_path(session, id)
  local dir = monitor_dir(session, id)
  return dir and maki.fs.joinpath(dir, "stdout.log")
end

local function stderr_path(session, id)
  local dir = monitor_dir(session, id)
  return dir and maki.fs.joinpath(dir, "stderr.log")
end

local function meta_path(session, id)
  local dir = monitor_dir(session, id)
  return dir and maki.fs.joinpath(dir, "meta.json")
end

local function file_size(path)
  if not path then
    return nil
  end
  local meta = maki.fs.metadata(path)
  return meta and meta.size
end

local function write_meta(session, id, fields)
  local path = meta_path(session, id)
  if not path then
    return
  end
  local encoded = maki.json.encode(fields)
  if encoded then
    maki.fs.atomic_write(path, encoded)
  end
end

local function format_paths(session, id)
  local out_path = stdout_path(session, id)
  if not out_path then
    return ""
  end
  return "stdout: " .. out_path .. "\nstderr: " .. stderr_path(session, id) .. "\nmeta: " .. meta_path(session, id)
end

local function format_snapshot(info)
  local header
  if info.status == "running" then
    header = string.format("monitor %d  [%ds]  %s  (pid %d)", info.id, info.elapsed_secs, info.command, info.pid)
  else
    header = string.format(
      "monitor %d exited with code %d after %ds: %s",
      info.id,
      info.exit_code,
      info.elapsed_secs,
      info.command
    )
  end
  local out_path = info.session and stdout_path(info.session, info.id)
  local err_path = info.session and stderr_path(info.session, info.id)
  local mpath = info.session and meta_path(info.session, info.id)
  if out_path then
    header = header .. string.format("\nstdout: %s (%s bytes)", out_path, file_size(out_path) or 0)
  end
  if err_path then
    header = header .. string.format("\nstderr: %s (%s bytes)", err_path, file_size(err_path) or 0)
  end
  if mpath then
    header = header .. "\nmeta: " .. mpath
  end
  local chunks = { header }
  if info.stdout_lines and #info.stdout_lines > 0 then
    chunks[#chunks + 1] = "--- stdout tail ---\n" .. table.concat(info.stdout_lines, "\n")
  end
  if info.stderr_lines and #info.stderr_lines > 0 then
    chunks[#chunks + 1] = "--- stderr tail ---\n" .. table.concat(info.stderr_lines, "\n")
  end
  if #chunks == 1 then
    return header .. "\n(no output captured)"
  end
  return table.concat(chunks, "\n")
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use monitor for a test, build, or server that should outlive the tool call. You will be notified when it exits. Do not bash-sleep or poll; use monitor_wait only when idle, and read the log files for the full output.",
})

maki.api.register_tool({
  name = "monitor",
  kind = "execute",
  description = description,
  schema = {
    type = "object",
    properties = {
      command = {
        type = "string",
        description = "The bash command to supervise (runs detached, outlives this call)",
        required = true,
      },
      cwd = { type = "string", description = "Working directory (default: cwd)" },
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
      session = {
        type = "string",
        description = "Session id to notify on exit. Defaults to the current session.",
      },
      wake = {
        type = "boolean",
        description = "Start a session turn when the process exits and the session is idle (default true). TUI only.",
      },
      notify_on_success = {
        type = "boolean",
        description = "Notify on every exit, including success (default true). Set false to hear only about failures.",
      },
      tail = {
        type = "integer",
        description = "Trailing lines per stream to keep for peek/notify (default 20, 0 disables)",
      },
    },
  },
  permission_scopes = function(input)
    local command = input.command
    if not command or command:match("^%s*$") then
      return nil
    end
    return { scopes = { command }, force_prompt = true }
  end,
  header = function(input)
    local s = input.description or input.command
    local buf = maki.ui.buf()
    buf:line({ { "monitor ", "dim" }, { s } })
    return buf
  end,
  handler = function(input)
    if not input.command or input.command:match("^%s*$") then
      return { llm_output = "error: command is required", is_error = true }
    end

    local session, err = parse_session(input)
    if not session then
      return { llm_output = "error: " .. (err or "no session"), is_error = true }
    end

    local notify_on_success = input.notify_on_success
    if notify_on_success == nil then
      notify_on_success = true
    end
    local wake = input.wake
    if wake == nil then
      wake = true
    end

    local ok, id_or_err = pcall(maki.fn.jobstart, input.command, {
      owner = "session",
      session = session,
      cwd = input.cwd,
      notify = { wake = wake, on_success = notify_on_success },
      tail = input.tail,
      on_stdout = function(job_id, line)
        local path = stdout_path(session, job_id)
        if path then
          maki.fs.append(path, line .. "\n")
        end
      end,
      on_stderr = function(job_id, line)
        local path = stderr_path(session, job_id)
        if path then
          maki.fs.append(path, line .. "\n")
        end
      end,
      on_exit = function(job_id, code)
        write_meta(session, job_id, {
          id = job_id,
          command = input.command,
          cwd = input.cwd,
          session = session,
          exit_code = code,
        })
      end,
    })
    if not ok then
      return { llm_output = "error: " .. tostring(id_or_err), is_error = true }
    end
    local id = id_or_err

    local dir = monitor_dir(session, id)
    if dir then
      maki.fs.mkdir(dir, { parents = true })
      local info = maki.fn.jobinfo(id)
      write_meta(session, id, {
        id = id,
        command = input.command,
        cwd = input.cwd,
        session = session,
        pid = info and info.pid,
        started = os.time(),
      })
    end

    local msg = "monitor " .. id .. " started: " .. input.command
    local paths = format_paths(session, id)
    if paths ~= "" then
      msg = msg .. "\n" .. paths
    end
    msg = msg
      .. "\nYou will be notified when it exits. Do not sleep or poll. Use monitor_wait only when idle. Read the log files for the full output."
    return msg
  end,
})

maki.api.register_tool({
  name = "monitor_stop",
  kind = "execute",
  description = [[Kill a running monitor and its process group.

Pass the monitor id returned by `monitor`. Safe to call after the process has
already exited.]],
  schema = {
    type = "object",
    properties = {
      id = { type = "integer", description = "Monitor id returned by `monitor`", required = true },
    },
  },
  permission_scopes = function(input)
    if input.id then
      local info = maki.fn.jobinfo(input.id)
      if info and info.command then
        return { scopes = { info.command }, force_prompt = true }
      end
    end
    return { scopes = { "monitor_stop" }, force_prompt = false }
  end,
  handler = function(input)
    local info = maki.fn.jobinfo(input.id)
    if not info then
      return { llm_output = "error: monitor not found", is_error = true }
    end
    maki.fn.jobstop(input.id)
    return "monitor " .. input.id .. " stopped"
  end,
})

maki.api.register_tool({
  name = "monitor_list",
  kind = "execute",
  description = [[List this session's live and recently-exited monitors.

Returns each monitor's id, command, pid, status, and how long it ran.]],
  schema = {
    type = "object",
    properties = {
      session = {
        type = "string",
        description = "Session id to list. Defaults to the current session.",
      },
    },
  },
  permission_scopes = function()
    return { scopes = { "monitor_list" }, force_prompt = false }
  end,
  handler = function(input)
    local session, err = parse_session(input or {})
    if not session then
      return { llm_output = "error: " .. (err or "no session"), is_error = true }
    end
    local monitors = maki.fn.joblist(session)
    if #monitors == 0 then
      return "no monitors"
    end
    local lines = {}
    for _, m in ipairs(monitors) do
      if m.status == "running" then
        lines[#lines + 1] =
          string.format("  %d  [%ds]  %s  (pid %d)  session %s", m.id, m.elapsed_secs, m.command, m.pid, m.session)
      else
        lines[#lines + 1] = string.format(
          "  %d  exited %d after %ds  %s  session %s",
          m.id,
          m.exit_code,
          m.elapsed_secs,
          m.command,
          m.session
        )
      end
    end
    return "monitors:\n" .. table.concat(lines, "\n")
  end,
})

maki.api.register_tool({
  name = "monitor_peek",
  kind = "execute",
  description = [[Read a monitor's recent stdout and stderr without waiting for it to exit.

Finished monitors still answer: exit status, tails, and log paths are reported
instead of "not found". For the full output, `read` the log paths.]],
  schema = {
    type = "object",
    properties = {
      id = { type = "integer", description = "Monitor id returned by `monitor`", required = true },
    },
  },
  permission_scopes = function()
    return { scopes = { "monitor_peek" }, force_prompt = false }
  end,
  handler = function(input)
    local info = maki.fn.jobinfo(input.id)
    if not info then
      return { llm_output = "error: not found", is_error = true }
    end
    return format_snapshot(info)
  end,
})

maki.api.register_tool({
  name = "monitor_wait",
  kind = "execute",
  description = [[Wait for a monitor to finish, or return a snapshot when the timeout elapses.

Use this only when you have no independent work left. You will already be
notified on exit, so do not call this in a loop. Timeout does not kill the
process. Default wait is 30s, max 10 minutes. timeout_ms 0 is an immediate peek.]],
  schema = {
    type = "object",
    properties = {
      id = { type = "integer", description = "Monitor id returned by `monitor`", required = true },
      timeout_ms = {
        type = "integer",
        description = "Maximum wait in milliseconds (default 30000, max 600000). 0 returns immediately.",
      },
    },
  },
  permission_scopes = function()
    return { scopes = { "monitor_wait" }, force_prompt = false }
  end,
  handler = function(input)
    local timeout_ms = input.timeout_ms
    if timeout_ms and timeout_ms > MAX_WAIT_MS then
      timeout_ms = MAX_WAIT_MS
    end

    local ok, result = pcall(maki.fn.jobwait, input.id, timeout_ms)
    if not ok then
      return { llm_output = "error: " .. tostring(result), is_error = true }
    end

    local info = maki.fn.jobinfo(input.id)
    if not result then
      if not info then
        return { llm_output = "error: not found", is_error = true }
      end
      return format_snapshot(info) .. "\n(wait timed out, still running)"
    end

    local session = info and info.session
    local command = (info and info.command) or ""
    local header = string.format("monitor %d exited with code %d: %s", input.id, result.exit_code, command)
    if result.truncated then
      header = header .. "\n(reporting from the captured tail; read the log files for full output)"
    end
    local paths = session and format_paths(session, input.id)
    if paths and paths ~= "" then
      header = header .. "\n" .. paths
    end

    local chunks = { header }
    if result.stdout ~= "" then
      chunks[#chunks + 1] = "--- stdout ---\n" .. result.stdout
    end
    if result.stderr ~= "" then
      chunks[#chunks + 1] = "--- stderr ---\n" .. result.stderr
    end
    return table.concat(chunks, "\n")
  end,
})

maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    local session_id = ev.data and ev.data.session_id
    if not session_id then
      return
    end
    local root = maki.env.logs_dir()
    if not root then
      return
    end
    maki.fs.rm(maki.fs.joinpath(root, session_id), { recursive = true, force = true })
  end,
})
