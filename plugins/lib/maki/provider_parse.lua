-- Rust parity for provider plugins that port a bespoke Rust parser. The Rust
-- side reads JSON with serde_json and prints with `format!`, and these helpers
-- reproduce its numbers bit for bit. Other plugins are better off with
-- `maki.json`.
--
-- Luau has a single number type, so `maki.json.decode` gives `8192` and
-- `8192.0` the same value, while serde_json's `as_u64` accepts only the first.
-- `M.decode` remembers which numbers were floats in the source text, and the
-- readers take the container and key (`M.as_u32(m, "context_length")` mirrors
-- `m["context_length"].as_u64().and_then(|v| u32::try_from(v).ok())`). A
-- missing key, a JSON null, a non-table container or the wrong type reads as
-- nil. Tables that did not come from `M.decode` carry no float marks, so there
-- a whole-valued float passes as an integer.
--
-- A JSON null decodes to nil, which looks like a missing key and leaves a hole
-- that stops `#` and `ipairs` early. `M.decode` also remembers where the nulls
-- were: `M.is_null` tells them from missing keys, and `M.items` walks an array
-- the way Rust's `as_array().iter()` does, nulls included.
--
-- `M.get_json` and `M.models` are the two halves of the Rust side's
-- `fetch_and_parse_models`, for a hook that fetches off the codec's request
-- path.
--
-- Luau numbers are doubles: a u64 above 2^53 comes back rounded, and
-- u64::MAX reads as 2^64.
local M = {}

local U32_MAX = 4294967295
local U64_MAX = 2 ^ 64
local U64_MAX_DIGITS = "18446744073709551615"
local QUOTE = string.byte('"')
local BACKSLASH = string.byte("\\")
local FLOAT_TAG = "\0f64:"
local FLOAT_TAG_JSON = '"\\u0000f64:'
local NULL_TAG = "\0null"
local NULL_TAG_JSON = '"\\u0000null"'
local NULL_LITERAL = "null"
local RESERVED_STRING = "string holds a reserved decode tag"
local NAN = 0 / 0
local HTTP_OK = 200
-- The Rust side never retries these calls, so a retried 5xx would show up as
-- extra requests in a golden.
local NO_RETRY = 0

local float_keys = setmetatable({}, { __mode = "k" })
local null_keys = setmetatable({}, { __mode = "k" })
local array_lens = setmetatable({}, { __mode = "k" })

local function starts_with_at(text, start, prefix)
  return string.sub(text, start, start + #prefix - 1) == prefix
end

local function string_end(text, start)
  local pos = start + 1
  while true do
    local found = string.find(text, '["\\]', pos)
    if not found then
      return #text
    end
    if string.byte(text, found) == QUOTE then
      return found
    end
    pos = found + 2
  end
end

-- serde_json reads a bare integer past u64::MAX as a float.
local function overflows_u64(lexeme)
  return #lexeme > #U64_MAX_DIGITS or (#lexeme == #U64_MAX_DIGITS and lexeme > U64_MAX_DIGITS)
end

-- Every number serde_json parses as a float becomes a tagged string holding
-- its index in the returned lexeme list, and every null a tagged string.
local function tag_values(text)
  local pieces, lexemes = {}, {}
  local pos, copied = 1, 1
  while true do
    local start = string.find(text, '[%-%dn"]', pos)
    if not start then
      break
    end
    if string.byte(text, start) == QUOTE then
      if
        string.byte(text, start + 1) == BACKSLASH
        and (starts_with_at(text, start, FLOAT_TAG_JSON) or starts_with_at(text, start, NULL_TAG_JSON))
      then
        return nil, nil, RESERVED_STRING
      end
      pos = string_end(text, start) + 1
    elseif starts_with_at(text, start, NULL_LITERAL) then
      table.insert(pieces, string.sub(text, copied, start - 1))
      table.insert(pieces, NULL_TAG_JSON)
      copied = start + #NULL_LITERAL
      pos = copied
    else
      local _, int_end = string.find(text, "^%-?%d+", start)
      local stop = int_end or start
      if int_end then
        local _, frac_end = string.find(text, "^%.%d+", stop + 1)
        stop = frac_end or stop
        local _, exp_end = string.find(text, "^[eE][%+%-]?%d+", stop + 1)
        stop = exp_end or stop
        local lexeme = string.sub(text, start, stop)
        if stop > int_end or overflows_u64(lexeme) then
          table.insert(lexemes, lexeme)
          table.insert(pieces, string.sub(text, copied, start - 1))
          table.insert(pieces, FLOAT_TAG_JSON .. #lexemes .. '"')
          copied = stop + 1
        end
      end
      pos = stop + 1
    end
  end
  table.insert(pieces, string.sub(text, copied))
  return table.concat(pieces), lexemes
end

local function mark(marks, node, key)
  marks[node] = marks[node] or {}
  marks[node][key] = true
end

-- Arrays come back without holes while every null is still a tag, so the
-- length is taken before any tag is cleared.
local function restore(node, values)
  local count = #node
  for key, value in pairs(node) do
    if type(value) == "table" then
      restore(value, values)
    elseif value == NULL_TAG then
      node[key] = nil
      mark(null_keys, node, key)
      if type(key) == "number" then
        array_lens[node] = count
      end
    elseif type(value) == "string" and string.sub(value, 1, #FLOAT_TAG) == FLOAT_TAG then
      node[key] = values[tonumber(string.sub(value, #FLOAT_TAG + 1))]
      mark(float_keys, node, key)
    end
  end
end

--- `maki.json.decode`, plus a record of which numbers serde_json would read
--- as floats and where the nulls were. Returns the value, or nil and an error.
function M.decode(text)
  local tagged, lexemes, err = tag_values(text)
  if err then
    return nil, err
  end
  if tagged == text then
    return maki.json.decode(text)
  end
  local wrapper = maki.json.decode("[" .. tagged .. "]")
  if not wrapper then
    return maki.json.decode(text)
  end
  -- serde_json parses each float, so the value matches Rust's to the bit.
  local values = maki.json.decode("[" .. table.concat(lexemes, ",") .. "]")
  restore(wrapper, values)
  return wrapper[1]
end

--- serde_json `tbl.get(key).is_some_and(Value::is_null)`: true only where the
--- decoded JSON held a null, never for a missing key or a table that did not
--- come from `M.decode`.
function M.is_null(tbl, key)
  if type(tbl) ~= "table" then
    return false
  end
  local nulls = null_keys[tbl]
  return nulls ~= nil and nulls[key] == true
end

--- The JSON array's length, nulls included. `#arr` for a table that did not
--- come from `M.decode`, 0 for a non-table.
function M.len(arr)
  if type(arr) ~= "table" then
    return 0
  end
  return array_lens[arr] or #arr
end

--- Rust `as_array().iter().enumerate()`, 1-based: `for i, v in M.items(arr)`
--- visits every index up to `M.len(arr)`, with v nil for a null element.
function M.items(arr)
  local count = M.len(arr)
  return function(tbl, index)
    index = index + 1
    if index <= count then
      return index, tbl[index]
    end
    return nil
  end,
    arr,
    0
end

--- Rust `get_text` then `serde_json::from_str`: a GET with the provider's
--- resolved `auth`, never retried. Returns the decoded body, nil for a JSON
--- null. On failure returns nil and an error for `M.fail`.
function M.get_json(auth, url)
  local res, err = maki.net.request(url, { headers = auth.headers, retry = NO_RETRY })
  if not res then
    return nil, err
  end
  if res.status ~= HTTP_OK then
    return nil, maki.provider.http_error(res)
  end
  return M.decode(res.body)
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

local function whole_at(tbl, key, max)
  if type(tbl) ~= "table" then
    return nil
  end
  local value = tbl[key]
  if type(value) ~= "number" or value ~= math.floor(value) or value < 0 or value > max or 1 / value < 0 then
    return nil
  end
  local floats = float_keys[tbl]
  if floats and floats[key] then
    return nil
  end
  return value
end

--- serde_json `Value::as_u64` on `tbl[key]`: a non-negative integer, never a
--- float such as `1.0`, `1e3` or `-0`.
function M.as_u64(tbl, key)
  return whole_at(tbl, key, U64_MAX)
end

--- `as_u64` then `u32::try_from(v).ok()`.
function M.as_u32(tbl, key)
  return whole_at(tbl, key, U32_MAX)
end

--- serde_json `Value::as_f64` on `tbl[key]`: any number, integer or float.
function M.as_f64(tbl, key)
  if type(tbl) ~= "table" or type(tbl[key]) ~= "number" then
    return nil
  end
  return tbl[key]
end

--- serde_json `Value::as_bool` on `tbl[key]`.
function M.as_bool(tbl, key)
  if type(tbl) ~= "table" or type(tbl[key]) ~= "boolean" then
    return nil
  end
  return tbl[key]
end

--- Rust `s.parse::<f64>().ok()`: no whitespace, no hex, an optional sign,
--- and `inf`, `infinity` or `nan` in any case. nil for a non-string.
function M.parse_f64(s)
  if type(s) ~= "string" then
    return nil
  end
  local sign, body = string.match(s, "^([%+%-]?)(.*)$")
  local word = string.lower(body)
  if word == "inf" or word == "infinity" then
    return sign == "-" and -math.huge or math.huge
  end
  if word == "nan" then
    return NAN
  end
  local mantissa, exponent = string.match(body, "^([%d%.]*)(.*)$")
  local mantissa_ok = string.find(mantissa, "^%d+%.?%d*$") or string.find(mantissa, "^%.%d+$")
  local exponent_ok = exponent == "" or string.find(exponent, "^[eE][%+%-]?%d+$")
  if not (mantissa_ok and exponent_ok) then
    return nil
  end
  return tonumber(s)
end

--- Rust `x as u64`: truncates, NaN and negatives give 0, saturates at the top.
function M.cast_u64(x)
  if x ~= x or x <= 0 then
    return 0
  end
  return math.min(math.floor(x), U64_MAX)
end

--- Rust `x as u32`: truncates, NaN and negatives give 0, saturates at the top.
function M.cast_u32(x)
  return math.min(M.cast_u64(x), U32_MAX)
end

--- Rust `f64::round`: halves round away from zero.
M.round = math.round

--- Rust `format!("{:.n$}", x)`: the exact binary value rounded, ties to even,
--- so `0.125` prints `0.12`. A whole number with `n = 0` prints like Rust's
--- integer `{}`.
function M.fixed(x, n)
  if x ~= x then
    return "NaN"
  end
  if x == math.huge then
    return "inf"
  end
  if x == -math.huge then
    return "-inf"
  end
  return string.format("%." .. n .. "f", x)
end

--- Rust `sort_by`, in place: stable, so elements that are not `less` than
--- each other keep their order. `less(a, b)` is true when `a` sorts first.
function M.stable_sort_by(list, less)
  local len = #list
  local src, dst = list, {}
  local width = 1
  while width < len do
    for lo = 1, len, 2 * width do
      local mid = math.min(lo + width, len + 1)
      local hi = math.min(lo + 2 * width, len + 1)
      local left, right = lo, mid
      for out = lo, hi - 1 do
        if right < hi and (left >= mid or less(src[right], src[left])) then
          dst[out] = src[right]
          right = right + 1
        else
          dst[out] = src[left]
          left = left + 1
        end
      end
    end
    src, dst = dst, src
    width = width * 2
  end
  if src ~= list then
    table.move(src, 1, len, 1, list)
  end
end

--- The rest of Rust `fetch_and_parse_models`: each `body.data` element through
--- `parse_row`, nils dropped, sorted by `id`. A body without a `data` array
--- lists nothing.
function M.models(body, parse_row)
  local rows = {}
  for _, raw in M.items(type(body) == "table" and body.data) do
    local row = parse_row(raw)
    if row then
      table.insert(rows, row)
    end
  end
  M.stable_sort_by(rows, function(a, b)
    return a.id < b.id
  end)
  return rows
end

return M
