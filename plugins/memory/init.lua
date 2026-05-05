local ToolView = require("tool_view")
local helpers = require("memory_helpers")

local function memories_path_suffix()
  local cwd = maki.uv.cwd()
  local root = maki.fs.root(cwd, ".git") or cwd
  return "projects/" .. helpers.project_id(root) .. "/memories"
end

local function resolve_dir(check_legacy)
  if check_legacy then
    local legacy = maki.env.legacy_dir()
    if legacy then
      local dir = maki.fs.joinpath(legacy, memories_path_suffix())
      local meta = maki.fs.metadata(dir)
      if meta and meta.is_dir then
        return dir
      end
    end
  end
  local state = maki.env.state_dir()
  if not state then
    return nil, "cannot resolve state dir"
  end
  return maki.fs.joinpath(state, memories_path_suffix())
end

local function render_content(content, path, ctx)
  local buf = maki.ui.buf()
  local tol = ctx:tool_output_lines()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.other) or 20,
    keep = "head",
  })
  buf:on("click", function()
    view:toggle()
  end)

  local ext = path:match("%.([^%.]+)$") or "md"
  local highlighted = maki.ui.highlight(content, ext)
  if highlighted then
    for idx, hl_line in ipairs(highlighted) do
      local spans = { { string.format("%4d ", idx), "line_nr" } }
      for _, seg in ipairs(hl_line) do
        spans[#spans + 1] = seg
      end
      view:append(spans)
    end
  else
    for line in (content .. "\n"):gmatch("([^\n]*)\n") do
      view:append(line)
    end
  end
  view:finish()
  return buf
end

local function cmd_view(path, dir, ctx)
  if not path then
    return helpers.list_memories(dir)
  end
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  local ok, content = pcall(maki.fs.read, file_path)
  if not ok then
    return nil, "read error: " .. tostring(content)
  end
  return {
    llm_output = content,
    body = render_content(content, path, ctx),
  }
end

local function cmd_write(path, content, dir, ctx)
  local lc = helpers.count_lines(content)
  if lc > helpers.MAX_LINES_PER_FILE then
    return nil, "content exceeds " .. helpers.MAX_LINES_PER_FILE .. " lines (" .. lc .. " lines); reduce content size"
  end
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  local meta = maki.fs.metadata(file_path)
  local existing_size = meta and meta.size or 0
  if helpers.dir_total_bytes(dir) - existing_size + #content > helpers.MAX_DIR_BYTES then
    return nil, "memory directory would exceed " .. helpers.MAX_DIR_BYTES .. " byte limit; delete stale entries first"
  end
  maki.fs.mkdir(dir, { parents = true })
  local ok, write_err = maki.fs.write(file_path, content)
  if not ok then
    return nil, "write error: " .. tostring(write_err)
  end
  return {
    llm_output = "wrote " .. path .. " (" .. lc .. " lines)",
    body = render_content(content, path, ctx),
  }
end

local function cmd_delete(path, dir)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  if not maki.fs.metadata(file_path) then
    return nil, "'" .. path .. "' does not exist"
  end
  local ok, rm_err = maki.fs.rm(file_path)
  if not ok then
    return nil, "delete error: " .. tostring(rm_err)
  end
  return "deleted " .. path
end

maki.api.register_tool({
  name = "memory",
  description = "Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.\n\n"
    .. "- Save important context before compaction or to build up project knowledge.\n"
    .. "- Keep entries concise and current. Delete outdated information.",

  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "Command: view, write, delete", required = true },
      path = { type = "string", description = "Relative path (e.g. 'architecture.md'). Omit to list all." },
      content = { type = "string", description = "File content for 'write'" },
    },
  },

  header = function(input)
    if input.path then
      return (input.command or "") .. " " .. input.path
    end
    return input.command
  end,

  handler = function(input, ctx)
    local cmd = input.command
    local dir, dir_err = resolve_dir(cmd == "view")
    if not dir then
      return "error: " .. dir_err
    end

    local result, err
    if cmd == "view" then
      result, err = cmd_view(input.path, dir, ctx)
    elseif cmd == "write" then
      if not input.path then
        return "error: 'path' is required for write"
      end
      if not input.content then
        return "error: 'content' is required for write"
      end
      result, err = cmd_write(input.path, input.content, dir, ctx)
    elseif cmd == "delete" then
      if not input.path then
        return "error: 'path' is required for delete"
      end
      result, err = cmd_delete(input.path, dir)
    else
      return "error: unknown command '" .. tostring(cmd) .. "'. Valid commands: view, write, delete"
    end
    if err then
      return "error: " .. err
    end
    return result
  end,
})

maki.api.register_command({
  name = "/memory",
  description = "View, edit, and delete memory files",
  handler = function()
    local dir = resolve_dir(true)
    if not dir then
      maki.ui.flash("Cannot resolve memory directory")
      return
    end

    local entries = helpers.collect_file_entries(dir)
    if #entries == 0 then
      maki.ui.flash("No memory files yet")
      return
    end
    table.sort(entries, function(a, b)
      return a[1] < b[1]
    end)

    while true do
      local event = maki.ui.select(entries, {
        title = " Memory Files ",
        footer = { { "enter", "open" }, { "ctrl+o", "edit" }, { "ctrl+d", "delete" } },
        format = function(item)
          return { label = item[1], detail = "(" .. item[2] .. " bytes)" }
        end,
        on_delete = true,
      })

      if not event or event.type == "close" then
        return
      elseif event.type == "choice" or event.type == "open_editor" then
        local item = entries[event.index]
        maki.ui.open_editor(maki.fs.joinpath(dir, item[1]))
        return
      elseif event.type == "delete" then
        local item = entries[event.index]
        local ok, err = maki.fs.rm(maki.fs.joinpath(dir, item[1]))
        if ok then
          maki.ui.flash("Deleted " .. item[1])
          table.remove(entries, event.index)
          if #entries == 0 then
            return
          end
        else
          maki.ui.flash("Delete failed: " .. tostring(err))
        end
      end
    end
  end,
})
