-- Row building for the /tasks picker in picker.lua: filtering, ordering and
-- sections, with no host calls and no globals. Every refresh throws the old
-- rows away and builds them again, so nothing here can drift from the host
-- list.

local ListPicker = require("maki.list_picker")

local RUNNING_SECTION = "Running"
local JOBS_SECTION = "Jobs"
local FINISHED_SECTION = "Finished"
local COST_FMT = "$%.3f"
local COL_GAP = "  "

local M = {}

-- An idle plugin task is waiting for its next prompt, not finished.
local function alive(status)
  return status == "working" or status == "idle"
end

local function index_by_id(tasks)
  local by_id = {}
  for _, task in ipairs(tasks) do
    by_id[task.id] = task
  end
  return by_id
end

-- A subagent started by a moved `batch` sits under the batch. When the batch
-- is not listed, because it never moved or was deleted, the subagent stands
-- on its own. A wrapper hides itself, so its children stand alone too.
local function parent_of(task, by_id)
  return task.parent and by_id[task.parent]
end

local function listed_parent(task, by_id)
  local parent = parent_of(task, by_id)
  return parent and not parent.wrapper and parent or nil
end

-- An unnamed job is identified by the command it runs.
local function job_label(job)
  return job.name or job.command
end

-- The main chat comes first and has no status. The other tasks follow, live
-- ones above finished ones so a long job never gets buried under the ones
-- that already returned. The session's jobs sit between the two, running ones
-- first: they are work in progress the picker can stop, whether or not this
-- plugin started them, and the exited ones keep their place, since their
-- outcome is worth seeing. `opts.running_only` lists just the running ones.
--
-- Within a task section, newest first (`tasks` comes in chat order, which is
-- spawn order), so the latest task is the one at hand. Children follow their
-- parent one level in, in spawn order, whatever their status, so a finished
-- child of a running batch stays under it. A parent shows when it or one of
-- its children matches the filter. Jobs keep list order within their group.
--
-- Returns { rows, sections }. A row carries a section header only when it opens
-- one, and `depth` 1 when it sits under a parent. `sections` counts the
-- top-level rows that survived the filter.
function M.build(tasks, jobs, query, opts)
  local words = ListPicker.split_words(query)
  local running_only = opts and opts.running_only
  local by_id = index_by_id(tasks)
  local function matches(task)
    return ListPicker.matches(task.name, words)
  end

  local children = {}
  for _, task in ipairs(tasks) do
    local parent = not task.wrapper and listed_parent(task, by_id)
    if parent and matches(task) then
      children[parent.id] = children[parent.id] or {}
      children[parent.id][#children[parent.id] + 1] = task
    end
  end

  local main, running, finished = nil, {}, {}
  for _, task in ipairs(tasks) do
    if not task.wrapper and not listed_parent(task, by_id) and (matches(task) or children[task.id]) then
      if not task.status then
        main = task
      elseif alive(task.status) then
        running[#running + 1] = task
      else
        finished[#finished + 1] = task
      end
    end
  end

  local live_jobs = {}
  local exited_jobs = {}
  for _, job in ipairs(jobs or {}) do
    local label = job_label(job)
    -- A job with neither name nor command has nothing to show or match.
    if label and ListPicker.matches(label, words) and (not running_only or job.status == "running") then
      if job.status == "running" then
        live_jobs[#live_jobs + 1] = job
      else
        exited_jobs[#exited_jobs + 1] = job
      end
    end
  end
  local section_jobs = {}
  for _, group in ipairs({ live_jobs, exited_jobs }) do
    for _, job in ipairs(group) do
      section_jobs[#section_jobs + 1] = job
    end
  end

  local rows = {}
  local function add(task, section)
    rows[#rows + 1] = { task = task, section = section }
    for _, child in ipairs(children[task.id] or {}) do
      rows[#rows + 1] = { task = child, depth = 1 }
    end
  end
  if main then
    add(main)
  end
  -- Tasks read newest first, so their header goes on the last row drawn.
  local function append(header, items, key, newest_first)
    local from, to, step = 1, #items, 1
    if newest_first then
      from, to, step = #items, 1, -1
    end
    for i = from, to, step do
      local section = i == from and header or nil
      if key == "task" then
        add(items[i], section)
      else
        rows[#rows + 1] = { [key] = items[i], section = section }
      end
    end
  end
  append(RUNNING_SECTION, running, "task", true)
  append(JOBS_SECTION, section_jobs, "job", false)
  append(FINISHED_SECTION, finished, "task", true)
  return {
    rows = rows,
    sections = { running = #running, jobs = #section_jobs, finished = #finished },
  }
end

function M.row_id(row)
  return row.task and row.task.id or row.job.id
end

function M.row_name(row)
  return row.task and row.task.name or job_label(row.job)
end

-- Counts what runs the way the quit flash does. A child under a listed parent
-- is part of that parent's row, so it is not counted again.
function M.working(tasks)
  local by_id = index_by_id(tasks)
  local n = 0
  for _, task in ipairs(tasks) do
    if task.status == "working" and not task.wrapper and not listed_parent(task, by_id) then
      n = n + 1
    end
  end
  return n
end

-- Only a finished subagent may leave the list. The main chat is the session
-- itself, dropping a live task would orphan its transcript, and a job's
-- process is not ours to delete. The host has the final say. This check only
-- decides which rows ask "are you sure".
function M.deletable(task)
  return task.status ~= nil and not alive(task.status)
end

-- A part has no run of its own, so stopping it stops its whole call. Returns
-- that call while it runs, so the picker can ask first. A subagent child
-- stops alone and returns nil.
function M.stopped_parent(task, tasks)
  local parent = task.part and index_by_id(tasks)[task.parent]
  return parent and alive(parent.status) and parent or nil
end

-- The dim text at a row's right edge: how long the task has run (its total
-- once finished), what it cost, and the plugin that owns it. A row with less
-- than {room} columns to spare keeps only the owner. {humantime} formats
-- seconds, passed in so this file stays free of host calls.
local function dispw(s)
  return utf8.len(s) or #s
end

-- How long the task has run (its total once finished), what it cost, and who
-- runs it: the tool behind its call, or else the plugin that opened it.
-- {humantime} formats seconds, passed in so this file stays free of host
-- calls.
local function cells(task, now, humantime)
  local until_at = alive(task.status) and now or task.ended_at
  local time = task.started_at and until_at and humantime(math.max(until_at - task.started_at, 0)) or ""
  return {
    time = time,
    cost = task.cost and COST_FMT:format(task.cost) or "",
    owner = task.tool or task.plugin or "",
  }
end

-- The widest cell of each kind over {tasks}, so every trailer lays out in the
-- same columns and the eye can run down each.
function M.columns(tasks, now, humantime)
  local widths = { time = 0, cost = 0, owner = 0 }
  for _, task in ipairs(tasks) do
    local cell = cells(task, now, humantime)
    for kind, width in pairs(widths) do
      widths[kind] = math.max(width, dispw(cell[kind]))
    end
  end
  return widths
end

local function pad_left(s, width)
  return string.rep(" ", width - dispw(s)) .. s
end

local function pad_right(s, width)
  return s .. string.rep(" ", width - dispw(s))
end

-- Numbers right-aligned, the owner left-aligned, so a row missing a value
-- leaves its column blank instead of shifting the rest.
local COLUMNS = { { "time", pad_left }, { "cost", pad_left }, { "owner", pad_right } }

-- The dim text at a row's right edge, in {columns} from M.columns(). A row
-- with less than {room} columns to spare keeps only the owner.
function M.trailer(task, now, room, humantime, columns)
  local cell = cells(task, now, humantime)
  local parts = {}
  for _, column in ipairs(COLUMNS) do
    local kind, pad = column[1], column[2]
    if columns[kind] > 0 then
      parts[#parts + 1] = pad(cell[kind], columns[kind])
    end
  end
  local full = #parts > 0 and table.concat(parts, COL_GAP) .. " " or ""
  if dispw(full) < room then
    return full
  end
  return columns.owner > 0 and pad_right(cell.owner, columns.owner) .. " " or ""
end

-- Whether a row's running time still ticks.
function M.has_clock(tasks)
  for _, task in ipairs(tasks) do
    if task.started_at and alive(task.status) then
      return true
    end
  end
  return false
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
