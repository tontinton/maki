local function line_nr_fmt(count)
  local w = math.max(1, math.floor(math.log(count + 1, 10)) + 1)
  return "%" .. w .. "d "
end

local function truncate_bytes(line, max_bytes)
  if #line <= max_bytes then
    return line
  end
  local i = max_bytes
  while i > 0 and line:byte(i) >= 0x80 and line:byte(i) < 0xC0 do
    i = i - 1
  end
  if i > 0 and line:byte(i) >= 0xC0 then
    i = i - 1
  end
  return line:sub(1, i) .. "..."
end

local function split_lines(content)
  local lines = {}
  local pos = 1
  while pos <= #content do
    local nl = content:find("\n", pos, true)
    if nl then
      local line = content:sub(pos, nl - 1)
      lines[#lines + 1] = line:find("\r$") and line:sub(1, -2) or line
      pos = nl + 1
    else
      local line = content:sub(pos)
      lines[#lines + 1] = line:find("\r$") and line:sub(1, -2) or line
      pos = #content + 1
    end
  end
  return lines
end

local function sort_dir_entries(entries, is_instruction_file)
  local sorted = {}
  for _, entry in ipairs(entries) do
    local name, typ = entry[1], entry[2]
    if typ == "directory" then
      sorted[#sorted + 1] = { name .. "/", true }
    elseif not is_instruction_file(name) then
      sorted[#sorted + 1] = { name, false }
    end
  end
  table.sort(sorted, function(a, b)
    if a[2] ~= b[2] then
      return a[2]
    end
    return a[1] < b[1]
  end)
  local names = {}
  for _, e in ipairs(sorted) do
    names[#names + 1] = e[1]
  end
  return names
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
  local name = "/tmp/maki_read_spec_" .. tostring(os.clock()):gsub("%.", "") .. "_" .. _tmpdir_counter
  maki.fs.mkdir(name)
  return name
end

local function rmtree(dir)
  maki.fs.rm(dir, { recursive = true })
end

-- line_nr_fmt: table-driven across all boundaries + alignment

case("line_nr_fmt_boundaries_and_alignment", function()
  local vectors = {
    { 0, "%1d " },
    { 1, "%1d " },
    { 8, "%1d " },
    { 9, "%2d " },
    { 10, "%2d " },
    { 98, "%2d " },
    { 99, "%3d " },
    { 100, "%3d " },
    { 999, "%4d " },
    { 1000, "%4d " },
  }
  for _, v in ipairs(vectors) do
    eq(line_nr_fmt(v[1]), v[2], "count=" .. v[1])
  end
  local fmt = line_nr_fmt(100)
  eq(string.format(fmt, 1), "  1 ")
  eq(string.format(fmt, 100), "100 ")
end)

-- truncate_bytes: ASCII + all UTF-8 widths

case("truncate_ascii", function()
  eq(truncate_bytes("", 10), "")
  eq(truncate_bytes("hello", 10), "hello")
  eq(truncate_bytes("hello", 5), "hello")
  eq(truncate_bytes("hello world", 5), "hello...")
  eq(truncate_bytes("ab", 1), "a...")
end)

case("truncate_utf8_boundary_safety", function()
  -- 2-byte: é = \xC3\xA9
  eq(truncate_bytes("caf\xC3\xA9", 10), "caf\xC3\xA9")
  eq(truncate_bytes("caf\xC3\xA9!", 5), "caf...")
  eq(truncate_bytes("caf\xC3\xA9", 4), "caf...")

  -- 3-byte: € = \xE2\x82\xAC — cut at each byte within the sequence
  eq(truncate_bytes("ab\xE2\x82\xACd", 5), "ab...")
  eq(truncate_bytes("ab\xE2\x82\xAC", 4), "ab...")
  eq(truncate_bytes("ab\xE2\x82\xAC", 3), "ab...")

  -- 4-byte: 🎉 = \xF0\x9F\x8E\x89 — cutting anywhere inside removes entire char
  local emoji = "\xF0\x9F\x8E\x89"
  eq(truncate_bytes(emoji, 4), emoji)
  eq(truncate_bytes(emoji, 3), "...")
  eq(truncate_bytes(emoji, 1), "...")

  -- all multibyte: cutting within sequences
  local s = "\xC3\xA9\xC3\xA9\xC3\xA9"
  eq(truncate_bytes(s, 4), "\xC3\xA9...")
  eq(truncate_bytes(s, 2), "...")
end)

-- split_lines: table-driven

case("split_lines", function()
  local vectors = {
    { "", 0, {} },
    { "hello", 1, { "hello" } },
    { "a\nb", 2, { "a", "b" } },
    { "a\nb\n", 2, { "a", "b" } },
    { "\n\n\n", 3, { "", "", "" } },
    { "a\r\nb\r\n", 2, { "a", "b" } },
  }
  for _, v in ipairs(vectors) do
    local lines = split_lines(v[1])
    eq(#lines, v[2], "count for " .. ("%q"):format(v[1]))
    for i, expected in ipairs(v[3]) do
      eq(lines[i], expected, "line " .. i .. " for " .. ("%q"):format(v[1]))
    end
  end
end)

-- sort_dir_entries

local function mock_is_instruction(name)
  local set = { ["AGENTS.md"] = true, ["CLAUDE.md"] = true, ["COPILOT.md"] = true }
  return set[name] or false
end

case("sort_dirs_first_alpha_within_group", function()
  local entries = {
    { "z", "directory" },
    { "a", "directory" },
    { "m.txt", "file" },
    { "b.txt", "file" },
  }
  local names = sort_dir_entries(entries, mock_is_instruction)
  eq(names[1], "a/")
  eq(names[2], "z/")
  eq(names[3], "b.txt")
  eq(names[4], "m.txt")
end)

case("sort_hides_instruction_files_not_dirs", function()
  local entries = {
    { "AGENTS.md", "file" },
    { "CLAUDE.md", "file" },
    { "COPILOT.md", "file" },
    { "AGENTS.md", "directory" },
    { "main.rs", "file" },
  }
  local names = sort_dir_entries(entries, mock_is_instruction)
  eq(#names, 2)
  eq(names[1], "AGENTS.md/")
  eq(names[2], "main.rs")
end)

-- integration: directory listing via real filesystem

case("dir_listing_sort_and_filter", function()
  local tmpdir = mktmpdir()
  maki.fs.write(maki.fs.joinpath(tmpdir, "c.txt"), "")
  maki.fs.write(maki.fs.joinpath(tmpdir, "a.txt"), "")
  maki.fs.write(maki.fs.joinpath(tmpdir, "AGENTS.md"), "instructions")
  maki.fs.mkdir(maki.fs.joinpath(tmpdir, "zdir"))
  maki.fs.mkdir(maki.fs.joinpath(tmpdir, "adir"))

  local entries, err = maki.fs.dir(tmpdir)
  assert(err == nil, "dir listing should succeed: " .. tostring(err))
  local names = sort_dir_entries(entries, mock_is_instruction)
  eq(#names, 4)
  eq(names[1], "adir/")
  eq(names[2], "zdir/")
  eq(names[3], "a.txt")
  eq(names[4], "c.txt")
  rmtree(tmpdir)
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
