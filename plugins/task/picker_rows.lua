-- Row building for the /tasks picker in picker.lua: filtering, ordering and
-- sections, with no
-- host calls and no globals. Every refresh throws the old rows away and builds
-- them again, so nothing here can drift from the host list.

local ListPicker = require("maki.list_picker")

local RUN_ICON = "· "
local OK_ICON = "✓ "
local BAD_ICON = "✗ "

local RUNNING_SECTION = "Running"
local JOBS_SECTION = "Jobs"
local FINISHED_SECTION = "Finished"
local ELAPSED_UNITS = { { 3600, "h" }, { 60, "m" } }
local UNDER_A_MINUTE = "<1m"

local M = {}

-- The main chat comes first and has no status. The subagents follow, running
-- ones above finished ones so a long job never gets buried under the ones that
-- already returned. The Jobs section sits between: the session's command jobs
-- from any plugin, running ones first. Within a task section, newest first
-- (`tasks` comes in chat order, which is spawn order), so the latest task is
-- the one at hand. Jobs stay in list order.
--
-- Returns { rows, sections }. A row carries a section header only when it opens
-- one, and `sections` counts what survived the filter.
function M.build(tasks, jobs, query)
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

  local live_jobs, exited_jobs = {}, {}
  for _, job in ipairs(jobs or {}) do
    if ListPicker.matches(job.name or job.command, words) then
      if job.status == "running" then
        live_jobs[#live_jobs + 1] = job
      else
        exited_jobs[#exited_jobs + 1] = job
      end
    end
  end

  local rows = {}
  if main then
    rows[#rows + 1] = { task = main }
  end
  -- Task sections are newest-first. Job sections keep list order, and an
  -- exited group repeats the Jobs header only when the live half is empty.
  local function append_section(header, items, key, newest_first)
    local n = #items
    local from, to, step = 1, n, 1
    if newest_first then
      from, to, step = n, 1, -1
    end
    local first = true
    for i = from, to, step do
      rows[#rows + 1] = { [key] = items[i], section = first and header or nil }
      first = false
    end
  end
  append_section(RUNNING_SECTION, running, "task", true)
  append_section(JOBS_SECTION, live_jobs, "job", false)
  append_section(#live_jobs == 0 and JOBS_SECTION or nil, exited_jobs, "job", false)
  append_section(FINISHED_SECTION, finished, "task", true)
  return {
    rows = rows,
    sections = {
      running = #running,
      jobs = #live_jobs + #exited_jobs,
      finished = #finished,
    },
  }
end

-- Same glyph language as the task rows: the live spinner, then check or
-- cross by exit code.
function M.job_icon(job)
  if job.status == "running" then
    return RUN_ICON, "accent", true
  elseif job.exit_code == 0 then
    return OK_ICON, "success"
  end
  return BAD_ICON, "error"
end

function M.row_id(row)
  return row.task and row.task.id or row.job.id
end

function M.row_name(row)
  if row.task then
    return row.task.name
  end
  return row.job.name or row.job.command
end

-- Minutes are the smallest unit. Seconds would change the text every second,
-- and each redraw pulls a mouse-scrolled view back to the cursor.
local function elapsed(secs)
  for _, unit in ipairs(ELAPSED_UNITS) do
    if secs >= unit[1] then
      return math.floor(secs / unit[1]) .. unit[2]
    end
  end
  return UNDER_A_MINUTE
end

-- A model with no price has no cost, and we leave it out rather than show a
-- made up $0. `now` comes in from outside so tests can pin the clock.
--
-- The second result sums up the drawn text in one string. The picker checks it
-- on every tick and redraws only when it changed, which keeps a mouse-scrolled
-- view where the user left it.
function M.background(entries, query, now)
  local words = ListPicker.split_words(query)
  local rows, parts = {}, {}
  for _, entry in ipairs(entries) do
    if ListPicker.matches(entry.name, words) then
      local detail = entry.status .. " · " .. elapsed(now - entry.started_at)
      if entry.cost then
        detail = detail .. string.format(" · $%.3f", entry.cost)
      end
      rows[#rows + 1] = {
        name = entry.name,
        plugin = entry.plugin,
        detail = detail,
      }
      parts[#parts + 1] = table.concat({ entry.name, entry.plugin, detail }, "\0")
    end
  end
  return rows, table.concat(parts, "\n")
end

-- Only a finished subagent may leave the list. The main chat is the session
-- itself, and dropping a running task would orphan its transcript. The host
-- has the final say. This check only decides which rows ask "are you sure".
function M.deletable(task)
  return task.status ~= nil and task.status ~= "working"
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
