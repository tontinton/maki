-- Search backends the websearch tool can talk to. Each one is an MCP server
-- that answers tools/call with a text content block, so they all share the
-- same SSE plumbing; what differs is the endpoint, the tool name, the
-- argument names, and how a key rides along when the user has one.

local EXA_ENDPOINT = "https://mcp.exa.ai/mcp"
local YOUCOM_ENDPOINT = "https://api.you.com/mcp"
local YOUCOM_FREE_ENDPOINT = "https://api.you.com/mcp?profile=free"

-- An exported but empty variable is still truthy in lua, and a blank key
-- means an `Authorization: Bearer ` header and a 401 instead of the keyless
-- path that would have worked.
local function env_key(name)
  local value = maki.uv.os_getenv(name)
  local key = value and value:match("^%s*(.-)%s*$")
  if key == "" then
    return nil
  end
  return key
end

local PROVIDERS = {
  exa = {
    api_key = function()
      return env_key("EXA_API_KEY")
    end,
    endpoint = function()
      return EXA_ENDPOINT
    end,
    tool = "web_search_exa",
    description = "Search the web for real-time information using Exa AI.",
    arguments = function(query, num_results)
      return {
        query = query,
        numResults = num_results,
        type = "auto",
        livecrawl = "fallback",
      }
    end,
    headers = function(api_key)
      if not api_key then
        return {}
      end
      return { ["x-api-key"] = api_key }
    end,
  },
  youcom = {
    api_key = function()
      return env_key("YDC_API_KEY")
    end,
    -- Keyless the request rides the free profile; with YDC_API_KEY set it
    -- goes to the authenticated server and carries the bearer token.
    endpoint = function(api_key)
      return api_key and YOUCOM_ENDPOINT or YOUCOM_FREE_ENDPOINT
    end,
    tool = "you-search",
    description = "Search the web for real-time information using You.com.",
    arguments = function(query, num_results)
      return {
        query = query,
        count = num_results,
      }
    end,
    headers = function(api_key)
      if not api_key then
        return {}
      end
      return { ["Authorization"] = "Bearer " .. api_key }
    end,
  },
}

return PROVIDERS
