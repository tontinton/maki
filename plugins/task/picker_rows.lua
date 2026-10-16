-- Row building for the /tasks picker in picker.lua: filtering, ordering and
-- sections, with no host calls and no globals. Every refresh throws the old
-- rows away and builds them again, so nothing here can drift from the host
-- list.

local ListPicker = require("maki.list_picker")

local RUNNING_SECTION = "Running"
local FINISHED_SECTION = "Finished"

local M = {}

-- An idle plugin task is waiting for its next prompt, not finished.
local function alive(status)
  return status == "working" or status == "idle"
end

-- The main chat comes first and has no status. The subagents follow, live
-- ones above finished ones so a long job never gets buried under the ones
-- that already returned. Within a section, newest first (`tasks` comes in
-- chat order, which is spawn order), so the latest task is the one at hand.
--
-- Returns { rows, sections }. A row carries a section header only when it opens
-- one, and `sections` counts what survived the filter.
function M.build(tasks, query)
  local words = ListPicker.split_words(query)
  local main, running, finished = nil, {}, {}
  for _, task in ipairs(tasks) do
    if ListPicker.matches(task.name, words) then
      if not task.status then
        main = task
      elseif alive(task.status) then
        running[#running + 1] = task
      else
        finished[#finished + 1] = task
      end
    end
  end

  local rows = {}
  if main then
    rows[#rows + 1] = { task = main }
  end
  for _, group in ipairs({ { RUNNING_SECTION, running }, { FINISHED_SECTION, finished } }) do
    local header, tasks_in_section = group[1], group[2]
    for i = #tasks_in_section, 1, -1 do
      rows[#rows + 1] = { task = tasks_in_section[i], section = i == #tasks_in_section and header or nil }
    end
  end
  return { rows = rows, sections = { running = #running, finished = #finished } }
end

-- Only a finished subagent may leave the list. The main chat is the session
-- itself, and dropping a live task would orphan its transcript. The host has
-- the final say. This check only decides which rows ask "are you sure".
function M.deletable(task)
  return task.status ~= nil and not alive(task.status)
end

-- Position of {id} among {rows}, or nil. The selection is kept as an id and
-- resolved here at render time, so a task moving between sections never drags
-- the cursor with it.
function M.index_of(rows, id)
  for i, row in ipairs(rows) do
    if row.task.id == id then
      return i
    end
  end
  return nil
end

return M
