-- @-mentions: typing "@" in the prompt completes project file paths inline.
-- Project files match fuzzily (maki.fn.matchfuzzypos), so "fmini" finds
-- "plugins/file_mention/init.lua" without knowing where it lives.
-- Queries starting with ~/, /, ./ or ../ complete shell-style against the
-- filesystem instead; accepting a directory keeps the popup open on its
-- contents.

local opts = maki.api.register_options({
  result_limit = { default = 10, min = 1, desc = "Max files shown in the completion popup." },
  cache_ms = {
    default = 2000,
    min = 0,
    desc = "How long the project file list is reused across keystrokes, in milliseconds.",
  },
})

local function is_path_query(query)
  return query:sub(1, 2) == "~/"
    or query:sub(1, 1) == "/"
    or query:sub(1, 2) == "./"
    or query:sub(1, 3) == "../"
    or query == "~"
    or query == "."
    or query == ".."
end

local function path_items(query)
  -- Bare "~", "." or ".." has not named a directory yet; offer the slash.
  if query == "~" or query == "." or query == ".." then
    return { { label = query .. "/", insert = "@" .. query .. "/" } }
  end
  local dir_part, partial = query:match("^(.*/)([^/]*)$")
  if not dir_part then
    return {}
  end
  local fs_dir = dir_part
  if fs_dir:sub(1, 2) == "~/" then
    local home = maki.uv.os_homedir()
    if not home then
      return {}
    end
    fs_dir = home .. fs_dir:sub(2)
  end
  local entries = maki.fs.dir(fs_dir)
  if not entries then
    return {}
  end
  local prefix = partial:lower()
  local show_hidden = partial:sub(1, 1) == "."
  local dirs, files = {}, {}
  for _, entry in ipairs(entries) do
    local name, kind = entry[1], entry[2]
    local hidden = name:sub(1, 1) == "."
    if (show_hidden or not hidden) and name:lower():sub(1, #prefix) == prefix then
      if kind == "directory" then
        dirs[#dirs + 1] = name
      else
        files[#files + 1] = name
      end
    end
  end
  table.sort(dirs)
  table.sort(files)
  local items = {}
  -- Directories re-insert the trigger so accepting one keeps completing
  -- inside it; files insert the finished path.
  for _, name in ipairs(dirs) do
    if #items >= opts.result_limit then
      break
    end
    items[#items + 1] = { label = dir_part .. name .. "/", insert = "@" .. dir_part .. name .. "/" }
  end
  for _, name in ipairs(files) do
    if #items >= opts.result_limit then
      break
    end
    items[#items + 1] = { label = dir_part .. name, insert = dir_part .. name }
  end
  return items
end

-- Walking the project is the expensive half, and its result does not change
-- between the keystrokes of one query, so it is listed once and reused. The
-- list stays mtime-sorted and matchfuzzy sorts stably, so entries that score
-- the same still come back most-recently-touched first.
local cache = { paths = nil, at = 0 }

local function project_paths()
  local now = maki.uv.hrtime()
  if cache.paths and (now - cache.at) / 1e6 < opts.cache_ms then
    return cache.paths
  end
  local files = maki.fs.glob("**/*", { gitignore = true, sort = "mtime" })
  if not files then
    return nil
  end
  local cwd = maki.uv.cwd()
  local paths = {}
  for i, f in ipairs(files) do
    paths[i] = cwd and maki.fs.relpath(cwd, f) or f
  end
  cache.paths = paths
  cache.at = now
  return paths
end

local function fuzzy_items(query)
  local paths = project_paths()
  if not paths then
    return nil
  end
  local matches, positions = table.unpack(maki.fn.matchfuzzypos(paths, query, {
    path = true,
    limit = opts.result_limit,
  }))
  local items = {}
  for i, path in ipairs(matches) do
    items[i] = { label = path, indices = positions[i] }
  end
  return items
end

maki.api.register_input_completer({
  trigger = "@",
  name = "files",
  handler = function(query)
    if is_path_query(query) then
      return path_items(query)
    end
    return fuzzy_items(query)
  end,
})
