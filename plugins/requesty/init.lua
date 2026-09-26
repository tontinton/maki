-- Requesty, as a declaration plus the one hook the openai codec cannot spell.
-- The slug is one maki ships, so claiming it inherits the display name, the key
-- env var and the fallback limits. Everything static about the wire is data:
-- the attribution headers, the cache flag in every body, and effort sent only to
-- a model that reasons, since Requesty hands the field on to upstreams that
-- reject it.

local parse = require("maki.provider_parse")

local SLUG = "requesty"
-- Requesty's own curated routing policies, with short stable ids like
-- `claude-sonnet-4-5`, listed before the raw `<vendor>/<model>` catalog.
local MANAGED_PATH = "/models/managed"
local CATALOG_PATH = "/models"
local CHAT_API = "chat"
-- Prices arrive per token, a row wants dollars per million.
local PER_MILLION = 1000000

-- A missing or null field is the normal "Requesty does not say" and stays
-- quiet, while a field in a shape we cannot read is upstream drift: it gets a
-- log line and costs that one value instead of the whole model.
local function read(m, name, reader)
  if m[name] == nil then
    return nil
  end
  local parsed = reader(m[name])
  if parsed == nil then
    maki.log.warn(
      string.format(
        "requesty: unreadable field, ignoring it: model=%s field=%s value=%s",
        m.id,
        name,
        (maki.json.encode(m[name]))
      )
    )
  end
  return parsed
end

-- Requesty sends `0` for a limit it does not know. Kept, it would beat the spec
-- fallback and go out as `"max_tokens": 0`.
local function limit(m, name)
  local value = read(m, name, parse.as_u32)
  if value and value > 0 then
    return value
  end
  return nil
end

local function price(m, name)
  local value = read(m, name, parse.as_f64)
  return value and value * PER_MILLION
end

-- Managed policies and the full catalog share this shape, so one parser reads
-- both.
local function parse_model(m)
  if type(m) ~= "table" or type(m.id) ~= "string" then
    return nil
  end
  -- The catalog also lists embedding and other non chat APIs.
  if type(m.api) == "string" and m.api ~= CHAT_API then
    return nil
  end

  -- Half a price is no price: without both sides it would read as free.
  local pricing
  local input, output = price(m, "input_price"), price(m, "output_price")
  if input and output then
    pricing = {
      input = input,
      output = output,
      cache_write = price(m, "caching_price") or 0,
      cache_read = price(m, "cached_price") or 0,
    }
  end

  return {
    id = m.id,
    context_window = limit(m, "context_window"),
    max_output_tokens = limit(m, "max_output_tokens"),
    pricing = pricing,
    supports_thinking = read(m, "supports_reasoning", parse.as_bool) == true,
    supports_vision = read(m, "supports_vision", parse.as_bool) == true,
  }
end

-- A failure is handed back rather than raised, so the other listing can still
-- stand in for it.
local function fetch_listing(auth, path)
  local body, err = parse.get_json(auth, auth.base_url .. path)
  if err then
    return { err = err }
  end
  return { models = parse.models(body, parse_model) }
end

-- Managed policies first, then the full catalog, deduplicated by id.
local function merge(managed, catalog)
  local merged, seen = {}, {}
  for _, model in ipairs(managed) do
    table.insert(merged, model)
    seen[model.id] = true
  end
  for _, model in ipairs(catalog) do
    if not seen[model.id] then
      table.insert(merged, model)
      seen[model.id] = true
    end
  end
  return merged
end

local function settled(result)
  if result.ok then
    return result.value
  end
  return { err = result.err }
end

maki.provider.register({
  slug = SLUG,
  codec = "openai",
  openai = {
    thinking = { dialect = "prefer-high", requires_support = true },
    headers = { ["HTTP-Referer"] = "https://maki.sh", ["X-Title"] = "maki" },
    -- Requesty only inserts Anthropic cache breakpoints when asked to. Without
    -- this flag Claude pays full input price every turn.
    extra_body = { requesty = { auto_cache = true } },
  },

  -- Both listings go out at once: the picker only shows up once the slowest
  -- provider answers. Either stands in for the other when it fails, and with
  -- both down the managed failure is the one that surfaces.
  list_models = function()
    local auth = assert(maki.provider.auth.resolved(SLUG))
    local results = maki.async.gather({
      function()
        return fetch_listing(auth, MANAGED_PATH)
      end,
      function()
        return fetch_listing(auth, CATALOG_PATH)
      end,
    })
    local managed, catalog = settled(results[1]), settled(results[2])
    if managed.models and catalog.models then
      return merge(managed.models, catalog.models)
    end
    if managed.models then
      maki.log.warn("requesty: full catalog unavailable, listing managed models only: " .. tostring(catalog.err))
      return managed.models
    end
    if catalog.models then
      maki.log.warn("requesty: managed models unavailable, listing full catalog only: " .. tostring(managed.err))
      return catalog.models
    end
    return parse.fail(managed.err)
  end,
})
