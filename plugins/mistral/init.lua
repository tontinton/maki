-- Mistral, as a declaration plus the two hooks the openai codec cannot spell.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"

-- Rows whose cards publish an effort ladder wider than the provider default,
-- as the API answers when asked: medium is refused outside Large 4, and the
-- GLM rows split between a full and a low/high/max ladder. The /models
-- listing says nothing about effort, so the knowledge rides on the row and
-- narrows `openai.thinking` for that model at request time; adaptive keeps
-- the dialect's high. Off unless the user opts in with
-- `plugins.mistral.effort_ladders = true`, since a stale row would silently
-- refuse a level the model grew to accept. Longest prefix first.
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

local opts = maki.api.register_options({
  responses_api = {
    default = false,
    type = "boolean",
    desc = "Speak the Responses API (/v1/responses) instead of chat completions (/v1/chat/completions). The thinking dialect and effort ladder apply to both.",
  },
  effort_ladders = {
    default = false,
    type = "boolean",
    desc = "Attach each discovered model's published reasoning-effort ladder (the Large 4, DeepSeek and GLM rows), so those models accept low/medium effort instead of the high-only default.",
  },
})

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

-- Mistral caches on the `prompt_cache_key` body field and routes a session to
-- one backend by the `x-affinity` header; both codecs send them, the
-- responses path renders them in the shapes its endpoint spells.
local openai = {
  thinking = { dialect = "high-only" },
  -- Mistral's small models refuse reasoning whatever the model table says.
  thinking_overrides = { ["ministral-"] = "no" },
  session_id = { header = "x-affinity", body_field = "prompt_cache_key" },
}

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
    effort = opts.effort_ladders and effort_ladder(m.id) or nil,
  }
end

maki.provider.register({
  slug = "mistral",
  display_name = "Mistral",
  codec = opts.responses_api and "openai-responses" or "openai",
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
  docs = {
    features = "Chat or Responses API (`plugins.mistral.responses_api = true`); reasoning effort high-only, or the ladder each model's card publishes with `plugins.mistral.effort_ladders = true`",
  },
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
  openai = openai,

  build_body = function(_, body)
    -- The responses wire has no `messages`; its input conversion drops
    -- thinking blocks, and the `thinking` part conversion below is chat-only.
    if not opts.responses_api and type(body.messages) == "table" then
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
