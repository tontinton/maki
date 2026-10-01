-- Mistral, as a declaration plus the two hooks the openai codec cannot spell.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"

-- Mistral takes reasoning back as a `thinking` part at the front of the
-- assistant turn's `content`, not as the `reasoning_content` the codec writes.
-- A non-string `reasoning_content` is dropped.
local function convert_assistant_messages(messages)
  for _, message in ipairs(messages) do
    if type(message) == "table" and message.role == "assistant" then
      local reasoning = message.reasoning_content
      message.reasoning_content = nil
      if type(reasoning) == "string" then
        local thinking = { type = "thinking", thinking = { { type = "text", text = reasoning } } }
        local content = message.content
        if type(content) == "string" and content ~= "" then
          message.content = { thinking, { type = "text", text = content } }
        elseif type(content) == "table" and content[1] ~= nil then
          table.insert(content, 1, thinking)
        else
          message.content = { thinking }
        end
      end
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
  }
end

maki.provider.register({
  slug = "mistral",
  display_name = "Mistral",
  codec = "openai",
  base_url = "https://api.mistral.ai/v1",
  api_key_env = "MISTRAL_API_KEY",
  login_url = "https://admin.mistral.ai/organization/api-keys",
  default_model = "mistral-medium-latest",
  plans = {
    { key = "standard", display_name = "Standard", default_model = "mistral-medium-latest" },
    {
      key = "coding",
      display_name = "Vibe / Coding",
      default_model = "mistral-vibe-cli-latest",
      login_url = "https://console.mistral.ai/codestral/cli",
    },
  },
  family = "generic",
  accepts_arbitrary_models = true,
  -- Mistral publishes no output caps.
  max_output_tokens = false,
  context_window = 128000,
  aperture = { path_prefix = "/v1" },
  models = {
    {
      prefixes = { "mistral-medium-latest", "mistral-medium-3.5", "mistral-medium-3-5", "mistral-medium-2604" },
      tier = "strong",
      default = true,
      supports_vision = true,
      context_window = 262144,
      pricing = { input = 1.5, output = 7.5, cache_write = 0.0, cache_read = 0.0 },
    },
    {
      prefixes = { "zai-glm-latest", "zai-glm-5-3", "zai-glm-5" },
      tier = "strong",
      family = "glm",
      supports_vision = false,
      context_window = 1000000,
      pricing = { input = 1.4, output = 4.4, cache_write = 0.0, cache_read = 0.14 },
    },
    {
      prefixes = { "glm-5-2", "zai-glm-5-2" },
      tier = "strong",
      family = "glm",
      supports_vision = false,
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
      prefixes = { "ministral-14b-latest", "ministral-14b-2512" },
      tier = "weak",
      default = true,
      supports_vision = false,
      context_window = 262144,
      pricing = { input = 0.2, output = 0.2, cache_write = 0.0, cache_read = 0.0 },
    },
  },
  openai = {
    thinking = { dialect = "high-only" },
    session_id = { header = "x-affinity" },
    -- Mistral's small models refuse reasoning whatever the model table says.
    thinking_overrides = { ["ministral-"] = "no" },
  },

  build_body = function(_, body)
    if type(body.messages) == "table" then
      convert_assistant_messages(body.messages)
    end
    return body
  end,

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
