-- DeepSeek, as a declaration plus the two hooks the openai codec cannot spell.
-- The slug is one maki ships, so claiming it inherits the display name, the key
-- env var, the curated model table and the peak-hour pricing. Restating any of
-- those here, or the codec's own max tokens field and stream usage, is a
-- registration error rather than an override.

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
  codec = "openai",
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
