-- The parts of automode that are pure string work, kept out of init.lua so
-- they can be tested without a reviewer chain or a state file.

local M = {}

-- A go-ahead is a short answer to a question the agent just asked. Past this
-- length the message is new work, and new work is the reviewer model's call.
local OVERRIDE_MAX_CHARS = 240

local GO_AHEAD_PHRASES = {
  "try again",
  "retry",
  "again",
  "go ahead",
  "do it",
  "just do it",
  "proceed",
  "continue",
  "yes",
  "yep",
  "ok",
  "okay",
  "allowed",
  "i allow",
  "approved",
  "you can",
  "you're allowed",
  "stop blocking",
  "let it through",
  "unblock",
  "override",
}

-- Checked before the go-ahead list, because "no, try again later" contains
-- "try again" and means the opposite of it.
local REFUSAL_PATTERNS = { "^no%f[%A]", "don't", "do not", "not ok", "never", "stop that", "wait" }

--- Whether a user message reads as authorisation to retry what was denied.
function M.is_go_ahead(text)
  local msg = (text or ""):match("^%s*(.-)%s*$"):lower()
  if msg == "" or #msg > OVERRIDE_MAX_CHARS then
    return false
  end
  for _, pattern in ipairs(REFUSAL_PATTERNS) do
    if msg:find(pattern) then
      return false
    end
  end
  for _, phrase in ipairs(GO_AHEAD_PHRASES) do
    if msg:find(phrase, 1, true) then
      return true
    end
  end
  return false
end

--- The program a permission scope is about, so an override granted for one
--- command cannot leak onto every other call in the same turn.
function M.executable_of(scopes)
  local first = scopes and scopes[1]
  if type(first) ~= "string" then
    return nil
  end
  return first:match("^%s*([^%s]+)")
end

--- Split a `plugins.automode.chain` string into model specs, cheapest first.
function M.parse_chain(spec)
  local specs = {}
  for part in tostring(spec or ""):gmatch("[^,]+") do
    local trimmed = part:match("^%s*(.-)%s*$")
    if trimmed ~= "" then
      specs[#specs + 1] = trimmed
    end
  end
  return specs
end

return M
