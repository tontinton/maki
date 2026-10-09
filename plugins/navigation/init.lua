local Navigation = require("navigation")

local HIGHLIGHT_DURATION_MS = 180

local opts = maki.api.register_options({
  wrap = { default = false, desc = "Wrap message navigation at transcript boundaries" },
})

for _, keys in ipairs({
  { "<M-j>", true, false, "Next message" },
  { "<M-k>", false, false, "Previous message" },
  { "<M-n>", true, true, "Next user prompt" },
  { "<M-p>", false, true, "Previous user prompt" },
}) do
  local binding = keys
  maki.keymap.set("n", binding[1], function()
    local view, err = maki.ui.transcript_positions()
    if not view then
      maki.notify(err, "error")
      return
    end
    local target, boundary = Navigation.target(view, binding[2], binding[3], opts.wrap)
    if target then
      if not boundary then
        local ok, scroll_err = maki.fn.winrestview({ topline = target.topline })
        if not ok then
          maki.notify(scroll_err, "error")
          return
        end
      end
      local ok, highlight_err = maki.ui.highlight_transcript(target.line, HIGHLIGHT_DURATION_MS, boundary)
      if not ok then
        maki.notify(highlight_err, "error")
      end
    end
  end, { desc = binding[4] })
end
