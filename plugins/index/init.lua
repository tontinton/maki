local indexer = require("indexer")
local ToolView = require("tool_view")

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

local function extract_line_range(line)
  local pos = line:find("%[%d[%d%-%,% ]*%]$")
  if not pos or pos <= 1 then
    return nil
  end
  return line:sub(1, pos - 1), line:sub(pos)
end

local function append_styled(view, line, style)
  local text, range = extract_line_range(line)
  if range then
    view:append({ { text, style }, { range, "line_nr" } })
  else
    view:append({ { line, style } })
  end
end

local function append_entry(view, line)
  local indent = line:sub(1, 2) == "  " and "  " or ""
  local content = line:sub(#indent + 1)

  local spans = {}
  if indent ~= "" then
    spans[#spans + 1] = { indent }
  end

  for kw in pairs(KEYWORDS) do
    local next_char = content:sub(#kw + 1, #kw + 1)
    if content:sub(1, #kw) == kw and (next_char == " " or next_char == "(") then
      spans[#spans + 1] = { kw, "keyword" }
      content = content:sub(#kw + 1)
      break
    end
  end

  local text, range = extract_line_range(content)
  if range then
    spans[#spans + 1] = { text, "tool" }
    spans[#spans + 1] = { range, "line_nr" }
  else
    spans[#spans + 1] = { content, "tool" }
  end

  view:append(spans)
end

local function is_section_header(line)
  local trimmed = line:match("^(.-)%s*$")
  return trimmed:sub(-1) == ":" or (trimmed:sub(-1) == "]" and trimmed:find(": %[") ~= nil)
end

local function render_skeleton(view, text)
  for line in text:gmatch("([^\n]*)\n?") do
    if line == "" then
      view:append("")
    elseif is_section_header(line) then
      append_styled(view, line, "section")
    else
      append_entry(view, line)
    end
  end
end

local function shorten_path(path)
  local cwd = maki.uv.cwd()
  if cwd and path:sub(1, #cwd + 1) == cwd .. "/" then
    local rel = path:sub(#cwd + 2)
    return rel == "" and "." or rel
  end
  local home = maki.uv.os_homedir()
  if home and path:sub(1, #home + 1) == home .. "/" then
    local rel = path:sub(#home + 2)
    return rel == "" and "~" or "~/" .. rel
  end
  return path
end

local function render_header(path, line_count)
  local buf = maki.ui.buf()
  local spans = { { shorten_path(path), "path" } }
  if line_count then
    spans[#spans + 1] = { " (" .. line_count .. " lines)", "dim" }
  end
  buf:line(spans)
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
  header = function(input)
    return render_header(input.path)
  end,
  handler = function(input, ctx)
    local path = input.path
    if not path then
      return "error: path is required"
    end

    local meta_ok, meta = pcall(maki.fs.metadata, path)
    if meta_ok and meta.is_dir then
      return {
        llm_output = "Path is a directory. Use index on files or use the read or glob tool to list directories.",
        is_error = true,
      }
    end

    local ext = path:match("%.([^%.]+)$")
    if not ext then
      return { llm_output = "Unsupported file type: (no extension). Use the read tool instead.", is_error = true }
    end

    local lang = indexer.EXT_TO_LANG[ext]
    if not lang then
      return { llm_output = "Unsupported file type: ." .. ext .. ". Use the read tool instead.", is_error = true }
    end

    local config = ctx:config()
    local max_file_size = (config and config.index_max_file_size) or (2 * 1024 * 1024)
    if meta_ok and meta.size > max_file_size then
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

    local skeleton, err = indexer.index_source(source, lang)
    if not skeleton then
      return "error: " .. tostring(err)
    end

    local tol = ctx:tool_output_lines()
    local buf = maki.ui.buf()
    local view = ToolView.new(buf, {
      max_lines = (tol and tol.index) or 5,
      keep = "head",
    })
    buf:on("click", function()
      view:toggle()
    end)

    render_skeleton(view, skeleton)
    view:finish()

    local line_count = select(2, skeleton:gsub("\n", "\n")) + 1
    return {
      llm_output = skeleton,
      body = buf,
      header = render_header(path, line_count),
    }
  end,
})
