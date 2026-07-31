local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("json_top_level_and_nested_keys", function()
  local src = [==[
{
  "name": "app",
  "scripts": {
    "test": "cargo test",
    "build": "cargo build"
  }
}
]==]
  local out = idx(src, "json")
  has(out, {
    "consts:",
    '  "name" [2]',
    '  "scripts" [3-6]',
    '    "test" [4]',
    '    "build" [5]',
  })
  lacks(out, {
    "cargo test",
    "cargo build",
  })
end)

case("json_array_of_objects_contributes_keys", function()
  local out = idx('{"refs": [{"path": "./a"}, {"path": "./b"}]}', "json")
  has(out, {
    "consts:",
    '"refs"',
    '"path"',
  })
  lacks(out, {
    "./a",
    "./b",
  })
end)

case("json_top_level_array", function()
  local out = idx('[{"name": "a"}, {"name": "b"}]', "json")
  has(out, {
    "consts:",
    '"name"',
  })
end)

case("json_depth_is_bounded", function()
  local out = idx('{"a": {"b": {"c": 1}}}', "json")
  has(out, {
    '"a"',
    '"b"',
  })
  lacks(out, {
    '"c"',
  })
end)

case("json_children_are_capped", function()
  local parts = {}
  for i = 1, 30 do
    parts[#parts + 1] = '"node_modules/pkg' .. i .. '": {"version": "1.0.0"}'
  end
  local out = idx('{"packages": {' .. table.concat(parts, ",") .. "}}", "json")
  has(out, {
    '  "packages"',
    '    "node_modules/pkg1"',
    '    "node_modules/pkg8"',
    "    [22 more truncated]",
  })
  lacks(out, {
    '"node_modules/pkg9"',
  })
end)

case("json_ranged_meta", function()
  local out, meta = helpers.idx_with_meta('{"a": 1, "b": {"c": 2}}', "json")
  helpers.assert_ranged_meta(out, meta, {
    '"a"',
    '"b"',
  })
end)
