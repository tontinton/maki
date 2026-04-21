local parse_sse_response = require("parse_sse")
local NO_RESULTS_MSG = "No search results found"

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

local function make_sse(text)
  return "data: "
    .. maki.json.encode({
      jsonrpc = "2.0",
      result = {
        content = { { type = "text", text = text } },
      },
    })
end

local function sse_line(obj)
  return "data: " .. maki.json.encode(obj)
end

-- ── parse_sse_response ──

case("parse_sse_extracts_text", function()
  local body = "event: message\n" .. make_sse("Rust is a systems language") .. "\n"
  local result = parse_sse_response(body)
  eq(result, "Rust is a systems language")
end)

case("parse_sse_first_data_line_wins", function()
  local body = make_sse("first") .. "\n" .. make_sse("second") .. "\n"
  eq(parse_sse_response(body), "first")
end)

case("parse_sse_empty_body", function()
  eq(parse_sse_response(""), NO_RESULTS_MSG)
end)

case("parse_sse_empty_content_array", function()
  local body = sse_line({ result = { content = {} } })
  eq(parse_sse_response(body), NO_RESULTS_MSG)
end)

case("parse_sse_missing_content_key", function()
  local body = sse_line({ result = {} })
  eq(parse_sse_response(body), NO_RESULTS_MSG)
end)

case("parse_sse_empty_text_falls_through", function()
  local body = make_sse("") .. "\n" .. make_sse("actual result")
  eq(parse_sse_response(body), "actual result")
end)

case("parse_sse_malformed_json_is_error", function()
  local text, err = parse_sse_response("data: {not valid json}")
  eq(text, nil, "should return nil on malformed JSON")
  assert(err and err:find("SSE JSON parse error"), "should have error message, got: " .. tostring(err))
end)

case("parse_sse_non_string_text_falls_through", function()
  local body = sse_line({ result = { content = { { type = "text", text = 42 } } } })
  eq(parse_sse_response(body), NO_RESULTS_MSG)
end)

case("parse_sse_skips_non_data_lines_finds_valid", function()
  local body = "event: message\nid: 1\nretry: 1000\n" .. make_sse("found it") .. "\n"
  eq(parse_sse_response(body), "found it")
end)

case("parse_sse_data_with_no_result_key_falls_through", function()
  local body = sse_line({ id = 1, method = "something" }) .. "\n" .. make_sse("actual")
  eq(parse_sse_response(body), "actual")
end)

case("parse_sse_only_no_result_lines_returns_no_results", function()
  local body = sse_line({ id = 1, method = "something" })
  eq(parse_sse_response(body), NO_RESULTS_MSG)
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
