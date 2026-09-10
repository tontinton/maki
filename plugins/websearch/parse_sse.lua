local NO_RESULTS_MSG = "No search results found"
local MCP_ERROR_PREFIX = "MCP error: "
local MCP_TOOL_ERROR_PREFIX = "MCP tool error: "
local UNKNOWN_ERROR_MSG = "unknown error"

local function extract_text(parsed)
  local content = parsed.result and parsed.result.content
  local text = type(content) == "table" and content[1] and content[1].text
  if type(text) == "string" and #text > 0 then
    return text
  end
end

-- Rate limits and auth failures come back as HTTP 200 with a jsonrpc error
-- or result.isError, so without this the tool would answer "no results" and
-- the model would believe the web had nothing to say.
local function failure_message(parsed)
  local err = parsed.error
  if err ~= nil then
    local message = type(err) == "table" and err.message or err
    return MCP_ERROR_PREFIX .. (type(message) == "string" and message or UNKNOWN_ERROR_MSG)
  end
  if parsed.result and parsed.result.isError then
    return MCP_TOOL_ERROR_PREFIX .. (extract_text(parsed) or UNKNOWN_ERROR_MSG)
  end
end

local function parse_sse_response(body)
  for line in body:gmatch("[^\n]+") do
    local data = line:match("^data: (.+)")
    if data then
      local parsed, parse_err = maki.json.decode(data)
      if not parsed then
        return nil, "SSE JSON parse error: " .. parse_err
      end
      local failure = failure_message(parsed)
      if failure then
        return nil, failure
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
