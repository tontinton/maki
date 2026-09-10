local parse_sse_response = require("parse_sse")
local NO_RESULTS_MSG = "No search results found"
local API_KEY = "ydc-test-key"
local RATE_LIMIT_MSG = "rate limit exceeded"

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

case("providers_exa_shape", function()
  local p = providers.exa
  assert(p, "exa provider should exist")
  eq(p.endpoint(), "https://mcp.exa.ai/mcp")
  eq(p.tool, "web_search_exa")
  local args = p.arguments("rust async runtime", 5)
  eq(args.query, "rust async runtime")
  eq(args.numResults, 5)
  eq(args.type, "auto")
  eq(args.livecrawl, "fallback")
end)

case("providers_youcom_shape", function()
  local p = providers.youcom
  assert(p, "youcom provider should exist")
  eq(p.tool, "you-search")
  local args = p.arguments("rust async runtime", 5)
  eq(args.query, "rust async runtime")
  eq(args.count, 5)
  assert(args.numResults == nil, "youcom should not carry exa's numResults")
end)

case("providers_headers_follow_the_key", function()
  eq(providers.exa.headers(nil)["x-api-key"], nil)
  eq(providers.exa.headers(API_KEY)["x-api-key"], API_KEY)
  eq(providers.youcom.headers(nil)["Authorization"], nil)
  eq(providers.youcom.headers(API_KEY)["Authorization"], "Bearer " .. API_KEY)
end)

case("providers_youcom_endpoint_follows_the_key", function()
  eq(providers.youcom.endpoint(nil), "https://api.you.com/mcp?profile=free")
  eq(providers.youcom.endpoint(API_KEY), "https://api.you.com/mcp")
end)

case("providers_api_key_is_never_blank", function()
  -- An exported but empty variable is truthy in lua; treating it as a key
  -- would send "Bearer " and pick the authenticated endpoint over the free
  -- profile. Holds whether or not the vars are set in this environment.
  for name, p in pairs(providers) do
    local key = p.api_key()
    assert(key == nil or (type(key) == "string" and #key > 0), name .. " api_key should be nil or non-empty")
  end
end)

case("providers_youcom_response_is_parse_sse_compatible", function()
  -- The youcom MCP server answers tools/call with SSE data: lines whose
  -- result.content[1].text carries the JSON results, the same shape
  -- parse_sse_response already extracts for exa.
  local body = "event: message\n"
    .. make_sse('{"results":{"web":[{"url":"https://example.com","title":"Example","description":"A page"}]}}')
    .. "\n"
  local text = parse_sse_response(body)
  assert(text:find("example.com", 1, true), "youcom result payload should round-trip through parse_sse_response")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
