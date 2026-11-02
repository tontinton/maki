-- /thinking: a picker over the ladder the host reports, and the direct form
-- `/thinking <level>` that sets a value without opening anything.

local Window = require("thinking_window")

maki.api.register_command({
  name = "/thinking",
  description = "Extended thinking: pick a level, set one, or step with + / -",
  nargs = "?",
  handler = function(opts)
    if opts.args == "" then
      Window.open()
    else
      Window.set(opts.args)
    end
  end,
})

maki.keymap.set({ "n", "i" }, "<M-t>", Window.open, { desc = "Thinking effort" })
maki.keymap.set({ "n", "i" }, "<M-,>", function()
  Window.set("-")
end, { desc = "Less thinking" })
maki.keymap.set({ "n", "i" }, "<M-.>", function()
  Window.set("+")
end, { desc = "More thinking" })
