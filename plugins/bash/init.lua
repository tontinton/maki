local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local partial = require("maki.partial")
local bash_log = require("bash_log")

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
local TASK_LOGS_DIR = "tasks"
local LOG_EXT = ".log"
local NO_SESSION_ERR = "no chat session to keep the output log under"
local NO_STATE_DIR_ERR = "no state directory to keep the output log in"
local FINISHED_ERR = "the command already finished"
local RECEIPT_TAIL_LINES = 20
local FALLBACK_TAIL_LINES = 2000
local FALLBACK_TAIL_BYTES = 50 * 1024
local LOG_MAX_BYTES = 32 * 1024 * 1024
local LOG_MAX_BYTES_CEILING = 1024 * 1024 * 1024
local FOREGROUND_NOTE = "Could not start in the background (%s). Ran in the foreground."
local EARLIER_IN_LOG = "[%d earlier lines are in the full output log]"
local EARLIER_DROPPED = "[%d earlier lines dropped]"
local FULL_OUTPUT_PATTERN = "^(.*)\nFull output: ([^\n]+)$"
local EXPIRE_BACKGROUND = { expire = "background" }

local rtk_available
local fallback_log_seq = 0

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

local function build_header_lines(lines)
  local header = {}
  for _, line in ipairs(lines) do
    header[#header + 1] = line
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
  if ctx:output_mode() == "full" then
    return nil
  end
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

local function log_path(ctx, session)
  local call_id = ctx:tool_use_id()
  -- A call nested in a batch has no id of its own.
  if not call_id then
    fallback_log_seq = fallback_log_seq + 1
    call_id = ("bash-%d-%d"):format(os.time(), fallback_log_seq)
  end
  local state = maki.env.state_dir()
  if not state then
    return nil, NO_STATE_DIR_ERR
  end
  return maki.fs.joinpath(state, TASK_LOGS_DIR, session, call_id:gsub("[^%w_%-]", "_") .. LOG_EXT)
end

local function receipt(tail, path, max_bytes)
  local text, count = bash_log.tail_text(tail, RECEIPT_TAIL_LINES)
  local progress = ("Full output (read it to check progress): %s"):format(path)
  if count == 0 then
    return "Running in the background. No output yet.\n" .. progress
  end
  local heading = count == 1 and "Last line:" or ("Last %d lines:"):format(count)
  local shown = output_limits.cut_middle(text, RECEIPT_TAIL_LINES, max_bytes)
  return ("Running in the background. %s\n%s\n%s"):format(heading, shown, progress)
end

local function create_bash_view(command, ctx)
  local tol = ctx:tool_output_lines()
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.bash) or 5,
    keep = "tail",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  })
  view:set_header(build_header_lines(maki.split(command, "\n")))
  maki.async.run(function()
    local highlighted = maki.ui.highlight(command, "bash")
    if highlighted then
      view:set_header(build_header_lines(highlighted))
    end
  end)
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

- Read, list, search, and edit files with dedicated tools, not cat/ls/find/grep/sed. Bash is for git, builds, tests, and file management (mv, cp, rm, mkdir).
- Use `workdir` param instead of `cd <dir> && <cmd>` patterns.
- Do NOT use to communicate text to the user.
- Chain dependent commands with `&&`. Use batch for independent ones.
- Provide a short `description` (3-5 words).
- Output sent to the model is cut beyond 2000 lines or 50KB.
- Interactive commands (sudo, ssh prompts) fail immediately.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Read, list, search, and edit files with dedicated tools, even outside cwd. Bash is for git, builds, tests, and file management (mv, cp, rm, mkdir).",
})

local opts = maki.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 120,
    min = 5,
    desc = "Kill the command after this many seconds. A call's `timeout` param overrides it.",
  },
  background_on_timeout = {
    default = false,
    desc = "Move a command that hits its timeout to the background instead of killing it.",
  },
  background_log_bytes = {
    default = LOG_MAX_BYTES,
    min = 1024,
    desc = "Size at which the output log of a command moved to the background rotates to `.log.1`. Values above 1 GiB count as 1 GiB.",
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
      background = {
        type = "boolean",
        description = "Run in the background and return at once; the result arrives as a message when it ends.",
      },
    },
  },
  permission = "run",
  permission_scopes = function(input)
    local command: string? = input.command
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
    if #hints == 0 then
      return s
    end
    local buf = maki.ui.buf()
    buf:line({ { s }, { " (" .. table.concat(hints, ", ") .. ")", "dim" } })
    return buf
  end,

  restore = function(input, output, is_error, ctx)
    local command = parse_cd_hint(input)
    local buf, view = create_bash_view(command, ctx)
    local timeout_secs = output:match("^tool bash timed out after (%d+)s$")
    local text, log_file = output:match(FULL_OUTPUT_PATTERN)
    text = text or output
    if timeout_secs then
      view:append({ { "Timed out after " .. timeout_secs .. "s", "dim" } })
    elseif is_error then
      local body, code = text:match("^(.-)\nExit code: (%d+)$")
      if body then
        view:append_text(body)
        view:append({ { "Exit code: " .. code, "dim" } })
      else
        view:append_text(text)
      end
    else
      if text == "Exit code: 0" or text == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      else
        view:append_text(text)
      end
    end
    if log_file then
      view:append({ { "Full output: " .. log_file, "dim" } })
    end
    view:finish()
    return buf
  end,

  handler = function(input, ctx)
    if not input.command then
      return { llm_output = "error: command is required", is_error = true }
    end

    local started_at = os.time()
    local command, workdir = parse_cd_hint(input)
    local timeout_secs = input.timeout or opts.timeout_secs
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    ctx:set_deadline(timeout_secs, opts.background_on_timeout and EXPIRE_BACKGROUND or nil)

    local rewritten = rtk_rewrite(command, ctx)
    if rewritten then
      command = rewritten
    end

    local buf, view = create_bash_view(command, ctx)

    -- `tail` and `log` replace `lines` once the call moves to the background:
    -- a command that outlives its turn can print without bound.
    local lines = {}
    local tail, log
    local has_output = false
    local finishing, finished = false, false
    local foreground_note

    -- Swaps `lines` for the bounded tail and returns what `lines` held. Never
    -- yields, so no line lands between the swap and the caller's next step.
    local function use_tail()
      local backlog = lines
      lines = nil
      tail = bash_log.new_tail(max_lines or FALLBACK_TAIL_LINES, max_bytes or FALLBACK_TAIL_BYTES)
      for _, line in ipairs(backlog) do
        bash_log.tail_push(tail, line)
      end
      return backlog
    end

    -- Opens the output log and moves to the bounded tail. Safe to call again:
    -- the runtime may refuse a move after this ran, and the next press asks
    -- again. Returns true, or nil and the reason the command cannot move.
    local function enter_background()
      if log then
        return true
      end
      if finishing or finished then
        return nil, FINISHED_ERR
      end
      local session = ctx:session_id()
      if not session then
        return nil, NO_SESSION_ERR
      end
      local path, path_err = log_path(ctx, session)
      if not path then
        return nil, path_err
      end
      local opened, open_err = bash_log.open(path, math.min(opts.background_log_bytes, LOG_MAX_BYTES_CEILING))
      if not opened then
        return nil, "cannot open the output log: " .. open_err
      end
      if finishing or finished then
        return nil, FINISHED_ERR
      end
      -- Nothing yields between the open's return and the swap, so no line is
      -- lost or written out of order.
      log = opened
      local backlog = use_tail()
      if #backlog > 0 then
        log:push(table.concat(backlog, "\n") .. "\n")
      end
      log:drain()
      return true
    end

    local function capture(line)
      if not has_output then
        has_output = true
        view:clear()
      end
      -- A bash inside a moved batch never runs its own hook, so its first
      -- output after the move does the same switch.
      if not tail and ctx:backgrounded() then
        local entered, enter_err = enter_background()
        if not entered then
          maki.log.warn("bash: moved without an output log: " .. enter_err)
          use_tail()
        end
      end
      if tail then
        bash_log.tail_push(tail, line)
        if log then
          log:push(line .. "\n")
        end
      else
        lines[#lines + 1] = line
      end
      view:append(line)
    end

    -- Presentation limits apply after the view streams every line. Both the
    -- exit and cancel path go through here.
    local function final_output()
      if not tail then
        return output_limits.cut_middle(table.concat(lines, "\n"), max_lines, max_bytes)
      end
      local text = output_limits.cut_middle(bash_log.tail_text(tail), max_lines, max_bytes)
      local dropped = bash_log.tail_dropped(tail)
      if dropped == 0 then
        return text
      end
      return (log and EARLIER_IN_LOG or EARLIER_DROPPED):format(dropped) .. "\n" .. text
    end

    local function reply_text(llm_output)
      if log then
        llm_output = ("%s\nFull output: %s"):format(llm_output, log.path)
      end
      return foreground_note and (foreground_note .. "\n" .. llm_output) or llm_output
    end

    local function finish(exit_code)
      if finishing or finished then
        return
      end
      -- A stop landing while the log drains still has to answer for the
      -- call, so `finished` waits until the reply is on its way.
      finishing = true
      if log then
        pcall(log.drain, log)
      end
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

      ctx:finish({ llm_output = reply_text(llm_output), is_error = is_error, body = buf })
    end

    view:append({ { "Waiting for output...", "dim" } })

    local job, err = maki.fn.jobstart(command, {
      cwd = workdir,
      env = { GIT_TERMINAL_PROMPT = "0" },
      on_stdout = function(_, line)
        capture(line)
      end,
      on_stderr = function(_, line)
        capture(line)
      end,
      on_exit = function(_, code)
        finish(code)
      end,
    })
    if not job then
      return { llm_output = "error: " .. err, is_error = true }
    end

    ctx:on_background(function()
      local entered, enter_err = enter_background()
      if not entered then
        return nil, enter_err
      end
      return receipt(tail, log.path, max_bytes)
    end)

    -- Esc or deadline: hand back the lines streamed so far, so the model
    -- keeps what the user just watched instead of a bare error.
    maki.async.on_cancel(function(reason)
      if finished then
        return
      end
      finished = true
      local cut = partial.cut(view, final_output(), reason, timeout_secs, started_at)
      cut.llm_output = reply_text(cut.llm_output)
      ctx:finish(cut)
    end)

    -- Fails without a live chat (headless, -p, SDK, ACP), and then the
    -- command simply stays in the foreground.
    if input.background == true then
      local moved, move_err = ctx:background()
      if not moved then
        foreground_note = FOREGROUND_NOTE:format(move_err)
      end
    end

    return nil
  end,
})
