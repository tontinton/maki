-- The /tasks picker: the subagents of the focused session, running ones first.
-- The tool in init.lua spawns them, this file only shows them.
--
-- The host keeps the transcripts, so there is no task state here. Every
-- refresh rebuilds the rows from maki.task.list(), and previewing is just
-- maki.task.focus() with a restore on cancel.

local TextInput = require("maki.text_input")
local ListPicker = require("maki.list_picker")
local Rows = require("picker_rows")

local TITLE = " Tasks "
local FILTER_PREFIX = "❯ "
local TICK_MS = 100
-- A placeholder frame. The host swaps "spinner:*" spans for the live one, so
-- running rows spin without this plugin redrawing.
local RUNNING_ICON = "· "
local MAIN_ICON = "● "
-- The footer counts never animate, so they get a real bullet.
local RUNNING_COUNT_ICON = "● "
local DONE_ICON = "✓ "
local ERROR_ICON = "✗ "
local NO_MATCHES_HINT = "  No matches"
local CONFIRM_HINT = "  Ctrl+D again to delete"
local MAIN_DELETE_HINT = "Cannot delete the main chat"
local RUNNING_DELETE_HINT = "Task is still running"
local FOOTER_KEYS = { { "Enter", "open" }, { "Ctrl+D", "delete" }, { "Esc", "cancel" } }
local HINT_KEY = "Ctrl+X"
-- The main chat has no status, so it falls through to MAIN.
local ICONS = {
  working = { RUNNING_ICON, "accent", true },
  done = { DONE_ICON, "success" },
  error = { ERROR_ICON, "error" },
}
local MAIN = { MAIN_ICON, "accent" }

local board = nil

local function dispw(s)
  return utf8.len(s) or #s
end

-- The input area advertises the picker key while subagents are still working,
-- and drops the hint once the last one finishes. maki.task.list() suspends and
-- autocmd callbacks cannot, so the round-trip runs off to the side.
local function refresh_hint()
  maki.async.run(function()
    local n = 0
    for _, task in ipairs(maki.task.list() or {}) do
      if task.status == "working" then
        n = n + 1
      end
    end
    if n == 0 then
      maki.ui.set_status_hint(nil)
    else
      maki.ui.set_status_hint({
        { string.format(" %d %s ", n, n == 1 and "task" or "tasks"), "foreground" },
        { HINT_KEY, "keybind_key" },
        { " ", "" },
      })
    end
  end)
end

local function icon_of(task)
  local icon = ICONS[task.status] or MAIN
  return icon[1], icon[2], icon[3]
end

-- The counts describe the rows on screen, so a filter that hides half the list
-- has to retally them.
local function update_footer(counts)
  local footer = {}
  if counts.running > 0 then
    footer[#footer + 1] = { RUNNING_COUNT_ICON .. counts.running, "running" }
  end
  if counts.finished > 0 then
    footer[#footer + 1] = { DONE_ICON .. counts.finished, "finished" }
  end
  for _, key in ipairs(FOOTER_KEYS) do
    footer[#footer + 1] = key
  end
  board.win:set_config({ footer = footer })
end

-- The cursor follows its task while the task is still listed, and otherwise
-- falls to whatever row took over the old position.
local function rebuild()
  local previous = Rows.index_of(board.rows, board.sel_id) or 1
  local built = Rows.build(board.tasks, board.input:value())
  board.rows = built.rows
  local idx = Rows.index_of(board.rows, board.sel_id) or math.min(previous, math.max(#board.rows, 1))
  board.sel_id = board.rows[idx] and board.rows[idx].task.id or nil
  update_footer(built.sections)
end

local function render()
  local lines = {}
  local inner = board.width - 4
  board.reserved = ListPicker.render_header(board.win, lines, board.input, FILTER_PREFIX, inner)
  local cursor_line = board.reserved
  local words = ListPicker.split_words(board.input:value())
  for _, row in ipairs(board.rows) do
    if row.section then
      lines[#lines + 1] = { { "  " .. row.section, "keybind_section" } }
    end
    local task = row.task
    local selected = task.id == board.sel_id
    local base = selected and "selected" or "item"
    local icon, icon_style, spinning = icon_of(task)
    if selected then
      icon_style = "selected"
    end
    -- Prefixed after the selection style so a running row keeps spinning on
    -- the selection background.
    if spinning then
      icon_style = "spinner:" .. icon_style
    end
    local line = { { "  ", base }, { icon, icon_style } }
    local match_style = selected and "match_selected" or "match"
    for _, span in ipairs(ListPicker.highlight_spans(task.name, words, base, match_style)) do
      line[#line + 1] = span
    end
    local confirm = board.confirm == task.id
    if confirm then
      line[#line + 1] = { CONFIRM_HINT, selected and "match_selected" or "error" }
    end
    -- Rows with nothing on the right would otherwise end short of the border
    -- and read as padding on one side only, so the bar runs the full width.
    local trail = board.width - 2 - dispw(icon) - dispw(task.name)
    if confirm then
      trail = trail - dispw(CONFIRM_HINT)
    end
    if trail > 0 then
      line[#line + 1] = { string.rep(" ", trail), base }
    end
    lines[#lines + 1] = line
    if selected then
      cursor_line = #lines
    end
  end
  if #board.rows == 0 then
    lines[#lines + 1] = { { NO_MATCHES_HINT, "dim" } }
  end
  board.buf:set_lines(lines)
  board.win:set_cursor(cursor_line)
end

-- The one host round-trip. `list()` suspends this coroutine and the picker can
-- close while it waits, so bail out unless this board is still the current one.
local function refresh()
  local this_board = board
  local tasks, err = maki.task.list()
  if board ~= this_board then
    return
  end
  if err then
    maki.ui.flash(err)
    return
  end
  board.tasks = tasks
  rebuild()
  render()
end

-- The only exit. Unless the user committed, it puts back whatever was on screen
-- before the picker opened, so a cancelled preview never sticks.
local function finish(commit)
  local closing = board
  if not closing then
    return
  end
  board = nil
  if not commit and closing.origin_id then
    maki.task.focus(closing.origin_id)
  end
  closing.win:close()
end

-- Previewing is a real focus, so the transcript behind the float is the one the
-- host already draws. Render first for an instant cursor and let the host catch
-- up next frame.
local function move_sel(delta, wrap)
  local n = #board.rows
  if n == 0 then
    return
  end
  board.confirm = nil
  local cur = Rows.index_of(board.rows, board.sel_id) or 1
  local idx
  if wrap then
    idx = (cur - 1 + delta) % n + 1
  else
    idx = math.min(math.max(cur + delta, 1), n)
  end
  board.sel_id = board.rows[idx].task.id
  render()
  local _, err = maki.task.focus(board.sel_id)
  if err then
    maki.ui.flash(err)
  end
end

local function page_size()
  return math.max(board.height - board.reserved - 1, 1)
end

local function open_selected()
  if not board.sel_id then
    return
  end
  local _, err = maki.task.focus(board.sel_id)
  if err then
    maki.ui.flash(err)
    return
  end
  finish(true)
end

-- After a delete the focused task is gone, so we re-focus the row that slides
-- into its slot (the one right after it, or the one before if it was last).
-- Keeping `board.sel_id` across `refresh()` lets `rebuild()` re-seat the
-- cursor there instead of jumping back to the top; `origin_id` remains the
-- fallback for when the list ends up empty.
local function delete_selected()
  local idx = Rows.index_of(board.rows, board.sel_id)
  if not idx then
    return
  end
  local task = board.rows[idx].task
  if not Rows.deletable(task) then
    board.confirm = nil
    maki.ui.flash(task.status and RUNNING_DELETE_HINT or MAIN_DELETE_HINT)
    return
  end
  if board.confirm ~= task.id then
    board.confirm = task.id
    render()
    return
  end
  board.confirm = nil
  local _, err = maki.task.remove(task.id)
  if err then
    maki.ui.flash(err)
    return
  end
  if board.origin_id == task.id then
    board.origin_id = "main"
  end
  -- Keep sel_id so rebuild() re-seats the cursor on the row that takes over
  -- the deleted row's slot, then focus whatever ends up there.
  local this_board = board
  refresh()
  if board == this_board and board.sel_id then
    local _, focus_err = maki.task.focus(board.sel_id)
    if focus_err then
      maki.ui.flash(focus_err)
    end
  end
end

local function handle_key(key)
  if key == "ctrl+c" or key == "ctrl+x" then
    finish(false)
  elseif key == "esc" then
    if board.confirm then
      board.confirm = nil
      render()
    elseif board.input:is_empty() then
      finish(false)
    else
      board.input:clear()
      rebuild()
      render()
    end
  elseif key == "up" then
    move_sel(-1, true)
  elseif key == "down" then
    move_sel(1, true)
  elseif key == "pageup" then
    move_sel(-page_size())
  elseif key == "pagedown" then
    move_sel(page_size())
  elseif key == "enter" then
    open_selected()
  elseif key == "ctrl+d" then
    delete_selected()
  elseif board.input:handle_key(key) ~= "ignored" then
    board.confirm = nil
    rebuild()
    render()
  end
end

local function open()
  if board then
    return
  end
  local buf = maki.ui.buf()
  local win = maki.ui.open_win(buf, {
    title = TITLE,
    width = "70%",
    height = "70%",
    border = "rounded",
    focus = true,
    footer = FOOTER_KEYS,
  })
  board = {
    win = win,
    buf = buf,
    width = win.width,
    height = win.height,
    input = TextInput.new(),
    -- Owned by render(), the only place that knows how tall the query block
    -- ended up once it wrapped.
    reserved = 0,
    tasks = {},
    rows = {},
  }
  refresh()
  if not board then
    return
  end
  for _, task in ipairs(board.tasks) do
    if task.focused then
      board.origin_id, board.sel_id = task.id, task.id
    end
  end
  render()

  while board do
    local ev = board.win:recv(TICK_MS)
    if not ev or ev.type == "close" then
      -- The window is already gone, so there is nothing to restore into.
      finish(true)
    elseif ev.type == "timeout" then
      if board.expired then
        -- The session changed under us and its ids mean nothing here now.
        finish(true)
      elseif board.dirty then
        board.dirty = false
        refresh()
      end
    elseif ev.type == "key" then
      handle_key(ev.key)
    elseif ev.type == "paste" then
      board.input:insert_text(ev.text)
      rebuild()
      render()
    elseif ev.type == "resize" then
      board.width = ev.width
      board.height = ev.height
      render()
    end
  end
end

-- Autocmds run synchronously while a refresh needs an async round-trip, so both
-- handlers only raise a flag and let the recv tick do the work.
maki.api.create_autocmd({ "TaskStatusChanged", "SessionStatusChanged" }, {
  callback = function()
    if board then
      board.dirty = true
    end
  end,
})

-- The picker only ever shows the focused session, so a session switch closes it
-- instead of leaving ids from elsewhere on screen.
maki.api.create_autocmd("SessionFocusChanged", {
  callback = function()
    if board then
      board.expired = true
    end
  end,
})

-- Every way the subagent count of the focused session can change.
maki.api.create_autocmd({ "TaskStatusChanged", "SessionFocusChanged", "SessionReset" }, {
  callback = refresh_hint,
})

maki.api.register_command({
  name = "/tasks",
  description = "Browse and search tasks",
  handler = open,
})

maki.keymap.set("n", "<C-x>", open, { desc = "Open tasks" })
