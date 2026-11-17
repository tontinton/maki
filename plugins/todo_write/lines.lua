-- Pure formatting for todo lists: the marker lines the panel and the chat body
-- share, and the panel's truncation rules. No maki state, so the spec can
-- require it directly.

local MORE_FMT = "+%d more"

local STATUS_MARKERS = {
  completed = { "[✓]", "todo_completed" },
  in_progress = { "[•]", "todo_in_progress" },
  pending = { "[ ]", "todo_pending" },
  cancelled = { "[x]", "todo_cancelled" },
}

local M = {}

local function line_for(item)
  local marker = STATUS_MARKERS[item.status] or STATUS_MARKERS.pending
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
    if item.status == "completed" then
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

local function more_hint(count)
  return { { string.format(MORE_FMT, count), "tool_dim" } }
end

-- A window that follows progress: one row before the first unfinished item,
-- then the live work and what comes next. A finished list shows its tail. The
-- clamp covers both ends, so the window never runs past the list.
function M.panel_lines(items, max)
  if #items <= max then
    return M.build_lines(items)
  end
  local live = first_of(items, "in_progress") or first_of(items, "pending")
  local start = live and live - (max > 1 and 1 or 0) or #items
  start = math.max(1, math.min(start, #items - max + 1))
  local lines = {}
  for i = start, start + max - 1 do
    lines[#lines + 1] = line_for(items[i])
  end
  lines[#lines + 1] = more_hint(#items - max)
  return lines
end

return M
