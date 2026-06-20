local helpers = require("tests.helpers")
local case = helpers.case
local idx_with_meta = helpers.idx_with_meta

local function meta_count(meta)
  local n = 0
  for _ in pairs(meta) do
    n = n + 1
  end
  return n
end

case("core_empty_and_trivial_source_returns_empty_meta", function()
  local cases = {
    { "", "empty" },
    { "   \n  \n\n", "whitespace-only" },
    { "// just a comment\n/* block comment */\n", "comments-only" },
  }
  for _, c in ipairs(cases) do
    local text, meta = idx_with_meta(c[1], "rust")
    assert(text == "", "expected empty text for " .. c[2] .. ", got: '" .. text .. "'")
    assert(meta_count(meta) == 0, "expected empty meta for " .. c[2])
  end
end)

case("core_meta_every_section_has_tag", function()
  local src = [[
use std::io;

const MAX: usize = 42;

pub struct Foo { x: i32 }

pub trait Bar { fn bar(&self); }

impl Foo { fn baz(&self) {} }

pub fn run() {}

pub mod utils;

macro_rules! m { () => {}; }

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
]]
  local text, meta = idx_with_meta(src, "rust")
  local lines = helpers.split_lines(text)
  local section_headers = {
    "imports:",
    "consts:",
    "types:",
    "traits:",
    "impls:",
    "fns:",
    "mod:",
    "macros:",
    "tests:",
  }
  for _, hdr in ipairs(section_headers) do
    local found = false
    for i, line in ipairs(lines) do
      if line:find(hdr, 1, true) then
        found = true
        local m = meta[i]
        assert(m, "section header '" .. hdr .. "' at line " .. i .. " has no meta")
        assert(
          m.tag == "section",
          "section header '" .. hdr .. "' tag is '" .. tostring(m.tag) .. "', expected 'section'"
        )
      end
    end
    assert(found, "section header '" .. hdr .. "' not found in output")
  end
end)
