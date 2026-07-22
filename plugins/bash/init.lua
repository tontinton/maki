local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

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

local function unquote(s)
  local q = s:sub(1, 1)
  if (q == '"' or q == "'") and s:sub(-1) == q then
    return s:sub(2, -2)
  end
  return s
end

local function parse_cd_hint(input)
  if input.workdir then
    return input.command, input.workdir
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

local cwd = maki.uv.cwd() or "."

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

local LEAF_COMMAND_TYPES = {
  command = true,
  redirected_statement = true,
  negated_command = true,
  subshell = true,
  compound_statement = true,
  if_statement = true,
  while_statement = true,
  for_statement = true,
  case_statement = true,
  function_definition = true,
  c_style_for_statement = true,
}

local function collect_commands(node, source)
  local out = {}
  local kind = node:type()
  if kind == "program" or kind == "list" then
    for child in node:iter_children() do
      local nested = collect_commands(child, source)
      for _, cmd in ipairs(nested) do
        out[#out + 1] = cmd
      end
    end
  elseif kind == "pipeline" then
    for child in node:iter_children() do
      if child:named() then
        local text = maki.treesitter.get_node_text(child, source):match("^%s*(.-)%s*$")
        if text ~= "" then
          out[#out + 1] = text
        end
      end
    end
  elseif LEAF_COMMAND_TYPES[kind] then
    local text = maki.treesitter.get_node_text(node, source):match("^%s*(.-)%s*$")
    if text ~= "" then
      out[#out + 1] = text
    end
  end
  return out
end

local description = [[Execute a bash command.
Commands run in ]] .. cwd .. [[ by default.

- **DO NOT** use for file ops! Only git, builds, tests, and system commands.
- Use `workdir` param instead of `cd <dir> && <cmd>` patterns.
- Do NOT use to communicate text to the user.
- Chain dependent commands with `&&`. Use batch for independent ones.
- Provide a short `description` (3-5 words).
- Output truncated beyond 2000 lines or 50KB.
- Interactive commands (sudo, ssh prompts) fail immediately.]]

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
}))

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
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
    },
  },
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
    if input.timeout then
      local buf = maki.ui.buf()
      buf:line({ { s }, { " (" .. maki.ui.humantime(input.timeout) .. " timeout)", "dim" } })
      return buf
    end
    return s
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

    local function finish(exit_code)
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

    maki.fn.jobstart(command, {
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

    return nil
  end,
})
