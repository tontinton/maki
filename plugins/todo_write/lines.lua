-- Pure formatting for todo lists: the marker lines the panel, the /todo
-- popup and the chat body share, and the panel's truncation rules. No maki
-- state, so the spec can require it directly.

local S = require("enums")
local HINT_STYLE = "tool_dim"
local MORE_FMT = "+%d more · /todo"
local TITLE_PREFIX = " Todos · "
local COUNT_FMT = "%d/%d"
local WINDOW_MAX_ITEMS = 16
local WINDOW_CHROME = 2

local STATUS_MARKERS = {
  [S.completed] = { "[✓]", "todo_completed" },
  [S.in_progress] = { "[•]", "todo_in_progress" },
  [S.pending] = { "[ ]", "todo_pending" },
  [S.cancelled] = { "[x]", "todo_cancelled" },
}

local M = {}

local function line_for(item)
  local marker = STATUS_MARKERS[item.status] or STATUS_MARKERS[S.pending]
  return { { marker[1] .. " " .. item.content, marker[2] } }
end

function M.build_lines(items)
  local lines = {}
  for _, item in ipairs(items) do
    lines[#lines + 1] = line_for(item)
  end
  return lines
end

function M.count_done(items)
  local n = 0
  for _, item in ipairs(items) do
    if item.status == S.completed then
      n = n + 1
    end
  end
  return n
end

local function first_of(items, status)
  for i, item in ipairs(items) do
    if item.status == status then
      return i
    end
  end
end

-- The row the panel starts from: the last completed item, so the list reads
-- as "just did this"; with nothing completed, the first item that is still
-- live; with nothing live either, the first at all.
local function anchor_index(items)
  for i = #items, 1, -1 do
    if items[i].status == S.completed then
      return i
    end
  end
  for i = 1, #items do
    local st = items[i].status
    if st ~= S.completed and st ~= S.cancelled then
      return i
    end
  end
  return 1
end

local function more_hint(count)
  return { { string.format(MORE_FMT, count), HINT_STYLE } }
end

-- The panel anchors on the last completed item, then shows the items after
-- it in list order up to `max` rows; cancelled items are never rows. With
-- `max` of 1 the single row is instead the first in_progress item, then the
-- first pending, then the anchor, so a finished list shows its newest item.
-- What the cut hides becomes a "+N more" hint.
function M.panel_lines(items, max)
  if #items <= max then
    return M.build_lines(items)
  end
  local start_i
  if max == 1 then
    start_i = first_of(items, S.in_progress) or first_of(items, S.pending)
  end
  start_i = start_i or anchor_index(items)
  local lines, shown = {}, 0
  for i = start_i, #items do
    if items[i].status ~= S.cancelled then
      shown = shown + 1
      lines[shown] = line_for(items[i])
      if shown == max then
        break
      end
    end
  end
  if shown == 0 then
    lines[1] = line_for(items[1])
    shown = 1
  end
  lines[#lines + 1] = more_hint(#items - shown)
  return lines
end

function M.window_title(done, total)
  return TITLE_PREFIX .. string.format(COUNT_FMT, done, total) .. " "
end

function M.window_height(count)
  return math.min(count, WINDOW_MAX_ITEMS) + WINDOW_CHROME
end

return M
