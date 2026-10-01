-- Requesty, as a declaration plus the one hook the openai codec cannot spell.

local parse = require("maki.provider_parse")

-- Requesty's own curated routing policies, with short stable ids like
-- `claude-sonnet-4-5`, listed before the raw `<vendor>/<model>` catalog.
local MANAGED_PATH = "/models/managed"
local CATALOG_PATH = "/models"
local CHAT_API = "chat"

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
  return read(m, name, parse.as_f64)
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

  return {
    id = m.id,
    context_window = limit(m, "context_window"),
    max_output_tokens = limit(m, "max_output_tokens"),
    pricing = parse.pricing(
      price(m, "input_price"),
      price(m, "output_price"),
      price(m, "caching_price"),
      price(m, "cached_price")
    ),
    supports_thinking = read(m, "supports_reasoning", parse.as_bool) == true,
    supports_vision = read(m, "supports_vision", parse.as_bool) == true,
  }
end

-- A failure is handed back rather than raised, so the other listing can still
-- stand in for it.
local function fetch_listing(ctx, path)
  local body, err = ctx.get_json(path)
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
  slug = "requesty",
  display_name = "Requesty",
  codec = "openai",
  base_url = "https://router.requesty.ai/v1",
  api_key_env = "REQUESTY_API_KEY",
  login_url = "https://app.requesty.ai/api-keys",
  default_model = "openai/gpt-5.5",
  family = "generic",
  accepts_arbitrary_models = true,
  max_output_tokens = 128000,
  context_window = 200000,
  aperture = { path_prefix = "/v1" },
  docs = {
    features = "700+ models behind one key, managed routing policies, EU region",
    discovery_note = "Models are listed live from the API. Managed policies come first, "
      .. "with short ids such as `requesty/claude-sonnet-4-5`, and their `@eu` variants "
      .. "use only EU providers. The full `<vendor>/<model>` catalog follows, "
      .. "e.g. `requesty/openai/gpt-4o-mini`. "
      .. "Get a key at [app.requesty.ai/api-keys](https://app.requesty.ai/api-keys). "
      .. "Set `REQUESTY_BASE_URL=https://router.eu.requesty.ai/v1` to keep all "
      .. "traffic in the EU.",
  },
  openai = {
    -- Requesty passes the effort field on to every upstream, and the ones
    -- without reasoning reject it.
    thinking = { dialect = "prefer-high", requires_support = true },
    headers = { ["HTTP-Referer"] = "https://maki.sh", ["X-Title"] = "maki" },
    -- Requesty only inserts Anthropic cache breakpoints when asked to. Without
    -- this flag Claude pays full input price every turn.
    extra_body = { requesty = { auto_cache = true } },
  },

  -- Both listings go out at once: the picker only shows up once the slowest
  -- provider answers. Either stands in for the other when it fails, and with
  -- both down the managed failure is the one that surfaces.
  list_models = function(ctx)
    local results = maki.async.gather({
      function()
        return fetch_listing(ctx, MANAGED_PATH)
      end,
      function()
        return fetch_listing(ctx, CATALOG_PATH)
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
    return nil, managed.err
  end,
})
