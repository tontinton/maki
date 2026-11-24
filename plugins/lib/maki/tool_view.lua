--!strict
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

-- A span is plain text or `{ text, style }`, a line is plain text or a list of spans.
export type Span = string | { string }
export type Line = string | { Span }
export type Opts = {
  max_lines: number?,
  keep: ("head" | "tail")?,
  max_expand_lines: number?,
  max_line_bytes: number?,
  width: number?,
}
type Buf = {
  set_lines: (self: Buf, lines: { Line }, opts: { head: number }?) -> (),
  on: (self: Buf, event: string, handler: () -> ()) -> (),
}
type Fields = {
  buf: Buf,
  max: number,
  keep: "head" | "tail",
  max_expand_lines: number,
  max_line_bytes: number?,
  header: { Line },
  ring: { Line },
  ring_start: number,
  ring_count: number,
  skipped: number,
  all_lines: { Line },
  all_skipped: number,
  expanded: boolean,
  ring_map: { [number]: number },
}
export type ToolView = setmetatable<Fields, typeof(ToolView)>

-- Worst case is a batch child: tool body + batch body indents (2 + 2),
-- plus the transcript's scrollbar column.
local BODY_CHROME_COLS = 5
local MIN_MD_WIDTH = 20
local SEPARATOR: Line = { { "──────", "dim" } }

local function format_line_nr(fmt: string, idx: number): Span
  return { string.format(fmt, idx), "line_nr" }
end

-- Right aligned, so the content column stays put when a number gains a digit.
-- Formats the number alone, callers add their own separator.
function ToolView.line_nr_fmt(max_line_nr: number): string
  return "%" .. #tostring(max_line_nr) .. "d"
end

-- Truncate a UTF-8 string at a byte boundary that is also a codepoint
-- boundary, appending an ellipsis if truncation happened.
local function utf8_truncate_bytes(s: string, max_bytes: number): string
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

local function line_text_bytes(line: Line): number
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

local function truncate_line(line: Line, max_bytes: number?): Line
  if not max_bytes or max_bytes <= 0 then
    return line
  end
  if type(line) == "string" then
    return utf8_truncate_bytes(line, max_bytes)
  end
  if line_text_bytes(line) <= max_bytes then
    return line
  end
  local out: { Span } = {}
  local used = 0
  for _, span in ipairs(line) do
    if used >= max_bytes then
      break
    end
    local text: string, style: string?
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
-- max_line_bytes (optional) per-line byte cap. Lines are stored already cut,
-- so a run of huge lines costs no more than short ones while kept for expansion.
-- With max_lines 0 only the set_header lines show until a click expands the
-- view. The UI puts the line count in the tool header, so the row still says
-- how much is hidden.
function ToolView.new(buf: Buf, opts: Opts?): ToolView
  return setmetatable({
    buf = buf,
    max = (opts and opts.max_lines) or 80,
    keep = (opts and opts.keep) or "tail",
    max_expand_lines = (opts and opts.max_expand_lines) or 2000,
    max_line_bytes = opts and opts.max_line_bytes,
    header = {},
    ring = {},
    ring_start = 1,
    ring_count = 0,
    skipped = 0,
    all_lines = {},
    all_skipped = 0,
    expanded = false,
    ring_map = {},
  }, ToolView)
end

function ToolView.set_header(self: ToolView, lines: { Line })
  self.header = lines
  self:flush()
end

function ToolView.clear(self: ToolView)
  self.ring = {}
  self.ring_start = 1
  self.ring_count = 0
  self.skipped = 0
  self.all_lines = {}
  self.all_skipped = 0
  self.ring_map = {}
  self:flush()
end

function ToolView.append(self: ToolView, line: Line)
  line = truncate_line(line, self.max_line_bytes)
  local all_idx
  if #self.all_lines < self.max_expand_lines then
    self.all_lines[#self.all_lines + 1] = line
    all_idx = #self.all_lines
  else
    self.all_skipped = self.all_skipped + 1
  end

  if self.ring_count < self.max then
    self.ring_count = self.ring_count + 1
    self.ring[self.ring_count] = line
    if all_idx then
      self.ring_map[self.ring_count] = all_idx
    end
  elseif self.keep == "head" or self.max == 0 then
    self.skipped = self.skipped + 1
    if not self.expanded then
      return
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

function ToolView.append_text(self: ToolView, text: string)
  for _, line in ipairs(maki.split(text, "\n")) do
    self:append(line)
  end
end

-- Append {content} with line numbers, then syntax-highlight it for {ext}
-- asynchronously. Returns false when {content} is empty.
function ToolView.set_highlight(self: ToolView, content: string, ext: string?): boolean
  local lang = ext or "md"
  if content:sub(-1) == "\n" then
    content = content:sub(1, -2)
  end
  if content == "" then
    return false
  end
  local lines = maki.split(content, "\n")

  local fmt = ToolView.line_nr_fmt(#lines) .. " "
  for idx, line in ipairs(lines) do
    self:append({ format_line_nr(fmt, idx), { line } })
  end

  maki.async.run(function()
    local highlighted = maki.ui.highlight(content, lang)
    if not highlighted then
      return
    end
    for idx, hl_line in ipairs(highlighted :: { { Span } }) do
      if not self.all_lines[idx] then
        break
      end
      local spans: { Span } = { format_line_nr(fmt, idx) }
      for _, seg in ipairs(hl_line) do
        spans[#spans + 1] = seg
      end
      self:update_line(idx, spans)
    end
    self:flush()
  end)

  return true
end

-- Content rows on screen, for callers with their own per-row click targets. A
-- single hidden line is drawn as itself instead of a notice, so it counts as
-- content too. Rows line up with `all_lines` under keep = "head"; keep = "tail"
-- prints its notice first and shifts them.
function ToolView.visible_count(self: ToolView): number
  if self.expanded then
    return #self.all_lines
  end
  return self.ring_count + (self.max > 0 and self.skipped == 1 and 1 or 0)
end

function ToolView.toggle(self: ToolView)
  self.expanded = not self.expanded
  self:flush()
end

function ToolView.flush(self: ToolView)
  local lines: { Line } = {}

  for _, h in ipairs(self.header) do
    lines[#lines + 1] = truncate_line(h, self.max_line_bytes)
  end
  if #self.header > 0 and (self.expanded or self.max > 0) then
    lines[#lines + 1] = SEPARATOR
  end

  if self.expanded then
    for _, line in ipairs(self.all_lines) do
      lines[#lines + 1] = truncate_line(line, self.max_line_bytes)
    end
    if self.all_skipped > 0 then
      lines[#lines + 1] = { { self.all_skipped .. " lines omitted", "dim" } }
    end
  elseif self.max > 0 then
    local hidden = self.skipped
    local notice: Line? = if hidden >= 2
      then { { "... (" .. hidden .. " lines) (click to expand)", "dim" } }
      elseif hidden == 1 then self.all_lines[if self.keep == "tail" then 1 else self.ring_count + 1]
      else nil

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

  -- The header says what runs, so a row that moved to the background keeps
  -- it. The separator stays out: with the output gone it divides nothing.
  self.buf:set_lines(lines, { head = #self.header })
end

function ToolView.update_line(self: ToolView, all_idx: number, line: Line)
  line = truncate_line(line, self.max_line_bytes)
  self.all_lines[all_idx] = line
  for ri = 1, self.ring_count do
    if self.ring_map[ri] == all_idx then
      self.ring[ri] = line
      return
    end
  end
end

-- Call once after the last append so the collapsed notice renders.
function ToolView.finish(self: ToolView)
  if (self.keep == "head" or self.max == 0) and self.skipped > 0 then
    self:flush()
  end
end

function ToolView.restore_lines(lines: { Line }, opts: Opts?): Buf
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
function ToolView.restore(output: string, opts: Opts?): Buf
  return ToolView.restore_lines(maki.split(output, "\n"), opts)
end

-- Same, for tools whose live output goes through markdown (`format =
-- "markdown"`). {opts.width} is the wrap width and defaults to the body
-- column of the current terminal. Errors stay plain, as they do live.
function ToolView.restore_markdown(output: string, is_error: boolean?, opts: Opts): Buf
  if not is_error then
    local width = opts.width or math.max(maki.ui.terminal_size().cols - BODY_CHROME_COLS, MIN_MD_WIDTH)
    local ok, md_lines = pcall(maki.ui.markdown, output, width)
    if ok then
      return ToolView.restore_lines(md_lines, opts)
    end
  end
  return ToolView.restore(output, opts)
end

return ToolView
