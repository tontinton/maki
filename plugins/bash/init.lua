local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local partial = require("maki.partial")

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

local function unquote(s)
  local q = s:sub(1, 1)
  if (q == '"' or q == "'") and s:sub(-1) == q then
    return s:sub(2, -2)
  end
  return s
end

local function normalize_sep(s)
  return s:gsub("\\", "/")
end

local function relative_path(p)
  local np = normalize_sep(p)
  local cwd = maki.uv.cwd()
  if cwd then
    cwd = normalize_sep(cwd)
    if np:sub(1, #cwd + 1) == cwd .. "/" then
      local rel = np:sub(#cwd + 2)
      return rel == "" and "." or rel
    end
    if np == cwd then
      return "."
    end
  end
  local home = maki.uv.os_homedir()
  if home then
    home = normalize_sep(home)
    if np:sub(1, #home + 1) == home .. "/" then
      local rel = np:sub(#home + 2)
      return rel == "" and "~" or "~/" .. rel
    end
  end
  return p
end

local function build_header_lines(command)
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
  return header
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

local function run_rtk(cmd)
  local id = maki.fn.jobstart(cmd)
  if not id then
    return nil
  end
  local result = maki.fn.jobwait(id, RTK_REWRITE_TIMEOUT_MS)
  if not result then
    maki.fn.jobstop(id)
  end
  return result
end

local function rtk_rewrite(command, ctx)
  local config = ctx:config()
  if config and not config.rtk then
    return nil
  end

  if rtk_available == nil then
    local result = run_rtk({ "rtk", "--version" })
    rtk_available = result ~= nil and result.exit_code == 0
  end

  if not rtk_available then
    return nil
  end

  local cmd = command:match("^%s*(.-)%s*$")
  if cmd:match("^cargo ") and cmd:find(" -- ", 1, true) then
    return nil
  end

  local result = run_rtk({ "rtk", "rewrite", command })
  if not result then
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

local function create_bash_view(command, ctx)
  local tol = ctx:tool_output_lines()
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.bash) or 5,
    keep = "tail",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  })
  view:set_header(build_header_lines(command))
  buf:on("click", function()
    view:toggle()
  end)
  return buf, view
end

local COMPLEX_TYPES = {
  command_substitution = true,
  process_substitution = true,
  subshell = true,
  arithmetic_expansion = true,
}

local function is_complex(node)
  if COMPLEX_TYPES[node:type()] then
    return true
  end
  for child in node:iter_children() do
    if is_complex(child) then
      return true
    end
  end
  return false
end

local REDIRECT_TYPES = {
  file_redirect = true,
  heredoc_redirect = true,
  herestring_redirect = true,
}

-- Nodes we walk through instead of turning into a scope. `redirected_statement`
-- has to be one of them: tree-sitter hangs a trailing `2>&1` off the entire
-- `cd x && cargo test` chain rather than off `cargo test`, so treating it as a
-- leaf turns the whole chain into a single scope starting with `cd `, and a
-- `cd *` allow rule then quietly covers whatever runs after the `&&`.
local WALK_THROUGH_TYPES = {
  program = true,
  list = true,
  pipeline = true,
  redirected_statement = true,
}

local function node_text(node, source)
  return maki.treesitter.get_node_text(node, source):match("^%s*(.-)%s*$")
end

-- Anything we don't walk through becomes one scope, its own text. That covers
-- plain commands and the block forms (`if`, `while`, subshells) we deliberately
-- keep whole, plus any node type we never thought of, which is what we want:
-- an unknown node has to end up in front of the user, not get dropped.
local function collect_commands(node, source)
  if not WALK_THROUGH_TYPES[node:type()] then
    local text = node_text(node, source)
    return text ~= "" and { text } or {}
  end

  local out, redirects = {}, {}
  for child in node:iter_children() do
    local kind = child:type()
    if child:named() and kind ~= "comment" then
      if REDIRECT_TYPES[kind] then
        redirects[#redirects + 1] = node_text(child, source)
      else
        for _, cmd in ipairs(collect_commands(child, source)) do
          out[#out + 1] = cmd
        end
      end
    end
  end

  -- The redirect belongs to the last command of the chain, the one bash would
  -- actually apply it to. A bodiless `> log` has no such command and still
  -- truncates the file, so it becomes a scope of its own instead of vanishing.
  if #redirects > 0 then
    local text = table.concat(redirects, " ")
    if #out > 0 then
      out[#out] = out[#out] .. " " .. text
    else
      out[1] = text
    end
  end
  return out
end

local description = [[Execute a bash command.
Commands run in the session's working directory (see Environment) by default.

- **DO NOT** use for file ops! Only git, builds, tests, and system commands.
- Use `workdir` param instead of `cd <dir> && <cmd>` patterns.
- Do NOT use to communicate text to the user.
- Chain dependent commands with `&&`. Use batch for independent ones.
- Provide a short `description` (3-5 words).
- Output truncated beyond 2000 lines or 50KB.
- Interactive commands (sudo, ssh prompts) fail immediately.
- Use the `tail` param, not `| tail`: piping hides live output.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Reserve bash for system commands (git, builds, tests). Do NOT use bash for file operations, including on files outside the working dir.",
})

local opts = maki.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 120,
    min = 5,
    desc = "Kill the command after this many seconds. A call's `timeout` param overrides it.",
  },
  git_bash_path = {
    default = "",
    type = "string",
    desc = "Windows only. Git Bash install to run string commands through. Empty probes the standard install locations, and cmd.exe is the fallback when none exists.",
  },
}))

-- PATH's bash.exe is often the WSL stub, so probe the standard Git for
-- Windows install locations; nonstandard installs set `git_bash_path`.
-- When none is found, commands fall back to cmd.exe.
local IS_WINDOWS = maki.fn.os_is_windows()
local resolve_git_bash

if IS_WINDOWS then
  local probed = false
  local git_bash
  local BASH_CANDIDATES = {
    "C:/Program Files/Git/bin/bash.exe",
    "C:/Program Files (x86)/Git/bin/bash.exe",
  }

  local function find_git_bash()
    local configured = opts.git_bash_path
    if configured ~= "" and maki.fn.executable(configured) == 1 then
      return configured
    end
    for _, candidate in ipairs(BASH_CANDIDATES) do
      if maki.fn.executable(candidate) == 1 then
        if configured ~= "" then
          maki.log.warn(("git_bash_path %q is not executable, using %s instead"):format(configured, candidate))
        end
        return candidate
      end
    end
    if configured ~= "" then
      local msg = "git_bash_path %q is not executable and no standard Git Bash found, falling back to cmd.exe"
      maki.log.warn(msg:format(configured))
    else
      maki.log.warn("no Git Bash found, falling back to cmd.exe")
    end
    return nil
  end

  resolve_git_bash = function()
    if not probed then
      probed = true
      git_bash = find_git_bash()
    end
    return git_bash
  end
else
  resolve_git_bash = function()
    return nil
  end
end

local function shell_argv(command)
  local bash_exe = resolve_git_bash()
  if bash_exe then
    return { bash_exe, "-c", command }
  end
  return command
end

-- With Git Bash the `cd` stays in the command and bash resolves the path
-- itself; extracting the hint would push a possibly-POSIX dir through the
-- native cwd option. Everywhere else the leading `cd dir &&` is lifted out
-- into the cwd option.
local function parse_cd_hint(input)
  if input.workdir then
    return input.command, input.workdir
  end
  if resolve_git_bash() then
    return input.command, nil
  end
  local rest = input.command:match("^cd%s+(.+)$")
  if rest then
    local dir, tail = rest:match("^(.-)%s+&&%s+(.+)$")
    if dir and dir ~= "" then
      return tail, unquote(dir)
    end
  end
  return input.command, nil
end

maki.api.register_tool({
  name = "bash",
  kind = "execute",
  description = description,
  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "The bash command to execute", required = true },
      timeout = { type = "integer", description = "Timeout in seconds (default 120)" },
      workdir = { type = "string", description = "Working directory (default: cwd)" },
      tail = { type = "integer", description = "Return only the last N lines" },
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
    },
  },
  permission = "run",
  permission_scopes = function(input)
    local command = input.command
    if not command or command:match("^%s*$") then
      return nil
    end

    local parser = maki.treesitter.get_parser(command, "bash")
    if not parser then
      return { scopes = { command }, force_prompt = true }
    end

    local root = parser:parse()[1]:root()
    if root:has_error() or is_complex(root) then
      return { scopes = { command }, force_prompt = true }
    end

    local segments = collect_commands(root, command)
    if #segments == 0 then
      segments = { command }
    end
    return { scopes = segments, force_prompt = false }
  end,

  header = function(input)
    local command, workdir = parse_cd_hint(input)
    local s = input.description or command
    if workdir then
      s = s .. " in " .. relative_path(workdir)
    end
    local hints = {}
    if input.timeout then
      hints[#hints + 1] = maki.ui.humantime(input.timeout) .. " timeout"
    end
    if input.tail then
      hints[#hints + 1] = "tail " .. input.tail
    end
    if #hints == 0 then
      return s
    end
    local buf = maki.ui.buf()
    buf:line({ { s }, { " (" .. table.concat(hints, ", ") .. ")", "dim" } })
    return buf
  end,

  restore = function(input, output, is_error, ctx)
    local command = input.command
    local buf, view = create_bash_view(command, ctx)
    local timeout_secs = output:match("^tool bash timed out after (%d+)s$")
    if timeout_secs then
      view:append({ { "Timed out after " .. timeout_secs .. "s", "dim" } })
    elseif is_error then
      local body, code = output:match("^(.-)\nExit code: (%d+)$")
      if body then
        view:append_text(body)
        view:append({ { "Exit code: " .. code, "dim" } })
      else
        view:append_text(output)
      end
    else
      if output == "Exit code: 0" or output == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      else
        view:append_text(output)
      end
    end
    view:finish()
    return buf
  end,

  handler = function(input, ctx)
    if not input.command then
      return { llm_output = "error: command is required", is_error = true }
    end

    if input.tail and input.tail < 1 then
      return { llm_output = "error: tail must be >= 1", is_error = true }
    end

    local command, workdir = parse_cd_hint(input)
    local timeout_secs = input.timeout or opts.timeout_secs
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    ctx:set_deadline(timeout_secs)

    local rewritten = rtk_rewrite(command, ctx)
    if rewritten then
      command = rewritten
    end

    local buf, view = create_bash_view(command, ctx)

    local output_parts = {}
    local has_output = false
    local finished = false

    -- The cut happens on the accumulated text, after the view already streamed
    -- every line, so the user still sees the whole run while the model gets
    -- only the tail. Both the exit and the cancel path go through here.
    local function final_output()
      local output = table.concat(output_parts)
      if input.tail then
        output = output_limits.tail(output, input.tail)
      end
      return truncate(output, max_lines, max_bytes)
    end

    local function finish(exit_code)
      if finished then
        return
      end
      finished = true
      local output = final_output()

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

    local job, err = maki.fn.jobstart(shell_argv(command), {
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
    if not job then
      return { llm_output = "error: " .. err, is_error = true }
    end

    -- Esc or deadline: hand back the lines streamed so far, so the model
    -- keeps what the user just watched instead of a bare error.
    maki.async.on_cancel(function(reason)
      if finished then
        return
      end
      finished = true
      ctx:finish(partial.cut(view, final_output(), reason, timeout_secs))
    end)

    return nil
  end,
})
