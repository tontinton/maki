local parse_sse_response = require("parse_sse")
local NO_RESULTS_MSG = "No search results found"
local RATE_LIMIT_MSG = "rate limit exceeded"
local QUERY = "rust async runtime"

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

case("parse_sse_jsonrpc_error_is_error", function()
  local body = sse_line({ id = 1, error = { code = -32000, message = RATE_LIMIT_MSG } })
  local text, err = parse_sse_response(body)
  eq(text, nil, "a jsonrpc error must not read as no results")
  assert(err and err:find(RATE_LIMIT_MSG, 1, true), "should surface the server message, got: " .. tostring(err))
end)

case("parse_sse_tool_is_error_is_error", function()
  local body = sse_line({
    result = { isError = true, content = { { type = "text", text = RATE_LIMIT_MSG } } },
  })
  local text, err = parse_sse_response(body)
  eq(text, nil, "an isError result must not read as a search result")
  assert(err and err:find(RATE_LIMIT_MSG, 1, true), "should surface the tool message, got: " .. tostring(err))
end)

-- ── providers ──

local providers = require("providers")

-- init.lua reads these off whichever backend the user picked, so a new entry
-- that forgets one would only blow up mid request, against the live server.
case("providers_carry_what_init_consumes", function()
  for name, p in pairs(providers) do
    for _, field in ipairs({ "label", "endpoint", "env", "auth_header", "tool" }) do
      assert(type(p[field]) == "string", name .. " is missing " .. field)
    end
    eq(p.arguments(QUERY, 5).query, QUERY, name .. " must pass the query through")
  end
end)

-- Each server names the result count differently and quietly falls back to
-- its own default when the argument it does not know shows up.
case("providers_count_argument_matches_the_backend", function()
  eq(providers.exa.arguments(QUERY, 5).numResults, 5)
  eq(providers.youcom.arguments(QUERY, 5).count, 5)
  eq(providers.youcom.arguments(QUERY, 5).numResults, nil)
  eq(providers.kagi.arguments(QUERY, 5).limit, 5)
  eq(providers.kagi.arguments(QUERY, 5).numResults, nil)
end)

local KAGI_RESULT = table.concat({
  "### [Tokio vs Smol: The Battle That&#39;s Dividing Rust](https://youtu.be/x)",
  "**URL:** https://youtu.be/x",
  "**Published:** 2025-10-15",
  "",
  "<strong>Smol</strong> &amp; <strong>async</strong> &lt;3 &#x2014; done",
}, "\n")

case("kagi_clean_strips_highlights_url_lines_and_entities", function()
  eq(
    providers.kagi.clean(KAGI_RESULT),
    table.concat({
      "### [Tokio vs Smol: The Battle That's Dividing Rust](https://youtu.be/x)",
      "**Published:** 2025-10-15",
      "",
      "Smol & async <3 \u{2014} done",
    }, "\n")
  )
end)

-- Decoding is one pass, so an escaped entity in a snippet stays readable as
-- the entity it spells instead of collapsing into the character.
case("kagi_clean_decodes_once", function()
  eq(providers.kagi.clean("&amp;#39; &amp;lt;strong&amp;gt;"), "&#39; &lt;strong&gt;")
end)

case("kagi_clean_keeps_unknown_and_out_of_range_entities", function()
  eq(providers.kagi.clean("&bogus; &#x110000;"), "&bogus; &#x110000;")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
