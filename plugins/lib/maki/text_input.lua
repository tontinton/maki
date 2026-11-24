--!strict
-- TextInput: multi-line editable buffer with a byte-offset cursor.
--
-- Invariants enforced everywhere:
--   * `line` is 1-based and indexes a line that always exists.
--   * `col` is a byte offset inside `lines[line]`, always on a UTF-8 codepoint
--     boundary, so `lines[line]:sub(1, col)` is a complete UTF-8 prefix.
--   * No line ever contains a literal newline; newlines split into rows.
--
-- Parents OWN their keys. `handle_key` returns one of R.IGNORED / R.MOVED /
-- R.CHANGED. Parent dispatchers must filter their own keys (`<Esc>`, `<C-c>`,
-- submit keys, etc.) BEFORE forwarding, because `handle_key` claims any key
-- it can interpret. `<C-a>` is bound to move-home, so a parent that wants it
-- for "select all" must intercept first.
--
-- Keys are in canonical notation, as `win:recv` delivers them. Splitting a
-- line is the `input:split_line()` method rather than a pseudo-key, so every
-- key in KEYMAP is one a terminal can send.
--
-- IGNORED is returned when the buffer literally cannot act (backspace at
-- (1, 0), right at end of buffer, etc.). Parents can use that signal to fall
-- through to their own logic.
--
-- Parity cases live in plugins/lib/tests/spec.lua (TRACE_CASES). Add one
-- whenever you change handle_key semantics.

local R = { IGNORED = "ignored", MOVED = "moved", CHANGED = "changed" }

-- Mirrors Rust char::is_ascii_whitespace: SP HT LF VT FF CR.
local WS = { [0x20] = true, [0x09] = true, [0x0A] = true, [0x0B] = true, [0x0C] = true, [0x0D] = true }

local function is_ws_byte(line: string, byte_pos: number): boolean
  return WS[line:byte(byte_pos)] == true
end

local function prev_codepoint_boundary(s: string, col: number): number
  if col <= 0 then
    return 0
  end
  return (utf8.offset(s, -1, col + 1) or 1) - 1
end

local function next_codepoint_boundary(s: string, col: number): number
  if col >= #s then
    return #s
  end
  return (utf8.offset(s, 2, col + 1) or (#s + 1)) - 1
end

-- Scans past whitespace then past non-whitespace, both backwards.
-- Mirrors `find_prev_word_boundary` in maki-ui/src/text_buffer.rs.
local function find_prev_word_boundary(line: string, col: number): number
  local i = col
  while i > 0 and is_ws_byte(line, i) do
    i = prev_codepoint_boundary(line, i)
  end
  while i > 0 and not is_ws_byte(line, i) do
    i = prev_codepoint_boundary(line, i)
  end
  return i
end

local function find_next_word_boundary(line: string, col: number): number
  local n = #line
  local i = col
  while i < n and is_ws_byte(line, i + 1) do
    i = next_codepoint_boundary(line, i)
  end
  while i < n and not is_ws_byte(line, i + 1) do
    i = next_codepoint_boundary(line, i)
  end
  return i
end

local function split_at_cursor(ln: string, col: number): (string, string, string)
  local before = ln:sub(1, col)
  if col >= #ln then
    return before, " ", ""
  end
  local next_byte = next_codepoint_boundary(ln, col)
  return before, ln:sub(col + 1, next_byte), ln:sub(next_byte + 1)
end

local TextInput = {}
TextInput.__index = TextInput
TextInput.Result = R
TextInput._debug = false

export type Result = string
type Fields = { lines: { string }, line: number, col: number }
export type TextInput = setmetatable<Fields, typeof(TextInput)>

function TextInput.new(): TextInput
  return setmetatable({ lines = { "" }, line = 1, col = 0 }, TextInput)
end

function TextInput.value(self: TextInput): string
  return table.concat(self.lines, "\n")
end
function TextInput.is_empty(self: TextInput): boolean
  return #self.lines == 1 and self.lines[1] == ""
end
function TextInput.line_count(self: TextInput): number
  return #self.lines
end

function TextInput.clear(self: TextInput)
  self.lines, self.line, self.col = { "" }, 1, 0
end

-- Returns the codepoint right before the cursor as a string, or nil at the
-- start of a line. Lets callers peek backwards (e.g. "is the previous char
-- a backslash?") without touching internal indices.
function TextInput.char_before_cursor(self: TextInput): string?
  if self.col == 0 then
    return nil
  end
  local prev = utf8.offset(self.lines[self.line], -1, self.col + 1)
  return prev and self.lines[self.line]:sub(prev, self.col) or nil
end

function TextInput.insert_text(self: TextInput, text: string)
  local start = 1
  while true do
    local nl = text:find("\n", start, true)
    if not nl then
      if start <= #text then
        self:_insert_chunk(text:sub(start))
      end
      self:_check_invariants()
      return R.CHANGED
    end
    if nl > start then
      self:_insert_chunk(text:sub(start, nl - 1))
    end
    self:split_line()
    start = nl + 1
  end
end

function TextInput.insert_char(self: TextInput, c: string)
  self:_insert_chunk(c)
  return R.CHANGED
end

function TextInput.insert_space(self: TextInput)
  return self:insert_char(" ")
end

function TextInput._insert_chunk(self: TextInput, text: string)
  local ln = self.lines[self.line]
  self.lines[self.line] = ln:sub(1, self.col) .. text .. ln:sub(self.col + 1)
  self.col = self.col + #text
end

function TextInput.split_line(self: TextInput)
  local ln = self.lines[self.line]
  self.lines[self.line] = ln:sub(1, self.col)
  table.insert(self.lines, self.line + 1, ln:sub(self.col + 1))
  self.line = self.line + 1
  self.col = 0
  return R.CHANGED
end

function TextInput._merge_with_prev_line(self: TextInput)
  local cur = table.remove(self.lines, self.line) :: string
  self.line = self.line - 1
  local prev = self.lines[self.line]
  self.col = #prev
  self.lines[self.line] = prev .. cur
end

function TextInput._merge_with_next_line(self: TextInput)
  local nxt = table.remove(self.lines, self.line + 1) :: string
  self.lines[self.line] = self.lines[self.line] .. nxt
end

function TextInput.remove_char(self: TextInput)
  if self.col > 0 then
    local ln = self.lines[self.line]
    local start = prev_codepoint_boundary(ln, self.col)
    self.lines[self.line] = ln:sub(1, start) .. ln:sub(self.col + 1)
    self.col = start
  elseif self.line > 1 then
    self:_merge_with_prev_line()
  else
    return R.IGNORED
  end
  return R.CHANGED
end

function TextInput.delete_char(self: TextInput)
  local ln = self.lines[self.line]
  if self.col < #ln then
    local next_byte = next_codepoint_boundary(ln, self.col)
    self.lines[self.line] = ln:sub(1, self.col) .. ln:sub(next_byte + 1)
  elseif self.line < #self.lines then
    self:_merge_with_next_line()
  else
    return R.IGNORED
  end
  return R.CHANGED
end

function TextInput.remove_word_before(self: TextInput)
  if self.col == 0 then
    if self.line == 1 then
      return R.IGNORED
    end
    self:_merge_with_prev_line()
    return R.CHANGED
  end
  local ln = self.lines[self.line]
  local start = find_prev_word_boundary(ln, self.col)
  self.lines[self.line] = ln:sub(1, start) .. ln:sub(self.col + 1)
  self.col = start
  return R.CHANGED
end

function TextInput.delete_word_after(self: TextInput)
  local ln = self.lines[self.line]
  if self.col == #ln then
    if self.line == #self.lines then
      return R.IGNORED
    end
    self:_merge_with_next_line()
    return R.CHANGED
  end
  local stop = find_next_word_boundary(ln, self.col)
  self.lines[self.line] = ln:sub(1, self.col) .. ln:sub(stop + 1)
  return R.CHANGED
end

function TextInput.kill_to_end_of_line(self: TextInput)
  local ln = self.lines[self.line]
  if self.col == #ln then
    return R.IGNORED
  end
  self.lines[self.line] = ln:sub(1, self.col)
  return R.CHANGED
end

function TextInput.move_left(self: TextInput)
  if self.col > 0 then
    self.col = prev_codepoint_boundary(self.lines[self.line], self.col)
  elseif self.line > 1 then
    self.line = self.line - 1
    self.col = #self.lines[self.line]
  else
    return R.IGNORED
  end
  return R.MOVED
end

function TextInput.move_right(self: TextInput)
  if self.col < #self.lines[self.line] then
    self.col = next_codepoint_boundary(self.lines[self.line], self.col)
  elseif self.line < #self.lines then
    self.line = self.line + 1
    self.col = 0
  else
    return R.IGNORED
  end
  return R.MOVED
end

-- Lua has no sticky-x. When jumping rows we clamp to end-of-line, or snap
-- to the codepoint boundary at-or-before the previous byte offset so we
-- never land inside a multibyte sequence. We step back over UTF-8
-- continuation bytes (0x80..0xBF) directly to stay independent of utf8.offset
-- which errors when given a continuation position.
function TextInput._snap_col_to_line(self: TextInput)
  local ln = self.lines[self.line]
  if self.col > #ln then
    self.col = #ln
    return
  end
  while self.col > 0 do
    local b = ln:byte(self.col + 1)
    if b == nil or b < 0x80 or b >= 0xC0 then
      return
    end
    self.col = self.col - 1
  end
end

function TextInput.move_up(self: TextInput)
  if self.line == 1 then
    return R.IGNORED
  end
  self.line = self.line - 1
  self:_snap_col_to_line()
  return R.MOVED
end

function TextInput.move_down(self: TextInput)
  if self.line == #self.lines then
    return R.IGNORED
  end
  self.line = self.line + 1
  self:_snap_col_to_line()
  return R.MOVED
end

function TextInput.move_home(self: TextInput)
  if self.col == 0 then
    return R.IGNORED
  end
  self.col = 0
  return R.MOVED
end

function TextInput.move_end(self: TextInput)
  local n = #self.lines[self.line]
  if self.col == n then
    return R.IGNORED
  end
  self.col = n
  return R.MOVED
end

function TextInput.move_word_left(self: TextInput)
  if self.col == 0 then
    return self:move_left()
  end
  local new_col = find_prev_word_boundary(self.lines[self.line], self.col)
  if new_col == self.col then
    return R.IGNORED
  end
  self.col = new_col
  return R.MOVED
end

function TextInput.move_word_right(self: TextInput)
  local ln = self.lines[self.line]
  if self.col == #ln then
    return self:move_right()
  end
  local new_col = find_next_word_boundary(ln, self.col)
  if new_col == self.col then
    return R.IGNORED
  end
  self.col = new_col
  return R.MOVED
end

function TextInput._check_invariants(self: TextInput)
  if not TextInput._debug then
    return
  end
  assert(
    self.line >= 1 and self.line <= #self.lines,
    "line out of range: " .. tostring(self.line) .. "/" .. #self.lines
  )
  local ln = self.lines[self.line]
  assert(self.col >= 0 and self.col <= #ln, "col out of range: " .. tostring(self.col) .. "/" .. #ln)
  if self.col > 0 and self.col < #ln then
    local b = ln:byte(self.col + 1)
    assert(b < 0x80 or b >= 0xC0, "col not on codepoint boundary: col=" .. tostring(self.col))
  end
  for i, l in ipairs(self.lines) do
    assert(not l:find("\n", 1, true), "line " .. i .. " contains embedded newline")
  end
end

local KEYMAP: { [string]: (TextInput) -> Result } = {
  ["<Left>"] = TextInput.move_left,
  ["<Right>"] = TextInput.move_right,
  ["<Up>"] = TextInput.move_up,
  ["<Down>"] = TextInput.move_down,
  ["<Home>"] = TextInput.move_home,
  ["<End>"] = TextInput.move_end,
  ["<C-Left>"] = TextInput.move_word_left,
  ["<C-Right>"] = TextInput.move_word_right,
  ["<M-Left>"] = TextInput.move_word_left,
  ["<M-Right>"] = TextInput.move_word_right,
  ["<M-b>"] = TextInput.move_word_left,
  ["<M-f>"] = TextInput.move_word_right,
  ["<C-a>"] = TextInput.move_home,
  ["<C-e>"] = TextInput.move_end,
  ["<BS>"] = TextInput.remove_char,
  ["<S-BS>"] = TextInput.remove_char,
  ["<Del>"] = TextInput.delete_char,
  ["<C-w>"] = TextInput.remove_word_before,
  ["<C-BS>"] = TextInput.remove_word_before,
  ["<M-BS>"] = TextInput.remove_word_before,
  ["<C-Del>"] = TextInput.delete_word_after,
  ["<M-Del>"] = TextInput.delete_word_after,
  ["<M-d>"] = TextInput.delete_word_after,
  ["<C-k>"] = TextInput.kill_to_end_of_line,
  ["<Space>"] = TextInput.insert_space,
}

function TextInput.handle_key(self: TextInput, key: string): Result
  local op = KEYMAP[key]
  local result
  if op then
    result = op(self)
  elseif (utf8.len(key) or 0) == 1 then
    result = self:insert_char(key)
  else
    return R.IGNORED
  end
  self:_check_invariants()
  return result
end

-- Wrap lines to {width} with {prefix} before the first row. Returns
-- { lines = styled lines, cursor_row = 1-based row holding the cursor }.
function TextInput.render(self: TextInput, prefix: string, prefix_width: number?, width: number?)
  local result: { { { string } } } = {}
  local pw = prefix_width or #prefix
  local pad = string.rep(" ", pw)
  local usable = math.max((width or 0x40000000) - pw, 1)
  local cursor_row = 1
  local first = true

  for i, ln in ipairs(self.lines) do
    local is_cursor_line = (i == self.line)
    local pos = 0

    repeat
      local chunk_start = pos + 1
      local chunk_end = pos
      local chars = 0
      while chunk_end < #ln and chars < usable do
        chunk_end = (utf8.offset(ln, 2, chunk_end + 1) or (#ln + 1)) - 1
        chars = chars + 1
      end

      local pfx_str = first and prefix or pad
      first = false
      local chunk = ln:sub(chunk_start, chunk_end)
      local has_cursor = is_cursor_line and self.col >= pos and (self.col < chunk_end or chunk_end >= #ln)

      if has_cursor then
        local before, cur, after = split_at_cursor(chunk, self.col - pos)
        cursor_row = #result + 1
        result[#result + 1] = {
          { pfx_str, "dim" },
          { before, "" },
          { cur, "cursor" },
          { after, "" },
        }
      else
        result[#result + 1] = { { pfx_str, "dim" }, { chunk, "" } }
      end

      pos = chunk_end
    until pos >= #ln
  end

  return { lines = result, cursor_row = cursor_row }
end

return TextInput
