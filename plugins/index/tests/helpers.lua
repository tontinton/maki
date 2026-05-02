-- Shared helpers for the index plugin spec.
--
-- Per-language spec files in tests/lang/<lang>.lua require this module to get
-- a consistent test vocabulary. `case` wraps each block in pcall so a single
-- failure does not abort the rest of the suite; failures are collected here
-- and surfaced by `report()` from tests/spec.lua at the end.

local indexer = require("indexer")

local M = {}

local failures = {}

function M.case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

function M.idx(source, lang)
  local result, err = indexer.index_source(source, lang)
  assert(result, "index failed for " .. lang .. ": " .. tostring(err))
  return result
end

function M.has(output, needles)
  for _, n in ipairs(needles) do
    assert(output:find(n, 1, true), "missing '" .. n .. "'")
  end
end

function M.lacks(output, needles)
  for _, n in ipairs(needles) do
    assert(not output:find(n, 1, true), "unexpected '" .. n .. "'")
  end
end

function M.report()
  if #failures > 0 then
    error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
  end
end

return M
