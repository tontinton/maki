local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")
local fuzzy_replace = require("maki.fuzzy_replace")
local replace_lines = require("edit_helpers").replace_lines

local SNIPPET_MAX_CHARS = 32
local FALLBACK_VIEW_LINES = 10

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

-- Line number of `needle` in `content` (plain find, first match).
local function line_of(content, needle)
  local pos = content:find(needle, 1, true)
  if not pos then
    return nil
  end
  local _, newlines = content:sub(1, pos - 1):gsub("\n", "")
  return newlines + 1
end

-- The edit already happened, so each block's `new` text sits in the file
-- right now: read it once and recover real line numbers. Best effort,
-- blocks stay unnumbered when the text moved or the file is gone.
local function resolve_block_nrs(blocks, path)
  if not path then
    return
  end
  local content
  for _, b in ipairs(blocks) do
    if not b.nr and (b.new or "") ~= "" then
      content = content or maki.fs.read(maki.fs.abspath(path))
      if not content then
        return
      end
      b.nr = line_of(content, b.new)
    end
  end
end

local function gutter_width(blocks)
  local max_nr = 0
  for _, b in ipairs(blocks) do
    if b.nr then
      local n = #split_lines(b.old or "")
      if n == 0 then
        n = #split_lines(b.new or "")
      end
      max_nr = math.max(max_nr, b.nr + n - 1)
    end
  end
  return max_nr > 0 and #tostring(max_nr) or 0
end

-- The one gutter builder both render passes share: the plain render and
-- the async highlight rewrite must produce byte-identical gutters or the
-- columns shift when highlights land.
local function nr_span(fmt, start_nr, i)
  return { string.format(fmt, start_nr and (start_nr + i - 1) or ""), "line_nr" }
end

local function append_diff_lines(view, text, style, prefix, nr_fmt, start_nr, jobs)
  local lines = split_lines(text or "")
  if #lines == 0 then
    return
  end
  jobs[#jobs + 1] = {
    first = #view.all_lines + 1,
    text = table.concat(lines, "\n"),
    style = style,
    prefix = prefix,
    start_nr = start_nr,
  }
  for i, line in ipairs(lines) do
    local spans = {}
    if nr_fmt then
      spans[#spans + 1] = nr_span(nr_fmt, start_nr, i)
    end
    spans[#spans + 1] = { prefix .. line, style }
    view:append(spans)
  end
end

-- Re-renders the block's lines with syntax colors on the diff backgrounds,
-- keeping the gutter and prefix the plain render put there.
local function apply_highlights(view, fmt, jobs, ext)
  maki.async.run(function()
    for _, job in ipairs(jobs) do
      local bg = maki.ui.theme_color(job.style)
      local highlighted = bg and maki.ui.highlight(job.text, ext)
      for i, hl_line in ipairs(highlighted or {}) do
        local idx = job.first + i - 1
        if not view.all_lines[idx] then
          break
        end
        local spans = {}
        if fmt then
          spans[#spans + 1] = nr_span(fmt, job.start_nr, i)
        end
        spans[#spans + 1] = { job.prefix, { bg = bg } }
        for _, seg in ipairs(hl_line) do
          local s = type(seg[2]) == "table" and seg[2] or {}
          s.bg = bg
          spans[#spans + 1] = { seg[1], s }
        end
        view:update_line(idx, spans)
      end
    end
    view:flush()
  end)
end

-- Mirrors the standalone Rust diff render (code_view.rs): numbered gutter
-- on removed lines, blank gutter + `+` on added lines, and no truncation
-- ever, a diff is exactly the change and hiding part of it lies.
local function diff_view(blocks, path)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = math.huge, keep = "head" })
  resolve_block_nrs(blocks, path)
  local w = gutter_width(blocks)
  local fmt = w > 0 and ("%" .. w .. "s ") or nil
  local jobs = {}
  local function append(text, style, prefix, start_nr)
    append_diff_lines(view, text, style, prefix, fmt, start_nr, jobs)
  end
  for i, block in ipairs(blocks) do
    if i > 1 then
      view:append({})
    end
    local has_old = (block.old or "") ~= ""
    append(block.old, "diff_old", "- ", block.nr)
    append(block.new, "diff_new", "+ ", not has_old and block.nr or nil)
  end
  view:finish()
  local ext = (path or ""):match("%.([^%.]+)$")
  if #jobs > 0 and ext then
    apply_highlights(view, fmt, jobs, ext)
  end
  return buf
end

local function diff_restore(blocks_from)
  return function(input, output, is_error, ctx)
    if is_error then
      return ToolView.restore(output, edit_view_opts(ctx))
    end
    return diff_view(blocks_from(input), input.path)
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
    return { { new = input.new_string, nr = input.start } }
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
    return { { new = input.new_string, nr = input.line } }
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
