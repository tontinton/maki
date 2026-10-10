-- GreenPT, an OpenAI-compatible endpoint with no API for model metadata, so
-- the curated table is the catalogue and `list_models` only drops an id the
-- API no longer lists.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"
local API_KEY_ENV = "GREENPT_API_KEY"

-- Prices are GreenPT's list in EUR per 1M tokens, stored in maki's dollar
-- fields as-is. GreenPT bills no cache write, so cache_write is 0.0. It
-- enables reasoning by default: omitting the effort field leaves thinking on,
-- so every model that supports disabling reasoning explicitly sets it to off
-- if requested.
local MODELS = {
  {
    prefixes = { "glm-5.3" },
    tier = "strong",
    context_window = 1000000,
    supports_thinking = true,
    pricing = { input = 1.10, output = 4.40, cache_write = 0.0, cache_read = 0.275 },
    thinking_fields = { off = { reasoning_effort = "none" } },
  },
  {
    prefixes = { "kimi-k3" },
    tier = "strong",
    context_window = 1000000,
    supports_thinking = true,
    supports_vision = true,
    pricing = { input = 3.30, output = 16.50, cache_write = 0.0, cache_read = 0.825 },
    thinking_fields = { off = { reasoning_effort = "none" } },
  },
  {
    prefixes = { "deepseek-v4.1-flash" },
    tier = "medium",
    context_window = 1000000,
    supports_thinking = true,
    supports_vision = true,
    pricing = { input = 0.22, output = 1.10, cache_write = 0.0, cache_read = 0.011 },
    thinking_fields = { off = { reasoning_effort = "none" } },
  },
  {
    prefixes = { "glm-5.3-flash" },
    tier = "medium",
    context_window = 1000000,
    supports_thinking = true,
    pricing = { input = 0.11, output = 0.44, cache_write = 0.0, cache_read = 0.022 },
    thinking_fields = { off = { reasoning_effort = "none" } },
  },
  {
    -- Reasons whether asked or not and accepts no off, so off clamps to
    -- minimal.
    prefixes = { "minimax-m2.5" },
    tier = "medium",
    context_window = 196608,
    max_output_tokens = 65536,
    requires_thinking = true,
    pricing = { input = 0.33, output = 1.32, cache_write = 0.0, cache_read = 0.0825 },
  },
}

local KNOWN = {}
for _, row in ipairs(MODELS) do
  KNOWN[row.prefixes[1]] = true
end

maki.provider.register({
  slug = "greenpt",
  display_name = "GreenPT",
  codec = "openai",
  base_url = "https://api.greenpt.ai/v1",
  openai = { thinking = { dialect = "standard" } },
  models = MODELS,

  auth = function()
    local key = maki.uv.os_getenv(API_KEY_ENV)
    if not key or key == "" then
      return nil, "set " .. API_KEY_ENV .. " to use greenpt"
    end
    return { headers = { Authorization = "Bearer " .. key } }
  end,

  -- /models also lists embedding, speech and rerank model ids. Anything the
  -- table does not list is filtered out as we can't know from the models list
  -- what kind of model is behind the id.
  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    return parse.models(body, function(m)
      if type(m) ~= "table" or type(m.id) ~= "string" or KNOWN[m.id] == nil then
        return nil
      end
      return { id = m.id }
    end)
  end,
})
