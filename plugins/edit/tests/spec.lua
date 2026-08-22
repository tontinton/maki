local fr = require("maki.fuzzy_replace")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq
local has = th.has

local R = "REPLACED"
local NO_MATCH = fr.NO_MATCH
local MULTIPLE_MATCHES = fr.MULTIPLE_MATCHES
local EMPTY_OLD_STRING = fr.EMPTY_OLD_STRING

-- fuzzy_replace unit tests

case("exact_match", function()
  local result = fr.replace("fn foo() {}\nfn bar() {}", "fn foo() {}", R, false)
  has(result, R)
end)

case("trimmed_boundary", function()
  local result = fr.replace("fn foo() {}", "\nfn foo() {}\n", R, false)
  has(result, R)
end)

case("different_indentation", function()
  local result = fr.replace("    fn f() {\n        bar();\n    }", "fn f() {\n    bar();\n}", R, false)
  has(result, R)
end)

case("fuzzy_match_reindents_new_string", function()
  local content = "class Foo\n  def bar\n    baz(\n      a,\n    )\n  end\nend\n"
  local old = "def bar\n  baz(\n    a,\n  )\nend"
  local new = "def bar\n  baz(\n    a,\n    b,\n  )\nend"
  local result = fr.replace(content, old, new, false)
  eq(result, "class Foo\n  def bar\n    baz(\n      a,\n      b,\n    )\n  end\nend\n")
end)

case("reindent_ignores_new_string_own_indentation", function()
  local content = "class A\n  def f\n    x\n  end\nend\n"
  local old = "def f\n  x\nend"
  local new = "  def f\n    y\n  end"
  local result = fr.replace(content, old, new, false)
  eq(result, "class A\n  def f\n    y\n  end\nend\n")
end)

case("reindent_converts_spaces_to_tabs", function()
  local content = "\tfn f() {\n\t\tif x {\n\t\t\tg();\n\t\t}\n\t}"
  local old = "fn f() {\n    if x {\n        g();\n    }\n}"
  local new = "fn f() {\n    if x {\n        h();\n    }\n}"
  local result = fr.replace(content, old, new, false)
  eq(result, "\tfn f() {\n\t\tif x {\n\t\t\th();\n\t\t}\n\t}")
end)

case("reindent_leaves_a_correct_new_string_alone", function()
  local content = "def f():\n    a = 1 \n    return a\n"
  local old = "    a = 1\n    return a"
  local new = "    a = 1\n    return a\n\n\ndef g():\n    return 2"
  local result = fr.replace(content, old, new, false)
  eq(result, "def f():\n    a = 1\n    return a\n\n\ndef g():\n    return 2\n")
end)

case("reindent_leaves_a_line_that_exits_the_block_alone", function()
  local content = "def f():\n    a = 1\n    return a\n"
  local new = "  a = 1\n  return a\n\n\ndef g():\n  return 2"
  local result = fr.replace(content, "  a = 1\n  return a", new, false)
  eq(result, "def f():\n    a = 1\n    return a\n\n\ndef g():\n    return 2\n")
end)

case("reindent_keeps_the_block_when_a_column_zero_line_is_dropped", function()
  local content = "def f():\n    a = 1\n# TODO\n    b = 2\n"
  local old = "  a = 1\n# TODO\n  b = 2"
  local result = fr.replace(content, old, "  a = 1\n  b = 2", false)
  eq(result, "def f():\n    a = 1\n    b = 2\n")
end)

case("reindent_takes_its_widths_from_the_block_not_the_file", function()
  local content = 'M = """\n\tgcc x.c\n"""\n\ndef g():\n    if x:\n        foo()\n'
  local old = "  if x:\n    foo()"
  local result = fr.replace(content, old, "  if x:\n    foo()\n    baz()", false)
  eq(result, 'M = """\n\tgcc x.c\n"""\n\ndef g():\n    if x:\n        foo()\n        baz()\n')
end)

case("reindent_leaves_a_midline_match_alone", function()
  local result = fr.replace("    let x = compute(a,  b);", "compute(a, b)", "compute(c, d)", false)
  eq(result, "    let x = compute(c, d);")
end)

case("reindent_applies_to_every_replaced_occurrence", function()
  local content = "  a();\n  b();\nx\n  a();\n  b();\n"
  local result = fr.replace(content, "a();\nb();", "a();\nc();\nb();", true)
  eq(result, "  a();\n  c();\n  b();\nx\n  a();\n  c();\n  b();\n")
end)

case("exact_match_keeps_new_string_indentation", function()
  local content = "  a\n  b\n"
  local result = fr.replace(content, "  a\n  b", "  a\n      b", false)
  eq(result, "  a\n      b\n")
end)

case("whitespace_collapsed", function()
  local result = fr.replace("let   x  =   1;", "let x = 1;", R, false)
  has(result, R)
end)

case("whitespace_substring", function()
  local result = fr.replace("    let   x  =   compute(a,  b);", "compute(a, b)", R, false)
  has(result, R)
end)

case("escaped_newline", function()
  local result = fr.replace('let s = "hello\nworld";', 'let s = "hello\\nworld";', R, false)
  has(result, R)
end)

case("escaped_tab", function()
  local result = fr.replace("col1\tcol2\tcol3", "col1\\tcol2\\tcol3", R, false)
  has(result, R)
end)

case("block_anchor_fuzzy_middle", function()
  local result = fr.replace(
    "fn test() {\n    let x = 1;\n    let y = 2;\n}",
    "fn test() {\n    let x = 99;\n    let y = 2;\n}",
    R,
    false
  )
  has(result, R)
end)

case("context_aware_partial_middle", function()
  local result = fr.replace(
    "fn h() {\n    validate();\n    process();\n    save();\n    respond();\n}",
    "fn h() {\n    validate();\n    WRONG();\n    save();\n    respond();\n}",
    R,
    false
  )
  has(result, R)
end)

case("no_match", function()
  local result, err = fr.replace("fn foo() {}", "MISSING", "x", false)
  eq(result, nil)
  eq(err, NO_MATCH)
end)

case("ambiguous_multiple_matches", function()
  local result, err = fr.replace("let x = 1;\nlet x = 1;", "let x = 1;", "x", false)
  eq(result, nil)
  eq(err, MULTIPLE_MATCHES)
end)

case("block_anchor_picks_best_among_multiple", function()
  local content = "fn a() {\n    unrelated();\n}\nfn a() {\n    target();\n}"
  local result = fr.replace(content, "fn a() {\n    target();\n}", R, false)
  has(result, R)
  has(result, "unrelated()")
end)

case("leading_whitespace_disambiguates", function()
  local result = fr.replace("fn foo() {}\n  fn foo() {}", "  fn foo() {}", R, false)
  eq(result:sub(1, 11), "fn foo() {}")
  has(result, R)
end)

case("strip_common_indent_skips_blank_lines", function()
  local result = fr.replace("    a\n\n    b", "a\n\nb", R, false)
  has(result, R)
end)

case("block_anchor_no_panic_short_content", function()
  local search = "fn test() {\n    body();\n}"
  for _, content in ipairs({
    "aaa\nbbb\nccc\nfn test() {",
    "fn test() {",
    "fn test() {\n}",
  }) do
    local result, err = fr.replace(content, search, "x", false)
    eq(result, nil)
  end
end)

case("escape_normalized_also_fixes_new_string", function()
  local content = 'print("hello")'
  local old = 'print(\\"hello\\")'
  local new = 'print(\\"world\\")'
  local result = fr.replace(content, old, new, false)
  eq(result, 'print("world")')
end)

case("escape_normalized_new_string_with_replace_all", function()
  local content = 'say("a")\nsay("b")'
  local old = 'say(\\"a\\")'
  local new = 'say(\\"x\\")'
  local result = fr.replace(content, old, new, true)
  eq(result, 'say("x")\nsay("b")')
end)

case("replace_all_replaces_every_occurrence", function()
  local result = fr.replace("aXbXc", "X", "Y", true)
  eq(result, "aYbYc")
end)

case("empty_content_no_match", function()
  local result, err = fr.replace("", "x", "y", false)
  eq(result, nil)
  eq(err, NO_MATCH)
end)

-- An old_string that trims down to nothing used to match at every offset, so
-- replace_all spliced forever. A regression hangs here instead of failing.
case("degenerate_old_string_is_rejected", function()
  local vectors = {
    { "abc", "", false, EMPTY_OLD_STRING },
    { "abc", "", true, EMPTY_OLD_STRING },
    { "a\n\nb", "   ", true, NO_MATCH },
  }
  for _, v in ipairs(vectors) do
    local content, old, replace_all, expected_err = table.unpack(v)
    local msg = ("old_string=%q replace_all=%s"):format(old, tostring(replace_all))
    local result, err = fr.replace(content, old, "x", replace_all)
    eq(result, nil, msg)
    eq(err, expected_err, msg)
  end
end)

case("replace_all_no_occurrences", function()
  local result, err = fr.replace("abc", "xyz", "y", true)
  eq(result, nil)
  eq(err, NO_MATCH)
end)

case("replace_all_fuzzy_whitespace", function()
  local result = fr.replace("let  x = 1;\nlet  x = 1;", "let x = 1;", "let x = 2;", true)
  eq(result, "let x = 2;\nlet x = 2;")
end)

case("replace_all_multiline_repeated_block", function()
  local content = "fn f() {\n    a();\n}\nfn f() {\n    a();\n}"
  local result = fr.replace(content, "fn f() {\n    a();\n}", "fn g() {}", true)
  eq(result, "fn g() {}\nfn g() {}")
end)

case("lua_pattern_special_chars", function()
  local content = "assert(x % 2 == 0);\nfoo(a+b).bar;"
  local result = fr.replace(content, "assert(x % 2 == 0);", R, false)
  has(result, R)
  has(result, "foo(a+b).bar;")
end)

case("double_backslash_literal", function()
  local content = "path\\name"
  local result = fr.replace(content, "path\\\\name", R, false)
  has(result, R)
end)

case("replace_all_overlapping_patterns", function()
  local result = fr.replace("aaa", "aa", "b", true)
  eq(result, "ba")
end)

case("exact_match_wins_over_fuzzy", function()
  local content = "let x = 1;\nlet  x = 1;"
  local result = fr.replace(content, "let x = 1;", R, false)
  eq(result, R .. "\nlet  x = 1;")
end)

case("cjk_exact_match", function()
  local content = "// こんにちは世界\n// hello"
  local result = fr.replace(content, "// こんにちは世界", R, false)
  has(result, R)
  has(result, "hello")
end)

local replace_lines = require("edit_helpers").replace_lines
local insert_after = require("edit_helpers").insert_after

case("replace_lines_range_replace_and_delete", function()
  local content = "aaa\nbbb\nccc\nddd\neee\n"

  local r1 = replace_lines(content, 2, 4, "XXX\nYYY")
  eq(r1, "aaa\nXXX\nYYY\neee\n")

  local r2 = replace_lines(content, 3, 3, "ZZZ")
  eq(r2, "aaa\nbbb\nZZZ\nddd\neee\n")

  local r3 = replace_lines(content, 2, 3, "")
  eq(r3, "aaa\nddd\neee\n")

  local _, e1 = replace_lines(content, 0, 1, "x")
  has(e1, "out of range")
  local _, e2 = replace_lines(content, 2, 6, "x")
  has(e2, "out of range")
  local _, e3 = replace_lines(content, 3, 2, "x")
  has(e3, "out of range")
end)

case("replace_lines_insert_mode", function()
  local content = "aaa\nbbb\nccc\n"

  local r1 = replace_lines(content, 1, nil, "ZZZ")
  eq(r1, "ZZZ\naaa\nbbb\nccc\n")

  local r2 = replace_lines(content, 2, nil, "XXX\nYYY")
  eq(r2, "aaa\nXXX\nYYY\nbbb\nccc\n")

  local r3 = replace_lines(content, 4, nil, "END")
  eq(r3, "aaa\nbbb\nccc\nEND\n")

  local _, e1 = replace_lines(content, 0, nil, "x")
  has(e1, "out of range")
  local _, e2 = replace_lines(content, 5, nil, "x")
  has(e2, "out of range")

  local r4 = replace_lines(content, 2, nil, "")
  eq(r4, "aaa\n\nbbb\nccc\n")
end)

case("insert_after_mode", function()
  local content = "aaa\nbbb\nccc\n"

  local r0 = insert_after(content, 0, "TOP")
  eq(r0, "TOP\naaa\nbbb\nccc\n")

  local r2 = insert_after(content, 2, "XXX\nYYY")
  eq(r2, "aaa\nbbb\nXXX\nYYY\nccc\n")

  local r3 = insert_after(content, 3, "END")
  eq(r3, "aaa\nbbb\nccc\nEND\n")

  local _, e1 = insert_after(content, -1, "x")
  has(e1, "out of range")
  local _, e2 = insert_after(content, 4, "x")
  has(e2, "out of range")
end)

th.report()
