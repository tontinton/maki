local M = {}

M.MAX_TAGS = 50
M.MAX_FILE_BYTES = 20 * 1024
M.CAP_HINT_REWRITE = "rewrite the memory to fit under the cap"
M.CAP_HINT_NARROW = "narrow tags or read files by path"
M.CAP_HINT_FILTER = "pass tags=[...] to filter the list"
M.NO_MEMORIES_MSG = "No memories yet."

local MAX_TAG_LEN = 64
local MAX_REJECT_DISPLAY = 64
local WRITE_REJECT_PREFIX = "invalid tag(s) rejected: "
local READ_REJECT_PREFIX = "warning: ignored invalid tag(s): "
local UNREADABLE_PREFIX = "warning: unreadable memory files: "
local NO_MATCH_MSG = "no memory files matched any of the given tags; use `list` to see available tags"
local PRUNE_ADVISORY = "Consider removing or consolidating stale memories to stay under " .. M.MAX_TAGS .. " tags."

local COMMANDS = { "list", "read", "write", "delete" }
local VALID_COMMANDS = {}
for _, c in ipairs(COMMANDS) do
  VALID_COMMANDS[c] = true
end

-- Lua's bit32 is 32-bit only, so we split the 64-bit FNV-1a state into
-- hi/lo halves and propagate carries by hand during multiplication.
function M.fnv1a_64(data)
  local lo = 0x84222325
  local hi = 0xcbf29ce4
  local p_lo = 0x000001b3
  local p_hi = 0x00000100
  for i = 1, #data do
    lo = bit32.bxor(lo, string.byte(data, i))
    local ll = lo * p_lo
    local ll_lo = ll % 0x100000000
    local ll_hi = (ll - ll_lo) / 0x100000000
    local new_hi = (hi * p_lo + lo * p_hi + ll_hi) % 0x100000000
    lo = ll_lo
    hi = new_hi
  end
  return string.format("%08x%08x", hi, lo)
end

function M.project_id(path)
  local base = maki.fs.basename(path) or "root"
  return base .. "-" .. M.fnv1a_64(path)
end

function M.safe_resolve(memories_dir, relative)
  if not relative or relative == "" then
    return nil, "path is required"
  end
  local first = relative:sub(1, 1)
  if relative:find("\0") or first == "/" or first == "\\" or relative:match("^%a:") then
    return nil, "path must be relative"
  end
  local resolved = maki.fs.normalize(maki.fs.joinpath(memories_dir, relative))
  local norm_base = maki.fs.normalize(memories_dir)
  local sep = norm_base:find("\\") and "\\" or "/"
  local prefix = norm_base .. sep
  if resolved:sub(1, #prefix) ~= prefix then
    return nil, "path traversal outside memories directory is not allowed"
  end
  return resolved
end

local function file_entries(dir)
  local files = {}
  for _, entry in ipairs(maki.fs.dir(dir) or {}) do
    if entry[2] == "file" then
      local meta = maki.fs.metadata(maki.fs.joinpath(dir, entry[1]))
      if meta then
        files[#files + 1] = { entry[1], meta.size, meta.mtime }
      end
    end
  end
  table.sort(files, function(a, b)
    return a[1] < b[1]
  end)
  return files
end

-- Coerces rather than rejects: any non-alphanumeric run becomes "_".
-- Returns nil only when nothing survives.
function M.normalize_tag(raw)
  if raw == nil then
    return nil
  end
  local s = tostring(raw):lower():gsub("[^%a%d]+", "_"):gsub("^_+", ""):gsub("_+$", "")
  if s == "" then
    return nil
  end
  return s:sub(1, MAX_TAG_LEN)
end

local function stem_tag(path)
  local base = maki.fs.basename(path) or path
  local stem = base:gsub("%.[^.]*$", "")
  if stem == "" then
    stem = base
  end
  return M.normalize_tag(stem) or "untagged"
end

function M.normalize_tags(list)
  local seen, out, rejected, coerced = {}, {}, {}, {}
  for _, t in ipairs(list) do
    local n = M.normalize_tag(t)
    if n then
      if not seen[n] then
        seen[n] = true
        out[#out + 1] = n
      end
      if n ~= tostring(t) then
        coerced[#coerced + 1] = tostring(t) .. " -> " .. n
      end
    else
      rejected[#rejected + 1] = tostring(t)
    end
  end
  return out, rejected, coerced
end

function M.format_rejected(rejected)
  if not rejected or #rejected == 0 then
    return nil
  end
  local out = {}
  for i, v in ipairs(rejected) do
    out[i] = #v > MAX_REJECT_DISPLAY and v:sub(1, MAX_REJECT_DISPLAY) .. "..." or v
  end
  return table.concat(out, ", ")
end

function M.validate_input(input)
  local cmd = input.command
  if not VALID_COMMANDS[cmd] then
    return "unknown command '" .. tostring(cmd) .. "'. Valid commands: " .. table.concat(COMMANDS, ", ")
  end
  if input.tags ~= nil and type(input.tags) ~= "table" then
    return "'tags' must be an array"
  end
  local has_path = input.path ~= nil and input.path ~= ""
  local has_tags = type(input.tags) == "table" and #input.tags > 0
  if cmd == "read" then
    if has_path and has_tags then
      return "provide 'path' or 'tags', not both"
    end
    if not has_path and not has_tags then
      return "'path' or 'tags' is required for read"
    end
  elseif cmd == "write" then
    if not has_path then
      return "'path' is required for write"
    end
    if not input.content then
      return "'content' is required for write"
    end
  elseif cmd == "delete" then
    if not has_path then
      return "'path' is required for delete"
    end
  end
  return nil
end

-- Returns (want set, warning?, error?): error when no tag survives
-- normalization, warning when only some were rejected.
local function normalize_to_want(raw_tags)
  local normalized, rejected = M.normalize_tags(raw_tags)
  local r = M.format_rejected(rejected)
  if #normalized == 0 then
    return nil, nil, "no valid tags after normalization" .. (r and ("; rejected: " .. r) or "")
  end
  local want = {}
  for _, n in ipairs(normalized) do
    want[n] = true
  end
  return want, r and (READ_REJECT_PREFIX .. r) or nil
end

function M.validate_write_tags(raw_tags)
  local normalized, rejected, coerced = M.normalize_tags(raw_tags)
  local r = M.format_rejected(rejected)
  if r then
    return nil, WRITE_REJECT_PREFIX .. r
  end
  local note = #coerced > 0 and ("normalized: " .. table.concat(coerced, ", ")) or nil
  return normalized, nil, note
end

local function join_parts(sep, ...)
  local parts = {}
  for i = 1, select("#", ...) do
    local v = select(i, ...)
    if v then
      parts[#parts + 1] = v
    end
  end
  return table.concat(parts, sep)
end

local function combine_unreadable(warnings)
  if #warnings == 0 then
    return nil
  end
  return UNREADABLE_PREFIX .. table.concat(warnings, ", ")
end

function M.parse_frontmatter(content)
  local rest = content:match("^%s*%-%-%-\n(.*)")
  if not rest then
    return {}, content
  end
  local end_pos = rest:find("\n%-%-%-")
  if not end_pos then
    return {}, content
  end
  local yaml_str = rest:sub(1, end_pos)
  local body = rest:sub(end_pos + 4):match("^%s*(.-)%s*$")
  local fm = maki.yaml.decode(yaml_str) or {}
  return fm, body
end

function M.extract_tags(frontmatter)
  if type(frontmatter) ~= "table" then
    return nil
  end
  local raw = frontmatter.tags
  if type(raw) == "string" then
    raw = { raw }
  end
  if type(raw) ~= "table" then
    return nil
  end
  local out = M.normalize_tags(raw)
  return #out > 0 and out or nil
end

function M.tags_for_file(path, content, read_err)
  if content then
    local tags = M.extract_tags(M.parse_frontmatter(content))
    if tags then
      return tags, nil
    end
  end
  return { stem_tag(path) }, read_err or (content == nil and "read error" or nil)
end

local function read_tags(dir, name)
  local content, read_err = maki.fs.read(maki.fs.joinpath(dir, name))
  local tags, warn = M.tags_for_file(name, content, read_err and tostring(read_err))
  return tags, warn
end

-- Tags per file, keyed on mtime and size so unchanged files are never re-read.
-- Unreadable files are never cached: a transient failure cannot pin stale stem tags.
local tag_cache = {}

local function cached_tags(dir, name, size, mtime)
  local key = maki.fs.joinpath(dir, name)
  local c = tag_cache[key]
  if c and mtime and c.mtime == mtime and c.size == size then
    return c.tags
  end
  local tags, warn = read_tags(dir, name)
  if not warn and mtime then
    tag_cache[key] = { mtime = mtime, size = size, tags = tags }
  end
  return tags, warn
end

local function has_wanted_tag(tags, want)
  for _, t in ipairs(tags) do
    if want[t] then
      return true
    end
  end
  return false
end

local function matching_entries(dir, want)
  local matches, warnings = {}, {}
  for _, f in ipairs(file_entries(dir)) do
    local name, size, mtime = f[1], f[2], f[3]
    local tags, warn = cached_tags(dir, name, size, mtime)
    if warn then
      warnings[#warnings + 1] = name .. ": " .. warn
    elseif has_wanted_tag(tags, want) then
      local content, read_err = maki.fs.read(maki.fs.joinpath(dir, name))
      if content then
        matches[#matches + 1] = { name = name, content = content, size = size }
      else
        warnings[#warnings + 1] = name .. ": " .. tostring(read_err)
      end
    end
  end
  return matches, warnings
end

-- Groups files under their tags, most-used tag first (name breaks ties).
-- Unreadable files land in warnings instead of a group.
function M.grouped_tags(dir)
  local by_tag, groups, warnings = {}, {}, {}
  for _, f in ipairs(file_entries(dir)) do
    local name, size = f[1], f[2]
    local tags, warn = cached_tags(dir, name, size, f[3])
    if warn then
      warnings[#warnings + 1] = name .. ": " .. warn
    else
      for _, t in ipairs(tags) do
        local g = by_tag[t]
        if not g then
          g = { tag = t, files = {} }
          by_tag[t] = g
          groups[#groups + 1] = g
        end
        g.files[#g.files + 1] = { name = name, size = size }
      end
    end
  end
  table.sort(groups, function(a, b)
    if #a.files == #b.files then
      return a.tag < b.tag
    end
    return #a.files > #b.files
  end)
  return groups, warnings
end

function M.format_tag_line(dir, max_tags)
  local groups, warnings = M.grouped_tags(dir)
  if #groups == 0 and #warnings == 0 then
    return nil
  end
  local tags = {}
  for i, g in ipairs(groups) do
    tags[i] = g.tag
  end
  local line = table.concat(tags, ", ", 1, math.min(#tags, max_tags))
  if #tags > max_tags then
    line = line .. " ... (" .. (#tags - max_tags) .. " tags omitted; use `list` to see all)"
  end
  if #warnings > 0 then
    line = (#line > 0 and line .. " " or "") .. "(unreadable: " .. #warnings .. ")"
  end
  return line
end

-- serde_yaml renders an empty list as an empty mapping; extract_tags treats both as empty.
function M.encode_frontmatter(tags)
  local yaml, _ = maki.yaml.encode({ tags = tags })
  return "---\n" .. yaml .. "---\n"
end

function M.cap_read_output(s, hint)
  if #s <= M.MAX_FILE_BYTES then
    return s
  end
  -- Back off UTF-8 continuation bytes so the cut never splits a codepoint.
  local cut = M.MAX_FILE_BYTES
  while cut > 0 do
    local b = s:byte(cut + 1)
    if b < 0x80 or b >= 0xC0 then
      break
    end
    cut = cut - 1
  end
  return s:sub(1, cut) .. "\n... (output truncated at " .. M.MAX_FILE_BYTES .. " bytes; " .. hint .. ")"
end

function M.validate_write_size(content)
  if #content > M.MAX_FILE_BYTES then
    return "content exceeds "
      .. M.MAX_FILE_BYTES
      .. " bytes (got "
      .. #content
      .. "); split the memory or trim to stay under the cap"
  end
  return nil
end

function M.format_read_entry(name, size, content)
  local fm, body = M.parse_frontmatter(content)
  local tags = M.extract_tags(fm) or {}
  local header = name .. " (" .. size .. " bytes)"
  if #tags > 0 then
    header = header .. " [" .. table.concat(tags, ", ") .. "]"
  end
  return header .. "\n\n" .. body
end

function M.format_list(dir, raw_tags)
  local want, warning
  if raw_tags and #raw_tags > 0 then
    local werr
    want, warning, werr = normalize_to_want(raw_tags)
    if werr then
      return nil, werr
    end
  end

  local groups, read_warnings = M.grouped_tags(dir)
  if #groups == 0 and not want then
    return join_parts("\n", combine_unreadable(read_warnings), M.NO_MEMORIES_MSG)
  end

  local lines = {}
  local matched = 0
  for _, g in ipairs(groups) do
    if not want or want[g.tag] then
      matched = matched + 1
      lines[#lines + 1] = g.tag .. " (" .. #g.files .. ")"
      for _, f in ipairs(g.files) do
        lines[#lines + 1] = "  - " .. f.name .. " (" .. f.size .. " bytes)"
      end
      lines[#lines + 1] = ""
    end
  end

  local unreadable = combine_unreadable(read_warnings)
  if matched == 0 then
    return join_parts("\n", warning, unreadable, NO_MATCH_MSG)
  end
  local body = M.cap_read_output(table.concat(lines, "\n"), M.CAP_HINT_FILTER)
  if not want and #groups > M.MAX_TAGS then
    body = body .. "\n" .. PRUNE_ADVISORY
  end
  return join_parts("\n", warning, unreadable, body), nil
end

function M.format_read(dir, raw_tags)
  local want, warning, err = normalize_to_want(raw_tags)
  if err then
    return nil, err
  end

  local matches, read_warnings = matching_entries(dir, want)
  local parts = {}
  for _, m in ipairs(matches) do
    parts[#parts + 1] = M.format_read_entry(m.name, m.size, m.content)
  end

  local hint = #parts <= 1 and M.CAP_HINT_REWRITE or M.CAP_HINT_NARROW
  local body = #parts > 0 and M.cap_read_output(table.concat(parts, "\n\n"), hint) or NO_MATCH_MSG
  return join_parts("\n\n", warning, combine_unreadable(read_warnings), body)
end

return M
