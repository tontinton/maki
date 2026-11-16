local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has

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
    port (a : in std_logic);
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

case("vhdl_all_sections", function()
  has(idx(ALL_SECTIONS, "vhdl"), {
    "imports:",
    "ieee.std_logic_1164.all",
    "work.util_pkg",
    "library: ieee",
    "mod:",
    "package util_pkg",
    "types:",
    "entity counter",
    "WIDTH : integer := 8",
    "clk : in std_logic",
    "q : out std_logic_vector(7 downto 0)",
    "type state_t is (IDLE, RUN)",
    "subtype small_t integer range 0 to 7",
    "component adder",
    "a : in std_logic",
    "blocks:",
    "configuration cfg of counter",
    "g_loop : for generate",
    "impls:",
    "architecture rtl of counter",
    "attribute keep : boolean",
    "attribute keep of cnt : signal is true",
    "cnt : unsigned(7 downto 0)",
    "constant MAX : integer",
    "procedure reset_all(signal s : out std_logic)",
    "u_adder : instance adder",
    "consts:",
    "constant WIDTH : natural",
    "fns:",
    "inc",
    "p_main : process",
  })
end)
