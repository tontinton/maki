local M = {}
local numbered_list = require("numbered_list")

-- Operator config wins over env; both are trimmed because a padded blank is
-- truthy in lua yet means "unset" to whoever exported it.
function M.resolve_base(option, env_value)
  local base = tostring(option or ""):match("^%s*(.-)%s*$")
  if base == "" then
    base = tostring(env_value or ""):match("^%s*(.-)%s*$")
  end
  return base
end

-- Percent-encode everything outside the RFC 3986 unreserved set; queries
-- carry spaces, ampersands and unicode.
function M.encode_param(s)
  return (tostring(s):gsub("[^%w%-_.~]", function(c)
    return string.format("%%%02X", c:byte())
  end))
end

-- {base} + query -> search URL. The base is operator config, never model
-- output; it is trimmed so stray spaces or trailing slashes cannot corrupt
-- the URL.
function M.search_url(base, query, time_range)
  local trimmed = tostring(base or ""):match("^%s*(.-)%s*$"):gsub("/+$", "")
  if trimmed == "" then
    return nil, "no SearXNG base URL (set plugins.websearch.searxng_base_url or SEARXNG_BASE_URL)"
  end
  local url = trimmed .. "/search?q=" .. M.encode_param(query) .. "&format=json"
  if time_range then
    url = url .. "&time_range=" .. M.encode_param(time_range)
  end
  return url
end

-- Decoded /search body -> model-facing text. An empty result set is a plain
-- "no results"; a payload without a results array is an error, carrying the
-- instance's own reason when it sent one (rate limits answer 200 with {error}).
function M.format_results(data, num_results)
  if type(data) ~= "table" or type(data.results) ~= "table" then
    local why = type(data) == "table" and (data.error or data.message) or nil
    return nil, "SearXNG returned no results array" .. (why ~= nil and (": " .. tostring(why)) or "")
  end
  return numbered_list(data.results, num_results)
end

return M
