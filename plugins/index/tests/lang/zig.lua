local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has

case("zig_all_sections", function()
  local src = [==[
const std = @import("std");
const mem = @import("std/mem.zig");

pub const MAX_SIZE = 1024;

var global_counter: u32 = 0;

pub const Point = struct {
    x: f32,
    y: f32,
};

const Color = enum {
    red,
    green,
    blue,
};

const Value = union {
    int: i32,
    float: f64,
};

const MyError = error{
    OutOfMemory,
    InvalidArgument,
};

pub const OpaqueHandle = opaque {};

pub fn add(a: i32, b: i32) i32 {
    return a + b;
}

fn internal_helper() void {}

test "basic math" {
    try std.testing.expect(add(2, 2) == 4);
}

usingnamespace @import("std/testing.zig");
]==]
  local out = idx(src, "zig")
  has(out, {
    "imports:",
    "std",
    "std/mem.zig",
    "std/testing.zig",
    "consts:",
    "const MAX_SIZE",
    "var global_counter: u32",
    "types:",
    "struct Point",
    "x: f32",
    "y: f32",
    "enum Color",
    "red, green, blue",
    "union Value",
    "int: i32",
    "float: f64",
    "error MyError",
    "OutOfMemory, InvalidArgument",
    "opaque OpaqueHandle",
    "fns:",
    "add(a: i32, b: i32) i32",
    "internal_helper()",
    "tests:",
  })
end)

case("zig_doc_comments", function()
  local src = [==[
//! Module-level documentation
//! More docs

/// Point documentation
pub const Point = struct {
    x: f32,
};

/// Adds two numbers
pub fn add(a: i32, b: i32) i32 {
    return a + b;
}
]==]
  local out = idx(src, "zig")
  has(out, {
    "module doc: [1-2]",
    "types:",
    "struct Point",
    "fns:",
    "add(a: i32, b: i32) i32",
  })
end)

case("zig_extern_function", function()
  local src = [==[
extern "c" fn printf(format: [*:0]const u8, ...) c_int;

pub export fn entry() void {}
]==]
  local out = idx(src, "zig")
  has(out, {
    "fns:",
    "printf(format: [*:0]const u8, ...) c_int",
    "entry()",
  })
end)

case("zig_var_no_type", function()
  local src = [==[
var counter = 0;
const label = "hello";
]==]
  local out = idx(src, "zig")
  has(out, {
    "consts:",
    "var counter",
    "const label",
  })
end)

case("zig_anytype_param", function()
  local src = [==[
pub fn print(writer: anytype, value: i32) void {}
pub fn identity(x: anytype) anytype { return x; }
]==]
  local out = idx(src, "zig")
  has(out, {
    "fns:",
    "print(writer: anytype, value: i32)",
    "identity(x: anytype) anytype",
  })
end)

case("zig_usingnamespace", function()
  local src = [==[
usingnamespace @import("std");

const Foo = struct {
    x: i32,
};
]==]
  local out = idx(src, "zig")
  has(out, {
    "imports:",
    "std",
    "types:",
    "struct Foo",
  })
end)

case("zig_struct_truncation", function()
  local src = [==[
pub const Big = struct {
    a: u8,
    b: u8,
    c: u8,
    d: u8,
    e: u8,
    f: u8,
    g: u8,
    h: u8,
    i: u8,
    j: u8,
};
]==]
  local out = idx(src, "zig")
  has(out, {
    "types:",
    "struct Big",
    "a: u8",
    "h: u8",
    "truncated",
  })
end)

case("zig_struct_fields_plain_children_no_meta", function()
  local src = [==[
pub const Point = struct {
    x: f32,
    y: f32,
};
]==]
  local text, meta = idx_with_meta(src, "zig")
  helpers.assert_fields_no_ranged_meta(text, meta, "struct Point", { "x: f32", "y: f32" })
end)
