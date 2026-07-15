-- Concurrent tool dispatch. Rules that keep this file boring to debug:
--
--   1. `children` is the single source of truth: llm output, UI lines,
--      persisted state, and click routing are all pure functions of it.
--   2. Every child ends through one door, `Batch:settle`, which refuses
--      to run twice, so status, output, and body buf never disagree.
--   3. Once a `Batch` owns the children, only its methods touch them,
--      and each method ends with a rerender, so the screen never goes
--      stale.
--   4. Child handles from `get_tool` come isolated and normalized by the
--      host: a broken child degrades to plain rendering, never sinks the
--      batch.

local ToolView = require("maki.tool_view")

local MAX_BATCH_SIZE = 25
local SEPARATOR = "──────────────────"
local BODY_INDENT = "  "
local ANNOTATION_SEP = " · "
local ERROR_PREFIX = "[ERROR] "
local EMPTY_ERROR = "provide at least one tool call"
local NESTED_ERROR = "cannot nest batch inside batch"
local CANCELLED_ERROR = "cancelled"
local DISCARDED_ERROR = string.format("maximum of %d tools per batch", MAX_BATCH_SIZE)
local SECTION_FMT = "## %s\n"
local SECTION_PAT = "^## (.+)$"

-- Anchored pattern matching exactly what a `%d`-only format produces, so
-- the no-state parser can never drift from the render formats below.
local function fmt_to_pat(fmt)
  local pat = fmt:gsub("%%d", "\1"):gsub("[%^%$%(%)%.%[%]%*%+%-%?%%]", "%%%0"):gsub("\1", "%%d+")
  return "^" .. pat .. "$"
end

local SUMMARY_MIXED_FMT = "Executed %d/%d successfully. %d failed."
local SUMMARY_ALL_OK_FMT = "All %d tools executed successfully."
local SUMMARY_PATS = { fmt_to_pat(SUMMARY_ALL_OK_FMT), fmt_to_pat(SUMMARY_MIXED_FMT) }
local RESETTLE_FMT = "batch: child %s settled twice (%s -> %s)"

local STATUS = { PENDING = "pending", RUNNING = "running", SUCCESS = "success", ERROR = "error" }
local TERMINAL = { [STATUS.SUCCESS] = true, [STATUS.ERROR] = true }
local INDICATOR = {
  [STATUS.PENDING] = { "○ ", "dim" },
  [STATUS.RUNNING] = { "· ", "spinner" },
  [STATUS.SUCCESS] = { "● ", "tool_success" },
  [STATUS.ERROR] = { "● ", "tool_error" },
}

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

--- Input normalization (pure) ---------------------------------------------

-- Models send entries in two shapes, { tool, parameters } and flat
-- { tool, ...params }, so accept either, or even both merged, as long
-- as no key appears twice.
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

--- Child presentation ------------------------------------------------------

-- Let the child tool draw its own header. Some tools have none (MCP
-- ones, for example) and a broken one comes back nil; either way the
-- plain tool name is enough.
local function header_spans(tool, params)
  local t = maki.api.get_tool(tool)
  local spans = t and t.header and t.header(params)
  return spans or { { tool, "tool" } }
end

-- The child's own restore fn builds the body, fed the real is_error, so
-- a failed child looks exactly like the same tool run standalone. When
-- restore is missing, throws, or returns no buf, the ToolView fallback
-- matches the standalone plain rendering too.
local function child_body_buf(c, tol)
  local output = c.output or ""
  local t = maki.api.get_tool(c.tool)
  local buf = t and t.restore and t.restore(c.params, output, c.status == STATUS.ERROR, { tool_output_lines = tol })
  return buf or ToolView.restore(output, { max_lines = tol[c.tool] or tol.other, keep = "head" })
end

-- Parsing plus per-entry policy: entries past MAX_BATCH_SIZE and nested
-- batches are born terminal (error), so they render but never run. Only
-- a malformed entry fails the batch as a whole, and that happens before
-- anything runs.
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
    c.status = STATUS.PENDING
    if i > MAX_BATCH_SIZE then
      c.status, c.output = STATUS.ERROR, DISCARDED_ERROR
    elseif c.tool == "batch" then
      c.status, c.output = STATUS.ERROR, NESTED_ERROR
    end
    c.header = header_spans(c.tool, c.params)
    children[i] = c
  end
  return children
end

--- Rendering (pure) ---------------------------------------------------------

local function indented(line)
  local out = { { BODY_INDENT } }
  for _, s in ipairs(line) do
    out[#out + 1] = s
  end
  return out
end

local function append_separator(lines)
  lines[#lines + 1] = {}
  lines[#lines + 1] = { { SEPARATOR, "dim" } }
  lines[#lines + 1] = {}
end

local function child_header_line(c)
  local ind = INDICATOR[c.status]
  local spans = { { ind[1], ind[2] }, { c.tool .. "> ", "tool_prefix" } }
  for _, s in ipairs(c.header or {}) do
    spans[#spans + 1] = s
  end
  if c.annotation then
    spans[#spans + 1] = { " (" .. c.annotation .. ")", "tool_annotation" }
  end
  return spans
end

-- Lines and click ranges come out of the same pass, so the row -> child
-- map can never drift from what is on screen. Bodies are read fresh from
-- each child's own buf, never cached.
local function render_children(children)
  local lines, ranges = {}, {}
  for i, c in ipairs(children) do
    if i > 1 then
      append_separator(lines)
    end
    local first = #lines + 1
    lines[#lines + 1] = child_header_line(c)
    if c.buf then
      for _, bl in ipairs(c.buf:get_lines()) do
        lines[#lines + 1] = indented(bl)
      end
    end
    ranges[i] = { first = first, last = #lines }
  end
  return lines, ranges
end

-- batch_policy.rs pins this exact llm format; keep it byte-identical.
local function render_llm(children)
  local out = {}
  local total = #children
  local failed = 0
  for _, c in ipairs(children) do
    out[#out + 1] = string.format(SECTION_FMT, c.tool)
    if c.status == STATUS.SUCCESS then
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

-- Try to recover per-child results from `## tool` sections in the LLM
-- output. Returns nil when the format doesn't match.
local function children_from_llm(children, output)
  if #children == 0 then
    return nil
  end
  local sections = {}
  local body, prev
  for line in (output .. "\n"):gmatch("([^\n]*)\n") do
    local tool = line:match(SECTION_PAT)
    local nxt = children[#sections + 1]
    -- render_llm puts a blank line before every header except the first;
    -- demand the same, so a body line that merely looks like the next
    -- header stays body text.
    if tool and nxt and tool == nxt.tool and (prev == nil or prev == "") then
      body = {}
      sections[#sections + 1] = body
    elseif body then
      body[#body + 1] = line
    else
      return nil
    end
    prev = line
  end
  if #sections ~= #children then
    return nil
  end
  local last = sections[#sections]
  for _, pat in ipairs(SUMMARY_PATS) do
    if last[#last] and last[#last]:match(pat) then
      last[#last] = nil
      break
    end
  end
  local out = {}
  for i, lines in ipairs(sections) do
    while #lines > 0 and lines[#lines] == "" do
      lines[#lines] = nil
    end
    local text = table.concat(lines, "\n")
    local failed = text:sub(1, #ERROR_PREFIX) == ERROR_PREFIX
    out[i] = {
      status = failed and STATUS.ERROR or STATUS.SUCCESS,
      output = failed and text:sub(#ERROR_PREFIX + 1) or text,
    }
  end
  return out
end

--- Batch view-model ---------------------------------------------------------

local Batch = {}
Batch.__index = Batch

function Batch.new(children, tol)
  local self = setmetatable({ children = children, tol = tol }, Batch)
  self.buf = maki.ui.buf()
  self.buf:on("click", function(ev)
    self:route_click(ev and ev.row or 0)
    self:rerender()
  end)
  for _, c in ipairs(children) do
    if TERMINAL[c.status] then
      self:attach_body(c)
    end
  end
  self:rerender()
  return self
end

function Batch:rerender()
  local lines, ranges = render_children(self.children)
  self.ranges = ranges
  self.buf:set_lines(lines)
end

-- A child's buf keeps changing after we get it (async highlights land
-- later), so watch it and recompose the batch on every change.
function Batch:watch(c, buf)
  c.buf = buf
  buf:on("change", function()
    self:rerender()
  end)
end

function Batch:attach_body(c)
  self:watch(c, child_body_buf(c, self.tol))
end

-- Forward the click to the child's own buf so its real toggle logic
-- runs. A row inside a child's range becomes that child's buffer row;
-- row 0 (the header, or the expand replay on restore) and unmapped rows
-- go to every child.
function Batch:route_click(row)
  if row >= 1 then
    for i, r in ipairs(self.ranges) do
      if row >= r.first and row <= r.last then
        local c = self.children[i]
        if c.buf then
          c.buf:click({ row = row - r.first })
        end
        return
      end
    end
  end
  for _, c in ipairs(self.children) do
    if c.buf then
      c.buf:click({ row = 0 })
    end
  end
end

-- Live annotations (say, a task child's model) arrive before the done
-- annotation ("12 lines"), so append rather than replace, with the same
-- separator the standalone header uses.
function Batch:annotate(c, ann)
  c.annotation = c.annotation and (c.annotation .. ANNOTATION_SEP .. ann) or ann
  self:rerender()
end

function Batch:settle(c, status, output)
  if TERMINAL[c.status] then
    error(string.format(RESETTLE_FMT, c.tool, c.status, status))
  end
  c.status = status
  c.output = output
  self:attach_body(c)
  self:rerender()
end

function Batch:run_child(c, ctx)
  c.status = STATUS.RUNNING
  self:rerender()
  local text, err = maki.agent.call_tool(ctx, c.tool, c.params, {
    -- Clicks on a still-streaming child are a no-op: its click handler
    -- lives on the child's own handle, not on this wrapper buf.
    on_live_buf = function(b)
      self:watch(c, b)
      self:rerender()
    end,
    on_annotation = function(a)
      self:annotate(c, a)
    end,
  })
  if err then
    self:settle(c, STATUS.ERROR, err)
  else
    self:settle(c, STATUS.SUCCESS, text)
  end
end

-- gather returns early when the user cancels, so sweep whatever is
-- still non-terminal into an error; no child is left dangling.
function Batch:run(ctx)
  local funs = {}
  for _, c in ipairs(self.children) do
    if c.status == STATUS.PENDING then
      funs[#funs + 1] = function()
        self:run_child(c, ctx)
      end
    end
  end
  maki.async.gather(funs)
  for _, c in ipairs(self.children) do
    if not TERMINAL[c.status] then
      self:settle(c, STATUS.ERROR, CANCELLED_ERROR)
    end
  end
end

--- Tool entry points --------------------------------------------------------

local function handler(input, ctx)
  local children, err = prepare_children(input.tool_calls)
  if not children then
    return { llm_output = err, is_error = true }
  end
  if #children == 0 then
    return { llm_output = EMPTY_ERROR, is_error = true }
  end

  local batch = Batch.new(children, ctx:tool_output_lines())
  ctx:live_buf(batch.buf)
  batch:run(ctx)

  return {
    llm_output = render_llm(children),
    body = batch.buf,
    state = to_state(children),
  }
end

-- Last resort: no state, output isn't section-formatted.
local function legacy_restore(children, output, tol)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = tol.other, keep = "head" })
  local header = render_children(children)
  append_separator(header)
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

local function restore(input, output, _is_error, rctx)
  local tol = rctx:tool_output_lines()
  local children = prepare_children(input.tool_calls or {})
  if not children then
    return ToolView.restore(output, { max_lines = tol.other, keep = "head" })
  end

  local st = rctx:state()
  local kids = st and type(st.children) == "table" and #st.children == #children and st.children
    or children_from_llm(children, output)
  if kids then
    -- Non-terminal statuses are treated as errors (corrupt data).
    for i, sc in ipairs(kids) do
      local c = children[i]
      c.status = TERMINAL[sc.status] and sc.status or STATUS.ERROR
      c.output, c.annotation = sc.output, sc.annotation
    end
    return Batch.new(children, tol).buf
  end
  return legacy_restore(children, output, tol)
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
