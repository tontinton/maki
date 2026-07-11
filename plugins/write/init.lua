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

local function build_view(content, path, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, write_view_opts(ctx))
  view:set_highlight(content, path:match("%.([^%.]+)$") or "")
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

maki.api.register_tool({
  name = "write",
  kind = "edit",
  mutable_path = "path",
  permission_scopes = "path",
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
    local content = input.content or ""
    if content == "" then
      return ToolView.restore(output, write_view_opts(ctx))
    end
    return build_view(content, input.path or "", ctx)
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

    return {
      llm_output = llm_output,
      body = build_view(content, path, ctx),
      annotation = annotation,
      written_path = path,
    }
  end,
})
