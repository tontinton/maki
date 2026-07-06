local h = require("memory_helpers")

local fnv1a_64 = h.fnv1a_64
local project_id = h.project_id
local safe_resolve = h.safe_resolve
local parse_frontmatter = h.parse_frontmatter
local collect_file_entries_with_tags = h.collect_file_entries_with_tags
local collect_tag_counts = h.collect_tag_counts
local file_matches_any_tag = h.file_matches_any_tag
local normalize_tag = h.normalize_tag

local function tags_eq(actual, expected, msg)
  if #actual ~= #expected then
    error((msg or "") .. "\nlength expected: " .. #expected .. " actual: " .. #actual)
  end
  for i = 1, #expected do
    if actual[i] ~= expected[i] then
      error(
        (msg or "") .. "\nidx " .. i .. " expected: " .. tostring(expected[i]) .. " actual: " .. tostring(actual[i])
      )
    end
  end
end

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
local function mktmpdir()
  _tmpdir_counter = _tmpdir_counter + 1
  local name = "/tmp/maki_spec_" .. tostring(os.clock()):gsub("%.", "") .. "_" .. _tmpdir_counter
  maki.fs.mkdir(name)
  return name
end

local function rmtree(dir)
  local entries = maki.fs.dir(dir)
  if entries then
    for _, e in ipairs(entries) do
      local p = maki.fs.joinpath(dir, e[1])
      if e[2] == "directory" then
        rmtree(p)
      else
        maki.fs.rm(p)
      end
    end
  end
  maki.fs.rm(dir)
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
end)

case("fnv1a_high_bytes_no_overflow", function()
  local result = fnv1a_64(string.rep("\xff", 64))
  eq(#result, 16, "should always produce 16 hex chars")
  assert(result:match("^%x+$"), "should be valid hex")
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

  local id1 = project_id("/home/alice/myapp")
  local id2 = project_id("/home/bob/myapp")
  assert(id1 ~= id2, "different full paths should produce different IDs")
end)

case("normalize_tag_collapses_non_alphanumeric", function()
  eq(normalize_tag("User Decision"), "user_decision")
  eq(normalize_tag("auth-token"), "auth_token")
  eq(normalize_tag("  API "), "api")
  eq(normalize_tag("a__b  c"), "a_b_c")
  eq(normalize_tag("--leading-trailing--"), "leading_trailing")
  eq(normalize_tag("already_snake"), "already_snake")
  eq(normalize_tag("mixed.CASE-here"), "mixed_case_here")
end)

case("parse_frontmatter_no_frontmatter", function()
  local r = parse_frontmatter("just a body\nline two")
  tags_eq(r.tags, {}, "no tags")
  eq(#r.preserved, 0, "no preserved lines")
end)

case("parse_frontmatter_bullet_list_tags", function()
  local r = parse_frontmatter("---\ntags:\n  - auth\n  - user decision\n---\nbody\n")
  tags_eq(r.tags, { "auth", "user_decision" })
  eq(#r.preserved, 0, "only tags block, nothing preserved")
end)

case("parse_frontmatter_bullet_list_normalizes", function()
  local r = parse_frontmatter("---\ntags:\n  - User-Decision\n  - API\n  - auth\n---\nbody\n")
  tags_eq(r.tags, { "user_decision", "api", "auth" })
end)

case("parse_frontmatter_inline_list_tags", function()
  local r = parse_frontmatter("---\ntags: [auth, decision]\n---\nbody\n")
  tags_eq(r.tags, { "auth", "decision" })
  eq(#r.preserved, 0, "only tags line, nothing preserved")
end)

case("parse_frontmatter_crlf", function()
  local r = parse_frontmatter("---\r\ntags: [auth]\r\n---\r\nbody\r\n")
  tags_eq(r.tags, { "auth" })
  eq(#r.preserved, 0)
end)

case("parse_frontmatter_quotes_dedup_whitespace", function()
  local r = parse_frontmatter("---\ntags: [ \"auth\" , 'User Decision', auth,]\n---\n")
  tags_eq(r.tags, { "auth", "user_decision" })
end)

case("parse_frontmatter_scalar", function()
  local r = parse_frontmatter("---\ntags: auth\n---\nbody\n")
  tags_eq(r.tags, { "auth" })
end)

case("parse_frontmatter_missing_close", function()
  local r = parse_frontmatter("---\ntags: [auth]\nbody without close\n")
  tags_eq(r.tags, {})
  eq(#r.preserved, 0, "no close means whole file body")
end)

case("parse_frontmatter_bullet_list_dedup", function()
  local r = parse_frontmatter("---\ntags:\n  - auth\n  - auth\n  - decision\n---\nbody\n")
  tags_eq(r.tags, { "auth", "decision" })
end)

case("parse_frontmatter_inline_list_dedup", function()
  local r = parse_frontmatter("---\ntags: [auth, Auth, AUTH]\n---\nbody\n")
  tags_eq(r.tags, { "auth" })
end)

case("parse_frontmatter_preserves_other_keys", function()
  local r = parse_frontmatter("---\ntitle: My Note\nauthor: bob\n---\nbody\n")
  tags_eq(r.tags, {})
  eq(#r.preserved, 2, "title and author preserved")
  eq(r.preserved[1], "title: My Note")
  eq(r.preserved[2], "author: bob")
end)

case("parse_frontmatter_preserves_other_keys_with_tags", function()
  local r = parse_frontmatter("---\ntitle: X\ntags: [auth]\nauthor: bob\n---\nbody\n")
  tags_eq(r.tags, { "auth" })
  eq(#r.preserved, 2, "title and author preserved, tags excluded")
  eq(r.preserved[1], "title: X")
  eq(r.preserved[2], "author: bob")
end)

case("parse_frontmatter_bullet_list_followed_by_preserved", function()
  local r = parse_frontmatter("---\ntags:\n  - auth\n  - api\ntitle: Note\n---\nbody\n")
  tags_eq(r.tags, { "auth", "api" })
  eq(#r.preserved, 1, "title preserved after bullet list")
  eq(r.preserved[1], "title: Note")
end)

case("collect_file_entries_with_tags", function()
  local tmpdir = mktmpdir()
  maki.fs.write(maki.fs.joinpath(tmpdir, "tagged.md"), "---\ntags: [auth, decision]\n---\nbody\n")
  maki.fs.write(maki.fs.joinpath(tmpdir, "plain.md"), "no frontmatter here\n")
  maki.fs.write(maki.fs.joinpath(tmpdir, "fmbutnotags.md"), "---\ntitle: Note\n---\nbody\n")

  local entries = collect_file_entries_with_tags(tmpdir)
  eq(#entries, 3, "three files")
  local by_name = {}
  for _, e in ipairs(entries) do
    by_name[e.name] = e
  end
  tags_eq(by_name["tagged.md"].tags, { "auth", "decision" })
  tags_eq(by_name["plain.md"].tags, { "plain" })
  tags_eq(by_name["fmbutnotags.md"].tags, { "fmbutnotags" })
  assert(by_name["tagged.md"].bytes > 0, "bytes should be set")
  rmtree(tmpdir)
end)

case("collect_file_entries_with_tags_normalizes_implicit_tag", function()
  local tmpdir = mktmpdir()
  maki.fs.write(maki.fs.joinpath(tmpdir, "User-Decision.md"), "no frontmatter\n")
  local entries = collect_file_entries_with_tags(tmpdir)
  tags_eq(entries[1].tags, { "user_decision" })
  rmtree(tmpdir)
end)

case("file_matches_any_tag_without_explicit_tags", function()
  local tmpdir = mktmpdir()
  maki.fs.write(maki.fs.joinpath(tmpdir, "architecture.md"), "no frontmatter\n")
  local entries = collect_file_entries_with_tags(tmpdir)
  assert(file_matches_any_tag(entries[1], { "architecture" }))
  assert(file_matches_any_tag(entries[1], { "Architecture" }))
  rmtree(tmpdir)
end)

case("collect_tag_counts_aggregates_across_files", function()
  local entries = {
    { name = "a.md", tags = { "auth", "decision" } },
    { name = "b.md", tags = { "auth" } },
    { name = "c.md", tags = {} },
    { name = "d.md", tags = { "auth", "api" } },
  }
  local c = collect_tag_counts(entries)
  eq(c["auth"], 3)
  eq(c["decision"], 1)
  eq(c["api"], 1)
  assert(c["missing"] == nil, "absent tags should not appear")
end)

case("file_matches_any_tag_union", function()
  local entry = { name = "x.md", tags = { "auth", "api" } }
  assert(file_matches_any_tag(entry, { "auth" }), "single tag match")
  assert(file_matches_any_tag(entry, { "api", "decision" }), "union: one of two matches")
  assert(file_matches_any_tag(entry, { "Auth", "API" }), "normalized: case-insensitive")
  assert(not file_matches_any_tag(entry, { "user-decision" }), "normalized: dash form does not falsely match")
  assert(not file_matches_any_tag(entry, { "decision" }), "no match when tag absent")
  assert(not file_matches_any_tag(entry, {}), "no match when no requested tags")
  assert(not file_matches_any_tag({ name = "y.md", tags = {} }, { "auth" }), "no match when file has no tags")
end)

case("file_matches_any_tag_normalizes_requested", function()
  local entry = { name = "z.md", tags = { "user_decision" } }
  assert(file_matches_any_tag(entry, { "User Decision" }), "request with spaces matches stored snake_case")
  assert(file_matches_any_tag(entry, { "user-decision" }), "request with dashes matches stored snake_case")
  assert(file_matches_any_tag(entry, { "USER_DECISION" }), "request uppercase matches stored snake_case")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
