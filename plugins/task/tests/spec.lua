local Rows = require("picker_rows")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local MAIN = { id = "main", name = "Main", focused = true }
local RESEARCH = { id = "toolu_01", name = "research", status = "working", focused = false }
local BUILD = { id = "toolu_02", name = "build", status = "done", focused = false }
local BENCH = { id = "toolu_03", name = "benchmark", status = "error", focused = false }
local DEPLOY = { id = "toolu_04", name = "deploy", status = "working", focused = false }
local AUDIT = { id = "toolu_05", name = "audit", status = "working", focused = false }
local MONITOR = { id = "session-01", name = "ci-monitor", status = "idle", plugin = "ci", focused = false }
local RESEARCH_DONE = { id = RESEARCH.id, name = RESEARCH.name, status = "done", focused = false }

case("maki_agent_has_expected_functions", function()
  assert(type(maki.agent) == "table", "maki.agent must be a table")
  local expected = { "resolve_model", "system_prompt", "tools", "call_tool", "session" }
  for _, fn_name in ipairs(expected) do
    eq(type(maki.agent[fn_name]), "function", "maki.agent." .. fn_name .. " must be a function")
  end
end)

case("schema_validator_compiles_and_validates", function()
  local validator, err = maki.json.schema_validator({
    type = "object",
    properties = { answer = { type = "string" } },
    required = { "answer" },
  })
  eq(err, nil, "valid schema must compile")
  eq(validator:validate({ answer = "42" }), nil, "matching value must produce no errors")
  local errors = validator:validate({ answer = 42 })
  assert(type(errors) == "table" and #errors > 0, "mismatch must produce error list")
end)

case("schema_validator_rejects_bad_schema", function()
  local validator, err = maki.json.schema_validator({ type = 42 })
  eq(validator, nil, "bad schema must not compile")
  assert(err ~= nil, "bad schema must return an error")
end)

local function ids(rows)
  local out = {}
  for _, row in ipairs(rows) do
    out[#out + 1] = tostring(Rows.row_id(row))
  end
  return table.concat(out, ",")
end

-- "3:Finished" reads as: row 3 opens the Finished section. Rows left out of the
-- string draw no header, so it also pins down where sections do not appear.
local function sections(rows)
  local out = {}
  for i, row in ipairs(rows) do
    if row.section then
      out[#out + 1] = i .. ":" .. row.section
    end
  end
  return table.concat(out, ",")
end

case("main_is_pinned_first_and_running_beats_finished", function()
  local built = Rows.build({ MAIN, RESEARCH, BUILD, BENCH, DEPLOY }, {}, "")
  eq(ids(built.rows), "main,toolu_04,toolu_01,toolu_03,toolu_02", "newest first within a section")
  eq(sections(built.rows), "2:Running,4:Finished", "main has no header and each section opens once")
  eq(built.sections.running, 2)
  eq(built.sections.finished, 2)
end)

-- The worst case for a cursor kept as a position: the first running task
-- finishes, crosses into the section below, and everything in between shifts up
-- a row. The selection is an id, so it has to follow the task.
case("a_task_that_finishes_moves_sections_and_stays_addressable", function()
  local before = Rows.build({ MAIN, RESEARCH, DEPLOY, AUDIT, BUILD }, {}, "")
  eq(ids(before.rows), table.concat({ MAIN.id, AUDIT.id, DEPLOY.id, RESEARCH.id, BUILD.id }, ","))
  eq(sections(before.rows), "2:Running,5:Finished")
  eq(Rows.index_of(before.rows, RESEARCH.id), 4)

  local after = Rows.build({ MAIN, RESEARCH_DONE, DEPLOY, AUDIT, BUILD }, {}, "")
  eq(sections(after.rows), "2:Running,4:Finished")
  eq(Rows.index_of(after.rows, AUDIT.id), 2)
  eq(Rows.index_of(after.rows, DEPLOY.id), 3)
  eq(Rows.index_of(after.rows, BUILD.id), 4)
  eq(Rows.index_of(after.rows, RESEARCH.id), 5)
end)

-- `rebuild` resolves the cursor through `index_of(rows, board.sel_id)`, and
-- `sel_id` is nil when nothing is selected, so a nil id has to match no row
-- rather than the first one.
case("index_of_matches_no_row_for_a_nil_or_departed_id", function()
  local built = Rows.build({ MAIN, RESEARCH }, {}, "")
  eq(Rows.index_of(built.rows, nil), nil)
  eq(Rows.index_of(built.rows, BUILD.id), nil)
end)

case("the_filter_matches_any_name_including_the_main_chat", function()
  local all = { MAIN, RESEARCH, BUILD, BENCH }
  eq(ids(Rows.build(all, {}, "arch").rows), RESEARCH.id, "matches inside a name")
  eq(ids(Rows.build(all, {}, RESEARCH.name).rows), RESEARCH.id, "the main chat is filtered out like any other row")
  eq(ids(Rows.build(all, {}, "nope").rows), "")
end)

-- The counts feed the footer, which describes the rows on screen, so a filter
-- that empties a section has to zero its tally and drop its header.
case("a_filter_that_empties_a_section_zeroes_its_count_and_header", function()
  local all = { MAIN, RESEARCH, BUILD, BENCH, DEPLOY }

  local finished_only = Rows.build(all, {}, "b")
  eq(ids(finished_only.rows), BENCH.id .. "," .. BUILD.id)
  eq(sections(finished_only.rows), "1:Finished", "no running row means no Running header")
  eq(finished_only.sections.running, 0)
  eq(finished_only.sections.finished, 2)

  local running_only = Rows.build(all, {}, DEPLOY.name)
  eq(ids(running_only.rows), DEPLOY.id)
  eq(sections(running_only.rows), "1:Running", "no finished row means no Finished header")
  eq(running_only.sections.running, 1)
  eq(running_only.sections.finished, 0)

  -- With zero rows the picker draws its "No matches" hint, and the footer next
  -- to it has to agree that nothing is left.
  local nothing = Rows.build({}, {}, "")
  eq(#nothing.rows, 0)
  eq(nothing.sections.running, 0)
  eq(nothing.sections.finished, 0)
end)

case("only_finished_subagents_are_deletable", function()
  eq(Rows.deletable(MAIN), false, "the main chat has no status and stays")
  eq(Rows.deletable(RESEARCH), false, "a running task stays")
  eq(Rows.deletable(BUILD), true, "a done task goes")
  eq(Rows.deletable(BENCH), true, "an errored task still counts as finished")
  eq(Rows.deletable(MONITOR), false, "an idle plugin task is still alive")
end)

local BATCH = { id = "toolu_08", name = "3 tools", status = "working", plugin = "batch", focused = false }
local CHILD_DONE = { id = "session-02", name = "lint", status = "done", parent = BATCH.id, focused = false }
local CHILD_RUNNING = { id = "session-03", name = "tests", status = "working", parent = BATCH.id, focused = false }
local ORPHAN = { id = "session-04", name = "docs", status = "working", parent = "toolu_gone", focused = false }

local function depths(rows)
  local out = {}
  for _, row in ipairs(rows) do
    out[#out + 1] = row.depth or 0
  end
  return table.concat(out, ",")
end

-- A finished subagent stays under its running batch rather than dropping into
-- the Finished section on its own.
case("subagents_sit_under_the_task_that_started_them", function()
  local built = Rows.build({ MAIN, BATCH, CHILD_DONE, CHILD_RUNNING, BUILD }, {}, "")
  eq(ids(built.rows), table.concat({ MAIN.id, BATCH.id, CHILD_DONE.id, CHILD_RUNNING.id, BUILD.id }, ","))
  eq(depths(built.rows), "0,0,1,1,0")
  eq(sections(built.rows), "2:Running,5:Finished")
  eq(built.sections.running, 1, "a subagent is part of its parent's task")
  eq(built.sections.finished, 1)
end)

case("a_subagent_whose_parent_is_not_listed_is_a_task_of_its_own", function()
  local built = Rows.build({ MAIN, ORPHAN }, {}, "")
  eq(ids(built.rows), MAIN.id .. "," .. ORPHAN.id)
  eq(depths(built.rows), "0,0")
end)

case("a_matching_subagent_brings_its_parent_along", function()
  local all = { MAIN, BATCH, CHILD_DONE, CHILD_RUNNING }
  eq(ids(Rows.build(all, {}, CHILD_RUNNING.name).rows), BATCH.id .. "," .. CHILD_RUNNING.id)
  eq(ids(Rows.build(all, {}, BATCH.name).rows), BATCH.id, "a matching parent leaves unmatched subagents out")
end)

case("the_working_count_leaves_out_subagents_of_a_listed_task", function()
  eq(Rows.working({ MAIN, BATCH, CHILD_RUNNING, ORPHAN, BUILD }), 2)
end)

-- A part stops with its whole call, so its stop names the call. A subagent
-- the call started stops alone.
case("only_a_part_of_a_running_call_stops_its_parent", function()
  local part = { id = BATCH.id .. "/1", name = "serve", status = "done", parent = BATCH.id, part = true }
  local finished_batch = { id = BATCH.id, name = BATCH.name, status = "done" }
  eq(Rows.stopped_parent(part, { MAIN, BATCH, part }), BATCH)
  eq(Rows.stopped_parent(CHILD_RUNNING, { MAIN, BATCH, CHILD_RUNNING }), nil, "a subagent stops alone")
  eq(Rows.stopped_parent(part, { MAIN, finished_batch, part }), nil, "a finished call stops nothing")
end)

case("a_cancelled_task_is_finished", function()
  local stopped = { id = "toolu_09", name = "serve", status = "cancelled", focused = false }
  eq(Rows.deletable(stopped), true)
  eq(sections(Rows.build({ MAIN, stopped }, {}, "").rows), "2:Finished")
end)

case("an_idle_plugin_task_sorts_with_the_running_ones", function()
  local built = Rows.build({ MAIN, BUILD, MONITOR }, {}, "")
  eq(ids(built.rows), "main,session-01,toolu_02")
  eq(sections(built.rows), "2:Running,3:Finished")
end)

local NOW = 1000
local WIDE = 80
local function secs(n)
  return n .. "s"
end

case("a_running_task_shows_its_running_time_and_cost", function()
  local task = { id = "toolu_06", name = "lint", status = "working", started_at = NOW - 12, cost = 0.0421 }
  eq(Rows.trailer(task, NOW, WIDE, secs), "12s · $0.042 ")
end)

case("a_finished_task_shows_its_total_time_not_a_growing_one", function()
  local task = { id = "toolu_07", name = "lint", status = "done", started_at = NOW - 30, ended_at = NOW - 10 }
  eq(Rows.trailer(task, NOW, WIDE, secs), "20s ")
  task.ended_at = nil
  eq(Rows.trailer(task, NOW, WIDE, secs), "", "no end time means no total to show")
end)

case("the_owner_closes_the_trailer_and_survives_a_narrow_row", function()
  local task = { id = "session-02", name = "ci", status = "idle", plugin = "ci", started_at = NOW - 5 }
  eq(Rows.trailer(task, NOW, WIDE, secs), "5s · ci ")
  eq(Rows.trailer(task, NOW, 4, secs), "ci ", "no room for the stats")
  eq(Rows.trailer(MAIN, NOW, WIDE, secs), "", "the main chat has no clock")
end)

case("only_a_live_task_with_a_start_keeps_the_clock_ticking", function()
  eq(Rows.has_clock({ MAIN, RESEARCH, BUILD }), false, "no start times")
  eq(Rows.has_clock({ MAIN, { id = "x", name = "x", status = "done", started_at = NOW } }), false)
  eq(Rows.has_clock({ MAIN, { id = "y", name = "y", status = "working", started_at = NOW } }), true)
end)

local TESTS = { id = 1, name = "just test", command = "just test", status = "running" }
local SERVER = { id = 2, name = "serve site", command = "cargo run", status = "running" }
local PLAIN = { id = 3, command = "sleep 30", status = "running" }

case("running_jobs_sit_between_running_and_finished_tasks", function()
  local built = Rows.build({ MAIN, RESEARCH, BUILD }, { TESTS, SERVER }, "")
  eq(ids(built.rows), "main,toolu_01,1,2,toolu_02")
  eq(sections(built.rows), "2:Running,3:Jobs,5:Finished")
  eq(built.sections.jobs, 2)
end)

-- A job's owner already reported how it ended, so the picker only lists the
-- ones it can still stop.
case("exited_jobs_leave_the_list", function()
  local done = { id = 1, name = "just test", status = "exited", exit_code = 0 }
  local built = Rows.build({ MAIN }, { done }, "")
  eq(ids(built.rows), "main")
  eq(built.sections.jobs, 0)
end)

case("the_filter_matches_job_names_and_commands", function()
  local by_name = Rows.build({ MAIN }, { TESTS, SERVER }, "serve")
  eq(ids(by_name.rows), "2")
  eq(by_name.sections.jobs, 1)
  eq(ids(Rows.build({ MAIN }, { TESTS, SERVER }, "nope").rows), "")
end)

-- An unnamed job used to crash the whole keybind callback: matches indexed
-- nil, the picker float stayed open and unclosable.
case("an_unnamed_job_falls_back_to_its_command", function()
  local built = Rows.build({ MAIN }, { PLAIN }, "sleep")
  eq(ids(built.rows), "3", "the command stands in for the missing name")
  eq(built.sections.jobs, 1)
  eq(ids(Rows.build({ MAIN }, { PLAIN }, "just").rows), "")
end)

case("a_job_row_is_addressable_by_its_id", function()
  local built = Rows.build({ MAIN, RESEARCH }, { TESTS }, "")
  eq(Rows.index_of(built.rows, TESTS.id), 3)
  eq(Rows.index_of(built.rows, RESEARCH.id), 2)
end)

th.report()
