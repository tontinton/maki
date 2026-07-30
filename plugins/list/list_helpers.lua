local dir_listing = require("maki.dir_listing")

local M = {}

-- Pure result building; init.lua attaches the body view, which needs a task scope.

function M.handler(input, ctx)
  local raw = input.path
  if not raw then
    return { llm_output = "error: path is required", is_error = true }
  end
  local path = maki.fs.abspath(raw)
  local meta = maki.fs.metadata(path)
  if not meta then
    return { llm_output = "error: path not found: " .. path, is_error = true }
  end
  if not meta.is_dir then
    return { llm_output = "error: path is not a directory: " .. path, is_error = true }
  end

  local listing, err = dir_listing.list(path, ctx)
  if not listing then
    return { llm_output = "error: " .. tostring(err), is_error = true }
  end

  local result = {
    llm_output = listing.text,
    annotation = listing.count .. " entries",
  }
  if listing.instructions then
    result.instructions = listing.instructions
  end
  return result
end

return M
