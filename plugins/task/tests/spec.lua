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
  local expected = { "resolve_model", "system_prompt", "tools", "call_tool", "session" }
  for _, fn_name in ipairs(expected) do
    eq(type(maki.agent[fn_name]), "function", "maki.agent." .. fn_name .. " must be a function")
  end
end)

case("schema_validator_compiles_and_validates", function()
  local validator, err = maki.json.schema_validator({
    type = "object",
    properties = { answer = { type = "string" } },
    required = { "answer" },
  })
  eq(err, nil, "valid schema must compile")
  eq(validator:validate({ answer = "42" }), nil, "matching value must produce no errors")
  local errors = validator:validate({ answer = 42 })
  assert(type(errors) == "table" and #errors > 0, "mismatch must produce error list")
end)

case("schema_validator_rejects_bad_schema", function()
  local validator, err = maki.json.schema_validator({ type = 42 })
  eq(validator, nil, "bad schema must not compile")
  assert(err ~= nil, "bad schema must return an error")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
