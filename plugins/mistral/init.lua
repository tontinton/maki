-- Mistral on the Responses API. The codec already speaks the item format.
-- Effort, the session key and reasoning replay are the parts it does not
-- spell on its own.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"

-- Rows whose cards publish an effort ladder wider than the provider default,
-- as the API answers when asked: medium is refused outside Large 4, and the
-- GLM rows split between a full and a low/high/max ladder. The /models
-- listing says nothing about effort, so the knowledge rides on the row and
-- narrows `openai.thinking` for that model at request time; adaptive keeps
-- the dialect's high. Longest prefix first.
local LADDERS = {
  { "mistral-large-4", { supported = { "low", "medium", "high" } } },
  { "zai-glm-5-2", { supported = { "low", "medium", "high", "xhigh", "max" } } },
  { "glm-5-2", { supported = { "low", "medium", "high", "xhigh", "max" } } },
  { "zai-glm-5", { supported = { "low", "high", "max" } } },
  { "zai-glm-latest", { supported = { "low", "high", "max" } } },
  { "deepseek-", { supported = { "low", "high", "xhigh", "max" } } },
}

local function effort_ladder(id)
  for _, ladder in ipairs(LADDERS) do
    if id:sub(1, #ladder[1]) == ladder[1] then
      return ladder[2]
    end
  end
end

-- Only chat-capable rows survive. `vision` defaults to off, where an unstated
-- `reasoning` stays unstated.
local function parse_model(m)
  local capabilities = type(m) == "table" and m.capabilities
  if type(capabilities) ~= "table" or capabilities.completion_chat ~= true or type(m.id) ~= "string" then
    return nil
  end
  return {
    id = m.id,
    context_window = parse.as_u32(m.max_context_length),
    supports_thinking = parse.as_bool(capabilities.reasoning),
    supports_vision = capabilities.vision == true,
    effort = effort_ladder(m.id),
  }
end

maki.provider.register({
  slug = "mistral",
  display_name = "Mistral",
  codec = "openai-responses",
  base_url = "https://api.mistral.ai/v1",
  api_key_env = "MISTRAL_API_KEY",
  login_url = "https://admin.mistral.ai/organization/api-keys",
  default_model = "mistral-large-4",
  plans = {
    { key = "standard", display_name = "Standard", default_model = "mistral-large-4" },
    {
      key = "coding",
      display_name = "Vibe / Coding",
      default_model = "mistral-vibe-cli-latest",
      login_url = "https://chat.mistral.ai/code/extensions",
    },
  },
  family = "generic",
  accepts_arbitrary_models = true,
  -- The /models listing only reports max_context_length; rows whose model card
  -- publishes an output cap state it themselves.
  max_output_tokens = false,
  context_window = 128000,
  aperture = { path_prefix = "/v1" },
  models = {
    {
      prefixes = { "mistral-large-4", "mistral-large-4-0" },
      tier = "strong",
      default = true,
      supports_vision = true,
      context_window = 1000000,
      pricing = { input = 0.7, output = 2.1, cache_write = 0.0, cache_read = 0.07 },
    },
    {
      prefixes = { "mistral-medium-latest", "mistral-medium-3.5", "mistral-medium-3-5", "mistral-medium-2604" },
      tier = "strong",
      supports_vision = true,
      context_window = 262144,
      pricing = { input = 1.5, output = 7.5, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "zai-glm-latest", "zai-glm-5-3", "zai-glm-5" },
      tier = "strong",
      family = "glm",
      supports_vision = false,
      max_output_tokens = 131072,
      context_window = 1000000,
      pricing = { input = 1.4, output = 4.4, cache_write = 0.0, cache_read = 0.14 },
    },
    {
      prefixes = { "glm-5-2", "zai-glm-5-2" },
      tier = "strong",
      family = "glm",
      supports_vision = false,
      max_output_tokens = 131072,
      context_window = 1000000,
      pricing = { input = 1.4, output = 4.4, cache_write = 0.0, cache_read = 0.14 },
    },
    {
      prefixes = { "mistral-small-latest", "mistral-small-2603" },
      tier = "medium",
      default = true,
      supports_vision = true,
      context_window = 262144,
      pricing = { input = 0.15, output = 0.6, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "deepseek-v4-1-flash-1017" },
      tier = "medium",
      supports_vision = true,
      max_output_tokens = 131072,
      context_window = 1000000,
      pricing = { input = 0.3, output = 1.2, cache_write = 0.0, cache_read = 0.007 },
    },
    {
      prefixes = { "zai-glm-5-3-flash" },
      tier = "weak",
      default = true,
      family = "glm",
      supports_vision = true,
      max_output_tokens = 131072,
      context_window = 1000000,
      pricing = { input = 0.15, output = 0.5, cache_write = 0.0, cache_read = 0.03 },
    },
    {
      prefixes = { "ministral-14b-latest", "ministral-14b-2512" },
      tier = "weak",
      supports_vision = false,
      context_window = 262144,
      pricing = { input = 0.2, output = 0.2, cache_write = 0.0, cache_read = 0.0 },
    },
  },
  openai = {
    -- Responses spells effort as `reasoning.effort` and has no summary field.
    thinking = { dialect = "high-only", field = "reasoning.effort" },
    -- Mistral caches on the `prompt_cache_key` body field and routes a
    -- session to one backend by the `x-affinity` header; send both.
    session_id = { header = "x-affinity", body_field = "prompt_cache_key" },
    -- Mistral's small models refuse reasoning whatever the model table says.
    thinking_overrides = { ["ministral-"] = "no" },
    -- The API is stateless. Prior thinking goes back as a reasoning item.
    replay_reasoning = true,
  },

  -- Mistral's `/models` lists embedding, OCR and moderation models too, and
  -- names its fields its own way.
  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    return parse.models(body, parse_model)
  end,
})
