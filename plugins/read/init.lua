local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")

local DESCRIPTION = [[Read a file or directory. Returns contents with line numbers (1-indexed).

- Supports absolute, relative, and ~/ paths.
- Use the index tool first to locate relevant line ranges.
- **Always include offset and limit**. Defaults: no offset = start at 1; no limit = up to 2000 lines.
- Use truncation hints (e.g. "truncated lines X-Y") to continue with the correct offset.
- For files >500 lines, always **read** with offset/limit (only what you need).
- Do not reread the same range (same file and same offset).
- Prefer grep to locate content instead of scanning full files.
- Call in parallel when reading multiple files.
- Avoid tiny repeated slices - read a larger window if you need more context.]]

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_LINE_BYTES = 500

local function line_nr_fmt(count)
  local w = math.max(1, math.floor(math.log(count + 1, 10)) + 1)
  return "%" .. w .. "d "
end

local function truncate_bytes(line, max_bytes)
  if #line <= max_bytes then
    return line
  end
  local i = max_bytes
  while i > 0 and line:byte(i) >= 0x80 and line:byte(i) < 0xC0 do
    i = i - 1
  end
  if i > 0 and line:byte(i) >= 0xC0 then
    i = i - 1
  end
  return line:sub(1, i) .. "..."
end

local function read_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.read) or 10, keep = "head" }
end

local function apply_highlights(view, hl_lines, ext)
  local texts = {}
  for _, fl in ipairs(hl_lines) do
    texts[#texts + 1] = fl.text
  end
  local highlighted = maki.ui.highlight(table.concat(texts, "\n"), ext)
  if not highlighted then
    return
  end
  for i, fl in ipairs(hl_lines) do
    local hl_spans = highlighted[i]
    if hl_spans then
      view:update_line(fl.idx, { view.all_lines[fl.idx][1], table.unpack(hl_spans) })
    end
  end
  view:flush()
end

local function build_file_view(lines, start_line, total_lines, path, ctx, sync)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, read_view_opts(ctx))
  local nr_fmt = line_nr_fmt(total_lines)

  local hl_lines = {}
  for i, line in ipairs(lines) do
    view:append({ { string.format(nr_fmt, start_line + i - 1), "line_nr" }, { line } })
    hl_lines[#hl_lines + 1] = { idx = #view.all_lines, text = line }
  end

  local trunc_start = start_line + #lines
  if trunc_start <= total_lines then
    view:append({
      {
        string.format(
          "... Truncated %d lines. Use offset=%d to read further.",
          total_lines - trunc_start + 1,
          trunc_start
        ),
        "dim",
      },
    })
  end

  view:finish()

  local ext = path:match("%.([^%.]+)$") or ""
  if sync then
    apply_highlights(view, hl_lines, ext)
  else
    maki.async.run(function()
      apply_highlights(view, hl_lines, ext)
    end)
  end

  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

local function build_dir_view(text, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, read_view_opts(ctx))
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    view:append(line)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

local function read_file(path, offset, limit, ctx)
  local content, err = maki.fs.read(path)
  if not content then
    return { llm_output = "read error: " .. tostring(err), is_error = true }
  end

  local all_lines = {}
  local pos = 1
  while pos <= #content do
    local nl = content:find("\n", pos, true)
    local raw = nl and content:sub(pos, nl - 1) or content:sub(pos)
    all_lines[#all_lines + 1] = raw:find("\r$") and raw:sub(1, -2) or raw
    pos = nl and nl + 1 or #content + 1
  end
  local total_lines = #all_lines

  local config = ctx:config()
  local start = math.max(offset or 1, 1)
  local max_lines = limit or (config and config.max_output_lines) or DEFAULT_MAX_OUTPUT_LINES
  local max_line_bytes = (config and config.max_line_bytes) or DEFAULT_MAX_LINE_BYTES

  local lines = {}
  for i = start, math.min(start + max_lines - 1, total_lines) do
    lines[#lines + 1] = truncate_bytes(all_lines[i], max_line_bytes)
  end

  ctx:record_read(path)

  local parts = {}
  for i, line in ipairs(lines) do
    parts[#parts + 1] = (start + i - 1) .. ": " .. line
  end
  local llm_output = table.concat(parts, "\n")

  local trunc_start = start + #lines
  if trunc_start <= total_lines then
    llm_output = llm_output
      .. string.format(
        "\n\n...\n\nTruncated lines: %d-%d. Use offset=%d to read further.",
        trunc_start,
        total_lines,
        trunc_start
      )
  end

  local shown = #lines
  local annotation = shown < total_lines and string.format("%d of %d lines", shown, total_lines)
    or string.format("%d lines", shown)

  local basename = path:match("([^/]+)$")
  if not ctx:is_instruction_file(basename) then
    local parent = maki.fs.dirname(path)
    if parent then
      local instructions = ctx:find_instructions(parent)
      if #instructions > 0 then
        return {
          llm_output = llm_output,
          body = build_file_view(lines, start, total_lines, path, ctx),
          annotation = annotation,
          instructions = instructions,
        }
      end
    end
  end

  return {
    llm_output = llm_output,
    body = build_file_view(lines, start, total_lines, path, ctx),
    annotation = annotation,
  }
end

local function list_dir(path, ctx)
  local entries = maki.fs.dir(path)
  if not entries then
    return { llm_output = "read error: cannot read directory: " .. path, is_error = true }
  end

  local sorted = {}
  for _, entry in ipairs(entries) do
    local name, typ = entry[1], entry[2]
    if typ == "directory" then
      sorted[#sorted + 1] = { name .. "/", true }
    elseif not ctx:is_instruction_file(name) then
      sorted[#sorted + 1] = { name, false }
    end
  end
  table.sort(sorted, function(a, b)
    if a[2] ~= b[2] then
      return a[2]
    end
    return a[1] < b[1]
  end)

  local names = {}
  for _, e in ipairs(sorted) do
    names[#names + 1] = e[1]
  end
  local text = table.concat(names, "\n")

  local instructions = ctx:find_instructions(path)
  local result = {
    llm_output = text,
    body = build_dir_view(text, ctx),
    annotation = #sorted .. " entries",
  }
  if #instructions > 0 then
    result.instructions = instructions
  end
  return result
end

maki.api.register_tool({
  name = "read",
  kind = "read",
  description = DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file or directory",
        required = true,
        alias = "file_path",
      },
      offset = { type = "integer", description = "Line number to start from (1-indexed)" },
      limit = {
        type = "integer",
        description = "Max number of lines to read. Omitting the limit reads up to 2000 lines.",
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    local s = shorten_path(input.path or "")
    local start = input.offset or 1
    if input.limit then
      s = s .. ":" .. start .. "-" .. (start + input.limit - 1)
    elseif input.offset then
      s = s .. ":" .. start
    end
    buf:line({ { s, "path" } })
    return buf
  end,

  restore = function(input, output, _is_error, ctx)
    local lines, start_line, total_lines = {}, nil, nil
    for raw in (output .. "\n"):gmatch("([^\n]*)\n") do
      local nr, text = raw:match("^%s*(%d+): (.*)$")
      if nr then
        start_line = start_line or tonumber(nr)
        lines[#lines + 1] = text
      else
        local trunc_end = raw:match("Truncated lines: %d+%-(%d+)")
        if trunc_end then
          total_lines = tonumber(trunc_end)
        end
      end
    end
    if #lines == 0 then
      return ToolView.restore(output, read_view_opts(ctx))
    end
    start_line = start_line or 1
    total_lines = total_lines or (start_line + #lines - 1)
    return build_file_view(lines, start_line, total_lines, input.path or "", ctx, true)
  end,

  handler = function(input, ctx)
    local raw = input.path
    if not raw then
      return { llm_output = "error: path is required", is_error = true }
    end
    local path = maki.fs.abspath(raw)
    local meta = maki.fs.metadata(path)
    if not meta then
      return { llm_output = "error: path not found: " .. path, is_error = true }
    end
    if meta.is_dir then
      return list_dir(path, ctx)
    end
    return read_file(path, input.offset, input.limit, ctx)
  end,
})
