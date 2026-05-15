local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("gleam_all_sections", function()
  local src = [==[
import gleam/io
import gleam/option.{Some, None} as opt

pub const max_size = 1024

pub type Shape {
  Circle(Float)
  Rectangle(Float, Float)
}

type Color = String

pub external type ForeignResource

pub external fn now() -> Int = "erlang" "erlang:system_time/0"

pub fn greet(name: String) -> String {
  "Hello, " <> name
}

fn helper(x: Int) -> Int {
  x + 1
}
]==]
  local out = idx(src, "gleam")
  has(out, {
    "imports:",
    "gleam",
    "io",
    "option",
    "Some, None as opt",
    "consts:",
    "const max_size",
    "types:",
    "type Shape",
    "Circle",
    "Rectangle",
    "type Color",
    "external type ForeignResource",
    "fns:",
    "external fn now() -> Int",
    "fn greet(name: String) -> String",
    "fn helper(x: Int) -> Int",
  })
end)

case("gleam_doc_comments", function()
  local src = [==[
//// Module documentation
//// More module docs

/// Shape documentation
pub type Shape {
  Circle(Float)
}

/// Adds two numbers
pub fn add(a: Int, b: Int) -> Int {
  a + b
}
]==]
  local out = idx(src, "gleam")
  has(out, {
    "module doc:",
    "type Shape",
    "fn add(a: Int, b: Int) -> Int",
  })
end)

case("gleam_import_unqualified", function()
  local src = [==[
import gleam/io
import gleam/option.{Some, None} as opt
import gleam/result.{Ok, Error}
]==]
  local out = idx(src, "gleam")
  has(out, {
    "imports:",
    "gleam",
    "io",
    "option",
    "Some, None as opt",
    "result",
    "Ok, Error",
  })
end)

case("gleam_opaque_type", function()
  local src = [==[
pub opaque type Currency(Int) {
  USD(Int)
  EUR(Int)
  GBP(Int)
}
]==]
  local out = idx(src, "gleam")
  has(out, {
    "opaque type Currency(Int)",
    "USD",
    "EUR",
    "GBP",
  })
end)

case("gleam_function_types", function()
  local src = [==[
pub fn map(list: List(a), with fun: fn(a) -> b) -> List(b) {
  todo
}

fn apply(f: fn(Int) -> String, x: Int) -> String {
  f(x)
}

pub external fn parse(input: String) -> Result(Int, Nil) = "erlang" "erlang:list_to_integer/1"
]==]
  local out = idx(src, "gleam")
  has(out, {
    "fn map(",
    "-> List(b)",
    "fn apply(",
    "-> String",
    "external fn parse(",
    "-> Result(Int, Nil)",
  })
end)

case("gleam_type_truncation", function()
  local src = [==[
pub type Big {
  A
  B
  C
  D
  E
  F
  G
  H
  I
  J
}
]==]
  local out = idx(src, "gleam")
  has(out, {
    "type Big",
    "A",
    "H",
    "truncated",
  })
end)

case("gleam_type_params", function()
  local src = [==[
pub type Result(a, b) {
  Ok(a)
  Error(b)
}

pub opaque type Token(t) {
  Token(t)
}

type Alias(a) = Result(a, Nil)
]==]
  local out = idx(src, "gleam")
  has(out, {
    "type Result(a, b)",
    "Ok",
    "Error",
    "opaque type Token(t)",
    "type Alias(a)",
  })
end)
