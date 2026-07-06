-- Concurrent tool dispatch. The state table is the single source of truth:
-- llm output, UI rendering, and session restore are all pure functions of
-- it. Each child body is the child tool's own live/restored buf, so a
-- batch child renders exactly like the same tool run standalone.

local ToolView = require("maki.tool_view")

local MAX_BATCH_SIZE = 25
local SEPARATOR = "──────────────────"
local BODY_INDENT = "  "
local ERROR_PREFIX = "[ERROR] "
local EMPTY_ERROR = "provide at least one tool call"
local NESTED_ERROR = "cannot nest batch inside batch"
local CANCELLED_ERROR = "cancelled"
local DISCARDED_ERROR = string.format("maximum of %d tools per batch", MAX_BATCH_SIZE)
local SECTION_FMT = "## %s\n"
local SUMMARY_MIXED_FMT = "Executed %d/%d successfully. %d failed."
local SUMMARY_ALL_OK_FMT = "All %d tools executed successfully."

local description = string.format(
  [[Executes multiple independent tool calls concurrently to reduce round-trips.

ALWAYS USE THE BATCH TOOL WHEN YOU HAVE MULTIPLE INDEPENDENT TOOL CALLS. This dramatically improves performance.

Rules:
- 1-%d tool calls per batch
- All calls run in parallel; order NOT guaranteed
- Partial failures do not stop other calls
- Do NOT nest batch inside batch
- Do NOT use for dependent operations or when filtering results (use code_execution)]],
  MAX_BATCH_SIZE
)

local schema = {
  type = "object",
  properties = {
    tool_calls = {
      type = "array",
      description = "Array of tool calls to execute in parallel",
      required = true,
      items = {
        description = "Tool invocation: { tool: string, parameters: object } or flat { tool: string, ...params }",
      },
    },
  },
}

local examples = {
  {
    tool_calls = {
      { tool = "glob", parameters = { pattern = "src/**/*.ts" } },
      { tool = "grep", parameters = { pattern = "import", include = "*.ts" } },
      { tool = "index", parameters = { path = "/project/index.ts" } },
    },
  },
}

-- Models send entries in two shapes: { tool, parameters } and flat
-- { tool, ...params }. Accept either, or both merged (duplicate keys
-- rejected). Never mutates the input entry.
local function normalize_entry(entry)
  if type(entry) ~= "table" then
    return nil, "batch entry must be an object"
  end
  local tool = entry.tool
  if type(tool) ~= "string" then
    return nil, "batch entry missing 'tool'"
  end
  local rest = {}
  local has_rest = false
  for k, v in pairs(entry) do
    if k ~= "tool" and k ~= "parameters" then
      rest[k] = v
      has_rest = true
    end
  end
  local nested = entry.parameters
  local params
  if nested == nil then
    if not has_rest then
      return nil, "batch entry missing 'parameters'"
    end
    params = rest
  elseif not has_rest then
    params = nested
  elseif type(nested) ~= "table" then
    return nil, "'parameters' must be an object when flat fields are also present"
  else
    params = rest
    for k, v in pairs(nested) do
      if params[k] ~= nil then
        return nil, "duplicate parameter '" .. k .. "' in both 'parameters' and flat fields"
      end
      params[k] = v
    end
  end
  return { tool = tool, params = params }
end

-- The child's own header fn draws the header, pcall-isolated. On error or
-- absence (e.g. MCP tools) the plain tool name is enough.
local function header_spans(tool, params)
  local ok, spans = pcall(function()
    local t = maki.api.get_tool(tool)
    if not (t and t.header) then
      return nil
    end
    local res = t.header(params)
    if type(res) == "string" then
      return { { res, "tool" } }
    end
    if type(res) == "userdata" then
      local lines = res:get_lines()
      if lines[1] then
        return lines[1]
      end
    end
    return nil
  end)
  if ok and spans then
    return spans
  end
  return { { tool, "tool" } }
end

-- The child's own restore fn builds the body buf, with the real is_error,
-- so an error child renders exactly like the standalone tool. It is
-- pcall-isolated; on error or absence the ToolView fallback matches the
-- standalone plain rendering.
local function child_body_buf(c, tol)
  local output = c.output or ""
  local ok, buf = pcall(function()
    local t = maki.api.get_tool(c.tool)
    if not (t and t.restore) then
      return nil
    end
    local rctx = {}
    function rctx:tool_output_lines()
      return tol
    end
    function rctx:state()
      return nil
    end
    local reply = t.restore(c.params, output, c.status == "error", rctx)
    local body = reply
    if type(reply) == "table" then
      body = reply.body
    end
    if type(body) == "userdata" then
      return body
    end
    return nil
  end)
  if ok and buf then
    return buf
  end
  return ToolView.restore(output, { max_lines = tol[c.tool] or tol.other, keep = "head" })
end

local function indented(line)
  local out = { { BODY_INDENT } }
  for _, s in ipairs(line) do
    out[#out + 1] = s
  end
  return out
end

local function indicator_span(status)
  if status == "running" then
    return { "· ", "spinner" }
  elseif status == "success" then
    return { "● ", "tool_success" }
  elseif status == "error" then
    return { "● ", "tool_error" }
  end
  return { "○ ", "dim" }
end

-- Live annotations (e.g. a task child's model) arrive before the done
-- annotation (e.g. "12 lines"); append with the same separator the
-- standalone header uses.
local function annotate(c, ann)
  c.annotation = c.annotation and (c.annotation .. " · " .. ann) or ann
end

-- Rebuilds the full body on every change. Each child body is read fresh
-- from the child's own buf, and click ranges come out of the same pass, so
-- the row -> child map can never drift from the lines.
local function render(children, tol)
  local lines, ranges = {}, {}
  for i, c in ipairs(children) do
    if i > 1 then
      lines[#lines + 1] = {}
      lines[#lines + 1] = { { SEPARATOR, "dim" } }
      lines[#lines + 1] = {}
    end
    local first = #lines + 1
    local spans = { indicator_span(c.status), { c.tool .. "> ", "tool_prefix" } }
    for _, s in ipairs(c.header or {}) do
      spans[#spans + 1] = s
    end
    if c.annotation then
      spans[#spans + 1] = { " (" .. c.annotation .. ")", "tool_annotation" }
    end
    lines[#lines + 1] = spans
    if c.buf then
      for _, bl in ipairs(c.buf:get_lines()) do
        lines[#lines + 1] = indented(bl)
      end
    end
    ranges[i] = { first = first, last = #lines }
  end
  return lines, ranges
end

-- Byte-identical to the old native batch's llm format; batch_policy.rs
-- pins it.
local function render_llm(children)
  local out = {}
  local total = #children
  local failed = 0
  for _, c in ipairs(children) do
    out[#out + 1] = string.format(SECTION_FMT, c.tool)
    if c.status == "success" then
      out[#out + 1] = c.output or ""
    else
      failed = failed + 1
      out[#out + 1] = ERROR_PREFIX .. (c.output or "")
    end
    out[#out + 1] = "\n\n"
  end
  if failed > 0 then
    out[#out + 1] = string.format(SUMMARY_MIXED_FMT, total - failed, total, failed)
  else
    out[#out + 1] = string.format(SUMMARY_ALL_OK_FMT, total)
  end
  return table.concat(out)
end

local function to_state(children)
  local out = {}
  for i, c in ipairs(children) do
    out[i] = { tool = c.tool, status = c.status, output = c.output, annotation = c.annotation }
  end
  return { children = out }
end

-- Normalizes children and attaches statuses and headers; the one entry
-- point both phases (handler, restore) go through. Bodies come later,
-- once a rerender closure exists to watch them.
local function prepare_children(tool_calls)
  if type(tool_calls) ~= "table" then
    return nil, "tool_calls must be an array"
  end
  local children = {}
  for i, entry in ipairs(tool_calls) do
    local c, err = normalize_entry(entry)
    if not c then
      return nil, err
    end
    if i > MAX_BATCH_SIZE then
      c.status, c.output = "error", DISCARDED_ERROR
    elseif c.tool == "batch" then
      c.status, c.output = "error", NESTED_ERROR
    else
      c.status = "pending"
    end
    c.header = header_spans(c.tool, c.params)
    children[i] = c
  end
  return children
end

-- One buf whose lines and click ranges always come from the same render
-- pass over state. A click is forwarded to the child's own buf handler:
-- a row inside a child's range maps to that child's buffer row; row 0
-- (header, or the restore-expand replay) and unmapped rows go to every
-- child. Either way the child's real toggle logic runs.
local function make_view(children, tol)
  local buf = maki.ui.buf()
  local ranges
  local function rerender()
    local lines
    lines, ranges = render(children, tol)
    buf:set_lines(lines)
  end
  rerender()
  local function forward_click(row)
    if row >= 1 and ranges then
      for i, r in ipairs(ranges) do
        if row >= r.first and row <= r.last then
          local c = children[i]
          if c.buf then
            c.buf:click({ row = row - r.first })
          end
          return
        end
      end
    end
    for _, c in ipairs(children) do
      if c.buf then
        c.buf:click({ row = 0 })
      end
    end
  end
  buf:on("click", function(ev)
    forward_click(ev and ev.row or 0)
    rerender()
  end)
  return buf, rerender
end

-- Builds the child's body buf and recomposes the batch view whenever it
-- changes (e.g. async highlights arriving after restore).
local function attach_body(c, tol, rerender)
  c.buf = child_body_buf(c, tol)
  c.buf:on("change", rerender)
end

local function handler(input, ctx)
  local tol = ctx:tool_output_lines()
  local children, err = prepare_children(input.tool_calls)
  if not children then
    return { llm_output = err, is_error = true }
  end
  if #children == 0 then
    return { llm_output = EMPTY_ERROR, is_error = true }
  end

  local buf, rerender = make_view(children, tol)
  for _, c in ipairs(children) do
    if c.status == "error" then
      attach_body(c, tol, rerender)
    end
  end
  rerender()
  ctx:live_buf(buf)

  local funs = {}
  for _, c in ipairs(children) do
    if c.status == "pending" then
      funs[#funs + 1] = function()
        c.status = "running"
        rerender()
        local text, cerr, ann = maki.agent.call_tool(ctx, c.tool, c.params, {
          -- Clicks on a still-streaming child are a no-op: its click
          -- handler lives on the child's own handle, not this wrapper.
          on_live_buf = function(b)
            c.buf = b
            b:on("change", rerender)
            rerender()
          end,
          on_annotation = function(a)
            annotate(c, a)
            rerender()
          end,
        })
        if ann then
          annotate(c, ann)
        end
        if cerr then
          c.status, c.output = "error", cerr
        else
          c.status, c.output = "success", text
        end
        attach_body(c, tol, rerender)
        rerender()
      end
    end
  end
  maki.async.gather(funs)
  for _, c in ipairs(children) do
    if c.status == "pending" or c.status == "running" then
      c.status, c.output = "error", CANCELLED_ERROR
      attach_body(c, tol, rerender)
    end
  end
  rerender()

  return {
    llm_output = render_llm(children),
    body = buf,
    state = to_state(children),
  }
end

local function restore(input, output, _is_error, rctx)
  local tol = rctx:tool_output_lines()
  local children = prepare_children(input.tool_calls or {})
  if not children then
    return ToolView.restore(output, { max_lines = tol.other, keep = "head" })
  end

  local st = rctx:state()
  if st and type(st.children) == "table" and #st.children == #children then
    local buf, rerender = make_view(children, tol)
    for i, sc in ipairs(st.children) do
      local c = children[i]
      c.status = sc.status or "error"
      c.output = sc.output
      c.annotation = sc.annotation
      attach_body(c, tol, rerender)
    end
    rerender()
    return buf
  end

  -- Old or foreign session without structured state: headers are still a
  -- pure function of the input; the stored output renders as one plain body.
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = tol.other, keep = "head" })
  local header = render(children, tol)
  header[#header + 1] = {}
  header[#header + 1] = { { SEPARATOR, "dim" } }
  header[#header + 1] = {}
  view:set_header(header)
  for line in (output .. "\n"):gmatch("([^\n]*)\n") do
    view:append(BODY_INDENT .. line)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

maki.api.register_tool({
  name = "batch",
  description = description,
  kind = "execute",
  audiences = { "main", "research_sub", "general_sub" },
  schema = schema,
  examples = examples,
  header = function(input)
    return #(input.tool_calls or {}) .. " tools"
  end,
  handler = handler,
  restore = restore,
})
