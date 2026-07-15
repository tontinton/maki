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

function ToolView.new(buf, opts)
  local self = setmetatable({}, ToolView)
  self.buf = buf
  self.max = (opts and opts.max_lines) or 80
  self.keep = (opts and opts.keep) or "tail"
  self.max_expand_lines = (opts and opts.max_expand_lines) or 2000
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

function ToolView:set_highlight(content, ext)
  ext = ext or "md"
  if content:sub(-1) == "\n" then
    content = content:sub(1, -2)
  end
  if content == "" then
    return false
  end
  local lines = {}
  for line in (content .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end

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
    lines[#lines + 1] = h
  end

  if self.expanded then
    for _, line in ipairs(self.all_lines) do
      lines[#lines + 1] = line
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
      lines[#lines + 1] = notice
    end

    for i = 0, self.ring_count - 1 do
      -- Modulo only after the ring wrapped: `x % math.huge` is NaN in Luau,
      -- and uncapped views (max_lines = math.huge) never wrap.
      local idx = self.ring_start == 1 and (i + 1) or (((self.ring_start - 1 + i) % self.max) + 1)
      lines[#lines + 1] = self.ring[idx]
    end

    if self.keep == "head" and notice then
      lines[#lines + 1] = notice
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

function ToolView:finish()
  if self.keep == "head" and self.skipped > 0 then
    self:flush()
  end
end

function ToolView.restore(output, opts)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, opts)
  for line in (output .. "\n"):gmatch("([^\n]*)\n") do
    view:append(line)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

return ToolView
