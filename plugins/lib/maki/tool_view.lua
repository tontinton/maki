-- The shared truncate/expand body that tool plugins render through.
--
-- Click handlers get `ev.row`, a 1-based line in this buf; 0 means the
-- click landed outside it (the header). The handler lives on the buf
-- itself, so any wrapper of the same buf (a batch child's foreign handle)
-- reaches the same toggle. Expansion is never stored: the UI records
-- clicked rows and replays them through `restore` in order, so `toggle`
-- stays a pure flag flip + re-render, deterministic across replays.
-- Async highlighting goes through `maki.async.run`; during restore the
-- runtime runs those tasks inline before snapshotting.
local ToolView = {}
ToolView.__index = ToolView

local function format_line_nr(fmt, idx)
  return { string.format(fmt, idx), "line_nr" }
end

local function line_nr_fmt(count)
  local w = math.max(1, math.floor(math.log(count, 10)) + 1)
  return "%" .. w .. "d "
end

-- Truncate a UTF-8 string at a byte boundary that is also a codepoint
-- boundary, appending an ellipsis if truncation happened.
local function utf8_truncate_bytes(s, max_bytes)
  if #s <= max_bytes then
    return s
  end
  if max_bytes <= 0 then
    return ""
  end
  local i = max_bytes
  while i > 0 do
    local next_b = s:byte(i + 1)
    if not next_b or next_b < 0x80 or next_b >= 0xC0 then
      break
    end
    i = i - 1
  end
  if i <= 0 then
    return "…"
  end
  return s:sub(1, i) .. "…"
end

local function line_text_bytes(line)
  if type(line) == "string" then
    return #line
  end
  local n = 0
  for _, span in ipairs(line) do
    if type(span) == "string" then
      n = n + #span
    else
      n = n + #(span[1] or "")
    end
  end
  return n
end

local function truncate_line(line, max_bytes)
  if not max_bytes or max_bytes <= 0 then
    return line
  end
  if type(line) == "string" then
    return utf8_truncate_bytes(line, max_bytes)
  end
  if line_text_bytes(line) <= max_bytes then
    return line
  end
  local out = {}
  local used = 0
  for _, span in ipairs(line) do
    if used >= max_bytes then
      break
    end
    local text, style
    if type(span) == "string" then
      text = span
    else
      text = span[1] or ""
      style = span[2]
    end
    local remaining = max_bytes - used
    if #text > remaining then
      text = utf8_truncate_bytes(text, remaining)
    end
    if style then
      out[#out + 1] = { text, style }
    else
      out[#out + 1] = text
    end
    used = used + #text
  end
  return out
end

-- opts: max_lines (default 80) shown while collapsed, keep "head"|"tail"
-- (default "tail"), max_expand_lines (default 2000) kept for expansion,
-- max_line_bytes (optional) per-line byte cap applied at render time.
function ToolView.new(buf, opts)
  local self = setmetatable({}, ToolView)
  self.buf = buf
  self.max = (opts and opts.max_lines) or 80
  self.keep = (opts and opts.keep) or "tail"
  self.max_expand_lines = (opts and opts.max_expand_lines) or 2000
  self.max_line_bytes = (opts and opts.max_line_bytes) or nil
  self.header = {}
  self.ring = {}
  self.ring_start = 1
  self.ring_count = 0
  self.skipped = 0
  self.all_lines = {}
  self.all_skipped = 0
  self.expanded = false
  self.ring_map = {}
  return self
end

function ToolView:set_header(lines)
  self.header = lines
  self:flush()
end

function ToolView:clear()
  self.ring = {}
  self.ring_start = 1
  self.ring_count = 0
  self.skipped = 0
  self.all_lines = {}
  self.all_skipped = 0
  self.ring_map = {}
  self:flush()
end

function ToolView:append(line)
  local all_idx
  if #self.all_lines < self.max_expand_lines then
    self.all_lines[#self.all_lines + 1] = line
    all_idx = #self.all_lines
  else
    self.all_skipped = self.all_skipped + 1
  end

  if self.keep == "head" then
    if self.ring_count < self.max then
      self.ring_count = self.ring_count + 1
      self.ring[self.ring_count] = line
      if all_idx then
        self.ring_map[self.ring_count] = all_idx
      end
      self:flush()
    else
      self.skipped = self.skipped + 1
    end
  else
    if self.ring_count < self.max then
      self.ring_count = self.ring_count + 1
      self.ring[self.ring_count] = line
      if all_idx then
        self.ring_map[self.ring_count] = all_idx
      end
    else
      self.ring[self.ring_start] = line
      if all_idx then
        self.ring_map[self.ring_start] = all_idx
      end
      self.ring_start = (self.ring_start % self.max) + 1
      self.skipped = self.skipped + 1
    end
    self:flush()
  end
end

function ToolView:append_text(text)
  for _, line in ipairs(maki.split(text, "\n")) do
    self:append(line)
  end
end

-- Append {content} with line numbers, then syntax-highlight it for {ext}
-- asynchronously. Returns false when {content} is empty.
function ToolView:set_highlight(content, ext)
  ext = ext or "md"
  if content:sub(-1) == "\n" then
    content = content:sub(1, -2)
  end
  if content == "" then
    return false
  end
  local lines = maki.split(content, "\n")

  local fmt = line_nr_fmt(#lines)
  for idx, line in ipairs(lines) do
    self:append({ format_line_nr(fmt, idx), { line } })
  end

  maki.async.run(function()
    local highlighted = maki.ui.highlight(content, ext)
    if not highlighted then
      return
    end
    for idx, hl_line in ipairs(highlighted) do
      if not self.all_lines[idx] then
        break
      end
      local spans = { format_line_nr(fmt, idx) }
      for _, seg in ipairs(hl_line) do
        spans[#spans + 1] = seg
      end
      self:update_line(idx, spans)
    end
    self:flush()
  end)

  return true
end

function ToolView:toggle()
  self.expanded = not self.expanded
  self:flush()
end

function ToolView:flush()
  local lines = {}

  for _, h in ipairs(self.header) do
    lines[#lines + 1] = truncate_line(h, self.max_line_bytes)
  end

  if self.expanded then
    for _, line in ipairs(self.all_lines) do
      lines[#lines + 1] = truncate_line(line, self.max_line_bytes)
    end
    if self.all_skipped > 0 then
      lines[#lines + 1] = { { self.all_skipped .. " lines omitted", "dim" } }
    end
  else
    local hidden = self.skipped
    local notice = hidden >= 2 and { { "... (" .. hidden .. " lines) (click to expand)", "dim" } }
      or hidden == 1 and self.all_lines[self.keep == "tail" and 1 or self.ring_count + 1]
      or nil

    if self.keep == "tail" and notice then
      lines[#lines + 1] = truncate_line(notice, self.max_line_bytes)
    end

    for i = 0, self.ring_count - 1 do
      -- Modulo only after the ring wrapped: `x % math.huge` is NaN in Luau,
      -- and uncapped views (max_lines = math.huge) never wrap.
      local idx = self.ring_start == 1 and (i + 1) or (((self.ring_start - 1 + i) % self.max) + 1)
      lines[#lines + 1] = truncate_line(self.ring[idx], self.max_line_bytes)
    end

    if self.keep == "head" and notice then
      lines[#lines + 1] = truncate_line(notice, self.max_line_bytes)
    end
  end

  self.buf:set_lines(lines)
end

function ToolView:update_line(all_idx, line)
  self.all_lines[all_idx] = line
  for ri = 1, self.ring_count do
    if self.ring_map[ri] == all_idx then
      self.ring[ri] = line
      return
    end
  end
end

-- Call once after the last append so the collapsed notice renders.
function ToolView:finish()
  if self.keep == "head" and self.skipped > 0 then
    self:flush()
  end
end

function ToolView.restore_lines(lines, opts)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, opts)
  for _, line in ipairs(lines) do
    view:append(line)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

-- Rebuild a collapsed view from a tool's saved llm_output, click-to-toggle
-- wired. For `restore` hooks.
function ToolView.restore(output, opts)
  return ToolView.restore_lines(maki.split(output, "\n"), opts)
end

return ToolView
