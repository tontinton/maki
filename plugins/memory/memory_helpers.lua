local M = {}

M.MAX_LINES_PER_FILE = 200
M.MAX_DIR_BYTES = 50 * 1024

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

-- Counts lines the way editors do: empty string is 1 line,
-- and a trailing newline does not start a new line.
function M.count_lines(s)
  if s == "" then
    return 1
  end
  local _, newlines = s:gsub("\n", "")
  if s:sub(-1) == "\n" then
    return math.max(newlines, 1)
  end
  return newlines + 1
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
  if relative:find("\0") or relative:sub(1, 1) == "/" then
    return nil, "path must be relative"
  end
  local resolved = maki.fs.normalize(maki.fs.joinpath(memories_dir, relative))
  local norm_base = maki.fs.normalize(memories_dir)
  local prefix = norm_base .. "/"
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

function M.dir_total_bytes(dir)
  local total = 0
  for _, f in ipairs(M.collect_file_entries(dir)) do
    total = total + f[2]
  end
  return total
end

function M.list_memories(dir)
  local files = M.collect_file_entries(dir)
  if #files == 0 then
    return "No memories yet."
  end
  table.sort(files, function(a, b)
    return a[1] < b[1]
  end)
  local lines = {}
  local total = 0
  for _, f in ipairs(files) do
    lines[#lines + 1] = f[1] .. " (" .. f[2] .. " bytes)"
    total = total + f[2]
  end
  lines[#lines + 1] = ""
  lines[#lines + 1] = #files .. " files, " .. total .. " bytes total"
  return table.concat(lines, "\n")
end

return M
