-- Policy for the Python interpreter: which tools it may call, what the model
-- sees (via the `describe(dctx)` callback), and the import preamble. The
-- sandbox and dispatch live in Rust, which exposes primitives only
-- (`maki.api.get_tools`, `maki.agent.call_tool`); orchestration policy is here.

local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local MAX_SCRIPT_LINES = 2000
local NO_OUTPUT = "(no output)"
local SEPARATOR = "──────"
local PREAMBLE = "import re\nimport asyncio\nimport sys\nimport os\nimport json\n"
local TOOLS_HEADER = "\n\nAvailable tools (called as Python functions with keyword arguments):\n"
local WORKFLOW_TOOLS_NOTE =
  "\nWorkflow mode: orchestrate subagents from this script. Await every `task(...)` call and use `asyncio.gather` for parallel fan-out. Pass `output_schema` to task for machine-readable results (a JSON string, parse with `json.loads`). Raise this tool's `timeout` param: subagents outlive the default code_execution timeout.\n"
local PY_TYPES = { string = "str", integer = "int", boolean = "bool", array = "list" }

local opts = maki.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 30,
    min = 5,
    desc = "Stop the script after this many seconds. A call's `timeout` param overrides it.",
  },
  max_memory_mb = { default = 50, min = 10, desc = "Memory limit for the Python sandbox (MB)." },
}))

local function new_view(ctx, buf)
  return ToolView.new(buf, { max_lines = ctx:tool_output_lines().code_execution or 30 })
end

local function line_nr_fmt(count)
  local w = math.max(1, math.floor(math.log(count, 10)) + 1)
  return "%" .. w .. "d "
end

-- One body builder for every path (start preview, handler, restore), so the
-- script renders the same no matter which lifecycle callbacks ran. The
-- header is always rebuilt from scratch; nothing mutates existing lines.
local function build_body(ctx, code)
  local lines = maki.split(code:gsub("\n+$", ""), "\n")
  local hl
  local buf = maki.ui.buf()
  local view = new_view(ctx, buf)

  local function header()
    local total = #lines
    local shown = view.expanded and total or math.min(total, MAX_SCRIPT_LINES)
    local fmt = line_nr_fmt(shown)
    local out = {}
    for i = 1, shown do
      local spans = { { string.format(fmt, i), "line_nr" } }
      for _, seg in ipairs(hl and hl[i] or { { lines[i] } }) do
        spans[#spans + 1] = seg
      end
      out[#out + 1] = spans
    end
    if shown < total then
      out[#out + 1] = { { "... (" .. (total - shown) .. " lines) (click to expand)", "dim" } }
    end
    out[#out + 1] = { { SEPARATOR, "dim" } }
    return out
  end

  view:set_header(header())
  buf:on("click", function()
    view:toggle()
    view:set_header(header())
  end)

  local function highlight()
    local highlighted = maki.ui.highlight(table.concat(lines, "\n"), "py")
    if highlighted then
      hl = highlighted
      view:set_header(header())
    end
  end
  return buf, view, highlight
end

local description = [[Execute Python code in a sandboxed interpreter with tools as callable functions.

Use for chained/dependent tool calls and filtering/processing results, e.g. filtering web tool output. **DRAMATICALLY** faster than sequential tool calls!

- All tools are async and return strings: `result = await read(path='file.txt')`. Parse output yourself.
- Use `asyncio.gather()` for concurrency within one execution.
- Available libs: re, asyncio, sys, os, json. No other imports, no classes, no filesystem/network access.
- Fresh sandbox each run: no state persists between executions.
- 30 second timeout (configurable via `timeout` parameter).
- Skip it when a single tool call needs no transformation.
- NOT a thinking scratchpad. Reason in your response text.
]]

local schema = {
  type = "object",
  required = { "code" },
  additionalProperties = false,
  properties = {
    code = {
      type = "string",
      description = "Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency.",
    },
    timeout = {
      type = "integer",
      description = "Timeout in seconds (default 30, max 300)",
    },
  },
}

local examples = {
  {
    code = [[files = (await glob(pattern='**/*.rs')).strip().split('\n')
results = await asyncio.gather(*[read(path=f) for f in files if f.strip()])
for f, c in zip(files, results):
    if 'fn main' in c: print(f)]],
  },
  {
    code = [[result = await grep(pattern='TODO', include='*.rs')
print(f"{len(result.strip().splitlines())} TODOs found")]],
  },
  {
    code = [[content = await webfetch(url='https://example.com/docs')
for line in content.splitlines():
    if 'auth' in line.lower(): print(line)]],
  },
}

-- Shared predicate for describe and handler so advertised == callable.
-- The interpreter is a calling convention, not a capability grant: a read-only
-- subagent must not reach edit/write through Python.
local function interpreter_tools(tools, audience, workflow)
  local out = {}
  for _, t in ipairs(tools) do
    local aud = {}
    for _, a in ipairs(t.audiences) do
      aud[a] = true
    end
    if t.enabled and aud[audience] and (aud.interpreter or (workflow and aud.workflow)) then
      t.workflow_only = not aud.interpreter
      out[#out + 1] = t
    end
  end
  return out
end

local function matches_filter(name, dctx)
  if dctx.only then
    for _, n in ipairs(dctx.only) do
      if n == name then
        return true
      end
    end
    return false
  end
  if dctx.except then
    for _, n in ipairs(dctx.except) do
      if n == name then
        return false
      end
    end
  end
  return true
end

local function signature(t)
  local schema_props = (t.schema and t.schema.properties) or {}
  local required = {}
  for _, r in ipairs((t.schema and t.schema.required) or {}) do
    required[r] = true
  end
  local names = {}
  for pname in pairs(schema_props) do
    names[#names + 1] = pname
  end
  table.sort(names, function(a, b)
    local ra, rb = required[a] or false, required[b] or false
    if ra ~= rb then
      return ra
    end
    return a < b
  end)
  local params = {}
  for _, pname in ipairs(names) do
    local ptype = PY_TYPES[schema_props[pname].type] or "any"
    params[#params + 1] = required[pname] and (pname .. ": " .. ptype) or (pname .. ": " .. ptype .. " = None")
  end
  return "- " .. t.name .. "(" .. table.concat(params, ", ") .. ") -> str"
end

-- Keep cheap: runs on every request build. get_tools skips descriptions
-- to avoid recursion from describe callbacks.
local function describe(dctx)
  local parts = { description, TOOLS_HEADER }
  local has_workflow_only = false
  for _, t in ipairs(interpreter_tools(maki.api.get_tools(), dctx.audience, dctx.workflow)) do
    if matches_filter(t.name, dctx) then
      has_workflow_only = has_workflow_only or t.workflow_only
      parts[#parts + 1] = signature(t) .. "\n"
    end
  end
  if has_workflow_only then
    parts[#parts + 1] = WORKFLOW_TOOLS_NOTE
  end
  return table.concat(parts)
end

-- Publishes the script before the permission prompt paints. Highlight is
-- awaited inline: an async task from here could outlive ToolDone on fast
-- auto-allowed runs and bake a stale script-only snapshot.
local function start(input, ctx)
  local buf, _, highlight = build_body(ctx, input.code)
  ctx:live_buf(buf)
  highlight()
end

local function handler(input, ctx)
  local config = ctx:config()
  local timeout = input.timeout or opts.timeout_secs

  local buf, view, highlight = build_body(ctx, input.code)
  ctx:live_buf(buf)
  maki.async.run(highlight)

  ctx:set_deadline(timeout)

  view:append({ { "Waiting for output...", "dim" } })

  local waiting = true
  local function show(line)
    if waiting then
      waiting = false
      view:clear()
    end
    view:append(line)
  end

  local tools = {}
  for _, t in ipairs(interpreter_tools(maki.api.get_tools({ config = config }), ctx:audience(), ctx:workflow())) do
    local name = t.name
    tools[name] = function(tool_input)
      return maki.agent.call_tool(ctx, name, tool_input, { timeout = timeout })
    end
  end

  local result, err = maki.interpreter.run(PREAMBLE .. input.code, {
    timeout = timeout,
    max_memory_mb = opts.max_memory_mb,
    on_output = show,
    tools = tools,
  })

  if err then
    if waiting then
      view:clear()
    end
    view:append_text(err)
    view:finish()
    return { llm_output = err, is_error = true, body = buf }
  end

  local output = result.stdout or ""
  if result.output then
    show("return: " .. result.output)
    output = (#output > 0 and output .. "\n" or "") .. "return: " .. result.output
  end
  if #output == 0 then
    output = NO_OUTPUT
    view:clear()
    view:append({ { "No output", "dim" } })
  end

  local max_lines, max_bytes = output_limits.resolve(opts, ctx)
  local llm_output = truncate(output, max_lines, max_bytes)
  view:finish()

  return { llm_output = llm_output, body = buf }
end

local function header(input)
  local lines = select(2, input.code:gsub("\n", "\n")) + 1
  return lines .. " lines"
end

local function restore(input, output, is_error, ctx)
  local buf, view, highlight = build_body(ctx, input.code)
  if is_error then
    view:append(output)
  elseif output == NO_OUTPUT then
    view:append({ { "No output", "dim" } })
  else
    view:append_text(output)
  end
  view:finish()
  highlight()
  return buf
end

maki.api.register_tool({
  name = "code_execution",
  description = description,
  describe = describe,
  schema = schema,
  examples = examples,
  kind = "execute",
  audiences = { "main", "research_sub", "general_sub" },
  start_annotation = { field = "timeout", kind = "timeout" },
  start = start,
  handler = handler,
  header = header,
  restore = restore,
})

maki.api.register_prompt_hint({
  slot = "efficient_tools",
  content = "code_execution",
})
