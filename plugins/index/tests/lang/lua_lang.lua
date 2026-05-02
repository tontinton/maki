local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("lua_all_sections", function()
  local src = [==[
local json = require("cjson")
local x, y = require("foo"), require("bar")
require("init")

local MAX_SIZE = 100
local min_val = 10

function process(data, opts)
  return data
end

function M.helper(x)
end

function M:method(self, val)
end
]==]
  local out = idx(src, "lua_lang")
  has(out, {
    "imports:",
    "cjson",
    "foo",
    "bar",
    "init",
    "consts:",
    "MAX_SIZE = 100",
    "fns:",
    "process(data, opts)",
    "M.helper(x)",
    "M:method(self, val)",
  })
end)
