-- Tests for the ctags fallback indexer. Only the pure helpers run here: the
-- job path needs `jobwait`, which cannot suspend inside `case`.

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

case("ctags_group_tags_buckets_by_kind_name", function()
  local tags = {
    { name = "b_fn", line = 20, kind = "function" },
    { name = "Widget", line = 3, kind = "class" },
    { name = "a_fn", line = 5, kind = "function" },
    { name = "Weird", line = 9 },
  }
  local grouped = ctags.group_tags(tags)
  eq(#grouped["function"], 2)
  eq(grouped["class"][1].name, "Widget")
  eq(grouped.tags[1].name, "Weird")
end)

case("ctags_format_outline_headings_use_the_reported_kind", function()
  local grouped = ctags.group_tags({
    { name = "Widget", line = 1, kind = "class" },
    { name = "hello", line = 1, ["end"] = 3, kind = "procedure" },
    { name = "draw", line = 3, ["end"] = 5, kind = "method", scope = "Widget" },
    { name = "setup", line = 2, kind = "subroutine" },
  })
  local out = ctags.format_outline("a.tcl", grouped)
  local expected = lines_of({
    "a.tcl (ctags outline)",
    "class:",
    "  Widget [1]",
    "method:",
    "  Widget.draw [3-5]",
    "procedure:",
    "  hello [1-3]",
    "subroutine:",
    "  setup [2]",
  })
  eq(out, expected)
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
