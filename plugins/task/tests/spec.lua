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

-- "2:1" reads as: row 2 sits one nesting step under the root. Main is the
-- root at 0, subagents and its jobs at 1, a subagent's jobs at 2.
local function depths(rows)
  local out = {}
  for _, row in ipairs(rows) do
    out[#out + 1] = tostring(row.depth)
  end
  return table.concat(out, ",")
end

local function folds(rows)
  local out = {}
  for _, row in ipairs(rows) do
    if row.task and row.job_count then
      out[#out + 1] = Rows.row_id(row) .. "=" .. (row.collapsed and "shut" or "open") .. row.job_count
    end
  end
  return table.concat(out, ",")
end

case("main_is_pinned_first_and_running_beats_finished", function()
  local built = Rows.build({ MAIN, RESEARCH, BUILD, BENCH, DEPLOY }, {}, "")
  eq(ids(built.rows), "main,toolu_01,toolu_04,toolu_02,toolu_03")
  eq(depths(built.rows), "0,1,1,1,1", "main at the root, subagents one step under it")
  eq(built.sections.running, 2)
  eq(built.sections.finished, 2)
end)

-- The worst case for a cursor kept as a position: the first running task
-- finishes, crosses into the section below, and everything in between shifts up
-- a row. The selection is an id, so it has to follow the task.
case("a_task_that_finishes_moves_sections_and_stays_addressable", function()
  local before = Rows.build({ MAIN, RESEARCH, DEPLOY, AUDIT, BUILD }, {}, "")
  eq(ids(before.rows), table.concat({ MAIN.id, RESEARCH.id, DEPLOY.id, AUDIT.id, BUILD.id }, ","))
  eq(Rows.index_of(before.rows, RESEARCH.id), 2)

  local after = Rows.build({ MAIN, RESEARCH_DONE, DEPLOY, AUDIT, BUILD }, {}, "")
  eq(Rows.index_of(after.rows, DEPLOY.id), 2)
  eq(Rows.index_of(after.rows, AUDIT.id), 3)
  eq(Rows.index_of(after.rows, RESEARCH.id), 4)
  eq(Rows.index_of(after.rows, BUILD.id), 5)
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
  eq(ids(finished_only.rows), BUILD.id .. "," .. BENCH.id)
  eq(depths(finished_only.rows), "1,1")
  eq(finished_only.sections.running, 0)
  eq(finished_only.sections.finished, 2)

  local running_only = Rows.build(all, {}, DEPLOY.name)
  eq(ids(running_only.rows), DEPLOY.id)
  eq(depths(running_only.rows), "1")
  eq(running_only.sections.running, 1)
  eq(running_only.sections.finished, 0)

  -- With zero rows the picker draws its "No matches" hint, and the footer next
  -- to it has to agree that nothing is left.
  local nothing = Rows.build({}, {}, "")
  eq(#nothing.rows, 0)
  eq(nothing.sections.running, 0)
  eq(nothing.sections.finished, 0)
end)

local TESTS = { id = 1, name = "just test", status = "running", exit_code = nil }
local SERVER = { id = 2, name = "serve site", command = "cargo run", status = "running", exit_code = nil }
local TESTS_DONE = { id = 1, name = "just test", command = "just test", status = "exited", exit_code = 0 }
local SERVER_CRASHED = { id = 2, name = "serve site", command = "cargo run", status = "exited", exit_code = 3 }

case("main_jobs_nest_under_main_before_the_subagents", function()
  local built = Rows.build({ MAIN, RESEARCH, BUILD }, { TESTS, SERVER }, "")
  eq(ids(built.rows), "main,1,2,toolu_01,toolu_02")
  eq(depths(built.rows), "0,1,1,1,1")
  eq(folds(built.rows), "main=open2", "main owns both jobs under one fold")
  eq(built.sections.jobs, 2)
end)

case("exited_jobs_follow_live_ones_and_survive_a_filtered_out_main", function()
  local built = Rows.build({}, { TESTS_DONE, SERVER_CRASHED }, "")
  eq(ids(built.rows), "1,2", "main filtered out does not strand its jobs")
  eq(depths(built.rows), "0,0")
end)

case("the_filter_matches_job_names_and_commands", function()
  local built = Rows.build({ MAIN }, { TESTS, SERVER }, "serve")
  eq(ids(built.rows), "2", "matches the command when there is no name")
  eq(built.sections.jobs, 1)
  eq(ids(Rows.build({ MAIN }, { TESTS, SERVER }, "nope").rows), "")
end)

-- An unnamed job used to crash the whole keybind callback: matches indexed
-- nil, the picker float stayed open and unclosable.
case("an_unnamed_job_falls_back_to_its_command", function()
  local plain = { id = 3, command = "sleep 30", status = "running", exit_code = nil }
  local built = Rows.build({ MAIN }, { plain }, "sleep")
  eq(ids(built.rows), "3", "the command stands in for the missing name")
  eq(built.sections.jobs, 1)
  eq(ids(Rows.build({ MAIN }, { plain }, "just").rows), "")
end)

case("job_icons_follow_the_exit_code", function()
  local icon, style, spinning = Rows.job_icon(TESTS)
  eq(spinning, true, "a running job spins")
  local ok_icon, ok_style, ok_spin = Rows.job_icon(TESTS_DONE)
  eq(ok_spin, nil, "an exited job does not spin")
  eq(ok_style, "success", "exit 0 gets the check")

  local bad_icon, bad_style = Rows.job_icon(SERVER_CRASHED)
  eq(bad_style, "error", "a nonzero exit gets the cross")
end)

local MONITOR_JOB =
  { id = 7, name = "watch tests", command = "just test", status = "running", spawned_by = RESEARCH.id }
local MONITOR_DONE = { id = 8, command = "sleep 30", status = "exited", exit_code = 1, spawned_by = BUILD.id }

case("a_subagents_jobs_group_under_its_name", function()
  local built = Rows.build({ MAIN, RESEARCH, BUILD }, { TESTS, MONITOR_JOB, MONITOR_DONE }, "")
  eq(ids(built.rows), "main,1,toolu_01,7,toolu_02,8")
  eq(depths(built.rows), "0,1,1,2,1,2", "each spawner nests its own jobs one step deeper")
  eq(folds(built.rows), "main=open1,toolu_01=open1,toolu_02=open1")
  eq(built.sections.jobs, 3)
end)

case("collapsing_a_task_hides_its_jobs_but_keeps_the_count", function()
  local collapsed = { [RESEARCH.id] = true }
  local built = Rows.build({ MAIN, RESEARCH, BUILD }, { MONITOR_JOB, MONITOR_DONE }, "", collapsed)
  eq(ids(built.rows), "main,toolu_01,toolu_02,8", "research's job is folded away, build's stays")
  eq(folds(built.rows), "toolu_01=shut1,toolu_02=open1", "task rows without jobs carry no fold")
end)

case("a_missing_collapsed_map_defaults_to_open", function()
  local built = Rows.build({ MAIN, RESEARCH }, { MONITOR_JOB }, "", nil)
  eq(folds(built.rows), "toolu_01=open1")
end)

case("the_filter_matches_a_job_through_its_spawners_name", function()
  local built = Rows.build({ MAIN, RESEARCH }, { MONITOR_JOB }, "research")
  eq(ids(built.rows), "toolu_01,7", "the subagent's name keeps its job visible")
  eq(ids(Rows.build({ MAIN, BUILD }, { MONITOR_JOB }, "benchmark").rows), "")
end)

case("a_subagent_job_without_a_matching_task_still_lists", function()
  -- The spawned_by id has no task row (the subagent's row was filtered out),
  -- but the job still lists rather than vanishing with its spawner.
  local built = Rows.build({ MAIN }, { MONITOR_JOB }, "")
  eq(ids(built.rows), "main,7")
  eq(depths(built.rows), "0,1", "the orphan sits one step under the root, like any job")
end)

th.report()
