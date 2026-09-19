-- Mistral, as a declaration plus the two hooks the openai codec cannot spell.
-- The slug is one maki ships, so claiming it inherits the display name, the key
-- env var, the curated model table and the login plans. Restating any of those
-- here, or the codec's own max tokens field and stream usage, is a
-- registration error rather than an override.

local parse = require("maki.provider_parse")

local SLUG = "mistral"
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
    context_window = parse.as_u32(m, "max_context_length"),
    supports_thinking = parse.as_bool(capabilities, "reasoning"),
    supports_vision = capabilities.vision == true,
  }
end

maki.provider.register({
  slug = SLUG,
  codec = "openai",
  openai = {
    thinking = { dialect = "high-only" },
    session_id = { header = "x-affinity" },
    -- Mistral's small models refuse reasoning whatever the model table says.
    thinking_overrides = { ["ministral-"] = "no" },
  },

  build_body = function(body)
    if type(body.messages) == "table" then
      convert_assistant_messages(body.messages)
    end
    return body
  end,

  -- Mistral's `/models` lists embedding, OCR and moderation models too, and
  -- names its fields its own way.
  list_models = function()
    local auth = assert(maki.provider.auth.resolved(SLUG))
    local body, err = parse.get_json(auth, auth.base_url .. MODELS_PATH)
    if err then
      return parse.fail(err)
    end
    return parse.models(body, parse_model)
  end,
})
