-- Structured-output story: the subagent gets a session-local structured_output
-- tool whose handler validates and captures the result as closure upvalues.
-- Invalid input is an inline tool error the model can fix in the same run.
-- This plugin owns structured output and subagent concurrency; Rust exposes
-- primitives only (`maki.agent.session`, `maki.json.schema_validator`,
-- `maki.async.semaphore`).
--
-- It also owns the /tasks picker over the subagents spawned here: picker.lua
-- registers the command and the keymap when this file is loaded, so the two
-- cannot be enabled apart and left pointing at each other's absence.

local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
require("picker")

local STRUCTURED_OUTPUT_NAME = "structured_output"
local STRUCTURED_OUTPUT_DESCRIPTION = "Report your final result. Call it exactly once when your task is complete."
local STRUCTURED_OUTPUT_ACK = "Output recorded."
local STRUCTURED_OUTPUT_PROMPT_SUFFIX = "\n\nWhen finished, call the structured_output tool with your final result."
local MAX_NUDGES = 2
local MAX_SCHEMA_ERRORS = 3
local SCHEMA_COMPILE_ERROR = "invalid output_schema"
local SCHEMA_ROOT_ERROR = "output_schema must have type object"
local STRUCTURED_MISSING_ERROR = "subagent finished without calling structured_output"
local STRUCTURED_INVALID_ERROR = "subagent result does not match output_schema"
local SUMMARY_MISSING_ERROR = "subagent finished without providing a summary"
local NUDGE_MISSING =
  "You did not call the structured_output tool. Call it now with your final result matching its input schema."
local NUDGE_SUMMARY =
  "You finished your work but did not provide a summary. Reply with a concise summary of what you did and found."
local INVALID_INPUT_PREFIX =
  "Input does not match the required schema. Fix the errors and call structured_output again:\n"
local BODY_INDENT_COLS = 4
local MIN_MD_WIDTH = 20
local DEFAULT_OUTPUT_LINES = 5
local BG_WORKING = "working"
local BG_DONE = "done"
local BG_ERROR = "error"
local BG_TICK_MS = 100
local BG_WAIT_DEFAULT_MS = 60000
local BG_WAIT_MAX_MS = 600000
local BG_WAIT_MIN_ERR = "timeout_ms must be >= 1"
local BG_NOTIFY_MAX_CHARS = 2000
local BG_UNKNOWN_ID_PREFIX = "unknown task id: "
-- Mirrors maki_agent::TASK_HANDOFF_ANNOTATION: the receipt annotation tells
-- the UI the subagent keeps running after this tool call ends.
local TASK_HANDOFF_ANNOTATION = "backgrounded"

local description = [[Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Subagent types (set via `subagent_type`):
- `research` (default): Read-only tools. For codebase exploration or gathering context.
- `general`: Full tool access. For delegating implementation work.

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. Each invocation starts fresh - inline any needed context into the prompt.
4. Tell it to return concise summaries with file:line refs, not full file contents.
5. With `background = true` the call returns a receipt at once, the outcome arrives as a message when the subagent finishes (`task_result` fetches it, `task_wait` blocks for it, a /reload cancels it).
]]

local opts = maki.api.register_options({
  max_concurrent = { default = 8, min = 1, desc = "Max concurrently running subagents." },
  allow_model = {
    default = false,
    desc = "Expose a `model` input that overrides the subagent model. Only enable if you trust callers to pick an exact model themselves.",
  },
})

local schema = {
  type = "object",
  required = { "description", "prompt" },
  additionalProperties = false,
  properties = {
    description = {
      type = "string",
      description = "Short (3-5 words) description of the task",
    },
    prompt = {
      type = "string",
      description = "Detailed task prompt for the agent",
    },
    subagent_type = {
      type = "string",
      description = 'Subagent type: "research" (read-only, default) or "general" (can modify files)',
    },
    background = {
      type = "boolean",
      description = "Run in the background. The call returns a receipt at once, the outcome arrives as a message when the subagent finishes, and task_result or task_wait can fetch it.",
    },
    model_tier = {
      type = "string",
      description = 'Model tier (optional, omit to use current model, capped at current tier):\n- "strong" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.\n- "medium" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.\n- "weak" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits.',
    },
    output_schema = {
      description = "JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string.",
    },
  },
}

-- Only advertise `model` when the plugin opts in: it costs tokens in every
-- task schema, and an off-by-default flag keeps the common path lean.
if opts.allow_model then
  schema.properties.model = {
    type = "string",
    description = 'Exact model spec, e.g. "ollama/glm-5.2". You tell maki the model; maki will not guess. Overrides model_tier.',
  }
end

local examples = {
  {
    description = "Find auth middleware",
    prompt = "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
    model_tier = "weak",
  },
}

-- Process-wide cap on concurrent subagents.
local semaphore = maki.async.semaphore(opts.max_concurrent)

-- Background receipts: task_id -> { description, status, is_error, result }.
local bg_tasks = {}
local bg_seq = 0

local function bounded_errors(errors)
  local out = {}
  for i = 1, math.min(#errors, MAX_SCHEMA_ERRORS) do
    out[i] = errors[i]
  end
  return table.concat(out, "\n")
end

local function finish_subagent(sess, message, validator, state)
  local result, err = sess:prompt(message)
  local retries = 0
  while not err and retries < MAX_NUDGES do
    if validator and not state.captured then
      retries = retries + 1
      result, err = sess:prompt(NUDGE_MISSING)
    elseif not validator and result.text == "" then
      retries = retries + 1
      result, err = sess:prompt(NUDGE_SUMMARY)
    else
      break
    end
  end

  -- Classify before closing: close carries the verdict so a backgrounded
  -- session's UI item ends correctly even though no tool result follows.
  local out
  if err then
    -- A result alongside the error means the run was cut short after
    -- streaming some text, and half a transcript beats a bare error.
    if result then
      out = {
        llm_output = "sub-agent interrupted (" .. err .. "). Partial output:\n" .. result.text,
        is_error = true,
      }
    else
      out = { llm_output = "sub-agent error: " .. err, is_error = true }
    end
  elseif validator and not state.captured then
    local msg = state.last_errors and (STRUCTURED_INVALID_ERROR .. ":\n" .. state.last_errors)
      or STRUCTURED_MISSING_ERROR
    out = { llm_output = msg, is_error = true }
  elseif not validator and result.text == "" then
    out = { llm_output = SUMMARY_MISSING_ERROR, is_error = true }
  else
    out = { llm_output = state.captured and maki.json.encode(state.captured) or result.text, format = "markdown" }
  end

  sess:close(out.is_error and out.llm_output or nil)
  return out
end

local function notify_bg(task_id, entry, session_id)
  local body = entry.result or ""
  if #body > BG_NOTIFY_MAX_CHARS then
    body = body:sub(1, BG_NOTIFY_MAX_CHARS) .. "..."
  end
  local label = entry.is_error and "failed" or "finished"
  maki.session.notify(
    "Background task " .. task_id .. " (" .. entry.description .. ") " .. label .. ":\n" .. body,
    { session = session_id, wake = true }
  )
end

local function task_output(entry, task_id)
  if entry.status == BG_WORKING then
    return { llm_output = "task " .. task_id .. " still working: " .. entry.description }
  end
  return { llm_output = entry.result, is_error = entry.is_error }
end

local function handler(input, ctx)
  local sid = ctx:session_id()
  local subagent_type = input.subagent_type or "research"
  if subagent_type ~= "research" and subagent_type ~= "general" then
    return { llm_output = "unknown subagent type: " .. subagent_type, is_error = true }
  end

  -- Compile early: a bad schema costs zero tokens.
  local validator
  if input.output_schema then
    if type(input.output_schema) ~= "table" or input.output_schema.type ~= "object" then
      return { llm_output = SCHEMA_ROOT_ERROR, is_error = true }
    end
    local compile_err
    validator, compile_err = maki.json.schema_validator(input.output_schema)
    if compile_err then
      return { llm_output = SCHEMA_COMPILE_ERROR .. ": " .. compile_err, is_error = true }
    end
  end

  local model, model_err = maki.agent.resolve_model(ctx, {
    tier = input.model_tier,
    spec = opts.allow_model and input.model or nil,
  })
  if model_err then
    return { llm_output = model_err, is_error = true }
  end

  local audience = subagent_type == "research" and "research_sub" or "general_sub"
  local prompt_id = subagent_type == "research" and "research" or "general"
  local system, system_err = maki.agent.system_prompt(ctx, {
    prompt_id = prompt_id,
    instructions = true,
  })
  if system_err then
    return { llm_output = system_err, is_error = true }
  end

  local tool_defs, tools_err = maki.agent.tools(ctx, {
    audience = audience,
    spec = model.spec,
  })
  if tools_err then
    return { llm_output = tools_err, is_error = true }
  end

  local state = {}
  local local_tools
  if validator then
    local_tools = {
      [STRUCTURED_OUTPUT_NAME] = {
        description = STRUCTURED_OUTPUT_DESCRIPTION,
        input_schema = input.output_schema,
        handler = function(value)
          local errs = validator:validate(value)
          if errs then
            state.last_errors = bounded_errors(errs)
            return nil, INVALID_INPUT_PREFIX .. state.last_errors
          end
          state.captured = value
          return STRUCTURED_OUTPUT_ACK
        end,
      },
    }
  end

  local message = input.prompt
  if validator then
    message = message .. STRUCTURED_OUTPUT_PROMPT_SUFFIX
  end

  local session_opts = {
    model_spec = model.spec,
    system = system,
    tools = tool_defs,
    local_tools = local_tools,
    audience = audience,
    name = input.description,
  }

  if not input.background then
    local permit = semaphore:acquire()
    local ok, out = pcall(function()
      local sess, sess_err = maki.agent.session(ctx, session_opts)
      if sess_err then
        return { llm_output = sess_err, is_error = true }
      end
      return finish_subagent(sess, message, validator, state)
    end)
    permit:release()
    if not ok then
      error(out, 0)
    end
    return out
  end

  local permit = semaphore:acquire()
  local sess, sess_err
  local ok, raised = pcall(function()
    sess, sess_err = maki.agent.session(ctx, session_opts)
  end)
  if not ok then
    permit:release()
    error(raised, 0)
  end
  if sess_err then
    permit:release()
    return { llm_output = sess_err, is_error = true }
  end

  bg_seq = bg_seq + 1
  local task_id = (sid or "session") .. ":" .. bg_seq
  local entry = { description = input.description, status = BG_WORKING, is_error = false }
  bg_tasks[task_id] = entry

  maki.async.run(function()
    return finish_subagent(sess, message, validator, state)
  end, {
    deadline_ms = false,
    on_finish = function(err, out)
      permit:release()
      if err then
        entry.status, entry.is_error, entry.result = BG_ERROR, true, tostring(err)
      elseif out.is_error then
        entry.status, entry.is_error, entry.result = BG_ERROR, true, out.llm_output
      else
        entry.status, entry.result = BG_DONE, out.llm_output
      end
      notify_bg(task_id, entry, sid)
    end,
  })

  return {
    llm_output = "Background task "
      .. task_id
      .. " started ("
      .. input.description
      .. "). The outcome arrives as a message when it finishes; task_result fetches it, task_wait blocks for it.",
    format = "markdown",
    annotation = TASK_HANDOFF_ANNOTATION,
  }
end

local function header(input)
  return input.description
end

-- Standalone runs render markdown on the Rust side (format = "markdown");
-- this mirrors that for restore and batch children, which build the body here.
local function restore(_input, output, is_error, ctx)
  local tol = ctx:tool_output_lines()
  return ToolView.restore_markdown(output, is_error, {
    max_lines = (tol and tol.task) or DEFAULT_OUTPUT_LINES,
    keep = "head",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
    width = math.max(maki.ui.terminal_size().cols - BODY_INDENT_COLS, MIN_MD_WIDTH),
  })
end

maki.api.register_tool({
  name = "task",
  description = description,
  kind = "execute",
  audiences = { "main", "workflow" },
  examples = examples,
  schema = schema,
  handler = handler,
  header = header,
  restore = restore,
})

local task_id_schema = {
  type = "object",
  required = { "task_id" },
  additionalProperties = false,
  properties = {
    task_id = {
      type = "string",
      description = "Task id from the background receipt.",
    },
  },
}

maki.api.register_tool({
  name = "task_result",
  description = "Fetch the status or final output of a background task launched with the task tool (`background = true`).",
  kind = "read",
  audiences = { "main", "workflow" },
  schema = task_id_schema,
  handler = function(input, _ctx)
    local entry = bg_tasks[input.task_id]
    if not entry then
      return { llm_output = BG_UNKNOWN_ID_PREFIX .. input.task_id, is_error = true }
    end
    return task_output(entry, input.task_id)
  end,
})

maki.api.register_tool({
  name = "task_wait",
  description = "Block until a background task finishes or the timeout passes. Returns the result when done, a still-working note on timeout.",
  kind = "read",
  audiences = { "main", "workflow" },
  schema = {
    type = "object",
    required = { "task_id" },
    additionalProperties = false,
    properties = {
      task_id = {
        type = "string",
        description = "Task id from the background receipt.",
      },
      timeout_ms = {
        type = "integer",
        description = "Maximum wait in milliseconds (default 60000, cap 600000).",
      },
    },
  },
  handler = function(input, _ctx)
    local entry = bg_tasks[input.task_id]
    if not entry then
      return { llm_output = BG_UNKNOWN_ID_PREFIX .. input.task_id, is_error = true }
    end
    local timeout_ms = input.timeout_ms or BG_WAIT_DEFAULT_MS
    if timeout_ms < 1 then
      return { llm_output = BG_WAIT_MIN_ERR, is_error = true }
    end
    timeout_ms = math.min(timeout_ms, BG_WAIT_MAX_MS)
    local waited = 0
    while entry.status == BG_WORKING and waited < timeout_ms do
      local tick = math.min(BG_TICK_MS, timeout_ms - waited)
      maki.async.sleep(tick)
      waited = waited + tick
    end
    if entry.status == BG_WORKING then
      return { llm_output = "task " .. input.task_id .. " still working after " .. timeout_ms .. "ms" }
    end
    return task_output(entry, input.task_id)
  end,
})
