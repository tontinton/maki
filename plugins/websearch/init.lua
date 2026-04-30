local EXA_MCP_ENDPOINT = "https://mcp.exa.ai/mcp"
local REQUEST_TIMEOUT_SECS = 25
local DEFAULT_NUM_RESULTS = 8

local parse_sse_response = require("parse_sse")
local truncate = require("truncate")
local ToolView = require("tool_view")

maki.api.register_tool({
  name = "websearch",
  description = "Search the web for real-time information using Exa AI.\n\n"
    .. "Today's date is "
    .. os.date("%Y-%m-%d")
    .. ".\n\n"
    .. "- Use for current events, documentation, APIs, or anything not in local files.\n"
    .. "- Prefer specific, targeted queries over broad ones.\n"
    .. "- Results include page titles, URLs, and content snippets.",

  schema = {
    type = "object",
    properties = {
      query = { type = "string", description = "Search query", required = true },
      num_results = { type = "integer", description = "Number of results to return (default 8)" },
    },
  },
  permission_scope = "query",
  audiences = { "main", "interpreter" },

  header = function(input)
    return input.query
  end,

  handler = function(input, ctx)
    local query = input.query
    if not query then
      return "error: query is required"
    end

    local num_results = input.num_results or DEFAULT_NUM_RESULTS

    local payload, encode_err = maki.json.encode({
      jsonrpc = "2.0",
      id = 1,
      method = "tools/call",
      params = {
        name = "web_search_exa",
        arguments = {
          query = query,
          numResults = num_results,
          type = "auto",
          livecrawl = "fallback",
        },
      },
    })
    if not payload then
      return "error: failed to encode request: " .. tostring(encode_err)
    end

    local config = ctx:config()
    local max_response = (config and config.max_response_bytes) or (5 * 1024 * 1024)
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)

    local resp, err = maki.net.request(EXA_MCP_ENDPOINT, {
      method = "POST",
      body = payload,
      headers = {
        ["Content-Type"] = "application/json",
        ["Accept"] = "application/json, text/event-stream",
      },
      timeout = REQUEST_TIMEOUT_SECS,
      max_bytes = max_response,
    })
    if not resp then
      return "error: " .. tostring(err)
    end

    if resp.status < 200 or resp.status >= 300 then
      local preview = resp.body:sub(1, 200)
      return "error: HTTP " .. tostring(resp.status) .. ": " .. preview
    end

    local text, parse_err = parse_sse_response(resp.body)
    if not text then
      return "error: " .. tostring(parse_err)
    end

    local llm_output = truncate(text, max_lines, max_bytes)

    local tol = ctx:tool_output_lines()
    local buf = maki.ui.buf()
    local view = ToolView.new(buf, {
      max_lines = (tol and tol.web) or 3,
      keep = "head",
    })
    buf:on("click", function()
      view:toggle()
    end)

    for line in text:gmatch("([^\n]*)\n?") do
      view:append(line)
    end
    view:finish()

    return {
      llm_output = llm_output,
      body = buf,
    }
  end,
})
