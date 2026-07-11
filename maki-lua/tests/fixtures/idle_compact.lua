local IDLE_CHECK_INTERVAL_MS = 60_000
local MIN_HISTORY_FOR_COMPACT = 2
local NS_PER_MINUTE = 60 * 1_000_000_000
local DEFAULT_IDLE_MINUTES = 10

local function idle_minutes()
  local raw = maki.uv.os_getenv("MAKI_IDLE_COMPACT_MINUTES")
  local n = tonumber(raw)
  if n and n > 0 then
    return n
  end
  return DEFAULT_IDLE_MINUTES
end

local last_input = maki.uv.hrtime()
local compacted_since_input = false

local function resetidle()
  last_input = maki.uv.hrtime()
  compacted_since_input = false
end

local function check_idle()
  if compacted_since_input then
    return
  end
  local elapsed_min = (maki.uv.hrtime() - last_input) / NS_PER_MINUTE
  if elapsed_min < idle_minutes() then
    return
  end
  local len = maki.session.history_len()
  if len == nil or len <= MIN_HISTORY_FOR_COMPACT then
    return
  end
  maki.session.compact()
  compacted_since_input = true
end

maki.api.create_autocmd({ "UserInput", "TurnEnd", "TurnError" }, {
  callback = resetidle,
})

local timer = maki.uv.new_timer()
timer:start(IDLE_CHECK_INTERVAL_MS, IDLE_CHECK_INTERVAL_MS, check_idle)
