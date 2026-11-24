local M = {}
local numbered_list = require("numbered_list")
local NOT_AN_OBJECT_MSG = require("constants").NOT_AN_OBJECT_MSG

M.ENDPOINT = "https://kagi.com/api/v1/search"

local BODY_PREVIEW_BYTES = 200
local MAX_CODEPOINT = 0x10FFFF
-- Null fields read as absent, so `data = null` on an error is a missing table.
local DECODE_OPTS = { luanil = { object = true, array = true } }
-- The categories Kagi's own markdown renders, in its order; related searches,
-- interesting finds and creator listings are dropped.
local CATEGORIES = { "search", "news", "video", "podcast", "image" }
-- Kagi wraps query-term matches in titles and snippets with these tags.
local HIGHLIGHT_TAGS = { "</?strong>", '<span class="ln%-search%-highlight">', "</span>" }
local NAMED_ENTITIES = { amp = "&", lt = "<", gt = ">", quot = '"', apos = "'", nbsp = " " }

-- An entity it does not know returns nil, which leaves the text as it was.
local function decode_entity(entity)
  local hex = entity:match("^#[xX](%x+)$")
  local digits = hex or entity:match("^#(%d+)$")
  if not digits then
    return NAMED_ENTITIES[entity]
  end
  local code = tonumber(digits, hex and 16 or 10)
  return code and code <= MAX_CODEPOINT and utf8.char(code) or nil
end

local function clean(text)
  if type(text) ~= "string" then
    return nil
  end
  for _, tag in ipairs(HIGHLIGHT_TAGS) do
    text = text:gsub(tag, "")
  end
  return (text:gsub("&(#?%w+);", decode_entity))
end

function M.request_body(query, num_results)
  return maki.json.encode({ query = query, limit = num_results })
end

-- Error envelope -> its messages and trace id, or nil when the body is not
-- one. The spec names the array `error`, the server sends `errors`.
function M.error_text(body)
  local parsed = maki.json.decode(body, DECODE_OPTS)
  local errors = type(parsed) == "table" and (parsed.errors or parsed.error)
  if type(errors) ~= "table" then
    return nil
  end
  local messages = {}
  for _, detail in ipairs(errors) do
    local message = type(detail) == "table" and (detail.message or detail.code)
    if type(message) == "string" then
      table.insert(messages, message)
    end
  end
  if #messages == 0 then
    return nil
  end
  local trace = type(parsed.meta) == "table" and parsed.meta.trace
  return table.concat(messages, "; ") .. (type(trace) == "string" and (" (trace id: " .. trace .. ")") or "")
end

-- /search body -> model-facing text. `limit` caps each category on its own,
-- and every result Kagi sent is kept.
function M.format_results(body)
  local parsed, decode_err = maki.json.decode(body, DECODE_OPTS)
  local preview = body:sub(1, BODY_PREVIEW_BYTES)
  if type(parsed) ~= "table" then
    return nil, "Kagi returned invalid JSON: " .. (decode_err or NOT_AN_OBJECT_MSG) .. " (body: " .. preview .. ")"
  end
  if type(parsed.data) ~= "table" then
    return nil, "Kagi returned no data: " .. (M.error_text(body) or preview)
  end
  local results = {}
  for _, category in ipairs(CATEGORIES) do
    local items = parsed.data[category]
    for _, item in ipairs(type(items) == "table" and items or {}) do
      if type(item) == "table" then
        table.insert(results, {
          url = item.url,
          title = clean(item.title),
          published = type(item.time) == "string" and item.time:match("^[^T]+") or nil,
          snippet = clean(item.snippet),
        })
      end
    end
  end
  return numbered_list(results, #results)
end

return M
