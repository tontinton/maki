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
  if config and not config.rtk then
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

-- Block forms: kept as one scope, plus their inner commands as scopes, so a
-- deny rule reaches inside and an allow on one can't claim the block.
local BLOCK_TYPES = {
  if_statement = true,
  for_statement = true,
  while_statement = true,
  c_style_for_statement = true,
  case_statement = true,
  compound_statement = true,
}

-- The smallest command nodes a rule can be about.
local ATOMIC_COMMAND_TYPES = {
  command = true,
  negated_command = true,
  test_command = true,
  declaration_command = true,
  unset_command = true,
}

local function node_text(node, source)
  return maki.treesitter.get_node_text(node, source):match("^%s*(.-)%s*$")
end

-- `time`/`nohup`/`env`/`exec`/`stdbuf` wrap a command, so scope what runs.
-- Tried one at a time: the LuaU runtime we ship never matches `|` alternation.
local PREFIX_WORDS = { "time", "nohup", "env", "exec" }

-- stdbuf's own flags (`-o0`, ...) are not part of the command it runs.
local function strip_flags(text)
  while true do
    local rest = text:match("^%-%S+%s+(.+)$")
    if rest then
      text = rest
    else
      return text
    end
  end
end

local function unwrap_prefixes(text)
  while true do
    local rest
    for _, p in ipairs(PREFIX_WORDS) do
      rest = text:match("^" .. p .. "%s+(.+)$")
      if rest then
        break
      end
    end
    if rest then
      text = rest
    else
      local body = text:match("^stdbuf%s+(.+)$")
      if body then
        text = strip_flags(body)
      else
        return text
      end
    end
  end
end

-- The scope for a command leaf. `!` only inverts the exit status, so scope
-- the wrapped command, not the negation.
local function command_scope(node, source)
  local kind = node:type()
  if kind == "negated_command" then
    for child in node:iter_children() do
      if child:named() and child:type() == "command" then
        local inner = node_text(child, source)
        if inner ~= "" then
          return unwrap_prefixes(inner)
        end
      end
    end
  end
  local text = node_text(node, source)
  if text == "" then
    return nil
  end
  return kind == "command" and unwrap_prefixes(text) or text
end

-- tree-sitter's `!` only negates a simple command; before a compound statement
-- it mis-parses into garbage scopes. `!` only inverts the exit status, so drop
-- any `!` that negates a compound statement, wherever it begins a pipeline
-- (not just the leading one). A `!` before a simple command is left alone (it
-- parses as a `negated_command`), and so is a `!` inside quotes or one that
-- does not start a pipeline (e.g. an argument like `echo ! if`).
local COMPOUND_KEYWORDS = { "{", "if", "while", "until", "for", "case" }
local PIPELINE_START_KEYWORDS = { "then", "do", "else", "elif" }

local function at_pipeline_start(command, i)
  local j = i - 1
  while j >= 1 and command:sub(j, j):match("%s") do
    j = j - 1
  end
  if j < 1 then
    return true
  end
  local c = command:sub(j, j)
  if c == ";" or c == "|" or c == "&" or c == "(" or c == "{" then
    return true
  end
  for _, kw in ipairs(PIPELINE_START_KEYWORDS) do
    local start = j - #kw + 1
    if start >= 1 and command:sub(start, j) == kw then
      local before = start > 1 and command:sub(start - 1, start - 1) or ""
      if before == "" or before:match("%s") then
        return true
      end
    end
  end
  return false
end

local function strip_compound_negations(command)
  local out = {}
  local n = #command
  local i = 1
  while i <= n do
    local c = command:sub(i, i)
    if c == "'" then
      out[#out + 1] = c
      i = i + 1
      while i <= n and command:sub(i, i) ~= "'" do
        out[#out + 1] = command:sub(i, i)
        i = i + 1
      end
      if i <= n then
        out[#out + 1] = command:sub(i, i)
        i = i + 1
      end
    elseif c == '"' then
      out[#out + 1] = c
      i = i + 1
      while i <= n do
        local q = command:sub(i, i)
        out[#out + 1] = q
        if q == "\\" and i < n then
          out[#out + 1] = command:sub(i + 1, i + 1)
          i = i + 2
        else
          i = i + 1
          if q == '"' then
            break
          end
        end
      end
    elseif c == "!" then
      local rest = command:sub(i + 1)
      local ws, after_ws = rest:match("^(%s+)(.*)$")
      local negates = false
      if ws and at_pipeline_start(command, i) then
        for _, kw in ipairs(COMPOUND_KEYWORDS) do
          if after_ws:sub(1, #kw) == kw then
            local after = after_ws:sub(#kw + 1, #kw + 1)
            if after == "" or after:match("^%s$") or after == ";" or after == "(" then
              negates = true
              break
            end
          end
        end
      end
      if negates then
        i = i + 1 + #ws
      else
        out[#out + 1] = c
        i = i + 1
      end
    else
      out[#out + 1] = c
      i = i + 1
    end
  end
  return table.concat(out)
end

-- Redirects attach to the last command of the chain, the one bash would
-- actually apply it to. A bodiless `> log` has no such command and becomes a
-- scope of its own instead of vanishing: it still truncates the file.
local function attach_redirects(out, redirects)
  if #redirects == 0 then
    return
  end
  local text = table.concat(redirects, " ")
  if #out > 0 then
    out[#out] = out[#out] .. " " .. text
  else
    out[1] = text
  end
end

-- The shared child walk: named, non-comment children yield their scopes
-- through {recurse}, redirects are handled by attach_redirects.
local function walk_children(node, source, recurse)
  local out, redirects = {}, {}
  for child in node:iter_children() do
    local kind = child:type()
    if child:named() and kind ~= "comment" then
      if REDIRECT_TYPES[kind] then
        redirects[#redirects + 1] = node_text(child, source)
      else
        for _, cmd in ipairs(recurse(child, source)) do
          out[#out + 1] = cmd
        end
      end
    end
  end
  attach_redirects(out, redirects)
  return out
end

-- The commands a block runs, through nested pipelines, lists, redirects and
-- blocks. Non-command words (loop variables, `case` patterns) yield nothing.
local function inner_commands(node, source)
  if ATOMIC_COMMAND_TYPES[node:type()] then
    local scope = command_scope(node, source)
    return scope and { scope } or {}
  end
  return walk_children(node, source, inner_commands)
end

-- Anything we don't walk through becomes a scope of its own text, so an
-- unknown node reaches the user instead of getting dropped. Blocks add their
-- inner commands as scopes too.
local function collect_commands(node, source)
  if WALK_THROUGH_TYPES[node:type()] then
    return walk_children(node, source, collect_commands)
  end

  local scope = command_scope(node, source)
  if scope == nil then
    return {}
  end
  local out = { scope }
  if BLOCK_TYPES[node:type()] then
    local seen = {}
    for _, inner in ipairs(inner_commands(node, source)) do
      if not seen[inner] then
        seen[inner] = true
        out[#out + 1] = inner
      end
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
  permission = "run",
  permission_scopes = function(input)
    local command = input.command
    if not command or command:match("^%s*$") then
      return nil
    end

    -- A `!` mis-parses the compound statement it negates, so drop such
    -- negations before parsing: they change nothing about what runs.
    local parse = strip_compound_negations(command)

    local parser = maki.treesitter.get_parser(parse, "bash")
    if not parser then
      return { scopes = { command }, force_prompt = true }
    end

    local root = parser:parse()[1]:root()
    if root:has_error() or is_complex(root) then
      return { scopes = { command }, force_prompt = true }
    end

    local segments = collect_commands(root, parse)
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

    -- Esc or deadline: hand back the lines streamed so far, so the model
    -- keeps what the user just watched instead of a bare error.
    maki.async.on_cancel(function(reason)
      if finished then
        return
      end
      finished = true
      local out = truncate(table.concat(output_parts), max_lines, max_bytes)
      ctx:finish(partial.cut(view, out, reason, timeout_secs))
    end)

    return nil
  end,
})
