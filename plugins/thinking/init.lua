-- /thinking: a picker over the ladder the host reports, and the direct form
-- `/thinking <level>` that sets a value without opening anything.

local Window = require("thinking_window")

maki.api.register_command({
  name = "/thinking",
  description = "Extended thinking: pick an effort level, or set one directly",
  nargs = "?",
  handler = function(opts)
    if opts.args == "" then
      Window.open()
    else
      Window.set(opts.args)
    end
  end,
})

maki.keymap.set("n", "<M-t>", Window.open, { desc = "Thinking effort" })
