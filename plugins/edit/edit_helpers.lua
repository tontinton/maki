local M = {}

local function split_lines(s)
  if s:sub(-1) == "\n" then
    s = s:sub(1, -2)
  end
  local lines = {}
  for line in (s .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end
  return lines
end

function M.replace_lines(content, start_line, end_line, new_string)
  local trailing_nl = content:sub(-1) == "\n"
  local lines = split_lines(content)
  local inserting = end_line == nil

  if inserting then
    if start_line < 1 or start_line > #lines + 1 then
      return nil, string.format("start line %d out of range (1-%d)", start_line, #lines + 1)
    end
  else
    if start_line < 1 or start_line > #lines then
      return nil, string.format("start line %d out of range (1-%d)", start_line, #lines)
    end
    if end_line < start_line or end_line > #lines then
      return nil, string.format("end line %d out of range (%d-%d)", end_line, start_line, #lines)
    end
  end

  local skip_from = inserting and start_line or start_line
  local skip_to = inserting and start_line - 1 or end_line

  local result = {}
  for i = 1, skip_from - 1 do
    result[#result + 1] = lines[i]
  end
  if new_string ~= "" or inserting then
    for _, line in ipairs(split_lines(new_string)) do
      result[#result + 1] = line
    end
  end
  for i = skip_to + 1, #lines do
    result[#result + 1] = lines[i]
  end

  local joined = table.concat(result, "\n")
  return trailing_nl and joined .. "\n" or joined
end

return M
