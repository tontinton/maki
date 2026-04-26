local truncate = require("truncate")
local ToolView = require("tool_view")

local RTK_REWRITE_TIMEOUT_MS = 2000
local RTK_UNSUPPORTED_FLAGS = {
  " -o ",
  " -not ",
  " ! ",
  " -exec ",
  " -execdir ",
  " -print0",
  " -delete",
  " -ok ",
  " -okdir ",
  " -fprint",
  " -fls ",
  " -fprintf ",
}
local SEPARATOR = "──────"

local rtk_available

local function shell_quote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

local function resolve_command(input)
  if input.workdir then
    return input.command, input.workdir
  end
  local rest = input.command:match("^cd%s+(.+)$")
  if rest then
    local dir, tail = rest:match("^(.-)%s+&&%s+(.+)$")
    if dir and dir ~= "" then
      return tail, dir
    end
  end
  return input.command, nil
end

local function relative_path(p)
  local cwd = maki.uv.cwd()
  if cwd and p:sub(1, #cwd + 1) == cwd .. "/" then
    local rel = p:sub(#cwd + 2)
    return rel == "" and "." or rel
  end
  if cwd and p == cwd then
    return "."
  end
  local home = maki.uv.os_homedir()
  if home and p:sub(1, #home + 1) == home .. "/" then
    local rel = p:sub(#home + 2)
    return rel == "" and "~" or "~/" .. rel
  end
  return p
end

local function rtk_find_unsupported(cmd)
  if not cmd:match("^rtk find ") then
    return false
  end
  for _, flag in ipairs(RTK_UNSUPPORTED_FLAGS) do
    if cmd:find(flag, 1, true) then
      return true
    end
  end
  return false
end

local function rtk_rewrite(command, ctx)
  local config = ctx:config()
  if config and config.no_rtk then
    return nil
  end

  if rtk_available == nil then
    local id = maki.fn.jobstart("rtk --version")
    local result = maki.fn.jobwait(id, RTK_REWRITE_TIMEOUT_MS)
    if result then
      rtk_available = (result.exit_code == 0)
    else
      maki.fn.jobstop(id)
      rtk_available = false
    end
  end

  if not rtk_available then
    return nil
  end

  local cmd = command:match("^%s*(.-)%s*$")
  if cmd:match("^cargo ") and cmd:find(" -- ", 1, true) then
    return nil
  end

  local id = maki.fn.jobstart("rtk rewrite " .. shell_quote(command))
  local result = maki.fn.jobwait(id, RTK_REWRITE_TIMEOUT_MS)
  if not result then
    maki.fn.jobstop(id)
    return nil
  end

  if result.exit_code ~= 0 and result.exit_code ~= 3 then
    return nil
  end

  local rewritten = (result.stdout or ""):match("^%s*(.-)%s*$")
  if rewritten == "" or rewritten == command:match("^%s*(.-)%s*$") then
    return nil
  end
  if rtk_find_unsupported(rewritten) then
    return nil
  end
  return rewritten
end

local function append_line(output, line)
  if #output > 0 then
    output[#output + 1] = "\n"
  end
  output[#output + 1] = line
end

local cwd = maki.uv.cwd() or "."
local description = [[Execute a bash command.
Commands run in ]] .. cwd .. [[ by default.

- **DO NOT** use for file ops! Only git, builds, tests, and system commands.
- Use `workdir` param instead of `cd <dir> && <cmd>` patterns.
- Do NOT use to communicate text to the user.
- Chain dependent commands with `&&`. Use batch for independent ones.
- Provide a short `description` (3-5 words).
- Output truncated beyond 2000 lines or 50KB.
- Interactive commands (sudo, ssh prompts) fail immediately.]]

maki.api.register_tool({
  name = "bash",
  description = description,
  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "The bash command to execute", required = true },
      timeout = { type = "integer", description = "Timeout in seconds (default 120)" },
      workdir = { type = "string", description = "Working directory (default: cwd)" },
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
    },
  },
  permission_scope = "command",

  header = function(input)
    local command, workdir = resolve_command(input)
    local s = input.description or command
    if workdir then
      s = s .. " in " .. relative_path(workdir)
    end
    return s
  end,

  handler = function(input, ctx)
    if not input.command then
      return { llm_output = "error: command is required", is_error = true }
    end

    local command, workdir = resolve_command(input)
    local config = ctx:config()
    local timeout_secs = input.timeout or (config and config.bash_timeout_secs) or 120
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)

    local rewritten = rtk_rewrite(command, ctx)
    if rewritten then
      command = rewritten
    end

    local tol = ctx:tool_output_lines()
    local buf = maki.ui.buf()
    local view = ToolView.new(buf, {
      max_lines = (tol and tol.bash) or 5,
      keep = "tail",
    })

    local header = {}
    local highlighted = maki.ui.highlight(command, "bash")
    if highlighted then
      for _, line in ipairs(highlighted) do
        header[#header + 1] = line
      end
    else
      header[#header + 1] = command
    end
    header[#header + 1] = { { SEPARATOR, "dim" } }
    view:set_header(header)

    local output_parts = {}
    local has_output = false
    local finished = false

    local function finish(exit_code)
      if finished then
        return
      end
      finished = true

      local output = table.concat(output_parts)
      output = truncate(output, max_lines, max_bytes)

      local is_error = exit_code ~= 0
      local llm_output
      if exit_code == 0 then
        llm_output = output == "" and "Exit code: 0" or output
      else
        if output == "" then
          llm_output = "Exit code: " .. exit_code
        else
          llm_output = output .. "\nExit code: " .. exit_code
        end
      end

      if output == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      end

      if is_error then
        view:append({ { "Exit code: " .. exit_code, "dim" } })
      end
      view:finish()

      ctx:finish({ llm_output = llm_output, is_error = is_error, body = buf })
    end

    view:append({ { "Waiting for output...", "dim" } })

    local id = maki.fn.jobstart(command, {
      cwd = workdir,
      env = { GIT_TERMINAL_PROMPT = "0" },
      on_stdout = function(_, line)
        if not has_output then
          has_output = true
          view:clear()
        end
        append_line(output_parts, line)
        view:append(line)
      end,
      on_stderr = function(_, line)
        if not has_output then
          has_output = true
          view:clear()
        end
        append_line(output_parts, line)
        view:append(line)
      end,
      on_exit = function(_, code)
        finish(code)
      end,
    })

    maki.defer_fn(function()
      if not finished then
        maki.fn.jobstop(id)
        finished = true
        local output = table.concat(output_parts)
        output = truncate(output, max_lines, max_bytes)
        local msg = "command timed out after " .. timeout_secs .. "s"
        if output ~= "" then
          msg = msg .. "\n" .. output
        end
        view:append({ { "Timed out after " .. timeout_secs .. "s", "dim" } })
        view:finish()
        ctx:finish({
          llm_output = msg,
          is_error = true,
          body = buf,
        })
      end
    end, timeout_secs * 1000)

    return nil
  end,
})
