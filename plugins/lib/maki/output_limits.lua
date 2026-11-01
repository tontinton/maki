-- Shared per-tool output limit options, so the tools that support them
-- cannot drift apart.

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local DEFAULT_MAX_LINE_BYTES = 500
-- `MAX_READ_BYTES` in maki-lua/src/api/fs.rs, the most one `maki.fs` read
-- returns. A line also costs a table slot and a string header, so full mode
-- caps the line count too, at about 64 bytes of heap per kept line.
local FULL_OUTPUT_MAX_BYTES = 64 * 1024 * 1024
local FULL_OUTPUT_MAX_LINES = FULL_OUTPUT_MAX_BYTES // 64
local UTF8_CONTINUATION = "^[\128-\191]"

local M = {}

M.DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_LINE_BYTES
M.FULL_OUTPUT_MAX_BYTES = FULL_OUTPUT_MAX_BYTES
M.FULL_OUTPUT_MAX_LINES = FULL_OUTPUT_MAX_LINES
--- Appended where full-mode output passed the ceilings, which callers that
--- promised the whole output report as an error.
M.FULL_LIMIT_NOTE = "[output passed the full-mode limit; redirect it to a file to keep all of it]"
M.specs = {
  max_output_lines = { type = "integer", desc = "Override `agent.max_output_lines` for this tool." },
  max_output_bytes = { type = "integer", desc = "Override `agent.max_output_bytes` for this tool." },
}

function M.extend(spec)
  for name, s in pairs(M.specs) do
    spec[name] = s
  end
  return spec
end

--- Returns max_lines, max_bytes: tool override when set, agent-wide otherwise.
--- Both nil in full mode.
function M.resolve(opts, ctx)
  if ctx:output_mode() == "full" then
    return nil, nil
  end
  return opts.max_output_lines or ctx:config("max_output_lines", DEFAULT_MAX_OUTPUT_LINES),
    opts.max_output_bytes or ctx:config("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES)
end

--- opts.max_line_bytes, or 0 (no limit) in full mode.
function M.line_bytes(opts, ctx)
  return ctx:output_mode() == "full" and 0 or opts.max_line_bytes
end

local function utf8_prefix(line, max_bytes)
  local cut = max_bytes
  while cut > 0 and line:find(UTF8_CONTINUATION, cut + 1) do
    cut = cut - 1
  end
  return line:sub(1, cut)
end

-- Each end gets half of every limit, the head taking the odd one. An
-- unlimited line budget stays unlimited on both ends: `math.huge - math.huge`
-- is NaN, and no line count is below NaN, so the tail would come out empty.
local function budgets(max_lines, max_bytes)
  local head_lines, head_bytes = math.ceil(max_lines / 2), math.ceil(max_bytes / 2)
  local tail_lines = max_lines == math.huge and math.huge or max_lines - head_lines
  return head_lines, head_bytes, tail_lines, max_bytes - head_bytes
end

-- How many of {lines} the head keeps, and its text. A first line too long for
-- the head still opens it, cut to a UTF-8 prefix.
local function take_head(lines, head_lines, head_bytes)
  local head_end, bytes = 0, 0
  while head_end < head_lines and head_end < #lines and bytes + #lines[head_end + 1] + 1 <= head_bytes do
    head_end = head_end + 1
    bytes = bytes + #lines[head_end] + 1
  end
  if head_end == 0 then
    return 1, utf8_prefix(lines[1], head_bytes)
  end
  return head_end, table.concat(lines, "\n", 1, head_end)
end

local Collector = {}
Collector.__index = Collector

--- Cuts streamed lines the way `cut_middle` cuts a whole text, holding no
--- more than the limits while they arrive. nil limits fall back to the
--- full-mode ceilings, so even full mode stops growing at 64 MiB. Lines go in
--- with `collector:append(line, bytes)` and the text comes out of
--- `collector:result()`.
function M.collector(max_lines, max_bytes)
  return setmetatable({
    max_lines = max_lines or FULL_OUTPUT_MAX_LINES,
    max_bytes = max_bytes or FULL_OUTPUT_MAX_BYTES,
    -- Every line while the output fits. Once it does not, the head is fixed
    -- and only the tail window keeps changing.
    lines = {},
    line_count = 0,
    byte_count = 0,
  }, Collector)
end

local function push_tail(self, line)
  local tail = self.tail
  self.tail_last = self.tail_last + 1
  tail[self.tail_last] = line
  self.tail_used = self.tail_used + #line + 1
  -- The tail is the longest suffix within both tail budgets, so the oldest
  -- lines leave until the rest fits.
  while
    self.tail_first <= self.tail_last
    and (self.tail_last - self.tail_first + 1 > self.tail_lines or self.tail_used > self.tail_bytes)
  do
    self.tail_used = self.tail_used - #tail[self.tail_first] - 1
    tail[self.tail_first] = nil
    self.tail_first = self.tail_first + 1
  end
end

local function start_cut(self)
  local lines = self.lines
  local head_lines, head_bytes, tail_lines, tail_bytes = budgets(self.max_lines, self.max_bytes)
  self.head_end, self.head = take_head(lines, head_lines, head_bytes)
  self.tail_lines, self.tail_bytes = tail_lines, tail_bytes
  self.tail, self.tail_first, self.tail_last, self.tail_used = {}, 1, 0, 0
  self.lines = nil
  for i = self.head_end + 1, #lines do
    push_tail(self, lines[i])
  end
end

-- What one more line of {bytes} adds to the output, its newline included.
local function added_bytes(self, bytes)
  return bytes + (self.line_count > 0 and 1 or 0)
end

--- Whether one more line of {bytes} still leaves every line kept, so a
--- caller can save the output elsewhere before the collector drops any.
function Collector:fits(bytes)
  return not self.head
    and self.line_count < self.max_lines
    and self.byte_count + added_bytes(self, bytes) <= self.max_bytes
end

--- Adds {line}. {bytes} is its length before a reader cut it, the fourth
--- argument of a `jobstart` line callback, so the marker counts what the cut
--- dropped too. Defaults to `#line`.
function Collector:append(line, bytes)
  bytes = bytes or #line
  local fits = self:fits(bytes)
  self.byte_count = self.byte_count + added_bytes(self, bytes)
  self.line_count = self.line_count + 1
  if self.head then
    push_tail(self, line)
    return
  end
  self.lines[#self.lines + 1] = line
  if not fits then
    start_cut(self)
  end
end

--- Whether the collector has dropped anything, so a caller that promised
--- the whole output can say it could not keep it.
function Collector:truncated()
  return self.head ~= nil
end

--- The newest lines kept with no gap between them, and how many lines came
--- before them: every line until the collector cuts, its tail window after.
function Collector:newest()
  if not self.head then
    return table.move(self.lines, 1, #self.lines, 1, {}), 0
  end
  local newest = table.move(self.tail, self.tail_first, self.tail_last, 1, {})
  return newest, self.line_count - #newest
end

function Collector:result()
  if not self.head then
    return table.concat(self.lines, "\n")
  end
  local kept = self.tail_last - self.tail_first + 1
  local tail = table.concat(self.tail, "\n", self.tail_first, self.tail_last)
  local dropped_lines = self.line_count - self.head_end - kept
  local dropped_bytes = self.byte_count - #self.head - #tail
  local marker = "[truncated " .. dropped_lines .. " lines, " .. dropped_bytes .. " bytes]"
  if kept == 0 then
    return self.head .. "\n\n" .. marker
  end
  return self.head .. "\n\n" .. marker .. "\n\n" .. tail
end

--- Cuts {text} to the limits keeping its head and its tail, since on a long
--- build or test run the errors and the summary sit at the end. Each end gets
--- half of every limit. A nil limit is unlimited. The marker between the two
--- notes how many lines and bytes it dropped.
function M.cut_middle(text, max_lines, max_bytes)
  if not max_lines and not max_bytes then
    return text
  end
  -- Line by line, so a long text never sits in a table of its lines.
  local collector = M.collector(max_lines or math.huge, max_bytes or #text)
  local pos = 1
  while true do
    local newline = text:find("\n", pos, true)
    if not newline then
      collector:append(text:sub(pos))
      return collector:result()
    end
    collector:append(text:sub(pos, newline - 1))
    pos = newline + 1
  end
end

return M
