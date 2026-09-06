-- @-mentions: typing "@" in the prompt completes project file paths inline.
-- Queries starting with ~/, /, ./ or ../ complete shell-style against the
-- filesystem instead of the project glob; accepting a directory keeps the
-- popup open on its contents.

local opts = maki.api.register_options({
  result_limit = { default = 10, min = 1, desc = "Max files shown in the completion popup." },
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

local function glob_items(query)
  local pattern
  if query == "" then
    pattern = "**/*"
  elseif query:find("/", 1, true) then
    pattern = "**/" .. query .. "*"
  else
    pattern = "**/*" .. query .. "*"
  end
  local files = maki.fs.glob(pattern, {
    gitignore = true,
    sort = "mtime",
    limit = opts.result_limit,
  })
  if not files then
    return nil
  end
  local cwd = maki.uv.cwd()
  local items = {}
  for _, f in ipairs(files) do
    items[#items + 1] = { label = cwd and maki.fs.relpath(cwd, f) or f }
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
    return glob_items(query)
  end,
})
