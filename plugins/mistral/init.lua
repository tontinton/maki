-- Mistral, as a declaration plus the hooks the openai codec cannot spell.

local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"
-- The billing endpoints sit in the versioned API, next to /models. The budget
-- needs the `see_org_inference_usage` permission, so its failure is a normal
-- "no allowance to show", not an error.
local SUBSCRIPTION_PATH = "/billing/subscription"
local BUDGET_PATH = "/billing/budget"

-- Rows whose cards publish an effort ladder wider than the provider default,
-- as the API answers when asked: medium is refused outside Large 4, and the
-- GLM rows take low/high/max. The /models listing says nothing about effort,
-- so the knowledge rides on the row and narrows `openai.thinking` for that
-- model at request time; adaptive keeps the dialect's high. Longest prefix
-- first.
local LADDERS = {
  { "mistral-large-4", { supported = { "low", "medium", "high" } } },
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

local function plan_name(subscription)
  local name = subscription and subscription.plan_name
  return type(name) == "string" and name or nil
end

-- The monthly Vibe Code allowance. A budget of zero or less is no allowance,
-- and past 100% is billed instead of paused when pay-as-you-go is on.
local function budget_limit(budget)
  local detail = budget and budget.vibe_budget
  if type(detail) ~= "table" then
    return nil
  end
  local percentage = parse.as_f64(detail.usage_percentage)
  local initial = parse.as_f64(detail.initial_budget)
  if percentage == nil or initial == nil or initial <= 0 then
    return nil
  end
  local reset = detail.reset_at
  if reset ~= nil and type(reset) ~= "string" then
    reset = nil
  end
  local payg = detail.payg_enabled == true
  return {
    label = "Monthly Vibe Code allowance",
    percentage = math.clamp(math.floor(percentage), 0, 100),
    reset_at = reset,
    detail = payg and "past 100% is billed (PAYG)" or nil,
  }
end

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
    effort = effort_ladder(m.id),
  }
end

maki.provider.register({
  slug = "mistral",
  display_name = "Mistral",
  codec = "openai",
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
  docs = {
    features = "`/usage` reads the plan and monthly Vibe Code allowance from `/v1/billing/subscription` and `/v1/billing/budget` (the budget needs the `see_org_inference_usage` permission)",
  },
  models = {
    {
      prefixes = { "mistral-large-4", "mistral-large-4-0" },
      tier = "strong",
      default = true,
      supports_vision = true,
      context_window = 1000000,
      pricing = { input = 0.7, output = 2.1, cache_write = 0.0, cache_read = 0.07 },
    },
    -- Mistral Medium 3.5 retires 2027-04-30. The Mistral model deprecation
    -- mail from Oct 30 2026 names only `mistral-medium-3-5`, so re-check
    -- whether `-latest` and `-2604` survive before dropping this row.
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
    thinking = { dialect = "high-only" },
    -- Mistral's small models refuse reasoning whatever the model table says.
    thinking_overrides = { ["ministral-"] = "no" },
  },
  -- Mistral caches on the `prompt_cache_key` body field and routes a session
  -- to one backend by the `x-affinity` header; send both.
  session_id = { headers = { "x-affinity" }, body_field = "prompt_cache_key" },

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

  -- The plan and the monthly Vibe Code allowance, read in one round trip:
  -- neither needs the other's answer. A failed subscription is the hook's
  -- error: the key is refused. The budget is optional, though: a key without
  -- the usage permission, or a self-hosted platform without billing, answers
  -- without it.
  fetch_usage = function(ctx)
    -- gather keeps only each child's first return value, so a child answers
    -- with a single table rather than a (body, err) pair.
    local results = maki.async.gather({
      function()
        local body, err = ctx.get_json(SUBSCRIPTION_PATH)
        return { body = body, err = err }
      end,
      function()
        local body, err = ctx.get_json(BUDGET_PATH)
        return { body = body, err = err }
      end,
    })
    local function unwrap(result)
      return result.ok and result.value or { err = result.err }
    end
    local subscription, budget = unwrap(results[1]), unwrap(results[2])
    if subscription.err then
      return nil, subscription.err
    end
    local limits = {}
    if type(budget.body) == "table" then
      local limit = budget_limit(budget.body)
      if limit then
        table.insert(limits, limit)
      end
    end
    return { plan = plan_name(subscription.body), limits = limits }
  end,
})
