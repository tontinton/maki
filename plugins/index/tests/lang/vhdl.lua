local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local eq = th.eq
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks
local lacks_in_section = helpers.lacks_in_section
local section_body = helpers.section_body

local ALL_SECTIONS = [==[
library ieee;
use ieee.std_logic_1164.all;
use work.util_pkg;

entity counter is
  generic (WIDTH : integer := 8);
  port (
    clk : in std_logic;
    q   : out std_logic_vector(7 downto 0)
  );
end entity counter;

architecture rtl of counter is
  attribute keep : boolean;
  attribute keep of cnt : signal is true;
  signal cnt : unsigned(7 downto 0);
  constant MAX : integer := 255;
  type state_t is (IDLE, RUN);
  subtype small_t is integer range 0 to 7;
  component adder is
    generic (type t; g : integer := 8);
    port (a : in std_logic; signal s : in std_logic);
  end component;
  procedure reset_all(signal s : out std_logic);
begin
  u_adder : adder port map (a => clk);
  p_main : process (clk)
  begin
  end process;
  g_loop : for i in 0 to 3 generate
  end generate;
end architecture rtl;

package util_pkg is
  function inc(x : integer) return integer;
end package;

package body util_pkg is
  constant WIDTH : natural := 8;
  type state_t is (IDLE, RUN);
  function inc(x : integer) return integer is
  begin
    return x + 1;
  end function;
end package body;

configuration cfg of counter is
  for rtl
  end for;
end configuration cfg;
]==]

-- Generics and ports are budgeted apart, so a long generic list cannot cut the
-- interface off.
case("vhdl_generics_do_not_starve_the_ports", function()
  local generics = {}
  for i = 1, 20 do
    generics[#generics + 1] = "    G" .. i .. " : natural := " .. i
  end
  local src = "entity e is\n  generic (\n"
    .. table.concat(generics, ";\n")
    .. "\n  );\n"
    .. "  port (clk : in std_logic; rst : in std_logic);\nend entity;\n"
  local out = idx(src, "vhdl")
  has(
    out,
    { "G16 : natural := 16 [18]", "[4 generics truncated]", "clk : in std_logic [24]", "rst : in std_logic [24]" }
  )
  lacks(out, { "G17 : natural" })
end)

case("vhdl_all_sections", function()
  local out = idx(ALL_SECTIONS, "vhdl")
  has(out, {
    "imports:",
    "ieee.std_logic_1164.all",
    "work.util_pkg",
    "library: ieee",
    "units:",
    "entity counter",
    "WIDTH : integer := 8 [6]",
    "clk : in std_logic [8]",
    "q : out std_logic_vector(7 downto 0) [9]",
    -- A package is a container: a declaration and a body are separate library
    -- units, so each names itself and holds its own members, rather than
    -- claiming file scope.
    "package util_pkg",
    "package body util_pkg",
    "type state_t is enum",
    "      IDLE",
    "      RUN",
    "subtype small_t integer range 0 to 7",
    "configuration cfg of counter",
    "impls:",
    "architecture rtl of counter",
    "function inc(x : integer) return integer",
  })
  -- An architecture is a container: its declarative part and concurrent
  -- statements nest under it rather than listing beside it, so a file with
  -- several architectures stays unambiguous.
  local arch = section_body(out, "impls:")
  has(arch, {
    "    attribute keep of cnt : signal is true",
    "    cnt : unsigned(7 downto 0)",
    "    constant MAX : integer",
    "    type state_t is enum",
    "      IDLE",
    "      RUN",
    "    subtype small_t integer range 0 to 7",
    "    component adder",
    "      generic type t",
    "      g : integer := 8",
    "      a : in std_logic",
    "      signal s : in std_logic",
    "    procedure reset_all(signal s : out std_logic)",
    "    u_adder : instance adder",
    "    p_main : process",
    "    g_loop : for generate",
  })
end)

-- A context groups library and use clauses, and a generic package instance is
-- a package; both are design units that used to index as empty.
case("vhdl_context_declaration", function()
  has(idx("context ctx is\n  library ieee;\n  use ieee.std_logic_1164.all;\nend context;\n", "vhdl"), {
    "context ctx",
    "library: ieee",
    "ieee.std_logic_1164.all",
  })
end)

case("vhdl_package_instantiation", function()
  has(idx("package p is new work.generic_pkg generic map (W => 8);\n", "vhdl"), {
    "package p is new work.generic_pkg",
  })
end)

-- Record fields are `element_declaration` siblings under
-- `record_type_definition`, so the type names the record and these list it.
case("vhdl_record_fields", function()
  local out =
    idx("package p is\n  type r_t is record\n    a, b : integer;\n    y : bit;\n  end record;\nend package;\n", "vhdl")
  has(out, {
    "type r_t is record",
    -- Listed as children, one line each, rather than flattened into the type
    -- line: the header ends its own line and the first field starts the next.
    "type r_t is record [2-5]\n      a, b : integer [3]\n      y : bit [4]",
  })
end)

-- Each alternative is the only thing that says which configuration a body
-- belongs to, and the three forms take different grammar paths: a plain
-- expression, a range, and the `others` token.
case("vhdl_case_generate", function()
  local src = "architecture a of e is\nbegin\n  g : case SEL generate\n"
    .. "    when 0 =>\n      b1 : block\n        signal s : bit;\n      begin\n      end block;\n"
    .. "    when 1 to 3 =>\n      b2 : block\n      begin\n      end block;\n"
    .. "    when others =>\n      b3 : block\n      begin\n      end block;\n"
    .. "  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(
    out,
    { "g : case generate", "when 0", "when 1 to 3", "when others", "b1 : block", "b2 : block", "b3 : block", "s : bit" }
  )
  local alternative_indent = out:match("( *)when 0")
  local body_indent = out:match("( *)b1 : block")
  assert(#body_indent > #alternative_indent, "a body should nest under its alternative:\n" .. out)
end)

case("vhdl_alias_and_group_declarations", function()
  has(
    idx(
      "architecture a of e is\n  alias slv is std_logic_vector;\n  group g is (signal, constant);\nbegin\nend architecture;\n",
      "vhdl"
    ),
    { "alias slv is std_logic_vector", "group g is (signal, constant)" }
  )
end)

case("vhdl_protected_type_methods", function()
  has(
    idx(
      "package p is\n  type c_t is protected\n    procedure inc;\n    impure function val return integer;\n  end protected;\nend package;\n",
      "vhdl"
    ),
    { "type c_t is protected", "procedure inc", "impure function val return integer" }
  )
end)

case("vhdl_nested_subprograms_in_process", function()
  has(
    idx(
      "architecture a of e is\nbegin\n  p : process\n    procedure lp (x : integer) is\n    begin\n    end;\n  begin\n  end process;\nend architecture;\n",
      "vhdl"
    ),
    { "p : process", "procedure lp (x : integer)" }
  )
end)

case("vhdl_generic_interface_types", function()
  has(
    idx("package p is\n  generic (type T; function f(x : T) return T);\nend package;\n", "vhdl"),
    { "package p", "type T", "function f(x : T) return T" }
  )
end)

case("vhdl_context_reference", function()
  has(idx("context work.ctx;\nentity e is end entity;\n", "vhdl"), { "work.ctx", "entity e" })
end)

case("vhdl_architecture_signal_list_is_truncated", function()
  local signals = {}
  for i = 1, 12 do
    signals[#signals + 1] = "  signal s" .. i .. " : std_logic;"
  end
  local src = "architecture rtl of e is\n" .. table.concat(signals, "\n") .. "\nbegin\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "architecture rtl of e", "s1 : std_logic", "nets truncated]" })
  assert(not out:find("s9 : std_logic", 1, true), "s9 should be truncated:\n" .. out)
end)

-- A package with more declarations than the threshold used to put the
-- truncation marker into the entry list, which crashed the skeleton formatter.
-- The marker is a child line of the package now that its members nest under it.
case("vhdl_package_truncates_declarations_without_crashing", function()
  local types = {}
  for i = 1, 70 do
    types[#types + 1] = "  type t" .. i .. " is array (natural range <>) of std_logic;"
  end
  local src = "package p is\n" .. table.concat(types, "\n") .. "\nend package;\n"
  local out = idx(src, "vhdl")
  has(out, { "package p", "type t1 is array", "type t32 is array", "[38 types truncated]" })
  assert(not out:find("type t33 is array", 1, true), "t33 should be truncated:\n" .. out)
end)

-- A package body lives in its own file, so it has to name itself or the
-- functions it holds cannot be told which package they belong to.
case("vhdl_package_body_names_itself", function()
  has(
    idx("package body p is\n  function f return integer is begin return 1; end function;\nend package body;\n", "vhdl"),
    { "package body p", "function f return integer" }
  )
end)

-- A declaration and a body are separate library units, so each names itself and
-- holds its own members; neither claims the other's.
case("vhdl_package_and_body_hold_their_own_members", function()
  local src = "package p is\n  constant N : integer := 4;\nend package;\n\n"
    .. "package body p is\n  constant K : integer := 9;\nend package body;\n"
  local out = idx(src, "vhdl")
  has(out, { "package p", "constant N : integer", "package body p", "constant K : integer" })
  local decl = out:match("package p %[%d+%-%d+%]\n(.-)package body")
  assert(decl and decl:find("constant N", 1, true), "the declaration holds its own members:\n" .. out)
  assert(decl and not decl:find("constant K", 1, true), "the body's members do not belong to it:\n" .. out)
end)

-- An unlabelled process is legal but anonymous, so its enclosing generate is
-- what tells it apart from its neighbours.
-- An unlabelled process borrows its owner's identity by nesting under it, not
-- by repeating the generate name in its own text.
case("vhdl_unlabelled_process_nests_under_its_generate", function()
  local src =
    "architecture rtl of e is\nbegin\n  g1 : if true generate\n    process (clk) begin end process;\n  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "g1 : if generate", "process (clk)" })
  lacks(out, { "g1 process" })
  local generate_indent = out:match("( *)g1 : if generate")
  local process_indent = out:match("( *)process %(clk%)")
  assert(#process_indent > #generate_indent, "process should nest under the generate:\n" .. out)
end)

-- Each kind gets its own budget, so a package's constants and types cannot
-- crowd out the components a downstream file instantiates against. Components
-- share the design-unit key, which no other member uses, so the budget holds.
case("vhdl_package_components_keep_their_own_budget", function()
  -- 35 types overflow the 32 cap on their own, so a component surviving with no
  -- truncation of its own is only possible if it is bucketed apart from them.
  local decls = {}
  for i = 1, 35 do
    decls[#decls + 1] = "  type t" .. i .. " is (" .. tostring(i) .. ");"
  end
  for i = 1, 3 do
    decls[#decls + 1] = "  component c" .. i .. " is port (clk : in bit); end component;"
  end
  local out = idx("package p is\n" .. table.concat(decls, "\n") .. "\nend package;\n", "vhdl")
  has(out, { "component c1", "component c2", "component c3", "[3 types truncated]" })
  lacks(out, { "components:" })
end)

-- A component declaration states the defaults a component-style instantiation
-- takes for its un-mapped formals, so the block is content, not a copy. It is
-- still the second statement of one interface, so it gets the smaller budget:
-- 32 keeps a mid-size package whole and truncates an IP wrapper's list.
case("vhdl_component_interface_uses_the_component_budget", function()
  local generics, ports = {}, {}
  for i = 1, 40 do
    generics[#generics + 1] = "    G" .. i .. " : natural := " .. i .. ";"
    ports[#ports + 1] = "    p" .. i .. " : in std_logic;"
  end
  local src = "entity e is end entity;\narchitecture a of e is\n  component c is\n"
    .. "  generic (\n"
    .. table.concat(generics, "\n")
    .. "\n  );\n  port (\n"
    .. table.concat(ports, "\n")
    .. "\n  );\n  end component;\nbegin\n  u : c;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(
    out,
    { "component c", "G32 : natural := 32", "p32 : in std_logic", "[8 generics truncated]", "[8 ports truncated]" }
  )
  assert(not out:find("G33 : natural", 1, true), "G33 should be truncated:\n" .. out)
end)

-- Only the attributes that state synthesis or CDC intent are raised: a vendor
-- IP fills a file with packaging metadata, which says nothing about the design.
case("vhdl_attributes_are_filtered_to_synthesis_intent", function()
  local src = "architecture a of e is\n  signal s : std_logic;\n"
    .. "  attribute keep : boolean;\n" -- forward declaration: names a type only
    .. "  attribute soft_hlutnm : string;\n"
    .. "  ATTRIBUTE Keep OF s : signal IS true;\n"
    .. '  attribute async_reg of s : signal is "true";\n'
    .. '  attribute X_INTERFACE_INFO of s : signal is "vendor";\n'
    .. "begin\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "ATTRIBUTE Keep OF s : signal IS true [5]", "attribute async_reg of s" })
  lacks(out, { "soft_hlutnm", "X_INTERFACE_INFO", "attribute keep : boolean" })
end)

-- Attributes annotate the declarations around them rather than being members of
-- the body, so they hold a budget of their own within a container: a
-- signal-heavy architecture cannot crowd them out, and one that annotates every
-- signal cannot crowd out the signals. Eight is the cap, which every
-- hand-written design in the corpus stays under.
-- Members print in the order the file declares them, not grouped by section.
case("vhdl_package_members_keep_source_order", function()
  -- The declaration order here is the reverse of the section order, so a walk
  -- that emits by section would print these the wrong way round.
  local out = idx(
    "package p is\n  type t is (x, y);\n  constant A : integer := 1;\n  function f return integer;\nend package;\n",
    "vhdl"
  )
  assert(out:find("type t", 1, true) < out:find("constant A", 1, true), "the type comes first:\n" .. out)
  assert(out:find("constant A", 1, true) < out:find("function f", 1, true), "the constant comes first:\n" .. out)
end)

case("vhdl_attributes_hold_a_budget_of_their_own", function()
  local attrs, signals = {}, {}
  for i = 1, 6 do
    attrs[#attrs + 1] = '  attribute keep of s : signal is "true";'
  end
  -- Twelve signals overflow a cap of eight on their own. They come first, so a
  -- shared bucket would be full before the attributes were reached; all six
  -- surviving is only possible when the two lists have separate budgets.
  for i = 1, 12 do
    signals[#signals + 1] = "  signal q" .. i .. " : bit;"
  end
  local src = "architecture a of e is\n  signal s : bit;\n"
    .. table.concat(signals, "\n")
    .. "\n"
    .. table.concat(attrs, "\n")
    .. "\nbegin\nend architecture;\n"
  local out = idx(src, "vhdl")
  local kept = select(2, out:gsub("attribute keep of s", ""))
  eq(kept, 6, "every attribute survives the signal flood:\n" .. out)
  has(out, { "[5 nets truncated]" })
end)

-- A case statement is raised because it carries the state machine or the
-- decoder; its branches stay in the source, and `if` is left alone because every
-- process has one for its clock edge.
case("vhdl_case_headers_are_raised_in_every_body", function()
  local src = "package body p is\n"
    .. "  function f (x : integer) return integer is\n    variable r : integer;\n  begin\n"
    .. "    case x is\n      when 0 => r := 1;\n      when others => r := 2;\n    end case;\n"
    .. "    return r;\n  end function;\nend package body;\n"
    .. "architecture a of e is\nbegin\n  p : process (clk)\n  begin\n"
    .. "    if rising_edge(clk) then\n      case sel is\n        when '0' => q <= a;\n"
    .. "        when others => q <= b;\n      end case;\n    end if;\n  end process;\n"
    .. "end architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "case x [5-8]", "case sel [17-20]" })
  -- The spacing before the parenthesis is regularised; the keyword's case is
  -- the author's.
  has(
    idx(
      "architecture a of e is\nbegin\n  p : process (x)\n  begin\n"
        .. "    case(x) is\n      when 0 => null;\n    end case;\n  end process;\nend architecture;\n",
      "vhdl"
    ),
    {
      "case (x) [5-7]",
    }
  )
  has(
    idx(
      "architecture a of e is\nbegin\n  p : process (x)\n  begin\n"
        .. "    CASE x IS\n      WHEN 0 => null;\n    END CASE;\n  end process;\nend architecture;\n",
      "vhdl"
    ),
    {
      "CASE x [5-7]",
    }
  )
  lacks(out, { "if rising_edge", "when 0", "when others" })
end)

-- The maps say what this instance is configured with; a component declaration
-- only says what it defaults to. Both get a line and a range, and the bindings
-- stay in source.
case("vhdl_instantiation_maps_are_labelled_with_ranges", function()
  local src = "entity e is end entity;\narchitecture a of e is\nbegin\n"
    .. "  u : entity work.sub\n    generic map (W => 8)\n    port map (q => open);\n"
    .. "end architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "u : instance entity work.sub [4-6]", "generic map [5]", "port map [6]" })
end)

-- A package exports its members, so its budget is the large one, and it is held
-- per container: a second package keeps its own 64 however big the first is.
case("vhdl_package_members_have_their_own_budget", function()
  local function pkg(name, n)
    local decls = {}
    for i = 1, n do
      decls[#decls + 1] = "  constant C" .. i .. " : integer := " .. i .. ";"
    end
    return "package " .. name .. " is\n" .. table.concat(decls, "\n") .. "\nend package " .. name .. ";\n"
  end
  local out = idx(pkg("a", 70) .. "\n" .. pkg("b", 3), "vhdl")
  has(out, { "package a", "package b", "constant C1 : integer", "constant C32 : integer", "[38 consts truncated]" })
  assert(not out:find("constant C33", 1, true), "C33 should be truncated:\n" .. out)
  -- The small package is listed whole, so the first one's cut did not carry over.
  local b = out:match("package b.-$")
  assert(
    b and b:find("constant C3 : integer", 1, true) and not b:find("truncated", 1, true),
    "b lost members:\n" .. out
  )
end)

-- Subprograms are members too: an overload-heavy package such as `fixed_pkg`
-- would otherwise list hundreds of them past the budget its constants obey.
case("vhdl_package_subprograms_share_the_member_budget", function()
  local specs = {}
  for i = 1, 40 do
    specs[#specs + 1] = "  function f" .. i .. " (a : integer) return integer;"
  end
  local out = idx("package p is\n" .. table.concat(specs, "\n") .. "\nend package;\n", "vhdl")
  has(out, {
    "function f1 (a : integer) return integer",
    "function f32 (a : integer) return integer",
    "[8 fns truncated]",
  })
  assert(not out:find("function f33", 1, true), "f33 should be truncated:\n" .. out)
end)

-- A generic package's formals share the package budget. Capping them per clause
-- as well would cut them to the eight-member default, the way a body's
-- declarations were once cut twice.
case("vhdl_generic_formals_use_the_package_budget", function()
  local formals = { "    type t_data;" }
  for i = 1, 40 do
    formals[#formals + 1] = "    constant C" .. i .. " : natural := " .. i .. ";"
  end
  local out = idx("package p is\n  generic (\n" .. table.concat(formals, "\n") .. "\n  );\nend package;\n", "vhdl")
  has(out, { "constant C1 : natural := 1", "constant C32 : natural := 32", "[8 consts truncated]" })
  assert(not out:find("constant C33", 1, true), "C33 should be truncated:\n" .. out)
end)

-- The sensitivity list is what separates a synchronous process from one with
-- an asynchronous reset, so it belongs on the line.
case("vhdl_process_sensitivity_lists", function()
  local src = "architecture rtl of e is\nbegin\n"
    .. "  p_all : process (all) begin end process;\n"
    .. "  p_async : process (clk, rst) begin end process;\n"
    .. "  p_bare : process begin wait; end process;\nend architecture;\n"
  has(idx(src, "vhdl"), { "p_all : process (all)", "p_async : process (clk, rst)", "p_bare : process" })
end)

-- A generic package's formals are its interface, and each kind belongs in the
-- section a real declaration of that kind would use.
case("vhdl_generic_package_formals_are_indexed", function()
  local src = "package p is\n  generic (\n    type t_data;\n"
    .. "    constant C_WIDTH : natural := 8;\n"
    .. "    function f_max (a : natural; b : natural) return natural\n  );\n"
    .. "  function f_common (x : t_data) return integer;\nend package;\n"
  local out = idx(src, "vhdl")
  has(out, {
    "constant C_WIDTH : natural := 8",
    "generic type t_data",
    "generic function f_max (a : natural; b : natural) return natural",
    "function f_common (x : t_data) return integer",
  })
  local header = out:match("( *)package p")
  local generic_fn = out:match("( *)generic function f_max")
  assert(header and generic_fn and #generic_fn > #header, "the formals should nest under the package:\n" .. out)
end)

-- An `if generate` and its `else` are two configurations of one statement, so
-- they stand beside each other, each owning its own contents.
case("vhdl_else_generate_stands_beside_its_if", function()
  local src = "architecture rtl of e is\nbegin\n  g : if W > 8 generate\n"
    .. "    u_w : entity work.wide;\n  else generate\n    u_n : entity work.narrow;\n"
    .. "  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(
    out,
    { "g : if generate", "g : else generate", "u_w : instance entity work.wide", "u_n : instance entity work.narrow" }
  )
  local count = select(2, out:gsub("u_n : instance", ""))
  assert(count == 1, "the else body must not be listed twice:\n" .. out)
end)

-- A generate with a `begin` has its own declarative region; those declarations
-- belong to the generate, not to the architecture around it.
case("vhdl_generate_declarations_nest_under_the_generate", function()
  local src = "architecture rtl of e is\nbegin\n  g : for i in 0 to 1 generate\n"
    .. "    signal gs : bit;\n  begin\n    u2 : entity work.lane;\n  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "g : for generate", "gs : bit", "u2 : instance entity work.lane" })
  local generate_indent = out:match("( *)g : for generate")
  assert(
    #out:match("( *)gs : bit") == #generate_indent + 2,
    "declaration should sit one level under its generate:\n" .. out
  )
end)

-- An `if generate` hides its declarative region one level deeper than a `for`
-- generate (`if_generate` then `generate_body`), so the declarations and the
-- instances under them are the cases that tell a real search from a guess.
case("vhdl_if_generate_keeps_its_declarations_and_instances", function()
  local src = "architecture rtl of e is\nbegin\n  g : if COND generate\n"
    .. "    signal gs : bit;\n  begin\n    u1 : sub;\n  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "g : if generate", "gs : bit", "u1 : instance sub" })
  local generate_indent = out:match("( *)g : if generate")
  assert(
    #out:match("( *)gs : bit") == #generate_indent + 2,
    "declaration should sit one level under its generate:\n" .. out
  )
  assert(
    #out:match("( *)u1 : instance sub") == #generate_indent + 2,
    "instance should sit one level under its generate:\n" .. out
  )
end)

-- A generate nested in another holds its declarations, not its parent: the
-- outer head must not reach through to the inner one.
case("vhdl_nested_generate_keeps_its_own_declarations", function()
  local src = "architecture rtl of e is\nbegin\n  outer : if C1 generate\n"
    .. "    inner : if C2 generate\n      signal only_inner : bit;\n    begin\n    end generate;\n"
    .. "  begin\n  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  assert(select(2, out:gsub("only_inner : bit", "")) == 1, "declaration belongs to the inner generate only:\n" .. out)
  local outer_indent = out:match("( *)outer : if generate")
  assert(
    #out:match("( *)only_inner : bit") == #outer_indent + 4,
    "declaration should sit under the inner generate:\n" .. out
  )
end)

-- A generate's head is a declarative region: its contents are attached to the
-- generate, and must not also be walked as concurrent statements, which would
-- list every subprogram in the head a second time.
case("vhdl_generate_head_is_listed_once", function()
  local src = "architecture rtl of e is\nbegin\n  g : if C1 generate\n"
    .. "    function f(x : bit) return bit is begin return x; end function;\n"
    .. "  begin\n  end generate;\nend architecture;\n"
  local out = idx(src, "vhdl")
  has(out, { "g : if generate", "function f(x : bit) return bit" })
  -- Counted literally: `(` is a pattern metacharacter, so gsub would not match.
  local seen = 0
  for _ in out:gmatch("function f%(x : bit%) return bit") do
    seen = seen + 1
  end
  assert(seen == 1, "a head declaration should be listed once:\n" .. out)
end)

-- A subprogram declared in another subprogram's head is scoped to it, so it
-- follows its owner rather than standing beside it.
case("vhdl_nested_subprogram_nests_under_its_owner", function()
  local src = "package body p is\n  function outer (x : bit) return bit is\n"
    .. "    function inner (y : bit) return bit is\n    begin\n      return y;\n    end function;\n"
    .. "  begin\n    return inner(x);\n  end function;\nend package body;\n"
  local out = idx(src, "vhdl")
  has(out, { "function outer", "function inner" })
  assert(
    #out:match("( *)function inner") > #out:match("( *)function outer"),
    "the inner function should nest:\n" .. out
  )
end)

-- A protected type body defines the methods its declaration announced; it is
-- not a plain type, so it must not render as raw truncated text.
case("vhdl_protected_type_body_lists_its_methods", function()
  local src = "package body p is\n  type t_prot is protected body\n    variable vv : bit;\n"
    .. "    procedure inc (v : in bit) is\n    begin\n    end procedure;\n  end protected body;\nend package body;\n"
  local out = idx(src, "vhdl")
  has(out, { "type t_prot is protected body", "procedure inc (v : in bit)" })
  lacks(out, { "[truncated]" })
end)
