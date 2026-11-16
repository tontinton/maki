local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

local ALL_SECTIONS = [==[
import pkg::*;

package pkg;
  parameter int N = 4;
  typedef enum logic [1:0] { IDLE, RUN } state_t;
endpackage

interface bus_if #(parameter W = 8) (input logic clk);
  logic [W-1:0] data;
  modport master (output data, input clk);
endinterface

module top #(parameter W = 8) (input logic clk, output logic [W-1:0] q);
  logic [W-1:0] tmp;
  function automatic int add(int a, int b);
    return a + b;
  endfunction
  task automatic drive(input logic [7:0] v);
    tmp <= v;
  endtask
  generate
    for (genvar i = 0; i < 4; i++) begin : gen_loop
      logic x;
    end
  endgenerate
endmodule

class Base extends Parent;
  int x;
endclass
]==]

case("systemverilog_all_sections", function()
  has(idx(ALL_SECTIONS, "systemverilog"), {
    "imports:",
    "pkg",
    "mod:",
    "package pkg",
    "consts:",
    "N",
    "W",
    "blocks:",
    "generate gen_loop",
    "types:",
    "typedef state_t = enum logic [1:0]",
    "IDLE",
    "RUN",
    "interface bus_if",
    "logic [W-1:0] data",
    "modport master (output data, input clk)",
    "classes:",
    "module top",
    "class Base extends Parent",
    "int x",
    "fns:",
    "add(int a, int b)",
    "drive(input logic [7:0] v)",
  })
end)

-- Verilog is a subset of the grammar, so a plain module still indexes.
case("systemverilog_indexes_plain_verilog", function()
  has(idx("module counter(input clk, output reg q);\nalways @(posedge clk) q <= ~q;\nendmodule\n", "systemverilog"), {
    "module counter",
  })
end)

-- `extern` outside a class breaks the grammar's recovery: declarations before
-- it are dropped from the tree. Pinned so a grammar bump that fixes this is
-- visible, at which point `logic x` starts appearing again.
case("systemverilog_extern_outside_class_degrades", function()
  local out = idx(
    "module m;\n  extern function void f();\n  logic x;\n  function int g();\n  endfunction\nendmodule\n",
    "systemverilog"
  )
  has(out, { "module m", "f()" })
  lacks(out, { "logic x" })
end)

-- The same prototype inside a class parses cleanly.
case("systemverilog_extern_inside_class_parses", function()
  has(idx("class C;\n  extern function void f();\n  int x;\nendclass\n", "systemverilog"), {
    "class C",
  })
end)

-- A class wraps each member in `class_item > class_property`, unlike modules
-- and interfaces, whose declarations sit directly on the container.
case("systemverilog_class_members", function()
  has(idx("class C;\n  int a;\n  string b;\nendclass\n", "systemverilog"), {
    "class C",
    "int a",
    "string b",
  })
end)

-- A class body holds methods, typedefs and parameters as well as fields, each
-- behind a different wrapper node, so the whole body is walked.
case("systemverilog_class_body", function()
  local src = [==[
class Base extends Parent;
  typedef enum logic [1:0] { A, B } e_t;
  function int f(input int a);
  endfunction
  task t(int b);
  endtask
  extern function void ext();
  pure virtual function void v();
  parameter int P = 1;
endclass
]==]
  has(idx(src, "systemverilog"), {
    "class Base extends Parent",
    "typedef e_t = enum logic [1:0]",
    "f(input int a)",
    "t(int b)",
    "ext()",
    "v()",
    "P",
  })
end)

-- Struct and union members are `struct_union_member` siblings of the
-- `struct_union` marker, not data declarations nested inside it.
case("systemverilog_struct_and_union_members", function()
  has(idx("typedef struct packed { int a; logic b; } p_t;\n", "systemverilog"), {
    "typedef p_t = struct packed",
    "int a",
    "logic b",
  })
  has(idx("typedef union tagged { int a; logic b; } u_t;\n", "systemverilog"), {
    "typedef u_t = union tagged",
    "int a",
    "logic b",
  })
end)

case("systemverilog_class_members_truncate", function()
  local lines = { "class C;" }
  for i = 1, 9 do
    lines[#lines + 1] = "  int s" .. i .. ";"
  end
  lines[#lines + 1] = "endclass"
  local out = idx(table.concat(lines, "\n") .. "\n", "systemverilog")
  has(out, { "int s8", "1 more truncated]" })
  -- The ninth field still reaches the `consts:` section; only the class's own
  -- child list is truncated, so the cut-off is asserted past the class header.
  lacks(out:sub(out:find("class C", 1, true)), { "int s9" })
end)
