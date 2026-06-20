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

function M.idx_with_meta(source, lang)
  local result, meta = indexer.index_source(source, lang)
  assert(result, "index failed for " .. lang .. ": " .. tostring(meta))
  return result, meta
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

function M.split_lines(text)
  local lines = {}
  for line in (text:gsub("\n+$", "") .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end
  return lines
end

function M.assert_ranged_meta(text, meta, needles)
  local lines = M.split_lines(text)
  local found = 0
  for i, line in ipairs(lines) do
    for _, needle in ipairs(needles) do
      if line:find(needle, 1, true) then
        local m = meta and meta[i]
        assert(m, "missing meta for line " .. i .. ": " .. line)
        assert(m.body, "missing body in meta for line " .. i)
        assert(m.range, "missing range in meta for line " .. i)
        found = found + 1
        break
      end
    end
  end
  assert(found >= #needles, "expected " .. #needles .. " ranged lines, got " .. found)
end

function M.assert_truncated_dim(text, meta)
  local lines = M.split_lines(text)
  for i, line in ipairs(lines) do
    if line:find("more truncated]", 1, true) then
      local m = meta and meta[i]
      assert(m, "truncated line " .. i .. " has no meta")
      assert(m.tag == "dim", "truncated line " .. i .. " tag is '" .. tostring(m.tag) .. "', expected 'dim'")
      return
    end
  end
  error("no truncated line found in output")
end

function M.assert_fields_no_ranged_meta(text, meta, struct_needle, field_needles)
  local lines = M.split_lines(text)
  local struct_found = false
  for i, line in ipairs(lines) do
    local m = meta and meta[i]
    if line:find(struct_needle, 1, true) then
      struct_found = true
      assert(m and m.range, struct_needle .. " line should have range in meta")
    end
    for _, f in ipairs(field_needles) do
      if line:find(f, 1, true) then
        assert(not (m and m.body and m.range), "field line should not have ranged meta: " .. line)
      end
    end
  end
  assert(struct_found, struct_needle .. " not found in output")
end

function M.report()
  if #failures > 0 then
    error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
  end
end

return M
