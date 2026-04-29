-- Shared test helpers for Lua plugin specs.
--
-- Provides a lightweight test harness: `case` wraps each block in pcall so a
-- single failure does not abort the rest of the suite. Failures are collected
-- and surfaced by `report()` at the end.

local M = {}

local failures = {}

function M.case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

function M.eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

local _tmpdir_counter = 0

function M.mktmpdir(prefix)
  _tmpdir_counter = _tmpdir_counter + 1
  local name = "/tmp/maki_"
    .. (prefix or "test")
    .. "_"
    .. tostring(os.clock()):gsub("%.", "")
    .. "_"
    .. _tmpdir_counter
  maki.fs.mkdir(name)
  return name
end

function M.rmtree(dir)
  maki.fs.rm(dir, { recursive = true })
end

function M.report()
  if #failures > 0 then
    error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
  end
end

return M
