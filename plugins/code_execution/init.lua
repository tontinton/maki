local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")

local DEFAULT_TIMEOUT = 30
local DEFAULT_MAX_MEMORY_MB = 50
local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local NO_OUTPUT = "(no output)"

local function new_view(ctx, buf)
  return ToolView.new(buf, { max_lines = ctx:tool_output_lines().code_execution or 30 })
end

local function append_lines(view, text)
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    view:append(line)
  end
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

local function handler(input, ctx)
  local config = ctx:config()
  local timeout = input.timeout or config.code_execution_timeout_secs or DEFAULT_TIMEOUT

  local buf = maki.ui.buf()
  local view = new_view(ctx, buf)
  buf:on("click", function()
    view:toggle()
  end)

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

  local result, err = maki.interpreter.run(input.code, {
    timeout = timeout,
    max_memory_mb = config.interpreter_max_memory_mb or DEFAULT_MAX_MEMORY_MB,
    buf = buf,
    agent_ctx = ctx:agent_context(),
    on_output = show,
  })

  if err then
    if waiting then
      view:clear()
    end
    append_lines(view, err)
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

  local llm_output = truncate(
    output,
    config.max_output_lines or DEFAULT_MAX_OUTPUT_LINES,
    config.max_output_bytes or DEFAULT_MAX_OUTPUT_BYTES
  )
  view:finish()

  return { llm_output = llm_output, body = buf }
end

local function header(input)
  local lines = select(2, input.code:gsub("\n", "\n")) + 1
  return lines .. " lines"
end

local function restore(_input, output, is_error, ctx)
  local buf = maki.ui.buf()
  local view = new_view(ctx, buf)
  if is_error then
    view:append(output)
  elseif output == NO_OUTPUT then
    view:append({ { "No output", "dim" } })
  else
    append_lines(view, output)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

maki.api.register_tool({
  name = "code_execution",
  description = description,
  augment = "interpreter_tools",
  schema = schema,
  examples = examples,
  kind = "execute",
  audiences = { "main", "research_sub", "general_sub" },
  start_input = { field = "code", language = "python" },
  start_annotation = { field = "timeout", kind = "timeout" },
  handler = handler,
  header = header,
  restore = restore,
})

maki.api.register_prompt_hint({
  slot = "efficient_tools",
  content = "code_execution",
})
