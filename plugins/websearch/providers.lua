-- The MCP backends answer tools/call with a text content block, so one SSE
-- parser serves them all; what changes is the endpoint, the tool name, the
-- argument names, and how a key rides along. searxng is the exception: a
-- plain GET /search?format=json, built and read in searxng.lua.

local numbered_list = require("numbered_list")

-- A decoded object has lost kagimcp's key order, so this restores it.
local KAGI_CATEGORIES = { "search", "news", "video", "podcast", "image" }

-- kagimcp sends compact JSON grouped by category, and the numbered list
-- drops its keys and quotes. Every result it sent is kept, as its markdown
-- did. Text it can't read, not JSON or only unknown categories, passes
-- through, so a format change upstream doesn't read as "no results".
local function format_kagi(text)
  local data = maki.json.decode(text)
  if type(data) ~= "table" then
    return text
  end
  local results = {}
  for _, category in ipairs(KAGI_CATEGORIES) do
    if type(data[category]) == "table" then
      for _, result in ipairs(data[category]) do
        table.insert(results, result)
      end
    end
  end
  if #results == 0 and next(data) ~= nil then
    return text
  end
  return numbered_list(results, #results)
end

return {
  exa = {
    label = "Exa AI",
    endpoint = "https://mcp.exa.ai/mcp",
    env = "EXA_API_KEY",
    auth_header = "x-api-key",
    tool = "web_search_exa",
    arguments = function(query, num_results)
      return {
        query = query,
        numResults = num_results,
        type = "auto",
        livecrawl = "fallback",
      }
    end,
  },
  youcom = {
    label = "You.com",
    endpoint = "https://api.you.com/mcp",
    -- Without a key the plain endpoint answers 401, the free profile serves.
    keyless_suffix = "?profile=free",
    env = "YDC_API_KEY",
    auth_header = "Authorization",
    auth_prefix = "Bearer ",
    tool = "you-search",
    arguments = function(query, num_results)
      return {
        query = query,
        count = num_results,
      }
    end,
  },
  kagi = {
    label = "Kagi",
    endpoint = "https://mcp.kagi.com/mcp",
    -- No free tier: without a key the server answers a bodiless 401.
    key_required = true,
    env = "KAGI_API_KEY",
    auth_header = "Authorization",
    auth_prefix = "Bearer ",
    tool = "kagi_search_fetch",
    arguments = function(query, num_results)
      return {
        query = query,
        limit = num_results,
      }
    end,
    format = format_kagi,
  },
  searxng = {
    label = "SearXNG",
    transport = "get", -- plain GET /search?format=json, no MCP
    env = "SEARXNG_BASE_URL",
  },
}
