local h = require("memory_helpers")

local fnv1a_64 = h.fnv1a_64
local count_lines = h.count_lines
local project_id = h.project_id
local safe_resolve = h.safe_resolve
local dir_total_bytes = h.dir_total_bytes
local list_memories = h.list_memories

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

case("count_lines", function()
  local vectors = {
    { "", 1 },
    { "hello", 1 },
    { "\n", 1 },
    { "a\nb", 2 },
    { "a\nb\n", 2 },
  }
  for _, v in ipairs(vectors) do
    eq(count_lines(v[1]), v[2], "input: " .. ("%q"):format(v[1]))
  end
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
  local good = {
    { "notes.md", "notes%.md" },
    { "sub/deep/notes.md", "sub/deep/notes%.md" },
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

case("dir_total_bytes", function()
  local tmpdir = mktmpdir()
  eq(dir_total_bytes(tmpdir), 0, "empty dir")
  eq(dir_total_bytes("/tmp/maki_test_surely_missing_" .. _tmpdir_counter), 0, "nonexistent dir")

  maki.fs.write(maki.fs.joinpath(tmpdir, "a"), "12345")
  maki.fs.write(maki.fs.joinpath(tmpdir, "b"), "67890")
  eq(dir_total_bytes(tmpdir), 10, "sum of files")
  rmtree(tmpdir)
end)

case("list_memories_empty_or_missing", function()
  local tmpdir = mktmpdir()
  eq(list_memories(tmpdir), "No memories yet.")
  eq(list_memories("/tmp/maki_test_does_not_exist_" .. _tmpdir_counter), "No memories yet.")
  rmtree(tmpdir)
end)

case("list_memories_sorts_sums_and_ignores_subdirs", function()
  local tmpdir = mktmpdir()
  maki.fs.write(maki.fs.joinpath(tmpdir, "zebra.md"), "zzz")
  maki.fs.write(maki.fs.joinpath(tmpdir, "alpha.md"), "a")
  maki.fs.mkdir(maki.fs.joinpath(tmpdir, "subdir"))

  local result = list_memories(tmpdir)
  assert(result:find("alpha.md") < result:find("zebra.md"), "should be sorted")
  assert(result:find("2 files"), "should count only files")
  assert(result:find("4 bytes total"), "should sum: 1+3=4")
  assert(not result:find("subdir"), "should not list subdirs")
  rmtree(tmpdir)
end)

case("write_read_delete_lifecycle", function()
  local tmpdir = mktmpdir()
  assert(not maki.fs.metadata(maki.fs.joinpath(tmpdir, "nope.md")), "metadata should be nil for nonexistent")

  local file_path = safe_resolve(tmpdir, "arch.md")
  maki.fs.write(file_path, "# Architecture\nMicroservices")
  eq(maki.fs.read(file_path), "# Architecture\nMicroservices")

  maki.fs.write(file_path, "v2")
  eq(maki.fs.read(file_path), "v2")

  maki.fs.rm(file_path)
  assert(not maki.fs.metadata(file_path), "file should be deleted")
  rmtree(tmpdir)
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
