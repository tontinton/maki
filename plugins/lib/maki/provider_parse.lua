-- Readers for the JSON a provider's side endpoints answer with, shared by the
-- bundled provider plugins.
--
-- Luau has one number type, so `8192` and `8192.0` decode to the same value
-- and both read as a whole number. A JSON null decodes to nil: it reads like a
-- missing key, and in an array it leaves a hole that stops `ipairs`. Numbers
-- above 2^53 come back rounded.
local M = {}

local U32_MAX = 4294967295
local U64_MAX = 2 ^ 64
local HTTP_OK = 200
-- A side call that failed is reported, not replayed: a retried 5xx would be
-- extra requests nobody asked for.
local NO_RETRY = 0

--- A GET with the provider's resolved `auth`, never retried. Returns the
--- decoded body. On failure returns nil and an error for `M.fail`.
function M.get_json(auth, url)
  local res, err = maki.net.request(url, { headers = auth.headers, retry = NO_RETRY })
  if not res then
    return nil, err
  end
  if res.status ~= HTTP_OK then
    return nil, maki.provider.http_error(res)
  end
  return maki.json.decode(res.body)
end

--- Hands a `M.get_json` error back from a hook. A refused request is returned,
--- so it fails the way the native provider does. Anything else never got an
--- HTTP status and is raised.
function M.fail(err)
  if type(err) == "string" then
    error(err, 0)
  end
  return nil, err
end

local function whole(value, max)
  if type(value) ~= "number" or value ~= math.floor(value) or value < 0 or value > max then
    return nil
  end
  return value
end

--- A whole, non-negative number up to 2^64, or nil.
function M.as_u64(value)
  return whole(value, U64_MAX)
end

--- A whole, non-negative number that fits a u32, or nil.
function M.as_u32(value)
  return whole(value, U32_MAX)
end

--- A number, or nil.
function M.as_f64(value)
  if type(value) ~= "number" then
    return nil
  end
  return value
end

--- A boolean, or nil.
function M.as_bool(value)
  if type(value) ~= "boolean" then
    return nil
  end
  return value
end

--- Each `body.data` element through `parse_row`, nils dropped, the first row
--- per id kept, sorted by id. A body without a `data` array lists nothing.
function M.models(body, parse_row)
  local rows, seen = {}, {}
  local data = type(body) == "table" and body.data
  for _, raw in ipairs(type(data) == "table" and data or {}) do
    local row = parse_row(raw)
    if row and not seen[row.id] then
      seen[row.id] = true
      table.insert(rows, row)
    end
  end
  table.sort(rows, function(a, b)
    return a.id < b.id
  end)
  return rows
end

return M
