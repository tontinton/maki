local TextInput = {}
TextInput.__index = TextInput

-- The buffer is split into lines (no embedded newlines), `line` is 1-based, and
-- `col` is a byte offset inside that line. The invariant we lean on everywhere:
-- `col` always sits on a codepoint boundary, so `lines[line]:sub(1, col)` is
-- a complete UTF-8 prefix. Multi-byte characters move the cursor in one jump.

function TextInput.new()
  return setmetatable({ lines = { "" }, line = 1, col = 0 }, TextInput)
end

function TextInput:value()
  return table.concat(self.lines, "\n")
end

function TextInput:is_empty()
  return #self.lines == 1 and self.lines[1] == ""
end

function TextInput:line_count()
  return #self.lines
end

function TextInput:cursor_line()
  return self.line
end

local function prev_boundary(s, col)
  if col <= 0 then
    return 0
  end
  return (utf8.offset(s, -1, col + 1) or 1) - 1
end

local function next_boundary(s, col)
  if col >= #s then
    return #s
  end
  return (utf8.offset(s, 2, col + 1) or (#s + 1)) - 1
end

-- Returns the codepoint right before the cursor as a string, or nil when the
-- cursor sits at the start of a line. Useful for keybinds that peek backwards
-- (like "is the previous char a backslash?").
function TextInput:char_before_cursor()
  if self.col == 0 then
    return nil
  end
  local prev = prev_boundary(self.lines[self.line], self.col)
  return self.lines[self.line]:sub(prev + 1, self.col)
end

function TextInput:_insert_string(text)
  local ln = self.lines[self.line]
  self.lines[self.line] = ln:sub(1, self.col) .. text .. ln:sub(self.col + 1)
  self.col = self.col + #text
end

function TextInput:insert_text(text)
  local start = 1
  while true do
    local nl = text:find("\n", start, true)
    if not nl then
      if start <= #text then
        self:_insert_string(text:sub(start))
      end
      return
    end
    if nl > start then
      self:_insert_string(text:sub(start, nl - 1))
    end
    self:_split_line()
    start = nl + 1
  end
end

function TextInput:_split_line()
  local ln = self.lines[self.line]
  local before = ln:sub(1, self.col)
  local after = ln:sub(self.col + 1)
  self.lines[self.line] = before
  table.insert(self.lines, self.line + 1, after)
  self.line = self.line + 1
  self.col = 0
end

function TextInput:_join_with_prev()
  local cur = table.remove(self.lines, self.line)
  self.line = self.line - 1
  local prev = self.lines[self.line]
  self.col = #prev
  self.lines[self.line] = prev .. cur
end

function TextInput:_delete_codepoint_before()
  local ln = self.lines[self.line]
  local start = prev_boundary(ln, self.col)
  self.lines[self.line] = ln:sub(1, start) .. ln:sub(self.col + 1)
  self.col = start
end

function TextInput:_clamp_col_to_line()
  local ln = self.lines[self.line]
  if self.col >= #ln then
    self.col = #ln
  else
    self.col = prev_boundary(ln, self.col)
  end
end

function TextInput:_delete_word_before()
  local ln = self.lines[self.line]
  local i = self.col
  if i == 0 then
    return
  end
  local at_space = ln:sub(i, i) == " "
  if at_space then
    while i > 0 and ln:sub(i, i) == " " do
      i = i - 1
    end
  else
    while i > 0 and ln:sub(i, i) ~= " " do
      i = prev_boundary(ln, i)
    end
  end
  self.lines[self.line] = ln:sub(1, i) .. ln:sub(self.col + 1)
  self.col = i
end

function TextInput:handle_key(key)
  if key == "newline" then
    self:_split_line()
    return true
  elseif key == "backspace" then
    if self.col > 0 then
      self:_delete_codepoint_before()
    elseif self.line > 1 then
      self:_join_with_prev()
    end
    return true
  elseif key == "ctrl+w" then
    self:_delete_word_before()
    return true
  elseif key == "left" then
    if self.col > 0 then
      self.col = prev_boundary(self.lines[self.line], self.col)
    elseif self.line > 1 then
      self.line = self.line - 1
      self.col = #self.lines[self.line]
    end
    return true
  elseif key == "right" then
    if self.col < #self.lines[self.line] then
      self.col = next_boundary(self.lines[self.line], self.col)
    elseif self.line < #self.lines then
      self.line = self.line + 1
      self.col = 0
    end
    return true
  elseif key == "up" then
    if self.line > 1 then
      self.line = self.line - 1
      self:_clamp_col_to_line()
    end
    return true
  elseif key == "down" then
    if self.line < #self.lines then
      self.line = self.line + 1
      self:_clamp_col_to_line()
    end
    return true
  elseif key == "home" then
    self.col = 0
    return true
  elseif key == "end" then
    self.col = #self.lines[self.line]
    return true
  elseif key == "space" then
    self:_insert_string(" ")
    return true
  elseif #key == 1 then
    self:_insert_string(key)
    return true
  end
  return false
end

local function split_at_cursor(ln, col)
  local before = ln:sub(1, col)
  if col >= #ln then
    return before, " ", ""
  end
  local nb = next_boundary(ln, col)
  return before, ln:sub(col + 1, nb), ln:sub(nb + 1)
end

function TextInput:render(prefix, prefix_width)
  local result = {}
  local pw = prefix_width or #prefix
  local pad = string.rep(" ", pw)
  for i, ln in ipairs(self.lines) do
    local pfx = i == 1 and prefix or pad
    if i == self.line then
      local before, cursor_char, after = split_at_cursor(ln, self.col)
      result[#result + 1] = {
        { pfx, "dim" },
        { before, "" },
        { cursor_char, "cursor" },
        { after, "" },
      }
    else
      result[#result + 1] = {
        { pfx, "dim" },
        { ln, "" },
      }
    end
  end
  return result
end

return TextInput
