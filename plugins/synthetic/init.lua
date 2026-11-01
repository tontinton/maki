local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"

-- Synthetic validates `reasoning_effort` against one ladder for every model it
-- serves: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` (and
-- `ultra`, which maki has no level for). Its docs still say low/medium/high,
-- and the `/models` listing says nothing about effort, so the knowledge rides
-- on the row and narrows `openai.thinking` for that model at request time;
-- adaptive keeps the dialect's medium.
local LADDER = {
  supported = { "minimal", "low", "medium", "high", "xhigh", "max" },
}

local function parse_model(m)
  if type(m) ~= "table" or type(m.id) ~= "string" then
    return nil
  end
  return {
    id = m.id,
    context_window = parse.as_u32(m.context_length),
    effort = LADDER,
  }
end

maki.provider.register({
  slug = "synthetic",
  display_name = "Synthetic",
  codec = "openai",
  base_url = "https://api.synthetic.new/openai/v1",
  api_key_env = "SYNTHETIC_API_KEY",
  login_url = "https://synthetic.new",
  default_model = "hf:moonshotai/Kimi-K2.5",
  family = "synthetic",
  accepts_arbitrary_models = false,
  max_output_tokens = 32000,
  context_window = 128000,
  aperture = { path_prefix = "/v1" },
  docs = { features = "Reasoning effort (minimal, low, medium, high, xhigh, max), open-weight models" },
  models = {
    {
      prefixes = { "hf:moonshotai/Kimi-K2.5" },
      tier = "strong",
      default = true,
      supports_vision = false,
      max_output_tokens = 131072,
      context_window = 200000,
      pricing = { input = 0.45, output = 3.4, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "hf:deepseek-ai/DeepSeek-V3.2" },
      tier = "medium",
      default = true,
      supports_vision = false,
      max_output_tokens = 131072,
      context_window = 200000,
      pricing = { input = 0.56, output = 1.68, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "hf:zai-org/GLM-4.7-Flash" },
      tier = "weak",
      default = true,
      supports_vision = false,
      max_output_tokens = 131072,
      context_window = 200000,
      pricing = { input = 0.1, output = 0.5, cache_write = 0.0, cache_read = 0.0 },
    },
  },
  openai = {
    max_tokens_field = "max_completion_tokens",
    include_stream_usage = false,
    thinking = { dialect = "standard" },
  },

  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    return parse.models(body, parse_model)
  end,
})
