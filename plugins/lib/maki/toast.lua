-- Corner toast notifications built on floating windows. `maki.ui.flash` gives
-- you one line in the status area. A toast stays up long enough to read, can
-- carry a title, and stacks under the toasts already on screen.

local Toast = {}

local MAX_LINES = 5
local MAX_WIDTH = 48
local MIN_WIDTH = 20
local DEFAULT_TIMEOUT_SECS = 4
-- Above the default 50 every other float gets, so a picker opened after the
-- toast doesn't bury it and callers don't have to time their notices.
local ZINDEX = 200

-- Show {text} as a toast, up to 5 lines of it. {opts}: title (string),
-- timeout_secs (integer, default 4). Returns right away and the toast
-- dismisses itself when the time is up.
function Toast.show(text, opts)
  opts = opts or {}
  local lines = {}
  local width = MIN_WIDTH
  for line in tostring(text):gmatch("[^\n]+") do
    if #lines == MAX_LINES then
      break
    end
    lines[#lines + 1] = { { line, "item" } }
    width = math.max(width, maki.ui.display_width(line) + 2)
  end
  if #lines == 0 then
    return
  end
  width = math.min(width, MAX_WIDTH)

  -- A live buffer would steal the output pane of whatever tool raised us.
  local buf = maki.ui.buf({ scratch = true })
  buf:set_lines(lines)
  local win = maki.ui.open_win(buf, {
    title = opts.title,
    anchor = "NE",
    stack = true,
    col = 0,
    width = width,
    height = #lines + 2,
    zindex = ZINDEX,
    focus = false,
  })

  -- defer_fn and not async.sleep: the toast outlives the tool call that
  -- raised it, so the timer must not hang off that call's cancel token.
  local timeout_secs = math.max(0, tonumber(opts.timeout_secs) or DEFAULT_TIMEOUT_SECS)
  maki.defer_fn(function()
    win:close()
  end, math.floor(timeout_secs * 1000))
end

return Toast
