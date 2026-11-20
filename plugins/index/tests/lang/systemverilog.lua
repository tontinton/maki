local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local eq = th.eq
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

local ALL_SECTIONS = [==[
import pkg::*;

parameter int GLOBAL_N = 2;

package pkg;
  parameter int N = 4;
  typedef enum logic [1:0] { IDLE, RUN } state_t;
  function int pkg_add(int a, int b);
    return a + b;
  endfunction
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
    -- Only a file-scope parameter keeps an entry; a unit's own parameters nest
    -- under it, so `N` and `W` are asserted with the units below.
    "consts:",
    "parameter int GLOBAL_N = 2",
    "units:",
    -- A package is a design unit and renders under `units:` like a module, so
    -- its constants, types and functions nest under it rather than claiming
    -- file scope they do not have.
    "package pkg",
    "parameter int N = 4",
    "typedef state_t = enum logic [1:0]",
    "IDLE",
    "RUN",
    "interface bus_if",
    "logic [W-1:0] data",
    "modport master (output data, input clk) [15]",
    -- A module is a design unit and renders under `units:`, matching VHDL's
    -- entity; `classes:` holds only the class.
    "module top",
    "class Base extends Parent",
    "int x",
    -- A function owned by a unit is part of it, so it renders as a child.
    "add(int a, int b)",
    "drive(input logic [7:0] v)",
    "pkg_add(int a, int b)",
    -- A bare `generate ... endgenerate` belongs to the unit that holds it, so
    -- its blocks nest rather than claiming a section of their own.
    "gen_loop : for generate",
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
  local out = idx(src, "systemverilog")
  has(out, {
    "class Base extends Parent",
    "f(input int a)",
    "function void ext()",
    "function void v()",
    "parameter int P = 1",
  })
  -- A type a class declares for itself is class detail, not a global name, so
  -- it sits indented under the class rather than as a top-level entry.
  assert(out:find("\n    typedef e_t", 1, true), "typedef should nest under its class:\n" .. out)
end)

-- A module lists its members and is also walked as a container, so a typedef
-- can be reached twice. It must appear once, under the module it belongs to.
case("systemverilog_module_typedef_is_listed_once", function()
  local out = idx("module m;\n  typedef struct packed { logic a; } m_t;\nendmodule\n", "systemverilog")
  assert(select(2, out:gsub("typedef m_t", "")) == 1, "typedef should be listed once:\n" .. out)
  assert(out:find("\n    typedef m_t", 1, true), "typedef should nest under its module:\n" .. out)
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

case("systemverilog_instantiations", function()
  has(idx("module top;\n  logic x;\n  sub u1 (.a(x));\n  sub2 #(.W(8)) u2 (.d(x));\nendmodule\n", "systemverilog"), {
    "u1 : instance sub",
    "u2 : instance sub2",
  })
end)

case("systemverilog_assertions_and_coverage", function()
  local src = [==[
module m(input logic clk, a, b);
  property p_check;
    @(posedge clk) a |-> b;
  endproperty
  assert property (p_check);
  covergroup cg @(posedge clk);
    cp: coverpoint a;
  endgroup
  sequence s1;
    a ##1 b;
  endsequence
endmodule
]==]
  local out = idx(src, "systemverilog")
  has(out, { "property p_check", "covergroup cg", "sequence s1" })
  -- An `assert property` site only names a property declared elsewhere, so it
  -- would duplicate that entry and crowd `rules:` in verification code.
  lacks(out, { "assert property" })
end)

-- An unlabelled `begin`/`end` names nothing, so it gets no entry of its own.
case("systemverilog_unlabelled_begin_end_is_not_an_entry", function()
  local src = "module m(input logic clk, a);\n  always_ff @(posedge clk) q <= a;\n  initial begin end\nendmodule\n"
  lacks(idx(src, "systemverilog"), { "block [" })
end)

-- A parameter belongs to the unit that declares it, like a VHDL generic under
-- its entity, so it nests instead of floating in a file-scope `consts:`.
case("systemverilog_parameter_kinds", function()
  -- The header parameter is on line 1, the ports on 2 and 3, and the body after
  -- them. A list emitted per kind would put the ports first and the parameter
  -- last, which is the order a reader would have to undo.
  local src = "module m #(parameter int W = 8) (\n  input logic c,\n  output logic d\n);\n"
    .. "  wire a;\n  localparam int L = 3;\n  wire b;\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "parameter int W = 8 [1]", "input logic c [2]", "output logic d [3]", "wire a [5]" })
  lacks(out, { "consts:" })
  assert(
    out:find("parameter int W", 1, true) < out:find("input logic c", 1, true),
    "the parameter comes first:\n" .. out
  )
  assert(out:find("input logic c", 1, true) < out:find("wire a", 1, true), "the ports come before the body:\n" .. out)
  assert(out:find("wire a", 1, true) < out:find("localparam int L", 1, true), "the wire comes first:\n" .. out)
  assert(out:find("localparam int L", 1, true) < out:find("wire b", 1, true), "the parameter comes first:\n" .. out)
end)

-- Attributes render above the declaration they describe, with their delimiters:
-- without them the line reads as a declaration. Only synthesis and CDC intent
-- survives, so a vendor IP's X_INTERFACE_* lines stay out.
-- An attribute annotates the declarations around it rather than being a member
-- of the body, so it holds a budget of its own: a signal-heavy module cannot
-- crowd attributes out, and a file that marks every signal cannot crowd out the
-- signals. Eight is the cap, which every hand-written design stays under.
-- A package exports its constants the way a VHDL package does, so they get the
-- package budget rather than the module parameter cap: a package holding twenty
-- lists all twenty, where sixteen would cut four of them.
case("systemverilog_package_constants_use_the_package_budget", function()
  local params = {}
  for i = 1, 20 do
    params[#params + 1] = "  parameter int P" .. i .. " = " .. i .. ";"
  end
  local out = idx("package p;\n" .. table.concat(params, "\n") .. "\nendpackage\n", "systemverilog")
  has(out, { "parameter int P20 = 20" })
  lacks(out, { "more truncated]" })
end)

case("systemverilog_attributes_hold_a_budget_of_their_own", function()
  local body = {}
  -- Twelve wires overflow a cap of eight on their own. They come first, so a
  -- shared bucket would be full before the attributes were reached; all six
  -- surviving is only possible when the two lists have separate budgets.
  for i = 1, 12 do
    body[#body + 1] = "  wire q" .. i .. ";"
  end
  for i = 1, 6 do
    body[#body + 1] = '  (* keep = "true" *)'
    body[#body + 1] = "  wire a" .. i .. ";"
  end
  local out = idx("module m;\n" .. table.concat(body, "\n") .. "\nendmodule\n", "systemverilog")
  local kept = select(2, out:gsub("keep", ""))
  eq(kept, 6, "every attribute survives the wire flood:\n" .. out)
  has(out, { "[10 nets truncated]" })
end)

case("systemverilog_attributes_cross_their_member_wrapper", function()
  has(idx('class C;\n  (* keep = "true" *)\n  logic x;\nendclass\n', "systemverilog"), {
    '(* keep = "true" *) [2]',
    "logic x [3]",
  })
  has(idx('package p;\n  (* dont_touch = "true" *)\n  logic y;\nendpackage\n', "systemverilog"), {
    '(* dont_touch = "true" *) [2]',
    "logic y [3]",
  })
  -- A non-ANSI port's attributes are not indexed: the attribute is a child of
  -- the declaration, so it is cut from the line rather than rendered.
  local port = idx('module m (a);\n  (* keep = "true" *)\n  input wire a;\nendmodule\n', "systemverilog")
  has(port, { "input wire a [2-3]" })
  lacks(port, { "keep" })
  -- A body declaration in a non-ANSI module sits behind `module_item`, and its
  -- attribute is kept.
  has(idx('module m (a);\n  (* keep = "true" *)\n  wire w;\nendmodule\n', "systemverilog"), {
    '(* keep = "true" *) [2]',
    "wire w [3]",
  })
end)

-- A class nests under the container that declares it. A package wraps one in
-- `package_item`, where a module declares it directly, so both levels are found.
case("systemverilog_class_nests_directly_under_its_container", function()
  has(idx("module m;\n  class Direct;\n    int a;\n  endclass\nendmodule\n", "systemverilog"), {
    "class Direct",
    "int a",
  })
  has(idx("package p;\n  class Wrapped;\n    int b;\n  endclass\nendpackage\n", "systemverilog"), {
    "class Wrapped",
    "int b",
  })
end)

-- A generate body declares nets and variables the way a module does, and its
-- attributes are kept like the module's.
case("systemverilog_generate_body_keeps_its_declarations_and_attributes", function()
  local out = idx(
    'module m;\n  if (1) begin : g\n    (* dont_touch = "true" *)\n    wire a;\n'
      .. "    localparam int L = 1;\n"
      .. '    (* keep = "true" *)\n    logic b;\n  end\nendmodule\n',
    "systemverilog"
  )
  has(out, {
    '(* dont_touch = "true" *) [3]',
    "wire a [4]",
    "localparam int L = 1 [5]",
    '(* keep = "true" *) [6]',
    "logic b [7]",
  })
end)

-- A parameter is a declaration like any other, so an attribute above it belongs
-- to it rather than being dropped.
case("systemverilog_parameter_keeps_its_attribute", function()
  local out = idx('module m;\n  (* keep = "true" *)\n  localparam int X = 2;\nendmodule\n', "systemverilog")
  has(out, { '(* keep = "true" *) [2]', "localparam int X = 2 [3]" })
end)

-- A nested generate's blocks land where they sit, not after every sibling of
-- the level above.
case("systemverilog_generate_blocks_keep_source_order", function()
  local out = idx(
    "module m(input logic clk);\n  generate\n    if (1) begin : g_if\n      sub u1 (.clk(clk));\n    end\n"
      .. "    for (genvar i = 0; i < 2; i++) begin : g_loop\n      sub u2 (.clk(clk));\n    end\n"
      .. "  endgenerate\nendmodule\n",
    "systemverilog"
  )
  assert(out:find("g_if", 1, true) < out:find("g_loop", 1, true), "generates should keep source order:\n" .. out)
end)

-- Members print in the order the file declares them, not grouped by category.
case("systemverilog_members_keep_source_order", function()
  local out =
    idx("class C;\n  typedef int t1;\n  function int f(); return 1; endfunction\n  int x;\nendclass\n", "systemverilog")
  assert(out:find("typedef", 1, true) < out:find("int f()", 1, true), "typedef comes first:\n" .. out)
  assert(out:find("int f()", 1, true) < out:find("int x", 1, true), "the function comes before the field:\n" .. out)
end)

case("systemverilog_attributes_are_kept_above_their_declaration", function()
  local src = "module m(input logic clk);\n"
    .. '  (* mark_debug = "true" *)\n  (* keep = "true" *)\n  wire [7:0] dbg;\n'
    .. '  (* ASYNC_REG = "TRUE" *)\n  logic s1;\n'
    .. '  (* X_INTERFACE_INFO = "vendor" *)\n  logic v1;\nendmodule\n'
  local out = idx(src, "systemverilog")
  has(out, { '(* mark_debug = "true" *) [2]', '(* keep = "true" *) [3]', "wire [7:0] dbg [4]" })
  has(out, { '(* ASYNC_REG = "TRUE" *) [5]' })
  lacks(out, { "X_INTERFACE_INFO" })
  local attr_indent = out:match("( *)%(%* keep")
  local wire_indent = out:match("( *)wire %[7:0%]")
  assert(
    attr_indent and wire_indent and #attr_indent == #wire_indent,
    "the attribute aligns with its declaration:\n" .. out
  )
end)

-- A case statement is raised because it carries the state machine or the
-- decoder; its branches stay in the source, and `if` is left alone because every
-- always block has one for its clock edge.
case("systemverilog_case_headers_are_raised_in_every_body", function()
  local src = "module m(input logic clk, input logic [1:0] sel, output logic q);\n"
    .. "  always_comb begin\n    if (en) begin\n      case (sel)\n        2'b00: q = 0;\n"
    .. "        default: q = 1;\n      endcase\n    end\n  end\nendmodule\n"
    .. "class C;\n  function int f(int x);\n    case (x)\n      0: return 1;\n"
    .. "      default: return 2;\n    endcase\n  endfunction\nendclass\n"
  local out = idx(src, "systemverilog")
  has(out, { "case sel [4-7]", "case x [13-16]" })
  has(
    idx(
      "module m(input logic [1:0] s);\n  always_comb begin\n    case(s)\n      0: q = 0;\n"
        .. "    endcase\n  end\nendmodule\n",
      "systemverilog"
    ),
    { "case s [3-5]" }
  )
  lacks(out, { "2'b00", "default:" })
end)

-- `timeunit` is declared inside the unit, unlike the file-scope `timescale`, so
-- it nests with the parameters however the statement spells its two literals.
case("systemverilog_timeunit_nests_under_its_module", function()
  local out = idx(
    "module a;\n  timeunit 10ns;\n  timeprecision 100ps;\nendmodule\n"
      .. "module b;\n  timeunit 1 us / 10 ns;\nendmodule\n",
    "systemverilog"
  )
  has(out, { "timeunit 10ns/100ps [2-3]", "timeunit 1us/10ns [6]" })
end)

-- Ports live in the `*_header` child, which the member walk does not enter.
case("systemverilog_module_ports", function()
  has(idx("module m(input logic clk, output logic [7:0] q);\nendmodule\n", "systemverilog"), {
    "module m",
    "input logic clk",
    "output logic [7:0] q",
  })
  has(idx("interface bus(input logic clk);\n  logic a;\nendinterface\n", "systemverilog"), {
    "interface bus",
    "input logic clk",
  })
end)

-- A field is listed under its container; it is not repeated as a `consts:`
-- entry. A typedef is not a field, so it still reaches `types:`.
case("systemverilog_fields_are_listed_once", function()
  local function count(text, needle)
    local n = 0
    for _ in text:gmatch(needle) do
      n = n + 1
    end
    return n
  end
  eq(count(idx("class C;\n  int x;\nendclass\n", "systemverilog"), "int x"), 1)
  local typed = idx("class C;\n  typedef logic [7:0] b_t;\n  int x;\nendclass\n", "systemverilog")
  eq(count(typed, "int x"), 1)
end)

case("systemverilog_class_members_truncate", function()
  local lines = { "class C;" }
  for i = 1, 9 do
    lines[#lines + 1] = "  int s" .. i .. ";"
  end
  lines[#lines + 1] = "endclass"
  local out = idx(table.concat(lines, "\n") .. "\n", "systemverilog")
  has(out, { "int s8", "[1 nets truncated]" })
  -- Fields are listed once, under the class, so the ninth appears nowhere.
  lacks(out, { "int s9" })
end)

case("systemverilog_program_and_checker", function()
  has(idx("program automatic p(input logic clk);\n  int x;\n  task t(); endtask\nendprogram\n", "systemverilog"), {
    "program p",
    "int x",
    "input logic clk",
    "t()",
  })
  has(
    idx(
      "checker c(input logic clk, a);\n  logic z;\n  assert property (@(posedge clk) a);\nendchecker\n",
      "systemverilog"
    ),
    {
      "checker c",
      "logic z",
    }
  )
end)

case("systemverilog_module_is_a_design_unit_not_a_class", function()
  local out = idx("class C;\n  int x;\nendclass\nmodule top(input logic clk);\nendmodule\n", "systemverilog")
  local class_idx = out:find("classes:", 1, true)
  local units_idx = out:find("units:", 1, true)
  assert(units_idx and class_idx and units_idx < class_idx, "module should be under units:, got:\n" .. out)
  assert(out:find("class C", 1, true) > class_idx, "class should be under classes:")
  assert(out:find("module top", 1, true) < class_idx, "module should not be under classes:")
end)

-- Ports and members share one cap, so listing ports first keeps the module's
-- interface at the cost of its internal signals.
case("systemverilog_ports_survive_truncation", function()
  local members = {}
  for i = 1, 9 do
    members[#members + 1] = "  logic w" .. i .. ";"
  end
  local src = "module big(input logic clk, output logic q);\n" .. table.concat(members, "\n") .. "\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "input logic clk", "output logic q", "nets truncated]" })
  lacks(out, { "logic w9" })
end)

-- A continued declarator is its own node carrying only a name, so the direction
-- and width of the declaration it continues say what it is.
case("systemverilog_continued_port_keeps_its_direction", function()
  local src = "module t (\n  input reset, clock,\n  output x, u,\n  input z\n);\nendmodule\n"
  has(
    idx(src, "systemverilog"),
    { "input reset [2]", "input clock [2]", "output x [3]", "output u [3]", "input z [4]" }
  )
end)

-- A non-ANSI module wraps every body item in a `module_item`, and declares its
-- ports as body statements rather than in the header.
case("systemverilog_non_ansi_module_indexes_ports_and_body", function()
  local out = idx("module m(clk, q);\n  input clk;\n  output logic [7:0] q;\n  logic s;\nendmodule\n", "systemverilog")
  has(out, { "module m", "input clk", "output logic [7:0] q", "logic s" })
end)

case("systemverilog_non_ansi_module_keeps_instances", function()
  local out = idx("module top(clk);\n  input clk;\n  sub u1(.x(clk));\nendmodule\n", "systemverilog")
  has(out, { "u1 : instance sub" })
end)

-- A function belongs to the construct that owns it, so it nests under that
-- entry; a package function is the package's content like a module's is the
-- module's, so it nests too.
case("systemverilog_module_functions_nest_under_the_module", function()
  local out =
    idx("module m(input logic clk);\n  function int f(int x); return x; endfunction\nendmodule\n", "systemverilog")
  lacks(out, { "fns:" })
  has(out, { "module m", "int f(int x)" })
end)

-- A block without a label is still a scope, so its declarations are as real as
-- a labelled block's and belong under the construct that holds them.
case("systemverilog_block_declarations_nest_under_their_construct", function()
  local out = idx(
    "module m();\n"
      .. "  initial begin\n    parameter int P = 4;\n    int q = 1;\n  end\n"
      .. "  initial begin : blk\n    parameter int R = 5;\n  end\n"
      .. "endmodule\n",
    "systemverilog"
  )
  has(out, { "parameter int P = 4", "int q = 1", "parameter int R = 5", "blk : initial" })
  -- The label goes on the construct's own line; an entry for the block itself
  -- would float up to the file's top level.
  assert(not out:find("blk : block", 1, true), "the block must not stand alone:\n" .. out)
  -- The block that names nothing still renders, without a label.
  assert(out:find("\n%s*initial %[", 1) ~= nil, "the bare initial is missing:\n" .. out)
  local module_indent = out:match("( *)module m")
  local param_indent = out:match("( *)parameter int R = 5")
  assert(#param_indent > #module_indent, "the parameter nests inside the module:\n" .. out)
end)

-- `interface class` is a class kind the grammar spells separately, and its
-- members hang off `interface_class_item`, so without its own arm the
-- declaration and every prototype in it vanish.
case("systemverilog_interface_class_is_indexed", function()
  local out = idx(
    "interface class IC #(parameter int W = 8);\n"
      .. "  pure virtual function int get();\n"
      .. "  pure virtual task put(int x);\n"
      .. "  typedef enum {A, B} st;\n"
      .. "endclass\n",
    "systemverilog"
  )
  has(out, { "interface class IC", "parameter int W = 8", "int get()", "put(int x)", "typedef st = enum", "A", "B" })
  assert(not out:find("\n  class IC", 1, true), "only the interface class is listed:\n" .. out)
end)

-- A function's return type separates two overloads and tells a caller what it
-- gets, and VHDL spells it out, so SV does too. A task has no return type and
-- `void` says nothing, so neither is shown.
case("systemverilog_signatures_keep_the_return_type", function()
  local out = idx(
    "module m();\n"
      .. "  function logic [7:0] f(int a); return a; endfunction\n"
      .. "  function void g(); endfunction\n"
      .. "  task t(input int b); endtask\n"
      .. "endmodule\n",
    "systemverilog"
  )
  has(out, { "logic [7:0] f(int a)", "g()", "t(input int b)" })
  lacks(out, { "void g()" })
end)

-- A package's constants and functions are scoped to it, so they nest under the
-- package entry; listing them at file scope would claim visibility they lack.
case("systemverilog_package_members_nest_under_the_package", function()
  local out = idx(
    "package p;\n  parameter int N = 4;\n  function int h(int z); return z; endfunction\nendpackage\n",
    "systemverilog"
  )
  has(out, { "package p", "parameter int N = 4", "int h(int z)" })
  lacks(out, { "fns:", "consts:" })
  local header = out:match("( *)package p")
  local param = out:match("( *)parameter int N = 4")
  local fn = out:match("( *)int h%(int z%)")
  assert(
    header and param and fn and #param > #header and #fn > #header,
    "package members should nest under it:\n" .. out
  )
end)

-- Two packages in one file are legal, so each one's members must stay tied to
-- their own package rather than merging into one file-scope list.
case("systemverilog_packages_scoped_separately", function()
  local out = idx(
    "package a_pkg;\n  parameter int A = 1;\nendpackage\npackage b_pkg;\n  parameter int B = 2;\nendpackage\n",
    "systemverilog"
  )
  has(out, { "package a_pkg", "package b_pkg", "parameter int A = 1", "parameter int B = 2" })
  lacks(out, { "consts:" })
end)

-- The grammar wraps a package import in a `data_declaration`, so it would be
-- read as a variable declaration and lost. Imports stay file-scope: the section
-- merges every import in the file into one trie.
case("systemverilog_package_import_is_not_lost", function()
  local out = idx("package pkg;\n  import other::*;\n  parameter int N = 4;\nendpackage\n", "systemverilog")
  has(out, { "imports:", "other", "package pkg", "parameter int N = 4" })
end)

-- A package exports its members, so its budget is the large one rather than the
-- module-internal eight, and it is held per container and per section.
case("systemverilog_package_members_have_their_own_budget", function()
  local function pkg(name, n)
    local decls = {}
    for i = 1, n do
      decls[#decls + 1] = "  typedef logic [" .. i .. ":0] t" .. i .. "_t;"
    end
    return "package " .. name .. ";\n" .. table.concat(decls, "\n") .. "\nendpackage\n"
  end
  local out = idx(pkg("a", 70) .. "\n" .. pkg("b", 3), "systemverilog")
  has(out, { "package a", "package b", "typedef t32_t", "[38 types truncated]" })
  assert(not out:find("typedef t33_t", 1, true), "t33_t should be truncated:\n" .. out)
  local b = out:match("package b.-$")
  assert(b and b:find("typedef t3_t", 1, true) and not b:find("truncated", 1, true), "b lost members:\n" .. out)
end)

-- Ports and the body are budgeted apart, so a wide interface cannot starve the
-- behaviour and a busy body cannot hide the interface.
case("systemverilog_ports_are_uncapped_while_the_body_is_capped", function()
  local ports = {}
  for i = 1, 12 do
    ports[#ports + 1] = "input logic p" .. i
  end
  local body = {}
  for i = 1, 12 do
    body[#body + 1] = "  function int f" .. i .. "(); return 1; endfunction"
  end
  for i = 1, 12 do
    body[#body + 1] = "  logic s" .. i .. ";"
  end
  local out = idx(
    "module m(" .. table.concat(ports, ", ") .. ");\n" .. table.concat(body, "\n") .. "\nendmodule\n",
    "systemverilog"
  )
  -- A module's interface is what an agent reads first and every port is
  -- distinct, so the port ceiling is far above a hand-written module while the
  -- body still caps at eight.
  has(out, { "input logic p12", "f8()", "logic s8" })
  lacks(out, { "f9()", "logic s9" })
end)

-- A generated IP wrapper has hundreds of config parameters, so the list stops at
-- a ceiling rather than burying the interface.
case("systemverilog_parameter_list_stops_at_the_ceiling", function()
  local params = {}
  for i = 1, 20 do
    params[#params + 1] = "parameter int P" .. i .. " = " .. i
  end
  local out = idx("module m #(" .. table.concat(params, ", ") .. ") (input logic clk);\nendmodule\n", "systemverilog")
  has(out, { "parameter int P16", "[4 consts truncated]" })
  lacks(out, { "parameter int P17" })
end)

-- A module may declare its ports on body statements instead of in the header.
-- Those lines are the interface too, so they must not fall into the eight-member
-- body budget.
case("systemverilog_non_ansi_body_ports_are_not_cut_to_eight", function()
  local names = {}
  for i = 1, 12 do
    names[#names + 1] = "p" .. i
  end
  local body = { "module m(" .. table.concat(names, ", ") .. ");" }
  for i = 1, 12 do
    body[#body + 1] = "  input p" .. i .. ";"
  end
  body[#body + 1] = "endmodule"
  local out = idx(table.concat(body, "\n") .. "\n", "systemverilog")
  has(out, { "input p11", "input p12" })
  lacks(out, { "more truncated" })
end)

-- A bare `generate` region is wrapped differently depending on the module
-- header style: a direct child of an ANSI module, a `module_item` child in a
-- non-ANSI one. It has to nest in both, or suppressing the duplicate entry
-- leaves nothing behind.
case("systemverilog_generate_region_nests_in_both_header_styles", function()
  local ansi =
    "module a(input logic clk);\n  generate\n    if (1) begin : g_a\n      logic x;\n    end\n  endgenerate\nendmodule\n"
  has(idx(ansi, "systemverilog"), { "g_a : if generate", "logic x" })
  local non_ansi = "module b(clk, q);\n  input clk;\n  output q;\n  generate\n"
    .. "    if (1) begin : g_b\n      logic y;\n    end\n  endgenerate\nendmodule\n"
  has(idx(non_ansi, "systemverilog"), { "g_b : if generate", "logic y" })
end)

-- A generated IP wrapper has hundreds of ports, so the interface is listed up
-- to a ceiling rather than either in full or cut to eight.
case("systemverilog_wide_port_list_stops_at_the_ceiling", function()
  local wide = {}
  for i = 1, 70 do
    wide[#wide + 1] = "input logic p" .. i
  end
  local out = idx("module w(" .. table.concat(wide, ", ") .. ");\nendmodule\n", "systemverilog")
  has(out, { "input logic p64", "[6 ports truncated]" })
  lacks(out, { "input logic p65" })
end)

-- A procedural block is this language's process, so it nests under the module
-- the way VHDL's process nests under its architecture.
case("systemverilog_procedural_blocks_nest_under_the_module", function()
  local src = "module m(input logic clk);\n  logic a;\n  always_ff @(posedge clk) a <= 1'b0;\n"
    .. "  always_comb a = 1'b1;\n  initial begin a = 0; end\nendmodule\n"
  has(idx(src, "systemverilog"), { "module m", "always_ff", "always_comb", "initial" })
end)

-- A procedural block is this language's process, so it nests under whatever
-- owns it and takes that owner's name when it has none of its own.
case("systemverilog_unlabelled_procedural_block_stays_unlabelled", function()
  local src = "module m(input logic clk);\n  logic a;\n  generate\n    for (genvar i=0;i<2;i++) begin : gen_i\n"
    .. "      always_ff @(posedge clk) a <= 1;\n    end\n  endgenerate\nendmodule\n"
  local out = idx(src, "systemverilog")
  -- The generate's label is its own; the block inside was never labelled, so
  -- borrowing it would name the block with a label the source does not put on it.
  has(out, { "gen_i : for generate", "always_ff @(posedge clk)" })
  lacks(out, { "gen_i : always_ff" })
end)

-- A conditional generate nests its block one wrapper deeper than a loop
-- generate; both have to surface, in source order.
case("systemverilog_conditional_and_loop_generates_both_surface", function()
  local src = "module m(input logic clk);\n  generate\n    for (genvar i=0;i<2;i++) begin : gen_i\n"
    .. "      logic a;\n    end\n    if (1) begin : gen_c\n      logic w;\n    end\n  endgenerate\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "gen_i : for generate", "gen_c : if generate", "logic w" })
  assert(out:find("gen_i", 1, true) < out:find("gen_c", 1, true), "generates should keep source order:\n" .. out)
end)

-- A generate is where instances live in real designs; its contents are
-- structure, so they nest as entries rather than as rendered lines.
-- A gate primitive names its type through a gatetype node, not a field, so it
-- is found the same way a module instance is once the type is read from range.
case("systemverilog_gate_primitives_are_instances", function()
  local src = "module t;\n  and g1 (q, a, b);\n  not g2 (z, w);\n  bufif1 g3 (o, i, e);\nendmodule\n"
  has(idx(src, "systemverilog"), {
    "g1 : instance and",
    "g2 : instance not",
    "g3 : instance bufif1",
  })
end)

-- A block generate holds its instances the way a module does; only a
-- module-level instance stays at module level.
case("systemverilog_generate_block_lists_its_instances", function()
  local src = "module top(input logic clk);\n  for (genvar i = 0; i < 4; i++) begin : g_ch\n"
    .. "    fifo u_fifo (.clk(clk));\n  end\n  cdc_sync u_cdc (.clk(clk));\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "g_ch : for generate", "u_fifo : instance fifo", "u_cdc : instance cdc_sync" })
  assert(
    #out:match("( *)u_fifo : instance") > #out:match("( *)g_ch : for generate"),
    "the instance should nest under its generate:\n" .. out
  )
  assert(
    #out:match("( *)u_cdc : instance") == #out:match("( *)module top"),
    "a module-level instance stays at module level:\n" .. out
  )
end)

case("systemverilog_nested_generates_nest", function()
  local src = "module m(input logic clk);\n  for (genvar i = 0; i < 2; i++) begin : g_a\n"
    .. "    for (genvar j = 0; j < 2; j++) begin : g_b\n      always_ff @(posedge clk) q <= d;\n    end\n  end\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "g_a : for generate", "g_b : for generate", "always_ff @(posedge clk)" })
  lacks(out, { "g_b : always_ff" })
  local outer_indent = out:match("( *)g_a : for generate")
  local inner_indent = out:match("( *)g_b : for generate")
  assert(#inner_indent > #outer_indent, "nested generate should indent under its parent:\n" .. out)
end)

-- A case generate item is identified only by its choice, so the choice joins
-- the label; `default` is an anonymous token, not a named child.
case("systemverilog_case_generate_shows_its_choice", function()
  local src = "module m(input logic clk);\n  case (MODE)\n"
    .. "    0: begin : g_a\n      fifo u_a (.clk(clk));\n    end\n"
    .. "    default: begin : g_b\n      fifo u_b (.clk(clk));\n    end\n"
    .. "  endcase\nendmodule\n"
  has(idx(src, "systemverilog"), { "g_a : case generate (0)", "g_b : case generate (default)" })
end)

-- `wire` is the default net type in Verilog-2001, so leaving it out hides the
-- interconnect of every legacy file.
-- A delay on a net declaration is timing refinement, not structure.
case("systemverilog_net_declaration_drops_its_delay", function()
  local out = idx("module m(input logic clk);\n  wire #5 w1;\n  wire w2;\nendmodule\n", "systemverilog")
  has(out, { "wire w1", "wire w2" })
  lacks(out, { "#5" })
end)

case("systemverilog_net_declarations_are_listed", function()
  local src =
    "module top(input logic clk, input logic [7:0] d);\n  wire [7:0] w_data;\n  wire w_valid;\n  logic [7:0] r_data;\nendmodule\n"
  has(idx(src, "systemverilog"), { "wire [7:0] w_data", "wire w_valid", "logic [7:0] r_data" })
end)

case("systemverilog_clocking_declaration_is_listed", function()
  local src =
    "interface axi_if (input logic clk);\n  clocking cb @(posedge clk);\n    output awaddr;\n  endclocking\nendinterface\n"
  has(idx(src, "systemverilog"), { "interface axi_if", "clocking cb @(posedge clk)" })
end)

-- The parameter's type and value are what make it worth naming.
case("systemverilog_parameters_keep_their_type_and_value", function()
  has(
    idx(
      "module m #(parameter int W = 8) (input logic c);\n  localparam [17:0] LO = 74694;\nendmodule\n",
      "systemverilog"
    ),
    {
      "parameter int W = 8",
      "localparam [17:0] LO = 74694",
    }
  )
end)

-- A wrong timescale makes a simulation misbehave, so the value is indexed. The
-- two spellings of the separator collapse to one, and a directive is one line
-- even though its node ends on the newline.
case("systemverilog_timescale_is_indexed_canonically", function()
  local out = idx(
    "`timescale 1ns / 1ps\n`timescale 1 ps / 10 ps\n"
      .. "`timescale 10ns/100ps//time-unit = 10 ns, precision = 100 ps\nmodule m; endmodule\n",
    "systemverilog"
  )
  has(out, { "timescale 1ns/1ps [1]", "timescale 1ps/10ps [2]", "timescale 10ns/100ps [3]" })
  lacks(out, { "1ns / 1ps", "1 ps", "[1-2]", "//", "time-unit" })
end)

-- A class-private type reads as a global name unless it is scoped to the class.
case("systemverilog_class_typedef_is_not_a_global_type", function()
  local out = idx("class C;\n  typedef enum { A, B } e_t;\nendclass\n", "systemverilog")
  assert(out:find("\n    typedef e_t", 1, true), "typedef should nest under its class:\n" .. out)
end)

-- An out-of-body method's prototype is the only place its signature appears.
case("systemverilog_extern_prototype_nests_under_its_class", function()
  has(idx("class C;\n  extern function void drive(int v);\nendclass\n", "systemverilog"), {
    "class C",
    "function void drive(int v)",
  })
end)

-- The event control names the clock and any asynchronous reset.
case("systemverilog_event_control_is_shown", function()
  local src =
    "module m(input logic clk, rst_n);\n  always_ff @(posedge clk or negedge rst_n) q <= d;\n  always_comb x = y;\nendmodule\n"
  has(idx(src, "systemverilog"), { "always_ff @(posedge clk or negedge rst_n)", "always_comb" })
end)

-- `begin : label` is optional, so an unlabelled generate still has to say that
-- it is conditional; otherwise its instances look unconditional.
case("systemverilog_unlabelled_generates_keep_their_kind", function()
  local src = "module m(input logic clk);\n  if (W > 8) begin\n    fifo u_wide (.clk(clk));\n  end\n"
    .. "  else begin\n    fifo u_narrow (.clk(clk));\n  end\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "if generate", "else generate", "u_wide : instance fifo", "u_narrow : instance fifo" })
end)

-- An `else if` chain nests each conditional inside the previous else, so the
-- wrapper must not become a nameless entry.
case("systemverilog_else_if_chain_keeps_every_branch", function()
  local src = "module m(input logic clk);\n  if (D > 4) begin : g_a\n  end\n"
    .. "  else if (D > 2) begin : g_b\n  end\n  else begin : g_c\n  end\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "g_a : if generate", "g_b : if generate", "g_c : else generate" })
  lacks(out, { " : generate [" })
end)

-- A generate body's declarations reach the tree as entries; the member-line
-- pass used to render each one a second time as a range-less line.
case("systemverilog_generate_lists_each_declaration_once", function()
  local src = "module m(input logic clk);\n  for (genvar i = 0; i < 2; i++) begin : g\n"
    .. "    logic inner;\n    fifo u_f (.clk(clk));\n  end\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "g : for generate", "logic inner", "u_f : instance fifo" })
  local count = select(2, out:gsub("logic inner", ""))
  assert(count == 1, "the declaration is listed twice:\n" .. out)
  assert(out:find("logic inner %[%d+%]"), "the declaration lost its line range:\n" .. out)
end)

-- A procedural block inside a generate is a titled line, not an entry, but it
-- still has to appear.
case("systemverilog_generate_keeps_its_procedural_block", function()
  local src = "module m(input logic clk);\n  for (genvar i = 0; i < 2; i++) begin : g\n"
    .. "    always_ff @(posedge clk) q <= d;\n  end\nendmodule\n"
  local out = idx(src, "systemverilog")
  has(out, { "g : for generate", "always_ff @(posedge clk)" })
  lacks(out, { "g : always_ff @(posedge clk)" })
end)

-- Members carry the line that holds them, like every other rendered line.
case("systemverilog_members_carry_their_line", function()
  local out = idx("class C;\n  int x;\n  int y;\nendclass\n", "systemverilog")
  has(out, { "int x [2]", "int y [3]" })
end)

-- A class declared among another class's items is scoped to that class, so it
-- nests instead of appearing beside it as a second top-level entry.
case("systemverilog_nested_class_nests_once", function()
  local src = "class Outer;\n  class Nested;\n    int n1;\n  endclass\n  int o1;\nendclass\n"
  local out = idx(src, "systemverilog")
  has(out, { "class Outer", "class Nested", "int n1", "int o1" })
  assert(select(2, out:gsub("class Nested", "")) == 1, "the inner class is listed twice:\n" .. out)
end)

-- The event control says when a testbench samples; the items say which signals
-- it samples or drives. Skew is timing refinement and a `default` item names no
-- signal, so neither is indexed.
case("systemverilog_clocking_block_lists_direction_and_signal", function()
  local src = "interface ifc(input logic clk);\n  logic sig, other, other2;\n"
    .. "  clocking cb @(posedge clk);\n    default input #1step output #0;\n"
    .. "    input sig;\n    output #3ns other;\n    output #(1+2) other2;\n  endclocking\nendinterface\n"
  local out = idx(src, "systemverilog")
  has(out, { "clocking cb @(posedge clk)", "input sig", "output other", "output other2" })
  lacks(out, { "#1step", "#3ns", "#(1+2)", "default input", ";" })
end)
