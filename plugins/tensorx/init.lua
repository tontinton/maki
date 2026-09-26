-- TensorX, as a declaration plus the two hooks the openai codec cannot spell.
-- The slug is one maki ships, so claiming it inherits the display name, the key
-- env var and the fallback limits. Restating any of those here, or the codec's
-- own max tokens field and stream usage, is a registration error rather than
-- an override.
--
-- The declared dialect writes `reasoning_effort` on every request, which is
-- right for a model that advertises the knob and wrong for every other one, so
-- `build_body` takes it back off where discovery did not vouch for it.

local parse = require("maki.provider_parse")

local SLUG = "tensorx"
local MODEL_INFO_PATH = "/model/info"
-- `opts.thinking` arrives rendered, and this is the one rendering that means
-- disabled.
local THINKING_OFF = "off"
-- The name of the knob both in `supported_openai_params` and on the wire.
local THINKING = "thinking"
local REASONING_EFFORT = "reasoning_effort"
local CHAT_MODE = "chat"
local PER_MILLION = 1000000.0
-- TensorX namespaces resold models by vendor, so DeepSeek ids arrive as
-- `deepseek/deepseek-flash`.
local DEEPSEEK_VENDOR_PREFIX = "deepseek/"
-- R1 is the one DeepSeek model outside the thinking protocol introduced with
-- V4, which is what takes the toggle through the chat template. The gate names
-- that id rather than matching a version marker in the others, which a rename
-- has broken once already.
local DEEPSEEK_REASONER = "deepseek-reasoner"

local function starts_with(text, prefix)
  return text:sub(1, #prefix) == prefix
end

local function uses_v4_thinking_protocol(model_id)
  return not starts_with(model_id, DEEPSEEK_REASONER)
end

local function price(info, key)
  return (parse.as_f64(info[key]) or 0) * PER_MILLION
end

-- Which of the two thinking knobs a model advertises, handed back to
-- `build_body` as `opts.model_info`. nil when the entry lists no params.
local function knobs(info)
  local params = info.supported_openai_params
  if type(params) ~= "table" then
    return nil
  end
  local found = { has_thinking = false, has_reasoning_effort = false }
  for _, param in pairs(params) do
    if param == THINKING then
      found.has_thinking = true
    elseif param == REASONING_EFFORT then
      found.has_reasoning_effort = true
    end
  end
  return found
end

-- One entry of `/model/info`. nil for anything that is not a chat model, and
-- for an entry without `model_info`. A `model_info` that is not a table lists
-- like an empty one.
local function model_row(entry)
  if type(entry) ~= "table" or type(entry.model_name) ~= "string" or entry.model_info == nil then
    return nil
  end
  local info = type(entry.model_info) == "table" and entry.model_info or {}
  if type(info.mode) == "string" and info.mode ~= CHAT_MODE then
    return nil
  end

  local window = parse.as_u64(info.max_tokens) and info.max_tokens or info.max_input_tokens

  local pricing
  if parse.as_f64(info.input_cost_per_token) or parse.as_f64(info.output_cost_per_token) then
    pricing = {
      input = price(info, "input_cost_per_token"),
      output = price(info, "output_cost_per_token"),
      cache_write = price(info, "cache_creation_input_token_cost"),
      cache_read = price(info, "cache_read_input_token_cost"),
    }
  end

  return {
    id = entry.model_name,
    context_window = parse.as_u32(window),
    max_output_tokens = parse.as_u32(info.max_output_tokens),
    pricing = pricing,
    supports_thinking = parse.as_bool(info.supports_reasoning),
    supports_vision = info.supports_vision == true,
    extra = knobs(info),
  }
end

maki.provider.register({
  slug = SLUG,
  codec = "openai",
  openai = { thinking = { dialect = "tensorx" } },

  list_models = function()
    local auth = assert(maki.provider.auth.resolved(SLUG))
    local body, err = parse.get_json(auth, auth.base_url .. MODEL_INFO_PATH)
    if err then
      return parse.fail(err)
    end
    return parse.models(body, model_row)
  end,

  -- Each knob goes on the wire only for a model that advertised it. A DeepSeek
  -- model that advertises neither takes the toggle through its chat template.
  build_body = function(body, model, opts)
    local advertised = opts.model_info or {}
    local enabled = opts.thinking ~= THINKING_OFF

    if advertised.has_thinking then
      body.thinking = enabled
    end
    if advertised.has_reasoning_effort then
      return body
    end
    body.reasoning_effort = nil
    if
      not advertised.has_thinking
      and enabled
      and starts_with(model, DEEPSEEK_VENDOR_PREFIX)
      and uses_v4_thinking_protocol(model:sub(#DEEPSEEK_VENDOR_PREFIX + 1))
    then
      body.chat_template_kwargs = { thinking = true }
    end
    return body
  end,
})
