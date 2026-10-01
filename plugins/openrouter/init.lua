-- OpenRouter, as a declaration plus the one hook the openai codec cannot spell.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"
local TEXT_MODALITY = "text"
local IMAGE_MODALITY = "image"
local REASONING_PARAMETER = "reasoning"

-- Lists are read as sets, with `pairs`: a null leaves a hole that would stop
-- `ipairs` early.
local function lists(modalities, wanted)
  for _, value in pairs(type(modalities) == "table" and modalities or {}) do
    if value == wanted then
      return true
    end
  end
  return false
end

-- Prices arrive per token as decimal strings. Routers like `openrouter/auto`
-- list `-1`, because the price depends on the pick, so a negative is unknown.
local function per_token(prices, key)
  local value = prices[key]
  if type(value) ~= "string" then
    return nil
  end
  local parsed = tonumber(value)
  if parsed and parsed >= 0 then
    return parsed
  end
end
local function pricing_of(prices)
  if type(prices) ~= "table" then
    return nil
  end
  return parse.pricing(
    per_token(prices, "prompt"),
    per_token(prices, "completion"),
    per_token(prices, "input_cache_write"),
    per_token(prices, "input_cache_read")
  )
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
  local listed = reasoning.supported_efforts
  for _, name in pairs(type(listed) == "table" and listed or {}) do
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
    context_window = parse.as_u32(m.context_length),
    pricing = pricing_of(m.pricing),
    supports_thinking = effort ~= nil or lists(m.supported_parameters, REASONING_PARAMETER),
    supports_vision = lists(input, IMAGE_MODALITY),
    effort = effort,
  }
end

maki.provider.register({
  slug = "openrouter",
  display_name = "OpenRouter",
  codec = "openai",
  base_url = "https://openrouter.ai/api/v1",
  api_key_env = "OPENROUTER_API_KEY",
  login_url = "https://openrouter.ai/keys",
  default_model = "openai/gpt-5.5",
  family = "generic",
  accepts_arbitrary_models = true,
  max_output_tokens = 128000,
  context_window = 200000,
  aperture = { path_prefix = "/v1" },
  docs = {
    features = "300+ models behind one key, prompt caching, provider routing",
    discovery_note = "Use any model id from [openrouter.ai/models](https://openrouter.ai/models), "
      .. "e.g. `openrouter/anthropic/claude-sonnet-4`.",
  },
  openai = {
    -- `prefer-high` is only for models the listing never described. Each listed
    -- model narrows it with the `effort` on its row.
    thinking = { dialect = "prefer-high", field = "reasoning.effort", requires_support = true },
    headers = { ["HTTP-Referer"] = "https://maki.sh", ["X-OpenRouter-Title"] = "maki" },
    -- Marks the whole prompt as cacheable, for the upstreams that only cache
    -- when asked to.
    extra_body = { cache_control = { type = "ephemeral" } },
    session_id = { body_field = "session_id" },
  },

  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    return parse.models(body, parse_model)
  end,
})
