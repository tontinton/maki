-- The /todo popup: the focused session's full todo list, live.
--
-- init.lua owns the state, so this module only ever sees one closure:
-- `fetch` re-reads the current list (items, done, total). Every `todo_write`
-- call pushes `refresh` into the open window, so the popup tracks the panel
-- instead of freezing on the list that was on screen when it opened.
--
-- The cursor exists only to scroll: the host slides the scroll offset to
-- follow it, and the wheel scrolls over the window. No row is selected,
-- nothing is committed, so a redraw is owed only by a changed list.

local L = require("lines")

local WIDTH = "70%"
local NO_TODOS = "No todos"
local FOOTER = { { "Esc", "close" } }

local M = {}

local win, buf = nil, nil
local fetch = nil
local last_items, lines, cursor, viewport = nil, {}, 1, 1

-- Redraws the window from a list `open` did not prime with. `refresh` runs it
-- when the list changes underneath, and an emptied list closes the window,
-- which is how a finished turn takes the popup down.
local function paint(items, done, total)
  if #items == 0 then
    win:close()
    return
  end
  lines = L.build_lines(items)
  if cursor > #lines then
    cursor = #lines
  end
  last_items = items
  buf:set_lines(lines)
  win:set_config({
    title = L.window_title(done, total),
    height = L.window_height(#lines),
  })
  win:set_cursor(cursor)
end

local function move_cursor(target)
  cursor = math.min(math.max(target, 1), math.max(#lines, 1))
  win:set_cursor(cursor)
end

local function handle_key(key)
  if key == "<Esc>" or key == "<C-c>" then
    win:close()
  elseif key == "<Up>" or key == "k" then
    move_cursor(cursor - 1)
  elseif key == "<Down>" or key == "j" then
    move_cursor(cursor + 1)
  elseif key == "<PageUp>" then
    move_cursor(cursor - viewport)
  elseif key == "<PageDown>" then
    move_cursor(cursor + viewport)
  elseif key == "<Home>" then
    move_cursor(1)
  elseif key == "<End>" then
    move_cursor(#lines)
  end
end

local function loop()
  while true do
    local ev = win:recv()
    if not ev or ev.type == "close" then
      return
    elseif ev.type == "key" then
      handle_key(ev.key)
    elseif ev.type == "resize" then
      viewport = ev.height
    end
  end
end

function M.open(fetch_fn)
  if win then
    return
  end
  fetch = fetch_fn
  local items, done, total = fetch()
  if #items == 0 then
    maki.ui.flash(NO_TODOS)
    return
  end
  buf = maki.ui.buf({ scratch = true })
  lines = L.build_lines(items)
  win = maki.ui.open_win(buf, {
    title = L.window_title(done, total),
    width = WIDTH,
    height = L.window_height(#lines),
    border = "rounded",
    focus = true,
    footer = FOOTER,
    cursor_line = true,
  })
  viewport = win.height
  last_items = items
  buf:set_lines(lines)
  win:set_cursor(cursor)

  -- However the loop ends, an error included, the window closes and the
  -- guard clears before anything is sent.
  local ok, err = pcall(loop)
  win:close()
  win, buf, last_items, lines, cursor, viewport = nil, nil, nil, {}, 1, 1
  if not ok then
    error(err)
  end
end

-- Called from the `todo_write` handler and `TurnEnd`. A no-op when the
-- focused list is the one already on screen.
function M.refresh()
  if not win or not win:is_open() then
    return
  end
  local items, done, total = fetch()
  if items == last_items then
    return
  end
  paint(items, done, total)
end

return M
