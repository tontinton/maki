local function normalize_sep(s)
  return s:gsub("\\", "/")
end

local function shorten_path(path)
  local p = normalize_sep(path)
  local cwd = maki.uv.cwd()
  if cwd then
    cwd = normalize_sep(cwd)
    if p:sub(1, #cwd + 1) == cwd .. "/" then
      local rel = p:sub(#cwd + 2)
      return rel == "" and "." or rel
    end
  end
  local home = maki.uv.os_homedir()
  if home then
    home = normalize_sep(home)
    if p:sub(1, #home + 1) == home .. "/" then
      local rel = p:sub(#home + 2)
      return rel == "" and "~" or "~/" .. rel
    end
  end
  return path
end

return shorten_path
