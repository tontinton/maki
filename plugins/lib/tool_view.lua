local ToolView = {}
ToolView.__index = ToolView

function ToolView.new(buf, opts)
  local self = setmetatable({}, ToolView)
  self.buf = buf
  self.max = (opts and opts.max_lines) or 80
  self.keep = (opts and opts.keep) or "tail"
  self.header = {}
  self.ring = {}
  self.ring_start = 1
  self.ring_count = 0
  self.skipped = 0
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
  self:flush()
end

function ToolView:append(line)
  if self.keep == "head" then
    if self.ring_count < self.max then
      self.ring_count = self.ring_count + 1
      self.ring[self.ring_count] = line
      self:flush()
    else
      self.skipped = self.skipped + 1
    end
  else
    if self.ring_count < self.max then
      self.ring_count = self.ring_count + 1
      self.ring[self.ring_count] = line
    else
      self.ring[self.ring_start] = line
      self.ring_start = (self.ring_start % self.max) + 1
      self.skipped = self.skipped + 1
    end
    self:flush()
  end
end

function ToolView:flush()
  local lines = {}

  for _, h in ipairs(self.header) do
    lines[#lines + 1] = h
  end

  if self.keep == "tail" and self.skipped > 0 then
    lines[#lines + 1] = { { self.skipped .. " lines hidden", "dim" } }
  end

  if self.keep == "tail" and self.ring_count == self.max then
    for i = 0, self.ring_count - 1 do
      local idx = ((self.ring_start - 1 + i) % self.max) + 1
      lines[#lines + 1] = self.ring[idx]
    end
  else
    for i = 1, self.ring_count do
      lines[#lines + 1] = self.ring[i]
    end
  end

  if self.keep == "head" and self.skipped > 0 then
    lines[#lines + 1] = { { self.skipped .. " lines hidden", "dim" } }
  end

  self.buf:set_lines(lines)
end

function ToolView:finish()
  if self.keep == "head" and self.skipped > 0 then
    self:flush()
  end
end

return ToolView
