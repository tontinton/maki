local parse_sse_response = require("parse_sse")
local NO_RESULTS_MSG = require("constants").NO_RESULTS_MSG
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

case("parse_sse_null_error_is_not_an_error", function()
  local body = 'data: {"jsonrpc":"2.0","error":null,"result":{"isError":null,"content":[{"text":"found"}]}}'
  eq(parse_sse_response(body), "found")
end)

case("parse_sse_non_object_json_is_error", function()
  local text, err = parse_sse_response("data: null")
  eq(text, nil)
  assert(err and err:find("SSE JSON parse error", 1, true), "got: " .. tostring(err))
end)

-- ── providers ──

local providers = require("providers")

-- init.lua reads these off whichever backend the user picked, so a new entry
-- that forgets one would only blow up mid request, against the live server.
case("providers_carry_what_init_consumes", function()
  for name, p in pairs(providers) do
    assert(type(p.label) == "string", name .. " is missing label")
    assert(type(p.env) == "string", name .. " is missing env")
    if p.transport ~= nil then
      eq(p.transport, "get", name .. " has an unknown transport")
    else
      for _, field in ipairs({ "endpoint", "auth_header", "tool" }) do
        assert(type(p[field]) == "string", name .. " is missing " .. field)
      end
      eq(p.arguments(QUERY, 5).query, QUERY, name .. " must pass the query through")
    end
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

-- Video ahead of search, so only the category order can put search first.
local KAGI_JSON = '{"video":[{"title":"Tokio vs Smol","url":"https://youtu.be/x","published":"2025-10-15",'
  .. '"snippet":"Smol & async"}],"search":[{"title":"smol","url":"https://docs.rs/smol",'
  .. '"snippet":"A small and fast async runtime."},{"title":"smol-rs","url":"https://github.com/smol-rs/smol",'
  .. '"snippet":"Contribute to smol-rs/smol."}]}'

case("kagi_format_lists_every_category_in_kagimcp_order", function()
  eq(
    providers.kagi.format(KAGI_JSON),
    table.concat({
      "1. smol",
      "   https://docs.rs/smol",
      "   A small and fast async runtime.",
      "2. smol-rs",
      "   https://github.com/smol-rs/smol",
      "   Contribute to smol-rs/smol.",
      "3. Tokio vs Smol (2025-10-15)",
      "   https://youtu.be/x",
      "   Smol & async",
    }, "\n")
  )
end)

case("kagi_format_empty_object_is_no_results", function()
  eq(providers.kagi.format("{}"), NO_RESULTS_MSG)
end)

case("kagi_format_passes_through_what_it_cannot_read", function()
  local markdown = "## Search Results\n\n### [smol](https://docs.rs/smol)"
  eq(providers.kagi.format(markdown), markdown)
  eq(providers.kagi.format(NO_RESULTS_MSG), NO_RESULTS_MSG)
  local renamed = '{"web":[{"title":"smol","url":"https://docs.rs/smol"}]}'
  eq(providers.kagi.format(renamed), renamed)
end)

-- ── searxng (plain GET /search?format=json backend) ──

local searxng = require("searxng")

case("searxng_encode_param_escapes_query_chars", function()
  eq(searxng.encode_param("rust & go?"), "rust%20%26%20go%3F")
  eq(searxng.encode_param("a-b_c.~1"), "a-b_c.~1")
  -- '+' must not survive as a space alias, and reserved delimiters go too.
  eq(searxng.encode_param("a+b=c#d"), "a%2Bb%3Dc%23d")
  -- Lua patterns are byte-based, so each UTF-8 byte is escaped on its own.
  eq(searxng.encode_param("héllo"), "h%C3%A9llo")
end)

case("searxng_resolve_base_prefers_option_over_env", function()
  eq(searxng.resolve_base("https://opt.example", "https://env.example"), "https://opt.example")
end)

case("searxng_resolve_base_blank_option_falls_back_to_env", function()
  eq(searxng.resolve_base("", "https://env.example"), "https://env.example")
  -- A padded blank is truthy in lua but means "unset"; it must fall through.
  eq(searxng.resolve_base("  ", "https://env.example"), "https://env.example")
end)

case("searxng_resolve_base_trims_the_option", function()
  eq(searxng.resolve_base("  https://opt.example ", nil), "https://opt.example")
end)

case("searxng_resolve_base_both_unset_is_blank", function()
  eq(searxng.resolve_base(nil, nil), "")
  eq(searxng.resolve_base(" ", ""), "")
end)

case("searxng_search_url_builds_and_trims", function()
  eq(searxng.search_url("https://s.example/", "rust", nil), "https://s.example/search?q=rust&format=json")
  eq(
    searxng.search_url("  https://s.example// ", "a b", "week"),
    "https://s.example/search?q=a%20b&format=json&time_range=week"
  )
  local url, err = searxng.search_url("", "q", nil)
  eq(url, nil, "a blank base must not build a URL")
  assert(err:find("SEARXNG_BASE_URL", 1, true), err)
end)

case("searxng_format_results_maps_fields", function()
  local text = searxng.format_results({
    results = {
      { title = "Rust", url = "https://rust-lang.org", content = "a language" },
      { url = "https://anon.example" },
    },
  }, 5)
  assert(text:find("1. Rust\n", 1, true), text)
  assert(text:find("   https://rust-lang.org\n   a language", 1, true), text)
  assert(text:find("2. https://anon.example", 1, true), "missing title falls back to url")
end)

case("searxng_format_results_caps_at_num_results", function()
  local data = { results = {} }
  for i = 1, 10 do
    table.insert(data.results, { title = "t" .. i, url = "https://example.com/" .. i })
  end
  local text = searxng.format_results(data, 3)
  assert(text:find("3. t3", 1, true), text)
  assert(not text:find("4. t4", 1, true), "num_results must cap the list")
end)

case("searxng_format_results_skips_entries_without_url", function()
  local text = searxng.format_results(
    { results = {
      { title = "no url" },
      { title = "ok", url = "https://ok.example" },
    } },
    5
  )
  assert(not text:find("no url"), text)
  -- Unanchored count: '^' would match only the string start and pass vacuously.
  eq(select(2, text:gsub("%d+%. ", "")), 1)
end)

case("searxng_format_results_empty_is_no_results", function()
  eq(searxng.format_results({ results = {} }, 5), NO_RESULTS_MSG)
end)

case("searxng_format_results_bad_payload_is_error", function()
  local text, err = searxng.format_results({ status = "boom" }, 5)
  eq(text, nil, "a payload without a results array must not read as results")
  assert(err and #err > 0, "bad payload must carry a message")
  -- Rate-limited instances answer 200 with {"error": ...}; losing the cause
  -- would leave the model retrying blind.
  local _, why = searxng.format_results({ error = "Too many requests" }, 5)
  assert(why:find("Too many requests", 1, true), why)
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
