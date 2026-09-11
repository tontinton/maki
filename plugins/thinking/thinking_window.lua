-- The /thinking picker window, plus the direct `/thinking <level>` form. Both
-- end up in `set`, the one place that talks to the host and says what it
-- answered, so the picker itself never has to undo anything: nothing is sent
-- until Enter.

local Picker = require("thinking_picker")

-- Mirrors THINKING_UNSUPPORTED_MSG in maki-ui, which is also what
-- `maki.model.set` answers on a model without thinking support.
local UNSUPPORTED = "Thinking requires a model that supports it"
local FLASH_PREFIX = "Thinking: "
local TITLE_PREFIX = " Thinking · "
local FOOTER = { { "↑↓", "move" }, { "⏎", "apply" }, { "esc", "cancel" } }
-- The content is a fixed-column table, so the window keeps an absolute width
-- instead of stretching across an ultrawide terminal.
local WIDTH = 52
local MARGIN = 4
-- `height` is the outer box, so the border owes it a row at each end: without
-- them the last rows of the ladder fall outside and the list starts scrolling.
local CHROME = 2

local M = {}

local open_win = nil

local function title_for(id, width)
  local room = width - maki.ui.display_width(TITLE_PREFIX) - 1
  return TITLE_PREFIX .. maki.ui.truncate_text(id, room).head .. " "
end

function M.set(value)
  local model, err = maki.model.set({ thinking = value })
  maki.ui.flash(err or (FLASH_PREFIX .. model.thinking))
end

function M.open()
  if open_win then
    return
  end
  local model, err = maki.model.get()
  if err then
    maki.ui.flash(err)
    return
  end
  -- The ladder is empty exactly when the model cannot think, so this one check
  -- covers both.
  if #model.thinking_options == 0 then
    maki.ui.flash(UNSUPPORTED)
    return
  end

  local state = Picker.new(model)
  local width = math.min(WIDTH, maki.ui.terminal_size().cols - MARGIN)
  local buf = maki.ui.buf({ scratch = true })
  local win = maki.ui.open_win(buf, {
    title = title_for(model.id, width),
    width = width,
    height = #state.rows + CHROME,
    border = "rounded",
    focus = true,
    footer = FOOTER,
  })
  open_win = win

  local function draw()
    buf:set_lines(Picker.render(state, width))
    win:set_cursor(state.cursor)
  end

  buf:on("click", function(ev)
    Picker.click(state, ev.row)
    draw()
  end)

  -- Returns the value to send, or nil when the picker was dismissed.
  local function loop()
    draw()
    while true do
      local ev = win:recv()
      if not ev or ev.type == "close" then
        return nil
      elseif ev.type == "key" then
        local action = Picker.handle_key(state, ev.key)
        if action == "commit" then
          return Picker.value(state)
        elseif action == "cancel" then
          return nil
        end
      elseif ev.type == "resize" then
        width = ev.width
      end
      draw()
    end
  end

  -- However the loop ends, an error included, the window closes and the guard
  -- clears before anything is sent.
  local ok, value = pcall(loop)
  win:close()
  open_win = nil
  if not ok then
    error(value)
  end
  if value then
    M.set(value)
  end
end

return M
