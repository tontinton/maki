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
local PER_MILLION = 1000000

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

--- A model row's `pricing` from per-token prices, in dollars per million
--- tokens. Half a price is no price: without both `input` and `output` it
--- would read as free, so the answer is nil. A missing cache price is 0.
function M.pricing(input, output, cache_write, cache_read)
  if input == nil or output == nil then
    return nil
  end
  return {
    input = input * PER_MILLION,
    output = output * PER_MILLION,
    cache_write = (cache_write or 0) * PER_MILLION,
    cache_read = (cache_read or 0) * PER_MILLION,
  }
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
