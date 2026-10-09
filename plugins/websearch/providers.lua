-- Every backend here is an MCP server that answers tools/call with a text
-- content block, so one SSE parser serves them all. What changes is the
-- endpoint, the tool name, the argument names, and how a key rides along.

local MAX_CODEPOINT = 0x10FFFF
local NAMED_ENTITIES = { amp = "&", lt = "<", gt = ">", quot = '"', apos = "'", nbsp = " " }

local function codepoint(code)
  return code <= MAX_CODEPOINT and utf8.char(code) or nil
end

local function decode_entities(text)
  return (
    text
      :gsub("&#[xX](%x+);", function(hex)
        return codepoint(tonumber(hex, 16))
      end)
      :gsub("&#(%d+);", function(dec)
        return codepoint(tonumber(dec))
      end)
      :gsub("&(%a+);", NAMED_ENTITIES)
  )
end

-- Kagi's markdown keeps the HTML of its search page: every query term wrapped
-- in <strong> and entity-encoded titles, plus a URL line repeating the link
-- in the heading above it. Together about a third of the response.
local function clean_kagi(text)
  return decode_entities(text:gsub("</?strong>", ""):gsub("\n%*%*URL:%*%* [^\n]*", ""))
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
    clean = clean_kagi,
  },
}
