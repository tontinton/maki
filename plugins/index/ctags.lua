-- Fallback indexer for languages maki has no tree-sitter extractor for.
--
-- Universal Ctags gives us a flat symbol list for almost any language, which
-- is a lot cheaper for the model to read than the whole file. It is not a
-- skeleton: ctags knows where a symbol starts, not where it ends, so this
-- emits positions only and says so in the first line. Callers must treat any
-- "no ctags" or "no tags" outcome as a signal to keep the old unsupported
-- error, never as an empty skeleton.

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

local PROBE_TIMEOUT_MS = 5000
local RUN_TIMEOUT_MS = 15000
local MIN_CTAGS_MAJOR = 6
local NO_TAGS = "ctags found no tags"

local available

local function probe()
  if available ~= nil then
    return available
  end
  available = false
  if maki.fn.executable("ctags") ~= 1 then
    return available
  end
  local id = maki.fn.jobstart({ "ctags", "--version" }, { scope = "plugin" })
  if not id then
    return available
  end
  local result = maki.fn.jobwait(id, PROBE_TIMEOUT_MS)
  if not result or result.exit_code ~= 0 then
    return available
  end
  -- Universal Ctags advertises compiled-in features as a "+json" token. JSON
  -- is what makes the output cheap to parse: the classic format repeats whole
  -- source lines, so reading it costs nearly as much as reading the file.
  local major = result.stdout:match("Universal Ctags (%d+)%.")
  available = major ~= nil and tonumber(major) >= MIN_CTAGS_MAJOR and result.stdout:find("+json", 1, true) ~= nil
  return available
end

local function run_ctags(path)
  local id = maki.fn.jobstart({
    "ctags",
    "--output-format=json",
    "--fields=+nK",
    "-f",
    "-",
    path,
  }, { scope = "plugin" })
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
    bucket[#bucket + 1] = { name = tag.name, line = tag.line }
  end
  for _, bucket in pairs(grouped) do
    table.sort(bucket, function(a, b)
      return a.line < b.line
    end)
  end
  return grouped
end

local function format_outline(path, grouped)
  -- The model reads this as prose, so the first line has to kill the
  -- expectation of ranges the real tree-sitter skeletons carry.
  local out = { path .. " (ctags outline, start lines only)" }
  for _, section in ipairs(SECTION_ORDER) do
    local bucket = grouped[section]
    if bucket then
      out[#out + 1] = SECTION_HEADER[section]
      for _, item in ipairs(bucket) do
        out[#out + 1] = "  " .. item.name .. " [" .. item.line .. "]"
      end
    end
  end
  return table.concat(out, "\n")
end

--- Build a flat outline for {path}, or nil when ctags cannot help.
--- Returns (skeleton, nil) on success and (nil, reason) otherwise, so a caller
--- can fall back to its own "unsupported" message.
local function outline(path)
  if not probe() then
    return nil, "ctags is not available"
  end
  local stdout = run_ctags(path)
  if not stdout or stdout == "" then
    return nil, NO_TAGS
  end
  local grouped = group_tags(parse_tags(stdout))
  if next(grouped) == nil then
    return nil, NO_TAGS
  end
  return format_outline(path, grouped)
end

return {
  outline = outline,
  probe = probe,
  format_outline = format_outline,
  group_tags = group_tags,
  parse_tags = parse_tags,
}
