local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

case("maki_agent_has_expected_functions", function()
  assert(type(maki.agent) == "table", "maki.agent must be a table")
  local expected = { "resolve_model", "system_prompt", "tools", "run" }
  for _, fn_name in ipairs(expected) do
    eq(type(maki.agent[fn_name]), "function", "maki.agent." .. fn_name .. " must be a function")
  end
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
