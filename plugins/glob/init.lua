local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")

local DEFAULT_SEARCH_LIMIT = 100
local NO_FILES_FOUND = "No files found"

local function glob_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 3, keep = "head" }
end

maki.api.register_tool({
  name = "glob",
  kind = "search",
  description = [[Find files by glob pattern.

- Respects .gitignore.
- Returns absolute paths sorted by modification time (newest first).
- Prefer speculative parallel searches over sequential rounds of glob+grep.]],

  schema = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "Glob pattern (e.g. **/*.rs, src/**/*.ts)" },
      path = { type = "string", description = "Directory to search in (default: cwd)" },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    local spans = { { shorten_path(input.pattern or ""), "tool" } }
    if input.path then
      spans[#spans + 1] = { " in ", "dim" }
      spans[#spans + 1] = { shorten_path(input.path), "path" }
    end
    buf:line(spans)
    return buf
  end,

  restore = function(_input, output, _is_error, ctx)
    return ToolView.restore(output, glob_view_opts(ctx))
  end,

  handler = function(input, ctx)
    local pattern = input.pattern
    if not pattern then
      return "error: pattern is required"
    end

    local limit = ctx:config("search_result_limit", DEFAULT_SEARCH_LIMIT)
    local max_lines = ctx:config("max_output_lines", 2000)
    local max_bytes = ctx:config("max_output_bytes", (50 * 1024))

    local files, err = maki.fs.glob(pattern, {
      path = input.path,
      gitignore = true,
      sort = "mtime",
      limit = limit,
    })

    if not files then
      return "error: " .. err
    end

    if #files == 0 then
      return { llm_output = NO_FILES_FOUND }
    end

    local lines = {}
    for i, f in ipairs(files) do
      lines[i] = shorten_path(f)
    end
    local text = table.concat(lines, "\n")
    local llm_output = truncate(text, max_lines, max_bytes)

    local buf = maki.ui.buf()
    local view = ToolView.new(buf, glob_view_opts(ctx))
    for _, line in ipairs(lines) do
      view:append(line)
    end
    view:finish()
    buf:on("click", function()
      view:toggle()
    end)

    return {
      llm_output = llm_output,
      body = buf,
    }
  end,
})
