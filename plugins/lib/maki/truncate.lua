--!strict
--- require("maki.truncate")(text, max_lines, max_bytes)
--- Cuts {text} to the limits and notes how many bytes it dropped. A nil limit
--- is unlimited.
local function truncate(text: string, max_lines: number?, max_bytes: number?): string
  if not max_lines and not max_bytes then
    return text
  end
  max_lines = max_lines or math.huge
  max_bytes = max_bytes or #text
  if #text <= max_bytes then
    local n = 0
    for _ in text:gmatch("\n") do
      n = n + 1
    end
    if n + 1 <= max_lines then
      return text
    end
  end
  local out = {}
  local bytes = 0
  local lines = 0
  for line in text:gmatch("([^\n]*)\n?") do
    lines = lines + 1
    if lines > max_lines then
      break
    end
    local new_bytes = bytes + #line + 1
    if new_bytes > max_bytes then
      if #out == 0 then
        -- Back off UTF-8 continuation bytes so no character is split in half.
        local cut = max_bytes
        while cut > 0 and line:find("^[\128-\191]", cut + 1) do
          cut = cut - 1
        end
        out[1] = line:sub(1, cut)
      end
      break
    end
    out[#out + 1] = line
    bytes = new_bytes
  end
  local result = table.concat(out, "\n")
  if #result < #text then
    result = result .. "\n\n[truncated " .. (#text - #result) .. " bytes]"
  end
  return result
end

return truncate
