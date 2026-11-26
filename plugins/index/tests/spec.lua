-- Indexer plugin spec orchestrator.
--
-- Per-language cases live in tests/lang/<lang>.lua and run as side effects of
-- require. Add a new language by creating tests/lang/<lang>.lua (use any
-- existing file as a template) and adding a require line below — keep them
-- alphabetized.

local indexer = require("indexer")
local th = require("maki.test_helpers")
local case = th.case
local eq = th.eq
local mktmpdir = function()
  return th.mktmpdir("index_spec")
end
local rmtree = th.rmtree

local dir_listing = require("maki.dir_listing")

local function mock_ctx()
  return {
    is_instruction_file = function(_, name)
      local set = { ["AGENTS.md"] = true, ["CLAUDE.md"] = true, ["COPILOT.md"] = true }
      return set[name] or false
    end,
    load_instructions = function()
      return true
    end,
  }
end

-- integration: directory listing via real filesystem

case("dir_listing_sort_and_filter", function()
  local tmpdir = mktmpdir()
  maki.fs.write(maki.fs.joinpath(tmpdir, "c.txt"), "")
  maki.fs.write(maki.fs.joinpath(tmpdir, "a.txt"), "")
  maki.fs.write(maki.fs.joinpath(tmpdir, "AGENTS.md"), "instructions")
  maki.fs.write(maki.fs.joinpath(tmpdir, "b.txt"), "")
  maki.fs.write(maki.fs.joinpath(tmpdir, "m.txt"), "")
  maki.fs.mkdir(maki.fs.joinpath(tmpdir, "zdir"))
  maki.fs.mkdir(maki.fs.joinpath(tmpdir, "adir"))
  maki.fs.mkdir(maki.fs.joinpath(tmpdir, "idir"))

  local listing, err = dir_listing.list(tmpdir, mock_ctx())
  assert(err == nil, "dir listing should succeed: " .. tostring(err))
  eq(#listing.names, 7)
  eq(listing.names[1], "adir/")
  eq(listing.names[2], "idir/")
  eq(listing.names[3], "zdir/")
  eq(listing.names[4], "a.txt")
  eq(listing.names[5], "b.txt")
  eq(listing.names[6], "c.txt")
  eq(listing.names[7], "m.txt")
  rmtree(tmpdir)
end)
require("tests.indexer_core")
require("tests.routing")
require("tests.lang.bash")
require("tests.lang.bazel")
require("tests.lang.c")
require("tests.lang.c_sharp")
require("tests.lang.clojure")
require("tests.lang.containerfile")
require("tests.lang.cpp")
require("tests.lang.css")
require("tests.lang.dart")
require("tests.lang.elixir")
require("tests.lang.gleam")
require("tests.lang.go")
require("tests.lang.hcl")
require("tests.lang.html")
require("tests.lang.java")
require("tests.lang.json")
require("tests.lang.kotlin")
require("tests.lang.lua_lang")
require("tests.lang.make")
require("tests.lang.markdown")
require("tests.lang.nix")
require("tests.lang.php")
require("tests.lang.python")
require("tests.lang.ruby")
require("tests.lang.rust")
require("tests.lang.scala")
require("tests.lang.sql")
require("tests.lang.swift")
require("tests.lang.systemverilog")
require("tests.lang.toml")
require("tests.lang.tsx")
require("tests.lang.typescript")
require("tests.lang.v")
require("tests.lang.vhdl")
require("tests.lang.yaml")
require("tests.lang.zig")

-- Every language the indexer routes an extension to must have a parser on the
-- Rust side. `.sv` shipped registered in language.rs but absent from the tables
-- here, so the tool refused those files; this pins the two layers together.
case("every_routed_language_has_a_parser", function()
  local names = {}
  local function add(name)
    names[name] = true
  end
  for _, langs in pairs(indexer.EXT_TO_LANG) do
    for _, lang in ipairs(langs) do
      add(lang)
    end
  end
  for _, lang in pairs(indexer.FILENAME_TO_LANG) do
    add(lang)
  end
  for name in pairs(names) do
    local skeleton, err = indexer.index_source("", name)
    assert(skeleton ~= nil, name .. ": " .. tostring(err))
  end
end)

th.report()
