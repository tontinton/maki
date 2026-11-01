local REQUEST_TIMEOUT_SECS = 25
local DEFAULT_NUM_RESULTS = 8
-- Null fields read as absent: `data.error or data.message` and friends test truthiness.
local DECODE_OPTS = { luanil = { object = true, array = true } }

local parse_sse_response = require("parse_sse")
local providers = require("providers")
local searxng = require("searxng")
local NOT_AN_OBJECT_MSG = require("constants").NOT_AN_OBJECT_MSG
local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local opts = maki.api.register_options(output_limits.extend({
  provider = {
    default = "exa",
    type = "string",
    desc = 'Search backend: "exa" (default), "youcom" (You.com MCP), "kagi" (Kagi MCP, needs KAGI_API_KEY), or "searxng" (self-hosted).',
  },
  searxng_base_url = {
    default = "",
    type = "string",
    desc = "SearXNG instance URL; falls back to the `SEARXNG_BASE_URL` env var. A LAN instance also needs its host in `net.allowed_private_hosts`. Only this backend accepts websearch's `time_range` (day, week, month, or year); its `num_results` is capped by the instance page size.",
  },
  max_response_bytes = {
    default = 5 * 1024 * 1024,
    min = 1024,
    desc = "Stop reading a response after this many bytes.",
  },
}))

local provider = providers[opts.provider]
if not provider then
  local names = {}
  for name in pairs(providers) do
    table.insert(names, '"' .. name .. '"')
  end
  table.sort(names)
  error(
    'websearch: unknown provider "'
      .. tostring(opts.provider)
      .. '" (expected one of '
      .. table.concat(names, ", ")
      .. ")"
  )
end

-- An exported but blank variable is still truthy in lua, and a blank key
-- means an empty auth header instead of the keyless path that would work.
local function trim(s)
  return (tostring(s or ""):match("^%s*(.-)%s*$"))
end

local function api_key()
  local key = trim(maki.uv.os_getenv(provider.env))
  return key ~= "" and key or nil
end

local function web_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.web) or 3, keep = "head" }
end

-- Request gate: a failed request or a non-2xx answer becomes an error
-- string, with a body preview so the server's own reason stays visible.
local function fetch(url, req_opts)
  local resp, err = maki.net.request(url, req_opts)
  if not resp then
    return nil, tostring(err)
  end
  if resp.status < 200 or resp.status >= 300 then
    return nil, "HTTP " .. tostring(resp.status) .. ": " .. resp.body:sub(1, 200)
  end
  return resp
end

local function search_mcp(query, num_results)
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
    return nil, "failed to encode request: " .. tostring(encode_err)
  end

  local key = api_key()
  if not key and provider.key_required then
    return nil, provider.label .. " search needs " .. provider.env .. " to be set"
  end
  local headers = {
    ["Content-Type"] = "application/json",
    ["Accept"] = "application/json, text/event-stream",
  }
  if key then
    headers[provider.auth_header] = (provider.auth_prefix or "") .. key
  end
  local endpoint = provider.endpoint .. ((not key and provider.keyless_suffix) or "")

  local resp, err = fetch(endpoint, {
    method = "POST",
    body = payload,
    headers = headers,
    timeout = REQUEST_TIMEOUT_SECS,
    max_bytes = opts.max_response_bytes,
  })
  if not resp then
    return nil, err
  end
  local text, sse_err = parse_sse_response(resp.body)
  if text and provider.format then
    text = provider.format(text)
  end
  return text, sse_err
end

local function search_searxng(query, num_results, time_range)
  local base = searxng.resolve_base(opts.searxng_base_url, maki.uv.os_getenv(provider.env))
  local url, url_err = searxng.search_url(base, query, time_range)
  if not url then
    return nil, url_err
  end
  local resp, err = fetch(url, {
    headers = { ["Accept"] = "application/json" },
    timeout = REQUEST_TIMEOUT_SECS,
    max_bytes = opts.max_response_bytes,
  })
  if not resp then
    return nil, err
  end
  local data, decode_err = maki.json.decode(resp.body, DECODE_OPTS)
  if type(data) ~= "table" then
    -- format=json disabled answers 200 + html; the preview is the clue.
    return nil,
      "SearXNG returned invalid JSON: "
        .. (decode_err or NOT_AN_OBJECT_MSG)
        .. " (body: "
        .. resp.body:sub(1, 200)
        .. ")"
  end
  return searxng.format_results(data, num_results)
end

-- time_range only means something to the GET backend; advertising it to the
-- MCP providers would spend prompt tokens on a parameter they silently drop.
local properties = {
  query = { type = "string", description = "Search query", required = true },
  num_results = {
    type = "integer",
    description = "Number of results to return (default " .. DEFAULT_NUM_RESULTS .. ")",
  },
}
if provider.transport == "get" then
  properties.time_range = {
    type = "string",
    enum = { "day", "week", "month", "year" },
    description = "Only results from the past day, week, month, or year",
  }
end

maki.api.register_tool({
  name = "websearch",
  kind = "fetch",
  description = "Search the web for real-time information using "
    .. provider.label
    .. ".\n\n"
    .. "For time-sensitive queries, use today's date (see Environment).\n\n"
    .. "- Use for current events, documentation, APIs, or anything not in local files.\n"
    .. "- Prefer specific, targeted queries over broad ones.\n"
    .. "- Results include page titles, URLs, and content snippets.",

  schema = {
    type = "object",
    properties = properties,
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

    local num_results = math.max(1, input.num_results or DEFAULT_NUM_RESULTS)

    local text, err
    if provider.transport == "get" then
      text, err = search_searxng(query, num_results, input.time_range)
    else
      text, err = search_mcp(query, num_results)
    end
    if not text then
      return { llm_output = "error: " .. tostring(err), is_error = true }
    end

    local max_lines, max_bytes = output_limits.resolve(opts, ctx)
    return {
      llm_output = truncate(text, max_lines, max_bytes),
      body = ToolView.restore(text, web_view_opts(ctx)),
    }
  end,
})
