local indexer = require("indexer")
local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")

local TRUNCATED_SUFFIX = indexer.TRUNCATED_SUFFIX
local TRUNCATED_INFIX = " more truncated]"

local function split_trailing_range(line)
  local pos = line:find(" %[%d[%d%-,]*%]$")
  if not pos then
    return nil, nil
  end
  return line:sub(1, pos - 1), line:sub(pos + 1)
end

local function is_section_header(line)
  if line:sub(1, 1) == " " then
    return false
  end
  local trimmed = line:match("^(.-)%s*$")
  if trimmed:sub(-1) == ":" then
    return true
  end
  local body = split_trailing_range(trimmed)
  return body ~= nil and body:sub(-1) == ":"
end

local function infer_line_meta(line)
  if line == "" then
    return nil
  end
  if is_section_header(line) then
    local body, range = split_trailing_range(line)
    if range then
      return { tag = "section", body = body .. " ", range = range }
    end
    return { tag = "section" }
  end
  if
    line:sub(-#TRUNCATED_SUFFIX) == TRUNCATED_SUFFIX
    or (line:match("^%s*%[") and line:sub(-#TRUNCATED_INFIX) == TRUNCATED_INFIX)
  then
    return { tag = "dim" }
  end
  local body, range = split_trailing_range(line)
  if range then
    return { body = body, range = range }
  end
  return nil
end

local function render_skeleton(view, text, meta)
  text = text:gsub("\n+$", "") .. "\n"
  local hl_entries = {}
  local line_nr = 0
  for line in text:gmatch("([^\n]*)\n") do
    line_nr = line_nr + 1
    local m = (meta and meta[line_nr]) or infer_line_meta(line)
    if line == "" then
      view:append("")
    elseif m and m.tag == "section" then
      if m.range then
        view:append({ { m.body, "section" }, { m.range, "line_nr" } })
      else
        view:append({ { line, "section" } })
      end
    elseif m and m.tag == "dim" then
      view:append({ { line, "dim" } })
    elseif m and m.range then
      view:append({ { m.body }, { " " }, { m.range, "line_nr" } })
      hl_entries[#hl_entries + 1] = { idx = #view.all_lines, text = m.body, range = m.range }
    else
      view:append({ { line } })
      hl_entries[#hl_entries + 1] = { idx = #view.all_lines, text = line }
    end
  end
  return hl_entries
end

local function apply_highlights(view, hl_entries, ext)
  if #hl_entries == 0 then
    return
  end
  local texts = {}
  for _, e in ipairs(hl_entries) do
    texts[#texts + 1] = e.text
  end
  local highlighted = maki.ui.highlight(table.concat(texts, "\n"), ext, { independent = true })
  if not highlighted then
    return
  end
  for i, e in ipairs(hl_entries) do
    local hl_spans = highlighted[i]
    if hl_spans then
      local new_line = {}
      for _, span in ipairs(hl_spans) do
        new_line[#new_line + 1] = span
      end
      if e.range then
        new_line[#new_line + 1] = { " " }
        new_line[#new_line + 1] = { e.range, "line_nr" }
      end
      view:update_line(e.idx, new_line)
    end
  end
  view:flush()
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

local function render_index(skeleton, path, ctx, ext, line_meta)
  local tol = ctx:tool_output_lines()
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.index) or 5,
    keep = "head",
  })
  buf:on("click", function()
    view:toggle()
  end)
  local hl_entries = render_skeleton(view, skeleton, line_meta)
  view:finish()

  if ext then
    maki.async.run(function()
      apply_highlights(view, hl_entries, ext)
    end)
  end

  local line_count = select(2, skeleton:gsub("\n", "\n")) + 1
  return buf, render_header(path, line_count)
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use **index** before **read**.",
})

maki.api.register_prompt_hint({
  slot = "efficient_tools",
  content = "index",
})

maki.api.register_tool({
  name = "index",
  kind = "read",
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
  restore = function(input, output, _is_error, ctx)
    local ext = input.path:match("%.([^%.]+)$") or ""
    local buf, header = render_index(output, input.path, ctx, ext)
    return { body = buf, header = header }
  end,
  handler = function(input, ctx)
    local path = input.path
    if not path then
      return "error: path is required"
    end

    local meta = maki.fs.metadata(path)
    if meta and meta.is_dir then
      return {
        llm_output = "Path is a directory. Use index on files or use the read or glob tool to list directories.",
        is_error = true,
      }
    end

    local filename = path:match("([^/]+)$")
    local lang = indexer.FILENAME_TO_LANG[filename]

    if not lang then
      local ext = path:match("%.([^%.]+)$")
      if not ext then
        return { llm_output = "Unsupported file type: (no extension). Use the read tool instead.", is_error = true }
      end

      lang = indexer.EXT_TO_LANG[ext]
      if not lang then
        return { llm_output = "Unsupported file type: ." .. ext .. ". Use the read tool instead.", is_error = true }
      end
    end

    local config = ctx:config()
    local max_file_size = (config and config.index_max_file_size) or (2 * 1024 * 1024)
    if meta and meta.size > max_file_size then
      return "error: File too large ("
        .. meta.size
        .. " bytes, max "
        .. max_file_size
        .. "). Use read with offset/limit instead."
    end

    local source, err = maki.fs.read(path)
    if not source then
      return "error: " .. err
    end

    local skeleton, line_meta = indexer.index_source(source, lang)
    if not skeleton then
      return "error: " .. tostring(line_meta)
    end

    local ext = indexer.LANG_TO_EXT[lang] or path:match("%.([^%.]+)$") or ""
    local buf, header = render_index(skeleton, path, ctx, ext, line_meta)
    return {
      llm_output = skeleton:gsub("\n+$", ""),
      body = buf,
      header = header,
    }
  end,
})
