-- What the host's events mean for the popup: which of them may open one,
-- which have to take it away, and which are this plugin's own writes coming
-- back around.
--
-- Separate from `init.lua` so the decisions can be tested without a running
-- event loop. `init.lua` is wiring and nothing else.

local Menu = require("menu")
local Trigger = require("trigger")

local Events = {}

-- The name the host stamps on this plugin's own `input_edit`, which is how
-- the `InputChanged` handler tells an accept from the user typing.
Events.PLUGIN = "completion"

-- The chat input moved, and the event says how: `text`, `cursor`, `version`
-- and `session_id` are every field `maki.ui.input` would answer with, so the
-- refresh below reads nothing back and the rows answer the keystroke a round
-- trip sooner.
function Events.input_changed(data)
  -- An accept is an `input_edit`, which fires an `InputChanged` of its own
  -- naming this plugin. Acting on it would reopen the popup on the path it
  -- just inserted. The host stamps the name only on a frame this plugin wrote
  -- alone, so a frame that also carried a keystroke still gets here.
  if data.source == Events.PLUGIN then
    return
  end
  -- A caret the user moved can take the popup away but never put one up.
  -- Arrowing back over an `@` that was dismissed with Esc would otherwise
  -- reopen it, and every arrow key near an `@` would cost a round-trip.
  if data.cursor_only and not Menu.session_id() then
    return
  end
  -- Focusing another tab fires this for the input that tab holds. That is
  -- another line of text entirely, and the popup was placed against the caret
  -- in the tab being left.
  local open = Menu.session_id()
  if open and open ~= data.session_id then
    Menu.close()
  end
  -- Which punctuation opens a mention depends on the sources loaded, and
  -- asking them is too slow for this handler. So any punctuation earns a
  -- refresh, and the refresh sorts it out.
  if not Menu.session_id() and not Trigger.may_open(data.text, data.cursor) then
    return
  end
  Menu.refresh_later(data)
end

-- `/new` in a tab the user is not looking at must leave the popup they are
-- typing into alone.
function Events.session_reset(data)
  if Menu.session_id() == data.session_id then
    Menu.close()
  end
end

-- A permission prompt, the plan form or a pack review takes the chat input
-- off screen, and the host then refuses to edit it. Leaving the popup up would
-- leave it holding keys for an accept that cannot land.
function Events.session_status_changed(data)
  if data.focused and data.status == "needs_input" then
    Menu.close()
  end
end

return Events
