local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local eq = th.eq
local indexer = require("indexer")
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

local PARSE_ERROR = "cannot index .v source as V: parse errors; use read instead"

case("v_all_sections", function()
  local src = [==[
module sample

import os
import encoding.json as json

pub const max_size = 1024

__global (
  counter int
  label = 'ready'
)

pub struct Point {
  x f64
  y f64
}

pub interface Drawable {
  name string
  Reader
  draw() string
}

pub enum Color {
  red
  green
  blue
}

pub type Identifier = int
pub type Number = int | f64

pub fn add(a int, b int) int {
  return a + b
}

pub fn (point Point) distance() f64 {
  return point.x + point.y
}

fn test_add() {
  assert add(1, 2) == 3
}
]==]
  local out = idx(src, "v")
  has(out, {
    "imports:",
    "os",
    "encoding.json",
    "consts:",
    "pub const max_size",
    "global counter int",
    "global label",
    "types:",
    "pub struct Point",
    "x f64",
    "y f64",
    "pub interface Drawable",
    "name string",
    "Reader",
    "draw() string",
    "pub enum Color",
    "red, green, blue",
    "pub type Identifier = int",
    "pub type Number = int | f64",
    "fns:",
    "pub add(a int, b int) int",
    "impls:",
    "pub (point Point) distance() f64",
    "tests:",
  })
  lacks(out, { "test_add()" })
end)

case("v_doc_comments_and_attributes", function()
  local src = [==[
// Point documentation
@[heap]
pub struct Point {
  value int
}

// Adds one
@[inline]
pub fn add_one(value int) int {
  return value + 1
}
]==]
  eq(idx(src, "v"), "types:\n  pub struct Point [1-5]\n    value int\n\nfns:\n  pub add_one(value int) int [7-11]\n")
end)

case("v_static_methods", function()
  local src = [==[
struct Foo {}
pub fn Foo.new() Foo { return Foo{} }
fn Foo.test_new(value int) Foo { return Foo{} }
pub fn (foo Foo) copy() Foo { return foo }
]==]
  eq(
    idx(src, "v"),
    "types:\n  struct Foo [1]\n\nimpls:\n  pub Foo.new() Foo [2]\n  Foo.test_new(value int) Foo [3]\n  pub (foo Foo) copy() Foo [4]\n"
  )
end)

case("v_const_and_global_groups", function()
  local src = [==[
@[deprecated]
const (
  first = 1
  second = 2
)
@[weak]
__global (
  counter int
  label = 'ready'
  flag bool
)
]==]
  eq(
    idx(src, "v"),
    "consts:\n  const first [1-3]\n  const second [4]\n  global counter int [6-8]\n  global label [9]\n  global flag bool [10]\n"
  )
end)

case("v_union_keyword", function()
  local src = "struct union_foo {\n value int\n}\npub union Value {\n integer int\n decimal f64\n}\n"
  eq(
    idx(src, "v"),
    "types:\n  struct union_foo [1-3]\n    value int\n  pub union Value [4-7]\n    integer int\n    decimal f64\n"
  )
end)

case("v_generic_parameters", function()
  local src = [==[
pub struct List[T] {
  data []T
}

pub interface Iter[T] {
  next() T
}

pub fn first[T](items []T) T {
  return items[0]
}
]==]
  has(idx(src, "v"), {
    "pub struct List[T]",
    "data []T",
    "pub interface Iter[T]",
    "next() T",
    "pub first[T](items []T) T",
  })
end)

case("v_rejects_other_languages_and_parse_errors", function()
  for _, src in ipairs({
    "module counter(input clk, output reg q);\nalways @(posedge clk) q <= ~q;\nendmodule\n",
    "Definition identity (n : nat) : nat := n.\nLemma identity_eq : forall n, identity n = n.\nProof. reflexivity. Qed.\n",
    "fn valid() {}\nstruct Broken {\n",
  }) do
    local result, err = indexer.index_source(src, "v")
    eq(result, nil)
    eq(err, PARSE_ERROR)
  end
end)

case("v_interface_members_truncate_across_kinds", function()
  local src = [==[
interface Big {
  a int
  b() string
  Reader
  d(x int)
  e string
  f() bool
  g int
  h()
  i int
  j()
}
]==]
  eq(
    idx(src, "v"),
    "types:\n  interface Big [1-12]\n    a int\n    b() string\n    Reader\n    d(x int)\n    e string\n    f() bool\n    g int\n    h()\n    [2 more truncated]\n"
  )
end)
