-- Regolo, as a declaration plus the catalogue and the usage report the openai
-- codec cannot fetch. The slug is one maki ships, so claiming it inherits the
-- display name, the key env var and the curated model table. Restating any of
-- those here is a registration error rather than an override.

local parse = require("maki.provider_parse")

local SLUG = "regolo"
local MODELS_PATH = "/models"
-- The management endpoints live at the host root, outside the versioned API.
-- They are reached through the configured origin with the version stripped,
-- never through an absolute url, so a gateway in front of Regolo keeps serving
-- them instead of being bypassed with the key meant for it.
local VERSION_SEGMENT = "/v1"
local MODEL_GROUP_INFO_PATH = "/model_group/info"
local KEY_INFO_PATH = "/key/info"
-- Key-scoped despite its name, and answers per UTC day.
local ACTIVITY_PATH = "/global/activity"
-- One row per model per hour, so rows are summed per model.
local SPEND_LOGS_PATH = "/spend/logs/v2"
local CHAT_MODE = "chat"
-- Prices and spend arrive in dollars per token, rows want them per million.
local PER_MILLION = 1000000
local U32_MAX = 4294967295
local SECONDS_PER_DAY = 86400
local MILLIS_PER_SECOND = 1000
local UTC_DAY_FORMAT = "!%Y-%m-%d"
local GROUP_NUMBERS = {
  "input_cost_per_token",
  "output_cost_per_token",
  "max_input_tokens",
  "max_output_tokens",
  "max_tokens",
}
local MALFORMED_KEY_INFO = "regolo: /key/info answered in a shape maki cannot read"

local function root_url(base)
  local trimmed = (string.gsub(base, "/+$", ""))
  if string.sub(trimmed, -#VERSION_SEGMENT) == VERSION_SEGMENT then
    return string.sub(trimmed, 1, -#VERSION_SEGMENT - 1)
  end
  return trimmed
end

-- A side answer the caller can do without: a failed request, a status other
-- than 200 or a shape `read` refuses all read as no answer.
local function fetch_optional(auth, url, read)
  local body = parse.get_json(auth, url)
  if type(body) ~= "table" then
    return nil
  end
  return read(body)
end

local function day_url(root, path, day)
  return root .. path .. "?start_date=" .. day .. "&end_date=" .. day
end

-- A group with any field in the wrong shape fails the whole answer, the way
-- one unreadable group fails maki's own parse of it.
local function valid_group(group)
  if
    type(group) ~= "table"
    or type(group.model_group) ~= "string"
    or type(group.mode) ~= "string"
    or type(group.supports_reasoning) ~= "boolean"
    or type(group.supports_vision) ~= "boolean"
  then
    return false
  end
  for _, key in ipairs(GROUP_NUMBERS) do
    if group[key] ~= nil and parse.as_f64(group, key) == nil then
      return false
    end
  end
  return true
end

local function parse_groups(body)
  if type(body.data) ~= "table" then
    return nil
  end
  for _, group in parse.items(body.data) do
    if not valid_group(group) then
      return nil
    end
  end
  return body.data
end

-- Rust `u32::try_from(tokens as u64).ok()`: an out-of-range window is no
-- window, not a clamped one.
local function window(tokens)
  if tokens == nil then
    return nil
  end
  local whole = parse.cast_u64(tokens)
  if whole > U32_MAX then
    return nil
  end
  return whole
end

local function listed_id(model)
  if type(model) == "table" and type(model.id) == "string" then
    return { id = model.id }
  end
  return nil
end

-- Groups in a non-chat mode (embedding, rerank, ocr, image, audio) and ids
-- without a chat group are not agent models. A later chat group for the same
-- id replaces an earlier one. Order follows `listed`.
local function join(listed, groups)
  local by_group = {}
  for _, group in ipairs(groups) do
    if group.mode == CHAT_MODE then
      by_group[group.model_group] = group
    end
  end
  local models = {}
  for _, row in ipairs(listed) do
    local group = by_group[row.id]
    if group then
      -- Half a price is no price: without both sides it would read as free.
      local pricing
      if group.input_cost_per_token ~= nil and group.output_cost_per_token ~= nil then
        pricing = {
          input = group.input_cost_per_token * PER_MILLION,
          output = group.output_cost_per_token * PER_MILLION,
          cache_write = 0,
          cache_read = 0,
        }
      end
      local context = group.max_input_tokens
      if context == nil then
        context = group.max_tokens
      end
      table.insert(models, {
        id = row.id,
        context_window = window(context),
        max_output_tokens = window(group.max_output_tokens),
        pricing = pricing,
        supports_thinking = group.supports_reasoning,
        supports_vision = group.supports_vision,
      })
    end
  end
  return models
end

local function spend_limit(info)
  if type(info) ~= "table" then
    return nil
  end
  local spend = parse.as_f64(info, "spend")
  local budget = parse.as_f64(info, "max_budget")
  local reset = info.budget_reset_at
  if spend == nil or (info.max_budget ~= nil and budget == nil) or (reset ~= nil and type(reset) ~= "string") then
    return nil
  end
  local detail = "$" .. parse.fixed(spend, 2) .. " spent"
  local percentage
  if budget ~= nil then
    detail = detail .. " of $" .. parse.fixed(budget, 2) .. " budget"
    if budget > 0 then
      percentage = math.min(parse.cast_u32(spend / budget * 100), 100)
    end
  end
  -- Handed back as the timestamp it arrived as: maki reads RFC 3339 itself, and
  -- a string that is no timestamp is no reset.
  return { label = "Spend", percentage = percentage, reset_at = reset, detail = detail }
end

local function next_utc_midnight()
  return (math.floor(os.time() / SECONDS_PER_DAY) + 1) * SECONDS_PER_DAY * MILLIS_PER_SECOND
end

-- Regolo tracks a per-account daily token cap but no endpoint reports it, so
-- there is no honest percentage to show.
local function activity_limit(body)
  local requests = parse.as_u64(body, "sum_api_requests")
  local tokens = parse.as_u64(body, "sum_total_tokens")
  if requests == nil or tokens == nil then
    return nil
  end
  return {
    label = "Today",
    reset_at = next_utc_midnight(),
    detail = string.format("%s requests · %s tokens", parse.fixed(requests, 0), parse.fixed(tokens, 0)),
  }
end

-- Hourly rows summed per model in name order, then ranked by spend, which
-- keeps the name order between equal spends.
local function spend_rows(body)
  if type(body.data) ~= "table" then
    return nil
  end
  local by_model, names = {}, {}
  for _, entry in parse.items(body.data) do
    if type(entry) ~= "table" or type(entry.model_group) ~= "string" then
      return nil
    end
    local input = parse.as_u64(entry, "prompt_tokens")
    local output = parse.as_u64(entry, "completion_tokens")
    local total = parse.as_u64(entry, "total_tokens")
    local spend = parse.as_f64(entry, "spend")
    if input == nil or output == nil or total == nil or spend == nil then
      return nil
    end
    local acc = by_model[entry.model_group]
    if acc then
      acc.input = acc.input + input
      acc.output = acc.output + output
      acc.total = acc.total + total
      acc.spend = acc.spend + spend
    else
      by_model[entry.model_group] = { input = input, output = output, total = total, spend = spend }
      table.insert(names, entry.model_group)
    end
  end
  table.sort(names)

  local rows = {}
  for _, name in ipairs(names) do
    local acc = by_model[name]
    table.insert(rows, {
      model = name,
      input_tokens = acc.input,
      output_tokens = acc.output,
      total_tokens = acc.total,
      spend_microdollars = parse.cast_u64(parse.round(acc.spend * PER_MILLION)),
    })
  end
  parse.stable_sort_by(rows, function(a, b)
    return a.spend_microdollars > b.spend_microdollars
  end)
  return rows
end

local function settled(result)
  if result.ok then
    return result.value
  end
  return nil
end

maki.provider.register({
  slug = SLUG,
  codec = "openai",
  openai = {
    max_tokens_field = "max_completion_tokens",
    thinking = { dialect = "standard" },
  },

  -- The group metadata is optional: the endpoint has 500ed in the wild, and
  -- the live ids beat the static few.
  list_models = function()
    local auth = assert(maki.provider.auth.resolved(SLUG))
    local body, err = parse.get_json(auth, auth.base_url .. MODELS_PATH)
    if err then
      return parse.fail(err)
    end
    local listed = parse.models(body, listed_id)

    local groups = fetch_optional(auth, root_url(auth.base_url) .. MODEL_GROUP_INFO_PATH, parse_groups)
    if groups then
      return join(listed, groups)
    end
    return listed
  end,

  -- The key's spend is the report and its failure is the hook's. Today's
  -- activity and per-model spend go out together once the key answered, and
  -- each is dropped on its own when it fails.
  fetch_usage = function()
    local auth = assert(maki.provider.auth.resolved(SLUG))
    local root = root_url(auth.base_url)
    local body, err = parse.get_json(auth, root .. KEY_INFO_PATH)
    if err then
      return parse.fail(err)
    end
    local spend = assert(spend_limit(type(body) == "table" and body.info), MALFORMED_KEY_INFO)

    local today = os.date(UTC_DAY_FORMAT)
    local side = maki.async.gather({
      function()
        return fetch_optional(auth, day_url(root, ACTIVITY_PATH, today), activity_limit)
      end,
      function()
        return fetch_optional(auth, day_url(root, SPEND_LOGS_PATH, today), spend_rows)
      end,
    })
    local limits = { spend }
    local activity = settled(side[1])
    if activity then
      table.insert(limits, activity)
    end
    return { limits = limits, by_model_today = settled(side[2]) }
  end,
})
