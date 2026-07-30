local h = require("memory_helpers")

local fnv1a_64 = h.fnv1a_64
local project_id = h.project_id
local safe_resolve = h.safe_resolve
local normalize_tag = h.normalize_tag
local parse_frontmatter = h.parse_frontmatter
local extract_tags = h.extract_tags
local tags_for_file = h.tags_for_file
local format_tag_line = h.format_tag_line
local encode_frontmatter = h.encode_frontmatter
local format_list = h.format_list
local format_read = h.format_read
local cap_read_output = h.cap_read_output
local validate_write_tags = h.validate_write_tags
local validate_write_size = h.validate_write_size
local validate_input = h.validate_input

local NO_MATCH_MSG = "no memory files matched any of the given tags; use `list` to see available tags"

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

local _tmpdir_counter = 0
-- time + clock + counter keeps dirs unique even across parallel test runs
local function mktmpdir()
  _tmpdir_counter = _tmpdir_counter + 1
  local name = "/tmp/maki_spec_" .. os.time() .. "_" .. tostring(os.clock()):gsub("%.", "") .. "_" .. _tmpdir_counter
  maki.fs.mkdir(name)
  return name
end

-- hands fn a fresh tmpdir and removes it even when the case fails
local function case_tmp(name, fn)
  case(name, function()
    local dir = mktmpdir()
    local ok, err = pcall(fn, dir)
    maki.fs.rm(dir, { recursive = true })
    if not ok then
      error(err, 0)
    end
  end)
end

local function write_mem(dir, name, tags, body)
  maki.fs.write(maki.fs.joinpath(dir, name), encode_frontmatter(tags) .. body)
end

case("fnv1a_known_vectors", function()
  local vectors = {
    { "", "cbf29ce484222325" },
    { "a", "af63dc4c8601ec8c" },
    { "/home/user/my-project", "fc6e8b528feefa1c" },
  }
  for _, v in ipairs(vectors) do
    eq(fnv1a_64(v[1]), v[2], "input: " .. ("%q"):format(v[1]))
  end
  local high = fnv1a_64(string.rep("\xff", 64))
  assert(#high == 16 and high:match("^%x+$"), "high bytes must still hash to 16 hex chars")
end)

case("safe_resolve_rejects_bad_paths", function()
  local bad = {
    { nil, "required" },
    { "", "required" },
    { "/etc/passwd", "must be relative" },
    { "bad\0path", "must be relative" },
    { "..", "traversal" },
    { "../escape", "traversal" },
    { "a/../../escape", "traversal" },
    { "inside/../../../etc/shadow", "traversal" },
    { "C:\\foo", "must be relative" },
    { "c:foo", "must be relative" },
    { "\\escape", "must be relative" },
  }
  for _, v in ipairs(bad) do
    local _, err = safe_resolve("/tmp/mem", v[1])
    assert(
      err and err:find(v[2]),
      "input " .. tostring(v[1]) .. " should match '" .. v[2] .. "', got: " .. tostring(err)
    )
  end
end)

case("safe_resolve_accepts_good_paths", function()
  local s = "[/\\\\]"
  local good = {
    { "notes.md", "notes%.md" },
    { "sub/deep/notes.md", "sub" .. s .. "deep" .. s .. "notes%.md" },
    { "./notes.md", "notes%.md" },
  }
  for _, v in ipairs(good) do
    local p, err = safe_resolve("/tmp/mem", v[1])
    assert(p, "input " .. v[1] .. " should be accepted, got error: " .. tostring(err))
    assert(p:find(v[2]), "result should match pattern '" .. v[2] .. "', got: " .. p)
  end
end)

case("project_id", function()
  local id = project_id("/home/user/my-project")
  assert(id:match("^my%-project%-%x+$"), "should be basename-hex, got: " .. id)
  eq(#id:match("%-(%x+)$"), 16, "hash should be 16 hex chars")

  local root_id = project_id("/")
  assert(root_id:match("^root%-"), "/ should use 'root' as basename")

  assert(project_id("/home/alice/myapp") ~= project_id("/home/bob/myapp"), "same basename, different paths")
end)

case_tmp("format_list_empty_or_missing_says_no_memories", function(dir)
  eq(format_list(dir), h.NO_MEMORIES_MSG)
  eq(format_list(dir .. "_missing"), h.NO_MEMORIES_MSG)
end)

case_tmp("format_list_groups_sorted_by_freq_no_bodies", function(dir)
  write_mem(dir, "a.md", { "rare" }, "A body")
  write_mem(dir, "b.md", { "common" }, "B")
  write_mem(dir, "c.md", { "common" }, "C")
  write_mem(dir, "d.md", { "common" }, "D")

  local result, err = format_list(dir)
  eq(err, nil)
  local common_pos = result:find("common %(3%)")
  local rare_pos = result:find("rare %(1%)")
  assert(common_pos and rare_pos and common_pos < rare_pos, "groups sorted by frequency")
  assert(result:find("  %- b%.md %(%d+ bytes%)"), "files listed under their tag")
  assert(not result:find("A body"), "index has no bodies")
end)

case_tmp("format_list_stem_pseudo_tag", function(dir)
  maki.fs.write(maki.fs.joinpath(dir, "notes.md"), "no frontmatter")
  assert(format_list(dir):find("notes %(1%)"), "untagged file grouped under its stem")
  local filtered, err = format_list(dir, { "notes" })
  eq(err, nil)
  assert(filtered:find("notes%.md %(%d+ bytes%)"), "stem works as a filter tag too")
end)

case_tmp("format_list_filters_by_tag_union", function(dir)
  write_mem(dir, "a.md", { "auth" }, "A")
  write_mem(dir, "b.md", { "sessions" }, "B")
  write_mem(dir, "c.md", { "storage" }, "C")

  local result, err = format_list(dir, { "auth", "sessions" })
  eq(err, nil)
  assert(result:find("a%.md") and result:find("b%.md"), "union of requested tags")
  assert(not result:find("c%.md"), "other tags filtered out")

  eq(format_list(dir, { "nope" }), NO_MATCH_MSG)
end)

case_tmp("format_list_invalid_tags", function(dir)
  write_mem(dir, "a.md", { "auth" }, "A")

  local result, err = format_list(dir, { "!!!", "", "  " })
  eq(result, nil)
  assert(err:find("no valid tags") and err:find("!!!", 1, true), "all-invalid errors and names the raw values")

  local partial, perr = format_list(dir, { "auth", "!!!" })
  eq(perr, nil)
  local wpos = partial:find("warning: ignored invalid tag")
  local bpos = partial:find("a%.md")
  assert(wpos and bpos and wpos < bpos, "partial invalid warns first, then lists matches")
  assert(partial:find("!!!", 1, true), "warning names the rejected tag")
end)

case_tmp("format_list_prune_advisory_only_unfiltered_over_cap", function(dir)
  for i = 1, h.MAX_TAGS + 1 do
    write_mem(dir, "f" .. i .. ".md", { "tag" .. i }, "body" .. i)
  end
  assert(format_list(dir):find("to stay under " .. h.MAX_TAGS .. " tags"), "advisory when over the tag cap")
  assert(not format_list(dir, { "tag1" }):find("stay under"), "no advisory on a filtered list")
end)

case_tmp("format_list_caps_oversized_output", function(dir)
  local stem = string.rep("a", 180)
  for i = 1, 130 do
    maki.fs.write(maki.fs.joinpath(dir, stem .. i .. ".md"), "x")
  end
  local result = format_list(dir)
  assert(#result <= h.MAX_FILE_BYTES + 200, "list output stays near the cap")
  assert(result:find("truncated at", 1, true), "oversized list is capped")
  assert(result:find(h.CAP_HINT_FILTER, 1, true), "cap hint suggests tag filtering")
end)

case("format_rejected", function()
  eq(h.format_rejected({}), nil)
  eq(h.format_rejected(nil), nil)
  assert(h.format_rejected({ "bad!tag", "" }):find("bad!tag"), "lists rejected tag names")
  local w = h.format_rejected({ string.rep("x", 200) })
  eq(#w, 64 + 3, "long values truncated to 64 chars plus ellipsis")
  eq(w:sub(-3), "...")
end)

case("normalize_tag", function()
  eq(normalize_tag("Auth Flow"), "auth_flow")
  eq(normalize_tag("auth-flow"), "auth_flow")
  eq(normalize_tag("  AUTH_FLOW  "), "auth_flow")
  eq(normalize_tag("auth--flow"), "auth_flow", "repeated separators collapse")
  eq(normalize_tag("auth!flow"), "auth_flow", "punctuation coerced")
  eq(normalize_tag("a,b"), "a_b")
  eq(normalize_tag(nil), nil)
  eq(normalize_tag(""), nil)
  eq(normalize_tag("   "), nil)
  eq(normalize_tag("!?"), nil, "nothing salvageable returns nil")
  eq(normalize_tag(string.rep("a", 65)), string.rep("a", 64), "over 64 chars truncated")
  eq(normalize_tag(string.rep("a", 64)), string.rep("a", 64), "exactly 64 ok")
end)

case("normalize_tags_dedupes_and_reports", function()
  local out, rejected = h.normalize_tags({ "Auth Flow", "auth_flow", "Sessions", "", "auth-flow" })
  eq(#out, 2, "variants dedupe to one normalized form")
  eq(out[1], "auth_flow")
  eq(out[2], "sessions")
  eq(#rejected, 1, "empty string rejected")

  local out2, rejected2, coerced = h.normalize_tags({ "valid", "bad!tag", "!", "" })
  eq(#out2, 2)
  eq(out2[2], "bad_tag")
  eq(#rejected2, 2)
  eq(#coerced, 1)
  eq(coerced[1], "bad!tag -> bad_tag")

  eq(#h.normalize_tags({}), 0)
end)

case("validate_write_size", function()
  local max = h.MAX_FILE_BYTES
  eq(validate_write_size(string.rep("a", max)), nil, "content exactly at cap accepted")
  local err = validate_write_size(string.rep("x", max + 1))
  assert(err and err:find("exceeds " .. max) and err:find("got " .. (max + 1)), "over cap rejected with both sizes")
end)

case("validate_write_tags", function()
  local tags, err, note = validate_write_tags({ "auth_flow", "sessions" })
  eq(err, nil)
  eq(tags[1], "auth_flow")
  eq(tags[2], "sessions")
  eq(note, nil, "no note when nothing coerced")

  local ctags, cerr, cnote = validate_write_tags({ "auth/login" })
  eq(cerr, nil)
  eq(ctags[1], "auth_login")
  eq(cnote, "normalized: auth/login -> auth_login")

  local etags, eerr = validate_write_tags({})
  eq(eerr, nil, "empty tag list is valid, stem fallback")
  eq(#etags, 0)

  local rtags, rerr = validate_write_tags({ "good", "!", "??" })
  eq(rtags, nil, "any unsalvageable tag rejects the write")
  assert(rerr:find("!, ??", 1, true) and not rerr:find("good", 1, true), "error lists only the bad tags")
end)

case("parse_frontmatter_extracts_tags_and_body", function()
  local fm, body = parse_frontmatter("---\ntags:\n  - auth\n  - sessions\n---\n# Body\nText")
  eq(fm.tags[1], "auth")
  eq(fm.tags[2], "sessions")
  eq(body, "# Body\nText")
end)

case("parse_frontmatter_falls_back_to_whole_content", function()
  local inputs = {
    "Just content",
    "---\ntags:\n  - a\nbody without close",
    "---\n---\nbody",
  }
  for _, input in ipairs(inputs) do
    local fm, body = parse_frontmatter(input)
    eq(body, input, "invalid frontmatter keeps content intact: " .. ("%q"):format(input:sub(1, 24)))
    eq(next(fm), nil)
  end
end)

case("parse_frontmatter_malformed_yaml_keeps_body", function()
  local fm, body = parse_frontmatter("---\ntags: [unterminated\n---\nbody")
  eq(body, "body", "body survives a YAML decode failure")
  eq(extract_tags(fm), nil)
end)

case("parse_frontmatter_empty_tags_and_mid_body_rule", function()
  local fm, body = parse_frontmatter("---\ntags:\n  - a\n---\npara1\n---\npara2")
  eq(body, "para1\n---\npara2", "mid-body thematic rule is not a closing fence")
  eq(fm.tags[1], "a")

  local fm2, body2 = parse_frontmatter("---\ntags: []\n---\nbody text")
  eq(body2, "body text")
  eq(extract_tags(fm2), nil, "empty tags list extracts to nil, stem fallback")
end)

case("extract_tags", function()
  local tags = extract_tags({ tags = { "Auth Flow", "auth_flow", "Sessions", "" } })
  eq(#tags, 2, "normalized and deduped")
  eq(tags[1], "auth_flow")
  eq(tags[2], "sessions")
  eq(extract_tags({ tags = "single_tag" })[1], "single_tag", "bare string coerced to list")
  eq(extract_tags({}), nil)
  eq(extract_tags(nil), nil)
  eq(extract_tags({ tags = 42 }), nil, "non-table/string ignored")
end)

case("tags_for_file_uses_frontmatter", function()
  local tags = tags_for_file("arch.md", "---\ntags:\n  - architecture\n  - rust\n---\nMicroservices")
  eq(#tags, 2)
  eq(tags[1], "architecture")
  eq(tags[2], "rust")
end)

case("tags_for_file_stem_fallbacks", function()
  local vectors = {
    { "architecture.md", "no frontmatter", "architecture" },
    { "config.local.md", "no tags", "config_local" },
    { ".bashrc", "no tags", "bashrc" },
    { "...md", "no tags", "untagged" },
    { "notes.md", "---\ntags: []\n---\nbody", "notes" },
    { "deep/nested/Notes.md", "x", "notes" },
  }
  for _, v in ipairs(vectors) do
    local tags = tags_for_file(v[1], v[2])
    eq(#tags, 1, v[1])
    eq(tags[1], v[3], v[1])
  end
end)

case("tags_for_file_propagates_read_errors", function()
  local tags, err = tags_for_file("Notes.md", nil)
  eq(tags[1], "notes", "stem fallback still works without content")
  eq(err, "read error")

  local _, err2 = tags_for_file("Notes.md", nil, "disk error")
  eq(err2, "disk error", "explicit read error wins over the default")
end)

case_tmp("format_tag_line_sorted_by_freq", function(dir)
  eq(format_tag_line(dir, 50), nil, "empty dir renders no line")

  write_mem(dir, "a.md", { "rare" }, "A")
  write_mem(dir, "b.md", { "common" }, "B")
  write_mem(dir, "c.md", { "common" }, "C")
  write_mem(dir, "d.md", { "common" }, "D")
  maki.fs.write(maki.fs.joinpath(dir, "untagged.md"), "no tags")

  local line = format_tag_line(dir, 50)
  local common_pos = line:find("common", 1, true)
  local rare_pos = line:find("rare", 1, true)
  assert(common_pos and rare_pos and common_pos < rare_pos, "most used tag first")
  assert(line:find("untagged", 1, true), "stem pseudo-tag included")
end)

case_tmp("format_tag_line_truncates_above_cap", function(dir)
  for i = 1, 5 do
    write_mem(dir, "f" .. i .. ".md", { "tag" .. i }, tostring(i))
  end
  local line = format_tag_line(dir, 3)
  assert(line:find("^tag1, tag2, tag3 "), "first three tags shown in order")
  assert(line:find("2 tags omitted") and line:find("use `list` to see all"), "omission hint with count")
end)

local function with_read_stub(stub, fn)
  local orig = maki.fs.read
  maki.fs.read = stub
  local ok, err = pcall(fn)
  maki.fs.read = orig
  if not ok then
    error(err, 0)
  end
end

case_tmp("grouped_tags_serves_unchanged_files_from_cache", function(dir)
  write_mem(dir, "a.md", { "auth" }, "body")
  local groups = h.grouped_tags(dir)
  eq(groups[1].tag, "auth")

  with_read_stub(function()
    error("cache miss: unchanged file was re-read")
  end, function()
    local cached = h.grouped_tags(dir)
    eq(cached[1].tag, "auth")
    eq(#cached[1].files, 1)
  end)
end)

case_tmp("grouped_tags_cache_invalidated_when_file_changes", function(dir)
  write_mem(dir, "a.md", { "auth" }, "body")
  eq(h.grouped_tags(dir)[1].tag, "auth")

  write_mem(dir, "a.md", { "storage" }, "a longer body so size differs too")
  local groups = h.grouped_tags(dir)
  eq(groups[1].tag, "storage", "rewrite must evict the cached tags")
  eq(#groups, 1)
end)

case_tmp("grouped_tags_never_caches_unreadable_files", function(dir)
  write_mem(dir, "a.md", { "auth" }, "body")

  with_read_stub(function()
    return nil, "boom"
  end, function()
    local groups, warnings = h.grouped_tags(dir)
    eq(#groups, 0, "unreadable file forms no group")
    eq(#warnings, 1)
    assert(warnings[1]:find("boom"), "warning carries the read error")
  end)

  local groups, warnings = h.grouped_tags(dir)
  eq(groups[1].tag, "auth", "transient failure must not pin stale stem tags")
  eq(#warnings, 0)
end)

case_tmp("format_list_and_tag_line_warn_when_all_files_unreadable", function(dir)
  write_mem(dir, "a.md", { "auth" }, "body")
  with_read_stub(function()
    return nil, "boom"
  end, function()
    local out = format_list(dir)
    assert(out:find("unreadable memory files: a.md: boom", 1, true), "list must not hide unreadable files")
    assert(out:find(h.NO_MEMORIES_MSG, 1, true))
    eq(format_tag_line(dir, 50), "(unreadable: 1)", "tag line surfaces unreadable-only dirs")
  end)
end)

case_tmp("format_read_matches_on_cached_tags_and_reads_fresh_content", function(dir)
  write_mem(dir, "a.md", { "auth" }, "old")
  eq(h.grouped_tags(dir)[1].tag, "auth")

  with_read_stub(function()
    return "fresh body without frontmatter"
  end, function()
    local out = format_read(dir, { "auth" })
    assert(
      out:find("fresh body without frontmatter", 1, true),
      "match must come from cached tags, content from a fresh read"
    )
  end)
end)

case_tmp("format_read_warns_when_matched_file_unreadable_at_read_time", function(dir)
  write_mem(dir, "a.md", { "auth" }, "body")
  eq(h.grouped_tags(dir)[1].tag, "auth")

  with_read_stub(function()
    return nil, "boom"
  end, function()
    local out = format_read(dir, { "auth" })
    assert(out:find("unreadable memory files: a.md: boom", 1, true), "content read failure surfaces as warning")
    assert(out:find(NO_MATCH_MSG, 1, true), "no bodies means the no-match message")
  end)
end)

case("cap_read_output_caps_and_picks_hint", function()
  local max = h.MAX_FILE_BYTES
  local small = string.rep("a", 100)
  eq(cap_read_output(small, h.CAP_HINT_REWRITE), small, "under the cap returned unchanged")

  local big = string.rep("x", max + 50)
  local out = cap_read_output(big, h.CAP_HINT_REWRITE)
  assert(out:find("truncated at " .. max .. " bytes", 1, true), "marker renders the cap")
  assert(out:sub(1, max) == string.rep("x", max), "prefix preserved up to the cap")
  assert(out:find(h.CAP_HINT_REWRITE, 1, true), "caller-chosen hint rendered")
  local narrow = cap_read_output(big, h.CAP_HINT_NARROW)
  assert(
    narrow:find(h.CAP_HINT_NARROW, 1, true) and not narrow:find(h.CAP_HINT_REWRITE, 1, true),
    "only the requested hint appears"
  )
end)

case("cap_read_output_never_splits_utf8_codepoint", function()
  local max = h.MAX_FILE_BYTES
  -- "é" is 2 bytes; the odd ASCII prefix forces the cap to land mid-codepoint
  local big = "x" .. string.rep("\195\169", max)
  local out = cap_read_output(big, h.CAP_HINT_REWRITE)
  local prefix = out:match("^(.-)\n%.%.%. %(output truncated")
  assert(prefix, "truncation marker present")
  eq(#prefix, max - 1, "backs off one byte to the last codepoint boundary")
  eq(prefix:byte(#prefix), 0xA9, "prefix ends on a complete codepoint")
end)

case("encode_frontmatter_round_trips", function()
  local fm = encode_frontmatter({ "auth", "sessions" })
  assert(fm:find("^%-%-%-\n") and fm:find("\n%-%-%-\n$"), "fenced frontmatter")
  local parsed = extract_tags(parse_frontmatter(fm .. "body"))
  eq(parsed[1], "auth")
  eq(parsed[2], "sessions")

  eq(extract_tags(parse_frontmatter(encode_frontmatter({}) .. "body")), nil, "empty tags round-trip to stem fallback")
end)

case_tmp("format_read_union_headers_and_no_match", function(dir)
  write_mem(dir, "a.md", { "auth" }, "A body")
  write_mem(dir, "b.md", { "sessions" }, "B body")
  write_mem(dir, "c.md", { "auth", "sessions" }, "C body")

  local result, err = format_read(dir, { "auth" })
  eq(err, nil)
  assert(result:find("a%.md %(%d+ bytes%) %[auth%]"), "header shows size and tags")
  assert(result:find("c%.md %(%d+ bytes%) %[auth, sessions%]"), "match on any tag shows all tags")
  assert(not result:find("b%.md"), "non-matching file excluded")
  assert(result:find("A body") and not result:find("tags:"), "bodies included, frontmatter stripped")

  local union = format_read(dir, { "auth", "sessions" })
  assert(union:find("a%.md") and union:find("b%.md") and union:find("c%.md"), "tags select the union")

  eq(format_read(dir, { "nope" }), NO_MATCH_MSG)
end)

case_tmp("format_read_normalizes_request_and_stem", function(dir)
  write_mem(dir, "a.md", { "auth_flow" }, "A")
  maki.fs.write(maki.fs.joinpath(dir, "notes.md"), "no frontmatter")

  assert(format_read(dir, { "Auth-Flow", "AUTH FLOW" }):find("a%.md"), "request tags normalized before matching")

  local result = format_read(dir, { "notes" })
  assert(result:find("notes%.md") and result:find("no frontmatter"), "stem pseudo-tag readable")
end)

case_tmp("format_read_invalid_tags", function(dir)
  write_mem(dir, "a.md", { "auth" }, "A")

  local result, err = format_read(dir, { "!!!", "" })
  eq(result, nil)
  assert(err:find("no valid tags") and err:find("!!!", 1, true), "all-invalid errors and names the raw values")

  local partial, perr = format_read(dir, { "auth", "!!!" })
  eq(perr, nil)
  local wpos = partial:find("warning: ignored invalid tag")
  assert(wpos and wpos < partial:find("a%.md"), "partial invalid warns before the matches")
end)

case_tmp("format_read_oversized_picks_hint_by_match_count", function(dir)
  local max = h.MAX_FILE_BYTES
  local one = maki.fs.joinpath(dir, "one")
  local many = maki.fs.joinpath(dir, "many")
  maki.fs.mkdir(one)
  maki.fs.mkdir(many)
  write_mem(one, "big.md", { "bulk" }, string.rep("x", max + 50))
  local half = math.floor(max / 2) + 100
  write_mem(many, "a.md", { "bulk" }, string.rep("a", half))
  write_mem(many, "b.md", { "bulk" }, string.rep("b", half))

  local single = format_read(one, { "bulk" })
  assert(single:find("truncated at", 1, true), "single oversized match truncated")
  assert(
    single:find(h.CAP_HINT_REWRITE, 1, true) and not single:find(h.CAP_HINT_NARROW, 1, true),
    "one match asks for a rewrite"
  )

  local concat = format_read(many, { "bulk" })
  assert(concat:find("truncated at", 1, true), "concat over the cap truncated")
  assert(
    concat:find(h.CAP_HINT_NARROW, 1, true) and not concat:find(h.CAP_HINT_REWRITE, 1, true),
    "many matches ask to narrow"
  )
end)

case("format_read_entry", function()
  local formatted = h.format_read_entry("arch.md", 42, "---\ntags:\n  - auth\n  - sessions\n---\n# Body\ntext")
  assert(formatted:find("^arch%.md %(42 bytes%) %[auth, sessions%]"), "header carries size and tags")
  assert(formatted:find("\n# Body\ntext$") and not formatted:find("%-%-%-"), "body follows, fences stripped")

  eq(
    h.format_read_entry("notes.md", 12, "just a body"),
    "notes.md (12 bytes)\n\njust a body",
    "no brackets when untagged"
  )
end)

case("validate_input", function()
  local ok_inputs = {
    { command = "list" },
    { command = "read", tags = { "auth" } },
    { command = "read", path = "a.md" },
    { command = "write", path = "a.md", content = "x" },
    { command = "write", path = "a.md", content = "x", tags = {} },
    { command = "delete", path = "a.md" },
  }
  for i, input in ipairs(ok_inputs) do
    eq(validate_input(input), nil, "ok input #" .. i .. " (" .. input.command .. ")")
  end

  local bad = {
    { { command = "find" }, "unknown command" },
    { { command = "read" }, "path.*or.*tags" },
    { { command = "read", path = "a.md", tags = { "auth" } }, "not both" },
    { { command = "read", tags = "auth" }, "must be an array" },
    { { command = "write" }, "path" },
    { { command = "write", path = "a.md" }, "content" },
    { { command = "delete" }, "path" },
  }
  for _, v in ipairs(bad) do
    local err = validate_input(v[1])
    assert(err and err:find(v[2]), "expected error matching '" .. v[2] .. "', got: " .. tostring(err))
  end
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
