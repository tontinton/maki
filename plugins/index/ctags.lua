-- Fallback indexer for languages maki has no tree-sitter extractor for.
--
-- Universal Ctags gives us a flat symbol list for almost any language, which
-- is a lot cheaper for the model to read than the whole file. It is not a
-- skeleton: symbols are grouped by kind and carry start lines, end lines, and
-- the enclosing scope when the parser reports them. Callers must treat a nil
-- result as a signal to keep the old unsupported error, never as an empty
-- skeleton.

local UNKNOWN_KIND = "tags"

local RUN_TIMEOUT_MS = 15000

local function run_ctags(path)
  local id = maki.fn.jobstart({
    "ctags",
    "--options=NONE",
    "--output-format=json",
    "--fields=+nKe",
    "--sort=no",
    "-f",
    "-",
    path,
  })
  if not id then
    return nil
  end
  local result = maki.fn.jobwait(id, RUN_TIMEOUT_MS)
  if not result or result.exit_code ~= 0 or result.truncated then
    return nil
  end
  return result.stdout
end

local function parse_tags(stdout)
  local tags = {}
  for line in stdout:gmatch("[^\n]+") do
    local tag = maki.json.decode(line)
    -- ctags also emits pseudo-tags ("_type":"ptag") we have no use for.
    if tag and tag._type == "tag" and tag.name and tag.line then
      tags[#tags + 1] = tag
    end
  end
  return tags
end

--- Groups tags by kind name, in file order.
local function group_tags(tags)
  local grouped = {}
  for _, tag in ipairs(tags) do
    local bucket = grouped[tag.kind or UNKNOWN_KIND]
    if not bucket then
      bucket = {}
      grouped[tag.kind or UNKNOWN_KIND] = bucket
    end
    bucket[#bucket + 1] = tag
  end
  return grouped
end

local function format_outline(path, grouped)
  local kinds = {}
  for kind in pairs(grouped) do
    kinds[#kinds + 1] = kind
  end
  table.sort(kinds)

  local out = { path .. " (ctags outline)" }
  for _, kind in ipairs(kinds) do
    out[#out + 1] = kind .. ":"
    for _, item in ipairs(grouped[kind]) do
      local end_line = item["end"]
      local name = item.scope and (item.scope .. "." .. item.name) or item.name
      local span = end_line and end_line > item.line and (item.line .. "-" .. end_line) or item.line
      out[#out + 1] = "  " .. name .. " [" .. span .. "]"
    end
  end
  return table.concat(out, "\n")
end

--- Build a flat outline for {path}, or nil when ctags cannot help.
local function outline(path)
  local stdout = run_ctags(path)
  if not stdout then
    return nil
  end
  local grouped = group_tags(parse_tags(stdout))
  if next(grouped) == nil then
    return nil
  end
  return format_outline(path, grouped)
end

return {
  outline = outline,
  format_outline = format_outline,
  group_tags = group_tags,
  parse_tags = parse_tags,
}
