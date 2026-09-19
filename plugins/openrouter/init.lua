-- OpenRouter, as a declaration plus the one hook the openai codec cannot spell.
-- The slug is one maki ships, so claiming it inherits the display name, the key
-- env var and the fallback limits. Everything static about the wire is data:
-- the attribution headers, the cache marker in every body, the session id in
-- the body, and effort under `reasoning.effort`, sent only to a model that
-- reasons. `prefer-high` is only the fallback for a model the listing never
-- described: each listed one narrows it through the `effort` on its row.

local parse = require("maki.provider_parse")

local SLUG = "openrouter"
local MODELS_PATH = "/models"
-- Prices arrive per token, a row wants dollars per million.
local PER_MILLION = 1000000
local TEXT_MODALITY = "text"
local IMAGE_MODALITY = "image"
local REASONING_PARAMETER = "reasoning"

local function lists(modalities, wanted)
  for _, value in parse.items(modalities) do
    if value == wanted then
      return true
    end
  end
  return false
end

local function per_million(prices, key)
  local value = parse.parse_f64(prices[key])
  return value and value * PER_MILLION
end

-- A price we cannot read leaves the pricing unknown rather than free.
local function pricing_of(prices)
  if type(prices) ~= "table" then
    return nil
  end
  local input, output = per_million(prices, "prompt"), per_million(prices, "completion")
  if not (input and output) then
    return nil
  end
  return {
    input = input,
    output = output,
    cache_write = per_million(prices, "input_cache_write") or 0,
    cache_read = per_million(prices, "input_cache_read") or 0,
  }
end

-- The `reasoning` block in its three states: mandatory (always on, Off sends
-- nothing), default enabled (Off sends `none`) and default off (Off sends
-- nothing, any effort turns it on). Effort names go through as listed, and
-- maki drops the ones it has no level for.
local function effort_of(reasoning)
  if type(reasoning) ~= "table" then
    return nil
  end
  local supported = {}
  for _, name in parse.items(reasoning.supported_efforts) do
    if type(name) == "string" then
      table.insert(supported, name)
    end
  end
  return {
    supported = supported,
    send_off = reasoning.default_enabled == true and reasoning.mandatory ~= true,
  }
end

-- Only text-in, text-out models are listed.
local function parse_model(m)
  if type(m) ~= "table" or type(m.architecture) ~= "table" then
    return nil
  end
  local input, output = m.architecture.input_modalities, m.architecture.output_modalities
  if type(input) ~= "table" or type(output) ~= "table" then
    return nil
  end
  if not (lists(input, TEXT_MODALITY) and lists(output, TEXT_MODALITY)) then
    return nil
  end
  if type(m.id) ~= "string" then
    return nil
  end

  local effort = effort_of(m.reasoning)
  return {
    id = m.id,
    context_window = parse.as_u32(m, "context_length"),
    pricing = pricing_of(m.pricing),
    supports_thinking = effort ~= nil or lists(m.supported_parameters, REASONING_PARAMETER),
    supports_vision = lists(input, IMAGE_MODALITY),
    effort = effort,
  }
end

maki.provider.register({
  slug = SLUG,
  codec = "openai",
  openai = {
    thinking = { dialect = "prefer-high", field = "reasoning.effort", requires_support = true },
    headers = { ["HTTP-Referer"] = "https://maki.sh", ["X-OpenRouter-Title"] = "maki" },
    -- Marks the whole prompt as cacheable, for the upstreams that only cache
    -- when asked to.
    extra_body = { cache_control = { type = "ephemeral" } },
    session_id = { body_field = "session_id" },
  },

  list_models = function()
    local auth = assert(maki.provider.auth.resolved(SLUG))
    local body, err = parse.get_json(auth, auth.base_url .. MODELS_PATH)
    if err then
      return parse.fail(err)
    end
    return parse.models(body, parse_model)
  end,
})
