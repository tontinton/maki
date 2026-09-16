local Helpers = require("automode_helpers")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

case("go_ahead_accepts_a_bare_approval", function()
  eq(Helpers.is_go_ahead("yes"), true)
  eq(Helpers.is_go_ahead("go ahead"), true)
  eq(Helpers.is_go_ahead("Try again"), true, "matching is case insensitive")
  eq(Helpers.is_go_ahead("  ok  "), true, "surrounding whitespace is not part of the answer")
end)

case("go_ahead_rejects_a_refusal_that_contains_an_approval", function()
  eq(Helpers.is_go_ahead("no, try again later"), false, "the leading refusal wins over `try again`")
  eq(Helpers.is_go_ahead("don't do it"), false)
  eq(Helpers.is_go_ahead("wait, continue tomorrow"), false)
end)

-- "nope" and "nothing" start with "no" but are not the word; the frontier
-- pattern is what keeps them out of the refusal list.
case("go_ahead_only_refuses_on_the_whole_word_no", function()
  eq(Helpers.is_go_ahead("nothing else, proceed"), true)
end)

case("go_ahead_ignores_an_empty_message", function()
  eq(Helpers.is_go_ahead(""), false)
  eq(Helpers.is_go_ahead(nil), false)
  eq(Helpers.is_go_ahead("   "), false)
end)

-- A long message is new work, and new work has to be judged on its merits
-- even when the user happened to write "ok" somewhere inside it.
case("go_ahead_ignores_a_long_message", function()
  eq(Helpers.is_go_ahead("ok " .. string.rep("x", 240)), false)
  eq(Helpers.is_go_ahead("ok " .. string.rep("x", 200)), true, "just under the limit still counts")
end)

case("go_ahead_rejects_an_unrelated_message", function()
  eq(Helpers.is_go_ahead("add a test for the parser"), false)
end)

case("executable_of_takes_the_program_from_the_first_scope", function()
  eq(Helpers.executable_of({ "rm -rf build", "ls" }), "rm")
  eq(Helpers.executable_of({ "  git push origin main" }), "git", "leading whitespace is skipped")
end)

case("executable_of_has_nothing_to_say_about_a_scopeless_call", function()
  eq(Helpers.executable_of(nil), nil)
  eq(Helpers.executable_of({}), nil)
  eq(Helpers.executable_of({ 42 }), nil, "a non-string scope is not a command")
end)

case("chain_parsing_splits_on_commas", function()
  local specs = Helpers.parse_chain("a/one,b/two")
  eq(#specs, 2)
  eq(specs[1], "a/one")
  eq(specs[2], "b/two")
end)

case("chain_parsing_trims_and_drops_empty_entries", function()
  local specs = Helpers.parse_chain(" a/one , , b/two ,")
  eq(#specs, 2, "blank entries are typos, not links")
  eq(specs[1], "a/one")
  eq(specs[2], "b/two")
end)

-- The default: no chain means automode registers nothing and stays inert.
case("chain_parsing_of_nothing_is_an_empty_chain", function()
  eq(#Helpers.parse_chain(""), 0)
  eq(#Helpers.parse_chain(nil), 0)
  eq(#Helpers.parse_chain("   "), 0)
end)

th.report()
