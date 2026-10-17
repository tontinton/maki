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

local Runner = require("runner")
local Background = require("background")

-- Worst case is a batch child: tool body + batch body indents (2 + 2),
-- plus the transcript's scrollbar column.
local BODY_CHROME_COLS = 5
local MIN_MD_WIDTH = 20
local DEFAULT_OUTPUT_LINES = 5

local description = [[Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Subagent types (set via `subagent_type`):
- `research` (default): Read-only tools. For codebase exploration or gathering context.
- `general`: Full tool access. For delegating implementation work.

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. Each invocation starts fresh - inline any needed context into the prompt.
4. Tell it to return concise summaries with file:line refs, not full file contents.
5. Set background=true to return an id immediately. Use task_result to wait for its result or task_cancel to stop it. Requires a live TUI chat. Background tasks cannot ask for permissions.
]]

local examples = {
  {
    description = "Find auth middleware",
    prompt = "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
    model_tier = "weak",
  },
}

local function handler(input, ctx)
  local prepared, prepare_err = Runner.prepare(input, ctx)
  if prepare_err then
    return { llm_output = prepare_err, is_error = true }
  end

  if input.background == true then
    return Background.start(prepared, ctx)
  end

  local permit = Runner.semaphore:acquire()
  local sess
  local ok, out = pcall(function()
    local sess_err
    sess, sess_err = Runner.open(ctx, prepared)
    if sess_err then
      return { llm_output = sess_err, is_error = true }
    end
    return Runner.run(sess, prepared)
  end)

  if sess then
    sess:close()
  end
  permit:release()
  if not ok then
    error(out, 0)
  end
  return out
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
    width = math.max(maki.ui.terminal_size().cols - BODY_CHROME_COLS, MIN_MD_WIDTH),
  })
end

maki.api.register_tool({
  name = "task",
  description = description,
  kind = "execute",
  audiences = { "main", "workflow" },
  examples = examples,
  schema = Runner.schema,
  handler = handler,
  header = header,
  restore = restore,
})

local receipt_schema = {
  type = "object",
  required = { "id" },
  additionalProperties = false,
  properties = {
    id = { type = "string", description = "Background task id returned by task in this chat." },
  },
}

maki.api.register_tool({
  name = "task_result",
  description = "Wait for a background task and return its result. Results stay available until the chat ends or plugins reload. Cancelling this wait does not stop the task.",
  kind = "execute",
  audiences = { "main", "workflow" },
  schema = receipt_schema,
  timeout = false,
  handler = Background.result,
  header = function(input)
    return input.id
  end,
  restore = restore,
})

maki.api.register_tool({
  name = "task_cancel",
  description = "Cancel a queued or running background task in this chat. Use task_result to read its final or partial output.",
  kind = "execute",
  audiences = { "main", "workflow" },
  schema = receipt_schema,
  handler = Background.cancel,
  header = function(input)
    return input.id
  end,
})
