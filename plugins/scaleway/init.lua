-- Scaleway, as a declaration plus the model knowledge the openai codec
-- cannot spell.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"

-- The effort levels each reasoning model takes, as Scaleway's
-- supported-models page documents them per model, and whether it takes
-- `reasoning_effort: "none"` to opt out; gpt-oss has no off switch. The
-- /models listing says nothing about effort, so the knowledge rides on the
-- row and narrows `openai.thinking` for that model at request time, while
-- adaptive leaves the model at its own default depth. A model the ladders
-- have not met is left there too: no ladder means no support claim, so maki
-- never asks a level the next release might refuse. Longest prefix first.
local LADDERS = {
  { "deepseek-v4.1-flash-", { supported = { "low", "high", "max" }, send_off = true } },
  { "deepseek-v4-flash-", { supported = { "low", "high", "max" }, send_off = true } },
  { "mistral-medium-3.5", { supported = { "high" }, send_off = true } },
  { "gemma-4-26b", { supported = { "low", "medium", "high" }, send_off = true } },
  { "glm-5.3", { supported = { "low", "high", "max" }, send_off = true } },
  { "gpt-oss", { supported = { "low", "medium", "high" }, send_off = false } },
  { "qwen3.5", { supported = { "low", "medium", "high" }, send_off = true } },
  { "qwen3.6", { supported = { "low", "medium", "high" }, send_off = true } },
  { "qwen3.8", { supported = { "low", "medium", "xhigh" }, send_off = true } },
}

local function effort_ladder(id)
  for _, ladder in ipairs(LADDERS) do
    if id:sub(1, #ladder[1]) == ladder[1] then
      return ladder[2]
    end
  end
end

-- The /models listing names ids and says nothing about reasoning, so what
-- discovery knows about a model's thinking comes from here.
local function parse_model(m)
  if type(m) ~= "table" or type(m.id) ~= "string" then
    return nil
  end
  local ladder = effort_ladder(m.id)
  return {
    id = m.id,
    supports_thinking = ladder ~= nil,
    effort = ladder,
  }
end

maki.provider.register({
  slug = "scaleway",
  display_name = "Scaleway",
  codec = "openai",
  base_url = "https://api.scaleway.ai/v1",
  -- The dedicated Generative API name first, then the secret half of a
  -- Scaleway API key, which the API takes as a bearer token just as well:
  -- the first one that is set wins.
  api_key_env = { "SCALEWAY_API_KEY", "SCW_SECRET_KEY" },
  login_url = "https://console.scaleway.com/iam/api-keys",
  default_model = "glm-5.3",
  family = "generic",
  accepts_arbitrary_models = false,
  max_output_tokens = 16384,
  context_window = 256000,
  aperture = { path_prefix = "/v1" },
  docs = {
    features = "EU-hosted models, per-model reasoning efforts, vision models",
  },
  models = {
    -- Prices are quoted only on the tier defaults, and at Scaleway's EUR
    -- list converted to USD at EUR 1 = USD 1.12, the ECB reference rate of
    -- 2026-11-02, so a tier never reads as free even if the catalog lags.
    -- Every other row takes models.dev's live USD rate, which quotes the
    -- same list and gets re-quoted without a release.
    --
    -- Only the chat and code models are here: the embeddings, OCR and
    -- audio rows of the pricing page are not chat models, and everything
    -- else the catalog names is End-of-Life or dedicated-deployment only.
    -- glm-5.2 is left out too, because it is End-of-Life mid-November
    -- 2026, after which the API routes its requests to glm-5.3 anyway.
    -- Only the dated `-1017` id of the DeepSeek flash is carried, because
    -- that is what the API serves; the bare spelling the supported-models
    -- page shows is deliberately not an alias.
    {
      prefixes = { "glm-5.3" },
      tier = "strong",
      default = true,
      family = "glm",
      supports_thinking = true,
      supports_vision = false,
      max_output_tokens = 16384,
      context_window = 256000,
      pricing = { input = 2.02, output = 6.16, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "qwen3.5-397b-a17b" },
      tier = "strong",
      supports_thinking = true,
      supports_vision = true,
      max_output_tokens = 16384,
      context_window = 250000,
    },
    {
      prefixes = { "deepseek-v4.1-flash-1017" },
      tier = "medium",
      default = true,
      supports_thinking = true,
      supports_vision = true,
      max_output_tokens = 32768,
      context_window = 256000,
      pricing = { input = 0.67, output = 2.02, cache_write = 0.0, cache_read = 0.13 },
    },
    {
      prefixes = { "qwen3.8-27b" },
      tier = "medium",
      supports_thinking = true,
      supports_vision = true,
      max_output_tokens = 32768,
      context_window = 256000,
    },
    {
      prefixes = { "mistral-medium-3.5-128b" },
      tier = "medium",
      supports_thinking = true,
      supports_vision = true,
      max_output_tokens = 16384,
      context_window = 180000,
    },
    {
      prefixes = { "llama-3.3-70b-instruct" },
      tier = "medium",
      supports_thinking = false,
      supports_vision = false,
      max_output_tokens = 16384,
      context_window = 100000,
    },
    {
      prefixes = { "qwen3-235b-a22b-instruct-2507" },
      tier = "medium",
      supports_thinking = false,
      supports_vision = false,
      max_output_tokens = 16384,
      context_window = 250000,
    },
    {
      prefixes = { "mistral-small-3.2-24b-instruct-2506" },
      tier = "weak",
      supports_thinking = false,
      supports_vision = true,
      max_output_tokens = 32768,
      context_window = 128000,
    },
    {
      prefixes = { "gemma-4-26b-a4b-it" },
      tier = "weak",
      supports_thinking = true,
      supports_vision = true,
      max_output_tokens = 32768,
      context_window = 256000,
    },
    {
      prefixes = { "qwen3.6-35b-a3b" },
      tier = "weak",
      default = true,
      supports_thinking = true,
      supports_vision = true,
      max_output_tokens = 32768,
      context_window = 256000,
      pricing = { input = 0.28, output = 1.68, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "deepseek-v4-flash-0731" },
      tier = "weak",
      supports_thinking = true,
      supports_vision = false,
      max_output_tokens = 32768,
      context_window = 256000,
    },
    {
      prefixes = { "gpt-oss-120b" },
      tier = "weak",
      supports_thinking = true,
      supports_vision = false,
      max_output_tokens = 32768,
      context_window = 128000,
    },
  },
  openai = {
    -- `stream_options` is not on Scaleway's supported parameter list;
    -- usage arrives on every chunk without asking for it.
    include_stream_usage = false,
    thinking = { dialect = "anthropic-adaptive", requires_support = true },
  },

  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    return parse.models(body, parse_model)
  end,
})
