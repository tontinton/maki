-- Live read-only output pane for a command job, opened over whatever float
-- is showing (the /tasks picker, usually). Piped streams stream via jobattach;
-- redirected streams (a stream sent to a file: no callbacks, no tails) are
-- followed by re-reading a window at the end of the file. Esc closes and
-- detaches. Any picker can reuse it: `JobPane.open(job)` with a joblist row.

local OUT_TAG = "out "
local ERR_TAG = "err "
local EXIT_FMT = "exited (code %d)"
local DROPPED_NOTE = "… older output not shown"
local TICK_MS = 100
-- Redirected files are re-read every FILE_TICKS ticks of the recv loop.
local FILE_TICKS = 5
-- Window read back from the end of a redirected file, and how many of its
-- lines the pane keeps per stream.
local FILE_WINDOW_BYTES = 8192
local FILE_MAX_LINES = 200
-- Once the buffer passes MAX_LINES, a redraw trims it back to KEEP_LINES, so
-- chatty jobs pay one set_lines per burst instead of one per line.
local MAX_LINES = 1000
local KEEP_LINES = 500
local CLOSE_KEYS = { esc = true, ["<C-c>"] = true }

local STREAMS = {
  {
    kind = "stdout",
    tail_key = "stdout_lines",
    path_key = "stdout_path",
    file_key = "stdout_file",
    attach_key = "on_stdout",
  },
  {
    kind = "stderr",
    tail_key = "stderr_lines",
    path_key = "stderr_path",
    file_key = "stderr_file",
    attach_key = "on_stderr",
  },
}

local JobPane = {}

JobPane.MAX_LINES = MAX_LINES
JobPane.KEEP_LINES = KEEP_LINES
JobPane.FILE_MAX_LINES = FILE_MAX_LINES

local pane = nil

-- One output line as spans: the stream tag carries the style, the text stays
-- plain. Theme roles only.
function JobPane.stream_line(kind, text)
  if kind == "stderr" then
    return { { ERR_TAG, "error" }, { text, "item" } }
  end
  return { { OUT_TAG, "dim" }, { text, "item" } }
end

function JobPane.exit_line(code)
  return { { EXIT_FMT:format(code), code == 0 and "success" or "error" } }
end

-- Lines of a redirected file's windowed content, keeping at most the last
-- {max}; nil content (file missing or unreadable yet) is empty.
function JobPane.file_lines(content, max)
  local lines = {}
  for text in (content or ""):gmatch("([^\n]*)\n?") do
    lines[#lines + 1] = text
  end
  if lines[#lines] == "" then
    lines[#lines] = nil
  end
  if #lines > max then
    local kept = {}
    for i = #lines - max + 1, #lines do
      kept[#kept + 1] = lines[i]
    end
    return kept
  end
  return lines
end

-- Per-stream texts: tail lines for a piped stream, lines of the file content
-- (already read into {info[file_key]}) for a redirected one.
function JobPane.stream_texts(info)
  local streams = {}
  for _, s in ipairs(STREAMS) do
    local texts
    if info[s.path_key] then
      texts = JobPane.file_lines(info[s.file_key], FILE_MAX_LINES)
    else
      texts = info[s.tail_key] or {}
    end
    streams[#streams + 1] = { kind = s.kind, texts = texts, path = info[s.path_key] }
  end
  return streams
end

-- The pane's starting content: the dropped note (tails lose lines past the
-- cap, and a redirected stream is a window by definition), then the streams,
-- then the exit line for a job that already finished.
function JobPane.initial_lines(info)
  local lines = {}
  if info.dropped_output then
    lines[#lines + 1] = { { DROPPED_NOTE, "dim" } }
  end
  for _, s in ipairs(JobPane.stream_texts(info)) do
    for _, text in ipairs(s.texts) do
      lines[#lines + 1] = JobPane.stream_line(s.kind, text)
    end
  end
  if info.exit_code then
    lines[#lines + 1] = JobPane.exit_line(info.exit_code)
  end
  return lines
end

-- Last KEEP_LINES of {lines} once it outgrows MAX_LINES, else nil (nothing to
-- do).
function JobPane.capped(lines)
  if #lines <= MAX_LINES then
    return nil
  end
  local kept = {}
  for i = #lines - KEEP_LINES + 1, #lines do
    kept[#kept + 1] = lines[i]
  end
  return kept
end

local function append(kind, text)
  local this = pane
  if not this then
    return
  end
  local line = JobPane.stream_line(kind, text)
  this.lines[#this.lines + 1] = line
  local capped = JobPane.capped(this.lines)
  if capped then
    this.lines = capped
    this.buf:set_lines(capped)
  else
    this.buf:line(line)
  end
  this.win:set_cursor(this.buf:len())
end

local function append_exit(code)
  local this = pane
  if not this then
    return
  end
  local line = JobPane.exit_line(code)
  this.lines[#this.lines + 1] = line
  this.buf:line(line)
  this.win:set_cursor(this.buf:len())
end

-- Re-read each redirected file and append only the lines past what the pane
-- already showed. A file that shrank (rotated, truncated) resyncs instead of
-- replaying.
local function follow_files(this)
  for kind, file in pairs(this.files) do
    local content = maki.fs.read(file.path, { offset = -FILE_WINDOW_BYTES })
    local texts = JobPane.file_lines(content, FILE_MAX_LINES)
    if #texts < file.shown then
      file.shown = #texts
    end
    for i = file.shown + 1, #texts do
      append(kind, texts[i])
    end
    file.shown = #texts
  end
end

-- Only exit. If the job is still running, clearing the callbacks detaches us;
-- the owner plugin re-adopts its streams on its next reload, and until then
-- the tails keep recording. Redirected streams were never attached, so their
-- keys stay absent (jobattach leaves those callbacks alone).
local function finish()
  local this = pane
  if not this then
    return
  end
  pane = nil
  if not this.exited then
    local detach = { session = this.job.session, on_exit = false }
    for _, s in ipairs(STREAMS) do
      if not this.files[s.kind] then
        detach[s.attach_key] = false
      end
    end
    maki.fn.jobattach(this.job.id, detach)
  end
  if this.win:is_open() then
    this.win:close()
  end
end

-- Opens over the current float and takes focus; the float manager hands focus
-- back to the picker underneath when the pane closes, so the picker keeps its
-- loop and selection.
function JobPane.open(job)
  if pane then
    return
  end
  local opts = { session = job.session }
  local info, err = maki.fn.jobinfo(job.id, opts)
  if err then
    maki.ui.flash(err)
    return
  end

  for _, s in ipairs(STREAMS) do
    if info[s.path_key] then
      -- Missing file (job not started writing yet) reads as empty.
      info[s.file_key] = maki.fs.read(info[s.path_key], { offset = -FILE_WINDOW_BYTES })
    end
  end

  local lines = JobPane.initial_lines(info)

  local buf = maki.ui.buf({ scratch = true })
  buf:set_lines(lines)
  local win = maki.ui.open_win(buf, {
    title = " " .. (job.name or job.command) .. " ",
    width = "70%",
    height = "70%",
    border = "rounded",
    footer = { { "Esc", "close" } },
  })
  win:set_cursor(buf:len())

  local this = {
    job = job,
    buf = buf,
    win = win,
    lines = lines,
    exited = info.exit_code ~= nil,
    files = {},
    ticks = 0,
  }
  for _, s in ipairs(JobPane.stream_texts(info)) do
    if s.path then
      this.files[s.kind] = { path = s.path, shown = #s.texts }
    end
  end
  pane = this

  -- A job that exits between jobinfo and attach replays its exit here, so the
  -- pane never waits on a line that already happened. Piped streams only:
  -- redirected ones are followed by reading their file instead.
  local attach = { session = job.session }
  for _, s in ipairs(STREAMS) do
    if not this.files[s.kind] then
      local kind = s.kind
      attach[s.attach_key] = function(_, line)
        append(kind, line)
      end
    end
  end
  attach.on_exit = function(_, code)
    this.exited = true
    append_exit(code)
  end
  local _, attach_err = maki.fn.jobattach(job.id, attach)
  if attach_err then
    pane = nil
    maki.ui.flash(attach_err)
    win:close()
    return
  end

  while pane == this do
    local ev = win:recv(TICK_MS)
    this.ticks = this.ticks + 1
    if this.ticks % FILE_TICKS == 0 then
      follow_files(this)
    end
    if not ev or ev.type == "close" then
      finish()
    elseif ev.type == "key" and CLOSE_KEYS[ev.key] then
      finish()
    end
  end
end

return JobPane
