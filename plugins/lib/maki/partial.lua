-- When a tool is cut short, it still hands back what it printed. The marker
-- tells the model that output is real but unfinished. One home for the
-- wording and the painting, so every tool says it the same way.
local M = {}

local CANCELLED_FMT = "[cancelled by user after %s, %s]"
local TIMEOUT_FMT = "[timed out after %ds, %s]"
-- Never claim output the model cannot see above the marker: it would go
-- looking for it, or invent it.
local SOME_OUTPUT = "output above is partial"
local NO_OUTPUT = "no output before the cut"

--- Close {view} on the marker and build the tool reply. {out} is everything
--- the tool streamed, already truncated; empty means the view still shows a
--- placeholder to drop. {reason} is a cancel-hook reason ("cancelled" |
--- "timeout"). {started_at} is the `os.time()` the tool began at.
function M.cut(view, out, reason, timeout_secs, started_at)
  local tail = out ~= "" and SOME_OUTPUT or NO_OUTPUT
  local marker
  if reason == "timeout" then
    marker = TIMEOUT_FMT:format(timeout_secs, tail)
  else
    marker = CANCELLED_FMT:format(maki.ui.humantime(math.max(os.time() - started_at, 0)), tail)
  end

  if out == "" then
    view:clear()
  end
  view:append({ { marker, "dim" } })
  view:finish()

  return {
    llm_output = (out ~= "" and out .. "\n" or "") .. marker,
    is_error = true,
    body = view.buf,
  }
end

return M
