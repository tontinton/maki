-- Layer-agnostic helpers for the Bazel indexer.
--
-- Imported by both extractors and analyze.lua. Has no dependencies on
-- ast.lua or render.lua, so it can be required from any layer.

local M = {}

-- Max length of a rendered assignment value before "[truncated]" is appended.
-- Shared by the .bzl and BUILD extractors so binding lines stay scannable.
M.BINDING_TRUNCATE = 60

function M.quote(text)
  return '"' .. text .. '"'
end

-- Render an analyze value record as it would appear in the index: quoted if
-- the source was a string literal, raw otherwise.
function M.render_value(v)
  return v.quoted and M.quote(v.text) or v.text
end

-- Match all-uppercase identifiers (with optional leading underscores) such
-- as MY_CONST or _PRIVATE. Used by module.lua to recognize Bzlmod constants.
function M.is_constant_name(name)
  return name and name:match("^_*[A-Z][A-Z0-9_]*$") ~= nil
end

-- Render a use_repo() argument as a repo label. The argument here is an
-- entry from a CallRecord's `args` list (not a value record), because the
-- keyword-argument case must read the keyword *name* as the alias.
--   keyword arg `alias = "real"` -> '"@alias"'
--   positional string "foo"     -> '"@foo"'
--   positional anything else    -> the argument's compacted text (e.g. a bare
--                                  identifier FOO renders as 'FOO', since it
--                                  cannot be resolved at index time)
function M.arg_repo_label(entry)
  if entry.kind == "keyword" then
    return M.quote("@" .. entry.name)
  end

  local v = entry.value

  if v.kind == "string" then
    return M.quote("@" .. v.text)
  end

  return v.text
end

return M
