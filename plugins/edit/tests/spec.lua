local fr = require("maki.fuzzy_replace")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

local function has(s, substr, msg)
  if not s:find(substr, 1, true) then
    error((msg or "") .. "\nexpected to contain: " .. tostring(substr) .. "\n  actual: " .. tostring(s))
  end
end

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

case("empty_old_string", function()
  local result, err = fr.replace("abc", "", "x", false)
  eq(result, nil)
  eq(err, EMPTY_OLD_STRING)
end)

case("empty_old_string_replace_all_does_not_hang", function()
  local result, err = fr.replace("abc", "", "x", true)
  eq(result, nil)
  eq(err, EMPTY_OLD_STRING)
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

case("tabs_vs_spaces_indentation", function()
  local content = "\tfn f() {\n\t\tbar();\n\t}"
  local search = "    fn f() {\n        bar();\n    }"
  local result = fr.replace(content, search, R, false)
  has(result, R)
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

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
