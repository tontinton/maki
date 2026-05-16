local TextInput = {}
TextInput.__index = TextInput

function TextInput.new()
  return setmetatable({ chars = {}, cursor = 0 }, TextInput)
end

function TextInput:value()
  return table.concat(self.chars)
end

function TextInput:is_empty()
  return #self.chars == 0
end

function TextInput:handle_key(key)
  if key == "backspace" then
    if self.cursor > 0 then
      table.remove(self.chars, self.cursor)
      self.cursor = self.cursor - 1
    end
    return true
  elseif key == "ctrl+w" then
    if self.cursor > 0 then
      local i = self.cursor
      if self.chars[i] == " " then
        while i > 0 and self.chars[i] == " " do
          table.remove(self.chars, i)
          i = i - 1
        end
      else
        while i > 0 and self.chars[i] ~= " " do
          table.remove(self.chars, i)
          i = i - 1
        end
      end
      self.cursor = i
    end
    return true
  elseif key == "left" then
    if self.cursor > 0 then
      self.cursor = self.cursor - 1
    end
    return true
  elseif key == "right" then
    if self.cursor < #self.chars then
      self.cursor = self.cursor + 1
    end
    return true
  elseif key == "home" then
    self.cursor = 0
    return true
  elseif key == "end" then
    self.cursor = #self.chars
    return true
  elseif #key == 1 then
    self.cursor = self.cursor + 1
    table.insert(self.chars, self.cursor, key)
    return true
  end
  return false
end

function TextInput:render(prefix)
  local before = table.concat(self.chars, "", 1, self.cursor)
  local cursor_char = self.chars[self.cursor + 1] or " "
  local after = ""
  if self.cursor + 2 <= #self.chars then
    after = table.concat(self.chars, "", self.cursor + 2)
  end
  return {
    { prefix, "dim" },
    { before, "" },
    { cursor_char, "cursor" },
    { after, "" },
  }
end

return TextInput
