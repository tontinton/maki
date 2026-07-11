-- Project-scoped memory: files are the granularity unit, frontmatter tags are the only index.
local ToolView = require("maki.tool_view")
local helpers = require("memory_helpers")
local ListPicker = require("maki.list_picker")

local HINT_MAX_TAGS = 50

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
  local state_dir = maki.env.state_dir()
  if not state_dir then
    return nil, "cannot resolve state dir"
  end
  return maki.fs.joinpath(state_dir, memories_path_suffix())
end

local function sorted_tag_keys(counts)
  local keys = {}
  for t in pairs(counts) do
    keys[#keys + 1] = t
  end
  table.sort(keys, function(a, b)
    if counts[a] ~= counts[b] then
      return counts[a] > counts[b]
    end
    return a < b
  end)
  return keys
end

maki.api.register_prompt_hint({
  prompt = "system",
  slot = "after_instructions",
  content = function()
    local dir = resolve_dir(true)
    if not dir then
      return nil
    end
    local entries = helpers.collect_file_entries_with_tags(dir)
    local counts = helpers.collect_tag_counts(entries)
    local keys = sorted_tag_keys(counts)
    if #keys == 0 then
      return nil
    end
    local out = "\n\nMemory tags (use `memory fetch` to recall; `memory write` to store):\n"
    local shown = keys
    local overflow = 0
    if #keys > HINT_MAX_TAGS then
      shown = {}
      for i = 1, HINT_MAX_TAGS do
        shown[i] = keys[i]
      end
      overflow = #keys - HINT_MAX_TAGS
    end
    for _, t in ipairs(shown) do
      out = out .. "- " .. t .. " (" .. counts[t] .. ")\n"
    end
    if overflow > 0 then
      out = out
        .. "...and "
        .. overflow
        .. " more tags. "
        .. HINT_MAX_TAGS
        .. " highest-count tags listed above; consider `memory delete` on stale files under the heaviest tags to reduce tag count, or `memory list_tags` for the full list.\n"
    end
    return out
  end,
})

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Proactively save non-obvious project gotchas and architecture decisions to **memory**,"
    .. " tagged for later `fetch`. Prefer reusing existing tags over inventing new ones.",
})

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
  if not view:set_highlight(content, ext) then
    for line in (content .. "\n"):gmatch("([^\n]*)\n") do
      view:append(line)
    end
  end
  view:finish()
  return buf
end

local function build_frontmatter(preserved, tags)
  if #preserved == 0 and #tags == 0 then
    return ""
  end
  local out = "---\n"
  for _, l in ipairs(preserved) do
    out = out .. l .. "\n"
  end
  if #tags > 0 then
    out = out .. "tags:\n"
    for _, t in ipairs(tags) do
      out = out .. "  - " .. t .. "\n"
    end
  end
  return out .. "---\n"
end

local function cmd_write(input, dir, ctx)
  local file_path, err = helpers.safe_resolve(dir, input.path)
  if not file_path then
    return nil, err
  end
  local existing = maki.fs.read(file_path)
  local preserved = {}
  local cur_tags = {}
  if existing then
    local parsed = helpers.parse_frontmatter(existing)
    preserved = parsed.preserved or {}
    cur_tags = parsed.tags or {}
  end
  local tags = input.tags == nil and cur_tags or helpers.normalize_tag_list(input.tags or {})
  local new_content = build_frontmatter(preserved, tags) .. input.content

  maki.fs.mkdir(dir, { parents = true })
  local ok, write_err = maki.fs.write(file_path, new_content)
  if not ok then
    return nil, "write error: " .. tostring(write_err)
  end
  local lc = helpers.count_lines(input.content)
  local msg = "wrote " .. input.path .. " (" .. lc .. " lines)"
  return {
    llm_output = msg,
    body = render_content(new_content, input.path, ctx),
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
  return { llm_output = "deleted " .. path }
end

local SEPARATOR = "\n\n---\n\n"

local function cmd_fetch(input, dir, ctx)
  if not input.tags or #input.tags == 0 then
    return nil, "'tags' is required for fetch"
  end
  local entries = helpers.collect_file_entries_with_tags(dir)
  local wanted = helpers.tag_set(input.tags)
  local matched = {}
  for _, e in ipairs(entries) do
    if helpers.matches_tag_set(e, wanted) then
      matched[#matched + 1] = e
    end
  end
  if #matched == 0 then
    return { llm_output = "no files matched tags: " .. table.concat(input.tags, ", ") }
  end
  table.sort(matched, function(a, b)
    return a.name < b.name
  end)
  local parts = {}
  for _, e in ipairs(matched) do
    local content = e.content
    local suffix = ""
    if not content then
      suffix = "[unread]"
      content = ""
    end
    parts[#parts + 1] = "# " .. e.name .. "\n\n" .. content .. suffix
  end
  local llm_output = table.concat(parts, SEPARATOR)
  return {
    llm_output = llm_output,
    body = render_content(llm_output, "memory.md", ctx),
  }
end

local function cmd_list_tags(dir)
  local entries = helpers.collect_file_entries_with_tags(dir)
  local counts = helpers.collect_tag_counts(entries)
  local keys = sorted_tag_keys(counts)
  if #keys == 0 then
    return { llm_output = "No tags found." }
  end
  local lines = { "tags:" }
  for _, t in ipairs(keys) do
    lines[#lines + 1] = "- " .. t .. " (" .. counts[t] .. " files)"
  end
  return { llm_output = table.concat(lines, "\n") }
end

maki.api.register_tool({
  name = "memory",
  description = "Persistent, project-scoped memory for learnings, decisions, and gotchas across sessions.\n\n"
    .. "Commands:\n"
    .. "- `fetch`: return the concatenated contents of all files matching ANY of `tags` (1 call). Primary recall primitive.\n"
    .. "- `write`: create or overwrite `path`. Optional `tags` set the file's tags (YAML bullet list); omit to preserve existing tags, pass [] to clear.\n"
    .. "- `delete`: remove a file by `path`.\n"
    .. "- `list_tags`: list all distinct tags with file counts.\n\n"
    .. "Tags normalize to lowercase snake_case; `User-Decision` and `user decision` both match `user_decision`.",

  schema = {
    type = "object",
    properties = {
      command = {
        type = "string",
        description = "Command: fetch, write, delete, list_tags",
      },
      path = { type = "string", description = "Relative path (e.g. 'architecture.md'). Required for write/delete." },
      content = { type = "string", description = "File content for 'write'." },
      tags = {
        type = "array",
        description = "Tags for fetch (any-match union) or write (set on file). Normalized to lowercase snake_case.",
        items = { type = "string" },
      },
    },
    required = { "command" },
  },

  header = function(input)
    if input.path then
      return (input.command or "") .. " " .. input.path
    end
    if input.tags and #input.tags > 0 then
      return (input.command or "") .. " " .. table.concat(input.tags, ", ")
    end
    return input.command
  end,

  restore = function(input, output, _is_error, ctx)
    local content = (input.command == "write" and input.content) or output
    return render_content(content, input.path or "memory.md", ctx)
  end,

  handler = function(input, ctx)
    local cmd = input.command
    if cmd == "fetch" then
      local dir, dir_err = resolve_dir(true)
      if not dir then
        return "error: " .. dir_err
      end
      local result, err = cmd_fetch(input, dir, ctx)
      if err then
        return "error: " .. err
      end
      return result
    elseif cmd == "write" then
      if not input.path then
        return "error: 'path' is required for write"
      end
      if not input.content then
        return "error: 'content' is required for write"
      end
      local dir, dir_err = resolve_dir(false)
      if not dir then
        return "error: " .. dir_err
      end
      local result, err = cmd_write(input, dir, ctx)
      if err then
        return "error: " .. err
      end
      return result
    elseif cmd == "delete" then
      if not input.path then
        return "error: 'path' is required for delete"
      end
      local dir, dir_err = resolve_dir(false)
      if not dir then
        return "error: " .. dir_err
      end
      local result, err = cmd_delete(input.path, dir)
      if err then
        return "error: " .. err
      end
      return result
    elseif cmd == "list_tags" then
      local dir, dir_err = resolve_dir(true)
      if not dir then
        return "error: " .. dir_err
      end
      return cmd_list_tags(dir)
    end
    return "error: unknown command '" .. tostring(cmd) .. "'. Valid commands: fetch, write, delete, list_tags"
  end,
})

maki.api.register_command({
  name = "/memory",
  description = "View, edit, and delete memory files, grouped by tag",
  handler = function()
    local dir = resolve_dir(true)
    if not dir then
      maki.ui.flash("Cannot resolve memory directory")
      return
    end

    local UNTAGGED = "(untagged)"

    local function build_items(entries)
      local tag_set = {}
      for _, e in ipairs(entries) do
        if not e.from_stem then
          for _, t in ipairs(e.tags) do
            tag_set[t] = true
          end
        end
      end
      local tag_order = sorted_tag_keys(tag_set)

      local grouped = {}
      for _, t in ipairs(tag_order) do
        grouped[t] = {}
      end
      grouped[UNTAGGED] = {}

      for _, e in ipairs(entries) do
        if e.from_stem then
          grouped[UNTAGGED][#grouped[UNTAGGED] + 1] = e
        else
          for _, t in ipairs(e.tags) do
            if grouped[t] then
              grouped[t][#grouped[t] + 1] = e
            end
          end
        end
      end

      local items = {}
      local function emit_group(label, members)
        if #members == 0 then
          return
        end
        table.sort(members, function(a, b)
          return a.name < b.name
        end)
        items[#items + 1] = { label = label .. " (" .. #members .. ")", header = true }
        for _, e in ipairs(members) do
          items[#items + 1] = {
            label = e.name,
            detail = "(" .. e.bytes .. " bytes)",
            match_text = table.concat(e.tags, " "),
            _entry = e,
          }
        end
      end

      for _, t in ipairs(tag_order) do
        emit_group(t, grouped[t])
      end
      emit_group(UNTAGGED, grouped[UNTAGGED])
      return items
    end

    local entries = helpers.collect_file_entries_with_tags(dir)
    if #entries == 0 then
      maki.ui.flash("No memory files yet")
      return
    end

    local last_name
    while true do
      local items = build_items(entries)
      local cursor = 1
      if last_name then
        for i, it in ipairs(items) do
          if it._entry and it._entry.name == last_name then
            cursor = i
            break
          end
        end
      end
      local event = ListPicker.open(items, {
        title = " Memory Files ",
        cursor = cursor,
        submit_keys = { "ctrl+o" },
        footer = {
          { "Enter", "open" },
          { "Ctrl+O", "edit" },
          { "Ctrl+D", "delete" },
        },
      })

      if event.type == "close" then
        break
      end

      local item = items[event.index]
      if not (item and item._entry) then
        break
      end
      local entry = item._entry
      last_name = entry.name
      local path = maki.fs.joinpath(dir, entry.name)

      if event.type == "choice" then
        local code = maki.ui.open_editor(path)
        if code == 0 then
          local meta = maki.fs.metadata(path)
          if meta then
            entry.bytes = meta.size
          end
        end
      elseif event.type == "delete" then
        local ok, err = maki.fs.rm(path)
        if ok then
          maki.ui.flash("Deleted " .. entry.name)
          for i, e in ipairs(entries) do
            if e == entry then
              table.remove(entries, i)
              break
            end
          end
          if #entries == 0 then
            break
          end
        else
          maki.ui.flash("Delete failed: " .. tostring(err))
        end
      else
        break
      end
    end
  end,
})
