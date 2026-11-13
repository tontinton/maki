-- /thinking: a picker over the ladder the host reports, the direct form
-- `/thinking <level>` that sets a value without opening anything, and keys that
-- step one level down or up.

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

maki.keymap.set({ "n", "i" }, "<M-t>", Window.open, { desc = "Thinking effort" })
maki.keymap.set({ "n", "i" }, "<M-,>", function()
  Window.step(-1)
end, { desc = "Less thinking" })
maki.keymap.set({ "n", "i" }, "<M-.>", function()
  Window.step(1)
end, { desc = "More thinking" })
