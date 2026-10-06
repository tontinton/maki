-- Fallback indexer for languages maki has no tree-sitter extractor for.
--
-- Universal Ctags gives us a flat symbol list for almost any language, which
-- is a lot cheaper for the model to read than the whole file. It is not a
-- skeleton: symbols are grouped by kind and carry start lines, end lines, and
-- the enclosing scope when the parser reports them. Callers must treat a nil
-- result as a signal to keep the old unsupported error, never as an empty
-- skeleton.

local SECTION_HEADER = {
  mod = "mod:",
  consts = "consts:",
  types = "types:",
  classes = "classes:",
  fns = "fns:",
  macros = "macros:",
  tags = "tags:",
}

-- ctags kinds are per-language and mostly stable, so unknown kinds land in
-- "tags" instead of being dropped. Order here is the output order.
local KIND_TO_SECTION = {
  module = "mod",
  namespace = "mod",
  package = "mod",
  constant = "consts",
  variable = "consts",
  -- Some parsers use "variable" for file-level globals, some for locals.
  member = "consts",
  type = "types",
  typedef = "types",
  enum = "types",
  union = "types",
  struct = "types",
  interface = "classes",
  class = "classes",
  trait = "classes",
  implementation = "classes",
  ["function"] = "fns",
  method = "fns",
  macro = "macros",
}

local SECTION_ORDER = { "mod", "consts", "types", "classes", "fns", "macros", "tags" }

local RUN_TIMEOUT_MS = 15000

local function run_ctags(path)
  local id = maki.fn.jobstart({
    "ctags",
    "--output-format=json",
    "--fields=+nKe",
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

local function group_tags(tags)
  local grouped = {}
  for _, tag in ipairs(tags) do
    local section = KIND_TO_SECTION[tag.kind] or "tags"
    local bucket = grouped[section]
    if not bucket then
      bucket = {}
      grouped[section] = bucket
    end
    bucket[#bucket + 1] = tag
  end
  for _, bucket in pairs(grouped) do
    table.sort(bucket, function(a, b)
      return a.line < b.line
    end)
  end
  return grouped
end

local function format_outline(path, grouped)
  local out = { path .. " (ctags outline)" }
  for _, section in ipairs(SECTION_ORDER) do
    local bucket = grouped[section]
    if bucket then
      out[#out + 1] = SECTION_HEADER[section]
      for _, item in ipairs(bucket) do
        local end_line = item["end"]
        local name = item.scope and (item.scope .. "." .. item.name) or item.name
        local span = end_line and end_line > item.line and (item.line .. "-" .. end_line) or item.line
        out[#out + 1] = "  " .. name .. " [" .. span .. "]"
      end
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
