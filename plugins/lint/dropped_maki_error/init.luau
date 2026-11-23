-- Flags a call to a `maki` function that returns `(nil, err)` on failure when the call is a statement
-- of its own, so nothing ever sees the error.
local query = require("@std/syntax/query")
local utils = require("@std/syntax/utils")

local fallible = require("@self/fallible")

local name = "dropped_maki_error"

local function maki_path(expr): string?
  local parts = {}
  while expr.tag == "indexname" do
    table.insert(parts, 1, expr.index.text)
    expr = expr.expression
  end
  if expr.tag ~= "global" or expr.name.text ~= "maki" then
    return nil
  end
  table.insert(parts, 1, "maki")
  return table.concat(parts, ".")
end

local function lint(cst, sourcepath)
  return query.findAllFromRoot(cst, utils.isStatExpr):mapToArray(function(stat)
    local call = utils.isExprCall(stat.expression)
    local path = call and maki_path(call.func)
    if not path or not fallible[path] then
      return nil
    end
    return {
      lintname = name,
      location = stat.location,
      message = `{path} returns (nil, err) on failure. Check the error, or assign the call to _ to drop it on purpose.`,
      severity = "warning",
      sourcepath = sourcepath,
    }
  end)
end

return table.freeze({ name = name, lint = lint })
