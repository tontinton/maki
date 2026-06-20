local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

local DESCRIPTION = [[Write content to a file, replacing existing content.

- Creates parent directories if needed.
- Always read the file first before writing.
- NEVER create files unless absolutely necessary - prefer editing existing files.
- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested by the User.]]

local function write_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.write) or 10, keep = "head" }
end

local function line_nr_fmt(count)
  local w = math.max(1, math.floor(math.log(count + 1, 10)) + 1)
  return "%" .. w .. "d "
end

local function build_view(lines, path, ctx, sync)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, write_view_opts(ctx))
  local nr_fmt = line_nr_fmt(#lines)

  local hl_lines = {}
  for i, line in ipairs(lines) do
    view:append({ { string.format(nr_fmt, i), "line_nr" }, { line } })
    hl_lines[#hl_lines + 1] = { idx = i, text = line }
  end
  view:finish()

  local ext = path:match("%.([^%.]+)$") or ""
  local function do_highlight()
    local highlighted = maki.ui.highlight(table.concat(lines, "\n"), ext)
    if not highlighted then
      return
    end
    for _, fl in ipairs(hl_lines) do
      local hl_spans = highlighted[fl.idx]
      if hl_spans then
        view:update_line(fl.idx, { view.all_lines[fl.idx][1], table.unpack(hl_spans) })
      end
    end
    view:flush()
  end

  if sync then
    do_highlight()
  else
    maki.async.run(do_highlight)
  end

  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

maki.api.register_tool({
  name = "write",
  kind = "execute",
  mutable_path = "path",
  permission_scope = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      content = {
        type = "string",
        description = "The complete file content to write",
        required = true,
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    buf:line({ { shorten_path(input.path or ""), "path" } })
    return buf
  end,

  restore = function(input, output, _is_error, ctx)
    local lines = {}
    for line in (output .. "\n"):gmatch("([^\n]*)\n") do
      lines[#lines + 1] = line
    end
    if #lines > 0 and lines[#lines] == "" then
      lines[#lines] = nil
    end
    if #lines == 0 then
      return ToolView.restore(output, write_view_opts(ctx))
    end
    return build_view(lines, input.path or "", ctx, true)
  end,

  handler = function(input, ctx)
    local raw = input.path
    if not raw then
      return { llm_output = "error: path is required", is_error = true }
    end
    local content = input.content
    if not content then
      return { llm_output = "error: content is required", is_error = true }
    end

    local path = maki.fs.abspath(raw)

    local ok, err = ctx:check_before_edit(path)
    if not ok then
      return { llm_output = err, is_error = true }
    end

    local parent = maki.fs.dirname(path)
    if parent then
      maki.fs.mkdir(parent, { parents = true })
    end

    local _, write_err = maki.fs.write(path, content)
    if write_err then
      return { llm_output = "write error: " .. tostring(write_err), is_error = true }
    end

    ctx:record_read(path)

    local byte_count = #content
    local rel = shorten_path(path)
    local llm_output = string.format("wrote %d bytes to %s", byte_count, rel)
    local annotation = string.format("%d bytes", byte_count)

    local preview_lines = {}
    for line in (content .. "\n"):gmatch("([^\n]*)\n") do
      preview_lines[#preview_lines + 1] = line
    end
    if #preview_lines > 0 and preview_lines[#preview_lines] == "" then
      preview_lines[#preview_lines] = nil
    end

    return {
      llm_output = llm_output,
      body = build_view(preview_lines, path, ctx),
      annotation = annotation,
      written_path = path,
    }
  end,
})
