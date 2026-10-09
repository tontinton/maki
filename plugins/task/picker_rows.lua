-- Row building for the /tasks picker in picker.lua: filtering, ordering and
-- nesting, with no host calls and no globals. Every refresh throws the old
-- rows away and builds them again, so nothing here can drift from the host
-- list.
--
-- The picker is one tree: Main at the root, subagents indented under it, and
-- each node's command jobs nested right under it, running ones first. A job
-- carries spawned_by (the task id of the subagent that started it, nil for
-- the main chat), which is what decides where it nests. `collapsed` maps task
-- ids with hidden jobs.

local ListPicker = require("maki.list_picker")

local RUN_ICON = "· "
local OK_ICON = "✓ "
local BAD_ICON = "✗ "

local M = {}

local function bucket_count(live, exited, key)
  return #(live[key] or {}) + #(exited[key] or {})
end

local function append_jobs(rows, live, exited, key, depth)
  for _, job in ipairs(live[key] or {}) do
    rows[#rows + 1] = { job = job, depth = depth }
  end
  for _, job in ipairs(exited[key] or {}) do
    rows[#rows + 1] = { job = job, depth = depth }
  end
end

-- Returns { rows, sections }. A task row owning jobs carries `job_count` and
-- `collapsed`, so the picker can draw the collapse glyph and know the
-- children are hidden; job rows carry `depth` only.
function M.build(tasks, jobs, query, collapsed)
  collapsed = collapsed or {}
  local words = ListPicker.split_words(query)
  local main, running, finished = nil, {}, {}
  local task_by_id = {}
  for _, task in ipairs(tasks) do
    task_by_id[task.id] = task
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

  -- A job matches on its own text or on its spawner's name, so filtering by
  -- the subagent keeps its jobs visible.
  local main_live, main_exited = {}, {}
  local sub_live, sub_exited = {}, {}
  local orphan_ids, seen_sub = {}, {}
  local job_count = 0
  for _, job in ipairs(jobs or {}) do
    local spawner = job.spawned_by and task_by_id[job.spawned_by]
    local matches = ListPicker.matches(job.name or job.command, words)
      or (spawner ~= nil and ListPicker.matches(spawner.name, words))
    if matches then
      local live, exited, key
      if job.spawned_by then
        live, exited, key = sub_live, sub_exited, job.spawned_by
        if not spawner and not seen_sub[key] then
          seen_sub[key] = true
          orphan_ids[#orphan_ids + 1] = key
        end
      else
        live, exited, key = main_live, main_exited, "main"
      end
      job_count = job_count + 1
      if job.status == "running" then
        live[key] = live[key] or {}
        local bucket = live[key]
        bucket[#bucket + 1] = job
      else
        exited[key] = exited[key] or {}
        local bucket = exited[key]
        bucket[#bucket + 1] = job
      end
    end
  end

  local rows = {}
  local function push_with_jobs(task, depth, live, exited, key)
    local count = bucket_count(live, exited, key)
    rows[#rows + 1] = {
      task = task,
      depth = depth,
      job_count = count > 0 and count or nil,
      collapsed = collapsed[task.id],
    }
    if not collapsed[task.id] then
      append_jobs(rows, live, exited, key, depth + 1)
    end
  end

  if main then
    push_with_jobs(main, 0, main_live, main_exited, "main")
  else
    -- Main filtered out by the query; its jobs still have nowhere else to go.
    append_jobs(rows, main_live, main_exited, "main", 0)
  end
  -- Running subagents before finished ones, each carrying its own jobs.
  for _, task in ipairs(running) do
    push_with_jobs(task, 1, sub_live, sub_exited, task.id)
  end
  -- A spawned_by with no matching task row (the row is filtered out, or the
  -- host never sent one) keeps its jobs listable under the raw id.
  for _, id in ipairs(orphan_ids) do
    append_jobs(rows, sub_live, sub_exited, id, 1)
  end
  for _, task in ipairs(finished) do
    push_with_jobs(task, 1, sub_live, sub_exited, task.id)
  end

  return {
    rows = rows,
    sections = {
      running = #running,
      jobs = job_count,
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
