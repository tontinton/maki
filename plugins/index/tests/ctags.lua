-- Tests for the ctags fallback indexer. Pure helpers are tested directly; the
-- end-to-end path is skipped where universal-ctags with +json is not on PATH.

local th = require("maki.test_helpers")
local ctags = require("ctags")
local case = th.case
local eq = th.eq

local function lines_of(tag_lines)
  return table.concat(tag_lines, "\n")
end

case("ctags_parse_tags_keeps_only_real_tags", function()
  local stdout = lines_of({
    '{"_type": "tag", "name": "add", "line": 8, "kind": "function"}',
    '{"_type": "ptag", "name": "JSON_OUTPUT_VERSION", "path": "1.1"}',
    "",
    "not json at all",
    '{"_type": "tag", "name": "Foo", "line": 1, "kind": "module"}',
  })
  local tags = ctags.parse_tags(stdout)
  eq(#tags, 2)
  eq(tags[1].name, "add")
  eq(tags[2].name, "Foo")
end)

case("ctags_group_tags_sorts_by_line_and_buckets_kinds", function()
  local tags = {
    { name = "b_fn", line = 20, kind = "function" },
    { name = "Widget", line = 3, kind = "class" },
    { name = "a_fn", line = 5, kind = "function" },
    { name = "Weird", line = 9, kind = "totally_unknown_kind" },
  }
  local grouped = ctags.group_tags(tags)
  eq(grouped.fns[1].name, "a_fn")
  eq(grouped.fns[2].name, "b_fn")
  eq(grouped.classes[1].name, "Widget")
  eq(grouped.tags[1].name, "Weird")
end)

case("ctags_format_outline_is_flat_and_announces_itself", function()
  local grouped = ctags.group_tags({
    { name = "add", line = 8, kind = "function" },
    { name = "Foo", line = 1, kind = "module" },
  })
  local out = ctags.format_outline("a.ex", grouped)
  local expected = lines_of({
    "a.ex (ctags outline, start lines only)",
    "mod:",
    "  Foo [1]",
    "fns:",
    "  add [8]",
  })
  eq(out, expected)
end)

case("ctags_format_outline_omits_missing_sections", function()
  local grouped = ctags.group_tags({ { name = "x", line = 4, kind = "function" } })
  local out = ctags.format_outline("f.ex", grouped)
  assert(not out:find("mod:", 1, true), "empty sections must not appear:\n" .. out)
  assert(not out:find("types:", 1, true), "empty sections must not appear:\n" .. out)
end)

case("ctags_empty_tags_yield_no_outline", function()
  -- What makes `outline` answer nil (so the handler keeps its own
  -- "unsupported" error rather than reporting an empty skeleton) is an empty
  -- grouping. The jobstart/jobwait path itself cannot run from here: jobwait
  -- parks the coroutine, and `case` wraps us in pcall, which cannot yield.
  local grouped = ctags.group_tags(ctags.parse_tags(""))
  eq(next(grouped), nil)
  eq(#ctags.parse_tags('{"_type": "ptag", "name": "x"}'), 0)
end)
