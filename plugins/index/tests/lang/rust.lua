local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has
local lacks = helpers.lacks

case("rust_all_sections", function()
  local src = [[//! Module doc
use std::collections::HashMap;
use std::io;
use std::io::*;
use std::{fs, net};

const MAX: usize = 1024;
static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

pub struct Empty;

enum Color { Red, Green }

pub type Result<T> = std::result::Result<T, MyError>;

pub trait Handler {
    fn handle(&self, req: Request) -> Response;
}

impl Display for Foo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Foo")
    }
}

impl Config {
    pub fn new(name: String) -> Self { todo!() }
    fn validate(&self) -> bool { true }
}

pub fn process(input: &str) -> Result<String, Error> { todo!() }

pub mod utils;
mod internal;

macro_rules! my_macro { () => {}; }
]]
  local out = idx(src, "rust")
  has(out, {
    "module doc:",
    "imports:",
    "std::",
    "collections::HashMap",
    "io",
    "io::*",
    "fs",
    "net",
    "consts:",
    "MAX: usize",
    "static COUNTER: AtomicU64",
    "types:",
    "#[derive(Debug, Clone)]",
    "pub struct Config",
    "pub name: String",
    "pub struct Empty",
    "enum Color",
    "Red, Green",
    "type Result",
    "traits:",
    "pub Handler",
    "handle(&self, req: Request) -> Response",
    "impls:",
    "Display for Foo",
    "Config",
    "pub new(name: String) -> Self",
    "validate(&self) -> bool",
    "fns:",
    "pub process(input: &str)",
    "mod:",
    "pub utils, internal",
    "macros:",
    "my_macro!",
  })
end)

case("rust_truncated_children_tagged_dim", function()
  local src = "struct Big {\n"
  for i = 1, 10 do
    src = src .. "    f" .. i .. ": u8,\n"
  end
  src = src .. "}\n"
  local text, meta = idx_with_meta(src, "rust")
  has(text, { "[2 more truncated]" })
  helpers.assert_truncated_dim(text, meta)
end)

case("rust_struct_fields_no_ranged_meta", function()
  local src = "pub struct Point {\n    pub x: f64,\n    pub y: f64,\n    pub z: f64,\n}\n"
  local text, meta = idx_with_meta(src, "rust")
  helpers.assert_fields_no_ranged_meta(text, meta, "pub struct Point", { "pub x:", "pub y:", "pub z:" })
end)

case("rust_impl_methods_have_ranged_meta", function()
  local src = [[
pub struct Widget;

impl Widget {
    pub fn new() -> Self { Widget }
    pub fn render(&self) -> String { String::new() }
    fn internal(&mut self, flag: bool) {}
}
]]
  local text, meta = idx_with_meta(src, "rust")
  helpers.assert_ranged_meta(text, meta, { "pub new()", "pub render(&self)", "internal(&mut self" })
end)

case("rust_test_module_collapsed", function()
  local src =
    "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn it_works() { assert!(true); }\n}\n"
  local out = idx(src, "rust")
  has(out, { "tests:" })
  lacks(out, { "it_works" })
end)

case("rust_test_detection", function()
  local cases = {
    { src = "#[test]\nfn it_works() { assert!(true); }\n", test = true, name = "standalone_test" },
    { src = "#[tokio::test]\nasync fn my_test() {}\n", test = true, name = "tokio_test" },
    { src = "#[attested]\nfn foo() {}\n", test = false, name = "attested_not_test" },
    { src = "#[cfg(not(test))]\nfn real_fn() {}\n", test = false, name = "cfg_not_test" },
    { src = "#[my_crate::test_helper]\nfn setup() {}\n", test = false, name = "test_helper_not_test" },
  }
  for _, c in ipairs(cases) do
    local out = idx(c.src, "rust")
    if c.test then
      has(out, { "tests:" })
      lacks(out, { "fns:" })
    else
      has(out, { "fns:" })
      lacks(out, { "tests:" })
    end
  end
end)

case("rust_doc_comment_line_ranges", function()
  local cases = {
    { src = "/// Documented\n/// More docs\npub fn foo() {}\n", expected = "pub foo() [1-3]" },
    {
      src = "/// Doc\n#[derive(Debug)]\npub struct Bar {\n    pub x: i32,\n}\n",
      expected = "pub struct Bar [1-5]",
    },
    { src = "pub fn plain() {}\n", expected = "pub plain() [1]" },
    { src = "// regular comment\npub fn foo() {}\n", expected = "pub foo() [2]" },
  }
  for _, c in ipairs(cases) do
    local out = idx(c.src, "rust")
    has(out, { c.expected })
  end
end)
