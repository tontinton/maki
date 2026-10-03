local parse = require("maki.provider_parse")

local MODELS_PATH = "/models"
local CHAT_ENDPOINT = "chat_completions"
local IMAGE_MODALITY = "image"
local OFF_EFFORT = "none"

local function contains(values, wanted)
  for _, value in pairs(type(values) == "table" and values or {}) do
    if value == wanted then
      return true
    end
  end
  return false
end

local function limit(value)
  local tokens = parse.as_u32(value)
  return tokens and tokens > 0 and tokens or nil
end

local function price(value)
  local amount = parse.as_f64(value)
  return amount and amount >= 0 and amount < math.huge and amount or nil
end

-- RunInfra publishes USD per million tokens, already in Maki's units.
local function pricing_of(model)
  local prices = model.pricing
  if type(prices) ~= "table" then
    return nil
  end
  local input, output = price(prices.input), price(prices.output)
  if input == nil or output == nil then
    return nil
  end
  return {
    input = input,
    output = output,
    cache_write = 0,
    cache_read = price(model.cached_input_price_usd_per_mtok) or price(model.cached_input_price) or 0,
  }
end

local function effort_of(values)
  if type(values) ~= "table" then
    return nil, nil
  end
  local supported, thinks = {}, nil
  for _, name in pairs(values) do
    if type(name) == "string" then
      thinks = true
      if name ~= OFF_EFFORT then
        table.insert(supported, name)
      end
    end
  end
  return { supported = supported, send_off = contains(values, OFF_EFFORT) }, thinks
end

local function parse_model(model)
  if type(model) ~= "table" or type(model.id) ~= "string" or model.id == "" then
    return nil
  end
  if not contains(model.supported_endpoints, CHAT_ENDPOINT) then
    return nil
  end
  local effort, thinks = effort_of(model.reasoning_efforts)
  local vision
  if type(model.input_modalities) == "table" then
    vision = contains(model.input_modalities, IMAGE_MODALITY)
  end
  return {
    id = model.id,
    context_window = limit(model.context_window) or limit(model.context_length),
    max_output_tokens = limit(model.max_output_tokens) or limit(model.max_completion_tokens),
    pricing = pricing_of(model),
    supports_thinking = thinks,
    supports_vision = vision,
    effort = effort,
    extra = effort and { reasoning_efforts = model.reasoning_efforts } or nil,
  }
end

maki.provider.register({
  slug = "runinfra",
  display_name = "RunInfra",
  codec = "openai",
  base_url = "https://api.runinfra.ai/v1",
  api_key_env = "RUNINFRA_API_KEY",
  login_url = "https://runinfra.ai/settings/api-keys",
  default_model = "deepseek-v4-1-flash",
  family = "generic",
  accepts_arbitrary_models = true,
  max_output_tokens = false,
  aperture = { path_prefix = "/v1" },
  docs = {
    features = "Open models, prompt caching, model-specific reasoning and image input",
    discovery_note = "Model IDs, limits, capabilities and USD token prices come from the authenticated "
      .. "[RunInfra model listing](https://runinfra.ai/docs/api-reference/models). "
      .. "Paused models stay listed with no published price.",
  },
  openai = { thinking = { dialect = "standard", requires_support = true } },

  -- An off-only or undeclared list must not inherit the codec's effort levels.
  build_body = function(_, body, _, opts)
    local efforts = opts.model_info and opts.model_info.reasoning_efforts
    if type(efforts) == "table" and not contains(efforts, body.reasoning_effort) then
      body.reasoning_effort = nil
    end
    return body
  end,

  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    return parse.models(body, parse_model)
  end,
})
