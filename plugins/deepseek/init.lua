-- DeepSeek, as a declaration plus the two hooks the openai codec cannot spell.

local BALANCE_PATH = "/user/balance"
-- The API only checks that the field exists.
local PAD = ""
-- R1 is the one model outside the thinking protocol DeepSeek introduced with
-- V4: it reasons unconditionally and refuses `reasoning_content` as input. The
-- gate names that id rather than matching a version marker in the others,
-- which a rename has broken once already.
local REASONER = "deepseek-reasoner"
local CURRENCY_SYMBOLS = { USD = "$", CNY = "¥" }

local function uses_v4_thinking_protocol(model_id)
  return model_id:sub(1, #REASONER) ~= REASONER
end

-- V4 and later answer 400 to a request that carries `tools` and an assistant
-- turn without `reasoning_content`, so the turns that have none are back-filled:
-- plain replies and tool-only turns. Requests without tools are left alone,
-- since nothing asks for the field there.
local function pad_reasoning_content(body, model)
  if not body.tools or not uses_v4_thinking_protocol(model) then
    return
  end
  for _, message in ipairs(body.messages or {}) do
    if message.role == "assistant" and type(message.reasoning_content) ~= "string" then
      message.reasoning_content = PAD
    end
  end
end

local function balance_limit(info)
  local symbol = CURRENCY_SYMBOLS[info.currency] or ""
  return {
    label = "Balance",
    detail = string.format(
      "total: %s%s, topped-up: %s%s, granted: %s%s",
      symbol,
      info.total_balance,
      symbol,
      info.topped_up_balance,
      symbol,
      info.granted_balance
    ),
  }
end

maki.provider.register({
  slug = "deepseek",
  display_name = "DeepSeek",
  codec = "openai",
  base_url = "https://api.deepseek.com",
  api_key_env = "DEEPSEEK_API_KEY",
  login_url = "https://platform.deepseek.com/api_keys",
  default_model = "deepseek-flash",
  family = "generic",
  accepts_arbitrary_models = false,
  max_output_tokens = 384000,
  context_window = 1000000,
  aperture = { path_prefix = "/v1" },
  -- Peak hours double every rate, and the rows below quote the off-peak ones.
  -- https://api-docs.deepseek.com/quick_start/pricing/
  pricing_schedule = { windows = { { 1, 4 }, { 6, 10 } }, multiplier = 2, weekdays_only = true },
  docs = { features = "Thinking on or off, open-weight models" },
  models = {
    -- `deepseek-flash` is V4.1 Flash. `deepseek-v4-flash` is the retired name
    -- the API still accepts, served by V4.1 Flash at its rates.
    {
      prefixes = { "deepseek-flash", "deepseek-v4-flash" },
      tier = "medium",
      default = true,
      supports_vision = true,
      pricing = { input = 0.15, output = 0.6, cache_write = 0.0, cache_read = 0.003 },
    },
    {
      prefixes = { "deepseek-v4-pro" },
      tier = "strong",
      default = true,
      supports_vision = false,
      pricing = { input = 0.66, output = 1.98, cache_write = 0.0, cache_read = 0.022 },
    },
  },
  openai = { thinking = { dialect = "deepseek" } },

  build_body = function(_, body, model, opts)
    local enabled = opts.thinking ~= nil
    body.thinking = { type = enabled and "enabled" or "disabled" }
    if enabled then
      pad_reasoning_content(body, model)
    end
    return body
  end,

  -- Asked of the configured origin, so a user who points the slug at a
  -- gateway does not have their balance read straight from DeepSeek with the
  -- gateway's key.
  fetch_usage = function(ctx)
    local parsed, err = ctx.get_json(BALANCE_PATH)
    if err then
      return nil, err
    end

    local limits = {}
    for _, info in ipairs(parsed.balance_infos or {}) do
      table.insert(limits, balance_limit(info))
    end
    return { limits = limits }
  end,
})
