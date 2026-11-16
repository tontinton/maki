-- Cases that depend on how an extension maps to a grammar rather than on what
-- one grammar extracts, so they span languages and belong outside tests/lang.

local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local indexer = require("indexer")
local case = th.case
local eq = th.eq
local has = helpers.has

case("routing_tsx_and_jsx_extensions_use_the_tsx_grammar", function()
  eq(indexer.EXT_TO_LANG.tsx[1], "tsx")
  eq(indexer.EXT_TO_LANG.jsx[1], "tsx")
end)

-- `.v` is shared with systemverilog. V wins when it parses, and the HDL file
-- falls through to systemverilog when V's grammar rejects it.
case("routing_v_extension_falls_back_to_systemverilog", function()
  local src = "module counter(input clk, output reg q);\nalways @(posedge clk) q <= ~q;\nendmodule\n"
  has(indexer.index_source(src, indexer.EXT_TO_LANG["v"]), { "module counter" })
end)

case("routing_v_extension_keeps_v_sources", function()
  local src = "fn main() {\n\tprintln('hi')\n}\n"
  has(indexer.index_source(src, indexer.EXT_TO_LANG["v"]), { "main()" })
end)

-- V refuses any tree with errors, so a `.v` file V cannot read must still reach
-- systemverilog's partial index rather than being reported as unindexable.
case("routing_v_extension_returns_partial_index_the_first_grammar_refused", function()
  local src = "module counter(input clk);\nalways @(posedge clk) q <= ~q;\nendmodule\n@@@\n"
  has(
    indexer.index_source(src, indexer.EXT_TO_LANG["v"]),
    { indexer.PARTIAL_INDEX_NOTE, "module counter", "input clk" }
  )
end)
