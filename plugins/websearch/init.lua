local REQUEST_TIMEOUT_SECS = 25
local DEFAULT_NUM_RESULTS = 8

local parse_sse_response = require("parse_sse")
local providers = require("providers")
local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local opts = maki.api.register_options(output_limits.extend({
  provider = {
    default = "exa",
    type = "string",
    desc = 'Search backend: "exa" (default) or "youcom" (You.com MCP).',
  },
  max_response_bytes = {
    default = 5 * 1024 * 1024,
    min = 1024,
    desc = "Stop reading a response after this many bytes.",
  },
}))

local provider = providers[opts.provider]
if not provider then
  error('websearch: unknown provider "' .. tostring(opts.provider) .. '" (expected "exa" or "youcom")')
end

-- An exported but blank variable is still truthy in lua, and a blank key
-- means an empty auth header instead of the keyless path that would work.
local function api_key()
  local key = (maki.uv.os_getenv(provider.env) or ""):match("^%s*(.-)%s*$")
  return key ~= "" and key or nil
end

local function web_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.web) or 3, keep = "head" }
end

maki.api.register_tool({
  name = "websearch",
  kind = "fetch",
  host_access = true,
  description = "Search the web for real-time information using "
    .. provider.label
    .. ".\n\n"
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
  permission = "net",
  permission_scopes = "query",
  -- research/general included so subagents keep web search now that the
  -- interpreter only exposes tools the host audience could see itself.
  audiences = { "main", "research_sub", "general_sub", "interpreter" },

  header = function(input)
    return input.query
  end,

  restore = function(_input, output, _is_error, ctx)
    return ToolView.restore(output, web_view_opts(ctx))
  end,

  handler = function(input, ctx)
    local query = input.query
    if not query then
      return { llm_output = "error: query is required", is_error = true }
    end

    local num_results = input.num_results or DEFAULT_NUM_RESULTS

    local payload, encode_err = maki.json.encode({
      jsonrpc = "2.0",
      id = 1,
      method = "tools/call",
      params = {
        name = provider.tool,
        arguments = provider.arguments(query, num_results),
      },
    })
    if not payload then
      return { llm_output = "error: failed to encode request: " .. tostring(encode_err), is_error = true }
    end

    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    local key = api_key()

    local headers = {
      ["Content-Type"] = "application/json",
      ["Accept"] = "application/json, text/event-stream",
    }
    if key then
      headers[provider.auth_header] = (provider.auth_prefix or "") .. key
    end
    local endpoint = provider.endpoint .. ((not key and provider.keyless_suffix) or "")

    local resp, err = maki.net.request(endpoint, {
      method = "POST",
      body = payload,
      headers = headers,
      timeout = REQUEST_TIMEOUT_SECS,
      max_bytes = opts.max_response_bytes,
    })
    if not resp then
      return { llm_output = "error: " .. tostring(err), is_error = true }
    end

    if resp.status < 200 or resp.status >= 300 then
      local preview = resp.body:sub(1, 200)
      return { llm_output = "error: HTTP " .. tostring(resp.status) .. ": " .. preview, is_error = true }
    end

    local text, parse_err = parse_sse_response(resp.body)
    if not text then
      return { llm_output = "error: " .. tostring(parse_err), is_error = true }
    end

    local llm_output = truncate(text, max_lines, max_bytes)

    return {
      llm_output = llm_output,
      body = ToolView.restore(text, web_view_opts(ctx)),
    }
  end,
})
