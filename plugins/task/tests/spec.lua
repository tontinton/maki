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
    out[#out + 1] = row.task.id
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
  local built = Rows.build({ MAIN, RESEARCH, BUILD, BENCH, DEPLOY }, "")
  eq(ids(built.rows), "main,toolu_01,toolu_04,toolu_02,toolu_03")
  eq(sections(built.rows), "2:Running,4:Finished", "main has no header and each section opens once")
  eq(built.sections.running, 2)
  eq(built.sections.finished, 2)
end)

-- The worst case for a cursor kept as a position: the first running task
-- finishes, crosses into the section below, and everything in between shifts up
-- a row. The selection is an id, so it has to follow the task.
case("a_task_that_finishes_moves_sections_and_stays_addressable", function()
  local before = Rows.build({ MAIN, RESEARCH, DEPLOY, AUDIT, BUILD }, "")
  eq(ids(before.rows), table.concat({ MAIN.id, RESEARCH.id, DEPLOY.id, AUDIT.id, BUILD.id }, ","))
  eq(sections(before.rows), "2:Running,5:Finished")
  eq(Rows.index_of(before.rows, RESEARCH.id), 2)

  local after = Rows.build({ MAIN, RESEARCH_DONE, DEPLOY, AUDIT, BUILD }, "")
  eq(sections(after.rows), "2:Running,4:Finished")
  eq(Rows.index_of(after.rows, DEPLOY.id), 2)
  eq(Rows.index_of(after.rows, AUDIT.id), 3)
  eq(Rows.index_of(after.rows, RESEARCH.id), 4)
  eq(Rows.index_of(after.rows, BUILD.id), 5)
end)

-- `rebuild` resolves the cursor through `index_of(rows, board.sel_id)`, and
-- `sel_id` is nil when nothing is selected, so a nil id has to match no row
-- rather than the first one.
case("index_of_matches_no_row_for_a_nil_or_departed_id", function()
  local built = Rows.build({ MAIN, RESEARCH }, "")
  eq(Rows.index_of(built.rows, nil), nil)
  eq(Rows.index_of(built.rows, BUILD.id), nil)
end)

case("the_filter_matches_any_name_including_the_main_chat", function()
  local all = { MAIN, RESEARCH, BUILD, BENCH }
  eq(ids(Rows.build(all, "arch").rows), RESEARCH.id, "matches inside a name")
  eq(ids(Rows.build(all, RESEARCH.name).rows), RESEARCH.id, "the main chat is filtered out like any other row")
  eq(ids(Rows.build(all, "nope").rows), "")
end)

-- The counts feed the footer, which describes the rows on screen, so a filter
-- that empties a section has to zero its tally and drop its header.
case("a_filter_that_empties_a_section_zeroes_its_count_and_header", function()
  local all = { MAIN, RESEARCH, BUILD, BENCH, DEPLOY }

  local finished_only = Rows.build(all, "b")
  eq(ids(finished_only.rows), BUILD.id .. "," .. BENCH.id)
  eq(sections(finished_only.rows), "1:Finished", "no running row means no Running header")
  eq(finished_only.sections.running, 0)
  eq(finished_only.sections.finished, 2)

  local running_only = Rows.build(all, DEPLOY.name)
  eq(ids(running_only.rows), DEPLOY.id)
  eq(sections(running_only.rows), "1:Running", "no finished row means no Finished header")
  eq(running_only.sections.running, 1)
  eq(running_only.sections.finished, 0)

  -- With zero rows the picker draws its "No matches" hint, and the footer next
  -- to it has to agree that nothing is left.
  local nothing = Rows.build({}, "")
  eq(#nothing.rows, 0)
  eq(nothing.sections.running, 0)
  eq(nothing.sections.finished, 0)
end)

-- The picker refuses on its own before ever round-tripping to the host, and
-- the rule lives in Rows so it can be exercised without a UI.
case("only_finished_subagents_are_deletable", function()
  eq(Rows.deletable(MAIN), false, "the main chat has no status and stays")
  eq(Rows.deletable(RESEARCH), false, "a running task stays")
  eq(Rows.deletable(BUILD), true, "a done task goes")
  eq(Rows.deletable(BENCH), true, "an errored task still counts as finished")
end)

th.report()
