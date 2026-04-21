local NO_RESULTS_MSG = "No search results found"

local function extract_text(parsed)
  local content = parsed.result and parsed.result.content
  local text = type(content) == "table" and content[1] and content[1].text
  if type(text) == "string" and #text > 0 then
    return text
  end
end

local function parse_sse_response(body)
  for line in body:gmatch("[^\n]+") do
    local data = line:match("^data: (.+)")
    if data then
      local ok, parsed = pcall(maki.json.decode, data)
      if not ok then
        return nil, "SSE JSON parse error: " .. tostring(parsed)
      end
      if parsed == nil then
        return nil, "SSE JSON parse error: decode returned nil"
      end
      local text = extract_text(parsed)
      if text then
        return text
      end
    end
  end
  return NO_RESULTS_MSG
end

return parse_sse_response
