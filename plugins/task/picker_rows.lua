-- Row building for the /tasks picker in picker.lua: filtering, ordering and
-- sections, with no
-- host calls and no globals. Every refresh throws the old rows away and builds
-- them again, so nothing here can drift from the host list.

local ListPicker = require("maki.list_picker")

local RUN_ICON = "· "
local OK_ICON = "✓ "
local BAD_ICON = "✗ "

local RUNNING_SECTION = "Running"
local MONITORS_SECTION = "Monitors"
local FINISHED_SECTION = "Finished"

local M = {}

-- The main chat comes first and has no status. The subagents follow, running
-- ones above finished ones so a long job never gets buried under the ones that
-- already returned. Monitors sit between: the session's live command jobs,
-- running ones first, whatever plugin started them. Within a section, chat
-- order.
--
-- Returns { rows, sections }. A row carries a section header only when it opens
-- one, and `sections` counts what survived the filter.
function M.build(tasks, monitors, query)
  local words = ListPicker.split_words(query)
  local main, running, finished = nil, {}, {}
  for _, task in ipairs(tasks) do
    if ListPicker.matches(task.name, words) then
      if not task.status then
        main = task
      elseif task.status == "working" then
        running[#running + 1] = task
      else
        finished[#finished + 1] = task
      end
    end
  end

  local live_monitors, exited_monitors = {}, {}
  for _, job in ipairs(monitors or {}) do
    if ListPicker.matches(job.name or job.command, words) then
      if job.status == "running" then
        live_monitors[#live_monitors + 1] = job
      else
        exited_monitors[#exited_monitors + 1] = job
      end
    end
  end

  local rows = {}
  if main then
    rows[#rows + 1] = { task = main }
  end
  for _, group in ipairs({
    { header = RUNNING_SECTION, items = running, key = "task" },
    { header = MONITORS_SECTION, items = live_monitors, key = "monitor" },
    -- Same section as the live ones, so the header repeats only when a
    -- filter emptied the live half.
    { header = #live_monitors == 0 and MONITORS_SECTION or nil, items = exited_monitors, key = "monitor" },
    { header = FINISHED_SECTION, items = finished, key = "task" },
  }) do
    for i, item in ipairs(group.items) do
      rows[#rows + 1] = { [group.key] = item, section = i == 1 and group.header or nil }
    end
  end
  return {
    rows = rows,
    sections = {
      running = #running,
      monitors = #live_monitors + #exited_monitors,
      finished = #finished,
    },
  }
end

-- Same glyph language as the task rows: the live spinner, then check or
-- cross by exit code.
function M.monitor_icon(job)
  if job.status == "running" then
    return RUN_ICON, "accent", true
  elseif job.exit_code == 0 then
    return OK_ICON, "success"
  end
  return BAD_ICON, "error"
end

function M.row_id(row)
  return row.task and row.task.id or row.monitor.id
end

function M.row_name(row)
  if row.task then
    return row.task.name
  end
  return row.monitor.name or row.monitor.command
end

-- Position of {id} among {rows}, or nil. The selection is kept as an id and
-- resolved here at render time, so a task moving between sections never drags
-- the cursor with it.
function M.index_of(rows, id)
  for i, row in ipairs(rows) do
    if M.row_id(row) == id then
      return i
    end
  end
  return nil
end

return M
