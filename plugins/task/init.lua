-- This plugin owns the /tasks picker over the subagents spawned here:
-- picker.lua registers the command and the keymap when this file is loaded, so
-- the two cannot be enabled apart and left pointing at each other's absence.

local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
require("picker")

local Runner = require("runner")
local Background = Runner.allow_background and require("background")

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

  if Background and input.background then
    local result = Background.start(prepared, ctx)
    if result then
      return result
    end
  end

  local permit = Runner.semaphore:acquire()
  -- Declared out here so the epilogue closes it on every path: left to the
  -- garbage collector it keeps the subagent's event relay alive on an idle VM.
  local sess

  -- pcall so a raised error cannot leak the permit or the session.
  local ok, out = pcall(function()
    local sess_err
    sess, sess_err = maki.agent.session(ctx, prepared.options)
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
