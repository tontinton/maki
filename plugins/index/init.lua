local indexer = require("indexer")

local function normalize(p)
  local cwd = maki.uv.cwd()
  if cwd and p:sub(1, #cwd + 1) == cwd .. "/" then
    local rel = p:sub(#cwd + 2)
    return rel == "" and "." or rel
  end
  local home = maki.uv.os_homedir()
  if home and p:sub(1, #home + 1) == home .. "/" then
    local rel = p:sub(#home + 2)
    return rel == "" and "~" or "~/" .. rel
  end
  return p
end

local KEYWORDS = {
  pub = true,
  fn = true,
  struct = true,
  enum = true,
  trait = true,
  type = true,
  impl = true,
  mod = true,
  const = true,
  static = true,
  async = true,
  class = true,
  interface = true,
  export = true,
  ["macro_rules!"] = true,
}

local function split_trailing_range(line)
  local bracket_start = line:find("%[%d[%d%-%,% ]*%]$")
  if not bracket_start or bracket_start <= 1 then
    return nil
  end
  return line:sub(1, bracket_start - 1), line:sub(bracket_start)
end

local function styled_section_line(buf, line)
  local before, range = split_trailing_range(line)
  if range then
    buf:line({ { before, "section" }, { range, "line_nr" } })
  else
    buf:line({ { line, "section" } })
  end
end

local function styled_content_line(buf, line)
  local leading, content
  if line:sub(1, 2) == "  " then
    leading = "  "
    content = line:sub(3)
  else
    leading = ""
    content = line
  end

  local spans = {}
  if leading ~= "" then
    spans[#spans + 1] = { leading }
  end

  local kw_found = nil
  for kw in pairs(KEYWORDS) do
    local kw_len = #kw
    if content:sub(1, kw_len) == kw then
      local next_char = content:sub(kw_len + 1, kw_len + 1)
      if next_char == " " or next_char == "(" then
        kw_found = kw
        break
      end
    end
  end

  local rest
  if kw_found then
    spans[#spans + 1] = { kw_found, "keyword" }
    rest = content:sub(#kw_found + 1)
  else
    rest = content
  end

  local before, range = split_trailing_range(rest)
  if range then
    spans[#spans + 1] = { before, "tool" }
    spans[#spans + 1] = { range, "line_nr" }
  else
    spans[#spans + 1] = { rest, "tool" }
  end

  buf:line(spans)
end

local function is_section_header(line)
  local trimmed = line:match("^(.-)%s*$")
  if trimmed:sub(-1) == ":" then
    return true
  end
  if trimmed:sub(-1) == "]" and trimmed:find(": %[") then
    return true
  end
  return false
end

local function build_styled_buf(text)
  local buf = maki.ui.buf()
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    if line == "" then
      buf:line("")
    elseif is_section_header(line) then
      styled_section_line(buf, line)
    else
      styled_content_line(buf, line)
    end
  end
  return buf
end

maki.api.register_tool({
  name = "index",
  description = [[
Return a compact overview of a source file: imports, type definitions, function signatures, and structure with their line numbers surrounded by []. ~70-90% more efficient than reading the full file.

- Use this FIRST to understand file structure before using read with offset/limit.
- Supports source files in different programming languages and markdown.
- Falls back with an error on unsupported languages. Use read instead.]],

  schema = {
    type = "object",
    properties = {
      path = { type = "string", description = "Absolute path to the file", required = true },
    },
  },
  render = {
    header_style = "path",
    always_annotate = true,
  },
  summary = function(input)
    return normalize(input.path)
  end,
  handler = function(input, ctx)
    local path = input.path
    if not path then
      return "error: path is required"
    end

    local ext = path:match("%.([^%.]+)$")
    if not ext then
      return "DELEGATE_NATIVE"
    end

    local lang = indexer.EXT_TO_LANG[ext]
    if not lang then
      return "DELEGATE_NATIVE"
    end

    local config = ctx:config()
    local max_file_size = (config and config.index_max_file_size) or (2 * 1024 * 1024)
    local ok_meta, meta = pcall(maki.fs.metadata, path)
    if ok_meta and meta.size > max_file_size then
      return "error: File too large ("
        .. meta.size
        .. " bytes, max "
        .. max_file_size
        .. "). Use read with offset/limit instead."
    end

    local ok, source = pcall(maki.fs.read, path)
    if not ok then
      return "error: " .. tostring(source)
    end

    local result, err = indexer.index_source(source, lang)
    if not result then
      return "error: " .. tostring(err)
    end

    local buf = build_styled_buf(result)

    return {
      output = result,
      render_buf = buf,
    }
  end,
})
