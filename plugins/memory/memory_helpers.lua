local M = {}

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

-- Normalize both paths and check the prefix to block "../" traversal
-- out of the memories sandbox.
function M.safe_resolve(memories_dir, relative)
  if not relative or relative == "" then
    return nil, "path is required"
  end
  local first = relative:sub(1, 1)
  if relative:find("\0") or first == "/" or first == "\\" then
    return nil, "path must be relative"
  end
  -- Drive letter (C:\, D:/)
  if relative:match("^%a:") then
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

function M.collect_file_entries(dir)
  local entries = maki.fs.dir(dir)
  if not entries then
    return {}
  end
  local files = {}
  for _, entry in ipairs(entries) do
    if entry[2] == "file" then
      local meta = maki.fs.metadata(maki.fs.joinpath(dir, entry[1]))
      if meta then
        files[#files + 1] = { entry[1], meta.size }
      end
    end
  end
  return files
end

local function trim(s)
  return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function split_lines(s)
  local lines = {}
  local pos = 1
  while pos <= #s do
    local nl = s:find("\n", pos, true)
    if not nl then
      lines[#lines + 1] = s:sub(pos)
      break
    end
    lines[#lines + 1] = s:sub(pos, nl - 1)
    pos = nl + 1
  end
  return lines
end

-- Collapse any run of non-alphanumeric chars to a single underscore,
-- then trim leading/trailing underscores. "User Decision" -> "user_decision",
-- "auth-token" -> "auth_token", "  API " -> "api".
-- Applied at write time (canonical stored form) and at fetch time
-- so caller-provided variants still match the stored form.
function M.normalize_tag(tag)
  local t = trim(tag):lower()
  t = t:gsub("[^%w]+", "_")
  t = t:gsub("^_+", ""):gsub("_+$", "")
  return t
end

-- Normalize and dedup a list of tags, preserving first-seen order.
function M.normalize_tag_list(tags)
  local seen = {}
  local out = {}
  for _, t in ipairs(tags or {}) do
    local n = M.normalize_tag(t)
    if #n > 0 and not seen[n] then
      seen[n] = true
      out[#out + 1] = n
    end
  end
  return out
end

local function parse_inline_tags(value)
  local v = trim(value)
  local tags = {}
  if v:sub(1, 1) == "[" and v:sub(-1) == "]" then
    local seen = {}
    for part in v:sub(2, -2):gmatch("([^,]+)") do
      local t = trim(part)
      if #t >= 2 then
        local q = t:sub(1, 1)
        if (q == '"' or q == "'") and t:sub(-1) == q then
          t = trim(t:sub(2, -2))
        end
      end
      t = M.normalize_tag(t)
      if #t > 0 and not seen[t] then
        seen[t] = true
        tags[#tags + 1] = t
      end
    end
  else
    local token = v:match("^(%S+)$")
    if token then
      tags = { M.normalize_tag(token) }
    end
  end
  return tags
end

function M.parse_frontmatter(content)
  local s = content:gsub("\r\n", "\n")
  if s:sub(1, 4) ~= "---\n" then
    return { tags = {}, preserved = {} }
  end
  local lines = split_lines(s)
  local close_line
  for i = 2, #lines do
    if lines[i] == "---" then
      close_line = i
      break
    end
  end
  if not close_line then
    return { tags = {}, preserved = {} }
  end
  local tags, preserved = {}, {}
  local i = 2
  while i < close_line do
    local line = lines[i]
    if line:lower():sub(1, 5) == "tags:" then
      local rest = trim(line:sub(6))
      if rest == "" then
        local seen = {}
        i = i + 1
        while i < close_line do
          local item = lines[i]
          local bullet = item:match("^%s*-%s+(.*)$")
          if bullet then
            local t = M.normalize_tag(bullet)
            if #t > 0 and not seen[t] then
              seen[t] = true
              tags[#tags + 1] = t
            end
            i = i + 1
          else
            break
          end
        end
      else
        local inline = parse_inline_tags(rest)
        for _, t in ipairs(inline) do
          tags[#tags + 1] = t
        end
        i = i + 1
      end
    else
      preserved[#preserved + 1] = line
      i = i + 1
    end
  end
  return { tags = tags, preserved = preserved }
end

function M.collect_file_entries_with_tags(dir)
  local entries = M.collect_file_entries(dir)
  local result = {}
  for _, entry in ipairs(entries) do
    local name, bytes = entry[1], entry[2]
    local path = maki.fs.joinpath(dir, name)
    local content, err = maki.fs.read(path)
    local tags = {}
    if content and not err then
      tags = M.parse_frontmatter(content).tags
    end
    local from_stem = false
    if content and not err then
      tags = M.parse_frontmatter(content).tags
    end
    if #tags == 0 then
      local stem = name:gsub("%.[^.]*$", "")
      local t = M.normalize_tag(stem)
      if #t > 0 then
        tags = { t }
        from_stem = true
      end
    end
    result[#result + 1] = { name = name, bytes = bytes, tags = tags, from_stem = from_stem }
  end
  return result
end

function M.collect_tag_counts(entries)
  local counts = {}
  for _, e in ipairs(entries) do
    for _, t in ipairs(e.tags or {}) do
      counts[t] = (counts[t] or 0) + 1
    end
  end
  return counts
end

-- True if any of the file's tags matches any of the requested tags (union).
-- Matches are normalized internally on both sides.
function M.file_matches_any_tag(entry, requested)
  if not entry.tags or #entry.tags == 0 then
    return false
  end
  local wanted = {}
  for _, t in ipairs(requested or {}) do
    wanted[M.normalize_tag(t)] = true
  end
  for _, t in ipairs(entry.tags) do
    if wanted[t] then
      return true
    end
  end
  return false
end

return M
