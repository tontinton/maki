-- The on-disk output of a bash call that outgrew its reply or moved to the
-- background. Writes are batched and serialized so output callbacks rarely
-- wait on the disk, and the file rotates so a dev server cannot fill it.

local ROTATED_SUFFIX = ".1"
local UTF8_CONTINUATION = "^[\128-\191]"
-- Queued output beyond this makes the pusher write it itself, so a writer that
-- is stuck or slow cannot let the queue grow without bound.
local PENDING_LIMIT_BYTES = 1024 * 1024

local M = {}

local Log = {}
Log.__index = Log

--- Creates the log file (and its directories), emptying any older file there.
--- Returns the log, or nil plus an error message.
function M.open(path, max_bytes)
  local made, err = maki.fs.mkdir(maki.fs.dirname(path), { parents = true })
  if not made then
    return nil, err
  end
  local emptied, write_err = maki.fs.write(path, "")
  if not emptied then
    return nil, write_err
  end
  return setmetatable({
    path = path,
    max_bytes = max_bytes,
    size = 0,
    pending = {},
    pending_bytes = 0,
    draining = false,
    broken = false,
    rotated_path = nil,
    truncated = false,
    gate = maki.async.semaphore(1),
  }, Log)
end

function Log:fail(action, err)
  self.broken = true
  self.pending = {}
  self.pending_bytes = 0
  maki.log.warn(("bash: cannot %s %s: %s"):format(action, self.path, tostring(err)))
end

-- A reload cancels the writer where it stands, and its flag would keep every
-- later push from starting a new one.
function Log:write_behind()
  maki.async.on_cancel(function()
    self.draining = false
  end)
  local ok, err = pcall(self.drain, self)
  self.draining = false
  if not ok then
    maki.log.warn(("bash: writing %s failed: %s"):format(self.path, tostring(err)))
  end
end

--- Queues {text} for the file. A task writes it behind the caller, which only
--- waits for the disk (and so yields) when the queue is over its limit or no
--- task can be started.
function Log:push(text)
  if self.broken then
    return
  end
  self.pending[#self.pending + 1] = text
  self.pending_bytes = self.pending_bytes + #text
  if self.pending_bytes > PENDING_LIMIT_BYTES then
    self:drain()
  elseif not self.draining then
    self.draining = true
    local started = pcall(maki.async.spawn, function()
      self:write_behind()
    end)
    if not started then
      self.draining = false
      self:drain()
    end
  end
end

--- The previous log is kept as `<path>.1`, replacing the one before it, and a
--- fresh log starts. Without a rename the old file is truncated instead, which
--- loses it but still holds the cap.
function Log:rotate()
  local renamed, err = maki.fs.rename(self.path, self.path .. ROTATED_SUFFIX)
  if renamed then
    self.rotated_path = self.path .. ROTATED_SUFFIX
  else
    self.truncated = true
    maki.log.warn(("bash: cannot rotate %s: %s"):format(self.path, tostring(err)))
  end
  local emptied, write_err = maki.fs.write(self.path, "")
  if not emptied then
    self:fail("empty", write_err)
    return
  end
  self.size = 0
end

--- Writes everything queued, returning once the file holds it all. Output
--- stays queued until written, so a drain cut short by a reload loses none.
function Log:drain()
  local permit = self.gate:acquire()
  local ok, err = pcall(function()
    while #self.pending > 0 and not self.broken do
      local count = #self.pending
      local chunk = table.concat(self.pending, "", 1, count)
      local appended, append_err = maki.fs.append(self.path, chunk)
      if not appended then
        self:fail("write", append_err)
        return
      end
      self.pending = table.move(self.pending, count + 1, #self.pending, 1, {})
      self.pending_bytes = self.pending_bytes - #chunk
      self.size = self.size + #chunk
      if self.size > self.max_bytes then
        self:rotate()
      end
    end
  end)
  permit:release()
  if not ok then
    error(err, 0)
  end
end

--- Keeps the last lines of the output in memory, whatever its total size.
--- {dropped} counts lines already gone before the first one pushed.
function M.new_tail(max_lines, max_bytes, dropped)
  local first = dropped + 1
  return { lines = {}, first = first, last = first - 1, bytes = 0, max_lines = max_lines, max_bytes = max_bytes }
end

local function utf8_prefix(line, max_bytes)
  local cut = max_bytes
  while cut > 0 and line:find(UTF8_CONTINUATION, cut + 1) do
    cut = cut - 1
  end
  return line:sub(1, cut)
end

function M.tail_push(tail, line)
  if #line > tail.max_bytes then
    line = utf8_prefix(line, tail.max_bytes)
  end
  tail.last = tail.last + 1
  tail.lines[tail.last] = line
  tail.bytes = tail.bytes + #line + 1
  while tail.last - tail.first >= tail.max_lines or (tail.bytes > tail.max_bytes and tail.first < tail.last) do
    tail.bytes = tail.bytes - #tail.lines[tail.first] - 1
    tail.lines[tail.first] = nil
    tail.first = tail.first + 1
  end
end

--- How many lines the tail has pushed out so far.
function M.tail_dropped(tail)
  return tail.first - 1
end

--- The last {count} lines (all of them when nil) and how many that is.
function M.tail_text(tail, count)
  local from = count and math.max(tail.first, tail.last - count + 1) or tail.first
  return table.concat(tail.lines, "\n", from, tail.last), tail.last - from + 1
end

return M
