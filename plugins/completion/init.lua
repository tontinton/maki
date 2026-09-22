-- `@` file completion for the chat input, in Lua.
--
-- Nothing here is special-cased in the host. The popup is an ordinary
-- unfocused float on the input caret anchor, the navigation keys are the
-- `keys` any window may declare, the ranking is the one the built-in file
-- picker uses, and accepting is one `maki.ui.input_edit` over the range the
-- `@` opened.
--
-- It triggers on `InputChanged`, not on a key. A shifted `@` arrives with
-- SHIFT set on some terminals and not others, and the override table compares
-- modifiers exactly. Watching the text is also what lets the list narrow while
-- you keep typing, and the `cursor_only` events that come with it are what
-- tell the popup the caret has left the mention it was opened for.

local Events = require("events")
local Menu = require("menu")

maki.api.create_autocmd("InputChanged", {
  callback = function(ev)
    Events.input_changed(ev.data)
  end,
})

maki.api.create_autocmd("SessionReset", {
  callback = function(ev)
    Events.session_reset(ev.data)
  end,
})

-- Cycling to a subagent tab stays within the session, so nothing else here
-- fires. The popup's keys are routed to it before the chat's own, so one left
-- open over a subagent takes `<Up>` away from the input history and lets
-- `<CR>` write a path into a tab the user is not looking at.
maki.api.create_autocmd("TaskFocusChanged", {
  callback = Menu.close,
})

maki.api.create_autocmd("SessionStatusChanged", {
  callback = function(ev)
    Events.session_status_changed(ev.data)
  end,
})
