-- Shared directory listing for index and read plugins.
-- Lists entries, filters instruction files, sorts dirs before files, and
-- renders the listing so every caller shows a directory the same way.

local ToolView = require("maki.tool_view")

local DEFAULT_MAX_LINES = 10

local M = {}

function M.list(path, ctx)
  local entries, err = maki.fs.dir(path)
  if not entries then
    return nil, err
  end

  local dirs = {}
  local files = {}
  for _, entry in ipairs(entries) do
    local name, typ = entry[1], entry[2]
    if typ == "directory" then
      dirs[#dirs + 1] = name .. "/"
    elseif not ctx:is_instruction_file(name) then
      files[#files + 1] = name
    end
  end
  table.sort(dirs)
  table.sort(files)
  for _, f in ipairs(files) do
    dirs[#dirs + 1] = f
  end

  local instructions = ctx:find_instructions(path)

  local result = {
    names = dirs,
    text = table.concat(dirs, "\n"),
    count = #dirs,
  }
  if #instructions > 0 then
    result.instructions = instructions
  end
  return result
end

function M.view(text, ctx)
  local tol = ctx:tool_output_lines()
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = (tol and tol.read) or DEFAULT_MAX_LINES, keep = "head" })
  view:append_text(text)
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

return M
