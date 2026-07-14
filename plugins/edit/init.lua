local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")
local fuzzy_replace = require("maki.fuzzy_replace")
local replace_lines = require("edit_helpers").replace_lines

local SNIPPET_MAX_CHARS = 32

local EDIT_LINES_DESCRIPTION =
  [[Edit lines by number. Replaces lines from `start` to `end` (inclusive) with `new_string`. Use empty `new_string` to delete a range. Do not use with the batch tool.]]

local INSERT_LINES_DESCRIPTION =
  [[Insert lines before a given line number. Lines at `line` and below shift down. Existing lines are preserved. Do not use with the batch tool.]]

local EDIT_DESCRIPTION = [[Replace an exact string match in a file.

- The old_string must appear exactly once unless replace_all is true.
- Read the file first to get exact content.
- When copying text from read output, do NOT include the line number prefix (e.g. `42: `) - only the content after it.
- Prefer this over write for targeted changes - it uses far fewer tokens.
- Use replace_all for renaming across a file.
]]

local MULTIEDIT_DESCRIPTION = [[Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

- Read the file first to get exact content.
- old_string must match the file contents exactly, including all whitespace and indentation.
- Each edit must match exactly once unless replace_all is true. Use replace_all for renaming across a file.
- Edits are applied in sequence - each operates on the result of the previous.
- If any edit fails, none are written.
- Ensure earlier edits don't affect text that later edits need to find.
]]

local function edit_header(input)
  local buf = maki.ui.buf()
  buf:line({ { shorten_path(input.path or ""), "path" } })
  return buf
end

local function split_lines(text)
  local lines = {}
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end
  if lines[#lines] == "" then
    lines[#lines] = nil
  end
  return lines
end

local function edit_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.write) or FALLBACK_VIEW_LINES, keep = "head" }
end

-- Old lines red, new lines green, one block per edit. Rebuilt purely from
-- the input, so a batch child can render the change without the file
-- snapshots (those live in ToolOutput::Diff, which Rust renders standalone).
local function diff_view(blocks, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, edit_view_opts(ctx))
  for i, block in ipairs(blocks) do
    if i > 1 then
      view:append({})
    end
    for _, line in ipairs(split_lines(block.old or "")) do
      view:append({ { line, "diff_old" } })
    end
    for _, line in ipairs(split_lines(block.new or "")) do
      view:append({ { line, "diff_new" } })
    end
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

local function diff_restore(blocks_from)
  return function(input, output, is_error, ctx)
    if is_error then
      return ToolView.restore(output, edit_view_opts(ctx))
    end
    return diff_view(blocks_from(input), ctx)
  end
end

local function apply_edit(path, ctx, transform)
  path = maki.fs.abspath(path)

  local ok, err = ctx:check_before_edit(path)
  if not ok then
    return nil, err
  end

  local before, read_err = maki.fs.read(path)
  if read_err then
    return nil, "read error: " .. tostring(read_err)
  end

  local after, transform_err = transform(before)
  if transform_err then
    return nil, transform_err
  end

  local _, write_err = maki.fs.write(path, after)
  if write_err then
    return nil, "write error: " .. tostring(write_err)
  end

  ctx:record_read(path)

  return {
    path = path,
    before = before,
    after = after,
  }
end

local function diff_result(edit_result, summary)
  return {
    llm_output = summary,
    diff_path = edit_result.path,
    diff_before = edit_result.before,
    diff_after = edit_result.after,
    written_path = edit_result.path,
  }
end

maki.api.register_tool({
  name = "edit",
  kind = "edit",
  mutable_path = "path",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = EDIT_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      old_string = {
        type = "string",
        description = "Exact string to find (must match uniquely unless replace_all is true)",
        required = true,
      },
      new_string = {
        type = "string",
        description = "Replacement string",
        required = true,
      },
      replace_all = {
        type = "boolean",
        description = "Replace all occurrences (default false)",
      },
    },
  },

  header = edit_header,
  restore = diff_restore(function(input)
    return { { old = input.old_string, new = input.new_string } }
  end),

  handler = function(input, ctx)
    local result, err = apply_edit(input.path, ctx, function(content)
      return fuzzy_replace.replace(content, input.old_string, input.new_string, input.replace_all or false)
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end

    return diff_result(result, "edited " .. shorten_path(result.path))
  end,
})

maki.api.register_tool({
  name = "multiedit",
  kind = "edit",
  mutable_path = "path",
  permission_scopes = "path",
  start_annotation = "edits",
  audiences = { "main", "general_sub", "interpreter" },
  description = MULTIEDIT_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      edits = {
        type = "array",
        description = "Array of edit operations to apply sequentially",
        required = true,
        items = {
          type = "object",
          properties = {
            old_string = {
              type = "string",
              description = "Exact string to find",
              required = true,
            },
            new_string = {
              type = "string",
              description = "Replacement string",
              required = true,
            },
            replace_all = {
              type = "boolean",
              description = "Replace all occurrences (default false)",
            },
          },
        },
      },
    },
  },

  header = edit_header,
  restore = diff_restore(function(input)
    local blocks = {}
    for _, edit in ipairs(input.edits or {}) do
      blocks[#blocks + 1] = { old = edit.old_string, new = edit.new_string }
    end
    return blocks
  end),

  handler = function(input, ctx)
    local edits = input.edits
    if #edits == 0 then
      return { llm_output = "provide at least one edit", is_error = true }
    end

    local result, err = apply_edit(input.path, ctx, function(content)
      for i, edit in ipairs(edits) do
        local replaced, replace_err =
          fuzzy_replace.replace(content, edit.old_string, edit.new_string, edit.replace_all or false)
        if replace_err then
          local snippet = edit.old_string:match("[^\n]*")
          local cut = utf8.offset(snippet, SNIPPET_MAX_CHARS + 1)
          if cut then
            snippet = snippet:sub(1, cut - 1) .. "…"
          end
          return nil, string.format("edits[%d] (old_string %q): %s", i - 1, snippet, replace_err)
        end
        content = replaced
      end
      return content
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end

    local n = #edits
    local s = n == 1 and "" or "s"
    return diff_result(result, string.format("applied %d edit%s to %s", n, s, shorten_path(result.path)))
  end,
})

maki.api.register_tool({
  name = "edit_lines",
  kind = "edit",
  mutable_path = "path",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = EDIT_LINES_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      start = {
        type = "integer",
        description = "First line (1-indexed)",
        required = true,
      },
      ["end"] = {
        type = "integer",
        description = "Last line, inclusive",
        required = true,
      },
      new_string = {
        type = "string",
        description = "Replacement text",
        required = true,
      },
    },
  },

  header = edit_header,
  restore = diff_restore(function(input)
    return { { new = input.new_string } }
  end),

  handler = function(input, ctx)
    local result, err = apply_edit(input.path, ctx, function(content)
      return replace_lines(content, input.start, input["end"], input.new_string)
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end
    return diff_result(
      result,
      string.format("replaced lines %d-%d in %s", input.start, input["end"], shorten_path(result.path))
    )
  end,
})

maki.api.register_tool({
  name = "insert_lines",
  kind = "edit",
  mutable_path = "path",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = INSERT_LINES_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      line = {
        type = "integer",
        description = "Line number to insert before (1-indexed). Use 1 to insert at the top.",
        required = true,
      },
      new_string = {
        type = "string",
        description = "Text to insert",
        required = true,
      },
    },
  },

  header = edit_header,
  restore = diff_restore(function(input)
    return { { new = input.new_string } }
  end),

  handler = function(input, ctx)
    local result, err = apply_edit(input.path, ctx, function(content)
      return replace_lines(content, input.line, nil, input.new_string)
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end
    return diff_result(result, string.format("inserted at line %d in %s", input.line, shorten_path(result.path)))
  end,
})
