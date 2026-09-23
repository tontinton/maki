-- Where a mention starts in the chat input, which character opened it, and
-- what has been typed into it so far.
--
-- Everything here counts bytes, because that is the unit `maki.ui.input` and
-- `maki.ui.input_edit` speak, and the one the Lua string library speaks too.
-- Nothing has to be converted: the offsets a match reports are the offsets an
-- edit writes over.

local Trigger = {}

Trigger.FILES = "@"

-- Where the word at the cursor starts, and the word up to the cursor. A
-- mention has to open the word, so an email address or a `user@host`
-- argument never counts as one, and a space ends it.
--
-- Looking for ASCII whitespace is enough for text of any encoding: every
-- whitespace byte is ASCII, and no byte of a multi-byte character is.
local function word(text, cursor)
  local before = text:sub(1, cursor)
  local start = before:match("^.*%s()") or 1
  return start, before:sub(start)
end

-- Returns the byte offset of the trigger, the query typed after it, and the
-- trigger itself, or nil when the cursor is not inside a mention opened by one
-- of the characters in {triggers}.
function Trigger.find(text, cursor, triggers)
  local start, typed = word(text, cursor)
  local lead = typed:sub(1, 1)
  if lead == "" or not triggers:find(lead, 1, true) then
    return nil
  end
  return start - 1, typed:sub(2), lead
end

-- The sources are only known once their slot has run, and running it is the
-- refresh's job. So this cheap check only asks whether the word opens with
-- punctuation some source could be listening for.
function Trigger.may_open(text, cursor)
  local _, typed = word(text, cursor)
  return typed:match("^%p") ~= nil
end

return Trigger
