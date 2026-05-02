-- Indexer plugin spec orchestrator.
--
-- Per-language cases live in tests/lang/<lang>.lua and run as side effects of
-- require. Add a new language by creating tests/lang/<lang>.lua (use any
-- existing file as a template) and adding a require line below — keep them
-- alphabetized. Each spec uses the shared `case` helper from tests/helpers.lua,
-- which collects failures so a single broken case does not abort the suite.
require("tests.lang.bash")
require("tests.lang.c")
require("tests.lang.c_sharp")
require("tests.lang.cpp")
require("tests.lang.elixir")
require("tests.lang.go")
require("tests.lang.java")
require("tests.lang.kotlin")
require("tests.lang.lua_lang")
require("tests.lang.markdown")
require("tests.lang.php")
require("tests.lang.python")
require("tests.lang.ruby")
require("tests.lang.rust")
require("tests.lang.scala")
require("tests.lang.swift")
require("tests.lang.typescript")

require("tests.helpers").report()
